//! Self-healing reliability subsystem — section 15.
//!
//! Reliability is a term in the objective, not an afterthought. The watchdog
//! restarts failed components, fails over to backup pools, and rolls back any
//! setpoint that produces instability or excess rejects. At scale, uptime is the
//! dominant profit lever, so this never trades a sliver of yield for instability.

use crate::cal::Market;
use crate::model::*;
use std::collections::BTreeMap;

#[derive(Clone, Copy, Debug)]
pub struct HealParams {
    /// Reject rate above which we roll back the setpoint to stock + failover.
    pub reject_rollback_threshold: f64,
    /// Hardware-error rate above which we treat it as instability.
    pub hw_error_threshold: f64,
}

impl Default for HealParams {
    fn default() -> Self {
        HealParams {
            reject_rollback_threshold: 0.03,
            hw_error_threshold: 0.02,
        }
    }
}

#[derive(Clone, Debug)]
pub struct HealEvent {
    pub device: DeviceId,
    pub action: String,
}

#[derive(Clone, Debug, Default)]
pub struct SelfHealer {
    params: HealParams,
    /// Devices currently being restarted (idled this tick, resume next).
    restarting: BTreeMap<DeviceId, u32>,
}

impl SelfHealer {
    pub fn new(params: HealParams) -> Self {
        SelfHealer {
            params,
            restarting: BTreeMap::new(),
        }
    }

    /// Inspect telemetry and patch the policy's action in place: restart faulted
    /// devices, roll back unstable setpoints, fail over pools. Runs *before* the
    /// shield, which gets the final hard-limit veto, so any recovery action is
    /// itself re-checked against the safety envelope. Returns events for the
    /// decision feed; corruption faults are also reported back to the autotuner.
    pub fn heal(
        &mut self,
        action: &mut FleetAction,
        belief: &Belief,
        devices: &BTreeMap<DeviceId, DeviceProfile>,
        market: &Market,
    ) -> Vec<HealEvent> {
        let mut events = Vec::new();
        for sp in action.setpoints.iter_mut() {
            let tel = match belief.devices.get(&sp.device) {
                Some(t) => t,
                None => continue,
            };
            if !devices.contains_key(&sp.device) {
                continue;
            }

            // 1) Faulted or offline → restart: idle for one tick, then resume.
            if tel.fault.is_some() || !tel.online {
                let n = self.restarting.entry(sp.device.clone()).or_insert(0);
                *n += 1;
                sp.assignment = None;
                sp.knobs = Knobs::stock(STANDBY_W);
                events.push(HealEvent {
                    device: sp.device.clone(),
                    action: format!(
                        "restart attempt {} ({})",
                        n,
                        tel.fault.clone().unwrap_or_else(|| "offline".into())
                    ),
                });
                continue;
            } else {
                self.restarting.remove(&sp.device);
            }

            // 2) Excess rejects or HW errors → roll back to stock + fail over pool.
            if tel.reject_rate > self.params.reject_rollback_threshold
                || tel.hw_error_rate > self.params.hw_error_threshold
            {
                if let Some(asg) = sp.assignment.clone() {
                    // Roll back tuning to stock (safe).
                    sp.knobs = Knobs::stock(sp.knobs.power_limit_w);
                    // Warm failover: pick a different pool for the same coin.
                    let pools = market.pools_for_coin(&asg.coin);
                    let failover = pools
                        .iter()
                        .find(|p| p.id != asg.pool)
                        .map(|p| p.id.clone());
                    let mut note = "rollback setpoint to stock".to_string();
                    if let Some(newp) = failover {
                        note = format!("rollback to stock + failover pool {}->{}", asg.pool, newp);
                        sp.assignment = Some(Assignment {
                            algo: asg.algo,
                            coin: asg.coin,
                            pool: newp,
                            secondary: asg.secondary,
                        });
                    }
                    events.push(HealEvent {
                        device: sp.device.clone(),
                        action: format!(
                            "{} (rejects {:.1}%, hw-err {:.1}%)",
                            note,
                            tel.reject_rate * 100.0,
                            tel.hw_error_rate * 100.0
                        ),
                    });
                }
            }
        }
        events
    }
}
