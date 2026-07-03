//! The control loop — section 27. One tick wires the whole engine together and
//! the shield + degradation fallback wrap every actuation.
//!
//! ```text
//! sense → update belief & forecasts → (brain ? decide : safe_heuristic)
//!       → shield (hard-limit override) → self-heal → actuate
//!       → ledgers (value/regret/credit, dev-fee split) → settle equity
//!       → learn from telemetry → Sim2Real correct
//! ```
//!
//! If the brain is unavailable the loop degrades to the safe heuristic and keeps
//! mining — the hardware never stops earning because an ML component hiccupped.

use crate::alerts::{Alert, Notifier, Severity, TrigMetric, Trigger};
use crate::config::Config;
use crate::degrade::{self, AutonomyTier};
use crate::devfee::DevFee;
use crate::forecast::{ForecastParams, Forecaster};
use crate::heal::SelfHealer;
use crate::intelligence::{Brain, BrainParams, Decision, EfficiencyOracle, Obligations, PowerBudget, RationaleItem};
use crate::ledger::{Ledgers, RegretEntry, ValueKind};
use crate::model::*;
use crate::onboard::{auto_benchmark, BenchmarkReport};
use crate::shield::Shield;
use crate::stats::FleetStats;
use crate::twin::SimWorld;
use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

/// A compact per-tick record for the console, dashboard, and API.
#[derive(Clone, Debug)]
pub struct TickSummary {
    pub t_secs: f64,
    pub user_net_per_s: f64,
    pub baseline_net_per_s: f64,
    pub power_w: f64,
    pub hashrate_hs: f64,
    pub idle_devices: usize,
    pub switches: usize,
    pub autonomy: &'static str,
    pub dr_credit_per_s: f64,
}

pub struct Engine {
    pub brain: Brain,
    pub shield: Shield,
    pub healer: SelfHealer,
    pub ledgers: Ledgers,
    pub devfee: DevFee,
    pub budget: PowerBudget,
    pub obligations: Obligations,
    pub devices: BTreeMap<DeviceId, DeviceProfile>,
    pub bench: BenchmarkReport,
    pub risk_word: RiskWord,

    /// Per-device + per-pool accepted/rejected share accounting.
    pub stats: FleetStats,
    /// Operator alert channels (webhook / Telegram).
    pub notifier: Notifier,
    /// Operator "if-this-then-notify" rules.
    pub triggers: Vec<Trigger>,
    /// Hours-of-day (fleet clock) the schedule pauses mining.
    pub pause_hours: Vec<u32>,
    /// True while a scheduled off-peak pause is in effect.
    pub scheduled_pause: bool,
    /// Vardiff target submitted-shares/sec (drives the share counters).
    target_share_hz: f64,
    /// Devices currently alerted-on, to debounce repeated notifications.
    alerted: BTreeSet<String>,

    /// A clone of the world driven by a competent profit-switching incumbent
    /// (greedy spot coin, stock clocks, default pool, shielded + self-healed).
    /// Run head-to-head on the same exogenous price path for honest, *realized*
    /// proof-of-uplift — not a per-tick counterfactual estimate.
    pub baseline_world: SimWorld,
    baseline_healer: SelfHealer,

    pub brain_available: bool,
    pub autonomy: AutonomyTier,
    pub paused: bool,

    pub rationale: Vec<RationaleItem>,
    pub events: Vec<String>,
    pub history: Vec<TickSummary>,
    pub last: Option<TickSummary>,
    pub last_decision: Option<Decision>,

    uptime_online: u64,
    uptime_total: u64,
}

impl Engine {
    /// Boot from config + a world: detect devices, auto-benchmark, build the
    /// brain, set power budget + obligations. The "from only wallets and a risk
    /// word" path.
    pub fn bootstrap(config: &Config, world: &SimWorld) -> Self {
        // Apply the operator's device-exclusion list and thermal stop temperature.
        let excluded = config.excluded_devices();
        let stop_c = config.thermal.stop_c;
        let mut profiles: BTreeMap<DeviceId, DeviceProfile> = world
            .device_profiles()
            .into_iter()
            .filter(|(id, _)| !excluded.contains(id.as_str()))
            .map(|(id, mut p)| {
                p.limits.max_temp_c = p.limits.max_temp_c.min(stop_c);
                (id, p)
            })
            .collect();
        let _ = &mut profiles;
        let bench = auto_benchmark(&profiles, world, world_ambient(world));

        let utility = config.utility();
        let fparams = ForecastParams::default();
        let forecaster = Forecaster::new(fparams);
        let bparams = BrainParams::default();
        let brain = Brain::new(
            "kairos-core",
            utility,
            forecaster,
            bparams,
            config.operator.equity_usd,
        );

        // Power budget: explicit MW caps, else auto = 1.25× per-site stock power.
        let mut site_cap_w: BTreeMap<SiteId, f64> = BTreeMap::new();
        let auto = bench.per_site_stock_power_w();
        for (site, p) in &auto {
            site_cap_w.insert(site.clone(), p * 1.25);
        }
        for (site, mw) in &config.power.cap_mw {
            site_cap_w.insert(SiteId::new(site), mw * 1_000_000.0);
        }

        Engine {
            brain,
            shield: Shield,
            healer: SelfHealer::new(Default::default()),
            ledgers: Ledgers::default(),
            devfee: config.dev_fee(),
            budget: PowerBudget { site_cap_w },
            obligations: Obligations {
                usd_per_day: config.operator.obligations_usd_per_day,
            },
            devices: profiles,
            bench,
            risk_word: config.risk_word(),
            stats: FleetStats::default(),
            notifier: config.notifier(),
            triggers: config.triggers(),
            pause_hours: config.schedule.pause_hours.clone(),
            scheduled_pause: false,
            target_share_hz: config.stratum.target_share_hz,
            alerted: BTreeSet::new(),
            baseline_world: world.clone(),
            baseline_healer: SelfHealer::new(Default::default()),
            brain_available: true,
            autonomy: AutonomyTier::FullIntelligence,
            paused: false,
            rationale: Vec::new(),
            events: Vec::new(),
            history: Vec::new(),
            last: None,
            last_decision: None,
            uptime_online: 0,
            uptime_total: 0,
        }
    }

    pub fn set_risk(&mut self, word: RiskWord) {
        self.risk_word = word;
        self.brain.utility = crate::utility::OperatorUtility::from_risk(word);
    }

    pub fn set_power_cap(&mut self, site: &str, watts: f64) {
        self.budget.site_cap_w.insert(SiteId::new(site), watts);
    }

    pub fn uptime_frac(&self) -> f64 {
        if self.uptime_total == 0 {
            1.0
        } else {
            self.uptime_online as f64 / self.uptime_total as f64
        }
    }

    fn push_event(&mut self, t: f64, msg: String) {
        self.events.push(format!("{:>8.0}s  {}", t, msg));
        let n = self.events.len();
        if n > 200 {
            self.events.drain(0..n - 200);
        }
    }

    /// Run one control tick against the world.
    pub fn tick(&mut self, world: &mut SimWorld) {
        let dt = world.dt();
        let mut belief = world.sense();
        self.brain.observe(&mut belief, &world.market, &world.algos);

        // Decide (full intelligence) or degrade to the safe heuristic.
        let decision = if self.brain_available && !self.paused {
            let oracle: &dyn EfficiencyOracle = world;
            Some(self.brain.decide(
                &belief,
                &world.market,
                &world.algos,
                &self.devices,
                oracle,
                &self.budget,
                &self.obligations,
            ))
        } else {
            None
        };

        let mut action = if self.paused {
            self.autonomy = AutonomyTier::SafeIdle;
            degrade::safe_idle(&self.devices)
        } else {
            match &decision {
                Some(d) => {
                    self.autonomy = AutonomyTier::FullIntelligence;
                    d.action.clone()
                }
                None => {
                    self.autonomy = AutonomyTier::SafeHeuristic;
                    degrade::safe_heuristic(
                        &belief,
                        &world.market,
                        &world.algos,
                        &self.devices,
                        world,
                    )
                }
            }
        };

        // Scheduler: during a configured off-peak window, pause the fleet to safe
        // idle (time-of-day scheduling, like a tariff window).
        let hour = ((belief.t_secs / 3600.0).floor() as i64).rem_euclid(24) as u32;
        let was_paused = self.scheduled_pause;
        self.scheduled_pause = self.pause_hours.contains(&hour);
        if self.scheduled_pause {
            action = degrade::safe_idle(&self.devices);
            self.autonomy = AutonomyTier::SafeIdle;
            if !was_paused {
                self.push_event(belief.t_secs, format!("schedule :: off-peak pause (hour {:02}:00)", hour));
            }
        } else if was_paused {
            self.push_event(belief.t_secs, "schedule :: off-peak window ended, resuming".into());
        }

        // Self-heal first (restart faulted devices, roll back unstable setpoints,
        // fail over pools), THEN shield — so the shield is the *last* transform
        // before actuation and nothing, not even a recovery action, can reach the
        // hardware past a hard limit. The intelligence proposes, healing recovers,
        // the shield disposes.
        let heal_events = self.healer.heal(&mut action, &belief, &self.devices, &world.market);
        for ev in &heal_events {
            self.push_event(belief.t_secs, format!("heal {} :: {}", ev.device, ev.action));
        }
        let (safe, shield_events) = self.shield.filter(&action, &self.devices, &belief);
        for ev in &shield_events {
            self.push_event(belief.t_secs, format!("shield {} :: {}", ev.device, ev.reason));
        }
        action = safe;

        // Map device → assigned algo (for autotuner feedback after telemetry).
        let assigned: BTreeMap<DeviceId, AlgorithmId> = action
            .setpoints
            .iter()
            .filter_map(|s| s.assignment.as_ref().map(|a| (s.device.clone(), a.algo.clone())))
            .collect();

        // Actuate.
        world.step(&action);

        // ── Ledgers ────────────────────────────────────────────────────────────
        let t = world.time();
        let gross = world.last_gross_per_s() * dt;
        let energy = world.last_energy_per_s() * dt;
        let wear = world.last_wear_per_s() * dt;
        let power_w = world.last_power_w();
        let joules = power_w * dt;
        let realized_net = world.last_realized_net_per_s();

        // Thermodynamic value ledger: revenue (+joules consumed), costs.
        self.ledgers.value.credit(
            t,
            ValueKind::MiningRevenue,
            gross,
            -joules,
            None,
            None,
            "mining revenue",
        );
        // Dev fee: 1% of gross mining revenue, visible + logged. Never custody.
        let _user_gross = self.devfee.realize(&mut self.ledgers.value, t, gross, "mining");
        self.ledgers
            .value
            .credit(t, ValueKind::EnergyCost, -energy, 0.0, None, None, "energy");
        self.ledgers.value.credit(
            t,
            ValueKind::DegradationCost,
            -wear,
            0.0,
            None,
            None,
            "lifespan amortization",
        );

        // Demand-response / grid credit is real income when the engine curtails
        // into a DR window — but the mining twin doesn't pay it, so book it here
        // from the decision (deterministic: credit × curtailed power). Not subject
        // to the mining dev fee.
        let dr_credit_per_s = decision.as_ref().map(|d| d.dr_credit_per_s).unwrap_or(0.0);
        if dr_credit_per_s > 0.0 {
            self.ledgers.value.credit(
                t,
                ValueKind::GridCredit,
                dr_credit_per_s * dt,
                0.0,
                None,
                None,
                "demand-response credit",
            );
        }

        let user_net_per_s =
            realized_net + dr_credit_per_s - world.last_gross_per_s() * self.devfee.rate;
        self.ledgers.value.sample_rate(t, user_net_per_s * 86_400.0);

        // Head-to-head baseline: step the cloned world under a competent
        // profit-switching incumbent and book its *realized* net. Same exogenous
        // price path, shielded + self-healed, so the only difference is the
        // intelligence — the honest proof-of-uplift.
        // Apply the same 1% fee to the baseline so the uplift compares mining
        // *skill* like-for-like, not KAIROS's business model against a fee-free
        // incumbent (the dev fee is disclosed separately on the ledger).
        let baseline_per_s = self.step_baseline();
        self.ledgers
            .value
            .add_baseline(baseline_per_s * dt * (1.0 - self.devfee.rate));

        // Regret: chosen vs the best alternative (baseline) in hindsight.
        self.ledgers.regret.record(RegretEntry {
            t_secs: t,
            chosen_value: realized_net,
            best_value: realized_net.max(baseline_per_s),
            policy: self.brain.name.clone(),
        });

        // Credit: per-lever marginal value.
        if let Some(d) = &decision {
            for (lever, usd_per_s) in &d.credit {
                self.ledgers.credit.assign(lever, usd_per_s * dt);
            }
            for g in &d.gate_trips {
                self.push_event(t, format!("gate :: {}", g));
            }
        }

        // Equity + drawdown.
        self.brain.settle(user_net_per_s * dt);

        // Sim2Real correction.
        if let Some(d) = &decision {
            world.correct(d.fleet_net_per_s, realized_net);
        }

        // Learn from realized telemetry (autotuner edge-seeking + backoff).
        let post = world.sense();
        for (dev_id, algo) in &assigned {
            if let Some(dev) = self.devices.get(dev_id) {
                if let Some(tel) = post.devices.get(dev_id) {
                    let corruption = tel.hw_error_rate > 0.05;
                    self.brain
                        .learn_from_telemetry(dev, &world.algos, algo, tel.hw_error_rate, corruption);
                }
            }
        }

        // Uptime accounting (managed fleet only).
        let online = post
            .devices
            .iter()
            .filter(|(id, t)| self.devices.contains_key(*id) && t.online)
            .count() as u64;
        self.uptime_online += online;
        self.uptime_total += self.devices.len() as u64;

        // Per-device / per-pool share accounting.
        self.stats
            .record_tick(&action, &post, dt, self.target_share_hz);

        // Operator alerts (debounced) for offline rigs, overheats, and recovery,
        // plus the configurable operator rules/triggers.
        self.scan_alerts(&post, t);
        self.scan_triggers(&post, t);

        // Rationale feed — collapse consecutive identical messages so the feed
        // reads as events, not a per-tick repeat.
        if let Some(d) = &decision {
            for r in &d.rationale {
                if self.rationale.last().map(|p| p.message != r.message).unwrap_or(true) {
                    self.rationale.push(r.clone());
                }
            }
            let n = self.rationale.len();
            if n > 50 {
                self.rationale.drain(0..n - 50);
            }
        }

        let total_hashrate: f64 = post.devices.values().map(|d| d.hashrate).sum();
        let summary = TickSummary {
            t_secs: t,
            user_net_per_s,
            baseline_net_per_s: baseline_per_s, // realized incumbent net (head-to-head)
            power_w,
            hashrate_hs: total_hashrate,
            idle_devices: decision.as_ref().map(|d| d.idle_devices).unwrap_or(0),
            switches: decision.as_ref().map(|d| d.switches).unwrap_or(0),
            autonomy: self.autonomy.label(),
            dr_credit_per_s: decision.as_ref().map(|d| d.dr_credit_per_s).unwrap_or(0.0),
        };
        self.last = Some(summary.clone());
        self.last_decision = decision;
        self.history.push(summary);
        let n = self.history.len();
        if n > 5000 {
            self.history.drain(0..n - 5000);
        }
    }

    /// Fire (debounced) operator alerts for offline/faulted rigs and overheats,
    /// and a recovery notice when a device comes back. Always logged to the feed;
    /// delivered to webhook/Telegram only if configured.
    fn scan_alerts(&mut self, belief: &Belief, t: f64) {
        for (id, tel) in &belief.devices {
            let lim = match self.devices.get(id) {
                Some(d) => d.limits,
                None => continue,
            };
            let offline = !tel.online || tel.fault.is_some();
            let overheat = tel.temp_c >= lim.max_temp_c;
            if offline || overheat {
                if self.alerted.insert(id.0.clone()) {
                    let (sev, body) = if offline {
                        (
                            Severity::Critical,
                            format!("offline — {}", tel.fault.clone().unwrap_or_else(|| "no telemetry".into())),
                        )
                    } else {
                        (Severity::Warn, format!("over temperature {:.0}°C", tel.temp_c))
                    };
                    let a = Alert::new(sev, format!("rig {}", id), body);
                    self.push_event(t, format!("ALERT {} :: {}", sev.tag(), a.body));
                    self.notifier.fire(&a);
                }
            } else if self.alerted.remove(&id.0) {
                let a = Alert::new(Severity::Info, format!("rig {}", id), "recovered");
                self.push_event(t, format!("ALERT INFO :: {} recovered", id));
                self.notifier.fire(&a);
            }
        }
    }

    /// Evaluate operator-defined `[[trigger]]` rules each tick and fire debounced
    /// notifications when they match — the Awesome-Miner-style rules engine.
    fn scan_triggers(&mut self, belief: &Belief, t: f64) {
        let triggers = self.triggers.clone();
        for trig in &triggers {
            if trig.per_device() {
                for (id, tel) in &belief.devices {
                    if !self.devices.contains_key(id) {
                        continue;
                    }
                    let (v, hit) = match trig.metric {
                        TrigMetric::Temp => (tel.temp_c, trig.fires(tel.temp_c)),
                        TrigMetric::RejectPct => {
                            let p = tel.reject_rate * 100.0;
                            (p, trig.fires(p))
                        }
                        TrigMetric::HashrateHs => (tel.hashrate, trig.fires(tel.hashrate)),
                        TrigMetric::Offline => (0.0, !tel.online),
                        TrigMetric::EnergyMwh => continue,
                    };
                    let key = format!("trig:{}:{}", trig.name, id);
                    if hit {
                        if self.alerted.insert(key) {
                            let body = trig_body(trig, v, &id.to_string());
                            self.push_event(t, format!("RULE {} :: {} — {}", trig.severity.tag(), trig.name, body));
                            self.notifier.fire(&Alert::new(trig.severity, format!("rule {}", trig.name), body));
                        }
                    } else {
                        self.alerted.remove(&key);
                    }
                }
            } else {
                let v = belief.energy_price_usd_kwh * 1000.0; // $/MWh
                let key = format!("trig:{}", trig.name);
                if trig.fires(v) {
                    if self.alerted.insert(key) {
                        let body = format!("energy ${:.0}/MWh", v);
                        self.push_event(t, format!("RULE {} :: {} — {}", trig.severity.tag(), trig.name, body));
                        self.notifier.fire(&Alert::new(trig.severity, format!("rule {}", trig.name), body));
                    }
                } else {
                    self.alerted.remove(&key);
                }
            }
        }
    }

    /// Advance the baseline (incumbent) world one tick and return its realized
    /// net per second. The incumbent: greedy spot-coin profit switcher, stock
    /// clocks, default pool — shielded and self-healed for a fair fight.
    fn step_baseline(&mut self) -> f64 {
        let belief = self.baseline_world.sense();
        let action = degrade::safe_heuristic(
            &belief,
            &self.baseline_world.market,
            &self.baseline_world.algos,
            &self.devices,
            &self.baseline_world,
        );
        let mut healed = action;
        let _ = self
            .baseline_healer
            .heal(&mut healed, &belief, &self.devices, &self.baseline_world.market);
        let (safe, _ev) = self.shield.filter(&healed, &self.devices, &belief);
        self.baseline_world.step(&safe);
        self.baseline_world.last_realized_net_per_s()
    }

    /// Run `ticks` control ticks.
    pub fn run(&mut self, world: &mut SimWorld, ticks: u64) {
        for _ in 0..ticks {
            self.tick(world);
        }
    }

    /// Net USD/day to the user (after dev fee), latest sample.
    pub fn net_per_day(&self) -> f64 {
        self.ledgers.value.latest_rate_per_day()
    }

    /// Mining-skill uplift vs the incumbent (grid income excluded) — the headline.
    pub fn mining_uplift_frac(&self) -> Option<f64> {
        self.ledgers.value.mining_uplift_frac()
    }

    /// Total economic uplift incl. grid income the incumbent ignores.
    pub fn uplift_frac(&self) -> Option<f64> {
        self.ledgers.value.uplift_frac()
    }

    /// Energy-market (demand-response) income, shown separately from mining.
    pub fn grid_income(&self) -> f64 {
        self.ledgers.value.grid_income_usd()
    }

    /// "What to mine" — per-coin best-device net profit/day at current prices and
    /// difficulty, ranked. This is the profitability comparison miner software
    /// shows, computed from the engine's own economics.
    pub fn profitability(&self, world: &SimWorld) -> Vec<CoinProfit> {
        use crate::cal::RewardGeometry;
        use crate::intelligence::{pool, EfficiencyOracle};
        let belief = world.sense();
        let oracle: &dyn EfficiencyOracle = world;

        let mut active: BTreeSet<String> = BTreeSet::new();
        if let Some(d) = &self.last_decision {
            for sp in &d.action.setpoints {
                if let Some(a) = &sp.assignment {
                    active.insert(a.coin.to_string());
                }
            }
        }

        let mut out = Vec::new();
        for (coin_id, coin) in &world.market.coins {
            let cb = match belief.coin(coin_id) {
                Some(c) => c,
                None => continue,
            };
            let (pool_id, _) = match pool::best_pool(
                &world.market, &belief, coin, &self.brain.stratum, &self.brain.utility,
            ) {
                Some(x) => x,
                None => continue,
            };
            let geo = match world.market.build_geometry(coin_id, &pool_id) {
                Some(g) => g,
                None => continue,
            };
            let latency = belief.pools.get(&pool_id).map(|p| p.latency_ms).unwrap_or(60.0);
            let stale = self.brain.stratum.expected_stale(latency, coin.block_time_s);

            let mut best_net = f64::NEG_INFINITY;
            let mut best_hr = 0.0;
            let mut best_cls = "—";
            for (dev_id, prof) in &self.devices {
                let cap = match prof.capability(&coin.algo) {
                    Some(c) => c,
                    None => continue,
                };
                let temp = belief.devices.get(dev_id).map(|t| t.temp_c).unwrap_or(belief.ambient_c + 25.0);
                let knobs = Knobs::stock(cap.stock_power_w);
                let eff = oracle.efficiency(dev_id, &coin.algo, &knobs, temp);
                if eff.hashrate <= 0.0 {
                    continue;
                }
                let rev = geo.net_of_pool_usd_per_s(eff.hashrate, cb.price_usd, cb.difficulty) * (1.0 - stale);
                let energy = eff.power_w * belief.energy_price_usd_kwh / 3_600_000.0;
                let net_day = (rev - energy) * 86_400.0;
                if net_day > best_net {
                    best_net = net_day;
                    best_hr = eff.hashrate;
                    best_cls = match prof.class {
                        DeviceClass::Gpu => "GPU",
                        DeviceClass::Asic => "ASIC",
                        DeviceClass::Fpga => "FPGA",
                    };
                }
            }
            if best_net.is_finite() {
                out.push(CoinProfit {
                    coin: coin_id.to_string(),
                    algo: coin.algo.to_string(),
                    class: best_cls.to_string(),
                    profit_per_day: best_net,
                    hashrate_hs: best_hr,
                    active: active.contains(&coin_id.to_string()),
                });
            }
        }
        out.sort_by(|a, b| b.profit_per_day.partial_cmp(&a.profit_per_day).unwrap_or(std::cmp::Ordering::Equal));
        out
    }
}

/// One coin's profitability for the "what to mine" comparison.
#[derive(Clone, Debug)]
pub struct CoinProfit {
    pub coin: String,
    pub algo: String,
    pub class: String,
    pub profit_per_day: f64,
    pub hashrate_hs: f64,
    pub active: bool,
}

fn world_ambient(world: &SimWorld) -> f64 {
    world.sense().ambient_c
}

fn trig_body(t: &Trigger, v: f64, dev: &str) -> String {
    match t.metric {
        TrigMetric::Temp => format!("{} temperature {:.0}°C", dev, v),
        TrigMetric::RejectPct => format!("{} reject rate {:.2}%", dev, v),
        TrigMetric::HashrateHs => format!("{} hashrate {:.2e} H/s", dev, v),
        TrigMetric::Offline => format!("{} offline", dev),
        TrigMetric::EnergyMwh => format!("energy ${:.0}/MWh", v),
    }
}

/// The engine paired with its world, shared between the ticker thread and the
/// HTTP server in live mode.
pub struct EngineWorld {
    pub engine: Engine,
    pub world: SimWorld,
}

impl EngineWorld {
    pub fn tick(&mut self) {
        let ew = self;
        ew.engine.tick(&mut ew.world);
    }
}

/// Run the app live: warm up, then keep ticking the engine in a background thread
/// while serving a live, auto-refreshing dashboard. Blocks until Ctrl-C.
pub fn serve_app(
    config: &Config,
    world: SimWorld,
    warmup_ticks: u64,
    tick_ms: u64,
    bind: &str,
) -> anyhow::Result<()> {
    let mut engine = Engine::bootstrap(config, &world);
    let mut world = world;
    println!(
        "kairos: warming up the control plane ({} ticks)…",
        warmup_ticks
    );
    for _ in 0..warmup_ticks {
        engine.tick(&mut world);
    }
    let shared = Arc::new(Mutex::new(EngineWorld { engine, world }));

    // Background ticker — keeps the fleet running in real time.
    let ticker = shared.clone();
    thread::spawn(move || loop {
        if let Ok(mut g) = ticker.lock() {
            g.tick();
        }
        thread::sleep(Duration::from_millis(tick_ms.max(20)));
    });

    crate::api::serve_live(shared, bind)
}
