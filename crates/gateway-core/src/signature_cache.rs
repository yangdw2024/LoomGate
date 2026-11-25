use dashmap::DashMap;
use std::time::{Duration, Instant};

#[derive(Debug, Clone)]
struct CachedSignature {
    signature: String,
    expires_at: Instant,
}

/// Sidecar TTL cache for opaque reasoning signatures (e.g., Anthropic `signature`,
/// Gemini `thought_signature`), indexed by `(prefix_key, block_index)` so multi-block
/// and multi-turn reasoning chains never overwrite each other or break.
pub struct SignatureCache {
    store: DashMap<String, CachedSignature>,
    ttl: Duration,
}

impl SignatureCache {
    pub fn new(ttl: Duration) -> Self {
        Self {
            store: DashMap::new(),
            ttl,
        }
    }

    fn key(prefix_key: &str, block_index: usize) -> String {
        format!("{}::{}", prefix_key, block_index)
    }

    pub fn put(&self, prefix_key: &str, block_index: usize, signature: String) {
        self.store.insert(
            Self::key(prefix_key, block_index),
            CachedSignature {
                signature,
                expires_at: Instant::now() + self.ttl,
            },
        );
    }

    pub fn get(&self, prefix_key: &str, block_index: usize) -> Option<String> {
        let k = Self::key(prefix_key, block_index);
        let now = Instant::now();
        if let Some(entry) = self.store.get(&k) {
            if entry.expires_at > now {
                return Some(entry.signature.clone());
            }
        }
        self.store.remove(&k);
        None
    }

    pub fn clear(&self) {
        self.store.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn caches_multiple_blocks_independently() {
        let cache = SignatureCache::new(Duration::from_secs(60));
        cache.put("pfx:1", 0, "sig_a".into());
        cache.put("pfx:1", 1, "sig_b".into());
        assert_eq!(cache.get("pfx:1", 0).as_deref(), Some("sig_a"));
        assert_eq!(cache.get("pfx:1", 1).as_deref(), Some("sig_b"));
    }
}
