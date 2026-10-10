//! Escrow ledger and billing accountant.
//!
//! * [`ledger`] — µcredit accounts with an append-only journal.
//! * [`meter`] — per-second usage settlement renter → provider (+ platform fee).
//! * [`collateral`] — provider lock-up, slashing to renters, pooled GPU staking.
//! * [`peering`] — prepaid credits and refunds between nodes renting from each other.
//! * [`payments`] — gateway checkout hooks and signed, idempotent top-up webhooks.

pub mod collateral;
pub mod ledger;
pub mod meter;
pub mod payments;
pub mod peering;

pub use collateral::{Collateral, CollateralState};
pub use ledger::{AccountKind, Ledger, LedgerEntry, MICROS_PER_CREDIT};
pub use meter::{Settlement, UsageMeter};
pub use peering::PeerFlows;
