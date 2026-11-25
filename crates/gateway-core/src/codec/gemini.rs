use crate::codec::{CodecError, ProtocolCodec};
use crate::config::UpstreamProtocol;
use crate::ir::{
    CanonicalMessage, CanonicalRequest, CanonicalResponse, CanonicalStreamEvent, CanonicalUsage,
    ContentPart, OpaqueProviderState, ReasoningConfig, Role, StrictToolSchema,
};
use serde_json::{json, Map, Value};
use std::collections::{BTreeMap, HashMap};

pub struct GeminiCodec;

fn extract_thought_signature(part: &Value) -> Option<String> {
    part.get("thoughtSignature")
        .or_else(|| part.get("thought_signature"))
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(String::from)
}

fn map_gemini_finish_reason_to_canonical(reason: &str, has_tool_calls: bool) -> String {
    if has_tool_calls && reason == "STOP" {
        return "tool_use".to_string();
    }
    match reason {
        "STOP" => "end_turn".to_string(),
        "MAX_TOKENS" => "max_tokens".to_string(),
        "SAFETY" | "RECITATION" | "BLOCKLIST" | "PROHIBITED_CONTENT" => "refusal".to_string(),
        other => other.to_lowercase(),
    }
}

fn map_canonical_stop_reason_to_gemini(reason: Option<&str>) -> &'static str {
    match reason {
        Some("max_tokens") | Some("length") => "MAX_TOKENS",
        Some("refusal") | Some("content_filter") | Some("safety") => "SAFETY",
        _ => "STOP",
    }
}

fn parse_usage_metadata(usage: Option<&Value>) -> CanonicalUsage {
    let Some(u) = usage else {
        return CanonicalUsage::default();
    };
    let prompt_tokens = u
        .get("promptTokenCount")
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    let cached_tokens = u
        .get("cachedContentTokenCount")
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    let output_tokens = u
        .get("candidatesTokenCount")
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    CanonicalUsage {
        input_tokens: prompt_tokens.saturating_sub(cached_tokens),
        output_tokens,
        cache_creation_input_tokens: 0,
        cache_read_input_tokens: cached_tokens,
    }
}

fn encode_usage_metadata(usage: &CanonicalUsage) -> Value {
    let prompt_tokens = usage.input_tokens + usage.cache_read_input_tokens;
    let total_tokens = prompt_tokens + usage.output_tokens;
    let mut obj = Map::new();
    obj.insert("promptTokenCount".into(), json!(prompt_tokens));
    obj.insert("candidatesTokenCount".into(), json!(usage.output_tokens));
    if usage.cache_read_input_tokens > 0 {
        obj.insert(
            "cachedContentTokenCount".into(),
            json!(usage.cache_read_input_tokens),
        );
    }
    obj.insert("totalTokenCount".into(), json!(total_tokens));
    Value::Object(obj)
}

impl ProtocolCodec for GeminiCodec {
    fn protocol(&self) -> UpstreamProtocol {
        UpstreamProtocol::Gemini
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
            .filter(|s| !s.is_empty())
            .ok_or_else(|| {
                CodecError::InvalidPayload(
                    "missing 'model' (must be supplied via URL path or payload)".into(),
                )
            })?
            .to_string();

        let stream = obj.get("stream").and_then(|v| v.as_bool()).unwrap_or(false);

        let mut system = Vec::new();
        if let Some(sys_val) = obj
            .get("systemInstruction")
            .or_else(|| obj.get("system_instruction"))
        {
            if let Some(parts) = sys_val.get("parts").and_then(|v| v.as_array()) {
                for p in parts {
                    if let Some(text) = p.get("text").and_then(|v| v.as_str()) {
                        system.push(ContentPart::Text {
                            text: text.to_string(),
                            cache_control: None,
                        });
                    }
                }
            } else if let Some(s) = sys_val.as_str() {
                system.push(ContentPart::Text {
                    text: s.to_string(),
                    cache_control: None,
                });
            }
        }

        let mut opaque_state = OpaqueProviderState::default();
        let mut messages = Vec::new();
        if let Some(contents) = obj.get("contents").and_then(|v| v.as_array()) {
            for (msg_idx, item) in contents.iter().enumerate() {
                let raw_role = item.get("role").and_then(|v| v.as_str()).unwrap_or("user");
                let mut parts = Vec::new();
                let mut all_tool_results = true;
                let mut has_any_part = false;

                if let Some(raw_parts) = item.get("parts").and_then(|v| v.as_array()) {
                    for (part_idx, p) in raw_parts.iter().enumerate() {
                        has_any_part = true;
                        let part_sig = extract_thought_signature(p);
                        if let Some(ref sig) = part_sig {
                            opaque_state.thought_signature = Some(sig.clone());
                        }

                        if p.get("thought").and_then(|v| v.as_bool()).unwrap_or(false) {
                            all_tool_results = false;
                            let thinking = p
                                .get("text")
                                .and_then(|v| v.as_str())
                                .unwrap_or("")
                                .to_string();
                            parts.push(ContentPart::Thinking {
                                thinking,
                                signature: part_sig,
                            });
                        } else if let Some(fc) = p.get("functionCall") {
                            all_tool_results = false;
                            let name = fc
                                .get("name")
                                .and_then(|v| v.as_str())
                                .unwrap_or("unknown_tool")
                                .to_string();
                            let id = fc
                                .get("id")
                                .and_then(|v| v.as_str())
                                .map(String::from)
                                .unwrap_or_else(|| format!("call_{}_{}_{}", name, msg_idx, part_idx));
                            let arguments = fc.get("args").cloned().unwrap_or_else(|| json!({}));
                            parts.push(ContentPart::ToolCall {
                                id,
                                name,
                                arguments,
                                cache_control: None,
                            });
                        } else if let Some(fr) = p.get("functionResponse") {
                            let name = fr
                                .get("name")
                                .and_then(|v| v.as_str())
                                .unwrap_or("unknown_tool")
                                .to_string();
                            let tool_call_id = fr
                                .get("id")
                                .and_then(|v| v.as_str())
                                .map(String::from)
                                .unwrap_or(name);
                            let content = fr.get("response").cloned().unwrap_or_else(|| json!({}));
                            parts.push(ContentPart::ToolResult {
                                tool_call_id,
                                content,
                                is_error: false,
                                cache_control: None,
                            });
                        } else if let Some(text) = p.get("text").and_then(|v| v.as_str()) {
                            all_tool_results = false;
                            parts.push(ContentPart::Text {
                                text: text.to_string(),
                                cache_control: None,
                            });
                        } else {
                            all_tool_results = false;
                            parts.push(ContentPart::Opaque { raw: p.clone() });
                        }
                    }
                }

                let role = if has_any_part && all_tool_results {
                    Role::Tool
                } else {
                    match raw_role {
                        "model" | "assistant" => Role::Assistant,
                        "system" => Role::System,
                        _ => Role::User,
                    }
                };
                messages.push(CanonicalMessage { role, parts });
            }
        }

        let mut tools = Vec::new();
        if let Some(raw_tools) = obj.get("tools").and_then(|v| v.as_array()) {
            for t in raw_tools {
                let decls = t
                    .get("functionDeclarations")
                    .or_else(|| t.get("function_declarations"))
                    .and_then(|v| v.as_array());
                if let Some(decls) = decls {
                    for decl in decls {
                        if let Some(name) = decl.get("name").and_then(|v| v.as_str()) {
                            tools.push(StrictToolSchema {
                                name: name.to_string(),
                                description: decl
                                    .get("description")
                                    .and_then(|v| v.as_str())
                                    .map(String::from),
                                parameters: decl
                                    .get("parameters")
                                    .cloned()
                                    .unwrap_or_else(|| json!({"type": "object", "properties": {}})),
                                strict: false,
                                cache_control: None,
                            });
                        }
                    }
                }
            }
        }

        let tool_choice = obj
            .get("toolConfig")
            .or_else(|| obj.get("tool_config"))
            .cloned();

        let gen_cfg = obj
            .get("generationConfig")
            .or_else(|| obj.get("generation_config"));

        let max_tokens = gen_cfg
            .and_then(|g| g.get("maxOutputTokens").or_else(|| g.get("max_output_tokens")))
            .and_then(|v| v.as_u64())
            .map(|n| n as u32);
        let temperature = gen_cfg
            .and_then(|g| g.get("temperature"))
            .and_then(|v| v.as_f64())
            .map(|f| f as f32);
        let top_p = gen_cfg
            .and_then(|g| g.get("topP").or_else(|| g.get("top_p")))
            .and_then(|v| v.as_f64())
            .map(|f| f as f32);

        let reasoning = gen_cfg
            .and_then(|g| g.get("thinkingConfig").or_else(|| g.get("thinking_config")))
            .map(|tc| ReasoningConfig {
                enabled: tc
                    .get("includeThoughts")
                    .or_else(|| tc.get("include_thoughts"))
                    .and_then(|v| v.as_bool())
                    .unwrap_or(true),
                budget_tokens: tc
                    .get("thinkingBudget")
                    .or_else(|| tc.get("thinking_budget"))
                    .and_then(|v| v.as_u64())
                    .map(|n| n as u32),
                effort: None,
            });

        let known = [
            "model",
            "stream",
            "systemInstruction",
            "system_instruction",
            "contents",
            "tools",
            "toolConfig",
            "tool_config",
            "generationConfig",
            "generation_config",
        ];
        let mut extra_fields = BTreeMap::new();
        for (k, v) in obj {
            if !known.contains(&k.as_str()) {
                extra_fields.insert(k.clone(), v.clone());
            }
        }

        Ok(CanonicalRequest {
            model,
            system,
            messages,
            tools,
            tool_choice,
            max_tokens,
            temperature,
            top_p,
            stream,
            reasoning,
            opaque_state,
            session_id,
            task_stage,
            extra_fields,
        })
    }

    fn encode_request(
        &self,
        req: &CanonicalRequest,
        _upstream_model: &str,
        same_protocol: bool,
    ) -> Result<Value, CodecError> {
        let mut out = Map::new();

        if !req.system.is_empty() {
            let sys_parts: Vec<Value> = req
                .system
                .iter()
                .filter_map(|p| match p {
                    ContentPart::Text { text, .. } => Some(json!({ "text": text })),
                    _ => None,
                })
                .collect();
            if !sys_parts.is_empty() {
                out.insert("systemInstruction".into(), json!({ "parts": sys_parts }));
            }
        }

        // Track tool_call_id -> tool_name so functionResponse always carries the function name.
        let mut call_id_to_name: HashMap<String, String> = HashMap::new();
        for msg in &req.messages {
            for part in &msg.parts {
                if let ContentPart::ToolCall { id, name, .. } = part {
                    call_id_to_name.insert(id.clone(), name.clone());
                }
            }
        }

        // Find the last assistant message index to attach fallback opaque thought_signature if
        // the client stripped Thinking blocks altogether.
        let last_assistant_idx = req
            .messages
            .iter()
            .rposition(|m| m.role == Role::Assistant);

        let mut contents = Vec::new();
        for (msg_idx, msg) in req.messages.iter().enumerate() {
            let role = match msg.role {
                Role::Assistant => "model",
                Role::User | Role::Tool | Role::System => "user",
            };

            let mut parts = Vec::new();
            let mut attached_sig = false;

            for part in &msg.parts {
                match part {
                    ContentPart::Thinking {
                        thinking,
                        signature,
                    } => {
                        let mut p = Map::new();
                        p.insert("thought".into(), json!(true));
                        p.insert("text".into(), json!(thinking));
                        if let Some(sig) = signature
                            .as_ref()
                            .or(req.opaque_state.thought_signature.as_ref())
                        {
                            p.insert("thoughtSignature".into(), json!(sig));
                            attached_sig = true;
                        }
                        parts.push(Value::Object(p));
                    }
                    ContentPart::Text { text, .. } => {
                        let mut p = Map::new();
                        p.insert("text".into(), json!(text));
                        if Some(msg_idx) == last_assistant_idx && !attached_sig {
                            if let Some(ref sig) = req.opaque_state.thought_signature {
                                p.insert("thoughtSignature".into(), json!(sig));
                                attached_sig = true;
                            }
                        }
                        parts.push(Value::Object(p));
                    }
                    ContentPart::ToolCall {
                        name, arguments, ..
                    } => {
                        let mut p = Map::new();
                        p.insert(
                            "functionCall".into(),
                            json!({
                                "name": name,
                                "args": arguments,
                            }),
                        );
                        if Some(msg_idx) == last_assistant_idx && !attached_sig {
                            if let Some(ref sig) = req.opaque_state.thought_signature {
                                p.insert("thoughtSignature".into(), json!(sig));
                                attached_sig = true;
                            }
                        }
                        parts.push(Value::Object(p));
                    }
                    ContentPart::ToolResult {
                        tool_call_id,
                        content,
                        ..
                    } => {
                        let name = call_id_to_name
                            .get(tool_call_id)
                            .cloned()
                            .unwrap_or_else(|| tool_call_id.clone());
                        let response_obj = if content.is_object() {
                            content.clone()
                        } else {
                            json!({ "result": content })
                        };
                        parts.push(json!({
                            "functionResponse": {
                                "name": name,
                                "response": response_obj,
                            }
                        }));
                    }
                    ContentPart::Opaque { raw } => {
                        parts.push(raw.clone());
                    }
                }
            }

            if !parts.is_empty() {
                contents.push(json!({
                    "role": role,
                    "parts": parts,
                }));
            }
        }
        out.insert("contents".into(), Value::Array(contents));

        if !req.tools.is_empty() {
            let decls: Vec<Value> = req
                .tools
                .iter()
                .map(|t| {
                    let mut d = Map::new();
                    d.insert("name".into(), json!(t.name));
                    if let Some(ref desc) = t.description {
                        d.insert("description".into(), json!(desc));
                    }
                    d.insert("parameters".into(), t.parameters.clone());
                    Value::Object(d)
                })
                .collect();
            out.insert(
                "tools".into(),
                json!([{ "functionDeclarations": decls }]),
            );
        }

        if let Some(ref tc) = req.tool_choice {
            if same_protocol {
                out.insert("toolConfig".into(), tc.clone());
            }
        }

        let mut gen_cfg = Map::new();
        if let Some(max) = req.max_tokens {
            gen_cfg.insert("maxOutputTokens".into(), json!(max));
        }
        if let Some(temp) = req.temperature {
            gen_cfg.insert("temperature".into(), json!(temp));
        }
        if let Some(tp) = req.top_p {
            gen_cfg.insert("topP".into(), json!(tp));
        }
        if let Some(ref reasoning) = req.reasoning {
            if reasoning.enabled {
                let mut tc = Map::new();
                tc.insert("includeThoughts".into(), json!(true));
                if let Some(budget) = reasoning.budget_tokens {
                    tc.insert("thinkingBudget".into(), json!(budget));
                }
                gen_cfg.insert("thinkingConfig".into(), Value::Object(tc));
            }
        }
        if !gen_cfg.is_empty() {
            out.insert("generationConfig".into(), Value::Object(gen_cfg));
        }

        if same_protocol {
            for (k, v) in &req.extra_fields {
                out.entry(k.clone()).or_insert_with(|| v.clone());
            }
        }

        Ok(Value::Object(out))
    }

    fn decode_response(&self, body: &Value) -> Result<CanonicalResponse, CodecError> {
        let id = body
            .get("responseId")
            .and_then(|v| v.as_str())
            .unwrap_or("resp_gemini")
            .to_string();
        let model = body
            .get("modelVersion")
            .and_then(|v| v.as_str())
            .unwrap_or("gemini")
            .to_string();

        let mut parts = Vec::new();
        let mut has_tool_calls = false;
        let mut raw_finish_reason: Option<String> = None;

        if let Some(candidate) = body
            .get("candidates")
            .and_then(|v| v.as_array())
            .and_then(|a| a.first())
        {
            raw_finish_reason = candidate
                .get("finishReason")
                .and_then(|v| v.as_str())
                .map(String::from);

            if let Some(raw_parts) = candidate
                .get("content")
                .and_then(|c| c.get("parts"))
                .and_then(|p| p.as_array())
            {
                let mut has_thinking_with_sig = false;
                let mut trailing_sig: Option<String> = None;

                for (idx, p) in raw_parts.iter().enumerate() {
                    let sig = extract_thought_signature(p);
                    if p.get("thought").and_then(|v| v.as_bool()).unwrap_or(false) {
                        let thinking = p
                            .get("text")
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .to_string();
                        if sig.is_some() {
                            has_thinking_with_sig = true;
                        }
                        parts.push(ContentPart::Thinking {
                            thinking,
                            signature: sig,
                        });
                    } else if let Some(fc) = p.get("functionCall") {
                        has_tool_calls = true;
                        if let Some(s) = sig {
                            trailing_sig = Some(s);
                        }
                        let name = fc
                            .get("name")
                            .and_then(|v| v.as_str())
                            .unwrap_or("unknown_tool")
                            .to_string();
                        let call_id = fc
                            .get("id")
                            .and_then(|v| v.as_str())
                            .map(String::from)
                            .unwrap_or_else(|| format!("call_{}_{}", name, idx));
                        let arguments = fc.get("args").cloned().unwrap_or_else(|| json!({}));
                        parts.push(ContentPart::ToolCall {
                            id: call_id,
                            name,
                            arguments,
                            cache_control: None,
                        });
                    } else if let Some(text) = p.get("text").and_then(|v| v.as_str()) {
                        if let Some(s) = sig {
                            trailing_sig = Some(s);
                        }
                        parts.push(ContentPart::Text {
                            text: text.to_string(),
                            cache_control: None,
                        });
                    } else {
                        parts.push(ContentPart::Opaque { raw: p.clone() });
                    }
                }

                // If Gemini attached `thoughtSignature` directly onto a text/functionCall part
                // without a separate thought part, preserve it as a Thinking signature block so
                // GatewayEngine's sidecar SignatureCache persists it across turns.
                if !has_thinking_with_sig {
                    if let Some(sig) = trailing_sig {
                        parts.insert(
                            0,
                            ContentPart::Thinking {
                                thinking: String::new(),
                                signature: Some(sig),
                            },
                        );
                    }
                }
            }
        }

        let stop_reason = raw_finish_reason
            .as_deref()
            .map(|r| map_gemini_finish_reason_to_canonical(r, has_tool_calls));
        let usage = parse_usage_metadata(body.get("usageMetadata"));

        Ok(CanonicalResponse {
            id,
            model,
            parts,
            stop_reason,
            usage,
        })
    }

    fn encode_response(&self, resp: &CanonicalResponse) -> Value {
        let mut parts = Vec::new();
        for part in &resp.parts {
            match part {
                ContentPart::Thinking {
                    thinking,
                    signature,
                } => {
                    let mut p = Map::new();
                    p.insert("thought".into(), json!(true));
                    p.insert("text".into(), json!(thinking));
                    if let Some(sig) = signature {
                        p.insert("thoughtSignature".into(), json!(sig));
                    }
                    parts.push(Value::Object(p));
                }
                ContentPart::Text { text, .. } => {
                    parts.push(json!({ "text": text }));
                }
                ContentPart::ToolCall {
                    name, arguments, ..
                } => {
                    parts.push(json!({
                        "functionCall": {
                            "name": name,
                            "args": arguments,
                        }
                    }));
                }
                ContentPart::Opaque { raw } => parts.push(raw.clone()),
                ContentPart::ToolResult { .. } => {}
            }
        }

        json!({
            "responseId": resp.id,
            "modelVersion": resp.model,
            "candidates": [{
                "index": 0,
                "content": {
                    "role": "model",
                    "parts": parts,
                },
                "finishReason": map_canonical_stop_reason_to_gemini(resp.stop_reason.as_deref()),
            }],
            "usageMetadata": encode_usage_metadata(&resp.usage),
        })
    }

    fn decode_sse_frame(&self, _event_name: Option<&str>, data: &str) -> Vec<CanonicalStreamEvent> {
        let trimmed = data.trim();
        if trimmed.is_empty() || trimmed == "[DONE]" {
            return vec![];
        }
        let Ok(val) = serde_json::from_str::<Value>(trimmed) else {
            return vec![];
        };

        let mut events = Vec::new();
        let mut has_tool_calls = false;

        if let Some(candidate) = val
            .get("candidates")
            .and_then(|v| v.as_array())
            .and_then(|a| a.first())
        {
            if let Some(raw_parts) = candidate
                .get("content")
                .and_then(|c| c.get("parts"))
                .and_then(|p| p.as_array())
            {
                for (idx, p) in raw_parts.iter().enumerate() {
                    let sig = extract_thought_signature(p);
                    if p.get("thought").and_then(|v| v.as_bool()).unwrap_or(false) {
                        if let Some(text) = p.get("text").and_then(|v| v.as_str()) {
                            if !text.is_empty() {
                                events.push(CanonicalStreamEvent::ThinkingDelta {
                                    index: idx,
                                    thinking: text.to_string(),
                                });
                            }
                        }
                        if let Some(signature) = sig {
                            events.push(CanonicalStreamEvent::SignatureDelta {
                                index: idx,
                                signature,
                            });
                        }
                    } else if let Some(fc) = p.get("functionCall") {
                        has_tool_calls = true;
                        if let Some(signature) = sig {
                            events.push(CanonicalStreamEvent::SignatureDelta {
                                index: 0,
                                signature,
                            });
                        }
                        let name = fc
                            .get("name")
                            .and_then(|v| v.as_str())
                            .unwrap_or("unknown_tool")
                            .to_string();
                        let id = fc
                            .get("id")
                            .and_then(|v| v.as_str())
                            .map(String::from)
                            .unwrap_or_else(|| format!("call_{}_{}", name, idx));
                        let args_str = serde_json::to_string(
                            fc.get("args").unwrap_or(&json!({})),
                        )
                        .unwrap_or_else(|_| "{}".into());
                        events.push(CanonicalStreamEvent::ToolCallStart {
                            index: idx,
                            id,
                            name,
                        });
                        events.push(CanonicalStreamEvent::ToolCallDelta {
                            index: idx,
                            arguments_delta: args_str,
                        });
                    } else if let Some(text) = p.get("text").and_then(|v| v.as_str()) {
                        if let Some(signature) = sig {
                            events.push(CanonicalStreamEvent::SignatureDelta {
                                index: 0,
                                signature,
                            });
                        }
                        if !text.is_empty() {
                            events.push(CanonicalStreamEvent::TextDelta {
                                index: idx,
                                text: text.to_string(),
                            });
                        }
                    }
                }
            }

            if let Some(usage_val) = val.get("usageMetadata") {
                events.push(CanonicalStreamEvent::UsageDelta {
                    usage: parse_usage_metadata(Some(usage_val)),
                });
            }

            if let Some(reason) = candidate.get("finishReason").and_then(|v| v.as_str()) {
                events.push(CanonicalStreamEvent::MessageStop {
                    stop_reason: Some(map_gemini_finish_reason_to_canonical(
                        reason,
                        has_tool_calls,
                    )),
                });
            }
        } else if let Some(usage_val) = val.get("usageMetadata") {
            events.push(CanonicalStreamEvent::UsageDelta {
                usage: parse_usage_metadata(Some(usage_val)),
            });
        } else if let Some(err_msg) = val
            .get("error")
            .and_then(|e| e.get("message"))
            .and_then(|m| m.as_str())
        {
            events.push(CanonicalStreamEvent::Error {
                message: err_msg.to_string(),
            });
        }

        events
    }

    fn encode_sse_event(&self, event: &CanonicalStreamEvent) -> Vec<String> {
        match event {
            CanonicalStreamEvent::MessageStart { .. }
            | CanonicalStreamEvent::ContentBlockStart { .. }
            | CanonicalStreamEvent::ContentBlockStop { .. } => vec![],
            CanonicalStreamEvent::TextDelta { text, .. } => {
                let chunk = json!({
                    "candidates": [{
                        "index": 0,
                        "content": {
                            "role": "model",
                            "parts": [{ "text": text }]
                        }
                    }]
                });
                vec![format!("data: {}\n\n", chunk)]
            }
            CanonicalStreamEvent::ThinkingDelta { thinking, .. } => {
                let chunk = json!({
                    "candidates": [{
                        "index": 0,
                        "content": {
                            "role": "model",
                            "parts": [{ "thought": true, "text": thinking }]
                        }
                    }]
                });
                vec![format!("data: {}\n\n", chunk)]
            }
            CanonicalStreamEvent::SignatureDelta { signature, .. } => {
                let chunk = json!({
                    "candidates": [{
                        "index": 0,
                        "content": {
                            "role": "model",
                            "parts": [{ "thought": true, "text": "", "thoughtSignature": signature }]
                        }
                    }]
                });
                vec![format!("data: {}\n\n", chunk)]
            }
            CanonicalStreamEvent::ToolCallStart { name, .. } => {
                let chunk = json!({
                    "candidates": [{
                        "index": 0,
                        "content": {
                            "role": "model",
                            "parts": [{ "functionCall": { "name": name, "args": {} } }]
                        }
                    }]
                });
                vec![format!("data: {}\n\n", chunk)]
            }
            CanonicalStreamEvent::ToolCallDelta {
                arguments_delta, ..
            } => {
                let parsed_args =
                    serde_json::from_str::<Value>(arguments_delta).unwrap_or_else(|_| json!({ "raw": arguments_delta }));
                let chunk = json!({
                    "candidates": [{
                        "index": 0,
                        "content": {
                            "role": "model",
                            "parts": [{ "functionCall": { "name": "", "args": parsed_args } }]
                        }
                    }]
                });
                vec![format!("data: {}\n\n", chunk)]
            }
            CanonicalStreamEvent::UsageDelta { usage } => {
                let chunk = json!({
                    "usageMetadata": encode_usage_metadata(usage)
                });
                vec![format!("data: {}\n\n", chunk)]
            }
            CanonicalStreamEvent::MessageStop { stop_reason } => {
                let chunk = json!({
                    "candidates": [{
                        "index": 0,
                        "finishReason": map_canonical_stop_reason_to_gemini(stop_reason.as_deref())
                    }]
                });
                vec![format!("data: {}\n\n", chunk)]
            }
            CanonicalStreamEvent::Error { message } => {
                let chunk = self.encode_error("api_error", message);
                vec![format!("data: {}\n\n", chunk)]
            }
        }
    }

    fn encode_error(&self, error_type: &str, message: &str) -> Value {
        let (code, status) = match error_type {
            "invalid_request_error" => (400, "INVALID_ARGUMENT"),
            "authentication_error" => (401, "UNAUTHENTICATED"),
            "rate_limit_error" => (429, "RESOURCE_EXHAUSTED"),
            "overloaded_error" => (503, "UNAVAILABLE"),
            _ => (500, "INTERNAL"),
        };
        json!({
            "error": {
                "code": code,
                "message": message,
                "status": status
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gemini_roundtrip_preserves_thought_signature_tools_and_sse() {
        let codec = GeminiCodec;
        let inbound = json!({
            "model": "gemini-2.5-pro",
            "stream": true,
            "systemInstruction": {
                "parts": [{ "text": "You are LoomGate." }]
            },
            "contents": [
                {
                    "role": "user",
                    "parts": [{ "text": "Inspect Cargo.toml" }]
                },
                {
                    "role": "model",
                    "parts": [
                        { "thought": true, "text": "Need to read Cargo.toml", "thoughtSignature": "sig_gem_001" },
                        { "functionCall": { "name": "read_file", "args": { "path": "Cargo.toml" } } }
                    ]
                },
                {
                    "role": "user",
                    "parts": [
                        { "functionResponse": { "name": "read_file", "response": { "content": "[workspace]" } } }
                    ]
                }
            ],
            "tools": [{
                "functionDeclarations": [{
                    "name": "read_file",
                    "description": "Read a file",
                    "parameters": {
                        "type": "object",
                        "properties": { "path": { "type": "string" } },
                        "required": ["path"]
                    }
                }]
            }],
            "generationConfig": {
                "maxOutputTokens": 2048,
                "thinkingConfig": { "includeThoughts": true, "thinkingBudget": 4096 }
            }
        });

        let ir = codec
            .decode_request(&inbound, Some("sess-gem".into()), None)
            .expect("decode gemini request");
        assert_eq!(ir.model, "gemini-2.5-pro");
        assert!(ir.stream);
        assert_eq!(ir.opaque_state.thought_signature.as_deref(), Some("sig_gem_001"));
        assert_eq!(ir.tools.len(), 1);
        assert_eq!(ir.tools[0].name, "read_file");

        let encoded = codec
            .encode_request(&ir, "gemini-2.5-pro", true)
            .expect("encode gemini request");
        assert!(encoded.get("model").is_none());
        assert!(encoded.get("stream").is_none());
        assert_eq!(
            encoded["contents"][1]["parts"][0]["thoughtSignature"],
            "sig_gem_001"
        );
        assert_eq!(
            encoded["contents"][1]["parts"][1]["functionCall"]["name"],
            "read_file"
        );
        assert_eq!(
            encoded["contents"][2]["parts"][0]["functionResponse"]["name"],
            "read_file"
        );

        let sse_frame = json!({
            "candidates": [{
                "index": 0,
                "content": {
                    "role": "model",
                    "parts": [
                        { "thought": true, "text": "Analyzing", "thoughtSignature": "sig_gem_002" },
                        { "text": "完成分析" }
                    ]
                },
                "finishReason": "STOP"
            }],
            "usageMetadata": {
                "promptTokenCount": 500,
                "cachedContentTokenCount": 300,
                "candidatesTokenCount": 40
            }
        });
        let events = codec.decode_sse_frame(None, &sse_frame.to_string());
        assert!(events.iter().any(|e| matches!(
            e,
            CanonicalStreamEvent::SignatureDelta { signature, .. } if signature == "sig_gem_002"
        )));
        assert!(events.iter().any(|e| matches!(
            e,
            CanonicalStreamEvent::TextDelta { text, .. } if text == "完成分析"
        )));
        assert!(events.iter().any(|e| matches!(
            e,
            CanonicalStreamEvent::UsageDelta { usage } if usage.cache_read_input_tokens == 300 && usage.input_tokens == 200
        )));

        let err = codec.encode_error("invalid_request_error", "bad schema");
        assert_eq!(err["error"]["code"], 400);
        assert_eq!(err["error"]["status"], "INVALID_ARGUMENT");
    }
}
