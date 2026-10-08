//! Acceptance test: drives the shipped `peervps` binary end to end, exactly as
//! an agent would — a `serve` process plus one CLI invocation per step.

use std::io::{BufRead, BufReader};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::Value;

const BIN: &str = env!("CARGO_BIN_EXE_peervps");

struct Server {
    child: Child,
    api: String,
    key: String,
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn serve() -> Server {
    let port = std::net::TcpListener::bind("127.0.0.1:0").expect("bind").local_addr().expect("addr").port();
    let mut child = Command::new(BIN)
        .args(["serve", "--listen", &format!("127.0.0.1:{port}")])
        .env("RUST_LOG", "warn")
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn serve");
    let stderr = child.stderr.take().expect("stderr");
    let mut key = None;
    for line in BufReader::new(stderr).lines() {
        let line = line.expect("read stderr");
        if let Some(k) = line.strip_prefix("demo renter API key: ") {
            key = Some(k.trim().to_owned());
            break;
        }
    }
    let api = format!("http://127.0.0.1:{port}");
    let deadline = Instant::now() + Duration::from_secs(20);
    while std::net::TcpStream::connect(("127.0.0.1", port)).is_err() {
        assert!(Instant::now() < deadline, "serve never started listening");
        std::thread::sleep(Duration::from_millis(50));
    }
    Server { child, api, key: key.expect("serve printed no API key") }
}

fn cli(s: &Server, args: &[&str]) -> Result<Value, String> {
    cli_as(&s.api, &s.key, args)
}

fn cli_as(api: &str, key: &str, args: &[&str]) -> Result<Value, String> {
    let out = Command::new(BIN).args(["--api", api, "--key", key]).args(args).output().expect("run cli");
    if !out.status.success() {
        return Err(String::from_utf8_lossy(&out.stderr).into_owned());
    }
    serde_json::from_slice(&out.stdout)
        .map_err(|e| format!("stdout is not JSON ({e}): {}", String::from_utf8_lossy(&out.stdout)))
}

#[test]
fn agent_workflow_through_the_cli() {
    let s = serve();

    let offers = cli(&s, &["offers", "--sort", "price"]).expect("offers");
    let offers = offers.as_array().expect("offer list");
    assert!(!offers.is_empty());
    let prices: Vec<i64> = offers.iter().map(|o| o["pricePerSec"].as_i64().expect("price")).collect();
    assert!(prices.windows(2).all(|w| w[0] <= w[1]), "sorted by price: {prices:?}");
    let gpu = cli(&s, &["offers", "--accelerator", "gpu"]).expect("gpu offers");
    assert!(gpu.as_array().expect("list").iter().all(|o| o["accelerator"] == "gpu"));

    let before = cli(&s, &["account"]).expect("account")["balance"].as_i64().expect("balance");
    let offer = offers[0]["id"].as_str().expect("offer id");
    let inst = cli(&s, &["deploy", offer, "--vcpus", "1", "--mem-mib", "1024", "--disk-gib", "10"]).expect("deploy");
    let id = inst["id"].as_str().expect("instance id").to_owned();
    assert_eq!(inst["state"], "running");
    assert!(inst["virtualIp"].as_str().expect("vip").starts_with("10.147."));

    let listed = cli(&s, &["status"]).expect("status");
    assert!(listed.as_array().expect("list").iter().any(|i| i["id"] == id.as_str()));

    // Let per-second billing settle at least once.
    std::thread::sleep(Duration::from_millis(2500));
    assert_eq!(cli(&s, &["scale", &id, "0"]).expect("scale 0")["state"], "scaledToZero");
    let after = cli(&s, &["account"]).expect("account");
    assert!(after["balance"].as_i64().expect("balance") < before, "usage was billed");
    assert_eq!(cli(&s, &["scale", &id, "1"]).expect("scale 1")["state"], "running");
    assert_eq!(cli(&s, &["terminate", &id]).expect("terminate")["state"], "terminated");
    assert!(cli(&s, &["scale", &id, "1"]).is_err(), "a terminated instance cannot be resumed");
}

#[test]
fn rejects_bad_credentials_and_input() {
    let s = serve();
    let err = cli_as(&s.api, "wrong", &["account"]).expect_err("bad key must fail");
    assert!(err.contains("401"), "{err}");
    let err = cli(&s, &["deploy", "no-such-offer"]).expect_err("unknown offer");
    assert!(err.contains("404"), "{err}");
    let err = cli(&s, &["status", "00000000-0000-0000-0000-000000000000"]).expect_err("unknown instance");
    assert!(err.contains("404"), "{err}");
}

#[cfg(not(target_os = "linux"))]
#[test]
fn firecracker_is_refused_off_linux() {
    let out = Command::new(BIN).args(["serve", "--hypervisor", "firecracker"]).output().expect("run");
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("needs Linux"));
}
