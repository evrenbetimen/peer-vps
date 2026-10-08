//! Double-entry style credit ledger. All amounts are µcredits (`i64`), never floats.

use rusqlite::{Connection, OptionalExtension, Transaction, params};
use serde::{Deserialize, Serialize};

use crate::events::{EventBus, NodeEvent};
use crate::storage::{Store, now_secs};
use crate::{Error, Result};

pub const MICROS_PER_CREDIT: i64 = 1_000_000;
/// System account that receives platform fees.
pub const TREASURY: &str = "system:treasury";
/// System account representing money entering from payment gateways.
pub const GATEWAY: &str = "system:gateway";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AccountKind {
    Renter,
    Provider,
    System,
}

impl AccountKind {
    fn as_str(self) -> &'static str {
        match self {
            Self::Renter => "renter",
            Self::Provider => "provider",
            Self::System => "system",
        }
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LedgerEntry {
    pub id: i64,
    pub account: String,
    pub delta: i64,
    pub balance_after: i64,
    pub kind: String,
    pub reference: Option<String>,
    pub at: i64,
}

#[derive(Debug, Clone)]
pub struct Ledger {
    store: Store,
    events: EventBus,
}

impl Ledger {
    pub fn new(store: Store, events: EventBus) -> Result<Self> {
        let ledger = Self { store, events };
        ledger.store.with(|c| {
            for id in [TREASURY, GATEWAY] {
                ensure_account(c, id, AccountKind::System)?;
            }
            Ok(())
        })?;
        Ok(ledger)
    }

    pub fn store(&self) -> &Store {
        &self.store
    }

    pub fn events(&self) -> &EventBus {
        &self.events
    }

    pub async fn open_account(&self, id: &str, kind: AccountKind) -> Result<()> {
        let id = id.to_owned();
        self.store.call(move |c| ensure_account(c, &id, kind)).await
    }

    pub async fn balance(&self, id: &str) -> Result<i64> {
        let id = id.to_owned();
        self.store.call(move |c| balance(c, &id)).await
    }

    /// Credit `account` from the gateway (a confirmed top-up).
    pub async fn top_up(&self, account: &str, amount: i64, reference: &str) -> Result<i64> {
        self.move_funds(GATEWAY, account, amount, "top_up", Some(reference.to_owned()), true).await
    }

    /// Move funds between two funded accounts atomically.
    pub async fn transfer(
        &self,
        from: &str,
        to: &str,
        amount: i64,
        kind: &str,
        reference: Option<String>,
    ) -> Result<i64> {
        self.move_funds(from, to, amount, kind, reference, false).await
    }

    async fn move_funds(
        &self,
        from: &str,
        to: &str,
        amount: i64,
        kind: &str,
        reference: Option<String>,
        mint: bool,
    ) -> Result<i64> {
        let (from, to, kind) = (from.to_owned(), to.to_owned(), kind.to_owned());
        let to_for_event = to.clone();
        let new_to = self
            .store
            .call(move |c| {
                let tx = c.transaction()?;
                let to_balance = transfer_in(&tx, &from, &to, amount, &kind, reference.as_deref(), mint)?;
                tx.commit()?;
                Ok(to_balance)
            })
            .await?;
        self.events.publish(NodeEvent::Balance { account: to_for_event, balance: new_to });
        Ok(new_to)
    }

    pub async fn history(&self, account: &str, limit: u32) -> Result<Vec<LedgerEntry>> {
        let account = account.to_owned();
        self.store
            .call(move |c| {
                let mut stmt = c.prepare(
                    "SELECT id, account, delta, balance_after, kind, reference, at FROM ledger
                     WHERE account = ?1 ORDER BY id DESC LIMIT ?2",
                )?;
                let rows = stmt.query_map(params![account, limit], |r| {
                    Ok(LedgerEntry {
                        id: r.get(0)?,
                        account: r.get(1)?,
                        delta: r.get(2)?,
                        balance_after: r.get(3)?,
                        kind: r.get(4)?,
                        reference: r.get(5)?,
                        at: r.get(6)?,
                    })
                })?;
                Ok(rows.collect::<Result<Vec<_>, _>>()?)
            })
            .await
    }
}

pub(crate) fn ensure_account(c: &Connection, id: &str, kind: AccountKind) -> Result<()> {
    c.execute(
        "INSERT INTO accounts(id, kind, balance, created_at) VALUES (?1, ?2, 0, ?3) ON CONFLICT(id) DO NOTHING",
        params![id, kind.as_str(), now_secs()],
    )?;
    Ok(())
}

pub(crate) fn balance(c: &Connection, id: &str) -> Result<i64> {
    c.query_row("SELECT balance FROM accounts WHERE id = ?1", [id], |r| r.get(0))
        .optional()?
        .ok_or_else(|| Error::NotFound(format!("account {id}")))
}

/// Apply `delta` to one account and journal it. Fails if the balance would go negative.
pub(crate) fn apply(
    tx: &Transaction<'_>,
    account: &str,
    delta: i64,
    kind: &str,
    reference: Option<&str>,
) -> Result<i64> {
    let current = balance(tx, account)?;
    let next = current.checked_add(delta).ok_or_else(|| Error::Invalid("balance overflow".into()))?;
    if next < 0 {
        return Err(Error::InsufficientFunds { needed: -delta, available: current });
    }
    tx.execute("UPDATE accounts SET balance = ?2 WHERE id = ?1", params![account, next])?;
    tx.execute(
        "INSERT INTO ledger(account, delta, balance_after, kind, reference, at) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        params![account, delta, next, kind, reference, now_secs()],
    )?;
    Ok(next)
}

/// Transfer inside an open transaction; returns the destination's new balance.
pub(crate) fn transfer_in(
    tx: &Transaction<'_>,
    from: &str,
    to: &str,
    amount: i64,
    kind: &str,
    reference: Option<&str>,
    mint: bool,
) -> Result<i64> {
    if amount <= 0 {
        return Err(Error::Invalid("amount must be positive".into()));
    }
    if from == to {
        return Err(Error::Invalid("cannot transfer to the same account".into()));
    }
    if mint {
        // The gateway account mirrors external money; journal it without a balance floor.
        tx.execute(
            "INSERT INTO ledger(account, delta, balance_after, kind, reference, at) VALUES (?1, ?2, 0, ?3, ?4, ?5)",
            params![from, -amount, kind, reference, now_secs()],
        )?;
    } else {
        apply(tx, from, -amount, kind, reference)?;
    }
    apply(tx, to, amount, kind, reference)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn transfers_are_atomic_and_floored_at_zero() {
        let l = Ledger::new(Store::in_memory().expect("db"), EventBus::default()).expect("ledger");
        l.open_account("alice", AccountKind::Renter).await.expect("alice");
        l.open_account("bob", AccountKind::Provider).await.expect("bob");
        l.top_up("alice", 5 * MICROS_PER_CREDIT, "pi_1").await.expect("top up");

        l.transfer("alice", "bob", 2 * MICROS_PER_CREDIT, "test", None).await.expect("ok");
        let err = l.transfer("alice", "bob", 4 * MICROS_PER_CREDIT, "test", None).await;
        assert!(matches!(err, Err(Error::InsufficientFunds { .. })));
        assert_eq!(l.balance("alice").await.expect("a"), 3 * MICROS_PER_CREDIT);
        assert_eq!(l.balance("bob").await.expect("b"), 2 * MICROS_PER_CREDIT);
        assert_eq!(l.history("alice", 10).await.expect("h").len(), 2);
    }
}
