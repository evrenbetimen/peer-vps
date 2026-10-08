//! Direct KVM backend (`/dev/kvm` through `kvm-ioctls`).
//!
//! Implemented today: capability probe, VM fd creation, guest RAM allocation
//! and registration, one vCPU fd per requested core.
//!
//! Not yet implemented (returns [`Error::Unsupported`]): loading a kernel and
//! running the vCPU loop, pause/resume and snapshotting. Those arrive with the
//! boot path (`linux-loader` + virtio-mmio devices, or a Firecracker process
//! backend behind the same [`Hypervisor`] trait).
//!
//! This is the only module allowed to use `unsafe`, because registering guest
//! memory with KVM is inherently an FFI contract about raw host addresses.
#![allow(unsafe_code)]

use std::collections::HashMap;
use std::fmt;

use async_trait::async_trait;
use kvm_bindings::kvm_userspace_memory_region;
use kvm_ioctls::{Cap, Kvm, VcpuFd, VmFd};
use tokio::sync::Mutex;
use vm_memory::{GuestAddress, GuestMemory, GuestMemoryMmap, GuestMemoryRegion};

use super::{Hypervisor, Placement, VmId, VmSnapshot, VmSpec};
use crate::{Error, Result};

struct KvmVm {
    // Fields drop in declaration order. The kernel VM object lives until every
    // vCPU fd and the VM fd are closed, and it holds a pointer into `_memory`,
    // so the mapping must be the last thing released.
    _vcpus: Vec<VcpuFd>,
    _vm: VmFd,
    _memory: GuestMemoryMmap,
}

pub struct KvmHypervisor {
    kvm: Kvm,
    vms: Mutex<HashMap<VmId, KvmVm>>,
}

impl fmt::Debug for KvmHypervisor {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("KvmHypervisor").finish_non_exhaustive()
    }
}

/// Result of probing `/dev/kvm`.
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct KvmInfo {
    pub api_version: i32,
    pub max_vcpus: usize,
    pub user_memory: bool,
    pub irqchip: bool,
}

impl KvmHypervisor {
    /// Open `/dev/kvm`. Fails on hosts without hardware virtualization or permission.
    pub fn open() -> Result<Self> {
        let kvm = Kvm::new().map_err(|e| Error::Hypervisor(format!("open /dev/kvm: {e}")))?;
        if !kvm.check_extension(Cap::UserMemory) {
            return Err(Error::Unsupported("KVM_CAP_USER_MEMORY".into()));
        }
        Ok(Self { kvm, vms: Mutex::new(HashMap::new()) })
    }

    pub fn info(&self) -> KvmInfo {
        KvmInfo {
            api_version: self.kvm.get_api_version(),
            max_vcpus: self.kvm.get_max_vcpus(),
            user_memory: self.kvm.check_extension(Cap::UserMemory),
            irqchip: self.kvm.check_extension(Cap::Irqchip),
        }
    }
}

fn hv_err(what: &str) -> impl FnOnce(kvm_ioctls::Error) -> Error + '_ {
    move |e| Error::Hypervisor(format!("{what}: {e}"))
}

#[async_trait]
impl Hypervisor for KvmHypervisor {
    fn name(&self) -> &'static str {
        "kvm"
    }

    async fn create(&self, id: VmId, spec: &VmSpec, placement: &Placement) -> Result<()> {
        if spec.vcpus as usize > self.kvm.get_max_vcpus() {
            return Err(Error::Capacity(format!("KVM allows {} vcpus", self.kvm.get_max_vcpus())));
        }
        let vm = self.kvm.create_vm().map_err(hv_err("KVM_CREATE_VM"))?;

        let mem_bytes = (placement.mem_mib as usize) << 20;
        let memory = GuestMemoryMmap::<()>::from_ranges(&[(GuestAddress(0), mem_bytes)])
            .map_err(|e| Error::Hypervisor(format!("allocate guest memory: {e}")))?;
        for (slot, region) in memory.iter().enumerate() {
            let host_addr = memory
                .get_host_address(region.start_addr())
                .map_err(|e| Error::Hypervisor(format!("guest memory host address: {e}")))?;
            let region_desc = kvm_userspace_memory_region {
                slot: slot as u32,
                flags: 0,
                guest_phys_addr: region.start_addr().0,
                memory_size: region.len(),
                userspace_addr: host_addr as u64,
            };
            // SAFETY: `host_addr` points at a live anonymous mapping of exactly
            // `region.len()` bytes owned by `memory`. `memory` is stored in the
            // same `KvmVm` as `vm` and is dropped only after `vm`'s fd is closed,
            // so KVM never holds a dangling userspace address.
            unsafe { vm.set_user_memory_region(region_desc) }.map_err(hv_err("KVM_SET_USER_MEMORY_REGION"))?;
        }

        let vcpus = (0..u64::from(spec.vcpus))
            .map(|i| vm.create_vcpu(i).map_err(hv_err("KVM_CREATE_VCPU")))
            .collect::<Result<Vec<_>>>()?;
        // Core pinning (sched_setaffinity on each vCPU thread to
        // `placement.pinned_cores`) happens when the run loop spawns threads.
        tracing::info!(%id, vcpus = spec.vcpus, mem_mib = placement.mem_mib, cores = ?placement.pinned_cores, "kvm vm created");

        self.vms.lock().await.insert(id, KvmVm { _vcpus: vcpus, _vm: vm, _memory: memory });
        Ok(())
    }

    async fn start(&self, _id: VmId) -> Result<()> {
        Err(Error::Unsupported("kvm backend: kernel boot / vCPU run loop not implemented yet".into()))
    }

    async fn pause(&self, _id: VmId) -> Result<()> {
        Err(Error::Unsupported("kvm backend: pause".into()))
    }

    async fn resume(&self, _id: VmId) -> Result<()> {
        Err(Error::Unsupported("kvm backend: resume".into()))
    }

    async fn snapshot(&self, _id: VmId) -> Result<VmSnapshot> {
        Err(Error::Unsupported("kvm backend: snapshot".into()))
    }

    async fn restore(&self, _snapshot: VmSnapshot, _placement: &Placement) -> Result<()> {
        Err(Error::Unsupported("kvm backend: restore".into()))
    }

    async fn destroy(&self, id: VmId) -> Result<()> {
        self.vms.lock().await.remove(&id);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::virtualization::VmSpec;

    #[tokio::test]
    async fn creates_vm_when_kvm_is_available() {
        let Ok(hv) = KvmHypervisor::open() else {
            eprintln!("skipping: /dev/kvm not available on this machine");
            return;
        };
        assert!(hv.info().api_version >= 12);
        let spec = VmSpec {
            vcpus: 1,
            mem_mib: 128,
            disk_gib: 1,
            image: "none".into(),
            accelerator: None,
            confidential: false,
        };
        let placement = Placement { pinned_cores: vec![0], mem_mib: 128, disk_gib: 1, accelerator: None };
        let id = VmId::new();
        hv.create(id, &spec, &placement).await.expect("create");
        hv.destroy(id).await.expect("destroy");
    }
}
