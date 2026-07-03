//! Ethash / Etchash — the proof-of-work of **Ethereum Classic (ETC)**, the biggest
//! GPU-mined algorithm. This is KAIROS's own pure-Rust implementation of the exact
//! Ethash spec (the same one used by go-ethereum), verified against the **official
//! go-ethereum `TestHashimoto` known-answer vector** (see the test at the bottom).
//!
//! Ethash is *memory-hard*: real mining streams a multi-GB DAG, which only makes
//! sense on a GPU (the CUDA kernel in `src/gpu`). The pure-Rust path here is the
//! **verification reference** (light client: it computes dataset items on the fly
//! from the ~tens-of-MB cache) and the source of truth the GPU kernel must match.
//!
//! **Etchash** (ECIP-1099) is identical to Ethash except the epoch length doubled
//! from 30000 to 60000 blocks at ETC block 11,700,000, which halves DAG growth.
//! [`etchash_epoch`] handles that.

use crate::pow::{keccak256, keccak512};

const HASH_BYTES: usize = 64;
const HASH_WORDS: usize = 16; // u32 words per 64-byte item
const DATASET_PARENTS: u32 = 256;
const CACHE_ROUNDS: usize = 3;
const MIX_BYTES: usize = 128;
const MIX_WORDS: usize = 32; // u32 words in the mix
const ACCESSES: u32 = 64;
const FNV_PRIME: u32 = 0x0100_0193;

#[inline]
fn fnv(a: u32, b: u32) -> u32 {
    a.wrapping_mul(FNV_PRIME) ^ b
}

#[inline]
fn read_u32(b: &[u8], word: usize) -> u32 {
    u32::from_le_bytes([b[word * 4], b[word * 4 + 1], b[word * 4 + 2], b[word * 4 + 3]])
}

/// The Ethash seedhash for an epoch: `keccak256` applied `epoch` times to 32 zero
/// bytes. (Epoch 0 → all zeros.)
pub fn seed_hash(epoch: u64) -> [u8; 32] {
    let mut seed = [0u8; 32];
    for _ in 0..epoch {
        seed = keccak256(&seed);
    }
    seed
}

/// Ethereum epoch for a block (30000-block epochs).
pub fn ethash_epoch(block: u64) -> u64 {
    block / 30_000
}

/// Etchash epoch for an ETC block (ECIP-1099: 60000-block epochs after the
/// activation block 11,700,000; before that, standard 30000).
pub fn etchash_epoch(block: u64) -> u64 {
    const ACTIVATION: u64 = 11_700_000;
    if block >= ACTIVATION {
        // epoch of the activation block under the old schedule, then 60k epochs.
        (ACTIVATION / 30_000) / 2 + (block - ACTIVATION) / 60_000
    } else {
        block / 30_000
    }
}

/// Generate the Ethash verification cache (`size` bytes, a multiple of 64) from a
/// 32-byte seedhash. This is the RandMemoHash construction from the spec.
pub fn generate_cache(size: usize, seed: &[u8; 32]) -> Vec<u8> {
    let n = size / HASH_BYTES;
    let mut cache = vec![0u8; size];
    cache[0..HASH_BYTES].copy_from_slice(&keccak512(seed));
    for i in 1..n {
        let prev: [u8; 64] = cache[(i - 1) * HASH_BYTES..i * HASH_BYTES].try_into().unwrap();
        cache[i * HASH_BYTES..(i + 1) * HASH_BYTES].copy_from_slice(&keccak512(&prev));
    }
    // 3 rounds of: cache[i] = keccak512(cache[i-1] XOR cache[cache[i][0] % n])
    let mut temp = [0u8; HASH_BYTES];
    for _ in 0..CACHE_ROUNDS {
        for i in 0..n {
            let src = ((i + n - 1) % n) * HASH_BYTES;
            let dst = i * HASH_BYTES;
            let xor = (read_u32(&cache[dst..dst + 4], 0) as usize % n) * HASH_BYTES;
            for k in 0..HASH_BYTES {
                temp[k] = cache[src + k] ^ cache[xor + k];
            }
            cache[dst..dst + HASH_BYTES].copy_from_slice(&keccak512(&temp));
        }
    }
    cache
}

/// Compute one 64-byte dataset item on the fly from the cache (the "light" path).
fn dataset_item(cache: &[u8], index: u32) -> [u8; 64] {
    let n = cache.len() / HASH_BYTES;
    let base = (index as usize % n) * HASH_BYTES;
    let mut mix = [0u8; 64];
    mix.copy_from_slice(&cache[base..base + HASH_BYTES]);
    // first word XOR index, then keccak512
    let w0 = read_u32(&mix, 0) ^ index;
    mix[0..4].copy_from_slice(&w0.to_le_bytes());
    let mut mix = keccak512(&mix);

    let mut m = [0u32; HASH_WORDS];
    for (i, mw) in m.iter_mut().enumerate() {
        *mw = read_u32(&mix, i);
    }
    for i in 0..DATASET_PARENTS {
        let parent = fnv(index ^ i, m[(i as usize) % HASH_WORDS]) as usize % n;
        let pbase = parent * HASH_BYTES;
        for w in 0..HASH_WORDS {
            m[w] = fnv(m[w], read_u32(&cache[pbase..pbase + HASH_BYTES], w));
        }
    }
    for (i, mw) in m.iter().enumerate() {
        mix[i * 4..i * 4 + 4].copy_from_slice(&mw.to_le_bytes());
    }
    keccak512(&mix)
}

/// The Ethash "light" hashimoto: given the full DAG `size` (bytes), the cache, the
/// 32-byte header hash and a nonce, return `(mix_digest, result)`. A share is valid
/// when `result` (as a big-endian 256-bit number) ≤ the target.
pub fn hashimoto_light(size: u64, cache: &[u8], header: &[u8; 32], nonce: u64) -> ([u8; 32], [u8; 32]) {
    let rows = (size / MIX_BYTES as u64) as u32;
    // seed = keccak512(header ‖ nonce_le)
    let mut buf = [0u8; 40];
    buf[0..32].copy_from_slice(header);
    buf[32..40].copy_from_slice(&nonce.to_le_bytes());
    let seed = keccak512(&buf);
    let seed_head = read_u32(&seed, 0);

    // mix = seed replicated to 128 bytes (32 u32 words)
    let mut mix = [0u32; MIX_WORDS];
    for i in 0..MIX_WORDS {
        mix[i] = read_u32(&seed, i % HASH_WORDS);
    }
    for i in 0..ACCESSES {
        let parent = fnv(i ^ seed_head, mix[(i as usize) % MIX_WORDS]) % rows;
        let mut temp = [0u32; MIX_WORDS];
        for k in 0..(MIX_BYTES / HASH_BYTES) as u32 {
            let item = dataset_item(cache, 2 * parent + k);
            for w in 0..HASH_WORDS {
                temp[(k as usize) * HASH_WORDS + w] = read_u32(&item, w);
            }
        }
        for w in 0..MIX_WORDS {
            mix[w] = fnv(mix[w], temp[w]);
        }
    }
    // compress 32 → 8 words
    let mut cmix = [0u32; 8];
    for i in 0..8 {
        cmix[i] = fnv(fnv(fnv(mix[i * 4], mix[i * 4 + 1]), mix[i * 4 + 2]), mix[i * 4 + 3]);
    }
    let mut digest = [0u8; 32];
    for i in 0..8 {
        digest[i * 4..i * 4 + 4].copy_from_slice(&cmix[i].to_le_bytes());
    }
    // result = keccak256(seed ‖ digest)
    let mut rbuf = [0u8; 96];
    rbuf[0..64].copy_from_slice(&seed);
    rbuf[64..96].copy_from_slice(&digest);
    (digest, keccak256(&rbuf))
}

/// DAG (full dataset) size in bytes for an epoch — grows ~8 MB per epoch. Used to
/// size the GPU DAG and to drive `hashimoto`. (The exact spec picks the largest
/// prime-bounded size below a linear cap; this returns that value.)
pub fn dataset_size(epoch: u64) -> u64 {
    const INIT: u64 = 1 << 30; // 1 GB
    const GROWTH: u64 = 1 << 23; // 8 MB / epoch
    let mut sz = INIT + GROWTH * epoch - MIX_BYTES as u64;
    while !is_prime(sz / MIX_BYTES as u64) {
        sz -= MIX_BYTES as u64;
    }
    sz
}

/// Cache size in bytes for an epoch (~tens of MB, grows 64 KB/epoch).
pub fn cache_size(epoch: u64) -> u64 {
    const INIT: u64 = 1 << 24; // 16 MB
    const GROWTH: u64 = 1 << 16; // 64 KB / epoch
    let mut sz = INIT + GROWTH * epoch - HASH_BYTES as u64;
    while !is_prime(sz / HASH_BYTES as u64) {
        sz -= HASH_BYTES as u64;
    }
    sz
}

fn is_prime(n: u64) -> bool {
    if n < 2 {
        return false;
    }
    if n % 2 == 0 {
        return n == 2;
    }
    let mut i = 3u64;
    while i * i <= n {
        if n % i == 0 {
            return false;
        }
        i += 2;
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex32(s: &str) -> [u8; 32] {
        let b = crate::stratum::from_hex(s).unwrap();
        let mut o = [0u8; 32];
        o.copy_from_slice(&b);
        o
    }

    // The official go-ethereum Ethash KAT (consensus/ethash TestHashimoto):
    // cache = generateCache(1024 bytes, epoch 0, zero seed); full size = 32*1024.
    #[test]
    fn hashimoto_matches_go_ethereum_kat() {
        let cache = generate_cache(1024, &[0u8; 32]);
        let header = hex32("c9149cc0386e689d789a1c2f3d5d169a61a6218ed30e74414dc736e442ef3d1f");
        let (digest, result) = hashimoto_light(32 * 1024, &cache, &header, 0);
        assert_eq!(
            digest,
            hex32("e4073cffaef931d37117cefd9afd27ea0f1cad6a981dd2605c4a1ac97c519800"),
            "mix digest must match go-ethereum vector"
        );
        assert_eq!(
            result,
            hex32("d3539235ee2e6f8db665c0a72169f55b7f6c605712330b778ec3944f0eb5a557"),
            "result must match go-ethereum vector"
        );
    }

    #[test]
    fn seed_and_epoch_helpers() {
        assert_eq!(seed_hash(0), [0u8; 32]);
        assert_eq!(seed_hash(1), keccak256(&[0u8; 32]));
        assert_eq!(ethash_epoch(30_000), 1);
        // ETC: epochs are 30k until the ECIP-1099 activation, then 60k.
        assert_eq!(etchash_epoch(11_700_000 - 1), 389);
        assert_eq!(etchash_epoch(11_700_000), 195);
    }
}
