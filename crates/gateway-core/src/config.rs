use crate::middleware::mcp_bridge::McpServerConfig;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::path::Path;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("failed to read config file: {0}")]
    Io(#[from] std::io::Error),
    #[error("failed to parse TOML config: {0}")]
    Parse(#[from] toml::de::Error),
    #[error("failed to serialize TOML config: {0}")]
    Serialize(#[from] toml::ser::Error),
    #[error("invalid configuration: {0}")]
    Validation(String),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelPricing {
    /// Input token price in USD per 1M tokens.
    pub input_per_1m_usd: f64,
    /// Prompt cache read token price in USD per 1M tokens.
    pub cache_read_per_1m_usd: f64,
    /// Output token price in USD per 1M tokens.
    pub output_per_1m_usd: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GatewayConfig {
    #[serde(default)]
    pub server: ServerConfig,
    #[serde(default)]
    pub routing: RoutingConfig,
    #[serde(default)]
    pub error_governance: ErrorGovernanceConfig,
    #[serde(default)]
    pub middleware: MiddlewareConfig,
    /// Optional per-model USD pricing overrides for the local Savings & Telemetry calculator.
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub model_pricing: HashMap<String, ModelPricing>,
    #[serde(default)]
    pub channels: Vec<ChannelConfig>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MiddlewareConfig {
    /// Local ingress rate limit in requests per minute (0 = unlimited).
    #[serde(default)]
    pub rpm_limit: u32,
    #[serde(default)]
    pub rate_limit_burst: u32,
    /// Outbound Guardrail: redact API keys/secrets before sending to cloud upstreams.
    #[serde(default)]
    pub redact_secrets: bool,
    /// Outbound Guardrail: redact emails & phone numbers before sending to cloud upstreams.
    #[serde(default)]
    pub redact_pii: bool,
    /// High-risk tool names blocked on upstream tool_use responses.
    #[serde(default)]
    pub blocked_tools: Vec<String>,
    /// Dangerous argument substrings blocked on upstream tool_use responses.
    #[serde(default)]
    pub blocked_arg_patterns: Vec<String>,
    /// Context/Memory middleware: inject workspace facts & episodic snippets.
    #[serde(default)]
    pub memory_enabled: bool,
    #[serde(default)]
    pub memory_facts: Vec<String>,
    #[serde(default = "default_memory_max_entries")]
    pub memory_max_entries: usize,
    /// Unified MCP Server aggregation bridge definitions.
    #[serde(default)]
    pub mcp_servers: Vec<McpServerConfig>,
}

fn default_memory_max_entries() -> usize {
    20
}

impl Default for MiddlewareConfig {
    fn default() -> Self {
        Self {
            rpm_limit: 0,
            rate_limit_burst: 0,
            redact_secrets: false,
            redact_pii: false,
            blocked_tools: Vec::new(),
            blocked_arg_patterns: Vec::new(),
            memory_enabled: false,
            memory_facts: Vec::new(),
            memory_max_entries: default_memory_max_entries(),
            mcp_servers: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServerConfig {
    #[serde(default = "default_bind_addr")]
    pub bind_addr: String,
    #[serde(default = "default_timeout_secs")]
    pub request_timeout_secs: u64,
    /// Legacy alias for `api_key` (retained for backward compatibility).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub local_auth_token: Option<String>,
    /// API key required for inference endpoints (`/v1/*`, `/v1beta/*`).
    /// Empty or omitted disables inference auth (passwordless localhost dev mode).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key: Option<String>,
    /// Admin password required for control/management endpoints (`/api/*`, `/__admin/*`).
    /// Empty or omitted disables admin auth (passwordless localhost dev mode).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub admin_password: Option<String>,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            bind_addr: default_bind_addr(),
            request_timeout_secs: default_timeout_secs(),
            local_auth_token: None,
            api_key: None,
            admin_password: None,
        }
    }
}

impl ServerConfig {
    pub fn effective_api_key(&self) -> Option<String> {
        self.api_key
            .as_deref()
            .or(self.local_auth_token.as_deref())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(String::from)
    }

    pub fn effective_admin_password(&self) -> Option<String> {
        self.admin_password
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(String::from)
    }
}

fn default_bind_addr() -> String {
    "127.0.0.1:8787".to_string()
}

fn default_timeout_secs() -> u64 {
    300
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RouteMode {
    /// Strict model lock: never silently downgrade to a cheaper model.
    #[default]
    Lock,
    /// Explicit rule-based routing per task stage (title_generation, code_edit, etc.).
    StageAware,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RoutingConfig {
    #[serde(default)]
    pub default_mode: RouteMode,
    #[serde(default = "default_sticky_ttl_secs")]
    pub sticky_ttl_secs: u64,
    #[serde(default = "default_prefix_hash_turns")]
    pub prefix_hash_turns: usize,
    #[serde(default)]
    pub stage_models: HashMap<String, String>,
}

impl Default for RoutingConfig {
    fn default() -> Self {
        Self {
            default_mode: RouteMode::Lock,
            sticky_ttl_secs: default_sticky_ttl_secs(),
            prefix_hash_turns: default_prefix_hash_turns(),
            stage_models: HashMap::new(),
        }
    }
}

fn default_sticky_ttl_secs() -> u64 {
    1800
}

fn default_prefix_hash_turns() -> usize {
    4
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ErrorGovernanceConfig {
    #[serde(default = "default_l1_max_retries")]
    pub l1_max_retries: u32,
    #[serde(default = "default_l1_base_backoff_ms")]
    pub l1_base_backoff_ms: u64,
    #[serde(default = "default_l1_max_backoff_ms")]
    pub l1_max_backoff_ms: u64,
    #[serde(default = "default_l2_cooldown_secs")]
    pub l2_cooldown_secs: u64,
    #[serde(default = "default_l3_quarantine_secs")]
    pub l3_quarantine_secs: u64,
}

impl Default for ErrorGovernanceConfig {
    fn default() -> Self {
        Self {
            l1_max_retries: default_l1_max_retries(),
            l1_base_backoff_ms: default_l1_base_backoff_ms(),
            l1_max_backoff_ms: default_l1_max_backoff_ms(),
            l2_cooldown_secs: default_l2_cooldown_secs(),
            l3_quarantine_secs: default_l3_quarantine_secs(),
        }
    }
}

fn default_l1_max_retries() -> u32 {
    2
}
fn default_l1_base_backoff_ms() -> u64 {
    1000
}
fn default_l1_max_backoff_ms() -> u64 {
    3000
}
fn default_l2_cooldown_secs() -> u64 {
    60
}
fn default_l3_quarantine_secs() -> u64 {
    900
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UpstreamProtocol {
    Anthropic,
    #[serde(rename = "openai", alias = "open_ai")]
    OpenAi,
    #[serde(rename = "gemini", alias = "google_gemini")]
    Gemini,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChannelConfig {
    pub id: String,
    pub protocol: UpstreamProtocol,
    pub base_url: String,
    pub api_key: String,
    #[serde(default = "default_weight")]
    pub weight: u32,
    #[serde(default = "default_channel_enabled")]
    pub enabled: bool,
    #[serde(default)]
    pub models: Vec<String>,
    /// Optional model alias map: requested_model -> upstream_model
    #[serde(default)]
    pub model_map: HashMap<String, String>,
    /// Optional per-channel HTTP/HTTPS/SOCKS5 proxy URL (e.g. "socks5h://127.0.0.1:1080").
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub proxy_url: Option<String>,
    /// Optional explicit fallback model chain (used in Stage-Aware mode when primary model is unavailable).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub fallback_models: Vec<String>,
    /// Optional upstream quota / balance probe endpoint URL.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub quota_endpoint: Option<String>,
    /// Marks this channel (typically Ollama / LM Studio / vLLM) as an offline fallback target
    /// automatically used when all cloud/primary channels are tripped or unreachable.
    #[serde(default)]
    pub offline_fallback: bool,
}

fn default_weight() -> u32 {
    100
}

fn default_channel_enabled() -> bool {
    true
}

/// Normalizes an upstream `base_url` by trimming whitespace/slashes and stripping redundant
/// trailing `/v1` or `/v1beta` segments so downstream endpoint joining never produces `/v1/v1/...`.
pub fn normalize_base_url(url: &str) -> String {
    let mut s = url.trim().trim_end_matches('/').to_string();
    loop {
        if let Some(stripped) = s.strip_suffix("/v1") {
            s = stripped.trim_end_matches('/').to_string();
        } else if let Some(stripped) = s.strip_suffix("/v1beta") {
            s = stripped.trim_end_matches('/').to_string();
        } else {
            break;
        }
    }
    s
}

impl ChannelConfig {
    pub fn normalize(&mut self) {
        self.id = self.id.trim().to_string();
        self.base_url = normalize_base_url(&self.base_url);
        self.proxy_url = self
            .proxy_url
            .take()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());
        self.quota_endpoint = self
            .quota_endpoint
            .take()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());
        if self.weight == 0 {
            self.weight = default_weight();
        }
    }

    pub fn supports_model(&self, model: &str) -> bool {
        if self.models.is_empty() || self.model_map.contains_key(model) {
            return true;
        }
        let resolved = self.resolve_upstream_model(model);
        self.models
            .iter()
            .any(|m| m == model || m == &resolved || m == "*")
    }

    pub fn resolve_upstream_model(&self, requested_model: &str) -> String {
        self.model_map
            .get(requested_model)
            .cloned()
            .unwrap_or_else(|| requested_model.to_string())
    }
}

impl GatewayConfig {
    pub fn from_file(path: impl AsRef<Path>) -> Result<Self, ConfigError> {
        let raw = std::fs::read_to_string(path)?;
        let mut cfg: Self = toml::from_str(&raw)?;
        for ch in &mut cfg.channels {
            ch.normalize();
        }
        cfg.validate()?;
        Ok(cfg)
    }

    /// Atomically persists the full configuration to `path` via sibling temp file + rename.
    pub fn save_atomic(&self, path: impl AsRef<Path>) -> Result<(), ConfigError> {
        let target = path.as_ref();
        if let Some(parent) = target.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent)?;
            }
        }
        let body = toml::to_string_pretty(self)?;
        let tmp = target.with_extension(format!("tmp.{}", std::process::id()));
        std::fs::write(&tmp, body.as_bytes())?;
        std::fs::rename(&tmp, target)?;
        Ok(())
    }

    pub fn validate(&self) -> Result<(), ConfigError> {
        if self.channels.is_empty() {
            return Err(ConfigError::Validation(
                "at least one upstream channel must be configured".into(),
            ));
        }

        let mut seen_ids = HashSet::new();
        for ch in &self.channels {
            if ch.id.trim().is_empty() {
                return Err(ConfigError::Validation("channel id cannot be empty".into()));
            }
            if !seen_ids.insert(ch.id.clone()) {
                return Err(ConfigError::Validation(format!(
                    "duplicate channel id: '{}'",
                    ch.id
                )));
            }
            if ch.base_url.trim().is_empty() {
                return Err(ConfigError::Validation(format!(
                    "channel '{}' has an empty base_url",
                    ch.id
                )));
            }
            if ch.weight == 0 {
                return Err(ConfigError::Validation(format!(
                    "channel '{}' weight must be >= 1",
                    ch.id
                )));
            }
        }

        for (stage, target_model) in &self.routing.stage_models {
            if !self.channels.iter().any(|c| c.supports_model(target_model)) {
                return Err(ConfigError::Validation(format!(
                    "stage_models['{}'] targets '{}' which no configured channel supports",
                    stage, target_model
                )));
            }
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_openai_and_open_ai_aliases_and_validates() {
        let raw = r#"
[routing.stage_models]
title_generation = "gpt-4o-mini"

[[channels]]
id = "oai"
protocol = "openai"
base_url = "http://127.0.0.1:18083"
api_key = "k"
models = ["gpt-4o-mini"]
"#;
        let cfg: GatewayConfig = toml::from_str(raw).unwrap();
        assert_eq!(cfg.channels[0].protocol, UpstreamProtocol::OpenAi);
        cfg.validate().unwrap();
    }

    #[test]
    fn normalizes_trailing_v1_and_v1beta_and_persists_atomically() {
        assert_eq!(normalize_base_url("https://api.openai.com/v1/"), "https://api.openai.com");
        assert_eq!(normalize_base_url("https://api.openai.com/v1/v1"), "https://api.openai.com");
        assert_eq!(
            normalize_base_url("https://generativelanguage.googleapis.com/v1beta/"),
            "https://generativelanguage.googleapis.com"
        );

        let mut cfg = GatewayConfig {
            server: ServerConfig {
                api_key: Some("lg_api".into()),
                admin_password: Some("lg_admin".into()),
                ..Default::default()
            },
            routing: Default::default(),
            error_governance: Default::default(),
            middleware: Default::default(),
            model_pricing: HashMap::new(),
            channels: vec![ChannelConfig {
                id: "ch1".into(),
                protocol: UpstreamProtocol::OpenAi,
                base_url: "https://api.openai.com/v1/".into(),
                api_key: "sk-test".into(),
                weight: 100,
                enabled: true,
                models: vec!["gpt-4o".into()],
                model_map: Default::default(),
                proxy_url: Some("socks5h://127.0.0.1:1080".into()),
                fallback_models: vec!["gpt-4o-mini".into()],
                quota_endpoint: Some("https://api.openai.com/dashboard/billing/usage".into()),
                offline_fallback: false,
            }],
        };
        cfg.channels[0].normalize();
        assert_eq!(cfg.channels[0].base_url, "https://api.openai.com");

        let tmp = std::env::temp_dir().join(format!("lg_cfg_save_{}.toml", std::process::id()));
        cfg.save_atomic(&tmp).unwrap();
        let reloaded = GatewayConfig::from_file(&tmp).unwrap();
        assert_eq!(reloaded.server.effective_api_key().as_deref(), Some("lg_api"));
        assert_eq!(reloaded.server.effective_admin_password().as_deref(), Some("lg_admin"));
        assert_eq!(reloaded.channels[0].proxy_url.as_deref(), Some("socks5h://127.0.0.1:1080"));
        let _ = std::fs::remove_file(&tmp);
    }
}
