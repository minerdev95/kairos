//! Switching as optimal stopping, priced by the *full* round-trip switch cost.
//!
//! A device only changes assignment when the forward-integrated relative edge
//! clears a hysteresis band wider than the round-trip cost. The cost itemizes
//! lost in-flight speculative work, pool reconnect, the warm-up under-hashrate
//! ramp, idle dead-time, and — the key fix from the design critique — a
//! **per-event** Coffin-Manson thermal-cycle wear charge. Charging wear on the
//! *transition* (not per running watt) is what makes the controller willing to
//! hold a slightly-suboptimal coin rather than burn a needless thermal cycle.
//!
//! A minimum dwell time and a per-device daily turnover budget bound flapping.

use crate::model::*;
use std::collections::BTreeMap;

#[derive(Clone, Copy, Debug)]
pub struct SwitchParams {
    /// Seconds of revenue lost to pool reconnect + new-job latency.
    pub reconnect_s: f64,
    /// Seconds of idle dead-time during the swap.
    pub idle_s: f64,
    /// Seconds of unsubmitted speculative work lost.
    pub state_s: f64,
    /// Warm-up ramp seconds and the average under-hashrate fraction over it.
    pub warmup_s: f64,
    pub warmup_loss_frac: f64,
    /// Coffin-Manson exponent m for thermal-cycle fatigue.
    pub cm_exponent: f64,
    /// Temperature swing (°C) a switch actually induces.
    pub switch_delta_t_c: f64,
    /// Rated reference swing (°C) at which `cycle_capacity` was characterized.
    pub cycle_ref_delta_t_c: f64,
    /// Device thermal-cycle capacity (cycles to failure at the reference swing).
    pub cycle_capacity: f64,
    /// Assumed device replacement value (USD) for the wear charge.
    pub device_value_usd: f64,
    /// Hysteresis band multiplier on cost: ASIC heavier than GPU.
    pub band_mult_asic: f64,
    pub band_mult_gpu: f64,
    /// Minimum dwell in ticks before a device may switch again.
    pub dwell_ticks: u64,
    /// Max switches per device per day (turnover budget).
    pub daily_turnover_budget: u32,
}

impl Default for SwitchParams {
    fn default() -> Self {
        SwitchParams {
            reconnect_s: 5.0,
            idle_s: 2.0,
            state_s: 1.0,
            warmup_s: 25.0,
            warmup_loss_frac: 0.30,
            cm_exponent: 2.0,
            switch_delta_t_c: 15.0,
            cycle_ref_delta_t_c: 20.0,
            cycle_capacity: 250_000.0,
            device_value_usd: 1500.0,
            band_mult_asic: 1.5,
            band_mult_gpu: 1.15,
            dwell_ticks: 4,
            daily_turnover_budget: 24,
        }
    }
}

#[derive(Clone, Debug)]
struct DevSwitchState {
    current: Option<Assignment>,
    last_switch_tick: u64,
    switches_today: u32,
    day: i64,
    /// Knobs last committed (so we can keep an incumbent's tuning).
    knobs: Option<Knobs>,
}

impl Default for DevSwitchState {
    fn default() -> Self {
        DevSwitchState {
            current: None,
            last_switch_tick: 0,
            switches_today: 0,
            day: i64::MIN,
            knobs: None,
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct SwitchBook {
    params: SwitchParams,
    states: BTreeMap<DeviceId, DevSwitchState>,
}

impl SwitchBook {
    pub fn new(params: SwitchParams) -> Self {
        SwitchBook {
            params,
            states: BTreeMap::new(),
        }
    }

    pub fn params(&self) -> &SwitchParams {
        &self.params
    }

    pub fn incumbent(&self, dev: &DeviceId) -> Option<&Assignment> {
        self.states.get(dev).and_then(|s| s.current.as_ref())
    }

    pub fn incumbent_knobs(&self, dev: &DeviceId) -> Option<Knobs> {
        self.states.get(dev).and_then(|s| s.knobs)
    }

    /// Full round-trip switch cost in USD, given the incumbent and target
    /// revenue rates (USD/s) and the device class.
    pub fn switch_cost_usd(
        &self,
        r_incumbent: f64,
        r_target: f64,
        class: DeviceClass,
    ) -> f64 {
        let p = self.params;
        let revenue_loss = r_incumbent.max(0.0) * (p.reconnect_s + p.idle_s + p.state_s)
            + r_target.max(0.0) * p.warmup_loss_frac * p.warmup_s;
        // Per-event Coffin-Manson wear: the fraction of the device's thermal-cycle
        // budget consumed by one switch, scaled by (ΔT/ΔT_ref)^m so a hotter swing
        // costs super-linearly more lifespan.
        let severity = (p.switch_delta_t_c / p.cycle_ref_delta_t_c.max(1e-3)).powf(p.cm_exponent);
        let cycles_consumed = severity / p.cycle_capacity.max(1.0);
        let wear = p.device_value_usd * cycles_consumed;
        let _ = class;
        revenue_loss + wear
    }

    fn band_mult(&self, class: DeviceClass) -> f64 {
        match class {
            DeviceClass::Gpu => self.params.band_mult_gpu,
            DeviceClass::Asic | DeviceClass::Fpga => self.params.band_mult_asic,
        }
    }

    /// Optimal-stopping decision. `forward_edge_usd` is the forward-integrated
    /// *relative* gain (USD over the horizon) of the best candidate vs the
    /// incumbent. `emergency` bypasses hysteresis (regime break / safety force).
    pub fn should_switch(
        &self,
        dev: &DeviceProfile,
        forward_edge_usd: f64,
        switch_cost_usd: f64,
        tick: u64,
        emergency: bool,
    ) -> bool {
        let st = self.states.get(&dev.id);
        // No incumbent ⇒ free to assign (cold start, no thermal cycle).
        if st.map(|s| s.current.is_none()).unwrap_or(true) {
            return true;
        }
        if emergency {
            return true;
        }
        let dwell_ok = st
            .map(|s| tick.saturating_sub(s.last_switch_tick) >= self.params.dwell_ticks)
            .unwrap_or(true);
        let turnover_ok = st
            .map(|s| s.switches_today < self.params.daily_turnover_budget)
            .unwrap_or(true);
        let band = switch_cost_usd * self.band_mult(dev.class);
        dwell_ok && turnover_ok && forward_edge_usd > band
    }

    /// Commit the chosen assignment/knobs, updating dwell + turnover counters.
    pub fn commit(
        &mut self,
        dev: &DeviceId,
        assignment: Option<Assignment>,
        knobs: Option<Knobs>,
        tick: u64,
        t_secs: f64,
    ) {
        let day = (t_secs / 86_400.0).floor() as i64;
        let st = self.states.entry(dev.clone()).or_default();
        if st.day != day {
            st.day = day;
            st.switches_today = 0;
        }
        let changed = assignment_key(&st.current) != assignment_key(&assignment);
        if changed && st.current.is_some() {
            st.last_switch_tick = tick;
            st.switches_today += 1;
        }
        st.current = assignment;
        st.knobs = knobs;
    }
}

/// A coarse identity for an assignment so we can tell a real switch from a no-op.
fn assignment_key(a: &Option<Assignment>) -> Option<(String, String, String)> {
    a.as_ref().map(|x| {
        (
            x.algo.0.clone(),
            x.coin.0.clone(),
            x.pool.0.clone(),
        )
    })
}
