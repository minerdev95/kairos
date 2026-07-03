//! Forward-difficulty forecasting — the engine's central edge.
//!
//! Incumbents optimize against *spot* difficulty. KAIROS optimizes against the
//! **forecast difficulty path** over the holding horizon. That single change is
//! what lets one number capture three levers at once:
//!
//!  * **Mispriced coin** — when price jumps, mining margin jumps the same instant
//!    but difficulty only catches up over the chain's retarget window. The
//!    near-term path stays low, so near-term revenue is elevated; the engine
//!    harvests it and the rising path tells it when to leave.
//!  * **Reflexive catch-up** — difficulty chases the level that restores the
//!    equilibrium margin miners tolerate, with the retarget lag as a time
//!    constant. Modeled as an exponential approach to a target.
//!  * **Migration overshoot** — a coin that pays better than its peers attracts
//!    extra hashrate inflow, so its target difficulty overshoots what price alone
//!    implies. The engine exits *before* the pile-in, not after.
//!
//! Everything here is estimated from the engine's own observations with EWMA and
//! a one-state lagged controller — no external data feed required.

use crate::algo::AlgorithmRegistry;
use crate::cal::Market;
use crate::model::*;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
pub struct ForecastParams {
    /// Number of horizon steps the forward path covers.
    pub horizon_steps: usize,
    /// Seconds per horizon step.
    pub step_secs: f64,
    /// EWMA half-life (s) for the slow "fair value" price anchor a spike is
    /// measured against. Long ⇒ a spike must persist to move the anchor.
    pub price_anchor_halflife_s: f64,
    /// Scales how fast difficulty chases its target relative to the retarget
    /// window. 1.0 ⇒ time-constant == retarget window.
    pub retarget_responsiveness: f64,
    /// Migration overshoot sensitivity. Higher ⇒ hotter-than-peers coins are
    /// assumed to attract more inflow, so the engine exits them sooner.
    pub migration_beta: f64,
    /// Equilibrium margin miners tolerate (small positive); difficulty targets
    /// the level that drives realized margin down to this.
    pub equilibrium_margin: f64,
}

impl Default for ForecastParams {
    fn default() -> Self {
        ForecastParams {
            horizon_steps: 12,
            step_secs: 300.0, // 1-hour horizon in 5-minute steps
            price_anchor_halflife_s: 6.0 * 3600.0,
            retarget_responsiveness: 1.0,
            migration_beta: 0.6,
            equilibrium_margin: 0.05,
        }
    }
}

/// Max fractional difficulty rise over the horizon attributable to migration
/// into a coin whose margin has spiked above its anchor. Bounds the reflexive loop.
const MIGRATION_CAP: f64 = 0.6;

/// Holds the slow EWMA state between ticks and produces the forward paths.
#[derive(Clone, Debug, Default)]
pub struct Forecaster {
    params: ForecastParams,
    price_anchor: BTreeMap<CoinId, f64>,
    /// EWMA of each coin's mining margin — the baseline a *transient* margin
    /// spike (the real migration trigger) is measured against. A persistently
    /// high-margin coin is at equilibrium and attracts no *new* inflow.
    margin_anchor: BTreeMap<CoinId, f64>,
    /// Fast EWMA of squared log price returns → instantaneous volatility (drives
    /// the risk haircut). Full two-sided variance.
    price_logret_var: BTreeMap<CoinId, f64>,
    /// Fast and slow EWMA of *downside* semivariance (squared negative returns).
    /// Their ratio is a crash detector: it spikes on a price drop but NOT on an
    /// upward mispricing spike, so the engine hunkers down before a crash yet
    /// stays aggressive enough to seize an up-spike.
    price_downvar_fast: BTreeMap<CoinId, f64>,
    price_downvar_slow: BTreeMap<CoinId, f64>,
    last_price: BTreeMap<CoinId, f64>,
    last_t: Option<f64>,
}

impl Forecaster {
    pub fn new(params: ForecastParams) -> Self {
        Forecaster {
            params,
            ..Default::default()
        }
    }

    pub fn params(&self) -> &ForecastParams {
        &self.params
    }

    fn ewma_alpha(dt: f64, halflife: f64) -> f64 {
        if halflife <= 0.0 {
            return 1.0;
        }
        1.0 - 0.5_f64.powf(dt / halflife)
    }

    /// Revenue in USD/s for one unit (1 H/s) of hashrate on `coin` at `difficulty`.
    fn revenue_per_hs(market: &Market, coin: &CoinDescriptor, price: f64, difficulty: f64) -> f64 {
        let _ = market;
        let net_h = coin.implied_network_hashrate(difficulty);
        if net_h <= 0.0 {
            return 0.0;
        }
        let issuance_per_s = (coin.block_reward + coin.fee_per_block) / coin.block_time_s;
        issuance_per_s / net_h * price
    }

    /// Normalized mining margin of a coin against the local energy price, using
    /// the algorithm's reference efficiency. margin = yield_per_joule / cost − 1.
    fn coin_margin(
        market: &Market,
        algos: &AlgorithmRegistry,
        coin: &CoinDescriptor,
        price: f64,
        difficulty: f64,
        energy_price_usd_kwh: f64,
    ) -> f64 {
        let rev_per_hs = Self::revenue_per_hs(market, coin, price, difficulty);
        let eff = algos
            .get(&coin.algo)
            .map(|a| a.ref_efficiency_j_per_h)
            .unwrap_or(1.0);
        if eff <= 0.0 {
            return 0.0;
        }
        // rev_per_hs is USD/s per (H/s); power per (H/s) is `eff` joules/hash = W.
        let yield_per_joule = rev_per_hs / eff;
        let cost_per_joule = energy_price_usd_kwh / 3_600_000.0;
        if cost_per_joule <= 0.0 {
            return 0.0;
        }
        yield_per_joule / cost_per_joule - 1.0
    }

    /// Update slow EWMA state and write forward paths + anchors + vol into the
    /// belief. Call once per tick after raw prices/difficulties are sensed.
    pub fn refresh(&mut self, belief: &mut Belief, market: &Market, algos: &AlgorithmRegistry) {
        let dt = match self.last_t {
            Some(t0) => (belief.t_secs - t0).max(1.0),
            None => self.params.step_secs,
        };
        self.last_t = Some(belief.t_secs);
        let alpha = Self::ewma_alpha(dt, self.params.price_anchor_halflife_s);
        // Volatility EWMA uses a shorter half-life so risk reacts faster.
        let alpha_vol = Self::ewma_alpha(dt, self.params.price_anchor_halflife_s / 6.0);

        // 1) Update anchors and volatility from observed prices.
        for (id, cb) in belief.coins.iter() {
            let anchor = self.price_anchor.entry(id.clone()).or_insert(cb.price_usd);
            *anchor += alpha * (cb.price_usd - *anchor);

            if let Some(&p_prev) = self.last_price.get(id) {
                if p_prev > 0.0 && cb.price_usd > 0.0 {
                    let r = (cb.price_usd / p_prev).ln();
                    let v = self.price_logret_var.entry(id.clone()).or_insert(0.0);
                    *v += alpha_vol * (r * r - *v);
                    // Downside semivariance (crash detector): only drops count.
                    let dn = r.min(0.0);
                    let df = self.price_downvar_fast.entry(id.clone()).or_insert(0.0);
                    *df += alpha_vol * (dn * dn - *df);
                    let ds = self.price_downvar_slow.entry(id.clone()).or_insert(0.0);
                    *ds += alpha * (dn * dn - *ds); // slow uses the long anchor halflife
                }
            }
            self.last_price.insert(id.clone(), cb.price_usd);
        }

        // 2) Compute each coin's current margin, then the peer mean (for overshoot).
        let mut margins: BTreeMap<CoinId, f64> = BTreeMap::new();
        for (id, cb) in belief.coins.iter() {
            if let Some(coin) = market.coin(id) {
                let m = Self::coin_margin(
                    market,
                    algos,
                    coin,
                    cb.price_usd,
                    cb.difficulty,
                    belief.energy_price_usd_kwh,
                );
                margins.insert(id.clone(), m);
            }
        }
        // Update each coin's slow margin anchor (the no-spike baseline).
        for (id, m) in margins.iter() {
            let a = self.margin_anchor.entry(id.clone()).or_insert(*m);
            *a += alpha * (m - *a);
        }

        // 3) Build each coin's forward difficulty path and fill belief fields.
        let p = self.params;
        let coin_ids: Vec<CoinId> = belief.coins.keys().cloned().collect();
        for id in coin_ids {
            let (price, difficulty) = {
                let cb = &belief.coins[&id];
                (cb.price_usd, cb.difficulty)
            };
            let anchor = *self.price_anchor.get(&id).unwrap_or(&price);
            let margin_now = *margins.get(&id).unwrap_or(&0.0);
            let margin_base = *self.margin_anchor.get(&id).unwrap_or(&margin_now);

            // Bounded, saturating migration model driven by the *transient* margin
            // spike (margin above its own slow anchor), not the persistent level —
            // a coin that has been high-margin for a long time is at equilibrium
            // and attracts no new inflow, so in calm markets forward ≈ spot and
            // the engine does not abandon the best coin prematurely. A genuine
            // spike (price jump / new opportunity) lifts margin above its anchor,
            // signalling difficulty will climb as hashrate floods in — the engine
            // mines it now but knows to exit before the pile-in. Bounded, because
            // global hashrate cannot teleport over one planning horizon.
            let spike = margin_now - margin_base;
            // tanh already bounds the drift to ±MIGRATION_CAP — the model boundary.
            let drift = MIGRATION_CAP * (p.migration_beta * spike).tanh();
            let d_target = (difficulty * (1.0 + drift)).max(difficulty * 0.25);

            // Time constant of the difficulty controller, in seconds.
            let tau = market
                .coin(&id)
                .map(|c| c.retarget_window_s)
                .unwrap_or(p.step_secs * 4.0)
                .max(p.step_secs)
                / p.retarget_responsiveness.max(1e-3);

            let mut path = Vec::with_capacity(p.horizon_steps);
            for k in 1..=p.horizon_steps {
                let h = k as f64 * p.step_secs;
                let approach = 1.0 - (-h / tau).exp();
                path.push(difficulty + (d_target - difficulty) * approach);
            }

            // Revenue volatility (USD/s per nominal unit hashrate) ≈ revenue · σ.
            let sigma = self
                .price_logret_var
                .get(&id)
                .map(|v| v.max(0.0).sqrt())
                .unwrap_or(0.0);
            let rev_per_hs = market
                .coin(&id)
                .map(|c| Self::revenue_per_hs(market, c, price, difficulty))
                .unwrap_or(0.0);

            let cb = belief.coins.get_mut(&id).unwrap();
            cb.price_anchor_usd = anchor;
            cb.price_sigma = sigma;
            cb.forward_difficulty = path;
            cb.network_hashrate = market
                .coin(&id)
                .map(|c| c.implied_network_hashrate(difficulty))
                .unwrap_or(cb.network_hashrate);
            cb.revenue_vol = rev_per_hs * sigma;
        }

        // 4) Energy forecast: persistence with mild mean reversion toward anchor.
        if belief.energy_forecast_usd_kwh.len() != p.horizon_steps {
            belief.energy_forecast_usd_kwh = vec![belief.energy_price_usd_kwh; p.horizon_steps];
        }

        // 5) Crash detector → confidence. When fast downside-volatility runs hot
        // relative to its slow regime baseline (a price break to the downside),
        // confidence drops and the switching logic freezes turnover so a crash
        // doesn't chew up switch costs in whipsaws. An *upward* mispricing spike
        // does not move downside vol, so the engine stays aggressive to seize it.
        // Computed by the brain from observed returns — honest in production too.
        const VOL_FLOOR: f64 = 4.0e-6; // (~0.2%/step)^2 baseline so 0/0 → ratio 1
        let mut worst_ratio = 1.0_f64;
        for id in belief.coins.keys() {
            let fast = self.price_downvar_fast.get(id).copied().unwrap_or(0.0);
            let slow = self.price_downvar_slow.get(id).copied().unwrap_or(0.0);
            let ratio = (fast + VOL_FLOOR) / (slow + VOL_FLOOR);
            if ratio > worst_ratio {
                worst_ratio = ratio;
            }
        }
        belief.confidence = (0.95 - 0.20 * (worst_ratio - 1.0).max(0.0)).clamp(0.2, 0.95);
    }

    /// The difficulty the engine expects at horizon-step `k` (0-based), falling
    /// back to spot if no path was built. The scorer reads this, not spot.
    pub fn forward_difficulty(belief: &Belief, coin: &CoinId, k: usize) -> f64 {
        belief
            .coin(coin)
            .map(|cb| {
                cb.forward_difficulty
                    .get(k)
                    .copied()
                    .unwrap_or(cb.difficulty)
            })
            .unwrap_or(1.0)
    }
}
