use crate::config::ChannelConfig;
use crate::trajectory::now_epoch_ms;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::RwLock;
use uuid::Uuid;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CloudSessionStatus {
    pub connected: bool,
    pub control_plane_url: Option<String>,
    pub email: Option<String>,
    pub tenant_id: Option<String>,
    pub synced_channels: usize,
    pub available_micros: Option<u64>,
    pub telemetry_uploaded: u64,
    pub last_sync_ms: Option<u64>,
}

#[derive(Debug, Clone)]
struct CloudCredentials {
    control_plane_url: String,
    token: String,
    email: String,
    tenant_id: String,
    synced_channels: usize,
    available_micros: Option<u64>,
    last_sync_ms: u64,
}

/// Strictly desensitized metadata uploaded to Cloud Control Plane (ClickHouse).
/// By structural construction, NO prompt text, system instructions, tool arguments, or response text
/// can ever be serialized or transmitted to the cloud (Red Line #3).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SanitizedTelemetryUpload {
    pub tenant_id: String,
    pub edge_node_id: String,
    pub session_hash: String,
    pub step_id: String,
    pub protocol: String,
    pub channel_id: String,
    pub actual_model: String,
    pub route_mode: String,
    pub sticky_hit: bool,
    pub ttft_ms: u64,
    pub input_tokens: u64,
    pub cache_read_tokens: u64,
    pub output_tokens: u64,
    pub cost_micros: u64,
    pub status: String,
    pub ts_ms: u64,
}

pub fn sha256_hex(input: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(input.as_bytes());
    hasher
        .finalize()
        .iter()
        .map(|b| format!("{:02x}", b))
        .collect()
}

/// Edge-side Cloud Control Plane connector:
/// - Authenticates edge daemon / desktop client to team account
/// - Pulls team channel pool for hot-merging into `StickyP2cRouter`
/// - Performs two-phase atomic reserve/settle billing
/// - Uploads strictly desensitized telemetry metadata asynchronously
pub struct CloudSyncClient {
    edge_node_id: String,
    creds: RwLock<Option<CloudCredentials>>,
    telemetry_uploaded: AtomicU64,
    http: Client,
}

impl CloudSyncClient {
    pub fn new(http: Client) -> Self {
        Self {
            edge_node_id: format!("edge_{}", &Uuid::new_v4().simple().to_string()[..12]),
            creds: RwLock::new(None),
            telemetry_uploaded: AtomicU64::new(0),
            http,
        }
    }

    pub fn is_connected(&self) -> bool {
        self.creds
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .is_some()
    }

    pub fn status(&self) -> CloudSessionStatus {
        let g = self.creds.read().unwrap_or_else(|e| e.into_inner());
        let uploaded = self.telemetry_uploaded.load(Ordering::Relaxed);
        match g.as_ref() {
            Some(c) => CloudSessionStatus {
                connected: true,
                control_plane_url: Some(c.control_plane_url.clone()),
                email: Some(c.email.clone()),
                tenant_id: Some(c.tenant_id.clone()),
                synced_channels: c.synced_channels,
                available_micros: c.available_micros,
                telemetry_uploaded: uploaded,
                last_sync_ms: Some(c.last_sync_ms),
            },
            None => CloudSessionStatus {
                connected: false,
                control_plane_url: None,
                email: None,
                tenant_id: None,
                synced_channels: 0,
                available_micros: None,
                telemetry_uploaded: uploaded,
                last_sync_ms: None,
            },
        }
    }

    pub fn logout(&self) {
        let mut g = self.creds.write().unwrap_or_else(|e| e.into_inner());
        *g = None;
    }

    /// Logs in to the Cloud Control Plane and immediately pulls the team's upstream channel pool.
    pub async fn login_and_bootstrap(
        &self,
        control_plane_url: &str,
        email: &str,
        password: &str,
    ) -> Result<(CloudSessionStatus, Vec<ChannelConfig>), String> {
        let base = control_plane_url.trim_end_matches('/').to_string();
        let resp = self
            .http
            .post(format!("{}/api/v1/auth/login", base))
            .json(&json!({ "email": email, "password": password }))
            .send()
            .await
            .map_err(|e| format!("cloud login transport error: {}", e))?;

        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            return Err(format!("cloud login failed ({}): {}", status, text));
        }

        let body: Value = resp.json().await.map_err(|e| e.to_string())?;
        let token = body
            .get("token")
            .and_then(|v| v.as_str())
            .ok_or("missing token in login response")?
            .to_string();
        let tenant_id = body
            .get("tenant_id")
            .and_then(|v| v.as_str())
            .unwrap_or("team-default")
            .to_string();
        let avail = body
            .get("balance")
            .and_then(|b| b.get("available_micros"))
            .and_then(|v| v.as_u64());

        {
            let mut g = self.creds.write().unwrap_or_else(|e| e.into_inner());
            *g = Some(CloudCredentials {
                control_plane_url: base,
                token,
                email: email.to_string(),
                tenant_id,
                synced_channels: 0,
                available_micros: avail,
                last_sync_ms: now_epoch_ms().max(0) as u64,
            });
        }

        let channels = self.fetch_team_channels().await?;
        Ok((self.status(), channels))
    }

    /// Pulls the latest team channel pool & balance from the Cloud Control Plane.
    pub async fn fetch_team_channels(&self) -> Result<Vec<ChannelConfig>, String> {
        let (base, token) = {
            let g = self.creds.read().unwrap_or_else(|e| e.into_inner());
            let Some(c) = g.as_ref() else {
                return Err("not logged into cloud control plane".into());
            };
            (c.control_plane_url.clone(), c.token.clone())
        };

        let resp = self
            .http
            .get(format!("{}/api/v1/edge/bootstrap", base))
            .bearer_auth(&token)
            .send()
            .await
            .map_err(|e| format!("cloud bootstrap error: {}", e))?;

        if !resp.status().is_success() {
            return Err(format!("cloud bootstrap HTTP {}", resp.status()));
        }

        let body: Value = resp.json().await.map_err(|e| e.to_string())?;
        let channels: Vec<ChannelConfig> = body
            .get("channels")
            .cloned()
            .map(serde_json::from_value)
            .transpose()
            .map_err(|e| format!("invalid team channels payload: {}", e))?
            .unwrap_or_default();
        let avail = body
            .get("balance")
            .and_then(|b| b.get("available_micros"))
            .and_then(|v| v.as_u64());

        {
            let mut g = self.creds.write().unwrap_or_else(|e| e.into_inner());
            if let Some(c) = g.as_mut() {
                c.synced_channels = channels.len();
                c.available_micros = avail;
                c.last_sync_ms = now_epoch_ms().max(0) as u64;
            }
        }
        Ok(channels)
    }

    /// Phase 1: Atomic credit pre-deduction before dispatching to upstream.
    pub async fn reserve_billing(
        &self,
        reservation_id: &str,
        model: &str,
        estimated_tokens: u64,
    ) -> Result<(), String> {
        let (base, token) = {
            let g = self.creds.read().unwrap_or_else(|e| e.into_inner());
            let Some(c) = g.as_ref() else {
                return Ok(());
            };
            (c.control_plane_url.clone(), c.token.clone())
        };

        let resp = self
            .http
            .post(format!("{}/api/v1/billing/reserve", base))
            .bearer_auth(&token)
            .json(&json!({
                "reservation_id": reservation_id,
                "model": model,
                "estimated_tokens": estimated_tokens,
            }))
            .send()
            .await
            .map_err(|e| format!("billing reserve transport error: {}", e))?;

        if resp.status().is_success() {
            if let Ok(v) = resp.json::<Value>().await {
                if let Some(avail) = v.get("available_micros").and_then(|x| x.as_u64()) {
                    let mut g = self.creds.write().unwrap_or_else(|e| e.into_inner());
                    if let Some(c) = g.as_mut() {
                        c.available_micros = Some(avail);
                    }
                }
            }
            Ok(())
        } else {
            let txt = resp.text().await.unwrap_or_default();
            Err(format!("cloud billing reserve rejected: {}", txt))
        }
    }

    /// Phase 2: Atomic final settlement (or release on error).
    pub async fn settle_billing(
        &self,
        reservation_id: &str,
        model: &str,
        input_tokens: u64,
        cache_read_tokens: u64,
        output_tokens: u64,
        release_only: bool,
    ) {
        let (base, token) = {
            let g = self.creds.read().unwrap_or_else(|e| e.into_inner());
            let Some(c) = g.as_ref() else {
                return;
            };
            (c.control_plane_url.clone(), c.token.clone())
        };

        if let Ok(resp) = self
            .http
            .post(format!("{}/api/v1/billing/settle", base))
            .bearer_auth(&token)
            .json(&json!({
                "reservation_id": reservation_id,
                "model": model,
                "input_tokens": input_tokens,
                "cache_read_tokens": cache_read_tokens,
                "output_tokens": output_tokens,
                "release_only": release_only,
            }))
            .send()
            .await
        {
            if let Ok(v) = resp.json::<Value>().await {
                if let Some(avail) = v.get("available_micros").and_then(|x| x.as_u64()) {
                    let mut g = self.creds.write().unwrap_or_else(|e| e.into_inner());
                    if let Some(c) = g.as_mut() {
                        c.available_micros = Some(avail);
                    }
                }
            }
        }
    }

    /// Uploads strictly desensitized trajectory metadata (ZERO prompt or response bodies).
    #[allow(clippy::too_many_arguments)]
    pub async fn upload_sanitized_telemetry(
        &self,
        raw_session_id: &str,
        step_id: &str,
        protocol: &str,
        channel_id: &str,
        actual_model: &str,
        route_mode: &str,
        sticky_hit: bool,
        ttft_ms: u64,
        input_tokens: u64,
        cache_read_tokens: u64,
        output_tokens: u64,
        status: &str,
    ) {
        let (base, token, tenant_id) = {
            let g = self.creds.read().unwrap_or_else(|e| e.into_inner());
            let Some(c) = g.as_ref() else {
                return;
            };
            (
                c.control_plane_url.clone(),
                c.token.clone(),
                c.tenant_id.clone(),
            )
        };

        let payload = SanitizedTelemetryUpload {
            tenant_id,
            edge_node_id: self.edge_node_id.clone(),
            session_hash: sha256_hex(raw_session_id),
            step_id: step_id.to_string(),
            protocol: protocol.to_string(),
            channel_id: channel_id.to_string(),
            actual_model: actual_model.to_string(),
            route_mode: route_mode.to_string(),
            sticky_hit,
            ttft_ms,
            input_tokens,
            cache_read_tokens,
            output_tokens,
            cost_micros: 0,
            status: status.to_string(),
            ts_ms: now_epoch_ms().max(0) as u64,
        };

        if let Ok(resp) = self
            .http
            .post(format!("{}/api/v1/telemetry/ingest", base))
            .bearer_auth(&token)
            .json(&payload)
            .send()
            .await
        {
            if resp.status().is_success() {
                self.telemetry_uploaded.fetch_add(1, Ordering::Relaxed);
            }
        }
    }
}
