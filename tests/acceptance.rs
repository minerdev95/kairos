//! Phase-0 / Phase-0.5 acceptance tests (spec section 33), as `cargo test`.
//!
//! These mirror the `kairos selftest` command but run under the test harness so
//! regressions are caught automatically. The shield + degradation fallback are
//! active throughout, per the validation discipline (section 31).

use kairos::config::Config;
use kairos::ledger::ValueKind;
use kairos::model::*;
use kairos::runtime::Engine;
use kairos::shield::Shield;
use kairos::twin;

fn run(seed: u64, ticks: u64, scenarios: bool) -> (Engine, twin::SimWorld) {
    let config = Config::demo();
    let mut world = twin::build_default_world(seed);
    if scenarios {
        for ev in twin::default_scenarios() {
            world.add_scenario(ev);
        }
    }
    let mut engine = Engine::bootstrap(&config, &world);
    engine.run(&mut world, ticks);
    (engine, world)
}

#[test]
fn twin_replays_deterministically() {
    let (a, _) = run(42, 400, true);
    let (b, _) = run(42, 400, true);
    assert!(
        (a.ledgers.value.cum_usd - b.ledgers.value.cum_usd).abs() < 1e-9,
        "same seed must yield identical net (got {} vs {})",
        a.ledgers.value.cum_usd,
        b.ledgers.value.cum_usd
    );
}

#[test]
fn full_loop_populates_all_three_ledgers() {
    let (e, _) = run(7, 600, true);
    assert!(!e.ledgers.value.entries.is_empty(), "value ledger empty");
    assert!(!e.ledgers.regret.entries.is_empty(), "regret ledger empty");
    assert!(
        e.ledgers.credit.total_usd.abs() > 0.0,
        "credit ledger unattributed"
    );
}

#[test]
fn shield_blocks_injected_limit_violation() {
    let (_, world) = run(1, 5, false);
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
            core_offset_mhz: dev.limits.max_core_offset_mhz + 9999.0,
            mem_offset_mhz: 0.0,
            power_limit_w: dev.limits.max_power_w * 10.0,
            core_voltage_mv: dev.limits.max_core_voltage_mv + 2000.0,
            fan_pct: 5.0,
        },
    };
    assert!(Shield::would_violate(&bad, dev, 25.0));
    let (safe, events) = Shield::filter_setpoint(&bad, dev, 25.0);
    assert!(!events.is_empty(), "shield must report the override");
    assert!(safe.knobs.power_limit_w <= dev.limits.max_power_w);
    assert!(safe.knobs.core_voltage_mv <= dev.limits.max_core_voltage_mv);
    assert!(safe.knobs.core_offset_mhz <= dev.limits.max_core_offset_mhz);
}

#[test]
fn thermal_protection_forces_idle() {
    let (_, world) = run(1, 5, false);
    let profiles = world.device_profiles();
    let (dev_id, dev) = profiles.iter().next().unwrap();
    let sp = DeviceSetpoint {
        device: dev_id.clone(),
        assignment: Some(Assignment::primary(
            dev.capabilities[0].algo.clone(),
            CoinId::new("KAS"),
            PoolId::new("kas-main"),
        )),
        knobs: Knobs::stock(dev.limits.max_power_w),
    };
    let (safe, _) = Shield::filter_setpoint(&sp, dev, dev.limits.max_temp_c + 3.0);
    assert!(safe.assignment.is_none(), "over-temp must force idle");
    assert!(safe.knobs.fan_pct >= 99.0, "over-temp must max fans");
}

#[test]
fn brain_down_fallback_keeps_mining() {
    let config = Config::demo();
    let mut world = twin::build_default_world(11);
    for ev in twin::default_scenarios() {
        world.add_scenario(ev);
    }
    let mut engine = Engine::bootstrap(&config, &world);
    engine.brain_available = false;
    engine.run(&mut world, 120);
    assert_eq!(engine.autonomy.label(), "safe-heuristic");
    let power: f64 = world.sense().devices.values().map(|t| t.power_w).sum();
    assert!(power > 0.0, "fallback must keep the fleet mining");
}

#[test]
fn value_ledger_is_internally_consistent() {
    let (e, _) = run(7, 600, true);
    let gross: f64 = e
        .ledgers
        .value
        .entries
        .iter()
        .filter(|x| matches!(x.kind, ValueKind::MiningRevenue))
        .map(|x| x.usd)
        .sum();
    assert!(gross > 0.0, "mining revenue must accrue");
    let entry_sum: f64 = e.ledgers.value.entries.iter().map(|x| x.usd).sum();
    assert!(
        (entry_sum - e.ledgers.value.cum_usd).abs() < 1e-6,
        "ledger net must equal the sum of its signed entries"
    );
}

#[test]
fn positive_mining_uplift_vs_incumbent() {
    // Robust across seeds: the engine must beat a competent profit-switching
    // incumbent on mining skill alone (grid income excluded).
    for seed in [1u64, 2, 3, 7, 11, 42, 99] {
        let (e, _) = run(seed, 1320, true);
        let u = e.mining_uplift_frac().expect("baseline should be warm");
        assert!(
            u > 0.0,
            "seed {seed}: expected positive mining uplift, got {:.2}%",
            u * 100.0
        );
    }
}

#[test]
fn onboarding_benchmarks_every_device_and_algo() {
    let (e, world) = run(1, 1, false);
    assert_eq!(e.bench.device_count(), world.device_profiles().len());
    // Every (device, supported-algo) pair benchmarked.
    let expected: usize = world
        .device_profiles()
        .values()
        .map(|d| d.capabilities.len())
        .sum();
    assert_eq!(e.bench.entries.len(), expected);
}

#[test]
fn energy_curtailment_under_binding_power_cap() {
    // Inject a tight site cap and confirm the engine curtails to respect it.
    let config = Config::demo();
    let mut world = twin::build_default_world(3);
    let mut engine = Engine::bootstrap(&config, &world);
    // Cap TX-01 well below its draw so the marginal-watt curtailment must bind.
    engine.set_power_cap("TX-01", 6000.0);
    engine.run(&mut world, 60);
    let tx_power: f64 = world
        .sense()
        .devices
        .iter()
        .filter(|(id, _)| id.as_str().starts_with("TX-01"))
        .map(|(_, t)| t.power_w)
        .sum();
    assert!(
        tx_power <= 6000.0 * 1.02,
        "site power {tx_power} must respect the injected 6000W cap"
    );
}
