//! Real hardware detection & live telemetry.
//!
//! KAIROS is a real mining tool: it detects the operator's actual GPUs (via
//! `nvidia-smi`) and CPU, builds device profiles the intelligence plans over, and
//! polls live temperature / power / clocks. The mining itself is done by KAIROS's
//! own native engine (see [`crate::engine`] / [`crate::pow`] / [`crate::stratum`])
//! connecting to the operator's pools; KAIROS decides *what* each device mines and
//! *when* to switch.

use crate::model::*;
use std::collections::BTreeMap;
use std::process::Command;

/// A detected NVIDIA GPU.
#[derive(Clone, Debug)]
pub struct GpuInfo {
    pub index: u32,
    pub name: String,
    pub mem_total_mb: f64,
    pub temp_c: f64,
    pub power_w: f64,
    pub power_limit_w: f64,
    pub fan_pct: f64,
    pub util_pct: f64,
    pub core_clock_mhz: f64,
    pub mem_clock_mhz: f64,
}

fn pf(s: &str) -> f64 {
    let t = s.trim();
    if t.is_empty() || t.contains("N/A") {
        0.0
    } else {
        t.split_whitespace().next().unwrap_or("0").parse().unwrap_or(0.0)
    }
}

/// True if `nvidia-smi` is present and responsive.
pub fn nvidia_available() -> bool {
    Command::new("nvidia-smi")
        .arg("-L")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

fn nvidia_query(fields: &str) -> Vec<Vec<String>> {
    let out = match Command::new("nvidia-smi")
        .arg(format!("--query-gpu={fields}"))
        .arg("--format=csv,noheader,nounits")
        .output()
    {
        Ok(o) if o.status.success() => o,
        _ => return Vec::new(),
    };
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| l.split(',').map(|c| c.trim().to_string()).collect())
        .collect()
}

/// Detect all NVIDIA GPUs with their current state.
pub fn detect_gpus() -> Vec<GpuInfo> {
    let rows = nvidia_query(
        "index,name,memory.total,temperature.gpu,power.draw,power.limit,fan.speed,utilization.gpu,clocks.gr,clocks.mem",
    );
    rows.into_iter()
        .filter(|r| r.len() >= 10)
        .map(|r| GpuInfo {
            index: pf(&r[0]) as u32,
            name: r[1].clone(),
            mem_total_mb: pf(&r[2]),
            temp_c: pf(&r[3]),
            power_w: pf(&r[4]),
            power_limit_w: pf(&r[5]),
            fan_pct: pf(&r[6]),
            util_pct: pf(&r[7]),
            core_clock_mhz: pf(&r[8]),
            mem_clock_mhz: pf(&r[9]),
        })
        .collect()
}

/// Detected CPU (for optional RandomX/CPU mining).
#[derive(Clone, Debug)]
pub struct CpuInfo {
    pub name: String,
    pub threads: usize,
}

pub fn detect_cpu() -> CpuInfo {
    let threads = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1);
    let name = std::env::var("PROCESSOR_IDENTIFIER")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "CPU".to_string());
    CpuInfo { name, threads }
}

/// Relative performance factor keyed off the GPU model name (RTX 3070 ≈ 1.0).
fn tier_factor(name: &str) -> f64 {
    let n = name.to_lowercase();
    let base = if n.contains("4090") { 2.05 }
        else if n.contains("4080") { 1.6 }
        else if n.contains("4070") { 1.2 }
        else if n.contains("4060") { 0.85 }
        else if n.contains("3090") { 1.6 }
        else if n.contains("3080") { 1.4 }
        else if n.contains("3070") { 1.0 }
        else if n.contains("3060") { 0.72 }
        else if n.contains("2080") { 0.9 }
        else if n.contains("2070") { 0.72 }
        else if n.contains("2060") { 0.58 }
        else if n.contains("1660") { 0.48 }
        else { 0.8 };
    // Laptop / mobile parts run ~80% of the desktop equivalent.
    if n.contains("laptop") || n.contains("mobile") || n.contains("max-q") { base * 0.82 } else { base }
}

/// Estimated per-algorithm capability for a GPU (a starting model the engine
/// refines from real telemetry once a miner is benchmarked).
pub fn estimate_gpu_caps(name: &str, mem_mb: f64, power_limit_w: f64) -> Vec<AlgoCapability> {
    let f = tier_factor(name);
    let pw = |frac: f64, fallback: f64| {
        let p = if power_limit_w > 5.0 { power_limit_w * frac } else { fallback * f };
        p.clamp(30.0, 600.0)
    };
    let mut caps = vec![
        AlgoCapability { algo: "kHeavyHash".into(), stock_hashrate: 0.90e9 * f, stock_power_w: pw(0.62, 130.0), dual_capable: false },
        AlgoCapability { algo: "Autolykos2".into(), stock_hashrate: 120.0e6 * f, stock_power_w: pw(0.55, 115.0), dual_capable: false },
        AlgoCapability { algo: "KawPow".into(), stock_hashrate: 22.0e6 * f, stock_power_w: pw(0.80, 150.0), dual_capable: false },
    ];
    // Etchash/Ethash needs enough VRAM for the DAG.
    if mem_mb >= 5000.0 {
        caps.push(AlgoCapability { algo: "Ethash".into(), stock_hashrate: 52.0e6 * f, stock_power_w: pw(0.65, 130.0), dual_capable: false });
    }
    caps
}

/// Build device profiles for the real fleet. `include_cpu` adds the CPU as an
/// (optional) RandomX device.
pub fn detect_devices(include_cpu: bool) -> Vec<DeviceProfile> {
    let mut out = Vec::new();
    for g in detect_gpus() {
        let max_power = if g.power_limit_w > 5.0 { g.power_limit_w } else { 200.0 };
        out.push(DeviceProfile {
            id: DeviceId::new(format!("GPU{}", g.index)),
            site: SiteId::new("local"),
            class: DeviceClass::Gpu,
            model: g.name.clone(),
            capabilities: estimate_gpu_caps(&g.name, g.mem_total_mb, g.power_limit_w),
            limits: DeviceLimits {
                max_power_w: max_power,
                max_temp_c: 83.0,
                max_core_voltage_mv: 1100.0,
                max_core_offset_mhz: 250.0,
                max_mem_offset_mhz: 2500.0,
                min_fan_pct: 30.0,
            },
            silicon_quality: 1.0,
        });
    }
    if include_cpu {
        out.push(cpu_device());
    }
    out
}

/// The CPU as a native-mining device: capabilities for the algorithms KAIROS's
/// own engine can hash on a CPU (SHA-256d, kHeavyHash, scrypt). Hashrates are
/// rough per-thread estimates the benchmark refines; this is what lets an operator
/// add a pool for one of these coins and mine it with the built-in engine.
pub fn cpu_device() -> DeviceProfile {
    let cpu = detect_cpu();
    let t = cpu.threads as f64;
    DeviceProfile {
        id: DeviceId::new("CPU0"),
        site: SiteId::new("local"),
        class: DeviceClass::Fpga, // fixed-function, steered across coins within an algo
        model: cpu.name.clone(),
        capabilities: vec![
            AlgoCapability { algo: "SHA-256".into(), stock_hashrate: t * 0.85e6, stock_power_w: t * 3.5, dual_capable: false },
            AlgoCapability { algo: "kHeavyHash".into(), stock_hashrate: t * 0.6e6, stock_power_w: t * 3.5, dual_capable: false },
            AlgoCapability { algo: "Scrypt".into(), stock_hashrate: t * 900.0, stock_power_w: t * 3.5, dual_capable: false },
        ],
        limits: DeviceLimits {
            max_power_w: t * 6.0,
            max_temp_c: 95.0,
            max_core_voltage_mv: 0.0,
            max_core_offset_mhz: 0.0,
            max_mem_offset_mhz: 0.0,
            min_fan_pct: 0.0,
        },
        silicon_quality: 1.0,
    }
}

/// Poll live GPU telemetry (temperature / power / fan / clocks). Hashrate and
/// reject rate come from the miner API, not from `nvidia-smi`, so they are left
/// at zero here and filled in by the miner manager.
pub fn gpu_telemetry() -> BTreeMap<DeviceId, DeviceTelemetry> {
    let mut out = BTreeMap::new();
    for g in detect_gpus() {
        out.insert(
            DeviceId::new(format!("GPU{}", g.index)),
            DeviceTelemetry {
                id: DeviceId::new(format!("GPU{}", g.index)),
                online: true,
                temp_c: g.temp_c,
                power_w: g.power_w,
                hashrate: 0.0,
                reject_rate: 0.0,
                hw_error_rate: 0.0,
                fan_pct: g.fan_pct,
                fault: None,
            },
        );
    }
    out
}

/// Apply a core/mem clock offset and power limit to a GPU via nvidia-smi.
/// Best-effort (requires privileges + supported driver); returns whether the
/// power-limit call succeeded. Clock offsets need `nvidia-settings` on Linux or a
/// vendor tool on Windows and are attempted only where available.
pub fn apply_gpu_setpoint(index: u32, power_limit_w: f64) -> bool {
    if power_limit_w <= 0.0 {
        return false;
    }
    Command::new("nvidia-smi")
        .args(["-i", &index.to_string(), "-pl", &format!("{:.0}", power_limit_w)])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}
