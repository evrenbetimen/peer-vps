//! Solution C — economic SLA enforcement.
//!
//! When a host is declared down, look at its presence history: if its last
//! event before disappearing was a completed hibernation, it left cleanly and
//! nothing happens. Otherwise record `offline_dirty` and slash its collateral
//! for every renter it was serving, proportional to their hourly rate.

use rusqlite::params;
use serde::Serialize;

use crate::Result;
use crate::billing::Collateral;
use crate::storage::presence::{self, PresenceEvent};
use crate::storage::{Store, now_secs};

#[derive(Debug, Clone, Copy)]
pub struct SlaPolicy {
    /// Compensation = renter's rate × this many seconds (default: one hour).
    pub penalty_seconds: i64,
    /// Floor per affected instance, in µcredits.
    pub min_penalty: i64,
}

impl Default for SlaPolicy {
    fn default() -> Self {
        Self { penalty_seconds: 3_600, min_penalty: 1_000_000 }
    }
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct SlaVerdict {
    pub node: String,
    pub clean: bool,
    pub slashed: Vec<(String, i64)>,
}

#[derive(Debug, Clone)]
pub struct SlaEnforcer {
    store: Store,
    collateral: Collateral,
    policy: SlaPolicy,
}

impl SlaEnforcer {
    pub fn new(store: Store, collateral: Collateral, policy: SlaPolicy) -> Self {
        Self { store, collateral, policy }
    }

    pub async fn record_online(&self, node: &str) -> Result<()> {
        let node = node.to_owned();
        self.store.call(move |c| presence::record(c, &node, PresenceEvent::Online, now_secs())).await
    }

    /// Called by the failover controller once heartbeats declare `node` down.
    pub async fn on_node_down(&self, node: &str) -> Result<SlaVerdict> {
        let n = node.to_owned();
        let policy = self.policy;
        let (clean, victims) = self
            .store
            .call(move |c| {
                let clean = matches!(presence::last(c, &n)?, Some((PresenceEvent::Hibernated, _)));
                let event = if clean { PresenceEvent::HeartbeatLost } else { PresenceEvent::OfflineDirty };
                presence::record(c, &n, event, now_secs())?;
                if clean {
                    return Ok((true, Vec::new()));
                }
                let mut stmt =
                    c.prepare("SELECT renter, rate_per_sec FROM instances WHERE provider = ?1 AND state = 'running'")?;
                let rows = stmt.query_map(params![n], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))?;
                let victims = rows
                    .map(|r| r.map(|(renter, rate)| (renter, (rate * policy.penalty_seconds).max(policy.min_penalty))))
                    .collect::<Result<Vec<_>, _>>()?;
                Ok((false, victims))
            })
            .await?;

        let mut slashed = Vec::new();
        for (renter, amount) in victims {
            let got = self.collateral.slash(node, &renter, amount, "host vanished without hibernation").await?;
            if got > 0 {
                slashed.push((renter, got));
            }
        }
        Ok(SlaVerdict { node: node.to_owned(), clean, slashed })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::billing::{AccountKind, Ledger, MICROS_PER_CREDIT as C, UsageMeter};
    use crate::events::EventBus;

    async fn setup() -> (Ledger, SlaEnforcer, UsageMeter) {
        let store = Store::in_memory().expect("db");
        let ledger = Ledger::new(store.clone(), EventBus::default()).expect("ledger");
        ledger.open_account("host", AccountKind::Provider).await.expect("h");
        ledger.open_account("renter", AccountKind::Renter).await.expect("r");
        ledger.top_up("host", 100 * C, "t").await.expect("top up");
        let col = Collateral::new(ledger.clone(), 10 * C);
        col.lock("host", 20 * C).await.expect("lock");
        let meter = UsageMeter::new(ledger.clone(), 0);
        meter.start("vm1", "renter", "host", 1_000, now_secs()).await.expect("start");
        (ledger, SlaEnforcer::new(store, col, SlaPolicy::default()), meter)
    }

    #[tokio::test]
    async fn dirty_exit_slashes_clean_exit_does_not() {
        let (ledger, sla, _m) = setup().await;
        sla.record_online("host").await.expect("online");
        let v = sla.on_node_down("host").await.expect("down");
        assert!(!v.clean);
        assert_eq!(v.slashed, vec![("renter".to_owned(), 3_600_000)]);
        assert_eq!(ledger.balance("renter").await.expect("bal"), 3_600_000);

        sla.store.call(|c| presence::record(c, "host", PresenceEvent::Hibernated, now_secs() + 1)).await.expect("rec");
        let v = sla.on_node_down("host").await.expect("down");
        assert!(v.clean && v.slashed.is_empty());
    }
}
