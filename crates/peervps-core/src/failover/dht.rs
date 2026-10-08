//! Kademlia routing table used to find where to ship a hibernation snapshot.
//!
//! Node ids and content keys share one 256-bit space. A snapshot for VM `v`
//! is stored on the peers closest (XOR metric) to `blake3(v)`, so anyone can
//! later find it without a central index. Among those `k` closest, the
//! hibernating host prefers the one with the lowest measured RTT.
//!
//! The network RPCs (PING / FIND_NODE / STORE) will ride on the overlay's UDP
//! socket; this module is the routing-table core they need.

use std::cmp::Ordering;
use std::fmt;
use std::net::SocketAddr;
use std::time::Duration;

use serde::{Deserialize, Serialize};

pub const K: usize = 20;
const BITS: usize = 256;

#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct NodeId(pub [u8; 32]);

impl NodeId {
    pub fn from_key(bytes: &[u8]) -> Self {
        Self(*blake3::hash(bytes).as_bytes())
    }

    pub fn distance(&self, other: &NodeId) -> [u8; 32] {
        let mut d = [0u8; 32];
        for (i, b) in d.iter_mut().enumerate() {
            *b = self.0[i] ^ other.0[i];
        }
        d
    }

    /// Index of the k-bucket `other` falls into (0 = farthest half of the space).
    fn bucket_index(&self, other: &NodeId) -> Option<usize> {
        let d = self.distance(other);
        let leading = d.iter().position(|b| *b != 0)?;
        Some(leading * 8 + d[leading].leading_zeros() as usize)
    }
}

impl fmt::Debug for NodeId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "NodeId({})", hex::encode(&self.0[..6]))
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Contact {
    pub id: NodeId,
    pub addr: SocketAddr,
    pub rtt: Option<Duration>,
    pub online: bool,
}

#[derive(Debug)]
pub struct RoutingTable {
    local: NodeId,
    buckets: Vec<Vec<Contact>>,
}

impl RoutingTable {
    pub fn new(local: NodeId) -> Self {
        Self { local, buckets: (0..BITS).map(|_| Vec::new()).collect() }
    }

    pub fn local(&self) -> NodeId {
        self.local
    }

    /// Insert or refresh a contact. Kademlia keeps long-lived contacts: when a
    /// bucket is full the newcomer is dropped unless an offline entry can be evicted.
    pub fn upsert(&mut self, contact: Contact) -> bool {
        let Some(idx) = self.local.bucket_index(&contact.id) else { return false };
        let bucket = &mut self.buckets[idx];
        if let Some(pos) = bucket.iter().position(|c| c.id == contact.id) {
            bucket.remove(pos);
            bucket.push(contact); // most-recently-seen at the tail
            return true;
        }
        if bucket.len() < K {
            bucket.push(contact);
            return true;
        }
        if let Some(pos) = bucket.iter().position(|c| !c.online) {
            bucket.remove(pos);
            bucket.push(contact);
            return true;
        }
        false
    }

    pub fn set_online(&mut self, id: &NodeId, online: bool) {
        for b in &mut self.buckets {
            if let Some(c) = b.iter_mut().find(|c| c.id == *id) {
                c.online = online;
            }
        }
    }

    pub fn len(&self) -> usize {
        self.buckets.iter().map(Vec::len).sum()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// The `n` known contacts closest to `target` by XOR distance.
    pub fn closest(&self, target: &NodeId, n: usize) -> Vec<Contact> {
        let mut all: Vec<&Contact> = self.buckets.iter().flatten().collect();
        all.sort_by_key(|c| c.id.distance(target));
        all.into_iter().take(n).cloned().collect()
    }

    /// Best online peer to hold data keyed by `key`: among the `K` closest, lowest RTT wins.
    pub fn best_holder(&self, key: &NodeId) -> Option<Contact> {
        self.closest(key, K).into_iter().filter(|c| c.online).min_by(|a, b| match (a.rtt, b.rtt) {
            (Some(x), Some(y)) => x.cmp(&y),
            (Some(_), None) => Ordering::Less,
            (None, Some(_)) => Ordering::Greater,
            (None, None) => Ordering::Equal,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id(b: u8) -> NodeId {
        let mut a = [0u8; 32];
        a[0] = b;
        NodeId(a)
    }

    fn contact(b: u8, rtt_ms: u64, online: bool) -> Contact {
        Contact {
            id: id(b),
            addr: format!("10.0.0.{b}:7000").parse().expect("addr"),
            rtt: Some(Duration::from_millis(rtt_ms)),
            online,
        }
    }

    #[test]
    fn closest_orders_by_xor_distance() {
        let mut t = RoutingTable::new(id(0));
        for b in [0b1000_0000, 0b0100_0000, 0b0000_0011, 0b0000_0001] {
            t.upsert(contact(b, 10, true));
        }
        let near = t.closest(&id(0b0000_0010), 2);
        assert_eq!(near.iter().map(|c| c.id.0[0]).collect::<Vec<_>>(), vec![0b0000_0011, 0b0000_0001]);
    }

    #[test]
    fn best_holder_skips_offline_and_prefers_low_rtt() {
        let mut t = RoutingTable::new(id(0));
        t.upsert(contact(1, 5, false));
        t.upsert(contact(2, 40, true));
        t.upsert(contact(3, 12, true));
        assert_eq!(t.best_holder(&id(1)).expect("holder").id, id(3));
    }
}
