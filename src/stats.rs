//! Fleet share accounting — the accepted/rejected counters professional miner
//! software shows per device and per pool, accumulated over the run.

use crate::model::*;
use serde::Serialize;
use std::collections::BTreeMap;

#[derive(Clone, Copy, Debug, Default, Serialize)]
pub struct ShareCount {
    pub accepted: u64,
    pub rejected: u64,
}

impl ShareCount {
    pub fn total(&self) -> u64 {
        self.accepted + self.rejected
    }
    pub fn reject_pct(&self) -> f64 {
        if self.total() == 0 {
            0.0
        } else {
            self.rejected as f64 / self.total() as f64 * 100.0
        }
    }
}

/// Cumulative share counters keyed by device and by pool.
#[derive(Clone, Debug, Default, Serialize)]
pub struct FleetStats {
    pub devices: BTreeMap<DeviceId, ShareCount>,
    pub pools: BTreeMap<PoolId, ShareCount>,
}

impl FleetStats {
    /// Accumulate one tick of shares from realized telemetry. Shares submitted ≈
    /// the vardiff target rate × dt; the realized reject rate splits them.
    pub fn record_tick(
        &mut self,
        action: &FleetAction,
        belief: &Belief,
        dt: f64,
        target_share_hz: f64,
    ) {
        for sp in &action.setpoints {
            let asg = match &sp.assignment {
                Some(a) => a,
                None => continue,
            };
            let tel = match belief.devices.get(&sp.device) {
                Some(t) => t,
                None => continue,
            };
            if !tel.online || tel.hashrate <= 0.0 {
                continue;
            }
            let shares = (target_share_hz * dt).round() as u64;
            if shares == 0 {
                continue;
            }
            let rej = (shares as f64 * tel.reject_rate).round() as u64;
            let acc = shares.saturating_sub(rej);
            let d = self.devices.entry(sp.device.clone()).or_default();
            d.accepted += acc;
            d.rejected += rej;
            let p = self.pools.entry(asg.pool.clone()).or_default();
            p.accepted += acc;
            p.rejected += rej;
        }
    }

    pub fn device(&self, id: &DeviceId) -> ShareCount {
        self.devices.get(id).copied().unwrap_or_default()
    }
    pub fn pool(&self, id: &PoolId) -> ShareCount {
        self.pools.get(id).copied().unwrap_or_default()
    }
}
