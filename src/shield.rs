//! The safety envelope — section 13. The shield wraps the learned policy and
//! enforces hard limits: power, thermal, voltage, clock ceilings, fan floor. If
//! the policy proposes any action that would violate a limit, the shield
//! overrides it with the nearest safe action. The intelligence proposes, the
//! shield disposes — no policy, however trained, can drive hardware past a hard
//! boundary. Thermal protection overrides every other goal, always.

use crate::model::*;
use std::collections::BTreeMap;

#[derive(Clone, Debug)]
pub struct ShieldEvent {
    pub device: DeviceId,
    pub reason: String,
}

#[derive(Clone, Debug, Default)]
pub struct Shield;

impl Shield {
    /// Clamp one setpoint to a device's hard limits, returning the safe setpoint
    /// and any events. `temp_c` is the live temperature for thermal protection.
    pub fn filter_setpoint(
        sp: &DeviceSetpoint,
        dev: &DeviceProfile,
        temp_c: f64,
    ) -> (DeviceSetpoint, Vec<ShieldEvent>) {
        let lim = dev.limits;
        let mut events = Vec::new();
        let mut knobs = sp.knobs;
        let mut assignment = sp.assignment.clone();

        // Clock + voltage ceilings.
        if knobs.core_offset_mhz > lim.max_core_offset_mhz {
            events.push(ev(&dev.id, format!(
                "core offset {:.0}>{:.0}MHz clamped",
                knobs.core_offset_mhz, lim.max_core_offset_mhz
            )));
            knobs.core_offset_mhz = lim.max_core_offset_mhz;
        }
        if knobs.mem_offset_mhz > lim.max_mem_offset_mhz {
            events.push(ev(&dev.id, format!(
                "mem offset {:.0}>{:.0}MHz clamped",
                knobs.mem_offset_mhz, lim.max_mem_offset_mhz
            )));
            knobs.mem_offset_mhz = lim.max_mem_offset_mhz;
        }
        if knobs.core_voltage_mv > lim.max_core_voltage_mv {
            events.push(ev(&dev.id, format!(
                "voltage {:.0}>{:.0}mV clamped",
                knobs.core_voltage_mv, lim.max_core_voltage_mv
            )));
            knobs.core_voltage_mv = lim.max_core_voltage_mv;
        }
        // Power ceiling.
        if knobs.power_limit_w > lim.max_power_w {
            events.push(ev(&dev.id, format!(
                "power {:.0}>{:.0}W clamped",
                knobs.power_limit_w, lim.max_power_w
            )));
            knobs.power_limit_w = lim.max_power_w;
        }
        // Fan floor.
        if knobs.fan_pct < lim.min_fan_pct {
            knobs.fan_pct = lim.min_fan_pct;
        }

        // Thermal protection overrides everything: at/over the ceiling, force the
        // device idle (assignment cleared) and fans to max. This dominates yield.
        if temp_c >= lim.max_temp_c {
            events.push(ev(&dev.id, format!(
                "THERMAL {:.0}C>={:.0}C — forced idle, fans max",
                temp_c, lim.max_temp_c
            )));
            assignment = None;
            knobs.power_limit_w = STANDBY_W;
            knobs.fan_pct = 100.0;
            knobs.core_offset_mhz = 0.0;
            knobs.mem_offset_mhz = 0.0;
        }

        (
            DeviceSetpoint {
                device: sp.device.clone(),
                assignment,
                knobs,
            },
            events,
        )
    }

    /// Filter a whole fleet action. Pure: produces a new, safe action.
    pub fn filter(
        &self,
        action: &FleetAction,
        devices: &BTreeMap<DeviceId, DeviceProfile>,
        belief: &Belief,
    ) -> (FleetAction, Vec<ShieldEvent>) {
        let mut out = Vec::with_capacity(action.setpoints.len());
        let mut events = Vec::new();
        for sp in &action.setpoints {
            if let Some(dev) = devices.get(&sp.device) {
                let temp = belief
                    .devices
                    .get(&sp.device)
                    .map(|t| t.temp_c)
                    .unwrap_or(belief.ambient_c + 25.0);
                let (safe, mut evs) = Self::filter_setpoint(sp, dev, temp);
                events.append(&mut evs);
                out.push(safe);
            } else {
                // Fail CLOSED: a setpoint for a device the shield has no limits for
                // is forced to safe standby idle and logged, never passed through.
                events.push(ev(
                    &sp.device,
                    "unknown device — forced safe idle (fail-closed)".into(),
                ));
                out.push(DeviceSetpoint {
                    device: sp.device.clone(),
                    assignment: None,
                    knobs: Knobs::stock(STANDBY_W),
                });
            }
        }
        (
            FleetAction {
                setpoints: out,
                policy: action.policy.clone(),
            },
            events,
        )
    }

    /// True if a setpoint would violate any hard limit (used by tests + the
    /// runtime's permit check).
    pub fn would_violate(sp: &DeviceSetpoint, dev: &DeviceProfile, temp_c: f64) -> bool {
        let lim = dev.limits;
        sp.knobs.power_limit_w > lim.max_power_w
            || sp.knobs.core_voltage_mv > lim.max_core_voltage_mv
            || sp.knobs.core_offset_mhz > lim.max_core_offset_mhz
            || sp.knobs.mem_offset_mhz > lim.max_mem_offset_mhz
            || (temp_c >= lim.max_temp_c && sp.assignment.is_some())
    }
}

fn ev(device: &DeviceId, reason: String) -> ShieldEvent {
    ShieldEvent {
        device: device.clone(),
        reason,
    }
}
