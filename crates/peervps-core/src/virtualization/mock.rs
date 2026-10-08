//! In-memory hypervisor used by tests, CI and hosts without KVM.

use std::collections::HashMap;

use async_trait::async_trait;
use tokio::sync::Mutex;

use super::{Hypervisor, Placement, VmId, VmSnapshot, VmSpec, VmState};
use crate::{Error, Result};

#[derive(Debug, Default)]
pub struct MockHypervisor {
    vms: Mutex<HashMap<VmId, (VmSpec, VmState)>>,
}

impl MockHypervisor {
    pub async fn state(&self, id: VmId) -> Option<VmState> {
        self.vms.lock().await.get(&id).map(|(_, s)| *s)
    }

    async fn transition(&self, id: VmId, from: &[VmState], to: VmState) -> Result<()> {
        let mut vms = self.vms.lock().await;
        let (_, state) = vms.get_mut(&id).ok_or_else(|| Error::NotFound(format!("vm {id}")))?;
        if !from.contains(state) {
            return Err(Error::Hypervisor(format!("vm {id}: cannot go {state:?} -> {to:?}")));
        }
        *state = to;
        Ok(())
    }
}

#[async_trait]
impl Hypervisor for MockHypervisor {
    fn name(&self) -> &'static str {
        "mock"
    }

    async fn create(&self, id: VmId, spec: &VmSpec, _placement: &Placement) -> Result<()> {
        self.vms.lock().await.insert(id, (spec.clone(), VmState::Created));
        Ok(())
    }

    async fn start(&self, id: VmId) -> Result<()> {
        self.transition(id, &[VmState::Created, VmState::Stopped], VmState::Running).await
    }

    async fn pause(&self, id: VmId) -> Result<()> {
        self.transition(id, &[VmState::Running, VmState::Paused], VmState::Paused).await
    }

    async fn resume(&self, id: VmId) -> Result<()> {
        self.transition(id, &[VmState::Paused], VmState::Running).await
    }

    async fn snapshot(&self, id: VmId) -> Result<VmSnapshot> {
        let vms = self.vms.lock().await;
        let (spec, state) = vms.get(&id).ok_or_else(|| Error::NotFound(format!("vm {id}")))?;
        if *state != VmState::Paused {
            return Err(Error::Hypervisor("snapshot requires a paused vm".into()));
        }
        // Stand-in for RAM + device state: highly compressible, sized like a tiny guest.
        let mut bytes = serde_json::to_vec(spec)?;
        bytes.resize(bytes.len() + 256 * 1024, 0);
        Ok(VmSnapshot { vm: id, bytes })
    }

    async fn restore(&self, snapshot: VmSnapshot, _placement: &Placement) -> Result<()> {
        let end = snapshot.bytes.iter().position(|b| *b == 0).unwrap_or(snapshot.bytes.len());
        let spec: VmSpec = serde_json::from_slice(&snapshot.bytes[..end])?;
        self.vms.lock().await.insert(snapshot.vm, (spec, VmState::Paused));
        Ok(())
    }

    async fn destroy(&self, id: VmId) -> Result<()> {
        self.vms.lock().await.remove(&id);
        Ok(())
    }
}
