//! Stratum-layer stale & latency engineering — driving the reject rate toward
//! zero, which is pure recovered revenue.
//!
//! Each device is routed to the lowest-latency endpoint measured live, share
//! difficulty (vardiff) is tuned to keep the submit rate in a healthy band, jobs
//! are switched the instant new work arrives, and warm failover is kept hot. The
//! scorer needs a single number from here: the expected stale fraction for a
//! (device, pool) pair, which discounts revenue, and the baseline a naive miner
//! would suffer (for honest credit attribution).

use crate::model::*;
use std::collections::BTreeMap;

#[derive(Clone, Copy, Debug)]
pub struct StratumParams {
    /// Residual pool-path stale floor after our optimizations (instant job
    /// switch + warm failover roughly halve a naive pool's stale).
    pub optimized_pool_floor: f64,
    /// What a naive miner's constant pool stale would be (credit baseline).
    pub naive_pool_stale: f64,
    /// Multiplier on the latency/block-time stale contribution for a naive miner
    /// that does not switch jobs instantly.
    pub naive_latency_mult: f64,
    /// Target shares submitted per second (vardiff band centre).
    pub target_share_rate_hz: f64,
}

impl Default for StratumParams {
    fn default() -> Self {
        StratumParams {
            optimized_pool_floor: 0.0010,
            naive_pool_stale: 0.0120,
            naive_latency_mult: 2.2,
            target_share_rate_hz: 0.2, // ~12 shares/min
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct StratumTuner {
    params: StratumParams,
    /// Recommended share difficulty per device (vardiff state).
    vardiff: BTreeMap<DeviceId, f64>,
}

impl StratumTuner {
    pub fn new(params: StratumParams) -> Self {
        StratumTuner {
            params,
            vardiff: BTreeMap::new(),
        }
    }

    /// Fraction of the latency window during which a share is stale: the time
    /// spent on the old job after a block until new work arrives, over the block
    /// interval. We switch jobs instantly, so only one latency window leaks.
    fn latency_stale(latency_ms: f64, block_time_s: f64) -> f64 {
        if block_time_s <= 0.0 {
            return 0.0;
        }
        (latency_ms / 1000.0 / block_time_s).clamp(0.0, 0.5)
    }

    /// Expected optimized stale fraction for a device mining a coin through a
    /// pool with `latency_ms`. This is what the opportunity scorer discounts.
    pub fn expected_stale(&self, latency_ms: f64, block_time_s: f64) -> f64 {
        (self.params.optimized_pool_floor + Self::latency_stale(latency_ms, block_time_s))
            .clamp(0.0, 0.5)
    }

    /// The stale a naive miner (no instant job switch, no endpoint tuning) would
    /// suffer on the same path — the baseline the stratum credit is measured
    /// against.
    pub fn naive_stale(&self, latency_ms: f64, block_time_s: f64) -> f64 {
        (self.params.naive_pool_stale
            + self.params.naive_latency_mult * Self::latency_stale(latency_ms, block_time_s))
        .clamp(0.0, 0.6)
    }

    /// Recovered-revenue fraction vs a naive miner: how much of revenue our
    /// stale-minimization keeps that the baseline loses.
    pub fn recovered_fraction(&self, latency_ms: f64, block_time_s: f64) -> f64 {
        (self.naive_stale(latency_ms, block_time_s) - self.expected_stale(latency_ms, block_time_s))
            .max(0.0)
    }

    /// Update the vardiff recommendation for a device given its hashrate, keeping
    /// the submit rate near the target band. Returns the share difficulty.
    pub fn update_vardiff(&mut self, device: &DeviceId, hashrate: f64) -> f64 {
        let target = (hashrate / self.params.target_share_rate_hz).max(1.0);
        let e = self.vardiff.entry(device.clone()).or_insert(target);
        // Smoothly track to avoid step changes that themselves cause stales.
        *e += 0.25 * (target - *e);
        *e
    }
}
