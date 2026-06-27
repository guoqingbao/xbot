use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use async_trait::async_trait;
use serde_json::{Value, json};
use tokio::sync::Mutex;
use tokio::time::timeout;

use crate::config::McpServerConfig;
use crate::tools::{Tool, ToolOutput, ToolRegistry, ToolSpec};

/// HTTP-based MCP client for connecting to remote MCP servers
#[derive(Debug)]
pub struct HttpMcpClient {
    server_name: String,
    base_url: String,
    headers: BTreeMap<String, String>,
    tool_timeout_s: u64,
    enabled_tools: Vec<String>,
    /// Cached tool list
    tools_cache: Mutex<Option<Vec<HttpMcpTool>>>,
}

impl HttpMcpClient {
    /// Create a new HTTP MCP client
    pub async fn connect(server_name: &str, config: &McpServerConfig) -> Result<Self> {
        let transport = if config.transport.trim().is_empty() {
            "stdio"
        } else {
            config.transport.as_str()
        };

        if transport != "http" && transport != "streamableHttp" && transport != "sse" {
            bail!(
                "MCP server '{}' uses unsupported HTTP transport '{}'; expected 'http', 'streamableHttp', or 'sse'",
                server_name,
                transport
            );
        }

        let url = config.url.as_ref().ok_or_else(|| {
            anyhow!("MCP server '{}' has transport='{}' but no URL configured", server_name, transport)
        })?;

        if url.trim().is_empty() {
            bail!("MCP server '{}' URL is empty", server_name);
        }

        // Validate URL
        url::Url::parse(url).context("Invalid MCP server URL")?;

        let client = Self {
            server_name: server_name.to_string(),
            base_url: url.clone(),
            headers: config.headers.clone(),
            tool_timeout_s: config.tool_timeout,
            enabled_tools: config.enabled_tools.clone(),
            tools_cache: Mutex::new(None),
        };

        // Test connection
        client.test_connection().await?;

        Ok(client)
    }

    /// Test the HTTP connection to the MCP server
    async fn test_connection(&self) -> Result<()> {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(10))
            .build()?;

        let mut request_builder = client.get(&self.base_url);

        for (key, value) in &self.headers {
            request_builder = request_builder.header(key, value);
        }

        let response = request_builder
            .send()
            .await
            .context("Failed to connect to MCP server")?;

        if !response.status().is_success() {
            bail!(
                "MCP server returned status {} at {}",
                response.status(),
                self.base_url
            );
        }

        Ok(())
    }

    /// Initialize connection with MCP server
    async fn initialize(&self) -> Result<Value> {
        let response = self
            .post_json("/mcp", json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "initialize",
                "params": {
                    "protocolVersion": "2024-11-05",
                    "capabilities": {},
                    "clientInfo": {
                        "name": "xbot",
                        "version": env!("CARGO_PKG_VERSION"),
                    }
                }
            }))
            .await?;

        if response.get("error").is_some() {
            bail!(
                "MCP server '{}' rejected initialize: {}",
                self.server_name,
                response.get("error").cloned().unwrap_or(Value::Null)
            );
        }

        // Send initialized notification
        self.post_json("/mcp", json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "notifications/initialized",
            "params": {}
        }))
        .await?;

        Ok(response)
    }

    /// List available tools from the MCP server
    pub async fn list_wrapped_tools(&self) -> Result<Vec<Arc<dyn Tool>>> {
        // Try to get from cache first
        {
            let cache = self.tools_cache.lock().await;
            if let Some(tools) = &*cache {
                return Ok(tools.iter().map(|t| Arc::new(t.clone()) as Arc<dyn Tool>).collect());
            }
        }

        let response = self
            .post_json("/mcp", json!({
                "jsonrpc": "2.0",
                "id": 3,
                "method": "tools/list",
                "params": {}
            }))
            .await?;

        let tools = response
            .get("result")
            .and_then(|result| result.get("tools"))
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();

        let allow_all = self.enabled_tools.iter().any(|name| name == "*");
        let mut registered = Vec::new();

        for tool in tools {
            let Some(name) = tool.get("name").and_then(Value::as_str) else {
                continue;
            };

            let wrapped_name = format!("mcp_http_{}_{}", self.server_name, name);

            if !allow_all
                && !self
                    .enabled_tools
                    .iter()
                    .any(|enabled| enabled == name || enabled == &wrapped_name)
            {
                continue;
            }

            let http_tool = HttpMcpTool {
                client: Arc::new(HttpMcpClient {
                    server_name: self.server_name.clone(),
                    base_url: self.base_url.clone(),
                    headers: self.headers.clone(),
                    tool_timeout_s: self.tool_timeout_s,
                    enabled_tools: self.enabled_tools.clone(),
                    tools_cache: Mutex::new(None),
                }),
                original_name: name.to_string(),
                wrapped_name,
                description: tool
                    .get("description")
                    .and_then(Value::as_str)
                    .unwrap_or(name)
                    .to_string(),
                parameters: tool
                    .get("inputSchema")
                    .cloned()
                    .unwrap_or_else(|| json!({"type": "object", "properties": {}})),
            };

            registered.push(Arc::new(http_tool) as Arc<dyn Tool>);
        }

        Ok(registered)
    }

    /// Call a tool on the MCP server
    pub async fn call_tool(&self, tool_name: &str, arguments: Value) -> Result<ToolOutput> {
        let response = timeout(
            Duration::from_secs(self.tool_timeout_s.max(1)),
            self.post_json("/mcp", json!({
                "jsonrpc": "2.0",
                "id": 4,
                "method": "tools/call",
                "params": {
                    "name": tool_name,
                    "arguments": arguments,
                }
            }))
        )
        .await
        .map_err(|_| anyhow!("MCP tool call '{}' timed out after {}s", tool_name, self.tool_timeout_s))??;

        if let Some(error) = response.get("error") {
            return Ok(ToolOutput::Text(format!(
                "Error: MCP HTTP tool '{}' failed: {}",
                tool_name, error
            )));
        }

        let content = response
            .get("result")
            .and_then(|result| result.get("content"))
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();

        let blocks = content
            .iter()
            .filter(|item| item.get("type").and_then(Value::as_str) != Some("text"))
            .cloned()
            .collect::<Vec<_>>();

        let text = content
            .iter()
            .filter_map(|item| item.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join("\n");

        if !blocks.is_empty() && text.is_empty() {
            Ok(ToolOutput::Blocks(blocks))
        } else if !text.is_empty() {
            Ok(ToolOutput::Text(text))
        } else {
            Ok(ToolOutput::Text("(no output)".to_string()))
        }
    }

    /// POST JSON to the MCP server endpoint
    async fn post_json(&self, path: &str, body: Value) -> Result<Value> {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(30))
            .build()?;

        let url = if path.starts_with('/') {
            format!("{}{}", self.base_url, path)
        } else {
            format!("{}/{}", self.base_url, path)
        };

        let mut request_builder = client.post(&url);

        for (key, value) in &self.headers {
            request_builder = request_builder.header(key, value);
        }

        let response = request_builder
            .json(&body)
            .send()
            .await
            .context(format!("Failed to send request to {}", url))?;

        if !response.status().is_success() {
            bail!(
                "HTTP request to {} failed with status {}: {}",
                url,
                response.status(),
                response.text().await.unwrap_or_default()
            );
        }

        let response_body = response
            .json::<Value>()
            .await
            .context("Failed to parse JSON response from MCP server")?;

        Ok(response_body)
    }
}

/// HTTP-based MCP tool wrapper
#[derive(Clone, Debug)]
struct HttpMcpTool {
    client: Arc<HttpMcpClient>,
    original_name: String,
    wrapped_name: String,
    description: String,
    parameters: Value,
}

#[async_trait]
impl Tool for HttpMcpTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: self.wrapped_name.clone(),
            description: self.description.clone(),
            parameters: self.parameters.clone(),
        }
    }

    async fn execute(&self, params: Value) -> ToolOutput {
        match self.client.call_tool(&self.original_name, params).await {
            Ok(output) => output,
            Err(err) => ToolOutput::Text(format!("Error: {err}")),
        }
    }
}

/// Register HTTP MCP tools in the tool registry
pub async fn register_http_mcp_tools(
    registry: &mut ToolRegistry,
    servers: &BTreeMap<String, McpServerConfig>,
) -> Result<()> {
    for (server_name, config) in servers {
        if !config.enabled {
            continue;
        }

        // Skip stdio servers, they are handled separately
        let transport = if config.transport.trim().is_empty() {
            "stdio"
        } else {
            config.transport.as_str()
        };

        if transport == "stdio" {
            continue;
        }

        let client = Arc::new(HttpMcpClient::connect(server_name, config).await?);
        let tools = client.list_wrapped_tools().await?;
        for tool in tools {
            registry.register_dyn(tool);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    #[tokio::test]
    async fn test_http_client_connection() {
        let mock_server = MockServer::start().await;

        Mock::given(method("GET"))
            .and(path("/"))
            .respond_with(ResponseTemplate::new(200).set_body_string("OK"))
            .mount(&mock_server)
            .await;

        let config = McpServerConfig {
            enabled: true,
            transport: "http".to_string(),
            command: String::new(),
            args: vec![],
            env: BTreeMap::new(),
            url: Some(mock_server.uri()),
            headers: BTreeMap::new(),
            enabled_tools: vec!["*".to_string()],
            tool_timeout: 30,
        };

        let client = HttpMcpClient::connect("test_server", &config).await;
        assert!(client.is_ok());
    }

    #[tokio::test]
    async fn test_http_client_rejects_invalid_transport() {
        let config = McpServerConfig {
            enabled: true,
            transport: "grpc".to_string(),
            command: String::new(),
            args: vec![],
            env: BTreeMap::new(),
            url: Some("http://localhost:8080".to_string()),
            headers: BTreeMap::new(),
            enabled_tools: vec!["*".to_string()],
            tool_timeout: 30,
        };

        let result = HttpMcpClient::connect("test", &config).await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("unsupported HTTP transport"));
    }

    #[tokio::test]
    async fn test_http_client_rejects_missing_url() {
        let config = McpServerConfig {
            enabled: true,
            transport: "http".to_string(),
            command: String::new(),
            args: vec![],
            env: BTreeMap::new(),
            url: None,
            headers: BTreeMap::new(),
            enabled_tools: vec!["*".to_string()],
            tool_timeout: 30,
        };

        let result = HttpMcpClient::connect("test", &config).await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("no URL configured"));
    }
}