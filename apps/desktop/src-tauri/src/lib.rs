//! Tauri shell around a PeerVPS node.
//!
//! The UI talks to Rust in two directions:
//! * **commands** ([`commands`]) — request/response via `invoke`, all async so
//!   they run on the Tokio runtime, never on the main (UI) thread;
//! * **events** ([`pump`]) — the node's event bus is coalesced and pushed to
//!   the webview as one `node://batch` event per frame (≤ 60 Hz).

mod commands;
mod demo;
mod metrics;
mod pump;

use std::sync::Arc;

use peervps_core::Node;
use peervps_core::storage::presence::{self, PresenceEvent};
use peervps_core::storage::{Store, now_secs};
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
}

pub fn run() {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .init();

    tauri::Builder::default()
        .setup(|app| {
            let data_dir = app.path().app_data_dir()?;
            std::fs::create_dir_all(&data_dir)?;
            let store = Store::open(data_dir.join("node.db"))?;
            let (node, _key) = tauri::async_runtime::block_on(Node::demo_with(store))?;
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
            let meter = node.meter.clone();
            tauri::async_runtime::spawn(async move {
                let (tx, mut rx) = tokio::sync::mpsc::channel(16);
                tokio::spawn(meter.run(tx));
                while let Some(id) = rx.recv().await {
                    tracing::warn!(%id, "renter out of credits; billing suspended");
                }
            });

            tauri::async_runtime::block_on(commands::publish_local_offer(&node, &allocation));
            app.manage(AppState {
                node,
                renter: "demo-agent".into(),
                provider,
                allocation: Mutex::new(allocation),
                demo,
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
        ])
        .run(tauri::generate_context!())
        .unwrap_or_else(|e| {
            eprintln!("error while running PeerVPS: {e}");
            std::process::exit(1);
        });
}
