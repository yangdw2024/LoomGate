use crate::ir::{CanonicalStreamEvent, ContentPart};
use crate::middleware::{BoxFuture, Middleware, MiddlewareContext, MiddlewareRejection, NextFn};
use std::collections::HashMap;

/// Maximum accumulated argument bytes retained per in-flight tool call during SSE inspection.
const MAX_STREAM_ARGS_BYTES: usize = 64 * 1024;

/// Guardrail middleware enforcing outbound privacy & safety:
/// 1. Pre-leg: Redacts secrets (API keys: `sk-*`, `AIza*`, `ghp_*`, `AKIA*`) and PII (emails,
///    CN mobile numbers) in outbound text blocks without disturbing `cache_control` or `Thinking`.
/// 2. Post-leg: Intercepts high-risk tool calls returned by upstream models before they reach
///    local coding agents (blocking listed tool names or dangerous argument substrings).
#[derive(Debug, Clone, Default)]
pub struct GuardrailMiddleware {
    pub redact_secrets: bool,
    pub redact_pii: bool,
    pub blocked_tools: Vec<String>,
    pub blocked_arg_patterns: Vec<String>,
}

fn is_token_char(b: u8) -> bool {
    b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.')
}

fn redact_prefixed_tokens(input: &str, prefixes: &[&str], placeholder: &str) -> (String, usize) {
    let mut out = String::with_capacity(input.len());
    let bytes = input.as_bytes();
    let mut i = 0;
    let mut count = 0;

    while i < bytes.len() {
        let mut matched = false;
        for prefix in prefixes {
            let pbytes = prefix.as_bytes();
            let boundary_ok = i == 0 || !is_token_char(bytes[i - 1]);
            if boundary_ok && bytes[i..].starts_with(pbytes) {
                let mut end = i + pbytes.len();
                while end < bytes.len() && is_token_char(bytes[end]) {
                    end += 1;
                }
                if end - i >= pbytes.len() + 10 {
                    out.push_str(placeholder);
                    i = end;
                    count += 1;
                    matched = true;
                    break;
                }
            }
        }
        if !matched {
            let ch = input[i..].chars().next().unwrap();
            out.push(ch);
            i += ch.len_utf8();
        }
    }
    (out, count)
}

fn redact_emails_and_phones(input: &str) -> (String, usize) {
    let mut count = 0usize;
    let mut out = String::with_capacity(input.len());

    for token in input.split_inclusive(|c: char| c.is_whitespace() || matches!(c, ',' | ';' | '"' | '\'' | '(' | ')' | '<' | '>')) {
        let trimmed = token.trim_end_matches(|c: char| {
            c.is_whitespace() || matches!(c, ',' | ';' | '"' | '\'' | '(' | ')' | '<' | '>')
        });
        let suffix = &token[trimmed.len()..];

        // Email heuristic: local@domain.tld
        let is_email = if let Some((local, domain)) = trimmed.split_once('@') {
            !local.is_empty()
                && !domain.is_empty()
                && domain.contains('.')
                && !domain.starts_with('.')
                && !domain.ends_with('.')
                && local
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-' | b'+'))
        } else {
            false
        };

        // CN 11-digit mobile number heuristic: 1[3-9]xxxxxxxxx
        let is_cn_phone = trimmed.len() == 11
            && trimmed.starts_with('1')
            && trimmed.as_bytes()[1].is_ascii_digit()
            && trimmed.as_bytes()[1] >= b'3'
            && trimmed.bytes().all(|b| b.is_ascii_digit());

        if is_email {
            out.push_str("[REDACTED_EMAIL]");
            out.push_str(suffix);
            count += 1;
        } else if is_cn_phone {
            out.push_str("[REDACTED_PHONE]");
            out.push_str(suffix);
            count += 1;
        } else {
            out.push_str(token);
        }
    }

    (out, count)
}

fn redact_bearer_tokens(input: &str, placeholder: &str) -> (String, usize) {
    let mut out = String::with_capacity(input.len());
    let bytes = input.as_bytes();
    let mut i = 0;
    let mut count = 0;

    while i < bytes.len() {
        let boundary_ok = i == 0 || !is_token_char(bytes[i - 1]);
        if boundary_ok && bytes.len().saturating_sub(i) >= 7
            && bytes[i..i + 7].eq_ignore_ascii_case(b"Bearer ")
        {
            let mut tok_start = i + 7;
            while tok_start < bytes.len() && bytes[tok_start] == b' ' {
                tok_start += 1;
            }
            let mut end = tok_start;
            while end < bytes.len() && is_token_char(bytes[end]) {
                end += 1;
            }
            if end.saturating_sub(tok_start) >= 8 {
                out.push_str(&input[i..tok_start]);
                out.push_str(placeholder);
                i = end;
                count += 1;
                continue;
            }
        }
        let ch = input[i..].chars().next().unwrap();
        out.push(ch);
        i += ch.len_utf8();
    }
    (out, count)
}

/// Shared Guardrail redaction routine reused by both the live Onion Middleware,
/// Live Traffic Inspector, and Trajectory Studio 2.0 Markdown/Share-Card exporters.
pub fn redact_sensitive_text(text: &str, redact_secrets: bool, redact_pii: bool) -> (String, usize) {
    let mut current = text.to_string();
    let mut total = 0;

    if redact_secrets {
        let (next, c1) = redact_prefixed_tokens(
            &current,
            &["sk-ant-", "sk-", "AIza", "ghp_", "gho_", "AKIA"],
            "[REDACTED_SECRET]",
        );
        let (next2, c2) = redact_bearer_tokens(&next, "[REDACTED_SECRET]");
        current = next2;
        total += c1 + c2;
    }
    if redact_pii {
        let (next, c) = redact_emails_and_phones(&current);
        current = next;
        total += c;
    }

    (current, total)
}

fn check_tool_name_policy(blocked_tools: &[String], name: &str) -> Result<(), String> {
    let lower_name = name.to_lowercase();
    for blocked in blocked_tools {
        if lower_name == blocked.to_lowercase() {
            return Err(format!("high-risk tool '{}' blocked by guardrail policy", name));
        }
    }
    Ok(())
}

fn check_tool_args_policy(
    blocked_arg_patterns: &[String],
    name: &str,
    args_json: &str,
) -> Result<(), String> {
    let lower_args = args_json.to_lowercase();
    for pat in blocked_arg_patterns {
        if !pat.is_empty() && lower_args.contains(&pat.to_lowercase()) {
            return Err(format!(
                "high-risk tool '{}' arguments matched blocked pattern '{}'",
                name, pat
            ));
        }
    }
    Ok(())
}

#[derive(Debug, Clone)]
struct ActiveStreamToolCall {
    name: String,
    args_buf: String,
}

/// Real-time mid-stream SSE guardrail checker that intercepts blocked tool names on
/// `ToolCallStart` and dangerous argument patterns across split `ToolCallDelta` chunks.
#[derive(Debug, Clone, Default)]
pub struct StreamGuardrailChecker {
    blocked_tools: Vec<String>,
    blocked_arg_patterns: Vec<String>,
    active_calls: HashMap<usize, ActiveStreamToolCall>,
}

impl StreamGuardrailChecker {
    pub fn new(blocked_tools: Vec<String>, blocked_arg_patterns: Vec<String>) -> Self {
        Self {
            blocked_tools,
            blocked_arg_patterns,
            active_calls: HashMap::new(),
        }
    }

    pub fn is_enabled(&self) -> bool {
        !self.blocked_tools.is_empty() || !self.blocked_arg_patterns.is_empty()
    }

    /// Inspects a single streaming canonical event and returns `Err(reason)` the moment a
    /// blocked tool name or dangerous argument pattern is detected.
    pub fn check_event(&mut self, ev: &CanonicalStreamEvent) -> Result<(), String> {
        if !self.is_enabled() {
            return Ok(());
        }

        match ev {
            CanonicalStreamEvent::ToolCallStart { index, name, .. } => {
                check_tool_name_policy(&self.blocked_tools, name)?;
                self.active_calls.insert(
                    *index,
                    ActiveStreamToolCall {
                        name: name.clone(),
                        args_buf: String::new(),
                    },
                );
                Ok(())
            }
            CanonicalStreamEvent::ToolCallDelta {
                index,
                arguments_delta,
            } => {
                let call = self
                    .active_calls
                    .entry(*index)
                    .or_insert_with(|| ActiveStreamToolCall {
                        name: "<unknown>".to_string(),
                        args_buf: String::new(),
                    });
                call.args_buf.push_str(arguments_delta);
                let check_res =
                    check_tool_args_policy(&self.blocked_arg_patterns, &call.name, &call.args_buf);
                if call.args_buf.len() > MAX_STREAM_ARGS_BYTES {
                    let mut trim_idx = call.args_buf.len() - MAX_STREAM_ARGS_BYTES;
                    while trim_idx < call.args_buf.len() && !call.args_buf.is_char_boundary(trim_idx)
                    {
                        trim_idx += 1;
                    }
                    call.args_buf.drain(..trim_idx);
                }
                check_res
            }
            CanonicalStreamEvent::ContentBlockStop { index } => {
                self.active_calls.remove(index);
                Ok(())
            }
            CanonicalStreamEvent::MessageStop { .. } => {
                self.active_calls.clear();
                Ok(())
            }
            _ => Ok(()),
        }
    }
}

impl GuardrailMiddleware {
    fn redact_text(&self, text: &str) -> (String, usize) {
        redact_sensitive_text(text, self.redact_secrets, self.redact_pii)
    }

    pub fn check_tool_call(&self, name: &str, args_json: &str) -> Result<(), String> {
        check_tool_name_policy(&self.blocked_tools, name)?;
        check_tool_args_policy(&self.blocked_arg_patterns, name, args_json)
    }

    pub fn stream_checker(&self) -> StreamGuardrailChecker {
        StreamGuardrailChecker::new(
            self.blocked_tools.clone(),
            self.blocked_arg_patterns.clone(),
        )
    }
}

impl Middleware for GuardrailMiddleware {
    fn name(&self) -> &'static str {
        "guardrail"
    }

    fn handle<'a>(
        &'a self,
        mut ctx: MiddlewareContext,
        next: NextFn<'a>,
    ) -> BoxFuture<'a, Result<MiddlewareContext, MiddlewareRejection>> {
        Box::pin(async move {
            if self.redact_secrets || self.redact_pii {
                let mut total_redactions = 0usize;
                for part in &mut ctx.request.system {
                    if let ContentPart::Text { text, .. } = part {
                        let (redacted, c) = self.redact_text(text);
                        if c > 0 {
                            *text = redacted;
                            total_redactions += c;
                        }
                    }
                }
                for msg in &mut ctx.request.messages {
                    for part in &mut msg.parts {
                        if let ContentPart::Text { text, .. } = part {
                            let (redacted, c) = self.redact_text(text);
                            if c > 0 {
                                *text = redacted;
                                total_redactions += c;
                            }
                        }
                    }
                }
                if total_redactions > 0 {
                    ctx.annotations.insert(
                        "X-Gateway-Guardrail-Redactions".into(),
                        total_redactions.to_string(),
                    );
                }
            }

            let ctx = next(ctx).await?;

            // Post-leg: intercept high-risk tool calls emitted by upstream models
            if !self.blocked_tools.is_empty() || !self.blocked_arg_patterns.is_empty() {
                if let Some(ref resp) = ctx.response {
                    for part in &resp.parts {
                        if let ContentPart::ToolCall {
                            name, arguments, ..
                        } = part
                        {
                            let args_str = serde_json::to_string(arguments).unwrap_or_default();
                            if let Err(reason) = self.check_tool_call(name, &args_str) {
                                return Err(MiddlewareRejection::new(
                                    self.name(),
                                    403,
                                    "guardrail_blocked",
                                    reason,
                                ));
                            }
                        }
                    }
                }
            }

            Ok(ctx)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::UpstreamProtocol;
    use crate::ir::{
        CacheControl, CanonicalMessage, CanonicalRequest, CanonicalResponse, CanonicalUsage, Role,
    };
    use crate::middleware::MiddlewarePipeline;
    use serde_json::json;
    use std::sync::Arc;

    #[tokio::test]
    async fn redacts_secrets_and_pii_while_preserving_cache_control_and_blocking_high_risk_tools() {
        let guard = Arc::new(GuardrailMiddleware {
            redact_secrets: true,
            redact_pii: true,
            blocked_tools: vec!["drop_database".into()],
            blocked_arg_patterns: vec!["rm -rf /".into()],
        });
        let mut pipe = MiddlewarePipeline::new();
        pipe.push(guard);

        let req = CanonicalRequest {
            model: "claude-sonnet-4-20250514".into(),
            system: vec![ContentPart::Text {
                text: "Contact alice@example.com".into(),
                cache_control: Some(CacheControl::Ephemeral { ttl: None }),
            }],
            messages: vec![CanonicalMessage {
                role: Role::User,
                parts: vec![ContentPart::Text {
                    text: "Use key sk-ant-1234567890abcdef and call 13812345678".into(),
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

        let err = pipe
            .execute(ctx, |mut c| {
                Box::pin(async move {
                    // Verify pre-leg redacted PII & secret while preserving cache_control
                    match &c.request.system[0] {
                        ContentPart::Text {
                            text,
                            cache_control,
                        } => {
                            assert!(text.contains("[REDACTED_EMAIL]"));
                            assert!(cache_control.is_some());
                        }
                        _ => panic!("expected text"),
                    }
                    match &c.request.messages[0].parts[0] {
                        ContentPart::Text { text, .. } => {
                            assert!(text.contains("[REDACTED_SECRET]"));
                            assert!(text.contains("[REDACTED_PHONE]"));
                            assert!(!text.contains("sk-ant-"));
                        }
                        _ => panic!("expected text"),
                    }
                    assert_eq!(
                        c.annotations
                            .get("X-Gateway-Guardrail-Redactions")
                            .map(String::as_str),
                        Some("3")
                    );
                    // Simulate upstream returning a dangerous tool call
                    c.response = Some(CanonicalResponse {
                        id: "resp_1".into(),
                        model: "claude-sonnet-4-20250514".into(),
                        parts: vec![ContentPart::ToolCall {
                            id: "call_1".into(),
                            name: "bash".into(),
                            arguments: json!({ "cmd": "rm -rf / --no-preserve-root" }),
                            cache_control: None,
                        }],
                        stop_reason: Some("tool_use".into()),
                        usage: CanonicalUsage::default(),
                    });
                    (c, Ok::<_, ()>(()))
                })
            })
            .await
            .unwrap_err();

        assert_eq!(err.status, 403);
        assert_eq!(err.error_type, "guardrail_blocked");
    }

    #[test]
    fn stream_checker_blocks_tool_name_and_split_argument_deltas() {
        let guard = GuardrailMiddleware {
            redact_secrets: false,
            redact_pii: false,
            blocked_tools: vec!["drop_database".into()],
            blocked_arg_patterns: vec!["rm -rf /".into()],
        };

        // Direct public check_tool_call check
        assert!(guard.check_tool_call("DROP_DATABASE", "{}").is_err());
        assert!(guard.check_tool_call("bash", r#"{"cmd":"echo ok"}"#).is_ok());

        let mut checker = guard.stream_checker();

        // Blocked tool name is intercepted immediately on ToolCallStart
        let err = checker
            .check_event(&CanonicalStreamEvent::ToolCallStart {
                index: 0,
                id: "call_0".into(),
                name: "drop_database".into(),
            })
            .unwrap_err();
        assert!(err.contains("drop_database"));

        // Safe tool name starts cleanly, then split ToolCallDelta ("rm " + "-rf /") is caught mid-stream
        checker
            .check_event(&CanonicalStreamEvent::ToolCallStart {
                index: 1,
                id: "call_1".into(),
                name: "bash".into(),
            })
            .unwrap();
        checker
            .check_event(&CanonicalStreamEvent::ToolCallDelta {
                index: 1,
                arguments_delta: r#"{"cmd":"rm "#.into(),
            })
            .unwrap();
        let err = checker
            .check_event(&CanonicalStreamEvent::ToolCallDelta {
                index: 1,
                arguments_delta: r#"-rf / --no-preserve-root"}"#.into(),
            })
            .unwrap_err();
        assert!(err.contains("rm -rf /"));
    }

    #[test]
    fn stream_checker_bounds_accumulated_args_to_64kb() {
        let mut checker =
            StreamGuardrailChecker::new(vec![], vec!["forbidden_payload".into()]);
        checker
            .check_event(&CanonicalStreamEvent::ToolCallStart {
                index: 0,
                id: "call_big".into(),
                name: "write_file".into(),
            })
            .unwrap();

        let chunk = "a".repeat(20 * 1024);
        for _ in 0..5 {
            checker
                .check_event(&CanonicalStreamEvent::ToolCallDelta {
                    index: 0,
                    arguments_delta: chunk.clone(),
                })
                .unwrap();
            let buf_len = checker.active_calls.get(&0).unwrap().args_buf.len();
            assert!(buf_len <= MAX_STREAM_ARGS_BYTES);
        }

        // Still catches a split pattern even after >64KB of preceding benign args
        checker
            .check_event(&CanonicalStreamEvent::ToolCallDelta {
                index: 0,
                arguments_delta: "forbidden_".into(),
            })
            .unwrap();
        assert!(checker
            .check_event(&CanonicalStreamEvent::ToolCallDelta {
                index: 0,
                arguments_delta: "payload".into(),
            })
            .is_err());
    }
}
