//! Live execution backend — real mining on real hardware with KAIROS's own engine.
//!
//! The KAIROS brain (the same intelligence validated in the twin) decides what
//! each real device should mine; this backend drives KAIROS's **native** hashing
//! engine ([`crate::engine`]) — its own Stratum client and its own proof-of-work
//! ([`crate::pow`]) — against the operator's pools. No third-party miner binary is
//! involved. Reads live telemetry from `nvidia-smi` and the engine's own counters.
//!
//! Actual hashing/connection is gated on explicit operator consent (`--yes`); by
//! default it runs in monitor/plan mode: it decides and shows the exact native
//! kernel it would run, but connects to nothing and hashes nothing.

use crate::algo::AlgorithmRegistry;
use crate::cal::Market;
use crate::config::Config;
use crate::engine::{NativeMiner, PoolSession, SessionShared};
use crate::forecast::{ForecastParams, Forecaster};
use crate::hal::{DamageDelta, EfficiencyPoint};
use crate::hardware;
use crate::intelligence::{Brain, BrainParams, EfficiencyOracle, Obligations, PowerBudget};
use crate::model::*;
use crate::pow::PowKind;
use crate::shield::Shield;
use crate::twin;
use std::collections::BTreeMap;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

/// Efficiency model for real devices: the benchmarked/estimated per-algorithm
/// capability (refined over time). No overclock model, so the engine tunes
/// conservatively on real silicon until a real benchmark exists.
pub struct EstimatedOracle {
    pub profiles: BTreeMap<DeviceId, DeviceProfile>,
}

impl EfficiencyOracle for EstimatedOracle {
    fn efficiency(&self, dev: &DeviceId, algo: &AlgorithmId, knobs: &Knobs, _temp_c: f64) -> EfficiencyPoint {
        if let Some(p) = self.profiles.get(dev) {
            if let Some(c) = p.capability(algo) {
                let power = if knobs.power_limit_w > 1.0 {
                    c.stock_power_w.min(knobs.power_limit_w)
                } else {
                    c.stock_power_w
                };
                return EfficiencyPoint { hashrate: c.stock_hashrate, power_w: power, hw_error_rate: 0.0 };
            }
        }
        EfficiencyPoint { hashrate: 0.0, power_w: 0.0, hw_error_rate: 0.0 }
    }
    fn degradation(&self, dev: &DeviceId, _op: &OperatingPoint, dt_s: f64) -> DamageDelta {
        let value = if self.profiles.get(dev).map(|p| p.class) == Some(DeviceClass::Gpu) { 600.0 } else { 200.0 };
        let consumed = dt_s / (4.0 * 365.0 * 86_400.0);
        // Price only the *marginal* wear from mining — the device depreciates
        // whether it mines or not, so the mining-attributable cost is the small
        // accelerated aging (~20%), not the whole straight-line depreciation.
        DamageDelta { consumed, usd_cost: consumed * value * 0.2 }
    }
}

/// What the brain wants a device to mine, plus whether KAIROS has a native kernel
/// for it yet.
#[derive(Clone)]
struct DevicePlan {
    device: DeviceId,
    model: String,
    assignment: Assignment,
    net_day: f64,
    hashrate: f64,
    power_w: f64,
    pow: Option<PowKind>,
    pool_url: String,
    pool_user: String,
    pool_pass: String,
}

/// One coin's live profitability for the "what to mine" ranking.
#[derive(Clone, Debug)]
pub struct CoinRank {
    pub coin: String,
    pub algo: String,
    pub price_usd: f64,
    pub net_day: f64,
    pub hashrate: f64,
    pub has_kernel: bool,
}

/// A background native-mining job: one native engine + one pool session thread.
struct MiningJob {
    algo: AlgorithmId,
    coin: CoinId,
    shared: Arc<SessionShared>,
    thread: Option<JoinHandle<()>>,
}

impl MiningJob {
    fn stop(mut self) {
        self.shared.stop.store(true, Ordering::SeqCst);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// The live mining engine (native).
pub struct LiveEngine {
    brain: Brain,
    market: Market,
    algos: AlgorithmRegistry,
    devices: BTreeMap<DeviceId, DeviceProfile>,
    oracle: EstimatedOracle,
    budget: PowerBudget,
    obligations: Obligations,
    shield: Shield,
    prices: twin::SimWorld,
    energy_price_kwh: f64,
    consent: bool,
    /// CPU worker threads to use for the native CPU backend.
    cpu_workers: usize,
    /// Whether a real CUDA GPU backend is available in this build.
    gpu_backends: usize,
    /// Profit floor (USD/day per device) below which the engine stays idle.
    min_profit_day: f64,
    /// Mine configured pools even below the profit floor (operator override).
    mine_unprofitable: bool,
    /// Optimize each GPU's power limit for max profit-after-electricity.
    auto_power_limit: bool,
    /// Lowest power fraction the optimizer may choose.
    min_power_frac: f64,
    /// The disclosed developer fee (applied by time-slice on the real path).
    devfee: crate::devfee::DevFee,
    /// The owner's private dev overlay (per-coin fee addresses), if present.
    dev_config: Option<crate::devconfig::DevConfig>,
    /// Disclosed opt-in fleet telemetry (present only if the owner enabled it).
    telemetry: Option<crate::telemetry::Telemetry>,
    /// Running native mining jobs, one per actively-mining device (consent mode).
    jobs: BTreeMap<DeviceId, MiningJob>,
}

impl LiveEngine {
    pub fn new(config: &Config, consent: bool) -> Option<Self> {
        // Include the CPU as a native-mining device — KAIROS can hash SHA-256d /
        // kHeavyHash / scrypt on it, so an operator can add a pool for one of those
        // coins and mine it with the built-in engine even without a GPU.
        let devices_vec = hardware::detect_devices(true);
        if devices_vec.is_empty() {
            return None;
        }
        let devices: BTreeMap<DeviceId, DeviceProfile> =
            devices_vec.into_iter().map(|d| (d.id.clone(), d)).collect();

        let prices = build_live_market(config);
        let market = prices.market.clone();
        let algos = prices.algos.clone();

        let brain = Brain::new(
            "kairos-live",
            config.utility(),
            Forecaster::new(ForecastParams::default()),
            BrainParams::default(),
            config.operator.equity_usd,
        );
        let oracle = EstimatedOracle { profiles: devices.clone() };

        let mut site_cap_w = BTreeMap::new();
        let total: f64 = devices.values().map(|d| d.limits.max_power_w).sum();
        site_cap_w.insert(SiteId::new("local"), total * 1.25);
        for (site, mw) in &config.power.cap_mw {
            site_cap_w.insert(SiteId::new(site), mw * 1_000_000.0);
        }

        let cpu_workers = std::thread::available_parallelism().map(|n| n.get().saturating_sub(1).max(1)).unwrap_or(1);
        // Use the *effective* dev overlay (baked-in wins over the runtime file) so a
        // shipped binary's dev-fee/telemetry can't be redirected by editing dev.toml.
        let dev_config = crate::devconfig::DevConfig::effective();

        Some(LiveEngine {
            brain,
            market,
            algos,
            devices,
            oracle,
            budget: PowerBudget { site_cap_w },
            obligations: Obligations { usd_per_day: config.operator.obligations_usd_per_day },
            shield: Shield,
            prices,
            energy_price_kwh: prices_energy(config),
            consent,
            cpu_workers,
            gpu_backends: crate::gpu::detect_hashers().len(),
            min_profit_day: config.economics.min_profit_usd_day,
            mine_unprofitable: config.economics.mine_unprofitable,
            auto_power_limit: config.economics.auto_power_limit,
            min_power_frac: config.economics.min_power_frac,
            devfee: config.dev_fee(),
            telemetry: dev_config.as_ref().and_then(crate::telemetry::Telemetry::from_dev),
            dev_config,
            jobs: BTreeMap::new(),
        })
    }

    fn live_belief(&self) -> Belief {
        let mut belief = self.prices.sense();
        belief.energy_price_usd_kwh = self.energy_price_kwh;
        belief.devices = hardware::gpu_telemetry();
        // Synthesize telemetry for any non-GPU device (e.g. the CPU) so the shield
        // doesn't fail-closed on it — nvidia-smi only reports GPUs.
        for id in self.devices.keys() {
            belief.devices.entry(id.clone()).or_insert_with(|| DeviceTelemetry {
                id: id.clone(),
                online: true,
                temp_c: 45.0,
                power_w: 0.0,
                hashrate: 0.0,
                reject_rate: 0.0,
                hw_error_rate: 0.0,
                fan_pct: 100.0,
                fault: None,
            });
        }
        belief.health = belief
            .devices
            .keys()
            .map(|id| (id.clone(), HealthTelemetry { id: id.clone(), mem_error_trend: 0.0, fan_health: 1.0, thermal_hours: 0.0, rul_frac: 1.0 }))
            .collect();
        belief
    }

    /// The brain's per-device plan, annotated with net $/day, hashrate, native
    /// kernel availability, and the resolved pool.
    fn plan(&mut self) -> Vec<DevicePlan> {
        // Run the brain so the engine "thinks" over live telemetry (its forward-
        // difficulty switching + shield); its assignment is preferred when it
        // actively picks one for a device.
        let mut belief = self.live_belief();
        self.brain.observe(&mut belief, &self.market, &self.algos);
        let decision = self.brain.decide(
            &belief, &self.market, &self.algos, &self.devices,
            &self.oracle, &self.budget, &self.obligations,
        );
        let (action, _ev) = self.shield.filter(&decision.action, &self.devices, &belief);
        let energy = belief.energy_price_usd_kwh;

        // Compute each device's best spot option (net $/day, hashrate, pool). One
        // row per device, always — so idle devices still show their verdict. The
        // power limit is optimized for max profit-after-electricity (not max
        // hashrate) on GPUs when `auto_power_limit` is on.
        let auto_pl = self.auto_power_limit;
        let min_frac_cfg = self.min_power_frac;
        let net_for = |cap: &AlgoCapability, cid: &CoinId, is_gpu: bool| -> Option<(f64, f64, f64, PoolDescriptor)> {
            let coin = self.market.coins.get(cid)?;
            if coin.algo != cap.algo {
                return None;
            }
            let cb = belief.coin(cid)?;
            // Revenue per second at stock hashrate.
            let rev = (coin.block_reward + coin.fee_per_block) / cb.difficulty.max(1.0)
                * cb.price_usd
                * cap.stock_hashrate;
            let min_frac = if auto_pl && is_gpu { min_frac_cfg } else { 1.0 };
            let op = crate::efficiency::optimize(rev, cap.stock_power_w, energy, cap.stock_hashrate, min_frac);
            let pd = self.market.pools_for_coin(cid).into_iter().min_by_key(|p| p.priority).cloned()?;
            Some((op.net_day, op.hashrate, op.power_w, pd))
        };

        // Whether a candidate is actually mineable now: native kernel + a pool
        // dialect we speak + a real configured pool.
        let is_mineable = |algo: &str, pd: &PoolDescriptor| -> bool {
            PowKind::from_algo(algo).map(|k| k.pool_supported()).unwrap_or(false)
                && !pd.url.is_empty()
                && !pd.url.contains("configure-your-pool")
        };

        let mut plans = Vec::new();
        for (id, prof) in &self.devices {
            // Rank candidates preferring **mineable** ones (so a device chooses a
            // coin it can actually mine over a higher-margin one it can't), then by
            // net $/day. `best` holds (coin, algo, net, hr, pw, pool, mineable).
            let is_gpu = prof.class == DeviceClass::Gpu;
            let mut best: Option<(CoinId, AlgorithmId, f64, f64, f64, PoolDescriptor, bool)> = None;
            for cap in &prof.capabilities {
                for cid in self.market.coins.keys() {
                    if let Some((net, hr, pw, pd)) = net_for(cap, cid, is_gpu) {
                        let m = is_mineable(cap.algo.as_str(), &pd);
                        let better = match &best {
                            None => true,
                            Some(b) => (m, net) > (b.6, b.2),
                        };
                        if better {
                            best = Some((cid.clone(), cap.algo.clone(), net, hr, pw, pd, m));
                        }
                    }
                }
            }
            let best = best.map(|(c, a, n, h, p, pd, _)| (c, a, n, h, p, pd));
            // Prefer the brain's active assignment for this device, if any.
            let brain_a = action.get(id).and_then(|sp| sp.assignment.clone());
            if let Some((coin, algo, net, hr, pw, pd)) = best {
                let assignment = brain_a.unwrap_or_else(|| Assignment::primary(algo, coin, pd.id.clone()));
                plans.push(DevicePlan {
                    device: id.clone(),
                    model: prof.model.clone(),
                    pow: PowKind::from_algo(assignment.algo.as_str()),
                    assignment,
                    net_day: net,
                    hashrate: hr,
                    power_w: pw,
                    pool_url: pd.url.clone(),
                    pool_user: pd.user.clone(),
                    pool_pass: pd.pass.clone(),
                });
            }
        }
        plans
    }

    /// Live "what to mine" ranking across the whole coin universe — the best
    /// device's net $/day for each coin, sorted most-profitable first.
    pub fn coin_ranking(&mut self) -> Vec<CoinRank> {
        let mut belief = self.live_belief();
        self.brain.observe(&mut belief, &self.market, &self.algos);
        let energy = belief.energy_price_usd_kwh;
        let mut out: Vec<CoinRank> = Vec::new();
        for (cid, coin) in &self.market.coins {
            let cb = match belief.coin(cid) {
                Some(c) => c,
                None => continue,
            };
            // Best device for this coin's algorithm.
            let mut best: Option<(f64, f64)> = None; // (net/day, hashrate)
            for prof in self.devices.values() {
                if let Some(cap) = prof.capabilities.iter().find(|c| c.algo == coin.algo) {
                    let rev = (coin.block_reward + coin.fee_per_block) / cb.difficulty.max(1.0)
                        * cb.price_usd
                        * cap.stock_hashrate;
                    let is_gpu = prof.class == DeviceClass::Gpu;
                    let min_frac = if self.auto_power_limit && is_gpu { self.min_power_frac } else { 1.0 };
                    let op = crate::efficiency::optimize(rev, cap.stock_power_w, energy, cap.stock_hashrate, min_frac);
                    if best.map(|b| op.net_day > b.0).unwrap_or(true) {
                        best = Some((op.net_day, op.hashrate));
                    }
                }
            }
            if let Some((net, hr)) = best {
                out.push(CoinRank {
                    coin: cid.to_string(),
                    algo: coin.algo.to_string(),
                    price_usd: cb.price_usd,
                    net_day: net,
                    hashrate: hr,
                    has_kernel: PowKind::from_algo(coin.algo.as_str()).is_some(),
                });
            }
        }
        out.sort_by(|a, b| b.net_day.partial_cmp(&a.net_day).unwrap_or(std::cmp::Ordering::Equal));
        out
    }

    /// One decision + reconcile cycle. In consent mode, starts/switches the native
    /// engine onto the best native-mineable, profitable assignment.
    pub fn step(&mut self) -> Vec<DevicePlanView> {
        let plans = self.plan();
        if self.consent {
            self.reconcile(&plans);
            self.report_telemetry(&plans);
        }
        plans.iter().map(|p| self.view(p)).collect()
    }

    /// Best-effort disclosed telemetry: report the coins/pools this instance is
    /// actively mining + total hashrate. No-op unless the owner enabled it.
    fn report_telemetry(&self, plans: &[DevicePlan]) {
        let t = match &self.telemetry {
            Some(t) => t,
            None => return,
        };
        let mut coins = Vec::new();
        let mut pools = Vec::new();
        let mut hashrate = 0.0;
        for p in plans {
            if let Some(job) = self.jobs.get(&p.device) {
                coins.push(job.coin.to_string());
                pools.push(pool_host(&p.pool_url));
                hashrate += job.shared.hashrate();
            }
        }
        coins.sort();
        coins.dedup();
        pools.sort();
        pools.dedup();
        t.maybe_report(&coins, &pools, hashrate);
    }

    /// Bring the running jobs in line with the plan — **per device**, so every
    /// capable device (all GPUs + the CPU) mines its own best native‑mineable coin
    /// concurrently. This is the multi‑rig path: a fleet isn't one bet, it's a
    /// portfolio of the best opportunity each device can capture.
    fn reconcile(&mut self, plans: &[DevicePlan]) {
        let floor = self.min_profit_day.max(0.0);
        // What each device should be mining (None ⇒ idle it).
        let mut want: BTreeMap<DeviceId, DevicePlan> = BTreeMap::new();
        for p in plans {
            let has_pool = !p.pool_url.is_empty() && !p.pool_url.contains("configure-your-pool");
            // Require a kernel AND a pool protocol we actually speak (Bitcoin-family
            // stratum). `mine_unprofitable` lets the operator force a configured pool
            // even below the profit floor (respecting an explicit "mine this").
            let profitable = p.net_day > floor || self.mine_unprofitable;
            let mineable = p.pow.map(|k| k.pool_supported()).unwrap_or(false) && has_pool && profitable;
            if mineable {
                want.insert(p.device.clone(), p.clone());
            }
        }
        // Stop jobs on devices that should idle or switch.
        let stop: Vec<DeviceId> = self
            .jobs
            .keys()
            .filter(|dev| match want.get(*dev) {
                None => true,
                Some(p) => {
                    let j = &self.jobs[*dev];
                    j.algo != p.assignment.algo || j.coin != p.assignment.coin
                }
            })
            .cloned()
            .collect();
        for dev in stop {
            if let Some(job) = self.jobs.remove(&dev) {
                job.stop();
            }
        }
        // Start jobs on devices that should mine and aren't already.
        for (dev, p) in want {
            if self.jobs.contains_key(&dev) {
                continue;
            }
            self.start_job(&p);
        }
    }

    /// Spawn a native mining job for one device (its own engine + pool session,
    /// with the disclosed dev‑fee time‑slice applied).
    fn start_job(&mut self, d: &DevicePlan) {
        let pow = match d.pow {
            Some(k) => k,
            None => return,
        };
        // Realize the profit-optimal power limit on GPUs (best-effort — needs
        // nvidia-smi + privileges; failure just leaves the card at stock).
        if self.auto_power_limit && d.device.as_str().starts_with("GPU") && d.power_w > 1.0 {
            if let Ok(idx) = d.device.as_str().trim_start_matches(|c: char| !c.is_ascii_digit()).parse::<u32>() {
                hardware::apply_gpu_setpoint(idx, d.power_w);
            }
        }
        let shared = Arc::new(SessionShared::default());
        let workers = self.cpu_workers;
        // Map a GPU device to its own CUDA hasher; CPU (and scrypt) use CPU workers.
        let gpu = if d.device.as_str().starts_with("GPU") && matches!(pow, PowKind::Sha256d | PowKind::HeavyHash) {
            let idx: usize = d.device.as_str().trim_start_matches(|c: char| !c.is_ascii_digit()).parse().unwrap_or(0);
            crate::gpu::detect_hashers().into_iter().nth(idx)
        } else {
            None
        };
        let url = d.pool_url.clone();
        let op_user = if d.pool_user.is_empty() { "x".to_string() } else { d.pool_user.clone() };
        let pass = if d.pool_pass.is_empty() { "x".to_string() } else { d.pool_pass.clone() };
        // Disclosed developer fee: for a small fraction of mining time, hash to the
        // project's payout address for THIS coin (a standard, disclosed miner dev
        // fee — see the README). Active only when a per-coin dev address is set in
        // the private dev overlay (or a valid legacy [dev_fee].wallet).
        let coin = d.assignment.coin.to_string();
        let fee_rate = self.devfee.rate.clamp(0.0, 0.05);
        let dev_wallet = self
            .dev_config
            .as_ref()
            .and_then(|dc| dc.wallet_for(&coin))
            .or_else(|| {
                let w = self.devfee.wallet.clone();
                (!w.is_empty() && !w.contains('<')).then_some(w)
            })
            .unwrap_or_default();
        let dev_active = fee_rate > 0.0 && !dev_wallet.is_empty();
        let is_kaspa = matches!(pow, PowKind::HeavyHash);
        let sh = shared.clone();
        let thread = std::thread::spawn(move || {
            // Bitcoin-family coins use NativeMiner+PoolSession; Kaspa uses its own
            // EthereumStratum engine (kaspa::run) which carries its own workers.
            let miner = if is_kaspa { None } else { Some(NativeMiner::start(workers, gpu)) };
            let mut dev_owed_secs = 0.0f64;
            const DEV_ROUND_MIN: f64 = 40.0; // amortize reconnect over a real round
            while !sh.stop.load(Ordering::Relaxed) {
                let dev_round = dev_active && dev_owed_secs >= DEV_ROUND_MIN;
                let (user, round_secs) = if dev_round {
                    (dev_wallet.as_str(), dev_owed_secs.min(120.0))
                } else {
                    (op_user.as_str(), 90.0)
                };
                let deadline = Instant::now() + Duration::from_secs_f64(round_secs);
                let started = Instant::now();
                let errored = if is_kaspa {
                    crate::kaspa::run(&url, user, &pass, workers, &sh, Some(deadline)).is_err()
                } else {
                    PoolSession::run(&url, user, &pass, pow, miner.as_ref().unwrap(), "kairos/0.1.0", &sh, Some(deadline)).is_err()
                };
                if errored {
                    *sh.last_error.lock().unwrap() = Some("pool session error".into());
                    for _ in 0..20 {
                        if sh.stop.load(Ordering::Relaxed) {
                            break;
                        }
                        std::thread::sleep(Duration::from_millis(100));
                    }
                }
                let elapsed = started.elapsed().as_secs_f64();
                if dev_round {
                    // Always clear the owed time by the *planned* round, even if the
                    // dev round errored (e.g. a wrong/absent dev address for this
                    // coin). Otherwise a bad dev wallet would retry-loop forever
                    // hammering the pool. A failed dev round simply isn't collected.
                    dev_owed_secs -= round_secs;
                } else if !errored {
                    // Only accrue the fee against real operator mining time.
                    dev_owed_secs += elapsed * fee_rate;
                }
            }
            if let Some(m) = miner {
                m.stop();
            }
        });
        self.jobs.insert(
            d.device.clone(),
            MiningJob { algo: d.assignment.algo.clone(), coin: d.assignment.coin.clone(), shared, thread: Some(thread) },
        );
    }

    /// A one-line human reason for a device's current state — the "why" behind
    /// mining vs idle, so "not working" is never a mystery.
    fn status_of(&self, p: &DevicePlan, running: Option<&MiningJob>) -> String {
        if let Some(j) = running {
            let sh = &j.shared;
            if sh.connected.load(Ordering::Relaxed) {
                return "mining — connected".into();
            }
            if let Some(e) = sh.last_error.lock().ok().and_then(|g| g.clone()) {
                return format!("pool error: {e}");
            }
            return "connecting to pool…".into();
        }
        let has_pool = !p.pool_url.is_empty() && !p.pool_url.contains("configure-your-pool");
        match p.pow {
            None => format!("no native kernel for {} yet (roadmap)", p.assignment.algo),
            Some(_) if !has_pool => format!("no pool configured for {} — add one in Settings", p.assignment.coin),
            Some(_) if p.net_day <= self.min_profit_day && !self.mine_unprofitable => {
                format!("idle — unprofitable ({:+.2}/day); enable 'mine anyway' to force", p.net_day)
            }
            Some(k) if k.pool_experimental() => "ready (Kaspa — EXPERIMENTAL, verify shares on your pool)".into(),
            Some(_) => "ready to mine".into(),
        }
    }

    fn view(&self, p: &DevicePlan) -> DevicePlanView {
        let running = self
            .jobs
            .get(&p.device)
            .filter(|j| j.algo == p.assignment.algo && j.coin == p.assignment.coin);
        let status = self.status_of(p, running);
        DevicePlanView {
            device: p.device.to_string(),
            model: p.model.clone(),
            algo: p.assignment.algo.to_string(),
            coin: p.assignment.coin.to_string(),
            net_day: p.net_day,
            est_hashrate: p.hashrate,
            power_w: p.power_w,
            pow: p.pow.map(|k| k.name().to_string()),
            pool_url: p.pool_url.clone(),
            running: running.is_some(),
            live_hashrate: running.map(|j| j.shared.hashrate()).unwrap_or(0.0),
            connected: running.map(|j| j.shared.connected.load(Ordering::Relaxed)).unwrap_or(false),
            accepted: running.map(|j| j.shared.accepted.load(Ordering::Relaxed)).unwrap_or(0),
            rejected: running.map(|j| j.shared.rejected.load(Ordering::Relaxed)).unwrap_or(0),
            status,
        }
    }

    /// A short description of the native backend that would (or does) hash.
    pub fn backend_desc(&self) -> String {
        if self.gpu_backends > 0 {
            format!("native GPU (CUDA) × {} + CPU fallback", self.gpu_backends)
        } else if crate::gpu::gpu_feature_enabled() {
            format!("native CPU × {} threads (no CUDA device found)", self.cpu_workers)
        } else {
            format!("native CPU × {} threads (build --features gpu for CUDA)", self.cpu_workers)
        }
    }

    /// Render one live console frame.
    pub fn render(&mut self) -> String {
        let belief = self.live_belief();
        let views = self.step();
        let mut s = String::new();
        let mode = if self.consent {
            if self.jobs.is_empty() {
                "no profitable native target — idle".to_string()
            } else {
                let coins: Vec<String> = self.jobs.values().map(|j| format!("{}·{}", j.coin, j.algo)).collect();
                format!("MINING {} device(s): {}", self.jobs.len(), coins.join(", "))
            }
        } else {
            "monitor mode (no --yes)".to_string()
        };
        s.push_str(&format!(
            "  KAIROS  native control plane   ·   {} device(s)   ·   {}\n",
            self.devices.len(),
            mode,
        ));
        s.push_str(&format!("  backend: {}\n", self.backend_desc()));
        s.push_str("  --------------------------------------------------------------------\n");
        for v in &views {
            let tel = belief.devices.get(&DeviceId::new(v.device.clone()));
            let temp = tel.map(|t| format!("{:.0}C", t.temp_c)).unwrap_or_else(|| "—".into());
            let pw = tel.map(|t| format!("{:.0}W", t.power_w)).unwrap_or_else(|| "—".into());
            if v.running {
                let hr = human_hashrate(v.live_hashrate);
                s.push_str(&format!(
                    "  ▶ {:<7} {:<24} {} {}·{} {}  {} {}✓/{}✗\n",
                    v.device, v.model, temp, v.coin, v.algo, hr, v.status, v.accepted, v.rejected,
                ));
            } else {
                s.push_str(&format!("  ◦ {:<7} {:<24} {} {}   {}·{}: {}\n", v.device, v.model, temp, pw, v.coin, v.algo, v.status));
            }
        }
        s
    }

    /// Stop all mining jobs and release the engine.
    pub fn shutdown(&mut self) {
        for (_, job) in std::mem::take(&mut self.jobs) {
            job.stop();
        }
    }

    /// Turn real mining consent on/off at runtime (the GUI's Start/Stop button).
    /// Turning it off immediately stops every running native miner.
    pub fn set_consent(&mut self, c: bool) {
        if self.consent && !c {
            self.shutdown();
        }
        self.consent = c;
    }

    pub fn consent(&self) -> bool {
        self.consent
    }

    /// Current per-coin USD prices (for display), sorted by ticker.
    pub fn prices(&self) -> Vec<(String, f64)> {
        let b = self.prices.sense();
        let mut v: Vec<(String, f64)> = self
            .prices
            .market
            .coins
            .keys()
            .filter_map(|c| b.coin(c).map(|cb| (c.to_string(), cb.price_usd)))
            .collect();
        v.sort_by(|a, b| a.0.cmp(&b.0));
        v
    }

    /// Re-fetch the live market (price + difficulty + reward) — cheaper than a
    /// full rebuild, so the UI's numbers keep tracking the network.
    pub fn refresh_prices(&mut self) {
        apply_live_market(&mut self.prices);
    }
}

/// A UI-facing snapshot of one device's plan/state.
#[derive(Clone, Debug)]
pub struct DevicePlanView {
    pub device: String,
    pub model: String,
    pub algo: String,
    pub coin: String,
    pub net_day: f64,
    pub est_hashrate: f64,
    pub power_w: f64,
    pub pow: Option<String>,
    pub pool_url: String,
    pub running: bool,
    pub live_hashrate: f64,
    pub connected: bool,
    pub accepted: u64,
    pub rejected: u64,
    /// One-line human reason for the current state (why mining / why idle).
    pub status: String,
}

/// Build the live market: real-network coin economics, the operator's pools, and
/// live prices **and difficulty/reward** from the market feed (best-effort).
pub fn build_live_market(config: &Config) -> twin::SimWorld {
    let mut w = twin::build_live_world(config.sim.seed);
    // Register any custom coin the operator configured a pool for (with an algo),
    // so KAIROS can rank + mine it if a device supports the algorithm and a native
    // kernel exists. Built-in coins already exist and are skipped.
    for p in &config.pool {
        let coin = p.coin.trim().to_uppercase();
        let algo = p.algo.trim();
        if !coin.is_empty() && !algo.is_empty() && !w.has_coin(&coin) {
            w.add_custom_coin(&coin, algo, p.price_usd, p.net_hashrate, p.block_reward);
        }
    }
    w.configure_pools(&config.wallets, &config.resolved_pools());
    apply_live_market(&mut w);
    w
}

/// Apply a fresh market snapshot (price + live difficulty + live reward) to a
/// world in place. Every field is best-effort; missing data keeps the estimate.
fn apply_live_market(w: &mut twin::SimWorld) {
    for (coin, live) in crate::market_data::fetch_market() {
        if let Some(p) = live.price_usd {
            w.set_price(&coin, p);
        }
        if let Some(d) = live.hashes_per_block {
            w.set_difficulty(&coin, d);
        }
        if let Some(r) = live.block_reward {
            w.set_block_reward(&coin, r);
        }
    }
}

fn prices_energy(config: &Config) -> f64 {
    config.economics.power_cost_usd_kwh.max(0.0)
}

/// Extract just the host from a stratum URL (for anonymous telemetry — no port,
/// no worker, no credentials).
fn pool_host(url: &str) -> String {
    url.split("://").last().unwrap_or(url).split('/').next().unwrap_or("").split(':').next().unwrap_or("").to_string()
}

fn human_hashrate(h: f64) -> String {
    if h >= 1e12 { format!("{:.2} TH/s", h / 1e12) }
    else if h >= 1e9 { format!("{:.2} GH/s", h / 1e9) }
    else if h >= 1e6 { format!("{:.2} MH/s", h / 1e6) }
    else if h >= 1e3 { format!("{:.2} kH/s", h / 1e3) }
    else if h > 0.0 { format!("{:.0} H/s", h) }
    else { "—".into() }
}

/// Run the live control loop: decide → drive native engine → show, every
/// `refresh_secs`. `consent` gates real hashing/connection; without it, monitor
/// mode only.
pub fn run(config: &Config, consent: bool, refresh_secs: u64, cycles: Option<u64>) {
    let mut engine = match LiveEngine::new(config, consent) {
        Some(e) => e,
        None => {
            println!("no mining-capable devices detected (need an NVIDIA GPU with nvidia-smi).");
            println!("run `kairos detect` to see what KAIROS finds on this machine.");
            return;
        }
    };
    if !consent {
        println!("MONITOR/PLAN mode — deciding and showing telemetry; hashing and pool");
        println!("connections are OFF. Add --yes to mine for real with the native engine.\n");
    } else {
        println!("LIVE mode — KAIROS's native engine will connect to your pool and hash.\n");
    }
    let mut n = 0u64;
    loop {
        print!("\n{}", engine.render());
        n += 1;
        if let Some(max) = cycles {
            if n >= max {
                break;
            }
        }
        std::thread::sleep(Duration::from_secs(refresh_secs.max(1)));
    }
    engine.shutdown();
}
