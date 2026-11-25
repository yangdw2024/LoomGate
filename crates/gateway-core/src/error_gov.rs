use crate::config::ErrorGovernanceConfig;
use rand::Rng;
use serde::Serialize;
use std::time::Duration;

/// Four-tier error governance classification (inspired by Antigravity Proxy resilience).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum ErrorTier {
    /// L1: Transient jitter (Soft 429 / 502 / 503 / 504 / network blip).
    /// Do NOT evict channel; retry in-place with 1–3s exponential backoff.
    L1TransientJitter,
    /// L2: Hard rate limit (RPM / TPM 429).
    /// Put channel into short cooldown (default 60s) and fail over seamlessly to a healthy peer.
    L2RateLimit,
    /// L3: Quota exhausted / Auth failure (401 / 403 / billing quota 429).
    /// Quarantine channel for long cooldown, unbind session affinity immediately, emit alert.
    L3QuotaOrAuth,
    /// L4: Semantic / Client / Safety error (400 / 422 / content_filter).
    /// NEVER retry on another channel; return normalized error immediately.
    L4SemanticNoRetry,
}

#[derive(Debug, Clone)]
pub struct ErrorAction {
    pub tier: ErrorTier,
    pub retry_same_channel: bool,
    pub failover_other_channel: bool,
    pub unbind_affinity: bool,
    pub backoff: Option<Duration>,
    pub channel_cooldown: Option<Duration>,
}

pub fn classify_upstream_error(
    status: u16,
    body: &str,
    retry_after_secs: Option<u64>,
    cfg: &ErrorGovernanceConfig,
    attempt: u32,
) -> ErrorAction {
    let lower = body.to_ascii_lowercase();

    // L3: Auth failure or hard quota exhaustion -> Quarantine + unbind affinity.
    // Checked BEFORE semantic keyword matching so 401/403 mentioning "schema" or "safety"
    // is never misclassified as L4.
    if status == 401
        || status == 403
        || lower.contains("insufficient_quota")
        || lower.contains("billing")
        || lower.contains("credit balance")
        || lower.contains("account_deactivated")
    {
        return ErrorAction {
            tier: ErrorTier::L3QuotaOrAuth,
            retry_same_channel: false,
            failover_other_channel: true,
            unbind_affinity: true,
            backoff: None,
            channel_cooldown: Some(Duration::from_secs(cfg.l3_quarantine_secs)),
        };
    }

    // L4: Semantic / Schema / Safety refusal -> scoped strictly to 400 / 422 (or other non-auth 4xx).
    if status == 400
        || status == 422
        || ((400..500).contains(&status)
            && status != 429
            && (lower.contains("invalid_request_error")
                || lower.contains("content_policy")
                || lower.contains("safety")
                || lower.contains("schema")))
    {
        return ErrorAction {
            tier: ErrorTier::L4SemanticNoRetry,
            retry_same_channel: false,
            failover_other_channel: false,
            unbind_affinity: false,
            backoff: None,
            channel_cooldown: None,
        };
    }

    // Distinguish L1 Soft 429 vs L2 Hard RPM/TPM 429.
    if status == 429 {
        let is_soft = lower.contains("overloaded")
            || lower.contains("try again in a moment")
            || lower.contains("temporarily busy")
            || retry_after_secs.map(|s| s <= 3).unwrap_or(false);

        if is_soft && attempt < cfg.l1_max_retries {
            return ErrorAction {
                tier: ErrorTier::L1TransientJitter,
                retry_same_channel: true,
                failover_other_channel: false,
                unbind_affinity: false,
                backoff: Some(compute_l1_backoff(cfg, attempt)),
                channel_cooldown: None,
            };
        }

        let cooldown = retry_after_secs
            .map(Duration::from_secs)
            .unwrap_or_else(|| Duration::from_secs(cfg.l2_cooldown_secs));
        return ErrorAction {
            tier: ErrorTier::L2RateLimit,
            retry_same_channel: false,
            failover_other_channel: true,
            unbind_affinity: false,
            backoff: None,
            channel_cooldown: Some(cooldown),
        };
    }

    // 500 / 502 / 503 / 504 / 529 -> L1 transient jitter first, then L2 failover if retries exhausted.
    if matches!(status, 500 | 502 | 503 | 504 | 529) {
        if attempt < cfg.l1_max_retries {
            return ErrorAction {
                tier: ErrorTier::L1TransientJitter,
                retry_same_channel: true,
                failover_other_channel: false,
                unbind_affinity: false,
                backoff: Some(compute_l1_backoff(cfg, attempt)),
                channel_cooldown: None,
            };
        }
        return ErrorAction {
            tier: ErrorTier::L2RateLimit,
            retry_same_channel: false,
            failover_other_channel: true,
            unbind_affinity: false,
            backoff: None,
            channel_cooldown: Some(Duration::from_secs(cfg.l2_cooldown_secs)),
        };
    }

    ErrorAction {
        tier: ErrorTier::L4SemanticNoRetry,
        retry_same_channel: false,
        failover_other_channel: false,
        unbind_affinity: false,
        backoff: None,
        channel_cooldown: None,
    }
}

fn compute_l1_backoff(cfg: &ErrorGovernanceConfig, attempt: u32) -> Duration {
    let exp = cfg
        .l1_base_backoff_ms
        .saturating_mul(1u64 << attempt.min(4))
        .min(cfg.l1_max_backoff_ms);
    let jitter = rand::thread_rng().gen_range(0..=250);
    Duration::from_millis((exp + jitter).min(cfg.l1_max_backoff_ms))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_all_four_tiers_without_keyword_misclassification() {
        let cfg = ErrorGovernanceConfig::default();

        // 401 mentioning "schema" must still be L3 quarantine, never L4!
        let l3_tricky = classify_upstream_error(401, "auth schema token expired", None, &cfg, 0);
        assert_eq!(l3_tricky.tier, ErrorTier::L3QuotaOrAuth);
        assert!(l3_tricky.unbind_affinity);

        // 502 mentioning "safety" must still be L1 transient retry, never L4!
        let l1_tricky = classify_upstream_error(502, "upstream safety proxy bad gateway", None, &cfg, 0);
        assert_eq!(l1_tricky.tier, ErrorTier::L1TransientJitter);
        assert!(l1_tricky.retry_same_channel);

        let l2 = classify_upstream_error(429, "rate_limit_error: RPM exceeded", Some(45), &cfg, 0);
        assert_eq!(l2.tier, ErrorTier::L2RateLimit);
        assert_eq!(l2.channel_cooldown, Some(Duration::from_secs(45)));

        let l4 = classify_upstream_error(400, "invalid_request_error: bad tool schema", None, &cfg, 0);
        assert_eq!(l4.tier, ErrorTier::L4SemanticNoRetry);
        assert!(!l4.retry_same_channel);
        assert!(!l4.failover_other_channel);
    }
}
