//! QEMU backend: one `qemu-system-*` process per guest, on Linux, macOS and Windows.
//!
//! Each host uses its native accelerator: KVM on Linux, Hypervisor.framework
//! on macOS, the Windows Hypervisor Platform on Windows, with TCG (software
//! emulation) when none is available. Guests boot stock cloud images (see
//! [`super::images`]) from a copy-on-write overlay and are provisioned by
//! cloud-init with a login user, a generated password and the node's SSH
//! keys. Networking is QEMU user mode with SSH forwarded to a loopback port,
//! so no root, bridge or tap device is needed on any OS.
//!
//! | trait method | QEMU                                                              |
//! |--------------|-------------------------------------------------------------------|
//! | `create`     | `qemu-img create` overlay, start the seed server, spawn `qemu -S`  |
//! | `start`      | QMP `cont`                                                        |
//! | `pause`      | QMP `stop`                                                        |
//! | `resume`     | QMP `cont`                                                        |
//! | `snapshot`   | QMP `migrate` to a file; state + disk overlay packed into a [`VmSnapshot`] |
//! | `restore`    | unpack, spawn `qemu -incoming defer`, QMP `migrate-incoming`        |
//! | `destroy`    | QMP `quit`, kill, delete the VM directory                         |

pub mod qmp;
pub mod seed;

use std::collections::HashMap;
use std::ffi::OsString;
use std::fmt;
use std::net::{Ipv4Addr, SocketAddr, TcpListener};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use rand::Rng;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::process::{Child, Command};
use tokio::sync::Mutex;
use tokio::task::AbortHandle;

use super::affinity::pin;
use super::{GuestAccess, Hypervisor, Placement, VmId, VmSnapshot, VmSpec, images};
use crate::{Error, Result};
use qmp::Qmp;

const SNAPSHOT_MAGIC: &[u8; 8] = b"PVQMSNP1";
const DEFAULT_USER: &str = "peervps";

/// Hardware acceleration QEMU runs guests with.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Accel {
    /// Linux `/dev/kvm`.
    Kvm,
    /// macOS Hypervisor.framework.
    Hvf,
    /// Windows Hypervisor Platform.
    Whpx,
    /// Software emulation; works anywhere, many times slower.
    Tcg,
}

impl Accel {
    /// The best accelerator this host offers.
    pub fn detect() -> Self {
        if cfg!(target_os = "linux") {
            let kvm = std::fs::OpenOptions::new().read(true).write(true).open("/dev/kvm").is_ok();
            if kvm { Self::Kvm } else { Self::Tcg }
        } else if cfg!(target_os = "macos") {
            Self::Hvf
        } else if cfg!(target_os = "windows") {
            // Needs the "Windows Hypervisor Platform" optional feature; QEMU reports it clearly if missing.
            Self::Whpx
        } else {
            Self::Tcg
        }
    }

    fn args(self) -> (&'static str, &'static str) {
        match self {
            Self::Kvm => ("kvm", "host"),
            Self::Hvf => ("hvf", "host"),
            // WHPX cannot expose `-cpu host`; kernel-irqchip=off is required for most guests.
            Self::Whpx => ("whpx,kernel-irqchip=off", "max"),
            Self::Tcg => ("tcg,thread=multi", "max"),
        }
    }
}

/// Guest CPU architecture, which follows the host's (no cross-arch emulation).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Arch {
    X86_64,
    Aarch64,
}

impl Arch {
    pub fn host() -> Result<Self> {
        match std::env::consts::ARCH {
            "x86_64" => Ok(Self::X86_64),
            "aarch64" => Ok(Self::Aarch64),
            other => Err(Error::Unsupported(format!("no QEMU guests for {other} hosts"))),
        }
    }

    fn binary_name(self) -> &'static str {
        match self {
            Self::X86_64 => "qemu-system-x86_64",
            Self::Aarch64 => "qemu-system-aarch64",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct QemuConfig {
    pub binary: PathBuf,
    pub img_binary: PathBuf,
    pub arch: Arch,
    pub accel: Accel,
    /// UEFI firmware; required for aarch64 guests (`edk2-aarch64-code.fd`).
    pub firmware: Option<PathBuf>,
    /// Directory of `<image>.qcow2` base disks.
    pub images_dir: PathBuf,
    /// Per-VM disks, logs and snapshots live in `<run_dir>/<vm>/`.
    pub run_dir: PathBuf,
    /// Login user created in every guest.
    pub user: String,
    /// Public keys authorized for `user`.
    pub ssh_keys: Vec<String>,
}

impl QemuConfig {
    /// Find QEMU on this machine (PATH plus the usual install locations) and pick the accelerator.
    pub fn detect(images_dir: PathBuf, run_dir: PathBuf) -> Result<Self> {
        let arch = Arch::host()?;
        let binary = find_tool(arch.binary_name()).ok_or_else(|| {
            Error::NotFound(format!("{} not found; install QEMU (see README, \"Local VMs\")", arch.binary_name()))
        })?;
        let img_binary = binary
            .parent()
            .map(|d| d.join(exe("qemu-img")))
            .filter(|p| p.is_file())
            .or_else(|| find_tool("qemu-img"))
            .ok_or_else(|| Error::NotFound("qemu-img not found next to QEMU or on PATH".into()))?;
        let firmware = match arch {
            Arch::X86_64 => None,
            Arch::Aarch64 => Some(find_firmware(&binary).ok_or_else(|| {
                Error::NotFound("edk2-aarch64-code.fd (UEFI firmware) not found in QEMU's share directory".into())
            })?),
        };
        Ok(Self {
            binary,
            img_binary,
            arch,
            accel: Accel::detect(),
            firmware,
            images_dir,
            run_dir,
            user: DEFAULT_USER.into(),
            ssh_keys: Vec::new(),
        })
    }
}

/// Everything needed to (re)launch a guest's QEMU process; also the snapshot header.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Launch {
    vcpus: u32,
    mem_mib: u64,
    image: String,
    ssh_port: u16,
    password: String,
}

struct Vm {
    child: Child,
    qmp: Arc<Qmp>,
    dir: PathBuf,
    launch: Launch,
    seed: Option<AbortHandle>,
}

pub struct QemuHypervisor {
    cfg: QemuConfig,
    vms: Mutex<HashMap<VmId, Vm>>,
}

impl fmt::Debug for QemuHypervisor {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("QemuHypervisor").field("cfg", &self.cfg).finish_non_exhaustive()
    }
}

impl QemuHypervisor {
    pub fn new(cfg: QemuConfig) -> Result<Self> {
        for (what, p) in [("QEMU", &cfg.binary), ("qemu-img", &cfg.img_binary)] {
            if !p.is_file() {
                return Err(Error::NotFound(format!("{what} at {}", p.display())));
            }
        }
        std::fs::create_dir_all(&cfg.images_dir)?;
        std::fs::create_dir_all(&cfg.run_dir)?;
        Ok(Self { cfg, vms: Mutex::new(HashMap::new()) })
    }

    pub fn config(&self) -> &QemuConfig {
        &self.cfg
    }

    fn vm_dir(&self, id: VmId) -> PathBuf {
        self.cfg.run_dir.join(id.to_string())
    }

    /// Launch QEMU for the VM in `dir` (paused, `-S`), pinned to `cores`, and wait for QMP.
    async fn spawn(&self, dir: &Path, launch: &Launch, cores: &[u32], extra: Extra) -> Result<Vm> {
        let qmp_port = free_port()?;
        let args = command_line(&self.cfg, dir, launch, qmp_port, &extra);
        let log = std::fs::File::create(dir.join("qemu.log"))?;
        let child = Command::new(&self.cfg.binary)
            .args(&args)
            .stdin(Stdio::null())
            .stdout(log.try_clone()?)
            .stderr(log)
            .kill_on_drop(true)
            .spawn()
            .map_err(|e| Error::Hypervisor(format!("spawn {}: {e}", self.cfg.binary.display())))?;
        let mut vm = Vm {
            child,
            qmp: Arc::new(Qmp::new(SocketAddr::from((Ipv4Addr::LOCALHOST, qmp_port)))),
            dir: dir.to_owned(),
            launch: launch.clone(),
            seed: None,
        };
        if let Some(pid) = vm.child.id() {
            pin(pid, cores);
        }
        if let Err(e) = wait_for_qmp(&mut vm, Duration::from_secs(20)).await {
            kill(&mut vm).await;
            return Err(e);
        }
        Ok(vm)
    }

    async fn with_vm<T>(&self, id: VmId, f: impl FnOnce(&Vm) -> T) -> Result<T> {
        self.vms.lock().await.get(&id).map(f).ok_or_else(|| Error::NotFound(format!("vm {id}")))
    }

    async fn qmp(&self, id: VmId, command: &str, args: Option<Value>) -> Result<Value> {
        let qmp = self.with_vm(id, |vm| vm.qmp.clone()).await?;
        qmp.execute(command, args).await
    }

    async fn qemu_img(&self, args: &[&std::ffi::OsStr]) -> Result<Vec<u8>> {
        let out = Command::new(&self.cfg.img_binary).args(args).output().await?;
        if !out.status.success() {
            return Err(Error::Hypervisor(format!("qemu-img: {}", String::from_utf8_lossy(&out.stderr).trim())));
        }
        Ok(out.stdout)
    }

    /// Copy-on-write overlay over `base`, at least `gib` large.
    async fn create_overlay(&self, base: &Path, disk: &Path, gib: u64) -> Result<()> {
        let info = self.qemu_img(&["info".as_ref(), "--output=json".as_ref(), base.as_os_str()]).await?;
        let base_size = serde_json::from_slice::<Value>(&info)?["virtual-size"].as_u64().unwrap_or(0);
        let size = (gib << 30).max(base_size).to_string();
        self.qemu_img(&[
            "create".as_ref(),
            "-f".as_ref(),
            "qcow2".as_ref(),
            "-F".as_ref(),
            "qcow2".as_ref(),
            "-b".as_ref(),
            base.as_os_str(),
            disk.as_os_str(),
            size.as_ref(),
        ])
        .await?;
        Ok(())
    }

    async fn wait_migration(&self, id: VmId) -> Result<()> {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(600);
        loop {
            let status = self.qmp(id, "query-migrate", None).await?;
            match status["status"].as_str() {
                Some("completed") => return Ok(()),
                Some("failed" | "cancelled") => {
                    let why = status["error-desc"].as_str().unwrap_or("unknown");
                    return Err(Error::Hypervisor(format!("migration failed: {why}")));
                }
                _ if tokio::time::Instant::now() >= deadline => {
                    return Err(Error::Hypervisor("migration timed out".into()));
                }
                _ => tokio::time::sleep(Duration::from_millis(50)).await,
            }
        }
    }

    async fn wait_incoming(&self, id: VmId) -> Result<()> {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(600);
        loop {
            let status = self.qmp(id, "query-status", None).await?;
            match status["status"].as_str() {
                Some("inmigrate") if tokio::time::Instant::now() < deadline => {
                    tokio::time::sleep(Duration::from_millis(50)).await;
                }
                Some("inmigrate") => return Err(Error::Hypervisor("restore timed out".into())),
                Some("paused" | "prelaunch" | "running") => return Ok(()),
                other => return Err(Error::Hypervisor(format!("restore ended in state {other:?}"))),
            }
        }
    }
}

#[async_trait]
impl Hypervisor for QemuHypervisor {
    fn name(&self) -> &'static str {
        match self.cfg.accel {
            Accel::Kvm => "qemu (kvm)",
            Accel::Hvf => "qemu (hvf)",
            Accel::Whpx => "qemu (whpx)",
            Accel::Tcg => "qemu (tcg, no acceleration)",
        }
    }

    async fn create(&self, id: VmId, spec: &VmSpec, placement: &Placement) -> Result<()> {
        if spec.accelerator.is_some() {
            return Err(Error::Unsupported("GPU/NPU passthrough is not wired into the QEMU backend yet".into()));
        }
        let base = images::resolve(&self.cfg.images_dir, &spec.image)?;
        let dir = self.vm_dir(id);
        tokio::fs::create_dir_all(&dir).await?;
        let setup = async {
            self.create_overlay(&base, &dir.join("disk.qcow2"), placement.disk_gib).await?;
            let launch = Launch {
                vcpus: spec.vcpus,
                mem_mib: placement.mem_mib,
                image: spec.image.clone(),
                ssh_port: free_port()?,
                password: password(),
            };
            let (url, seed) = seed::serve(seed::Seed {
                instance_id: id.to_string(),
                hostname: format!("pv-{}", &id.0.simple().to_string()[..8]),
                user: self.cfg.user.clone(),
                password: launch.password.clone(),
                ssh_keys: self.cfg.ssh_keys.clone(),
            })
            .await?;
            match self
                .spawn(&dir, &launch, &placement.pinned_cores, Extra { seed_url: Some(url), incoming: false })
                .await
            {
                Ok(mut vm) => {
                    vm.seed = Some(seed);
                    Ok(vm)
                }
                Err(e) => {
                    seed.abort();
                    Err(e)
                }
            }
        };
        match setup.await {
            Ok(vm) => {
                self.vms.lock().await.insert(id, vm);
                Ok(())
            }
            Err(e) => {
                let _ = tokio::fs::remove_dir_all(&dir).await;
                Err(e)
            }
        }
    }

    async fn start(&self, id: VmId) -> Result<()> {
        self.qmp(id, "cont", None).await.map(drop)
    }

    async fn pause(&self, id: VmId) -> Result<()> {
        self.qmp(id, "stop", None).await.map(drop)
    }

    async fn resume(&self, id: VmId) -> Result<()> {
        self.qmp(id, "cont", None).await.map(drop)
    }

    async fn snapshot(&self, id: VmId) -> Result<VmSnapshot> {
        let (dir, launch) = self.with_vm(id, |vm| (vm.dir.clone(), vm.launch.clone())).await?;
        let state = dir.join("state.bin");
        self.qmp(id, "migrate", Some(json!({ "uri": format!("file:{}", state.display()) }))).await?;
        self.wait_migration(id).await?;
        // TODO(storage): stream these files instead of holding guest RAM in memory.
        let state_bytes = tokio::fs::read(&state).await?;
        let disk = tokio::fs::read(dir.join("disk.qcow2")).await?;
        let _ = tokio::fs::remove_file(&state).await;
        Ok(VmSnapshot { vm: id, bytes: pack_snapshot(&launch, &state_bytes, &disk)? })
    }

    async fn restore(&self, snapshot: VmSnapshot, placement: &Placement) -> Result<()> {
        let id = snapshot.vm;
        let (mut launch, state_bytes, disk) = unpack_snapshot(&snapshot.bytes)?;
        let base = images::resolve(&self.cfg.images_dir, &launch.image)?;
        if let Some(mut old) = self.vms.lock().await.remove(&id) {
            kill(&mut old).await;
        }
        let dir = self.vm_dir(id);
        tokio::fs::create_dir_all(&dir).await?;
        let state = dir.join("state.bin");
        let disk_path = dir.join("disk.qcow2");
        tokio::fs::write(&state, state_bytes).await?;
        tokio::fs::write(&disk_path, disk).await?;
        // The overlay names its base by absolute path; point it at this host's copy.
        self.qemu_img(&[
            "rebase".as_ref(),
            "-u".as_ref(),
            "-F".as_ref(),
            "qcow2".as_ref(),
            "-b".as_ref(),
            base.as_os_str(),
            disk_path.as_os_str(),
        ])
        .await?;
        launch.ssh_port = free_port()?;
        let vm = self.spawn(&dir, &launch, &placement.pinned_cores, Extra { seed_url: None, incoming: true }).await?;
        self.vms.lock().await.insert(id, vm);
        let incoming = async {
            self.qmp(id, "migrate-incoming", Some(json!({ "uri": format!("file:{}", state.display()) }))).await?;
            self.wait_incoming(id).await
        };
        let result = incoming.await;
        let _ = tokio::fs::remove_file(&state).await;
        if let Err(e) = result {
            if let Some(mut vm) = self.vms.lock().await.remove(&id) {
                kill(&mut vm).await;
            }
            return Err(e);
        }
        Ok(())
    }

    async fn destroy(&self, id: VmId) -> Result<()> {
        let vm = self.vms.lock().await.remove(&id);
        if let Some(mut vm) = vm {
            let _ = vm.qmp.execute("quit", None).await;
            kill(&mut vm).await;
            let _ = tokio::fs::remove_dir_all(&vm.dir).await;
        }
        Ok(())
    }

    async fn console_tail(&self, id: VmId, max_bytes: usize) -> Result<Option<String>> {
        let dir = self.with_vm(id, |vm| vm.dir.clone()).await?;
        let log = tokio::fs::read(dir.join("console.log")).await.unwrap_or_default();
        let start = log.len().saturating_sub(max_bytes);
        Ok(Some(String::from_utf8_lossy(&log[start..]).into_owned()))
    }

    async fn access(&self, id: VmId) -> Result<Option<GuestAccess>> {
        let launch = self.with_vm(id, |vm| vm.launch.clone()).await?;
        Ok(Some(GuestAccess {
            ssh_host: "127.0.0.1".into(),
            ssh_port: launch.ssh_port,
            user: self.cfg.user.clone(),
            password: Some(launch.password),
        }))
    }
}

struct Extra {
    seed_url: Option<String>,
    incoming: bool,
}

/// QEMU's option parser splits on commas; a literal comma in a value is written `,,`.
fn esc(p: &Path) -> String {
    p.display().to_string().replace(',', ",,")
}

fn command_line(cfg: &QemuConfig, dir: &Path, launch: &Launch, qmp_port: u16, extra: &Extra) -> Vec<OsString> {
    let (accel, cpu) = cfg.accel.args();
    let machine = match cfg.arch {
        Arch::X86_64 => "q35",
        Arch::Aarch64 => "virt",
    };
    let mut a: Vec<String> = vec![
        "-nodefaults".into(),
        "-display".into(),
        "none".into(),
        "-machine".into(),
        machine.into(),
        "-accel".into(),
        accel.into(),
        "-cpu".into(),
        cpu.into(),
        "-smp".into(),
        launch.vcpus.to_string(),
        "-m".into(),
        format!("{}M", launch.mem_mib),
        "-serial".into(),
        format!("file:{}", esc(&dir.join("console.log"))),
        "-qmp".into(),
        format!("tcp:127.0.0.1:{qmp_port},server=on,wait=off"),
        "-drive".into(),
        format!("if=virtio,file={},format=qcow2,discard=unmap", esc(&dir.join("disk.qcow2"))),
        "-netdev".into(),
        format!("user,id=n0,hostfwd=tcp:127.0.0.1:{}-:22", launch.ssh_port),
        "-device".into(),
        "virtio-net-pci,netdev=n0".into(),
        // Cloud images generate SSH host keys on first boot; give them entropy.
        "-device".into(),
        "virtio-rng-pci".into(),
        "-S".into(),
    ];
    if let Some(fw) = &cfg.firmware {
        a.extend(["-bios".into(), esc(fw)]);
    }
    if let Some(url) = &extra.seed_url {
        a.extend(["-smbios".into(), format!("type=1,serial=ds=nocloud;s={url}")]);
    }
    if extra.incoming {
        a.extend(["-incoming".into(), "defer".into()]);
    }
    a.into_iter().map(OsString::from).collect()
}

async fn wait_for_qmp(vm: &mut Vm, timeout: Duration) -> Result<()> {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        if vm.qmp.execute("query-status", None).await.is_ok() {
            return Ok(());
        }
        if let Some(status) = vm.child.try_wait()? {
            let log = tokio::fs::read_to_string(vm.dir.join("qemu.log")).await.unwrap_or_default();
            return Err(Error::Hypervisor(format!("qemu exited ({status}): {}", log.trim())));
        }
        if tokio::time::Instant::now() >= deadline {
            return Err(Error::Hypervisor("qemu monitor did not come up".into()));
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

async fn kill(vm: &mut Vm) {
    if let Some(seed) = vm.seed.take() {
        seed.abort();
    }
    let _ = vm.child.start_kill();
    let _ = tokio::time::timeout(Duration::from_secs(5), vm.child.wait()).await;
}

/// A loopback port free right now (QEMU binds it a moment later).
fn free_port() -> Result<u16> {
    Ok(TcpListener::bind((Ipv4Addr::LOCALHOST, 0))?.local_addr()?.port())
}

fn password() -> String {
    const ALPHABET: &[u8] = b"abcdefghjkmnpqrstuvwxyzABCDEFGHJKLMNPQRSTUVWXYZ23456789";
    let mut rng = rand::rng();
    (0..16).map(|_| ALPHABET[rng.random_range(0..ALPHABET.len())] as char).collect()
}

fn exe(name: &str) -> String {
    if cfg!(windows) { format!("{name}.exe") } else { name.to_owned() }
}

/// Look for `name` on PATH, then where the QEMU installers for each OS put it.
pub fn find_tool(name: &str) -> Option<PathBuf> {
    let file = exe(name);
    let mut dirs: Vec<PathBuf> =
        std::env::var_os("PATH").map(|p| std::env::split_paths(&p).collect()).unwrap_or_default();
    dirs.extend(
        ["/opt/homebrew/bin", "/usr/local/bin", "/usr/bin", r"C:\Program Files\qemu", r"C:\msys64\ucrt64\bin"]
            .map(PathBuf::from),
    );
    dirs.into_iter().map(|d| d.join(&file)).find(|p| p.is_file())
}

/// `<prefix>/share/qemu/edk2-aarch64-code.fd`, next to `<prefix>/bin/qemu-system-aarch64`.
fn find_firmware(binary: &Path) -> Option<PathBuf> {
    let bin = binary.parent()?;
    [bin.join("../share/qemu"), bin.join("share"), PathBuf::from("/usr/share/qemu"), PathBuf::from("/usr/share/AAVMF")]
        .into_iter()
        .flat_map(|d| [d.join("edk2-aarch64-code.fd"), d.join("AAVMF_CODE.fd")])
        .find(|p| p.is_file())
}

fn pack_snapshot(launch: &Launch, state: &[u8], disk: &[u8]) -> Result<Vec<u8>> {
    let header = serde_json::to_vec(launch)?;
    let mut out = Vec::with_capacity(24 + header.len() + state.len() + disk.len());
    out.extend_from_slice(SNAPSHOT_MAGIC);
    out.extend_from_slice(&(header.len() as u64).to_be_bytes());
    out.extend_from_slice(&header);
    out.extend_from_slice(&(state.len() as u64).to_be_bytes());
    out.extend_from_slice(state);
    out.extend_from_slice(disk);
    Ok(out)
}

fn unpack_snapshot(bytes: &[u8]) -> Result<(Launch, &[u8], &[u8])> {
    let bad = || Error::Invalid("not a qemu snapshot".into());
    if bytes.len() < 16 || &bytes[..8] != SNAPSHOT_MAGIC {
        return Err(bad());
    }
    let mut at = 8usize;
    let chunk = |at: &mut usize| -> Result<&[u8]> {
        let len_end = at.checked_add(8).filter(|e| *e <= bytes.len()).ok_or_else(bad)?;
        let len = u64::from_be_bytes(bytes[*at..len_end].try_into().map_err(|_| bad())?) as usize;
        let end = len_end.checked_add(len).filter(|e| *e <= bytes.len()).ok_or_else(bad)?;
        *at = end;
        Ok(&bytes[len_end..end])
    };
    let header = chunk(&mut at)?;
    let state = chunk(&mut at)?;
    let launch = serde_json::from_slice(header).map_err(|_| bad())?;
    Ok((launch, state, &bytes[at..]))
}

#[cfg(test)]
mod tests;
