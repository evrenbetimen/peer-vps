//! Host telemetry sampler. Reads `/proc` and `/sys` on Linux; on other
//! platforms (until native probes land) it reports load-only estimates.

use std::time::Duration;

use peervps_core::events::HostMetrics;
use peervps_core::storage::{now_secs, presence};
use peervps_core::{Node, NodeEvent};

const PERIOD: Duration = Duration::from_millis(250);

pub async fn run(node: Node, provider: String) {
    let mut tick = tokio::time::interval(PERIOD);
    let mut last_net = read_net();
    loop {
        tick.tick().await;
        let net = read_net();
        let secs = PERIOD.as_secs_f64();
        let (rx_bps, tx_bps) = match (last_net, net) {
            (Some((r0, t0)), Some((r1, t1))) => {
                (((r1.saturating_sub(r0)) as f64 / secs) as u64, ((t1.saturating_sub(t0)) as f64 / secs) as u64)
            }
            _ => (0, 0),
        };
        last_net = net;
        let (mem_total, mem_avail) = read_mem().unwrap_or((16 * 1024, 8 * 1024));
        let p = provider.clone();
        // Rolling 30 days, or since the node was first seen if that is more recent.
        let sla = node
            .store
            .call(move |c| {
                let now = now_secs();
                let first: Option<i64> =
                    c.query_row("SELECT MIN(at) FROM presence WHERE node = ?1", [&p], |r| r.get(0))?;
                let since = first.unwrap_or(now).max(now - 30 * 86_400);
                presence::uptime_ratio(c, &p, since, now)
            })
            .await
            .map(|r| r * 100.0)
            .unwrap_or(0.0);
        node.events.publish(NodeEvent::Metrics(HostMetrics {
            cpu_load_pct: read_load().unwrap_or(0.0),
            cpu_temp_c: read_temp().unwrap_or(0.0),
            mem_used_mib: mem_total.saturating_sub(mem_avail),
            mem_total_mib: mem_total,
            running_vms: node.running_vms().await as u32,
            sla_pct: sla,
            net_rx_bps: rx_bps,
            net_tx_bps: tx_bps,
        }));
    }
}

fn read_load() -> Option<f32> {
    let s = std::fs::read_to_string("/proc/loadavg").ok()?;
    let one: f32 = s.split_whitespace().next()?.parse().ok()?;
    let cores = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1) as f32;
    Some((one / cores * 100.0).min(100.0))
}

fn read_temp() -> Option<f32> {
    let s = std::fs::read_to_string("/sys/class/thermal/thermal_zone0/temp").ok()?;
    Some(s.trim().parse::<f32>().ok()? / 1000.0)
}

/// (total MiB, available MiB)
fn read_mem() -> Option<(u64, u64)> {
    let s = std::fs::read_to_string("/proc/meminfo").ok()?;
    let field = |name: &str| -> Option<u64> {
        s.lines().find(|l| l.starts_with(name))?.split_whitespace().nth(1)?.parse::<u64>().ok().map(|kib| kib / 1024)
    };
    Some((field("MemTotal:")?, field("MemAvailable:")?))
}

/// Sum of (rx bytes, tx bytes) over non-loopback interfaces.
fn read_net() -> Option<(u64, u64)> {
    let s = std::fs::read_to_string("/proc/net/dev").ok()?;
    let mut rx = 0u64;
    let mut tx = 0u64;
    for line in s.lines().skip(2) {
        let (iface, rest) = line.split_once(':')?;
        if iface.trim() == "lo" {
            continue;
        }
        let cols: Vec<u64> = rest.split_whitespace().filter_map(|c| c.parse().ok()).collect();
        rx += cols.first().copied().unwrap_or(0);
        tx += cols.get(8).copied().unwrap_or(0);
    }
    Some((rx, tx))
}

/// Physical RAM in MiB, for the allocation sliders.
pub fn host_mem_mib() -> u64 {
    read_mem().map(|(t, _)| t).unwrap_or(16 * 1024)
}
