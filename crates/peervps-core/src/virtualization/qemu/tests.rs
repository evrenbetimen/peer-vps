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
        images_dir: root.join("images"),
        run_dir: root.join("run"),
        user: DEFAULT_USER.into(),
        ssh_keys: vec![],
    }
}

fn launch() -> Launch {
    Launch { vcpus: 2, mem_mib: 1024, image: "ubuntu-24.04".into(), ssh_port: 2222, password: "pw".into() }
}

fn joined(args: &[OsString]) -> String {
    args.iter().map(|a| a.to_string_lossy().into_owned()).collect::<Vec<_>>().join(" ")
}

#[test]
fn command_line_wires_disk_network_console_and_seed() {
    let root = PathBuf::from("/srv/pv,x");
    let dir = root.join("run/vm");
    let extra = Extra { seed_url: Some("http://10.0.2.2:4000/".into()), incoming: false };
    let cmd = joined(&command_line(&cfg(&root), &dir, &launch(), 4444, &extra));
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
    let cmd = joined(&command_line(&arm, &dir, &launch(), 1, &Extra { seed_url: None, incoming: true }));
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

    hv.pause(id).await.expect("pause");
    let snap = hv.snapshot(id).await.expect("snapshot");
    assert!(snap.bytes.len() > 64 * 1024, "snapshot carries guest state");
    hv.resume(id).await.expect("resume after snapshot");
    assert_eq!(status(hv.qmp(id, "query-status", None).await.expect("status")), "running");

    hv.restore(snap, &placement).await.expect("restore");
    hv.resume(id).await.expect("resume restored");
    assert_eq!(status(hv.qmp(id, "query-status", None).await.expect("status")), "running");
    assert!(hv.console_tail(id, 1024).await.expect("console").is_some());

    hv.destroy(id).await.expect("destroy");
    assert!(!hv.vm_dir(id).exists());
    assert!(hv.start(id).await.is_err());

    let mut missing = spec.clone();
    missing.image = "nope".into();
    let err = hv.create(VmId::new(), &missing, &placement).await.expect_err("missing image");
    assert!(matches!(err, Error::NotFound(_)), "{err}");
    let _ = std::fs::remove_dir_all(root);
}
