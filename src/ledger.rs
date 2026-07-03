//! The three ledgers — section 30 of the spec.
//!
//!  * [`ValueLedger`] — append-only, prices every action in **joules and USD**
//!    in one unit (the thermodynamic value ledger). Says how much net value was
//!    produced, and carries the dev-fee and baseline entries for proof-of-uplift.
//!  * [`RegretLedger`] — counterfactual off-policy scores. Says how *good* the
//!    decisions were against the best alternative at decision time.
//!  * [`CreditLedger`] — attribution of realized value across levers (a Shapley-
//!    lite marginal contribution). Says *which behaviors* produced the value, and
//!    is what the enterprise uplift-share billing is computed from.

use crate::model::*;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub enum ValueKind {
    MiningRevenue,
    EnergyCost,
    DegradationCost,
    StaleLoss,
    SwitchCost,
    GridCredit,
    DevFee,
}

/// One priced event. `usd` is signed (revenue/credit positive, cost negative).
/// `joules` is signed energy flow (consumed negative, harvested/sold positive).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ValueEntry {
    pub t_secs: f64,
    pub kind: ValueKind,
    pub usd: f64,
    pub joules: f64,
    pub device: Option<DeviceId>,
    pub coin: Option<CoinId>,
    pub note: String,
}

/// Append-only thermodynamic value ledger.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct ValueLedger {
    pub entries: Vec<ValueEntry>,
    pub cum_usd: f64,
    pub cum_joules: f64,
    /// What the live baseline (the operator's existing stack) earned over the
    /// same period — the denominator of proven uplift.
    pub baseline_cum_usd: f64,
    /// Dev-fee USD routed (visible, logged).
    pub dev_fee_cum_usd: f64,
    /// Grid/demand-response income (energy-market, not mining). Tracked apart so
    /// the mining-skill uplift headline is not flattered by a DR windfall.
    pub grid_cum_usd: f64,
    /// Rolling per-window samples of net USD/day for the dashboard.
    window: Vec<(f64, f64)>, // (t_secs, net_usd_per_day)
}

impl ValueLedger {
    pub fn record(&mut self, e: ValueEntry) {
        self.cum_usd += e.usd;
        self.cum_joules += e.joules;
        if e.kind == ValueKind::DevFee {
            self.dev_fee_cum_usd += -e.usd; // dev fee recorded as a negative-to-user entry
        }
        if e.kind == ValueKind::GridCredit {
            self.grid_cum_usd += e.usd;
        }
        // Cap retained detail entries to keep memory bounded on long runs.
        self.entries.push(e);
        if self.entries.len() > 50_000 {
            self.entries.drain(0..10_000);
        }
    }

    pub fn credit(
        &mut self,
        t: f64,
        kind: ValueKind,
        usd: f64,
        joules: f64,
        device: Option<DeviceId>,
        coin: Option<CoinId>,
        note: impl Into<String>,
    ) {
        self.record(ValueEntry {
            t_secs: t,
            kind,
            usd,
            joules,
            device,
            coin,
            note: note.into(),
        });
    }

    pub fn add_baseline(&mut self, usd: f64) {
        self.baseline_cum_usd += usd;
    }

    pub fn sample_rate(&mut self, t: f64, net_usd_per_day: f64) {
        self.window.push((t, net_usd_per_day));
        if self.window.len() > 4096 {
            self.window.drain(0..1024);
        }
    }

    pub fn net_usd(&self) -> f64 {
        self.cum_usd
    }

    /// Cumulative *mining* net to the user (excludes energy-market grid income),
    /// the like-for-like quantity compared against the incumbent baseline.
    pub fn mining_net_usd(&self) -> f64 {
        self.cum_usd - self.grid_cum_usd
    }

    /// Energy-market (demand-response / grid) income, reported separately.
    pub fn grid_income_usd(&self) -> f64 {
        self.grid_cum_usd
    }

    /// Proven mining-skill uplift vs the incumbent baseline (mining-vs-mining,
    /// grid income excluded) — the like-for-like measure of the engine's mining
    /// intelligence. Returns None until the baseline is meaningful.
    pub fn mining_uplift_frac(&self) -> Option<f64> {
        if self.baseline_cum_usd.abs() < 1e-6 {
            return None;
        }
        Some((self.mining_net_usd() - self.baseline_cum_usd) / self.baseline_cum_usd.abs())
    }

    /// Total economic uplift vs the incumbent — mining skill *plus* the grid
    /// income the incumbent leaves on the table. The headline number.
    pub fn uplift_frac(&self) -> Option<f64> {
        if self.baseline_cum_usd.abs() < 1e-6 {
            return None;
        }
        Some((self.cum_usd - self.baseline_cum_usd) / self.baseline_cum_usd.abs())
    }

    /// Latest sampled net USD/day.
    pub fn latest_rate_per_day(&self) -> f64 {
        self.window.last().map(|x| x.1).unwrap_or(0.0)
    }
}

/// Counterfactual off-policy regret. For each scored decision we keep the value
/// the chosen action realized and the value the best feasible alternative would
/// have realized in the same belief.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RegretEntry {
    pub t_secs: f64,
    pub chosen_value: f64,
    pub best_value: f64,
    pub policy: String,
}

impl RegretEntry {
    pub fn regret(&self) -> f64 {
        (self.best_value - self.chosen_value).max(0.0)
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct RegretLedger {
    pub entries: Vec<RegretEntry>,
    pub cum_regret: f64,
}

impl RegretLedger {
    pub fn record(&mut self, e: RegretEntry) {
        self.cum_regret += e.regret();
        self.entries.push(e);
        if self.entries.len() > 50_000 {
            self.entries.drain(0..10_000);
        }
    }

    pub fn cumulative_regret(&self) -> f64 {
        self.cum_regret
    }

    /// Average regret per decision — a competence signal for the dashboard.
    pub fn mean_regret(&self) -> f64 {
        if self.entries.is_empty() {
            0.0
        } else {
            self.cum_regret / self.entries.len() as f64
        }
    }
}

/// Shapley-lite credit assignment across levers. Each lever (switching, tuning,
/// energy, stratum, pool …) is credited the marginal value it added this tick.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct CreditLedger {
    pub per_lever_usd: BTreeMap<String, f64>,
    pub total_usd: f64,
}

impl CreditLedger {
    pub fn assign(&mut self, lever: &str, usd: f64) {
        *self.per_lever_usd.entry(lever.to_string()).or_insert(0.0) += usd;
        self.total_usd += usd;
    }

    /// Levers sorted by contribution, descending.
    pub fn ranked(&self) -> Vec<(String, f64)> {
        let mut v: Vec<_> = self
            .per_lever_usd
            .iter()
            .map(|(k, v)| (k.clone(), *v))
            .collect();
        v.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        v
    }
}

/// The three ledgers travel together.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Ledgers {
    pub value: ValueLedger,
    pub regret: RegretLedger,
    pub credit: CreditLedger,
}
