//! Tauri shell around a PeerVPS node.
//!
//! The UI talks to Rust in two directions:
//! * **commands** ([`commands`]) — request/response via `invoke`, all async so
//!   they run on the Tokio runtime, never on the main (UI) thread;
//! * **events** ([`pump`]) — the node's event bus is coalesced and pushed to
//!   the webview as one `node://batch` event per frame (≤ 60 Hz).

mod commands;
mod demo;
mod images;
mod metrics;
mod pump;

use std::sync::Arc;

use peervps_core::Node;
use peervps_core::storage::presence::{self, PresenceEvent};
use peervps_core::storage::{Store, now_secs};
use peervps_core::virtualization::Hypervisor;
use peervps_core::virtualization::mock::MockHypervisor;
use tauri::Manager;
use tokio::sync::Mutex;

/// Shared state handed to every command.
#[derive(Debug)]
pub struct AppState {
    pub node: Node,
    pub renter: String,
    pub provider: String,
    pub allocation: Mutex<commands::HostAllocation>,
    pub demo: Arc<demo::DemoTopology>,
    pub images: images::ImageStore,
    /// Why guests are simulated instead of real VMs (QEMU missing), shown in the Host view.
    pub hypervisor_note: Option<String>,
}

/// Real VMs through QEMU when it is installed; the in-memory simulator otherwise.
fn pick_hypervisor(data_dir: &std::path::Path) -> (Arc<dyn Hypervisor>, Option<String>) {
    use peervps_core::virtualization::qemu::{QemuConfig, QemuHypervisor};
    match QemuConfig::detect(data_dir.join("images"), data_dir.join("vms")).and_then(QemuHypervisor::new) {
        Ok(hv) => {
            tracing::info!(binary = %hv.config().binary.display(), accel = ?hv.config().accel, "running guests with QEMU");
            (Arc::new(hv), None)
        }
        Err(e) => {
            tracing::warn!(error = %e, "QEMU unavailable; guests are simulated");
            (Arc::new(MockHypervisor::default()), Some(e.to_string()))
        }
    }
}

/// Accept other PeerVPS machines on the LAN (port 7071, or any free port when
/// it is taken) and keep the peers' offers fresh.
async fn start_peering(node: &Node, data_dir: &std::path::Path) -> peervps_core::Result<()> {
    use peervps_core::peer::{DEFAULT_PORT, Identity, Peers};
    let identity = Identity::load_or_create(&data_dir.join("node.key"))?;
    let peers = Peers::attach(node, identity, Some(data_dir.join("peers.json")))?;
    let any = std::net::Ipv4Addr::UNSPECIFIED;
    if let Err(e) = peers.listen((any, DEFAULT_PORT).into()).await {
        tracing::warn!(error = %e, "port {DEFAULT_PORT} is taken; accepting peers on a free port");
        peers.listen((any, 0).into()).await?;
    }
    let (beacons, targets) = peervps_core::peer::discovery::lan();
    if let Err(e) = peers.discover(beacons, targets).await {
        tracing::warn!(error = %e, "not announcing this machine on the LAN");
    }
    peers.spawn_refresh(std::time::Duration::from_secs(10));
    Ok(())
}

pub fn run() {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .init();

    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_opener::init())
        .setup(|app| {
            let data_dir = app.path().app_data_dir()?;
            std::fs::create_dir_all(&data_dir)?;
            let store = Store::open(data_dir.join("node.db"))?;
            let (hypervisor, hypervisor_note) = pick_hypervisor(&data_dir);
            let (node, _key) = tauri::async_runtime::block_on(Node::demo_with_hypervisor(store, hypervisor))?;
            let provider = node.config.node_id.clone();
            node.store.with(|c| presence::record(c, &provider, PresenceEvent::Online, now_secs()))?;
            // Start by offering at most half the machine; the provider widens it with the sliders.
            let host_cores = std::thread::available_parallelism().map(|n| n.get() as u32).unwrap_or(2);
            let allocation = commands::HostAllocation {
                max_cores: (host_cores / 2).max(1),
                max_mem_mib: (metrics::host_mem_mib() / 2).max(1024),
                max_disk_gib: 100,
                gpu_enabled: true,
                price_per_core_sec: 150,
            };
            tauri::async_runtime::block_on(node.provisioner.set_budget(allocation.budget()))?;

            let demo = Arc::new(demo::DemoTopology::new(node.clone()));
            tauri::async_runtime::spawn(demo.clone().run());
            tauri::async_runtime::spawn(metrics::run(node.clone(), provider.clone()));
            tauri::async_runtime::spawn(pump::run(app.handle().clone(), node.events.subscribe()));
            let n = node.clone();
            tauri::async_runtime::spawn(async move {
                let (tx, mut rx) = tokio::sync::mpsc::channel(16);
                tokio::spawn(n.meter.clone().run(tx));
                while let Some(id) = rx.recv().await {
                    tracing::warn!(%id, "renter out of credits; instance scaled to zero");
                    if let Err(e) = n.suspend_exhausted(&id).await {
                        tracing::error!(%id, error = %e, "could not stop an unpaid instance");
                    }
                }
            });

            tauri::async_runtime::block_on(commands::publish_local_offer(&node, &allocation));
            tauri::async_runtime::block_on(start_peering(&node, &data_dir))?;
            app.manage(AppState {
                node,
                renter: "demo-agent".into(),
                provider,
                allocation: Mutex::new(allocation),
                demo,
                images: images::ImageStore::new(data_dir.join("images")),
                hypervisor_note,
            });
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            commands::get_host_snapshot,
            commands::set_host_allocation,
            commands::list_offers,
            commands::deploy_instance,
            commands::list_instances,
            commands::scale_instance,
            commands::terminate_instance,
            commands::get_wallet,
            commands::top_up,
            commands::get_topology,
            commands::kill_peer,
            commands::restore_peer,
            commands::get_instance_access,
            commands::get_console,
            commands::send_console,
            commands::open_guest_screen,
            commands::get_peers,
            commands::add_peer,
            commands::approve_peer,
            commands::remove_peer,
            commands::set_internet,
            commands::set_relay,
            images::list_images,
            images::pull_image,
            images::import_image,
        ])
        .build(tauri::generate_context!())
        .unwrap_or_else(|e| {
            eprintln!("error while running PeerVPS: {e}");
            std::process::exit(1);
        })
        .run(|app, event| {
            // The process exits right after this without running destructors,
            // so stop the VMs (and their billing) now or they outlive the app.
            if let tauri::RunEvent::Exit = event
                && let Some(state) = app.try_state::<AppState>()
            {
                tauri::async_runtime::block_on(state.node.shutdown(std::time::Duration::from_secs(5)));
            }
        });
}
