//! `peervps` — CLI for the PeerVPS agent API, plus `peervps serve` to run a headless node.
//!
//! Output is always JSON on stdout so agents can pipe it straight into a parser;
//! diagnostics go to stderr.

use std::net::SocketAddr;
use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use peervps_core::Node;
use peervps_core::storage::Store;
use serde_json::{Value, json};

#[derive(Debug, Parser)]
#[command(name = "peervps", version, about)]
struct Cli {
    /// Node API base URL.
    #[arg(long, env = "PEERVPS_API", default_value = "http://127.0.0.1:7070", global = true)]
    api: String,
    /// Bearer API key.
    #[arg(long, env = "PEERVPS_API_KEY", global = true, hide_env_values = true)]
    key: Option<String>,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Debug, Subcommand)]
enum Cmd {
    /// Run a headless node (mock hypervisor, demo marketplace) serving the REST API.
    Serve {
        #[arg(long, default_value = "127.0.0.1:7070")]
        listen: SocketAddr,
        /// SQLite database path; in-memory when omitted.
        #[arg(long)]
        db: Option<PathBuf>,
        #[command(flatten)]
        hv: HypervisorArgs,
    },
    /// List marketplace offers.
    Offers {
        #[arg(long)]
        min_vram_mib: Option<u64>,
        /// Max price in credits per hour.
        #[arg(long)]
        max_price_per_hour: Option<f64>,
        #[arg(long)]
        min_sla_pct: Option<f64>,
        /// gpu | npu | none
        #[arg(long)]
        accelerator: Option<String>,
        /// price | vram | sla
        #[arg(long, default_value = "price")]
        sort: String,
    },
    /// Deploy an instance on an offer.
    Deploy {
        offer: String,
        #[arg(long, default_value_t = 2)]
        vcpus: u32,
        #[arg(long, default_value_t = 4096)]
        mem_mib: u64,
        #[arg(long, default_value_t = 20)]
        disk_gib: u64,
        #[arg(long, default_value = "ubuntu-24.04")]
        image: String,
        #[arg(long)]
        confidential: bool,
    },
    /// Show one instance, or all when no id is given.
    Status { id: Option<String> },
    /// Tail of the instance's serial console.
    Console { id: String },
    /// Scale an instance to 0 (hibernate, stop billing) or 1.
    Scale { id: String, replicas: u32 },
    /// Terminate an instance.
    Terminate { id: String },
    /// Balance and recent ledger entries.
    Account,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
enum Backend {
    /// In-memory simulation; runs anywhere.
    Mock,
    /// Real MicroVMs via Firecracker (Linux + /dev/kvm). See scripts/fetch-firecracker-assets.sh.
    Firecracker,
}

#[derive(Debug, clap::Args)]
struct HypervisorArgs {
    #[arg(long, value_enum, default_value_t = Backend::Mock)]
    hypervisor: Backend,
    /// `firecracker` binary; looked up on PATH when omitted.
    #[arg(long)]
    fc_binary: Option<PathBuf>,
    /// Guest kernel (vmlinux).
    #[arg(long)]
    fc_kernel: Option<PathBuf>,
    /// Directory of `<image>.ext4` root filesystems.
    #[arg(long)]
    fc_images: Option<PathBuf>,
    /// Per-VM working directory (sockets, disks, snapshots, console logs).
    #[arg(long, default_value = "/var/lib/peervps/vms")]
    run_dir: PathBuf,
    /// Attach each guest's tap device to this bridge (guests get no NIC when omitted).
    #[arg(long)]
    bridge: Option<String>,
}

impl HypervisorArgs {
    fn build(self) -> Result<std::sync::Arc<dyn peervps_core::virtualization::Hypervisor>> {
        match self.hypervisor {
            Backend::Mock => Ok(std::sync::Arc::new(peervps_core::virtualization::mock::MockHypervisor::default())),
            Backend::Firecracker => firecracker(self),
        }
    }
}

#[cfg(target_os = "linux")]
fn firecracker(a: HypervisorArgs) -> Result<std::sync::Arc<dyn peervps_core::virtualization::Hypervisor>> {
    use peervps_core::virtualization::firecracker::{
        BridgeNetwork, FirecrackerConfig, FirecrackerHypervisor, find_binary,
    };
    let binary = a.fc_binary.or_else(find_binary).context("firecracker not found on PATH; pass --fc-binary")?;
    let kernel = a.fc_kernel.context("--fc-kernel is required with --hypervisor firecracker")?;
    let images = a.fc_images.context("--fc-images is required with --hypervisor firecracker")?;
    let mut cfg = FirecrackerConfig::new(binary, kernel, images, a.run_dir);
    cfg.network = a.bridge.map(|bridge| BridgeNetwork { bridge });
    Ok(std::sync::Arc::new(FirecrackerHypervisor::new(cfg)?))
}

#[cfg(not(target_os = "linux"))]
fn firecracker(_: HypervisorArgs) -> Result<std::sync::Arc<dyn peervps_core::virtualization::Hypervisor>> {
    bail!("the firecracker backend needs Linux with /dev/kvm")
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .init();
    let cli = Cli::parse();
    let client = Client { http: reqwest::Client::new(), base: cli.api.trim_end_matches('/').to_owned(), key: cli.key };

    let out = match cli.cmd {
        Cmd::Serve { listen, db, hv } => return serve(listen, db, hv).await,
        Cmd::Offers { min_vram_mib, max_price_per_hour, min_sla_pct, accelerator, sort } => {
            let mut q: Vec<(&str, String)> = vec![("sort", sort)];
            if let Some(v) = min_vram_mib {
                q.push(("minVramMib", v.to_string()));
            }
            if let Some(p) = max_price_per_hour {
                q.push(("maxPricePerHour", ((p * 1_000_000.0) as i64).to_string()));
            }
            if let Some(s) = min_sla_pct {
                q.push(("minSlaPct", s.to_string()));
            }
            if let Some(a) = accelerator {
                q.push(("accelerator", a));
            }
            client.get("/v1/offers", &q).await?
        }
        Cmd::Deploy { offer, vcpus, mem_mib, disk_gib, image, confidential } => {
            let body = json!({
                "offerId": offer,
                "spec": { "vcpus": vcpus, "memMib": mem_mib, "diskGib": disk_gib, "image": image, "confidential": confidential }
            });
            client.post("/v1/instances", body).await?
        }
        Cmd::Status { id: Some(id) } => client.get(&format!("/v1/instances/{id}"), &[]).await?,
        Cmd::Status { id: None } => client.get("/v1/instances", &[]).await?,
        Cmd::Scale { id, replicas } => {
            client.post(&format!("/v1/instances/{id}/scale"), json!({ "replicas": replicas })).await?
        }
        Cmd::Console { id } => {
            let out = client.get(&format!("/v1/instances/{id}/console"), &[]).await?;
            // Print the console verbatim rather than as a JSON string.
            match out.get("console").and_then(Value::as_str) {
                Some(text) => print!("{text}"),
                None => eprintln!("this node's hypervisor does not capture a console"),
            }
            return Ok(());
        }
        Cmd::Terminate { id } => client.delete(&format!("/v1/instances/{id}")).await?,
        Cmd::Account => client.get("/v1/account", &[]).await?,
    };
    println!("{}", serde_json::to_string_pretty(&out)?);
    Ok(())
}

async fn serve(listen: SocketAddr, db: Option<PathBuf>, hv: HypervisorArgs) -> Result<()> {
    let hypervisor = hv.build()?;
    eprintln!("hypervisor: {}", hypervisor.name());
    let store = match db {
        Some(path) => Store::open(&path).with_context(|| format!("open {}", path.display()))?,
        None => Store::in_memory()?,
    };
    let (node, key) = Node::demo_with_hypervisor(store, hypervisor).await?;
    eprintln!("demo renter API key: {key}");
    let (tx, mut rx) = tokio::sync::mpsc::channel(64);
    tokio::spawn(node.meter.clone().run(tx));
    tokio::spawn(async move {
        while let Some(billing_id) = rx.recv().await {
            tracing::warn!(%billing_id, "renter out of credits; instance suspended");
        }
    });
    tokio::select! {
        r = peervps_core::api::serve(node, listen) => r?,
        _ = tokio::signal::ctrl_c() => eprintln!("shutting down"),
    }
    Ok(())
}

#[derive(Debug)]
struct Client {
    http: reqwest::Client,
    base: String,
    key: Option<String>,
}

impl Client {
    fn req(&self, method: reqwest::Method, path: &str) -> reqwest::RequestBuilder {
        let r = self.http.request(method, format!("{}{path}", self.base));
        match &self.key {
            Some(k) => r.bearer_auth(k),
            None => r,
        }
    }

    async fn get(&self, path: &str, query: &[(&str, String)]) -> Result<Value> {
        Self::finish(self.req(reqwest::Method::GET, path).query(query)).await
    }

    async fn post(&self, path: &str, body: Value) -> Result<Value> {
        Self::finish(self.req(reqwest::Method::POST, path).json(&body)).await
    }

    async fn delete(&self, path: &str) -> Result<Value> {
        Self::finish(self.req(reqwest::Method::DELETE, path)).await
    }

    async fn finish(rb: reqwest::RequestBuilder) -> Result<Value> {
        let resp = rb.send().await.context("request failed (is `peervps serve` running?)")?;
        let status = resp.status();
        let body: Value = resp.json().await.unwrap_or(Value::Null);
        if !status.is_success() {
            bail!("{status}: {body}");
        }
        Ok(body)
    }
}
