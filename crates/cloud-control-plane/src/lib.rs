pub mod billing;
pub mod telemetry;

use axum::{
    extract::State,
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use billing::{estimate_cost_micros, BillingLedger};
use dashmap::DashMap;
use gateway_core::ChannelConfig;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::sync::Arc;
use telemetry::{validate_sanitized_json, SanitizedTelemetryEvent, TelemetryStore};
use uuid::Uuid;

pub const POSTGRES_SCHEMA_SQL: &str = include_str!("../deploy/postgres_schema.sql");

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UserAccount {
    pub email: String,
    pub password: String,
    pub tenant_id: String,
    pub role: String,
}

#[derive(Debug, Clone)]
pub struct AuthSession {
    pub email: String,
    pub tenant_id: String,
}

pub struct ControlPlaneState {
    pub users: DashMap<String, UserAccount>,
    pub sessions: DashMap<String, AuthSession>,
    pub team_channels: DashMap<String, Vec<ChannelConfig>>,
    pub billing: BillingLedger,
    pub telemetry: TelemetryStore,
}

impl ControlPlaneState {
    /// Initializes the Cloud Control Plane with a default developer team account (`dev@loomgate.dev`)
    /// and 100 USD (100_000_000 micro-USD) credit balance ready for immediate local/team onboarding.
    pub fn with_default_seed() -> Arc<Self> {
        let state = Arc::new(Self {
            users: DashMap::new(),
            sessions: DashMap::new(),
            team_channels: DashMap::new(),
            billing: BillingLedger::new(),
            telemetry: TelemetryStore::new(),
        });
        state.users.insert(
            "dev@loomgate.dev".into(),
            UserAccount {
                email: "dev@loomgate.dev".into(),
                password: "loomgate-dev-password".into(),
                tenant_id: "team-default".into(),
                role: "admin".into(),
            },
        );
        state.billing.seed_tenant("team-default", 100_000_000);
        state.team_channels.insert("team-default".into(), vec![]);
        state
    }

    fn authenticate(&self, headers: &HeaderMap) -> Option<AuthSession> {
        let token = headers
            .get("authorization")
            .and_then(|v| v.to_str().ok())
            .and_then(|s| s.strip_prefix("Bearer "))?;
        self.sessions.get(token).map(|s| s.clone())
    }
}

#[derive(Deserialize)]
struct LoginRequest {
    email: String,
    password: String,
}

#[derive(Deserialize)]
struct RegisterChannelRequest {
    channel: ChannelConfig,
}

#[derive(Deserialize)]
struct ReserveRequest {
    reservation_id: String,
    model: String,
    estimated_tokens: u64,
}

#[derive(Deserialize)]
struct SettleRequest {
    reservation_id: String,
    model: String,
    input_tokens: u64,
    cache_read_tokens: u64,
    output_tokens: u64,
    #[serde(default)]
    release_only: bool,
}

pub fn build_router(state: Arc<ControlPlaneState>) -> Router {
    Router::new()
        .route("/healthz", get(healthz))
        .route("/api/v1/auth/login", post(login))
        .route("/api/v1/edge/bootstrap", get(edge_bootstrap))
        .route("/api/v1/admin/channels", post(register_team_channel))
        .route("/api/v1/billing/reserve", post(billing_reserve))
        .route("/api/v1/billing/settle", post(billing_settle))
        .route("/api/v1/billing/balance", get(billing_balance))
        .route("/api/v1/telemetry/ingest", post(telemetry_ingest))
        .route("/api/v1/telemetry/analytics", get(telemetry_analytics))
        .with_state(state)
}

async fn healthz() -> impl IntoResponse {
    Json(json!({
        "status": "ok",
        "service": "loomgate-cloud-control-plane",
        "version": "0.3.0",
        "privacy_mode": "sanitized_metadata_only"
    }))
}

async fn login(
    State(state): State<Arc<ControlPlaneState>>,
    Json(req): Json<LoginRequest>,
) -> Response {
    let Some(user) = state.users.get(&req.email) else {
        return (
            StatusCode::UNAUTHORIZED,
            Json(json!({ "error": "invalid email or password" })),
        )
            .into_response();
    };
    if user.password != req.password {
        return (
            StatusCode::UNAUTHORIZED,
            Json(json!({ "error": "invalid email or password" })),
        )
            .into_response();
    }

    let token = format!("lgc_{}", Uuid::new_v4().simple());
    state.sessions.insert(
        token.clone(),
        AuthSession {
            email: user.email.clone(),
            tenant_id: user.tenant_id.clone(),
        },
    );

    let balance = state.billing.balance(&user.tenant_id);
    Json(json!({
        "token": token,
        "email": user.email,
        "tenant_id": user.tenant_id,
        "role": user.role,
        "balance": balance,
    }))
    .into_response()
}

async fn edge_bootstrap(
    State(state): State<Arc<ControlPlaneState>>,
    headers: HeaderMap,
) -> Response {
    let Some(sess) = state.authenticate(&headers) else {
        return (
            StatusCode::UNAUTHORIZED,
            Json(json!({ "error": "unauthorized" })),
        )
            .into_response();
    };
    let channels = state
        .team_channels
        .get(&sess.tenant_id)
        .map(|c| c.clone())
        .unwrap_or_default();
    let balance = state.billing.balance(&sess.tenant_id);
    Json(json!({
        "tenant_id": sess.tenant_id,
        "channels": channels,
        "balance": balance,
        "telemetry_policy": "sanitized_metadata_only"
    }))
    .into_response()
}

async fn register_team_channel(
    State(state): State<Arc<ControlPlaneState>>,
    headers: HeaderMap,
    Json(req): Json<RegisterChannelRequest>,
) -> Response {
    let Some(sess) = state.authenticate(&headers) else {
        return (
            StatusCode::UNAUTHORIZED,
            Json(json!({ "error": "unauthorized" })),
        )
            .into_response();
    };
    let mut entry = state.team_channels.entry(sess.tenant_id).or_default();
    if let Some(existing) = entry.iter_mut().find(|c| c.id == req.channel.id) {
        *existing = req.channel;
    } else {
        entry.push(req.channel);
    }
    Json(json!({ "ok": true, "channel_count": entry.len() })).into_response()
}

async fn billing_reserve(
    State(state): State<Arc<ControlPlaneState>>,
    headers: HeaderMap,
    Json(req): Json<ReserveRequest>,
) -> Response {
    let Some(sess) = state.authenticate(&headers) else {
        return (
            StatusCode::UNAUTHORIZED,
            Json(json!({ "error": "unauthorized" })),
        )
            .into_response();
    };
    let est_micros = estimate_cost_micros(&req.model, req.estimated_tokens, 0, req.estimated_tokens / 2);
    let res = state
        .billing
        .reserve(&sess.tenant_id, &req.reservation_id, &req.model, est_micros);
    if res.ok {
        Json(res).into_response()
    } else {
        (StatusCode::PAYMENT_REQUIRED, Json(res)).into_response()
    }
}

async fn billing_settle(
    State(state): State<Arc<ControlPlaneState>>,
    headers: HeaderMap,
    Json(req): Json<SettleRequest>,
) -> Response {
    let Some(_sess) = state.authenticate(&headers) else {
        return (
            StatusCode::UNAUTHORIZED,
            Json(json!({ "error": "unauthorized" })),
        )
            .into_response();
    };
    let res = if req.release_only {
        state.billing.release(&req.reservation_id)
    } else {
        let actual_micros = estimate_cost_micros(
            &req.model,
            req.input_tokens,
            req.cache_read_tokens,
            req.output_tokens,
        );
        state.billing.settle(&req.reservation_id, actual_micros)
    };
    Json(res).into_response()
}

async fn billing_balance(
    State(state): State<Arc<ControlPlaneState>>,
    headers: HeaderMap,
) -> Response {
    let Some(sess) = state.authenticate(&headers) else {
        return (
            StatusCode::UNAUTHORIZED,
            Json(json!({ "error": "unauthorized" })),
        )
            .into_response();
    };
    match state.billing.balance(&sess.tenant_id) {
        Some(snap) => Json(snap).into_response(),
        None => (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": "tenant wallet not found" })),
        )
            .into_response(),
    }
}

async fn telemetry_ingest(
    State(state): State<Arc<ControlPlaneState>>,
    headers: HeaderMap,
    Json(raw): Json<Value>,
) -> Response {
    let Some(sess) = state.authenticate(&headers) else {
        return (
            StatusCode::UNAUTHORIZED,
            Json(json!({ "error": "unauthorized" })),
        )
            .into_response();
    };

    if let Err(msg) = validate_sanitized_json(&raw) {
        return (StatusCode::BAD_REQUEST, Json(json!({ "error": msg }))).into_response();
    }

    let mut ev: SanitizedTelemetryEvent = match serde_json::from_value(raw) {
        Ok(e) => e,
        Err(e) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({ "error": format!("invalid telemetry schema: {}", e) })),
            )
                .into_response();
        }
    };
    ev.tenant_id = sess.tenant_id;
    state.telemetry.ingest(ev);
    Json(json!({ "ok": true })).into_response()
}

async fn telemetry_analytics(
    State(state): State<Arc<ControlPlaneState>>,
    headers: HeaderMap,
) -> Response {
    let Some(sess) = state.authenticate(&headers) else {
        return (
            StatusCode::UNAUTHORIZED,
            Json(json!({ "error": "unauthorized" })),
        )
            .into_response();
    };
    let summary = state.telemetry.summarize_tenant(&sess.tenant_id);
    let events = state.telemetry.events_for_tenant(&sess.tenant_id);
    Json(json!({
        "summary": summary,
        "recent_events": events,
    }))
    .into_response()
}
