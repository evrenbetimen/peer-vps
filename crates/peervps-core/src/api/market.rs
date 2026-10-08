//! Marketplace offers and the query filter agents use to pick one.

use serde::{Deserialize, Serialize};

use crate::virtualization::accel::AcceleratorKind;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Offer {
    pub id: String,
    pub provider: String,
    pub region: String,
    pub vcpus: u32,
    pub mem_mib: u64,
    pub disk_gib: u64,
    pub accelerator: AcceleratorKind,
    pub accelerator_model: Option<String>,
    pub vram_mib: u64,
    /// µcredits per second.
    pub price_per_sec: i64,
    /// Rolling 30-day uptime from the presence log.
    pub sla_pct: f64,
    pub confidential: bool,
    pub collateral_locked: i64,
}

#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OfferQuery {
    pub min_vram_mib: Option<u64>,
    /// µcredits per hour, which is how humans and agents usually budget.
    pub max_price_per_hour: Option<i64>,
    pub min_sla_pct: Option<f64>,
    pub accelerator: Option<AcceleratorKind>,
    pub confidential: Option<bool>,
    pub min_vcpus: Option<u32>,
    pub min_mem_mib: Option<u64>,
    /// `price` (default), `vram`, or `sla`.
    pub sort: Option<String>,
    pub limit: Option<usize>,
}

impl OfferQuery {
    pub fn matches(&self, o: &Offer) -> bool {
        self.min_vram_mib.is_none_or(|v| o.vram_mib >= v)
            && self.max_price_per_hour.is_none_or(|p| o.price_per_sec.saturating_mul(3600) <= p)
            && self.min_sla_pct.is_none_or(|s| o.sla_pct >= s)
            && self.accelerator.is_none_or(|a| o.accelerator == a)
            && self.confidential.is_none_or(|c| !c || o.confidential)
            && self.min_vcpus.is_none_or(|c| o.vcpus >= c)
            && self.min_mem_mib.is_none_or(|m| o.mem_mib >= m)
    }

    pub fn apply(&self, offers: &[Offer]) -> Vec<Offer> {
        let mut out: Vec<Offer> = offers.iter().filter(|o| self.matches(o)).cloned().collect();
        match self.sort.as_deref() {
            Some("vram") => out.sort_by_key(|o| std::cmp::Reverse(o.vram_mib)),
            Some("sla") => out.sort_by(|a, b| b.sla_pct.total_cmp(&a.sla_pct)),
            _ => out.sort_by_key(|o| o.price_per_sec),
        }
        out.truncate(self.limit.unwrap_or(100).min(500));
        out
    }
}

/// A few offers so a fresh node, the CLI and the desktop app have something to show.
pub fn demo_offers() -> Vec<Offer> {
    let mk = |id: &str,
              region: &str,
              vcpus,
              mem_gib: u64,
              kind,
              model: Option<&str>,
              vram_gib: u64,
              cents_hr: i64,
              sla,
              conf| Offer {
        id: id.into(),
        provider: format!("node-{id}"),
        region: region.into(),
        vcpus,
        mem_mib: mem_gib * 1024,
        disk_gib: 200,
        accelerator: kind,
        accelerator_model: model.map(Into::into),
        vram_mib: vram_gib * 1024,
        // 1 credit == 1 USD-cent equivalent in the demo price list.
        price_per_sec: cents_hr * 1_000_000 / 3600,
        sla_pct: sla,
        confidential: conf,
        collateral_locked: 500_000_000,
    };
    vec![
        mk("fra-cpu-1", "eu-central", 4, 4, AcceleratorKind::None, None, 0, 2, 99.95, false),
        mk("ams-4090-2", "eu-west", 8, 32, AcceleratorKind::Gpu, Some("RTX 4090 (½)"), 12, 18, 99.7, false),
        mk("iad-h100-3", "us-east", 16, 128, AcceleratorKind::Gpu, Some("H100 MIG 3g.40gb"), 40, 95, 99.9, true),
        mk("sfo-npu-4", "us-west", 8, 16, AcceleratorKind::Npu, Some("Edge NPU x2"), 8, 9, 99.2, false),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn filters_by_vram_and_hourly_price() {
        let q = OfferQuery {
            min_vram_mib: Some(10 * 1024),
            max_price_per_hour: Some(50 * 1_000_000),
            ..Default::default()
        };
        let ids: Vec<String> = q.apply(&demo_offers()).into_iter().map(|o| o.id).collect();
        assert_eq!(ids, vec!["ams-4090-2"]);
    }
}
