use crate::codec::{CodecError, ProtocolCodec};
use crate::config::UpstreamProtocol;
use crate::ir::{
    CacheControl, CanonicalMessage, CanonicalRequest, CanonicalResponse, CanonicalStreamEvent,
    CanonicalUsage, ContentPart, OpaqueProviderState, ReasoningConfig, Role, StrictToolSchema,
};
use serde_json::{json, Map, Value};
use std::collections::BTreeMap;

pub struct AnthropicCodec;

fn parse_cache_control(v: Option<&Value>) -> Option<CacheControl> {
    let obj = v?.as_object()?;
    if obj.get("type").and_then(|t| t.as_str()) == Some("ephemeral") {
        let ttl = obj.get("ttl").and_then(|t| t.as_str()).map(String::from);
        return Some(CacheControl::Ephemeral { ttl });
    }
    None
}

fn encode_cache_control(cc: &CacheControl) -> Value {
    match cc {
        CacheControl::Ephemeral { ttl: Some(ttl) } => json!({ "type": "ephemeral", "ttl": ttl }),
        CacheControl::Ephemeral { ttl: None } => json!({ "type": "ephemeral" }),
    }
}

fn parse_content_blocks(val: &Value) -> Vec<ContentPart> {
    if let Some(s) = val.as_str() {
        return vec![ContentPart::Text {
            text: s.to_string(),
            cache_control: None,
        }];
    }
    let Some(arr) = val.as_array() else {
        return vec![];
    };
    arr.iter()
        .map(|block| {
            let btype = block.get("type").and_then(|t| t.as_str()).unwrap_or("");
            let cc = parse_cache_control(block.get("cache_control"));
            match btype {
                "text" => ContentPart::Text {
                    text: block
                        .get("text")
                        .and_then(|t| t.as_str())
                        .unwrap_or("")
                        .to_string(),
                    cache_control: cc,
                },
                "thinking" => ContentPart::Thinking {
                    thinking: block
                        .get("thinking")
                        .and_then(|t| t.as_str())
                        .unwrap_or("")
                        .to_string(),
                    signature: block
                        .get("signature")
                        .and_then(|s| s.as_str())
                        .map(String::from),
                },
                "tool_use" => ContentPart::ToolCall {
                    id: block
                        .get("id")
                        .and_then(|s| s.as_str())
                        .unwrap_or("")
                        .to_string(),
                    name: block
                        .get("name")
                        .and_then(|s| s.as_str())
                        .unwrap_or("")
                        .to_string(),
                    arguments: block.get("input").cloned().unwrap_or_else(|| json!({})),
                    cache_control: cc,
                },
                "tool_result" => ContentPart::ToolResult {
                    tool_call_id: block
                        .get("tool_use_id")
                        .and_then(|s| s.as_str())
                        .unwrap_or("")
                        .to_string(),
                    content: block.get("content").cloned().unwrap_or(Value::Null),
                    is_error: block
                        .get("is_error")
                        .and_then(|b| b.as_bool())
                        .unwrap_or(false),
                    cache_control: cc,
                },
                _ => ContentPart::Opaque { raw: block.clone() },
            }
        })
        .collect()
}

fn encode_content_part(part: &ContentPart) -> Value {
    match part {
        ContentPart::Text { text, cache_control } => {
            let mut obj = Map::new();
            obj.insert("type".into(), json!("text"));
            obj.insert("text".into(), json!(text));
            if let Some(cc) = cache_control {
                obj.insert("cache_control".into(), encode_cache_control(cc));
            }
            Value::Object(obj)
        }
        ContentPart::Thinking { thinking, signature } => {
            let mut obj = Map::new();
            obj.insert("type".into(), json!("thinking"));
            obj.insert("thinking".into(), json!(thinking));
            if let Some(sig) = signature {
                obj.insert("signature".into(), json!(sig));
            }
            Value::Object(obj)
        }
        ContentPart::ToolCall {
            id,
            name,
            arguments,
            cache_control,
        } => {
            let mut obj = Map::new();
            obj.insert("type".into(), json!("tool_use"));
            obj.insert("id".into(), json!(id));
            obj.insert("name".into(), json!(name));
            obj.insert("input".into(), arguments.clone());
            if let Some(cc) = cache_control {
                obj.insert("cache_control".into(), encode_cache_control(cc));
            }
            Value::Object(obj)
        }
        ContentPart::ToolResult {
            tool_call_id,
            content,
            is_error,
            cache_control,
        } => {
            let mut obj = Map::new();
            obj.insert("type".into(), json!("tool_result"));
            obj.insert("tool_use_id".into(), json!(tool_call_id));
            obj.insert("content".into(), content.clone());
            if *is_error {
                obj.insert("is_error".into(), json!(true));
            }
            if let Some(cc) = cache_control {
                obj.insert("cache_control".into(), encode_cache_control(cc));
            }
            Value::Object(obj)
        }
        ContentPart::Opaque { raw } => raw.clone(),
    }
}

impl ProtocolCodec for AnthropicCodec {
    fn protocol(&self) -> UpstreamProtocol {
        UpstreamProtocol::Anthropic
    }

    fn decode_request(
        &self,
        payload: &Value,
        session_id: Option<String>,
        task_stage: Option<String>,
    ) -> Result<CanonicalRequest, CodecError> {
        let obj = payload
            .as_object()
            .ok_or_else(|| CodecError::InvalidPayload("expected JSON object".into()))?;

        let model = obj
            .get("model")
            .and_then(|v| v.as_str())
            .ok_or_else(|| CodecError::InvalidPayload("missing 'model'".into()))?
            .to_string();

        let system = obj
            .get("system")
            .map(parse_content_blocks)
            .unwrap_or_default();

        let messages = obj
            .get("messages")
            .and_then(|v| v.as_array())
            .ok_or_else(|| CodecError::InvalidPayload("missing 'messages' array".into()))?
            .iter()
            .map(|m| {
                let role = match m.get("role").and_then(|r| r.as_str()).unwrap_or("user") {
                    "assistant" => Role::Assistant,
                    "system" => Role::System,
                    _ => Role::User,
                };
                let parts = m
                    .get("content")
                    .map(parse_content_blocks)
                    .unwrap_or_default();
                CanonicalMessage { role, parts }
            })
            .collect();

        let tools = obj
            .get("tools")
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|t| {
                        let name = t.get("name")?.as_str()?.to_string();
                        let description = t
                            .get("description")
                            .and_then(|d| d.as_str())
                            .map(String::from);
                        let parameters = t
                            .get("input_schema")
                            .cloned()
                            .unwrap_or_else(|| json!({"type": "object"}));
                        let strict = t
                            .get("strict")
                            .and_then(|b| b.as_bool())
                            .unwrap_or(false);
                        let cache_control = parse_cache_control(t.get("cache_control"));
                        Some(StrictToolSchema {
                            name,
                            description,
                            parameters,
                            strict,
                            cache_control,
                        })
                    })
                    .collect()
            })
            .unwrap_or_default();

        let reasoning = obj.get("thinking").map(|t| {
            let enabled = t.get("type").and_then(|x| x.as_str()) == Some("enabled");
            let budget_tokens = t
                .get("budget_tokens")
                .and_then(|b| b.as_u64())
                .map(|b| b as u32);
            ReasoningConfig {
                enabled,
                budget_tokens,
                effort: None,
            }
        });

        let derived_session = session_id.or_else(|| {
            obj.get("metadata")
                .and_then(|m| m.get("user_id"))
                .and_then(|u| u.as_str())
                .map(String::from)
        });

        let known_keys = [
            "model",
            "system",
            "messages",
            "tools",
            "tool_choice",
            "max_tokens",
            "temperature",
            "top_p",
            "stream",
            "thinking",
        ];
        let mut extra_fields = BTreeMap::new();
        for (k, v) in obj {
            if !known_keys.contains(&k.as_str()) {
                extra_fields.insert(k.clone(), v.clone());
            }
        }

        Ok(CanonicalRequest {
            model,
            system,
            messages,
            tools,
            tool_choice: obj.get("tool_choice").cloned(),
            max_tokens: obj
                .get("max_tokens")
                .and_then(|v| v.as_u64())
                .map(|v| v as u32),
            temperature: obj
                .get("temperature")
                .and_then(|v| v.as_f64())
                .map(|v| v as f32),
            top_p: obj.get("top_p").and_then(|v| v.as_f64()).map(|v| v as f32),
            stream: obj.get("stream").and_then(|v| v.as_bool()).unwrap_or(false),
            reasoning,
            opaque_state: OpaqueProviderState::default(),
            session_id: derived_session,
            task_stage,
            extra_fields,
        })
    }

    fn encode_request(
        &self,
        req: &CanonicalRequest,
        upstream_model: &str,
        same_protocol: bool,
    ) -> Result<Value, CodecError> {
        let mut out = Map::new();
        out.insert("model".into(), json!(upstream_model));
        out.insert("max_tokens".into(), json!(req.max_tokens.unwrap_or(4096)));
        out.insert("stream".into(), json!(req.stream));

        if !req.system.is_empty() {
            let sys_blocks: Vec<Value> = req.system.iter().map(encode_content_part).collect();
            out.insert("system".into(), Value::Array(sys_blocks));
        }

        let msgs: Vec<Value> = req
            .messages
            .iter()
            .map(|m| {
                let role_str = match m.role {
                    Role::Assistant => "assistant",
                    _ => "user",
                };
                let content: Vec<Value> = m.parts.iter().map(encode_content_part).collect();
                json!({ "role": role_str, "content": content })
            })
            .collect();
        out.insert("messages".into(), Value::Array(msgs));

        if !req.tools.is_empty() {
            let tools: Vec<Value> = req
                .tools
                .iter()
                .map(|t| {
                    let mut tobj = Map::new();
                    tobj.insert("name".into(), json!(t.name));
                    if let Some(ref desc) = t.description {
                        tobj.insert("description".into(), json!(desc));
                    }
                    tobj.insert("input_schema".into(), t.parameters.clone());
                    if let Some(ref cc) = t.cache_control {
                        tobj.insert("cache_control".into(), encode_cache_control(cc));
                    }
                    Value::Object(tobj)
                })
                .collect();
            out.insert("tools".into(), Value::Array(tools));
        }

        if let Some(ref tc) = req.tool_choice {
            out.insert("tool_choice".into(), tc.clone());
        }
        if let Some(temp) = req.temperature {
            out.insert("temperature".into(), json!(temp));
        }
        if let Some(tp) = req.top_p {
            out.insert("top_p".into(), json!(tp));
        }
        if let Some(ref r) = req.reasoning {
            if r.enabled {
                out.insert(
                    "thinking".into(),
                    json!({
                        "type": "enabled",
                        "budget_tokens": r.budget_tokens.unwrap_or(4096),
                    }),
                );
            }
        }
        // Only forward protocol-specific extra_fields on same-protocol passthrough.
        if same_protocol {
            for (k, v) in &req.extra_fields {
                out.entry(k.clone()).or_insert_with(|| v.clone());
            }
        }

        Ok(Value::Object(out))
    }

    fn decode_response(&self, body: &Value) -> Result<CanonicalResponse, CodecError> {
        let id = body
            .get("id")
            .and_then(|v| v.as_str())
            .unwrap_or("msg_unknown")
            .to_string();
        let model = body
            .get("model")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let parts = body
            .get("content")
            .map(parse_content_blocks)
            .unwrap_or_default();
        let stop_reason = body
            .get("stop_reason")
            .and_then(|v| v.as_str())
            .map(String::from);
        let usage = body
            .get("usage")
            .map(parse_anthropic_usage)
            .unwrap_or_default();

        Ok(CanonicalResponse {
            id,
            model,
            parts,
            stop_reason,
            usage,
        })
    }

    fn encode_response(&self, resp: &CanonicalResponse) -> Value {
        let content: Vec<Value> = resp.parts.iter().map(encode_content_part).collect();
        let stop_reason = match resp.stop_reason.as_deref() {
            Some("stop") => Some("end_turn"),
            Some("length") => Some("max_tokens"),
            Some("tool_calls") => Some("tool_use"),
            other => other,
        };
        json!({
            "id": resp.id,
            "type": "message",
            "role": "assistant",
            "model": resp.model,
            "content": content,
            "stop_reason": stop_reason.unwrap_or("end_turn"),
            "stop_sequence": null,
            "usage": {
                "input_tokens": resp.usage.input_tokens,
                "output_tokens": resp.usage.output_tokens,
                "cache_creation_input_tokens": resp.usage.cache_creation_input_tokens,
                "cache_read_input_tokens": resp.usage.cache_read_input_tokens
            }
        })
    }

    fn decode_sse_frame(&self, event_name: Option<&str>, data: &str) -> Vec<CanonicalStreamEvent> {
        let Ok(v) = serde_json::from_str::<Value>(data) else {
            return vec![];
        };
        let etype = event_name
            .or_else(|| v.get("type").and_then(|t| t.as_str()))
            .unwrap_or("");

        match etype {
            "message_start" => {
                let msg = v.get("message");
                let id = msg
                    .and_then(|m| m.get("id"))
                    .and_then(|s| s.as_str())
                    .unwrap_or("msg_unknown")
                    .to_string();
                let model = msg
                    .and_then(|m| m.get("model"))
                    .and_then(|s| s.as_str())
                    .unwrap_or("")
                    .to_string();
                let usage = msg.and_then(|m| m.get("usage")).map(parse_anthropic_usage);
                vec![CanonicalStreamEvent::MessageStart { id, model, usage }]
            }
            "content_block_start" => {
                let index = v.get("index").and_then(|i| i.as_u64()).unwrap_or(0) as usize;
                let cb = v.get("content_block");
                let btype = cb
                    .and_then(|b| b.get("type"))
                    .and_then(|t| t.as_str())
                    .unwrap_or("text");
                if btype == "tool_use" {
                    let id = cb
                        .and_then(|b| b.get("id"))
                        .and_then(|s| s.as_str())
                        .unwrap_or("")
                        .to_string();
                    let name = cb
                        .and_then(|b| b.get("name"))
                        .and_then(|s| s.as_str())
                        .unwrap_or("")
                        .to_string();
                    vec![CanonicalStreamEvent::ToolCallStart { index, id, name }]
                } else {
                    vec![CanonicalStreamEvent::ContentBlockStart {
                        index,
                        block_type: btype.to_string(),
                    }]
                }
            }
            "content_block_stop" => {
                let index = v.get("index").and_then(|i| i.as_u64()).unwrap_or(0) as usize;
                vec![CanonicalStreamEvent::ContentBlockStop { index }]
            }
            "content_block_delta" => {
                let index = v.get("index").and_then(|i| i.as_u64()).unwrap_or(0) as usize;
                let Some(delta) = v.get("delta") else {
                    return vec![];
                };
                match delta.get("type").and_then(|t| t.as_str()).unwrap_or("") {
                    "text_delta" => delta
                        .get("text")
                        .and_then(|t| t.as_str())
                        .map(|t| {
                            vec![CanonicalStreamEvent::TextDelta {
                                index,
                                text: t.to_string(),
                            }]
                        })
                        .unwrap_or_default(),
                    "thinking_delta" => delta
                        .get("thinking")
                        .and_then(|t| t.as_str())
                        .map(|t| {
                            vec![CanonicalStreamEvent::ThinkingDelta {
                                index,
                                thinking: t.to_string(),
                            }]
                        })
                        .unwrap_or_default(),
                    "signature_delta" => delta
                        .get("signature")
                        .and_then(|s| s.as_str())
                        .map(|s| {
                            vec![CanonicalStreamEvent::SignatureDelta {
                                index,
                                signature: s.to_string(),
                            }]
                        })
                        .unwrap_or_default(),
                    "input_json_delta" => delta
                        .get("partial_json")
                        .and_then(|s| s.as_str())
                        .map(|s| {
                            vec![CanonicalStreamEvent::ToolCallDelta {
                                index,
                                arguments_delta: s.to_string(),
                            }]
                        })
                        .unwrap_or_default(),
                    _ => vec![],
                }
            }
            "message_delta" => {
                let mut events = vec![];
                if let Some(u) = v.get("usage") {
                    events.push(CanonicalStreamEvent::UsageDelta {
                        usage: parse_anthropic_usage(u),
                    });
                }
                if let Some(reason) = v
                    .get("delta")
                    .and_then(|d| d.get("stop_reason"))
                    .and_then(|r| r.as_str())
                {
                    events.push(CanonicalStreamEvent::MessageStop {
                        stop_reason: Some(reason.to_string()),
                    });
                }
                events
            }
            "message_stop" => vec![CanonicalStreamEvent::MessageStop { stop_reason: None }],
            "error" => {
                let message = v
                    .get("error")
                    .and_then(|e| e.get("message"))
                    .and_then(|m| m.as_str())
                    .unwrap_or("upstream stream error")
                    .to_string();
                vec![CanonicalStreamEvent::Error { message }]
            }
            _ => vec![],
        }
    }

    fn encode_sse_event(&self, event: &CanonicalStreamEvent) -> Vec<String> {
        match event {
            CanonicalStreamEvent::MessageStart { id, model, usage } => {
                let u = usage.clone().unwrap_or_default();
                let payload = json!({
                    "type": "message_start",
                    "message": {
                        "id": id,
                        "type": "message",
                        "role": "assistant",
                        "model": model,
                        "content": [],
                        "usage": {
                            "input_tokens": u.input_tokens,
                            "output_tokens": u.output_tokens,
                            "cache_creation_input_tokens": u.cache_creation_input_tokens,
                            "cache_read_input_tokens": u.cache_read_input_tokens
                        }
                    }
                });
                vec![format!("event: message_start\ndata: {}\n\n", payload)]
            }
            CanonicalStreamEvent::ContentBlockStart { index, block_type } => {
                let block = if block_type == "thinking" {
                    json!({ "type": "thinking", "thinking": "" })
                } else {
                    json!({ "type": "text", "text": "" })
                };
                let payload = json!({
                    "type": "content_block_start",
                    "index": index,
                    "content_block": block
                });
                vec![format!("event: content_block_start\ndata: {}\n\n", payload)]
            }
            CanonicalStreamEvent::ContentBlockStop { index } => {
                let payload = json!({
                    "type": "content_block_stop",
                    "index": index
                });
                vec![format!("event: content_block_stop\ndata: {}\n\n", payload)]
            }
            CanonicalStreamEvent::TextDelta { index, text } => {
                let payload = json!({
                    "type": "content_block_delta",
                    "index": index,
                    "delta": { "type": "text_delta", "text": text }
                });
                vec![format!("event: content_block_delta\ndata: {}\n\n", payload)]
            }
            CanonicalStreamEvent::ThinkingDelta { index, thinking } => {
                let payload = json!({
                    "type": "content_block_delta",
                    "index": index,
                    "delta": { "type": "thinking_delta", "thinking": thinking }
                });
                vec![format!("event: content_block_delta\ndata: {}\n\n", payload)]
            }
            CanonicalStreamEvent::SignatureDelta { index, signature } => {
                let payload = json!({
                    "type": "content_block_delta",
                    "index": index,
                    "delta": { "type": "signature_delta", "signature": signature }
                });
                vec![format!("event: content_block_delta\ndata: {}\n\n", payload)]
            }
            CanonicalStreamEvent::ToolCallStart { index, id, name } => {
                let payload = json!({
                    "type": "content_block_start",
                    "index": index,
                    "content_block": { "type": "tool_use", "id": id, "name": name, "input": {} }
                });
                vec![format!("event: content_block_start\ndata: {}\n\n", payload)]
            }
            CanonicalStreamEvent::ToolCallDelta {
                index,
                arguments_delta,
            } => {
                let payload = json!({
                    "type": "content_block_delta",
                    "index": index,
                    "delta": { "type": "input_json_delta", "partial_json": arguments_delta }
                });
                vec![format!("event: content_block_delta\ndata: {}\n\n", payload)]
            }
            CanonicalStreamEvent::UsageDelta { usage } => {
                let payload = json!({
                    "type": "message_delta",
                    "delta": {},
                    "usage": {
                        "output_tokens": usage.output_tokens,
                        "cache_read_input_tokens": usage.cache_read_input_tokens
                    }
                });
                vec![format!("event: message_delta\ndata: {}\n\n", payload)]
            }
            CanonicalStreamEvent::MessageStop { stop_reason } => {
                let mut frames = vec![];
                if let Some(reason) = stop_reason {
                    let mapped = match reason.as_str() {
                        "stop" => "end_turn",
                        "length" => "max_tokens",
                        "tool_calls" => "tool_use",
                        other => other,
                    };
                    let delta = json!({
                        "type": "message_delta",
                        "delta": { "stop_reason": mapped }
                    });
                    frames.push(format!("event: message_delta\ndata: {}\n\n", delta));
                }
                frames.push("event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n".into());
                frames
            }
            CanonicalStreamEvent::Error { message } => {
                let payload = json!({
                    "type": "error",
                    "error": { "type": "api_error", "message": message }
                });
                vec![format!("event: error\ndata: {}\n\n", payload)]
            }
        }
    }

    fn encode_error(&self, error_type: &str, message: &str) -> Value {
        json!({
            "type": "error",
            "error": {
                "type": error_type,
                "message": message
            }
        })
    }
}

fn parse_anthropic_usage(u: &Value) -> CanonicalUsage {
    CanonicalUsage {
        input_tokens: u.get("input_tokens").and_then(|x| x.as_u64()).unwrap_or(0),
        output_tokens: u.get("output_tokens").and_then(|x| x.as_u64()).unwrap_or(0),
        cache_creation_input_tokens: u
            .get("cache_creation_input_tokens")
            .and_then(|x| x.as_u64())
            .unwrap_or(0),
        cache_read_input_tokens: u
            .get("cache_read_input_tokens")
            .and_then(|x| x.as_u64())
            .unwrap_or(0),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lossless_roundtrip_preserves_cache_control_thinking_and_block_events() {
        let codec = AnthropicCodec;
        let input = json!({
            "model": "claude-sonnet-4-20250514",
            "max_tokens": 2048,
            "stream": true,
            "thinking": { "type": "enabled", "budget_tokens": 1024 },
            "system": [{ "type": "text", "text": "System prompt", "cache_control": { "type": "ephemeral" } }],
            "messages": [
                {
                    "role": "assistant",
                    "content": [
                        { "type": "thinking", "thinking": "step 1", "signature": "sig_opaque_xyz" },
                        { "type": "text", "text": "hello" }
                    ]
                }
            ]
        });

        let ir = codec.decode_request(&input, None, None).unwrap();
        assert!(ir.has_cache_breakpoints());
        assert_eq!(ir.reasoning.as_ref().unwrap().budget_tokens, Some(1024));

        let encoded = codec
            .encode_request(&ir, "claude-sonnet-4-20250514", true)
            .unwrap();
        assert_eq!(
            encoded["system"][0]["cache_control"]["type"],
            json!("ephemeral")
        );
        assert_eq!(
            encoded["messages"][0]["content"][0]["signature"],
            json!("sig_opaque_xyz")
        );

        let start_ev = codec.decode_sse_frame(
            Some("content_block_start"),
            r#"{"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}"#,
        );
        assert_eq!(
            start_ev,
            vec![CanonicalStreamEvent::ContentBlockStart {
                index: 0,
                block_type: "text".into()
            }]
        );
        let stop_ev = codec.decode_sse_frame(
            Some("content_block_stop"),
            r#"{"type":"content_block_stop","index":0}"#,
        );
        assert_eq!(
            stop_ev,
            vec![CanonicalStreamEvent::ContentBlockStop { index: 0 }]
        );
    }
}
