//! The fleet API + live operator dashboard.
//!
//! A JSON endpoint compatible with what existing fleet tools expect, plus a
//! single-port web console that updates in real time — the monitoring model
//! professional miner software is built around. Pure `std::net`: no async
//! runtime, no heavy dependency. The dashboard shell is static HTML+CSS+JS that
//! polls `/metrics` every couple of seconds and re-renders.

use crate::model::*;
use crate::runtime::{Engine, EngineWorld};
use crate::twin::SimWorld;
use anyhow::Result;
use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;
use std::sync::{Arc, Mutex};

fn human_hashrate(h: f64) -> String {
    if h >= 1e15 {
        format!("{:.2} PH/s", h / 1e15)
    } else if h >= 1e12 {
        format!("{:.2} TH/s", h / 1e12)
    } else if h >= 1e9 {
        format!("{:.2} GH/s", h / 1e9)
    } else if h >= 1e6 {
        format!("{:.1} MH/s", h / 1e6)
    } else if h >= 1e3 {
        format!("{:.1} kH/s", h / 1e3)
    } else {
        format!("{:.0} H/s", h)
    }
}

/// A rich, live JSON snapshot of the whole fleet — the `/metrics` payload.
pub fn snapshot_json(engine: &Engine, world: &SimWorld) -> String {
    let belief = world.sense();

    // Per-device assignment from the latest decision.
    let mut asg: std::collections::BTreeMap<DeviceId, Assignment> = Default::default();
    if let Some(d) = &engine.last_decision {
        for sp in &d.action.setpoints {
            if let Some(a) = &sp.assignment {
                asg.insert(sp.device.clone(), a.clone());
            }
        }
    }

    let class_str = |c: DeviceClass| match c {
        DeviceClass::Gpu => "GPU",
        DeviceClass::Asic => "ASIC",
        DeviceClass::Fpga => "FPGA",
    };

    let mut devices = Vec::new();
    for (id, prof) in &engine.devices {
        let tel = belief.devices.get(id);
        let a = asg.get(id);
        let hr = tel.map(|t| t.hashrate).unwrap_or(0.0);
        let pw = tel.map(|t| t.power_w).unwrap_or(0.0);
        let sc = engine.stats.device(id);
        let rul = belief.health.get(id).map(|h| h.rul_frac).unwrap_or(1.0);
        let eff = if hr > 0.0 { pw / (hr / 1e12) } else { 0.0 };
        devices.push(serde_json::json!({
            "id": id.to_string(),
            "site": prof.site.to_string(),
            "class": class_str(prof.class),
            "model": prof.model,
            "algo": a.map(|x| x.algo.to_string()).unwrap_or_default(),
            "coin": a.map(|x| x.coin.to_string()).unwrap_or_default(),
            "pool": a.map(|x| x.pool.to_string()).unwrap_or_default(),
            "hashrate_hs": hr,
            "hashrate_h": human_hashrate(hr),
            "temp_c": tel.map(|t| t.temp_c).unwrap_or(0.0),
            "power_w": pw,
            "fan_pct": tel.map(|t| t.fan_pct).unwrap_or(0.0),
            "accepted": sc.accepted,
            "rejected": sc.rejected,
            "reject_pct": sc.reject_pct(),
            "efficiency_jth": eff,
            "rul_frac": rul,
            "online": tel.map(|t| t.online).unwrap_or(false),
            "mining": a.is_some() && hr > 0.0,
            "fault": tel.and_then(|t| t.fault.clone()),
        }));
    }

    let scheme_str = |s: RewardScheme| match s {
        RewardScheme::Pps => "PPS",
        RewardScheme::Fpps => "FPPS",
        RewardScheme::Pplns => "PPLNS",
        RewardScheme::Prop => "PROP",
        RewardScheme::Solo => "SOLO",
    };
    let mut pools = Vec::new();
    for (id, pd) in &world.market.pools {
        let pb = belief.pools.get(id);
        let sc = engine.stats.pool(id);
        pools.push(serde_json::json!({
            "id": id.to_string(),
            "coin": pd.coin.to_string(),
            "scheme": scheme_str(pd.scheme),
            "fee_pct": pd.fee_frac * 100.0,
            "latency_ms": pb.map(|p| p.latency_ms.min(99999.0)).unwrap_or(0.0),
            "accepted": sc.accepted,
            "rejected": sc.rejected,
            "reject_pct": sc.reject_pct(),
            "online": pb.map(|p| p.online).unwrap_or(true),
            "url": pd.url,
            "user": pd.user,
            "priority": pd.priority,
        }));
    }

    let mut sites = Vec::new();
    for site in world.sites() {
        let power: f64 = engine
            .devices
            .iter()
            .filter(|(_, p)| p.site == site)
            .filter_map(|(id, _)| belief.devices.get(id))
            .map(|t| t.power_w)
            .sum();
        let cap = engine.budget.cap(&site);
        sites.push(serde_json::json!({
            "id": site.to_string(),
            "power_w": power,
            "cap_w": if cap.is_finite() { cap } else { 0.0 },
            "ambient_c": belief.ambient_c,
            "dr_armed": belief.dr_credit_usd_kwh.is_some(),
        }));
    }

    let credit: Vec<_> = engine
        .ledgers
        .credit
        .ranked()
        .into_iter()
        .map(|(k, v)| serde_json::json!({ "lever": k, "usd": v }))
        .collect();

    // Downsampled time series for the dashboard charts (≈80 points).
    let hist = &engine.history;
    let step = (hist.len() / 80).max(1);
    let history: Vec<_> = hist
        .iter()
        .step_by(step)
        .map(|s| {
            serde_json::json!({
                "t": s.t_secs,
                "net_day": s.user_net_per_s * 86_400.0,
                "base_day": s.baseline_net_per_s * 86_400.0,
                "hashrate": s.hashrate_hs,
                "power_kw": s.power_w / 1000.0,
            })
        })
        .collect();

    // Merge rationale + events into one recent feed, newest first.
    let mut feed: Vec<(f64, String, String)> = Vec::new();
    for r in &engine.rationale {
        feed.push((r.t_secs, r.scope.clone(), r.message.clone()));
    }
    for e in &engine.events {
        feed.push((-1.0, "system".into(), e.clone()));
    }
    let feed_json: Vec<_> = engine
        .rationale
        .iter()
        .rev()
        .take(8)
        .map(|r| serde_json::json!({ "t": r.t_secs, "scope": r.scope, "message": r.message }))
        .collect();
    let events_json: Vec<_> = engine
        .events
        .iter()
        .rev()
        .take(8)
        .map(|e| serde_json::json!({ "message": e }))
        .collect();
    let _ = feed;

    let total_h: f64 = belief.devices.values().map(|t| t.hashrate).sum();
    let total_p: f64 = engine
        .devices
        .keys()
        .filter_map(|id| belief.devices.get(id))
        .map(|t| t.power_w)
        .sum();

    let v = serde_json::json!({
        "name": "kairos",
        "version": "0.1.0",
        "mode": if engine.scheduled_pause { "off-peak pause" } else { engine.autonomy.label() },
        "risk": engine.risk_word.to_string(),
        "confidence": belief.confidence,
        "sim2real_error": world.s2r_error(),
        "t_secs": belief.t_secs,
        "fleet": {
            "sites": world.sites().len(),
            "devices": engine.devices.len(),
            "online": engine.devices.keys().filter(|id| belief.devices.get(*id).map(|t| t.online).unwrap_or(false)).count(),
            "mining": devices.iter().filter(|d| d["mining"].as_bool().unwrap_or(false)).count(),
            "hashrate_hs": total_h,
            "hashrate_h": human_hashrate(total_h),
            "power_w": total_p,
            "uptime_frac": engine.uptime_frac(),
        },
        "economics": {
            "net_usd_per_day": engine.net_per_day(),
            "mining_uplift_frac": engine.mining_uplift_frac(),
            "total_uplift_frac": engine.uplift_frac(),
            "mining_net_usd": engine.ledgers.value.mining_net_usd(),
            "grid_income_usd": engine.grid_income(),
            "cum_net_usd": engine.ledgers.value.cum_usd,
            "baseline_cum_usd": engine.ledgers.value.baseline_cum_usd,
        },
        "sites": sites,
        "devices": devices,
        "pools": pools,
        "credit": credit,
        "history": history,
        "profitability": engine.profitability(world).iter().map(|c| serde_json::json!({
            "coin": c.coin, "algo": c.algo, "class": c.class,
            "profit_per_day": c.profit_per_day, "hashrate_hs": c.hashrate_hs, "active": c.active,
        })).collect::<Vec<_>>(),
        "feed": feed_json,
        "events": events_json,
        "ledger": {
            "cum_regret": engine.ledgers.regret.cum_regret,
            "mean_regret": engine.ledgers.regret.mean_regret(),
        },
    });
    serde_json::to_string(&v).unwrap_or_else(|_| "{}".into())
}

/// Best-effort open of a URL in the operator's default browser.
pub fn open_browser(url: &str) {
    let _ = if cfg!(target_os = "windows") {
        std::process::Command::new("cmd").args(["/c", "start", "", url]).spawn()
    } else if cfg!(target_os = "macos") {
        std::process::Command::new("open").arg(url).spawn()
    } else {
        std::process::Command::new("xdg-open").arg(url).spawn()
    };
}

/// The static dashboard shell — HTML + CSS + JS that polls `/metrics` and
/// re-renders. No interpolation, so it is a plain constant.
pub const DASHBOARD_SHELL: &str = include_str!("dashboard.html");

fn respond(stream: &mut std::net::TcpStream, status: &str, ctype: &str, body: &str) {
    let resp = format!(
        "HTTP/1.1 {}\r\nContent-Type: {}\r\nContent-Length: {}\r\nCache-Control: no-store\r\nConnection: close\r\n\r\n{}",
        status,
        ctype,
        body.as_bytes().len(),
        body
    );
    let _ = stream.write_all(resp.as_bytes());
}

/// A parsed inbound HTTP request (method + path + body).
struct Req {
    method: String,
    path: String,
    body: String,
}

fn read_request(stream: &std::net::TcpStream) -> Req {
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    if reader.read_line(&mut line).is_err() {
        return Req { method: "GET".into(), path: "/".into(), body: String::new() };
    }
    let mut parts = line.split_whitespace();
    let method = parts.next().unwrap_or("GET").to_string();
    let path = parts.next().unwrap_or("/").to_string();
    // Read headers until an empty line, capturing Content-Length.
    let mut content_length = 0usize;
    loop {
        let mut hdr = String::new();
        if reader.read_line(&mut hdr).is_err() {
            break;
        }
        let trimmed = hdr.trim_end_matches(&['\r', '\n'][..]);
        if trimmed.is_empty() {
            break;
        }
        if let Some(v) = trimmed.to_ascii_lowercase().strip_prefix("content-length:") {
            content_length = v.trim().parse().unwrap_or(0);
        }
    }
    // Read the body, capped so a malicious client can't exhaust memory.
    let mut body = String::new();
    if content_length > 0 && content_length < 1_000_000 {
        use std::io::Read;
        let mut buf = vec![0u8; content_length];
        if reader.read_exact(&mut buf).is_ok() {
            body = String::from_utf8_lossy(&buf).into_owned();
        }
    }
    Req { method, path, body }
}

/// GET /config — return the operator-editable subset (wallets + pools) as JSON.
fn handle_config_get(config_path: &str) -> String {
    let cfg = match std::fs::read_to_string(config_path).ok().and_then(|s| crate::config::Config::from_toml(&s).ok()) {
        Some(c) => c,
        None => crate::config::Config::demo(),
    };
    // Emit only the fields the Settings form owns, in a shape the JS uses directly.
    let pools: Vec<serde_json::Value> = cfg
        .pool
        .iter()
        .map(|p| {
            serde_json::json!({
                "coin": p.coin, "url": p.url, "user": p.user, "worker": p.worker,
                "pass": p.pass, "scheme": p.scheme, "priority": p.priority,
            })
        })
        .collect();
    serde_json::json!({ "wallets": cfg.wallets, "pools": pools, "path": config_path }).to_string()
}

/// POST /config — accept the same shape and persist it, preserving every other
/// section of `kairos.toml`. Returns `{ ok: true }` or an error message.
fn handle_config_post(body: &str, config_path: &str) -> (String, String) {
    #[derive(serde::Deserialize)]
    struct Payload {
        #[serde(default)]
        wallets: std::collections::BTreeMap<String, String>,
        #[serde(default)]
        pools: Vec<crate::config::PoolEntry>,
    }
    let p: Payload = match serde_json::from_str(body) {
        Ok(v) => v,
        Err(e) => return ("400 Bad Request".into(), serde_json::json!({ "ok": false, "error": format!("invalid JSON: {e}") }).to_string()),
    };
    let mut cfg = match std::fs::read_to_string(config_path).ok().and_then(|s| crate::config::Config::from_toml(&s).ok()) {
        Some(c) => c,
        None => crate::config::Config::demo(),
    };
    cfg.apply_operator_edits(p.wallets, p.pools);
    let toml_str = match cfg.to_toml() {
        Ok(s) => s,
        Err(e) => return ("500 Internal Server Error".into(), serde_json::json!({ "ok": false, "error": e.to_string() }).to_string()),
    };
    // Atomic-ish write: write to a temp file, then rename.
    let tmp = format!("{config_path}.tmp");
    if let Err(e) = std::fs::write(&tmp, &toml_str) {
        return ("500 Internal Server Error".into(), serde_json::json!({ "ok": false, "error": format!("write failed: {e}") }).to_string());
    }
    if let Err(e) = std::fs::rename(&tmp, config_path) {
        return ("500 Internal Server Error".into(), serde_json::json!({ "ok": false, "error": format!("rename failed: {e}") }).to_string());
    }
    ("200 OK".into(), serde_json::json!({ "ok": true, "saved_to": config_path }).to_string())
}

fn respond_404(stream: &mut std::net::TcpStream) {
    let r = "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
    let _ = stream.write_all(r.as_bytes());
}

/// Serve the LIVE dashboard backed by shared engine+world state (the ticker
/// thread keeps it advancing). Opens the dashboard in the browser. Blocks.
pub fn serve_live(shared: Arc<Mutex<EngineWorld>>, bind: &str) -> Result<()> {
    let listener = TcpListener::bind(bind)?;
    let dash_url = format!("http://{bind}/dash");
    println!("\n  KAIROS dashboard ready  →  {dash_url}");
    println!("  fleet API: http://{bind}/metrics (JSON)   ·   Ctrl-C to stop");
    println!("  opening your browser…\n");
    open_browser(&dash_url);
    let config_path = "kairos.toml".to_string();
    for stream in listener.incoming() {
        let mut stream = match stream {
            Ok(s) => s,
            Err(_) => continue,
        };
        let req = read_request(&stream);
        match (req.method.as_str(), req.path.as_str()) {
            ("GET", "/metrics") | ("GET", "/api") => {
                let body = match shared.lock() {
                    Ok(g) => snapshot_json(&g.engine, &g.world),
                    Err(_) => "{}".into(),
                };
                respond(&mut stream, "200 OK", "application/json", &body);
            }
            ("GET", "/") | ("GET", "/dash") | ("GET", "/dashboard") => {
                respond(&mut stream, "200 OK", "text/html; charset=utf-8", DASHBOARD_SHELL);
            }
            ("GET", "/config") => {
                respond(&mut stream, "200 OK", "application/json", &handle_config_get(&config_path));
            }
            ("POST", "/config") => {
                let (status, body) = handle_config_post(&req.body, &config_path);
                respond(&mut stream, &status, "application/json", &body);
            }
            _ => respond_404(&mut stream),
        }
    }
    Ok(())
}

/// Serve a one-shot static snapshot (no background ticking) — used by the
/// fixed-run `start --serve` path and for quick inspection.
pub fn serve(bind: &str, engine: &Engine, world: &SimWorld) -> Result<()> {
    let listener = TcpListener::bind(bind)?;
    let json = snapshot_json(engine, world);
    let dash_url = format!("http://{bind}/dash");
    println!("\n  KAIROS dashboard ready  →  {dash_url}   ·   Ctrl-C to stop");
    open_browser(&dash_url);
    let config_path = "kairos.toml".to_string();
    for stream in listener.incoming() {
        let mut stream = match stream {
            Ok(s) => s,
            Err(_) => continue,
        };
        let req = read_request(&stream);
        match (req.method.as_str(), req.path.as_str()) {
            ("GET", "/metrics") | ("GET", "/api") => {
                respond(&mut stream, "200 OK", "application/json", &json);
            }
            ("GET", "/") | ("GET", "/dash") | ("GET", "/dashboard") => {
                respond(&mut stream, "200 OK", "text/html; charset=utf-8", DASHBOARD_SHELL);
            }
            ("GET", "/config") => {
                respond(&mut stream, "200 OK", "application/json", &handle_config_get(&config_path));
            }
            ("POST", "/config") => {
                let (status, body) = handle_config_post(&req.body, &config_path);
                respond(&mut stream, &status, "application/json", &body);
            }
            _ => respond_404(&mut stream),
        }
    }
    Ok(())
}
