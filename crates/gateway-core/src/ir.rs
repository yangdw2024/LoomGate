use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;

/// First-class Prompt Caching breakpoint semantics (losslessly preserved across protocols).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum CacheControl {
    Ephemeral {
        #[serde(skip_serializing_if = "Option::is_none")]
        ttl: Option<String>,
    },
}

/// First-class reasoning / extended thinking configuration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReasoningConfig {
    pub enabled: bool,
    /// Anthropic-style explicit token budget (e.g., 8192).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub budget_tokens: Option<u32>,
    /// OpenAI-style reasoning effort ("low" | "medium" | "high").
    #[serde(skip_serializing_if = "Option::is_none")]
    pub effort: Option<String>,
}

/// Opaque provider state (e.g., Anthropic thinking `signature`, Gemini `thought_signature`).
/// Preserved verbatim through the IR and hydrated from sidecar `SignatureCache` when omitted.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct OpaqueProviderState {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thought_signature: Option<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub extra: BTreeMap<String, Value>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    System,
    User,
    Assistant,
    Tool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ContentPart {
    Text {
        text: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        cache_control: Option<CacheControl>,
    },
    Thinking {
        thinking: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        signature: Option<String>,
    },
    ToolCall {
        id: String,
        name: String,
        arguments: Value,
        #[serde(skip_serializing_if = "Option::is_none")]
        cache_control: Option<CacheControl>,
    },
    ToolResult {
        tool_call_id: String,
        content: Value,
        #[serde(default)]
        is_error: bool,
        #[serde(skip_serializing_if = "Option::is_none")]
        cache_control: Option<CacheControl>,
    },
    /// Pass-through for media or provider-specific blocks not yet normalized.
    Opaque {
        raw: Value,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CanonicalMessage {
    pub role: Role,
    pub parts: Vec<ContentPart>,
}

/// Strict Tool Schema preserved as a first-class citizen.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StrictToolSchema {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub parameters: Value,
    #[serde(default)]
    pub strict: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache_control: Option<CacheControl>,
}

/// Canonical Intermediate Representation (Superset IR) for all LLM inference requests.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CanonicalRequest {
    pub model: String,
    #[serde(default)]
    pub system: Vec<ContentPart>,
    pub messages: Vec<CanonicalMessage>,
    #[serde(default)]
    pub tools: Vec<StrictToolSchema>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_choice: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub top_p: Option<f32>,
    #[serde(default)]
    pub stream: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<ReasoningConfig>,
    #[serde(default)]
    pub opaque_state: OpaqueProviderState,
    /// Client-supplied session ID (from X-Session-Id or metadata.user_id) for sticky routing.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    /// Optional explicit task stage tag (e.g., "title_generation", "code_edit") for Stage-Aware routing.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub task_stage: Option<String>,
    /// Unknown top-level fields preserved ONLY for same-protocol passthrough.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub extra_fields: BTreeMap<String, Value>,
}

fn hash_length_prefixed(hasher: &mut Sha256, bytes: &[u8]) {
    hasher.update((bytes.len() as u64).to_be_bytes());
    hasher.update(bytes);
}

impl CanonicalRequest {
    /// Computes a deterministic, Turn-1-stable prefix fingerprint from `session_id` (if present)
    /// or `system + tools + first user message` with length-prefixed framing.
    /// Because the anchor is fixed on Turn 1, appending Turn 2..N never mutates the fingerprint,
    /// guaranteeing 100% session stickiness from the very first turn.
    pub fn prefix_fingerprint(&self, _prefix_turns: usize) -> String {
        if let Some(ref sid) = self.session_id {
            if !sid.trim().is_empty() {
                return format!("sid:{}", sid.trim());
            }
        }
        let mut hasher = Sha256::new();
        hash_length_prefixed(&mut hasher, b"v2");
        if let Ok(sys_bytes) = serde_json::to_vec(&self.system) {
            hash_length_prefixed(&mut hasher, &sys_bytes);
        }
        if let Ok(tool_bytes) = serde_json::to_vec(&self.tools) {
            hash_length_prefixed(&mut hasher, &tool_bytes);
        }
        if let Some(first_user) = self
            .messages
            .iter()
            .find(|m| m.role == Role::User)
            .or_else(|| self.messages.first())
        {
            if let Ok(m_bytes) = serde_json::to_vec(first_user) {
                hash_length_prefixed(&mut hasher, &m_bytes);
            }
        }
        let digest = hasher.finalize();
        let hex: String = digest.iter().take(12).map(|b| format!("{:02x}", b)).collect();
        format!("pfx:{}", hex)
    }

    pub fn has_cache_breakpoints(&self) -> bool {
        let sys_has = self.system.iter().any(|p| match p {
            ContentPart::Text { cache_control, .. } => cache_control.is_some(),
            _ => false,
        });
        let tools_has = self.tools.iter().any(|t| t.cache_control.is_some());
        let msg_has = self.messages.iter().any(|m| {
            m.parts.iter().any(|p| match p {
                ContentPart::Text { cache_control, .. }
                | ContentPart::ToolCall { cache_control, .. }
                | ContentPart::ToolResult { cache_control, .. } => cache_control.is_some(),
                _ => false,
            })
        });
        sys_has || tools_has || msg_has
    }
}

/// Token usage & Prompt Caching metrics normalized across all providers.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CanonicalUsage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    #[serde(default)]
    pub cache_creation_input_tokens: u64,
    #[serde(default)]
    pub cache_read_input_tokens: u64,
}

impl CanonicalUsage {
    pub fn cache_hit_ratio(&self) -> f64 {
        let total_input = self.input_tokens + self.cache_read_input_tokens;
        if total_input == 0 {
            0.0
        } else {
            self.cache_read_input_tokens as f64 / total_input as f64
        }
    }
}

/// Unified non-streaming response IR so cross-protocol responses (e.g. OpenAI -> Anthropic)
/// are losslessly decoded and re-encoded in the downstream client's native schema.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CanonicalResponse {
    pub id: String,
    pub model: String,
    pub parts: Vec<ContentPart>,
    pub stop_reason: Option<String>,
    pub usage: CanonicalUsage,
}

impl CanonicalResponse {
    pub fn concatenated_text(&self) -> String {
        self.parts
            .iter()
            .filter_map(|p| match p {
                ContentPart::Text { text, .. } => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n")
    }
}

/// Unified streaming event emitted by the zero-buffer SSE Finite State Machine.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum CanonicalStreamEvent {
    MessageStart {
        id: String,
        model: String,
        usage: Option<CanonicalUsage>,
    },
    ContentBlockStart {
        index: usize,
        block_type: String,
    },
    ContentBlockStop {
        index: usize,
    },
    TextDelta {
        index: usize,
        text: String,
    },
    ThinkingDelta {
        index: usize,
        thinking: String,
    },
    SignatureDelta {
        index: usize,
        signature: String,
    },
    ToolCallStart {
        index: usize,
        id: String,
        name: String,
    },
    ToolCallDelta {
        index: usize,
        arguments_delta: String,
    },
    UsageDelta {
        usage: CanonicalUsage,
    },
    MessageStop {
        stop_reason: Option<String>,
    },
    Error {
        message: String,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prefix_fingerprint_is_stable_from_turn_1_through_turn_10() {
        let mut req = CanonicalRequest {
            model: "claude-sonnet-4-20250514".into(),
            system: vec![ContentPart::Text {
                text: "You are a coding agent.".into(),
                cache_control: Some(CacheControl::Ephemeral { ttl: None }),
            }],
            messages: vec![CanonicalMessage {
                role: Role::User,
                parts: vec![ContentPart::Text {
                    text: "Turn 0: inspect Cargo.toml".into(),
                    cache_control: None,
                }],
            }],
            tools: vec![],
            tool_choice: None,
            max_tokens: Some(4096),
            temperature: None,
            top_p: None,
            stream: true,
            reasoning: None,
            opaque_state: Default::default(),
            session_id: None,
            task_stage: None,
            extra_fields: Default::default(),
        };

        let fp0 = req.prefix_fingerprint(4);
        for turn in 1..10 {
            req.messages.push(CanonicalMessage {
                role: Role::Assistant,
                parts: vec![ContentPart::Text {
                    text: format!("Ack {}", turn),
                    cache_control: None,
                }],
            });
            req.messages.push(CanonicalMessage {
                role: Role::User,
                parts: vec![ContentPart::Text {
                    text: format!("Turn {}", turn),
                    cache_control: None,
                }],
            });
            assert_eq!(
                req.prefix_fingerprint(4),
                fp0,
                "fingerprint drifted on turn {}",
                turn
            );
        }
        assert!(req.has_cache_breakpoints());
    }
}
