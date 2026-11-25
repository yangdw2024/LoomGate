use crate::codec::{CodecError, ProtocolCodec};
use crate::config::UpstreamProtocol;
use crate::ir::{
    CanonicalMessage, CanonicalRequest, CanonicalResponse, CanonicalStreamEvent, CanonicalUsage,
    ContentPart, OpaqueProviderState, ReasoningConfig, Role, StrictToolSchema,
};
use serde_json::{json, Map, Value};
use std::collections::BTreeMap;

pub struct OpenAiCodec;

impl ProtocolCodec for OpenAiCodec {
    fn protocol(&self) -> UpstreamProtocol {
        UpstreamProtocol::OpenAi
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

        let raw_msgs = obj
            .get("messages")
            .and_then(|v| v.as_array())
            .ok_or_else(|| CodecError::InvalidPayload("missing 'messages' array".into()))?;

        let mut system = vec![];
        let mut messages = vec![];

        for m in raw_msgs {
            let role_str = m.get("role").and_then(|r| r.as_str()).unwrap_or("user");
            let content_val = m.get("content");

            if role_str == "system" || role_str == "developer" {
                if let Some(text) = content_val.and_then(|c| c.as_str()) {
                    system.push(ContentPart::Text {
                        text: text.to_string(),
                        cache_control: None,
                    });
                }
                continue;
            }

            if role_str == "tool" {
                let tool_call_id = m
                    .get("tool_call_id")
                    .and_then(|s| s.as_str())
                    .unwrap_or("")
                    .to_string();
                let content = content_val.cloned().unwrap_or(Value::Null);
                messages.push(CanonicalMessage {
                    role: Role::Tool,
                    parts: vec![ContentPart::ToolResult {
                        tool_call_id,
                        content,
                        is_error: false,
                        cache_control: None,
                    }],
                });
                continue;
            }

            let role = match role_str {
                "assistant" => Role::Assistant,
                _ => Role::User,
            };
            let mut parts = vec![];
            if let Some(text) = content_val.and_then(|c| c.as_str()) {
                if !text.is_empty() {
                    parts.push(ContentPart::Text {
                        text: text.to_string(),
                        cache_control: None,
                    });
                }
            }
            if let Some(tcs) = m.get("tool_calls").and_then(|v| v.as_array()) {
                for tc in tcs {
                    let id = tc
                        .get("id")
                        .and_then(|s| s.as_str())
                        .unwrap_or("")
                        .to_string();
                    let func = tc.get("function");
                    let name = func
                        .and_then(|f| f.get("name"))
                        .and_then(|s| s.as_str())
                        .unwrap_or("")
                        .to_string();
                    let args_str = func
                        .and_then(|f| f.get("arguments"))
                        .and_then(|s| s.as_str())
                        .unwrap_or("{}");
                    let arguments = serde_json::from_str(args_str).unwrap_or_else(|_| json!({}));
                    parts.push(ContentPart::ToolCall {
                        id,
                        name,
                        arguments,
                        cache_control: None,
                    });
                }
            }
            messages.push(CanonicalMessage { role, parts });
        }

        let tools = obj
            .get("tools")
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|t| {
                        let func = t.get("function")?;
                        let name = func.get("name")?.as_str()?.to_string();
                        let description = func
                            .get("description")
                            .and_then(|d| d.as_str())
                            .map(String::from);
                        let parameters = func
                            .get("parameters")
                            .cloned()
                            .unwrap_or_else(|| json!({"type": "object"}));
                        let strict = func
                            .get("strict")
                            .and_then(|b| b.as_bool())
                            .unwrap_or(false);
                        Some(StrictToolSchema {
                            name,
                            description,
                            parameters,
                            strict,
                            cache_control: None,
                        })
                    })
                    .collect()
            })
            .unwrap_or_default();

        let reasoning = obj
            .get("reasoning_effort")
            .and_then(|e| e.as_str())
            .map(|effort| ReasoningConfig {
                enabled: true,
                budget_tokens: None,
                effort: Some(effort.to_string()),
            });

        let derived_session = session_id.or_else(|| {
            obj.get("user")
                .and_then(|u| u.as_str())
                .map(String::from)
        });

        let known_keys = [
            "model",
            "messages",
            "tools",
            "tool_choice",
            "max_tokens",
            "max_completion_tokens",
            "temperature",
            "top_p",
            "stream",
            "reasoning_effort",
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
                .get("max_completion_tokens")
                .or_else(|| obj.get("max_tokens"))
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
        out.insert("stream".into(), json!(req.stream));
        if req.stream {
            out.insert("stream_options".into(), json!({ "include_usage": true }));
        }

        let mut msgs = vec![];
        if !req.system.is_empty() {
            let sys_text: String = req
                .system
                .iter()
                .filter_map(|p| match p {
                    ContentPart::Text { text, .. } => Some(text.as_str()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join("\n");
            if !sys_text.is_empty() {
                msgs.push(json!({ "role": "system", "content": sys_text }));
            }
        }

        for m in &req.messages {
            let mut text_buf = String::new();
            let mut tool_calls = vec![];
            let mut tool_results = vec![];

            for p in &m.parts {
                match p {
                    ContentPart::Text { text, .. } => {
                        if !text_buf.is_empty() {
                            text_buf.push('\n');
                        }
                        text_buf.push_str(text);
                    }
                    ContentPart::ToolCall {
                        id,
                        name,
                        arguments,
                        ..
                    } => {
                        tool_calls.push(json!({
                            "id": id,
                            "type": "function",
                            "function": {
                                "name": name,
                                "arguments": arguments.to_string()
                            }
                        }));
                    }
                    ContentPart::ToolResult {
                        tool_call_id,
                        content,
                        ..
                    } => {
                        let content_str = match content {
                            Value::String(s) => s.clone(),
                            other => other.to_string(),
                        };
                        tool_results.push(json!({
                            "role": "tool",
                            "tool_call_id": tool_call_id,
                            "content": content_str
                        }));
                    }
                    _ => {}
                }
            }

            if !text_buf.is_empty() || !tool_calls.is_empty() {
                let role_str = match m.role {
                    Role::Assistant => "assistant",
                    Role::System => "system",
                    _ => "user",
                };
                let mut mobj = Map::new();
                mobj.insert("role".into(), json!(role_str));
                mobj.insert(
                    "content".into(),
                    if text_buf.is_empty() {
                        Value::Null
                    } else {
                        json!(text_buf)
                    },
                );
                if !tool_calls.is_empty() {
                    mobj.insert("tool_calls".into(), Value::Array(tool_calls));
                }
                msgs.push(Value::Object(mobj));
            }
            msgs.extend(tool_results);
        }

        out.insert("messages".into(), Value::Array(msgs));

        if !req.tools.is_empty() {
            let tools: Vec<Value> = req
                .tools
                .iter()
                .map(|t| {
                    let mut func = Map::new();
                    func.insert("name".into(), json!(t.name));
                    if let Some(ref desc) = t.description {
                        func.insert("description".into(), json!(desc));
                    }
                    func.insert("parameters".into(), t.parameters.clone());
                    if t.strict {
                        func.insert("strict".into(), json!(true));
                    }
                    json!({ "type": "function", "function": Value::Object(func) })
                })
                .collect();
            out.insert("tools".into(), Value::Array(tools));
        }

        // OpenAI o1/o3/o4 reasoning models require `max_completion_tokens` instead of `max_tokens`.
        if let Some(max_t) = req.max_tokens {
            let is_o_series = upstream_model.starts_with("o1")
                || upstream_model.starts_with("o3")
                || upstream_model.starts_with("o4");
            if is_o_series {
                out.insert("max_completion_tokens".into(), json!(max_t));
            } else {
                out.insert("max_tokens".into(), json!(max_t));
            }
        }
        if let Some(temp) = req.temperature {
            out.insert("temperature".into(), json!(temp));
        }
        if let Some(tp) = req.top_p {
            out.insert("top_p".into(), json!(tp));
        }
        if let Some(ref r) = req.reasoning {
            if let Some(ref effort) = r.effort {
                out.insert("reasoning_effort".into(), json!(effort));
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
            .unwrap_or("chatcmpl_unknown")
            .to_string();
        let model = body
            .get("model")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();

        let mut parts = vec![];
        let mut stop_reason = None;

        if let Some(choice) = body
            .get("choices")
            .and_then(|c| c.as_array())
            .and_then(|a| a.first())
        {
            stop_reason = choice
                .get("finish_reason")
                .and_then(|v| v.as_str())
                .map(String::from);
            if let Some(msg) = choice.get("message") {
                if let Some(reasoning) = msg.get("reasoning_content").and_then(|r| r.as_str()) {
                    if !reasoning.is_empty() {
                        parts.push(ContentPart::Thinking {
                            thinking: reasoning.to_string(),
                            signature: None,
                        });
                    }
                }
                if let Some(text) = msg.get("content").and_then(|c| c.as_str()) {
                    if !text.is_empty() {
                        parts.push(ContentPart::Text {
                            text: text.to_string(),
                            cache_control: None,
                        });
                    }
                }
                if let Some(tcs) = msg.get("tool_calls").and_then(|t| t.as_array()) {
                    for tc in tcs {
                        let tc_id = tc
                            .get("id")
                            .and_then(|s| s.as_str())
                            .unwrap_or("")
                            .to_string();
                        let func = tc.get("function");
                        let name = func
                            .and_then(|f| f.get("name"))
                            .and_then(|s| s.as_str())
                            .unwrap_or("")
                            .to_string();
                        let args_str = func
                            .and_then(|f| f.get("arguments"))
                            .and_then(|s| s.as_str())
                            .unwrap_or("{}");
                        let arguments =
                            serde_json::from_str(args_str).unwrap_or_else(|_| json!({}));
                        parts.push(ContentPart::ToolCall {
                            id: tc_id,
                            name,
                            arguments,
                            cache_control: None,
                        });
                    }
                }
            }
        }

        let usage = body
            .get("usage")
            .and_then(|u| u.as_object())
            .map(|u| CanonicalUsage {
                input_tokens: u.get("prompt_tokens").and_then(|x| x.as_u64()).unwrap_or(0),
                output_tokens: u
                    .get("completion_tokens")
                    .and_then(|x| x.as_u64())
                    .unwrap_or(0),
                cache_creation_input_tokens: 0,
                cache_read_input_tokens: u
                    .get("prompt_tokens_details")
                    .and_then(|d| d.get("cached_tokens"))
                    .and_then(|c| c.as_u64())
                    .unwrap_or(0),
            })
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
        let text = resp.concatenated_text();
        let tool_calls: Vec<Value> = resp
            .parts
            .iter()
            .filter_map(|p| match p {
                ContentPart::ToolCall {
                    id,
                    name,
                    arguments,
                    ..
                } => Some(json!({
                    "id": id,
                    "type": "function",
                    "function": {
                        "name": name,
                        "arguments": arguments.to_string()
                    }
                })),
                _ => None,
            })
            .collect();

        let finish_reason = match resp.stop_reason.as_deref() {
            Some("end_turn") => "stop",
            Some("max_tokens") => "length",
            Some("tool_use") => "tool_calls",
            Some(other) => other,
            None => "stop",
        };

        let mut msg = Map::new();
        msg.insert("role".into(), json!("assistant"));
        msg.insert(
            "content".into(),
            if text.is_empty() && !tool_calls.is_empty() {
                Value::Null
            } else {
                json!(text)
            },
        );
        if !tool_calls.is_empty() {
            msg.insert("tool_calls".into(), Value::Array(tool_calls));
        }

        json!({
            "id": resp.id,
            "object": "chat.completion",
            "model": resp.model,
            "choices": [{
                "index": 0,
                "message": Value::Object(msg),
                "finish_reason": finish_reason
            }],
            "usage": {
                "prompt_tokens": resp.usage.input_tokens,
                "completion_tokens": resp.usage.output_tokens,
                "total_tokens": resp.usage.input_tokens + resp.usage.output_tokens,
                "prompt_tokens_details": {
                    "cached_tokens": resp.usage.cache_read_input_tokens
                }
            }
        })
    }

    fn decode_sse_frame(&self, _event_name: Option<&str>, data: &str) -> Vec<CanonicalStreamEvent> {
        if data.trim() == "[DONE]" {
            return vec![CanonicalStreamEvent::MessageStop { stop_reason: None }];
        }
        let Ok(v) = serde_json::from_str::<Value>(data) else {
            return vec![];
        };

        let mut events = vec![];
        if let Some(u) = v.get("usage").and_then(|x| x.as_object()) {
            let cached = u
                .get("prompt_tokens_details")
                .and_then(|d| d.get("cached_tokens"))
                .and_then(|c| c.as_u64())
                .unwrap_or(0);
            events.push(CanonicalStreamEvent::UsageDelta {
                usage: CanonicalUsage {
                    input_tokens: u.get("prompt_tokens").and_then(|x| x.as_u64()).unwrap_or(0),
                    output_tokens: u
                        .get("completion_tokens")
                        .and_then(|x| x.as_u64())
                        .unwrap_or(0),
                    cache_creation_input_tokens: 0,
                    cache_read_input_tokens: cached,
                },
            });
        }

        if let Some(choice) = v
            .get("choices")
            .and_then(|c| c.as_array())
            .and_then(|a| a.first())
        {
            let index = choice.get("index").and_then(|i| i.as_u64()).unwrap_or(0) as usize;
            if let Some(delta) = choice.get("delta") {
                if let Some(content) = delta.get("content").and_then(|c| c.as_str()) {
                    if !content.is_empty() {
                        events.push(CanonicalStreamEvent::TextDelta {
                            index,
                            text: content.to_string(),
                        });
                    }
                }
                if let Some(reasoning) = delta.get("reasoning_content").and_then(|c| c.as_str()) {
                    if !reasoning.is_empty() {
                        events.push(CanonicalStreamEvent::ThinkingDelta {
                            index,
                            thinking: reasoning.to_string(),
                        });
                    }
                }
                if let Some(tcs) = delta.get("tool_calls").and_then(|t| t.as_array()) {
                    for tc in tcs {
                        let tc_idx = tc.get("index").and_then(|i| i.as_u64()).unwrap_or(0) as usize;
                        if let Some(id) = tc.get("id").and_then(|s| s.as_str()) {
                            let name = tc
                                .get("function")
                                .and_then(|f| f.get("name"))
                                .and_then(|s| s.as_str())
                                .unwrap_or("")
                                .to_string();
                            events.push(CanonicalStreamEvent::ToolCallStart {
                                index: tc_idx,
                                id: id.to_string(),
                                name,
                            });
                        }
                        if let Some(args) = tc
                            .get("function")
                            .and_then(|f| f.get("arguments"))
                            .and_then(|s| s.as_str())
                        {
                            if !args.is_empty() {
                                events.push(CanonicalStreamEvent::ToolCallDelta {
                                    index: tc_idx,
                                    arguments_delta: args.to_string(),
                                });
                            }
                        }
                    }
                }
            }
            if let Some(reason) = choice.get("finish_reason").and_then(|r| r.as_str()) {
                events.push(CanonicalStreamEvent::MessageStop {
                    stop_reason: Some(reason.to_string()),
                });
            }
        }
        events
    }

    fn encode_sse_event(&self, event: &CanonicalStreamEvent) -> Vec<String> {
        match event {
            CanonicalStreamEvent::MessageStart { id, model, .. } => {
                let chunk = json!({
                    "id": id,
                    "object": "chat.completion.chunk",
                    "model": model,
                    "choices": [{ "index": 0, "delta": { "role": "assistant" }, "finish_reason": null }]
                });
                vec![format!("data: {}\n\n", chunk)]
            }
            CanonicalStreamEvent::ContentBlockStart { .. }
            | CanonicalStreamEvent::ContentBlockStop { .. }
            | CanonicalStreamEvent::SignatureDelta { .. } => vec![],
            CanonicalStreamEvent::TextDelta { index, text } => {
                let chunk = json!({
                    "object": "chat.completion.chunk",
                    "choices": [{ "index": index, "delta": { "content": text }, "finish_reason": null }]
                });
                vec![format!("data: {}\n\n", chunk)]
            }
            CanonicalStreamEvent::ThinkingDelta { index, thinking } => {
                let chunk = json!({
                    "object": "chat.completion.chunk",
                    "choices": [{ "index": index, "delta": { "reasoning_content": thinking }, "finish_reason": null }]
                });
                vec![format!("data: {}\n\n", chunk)]
            }
            CanonicalStreamEvent::ToolCallStart { index, id, name } => {
                let chunk = json!({
                    "object": "chat.completion.chunk",
                    "choices": [{
                        "index": 0,
                        "delta": {
                            "tool_calls": [{
                                "index": index,
                                "id": id,
                                "type": "function",
                                "function": { "name": name, "arguments": "" }
                            }]
                        },
                        "finish_reason": null
                    }]
                });
                vec![format!("data: {}\n\n", chunk)]
            }
            CanonicalStreamEvent::ToolCallDelta {
                index,
                arguments_delta,
            } => {
                let chunk = json!({
                    "object": "chat.completion.chunk",
                    "choices": [{
                        "index": 0,
                        "delta": {
                            "tool_calls": [{
                                "index": index,
                                "function": { "arguments": arguments_delta }
                            }]
                        },
                        "finish_reason": null
                    }]
                });
                vec![format!("data: {}\n\n", chunk)]
            }
            CanonicalStreamEvent::UsageDelta { usage } => {
                let chunk = json!({
                    "object": "chat.completion.chunk",
                    "choices": [],
                    "usage": {
                        "prompt_tokens": usage.input_tokens,
                        "completion_tokens": usage.output_tokens,
                        "prompt_tokens_details": {
                            "cached_tokens": usage.cache_read_input_tokens
                        }
                    }
                });
                vec![format!("data: {}\n\n", chunk)]
            }
            CanonicalStreamEvent::MessageStop { stop_reason } => {
                let mut frames = vec![];
                if let Some(reason) = stop_reason {
                    let mapped = match reason.as_str() {
                        "end_turn" => "stop",
                        "max_tokens" => "length",
                        "tool_use" => "tool_calls",
                        other => other,
                    };
                    let chunk = json!({
                        "object": "chat.completion.chunk",
                        "choices": [{ "index": 0, "delta": {}, "finish_reason": mapped }]
                    });
                    frames.push(format!("data: {}\n\n", chunk));
                }
                frames.push("data: [DONE]\n\n".into());
                frames
            }
            CanonicalStreamEvent::Error { message } => {
                let chunk = json!({ "error": { "message": message } });
                vec![format!("data: {}\n\n", chunk)]
            }
        }
    }

    fn encode_error(&self, error_type: &str, message: &str) -> Value {
        json!({
            "error": {
                "message": message,
                "type": error_type,
                "param": null,
                "code": null
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn openai_streaming_enforces_include_usage_strict_schema_and_o3_max_completion_tokens() {
        let codec = OpenAiCodec;
        let input = json!({
            "model": "o3-mini",
            "max_tokens": 1024,
            "stream": true,
            "messages": [{ "role": "user", "content": "check status" }],
            "tools": [{
                "type": "function",
                "function": {
                    "name": "get_status",
                    "strict": true,
                    "parameters": { "type": "object", "properties": {}, "additionalProperties": false }
                }
            }]
        });

        let ir = codec.decode_request(&input, None, None).unwrap();
        assert!(ir.tools[0].strict);

        let encoded = codec.encode_request(&ir, "o3-mini", false).unwrap();
        assert_eq!(encoded["stream_options"]["include_usage"], json!(true));
        assert_eq!(encoded["tools"][0]["function"]["strict"], json!(true));
        assert_eq!(encoded["max_completion_tokens"], json!(1024));
        assert!(encoded.get("max_tokens").is_none());
    }
}
