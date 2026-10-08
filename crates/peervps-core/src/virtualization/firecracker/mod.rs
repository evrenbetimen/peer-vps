//! Firecracker backend: one `firecracker` process per MicroVM.
//!
//! Lifecycle, each step a call on the VM's API socket:
//!
//! | trait method | Firecracker API                                                    |
//! |--------------|--------------------------------------------------------------------|
//! | `create`     | spawn process, `PUT /machine-config`, `/boot-source`, `/drives`, `/network-interfaces` |
//! | `start`      | `PUT /actions {InstanceStart}`                                     |
//! | `pause`      | `PATCH /vm {Paused}`                                               |
//! | `resume`     | `PATCH /vm {Resumed}`                                              |
//! | `snapshot`   | `PUT /snapshot/create` (full), files packed into a [`VmSnapshot`]  |
//! | `restore`    | spawn process, `PUT /snapshot/load`                                |
//! | `destroy`    | kill process, delete the VM directory                              |
//!
//! The process is pinned to the placement's cores before boot, so the vCPU
//! threads Firecracker spawns inherit the affinity. Each VM gets a private copy
//! of its image as root disk under `<run_dir>/<vm>/`. The guest's serial
//! console is written to `<run_dir>/<vm>/console.log`.
//!
//! Production hardening still to come: launching through Firecracker's
//! `jailer` (chroot, cgroups, seccomp, dropped privileges) and reflink/overlay
//! disks instead of full copies.

pub mod api;

use std::collections::HashMap;
use std::fmt;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::json;
use tokio::process::{Child, Command};
use tokio::sync::Mutex;

use super::{Hypervisor, Placement, VmId, VmSnapshot, VmSpec};
use crate::{Error, Result};
use api::ApiClient;

const SNAPSHOT_MAGIC: &[u8; 8] = b"PVFCSNP1";
const DEFAULT_BOOT_ARGS: &str = "console=ttyS0 reboot=k panic=1 pci=off";

/// Host networking for guests: each VM gets a tap device enslaved to `bridge`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BridgeNetwork {
    pub bridge: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FirecrackerConfig {
    /// Path to the `firecracker` binary.
    pub binary: PathBuf,
    /// Uncompressed guest kernel (`vmlinux`).
    pub kernel: PathBuf,
    /// Directory of root filesystem images; `VmSpec::image` = `foo` resolves to `foo.ext4`.
    pub images_dir: PathBuf,
    /// Where per-VM sockets, disks, snapshots and console logs live.
    pub run_dir: PathBuf,
    pub boot_args: String,
    /// `None` boots guests without a NIC.
    pub network: Option<BridgeNetwork>,
}

impl FirecrackerConfig {
    pub fn new(binary: PathBuf, kernel: PathBuf, images_dir: PathBuf, run_dir: PathBuf) -> Self {
        Self { binary, kernel, images_dir, run_dir, boot_args: DEFAULT_BOOT_ARGS.into(), network: None }
    }

    fn image_path(&self, image: &str) -> Result<PathBuf> {
        // Image names come from renters: allow only a plain file stem.
        if image.is_empty() || !image.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.')) {
            return Err(Error::Invalid(format!("invalid image name {image:?}")));
        }
        if image.starts_with('.') {
            return Err(Error::Invalid(format!("invalid image name {image:?}")));
        }
        let path = self.images_dir.join(format!("{image}.ext4"));
        if !path.is_file() {
            return Err(Error::NotFound(format!("image {image} ({})", path.display())));
        }
        Ok(path)
    }
}

struct Instance {
    child: Child,
    api: ApiClient,
    dir: PathBuf,
}

pub struct FirecrackerHypervisor {
    cfg: FirecrackerConfig,
    vms: Mutex<HashMap<VmId, Instance>>,
}

impl fmt::Debug for FirecrackerHypervisor {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("FirecrackerHypervisor").field("cfg", &self.cfg).finish_non_exhaustive()
    }
}

impl FirecrackerHypervisor {
    /// Validate the configuration (binary, kernel, image dir) and prepare `run_dir`.
    pub fn new(cfg: FirecrackerConfig) -> Result<Self> {
        for (what, p) in [("firecracker binary", &cfg.binary), ("kernel", &cfg.kernel)] {
            if !p.is_file() {
                return Err(Error::NotFound(format!("{what} at {}", p.display())));
            }
        }
        if !cfg.images_dir.is_dir() {
            return Err(Error::NotFound(format!("image directory {}", cfg.images_dir.display())));
        }
        std::fs::create_dir_all(&cfg.run_dir)?;
        // Unix socket paths are limited to 108 bytes (sun_path), and every VM's API
        // socket lives at <run_dir>/<uuid>/api.sock. Fail here rather than on first boot.
        let longest = cfg.run_dir.join(VmId::new().to_string()).join("api.sock");
        if longest.as_os_str().len() >= 108 {
            return Err(Error::Invalid(format!(
                "run dir {} is too long for Firecracker API sockets (max ~60 bytes)",
                cfg.run_dir.display()
            )));
        }
        Ok(Self { cfg, vms: Mutex::new(HashMap::new()) })
    }

    pub fn console_log(&self, id: VmId) -> PathBuf {
        self.vm_dir(id).join("console.log")
    }

    fn vm_dir(&self, id: VmId) -> PathBuf {
        self.cfg.run_dir.join(id.to_string())
    }

    /// Spawn a fresh Firecracker process for `id`, pinned to `cores`, and wait for its socket.
    async fn spawn(&self, id: VmId, cores: &[u32]) -> Result<Instance> {
        let dir = self.vm_dir(id);
        tokio::fs::create_dir_all(&dir).await?;
        let socket = dir.join("api.sock");
        let _ = tokio::fs::remove_file(&socket).await;
        let console = std::fs::File::create(dir.join("console.log"))?;
        let child = Command::new(&self.cfg.binary)
            .arg("--api-sock")
            .arg(&socket)
            .arg("--id")
            .arg(id.0.simple().to_string())
            .stdin(Stdio::null())
            .stdout(console.try_clone()?)
            .stderr(console)
            .kill_on_drop(true)
            .spawn()
            .map_err(|e| Error::Hypervisor(format!("spawn {}: {e}", self.cfg.binary.display())))?;
        let mut inst = Instance { child, api: ApiClient::new(&socket), dir };
        if let Some(pid) = inst.child.id() {
            pin(pid, cores);
        }
        if let Err(e) = wait_for_socket(&mut inst, Duration::from_secs(5)).await {
            cleanup(inst, None).await;
            return Err(e);
        }
        Ok(inst)
    }

    async fn api(&self, id: VmId) -> Result<ApiClient> {
        self.vms.lock().await.get(&id).map(|i| i.api.clone()).ok_or_else(|| Error::NotFound(format!("vm {id}")))
    }

    async fn configure(&self, id: VmId, inst: &Instance, spec: &VmSpec, placement: &Placement) -> Result<()> {
        let api = &inst.api;
        api.put(
            "/machine-config",
            &json!({ "vcpu_count": spec.vcpus, "mem_size_mib": placement.mem_mib, "smt": false }),
        )
        .await?;
        api.put("/boot-source", &json!({ "kernel_image_path": self.cfg.kernel, "boot_args": self.cfg.boot_args }))
            .await?;

        let disk = inst.dir.join("rootfs.ext4");
        let image = self.cfg.image_path(&spec.image)?;
        tokio::fs::copy(&image, &disk).await?;
        // Grow the file to the rented size; the guest grows its filesystem on first boot.
        let want = placement.disk_gib << 30;
        let f = tokio::fs::OpenOptions::new().write(true).open(&disk).await?;
        if f.metadata().await?.len() < want {
            f.set_len(want).await?;
        }
        api.put(
            "/drives/rootfs",
            &json!({ "drive_id": "rootfs", "path_on_host": disk, "is_root_device": true, "is_read_only": false }),
        )
        .await?;

        if let Some(net) = &self.cfg.network {
            let tap = tap_name(id);
            create_tap(&tap, &net.bridge).await?;
            api.put(
                "/network-interfaces/eth0",
                &json!({ "iface_id": "eth0", "guest_mac": guest_mac(id), "host_dev_name": tap }),
            )
            .await?;
        }
        Ok(())
    }
}

#[async_trait]
impl Hypervisor for FirecrackerHypervisor {
    fn name(&self) -> &'static str {
        "firecracker"
    }

    async fn create(&self, id: VmId, spec: &VmSpec, placement: &Placement) -> Result<()> {
        if spec.accelerator.is_some() {
            // Firecracker has no PCI passthrough; accelerator guests need a QEMU/cloud-hypervisor backend.
            return Err(Error::Unsupported("firecracker backend cannot pass through GPUs/NPUs".into()));
        }
        let inst = self.spawn(id, &placement.pinned_cores).await?;
        if let Err(e) = self.configure(id, &inst, spec, placement).await {
            cleanup(inst, self.cfg.network.is_some().then(|| tap_name(id))).await;
            return Err(e);
        }
        self.vms.lock().await.insert(id, inst);
        Ok(())
    }

    async fn start(&self, id: VmId) -> Result<()> {
        self.api(id).await?.put("/actions", &json!({ "action_type": "InstanceStart" })).await
    }

    async fn pause(&self, id: VmId) -> Result<()> {
        self.api(id).await?.patch("/vm", &json!({ "state": "Paused" })).await
    }

    async fn resume(&self, id: VmId) -> Result<()> {
        self.api(id).await?.patch("/vm", &json!({ "state": "Resumed" })).await
    }

    async fn snapshot(&self, id: VmId) -> Result<VmSnapshot> {
        let (api, dir) = {
            let vms = self.vms.lock().await;
            let inst = vms.get(&id).ok_or_else(|| Error::NotFound(format!("vm {id}")))?;
            (inst.api.clone(), inst.dir.clone())
        };
        let (state, mem) = (dir.join("snapshot.state"), dir.join("snapshot.mem"));
        api.put("/snapshot/create", &json!({ "snapshot_type": "Full", "snapshot_path": state, "mem_file_path": mem }))
            .await?;
        // TODO(storage): stream these files instead of holding guest RAM in memory.
        let state_bytes = tokio::fs::read(&state).await?;
        let mem_bytes = tokio::fs::read(&mem).await?;
        Ok(VmSnapshot { vm: id, bytes: pack_snapshot(&state_bytes, &mem_bytes) })
    }

    async fn restore(&self, snapshot: VmSnapshot, placement: &Placement) -> Result<()> {
        let id = snapshot.vm;
        let (state_bytes, mem_bytes) = unpack_snapshot(&snapshot.bytes)?;
        let dir = self.vm_dir(id);
        tokio::fs::create_dir_all(&dir).await?;
        // The guest's root disk must already be at `dir/rootfs.ext4` (replicated or
        // left in place); the snapshot references it by path.
        if !dir.join("rootfs.ext4").is_file() {
            return Err(Error::NotFound(format!("root disk for vm {id} is not on this host")));
        }
        let (state, mem) = (dir.join("snapshot.state"), dir.join("snapshot.mem"));
        tokio::fs::write(&state, state_bytes).await?;
        tokio::fs::write(&mem, mem_bytes).await?;
        if let Some(old) = self.vms.lock().await.remove(&id) {
            let _ = kill(old).await;
        }
        let inst = self.spawn(id, &placement.pinned_cores).await?;
        let load = inst
            .api
            .put(
                "/snapshot/load",
                &json!({
                    "snapshot_path": state,
                    "mem_backend": { "backend_type": "File", "backend_path": mem },
                    "resume_vm": false,
                }),
            )
            .await;
        if let Err(e) = load {
            cleanup(inst, None).await;
            return Err(e);
        }
        self.vms.lock().await.insert(id, inst);
        Ok(())
    }

    async fn console_tail(&self, id: VmId, max_bytes: usize) -> Result<Option<String>> {
        if !self.vms.lock().await.contains_key(&id) {
            return Err(Error::NotFound(format!("vm {id}")));
        }
        let log = tokio::fs::read(self.console_log(id)).await.unwrap_or_default();
        let start = log.len().saturating_sub(max_bytes);
        Ok(Some(String::from_utf8_lossy(&log[start..]).into_owned()))
    }

    async fn destroy(&self, id: VmId) -> Result<()> {
        let inst = self.vms.lock().await.remove(&id);
        if let Some(inst) = inst {
            cleanup(inst, self.cfg.network.is_some().then(|| tap_name(id))).await;
        }
        Ok(())
    }
}

async fn wait_for_socket(inst: &mut Instance, timeout: Duration) -> Result<()> {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        if inst.api.socket().exists() && inst.api.get("/").await.is_ok() {
            return Ok(());
        }
        if let Some(status) = inst.child.try_wait()? {
            let log = tokio::fs::read_to_string(inst.dir.join("console.log")).await.unwrap_or_default();
            return Err(Error::Hypervisor(format!("firecracker exited early ({status}): {}", tail(&log, 400))));
        }
        if tokio::time::Instant::now() >= deadline {
            return Err(Error::Hypervisor("firecracker api socket did not come up".into()));
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

async fn kill(mut inst: Instance) -> Result<()> {
    let _ = inst.child.start_kill();
    let _ = tokio::time::timeout(Duration::from_secs(5), inst.child.wait()).await;
    Ok(())
}

async fn cleanup(inst: Instance, tap: Option<String>) {
    let dir = inst.dir.clone();
    let _ = kill(inst).await;
    if let Some(tap) = tap {
        let _ = run("ip", &["link", "del", &tap]).await;
    }
    let _ = tokio::fs::remove_dir_all(dir).await;
}

fn tail(s: &str, n: usize) -> &str {
    let start = s.len().saturating_sub(n);
    let start = (start..s.len()).find(|i| s.is_char_boundary(*i)).unwrap_or(s.len());
    &s[start..]
}

/// Pin the Firecracker process (and the vCPU threads it will spawn) to `cores`.
fn pin(pid: u32, cores: &[u32]) {
    if cores.is_empty() {
        return;
    }
    let mut set = nix::sched::CpuSet::new();
    for &c in cores {
        if let Err(e) = set.set(c as usize) {
            tracing::warn!(core = c, error = %e, "cannot pin to core");
            return;
        }
    }
    let pid = nix::unistd::Pid::from_raw(pid as i32);
    if let Err(e) = nix::sched::sched_setaffinity(pid, &set) {
        tracing::warn!(?cores, error = %e, "sched_setaffinity failed; vm runs unpinned");
    }
}

/// Linux interface names are limited to 15 bytes.
fn tap_name(id: VmId) -> String {
    format!("pv{}", &id.0.simple().to_string()[..12])
}

/// Locally administered unicast MAC derived from the VM id.
fn guest_mac(id: VmId) -> String {
    let b = id.0.as_bytes();
    format!("06:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}", b[0], b[1], b[2], b[3], b[4])
}

async fn create_tap(tap: &str, bridge: &str) -> Result<()> {
    run("ip", &["tuntap", "add", "dev", tap, "mode", "tap"]).await?;
    run("ip", &["link", "set", tap, "master", bridge]).await?;
    run("ip", &["link", "set", tap, "up"]).await
}

async fn run(cmd: &str, args: &[&str]) -> Result<()> {
    let out = Command::new(cmd).args(args).output().await?;
    if !out.status.success() {
        return Err(Error::Hypervisor(format!(
            "{cmd} {}: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        )));
    }
    Ok(())
}

fn pack_snapshot(state: &[u8], mem: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(8 + 8 + state.len() + mem.len());
    out.extend_from_slice(SNAPSHOT_MAGIC);
    out.extend_from_slice(&(state.len() as u64).to_be_bytes());
    out.extend_from_slice(state);
    out.extend_from_slice(mem);
    out
}

fn unpack_snapshot(bytes: &[u8]) -> Result<(&[u8], &[u8])> {
    let bad = || Error::Invalid("not a firecracker snapshot".into());
    if bytes.len() < 16 || &bytes[..8] != SNAPSHOT_MAGIC {
        return Err(bad());
    }
    let len = u64::from_be_bytes(bytes[8..16].try_into().map_err(|_| bad())?) as usize;
    let state_end = 16usize.checked_add(len).filter(|e| *e <= bytes.len()).ok_or_else(bad)?;
    Ok((&bytes[16..state_end], &bytes[state_end..]))
}

/// Locate the `firecracker` binary on `PATH`.
pub fn find_binary() -> Option<PathBuf> {
    std::env::var_os("PATH")?.to_str()?.split(':').map(|d| Path::new(d).join("firecracker")).find(|p| p.is_file())
}

#[cfg(test)]
mod tests;
