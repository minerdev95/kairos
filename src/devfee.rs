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
