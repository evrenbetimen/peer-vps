//! Per-second usage metering.
//!
//! Charges are derived from wall-clock time since `started_at` minus seconds
//! already billed, so a delayed or skipped tick never loses or double-charges
//! a second. Each tick settles every running instance in its own transaction:
//! renter → provider (minus the platform fee → treasury).

use std::time::Duration;

use rusqlite::{OptionalExtension, params};
use serde::Serialize;

use super::ledger::{self, Ledger, TREASURY};
use crate::events::NodeEvent;
use crate::storage::now_secs;
use crate::{Error, Result};

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Settlement {
    pub instance: String,
    pub seconds: i64,
    pub charged: i64,
    /// Renter ran out of credits; the instance must be stopped (scaled to zero).
    pub exhausted: bool,
}

#[derive(Debug, Clone)]
pub struct UsageMeter {
    ledger: Ledger,
    /// Platform fee in basis points (100 = 1%).
    fee_bps: i64,
}

impl UsageMeter {
    pub fn new(ledger: Ledger, fee_bps: i64) -> Self {
        Self { ledger, fee_bps: fee_bps.clamp(0, 10_000) }
    }

    /// Start billing an instance at `rate_per_sec` µcredits.
    pub async fn start(&self, instance: &str, renter: &str, provider: &str, rate_per_sec: i64, at: i64) -> Result<()> {
        if rate_per_sec < 0 {
            return Err(Error::Invalid("rate must be >= 0".into()));
        }
        let (i, r, p) = (instance.to_owned(), renter.to_owned(), provider.to_owned());
        self.ledger
            .store()
            .call(move |c| {
                c.execute(
                    "INSERT INTO instances(id, renter, provider, rate_per_sec, state, started_at)
                     VALUES (?1, ?2, ?3, ?4, 'running', ?5)",
                    params![i, r, p, rate_per_sec, at],
                )?;
                Ok(())
            })
            .await
    }

    /// Settle outstanding seconds and stop billing.
    pub async fn stop(&self, instance: &str, at: i64) -> Result<Settlement> {
        let s = self.settle_one(instance.to_owned(), at).await?;
        let id = instance.to_owned();
        self.ledger
            .store()
            .call(move |c| {
                c.execute("UPDATE instances SET state = 'stopped', stopped_at = ?2 WHERE id = ?1", params![id, at])?;
                Ok(())
            })
            .await?;
        Ok(s)
    }

    /// Bill every running instance up to `now`.
    pub async fn tick(&self, now: i64) -> Result<Vec<Settlement>> {
        let ids: Vec<String> = self
            .ledger
            .store()
            .call(|c| {
                let mut stmt = c.prepare("SELECT id FROM instances WHERE state = 'running'")?;
                let ids = stmt.query_map([], |r| r.get(0))?.collect::<Result<Vec<String>, _>>()?;
                Ok(ids)
            })
            .await?;
        let mut out = Vec::with_capacity(ids.len());
        for id in ids {
            let s = self.settle_one(id, now).await?;
            if s.exhausted {
                let id = s.instance.clone();
                self.ledger
                    .store()
                    .call(move |c| {
                        c.execute(
                            "UPDATE instances SET state = 'suspended', stopped_at = ?2 WHERE id = ?1",
                            params![id, now],
                        )?;
                        Ok(())
                    })
                    .await?;
            }
            if s.seconds > 0 || s.exhausted {
                out.push(s);
            }
        }
        Ok(out)
    }

    /// Run [`Self::tick`] every second until the task is dropped. Exhausted
    /// instances are reported on `on_exhausted` so the scheduler can scale them to zero.
    pub async fn run(self, on_exhausted: tokio::sync::mpsc::Sender<String>) {
        let mut every = tokio::time::interval(Duration::from_secs(1));
        every.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            every.tick().await;
            match self.tick(now_secs()).await {
                Ok(settled) => {
                    for s in settled.into_iter().filter(|s| s.exhausted) {
                        let _ = on_exhausted.send(s.instance).await;
                    }
                }
                Err(e) => tracing::error!(error = %e, "billing tick failed"),
            }
        }
    }

    async fn settle_one(&self, instance: String, now: i64) -> Result<Settlement> {
        let fee_bps = self.fee_bps;
        let (settlement, balances) = self
            .ledger
            .store()
            .call(move |c| {
                let tx = c.transaction()?;
                let row: Option<(String, String, i64, i64, i64)> = tx
                    .query_row(
                        "SELECT renter, provider, rate_per_sec, started_at, billed_seconds FROM instances
                         WHERE id = ?1 AND state = 'running'",
                        [&instance],
                        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
                    )
                    .optional()?;
                let Some((renter, provider, rate, started, billed)) = row else {
                    return Ok((Settlement { instance, seconds: 0, charged: 0, exhausted: false }, vec![]));
                };
                let due = (now - started - billed).max(0);
                if due == 0 || rate == 0 {
                    tx.execute(
                        "UPDATE instances SET billed_seconds = billed_seconds + ?2 WHERE id = ?1",
                        params![instance, due],
                    )?;
                    tx.commit()?;
                    return Ok((Settlement { instance, seconds: due, charged: 0, exhausted: false }, vec![]));
                }
                // Charge only whole seconds the renter can afford.
                let available = ledger::balance(&tx, &renter)?;
                let affordable = (available / rate).min(due);
                let exhausted = affordable < due;
                let charged = affordable * rate;
                let mut balances = Vec::new();
                if charged > 0 {
                    let fee = charged * fee_bps / 10_000;
                    let reference = Some(instance.as_str());
                    balances.push((renter.clone(), ledger::apply(&tx, &renter, -charged, "usage", reference)?));
                    balances
                        .push((provider.clone(), ledger::apply(&tx, &provider, charged - fee, "usage", reference)?));
                    if fee > 0 {
                        ledger::apply(&tx, TREASURY, fee, "platform_fee", reference)?;
                    }
                }
                tx.execute(
                    "UPDATE instances SET billed_seconds = billed_seconds + ?2 WHERE id = ?1",
                    params![instance, affordable],
                )?;
                tx.commit()?;
                Ok((Settlement { instance, seconds: affordable, charged, exhausted }, balances))
            })
            .await?;
        for (account, balance) in balances {
            self.ledger.events().publish(NodeEvent::Balance { account, balance });
        }
        Ok(settlement)
    }
}

#[cfg(test)]
mod tests {
    use super::super::ledger::{AccountKind, MICROS_PER_CREDIT};
    use super::*;
    use crate::events::EventBus;
    use crate::storage::Store;

    async fn setup() -> (Ledger, UsageMeter) {
        let l = Ledger::new(Store::in_memory().expect("db"), EventBus::default()).expect("ledger");
        l.open_account("renter", AccountKind::Renter).await.expect("r");
        l.open_account("host", AccountKind::Provider).await.expect("h");
        l.top_up("renter", MICROS_PER_CREDIT, "pi").await.expect("top up");
        let m = UsageMeter::new(l.clone(), 1_000); // 10% fee
        (l, m)
    }

    #[tokio::test]
    async fn bills_whole_seconds_without_drift() {
        let (l, m) = setup().await;
        m.start("vm1", "renter", "host", 1_000, 100).await.expect("start");
        m.tick(105).await.expect("tick");
        m.tick(105).await.expect("idempotent tick");
        m.tick(110).await.expect("tick");
        // 10 s × 1000 µc = 10_000 µc; provider gets 90%.
        assert_eq!(l.balance("renter").await.expect("r"), MICROS_PER_CREDIT - 10_000);
        assert_eq!(l.balance("host").await.expect("h"), 9_000);
        assert_eq!(l.balance(TREASURY).await.expect("t"), 1_000);
    }

    #[tokio::test]
    async fn suspends_when_credits_run_out() {
        let (l, m) = setup().await;
        m.start("vm1", "renter", "host", 300_000, 0).await.expect("start");
        let s = m.tick(10).await.expect("tick");
        assert_eq!(s.len(), 1);
        assert!(s[0].exhausted);
        assert_eq!(s[0].seconds, 3);
        assert_eq!(l.balance("renter").await.expect("r"), 100_000);
        assert!(m.tick(20).await.expect("tick").is_empty(), "suspended instances are not billed");
    }
}
