//! KAIROS — an intelligent mining control plane.
//!
//! The layer above the hasher. It decides what to mine, when, where, on which
//! hardware, at which clock and power, with which pools, hedged how, to maximize
//! the operator's risk-adjusted profit across many algorithms and mixed ASIC+GPU
//! fleets — wrapped in a hard safety envelope, reliable enough for industrial
//! scale, and proving its uplift on the operator's own baseline before they pay.
//!
//! This crate is the Phase-0 + Phase-0.5 build: the spine (twin, traits, three
//! ledgers, operator utility, shield, graceful degradation, onboarding, console,
//! config, fleet API, dev fee) plus the vital-few profit levers (degradation-
//! priced per-chip autotuning, forward-difficulty smart switching with mispriced-
//! coin timing, stratum stale minimization, scheme/luck-aware pool selection, the
//! energy option, and the self-healing reliability subsystem).
//!
//! The Python "brain" (Tiers 4–7: hierarchical world-model, self-play, federated
//! learning, …) attaches over gRPC at [`runtime`]'s decision boundary; the Rust
//! core here runs the hard-real-time loop and degrades safely without it.

// These clippy lints fire on deliberate patterns in this codebase: index-based
// loops in the KAT-verified hashing kernels (pow.rs) where rewriting them would
// obscure the spec, builder-style constructors with many parameters, and a modulo
// idiom that predates `is_multiple_of`. They are style, not bugs, and rewriting the
// crypto loops would risk the verified hashes — so they are allowed crate-wide.
#![allow(clippy::too_many_arguments)]
#![allow(clippy::needless_range_loop)]
#![allow(clippy::manual_is_multiple_of)]

pub mod alerts;
pub mod algo;
pub mod api;
pub mod asic;
pub mod cal;
pub mod config;
pub mod console;
pub mod degrade;
pub mod devconfig;
pub mod devfee;
pub mod efficiency;
pub mod engine;
pub mod ethash;
pub mod forecast;
pub mod gpu;
#[cfg(feature = "gui")]
pub mod gui;
pub mod hal;
pub mod hardware;
pub mod heal;
pub mod intelligence;
pub mod kaspa;
pub mod ledger;
pub mod live;
pub mod market_data;
pub mod model;
pub mod pow;
pub mod stratum;
pub mod onboard;
pub mod runtime;
pub mod shield;
pub mod stats;
pub mod telemetry;
pub mod twin;
pub mod utility;

pub use model::*;
