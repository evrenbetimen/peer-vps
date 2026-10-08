//! Host telemetry sampler, backed by `sysinfo` so the same code reports real
//! CPU, memory, network and temperature figures on Linux and macOS.

use std::time::Duration;

use peervps_core::events::HostMetrics;
use peervps_core::storage::{now_secs, presence};
use peervps_core::{Node, NodeEvent};
use sysinfo::{Components, Networks, System};

const PERIOD: Duration = Duration::from_millis(250);

pub async fn run(node: Node, provider: String) {
    let mut tick = tokio::time::interval(PERIOD);
    let mut probes = Probes::new();
    let mut last_net = probes.net();
    loop {
        tick.tick().await;
        let net = probes.net();
        let secs = PERIOD.as_secs_f64();
        let rx_bps = (net.0.saturating_sub(last_net.0) as f64 / secs) as u64;
        let tx_bps = (net.1.saturating_sub(last_net.1) as f64 / secs) as u64;
        last_net = net;
        let (mem_total, mem_avail) = probes.mem();
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
            cpu_load_pct: probes.cpu_load(),
            cpu_temp_c: probes.temp().unwrap_or(0.0),
            mem_used_mib: mem_total.saturating_sub(mem_avail),
            mem_total_mib: mem_total,
            running_vms: node.running_vms().await as u32,
            sla_pct: sla,
            net_rx_bps: rx_bps,
            net_tx_bps: tx_bps,
        }));
    }
}

/// Cross-platform probes (Linux `/proc`, macOS `host_statistics`/IOKit) via `sysinfo`.
struct Probes {
    sys: System,
    nets: Networks,
    comps: Components,
}

impl Probes {
    fn new() -> Self {
        Self {
            sys: System::new(),
            nets: Networks::new_with_refreshed_list(),
            comps: Components::new_with_refreshed_list(),
        }
    }

    fn cpu_load(&mut self) -> f32 {
        self.sys.refresh_cpu_usage();
        self.sys.global_cpu_usage().clamp(0.0, 100.0)
    }

    /// Hottest CPU-ish sensor; `None` where the OS exposes none (VMs, some Macs without privileges).
    fn temp(&mut self) -> Option<f32> {
        self.comps.refresh(false);
        self.comps.iter().filter_map(|c| c.temperature()).filter(|t| t.is_finite() && *t > 0.0).reduce(f32::max)
    }

    /// (total MiB, available MiB)
    fn mem(&mut self) -> (u64, u64) {
        self.sys.refresh_memory();
        (self.sys.total_memory() / MIB, self.sys.available_memory() / MIB)
    }

    /// Sum of (rx bytes, tx bytes) since boot over non-loopback interfaces.
    fn net(&mut self) -> (u64, u64) {
        self.nets.refresh(true);
        self.nets
            .iter()
            .filter(|(name, _)| !is_loopback(name))
            .fold((0, 0), |(rx, tx), (_, d)| (rx + d.total_received(), tx + d.total_transmitted()))
    }
}

const MIB: u64 = 1024 * 1024;

fn is_loopback(name: &str) -> bool {
    name == "lo" || name == "lo0"
}

/// Physical RAM in MiB, for the allocation sliders.
pub fn host_mem_mib() -> u64 {
    let mut sys = System::new();
    sys.refresh_memory();
    match sys.total_memory() / MIB {
        0 => 16 * 1024,
        n => n,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn probes_report_plausible_values() {
        let mut p = Probes::new();
        let (total, avail) = p.mem();
        assert!(total > 0 && avail <= total, "{total} {avail}");
        assert!((0.0..=100.0).contains(&p.cpu_load()));
        let _ = p.net();
        assert!(host_mem_mib() > 0);
        assert!(is_loopback("lo") && is_loopback("lo0") && !is_loopback("en0") && !is_loopback("eth0"));
    }
}
