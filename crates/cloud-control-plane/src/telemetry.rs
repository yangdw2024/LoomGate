use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;
use std::sync::Mutex;

pub const CLICKHOUSE_SCHEMA_SQL: &str = include_str!("../deploy/clickhouse_schema.sql");

/// Forbidden field names that MUST NEVER appear in any cloud telemetry payload (Red Line #3).
const FORBIDDEN_RAW_FIELDS: &[&str] = &[
    "prompt",
    "messages",
    "system",
    "response_text",
    "content",
    "request_json",
    "tool_calls_json",
];

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SanitizedTelemetryEvent {
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

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelAnalytics {
    pub model: String,
    pub requests: u64,
    pub avg_ttft_ms: u64,
    pub cache_hit_ratio: f64,
    pub total_cost_micros: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TenantAnalyticsSummary {
    pub tenant_id: String,
    pub total_events: u64,
    pub total_input_tokens: u64,
    pub total_cache_read_tokens: u64,
    pub total_output_tokens: u64,
    pub overall_cache_hit_ratio: f64,
    pub total_cost_micros: u64,
    pub by_model: Vec<ModelAnalytics>,
}

/// Validates that a raw JSON telemetry payload contains zero raw prompt/response fields.
pub fn validate_sanitized_json(payload: &Value) -> Result<(), String> {
    let Some(obj) = payload.as_object() else {
        return Err("telemetry payload must be a JSON object".into());
    };
    for forbidden in FORBIDDEN_RAW_FIELDS {
        if obj.contains_key(*forbidden) {
            return Err(format!(
                "privacy violation: forbidden raw content field '{}' rejected by cloud control plane",
                forbidden
            ));
        }
    }
    Ok(())
}

/// ClickHouse-compatible desensitized trajectory telemetry store & real-time aggregator.
pub struct TelemetryStore {
    events: Mutex<Vec<SanitizedTelemetryEvent>>,
}

impl TelemetryStore {
    pub fn new() -> Self {
        Self {
            events: Mutex::new(Vec::new()),
        }
    }

    pub fn ingest(&self, ev: SanitizedTelemetryEvent) {
        let mut g = self.events.lock().unwrap_or_else(|e| e.into_inner());
        g.push(ev);
    }

    pub fn events_for_tenant(&self, tenant_id: &str) -> Vec<SanitizedTelemetryEvent> {
        let g = self.events.lock().unwrap_or_else(|e| e.into_inner());
        g.iter()
            .filter(|e| e.tenant_id == tenant_id)
            .cloned()
            .collect()
    }

    pub fn summarize_tenant(&self, tenant_id: &str) -> TenantAnalyticsSummary {
        let g = self.events.lock().unwrap_or_else(|e| e.into_inner());
        let mut total_events = 0u64;
        let mut total_in = 0u64;
        let mut total_cache = 0u64;
        let mut total_out = 0u64;
        let mut total_cost = 0u64;
        let mut per_model: HashMap<String, (u64, u64, u64, u64, u64)> = HashMap::new();

        for ev in g.iter().filter(|e| e.tenant_id == tenant_id) {
            total_events += 1;
            total_in += ev.input_tokens;
            total_cache += ev.cache_read_tokens;
            total_out += ev.output_tokens;
            total_cost += ev.cost_micros;

            let entry = per_model
                .entry(ev.actual_model.clone())
                .or_insert((0, 0, 0, 0, 0));
            entry.0 += 1;
            entry.1 += ev.ttft_ms;
            entry.2 += ev.input_tokens;
            entry.3 += ev.cache_read_tokens;
            entry.4 += ev.cost_micros;
        }

        let overall_denom = total_in + total_cache;
        let overall_cache_hit_ratio = if overall_denom == 0 {
            0.0
        } else {
            total_cache as f64 / overall_denom as f64
        };

        let mut by_model: Vec<ModelAnalytics> = per_model
            .into_iter()
            .map(|(model, (cnt, ttft_sum, m_in, m_cache, cost))| {
                let denom = m_in + m_cache;
                ModelAnalytics {
                    model,
                    requests: cnt,
                    avg_ttft_ms: ttft_sum.checked_div(cnt).unwrap_or(0),
                    cache_hit_ratio: if denom == 0 {
                        0.0
                    } else {
                        m_cache as f64 / denom as f64
                    },
                    total_cost_micros: cost,
                }
            })
            .collect();
        by_model.sort_by(|a, b| a.model.cmp(&b.model));

        TenantAnalyticsSummary {
            tenant_id: tenant_id.to_string(),
            total_events,
            total_input_tokens: total_in,
            total_cache_read_tokens: total_cache,
            total_output_tokens: total_out,
            overall_cache_hit_ratio,
            total_cost_micros: total_cost,
            by_model,
        }
    }
}

impl Default for TelemetryStore {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn rejects_raw_prompt_fields_and_aggregates_sanitized_metrics() {
        let bad = json!({ "tenant_id": "t1", "prompt": "secret user code" });
        assert!(validate_sanitized_json(&bad).is_err());

        let store = TelemetryStore::new();
        store.ingest(SanitizedTelemetryEvent {
            tenant_id: "team-demo".into(),
            edge_node_id: "node-1".into(),
            session_hash: "a".repeat(64),
            step_id: "step_1".into(),
            protocol: "anthropic".into(),
            channel_id: "team-pool-1".into(),
            actual_model: "claude-sonnet-4-20250514".into(),
            route_mode: "lock".into(),
            sticky_hit: true,
            ttft_ms: 120,
            input_tokens: 100,
            cache_read_tokens: 900,
            output_tokens: 50,
            cost_micros: 1320,
            status: "completed".into(),
            ts_ms: 1_700_000_000_000,
        });
        let summary = store.summarize_tenant("team-demo");
        assert_eq!(summary.total_events, 1);
        assert!((summary.overall_cache_hit_ratio - 0.9).abs() < 1e-6);
        assert!(CLICKHOUSE_SCHEMA_SQL.contains("sanitized_trajectories"));
    }
}
