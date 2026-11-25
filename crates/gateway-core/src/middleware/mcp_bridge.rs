use crate::ir::StrictToolSchema;
use crate::middleware::{BoxFuture, Middleware, MiddlewareContext, MiddlewareRejection, NextFn};
use dashmap::DashMap;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::HashSet;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpServerConfig {
    pub name: String,
    pub endpoint: String,
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Optional statically declared tools (or hydrated dynamically via `discover_tools`).
    #[serde(default)]
    pub tools: Vec<StrictToolSchema>,
}

fn default_true() -> bool {
    true
}

/// Unified MCP Server Aggregation Bridge Middleware.
/// - Aggregates tools from all enabled MCP servers into `ctx.request.tools` under the
///   canonical namespace `mcp__<server>__<tool>` without clobbering client-defined tools.
/// - Provides JSON-RPC 2.0 `tools/list` discovery and `tools/call` invocation over HTTP.
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

    /// Calls `tools/list` on a registered HTTP MCP server and caches its tool schemas.
    pub async fn discover_tools(&self, server_name: &str) -> Result<usize, String> {
        let endpoint = self
            .servers
            .get(server_name)
            .map(|s| s.endpoint.clone())
            .ok_or_else(|| format!("unknown MCP server '{}'", server_name))?;

        let resp = self
            .http
            .post(&endpoint)
            .json(&json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "tools/list",
                "params": {}
            }))
            .send()
            .await
            .map_err(|e| e.to_string())?;

        let body: Value = resp.json().await.map_err(|e| e.to_string())?;
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

        let endpoint = self
            .servers
            .get(server_name)
            .map(|s| s.endpoint.clone())
            .ok_or_else(|| format!("unknown MCP server '{}'", server_name))?;

        let resp = self
            .http
            .post(&endpoint)
            .json(&json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "tools/call",
                "params": {
                    "name": tool_name,
                    "arguments": arguments
                }
            }))
            .send()
            .await
            .map_err(|e| e.to_string())?;

        let body: Value = resp.json().await.map_err(|e| e.to_string())?;
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
}
