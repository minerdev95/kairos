//! KAIROS native mining engine — the hashing runtime KAIROS runs itself.
//!
//! This is what replaces a wrapped third-party binary: [`NativeMiner`] spins up
//! CPU worker threads that pull the current job, scan disjoint nonce ranges with
//! [`crate::pow`], and report hashrate + found shares through lock-free counters
//! and a channel. [`PoolSession`] wires a [`crate::stratum`] connection to the
//! miner — jobs in, shares out — so KAIROS connects to the operator's pool and
//! does the proof-of-work end to end, with no external process.
//!
//! GPU hashing is expressed through the [`GpuHasher`] trait; a real CUDA backend
//! lives in [`crate::gpu`] behind the `gpu` cargo feature. The CPU path is the
//! always-available default and is what the tests exercise.

use crate::pow::{self, PowKind, Solved};
use crate::stratum::{self, Job, StratumClient, StratumMsg};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

/// How many nonces a worker claims per chunk before reporting back. Sized per
/// algorithm so a chunk stays short (≈sub-second) even for slow, memory-hard PoW
/// — otherwise a worker stuck mid-chunk can't see a new job or a stop signal.
fn chunk_for(kind: PowKind) -> u32 {
    match kind {
        PowKind::Scrypt => 512,       // memory-hard, ~kH/s on CPU
        PowKind::HeavyHash => 50_000, // ~MH/s
        PowKind::Sha256d => 200_000,  // fast
    }
}

/// The current work all workers share. `epoch` bumps on every new job so workers
/// know to reload; `epoch == 0` means "no work yet, idle".
#[derive(Clone)]
struct Work {
    kind: PowKind,
    header: [u8; 80],
    target: [u8; 32],
    epoch: u64,
    /// Bookkeeping so a found share can be submitted with the right context.
    extranonce2_hex: String,
    ntime_hex: String,
    job_id: String,
}

impl Default for Work {
    fn default() -> Self {
        Work {
            kind: PowKind::Sha256d,
            header: [0u8; 80],
            target: [0xff; 32],
            epoch: 0,
            extranonce2_hex: String::new(),
            ntime_hex: String::new(),
            job_id: String::new(),
        }
    }
}

/// A found share, tagged with the job context needed to submit it.
#[derive(Clone, Debug)]
pub struct FoundShare {
    pub job_id: String,
    pub extranonce2_hex: String,
    pub ntime_hex: String,
    pub nonce: u32,
    pub hash: [u8; 32],
}

/// A GPU hashing backend. Implemented for real by [`crate::gpu`] behind the `gpu`
/// feature; the CPU engine works without it.
pub trait GpuHasher: Send + Sync {
    fn name(&self) -> String;
    /// Search a nonce range on the GPU, mirroring [`pow::search`]'s contract.
    fn search(&self, kind: PowKind, header: &[u8; 80], target: &[u8; 32], start: u32, count: u32) -> Result<Solved, u64>;
}

/// Lock-free engine statistics.
#[derive(Default)]
struct Stats {
    hashes: AtomicU64,
    shares_found: AtomicU64,
}

/// The native miner: N workers hashing one job at a time.
pub struct NativeMiner {
    shared: Arc<Mutex<Work>>,
    cursor: Arc<AtomicU32>,
    running: Arc<AtomicBool>,
    stats: Arc<Stats>,
    found_rx: Receiver<FoundShare>,
    handles: Vec<JoinHandle<()>>,
    started: Instant,
    last_sample: Mutex<(Instant, u64)>,
    pub workers: usize,
    pub backend_label: String,
}

impl NativeMiner {
    /// Start `workers` CPU worker threads (idle until [`set_job`] is called). If a
    /// `gpu` hasher is supplied, a single GPU-driver thread is used instead.
    pub fn start(workers: usize, gpu: Option<Arc<dyn GpuHasher>>) -> Self {
        let shared = Arc::new(Mutex::new(Work::default()));
        let cursor = Arc::new(AtomicU32::new(0));
        let running = Arc::new(AtomicBool::new(true));
        let stats = Arc::new(Stats::default());
        let (tx, found_rx) = std::sync::mpsc::channel();

        let mut handles = Vec::new();
        let (n, label) = match &gpu {
            Some(g) => (1usize, format!("GPU:{}", g.name())),
            None => (workers.max(1), format!("CPU:{} thread(s)", workers.max(1))),
        };
        for _ in 0..n {
            let shared = shared.clone();
            let cursor = cursor.clone();
            let running = running.clone();
            let stats = stats.clone();
            let tx = tx.clone();
            let gpu = gpu.clone();
            handles.push(std::thread::spawn(move || {
                worker_loop(shared, cursor, running, stats, tx, gpu);
            }));
        }

        NativeMiner {
            shared,
            cursor,
            running,
            stats,
            found_rx,
            handles,
            started: Instant::now(),
            last_sample: Mutex::new((Instant::now(), 0)),
            workers: n,
            backend_label: label,
        }
    }

    /// Install a new job. Workers reload it and restart their nonce scan.
    pub fn set_job(&self, kind: PowKind, header: [u8; 80], target: [u8; 32], job_id: String, extranonce2_hex: String, ntime_hex: String) {
        let mut g = self.shared.lock().unwrap();
        g.kind = kind;
        g.header = header;
        g.target = target;
        g.epoch += 1;
        g.job_id = job_id;
        g.extranonce2_hex = extranonce2_hex;
        g.ntime_hex = ntime_hex;
        self.cursor.store(0, Ordering::SeqCst);
    }

    /// Clear work (workers go idle).
    pub fn idle(&self) {
        let mut g = self.shared.lock().unwrap();
        g.epoch = 0;
        self.cursor.store(0, Ordering::SeqCst);
    }

    /// Total hashes computed since start.
    pub fn total_hashes(&self) -> u64 {
        self.stats.hashes.load(Ordering::Relaxed)
    }

    pub fn total_found(&self) -> u64 {
        self.stats.shares_found.load(Ordering::Relaxed)
    }

    /// Average hashrate since start (H/s).
    pub fn avg_hashrate(&self) -> f64 {
        let secs = self.started.elapsed().as_secs_f64().max(1e-6);
        self.total_hashes() as f64 / secs
    }

    /// Instantaneous hashrate since the last call (H/s).
    pub fn sample_hashrate(&self) -> f64 {
        let now = Instant::now();
        let total = self.total_hashes();
        let mut g = self.last_sample.lock().unwrap();
        let dt = (now - g.0).as_secs_f64();
        let dh = total.saturating_sub(g.1);
        *g = (now, total);
        if dt > 1e-3 {
            dh as f64 / dt
        } else {
            self.avg_hashrate()
        }
    }

    /// Drain any shares found since the last call.
    pub fn drain_found(&self) -> Vec<FoundShare> {
        let mut out = Vec::new();
        while let Ok(s) = self.found_rx.try_recv() {
            out.push(s);
        }
        out
    }

    /// Stop all workers and join.
    pub fn stop(mut self) {
        self.running.store(false, Ordering::SeqCst);
        for h in self.handles.drain(..) {
            let _ = h.join();
        }
    }
}

fn worker_loop(
    shared: Arc<Mutex<Work>>,
    cursor: Arc<AtomicU32>,
    running: Arc<AtomicBool>,
    stats: Arc<Stats>,
    tx: Sender<FoundShare>,
    gpu: Option<Arc<dyn GpuHasher>>,
) {
    while running.load(Ordering::Relaxed) {
        // Snapshot the current work.
        let work = {
            let g = shared.lock().unwrap();
            g.clone()
        };
        if work.epoch == 0 {
            std::thread::sleep(Duration::from_millis(20));
            continue;
        }
        // Claim a disjoint nonce range for this job (chunk sized per algorithm).
        let chunk = chunk_for(work.kind);
        let start = cursor.fetch_add(chunk, Ordering::SeqCst);
        if start.checked_add(chunk).is_none() {
            // Nonce space for this job exhausted; wait for fresh work.
            std::thread::sleep(Duration::from_millis(20));
            continue;
        }
        let res = match &gpu {
            Some(g) => g.search(work.kind, &work.header, &work.target, start, chunk),
            None => pow::search(work.kind, &work.header, &work.target, start, chunk),
        };
        match res {
            Ok(sol) => {
                stats.hashes.fetch_add(sol.tries, Ordering::Relaxed);
                stats.shares_found.fetch_add(1, Ordering::Relaxed);
                let _ = tx.send(FoundShare {
                    job_id: work.job_id.clone(),
                    extranonce2_hex: work.extranonce2_hex.clone(),
                    ntime_hex: work.ntime_hex.clone(),
                    nonce: sol.nonce,
                    hash: sol.hash,
                });
            }
            Err(tries) => {
                stats.hashes.fetch_add(tries, Ordering::Relaxed);
            }
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Turning a stratum job into work.
// ─────────────────────────────────────────────────────────────────────────────

/// Build an 80-byte header template + share target + the extranonce2 used, from a
/// `mining.notify` job and the connection's extranonce1. `extranonce2` is chosen
/// by the miner (here a fixed zero-filled value of the pool's required size — one
/// full nonce sweep per job is ample for CPU speeds; the pool pushes fresh jobs
/// frequently). Returns `None` if any field is malformed.
pub fn work_from_job(
    job: &Job,
    extranonce1: &[u8],
    extranonce2: &[u8],
    difficulty: f64,
) -> Option<([u8; 80], [u8; 32], String)> {
    let coinb1 = stratum::from_hex(&job.coinb1)?;
    let coinb2 = stratum::from_hex(&job.coinb2)?;
    let coinbase = stratum::build_coinbase(&coinb1, extranonce1, extranonce2, &coinb2);
    let mut branch = Vec::with_capacity(job.merkle_branch.len());
    for h in &job.merkle_branch {
        let v = stratum::from_hex(h)?;
        if v.len() != 32 {
            return None;
        }
        let mut a = [0u8; 32];
        a.copy_from_slice(&v);
        branch.push(a);
    }
    let mr = stratum::merkle_root(&coinbase, &branch);
    let header = stratum::build_header(job, &mr, &job.ntime, 0)?;
    let target = stratum::difficulty_to_target(difficulty);
    Some((header, target, stratum::to_hex(extranonce2)))
}

// ─────────────────────────────────────────────────────────────────────────────
// Pool session: stratum connection ↔ native miner.
// ─────────────────────────────────────────────────────────────────────────────

/// Shared, live-published state for a running pool session (read by the UI).
#[derive(Default)]
pub struct SessionShared {
    pub stop: AtomicBool,
    pub connected: AtomicBool,
    pub accepted: AtomicU64,
    pub rejected: AtomicU64,
    pub submitted: AtomicU64,
    /// Hashrate in milli-hashes/sec (H/s × 1000) for lock-free publishing.
    pub hashrate_mhs: AtomicU64,
    pub last_error: Mutex<Option<String>>,
}

impl SessionShared {
    pub fn hashrate(&self) -> f64 {
        self.hashrate_mhs.load(Ordering::Relaxed) as f64 / 1000.0
    }
}

/// Drives a real pool connection with the native miner: reads jobs, feeds work,
/// submits found shares, tracks accept/reject, and publishes live stats into
/// `shared`. Runs until `shared.stop` is set or a fatal connection error.
///
/// This is the real thing (it connects and submits); it is not exercised by the
/// unit tests because that needs a live pool, but every arithmetic step it relies
/// on ([`work_from_job`], header/target/merkle) is tested in isolation.
pub struct PoolSession;

impl PoolSession {
    pub fn run(
        url: &str,
        user: &str,
        pass: &str,
        kind: PowKind,
        miner: &NativeMiner,
        agent: &str,
        shared: &SessionShared,
        deadline: Option<Instant>,
    ) -> std::io::Result<()> {
        let mut client = StratumClient::connect(url, Duration::from_secs(15))?;
        client.subscribe(agent)?;
        client.authorize(user, pass)?;
        shared.connected.store(true, Ordering::SeqCst);
        let extranonce1 = client.extranonce1.clone();
        let extranonce2 = vec![0u8; client.extranonce2_size];
        let mut last_job: Option<Job> = None;

        // Install (or reinstall) work for the current job at the current difficulty.
        let install = |client: &StratumClient, job: &Job| {
            work_from_job(job, &extranonce1, &extranonce2, client.difficulty)
                .map(|(header, target, e2)| (header, target, e2, job.job_id.clone(), job.ntime.clone()))
        };

        while !shared.stop.load(Ordering::Relaxed) {
            // End this session when the round deadline passes (used for dev-fee
            // time-slicing — the caller reconnects with the next round's wallet).
            if let Some(dl) = deadline {
                if Instant::now() >= dl {
                    break;
                }
            }
            // Submit any shares the workers found.
            for f in miner.drain_found() {
                if client.submit(user, &f.job_id, &f.extranonce2_hex, &f.ntime_hex, f.nonce).is_ok() {
                    shared.submitted.fetch_add(1, Ordering::Relaxed);
                }
            }
            // Publish live hashrate.
            shared.hashrate_mhs.store((miner.sample_hashrate() * 1000.0) as u64, Ordering::Relaxed);
            // Process one inbound message (bounded by the socket timeout).
            match client.next_message() {
                Ok(Some(StratumMsg::SetDifficulty(d))) => {
                    client.difficulty = d;
                    if let Some(job) = &last_job {
                        if let Some((h, t, e2, jid, nt)) = install(&client, job) {
                            miner.set_job(kind, h, t, jid, e2, nt);
                        }
                    }
                }
                Ok(Some(StratumMsg::Notify(job))) => {
                    if let Some((h, t, e2, jid, nt)) = install(&client, &job) {
                        miner.set_job(kind, h, t, jid, e2, nt);
                    }
                    last_job = Some(job);
                }
                Ok(Some(StratumMsg::Result { ok, .. })) => {
                    if ok {
                        shared.accepted.fetch_add(1, Ordering::Relaxed);
                    } else {
                        shared.rejected.fetch_add(1, Ordering::Relaxed);
                    }
                }
                Ok(Some(StratumMsg::Other(_))) | Ok(None) => {}
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock || e.kind() == std::io::ErrorKind::TimedOut => {}
                Err(e) => {
                    shared.connected.store(false, Ordering::SeqCst);
                    return Err(e);
                }
            }
        }
        shared.connected.store(false, Ordering::SeqCst);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn native_miner_finds_shares_multithreaded() {
        // Spin up the real multi-threaded engine on an easy target and prove it
        // computes proof-of-work and finds shares — the whole CPU pipeline.
        let miner = NativeMiner::start(4, None);
        let header = [42u8; 80];
        let target = pow::target_leading_zeros(18);
        miner.set_job(PowKind::Sha256d, header, target, "job".into(), "00000000".into(), "5f5e1000".into());

        let start = Instant::now();
        let mut found = Vec::new();
        while start.elapsed() < Duration::from_secs(6) && found.is_empty() {
            std::thread::sleep(Duration::from_millis(50));
            found.extend(miner.drain_found());
        }
        assert!(miner.total_hashes() > 0, "workers should have hashed");
        assert!(!found.is_empty(), "should find at least one 18-zero-bit share");
        // Independently verify a found share really meets the target.
        let f = &found[0];
        let mut hdr = header;
        hdr[76..80].copy_from_slice(&f.nonce.to_le_bytes());
        let h = pow::sha256d(&hdr);
        assert!(pow::meets_target(&h, &target));
        assert!(miner.avg_hashrate() > 0.0);
        miner.stop();
    }

    #[test]
    fn work_from_job_produces_valid_header() {
        let job = Job {
            job_id: "j1".into(),
            prevhash: "00000000000000000008a1b2c3d4e5f60718293a4b5c6d7e8f90a1b2c3d4e5f6".into(),
            coinb1: "01000000".into(),
            coinb2: "ffffffff".into(),
            merkle_branch: vec![],
            version: "20000000".into(),
            nbits: "170355f0".into(),
            ntime: "5f5e1000".into(),
            clean_jobs: true,
        };
        let e1 = vec![0xde, 0xad];
        let e2 = vec![0x00, 0x00, 0x00, 0x00];
        let (header, target, e2hex) = work_from_job(&job, &e1, &e2, 1.0).expect("valid work");
        assert_eq!(e2hex, "00000000");
        // version little-endian in the header.
        assert_eq!(&header[0..4], &[0x00, 0x00, 0x00, 0x20]);
        // difficulty-1 target.
        assert_eq!(&target[..6], &[0x00, 0x00, 0x00, 0x00, 0xff, 0xff]);
    }
}
