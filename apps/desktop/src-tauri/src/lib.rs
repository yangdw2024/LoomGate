use axum::{
    body::Body,
    extract::{Path, State},
    http::{HeaderMap, HeaderName, HeaderValue, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use gateway_core::{
    discover_local_providers, AuthSettings, ChannelConfig, ChannelSnapshot, ChannelTestResult,
    CliIntegrationStatus, CliTakeoverManager, CliTool, ClientProfileStatus, CloudSessionStatus,
    DiscoveredLocalProvider, ForkDiffExport, ForkDiffResult, GatewayConfig, GatewayEngine,
    GatewayError, GatewayResponse, RouteMode, SavingsSummary, SessionSummary, TrajectoryStep,
    UpstreamProtocol,
};
use serde_json::{json, Value};
use std::path::PathBuf;
use std::sync::Arc;
use tauri::{
    image::Image,
    menu::{Menu, MenuItem},
    tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent},
    AppHandle, Emitter, Manager, RunEvent, State as TauriState,
};
use tokio::sync::watch;
use tracing::{error, info};

pub struct DesktopState {
    pub engine: Arc<GatewayEngine>,
    pub takeover: CliTakeoverManager,
    pub bind_addr: String,
    pub shutdown_tx: watch::Sender<bool>,
}

#[tauri::command]
fn get_gateway_status(state: TauriState<'_, DesktopState>) -> Value {
    json!({
        "bind_addr": state.bind_addr,
        "route_mode": match state.engine.active_route_mode() {
            RouteMode::Lock => "lock",
            RouteMode::StageAware => "stage_aware",
        },
        "channels": state.engine.channel_snapshots(),
    })
}

#[tauri::command]
fn get_channel_snapshots(state: TauriState<'_, DesktopState>) -> Vec<ChannelSnapshot> {
    state.engine.channel_snapshots()
}

/// Renders a crisp 32x32 RGBA LoomGate tray icon with a live bottom-right status LED:
/// - Lock mode: Blue LED (#388bfd)
/// - Stage-Aware mode: Emerald LED (#3fb950)
fn tray_icon_for_mode(mode: RouteMode) -> Image<'static> {
    const SIZE: u32 = 32;
    let mut rgba = Vec::with_capacity((SIZE * SIZE * 4) as usize);
    let (led_r, led_g, led_b) = match mode {
        RouteMode::Lock => (56u8, 139u8, 253u8),
        RouteMode::StageAware => (63u8, 185u8, 80u8),
    };

    for y in 0..SIZE {
        let ny = (y as f32 + 0.5) / SIZE as f32;
        for x in 0..SIZE {
            let nx = (x as f32 + 0.5) / SIZE as f32;
            let ax = (nx - 0.5).abs() * 2.0;
            let ay = (ny - 0.5).abs() * 2.0;
            let r_sq = (ax.powi(4) + ay.powi(4)).powf(0.25);

            // Bottom-right status indicator LED (with dark halo ring)
            let dx = nx - 0.78;
            let dy = ny - 0.78;
            let led_dist = (dx * dx + dy * dy).sqrt();
            if led_dist <= 0.16 {
                rgba.extend_from_slice(&[led_r, led_g, led_b, 255]);
                continue;
            }
            if led_dist <= 0.21 {
                rgba.extend_from_slice(&[13, 17, 23, 255]);
                continue;
            }

            if r_sq > 0.94 {
                rgba.extend_from_slice(&[0, 0, 0, 0]);
                continue;
            }

            let t = 0.5 * nx + 0.5 * ny;
            let mut r = (16.0 + 24.0 * t) as u8;
            let mut g = (24.0 + 62.0 * t) as u8;
            let mut b = (58.0 + 112.0 * t) as u8;

            let left_pillar = (0.22..=0.33).contains(&nx) && (0.24..=0.76).contains(&ny);
            let right_pillar = (0.65..=0.76).contains(&nx) && (0.24..=0.60).contains(&ny);
            let top_beam = (0.22..=0.76).contains(&nx) && (0.20..=0.30).contains(&ny);
            let mid_weave = (0.28..=0.68).contains(&nx) && (0.46..=0.55).contains(&ny);
            let base_bar = (0.22..=0.50).contains(&nx) && (0.66..=0.76).contains(&ny);

            if left_pillar || right_pillar || top_beam || base_bar {
                r = 242;
                g = 246;
                b = 255;
            } else if mid_weave {
                r = 56;
                g = 189;
                b = 248;
            }
            rgba.extend_from_slice(&[r, g, b, 255]);
        }
    }

    Image::new_owned(rgba, SIZE, SIZE)
}

fn sync_tray_visuals(app: &AppHandle, bind_addr: &str, mode: RouteMode) {
    if let Some(tray) = app.tray_by_id("main-tray") {
        let label = match mode {
            RouteMode::Lock => "Lock",
            RouteMode::StageAware => "Stage-Aware",
        };
        let _ = tray.set_tooltip(Some(format!("LoomGate ({}) — {}", bind_addr, label)));
        let _ = tray.set_icon(Some(tray_icon_for_mode(mode)));
    }
}

#[tauri::command]
fn set_route_mode(
    app: AppHandle,
    state: TauriState<'_, DesktopState>,
    mode: String,
) -> Result<String, String> {
    let parsed = match mode.as_str() {
        "lock" => RouteMode::Lock,
        "stage_aware" => RouteMode::StageAware,
        other => return Err(format!("unsupported route mode: {}", other)),
    };
    state.engine.set_route_mode(parsed);
    sync_tray_visuals(&app, &state.bind_addr, parsed);
    Ok(mode)
}

#[tauri::command]
fn get_local_auth_token(state: TauriState<'_, DesktopState>) -> Option<String> {
    state.engine.local_auth_token()
}

#[tauri::command]
fn set_local_auth_token(
    state: TauriState<'_, DesktopState>,
    token: Option<String>,
) -> Option<String> {
    state.engine.set_local_auth_token(token);
    state.engine.local_auth_token()
}

#[tauri::command]
fn get_autostart_enabled() -> bool {
    #[cfg(target_os = "windows")]
    {
        std::process::Command::new("reg")
            .args([
                "query",
                r"HKCU\Software\Microsoft\Windows\CurrentVersion\Run",
                "/v",
                "LoomGate",
            ])
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    }
    #[cfg(not(target_os = "windows"))]
    {
        false
    }
}

#[tauri::command]
fn set_autostart_enabled(enabled: bool) -> Result<bool, String> {
    #[cfg(target_os = "windows")]
    {
        const RUN_KEY: &str = r"HKCU\Software\Microsoft\Windows\CurrentVersion\Run";
        if enabled {
            let exe = std::env::current_exe().map_err(|e| e.to_string())?;
            let quoted = format!("\"{}\"", exe.display());
            let out = std::process::Command::new("reg")
                .args(["add", RUN_KEY, "/v", "LoomGate", "/t", "REG_SZ", "/d", &quoted, "/f"])
                .output()
                .map_err(|e| e.to_string())?;
            if !out.status.success() {
                return Err(String::from_utf8_lossy(&out.stderr).trim().to_string());
            }
            Ok(true)
        } else {
            let _ = std::process::Command::new("reg")
                .args(["delete", RUN_KEY, "/v", "LoomGate", "/f"])
                .output();
            Ok(false)
        }
    }
    #[cfg(not(target_os = "windows"))]
    {
        let _ = enabled;
        Err("autostart is only implemented for Windows in this build".into())
    }
}

#[tauri::command]
fn get_cloud_status(state: TauriState<'_, DesktopState>) -> CloudSessionStatus {
    state.engine.cloud_status()
}

#[tauri::command]
async fn cloud_login(
    state: TauriState<'_, DesktopState>,
    control_plane_url: String,
    email: String,
    password: String,
) -> Result<CloudSessionStatus, String> {
    state
        .engine
        .cloud_login(&control_plane_url, &email, &password)
        .await
}

#[tauri::command]
async fn cloud_sync(state: TauriState<'_, DesktopState>) -> Result<CloudSessionStatus, String> {
    state.engine.cloud_sync_team_pool().await
}

#[tauri::command]
fn cloud_logout(state: TauriState<'_, DesktopState>) -> CloudSessionStatus {
    state.engine.cloud_logout();
    state.engine.cloud_status()
}

#[tauri::command]
fn get_auth_settings(state: TauriState<'_, DesktopState>) -> AuthSettings {
    state.engine.auth_settings()
}

#[tauri::command]
fn set_auth_settings(
    state: TauriState<'_, DesktopState>,
    api_key: Option<String>,
    admin_password: Option<String>,
) -> Result<AuthSettings, String> {
    state
        .engine
        .set_auth_settings(api_key, admin_password)
        .map_err(|e| e.to_string())
}

#[tauri::command]
fn list_channel_configs(state: TauriState<'_, DesktopState>) -> Vec<ChannelConfig> {
    state.engine.list_channel_configs()
}

#[tauri::command]
fn upsert_channel(
    state: TauriState<'_, DesktopState>,
    channel: ChannelConfig,
) -> Result<ChannelSnapshot, String> {
    state
        .engine
        .upsert_channel(channel)
        .map_err(|e| e.to_string())
}

#[tauri::command]
fn delete_channel(state: TauriState<'_, DesktopState>, id: String) -> Result<bool, String> {
    state.engine.delete_channel(&id).map_err(|e| e.to_string())
}

#[tauri::command]
fn toggle_channel(
    state: TauriState<'_, DesktopState>,
    id: String,
    enabled: bool,
) -> Result<ChannelSnapshot, String> {
    state
        .engine
        .set_channel_enabled(&id, enabled)
        .map_err(|e| e.to_string())
}

#[tauri::command]
async fn test_channel_connection(
    state: TauriState<'_, DesktopState>,
    channel: ChannelConfig,
) -> Result<ChannelTestResult, String> {
    Ok(state.engine.test_channel_connection(channel).await)
}

#[tauri::command]
async fn scan_local_providers(
    custom_ports: Option<Vec<u16>>,
) -> Result<Vec<DiscoveredLocalProvider>, String> {
    let ports = custom_ports.unwrap_or_default();
    Ok(discover_local_providers(&ports).await)
}

#[tauri::command]
fn import_local_provider(
    state: TauriState<'_, DesktopState>,
    provider: DiscoveredLocalProvider,
    channel_id: Option<String>,
    offline_fallback: bool,
) -> Result<ChannelSnapshot, String> {
    state
        .engine
        .import_local_provider(&provider, channel_id, offline_fallback)
        .map_err(|e| e.to_string())
}

#[tauri::command]
fn get_savings_summary(
    state: TauriState<'_, DesktopState>,
    days: u32,
) -> Result<SavingsSummary, String> {
    state
        .engine
        .get_savings_summary(days)
        .map_err(|e| e.to_string())
}

#[tauri::command]
fn list_client_profiles(state: TauriState<'_, DesktopState>) -> Vec<ClientProfileStatus> {
    let api_key = state.engine.auth_settings().api_key;
    state.takeover.list_profiles(api_key.as_deref())
}

#[tauri::command]
fn takeover_client_profile(
    state: TauriState<'_, DesktopState>,
    profile_id: String,
) -> Result<ClientProfileStatus, String> {
    let api_key = state.engine.auth_settings().api_key;
    state
        .takeover
        .takeover_by_id(&profile_id, api_key.as_deref())
        .map_err(|e| e.to_string())
}

#[tauri::command]
fn restore_client_profile(
    state: TauriState<'_, DesktopState>,
    profile_id: String,
) -> Result<ClientProfileStatus, String> {
    let api_key = state.engine.auth_settings().api_key;
    state
        .takeover
        .restore_by_id(&profile_id, api_key.as_deref())
        .map_err(|e| e.to_string())
}

#[tauri::command]
fn list_cli_integrations(state: TauriState<'_, DesktopState>) -> Vec<CliIntegrationStatus> {
    state.takeover.list_statuses()
}

#[tauri::command]
fn takeover_cli_config(
    state: TauriState<'_, DesktopState>,
    tool: CliTool,
) -> Result<CliIntegrationStatus, String> {
    state.takeover.takeover(tool).map_err(|e| e.to_string())
}

#[tauri::command]
fn restore_cli_config(
    state: TauriState<'_, DesktopState>,
    tool: CliTool,
) -> Result<CliIntegrationStatus, String> {
    state.takeover.restore(tool).map_err(|e| e.to_string())
}

#[tauri::command]
fn list_sessions(state: TauriState<'_, DesktopState>) -> Result<Vec<SessionSummary>, String> {
    state.engine.list_sessions(50).map_err(|e| e.to_string())
}

#[tauri::command]
fn get_session_steps(
    state: TauriState<'_, DesktopState>,
    session_id: String,
) -> Result<Vec<TrajectoryStep>, String> {
    state
        .engine
        .get_session_steps(&session_id)
        .map_err(|e| e.to_string())
}

#[tauri::command]
async fn fork_and_diff_step(
    state: TauriState<'_, DesktopState>,
    step_id: String,
    target_model: String,
) -> Result<ForkDiffResult, String> {
    state
        .engine
        .fork_and_diff_step(&step_id, target_model)
        .await
        .map_err(|e| e.to_string())
}

#[tauri::command]
fn export_fork_diff_markdown(
    state: TauriState<'_, DesktopState>,
    original_step_id: String,
    forked_step_id: String,
    redact_secrets: bool,
) -> Result<ForkDiffExport, String> {
    state
        .engine
        .export_fork_diff_markdown(&original_step_id, &forked_step_id, redact_secrets)
        .map_err(|e| e.to_string())
}

/// Starts the embedded local HTTP proxy (127.0.0.1:8787) sharing the exact `Arc<GatewayEngine>`
/// managed by Tauri, so CLI tools and the Tray UI share zero-latency in-process state.
async fn run_embedded_proxy(
    engine: Arc<GatewayEngine>,
    bind_addr: String,
    mut shutdown_rx: watch::Receiver<bool>,
) {
    let app = Router::new()
        .route("/healthz", get(proxy_healthz))
        .route("/v1/messages", post(proxy_anthropic))
        .route("/v1/messages/count_tokens", post(proxy_anthropic_count_tokens))
        .route("/v1/chat/completions", post(proxy_openai))
        .route("/v1beta/models/:model_action", post(proxy_gemini))
        .with_state(engine);

    match tokio::net::TcpListener::bind(&bind_addr).await {
        Ok(listener) => {
            info!(bind_addr = %bind_addr, "embedded Tauri edge-gateway proxy listening");
            let graceful = axum::serve(listener, app).with_graceful_shutdown(async move {
                while !*shutdown_rx.borrow() {
                    if shutdown_rx.changed().await.is_err() {
                        break;
                    }
                }
                info!("embedded Tauri edge-gateway proxy shutting down gracefully");
            });
            if let Err(e) = graceful.await {
                error!(error = %e, "embedded proxy terminated unexpectedly");
            }
        }
        Err(e) => error!(bind_addr = %bind_addr, error = %e, "failed to bind embedded proxy"),
    }
}

async fn proxy_healthz(State(engine): State<Arc<GatewayEngine>>) -> impl IntoResponse {
    let mode = match engine.active_route_mode() {
        RouteMode::Lock => "lock",
        RouteMode::StageAware => "stage_aware",
    };
    Json(json!({
        "status": "ok",
        "embedded": "tauri",
        "route_mode": mode
    }))
}

async fn proxy_anthropic(
    State(engine): State<Arc<GatewayEngine>>,
    headers: HeaderMap,
    Json(payload): Json<Value>,
) -> Response {
    dispatch(engine, UpstreamProtocol::Anthropic, headers, payload).await
}

async fn proxy_anthropic_count_tokens(
    State(engine): State<Arc<GatewayEngine>>,
    headers: HeaderMap,
    Json(payload): Json<Value>,
) -> Response {
    if !engine.verify_api_auth(extract_inbound_token(&headers)) {
        let body = engine.format_error(
            UpstreamProtocol::Anthropic,
            "authentication_error",
            "invalid or missing local gateway API key",
        );
        return (StatusCode::UNAUTHORIZED, Json(body)).into_response();
    }
    match engine.count_tokens(payload).await {
        Ok(res) => Json(res).into_response(),
        Err(e) => (
            StatusCode::BAD_REQUEST,
            Json(engine.format_error(
                UpstreamProtocol::Anthropic,
                "invalid_request_error",
                &e.to_string(),
            )),
        )
            .into_response(),
    }
}

async fn proxy_openai(
    State(engine): State<Arc<GatewayEngine>>,
    headers: HeaderMap,
    Json(payload): Json<Value>,
) -> Response {
    dispatch(engine, UpstreamProtocol::OpenAi, headers, payload).await
}

async fn proxy_gemini(
    State(engine): State<Arc<GatewayEngine>>,
    Path(model_action): Path<String>,
    headers: HeaderMap,
    Json(mut payload): Json<Value>,
) -> Response {
    let Some((model, action)) = model_action.rsplit_once(':') else {
        let body = engine.format_error(
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
            let body = engine.format_error(
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
    dispatch(engine, UpstreamProtocol::Gemini, headers, payload).await
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
    engine: Arc<GatewayEngine>,
    protocol: UpstreamProtocol,
    headers: HeaderMap,
    payload: Value,
) -> Response {
    if !engine.verify_inbound_auth(extract_inbound_token(&headers)) {
        let body = engine.format_error(
            protocol,
            "authentication_error",
            "invalid or missing local gateway token",
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

    match engine
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
        Err(GatewayError::UpstreamError { status, body, tier }) => {
            let code = StatusCode::from_u16(status).unwrap_or(StatusCode::BAD_GATEWAY);
            let err_type = if status == 400 || status == 422 {
                "invalid_request_error"
            } else {
                "api_error"
            };
            let mut resp = (code, Json(engine.format_error(protocol, err_type, &body))).into_response();
            if let Ok(hv) = HeaderValue::from_str(&format!("{:?}", tier)) {
                resp.headers_mut().insert(HeaderName::from_static("x-gateway-error-tier"), hv);
            }
            resp
        }
        Err(GatewayError::MiddlewareRejected {
            status,
            error_type,
            message,
            middleware,
        }) => {
            let code = StatusCode::from_u16(status).unwrap_or(StatusCode::FORBIDDEN);
            let mut resp = (code, Json(engine.format_error(protocol, &error_type, &message)))
                .into_response();
            if let Ok(hv) = HeaderValue::from_str(middleware) {
                resp.headers_mut()
                    .insert(HeaderName::from_static("x-gateway-middleware"), hv);
            }
            resp
        }
        Err(err) => (
            StatusCode::BAD_GATEWAY,
            Json(engine.format_error(protocol, "api_error", &err.to_string())),
        )
            .into_response(),
    }
}

fn load_config_and_db() -> (GatewayConfig, PathBuf, PathBuf) {
    let config_path = std::env::var("GATEWAY_CONFIG")
        .unwrap_or_else(|_| "../../../config.example.toml".to_string());
    let persist_path = {
        let loaded = PathBuf::from(&config_path);
        match loaded.parent() {
            Some(dir) if !dir.as_os_str().is_empty() => dir.join("config.local.toml"),
            _ => PathBuf::from("config.local.toml"),
        }
    };
    let config = GatewayConfig::from_file(&config_path).unwrap_or_else(|_| GatewayConfig {
        server: Default::default(),
        routing: Default::default(),
        error_governance: Default::default(),
        middleware: Default::default(),
        model_pricing: Default::default(),
        channels: vec![],
    });
    let db_path = std::env::var("GATEWAY_DB_PATH")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from(".edge-gateway/trajectory.db"));
    (config, db_path, persist_path)
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter("edge_gateway_desktop=info,gateway_core=info")
        .try_init();

    let (config, db_path, persist_path) = load_config_and_db();
    let bind_addr = config.server.bind_addr.clone();
    let gateway_url = format!("http://{}", bind_addr);

    let engine = Arc::new(
        GatewayEngine::new_with_sqlite(config, &db_path)
            .expect("failed to initialize embedded GatewayEngine with SQLite WAL"),
    );
    engine.set_persist_config_path(persist_path);
    let takeover = CliTakeoverManager::new(gateway_url)
        .expect("failed to initialize CliTakeoverManager");

    let bg_engine = Arc::clone(&engine);
    let bg_bind = bind_addr.clone();
    let tray_bind = bind_addr.clone();
    let (shutdown_tx, shutdown_rx) = watch::channel(false);

    let app = tauri::Builder::default()
        .manage(DesktopState {
            engine: Arc::clone(&engine),
            takeover,
            bind_addr,
            shutdown_tx: shutdown_tx.clone(),
        })
        .setup(move |app| {
            // 1. Spawn embedded Axum data plane on Tauri's Tokio runtime
            tauri::async_runtime::spawn(run_embedded_proxy(bg_engine, bg_bind, shutdown_rx));

            // 2. Build lightweight System Tray with dynamic LoomGate status indicator & live hot-switching
            let open_i = MenuItem::with_id(app, "open_studio", "Open LoomGate Studio", true, None::<&str>)?;
            let mode_lock = MenuItem::with_id(app, "mode_lock", "Route Mode: Lock (No Downgrade)", true, None::<&str>)?;
            let mode_stage = MenuItem::with_id(app, "mode_stage", "Route Mode: Stage-Aware", true, None::<&str>)?;
            let quit_i = MenuItem::with_id(app, "quit", "Quit LoomGate", true, None::<&str>)?;
            let menu = Menu::with_items(app, &[&open_i, &mode_lock, &mode_stage, &quit_i])?;

            let initial_mode = app.state::<DesktopState>().engine.active_route_mode();
            let initial_label = match initial_mode {
                RouteMode::Lock => "Lock",
                RouteMode::StageAware => "Stage-Aware",
            };
            let tray_builder = TrayIconBuilder::with_id("main-tray")
                .menu(&menu)
                .icon(tray_icon_for_mode(initial_mode))
                .tooltip(format!("LoomGate ({}) — {}", tray_bind, initial_label));

            let tooltip_bind = tray_bind.clone();
            let _tray = tray_builder
                .on_menu_event(move |app, event| match event.id.as_ref() {
                    "open_studio" => {
                        if let Some(win) = app.get_webview_window("main") {
                            let _ = win.show();
                            let _ = win.set_focus();
                        }
                    }
                    "mode_lock" => {
                        let st = app.state::<DesktopState>();
                        st.engine.set_route_mode(RouteMode::Lock);
                        sync_tray_visuals(app, &tooltip_bind, RouteMode::Lock);
                        let _ = app.emit("route-mode-changed", "lock");
                    }
                    "mode_stage" => {
                        let st = app.state::<DesktopState>();
                        st.engine.set_route_mode(RouteMode::StageAware);
                        sync_tray_visuals(app, &tooltip_bind, RouteMode::StageAware);
                        let _ = app.emit("route-mode-changed", "stage_aware");
                    }
                    "quit" => {
                        let st = app.state::<DesktopState>();
                        let _ = st.shutdown_tx.send(true);
                        app.exit(0);
                    }
                    _ => {}
                })
                .on_tray_icon_event(|tray, event| {
                    if let TrayIconEvent::Click {
                        button: MouseButton::Left,
                        button_state: MouseButtonState::Up,
                        ..
                    } = event
                    {
                        if let Some(win) = tray.app_handle().get_webview_window("main") {
                            let _ = win.show();
                            let _ = win.set_focus();
                        }
                    }
                })
                .build(app)?;

            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            get_gateway_status,
            get_channel_snapshots,
            set_route_mode,
            get_local_auth_token,
            set_local_auth_token,
            get_auth_settings,
            set_auth_settings,
            list_channel_configs,
            upsert_channel,
            delete_channel,
            toggle_channel,
            test_channel_connection,
            scan_local_providers,
            import_local_provider,
            get_savings_summary,
            list_client_profiles,
            takeover_client_profile,
            restore_client_profile,
            get_autostart_enabled,
            set_autostart_enabled,
            get_cloud_status,
            cloud_login,
            cloud_sync,
            cloud_logout,
            list_cli_integrations,
            takeover_cli_config,
            restore_cli_config,
            list_sessions,
            get_session_steps,
            fork_and_diff_step,
            export_fork_diff_markdown,
        ])
        .build(tauri::generate_context!())
        .expect("error while building Edge Gateway Tauri application");

    app.run(move |_app_handle, event| {
        if let RunEvent::Exit = event {
            let _ = shutdown_tx.send(true);
        }
    });
}
