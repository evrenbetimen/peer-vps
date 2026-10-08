//! Finding other PeerVPS machines on the same network.
//!
//! Every node that accepts peers broadcasts a small beacon on UDP
//! [`BEACON_PORT`] every few seconds: its peer id, its public key and the TCP
//! port it accepts peers on. Beacons are not trusted: adding a machine found
//! this way still dials it and checks that the key it proves in the handshake
//! matches the id it announced.

use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tokio::net::UdpSocket;
use tokio::sync::RwLock;

use crate::Result;
use crate::storage::now_secs;

pub const BEACON_PORT: u16 = 7072;
const EVERY: Duration = Duration::from_secs(5);
/// A machine that stopped announcing itself drops out of the list after this long.
const FORGET_AFTER_SECS: i64 = 20;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct Beacon {
    peervps: u8,
    id: String,
    key: String,
    port: u16,
}

/// A machine announcing itself on the local network.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NearbyPeer {
    pub id: String,
    /// `pv-…@ip:port`, ready to add.
    pub invite: String,
    pub last_seen: i64,
}

#[derive(Debug, Clone, Default)]
pub struct Nearby {
    seen: Arc<RwLock<HashMap<String, NearbyPeer>>>,
}

impl Nearby {
    pub async fn list(&self) -> Vec<NearbyPeer> {
        let cutoff = now_secs() - FORGET_AFTER_SECS;
        let mut seen = self.seen.write().await;
        seen.retain(|_, p| p.last_seen >= cutoff);
        let mut v: Vec<NearbyPeer> = seen.values().cloned().collect();
        v.sort_by(|a, b| a.id.cmp(&b.id));
        v
    }

    /// Record a beacon received from `from`; ignores our own and malformed ones.
    async fn heard(&self, me: &str, from: SocketAddr, bytes: &[u8]) {
        let Ok(b) = serde_json::from_slice::<Beacon>(bytes) else { return };
        let id_matches_key = hex::decode(&b.key).is_ok_and(|k| k.len() == 32 && super::Identity::id_for(&k) == b.id);
        if b.peervps != 1 || b.id == me || !id_matches_key || b.port == 0 {
            return;
        }
        let invite = format!("{}@{}", b.id, SocketAddr::new(from.ip(), b.port));
        self.seen.write().await.insert(b.id.clone(), NearbyPeer { id: b.id, invite, last_seen: now_secs() });
    }
}

/// Announce `id` (accepting peers on `peer_port`) to `targets` and listen for
/// others on `listen`. Runs until the returned task is aborted.
pub async fn run(
    nearby: Nearby,
    id: String,
    key: Vec<u8>,
    peer_port: u16,
    listen: SocketAddr,
    targets: Vec<SocketAddr>,
) -> Result<tokio::task::JoinHandle<()>> {
    let socket = bind_shared(listen)?;
    socket.set_broadcast(true)?;
    let beacon = serde_json::to_vec(&Beacon { peervps: 1, id: id.clone(), key: hex::encode(key), port: peer_port })?;
    Ok(tokio::spawn(async move {
        let mut tick = tokio::time::interval(EVERY);
        let mut buf = vec![0u8; 1024];
        loop {
            tokio::select! {
                _ = tick.tick() => {
                    for t in &targets {
                        // A network without broadcast (or a sleeping laptop) is not an error worth more than a trace.
                        if let Err(e) = socket.send_to(&beacon, t).await {
                            tracing::trace!(target = %t, error = %e, "beacon not sent");
                        }
                    }
                }
                r = socket.recv_from(&mut buf) => {
                    if let Ok((n, from)) = r {
                        nearby.heard(&id, from, &buf[..n]).await;
                    }
                }
            }
        }
    }))
}

/// The default: broadcast on the LAN and listen on [`BEACON_PORT`].
pub fn lan() -> (SocketAddr, Vec<SocketAddr>) {
    ((Ipv4Addr::UNSPECIFIED, BEACON_PORT).into(), vec![SocketAddr::new(IpAddr::V4(Ipv4Addr::BROADCAST), BEACON_PORT)])
}

/// Several PeerVPS processes on one machine (the app and a CLI node) can all hear beacons.
fn bind_shared(addr: SocketAddr) -> Result<UdpSocket> {
    let socket = socket2::Socket::new(socket2::Domain::for_address(addr), socket2::Type::DGRAM, None)?;
    socket.set_reuse_address(true)?;
    #[cfg(unix)]
    socket.set_reuse_port(true)?;
    socket.set_nonblocking(true)?;
    socket.bind(&addr.into())?;
    Ok(UdpSocket::from_std(socket.into())?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::peer::Identity;

    #[tokio::test]
    async fn machines_hear_each_others_beacons_and_ignore_forgeries() {
        let (a, b) = (Identity::generate().expect("a"), Identity::generate().expect("b"));
        let probe = std::net::UdpSocket::bind("127.0.0.1:0").expect("probe");
        let port_b = probe.local_addr().expect("addr").port();
        drop(probe);
        let listen_b: SocketAddr = ([127, 0, 0, 1], port_b).into();
        let (nearby_a, nearby_b) = (Nearby::default(), Nearby::default());
        let ta = run(
            nearby_a,
            a.id.clone(),
            a.keypair.public.clone(),
            7071,
            "127.0.0.1:0".parse().expect("a"),
            vec![listen_b],
        )
        .await
        .expect("a");
        let tb = run(nearby_b.clone(), b.id.clone(), b.keypair.public.clone(), 7171, listen_b, vec![listen_b])
            .await
            .expect("b");

        let mut found = Vec::new();
        for _ in 0..40 {
            found = nearby_b.list().await;
            if !found.is_empty() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        assert_eq!(found.len(), 1, "b hears a but not itself: {found:?}");
        assert_eq!(found[0].invite, format!("{}@127.0.0.1:7071", a.id));

        let forged = serde_json::to_vec(&Beacon {
            peervps: 1,
            id: "pv-0000000000000000".into(),
            key: hex::encode(&a.keypair.public),
            port: 1,
        })
        .expect("json");
        nearby_b.heard(&b.id, "127.0.0.1:9".parse().expect("addr"), &forged).await;
        nearby_b.heard(&b.id, "127.0.0.1:9".parse().expect("addr"), b"not json").await;
        assert_eq!(nearby_b.list().await.len(), 1, "an id that does not match its key is dropped");
        ta.abort();
        tb.abort();
    }
}
