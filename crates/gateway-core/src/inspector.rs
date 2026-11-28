use crate::middleware::guardrail::redact_sensitive_text;
use serde::{Deserialize, Serialize};
use std::collections::VecDeque;
use std::sync::RwLock;

/// Default bounded capacity for the Live Traffic Inspector ring buffer.
pub const DEFAULT_INSPECTOR_CAPACITY: usize = 200;

/// Maximum number of Unicode scalar values retained in captured inbound/upstream previews.
pub const MAX_PREVIEW_CHARS: usize = 4096;

/// Captured snapshot of a single gateway request/response for the "Charles for AI Agents"
/// Live Traffic Inspector.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TrafficInspectEntry {
    pub request_id: String,
    pub ts_ms: u64,
    pub downstream_protocol: String,
    pub upstream_protocol: String,
    pub requested_model: String,
    pub actual_model: String,
    pub channel_id: String,
    pub route_mode: String,
    pub sticky_hit: bool,
    pub stream: bool,
    pub ttft_ms: u64,
    pub status: String,
    pub input_tokens: u64,
    pub cache_read_tokens: u64,
    pub output_tokens: u64,
    pub guardrail_redactions: usize,
    pub blocked_reason: Option<String>,
    pub inbound_preview: String,
    pub canonical_ir_summary: String,
    pub upstream_preview: String,
    pub sse_events: Vec<String>,
}

/// Lightweight bounded ring-buffer storing recent gateway traffic with automatic
/// Guardrail secret/PII redaction and UTF-8 safe preview truncation.
#[derive(Debug)]
pub struct LiveTrafficInspector {
    capacity: usize,
    entries: RwLock<VecDeque<TrafficInspectEntry>>,
}

impl Default for LiveTrafficInspector {
    fn default() -> Self {
        Self::new(DEFAULT_INSPECTOR_CAPACITY)
    }
}

fn sanitize_and_truncate_preview(raw: &str, max_chars: usize) -> String {
    let (redacted, _) = redact_sensitive_text(raw, true, true);
    truncate_chars(&redacted, max_chars)
}

fn truncate_chars(s: &str, max_chars: usize) -> String {
    match s.char_indices().nth(max_chars) {
        Some((byte_idx, _)) => s[..byte_idx].to_string(),
        None => s.to_string(),
    }
}

impl LiveTrafficInspector {
    /// Creates a new bounded traffic inspector holding at most `capacity` recent entries.
    pub fn new(capacity: usize) -> Self {
        Self {
            capacity,
            entries: RwLock::new(VecDeque::with_capacity(capacity.min(1024))),
        }
    }

    /// Records a traffic entry, automatically redacting secrets & PII in `inbound_preview`
    /// and `upstream_preview` and truncating previews to <= 4096 Unicode chars.
    pub fn record(&self, mut entry: TrafficInspectEntry) {
        if self.capacity == 0 {
            return;
        }
        entry.inbound_preview =
            sanitize_and_truncate_preview(&entry.inbound_preview, MAX_PREVIEW_CHARS);
        entry.upstream_preview =
            sanitize_and_truncate_preview(&entry.upstream_preview, MAX_PREVIEW_CHARS);

        let mut guard = self.entries.write().unwrap_or_else(|e| e.into_inner());
        while guard.len() >= self.capacity {
            guard.pop_front();
        }
        guard.push_back(entry);
    }

    /// Returns up to `limit` captured entries ordered newest-first.
    pub fn list(&self, limit: usize) -> Vec<TrafficInspectEntry> {
        let guard = self.entries.read().unwrap_or_else(|e| e.into_inner());
        guard.iter().rev().take(limit).cloned().collect()
    }

    /// Retrieves the most recent captured entry matching `request_id`.
    pub fn get(&self, request_id: &str) -> Option<TrafficInspectEntry> {
        let guard = self.entries.read().unwrap_or_else(|e| e.into_inner());
        guard
            .iter()
            .rev()
            .find(|entry| entry.request_id == request_id)
            .cloned()
    }

    /// Clears all buffered traffic entries.
    pub fn clear(&self) {
        let mut guard = self.entries.write().unwrap_or_else(|e| e.into_inner());
        guard.clear();
    }

    pub fn len(&self) -> usize {
        let guard = self.entries.read().unwrap_or_else(|e| e.into_inner());
        guard.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn capacity(&self) -> usize {
        self.capacity
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_entry(id: &str, inbound: &str, upstream: &str) -> TrafficInspectEntry {
        TrafficInspectEntry {
            request_id: id.to_string(),
            ts_ms: 1_700_000_000_000,
            downstream_protocol: "anthropic".to_string(),
            upstream_protocol: "openai".to_string(),
            requested_model: "claude-sonnet-4-20250514".to_string(),
            actual_model: "gpt-4o".to_string(),
            channel_id: "ch-1".to_string(),
            route_mode: "sticky_p2c".to_string(),
            sticky_hit: true,
            stream: true,
            ttft_ms: 120,
            status: "200".to_string(),
            input_tokens: 100,
            cache_read_tokens: 80,
            output_tokens: 40,
            guardrail_redactions: 0,
            blocked_reason: None,
            inbound_preview: inbound.to_string(),
            canonical_ir_summary: "1 user msg".to_string(),
            upstream_preview: upstream.to_string(),
            sse_events: vec!["message_start".to_string()],
        }
    }

    #[test]
    fn ring_buffer_evicts_oldest_and_lists_newest_first() {
        let inspector = LiveTrafficInspector::new(3);
        for i in 1..=5 {
            inspector.record(sample_entry(&format!("req-{i}"), "hello", "world"));
        }

        assert_eq!(inspector.len(), 3);
        let listed = inspector.list(10);
        let ids: Vec<&str> = listed.iter().map(|e| e.request_id.as_str()).collect();
        assert_eq!(ids, vec!["req-5", "req-4", "req-3"]);

        assert!(inspector.get("req-1").is_none());
        assert!(inspector.get("req-2").is_none());
        assert_eq!(inspector.get("req-4").unwrap().request_id, "req-4");

        let limited = inspector.list(2);
        assert_eq!(limited.len(), 2);
        assert_eq!(limited[0].request_id, "req-5");
        assert_eq!(limited[1].request_id, "req-4");

        inspector.clear();
        assert!(inspector.is_empty());
        assert!(inspector.list(10).is_empty());
    }

    #[test]
    fn record_redacts_secrets_and_pii_automatically() {
        let inspector = LiveTrafficInspector::default();
        assert_eq!(inspector.capacity(), DEFAULT_INSPECTOR_CAPACITY);

        inspector.record(sample_entry(
            "req-secret",
            "Authorization: Bearer super_secret_token_987654 and key sk-ant-abcdef1234567890",
            "User email alice@example.com phone 13812345678 key sk-1234567890abcdef",
        ));

        let entry = inspector.get("req-secret").expect("entry should exist");
        assert!(!entry.inbound_preview.contains("super_secret_token_987654"));
        assert!(!entry.inbound_preview.contains("sk-ant-abcdef1234567890"));
        assert!(entry.inbound_preview.contains("[REDACTED_SECRET]"));

        assert!(!entry.upstream_preview.contains("alice@example.com"));
        assert!(!entry.upstream_preview.contains("13812345678"));
        assert!(!entry.upstream_preview.contains("sk-1234567890abcdef"));
        assert!(entry.upstream_preview.contains("[REDACTED_EMAIL]"));
        assert!(entry.upstream_preview.contains("[REDACTED_PHONE]"));
        assert!(entry.upstream_preview.contains("[REDACTED_SECRET]"));
    }

    #[test]
    fn record_truncates_multibyte_utf8_previews_safely() {
        let inspector = LiveTrafficInspector::new(4);
        let long_cjk = "你好🚀世界".repeat(1200); // 6000 Unicode chars, multi-byte UTF-8
        inspector.record(sample_entry("req-utf8", &long_cjk, &long_cjk));

        let entry = inspector.get("req-utf8").expect("entry should exist");
        assert_eq!(entry.inbound_preview.chars().count(), MAX_PREVIEW_CHARS);
        assert_eq!(entry.upstream_preview.chars().count(), MAX_PREVIEW_CHARS);
        assert!(entry.inbound_preview.starts_with("你好🚀世界"));
    }
}
