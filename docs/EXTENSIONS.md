# Extending LoomGate

LoomGate is built around a **zero-hardcoding microkernel** with three first-class extension points:

1. **Declarative Client Profiles** (zero Rust code — TOML/JSON)
2. **Onion-Model `Middleware` Plugins** (Rust `Middleware` Trait)
3. **Custom Wire Protocols** (Rust `ProtocolCodec` Trait)

---

## 1. Add Any New AI Agent Profile (Zero Rust Code)

LoomGate loads built-in profiles and merges user-defined profiles from:
- `~/.loomgate/client_profiles.toml`
- `~/.loomgate/client_profiles.json`
- Any explicit path in the `LOOMGATE_CLIENT_PROFILES` environment variable

### Example A: Atomic Takeover Profile (TOML)
```toml
[[profiles]]
id = "aider"
display_name = "Aider CLI"
category = "CLI Agent"
kind = "atomic_takeover"
description = "Atomically point Aider to LoomGate with automatic .bak rollback."

[profiles.atomic]
relative_config_path = ".aider/config.json"
format = "json"

[profiles.atomic.set_fields]
"openai.api_base" = "{{GATEWAY_URL}}/v1"

[profiles.atomic.default_fields]
"openai.api_key" = "{{API_KEY}}"
```

### Example B: Setup Wizard Profile (JSON)
```json
{
  "profiles": [
    {
      "id": "windsurf",
      "display_name": "Windsurf IDE",
      "category": "AI IDE",
      "kind": "setup_wizard",
      "description": "Connect Windsurf custom OpenAI-compatible provider to LoomGate.",
      "wizard": {
        "protocol": "openai",
        "endpoint_template": "{{GATEWAY_URL}}/v1",
        "env_snippet_template": "OPENAI_BASE_URL={{GATEWAY_URL}}/v1\nOPENAI_API_KEY={{API_KEY}}",
        "config_snippet_template": "{\n  \"baseUrl\": \"{{GATEWAY_URL}}/v1\",\n  \"apiKey\": \"{{API_KEY}}\"\n}",
        "instructions": [
          "Open Windsurf Settings -> Custom Provider.",
          "Paste {{GATEWAY_URL}}/v1 as the Base URL and {{API_KEY}} as API Key."
        ]
      }
    }
  ]
}
```

Restarting is not required — `list_profiles()` reloads the file dynamically on every inspection.

---

## 2. Write a Custom Onion `Middleware` Plugin

Every request passes symmetrically through the `MiddlewarePipeline`:
`M0(pre) → M1(pre) → ... → Upstream Dispatch → ... → M1(post) → M0(post)`.

Implement `gateway_core::Middleware` to inspect/transform `CanonicalRequest` on the inbound leg, inspect `CanonicalResponse` on the outbound leg, emit transparent HTTP headers via `ctx.annotations`, or short-circuit with `MiddlewareRejection`:

```rust
use gateway_core::middleware::{
    BoxFuture, Middleware, MiddlewareContext, MiddlewareRejection, NextFn,
};
use gateway_core::ir::ContentPart;

/// Example: blocks prompts exceeding a max character budget and adds an audit header.
pub struct PromptBudgetMiddleware {
    pub max_chars: usize,
}

impl Middleware for PromptBudgetMiddleware {
    fn name(&self) -> &'static str {
        "prompt_budget"
    }

    fn handle<'a>(
        &'a self,
        mut ctx: MiddlewareContext,
        next: NextFn<'a>,
    ) -> BoxFuture<'a, Result<MiddlewareContext, MiddlewareRejection>> {
        Box::pin(async move {
            let total_chars: usize = ctx
                .request
                .messages
                .iter()
                .flat_map(|m| m.parts.iter())
                .map(|p| match p {
                    ContentPart::Text { text, .. } => text.len(),
                    _ => 0,
                })
                .sum();

            if total_chars > self.max_chars {
                return Err(MiddlewareRejection::new(
                    self.name(),
                    413,
                    "prompt_too_large",
                    format!("prompt length {} exceeds budget {}", total_chars, self.max_chars),
                ));
            }

            ctx.annotations
                .insert("X-Gateway-Prompt-Chars".into(), total_chars.to_string());

            // Continue inward to downstream middlewares & upstream dispatch
            let ctx = next(ctx).await?;

            // Outbound post-leg runs here (inspect `ctx.response` if non-streaming)
            Ok(ctx)
        })
    }
}
```

Register your layer inside `MiddlewarePipeline::push(Arc::new(PromptBudgetMiddleware { max_chars: 200_000 }))` in `crates/gateway-core/src/engine.rs`.

---

## 3. Add a New Wire Protocol (`ProtocolCodec` Trait)

LoomGate isolates all provider-specific wire formats behind `gateway_core::codec::ProtocolCodec`. Adding a new protocol (e.g. AWS Bedrock Converse or Cohere v2) requires **zero changes** to the Sticky P2C Router, SSE UTF-8 FSM, Error Governance, Trajectory SQLite Store, or Middleware Pipeline:

```rust
use gateway_core::codec::{CodecError, ProtocolCodec};
use gateway_core::config::UpstreamProtocol;
use gateway_core::ir::{CanonicalRequest, CanonicalResponse, CanonicalStreamEvent};
use serde_json::{json, Value};

pub struct CustomProtocolCodec;

impl ProtocolCodec for CustomProtocolCodec {
    fn protocol(&self) -> UpstreamProtocol {
        UpstreamProtocol::OpenAi // Replace with your new UpstreamProtocol variant
    }

    fn decode_request(
        &self,
        payload: &Value,
        session_id: Option<String>,
        task_stage: Option<String>,
    ) -> Result<CanonicalRequest, CodecError> {
        // Parse wire JSON into lossless CanonicalRequest (preserving cache_control, thinking, tools)
        todo!("decode wire request -> CanonicalRequest")
    }

    fn encode_request(
        &self,
        req: &CanonicalRequest,
        upstream_model: &str,
        same_protocol: bool,
    ) -> Result<Value, CodecError> {
        // Serialize CanonicalRequest -> upstream wire JSON
        todo!("encode CanonicalRequest -> wire request")
    }

    fn decode_response(&self, body: &Value) -> Result<CanonicalResponse, CodecError> {
        todo!("decode upstream response -> CanonicalResponse")
    }

    fn encode_response(&self, resp: &CanonicalResponse) -> Value {
        json!({ "id": resp.id, "model": resp.model, "text": resp.concatenated_text() })
    }

    fn decode_sse_frame(&self, _event_name: Option<&str>, _data: &str) -> Vec<CanonicalStreamEvent> {
        vec![]
    }

    fn encode_sse_event(&self, _event: &CanonicalStreamEvent) -> Vec<String> {
        vec![]
    }

    fn encode_error(&self, error_type: &str, message: &str) -> Value {
        json!({ "error": { "type": error_type, "message": message } })
    }
}
```

Wiring checklist for a new protocol:
1. Add a variant to `UpstreamProtocol` in `crates/gateway-core/src/config.rs`.
2. Register it in `codec_for(...)` inside `crates/gateway-core/src/codec/mod.rs`.
3. Add endpoint URL + auth header mapping in `crates/gateway-core/src/engine.rs`.
4. Add a round-trip unit test in your codec module and an end-to-end test in `tests/conformance_suite.py`.
