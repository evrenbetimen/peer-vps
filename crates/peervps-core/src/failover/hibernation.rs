//! Solution A — snapshot hibernation on host shutdown.
//!
//! On `SIGTERM`/`SIGINT` (what systemd and ACPI power-button handling deliver
//! to services on shutdown), every running guest is paused and serialized,
//! the image is zstd-compressed, sealed in authenticated chunks with
//! ChaCha20-Poly1305 under a per-snapshot key, and streamed to the best DHT
//! holder for that VM. Only after every transfer is acknowledged is the node's
//! presence recorded as `hibernated`, which is what spares its collateral.
//!
//! The per-snapshot key is generated here and returned to the caller to be
//! wrapped for the renter (and the restoring host) out of band; the receiving
//! peer only ever stores ciphertext.

use std::sync::Arc;

use async_trait::async_trait;
use chacha20poly1305::aead::{Aead, KeyInit, Payload};
use chacha20poly1305::{ChaCha20Poly1305, Key, Nonce};
use rand::Rng;
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;

use super::dht::{Contact, NodeId, RoutingTable as DhtTable};
use crate::storage::presence::{self, PresenceEvent};
use crate::storage::{Store, now_secs};
use crate::virtualization::{Provisioner, VmId, VmSnapshot, VmState};
use crate::{Error, Result};

pub const CHUNK_SIZE: usize = 1 << 20;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SnapshotManifest {
    pub vm: VmId,
    pub chunks: u32,
    pub compressed_len: u64,
    pub raw_len: u64,
    /// BLAKE3 of the plaintext snapshot, checked after restore.
    pub digest: String,
}

#[derive(Debug, Clone)]
pub struct SealedSnapshot {
    pub manifest: SnapshotManifest,
    pub chunks: Vec<Vec<u8>>,
}

fn chunk_aad(m: &SnapshotManifest, index: u32) -> Vec<u8> {
    // Binding vm id, position and count prevents chunk reordering, truncation
    // and splicing chunks from another snapshot.
    let mut aad = Vec::with_capacity(16 + 8);
    aad.extend_from_slice(m.vm.0.as_bytes());
    aad.extend_from_slice(&index.to_be_bytes());
    aad.extend_from_slice(&m.chunks.to_be_bytes());
    aad
}

fn chunk_nonce(index: u32) -> Nonce {
    let mut n = [0u8; 12];
    n[8..].copy_from_slice(&index.to_be_bytes());
    *Nonce::from_slice(&n)
}

/// Compress and encrypt a snapshot. Returns the sealed form and its fresh key.
pub fn seal(snapshot: &VmSnapshot, zstd_level: i32) -> Result<(SealedSnapshot, [u8; 32])> {
    let mut key = [0u8; 32];
    rand::rng().fill(&mut key);
    let compressed = zstd::bulk::compress(&snapshot.bytes, zstd_level)?;
    let chunks = u32::try_from(compressed.len().div_ceil(CHUNK_SIZE).max(1))
        .map_err(|_| Error::Invalid("snapshot too large".into()))?;
    let manifest = SnapshotManifest {
        vm: snapshot.vm,
        chunks,
        compressed_len: compressed.len() as u64,
        raw_len: snapshot.bytes.len() as u64,
        digest: blake3::hash(&snapshot.bytes).to_hex().to_string(),
    };
    let cipher = ChaCha20Poly1305::new(Key::from_slice(&key));
    let sealed = compressed
        .chunks(CHUNK_SIZE)
        .chain(std::iter::once(&[][..]).take(usize::from(compressed.is_empty())))
        .enumerate()
        .map(|(i, part)| {
            let i = i as u32;
            cipher
                .encrypt(&chunk_nonce(i), Payload { msg: part, aad: &chunk_aad(&manifest, i) })
                .map_err(|_| Error::Crypto("seal chunk".into()))
        })
        .collect::<Result<Vec<_>>>()?;
    Ok((SealedSnapshot { manifest, chunks: sealed }, key))
}

/// Decrypt, decompress and integrity-check a sealed snapshot.
pub fn open(sealed: &SealedSnapshot, key: &[u8; 32]) -> Result<VmSnapshot> {
    let m = &sealed.manifest;
    if sealed.chunks.len() != m.chunks as usize {
        return Err(Error::Crypto("chunk count mismatch".into()));
    }
    let cipher = ChaCha20Poly1305::new(Key::from_slice(key));
    let mut compressed = Vec::with_capacity(m.compressed_len as usize);
    for (i, c) in sealed.chunks.iter().enumerate() {
        let i = i as u32;
        let part = cipher
            .decrypt(&chunk_nonce(i), Payload { msg: c, aad: &chunk_aad(m, i) })
            .map_err(|_| Error::Crypto(format!("chunk {i} failed authentication")))?;
        compressed.extend_from_slice(&part);
    }
    let bytes = zstd::bulk::decompress(&compressed, m.raw_len as usize)?;
    if blake3::hash(&bytes).to_hex().as_str() != m.digest {
        return Err(Error::Crypto("snapshot digest mismatch".into()));
    }
    Ok(VmSnapshot { vm: m.vm, bytes })
}

/// Ships sealed snapshots to a peer (QUIC/UDP stream over the overlay in production).
#[async_trait]
pub trait SnapshotTransport: Send + Sync + std::fmt::Debug {
    async fn send(&self, to: &Contact, snapshot: &SealedSnapshot) -> Result<()>;
}

/// Transport that keeps snapshots in memory; used in tests and the desktop demo.
#[derive(Debug, Default)]
pub struct InMemoryTransport {
    pub delivered: Mutex<Vec<(NodeId, SealedSnapshot)>>,
}

#[async_trait]
impl SnapshotTransport for InMemoryTransport {
    async fn send(&self, to: &Contact, snapshot: &SealedSnapshot) -> Result<()> {
        self.delivered.lock().await.push((to.id, snapshot.clone()));
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HibernationReceipt {
    pub vm: VmId,
    pub holder: String,
    pub chunks: u32,
    pub compressed_len: u64,
    pub raw_len: u64,
    /// Snapshot key; must be wrapped for the renter before leaving this process.
    #[serde(skip)]
    pub key: [u8; 32],
}

#[derive(Debug, Clone)]
pub struct HibernationManager {
    pub node_id: String,
    provisioner: Provisioner,
    dht: Arc<Mutex<DhtTable>>,
    transport: Arc<dyn SnapshotTransport>,
    store: Store,
}

impl HibernationManager {
    pub fn new(
        node_id: String,
        provisioner: Provisioner,
        dht: Arc<Mutex<DhtTable>>,
        transport: Arc<dyn SnapshotTransport>,
        store: Store,
    ) -> Self {
        Self { node_id, provisioner, dht, transport, store }
    }

    /// Hibernate every running VM. Presence is marked `hibernated` only if all succeed.
    pub async fn hibernate_all(&self) -> Result<Vec<HibernationReceipt>> {
        let mut receipts = Vec::new();
        let mut first_err = None;
        for vm in self.provisioner.list().await.into_iter().filter(|v| v.state == VmState::Running) {
            match self.hibernate_one(vm.id).await {
                Ok(r) => receipts.push(r),
                Err(e) => {
                    tracing::error!(vm = %vm.id, error = %e, "hibernation failed");
                    first_err.get_or_insert(e);
                }
            }
        }
        if let Some(e) = first_err {
            return Err(e);
        }
        let node = self.node_id.clone();
        self.store.call(move |c| presence::record(c, &node, PresenceEvent::Hibernated, now_secs())).await?;
        Ok(receipts)
    }

    pub async fn hibernate_one(&self, vm: VmId) -> Result<HibernationReceipt> {
        let snapshot = self.provisioner.hibernate(vm).await?;
        let (sealed, key) = tokio::task::spawn_blocking(move || seal(&snapshot, 3)).await??;
        let holder = self
            .dht
            .lock()
            .await
            .best_holder(&NodeId::from_key(vm.0.as_bytes()))
            .ok_or_else(|| Error::NotFound("no online peer to hold the snapshot".into()))?;
        self.transport.send(&holder, &sealed).await?;
        Ok(HibernationReceipt {
            vm,
            holder: holder.addr.to_string(),
            chunks: sealed.manifest.chunks,
            compressed_len: sealed.manifest.compressed_len,
            raw_len: sealed.manifest.raw_len,
            key,
        })
    }

    /// Wait for a shutdown signal, then hibernate everything.
    pub async fn run_until_shutdown(self) -> Result<Vec<HibernationReceipt>> {
        wait_for_shutdown_signal().await?;
        tracing::warn!("shutdown signal received: hibernating guests");
        self.hibernate_all().await
    }
}

/// Resolves on SIGTERM/SIGINT (Unix) or Ctrl-C (elsewhere).
///
/// TODO(failover): also take a systemd-logind `PrepareForShutdown` delay
/// inhibitor on Linux so hibernation gets time to finish before poweroff.
pub async fn wait_for_shutdown_signal() -> Result<()> {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        let mut term = signal(SignalKind::terminate())?;
        let mut int = signal(SignalKind::interrupt())?;
        tokio::select! {
            _ = term.recv() => {},
            _ = int.recv() => {},
        }
        Ok(())
    }
    #[cfg(not(unix))]
    {
        tokio::signal::ctrl_c().await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn seal_open_roundtrip_and_tamper_detection() {
        // ~2.5 MiB of noise (multi-chunk after compression) followed by 1 MiB of zero pages.
        let mut x = 0x9E37_79B9_7F4A_7C15u64;
        let mut bytes: Vec<u8> = (0..(5 * CHUNK_SIZE / 2))
            .map(|_| {
                x ^= x << 13;
                x ^= x >> 7;
                x ^= x << 17;
                x as u8
            })
            .collect();
        bytes.resize(bytes.len() + CHUNK_SIZE, 0);
        let snap = VmSnapshot { vm: VmId::new(), bytes };
        let (sealed, key) = seal(&snap, 1).expect("seal");
        assert!(sealed.manifest.compressed_len < snap.bytes.len() as u64);
        assert!(sealed.chunks.len() >= 2);
        assert_eq!(open(&sealed, &key).expect("open"), snap);

        let mut swapped = sealed.clone();
        swapped.chunks.swap(0, 1);
        assert!(open(&swapped, &key).is_err(), "reordered chunks must fail");
        let mut truncated = sealed.clone();
        truncated.chunks.pop();
        assert!(open(&truncated, &key).is_err(), "missing chunk must fail");
        assert!(open(&sealed, &[0u8; 32]).is_err());
    }
}
