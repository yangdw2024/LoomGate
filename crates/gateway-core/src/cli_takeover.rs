use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum TakeoverError {
    #[error("could not resolve user home directory")]
    NoHomeDir,
    #[error("unknown client profile '{0}'")]
    UnknownProfile(String),
    #[error("client profile '{0}' is a setup wizard and does not support atomic file takeover")]
    NotAtomicTakeover(String),
    #[error("filesystem error: {0}")]
    Io(#[from] std::io::Error),
    #[error("json parse error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("toml parse error: {0}")]
    Toml(#[from] toml::de::Error),
}

/// Legacy enum retained for backward compatibility with existing callers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CliTool {
    ClaudeCode,
    Codex,
    GeminiCli,
}

impl CliTool {
    pub fn as_profile_id(self) -> &'static str {
        match self {
            CliTool::ClaudeCode => "claude_code",
            CliTool::Codex => "codex",
            CliTool::GeminiCli => "gemini_cli",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IntegrationKind {
    AtomicTakeover,
    SetupWizard,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConfigFormat {
    Json,
    Toml,
}

/// Declarative spec for atomic config takeover (supports nested JSON dot-paths or TOML keys).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AtomicTakeoverSpec {
    /// Path relative to user home directory (e.g. ".claude/settings.json").
    pub relative_config_path: String,
    pub format: ConfigFormat,
    /// Key-value patches applied atomically. Supports `{{GATEWAY_URL}}` and `{{API_KEY}}`.
    /// For JSON format, dot-separated paths (e.g. "env.ANTHROPIC_BASE_URL") create parent objects.
    pub set_fields: BTreeMap<String, String>,
    /// Optional default fields inserted ONLY if the key does not already exist.
    #[serde(default)]
    pub default_fields: BTreeMap<String, String>,
}

/// Declarative spec for setup-wizard clients (Cursor, Cline/Roo, Cherry Studio, Trae, SDKs, etc.).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SetupWizardSpec {
    pub protocol: String,
    pub endpoint_template: String,
    pub env_snippet_template: String,
    pub config_snippet_template: String,
    pub instructions: Vec<String>,
}

/// Declarative Client Profile loaded either from built-ins or external JSON/TOML files.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClientProfile {
    pub id: String,
    pub display_name: String,
    pub category: String,
    pub kind: IntegrationKind,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub custom: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub atomic: Option<AtomicTakeoverSpec>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wizard: Option<SetupWizardSpec>,
}

#[derive(Debug, Clone, Deserialize)]
struct CustomProfilesFile {
    #[serde(default)]
    profiles: Vec<ClientProfile>,
}

/// Rendered status & copy-ready setup snippets for any Client Profile in the registry.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClientProfileStatus {
    pub id: String,
    pub display_name: String,
    pub category: String,
    pub kind: IntegrationKind,
    pub description: String,
    pub custom: bool,
    pub config_path: Option<String>,
    pub installed: bool,
    pub taken_over: bool,
    pub has_backup: bool,
    pub active_endpoint: Option<String>,
    pub wizard_endpoint: Option<String>,
    pub wizard_protocol: Option<String>,
    pub env_snippet: Option<String>,
    pub config_snippet: Option<String>,
    pub instructions: Vec<String>,
}

/// Legacy status struct kept for backward compatibility with existing Tauri/CLI callers.
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

fn builtin_profiles() -> Vec<ClientProfile> {
    let mut claude_set = BTreeMap::new();
    claude_set.insert("env.ANTHROPIC_BASE_URL".into(), "{{GATEWAY_URL}}".into());
    let mut claude_defaults = BTreeMap::new();
    claude_defaults.insert("env.ANTHROPIC_AUTH_TOKEN".into(), "{{API_KEY}}".into());

    let mut codex_set = BTreeMap::new();
    codex_set.insert("openai_base_url".into(), "{{GATEWAY_URL}}/v1".into());

    let mut gemini_set = BTreeMap::new();
    gemini_set.insert("env.GEMINI_API_BASE_URL".into(), "{{GATEWAY_URL}}".into());

    vec![
        // --- Atomic Takeover Profiles ---
        ClientProfile {
            id: "claude_code".into(),
            display_name: "Claude Code".into(),
            category: "CLI Agent".into(),
            kind: IntegrationKind::AtomicTakeover,
            description: "Anthropic Claude Code CLI (~/.claude/settings.json atomic takeover + .bak rollback).".into(),
            custom: false,
            atomic: Some(AtomicTakeoverSpec {
                relative_config_path: ".claude/settings.json".into(),
                format: ConfigFormat::Json,
                set_fields: claude_set,
                default_fields: claude_defaults,
            }),
            wizard: None,
        },
        ClientProfile {
            id: "codex".into(),
            display_name: "Codex CLI".into(),
            category: "CLI Agent".into(),
            kind: IntegrationKind::AtomicTakeover,
            description: "OpenAI Codex CLI (~/.codex/config.toml atomic takeover + .bak rollback).".into(),
            custom: false,
            atomic: Some(AtomicTakeoverSpec {
                relative_config_path: ".codex/config.toml".into(),
                format: ConfigFormat::Toml,
                set_fields: codex_set,
                default_fields: BTreeMap::new(),
            }),
            wizard: None,
        },
        ClientProfile {
            id: "gemini_cli".into(),
            display_name: "Gemini CLI".into(),
            category: "CLI Agent".into(),
            kind: IntegrationKind::AtomicTakeover,
            description: "Google Gemini CLI (~/.gemini/settings.json atomic takeover + .bak rollback).".into(),
            custom: false,
            atomic: Some(AtomicTakeoverSpec {
                relative_config_path: ".gemini/settings.json".into(),
                format: ConfigFormat::Json,
                set_fields: gemini_set,
                default_fields: BTreeMap::new(),
            }),
            wizard: None,
        },
        // --- Setup Wizard Profiles ---
        ClientProfile {
            id: "cursor".into(),
            display_name: "Cursor".into(),
            category: "AI IDE".into(),
            kind: IntegrationKind::SetupWizard,
            description: "Route Cursor OpenAI-compatible requests through LoomGate P2C pool & trajectory recorder.".into(),
            custom: false,
            atomic: None,
            wizard: Some(SetupWizardSpec {
                protocol: "openai".into(),
                endpoint_template: "{{GATEWAY_URL}}/v1".into(),
                env_snippet_template: "OPENAI_BASE_URL={{GATEWAY_URL}}/v1\nOPENAI_API_KEY={{API_KEY}}".into(),
                config_snippet_template: "{\n  \"openaiBaseUrl\": \"{{GATEWAY_URL}}/v1\",\n  \"openaiApiKey\": \"{{API_KEY}}\"\n}".into(),
                instructions: vec![
                    "Open Cursor Settings -> Models -> OpenAI API Key.".into(),
                    "Enable 'Override OpenAI Base URL' and paste {{GATEWAY_URL}}/v1.".into(),
                    "Paste {{API_KEY}} as the API Key and enable your target models.".into(),
                ],
            }),
        },
        ClientProfile {
            id: "cline_roo".into(),
            display_name: "Cline / Roo Code".into(),
            category: "VS Code Extension".into(),
            kind: IntegrationKind::SetupWizard,
            description: "Connect Cline or Roo Code via Anthropic or OpenAI-Compatible provider with Prompt Caching preserved.".into(),
            custom: false,
            atomic: None,
            wizard: Some(SetupWizardSpec {
                protocol: "anthropic / openai".into(),
                endpoint_template: "{{GATEWAY_URL}}".into(),
                env_snippet_template: "ANTHROPIC_BASE_URL={{GATEWAY_URL}}\nANTHROPIC_API_KEY={{API_KEY}}\n# Or OpenAI Compatible:\nOPENAI_BASE_URL={{GATEWAY_URL}}/v1".into(),
                config_snippet_template: "{\n  \"apiProvider\": \"anthropic\",\n  \"anthropicBaseUrl\": \"{{GATEWAY_URL}}\",\n  \"apiKey\": \"{{API_KEY}}\"\n}".into(),
                instructions: vec![
                    "Open Cline / Roo Code Settings -> API Provider.".into(),
                    "Choose 'Anthropic' (Base URL: {{GATEWAY_URL}}) or 'OpenAI Compatible' (Base URL: {{GATEWAY_URL}}/v1).".into(),
                    "Set API Key to {{API_KEY}} (LoomGate preserves cache_control & thinking signatures).".into(),
                ],
            }),
        },
        ClientProfile {
            id: "cherry_studio".into(),
            display_name: "Cherry Studio".into(),
            category: "Desktop Chat".into(),
            kind: IntegrationKind::SetupWizard,
            description: "Add LoomGate as a unified provider in Cherry Studio supporting Anthropic, OpenAI & Gemini models.".into(),
            custom: false,
            atomic: None,
            wizard: Some(SetupWizardSpec {
                protocol: "openai / anthropic / gemini".into(),
                endpoint_template: "{{GATEWAY_URL}}/v1".into(),
                env_snippet_template: "API_HOST={{GATEWAY_URL}}/v1\nAPI_KEY={{API_KEY}}".into(),
                config_snippet_template: "{\n  \"provider\": \"LoomGate\",\n  \"type\": \"openai\",\n  \"apiHost\": \"{{GATEWAY_URL}}/v1\",\n  \"apiKey\": \"{{API_KEY}}\"\n}".into(),
                instructions: vec![
                    "Open Cherry Studio -> Settings -> Model Providers -> Add Custom Provider.".into(),
                    "Set Provider Name = LoomGate, API Host = {{GATEWAY_URL}}/v1 (or {{GATEWAY_URL}} for Anthropic/Gemini).".into(),
                    "Set API Key = {{API_KEY}} and add any model configured in your LoomGate channel pool.".into(),
                ],
            }),
        },
        ClientProfile {
            id: "trae".into(),
            display_name: "Trae".into(),
            category: "AI IDE".into(),
            kind: IntegrationKind::SetupWizard,
            description: "Configure Trae IDE custom model endpoint pointing to LoomGate.".into(),
            custom: false,
            atomic: None,
            wizard: Some(SetupWizardSpec {
                protocol: "openai".into(),
                endpoint_template: "{{GATEWAY_URL}}/v1".into(),
                env_snippet_template: "OPENAI_BASE_URL={{GATEWAY_URL}}/v1\nOPENAI_API_KEY={{API_KEY}}".into(),
                config_snippet_template: "{\n  \"provider\": \"OpenAI Compatible\",\n  \"baseUrl\": \"{{GATEWAY_URL}}/v1\",\n  \"apiKey\": \"{{API_KEY}}\"\n}".into(),
                instructions: vec![
                    "Open Trae -> Settings -> Models -> Add Model -> Custom / OpenAI Compatible.".into(),
                    "Set Base URL to {{GATEWAY_URL}}/v1 and API Key to {{API_KEY}}.".into(),
                    "Pick any model ID hosted in LoomGate (cross-protocol routing works transparently).".into(),
                ],
            }),
        },
        ClientProfile {
            id: "deepseek_harness".into(),
            display_name: "DeepSeek Harness".into(),
            category: "AI Coding Agent".into(),
            kind: IntegrationKind::SetupWizard,
            description: "Route DeepSeek Harness sessions through LoomGate with Anthropic /v1/messages and OpenAI /v1/responses support.".into(),
            custom: false,
            atomic: None,
            wizard: Some(SetupWizardSpec {
                protocol: "anthropic / openai / responses".into(),
                endpoint_template: "{{GATEWAY_URL}}/v1".into(),
                env_snippet_template: "export ANTHROPIC_BASE_URL=\"{{GATEWAY_URL}}\"\nexport ANTHROPIC_API_KEY=\"{{API_KEY}}\"\nexport OPENAI_BASE_URL=\"{{GATEWAY_URL}}/v1\"\nexport OPENAI_API_KEY=\"{{API_KEY}}\"".into(),
                config_snippet_template: "{\n  \"provider\": \"loomgate\",\n  \"apiKey\": \"{{API_KEY}}\",\n  \"baseUrl\": \"{{GATEWAY_URL}}/v1\",\n  \"endpoints\": {\n    \"anthropicMessages\": \"{{GATEWAY_URL}}/v1/messages\",\n    \"openaiResponses\": \"{{GATEWAY_URL}}/v1/responses\"\n  }\n}".into(),
                instructions: vec![
                    "Open DeepSeek Harness Settings -> Providers -> Add Custom Provider (LoomGate).".into(),
                    "Set Base URL to {{GATEWAY_URL}}/v1 (supports /v1/responses and /v1/chat/completions) or {{GATEWAY_URL}} for Anthropic /v1/messages.".into(),
                    "Set API Key to {{API_KEY}} to enable sticky prefix-cache routing and local trajectory capture.".into(),
                ],
            }),
        },
        ClientProfile {
            id: "opencode".into(),
            display_name: "OpenCode".into(),
            category: "CLI Agent".into(),
            kind: IntegrationKind::SetupWizard,
            description: "Connect OpenCode CLI via ~/.config/opencode/opencode.json or environment variables.".into(),
            custom: false,
            atomic: None,
            wizard: Some(SetupWizardSpec {
                protocol: "openai / anthropic".into(),
                endpoint_template: "{{GATEWAY_URL}}/v1".into(),
                env_snippet_template: "export OPENAI_BASE_URL=\"{{GATEWAY_URL}}/v1\"\nexport OPENAI_API_KEY=\"{{API_KEY}}\"\nexport ANTHROPIC_BASE_URL=\"{{GATEWAY_URL}}\"\nexport ANTHROPIC_API_KEY=\"{{API_KEY}}\"".into(),
                config_snippet_template: "{\n  \"provider\": {\n    \"loomgate\": {\n      \"npm\": \"@ai-sdk/openai-compatible\",\n      \"name\": \"LoomGate\",\n      \"options\": {\n        \"baseURL\": \"{{GATEWAY_URL}}/v1\",\n        \"apiKey\": \"{{API_KEY}}\"\n      }\n    }\n  }\n}".into(),
                instructions: vec![
                    "Create or edit ~/.config/opencode/opencode.json and add the LoomGate provider block.".into(),
                    "Set options.baseURL to {{GATEWAY_URL}}/v1 and options.apiKey to {{API_KEY}} (or export the env vars).".into(),
                    "Select a model under the loomgate provider in OpenCode to route through LoomGate.".into(),
                ],
            }),
        },
        ClientProfile {
            id: "claude_desktop".into(),
            display_name: "Claude Desktop".into(),
            category: "Desktop App".into(),
            kind: IntegrationKind::SetupWizard,
            description: "Route Claude Desktop Anthropic API integrations and MCP server workflows through LoomGate.".into(),
            custom: false,
            atomic: None,
            wizard: Some(SetupWizardSpec {
                protocol: "anthropic".into(),
                endpoint_template: "{{GATEWAY_URL}}".into(),
                env_snippet_template: "export ANTHROPIC_BASE_URL=\"{{GATEWAY_URL}}\"\nexport ANTHROPIC_API_KEY=\"{{API_KEY}}\"".into(),
                config_snippet_template: "{\n  \"anthropicBaseUrl\": \"{{GATEWAY_URL}}\",\n  \"anthropicApiKey\": \"{{API_KEY}}\",\n  \"mcpServers\": {\n    \"loomgate-env\": {\n      \"env\": {\n        \"ANTHROPIC_BASE_URL\": \"{{GATEWAY_URL}}\",\n        \"ANTHROPIC_API_KEY\": \"{{API_KEY}}\"\n      }\n    }\n  }\n}".into(),
                instructions: vec![
                    "Open Claude Desktop Settings -> Developer -> Edit Config (claude_desktop_config.json).".into(),
                    "Point Anthropic API or MCP server env blocks to ANTHROPIC_BASE_URL={{GATEWAY_URL}} with ANTHROPIC_API_KEY={{API_KEY}}.".into(),
                    "Restart Claude Desktop to route Anthropic requests through LoomGate with cache_control preserved.".into(),
                ],
            }),
        },
        ClientProfile {
            id: "aider".into(),
            display_name: "Aider".into(),
            category: "CLI Agent".into(),
            kind: IntegrationKind::SetupWizard,
            description: "Configure Aider CLI via ~/.aider.conf.yml or OPENAI_API_BASE / ANTHROPIC_BASE_URL.".into(),
            custom: false,
            atomic: None,
            wizard: Some(SetupWizardSpec {
                protocol: "openai / anthropic".into(),
                endpoint_template: "{{GATEWAY_URL}}/v1".into(),
                env_snippet_template: "export OPENAI_API_BASE=\"{{GATEWAY_URL}}/v1\"\nexport OPENAI_API_KEY=\"{{API_KEY}}\"\nexport ANTHROPIC_BASE_URL=\"{{GATEWAY_URL}}\"\nexport ANTHROPIC_API_KEY=\"{{API_KEY}}\"".into(),
                config_snippet_template: "# ~/.aider.conf.yml\nopenai-api-base: {{GATEWAY_URL}}/v1\nopenai-api-key: {{API_KEY}}\nanthropic-api-key: {{API_KEY}}\n# Set ANTHROPIC_BASE_URL={{GATEWAY_URL}} in your shell when using Claude models".into(),
                instructions: vec![
                    "Add openai-api-base: {{GATEWAY_URL}}/v1 and api keys to ~/.aider.conf.yml.".into(),
                    "Or export OPENAI_API_BASE={{GATEWAY_URL}}/v1 and ANTHROPIC_BASE_URL={{GATEWAY_URL}} with key {{API_KEY}}.".into(),
                    "Launch aider with --model openai/<model> or --model anthropic/<model> to use LoomGate sticky routing.".into(),
                ],
            }),
        },
        ClientProfile {
            id: "generic_sdk".into(),
            display_name: "Generic OpenAI / Anthropic / Gemini SDK".into(),
            category: "SDK / Script".into(),
            kind: IntegrationKind::SetupWizard,
            description: "Drop-in environment variables for Python, TypeScript, Go, or Rust LLM SDKs.".into(),
            custom: false,
            atomic: None,
            wizard: Some(SetupWizardSpec {
                protocol: "anthropic / openai / gemini".into(),
                endpoint_template: "{{GATEWAY_URL}}".into(),
                env_snippet_template: "export ANTHROPIC_BASE_URL=\"{{GATEWAY_URL}}\"\nexport ANTHROPIC_API_KEY=\"{{API_KEY}}\"\nexport OPENAI_BASE_URL=\"{{GATEWAY_URL}}/v1\"\nexport OPENAI_API_KEY=\"{{API_KEY}}\"\nexport GEMINI_API_BASE_URL=\"{{GATEWAY_URL}}\"\nexport GEMINI_API_KEY=\"{{API_KEY}}\"".into(),
                config_snippet_template: "{\n  \"anthropic\": \"{{GATEWAY_URL}}/v1/messages\",\n  \"openai\": \"{{GATEWAY_URL}}/v1/chat/completions\",\n  \"gemini\": \"{{GATEWAY_URL}}/v1beta/models/<model>:generateContent\",\n  \"apiKey\": \"{{API_KEY}}\"\n}".into(),
                instructions: vec![
                    "Export the environment variables for your SDK (Anthropic, OpenAI, or Gemini).".into(),
                    "Optionally pass X-Session-Id for sticky prefix routing and X-Gateway-Route-Mode: lock|stage_aware.".into(),
                    "Inspect every turn and fork/diff historical steps locally via LoomGate Studio.".into(),
                ],
            }),
        },
    ]
}

/// Manages Declarative Client Profiles: both atomic config takeover with `.bak` rollback
/// and setup-wizard configuration generation, extensible via local JSON/TOML profile files.
pub struct CliTakeoverManager {
    home_dir: PathBuf,
    gateway_url: String,
    extra_profile_paths: Vec<PathBuf>,
}

impl CliTakeoverManager {
    pub fn new(gateway_url: impl Into<String>) -> Result<Self, TakeoverError> {
        let home_dir = dirs::home_dir().ok_or(TakeoverError::NoHomeDir)?;
        let mut extra = Vec::new();
        if let Ok(p) = std::env::var("LOOMGATE_CLIENT_PROFILES") {
            if !p.trim().is_empty() {
                extra.push(PathBuf::from(p));
            }
        }
        Ok(Self {
            home_dir,
            gateway_url: gateway_url.into().trim_end_matches('/').to_string(),
            extra_profile_paths: extra,
        })
    }

    pub fn with_home_dir(home_dir: PathBuf, gateway_url: impl Into<String>) -> Self {
        Self {
            home_dir,
            gateway_url: gateway_url.into().trim_end_matches('/').to_string(),
            extra_profile_paths: Vec::new(),
        }
    }

    pub fn with_custom_profile_path(mut self, path: PathBuf) -> Self {
        self.extra_profile_paths.push(path);
        self
    }

    /// Loads built-in profiles merged with any user-defined profiles from:
    /// - `<home>/.loomgate/client_profiles.json`
    /// - `<home>/.loomgate/client_profiles.toml`
    /// - any explicit path passed via `LOOMGATE_CLIENT_PROFILES` or `with_custom_profile_path`
    pub fn load_registry(&self) -> Vec<ClientProfile> {
        let mut profiles = builtin_profiles();
        let mut candidate_paths = vec![
            self.home_dir.join(".loomgate").join("client_profiles.json"),
            self.home_dir.join(".loomgate").join("client_profiles.toml"),
        ];
        candidate_paths.extend(self.extra_profile_paths.iter().cloned());

        for path in candidate_paths {
            if let Ok(loaded) = Self::load_profiles_from_file(&path) {
                for mut prof in loaded {
                    prof.custom = true;
                    if let Some(existing) = profiles.iter_mut().find(|p| p.id == prof.id) {
                        *existing = prof;
                    } else {
                        profiles.push(prof);
                    }
                }
            }
        }
        profiles
    }

    pub fn load_profiles_from_file(path: &Path) -> Result<Vec<ClientProfile>, TakeoverError> {
        let raw = fs::read_to_string(path)?;
        let ext = path
            .extension()
            .and_then(|s| s.to_str())
            .unwrap_or("")
            .to_lowercase();
        if ext == "toml" {
            let parsed: CustomProfilesFile = toml::from_str(&raw)?;
            Ok(parsed.profiles)
        } else {
            // Support either a top-level array `[...]` or `{ "profiles": [...] }`
            if raw.trim_start().starts_with('[') {
                Ok(serde_json::from_str(&raw)?)
            } else {
                let parsed: CustomProfilesFile = serde_json::from_str(&raw)?;
                Ok(parsed.profiles)
            }
        }
    }

    fn resolve_template(&self, template: &str, api_key: Option<&str>) -> String {
        let key = api_key
            .filter(|s| !s.trim().is_empty())
            .unwrap_or("local-loomgate");
        template
            .replace("{{GATEWAY_URL}}", &self.gateway_url)
            .replace("{{API_KEY}}", key)
    }

    fn resolve_profile_path(&self, rel: &str) -> PathBuf {
        let mut p = self.home_dir.clone();
        for seg in rel.split(['/', '\\']) {
            if !seg.is_empty() {
                p.push(seg);
            }
        }
        p
    }

    fn backup_path(target: &Path) -> PathBuf {
        let mut os = target.as_os_str().to_os_string();
        os.push(".edge-gateway.bak");
        PathBuf::from(os)
    }

    pub fn list_profiles(&self, api_key: Option<&str>) -> Vec<ClientProfileStatus> {
        self.load_registry()
            .into_iter()
            .map(|p| self.inspect_profile(&p, api_key))
            .collect()
    }

    fn inspect_profile(&self, profile: &ClientProfile, api_key: Option<&str>) -> ClientProfileStatus {
        match profile.kind {
            IntegrationKind::AtomicTakeover => {
                if let Some(ref spec) = profile.atomic {
                    let path = self.resolve_profile_path(&spec.relative_config_path);
                    let backup = Self::backup_path(&path);
                    let installed =
                        path.exists() || path.parent().map(|p| p.exists()).unwrap_or(false);
                    let raw = fs::read_to_string(&path).unwrap_or_default();
                    let taken_over = raw.contains(&self.gateway_url);

                    ClientProfileStatus {
                        id: profile.id.clone(),
                        display_name: profile.display_name.clone(),
                        category: profile.category.clone(),
                        kind: profile.kind,
                        description: profile.description.clone(),
                        custom: profile.custom,
                        config_path: Some(path.display().to_string()),
                        installed,
                        taken_over,
                        has_backup: backup.exists(),
                        active_endpoint: if taken_over {
                            Some(self.gateway_url.clone())
                        } else {
                            None
                        },
                        wizard_endpoint: None,
                        wizard_protocol: None,
                        env_snippet: None,
                        config_snippet: None,
                        instructions: vec![],
                    }
                } else {
                    ClientProfileStatus {
                        id: profile.id.clone(),
                        display_name: profile.display_name.clone(),
                        category: profile.category.clone(),
                        kind: profile.kind,
                        description: profile.description.clone(),
                        custom: profile.custom,
                        config_path: None,
                        installed: false,
                        taken_over: false,
                        has_backup: false,
                        active_endpoint: None,
                        wizard_endpoint: None,
                        wizard_protocol: None,
                        env_snippet: None,
                        config_snippet: None,
                        instructions: vec![],
                    }
                }
            }
            IntegrationKind::SetupWizard => {
                let wiz = profile.wizard.as_ref();
                ClientProfileStatus {
                    id: profile.id.clone(),
                    display_name: profile.display_name.clone(),
                    category: profile.category.clone(),
                    kind: profile.kind,
                    description: profile.description.clone(),
                    custom: profile.custom,
                    config_path: None,
                    installed: true,
                    taken_over: false,
                    has_backup: false,
                    active_endpoint: None,
                    wizard_endpoint: wiz.map(|w| self.resolve_template(&w.endpoint_template, api_key)),
                    wizard_protocol: wiz.map(|w| w.protocol.clone()),
                    env_snippet: wiz.map(|w| self.resolve_template(&w.env_snippet_template, api_key)),
                    config_snippet: wiz
                        .map(|w| self.resolve_template(&w.config_snippet_template, api_key)),
                    instructions: wiz
                        .map(|w| {
                            w.instructions
                                .iter()
                                .map(|s| self.resolve_template(s, api_key))
                                .collect()
                        })
                        .unwrap_or_default(),
                }
            }
        }
    }

    pub fn takeover_by_id(
        &self,
        profile_id: &str,
        api_key: Option<&str>,
    ) -> Result<ClientProfileStatus, TakeoverError> {
        let profiles = self.load_registry();
        let profile = profiles
            .iter()
            .find(|p| p.id == profile_id)
            .ok_or_else(|| TakeoverError::UnknownProfile(profile_id.to_string()))?;

        let spec = profile
            .atomic
            .as_ref()
            .filter(|_| profile.kind == IntegrationKind::AtomicTakeover)
            .ok_or_else(|| TakeoverError::NotAtomicTakeover(profile_id.to_string()))?;

        let path = self.resolve_profile_path(&spec.relative_config_path);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }

        let backup = Self::backup_path(&path);
        if path.exists() && !backup.exists() {
            fs::copy(&path, &backup)?;
        }

        match spec.format {
            ConfigFormat::Json => {
                let mut doc: Value = if path.exists() {
                    serde_json::from_str(&fs::read_to_string(&path)?)?
                } else {
                    json!({})
                };
                if !doc.is_object() {
                    doc = json!({});
                }
                for (dot_path, tmpl) in &spec.set_fields {
                    let val = self.resolve_template(tmpl, api_key);
                    set_json_dot_path(&mut doc, dot_path, json!(val), true);
                }
                for (dot_path, tmpl) in &spec.default_fields {
                    let val = self.resolve_template(tmpl, api_key);
                    set_json_dot_path(&mut doc, dot_path, json!(val), false);
                }
                atomic_write(&path, serde_json::to_string_pretty(&doc)?.as_bytes())?;
            }
            ConfigFormat::Toml => {
                let existing = fs::read_to_string(&path).unwrap_or_default();
                let mut table: toml::Table = if existing.trim().is_empty() {
                    toml::Table::new()
                } else {
                    existing.parse().unwrap_or_else(|_| toml::Table::new())
                };
                for (k, tmpl) in &spec.set_fields {
                    let val = self.resolve_template(tmpl, api_key);
                    table.insert(k.clone(), toml::Value::String(val));
                }
                for (k, tmpl) in &spec.default_fields {
                    if !table.contains_key(k) {
                        let val = self.resolve_template(tmpl, api_key);
                        table.insert(k.clone(), toml::Value::String(val));
                    }
                }
                let serialized = toml::to_string_pretty(&table).unwrap_or_default();
                atomic_write(&path, serialized.as_bytes())?;
            }
        }

        Ok(self.inspect_profile(profile, api_key))
    }

    pub fn restore_by_id(
        &self,
        profile_id: &str,
        api_key: Option<&str>,
    ) -> Result<ClientProfileStatus, TakeoverError> {
        let profiles = self.load_registry();
        let profile = profiles
            .iter()
            .find(|p| p.id == profile_id)
            .ok_or_else(|| TakeoverError::UnknownProfile(profile_id.to_string()))?;

        let spec = profile
            .atomic
            .as_ref()
            .filter(|_| profile.kind == IntegrationKind::AtomicTakeover)
            .ok_or_else(|| TakeoverError::NotAtomicTakeover(profile_id.to_string()))?;

        let path = self.resolve_profile_path(&spec.relative_config_path);
        let backup = Self::backup_path(&path);
        if backup.exists() {
            let bytes = fs::read(&backup)?;
            atomic_write(&path, &bytes)?;
            let _ = fs::remove_file(&backup);
        }
        Ok(self.inspect_profile(profile, api_key))
    }

    // --- Backward-compatible legacy helpers ---

    pub fn config_path(&self, tool: CliTool) -> PathBuf {
        match tool {
            CliTool::ClaudeCode => self.home_dir.join(".claude").join("settings.json"),
            CliTool::Codex => self.home_dir.join(".codex").join("config.toml"),
            CliTool::GeminiCli => self.home_dir.join(".gemini").join("settings.json"),
        }
    }

    pub fn list_statuses(&self) -> Vec<CliIntegrationStatus> {
        [CliTool::ClaudeCode, CliTool::Codex, CliTool::GeminiCli]
            .into_iter()
            .map(|tool| self.inspect(tool))
            .collect()
    }

    pub fn inspect(&self, tool: CliTool) -> CliIntegrationStatus {
        let profiles = self.load_registry();
        let st = profiles
            .iter()
            .find(|p| p.id == tool.as_profile_id())
            .map(|p| self.inspect_profile(p, None));
        let fallback_path = self.config_path(tool).display().to_string();
        match st {
            Some(s) => CliIntegrationStatus {
                tool,
                display_name: s.display_name,
                config_path: s.config_path.unwrap_or(fallback_path),
                installed: s.installed,
                taken_over: s.taken_over,
                has_backup: s.has_backup,
                active_endpoint: s.active_endpoint,
            },
            None => CliIntegrationStatus {
                tool,
                display_name: format!("{:?}", tool),
                config_path: fallback_path,
                installed: false,
                taken_over: false,
                has_backup: false,
                active_endpoint: None,
            },
        }
    }

    pub fn takeover(&self, tool: CliTool) -> Result<CliIntegrationStatus, TakeoverError> {
        self.takeover_by_id(tool.as_profile_id(), None)?;
        Ok(self.inspect(tool))
    }

    pub fn restore(&self, tool: CliTool) -> Result<CliIntegrationStatus, TakeoverError> {
        self.restore_by_id(tool.as_profile_id(), None)?;
        Ok(self.inspect(tool))
    }
}

fn set_json_dot_path(doc: &mut Value, dot_path: &str, value: Value, overwrite: bool) {
    let segments: Vec<&str> = dot_path.split('.').filter(|s| !s.is_empty()).collect();
    if segments.is_empty() {
        return;
    }
    let mut cursor = doc;
    for seg in &segments[..segments.len() - 1] {
        if !cursor.is_object() {
            *cursor = Value::Object(Map::new());
        }
        let map = cursor.as_object_mut().unwrap();
        cursor = map
            .entry((*seg).to_string())
            .or_insert_with(|| Value::Object(Map::new()));
    }
    if !cursor.is_object() {
        *cursor = Value::Object(Map::new());
    }
    let last = segments[segments.len() - 1];
    let map = cursor.as_object_mut().unwrap();
    if overwrite || !map.contains_key(last) {
        map.insert(last.to_string(), value);
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

    #[test]
    fn declarative_registry_includes_wizards_and_loads_custom_toml_profile() {
        let tmp_home = std::env::temp_dir().join(format!("lg_registry_{}", std::process::id()));
        let _ = fs::remove_dir_all(&tmp_home);
        fs::create_dir_all(tmp_home.join(".loomgate")).unwrap();

        let custom_toml = r#"
[[profiles]]
id = "aider_custom"
display_name = "Aider (Custom TOML)"
category = "CLI Agent"
kind = "atomic_takeover"
description = "User-defined custom profile loaded without Rust changes"

[profiles.atomic]
relative_config_path = ".aider/config.json"
format = "json"

[profiles.atomic.set_fields]
"openai.api_base" = "{{GATEWAY_URL}}/v1"
"#;
        fs::write(tmp_home.join(".loomgate/client_profiles.toml"), custom_toml).unwrap();

        let mgr = CliTakeoverManager::with_home_dir(tmp_home.clone(), "http://127.0.0.1:8787");
        let list = mgr.list_profiles(Some("lg_secret_123"));
        // 12 built-ins (3 atomic + 9 wizards) + 1 custom = 13
        assert_eq!(list.len(), 13);
        let cursor = list.iter().find(|p| p.id == "cursor").unwrap();
        assert_eq!(cursor.kind, IntegrationKind::SetupWizard);
        assert!(cursor
            .env_snippet
            .as_deref()
            .unwrap()
            .contains("lg_secret_123"));

        for (id, expected_endpoint) in [
            ("deepseek_harness", "http://127.0.0.1:8787/v1"),
            ("opencode", "http://127.0.0.1:8787/v1"),
            ("claude_desktop", "http://127.0.0.1:8787"),
            ("aider", "http://127.0.0.1:8787/v1"),
        ] {
            let prof = list
                .iter()
                .find(|p| p.id == id)
                .unwrap_or_else(|| panic!("missing built-in profile {id}"));
            assert_eq!(prof.kind, IntegrationKind::SetupWizard);
            assert!(!prof.custom);
            assert_eq!(prof.wizard_endpoint.as_deref(), Some(expected_endpoint));
            let env_snippet = prof.env_snippet.as_deref().unwrap();
            let config_snippet = prof.config_snippet.as_deref().unwrap();
            assert!(env_snippet.contains("http://127.0.0.1:8787"));
            assert!(env_snippet.contains("lg_secret_123"));
            assert!(!env_snippet.contains("{{GATEWAY_URL}}"));
            assert!(!env_snippet.contains("{{API_KEY}}"));
            assert!(config_snippet.contains("http://127.0.0.1:8787"));
            assert!(config_snippet.contains("lg_secret_123"));
            assert!(!config_snippet.contains("{{GATEWAY_URL}}"));
            assert!(!config_snippet.contains("{{API_KEY}}"));
        }

        let custom = mgr
            .takeover_by_id("aider_custom", Some("lg_secret_123"))
            .unwrap();
        assert!(custom.custom);
        assert!(custom.taken_over);
        let written: Value =
            serde_json::from_str(&fs::read_to_string(tmp_home.join(".aider/config.json")).unwrap())
                .unwrap();
        assert_eq!(written["openai"]["api_base"], "http://127.0.0.1:8787/v1");

        let _ = fs::remove_dir_all(&tmp_home);
    }
}
