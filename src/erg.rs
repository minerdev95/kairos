//! Ergo (ERG) stratum — the Autolykos2 pool protocol on top of the KAT-verified
//! [`crate::autolykos`] core. Ergo pools broadly follow a bitcoin-derived stratum:
//!   - `mining.subscribe` → `[[["mining.set_difficulty",id],["mining.notify",id]],
//!      extraNonce1, extraNonce2Size]` (extraNonce1 = the HIGH bytes of the 8-byte nonce).
//!   - `mining.set_difficulty [d]`.
//!   - `mining.notify [jobId, height, msg, …, b?, …, cleanJobs]` — `msg` is the
//!     32-byte header message, `height` sizes the table (`calc_n`), and some pools
//!     also send the raw target `b` (= q / difficulty).
//!   - `mining.submit [worker, jobId, nonceHex]`.
//! Wire details vary by pool, so [`verify`] dumps the RAW notify params alongside the
//! parsed view — the live probe is the source of truth (`kairos erg-verify`).

use crate::autolykos;
use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Write};
use std::net::TcpStream;
use std::time::{Duration, Instant};

/// secp256k1 group order `q` — Autolykos targets are `b = q / difficulty`.
pub const Q_HEX: &str = "fffffffffffffffffffffffffffffffebaaedce6af48a03bbfd25e8cd0364141";

/// Compute the 32-byte big-endian target `b = q / difficulty` for a stratum diff.
pub fn difficulty_to_target(diff: f64) -> [u8; 32] {
    let q = crate::stratum::from_hex(Q_HEX).unwrap();
    // q as [u64;4] big-endian
    let mut qw = [0u64; 4];
    for i in 0..4 {
        qw[i] = u64::from_be_bytes(q[i * 8..i * 8 + 8].try_into().unwrap());
    }
    // q's top bit is set, so it can't be left-shifted for fractional scaling without
    // overflow; divide by the rounded integer difficulty (exact for diff ≥ 1, which is
    // the norm for ERG pools; a diagnostic display only).
    let d = diff.max(1e-9).round().max(1.0) as u64;
    let mut words = qw;
    let mut rem: u128 = 0;
    for w in words.iter_mut() {
        let cur = (rem << 64) | *w as u128;
        *w = (cur / d as u128) as u64;
        rem = cur % d as u128;
    }
    let mut out = [0u8; 32];
    for i in 0..4 {
        out[i * 8..i * 8 + 8].copy_from_slice(&words[i].to_be_bytes());
    }
    out
}

/// Convert a decimal bigint string (the pool's Autolykos target `b`) to a 32-byte
/// big-endian array.
pub fn decimal_to_be32(s: &str) -> Option<[u8; 32]> {
    let mut acc = [0u8; 32]; // big-endian accumulator
    for ch in s.bytes() {
        if !ch.is_ascii_digit() {
            return None;
        }
        let digit = (ch - b'0') as u16;
        // acc = acc * 10 + digit
        let mut carry = digit;
        for byte in acc.iter_mut().rev() {
            let v = *byte as u16 * 10 + carry;
            *byte = (v & 0xff) as u8;
            carry = v >> 8;
        }
        if carry != 0 {
            return None; // overflow > 2^256
        }
    }
    Some(acc)
}

/// A parsed ERG job from a live pool (diagnostic).
pub struct ErgProbe {
    pub subscribe_ok: bool,
    pub subscribe_result: String,
    pub extranonce_hex: Option<String>,
    pub authorize_ok: Option<bool>,
    pub difficulty: Option<f64>,
    pub job_id: Option<String>,
    pub height: Option<u32>,
    pub msg_hex: Option<String>,
    pub table_n: Option<u64>,
    pub target_hex: Option<String>,
    pub raw_notify: Option<String>,
    pub lines_seen: usize,
}

/// Connect, do the Autolykos2 stratum handshake, and capture the first job. Submits
/// nothing — the "does KAIROS understand my ERG pool?" check.
pub fn verify(url: &str, user: &str, pass: &str, timeout: Duration) -> std::io::Result<ErgProbe> {
    let (host, port) = crate::stratum::parse_endpoint(url)
        .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::InvalidInput, "bad erg url"))?;
    let addr = std::net::ToSocketAddrs::to_socket_addrs(&(host.as_str(), port))?
        .next()
        .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::NotFound, "no address"))?;
    let stream = TcpStream::connect_timeout(&addr, Duration::from_secs(15))?;
    stream.set_read_timeout(Some(Duration::from_millis(500)))?;
    let mut writer = stream.try_clone()?;
    let mut reader = BufReader::new(stream);

    let mut send = |id: u64, method: &str, params: Value| -> std::io::Result<()> {
        let line = format!("{}\n", json!({"id": id, "method": method, "params": params}));
        writer.write_all(line.as_bytes())
    };
    send(1, "mining.subscribe", json!(["kairos/0.1.0"]))?;
    send(2, "mining.authorize", json!([user, pass]))?;

    let mut p = ErgProbe {
        subscribe_ok: false,
        subscribe_result: String::new(),
        extranonce_hex: None,
        authorize_ok: None,
        difficulty: None,
        job_id: None,
        height: None,
        msg_hex: None,
        table_n: None,
        target_hex: None,
        raw_notify: None,
        lines_seen: 0,
    };
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline && p.job_id.is_none() {
        let mut line = String::new();
        match reader.read_line(&mut line) {
            Ok(0) => break,
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock || e.kind() == std::io::ErrorKind::TimedOut => continue,
            Err(e) => return Err(e),
        }
        let v: Value = match serde_json::from_str(line.trim()) {
            Ok(v) => v,
            Err(_) => continue,
        };
        p.lines_seen += 1;
        match v["method"].as_str() {
            Some("mining.set_difficulty") => {
                p.difficulty = v["params"][0].as_f64();
            }
            Some("mining.set_extranonce") => {
                p.extranonce_hex = v["params"][0].as_str().map(|s| s.to_string());
            }
            Some("mining.notify") => {
                let pr = &v["params"];
                p.raw_notify = Some(pr.to_string());
                if let Some(arr) = pr.as_array() {
                    p.job_id = arr.first().and_then(|x| x.as_str()).map(|s| s.to_string());
                    // height is the first numeric-ish param after jobId
                    for x in arr.iter().skip(1) {
                        if let Some(h) = x.as_u64() {
                            p.height = Some(h as u32);
                            break;
                        }
                        if let Some(s) = x.as_str() {
                            if let Ok(h) = s.parse::<u32>() {
                                p.height = Some(h);
                                break;
                            }
                        }
                    }
                    // msg = the 32-byte (64-hex) param
                    for x in arr.iter() {
                        if let Some(s) = x.as_str() {
                            if s.len() == 64 && crate::stratum::from_hex(s).is_some() {
                                p.msg_hex = Some(s.to_string());
                                break;
                            }
                        }
                    }
                }
                if let Some(h) = p.height {
                    p.table_n = Some(autolykos::calc_n(h));
                }
                // Prefer the pool's explicit target `b` (a long decimal bigint param);
                // fall back to deriving it from set_difficulty.
                if let Some(arr) = pr.as_array() {
                    for x in arr.iter() {
                        if let Some(s) = x.as_str() {
                            if s.len() >= 20 && s.bytes().all(|c| c.is_ascii_digit()) {
                                if let Some(t) = decimal_to_be32(s) {
                                    p.target_hex = Some(crate::stratum::to_hex(&t));
                                }
                            }
                        }
                    }
                }
                if p.target_hex.is_none() {
                    if let Some(d) = p.difficulty {
                        p.target_hex = Some(crate::stratum::to_hex(&difficulty_to_target(d)));
                    }
                }
            }
            _ => {
                if v["id"].as_u64() == Some(1) {
                    p.subscribe_ok = v["error"].is_null();
                    p.subscribe_result = v["result"].to_string();
                    // extraNonce1 is commonly result[1] (bitcoin-style subscribe).
                    if let Some(x) = v["result"][1].as_str() {
                        p.extranonce_hex = Some(x.to_string());
                    }
                } else if v["id"].as_u64() == Some(2) {
                    p.authorize_ok = v["result"].as_bool();
                }
            }
        }
    }
    Ok(p)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn target_scales_with_difficulty() {
        // diff 1 → target ≈ q; larger diff → smaller target.
        let t1 = difficulty_to_target(1.0);
        let t2 = difficulty_to_target(2.0);
        assert!(t2 < t1);
        assert!(difficulty_to_target(1000.0) < t2);
    }
}
