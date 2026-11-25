use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Mutex;

/// Reference Redis Lua script bundled directly into the binary for production Redis deployment.
pub const REDIS_BILLING_LUA: &str = include_str!("../deploy/redis_billing.lua");

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReservationStatus {
    Reserved,
    Settled,
    Released,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LedgerEntry {
    pub reservation_id: String,
    pub tenant_id: String,
    pub model: String,
    pub reserved_micros: u64,
    pub settled_micros: Option<u64>,
    pub status: ReservationStatus,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BalanceSnapshot {
    pub tenant_id: String,
    pub available_micros: u64,
    pub reserved_micros: u64,
    pub total_settled_micros: u64,
    pub ledger_count: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BillingOpResult {
    pub ok: bool,
    pub code: String,
    pub available_micros: u64,
    pub reserved_micros: u64,
}

#[derive(Debug, Clone)]
struct TenantWallet {
    available_micros: u64,
    reserved_micros: u64,
    total_settled_micros: u64,
}

/// Atomic Two-Phase Reserve / Settle / Release Billing Ledger.
/// Mirrors `deploy/redis_billing.lua` semantics exactly so local dev & Redis-backed prod behave identically.
pub struct BillingLedger {
    inner: Mutex<LedgerInner>,
}

struct LedgerInner {
    wallets: HashMap<String, TenantWallet>,
    reservations: HashMap<String, LedgerEntry>,
}

impl BillingLedger {
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(LedgerInner {
                wallets: HashMap::new(),
                reservations: HashMap::new(),
            }),
        }
    }

    pub fn seed_tenant(&self, tenant_id: &str, initial_micros: u64) {
        let mut g = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        g.wallets.insert(
            tenant_id.to_string(),
            TenantWallet {
                available_micros: initial_micros,
                reserved_micros: 0,
                total_settled_micros: 0,
            },
        );
    }

    pub fn balance(&self, tenant_id: &str) -> Option<BalanceSnapshot> {
        let g = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let w = g.wallets.get(tenant_id)?;
        let ledger_count = g
            .reservations
            .values()
            .filter(|r| r.tenant_id == tenant_id)
            .count();
        Some(BalanceSnapshot {
            tenant_id: tenant_id.to_string(),
            available_micros: w.available_micros,
            reserved_micros: w.reserved_micros,
            total_settled_micros: w.total_settled_micros,
            ledger_count,
        })
    }

    /// Phase 1: Atomic pre-deduction (idempotent by `reservation_id`).
    pub fn reserve(
        &self,
        tenant_id: &str,
        reservation_id: &str,
        model: &str,
        estimated_micros: u64,
    ) -> BillingOpResult {
        let mut g = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(existing) = g.reservations.get(reservation_id) {
            let w = g.wallets.get(&existing.tenant_id);
            return BillingOpResult {
                ok: existing.status == ReservationStatus::Reserved,
                code: "IDEMPOTENT_OK".into(),
                available_micros: w.map(|x| x.available_micros).unwrap_or(0),
                reserved_micros: w.map(|x| x.reserved_micros).unwrap_or(0),
            };
        }

        let Some(wallet) = g.wallets.get_mut(tenant_id) else {
            return BillingOpResult {
                ok: false,
                code: "TENANT_NOT_FOUND".into(),
                available_micros: 0,
                reserved_micros: 0,
            };
        };

        if wallet.available_micros < estimated_micros {
            return BillingOpResult {
                ok: false,
                code: "INSUFFICIENT_BALANCE".into(),
                available_micros: wallet.available_micros,
                reserved_micros: wallet.reserved_micros,
            };
        }

        wallet.available_micros -= estimated_micros;
        wallet.reserved_micros += estimated_micros;
        let avail = wallet.available_micros;
        let resv = wallet.reserved_micros;

        g.reservations.insert(
            reservation_id.to_string(),
            LedgerEntry {
                reservation_id: reservation_id.to_string(),
                tenant_id: tenant_id.to_string(),
                model: model.to_string(),
                reserved_micros: estimated_micros,
                settled_micros: None,
                status: ReservationStatus::Reserved,
            },
        );

        BillingOpResult {
            ok: true,
            code: "RESERVED".into(),
            available_micros: avail,
            reserved_micros: resv,
        }
    }

    /// Phase 2: Atomic final settlement with exact refund of over-reserved micro-credits.
    pub fn settle(&self, reservation_id: &str, actual_micros: u64) -> BillingOpResult {
        let mut g = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let Some((tenant_id, prev_status, reserved)) = g
            .reservations
            .get(reservation_id)
            .map(|r| (r.tenant_id.clone(), r.status, r.reserved_micros))
        else {
            return BillingOpResult {
                ok: false,
                code: "NOT_FOUND".into(),
                available_micros: 0,
                reserved_micros: 0,
            };
        };

        if prev_status == ReservationStatus::Settled {
            let w = g.wallets.get(&tenant_id);
            return BillingOpResult {
                ok: true,
                code: "IDEMPOTENT_SETTLED".into(),
                available_micros: w.map(|x| x.available_micros).unwrap_or(0),
                reserved_micros: w.map(|x| x.reserved_micros).unwrap_or(0),
            };
        }

        if let Some(wallet) = g.wallets.get_mut(&tenant_id) {
            wallet.reserved_micros = wallet.reserved_micros.saturating_sub(reserved);
            if reserved >= actual_micros {
                wallet.available_micros += reserved - actual_micros;
            } else {
                let extra = actual_micros - reserved;
                wallet.available_micros = wallet.available_micros.saturating_sub(extra);
            }
            wallet.total_settled_micros += actual_micros;
        }

        if let Some(entry) = g.reservations.get_mut(reservation_id) {
            entry.status = ReservationStatus::Settled;
            entry.settled_micros = Some(actual_micros);
        }

        let w = g.wallets.get(&tenant_id);
        BillingOpResult {
            ok: true,
            code: "SETTLED".into(),
            available_micros: w.map(|x| x.available_micros).unwrap_or(0),
            reserved_micros: w.map(|x| x.reserved_micros).unwrap_or(0),
        }
    }

    /// Releases a reservation back to available balance when an upstream request fails before billing.
    pub fn release(&self, reservation_id: &str) -> BillingOpResult {
        let mut g = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let Some((tenant_id, prev_status, reserved)) = g
            .reservations
            .get(reservation_id)
            .map(|r| (r.tenant_id.clone(), r.status, r.reserved_micros))
        else {
            return BillingOpResult {
                ok: true,
                code: "ALREADY_CLOSED".into(),
                available_micros: 0,
                reserved_micros: 0,
            };
        };

        if prev_status != ReservationStatus::Reserved {
            let w = g.wallets.get(&tenant_id);
            return BillingOpResult {
                ok: true,
                code: "ALREADY_CLOSED".into(),
                available_micros: w.map(|x| x.available_micros).unwrap_or(0),
                reserved_micros: w.map(|x| x.reserved_micros).unwrap_or(0),
            };
        }

        if let Some(wallet) = g.wallets.get_mut(&tenant_id) {
            wallet.reserved_micros = wallet.reserved_micros.saturating_sub(reserved);
            wallet.available_micros += reserved;
        }
        if let Some(entry) = g.reservations.get_mut(reservation_id) {
            entry.status = ReservationStatus::Released;
            entry.settled_micros = Some(0);
        }

        let w = g.wallets.get(&tenant_id);
        BillingOpResult {
            ok: true,
            code: "RELEASED".into(),
            available_micros: w.map(|x| x.available_micros).unwrap_or(0),
            reserved_micros: w.map(|x| x.reserved_micros).unwrap_or(0),
        }
    }
}

impl Default for BillingLedger {
    fn default() -> Self {
        Self::new()
    }
}

/// Computes cost in micro-USD (1 USD = 1_000_000 micros) with explicit prompt-cache discount (90% off).
pub fn estimate_cost_micros(
    model: &str,
    input_tokens: u64,
    cache_read_tokens: u64,
    output_tokens: u64,
) -> u64 {
    let lower = model.to_lowercase();
    // (input $/1M, output $/1M) in micros per 1M tokens -> micro-USD per token
    let (in_per_1m, out_per_1m): (u64, u64) = if lower.contains("mini") || lower.contains("haiku") || lower.contains("flash") {
        (500_000, 1_500_000)
    } else if lower.contains("opus") {
        (15_000_000, 75_000_000)
    } else {
        (3_000_000, 15_000_000)
    };
    let cache_per_1m = in_per_1m / 10;
    let total = input_tokens
        .saturating_mul(in_per_1m)
        .saturating_add(cache_read_tokens.saturating_mul(cache_per_1m))
        .saturating_add(output_tokens.saturating_mul(out_per_1m));
    (total / 1_000_000).max(1)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn two_phase_reserve_settle_refunds_unused_credits_and_blocks_overspend() {
        let ledger = BillingLedger::new();
        ledger.seed_tenant("team-a", 10_000);

        let r1 = ledger.reserve("team-a", "resv-1", "claude-sonnet-4-20250514", 6_000);
        assert!(r1.ok && r1.code == "RESERVED");
        assert_eq!(r1.available_micros, 4_000);
        assert_eq!(r1.reserved_micros, 6_000);

        // Concurrent request exceeding remaining balance must fail cleanly
        let r2 = ledger.reserve("team-a", "resv-2", "claude-sonnet-4-20250514", 5_000);
        assert!(!r2.ok && r2.code == "INSUFFICIENT_BALANCE");

        // Idempotent retry of resv-1 must not double-deduct
        let r1_dup = ledger.reserve("team-a", "resv-1", "claude-sonnet-4-20250514", 6_000);
        assert!(r1_dup.ok && r1_dup.code == "IDEMPOTENT_OK");
        assert_eq!(r1_dup.available_micros, 4_000);

        // Settle resv-1 for 1_500 -> 4_500 refunded to available (total available = 8_500)
        let s1 = ledger.settle("resv-1", 1_500);
        assert!(s1.ok && s1.code == "SETTLED");
        assert_eq!(s1.available_micros, 8_500);
        assert_eq!(s1.reserved_micros, 0);

        // Embedded Lua script is non-empty and contains atomic commands
        assert!(REDIS_BILLING_LUA.contains("DECRBY") && REDIS_BILLING_LUA.contains("SETTLE"));
    }
}
