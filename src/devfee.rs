//! The developer-fee module — section 29 of the spec.
//!
//! A transparent 1% fee to a fixed, disclosed project address. The engine never
//! holds keys and never moves user funds: mining payouts go to the operator's own
//! wallet, and the fee is realized by routing a disclosed fraction of *work* to
//! the project address (`share_route`, the standard, overlay-safe mechanism).
//!
//! Every split is visible and logged. Transparency is not a configurable knob.

use crate::ledger::{ValueKind, ValueLedger};
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub enum DevFeeMode {
    /// Default overlay mechanism: realize the split at the pool by routing a
    /// fraction of work. Preserves speculative state.
    ShareRoute,
    /// Simpler but discards speculative state — avoid where possible.
    TimeSlice,
    /// Own-pool phase: realize the split on the value ledger directly.
    ValueLedger,
    /// Enterprise: 1% of *measured uplift* from the credit ledger and twin.
    Uplift,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DevFee {
    /// Fee rate (1% by default). Disclosed and visible.
    pub rate: f64,
    /// Fixed, disclosed project payout address.
    pub wallet: String,
    pub mode: DevFeeMode,
    /// Always true — surfaced on the dashboard and in logs.
    pub visible: bool,
}

impl Default for DevFee {
    fn default() -> Self {
        DevFee {
            rate: 0.01,
            wallet: "<project_payout_address>".to_string(),
            mode: DevFeeMode::ShareRoute,
            visible: true,
        }
    }
}

/// The split applied on each unit of realized value.
#[derive(Clone, Copy, Debug)]
pub struct FeeSplit {
    pub user_usd: f64,
    pub dev_usd: f64,
}

impl DevFee {
    /// Split realized value `v` (USD) into user and dev shares. The dev share is
    /// only ever a fraction of *work routed*, never custody of user funds.
    pub fn split(&self, v: f64) -> FeeSplit {
        let dev = v * self.rate;
        FeeSplit {
            user_usd: v - dev,
            dev_usd: dev,
        }
    }

    /// Realize a value event: credit the user the bulk, record the disclosed dev
    /// fraction as a visible, logged ledger entry. Returns the user share.
    pub fn realize(&self, ledger: &mut ValueLedger, t: f64, v: f64, source: &str) -> f64 {
        let split = self.split(v);
        // The user share is the engine's primary credit; the dev share is logged
        // as a (negative-to-user) DevFee entry so it is always visible.
        ledger.credit(
            t,
            ValueKind::DevFee,
            -split.dev_usd,
            0.0,
            None,
            None,
            format!("dev_fee {:.2}% -> {} (source: {})", self.rate * 100.0, self.wallet, source),
        );
        split.user_usd
    }
}

/// Live-mining developer time-slice (the real, on-the-wire mechanism shared by every
/// mining path). Mines to the operator's `user` login most of the time, and for ~1% of
/// the time reconnects under the baked dev payout address for `coin`, so the pool itself
/// credits the disclosed fee — no server, no custody of keys. Identical cadence to the
/// Ergo path: operator rounds of `op_chunk` seconds interleaved with `DEV_ROUND_SECS`
/// dev rounds. `session` runs one connected session to a given login until the supplied
/// round deadline (or `shared.stop`); it is the caller's real mining loop.
pub mod time_slice {
    use std::time::{Duration, Instant};

    /// Disclosed developer fee rate (1%).
    pub const DEV_FEE_RATE: f64 = 0.01;
    /// Length of one dev-fee round, in seconds.
    pub const DEV_ROUND_SECS: u64 = 30;

    /// The baked dev payout address for `coin`, if a real one is present.
    pub fn dev_addr(coin: &str) -> Option<String> {
        crate::devconfig::DevConfig::effective()
            .and_then(|d| d.wallet_for(coin))
            .filter(|w| !w.trim().is_empty() && !w.contains('<'))
    }

    /// Run `session` under the 1% developer time-slice. `session(login, round_deadline)`
    /// performs one connected mining session and returns when it hits the deadline/stop
    /// or errors. Loops operator/dev rounds until `deadline` or `shared.stop`.
    pub fn run_with_fee<F>(
        coin: &str,
        user: &str,
        shared: &crate::engine::SessionShared,
        deadline: Option<Instant>,
        mut session: F,
    ) -> std::io::Result<()>
    where
        F: FnMut(&str, Instant) -> std::io::Result<()>,
    {
        use std::sync::atomic::Ordering;
        let dev = dev_addr(coin);
        let fee_active = dev.is_some();
        // Fast test cadence (KAIROS_DEV_FAST) triggers a visible dev round in seconds.
        let fast = std::env::var("KAIROS_DEV_FAST").is_ok();
        let accrue_rate = if fast { 1.0 } else { DEV_FEE_RATE };
        let dev_round: u64 = if fast { 8 } else { DEV_ROUND_SECS };
        let op_chunk: u64 = if fast {
            12
        } else {
            (DEV_ROUND_SECS as f64 / DEV_FEE_RATE).ceil() as u64
        };

        let mut dev_owed = 0.0f64;
        loop {
            if shared.stop.load(Ordering::Relaxed) {
                break;
            }
            if let Some(dl) = deadline {
                if Instant::now() >= dl {
                    break;
                }
            }
            let is_dev = fee_active && dev_owed >= dev_round as f64;
            let (login, round_secs) = if is_dev {
                (dev.clone().unwrap(), dev_round)
            } else {
                (user.to_string(), op_chunk)
            };
            if is_dev && std::env::var("KAIROS_DEV_DEBUG").is_ok() {
                eprintln!("[dev-fee] {dev_round}s round -> {login}");
            }
            let mut round_dl = Instant::now() + Duration::from_secs(round_secs);
            if let Some(dl) = deadline {
                if dl < round_dl {
                    round_dl = dl;
                }
            }
            let t0 = Instant::now();
            let r = session(&login, round_dl);
            if is_dev {
                dev_owed -= dev_round as f64;
            } else {
                dev_owed += t0.elapsed().as_secs_f64() * accrue_rate;
            }
            r?;
        }
        Ok(())
    }
}
