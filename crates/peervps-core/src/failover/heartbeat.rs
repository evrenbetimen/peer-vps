//! Authenticated UDP heartbeats and miss counting.
//!
//! Every node sends `interval`-spaced heartbeats to the peers it has a stake
//! in (hosts → replicas, replicas → hosts). A peer is `Suspect` after one
//! missed beat and `Down` after [`HeartbeatConfig::down_after`] (3 by default),
//! which is what triggers the live-replication failover.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use tokio::net::UdpSocket;

use crate::{Error, Result};

const MAGIC: &[u8; 4] = b"PVHB";

#[derive(Debug, Clone, Copy)]
pub struct HeartbeatConfig {
    pub interval: Duration,
    pub down_after: u32,
}

impl Default for HeartbeatConfig {
    fn default() -> Self {
        Self { interval: Duration::from_millis(500), down_after: 3 }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", tag = "status", content = "missed")]
pub enum PeerHealth {
    Healthy,
    Suspect(u32),
    Down,
}

#[derive(Debug)]
struct PeerState {
    last_seen: Instant,
    last_seq: u64,
    health: PeerHealth,
}

/// Pure miss-counting state machine (no I/O), driven by [`Self::observe`] and [`Self::evaluate`].
#[derive(Debug)]
pub struct HeartbeatTracker {
    cfg: HeartbeatConfig,
    peers: HashMap<String, PeerState>,
}

impl HeartbeatTracker {
    pub fn new(cfg: HeartbeatConfig) -> Self {
        Self { cfg, peers: HashMap::new() }
    }

    pub fn watch(&mut self, peer: &str, now: Instant) {
        self.peers.entry(peer.to_owned()).or_insert(PeerState {
            last_seen: now,
            last_seq: 0,
            health: PeerHealth::Healthy,
        });
    }

    pub fn unwatch(&mut self, peer: &str) {
        self.peers.remove(peer);
    }

    /// Record a verified heartbeat. Old/duplicate sequence numbers are ignored.
    /// Returns `Some(Healthy)` if this beat recovered a suspect/down peer.
    pub fn observe(&mut self, peer: &str, seq: u64, now: Instant) -> Option<PeerHealth> {
        let st = self.peers.get_mut(peer)?;
        if seq <= st.last_seq && st.last_seq != 0 {
            return None;
        }
        st.last_seq = seq;
        st.last_seen = now;
        if st.health != PeerHealth::Healthy {
            st.health = PeerHealth::Healthy;
            return Some(PeerHealth::Healthy);
        }
        None
    }

    /// Re-evaluate every peer; returns peers whose health changed.
    pub fn evaluate(&mut self, now: Instant) -> Vec<(String, PeerHealth)> {
        let mut changes = Vec::new();
        for (peer, st) in &mut self.peers {
            let missed =
                (now.saturating_duration_since(st.last_seen).as_millis() / self.cfg.interval.as_millis().max(1)) as u32;
            let next = match missed {
                0 => PeerHealth::Healthy,
                m if m >= self.cfg.down_after => PeerHealth::Down,
                m => PeerHealth::Suspect(m),
            };
            // Down is sticky until a heartbeat arrives.
            if next != st.health && st.health != PeerHealth::Down {
                st.health = next;
                changes.push((peer.clone(), next));
            }
        }
        changes
    }

    pub fn health(&self, peer: &str) -> Option<PeerHealth> {
        self.peers.get(peer).map(|s| s.health)
    }
}

/// Heartbeat datagram: `MAGIC ‖ seq u64 ‖ id_len u8 ‖ node id ‖ blake3-keyed MAC (32)`.
pub fn encode(key: &[u8; 32], node_id: &str, seq: u64) -> Result<Vec<u8>> {
    let id = node_id.as_bytes();
    let id_len = u8::try_from(id.len()).map_err(|_| Error::Invalid("node id too long".into()))?;
    let mut out = Vec::with_capacity(4 + 8 + 1 + id.len() + 32);
    out.extend_from_slice(MAGIC);
    out.extend_from_slice(&seq.to_be_bytes());
    out.push(id_len);
    out.extend_from_slice(id);
    let mac = blake3::keyed_hash(key, &out);
    out.extend_from_slice(mac.as_bytes());
    Ok(out)
}

/// Verify and decode a heartbeat; returns (node id, seq).
pub fn decode(key: &[u8; 32], msg: &[u8]) -> Result<(String, u64)> {
    let bad = |m: &str| Error::Crypto(format!("heartbeat: {m}"));
    if msg.len() < 4 + 8 + 1 + 32 || &msg[..4] != MAGIC {
        return Err(bad("malformed"));
    }
    let (body, mac) = msg.split_at(msg.len() - 32);
    let expected = blake3::keyed_hash(key, body);
    // blake3::Hash equality is constant-time.
    let mac: [u8; 32] = mac.try_into().map_err(|_| bad("mac length"))?;
    if expected != blake3::Hash::from(mac) {
        return Err(bad("bad mac"));
    }
    let mut seq = [0u8; 8];
    seq.copy_from_slice(&body[4..12]);
    let id_len = body[12] as usize;
    let id = body.get(13..13 + id_len).ok_or_else(|| bad("truncated id"))?;
    let id = std::str::from_utf8(id).map_err(|_| bad("id not utf-8"))?;
    Ok((id.to_owned(), u64::from_be_bytes(seq)))
}

/// Send heartbeats to `targets` forever.
pub async fn run_sender(
    socket: &UdpSocket,
    key: [u8; 32],
    node_id: String,
    targets: Vec<SocketAddr>,
    interval: Duration,
) -> Result<()> {
    let mut seq = 0u64;
    let mut tick = tokio::time::interval(interval);
    loop {
        tick.tick().await;
        seq += 1;
        let msg = encode(&key, &node_id, seq)?;
        for t in &targets {
            if let Err(e) = socket.send_to(&msg, t).await {
                tracing::debug!(target = %t, error = %e, "heartbeat send failed");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn three_missed_beats_mark_down_and_a_beat_recovers() {
        let cfg = HeartbeatConfig { interval: Duration::from_millis(100), down_after: 3 };
        let mut t = HeartbeatTracker::new(cfg);
        let t0 = Instant::now();
        t.watch("host-b", t0);
        assert!(t.evaluate(t0 + Duration::from_millis(50)).is_empty());
        assert_eq!(t.evaluate(t0 + Duration::from_millis(150)), vec![("host-b".into(), PeerHealth::Suspect(1))]);
        assert_eq!(t.evaluate(t0 + Duration::from_millis(250)), vec![("host-b".into(), PeerHealth::Suspect(2))]);
        assert_eq!(t.evaluate(t0 + Duration::from_millis(350)), vec![("host-b".into(), PeerHealth::Down)]);
        assert!(t.evaluate(t0 + Duration::from_millis(900)).is_empty(), "down is sticky");
        assert_eq!(t.observe("host-b", 7, t0 + Duration::from_millis(950)), Some(PeerHealth::Healthy));
        assert_eq!(t.observe("host-b", 6, t0 + Duration::from_millis(960)), None, "stale seq ignored");
    }

    #[test]
    fn heartbeat_mac_roundtrip() {
        let key = [3u8; 32];
        let msg = encode(&key, "node-a", 42).expect("encode");
        assert_eq!(decode(&key, &msg).expect("decode"), ("node-a".to_owned(), 42));
        let mut forged = msg.clone();
        forged[5] ^= 1;
        assert!(decode(&key, &forged).is_err());
        assert!(decode(&[4u8; 32], &msg).is_err());
    }
}
