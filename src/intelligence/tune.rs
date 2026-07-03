//! Per-chip, per-algorithm degradation-priced autotuning.
//!
//! Finds each chip's *stable edge* per algorithm by conservative-by-construction
//! edge-seeking: push the bound subsystem (memory for memory-bound coins, core
//! for compute-bound ones — clock asymmetry), watch an independent validity
//! signal, and back off. Two non-negotiables from the design critique:
//!
//!  * The error signal is *lagging and non-monotone*, so backoff is **asymmetric**
//!    — nudge up slowly, and on any corruption-class fault drop the per-chip
//!    ceiling **permanently** (it is never EWMA'd back up).
//!  * Absolute datasheet ceilings ([`DeviceLimits`]) are hard; the learner can
//!    never cross them. The shield enforces them again after the fact.
//!
//! Lifespan is priced as dollars-per-second on the same ledger as energy and
//! revenue, so the marginal rule is: raise clock while the marginal revenue gain
//! exceeds the marginal energy **plus** the marginal degradation cost.

use crate::model::*;
use std::collections::BTreeMap;

/// Tuning knobs that bound the search. Conservative defaults.
#[derive(Clone, Copy, Debug)]
pub struct TuneParams {
    /// Error rate above which we back off immediately.
    pub error_backoff_threshold: f64,
    /// MHz step when pushing an edge up.
    pub probe_step_mhz: f64,
    /// MHz to retreat on a soft error.
    pub backoff_step_mhz: f64,
    /// Permanent ceiling drop (MHz) on a corruption-class fault.
    pub fault_ceiling_drop_mhz: f64,
    /// EWMA factor for the error-rate estimate.
    pub error_ewma_alpha: f64,
    /// Minimum ticks between upward probes on one chip (round-robin in caller).
    pub probe_cooldown_ticks: u64,
}

impl Default for TuneParams {
    fn default() -> Self {
        TuneParams {
            error_backoff_threshold: 0.005, // 0.5% invalid nonces
            probe_step_mhz: 15.0,
            backoff_step_mhz: 45.0,
            fault_ceiling_drop_mhz: 60.0,
            error_ewma_alpha: 0.3,
            probe_cooldown_ticks: 6,
        }
    }
}

/// Per (device, algorithm) learned operating edge.
#[derive(Clone, Debug)]
pub struct EdgeState {
    pub core_edge_mhz: f64,
    pub mem_edge_mhz: f64,
    pub volt_mv: f64,
    /// Hard learned ceilings — only ever lowered (after a corruption fault).
    pub core_ceiling_mhz: f64,
    pub mem_ceiling_mhz: f64,
    pub error_ewma: f64,
    pub last_probe_tick: u64,
    pub faults: u32,
}

impl EdgeState {
    fn new(limits: &DeviceLimits) -> Self {
        EdgeState {
            core_edge_mhz: 0.0,
            mem_edge_mhz: 0.0,
            volt_mv: 0.0,
            core_ceiling_mhz: limits.max_core_offset_mhz,
            mem_ceiling_mhz: limits.max_mem_offset_mhz,
            error_ewma: 0.0,
            last_probe_tick: 0,
            faults: 0,
        }
    }
}

/// The autotuner's memory across the whole fleet.
#[derive(Clone, Debug, Default)]
pub struct TunerBook {
    states: BTreeMap<(DeviceId, AlgorithmId), EdgeState>,
    params: TuneParams,
}

impl TunerBook {
    pub fn new(params: TuneParams) -> Self {
        TunerBook {
            states: BTreeMap::new(),
            params,
        }
    }

    fn state_mut(&mut self, dev: &DeviceProfile, algo: &AlgorithmId) -> &mut EdgeState {
        self.states
            .entry((dev.id.clone(), algo.clone()))
            .or_insert_with(|| EdgeState::new(&dev.limits))
    }

    /// The knobs the tuner currently believes are this chip's stable edge for
    /// `algo`, at the supplied power limit. Clock asymmetry is applied from the
    /// algorithm's `mem_sensitivity`: memory-bound ⇒ push memory, relax core.
    pub fn propose(
        &mut self,
        dev: &DeviceProfile,
        algo_profile: &AlgoProfile,
        power_limit_w: f64,
    ) -> Knobs {
        let limits = dev.limits;
        let q = dev.silicon_quality;
        let st = self.state_mut(dev, &algo_profile.id);
        // Asymmetry: bias the offsets toward the bound subsystem.
        let bias = algo_profile.mem_sensitivity; // 1 = pure memory-bound
        let core = (st.core_edge_mhz * (1.0 - bias) * q)
            .clamp(-limits.max_core_offset_mhz, st.core_ceiling_mhz.min(limits.max_core_offset_mhz));
        let mem = (st.mem_edge_mhz * bias.max(0.15) * q)
            .clamp(-limits.max_mem_offset_mhz, st.mem_ceiling_mhz.min(limits.max_mem_offset_mhz));
        Knobs {
            core_offset_mhz: core,
            mem_offset_mhz: mem,
            power_limit_w: power_limit_w.min(limits.max_power_w),
            core_voltage_mv: st.volt_mv.min(limits.max_core_voltage_mv),
            fan_pct: 60.0,
        }
    }

    /// Stock knobs (no tuning) — the counterfactual the tuning credit is measured
    /// against, and the safe fallback.
    pub fn stock(dev: &DeviceProfile, algo: &AlgorithmId, power_limit_w: f64) -> Knobs {
        let _ = algo;
        Knobs::stock(power_limit_w.min(dev.limits.max_power_w))
    }

    /// Feed back realized telemetry after actuation so the edge adapts. `fault`
    /// true ⇒ a corruption-class event: drop the ceiling permanently.
    pub fn observe(
        &mut self,
        dev: &DeviceProfile,
        algo_profile: &AlgoProfile,
        error_rate: f64,
        fault: bool,
        tick: u64,
    ) {
        let p = self.params;
        let limits = dev.limits;
        let st = self.state_mut(dev, &algo_profile.id);
        st.error_ewma += p.error_ewma_alpha * (error_rate - st.error_ewma);

        if fault {
            // Permanent, asymmetric: never raised back up.
            st.faults += 1;
            st.core_ceiling_mhz = (st.core_ceiling_mhz - p.fault_ceiling_drop_mhz).max(0.0);
            st.mem_ceiling_mhz = (st.mem_ceiling_mhz - p.fault_ceiling_drop_mhz).max(0.0);
            st.core_edge_mhz = st.core_edge_mhz.min(st.core_ceiling_mhz - p.backoff_step_mhz);
            st.mem_edge_mhz = st.mem_edge_mhz.min(st.mem_ceiling_mhz - p.backoff_step_mhz);
            st.core_edge_mhz = st.core_edge_mhz.max(0.0);
            st.mem_edge_mhz = st.mem_edge_mhz.max(0.0);
            return;
        }

        if st.error_ewma > p.error_backoff_threshold {
            // Soft instability: retreat on the bound subsystem.
            if algo_profile.mem_sensitivity >= 0.5 {
                st.mem_edge_mhz = (st.mem_edge_mhz - p.backoff_step_mhz).max(0.0);
            } else {
                st.core_edge_mhz = (st.core_edge_mhz - p.backoff_step_mhz).max(0.0);
            }
            return;
        }

        // Stable and cooled down ⇒ probe one step further toward the edge.
        if tick.saturating_sub(st.last_probe_tick) >= p.probe_cooldown_ticks {
            st.last_probe_tick = tick;
            if algo_profile.mem_sensitivity >= 0.5 {
                let ceil = st.mem_ceiling_mhz.min(limits.max_mem_offset_mhz);
                st.mem_edge_mhz = (st.mem_edge_mhz + p.probe_step_mhz).min(ceil);
            } else {
                let ceil = st.core_ceiling_mhz.min(limits.max_core_offset_mhz);
                st.core_edge_mhz = (st.core_edge_mhz + p.probe_step_mhz).min(ceil);
            }
        }
    }

    pub fn edge(&self, dev: &DeviceId, algo: &AlgorithmId) -> Option<&EdgeState> {
        self.states.get(&(dev.clone(), algo.clone()))
    }
}
