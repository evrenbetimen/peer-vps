//! Event pump: node event bus → webview, at most once per frame.
//!
//! High-rate state (metrics, balances) is coalesced to its latest value;
//! discrete facts (route changes, failover transitions, VM state changes,
//! slashes) are queued in order and never coalesced. A frame with nothing new
//! emits nothing, so an idle node costs the webview zero work.

use std::collections::BTreeMap;
use std::time::Duration;

use peervps_core::NodeEvent;
use peervps_core::events::HostMetrics;
use serde::Serialize;
use tauri::{AppHandle, Emitter};
use tokio::sync::broadcast::{self, error::RecvError};

pub const EVENT: &str = "node://batch";
const FRAME: Duration = Duration::from_micros(16_667); // 60 Hz
const MAX_DISCRETE: usize = 512;

#[derive(Debug, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Batch {
    metrics: Option<HostMetrics>,
    balances: BTreeMap<String, i64>,
    events: Vec<NodeEvent>,
    /// Events lost because the pump fell behind the bus (should stay 0).
    dropped: u64,
}

impl Batch {
    fn is_empty(&self) -> bool {
        self.metrics.is_none() && self.balances.is_empty() && self.events.is_empty() && self.dropped == 0
    }

    fn push(&mut self, ev: NodeEvent) {
        match ev {
            NodeEvent::Metrics(m) => self.metrics = Some(m),
            NodeEvent::Balance { account, balance } => {
                self.balances.insert(account, balance);
            }
            other => {
                if self.events.len() < MAX_DISCRETE {
                    self.events.push(other);
                } else {
                    self.dropped += 1;
                }
            }
        }
    }
}

pub async fn run(app: AppHandle, mut rx: broadcast::Receiver<NodeEvent>) {
    let mut frame = tokio::time::interval(FRAME);
    frame.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut batch = Batch::default();
    loop {
        tokio::select! {
            r = rx.recv() => match r {
                Ok(ev) => batch.push(ev),
                Err(RecvError::Lagged(n)) => batch.dropped += n,
                Err(RecvError::Closed) => return,
            },
            _ = frame.tick() => {
                if !batch.is_empty() {
                    let out = std::mem::take(&mut batch);
                    if let Err(e) = app.emit(EVENT, &out) {
                        tracing::warn!(error = %e, "failed to emit node batch");
                    }
                }
            }
        }
    }
}
