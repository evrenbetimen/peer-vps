//! Credits moving between nodes that rent from each other.
//!
//! A renter prepays its host: it takes credits out of a local account into
//! [`PEERS`] and tells the host the running total it has ever paid. The host
//! credits the difference from the last total it saw to the renter's
//! `peer-<id>` account, which the usage meter then bills as usual. Refunds go
//! the other way with their own running total. Because only totals cross the
//! wire, a message that is lost or repeated is caught up or ignored by the next.
//!
//! The totals are claims between approved peers, carried over the
//! authenticated channel; there is no third party that clears them.

use rusqlite::{Transaction, params};
use serde::{Deserialize, Serialize};

use super::ledger::{self, Ledger};
use crate::events::NodeEvent;
use crate::{Error, Result};

/// System account for credits that are out at (or came in from) peers.
pub const PEERS: &str = "system:peers";

/// What has moved between us and one peer, net of refunds.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PeerFlows {
    /// We paid it to rent its machine.
    pub paid: i64,
    /// It paid us to rent ours.
    pub earned: i64,
}

#[derive(Debug, Clone, Copy, Default)]
struct Totals {
    sent: i64,
    received: i64,
    refunded_out: i64,
    refunded_in: i64,
}

fn totals(tx: &Transaction<'_>, peer: &str) -> Result<Totals> {
    tx.execute("INSERT INTO peer_payments(peer) VALUES (?1) ON CONFLICT(peer) DO NOTHING", [peer])?;
    Ok(tx.query_row(
        "SELECT sent, received, refunded_out, refunded_in FROM peer_payments WHERE peer = ?1",
        [peer],
        |r| Ok(Totals { sent: r.get(0)?, received: r.get(1)?, refunded_out: r.get(2)?, refunded_in: r.get(3)? }),
    )?)
}

/// Journal `delta` against [`PEERS`]. Like the gateway, it mirrors money held
/// outside this ledger, so only the journal records it, not a balance.
fn peers_account(tx: &Transaction<'_>, delta: i64, kind: &str, peer: &str) -> Result<()> {
    tx.execute(
        "INSERT INTO ledger(account, delta, balance_after, kind, reference, at) VALUES (?1, ?2, 0, ?3, ?4, ?5)",
        params![PEERS, delta, kind, peer, crate::storage::now_secs()],
    )?;
    Ok(())
}

impl Ledger {
    /// Renter side: take `amount` out of `account` towards `peer`; returns the new total paid.
    pub async fn pay_peer(&self, account: &str, peer: &str, amount: i64) -> Result<i64> {
        if amount <= 0 {
            return Err(Error::Invalid("amount must be positive".into()));
        }
        let (account, peer) = (account.to_owned(), peer.to_owned());
        let (balance, sent) = self
            .store()
            .call({
                let account = account.clone();
                move |c| {
                    let tx = c.transaction()?;
                    let t = totals(&tx, &peer)?;
                    let balance = ledger::apply(&tx, &account, -amount, "peer_payment", Some(&peer))?;
                    peers_account(&tx, amount, "peer_payment", &peer)?;
                    let sent = t.sent + amount;
                    tx.execute("UPDATE peer_payments SET sent = ?2 WHERE peer = ?1", params![peer, sent])?;
                    tx.commit()?;
                    Ok((balance, sent))
                }
            })
            .await?;
        self.events().publish(NodeEvent::Balance { account, balance });
        Ok(sent)
    }

    /// The total we have paid `peer` so far (what to tell it again after a lost message).
    pub async fn paid_to(&self, peer: &str) -> Result<i64> {
        let peer = peer.to_owned();
        self.store()
            .call(move |c| {
                let tx = c.transaction()?;
                let t = totals(&tx, &peer)?;
                tx.commit()?;
                Ok(t.sent)
            })
            .await
    }

    /// Host side: `peer` says it has paid `total` in all; credit what is new to `account`.
    /// Returns the account's balance.
    pub async fn receive_from_peer(&self, peer: &str, account: &str, total: i64) -> Result<i64> {
        let (peer, account) = (peer.to_owned(), account.to_owned());
        let (balance, credited) = self
            .store()
            .call({
                let account = account.clone();
                move |c| {
                    let tx = c.transaction()?;
                    let t = totals(&tx, &peer)?;
                    let new = total - t.received;
                    let balance = if new > 0 {
                        peers_account(&tx, -new, "peer_payment", &peer)?;
                        tx.execute("UPDATE peer_payments SET received = ?2 WHERE peer = ?1", params![peer, total])?;
                        ledger::apply(&tx, &account, new, "peer_payment", Some(&peer))?
                    } else {
                        ledger::balance(&tx, &account)?
                    };
                    tx.commit()?;
                    Ok((balance, new > 0))
                }
            })
            .await?;
        if credited {
            self.events().publish(NodeEvent::Balance { account, balance });
        }
        Ok(balance)
    }

    /// Host side: hand everything left on `account` back to `peer`; returns the new total refunded.
    pub async fn refund_peer(&self, peer: &str, account: &str) -> Result<i64> {
        let (peer, account) = (peer.to_owned(), account.to_owned());
        self.store()
            .call(move |c| {
                let tx = c.transaction()?;
                let t = totals(&tx, &peer)?;
                let left = ledger::balance(&tx, &account)?;
                let total = t.refunded_out + left;
                if left > 0 {
                    ledger::apply(&tx, &account, -left, "peer_refund", Some(&peer))?;
                    peers_account(&tx, left, "peer_refund", &peer)?;
                    tx.execute("UPDATE peer_payments SET refunded_out = ?2 WHERE peer = ?1", params![peer, total])?;
                }
                tx.commit()?;
                Ok(total)
            })
            .await
    }

    /// Renter side: `peer` says it has refunded `total` in all; credit what is new to `account`.
    pub async fn refunded_by_peer(&self, peer: &str, account: &str, total: i64) -> Result<i64> {
        let (peer, account) = (peer.to_owned(), account.to_owned());
        let (new, balance) = self
            .store()
            .call({
                let account = account.clone();
                move |c| {
                    let tx = c.transaction()?;
                    let t = totals(&tx, &peer)?;
                    // Never take back more than we paid.
                    let new = (total.min(t.sent) - t.refunded_in).max(0);
                    let balance = if new > 0 {
                        peers_account(&tx, -new, "peer_refund", &peer)?;
                        tx.execute(
                            "UPDATE peer_payments SET refunded_in = ?2 WHERE peer = ?1",
                            params![peer, t.refunded_in + new],
                        )?;
                        ledger::apply(&tx, &account, new, "peer_refund", Some(&peer))?
                    } else {
                        ledger::balance(&tx, &account)?
                    };
                    tx.commit()?;
                    Ok((new, balance))
                }
            })
            .await?;
        if new > 0 {
            self.events().publish(NodeEvent::Balance { account, balance });
        }
        Ok(new)
    }

    /// Net credits between us and every peer we have settled with.
    pub async fn peer_flows(&self) -> Result<Vec<(String, PeerFlows)>> {
        self.store()
            .call(|c| {
                let mut stmt =
                    c.prepare("SELECT peer, sent - refunded_in, received - refunded_out FROM peer_payments")?;
                let rows = stmt.query_map([], |r| Ok((r.get(0)?, PeerFlows { paid: r.get(1)?, earned: r.get(2)? })))?;
                Ok(rows.collect::<Result<Vec<_>, _>>()?)
            })
            .await
    }
}

#[cfg(test)]
mod tests {
    use super::super::ledger::{AccountKind, MICROS_PER_CREDIT};
    use super::*;
    use crate::events::EventBus;
    use crate::storage::Store;

    fn ledger() -> Ledger {
        Ledger::new(Store::in_memory().expect("db"), EventBus::default()).expect("ledger")
    }

    #[tokio::test]
    async fn totals_make_payments_and_refunds_idempotent() {
        let (renter, host) = (ledger(), ledger());
        renter.open_account("me", AccountKind::Renter).await.expect("me");
        renter.top_up("me", 10 * MICROS_PER_CREDIT, "pi").await.expect("top up");
        host.open_account("peer-me", AccountKind::Renter).await.expect("peer account");

        let sent = renter.pay_peer("me", "pv-host", 3 * MICROS_PER_CREDIT).await.expect("pay");
        assert_eq!(renter.balance("me").await.expect("b"), 7 * MICROS_PER_CREDIT);
        assert!(matches!(
            renter.pay_peer("me", "pv-host", 8 * MICROS_PER_CREDIT).await,
            Err(Error::InsufficientFunds { .. })
        ));
        assert_eq!(renter.paid_to("pv-host").await.expect("paid"), sent);

        // The same total twice credits once; an older total credits nothing.
        for total in [sent, sent, MICROS_PER_CREDIT] {
            let b = host.receive_from_peer("pv-me", "peer-me", total).await.expect("receive");
            assert_eq!(b, 3 * MICROS_PER_CREDIT);
        }
        host.transfer("peer-me", ledger::TREASURY, MICROS_PER_CREDIT, "usage", None).await.expect("spend");

        let refunded = host.refund_peer("pv-me", "peer-me").await.expect("refund");
        assert_eq!(refunded, 2 * MICROS_PER_CREDIT);
        assert_eq!(host.balance("peer-me").await.expect("empty"), 0);
        assert_eq!(host.refund_peer("pv-me", "peer-me").await.expect("again"), refunded, "nothing new to return");
        for _ in 0..2 {
            renter.refunded_by_peer("pv-host", "me", refunded).await.expect("refunded");
        }
        assert_eq!(renter.balance("me").await.expect("b"), 9 * MICROS_PER_CREDIT);
        // A host cannot refund more than it was paid.
        assert_eq!(
            renter.refunded_by_peer("pv-host", "me", 50 * MICROS_PER_CREDIT).await.expect("capped"),
            MICROS_PER_CREDIT
        );

        let flows = renter.peer_flows().await.expect("flows");
        assert_eq!(flows, vec![("pv-host".to_owned(), PeerFlows { paid: 0, earned: 0 })]);
        let flows = host.peer_flows().await.expect("flows");
        assert_eq!(flows, vec![("pv-me".to_owned(), PeerFlows { paid: 0, earned: MICROS_PER_CREDIT })]);
    }
}
