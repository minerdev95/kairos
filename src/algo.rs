//! Algorithm registry.
//!
//! Adding an algorithm is registering a profile, not rewriting the engine. Each
//! algorithm carries its compute character, execution backend (native kernel or
//! wrapped binary), and a tuning profile that tells the autotuner which way the
//! stable edge leans (push memory for memory-bound coins, core for compute-bound).

use crate::model::*;
use std::collections::BTreeMap;

/// The contract a registered algorithm satisfies.
pub trait Algorithm {
    fn id(&self) -> AlgorithmId;
    fn character(&self) -> ComputeProfile;
    fn backend(&self) -> ExecutionBackend;
    fn profile(&self) -> &AlgoProfile;
}

impl Algorithm for AlgoProfile {
    fn id(&self) -> AlgorithmId {
        self.id.clone()
    }
    fn character(&self) -> ComputeProfile {
        self.character
    }
    fn backend(&self) -> ExecutionBackend {
        self.backend.clone()
    }
    fn profile(&self) -> &AlgoProfile {
        self
    }
}

/// The registry. Keyed by id; the engine looks up profiles, never special-cases.
#[derive(Clone, Debug, Default)]
pub struct AlgorithmRegistry {
    profiles: BTreeMap<AlgorithmId, AlgoProfile>,
}

impl AlgorithmRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register(&mut self, p: AlgoProfile) {
        self.profiles.insert(p.id.clone(), p);
    }

    pub fn get(&self, id: &AlgorithmId) -> Option<&AlgoProfile> {
        self.profiles.get(id)
    }

    pub fn ids(&self) -> impl Iterator<Item = &AlgorithmId> {
        self.profiles.keys()
    }

    pub fn len(&self) -> usize {
        self.profiles.len()
    }

    pub fn is_empty(&self) -> bool {
        self.profiles.is_empty()
    }

    /// A default registry covering the algorithms in the spec's console mock:
    /// SHA-256 (ASIC), kHeavyHash (GPU+ASIC), Autolykos2, Ethash, KawPow.
    pub fn with_defaults() -> Self {
        let mut r = Self::new();
        // Reference efficiencies are the network-average J/hash, kept consistent
        // with the representative fleet's J/hash so implied network margins are
        // realistic (tens of percent), not absurd. Mis-scaling these makes the
        // difficulty forecaster extrapolate wildly.
        r.register(AlgoProfile {
            id: "SHA-256".into(),
            character: ComputeProfile::ComputeBound,
            backend: ExecutionBackend::Native,
            mem_sensitivity: 0.0,
            ref_efficiency_j_per_h: 2.11e-11, // ~21 J/TH
        });
        r.register(AlgoProfile {
            id: "kHeavyHash".into(),
            character: ComputeProfile::ComputeBound,
            backend: ExecutionBackend::Native,
            mem_sensitivity: 0.15,
            ref_efficiency_j_per_h: 9.3e-8,
        });
        r.register(AlgoProfile {
            id: "Autolykos2".into(),
            character: ComputeProfile::MemoryBound,
            backend: ExecutionBackend::Native,
            mem_sensitivity: 0.85,
            ref_efficiency_j_per_h: 5.75e-7,
        });
        r.register(AlgoProfile {
            id: "Ethash".into(),
            character: ComputeProfile::MemoryBound,
            backend: ExecutionBackend::Native,
            mem_sensitivity: 0.95,
            ref_efficiency_j_per_h: 2.17e-6,
        });
        r.register(AlgoProfile {
            id: "KawPow".into(),
            character: ComputeProfile::Mixed,
            backend: ExecutionBackend::Native,
            mem_sensitivity: 0.55,
            ref_efficiency_j_per_h: 4.8e-6,
        });
        // Scrypt (Litecoin/Dogecoin) — memory-hard; a real KAIROS-native kernel.
        r.register(AlgoProfile {
            id: "Scrypt".into(),
            character: ComputeProfile::MemoryBound,
            backend: ExecutionBackend::Native,
            mem_sensitivity: 0.7,
            ref_efficiency_j_per_h: 2.0e-4,
        });
        r
    }
}
