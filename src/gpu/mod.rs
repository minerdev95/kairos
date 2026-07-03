//! KAIROS native GPU backend — our own CUDA kernels behind [`GpuHasher`].
//!
//! The real kernels are in `src/gpu/kairos_kernels.cu` (SHA-256d + kHeavyHash),
//! compiled by `build.rs` with `nvcc` when the crate is built with
//! `--features gpu`. Without that feature this module is a documented stub so the
//! default, fully-portable CPU build has no CUDA dependency whatsoever.
//!
//! Build the GPU backend on a machine with the CUDA toolkit:
//! ```text
//! cargo build --release --features gpu
//! ```
//! (Not built or benchmarked in the KAIROS CI/dev sandbox, which has no GPU.)

use crate::engine::GpuHasher;
use std::sync::Arc;

/// Detect usable GPU hashers. Returns an empty list unless the crate was built
/// with `--features gpu` and an NVIDIA GPU is present.
pub fn detect_hashers() -> Vec<Arc<dyn GpuHasher>> {
    #[cfg(feature = "gpu")]
    {
        imp::detect()
    }
    #[cfg(not(feature = "gpu"))]
    {
        Vec::new()
    }
}

/// Whether this build includes the CUDA GPU backend at all.
pub fn gpu_feature_enabled() -> bool {
    cfg!(feature = "gpu")
}

/// Search a u64 nonce range for a Kaspa share on the GPU using the EXACT Kaspa
/// kHeavyHash kernel, with the rank-64 job matrix precomputed on the host. Returns
/// the winning nonce ONLY after re-verifying it on the CPU (so a miscompiled kernel
/// can never yield an accepted-but-wrong share). Returns `None` without the `gpu`
/// feature or when no nonce in the range qualifies.
#[allow(unused_variables)]
pub fn cuda_kaspa_search(
    pre_pow: &[u8; 32],
    timestamp: u64,
    matrix: &[[u16; 64]; 64],
    target: &[u8; 32],
    start: u64,
    count: u64,
) -> Option<u64> {
    #[cfg(feature = "gpu")]
    {
        imp::kaspa_search(pre_pow, timestamp, matrix, target, start, count)
    }
    #[cfg(not(feature = "gpu"))]
    {
        None
    }
}

#[cfg(feature = "gpu")]
mod imp {
    use super::*;
    use crate::pow::{PowKind, Solved};

    extern "C" {
        fn kairos_cuda_device_count() -> i32;
        fn kairos_cuda_search_sha256d(
            header80: *const u8,
            target32: *const u8,
            start: u32,
            count: u32,
            out_nonce: *mut u32,
            out_hash32: *mut u8,
        ) -> i32;
        fn kairos_cuda_search_heavyhash(
            header80: *const u8,
            target32: *const u8,
            start: u32,
            count: u32,
            out_nonce: *mut u32,
            out_hash32: *mut u8,
        ) -> i32;
        fn kairos_cuda_search_kaspa(
            pre_pow32: *const u8,
            timestamp: u64,
            matrix4096: *const u16,
            target32: *const u8,
            start: u64,
            count: u64,
            out_nonce: *mut u64,
        ) -> i32;
    }

    /// GPU Kaspa search + CPU re-verification of the winning nonce.
    pub fn kaspa_search(
        pre_pow: &[u8; 32],
        timestamp: u64,
        matrix: &[[u16; 64]; 64],
        target: &[u8; 32],
        start: u64,
        count: u64,
    ) -> Option<u64> {
        let mut nonce: u64 = 0;
        let hit = unsafe {
            kairos_cuda_search_kaspa(
                pre_pow.as_ptr(),
                timestamp,
                matrix.as_ptr() as *const u16, // row-major 64×64
                target.as_ptr(),
                start,
                count,
                &mut nonce,
            )
        };
        if hit != 1 {
            return None;
        }
        // Re-verify on the CPU: recompute the exact PoW and check ≤ target as a
        // little-endian integer. Only submit if it genuinely qualifies.
        let h = crate::pow::kaspa_pow_hash(pre_pow, timestamp, nonce, matrix);
        let mut be = h;
        be.reverse();
        if &be <= target {
            Some(nonce)
        } else {
            None
        }
    }

    pub struct CudaHasher {
        index: i32,
    }

    impl GpuHasher for CudaHasher {
        fn name(&self) -> String {
            format!("CUDA device {}", self.index)
        }

        fn search(&self, kind: PowKind, header: &[u8; 80], target: &[u8; 32], start: u32, count: u32) -> Result<Solved, u64> {
            let mut nonce: u32 = 0;
            let mut hash = [0u8; 32];
            let hit = unsafe {
                match kind {
                    PowKind::Sha256d => kairos_cuda_search_sha256d(
                        header.as_ptr(), target.as_ptr(), start, count, &mut nonce, hash.as_mut_ptr(),
                    ),
                    PowKind::HeavyHash => kairos_cuda_search_heavyhash(
                        header.as_ptr(), target.as_ptr(), start, count, &mut nonce, hash.as_mut_ptr(),
                    ),
                    // Scrypt is memory-hard; no GPU kernel — fall back to CPU (0 = not found here).
                    // Kaspa's exact PoW uses the u64-nonce `cuda_kaspa_search` path, not this one.
                    _ => 0,
                }
            };
            if hit == 1 {
                // Recompute the winning hash on the CPU for a verified return value.
                let mut hdr = *header;
                hdr[76..80].copy_from_slice(&nonce.to_le_bytes());
                let h = crate::pow::hash(kind, &hdr);
                Ok(Solved { nonce, hash: h, tries: count as u64 })
            } else {
                Err(count as u64)
            }
        }
    }

    pub fn detect() -> Vec<Arc<dyn GpuHasher>> {
        let n = unsafe { kairos_cuda_device_count() };
        (0..n).map(|i| Arc::new(CudaHasher { index: i }) as Arc<dyn GpuHasher>).collect()
    }
}
