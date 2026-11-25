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
    #[error("invalid configuration: {0}")]
    Validation(String),
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
    pub channels: Vec<ChannelConfig>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServerConfig {
    #[serde(default = "default_bind_addr")]
    pub bind_addr: String,
    #[serde(default = "default_timeout_secs")]
    pub request_timeout_secs: u64,
    /// Optional local bearer/x-api-key token required for inbound requests.
    /// Empty or omitted disables inbound auth (local dev default).
    #[serde(default)]
    pub local_auth_token: Option<String>,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            bind_addr: default_bind_addr(),
            request_timeout_secs: default_timeout_secs(),
            local_auth_token: None,
        }
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
    #[serde(default)]
    pub models: Vec<String>,
    /// Optional model alias map: requested_model -> upstream_model
    #[serde(default)]
    pub model_map: HashMap<String, String>,
}

fn default_weight() -> u32 {
    100
}

impl ChannelConfig {
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
        let cfg: Self = toml::from_str(&raw)?;
        cfg.validate()?;
        Ok(cfg)
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
}
