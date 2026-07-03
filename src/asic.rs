//! ASIC fleet management via the **CGMiner/bmminer RPC API** (TCP port 4028).
//!
//! KAIROS can't (and shouldn't) reflash ASIC firmware, but nearly every ASIC —
//! Antminer (bmminer), Whatsminer (btminer), Avalon, Goldshell, Innosilicon, and
//! anything running cgminer/bfgminer — exposes the *same* JSON RPC API on port
//! 4028. That API is the standard, vendor-neutral way Awesome Miner / Hive / Foreman
//! manage rigs. This module speaks it directly:
//!
//!   * `summary` → hashrate, accepted/rejected shares, hardware errors, uptime
//!   * `pools`   → configured pools + which is active/alive
//!   * `stats`   → per-board temperatures + fan speeds (vendor-specific keys, parsed
//!                 best-effort by scanning for `temp*` / `fan*`)
//!   * `addpool` + `switchpool` → repoint a miner at a KAIROS-chosen pool (privileged;
//!                 requires the miner's API to allow writes — `--api-allow W:0/0`)
//!
//! **Testing without hardware:** the request/response format is exercised by a mock
//! CGMiner server in the tests below (a `TcpListener` serving canned Antminer JSON),
//! so the client + parsers are verified end-to-end even though no physical ASIC is
//! present in this build environment. On real hardware, point `kairos asic status
//! <ip>` at a miner to confirm.

use serde_json::{json, Value};
use std::io::{Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::time::{Duration, Instant};

pub const DEFAULT_API_PORT: u16 = 4028;

/// A snapshot of one ASIC as read from its CGMiner API.
#[derive(Debug, Clone, Default)]
pub struct AsicInfo {
    pub addr: String,
    pub kind: String,      // miner software / model, e.g. "bmminer 1.0" / "Antminer S19"
    pub ghs: f64,          // hashrate in GH/s (normalized from GHS/MHS/KHS)
    pub accepted: u64,
    pub rejected: u64,
    pub hw_errors: u64,
    pub uptime_s: u64,
    pub temp_c: f64,       // hottest board temperature seen (0 if unknown)
    pub fan_rpm: u64,      // fastest fan seen (0 if unknown)
    pub pools: Vec<AsicPool>,
}

#[derive(Debug, Clone, Default)]
pub struct AsicPool {
    pub url: String,
    pub user: String,
    pub status: String, // "Alive" / "Dead" / "Disabled"
    pub active: bool,   // stratum-active (currently mining to this pool)
    pub accepted: u64,
    pub rejected: u64,
}

impl AsicInfo {
    /// Reject rate as a percentage of submitted shares (0 if none submitted).
    pub fn reject_pct(&self) -> f64 {
        let total = self.accepted + self.rejected;
        if total == 0 {
            0.0
        } else {
            100.0 * self.rejected as f64 / total as f64
        }
    }
    /// The pool the miner is actively hashing to, if any.
    pub fn active_pool(&self) -> Option<&AsicPool> {
        self.pools.iter().find(|p| p.active).or_else(|| self.pools.first())
    }
}

/// Parse "host", "host:port", or "host:port" → a socket target string with the
/// default API port applied when absent.
fn with_port(addr: &str) -> String {
    if addr.contains(':') {
        addr.to_string()
    } else {
        format!("{addr}:{DEFAULT_API_PORT}")
    }
}

/// Issue one CGMiner API command and return the parsed JSON. The API answers with a
/// single JSON document then closes the socket; many firmwares append a trailing NUL
/// which we trim.
pub fn cgminer_call(addr: &str, command: &str, parameter: Option<&str>, timeout: Duration) -> std::io::Result<Value> {
    let target = with_port(addr);
    let sock = target
        .to_socket_addrs()?
        .next()
        .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::NotFound, "no address"))?;
    let mut stream = TcpStream::connect_timeout(&sock, timeout)?;
    stream.set_read_timeout(Some(timeout))?;
    stream.set_write_timeout(Some(timeout))?;

    let req = match parameter {
        Some(p) => json!({ "command": command, "parameter": p }),
        None => json!({ "command": command }),
    };
    stream.write_all(format!("{req}\n").as_bytes())?;

    let mut buf = Vec::with_capacity(4096);
    let mut chunk = [0u8; 2048];
    let start = Instant::now();
    loop {
        match stream.read(&mut chunk) {
            Ok(0) => break, // socket closed = end of response
            Ok(n) => {
                buf.extend_from_slice(&chunk[..n]);
                if buf.len() > 1 << 20 {
                    break; // 1 MiB guard
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock || e.kind() == std::io::ErrorKind::TimedOut => break,
            Err(e) => return Err(e),
        }
        if start.elapsed() > timeout {
            break;
        }
    }
    // Trim trailing NULs / whitespace that bmminer/cgminer append.
    while matches!(buf.last(), Some(0) | Some(b'\n') | Some(b'\r') | Some(b' ')) {
        buf.pop();
    }
    let text = String::from_utf8_lossy(&buf);
    serde_json::from_str(&text).map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))
}

/// Query one ASIC (summary + pools + stats) into an [`AsicInfo`].
pub fn query(addr: &str, timeout: Duration) -> std::io::Result<AsicInfo> {
    let mut info = AsicInfo {
        addr: addr.to_string(),
        ..Default::default()
    };
    let summary = cgminer_call(addr, "summary", None, timeout)?;
    apply_summary(&mut info, &summary);
    // pools + stats are best-effort; a miner with a locked API may refuse them.
    if let Ok(pools) = cgminer_call(addr, "pools", None, timeout) {
        apply_pools(&mut info, &pools);
    }
    if let Ok(stats) = cgminer_call(addr, "stats", None, timeout) {
        apply_stats(&mut info, &stats);
    }
    Ok(info)
}

/// First array section in a CGMiner reply whose key isn't STATUS/id (the payload).
fn section<'a>(v: &'a Value, name: &str) -> Option<&'a Value> {
    v.get(name).and_then(|s| s.as_array()).and_then(|a| a.first())
}

/// Case-insensitive field lookup within a JSON object (CGMiner casing varies).
fn field<'a>(obj: &'a Value, key: &str) -> Option<&'a Value> {
    let map = obj.as_object()?;
    if let Some(v) = map.get(key) {
        return Some(v);
    }
    let lk = key.to_ascii_lowercase();
    map.iter().find(|(k, _)| k.to_ascii_lowercase() == lk).map(|(_, v)| v)
}

fn num(v: Option<&Value>) -> f64 {
    match v {
        Some(Value::Number(n)) => n.as_f64().unwrap_or(0.0),
        Some(Value::String(s)) => s.trim().parse().unwrap_or(0.0),
        _ => 0.0,
    }
}

fn apply_summary(info: &mut AsicInfo, v: &Value) {
    // Identify the miner software from the STATUS section if present.
    if let Some(status) = section(v, "STATUS") {
        if let Some(desc) = field(status, "Description").and_then(|d| d.as_str()) {
            if !desc.is_empty() {
                info.kind = desc.to_string();
            }
        }
    }
    let s = match section(v, "SUMMARY") {
        Some(s) => s,
        None => return,
    };
    // Hashrate: prefer GHS, fall back to MHS/KHS, normalize to GH/s.
    let ghs = num(field(s, "GHS av"));
    let ghs5 = num(field(s, "GHS 5s"));
    let mhs = num(field(s, "MHS av"));
    let khs = num(field(s, "KHS av"));
    info.ghs = if ghs > 0.0 {
        ghs
    } else if ghs5 > 0.0 {
        ghs5
    } else if mhs > 0.0 {
        mhs / 1_000.0
    } else if khs > 0.0 {
        khs / 1_000_000.0
    } else {
        0.0
    };
    info.accepted = num(field(s, "Accepted")) as u64;
    info.rejected = num(field(s, "Rejected")) as u64;
    info.hw_errors = num(field(s, "Hardware Errors")) as u64;
    info.uptime_s = num(field(s, "Elapsed")) as u64;
    // Some firmwares expose temperature straight from summary.
    let t = num(field(s, "Temperature"));
    if t > info.temp_c {
        info.temp_c = t;
    }
}

fn apply_pools(info: &mut AsicInfo, v: &Value) {
    let pools = match v.get("POOLS").and_then(|p| p.as_array()) {
        Some(p) => p,
        None => return,
    };
    info.pools.clear();
    for p in pools {
        let status = field(p, "Status").and_then(|s| s.as_str()).unwrap_or("").to_string();
        let stratum_active = field(p, "Stratum Active").and_then(|s| s.as_bool()).unwrap_or(false);
        info.pools.push(AsicPool {
            url: field(p, "URL").and_then(|s| s.as_str()).unwrap_or("").to_string(),
            user: field(p, "User").and_then(|s| s.as_str()).unwrap_or("").to_string(),
            active: stratum_active,
            status,
            accepted: num(field(p, "Accepted")) as u64,
            rejected: num(field(p, "Rejected")) as u64,
        });
    }
}

/// Stats keys are wildly vendor-specific, so scan every field for anything that
/// looks like a temperature or fan reading and keep the maxima.
fn apply_stats(info: &mut AsicInfo, v: &Value) {
    let stats = match v.get("STATS").and_then(|s| s.as_array()) {
        Some(s) => s,
        None => return,
    };
    for entry in stats {
        let map = match entry.as_object() {
            Some(m) => m,
            None => continue,
        };
        for (k, val) in map {
            let lk = k.to_ascii_lowercase();
            let n = num(Some(val));
            // temperatures: keys containing "temp", plausible 0<t<200 °C.
            if lk.contains("temp") && n > info.temp_c && n < 200.0 {
                info.temp_c = n;
            }
            // fans: keys starting with "fan" and a plausible RPM.
            if lk.starts_with("fan") && n > info.fan_rpm as f64 && n < 30_000.0 {
                info.fan_rpm = n as u64;
            }
        }
    }
}

/// Concurrently probe many targets; returns only those that answered. `targets` may
/// be individual "ip" / "ip:port" strings, or a single "a.b.c.0/24" CIDR.
pub fn scan(targets: &[String], timeout: Duration) -> Vec<AsicInfo> {
    let list = expand_targets(targets);
    let mut handles = Vec::new();
    // Bounded fan-out so a /24 sweep doesn't open 254 sockets at once.
    for batch in list.chunks(32) {
        let mut batch_handles = Vec::new();
        for addr in batch.to_vec() {
            batch_handles.push(std::thread::spawn(move || query(&addr, timeout).ok()));
        }
        for h in batch_handles {
            handles.push(h.join().ok().flatten());
        }
    }
    handles.into_iter().flatten().collect()
}

/// Expand a "/24" CIDR into 254 host addresses; pass other entries through.
fn expand_targets(targets: &[String]) -> Vec<String> {
    let mut out = Vec::new();
    for t in targets {
        if let Some((base, "24")) = t.split_once('/') {
            let octs: Vec<&str> = base.split('.').collect();
            if octs.len() == 4 {
                for host in 1..=254 {
                    out.push(format!("{}.{}.{}.{host}:{DEFAULT_API_PORT}", octs[0], octs[1], octs[2]));
                }
                continue;
            }
        }
        out.push(with_port(t));
    }
    out
}

/// Repoint an ASIC at a new pool (privileged). Adds the pool then switches to it.
/// Requires the miner's API to permit writes (`--api-allow W:...`). Returns the
/// miner's textual acknowledgements.
pub fn switch_pool(addr: &str, url: &str, user: &str, pass: &str, timeout: Duration) -> std::io::Result<String> {
    let add = cgminer_call(addr, "addpool", Some(&format!("{url},{user},{pass}")), timeout)?;
    let add_msg = status_msg(&add);
    // The newly added pool is the highest index; ask for the pool list to find it.
    let pools = cgminer_call(addr, "pools", None, timeout)?;
    let idx = pools
        .get("POOLS")
        .and_then(|p| p.as_array())
        .map(|a| a.len().saturating_sub(1))
        .unwrap_or(0);
    let sw = cgminer_call(addr, "switchpool", Some(&idx.to_string()), timeout)?;
    Ok(format!("{add_msg}; switch→pool {idx}: {}", status_msg(&sw)))
}

fn status_msg(v: &Value) -> String {
    section(v, "STATUS")
        .and_then(|s| field(s, "Msg"))
        .and_then(|m| m.as_str())
        .unwrap_or("(no message)")
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write as _;
    use std::net::TcpListener;

    // Canned Antminer/bmminer-style responses for the mock server.
    const SUMMARY: &str = r#"{"STATUS":[{"STATUS":"S","Description":"bmminer 1.0.0"}],"SUMMARY":[{"GHS av":95000.42,"Accepted":12000,"Rejected":37,"Hardware Errors":5,"Elapsed":86400}],"id":1}"#;
    const POOLS: &str = r#"{"STATUS":[{"STATUS":"S"}],"POOLS":[{"POOL":0,"URL":"stratum+tcp://btc.pool.example:3333","User":"bc1qxyz.worker","Status":"Alive","Stratum Active":true,"Accepted":11950,"Rejected":30},{"POOL":1,"URL":"stratum+tcp://backup.example:3333","User":"bc1qxyz.bak","Status":"Alive","Stratum Active":false,"Accepted":50,"Rejected":7}],"id":1}"#;
    const STATS: &str = r#"{"STATUS":[{"STATUS":"S"}],"STATS":[{"STATS":0,"temp1":62,"temp2":68,"temp2_1":71,"fan1":4200,"fan2":4380,"GHS av":95000.42}],"id":1}"#;

    fn spawn_mock() -> String {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        std::thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let mut s = stream;
                let mut buf = [0u8; 512];
                let _ = s.read(&mut buf);
                let req = String::from_utf8_lossy(&buf);
                let body = if req.contains("summary") {
                    SUMMARY
                } else if req.contains("pools") {
                    POOLS
                } else if req.contains("stats") {
                    STATS
                } else {
                    r#"{"STATUS":[{"STATUS":"E","Msg":"unknown"}]}"#
                };
                // bmminer appends a trailing NUL — include one to exercise trimming.
                let _ = s.write_all(body.as_bytes());
                let _ = s.write_all(&[0u8]);
            }
        });
        addr
    }

    #[test]
    fn parses_summary_pools_stats_from_mock() {
        let addr = spawn_mock();
        let info = query(&addr, Duration::from_secs(2)).unwrap();
        assert_eq!(info.kind, "bmminer 1.0.0");
        assert!((info.ghs - 95000.42).abs() < 1e-3); // GHS av normalized as GH/s
        assert_eq!(info.accepted, 12000);
        assert_eq!(info.rejected, 37);
        assert_eq!(info.hw_errors, 5);
        assert_eq!(info.uptime_s, 86400);
        // hottest board temp + fastest fan from stats
        assert_eq!(info.temp_c as u64, 71);
        assert_eq!(info.fan_rpm, 4380);
        // pools: active one is index 0
        assert_eq!(info.pools.len(), 2);
        assert!(info.active_pool().unwrap().active);
        assert_eq!(info.active_pool().unwrap().url, "stratum+tcp://btc.pool.example:3333");
        assert!(info.reject_pct() < 1.0);
    }

    #[test]
    fn mhs_hashrate_is_normalized_to_ghs() {
        let mut info = AsicInfo::default();
        let v: Value = serde_json::from_str(
            r#"{"SUMMARY":[{"MHS av":13500.0,"Accepted":1,"Rejected":0}]}"#,
        )
        .unwrap();
        apply_summary(&mut info, &v);
        assert!((info.ghs - 13.5).abs() < 1e-6); // 13500 MH/s = 13.5 GH/s
    }

    #[test]
    fn cidr_slash24_expands_to_254_hosts() {
        let hosts = expand_targets(&["10.0.0.0/24".to_string()]);
        assert_eq!(hosts.len(), 254);
        assert_eq!(hosts[0], format!("10.0.0.1:{DEFAULT_API_PORT}"));
        assert_eq!(hosts[253], format!("10.0.0.254:{DEFAULT_API_PORT}"));
    }
}
