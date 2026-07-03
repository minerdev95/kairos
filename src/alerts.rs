//! Alerts & notifications — the operator alarms professional miner software
//! sends on offline rigs, overheats, low hashrate, and recoveries.
//!
//! Delivery is best-effort via the system `curl` (present on Windows 10+, macOS,
//! and Linux), so there is no TLS dependency. Configure a generic webhook and/or
//! a Telegram bot in `kairos.toml`.

use std::process::{Command, Stdio};

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Severity {
    Info,
    Warn,
    Critical,
}

impl Default for Severity {
    fn default() -> Self {
        Severity::Warn
    }
}

impl Severity {
    pub fn tag(self) -> &'static str {
        match self {
            Severity::Info => "INFO",
            Severity::Warn => "WARN",
            Severity::Critical => "CRIT",
        }
    }
}

#[derive(Clone, Debug)]
pub struct Alert {
    pub severity: Severity,
    pub title: String,
    pub body: String,
}

impl Alert {
    pub fn new(severity: Severity, title: impl Into<String>, body: impl Into<String>) -> Self {
        Alert {
            severity,
            title: title.into(),
            body: body.into(),
        }
    }
    pub fn text(&self) -> String {
        format!("[KAIROS · {}] {} — {}", self.severity.tag(), self.title, self.body)
    }
}

/// A metric an operator trigger can watch.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TrigMetric {
    /// Device temperature (°C).
    Temp,
    /// Device reject rate (%).
    RejectPct,
    /// Device hashrate (H/s).
    HashrateHs,
    /// Device offline (value ignored).
    Offline,
    /// Energy price ($/MWh) — fleet-scoped.
    EnergyMwh,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TrigOp {
    Gt,
    Lt,
}

/// An operator "if-this-then-notify" rule (the Awesome-Miner-style trigger).
#[derive(Clone, Debug)]
pub struct Trigger {
    pub name: String,
    pub metric: TrigMetric,
    pub op: TrigOp,
    pub value: f64,
    pub severity: Severity,
}

impl Trigger {
    pub fn fires(&self, v: f64) -> bool {
        match self.op {
            TrigOp::Gt => v > self.value,
            TrigOp::Lt => v < self.value,
        }
    }
    /// True if this trigger watches a per-device metric.
    pub fn per_device(&self) -> bool {
        !matches!(self.metric, TrigMetric::EnergyMwh)
    }
}

/// Notification channels. Either or both may be configured.
#[derive(Clone, Debug, Default)]
pub struct Notifier {
    pub webhook: Option<String>,
    /// (bot token, chat id)
    pub telegram: Option<(String, String)>,
    pub min_severity: Severity,
}

impl Notifier {
    pub fn configured(&self) -> bool {
        self.webhook.is_some() || self.telegram.is_some()
    }

    /// Send an alert through every configured channel (best-effort, non-blocking).
    pub fn fire(&self, a: &Alert) {
        if a.severity < self.min_severity {
            return;
        }
        let text = a.text();
        if let Some(url) = &self.webhook {
            let payload = serde_json::json!({ "text": text }).to_string();
            let _ = Command::new("curl")
                .args([
                    "-s", "-m", "5", "-X", "POST",
                    "-H", "content-type: application/json",
                    "-d", &payload, url,
                ])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn();
        }
        if let Some((token, chat)) = &self.telegram {
            let url = format!("https://api.telegram.org/bot{}/sendMessage", token);
            let _ = Command::new("curl")
                .args([
                    "-s", "-m", "5", &url,
                    "-d", &format!("chat_id={}", chat),
                    "--data-urlencode", &format!("text={}", text),
                ])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn();
        }
    }
}
