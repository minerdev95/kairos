//! Scheme- and luck-aware fair-value pool routing.
//!
//! Beyond *which coin*, *which pool*. Pools differ in fee, reward scheme, payout
//! reliability, latency, and stale rate. The router picks the pool with the best
//! **risk-adjusted** value for the operator's hashrate — net of fee, solvency
//! haircut, and expected stale, penalized for the scheme's payout variance
//! according to the operator's risk word. It routes fairly: it chooses
//! fair-value pools, it does not hop to exploit other members.

use super::stratum::StratumTuner;
use crate::cal::Market;
use crate::model::*;
use crate::utility::OperatorUtility;

/// A pool's risk-adjusted quality multiplier on gross revenue, in [0, 1].
/// 1.0 would be a free, instantly-connected, zero-variance, always-solvent pool.
pub fn pool_quality(
    pool: &PoolDescriptor,
    pool_belief: Option<&PoolBelief>,
    coin: &CoinDescriptor,
    stratum: &StratumTuner,
    utility: &OperatorUtility,
) -> f64 {
    let latency_ms = pool_belief.map(|b| b.latency_ms).unwrap_or(60.0);
    let luck = pool_belief.map(|b| b.luck).unwrap_or(1.0);
    let stale = stratum.expected_stale(latency_ms, coin.block_time_s);
    // Variance penalty: scheme payout variance times the operator's risk aversion,
    // expressed as a small multiplicative haircut so a high-variance scheme must
    // out-yield a low-variance one to win.
    let var_pen = 1.0 - (0.15 * utility.gamma * pool.scheme.variance_factor()).min(0.5);
    (1.0 - pool.fee_frac).max(0.0)
        * (1.0 - pool.solvency_risk).max(0.0)
        * (1.0 - stale).max(0.0)
        * luck.clamp(0.5, 1.5)
        * var_pen
}

/// Choose the best pool for a coin, returning (pool id, its quality). Returns
/// `None` if no pool serves the coin.
pub fn best_pool(
    market: &Market,
    belief: &Belief,
    coin: &CoinDescriptor,
    stratum: &StratumTuner,
    utility: &OperatorUtility,
) -> Option<(PoolId, f64)> {
    let mut best: Option<(PoolId, f64)> = None;
    for pool in market.pools_for_coin(&coin.id) {
        let q = pool_quality(pool, belief.pools.get(&pool.id), coin, stratum, utility);
        match &best {
            Some((_, bq)) if *bq >= q => {}
            _ => best = Some((pool.id.clone(), q)),
        }
    }
    best
}

/// The worst pool's quality for a coin — the counterfactual the pool-routing
/// credit is measured against.
pub fn worst_pool_quality(
    market: &Market,
    belief: &Belief,
    coin: &CoinDescriptor,
    stratum: &StratumTuner,
    utility: &OperatorUtility,
) -> Option<f64> {
    let mut worst: Option<f64> = None;
    for pool in market.pools_for_coin(&coin.id) {
        let q = pool_quality(pool, belief.pools.get(&pool.id), coin, stratum, utility);
        worst = Some(worst.map_or(q, |w: f64| w.min(q)));
    }
    worst
}
