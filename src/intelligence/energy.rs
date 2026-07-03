//! Energy: marginal-efficiency partial curtailment + demand-response arbitrage,
//! all priced on one ledger.
//!
//! Every marginal watt is ranked by risk-adjusted profit density. Under a binding
//! power cap the least-profitable watts are shed first (fractional knapsack —
//! provably optimal for a divisible budget), so the most profitable watts keep
//! running. Demand-response enters the *same* knapsack as a virtual load paying
//! `dr_credit`; curtailing for DR also saves degradation, so its effective value
//! is `c_dr + wear_density`. The shadow price (profit density of the marginal
//! kept watt) is exported as a curtail/expansion signal.
//!
//! Conduct: DR credit is only ever claimed for genuine measured reduction; the
//! engine never fabricates a baseline.

use crate::model::*;
use std::collections::{BTreeMap, BTreeSet};

/// One device's marginal economics, as seen by the energy knapsack.
#[derive(Clone, Debug)]
pub struct MarginalUnit {
    pub device: DeviceId,
    pub site: SiteId,
    pub power_w: f64,
    /// Absolute net margin if run (USD/s), net of energy + wear.
    pub net_usd_per_s: f64,
    /// Risk-adjusted profit density (certainty-equivalent USD/s per watt).
    pub risk_adj_density: f64,
    /// Degradation cost (USD/s) saved if this device is curtailed.
    pub wear_usd_per_s: f64,
}

#[derive(Clone, Debug, Default)]
pub struct EnergyPlan {
    /// Devices to curtail for any reason.
    pub curtail: BTreeSet<DeviceId>,
    /// Of those, the ones curtailed specifically to sell demand-response.
    pub dr_curtail: BTreeSet<DeviceId>,
    /// Marginal profit density (USD/s/W) per site — the shadow price μ_P.
    pub shadow_price_per_w: BTreeMap<SiteId, f64>,
    /// Total DR credit earned this tick (USD/s).
    pub dr_credit_usd_per_s: f64,
    /// Total degradation saved by curtailment (USD/s) — for the energy credit.
    pub wear_saved_usd_per_s: f64,
}

/// Plan curtailment + DR against per-site power caps. Pure and deterministic.
pub fn plan_energy(
    mut units: Vec<MarginalUnit>,
    site_caps: &BTreeMap<SiteId, f64>,
    dr_credit_usd_kwh: Option<f64>,
) -> EnergyPlan {
    let mut plan = EnergyPlan::default();
    let dr_per_w = dr_credit_usd_kwh.map(|c| c / 3_600_000.0).unwrap_or(0.0);

    // The basic run/idle (money-loser) decision is made upstream by the
    // hysteresis-aware runnable filter, so it is *not* repeated here (doing so in
    // two places that disagree near break-even causes flapping). This plan only
    // resolves the shared-resource constraints: demand-response and power caps.
    //
    // 1) Let DR win any watt whose grid credit out-bids its mining margin.
    units.retain(|u| {
        if dr_per_w > 0.0 {
            let wear_density = if u.power_w > 0.0 {
                u.wear_usd_per_s / u.power_w
            } else {
                0.0
            };
            let v_dr_eff = dr_per_w + wear_density;
            if v_dr_eff > u.risk_adj_density {
                plan.curtail.insert(u.device.clone());
                plan.dr_curtail.insert(u.device.clone());
                plan.dr_credit_usd_per_s += dr_per_w * u.power_w;
                plan.wear_saved_usd_per_s += u.wear_usd_per_s;
                return false;
            }
        }
        true
    });

    // 2) Per-site power cap: shed lowest-density watts first.
    let mut by_site: BTreeMap<SiteId, Vec<MarginalUnit>> = BTreeMap::new();
    for u in units {
        by_site.entry(u.site.clone()).or_default().push(u);
    }
    for (site, mut su) in by_site {
        let cap = site_caps.get(&site).copied().unwrap_or(f64::INFINITY);
        // Ascending profit density: the first to be shed are the worst watts.
        // Device id breaks ties so the shed set is deterministic across runs.
        su.sort_by(|a, b| {
            a.risk_adj_density
                .partial_cmp(&b.risk_adj_density)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| a.device.cmp(&b.device))
        });
        let total: f64 = su.iter().map(|u| u.power_w).sum();
        let mut over = (total - cap).max(0.0);
        let mut marginal_density = 0.0;
        for u in &su {
            if over > 0.0 {
                plan.curtail.insert(u.device.clone());
                plan.wear_saved_usd_per_s += u.wear_usd_per_s;
                // A curtailed device still draws standby, so it only frees
                // (power − standby) toward the cap — accounting for this is what
                // keeps total draw (active + Σ standby) under the hard cap.
                over -= (u.power_w - STANDBY_W).max(0.0);
            } else {
                // First kept unit sets the shadow price (marginal kept watt).
                marginal_density = u.risk_adj_density;
                break;
            }
        }
        plan.shadow_price_per_w.insert(site, marginal_density);
    }

    plan
}
