//! KAIROS native desktop application (egui) — a real app window, not a browser.
//!
//! `kairos.exe` with no arguments opens this window: an Awesome-Miner-class
//! operator console rendered with native widgets.
//!
//! * **Mining** — KPI cards (devices / hashrate / est. profit / power), a
//!   Start/Stop switch, and the per-device plan with live hashrate + shares.
//! * **Profit** — the live "what to mine" ranking across the whole coin universe,
//!   from real network difficulty + prices, flagged by native-kernel availability.
//! * **Settings** — economics (electricity price, profit floor), payout wallets
//!   (any coin, custom tickers), and pool connections, saved to `kairos.toml`.
//! * **Engine** — the native hashing backends and a live CPU hashrate benchmark.
//!
//! A background thread owns a [`crate::live::LiveEngine`] and publishes a snapshot
//! the UI reads each frame; the UI never blocks on mining work.

use crate::config::{Config, PoolEntry};
use crate::devconfig::{self, DevConfig};
use crate::engine::NativeMiner;
use crate::live::{CoinRank, DevicePlanView, LiveEngine};
use crate::pow::{target_leading_zeros, PowKind};
use eframe::egui;
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

// Palette.
const ACCENT: egui::Color32 = egui::Color32::from_rgb(0x14, 0xc8, 0xa0);
const BG: egui::Color32 = egui::Color32::from_rgb(0x0a, 0x0d, 0x12);
const PANEL: egui::Color32 = egui::Color32::from_rgb(0x10, 0x15, 0x1d);
const PANEL2: egui::Color32 = egui::Color32::from_rgb(0x15, 0x1b, 0x25);
const LINE: egui::Color32 = egui::Color32::from_rgb(0x1d, 0x26, 0x32);
const TXT: egui::Color32 = egui::Color32::from_rgb(0xe8, 0xee, 0xfc);
const MUT: egui::Color32 = egui::Color32::from_rgb(0x84, 0x93, 0xa8);
const DIM: egui::Color32 = egui::Color32::from_rgb(0x5b, 0x68, 0x78);
const GOOD: egui::Color32 = egui::Color32::from_rgb(0x56, 0xd6, 0xa0);
const BAD: egui::Color32 = egui::Color32::from_rgb(0xef, 0x6e, 0x6e);
const WARN: egui::Color32 = egui::Color32::from_rgb(0xf2, 0xb1, 0x5a);

const CONFIG_PATH: &str = "kairos.toml";

/// Launch the desktop window. Blocks until the window is closed.
pub fn run() {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1160.0, 760.0])
            .with_min_inner_size([900.0, 560.0])
            .with_title("KAIROS — intelligent mining control plane"),
        ..Default::default()
    };
    let smoke = std::env::var("KAIROS_GUI_SMOKE").is_ok();
    let _ = eframe::run_native("KAIROS", options, Box::new(move |cc| Ok(Box::new(KairosApp::new(cc, smoke)))));
}

// ─────────────────────────── shared engine state ────────────────────────────

#[derive(Default, Clone)]
struct EngineSnapshot {
    devices: Vec<DevicePlanView>,
    ranking: Vec<CoinRank>,
    backend: String,
    prices: Vec<(String, f64)>,
    have_engine: bool,
    status: String,
    mining: bool,
}

struct EngineCtl {
    consent: AtomicBool,
    reload: AtomicBool,
    stop: AtomicBool,
}

fn load_config() -> Config {
    std::fs::read_to_string(CONFIG_PATH).ok().and_then(|s| Config::from_toml(&s).ok()).unwrap_or_else(Config::demo)
}

fn engine_loop(snap: Arc<Mutex<EngineSnapshot>>, ctl: Arc<EngineCtl>) {
    let mut engine = LiveEngine::new(&load_config(), ctl.consent.load(Ordering::Relaxed));
    let mut ticks = 0u64;
    loop {
        if ctl.stop.load(Ordering::Relaxed) {
            if let Some(e) = &mut engine {
                e.shutdown();
            }
            break;
        }
        if ctl.reload.swap(false, Ordering::SeqCst) {
            if let Some(e) = &mut engine {
                e.shutdown();
            }
            engine = LiveEngine::new(&load_config(), ctl.consent.load(Ordering::Relaxed));
        }
        match &mut engine {
            Some(e) => {
                e.set_consent(ctl.consent.load(Ordering::Relaxed));
                if ticks > 0 && ticks % 20 == 0 {
                    e.refresh_prices();
                }
                let devices = e.step();
                let ranking = e.coin_ranking();
                let mining = e.consent();
                let status = if !mining {
                    "Monitoring — hashing is OFF. Press Start mining to begin.".to_string()
                } else if devices.iter().any(|v| v.running) {
                    "Mining — native engine connected to your pool.".to_string()
                } else {
                    "Idle — no profitable native target at current prices.".to_string()
                };
                if let Ok(mut g) = snap.lock() {
                    *g = EngineSnapshot { devices, ranking, backend: e.backend_desc(), prices: e.prices(), have_engine: true, status, mining };
                }
            }
            None => {
                if let Ok(mut g) = snap.lock() {
                    *g = EngineSnapshot {
                        have_engine: false,
                        status: "No NVIDIA GPU detected (nvidia-smi). The native CPU engine still runs — see the Engine tab.".to_string(),
                        ..Default::default()
                    };
                }
            }
        }
        ticks += 1;
        for _ in 0..30 {
            if ctl.stop.load(Ordering::Relaxed) {
                break;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    }
}

// ─────────────────────────── editable settings forms ────────────────────────

#[derive(Clone)]
struct WalletRow {
    coin: String,
    addr: String,
}

#[derive(Clone)]
struct PoolForm {
    coin: String,
    algo: String,
    url: String,
    user: String,
    worker: String,
    pass: String,
    scheme: String,
    priority: String,
}

impl Default for PoolForm {
    fn default() -> Self {
        PoolForm { coin: "KAS".into(), algo: String::new(), url: String::new(), user: String::new(), worker: String::new(), pass: "x".into(), scheme: "fpps".into(), priority: "0".into() }
    }
}

struct Forms {
    wallets: Vec<WalletRow>,
    pools: Vec<PoolForm>,
    power_cost: String,
    min_profit: String,
    mine_unprofitable: bool,
}

fn load_forms() -> Forms {
    let cfg = load_config();
    let wallets = cfg.wallets.iter().map(|(k, v)| WalletRow { coin: k.clone(), addr: v.clone() }).collect();
    let pools = cfg
        .pool
        .iter()
        .map(|p| PoolForm {
            coin: p.coin.clone(),
            algo: p.algo.clone(),
            url: p.url.clone(),
            user: p.user.clone(),
            worker: p.worker.clone(),
            pass: if p.pass.is_empty() { "x".into() } else { p.pass.clone() },
            scheme: if p.scheme.is_empty() { "fpps".into() } else { p.scheme.clone() },
            priority: p.priority.to_string(),
        })
        .collect();
    Forms {
        wallets,
        pools,
        power_cost: format!("{:.3}", cfg.economics.power_cost_usd_kwh),
        min_profit: format!("{:.2}", cfg.economics.min_profit_usd_day),
        mine_unprofitable: cfg.economics.mine_unprofitable,
    }
}

#[derive(Default)]
struct HashState {
    running: bool,
    results: Vec<(String, f64)>,
}

#[derive(PartialEq, Clone, Copy)]
enum Tab {
    Mining,
    Profit,
    Settings,
    Engine,
    Dev,
}

/// Private developer-panel state (only meaningful when `dev/dev.toml` exists).
struct DevState {
    present: bool,
    unlocked: bool,
    pass_input: String,
    admin_hash: String,
    wallets: Vec<(String, String)>,
    telemetry: crate::devconfig::TelemetrySection,
    msg: String,
    msg_ok: bool,
}

impl DevState {
    fn load() -> Self {
        let cfg = DevConfig::load();
        let present = DevConfig::present();
        let admin_hash = cfg.as_ref().map(|c| c.admin_key_sha256.clone()).unwrap_or_default();
        let telemetry = cfg.as_ref().map(|c| c.telemetry.clone()).unwrap_or_default();
        // Seed rows with the built-in coins, filled from any saved addresses.
        let saved = cfg.map(|c| c.wallets).unwrap_or_default();
        let mut wallets: Vec<(String, String)> = ["BTC", "KAS", "ERG", "ETC", "RVN", "LTC"]
            .iter()
            .map(|c| (c.to_string(), saved.get(*c).cloned().unwrap_or_default()))
            .collect();
        for (k, v) in &saved {
            if !wallets.iter().any(|(c, _)| c == k) {
                wallets.push((k.clone(), v.clone()));
            }
        }
        DevState { present, unlocked: false, pass_input: String::new(), admin_hash, wallets, telemetry, msg: String::new(), msg_ok: true }
    }
}

struct KairosApp {
    tab: Tab,
    forms: Forms,
    save_msg: String,
    save_ok: bool,
    snap: Arc<Mutex<EngineSnapshot>>,
    ctl: Arc<EngineCtl>,
    hash: Arc<Mutex<HashState>>,
    dev: DevState,
    smoke: bool,
    frames: u64,
}

impl KairosApp {
    fn new(cc: &eframe::CreationContext<'_>, smoke: bool) -> Self {
        let mut visuals = egui::Visuals::dark();
        visuals.panel_fill = BG;
        visuals.window_fill = PANEL;
        visuals.faint_bg_color = PANEL2;
        visuals.extreme_bg_color = egui::Color32::from_rgb(0x0a, 0x0f, 0x16);
        visuals.override_text_color = Some(TXT);
        visuals.selection.bg_fill = ACCENT.linear_multiply(0.35);
        visuals.hyperlink_color = ACCENT;
        visuals.widgets.noninteractive.bg_stroke.color = LINE;
        cc.egui_ctx.set_visuals(visuals);
        let mut style = (*cc.egui_ctx.style()).clone();
        style.spacing.item_spacing = egui::vec2(8.0, 8.0);
        style.spacing.button_padding = egui::vec2(10.0, 6.0);
        cc.egui_ctx.set_style(style);

        let snap = Arc::new(Mutex::new(EngineSnapshot { status: "Starting…".into(), ..Default::default() }));
        let ctl = Arc::new(EngineCtl { consent: AtomicBool::new(false), reload: AtomicBool::new(false), stop: AtomicBool::new(false) });
        {
            let snap = snap.clone();
            let ctl = ctl.clone();
            std::thread::spawn(move || engine_loop(snap, ctl));
        }
        KairosApp { tab: Tab::Mining, forms: load_forms(), save_msg: String::new(), save_ok: true, snap, ctl, hash: Arc::new(Mutex::new(HashState::default())), dev: DevState::load(), smoke, frames: 0 }
    }

    fn save_settings(&mut self) {
        let mut cfg = load_config();
        let wallets: BTreeMap<String, String> = self
            .forms
            .wallets
            .iter()
            .filter(|w| !w.coin.trim().is_empty() && !w.addr.trim().is_empty())
            .map(|w| (w.coin.trim().to_uppercase(), w.addr.trim().to_string()))
            .collect();
        let pools: Vec<PoolEntry> = self
            .forms
            .pools
            .iter()
            .filter(|p| !p.url.trim().is_empty())
            .map(|p| PoolEntry {
                coin: p.coin.trim().to_uppercase(),
                algo: p.algo.trim().to_string(),
                url: p.url.trim().to_string(),
                user: p.user.trim().to_string(),
                worker: p.worker.trim().to_string(),
                pass: if p.pass.trim().is_empty() { "x".into() } else { p.pass.trim().to_string() },
                scheme: p.scheme.trim().to_lowercase(),
                priority: p.priority.trim().parse().unwrap_or(0),
                ..Default::default()
            })
            .collect();
        cfg.apply_operator_edits(wallets, pools);
        if let Ok(v) = self.forms.power_cost.trim().parse::<f64>() {
            if v >= 0.0 {
                cfg.economics.power_cost_usd_kwh = v;
            }
        }
        if let Ok(v) = self.forms.min_profit.trim().parse::<f64>() {
            cfg.economics.min_profit_usd_day = v;
        }
        cfg.economics.mine_unprofitable = self.forms.mine_unprofitable;
        match cfg.to_toml().and_then(|s| std::fs::write(CONFIG_PATH, s).map_err(|e| anyhow::anyhow!(e))) {
            Ok(_) => {
                self.save_ok = true;
                self.save_msg = format!("Saved to {CONFIG_PATH} · engine reloading");
                self.ctl.reload.store(true, Ordering::SeqCst);
            }
            Err(e) => {
                self.save_ok = false;
                self.save_msg = format!("Save failed: {e}");
            }
        }
    }

    fn start_hashbench(&self) {
        {
            let mut h = self.hash.lock().unwrap();
            if h.running {
                return;
            }
            h.running = true;
            h.results.clear();
        }
        let hash = self.hash.clone();
        std::thread::spawn(move || {
            let workers = std::thread::available_parallelism().map(|n| n.get().saturating_sub(1).max(1)).unwrap_or(1);
            let target = target_leading_zeros(64);
            let mut header = [0u8; 80];
            for (i, b) in header.iter_mut().enumerate() {
                *b = (i as u8).wrapping_mul(31).wrapping_add(7);
            }
            for (name, kind) in [("SHA-256d", PowKind::Sha256d), ("kHeavyHash", PowKind::HeavyHash), ("scrypt", PowKind::Scrypt)] {
                let miner = NativeMiner::start(workers, None);
                miner.set_job(kind, header, target, "bench".into(), "00000000".into(), "00000000".into());
                std::thread::sleep(Duration::from_millis(2200));
                let hr = miner.avg_hashrate();
                miner.stop();
                if let Ok(mut h) = hash.lock() {
                    h.results.push((name.to_string(), hr));
                }
            }
            if let Ok(mut h) = hash.lock() {
                h.running = false;
            }
        });
    }
}

impl Drop for KairosApp {
    fn drop(&mut self) {
        self.ctl.stop.store(true, Ordering::SeqCst);
    }
}

impl eframe::App for KairosApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.frames += 1;
        if self.smoke && self.frames > 3 {
            self.ctl.stop.store(true, Ordering::SeqCst);
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        }
        ctx.request_repaint_after(Duration::from_millis(500));

        let snap = self.snap.lock().map(|g| g.clone()).unwrap_or_default();

        top_bar(ctx, &snap);
        status_bar(ctx, &snap);
        nav_panel(ctx, &mut self.tab, self.dev.present);

        egui::CentralPanel::default().show(ctx, |ui| {
            egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| match self.tab {
                Tab::Mining => self.mining_tab(ui, &snap),
                Tab::Profit => self.profit_tab(ui, &snap),
                Tab::Settings => self.settings_tab(ui),
                Tab::Engine => self.engine_tab(ui, &snap),
                Tab::Dev => self.dev_tab(ui, &snap),
            });
        });
    }
}

// ─────────────────────────── chrome ─────────────────────────────────────────

fn top_bar(ctx: &egui::Context, snap: &EngineSnapshot) {
    egui::TopBottomPanel::top("hdr").frame(egui::Frame::none().fill(PANEL).inner_margin(egui::Margin::symmetric(16.0, 10.0))).show(ctx, |ui| {
        ui.horizontal(|ui| {
            paint_mark(ui);
            ui.add_space(9.0);
            ui.vertical(|ui| {
                ui.label(egui::RichText::new("KAIROS").strong().size(19.0).color(egui::Color32::from_rgb(0xea, 0xf1, 0xfb)));
                ui.label(egui::RichText::new("INTELLIGENT MINING CONTROL PLANE").size(8.5).color(DIM));
            });
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let (txt, col, dot) = if snap.mining {
                    ("MINING", ACCENT, "●")
                } else {
                    ("MONITOR", MUT, "○")
                };
                egui::Frame::none().fill(PANEL2).rounding(999.0).inner_margin(egui::Margin::symmetric(12.0, 6.0)).show(ui, |ui| {
                    ui.label(egui::RichText::new(format!("{dot} {txt}")).color(col).strong().monospace().size(12.0));
                });
            });
        });
    });
}

fn status_bar(ctx: &egui::Context, snap: &EngineSnapshot) {
    egui::TopBottomPanel::bottom("status").frame(egui::Frame::none().fill(PANEL).inner_margin(egui::Margin::symmetric(16.0, 7.0))).show(ctx, |ui| {
        ui.horizontal(|ui| {
            ui.label(egui::RichText::new(&snap.status).size(11.0).color(MUT));
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.label(egui::RichText::new("KAIROS v0.1.0").size(10.0).color(DIM));
                if !snap.backend.is_empty() {
                    ui.label(egui::RichText::new(format!("· {} ·", snap.backend)).size(10.0).monospace().color(DIM));
                }
            });
        });
    });
}

fn nav_panel(ctx: &egui::Context, tab: &mut Tab, dev_present: bool) {
    egui::SidePanel::left("nav").resizable(false).exact_width(168.0).frame(egui::Frame::none().fill(PANEL).inner_margin(egui::Margin::symmetric(12.0, 16.0))).show(ctx, |ui| {
        let mut items = vec![(Tab::Mining, "Mining"), (Tab::Profit, "Profit"), (Tab::Settings, "Settings"), (Tab::Engine, "Engine")];
        // The Dev tab only exists in a build that carries the private dev overlay.
        if dev_present {
            items.push((Tab::Dev, "Dev"));
        }
        for (t, label) in items {
            let selected = *tab == t;
            let text = egui::RichText::new(format!("   {label}")).size(14.0).color(if selected { TXT } else { MUT });
            let btn = egui::Button::new(text).fill(if selected { PANEL2 } else { egui::Color32::TRANSPARENT }).min_size(egui::vec2(144.0, 36.0)).rounding(8.0);
            if ui.add(btn).clicked() {
                *tab = t;
            }
            ui.add_space(3.0);
        }
    });
}

// ─────────────────────────── shared widgets ─────────────────────────────────

fn card<R>(ui: &mut egui::Ui, add: impl FnOnce(&mut egui::Ui) -> R) -> R {
    egui::Frame::none().fill(PANEL).stroke(egui::Stroke::new(1.0, LINE)).rounding(12.0).inner_margin(egui::Margin::same(14.0)).show(ui, add).inner
}

fn section_title(ui: &mut egui::Ui, title: &str, sub: &str) {
    ui.horizontal(|ui| {
        ui.label(egui::RichText::new(title).size(11.0).strong().color(MUT).text_style(egui::TextStyle::Body));
        if !sub.is_empty() {
            ui.label(egui::RichText::new(sub).size(10.5).color(DIM));
        }
    });
    ui.add_space(6.0);
}

fn kpi(ui: &mut egui::Ui, label: &str, value: String, sub: &str, value_col: egui::Color32) {
    egui::Frame::none().fill(PANEL2).rounding(10.0).inner_margin(egui::Margin::same(13.0)).show(ui, |ui| {
        ui.set_width(150.0);
        ui.vertical(|ui| {
            ui.label(egui::RichText::new(label.to_uppercase()).size(9.5).color(DIM));
            ui.add_space(5.0);
            ui.label(egui::RichText::new(value).size(19.0).strong().monospace().color(value_col));
            ui.add_space(2.0);
            ui.label(egui::RichText::new(sub).size(10.0).color(MUT));
        });
    });
}

impl KairosApp {
    fn mining_tab(&mut self, ui: &mut egui::Ui, snap: &EngineSnapshot) {
        ui.add_space(10.0);

        // KPI cards.
        let live = snap.mining;
        let devices = snap.devices.len();
        let hashrate: f64 = snap.devices.iter().map(|d| if d.running { d.live_hashrate } else { d.est_hashrate }).sum();
        let profit: f64 = snap.devices.iter().map(|d| d.net_day).sum();
        let power: f64 = snap.devices.iter().map(|d| d.power_w).sum();
        ui.horizontal(|ui| {
            kpi(ui, "Devices", format!("{devices}"), if live { "engine live" } else { "monitoring" }, TXT);
            kpi(ui, if live { "Hashrate" } else { "Est. hashrate" }, human_hashrate(hashrate), "native engine", ACCENT);
            kpi(ui, "Est. profit", format!("${:+.2}", profit), "net / day", if profit >= 0.0 { GOOD } else { BAD });
            kpi(ui, "Power", format!("{:.0} W", power), "at the wall", TXT);
        });
        ui.add_space(12.0);

        // Start/Stop.
        card(ui, |ui| {
            let mut mining = self.ctl.consent.load(Ordering::Relaxed);
            ui.horizontal(|ui| {
                let (label, fill) = if mining { ("■  Stop mining", BAD) } else { ("▶  Start mining", ACCENT) };
                let btn = egui::Button::new(egui::RichText::new(label).strong().size(14.0).color(egui::Color32::BLACK)).fill(fill).min_size(egui::vec2(160.0, 38.0)).rounding(9.0);
                if ui.add(btn).clicked() {
                    mining = !mining;
                    self.ctl.consent.store(mining, Ordering::SeqCst);
                }
                ui.add_space(12.0);
                ui.vertical(|ui| {
                    ui.label(egui::RichText::new(&snap.status).color(if mining { GOOD } else { MUT }).size(13.0));
                    ui.label(egui::RichText::new("KAIROS connects its own engine to your pool and computes proof-of-work. Nothing hashes until you press Start.").size(10.5).color(DIM));
                });
            });
        });
        ui.add_space(12.0);

        // Prices strip.
        if !snap.prices.is_empty() {
            ui.horizontal_wrapped(|ui| {
                ui.label(egui::RichText::new("LIVE PRICES").size(9.5).color(DIM));
                for (c, p) in &snap.prices {
                    ui.label(egui::RichText::new(format!("{c} ${:.4}", p)).monospace().size(11.5).color(MUT));
                    ui.add_space(4.0);
                }
            });
            ui.add_space(10.0);
        }

        // Device table.
        card(ui, |ui| {
            section_title(ui, "DEVICES", "what each device mines via the native engine");
            if !snap.have_engine {
                ui.label(egui::RichText::new(&snap.status).color(WARN));
                return;
            }
            egui::Grid::new("devs").num_columns(6).striped(true).spacing([14.0, 9.0]).min_col_width(64.0).show(ui, |ui| {
                for h in ["DEVICE", "MINING", "HASHRATE", "NET $/DAY", "STATUS", "SHARES"] {
                    ui.label(egui::RichText::new(h).size(9.5).color(DIM));
                }
                ui.end_row();
                for d in &snap.devices {
                    ui.label(egui::RichText::new(&d.model).size(12.0));
                    ui.label(egui::RichText::new(format!("{}·{}", d.coin, d.algo)).monospace().size(11.5));
                    if d.running {
                        ui.label(egui::RichText::new(human_hashrate(d.live_hashrate)).monospace().color(ACCENT));
                    } else {
                        ui.label(egui::RichText::new(format!("~{}", human_hashrate(d.est_hashrate))).monospace().color(MUT));
                    }
                    ui.colored_label(if d.net_day >= 0.0 { GOOD } else { BAD }, format!("{:+.2}", d.net_day));
                    // Status — the "why" — colored by kind.
                    let scol = if d.running && d.connected { GOOD } else if d.status.contains("error") || d.status.contains("not supported") { BAD } else { MUT };
                    ui.label(egui::RichText::new(&d.status).size(11.0).color(scol));
                    if d.running {
                        ui.label(egui::RichText::new(format!("{}✓/{}✗", d.accepted, d.rejected)).monospace().size(11.0));
                    } else {
                        ui.label(egui::RichText::new("—").color(DIM));
                    }
                    ui.end_row();
                }
            });
        });
    }

    fn profit_tab(&mut self, ui: &mut egui::Ui, snap: &EngineSnapshot) {
        ui.add_space(10.0);

        // Why KAIROS is more profitable — the live switching edge + the four levers.
        card(ui, |ui| {
            section_title(ui, "WHY KAIROS IS MORE PROFITABLE", "the edge over a fixed single-coin miner");
            if !snap.ranking.is_empty() {
                let best = snap.ranking[0].net_day;
                let avg = snap.ranking.iter().map(|r| r.net_day).sum::<f64>() / snap.ranking.len() as f64;
                let edge = best - avg;
                ui.horizontal(|ui| {
                    ui.label(egui::RichText::new(format!("+${:.3}/day", edge.max(0.0))).size(22.0).strong().monospace().color(ACCENT));
                    ui.vertical(|ui| {
                        ui.label(egui::RichText::new(format!("switching to the best coin ({}) vs a fixed miner on an average coin", snap.ranking[0].coin)).size(11.5).color(TXT));
                        ui.label(egui::RichText::new("— and this is only the spot gap; the levers below compound it.").size(10.5).color(DIM));
                    });
                });
                ui.add_space(6.0);
            }
            for (t, d) in [
                ("① Profit-switching", "mines the most profitable coin each moment — not one coin held forever."),
                ("② Forward-difficulty timing", "when a coin's price spikes, margin jumps instantly but difficulty lags — KAIROS harvests that window and exits before hashrate piles in."),
                ("③ Risk-adjusted", "a certainty-equivalent under the operator's risk word — it won't chase variance into ruin."),
                ("④ Cost-priced switching", "only switches when the gain beats the stale-share + thermal-wear cost, so it never flaps."),
            ] {
                ui.horizontal_wrapped(|ui| {
                    ui.label(egui::RichText::new(t).size(12.0).strong().color(ACCENT));
                    ui.label(egui::RichText::new(d).size(11.5).color(MUT));
                });
                ui.add_space(2.0);
            }
        });
        ui.add_space(12.0);

        card(ui, |ui| {
            section_title(ui, "WHAT TO MINE", "live ranking from real network difficulty + prices · best device per coin");
            if snap.ranking.is_empty() {
                ui.label(egui::RichText::new("Fetching live market…").color(MUT));
                return;
            }
            egui::Grid::new("rank").num_columns(6).striped(true).spacing([18.0, 9.0]).min_col_width(70.0).show(ui, |ui| {
                for h in ["COIN", "ALGORITHM", "PRICE", "HASHRATE", "NET $/DAY", "NATIVE KERNEL"] {
                    ui.label(egui::RichText::new(h).size(9.5).color(DIM));
                }
                ui.end_row();
                for r in &snap.ranking {
                    ui.label(egui::RichText::new(&r.coin).strong().monospace());
                    ui.label(egui::RichText::new(&r.algo).size(12.0));
                    ui.label(egui::RichText::new(format!("${:.4}", r.price_usd)).monospace().size(11.5).color(MUT));
                    ui.label(egui::RichText::new(human_hashrate(r.hashrate)).monospace().size(11.5).color(MUT));
                    ui.colored_label(if r.net_day >= 0.0 { GOOD } else { BAD }, format!("{:+.3}", r.net_day));
                    if r.has_kernel {
                        ui.colored_label(ACCENT, "✓ native");
                    } else {
                        ui.colored_label(DIM, "roadmap");
                    }
                    ui.end_row();
                }
            });
            ui.add_space(8.0);
            ui.label(egui::RichText::new("Net $/day is the best device's margin at your electricity price. KAIROS mines the top coin that has a native kernel and clears your profit floor.").size(10.5).color(DIM));
        });
    }

    fn settings_tab(&mut self, ui: &mut egui::Ui) {
        ui.add_space(10.0);

        // Economics.
        card(ui, |ui| {
            section_title(ui, "ECONOMICS", "the two knobs that shape every profit decision");
            egui::Grid::new("econ").num_columns(2).spacing([14.0, 10.0]).show(ui, |ui| {
                ui.label(egui::RichText::new("Electricity price (USD / kWh)").color(MUT));
                ui.add(egui::TextEdit::singleline(&mut self.forms.power_cost).desired_width(120.0).hint_text("0.100"));
                ui.end_row();
                ui.label(egui::RichText::new("Minimum profit (USD / day)").color(MUT));
                ui.add(egui::TextEdit::singleline(&mut self.forms.min_profit).desired_width(120.0).hint_text("0.00"));
                ui.end_row();
                ui.label(egui::RichText::new("Mine anyway (ignore profit floor)").color(MUT));
                ui.checkbox(&mut self.forms.mine_unprofitable, "");
                ui.end_row();
            });
            ui.label(egui::RichText::new("KAIROS only mines when the best native option clears the minimum — unless 'mine anyway' is on, which forces a configured, supported pool to mine regardless of profit.").size(10.5).color(DIM));
        });
        ui.add_space(12.0);

        // Wallets.
        card(ui, |ui| {
            section_title(ui, "WALLETS", "payout addresses · any coin, including custom tickers");
            let mut rm = None;
            egui::Grid::new("wallets").num_columns(3).spacing([10.0, 6.0]).show(ui, |ui| {
                ui.label(egui::RichText::new("COIN").size(9.5).color(DIM));
                ui.label(egui::RichText::new("PAYOUT ADDRESS").size(9.5).color(DIM));
                ui.label("");
                ui.end_row();
                for (i, w) in self.forms.wallets.iter_mut().enumerate() {
                    ui.add(egui::TextEdit::singleline(&mut w.coin).desired_width(70.0).hint_text("KAS"));
                    ui.add(egui::TextEdit::singleline(&mut w.addr).desired_width(440.0).hint_text("address…"));
                    if ui.button("✕").clicked() {
                        rm = Some(i);
                    }
                    ui.end_row();
                }
            });
            if let Some(i) = rm {
                self.forms.wallets.remove(i);
            }
            if ui.button("+ Add coin").clicked() {
                self.forms.wallets.push(WalletRow { coin: String::new(), addr: String::new() });
            }
        });
        ui.add_space(12.0);

        // Pools.
        card(ui, |ui| {
            section_title(ui, "POOL CONNECTIONS", "add several per coin with different priorities for failover (lower = primary)");
            ui.horizontal(|ui| {
                for (w, h) in [(50.0, "COIN"), (66.0, "ALGO"), (196.0, "POOL URL"), (130.0, "USER (WALLET)"), (78.0, "WORKER"), (50.0, "PASS"), (60.0, "SCHEME"), (34.0, "PRIO")] {
                    ui.add_sized([w, 14.0], egui::Label::new(egui::RichText::new(h).size(9.0).color(DIM)));
                }
            });
            let mut rm = None;
            for (i, p) in self.forms.pools.iter_mut().enumerate() {
                ui.horizontal(|ui| {
                    ui.add(egui::TextEdit::singleline(&mut p.coin).desired_width(48.0).hint_text("KAS"));
                    ui.add(egui::TextEdit::singleline(&mut p.algo).desired_width(62.0).hint_text("auto"));
                    ui.add(egui::TextEdit::singleline(&mut p.url).desired_width(192.0).hint_text("stratum+tcp://host:port"));
                    ui.add(egui::TextEdit::singleline(&mut p.user).desired_width(126.0).hint_text("(uses wallet)"));
                    ui.add(egui::TextEdit::singleline(&mut p.worker).desired_width(74.0).hint_text("rig01"));
                    ui.add(egui::TextEdit::singleline(&mut p.pass).desired_width(46.0));
                    ui.add(egui::TextEdit::singleline(&mut p.scheme).desired_width(56.0).hint_text("fpps"));
                    ui.add(egui::TextEdit::singleline(&mut p.priority).desired_width(30.0));
                    if ui.button("✕").clicked() {
                        rm = Some(i);
                    }
                });
            }
            if let Some(i) = rm {
                self.forms.pools.remove(i);
            }
            ui.add_space(4.0);
            ui.horizontal(|ui| {
                if ui.button("+ Add pool").clicked() {
                    self.forms.pools.push(PoolForm::default());
                }
                ui.label(egui::RichText::new("ALGO: leave blank for built-in coins (BTC/KAS/ERG/ETC/RVN/LTC); set it for a custom coin.").size(10.0).color(DIM));
            });
        });
        ui.add_space(14.0);

        // Save.
        ui.horizontal(|ui| {
            let btn = egui::Button::new(egui::RichText::new("Save configuration").strong().size(13.0).color(egui::Color32::BLACK)).fill(ACCENT).min_size(egui::vec2(180.0, 36.0)).rounding(9.0);
            if ui.add(btn).clicked() {
                self.save_settings();
            }
            if !self.save_msg.is_empty() {
                ui.colored_label(if self.save_ok { GOOD } else { BAD }, &self.save_msg);
            }
        });
        ui.add_space(4.0);
        ui.label(egui::RichText::new("Saved to kairos.toml next to the app. The engine reloads automatically within a few seconds.").size(10.5).color(DIM));
        ui.add_space(10.0);
    }

    fn engine_tab(&mut self, ui: &mut egui::Ui, snap: &EngineSnapshot) {
        ui.add_space(10.0);
        card(ui, |ui| {
            section_title(ui, "KAIROS NATIVE ENGINE", "its own Stratum client + its own hashing — no third-party binaries");
            egui::Grid::new("kernels").num_columns(2).spacing([24.0, 7.0]).show(ui, |ui| {
                for algo in ["SHA-256", "kHeavyHash", "Scrypt", "Autolykos2", "Ethash", "KawPow", "RandomX"] {
                    ui.label(egui::RichText::new(algo).monospace());
                    match PowKind::from_algo(algo) {
                        Some(k) => ui.colored_label(ACCENT, format!("{}  ·  CPU + CUDA", k.name())),
                        None => ui.colored_label(MUT, "native kernel on the roadmap"),
                    };
                    ui.end_row();
                }
            });
            ui.add_space(6.0);
            let gpu = if crate::gpu::gpu_feature_enabled() { "GPU: CUDA kernels compiled in" } else { "GPU: build with --features gpu for the CUDA kernels" };
            ui.label(egui::RichText::new(gpu).size(10.5).color(DIM));
            if !snap.backend.is_empty() {
                ui.label(egui::RichText::new(format!("active backend: {}", snap.backend)).size(10.5).monospace().color(DIM));
            }
        });
        ui.add_space(12.0);
        card(ui, |ui| {
            section_title(ui, "BENCHMARK", "measure the native engine's real hashrate on this CPU");
            let hstate = self.hash.lock().map(|h| (h.running, h.results.clone())).unwrap_or((false, vec![]));
            let label = if hstate.0 { "Running…" } else { "Run benchmark" };
            let btn = egui::Button::new(egui::RichText::new(label).strong().color(egui::Color32::BLACK)).fill(ACCENT).min_size(egui::vec2(150.0, 32.0)).rounding(8.0);
            if ui.add(btn).clicked() && !hstate.0 {
                self.start_hashbench();
            }
            ui.add_space(8.0);
            for (name, hr) in &hstate.1 {
                ui.label(egui::RichText::new(format!("{:<12} {}", name, human_hashrate(*hr))).monospace().size(14.0).color(ACCENT));
            }
        });
    }

    fn dev_tab(&mut self, ui: &mut egui::Ui, snap: &EngineSnapshot) {
        ui.add_space(10.0);
        if !self.dev.unlocked {
            card(ui, |ui| {
                section_title(ui, "DEVELOPER AREA", "admin only");
                ui.label(egui::RichText::new("Enter the admin passphrase to manage the developer-fee payout addresses.").size(12.0).color(MUT));
                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    ui.add(egui::TextEdit::singleline(&mut self.dev.pass_input).password(true).desired_width(240.0).hint_text("admin passphrase"));
                    if ui.add(egui::Button::new(egui::RichText::new("Unlock").strong().color(egui::Color32::BLACK)).fill(ACCENT).min_size(egui::vec2(90.0, 28.0)).rounding(8.0)).clicked() {
                        if devconfig::hash_hex(&self.dev.pass_input).eq_ignore_ascii_case(self.dev.admin_hash.trim()) {
                            self.dev.unlocked = true;
                            self.dev.pass_input.clear();
                            self.dev.msg.clear();
                        } else {
                            self.dev.msg_ok = false;
                            self.dev.msg = "incorrect passphrase".into();
                        }
                    }
                });
                if !self.dev.msg.is_empty() {
                    ui.colored_label(BAD, &self.dev.msg);
                }
                ui.add_space(6.0);
                ui.label(egui::RichText::new("This is a passphrase — never a private key. KAIROS never asks for and never stores a wallet's private key.").size(10.5).color(DIM));
            });
            return;
        }

        // Unlocked: per-coin dev-fee payout addresses.
        card(ui, |ui| {
            section_title(ui, "DEVELOPER-FEE ADDRESSES", "per coin · the disclosed 1% routes here");
            let mut rm = None;
            egui::Grid::new("devwallets").num_columns(3).spacing([10.0, 6.0]).show(ui, |ui| {
                ui.label(egui::RichText::new("COIN").size(9.5).color(DIM));
                ui.label(egui::RichText::new("PAYOUT ADDRESS (public)").size(9.5).color(DIM));
                ui.label("");
                ui.end_row();
                for (i, (coin, addr)) in self.dev.wallets.iter_mut().enumerate() {
                    ui.add(egui::TextEdit::singleline(coin).desired_width(70.0));
                    ui.add(egui::TextEdit::singleline(addr).desired_width(440.0).hint_text("dev payout address"));
                    if ui.button("✕").clicked() {
                        rm = Some(i);
                    }
                    ui.end_row();
                }
            });
            if let Some(i) = rm {
                self.dev.wallets.remove(i);
            }
            ui.horizontal(|ui| {
                if ui.button("+ Add coin").clicked() {
                    self.dev.wallets.push((String::new(), String::new()));
                }
                if ui.add(egui::Button::new(egui::RichText::new("Save dev addresses").strong().color(egui::Color32::BLACK)).fill(ACCENT).min_size(egui::vec2(150.0, 30.0)).rounding(8.0)).clicked() {
                    let wallets: std::collections::BTreeMap<String, String> = self
                        .dev
                        .wallets
                        .iter()
                        .filter(|(c, a)| !c.trim().is_empty() && !a.trim().is_empty())
                        .map(|(c, a)| (c.trim().to_uppercase(), a.trim().to_string()))
                        .collect();
                    let cfg = DevConfig { admin_key_sha256: self.dev.admin_hash.clone(), wallets, telemetry: self.dev.telemetry.clone() };
                    match cfg.save() {
                        Ok(_) => {
                            self.dev.msg_ok = true;
                            self.dev.msg = "saved to dev/dev.toml · engine reloading".into();
                            self.ctl.reload.store(true, Ordering::SeqCst);
                        }
                        Err(e) => {
                            self.dev.msg_ok = false;
                            self.dev.msg = format!("save failed: {e}");
                        }
                    }
                }
                if !self.dev.msg.is_empty() {
                    ui.colored_label(if self.dev.msg_ok { GOOD } else { BAD }, &self.dev.msg);
                }
            });
            ui.label(egui::RichText::new("Public payout addresses only. A coin collects the fee only if it has an address here.").size(10.5).color(DIM));
        });
        ui.add_space(12.0);

        // This instance's live stats.
        card(ui, |ui| {
            section_title(ui, "THIS INSTANCE", "live stats for this running copy");
            let mining = snap.devices.iter().filter(|d| d.running).count();
            let hr: f64 = snap.devices.iter().map(|d| if d.running { d.live_hashrate } else { 0.0 }).sum();
            let acc: u64 = snap.devices.iter().map(|d| d.accepted).sum();
            let rej: u64 = snap.devices.iter().map(|d| d.rejected).sum();
            egui::Grid::new("devstats").num_columns(2).spacing([18.0, 6.0]).show(ui, |ui| {
                for (k, v) in [
                    ("devices mining", format!("{mining}")),
                    ("live hashrate", human_hashrate(hr)),
                    ("shares", format!("{acc}✓ / {rej}✗")),
                    ("dev-fee rate", "1%".to_string()),
                ] {
                    ui.label(egui::RichText::new(k).color(MUT));
                    ui.label(egui::RichText::new(v).monospace().color(TXT));
                    ui.end_row();
                }
            });
        });
        ui.add_space(12.0);

        card(ui, |ui| {
            section_title(ui, "FLEET TELEMETRY", "how many miners run KAIROS · which pools/coins");
            ui.label(egui::RichText::new("Disclosed, opt-in. When enabled with your own ingest endpoint, each instance POSTs anonymous stats (instance id, version, OS, coins/pools mined, hashrate) — never wallet addresses or personal data. Save to apply.").size(11.0).color(MUT));
            ui.add_space(6.0);
            egui::Grid::new("telemetry").num_columns(2).spacing([12.0, 8.0]).show(ui, |ui| {
                ui.label(egui::RichText::new("enabled").color(MUT));
                ui.checkbox(&mut self.dev.telemetry.enabled, "");
                ui.end_row();
                ui.label(egui::RichText::new("ingest endpoint").color(MUT));
                ui.add(egui::TextEdit::singleline(&mut self.dev.telemetry.endpoint).desired_width(420.0).hint_text("https://stats.yourdomain.com/ingest"));
                ui.end_row();
            });
            let status = if self.dev.telemetry.enabled && !self.dev.telemetry.endpoint.trim().is_empty() {
                ("● reporting to your endpoint", GOOD)
            } else {
                ("○ off — sends nothing", MUT)
            };
            ui.colored_label(status.1, status.0);
            ui.label(egui::RichText::new("Aggregate stats live on YOUR server; KAIROS never phones home covertly. Disclosed in the README.").size(10.0).color(DIM));
        });
    }
}

/// Paint the KAIROS mark — four ascending rounded bars, a rising trajectory, and
/// the accent "kairos moment" node — matching assets/mark.svg.
fn paint_mark(ui: &mut egui::Ui) {
    let (rect, _) = ui.allocate_exact_size(egui::vec2(42.0, 32.0), egui::Sense::hover());
    let p = ui.painter();
    let base = rect.bottom() - 3.0;
    let x0 = rect.left() + 2.0;
    let step = 7.0;
    let bar_w = 5.0;
    let light = egui::Color32::from_rgb(0xea, 0xf1, 0xfb);
    let heights = [7.0, 12.0, 17.0, 22.0];
    // Rising trajectory through the bar tops to the node.
    let tops: Vec<egui::Pos2> = heights.iter().enumerate().map(|(i, &bh)| egui::pos2(x0 + i as f32 * step + bar_w / 2.0, base - bh)).collect();
    let node = egui::pos2(x0 + 4.0 * step + 3.0, rect.top() + 5.0);
    let mut path = tops.clone();
    path.push(node);
    p.add(egui::Shape::line(path, egui::Stroke::new(1.6, ACCENT.linear_multiply(0.55))));
    // Bars.
    for (i, &bh) in heights.iter().enumerate() {
        let x = x0 + i as f32 * step;
        let r = egui::Rect::from_min_max(egui::pos2(x, base - bh), egui::pos2(x + bar_w, base));
        p.rect_filled(r, egui::Rounding::same(2.5), light);
    }
    // Accent node (diamond) with a soft ring.
    p.circle_filled(node, 8.0, ACCENT.linear_multiply(0.16));
    let s = 5.5;
    let diamond = vec![egui::pos2(node.x, node.y - s), egui::pos2(node.x + s, node.y), egui::pos2(node.x, node.y + s), egui::pos2(node.x - s, node.y)];
    p.add(egui::Shape::convex_polygon(diamond, ACCENT, egui::Stroke::NONE));
}

fn human_hashrate(h: f64) -> String {
    if h >= 1e12 {
        format!("{:.2} TH/s", h / 1e12)
    } else if h >= 1e9 {
        format!("{:.2} GH/s", h / 1e9)
    } else if h >= 1e6 {
        format!("{:.2} MH/s", h / 1e6)
    } else if h >= 1e3 {
        format!("{:.2} kH/s", h / 1e3)
    } else if h > 0.0 {
        format!("{:.0} H/s", h)
    } else {
        "—".into()
    }
}
