//! Autolykos v2 — the proof-of-work of **Ergo (ERG)**, the algorithm an 8 GB GPU
//! can actually mine in 2026 (~2 GB memory footprint). KAIROS's own pure-Rust
//! implementation of the exact consensus algorithm from `ergoplatform/ergo`
//! (`AutolykosPowScheme.hitForVersion2ForMessage`), verified against the official
//! height-614400 known-answer vector in the tests below.
//!
//! Per nonce: derive a seed, pick `k=32` indexes into a table of `N` elements, sum
//! those elements, hash the sum → the "hit". A share is valid when `hit < target`.
//! The `N` elements (`genElement(j)`) are identical for every nonce in a block, so a
//! GPU precomputes them once into a ~2 GB table and each nonce is then 32 lookups —
//! that table is the memory-hard core (built on the GPU in the CUDA path).

use crate::pow::blake2b256;

/// Number of table elements summed per hit.
pub const K: usize = 32;

/// Autolykos2 table size `N` for a block height (2^26 base, +5% every 50·1024 blocks
/// from height 600·1024, capped at height 4,198,400).
pub fn calc_n(height: u32) -> u64 {
    const N_BASE: u64 = 1 << 26; // 67,108,864
    const INCREASE_START: u32 = 600 * 1024; // 614,400
    const INCREASE_PERIOD: u32 = 50 * 1024; // 51,200
    const MAX_HEIGHT: u32 = 4_198_400;
    let h = height.min(MAX_HEIGHT);
    if h < INCREASE_START {
        return N_BASE;
    }
    let iters = (h - INCREASE_START) / INCREASE_PERIOD + 1;
    let mut n = N_BASE;
    for _ in 0..iters {
        n = n / 100 * 105;
    }
    n
}

/// The constant `M` mixed into every element: 8-byte big-endian of `i` for
/// `i ∈ [0, 1024)` — 8192 bytes.
pub fn calc_m() -> Vec<u8> {
    let mut m = Vec::with_capacity(8192);
    for i in 0u64..1024 {
        m.extend_from_slice(&i.to_be_bytes());
    }
    m
}

/// The `k=32` table indexes for a seed: blake2b256(seed) extended by its own first 3
/// bytes, read as 32 overlapping big-endian u32s, each mod N.
fn gen_indexes(seed: &[u8], n: u64) -> [u32; K] {
    let hash = blake2b256(seed);
    let mut ext = [0u8; 35];
    ext[0..32].copy_from_slice(&hash);
    ext[32..35].copy_from_slice(&hash[0..3]);
    let mut idxs = [0u32; K];
    for i in 0..K {
        let v = u32::from_be_bytes([ext[i], ext[i + 1], ext[i + 2], ext[i + 3]]);
        idxs[i] = (v as u64 % n) as u32;
    }
    idxs
}

/// One table element `genElement(j)` for Autolykos v2 = blake2b256(j‖h‖M) with the
/// leading byte dropped (31 bytes), as an unsigned big-endian integer.
pub fn gen_element(index: u32, height_be: &[u8; 4], m: &[u8]) -> [u8; 31] {
    let mut buf = Vec::with_capacity(4 + 4 + m.len());
    buf.extend_from_slice(&index.to_be_bytes());
    buf.extend_from_slice(height_be);
    buf.extend_from_slice(m);
    let h = blake2b256(&buf);
    let mut out = [0u8; 31];
    out.copy_from_slice(&h[1..32]);
    out
}

/// Add a big-endian value (≤ 32 bytes, right-aligned) into a 256-bit accumulator.
fn add_be(acc: &mut [u8; 32], val: &[u8]) {
    let mut carry = 0u16;
    let voff = 32 - val.len();
    for i in (0..32).rev() {
        let a = acc[i] as u16;
        let b = if i >= voff { val[i - voff] as u16 } else { 0 };
        let s = a + b + carry;
        acc[i] = (s & 0xff) as u8;
        carry = s >> 8;
    }
}

/// The Autolykos2 hit for `(msg, nonce, height, N)`, computing the 32 elements on the
/// fly (the verification reference; the GPU uses a precomputed table). Compare
/// `hit < target` as 32-byte big-endian integers for a valid share.
pub fn hit(msg: &[u8; 32], nonce: &[u8; 8], height: u32, n: u64) -> [u8; 32] {
    let m = calc_m();
    let h = height.to_be_bytes();
    // prei8 = last 8 bytes of blake2b256(msg‖nonce); i = prei8 mod N (4-byte BE)
    let mut b0 = Vec::with_capacity(40);
    b0.extend_from_slice(msg);
    b0.extend_from_slice(nonce);
    let hh = blake2b256(&b0);
    let prei8 = u64::from_be_bytes(hh[24..32].try_into().unwrap());
    let i_val = (prei8 % n) as u32;
    // f = blake2b256(i‖h‖M) with leading byte dropped (31 bytes)
    let mut bf = Vec::with_capacity(4 + 4 + m.len());
    bf.extend_from_slice(&i_val.to_be_bytes());
    bf.extend_from_slice(&h);
    bf.extend_from_slice(&m);
    let fh = blake2b256(&bf);
    // seed = f ‖ msg ‖ nonce
    let mut seed = Vec::with_capacity(31 + 32 + 8);
    seed.extend_from_slice(&fh[1..32]);
    seed.extend_from_slice(msg);
    seed.extend_from_slice(nonce);
    let indexes = gen_indexes(&seed, n);
    // sum of the 32 elements
    let mut sum = [0u8; 32];
    for idx in indexes {
        let e = gen_element(idx, &h, &m);
        add_be(&mut sum, &e);
    }
    // hit = blake2b256(sum-as-32-byte-BE)
    blake2b256(&sum)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hexb(s: &str) -> Vec<u8> {
        crate::stratum::from_hex(s).unwrap()
    }

    #[test]
    fn calc_n_matches_ergo_vectors() {
        assert_eq!(calc_n(700_000.min(500_000)), 1 << 26); // pre-increase
        assert_eq!(calc_n(600 * 1024), 70_464_240);
        assert_eq!(calc_n(650 * 1024), 73_987_410);
        assert_eq!(calc_n(788_400), 81_571_035);
        assert_eq!(calc_n(1_051_200), 104_107_290);
        assert_eq!(calc_n(4_198_400), 2_143_944_600); // max
        assert_eq!(calc_n(41_984_000), 2_143_944_600);
    }

    #[test]
    fn hit_matches_ergo_height_614400_kat() {
        // Official Ergo AutolykosPowSchemeSpec vector (height 614,400).
        let msg: [u8; 32] = hexb("548c3e602a8f36f8f2738f5f643b02425038044d98543a51cabaa9785e7e864f")
            .try_into()
            .unwrap();
        let nonce: [u8; 8] = hexb("0000000000003105").try_into().unwrap();
        let n = calc_n(614_400);
        assert_eq!(n, 70_464_240);
        let hit = hit(&msg, &nonce, 614_400, n);
        assert_eq!(
            crate::stratum::to_hex(&hit),
            "0002fcb113fe65e5754959872dfdbffea0489bf830beb4961ddc0e9e66a1412a"
        );
    }
}
