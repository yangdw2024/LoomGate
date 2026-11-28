use crate::ir::StrictToolSchema;
use crate::middleware::{BoxFuture, Middleware, MiddlewareContext, MiddlewareRejection, NextFn};
use dashmap::DashMap;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::HashSet;
use std::process::Stdio;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::Command;

const STDIO_RPC_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum McpTransport {
    #[default]
    Http,
    Stdio {
        command: String,
        #[serde(default)]
        args: Vec<String>,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpServerConfig {
    pub name: String,
    #[serde(default)]
    pub endpoint: String,
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Optional statically declared tools (or hydrated dynamically via `discover_tools`).
    #[serde(default)]
    pub tools: Vec<StrictToolSchema>,
    /// Transport protocol (defaults to HTTP for full backward compatibility).
    #[serde(default)]
    pub transport: McpTransport,
    /// Optional shorthand stdio command (used when `transport` is left as default).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
    /// Optional shorthand stdio args accompanying `command`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub args: Vec<String>,
}

impl McpServerConfig {
    /// Resolves the active transport, honoring either an explicit `McpTransport::Stdio`
    /// or shorthand `command`/`args` fields while defaulting to HTTP.
    pub fn effective_transport(&self) -> McpTransport {
        match &self.transport {
            McpTransport::Stdio { command, args } => McpTransport::Stdio {
                command: command.clone(),
                args: args.clone(),
            },
            McpTransport::Http => {
                if let Some(cmd) = self.command.as_ref().filter(|c| !c.trim().is_empty()) {
                    McpTransport::Stdio {
                        command: cmd.clone(),
                        args: self.args.clone(),
                    }
                } else {
                    McpTransport::Http
                }
            }
        }
    }
}

fn default_true() -> bool {
    true
}

/// Sends a single JSON-RPC 2.0 request line over a spawned child process's stdio pipes
/// with a 5-second timeout and guarantees child termination & reaping.
pub async fn send_stdio_jsonrpc(
    command: &str,
    args: &[String],
    payload: &Value,
) -> Result<Value, String> {
    let mut line = serde_json::to_string(payload).map_err(|e| e.to_string())?;
    line.push('\n');

    let mut child = Command::new(command)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| format!("failed to spawn MCP stdio command '{}': {}", command, e))?;

    let mut stdin = child
        .stdin
        .take()
        .ok_or_else(|| "failed to open MCP stdio stdin pipe".to_string())?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| "failed to open MCP stdio stdout pipe".to_string())?;

    let exchange = tokio::time::timeout(STDIO_RPC_TIMEOUT, async move {
        stdin
            .write_all(line.as_bytes())
            .await
            .map_err(|e| format!("failed to write MCP stdio request: {}", e))?;
        stdin
            .flush()
            .await
            .map_err(|e| format!("failed to flush MCP stdio stdin: {}", e))?;

        let mut reader = BufReader::new(stdout);
        let mut resp_line = String::new();
        loop {
            resp_line.clear();
            let n = reader
                .read_line(&mut resp_line)
                .await
                .map_err(|e| format!("failed to read MCP stdio response: {}", e))?;
            if n == 0 {
                return Err("MCP stdio process exited without returning a JSON-RPC response".into());
            }
            let trimmed = resp_line.trim();
            if !trimmed.is_empty() {
                return serde_json::from_str::<Value>(trimmed)
                    .map_err(|e| format!("invalid JSON-RPC response from MCP stdio server: {}", e));
            }
        }
    })
    .await;

    let _ = child.kill().await;
    let _ = child.wait().await;

    match exchange {
        Ok(res) => res,
        Err(_) => Err(format!(
            "MCP stdio request to '{}' timed out after {}s",
            command,
            STDIO_RPC_TIMEOUT.as_secs()
        )),
    }
}

/// Unified MCP Server Aggregation Bridge Middleware.
/// - Aggregates tools from all enabled MCP servers into `ctx.request.tools` under the
///   canonical namespace `mcp__<server>__<tool>` without clobbering client-defined tools.
/// - Provides JSON-RPC 2.0 `tools/list` discovery and `tools/call` invocation over HTTP or stdio.
pub struct McpBridgeMiddleware {
    servers: DashMap<String, McpServerConfig>,
    http: Client,
}

impl McpBridgeMiddleware {
    pub fn new(configs: Vec<McpServerConfig>) -> Self {
        let servers = DashMap::new();
        for cfg in configs {
            if cfg.enabled {
                servers.insert(cfg.name.clone(), cfg);
            }
        }
        Self {
            servers,
            http: Client::new(),
        }
    }

    pub fn register_server(&self, cfg: McpServerConfig) {
        if cfg.enabled {
            self.servers.insert(cfg.name.clone(), cfg);
        }
    }

    pub fn aggregated_tools(&self) -> Vec<StrictToolSchema> {
        let mut out = Vec::new();
        for entry in self.servers.iter() {
            let srv = entry.value();
            for t in &srv.tools {
                let mut cloned = t.clone();
                cloned.name = format!("mcp__{}__{}", srv.name, t.name);
                out.push(cloned);
            }
        }
        out
    }

    async fn send_jsonrpc(&self, cfg: &McpServerConfig, payload: &Value) -> Result<Value, String> {
        match cfg.effective_transport() {
            McpTransport::Stdio { command, args } => {
                send_stdio_jsonrpc(&command, &args, payload).await
            }
            McpTransport::Http => {
                if cfg.endpoint.trim().is_empty() {
                    return Err(format!("MCP HTTP server '{}' has empty endpoint", cfg.name));
                }
                let resp = self
                    .http
                    .post(&cfg.endpoint)
                    .json(payload)
                    .send()
                    .await
                    .map_err(|e| e.to_string())?;
                resp.json().await.map_err(|e| e.to_string())
            }
        }
    }

    /// Calls `tools/list` on a registered HTTP or stdio MCP server and caches its tool schemas.
    pub async fn discover_tools(&self, server_name: &str) -> Result<usize, String> {
        let cfg = self
            .servers
            .get(server_name)
            .map(|s| s.clone())
            .ok_or_else(|| format!("unknown MCP server '{}'", server_name))?;

        let body = self
            .send_jsonrpc(
                &cfg,
                &json!({
                    "jsonrpc": "2.0",
                    "id": 1,
                    "method": "tools/list",
                    "params": {}
                }),
            )
            .await?;

        if let Some(err) = body.get("error") {
            return Err(format!("MCP server error: {}", err));
        }

        let raw_tools = body
            .get("result")
            .and_then(|r| r.get("tools"))
            .and_then(|t| t.as_array())
            .cloned()
            .unwrap_or_default();

        let parsed: Vec<StrictToolSchema> = raw_tools
            .into_iter()
            .filter_map(|t| {
                let name = t.get("name")?.as_str()?.to_string();
                let description = t
                    .get("description")
                    .and_then(|v| v.as_str())
                    .map(String::from);
                let parameters = t
                    .get("inputSchema")
                    .or_else(|| t.get("parameters"))
                    .cloned()
                    .unwrap_or_else(|| json!({"type": "object", "properties": {}}));
                Some(StrictToolSchema {
                    name,
                    description,
                    parameters,
                    strict: false,
                    cache_control: None,
                })
            })
            .collect();

        let count = parsed.len();
        if let Some(mut entry) = self.servers.get_mut(server_name) {
            entry.tools = parsed;
        }
        Ok(count)
    }

    /// Invokes a namespaced MCP tool (`mcp__<server>__<tool>`) via JSON-RPC 2.0 `tools/call`.
    pub async fn call_tool(&self, namespaced_name: &str, arguments: Value) -> Result<Value, String> {
        let rest = namespaced_name
            .strip_prefix("mcp__")
            .ok_or_else(|| format!("not an MCP namespaced tool: '{}'", namespaced_name))?;
        let (server_name, tool_name) = rest
            .split_once("__")
            .ok_or_else(|| format!("malformed MCP tool name: '{}'", namespaced_name))?;

        let cfg = self
            .servers
            .get(server_name)
            .map(|s| s.clone())
            .ok_or_else(|| format!("unknown MCP server '{}'", server_name))?;

        let body = self
            .send_jsonrpc(
                &cfg,
                &json!({
                    "jsonrpc": "2.0",
                    "id": 1,
                    "method": "tools/call",
                    "params": {
                        "name": tool_name,
                        "arguments": arguments
                    }
                }),
            )
            .await?;

        if let Some(err) = body.get("error") {
            return Err(format!("MCP server error: {}", err));
        }
        Ok(body.get("result").cloned().unwrap_or(Value::Null))
    }
}

impl Middleware for McpBridgeMiddleware {
    fn name(&self) -> &'static str {
        "mcp_bridge"
    }

    fn handle<'a>(
        &'a self,
        mut ctx: MiddlewareContext,
        next: NextFn<'a>,
    ) -> BoxFuture<'a, Result<MiddlewareContext, MiddlewareRejection>> {
        Box::pin(async move {
            let mcp_tools = self.aggregated_tools();
            if !mcp_tools.is_empty() {
                let mut existing: HashSet<String> =
                    ctx.request.tools.iter().map(|t| t.name.clone()).collect();
                let mut injected = 0usize;
                for tool in mcp_tools {
                    if existing.insert(tool.name.clone()) {
                        ctx.request.tools.push(tool);
                        injected += 1;
                    }
                }
                if injected > 0 {
                    ctx.annotations
                        .insert("X-Gateway-Mcp-Tools".into(), injected.to_string());
                }
            }
            next(ctx).await
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::UpstreamProtocol;
    use crate::ir::{CanonicalMessage, CanonicalRequest, ContentPart, Role};
    use crate::middleware::MiddlewarePipeline;
    use std::sync::Arc;

    #[tokio::test]
    async fn aggregates_mcp_tools_without_duplicating_client_tools() {
        let bridge = Arc::new(McpBridgeMiddleware::new(vec![McpServerConfig {
            name: "filesystem".into(),
            endpoint: "http://127.0.0.1:19999/mcp".into(),
            enabled: true,
            tools: vec![StrictToolSchema {
                name: "read_file".into(),
                description: Some("Read workspace file".into()),
                parameters: json!({"type": "object", "properties": {"path": {"type": "string"}}}),
                strict: true,
                cache_control: None,
            }],
            transport: McpTransport::Http,
            command: None,
            args: Vec::new(),
        }]));
        let mut pipe = MiddlewarePipeline::new();
        pipe.push(bridge);

        let req = CanonicalRequest {
            model: "claude-sonnet-4-20250514".into(),
            system: vec![],
            messages: vec![CanonicalMessage {
                role: Role::User,
                parts: vec![ContentPart::Text {
                    text: "list files".into(),
                    cache_control: None,
                }],
            }],
            tools: vec![],
            tool_choice: None,
            max_tokens: None,
            temperature: None,
            top_p: None,
            stream: false,
            reasoning: None,
            opaque_state: Default::default(),
            session_id: None,
            task_stage: None,
            extra_fields: Default::default(),
        };
        let ctx = MiddlewareContext::new(
            "r1".into(),
            UpstreamProtocol::Anthropic,
            "c1".into(),
            "default".into(),
            req,
        );

        let (final_ctx, res) = pipe
            .execute(ctx, |c| {
                Box::pin(async move {
                    assert_eq!(c.request.tools.len(), 1);
                    assert_eq!(c.request.tools[0].name, "mcp__filesystem__read_file");
                    assert!(c.request.tools[0].strict);
                    (c, Ok::<_, ()>(()))
                })
            })
            .await
            .unwrap();
        res.unwrap();

        assert_eq!(
            final_ctx
                .annotations
                .get("X-Gateway-Mcp-Tools")
                .map(String::as_str),
            Some("1")
        );
    }

    #[test]
    fn config_deserialization_stays_backward_compatible_for_http_and_stdio() {
        // Legacy HTTP TOML without transport/command/args
        let legacy_toml = r#"
name = "legacy_http"
endpoint = "http://127.0.0.1:8080/mcp"
enabled = true
"#;
        let legacy: McpServerConfig = toml::from_str(legacy_toml).unwrap();
        assert_eq!(legacy.effective_transport(), McpTransport::Http);
        assert_eq!(legacy.endpoint, "http://127.0.0.1:8080/mcp");

        // Tagged stdio transport TOML
        let tagged_toml = r#"
name = "tagged_stdio"
[transport]
type = "stdio"
command = "node"
args = ["-e", "console.log(1)"]
"#;
        let tagged: McpServerConfig = toml::from_str(tagged_toml).unwrap();
        assert_eq!(
            tagged.effective_transport(),
            McpTransport::Stdio {
                command: "node".into(),
                args: vec!["-e".into(), "console.log(1)".into()],
            }
        );

        // Shorthand flat command/args JSON
        let flat_json = json!({
            "name": "flat_stdio",
            "command": "node",
            "args": ["--version"]
        });
        let flat: McpServerConfig = serde_json::from_value(flat_json).unwrap();
        assert_eq!(
            flat.effective_transport(),
            McpTransport::Stdio {
                command: "node".into(),
                args: vec!["--version".into()],
            }
        );
    }

    #[tokio::test]
    async fn stdio_jsonrpc_discovers_and_calls_tools_via_node() {
        let script = r#"
const rl = require('readline').createInterface({ input: process.stdin });
rl.once('line', (line) => {
  const req = JSON.parse(line);
  if (req.method === 'tools/list') {
    process.stdout.write(JSON.stringify({
      jsonrpc: '2.0',
      id: req.id,
      result: {
        tools: [{
          name: 'echo_msg',
          description: 'Echoes message back',
          inputSchema: { type: 'object', properties: { text: { type: 'string' } } }
        }]
      }
    }) + '\n');
  } else if (req.method === 'tools/call') {
    process.stdout.write(JSON.stringify({
      jsonrpc: '2.0',
      id: req.id,
      result: {
        echoed: req.params.name + ':' + (req.params.arguments && req.params.arguments.text || '')
      }
    }) + '\n');
  }
  rl.close();
});
"#;

        let bridge = McpBridgeMiddleware::new(vec![McpServerConfig {
            name: "local_node".into(),
            endpoint: String::new(),
            enabled: true,
            tools: Vec::new(),
            transport: McpTransport::Stdio {
                command: "node".into(),
                args: vec!["-e".into(), script.into()],
            },
            command: None,
            args: Vec::new(),
        }]);

        let count = bridge
            .discover_tools("local_node")
            .await
            .expect("stdio tools/list should succeed");
        assert_eq!(count, 1);

        let tools = bridge.aggregated_tools();
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].name, "mcp__local_node__echo_msg");

        let call_res = bridge
            .call_tool("mcp__local_node__echo_msg", json!({ "text": "hello_mcp" }))
            .await
            .expect("stdio tools/call should succeed");
        assert_eq!(call_res, json!({ "echoed": "echo_msg:hello_mcp" }));
    }
}
