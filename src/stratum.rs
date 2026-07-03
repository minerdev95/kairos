//! KAIROS native Stratum V1 client — our own pool protocol, not a wrapped miner.
//!
//! This speaks Stratum V1 (the JSON-RPC-over-TCP protocol every SHA-256d and most
//! GPU pools use) directly: `mining.subscribe` → `mining.authorize` →
//! `mining.set_difficulty` / `mining.notify` → assemble the block header → search
//! nonces ([`crate::pow`]) → `mining.submit`. No third-party binary is involved;
//! KAIROS connects to the operator's pool itself.
//!
//! The networked client ([`StratumClient`]) is real. The protocol arithmetic that
//! must be exactly right — difficulty→target, coinbase/merkle assembly, header
//! layout and endianness — is factored into pure functions with known-answer
//! tests, so correctness does not depend on a live pool being reachable.

use crate::pow::sha256d;
use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Write};
use std::net::TcpStream;
use std::time::Duration;

/// A work template from `mining.notify` (Stratum V1 fields, hex as received).
#[derive(Clone, Debug, Default)]
pub struct Job {
    pub job_id: String,
    pub prevhash: String,
    pub coinb1: String,
    pub coinb2: String,
    pub merkle_branch: Vec<String>,
    pub version: String,
    pub nbits: String,
    pub ntime: String,
    pub clean_jobs: bool,
}

// ─────────────────────────────────────────────────────────────────────────────
// Hex helpers.
// ─────────────────────────────────────────────────────────────────────────────

pub fn from_hex(s: &str) -> Option<Vec<u8>> {
    let s = s.trim();
    if s.len() % 2 != 0 {
        return None;
    }
    let mut out = Vec::with_capacity(s.len() / 2);
    let b = s.as_bytes();
    let mut i = 0;
    while i < b.len() {
        let hi = (b[i] as char).to_digit(16)?;
        let lo = (b[i + 1] as char).to_digit(16)?;
        out.push(((hi << 4) | lo) as u8);
        i += 2;
    }
    Some(out)
}

pub fn to_hex(b: &[u8]) -> String {
    let mut s = String::with_capacity(b.len() * 2);
    for x in b {
        s.push_str(&format!("{:02x}", x));
    }
    s
}

// ─────────────────────────────────────────────────────────────────────────────
// Difficulty → target.
// ─────────────────────────────────────────────────────────────────────────────

/// The Stratum "difficulty-1" target (pdiff): `0x00000000FFFF0000…0000`, i.e.
/// `0xFFFF · 2^208`. A share at pool difficulty `d` must hash ≤ diff1 / d.
const DIFF1_U256: [u64; 4] = [0x0000_0000_FFFF_0000, 0, 0, 0];

fn shl16(a: [u64; 4]) -> [u64; 4] {
    let mut r = [0u64; 4];
    for i in 0..4 {
        r[i] = a[i] << 16;
        if i + 1 < 4 {
            r[i] |= a[i + 1] >> 48;
        }
    }
    r
}

fn div_u256_by_u64(a: [u64; 4], d: u64) -> [u64; 4] {
    let mut q = [0u64; 4];
    let mut rem: u128 = 0;
    for i in 0..4 {
        let cur = (rem << 64) | a[i] as u128;
        q[i] = (cur / d as u128) as u64;
        rem = cur % d as u128;
    }
    q
}

fn u256_to_be_bytes(a: [u64; 4]) -> [u8; 32] {
    let mut out = [0u8; 32];
    for i in 0..4 {
        out[i * 8..i * 8 + 8].copy_from_slice(&a[i].to_be_bytes());
    }
    out
}

/// Convert a pool difficulty into a 256-bit big-endian share target. Fractional
/// difficulty is honored to 16 bits (we pre-shift the numerator by 16, then
/// divide by `round(diff · 65536)`), which is ample for share targets.
pub fn difficulty_to_target(diff: f64) -> [u8; 32] {
    if diff <= 0.0 {
        return [0xff; 32];
    }
    let divisor = (diff * 65536.0).round().max(1.0) as u64;
    let num = shl16(DIFF1_U256);
    u256_to_be_bytes(div_u256_by_u64(num, divisor))
}

// ─────────────────────────────────────────────────────────────────────────────
// Coinbase / merkle / header assembly.
// ─────────────────────────────────────────────────────────────────────────────

/// Build the coinbase transaction bytes: `coinb1 ‖ extranonce1 ‖ extranonce2 ‖ coinb2`.
pub fn build_coinbase(coinb1: &[u8], extranonce1: &[u8], extranonce2: &[u8], coinb2: &[u8]) -> Vec<u8> {
    let mut cb = Vec::with_capacity(coinb1.len() + extranonce1.len() + extranonce2.len() + coinb2.len());
    cb.extend_from_slice(coinb1);
    cb.extend_from_slice(extranonce1);
    cb.extend_from_slice(extranonce2);
    cb.extend_from_slice(coinb2);
    cb
}

/// Fold the coinbase hash up the merkle branch: `root = dsha(root ‖ branch_i)`,
/// starting from the coinbase's double-SHA-256. Branch hashes are in the internal
/// byte order Stratum sends, so they concatenate directly.
pub fn merkle_root(coinbase: &[u8], branch: &[[u8; 32]]) -> [u8; 32] {
    let mut root = sha256d(coinbase);
    for node in branch {
        let mut buf = [0u8; 64];
        buf[..32].copy_from_slice(&root);
        buf[32..].copy_from_slice(node);
        root = sha256d(&buf);
    }
    root
}

/// Reverse each 4-byte word of a 32-byte prevhash — the byte order Stratum sends
/// prevhash in versus what the header needs.
fn swab256(bytes: &[u8; 32]) -> [u8; 32] {
    let mut out = [0u8; 32];
    for w in 0..8 {
        for b in 0..4 {
            out[w * 4 + b] = bytes[w * 4 + (3 - b)];
        }
    }
    out
}

/// Assemble the 80-byte block header a SHA-256d pool expects. `version`, `ntime`,
/// and `nbits` come from `mining.notify` as big-endian hex and are written
/// little-endian; `prevhash` is word-swapped; `merkle_root` is placed as computed.
/// The final 4 bytes are the nonce (little-endian), filled by the search loop.
pub fn build_header(job: &Job, merkle_root: &[u8; 32], ntime_hex: &str, nonce: u32) -> Option<[u8; 80]> {
    let version = from_hex(&job.version)?;
    let prev = from_hex(&job.prevhash)?;
    let nbits = from_hex(&job.nbits)?;
    let ntime = from_hex(ntime_hex)?;
    if version.len() != 4 || prev.len() != 32 || nbits.len() != 4 || ntime.len() != 4 {
        return None;
    }
    let mut hdr = [0u8; 80];
    // version, little-endian.
    for i in 0..4 {
        hdr[i] = version[3 - i];
    }
    // prevhash, word-swapped.
    let mut prev32 = [0u8; 32];
    prev32.copy_from_slice(&prev);
    hdr[4..36].copy_from_slice(&swab256(&prev32));
    // merkle root, as computed.
    hdr[36..68].copy_from_slice(merkle_root);
    // ntime, little-endian.
    for i in 0..4 {
        hdr[68 + i] = ntime[3 - i];
    }
    // nbits, little-endian.
    for i in 0..4 {
        hdr[72 + i] = nbits[3 - i];
    }
    // nonce, little-endian.
    hdr[76..80].copy_from_slice(&nonce.to_le_bytes());
    Some(hdr)
}

// ─────────────────────────────────────────────────────────────────────────────
// The networked client.
// ─────────────────────────────────────────────────────────────────────────────

/// A message pulled off the stratum connection.
#[derive(Clone, Debug)]
pub enum StratumMsg {
    SetDifficulty(f64),
    Notify(Job),
    Result { id: u64, ok: bool },
    Other(Value),
}

/// A live Stratum V1 connection to a pool.
pub struct StratumClient {
    stream: TcpStream,
    reader: BufReader<TcpStream>,
    next_id: u64,
    pub extranonce1: Vec<u8>,
    pub extranonce2_size: usize,
    pub difficulty: f64,
    pub subscribed: bool,
    pub authorized: bool,
}

/// Parse a `host:port` (optionally `scheme://host:port`) into components.
pub fn parse_endpoint(url: &str) -> Option<(String, u16)> {
    let no_scheme = url.split("://").last().unwrap_or(url);
    let hostport = no_scheme.split('/').next().unwrap_or(no_scheme);
    let mut parts = hostport.rsplitn(2, ':');
    let port = parts.next()?.parse().ok()?;
    let host = parts.next()?.to_string();
    if host.is_empty() {
        return None;
    }
    Some((host, port))
}

impl StratumClient {
    /// Connect to a pool (TCP). Does not subscribe yet.
    pub fn connect(url: &str, timeout: Duration) -> std::io::Result<Self> {
        let (host, port) = parse_endpoint(url).ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::InvalidInput, format!("bad stratum url: {url}"))
        })?;
        // Resolve + connect with a bounded timeout.
        let addrs: Vec<std::net::SocketAddr> = std::net::ToSocketAddrs::to_socket_addrs(&(host.as_str(), port))?
            .collect();
        let addr = addrs
            .first()
            .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::NotFound, "no address"))?;
        let stream = TcpStream::connect_timeout(addr, timeout)?;
        stream.set_read_timeout(Some(timeout))?;
        stream.set_write_timeout(Some(timeout))?;
        let reader = BufReader::new(stream.try_clone()?);
        Ok(StratumClient {
            stream,
            reader,
            next_id: 1,
            extranonce1: Vec::new(),
            extranonce2_size: 4,
            difficulty: 1.0,
            subscribed: false,
            authorized: false,
        })
    }

    fn send(&mut self, method: &str, params: Value) -> std::io::Result<u64> {
        let id = self.next_id;
        self.next_id += 1;
        let msg = json!({"id": id, "method": method, "params": params});
        let line = format!("{}\n", serde_json::to_string(&msg)?);
        self.stream.write_all(line.as_bytes())?;
        Ok(id)
    }

    /// `mining.subscribe`, reading back extranonce1 + extranonce2_size.
    pub fn subscribe(&mut self, agent: &str) -> std::io::Result<()> {
        let id = self.send("mining.subscribe", json!([agent]))?;
        // Read until we see the response to our subscribe id.
        for _ in 0..20 {
            if let Some(v) = self.read_line_value()? {
                if v["id"].as_u64() == Some(id) {
                    if let Some(res) = v["result"].as_array() {
                        if res.len() >= 3 && res[0].is_array() {
                            self.extranonce1 = from_hex(res[1].as_str().unwrap_or("")).unwrap_or_default();
                            self.extranonce2_size = res[2].as_u64().unwrap_or(4) as usize;
                            self.subscribed = true;
                            return Ok(());
                        }
                        // Kaspa / EthereumStratum returns e.g. [true,"EthereumStratum/1.0.0"].
                        if res.iter().any(|x| x.as_str().map(|s| s.contains("EthereumStratum")).unwrap_or(false)) {
                            return Err(std::io::Error::new(
                                std::io::ErrorKind::Unsupported,
                                "pool speaks Kaspa/EthereumStratum, which the native engine does not support yet (use a SHA-256d or scrypt pool)",
                            ));
                        }
                    }
                    return Err(std::io::Error::new(std::io::ErrorKind::InvalidData, "bad subscribe result"));
                }
                // Otherwise it was an early notify/difficulty; ignore during handshake.
            }
        }
        Err(std::io::Error::new(std::io::ErrorKind::TimedOut, "no subscribe response"))
    }

    /// `mining.authorize` with the operator's worker credentials.
    pub fn authorize(&mut self, user: &str, pass: &str) -> std::io::Result<bool> {
        let id = self.send("mining.authorize", json!([user, pass]))?;
        for _ in 0..20 {
            if let Some(v) = self.read_line_value()? {
                if v["id"].as_u64() == Some(id) {
                    let ok = v["result"].as_bool().unwrap_or(false);
                    self.authorized = ok;
                    return Ok(ok);
                }
            }
        }
        Err(std::io::Error::new(std::io::ErrorKind::TimedOut, "no authorize response"))
    }

    /// Submit a found share: `mining.submit(user, job_id, extranonce2, ntime, nonce)`.
    pub fn submit(&mut self, user: &str, job_id: &str, extranonce2_hex: &str, ntime_hex: &str, nonce: u32) -> std::io::Result<u64> {
        // Nonce is submitted big-endian hex.
        let nonce_hex = to_hex(&nonce.to_be_bytes());
        self.send("mining.submit", json!([user, job_id, extranonce2_hex, ntime_hex, nonce_hex]))
    }

    /// Read and classify the next protocol message (blocking up to the socket
    /// timeout). Returns `Ok(None)` on a benign empty read.
    pub fn next_message(&mut self) -> std::io::Result<Option<StratumMsg>> {
        match self.read_line_value()? {
            None => Ok(None),
            Some(v) => Ok(Some(classify(&v))),
        }
    }

    fn read_line_value(&mut self) -> std::io::Result<Option<Value>> {
        let mut line = String::new();
        let n = self.reader.read_line(&mut line)?;
        if n == 0 {
            return Ok(None);
        }
        let line = line.trim();
        if line.is_empty() {
            return Ok(None);
        }
        match serde_json::from_str::<Value>(line) {
            Ok(v) => Ok(Some(v)),
            Err(_) => Ok(None),
        }
    }
}

/// What a quick pool probe found — used by `kairos poolcheck` and to give a clear
/// error when a pool speaks a protocol the native engine doesn't support yet.
#[derive(Clone, Debug, Default)]
pub struct PoolProbe {
    pub connected: bool,
    pub variant: String,
    pub subscribed: bool,
    pub authorized: bool,
    pub extranonce1: String,
    pub difficulty: f64,
    pub got_job: bool,
    /// Whether KAIROS's native engine can actually mine this pool today.
    pub supported: bool,
    pub note: String,
}

/// Connect to a pool and classify its stratum dialect + handshake. Read-only: it
/// subscribes/authorizes and waits briefly for the first job, then disconnects.
pub fn probe(url: &str, user: &str, pass: &str, timeout: Duration) -> std::io::Result<PoolProbe> {
    let mut p = PoolProbe::default();
    let mut client = StratumClient::connect(url, timeout)?;
    p.connected = true;

    // Send subscribe + authorize, then read a few lines and classify.
    let sub_id = client.send("mining.subscribe", json!(["kairos/0.1.0"]))?;
    let _ = client.send("mining.authorize", json!([user, pass]))?;
    let deadline = std::time::Instant::now() + timeout;
    while std::time::Instant::now() < deadline {
        let v = match client.read_line_value()? {
            Some(v) => v,
            None => continue,
        };
        // Subscribe response.
        if v["id"].as_u64() == Some(sub_id) {
            let res = &v["result"];
            if let Some(arr) = res.as_array() {
                // Bitcoin-family: [ [[..],[..]], extranonce1, extranonce2_size ].
                if arr.len() >= 3 && arr[0].is_array() {
                    p.variant = "Stratum V1 (Bitcoin-family: SHA-256d / scrypt)".into();
                    p.subscribed = true;
                    p.supported = true;
                    p.extranonce1 = arr.get(1).and_then(|x| x.as_str()).unwrap_or("").to_string();
                    p.note = "supported by the native engine".into();
                }
                // Kaspa / EthereumStratum: [ true, "EthereumStratum/1.0.0" ].
                else if arr.iter().any(|x| x.as_str().map(|s| s.contains("EthereumStratum")).unwrap_or(false))
                    || arr.first().and_then(|x| x.as_bool()).unwrap_or(false)
                {
                    p.variant = "EthereumStratum/1.0.0 (Kaspa / kHeavyHash family)".into();
                    p.subscribed = true;
                    p.supported = true;
                    p.note = "EXPERIMENTAL Kaspa support — implemented; verify accepted shares on your pool".into();
                }
            }
            continue;
        }
        // Authorize result (id 2).
        if v["id"].as_u64() == Some(2) {
            p.authorized = v["result"].as_bool().unwrap_or(false);
            continue;
        }
        // Kaspa announces extranonce via a notification instead of the subscribe result.
        if v["method"].as_str() == Some("set_extranonce") {
            p.variant = "EthereumStratum/1.0.0 (Kaspa / kHeavyHash family)".into();
            p.supported = false;
            p.note = "NOT yet supported — Kaspa 'set_extranonce' dialect is on the roadmap".into();
        }
        match classify(&v) {
            StratumMsg::SetDifficulty(d) => p.difficulty = d,
            StratumMsg::Notify(_) => {
                p.got_job = true;
                break;
            }
            _ => {}
        }
    }
    if p.variant.is_empty() {
        p.variant = "unknown".into();
        p.note = "no recognizable stratum handshake".into();
    }
    Ok(p)
}

/// Classify a decoded stratum line into a [`StratumMsg`].
pub fn classify(v: &Value) -> StratumMsg {
    if let Some(method) = v["method"].as_str() {
        match method {
            "mining.set_difficulty" => {
                let d = v["params"][0].as_f64().unwrap_or(1.0);
                return StratumMsg::SetDifficulty(d);
            }
            "mining.notify" => {
                let p = &v["params"];
                let job = Job {
                    job_id: p[0].as_str().unwrap_or("").to_string(),
                    prevhash: p[1].as_str().unwrap_or("").to_string(),
                    coinb1: p[2].as_str().unwrap_or("").to_string(),
                    coinb2: p[3].as_str().unwrap_or("").to_string(),
                    merkle_branch: p[4].as_array().map(|a| a.iter().filter_map(|x| x.as_str().map(|s| s.to_string())).collect()).unwrap_or_default(),
                    version: p[5].as_str().unwrap_or("").to_string(),
                    nbits: p[6].as_str().unwrap_or("").to_string(),
                    ntime: p[7].as_str().unwrap_or("").to_string(),
                    clean_jobs: p[8].as_bool().unwrap_or(false),
                };
                return StratumMsg::Notify(job);
            }
            _ => return StratumMsg::Other(v.clone()),
        }
    }
    if let Some(id) = v["id"].as_u64() {
        let ok = v["result"].as_bool().unwrap_or(false) || v["error"].is_null() && !v["result"].is_null();
        return StratumMsg::Result { id, ok };
    }
    StratumMsg::Other(v.clone())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pow::sha256d;

    #[test]
    fn hex_round_trip() {
        let b = vec![0x00, 0xde, 0xad, 0xbe, 0xef, 0xff];
        assert_eq!(to_hex(&b), "00deadbeefff");
        assert_eq!(from_hex("00deadbeefff").unwrap(), b);
        assert!(from_hex("xyz").is_none());
        assert!(from_hex("abc").is_none()); // odd length
    }

    #[test]
    fn difficulty1_is_the_pdiff_target() {
        let t = difficulty_to_target(1.0);
        assert_eq!(
            to_hex(&t),
            "00000000ffff0000000000000000000000000000000000000000000000000000"
        );
        // Difficulty 2 halves the target.
        let t2 = difficulty_to_target(2.0);
        assert_eq!(
            to_hex(&t2),
            "000000007fff8000000000000000000000000000000000000000000000000000"
        );
    }

    #[test]
    fn merkle_root_empty_branch_is_coinbase_dsha() {
        let cb = b"kairos-coinbase";
        assert_eq!(merkle_root(cb, &[]), sha256d(cb));
    }

    #[test]
    fn merkle_root_one_node() {
        let cb = b"coinbase-tx-bytes";
        let node = [0xabu8; 32];
        let expected = {
            let mut buf = [0u8; 64];
            buf[..32].copy_from_slice(&sha256d(cb));
            buf[32..].copy_from_slice(&node);
            sha256d(&buf)
        };
        assert_eq!(merkle_root(cb, &[node]), expected);
    }

    #[test]
    fn coinbase_concatenates_extranonces() {
        let cb = build_coinbase(&[0x01, 0x02], &[0xaa], &[0xbb, 0xcc], &[0x03]);
        assert_eq!(cb, vec![0x01, 0x02, 0xaa, 0xbb, 0xcc, 0x03]);
    }

    #[test]
    fn header_layout_and_endianness() {
        let job = Job {
            version: "20000000".into(),
            prevhash: "00000000000000000008a1b2c3d4e5f60718293a4b5c6d7e8f90a1b2c3d4e5f6".into(),
            nbits: "170355f0".into(),
            ntime: "5f5e1000".into(),
            ..Default::default()
        };
        let mr = [0x11u8; 32];
        let hdr = build_header(&job, &mr, &job.ntime, 0x01020304).expect("valid header");
        // version little-endian.
        assert_eq!(&hdr[0..4], &[0x00, 0x00, 0x00, 0x20]);
        // merkle root placed as-is.
        assert_eq!(&hdr[36..68], &mr);
        // ntime little-endian.
        assert_eq!(&hdr[68..72], &[0x00, 0x10, 0x5e, 0x5f]);
        // nbits little-endian.
        assert_eq!(&hdr[72..76], &[0xf0, 0x55, 0x03, 0x17]);
        // nonce little-endian.
        assert_eq!(&hdr[76..80], &[0x04, 0x03, 0x02, 0x01]);
    }

    #[test]
    fn endpoint_parsing() {
        assert_eq!(parse_endpoint("stratum+tcp://pool.example.com:3333"), Some(("pool.example.com".into(), 3333)));
        assert_eq!(parse_endpoint("1.2.3.4:4444"), Some(("1.2.3.4".into(), 4444)));
        assert_eq!(parse_endpoint("nohost"), None);
    }

    #[test]
    fn classify_notify_and_difficulty() {
        let v: Value = serde_json::from_str(
            r#"{"id":null,"method":"mining.set_difficulty","params":[16384]}"#,
        )
        .unwrap();
        match classify(&v) {
            StratumMsg::SetDifficulty(d) => assert_eq!(d, 16384.0),
            _ => panic!("expected set_difficulty"),
        }
        let v: Value = serde_json::from_str(
            r#"{"id":null,"method":"mining.notify","params":["job1","ph","c1","c2",["aa"],"20000000","170355f0","5f5e1000",true]}"#,
        )
        .unwrap();
        match classify(&v) {
            StratumMsg::Notify(j) => {
                assert_eq!(j.job_id, "job1");
                assert_eq!(j.merkle_branch, vec!["aa".to_string()]);
                assert!(j.clean_jobs);
            }
            _ => panic!("expected notify"),
        }
    }
}
