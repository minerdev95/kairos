//! Ethereum Classic (ETC / Etchash) mining — the **EthereumStratum/1.0.0** (NiceHash)
//! pool protocol + job parsing, on top of the KAT-verified Ethash core in
//! [`crate::ethash`].
//!
//! Protocol (per the NiceHash EthereumStratum/1.0.0 spec):
//!   - `mining.subscribe` → result `[[ "mining.notify", <subId>, "EthereumStratum/1.0.0"], <extranonceHex>]`.
//!     The extranonce (≤ 3 bytes) is the HIGH bytes of the 64-bit nonce.
//!   - `mining.set_difficulty [d]` — difficulty as a double; **diff-1 target =
//!     0xFFFF·2^208** (Bitcoin bdiff), so share target = floor(diff1 / d).
//!   - `mining.notify [jobId, seedHash, headerHash, cleanJobs]`.
//!   - `mining.submit [user, jobId, minerNonceHex]` — miner sends only its own
//!     nonce bytes; the pool prepends the extranonce.
//!
//! A share is valid when `hashimoto(headerHash, fullNonce).result` (big-endian
//! 256-bit) ≤ target. The DAG needed to compute that only fits/streams on a GPU, so
//! actual mining lives in the GPU path; this module handles the pool protocol and is
//! **live-verifiable** with `kairos etc-verify <url> <wallet>`.

use crate::ethash;
use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Write};
use std::net::TcpStream;
use std::time::{Duration, Instant};

/// The share target for a stratum difficulty: floor((0xFFFF·2^208) / difficulty),
/// big-endian. Handles fractional (vardiff) difficulties via a fixed-point divide.
pub fn difficulty_to_target(diff: f64) -> [u8; 32] {
    // diff-1 target = 0x00000000FFFF0000…0000 (0xFFFF shifted left by 208 bits).
    let diff1: [u64; 4] = [0x0000_0000_FFFF_0000, 0, 0, 0];
    const SCALE_BITS: u32 = 20;
    let divisor = (diff.max(1e-12) * (1u64 << SCALE_BITS) as f64).round().max(1.0) as u64;
    // numerator = diff1 << SCALE_BITS (safe: diff1's top 32 bits are zero)
    let mut words = [0u64; 4];
    for i in 0..4 {
        let hi = diff1[i] << SCALE_BITS;
        let carry = if i + 1 < 4 { diff1[i + 1] >> (64 - SCALE_BITS) } else { 0 };
        words[i] = hi | carry;
    }
    let mut rem: u128 = 0;
    for w in words.iter_mut() {
        let cur = (rem << 64) | *w as u128;
        *w = (cur / divisor as u128) as u64;
        rem = cur % divisor as u128;
    }
    let mut out = [0u8; 32];
    for i in 0..4 {
        out[i * 8..i * 8 + 8].copy_from_slice(&words[i].to_be_bytes());
    }
    out
}

/// Find the Ethash/Etchash epoch whose seedhash matches `seed` (searching a
/// reasonable range). Returns `None` if not found.
pub fn epoch_from_seedhash(seed: &[u8; 32]) -> Option<u64> {
    let mut s = [0u8; 32];
    for epoch in 0..2048u64 {
        if &s == seed {
            return Some(epoch);
        }
        s = crate::pow::keccak256(&s);
    }
    None
}

/// A parsed ETC job from a live pool — diagnostic output for `etc-verify`.
pub struct EtcProbe {
    pub subscribe_ok: bool,
    pub subscribe_result: String,
    pub extranonce_hex: Option<String>,
    pub authorize_ok: Option<bool>,
    pub difficulty: Option<f64>,
    pub job_id: Option<String>,
    pub seed_hash_hex: Option<String>,
    pub header_hash_hex: Option<String>,
    pub epoch: Option<u64>,
    pub dag_bytes: Option<u64>,
    pub target_hex: Option<String>,
    pub lines_seen: usize,
}

/// Connect, do the EthereumStratum/1.0.0 handshake, and capture the first job.
/// Submits nothing — the "does KAIROS understand my ETC pool?" check.
pub fn verify(url: &str, user: &str, pass: &str, timeout: Duration) -> std::io::Result<EtcProbe> {
    let (host, port) = crate::stratum::parse_endpoint(url)
        .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::InvalidInput, "bad etc url"))?;
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
    send(1, "mining.subscribe", json!(["kairos/0.1.0", "EthereumStratum/1.0.0"]))?;
    send(2, "mining.authorize", json!([user, pass]))?;

    let mut p = EtcProbe {
        subscribe_ok: false,
        subscribe_result: String::new(),
        extranonce_hex: None,
        authorize_ok: None,
        difficulty: None,
        job_id: None,
        seed_hash_hex: None,
        header_hash_hex: None,
        epoch: None,
        dag_bytes: None,
        target_hex: None,
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
                p.job_id = pr[0].as_str().map(|s| s.to_string());
                if let Some(seed) = pr[1].as_str() {
                    p.seed_hash_hex = Some(seed.to_string());
                    if let Some(b) = crate::stratum::from_hex(seed) {
                        if b.len() == 32 {
                            let mut s = [0u8; 32];
                            s.copy_from_slice(&b);
                            if let Some(ep) = epoch_from_seedhash(&s) {
                                p.epoch = Some(ep);
                                p.dag_bytes = Some(ethash::dataset_size(ep));
                            }
                        }
                    }
                }
                p.header_hash_hex = pr[2].as_str().map(|s| s.to_string());
                let d = p.difficulty.unwrap_or(1.0);
                p.target_hex = Some(crate::stratum::to_hex(&difficulty_to_target(d)));
            }
            _ => {
                if v["id"].as_u64() == Some(1) {
                    p.subscribe_ok = v["error"].is_null();
                    p.subscribe_result = v["result"].to_string();
                    // extranonce = result[1] per the spec.
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
    fn diff1_target_is_bitcoin_bdiff() {
        // difficulty 1 → 0x00000000FFFF0000…0000
        let t = difficulty_to_target(1.0);
        assert_eq!(&t[0..8], &[0x00, 0x00, 0x00, 0x00, 0xFF, 0xFF, 0x00, 0x00]);
        assert!(t[8..].iter().all(|&b| b == 0));
        // harder difficulty → smaller target
        assert!(difficulty_to_target(2.0) < t);
        // fractional vardiff → larger target than diff 1
        assert!(difficulty_to_target(0.5) > t);
    }

    #[test]
    fn seedhash_epoch_roundtrip() {
        let s0 = [0u8; 32];
        assert_eq!(epoch_from_seedhash(&s0), Some(0));
        let s1 = crate::pow::keccak256(&[0u8; 32]);
        assert_eq!(epoch_from_seedhash(&s1), Some(1));
    }
}
