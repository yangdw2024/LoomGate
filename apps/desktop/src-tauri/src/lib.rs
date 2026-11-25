use axum::{
    body::Body,
    extract::State,
    http::{HeaderMap, HeaderName, HeaderValue, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use gateway_core::{
    ChannelSnapshot, CliIntegrationStatus, CliTakeoverManager, CliTool, ForkDiffResult,
    GatewayConfig, GatewayEngine, GatewayError, GatewayResponse, RouteMode, SessionSummary,
    TrajectoryStep, UpstreamProtocol,
};
use serde_json::{json, Value};
use std::path::PathBuf;
use std::sync::Arc;
use tauri::{
    menu::{Menu, MenuItem},
    tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent},
    Emitter, Manager, RunEvent, State as TauriState,
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

#[tauri::command]
fn set_route_mode(state: TauriState<'_, DesktopState>, mode: String) -> Result<String, String> {
    let parsed = match mode.as_str() {
        "lock" => RouteMode::Lock,
        "stage_aware" => RouteMode::StageAware,
        other => return Err(format!("unsupported route mode: {}", other)),
    };
    state.engine.set_route_mode(parsed);
    Ok(mode)
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
        .route("/v1/chat/completions", post(proxy_openai))
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

async fn proxy_openai(
    State(engine): State<Arc<GatewayEngine>>,
    headers: HeaderMap,
    Json(payload): Json<Value>,
) -> Response {
    dispatch(engine, UpstreamProtocol::OpenAi, headers, payload).await
}

async fn dispatch(
    engine: Arc<GatewayEngine>,
    protocol: UpstreamProtocol,
    headers: HeaderMap,
    payload: Value,
) -> Response {
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
        Err(err) => (
            StatusCode::BAD_GATEWAY,
            Json(engine.format_error(protocol, "api_error", &err.to_string())),
        )
            .into_response(),
    }
}

fn load_config_and_db() -> (GatewayConfig, PathBuf) {
    let config_path = std::env::var("GATEWAY_CONFIG")
        .unwrap_or_else(|_| "../../../config.example.toml".to_string());
    let config = GatewayConfig::from_file(&config_path).unwrap_or_else(|_| GatewayConfig {
        server: Default::default(),
        routing: Default::default(),
        error_governance: Default::default(),
        channels: vec![],
    });
    let db_path = std::env::var("GATEWAY_DB_PATH")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from(".edge-gateway/trajectory.db"));
    (config, db_path)
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter("edge_gateway_desktop=info,gateway_core=info")
        .try_init();

    let (config, db_path) = load_config_and_db();
    let bind_addr = config.server.bind_addr.clone();
    let gateway_url = format!("http://{}", bind_addr);

    let engine = Arc::new(
        GatewayEngine::new_with_sqlite(config, &db_path)
            .expect("failed to initialize embedded GatewayEngine with SQLite WAL"),
    );
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

            // 2. Build lightweight System Tray with visible icon & live Route Mode hot-switching
            let open_i = MenuItem::with_id(app, "open_studio", "Open Control Studio", true, None::<&str>)?;
            let mode_lock = MenuItem::with_id(app, "mode_lock", "Route Mode: Lock (No Downgrade)", true, None::<&str>)?;
            let mode_stage = MenuItem::with_id(app, "mode_stage", "Route Mode: Stage-Aware", true, None::<&str>)?;
            let quit_i = MenuItem::with_id(app, "quit", "Quit Edge Gateway", true, None::<&str>)?;
            let menu = Menu::with_items(app, &[&open_i, &mode_lock, &mode_stage, &quit_i])?;

            let mut tray_builder = TrayIconBuilder::with_id("main-tray")
                .menu(&menu)
                .tooltip(format!("Edge Gateway ({}) — Lock", tray_bind));
            if let Some(icon) = app.default_window_icon() {
                tray_builder = tray_builder.icon(icon.clone());
            }

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
                        if let Some(tray) = app.tray_by_id("main-tray") {
                            let _ = tray.set_tooltip(Some(format!(
                                "Edge Gateway ({}) — Lock",
                                tooltip_bind
                            )));
                        }
                        let _ = app.emit("route-mode-changed", "lock");
                    }
                    "mode_stage" => {
                        let st = app.state::<DesktopState>();
                        st.engine.set_route_mode(RouteMode::StageAware);
                        if let Some(tray) = app.tray_by_id("main-tray") {
                            let _ = tray.set_tooltip(Some(format!(
                                "Edge Gateway ({}) — Stage-Aware",
                                tooltip_bind
                            )));
                        }
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
            list_cli_integrations,
            takeover_cli_config,
            restore_cli_config,
            list_sessions,
            get_session_steps,
            fork_and_diff_step,
        ])
        .build(tauri::generate_context!())
        .expect("error while building Edge Gateway Tauri application");

    app.run(move |_app_handle, event| {
        if let RunEvent::Exit = event {
            let _ = shutdown_tx.send(true);
        }
    });
}
