//! Best-effort live market data via the system `curl` — prices *and* network
//! difficulty/reward, so profitability reflects the real network, not estimates.
//!
//! Sources (all no-key, best-effort; any failure degrades gracefully to the
//! engine's configured values):
//! * **CoinGecko** — USD spot prices for every coin.
//! * **WhatToMine** (`coins.json`) — network hashrate, block time, and block
//!   reward for the GPU coins (ERG/ETC/RVN…). We derive difficulty as
//!   `hashes_per_block = nethash × block_time`, which is exactly the unit the
//!   profit formula (`reward / difficulty × price × hashrate`) needs — so live
//!   data slots in without the unit-mismatch that would distort margins.
//! * **blockchain.info** — Bitcoin difficulty (`× 2³²` for hashes-per-block).

use serde_json::Value;
use std::collections::BTreeMap;
use std::process::Command;

/// (coingecko id, KAIROS ticker) for the built-in coin universe.
const COINS: &[(&str, &str)] = &[
    ("bitcoin", "BTC"),
    ("kaspa", "KAS"),
    ("ergo", "ERG"),
    ("ethereum-classic", "ETC"),
    ("ravencoin", "RVN"),
    ("litecoin", "LTC"),
];

/// WhatToMine tags we consume difficulty/reward for (GPU coins it lists).
const WTM_TAGS: &[&str] = &["ERG", "ETC", "RVN"];

/// Live market facts for one coin. Every field is optional — the engine keeps its
/// existing value for anything a feed doesn't provide.
#[derive(Clone, Copy, Debug, Default)]
pub struct CoinLive {
    pub price_usd: Option<f64>,
    /// Difficulty expressed as expected hashes per block (formula-ready).
    pub hashes_per_block: Option<f64>,
    pub block_reward: Option<f64>,
}

fn curl(url: &str, timeout_s: u32) -> Option<String> {
    let out = Command::new("curl")
        .args(["-s", "-m", &timeout_s.to_string(), url])
        .output()
        .ok()?;
    if out.stdout.is_empty() {
        return None;
    }
    Some(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Fetch USD prices for the known coins (kept as a standalone helper too).
pub fn fetch_prices() -> BTreeMap<String, f64> {
    let ids: Vec<&str> = COINS.iter().map(|(id, _)| *id).collect();
    let url = format!(
        "https://api.coingecko.com/api/v3/simple/price?ids={}&vs_currencies=usd",
        ids.join(",")
    );
    let mut map = BTreeMap::new();
    if let Some(body) = curl(&url, 8) {
        if let Ok(v) = serde_json::from_str::<Value>(&body) {
            for (id, ticker) in COINS {
                if let Some(p) = v[*id]["usd"].as_f64() {
                    if p > 0.0 {
                        map.insert(ticker.to_string(), p);
                    }
                }
            }
        }
    }
    map
}

/// Merge WhatToMine network stats (difficulty via nethash×block_time, reward).
fn fetch_whattomine(out: &mut BTreeMap<String, CoinLive>) {
    let body = match curl("https://whattomine.com/coins.json", 12) {
        Some(b) => b,
        None => return,
    };
    let v: Value = match serde_json::from_str(&body) {
        Ok(v) => v,
        Err(_) => return,
    };
    let coins = match v["coins"].as_object() {
        Some(c) => c,
        None => return,
    };
    for c in coins.values() {
        let tag = c["tag"].as_str().unwrap_or("");
        if !WTM_TAGS.contains(&tag) {
            continue;
        }
        let entry = out.entry(tag.to_string()).or_default();
        let nethash = c["nethash"].as_f64();
        // block_time is a string like "120.0".
        let block_time = c["block_time"].as_str().and_then(|s| s.parse::<f64>().ok());
        if let (Some(nh), Some(bt)) = (nethash, block_time) {
            if nh > 0.0 && bt > 0.0 {
                entry.hashes_per_block = Some(nh * bt);
            }
        }
        if let Some(r) = c["block_reward"].as_f64() {
            if r > 0.0 {
                entry.block_reward = Some(r);
            }
        }
    }
}

/// Live Bitcoin difficulty → hashes per block (difficulty × 2³²). Reward is the
/// fixed post-2024-halving subsidy.
fn fetch_btc(out: &mut BTreeMap<String, CoinLive>) {
    if let Some(body) = curl("https://blockchain.info/q/getdifficulty", 8) {
        if let Ok(diff) = body.trim().parse::<f64>() {
            if diff > 0.0 {
                let entry = out.entry("BTC".to_string()).or_default();
                entry.hashes_per_block = Some(diff * 4_294_967_296.0);
                entry.block_reward = Some(3.125);
            }
        }
    }
}

/// The full live market snapshot: prices for all coins plus live difficulty/reward
/// where a feed provides it.
pub fn fetch_market() -> BTreeMap<String, CoinLive> {
    let mut out: BTreeMap<String, CoinLive> = BTreeMap::new();
    for (coin, price) in fetch_prices() {
        out.entry(coin).or_default().price_usd = Some(price);
    }
    fetch_whattomine(&mut out);
    fetch_btc(&mut out);
    out
}
