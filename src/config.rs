//! TOML configuration — section 23.
//!
//! Zero-configuration is the design goal: the operator supplies payout wallets
//! and one risk word, and everything below is auto-tuned with optional manual
//! overrides. The dev fee is always visible and fixed at 1%.

use crate::devfee::{DevFee, DevFeeMode};
use crate::alerts::{Notifier, Severity, TrigMetric, TrigOp, Trigger};
use crate::model::{CoinId, PoolDescriptor, PoolId, RewardScheme, RiskWord};
use crate::utility::OperatorUtility;
use anyhow::{anyhow, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Config {
    pub operator: OperatorSection,
    #[serde(default)]
    pub wallets: BTreeMap<String, String>,
    #[serde(default)]
    pub power: PowerSection,
    #[serde(default)]
    pub economics: EconomicsSection,
    #[serde(default)]
    pub energy: EnergySection,
    #[serde(default)]
    pub thermal: ThermalSection,
    #[serde(default)]
    pub watchdog: WatchdogSection,
    #[serde(default)]
    pub alerts: AlertsSection,
    #[serde(default)]
    pub devices: DevicesSection,
    #[serde(default)]
    pub stratum: StratumSection,
    #[serde(default)]
    pub logging: LoggingSection,
    #[serde(default)]
    pub api: ApiSection,
    #[serde(default)]
    pub dev_fee: DevFeeSection,
    #[serde(default)]
    pub sim: SimSection,
    /// Optional explicit pool connections — the familiar miner shape: pool URL,
    /// wallet/worker username, password, scheme, and failover priority. Entered
    /// as `[[pool]]` tables. When present they replace the auto pools for their
    /// coin; coins without an entry keep an auto pool built from `[wallets]`.
    #[serde(default)]
    pub pool: Vec<PoolEntry>,
    /// Operator rules — "if this then notify" triggers (`[[trigger]]`).
    #[serde(default)]
    pub trigger: Vec<TriggerEntry>,
    /// Time-of-day scheduling (off-peak pause).
    #[serde(default)]
    pub schedule: ScheduleSection,
}

/// One operator trigger rule, e.g. `metric="temp" op="gt" value=75`.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct TriggerEntry {
    #[serde(default)]
    pub name: String,
    /// temp | reject | hashrate | offline | energy
    pub metric: String,
    /// gt | lt
    #[serde(default)]
    pub op: String,
    #[serde(default)]
    pub value: f64,
    /// info | warn | critical
    #[serde(default = "default_min_sev")]
    pub severity: String,
}

/// Time-of-day scheduling: hours (0–23, by the fleet clock) to pause mining,
/// e.g. peak-tariff windows.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct ScheduleSection {
    #[serde(default)]
    pub pause_hours: Vec<u32>,
}

/// One configured pool connection — modelled on T-Rex / lolMiner / HiveOS:
/// `url`, `user` (`wallet.worker`), `pass`, `scheme`, `priority`.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct PoolEntry {
    /// Coin ticker this pool mines, e.g. "BTC".
    pub coin: String,
    /// Stratum URL, e.g. "stratum+tcp://host:port" (ssl variant supported).
    pub url: String,
    /// Login username — usually the payout wallet (defaults to `[wallets].<coin>`).
    #[serde(default)]
    pub user: String,
    /// Optional worker name appended as `user.worker`.
    #[serde(default)]
    pub worker: String,
    /// Stratum password — defaults to "x".
    #[serde(default)]
    pub pass: String,
    /// fpps | pplns | pps | prop | solo (defaults to fpps).
    #[serde(default)]
    pub scheme: String,
    /// Failover order; lower is primary.
    #[serde(default)]
    pub priority: u8,
    /// Pool fee percent (defaults to a sensible value for the scheme).
    #[serde(default)]
    pub fee_pct: f64,
    /// Optional measured/assumed latency (ms) for the twin and the router.
    #[serde(default)]
    pub latency_ms: f64,
    /// Optional explicit pool id (defaults to a slug of the URL host).
    #[serde(default)]
    pub name: String,
    /// Algorithm for this coin (e.g. "Scrypt", "kHeavyHash"). Optional for the
    /// built-in coins (inferred); REQUIRED for a custom coin so KAIROS knows how to
    /// mine it. If it names a native kernel, the coin becomes mineable.
    #[serde(default)]
    pub algo: String,
    /// Optional USD price for a *custom* coin (built-in coins use the live feed).
    /// Providing this lets KAIROS put the custom coin in the profit ranking.
    #[serde(default)]
    pub price_usd: f64,
    /// Optional network hashrate (H/s) for a custom coin, used to derive its
    /// difficulty for the profit calc. With `block_reward`, profit is computable.
    #[serde(default)]
    pub net_hashrate: f64,
    /// Optional block reward (coins/block) for a custom coin's profit calc.
    #[serde(default)]
    pub block_reward: f64,
}

/// A pool entry resolved into a descriptor plus its simulated latency.
#[derive(Clone, Debug)]
pub struct ResolvedPool {
    pub desc: PoolDescriptor,
    pub latency_ms: f64,
}

fn parse_scheme(s: &str) -> RewardScheme {
    match s.trim().to_lowercase().as_str() {
        "pplns" => RewardScheme::Pplns,
        "pps" => RewardScheme::Pps,
        "prop" => RewardScheme::Prop,
        "solo" => RewardScheme::Solo,
        _ => RewardScheme::Fpps,
    }
}

fn default_fee(scheme: RewardScheme) -> f64 {
    match scheme {
        RewardScheme::Fpps => 0.01,
        RewardScheme::Pps => 0.015,
        RewardScheme::Pplns => 0.007,
        RewardScheme::Prop => 0.01,
        RewardScheme::Solo => 0.005,
    }
}

/// Slug a stratum URL host into a pool id, e.g.
/// "stratum+tcp://btc.pool.com:3333" → "btc-pool-com".
fn url_slug(url: &str) -> String {
    let s = url
        .split("://")
        .nth(1)
        .unwrap_or(url)
        .split(['/', ':'])
        .next()
        .unwrap_or(url);
    let slug: String = s
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c.to_ascii_lowercase() } else { '-' })
        .collect();
    if slug.is_empty() { "pool".into() } else { slug }
}

/// Temperature protection — the fan/stop policy real miners expose.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ThermalSection {
    /// Force-idle a device at/above this temperature (°C).
    #[serde(default = "default_stop_c")]
    pub stop_c: f64,
    /// Resume a stopped device once it cools below this (°C).
    #[serde(default = "default_start_c")]
    pub start_c: f64,
    /// "auto" | a fixed fan percentage as a string.
    #[serde(default = "default_fan")]
    pub fan: String,
}
impl Default for ThermalSection {
    fn default() -> Self {
        ThermalSection { stop_c: default_stop_c(), start_c: default_start_c(), fan: default_fan() }
    }
}
fn default_stop_c() -> f64 { 83.0 }
fn default_start_c() -> f64 { 65.0 }
fn default_fan() -> String { "auto".into() }

/// Watchdog / self-healing behaviour.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct WatchdogSection {
    #[serde(default = "default_true")]
    pub restart_on_hang: bool,
    /// Roll back + fail over a device above this reject rate (%).
    #[serde(default = "default_max_rejects")]
    pub max_rejects_pct: f64,
}
impl Default for WatchdogSection {
    fn default() -> Self {
        WatchdogSection { restart_on_hang: true, max_rejects_pct: default_max_rejects() }
    }
}
fn default_max_rejects() -> f64 { 3.0 }

/// Alert channels (generic webhook + Telegram). Delivered via system `curl`.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct AlertsSection {
    #[serde(default)]
    pub webhook: String,
    #[serde(default)]
    pub telegram_token: String,
    #[serde(default)]
    pub telegram_chat: String,
    /// info | warn | critical — minimum severity to send.
    #[serde(default = "default_min_sev")]
    pub min_severity: String,
}
fn default_min_sev() -> String { "warn".into() }

/// Device selection.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct DevicesSection {
    /// Device ids to exclude from the fleet entirely.
    #[serde(default)]
    pub exclude: Vec<String>,
}

/// Stratum options.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct StratumSection {
    #[serde(default)]
    pub nicehash_mode: bool,
    #[serde(default)]
    pub proxy: String,
    /// Target submitted shares per second (vardiff band centre).
    #[serde(default = "default_share_hz")]
    pub target_share_hz: f64,
}
impl Default for StratumSection {
    fn default() -> Self {
        StratumSection { nicehash_mode: false, proxy: String::new(), target_share_hz: default_share_hz() }
    }
}
fn default_share_hz() -> f64 { 0.2 }

/// Logging.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct LoggingSection {
    /// error | warn | info | debug
    #[serde(default = "default_log_level")]
    pub level: String,
    #[serde(default)]
    pub file: String,
}
impl Default for LoggingSection {
    fn default() -> Self {
        LoggingSection { level: default_log_level(), file: String::new() }
    }
}
fn default_log_level() -> String { "info".into() }

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct OperatorSection {
    /// conservative | balanced | aggressive
    pub risk: String,
    /// Starting equity for the drawdown-aware utility + ruin gate (USD).
    #[serde(default = "default_equity")]
    pub equity_usd: f64,
    /// Daily fixed obligations (power contracts + rent) the ruin floor protects.
    #[serde(default = "default_obligations")]
    pub obligations_usd_per_day: f64,
}

impl Default for OperatorSection {
    fn default() -> Self {
        OperatorSection {
            risk: "balanced".into(),
            equity_usd: default_equity(),
            obligations_usd_per_day: default_obligations(),
        }
    }
}

fn default_equity() -> f64 {
    250_000.0
}
fn default_obligations() -> f64 {
    400.0
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct PowerSection {
    /// Per-site power cap in MW. Empty ⇒ auto (generous).
    #[serde(default)]
    pub cap_mw: BTreeMap<String, f64>,
}

/// Operator economics — the two knobs that most directly shape profit decisions.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct EconomicsSection {
    /// Your electricity price in USD per kWh (drives the power-cost term).
    #[serde(default = "default_power_cost")]
    pub power_cost_usd_kwh: f64,
    /// Only mine when the best option clears at least this much net USD/day per
    /// device (a profit floor; 0 = mine anything with positive margin).
    #[serde(default)]
    pub min_profit_usd_day: f64,
    /// Mine the best configured pool even when it's below the profit floor (an
    /// explicit "mine my pool regardless" override). Default off — KAIROS idles
    /// money-losers unless you opt in.
    #[serde(default)]
    pub mine_unprofitable: bool,
    /// Optimize each GPU's power limit for maximum profit-after-electricity rather
    /// than max hashrate (undervolt to the profit sweet spot). Default on.
    #[serde(default = "default_true")]
    pub auto_power_limit: bool,
    /// The lowest power fraction the optimizer may pick (guards against unstable
    /// deep undervolts). Default 0.5 (=50% of stock power).
    #[serde(default = "default_min_power_frac")]
    pub min_power_frac: f64,
}

impl Default for EconomicsSection {
    fn default() -> Self {
        EconomicsSection {
            power_cost_usd_kwh: default_power_cost(),
            min_profit_usd_day: 0.0,
            mine_unprofitable: false,
            auto_power_limit: true,
            min_power_frac: default_min_power_frac(),
        }
    }
}

fn default_min_power_frac() -> f64 {
    0.5
}

fn default_power_cost() -> f64 {
    0.10
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct EnergySection {
    #[serde(default)]
    pub demand_response: bool,
    #[serde(default = "default_pause_above")]
    pub pause_above_usd_mwh: f64,
}

impl Default for EnergySection {
    fn default() -> Self {
        EnergySection {
            demand_response: true,
            pause_above_usd_mwh: default_pause_above(),
        }
    }
}

fn default_pause_above() -> f64 {
    120.0
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ApiSection {
    #[serde(default = "default_bind")]
    pub bind: String,
}

impl Default for ApiSection {
    fn default() -> Self {
        ApiSection {
            bind: default_bind(),
        }
    }
}

fn default_bind() -> String {
    "127.0.0.1:4280".into()
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DevFeeSection {
    #[serde(default = "default_rate")]
    pub rate: f64,
    #[serde(default = "default_wallet")]
    pub wallet: String,
}

impl Default for DevFeeSection {
    fn default() -> Self {
        DevFeeSection {
            rate: default_rate(),
            wallet: default_wallet(),
        }
    }
}

fn default_rate() -> f64 {
    0.01
}
fn default_wallet() -> String {
    "kairos:dev-fund-disclosed-address".into()
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SimSection {
    #[serde(default = "default_seed")]
    pub seed: u64,
    /// How many ticks the `start`/twin run advances by default.
    #[serde(default = "default_ticks")]
    pub ticks: u64,
    /// Run the generated adversarial scenario schedule.
    #[serde(default = "default_true")]
    pub scenarios: bool,
}

impl Default for SimSection {
    fn default() -> Self {
        SimSection {
            seed: default_seed(),
            ticks: default_ticks(),
            scenarios: default_true(),
        }
    }
}

fn default_seed() -> u64 {
    0x5EED_1234_ABCD_EF01
}
fn default_ticks() -> u64 {
    1320 // 22 hours at 60s ticks — covers the scenario schedule
}
fn default_true() -> bool {
    true
}

impl Config {
    pub fn risk_word(&self) -> RiskWord {
        RiskWord::parse(&self.operator.risk).unwrap_or(RiskWord::Balanced)
    }

    pub fn utility(&self) -> OperatorUtility {
        OperatorUtility::from_risk(self.risk_word())
    }

    pub fn dev_fee(&self) -> DevFee {
        DevFee {
            rate: self.dev_fee.rate,
            wallet: self.dev_fee.wallet.clone(),
            mode: DevFeeMode::ShareRoute,
            visible: true,
        }
    }

    /// Build the notifier from the `[alerts]` section.
    pub fn notifier(&self) -> Notifier {
        let webhook = if self.alerts.webhook.is_empty() {
            None
        } else {
            Some(self.alerts.webhook.clone())
        };
        let telegram = if self.alerts.telegram_token.is_empty() || self.alerts.telegram_chat.is_empty() {
            None
        } else {
            Some((self.alerts.telegram_token.clone(), self.alerts.telegram_chat.clone()))
        };
        let min_severity = match self.alerts.min_severity.to_lowercase().as_str() {
            "info" => Severity::Info,
            "critical" | "crit" => Severity::Critical,
            _ => Severity::Warn,
        };
        Notifier { webhook, telegram, min_severity }
    }

    /// Device ids excluded from the fleet.
    pub fn excluded_devices(&self) -> std::collections::BTreeSet<String> {
        self.devices.exclude.iter().cloned().collect()
    }

    /// Resolve `[[trigger]]` entries into evaluable rules.
    pub fn triggers(&self) -> Vec<Trigger> {
        let mut out = Vec::new();
        for (i, t) in self.trigger.iter().enumerate() {
            let metric = match t.metric.to_lowercase().as_str() {
                "temp" | "temperature" => TrigMetric::Temp,
                "reject" | "rejects" | "reject_pct" => TrigMetric::RejectPct,
                "hashrate" | "hash" => TrigMetric::HashrateHs,
                "offline" | "down" => TrigMetric::Offline,
                "energy" | "energy_mwh" | "power_price" => TrigMetric::EnergyMwh,
                _ => continue,
            };
            let op = if t.op.eq_ignore_ascii_case("lt") || t.op.eq_ignore_ascii_case("below") {
                TrigOp::Lt
            } else {
                TrigOp::Gt
            };
            let severity = match t.severity.to_lowercase().as_str() {
                "info" => Severity::Info,
                "critical" | "crit" => Severity::Critical,
                _ => Severity::Warn,
            };
            let name = if t.name.is_empty() { format!("trigger-{}", i + 1) } else { t.name.clone() };
            out.push(Trigger { name, metric, op, value: t.value, severity });
        }
        out
    }

    /// True if the fleet should be scheduled-paused at the given hour-of-day.
    pub fn scheduled_pause(&self, hour: u32) -> bool {
        self.schedule.pause_hours.contains(&hour)
    }

    /// Resolve the operator's `[[pool]]` entries into pool descriptors. The
    /// username defaults to the coin's wallet; the worker is appended; the fee
    /// and latency default by scheme. Unique ids are derived from the URL host.
    pub fn resolved_pools(&self) -> Vec<ResolvedPool> {
        let mut seen: BTreeMap<String, u32> = BTreeMap::new();
        let mut out = Vec::new();
        for e in &self.pool {
            if e.coin.is_empty() || e.url.is_empty() {
                continue;
            }
            let scheme = parse_scheme(&e.scheme);
            let fee = if e.fee_pct > 0.0 { e.fee_pct / 100.0 } else { default_fee(scheme) };
            let base = if e.name.is_empty() { url_slug(&e.url) } else { e.name.clone() };
            let n = seen.entry(base.clone()).or_insert(0);
            let id = if *n == 0 { base.clone() } else { format!("{base}-{}", *n) };
            *n += 1;
            let mut user = if e.user.is_empty() {
                self.wallets.get(&e.coin).cloned().unwrap_or_default()
            } else {
                e.user.clone()
            };
            if !e.worker.is_empty() {
                user = format!("{user}.{}", e.worker);
            }
            let pass = if e.pass.is_empty() { "x".to_string() } else { e.pass.clone() };
            let latency = if e.latency_ms > 0.0 { e.latency_ms } else { 50.0 };
            out.push(ResolvedPool {
                desc: PoolDescriptor {
                    id: PoolId::new(id),
                    coin: CoinId::new(&e.coin),
                    scheme,
                    fee_frac: fee,
                    solvency_risk: 0.002,
                    url: e.url.clone(),
                    user,
                    pass,
                    priority: e.priority,
                },
                latency_ms: latency,
            });
        }
        out
    }

    /// Serialize the config to TOML — the round trip of [`from_toml`]. Used by
    /// the dashboard's Settings page to write `kairos.toml` after edits.
    pub fn to_toml(&self) -> Result<String> {
        toml::to_string_pretty(self).map_err(|e| anyhow!("config serialize error: {e}"))
    }

    /// Apply only the operator-editable subset (wallets + pools) from another
    /// config, leaving everything else (risk, thermal, alerts, …) untouched. This
    /// is what the dashboard's Settings form saves.
    pub fn apply_operator_edits(&mut self, wallets: BTreeMap<String, String>, pools: Vec<PoolEntry>) {
        self.wallets = wallets;
        self.pool = pools;
    }

    pub fn from_toml(s: &str) -> Result<Self> {
        let cfg: Config = toml::from_str(s).map_err(|e| anyhow!("config parse error: {e}"))?;
        if RiskWord::parse(&cfg.operator.risk).is_none() {
            return Err(anyhow!(
                "operator.risk must be conservative|balanced|aggressive, got '{}'",
                cfg.operator.risk
            ));
        }
        Ok(cfg)
    }

    /// The zero-config default: balanced risk, demo wallets, auto everything.
    pub fn demo() -> Self {
        let mut wallets = BTreeMap::new();
        wallets.insert("BTC".into(), "bc1qexampledemowallet".into());
        wallets.insert("KAS".into(), "kaspa:qrdemo".into());
        wallets.insert("ERG".into(), "9fRusdemo".into());
        wallets.insert("ETC".into(), "0xetcdemo".into());
        Config {
            operator: OperatorSection::default(),
            wallets,
            ..Default::default()
        }
    }
}
