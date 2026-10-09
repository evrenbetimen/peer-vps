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
//! An installer ISO (a Windows ISO included) boots with a blank disk and its
//! screen on a password-protected loopback VNC port. Windows guests get
//! devices with in-box drivers (NVMe disk, e1000e NIC on x86), UEFI on x86
//! when OVMF is installed, Remote Desktop forwarded to a loopback port, and an
//! answer file ([`unattend`]) that skips Windows 11's TPM/Secure Boot checks
//! and creates the node's login user.
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
pub mod unattend;

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
use tokio::io::AsyncWriteExt;
use tokio::net::TcpStream;
use tokio::net::tcp::OwnedWriteHalf;
use tokio::process::{Child, Command};
use tokio::sync::Mutex;
use tokio::task::AbortHandle;

use super::affinity::pin;
use super::images::{self, ImageKind};
use super::{GuestAccess, Hypervisor, Placement, VmId, VmSnapshot, VmSpec};
use crate::{Error, Result};
use qmp::Qmp;

const SNAPSHOT_MAGIC: &[u8; 8] = b"PVQMSNP1";
const DEFAULT_USER: &str = "peervps";
/// Minimums for a Windows guest (Windows 11 needs 4 GiB; Setup alone fills ~20 GiB).
const WINDOWS_MIN_MEM_MIB: u64 = 4096;
const WINDOWS_MIN_DISK_GIB: u64 = 32;
/// virtio-win driver disc, attached to Windows guests when present in the image directory.
pub const VIRTIO_WIN_IMAGE: &str = "virtio-win";

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

    fn as_str(self) -> &'static str {
        match self {
            Self::X86_64 => "x86_64",
            Self::Aarch64 => "aarch64",
        }
    }
}

/// x86 UEFI firmware (OVMF): read-only code plus a template for each VM's variable store.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Uefi {
    pub code: PathBuf,
    pub vars: PathBuf,
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
    /// x86 UEFI (OVMF) for Windows installs; they fall back to BIOS without it.
    #[serde(default)]
    pub uefi: Option<Uefi>,
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
        let uefi = match arch {
            Arch::X86_64 => find_ovmf(&binary),
            Arch::Aarch64 => None,
        };
        Ok(Self {
            binary,
            img_binary,
            arch,
            accel: Accel::detect(),
            firmware,
            uefi,
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
    /// Set for guests booted from an installer ISO.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    installer: Option<Installer>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Installer {
    iso: PathBuf,
    windows: bool,
    /// Loopback TCP port of the guest's screen.
    vnc_port: u16,
    /// Loopback port forwarded to the guest's Remote Desktop (Windows).
    rdp_port: Option<u16>,
    /// virtio-win driver disc, if one is installed.
    drivers: Option<PathBuf>,
}

impl Installer {
    /// VNC's own password is limited to 8 characters.
    fn vnc_password(password: &str) -> &str {
        &password[..password.len().min(8)]
    }
}

struct Vm {
    child: Child,
    qmp: Arc<Qmp>,
    dir: PathBuf,
    launch: Launch,
    seed: Option<AbortHandle>,
    /// Press a key at the installer's "Press any key to boot from CD" prompt on first start.
    boot_keys: bool,
    serial: Arc<SerialInput>,
}

/// Keyboard input for the guest's serial port: a loopback connection to QEMU's
/// socket chardev, opened on first use. QEMU logs everything the guest prints to
/// `console.log` whether or not anyone is connected, so the bytes coming back on
/// the socket are only drained, to keep the guest from blocking on a full buffer.
struct SerialInput {
    port: u16,
    conn: Mutex<Option<(OwnedWriteHalf, AbortHandle)>>,
}

impl SerialInput {
    fn new(port: u16) -> Self {
        Self { port, conn: Mutex::new(None) }
    }

    async fn write(&self, data: &[u8]) -> Result<()> {
        let mut conn = self.conn.lock().await;
        // A connection QEMU dropped (e.g. across a restore) fails once; reconnect and retry.
        for _ in 0..2 {
            if conn.is_none() {
                let stream = TcpStream::connect((Ipv4Addr::LOCALHOST, self.port))
                    .await
                    .map_err(|e| Error::Hypervisor(format!("serial console: {e}")))?;
                let (mut rx, tx) = stream.into_split();
                let drain = tokio::spawn(async move {
                    let _ = tokio::io::copy(&mut rx, &mut tokio::io::sink()).await;
                });
                *conn = Some((tx, drain.abort_handle()));
            }
            if let Some((tx, drain)) = conn.as_mut() {
                if tx.write_all(data).await.is_ok() {
                    return Ok(());
                }
                drain.abort();
                *conn = None;
            }
        }
        Err(Error::Hypervisor("serial console connection lost".into()))
    }
}

impl Drop for SerialInput {
    fn drop(&mut self) {
        if let Some((_, drain)) = self.conn.get_mut().take() {
            drain.abort();
        }
    }
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
    async fn spawn(&self, dir: &Path, launch: &Launch, cores: &[u32], mut extra: Extra) -> Result<Vm> {
        let qmp_port = free_port()?;
        let serial_port = free_port()?;
        extra.serial_port = Some(serial_port);
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
            boot_keys: false,
            serial: Arc::new(SerialInput::new(serial_port)),
        };
        if let Some(pid) = vm.child.id() {
            pin(pid, cores);
        }
        if let Err(e) = wait_for_qmp(&mut vm, Duration::from_secs(20)).await {
            kill(&mut vm).await;
            return Err(e);
        }
        if launch.installer.is_some() {
            let password = Installer::vnc_password(&launch.password);
            if let Err(e) =
                vm.qmp.execute("set_password", Some(json!({ "protocol": "vnc", "password": password }))).await
            {
                kill(&mut vm).await;
                return Err(e);
            }
        }
        Ok(vm)
    }

    /// Blank disk plus everything an installer ISO boots with.
    async fn prepare_installer(
        &self,
        dir: &Path,
        iso: &Path,
        spec: &VmSpec,
        placement: &Placement,
    ) -> Result<Installer> {
        let info = images::iso_info(iso, &spec.image)?;
        if let Some(arch) = info.arch
            && arch != self.cfg.arch.as_str()
        {
            let hint = match (info.windows, self.cfg.arch) {
                (true, Arch::Aarch64) => {
                    "; use the Windows 11 ARM64 ISO (microsoft.com/software-download/windows11arm64)"
                }
                (true, Arch::X86_64) => "; use the x64 Windows ISO",
                _ => "",
            };
            return Err(Error::Unsupported(format!(
                "{} is an {arch} installer but this host runs {} guests{hint}",
                spec.image,
                self.cfg.arch.as_str()
            )));
        }
        if info.windows && placement.mem_mib < WINDOWS_MIN_MEM_MIB {
            return Err(Error::Invalid(format!("Windows needs at least {WINDOWS_MIN_MEM_MIB} MiB of memory")));
        }
        if info.windows && placement.disk_gib < WINDOWS_MIN_DISK_GIB {
            return Err(Error::Invalid(format!("Windows needs a disk of at least {WINDOWS_MIN_DISK_GIB} GiB")));
        }
        let size = format!("{}G", placement.disk_gib.max(1));
        let disk = dir.join("disk.qcow2");
        self.qemu_img(&["create".as_ref(), "-f".as_ref(), "qcow2".as_ref(), disk.as_os_str(), size.as_ref()]).await?;
        let mut installer = Installer {
            iso: iso.to_owned(),
            windows: info.windows,
            vnc_port: free_vnc_port()?,
            rdp_port: None,
            drivers: None,
        };
        if info.windows {
            installer.rdp_port = Some(free_port()?);
            installer.drivers = Some(images::iso_path(&self.cfg.images_dir, VIRTIO_WIN_IMAGE)).filter(|p| p.is_file());
            if self.cfg.arch == Arch::X86_64
                && let Some(uefi) = &self.cfg.uefi
            {
                tokio::fs::copy(&uefi.vars, dir.join("efivars.fd")).await?;
            }
        }
        Ok(installer)
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
        let image = images::resolve(&self.cfg.images_dir, &spec.image)?;
        let dir = self.vm_dir(id);
        tokio::fs::create_dir_all(&dir).await?;
        let hostname = format!("pv-{}", &id.0.simple().to_string()[..8]);
        let setup = async {
            let mut launch = Launch {
                vcpus: spec.vcpus,
                mem_mib: placement.mem_mib,
                image: spec.image.clone(),
                ssh_port: free_port()?,
                password: password(),
                installer: None,
            };
            if image.kind == ImageKind::Iso {
                let installer = self.prepare_installer(&dir, &image.path, spec, placement).await?;
                if installer.windows {
                    let answers = unattend::autounattend(self.cfg.arch, &hostname, &self.cfg.user, &launch.password);
                    tokio::fs::create_dir_all(dir.join("unattend")).await?;
                    tokio::fs::write(dir.join("unattend/autounattend.xml"), answers).await?;
                }
                launch.installer = Some(installer);
                let mut vm = self
                    .spawn(
                        &dir,
                        &launch,
                        &placement.pinned_cores,
                        Extra { seed_url: None, incoming: false, serial_port: None },
                    )
                    .await?;
                vm.boot_keys = true;
                return Ok(vm);
            }
            self.create_overlay(&image.path, &dir.join("disk.qcow2"), placement.disk_gib).await?;
            let (url, seed) = seed::serve(seed::Seed {
                instance_id: id.to_string(),
                hostname: hostname.clone(),
                user: self.cfg.user.clone(),
                password: launch.password.clone(),
                ssh_keys: self.cfg.ssh_keys.clone(),
            })
            .await?;
            match self
                .spawn(
                    &dir,
                    &launch,
                    &placement.pinned_cores,
                    Extra { seed_url: Some(url), incoming: false, serial_port: None },
                )
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
        let (qmp, boot_keys) = {
            let mut vms = self.vms.lock().await;
            let vm = vms.get_mut(&id).ok_or_else(|| Error::NotFound(format!("vm {id}")))?;
            (vm.qmp.clone(), std::mem::take(&mut vm.boot_keys))
        };
        qmp.execute("cont", None).await?;
        if boot_keys {
            // Installer discs wait a few seconds for a key before falling through
            // to the (still empty) disk; nobody has the screen open that early.
            let window = if self.cfg.accel == Accel::Tcg { 30 } else { 12 };
            tokio::spawn(async move {
                for _ in 0..window * 2 {
                    tokio::time::sleep(Duration::from_millis(500)).await;
                    let key = json!({ "keys": [{ "type": "qcode", "data": "spc" }] });
                    if qmp.execute("send-key", Some(key)).await.is_err() {
                        break;
                    }
                }
            });
        }
        Ok(())
    }

    async fn pause(&self, id: VmId) -> Result<()> {
        self.qmp(id, "stop", None).await.map(drop)
    }

    async fn resume(&self, id: VmId) -> Result<()> {
        self.qmp(id, "cont", None).await.map(drop)
    }

    async fn snapshot(&self, id: VmId) -> Result<VmSnapshot> {
        let (dir, launch) = self.with_vm(id, |vm| (vm.dir.clone(), vm.launch.clone())).await?;
        if launch.installer.is_some() {
            // Their disk is a full image, not a small overlay over a shared base.
            return Err(Error::Unsupported(
                "scale to zero is not supported for guests installed from an ISO yet".into(),
            ));
        }
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
        let base = images::resolve(&self.cfg.images_dir, &launch.image)?.path;
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
        let vm = self
            .spawn(&dir, &launch, &placement.pinned_cores, Extra { seed_url: None, incoming: true, serial_port: None })
            .await?;
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

    async fn console_write(&self, id: VmId, data: &[u8]) -> Result<()> {
        let serial = self.with_vm(id, |vm| vm.serial.clone()).await?;
        serial.write(data).await
    }

    async fn access(&self, id: VmId) -> Result<Option<GuestAccess>> {
        let launch = self.with_vm(id, |vm| vm.launch.clone()).await?;
        let installer = launch.installer.as_ref();
        // A Linux installer asks for its own user; cloud images and Windows get ours.
        let ours = installer.is_none_or(|i| i.windows);
        Ok(Some(GuestAccess {
            ssh_host: "127.0.0.1".into(),
            ssh_port: launch.ssh_port,
            user: if ours { self.cfg.user.clone() } else { String::new() },
            password: ours.then(|| launch.password.clone()),
            windows: installer.is_some_and(|i| i.windows),
            rdp: installer.and_then(|i| i.rdp_port).map(|p| format!("127.0.0.1:{p}")),
            display: installer.map(|i| format!("vnc://127.0.0.1:{}", i.vnc_port)),
            display_password: installer.map(|_| Installer::vnc_password(&launch.password).to_owned()),
        }))
    }
}

struct Extra {
    seed_url: Option<String>,
    incoming: bool,
    /// Loopback port for the serial console's input; `None` logs output only.
    serial_port: Option<u16>,
}

/// QEMU's option parser splits on commas; a literal comma in a value is written `,,`.
fn esc(p: &Path) -> String {
    p.display().to_string().replace(',', ",,")
}

fn command_line(cfg: &QemuConfig, dir: &Path, launch: &Launch, qmp_port: u16, extra: &Extra) -> Vec<OsString> {
    let (accel, cpu) = cfg.accel.args();
    let installer = launch.installer.as_ref();
    let windows = installer.is_some_and(|i| i.windows);
    let x86 = cfg.arch == Arch::X86_64;
    let machine = match cfg.arch {
        Arch::X86_64 => "q35",
        // Windows on Arm requires a GICv3.
        Arch::Aarch64 if windows => "virt,gic-version=3",
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
        "-qmp".into(),
        format!("tcp:127.0.0.1:{qmp_port},server=on,wait=off"),
    ];
    let console = esc(&dir.join("console.log"));
    match extra.serial_port {
        Some(port) => a.extend([
            "-chardev".into(),
            format!("socket,id=ser0,host=127.0.0.1,port={port},server=on,wait=off,logfile={console},logappend=on"),
            "-serial".into(),
            "chardev:ser0".into(),
        ]),
        None => a.extend(["-serial".into(), format!("file:{console}")]),
    }
    let disk = esc(&dir.join("disk.qcow2"));
    if windows {
        // Windows has in-box NVMe drivers on x86 and Arm; it has none for virtio-blk.
        a.extend([
            "-drive".into(),
            format!("if=none,id=disk0,file={disk},format=qcow2,discard=unmap"),
            "-device".into(),
            "nvme,drive=disk0,serial=peervps0,bootindex=1".into(),
        ]);
    } else {
        a.extend(["-drive".into(), format!("if=virtio,file={disk},format=qcow2,discard=unmap")]);
    }
    let mut net = format!("user,id=n0,hostfwd=tcp:127.0.0.1:{}-:22", launch.ssh_port);
    if let Some(rdp) = installer.and_then(|i| i.rdp_port) {
        net.push_str(&format!(",hostfwd=tcp:127.0.0.1:{rdp}-:3389"));
    }
    // e1000e has an in-box Windows driver on x86; Windows on Arm needs virtio-win either way.
    let nic = if windows && x86 { "e1000e" } else { "virtio-net-pci" };
    a.extend([
        "-netdev".into(),
        net,
        "-device".into(),
        format!("{nic},netdev=n0"),
        // Cloud images generate SSH host keys on first boot; give them entropy.
        "-device".into(),
        "virtio-rng-pci".into(),
        "-S".into(),
    ]);
    if let Some(i) = installer {
        // A screen, keyboard and pointer for the installer, on a loopback VNC port.
        let cdrom = |id: &str, bus: &str, boot: Option<u8>| {
            let boot = boot.map(|b| format!(",bootindex={b}")).unwrap_or_default();
            if x86 {
                format!("ide-cd,drive={id},bus={bus}{boot}")
            } else {
                format!("usb-storage,drive={id},removable=on{boot}")
            }
        };
        a.extend([
            "-device".into(),
            "qemu-xhci,id=xhci".into(),
            "-device".into(),
            "usb-kbd".into(),
            "-device".into(),
            "usb-tablet".into(),
            "-device".into(),
            if x86 { "VGA,vgamem_mb=64".into() } else { "ramfb".into() },
            "-vnc".into(),
            format!("127.0.0.1:{},password=on", i.vnc_port.saturating_sub(5900)),
            "-drive".into(),
            format!("if=none,id=cd0,media=cdrom,readonly=on,file={}", esc(&i.iso)),
            "-device".into(),
            cdrom("cd0", "ide.0", Some(0)),
        ]);
        if windows {
            // autounattend.xml on a read-only USB stick; Setup reads it from any removable drive.
            a.extend([
                "-blockdev".into(),
                format!(
                    "driver=vvfat,node-name=unattend,dir={},label=PEERVPS,read-only=on",
                    esc(&dir.join("unattend"))
                ),
                "-device".into(),
                "usb-storage,drive=unattend,removable=on".into(),
            ]);
        }
        if let Some(drivers) = &i.drivers {
            a.extend([
                "-drive".into(),
                format!("if=none,id=cd1,media=cdrom,readonly=on,file={}", esc(drivers)),
                "-device".into(),
                cdrom("cd1", "ide.1", None),
            ]);
        }
    }
    if let Some(fw) = &cfg.firmware {
        a.extend(["-bios".into(), esc(fw)]);
    } else if windows && let Some(uefi) = &cfg.uefi {
        a.extend([
            "-drive".into(),
            format!("if=pflash,format=raw,unit=0,readonly=on,file={}", esc(&uefi.code)),
            "-drive".into(),
            format!("if=pflash,format=raw,unit=1,file={}", esc(&dir.join("efivars.fd"))),
        ]);
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

/// QEMU's `-vnc` takes a display number, port 5900 + n.
fn free_vnc_port() -> Result<u16> {
    (5900..6000)
        .find(|p| TcpListener::bind((Ipv4Addr::LOCALHOST, *p)).is_ok())
        .ok_or_else(|| Error::Hypervisor("no free VNC port in 5900-5999".into()))
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

/// OVMF code + variable template pairs, as Linux distros, Homebrew and the Windows installer ship them.
fn find_ovmf(binary: &Path) -> Option<Uefi> {
    let bin = binary.parent()?;
    let dirs = [
        bin.join("../share/qemu"),
        bin.join("share"),
        PathBuf::from("/usr/share/OVMF"),
        PathBuf::from("/usr/share/edk2/ovmf"),
    ];
    let pairs = [
        ("edk2-x86_64-code.fd", "edk2-i386-vars.fd"),
        ("OVMF_CODE_4M.fd", "OVMF_VARS_4M.fd"),
        ("OVMF_CODE.fd", "OVMF_VARS.fd"),
    ];
    dirs.iter()
        .flat_map(|d| pairs.map(|(c, v)| (d.join(c), d.join(v))))
        .find(|(c, v)| c.is_file() && v.is_file())
        .map(|(code, vars)| Uefi { code, vars })
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
