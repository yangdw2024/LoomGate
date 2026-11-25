use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::fs;
use std::path::{Path, PathBuf};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum TakeoverError {
    #[error("could not resolve user home directory")]
    NoHomeDir,
    #[error("filesystem error: {0}")]
    Io(#[from] std::io::Error),
    #[error("json parse error: {0}")]
    Json(#[from] serde_json::Error),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CliTool {
    ClaudeCode,
    Codex,
    GeminiCli,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CliIntegrationStatus {
    pub tool: CliTool,
    pub display_name: String,
    pub config_path: String,
    pub installed: bool,
    pub taken_over: bool,
    pub has_backup: bool,
    pub active_endpoint: Option<String>,
}

/// Manages CC Switch-style atomic config takeover & lossless rollback for local coding CLIs.
pub struct CliTakeoverManager {
    home_dir: PathBuf,
    gateway_url: String,
}

impl CliTakeoverManager {
    pub fn new(gateway_url: impl Into<String>) -> Result<Self, TakeoverError> {
        let home_dir = dirs::home_dir().ok_or(TakeoverError::NoHomeDir)?;
        Ok(Self {
            home_dir,
            gateway_url: gateway_url.into().trim_end_matches('/').to_string(),
        })
    }

    pub fn with_home_dir(home_dir: PathBuf, gateway_url: impl Into<String>) -> Self {
        Self {
            home_dir,
            gateway_url: gateway_url.into().trim_end_matches('/').to_string(),
        }
    }

    pub fn config_path(&self, tool: CliTool) -> PathBuf {
        match tool {
            CliTool::ClaudeCode => self.home_dir.join(".claude").join("settings.json"),
            CliTool::Codex => self.home_dir.join(".codex").join("config.toml"),
            CliTool::GeminiCli => self.home_dir.join(".gemini").join("settings.json"),
        }
    }

    fn backup_path(target: &Path) -> PathBuf {
        let mut os = target.as_os_str().to_os_string();
        os.push(".edge-gateway.bak");
        PathBuf::from(os)
    }

    pub fn list_statuses(&self) -> Vec<CliIntegrationStatus> {
        [CliTool::ClaudeCode, CliTool::Codex, CliTool::GeminiCli]
            .into_iter()
            .map(|tool| self.inspect(tool))
            .collect()
    }

    pub fn inspect(&self, tool: CliTool) -> CliIntegrationStatus {
        let path = self.config_path(tool);
        let backup = Self::backup_path(&path);
        let installed = path.exists() || path.parent().map(|p| p.exists()).unwrap_or(false);
        let raw = fs::read_to_string(&path).unwrap_or_default();
        let taken_over = raw.contains(&self.gateway_url);

        let display_name = match tool {
            CliTool::ClaudeCode => "Claude Code",
            CliTool::Codex => "Codex CLI",
            CliTool::GeminiCli => "Gemini CLI",
        }
        .to_string();

        CliIntegrationStatus {
            tool,
            display_name,
            config_path: path.display().to_string(),
            installed,
            taken_over,
            has_backup: backup.exists(),
            active_endpoint: if taken_over {
                Some(self.gateway_url.clone())
            } else {
                None
            },
        }
    }

    /// Atomically takes over the target CLI's configuration without requiring terminal restart.
    /// Creates a `.edge-gateway.bak` snapshot on first takeover for lossless rollback.
    pub fn takeover(&self, tool: CliTool) -> Result<CliIntegrationStatus, TakeoverError> {
        let path = self.config_path(tool);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }

        let backup = Self::backup_path(&path);
        if path.exists() && !backup.exists() {
            fs::copy(&path, &backup)?;
        }

        match tool {
            CliTool::ClaudeCode => {
                let mut doc: Value = if path.exists() {
                    serde_json::from_str(&fs::read_to_string(&path)?)?
                } else {
                    json!({})
                };
                let env = doc
                    .as_object_mut()
                    .unwrap()
                    .entry("env")
                    .or_insert_with(|| json!({}));
                if let Some(env_obj) = env.as_object_mut() {
                    env_obj.insert("ANTHROPIC_BASE_URL".into(), json!(self.gateway_url));
                    env_obj
                        .entry("ANTHROPIC_AUTH_TOKEN")
                        .or_insert_with(|| json!("local-edge-gateway"));
                }
                atomic_write(&path, serde_json::to_string_pretty(&doc)?.as_bytes())?;
            }
            CliTool::Codex => {
                let existing = fs::read_to_string(&path).unwrap_or_default();
                let mut table: toml::Table = if existing.trim().is_empty() {
                    toml::Table::new()
                } else {
                    existing.parse().unwrap_or_else(|_| toml::Table::new())
                };
                table.insert(
                    "openai_base_url".into(),
                    toml::Value::String(format!("{}/v1", self.gateway_url)),
                );
                let serialized = toml::to_string_pretty(&table).unwrap_or_default();
                atomic_write(&path, serialized.as_bytes())?;
            }
            CliTool::GeminiCli => {
                let mut doc: Value = if path.exists() {
                    serde_json::from_str(&fs::read_to_string(&path)?)?
                } else {
                    json!({})
                };
                let env = doc
                    .as_object_mut()
                    .unwrap()
                    .entry("env")
                    .or_insert_with(|| json!({}));
                if let Some(env_obj) = env.as_object_mut() {
                    env_obj.insert("GEMINI_API_BASE_URL".into(), json!(self.gateway_url));
                }
                atomic_write(&path, serde_json::to_string_pretty(&doc)?.as_bytes())?;
            }
        }

        Ok(self.inspect(tool))
    }

    /// Restores the original pre-takeover CLI configuration atomically.
    pub fn restore(&self, tool: CliTool) -> Result<CliIntegrationStatus, TakeoverError> {
        let path = self.config_path(tool);
        let backup = Self::backup_path(&path);
        if backup.exists() {
            let bytes = fs::read(&backup)?;
            atomic_write(&path, &bytes)?;
            let _ = fs::remove_file(&backup);
        }
        Ok(self.inspect(tool))
    }
}

/// Write to sibling temp file and atomically rename so concurrent CLI reads never see partial JSON/TOML.
fn atomic_write(target: &Path, data: &[u8]) -> Result<(), std::io::Error> {
    let tmp = target.with_extension(format!("tmp.{}", std::process::id()));
    fs::write(&tmp, data)?;
    fs::rename(&tmp, target)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn claude_code_atomic_takeover_and_restore_roundtrip() {
        let tmp_home = std::env::temp_dir().join(format!("eg_takeover_{}", std::process::id()));
        let _ = fs::remove_dir_all(&tmp_home);
        fs::create_dir_all(tmp_home.join(".claude")).unwrap();
        let settings = tmp_home.join(".claude/settings.json");
        fs::write(&settings, r#"{"theme":"dark","env":{"FOO":"bar"}}"#).unwrap();

        let mgr = CliTakeoverManager::with_home_dir(tmp_home.clone(), "http://127.0.0.1:8787");
        let st1 = mgr.takeover(CliTool::ClaudeCode).unwrap();
        assert!(st1.taken_over);
        assert!(st1.has_backup);

        let after: Value = serde_json::from_str(&fs::read_to_string(&settings).unwrap()).unwrap();
        assert_eq!(after["theme"], "dark");
        assert_eq!(after["env"]["FOO"], "bar");
        assert_eq!(after["env"]["ANTHROPIC_BASE_URL"], "http://127.0.0.1:8787");

        let st2 = mgr.restore(CliTool::ClaudeCode).unwrap();
        assert!(!st2.taken_over);
        assert!(!st2.has_backup);

        let _ = fs::remove_dir_all(&tmp_home);
    }
}
