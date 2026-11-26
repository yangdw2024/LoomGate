use axum::{
    body::Body,
    extract::{Path, Query, Request, State},
    http::{HeaderMap, HeaderName, HeaderValue, StatusCode},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{delete, get, post},
    Json, Router,
};
use gateway_core::{
    discover_local_providers, AuthSettings, ChannelConfig, CliTakeoverManager,
    DiscoveredLocalProvider, GatewayConfig, GatewayEngine, GatewayError, GatewayResponse,
    RouteMode, UpstreamProtocol,
};
use serde::Deserialize;
use serde_json::{json, Value};
use std::path::PathBuf;
use std::sync::Arc;
use tracing::{error, info};

#[derive(Clone)]
struct AppState {
    engine: Arc<GatewayEngine>,
    takeover: Arc<CliTakeoverManager>,
}

#[derive(Deserialize)]
struct ForkRequest {
    target_model: String,
}

#[derive(Deserialize)]
struct McpCallRequest {
    tool: String,
    #[serde(default)]
    arguments: Value,
}

#[derive(Deserialize)]
struct CloudLoginRequest {
    control_plane_url: String,
    email: String,
    password: String,
}

#[derive(Deserialize)]
struct ToggleChannelRequest {
    enabled: bool,
}

#[derive(Deserialize)]
struct DiscoverLocalQuery {
    #[serde(default)]
    ports: Option<String>,
}

#[derive(Deserialize)]
struct ImportLocalRequest {
    provider: DiscoveredLocalProvider,
    #[serde(default)]
    channel_id: Option<String>,
    #[serde(default)]
    offline_fallback: bool,
}

#[derive(Deserialize)]
struct SavingsQuery {
    #[serde(default = "default_savings_days")]
    days: u32,
}

fn default_savings_days() -> u32 {
    30
}

fn extract_admin_credential(headers: &HeaderMap) -> Option<&str> {
    if let Some(p) = headers.get("x-admin-password").and_then(|v| v.to_str().ok()) {
        return Some(p);
    }
    headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.strip_prefix("Bearer "))
}

async fn require_admin_auth(
    State(state): State<AppState>,
    req: Request,
    next: Next,
) -> Response {
    if !state
        .engine
        .verify_admin_auth(extract_admin_credential(req.headers()))
    {
        return (
            StatusCode::UNAUTHORIZED,
            Json(json!({
                "error": {
                    "type": "admin_authentication_error",
                    "message": "invalid or missing admin password (supply x-admin-password or Bearer token)"
                }
            })),
        )
            .into_response();
    }
    next.run(req).await
}

fn resolve_persist_path(loaded_config_path: &str) -> PathBuf {
    if let Ok(explicit) = std::env::var("GATEWAY_PERSIST_CONFIG") {
        if !explicit.trim().is_empty() {
            return PathBuf::from(explicit);
        }
    }
    let loaded = PathBuf::from(loaded_config_path);
    match loaded.parent() {
        Some(dir) if !dir.as_os_str().is_empty() => dir.join("config.local.toml"),
        _ => PathBuf::from("config.local.toml"),
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "gateway_daemon=info,gateway_core=info".into()),
        )
        .init();

    let config_path = std::env::args()
        .nth(1)
        .or_else(|| std::env::var("GATEWAY_CONFIG").ok())
        .unwrap_or_else(|| "config.example.toml".to_string());

    let db_path = std::env::var("GATEWAY_DB_PATH")
        .unwrap_or_else(|_| ".edge-gateway/trajectory.db".to_string());

    let config = GatewayConfig::from_file(&config_path)?;
    let bind_addr = config.server.bind_addr.clone();
    let gateway_url = format!("http://{}", bind_addr);
    let engine = Arc::new(GatewayEngine::new_with_sqlite(config, &db_path)?);
    engine.set_persist_config_path(resolve_persist_path(&config_path));

    let takeover = Arc::new(
        CliTakeoverManager::new(gateway_url)
            .unwrap_or_else(|_| CliTakeoverManager::with_home_dir(PathBuf::from("."), "http://127.0.0.1:8787")),
    );
    let state = AppState { engine, takeover };

    let admin_routes = Router::new()
        .route("/__admin/reset", post(admin_reset))
        .route("/api/sessions", get(list_sessions))
        .route("/api/sessions/:session_id/steps", get(get_session_steps))
        .route("/api/steps/:step_id/fork", post(fork_step))
        .route("/api/middleware", get(list_middleware))
        .route("/api/mcp/call", post(call_mcp_tool))
        .route("/api/cloud/status", get(cloud_status))
        .route("/api/cloud/login", post(cloud_login))
        .route("/api/cloud/sync", post(cloud_sync))
        .route("/api/cloud/logout", post(cloud_logout))
        .route("/api/channels", get(list_channels).post(upsert_channel))
        .route("/api/channels/test", post(test_channel))
        .route("/api/channels/:id", delete(delete_channel))
        .route("/api/channels/:id/toggle", post(toggle_channel))
        .route("/api/local/discover", get(local_discover))
        .route("/api/local/import", post(local_import))
        .route("/api/analytics/savings", get(analytics_savings))
        .route("/api/clients", get(list_clients))
        .route("/api/auth/settings", get(get_auth_settings).post(set_auth_settings))
        .route_layer(middleware::from_fn_with_state(
            state.clone(),
            require_admin_auth,
        ));

    let app = Router::new()
        .route("/healthz", get(healthz))
        .route("/metrics/channels", get(channel_metrics))
        .route("/v1/messages", post(handle_anthropic))
        .route("/v1/messages/count_tokens", post(handle_anthropic_count_tokens))
        .route("/v1/chat/completions", post(handle_openai))
        .route("/v1beta/models/:model_action", post(handle_gemini))
        .merge(admin_routes)
        .with_state(state);

    let listener = tokio::net::TcpListener::bind(&bind_addr).await?;
    info!(bind_addr = %bind_addr, db = %db_path, "LoomGate local daemon listening (v0.4.0)");
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await?;
    Ok(())
}

async fn healthz(State(state): State<AppState>) -> impl IntoResponse {
    Json(json!({
        "status": "ok",
        "version": "0.4.0",
        "kernel": "gateway-core",
        "route_mode": format!("{:?}", state.engine.active_route_mode()).to_lowercase()
    }))
}

async fn channel_metrics(State(state): State<AppState>) -> impl IntoResponse {
    Json(json!({ "channels": state.engine.channel_snapshots() }))
}

async fn admin_reset(State(state): State<AppState>) -> impl IntoResponse {
    state.engine.reset_runtime_state();
    Json(json!({ "ok": true }))
}

async fn list_channels(State(state): State<AppState>) -> impl IntoResponse {
    Json(json!({
        "snapshots": state.engine.channel_snapshots(),
        "configs": state.engine.list_channel_configs(),
    }))
}

async fn upsert_channel(
    State(state): State<AppState>,
    Json(cfg): Json<ChannelConfig>,
) -> impl IntoResponse {
    match state.engine.upsert_channel(cfg) {
        Ok(snap) => Json(json!({ "ok": true, "channel": snap })).into_response(),
        Err(e) => (
            StatusCode::BAD_REQUEST,
            Json(json!({ "ok": false, "error": e.to_string() })),
        )
            .into_response(),
    }
}

async fn delete_channel(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    match state.engine.delete_channel(&id) {
        Ok(deleted) => Json(json!({ "ok": true, "deleted": deleted })).into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "ok": false, "error": e.to_string() })),
        )
            .into_response(),
    }
}

async fn toggle_channel(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(req): Json<ToggleChannelRequest>,
) -> impl IntoResponse {
    match state.engine.set_channel_enabled(&id, req.enabled) {
        Ok(snap) => Json(json!({ "ok": true, "channel": snap })).into_response(),
        Err(e) => (
            StatusCode::NOT_FOUND,
            Json(json!({ "ok": false, "error": e.to_string() })),
        )
            .into_response(),
    }
}

async fn test_channel(
    State(state): State<AppState>,
    Json(cfg): Json<ChannelConfig>,
) -> impl IntoResponse {
    let result = state.engine.test_channel_connection(cfg).await;
    Json(result)
}

async fn local_discover(Query(q): Query<DiscoverLocalQuery>) -> impl IntoResponse {
    let custom_ports: Vec<u16> = q
        .ports
        .unwrap_or_default()
        .split(',')
        .filter_map(|s| s.trim().parse::<u16>().ok())
        .collect();
    let providers = discover_local_providers(&custom_ports).await;
    Json(json!({ "providers": providers }))
}

async fn local_import(
    State(state): State<AppState>,
    Json(req): Json<ImportLocalRequest>,
) -> impl IntoResponse {
    match state
        .engine
        .import_local_provider(&req.provider, req.channel_id, req.offline_fallback)
    {
        Ok(channel) => Json(json!({ "ok": true, "channel": channel })).into_response(),
        Err(e) => (
            StatusCode::BAD_REQUEST,
            Json(json!({ "ok": false, "error": e.to_string() })),
        )
            .into_response(),
    }
}

async fn analytics_savings(
    State(state): State<AppState>,
    Query(q): Query<SavingsQuery>,
) -> impl IntoResponse {
    match state.engine.get_savings_summary(q.days) {
        Ok(summary) => Json(summary).into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": e.to_string() })),
        )
            .into_response(),
    }
}

async fn list_clients(State(state): State<AppState>) -> impl IntoResponse {
    let api_key = state.engine.auth_settings().api_key;
    Json(json!({
        "profiles": state.takeover.list_profiles(api_key.as_deref())
    }))
}

async fn get_auth_settings(State(state): State<AppState>) -> impl IntoResponse {
    Json(state.engine.auth_settings())
}

async fn set_auth_settings(
    State(state): State<AppState>,
    Json(req): Json<AuthSettings>,
) -> impl IntoResponse {
    match state
        .engine
        .set_auth_settings(req.api_key, req.admin_password)
    {
        Ok(updated) => Json(json!({ "ok": true, "auth": updated })).into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "ok": false, "error": e.to_string() })),
        )
            .into_response(),
    }
}

async fn list_sessions(State(state): State<AppState>) -> impl IntoResponse {
    match state.engine.list_sessions(50) {
        Ok(sessions) => Json(json!({ "sessions": sessions })).into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": e.to_string() })),
        )
            .into_response(),
    }
}

async fn get_session_steps(
    State(state): State<AppState>,
    Path(session_id): Path<String>,
) -> impl IntoResponse {
    match state.engine.get_session_steps(&session_id) {
        Ok(steps) => Json(json!({ "steps": steps })).into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": e.to_string() })),
        )
            .into_response(),
    }
}

async fn fork_step(
    State(state): State<AppState>,
    Path(step_id): Path<String>,
    Json(req): Json<ForkRequest>,
) -> impl IntoResponse {
    match state
        .engine
        .fork_and_diff_step(&step_id, req.target_model)
        .await
    {
        Ok(diff) => Json(diff).into_response(),
        Err(e) => (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": e.to_string() })),
        )
            .into_response(),
    }
}

async fn list_middleware(State(state): State<AppState>) -> impl IntoResponse {
    Json(json!({
        "layers": state.engine.middleware_layers(),
        "mcp_tools": state.engine.mcp_bridge().aggregated_tools(),
    }))
}

async fn call_mcp_tool(
    State(state): State<AppState>,
    Json(req): Json<McpCallRequest>,
) -> impl IntoResponse {
    match state
        .engine
        .mcp_bridge()
        .call_tool(&req.tool, req.arguments)
        .await
    {
        Ok(result) => Json(json!({ "ok": true, "result": result })).into_response(),
        Err(e) => (
            StatusCode::BAD_GATEWAY,
            Json(json!({ "ok": false, "error": e })),
        )
            .into_response(),
    }
}

async fn cloud_status(State(state): State<AppState>) -> impl IntoResponse {
    Json(state.engine.cloud_status())
}

async fn cloud_login(
    State(state): State<AppState>,
    Json(req): Json<CloudLoginRequest>,
) -> impl IntoResponse {
    match state
        .engine
        .cloud_login(&req.control_plane_url, &req.email, &req.password)
        .await
    {
        Ok(status) => Json(json!({ "ok": true, "cloud": status })).into_response(),
        Err(e) => (
            StatusCode::UNAUTHORIZED,
            Json(json!({ "ok": false, "error": e })),
        )
            .into_response(),
    }
}

async fn cloud_sync(State(state): State<AppState>) -> impl IntoResponse {
    match state.engine.cloud_sync_team_pool().await {
        Ok(status) => Json(json!({ "ok": true, "cloud": status })).into_response(),
        Err(e) => (
            StatusCode::BAD_REQUEST,
            Json(json!({ "ok": false, "error": e })),
        )
            .into_response(),
    }
}

async fn cloud_logout(State(state): State<AppState>) -> impl IntoResponse {
    state.engine.cloud_logout();
    Json(json!({ "ok": true, "cloud": state.engine.cloud_status() }))
}

async fn handle_anthropic(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(payload): Json<Value>,
) -> Response {
    dispatch(state, UpstreamProtocol::Anthropic, headers, payload).await
}

async fn handle_anthropic_count_tokens(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(payload): Json<Value>,
) -> Response {
    if !state.engine.verify_api_auth(extract_inbound_token(&headers)) {
        let body = state.engine.format_error(
            UpstreamProtocol::Anthropic,
            "authentication_error",
            "invalid or missing local gateway API key",
        );
        return (StatusCode::UNAUTHORIZED, Json(body)).into_response();
    }
    match state.engine.count_tokens(payload).await {
        Ok(res) => Json(res).into_response(),
        Err(e) => (
            StatusCode::BAD_REQUEST,
            Json(state.engine.format_error(
                UpstreamProtocol::Anthropic,
                "invalid_request_error",
                &e.to_string(),
            )),
        )
            .into_response(),
    }
}

async fn handle_openai(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(payload): Json<Value>,
) -> Response {
    dispatch(state, UpstreamProtocol::OpenAi, headers, payload).await
}

async fn handle_gemini(
    State(state): State<AppState>,
    Path(model_action): Path<String>,
    headers: HeaderMap,
    Json(mut payload): Json<Value>,
) -> Response {
    let Some((model, action)) = model_action.rsplit_once(':') else {
        let body = state.engine.format_error(
            UpstreamProtocol::Gemini,
            "invalid_request_error",
            &format!(
                "invalid Gemini endpoint '{}': expected '<model>:generateContent' or '<model>:streamGenerateContent'",
                model_action
            ),
        );
        return (StatusCode::BAD_REQUEST, Json(body)).into_response();
    };
    let stream = match action {
        "generateContent" => false,
        "streamGenerateContent" => true,
        other => {
            let body = state.engine.format_error(
                UpstreamProtocol::Gemini,
                "invalid_request_error",
                &format!("unsupported Gemini action '{}'", other),
            );
            return (StatusCode::BAD_REQUEST, Json(body)).into_response();
        }
    };
    if let Some(obj) = payload.as_object_mut() {
        obj.insert("model".into(), json!(model));
        obj.insert("stream".into(), json!(stream));
    }
    dispatch(state, UpstreamProtocol::Gemini, headers, payload).await
}

fn extract_inbound_token(headers: &HeaderMap) -> Option<&str> {
    if let Some(k) = headers.get("x-api-key").and_then(|v| v.to_str().ok()) {
        return Some(k);
    }
    if let Some(k) = headers.get("x-goog-api-key").and_then(|v| v.to_str().ok()) {
        return Some(k);
    }
    headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.strip_prefix("Bearer "))
}

async fn dispatch(
    state: AppState,
    protocol: UpstreamProtocol,
    headers: HeaderMap,
    payload: Value,
) -> Response {
    if !state.engine.verify_api_auth(extract_inbound_token(&headers)) {
        let body = state.engine.format_error(
            protocol,
            "authentication_error",
            "invalid or missing local gateway API key",
        );
        return (StatusCode::UNAUTHORIZED, Json(body)).into_response();
    }

    let session_id = headers
        .get("x-session-id")
        .and_then(|v| v.to_str().ok())
        .map(String::from);
    let task_stage = headers
        .get("x-gateway-task-stage")
        .and_then(|v| v.to_str().ok())
        .map(String::from);
    let override_mode = headers
        .get("x-gateway-route-mode")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| match s {
            "lock" => Some(RouteMode::Lock),
            "stage_aware" => Some(RouteMode::StageAware),
            _ => None,
        });

    match state
        .engine
        .handle_request(protocol, payload, session_id, task_stage, override_mode)
        .await
    {
        Ok(GatewayResponse::Streaming { meta, stream }) => {
            let mut builder = Response::builder()
                .status(StatusCode::OK)
                .header("content-type", "text/event-stream")
                .header("cache-control", "no-cache");

            for (k, v) in meta.to_headers() {
                if let (Ok(hn), Ok(hv)) = (
                    HeaderName::from_bytes(k.as_bytes()),
                    HeaderValue::from_str(&v),
                ) {
                    builder = builder.header(hn, hv);
                }
            }

            builder
                .body(Body::from_stream(stream))
                .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response())
        }
        Ok(GatewayResponse::Json { meta, body }) => {
            let mut resp = Json(body).into_response();
            for (k, v) in meta.to_headers() {
                if let (Ok(hn), Ok(hv)) = (
                    HeaderName::from_bytes(k.as_bytes()),
                    HeaderValue::from_str(&v),
                ) {
                    resp.headers_mut().insert(hn, hv);
                }
            }
            resp
        }
        Err(err) => {
            error!(error = %err, "request failed");
            match err {
                GatewayError::UpstreamError { status, body, tier } => {
                    let code = StatusCode::from_u16(status).unwrap_or(StatusCode::BAD_GATEWAY);
                    let err_type = if status == 400 || status == 422 {
                        "invalid_request_error"
                    } else {
                        "api_error"
                    };
                    let mut resp = (
                        code,
                        Json(state.engine.format_error(protocol, err_type, &body)),
                    )
                        .into_response();
                    if let Ok(hv) = HeaderValue::from_str(&format!("{:?}", tier)) {
                        resp.headers_mut()
                            .insert(HeaderName::from_static("x-gateway-error-tier"), hv);
                    }
                    resp
                }
                GatewayError::MiddlewareRejected {
                    status,
                    error_type,
                    message,
                    middleware,
                } => {
                    let code = StatusCode::from_u16(status).unwrap_or(StatusCode::FORBIDDEN);
                    let mut resp = (
                        code,
                        Json(state.engine.format_error(protocol, &error_type, &message)),
                    )
                        .into_response();
                    if let Ok(hv) = HeaderValue::from_str(middleware) {
                        resp.headers_mut()
                            .insert(HeaderName::from_static("x-gateway-middleware"), hv);
                    }
                    resp
                }
                GatewayError::NoAvailableChannel(model) => (
                    StatusCode::SERVICE_UNAVAILABLE,
                    Json(state.engine.format_error(
                        protocol,
                        "overloaded_error",
                        &format!("no healthy channel for model {}", model),
                    )),
                )
                    .into_response(),
                other => (
                    StatusCode::BAD_REQUEST,
                    Json(state.engine.format_error(
                        protocol,
                        "invalid_request_error",
                        &other.to_string(),
                    )),
                )
                    .into_response(),
            }
        }
    }
}

async fn shutdown_signal() {
    let _ = tokio::signal::ctrl_c().await;
    info!("received Ctrl+C, shutting down LoomGate daemon cleanly");
}
