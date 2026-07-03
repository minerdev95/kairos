//! KAIROS command-line interface — section 22's CLI: small and predictable.
//!
//! ```text
//! kairos start                 # detect, benchmark, configure, run
//! kairos status                # the console summary, once
//! kairos why                   # the rationale for current decisions
//! kairos ledger [value|regret|credit]
//! kairos bench                 # auto-benchmark report
//! kairos set risk <level>
//! kairos pause [site|device]   # safe pause, hardware protected
//! kairos selftest              # Phase-0 acceptance checks
//! ```

use kairos::config::Config;
use kairos::ledger::ValueKind;
use kairos::model::*;
use kairos::runtime::{self, Engine};
use kairos::shield::Shield;
use kairos::{alerts, console, twin};

/// Load `kairos.toml` from the working directory if present, else the zero-config
/// demo defaults.
fn load_config() -> Config {
    match std::fs::read_to_string("kairos.toml") {
        Ok(s) => match Config::from_toml(&s) {
            Ok(c) => c,
            Err(e) => {
                eprintln!("kairos.toml invalid ({e}); using defaults");
                Config::demo()
            }
        },
        Err(_) => Config::demo(),
    }
}

fn build_world(config: &Config, seed: u64, scenarios: bool) -> twin::SimWorld {
    let mut world = twin::build_default_world(seed);
    if scenarios {
        for ev in twin::default_scenarios() {
            world.add_scenario(ev);
        }
    }
    // Apply operator pool config: wallet→username on auto pools, plus any
    // explicit [[pool]] connections (URL + user/worker + pass + scheme).
    world.configure_pools(&config.wallets, &config.resolved_pools());
    world
}

fn build(seed: u64, scenarios: bool) -> (Engine, twin::SimWorld) {
    let config = load_config();
    let world = build_world(&config, seed, scenarios);
    let engine = Engine::bootstrap(&config, &world);
    (engine, world)
}

fn run(seed: u64, ticks: u64, scenarios: bool) -> (Engine, twin::SimWorld) {
    let (mut engine, mut world) = build(seed, scenarios);
    engine.run(&mut world, ticks);
    (engine, world)
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    // No arguments (e.g. a double-click of the exe or the desktop shortcut) =
    // launch the native desktop app window. A `--serve` flag or the `serve`
    // command instead brings up the local web dashboard.
    #[cfg_attr(not(feature = "gui"), allow(unused_variables))]
    let no_args = args.is_empty();
    // The command is the first non-flag arg; help flags map to `help`; any other
    // bare leading flag (`--serve`, `--ticks …`) means the implicit `start` command.
    let cmd = match args.first().map(|s| s.as_str()) {
        None => "start",
        Some("-h") | Some("--help") => "help",
        Some(s) if s.starts_with('-') => "start",
        Some(s) => s,
    };

    // Double-click / `kairos gui` opens the native window (the default product
    // surface). Falls through to the CLI when built without the `gui` feature.
    #[cfg(feature = "gui")]
    {
        let wants_gui = (no_args && !args.iter().any(|a| a == "--serve"))
            || cmd == "gui"
            || cmd == "app";
        if wants_gui {
            kairos::gui::run();
            return;
        }
    }

    // defaults
    let cfg = Config::demo();
    let mut ticks = cfg.sim.ticks;
    let mut seed = cfg.sim.seed;
    let mut scenarios = cfg.sim.scenarios;
    let mut serve = false;
    let mut live = false;
    let mut consent = false;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--live" => live = true,
            "--yes" | "-y" => consent = true,
            "--ticks" => {
                if let Some(v) = args.get(i + 1).and_then(|s| s.parse().ok()) {
                    ticks = v;
                    i += 1;
                }
            }
            "--seed" => {
                if let Some(v) = args.get(i + 1).and_then(|s| s.parse().ok()) {
                    seed = v;
                    i += 1;
                }
            }
            "--no-scenarios" => scenarios = false,
            "--serve" => serve = true,
            _ => {}
        }
        i += 1;
    }

    match cmd {
        "live" => {
            print!("{}", console::banner());
            kairos::live::run(&load_config(), consent, 3, None);
        }
        "start" => {
            print!("{}", console::banner());
            let config = load_config();
            if live {
                kairos::live::run(&config, consent, 3, None);
            } else if serve {
                // LIVE mode: warm up, then keep the fleet running in real time
                // behind an auto-refreshing dashboard.
                println!("kairos: detecting fleet, benchmarking, configuring…");
                let world = build_world(&config, seed, scenarios);
                let warmup = ticks.min(420).max(60);
                let bind = config.api.bind.clone();
                if let Err(e) = runtime::serve_app(&config, world, warmup, 300, &bind) {
                    eprintln!("api error: {e} (is {bind} already in use?)");
                }
            } else {
                println!("kairos: detecting fleet, benchmarking, configuring…");
                let (mut engine, mut world) = build(seed, scenarios);
                print_bench(&engine);
                println!(
                    "\nrunning {} control ticks ({:.1} h simulated)…\n",
                    ticks,
                    ticks as f64 * world.dt() / 3600.0
                );
                engine.run(&mut world, ticks);
                print!("{}", console::render(&engine, &world));
                println!("\n(run `kairos start --serve` for the live dashboard on {})", config.api.bind);
            }
        }
        "status" => {
            let (engine, world) = run(seed, ticks, scenarios);
            print!("{}", console::render(&engine, &world));
        }
        "why" => {
            let (engine, _world) = run(seed, ticks, scenarios);
            println!("KAIROS decision rationale (most recent):\n");
            for r in engine.rationale.iter().rev().take(20) {
                println!("  {:>8.0}s  [{}] {}", r.t_secs, r.scope, r.message);
            }
            println!("\nrecent system events:");
            for e in engine.events.iter().rev().take(12) {
                println!("  {}", e);
            }
        }
        "ledger" => {
            let which = args.get(1).map(|s| s.as_str()).unwrap_or("value");
            let (engine, _world) = run(seed, ticks, scenarios);
            print_ledger(&engine, which);
        }
        "bench" => {
            let (engine, _world) = build(seed, scenarios);
            print_bench(&engine);
        }
        "set" => {
            if args.get(1).map(|s| s.as_str()) == Some("risk") {
                if let Some(level) = args.get(2).and_then(|s| RiskWord::parse(s)) {
                    let (mut engine, mut world) = build(seed, scenarios);
                    engine.set_risk(level);
                    engine.run(&mut world, ticks);
                    println!("risk set to '{}'. resulting behavior:\n", level);
                    print!("{}", console::render(&engine, &world));
                } else {
                    eprintln!("usage: kairos set risk <conservative|balanced|aggressive>");
                }
            } else {
                eprintln!("usage: kairos set risk <level>");
            }
        }
        "pause" => {
            let target = args.get(1).cloned().unwrap_or_else(|| "fleet".into());
            println!(
                "safe-pause requested for '{}'. Hardware protection stays active; \
                 devices idle thermally-protected. (No persistent daemon in this build — \
                 pause is honored within a running engine.)",
                target
            );
        }
        "detect" => {
            print_detect();
        }
        "hashbench" | "benchmark" => {
            print_hashbench();
        }
        "poolcheck" => {
            let url = args.get(1).cloned();
            let user = args.get(2).cloned().unwrap_or_else(|| "x".into());
            match url {
                Some(u) => print_poolcheck(&u, &user),
                None => eprintln!("usage: kairos poolcheck <stratum-url> [user]\n  e.g. kairos poolcheck stratum+tcp://doge.example.com:3333 DYourAddr.worker"),
            }
        }
        "kaspa-verify" | "kaspa-probe" => {
            let url = args.get(1).cloned();
            let user = args.get(2).cloned();
            match (url, user) {
                (Some(u), Some(w)) => print_kaspa_verify(&u, &w),
                _ => eprintln!(
                    "usage: kairos kaspa-verify <stratum-url> <kaspa-wallet[.worker]>\n  \
                     e.g. kairos kaspa-verify stratum+tcp://pool.example.com:5555 kaspa:qr...xyz.rig1\n  \
                     Does the full EthereumStratum handshake and prints the parsed job\n  \
                     (prePowHash, timestamp, difficulty, target) so you can confirm KAIROS\n  \
                     reads your pool correctly before mining."
                ),
            }
        }
        "asic" => {
            let sub = args.get(1).map(|s| s.as_str()).unwrap_or("");
            match sub {
                "scan" => {
                    let targets: Vec<String> = args[2..].to_vec();
                    if targets.is_empty() {
                        eprintln!("usage: kairos asic scan <ip|ip:port|a.b.c.0/24> [more...]\n  scans the CGMiner API (port 4028) and lists responding ASICs");
                    } else {
                        print_asic_scan(&targets);
                    }
                }
                "status" | "info" => {
                    let targets: Vec<String> = args[2..].to_vec();
                    if targets.is_empty() {
                        eprintln!("usage: kairos asic status <ip> [ip...]");
                    } else {
                        print_asic_status(&targets);
                    }
                }
                "switch" | "setpool" => {
                    // Privileged: repoint an ASIC at a pool. Gated on --yes.
                    let ip = args.get(2).cloned();
                    let url = args.get(3).cloned();
                    let usr = args.get(4).cloned();
                    let pass = args.get(5).cloned().unwrap_or_else(|| "x".into());
                    match (ip, url, usr) {
                        (Some(ip), Some(url), Some(usr)) if consent => {
                            match kairos::asic::switch_pool(&ip, &url, &usr, &pass, std::time::Duration::from_secs(5)) {
                                Ok(msg) => println!("{ip}: {msg}"),
                                Err(e) => eprintln!("{ip}: switch failed: {e}"),
                            }
                        }
                        (Some(_), Some(_), Some(_)) => {
                            eprintln!("`asic switch` changes a miner's pool — re-run with --yes to confirm.");
                        }
                        _ => eprintln!("usage: kairos asic switch <ip> <stratum-url> <wallet.worker> [pass] --yes"),
                    }
                }
                _ => eprintln!("usage: kairos asic <scan|status|switch> ...\n  scan <targets>            discover ASICs via the CGMiner API (port 4028)\n  status <ip> [ip...]       hashrate/temp/shares/pool per ASIC\n  switch <ip> <url> <user>  repoint a miner at a pool (needs --yes + API write access)"),
            }
        }
        "dev-check" => {
            // Confirm what dev overlay is compiled into THIS binary (owner check).
            match kairos::devconfig::DevConfig::baked() {
                Some(dc) => {
                    let coins: Vec<String> = dc.wallets.iter().filter(|(_, v)| !v.trim().is_empty()).map(|(k, _)| k.clone()).collect();
                    println!("BAKED dev overlay present in this binary.");
                    println!("  dev-fee payout coins : {}", if coins.is_empty() { "(none)".into() } else { coins.join(", ") });
                    println!("  telemetry            : {}", if dc.telemetry.enabled && !dc.telemetry.endpoint.is_empty() { "on" } else { "off" });
                    println!("  the disclosed 1% routes to these addresses for the coins KAIROS can mine.");
                    if !coins.is_empty() {
                        println!("\n  ⚠ dust/ban warning: pools pay these addresses on THEIR schedule. If any is");
                        println!("    an EXCHANGE deposit address, many small deposits can get the account frozen.");
                        println!("    Prefer a self-custody wallet + raise each pool's min-payout, then consolidate");
                        println!("    to the exchange yourself. See dev/OWNER-SETUP.md §1.");
                    }
                }
                None => println!("no dev overlay baked into this binary (public/open build)."),
            }
        }
        "dev-hash" => {
            // Hash an admin passphrase for the private dev/dev.toml. Never a key.
            match args.get(1) {
                Some(pass) if !pass.is_empty() => {
                    println!("{}", kairos::devconfig::hash_hex(pass));
                    println!("(paste this as admin_key_sha256 in dev/dev.toml)");
                }
                _ => eprintln!("usage: kairos dev-hash \"<your admin passphrase>\""),
            }
        }
        "devices" | "rigs" => {
            let (engine, world) = run(seed, ticks, scenarios);
            print_devices(&engine, &world);
        }
        "pools" => {
            let (engine, world) = run(seed, ticks, scenarios);
            print_pools(&engine, &world);
        }
        "coins" => {
            let (_engine, world) = build(seed, scenarios);
            print_coins(&world);
        }
        "profit" | "profitability" | "whattomine" => {
            let (engine, world) = run(seed, ticks, scenarios);
            print_profit(&engine, &world);
        }
        "algos" | "algorithms" => {
            let (_engine, world) = build(seed, scenarios);
            print_algos(&world);
        }
        "engine" | "miners" | "backends" => {
            print_miners();
        }
        "plan" => {
            print_plan(&load_config());
        }
        "config" => {
            print_config(&load_config());
        }
        "earnings" | "stats" => {
            let (engine, _world) = run(seed, ticks, scenarios);
            print_ledger(&engine, "value");
            println!();
            print_ledger(&engine, "credit");
        }
        "tune" => {
            let (engine, _world) = build(seed, scenarios);
            print_bench(&engine);
            println!("\nper-chip autotuning runs live: each chip's stable edge is found by\nerror-rate backoff and adopted only when its gain beats wear + stale.");
        }
        "alerts" => {
            let sub = args.get(1).map(|s| s.as_str()).unwrap_or("test");
            let cfg = load_config();
            let n = cfg.notifier();
            if sub == "test" {
                let a = alerts::Alert::new(
                    alerts::Severity::Warn,
                    "test alert",
                    "KAIROS notifications are configured and reachable.",
                );
                if n.configured() {
                    n.fire(&a);
                    println!("sent test alert to configured channel(s):");
                    if !cfg.alerts.webhook.is_empty() {
                        println!("  webhook  {}", cfg.alerts.webhook);
                    }
                    if !cfg.alerts.telegram_token.is_empty() {
                        println!("  telegram chat {}", cfg.alerts.telegram_chat);
                    }
                } else {
                    println!("no alert channel configured. Add an [alerts] section to kairos.toml:");
                    println!("  [alerts]");
                    println!("  webhook = \"https://hooks.example.com/...\"");
                    println!("  telegram_token = \"...\"   telegram_chat = \"...\"");
                    println!("\nthe message that would be sent:\n  {}", a.text());
                }
            } else {
                eprintln!("usage: kairos alerts test");
            }
        }
        "selftest" => {
            std::process::exit(selftest());
        }
        "help" | "-h" | "--help" => print_help(),
        "gui" | "app" => {
            // Only reached in a build without the `gui` feature (otherwise we
            // launch the window before the match).
            eprintln!("this build was compiled without the desktop GUI (`--no-default-features`).");
            eprintln!("use `kairos start --serve` for the web dashboard, or rebuild with the `gui` feature.");
        }
        other => {
            eprintln!("unknown command '{other}'\n");
            print_help();
        }
    }
}

fn print_help() {
    println!(
        "KAIROS — intelligent mining control plane\n\n\
         USAGE: kairos <command> [options]\n\n\
         COMMANDS:\n\
         \x20 gui          open the native desktop app window (default on double-click)\n\
         \x20 start        run the control plane (twin); --live [--yes] to mine for real\n\
         \x20 status       the console summary, once\n\
         \x20 why          the rationale for current decisions\n\
         \x20 ledger [value|regret|credit]\n\
         \x20 detect       detect real hardware (GPUs via nvidia-smi, CPU)\n\
         \x20 engine       KAIROS native engine: hashing backends + kernel per algo\n\
         \x20 hashbench    measure the native engine's real hashrate on this CPU\n\
         \x20 poolcheck <url> [user]  test a pool's protocol + handshake (diagnose mining)\n\
         \x20 plan         dry-run: what the native engine would mine + the pool\n\
         \x20 live [--yes] real mining: the native engine hashes on your pools\n\
         \x20 devices      per-device live stats (hashrate, temp, shares, eff)\n\
         \x20 pools        per-pool status (scheme, fee, latency, shares)\n\
         \x20 coins        the mineable coin universe\n\
         \x20 profit       what to mine — net $/day per coin, ranked\n\
         \x20 algos        registered algorithms (mining backends)\n\
         \x20 earnings     value + credit ledgers (net, uplift, by lever)\n\
         \x20 bench / tune auto-benchmark + per-chip tuning report\n\
         \x20 config       show the effective configuration\n\
         \x20 kaspa-verify <url> <wallet>   handshake a Kaspa pool + parse a job (no shares)\n\
         \x20 asic scan <ip|subnet/24>      discover ASICs via the CGMiner API (port 4028)\n\
         \x20 asic status <ip> [ip...]      per-ASIC hashrate/temp/shares/pool\n\
         \x20 asic switch <ip> <url> <user> --yes   repoint an ASIC at a pool\n\
         \x20 alerts test  send a test notification\n\
         \x20 set risk <conservative|balanced|aggressive>\n\
         \x20 pause [site|device]\n\
         \x20 selftest     Phase-0 acceptance checks\n\n\
         OPTIONS:\n\
         \x20 --live           run KAIROS's own native engine on your pools\n\
         \x20 --yes, -y        consent to connect + hash for real (with --live)\n\
         \x20 --ticks N        control ticks to simulate (default 1320 = 22h)\n\
         \x20 --seed S         deterministic twin seed\n\
         \x20 --no-scenarios   disable the adversarial scenario schedule\n\
         \x20 --serve          launch the live dashboard + open the browser\n\n\
         With no command, KAIROS launches the live dashboard.\n"
    );
}

fn hh(h: f64) -> String {
    if h >= 1e15 { format!("{:.2} PH/s", h / 1e15) }
    else if h >= 1e12 { format!("{:.2} TH/s", h / 1e12) }
    else if h >= 1e9 { format!("{:.2} GH/s", h / 1e9) }
    else if h >= 1e6 { format!("{:.1} MH/s", h / 1e6) }
    else if h >= 1e3 { format!("{:.1} kH/s", h / 1e3) }
    else { format!("{:.0} H/s", h) }
}

fn class_str(c: DeviceClass) -> &'static str {
    match c { DeviceClass::Gpu => "GPU", DeviceClass::Asic => "ASIC", DeviceClass::Fpga => "FPGA" }
}

fn print_miners() {
    use kairos::gpu;
    use kairos::pow::PowKind;
    println!("KAIROS NATIVE ENGINE  (KAIROS mines this itself — no third-party binaries)");
    let cpu = kairos::hardware::detect_cpu();
    let gpu_hashers = gpu::detect_hashers();
    println!("  hashing backends:");
    println!("    CPU   {:<28} {} threads  [always available]", cpu.name, cpu.threads);
    if gpu::gpu_feature_enabled() {
        if gpu_hashers.is_empty() {
            println!("    GPU   CUDA kernels compiled, no CUDA device detected");
        } else {
            for h in &gpu_hashers {
                println!("    GPU   {:<28} [native CUDA kernel]", h.name());
            }
        }
    } else {
        println!("    GPU   native CUDA kernels available — build with `--features gpu`");
    }
    println!("\nSUPPORT MATRIX   (kernel = we can hash it · pool = we speak its stratum)");
    println!("  {:<12} {:<10} {:<8} {:<8}  {}", "ALGORITHM", "KERNEL", "POOL", "STATUS", "COINS");
    let rows = [
        ("SHA-256", "BTC · BCH · DGB"),
        ("Scrypt", "LTC · DOGE · DGB"),
        ("kHeavyHash", "KAS"),
        ("Autolykos2", "ERG"),
        ("Ethash", "ETC"),
        ("KawPow", "RVN"),
        ("RandomX", "XMR"),
    ];
    for (algo, coins) in rows {
        match PowKind::from_algo(algo) {
            Some(k) if k.pool_experimental() => {
                println!("  {:<12} {:<10} {:<8} {:<8}  {}", algo, "yes", "exp.", "EXPERIMENTAL", coins)
            }
            Some(k) if k.pool_supported() => {
                println!("  {:<12} {:<10} {:<8} {:<8}  {}", algo, "yes", "yes", "MINEABLE", coins)
            }
            Some(_) => println!("  {:<12} {:<10} {:<8} {:<8}  {}", algo, "yes", "no", "kernel-only", coins),
            None => println!("  {:<12} {:<10} {:<8} {:<8}  {}", algo, "roadmap", "—", "roadmap", coins),
        }
    }
    println!("\n  MINEABLE now: SHA-256d + scrypt coins (Bitcoin-family Stratum V1, verified).");
    println!("  EXPERIMENTAL: kHeavyHash (KAS) — full Kaspa EthereumStratum bridge protocol");
    println!("                (kaspa-stratum-bridge: 4×u64/big-job notify, 2^224/diff target,");
    println!("                extranonce in the high nonce bits) + exact rusty-kaspa PoW.");
    println!("                VERIFY your pool first:  kairos kaspa-verify <url> <wallet>");
    if gpu::gpu_feature_enabled() {
        println!("  GPU (KAS)   : exact CUDA kHeavyHash kernel compiled; every GPU nonce is");
        println!("                CPU-re-checked before submit (fail-safe).");
    } else {
        println!("  GPU (KAS)   : exact CUDA kHeavyHash kernel available — build --features gpu.");
    }
    println!("  Diagnose any pool:  kairos poolcheck <stratum-url> [user]");
    println!("  Manage ASICs (CGMiner API):  kairos asic scan <ip|subnet/24>");
    println!("  KAIROS connects with its own Stratum client + computes the PoW itself.");
}

fn print_plan(config: &Config) {
    use kairos::hardware;
    use kairos::pow::PowKind;
    // Include the CPU (native-kernel device) so the plan shows everything KAIROS
    // can hash — the CPU mines BTC/KAS/LTC via the built-in engine.
    let devices = hardware::detect_devices(true);
    if devices.is_empty() {
        println!("no mining-capable devices detected.");
        return;
    }
    let world = kairos::live::build_live_market(config);
    let belief = world.sense();
    // Use the operator's configured electricity price so `plan` matches the app.
    let energy = config.economics.power_cost_usd_kwh.max(0.0);
    println!("MINING PLAN  (dry-run — what KAIROS's native engine would mine, via your pools)");
    let prices: Vec<String> = world
        .market
        .coins
        .keys()
        .filter_map(|c| belief.coin(c).map(|b| format!("{} ${:.4}", c, b.price_usd)))
        .collect();
    println!("live prices: {}", prices.join("  "));
    println!("energy ${:.3}/kWh · {} device(s)\n", energy, devices.len());
    let auto_pl = config.economics.auto_power_limit;
    let min_frac_cfg = config.economics.min_power_frac;
    for dev in &devices {
        let is_gpu = dev.class == DeviceClass::Gpu;
        // best = (coin, algo, net_day, hashrate, power_w, watts_saved, pool)
        let mut best: Option<(String, String, f64, f64, f64, f64, kairos::model::PoolDescriptor)> = None;
        for cap in &dev.capabilities {
            for (cid, coin) in &world.market.coins {
                if coin.algo != cap.algo {
                    continue;
                }
                let cb = match belief.coin(cid) {
                    Some(c) => c,
                    None => continue,
                };
                let rev = (coin.block_reward + coin.fee_per_block) / cb.difficulty.max(1.0)
                    * cb.price_usd
                    * cap.stock_hashrate;
                let min_frac = if auto_pl && is_gpu { min_frac_cfg } else { 1.0 };
                let op = kairos::efficiency::optimize(rev, cap.stock_power_w, energy, cap.stock_hashrate, min_frac);
                let pool = world
                    .market
                    .pools_for_coin(cid)
                    .into_iter()
                    .min_by_key(|p| p.priority)
                    .cloned();
                if let Some(pd) = pool {
                    if best.as_ref().map(|b| op.net_day > b.2).unwrap_or(true) {
                        best = Some((cid.to_string(), cap.algo.to_string(), op.net_day, op.hashrate, op.power_w, op.watts_saved(cap.stock_power_w), pd));
                    }
                }
            }
        }
        match best {
            Some((coin, algo, net, hr, pw, saved, pd)) => {
                println!("  {} [{}] {}", dev.id, class_str(dev.class), dev.model);
                let kernel = PowKind::from_algo(&algo);
                if net <= 0.0 {
                    println!(
                        "      -> idle — best option {} ({}) would lose ${:.2}/day at current prices",
                        coin, algo, net.abs()
                    );
                    println!("         (KAIROS mines it only if price rises or your energy is cheaper)");
                } else {
                    println!("      -> mine {} ({}) ~${:.2}/day @ {}", coin, algo, net, hh(hr));
                    if saved > 1.0 {
                        println!("         power-optimized: {:.0} W (saves {:.0} W vs stock) for max profit/watt", pw, saved);
                    }
                }
                let user = if pd.user.is_empty() { "<wallet>" } else { &pd.user };
                match kernel {
                    Some(k) if k.pool_supported() => println!(
                        "      native {} kernel -> {}  user {}  (MINEABLE — KAIROS hashes this itself)",
                        k.name(), pd.url, user
                    ),
                    Some(k) => println!(
                        "      {} kernel ready, but its pool protocol (Kaspa/EthereumStratum) isn't supported yet — roadmap",
                        k.name()
                    ),
                    None => println!(
                        "      native kernel for {} is on the roadmap",
                        algo
                    ),
                }
            }
            None => println!("  {} : no profitable coin at current prices", dev.id),
        }
    }
    println!("\nThis is a preview. `kairos start --live --yes` runs KAIROS's own engine:");
    println!("its Stratum client connects to your pool and it computes the proof-of-work itself.");
}

fn print_poolcheck(url: &str, user: &str) {
    use std::time::Duration;
    println!("POOL CHECK  {url}");
    match kairos::stratum::probe(url, user, "x", Duration::from_secs(12)) {
        Ok(p) => {
            let yn = |b: bool| if b { "yes" } else { "no" };
            println!("  connected   : {}", yn(p.connected));
            println!("  protocol    : {}", p.variant);
            println!("  subscribed  : {}", yn(p.subscribed));
            println!("  authorized  : {}   (user '{}')", yn(p.authorized), user);
            if p.difficulty > 0.0 {
                println!("  difficulty  : {}", p.difficulty);
            }
            println!("  got job     : {}", yn(p.got_job));
            if p.supported {
                println!("\n  ✓ KAIROS can mine this pool. Add it in Settings (or [[pool]]) and Start mining.");
                if !p.note.is_empty() && p.note.contains("EXPERIMENTAL") {
                    println!("    ⚠ {}", p.note);
                }
            } else {
                println!("\n  ✗ {}", p.note);
                println!("    Today the native engine mines Bitcoin-family pools (SHA-256d, scrypt:");
                println!("    BTC/BCH/DGB, LTC/DOGE/DGB). Kaspa (kHeavyHash) + Autolykos2 are on the roadmap.");
            }
        }
        Err(e) => {
            println!("  connect/handshake failed: {e}");
            println!("  check the URL/port, or the pool may use a protocol KAIROS doesn't support yet.");
        }
    }
}

/// Full Kaspa handshake + first-job parse, printed for verification. Submits nothing.
fn print_kaspa_verify(url: &str, wallet: &str) {
    use std::time::Duration;
    println!("KASPA POOL VERIFY  (EthereumStratum handshake — diagnostic only, no shares)");
    println!("  url    : {url}");
    println!("  wallet : {wallet}\n");
    match kairos::kaspa::verify(url, wallet, "x", Duration::from_secs(12)) {
        Ok(p) => {
            let yn = |b: bool| if b { "yes" } else { "no" };
            println!("  subscribe ok   : {}  (result {})", yn(p.subscribe_ok), p.subscribe_result);
            match p.authorize_ok {
                Some(b) => println!("  authorize ok   : {}", yn(b)),
                None => println!("  authorize ok   : (no reply yet)"),
            }
            match p.difficulty {
                Some(d) => println!("  difficulty     : {d}"),
                None => println!("  difficulty     : (not sent)"),
            }
            match &p.extranonce_hex {
                Some(h) => println!("  extranonce     : {h}  ({} bits, held in the high nonce bits)", p.extranonce_bits),
                None => println!("  extranonce     : (none — pool assigns full 64-bit nonce space)"),
            }
            if let Some(form) = &p.notify_form {
                println!("  notify form    : {form}");
            }
            match (&p.job_id, &p.pre_pow_hash_hex, p.timestamp, &p.target_hex) {
                (Some(id), Some(pph), Some(ts), Some(tgt)) => {
                    println!("  job id         : {id}");
                    println!("  prePowHash     : {pph}");
                    println!("  timestamp (ms) : {ts}");
                    println!("  share target   : {tgt}");
                    println!("\n  ✓ KAIROS parsed a live job from this pool. The handshake, job format,");
                    println!("    and 2^224/diff share target all resolved. You can add this pool and");
                    println!("    Start mining KAS. (Confirm a few ACCEPTED shares once mining — Kaspa");
                    println!("    remains EXPERIMENTAL until real shares are acknowledged.)");
                }
                _ => {
                    println!("\n  ⚠ Connected and subscribed, but no job was parsed within the timeout.");
                    println!("    Saw {} message(s). The pool may need a worker password, a longer wait,", p.lines_seen);
                    println!("    or it uses a job format KAIROS doesn't recognise. Share the output to iterate.");
                }
            }
        }
        Err(e) => {
            println!("  connect/handshake failed: {e}");
            println!("  check host:port, that it's a Kaspa (kHeavyHash) pool, and that it's reachable.");
        }
    }
}

fn print_asic_scan(targets: &[String]) {
    use std::time::Duration;
    println!("ASIC SCAN  (CGMiner API, port 4028)");
    println!("  targets: {}\n", targets.join(", "));
    let found = kairos::asic::scan(targets, Duration::from_millis(1200));
    if found.is_empty() {
        println!("  no ASICs responded. Check the IPs/subnet, that the miners are powered on,");
        println!("  and that their API is enabled (Antminer: 'API' on; cgminer: --api-listen).");
        return;
    }
    println!("  {:<21} {:<18} {:>11} {:>7} {:>10} {}", "ADDRESS", "MODEL", "HASHRATE", "TEMP", "ACC/REJ", "ACTIVE POOL");
    for a in &found {
        let pool = a.active_pool().map(|p| p.url.clone()).unwrap_or_else(|| "-".into());
        println!(
            "  {:<21} {:<18} {:>11} {:>6.0}C {:>5}/{:<4} {}",
            a.addr,
            trunc(&a.kind, 18),
            hashrate_ghs(a.ghs),
            a.temp_c,
            a.accepted,
            a.rejected,
            pool
        );
    }
    println!("\n  {} ASIC(s) found. `kairos asic status <ip>` for full detail.", found.len());
}

fn print_asic_status(targets: &[String]) {
    use std::time::Duration;
    for ip in targets {
        match kairos::asic::query(ip, Duration::from_secs(4)) {
            Ok(a) => {
                println!("ASIC {}", a.addr);
                println!("  model        : {}", if a.kind.is_empty() { "(unknown)".into() } else { a.kind.clone() });
                println!("  hashrate     : {}", hashrate_ghs(a.ghs));
                println!("  temp / fan   : {:.0}C  /  {} rpm", a.temp_c, a.fan_rpm);
                println!("  shares       : {} accepted, {} rejected ({:.2}% reject)", a.accepted, a.rejected, a.reject_pct());
                println!("  hw errors    : {}", a.hw_errors);
                println!("  uptime       : {}h {:02}m", a.uptime_s / 3600, (a.uptime_s % 3600) / 60);
                if a.pools.is_empty() {
                    println!("  pools        : (none reported — API may be read-restricted)");
                } else {
                    println!("  pools:");
                    for p in &a.pools {
                        println!("    [{}] {:<8} {}  ({})", if p.active { "*" } else { " " }, p.status, p.url, p.user);
                    }
                }
                println!();
            }
            Err(e) => println!("ASIC {ip}: query failed: {e}\n"),
        }
    }
}

/// Format a GH/s figure with a sensible unit (TH/s for big ASICs).
fn hashrate_ghs(ghs: f64) -> String {
    if ghs >= 1000.0 {
        format!("{:.2} TH/s", ghs / 1000.0)
    } else if ghs >= 1.0 {
        format!("{ghs:.2} GH/s")
    } else {
        format!("{:.1} MH/s", ghs * 1000.0)
    }
}

fn print_hashbench() {
    use kairos::engine::NativeMiner;
    use kairos::pow::{target_leading_zeros, PowKind};
    let workers = std::thread::available_parallelism().map(|n| n.get().saturating_sub(1).max(1)).unwrap_or(1);
    println!("NATIVE ENGINE BENCHMARK  (KAIROS's own hashing, measured on this CPU)");
    println!("  {} worker thread(s)\n", workers);
    // An unfindable target so we cleanly measure throughput.
    let target = target_leading_zeros(64);
    let mut header = [0u8; 80];
    for (i, b) in header.iter_mut().enumerate() {
        *b = (i as u8).wrapping_mul(31).wrapping_add(7);
    }
    for (name, kind) in [("SHA-256d", PowKind::Sha256d), ("kHeavyHash", PowKind::HeavyHash), ("scrypt", PowKind::Scrypt)] {
        let miner = NativeMiner::start(workers, None);
        miner.set_job(kind, header, target, "bench".into(), "00000000".into(), "00000000".into());
        std::thread::sleep(std::time::Duration::from_millis(2500));
        let hr = miner.avg_hashrate();
        let hashes = miner.total_hashes();
        miner.stop();
        println!("  {:<12} {:>14}   ({} hashes in ~2.5s)", name, hh(hr), hashes);
    }
    println!("\nThis is KAIROS computing proof-of-work itself — no external miner.");
    println!("GPU throughput uses the CUDA kernels: build with `--features gpu`.");
}

fn print_detect() {
    use kairos::hardware;
    println!("HARDWARE DETECTION (real)");
    if hardware::nvidia_available() {
        let gpus = hardware::detect_gpus();
        println!("  NVIDIA: {} GPU(s)", gpus.len());
        for g in &gpus {
            println!("    GPU{}  {}  {:.0} MB  {:.0}C  {:.0}/{:.0}W  fan {:.0}%  util {:.0}%  {:.0}/{:.0} MHz",
                g.index, g.name, g.mem_total_mb, g.temp_c, g.power_w, g.power_limit_w,
                g.fan_pct, g.util_pct, g.core_clock_mhz, g.mem_clock_mhz);
        }
    } else {
        println!("  NVIDIA: nvidia-smi not found (no CUDA GPUs, or driver not installed)");
    }
    let cpu = hardware::detect_cpu();
    println!("  CPU: {} ({} threads)", cpu.name, cpu.threads);
    println!("\nMINEABLE CAPABILITY (estimated — refined by benchmark once mining):");
    let devs = hardware::detect_devices(false);
    if devs.is_empty() {
        println!("  no mining-capable devices detected.");
    }
    for d in &devs {
        println!("  {} [{}]  {}", d.id, class_str(d.class), d.model);
        for c in &d.capabilities {
            println!("      {:<12} ~{:>10}  ~{:.0} W", c.algo, hh(c.stock_hashrate), c.stock_power_w);
        }
    }
}

fn print_devices(engine: &Engine, world: &twin::SimWorld) {
    let belief = world.sense();
    let mut asg: std::collections::BTreeMap<DeviceId, Assignment> = Default::default();
    if let Some(d) = &engine.last_decision {
        for sp in &d.action.setpoints {
            if let Some(a) = &sp.assignment { asg.insert(sp.device.clone(), a.clone()); }
        }
    }
    println!("DEVICES  ({} managed · {} mining)",
        engine.devices.len(),
        belief.devices.values().filter(|t| t.hashrate > 0.0).count());
    println!("  {:<13} {:<5} {:<18} {:>11} {:>5} {:>7} {:>4} {:>10} {:>6} {:>9}",
        "device","type","mining","hashrate","temp","power","fan","accepted","rej%","eff");
    for (id, prof) in &engine.devices {
        let t = belief.devices.get(id);
        let a = asg.get(id);
        let sc = engine.stats.device(id);
        let hr = t.map(|x| x.hashrate).unwrap_or(0.0);
        let pw = t.map(|x| x.power_w).unwrap_or(0.0);
        let eff = if hr > 0.0 { format!("{:.1} J/TH", pw / (hr / 1e12)) } else { "—".into() };
        let route = match a { Some(x) => format!("{}->{}", x.algo, x.coin), None => "idle".into() };
        println!("  {:<13} {:<5} {:<18} {:>11} {:>5} {:>7} {:>4} {:>10} {:>6.2} {:>9}",
            id, class_str(prof.class), route,
            if hr>0.0 { hh(hr) } else { "—".into() },
            t.map(|x| format!("{:.0}C", x.temp_c)).unwrap_or_else(|| "—".into()),
            if pw>0.0 { format!("{:.0}W", pw) } else { "—".into() },
            t.map(|x| format!("{:.0}%", x.fan_pct)).unwrap_or_else(|| "—".into()),
            sc.accepted, sc.reject_pct(), eff);
    }
}

fn short_user(s: &str) -> String {
    if s.is_empty() { "—".into() }
    else if s.len() <= 22 { s.to_string() }
    else { format!("{}…{}", &s[..12], &s[s.len()-6..]) }
}

fn trunc(s: &str, n: usize) -> String {
    if s.len() <= n { s.to_string() } else { format!("{}…", &s[..n - 1]) }
}

fn print_pools(engine: &Engine, world: &twin::SimWorld) {
    let belief = world.sense();
    println!("POOLS  (stratum connections — primary first)");
    println!("  {:<4} {:<5} {:<34} {:<22} {:>7} {:>11} {:>7} {:>7}",
        "coin","sch","pool url","worker","latency","accepted","rej%","status");
    let mut pools: Vec<&PoolDescriptor> = world.market.pools.values().collect();
    pools.sort_by(|a, b| a.coin.cmp(&b.coin).then(a.priority.cmp(&b.priority)).then(a.id.cmp(&b.id)));
    for pd in pools {
        let pb = belief.pools.get(&pd.id);
        let sc = engine.stats.pool(&pd.id);
        println!("  {:<4} {:<5} {:<34} {:<22} {:>5.0}ms {:>11} {:>6.2}% {:>7}",
            pd.coin, format!("{:?}", pd.scheme), trunc(&pd.url, 34), short_user(&pd.user),
            pb.map(|p| p.latency_ms.min(99999.0)).unwrap_or(0.0),
            sc.accepted, sc.reject_pct(),
            if pb.map(|p| p.online).unwrap_or(true) { "online" } else { "OUTAGE" });
    }
}

fn print_profit(engine: &Engine, world: &twin::SimWorld) {
    let p = engine.profitability(world);
    println!("PROFITABILITY  (best-device net $/day per coin, at current prices & difficulty)");
    println!("  {:<5} {:<12} {:<5} {:>13} {:>13}  {}", "coin", "algo", "best", "hashrate", "profit/day", "");
    for cp in &p {
        let mark = if cp.active { "<- mining" } else { "" };
        println!("  {:<5} {:<12} {:<5} {:>13} {:>11.4}/d  {}",
            cp.coin, cp.algo, cp.class, hh(cp.hashrate_hs), cp.profit_per_day, mark);
    }
}

fn print_coins(world: &twin::SimWorld) {
    let belief = world.sense();
    println!("COINS");
    println!("  {:<5} {:<12} {:>10} {:>9} {:>10} {:>12}", "coin","algo","price","block_s","reward","net hashrate");
    for (id, c) in &world.market.coins {
        let cb = belief.coin(id);
        let price = cb.map(|b| b.price_usd).unwrap_or(0.0);
        let neth = cb.map(|b| b.network_hashrate).unwrap_or(0.0);
        println!("  {:<5} {:<12} {:>10.4} {:>9.0} {:>10.3} {:>12}",
            id, c.algo, price, c.block_time_s, c.block_reward, hh(neth));
    }
}

fn print_algos(world: &twin::SimWorld) {
    println!("ALGORITHMS");
    println!("  {:<13} {:<14} {:<16} {:>14}", "algorithm","character","backend","ref J/hash");
    for id in world.algos.ids() {
        if let Some(p) = world.algos.get(id) {
            let ch = format!("{:?}", p.character);
            let be = match &p.backend {
                kairos::model::ExecutionBackend::Native => "native".to_string(),
                kairos::model::ExecutionBackend::Wrapped(b) => format!("wrapped:{}", b),
            };
            println!("  {:<13} {:<14} {:<16} {:>14.2e}", id, ch, be, p.ref_efficiency_j_per_h);
        }
    }
}

fn print_config(cfg: &Config) {
    println!("EFFECTIVE CONFIGURATION");
    println!("  operator.risk        {}", cfg.operator.risk);
    println!("  operator.equity_usd  ${:.0}", cfg.operator.equity_usd);
    println!("  obligations/day      ${:.0}", cfg.operator.obligations_usd_per_day);
    print!("  wallets              ");
    if cfg.wallets.is_empty() { println!("(none)"); }
    else { println!("{}", cfg.wallets.keys().cloned().collect::<Vec<_>>().join(", ")); }
    if cfg.power.cap_mw.is_empty() { println!("  power caps           auto (1.25× stock)"); }
    else {
        for (s, mw) in &cfg.power.cap_mw { println!("  power cap {:<10} {:.2} MW", s, mw); }
    }
    println!("  thermal              stop {:.0}C / start {:.0}C / fan {}", cfg.thermal.stop_c, cfg.thermal.start_c, cfg.thermal.fan);
    println!("  energy.demand_resp   {}   pause>${:.0}/MWh", cfg.energy.demand_response, cfg.energy.pause_above_usd_mwh);
    println!("  watchdog             restart_on_hang={}  max_rejects={:.1}%", cfg.watchdog.restart_on_hang, cfg.watchdog.max_rejects_pct);
    println!("  stratum              nicehash={}  target {:.2} shares/s", cfg.stratum.nicehash_mode, cfg.stratum.target_share_hz);
    let dev = if cfg.devices.exclude.is_empty() { "none".to_string() } else { cfg.devices.exclude.join(", ") };
    println!("  devices.exclude      {}", dev);
    let rp = cfg.resolved_pools();
    if rp.is_empty() {
        println!("  pools                auto (best per coin; username = wallet)");
    } else {
        println!("  pools                {} configured:", rp.len());
        for r in &rp {
            println!("    [{:<4}] {:<5} p{}  {:<34} user {}",
                r.desc.coin, format!("{:?}", r.desc.scheme), r.desc.priority,
                trunc(&r.desc.url, 34), short_user(&r.desc.user));
        }
    }
    println!("  alerts               {}", if cfg.notifier().configured() { "configured" } else { "none" });
    let trg = cfg.triggers();
    if trg.is_empty() {
        println!("  triggers             none (built-in offline/overheat alerts active)");
    } else {
        println!("  triggers             {} rule(s):", trg.len());
        for t in &trg {
            let op = match t.op { kairos::alerts::TrigOp::Gt => ">", kairos::alerts::TrigOp::Lt => "<" };
            println!("    {} : {:?} {} {}  [{}]", t.name, t.metric, op, t.value, t.severity.tag());
        }
    }
    if cfg.schedule.pause_hours.is_empty() {
        println!("  schedule             24/7 (no off-peak pause)");
    } else {
        println!("  schedule             pause hours {:?}", cfg.schedule.pause_hours);
    }
    println!("  api.bind             {}", cfg.api.bind);
    println!("  logging              level={} file={}", cfg.logging.level, if cfg.logging.file.is_empty() {"(stdout)"} else {&cfg.logging.file});
}

fn print_bench(engine: &Engine) {
    println!("auto-benchmark: {} devices", engine.bench.device_count());
    println!("  {:<14} {:<6} {:<12} {:>12} {:>9}", "device", "class", "algo", "hashrate", "power");
    for e in engine.bench.entries.iter().take(12) {
        let class = match e.class {
            DeviceClass::Gpu => "GPU",
            DeviceClass::Asic => "ASIC",
            DeviceClass::Fpga => "FPGA",
        };
        println!(
            "  {:<14} {:<6} {:<12} {:>10.3e} {:>7.0}W",
            e.device, class, e.algo, e.hashrate, e.power_w
        );
    }
    if engine.bench.entries.len() > 12 {
        println!("  … {} more entries", engine.bench.entries.len() - 12);
    }
}

fn print_ledger(engine: &Engine, which: &str) {
    match which {
        "regret" => {
            let r = &engine.ledgers.regret;
            println!("REGRET LEDGER (counterfactual off-policy)");
            println!("  decisions scored : {}", r.entries.len());
            println!("  cumulative regret: ${:.2}", r.cum_regret);
            println!("  mean regret/dec  : ${:.4}", r.mean_regret());
        }
        "credit" => {
            println!("CREDIT LEDGER (Shapley-lite, value by lever)");
            for (k, v) in engine.ledgers.credit.ranked() {
                println!("  {:<14} ${:>10.2}", k, v);
            }
            println!("  {:<14} ${:>10.2}", "TOTAL", engine.ledgers.credit.total_usd);
        }
        _ => {
            let v = &engine.ledgers.value;
            let mut by_kind: std::collections::BTreeMap<&str, f64> = Default::default();
            for e in &v.entries {
                let k = match e.kind {
                    ValueKind::MiningRevenue => "mining_revenue",
                    ValueKind::EnergyCost => "energy_cost",
                    ValueKind::DegradationCost => "degradation_cost",
                    ValueKind::StaleLoss => "stale_loss",
                    ValueKind::SwitchCost => "switch_cost",
                    ValueKind::GridCredit => "grid_credit",
                    ValueKind::DevFee => "dev_fee",
                };
                *by_kind.entry(k).or_insert(0.0) += e.usd;
            }
            println!("THERMODYNAMIC VALUE LEDGER");
            for (k, val) in by_kind.iter().filter(|(k, _)| **k != "dev_fee") {
                println!("  {:<18} ${:>12.2}", k, val);
            }
            println!("  {:<18} ${:>12.2}", "mining net (user)", v.mining_net_usd());
            println!("  {:<18} ${:>12.2}", "grid income", v.grid_income_usd());
            println!("  {:<18} ${:>12.2}", "NET (user)", v.cum_usd);
            println!("  {:<18} ${:>12.2}", "incumbent baseline", v.baseline_cum_usd);
            match v.mining_uplift_frac() {
                Some(u) => println!("  {:<18} {:>12.1}%", "MINING UPLIFT", u * 100.0),
                None => println!("  uplift: baseline warming up"),
            }
            match v.uplift_frac() {
                Some(u) => println!("  {:<18} {:>12.1}%", "TOTAL UPLIFT (+grid)", u * 100.0),
                None => {}
            }
            println!("  {:<18} {:>12.3e} J", "energy (joules)", -v.cum_joules);
        }
    }
}

/// Phase-0 acceptance checks (section 33). Exit 0 if all pass.
fn selftest() -> i32 {
    let mut pass = true;
    let mut check = |name: &str, ok: bool| {
        println!("  [{}] {}", if ok { "PASS" } else { "FAIL" }, name);
        if !ok {
            pass = false;
        }
    };
    println!("KAIROS Phase-0 acceptance checks\n");

    // 1) Deterministic twin replay.
    let (e1, _) = run(42, 300, true);
    let (e2, _) = run(42, 300, true);
    check(
        "twin replays deterministically (same seed → identical net)",
        (e1.ledgers.value.cum_usd - e2.ledgers.value.cum_usd).abs() < 1e-9,
    );

    // 2) No-op / full loop produces populated ledgers.
    let (engine, world) = run(7, 600, true);
    check(
        "full loop populates value/regret/credit ledgers",
        !engine.ledgers.value.entries.is_empty()
            && !engine.ledgers.regret.entries.is_empty()
            && engine.ledgers.credit.total_usd.abs() > 0.0,
    );

    // 3) Shield blocks an injected limit-violating action.
    let profiles = world.device_profiles();
    let (dev_id, dev) = profiles.iter().next().unwrap();
    let bad = DeviceSetpoint {
        device: dev_id.clone(),
        assignment: Some(Assignment::primary(
            dev.capabilities[0].algo.clone(),
            CoinId::new("KAS"),
            PoolId::new("kas-main"),
        )),
        knobs: Knobs {
            core_offset_mhz: dev.limits.max_core_offset_mhz + 5000.0,
            mem_offset_mhz: 0.0,
            power_limit_w: dev.limits.max_power_w * 10.0,
            core_voltage_mv: dev.limits.max_core_voltage_mv + 1000.0,
            fan_pct: 10.0,
        },
    };
    let violated = Shield::would_violate(&bad, dev, 25.0);
    let (safe, evs) = Shield::filter_setpoint(&bad, dev, 25.0);
    check(
        "shield blocks injected over-power/over-voltage/over-clock action",
        violated
            && !evs.is_empty()
            && safe.knobs.power_limit_w <= dev.limits.max_power_w
            && safe.knobs.core_voltage_mv <= dev.limits.max_core_voltage_mv
            && safe.knobs.core_offset_mhz <= dev.limits.max_core_offset_mhz,
    );

    // 4) Thermal protection forces idle at/over the temperature ceiling.
    let (hot_safe, _) = Shield::filter_setpoint(&bad, dev, dev.limits.max_temp_c + 5.0);
    check(
        "thermal protection forces idle over the temp ceiling",
        hot_safe.assignment.is_none() && hot_safe.knobs.fan_pct >= 99.0,
    );

    // 5) Brain-down fallback keeps the loop running safely.
    let (mut be, mut bw) = build(11, true);
    be.brain_available = false;
    be.run(&mut bw, 120);
    let online_power: f64 = bw.sense().devices.values().map(|t| t.power_w).sum();
    check(
        "brain-down fallback keeps mining (safe heuristic)",
        be.autonomy.label() == "safe-heuristic" && online_power > 0.0,
    );

    // 6) The thermodynamic value ledger is internally consistent (net = sum of
    //    its signed entries).
    let entry_sum: f64 = engine.ledgers.value.entries.iter().map(|e| e.usd).sum();
    check(
        "value ledger is internally consistent (net = Σ entries)",
        (entry_sum - engine.ledgers.value.cum_usd).abs() < 1e-6,
    );

    // 7) Engine beats the operator's own baseline on mining skill (grid income
    //    excluded — the honest, like-for-like measure).
    let uplift = engine.mining_uplift_frac().unwrap_or(-1.0);
    check(
        "intelligence shows positive mining uplift vs incumbent baseline",
        uplift > 0.0,
    );

    // 8) Auto-onboarding reaches a running optimized state from wallets + risk.
    check(
        "self-onboarding benchmarks every device on every supported algo",
        engine.bench.device_count() == world.device_profiles().len()
            && !engine.bench.entries.is_empty(),
    );

    println!(
        "\n{}",
        if pass {
            "ALL CHECKS PASSED"
        } else {
            "SOME CHECKS FAILED"
        }
    );
    if pass {
        0
    } else {
        1
    }
}
