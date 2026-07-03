//! The digital twin — the simulator the policy cannot tell from reality.
//!
//! It shares the CAL, HAL, and algorithm interfaces (it *is* the
//! [`EfficiencyOracle`] the brain queries), so a policy validated here behaves
//! identically in production. It evolves a coupled market: coin prices random-
//! walk and take scenario shocks, exogenous network hashrate migrates toward
//! profitable coins, and difficulty retargets toward the hashrate-implied level
//! **with a lag** — which is exactly the structure the forward-difficulty
//! forecaster exploits. Adversarial scenarios (price spike/crash, difficulty
//! spike, migration flood, pool outage, overheat, device crash) are injected on
//! a schedule. Everything is deterministic from a seed.

use crate::algo::AlgorithmRegistry;
use crate::cal::Market;
use crate::hal::{DamageDelta, EfficiencyPoint};
use crate::intelligence::EfficiencyOracle;
use crate::model::*;
use std::collections::BTreeMap;

// ── Deterministic PRNG (SplitMix64) ───────────────────────────────────────────
#[derive(Clone, Debug)]
struct Rng(u64);
impl Rng {
    fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }
    fn uniform(&mut self) -> f64 {
        (self.next_u64() >> 11) as f64 / (1u64 << 53) as f64
    }
    /// Standard normal via Box–Muller.
    fn normal(&mut self) -> f64 {
        let u1 = self.uniform().max(1e-12);
        let u2 = self.uniform();
        (-2.0 * u1.ln()).sqrt() * (std::f64::consts::TAU * u2).cos()
    }
}

// ── Scenario engine ───────────────────────────────────────────────────────────
#[derive(Clone, Debug)]
pub enum Scenario {
    PriceSpike { coin: CoinId, factor: f64 },
    PriceCrash { coin: CoinId, factor: f64 },
    DifficultySpike { coin: CoinId, factor: f64 },
    MigrationFlood { coin: CoinId, kappa_mult: f64, duration_s: f64 },
    PoolOutage { pool: PoolId, duration_s: f64 },
    EnergySpike { factor: f64, duration_s: f64 },
    DemandResponse { credit_usd_kwh: f64, duration_s: f64 },
    Overheat { device: DeviceId, ambient_add_c: f64, duration_s: f64 },
    Crash { device: DeviceId },
}

#[derive(Clone, Debug)]
pub struct ScheduledEvent {
    pub at_s: f64,
    pub scenario: Scenario,
    fired: bool,
}

impl ScheduledEvent {
    pub fn new(at_s: f64, scenario: Scenario) -> Self {
        ScheduledEvent {
            at_s,
            scenario,
            fired: false,
        }
    }
}

// ── Hidden device + coin + pool state ─────────────────────────────────────────
#[derive(Clone, Debug)]
struct SimDev {
    profile: DeviceProfile,
    temp_c: f64,
    damage: DamageState,
    /// Hidden true stable-clock edges (MHz) the autotuner must discover.
    core_edge_mhz: f64,
    mem_edge_mhz: f64,
    value_usd: f64,
    /// Extra ambient applied by an overheat scenario, decays after duration.
    overheat_add_c: f64,
    overheat_until: f64,
    fault_until: f64,
    fault_msg: Option<String>,
    last: DeviceTelemetry,
}

#[derive(Clone, Debug)]
struct SimCoin {
    desc: CoinDescriptor,
    price: f64,
    price_anchor_exo: f64,
    net_hashrate_ext: f64,
    difficulty: f64,
    kappa_mult: f64,
    kappa_until: f64,
}

#[derive(Clone, Debug)]
struct SimPool {
    base_latency_ms: f64,
    latency_ms: f64,
    online: bool,
    outage_until: f64,
    luck: f64,
}

#[derive(Clone, Copy, Debug)]
pub struct SimParams {
    pub dt_s: f64,
    pub migration_kappa: f64,
    pub price_vol: f64,
    pub equilibrium_margin: f64,
    pub seed: u64,
}

impl Default for SimParams {
    fn default() -> Self {
        SimParams {
            dt_s: 60.0,
            migration_kappa: 0.009,
            price_vol: 0.004,
            equilibrium_margin: 0.05,
            seed: 0x5EED_1234_ABCD_EF01_u64,
        }
    }
}

#[derive(Clone)]
pub struct SimWorld {
    pub params: SimParams,
    pub algos: AlgorithmRegistry,
    pub market: Market,
    coins: BTreeMap<CoinId, SimCoin>,
    devices: BTreeMap<DeviceId, SimDev>,
    pools: BTreeMap<PoolId, SimPool>,
    energy_price: f64,
    energy_base: f64,
    energy_spike_until: f64,
    ambient_c: f64,
    dr_credit: Option<f64>,
    dr_until: f64,
    t: f64,
    /// Exogenous market-noise RNG (fixed draws/tick) so a cloned world used for a
    /// baseline policy sees the *same* price path — a fair head-to-head. Device
    /// noise is deterministic per (seed, device, tick) via `det_normal`.
    market_rng: Rng,
    scenarios: Vec<ScheduledEvent>,
    confidence: f64,
    recent_abs_return: f64,
    s2r_err_ewma: f64,
    /// Our realized hashrate per coin from the last applied action.
    our_hash: BTreeMap<CoinId, f64>,
    last_realized_net_per_s: f64,
    last_gross_per_s: f64,
    last_energy_per_s: f64,
    last_wear_per_s: f64,
    last_power_w: f64,
}

// ── Physics (shared by prediction and realization → Sim2Real by construction) ──

fn logistic(x: f64) -> f64 {
    1.0 / (1.0 + (-x).exp())
}

fn phys_eff(
    dev: &SimDev,
    ap: &AlgoProfile,
    cap: &AlgoCapability,
    knobs: &Knobs,
    temp_c: f64,
) -> EfficiencyPoint {
    let sens = ap.mem_sensitivity;
    // Hashrate gain from clock offsets, weighted toward the bound subsystem.
    // Per-chip autotuning vs stock clocks delivers a real efficiency gain because
    // hashrate scales faster with clock than power does (undervolt-assisted), and
    // each chip's stable edge differs — a single global OC leaves it on the table.
    let mem_gain = 1.0 + 0.00050 * knobs.mem_offset_mhz * sens;
    let core_gain = 1.0 + 0.00060 * knobs.core_offset_mhz * (1.0 - sens);
    let mut hashrate = cap.stock_hashrate * mem_gain.max(0.1) * core_gain.max(0.1);

    // Power grows with clocks and voltage, but sub-linearly (undervolt headroom),
    // so the tuned point is more efficient — clamped by the power limit.
    let v_factor = 1.0 + 0.0006 * knobs.core_voltage_mv;
    let mut power = cap.stock_power_w
        * (1.0 + 0.00006 * knobs.core_offset_mhz + 0.00002 * knobs.mem_offset_mhz)
        * v_factor;
    if power > knobs.power_limit_w && power > 0.0 {
        let ratio = (knobs.power_limit_w / power).clamp(0.05, 1.0);
        hashrate *= ratio; // power-limited proportional throttle
        power = knobs.power_limit_w;
    }
    // Thermal derate near the limit.
    if temp_c > 78.0 {
        hashrate *= (1.0 - (temp_c - 78.0) * 0.012).max(0.5);
    }

    // Hardware error rate climbs as offsets approach/exceed the hidden edge.
    let e_core = logistic((knobs.core_offset_mhz - dev.core_edge_mhz) / 18.0);
    let e_mem = logistic((knobs.mem_offset_mhz - dev.mem_edge_mhz) / 18.0);
    let hw_error_rate = (e_core.max(e_mem) * 0.5).clamp(0.0, 0.9);

    EfficiencyPoint {
        hashrate: hashrate.max(0.0),
        power_w: power.max(0.0),
        hw_error_rate,
    }
}

fn phys_deg(dev: &SimDev, op: &OperatingPoint, dt_s: f64) -> DamageDelta {
    const EA: f64 = 0.7; // eV
    const K: f64 = 8.617e-5; // eV/K
    let t_ref = 273.15 + 65.0;
    // Clamp into a physical Kelvin band so an unphysical temp can never make the
    // Arrhenius exp() overflow to Inf and poison every downstream score.
    let t = (op.temp_c + 273.15).clamp(250.0, 500.0);
    let a_t = ((EA / K) * (1.0 / t_ref - 1.0 / t)).exp();
    // Electromigration: voltage + clock raise effective stress.
    let v_ref = 1000.0;
    let v = v_ref + op.knobs.core_voltage_mv + 0.2 * op.knobs.core_offset_mhz;
    let a_v = (v / v_ref).powf(3.0).max(0.1);
    let mttf_ref = 5.0 * 365.0 * 86400.0;
    let consumed = dt_s / mttf_ref * a_t * a_v;
    DamageDelta {
        consumed,
        usd_cost: consumed * dev.value_usd,
    }
}

impl EfficiencyOracle for SimWorld {
    fn efficiency(
        &self,
        dev: &DeviceId,
        algo: &AlgorithmId,
        knobs: &Knobs,
        temp_c: f64,
    ) -> EfficiencyPoint {
        if let (Some(d), Some(ap)) = (self.devices.get(dev), self.algos.get(algo)) {
            if let Some(cap) = d.profile.capability(algo) {
                return phys_eff(d, ap, cap, knobs, temp_c);
            }
        }
        EfficiencyPoint {
            hashrate: 0.0,
            power_w: 0.0,
            hw_error_rate: 0.0,
        }
    }

    fn degradation(&self, dev: &DeviceId, op: &OperatingPoint, dt_s: f64) -> DamageDelta {
        match self.devices.get(dev) {
            Some(d) => phys_deg(d, op, dt_s),
            None => DamageDelta::default(),
        }
    }
}

impl SimWorld {
    pub fn new(params: SimParams, algos: AlgorithmRegistry, market: Market) -> Self {
        SimWorld {
            market_rng: Rng(params.seed ^ 0x1234_5678_9ABC_DEF0),
            params,
            algos,
            market,
            coins: BTreeMap::new(),
            devices: BTreeMap::new(),
            pools: BTreeMap::new(),
            energy_price: 0.06,
            energy_base: 0.06,
            energy_spike_until: 0.0,
            ambient_c: 30.0,
            dr_credit: None,
            dr_until: 0.0,
            t: 0.0,
            scenarios: Vec::new(),
            confidence: 0.85,
            recent_abs_return: 0.0,
            s2r_err_ewma: 0.0,
            our_hash: BTreeMap::new(),
            last_realized_net_per_s: 0.0,
            last_gross_per_s: 0.0,
            last_energy_per_s: 0.0,
            last_wear_per_s: 0.0,
            last_power_w: 0.0,
        }
    }

    pub fn time(&self) -> f64 {
        self.t
    }
    pub fn confidence(&self) -> f64 {
        self.confidence
    }
    pub fn s2r_error(&self) -> f64 {
        self.s2r_err_ewma
    }
    pub fn energy_price(&self) -> f64 {
        self.energy_price
    }
    pub fn last_realized_net_per_s(&self) -> f64 {
        self.last_realized_net_per_s
    }
    pub fn last_gross_per_s(&self) -> f64 {
        self.last_gross_per_s
    }
    pub fn last_energy_per_s(&self) -> f64 {
        self.last_energy_per_s
    }
    pub fn last_wear_per_s(&self) -> f64 {
        self.last_wear_per_s
    }
    pub fn last_power_w(&self) -> f64 {
        self.last_power_w
    }
    /// Average temperature of running devices of a class (debug/inspection).
    pub fn avg_temp_class(&self, class: DeviceClass) -> f64 {
        let v: Vec<f64> = self
            .devices
            .values()
            .filter(|d| d.profile.class == class && d.last.power_w > 0.0)
            .map(|d| d.temp_c)
            .collect();
        if v.is_empty() {
            0.0
        } else {
            v.iter().sum::<f64>() / v.len() as f64
        }
    }
    pub fn dt(&self) -> f64 {
        self.params.dt_s
    }

    pub fn add_scenario(&mut self, ev: ScheduledEvent) {
        self.scenarios.push(ev);
    }

    pub fn device_profiles(&self) -> BTreeMap<DeviceId, DeviceProfile> {
        self.devices
            .iter()
            .map(|(k, v)| (k.clone(), v.profile.clone()))
            .collect()
    }

    pub fn sites(&self) -> Vec<SiteId> {
        let mut s: Vec<SiteId> = self
            .devices
            .values()
            .map(|d| d.profile.site.clone())
            .collect();
        s.sort();
        s.dedup();
        s
    }

    /// Set a coin's spot price (from a live market feed). Difficulty is left as
    /// configured; the forecaster models the forward path from margin.
    pub fn set_price(&mut self, coin: &str, usd: f64) {
        if usd <= 0.0 {
            return;
        }
        if let Some(c) = self.coins.get_mut(&CoinId::new(coin)) {
            c.price = usd;
            c.price_anchor_exo = usd;
        }
    }

    /// Set a coin's live network difficulty, expressed as expected **hashes per
    /// block** (the unit the revenue formula uses: `reward / difficulty` is coins
    /// per hash). The forecaster re-anchors its forward path around the new value.
    pub fn set_difficulty(&mut self, coin: &str, hashes_per_block: f64) {
        if !(hashes_per_block.is_finite() && hashes_per_block > 0.0) {
            return;
        }
        if let Some(c) = self.coins.get_mut(&CoinId::new(coin)) {
            c.difficulty = hashes_per_block;
        }
    }

    /// Set a coin's live block reward (coins issued per block, excluding fees).
    pub fn set_block_reward(&mut self, coin: &str, reward: f64) {
        if !(reward.is_finite() && reward > 0.0) {
            return;
        }
        if let Some(c) = self.coins.get_mut(&CoinId::new(coin)) {
            c.desc.block_reward = reward;
        }
    }

    /// Is this coin already in the market universe?
    pub fn has_coin(&self, coin: &str) -> bool {
        self.coins.contains_key(&CoinId::new(coin))
    }

    /// Register an operator-defined custom coin so KAIROS can rank/mine it. Given a
    /// ticker + algorithm, plus optional economics (price, network hashrate, block
    /// reward) that make its profit computable — pass 0 for any unknown and the
    /// coin still mines, it just ranks with placeholder economics. The coin becomes
    /// mineable when a device supports `algo` and a native kernel exists for it.
    pub fn add_custom_coin(&mut self, coin: &str, algo: &str, price: f64, net_hashrate: f64, block_reward: f64) {
        let id = CoinId::new(coin);
        if self.coins.contains_key(&id) {
            return;
        }
        let block_time_s = 60.0;
        let reward = if block_reward > 0.0 { block_reward } else { 1.0 };
        let net_h = if net_hashrate > 0.0 { net_hashrate } else { 1.0e12 };
        let desc = CoinDescriptor {
            id: id.clone(),
            algo: AlgorithmId::new(algo),
            inclusion: InclusionKind::Nakamoto,
            block_reward: reward,
            block_time_s,
            fee_per_block: 0.0,
            retarget_window_s: 3600.0,
            decimals: 8,
        };
        self.market.add_coin(desc.clone());
        self.coins.insert(
            id,
            SimCoin { desc, price: price.max(0.0), price_anchor_exo: price.max(0.0), net_hashrate_ext: net_h, difficulty: net_h * block_time_s, kappa_mult: 1.0, kappa_until: 0.0 },
        );
    }

    /// Apply operator pool configuration. Tags every auto pool with the wallet
    /// for its coin (so the username shows as in real miner software), then, for
    /// any coin with explicit `[[pool]]` entries, replaces the auto pools with the
    /// operator's configured connections (URL + user/worker + pass + scheme).
    pub fn configure_pools(
        &mut self,
        wallets: &BTreeMap<String, String>,
        resolved: &[crate::config::ResolvedPool],
    ) {
        for pd in self.market.pools.values_mut() {
            if pd.user.is_empty() {
                if let Some(w) = wallets.get(pd.coin.as_str()) {
                    pd.user = w.clone();
                }
            }
        }
        if resolved.is_empty() {
            return;
        }
        let coins: std::collections::BTreeSet<CoinId> =
            resolved.iter().map(|r| r.desc.coin.clone()).collect();
        let drop_ids: Vec<PoolId> = self
            .market
            .pools
            .iter()
            .filter(|(_, p)| coins.contains(&p.coin))
            .map(|(id, _)| id.clone())
            .collect();
        for id in &drop_ids {
            self.market.pools.remove(id);
            self.pools.remove(id);
        }
        for r in resolved {
            self.pools.insert(
                r.desc.id.clone(),
                SimPool {
                    base_latency_ms: r.latency_ms,
                    latency_ms: r.latency_ms,
                    online: true,
                    outage_until: 0.0,
                    luck: 1.0,
                },
            );
            self.market.add_pool(r.desc.clone());
        }
    }

    fn coin_margin(&self, c: &SimCoin) -> f64 {
        let eff = self
            .algos
            .get(&c.desc.algo)
            .map(|a| a.ref_efficiency_j_per_h)
            .unwrap_or(1.0);
        if eff <= 0.0 || c.difficulty <= 0.0 {
            return 0.0;
        }
        let rev_per_hs = (c.desc.block_reward + c.desc.fee_per_block) / c.difficulty * c.price;
        let yield_per_joule = rev_per_hs / eff;
        let cost_per_joule = self.energy_price / 3_600_000.0;
        if cost_per_joule <= 0.0 {
            return 0.0;
        }
        yield_per_joule / cost_per_joule - 1.0
    }

    fn fire_due_scenarios(&mut self) {
        let t = self.t;
        // Collect indices to fire to avoid borrow issues.
        let due: Vec<usize> = self
            .scenarios
            .iter()
            .enumerate()
            .filter(|(_, e)| !e.fired && e.at_s <= t)
            .map(|(i, _)| i)
            .collect();
        for i in due {
            let sc = self.scenarios[i].scenario.clone();
            self.scenarios[i].fired = true;
            self.confidence = (self.confidence - 0.4).max(0.2); // a shock lowers confidence
            match sc {
                Scenario::PriceSpike { coin, factor } => {
                    if let Some(c) = self.coins.get_mut(&coin) {
                        c.price *= factor;
                    }
                }
                Scenario::PriceCrash { coin, factor } => {
                    if let Some(c) = self.coins.get_mut(&coin) {
                        c.price *= factor;
                    }
                }
                Scenario::DifficultySpike { coin, factor } => {
                    if let Some(c) = self.coins.get_mut(&coin) {
                        c.difficulty *= factor;
                        c.net_hashrate_ext *= factor;
                    }
                }
                Scenario::MigrationFlood {
                    coin,
                    kappa_mult,
                    duration_s,
                } => {
                    if let Some(c) = self.coins.get_mut(&coin) {
                        c.kappa_mult = kappa_mult;
                        c.kappa_until = t + duration_s;
                    }
                }
                Scenario::PoolOutage { pool, duration_s } => {
                    if let Some(p) = self.pools.get_mut(&pool) {
                        p.online = false;
                        p.outage_until = t + duration_s;
                        p.latency_ms = 100_000.0;
                    }
                }
                Scenario::EnergySpike {
                    factor,
                    duration_s,
                } => {
                    self.energy_price = self.energy_base * factor;
                    self.energy_spike_until = t + duration_s;
                }
                Scenario::DemandResponse {
                    credit_usd_kwh,
                    duration_s,
                } => {
                    self.dr_credit = Some(credit_usd_kwh);
                    self.dr_until = t + duration_s;
                }
                Scenario::Overheat {
                    device,
                    ambient_add_c,
                    duration_s,
                } => {
                    if let Some(d) = self.devices.get_mut(&device) {
                        d.overheat_add_c = ambient_add_c;
                        d.overheat_until = t + duration_s;
                    }
                }
                Scenario::Crash { device } => {
                    if let Some(d) = self.devices.get_mut(&device) {
                        d.fault_until = t + 120.0;
                        d.fault_msg = Some("driver timeout".into());
                    }
                }
            }
        }
    }

    /// Build the [`Belief`] the policy reads. Observation noise is small so the
    /// twin and a learned real-world oracle diverge only slightly (Sim2Real).
    pub fn sense(&self) -> Belief {
        let mut coins = BTreeMap::new();
        for (id, c) in &self.coins {
            let net_h = (c.difficulty / c.desc.block_time_s).max(1.0);
            coins.insert(
                id.clone(),
                CoinBelief {
                    coin: id.clone(),
                    price_usd: c.price,
                    price_anchor_usd: c.price,
                    difficulty: c.difficulty,
                    network_hashrate: net_h,
                    forward_difficulty: Vec::new(),
                    revenue_vol: 0.0,
                    price_sigma: 0.0,
                },
            );
        }
        let mut pools = BTreeMap::new();
        for (id, p) in &self.pools {
            pools.insert(
                id.clone(),
                PoolBelief {
                    pool: id.clone(),
                    latency_ms: p.latency_ms,
                    stale_rate: 0.0,
                    luck: p.luck,
                    online: p.online,
                },
            );
        }
        let mut devices = BTreeMap::new();
        let mut health = BTreeMap::new();
        for (id, d) in &self.devices {
            devices.insert(id.clone(), d.last.clone());
            health.insert(
                id.clone(),
                HealthTelemetry {
                    id: id.clone(),
                    mem_error_trend: d.last.hw_error_rate,
                    fan_health: 1.0,
                    thermal_hours: d.damage.consumed * 5.0 * 365.0 * 24.0,
                    rul_frac: (1.0 - d.damage.consumed).clamp(0.0, 1.0),
                },
            );
        }
        let dr = if self.t < self.dr_until {
            self.dr_credit
        } else {
            None
        };
        Belief {
            t_secs: self.t,
            coins,
            pools,
            energy_price_usd_kwh: self.energy_price,
            energy_forecast_usd_kwh: Vec::new(),
            dr_credit_usd_kwh: dr,
            ambient_c: self.ambient_c,
            devices,
            health,
            confidence: self.confidence,
        }
    }

    /// Apply an action and advance the world by one tick.
    pub fn step(&mut self, action: &FleetAction) {
        self.fire_due_scenarios();
        let dt = self.params.dt_s;
        let t = self.t;
        let ambient = self.ambient_c;
        let energy_price = self.energy_price;

        // Disjoint borrows: read coins/pools/algos/market, mutate devices.
        let coins = &self.coins;
        let pools = &self.pools;
        let algos = &self.algos;
        let market = &self.market;
        let devices = &mut self.devices;

        let mut our_hash: BTreeMap<CoinId, f64> = BTreeMap::new();
        let mut realized_net = 0.0_f64;
        let mut gross_acc = 0.0_f64;
        let mut energy_acc = 0.0_f64;
        let mut wear_acc = 0.0_f64;
        let mut power_acc = 0.0_f64;
        let seed = self.params.seed;
        let tick_index = (t / dt) as u64;

        for (id, d) in devices.iter_mut() {
            let sp = action.setpoints.iter().find(|s| &s.device == id);
            let faulted = t < d.fault_until;
            let overheating = t < d.overheat_until;
            let ambient_eff = ambient + if overheating { d.overheat_add_c } else { 0.0 };

            let mut tel = DeviceTelemetry {
                id: id.clone(),
                online: !faulted,
                temp_c: d.temp_c,
                power_w: 0.0,
                hashrate: 0.0,
                reject_rate: 0.0,
                hw_error_rate: 0.0,
                fan_pct: 60.0,
                fault: if faulted { d.fault_msg.clone() } else { None },
            };

            let mut power = 0.0;
            if let (Some(sp), false) = (sp, faulted) {
                tel.fan_pct = sp.knobs.fan_pct;
                if let Some(asg) = &sp.assignment {
                    if let (Some(ap), Some(cap)) =
                        (algos.get(&asg.algo), d.profile.capability(&asg.algo))
                    {
                        let eff = phys_eff(d, ap, cap, &sp.knobs, d.temp_c);
                        let noise = 1.0 + 0.01 * det_normal(seed, id.as_str(), tick_index);
                        let hashrate = (eff.hashrate * noise).max(0.0);
                        power = eff.power_w;
                        // Realized stale: pool path + clock instability.
                        let (latency, online, _luck) = pools
                            .get(&asg.pool)
                            .map(|p| (p.latency_ms, p.online, p.luck))
                            .unwrap_or((60.0, true, 1.0));
                        let block_time = coins
                            .get(&asg.coin)
                            .map(|c| c.desc.block_time_s)
                            .unwrap_or(60.0);
                        let stale = if !online {
                            1.0
                        } else {
                            (0.001 + (latency / 1000.0 / block_time) * 0.5 + eff.hw_error_rate)
                                .clamp(0.0, 0.95)
                        };
                        tel.hashrate = hashrate;
                        tel.power_w = power;
                        tel.hw_error_rate = eff.hw_error_rate;
                        tel.reject_rate = stale;

                        // Energy and lifespan are consumed whenever the device runs
                        // — including during a pool outage, when it still draws
                        // power and degrades but earns nothing. Only the revenue
                        // term is gated on the pool being online.
                        let energy = power * energy_price / 3_600_000.0;
                        let op = OperatingPoint {
                            algo: asg.algo.clone(),
                            knobs: sp.knobs,
                            temp_c: d.temp_c,
                        };
                        let wear = phys_deg(d, &op, 1.0).usd_cost;
                        energy_acc += energy;
                        wear_acc += wear;

                        let mut rev = 0.0;
                        if online {
                            *our_hash.entry(asg.coin.clone()).or_insert(0.0) += hashrate;
                            if let Some(c) = coins.get(&asg.coin) {
                                let rev_per_hs = (c.desc.block_reward + c.desc.fee_per_block)
                                    / c.difficulty.max(1.0)
                                    * c.price;
                                let pool = market.pool(&asg.pool);
                                let (fee, solv) = pool
                                    .map(|p| (p.fee_frac, p.solvency_risk))
                                    .unwrap_or((0.01, 0.0));
                                rev = hashrate
                                    * rev_per_hs
                                    * (1.0 - fee)
                                    * (1.0 - solv)
                                    * (1.0 - stale);
                                gross_acc += rev;
                            }
                        }
                        realized_net += rev - energy - wear;
                    }
                } else {
                    // Idle: just the idle power draw.
                    power = sp.knobs.power_limit_w;
                    tel.power_w = power;
                }
            }

            // Thermal update toward equilibrium.
            let k_th = match d.profile.class {
                DeviceClass::Gpu => 0.38,
                DeviceClass::Asic | DeviceClass::Fpga => 0.012,
            };
            let fan = tel.fan_pct.max(20.0);
            let t_eq = ambient_eff + power * k_th * (1.0 - fan / 300.0);
            let tau = 120.0;
            d.temp_c = (d.temp_c + (t_eq - d.temp_c) * (dt / tau)).clamp(-20.0, 200.0);
            tel.temp_c = d.temp_c;
            power_acc += power;

            // Accumulate degradation for the running point.
            if power > 0.0 {
                let op = OperatingPoint {
                    algo: AlgorithmId::new("_"),
                    knobs: sp.map(|s| s.knobs).unwrap_or(Knobs::stock(power)),
                    temp_c: d.temp_c,
                };
                let dmg = phys_deg(d, &op, dt);
                d.damage.consumed = (d.damage.consumed + dmg.consumed).min(1.0);
            }

            d.last = tel;
        }

        self.our_hash = our_hash;
        self.last_realized_net_per_s = realized_net;
        self.last_gross_per_s = gross_acc;
        self.last_energy_per_s = energy_acc;
        self.last_wear_per_s = wear_acc;
        self.last_power_w = power_acc;

        // ── Evolve the market ──────────────────────────────────────────────────
        let dt_hours = dt / 3600.0;
        let base_kappa = self.params.migration_kappa;
        let eq = self.params.equilibrium_margin;
        let price_vol = self.params.price_vol;
        // Need margins computed with current energy price.
        let coin_ids: Vec<CoinId> = self.coins.keys().cloned().collect();
        let mut total_abs_return = 0.0;
        for id in &coin_ids {
            let margin = {
                let c = &self.coins[id];
                self.coin_margin(c)
            };
            let our_h = self.our_hash.get(id).copied().unwrap_or(0.0);
            let c = self.coins.get_mut(id).unwrap();
            // Price: random walk + slow mean reversion to the exogenous anchor.
            // Uses the market RNG (fixed draws/tick) so a baseline clone sees the
            // identical exogenous price path.
            let ret = price_vol * rng_normal(&mut self.market_rng);
            total_abs_return += ret.abs();
            c.price *= 1.0 + ret;
            c.price += (c.price_anchor_exo - c.price) * 0.02 * dt_hours;
            c.price = c.price.max(1e-9);
            // Exogenous hashrate migrates toward profitability (reflexive).
            let kappa = if t < c.kappa_until {
                base_kappa * c.kappa_mult
            } else {
                base_kappa
            };
            c.net_hashrate_ext +=
                kappa * c.net_hashrate_ext * (margin - eq) * dt_hours;
            c.net_hashrate_ext = c.net_hashrate_ext.max(1.0);
            // Difficulty retargets toward implied total hashrate, lagged.
            let total_h = c.net_hashrate_ext + our_h;
            let target_diff = total_h * c.desc.block_time_s;
            let tau = c.desc.retarget_window_s.max(dt);
            c.difficulty += (target_diff - c.difficulty) * (dt / tau);
            c.difficulty = c.difficulty.max(1.0);
        }

        // Energy + DR + pool-outage expiries.
        if self.t >= self.energy_spike_until && self.energy_price != self.energy_base {
            self.energy_price = self.energy_base;
        }
        for p in self.pools.values_mut() {
            if !p.online && self.t >= p.outage_until {
                p.online = true;
                p.latency_ms = p.base_latency_ms;
            }
        }
        for d in self.devices.values_mut() {
            if self.t >= d.overheat_until {
                d.overheat_add_c = 0.0;
            }
        }

        // Confidence recovers toward calm as volatility subsides.
        let abs_ret = total_abs_return / coin_ids.len().max(1) as f64;
        self.recent_abs_return += 0.2 * (abs_ret - self.recent_abs_return);
        let calm = (0.92 - 12.0 * self.recent_abs_return).clamp(0.25, 0.95);
        self.confidence += 0.1 * (calm - self.confidence);

        self.t += dt;
    }

    /// Track Sim2Real error: the gap between the brain's predicted net and the
    /// twin's realized net, as a normalized EWMA. A regression alarm in shadow.
    pub fn correct(&mut self, predicted_net_per_s: f64, realized_net_per_s: f64) {
        let denom = realized_net_per_s.abs().max(1e-6);
        let err = (predicted_net_per_s - realized_net_per_s).abs() / denom;
        self.s2r_err_ewma += 0.1 * (err - self.s2r_err_ewma);
    }
}

fn rng_normal(rng: &mut Rng) -> f64 {
    rng.normal()
}

/// Deterministic standard-normal device noise keyed by (seed, device, tick).
/// Both the live and baseline clones share the seed and device ids, so they
/// replay *identical* per-device stochastics — the only thing that differs
/// between the two worlds is the policy's decisions, keeping proof-of-uplift a
/// pure measure of decision quality rather than luck.
fn det_normal(seed: u64, device_id: &str, tick: u64) -> f64 {
    let mut h = seed ^ 0x9E37_79B9_7F4A_7C15;
    for b in device_id.bytes() {
        h = (h ^ b as u64).wrapping_mul(0x0000_0100_0000_01B3);
    }
    h ^= tick.wrapping_mul(0xD1B5_4A32_D192_ED03);
    Rng(h).normal()
}

// ── Builders: a representative mixed ASIC+GPU fleet and a coin/pool universe ───

fn coin(
    id: &str,
    algo: &str,
    reward: f64,
    fee: f64,
    block_time_s: f64,
    _price: f64,
    net_hashrate: f64,
    retarget_window_s: f64,
    inclusion: InclusionKind,
    decimals: u8,
) -> (CoinDescriptor, f64) {
    let difficulty = net_hashrate * block_time_s;
    (
        CoinDescriptor {
            id: id.into(),
            algo: algo.into(),
            inclusion,
            block_reward: reward,
            block_time_s,
            fee_per_block: fee,
            retarget_window_s,
            decimals,
        },
        difficulty,
    )
}

fn pool(id: &str, coin: &str, scheme: RewardScheme, fee: f64, solvency: f64, url: &str) -> PoolDescriptor {
    PoolDescriptor {
        id: id.into(),
        coin: coin.into(),
        scheme,
        fee_frac: fee,
        solvency_risk: solvency,
        url: url.into(),
        user: String::new(),
        pass: "x".into(),
        priority: 0,
    }
}

fn gpu_limits() -> DeviceLimits {
    DeviceLimits {
        max_power_w: 200.0,
        max_temp_c: 80.0,
        max_core_voltage_mv: 1100.0,
        max_core_offset_mhz: 250.0,
        max_mem_offset_mhz: 1800.0,
        min_fan_pct: 30.0,
    }
}

fn asic_limits() -> DeviceLimits {
    DeviceLimits {
        max_power_w: 2200.0,
        max_temp_c: 85.0,
        max_core_voltage_mv: 50.0,
        max_core_offset_mhz: 130.0,
        max_mem_offset_mhz: 130.0,
        min_fan_pct: 40.0,
    }
}

fn init_tel(id: &DeviceId, temp: f64) -> DeviceTelemetry {
    DeviceTelemetry {
        id: id.clone(),
        online: true,
        temp_c: temp,
        power_w: 0.0,
        hashrate: 0.0,
        reject_rate: 0.0,
        hw_error_rate: 0.0,
        fan_pct: 60.0,
        fault: None,
    }
}

fn make_device(
    site: &str,
    id: &str,
    class: DeviceClass,
    model: &str,
    caps: Vec<AlgoCapability>,
    limits: DeviceLimits,
    quality: f64,
    value_usd: f64,
    ambient: f64,
) -> SimDev {
    let did = DeviceId::new(id);
    // Hidden stable-clock edges (MHz). Set well above 0 so a *stock* device is
    // stable (no errors), but reachable within the device's offset ceiling so the
    // autotuner has real headroom to find on each chip.
    let (core_edge, mem_edge) = match class {
        DeviceClass::Gpu => (200.0 * quality, 1500.0 * quality),
        DeviceClass::Asic | DeviceClass::Fpga => (185.0 * quality, 185.0 * quality),
    };
    let profile = DeviceProfile {
        id: did.clone(),
        site: SiteId::new(site),
        class,
        model: model.into(),
        capabilities: caps,
        limits,
        silicon_quality: quality,
    };
    SimDev {
        profile,
        temp_c: ambient + 5.0,
        damage: DamageState::default(),
        core_edge_mhz: core_edge,
        mem_edge_mhz: mem_edge,
        value_usd,
        overheat_add_c: 0.0,
        overheat_until: 0.0,
        fault_until: 0.0,
        fault_msg: None,
        last: init_tel(&did, ambient + 5.0),
    }
}

fn cap(algo: &str, h: f64, p: f64, dual: bool) -> AlgoCapability {
    AlgoCapability {
        algo: algo.into(),
        stock_hashrate: h,
        stock_power_w: p,
        dual_capable: dual,
    }
}

/// A representative world: 2 sites, mixed ASIC+GPU fleet, 4 coins, 8 pools.
pub fn build_default_world(seed: u64) -> SimWorld {
    let algos = AlgorithmRegistry::with_defaults();
    let mut market = Market::new();

    // Coins (difficulty derived from a target network hashrate).
    // Network hashrates calibrated so a representative fleet device earns a
    // realistic, modest margin (~20–40%) on each coin.
    let coins_spec = [
        coin("BTC", "SHA-256", 3.125, 0.40, 600.0, 65000.0, 6.6e20, 1_209_600.0, InclusionKind::Nakamoto, 8),
        coin("KAS", "kHeavyHash", 80.0, 0.10, 1.0, 0.12, 1.55e15, 3600.0, InclusionKind::DagOrdered, 8),
        coin("ERG", "Autolykos2", 12.0, 0.05, 120.0, 1.50, 4.3e12, 7200.0, InclusionKind::Nakamoto, 9),
        coin("ETC", "Ethash", 2.56, 0.05, 13.0, 22.0, 3.5e13, 7200.0, InclusionKind::Nakamoto, 18),
    ];
    let mut params = SimParams::default();
    params.seed = seed;
    let mut world = SimWorld::new(params, algos, Market::new());

    for (desc, difficulty) in coins_spec {
        let price = match desc.id.as_str() {
            "BTC" => 65000.0,
            "KAS" => 0.12,
            "ERG" => 1.50,
            _ => 22.0,
        };
        market.add_coin(desc.clone());
        world.coins.insert(
            desc.id.clone(),
            SimCoin {
                desc: desc.clone(),
                price,
                price_anchor_exo: price,
                net_hashrate_ext: difficulty / desc.block_time_s,
                difficulty,
                kappa_mult: 1.0,
                kappa_until: 0.0,
            },
        );
    }

    // Pools: a primary and an alternate per coin, differing in scheme/fee/latency.
    let pool_spec = [
        pool("btc-main", "BTC", RewardScheme::Fpps, 0.010, 0.001, "stratum+tcp://btc-main:3333"),
        pool("btc-alt", "BTC", RewardScheme::Pplns, 0.006, 0.004, "stratum+tcp://btc-alt:3333"),
        pool("kas-main", "KAS", RewardScheme::Fpps, 0.010, 0.001, "stratum+tcp://kas-main:4444"),
        pool("kas-alt", "KAS", RewardScheme::Pplns, 0.007, 0.003, "stratum+tcp://kas-alt:4444"),
        pool("erg-main", "ERG", RewardScheme::Pplns, 0.010, 0.002, "stratum+tcp://erg-main:3000"),
        pool("erg-alt", "ERG", RewardScheme::Pps, 0.018, 0.001, "stratum+tcp://erg-alt:3000"),
        pool("etc-main", "ETC", RewardScheme::Pplns, 0.009, 0.002, "stratum+tcp://etc-main:8008"),
        pool("etc-alt", "ETC", RewardScheme::Fpps, 0.013, 0.001, "stratum+tcp://etc-alt:8008"),
    ];
    let pool_latency = |id: &str| -> f64 {
        match id {
            "btc-main" => 41.0,
            "btc-alt" => 78.0,
            "kas-main" => 28.0,
            "kas-alt" => 60.0,
            "erg-main" => 66.0,
            "erg-alt" => 44.0,
            "etc-main" => 52.0,
            "etc-alt" => 39.0,
            _ => 60.0,
        }
    };
    for p in pool_spec {
        let lat = pool_latency(p.id.as_str());
        world.pools.insert(
            p.id.clone(),
            SimPool {
                base_latency_ms: lat,
                latency_ms: lat,
                online: true,
                outage_until: 0.0,
                luck: 1.0,
            },
        );
        market.add_pool(p);
    }
    world.market = market;

    // Devices.
    let ambient = world.ambient_c;
    let n_tx_gpu = 10;
    let n_tx_asic = 6;
    let n_is_gpu = 6;
    let total = (n_tx_gpu + n_is_gpu + n_tx_asic) as f64;
    let mut k = 0.0;
    let q = |k: &mut f64| -> f64 {
        *k += 1.0;
        0.93 + 0.14 * (*k / total)
    };

    // GPUs are flexible capital: every GPU can serve every GPU algorithm, so the
    // cross-algorithm switcher always has somewhere profitable to steer them.
    let gpu_caps = || {
        vec![
            cap("kHeavyHash", 1.5e9, 140.0, false),
            cap("Autolykos2", 2.0e8, 115.0, false),
            cap("Ethash", 6.0e7, 130.0, false),
        ]
    };
    for i in 1..=n_tx_gpu {
        let id = format!("TX-01/g{:03}", i);
        world.devices.insert(
            DeviceId::new(&id),
            make_device("TX-01", &id, DeviceClass::Gpu, "GPU-A", gpu_caps(), gpu_limits(), q(&mut k), 600.0, ambient),
        );
    }
    for i in 1..=n_tx_asic {
        let id = format!("TX-01/a{:03}", i);
        let caps = vec![cap("SHA-256", 90.0e12, 1900.0, false)];
        world.devices.insert(
            DeviceId::new(&id),
            make_device("TX-01", &id, DeviceClass::Asic, "ASIC-S", caps, asic_limits(), q(&mut k), 3000.0, ambient),
        );
    }
    for i in 1..=n_is_gpu {
        let id = format!("IS-02/g{:03}", i);
        world.devices.insert(
            DeviceId::new(&id),
            make_device("IS-02", &id, DeviceClass::Gpu, "GPU-B", gpu_caps(), gpu_limits(), q(&mut k), 600.0, ambient),
        );
    }

    world
}

/// Build a world for LIVE mode: real-network coin economics (approximate current
/// network hashrates + rewards) and placeholder pools per coin the operator
/// overrides via `[[pool]]`. Prices are refreshed from a live feed. No simulated
/// devices — live mode uses the real detected hardware. Kept separate from
/// `build_default_world` so the twin's calibrated sim/tests are untouched.
pub fn build_live_world(seed: u64) -> SimWorld {
    let algos = AlgorithmRegistry::with_defaults();
    let mut market = Market::new();
    let specs = [
        // (id, algo, reward, fee, block_s, price0, net_hashrate, retarget_s, inclusion, decimals)
        coin("BTC", "SHA-256", 3.125, 0.40, 600.0, 58560.0, 6.0e20, 1_209_600.0, InclusionKind::Nakamoto, 8),
        coin("KAS", "kHeavyHash", 55.0, 0.05, 1.0, 0.0296, 1.2e18, 3600.0, InclusionKind::DagOrdered, 8),
        coin("ERG", "Autolykos2", 3.0, 0.02, 120.0, 0.19, 1.6e13, 7200.0, InclusionKind::Nakamoto, 9),
        coin("ETC", "Ethash", 2.048, 0.03, 13.0, 6.85, 2.6e14, 7200.0, InclusionKind::Nakamoto, 18),
        coin("RVN", "KawPow", 2500.0, 1.0, 60.0, 0.0134, 7.0e12, 3600.0, InclusionKind::Nakamoto, 8),
        coin("LTC", "Scrypt", 6.25, 0.02, 150.0, 65.0, 2.2e15, 302_400.0, InclusionKind::Nakamoto, 8),
        // More coins KAIROS can mine TODAY (Bitcoin-family Stratum V1: SHA-256d / scrypt).
        coin("DOGE", "Scrypt", 10000.0, 2.0, 60.0, 0.085, 1.9e15, 302_400.0, InclusionKind::Nakamoto, 8),
        coin("BCH", "SHA-256", 3.125, 0.05, 600.0, 350.0, 4.6e18, 1_209_600.0, InclusionKind::Nakamoto, 8),
        coin("DGB", "Scrypt", 665.0, 0.1, 15.0, 0.0075, 1.1e14, 3600.0, InclusionKind::Nakamoto, 8),
    ];
    let mut params = SimParams::default();
    params.seed = seed;
    let mut world = SimWorld::new(params, algos, Market::new());
    for (desc, difficulty) in specs {
        let price = match desc.id.as_str() {
            "BTC" => 58560.0,
            "KAS" => 0.0296,
            "ERG" => 0.19,
            "ETC" => 6.85,
            "LTC" => 65.0,
            "DOGE" => 0.085,
            "BCH" => 350.0,
            "DGB" => 0.0075,
            _ => 0.0134,
        };
        market.add_coin(desc.clone());
        world.coins.insert(
            desc.id.clone(),
            SimCoin {
                desc: desc.clone(),
                price,
                price_anchor_exo: price,
                net_hashrate_ext: difficulty / desc.block_time_s,
                difficulty,
                kappa_mult: 1.0,
                kappa_until: 0.0,
            },
        );
        // A placeholder pool per coin — the operator overrides with [[pool]].
        let pid = format!("{}-pool", desc.id.as_str().to_lowercase());
        let pd = pool(&pid, desc.id.as_str(), RewardScheme::Fpps, 0.01, 0.002, "stratum+tcp://configure-your-pool:0");
        world.pools.insert(
            PoolId::new(&pid),
            SimPool { base_latency_ms: 50.0, latency_ms: 50.0, online: true, outage_until: 0.0, luck: 1.0 },
        );
        market.add_pool(pd);
    }
    world.market = market;
    world
}

/// A generated adversarial scenario schedule for the "generated scenario" run:
/// an ERG mispricing window then a migration flood, an energy spike, a demand-
/// response window, a pool outage, a device overheat, and a crash.
pub fn default_scenarios() -> Vec<ScheduledEvent> {
    let h = 3600.0;
    vec![
        // ERG price spikes (mispricing window opens), then hashrate floods in.
        ScheduledEvent::new(4.0 * h, Scenario::PriceSpike { coin: CoinId::new("ERG"), factor: 2.6 }),
        ScheduledEvent::new(5.0 * h, Scenario::MigrationFlood { coin: CoinId::new("ERG"), kappa_mult: 5.0, duration_s: 4.0 * h }),
        // Energy spike → marginal-efficiency curtailment.
        ScheduledEvent::new(10.0 * h, Scenario::EnergySpike { factor: 2.4, duration_s: 3.0 * h }),
        // Grid demand-response window. Mining margins are thin, so the engine
        // sells interruptibility to the grid when the credit beats the mining
        // margin — income the incumbent ignores entirely.
        ScheduledEvent::new(14.0 * h, Scenario::DemandResponse { credit_usd_kwh: 0.12, duration_s: 0.75 * h }),
        // Pool outage on the primary KAS pool → warm failover.
        ScheduledEvent::new(16.0 * h, Scenario::PoolOutage { pool: PoolId::new("kas-main"), duration_s: 1.0 * h }),
        // A device overheats → thermal shield idles it.
        ScheduledEvent::new(18.0 * h, Scenario::Overheat { device: DeviceId::new("TX-01/g004"), ambient_add_c: 60.0, duration_s: 1.0 * h }),
        // A device crashes → self-healing restart.
        ScheduledEvent::new(19.0 * h, Scenario::Crash { device: DeviceId::new("TX-01/g007") }),
        // KAS price slips back (relative rotation).
        ScheduledEvent::new(20.0 * h, Scenario::PriceCrash { coin: CoinId::new("KAS"), factor: 0.85 }),
    ]
}
