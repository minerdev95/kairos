//! Graceful degradation and tiered autonomy — section 14.
//!
//! The engine is software that can fail. If the learning brain becomes
//! unavailable, the real-time core falls back to a safe heuristic and keeps
//! mining rather than halting. The operator's hardware never stops earning
//! because an ML component hiccupped. Tiers: full intelligence → safe heuristic
//! → safe idle.

use crate::algo::AlgorithmRegistry;
use crate::cal::{Market, RewardGeometry};
use crate::intelligence::tune::TunerBook;
use crate::intelligence::EfficiencyOracle;
use crate::model::*;
use std::collections::BTreeMap;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum AutonomyTier {
    FullIntelligence,
    SafeHeuristic,
    SafeIdle,
}

impl AutonomyTier {
    pub fn label(self) -> &'static str {
        match self {
            AutonomyTier::FullIntelligence => "full-intelligence",
            AutonomyTier::SafeHeuristic => "safe-heuristic",
            AutonomyTier::SafeIdle => "safe-idle",
        }
    }
}

/// The brain-down fallback: stock clocks, best *spot*-margin coin per device,
/// run iff positive. Deliberately dumb, robust, and safe.
pub fn safe_heuristic(
    belief: &Belief,
    market: &Market,
    algos: &AlgorithmRegistry,
    devices: &BTreeMap<DeviceId, DeviceProfile>,
    oracle: &dyn EfficiencyOracle,
) -> FleetAction {
    let mut setpoints = Vec::new();
    for (dev_id, dev) in devices.iter() {
        let temp = belief
            .devices
            .get(dev_id)
            .map(|t| t.temp_c)
            .unwrap_or(belief.ambient_c + 25.0);
        let mut best: Option<(Assignment, Knobs, f64)> = None;
        for cap in &dev.capabilities {
            let algo = &cap.algo;
            if algos.get(algo).is_none() {
                continue;
            }
            for coin in market.coins.values().filter(|c| &c.algo == algo) {
                let cb = match belief.coin(&coin.id) {
                    Some(cb) => cb,
                    None => continue,
                };
                let pools = market.pools_for_coin(&coin.id);
                let pool = match pools.first() {
                    Some(p) => *p,
                    None => continue,
                };
                let geo = market.build_geometry(&coin.id, &pool.id).unwrap();
                let knobs = TunerBook::stock(dev, algo, cap.stock_power_w);
                let eff = oracle.efficiency(dev_id, algo, &knobs, temp);
                let rev = geo.net_of_pool_usd_per_s(eff.hashrate, cb.price_usd, cb.difficulty);
                let energy = eff.power_w * belief.energy_price_usd_kwh / 3_600_000.0;
                // A competent incumbent thermostat accounts for lifespan cost, not
                // just energy — otherwise it would run marginal devices hot at a
                // hidden loss.
                let op = OperatingPoint {
                    algo: algo.clone(),
                    knobs,
                    temp_c: temp,
                };
                let wear = oracle.degradation(dev_id, &op, 1.0).usd_cost;
                let net = rev - energy - wear;
                if best.as_ref().map(|b| net > b.2).unwrap_or(true) {
                    best = Some((
                        Assignment::primary(algo.clone(), coin.id.clone(), pool.id.clone()),
                        knobs,
                        net,
                    ));
                }
            }
        }
        match best {
            Some((asg, knobs, net)) if net > 0.0 => setpoints.push(DeviceSetpoint {
                device: dev_id.clone(),
                assignment: Some(asg),
                knobs,
            }),
            _ => setpoints.push(DeviceSetpoint {
                device: dev_id.clone(),
                assignment: None,
                knobs: Knobs::stock(STANDBY_W),
            }),
        }
    }
    FleetAction {
        setpoints,
        policy: "safe-heuristic".into(),
    }
}

/// Safe idle: everything off but thermally protected. The last resort.
pub fn safe_idle(devices: &BTreeMap<DeviceId, DeviceProfile>) -> FleetAction {
    FleetAction {
        setpoints: devices
            .iter()
            .map(|(id, _dev)| DeviceSetpoint {
                device: id.clone(),
                assignment: None,
                knobs: Knobs::stock(STANDBY_W),
            })
            .collect(),
        policy: "safe-idle".into(),
    }
}
