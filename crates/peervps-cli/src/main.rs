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
    /// SSH endpoint, user and password for an instance (QEMU backend).
    Access { id: String },
    /// Manage guest disk images for the QEMU backend.
    Image {
        #[command(subcommand)]
        cmd: ImageCmd,
    },
    /// Scale an instance to 0 (hibernate, stop billing) or 1.
    Scale { id: String, replicas: u32 },
    /// Terminate an instance.
    Terminate { id: String },
    /// Balance and recent ledger entries.
    Account,
}

#[derive(Debug, Subcommand)]
enum ImageCmd {
    /// Download a cloud image (checksum-verified) into the image directory.
    Pull {
        /// ubuntu-24.04 | ubuntu-22.04 | debian-13
        name: String,
        #[arg(long)]
        dir: Option<PathBuf>,
    },
    /// List installed images and the downloadable catalog.
    List {
        #[arg(long)]
        dir: Option<PathBuf>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
enum Backend {
    /// In-memory simulation; runs anywhere.
    Mock,
    /// Real VMs on Linux, macOS or Windows via QEMU with the host's accelerator
    /// (KVM / Hypervisor.framework / WHPX). Images: `peervps image pull`.
    Qemu,
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
    /// QEMU image directory (`<name>.qcow2`); defaults to `<data dir>/images`.
    #[arg(long)]
    images: Option<PathBuf>,
    /// Public key file(s) authorized in QEMU guests, e.g. ~/.ssh/id_ed25519.pub.
    #[arg(long = "ssh-key")]
    ssh_keys: Vec<PathBuf>,
    /// Per-VM working directory (disks, snapshots, console logs); defaults to `<data dir>/vms`.
    #[arg(long)]
    run_dir: Option<PathBuf>,
    /// Attach each guest's tap device to this bridge (guests get no NIC when omitted).
    #[arg(long)]
    bridge: Option<String>,
}

impl HypervisorArgs {
    fn build(self) -> Result<std::sync::Arc<dyn peervps_core::virtualization::Hypervisor>> {
        match self.hypervisor {
            Backend::Mock => Ok(std::sync::Arc::new(peervps_core::virtualization::mock::MockHypervisor::default())),
            Backend::Qemu => qemu(self),
            Backend::Firecracker => firecracker(self),
        }
    }
}

/// Per-user data directory: `~/.local/share/peervps`, `~/Library/Application Support/PeerVPS`
/// or `%LOCALAPPDATA%\PeerVPS`.
fn data_dir() -> PathBuf {
    let home = || std::env::var_os("HOME").map(PathBuf::from).unwrap_or_else(std::env::temp_dir);
    if cfg!(windows) {
        std::env::var_os("LOCALAPPDATA").map(PathBuf::from).unwrap_or_else(std::env::temp_dir).join("PeerVPS")
    } else if cfg!(target_os = "macos") {
        home().join("Library/Application Support/PeerVPS")
    } else {
        std::env::var_os("XDG_DATA_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| home().join(".local/share"))
            .join("peervps")
    }
}

fn images_dir(dir: Option<PathBuf>) -> PathBuf {
    dir.unwrap_or_else(|| data_dir().join("images"))
}

fn qemu(a: HypervisorArgs) -> Result<std::sync::Arc<dyn peervps_core::virtualization::Hypervisor>> {
    use peervps_core::virtualization::qemu::{QemuConfig, QemuHypervisor};
    let mut cfg = QemuConfig::detect(images_dir(a.images), a.run_dir.unwrap_or_else(|| data_dir().join("vms")))?;
    for path in a.ssh_keys {
        let key = std::fs::read_to_string(&path).with_context(|| format!("read {}", path.display()))?;
        cfg.ssh_keys.extend(key.lines().map(str::trim).filter(|l| !l.is_empty()).map(str::to_owned));
    }
    eprintln!("images: {}", cfg.images_dir.display());
    Ok(std::sync::Arc::new(QemuHypervisor::new(cfg)?))
}

async fn image(cmd: ImageCmd) -> Result<Value> {
    use peervps_core::virtualization::images;
    match cmd {
        ImageCmd::Pull { name, dir } => {
            let dir = images_dir(dir);
            let mut last = 0u64;
            let path = images::pull(&reqwest::Client::new(), &dir, &name, |done, total| {
                // One progress line per ~32 MiB keeps stderr readable.
                if done - last >= 32 << 20 || Some(done) == total {
                    last = done;
                    match total {
                        Some(t) => eprintln!("{name}: {} / {} MiB", done >> 20, t >> 20),
                        None => eprintln!("{name}: {} MiB", done >> 20),
                    }
                }
            })
            .await?;
            Ok(json!({ "image": name, "path": path }))
        }
        ImageCmd::List { dir } => {
            let dir = images_dir(dir);
            Ok(json!({ "dir": dir, "installed": images::list(&dir)?, "catalog": images::CATALOG }))
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
    let mut cfg = FirecrackerConfig::new(binary, kernel, images, a.run_dir.unwrap_or_else(|| data_dir().join("vms")));
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
        Cmd::Access { id } => {
            let out = client.get(&format!("/v1/instances/{id}/access"), &[]).await?;
            match out.get("access") {
                Some(a) if !a.is_null() => {
                    let cmd = format!(
                        "ssh -p {} {}@{}",
                        a["sshPort"],
                        a["user"].as_str().unwrap_or_default(),
                        a["sshHost"].as_str().unwrap_or_default()
                    );
                    json!({ "access": a, "command": cmd })
                }
                _ => bail!("this node's hypervisor gives guests no SSH endpoint"),
            }
        }
        Cmd::Image { cmd } => image(cmd).await?,
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
