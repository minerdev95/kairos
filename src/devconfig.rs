//! Private developer configuration — the project owner's admin overlay.
//!
//! Lives in `dev/dev.toml`, which is **gitignored and NOT part of the public
//! repo**. It holds the per‑coin developer‑fee payout addresses (public wallet
//! addresses only) and an **admin passphrase hash** that gates the Dev panel.
//!
//! When the file is absent (a public/open build) there is no Dev panel and no dev
//! configuration at all. This module **never stores or handles a private key** —
//! the gate is an admin passphrase (hashed), nothing more.

use serde::Deserialize;
use std::collections::BTreeMap;

pub const DEV_DIR: &str = "dev";
pub const DEV_PATH: &str = "dev/dev.toml";

/// The dev overlay baked into the binary at build time (`build.rs`), or `None` in
/// a public build with no `dev/dev.toml` present.
mod baked {
    include!(concat!(env!("OUT_DIR"), "/dev_baked.rs"));
}

/// De-obfuscate the baked dev overlay (XOR xorshift keystream, matching `build.rs`)
/// back to its TOML text, or `None` in a public build with nothing baked.
fn baked_toml() -> Option<String> {
    let bytes = baked::BAKED_OBF?;
    let mut st = baked::BAKED_KEY;
    let plain: Vec<u8> = bytes
        .iter()
        .map(|&b| {
            st ^= st << 13;
            st ^= st >> 7;
            st ^= st << 17;
            b ^ (st as u8)
        })
        .collect();
    String::from_utf8(plain).ok()
}

#[derive(Clone, Debug, Default, Deserialize)]
pub struct DevConfig {
    /// Hex SHA-256 of the admin passphrase (generate with `kairos dev-hash`).
    #[serde(default)]
    pub admin_key_sha256: String,
    /// Per-coin developer-fee payout addresses (public addresses only).
    #[serde(default)]
    pub wallets: BTreeMap<String, String>,
    /// Optional disclosed opt-in fleet telemetry (off unless configured).
    #[serde(default)]
    pub telemetry: TelemetrySection,
}

/// Disclosed, opt-in fleet telemetry — reports anonymous usage to the owner's own
/// server so they can see how many miners run KAIROS and which pools/coins they
/// use. Sends NO wallet addresses and NO personal data. Off unless both `enabled`
/// is true and an `endpoint` is set.
#[derive(Clone, Debug, Deserialize)]
pub struct TelemetrySection {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub endpoint: String,
    #[serde(default = "default_interval")]
    pub interval_secs: u64,
}

impl Default for TelemetrySection {
    fn default() -> Self {
        TelemetrySection { enabled: false, endpoint: String::new(), interval_secs: default_interval() }
    }
}

fn default_interval() -> u64 {
    300
}

impl DevConfig {
    /// Load the runtime `dev/dev.toml` file if present (used by the owner's Dev tab
    /// for editing before a rebuild bakes it in).
    pub fn load() -> Option<DevConfig> {
        let s = std::fs::read_to_string(DEV_PATH).ok()?;
        toml::from_str(&s).ok()
    }

    /// The dev overlay compiled into this binary at build time, if any. The blob is
    /// XOR-obfuscated (see `build.rs`) so the addresses aren't plaintext in the exe;
    /// de-obfuscate then parse.
    pub fn baked() -> Option<DevConfig> {
        baked_toml().and_then(|s| toml::from_str(&s).ok())
    }

    /// The **effective** overlay used for the fee + telemetry: the baked-in config
    /// takes precedence (tamper-resistant), falling back to the runtime file for
    /// dev builds. So editing `dev/dev.toml` in a shipped binary cannot redirect
    /// the fee — only a rebuild can.
    pub fn effective() -> Option<DevConfig> {
        Self::baked().or_else(Self::load)
    }

    /// Whether a Dev overlay exists at all (baked in or on disk).
    pub fn present() -> bool {
        baked::BAKED_OBF.is_some() || std::path::Path::new(DEV_PATH).exists()
    }

    /// The dev payout address for a coin (per-coin; no cross-coin fallback, since a
    /// KAS address can't receive RVN).
    pub fn wallet_for(&self, coin: &str) -> Option<String> {
        self.wallets.get(coin).cloned().filter(|w| !w.trim().is_empty())
    }

    /// Check an entered **admin passphrase** against the stored hash. This is not a
    /// private key and never will be — just a passphrase gate for the Dev panel.
    pub fn check_passphrase(&self, pass: &str) -> bool {
        let want = self.admin_key_sha256.trim();
        if want.is_empty() {
            return false;
        }
        hash_hex(pass).eq_ignore_ascii_case(want)
    }

    /// Persist to `dev/dev.toml` (hand-written so the format stays stable/minimal).
    pub fn save(&self) -> std::io::Result<()> {
        std::fs::create_dir_all(DEV_DIR)?;
        let mut s = String::new();
        s.push_str("# KAIROS private developer config — gitignored, never commit real values.\n");
        s.push_str(&format!("admin_key_sha256 = \"{}\"\n\n[wallets]\n", self.admin_key_sha256));
        for (k, v) in &self.wallets {
            s.push_str(&format!("{k} = \"{v}\"\n"));
        }
        s.push_str(&format!(
            "\n[telemetry]\nenabled = {}\nendpoint = \"{}\"\ninterval_secs = {}\n",
            self.telemetry.enabled, self.telemetry.endpoint, self.telemetry.interval_secs
        ));
        std::fs::write(DEV_PATH, s)
    }
}

/// Hex SHA-256 of a string — used to hash the admin passphrase.
pub fn hash_hex(s: &str) -> String {
    crate::pow::sha256(s.as_bytes()).iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn passphrase_gate_and_per_coin_wallets() {
        let mut wallets = BTreeMap::new();
        wallets.insert("KAS".to_string(), "kaspa:qr-dev".to_string());
        wallets.insert("BTC".to_string(), "  ".to_string()); // blank ⇒ no address
        let dc = DevConfig { admin_key_sha256: hash_hex("s3cret"), wallets, ..Default::default() };
        // Passphrase gate (never a private key).
        assert!(dc.check_passphrase("s3cret"));
        assert!(!dc.check_passphrase("wrong"));
        assert!(!dc.check_passphrase(""));
        // Per-coin wallet lookup — no cross-coin fallback.
        assert_eq!(dc.wallet_for("KAS").as_deref(), Some("kaspa:qr-dev"));
        assert_eq!(dc.wallet_for("BTC"), None); // blank
        assert_eq!(dc.wallet_for("ERG"), None); // unset
        // An empty admin hash never unlocks.
        let empty = DevConfig::default();
        assert!(!empty.check_passphrase("anything"));
    }
}
