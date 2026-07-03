//! Consensus & Market Abstraction Layer — the [`RewardGeometry`] contract and
//! the [`Market`] that owns the coin/pool universe.
//!
//! A reward geometry is one (coin, pool) the fleet could mine. Its job is to
//! convert hashrate + price + difficulty into expected USD/second. The
//! mispriced-coin lever lives here: revenue responds to price *now* while
//! difficulty (hence network hashrate) lags by `retarget_window_s`.

use crate::model::*;
use std::collections::BTreeMap;

/// One mineable opportunity. Pure economics — costs are applied by the scorer.
pub trait RewardGeometry {
    fn coin(&self) -> &CoinDescriptor;
    fn pool(&self) -> &PoolDescriptor;
    fn inclusion(&self) -> InclusionKind {
        self.coin().inclusion
    }

    /// Expected gross revenue in USD/second for `hashrate` H/s, at the given
    /// coin `price_usd` and network `difficulty`, *before* pool fee and stale
    /// loss. This is the term `value per accepted unit` from the profit identity.
    fn gross_revenue_usd_per_s(&self, hashrate: f64, price_usd: f64, difficulty: f64) -> f64 {
        let c = self.coin();
        let net_h = c.implied_network_hashrate(difficulty);
        if net_h <= 0.0 {
            return 0.0;
        }
        // Issuance per second network-wide, in coin, split by our hashrate share.
        let issuance_per_s = (c.block_reward + c.fee_per_block) / c.block_time_s;
        let our_share = hashrate / net_h;
        issuance_per_s * our_share * price_usd
    }

    /// Revenue net of pool fee and counterparty solvency haircut, but *not* stale
    /// loss (which depends on the device's pool latency, applied by the scorer).
    fn net_of_pool_usd_per_s(&self, hashrate: f64, price_usd: f64, difficulty: f64) -> f64 {
        let gross = self.gross_revenue_usd_per_s(hashrate, price_usd, difficulty);
        let p = self.pool();
        gross * (1.0 - p.fee_frac) * (1.0 - p.solvency_risk)
    }
}

/// A concrete geometry built from owned descriptors.
#[derive(Clone, Debug)]
pub struct GeometryView {
    coin: CoinDescriptor,
    pool: PoolDescriptor,
}

impl GeometryView {
    pub fn new(coin: CoinDescriptor, pool: PoolDescriptor) -> Self {
        Self { coin, pool }
    }
}

impl RewardGeometry for GeometryView {
    fn coin(&self) -> &CoinDescriptor {
        &self.coin
    }
    fn pool(&self) -> &PoolDescriptor {
        &self.pool
    }
}

/// The universe of coins and pools the engine may mine. Enumerates the feasible
/// geometries for a device given its algorithm capabilities.
#[derive(Clone, Debug, Default)]
pub struct Market {
    pub coins: BTreeMap<CoinId, CoinDescriptor>,
    pub pools: BTreeMap<PoolId, PoolDescriptor>,
}

impl Market {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn add_coin(&mut self, c: CoinDescriptor) {
        self.coins.insert(c.id.clone(), c);
    }

    pub fn add_pool(&mut self, p: PoolDescriptor) {
        self.pools.insert(p.id.clone(), p);
    }

    pub fn coin(&self, id: &CoinId) -> Option<&CoinDescriptor> {
        self.coins.get(id)
    }

    pub fn pool(&self, id: &PoolId) -> Option<&PoolDescriptor> {
        self.pools.get(id)
    }

    /// All (coin, pool) geometries whose coin runs on `algo`.
    pub fn geometries_for_algo(&self, algo: &AlgorithmId) -> Vec<GeometryView> {
        let mut out = Vec::new();
        for coin in self.coins.values().filter(|c| &c.algo == algo) {
            for pool in self.pools.values().filter(|p| p.coin == coin.id) {
                out.push(GeometryView::new(coin.clone(), pool.clone()));
            }
        }
        out
    }

    /// All pools serving a coin.
    pub fn pools_for_coin(&self, coin: &CoinId) -> Vec<&PoolDescriptor> {
        self.pools.values().filter(|p| &p.coin == coin).collect()
    }

    pub fn build_geometry(&self, coin: &CoinId, pool: &PoolId) -> Option<GeometryView> {
        Some(GeometryView::new(
            self.coins.get(coin)?.clone(),
            self.pools.get(pool)?.clone(),
        ))
    }
}
