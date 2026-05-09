use std::collections::BTreeMap;
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::{Context, Result, anyhow, bail};
use async_trait::async_trait;
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
use tokio::sync::Mutex;

use crate::config::McpServerConfig;
use crate::tools::{Tool, ToolOutput, ToolRegistry, ToolSpec};

pub async fn register_mcp_tools(
    registry: &mut ToolRegistry,
    servers: &BTreeMap<String, McpServerConfig>,
) -> Result<()> {
    for (server_name, config) in servers {
        if !config.enabled {
            continue;
        }
        let client = Arc::new(StdioMcpClient::connect(server_name, config).await?);
        for tool in client.list_wrapped_tools().await? {
            registry.register_dyn(tool);
        }
    }
    Ok(())
}

struct StdioMcpProcess {
    #[allow(dead_code)]
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
}

struct StdioMcpClient {
    server_name: String,
    tool_timeout_s: u64,
    enabled_tools: Vec<String>,
    process: Mutex<StdioMcpProcess>,
    next_id: AtomicU64,
}

impl StdioMcpClient {
    async fn connect(server_name: &str, config: &McpServerConfig) -> Result<Self> {
        let transport = if config.transport.trim().is_empty() {
            "stdio"
        } else {
            config.transport.as_str()
        };
        if transport != "stdio" {
            bail!(
                "MCP server '{}' uses unsupported transport '{}'; only stdio is currently supported",
                server_name,
                transport
            );
        }
        if config.command.trim().is_empty() {
            bail!("MCP server '{}' is missing command", server_name);
        }

        let mut command = Command::new(&config.command);
        command
            .args(&config.args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit());
        if !config.env.is_empty() {
            command.envs(&config.env);
        }
        let mut child = command
            .spawn()
            .with_context(|| format!("failed to spawn MCP server '{}'", server_name))?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| anyhow!("failed to capture stdin for MCP server '{}'", server_name))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| anyhow!("failed to capture stdout for MCP server '{}'", server_name))?;

        let client = Self {
            server_name: server_name.to_string(),
            tool_timeout_s: config.tool_timeout,
            enabled_tools: config.enabled_tools.clone(),
            process: Mutex::new(StdioMcpProcess {
                child,
                stdin,
                stdout: BufReader::new(stdout),
            }),
            next_id: AtomicU64::new(1),
        };
        client.initialize().await?;
        Ok(client)
    }

    async fn initialize(&self) -> Result<()> {
        let response = self
            .request(
                "initialize",
                json!({
                    "protocolVersion": "2024-11-05",
                    "capabilities": {},
                    "clientInfo": {
                        "name": "xbot",
                        "version": env!("CARGO_PKG_VERSION"),
                    }
                }),
            )
            .await?;
        if response.get("error").is_some() {
            bail!(
                "MCP server '{}' rejected initialize: {}",
                self.server_name,
                response.get("error").cloned().unwrap_or(Value::Null)
            );
        }
        self.notify("notifications/initialized", json!({})).await?;
        Ok(())
    }

    async fn list_wrapped_tools(self: &Arc<Self>) -> Result<Vec<Arc<dyn Tool>>> {
        let response = self.request("tools/list", json!({})).await?;
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
            let wrapped_name = format!("mcp_{}_{}", self.server_name, name);
            if !allow_all
                && !self
                    .enabled_tools
                    .iter()
                    .any(|enabled| enabled == name || enabled == &wrapped_name)
            {
                continue;
            }
            registered.push(Arc::new(McpTool {
                client: self.clone(),
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
            }) as Arc<dyn Tool>);
        }
        Ok(registered)
    }

    async fn call_tool(&self, tool_name: &str, arguments: Value) -> Result<ToolOutput> {
        let response = tokio::time::timeout(
            std::time::Duration::from_secs(self.tool_timeout_s.max(1)),
            self.request(
                "tools/call",
                json!({
                    "name": tool_name,
                    "arguments": arguments,
                }),
            ),
        )
        .await
        .map_err(|_| {
            anyhow!(
                "MCP tool call '{}' timed out after {}s",
                tool_name,
                self.tool_timeout_s
            )
        })??;
        if let Some(error) = response.get("error") {
            return Ok(ToolOutput::Text(format!(
                "Error: MCP tool '{}' failed: {}",
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

    async fn request(&self, method: &str, params: Value) -> Result<Value> {
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let mut process = self.process.lock().await;
        let payload = json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        });
        write_frame(&mut process.stdin, &payload).await?;
        loop {
            let message = read_frame(&mut process.stdout).await?;
            if message.get("id").and_then(Value::as_u64) == Some(id) {
                return Ok(message);
            }
        }
    }

    async fn notify(&self, method: &str, params: Value) -> Result<()> {
        let mut process = self.process.lock().await;
        let payload = json!({
            "jsonrpc": "2.0",
            "method": method,
            "params": params,
        });
        write_frame(&mut process.stdin, &payload).await
    }
}

struct McpTool {
    client: Arc<StdioMcpClient>,
    original_name: String,
    wrapped_name: String,
    description: String,
    parameters: Value,
}

#[async_trait]
impl Tool for McpTool {
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

async fn write_frame<W: AsyncWrite + Unpin>(stdin: &mut W, payload: &Value) -> Result<()> {
    let body = serde_json::to_vec(payload)?;
    stdin
        .write_all(format!("Content-Length: {}\r\n\r\n", body.len()).as_bytes())
        .await?;
    stdin.write_all(&body).await?;
    stdin.flush().await?;
    Ok(())
}

async fn read_frame(stdout: &mut BufReader<ChildStdout>) -> Result<Value> {
    let mut content_length = None;
    loop {
        let mut line = String::new();
        let read = stdout.read_line(&mut line).await?;
        if read == 0 {
            bail!("MCP stream closed unexpectedly");
        }
        let trimmed = line.trim_end_matches(['\r', '\n']);
        if trimmed.is_empty() {
            break;
        }
        if let Some(value) = trimmed.strip_prefix("Content-Length:") {
            content_length = Some(value.trim().parse::<usize>()?);
        }
    }
    let content_length =
        content_length.ok_or_else(|| anyhow!("missing MCP Content-Length header"))?;
    let mut body = vec![0_u8; content_length];
    stdout.read_exact(&mut body).await?;
    Ok(serde_json::from_slice(&body)?)
}

#[cfg(test)]
mod tests {
    use super::write_frame;
    use serde_json::json;
    use tokio::io::{AsyncReadExt, BufReader};

    #[tokio::test]
    async fn frame_codec_roundtrip() {
        let (mut client, server) = tokio::io::duplex(512);
        let payload = json!({"jsonrpc":"2.0","id":1,"method":"ping"});
        let write = tokio::spawn(async move { write_frame(&mut client, &payload).await.unwrap() });
        let mut reader = BufReader::new(server);
        let parsed = read_frame_from_reader(&mut reader).await.unwrap();
        write.await.unwrap();
        assert_eq!(parsed["method"], "ping");
    }

    async fn read_frame_from_reader(
        stdout: &mut BufReader<tokio::io::DuplexStream>,
    ) -> Result<serde_json::Value, anyhow::Error> {
        let mut content_length = None;
        loop {
            let mut line = String::new();
            let read = tokio::io::AsyncBufReadExt::read_line(stdout, &mut line).await?;
            if read == 0 {
                anyhow::bail!("closed");
            }
            let trimmed = line.trim_end_matches(['\r', '\n']);
            if trimmed.is_empty() {
                break;
            }
            if let Some(value) = trimmed.strip_prefix("Content-Length:") {
                content_length = Some(value.trim().parse::<usize>()?);
            }
        }
        let mut body = vec![0_u8; content_length.unwrap_or_default()];
        stdout.read_exact(&mut body).await?;
        Ok(serde_json::from_slice(&body)?)
    }
}
