//! In-process event bus.
//!
//! Every subsystem publishes [`NodeEvent`]s here; consumers (the Tauri bridge,
//! the REST API's future streaming endpoints, loggers) subscribe independently.
//! A lagging subscriber only loses its own oldest events, it never blocks a
//! publisher.

use serde::Serialize;
use tokio::sync::broadcast;

use crate::virtualization::VmId;

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "camelCase", rename_all_fields = "camelCase")]
pub enum NodeEvent {
    /// Periodic host telemetry sample.
    Metrics(HostMetrics),
    /// An account balance changed (µcredits).
    Balance { account: String, balance: i64 },
    /// A VM changed lifecycle state.
    VmState { vm: VmId, state: crate::virtualization::VmState },
    /// The overlay routing table now sends `virtual_ip` to `endpoint`.
    RouteChanged { virtual_ip: String, peer: String, endpoint: String },
    /// Failover state transition for a peer.
    Failover(crate::failover::FailoverTransition),
    /// Collateral was slashed to compensate a renter.
    Slashed { provider: String, renter: String, amount: i64, reason: String },
}

#[derive(Debug, Clone, Default, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HostMetrics {
    pub cpu_load_pct: f32,
    pub cpu_temp_c: f32,
    pub mem_used_mib: u64,
    pub mem_total_mib: u64,
    pub running_vms: u32,
    pub sla_pct: f64,
    pub net_rx_bps: u64,
    pub net_tx_bps: u64,
}

/// Cloneable handle to the node's broadcast channel.
#[derive(Debug, Clone)]
pub struct EventBus {
    tx: broadcast::Sender<NodeEvent>,
}

impl EventBus {
    pub fn new(capacity: usize) -> Self {
        let (tx, _) = broadcast::channel(capacity);
        Self { tx }
    }

    /// Publish an event. Having no subscribers is not an error.
    pub fn publish(&self, event: NodeEvent) {
        let _ = self.tx.send(event);
    }

    pub fn subscribe(&self) -> broadcast::Receiver<NodeEvent> {
        self.tx.subscribe()
    }
}

impl Default for EventBus {
    fn default() -> Self {
        Self::new(1024)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn events_serialize_camel_case_for_the_ui() {
        let ev =
            NodeEvent::RouteChanged { virtual_ip: "10.147.0.2".into(), peer: "b".into(), endpoint: "1.2.3.4:5".into() };
        let v = serde_json::to_value(ev).expect("json");
        assert_eq!(v["type"], "routeChanged");
        assert_eq!(v["virtualIp"], "10.147.0.2");
    }
}
