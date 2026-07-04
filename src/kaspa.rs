//! Kaspa (kHeavyHash) mining — the `EthereumStratum/1.0.0` pool protocol + a
//! u64-nonce mining engine over [`crate::pow::kaspa_pow_hash`].
//!
//! Kaspa pools do **not** speak Bitcoin-family stratum (no coinbase/merkle, a
//! 64-bit nonce, a `set_extranonce` prefix, and a `prePowHash`-based job). This
//! module implements that dialect and drives the exact Kaspa PoW.
//!
//! **Protocol source of truth:** this implements the framing used by
//! `onemorebsmith/kaspa-stratum-bridge` (the bridge the large majority of Kaspa
//! pools run). Verified against its source:
//!   - `mining.subscribe` → result `[true, "EthereumStratum/1.0.0"]`.
//!   - `mining.set_difficulty` params `[diff]` (a float, ≥ 1 in practice).
//!   - `set_extranonce` params `[extranonceHex]` (no `mining.` prefix) — the
//!     extranonce occupies the HIGH hex digits of the 16-hex (u64) nonce.
//!   - `mining.notify`, DEFAULT form: `[jobId, [w0,w1,w2,w3], timestamp]` where the
//!     four u64 words are the little-endian lanes of the 32-byte prePowHash and
//!     `timestamp` is a separate number (ms). BIG-JOB form (BzMiner): `[jobId,
//!     "<80 hex>"]` = prePowHash(32B) hex ++ byteswapped-timestamp(8B).
//!   - `mining.submit` params `[wallet.worker, jobId, nonceHex]`, nonce = the full
//!     u64 as 16 hex chars (bridge parses it directly).
//!   - **share target = floor((2^224 − 1) / diff)** — the bridge's `DiffToTarget`
//!     (maxTarget = 2^224−1). This is the single most common reject cause if wrong.
//!   - accept test: PoW value (heavyhash as a little-endian u256) ≤ target.
//! The cSHAKE256 primitive is NIST-KAT-verified and kHeavyHash follows rusty-kaspa
//! consensus. Still EXPERIMENTAL until you confirm accepted shares on your pool —
//! diff scaling and prePowHash lane order are the knobs if a specific pool differs.

use crate::engine::SessionShared;
use crate::pow::{kaspa_matrix, kaspa_pow_hash};
use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Write};
use std::net::TcpStream;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const CHUNK: u64 = 4096; // nonces per worker claim (kHeavyHash is heavy)

/// A resolved Kaspa job: the pre-pow hash + its matrix + timestamp + target.
#[derive(Clone)]
struct KaspaJob {
    job_id: String,
    pre_pow_hash: [u8; 32],
    matrix: Arc<[[u16; 64]; 64]>,
    timestamp: u64,
    target: [u8; 32], // big-endian numeric threshold
    extranonce: u64,
    extranonce_bits: u32,
}

struct FoundNonce {
    job_id: String,
    nonce: u64,
}

/// Live Kaspa mining under the disclosed 1% developer time-slice: mines to the
/// operator's login, and for ~1% of the time reconnects under the baked KAS dev address
/// (only when one is present) so the pool credits the disclosed fee. Wraps
/// [`run_session`], which is one connected mining session.
pub fn run(url: &str, user: &str, pass: &str, workers: usize, shared: &SessionShared, deadline: Option<Instant>) -> std::io::Result<()> {
    let r = crate::devfee::time_slice::run_with_fee("KAS", user, shared, deadline, |login, round_dl| {
        run_session(url, login, pass, workers, shared, Some(round_dl))
    });
    shared.connected.store(false, std::sync::atomic::Ordering::SeqCst);
    r
}

/// A full Kaspa mining session: connect, EthereumStratum handshake, spin worker
/// threads searching u64 nonces, submit shares. Runs until `shared.stop` or `deadline`.
pub fn run_session(url: &str, user: &str, pass: &str, workers: usize, shared: &SessionShared, deadline: Option<Instant>) -> std::io::Result<()> {
    let (host, port) = crate::stratum::parse_endpoint(url)
        .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::InvalidInput, "bad kaspa url"))?;
    let addr = std::net::ToSocketAddrs::to_socket_addrs(&(host.as_str(), port))?
        .next()
        .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::NotFound, "no address"))?;
    let stream = TcpStream::connect_timeout(&addr, Duration::from_secs(15))?;
    stream.set_read_timeout(Some(Duration::from_secs(20)))?;
    stream.set_write_timeout(Some(Duration::from_secs(15)))?;
    let mut writer = stream.try_clone()?;
    let mut reader = BufReader::new(stream);

    let send = |w: &mut TcpStream, id: u64, method: &str, params: Value| -> std::io::Result<()> {
        let line = format!("{}\n", json!({"id": id, "method": method, "params": params}));
        w.write_all(line.as_bytes())
    };
    send(&mut writer, 1, "mining.subscribe", json!(["kairos/0.1.0", "EthereumStratum/1.0.0"]))?;
    send(&mut writer, 2, "mining.authorize", json!([user, pass]))?;

    // Shared work + worker plumbing.
    let job: Arc<Mutex<Option<KaspaJob>>> = Arc::new(Mutex::new(None));
    let cursor = Arc::new(AtomicU64::new(0));
    let running = Arc::new(AtomicBool::new(true));
    let hashes = Arc::new(AtomicU64::new(0));
    let (tx, found_rx): (Sender<FoundNonce>, Receiver<FoundNonce>) = std::sync::mpsc::channel();

    let mut handles = Vec::new();
    for _ in 0..workers.max(1) {
        let job = job.clone();
        let cursor = cursor.clone();
        let running = running.clone();
        let hashes = hashes.clone();
        let tx = tx.clone();
        handles.push(std::thread::spawn(move || kaspa_worker(job, cursor, running, hashes, tx)));
    }
    drop(tx);

    shared.connected.store(true, Ordering::SeqCst);
    let mut difficulty = 1.0f64;
    let mut extranonce: u64 = 0;
    let mut extranonce_bits: u32 = 0;
    let started = Instant::now();

    let result: std::io::Result<()> = (|| {
        while !shared.stop.load(Ordering::Relaxed) {
            if let Some(dl) = deadline {
                if Instant::now() >= dl {
                    break;
                }
            }
            // Submit found shares.
            while let Ok(f) = found_rx.try_recv() {
                let nonce_hex = format!("{:016x}", f.nonce);
                let _ = send(&mut writer, 4, "mining.submit", json!([user, f.job_id, nonce_hex]));
                shared.submitted.fetch_add(1, Ordering::Relaxed);
            }
            let rate = hashes.load(Ordering::Relaxed) as f64 / started.elapsed().as_secs_f64().max(1e-6);
            shared.hashrate_mhs.store((rate * 1000.0) as u64, Ordering::Relaxed);

            let mut line = String::new();
            let n = match reader.read_line(&mut line) {
                Ok(n) => n,
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock || e.kind() == std::io::ErrorKind::TimedOut => continue,
                Err(e) => return Err(e),
            };
            if n == 0 {
                return Err(std::io::Error::new(std::io::ErrorKind::ConnectionAborted, "pool closed"));
            }
            let v: Value = match serde_json::from_str(line.trim()) {
                Ok(v) => v,
                Err(_) => continue,
            };
            match v["method"].as_str() {
                Some("mining.set_extranonce") | Some("set_extranonce") => {
                    if let Some(hex) = v["params"][0].as_str() {
                        if let Some((val, bits)) = parse_extranonce(hex) {
                            extranonce = val;
                            extranonce_bits = bits;
                        }
                    }
                }
                Some("mining.set_difficulty") | Some("set_difficulty") => {
                    difficulty = v["params"][0].as_f64().unwrap_or(1.0).max(1e-9);
                }
                Some("mining.notify") => {
                    if let Some(j) = parse_notify(&v["params"], difficulty, extranonce, extranonce_bits) {
                        *job.lock().unwrap() = Some(j);
                        cursor.store(0, Ordering::SeqCst);
                    }
                }
                _ => {
                    // Submit/authorize results.
                    if v["id"].as_u64() == Some(4) {
                        if v["result"].as_bool().unwrap_or(false) {
                            shared.accepted.fetch_add(1, Ordering::Relaxed);
                        } else {
                            shared.rejected.fetch_add(1, Ordering::Relaxed);
                        }
                    }
                }
            }
        }
        Ok(())
    })();

    running.store(false, Ordering::SeqCst);
    for h in handles {
        let _ = h.join();
    }
    shared.connected.store(false, Ordering::SeqCst);
    result
}

fn kaspa_worker(
    job: Arc<Mutex<Option<KaspaJob>>>,
    cursor: Arc<AtomicU64>,
    running: Arc<AtomicBool>,
    hashes: Arc<AtomicU64>,
    tx: Sender<FoundNonce>,
) {
    while running.load(Ordering::Relaxed) {
        let j = { job.lock().unwrap().clone() };
        let j = match j {
            Some(j) => j,
            None => {
                std::thread::sleep(Duration::from_millis(30));
                continue;
            }
        };
        let start = cursor.fetch_add(CHUNK, Ordering::SeqCst);
        let low_mask: u64 = if j.extranonce_bits >= 64 { 0 } else { (1u64 << (64 - j.extranonce_bits)) - 1 };
        let high = if j.extranonce_bits == 0 { 0 } else { j.extranonce << (64 - j.extranonce_bits) };
        for i in 0..CHUNK {
            let nonce = high | ((start + i) & low_mask);
            let h = kaspa_pow_hash(&j.pre_pow_hash, j.timestamp, nonce, &j.matrix);
            if le_meets_target(&h, &j.target) {
                let _ = tx.send(FoundNonce { job_id: j.job_id.clone(), nonce });
            }
        }
        hashes.fetch_add(CHUNK, Ordering::Relaxed);
    }
}

/// The Kaspa hash is compared as a little-endian integer against the target.
fn le_meets_target(hash_le: &[u8; 32], target_be: &[u8; 32]) -> bool {
    let mut be = *hash_le;
    be.reverse();
    &be <= target_be // lexicographic == numeric big-endian
}

fn parse_extranonce(hex: &str) -> Option<(u64, u32)> {
    let bytes = crate::stratum::from_hex(hex)?;
    if bytes.is_empty() || bytes.len() > 8 {
        return None;
    }
    let mut val = 0u64;
    for b in &bytes {
        val = (val << 8) | *b as u64;
    }
    Some((val, (bytes.len() * 8) as u32))
}

/// Parse `mining.notify` params → a resolved job. Handles both the hex-string and
/// the `[u64; 4]` pre-pow-hash encodings pools use.
fn parse_notify(params: &Value, difficulty: f64, extranonce: u64, extranonce_bits: u32) -> Option<KaspaJob> {
    let arr = params.as_array()?;
    let job_id = arr.first()?.as_str()?.to_string();
    let pre = &arr[1];
    let mut pre_pow_hash = [0u8; 32];
    let mut timestamp = arr.get(2).and_then(|x| x.as_u64()).unwrap_or(0);
    if let Some(hexs) = pre.as_str() {
        let b = crate::stratum::from_hex(hexs)?;
        match b.len() {
            // Plain prePowHash hex; timestamp is the separate 3rd param.
            32 => pre_pow_hash.copy_from_slice(&b),
            // BIG-JOB form: prePowHash(32B) ++ timestamp(8B). The bridge writes the
            // timestamp byteswapped, so from_le_bytes here recovers the real ms.
            40 => {
                pre_pow_hash.copy_from_slice(&b[0..32]);
                let mut ts = [0u8; 8];
                ts.copy_from_slice(&b[32..40]);
                timestamp = u64::from_le_bytes(ts);
            }
            _ => return None,
        }
    } else if let Some(words) = pre.as_array() {
        // DEFAULT form: 4 × u64, little-endian lanes of the 32-byte prePowHash.
        if words.len() != 4 {
            return None;
        }
        for (i, w) in words.iter().enumerate() {
            let u = w.as_u64()?;
            pre_pow_hash[i * 8..i * 8 + 8].copy_from_slice(&u.to_le_bytes());
        }
    } else {
        return None;
    }
    Some(KaspaJob {
        job_id,
        pre_pow_hash,
        matrix: Arc::new(kaspa_matrix(&pre_pow_hash)),
        timestamp,
        target: difficulty_to_target(difficulty),
        extranonce,
        extranonce_bits,
    })
}

/// Kaspa share target = floor((2^224 − 1) / difficulty), big-endian — matches the
/// bridge's `DiffToTarget` (maxTarget = 2^224 − 1, i.e. diff-1 target = 2^224 − 1,
/// which is 2^256/2^32). Getting the 2^32 factor wrong here makes a pool reject
/// every otherwise-valid share, so this is the first knob to check.
///
/// Difficulty is a float; we divide with a fixed-point scale so fractional
/// (vardiff) difficulties are handled, not just integers.
fn difficulty_to_target(difficulty: f64) -> [u8; 32] {
    // maxTarget = 2^224 − 1 as [u64;4] big-endian (top 32 bits zero).
    let max_target: [u64; 4] = [0x0000_0000_FFFF_FFFF, u64::MAX, u64::MAX, u64::MAX];
    const SCALE_BITS: u32 = 12; // 4096 fixed-point steps on the difficulty
    let divisor = ((difficulty.max(1e-9)) * (1u64 << SCALE_BITS) as f64)
        .round()
        .max(1.0) as u64;
    // numerator = maxTarget << SCALE_BITS (safe: maxTarget's top 32 bits are zero).
    let mut words = [0u64; 4];
    for i in 0..4 {
        let hi = max_target[i] << SCALE_BITS;
        let carry = if i + 1 < 4 {
            max_target[i + 1] >> (64 - SCALE_BITS)
        } else {
            0
        };
        words[i] = hi | carry;
    }
    // long division of the 256-bit numerator by the u64 divisor.
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

/// The result of a live handshake against a Kaspa pool — what KAIROS parsed from
/// the pool, so an operator can confirm it reads their pool correctly *before*
/// committing hashpower. Purely diagnostic: no shares are submitted.
pub struct KaspaProbe {
    pub subscribe_ok: bool,
    pub subscribe_result: String,
    pub authorize_ok: Option<bool>,
    pub difficulty: Option<f64>,
    pub extranonce_hex: Option<String>,
    pub extranonce_bits: u32,
    pub job_id: Option<String>,
    pub pre_pow_hash_hex: Option<String>,
    pub timestamp: Option<u64>,
    pub target_hex: Option<String>,
    pub notify_form: Option<String>,
    pub lines_seen: usize,
}

/// Connect, do the EthereumStratum handshake, and capture the first job (up to
/// `timeout`). Submits nothing — this is the "does KAIROS understand my pool?" check
/// behind `kairos kaspa-verify`.
pub fn verify(url: &str, user: &str, pass: &str, timeout: Duration) -> std::io::Result<KaspaProbe> {
    let (host, port) = crate::stratum::parse_endpoint(url)
        .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::InvalidInput, "bad kaspa url"))?;
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

    let mut probe = KaspaProbe {
        subscribe_ok: false,
        subscribe_result: String::new(),
        authorize_ok: None,
        difficulty: None,
        extranonce_hex: None,
        extranonce_bits: 0,
        job_id: None,
        pre_pow_hash_hex: None,
        timestamp: None,
        target_hex: None,
        notify_form: None,
        lines_seen: 0,
    };
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline && probe.job_id.is_none() {
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
        probe.lines_seen += 1;
        match v["method"].as_str() {
            Some("mining.set_extranonce") | Some("set_extranonce") => {
                if let Some(hex) = v["params"][0].as_str() {
                    if let Some((_, bits)) = parse_extranonce(hex) {
                        probe.extranonce_hex = Some(hex.to_string());
                        probe.extranonce_bits = bits;
                    }
                }
            }
            Some("mining.set_difficulty") | Some("set_difficulty") => {
                probe.difficulty = v["params"][0].as_f64();
            }
            Some("mining.notify") => {
                probe.notify_form = Some(if v["params"][1].is_array() {
                    "default (4×u64 lanes + timestamp)".into()
                } else {
                    "big-job (hex string)".into()
                });
                let diff = probe.difficulty.unwrap_or(1.0);
                if let Some(j) = parse_notify(&v["params"], diff, 0, 0) {
                    probe.job_id = Some(j.job_id);
                    probe.pre_pow_hash_hex = Some(crate::stratum::to_hex(&j.pre_pow_hash));
                    probe.timestamp = Some(j.timestamp);
                    probe.target_hex = Some(crate::stratum::to_hex(&j.target));
                }
            }
            _ => {
                if v["id"].as_u64() == Some(1) {
                    probe.subscribe_ok = v["error"].is_null();
                    probe.subscribe_result = v["result"].to_string();
                } else if v["id"].as_u64() == Some(2) {
                    probe.authorize_ok = v["result"].as_bool();
                }
            }
        }
    }
    Ok(probe)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extranonce_parse() {
        assert_eq!(parse_extranonce("4723"), Some((0x4723, 16)));
        assert_eq!(parse_extranonce("ab"), Some((0xab, 8)));
    }

    #[test]
    fn target_is_2pow224_over_diff() {
        // diff 1 → maxTarget = 2^224 − 1: top 4 bytes zero, remaining 28 bytes 0xff.
        let t1 = difficulty_to_target(1.0);
        assert_eq!(&t1[0..4], &[0, 0, 0, 0]);
        assert!(t1[4..].iter().all(|&b| b == 0xff));
        // diff 2 → half: the most-significant non-zero byte is ~0x7f.
        let t2 = difficulty_to_target(2.0);
        assert_eq!(&t2[0..4], &[0, 0, 0, 0]);
        assert_eq!(t2[4], 0x7f);
        // larger diff → strictly smaller target (harder).
        assert!(t2 < t1);
        assert!(difficulty_to_target(1000.0) < t2);
    }

    #[test]
    fn notify_default_array_form() {
        // DEFAULT bridge form: [jobId, [4×u64 LE lanes], timestamp].
        let pre = [0x11u8; 32];
        let words: Vec<u64> = (0..4).map(|i| u64::from_le_bytes(pre[i * 8..i * 8 + 8].try_into().unwrap())).collect();
        let jh = parse_notify(&serde_json::json!(["7", words, 1717171717000u64]), 1.0, 0, 0).unwrap();
        assert_eq!(jh.pre_pow_hash, pre);
        assert_eq!(jh.timestamp, 1717171717000);
        assert_eq!(jh.job_id, "7");
    }

    #[test]
    fn notify_bigjob_hex_form() {
        // BIG-JOB form: 80-hex string = prePowHash(32B) ++ byteswapped-timestamp(8B).
        let pre = [0x22u8; 32];
        let ts: u64 = 1717171717000;
        let mut buf = pre.to_vec();
        buf.extend_from_slice(&ts.swap_bytes().to_be_bytes()); // bridge writes it byteswapped
        let hexs = crate::stratum::to_hex(&buf);
        let jh = parse_notify(&serde_json::json!(["9", hexs]), 1.0, 0, 0).unwrap();
        assert_eq!(jh.pre_pow_hash, pre);
        assert_eq!(jh.timestamp, ts);
    }
}
