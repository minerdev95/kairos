//! Self-onboarding and auto-benchmarking — section 21's "zero configuration".
//!
//! From only payout wallets and one risk word, KAIROS detects every device,
//! benchmarks each device on each algorithm it can run (hashrate + efficiency),
//! seeds the autotuner, and reaches a running, optimized state. No coin, pool, or
//! clock is ever asked of the operator.

use crate::intelligence::EfficiencyOracle;
use crate::model::*;
use std::collections::BTreeMap;

#[derive(Clone, Debug)]
pub struct BenchEntry {
    pub device: DeviceId,
    pub site: SiteId,
    pub class: DeviceClass,
    pub algo: AlgorithmId,
    pub hashrate: f64,
    pub power_w: f64,
    pub joules_per_h: f64,
}

#[derive(Clone, Debug, Default)]
pub struct BenchmarkReport {
    pub entries: Vec<BenchEntry>,
}

impl BenchmarkReport {
    pub fn device_count(&self) -> usize {
        self.entries
            .iter()
            .map(|e| e.device.clone())
            .collect::<std::collections::BTreeSet<_>>()
            .len()
    }

    pub fn total_stock_power_w(&self, profiles: &BTreeMap<DeviceId, DeviceProfile>) -> f64 {
        // Best (lowest J/H) algo per device, summed — a planning estimate.
        let mut by_dev: BTreeMap<DeviceId, f64> = BTreeMap::new();
        for e in &self.entries {
            by_dev
                .entry(e.device.clone())
                .and_modify(|p| {
                    if e.power_w < *p {
                        *p = e.power_w
                    }
                })
                .or_insert(e.power_w);
        }
        let _ = profiles;
        by_dev.values().sum()
    }

    pub fn per_site_stock_power_w(&self) -> BTreeMap<SiteId, f64> {
        // Use each device's *highest*-power algorithm so an auto power cap sized
        // off this never binds spuriously when the switcher picks a hotter coin.
        let mut best: BTreeMap<DeviceId, (SiteId, f64)> = BTreeMap::new();
        for e in &self.entries {
            best.entry(e.device.clone())
                .and_modify(|(_, p)| {
                    if e.power_w > *p {
                        *p = e.power_w
                    }
                })
                .or_insert((e.site.clone(), e.power_w));
        }
        let mut out: BTreeMap<SiteId, f64> = BTreeMap::new();
        for (_, (site, p)) in best {
            *out.entry(site).or_insert(0.0) += p;
        }
        out
    }
}

/// Benchmark every device on every algorithm it supports, at stock clocks.
pub fn auto_benchmark(
    profiles: &BTreeMap<DeviceId, DeviceProfile>,
    oracle: &dyn EfficiencyOracle,
    ambient_c: f64,
) -> BenchmarkReport {
    let mut report = BenchmarkReport::default();
    for (id, dev) in profiles {
        let temp = ambient_c + 25.0;
        for cap in &dev.capabilities {
            let knobs = Knobs::stock(cap.stock_power_w);
            let eff = oracle.efficiency(id, &cap.algo, &knobs, temp);
            report.entries.push(BenchEntry {
                device: id.clone(),
                site: dev.site.clone(),
                class: dev.class,
                algo: cap.algo.clone(),
                hashrate: eff.hashrate,
                power_w: eff.power_w,
                joules_per_h: eff.joules_per_hash(),
            });
        }
    }
    report
}
