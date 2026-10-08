//! Overlay routing: which peer (and UDP endpoint) owns each virtual IP.

use std::collections::HashMap;
use std::net::{Ipv4Addr, SocketAddr};
use std::sync::{Arc, RwLock};

use serde::Serialize;

use crate::events::{EventBus, NodeEvent};
use crate::{Error, Result};

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Route {
    pub peer_id: String,
    pub endpoint: SocketAddr,
    /// Warm standby that already holds a replica of the guest.
    pub backup: Option<(String, SocketAddr)>,
}

/// Lock-light routing table shared by the TUN pump and the failover controller.
#[derive(Debug, Clone)]
pub struct RoutingTable {
    routes: Arc<RwLock<HashMap<Ipv4Addr, Route>>>,
    events: EventBus,
}

impl RoutingTable {
    pub fn new(events: EventBus) -> Self {
        Self { routes: Arc::new(RwLock::new(HashMap::new())), events }
    }

    fn read(&self) -> std::sync::RwLockReadGuard<'_, HashMap<Ipv4Addr, Route>> {
        self.routes.read().unwrap_or_else(|p| p.into_inner())
    }

    fn write(&self) -> std::sync::RwLockWriteGuard<'_, HashMap<Ipv4Addr, Route>> {
        self.routes.write().unwrap_or_else(|p| p.into_inner())
    }

    pub fn insert(&self, vip: Ipv4Addr, route: Route) {
        self.events.publish(NodeEvent::RouteChanged {
            virtual_ip: vip.to_string(),
            peer: route.peer_id.clone(),
            endpoint: route.endpoint.to_string(),
        });
        self.write().insert(vip, route);
    }

    pub fn lookup(&self, vip: Ipv4Addr) -> Option<Route> {
        self.read().get(&vip).cloned()
    }

    pub fn snapshot(&self) -> Vec<(Ipv4Addr, Route)> {
        self.read().iter().map(|(k, v)| (*k, v.clone())).collect()
    }

    /// Promote the backup for every VIP served by `failed_peer`. Returns the VIPs moved.
    pub fn fail_over_peer(&self, failed_peer: &str) -> Vec<Ipv4Addr> {
        let mut moved = Vec::new();
        {
            let mut routes = self.write();
            for (vip, route) in routes.iter_mut() {
                if route.peer_id == failed_peer {
                    if let Some((peer, endpoint)) = route.backup.take() {
                        route.peer_id = peer;
                        route.endpoint = endpoint;
                        moved.push(*vip);
                    }
                }
            }
        }
        for vip in &moved {
            if let Some(r) = self.lookup(*vip) {
                self.events.publish(NodeEvent::RouteChanged {
                    virtual_ip: vip.to_string(),
                    peer: r.peer_id,
                    endpoint: r.endpoint.to_string(),
                });
            }
        }
        moved
    }

    /// Route for an outbound IP packet read from the TUN device.
    pub fn route_packet(&self, packet: &[u8]) -> Result<Route> {
        let dst = ipv4_destination(packet)?;
        self.lookup(dst).ok_or_else(|| Error::NotFound(format!("no overlay route to {dst}")))
    }
}

/// Destination address of a raw IPv4 packet.
pub fn ipv4_destination(packet: &[u8]) -> Result<Ipv4Addr> {
    if packet.len() < 20 || packet[0] >> 4 != 4 {
        return Err(Error::Invalid("not an IPv4 packet".into()));
    }
    Ok(Ipv4Addr::new(packet[16], packet[17], packet[18], packet[19]))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn failover_swaps_to_backup() {
        let t = RoutingTable::new(EventBus::default());
        let vip = Ipv4Addr::new(10, 147, 0, 5);
        t.insert(
            vip,
            Route {
                peer_id: "host-b".into(),
                endpoint: "198.51.100.2:51820".parse().expect("addr"),
                backup: Some(("host-c".into(), "198.51.100.3:51820".parse().expect("addr"))),
            },
        );
        let mut pkt = [0u8; 20];
        pkt[0] = 0x45;
        pkt[16..20].copy_from_slice(&vip.octets());
        assert_eq!(t.route_packet(&pkt).expect("route").peer_id, "host-b");
        assert_eq!(t.fail_over_peer("host-b"), vec![vip]);
        assert_eq!(t.route_packet(&pkt).expect("route").peer_id, "host-c");
        assert!(t.fail_over_peer("host-b").is_empty());
    }
}
