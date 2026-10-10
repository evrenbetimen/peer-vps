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
        #[command(flatten)]
        peer: Box<PeerArgs>,
    },
    /// Run a relay: a machine anyone can reach that joins peers which cannot
    /// accept connections themselves (both behind CGNAT). It only sees ciphertext.
    Relay {
        #[arg(long, default_value = "0.0.0.0:7073")]
        listen: SocketAddr,
        /// Where the relay's key lives; defaults to the data dir.
        #[arg(long)]
        state_dir: Option<PathBuf>,
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
    /// Tail of the instance's serial console, or type into it with `--send`.
    Console {
        id: String,
        /// Text to type, followed by Enter (e.g. a login name, a password or a command).
        #[arg(long)]
        send: Option<String>,
    },
    /// How to reach an instance (QEMU backend): SSH, or for ISO installs the
    /// screen (VNC) and, for Windows, Remote Desktop.
    Access { id: String },
    /// Manage guest images (cloud disks and installer ISOs) for the QEMU backend.
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
    /// Rent from (and to) other PeerVPS nodes. The node must run with `--peer-listen`.
    Peer {
        #[command(subcommand)]
        cmd: PeerCmd,
    },
}

#[derive(Debug, Subcommand)]
enum PeerCmd {
    /// This node's peer id and invite, and every known peer.
    List,
    /// Add a peer by its invite (`pv-…@host:port`) or address (`host[:port]`).
    Add { address: String },
    /// Let a pending peer rent from this node.
    Approve { id: String },
    /// Forget a peer.
    Remove { id: String },
    /// Stay reachable through a relay (`host[:port]`), or `off`.
    Relay { address: String },
    /// Ask the router (UPnP) to forward a port so machines on other networks can add this one.
    Internet {
        #[arg(value_parser = ["on", "off"])]
        state: String,
    },
}

#[derive(Debug, clap::Args)]
struct PeerArgs {
    /// Accept other nodes on this address, e.g. 0.0.0.0:7071, and rent this machine to them.
    #[arg(long)]
    peer_listen: Option<SocketAddr>,
    /// Peer to add on start (`pv-…@host:port`); repeatable.
    #[arg(long = "peer")]
    peers: Vec<String>,
    /// Ask the router (UPnP) to forward a port so other networks can reach this node.
    #[arg(long)]
    upnp: bool,
    /// Stay reachable through this relay (`host[:port]`), for machines behind CGNAT.
    #[arg(long)]
    relay: Option<String>,
    /// Do not announce this node to, or look for, PeerVPS machines on the LAN.
    #[arg(long)]
    no_discovery: bool,
    /// Where the node key and the peer list live; defaults to the data dir.
    #[arg(long)]
    state_dir: Option<PathBuf>,
    /// vCPUs offered to peers; defaults to half of this machine's.
    #[arg(long)]
    offer_vcpus: Option<u32>,
    #[arg(long, default_value_t = 8192)]
    offer_mem_mib: u64,
    #[arg(long, default_value_t = 100)]
    offer_disk_gib: u64,
    /// Asking price, µcredits per vCPU-second.
    #[arg(long, default_value_t = 150)]
    price_per_core_sec: i64,
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
    /// Copy a qcow2 disk or an installer ISO (a Windows ISO too) into the image
    /// directory; deploy it with `--image <name>`.
    Import {
        file: PathBuf,
        /// Image name; defaults to the file name.
        #[arg(long)]
        name: Option<String>,
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
        ImageCmd::Import { file, name, dir } => {
            let dir = images_dir(dir);
            eprintln!("copying {} into {}", file.display(), dir.display());
            let image = images::import(&dir, &file, name.as_deref()).await?;
            Ok(json!({ "image": image, "deploy": format!("peervps deploy <offer> --image {}", image.name) }))
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
        Cmd::Serve { listen, db, hv, peer } => return serve(listen, db, hv, *peer).await,
        Cmd::Relay { listen, state_dir } => return relay(listen, state_dir).await,
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
        Cmd::Console { id, send: Some(text) } => {
            client.post(&format!("/v1/instances/{id}/console"), json!({ "input": format!("{text}\r") })).await?
        }
        Cmd::Console { id, send: None } => {
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
                    let access: peervps_core::virtualization::GuestAccess = serde_json::from_value(a.clone())?;
                    let mut out = json!({ "access": a });
                    if access.windows {
                        if let Some(rdp) = &access.rdp {
                            out["rdp"] = json!(format!("Remote Desktop to {rdp} as {}", access.user));
                        }
                    } else {
                        out["command"] = json!(access.ssh_command());
                    }
                    if let Some(display) = &access.display {
                        out["screen"] = json!(format!("open {display} (VNC)"));
                    }
                    out
                }
                _ => bail!("this node's hypervisor gives guests no SSH endpoint"),
            }
        }
        Cmd::Image { cmd } => image(cmd).await?,
        Cmd::Terminate { id } => client.delete(&format!("/v1/instances/{id}")).await?,
        Cmd::Account => client.get("/v1/account", &[]).await?,
        Cmd::Peer { cmd: PeerCmd::List } => client.get("/v1/peers", &[]).await?,
        Cmd::Peer { cmd: PeerCmd::Add { address } } => client.post("/v1/peers", json!({ "address": address })).await?,
        Cmd::Peer { cmd: PeerCmd::Approve { id } } => {
            client.post(&format!("/v1/peers/{id}/approve"), json!({})).await?
        }
        Cmd::Peer { cmd: PeerCmd::Remove { id } } => client.delete(&format!("/v1/peers/{id}")).await?,
        Cmd::Peer { cmd: PeerCmd::Relay { address } } => {
            let address = (address != "off").then_some(address);
            client.put("/v1/peers/relay", json!({ "address": address })).await?
        }
        Cmd::Peer { cmd: PeerCmd::Internet { state } } => {
            client.put("/v1/peers/internet", json!({ "enabled": state == "on" })).await?
        }
    };
    println!("{}", serde_json::to_string_pretty(&out)?);
    Ok(())
}

async fn serve(listen: SocketAddr, db: Option<PathBuf>, hv: HypervisorArgs, peer: PeerArgs) -> Result<()> {
    let hypervisor = hv.build()?;
    eprintln!("hypervisor: {}", hypervisor.name());
    match hypervisor.reap_orphans().await? {
        0 => {}
        n => eprintln!("stopped {n} guest(s) an earlier run left behind"),
    }
    let store = match db {
        Some(path) => Store::open(&path).with_context(|| format!("open {}", path.display()))?,
        None => Store::in_memory()?,
    };
    let (node, key) = Node::demo_with_hypervisor(store, hypervisor).await?;
    eprintln!("demo renter API key: {key}");
    if let Some(addr) = peer.peer_listen {
        start_peering(&node, addr, peer).await?;
    } else if !peer.peers.is_empty() {
        bail!("--peer needs --peer-listen");
    }
    let (tx, mut rx) = tokio::sync::mpsc::channel(64);
    tokio::spawn(node.meter.clone().run(tx));
    let n = node.clone();
    tokio::spawn(async move {
        while let Some(billing_id) = rx.recv().await {
            tracing::warn!(%billing_id, "renter out of credits; instance scaled to zero");
            if let Err(e) = n.suspend_exhausted(&billing_id).await {
                tracing::error!(%billing_id, error = %e, "could not stop an unpaid instance");
            }
        }
    });
    tokio::select! {
        r = peervps_core::api::serve(node.clone(), listen) => r?,
        _ = tokio::signal::ctrl_c() => eprintln!("shutting down"),
    }
    // Guests run as separate processes; stop them rather than leave them behind.
    node.shutdown().await;
    Ok(())
}

async fn relay(listen: SocketAddr, state_dir: Option<PathBuf>) -> Result<()> {
    use peervps_core::peer::{Identity, relay};
    let identity = Identity::load_or_create(&state_dir.unwrap_or_else(data_dir).join("relay.key"))?;
    let (bound, task) = relay::serve(listen, identity).await.with_context(|| format!("listen on {listen}"))?;
    eprintln!("relaying on {bound}; nodes use it with `peervps peer relay <this machine's address>:{}`", bound.port());
    tokio::select! {
        _ = task => {}
        _ = tokio::signal::ctrl_c() => eprintln!("shutting down"),
    }
    Ok(())
}

/// Rent a slice of this machine to approved peers and connect to the given ones.
async fn start_peering(node: &Node, addr: SocketAddr, a: PeerArgs) -> Result<()> {
    use peervps_core::api::market::Offer;
    use peervps_core::peer::{Identity, Peers};
    use peervps_core::virtualization::HostBudget;
    use peervps_core::virtualization::accel::AcceleratorKind;

    let dir = a.state_dir.unwrap_or_else(data_dir);
    let identity = Identity::load_or_create(&dir.join("node.key"))?;
    let peers = Peers::attach(node, identity, Some(dir.join("peers.json")))?;
    let bound = peers.listen(addr).await.with_context(|| format!("listen for peers on {addr}"))?;

    let cores = std::thread::available_parallelism().map(|n| n.get() as u32).unwrap_or(2);
    let vcpus = a.offer_vcpus.unwrap_or((cores / 2).max(1));
    node.provisioner
        .set_budget(HostBudget { cores: (0..vcpus).collect(), mem_mib: a.offer_mem_mib, disk_gib: a.offer_disk_gib })
        .await?;
    let collateral = node.collateral.state(&node.config.node_id).await.map(|c| c.locked).unwrap_or(0);
    node.publish_offer(Offer {
        id: "this-machine".into(),
        provider: node.config.node_id.clone(),
        region: "peer".into(),
        vcpus,
        mem_mib: a.offer_mem_mib,
        disk_gib: a.offer_disk_gib,
        accelerator: AcceleratorKind::None,
        accelerator_model: None,
        vram_mib: 0,
        price_per_sec: a.price_per_core_sec * i64::from(vcpus),
        sla_pct: 100.0,
        confidential: false,
        collateral_locked: collateral,
    })
    .await;

    if !a.no_discovery {
        let (beacons, targets) = peervps_core::peer::discovery::lan();
        if let Err(e) = peers.discover(beacons, targets).await {
            eprintln!("not announcing on the LAN: {e}");
        }
    }
    if a.upnp {
        peers.set_internet(true).await?;
    }
    if let Some(relay) = &a.relay {
        peers.set_relay(Some(relay)).await?;
    }
    for target in &a.peers {
        match peers.add(target).await {
            Ok(p) => eprintln!("peer {}: {:?}", p.id, p.status),
            Err(e) => eprintln!("peer {target}: {e}"),
        }
    }
    peers.spawn_refresh(std::time::Duration::from_secs(10));
    let invite = peers.overview().await.invite.unwrap_or_else(|| bound.to_string());
    eprintln!(
        "peer id {} accepting peers on {bound}; others add this node with: peervps peer add {invite}",
        peers.id()
    );
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

    async fn put(&self, path: &str, body: Value) -> Result<Value> {
        Self::finish(self.req(reqwest::Method::PUT, path).json(&body)).await
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
