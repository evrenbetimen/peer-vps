//! PeerVPS node engine.
//!
//! The crate is split along the six core subsystems of a PeerVPS node:
//!
//! | module             | responsibility                                                         |
//! |--------------------|------------------------------------------------------------------------|
//! | [`virtualization`] | MicroVM provisioning, accelerator slices, confidential computing, proofs |
//! | [`network`]        | Compressed + encrypted UDP overlay, STUN, hole punching, Noise, routing |
//! | [`failover`]       | Heartbeats, snapshot hibernation, live replication, Kademlia peer lookup |
//! | [`storage`]        | SQLite (WAL) persistence for presence, ledger, collateral and instances |
//! | [`api`]            | Agent-native REST API (gRPC contract lives in `proto/`)                 |
//! | [`billing`]        | Per-second ledger, collateral locking/slashing, pools, payment webhooks |
//!
//! Everything that needs real hardware (KVM, SEV-SNP/TDX/SGX, vGPU partitioning,
//! zero-knowledge proving) sits behind a trait with a software stub so the whole
//! node can be built, tested and demoed on any machine.

pub mod api;
pub mod billing;
pub mod error;
pub mod events;
pub mod failover;
pub mod network;
pub mod node;
pub mod storage;
pub mod virtualization;

pub use error::{Error, Result};
pub use events::{EventBus, NodeEvent};
pub use node::Node;
