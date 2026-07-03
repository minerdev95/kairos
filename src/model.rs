//! Core data model — the shared vocabulary of KAIROS.
//!
//! Everything below is a plain value type. The policy never touches hardware or
//! a chain directly; it reasons over a [`Belief`] and emits a [`FleetAction`].
//! New chains/algorithms/hardware are implementations of the traits in
//! [`crate::cal`], [`crate::hal`], and [`crate::algo`] — never edits here.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fmt;

// ─────────────────────────────────────────────────────────────────────────────
// Identifiers (interned as strings, transparent in serde so they read cleanly)
// ─────────────────────────────────────────────────────────────────────────────

macro_rules! str_id {
    ($name:ident) => {
        #[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
        #[serde(transparent)]
        pub struct $name(pub String);
        impl $name {
            pub fn new(s: impl Into<String>) -> Self {
                Self(s.into())
            }
            pub fn as_str(&self) -> &str {
                &self.0
            }
        }
        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.pad(&self.0) // honor width/alignment in `{:<N}` formatting
            }
        }
        impl fmt::Debug for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, "{}({})", stringify!($name), self.0)
            }
        }
        impl From<&str> for $name {
            fn from(s: &str) -> Self {
                Self(s.to_string())
            }
        }
    };
}

str_id!(AlgorithmId);
str_id!(CoinId);
str_id!(PoolId);
str_id!(DeviceId);
str_id!(SiteId);

// ─────────────────────────────────────────────────────────────────────────────
// Enumerations
// ─────────────────────────────────────────────────────────────────────────────

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug, Serialize, Deserialize)]
pub enum DeviceClass {
    /// Fixed-function. Steerable only across coins *within* one algorithm.
    Asic,
    /// General purpose. Flexible capital steerable across the whole landscape.
    Gpu,
    /// Reconfigurable; treated like an ASIC with a (costly) bitstream knob.
    Fpga,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub enum ComputeProfile {
    MemoryBound,
    ComputeBound,
    Mixed,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub enum InclusionKind {
    Nakamoto,
    DagOrdered,
    ComputeMarket,
}

/// Pool reward scheme. Drives the variance and the operator-risk interaction.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub enum RewardScheme {
    /// Pay-per-share: zero variance for the miner, pool eats luck. Higher fee.
    Pps,
    /// Full-pay-per-share: PPS plus transaction-fee share.
    Fpps,
    /// Pay-per-last-N-shares: variance passed to the miner, lower fee, hop-sensitive.
    Pplns,
    /// Proportional per round.
    Prop,
    /// Solo: maximum variance, full reward on a found block.
    Solo,
}

impl RewardScheme {
    /// Relative payout-variance multiplier the miner bears (PPS≈0, Solo≈1).
    /// Used by the risk-aware pool router and the utility penalty.
    pub fn variance_factor(self) -> f64 {
        match self {
            RewardScheme::Pps => 0.02,
            RewardScheme::Fpps => 0.05,
            RewardScheme::Pplns => 0.35,
            RewardScheme::Prop => 0.45,
            RewardScheme::Solo => 1.0,
        }
    }
}

#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub enum ExecutionBackend {
    /// A KAIROS-native kernel (where we add value).
    Native,
    /// A wrapped best-in-class external miner binary (breadth).
    Wrapped(String),
}

// ─────────────────────────────────────────────────────────────────────────────
// Algorithm profile
// ─────────────────────────────────────────────────────────────────────────────

/// A registered algorithm: how it computes, how it is executed, how it tunes.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AlgoProfile {
    pub id: AlgorithmId,
    pub character: ComputeProfile,
    pub backend: ExecutionBackend,
    /// Where the stable edge tends to be: tuning pushes the bound subsystem and
    /// relaxes the other. `mem_sensitivity` in [0,1] (1 = pure memory-bound).
    pub mem_sensitivity: f64,
    /// Reference energy efficiency for a *nominal* device, joules per unit work
    /// (per hash). Per-device deviation is captured by [`DeviceProfile`].
    pub ref_efficiency_j_per_h: f64,
}

// ─────────────────────────────────────────────────────────────────────────────
// Coins / reward geometries
// ─────────────────────────────────────────────────────────────────────────────

/// The economic + consensus description of one coin (a reward geometry instance).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CoinDescriptor {
    pub id: CoinId,
    pub algo: AlgorithmId,
    pub inclusion: InclusionKind,
    /// Coins issued per block (excludes fees, which we add via `fee_per_block`).
    pub block_reward: f64,
    /// Average seconds between blocks the protocol targets.
    pub block_time_s: f64,
    /// Average transaction fees per block, in coin units.
    pub fee_per_block: f64,
    /// Difficulty retarget window, in seconds. A long window ⇒ a wide mispricing
    /// gap when price jumps (revenue moves now, difficulty lags this long).
    pub retarget_window_s: f64,
    /// Number of decimals (display only).
    pub decimals: u8,
}

impl CoinDescriptor {
    /// Network hashrate implied by a difficulty + block time, in H/s.
    pub fn implied_network_hashrate(&self, difficulty: f64) -> f64 {
        // diff * 2^32 / block_time for Bitcoin-like; we use a normalized model
        // where difficulty already encodes the work target. Keep it monotone.
        (difficulty / self.block_time_s).max(1.0)
    }
}

/// A pool serving a coin, with the qualities the router cares about plus the
/// stratum connection details an operator enters in real miner software.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PoolDescriptor {
    pub id: PoolId,
    pub coin: CoinId,
    pub scheme: RewardScheme,
    /// Pool fee as a fraction of reward, e.g. 0.01.
    pub fee_frac: f64,
    /// Static counterparty/payout-reliability risk, fraction expected to be lost
    /// to non-payment over the horizon (default-risk discount). Small.
    pub solvency_risk: f64,
    /// Stratum endpoint, e.g. `stratum+tcp://host:port` (display + connection).
    pub url: String,
    /// Worker/login username — typically `wallet.worker` (real-miner convention).
    #[serde(default)]
    pub user: String,
    /// Stratum password — usually `x` or a worker tag. Optional.
    #[serde(default)]
    pub pass: String,
    /// Failover priority; lower is primary. The router prefers low-priority pools
    /// of comparable risk-adjusted value and fails over when one drops.
    #[serde(default)]
    pub priority: u8,
}

// ─────────────────────────────────────────────────────────────────────────────
// Devices
// ─────────────────────────────────────────────────────────────────────────────

/// Tunable knobs on a device for a given algorithm. Offsets are relative to the
/// vendor-stock operating point; `power_limit_w` is absolute.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct Knobs {
    pub core_offset_mhz: f64,
    pub mem_offset_mhz: f64,
    pub power_limit_w: f64,
    pub core_voltage_mv: f64,
    pub fan_pct: f64,
}

impl Knobs {
    pub fn stock(power_limit_w: f64) -> Self {
        Knobs {
            core_offset_mhz: 0.0,
            mem_offset_mhz: 0.0,
            power_limit_w,
            core_voltage_mv: 0.0, // 0 ⇒ stock voltage
            fan_pct: 60.0,
        }
    }
}

/// Static per-device, per-algorithm capability: the shape of its frontier.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AlgoCapability {
    pub algo: AlgorithmId,
    /// Hashrate at the stock operating point, H/s.
    pub stock_hashrate: f64,
    /// Power at the stock operating point, W.
    pub stock_power_w: f64,
    /// Whether this device can dual-mine this algo with a compatible secondary.
    pub dual_capable: bool,
}

/// Identity + static description of a physical device.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DeviceProfile {
    pub id: DeviceId,
    pub site: SiteId,
    pub class: DeviceClass,
    pub model: String,
    /// The algorithms this device can serve (its feasible-geometry generator).
    pub capabilities: Vec<AlgoCapability>,
    /// Absolute hard ceilings for the shield (never exceeded, learned or not).
    pub limits: DeviceLimits,
    /// Per-unit silicon-quality multiplier in [~0.9, ~1.1]; >1 is a golden chip
    /// that holds a tighter clock. Drives per-chip tuning asymmetry.
    pub silicon_quality: f64,
}

impl DeviceProfile {
    pub fn capability(&self, algo: &AlgorithmId) -> Option<&AlgoCapability> {
        self.capabilities.iter().find(|c| &c.algo == algo)
    }
    pub fn supports(&self, algo: &AlgorithmId) -> bool {
        self.capability(algo).is_some()
    }
}

/// Hard limits enforced by the shield. These are *device safety*, not policy.
#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
pub struct DeviceLimits {
    pub max_power_w: f64,
    pub max_temp_c: f64,
    pub max_core_voltage_mv: f64,
    pub max_core_offset_mhz: f64,
    pub max_mem_offset_mhz: f64,
    pub min_fan_pct: f64,
}

impl Default for DeviceLimits {
    fn default() -> Self {
        DeviceLimits {
            max_power_w: 350.0,
            max_temp_c: 83.0,
            max_core_voltage_mv: 1100.0,
            max_core_offset_mhz: 300.0,
            max_mem_offset_mhz: 2000.0,
            min_fan_pct: 30.0,
        }
    }
}

/// Live, measured per-device state (from real telemetry or the twin).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DeviceTelemetry {
    pub id: DeviceId,
    pub online: bool,
    pub temp_c: f64,
    pub power_w: f64,
    /// Realized hashrate on the *currently assigned* algo, H/s.
    pub hashrate: f64,
    /// Fraction of submitted shares rejected/stale in the last window.
    pub reject_rate: f64,
    /// Hardware error rate (invalid nonces) — the autotuner's backoff signal.
    pub hw_error_rate: f64,
    pub fan_pct: f64,
    /// True once the watchdog has flagged this device as needing recovery.
    pub fault: Option<String>,
}

/// Slowly-evolving health signals for predictive maintenance (Tier 6 input).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct HealthTelemetry {
    pub id: DeviceId,
    /// Memory error trend (rising ⇒ failing VRAM/HBM).
    pub mem_error_trend: f64,
    /// Fan RPM as a fraction of nominal (falling ⇒ dying fan).
    pub fan_health: f64,
    /// Cumulative thermal-stress hours.
    pub thermal_hours: f64,
    /// Estimated remaining-useful-life fraction in [0,1].
    pub rul_frac: f64,
}

/// Per-chip accumulated, irreversible degradation (DamageState in the contract).
#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize)]
pub struct DamageState {
    /// Accumulated lifetime fraction consumed in [0,1]. 1.0 ⇒ end of life.
    pub consumed: f64,
    /// Dollar value of remaining life already amortized away.
    pub amortized_usd: f64,
}

/// An operating point: the thing whose degradation we price.
#[derive(Clone, Debug)]
pub struct OperatingPoint {
    pub algo: AlgorithmId,
    pub knobs: Knobs,
    pub temp_c: f64,
}

// ─────────────────────────────────────────────────────────────────────────────
// Belief — the only input the policy is allowed to read
// ─────────────────────────────────────────────────────────────────────────────

/// Estimated, uncertain state of one coin's economy.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CoinBelief {
    pub coin: CoinId,
    /// Current price in USD per coin.
    pub price_usd: f64,
    /// EWMA of recent price (the "fair value" the spike is measured against).
    pub price_anchor_usd: f64,
    /// Current network difficulty (normalized work units).
    pub difficulty: f64,
    /// Estimated network hashrate, H/s.
    pub network_hashrate: f64,
    /// Our forecast of the difficulty path over the planning horizon. Indexed by
    /// horizon step; `forward_difficulty[0]` is the next step. See `forecast`.
    pub forward_difficulty: Vec<f64>,
    /// Estimated standard deviation of next-step coin revenue (for risk sizing).
    pub revenue_vol: f64,
    /// Relative price volatility (std of log returns) — the scale-free risk input.
    pub price_sigma: f64,
}

impl CoinBelief {
    /// Fractional price spike above the slow anchor: (p − p̄)/p̄. Positive ⇒
    /// the mispricing window is open (revenue jumped, difficulty hasn't).
    pub fn price_spike(&self) -> f64 {
        if self.price_anchor_usd <= 0.0 {
            return 0.0;
        }
        (self.price_usd - self.price_anchor_usd) / self.price_anchor_usd
    }
}

/// Estimated state/quality of one pool.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PoolBelief {
    pub pool: PoolId,
    /// Measured round-trip latency to the lowest endpoint, ms.
    pub latency_ms: f64,
    /// Measured stale/reject rate attributable to the pool path.
    pub stale_rate: f64,
    /// Realized luck multiplier EWMA (>1 lucky). Honest, observation-only.
    pub luck: f64,
    /// Online/healthy flag from the connection watchdog.
    pub online: bool,
}

/// Probabilistic, multi-resolution world-state. The *only* policy input.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Belief {
    /// Simulation/world clock, seconds.
    pub t_secs: f64,
    pub coins: BTreeMap<CoinId, CoinBelief>,
    pub pools: BTreeMap<PoolId, PoolBelief>,
    /// Current local energy price, USD per kWh.
    pub energy_price_usd_kwh: f64,
    /// Forecast energy price over the horizon, USD/kWh (index = horizon step).
    pub energy_forecast_usd_kwh: Vec<f64>,
    /// If a demand-response window is armed, the credit it pays per kWh curtailed.
    pub dr_credit_usd_kwh: Option<f64>,
    pub ambient_c: f64,
    /// Live device telemetry, keyed by device.
    pub devices: BTreeMap<DeviceId, DeviceTelemetry>,
    /// Health signals for predictive maintenance.
    pub health: BTreeMap<DeviceId, HealthTelemetry>,
    /// Overall confidence the brain has in its own beliefs right now, [0,1].
    /// Drives graceful de-risking (a crude competence signal for Phase 0.5).
    pub confidence: f64,
}

impl Belief {
    pub fn coin(&self, c: &CoinId) -> Option<&CoinBelief> {
        self.coins.get(c)
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Action — what the policy emits; the shield filters it before actuation
// ─────────────────────────────────────────────────────────────────────────────

/// What a single device is told to do this tick.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct DeviceSetpoint {
    pub device: DeviceId,
    /// `None` ⇒ curtailed (powered down / idle), e.g. under a binding cap or
    /// when no geometry clears its cost.
    pub assignment: Option<Assignment>,
    pub knobs: Knobs,
}

/// A concrete (algorithm, coin, pool) the device is mining, plus an optional
/// dual-mined secondary that uses otherwise-idle subsystem capacity.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Assignment {
    pub algo: AlgorithmId,
    pub coin: CoinId,
    pub pool: PoolId,
    pub secondary: Option<(AlgorithmId, CoinId, PoolId)>,
}

impl Assignment {
    pub fn primary(algo: AlgorithmId, coin: CoinId, pool: PoolId) -> Self {
        Assignment {
            algo,
            coin,
            pool,
            secondary: None,
        }
    }
}

/// The fleet-wide control the policy emits each tick.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct FleetAction {
    pub setpoints: Vec<DeviceSetpoint>,
    /// Which named policy produced this action (for the online bandit + credit).
    pub policy: String,
}

impl FleetAction {
    pub fn get(&self, d: &DeviceId) -> Option<&DeviceSetpoint> {
        self.setpoints.iter().find(|s| &s.device == d)
    }
}

/// Risk word the operator sets. Maps to (γ, λ_dd, ε) in [`crate::utility`].
#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RiskWord {
    Conservative,
    Balanced,
    Aggressive,
}

impl RiskWord {
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_lowercase().as_str() {
            "conservative" | "low" | "safe" => Some(RiskWord::Conservative),
            "balanced" | "medium" | "default" => Some(RiskWord::Balanced),
            "aggressive" | "high" | "max" => Some(RiskWord::Aggressive),
            _ => None,
        }
    }
}

impl fmt::Display for RiskWord {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            RiskWord::Conservative => "conservative",
            RiskWord::Balanced => "balanced",
            RiskWord::Aggressive => "aggressive",
        };
        f.write_str(s)
    }
}

/// Convenience: watts·seconds → kWh.
pub fn ws_to_kwh(watts: f64, secs: f64) -> f64 {
    watts * secs / 3_600_000.0
}

/// Standby power (W) a curtailed or idle device still draws. Small but real, and
/// it counts toward facility power caps, so the curtailment planner must respect
/// it. Used wherever a device is idled.
pub const STANDBY_W: f64 = 8.0;
