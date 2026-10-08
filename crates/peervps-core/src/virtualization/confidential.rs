//! Confidential computing: keep guest RAM and disk opaque to the host's root.
//!
//! On AMD SEV-SNP and Intel TDX the CPU encrypts guest memory with a key the
//! host kernel never sees, and the guest can obtain a signed attestation report
//! binding its launch measurement to a renter-supplied nonce. SGX gives the same
//! property to an enclave inside the guest rather than the whole VM.
//!
//! This module defines the contract and a host probe. The real launch flow
//! (`KVM_SEV_SNP_LAUNCH_START`/`UPDATE`/`FINISH`, TDX `TDH.MNG.*`) and report
//! verification against the vendor certificate chain are deliberately stubbed:
//! [`SevSnpStub`] reports the hardware as present but returns an unsigned report
//! marked `simulated` so nothing downstream can mistake it for a real one.

use std::fmt;
use std::path::Path;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use super::{Placement, VmId};
use crate::{Error, Result};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum TeeKind {
    None,
    SevSnp,
    Tdx,
    Sgx,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TeeCapabilities {
    /// Whole-VM memory encryption (SEV-SNP / TDX).
    pub guest_memory_encrypted: bool,
    /// Memory integrity protection against replay/remap by the host.
    pub integrity_protected: bool,
    /// Disk blocks are encrypted with a key released only to the attested guest.
    pub disk_sealed: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AttestationReport {
    pub tee: TeeKind,
    pub vm: VmId,
    /// Launch measurement (hash of firmware + kernel + initrd + cmdline).
    pub measurement: String,
    /// Renter nonce echoed back, preventing replay of old reports.
    pub report_data: String,
    /// Vendor signature over the report. Empty for simulated reports.
    pub signature: String,
    pub simulated: bool,
}

#[async_trait]
pub trait MemoryEncryption: Send + Sync + fmt::Debug {
    fn kind(&self) -> TeeKind;
    fn capabilities(&self) -> TeeCapabilities;
    /// Called before the hypervisor creates the VM (e.g. SNP launch start).
    async fn prepare_guest(&self, vm: VmId, placement: &Placement) -> Result<()>;
    /// Produce an attestation report bound to `nonce`.
    async fn attest(&self, vm: VmId, nonce: &[u8]) -> Result<AttestationReport>;
}

/// Host without a TEE: guests run unencrypted and cannot be attested.
#[derive(Debug, Default, Clone, Copy)]
pub struct NoEncryption;

#[async_trait]
impl MemoryEncryption for NoEncryption {
    fn kind(&self) -> TeeKind {
        TeeKind::None
    }
    fn capabilities(&self) -> TeeCapabilities {
        TeeCapabilities { guest_memory_encrypted: false, integrity_protected: false, disk_sealed: false }
    }
    async fn prepare_guest(&self, _vm: VmId, _placement: &Placement) -> Result<()> {
        Ok(())
    }
    async fn attest(&self, _vm: VmId, _nonce: &[u8]) -> Result<AttestationReport> {
        Err(Error::Unsupported("host has no trusted execution environment".into()))
    }
}

/// Placeholder for SEV-SNP / TDX. Advertises the capability so the scheduling
/// path can be exercised, but every report it emits has `simulated = true`.
#[derive(Debug, Clone, Copy)]
pub struct SevSnpStub {
    pub kind: TeeKind,
}

#[async_trait]
impl MemoryEncryption for SevSnpStub {
    fn kind(&self) -> TeeKind {
        self.kind
    }
    fn capabilities(&self) -> TeeCapabilities {
        TeeCapabilities {
            guest_memory_encrypted: matches!(self.kind, TeeKind::SevSnp | TeeKind::Tdx),
            integrity_protected: matches!(self.kind, TeeKind::SevSnp | TeeKind::Tdx),
            disk_sealed: true,
        }
    }
    async fn prepare_guest(&self, vm: VmId, _placement: &Placement) -> Result<()> {
        tracing::warn!(%vm, tee = ?self.kind, "simulated TEE launch: guest memory is NOT encrypted");
        Ok(())
    }
    async fn attest(&self, vm: VmId, nonce: &[u8]) -> Result<AttestationReport> {
        let measurement = blake3::hash(format!("simulated-launch:{vm}").as_bytes());
        Ok(AttestationReport {
            tee: self.kind,
            vm,
            measurement: measurement.to_hex().to_string(),
            report_data: hex::encode(nonce),
            signature: String::new(),
            simulated: true,
        })
    }
}

/// Detect which TEE the host kernel exposes.
pub fn probe_host() -> TeeKind {
    if Path::new("/dev/sev").exists() || Path::new("/dev/sev-guest").exists() {
        TeeKind::SevSnp
    } else if Path::new("/dev/tdx_guest").exists() || Path::new("/sys/firmware/tdx").exists() {
        TeeKind::Tdx
    } else if Path::new("/dev/sgx_enclave").exists() {
        TeeKind::Sgx
    } else {
        TeeKind::None
    }
}
