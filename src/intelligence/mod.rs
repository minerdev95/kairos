//! The intelligence core — the profitability brain.
//!
//! One tick: enumerate each device's feasible (algo, coin, pool, knobs)
//! candidates; score every candidate as a forward-integrated, risk-adjusted
//! $/sec margin **against the forecast difficulty path** (never spot); decide
//! per device with hysteresis-priced optimal stopping; allocate the fleet under
//! shared power caps with marginal-watt curtailment and demand-response; pass the
//! lexicographic gate cascade (conduct → ruin → utility); and attribute realized
//! value across levers for honest credit.
//!
//! The single differentiator, restated: the opportunity score is **relative to
//! the incumbent** and computed over the difficulty the engine *will* face, so a
//! price move that lifts all coins equally is correctly seen as *no* edge — only
//! cross-sectional dispersion and migration overshoot are.

pub mod energy;
pub mod pool;
pub mod stratum;
pub mod switch;
pub mod tune;

use crate::algo::AlgorithmRegistry;
use crate::cal::{Market, RewardGeometry};
use crate::forecast::Forecaster;
use crate::hal::{DamageDelta, EfficiencyPoint};
use crate::model::*;
use crate::utility::OperatorUtility;
use std::collections::{BTreeMap, BTreeSet};

use energy::MarginalUnit;
use stratum::StratumTuner;
use switch::SwitchBook;
use tune::TunerBook;

/// The brain queries device physics through this oracle. The HAL fleet and the
/// twin implement it identically, so the policy is blind to sim-vs-real.
pub trait EfficiencyOracle {
    fn efficiency(
        &self,
        dev: &DeviceId,
        algo: &AlgorithmId,
        knobs: &Knobs,
        temp_c: f64,
    ) -> EfficiencyPoint;
    fn degradation(&self, dev: &DeviceId, op: &OperatingPoint, dt_s: f64) -> DamageDelta;
}

/// Per-site power caps (W).
#[derive(Clone, Debug, Default)]
pub struct PowerBudget {
    pub site_cap_w: BTreeMap<SiteId, f64>,
}

impl PowerBudget {
    pub fn cap(&self, site: &SiteId) -> f64 {
        self.site_cap_w.get(site).copied().unwrap_or(f64::INFINITY)
    }
}

/// Deterministic obligations the ruin gate must keep equity above (power + rent
/// over the runway). Not a guessed runway — a real floor.
#[derive(Clone, Copy, Debug, Default)]
pub struct Obligations {
    pub usd_per_day: f64,
}

#[derive(Clone, Copy, Debug)]
pub struct BrainParams {
    pub discount_rate_per_s: f64,
    pub step_secs: f64,
    pub horizon_steps: usize,
    /// γ ramps with drawdown: γ_t = γ·(1 + ramp·DD).
    pub gamma_dd_ramp: f64,
    /// Wear is priced up as drawdown rises (clock down when losing).
    pub wear_dd_ramp: f64,
    /// Hard per-chain network-share ceiling (consensus safety). Never exceeded.
    pub consensus_share_ceiling: f64,
    /// Days of runway the ruin floor must cover.
    pub runway_days: f64,
    /// Sigmas of downside used in the ruin check.
    pub ruin_k_sigma: f64,
    /// Confidence below which the engine freezes switching (regime-break proxy).
    pub freeze_confidence: f64,
}

impl Default for BrainParams {
    fn default() -> Self {
        BrainParams {
            discount_rate_per_s: 1.0 / 3600.0,
            step_secs: 300.0,
            horizon_steps: 12,
            gamma_dd_ramp: 4.0,
            wear_dd_ramp: 2.0,
            consensus_share_ceiling: 0.25,
            runway_days: 14.0,
            ruin_k_sigma: 2.0,
            freeze_confidence: 0.35,
        }
    }
}

/// Fleet equity tracker for the drawdown-aware utility and the ruin gate.
#[derive(Clone, Copy, Debug)]
pub struct EquityState {
    pub dollars: f64,
    pub peak: f64,
    pub dd: f64,
}

impl EquityState {
    pub fn new(initial: f64) -> Self {
        EquityState {
            dollars: initial,
            peak: initial,
            dd: 0.0,
        }
    }
    pub fn settle(&mut self, delta_usd: f64) {
        self.dollars += delta_usd;
        if self.dollars > self.peak {
            self.peak = self.dollars;
        }
        self.dd = if self.peak > 0.0 {
            ((self.peak - self.dollars) / self.peak).clamp(0.0, 1.0)
        } else {
            0.0
        };
    }
}

/// A scored candidate operating point for one device.
#[derive(Clone, Debug)]
struct Candidate {
    algo: AlgorithmId,
    coin: CoinId,
    pool: PoolId,
    knobs: Knobs,
    hashrate: f64,
    power_w: f64,
    /// Forward-integrated revenue net of pool fee + stale (USD/s).
    fwd_rev: f64,
    energy_usd_s: f64,
    wear_usd_s: f64,
    /// Risk-adjusted certainty-equivalent margin over the FORWARD path (USD/s) —
    /// the coin-choice and switching number.
    score_ce: f64,
    /// Net at *spot* difficulty (USD/s) — the run/idle, curtailment, and realized
    /// number. Spot, because that is the profit actually captured this tick.
    spot_net: f64,
}

/// Human-readable rationale line (the `kairos why` feed).
#[derive(Clone, Debug)]
pub struct RationaleItem {
    pub t_secs: f64,
    pub scope: String,
    pub message: String,
}

/// The brain's output for one tick.
#[derive(Clone, Debug)]
pub struct Decision {
    pub action: FleetAction,
    pub rationale: Vec<RationaleItem>,
    /// Per-lever marginal credit (USD/s) for the credit ledger.
    pub credit: Vec<(String, f64)>,
    /// What a fixed, reasonable incumbent baseline would net this tick (USD/s).
    pub baseline_net_per_s: f64,
    /// Optimized fleet net margin this tick (USD/s), risk-neutral.
    pub fleet_net_per_s: f64,
    /// DR credit earned (USD/s), already included in fleet_net.
    pub dr_credit_per_s: f64,
    pub idle_devices: usize,
    pub total_power_w: f64,
    /// Number of candidate switches actually taken.
    pub switches: usize,
    /// Conduct/ruin gate trips this tick (audit).
    pub gate_trips: Vec<String>,
}

/// The brain. Owns all learned state between ticks.
pub struct Brain {
    pub name: String,
    pub utility: OperatorUtility,
    pub forecaster: Forecaster,
    pub tuner: TunerBook,
    pub switch: SwitchBook,
    pub stratum: StratumTuner,
    pub equity: EquityState,
    pub params: BrainParams,
    pub tick: u64,
    pub conduct_audit: Vec<String>,
}

impl Brain {
    pub fn new(
        name: impl Into<String>,
        utility: OperatorUtility,
        forecaster: Forecaster,
        params: BrainParams,
        initial_equity: f64,
    ) -> Self {
        Brain {
            name: name.into(),
            utility,
            forecaster,
            tuner: TunerBook::new(tune::TuneParams::default()),
            switch: SwitchBook::new(switch::SwitchParams::default()),
            stratum: StratumTuner::new(stratum::StratumParams::default()),
            equity: EquityState::new(initial_equity),
            params,
            tick: 0,
            conduct_audit: Vec::new(),
        }
    }

    fn horizon_secs(&self) -> f64 {
        self.params.horizon_steps as f64 * self.params.step_secs
    }

    /// Effective, drawdown-ramped risk aversion.
    fn gamma_eff(&self) -> f64 {
        self.utility.gamma * (1.0 + self.params.gamma_dd_ramp * self.equity.dd)
    }

    /// Update beliefs/forecasts in place. Call before `decide`.
    pub fn observe(&mut self, belief: &mut Belief, market: &Market, algos: &AlgorithmRegistry) {
        self.forecaster.refresh(belief, market, algos);
    }

    /// Settle realized net into equity after actuation (drives drawdown).
    pub fn settle(&mut self, realized_net_usd: f64) {
        self.equity.settle(realized_net_usd);
    }

    /// Feed realized telemetry back into the autotuner.
    pub fn learn_from_telemetry(
        &mut self,
        dev: &DeviceProfile,
        algos: &AlgorithmRegistry,
        algo: &AlgorithmId,
        error_rate: f64,
        fault: bool,
    ) {
        if let Some(ap) = algos.get(algo) {
            self.tuner.observe(dev, ap, error_rate, fault, self.tick);
        }
    }

    /// Forward-integrated revenue (USD/s) over the forecast difficulty path,
    /// discounted, net of pool fee/solvency and the expected stale fraction.
    fn forward_revenue(
        &self,
        geo: &crate::cal::GeometryView,
        cb: &CoinBelief,
        hashrate: f64,
        stale: f64,
    ) -> f64 {
        let n = cb.forward_difficulty.len();
        if n == 0 {
            let r = geo.net_of_pool_usd_per_s(hashrate, cb.price_usd, cb.difficulty);
            return r * (1.0 - stale);
        }
        let mut num = 0.0;
        let mut den = 0.0;
        for (k, &d) in cb.forward_difficulty.iter().enumerate() {
            let t = (k + 1) as f64 * self.params.step_secs;
            let disc = (-self.params.discount_rate_per_s * t).exp();
            let r = geo.net_of_pool_usd_per_s(hashrate, cb.price_usd, d) * (1.0 - stale);
            num += disc * r;
            den += disc;
        }
        if den > 0.0 {
            num / den
        } else {
            0.0
        }
    }

    /// Scale-free relative revenue variance for the risk haircut. The pool
    /// scheme's payout variance enters *multiplicatively* (a high-variance scheme
    /// like PPLNS amplifies the coin's price variance) rather than as a tiny
    /// additive term that price variance would otherwise swamp — so PPLNS is
    /// genuinely penalized more than FPPS under the same price uncertainty.
    fn relative_var(&self, cb: &CoinBelief, scheme: RewardScheme) -> f64 {
        let s = cb.price_sigma;
        let scheme_amp = 1.0 + 1.5 * scheme.variance_factor(); // FPPS≈1.075, PPLNS≈1.525, Solo=2.5
        ((s * s) * scheme_amp + 0.02 * scheme.variance_factor()).clamp(0.0, 1.0)
    }

    /// Build every feasible scored candidate for one device.
    fn candidates_for_device(
        &mut self,
        dev: &DeviceProfile,
        belief: &Belief,
        market: &Market,
        algos: &AlgorithmRegistry,
        oracle: &dyn EfficiencyOracle,
    ) -> Vec<Candidate> {
        let gamma_eff = self.gamma_eff();
        let wear_scale = 1.0 + self.params.wear_dd_ramp * self.equity.dd;
        let temp = belief
            .devices
            .get(&dev.id)
            .map(|t| t.temp_c)
            .unwrap_or(belief.ambient_c + 25.0);
        let energy_price = belief.energy_price_usd_kwh;

        let mut out = Vec::new();
        for cap in &dev.capabilities {
            let algo = &cap.algo;
            let ap = match algos.get(algo) {
                Some(a) => a.clone(),
                None => continue,
            };
            // The knob settings the score chooses between: stock (safe) and the
            // edge-seeker's learned stable edge. Because the opportunity score
            // prices degradation, the engine only adopts a clock whose revenue
            // gain beats its wear+energy cost — the degradation-priced marginal
            // tuning rule, enforced by selection rather than by a hand gradient.
            let stock_knobs = TunerBook::stock(dev, algo, cap.stock_power_w);
            let tuned_knobs = self.tuner.propose(dev, &ap, cap.stock_power_w);
            let mut knob_opts = vec![stock_knobs];
            if tuned_knobs != stock_knobs {
                knob_opts.push(tuned_knobs);
            }

            for coin_desc in market.coins.values().filter(|c| &c.algo == algo) {
                let cb = match belief.coin(&coin_desc.id) {
                    Some(cb) => cb,
                    None => continue,
                };
                let (pool_id, _q) =
                    match pool::best_pool(market, belief, coin_desc, &self.stratum, &self.utility) {
                        Some(x) => x,
                        None => continue,
                    };
                let geo = match market.build_geometry(&coin_desc.id, &pool_id) {
                    Some(g) => g,
                    None => continue,
                };
                let pool_desc = market.pool(&pool_id).unwrap();
                let latency = belief
                    .pools
                    .get(&pool_id)
                    .map(|p| p.latency_ms)
                    .unwrap_or(60.0);
                let stale = self.stratum.expected_stale(latency, coin_desc.block_time_s);
                let relvar = self.relative_var(cb, pool_desc.scheme);
                let haircut = (0.5 * gamma_eff * relvar).clamp(0.0, 0.85);

                for knobs in &knob_opts {
                    let eff = oracle.efficiency(&dev.id, algo, knobs, temp);
                    if eff.hashrate <= 0.0 {
                        continue;
                    }
                    // Tuning-induced instability shows up as extra stale, so the
                    // score sees it and only adopts a clock whose hashrate gain
                    // survives its own reject rate (closes the Sim2Real gap).
                    let stale = (stale + eff.hw_error_rate).clamp(0.0, 0.6);
                    let fwd_rev = self.forward_revenue(&geo, cb, eff.hashrate, stale);
                    let energy_usd_s = eff.power_w * energy_price / 3_600_000.0;
                    let op = OperatingPoint {
                        algo: algo.clone(),
                        knobs: *knobs,
                        temp_c: temp,
                    };
                    let wear_usd_s = oracle.degradation(&dev.id, &op, 1.0).usd_cost * wear_scale;
                    let score_ce = fwd_rev * (1.0 - haircut) - energy_usd_s - wear_usd_s;
                    let spot_rev = geo
                        .net_of_pool_usd_per_s(eff.hashrate, cb.price_usd, cb.difficulty)
                        * (1.0 - stale);
                    let spot_net = spot_rev - energy_usd_s - wear_usd_s;

                    out.push(Candidate {
                        algo: algo.clone(),
                        coin: coin_desc.id.clone(),
                        pool: pool_id.clone(),
                        knobs: *knobs,
                        hashrate: eff.hashrate,
                        power_w: eff.power_w,
                        fwd_rev,
                        energy_usd_s,
                        wear_usd_s,
                        score_ce,
                        spot_net,
                    });
                }
            }
        }
        out
    }

    /// The **fixed incumbent** baseline — the operator's own existing stack, the
    /// honest thing uplift is measured against. It pins each device to a single
    /// coin (its primary algorithm's coin), runs stock clocks on the default
    /// pool with naive stale, never switches coins, never tunes, and only does a
    /// crude on/off thermostat (run iff currently positive). KAIROS must beat
    /// *this* by switching, timing, tuning, pool routing, stale-min, and finer
    /// curtailment — not a strawman, but not a clone of itself either.
    fn baseline_net_for_device(
        &self,
        dev: &DeviceProfile,
        belief: &Belief,
        market: &Market,
        algos: &AlgorithmRegistry,
        oracle: &dyn EfficiencyOracle,
    ) -> f64 {
        let temp = belief
            .devices
            .get(&dev.id)
            .map(|t| t.temp_c)
            .unwrap_or(belief.ambient_c + 25.0);
        let energy_price = belief.energy_price_usd_kwh;

        // Primary algorithm = the device's first capability; primary coin = the
        // first coin on that algorithm; primary pool = the first pool for it.
        for cap in &dev.capabilities {
            let algo = &cap.algo;
            if algos.get(algo).is_none() {
                continue;
            }
            let coin_desc = match market.coins.values().find(|c| &c.algo == algo) {
                Some(c) => c,
                None => continue,
            };
            let cb = match belief.coin(&coin_desc.id) {
                Some(cb) => cb,
                None => continue,
            };
            let pool_desc = match market.pools_for_coin(&coin_desc.id).first() {
                Some(p) => (*p).clone(),
                None => continue,
            };
            let geo = market.build_geometry(&coin_desc.id, &pool_desc.id).unwrap();
            let latency = belief
                .pools
                .get(&pool_desc.id)
                .map(|p| p.latency_ms)
                .unwrap_or(60.0);
            let naive_stale = self.stratum.naive_stale(latency, coin_desc.block_time_s);
            let knobs = TunerBook::stock(dev, algo, cap.stock_power_w);
            let eff = oracle.efficiency(&dev.id, algo, &knobs, temp);
            let rev = geo.net_of_pool_usd_per_s(eff.hashrate, cb.price_usd, cb.difficulty)
                * (1.0 - naive_stale);
            let energy = eff.power_w * energy_price / 3_600_000.0;
            let op = OperatingPoint {
                algo: algo.clone(),
                knobs,
                temp_c: temp,
            };
            let wear = oracle.degradation(&dev.id, &op, 1.0).usd_cost;
            let net = rev - energy - wear;
            // Crude thermostat only: idle if currently underwater.
            return net.max(0.0);
        }
        0.0
    }

    /// The whole tick.
    pub fn decide(
        &mut self,
        belief: &Belief,
        market: &Market,
        algos: &AlgorithmRegistry,
        devices: &BTreeMap<DeviceId, DeviceProfile>,
        oracle: &dyn EfficiencyOracle,
        budget: &PowerBudget,
        obligations: &Obligations,
    ) -> Decision {
        self.tick += 1;
        let t = belief.t_secs;
        let horizon = self.horizon_secs();
        let freeze = belief.confidence < self.params.freeze_confidence;

        let mut rationale: Vec<RationaleItem> = Vec::new();
        let mut credit: BTreeMap<String, f64> = BTreeMap::new();
        let mut gate_trips: Vec<String> = Vec::new();

        // Per-device chosen candidate (or None = idle), plus bookkeeping.
        struct Chosen {
            cand: Option<Candidate>,
            switched: bool,
            forecast_credit: f64,
            tuning_credit: f64,
            pool_credit: f64,
            stratum_credit: f64,
        }
        let mut chosen_map: BTreeMap<DeviceId, Chosen> = BTreeMap::new();
        let mut baseline_total = 0.0;

        for (dev_id, dev) in devices.iter() {
            baseline_total += self.baseline_net_for_device(dev, belief, market, algos, oracle);

            let cands = self.candidates_for_device(dev, belief, market, algos, oracle);
            // Runnable = positive *current* (spot) margin, with a hysteresis
            // deadband so a device near break-even does not flap on/off with
            // price noise (each toggle is a real reconnect + thermal cycle). A
            // *running* device keeps running until it is clearly underwater; an
            // *idle* device only starts when it is clearly profitable. The run/idle
            // test uses spot so the engine never leaves money on the table; the
            // forward path is used only to rank coins and time exits.
            let running_now = self.switch.incumbent(dev_id).is_some();
            let runnable: Vec<&Candidate> = cands
                .iter()
                .filter(|c| {
                    let band = 0.03 * c.fwd_rev.max(0.0);
                    if running_now {
                        c.spot_net > -band
                    } else {
                        c.spot_net > band
                    }
                })
                .collect();
            if runnable.is_empty() {
                chosen_map.insert(
                    dev_id.clone(),
                    Chosen {
                        cand: None,
                        switched: false,
                        forecast_credit: 0.0,
                        tuning_credit: 0.0,
                        pool_credit: 0.0,
                        stratum_credit: 0.0,
                    },
                );
                continue;
            }

            // Best by risk-adjusted certainty-equivalent.
            let best = runnable
                .iter()
                .copied()
                .max_by(|a, b| {
                    a.score_ce
                        .partial_cmp(&b.score_ce)
                        .unwrap_or(std::cmp::Ordering::Equal)
                })
                .unwrap();

            // Incumbent candidate (same coin+algo), re-priced now.
            let incumbent_asg = self.switch.incumbent(dev_id).cloned();
            let incumbent_cand: Option<&Candidate> = incumbent_asg.as_ref().and_then(|a| {
                cands
                    .iter()
                    .filter(|c| c.coin == a.coin && c.algo == a.algo && c.spot_net > 0.0)
                    .max_by(|x, y| {
                        x.score_ce
                            .partial_cmp(&y.score_ce)
                            .unwrap_or(std::cmp::Ordering::Equal)
                    })
            });

            // Switch decision (optimal stopping with hysteresis).
            let (pick, switched) = match incumbent_cand {
                Some(inc) => {
                    let edge = (best.score_ce - inc.score_ce) * horizon;
                    let cost = self.switch.switch_cost_usd(inc.fwd_rev, best.fwd_rev, dev.class);
                    if self.switch.should_switch(dev, edge, cost, self.tick, false) && !freeze {
                        (best.clone(), best.coin != inc.coin || best.algo != inc.algo)
                    } else {
                        (inc.clone(), false)
                    }
                }
                None => (best.clone(), false),
            };

            // Credit decomposition (marginal, order-based Shapley-lite).
            // Forecasting value = the forward-value advantage of the forward-best
            // coin over the coin a spot-greedy switcher would have chosen.
            let spot_pick_score = cands
                .iter()
                .filter(|c| c.spot_net > 0.0)
                .max_by(|a, b| {
                    a.spot_net
                        .partial_cmp(&b.spot_net)
                        .unwrap_or(std::cmp::Ordering::Equal)
                })
                .map(|c| c.score_ce)
                .unwrap_or(0.0);
            let forecast_credit = (pick.score_ce - spot_pick_score).max(0.0);
            let stock_knobs = TunerBook::stock(dev, &pick.algo, pick.power_w);
            let temp = belief
                .devices
                .get(dev_id)
                .map(|x| x.temp_c)
                .unwrap_or(belief.ambient_c + 25.0);
            let stock_eff = oracle.efficiency(dev_id, &pick.algo, &stock_knobs, temp);
            let geo = market.build_geometry(&pick.coin, &pick.pool).unwrap();
            let cb = belief.coin(&pick.coin).unwrap();
            let latency = belief
                .pools
                .get(&pick.pool)
                .map(|p| p.latency_ms)
                .unwrap_or(60.0);
            let block_time = market.coin(&pick.coin).unwrap().block_time_s;
            let stale = self.stratum.expected_stale(latency, block_time);
            let stock_rev = self.forward_revenue(&geo, cb, stock_eff.hashrate, stale);
            let stock_energy = stock_eff.power_w * belief.energy_price_usd_kwh / 3_600_000.0;
            let stock_op = OperatingPoint {
                algo: pick.algo.clone(),
                knobs: stock_knobs,
                temp_c: temp,
            };
            let stock_wear = oracle.degradation(dev_id, &stock_op, 1.0).usd_cost;
            let tuning_credit = (pick.fwd_rev - pick.energy_usd_s - pick.wear_usd_s)
                - (stock_rev - stock_energy - stock_wear);

            let worst_q = pool::worst_pool_quality(market, belief, market.coin(&pick.coin).unwrap(), &self.stratum, &self.utility)
                .unwrap_or(1.0);
            let best_q = pool::pool_quality(
                market.pool(&pick.pool).unwrap(),
                belief.pools.get(&pick.pool),
                market.coin(&pick.coin).unwrap(),
                &self.stratum,
                &self.utility,
            );
            let pool_credit = if best_q > 0.0 {
                pick.fwd_rev * (1.0 - worst_q / best_q).max(0.0)
            } else {
                0.0
            };
            let recovered = self.stratum.recovered_fraction(latency, block_time);
            let stratum_credit = pick.fwd_rev / (1.0 - stale).max(1e-6) * recovered;

            chosen_map.insert(
                dev_id.clone(),
                Chosen {
                    cand: Some(pick),
                    switched,
                    forecast_credit,
                    tuning_credit,
                    pool_credit,
                    stratum_credit,
                },
            );
        }

        // ── Energy: marginal-watt curtailment + DR, per-site caps ──────────────
        let mut units: Vec<MarginalUnit> = Vec::new();
        for (dev_id, ch) in chosen_map.iter() {
            if let Some(c) = &ch.cand {
                let dev = &devices[dev_id];
                let density = if c.power_w > 0.0 {
                    c.score_ce / c.power_w
                } else {
                    0.0
                };
                units.push(MarginalUnit {
                    device: dev_id.clone(),
                    site: dev.site.clone(),
                    power_w: c.power_w,
                    net_usd_per_s: c.spot_net,
                    risk_adj_density: density,
                    wear_usd_per_s: c.wear_usd_s,
                });
            }
        }
        let plan = energy::plan_energy(units, &budget.site_cap_w, belief.dr_credit_usd_kwh);

        // ── Conduct gate: per-chain network-share ceiling (hard) ───────────────
        // Aggregate our hashrate per coin among devices still running.
        let mut coin_hash: BTreeMap<CoinId, f64> = BTreeMap::new();
        for (dev_id, ch) in chosen_map.iter() {
            if plan.curtail.contains(dev_id) {
                continue;
            }
            if let Some(c) = &ch.cand {
                *coin_hash.entry(c.coin.clone()).or_insert(0.0) += c.hashrate;
            }
        }
        let mut forced_curtail: BTreeSet<DeviceId> = BTreeSet::new();
        for (coin, our_h) in &coin_hash {
            if let Some(cb) = belief.coin(coin) {
                let net_h = cb.network_hashrate.max(1.0);
                let share = our_h / (net_h + our_h);
                if share > self.params.consensus_share_ceiling {
                    let msg = format!(
                        "conduct: {} share {:.1}% over ceiling {:.0}% — curtailing to comply",
                        coin,
                        share * 100.0,
                        self.params.consensus_share_ceiling * 100.0
                    );
                    gate_trips.push(msg.clone());
                    self.conduct_audit.push(format!("[t={:.0}] {}", t, msg));
                    // Bound the audit log so a long run in a volatile market can't
                    // grow it without limit.
                    if self.conduct_audit.len() > 10_000 {
                        let n = self.conduct_audit.len();
                        self.conduct_audit.drain(0..n - 5_000);
                    }
                    // Curtail enough of the lowest-density devices on this coin.
                    let mut on_coin: Vec<(&DeviceId, f64)> = chosen_map
                        .iter()
                        .filter_map(|(d, ch)| {
                            ch.cand.as_ref().filter(|c| &c.coin == coin).map(|c| {
                                (d, if c.power_w > 0.0 { c.score_ce / c.power_w } else { 0.0 })
                            })
                        })
                        .collect();
                    // Density ascending, device id tiebreak → deterministic shed set.
                    on_coin.sort_by(|a, b| {
                        a.1.partial_cmp(&b.1)
                            .unwrap_or(std::cmp::Ordering::Equal)
                            .then_with(|| a.0.cmp(b.0))
                    });
                    let mut remaining = *our_h;
                    for (d, _) in on_coin {
                        if remaining / (net_h + remaining) <= self.params.consensus_share_ceiling {
                            break;
                        }
                        if let Some(c) = chosen_map.get(d).and_then(|ch| ch.cand.as_ref()) {
                            remaining -= c.hashrate;
                            forced_curtail.insert(d.clone());
                        }
                    }
                }
            }
        }

        // ── Compose the fleet action + accumulate credit/economics ─────────────
        let mut setpoints = Vec::new();
        let mut fleet_net = 0.0;
        let mut total_power = 0.0;
        let mut idle = 0usize;
        let mut switches = 0usize;
        let mut commits: Vec<(DeviceId, Option<Assignment>, Option<Knobs>)> = Vec::new();

        for (dev_id, ch) in chosen_map.iter() {
            let curtailed = plan.curtail.contains(dev_id) || forced_curtail.contains(dev_id);
            match (&ch.cand, curtailed) {
                (Some(c), false) => {
                    let asg = Assignment::primary(c.algo.clone(), c.coin.clone(), c.pool.clone());
                    setpoints.push(DeviceSetpoint {
                        device: dev_id.clone(),
                        assignment: Some(asg.clone()),
                        knobs: c.knobs,
                    });
                    commits.push((dev_id.clone(), Some(asg), Some(c.knobs)));
                    fleet_net += c.spot_net;
                    total_power += c.power_w;
                    if ch.switched {
                        switches += 1;
                    }
                    // Credit the levers for this device.
                    *credit.entry("forecasting".into()).or_insert(0.0) += ch.forecast_credit;
                    *credit.entry("tuning".into()).or_insert(0.0) += ch.tuning_credit;
                    *credit.entry("pool".into()).or_insert(0.0) += ch.pool_credit;
                    *credit.entry("stratum".into()).or_insert(0.0) += ch.stratum_credit;
                }
                _ => {
                    // Idle: low-power stock setpoint.
                    let low = Knobs::stock(STANDBY_W);
                    setpoints.push(DeviceSetpoint {
                        device: dev_id.clone(),
                        assignment: None,
                        knobs: low,
                    });
                    commits.push((dev_id.clone(), None, Some(low)));
                    idle += 1;
                    // Curtailment economics (DR + wear saved) are credited to the
                    // energy lever in aggregate below.
                }
            }
        }
        // DR + wear-saving credit to the energy lever.
        *credit.entry("energy".into()).or_insert(0.0) +=
            plan.dr_credit_usd_per_s + plan.wear_saved_usd_per_s;
        fleet_net += plan.dr_credit_usd_per_s;

        // ── Ruin gate (decoupled): keep equity above the obligations floor ─────
        // Preventative: gate on the projected worst-case equity path breaching the
        // deterministic obligations floor (not on already being underwater), and
        // ENFORCING: if it trips, actually idle the whole fleet to cash this tick
        // rather than merely logging — survival dominates yield.
        let floor = obligations.usd_per_day * self.params.runway_days;
        let fleet_net_day = fleet_net * 86_400.0;
        let downside_day = fleet_net_day - self.params.ruin_k_sigma * fleet_net_day.abs() * 0.1;
        let projected = self.equity.dollars + downside_day * self.params.runway_days;
        if projected < floor {
            let msg = format!(
                "ruin-gate: projected equity ${:.0} under floor ${:.0} — fleet idled to cash",
                projected, floor
            );
            gate_trips.push(msg.clone());
            // Force every device to safe standby idle (override the composed action).
            for sp in setpoints.iter_mut() {
                sp.assignment = None;
                sp.knobs = Knobs::stock(STANDBY_W);
            }
            fleet_net = 0.0;
            total_power = STANDBY_W * setpoints.len() as f64;
            idle = setpoints.len();
            rationale.push(RationaleItem {
                t_secs: t,
                scope: "fleet".into(),
                message: msg,
            });
        }

        // Commit switch-book state (dwell/turnover) after all decisions.
        for (d, asg, kn) in commits {
            self.switch.commit(&d, asg, kn, self.tick, t);
        }

        // ── Rationale highlights ───────────────────────────────────────────────
        if switches > 0 {
            rationale.push(RationaleItem {
                t_secs: t,
                scope: "fleet".into(),
                message: format!("{} device(s) switched after clearing round-trip switch cost.", switches),
            });
        }
        if !plan.dr_curtail.is_empty() {
            rationale.push(RationaleItem {
                t_secs: t,
                scope: "energy".into(),
                message: format!(
                    "demand-response window: selling interruptible capacity to the grid (+${:.0}/day).",
                    plan.dr_credit_usd_per_s * 86_400.0
                ),
            });
        }
        // Surface *mass* curtailment as a single event (the count is omitted so
        // the runtime's consecutive-dedupe collapses a sustained energy-price /
        // cap event into one line rather than repeating every tick). Routine
        // marginal idling is normal churn, already shown in the console's count.
        if idle >= devices.len() / 2 && plan.dr_curtail.is_empty() {
            rationale.push(RationaleItem {
                t_secs: t,
                scope: "energy".into(),
                message:
                    "curtailing low-margin capacity: marginal watts below cost or under a binding cap."
                        .into(),
            });
        }
        if freeze {
            rationale.push(RationaleItem {
                t_secs: t,
                scope: "fleet".into(),
                message: "low confidence (regime uncertainty): switching frozen, holding positions.".into(),
            });
        }

        Decision {
            action: FleetAction {
                setpoints,
                policy: self.name.clone(),
            },
            rationale,
            credit: credit.into_iter().collect(),
            baseline_net_per_s: baseline_total,
            fleet_net_per_s: fleet_net,
            dr_credit_per_s: plan.dr_credit_usd_per_s,
            idle_devices: idle,
            total_power_w: total_power,
            switches,
            gate_trips,
        }
    }
}
