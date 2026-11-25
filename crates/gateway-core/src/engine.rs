use crate::codec::{codec_for, CodecError};
use crate::config::{GatewayConfig, RouteMode, UpstreamProtocol};
use crate::error_gov::{classify_upstream_error, ErrorTier};
use crate::ir::{CanonicalRequest, CanonicalStreamEvent, CanonicalUsage, ContentPart};
use crate::middleware::guardrail::GuardrailMiddleware;
use crate::middleware::ingress::IngressRateLimitMiddleware;
use crate::middleware::mcp_bridge::McpBridgeMiddleware;
use crate::middleware::memory::ContextMemoryMiddleware;
use crate::middleware::{MiddlewareContext, MiddlewarePipeline};
use crate::router::{ChannelRuntime, ChannelSnapshot, StickyP2cRouter};
use crate::signature_cache::SignatureCache;
use crate::sse_fsm::{CancelPropagatingStream, SseFrameParser, StreamMetrics};
use crate::trajectory::{
    new_step_id, now_epoch_ms, ForkDiffResult, SessionSummary, TrajectoryError, TrajectoryStep,
    TrajectoryStore,
};
use bytes::Bytes;
use futures_util::StreamExt;
use reqwest::Client;
use serde_json::{json, Value};
use std::collections::{BTreeMap, HashMap};
use std::path::Path;
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
    #[error("transport error: {0}")]
    Transport(String),
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
    middleware: MiddlewarePipeline,
    rate_limiter: Arc<IngressRateLimitMiddleware>,
    memory: Arc<ContextMemoryMiddleware>,
    mcp_bridge: Arc<McpBridgeMiddleware>,
    /// Hot-switchable global route mode (0 = Lock, 1 = StageAware) toggled from Tauri Tray.
    active_route_mode: AtomicU8,
    /// Hot-updatable local inbound auth token (empty / None disables local auth).
    local_auth_token: RwLock<Option<String>>,
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

        let initial_token = config
            .server
            .local_auth_token
            .clone()
            .filter(|s| !s.trim().is_empty());

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
        middleware.push(guardrail);
        middleware.push(Arc::clone(&memory) as Arc<dyn crate::middleware::Middleware>);
        middleware.push(Arc::clone(&mcp_bridge) as Arc<dyn crate::middleware::Middleware>);

        Ok(Self {
            config,
            router,
            signatures,
            trajectory,
            middleware,
            rate_limiter,
            memory,
            mcp_bridge,
            active_route_mode: AtomicU8::new(mode_byte),
            local_auth_token: RwLock::new(initial_token),
            http,
        })
    }

    pub fn middleware_layers(&self) -> Vec<&'static str> {
        self.middleware.layer_names()
    }

    pub fn mcp_bridge(&self) -> Arc<McpBridgeMiddleware> {
        Arc::clone(&self.mcp_bridge)
    }

    pub fn verify_inbound_auth(&self, provided_token: Option<&str>) -> bool {
        let guard = self
            .local_auth_token
            .read()
            .unwrap_or_else(|e| e.into_inner());
        match guard.as_deref() {
            None | Some("") => true,
            Some(expected) => provided_token == Some(expected),
        }
    }

    pub fn local_auth_token(&self) -> Option<String> {
        self.local_auth_token
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    pub fn set_local_auth_token(&self, token: Option<String>) {
        let normalized = token.and_then(|s| {
            let trimmed = s.trim().to_string();
            if trimmed.is_empty() {
                None
            } else {
                Some(trimmed)
            }
        });
        let mut guard = self
            .local_auth_token
            .write()
            .unwrap_or_else(|e| e.into_inner());
        *guard = normalized;
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
            Err(rej) => Err(GatewayError::MiddlewareRejected {
                status: rej.status,
                error_type: rej.error_type,
                message: rej.message,
                middleware: rej.middleware,
            }),
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
        let max_failovers = self.config.channels.len().max(1);

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

            let mut l1_attempt = 0u32;
            loop {
                let started = Instant::now();
                let inflight_guard = InflightGuard::enter(Arc::clone(&channel));

                let mut req_builder = self.http.post(&endpoint).json(&encoded_body);
                req_builder = match channel.config.protocol {
                    UpstreamProtocol::Anthropic => req_builder
                        .header("x-api-key", &channel.config.api_key)
                        .header("anthropic-version", "2023-06-01")
                        .header("X-Gateway-Prefix-Key", &decision.prefix_key),
                    UpstreamProtocol::OpenAi => req_builder
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

                        let meta = ResponseMetadata {
                            request_id: request_id.clone(),
                            step_id: step_id.clone(),
                            actual_model: upstream_model.clone(),
                            channel_id: channel.config.id.clone(),
                            route_mode: decision.mode,
                            sticky_hit: decision.sticky_hit,
                            prefix_key: decision.prefix_key.clone(),
                            extra_headers: BTreeMap::new(),
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
                            UpstreamProtocol::Gemini => "gemini",
                        }
                        .to_string();
                        let mode_str = match decision.mode {
                            RouteMode::Lock => "lock",
                            RouteMode::StageAware => "stage_aware",
                        }
                        .to_string();

                        if canonical.stream {
                            let cancel_token = CancellationToken::new();
                            let metrics = Arc::new(StreamMetrics::default());
                            let (tx, rx) = mpsc::channel::<Result<Bytes, std::io::Error>>(32);

                            let bg_token = cancel_token.clone();
                            let bg_metrics = Arc::clone(&metrics);
                            let bg_signatures = Arc::clone(&self.signatures);
                            let bg_traj = Arc::clone(&self.trajectory);
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

                                loop {
                                    tokio::select! {
                                        biased;
                                        _ = bg_token.cancelled() => {
                                            drop(byte_stream);
                                            status_label = "aborted_by_client";
                                            break;
                                        }
                                        maybe_chunk = byte_stream.next() => {
                                            match maybe_chunk {
                                                Some(Ok(chunk)) => {
                                                    let frames = parser.push_bytes(&chunk);
                                                    for frame in frames {
                                                        let raw_events = up_codec.decode_sse_frame(frame.event.as_deref(), &frame.data);
                                                        for raw_ev in raw_events {
                                                            for ev in normalizer.normalize(raw_ev) {
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
                                                                        break;
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
                                                    break;
                                                }
                                                None => {
                                                    for frame in parser.finish() {
                                                        let raw_events = up_codec.decode_sse_frame(frame.event.as_deref(), &frame.data);
                                                        for raw_ev in raw_events {
                                                            for ev in normalizer.normalize(raw_ev) {
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
                                                    break;
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
                                    step_id: step_id_bg,
                                    workspace: traj_workspace,
                                    session_id: traj_session,
                                    parent_step_id: parent_bg,
                                    turn_index,
                                    protocol: proto_str,
                                    channel_id,
                                    requested_model,
                                    actual_model,
                                    route_mode: mode_str,
                                    sticky_hit,
                                    ttft_ms,
                                    input_tokens: acc_usage.input_tokens,
                                    cache_read_tokens: acc_usage.cache_read_input_tokens,
                                    output_tokens: final_out_tokens,
                                    status: status_label.into(),
                                    request_json: req_json_bg,
                                    response_text: acc_text,
                                    tool_calls_json: serde_json::to_string(&acc_tools).unwrap_or_else(|_| "[]".into()),
                                    created_at_ms: now_epoch_ms(),
                                };
                                let _ = bg_traj.append_step(&step);
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
                                session_id: traj_session,
                                parent_step_id: parent_step_id.clone(),
                                turn_index,
                                protocol: proto_str,
                                channel_id: channel.config.id.clone(),
                                requested_model: canonical.model.clone(),
                                actual_model: upstream_model.clone(),
                                route_mode: mode_str,
                                sticky_hit: decision.sticky_hit,
                                ttft_ms,
                                input_tokens: canonical_resp.usage.input_tokens,
                                cache_read_tokens: canonical_resp.usage.cache_read_input_tokens,
                                output_tokens: canonical_resp.usage.output_tokens,
                                status: "completed".into(),
                                request_json: raw_request_json,
                                response_text: canonical_resp.concatenated_text(),
                                tool_calls_json: "[]".into(),
                                created_at_ms: now_epoch_ms(),
                            };
                            let _ = self.trajectory.append_step(&step);

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

        Err(GatewayError::NoAvailableChannel(canonical.model.clone()))
    }
}

fn upstream_endpoint(
    base_url: &str,
    protocol: UpstreamProtocol,
    upstream_model: &str,
    stream: bool,
) -> String {
    let trimmed = base_url.trim_end_matches('/');
    match protocol {
        UpstreamProtocol::Anthropic => format!("{}/v1/messages", trimmed),
        UpstreamProtocol::OpenAi => format!("{}/v1/chat/completions", trimmed),
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
