//! MicroVM provisioning.
//!
//! [`Provisioner`] owns the host's allocatable budget (cores, RAM, disk,
//! accelerator slices), turns a renter's [`VmSpec`] into a concrete
//! [`Placement`] (pinned cores, accelerator partition), and drives a
//! [`Hypervisor`] backend. Backends:
//!
//! * [`kvm::KvmHypervisor`] — direct `/dev/kvm` via `kvm-ioctls` (Linux only).
//! * [`firecracker::FirecrackerHypervisor`] — one Firecracker process per guest;
//!   boots real kernels, pauses, snapshots and restores (Linux only).
//! * [`qemu::QemuHypervisor`] — one QEMU process per guest with the host's native
//!   accelerator (KVM on Linux, Hypervisor.framework on macOS, WHPX on Windows);
//!   boots stock cloud images with SSH access on every desktop OS.
//! * [`mock::MockHypervisor`] — in-memory backend for tests, CI and hosts without a hypervisor.
//!

pub mod accel;
pub mod affinity;
pub mod confidential;
#[cfg(target_os = "linux")]
pub mod firecracker;
pub mod images;
#[cfg(target_os = "linux")]
pub mod kvm;
pub mod mock;
pub mod proof;
pub mod qemu;

use std::collections::{BTreeSet, HashMap};
use std::fmt;
use std::sync::Arc;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;
use uuid::Uuid;

use crate::events::{EventBus, NodeEvent};
use crate::{Error, Result};
use accel::{AcceleratorKind, AcceleratorManager, AcceleratorPartition, AcceleratorRequest};
use confidential::MemoryEncryption;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct VmId(pub Uuid);

impl VmId {
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }
}

impl Default for VmId {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Display for VmId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

/// What a renter asks for.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct VmSpec {
    pub vcpus: u32,
    pub mem_mib: u64,
    pub disk_gib: u64,
    /// OS / AI template identifier, e.g. `ubuntu-24.04` or `ubuntu-24.04-cuda`.
    pub image: String,
    #[serde(default)]
    pub accelerator: Option<AcceleratorRequest>,
    /// Require hardware memory encryption (SEV-SNP / TDX); refuse hosts without it.
    #[serde(default)]
    pub confidential: bool,
}

/// Concrete resources a VM was given on this host.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Placement {
    pub pinned_cores: Vec<u32>,
    pub mem_mib: u64,
    pub disk_gib: u64,
    pub accelerator: Option<AcceleratorPartition>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum VmState {
    Created,
    Running,
    Paused,
    Hibernated,
    Stopped,
}

/// Opaque serialized VM state (vCPU registers + device state + dirty RAM).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VmSnapshot {
    pub vm: VmId,
    pub bytes: Vec<u8>,
}

/// How a renter reaches a running guest from the host it runs on.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GuestAccess {
    pub ssh_host: String,
    pub ssh_port: u16,
    /// Login user; empty when it is chosen during an OS installation.
    pub user: String,
    /// Set when the guest was provisioned with password login.
    pub password: Option<String>,
    /// Windows guest: sign in over RDP, there is no SSH server by default.
    #[serde(default)]
    pub windows: bool,
    /// Remote Desktop endpoint (`host:port`), for Windows guests.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rdp: Option<String>,
    /// The guest's screen (`vnc://host:port`), for guests installed from an ISO.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_password: Option<String>,
}

impl GuestAccess {
    pub fn ssh_command(&self) -> String {
        if self.user.is_empty() {
            format!("ssh -p {} {}", self.ssh_port, self.ssh_host)
        } else {
            format!("ssh -p {} {}@{}", self.ssh_port, self.user, self.ssh_host)
        }
    }
}

/// Hypervisor backend contract. Implementations must be cheap to share.
#[async_trait]
pub trait Hypervisor: Send + Sync + fmt::Debug {
    fn name(&self) -> &'static str;
    async fn create(&self, id: VmId, spec: &VmSpec, placement: &Placement) -> Result<()>;
    async fn start(&self, id: VmId) -> Result<()>;
    async fn pause(&self, id: VmId) -> Result<()>;
    async fn resume(&self, id: VmId) -> Result<()>;
    /// Serialize a *paused* VM.
    async fn snapshot(&self, id: VmId) -> Result<VmSnapshot>;
    async fn restore(&self, snapshot: VmSnapshot, placement: &Placement) -> Result<()>;
    async fn destroy(&self, id: VmId) -> Result<()>;
    /// Last `max_bytes` of the guest's serial console, if the backend captures it.
    async fn console_tail(&self, _id: VmId, _max_bytes: usize) -> Result<Option<String>> {
        Ok(None)
    }
    /// Type `data` into the guest's serial console, if the backend accepts input.
    async fn console_write(&self, _id: VmId, _data: &[u8]) -> Result<()> {
        Err(Error::Unsupported(format!("{} has a read-only console", self.name())))
    }
    /// SSH endpoint for the guest, if the backend wires one up.
    async fn access(&self, _id: VmId) -> Result<Option<GuestAccess>> {
        Ok(None)
    }
}

/// The slice of this machine the provider chose to rent out.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HostBudget {
    /// Physical core ids that may be pinned to guests.
    pub cores: Vec<u32>,
    pub mem_mib: u64,
    pub disk_gib: u64,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct VmRecord {
    pub id: VmId,
    pub spec: VmSpec,
    pub placement: Placement,
    pub state: VmState,
}

#[derive(Debug, Default)]
struct Inner {
    budget: Option<HostBudget>,
    free_cores: BTreeSet<u32>,
    free_mem_mib: u64,
    free_disk_gib: u64,
    vms: HashMap<VmId, VmRecord>,
}

/// Allocates host resources to VMs and drives the hypervisor.
#[derive(Debug, Clone)]
pub struct Provisioner {
    hv: Arc<dyn Hypervisor>,
    accel: Arc<dyn AcceleratorManager>,
    mem_enc: Arc<dyn MemoryEncryption>,
    events: EventBus,
    inner: Arc<Mutex<Inner>>,
}

impl Provisioner {
    pub fn new(
        hv: Arc<dyn Hypervisor>,
        accel: Arc<dyn AcceleratorManager>,
        mem_enc: Arc<dyn MemoryEncryption>,
        events: EventBus,
        budget: HostBudget,
    ) -> Self {
        let inner = Inner {
            free_cores: budget.cores.iter().copied().collect(),
            free_mem_mib: budget.mem_mib,
            free_disk_gib: budget.disk_gib,
            budget: Some(budget),
            vms: HashMap::new(),
        };
        Self { hv, accel, mem_enc, events, inner: Arc::new(Mutex::new(inner)) }
    }

    pub fn hypervisor(&self) -> &Arc<dyn Hypervisor> {
        &self.hv
    }

    /// Replace the rentable budget. Shrinking below what running VMs use is refused.
    pub async fn set_budget(&self, budget: HostBudget) -> Result<()> {
        let mut inner = self.inner.lock().await;
        let used_cores: BTreeSet<u32> =
            inner.vms.values().flat_map(|v| v.placement.pinned_cores.iter().copied()).collect();
        let used_mem: u64 = inner.vms.values().map(|v| v.placement.mem_mib).sum();
        let used_disk: u64 = inner.vms.values().map(|v| v.placement.disk_gib).sum();
        let new_cores: BTreeSet<u32> = budget.cores.iter().copied().collect();
        if !used_cores.is_subset(&new_cores) || budget.mem_mib < used_mem || budget.disk_gib < used_disk {
            return Err(Error::Capacity("new budget is smaller than resources in use".into()));
        }
        inner.free_cores = new_cores.difference(&used_cores).copied().collect();
        inner.free_mem_mib = budget.mem_mib - used_mem;
        inner.free_disk_gib = budget.disk_gib - used_disk;
        inner.budget = Some(budget);
        Ok(())
    }

    /// Reserve resources, create and boot a VM.
    pub async fn provision(&self, spec: VmSpec) -> Result<VmRecord> {
        validate_spec(&spec)?;
        if spec.confidential && !self.mem_enc.capabilities().guest_memory_encrypted {
            return Err(Error::Unsupported(format!(
                "confidential VM requested but host only offers {:?}",
                self.mem_enc.kind()
            )));
        }

        let id = VmId::new();
        let placement = {
            let mut inner = self.inner.lock().await;
            if inner.free_cores.len() < spec.vcpus as usize {
                return Err(Error::Capacity(format!(
                    "{} cores requested, {} free",
                    spec.vcpus,
                    inner.free_cores.len()
                )));
            }
            if inner.free_mem_mib < spec.mem_mib {
                return Err(Error::Capacity(format!("{} MiB requested, {} free", spec.mem_mib, inner.free_mem_mib)));
            }
            if inner.free_disk_gib < spec.disk_gib {
                return Err(Error::Capacity(format!("{} GiB requested, {} free", spec.disk_gib, inner.free_disk_gib)));
            }
            let accelerator = match &spec.accelerator {
                Some(req) => Some(self.accel.allocate(id, req)?),
                None => None,
            };
            let pinned_cores: Vec<u32> = inner.free_cores.iter().take(spec.vcpus as usize).copied().collect();
            for c in &pinned_cores {
                inner.free_cores.remove(c);
            }
            inner.free_mem_mib -= spec.mem_mib;
            inner.free_disk_gib -= spec.disk_gib;
            let placement = Placement { pinned_cores, mem_mib: spec.mem_mib, disk_gib: spec.disk_gib, accelerator };
            inner
                .vms
                .insert(id, VmRecord { id, spec: spec.clone(), placement: placement.clone(), state: VmState::Created });
            placement
        };

        let boot = async {
            self.mem_enc.prepare_guest(id, &placement).await?;
            self.hv.create(id, &spec, &placement).await?;
            self.hv.start(id).await
        };
        if let Err(e) = boot.await {
            self.release(id).await;
            return Err(e);
        }
        self.set_state(id, VmState::Running).await
    }

    pub async fn pause(&self, id: VmId) -> Result<VmRecord> {
        self.hv.pause(id).await?;
        self.set_state(id, VmState::Paused).await
    }

    pub async fn resume(&self, id: VmId) -> Result<VmRecord> {
        self.hv.resume(id).await?;
        self.set_state(id, VmState::Running).await
    }

    /// Pause and serialize a VM; it stays reserved in state `Hibernated`.
    pub async fn hibernate(&self, id: VmId) -> Result<VmSnapshot> {
        self.hv.pause(id).await?;
        let snap = self.hv.snapshot(id).await?;
        self.set_state(id, VmState::Hibernated).await?;
        Ok(snap)
    }

    pub async fn destroy(&self, id: VmId) -> Result<()> {
        self.hv.destroy(id).await?;
        self.release(id).await;
        self.events.publish(NodeEvent::VmState { vm: id, state: VmState::Stopped });
        Ok(())
    }

    pub async fn get(&self, id: VmId) -> Result<VmRecord> {
        self.inner.lock().await.vms.get(&id).cloned().ok_or_else(|| Error::NotFound(format!("vm {id}")))
    }

    pub async fn list(&self) -> Vec<VmRecord> {
        self.inner.lock().await.vms.values().cloned().collect()
    }

    /// (free cores, free MiB, free GiB)
    pub async fn free(&self) -> (usize, u64, u64) {
        let inner = self.inner.lock().await;
        (inner.free_cores.len(), inner.free_mem_mib, inner.free_disk_gib)
    }

    async fn set_state(&self, id: VmId, state: VmState) -> Result<VmRecord> {
        let rec = {
            let mut inner = self.inner.lock().await;
            let rec = inner.vms.get_mut(&id).ok_or_else(|| Error::NotFound(format!("vm {id}")))?;
            rec.state = state;
            rec.clone()
        };
        self.events.publish(NodeEvent::VmState { vm: id, state });
        Ok(rec)
    }

    async fn release(&self, id: VmId) {
        let mut inner = self.inner.lock().await;
        if let Some(rec) = inner.vms.remove(&id) {
            inner.free_cores.extend(rec.placement.pinned_cores);
            inner.free_mem_mib += rec.placement.mem_mib;
            inner.free_disk_gib += rec.placement.disk_gib;
            if rec.placement.accelerator.is_some() {
                self.accel.release(id);
            }
        }
    }
}

fn validate_spec(spec: &VmSpec) -> Result<()> {
    if spec.vcpus == 0 {
        return Err(Error::Invalid("vcpus must be > 0".into()));
    }
    if spec.mem_mib < 128 {
        return Err(Error::Invalid("mem_mib must be >= 128".into()));
    }
    if spec.disk_gib == 0 {
        return Err(Error::Invalid("disk_gib must be > 0".into()));
    }
    if let Some(req) = &spec.accelerator
        && req.kind == AcceleratorKind::None
    {
        return Err(Error::Invalid("accelerator kind must not be None".into()));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::accel::{AcceleratorDevice, PartitionedAccelerators};
    use super::confidential::NoEncryption;
    use super::mock::MockHypervisor;
    use super::*;

    fn provisioner() -> Provisioner {
        let accel = PartitionedAccelerators::new(vec![AcceleratorDevice {
            id: "gpu0".into(),
            kind: AcceleratorKind::Gpu,
            model: "Sim H100".into(),
            vram_mib: 81_920,
            slices: 7,
        }]);
        Provisioner::new(
            Arc::new(MockHypervisor::default()),
            Arc::new(accel),
            Arc::new(NoEncryption),
            EventBus::default(),
            HostBudget { cores: (0..8).collect(), mem_mib: 16_384, disk_gib: 200 },
        )
    }

    fn ubuntu(vcpus: u32) -> VmSpec {
        VmSpec {
            vcpus,
            mem_mib: 4096,
            disk_gib: 20,
            image: "ubuntu-24.04".into(),
            accelerator: None,
            confidential: false,
        }
    }

    #[tokio::test]
    async fn pins_distinct_cores_and_releases_them() {
        let p = provisioner();
        let a = p.provision(ubuntu(2)).await.expect("a");
        let b = p.provision(ubuntu(2)).await.expect("b");
        assert_eq!(a.state, VmState::Running);
        assert!(a.placement.pinned_cores.iter().all(|c| !b.placement.pinned_cores.contains(c)));
        assert_eq!(p.free().await, (4, 8192, 160));
        p.destroy(a.id).await.expect("destroy");
        assert_eq!(p.free().await, (6, 12_288, 180));
    }

    #[tokio::test]
    async fn refuses_overcommit() {
        let p = provisioner();
        let err = p.provision(ubuntu(9)).await.expect_err("too many cores");
        assert!(matches!(err, Error::Capacity(_)));
    }

    #[tokio::test]
    async fn confidential_requires_encrypting_host() {
        let p = provisioner();
        let mut spec = ubuntu(1);
        spec.confidential = true;
        assert!(matches!(p.provision(spec).await, Err(Error::Unsupported(_))));
    }

    #[tokio::test]
    async fn fractional_gpu_slices() {
        let p = provisioner();
        let mut spec = ubuntu(1);
        spec.accelerator = Some(AcceleratorRequest { kind: AcceleratorKind::Gpu, slices: 3, min_vram_mib: 0 });
        let vm = p.provision(spec.clone()).await.expect("first");
        let part = vm.placement.accelerator.expect("partition");
        assert_eq!(part.slices, 3);
        p.provision(spec.clone()).await.expect("second fits 6/7");
        assert!(p.provision(spec).await.is_err(), "third would need 9/7 slices");
    }
}
