//! Provider collateral, slashing, and pooled staking.
//!
//! A provider moves credits from its balance into `collateral.locked` before
//! the scheduler will place workloads on it. If the node disappears without a
//! clean hibernation, [`Collateral::slash`] moves locked credits straight to the
//! affected renter. Several smaller providers can stake into a pool that, once
//! its total crosses `min_total`, qualifies for enterprise GPU workloads; pool
//! slashes are shared pro rata.

use rusqlite::{OptionalExtension, Transaction, params};
use serde::Serialize;

use super::ledger::{self, Ledger};
use crate::events::NodeEvent;
use crate::storage::now_secs;
use crate::{Error, Result};

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CollateralState {
    pub provider: String,
    pub locked: i64,
    pub minimum: i64,
    pub eligible: bool,
}

#[derive(Debug, Clone)]
pub struct Collateral {
    ledger: Ledger,
    /// Minimum locked µcredits before a provider may accept workloads.
    minimum: i64,
}

fn locked(tx: &Transaction<'_>, provider: &str) -> Result<i64> {
    Ok(tx
        .query_row("SELECT locked FROM collateral WHERE provider = ?1", [provider], |r| r.get(0))
        .optional()?
        .unwrap_or(0))
}

fn set_locked(tx: &Transaction<'_>, provider: &str, amount: i64) -> Result<()> {
    tx.execute(
        "INSERT INTO collateral(provider, locked, updated_at) VALUES (?1, ?2, ?3)
         ON CONFLICT(provider) DO UPDATE SET locked = excluded.locked, updated_at = excluded.updated_at",
        params![provider, amount, now_secs()],
    )?;
    Ok(())
}

impl Collateral {
    pub fn new(ledger: Ledger, minimum: i64) -> Self {
        Self { ledger, minimum }
    }

    pub fn minimum(&self) -> i64 {
        self.minimum
    }

    /// Move `amount` from the provider's balance into locked collateral.
    pub async fn lock(&self, provider: &str, amount: i64) -> Result<CollateralState> {
        if amount <= 0 {
            return Err(Error::Invalid("amount must be positive".into()));
        }
        let p = provider.to_owned();
        let total = self
            .ledger
            .store()
            .call(move |c| {
                let tx = c.transaction()?;
                ledger::apply(&tx, &p, -amount, "collateral_lock", None)?;
                let total = locked(&tx, &p)? + amount;
                set_locked(&tx, &p, total)?;
                tx.commit()?;
                Ok(total)
            })
            .await?;
        Ok(self.state_from(provider, total))
    }

    /// Release collateral back to the balance (only while no workloads run;
    /// the scheduler enforces that before calling).
    pub async fn unlock(&self, provider: &str, amount: i64) -> Result<CollateralState> {
        let p = provider.to_owned();
        let total = self
            .ledger
            .store()
            .call(move |c| {
                let tx = c.transaction()?;
                let current = locked(&tx, &p)?;
                if amount <= 0 || amount > current {
                    return Err(Error::Invalid(format!("can unlock at most {current}")));
                }
                set_locked(&tx, &p, current - amount)?;
                ledger::apply(&tx, &p, amount, "collateral_unlock", None)?;
                tx.commit()?;
                Ok(current - amount)
            })
            .await?;
        Ok(self.state_from(provider, total))
    }

    pub async fn state(&self, provider: &str) -> Result<CollateralState> {
        let p = provider.to_owned();
        let total = self.ledger.store().call(move |c| locked(&c.transaction()?, &p)).await?;
        Ok(self.state_from(provider, total))
    }

    fn state_from(&self, provider: &str, locked: i64) -> CollateralState {
        CollateralState {
            provider: provider.to_owned(),
            locked,
            minimum: self.minimum,
            eligible: locked >= self.minimum,
        }
    }

    /// Slash up to `amount` of the provider's collateral to the renter. Returns
    /// what was actually slashed (bounded by what is locked).
    pub async fn slash(&self, provider: &str, renter: &str, amount: i64, reason: &str) -> Result<i64> {
        let (p, r, why) = (provider.to_owned(), renter.to_owned(), reason.to_owned());
        let (slashed, renter_balance) = self
            .ledger
            .store()
            .call(move |c| {
                let tx = c.transaction()?;
                let current = locked(&tx, &p)?;
                let slashed = amount.clamp(0, current);
                if slashed == 0 {
                    return Ok((0, None));
                }
                set_locked(&tx, &p, current - slashed)?;
                let bal = ledger::apply(&tx, &r, slashed, "slash_compensation", Some(&why))?;
                tx.commit()?;
                Ok((slashed, Some(bal)))
            })
            .await?;
        if let Some(balance) = renter_balance {
            let events = self.ledger.events();
            events.publish(NodeEvent::Balance { account: renter.to_owned(), balance });
            events.publish(NodeEvent::Slashed {
                provider: provider.to_owned(),
                renter: renter.to_owned(),
                amount: slashed,
                reason: reason.to_owned(),
            });
        }
        Ok(slashed)
    }

    // ---- pooled staking -------------------------------------------------

    pub async fn create_pool(&self, id: &str, name: &str, min_total: i64) -> Result<()> {
        let (id, name) = (id.to_owned(), name.to_owned());
        self.ledger
            .store()
            .call(move |c| {
                c.execute(
                    "INSERT INTO pools(id, name, min_total, created_at) VALUES (?1, ?2, ?3, ?4)",
                    params![id, name, min_total, now_secs()],
                )?;
                Ok(())
            })
            .await
    }

    pub async fn stake(&self, pool: &str, provider: &str, amount: i64) -> Result<i64> {
        if amount <= 0 {
            return Err(Error::Invalid("amount must be positive".into()));
        }
        let (pool, p) = (pool.to_owned(), provider.to_owned());
        self.ledger
            .store()
            .call(move |c| {
                let tx = c.transaction()?;
                ledger::apply(&tx, &p, -amount, "pool_stake", Some(&pool))?;
                tx.execute(
                    "INSERT INTO pool_members(pool, provider, stake) VALUES (?1, ?2, ?3)
                     ON CONFLICT(pool, provider) DO UPDATE SET stake = stake + excluded.stake",
                    params![pool, p, amount],
                )?;
                let total: i64 =
                    tx.query_row("SELECT COALESCE(SUM(stake), 0) FROM pool_members WHERE pool = ?1", [&pool], |r| {
                        r.get(0)
                    })?;
                tx.commit()?;
                Ok(total)
            })
            .await
    }

    /// (total staked, qualifies for enterprise workloads)
    pub async fn pool_status(&self, pool: &str) -> Result<(i64, bool)> {
        let pool = pool.to_owned();
        self.ledger
            .store()
            .call(move |c| {
                let min: i64 = c
                    .query_row("SELECT min_total FROM pools WHERE id = ?1", [&pool], |r| r.get(0))
                    .optional()?
                    .ok_or_else(|| Error::NotFound(format!("pool {pool}")))?;
                let total: i64 =
                    c.query_row("SELECT COALESCE(SUM(stake), 0) FROM pool_members WHERE pool = ?1", [&pool], |r| {
                        r.get(0)
                    })?;
                Ok((total, total >= min))
            })
            .await
    }

    /// Slash a pool pro rata to each member's stake. Rounding dust is taken
    /// from the largest staker so the renter receives exactly `amount`
    /// (bounded by the pool total).
    pub async fn slash_pool(&self, pool: &str, renter: &str, amount: i64, reason: &str) -> Result<i64> {
        let (pool, r, why) = (pool.to_owned(), renter.to_owned(), reason.to_owned());
        self.ledger
            .store()
            .call(move |c| {
                let tx = c.transaction()?;
                let members: Vec<(String, i64)> = {
                    let mut stmt = tx.prepare(
                        "SELECT provider, stake FROM pool_members WHERE pool = ?1 AND stake > 0 ORDER BY stake DESC",
                    )?;
                    let rows = stmt.query_map([&pool], |row| Ok((row.get(0)?, row.get(1)?)))?;
                    rows.collect::<Result<_, _>>()?
                };
                let total: i64 = members.iter().map(|(_, s)| s).sum();
                let target = amount.clamp(0, total);
                if target == 0 {
                    return Ok(0);
                }
                let mut cuts: Vec<i64> =
                    members.iter().map(|(_, s)| ((*s as i128 * target as i128) / total as i128) as i64).collect();
                let dust = target - cuts.iter().sum::<i64>();
                cuts[0] += dust;
                for ((provider, _), cut) in members.iter().zip(&cuts) {
                    tx.execute(
                        "UPDATE pool_members SET stake = stake - ?3 WHERE pool = ?1 AND provider = ?2",
                        params![pool, provider, cut],
                    )?;
                }
                ledger::apply(&tx, &r, target, "slash_compensation", Some(&why))?;
                tx.commit()?;
                Ok(target)
            })
            .await
    }
}

#[cfg(test)]
mod tests {
    use super::super::ledger::{AccountKind, MICROS_PER_CREDIT as C};
    use super::*;
    use crate::events::EventBus;
    use crate::storage::Store;

    async fn setup() -> (Ledger, Collateral) {
        let l = Ledger::new(Store::in_memory().expect("db"), EventBus::default()).expect("ledger");
        for (id, kind) in [("h1", AccountKind::Provider), ("h2", AccountKind::Provider), ("r", AccountKind::Renter)] {
            l.open_account(id, kind).await.expect("account");
        }
        l.top_up("h1", 100 * C, "a").await.expect("top up");
        l.top_up("h2", 100 * C, "b").await.expect("top up");
        (l.clone(), Collateral::new(l, 50 * C))
    }

    #[tokio::test]
    async fn lock_gates_eligibility_and_slash_pays_renter() {
        let (l, col) = setup().await;
        assert!(!col.lock("h1", 40 * C).await.expect("lock").eligible);
        assert!(col.lock("h1", 10 * C).await.expect("lock").eligible);
        assert_eq!(l.balance("h1").await.expect("bal"), 50 * C);

        assert_eq!(col.slash("h1", "r", 80 * C, "vanished").await.expect("slash"), 50 * C);
        assert_eq!(l.balance("r").await.expect("bal"), 50 * C);
        assert!(!col.state("h1").await.expect("state").eligible);
    }

    #[tokio::test]
    async fn pool_slash_is_pro_rata() {
        let (l, col) = setup().await;
        col.create_pool("h100", "H100 pool", 90 * C).await.expect("pool");
        col.stake("h100", "h1", 60 * C).await.expect("stake");
        assert_eq!(col.pool_status("h100").await.expect("status"), (60 * C, false));
        col.stake("h100", "h2", 30 * C).await.expect("stake");
        assert_eq!(col.pool_status("h100").await.expect("status"), (90 * C, true));

        assert_eq!(col.slash_pool("h100", "r", 9 * C, "sla").await.expect("slash"), 9 * C);
        assert_eq!(l.balance("r").await.expect("bal"), 9 * C);
        assert_eq!(col.pool_status("h100").await.expect("status").0, 81 * C);
    }
}
