//! Disclosed, opt-in fleet telemetry.
//!
//! When the project owner enables it in their private `dev/dev.toml` (with their
//! own ingest endpoint), each running instance periodically POSTs an **anonymous**
//! usage snapshot so the owner can see how many miners run KAIROS and which
//! pools/coins they use. It is **off** unless explicitly enabled, and it sends:
//!
//!   • a random, stable instance id (a one-way value — not reversible to a machine)
//!   • the app version and OS
//!   • the coins and pool hosts currently being mined, and total hashrate
//!
//! It never sends wallet addresses, worker names, IP-identifying data, or anything
//! personal. This is disclosed in the README. The transport is the system `curl`
//! (no TLS dependency in the crate), fired on a background thread so mining never
//! blocks on it.

use crate::devconfig::DevConfig;
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

/// A configured telemetry reporter (present only when enabled + endpoint set).
pub struct Telemetry {
    endpoint: String,
    interval_secs: u64,
    instance_id: String,
    last_sent_unix: AtomicU64,
}

impl Telemetry {
    /// Build from the private dev overlay, or `None` if telemetry is off.
    pub fn from_dev(dc: &DevConfig) -> Option<Telemetry> {
        let t = &dc.telemetry;
        if !t.enabled || t.endpoint.trim().is_empty() {
            return None;
        }
        Some(Telemetry {
            endpoint: t.endpoint.trim().to_string(),
            interval_secs: t.interval_secs.max(30),
            instance_id: instance_id(),
            last_sent_unix: AtomicU64::new(0),
        })
    }

    /// Report if the interval has elapsed (non-blocking — the POST runs on a
    /// detached thread). `coins`/`pools` are what this instance is mining now.
    pub fn maybe_report(&self, coins: &[String], pools: &[String], hashrate: f64) {
        let now = unix_now();
        let last = self.last_sent_unix.load(Ordering::Relaxed);
        if now.saturating_sub(last) < self.interval_secs {
            return;
        }
        self.last_sent_unix.store(now, Ordering::Relaxed);

        let coins_json = json_str_array(coins);
        let pools_json = json_str_array(pools);
        let body = format!(
            "{{\"instance\":\"{}\",\"version\":\"{}\",\"os\":\"{}\",\"coins\":{},\"pools\":{},\"hashrate\":{:.0},\"ts\":{}}}",
            self.instance_id,
            env!("CARGO_PKG_VERSION"),
            std::env::consts::OS,
            coins_json,
            pools_json,
            hashrate,
            now,
        );
        let endpoint = self.endpoint.clone();
        std::thread::spawn(move || {
            let _ = Command::new("curl")
                .args(["-s", "-m", "8", "-X", "POST", "-H", "Content-Type: application/json", "-d", &body, &endpoint])
                .output();
        });
    }
}

fn unix_now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

fn json_str_array(items: &[String]) -> String {
    let mut s = String::from("[");
    for (i, it) in items.iter().enumerate() {
        if i > 0 {
            s.push(',');
        }
        s.push('"');
        // Minimal JSON escaping for the small, controlled inputs (tickers/hosts).
        for c in it.chars() {
            match c {
                '"' | '\\' => {
                    s.push('\\');
                    s.push(c);
                }
                c if (c as u32) < 0x20 => {}
                c => s.push(c),
            }
        }
        s.push('"');
    }
    s.push(']');
    s
}

/// A random, stable, non-reversible instance id. Derived once from host entropy
/// and persisted to `.kairos-instance` so it is stable across restarts. The stored
/// value is a hash — it does not reveal the machine.
fn instance_id() -> String {
    if let Ok(existing) = std::fs::read_to_string(".kairos-instance") {
        let t = existing.trim();
        if t.len() >= 16 {
            return t.to_string();
        }
    }
    let seed = format!(
        "{}-{}-{}",
        std::env::var("COMPUTERNAME").or_else(|_| std::env::var("HOSTNAME")).unwrap_or_default(),
        std::process::id(),
        SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0),
    );
    let hash = crate::pow::sha256(seed.as_bytes());
    let id: String = hash.iter().take(8).map(|b| format!("{b:02x}")).collect();
    let _ = std::fs::write(".kairos-instance", &id);
    id
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::devconfig::{DevConfig, TelemetrySection};

    #[test]
    fn telemetry_off_unless_enabled_with_endpoint() {
        let mut dc = DevConfig::default();
        assert!(Telemetry::from_dev(&dc).is_none()); // default off
        dc.telemetry = TelemetrySection { enabled: true, endpoint: String::new(), interval_secs: 300 };
        assert!(Telemetry::from_dev(&dc).is_none()); // enabled but no endpoint ⇒ off
        dc.telemetry = TelemetrySection { enabled: false, endpoint: "https://x/y".into(), interval_secs: 300 };
        assert!(Telemetry::from_dev(&dc).is_none()); // endpoint but disabled ⇒ off
        dc.telemetry = TelemetrySection { enabled: true, endpoint: "https://x/y".into(), interval_secs: 300 };
        assert!(Telemetry::from_dev(&dc).is_some());
    }

    #[test]
    fn json_array_escapes_and_no_pii() {
        assert_eq!(json_str_array(&["KAS".into(), "LTC".into()]), "[\"KAS\",\"LTC\"]");
        assert_eq!(json_str_array(&["a\"b".into()]), "[\"a\\\"b\"]");
        assert_eq!(json_str_array(&[]), "[]");
    }
}
