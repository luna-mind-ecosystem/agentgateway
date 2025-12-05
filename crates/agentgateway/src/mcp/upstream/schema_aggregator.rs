// Schema Aggregator for Dynamic Multi-Service MCP Tools
// Luna Mind Ecosystem - 2025-11-19
//
// This module implements dynamic schema aggregation from multiple OpenAPI services.
// It polls each service's /v3/api-docs endpoint, merges the schemas, and provides
// dynamic tool routing for MCP tools/call requests.

use std::sync::{Arc, RwLock};
use std::time::Duration;

use openapiv3::OpenAPI;
use rmcp::model::Tool;
use serde::{Deserialize, Serialize};
use tokio::time;

use super::openapi;

/// Configuration for a single backend service that exposes OpenAPI schema
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ServiceConfig {
    /// Service name (e.g., "personality", "thoughts", "goals")
    pub name: String,
    /// Base URL for HTTP requests (e.g., "http://localhost:8080")
    pub base_url: String,
    /// Path to OpenAPI schema endpoint (e.g., "/v3/api-docs")
    pub openapi_path: String,
}

/// Tool routing information that maps MCP tool name to backend service endpoint
#[derive(Clone, Debug)]
pub struct ToolRoutingInfo {
    /// Service name that handles this tool
    pub service_name: String,
    /// HTTP method (GET, POST, PUT, DELETE, etc.)
    pub method: String,
    /// URL path for the endpoint
    pub path: String,
    /// MCP tool definition (name, description, input schema)
    pub tool: Tool,
}

/// Aggregated schema containing all tools from all services
#[derive(Clone, Debug)]
pub struct AggregatedSchema {
    /// All tools with their routing information
    pub tools: Vec<ToolRoutingInfo>,
    /// Hash of the schema for change detection
    pub schema_hash: String,
}

/// Schema Aggregator that polls multiple services and merges their OpenAPI schemas
#[derive(Debug)]
pub struct SchemaAggregator {
    /// List of services to poll
    services: Vec<ServiceConfig>,
    /// Current aggregated schema (protected by RwLock for concurrent access)
    current_schema: Arc<RwLock<Option<AggregatedSchema>>>,
    /// Polling interval (default: 30 seconds)
    polling_interval: Duration,
    /// HTTP client for fetching schemas
    http_client: reqwest::Client,
    /// Broadcast channel to notify listeners when schema changes
    change_notifier: tokio::sync::broadcast::Sender<()>,
}

impl SchemaAggregator {
    /// Create a new SchemaAggregator
    ///
    /// # Arguments
    /// * `services` - List of services to poll for OpenAPI schemas
    /// * `polling_interval` - How often to poll services (e.g., Duration::from_secs(30))
    ///
    /// # Returns
    /// * Tuple of (SchemaAggregator, Receiver for schema change notifications)
    pub fn new(
        services: Vec<ServiceConfig>,
        polling_interval: Duration,
    ) -> (Self, tokio::sync::broadcast::Receiver<()>) {
        let (tx, rx) = tokio::sync::broadcast::channel(16);
        let aggregator = Self {
            services,
            current_schema: Arc::new(RwLock::new(None)),
            polling_interval,
            http_client: reqwest::Client::builder()
                .timeout(Duration::from_secs(10))
                .build()
                .expect("Failed to create HTTP client"),
            change_notifier: tx,
        };
        (aggregator, rx)
    }

    /// Start polling services in the background
    ///
    /// This spawns a tokio task that polls all services at the configured interval.
    /// When schema changes are detected, notifications are sent via the change channel.
    pub fn start_polling(self: Arc<Self>) {
        tokio::spawn(async move {
            tracing::info!("SchemaAggregator: Starting polling loop with interval {:?}", self.polling_interval);
            let mut interval = time::interval(self.polling_interval);

            // Do initial poll immediately
            if let Err(e) = self.poll_and_update().await {
                tracing::error!("SchemaAggregator: Initial poll failed: {}", e);
            }

            loop {
                interval.tick().await;
                if let Err(e) = self.poll_and_update().await {
                    tracing::error!("SchemaAggregator: Polling error: {}", e);
                }
            }
        });
    }

    /// Poll all services and update the aggregated schema if changed
    async fn poll_and_update(&self) -> Result<(), anyhow::Error> {
        tracing::debug!("SchemaAggregator: Starting poll of {} services", self.services.len());
        let mut all_tools = Vec::new();

        // Fetch schema from each service
        for service in &self.services {
            match self.fetch_service_schema(service).await {
                Ok(schema) => {
                    tracing::debug!("SchemaAggregator: Fetched schema from service '{}'", service.name);
                    // Convert OpenAPI schema to MCP tools with routing info
                    match self.extract_tools(&schema, &service.name, &service.base_url) {
                        Ok(tools) => {
                            tracing::info!("SchemaAggregator: Extracted {} tools from service '{}'", tools.len(), service.name);
                            all_tools.extend(tools);
                        }
                        Err(e) => {
                            tracing::warn!("SchemaAggregator: Failed to extract tools from '{}': {}", service.name, e);
                        }
                    }
                }
                Err(e) => {
                    tracing::warn!("SchemaAggregator: Failed to fetch schema from '{}': {}", service.name, e);
                }
            }
        }

        // Calculate hash of the new schema
        let new_hash = Self::calculate_hash(&all_tools);

        // Check if schema changed
        let schema_changed = {
            let current = self.current_schema.read().unwrap();
            match current.as_ref() {
                Some(current_schema) => current_schema.schema_hash != new_hash,
                None => true, // First time
            }
        };

        // If changed, update and notify
        if schema_changed {
            let new_schema = AggregatedSchema {
                tools: all_tools,
                schema_hash: new_hash.clone(),
            };

            let tool_count = new_schema.tools.len();
            *self.current_schema.write().unwrap() = Some(new_schema);

            tracing::info!("SchemaAggregator: Schema changed (hash: {}), {} total tools available", new_hash, tool_count);

            // Notify listeners
            if let Err(e) = self.change_notifier.send(()) {
                tracing::warn!("SchemaAggregator: Failed to send change notification: {}", e);
            }
        } else {
            tracing::debug!("SchemaAggregator: Schema unchanged (hash: {})", new_hash);
        }

        Ok(())
    }

    /// Fetch OpenAPI schema from a service and normalize server URLs
    ///
    /// CRITICAL: This function normalizes the server URLs to "/" for AgentGateway compatibility.
    /// AgentGateway expects a single server with relative URL, not absolute URLs like "http://localhost:8080".
    async fn fetch_service_schema(
        &self,
        service: &ServiceConfig,
    ) -> Result<OpenAPI, anyhow::Error> {
        let url = format!("{}{}", service.base_url, service.openapi_path);
        tracing::debug!("SchemaAggregator: Fetching OpenAPI schema from: {}", url);

        let response = self
            .http_client
            .get(&url)
            .send()
            .await?;

        if !response.status().is_success() {
            anyhow::bail!("HTTP error {}: {}", response.status(), response.text().await?);
        }

        let mut schema: OpenAPI = response.json().await?;

        // CRITICAL: Normalize server URLs for AgentGateway compatibility
        // AgentGateway only supports 0 or 1 server, and expects relative URLs
        // See: agentgateway-luna-fork/crates/agentgateway/src/mcp/upstream/openapi/mod.rs:48-56
        schema.servers = vec![openapiv3::Server {
            url: "/".to_string(),
            description: None,
            variables: Default::default(),
            extensions: Default::default(),
        }];

        tracing::debug!("SchemaAggregator: Normalized server URLs to '/' for service '{}'", service.name);

        Ok(schema)
    }

    /// Extract MCP tools from OpenAPI schema
    ///
    /// This converts OpenAPI paths and operations into MCP Tool definitions with routing info.
    /// Uses the existing parse_openapi_schema function from the openapi module.
    fn extract_tools(
        &self,
        schema: &OpenAPI,
        service_name: &str,
        base_url: &str,
    ) -> Result<Vec<ToolRoutingInfo>, anyhow::Error> {
        tracing::debug!("SchemaAggregator: Extracting tools from service '{}' at base_url '{}'", service_name, base_url);

        // Use the existing parse_openapi_schema function
        let parsed_tools = openapi::parse_openapi_schema(schema)
            .map_err(|e| anyhow::anyhow!("Failed to parse OpenAPI schema: {}", e))?;

        // Convert to ToolRoutingInfo with service name and base URL
        let routing_infos: Vec<ToolRoutingInfo> = parsed_tools
            .into_iter()
            .map(|(tool, upstream_call)| {
                ToolRoutingInfo {
                    service_name: service_name.to_string(),
                    method: upstream_call.method,
                    path: upstream_call.path,
                    tool,
                }
            })
            .collect();

        tracing::debug!(
            "SchemaAggregator: Extracted {} tools from service '{}': {:?}",
            routing_infos.len(),
            service_name,
            routing_infos.iter().map(|r| r.tool.name.as_ref()).collect::<Vec<_>>()
        );

        Ok(routing_infos)
    }

    /// Calculate hash of the schema for change detection
    ///
    /// Uses Rust's DefaultHasher to create a simple hash of all tool names and routing info.
    fn calculate_hash(tools: &[ToolRoutingInfo]) -> String {
        use std::collections::hash_map::DefaultHasher;
        use std::hash::{Hash, Hasher};

        let mut hasher = DefaultHasher::new();
        for tool in tools {
            tool.service_name.hash(&mut hasher);
            tool.method.hash(&mut hasher);
            tool.path.hash(&mut hasher);
            tool.tool.name.hash(&mut hasher);
        }
        format!("{:x}", hasher.finish())
    }

    /// Find tool routing info by tool name
    ///
    /// Returns None if tool is not found in any service.
    pub fn find_tool(&self, tool_name: &str) -> Option<ToolRoutingInfo> {
        let schema = self.current_schema.read().unwrap();
        schema
            .as_ref()?
            .tools
            .iter()
            .find(|t| t.tool.name.as_ref() == tool_name)
            .cloned()
    }

    /// Get base URL for a service by name
    ///
    /// Returns the base URL (e.g., "http://localhost:8080") for making HTTP requests
    pub fn get_service_base_url(&self, service_name: &str) -> Option<String> {
        self.services
            .iter()
            .find(|s| s.name == service_name)
            .map(|s| s.base_url.clone())
    }

    /// Get current OpenAPI schema for a specific service
    ///
    /// Returns the full OpenAPI schema for dynamic tool lookup
    ///
    /// NOTE: Currently unused - we use ToolRoutingInfo directly instead
    #[allow(dead_code)]
    pub fn get_current_schema(&self) -> Option<OpenAPI> {
        // For now, return the schema for the first service
        // TODO: Filter by service name when we support multiple services
        let config = self.services.first()?;

        // Fetch fresh schema synchronously (blocking)
        let url = format!("{}{}", config.base_url, config.openapi_path);
        let response = reqwest::blocking::get(&url).ok()?;
        let schema: OpenAPI = response.json().ok()?;
        Some(schema)
    }

    /// Get all available tools (for tools/list response)
    pub fn get_all_tools(&self) -> Vec<Tool> {
        let schema = self.current_schema.read().unwrap();
        match schema.as_ref() {
            Some(schema) => schema.tools.iter().map(|t| t.tool.clone()).collect(),
            None => {
                tracing::warn!("SchemaAggregator: No schema available yet");
                vec![]
            }
        }
    }

    /// Get number of services being polled
    #[allow(dead_code)]
    pub fn service_count(&self) -> usize {
        self.services.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_service_config_creation() {
        let config = ServiceConfig {
            name: "personality".to_string(),
            base_url: "http://localhost:8080".to_string(),
            openapi_path: "/v3/api-docs".to_string(),
        };
        assert_eq!(config.name, "personality");
    }

    #[test]
    fn test_schema_aggregator_creation() {
        let services = vec![ServiceConfig {
            name: "test".to_string(),
            base_url: "http://localhost:8080".to_string(),
            openapi_path: "/v3/api-docs".to_string(),
        }];
        let (agg, _rx) = SchemaAggregator::new(services, Duration::from_secs(30));
        assert_eq!(agg.service_count(), 1);
    }
}
