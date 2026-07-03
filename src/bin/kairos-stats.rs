//! KAIROS telemetry ingest server — the owner's fleet dashboard.
//!
//! A tiny, dependency-light server (std::net + serde_json) the project owner runs
//! on their own machine/VPS. KAIROS miners POST anonymous usage snapshots to it
//! (only when the owner has enabled telemetry in their private `dev/dev.toml`), and
//! this serves a live dashboard: how many miners are running, which coins/pools
//! they mine, on which OS, and total fleet hashrate.
//!
//! It stores nothing personal — just the anonymous fields the miner sends
//! (random instance id, version, OS, coins, pool hosts, hashrate). Events are
//! appended to `kairos-stats.jsonl` for durability and replayed on startup.
//!
//! Run:  kairos-stats [port]        (default 8899; or set KAIROS_STATS_PORT)
//! Point your miners' telemetry endpoint at:  http://<this-host>:<port>/ingest

use serde_json::Value;
use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

const STORE: &str = "kairos-stats.jsonl";
const ACTIVE_WINDOW_SECS: u64 = 900; // an instance is "active" if seen in 15 min

#[derive(Clone)]
struct Record {
    version: String,
    os: String,
    coins: Vec<String>,
    pools: Vec<String>,
    hashrate: f64,
    received_at: u64,
}

type State = HashMap<String, Record>; // instance id -> latest record

fn now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

fn main() {
    let port: u16 = std::env::args()
        .nth(1)
        .or_else(|| std::env::var("KAIROS_STATS_PORT").ok())
        .and_then(|s| s.parse().ok())
        .unwrap_or(8899);

    let state: Arc<Mutex<State>> = Arc::new(Mutex::new(HashMap::new()));
    replay_store(&state);

    let bind = format!("0.0.0.0:{port}");
    let listener = TcpListener::bind(&bind).unwrap_or_else(|e| {
        eprintln!("kairos-stats: cannot bind {bind}: {e}");
        std::process::exit(1);
    });
    println!("KAIROS telemetry server");
    println!("  dashboard : http://<this-host>:{port}/");
    println!("  ingest    : http://<this-host>:{port}/ingest   (point miners here)");
    println!("  store     : {STORE}");

    for stream in listener.incoming() {
        if let Ok(s) = stream {
            let state = state.clone();
            std::thread::spawn(move || handle(s, state));
        }
    }
}

fn replay_store(state: &Arc<Mutex<State>>) {
    if let Ok(content) = std::fs::read_to_string(STORE) {
        let mut g = state.lock().unwrap();
        for line in content.lines() {
            if let Ok(v) = serde_json::from_str::<Value>(line) {
                if let Some((id, rec)) = record_from_json(&v) {
                    g.insert(id, rec);
                }
            }
        }
        println!("  replayed {} instance(s) from {STORE}", g.len());
    }
}

fn record_from_json(v: &Value) -> Option<(String, Record)> {
    let id = v["instance"].as_str()?.to_string();
    let strs = |key: &str| -> Vec<String> {
        v[key].as_array().map(|a| a.iter().filter_map(|x| x.as_str().map(String::from)).collect()).unwrap_or_default()
    };
    Some((
        id,
        Record {
            version: v["version"].as_str().unwrap_or("?").to_string(),
            os: v["os"].as_str().unwrap_or("?").to_string(),
            coins: strs("coins"),
            pools: strs("pools"),
            hashrate: v["hashrate"].as_f64().unwrap_or(0.0),
            received_at: now(),
        },
    ))
}

fn handle(mut stream: TcpStream, state: Arc<Mutex<State>>) {
    let (method, path, body) = match read_request(&stream) {
        Some(r) => r,
        None => return,
    };
    match (method.as_str(), path.as_str()) {
        ("POST", "/ingest") => {
            let mut ok = false;
            if let Ok(v) = serde_json::from_str::<Value>(&body) {
                if let Some((id, rec)) = record_from_json(&v) {
                    // Persist the raw (anonymous) line, then update the live map.
                    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(STORE) {
                        let _ = writeln!(f, "{}", body.trim());
                    }
                    state.lock().unwrap().insert(id, rec);
                    ok = true;
                }
            }
            let payload = if ok { "{\"ok\":true}" } else { "{\"ok\":false}" };
            respond(&mut stream, "200 OK", "application/json", payload);
        }
        ("GET", "/api") => {
            let body = api_json(&state.lock().unwrap());
            respond(&mut stream, "200 OK", "application/json", &body);
        }
        ("GET", "/") | ("GET", "/dashboard") => {
            let html = dashboard(&state.lock().unwrap());
            respond(&mut stream, "200 OK", "text/html; charset=utf-8", &html);
        }
        _ => respond(&mut stream, "404 Not Found", "text/plain", "not found"),
    }
}

fn read_request(stream: &TcpStream) -> Option<(String, String, String)> {
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    reader.read_line(&mut line).ok()?;
    let mut parts = line.split_whitespace();
    let method = parts.next()?.to_string();
    let path = parts.next()?.to_string();
    let mut content_length = 0usize;
    loop {
        let mut h = String::new();
        if reader.read_line(&mut h).ok()? == 0 {
            break;
        }
        let t = h.trim_end();
        if t.is_empty() {
            break;
        }
        if let Some(v) = t.to_ascii_lowercase().strip_prefix("content-length:") {
            content_length = v.trim().parse().unwrap_or(0);
        }
    }
    let mut body = String::new();
    if content_length > 0 && content_length < 1_000_000 {
        let mut buf = vec![0u8; content_length];
        reader.read_exact(&mut buf).ok()?;
        body = String::from_utf8_lossy(&buf).into_owned();
    }
    Some((method, path, body))
}

fn respond(stream: &mut TcpStream, status: &str, ctype: &str, body: &str) {
    let resp = format!(
        "HTTP/1.1 {status}\r\nContent-Type: {ctype}\r\nContent-Length: {}\r\nAccess-Control-Allow-Origin: *\r\nConnection: close\r\n\r\n{body}",
        body.as_bytes().len()
    );
    let _ = stream.write_all(resp.as_bytes());
}

// ─────────────────────────── aggregation ────────────────────────────────────

struct Agg {
    total: usize,
    active: usize,
    hashrate: f64,
    by_coin: Vec<(String, usize)>,
    by_pool: Vec<(String, usize)>,
    by_os: Vec<(String, usize)>,
    by_version: Vec<(String, usize)>,
}

fn aggregate(state: &State) -> Agg {
    let cutoff = now().saturating_sub(ACTIVE_WINDOW_SECS);
    let mut active = 0;
    let mut hashrate = 0.0;
    let mut coin: HashMap<String, usize> = HashMap::new();
    let mut pool: HashMap<String, usize> = HashMap::new();
    let mut os: HashMap<String, usize> = HashMap::new();
    let mut ver: HashMap<String, usize> = HashMap::new();
    for r in state.values() {
        let live = r.received_at >= cutoff;
        if live {
            active += 1;
            hashrate += r.hashrate;
            for c in &r.coins {
                *coin.entry(c.clone()).or_default() += 1;
            }
            for p in &r.pools {
                if !p.is_empty() {
                    *pool.entry(p.clone()).or_default() += 1;
                }
            }
            *os.entry(r.os.clone()).or_default() += 1;
            *ver.entry(r.version.clone()).or_default() += 1;
        }
    }
    let sorted = |m: HashMap<String, usize>| {
        let mut v: Vec<(String, usize)> = m.into_iter().collect();
        v.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
        v
    };
    Agg { total: state.len(), active, hashrate, by_coin: sorted(coin), by_pool: sorted(pool), by_os: sorted(os), by_version: sorted(ver) }
}

fn hr(h: f64) -> String {
    if h >= 1e12 { format!("{:.2} TH/s", h / 1e12) }
    else if h >= 1e9 { format!("{:.2} GH/s", h / 1e9) }
    else if h >= 1e6 { format!("{:.2} MH/s", h / 1e6) }
    else if h >= 1e3 { format!("{:.2} kH/s", h / 1e3) }
    else { format!("{h:.0} H/s") }
}

fn api_json(state: &State) -> String {
    let a = aggregate(state);
    let arr = |v: &[(String, usize)]| -> String {
        let items: Vec<String> = v.iter().map(|(k, n)| format!("{{\"name\":{k:?},\"count\":{n}}}")).collect();
        format!("[{}]", items.join(","))
    };
    format!(
        "{{\"total_instances\":{},\"active\":{},\"hashrate\":{:.0},\"by_coin\":{},\"by_pool\":{},\"by_os\":{},\"by_version\":{}}}",
        a.total, a.active, a.hashrate, arr(&a.by_coin), arr(&a.by_pool), arr(&a.by_os), arr(&a.by_version)
    )
}

fn dashboard(state: &State) -> String {
    let a = aggregate(state);
    let rows = |v: &[(String, usize)], empty: &str| -> String {
        if v.is_empty() {
            return format!("<div class=e>{empty}</div>");
        }
        v.iter()
            .map(|(k, n)| format!("<div class=row><span>{}</span><b>{n}</b></div>", esc(k)))
            .collect::<Vec<_>>()
            .join("")
    };
    format!(
        r#"<!doctype html><html><head><meta charset=utf-8><title>KAIROS · fleet</title>
<meta http-equiv=refresh content=15>
<style>
:root{{--bg:#0a0d12;--pan:#10151d;--pan2:#151b25;--ln:#1d2632;--tx:#e8eefc;--mut:#8493a8;--dim:#5b6878;--acc:#14c8a0}}
*{{box-sizing:border-box}}body{{margin:0;background:var(--bg);color:var(--tx);font-family:Segoe UI,Helvetica,Arial,sans-serif}}
.wrap{{max-width:1000px;margin:0 auto;padding:24px}}
h1{{font-size:18px;letter-spacing:.14em}}h1 span{{color:var(--acc)}}
.sub{{color:var(--dim);font:500 11px/1.5 ui-monospace,monospace;margin-bottom:18px}}
.kpis{{display:grid;grid-template-columns:repeat(4,1fr);gap:12px;margin-bottom:16px}}
.kpi{{background:var(--pan2);border:1px solid var(--ln);border-radius:12px;padding:14px}}
.kpi .k{{color:var(--dim);font:600 10px/1 sans-serif;letter-spacing:.12em;text-transform:uppercase}}
.kpi .v{{margin-top:8px;font:600 22px/1 ui-monospace,monospace;color:var(--acc)}}
.grid{{display:grid;grid-template-columns:repeat(3,1fr);gap:12px}}
.card{{background:var(--pan);border:1px solid var(--ln);border-radius:12px;overflow:hidden}}
.card h2{{margin:0;padding:12px 14px;font:600 11px/1 sans-serif;letter-spacing:.12em;text-transform:uppercase;color:var(--mut);border-bottom:1px solid var(--ln)}}
.row{{display:flex;justify-content:space-between;padding:8px 14px;border-bottom:1px solid #141b25;font:500 13px/1.4 ui-monospace,monospace}}
.row b{{color:var(--acc)}} .row:last-child{{border:none}} .e{{padding:12px 14px;color:var(--dim);font-size:12px}}
.foot{{margin-top:22px;color:var(--dim);font:500 10.5px/1.6 ui-monospace,monospace;text-align:center}}
</style></head><body><div class=wrap>
<h1>▃▅▇◆ KAIROS <span>· fleet telemetry</span></h1>
<div class=sub>anonymous usage · active window {win}s · auto-refresh 15s</div>
<div class=kpis>
  <div class=kpi><div class=k>active miners</div><div class=v>{active}</div></div>
  <div class=kpi><div class=k>total seen</div><div class=v>{total}</div></div>
  <div class=kpi><div class=k>fleet hashrate</div><div class=v>{hrate}</div></div>
  <div class=kpi><div class=k>coins mined</div><div class=v>{ncoins}</div></div>
</div>
<div class=grid>
  <div class=card><h2>By coin</h2>{coins}</div>
  <div class=card><h2>By pool</h2>{pools}</div>
  <div class=card><h2>By OS</h2>{oses}</div>
  <div class=card><h2>By version</h2>{vers}</div>
</div>
<div class=foot>KAIROS telemetry server · data is anonymous (no wallets, no personal data)</div>
</div></body></html>"#,
        win = ACTIVE_WINDOW_SECS,
        active = a.active,
        total = a.total,
        hrate = hr(a.hashrate),
        ncoins = a.by_coin.len(),
        coins = rows(&a.by_coin, "no active miners yet"),
        pools = rows(&a.by_pool, "—"),
        oses = rows(&a.by_os, "—"),
        vers = rows(&a.by_version, "—"),
    )
}

fn esc(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}
