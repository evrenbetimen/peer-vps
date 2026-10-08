//! Solution B — asynchronous block-level replication to a warm standby.
//!
//! The primary's virtio-blk backend reports every guest write to
//! [`BlockReplicator::record_write`], which only marks the block dirty (no
//! I/O on the guest's write path). A background loop ships dirty blocks to the
//! replica in batches tagged with a monotonically increasing generation, so the
//! replica can apply them idempotently and report how far behind it is.
//! When the primary is declared down, the replica's disk is at most one batch
//! behind, and the overlay route is flipped to it ([`super::FailoverController`]).

use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;

use crate::{Error, Result};

pub const BLOCK_SIZE: usize = 4096;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BlockBatch {
    pub generation: u64,
    pub blocks: Vec<(u64, Vec<u8>)>,
}

/// Receiving side of replication (remote replica over the overlay in production).
#[async_trait]
pub trait ReplicaSink: Send + Sync {
    /// Apply a batch; returns the replica's latest applied generation.
    async fn apply(&self, batch: BlockBatch) -> Result<u64>;
}

/// Source of current block contents (the primary's disk image).
#[async_trait]
pub trait BlockSource: Send + Sync {
    async fn read_block(&self, index: u64) -> Result<Vec<u8>>;
}

#[derive(Debug, Default)]
pub struct BlockReplicator {
    dirty: Mutex<BTreeSet<u64>>,
    generation: Mutex<u64>,
    pub max_batch: usize,
}

impl BlockReplicator {
    pub fn new(max_batch: usize) -> Self {
        Self { dirty: Mutex::new(BTreeSet::new()), generation: Mutex::new(0), max_batch: max_batch.max(1) }
    }

    pub async fn record_write(&self, first_block: u64, count: u64) {
        let mut d = self.dirty.lock().await;
        d.extend(first_block..first_block + count);
    }

    pub async fn backlog(&self) -> usize {
        self.dirty.lock().await.len()
    }

    /// Ship up to `max_batch` dirty blocks. Blocks are re-read at send time so
    /// repeated writes to one block coalesce. On failure the blocks stay dirty.
    pub async fn flush_once(&self, source: &dyn BlockSource, sink: &dyn ReplicaSink) -> Result<usize> {
        let picked: Vec<u64> = {
            let mut d = self.dirty.lock().await;
            let picked: Vec<u64> = d.iter().take(self.max_batch).copied().collect();
            for b in &picked {
                d.remove(b);
            }
            picked
        };
        if picked.is_empty() {
            return Ok(0);
        }
        let mut blocks = Vec::with_capacity(picked.len());
        for &b in &picked {
            match source.read_block(b).await {
                Ok(data) => blocks.push((b, data)),
                Err(e) => {
                    self.dirty.lock().await.extend(&picked);
                    return Err(e);
                }
            }
        }
        let generation = {
            let mut g = self.generation.lock().await;
            *g += 1;
            *g
        };
        if let Err(e) = sink.apply(BlockBatch { generation, blocks }).await {
            self.dirty.lock().await.extend(&picked);
            return Err(e);
        }
        Ok(picked.len())
    }

    /// Replicate continuously until the task is dropped.
    pub async fn run(&self, source: &dyn BlockSource, sink: &dyn ReplicaSink, every: Duration) {
        let mut tick = tokio::time::interval(every);
        loop {
            tick.tick().await;
            loop {
                match self.flush_once(source, sink).await {
                    Ok(0) => break,
                    Ok(_) => continue,
                    Err(e) => {
                        tracing::warn!(error = %e, "replication batch failed; will retry");
                        break;
                    }
                }
            }
        }
    }
}

/// In-memory disk usable as both a source and a replica sink.
#[derive(Debug, Default)]
pub struct MemDisk {
    blocks: Mutex<BTreeMap<u64, Vec<u8>>>,
    applied_generation: Mutex<u64>,
}

impl MemDisk {
    pub async fn write(&self, index: u64, data: Vec<u8>) {
        self.blocks.lock().await.insert(index, data);
    }

    pub async fn digest(&self) -> blake3::Hash {
        let mut h = blake3::Hasher::new();
        for (i, b) in self.blocks.lock().await.iter() {
            h.update(&i.to_be_bytes());
            h.update(b);
        }
        h.finalize()
    }
}

#[async_trait]
impl BlockSource for MemDisk {
    async fn read_block(&self, index: u64) -> Result<Vec<u8>> {
        Ok(self.blocks.lock().await.get(&index).cloned().unwrap_or_else(|| vec![0; BLOCK_SIZE]))
    }
}

#[async_trait]
impl ReplicaSink for MemDisk {
    async fn apply(&self, batch: BlockBatch) -> Result<u64> {
        let mut applied = self.applied_generation.lock().await;
        if batch.generation <= *applied {
            return Ok(*applied); // duplicate delivery
        }
        let mut blocks = self.blocks.lock().await;
        for (i, data) in batch.blocks {
            if data.len() != BLOCK_SIZE {
                return Err(Error::Invalid(format!("block {i} has wrong size")));
            }
            blocks.insert(i, data);
        }
        *applied = batch.generation;
        Ok(*applied)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn replica_converges_and_coalesces_rewrites() {
        let primary = MemDisk::default();
        let replica = MemDisk::default();
        let rep = BlockReplicator::new(2);
        for i in 0..5u64 {
            primary.write(i, vec![i as u8; BLOCK_SIZE]).await;
            rep.record_write(i, 1).await;
        }
        primary.write(3, vec![0xAB; BLOCK_SIZE]).await;
        rep.record_write(3, 1).await; // already dirty: coalesced
        assert_eq!(rep.backlog().await, 5);

        while rep.flush_once(&primary, &replica).await.expect("flush") > 0 {}
        assert_eq!(primary.digest().await, replica.digest().await);
        assert_eq!(rep.backlog().await, 0);
    }
}
