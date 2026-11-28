use crate::cloud_sync::{CloudSessionStatus, CloudSyncClient};
use crate::codec::{codec_for, CodecError};
use crate::config::{ChannelConfig, ConfigError, GatewayConfig, RouteMode, UpstreamProtocol};
use crate::error_gov::{classify_upstream_error, ErrorTier};
use crate::inspector::{LiveTrafficInspector, TrafficInspectEntry};
use crate::ir::{CanonicalRequest, CanonicalStreamEvent, CanonicalUsage, ContentPart};
use crate::local_discovery::{build_local_channel_config, DiscoveredLocalProvider};
use crate::middleware::guardrail::GuardrailMiddleware;
use crate::middleware::ingress::IngressRateLimitMiddleware;
use crate::middleware::mcp_bridge::McpBridgeMiddleware;
use crate::middleware::memory::ContextMemoryMiddleware;
use crate::middleware::{MiddlewareContext, MiddlewarePipeline};
use crate::router::{ChannelRuntime, ChannelSnapshot, StickyP2cRouter};
use crate::signature_cache::SignatureCache;
use crate::sse_fsm::{CancelPropagatingStream, SseFrameParser, StreamMetrics};
use crate::trajectory::{
    new_step_id, now_epoch_ms, render_fork_diff_markdown, ForkDiffExport, ForkDiffResult,
    SavingsSummary, SessionSummary, TrajectoryError, TrajectoryStep, TrajectoryStore,
};
use bytes::Bytes;
use futures_util::StreamExt;
use reqwest::{Client, Proxy};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};
use thiserror::Error;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tracing::{error, info, warn};
use uuid::Uuid;

#[derive(Debug, Error)]
pub enum GatewayError {
    #[error("codec error: {0}")]
    Codec(#[from] CodecError),
    #[error("trajectory store error: {0}")]
    Trajectory(#[from] TrajectoryError),
    #[error("unauthorized: invalid or missing local gateway token")]
    Unauthorized,
    #[error("no healthy upstream channel available for model '{0}'")]
    NoAvailableChannel(String),
    #[error("upstream returned HTTP {status}: {body}")]
    UpstreamError {
        status: u16,
        body: String,
        tier: ErrorTier,
    },
    #[error("middleware '{middleware}' rejected request (HTTP {status}): {message}")]
    MiddlewareRejected {
        status: u16,
        error_type: String,
        message: String,
        middleware: &'static str,
    },
    #[error("config error: {0}")]
    Config(#[from] ConfigError),
    #[error("transport error: {0}")]
    Transport(String),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuthSettings {
    pub api_key: Option<String>,
    pub admin_password: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChannelTestResult {
    pub ok: bool,
    pub channel_id: String,
    pub status_code: Option<u16>,
    pub latency_ms: u64,
    pub endpoint: String,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub quota_info: Option<Value>,
}

/// Transparent routing & observability metadata attached as HTTP response headers.
#[derive(Debug, Clone)]
pub struct ResponseMetadata {
    pub request_id: String,
    pub step_id: String,
    pub actual_model: String,
    pub channel_id: String,
    pub route_mode: RouteMode,
    pub sticky_hit: bool,
    pub prefix_key: String,
    pub extra_headers: BTreeMap<String, String>,
}

impl ResponseMetadata {
    pub fn to_headers(&self) -> HashMap<String, String> {
        let mut h = HashMap::new();
        h.insert("X-Gateway-Request-Id".into(), self.request_id.clone());
        h.insert("X-Gateway-Step-Id".into(), self.step_id.clone());
        h.insert("X-Gateway-Actual-Model".into(), self.actual_model.clone());
        h.insert("X-Gateway-Channel-Id".into(), self.channel_id.clone());
        h.insert(
            "X-Gateway-Route-Mode".into(),
            match self.route_mode {
                RouteMode::Lock => "lock".into(),
                RouteMode::StageAware => "stage_aware".into(),
            },
        );
        h.insert("X-Gateway-Sticky-Hit".into(), self.sticky_hit.to_string());
        h.insert("X-Gateway-Prefix-Key".into(), self.prefix_key.clone());
        for (k, v) in &self.extra_headers {
            h.insert(k.clone(), v.clone());
        }
        h
    }
}

pub enum GatewayResponse {
    Streaming {
        meta: ResponseMetadata,
        stream: CancelPropagatingStream,
    },
    Json {
        meta: ResponseMetadata,
        body: Value,
    },
}

struct InflightGuard(Arc<ChannelRuntime>);

impl InflightGuard {
    fn enter(ch: Arc<ChannelRuntime>) -> Self {
        ch.inflight.fetch_add(1, Ordering::Relaxed);
        Self(ch)
    }
}

impl Drop for InflightGuard {
    fn drop(&mut self) {
        self.0.inflight.fetch_sub(1, Ordering::Relaxed);
    }
}

/// Ensures every downstream streaming response obeys strict protocol framing:
/// guarantees `MessageStart` before deltas, `ContentBlockStart` before block deltas,
/// and `ContentBlockStop` before `MessageStop` (critical for cross-protocol OpenAI -> Anthropic).
struct StreamEventNormalizer {
    msg_id: String,
    model: String,
    sent_message_start: bool,
    sent_message_stop: bool,
    open_block: Option<(usize, String)>,
}

impl StreamEventNormalizer {
    fn new(msg_id: String, model: String) -> Self {
        Self {
            msg_id,
            model,
            sent_message_start: false,
            sent_message_stop: false,
            open_block: None,
        }
    }

    fn normalize(&mut self, ev: CanonicalStreamEvent) -> Vec<CanonicalStreamEvent> {
        let mut out = Vec::new();
        match ev {
            CanonicalStreamEvent::MessageStart { id, model, usage } => {
                if !self.sent_message_start {
                    self.sent_message_start = true;
                    out.push(CanonicalStreamEvent::MessageStart { id, model, usage });
                }
            }
            CanonicalStreamEvent::ContentBlockStart { index, block_type } => {
                self.ensure_message_start(&mut out);
                if let Some((prev_idx, _)) = self.open_block.take() {
                    out.push(CanonicalStreamEvent::ContentBlockStop { index: prev_idx });
                }
                self.open_block = Some((index, block_type.clone()));
                out.push(CanonicalStreamEvent::ContentBlockStart { index, block_type });
            }
            CanonicalStreamEvent::ToolCallStart { index, id, name } => {
                self.ensure_message_start(&mut out);
                if let Some((prev_idx, _)) = self.open_block.take() {
                    out.push(CanonicalStreamEvent::ContentBlockStop { index: prev_idx });
                }
                self.open_block = Some((index, "tool_use".into()));
                out.push(CanonicalStreamEvent::ToolCallStart { index, id, name });
            }
            CanonicalStreamEvent::ContentBlockStop { index } => {
                if self.open_block.as_ref().map(|(i, _)| *i) == Some(index) {
                    self.open_block = None;
                }
                out.push(CanonicalStreamEvent::ContentBlockStop { index });
            }
            CanonicalStreamEvent::TextDelta { index, text } => {
                self.ensure_message_start(&mut out);
                self.ensure_block_open(index, "text", &mut out);
                out.push(CanonicalStreamEvent::TextDelta { index, text });
            }
            CanonicalStreamEvent::ThinkingDelta { index, thinking } => {
                self.ensure_message_start(&mut out);
                self.ensure_block_open(index, "thinking", &mut out);
                out.push(CanonicalStreamEvent::ThinkingDelta { index, thinking });
            }
            CanonicalStreamEvent::SignatureDelta { index, signature } => {
                self.ensure_message_start(&mut out);
                out.push(CanonicalStreamEvent::SignatureDelta { index, signature });
            }
            CanonicalStreamEvent::ToolCallDelta {
                index,
                arguments_delta,
            } => {
                self.ensure_message_start(&mut out);
                out.push(CanonicalStreamEvent::ToolCallDelta {
                    index,
                    arguments_delta,
                });
            }
            CanonicalStreamEvent::MessageStop { stop_reason } => {
                if self.sent_message_stop {
                    return out;
                }
                self.sent_message_stop = true;
                self.ensure_message_start(&mut out);
                if let Some((prev_idx, _)) = self.open_block.take() {
                    out.push(CanonicalStreamEvent::ContentBlockStop { index: prev_idx });
                }
                out.push(CanonicalStreamEvent::MessageStop { stop_reason });
            }
            other => out.push(other),
        }
        out
    }

    fn ensure_message_start(&mut self, out: &mut Vec<CanonicalStreamEvent>) {
        if !self.sent_message_start {
            self.sent_message_start = true;
            out.push(CanonicalStreamEvent::MessageStart {
                id: self.msg_id.clone(),
                model: self.model.clone(),
                usage: None,
            });
        }
    }

    fn ensure_block_open(&mut self, index: usize, kind: &str, out: &mut Vec<CanonicalStreamEvent>) {
        let needs_open = match &self.open_block {
            Some((cur_idx, cur_kind)) => *cur_idx != index || cur_kind != kind,
            None => true,
        };
        if needs_open {
            if let Some((prev_idx, _)) = self.open_block.take() {
                out.push(CanonicalStreamEvent::ContentBlockStop { index: prev_idx });
            }
            self.open_block = Some((index, kind.to_string()));
            out.push(CanonicalStreamEvent::ContentBlockStart {
                index,
                block_type: kind.to_string(),
            });
        }
    }
}

/// Core Edge-Cloud Microkernel Engine.
/// Managed directly by Tauri (`app.manage(Arc<GatewayEngine>)`) or wrapped by `gateway-daemon`.
pub struct GatewayEngine {
    config: GatewayConfig,
    router: Arc<StickyP2cRouter>,
    signatures: Arc<SignatureCache>,
    trajectory: Arc<TrajectoryStore>,
    inspector: Arc<LiveTrafficInspector>,
    cloud: Arc<CloudSyncClient>,
    middleware: MiddlewarePipeline,
    rate_limiter: Arc<IngressRateLimitMiddleware>,
    guardrail: Arc<GuardrailMiddleware>,
    memory: Arc<ContextMemoryMiddleware>,
    mcp_bridge: Arc<McpBridgeMiddleware>,
    /// Hot-switchable global route mode (0 = Lock, 1 = StageAware) toggled from Tauri Tray.
    active_route_mode: AtomicU8,
    /// Split inbound auth: `api_key` guards `/v1/*` and `/v1beta/*`; `admin_password` guards `/api/*`.
    api_key: RwLock<Option<String>>,
    admin_password: RwLock<Option<String>>,
    /// Optional target path (typically `config.local.toml`) for atomic persistence of runtime GUI edits.
    persist_config_path: RwLock<Option<PathBuf>>,
    http: Client,
}

impl GatewayEngine {
    pub fn new(config: GatewayConfig) -> Result<Self, reqwest::Error> {
        let store = TrajectoryStore::open_in_memory().expect("in-memory sqlite should open");
        Self::with_trajectory_store(config, Arc::new(store))
    }

    pub fn new_with_sqlite(
        config: GatewayConfig,
        db_path: impl AsRef<Path>,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        let store = Arc::new(TrajectoryStore::open(db_path)?);
        Ok(Self::with_trajectory_store(config, store)?)
    }

    pub fn with_trajectory_store(
        config: GatewayConfig,
        trajectory: Arc<TrajectoryStore>,
    ) -> Result<Self, reqwest::Error> {
        let http = Client::builder()
            .timeout(Duration::from_secs(config.server.request_timeout_secs))
            .pool_max_idle_per_host(16)
            .build()?;
        let mode_byte = match config.routing.default_mode {
            RouteMode::Lock => 0,
            RouteMode::StageAware => 1,
        };
        let router = Arc::new(StickyP2cRouter::new(
            config.channels.clone(),
            config.routing.clone(),
        ));
        let signatures = Arc::new(SignatureCache::new(Duration::from_secs(3600)));

        let initial_api_key = config.server.effective_api_key();
        let initial_admin_password = config.server.effective_admin_password();

        let rate_limiter = Arc::new(IngressRateLimitMiddleware::new(
            config.middleware.rpm_limit,
            config.middleware.rate_limit_burst,
        ));
        let guardrail = Arc::new(GuardrailMiddleware {
            redact_secrets: config.middleware.redact_secrets,
            redact_pii: config.middleware.redact_pii,
            blocked_tools: config.middleware.blocked_tools.clone(),
            blocked_arg_patterns: config.middleware.blocked_arg_patterns.clone(),
        });
        let memory = Arc::new(ContextMemoryMiddleware::new(
            config.middleware.memory_enabled,
            config.middleware.memory_facts.clone(),
            config.middleware.memory_max_entries,
        ));
        let mcp_bridge = Arc::new(McpBridgeMiddleware::new(
            config.middleware.mcp_servers.clone(),
        ));

        let mut middleware = MiddlewarePipeline::new();
        middleware.push(Arc::clone(&rate_limiter) as Arc<dyn crate::middleware::Middleware>);
        middleware.push(Arc::clone(&guardrail) as Arc<dyn crate::middleware::Middleware>);
        middleware.push(Arc::clone(&memory) as Arc<dyn crate::middleware::Middleware>);
        middleware.push(Arc::clone(&mcp_bridge) as Arc<dyn crate::middleware::Middleware>);

        let cloud = Arc::new(CloudSyncClient::new(http.clone()));
        let inspector = Arc::new(LiveTrafficInspector::default());

        Ok(Self {
            config,
            router,
            signatures,
            trajectory,
            inspector,
            cloud,
            middleware,
            rate_limiter,
            guardrail,
            memory,
            mcp_bridge,
            active_route_mode: AtomicU8::new(mode_byte),
            api_key: RwLock::new(initial_api_key),
            admin_password: RwLock::new(initial_admin_password),
            persist_config_path: RwLock::new(None),
            http,
        })
    }

    pub fn set_persist_config_path(&self, path: PathBuf) {
        let mut g = self
            .persist_config_path
            .write()
            .unwrap_or_else(|e| e.into_inner());
        *g = Some(path);
    }

    fn persist_local_config(&self) -> Result<(), GatewayError> {
        let maybe_path = self
            .persist_config_path
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        let Some(path) = maybe_path else {
            return Ok(());
        };

        let mut snapshot = self.config.clone();
        snapshot.routing.default_mode = self.active_route_mode();
        snapshot.server.api_key = self.api_key.read().unwrap_or_else(|e| e.into_inner()).clone();
        snapshot.server.local_auth_token = snapshot.server.api_key.clone();
        snapshot.server.admin_password = self
            .admin_password
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        snapshot.channels = self.router.channel_configs();
        snapshot.save_atomic(path)?;
        Ok(())
    }

    pub fn list_channel_configs(&self) -> Vec<ChannelConfig> {
        self.router.channel_configs()
    }

    /// Hot-updates or inserts a channel into the live P2C router (auto-normalizing `base_url`)
    /// and atomically persists to `config.local.toml` when configured.
    pub fn upsert_channel(&self, mut cfg: ChannelConfig) -> Result<ChannelSnapshot, GatewayError> {
        cfg.normalize();
        if cfg.id.is_empty() {
            return Err(GatewayError::Config(ConfigError::Validation(
                "channel id cannot be empty".into(),
            )));
        }
        if cfg.base_url.is_empty() {
            return Err(GatewayError::Config(ConfigError::Validation(
                "channel base_url cannot be empty".into(),
            )));
        }
        let snap = self.router.upsert_channel(cfg);
        self.persist_local_config()?;
        Ok(snap)
    }

    pub fn delete_channel(&self, id: &str) -> Result<bool, GatewayError> {
        let removed = self.router.delete_channel(id);
        if removed {
            self.persist_local_config()?;
        }
        Ok(removed)
    }

    pub fn set_channel_enabled(&self, id: &str, enabled: bool) -> Result<ChannelSnapshot, GatewayError> {
        let snap = self
            .router
            .set_channel_enabled(id, enabled)
            .ok_or_else(|| GatewayError::NoAvailableChannel(id.to_string()))?;
        self.persist_local_config()?;
        Ok(snap)
    }

    /// Imports a discovered local inference provider (Ollama / LM Studio / vLLM / custom) into
    /// the live P2C pool as an OpenAI-compatible channel and atomically persists `config.local.toml`.
    pub fn import_local_provider(
        &self,
        provider: &DiscoveredLocalProvider,
        channel_id_override: Option<String>,
        offline_fallback: bool,
    ) -> Result<ChannelSnapshot, GatewayError> {
        let cfg = build_local_channel_config(provider, channel_id_override, offline_fallback);
        self.upsert_channel(cfg)
    }

    pub fn get_savings_summary(&self, days: u32) -> Result<SavingsSummary, TrajectoryError> {
        self.trajectory
            .get_savings_summary(days, &self.config.model_pricing)
    }

    pub fn export_fork_diff_markdown(
        &self,
        original_step_id: &str,
        forked_step_id: &str,
        redact_secrets: bool,
    ) -> Result<ForkDiffExport, GatewayError> {
        let original = self.trajectory.get_step(original_step_id)?;
        let forked = self.trajectory.get_step(forked_step_id)?;
        Ok(render_fork_diff_markdown(&original, &forked, redact_secrets))
    }

    fn http_client_for_channel(&self, cfg: &ChannelConfig) -> Result<Client, GatewayError> {
        let Some(ref proxy_url) = cfg.proxy_url else {
            return Ok(self.http.clone());
        };
        let proxy = Proxy::all(proxy_url)
            .map_err(|e| GatewayError::Transport(format!("invalid proxy_url '{}': {}", proxy_url, e)))?;
        Client::builder()
            .timeout(Duration::from_secs(self.config.server.request_timeout_secs))
            .proxy(proxy)
            .build()
            .map_err(|e| GatewayError::Transport(e.to_string()))
    }

    /// Probes an upstream channel's reachability, latency (ms), and optional `quota_endpoint`.
    pub async fn test_channel_connection(&self, mut cfg: ChannelConfig) -> ChannelTestResult {
        cfg.normalize();
        let probe_url = match cfg.protocol {
            UpstreamProtocol::Gemini => format!("{}/v1beta/models", cfg.base_url),
            UpstreamProtocol::Anthropic
            | UpstreamProtocol::OpenAi
            | UpstreamProtocol::OpenAiResponses => {
                format!("{}/v1/models", cfg.base_url)
            }
        };

        let client = match cfg.proxy_url.as_deref() {
            Some(proxy_url) => match Proxy::all(proxy_url) {
                Ok(p) => match Client::builder()
                    .timeout(Duration::from_secs(10))
                    .proxy(p)
                    .build()
                {
                    Ok(c) => c,
                    Err(e) => {
                        return ChannelTestResult {
                            ok: false,
                            channel_id: cfg.id,
                            status_code: None,
                            latency_ms: 0,
                            endpoint: probe_url,
                            message: format!("proxy build error: {}", e),
                            quota_info: None,
                        };
                    }
                },
                Err(e) => {
                    return ChannelTestResult {
                        ok: false,
                        channel_id: cfg.id,
                        status_code: None,
                        latency_ms: 0,
                        endpoint: probe_url,
                        message: format!("invalid proxy_url: {}", e),
                        quota_info: None,
                    };
                }
            },
            None => match Client::builder().timeout(Duration::from_secs(10)).build() {
                Ok(c) => c,
                Err(_) => self.http.clone(),
            },
        };

        let started = Instant::now();
        let mut req = client.get(&probe_url);
        req = match cfg.protocol {
            UpstreamProtocol::Anthropic => req
                .header("x-api-key", &cfg.api_key)
                .header("anthropic-version", "2023-06-01"),
            UpstreamProtocol::OpenAi | UpstreamProtocol::OpenAiResponses => {
                req.bearer_auth(&cfg.api_key)
            }
            UpstreamProtocol::Gemini => req.header("x-goog-api-key", &cfg.api_key),
        };

        let (ok, status_code, message) = match req.send().await {
            Ok(resp) => {
                let code = resp.status().as_u16();
                // Any non-5xx / reachable HTTP response proves connectivity; 2xx is healthy,
                // 404 on /v1/models is common on minimal proxies so treat < 500 && != 401/403 as reachable.
                let is_ok = (200..300).contains(&code) || code == 404 || code == 405;
                let msg = if (200..300).contains(&code) {
                    format!("HTTP {} OK", code)
                } else if code == 401 || code == 403 {
                    format!("HTTP {} Auth Failed (check api_key)", code)
                } else if code == 404 || code == 405 {
                    format!("HTTP {} Reachable (endpoint reachable, /models not exposed)", code)
                } else {
                    format!("HTTP {}", code)
                };
                (is_ok, Some(code), msg)
            }
            Err(e) => (false, None, format!("connection failed: {}", e)),
        };
        let latency_ms = started.elapsed().as_millis() as u64;

        let quota_info = if let Some(ref q_url) = cfg.quota_endpoint {
            let mut q_req = client.get(q_url);
            q_req = match cfg.protocol {
                UpstreamProtocol::Anthropic => q_req.header("x-api-key", &cfg.api_key),
                UpstreamProtocol::OpenAi | UpstreamProtocol::OpenAiResponses => {
                    q_req.bearer_auth(&cfg.api_key)
                }
                UpstreamProtocol::Gemini => q_req.header("x-goog-api-key", &cfg.api_key),
            };
            match q_req.send().await {
                Ok(qr) if qr.status().is_success() => qr.json::<Value>().await.ok(),
                _ => None,
            }
        } else {
            None
        };

        ChannelTestResult {
            ok,
            channel_id: cfg.id,
            status_code,
            latency_ms,
            endpoint: probe_url,
            message,
            quota_info,
        }
    }

    /// Claude CLI / Anthropic preflight `POST /v1/messages/count_tokens`.
    /// Forwards to upstream `/v1/messages/count_tokens` when routed to a native Anthropic channel
    /// that supports it, falling back seamlessly to deterministic Canonical IR estimation so
    /// Claude Code works transparently even when backed by OpenAI or Gemini upstreams.
    pub async fn count_tokens(&self, payload: Value) -> Result<Value, GatewayError> {
        let codec = codec_for(UpstreamProtocol::Anthropic);
        let canonical = codec.decode_request(&payload, None, None)?;

        if let Some(decision) =
            self.router
                .select_channel(&canonical, Some(self.active_route_mode()), &[])
        {
            if decision.channel.config.protocol == UpstreamProtocol::Anthropic {
                let upstream_model = decision
                    .channel
                    .config
                    .resolve_upstream_model(&decision.actual_model);
                let mut body = payload.clone();
                if let Some(obj) = body.as_object_mut() {
                    obj.insert("model".into(), json!(upstream_model));
                    obj.remove("stream");
                }
                let url = format!("{}/v1/messages/count_tokens", decision.channel.config.base_url);
                if let Ok(client) = self.http_client_for_channel(&decision.channel.config) {
                    if let Ok(resp) = client
                        .post(&url)
                        .timeout(Duration::from_secs(5))
                        .header("x-api-key", &decision.channel.config.api_key)
                        .header("anthropic-version", "2023-06-01")
                        .json(&body)
                        .send()
                        .await
                    {
                        if resp.status().is_success() {
                            if let Ok(val) = resp.json::<Value>().await {
                                if val.get("input_tokens").and_then(|v| v.as_u64()).is_some() {
                                    return Ok(val);
                                }
                            }
                        }
                    }
                }
            }
        }

        let estimated = estimate_canonical_input_tokens(&canonical);
        Ok(json!({ "input_tokens": estimated }))
    }

    pub fn cloud_status(&self) -> CloudSessionStatus {
        self.cloud.status()
    }

    pub async fn cloud_login(
        &self,
        control_plane_url: &str,
        email: &str,
        password: &str,
    ) -> Result<CloudSessionStatus, String> {
        let (_status, channels) = self
            .cloud
            .login_and_bootstrap(control_plane_url, email, password)
            .await?;
        let merged = self.router.merge_remote_channels(channels);
        info!(merged_channels = merged, "synced team channel pool from LoomGate Cloud Control Plane");
        Ok(self.cloud.status())
    }

    pub async fn cloud_sync_team_pool(&self) -> Result<CloudSessionStatus, String> {
        let channels = self.cloud.fetch_team_channels().await?;
        let merged = self.router.merge_remote_channels(channels);
        info!(merged_channels = merged, "refreshed team channel pool from Cloud Control Plane");
        Ok(self.cloud.status())
    }

    pub fn cloud_logout(&self) {
        self.cloud.logout();
    }

    pub fn middleware_layers(&self) -> Vec<&'static str> {
        self.middleware.layer_names()
    }

    pub fn mcp_bridge(&self) -> Arc<McpBridgeMiddleware> {
        Arc::clone(&self.mcp_bridge)
    }

    /// Verifies inference API key (`/v1/*`, `/v1beta/*`). Returns true when no `api_key` is set.
    pub fn verify_api_auth(&self, provided_token: Option<&str>) -> bool {
        let guard = self.api_key.read().unwrap_or_else(|e| e.into_inner());
        match guard.as_deref() {
            None | Some("") => true,
            Some(expected) => provided_token == Some(expected),
        }
    }

    /// Legacy alias for `verify_api_auth`.
    pub fn verify_inbound_auth(&self, provided_token: Option<&str>) -> bool {
        self.verify_api_auth(provided_token)
    }

    /// Verifies management/admin password (`/api/*`, `/__admin/*`). Returns true when unset.
    pub fn verify_admin_auth(&self, provided_password: Option<&str>) -> bool {
        let guard = self
            .admin_password
            .read()
            .unwrap_or_else(|e| e.into_inner());
        match guard.as_deref() {
            None | Some("") => true,
            Some(expected) => provided_password == Some(expected),
        }
    }

    pub fn auth_settings(&self) -> AuthSettings {
        AuthSettings {
            api_key: self.api_key.read().unwrap_or_else(|e| e.into_inner()).clone(),
            admin_password: self
                .admin_password
                .read()
                .unwrap_or_else(|e| e.into_inner())
                .clone(),
        }
    }

    pub fn set_auth_settings(
        &self,
        api_key: Option<String>,
        admin_password: Option<String>,
    ) -> Result<AuthSettings, GatewayError> {
        let norm = |v: Option<String>| {
            v.and_then(|s| {
                let t = s.trim().to_string();
                if t.is_empty() {
                    None
                } else {
                    Some(t)
                }
            })
        };
        *self.api_key.write().unwrap_or_else(|e| e.into_inner()) = norm(api_key);
        *self
            .admin_password
            .write()
            .unwrap_or_else(|e| e.into_inner()) = norm(admin_password);
        self.persist_local_config()?;
        Ok(self.auth_settings())
    }

    pub fn local_auth_token(&self) -> Option<String> {
        self.api_key.read().unwrap_or_else(|e| e.into_inner()).clone()
    }

    pub fn set_local_auth_token(&self, token: Option<String>) {
        let current_admin = self
            .admin_password
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        let _ = self.set_auth_settings(token, current_admin);
    }

    pub fn format_error(
        &self,
        protocol: UpstreamProtocol,
        error_type: &str,
        message: &str,
    ) -> Value {
        codec_for(protocol).encode_error(error_type, message)
    }

    pub fn reset_runtime_state(&self) {
        self.router.reset_state();
        self.signatures.clear();
        self.rate_limiter.reset();
        self.memory.clear();
        self.inspector.clear();
    }

    pub fn list_traffic(&self, limit: usize) -> Vec<TrafficInspectEntry> {
        self.inspector.list(limit)
    }

    pub fn get_traffic(&self, request_id: &str) -> Option<TrafficInspectEntry> {
        self.inspector.get(request_id)
    }

    pub fn clear_traffic(&self) {
        self.inspector.clear();
    }

    pub fn active_route_mode(&self) -> RouteMode {
        match self.active_route_mode.load(Ordering::Relaxed) {
            1 => RouteMode::StageAware,
            _ => RouteMode::Lock,
        }
    }

    pub fn set_route_mode(&self, mode: RouteMode) {
        let val = match mode {
            RouteMode::Lock => 0,
            RouteMode::StageAware => 1,
        };
        self.active_route_mode.store(val, Ordering::Relaxed);
        info!(mode = ?mode, "hot-switched gateway route mode");
    }

    pub fn channel_snapshots(&self) -> Vec<ChannelSnapshot> {
        self.router.snapshots()
    }

    pub fn list_sessions(&self, limit: usize) -> Result<Vec<SessionSummary>, TrajectoryError> {
        self.trajectory.list_sessions(limit)
    }

    pub fn get_session_steps(&self, session_id: &str) -> Result<Vec<TrajectoryStep>, TrajectoryError> {
        self.trajectory.get_session_steps(session_id)
    }

    /// Hydrates any thinking blocks missing a `signature` from the sidecar `SignatureCache`,
    /// keeping multi-turn reasoning chains intact even if a client strips opaque signatures.
    fn hydrate_signatures(&self, prefix_key: &str, req: &mut CanonicalRequest) {
        for msg in &mut req.messages {
            for (idx, part) in msg.parts.iter_mut().enumerate() {
                if let ContentPart::Thinking { signature, .. } = part {
                    if signature.is_none() {
                        if let Some(cached) = self
                            .signatures
                            .get(prefix_key, idx)
                            .or_else(|| self.signatures.get(prefix_key, 0))
                        {
                            *signature = Some(cached);
                        }
                    }
                }
            }
        }
        if req.opaque_state.thought_signature.is_none() {
            req.opaque_state.thought_signature = self.signatures.get(prefix_key, 0);
        }
    }

    /// Stage 2 Differentiator: Fork & Diff from any historical trajectory step.
    pub async fn fork_and_diff_step(
        &self,
        step_id: &str,
        target_model: String,
    ) -> Result<ForkDiffResult, GatewayError> {
        let original = self.trajectory.get_step(step_id)?;
        let mut req_val: Value = serde_json::from_str(&original.request_json)
            .map_err(|e| GatewayError::Codec(CodecError::Json(e)))?;

        if let Some(obj) = req_val.as_object_mut() {
            obj.insert("model".into(), json!(target_model));
            obj.insert("stream".into(), json!(false));
        }

        let protocol = match original.protocol.as_str() {
            "openai" => UpstreamProtocol::OpenAi,
            "openai_responses" => UpstreamProtocol::OpenAiResponses,
            "gemini" => UpstreamProtocol::Gemini,
            _ => UpstreamProtocol::Anthropic,
        };

        let resp = self
            .handle_request_internal(
                protocol,
                req_val,
                Some(original.session_id.clone()),
                None,
                Some(RouteMode::Lock),
                Some(original.workspace.clone()),
                Some(original.step_id.clone()),
                Some(original.turn_index),
            )
            .await?;

        let forked_step_id = match resp {
            GatewayResponse::Json { meta, .. } => meta.step_id,
            GatewayResponse::Streaming { meta, .. } => meta.step_id,
        };

        let forked = self.trajectory.get_step(&forked_step_id)?;
        let ttft_delta_ms = forked.ttft_ms as i64 - original.ttft_ms as i64;
        let output_tokens_delta = forked.output_tokens as i64 - original.output_tokens as i64;
        let cache_hit_ratio_delta = forked.cache_hit_ratio() - original.cache_hit_ratio();

        Ok(ForkDiffResult {
            original,
            forked,
            ttft_delta_ms,
            output_tokens_delta,
            cache_hit_ratio_delta,
        })
    }

    pub async fn handle_request(
        &self,
        downstream_protocol: UpstreamProtocol,
        payload: Value,
        session_id: Option<String>,
        task_stage: Option<String>,
        override_mode: Option<RouteMode>,
    ) -> Result<GatewayResponse, GatewayError> {
        self.handle_request_internal(
            downstream_protocol,
            payload,
            session_id,
            task_stage,
            override_mode,
            None,
            None,
            None,
        )
        .await
    }

    #[allow(clippy::too_many_arguments)]
    async fn handle_request_internal(
        &self,
        downstream_protocol: UpstreamProtocol,
        payload: Value,
        session_id: Option<String>,
        task_stage: Option<String>,
        override_mode: Option<RouteMode>,
        workspace: Option<String>,
        parent_step_id: Option<String>,
        fixed_turn_index: Option<i64>,
    ) -> Result<GatewayResponse, GatewayError> {
        let request_id = Uuid::new_v4().to_string();
        let step_id = new_step_id();
        let raw_request_json = serde_json::to_string(&payload).unwrap_or_else(|_| "{}".into());

        let downstream_codec = codec_for(downstream_protocol);
        let canonical = downstream_codec.decode_request(&payload, session_id, task_stage)?;
        let effective_mode = override_mode.or_else(|| Some(self.active_route_mode()));

        let client_key = canonical
            .session_id
            .clone()
            .unwrap_or_else(|| "local".to_string());
        let ws = workspace.clone().unwrap_or_else(|| "default".to_string());
        let mw_ctx = MiddlewareContext::new(
            request_id.clone(),
            downstream_protocol,
            client_key,
            ws,
            canonical,
        );

        let mw_outcome = self
            .middleware
            .execute(mw_ctx, |mut ctx| {
                Box::pin(async move {
                    let res = self
                        .dispatch_upstream(
                            &mut ctx,
                            step_id,
                            raw_request_json,
                            effective_mode,
                            parent_step_id,
                            fixed_turn_index,
                        )
                        .await;
                    (ctx, res)
                })
            })
            .await;

        match mw_outcome {
            Err(rej) => {
                self.inspector.record(TrafficInspectEntry {
                    request_id: request_id.clone(),
                    ts_ms: now_epoch_ms().max(0) as u64,
                    downstream_protocol: protocol_label(downstream_protocol).into(),
                    upstream_protocol: "none".into(),
                    requested_model: payload
                        .get("model")
                        .and_then(|v| v.as_str())
                        .unwrap_or("unknown")
                        .to_string(),
                    actual_model: "blocked".into(),
                    channel_id: rej.middleware.into(),
                    route_mode: match self.active_route_mode() {
                        RouteMode::Lock => "lock".into(),
                        RouteMode::StageAware => "stage_aware".into(),
                    },
                    sticky_hit: false,
                    stream: payload.get("stream").and_then(|v| v.as_bool()).unwrap_or(false),
                    ttft_ms: 0,
                    status: format!("rejected_{}", rej.status),
                    input_tokens: 0,
                    cache_read_tokens: 0,
                    output_tokens: 0,
                    guardrail_redactions: 0,
                    blocked_reason: Some(rej.message.clone()),
                    inbound_preview: serde_json::to_string_pretty(&payload).unwrap_or_default(),
                    canonical_ir_summary: json!({ "rejected_by": rej.middleware }).to_string(),
                    upstream_preview: String::new(),
                    sse_events: vec![],
                });
                Err(GatewayError::MiddlewareRejected {
                    status: rej.status,
                    error_type: rej.error_type,
                    message: rej.message,
                    middleware: rej.middleware,
                })
            }
            Ok((final_ctx, Ok(mut resp))) => {
                match &mut resp {
                    GatewayResponse::Streaming { meta, .. }
                    | GatewayResponse::Json { meta, .. } => {
                        meta.extra_headers.extend(final_ctx.annotations);
                    }
                }
                Ok(resp)
            }
            Ok((_, Err(e))) => Err(e),
        }
    }

    async fn dispatch_upstream(
        &self,
        mw_ctx: &mut MiddlewareContext,
        step_id: String,
        raw_request_json: String,
        effective_mode: Option<RouteMode>,
        parent_step_id: Option<String>,
        fixed_turn_index: Option<i64>,
    ) -> Result<GatewayResponse, GatewayError> {
        let request_id = mw_ctx.request_id.clone();
        let downstream_protocol = mw_ctx.downstream_protocol;
        let downstream_codec = codec_for(downstream_protocol);
        let canonical = &mut mw_ctx.request;
        let workspace = Some(mw_ctx.workspace.clone());

        let mut excluded_channels: Vec<String> = Vec::new();
        let max_failovers = self.router.channel_count().max(1);

        // Phase 1 Cloud Billing Pre-Deduction (if Edge is connected to Cloud Control Plane)
        let cloud_reserved = if self.cloud.is_connected() {
            let est_tokens = u64::from(canonical.max_tokens.unwrap_or(1024)).max(256);
            if let Err(msg) = self
                .cloud
                .reserve_billing(&request_id, &canonical.model, est_tokens)
                .await
            {
                return Err(GatewayError::MiddlewareRejected {
                    status: 402,
                    error_type: "payment_required".into(),
                    message: msg,
                    middleware: "cloud_billing",
                });
            }
            true
        } else {
            false
        };

        for _failover_idx in 0..max_failovers {
            let decision = self
                .router
                .select_channel(canonical, effective_mode, &excluded_channels)
                .ok_or_else(|| GatewayError::NoAvailableChannel(canonical.model.clone()))?;

            // Hydrate sidecar-cached thinking signatures before encoding upstream request.
            self.hydrate_signatures(&decision.prefix_key, canonical);

            let channel = Arc::clone(&decision.channel);
            let upstream_codec = codec_for(channel.config.protocol);
            let same_protocol = downstream_protocol == channel.config.protocol;
            let upstream_model = channel.config.resolve_upstream_model(&decision.actual_model);
            let encoded_body =
                upstream_codec.encode_request(canonical, &upstream_model, same_protocol)?;
            let endpoint = upstream_endpoint(
                &channel.config.base_url,
                channel.config.protocol,
                &upstream_model,
                canonical.stream,
            );

            let channel_http = self.http_client_for_channel(&channel.config)?;
            let mut l1_attempt = 0u32;
            loop {
                let started = Instant::now();
                let inflight_guard = InflightGuard::enter(Arc::clone(&channel));

                let mut req_builder = channel_http.post(&endpoint).json(&encoded_body);
                req_builder = match channel.config.protocol {
                    UpstreamProtocol::Anthropic => req_builder
                        .header("x-api-key", &channel.config.api_key)
                        .header("anthropic-version", "2023-06-01")
                        .header("X-Gateway-Prefix-Key", &decision.prefix_key),
                    UpstreamProtocol::OpenAi | UpstreamProtocol::OpenAiResponses => req_builder
                        .bearer_auth(&channel.config.api_key)
                        .header("X-Gateway-Prefix-Key", &decision.prefix_key),
                    UpstreamProtocol::Gemini => req_builder
                        .header("x-goog-api-key", &channel.config.api_key)
                        .header("X-Gateway-Prefix-Key", &decision.prefix_key),
                };

                let send_result = req_builder.send().await;
                match send_result {
                    Ok(resp) if resp.status().is_success() => {
                        let ttft_ms = started.elapsed().as_millis() as u64;
                        if canonical.stream {
                            channel.record_ttft(ttft_ms);
                        }

                        let mut extra_headers = BTreeMap::new();
                        if decision.offline_fallback_used {
                            extra_headers.insert("X-Gateway-Offline-Fallback".into(), "true".into());
                        }
                        let meta = ResponseMetadata {
                            request_id: request_id.clone(),
                            step_id: step_id.clone(),
                            actual_model: upstream_model.clone(),
                            channel_id: channel.config.id.clone(),
                            route_mode: decision.mode,
                            sticky_hit: decision.sticky_hit,
                            prefix_key: decision.prefix_key.clone(),
                            extra_headers,
                        };

                        let traj_session = canonical
                            .session_id
                            .clone()
                            .unwrap_or_else(|| decision.prefix_key.clone());
                        let traj_workspace = workspace.clone().unwrap_or_else(|| "default".into());
                        let turn_index = fixed_turn_index.unwrap_or_else(|| {
                            self.trajectory.next_turn_index(&traj_session).unwrap_or(1)
                        });
                        let proto_str = match downstream_protocol {
                            UpstreamProtocol::Anthropic => "anthropic",
                            UpstreamProtocol::OpenAi => "openai",
                            UpstreamProtocol::OpenAiResponses => "openai_responses",
                            UpstreamProtocol::Gemini => "gemini",
                        }
                        .to_string();
                        let mode_str = match decision.mode {
                            RouteMode::Lock => "lock",
                            RouteMode::StageAware => "stage_aware",
                        }
                        .to_string();

                        let guardrail_redactions = mw_ctx
                            .annotations
                            .get("X-Gateway-Guardrail-Redactions")
                            .and_then(|s| s.parse::<usize>().ok())
                            .unwrap_or(0);
                        let canonical_ir_summary = summarize_canonical_ir(canonical);
                        let upstream_preview =
                            serde_json::to_string_pretty(&encoded_body).unwrap_or_default();
                        let upstream_proto_str = protocol_label(channel.config.protocol).to_string();

                        if canonical.stream {
                            let cancel_token = CancellationToken::new();
                            let metrics = Arc::new(StreamMetrics::default());
                            let (tx, rx) = mpsc::channel::<Result<Bytes, std::io::Error>>(32);

                            let bg_token = cancel_token.clone();
                            let bg_metrics = Arc::clone(&metrics);
                            let bg_signatures = Arc::clone(&self.signatures);
                            let bg_traj = Arc::clone(&self.trajectory);
                            let bg_inspector = Arc::clone(&self.inspector);
                            let bg_cloud = Arc::clone(&self.cloud);
                            let mut stream_guardrail = self.guardrail.stream_checker();
                            let bg_req_id = request_id.clone();
                            let prefix_key = decision.prefix_key.clone();
                            let upstream_proto = channel.config.protocol;
                            let requested_model = canonical.model.clone();
                            let actual_model = upstream_model.clone();
                            let channel_id = channel.config.id.clone();
                            let step_id_bg = step_id.clone();
                            let parent_bg = parent_step_id.clone();
                            let req_json_bg = raw_request_json.clone();
                            let sticky_hit = decision.sticky_hit;
                            let synth_msg_id = format!("msg_{}", &request_id[..8]);

                            tokio::spawn(async move {
                                let _guard = inflight_guard;
                                let up_codec = codec_for(upstream_proto);
                                let down_codec = codec_for(downstream_protocol);
                                let mut parser = SseFrameParser::new();
                                let mut normalizer = StreamEventNormalizer::new(
                                    synth_msg_id,
                                    actual_model.clone(),
                                );
                                let mut byte_stream = resp.bytes_stream();
                                let mut acc_text = String::new();
                                let mut acc_tools: Vec<Value> = Vec::new();
                                let mut acc_usage = CanonicalUsage::default();
                                let mut status_label = "completed";
                                let mut blocked_reason: Option<String> = None;
                                let mut sse_events_log: Vec<String> = Vec::new();

                                'stream_loop: loop {
                                    tokio::select! {
                                        biased;
                                        _ = bg_token.cancelled() => {
                                            drop(byte_stream);
                                            status_label = "aborted_by_client";
                                            break 'stream_loop;
                                        }
                                        maybe_chunk = byte_stream.next() => {
                                            match maybe_chunk {
                                                Some(Ok(chunk)) => {
                                                    let frames = parser.push_bytes(&chunk);
                                                    for frame in frames {
                                                        let raw_events = up_codec.decode_sse_frame(frame.event.as_deref(), &frame.data);
                                                        for raw_ev in raw_events {
                                                            for ev in normalizer.normalize(raw_ev) {
                                                                if sse_events_log.len() < 64 {
                                                                    sse_events_log.push(stream_event_label(&ev).into());
                                                                }
                                                                if stream_guardrail.is_enabled() {
                                                                    if let Err(reason) = stream_guardrail.check_event(&ev) {
                                                                        status_label = "guardrail_blocked";
                                                                        blocked_reason = Some(reason.clone());
                                                                        let err_ev = CanonicalStreamEvent::Error { message: reason };
                                                                        for out_sse in down_codec.encode_sse_event(&err_ev) {
                                                                            bg_metrics.add_output_bytes(out_sse.len() as u64);
                                                                            let _ = tx.send(Ok(Bytes::from(out_sse))).await;
                                                                        }
                                                                        bg_token.cancel();
                                                                        drop(byte_stream);
                                                                        break 'stream_loop;
                                                                    }
                                                                }
                                                                match &ev {
                                                                    CanonicalStreamEvent::MessageStart { usage: Some(u), .. } => {
                                                                        acc_usage.input_tokens = acc_usage.input_tokens.max(u.input_tokens);
                                                                        acc_usage.cache_read_input_tokens = acc_usage.cache_read_input_tokens.max(u.cache_read_input_tokens);
                                                                    }
                                                                    CanonicalStreamEvent::TextDelta { text, .. } => acc_text.push_str(text),
                                                                    CanonicalStreamEvent::ToolCallStart { id, name, .. } => {
                                                                        acc_tools.push(json!({ "id": id, "name": name }));
                                                                    }
                                                                    CanonicalStreamEvent::SignatureDelta { index, signature } => {
                                                                        bg_signatures.put(&prefix_key, *index, signature.clone());
                                                                    }
                                                                    CanonicalStreamEvent::UsageDelta { usage } => {
                                                                        if usage.input_tokens > 0 { acc_usage.input_tokens = usage.input_tokens; }
                                                                        if usage.output_tokens > 0 { acc_usage.output_tokens = usage.output_tokens; }
                                                                        if usage.cache_read_input_tokens > 0 { acc_usage.cache_read_input_tokens = usage.cache_read_input_tokens; }
                                                                    }
                                                                    _ => {}
                                                                }
                                                                for out_sse in down_codec.encode_sse_event(&ev) {
                                                                    bg_metrics.add_output_bytes(out_sse.len() as u64);
                                                                    if tx.send(Ok(Bytes::from(out_sse))).await.is_err() {
                                                                        bg_token.cancel();
                                                                        status_label = "aborted_by_client";
                                                                        break 'stream_loop;
                                                                    }
                                                                }
                                                            }
                                                        }
                                                    }
                                                }
                                                Some(Err(err)) => {
                                                    status_label = "stream_error";
                                                    let _ = tx.send(Err(std::io::Error::new(
                                                        std::io::ErrorKind::BrokenPipe,
                                                        err.to_string(),
                                                    ))).await;
                                                    break 'stream_loop;
                                                }
                                                None => {
                                                    for frame in parser.finish() {
                                                        let raw_events = up_codec.decode_sse_frame(frame.event.as_deref(), &frame.data);
                                                        for raw_ev in raw_events {
                                                            for ev in normalizer.normalize(raw_ev) {
                                                                if sse_events_log.len() < 64 {
                                                                    sse_events_log.push(stream_event_label(&ev).into());
                                                                }
                                                                if stream_guardrail.is_enabled() {
                                                                    if let Err(reason) = stream_guardrail.check_event(&ev) {
                                                                        status_label = "guardrail_blocked";
                                                                        blocked_reason = Some(reason.clone());
                                                                        let err_ev = CanonicalStreamEvent::Error { message: reason };
                                                                        for out_sse in down_codec.encode_sse_event(&err_ev) {
                                                                            bg_metrics.add_output_bytes(out_sse.len() as u64);
                                                                            let _ = tx.send(Ok(Bytes::from(out_sse))).await;
                                                                        }
                                                                        break 'stream_loop;
                                                                    }
                                                                }
                                                                match &ev {
                                                                    CanonicalStreamEvent::TextDelta { text, .. } => {
                                                                        acc_text.push_str(text);
                                                                    }
                                                                    CanonicalStreamEvent::SignatureDelta { index, signature } => {
                                                                        bg_signatures.put(&prefix_key, *index, signature.clone());
                                                                    }
                                                                    _ => {}
                                                                }
                                                                for out_sse in down_codec.encode_sse_event(&ev) {
                                                                    bg_metrics.add_output_bytes(out_sse.len() as u64);
                                                                    let _ = tx.send(Ok(Bytes::from(out_sse))).await;
                                                                }
                                                            }
                                                        }
                                                    }
                                                    bg_metrics.completed.store(1, Ordering::Relaxed);
                                                    break 'stream_loop;
                                                }
                                            }
                                        }
                                    }
                                }

                                let final_out_tokens = if acc_usage.output_tokens > 0 {
                                    acc_usage.output_tokens
                                } else {
                                    bg_metrics.fallback_output_tokens()
                                };

                                let step = TrajectoryStep {
                                    step_id: step_id_bg.clone(),
                                    workspace: traj_workspace,
                                    session_id: traj_session.clone(),
                                    parent_step_id: parent_bg,
                                    turn_index,
                                    protocol: proto_str.clone(),
                                    channel_id: channel_id.clone(),
                                    requested_model: requested_model.clone(),
                                    actual_model: actual_model.clone(),
                                    route_mode: mode_str.clone(),
                                    sticky_hit,
                                    ttft_ms,
                                    input_tokens: acc_usage.input_tokens,
                                    cache_read_tokens: acc_usage.cache_read_input_tokens,
                                    output_tokens: final_out_tokens,
                                    status: status_label.into(),
                                    request_json: req_json_bg.clone(),
                                    response_text: acc_text,
                                    tool_calls_json: serde_json::to_string(&acc_tools).unwrap_or_else(|_| "[]".into()),
                                    created_at_ms: now_epoch_ms(),
                                };
                                let _ = bg_traj.append_step(&step);

                                bg_inspector.record(TrafficInspectEntry {
                                    request_id: bg_req_id.clone(),
                                    ts_ms: now_epoch_ms().max(0) as u64,
                                    downstream_protocol: proto_str.clone(),
                                    upstream_protocol: upstream_proto_str,
                                    requested_model,
                                    actual_model: actual_model.clone(),
                                    channel_id: channel_id.clone(),
                                    route_mode: mode_str.clone(),
                                    sticky_hit,
                                    stream: true,
                                    ttft_ms,
                                    status: status_label.into(),
                                    input_tokens: acc_usage.input_tokens,
                                    cache_read_tokens: acc_usage.cache_read_input_tokens,
                                    output_tokens: final_out_tokens,
                                    guardrail_redactions,
                                    blocked_reason,
                                    inbound_preview: req_json_bg,
                                    canonical_ir_summary,
                                    upstream_preview,
                                    sse_events: sse_events_log,
                                });

                                if cloud_reserved {
                                    bg_cloud
                                        .settle_billing(
                                            &bg_req_id,
                                            &actual_model,
                                            acc_usage.input_tokens,
                                            acc_usage.cache_read_input_tokens,
                                            final_out_tokens,
                                            false,
                                        )
                                        .await;
                                    bg_cloud
                                        .upload_sanitized_telemetry(
                                            &traj_session,
                                            &step_id_bg,
                                            &proto_str,
                                            &channel_id,
                                            &actual_model,
                                            &mode_str,
                                            sticky_hit,
                                            ttft_ms,
                                            acc_usage.input_tokens,
                                            acc_usage.cache_read_input_tokens,
                                            final_out_tokens,
                                            status_label,
                                        )
                                        .await;
                                }
                            });

                            let stream = CancelPropagatingStream::new(
                                rx,
                                cancel_token,
                                metrics,
                                channel.config.id.clone(),
                                request_id,
                            );
                            return Ok(GatewayResponse::Streaming { meta, stream });
                        } else {
                            let raw_body: Value = resp
                                .json()
                                .await
                                .map_err(|e| GatewayError::Transport(e.to_string()))?;
                            drop(inflight_guard);

                            // True bidirectional lossless non-streaming translation via CanonicalResponse IR.
                            let canonical_resp = upstream_codec.decode_response(&raw_body)?;
                            for (idx, part) in canonical_resp.parts.iter().enumerate() {
                                if let ContentPart::Thinking {
                                    signature: Some(sig),
                                    ..
                                } = part
                                {
                                    self.signatures
                                        .put(&decision.prefix_key, idx, sig.clone());
                                }
                            }
                            let encoded_downstream =
                                downstream_codec.encode_response(&canonical_resp);
                            mw_ctx.response = Some(canonical_resp.clone());

                            let step = TrajectoryStep {
                                step_id: step_id.clone(),
                                workspace: traj_workspace,
                                session_id: traj_session.clone(),
                                parent_step_id: parent_step_id.clone(),
                                turn_index,
                                protocol: proto_str.clone(),
                                channel_id: channel.config.id.clone(),
                                requested_model: canonical.model.clone(),
                                actual_model: upstream_model.clone(),
                                route_mode: mode_str.clone(),
                                sticky_hit: decision.sticky_hit,
                                ttft_ms,
                                input_tokens: canonical_resp.usage.input_tokens,
                                cache_read_tokens: canonical_resp.usage.cache_read_input_tokens,
                                output_tokens: canonical_resp.usage.output_tokens,
                                status: "completed".into(),
                                request_json: raw_request_json.clone(),
                                response_text: canonical_resp.concatenated_text(),
                                tool_calls_json: "[]".into(),
                                created_at_ms: now_epoch_ms(),
                            };
                            let _ = self.trajectory.append_step(&step);

                            self.inspector.record(TrafficInspectEntry {
                                request_id: request_id.clone(),
                                ts_ms: now_epoch_ms().max(0) as u64,
                                downstream_protocol: proto_str.clone(),
                                upstream_protocol: upstream_proto_str,
                                requested_model: canonical.model.clone(),
                                actual_model: upstream_model.clone(),
                                channel_id: channel.config.id.clone(),
                                route_mode: mode_str.clone(),
                                sticky_hit: decision.sticky_hit,
                                stream: false,
                                ttft_ms,
                                status: "completed".into(),
                                input_tokens: canonical_resp.usage.input_tokens,
                                cache_read_tokens: canonical_resp.usage.cache_read_input_tokens,
                                output_tokens: canonical_resp.usage.output_tokens,
                                guardrail_redactions,
                                blocked_reason: None,
                                inbound_preview: raw_request_json,
                                canonical_ir_summary,
                                upstream_preview,
                                sse_events: vec![],
                            });

                            if cloud_reserved {
                                self.cloud
                                    .settle_billing(
                                        &request_id,
                                        &upstream_model,
                                        canonical_resp.usage.input_tokens,
                                        canonical_resp.usage.cache_read_input_tokens,
                                        canonical_resp.usage.output_tokens,
                                        false,
                                    )
                                    .await;
                                self.cloud
                                    .upload_sanitized_telemetry(
                                        &traj_session,
                                        &step_id,
                                        &proto_str,
                                        &channel.config.id,
                                        &upstream_model,
                                        &mode_str,
                                        decision.sticky_hit,
                                        ttft_ms,
                                        canonical_resp.usage.input_tokens,
                                        canonical_resp.usage.cache_read_input_tokens,
                                        canonical_resp.usage.output_tokens,
                                        "completed",
                                    )
                                    .await;
                            }

                            return Ok(GatewayResponse::Json {
                                meta,
                                body: encoded_downstream,
                            });
                        }
                    }
                    Ok(resp) => {
                        let status = resp.status().as_u16();
                        let retry_after = resp
                            .headers()
                            .get("retry-after")
                            .and_then(|v| v.to_str().ok())
                            .and_then(|s| s.parse::<u64>().ok());
                        let err_body = resp.text().await.unwrap_or_default();
                        drop(inflight_guard);

                        let action = classify_upstream_error(
                            status,
                            &err_body,
                            retry_after,
                            &self.config.error_governance,
                            l1_attempt,
                        );

                        warn!(
                            request_id = %request_id,
                            channel = %channel.config.id,
                            status = status,
                            tier = ?action.tier,
                            "upstream error classified by 4-tier governance"
                        );

                        if action.unbind_affinity {
                            self.router
                                .unbind_affinity(&decision.actual_model, &decision.prefix_key);
                        }

                        if action.retry_same_channel {
                            l1_attempt += 1;
                            if let Some(delay) = action.backoff {
                                tokio::time::sleep(delay).await;
                            }
                            continue;
                        }

                        if let Some(cooldown) = action.channel_cooldown {
                            match action.tier {
                                ErrorTier::L3QuotaOrAuth => channel.mark_quarantined(cooldown),
                                _ => channel.mark_cooldown(cooldown),
                            }
                        }

                        if action.failover_other_channel {
                            excluded_channels.push(channel.config.id.clone());
                            break;
                        }

                        if cloud_reserved {
                            self.cloud
                                .settle_billing(&request_id, &upstream_model, 0, 0, 0, true)
                                .await;
                        }
                        return Err(GatewayError::UpstreamError {
                            status,
                            body: err_body,
                            tier: action.tier,
                        });
                    }
                    Err(e) => {
                        drop(inflight_guard);
                        error!(channel = %channel.config.id, error = %e, "upstream network failure");
                        if l1_attempt < self.config.error_governance.l1_max_retries {
                            l1_attempt += 1;
                            tokio::time::sleep(Duration::from_millis(
                                self.config.error_governance.l1_base_backoff_ms,
                            ))
                            .await;
                            continue;
                        }
                        channel.mark_cooldown(Duration::from_secs(
                            self.config.error_governance.l2_cooldown_secs,
                        ));
                        excluded_channels.push(channel.config.id.clone());
                        break;
                    }
                }
            }
        }

        if cloud_reserved {
            self.cloud
                .settle_billing(&request_id, &canonical.model, 0, 0, 0, true)
                .await;
        }
        Err(GatewayError::NoAvailableChannel(canonical.model.clone()))
    }
}

fn protocol_label(protocol: UpstreamProtocol) -> &'static str {
    match protocol {
        UpstreamProtocol::Anthropic => "anthropic",
        UpstreamProtocol::OpenAi => "openai",
        UpstreamProtocol::OpenAiResponses => "openai_responses",
        UpstreamProtocol::Gemini => "gemini",
    }
}

fn stream_event_label(ev: &CanonicalStreamEvent) -> &'static str {
    match ev {
        CanonicalStreamEvent::MessageStart { .. } => "message_start",
        CanonicalStreamEvent::ContentBlockStart { .. } => "content_block_start",
        CanonicalStreamEvent::ContentBlockStop { .. } => "content_block_stop",
        CanonicalStreamEvent::TextDelta { .. } => "text_delta",
        CanonicalStreamEvent::ThinkingDelta { .. } => "thinking_delta",
        CanonicalStreamEvent::SignatureDelta { .. } => "signature_delta",
        CanonicalStreamEvent::ToolCallStart { .. } => "tool_call_start",
        CanonicalStreamEvent::ToolCallDelta { .. } => "tool_call_delta",
        CanonicalStreamEvent::UsageDelta { .. } => "usage_delta",
        CanonicalStreamEvent::MessageStop { .. } => "message_stop",
        CanonicalStreamEvent::Error { .. } => "error",
    }
}

fn summarize_canonical_ir(req: &CanonicalRequest) -> String {
    let tool_names: Vec<&str> = req.tools.iter().map(|t| t.name.as_str()).collect();
    json!({
        "model": req.model,
        "stream": req.stream,
        "system_parts": req.system.len(),
        "messages": req.messages.len(),
        "tools": tool_names,
        "has_cache_breakpoints": req.has_cache_breakpoints(),
        "has_reasoning": req.reasoning.is_some(),
        "has_thought_signature": req.opaque_state.thought_signature.is_some(),
        "session_id": req.session_id,
        "task_stage": req.task_stage,
    })
    .to_string()
}

fn upstream_endpoint(
    base_url: &str,
    protocol: UpstreamProtocol,
    upstream_model: &str,
    stream: bool,
) -> String {
    let trimmed = crate::config::normalize_base_url(base_url);
    match protocol {
        UpstreamProtocol::Anthropic => format!("{}/v1/messages", trimmed),
        UpstreamProtocol::OpenAi => format!("{}/v1/chat/completions", trimmed),
        UpstreamProtocol::OpenAiResponses => format!("{}/v1/responses", trimmed),
        UpstreamProtocol::Gemini => {
            if stream {
                format!(
                    "{}/v1beta/models/{}:streamGenerateContent?alt=sse",
                    trimmed, upstream_model
                )
            } else {
                format!("{}/v1beta/models/{}:generateContent", trimmed, upstream_model)
            }
        }
    }
}

/// Deterministic Canonical IR token estimator used by `/v1/messages/count_tokens` when upstream
/// does not expose Anthropic's native count_tokens endpoint (e.g. OpenAI / Gemini upstreams).
fn estimate_canonical_input_tokens(req: &CanonicalRequest) -> u64 {
    let mut chars = 0usize;
    let count_part = |p: &ContentPart| -> usize {
        match p {
            ContentPart::Text { text, .. } => text.len(),
            ContentPart::Thinking { thinking, .. } => thinking.len(),
            ContentPart::ToolCall {
                name, arguments, ..
            } => name.len() + serde_json::to_string(arguments).map(|s| s.len()).unwrap_or(0),
            ContentPart::ToolResult { content, .. } => {
                serde_json::to_string(content).map(|s| s.len()).unwrap_or(0)
            }
            ContentPart::Opaque { raw } => {
                serde_json::to_string(raw).map(|s| s.len()).unwrap_or(0)
            }
        }
    };
    for p in &req.system {
        chars += count_part(p);
    }
    for m in &req.messages {
        chars += 12; // per-message framing overhead
        for p in &m.parts {
            chars += count_part(p);
        }
    }
    for t in &req.tools {
        chars += t.name.len()
            + t.description.as_deref().map(str::len).unwrap_or(0)
            + serde_json::to_string(&t.parameters)
                .map(|s| s.len())
                .unwrap_or(0);
    }
    ((chars as u64) / 4).max(1)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stream_normalizer_synthesizes_message_start_and_block_boundaries() {
        let mut norm = StreamEventNormalizer::new("msg_synth".into(), "gpt-4o-mini".into());
        let ev1 = norm.normalize(CanonicalStreamEvent::TextDelta {
            index: 0,
            text: "hi".into(),
        });
        assert!(matches!(ev1[0], CanonicalStreamEvent::MessageStart { .. }));
        assert!(matches!(
            ev1[1],
            CanonicalStreamEvent::ContentBlockStart { index: 0, .. }
        ));
        assert!(matches!(ev1[2], CanonicalStreamEvent::TextDelta { .. }));

        let ev2 = norm.normalize(CanonicalStreamEvent::MessageStop {
            stop_reason: Some("stop".into()),
        });
        assert!(matches!(
            ev2[0],
            CanonicalStreamEvent::ContentBlockStop { index: 0 }
        ));
        assert!(matches!(ev2[1], CanonicalStreamEvent::MessageStop { .. }));
    }
}
