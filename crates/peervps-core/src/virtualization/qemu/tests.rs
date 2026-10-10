//! The pure parts run everywhere; the lifecycle test drives a real QEMU when
//! one is installed (CI installs it on Linux and macOS) with a blank disk, so
//! it needs neither a guest OS nor hardware acceleration.

use super::*;

fn cfg(root: &Path) -> QemuConfig {
    QemuConfig {
        binary: PathBuf::from("qemu-system-x86_64"),
        img_binary: PathBuf::from("qemu-img"),
        arch: Arch::X86_64,
        accel: Accel::Tcg,
        firmware: None,
        uefi: None,
        images_dir: root.join("images"),
        run_dir: root.join("run"),
        user: DEFAULT_USER.into(),
        ssh_keys: vec![],
    }
}

fn launch() -> Launch {
    Launch {
        vcpus: 2,
        mem_mib: 1024,
        image: "ubuntu-24.04".into(),
        ssh_port: 2222,
        password: "pw".into(),
        installer: None,
    }
}

fn joined(args: &[OsString]) -> String {
    args.iter().map(|a| a.to_string_lossy().into_owned()).collect::<Vec<_>>().join(" ")
}

#[test]
fn command_line_wires_disk_network_console_and_seed() {
    let root = PathBuf::from("/srv/pv,x");
    let dir = root.join("run/vm");
    let extra = Extra { seed_url: Some("http://10.0.2.2:4000/".into()), incoming: false, serial_port: Some(5555) };
    let cmd = joined(&command_line(&cfg(&root), &dir, &launch(), 4444, &extra));
    assert!(
        cmd.contains("-chardev socket,id=ser0,host=127.0.0.1,port=5555,server=on,wait=off,logfile=")
            && cmd.contains("console.log,logappend=on -serial chardev:ser0"),
        "serial console takes input: {cmd}"
    );
    assert!(cmd.contains("-machine q35 -accel tcg,thread=multi -cpu max -smp 2 -m 1024M"), "{cmd}");
    let disk = format!("file={},format=qcow2", dir.join("disk.qcow2").display().to_string().replace(',', ",,"));
    assert!(cmd.contains(&disk), "commas escaped: {cmd}");
    assert!(cmd.contains("pv,,x") && !cmd.contains("pv,x"));
    assert!(cmd.contains("hostfwd=tcp:127.0.0.1:2222-:22"));
    assert!(cmd.contains("-qmp tcp:127.0.0.1:4444,server=on,wait=off"));
    assert!(cmd.contains("-smbios type=1,serial=ds=nocloud;s=http://10.0.2.2:4000/"));
    assert!(cmd.contains(" -S"));
    assert!(!cmd.contains("-incoming"));

    let mut arm = cfg(&root);
    arm.arch = Arch::Aarch64;
    arm.accel = Accel::Hvf;
    let fw = PathBuf::from("/opt/homebrew/share/qemu/edk2-aarch64-code.fd");
    arm.firmware = Some(fw.clone());
    let cmd =
        joined(&command_line(&arm, &dir, &launch(), 1, &Extra { seed_url: None, incoming: true, serial_port: None }));
    assert!(cmd.contains("-machine virt -accel hvf -cpu host"), "{cmd}");
    assert!(cmd.contains(&format!("-bios {}", fw.display())));
    assert!(cmd.ends_with("-incoming defer"));
    assert!(!cmd.contains("-smbios"));

    let mut win = cfg(&root);
    win.accel = Accel::Whpx;
    assert!(
        joined(&command_line(&win, &dir, &launch(), 1, &extra)).contains("-accel whpx,kernel-irqchip=off -cpu max")
    );
}

fn windows_launch(root: &Path) -> Launch {
    Launch {
        mem_mib: 4096,
        image: "win11".into(),
        password: "abcdefgh12345678".into(),
        installer: Some(Installer {
            iso: root.join("images/win11.iso"),
            windows: true,
            vnc_port: 5903,
            rdp_port: Some(3390),
            drivers: Some(root.join("images/virtio-win.iso")),
        }),
        ..launch()
    }
}

#[test]
fn windows_installs_get_in_box_devices_a_screen_and_answers() {
    let root = PathBuf::from("/srv/pv");
    let dir = root.join("run/vm");
    let none = Extra { seed_url: None, incoming: false, serial_port: None };
    let mut x86 = cfg(&root);
    x86.uefi =
        Some(Uefi { code: "/usr/share/OVMF/OVMF_CODE_4M.fd".into(), vars: "/usr/share/OVMF/OVMF_VARS_4M.fd".into() });
    let cmd = joined(&command_line(&x86, &dir, &windows_launch(&root), 1, &none));
    for want in [
        "-machine q35 ",
        "nvme,drive=disk0,serial=peervps0,bootindex=1",
        "hostfwd=tcp:127.0.0.1:2222-:22,hostfwd=tcp:127.0.0.1:3390-:3389",
        "-device e1000e,netdev=n0",
        "-device VGA,vgamem_mb=64",
        "-vnc 127.0.0.1:3,password=on",
        "file=/srv/pv/images/win11.iso",
        "ide-cd,drive=cd0,bus=ide.0,bootindex=0",
        "driver=vvfat,node-name=unattend,dir=/srv/pv/run/vm/unattend",
        "ide-cd,drive=cd1,bus=ide.1",
        "if=pflash,format=raw,unit=0,readonly=on,file=/usr/share/OVMF/OVMF_CODE_4M.fd",
        "if=pflash,format=raw,unit=1,file=/srv/pv/run/vm/efivars.fd",
    ] {
        // Joined paths mix separators on Windows (`/srv/pv\\images/win11.iso`).
        assert!(cmd.replace('\\', "/").contains(want), "missing {want}: {cmd}");
    }
    assert!(!cmd.contains("if=virtio,file") && !cmd.contains("-smbios"));

    let mut arm = cfg(&root);
    arm.arch = Arch::Aarch64;
    arm.accel = Accel::Hvf;
    arm.firmware = Some("/opt/homebrew/share/qemu/edk2-aarch64-code.fd".into());
    let cmd = joined(&command_line(&arm, &dir, &windows_launch(&root), 1, &none));
    for want in [
        "-machine virt,gic-version=3 -accel hvf -cpu host",
        "-device ramfb",
        "usb-storage,drive=cd0,removable=on,bootindex=0",
        "virtio-net-pci,netdev=n0",
        "-bios ",
    ] {
        assert!(cmd.contains(want), "missing {want}: {cmd}");
    }
    assert!(!cmd.contains("pflash") && !cmd.contains("ide-cd"));

    // A Linux installer keeps virtio devices and gets no answer file or RDP.
    let mut linux = windows_launch(&root);
    let i = linux.installer.as_mut().expect("installer");
    (i.windows, i.rdp_port, i.drivers) = (false, None, None);
    let cmd = joined(&command_line(&x86, &dir, &linux, 1, &none));
    assert!(cmd.contains("if=virtio,file=") && cmd.contains("virtio-net-pci") && cmd.contains("-vnc "));
    assert!(!cmd.contains("vvfat") && !cmd.contains("3389") && !cmd.contains("pflash"));
}

#[test]
fn only_our_own_qemus_count_as_orphans() {
    let none = Extra { seed_url: None, incoming: false, serial_port: None };
    let ours = cfg(Path::new("/data/peervps"));
    let args = command_line(&ours, &ours.run_dir.join("vm-1"), &launch(), 4444, &none);
    let qemu = std::ffi::OsStr::new("qemu-system-x86_64");
    assert!(is_orphan(qemu, &args, &ours.run_dir));
    // Another install's VM, or another program reading our files, is left alone.
    let other = cfg(Path::new("/elsewhere"));
    assert!(!is_orphan(
        qemu,
        &command_line(&other, &other.run_dir.join("vm-1"), &launch(), 4444, &none),
        &ours.run_dir
    ));
    assert!(!is_orphan(std::ffi::OsStr::new("tail"), &args, &ours.run_dir));
}

/// A stand-in "QEMU" (a shell renamed so the OS reports it as qemu-system)
/// whose command line names a VM directory, as a crashed app would leave it.
/// Linux only: macOS's /bin/sh is a launcher that re-execs bash, so the copy
/// does not keep the qemu-system name there.
#[cfg(target_os = "linux")]
#[test]
fn a_fresh_start_stops_qemus_left_in_its_run_dir() {
    let root = std::env::temp_dir().join(format!("pvqo-{}", uuid::Uuid::new_v4().simple()));
    std::fs::create_dir_all(&root).expect("tmp");
    let fake = root.join("qemu-system-x86_64");
    std::fs::copy("/bin/sh", &fake).expect("copy sh");
    let run_dir = root.join("run");
    let spawn = |dir: &Path| {
        std::process::Command::new(&fake)
            .args(["-c", "sleep 60; true"])
            .arg(dir.join("vm-1").join("console.log"))
            .spawn()
            .expect("spawn")
    };
    let mut ours = spawn(&run_dir);
    let mut other = spawn(&root.join("elsewhere"));
    std::thread::sleep(Duration::from_millis(200));

    reap_orphans(&run_dir);

    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while ours.try_wait().expect("wait").is_none() && std::time::Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(ours.try_wait().expect("wait").is_some(), "our leftover QEMU was stopped");
    assert!(other.try_wait().expect("wait").is_none(), "another install's QEMU keeps running");
    let _ = other.kill();
    let _ = other.wait();
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn snapshot_framing_round_trips_and_rejects_garbage() {
    let packed = pack_snapshot(&launch(), b"state", b"disk-bytes").expect("pack");
    let (l, state, disk) = unpack_snapshot(&packed).expect("unpack");
    assert_eq!(l, launch());
    assert_eq!(state, b"state");
    assert_eq!(disk, b"disk-bytes");
    assert!(unpack_snapshot(b"PVFCSNP1........").is_err());
    let mut truncated = packed.clone();
    truncated.truncate(30);
    assert!(unpack_snapshot(&truncated).is_err());
}

#[test]
fn passwords_are_random_and_unambiguous() {
    let (a, b) = (password(), password());
    assert_eq!(a.len(), 16);
    assert_ne!(a, b);
    assert!(!a.contains(['0', 'O', 'l', '1', 'I']));
}

/// Real QEMU, blank disk, TCG: create → start → pause → snapshot → resume →
/// destroy, then restore the snapshot into a fresh process.
#[tokio::test]
async fn lifecycle_against_real_qemu() {
    let root = std::env::temp_dir().join(format!("pvq-{}", uuid::Uuid::new_v4().simple()));
    let cfg = match QemuConfig::detect(root.join("images"), root.join("run")) {
        Ok(mut c) => {
            c.accel = Accel::Tcg; // CI runners have no nested virtualization.
            c
        }
        Err(e) => {
            eprintln!("skipping: {e}");
            return;
        }
    };
    if cfg.arch == Arch::Aarch64 && cfg.firmware.is_none() {
        eprintln!("skipping: no aarch64 firmware");
        return;
    }
    let hv = QemuHypervisor::new(cfg).expect("hypervisor");
    let blank = images::path(&hv.cfg.images_dir, "blank");
    hv.qemu_img(&["create".as_ref(), "-f".as_ref(), "qcow2".as_ref(), blank.as_os_str(), "64M".as_ref()])
        .await
        .expect("blank image");

    let spec =
        VmSpec { vcpus: 1, mem_mib: 128, disk_gib: 1, image: "blank".into(), accelerator: None, confidential: false };
    let placement = Placement { pinned_cores: vec![0], mem_mib: 128, disk_gib: 1, accelerator: None };
    let id = VmId::new();
    hv.create(id, &spec, &placement).await.expect("create");
    let status = |v: Value| v["status"].as_str().unwrap_or_default().to_owned();
    assert_eq!(status(hv.qmp(id, "query-status", None).await.expect("status")), "prelaunch");

    hv.start(id).await.expect("start");
    assert_eq!(status(hv.qmp(id, "query-status", None).await.expect("status")), "running");
    let access = hv.access(id).await.expect("access").expect("ssh");
    assert_eq!(access.user, "peervps");
    assert!(access.ssh_command().starts_with("ssh -p "));
    let disk = hv
        .qemu_img(&[
            "info".as_ref(),
            "-U".as_ref(),
            "--output=json".as_ref(),
            hv.vm_dir(id).join("disk.qcow2").as_os_str(),
        ])
        .await
        .expect("info");
    let disk: Value = serde_json::from_slice(&disk).expect("json");
    assert_eq!(disk["virtual-size"], 1u64 << 30, "overlay grown to the rented size");
    assert!(disk["backing-filename"].as_str().is_some_and(|b| b.ends_with("blank.qcow2")));

    hv.console_write(id, b"peervps\r").await.expect("type into the serial console");

    hv.pause(id).await.expect("pause");
    let snap = hv.snapshot(id).await.expect("snapshot");
    assert!(snap.bytes.len() > 64 * 1024, "snapshot carries guest state");
    hv.resume(id).await.expect("resume after snapshot");
    assert_eq!(status(hv.qmp(id, "query-status", None).await.expect("status")), "running");

    hv.restore(snap, &placement).await.expect("restore");
    hv.resume(id).await.expect("resume restored");
    assert_eq!(status(hv.qmp(id, "query-status", None).await.expect("status")), "running");
    assert!(hv.console_tail(id, 1024).await.expect("console").is_some());
    hv.console_write(id, b"\r").await.expect("type after a restore");

    hv.destroy(id).await.expect("destroy");
    assert!(!hv.vm_dir(id).exists());
    assert!(hv.start(id).await.is_err());

    let mut missing = spec.clone();
    missing.image = "nope".into();
    let err = hv.create(VmId::new(), &missing, &placement).await.expect_err("missing image");
    assert!(matches!(err, Error::NotFound(_)), "{err}");
    let _ = std::fs::remove_dir_all(root);
}

fn real_qemu(root: &Path) -> Option<QemuHypervisor> {
    let cfg = match QemuConfig::detect(root.join("images"), root.join("run")) {
        Ok(mut c) => {
            c.accel = Accel::Tcg;
            c
        }
        Err(e) => {
            eprintln!("skipping: {e}");
            return None;
        }
    };
    if cfg.arch == Arch::Aarch64 && cfg.firmware.is_none() {
        eprintln!("skipping: no aarch64 firmware");
        return None;
    }
    Some(QemuHypervisor::new(cfg).expect("hypervisor"))
}

/// Real QEMU, TCG, a Windows-labelled ISO with no OS on it: the installer
/// devices, answer-file stick and password-protected screen all come up.
#[tokio::test]
async fn windows_iso_install_boots_with_a_locked_screen() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let root = std::env::temp_dir().join(format!("pvqi-{}", uuid::Uuid::new_v4().simple()));
    let Some(hv) = real_qemu(&root) else { return };
    let arch_label = if hv.cfg.arch == Arch::X86_64 { "CCCOMA_X64FRE_EN-US_DV9" } else { "CPBA_A64FRE_EN-US_DV9" };
    images::tests::fake_iso(&images::iso_path(&hv.cfg.images_dir, "win11"), arch_label);

    let spec =
        VmSpec { vcpus: 1, mem_mib: 4096, disk_gib: 64, image: "win11".into(), accelerator: None, confidential: false };
    let placement = Placement { pinned_cores: vec![0], mem_mib: 4096, disk_gib: 64, accelerator: None };
    let small = Placement { mem_mib: 2048, ..placement.clone() };
    let err = hv.create(VmId::new(), &spec, &small).await.expect_err("too little memory");
    assert!(err.to_string().contains("4096 MiB"), "{err}");

    let id = VmId::new();
    hv.create(id, &spec, &placement).await.expect("create");
    let xml = std::fs::read_to_string(hv.vm_dir(id).join("unattend/autounattend.xml")).expect("answer file");
    assert!(xml.contains("BypassTPMCheck"));
    hv.start(id).await.expect("start");

    let access = hv.access(id).await.expect("access").expect("some");
    assert!(access.windows && access.rdp.is_some());
    let (user, password) = (access.user.clone(), access.password.clone().expect("password"));
    assert_eq!(user, "peervps");
    assert!(xml.contains(&format!("<Value>{password}</Value>")), "the answer file creates the advertised login");
    let display = access.display.expect("display");
    let port: u16 = display.rsplit(':').next().and_then(|p| p.parse().ok()).expect("port");
    assert_eq!(access.display_password.as_deref(), Some(&password[..8]));

    // RFB handshake: the server must offer only VNC authentication (type 2).
    let mut s = tokio::net::TcpStream::connect(("127.0.0.1", port)).await.expect("vnc");
    let mut version = [0u8; 12];
    s.read_exact(&mut version).await.expect("version");
    assert!(version.starts_with(b"RFB 003."), "{version:?}");
    s.write_all(b"RFB 003.008\n").await.expect("write");
    let n = s.read_u8().await.expect("count") as usize;
    let mut types = vec![0u8; n];
    s.read_exact(&mut types).await.expect("types");
    assert_eq!(types, vec![2], "password required");

    let err = hv.snapshot(id).await.expect_err("no scale-to-zero yet");
    assert!(matches!(err, Error::Unsupported(_)), "{err}");
    hv.destroy(id).await.expect("destroy");

    // An installer for the other architecture is refused with a pointer to the right one.
    let other = if hv.cfg.arch == Arch::X86_64 { "CPBA_A64FRE_EN-US_DV9" } else { "CCCOMA_X64FRE_EN-US_DV9" };
    images::tests::fake_iso(&images::iso_path(&hv.cfg.images_dir, "win-other"), other);
    let wrong = VmSpec { image: "win-other".into(), ..spec };
    let err = hv.create(VmId::new(), &wrong, &placement).await.expect_err("wrong arch");
    assert!(matches!(err, Error::Unsupported(_)) && err.to_string().contains("Windows"), "{err}");
    let _ = std::fs::remove_dir_all(root);
}
