//! Fractional GPU / NPU partitioning.
//!
//! A physical accelerator is advertised as `slices` equal partitions (think
//! NVIDIA MIG 1g/2g/3g profiles, AMD SR-IOV VFs, or NPU tiles). Guests ask for
//! N slices; the manager packs them onto one device and hands back the VFIO
//! group the hypervisor should pass through.
//!
//! [`PartitionedAccelerators`] does the bookkeeping in software. Binding the
//! partition to real hardware (creating the MIG instance / VF and the VFIO
//! group) is the job of a vendor backend that will implement the same trait.

use std::collections::HashMap;
use std::fmt;
use std::sync::Mutex;

use serde::{Deserialize, Serialize};

use super::VmId;
use crate::{Error, Result};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum AcceleratorKind {
    None,
    Gpu,
    Npu,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AcceleratorDevice {
    pub id: String,
    pub kind: AcceleratorKind,
    pub model: String,
    pub vram_mib: u64,
    /// Number of equal partitions this device is split into.
    pub slices: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AcceleratorRequest {
    pub kind: AcceleratorKind,
    pub slices: u32,
    /// Minimum VRAM the partition must expose (0 = any).
    #[serde(default)]
    pub min_vram_mib: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AcceleratorPartition {
    pub device_id: String,
    pub kind: AcceleratorKind,
    pub slices: u32,
    pub vram_mib: u64,
    /// VFIO group path to pass through, once a hardware backend creates one.
    pub vfio_group: Option<String>,
}

pub trait AcceleratorManager: Send + Sync + fmt::Debug {
    fn devices(&self) -> Vec<AcceleratorDevice>;
    fn allocate(&self, vm: VmId, req: &AcceleratorRequest) -> Result<AcceleratorPartition>;
    fn release(&self, vm: VmId);
}

/// Software slice accounting over a static device list.
#[derive(Debug, Default)]
pub struct PartitionedAccelerators {
    devices: Vec<AcceleratorDevice>,
    // device id -> (vm, slices)
    used: Mutex<HashMap<String, Vec<(VmId, u32)>>>,
}

impl PartitionedAccelerators {
    pub fn new(devices: Vec<AcceleratorDevice>) -> Self {
        Self { devices, used: Mutex::new(HashMap::new()) }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, Vec<(VmId, u32)>>> {
        // A poisoned lock only means another thread panicked mid-update of a
        // plain map; the data is still structurally valid.
        self.used.lock().unwrap_or_else(|p| p.into_inner())
    }
}

impl AcceleratorManager for PartitionedAccelerators {
    fn devices(&self) -> Vec<AcceleratorDevice> {
        self.devices.clone()
    }

    fn allocate(&self, vm: VmId, req: &AcceleratorRequest) -> Result<AcceleratorPartition> {
        if req.slices == 0 {
            return Err(Error::Invalid("accelerator slices must be > 0".into()));
        }
        let mut used = self.lock();
        for dev in self.devices.iter().filter(|d| d.kind == req.kind) {
            let per_slice = dev.vram_mib / u64::from(dev.slices.max(1));
            let vram = per_slice * u64::from(req.slices);
            if vram < req.min_vram_mib {
                continue;
            }
            let taken: u32 = used.get(&dev.id).map(|v| v.iter().map(|(_, s)| s).sum()).unwrap_or(0);
            if taken + req.slices <= dev.slices {
                used.entry(dev.id.clone()).or_default().push((vm, req.slices));
                return Ok(AcceleratorPartition {
                    device_id: dev.id.clone(),
                    kind: dev.kind,
                    slices: req.slices,
                    vram_mib: vram,
                    vfio_group: None,
                });
            }
        }
        Err(Error::Capacity(format!("no {:?} with {} free slices", req.kind, req.slices)))
    }

    fn release(&self, vm: VmId) {
        for allocations in self.lock().values_mut() {
            allocations.retain(|(id, _)| *id != vm);
        }
    }
}
