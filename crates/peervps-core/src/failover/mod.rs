//! High-availability: three complementary failover strategies.
//!
//! * **A — hibernation** ([`hibernation`]): planned shutdowns freeze, compress,
//!   encrypt and hand guests to the closest DHT peer ([`dht`]).
//! * **B — live replication** ([`replication`]): a warm standby receives
//!   block-level updates; when the primary misses 3 heartbeats
//!   ([`heartbeat`]) the overlay route flips to the standby.
//! * **C — economic penalties** ([`sla`]): a host that vanishes without
//!   hibernating has its collateral slashed to compensate renters.
//!
//! [`FailoverController`] ties B and C together and narrates every transition
//! on the event bus so the UI can animate it.

pub mod dht;
pub mod heartbeat;
pub mod hibernation;
pub mod replication;
pub mod sla;

use std::sync::Arc;
use std::time::{Duration, Instant};

use serde::Serialize;
use tokio::net::UdpSocket;
use tokio::sync::Mutex;

use crate::Result;
use crate::events::{EventBus, NodeEvent};
use crate::network::RoutingTable;
use heartbeat::{HeartbeatConfig, HeartbeatTracker, PeerHealth};
use sla::SlaEnforcer;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum FailoverPhase {
    Healthy,
    Suspect,
    Down,
    /// Traffic now flows to the standby.
    Rerouted,
    /// No standby: the guest is lost until restored from a snapshot.
    Stranded,
    Recovered,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FailoverTransition {
    pub peer: String,
    pub phase: FailoverPhase,
    pub missed_heartbeats: u32,
    /// Virtual IPs whose route moved.
    pub moved_vips: Vec<String>,
    pub at_ms: i64,
}

#[derive(Debug, Clone)]
pub struct FailoverController {
    tracker: Arc<Mutex<HeartbeatTracker>>,
    routes: RoutingTable,
    sla: Option<SlaEnforcer>,
    events: EventBus,
    cfg: HeartbeatConfig,
}

impl FailoverController {
    pub fn new(cfg: HeartbeatConfig, routes: RoutingTable, sla: Option<SlaEnforcer>, events: EventBus) -> Self {
        Self { tracker: Arc::new(Mutex::new(HeartbeatTracker::new(cfg))), routes, sla, events, cfg }
    }

    pub async fn watch(&self, peer: &str) {
        self.tracker.lock().await.watch(peer, Instant::now());
    }

    pub async fn heartbeat(&self, peer: &str, seq: u64) {
        let recovered = self.tracker.lock().await.observe(peer, seq, Instant::now());
        if recovered.is_some() {
            self.emit(peer, FailoverPhase::Recovered, 0, Vec::new());
        }
    }

    /// Evaluate heartbeat state at `now` and act on every change.
    pub async fn evaluate(&self, now: Instant) -> Result<Vec<FailoverTransition>> {
        let changes = self.tracker.lock().await.evaluate(now);
        let mut out = Vec::new();
        for (peer, health) in changes {
            match health {
                PeerHealth::Healthy => out.push(self.emit(&peer, FailoverPhase::Healthy, 0, vec![])),
                PeerHealth::Suspect(m) => out.push(self.emit(&peer, FailoverPhase::Suspect, m, vec![])),
                PeerHealth::Down => {
                    out.push(self.emit(&peer, FailoverPhase::Down, self.cfg.down_after, vec![]));
                    let moved: Vec<String> =
                        self.routes.fail_over_peer(&peer).iter().map(ToString::to_string).collect();
                    let phase = if moved.is_empty() { FailoverPhase::Stranded } else { FailoverPhase::Rerouted };
                    out.push(self.emit(&peer, phase, self.cfg.down_after, moved));
                    if let Some(sla) = &self.sla {
                        let verdict = sla.on_node_down(&peer).await?;
                        tracing::warn!(?verdict, "sla verdict");
                    }
                }
            }
        }
        Ok(out)
    }

    fn emit(&self, peer: &str, phase: FailoverPhase, missed: u32, moved_vips: Vec<String>) -> FailoverTransition {
        let t = FailoverTransition {
            peer: peer.to_owned(),
            phase,
            missed_heartbeats: missed,
            moved_vips,
            at_ms: chrono::Utc::now().timestamp_millis(),
        };
        self.events.publish(NodeEvent::Failover(t.clone()));
        t
    }

    /// Receive authenticated heartbeats on `socket` and evaluate on every interval.
    pub async fn run(&self, socket: &UdpSocket, key: [u8; 32]) -> Result<()> {
        let mut buf = [0u8; 512];
        let mut tick = tokio::time::interval(self.cfg.interval / 2);
        loop {
            tokio::select! {
                _ = tick.tick() => { self.evaluate(Instant::now()).await?; }
                r = socket.recv_from(&mut buf) => {
                    let (n, from) = r?;
                    match heartbeat::decode(&key, &buf[..n]) {
                        Ok((peer, seq)) => self.heartbeat(&peer, seq).await,
                        Err(e) => tracing::debug!(%from, error = %e, "ignoring datagram"),
                    }
                }
            }
        }
    }

    pub fn interval(&self) -> Duration {
        self.cfg.interval
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::network::Route;

    #[tokio::test]
    async fn missed_heartbeats_reroute_to_standby() {
        let events = EventBus::default();
        let mut rx = events.subscribe();
        let routes = RoutingTable::new(events.clone());
        routes.insert(
            "10.147.0.9".parse().expect("vip"),
            Route {
                peer_id: "host-b".into(),
                endpoint: "198.51.100.2:51820".parse().expect("addr"),
                backup: Some(("host-c".into(), "198.51.100.3:51820".parse().expect("addr"))),
            },
        );
        let cfg = HeartbeatConfig { interval: Duration::from_millis(100), down_after: 3 };
        let ctl = FailoverController::new(cfg, routes.clone(), None, events);
        ctl.watch("host-b").await;
        let t0 = Instant::now();
        let mut phases = Vec::new();
        for ms in [150, 250, 350] {
            phases
                .extend(ctl.evaluate(t0 + Duration::from_millis(ms)).await.expect("eval").into_iter().map(|t| t.phase));
        }
        assert_eq!(
            phases,
            vec![FailoverPhase::Suspect, FailoverPhase::Suspect, FailoverPhase::Down, FailoverPhase::Rerouted]
        );
        assert_eq!(routes.lookup("10.147.0.9".parse().expect("vip")).expect("route").peer_id, "host-c");
        // The UI sees the route change on the bus.
        let mut saw_reroute = false;
        while let Ok(ev) = rx.try_recv() {
            if let NodeEvent::RouteChanged { peer, .. } = ev {
                saw_reroute |= peer == "host-c";
            }
        }
        assert!(saw_reroute);
    }
}
