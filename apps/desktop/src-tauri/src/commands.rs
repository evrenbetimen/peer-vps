//! `invoke` handlers. Every command is `async`, so Tauri runs it on the async
//! runtime instead of the main thread, and returns a structured error the
//! TypeScript bridge can switch on.

use peervps_core::api::market::{Offer, OfferQuery};
use peervps_core::billing::CollateralState;
use peervps_core::billing::payments::{self, WebhookOutcome};
use peervps_core::node::{AccountSummary, DeployRequest, Instance};
use peervps_core::storage::now_secs;
use peervps_core::virtualization::accel::AcceleratorKind;
use peervps_core::virtualization::{HostBudget, VmRecord};
use peervps_core::{Error, Node};
use serde::{Deserialize, Serialize};
use tauri::State;

use crate::AppState;
use crate::demo::Topology;

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CmdError {
    code: &'static str,
    message: String,
}

impl From<Error> for CmdError {
    fn from(e: Error) -> Self {
        let code = match &e {
            Error::NotFound(_) => "not_found",
            Error::Capacity(_) => "insufficient_capacity",
            Error::InsufficientFunds { .. } => "insufficient_funds",
            Error::Invalid(_) => "invalid_argument",
            Error::Unauthorized(_) => "unauthorized",
            Error::Unsupported(_) => "unsupported",
            _ => "internal",
        };
        Self { code, message: e.to_string() }
    }
}

type CmdResult<T> = Result<T, CmdError>;

/// What the provider chose to rent out (Host Dashboard sliders).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HostAllocation {
    pub max_cores: u32,
    pub max_mem_mib: u64,
    pub max_disk_gib: u64,
    pub gpu_enabled: bool,
    /// Asking price, µcredits per vCPU-second.
    pub price_per_core_sec: i64,
}

impl HostAllocation {
    pub fn budget(&self) -> HostBudget {
        HostBudget { cores: (0..self.max_cores).collect(), mem_mib: self.max_mem_mib, disk_gib: self.max_disk_gib }
    }
}

pub const LOCAL_OFFER: &str = "this-machine";

pub async fn publish_local_offer(node: &Node, a: &HostAllocation) {
    let collateral = node.collateral.state(&node.config.node_id).await.map(|c| c.locked).unwrap_or(0);
    node.publish_offer(Offer {
        id: LOCAL_OFFER.into(),
        provider: node.config.node_id.clone(),
        region: "local".into(),
        vcpus: a.max_cores,
        mem_mib: a.max_mem_mib,
        disk_gib: a.max_disk_gib,
        accelerator: if a.gpu_enabled { AcceleratorKind::Gpu } else { AcceleratorKind::None },
        accelerator_model: a.gpu_enabled.then(|| "Simulated GPU (¼ slices)".to_owned()),
        vram_mib: if a.gpu_enabled { 24 * 1024 } else { 0 },
        price_per_sec: a.price_per_core_sec * i64::from(a.max_cores.max(1)),
        sla_pct: 100.0,
        confidential: false,
        collateral_locked: collateral,
    })
    .await;
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HostSnapshot {
    allocation: HostAllocation,
    host_cores: u32,
    host_mem_mib: u64,
    free_cores: usize,
    free_mem_mib: u64,
    free_disk_gib: u64,
    vms: Vec<VmRecord>,
    collateral: CollateralState,
    earnings: i64,
    hypervisor: &'static str,
}

#[tauri::command]
pub async fn get_host_snapshot(state: State<'_, AppState>) -> CmdResult<HostSnapshot> {
    let node = &state.node;
    let (free_cores, free_mem_mib, free_disk_gib) = node.provisioner.free().await;
    Ok(HostSnapshot {
        allocation: state.allocation.lock().await.clone(),
        host_cores: std::thread::available_parallelism().map(|n| n.get() as u32).unwrap_or(1),
        host_mem_mib: crate::metrics::host_mem_mib(),
        free_cores,
        free_mem_mib,
        free_disk_gib,
        vms: node.provisioner.list().await,
        collateral: node.collateral.state(&state.provider).await?,
        earnings: node.ledger.balance(&state.provider).await?,
        hypervisor: node.provisioner.hypervisor().name(),
    })
}

#[tauri::command]
pub async fn set_host_allocation(state: State<'_, AppState>, allocation: HostAllocation) -> CmdResult<HostAllocation> {
    if allocation.max_cores == 0 || allocation.max_mem_mib < 512 || allocation.max_disk_gib == 0 {
        return Err(Error::Invalid("allocate at least 1 core, 512 MiB and 1 GiB".into()).into());
    }
    state.node.provisioner.set_budget(allocation.budget()).await?;
    publish_local_offer(&state.node, &allocation).await;
    *state.allocation.lock().await = allocation.clone();
    Ok(allocation)
}

#[tauri::command]
pub async fn list_offers(state: State<'_, AppState>, query: OfferQuery) -> CmdResult<Vec<Offer>> {
    Ok(state.node.offers(&query).await)
}

#[tauri::command]
pub async fn deploy_instance(state: State<'_, AppState>, request: DeployRequest) -> CmdResult<Instance> {
    Ok(state.node.deploy(&state.renter, request).await?)
}

#[tauri::command]
pub async fn list_instances(state: State<'_, AppState>) -> CmdResult<Vec<Instance>> {
    Ok(state.node.instances(&state.renter).await)
}

#[tauri::command]
pub async fn scale_instance(state: State<'_, AppState>, id: String, replicas: u32) -> CmdResult<Instance> {
    Ok(state.node.scale(&state.renter, &id, replicas).await?)
}

#[tauri::command]
pub async fn terminate_instance(state: State<'_, AppState>, id: String) -> CmdResult<Instance> {
    Ok(state.node.terminate(&state.renter, &id).await?)
}

#[tauri::command]
pub async fn get_wallet(state: State<'_, AppState>) -> CmdResult<AccountSummary> {
    Ok(state.node.account(&state.renter).await?)
}

/// Simulates a card top-up end to end: builds the gateway's webhook, signs it
/// with the node's webhook secret and feeds it through the same verifier the
/// REST endpoint uses.
#[tauri::command]
pub async fn top_up(state: State<'_, AppState>, amount: i64) -> CmdResult<WebhookOutcome> {
    let body = serde_json::json!({
        "id": format!(
            "evt_desktop_{}",
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or_default()
        ),
        "account": state.renter,
        "amount": amount,
    })
    .to_string();
    let sig = state.node.webhooks.sign(body.as_bytes(), now_secs());
    Ok(payments::handle_top_up(&state.node.ledger, &state.node.webhooks, "desktop-sim", &sig, body.as_bytes()).await?)
}

#[tauri::command]
pub async fn get_topology(state: State<'_, AppState>) -> CmdResult<Topology> {
    Ok(state.demo.topology().await)
}

#[tauri::command]
pub async fn kill_peer(state: State<'_, AppState>, peer: String) -> CmdResult<Topology> {
    if !state.demo.set_alive(&peer, false).await {
        return Err(Error::NotFound(format!("peer {peer}")).into());
    }
    Ok(state.demo.topology().await)
}

#[tauri::command]
pub async fn restore_peer(state: State<'_, AppState>, peer: String) -> CmdResult<Topology> {
    if !state.demo.set_alive(&peer, true).await {
        return Err(Error::NotFound(format!("peer {peer}")).into());
    }
    Ok(state.demo.topology().await)
}
