//! Payment gateway hooks: checkout creation and signed top-up webhooks.
//!
//! Signature scheme (Stripe-compatible shape): the gateway sends a header
//! `t=<unix seconds>,v1=<hex HMAC-SHA256(secret, "<t>.<raw body>")>`. We check
//! the timestamp is within the tolerance (replay window), compare the MAC in
//! constant time, then record the event id so a redelivery is a no-op.

use async_trait::async_trait;
use hmac::{Hmac, Mac};
use rusqlite::params;
use serde::{Deserialize, Serialize};
use sha2::Sha256;

use super::ledger::{self, GATEWAY, Ledger};
use crate::events::NodeEvent;
use crate::storage::now_secs;
use crate::{Error, Result};

type HmacSha256 = Hmac<Sha256>;

#[derive(Clone)]
pub struct WebhookVerifier {
    secret: Vec<u8>,
    tolerance_secs: i64,
}

impl std::fmt::Debug for WebhookVerifier {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WebhookVerifier").field("tolerance_secs", &self.tolerance_secs).finish_non_exhaustive()
    }
}

impl WebhookVerifier {
    pub fn new(secret: impl Into<Vec<u8>>, tolerance_secs: i64) -> Self {
        Self { secret: secret.into(), tolerance_secs }
    }

    /// Produce a header for `body` (used by tests and the local gateway simulator).
    pub fn sign(&self, body: &[u8], timestamp: i64) -> String {
        let tag = self.mac(timestamp, body).finalize().into_bytes();
        format!("t={timestamp},v1={}", hex::encode(tag))
    }

    pub fn verify(&self, header: &str, body: &[u8], now: i64) -> Result<()> {
        let mut ts = None;
        let mut sigs = Vec::new();
        for part in header.split(',') {
            match part.trim().split_once('=') {
                Some(("t", v)) => ts = v.parse::<i64>().ok(),
                Some(("v1", v)) => sigs.push(v),
                _ => {}
            }
        }
        let ts = ts.ok_or_else(|| Error::Unauthorized("webhook: missing timestamp".into()))?;
        if (now - ts).abs() > self.tolerance_secs {
            return Err(Error::Unauthorized("webhook: timestamp outside tolerance".into()));
        }
        // Multiple v1 entries are allowed during secret rotation.
        for sig in sigs {
            if let Ok(raw) = hex::decode(sig) {
                if self.mac(ts, body).verify_slice(&raw).is_ok() {
                    return Ok(());
                }
            }
        }
        Err(Error::Unauthorized("webhook: bad signature".into()))
    }

    fn mac(&self, ts: i64, body: &[u8]) -> HmacSha256 {
        // HMAC accepts keys of any length, so construction cannot fail.
        let mut mac = <HmacSha256 as Mac>::new_from_slice(&self.secret).unwrap_or_else(|_| unreachable!());
        mac.update(ts.to_string().as_bytes());
        mac.update(b".");
        mac.update(body);
        mac
    }
}

/// Body of a confirmed top-up webhook.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TopUpEvent {
    /// Gateway event id, used for idempotency.
    pub id: String,
    pub account: String,
    /// µcredits to credit.
    pub amount: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum WebhookOutcome {
    Credited { balance: i64 },
    Duplicate,
}

/// Verify, de-duplicate and apply a top-up webhook.
pub async fn handle_top_up(
    ledger: &Ledger,
    verifier: &WebhookVerifier,
    gateway: &str,
    signature_header: &str,
    body: &[u8],
) -> Result<WebhookOutcome> {
    verifier.verify(signature_header, body, now_secs())?;
    let event: TopUpEvent = serde_json::from_slice(body)?;
    if event.amount <= 0 {
        return Err(Error::Invalid("top-up amount must be positive".into()));
    }
    let gateway = gateway.to_owned();
    let account = event.account.clone();
    let outcome = ledger
        .store()
        .call(move |c| {
            let tx = c.transaction()?;
            let inserted = tx.execute(
                "INSERT INTO webhook_events(id, gateway, received_at) VALUES (?1, ?2, ?3) ON CONFLICT(id) DO NOTHING",
                params![event.id, gateway, now_secs()],
            )?;
            if inserted == 0 {
                return Ok(WebhookOutcome::Duplicate);
            }
            let balance =
                ledger::transfer_in(&tx, GATEWAY, &event.account, event.amount, "top_up", Some(&event.id), true)?;
            tx.commit()?;
            Ok(WebhookOutcome::Credited { balance })
        })
        .await?;
    if let WebhookOutcome::Credited { balance } = outcome {
        ledger.events().publish(NodeEvent::Balance { account, balance });
    }
    Ok(outcome)
}

/// Outbound side of a payment provider (card processor, on-chain bridge, …).
#[async_trait]
pub trait PaymentGateway: Send + Sync + std::fmt::Debug {
    fn name(&self) -> &'static str;
    /// Create a hosted checkout for `amount` µcredits; returns the URL to send the user to.
    async fn create_checkout(&self, account: &str, amount: i64) -> Result<String>;
}

/// Development gateway: returns a local URL instead of calling out.
#[derive(Debug, Default, Clone)]
pub struct StubGateway;

#[async_trait]
impl PaymentGateway for StubGateway {
    fn name(&self) -> &'static str {
        "stub"
    }
    async fn create_checkout(&self, account: &str, amount: i64) -> Result<String> {
        Ok(format!("http://localhost/checkout?account={account}&amount={amount}"))
    }
}

/// HTTP gateway client skeleton; endpoint shape is provider-specific.
#[derive(Debug, Clone)]
pub struct HttpGateway {
    client: reqwest::Client,
    base_url: String,
    api_key: String,
}

impl HttpGateway {
    pub fn new(base_url: impl Into<String>, api_key: impl Into<String>) -> Self {
        Self { client: reqwest::Client::new(), base_url: base_url.into(), api_key: api_key.into() }
    }
}

#[async_trait]
impl PaymentGateway for HttpGateway {
    fn name(&self) -> &'static str {
        "http"
    }
    async fn create_checkout(&self, account: &str, amount: i64) -> Result<String> {
        #[derive(Deserialize)]
        struct Resp {
            url: String,
        }
        let resp: Resp = self
            .client
            .post(format!("{}/v1/checkout", self.base_url))
            .bearer_auth(&self.api_key)
            .json(&serde_json::json!({ "account": account, "amount": amount }))
            .send()
            .await
            .and_then(|r| r.error_for_status())
            .map_err(|e| Error::Io(std::io::Error::other(e)))?
            .json()
            .await
            .map_err(|e| Error::Io(std::io::Error::other(e)))?;
        Ok(resp.url)
    }
}

#[cfg(test)]
mod tests {
    use super::super::ledger::AccountKind;
    use super::*;
    use crate::events::EventBus;
    use crate::storage::Store;

    #[test]
    fn signature_checks() {
        let v = WebhookVerifier::new(b"whsec_test".to_vec(), 300);
        let body = br#"{"id":"evt_1","account":"a","amount":5}"#;
        let header = v.sign(body, 1_000);
        v.verify(&header, body, 1_100).expect("valid");
        assert!(v.verify(&header, body, 2_000).is_err(), "stale");
        assert!(v.verify(&header, b"{}", 1_100).is_err(), "body changed");
        assert!(WebhookVerifier::new(b"other".to_vec(), 300).verify(&header, body, 1_100).is_err());
    }

    #[tokio::test]
    async fn top_up_is_idempotent() {
        let ledger = Ledger::new(Store::in_memory().expect("db"), EventBus::default()).expect("ledger");
        ledger.open_account("alice", AccountKind::Renter).await.expect("acct");
        let v = WebhookVerifier::new(b"whsec".to_vec(), 300);
        let body = br#"{"id":"evt_9","account":"alice","amount":2500000}"#;
        let header = v.sign(body, now_secs());
        assert_eq!(
            handle_top_up(&ledger, &v, "stub", &header, body).await.expect("first"),
            WebhookOutcome::Credited { balance: 2_500_000 }
        );
        assert_eq!(handle_top_up(&ledger, &v, "stub", &header, body).await.expect("again"), WebhookOutcome::Duplicate);
        assert_eq!(ledger.balance("alice").await.expect("bal"), 2_500_000);
    }
}
