//! Hardware Abstraction Layer — the [`ActuationSurface`] contract.
//!
//! The policy never touches a device directly. A device is anything that
//! implements this trait: a native kernel driver, a wrapped external miner, or
//! the digital twin's simulated silicon. The twin implements the *same* trait so
//! a policy cannot tell simulation from reality (the Sim2Real guarantee).

use crate::model::*;
use anyhow::Result;

/// What a device produces at a given operating point. On real hardware this is a
/// *learned surrogate* fit from telemetry; in the twin it is ground-truth physics.
#[derive(Clone, Copy, Debug)]
pub struct EfficiencyPoint {
    pub hashrate: f64,
    pub power_w: f64,
    /// Hardware error rate (invalid-nonce fraction) at this point — the signal
    /// the autotuner backs off on when it pushes past the stable edge.
    pub hw_error_rate: f64,
}

impl EfficiencyPoint {
    /// Joules per hash. The unit the thermodynamic ledger prices everything in.
    pub fn joules_per_hash(&self) -> f64 {
        if self.hashrate <= 0.0 {
            f64::INFINITY
        } else {
            self.power_w / self.hashrate
        }
    }
}

/// Incremental, irreversible damage from holding an operating point for `dt`.
#[derive(Clone, Copy, Debug, Default)]
pub struct DamageDelta {
    /// Lifetime fraction consumed in this interval, [0,1].
    pub consumed: f64,
    /// Dollar value of lifespan consumed (degradation-pricing input).
    pub usd_cost: f64,
}

/// The device contract. `&mut self` only on `apply`, which mutates the live
/// operating point; everything else is an observation.
pub trait ActuationSurface {
    fn profile(&self) -> &DeviceProfile;

    /// Algorithms this device can serve (its feasible-geometry generator).
    fn algo_capabilities(&self) -> &[AlgoCapability] {
        &self.profile().capabilities
    }

    /// Predicted output at an operating point. Queried by the autotuner to walk
    /// the per-algo frontier and by the opportunity scorer to price candidates.
    fn efficiency_model(&self, algo: &AlgorithmId, knobs: &Knobs, temp_c: f64) -> EfficiencyPoint;

    /// Degradation incurred by holding `op` for `dt_s` seconds, in lifetime
    /// fraction *and* dollars. The price tag the tuner trades clock against.
    fn degradation_model(&self, op: &OperatingPoint, dt_s: f64) -> DamageDelta;

    fn health(&self) -> HealthTelemetry;

    fn backend(&self) -> ExecutionBackend;

    /// Commit a setpoint to the device. The shield has already filtered it.
    fn apply(&mut self, setpoint: &DeviceSetpoint) -> Result<()>;

    /// Live measured telemetry.
    fn telemetry(&self) -> DeviceTelemetry;

    fn id(&self) -> DeviceId {
        self.profile().id.clone()
    }
}
