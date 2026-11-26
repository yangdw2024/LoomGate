use crate::config::{ChannelConfig, RouteMode, RoutingConfig};
use crate::ir::CanonicalRequest;
use dashmap::DashMap;
use rand::seq::SliceRandom;
use serde::Serialize;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ChannelHealth {
    Healthy,
    Cooldown,
    Quarantined,
}

#[derive(Debug)]
pub struct ChannelRuntime {
    pub config: ChannelConfig,
    pub inflight: AtomicUsize,
    /// Exponentially Weighted Moving Average of streaming TTFT in milliseconds.
    ewma_ttft_ms: AtomicU64,
    has_ttft_sample: AtomicBool,
    total_requests: AtomicU64,
    cooldown_until_ms: AtomicU64,
    quarantined_until_ms: AtomicU64,
    epoch: Instant,
}

impl ChannelRuntime {
    pub fn new(config: ChannelConfig, epoch: Instant) -> Self {
        Self {
            config,
            inflight: AtomicUsize::new(0),
            ewma_ttft_ms: AtomicU64::new(150),
            has_ttft_sample: AtomicBool::new(false),
            total_requests: AtomicU64::new(0),
            cooldown_until_ms: AtomicU64::new(0),
            quarantined_until_ms: AtomicU64::new(0),
            epoch,
        }
    }

    fn now_ms(&self) -> u64 {
        self.epoch.elapsed().as_millis() as u64
    }

    pub fn health(&self) -> ChannelHealth {
        let now = self.now_ms();
        if self.quarantined_until_ms.load(Ordering::Relaxed) > now {
            ChannelHealth::Quarantined
        } else if self.cooldown_until_ms.load(Ordering::Relaxed) > now {
            ChannelHealth::Cooldown
        } else {
            ChannelHealth::Healthy
        }
    }

    pub fn is_available(&self) -> bool {
        self.config.enabled && self.health() == ChannelHealth::Healthy
    }

    pub fn cooldown_remaining_secs(&self) -> u64 {
        let now = self.now_ms();
        let until = self.cooldown_until_ms.load(Ordering::Relaxed);
        if until > now {
            (until - now).div_ceil(1000)
        } else {
            0
        }
    }

    pub fn quarantine_remaining_secs(&self) -> u64 {
        let now = self.now_ms();
        let until = self.quarantined_until_ms.load(Ordering::Relaxed);
        if until > now {
            (until - now).div_ceil(1000)
        } else {
            0
        }
    }

    pub fn mark_cooldown(&self, duration: Duration) {
        let until = self.now_ms().saturating_add(duration.as_millis() as u64);
        self.cooldown_until_ms.store(until, Ordering::Relaxed);
    }

    pub fn mark_quarantined(&self, duration: Duration) {
        let until = self.now_ms().saturating_add(duration.as_millis() as u64);
        self.quarantined_until_ms.store(until, Ordering::Relaxed);
    }

    pub fn clear_health_penalties(&self) {
        self.cooldown_until_ms.store(0, Ordering::Relaxed);
        self.quarantined_until_ms.store(0, Ordering::Relaxed);
    }

    /// Lock-free atomic EWMA update (alpha = 0.25) so concurrent requests never lose updates.
    pub fn record_ttft(&self, ttft_ms: u64) {
        let clamped = ttft_ms.max(1);
        if !self.has_ttft_sample.swap(true, Ordering::Relaxed) {
            self.ewma_ttft_ms.store(clamped, Ordering::Relaxed);
            return;
        }
        let _ = self.ewma_ttft_ms.try_update(
            Ordering::Relaxed,
            Ordering::Relaxed,
            |prev| Some(((prev.saturating_mul(3).saturating_add(clamped)) / 4).max(1)),
        );
    }

    /// Lower score is better: combines live inflight concurrency, EWMA TTFT, and configured weight.
    pub fn p2c_load_score(&self) -> u64 {
        let inflight = self.inflight.load(Ordering::Relaxed) as u64;
        let ttft = self.ewma_ttft_ms.load(Ordering::Relaxed);
        let weight = u64::from(self.config.weight.max(1));
        (inflight + 1)
            .saturating_mul(ttft)
            .saturating_mul(100)
            / weight
    }

    pub fn snapshot(&self) -> ChannelSnapshot {
        let ewma_ttft_ms = if self.has_ttft_sample.load(Ordering::Relaxed) {
            Some(self.ewma_ttft_ms.load(Ordering::Relaxed))
        } else {
            None
        };
        ChannelSnapshot {
            id: self.config.id.clone(),
            protocol: format!("{:?}", self.config.protocol).to_lowercase(),
            base_url: self.config.base_url.clone(),
            enabled: self.config.enabled,
            weight: self.config.weight,
            models: self.config.models.clone(),
            proxy_url: self.config.proxy_url.clone(),
            fallback_models: self.config.fallback_models.clone(),
            quota_endpoint: self.config.quota_endpoint.clone(),
            offline_fallback: self.config.offline_fallback,
            health: self.health(),
            cooldown_remaining_secs: self.cooldown_remaining_secs(),
            quarantine_remaining_secs: self.quarantine_remaining_secs(),
            inflight: self.inflight.load(Ordering::Relaxed),
            ewma_ttft_ms,
            total_requests: self.total_requests.load(Ordering::Relaxed),
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct ChannelSnapshot {
    pub id: String,
    pub protocol: String,
    pub base_url: String,
    pub enabled: bool,
    pub weight: u32,
    pub models: Vec<String>,
    pub proxy_url: Option<String>,
    pub fallback_models: Vec<String>,
    pub quota_endpoint: Option<String>,
    pub offline_fallback: bool,
    pub health: ChannelHealth,
    pub cooldown_remaining_secs: u64,
    pub quarantine_remaining_secs: u64,
    pub inflight: usize,
    pub ewma_ttft_ms: Option<u64>,
    pub total_requests: u64,
}

#[derive(Debug, Clone)]
struct StickyBinding {
    channel_id: String,
    expires_at: Instant,
}

#[derive(Debug, Clone)]
pub struct RouteDecision {
    pub channel: Arc<ChannelRuntime>,
    pub actual_model: String,
    pub mode: RouteMode,
    pub sticky_hit: bool,
    pub prefix_key: String,
    pub offline_fallback_used: bool,
}

/// Prefix-Cache Sticky + P2C Concurrency-Aware Router with Explicit Lock / Stage-Aware Modes.
pub struct StickyP2cRouter {
    channels: RwLock<Vec<Arc<ChannelRuntime>>>,
    sticky_map: DashMap<String, StickyBinding>,
    config: RoutingConfig,
    epoch: Instant,
}

impl StickyP2cRouter {
    pub fn new(channel_configs: Vec<ChannelConfig>, config: RoutingConfig) -> Self {
        let epoch = Instant::now();
        let channels = channel_configs
            .into_iter()
            .map(|c| Arc::new(ChannelRuntime::new(c, epoch)))
            .collect();
        Self {
            channels: RwLock::new(channels),
            sticky_map: DashMap::new(),
            config,
            epoch,
        }
    }

    /// Hot-merges remote team-pool channels synced from Cloud Control Plane (upsert by channel ID).
    pub fn merge_remote_channels(&self, remote: Vec<ChannelConfig>) -> usize {
        let mut guard = self.channels.write().unwrap_or_else(|e| e.into_inner());
        let mut merged = 0usize;
        for mut cfg in remote {
            cfg.normalize();
            let rt = Arc::new(ChannelRuntime::new(cfg.clone(), self.epoch));
            if let Some(slot) = guard.iter_mut().find(|c| c.config.id == cfg.id) {
                *slot = rt;
            } else {
                guard.push(rt);
            }
            merged += 1;
        }
        merged
    }

    pub fn upsert_channel(&self, mut cfg: ChannelConfig) -> ChannelSnapshot {
        cfg.normalize();
        let rt = Arc::new(ChannelRuntime::new(cfg.clone(), self.epoch));
        let snap = rt.snapshot();
        let mut guard = self.channels.write().unwrap_or_else(|e| e.into_inner());
        if let Some(slot) = guard.iter_mut().find(|c| c.config.id == cfg.id) {
            *slot = rt;
        } else {
            guard.push(rt);
        }
        snap
    }

    pub fn delete_channel(&self, id: &str) -> bool {
        let mut guard = self.channels.write().unwrap_or_else(|e| e.into_inner());
        let before = guard.len();
        guard.retain(|c| c.config.id != id);
        if guard.len() < before {
            self.sticky_map.retain(|_, v| v.channel_id != id);
            true
        } else {
            false
        }
    }

    pub fn set_channel_enabled(&self, id: &str, enabled: bool) -> Option<ChannelSnapshot> {
        let mut guard = self.channels.write().unwrap_or_else(|e| e.into_inner());
        let slot = guard.iter_mut().find(|c| c.config.id == id)?;
        let mut next_cfg = slot.config.clone();
        next_cfg.enabled = enabled;
        let rt = Arc::new(ChannelRuntime::new(next_cfg, self.epoch));
        let snap = rt.snapshot();
        *slot = rt;
        if !enabled {
            self.sticky_map.retain(|_, v| v.channel_id != id);
        }
        Some(snap)
    }

    pub fn get_channel(&self, id: &str) -> Option<Arc<ChannelRuntime>> {
        self.channels
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .find(|c| c.config.id == id)
            .cloned()
    }

    pub fn channel_configs(&self) -> Vec<ChannelConfig> {
        self.channels
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .map(|c| c.config.clone())
            .collect()
    }

    pub fn channel_count(&self) -> usize {
        self.channels
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .len()
    }

    /// Resets sticky bindings and channel cooldowns (used by test harness isolation).
    pub fn reset_state(&self) {
        self.sticky_map.clear();
        let guard = self.channels.read().unwrap_or_else(|e| e.into_inner());
        for ch in guard.iter() {
            ch.clear_health_penalties();
        }
    }

    /// Resolves target model transparently: Lock mode never downgrades; Stage-Aware applies explicit rules.
    pub fn resolve_model(&self, req: &CanonicalRequest, override_mode: Option<RouteMode>) -> (String, RouteMode) {
        let mode = override_mode.unwrap_or(self.config.default_mode);
        match mode {
            RouteMode::Lock => (req.model.clone(), RouteMode::Lock),
            RouteMode::StageAware => {
                if let Some(ref stage) = req.task_stage {
                    if let Some(mapped) = self.config.stage_models.get(stage) {
                        return (mapped.clone(), RouteMode::StageAware);
                    }
                }
                (req.model.clone(), RouteMode::StageAware)
            }
        }
    }

    /// Selects an upstream channel using:
    /// 1. Prefix-Cache Sticky Affinity (if bound & healthy & not excluded)
    /// 2. P2C (Power of Two Choices) across healthy candidates supporting the resolved model
    pub fn select_channel(
        &self,
        req: &CanonicalRequest,
        override_mode: Option<RouteMode>,
        excluded_ids: &[String],
    ) -> Option<RouteDecision> {
        let (actual_model, mode) = self.resolve_model(req, override_mode);
        let prefix_key = req.prefix_fingerprint(self.config.prefix_hash_turns);
        let sticky_key = format!("{}::{}", actual_model, prefix_key);
        let now = Instant::now();

        let channels_guard = self.channels.read().unwrap_or_else(|e| e.into_inner());

        if let Some(binding) = self.sticky_map.get(&sticky_key) {
            if binding.expires_at > now && !excluded_ids.contains(&binding.channel_id) {
                if let Some(ch) = channels_guard
                    .iter()
                    .find(|c| c.config.id == binding.channel_id && c.is_available())
                {
                    ch.total_requests.fetch_add(1, Ordering::Relaxed);
                    return Some(RouteDecision {
                        channel: Arc::clone(ch),
                        actual_model,
                        mode,
                        sticky_hit: true,
                        prefix_key,
                        offline_fallback_used: false,
                    });
                }
            }
        }

        let mut resolved_model = actual_model.clone();
        let mut offline_fallback_used = false;

        // Prefer non-offline-fallback channels first when matching requested model;
        // if the user directly requested a local model, include local channels that explicitly list it.
        let mut candidates: Vec<&Arc<ChannelRuntime>> = channels_guard
            .iter()
            .filter(|c| {
                c.is_available()
                    && !c.config.offline_fallback
                    && c.config.supports_model(&resolved_model)
                    && !excluded_ids.contains(&c.config.id)
            })
            .collect();

        if candidates.is_empty() {
            candidates = channels_guard
                .iter()
                .filter(|c| {
                    c.is_available()
                        && c.config.supports_model(&resolved_model)
                        && !excluded_ids.contains(&c.config.id)
                })
                .collect();
        }

        // Explicit fallback_models chain (ONLY active in StageAware mode — Lock mode NEVER silently downgrades).
        if candidates.is_empty() && mode == RouteMode::StageAware {
            let fallback_list: Vec<String> = channels_guard
                .iter()
                .filter(|c| c.config.supports_model(&actual_model))
                .flat_map(|c| c.config.fallback_models.iter().cloned())
                .collect();
            for fb_model in fallback_list {
                let fb_candidates: Vec<&Arc<ChannelRuntime>> = channels_guard
                    .iter()
                    .filter(|c| {
                        c.is_available()
                            && c.config.supports_model(&fb_model)
                            && !excluded_ids.contains(&c.config.id)
                    })
                    .collect();
                if !fb_candidates.is_empty() {
                    resolved_model = fb_model;
                    candidates = fb_candidates;
                    break;
                }
            }
        }

        // Offline Fallback: when ALL primary/cloud channels are tripped/unavailable/excluded,
        // transparently fail over to any healthy channel explicitly marked `offline_fallback = true`.
        if candidates.is_empty() {
            let offline_candidates: Vec<&Arc<ChannelRuntime>> = channels_guard
                .iter()
                .filter(|c| {
                    c.is_available()
                        && c.config.offline_fallback
                        && !excluded_ids.contains(&c.config.id)
                })
                .collect();
            if !offline_candidates.is_empty() {
                let first_local_model = offline_candidates[0]
                    .config
                    .models
                    .first()
                    .cloned()
                    .unwrap_or_else(|| actual_model.clone());
                resolved_model = first_local_model;
                candidates = offline_candidates;
                offline_fallback_used = true;
            }
        }

        if candidates.is_empty() {
            return None;
        }

        let chosen = if candidates.len() == 1 {
            candidates[0]
        } else {
            let mut rng = rand::thread_rng();
            let pair: Vec<&&Arc<ChannelRuntime>> = candidates.choose_multiple(&mut rng, 2).collect();
            if pair[0].p2c_load_score() <= pair[1].p2c_load_score() {
                pair[0]
            } else {
                pair[1]
            }
        };

        let resolved_sticky_key = format!("{}::{}", resolved_model, prefix_key);
        self.sticky_map.insert(
            resolved_sticky_key,
            StickyBinding {
                channel_id: chosen.config.id.clone(),
                expires_at: now + Duration::from_secs(self.config.sticky_ttl_secs),
            },
        );
        chosen.total_requests.fetch_add(1, Ordering::Relaxed);

        Some(RouteDecision {
            channel: Arc::clone(chosen),
            actual_model: resolved_model,
            mode,
            sticky_hit: false,
            prefix_key,
            offline_fallback_used,
        })
    }

    pub fn unbind_affinity(&self, actual_model: &str, prefix_key: &str) {
        let sticky_key = format!("{}::{}", actual_model, prefix_key);
        self.sticky_map.remove(&sticky_key);
    }

    pub fn snapshots(&self) -> Vec<ChannelSnapshot> {
        self.channels
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .map(|c| c.snapshot())
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::UpstreamProtocol;
    use crate::ir::{CanonicalMessage, ContentPart, Role};

    fn make_req(session: Option<&str>) -> CanonicalRequest {
        CanonicalRequest {
            model: "claude-sonnet-4-20250514".into(),
            system: vec![ContentPart::Text {
                text: "Agent system".into(),
                cache_control: None,
            }],
            messages: vec![CanonicalMessage {
                role: Role::User,
                parts: vec![ContentPart::Text {
                    text: "hello".into(),
                    cache_control: None,
                }],
            }],
            tools: vec![],
            tool_choice: None,
            max_tokens: Some(1024),
            temperature: None,
            top_p: None,
            stream: true,
            reasoning: None,
            opaque_state: Default::default(),
            session_id: session.map(String::from),
            task_stage: None,
            extra_fields: Default::default(),
        }
    }

    #[test]
    fn sticky_routing_pins_session_until_channel_cools_down() {
        let channels = vec![
            ChannelConfig {
                id: "ch-a".into(),
                protocol: UpstreamProtocol::Anthropic,
                base_url: "http://localhost:1".into(),
                api_key: "k1".into(),
                weight: 100,
                enabled: true,
                models: vec!["claude-sonnet-4-20250514".into()],
                model_map: Default::default(),
                proxy_url: None,
                fallback_models: vec![],
                quota_endpoint: None,
                offline_fallback: false,
            },
            ChannelConfig {
                id: "ch-b".into(),
                protocol: UpstreamProtocol::Anthropic,
                base_url: "http://localhost:2".into(),
                api_key: "k2".into(),
                weight: 100,
                enabled: true,
                models: vec!["claude-sonnet-4-20250514".into()],
                model_map: Default::default(),
                proxy_url: None,
                fallback_models: vec![],
                quota_endpoint: None,
                offline_fallback: false,
            },
        ];
        let router = StickyP2cRouter::new(channels, RoutingConfig::default());
        let req = make_req(Some("sess-42"));

        let d1 = router.select_channel(&req, None, &[]).unwrap();
        assert!(!d1.sticky_hit);
        let pinned_id = d1.channel.config.id.clone();

        for _ in 0..5 {
            let dn = router.select_channel(&req, None, &[]).unwrap();
            assert!(dn.sticky_hit);
            assert_eq!(dn.channel.config.id, pinned_id);
        }

        d1.channel.mark_cooldown(Duration::from_secs(60));
        assert!(d1.channel.cooldown_remaining_secs() > 0);
        let d_failover = router.select_channel(&req, None, &[]).unwrap();
        assert!(!d_failover.sticky_hit);
        assert_ne!(d_failover.channel.config.id, pinned_id);
    }
}
