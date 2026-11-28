use crate::codec::{CodecError, ProtocolCodec};
use crate::config::UpstreamProtocol;
use crate::ir::{
    CanonicalMessage, CanonicalRequest, CanonicalResponse, CanonicalStreamEvent, CanonicalUsage,
    ContentPart, OpaqueProviderState, ReasoningConfig, Role, StrictToolSchema,
};
use serde_json::{json, Map, Value};
use std::collections::BTreeMap;

pub struct OpenAiResponsesCodec;

fn parse_responses_usage(u: &Value) -> CanonicalUsage {
    let cached = u
        .get("input_tokens_details")
        .and_then(|d| d.get("cached_tokens"))
        .and_then(|c| c.as_u64())
        .unwrap_or(0);
    CanonicalUsage {
        input_tokens: u.get("input_tokens").and_then(|x| x.as_u64()).unwrap_or(0),
        output_tokens: u.get("output_tokens").and_then(|x| x.as_u64()).unwrap_or(0),
        cache_creation_input_tokens: 0,
        cache_read_input_tokens: cached,
    }
}

fn encode_responses_usage(u: &CanonicalUsage) -> Value {
    json!({
        "input_tokens": u.input_tokens,
        "output_tokens": u.output_tokens,
        "total_tokens": u.input_tokens + u.output_tokens,
        "input_tokens_details": {
            "cached_tokens": u.cache_read_input_tokens
        }
    })
}

fn extract_reasoning_summary(item: &Value) -> String {
    if let Some(summary) = item.get("summary") {
        if let Some(s) = summary.as_str() {
            return s.to_string();
        }
        if let Some(arr) = summary.as_array() {
            let joined = arr
                .iter()
                .filter_map(|entry| {
                    entry
                        .as_str()
                        .or_else(|| entry.get("text").and_then(|t| t.as_str()))
                })
                .collect::<Vec<_>>()
                .join("\n");
            if !joined.is_empty() {
                return joined;
            }
        }
    }
    if let Some(content) = item.get("content").and_then(|c| c.as_array()) {
        return content
            .iter()
            .filter_map(|entry| entry.get("text").and_then(|t| t.as_str()))
            .collect::<Vec<_>>()
            .join("\n");
    }
    String::new()
}

fn parse_arguments(val: Option<&Value>) -> Value {
    match val {
        Some(Value::String(s)) => serde_json::from_str(s).unwrap_or_else(|_| json!({})),
        Some(Value::Null) | None => json!({}),
        Some(other) => other.clone(),
    }
}

fn serialize_arguments(val: &Value) -> String {
    match val {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

fn decode_message_content_parts(content_val: Option<&Value>) -> Vec<ContentPart> {
    let mut parts = vec![];
    match content_val {
        Some(Value::String(s)) => {
            if !s.is_empty() {
                parts.push(ContentPart::Text {
                    text: s.clone(),
                    cache_control: None,
                });
            }
        }
        Some(Value::Array(arr)) => {
            for p in arr {
                let ptype = p.get("type").and_then(|t| t.as_str()).unwrap_or("");
                match ptype {
                    "input_text" | "output_text" | "text" => {
                        if let Some(text) = p.get("text").and_then(|t| t.as_str()) {
                            if !text.is_empty() {
                                parts.push(ContentPart::Text {
                                    text: text.to_string(),
                                    cache_control: None,
                                });
                            }
                        }
                    }
                    "" => {
                        if let Some(text) = p.get("text").and_then(|t| t.as_str()) {
                            if !text.is_empty() {
                                parts.push(ContentPart::Text {
                                    text: text.to_string(),
                                    cache_control: None,
                                });
                            }
                        } else {
                            parts.push(ContentPart::Opaque { raw: p.clone() });
                        }
                    }
                    _ => parts.push(ContentPart::Opaque { raw: p.clone() }),
                }
            }
        }
        _ => {}
    }
    parts
}

fn is_message_content_part(raw: &Value) -> bool {
    matches!(
        raw.get("type").and_then(|t| t.as_str()),
        Some(
            "input_text"
                | "output_text"
                | "text"
                | "input_image"
                | "input_file"
                | "input_audio"
                | "refusal"
        )
    )
}

fn derive_stop_reason(resp: Option<&Value>, has_tool_calls: bool, force_incomplete: bool) -> Option<String> {
    let status = resp.and_then(|r| r.get("status")).and_then(|s| s.as_str());
    if force_incomplete || status == Some("incomplete") {
        let reason = resp
            .and_then(|r| r.get("incomplete_details"))
            .and_then(|d| d.get("reason"))
            .and_then(|r| r.as_str());
        return Some(match reason {
            Some("max_output_tokens") | None => "max_tokens".to_string(),
            Some(other) => other.to_string(),
        });
    }
    match status {
        Some("completed") => Some(if has_tool_calls {
            "tool_use".to_string()
        } else {
            "end_turn".to_string()
        }),
        Some(other) => Some(other.to_string()),
        None => None,
    }
}

fn map_stop_reason_to_status(stop_reason: Option<&str>) -> (&'static str, Option<&'static str>) {
    match stop_reason {
        Some("max_tokens") | Some("length") | Some("max_output_tokens") => {
            ("incomplete", Some("max_output_tokens"))
        }
        Some("content_filter") => ("incomplete", Some("content_filter")),
        Some("failed") => ("failed", None),
        _ => ("completed", None),
    }
}

impl ProtocolCodec for OpenAiResponsesCodec {
    fn protocol(&self) -> UpstreamProtocol {
        UpstreamProtocol::OpenAiResponses
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

        let mut system = vec![];
        if let Some(instructions) = obj.get("instructions").and_then(|v| v.as_str()) {
            if !instructions.is_empty() {
                system.push(ContentPart::Text {
                    text: instructions.to_string(),
                    cache_control: None,
                });
            }
        }

        let mut messages = vec![];
        match obj.get("input") {
            Some(Value::String(text)) => {
                messages.push(CanonicalMessage {
                    role: Role::User,
                    parts: vec![ContentPart::Text {
                        text: text.clone(),
                        cache_control: None,
                    }],
                });
            }
            Some(Value::Array(items)) => {
                for item in items {
                    let item_type = item.get("type").and_then(|t| t.as_str());
                    match item_type {
                        Some("message") => {
                            let role_str = item.get("role").and_then(|r| r.as_str()).unwrap_or("user");
                            let parts = decode_message_content_parts(item.get("content"));
                            if role_str == "system" || role_str == "developer" {
                                system.extend(parts);
                            } else {
                                let role = match role_str {
                                    "assistant" => Role::Assistant,
                                    "tool" => Role::Tool,
                                    _ => Role::User,
                                };
                                messages.push(CanonicalMessage { role, parts });
                            }
                        }
                        None if item.get("role").is_some() => {
                            let role_str = item.get("role").and_then(|r| r.as_str()).unwrap_or("user");
                            let parts = decode_message_content_parts(item.get("content"));
                            if role_str == "system" || role_str == "developer" {
                                system.extend(parts);
                            } else {
                                let role = match role_str {
                                    "assistant" => Role::Assistant,
                                    "tool" => Role::Tool,
                                    _ => Role::User,
                                };
                                messages.push(CanonicalMessage { role, parts });
                            }
                        }
                        Some("function_call") => {
                            let id = item
                                .get("call_id")
                                .or_else(|| item.get("id"))
                                .and_then(|v| v.as_str())
                                .unwrap_or("")
                                .to_string();
                            let name = item
                                .get("name")
                                .and_then(|v| v.as_str())
                                .unwrap_or("")
                                .to_string();
                            let arguments = parse_arguments(item.get("arguments"));
                            messages.push(CanonicalMessage {
                                role: Role::Assistant,
                                parts: vec![ContentPart::ToolCall {
                                    id,
                                    name,
                                    arguments,
                                    cache_control: None,
                                }],
                            });
                        }
                        Some("function_call_output") => {
                            let tool_call_id = item
                                .get("call_id")
                                .or_else(|| item.get("id"))
                                .and_then(|v| v.as_str())
                                .unwrap_or("")
                                .to_string();
                            let content = item.get("output").cloned().unwrap_or(Value::Null);
                            let is_error = item
                                .get("is_error")
                                .and_then(|v| v.as_bool())
                                .unwrap_or(false);
                            messages.push(CanonicalMessage {
                                role: Role::Tool,
                                parts: vec![ContentPart::ToolResult {
                                    tool_call_id,
                                    content,
                                    is_error,
                                    cache_control: None,
                                }],
                            });
                        }
                        Some("reasoning") => {
                            let thinking = extract_reasoning_summary(item);
                            let signature = item
                                .get("encrypted_content")
                                .and_then(|v| v.as_str())
                                .map(String::from);
                            messages.push(CanonicalMessage {
                                role: Role::Assistant,
                                parts: vec![ContentPart::Thinking {
                                    thinking,
                                    signature,
                                }],
                            });
                        }
                        _ => {
                            messages.push(CanonicalMessage {
                                role: Role::User,
                                parts: vec![ContentPart::Opaque { raw: item.clone() }],
                            });
                        }
                    }
                }
            }
            _ => {
                return Err(CodecError::InvalidPayload(
                    "missing or invalid 'input' (expected string or array)".into(),
                ));
            }
        }

        let tools = obj
            .get("tools")
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|t| {
                        if t.get("type").and_then(|v| v.as_str()) != Some("function") {
                            return None;
                        }
                        let src = if t.get("name").is_some() {
                            t
                        } else {
                            t.get("function")?
                        };
                        let name = src.get("name")?.as_str()?.to_string();
                        let description = src
                            .get("description")
                            .and_then(|d| d.as_str())
                            .map(String::from);
                        let parameters = src
                            .get("parameters")
                            .cloned()
                            .unwrap_or_else(|| json!({ "type": "object" }));
                        let strict = src
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
            .get("reasoning")
            .and_then(|r| r.as_object())
            .and_then(|r| {
                let effort = r.get("effort").and_then(|e| e.as_str()).map(String::from);
                if effort.is_some() || !r.is_empty() {
                    Some(ReasoningConfig {
                        enabled: effort.as_deref() != Some("none"),
                        budget_tokens: None,
                        effort,
                    })
                } else {
                    None
                }
            });

        let derived_session = session_id
            .or_else(|| {
                obj.get("metadata")
                    .and_then(|m| {
                        m.get("user_id")
                            .or_else(|| m.get("session_id"))
                            .and_then(|v| v.as_str())
                    })
                    .map(String::from)
            })
            .or_else(|| {
                obj.get("session_id")
                    .or_else(|| obj.get("user"))
                    .and_then(|v| v.as_str())
                    .map(String::from)
            });

        let known_keys = [
            "model",
            "instructions",
            "input",
            "tools",
            "tool_choice",
            "max_output_tokens",
            "max_tokens",
            "temperature",
            "top_p",
            "stream",
            "reasoning",
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
                .get("max_output_tokens")
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

        if !req.system.is_empty() {
            let sys_text = req
                .system
                .iter()
                .filter_map(|p| match p {
                    ContentPart::Text { text, .. } => Some(text.as_str()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join("\n");
            if !sys_text.is_empty() {
                out.insert("instructions".into(), json!(sys_text));
            }
        }

        let mut input_items = vec![];
        for m in &req.messages {
            let role_str = match m.role {
                Role::Assistant => "assistant",
                Role::System => "system",
                _ => "user",
            };
            let text_part_type = if m.role == Role::Assistant {
                "output_text"
            } else {
                "input_text"
            };
            let mut msg_content: Vec<Value> = vec![];

            let flush_msg = |content: &mut Vec<Value>, items: &mut Vec<Value>| {
                if !content.is_empty() {
                    items.push(json!({
                        "type": "message",
                        "role": role_str,
                        "content": std::mem::take(content)
                    }));
                }
            };

            for p in &m.parts {
                match p {
                    ContentPart::Text { text, .. } => {
                        msg_content.push(json!({
                            "type": text_part_type,
                            "text": text
                        }));
                    }
                    ContentPart::Thinking {
                        thinking,
                        signature,
                    } => {
                        flush_msg(&mut msg_content, &mut input_items);
                        let mut item = Map::new();
                        item.insert("type".into(), json!("reasoning"));
                        let summary = if thinking.is_empty() {
                            vec![]
                        } else {
                            vec![json!({ "type": "summary_text", "text": thinking })]
                        };
                        item.insert("summary".into(), Value::Array(summary));
                        if let Some(sig) = signature {
                            item.insert("encrypted_content".into(), json!(sig));
                        }
                        input_items.push(Value::Object(item));
                    }
                    ContentPart::ToolCall {
                        id,
                        name,
                        arguments,
                        ..
                    } => {
                        flush_msg(&mut msg_content, &mut input_items);
                        input_items.push(json!({
                            "type": "function_call",
                            "call_id": id,
                            "name": name,
                            "arguments": serialize_arguments(arguments)
                        }));
                    }
                    ContentPart::ToolResult {
                        tool_call_id,
                        content,
                        ..
                    } => {
                        flush_msg(&mut msg_content, &mut input_items);
                        let output_str = match content {
                            Value::String(s) => s.clone(),
                            other => other.to_string(),
                        };
                        input_items.push(json!({
                            "type": "function_call_output",
                            "call_id": tool_call_id,
                            "output": output_str
                        }));
                    }
                    ContentPart::Opaque { raw } => {
                        if is_message_content_part(raw) {
                            msg_content.push(raw.clone());
                        } else {
                            flush_msg(&mut msg_content, &mut input_items);
                            input_items.push(raw.clone());
                        }
                    }
                }
            }
            flush_msg(&mut msg_content, &mut input_items);
        }
        out.insert("input".into(), Value::Array(input_items));

        if !req.tools.is_empty() {
            let tools: Vec<Value> = req
                .tools
                .iter()
                .map(|t| {
                    let mut tool_obj = Map::new();
                    tool_obj.insert("type".into(), json!("function"));
                    tool_obj.insert("name".into(), json!(t.name));
                    if let Some(ref desc) = t.description {
                        tool_obj.insert("description".into(), json!(desc));
                    }
                    tool_obj.insert("parameters".into(), t.parameters.clone());
                    if t.strict {
                        tool_obj.insert("strict".into(), json!(true));
                    }
                    Value::Object(tool_obj)
                })
                .collect();
            out.insert("tools".into(), Value::Array(tools));
        }

        if let Some(ref tc) = req.tool_choice {
            out.insert("tool_choice".into(), tc.clone());
        }
        if let Some(max_t) = req.max_tokens {
            out.insert("max_output_tokens".into(), json!(max_t));
        }
        if let Some(temp) = req.temperature {
            out.insert("temperature".into(), json!(temp));
        }
        if let Some(tp) = req.top_p {
            out.insert("top_p".into(), json!(tp));
        }
        if let Some(ref r) = req.reasoning {
            if let Some(ref effort) = r.effort {
                out.insert("reasoning".into(), json!({ "effort": effort }));
            } else if r.enabled {
                let mapped_effort = match r.budget_tokens {
                    Some(b) if b <= 2048 => "low",
                    Some(b) if b <= 8192 => "medium",
                    Some(_) => "high",
                    None => "medium",
                };
                out.insert("reasoning".into(), json!({ "effort": mapped_effort }));
            }
        }

        if same_protocol {
            for (k, v) in &req.extra_fields {
                out.entry(k.clone()).or_insert_with(|| v.clone());
            }
        }

        Ok(Value::Object(out))
    }

    fn decode_response(&self, body: &Value) -> Result<CanonicalResponse, CodecError> {
        let obj = body
            .as_object()
            .ok_or_else(|| CodecError::InvalidPayload("expected JSON object".into()))?;

        let id = obj
            .get("id")
            .and_then(|v| v.as_str())
            .unwrap_or("resp_unknown")
            .to_string();
        let model = obj
            .get("model")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();

        let mut parts = vec![];
        if let Some(output) = obj.get("output").and_then(|v| v.as_array()) {
            for item in output {
                match item.get("type").and_then(|t| t.as_str()).unwrap_or("") {
                    "reasoning" => {
                        let thinking = extract_reasoning_summary(item);
                        let signature = item
                            .get("encrypted_content")
                            .and_then(|v| v.as_str())
                            .map(String::from);
                        if !thinking.is_empty() || signature.is_some() {
                            parts.push(ContentPart::Thinking {
                                thinking,
                                signature,
                            });
                        }
                    }
                    "message" => {
                        parts.extend(decode_message_content_parts(item.get("content")));
                    }
                    "function_call" => {
                        let call_id = item
                            .get("call_id")
                            .or_else(|| item.get("id"))
                            .and_then(|s| s.as_str())
                            .unwrap_or("")
                            .to_string();
                        let name = item
                            .get("name")
                            .and_then(|s| s.as_str())
                            .unwrap_or("")
                            .to_string();
                        let arguments = parse_arguments(item.get("arguments"));
                        parts.push(ContentPart::ToolCall {
                            id: call_id,
                            name,
                            arguments,
                            cache_control: None,
                        });
                    }
                    _ => {
                        parts.push(ContentPart::Opaque { raw: item.clone() });
                    }
                }
            }
        } else if let Some(text) = obj.get("output_text").and_then(|v| v.as_str()) {
            if !text.is_empty() {
                parts.push(ContentPart::Text {
                    text: text.to_string(),
                    cache_control: None,
                });
            }
        }

        let has_tool_calls = parts
            .iter()
            .any(|p| matches!(p, ContentPart::ToolCall { .. }));
        let stop_reason = derive_stop_reason(Some(body), has_tool_calls, false);

        let usage = obj
            .get("usage")
            .filter(|u| !u.is_null())
            .map(parse_responses_usage)
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
        let mut output = vec![];
        let mut pending_text: Vec<Value> = vec![];

        let flush_text = |text_parts: &mut Vec<Value>, out: &mut Vec<Value>| {
            if !text_parts.is_empty() {
                out.push(json!({
                    "type": "message",
                    "role": "assistant",
                    "status": "completed",
                    "content": std::mem::take(text_parts)
                }));
            }
        };

        for p in &resp.parts {
            match p {
                ContentPart::Thinking {
                    thinking,
                    signature,
                } => {
                    flush_text(&mut pending_text, &mut output);
                    let mut item = Map::new();
                    item.insert("type".into(), json!("reasoning"));
                    let summary = if thinking.is_empty() {
                        vec![]
                    } else {
                        vec![json!({ "type": "summary_text", "text": thinking })]
                    };
                    item.insert("summary".into(), Value::Array(summary));
                    if let Some(sig) = signature {
                        item.insert("encrypted_content".into(), json!(sig));
                    }
                    output.push(Value::Object(item));
                }
                ContentPart::Text { text, .. } => {
                    pending_text.push(json!({
                        "type": "output_text",
                        "text": text
                    }));
                }
                ContentPart::ToolCall {
                    id,
                    name,
                    arguments,
                    ..
                } => {
                    flush_text(&mut pending_text, &mut output);
                    output.push(json!({
                        "type": "function_call",
                        "call_id": id,
                        "name": name,
                        "arguments": serialize_arguments(arguments)
                    }));
                }
                ContentPart::ToolResult {
                    tool_call_id,
                    content,
                    ..
                } => {
                    flush_text(&mut pending_text, &mut output);
                    let output_str = match content {
                        Value::String(s) => s.clone(),
                        other => other.to_string(),
                    };
                    output.push(json!({
                        "type": "function_call_output",
                        "call_id": tool_call_id,
                        "output": output_str
                    }));
                }
                ContentPart::Opaque { raw } => {
                    if is_message_content_part(raw) {
                        pending_text.push(raw.clone());
                    } else {
                        flush_text(&mut pending_text, &mut output);
                        output.push(raw.clone());
                    }
                }
            }
        }
        flush_text(&mut pending_text, &mut output);

        let (status, incomplete_reason) = map_stop_reason_to_status(resp.stop_reason.as_deref());
        let mut root = Map::new();
        root.insert("id".into(), json!(resp.id));
        root.insert("object".into(), json!("response"));
        root.insert("status".into(), json!(status));
        if let Some(reason) = incomplete_reason {
            root.insert("incomplete_details".into(), json!({ "reason": reason }));
        }
        root.insert("model".into(), json!(resp.model));
        root.insert("output".into(), Value::Array(output));
        root.insert("usage".into(), encode_responses_usage(&resp.usage));
        Value::Object(root)
    }

    fn decode_sse_frame(&self, event_name: Option<&str>, data: &str) -> Vec<CanonicalStreamEvent> {
        if data.trim() == "[DONE]" {
            return vec![CanonicalStreamEvent::MessageStop { stop_reason: None }];
        }
        let Ok(v) = serde_json::from_str::<Value>(data) else {
            return vec![];
        };
        let etype = event_name
            .or_else(|| v.get("type").and_then(|t| t.as_str()))
            .unwrap_or("");

        match etype {
            "response.created" => {
                let resp = v.get("response");
                let id = resp
                    .and_then(|r| r.get("id"))
                    .and_then(|s| s.as_str())
                    .unwrap_or("resp_unknown")
                    .to_string();
                let model = resp
                    .and_then(|r| r.get("model"))
                    .and_then(|s| s.as_str())
                    .unwrap_or("")
                    .to_string();
                let usage = resp
                    .and_then(|r| r.get("usage"))
                    .filter(|u| !u.is_null())
                    .map(parse_responses_usage);
                vec![CanonicalStreamEvent::MessageStart { id, model, usage }]
            }
            "response.output_text.delta" => {
                let index = v
                    .get("output_index")
                    .or_else(|| v.get("content_index"))
                    .and_then(|i| i.as_u64())
                    .unwrap_or(0) as usize;
                v.get("delta")
                    .and_then(|d| d.as_str())
                    .filter(|s| !s.is_empty())
                    .map(|text| {
                        vec![CanonicalStreamEvent::TextDelta {
                            index,
                            text: text.to_string(),
                        }]
                    })
                    .unwrap_or_default()
            }
            "response.reasoning_summary_text.delta" | "response.reasoning_text.delta" => {
                let index = v
                    .get("output_index")
                    .or_else(|| v.get("summary_index"))
                    .and_then(|i| i.as_u64())
                    .unwrap_or(0) as usize;
                v.get("delta")
                    .and_then(|d| d.as_str())
                    .filter(|s| !s.is_empty())
                    .map(|thinking| {
                        vec![CanonicalStreamEvent::ThinkingDelta {
                            index,
                            thinking: thinking.to_string(),
                        }]
                    })
                    .unwrap_or_default()
            }
            "response.output_item.added" => {
                let index = v
                    .get("output_index")
                    .and_then(|i| i.as_u64())
                    .unwrap_or(0) as usize;
                let Some(item) = v.get("item") else {
                    return vec![];
                };
                match item.get("type").and_then(|t| t.as_str()).unwrap_or("") {
                    "function_call" => {
                        let id = item
                            .get("call_id")
                            .or_else(|| item.get("id"))
                            .and_then(|s| s.as_str())
                            .unwrap_or("")
                            .to_string();
                        let name = item
                            .get("name")
                            .and_then(|s| s.as_str())
                            .unwrap_or("")
                            .to_string();
                        vec![CanonicalStreamEvent::ToolCallStart { index, id, name }]
                    }
                    "message" => vec![CanonicalStreamEvent::ContentBlockStart {
                        index,
                        block_type: "text".to_string(),
                    }],
                    "reasoning" => vec![CanonicalStreamEvent::ContentBlockStart {
                        index,
                        block_type: "thinking".to_string(),
                    }],
                    _ => vec![],
                }
            }
            "response.output_item.done" => {
                let index = v
                    .get("output_index")
                    .and_then(|i| i.as_u64())
                    .unwrap_or(0) as usize;
                let mut events = vec![];
                if let Some(sig) = v
                    .get("item")
                    .and_then(|i| i.get("encrypted_content"))
                    .and_then(|s| s.as_str())
                {
                    if !sig.is_empty() {
                        events.push(CanonicalStreamEvent::SignatureDelta {
                            index,
                            signature: sig.to_string(),
                        });
                    }
                }
                events.push(CanonicalStreamEvent::ContentBlockStop { index });
                events
            }
            "response.function_call_arguments.delta" => {
                let index = v
                    .get("output_index")
                    .and_then(|i| i.as_u64())
                    .unwrap_or(0) as usize;
                v.get("delta")
                    .and_then(|d| d.as_str())
                    .filter(|s| !s.is_empty())
                    .map(|args| {
                        vec![CanonicalStreamEvent::ToolCallDelta {
                            index,
                            arguments_delta: args.to_string(),
                        }]
                    })
                    .unwrap_or_default()
            }
            "response.completed" | "response.incomplete" => {
                let mut events = vec![];
                let resp = v.get("response");
                if let Some(u) = resp
                    .and_then(|r| r.get("usage"))
                    .filter(|u| !u.is_null())
                {
                    events.push(CanonicalStreamEvent::UsageDelta {
                        usage: parse_responses_usage(u),
                    });
                }
                let has_tool_calls = resp
                    .and_then(|r| r.get("output"))
                    .and_then(|o| o.as_array())
                    .map(|arr| {
                        arr.iter().any(|item| {
                            item.get("type").and_then(|t| t.as_str()) == Some("function_call")
                        })
                    })
                    .unwrap_or(false);
                let stop_reason = derive_stop_reason(
                    resp,
                    has_tool_calls,
                    etype == "response.incomplete",
                )
                .or_else(|| Some("end_turn".to_string()));
                events.push(CanonicalStreamEvent::MessageStop { stop_reason });
                events
            }
            "response.failed" => {
                let message = v
                    .get("response")
                    .and_then(|r| r.get("error"))
                    .and_then(|e| e.get("message"))
                    .and_then(|m| m.as_str())
                    .unwrap_or("upstream response failed")
                    .to_string();
                vec![CanonicalStreamEvent::Error { message }]
            }
            "error" => {
                let message = v
                    .get("message")
                    .or_else(|| v.get("error").and_then(|e| e.get("message")))
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
                let mut resp = Map::new();
                resp.insert("id".into(), json!(id));
                resp.insert("object".into(), json!("response"));
                resp.insert("status".into(), json!("in_progress"));
                resp.insert("model".into(), json!(model));
                resp.insert("output".into(), json!([]));
                if let Some(u) = usage {
                    resp.insert("usage".into(), encode_responses_usage(u));
                }
                let payload = json!({
                    "type": "response.created",
                    "response": Value::Object(resp)
                });
                vec![format!("event: response.created\ndata: {}\n\n", payload)]
            }
            CanonicalStreamEvent::ContentBlockStart { index, block_type } => {
                let item = if block_type == "thinking" {
                    json!({ "type": "reasoning", "summary": [] })
                } else {
                    json!({
                        "type": "message",
                        "role": "assistant",
                        "status": "in_progress",
                        "content": []
                    })
                };
                let payload = json!({
                    "type": "response.output_item.added",
                    "output_index": index,
                    "item": item
                });
                vec![format!(
                    "event: response.output_item.added\ndata: {}\n\n",
                    payload
                )]
            }
            CanonicalStreamEvent::ContentBlockStop { index } => {
                let payload = json!({
                    "type": "response.output_item.done",
                    "output_index": index,
                    "item": { "status": "completed" }
                });
                vec![format!(
                    "event: response.output_item.done\ndata: {}\n\n",
                    payload
                )]
            }
            CanonicalStreamEvent::TextDelta { index, text } => {
                let payload = json!({
                    "type": "response.output_text.delta",
                    "output_index": index,
                    "content_index": 0,
                    "delta": text
                });
                vec![format!(
                    "event: response.output_text.delta\ndata: {}\n\n",
                    payload
                )]
            }
            CanonicalStreamEvent::ThinkingDelta { index, thinking } => {
                let payload = json!({
                    "type": "response.reasoning_summary_text.delta",
                    "output_index": index,
                    "summary_index": 0,
                    "delta": thinking
                });
                vec![format!(
                    "event: response.reasoning_summary_text.delta\ndata: {}\n\n",
                    payload
                )]
            }
            CanonicalStreamEvent::SignatureDelta { .. } => vec![],
            CanonicalStreamEvent::ToolCallStart { index, id, name } => {
                let payload = json!({
                    "type": "response.output_item.added",
                    "output_index": index,
                    "item": {
                        "type": "function_call",
                        "id": id,
                        "call_id": id,
                        "name": name,
                        "arguments": "",
                        "status": "in_progress"
                    }
                });
                vec![format!(
                    "event: response.output_item.added\ndata: {}\n\n",
                    payload
                )]
            }
            CanonicalStreamEvent::ToolCallDelta {
                index,
                arguments_delta,
            } => {
                let payload = json!({
                    "type": "response.function_call_arguments.delta",
                    "output_index": index,
                    "delta": arguments_delta
                });
                vec![format!(
                    "event: response.function_call_arguments.delta\ndata: {}\n\n",
                    payload
                )]
            }
            CanonicalStreamEvent::UsageDelta { usage } => {
                let payload = json!({
                    "type": "response.completed",
                    "response": {
                        "object": "response",
                        "status": "completed",
                        "usage": encode_responses_usage(usage)
                    }
                });
                vec![format!(
                    "event: response.completed\ndata: {}\n\n",
                    payload
                )]
            }
            CanonicalStreamEvent::MessageStop { stop_reason } => {
                let (status, incomplete_reason) =
                    map_stop_reason_to_status(stop_reason.as_deref());
                let mut resp = Map::new();
                resp.insert("object".into(), json!("response"));
                resp.insert("status".into(), json!(status));
                if let Some(reason) = incomplete_reason {
                    resp.insert("incomplete_details".into(), json!({ "reason": reason }));
                }
                let payload = json!({
                    "type": "response.completed",
                    "response": Value::Object(resp)
                });
                vec![format!(
                    "event: response.completed\ndata: {}\n\n",
                    payload
                )]
            }
            CanonicalStreamEvent::Error { message } => {
                let payload = json!({
                    "type": "error",
                    "error": {
                        "type": "api_error",
                        "message": message,
                        "code": null
                    }
                });
                vec![format!("event: error\ndata: {}\n\n", payload)]
            }
        }
    }

    fn encode_error(&self, error_type: &str, message: &str) -> Value {
        json!({
            "error": {
                "type": error_type,
                "message": message,
                "code": null
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sse_fsm::SseFrameParser;

    #[test]
    fn request_and_response_roundtrip_preserves_instructions_usage_and_extra_fields() {
        let codec = OpenAiResponsesCodec;
        assert_eq!(codec.protocol(), UpstreamProtocol::OpenAiResponses);

        // Plain string input + instructions + metadata + same-protocol extra_fields
        let simple_req = json!({
            "model": "gpt-5.1",
            "instructions": "You are a concise Rust engineer.",
            "input": "Explain Send + Sync in one sentence.",
            "max_output_tokens": 512,
            "temperature": 0.25,
            "top_p": 0.9,
            "stream": false,
            "store": false,
            "metadata": { "user_id": "sess-user-42" }
        });

        let ir = codec.decode_request(&simple_req, None, Some("qa".into())).unwrap();
        assert_eq!(ir.model, "gpt-5.1");
        assert_eq!(ir.session_id.as_deref(), Some("sess-user-42"));
        assert_eq!(ir.task_stage.as_deref(), Some("qa"));
        assert_eq!(ir.max_tokens, Some(512));
        assert_eq!(ir.system.len(), 1);
        assert_eq!(ir.messages.len(), 1);
        assert_eq!(ir.messages[0].role, Role::User);
        assert!(ir.extra_fields.contains_key("store"));

        let same_proto = codec.encode_request(&ir, "gpt-5.1-codex", true).unwrap();
        assert_eq!(same_proto["model"], json!("gpt-5.1-codex"));
        assert_eq!(same_proto["instructions"], json!("You are a concise Rust engineer."));
        assert_eq!(same_proto["max_output_tokens"], json!(512));
        assert_eq!(same_proto["store"], json!(false));
        assert_eq!(same_proto["input"][0]["type"], json!("message"));
        assert_eq!(same_proto["input"][0]["role"], json!("user"));
        assert_eq!(
            same_proto["input"][0]["content"][0],
            json!({ "type": "input_text", "text": "Explain Send + Sync in one sentence." })
        );

        let cross_proto = codec.encode_request(&ir, "gpt-5.1-codex", false).unwrap();
        assert!(cross_proto.get("store").is_none());
        assert!(cross_proto.get("metadata").is_none());

        // Response decode -> encode -> decode roundtrip including reasoning, message, function_call, cached usage
        let raw_resp = json!({
            "id": "resp_abc123",
            "object": "response",
            "status": "completed",
            "model": "gpt-5.1",
            "output": [
                {
                    "type": "reasoning",
                    "summary": [{ "type": "summary_text", "text": "Checked thread safety bounds." }],
                    "encrypted_content": "enc_sig_001"
                },
                {
                    "type": "message",
                    "role": "assistant",
                    "status": "completed",
                    "content": [{ "type": "output_text", "text": "Send transfers ownership; Sync shares references." }]
                }
            ],
            "usage": {
                "input_tokens": 1200,
                "output_tokens": 64,
                "total_tokens": 1264,
                "input_tokens_details": { "cached_tokens": 900 }
            }
        });

        let decoded_resp = codec.decode_response(&raw_resp).unwrap();
        assert_eq!(decoded_resp.id, "resp_abc123");
        assert_eq!(decoded_resp.model, "gpt-5.1");
        assert_eq!(decoded_resp.stop_reason.as_deref(), Some("end_turn"));
        assert_eq!(decoded_resp.usage.input_tokens, 1200);
        assert_eq!(decoded_resp.usage.output_tokens, 64);
        assert_eq!(decoded_resp.usage.cache_read_input_tokens, 900);
        assert_eq!(decoded_resp.parts.len(), 2);
        assert_eq!(
            decoded_resp.parts[0],
            ContentPart::Thinking {
                thinking: "Checked thread safety bounds.".into(),
                signature: Some("enc_sig_001".into()),
            }
        );

        let reencoded = codec.encode_response(&decoded_resp);
        assert_eq!(reencoded["object"], json!("response"));
        assert_eq!(reencoded["status"], json!("completed"));
        assert_eq!(reencoded["usage"]["input_tokens"], json!(1200));
        assert_eq!(reencoded["usage"]["output_tokens"], json!(64));
        assert_eq!(reencoded["usage"]["input_tokens_details"]["cached_tokens"], json!(900));
        assert_eq!(reencoded["output"][0]["encrypted_content"], json!("enc_sig_001"));
        assert_eq!(
            reencoded["output"][1]["content"][0],
            json!({ "type": "output_text", "text": "Send transfers ownership; Sync shares references." })
        );

        let redecoded = codec.decode_response(&reencoded).unwrap();
        assert_eq!(redecoded, decoded_resp);

        let err_json = codec.encode_error("invalid_request_error", "bad input");
        assert_eq!(
            err_json,
            json!({ "error": { "type": "invalid_request_error", "message": "bad input", "code": null } })
        );
    }

    #[test]
    fn tool_calling_strict_schema_and_reasoning_effort_roundtrip() {
        let codec = OpenAiResponsesCodec;
        let payload = json!({
            "model": "o3",
            "stream": true,
            "max_output_tokens": 2048,
            "reasoning": { "effort": "high" },
            "metadata": { "session_id": "codex-sess-9" },
            "tool_choice": "auto",
            "tools": [{
                "type": "function",
                "name": "read_file",
                "description": "Read a UTF-8 file from disk",
                "strict": true,
                "parameters": {
                    "type": "object",
                    "properties": { "path": { "type": "string" } },
                    "required": ["path"],
                    "additionalProperties": false
                }
            }],
            "input": [
                {
                    "type": "message",
                    "role": "developer",
                    "content": [{ "type": "input_text", "text": "Always inspect before editing." }]
                },
                {
                    "type": "message",
                    "role": "user",
                    "content": [{ "type": "input_text", "text": "Read Cargo.toml" }]
                },
                {
                    "type": "reasoning",
                    "summary": [{ "type": "summary_text", "text": "Calling read_file on Cargo.toml" }],
                    "encrypted_content": "enc_r_42"
                },
                {
                    "type": "function_call",
                    "call_id": "call_1",
                    "name": "read_file",
                    "arguments": "{\"path\":\"Cargo.toml\"}"
                },
                {
                    "type": "function_call_output",
                    "call_id": "call_1",
                    "output": "[workspace]\nmembers = [\"crates/*\"]"
                }
            ]
        });

        let ir = codec.decode_request(&payload, None, None).unwrap();
        assert_eq!(ir.session_id.as_deref(), Some("codex-sess-9"));
        assert_eq!(ir.system.len(), 1);
        assert_eq!(ir.tools.len(), 1);
        assert_eq!(ir.tools[0].name, "read_file");
        assert!(ir.tools[0].strict);
        assert_eq!(
            ir.reasoning,
            Some(ReasoningConfig {
                enabled: true,
                budget_tokens: None,
                effort: Some("high".into()),
            })
        );
        assert_eq!(ir.messages.len(), 4);
        assert_eq!(ir.messages[0].role, Role::User);
        assert_eq!(ir.messages[1].role, Role::Assistant);
        assert!(matches!(ir.messages[1].parts[0], ContentPart::Thinking { .. }));
        assert_eq!(ir.messages[2].role, Role::Assistant);
        assert_eq!(
            ir.messages[2].parts[0],
            ContentPart::ToolCall {
                id: "call_1".into(),
                name: "read_file".into(),
                arguments: json!({ "path": "Cargo.toml" }),
                cache_control: None,
            }
        );
        assert_eq!(ir.messages[3].role, Role::Tool);
        assert!(matches!(ir.messages[3].parts[0], ContentPart::ToolResult { .. }));

        let encoded = codec.encode_request(&ir, "o3", true).unwrap();
        assert_eq!(encoded["reasoning"]["effort"], json!("high"));
        assert_eq!(encoded["tools"][0]["type"], json!("function"));
        assert_eq!(encoded["tools"][0]["name"], json!("read_file"));
        assert_eq!(encoded["tools"][0]["strict"], json!(true));
        assert_eq!(encoded["tool_choice"], json!("auto"));
        assert_eq!(encoded["instructions"], json!("Always inspect before editing."));
        assert_eq!(encoded["input"][1]["type"], json!("reasoning"));
        assert_eq!(encoded["input"][1]["encrypted_content"], json!("enc_r_42"));
        assert_eq!(encoded["input"][2]["type"], json!("function_call"));
        assert_eq!(encoded["input"][2]["call_id"], json!("call_1"));
        assert_eq!(encoded["input"][2]["arguments"], json!("{\"path\":\"Cargo.toml\"}"));
        assert_eq!(encoded["input"][3]["type"], json!("function_call_output"));
        assert_eq!(encoded["input"][3]["call_id"], json!("call_1"));

        // Function call response roundtrip
        let tc_resp = json!({
            "id": "resp_tc_1",
            "object": "response",
            "status": "completed",
            "model": "o3",
            "output": [{
                "type": "function_call",
                "call_id": "call_99",
                "name": "read_file",
                "arguments": "{\"path\":\"README.md\"}"
            }],
            "usage": {
                "input_tokens": 500,
                "output_tokens": 25,
                "input_tokens_details": { "cached_tokens": 200 }
            }
        });
        let ir_resp = codec.decode_response(&tc_resp).unwrap();
        assert_eq!(ir_resp.stop_reason.as_deref(), Some("tool_use"));
        let enc_resp = codec.encode_response(&ir_resp);
        assert_eq!(enc_resp["status"], json!("completed"));
        assert_eq!(enc_resp["output"][0]["type"], json!("function_call"));
        assert_eq!(enc_resp["output"][0]["call_id"], json!("call_99"));
        assert_eq!(enc_resp["output"][0]["arguments"], json!("{\"path\":\"README.md\"}"));
    }

    #[test]
    fn sse_streaming_decode_and_encode_with_split_cjk_utf8() {
        let codec = OpenAiResponsesCodec;
        let raw_stream = concat!(
            "event: response.created\n",
            "data: {\"type\":\"response.created\",\"response\":{\"id\":\"resp_stream_1\",\"model\":\"gpt-5.1\",\"status\":\"in_progress\"}}\n\n",
            "event: response.reasoning_summary_text.delta\n",
            "data: {\"type\":\"response.reasoning_summary_text.delta\",\"output_index\":0,\"delta\":\"先分析中文需求\"}\n\n",
            "event: response.output_text.delta\n",
            "data: {\"type\":\"response.output_text.delta\",\"output_index\":1,\"delta\":\"你好，世界！\"}\n\n",
            "event: response.output_item.added\n",
            "data: {\"type\":\"response.output_item.added\",\"output_index\":2,\"item\":{\"type\":\"function_call\",\"call_id\":\"call_cn_1\",\"name\":\"search_docs\"}}\n\n",
            "event: response.function_call_arguments.delta\n",
            "data: {\"type\":\"response.function_call_arguments.delta\",\"output_index\":2,\"delta\":\"{\\\"q\\\":\\\"边缘网关\\\"}\"}\n\n",
            "event: response.completed\n",
            "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_stream_1\",\"status\":\"completed\",\"output\":[{\"type\":\"function_call\",\"call_id\":\"call_cn_1\"}],\"usage\":{\"input_tokens\":800,\"output_tokens\":42,\"input_tokens_details\":{\"cached_tokens\":600}}}}\n\n",
            "event: error\n",
            "data: {\"type\":\"error\",\"error\":{\"type\":\"server_error\",\"message\":\"临时上游故障\"}}\n\n"
        );

        // Feed raw stream split right inside a 3-byte CJK UTF-8 sequence ("世")
        let bytes = raw_stream.as_bytes();
        let needle = "世".as_bytes();
        let pos = bytes.windows(needle.len()).position(|w| w == needle).unwrap();
        let split_at = pos + 1;

        let mut parser = SseFrameParser::new();
        let mut frames = parser.push_bytes(&bytes[..split_at]);
        frames.extend(parser.push_bytes(&bytes[split_at..]));
        frames.extend(parser.finish());
        assert_eq!(frames.len(), 7);

        let mut decoded_events = vec![];
        for frame in &frames {
            decoded_events.extend(codec.decode_sse_frame(frame.event.as_deref(), &frame.data));
        }

        assert_eq!(
            decoded_events,
            vec![
                CanonicalStreamEvent::MessageStart {
                    id: "resp_stream_1".into(),
                    model: "gpt-5.1".into(),
                    usage: None,
                },
                CanonicalStreamEvent::ThinkingDelta {
                    index: 0,
                    thinking: "先分析中文需求".into(),
                },
                CanonicalStreamEvent::TextDelta {
                    index: 1,
                    text: "你好，世界！".into(),
                },
                CanonicalStreamEvent::ToolCallStart {
                    index: 2,
                    id: "call_cn_1".into(),
                    name: "search_docs".into(),
                },
                CanonicalStreamEvent::ToolCallDelta {
                    index: 2,
                    arguments_delta: "{\"q\":\"边缘网关\"}".into(),
                },
                CanonicalStreamEvent::UsageDelta {
                    usage: CanonicalUsage {
                        input_tokens: 800,
                        output_tokens: 42,
                        cache_creation_input_tokens: 0,
                        cache_read_input_tokens: 600,
                    },
                },
                CanonicalStreamEvent::MessageStop {
                    stop_reason: Some("tool_use".into()),
                },
                CanonicalStreamEvent::Error {
                    message: "临时上游故障".into(),
                },
            ]
        );

        // Encode every CanonicalStreamEvent back into SSE frames and verify format + roundtrip
        let mut reencoded_bytes = Vec::new();
        for ev in &decoded_events {
            let encoded_frames = codec.encode_sse_event(ev);
            assert_eq!(encoded_frames.len(), 1);
            assert!(encoded_frames[0].starts_with("event: "));
            assert!(encoded_frames[0].contains("\ndata: "));
            assert!(encoded_frames[0].ends_with("\n\n"));
            assert!(!encoded_frames[0].contains('\u{FFFD}'));
            reencoded_bytes.extend_from_slice(encoded_frames[0].as_bytes());
        }

        let mut parser2 = SseFrameParser::new();
        let mut roundtrip_frames = parser2.push_bytes(&reencoded_bytes);
        roundtrip_frames.extend(parser2.finish());
        assert_eq!(roundtrip_frames[0].event.as_deref(), Some("response.created"));
        assert_eq!(
            roundtrip_frames[1].event.as_deref(),
            Some("response.reasoning_summary_text.delta")
        );
        assert_eq!(
            roundtrip_frames[2].event.as_deref(),
            Some("response.output_text.delta")
        );
        assert_eq!(
            roundtrip_frames[3].event.as_deref(),
            Some("response.output_item.added")
        );
        assert_eq!(
            roundtrip_frames[4].event.as_deref(),
            Some("response.function_call_arguments.delta")
        );
        assert_eq!(
            roundtrip_frames[5].event.as_deref(),
            Some("response.completed")
        );
        assert_eq!(
            roundtrip_frames[6].event.as_deref(),
            Some("response.completed")
        );
        assert_eq!(roundtrip_frames[7].event.as_deref(), Some("error"));

        let text_ev = codec.decode_sse_frame(
            roundtrip_frames[2].event.as_deref(),
            &roundtrip_frames[2].data,
        );
        assert_eq!(
            text_ev,
            vec![CanonicalStreamEvent::TextDelta {
                index: 1,
                text: "你好，世界！".into(),
            }]
        );
    }
}
