//! Drives the backend against a fake `firecracker` (a small Python API server
//! that records every request), so the full lifecycle is tested without KVM.

use std::os::unix::fs::PermissionsExt;

use super::*;

const FAKE_FIRECRACKER: &str = r#"#!/usr/bin/env python3
import json, os, socketserver, sys
from http.server import BaseHTTPRequestHandler

sock = sys.argv[sys.argv.index("--api-sock") + 1]
log = os.path.join(os.path.dirname(sock), "requests.log")

class H(BaseHTTPRequestHandler):
    def _handle(self):
        n = int(self.headers.get("Content-Length") or 0)
        body = json.loads(self.rfile.read(n) or b"null")
        with open(log, "a") as f:
            f.write(json.dumps({"m": self.command, "p": self.path, "b": body}) + "\n")
        if self.command == "GET":
            out = b'{"state":"Not started"}'
            self.send_response(200)
            self.send_header("Content-Length", str(len(out)))
            self.end_headers()
            self.wfile.write(out)
            return
        if self.path == "/snapshot/create":
            open(body["snapshot_path"], "wb").write(b"vmstate")
            open(body["mem_file_path"], "wb").write(b"\0" * 4096)
        if self.path == "/boot-source" and not os.path.exists(body["kernel_image_path"]):
            out = b'{"fault_message":"kernel missing"}'
            self.send_response(400)
            self.send_header("Content-Length", str(len(out)))
            self.end_headers()
            self.wfile.write(out)
            return
        self.send_response(204)
        self.end_headers()
    do_GET = do_PUT = do_PATCH = _handle
    def log_message(self, *a):
        pass

socketserver.UnixStreamServer(sock, H).serve_forever()
"#;

struct Fixture {
    root: PathBuf,
    hv: FirecrackerHypervisor,
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

fn fixture() -> Option<Fixture> {
    if std::process::Command::new("python3").arg("--version").output().is_err() {
        eprintln!("skipping: python3 not available for the fake firecracker");
        return None;
    }
    let root = std::env::temp_dir().join(format!("pvps-fc-{}", uuid::Uuid::new_v4().simple()));
    let images = root.join("images");
    std::fs::create_dir_all(&images).expect("mkdir");
    let binary = root.join("firecracker");
    std::fs::write(&binary, FAKE_FIRECRACKER).expect("write fake");
    std::fs::set_permissions(&binary, std::fs::Permissions::from_mode(0o755)).expect("chmod");
    let kernel = root.join("vmlinux");
    std::fs::write(&kernel, b"kernel").expect("kernel");
    std::fs::write(images.join("ubuntu-24.04.ext4"), b"rootfs").expect("image");
    let cfg = FirecrackerConfig::new(binary, kernel, images, root.join("run"));
    Some(Fixture { hv: FirecrackerHypervisor::new(cfg).expect("hypervisor"), root })
}

fn spec() -> VmSpec {
    VmSpec { vcpus: 1, mem_mib: 512, disk_gib: 1, image: "ubuntu-24.04".into(), accelerator: None, confidential: false }
}

fn placement() -> Placement {
    Placement { pinned_cores: vec![0], mem_mib: 512, disk_gib: 1, accelerator: None }
}

fn requests(hv: &FirecrackerHypervisor, id: VmId) -> Vec<(String, String, serde_json::Value)> {
    let log = std::fs::read_to_string(hv.vm_dir(id).join("requests.log")).unwrap_or_default();
    log.lines()
        .map(|l| serde_json::from_str::<serde_json::Value>(l).expect("json"))
        .filter(|v| v["m"] != "GET")
        .map(|v| (v["m"].as_str().unwrap_or("").to_owned(), v["p"].as_str().unwrap_or("").to_owned(), v["b"].clone()))
        .collect()
}

#[tokio::test]
async fn full_lifecycle_against_fake_firecracker() {
    let Some(fx) = fixture() else { return };
    let id = VmId::new();
    fx.hv.create(id, &spec(), &placement()).await.expect("create");
    fx.hv.start(id).await.expect("start");
    fx.hv.pause(id).await.expect("pause");
    let snap = fx.hv.snapshot(id).await.expect("snapshot");

    let calls: Vec<(String, String)> = requests(&fx.hv, id).into_iter().map(|(m, p, _)| (m, p)).collect();
    let expect = [
        ("PUT", "/machine-config"),
        ("PUT", "/boot-source"),
        ("PUT", "/drives/rootfs"),
        ("PUT", "/actions"),
        ("PATCH", "/vm"),
        ("PUT", "/snapshot/create"),
    ];
    assert_eq!(calls, expect.map(|(m, p)| (m.to_owned(), p.to_owned())).to_vec());

    let reqs = requests(&fx.hv, id);
    assert_eq!(reqs[0].2["vcpu_count"], 1);
    assert_eq!(reqs[0].2["mem_size_mib"], 512);
    let disk = fx.hv.vm_dir(id).join("rootfs.ext4");
    assert_eq!(std::fs::metadata(&disk).expect("disk").len(), 1 << 30, "disk grown to rented size");
    let (state, mem) = unpack_snapshot(&snap.bytes).expect("unpack");
    assert_eq!(state, b"vmstate");
    assert_eq!(mem.len(), 4096);

    // Restore replaces the process and loads the snapshot into the new one.
    fx.hv.restore(snap, &placement()).await.expect("restore");
    let last = requests(&fx.hv, id).pop().expect("load request");
    assert_eq!(last.1, "/snapshot/load");
    assert_eq!(last.2["mem_backend"]["backend_type"], "File");
    fx.hv.resume(id).await.expect("resume");

    fx.hv.destroy(id).await.expect("destroy");
    assert!(!fx.hv.vm_dir(id).exists(), "vm directory removed");
}

#[tokio::test]
async fn bad_kernel_fails_create_and_cleans_up() {
    let Some(fx) = fixture() else { return };
    std::fs::remove_file(&fx.hv.cfg.kernel).expect("rm kernel");
    let id = VmId::new();
    let err = fx.hv.create(id, &spec(), &placement()).await.expect_err("must fail");
    assert!(err.to_string().contains("kernel missing"), "{err}");
    assert!(!fx.hv.vm_dir(id).exists());
    assert!(fx.hv.start(id).await.is_err());
}

#[tokio::test]
async fn rejects_path_traversal_in_image_names() {
    let Some(fx) = fixture() else { return };
    let mut s = spec();
    for bad in ["../etc/passwd", "a/b", "", ".hidden"] {
        s.image = bad.into();
        assert!(fx.hv.cfg.image_path(&s.image).is_err(), "{bad:?} accepted");
    }
    s.image = "missing".into();
    assert!(matches!(fx.hv.cfg.image_path(&s.image), Err(Error::NotFound(_))));
}

#[test]
fn rejects_run_dir_too_long_for_unix_sockets() {
    let Some(fx) = fixture() else { return };
    let mut cfg = fx.hv.cfg.clone();
    cfg.run_dir = fx.root.join("x".repeat(80));
    assert!(matches!(FirecrackerHypervisor::new(cfg), Err(Error::Invalid(_))));
}

#[test]
fn snapshot_framing_rejects_garbage() {
    assert!(unpack_snapshot(b"nope").is_err());
    let mut bad = pack_snapshot(b"s", b"m");
    bad[15] = 0xff;
    assert!(unpack_snapshot(&bad).is_err());
    assert_eq!(guest_mac(VmId::new()).len(), 17);
    assert!(tap_name(VmId::new()).len() <= 15);
}
