//! Demo overlay topology driving the real failover controller.
//!
//! Two simulated peers, `host-b` (primary for a guest at 10.147.0.200) and
//! `host-c` (its warm standby), heartbeat every interval. Killing a peer from
//! the UI stops its heartbeats; the actual [`FailoverController`] then walks
//! Suspect → Down → Rerouted and the transitions reach the UI through the
//! normal event pump.
//!
//! [`FailoverController`]: peervps_core::failover::FailoverController

use std::collections::HashMap;
use std::net::Ipv4Addr;
use std::time::Instant;

use peervps_core::Node;
use peervps_core::network::Route;
use serde::Serialize;
use tokio::sync::Mutex;

pub const DEMO_VIP: Ipv4Addr = Ipv4Addr::new(10, 147, 0, 200);
pub const PEERS: [(&str, &str, &str); 2] =
    [("host-b", "198.51.100.2:51820", "Frankfurt"), ("host-c", "198.51.100.3:51820", "Amsterdam")];

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TopologyNode {
    pub id: String,
    pub label: String,
    pub role: &'static str,
    pub endpoint: String,
    pub alive: bool,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Topology {
    pub nodes: Vec<TopologyNode>,
    pub virtual_ip: String,
    pub active_peer: Option<String>,
    pub heartbeat_ms: u64,
}

#[derive(Debug)]
pub struct DemoTopology {
    node: Node,
    alive: Mutex<HashMap<String, bool>>,
}

impl DemoTopology {
    pub fn new(node: Node) -> Self {
        let alive = PEERS.iter().map(|(id, _, _)| ((*id).to_owned(), true)).collect();
        Self { node, alive: Mutex::new(alive) }
    }

    fn primary_route() -> Route {
        let (b, c) = (PEERS[0], PEERS[1]);
        Route {
            peer_id: b.0.into(),
            endpoint: b.1.parse().unwrap_or_else(|_| unreachable!("static address")),
            backup: Some((c.0.into(), c.1.parse().unwrap_or_else(|_| unreachable!("static address")))),
        }
    }

    pub async fn run(self: std::sync::Arc<Self>) {
        self.node.routes.insert(DEMO_VIP, Self::primary_route());
        for (id, _, _) in PEERS {
            self.node.failover.watch(id).await;
        }
        let interval = self.node.failover.interval();
        let mut beat = tokio::time::interval(interval);
        let mut seq = 0u64;
        loop {
            beat.tick().await;
            seq += 1;
            let alive = self.alive.lock().await.clone();
            for (peer, up) in alive {
                if up {
                    self.node.failover.heartbeat(&peer, seq).await;
                }
            }
            if let Err(e) = self.node.failover.evaluate(Instant::now()).await {
                tracing::warn!(error = %e, "failover evaluation failed");
            }
        }
    }

    pub async fn set_alive(&self, peer: &str, up: bool) -> bool {
        let mut alive = self.alive.lock().await;
        let Some(slot) = alive.get_mut(peer) else { return false };
        *slot = up;
        if up && alive.values().all(|v| *v) {
            // Everyone healthy again: fail back to the original primary.
            self.node.routes.insert(DEMO_VIP, Self::primary_route());
        }
        true
    }

    pub async fn topology(&self) -> Topology {
        let alive = self.alive.lock().await;
        let mut nodes = vec![TopologyNode {
            id: "client".into(),
            label: "You (client)".into(),
            role: "client",
            endpoint: "nat:symmetric".into(),
            alive: true,
        }];
        for (i, (id, ep, label)) in PEERS.iter().enumerate() {
            nodes.push(TopologyNode {
                id: (*id).into(),
                label: (*label).into(),
                role: if i == 0 { "primary" } else { "standby" },
                endpoint: (*ep).into(),
                alive: alive.get(*id).copied().unwrap_or(false),
            });
        }
        Topology {
            nodes,
            virtual_ip: DEMO_VIP.to_string(),
            active_peer: self.node.routes.lookup(DEMO_VIP).map(|r| r.peer_id),
            heartbeat_ms: self.node.failover.interval().as_millis() as u64,
        }
    }
}
