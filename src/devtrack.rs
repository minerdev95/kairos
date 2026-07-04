//! Server-free dev-fee tracking. The disclosed 1% mines to the owner's per-coin
//! address on whatever pool each user picks — so the **pools already track it**. This
//! reads that back from pools' public APIs: no telemetry, no phone-home, no server,
//! no user data collected. It shows the live hashrate + workers + balance on the dev
//! addresses across major pools — a direct, privacy-clean view of adoption + earnings.
//!
//! Owner-only: gated by the admin passphrase so a shipped binary doesn't leak the
//! owner's revenue to users. `kairos dev-track "<admin-passphrase>"`.

use serde_json::Value;

/// One pool's view of an address.
struct PoolStat {
    pool: String,
    hashrate_hs: f64,
    workers: u64,
    unpaid: f64, // in the coin's whole units
    paid: f64,
}

fn get_json(url: &str) -> Option<Value> {
    let body = crate::market_data::curl(url, 10)?;
    serde_json::from_str(&body).ok()
}

/// herominers-family API: /api/stats_address?address=X
fn herominers(host: &str, addr: &str, div: f64) -> Option<PoolStat> {
    let v = get_json(&format!("https://{host}/api/stats_address?address={addr}"))?;
    let s = &v["stats"];
    let hr = v["currentHashrate"].as_f64().or_else(|| s["hashrate"].as_f64()).unwrap_or(0.0);
    Some(PoolStat {
        pool: host.to_string(),
        hashrate_hs: hr,
        workers: v["workers"].as_array().map(|a| a.len() as u64).unwrap_or(0),
        unpaid: s["balance"].as_f64().unwrap_or(0.0) / div,
        paid: s["paid"].as_f64().unwrap_or(0.0) / div,
    })
}

/// 2miners-family (open-ethereum-pool) API: /api/accounts/X
fn twominers(host: &str, addr: &str, div: f64) -> Option<PoolStat> {
    let v = get_json(&format!("https://{host}/api/accounts/{addr}"))?;
    let s = &v["stats"];
    Some(PoolStat {
        pool: host.to_string(),
        hashrate_hs: v["currentHashrate"].as_f64().unwrap_or(0.0),
        workers: v["workersOnline"].as_u64().unwrap_or(0),
        unpaid: s["balance"].as_f64().unwrap_or(0.0) / div,
        paid: s["paid"].as_f64().unwrap_or(0.0) / div,
    })
}

/// Query the well-known pools for a coin's dev address.
fn pools_for(coin: &str, addr: &str) -> Vec<PoolStat> {
    // (div = smallest-unit → whole-coin divisor for balances)
    let candidates: Vec<Option<PoolStat>> = match coin {
        "ERG" => vec![
            herominers("ergo.herominers.com", addr, 1e9),
            twominers("erg.2miners.com", addr, 1e9),
        ],
        "ETC" => vec![twominers("etc.2miners.com", addr, 1e18)],
        "RVN" => vec![twominers("rvn.2miners.com", addr, 1e8)],
        "KAS" => vec![herominers("kaspa.herominers.com", addr, 1e8)],
        "ERGO" => vec![herominers("ergo.herominers.com", addr, 1e9)],
        _ => vec![],
    };
    candidates.into_iter().flatten().collect()
}

fn hr(h: f64) -> String {
    if h >= 1e9 {
        format!("{:.2} GH/s", h / 1e9)
    } else if h >= 1e6 {
        format!("{:.2} MH/s", h / 1e6)
    } else if h >= 1e3 {
        format!("{:.1} kH/s", h / 1e3)
    } else {
        format!("{h:.0} H/s")
    }
}

/// Print the dev-fee's live activity + earnings across pools. Owner-only.
pub fn track(passphrase: &str) {
    let dc = match crate::devconfig::DevConfig::effective() {
        Some(d) => d,
        None => {
            println!("no dev overlay in this binary (public build).");
            return;
        }
    };
    if !dc.check_passphrase(passphrase) {
        eprintln!("admin passphrase incorrect — dev-track is owner-only.");
        return;
    }
    println!("DEV-FEE TRACKER  (live pool stats for the baked dev addresses — no telemetry)");
    println!("  the 1% mines to these addresses on whatever pool each user runs; the pools");
    println!("  report it back. Hashrate here ≈ how much KAIROS is running out there.\n");

    let mut any = false;
    let mut fleet_hr = 0.0;
    for (coin, addr) in &dc.wallets {
        if addr.trim().is_empty() {
            continue;
        }
        let stats = pools_for(coin, addr);
        if stats.is_empty() {
            continue; // no pool API wired for this coin yet
        }
        any = true;
        let coin_hr: f64 = stats.iter().map(|s| s.hashrate_hs).sum();
        let workers: u64 = stats.iter().map(|s| s.workers).sum();
        let unpaid: f64 = stats.iter().map(|s| s.unpaid).sum();
        let paid: f64 = stats.iter().map(|s| s.paid).sum();
        fleet_hr += coin_hr;
        println!("{coin}   {} · {workers} worker(s) · unpaid {unpaid:.6} · paid {paid:.6}", hr(coin_hr));
        for s in &stats {
            if s.hashrate_hs > 0.0 || s.unpaid > 0.0 || s.paid > 0.0 {
                println!("    {:<26} {}  {} wk  unpaid {:.6}", s.pool, hr(s.hashrate_hs), s.workers, s.unpaid);
            } else {
                println!("    {:<26} (no activity yet)", s.pool);
            }
        }
    }
    if !any {
        println!("  no pool APIs matched the baked coins yet (currently: ERG, ETC, RVN, KAS).");
        println!("  Activity appears here once miners run KAIROS and the 1% mines to your address.");
    } else {
        println!("\n  fleet dev-fee hashrate (tracked pools): {}", hr(fleet_hr));
        println!("  Note: only the pools KAIROS knows are queried; add more in src/devtrack.rs.");
    }
}
