//! The console — section 22. Reads like the consoles professionals already run,
//! with the differentiators carried in the same restrained style. An instrument
//! panel, not a brochure.

use crate::model::*;
use crate::runtime::Engine;
use crate::twin::SimWorld;
use std::collections::BTreeMap;

/// A minimalist console banner echoing the logo: three ascending bars rising to
/// the accent node ("the opportune moment") beside the wordmark.
pub fn banner() -> String {
    let mut s = String::new();
    s.push('\n');
    s.push_str("  \u{2583}\u{2585}\u{2587}\u{25C6}  KAIROS\n");
    s.push_str("        intelligent mining control plane              v0.1.0\n");
    s.push('\n');
    s
}

fn human_hashrate(h: f64) -> String {
    if h >= 1e15 {
        format!("{:.2} PH/s", h / 1e15)
    } else if h >= 1e12 {
        format!("{:.2} TH/s", h / 1e12)
    } else if h >= 1e9 {
        format!("{:.2} GH/s", h / 1e9)
    } else if h >= 1e6 {
        format!("{:.2} MH/s", h / 1e6)
    } else if h >= 1e3 {
        format!("{:.2} kH/s", h / 1e3)
    } else {
        format!("{:.0} H/s", h)
    }
}

struct Group {
    count: usize,
    hashrate: f64,
    power: f64,
    temp_sum: f64,
    reject_sum: f64,
}

/// Render the console summary (the `kairos status` view).
pub fn render(engine: &Engine, world: &SimWorld) -> String {
    let belief = world.sense();
    let mut out = String::new();
    let net_day = engine.net_per_day();
    let uplift = engine
        .mining_uplift_frac()
        .map(|u| format!("{:+.1}% vs baseline (run)", u * 100.0))
        .unwrap_or_else(|| "baseline warming up".into());

    let total_h: f64 = belief.devices.values().map(|t| t.hashrate).sum();
    let sites = world.sites();

    out.push_str("  KAIROS  v0.1.0   intelligent mining control plane\n");
    out.push_str("  --------------------------------------------------------------------\n");
    out.push_str(&format!(
        "  fleet  {} sites · {} devices · {:<18}  uptime {:.2}% (run)\n",
        sites.len(),
        belief.devices.len(),
        human_hashrate(total_h),
        engine.uptime_frac() * 100.0
    ));
    let mode = if engine.scheduled_pause { "off-peak pause" } else { engine.autonomy.label() };
    out.push_str(&format!(
        "  mode   {:<14} risk {:<10} uplift {}\n\n",
        mode, engine.risk_word, uplift
    ));

    // Per-device assignment from the last decision.
    let mut asg: BTreeMap<DeviceId, Assignment> = BTreeMap::new();
    if let Some(d) = &engine.last_decision {
        for sp in &d.action.setpoints {
            if let Some(a) = &sp.assignment {
                asg.insert(sp.device.clone(), a.clone());
            }
        }
    }

    for site in &sites {
        let site_power: f64 = belief
            .devices
            .iter()
            .filter(|(id, _)| engine.devices.get(*id).map(|d| &d.site == site).unwrap_or(false))
            .map(|(_, t)| t.power_w)
            .sum();
        let cap = engine.budget.cap(site);
        let cap_str = if cap.is_finite() {
            format!("{:.2}", cap / 1e6)
        } else {
            "—".into()
        };
        let mut dr = String::new();
        if belief.dr_credit_usd_kwh.is_some() {
            dr = "   demand-response: armed".into();
        }
        out.push_str(&format!(
            "  [{}]  power {:.2} / {} MW   ambient {:.0}C{}\n",
            site,
            site_power / 1e6,
            cap_str,
            belief.ambient_c,
            dr
        ));

        // Group running devices by (algo, coin).
        let mut groups: BTreeMap<(AlgorithmId, CoinId, DeviceClass), Group> = BTreeMap::new();
        for (id, tel) in &belief.devices {
            let dev = match engine.devices.get(id) {
                Some(d) if &d.site == site => d,
                _ => continue,
            };
            if let Some(a) = asg.get(id) {
                if !tel.online || tel.hashrate <= 0.0 {
                    continue;
                }
                let g = groups
                    .entry((a.algo.clone(), a.coin.clone(), dev.class))
                    .or_insert(Group {
                        count: 0,
                        hashrate: 0.0,
                        power: 0.0,
                        temp_sum: 0.0,
                        reject_sum: 0.0,
                    });
                g.count += 1;
                g.hashrate += tel.hashrate;
                g.power += tel.power_w;
                g.temp_sum += tel.temp_c;
                g.reject_sum += tel.reject_rate;
            }
        }
        for ((algo, coin, class), g) in &groups {
            let kind = match class {
                DeviceClass::Gpu => "GPU ",
                DeviceClass::Asic => "ASIC",
                DeviceClass::Fpga => "FPGA",
            };
            let eff = if g.hashrate > 0.0 {
                format!("{:.1} J/TH", g.power / (g.hashrate / 1e12))
            } else {
                "—".into()
            };
            out.push_str(&format!(
                "    {} x{:<4} {:<11} -> {:<4} {:>12}  {:.0}C  {:>5.0}W  {}\n",
                kind,
                g.count,
                algo,
                coin,
                human_hashrate(g.hashrate),
                g.temp_sum / g.count.max(1) as f64,
                g.power,
                eff
            ));
        }
        // Idle/curtailed devices on this site.
        let idle = belief
            .devices
            .iter()
            .filter(|(id, t)| {
                engine.devices.get(*id).map(|d| &d.site == site).unwrap_or(false)
                    && (asg.get(*id).is_none() || t.hashrate <= 0.0)
            })
            .count();
        if idle > 0 {
            out.push_str(&format!("    idle x{:<4} curtailed / restarting / thermal-held\n", idle));
        }
    }
    out.push('\n');

    // Pools.
    for (id, p) in &belief.pools {
        let status = if p.online { "ok" } else { "OUTAGE" };
        out.push_str(&format!(
            "  pool  {:<9} {:<32} {:>4}  {:>4.0}ms\n",
            id,
            world.market.pool(id).map(|d| d.url.clone()).unwrap_or_default(),
            status,
            p.latency_ms.min(99999.0)
        ));
    }
    out.push('\n');

    // Decision feed (most recent first).
    for r in engine.rationale.iter().rev().take(4) {
        out.push_str(&format!("  {:>8.0}s  {}\n", r.t_secs, r.message));
    }
    for e in engine.events.iter().rev().take(3) {
        out.push_str(&format!("  {}\n", e));
    }
    out.push('\n');

    let grid = engine.grid_income();
    let mining_net = engine.ledgers.value.mining_net_usd();
    out.push_str(&format!(
        "  net  ${:.2} / day   mining ${:.2} · grid ${:.2}        confidence {}\n",
        net_day,
        mining_net.max(0.0),
        grid,
        confidence_word(belief.confidence)
    ));
    out.push_str(&format!(
        "       sim2real-err {:.1}%   uptime {:.2}%   regret/dec ${:.4}\n",
        world.s2r_error() * 100.0,
        engine.uptime_frac() * 100.0,
        engine.ledgers.regret.mean_regret()
    ));
    out
}

fn confidence_word(c: f64) -> &'static str {
    if c >= 0.75 {
        "high"
    } else if c >= 0.5 {
        "medium"
    } else {
        "low"
    }
}
