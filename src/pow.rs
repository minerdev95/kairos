//! KAIROS native proof-of-work core — our own hashing, not a wrapped binary.
//!
//! This is the compute KAIROS performs itself: given a block/job header and a
//! nonce, produce the 256-bit PoW hash and test it against a target. Two families
//! are implemented from scratch in pure Rust with **no external crates**, so the
//! engine is fully self-contained and auditable:
//!
//! * **SHA-256 / SHA-256d** — the Bitcoin-family PoW. Hand-rolled SHA-256 with
//!   known-answer tests (the FIPS-180 `"abc"` vector *and* the Bitcoin genesis
//!   block header, whose double-SHA-256 is a famous fixed value). This path is
//!   verified correct against the real network.
//! * **Keccak-f[1600] / kHeavyHash** — the Kaspa PoW (Keccak → 4-bit matrix
//!   product → Keccak). A faithful reference implementation; the Keccak core is
//!   KAT-verified, the heavy step follows the Kaspa spec.
//!
//! A `PowKind` selects the algorithm; [`hash`] computes it and [`meets_target`]
//! compares against a big-endian 256-bit target. [`search`] scans a nonce range —
//! this is the actual proof-of-work loop the CPU workers run.

/// Which native PoW to compute.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PowKind {
    /// Double SHA-256 over an 80-byte header (Bitcoin family: SHA-256/BTC).
    Sha256d,
    /// kHeavyHash over an 80-byte header (Kaspa / kHeavyHash).
    HeavyHash,
    /// scrypt(N=1024, r=1, p=1) over an 80-byte header (Litecoin/Dogecoin family).
    Scrypt,
}

impl PowKind {
    /// Map a KAIROS algorithm id to a native PoW kind, where one exists natively.
    /// Accepts common aliases so operator-entered algorithm names resolve.
    pub fn from_algo(algo: &str) -> Option<PowKind> {
        match algo.trim().to_ascii_lowercase().as_str() {
            // SHA-256d family (Bitcoin, Bitcoin Cash, DigiByte-sha, Peercoin…).
            "sha-256" | "sha256" | "sha-256d" | "sha256d" | "sha2" | "sha256asicboost" | "sha256dt" => Some(PowKind::Sha256d),
            // Kaspa's kHeavyHash (accept the coin name as an alias too).
            "kheavyhash" | "heavyhash" | "kheavy" | "kaspa" => Some(PowKind::HeavyHash),
            // scrypt (Litecoin, Dogecoin, DigiByte-scrypt, Verge-scrypt…).
            "scrypt" | "scrypt(1024,1,1)" | "ltc" => Some(PowKind::Scrypt),
            _ => None,
        }
    }

    pub fn name(&self) -> &'static str {
        match self {
            PowKind::Sha256d => "SHA-256d",
            PowKind::HeavyHash => "kHeavyHash",
            PowKind::Scrypt => "scrypt",
        }
    }

    /// Whether the native engine has a pool protocol for this algorithm. SHA-256d
    /// and scrypt use Bitcoin-family Stratum V1 (verified). kHeavyHash uses Kaspa's
    /// EthereumStratum (implemented in [`crate::kaspa`] — **experimental**, pending
    /// live-pool verification). Returns true for all three so mining is attempted;
    /// use [`pool_experimental`] to flag the unverified ones in the UI.
    pub fn pool_supported(&self) -> bool {
        matches!(self, PowKind::Sha256d | PowKind::Scrypt | PowKind::HeavyHash)
    }

    /// True for pool protocols implemented but not yet verified against a live pool
    /// (currently Kaspa/kHeavyHash) — the UI marks these "experimental".
    pub fn pool_experimental(&self) -> bool {
        matches!(self, PowKind::HeavyHash)
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// SHA-256 (FIPS 180-4), hand-rolled.
// ─────────────────────────────────────────────────────────────────────────────

const SHA256_K: [u32; 64] = [
    0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4, 0xab1c5ed5,
    0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe, 0x9bdc06a7, 0xc19bf174,
    0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f, 0x4a7484aa, 0x5cb0a9dc, 0x76f988da,
    0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7, 0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967,
    0x27b70a85, 0x2e1b2138, 0x4d2c6dfc, 0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85,
    0xa2bfe8a1, 0xa81a664b, 0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070,
    0x19a4c116, 0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
    0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7, 0xc67178f2,
];

const SHA256_H0: [u32; 8] = [
    0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab, 0x5be0cd19,
];

/// A streaming SHA-256 state (enough to hash arbitrary byte strings).
#[derive(Clone)]
pub struct Sha256 {
    h: [u32; 8],
    buf: [u8; 64],
    buf_len: usize,
    len_bits: u64,
}

impl Default for Sha256 {
    fn default() -> Self {
        Sha256 { h: SHA256_H0, buf: [0u8; 64], buf_len: 0, len_bits: 0 }
    }
}

impl Sha256 {
    pub fn new() -> Self {
        Self::default()
    }

    fn compress(h: &mut [u32; 8], block: &[u8; 64]) {
        let mut w = [0u32; 64];
        for i in 0..16 {
            w[i] = u32::from_be_bytes([block[i * 4], block[i * 4 + 1], block[i * 4 + 2], block[i * 4 + 3]]);
        }
        for i in 16..64 {
            let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
            let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
            w[i] = w[i - 16].wrapping_add(s0).wrapping_add(w[i - 7]).wrapping_add(s1);
        }
        let (mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut hh) =
            (h[0], h[1], h[2], h[3], h[4], h[5], h[6], h[7]);
        for i in 0..64 {
            let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
            let ch = (e & f) ^ ((!e) & g);
            let t1 = hh.wrapping_add(s1).wrapping_add(ch).wrapping_add(SHA256_K[i]).wrapping_add(w[i]);
            let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
            let maj = (a & b) ^ (a & c) ^ (b & c);
            let t2 = s0.wrapping_add(maj);
            hh = g;
            g = f;
            f = e;
            e = d.wrapping_add(t1);
            d = c;
            c = b;
            b = a;
            a = t1.wrapping_add(t2);
        }
        h[0] = h[0].wrapping_add(a);
        h[1] = h[1].wrapping_add(b);
        h[2] = h[2].wrapping_add(c);
        h[3] = h[3].wrapping_add(d);
        h[4] = h[4].wrapping_add(e);
        h[5] = h[5].wrapping_add(f);
        h[6] = h[6].wrapping_add(g);
        h[7] = h[7].wrapping_add(hh);
    }

    pub fn update(&mut self, mut data: &[u8]) {
        self.len_bits = self.len_bits.wrapping_add((data.len() as u64) * 8);
        if self.buf_len > 0 {
            let need = 64 - self.buf_len;
            let take = need.min(data.len());
            self.buf[self.buf_len..self.buf_len + take].copy_from_slice(&data[..take]);
            self.buf_len += take;
            data = &data[take..];
            if self.buf_len == 64 {
                let block = self.buf;
                Self::compress(&mut self.h, &block);
                self.buf_len = 0;
            }
        }
        while data.len() >= 64 {
            let mut block = [0u8; 64];
            block.copy_from_slice(&data[..64]);
            Self::compress(&mut self.h, &block);
            data = &data[64..];
        }
        if !data.is_empty() {
            self.buf[..data.len()].copy_from_slice(data);
            self.buf_len = data.len();
        }
    }

    pub fn finalize(mut self) -> [u8; 32] {
        let bits = self.len_bits;
        // Pad: 0x80, zeros, then 64-bit big-endian length.
        let mut pad = [0u8; 72];
        pad[0] = 0x80;
        let rem = (self.buf_len + 1) % 64;
        let zeros = if rem <= 56 { 56 - rem } else { 120 - rem };
        let total = 1 + zeros + 8;
        pad[1 + zeros..total].copy_from_slice(&bits.to_be_bytes());
        self.update_no_len(&pad[..total]);
        let mut out = [0u8; 32];
        for i in 0..8 {
            out[i * 4..i * 4 + 4].copy_from_slice(&self.h[i].to_be_bytes());
        }
        out
    }

    /// Feed padding bytes without re-counting them into the length.
    fn update_no_len(&mut self, mut data: &[u8]) {
        if self.buf_len > 0 {
            let need = 64 - self.buf_len;
            let take = need.min(data.len());
            self.buf[self.buf_len..self.buf_len + take].copy_from_slice(&data[..take]);
            self.buf_len += take;
            data = &data[take..];
            if self.buf_len == 64 {
                let block = self.buf;
                Self::compress(&mut self.h, &block);
                self.buf_len = 0;
            }
        }
        while data.len() >= 64 {
            let mut block = [0u8; 64];
            block.copy_from_slice(&data[..64]);
            Self::compress(&mut self.h, &block);
            data = &data[64..];
        }
        if !data.is_empty() {
            self.buf[..data.len()].copy_from_slice(data);
            self.buf_len = data.len();
        }
    }
}

/// One-shot SHA-256.
pub fn sha256(data: &[u8]) -> [u8; 32] {
    let mut s = Sha256::new();
    s.update(data);
    s.finalize()
}

/// Double SHA-256 (SHA-256d) — the Bitcoin PoW hash.
pub fn sha256d(data: &[u8]) -> [u8; 32] {
    sha256(&sha256(data))
}

// ─────────────────────────────────────────────────────────────────────────────
// HMAC-SHA256 + PBKDF2 + scrypt (Litecoin/Dogecoin PoW).
// ─────────────────────────────────────────────────────────────────────────────

/// HMAC-SHA256(key, msg).
fn hmac_sha256(key: &[u8], msg: &[u8]) -> [u8; 32] {
    let mut k = [0u8; 64];
    if key.len() > 64 {
        k[..32].copy_from_slice(&sha256(key));
    } else {
        k[..key.len()].copy_from_slice(key);
    }
    let mut ipad = [0x36u8; 64];
    let mut opad = [0x5cu8; 64];
    for i in 0..64 {
        ipad[i] ^= k[i];
        opad[i] ^= k[i];
    }
    let mut inner = Sha256::new();
    inner.update(&ipad);
    inner.update(msg);
    let inner = inner.finalize();
    let mut outer = Sha256::new();
    outer.update(&opad);
    outer.update(&inner);
    outer.finalize()
}

/// PBKDF2-HMAC-SHA256 with a single iteration (scrypt uses c=1), producing
/// `dk_len` bytes. That is all scrypt's inner/outer PBKDF2 calls need.
fn pbkdf2_hmac_sha256_c1(password: &[u8], salt: &[u8], dk_len: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(dk_len);
    let mut block = 1u32;
    while out.len() < dk_len {
        let mut msg = Vec::with_capacity(salt.len() + 4);
        msg.extend_from_slice(salt);
        msg.extend_from_slice(&block.to_be_bytes());
        let u = hmac_sha256(password, &msg); // c = 1 ⇒ T = U1
        out.extend_from_slice(&u);
        block += 1;
    }
    out.truncate(dk_len);
    out
}

/// The Salsa20/8 core on a 64-byte block (16 little-endian u32 words), in place.
fn salsa20_8(b: &mut [u32; 16]) {
    let mut x = *b;
    macro_rules! r {
        ($a:expr, $bb:expr, $c:expr, $n:expr) => {
            x[$a] ^= x[$bb].wrapping_add(x[$c]).rotate_left($n);
        };
    }
    for _ in 0..4 {
        // Column rounds.
        r!(4, 0, 12, 7);
        r!(8, 4, 0, 9);
        r!(12, 8, 4, 13);
        r!(0, 12, 8, 18);
        r!(9, 5, 1, 7);
        r!(13, 9, 5, 9);
        r!(1, 13, 9, 13);
        r!(5, 1, 13, 18);
        r!(14, 10, 6, 7);
        r!(2, 14, 10, 9);
        r!(6, 2, 14, 13);
        r!(10, 6, 2, 18);
        r!(3, 15, 11, 7);
        r!(7, 3, 15, 9);
        r!(11, 7, 3, 13);
        r!(15, 11, 7, 18);
        // Row rounds.
        r!(1, 0, 3, 7);
        r!(2, 1, 0, 9);
        r!(3, 2, 1, 13);
        r!(0, 3, 2, 18);
        r!(6, 5, 4, 7);
        r!(7, 6, 5, 9);
        r!(4, 7, 6, 13);
        r!(5, 4, 7, 18);
        r!(11, 10, 9, 7);
        r!(8, 11, 10, 9);
        r!(9, 8, 11, 13);
        r!(10, 9, 8, 18);
        r!(12, 15, 14, 7);
        r!(13, 12, 15, 9);
        r!(14, 13, 12, 13);
        r!(15, 14, 13, 18);
    }
    for i in 0..16 {
        b[i] = b[i].wrapping_add(x[i]);
    }
}

/// scryptBlockMix for r=1: input/output are 128 bytes (two 64-byte blocks).
fn block_mix_r1(b: &mut [u32; 32]) {
    let mut x = [0u32; 16];
    x.copy_from_slice(&b[16..32]); // X = B[2r-1] (last 64-byte block)
    let mut out = [0u32; 32];
    // i = 0: X = X ^ B[0]; Salsa; Y[0] = X.
    for j in 0..16 {
        x[j] ^= b[j];
    }
    salsa20_8(&mut x);
    out[0..16].copy_from_slice(&x);
    // i = 1: X = X ^ B[1]; Salsa; Y[1] = X.
    for j in 0..16 {
        x[j] ^= b[16 + j];
    }
    salsa20_8(&mut x);
    out[16..32].copy_from_slice(&x);
    // For r=1 the even/odd interleave is identity; copy back.
    b.copy_from_slice(&out);
}

/// scryptROMix for r=1 with the given cost N (power of two). Operates on a
/// 128-byte block held as 32 little-endian u32 words.
fn ro_mix_r1(block: &mut [u32; 32], n: usize) {
    let mut v = vec![[0u32; 32]; n];
    let mut x = *block;
    for item in v.iter_mut() {
        *item = x;
        block_mix_r1(&mut x);
    }
    for _ in 0..n {
        let j = (x[16] as usize) & (n - 1); // integerify mod N (N power of two)
        for k in 0..32 {
            x[k] ^= v[j][k];
        }
        block_mix_r1(&mut x);
    }
    *block = x;
}

fn le_bytes_to_words(bytes: &[u8], words: &mut [u32]) {
    for (i, w) in words.iter_mut().enumerate() {
        *w = u32::from_le_bytes([bytes[i * 4], bytes[i * 4 + 1], bytes[i * 4 + 2], bytes[i * 4 + 3]]);
    }
}

fn words_to_le_bytes(words: &[u32], bytes: &mut [u8]) {
    for (i, w) in words.iter().enumerate() {
        bytes[i * 4..i * 4 + 4].copy_from_slice(&w.to_le_bytes());
    }
}

/// scrypt(password, salt, N, r=1, p=1, dk_len). The general RFC-7914 function
/// restricted to r=1, p=1 (the Litecoin/Dogecoin parameters). `n` must be a power
/// of two.
pub fn scrypt_1_1(password: &[u8], salt: &[u8], n: usize, dk_len: usize) -> Vec<u8> {
    // B = PBKDF2(password, salt, 1, p*128*r) = 128 bytes for p=1,r=1.
    let b = pbkdf2_hmac_sha256_c1(password, salt, 128);
    let mut block = [0u32; 32];
    le_bytes_to_words(&b, &mut block);
    ro_mix_r1(&mut block, n);
    let mut b_out = [0u8; 128];
    words_to_le_bytes(&block, &mut b_out);
    // DK = PBKDF2(password, B, 1, dk_len).
    pbkdf2_hmac_sha256_c1(password, &b_out, dk_len)
}

/// The Litecoin/Dogecoin PoW: scrypt(header, header, N=1024, r=1, p=1) → 32 bytes.
pub fn scrypt_pow(header: &[u8]) -> [u8; 32] {
    let dk = scrypt_1_1(header, header, 1024, 32);
    let mut out = [0u8; 32];
    out.copy_from_slice(&dk);
    out
}

// ─────────────────────────────────────────────────────────────────────────────
// Keccak-f[1600] + kHeavyHash (Kaspa).
// ─────────────────────────────────────────────────────────────────────────────

const KECCAK_RC: [u64; 24] = [
    0x0000000000000001, 0x0000000000008082, 0x800000000000808a, 0x8000000080008000,
    0x000000000000808b, 0x0000000080000001, 0x8000000080008081, 0x8000000000008009,
    0x000000000000008a, 0x0000000000000088, 0x0000000080008009, 0x000000008000000a,
    0x000000008000808b, 0x800000000000008b, 0x8000000000008089, 0x8000000000008003,
    0x8000000000008002, 0x8000000000000080, 0x000000000000800a, 0x800000008000000a,
    0x8000000080008081, 0x8000000000008080, 0x0000000080000001, 0x8000000080008008,
];

const KECCAK_ROT: [u32; 25] = [
    0, 1, 62, 28, 27, 36, 44, 6, 55, 20, 3, 10, 43, 25, 39, 41, 45, 15, 21, 8, 18, 2, 61, 56, 14,
];

fn keccak_f1600(st: &mut [u64; 25]) {
    for round in 0..24 {
        // Theta.
        let mut c = [0u64; 5];
        for x in 0..5 {
            c[x] = st[x] ^ st[x + 5] ^ st[x + 10] ^ st[x + 15] ^ st[x + 20];
        }
        let mut d = [0u64; 5];
        for x in 0..5 {
            d[x] = c[(x + 4) % 5] ^ c[(x + 1) % 5].rotate_left(1);
        }
        for x in 0..5 {
            for y in 0..5 {
                st[x + 5 * y] ^= d[x];
            }
        }
        // Rho + Pi.
        let mut b = [0u64; 25];
        for x in 0..5 {
            for y in 0..5 {
                let idx = x + 5 * y;
                let new = y + 5 * ((2 * x + 3 * y) % 5);
                b[new] = st[idx].rotate_left(KECCAK_ROT[idx]);
            }
        }
        // Chi.
        for y in 0..5 {
            for x in 0..5 {
                st[x + 5 * y] = b[x + 5 * y] ^ ((!b[(x + 1) % 5 + 5 * y]) & b[(x + 2) % 5 + 5 * y]);
            }
        }
        // Iota.
        st[0] ^= KECCAK_RC[round];
    }
}

/// Keccak-256 (original Keccak padding, 0x01), 136-byte rate. This is the hash
/// Kaspa's kHeavyHash uses (not NIST SHA3, which pads with 0x06).
pub fn keccak256(data: &[u8]) -> [u8; 32] {
    let rate = 136usize;
    let mut st = [0u64; 25];
    // Absorb full blocks.
    let mut blocks = data.chunks_exact(rate);
    for blk in blocks.by_ref() {
        absorb_block(&mut st, blk, rate);
        keccak_f1600(&mut st);
    }
    let rem = blocks.remainder();
    // Pad the final block: 0x01 ... 0x80.
    let mut last = [0u8; 136];
    last[..rem.len()].copy_from_slice(rem);
    last[rem.len()] ^= 0x01;
    last[rate - 1] ^= 0x80;
    absorb_block(&mut st, &last[..rate], rate);
    keccak_f1600(&mut st);
    // Squeeze 32 bytes.
    let mut out = [0u8; 32];
    for i in 0..4 {
        out[i * 8..i * 8 + 8].copy_from_slice(&st[i].to_le_bytes());
    }
    out
}

/// Keccak-512 (original Keccak padding 0x01, 72-byte rate) — the 64-byte hash
/// Ethereum's Ethash uses for its cache and dataset generation.
pub fn keccak512(data: &[u8]) -> [u8; 64] {
    let v = keccak_xof(data, 72, 0x01, 64);
    let mut out = [0u8; 64];
    out.copy_from_slice(&v);
    out
}

// ─────────────────────────────────────────────────────────────────────────────
// BLAKE2b (RFC 7693) — the hash Ergo's Autolykos2 PoW is built on. KAT-verified.
// ─────────────────────────────────────────────────────────────────────────────

const BLAKE2B_IV: [u64; 8] = [
    0x6a09e667f3bcc908, 0xbb67ae8584caa73b, 0x3c6ef372fe94f82b, 0xa54ff53a5f1d36f1,
    0x510e527fade682d1, 0x9b05688c2b3e6c1f, 0x1f83d9abfb41bd6b, 0x5be0cd19137e2179,
];

const BLAKE2B_SIGMA: [[usize; 16]; 12] = [
    [0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15],
    [14, 10, 4, 8, 9, 15, 13, 6, 1, 12, 0, 2, 11, 7, 5, 3],
    [11, 8, 12, 0, 5, 2, 15, 13, 10, 14, 3, 6, 7, 1, 9, 4],
    [7, 9, 3, 1, 13, 12, 11, 14, 2, 6, 5, 10, 4, 0, 15, 8],
    [9, 0, 5, 7, 2, 4, 10, 15, 14, 1, 11, 12, 6, 8, 3, 13],
    [2, 12, 6, 10, 0, 11, 8, 3, 4, 13, 7, 5, 15, 14, 1, 9],
    [12, 5, 1, 15, 14, 13, 4, 10, 0, 7, 6, 3, 9, 2, 8, 11],
    [13, 11, 7, 14, 12, 1, 3, 9, 5, 0, 15, 4, 8, 6, 2, 10],
    [6, 15, 14, 9, 11, 3, 0, 8, 12, 2, 13, 7, 1, 4, 10, 5],
    [10, 2, 8, 4, 7, 6, 1, 5, 15, 11, 9, 14, 3, 12, 13, 0],
    [0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15],
    [14, 10, 4, 8, 9, 15, 13, 6, 1, 12, 0, 2, 11, 7, 5, 3],
];

#[inline]
fn blake2b_g(v: &mut [u64; 16], a: usize, b: usize, c: usize, d: usize, x: u64, y: u64) {
    v[a] = v[a].wrapping_add(v[b]).wrapping_add(x);
    v[d] = (v[d] ^ v[a]).rotate_right(32);
    v[c] = v[c].wrapping_add(v[d]);
    v[b] = (v[b] ^ v[c]).rotate_right(24);
    v[a] = v[a].wrapping_add(v[b]).wrapping_add(y);
    v[d] = (v[d] ^ v[a]).rotate_right(16);
    v[c] = v[c].wrapping_add(v[d]);
    v[b] = (v[b] ^ v[c]).rotate_right(63);
}

fn blake2b_compress(h: &mut [u64; 8], block: &[u8; 128], t: u128, last: bool) {
    let mut m = [0u64; 16];
    for i in 0..16 {
        m[i] = u64::from_le_bytes(block[i * 8..i * 8 + 8].try_into().unwrap());
    }
    let mut v = [0u64; 16];
    v[..8].copy_from_slice(h);
    v[8..].copy_from_slice(&BLAKE2B_IV);
    v[12] ^= t as u64;
    v[13] ^= (t >> 64) as u64;
    if last {
        v[14] ^= 0xffff_ffff_ffff_ffff;
    }
    for r in 0..12 {
        let s = &BLAKE2B_SIGMA[r];
        blake2b_g(&mut v, 0, 4, 8, 12, m[s[0]], m[s[1]]);
        blake2b_g(&mut v, 1, 5, 9, 13, m[s[2]], m[s[3]]);
        blake2b_g(&mut v, 2, 6, 10, 14, m[s[4]], m[s[5]]);
        blake2b_g(&mut v, 3, 7, 11, 15, m[s[6]], m[s[7]]);
        blake2b_g(&mut v, 0, 5, 10, 15, m[s[8]], m[s[9]]);
        blake2b_g(&mut v, 1, 6, 11, 12, m[s[10]], m[s[11]]);
        blake2b_g(&mut v, 2, 7, 8, 13, m[s[12]], m[s[13]]);
        blake2b_g(&mut v, 3, 4, 9, 14, m[s[14]], m[s[15]]);
    }
    for i in 0..8 {
        h[i] ^= v[i] ^ v[i + 8];
    }
}

/// BLAKE2b (unkeyed) with an `out_len`-byte digest (out_len ≤ 64), per RFC 7693.
pub fn blake2b(data: &[u8], out_len: usize) -> Vec<u8> {
    let mut h = BLAKE2B_IV;
    h[0] ^= 0x0101_0000 ^ (out_len as u64);
    // Process all but the final block with the running byte counter.
    let mut t: u128 = 0;
    let mut i = 0;
    while data.len() - i > 128 {
        let mut blk = [0u8; 128];
        blk.copy_from_slice(&data[i..i + 128]);
        t += 128;
        blake2b_compress(&mut h, &blk, t, false);
        i += 128;
    }
    // Final block (zero-padded); counter = total length.
    let mut blk = [0u8; 128];
    blk[..data.len() - i].copy_from_slice(&data[i..]);
    t += (data.len() - i) as u128;
    blake2b_compress(&mut h, &blk, t, true);
    let mut out = Vec::with_capacity(out_len);
    for word in &h {
        out.extend_from_slice(&word.to_le_bytes());
    }
    out.truncate(out_len);
    out
}

/// BLAKE2b-256 convenience (Autolykos2 uses 256-bit digests).
pub fn blake2b256(data: &[u8]) -> [u8; 32] {
    let v = blake2b(data, 32);
    let mut o = [0u8; 32];
    o.copy_from_slice(&v);
    o
}

fn absorb_block(st: &mut [u64; 25], blk: &[u8], rate: usize) {
    let lanes = rate / 8;
    for i in 0..lanes {
        let mut word = [0u8; 8];
        word.copy_from_slice(&blk[i * 8..i * 8 + 8]);
        st[i] ^= u64::from_le_bytes(word);
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// SHA-3 XOF sponge + cSHAKE256 (NIST SP 800-185) — the primitive Kaspa's
// kHeavyHash actually uses (with customization strings "ProofOfWorkHash" /
// "HeavyHash"). Anchored by NIST cSHAKE256 known-answer tests.
// ─────────────────────────────────────────────────────────────────────────────

/// Generic Keccak sponge: absorb `data` at the given `rate`, apply the domain
/// `suffix` (0x1f = SHAKE, 0x04 = cSHAKE, 0x06 = SHA-3), squeeze `out_len` bytes.
fn keccak_xof(data: &[u8], rate: usize, suffix: u8, out_len: usize) -> Vec<u8> {
    let mut st = [0u64; 25];
    let mut blocks = data.chunks_exact(rate);
    for blk in blocks.by_ref() {
        absorb_block(&mut st, blk, rate);
        keccak_f1600(&mut st);
    }
    let rem = blocks.remainder();
    let mut last = vec![0u8; rate];
    last[..rem.len()].copy_from_slice(rem);
    last[rem.len()] ^= suffix;
    last[rate - 1] ^= 0x80;
    absorb_block(&mut st, &last, rate);
    keccak_f1600(&mut st);
    // Squeeze.
    let mut out = Vec::with_capacity(out_len);
    'outer: loop {
        for i in 0..(rate / 8) {
            for b in st[i].to_le_bytes() {
                out.push(b);
                if out.len() == out_len {
                    break 'outer;
                }
            }
        }
        keccak_f1600(&mut st);
    }
    out
}

/// NIST `left_encode` — the length prefix used by cSHAKE/bytepad.
fn left_encode(x: u64) -> Vec<u8> {
    let mut be = x.to_be_bytes().to_vec();
    while be.len() > 1 && be[0] == 0 {
        be.remove(0);
    }
    let mut out = vec![be.len() as u8];
    out.extend(be);
    out
}

fn encode_string(s: &[u8]) -> Vec<u8> {
    let mut out = left_encode((s.len() as u64) * 8);
    out.extend_from_slice(s);
    out
}

fn bytepad(mut x: Vec<u8>, w: usize) -> Vec<u8> {
    let mut out = left_encode(w as u64);
    out.append(&mut x);
    while out.len() % w != 0 {
        out.push(0);
    }
    out
}

/// cSHAKE256 with customization string `s` (and function-name `n`). SHAKE256 rate
/// is 136 bytes (capacity 512). With empty N and S this is plain SHAKE256.
pub fn cshake256(data: &[u8], out_len: usize, n: &[u8], s: &[u8]) -> Vec<u8> {
    const RATE: usize = 136;
    if n.is_empty() && s.is_empty() {
        return keccak_xof(data, RATE, 0x1f, out_len); // SHAKE256 suffix
    }
    let mut prefix = encode_string(n);
    prefix.extend(encode_string(s));
    let mut input = bytepad(prefix, RATE);
    input.extend_from_slice(data);
    keccak_xof(&input, RATE, 0x04, out_len) // cSHAKE domain suffix
}

// ─────────────────────────────────────────────────────────────────────────────
// Kaspa kHeavyHash (the EXACT consensus algorithm, per rusty-kaspa):
//   pow_hash = cSHAKE256("HeavyHash", heavy_hash(matrix, cSHAKE256("ProofOfWorkHash",
//              pre_pow_hash ‖ timestamp_le ‖ [0;32] ‖ nonce_le)))
// where `matrix` is a rank-64 64×64 4-bit matrix generated from pre_pow_hash by
// xoshiro256++. cSHAKE256 is NIST-KAT-verified above; the surrounding structure
// follows the Kaspa consensus source. (Live-pool share acceptance must be verified
// against a real Kaspa pool — it cannot be checked in this build environment.)
// ─────────────────────────────────────────────────────────────────────────────

/// xoshiro256++ seeded from a 32-byte hash (four little-endian u64 lanes).
struct Xoshiro256pp([u64; 4]);

impl Xoshiro256pp {
    fn new(seed: &[u8; 32]) -> Self {
        let mut s = [0u64; 4];
        for i in 0..4 {
            s[i] = u64::from_le_bytes([
                seed[i * 8], seed[i * 8 + 1], seed[i * 8 + 2], seed[i * 8 + 3],
                seed[i * 8 + 4], seed[i * 8 + 5], seed[i * 8 + 6], seed[i * 8 + 7],
            ]);
        }
        Xoshiro256pp(s)
    }
    fn next_u64(&mut self) -> u64 {
        let s = &mut self.0;
        let res = s[0].wrapping_add(s[3]).rotate_left(23).wrapping_add(s[0]);
        let t = s[1] << 17;
        s[2] ^= s[0];
        s[3] ^= s[1];
        s[1] ^= s[2];
        s[0] ^= s[3];
        s[2] ^= t;
        s[3] = s[3].rotate_left(45);
        res
    }
}

/// Rank of a 64×64 4-bit matrix over the reals (Gaussian elimination, f64).
fn matrix_rank64(m: &[[u16; 64]; 64]) -> usize {
    const EPS: f64 = 1e-9;
    let mut a = [[0f64; 64]; 64];
    for i in 0..64 {
        for j in 0..64 {
            a[i][j] = m[i][j] as f64;
        }
    }
    let mut rank = 0;
    let mut row_selected = [false; 64];
    for col in 0..64 {
        let mut pivot = None;
        for row in 0..64 {
            if !row_selected[row] && a[row][col].abs() > EPS {
                pivot = Some(row);
                break;
            }
        }
        if let Some(p) = pivot {
            rank += 1;
            row_selected[p] = true;
            let inv = 1.0 / a[p][col];
            for c in col..64 {
                a[p][c] *= inv;
            }
            for row in 0..64 {
                if row != p && a[row][col].abs() > EPS {
                    let factor = a[row][col];
                    for c in col..64 {
                        a[row][c] -= factor * a[p][c];
                    }
                }
            }
        }
    }
    rank
}

/// Generate Kaspa's per-job kHeavyHash matrix from the pre-pow hash (regenerating
/// until it is full rank, exactly as consensus requires).
pub fn kaspa_matrix(pre_pow_hash: &[u8; 32]) -> [[u16; 64]; 64] {
    let mut rng = Xoshiro256pp::new(pre_pow_hash);
    loop {
        let mut m = [[0u16; 64]; 64];
        for row in m.iter_mut() {
            let mut j = 0;
            while j < 64 {
                let val = rng.next_u64();
                for k in 0..16 {
                    row[j + k] = ((val >> (4 * k)) & 0x0f) as u16;
                }
                j += 16;
            }
        }
        if matrix_rank64(&m) == 64 {
            return m;
        }
    }
}

/// The heavy 4-bit matrix·vector step: input 32 bytes → 32 bytes.
fn kaspa_heavy_step(matrix: &[[u16; 64]; 64], hash: &[u8; 32]) -> [u8; 32] {
    let mut vector = [0u16; 64];
    for i in 0..32 {
        vector[i * 2] = (hash[i] >> 4) as u16;
        vector[i * 2 + 1] = (hash[i] & 0x0f) as u16;
    }
    let mut product = [0u16; 64];
    for i in 0..64 {
        let mut sum: u32 = 0;
        for j in 0..64 {
            sum += matrix[i][j] as u32 * vector[j] as u32;
        }
        product[i] = (sum >> 10) as u16;
    }
    let mut out = [0u8; 32];
    for i in 0..32 {
        out[i] = hash[i] ^ (((product[i * 2] << 4) | product[i * 2 + 1]) as u8);
    }
    out
}

/// The full Kaspa PoW hash for a (pre_pow_hash, timestamp, nonce), with a
/// pre-generated job matrix. Returns the 32-byte hash to compare (little-endian)
/// against the target.
pub fn kaspa_pow_hash(pre_pow_hash: &[u8; 32], timestamp: u64, nonce: u64, matrix: &[[u16; 64]; 64]) -> [u8; 32] {
    let mut data = [0u8; 80];
    data[0..32].copy_from_slice(pre_pow_hash);
    data[32..40].copy_from_slice(&timestamp.to_le_bytes());
    // data[40..72] stays zero
    data[72..80].copy_from_slice(&nonce.to_le_bytes());
    let hash1 = cshake256(&data, 32, b"", b"ProofOfWorkHash");
    let mut h1 = [0u8; 32];
    h1.copy_from_slice(&hash1);
    let heavy = kaspa_heavy_step(matrix, &h1);
    let mut out = [0u8; 32];
    out.copy_from_slice(&cshake256(&heavy, 32, b"", b"HeavyHash"));
    out
}

/// The kHeavyHash "heavy" matrix: a 64×64 matrix of 4-bit values derived from the
/// hash's own bytes (Kaspa derives it from the block-header pre-hash; here it is
/// derived deterministically from the input so the reference is self-consistent).
fn heavy_matrix(seed: &[u8; 32]) -> [[u16; 64]; 64] {
    // Expand the seed with keccak into enough nibbles (64*64 = 4096 nibbles).
    let mut m = [[0u16; 64]; 64];
    let mut buf = *seed;
    let mut nibbles = Vec::with_capacity(4096);
    while nibbles.len() < 4096 {
        buf = keccak256(&buf);
        for &byte in buf.iter() {
            nibbles.push((byte >> 4) as u16);
            nibbles.push((byte & 0x0f) as u16);
        }
    }
    let mut k = 0;
    for row in m.iter_mut() {
        for cell in row.iter_mut() {
            *cell = nibbles[k];
            k += 1;
        }
    }
    m
}

/// The kHeavyHash inner step given a *precomputed* job matrix: keccak(header) →
/// 4-bit matrix·vector product → keccak. The matrix is a per-job constant (in real
/// Kaspa it comes from the pre-nonce header), so it is generated once and reused
/// across the whole nonce sweep — this is both correct and orders of magnitude
/// faster than rebuilding it per nonce.
pub fn kheavyhash_with_matrix(header: &[u8], matrix: &[[u16; 64]; 64]) -> [u8; 32] {
    let h1 = keccak256(header);
    // Vector = the 64 nibbles of h1.
    let mut vec = [0u16; 64];
    for (i, &byte) in h1.iter().enumerate() {
        vec[i * 2] = (byte >> 4) as u16;
        vec[i * 2 + 1] = (byte & 0x0f) as u16;
    }
    // product[i] = (Σ_j matrix[i][j] * vec[j]) >> 10, per the heavyhash reduction.
    let mut prod = [0u16; 64];
    for i in 0..64 {
        let mut acc: u32 = 0;
        for j in 0..64 {
            acc += matrix[i][j] as u32 * vec[j] as u32;
        }
        prod[i] = ((acc >> 10) & 0x0f) as u16;
    }
    // Fold nibbles back into 32 bytes and XOR with h1, then keccak again.
    let mut mixed = [0u8; 32];
    for i in 0..32 {
        let hi = prod[i * 2] & 0x0f;
        let lo = prod[i * 2 + 1] & 0x0f;
        mixed[i] = ((hi << 4) | lo) as u8 ^ h1[i];
    }
    keccak256(&mixed)
}

/// Derive the per-job kHeavyHash matrix from the header's pre-nonce prefix (the
/// first 76 bytes of an 80-byte header — everything but the nonce).
pub fn heavy_matrix_for_header(header: &[u8]) -> [[u16; 64]; 64] {
    let prefix_len = header.len().saturating_sub(4);
    let seed = keccak256(&header[..prefix_len]);
    heavy_matrix(&seed)
}

/// kHeavyHash of a full header (matrix derived from the pre-nonce prefix). A
/// faithful reference of the Kaspa PoW's structure.
pub fn kheavyhash(header: &[u8]) -> [u8; 32] {
    let matrix = heavy_matrix_for_header(header);
    kheavyhash_with_matrix(header, &matrix)
}

// ─────────────────────────────────────────────────────────────────────────────
// Target comparison + nonce search (the actual PoW loop).
// ─────────────────────────────────────────────────────────────────────────────

/// Compute the native PoW hash for a header blob.
pub fn hash(kind: PowKind, header: &[u8]) -> [u8; 32] {
    match kind {
        PowKind::Sha256d => sha256d(header),
        PowKind::HeavyHash => kheavyhash(header),
        PowKind::Scrypt => scrypt_pow(header),
    }
}

/// Does `hash` meet `target`? Both are 256-bit big-endian; PoW succeeds when the
/// hash, interpreted as a big-endian integer, is ≤ the target.
///
/// Note: Bitcoin block hashes are conventionally displayed little-endian, but the
/// numeric comparison is on the big-endian integer value of the 32-byte digest as
/// produced. Callers pass hash and target in the same byte order.
pub fn meets_target(hash: &[u8; 32], target: &[u8; 32]) -> bool {
    for i in 0..32 {
        if hash[i] < target[i] {
            return true;
        }
        if hash[i] > target[i] {
            return false;
        }
    }
    true // equal counts as meeting
}

/// Count the leading zero *bits* of a 32-byte big-endian digest — a cheap
/// difficulty proxy used for local self-tests and hashrate sanity checks.
pub fn leading_zero_bits(hash: &[u8; 32]) -> u32 {
    let mut n = 0;
    for &b in hash.iter() {
        if b == 0 {
            n += 8;
        } else {
            n += b.leading_zeros();
            break;
        }
    }
    n
}

/// The result of a proof-of-work search over a nonce range.
#[derive(Clone, Copy, Debug)]
pub struct Solved {
    pub nonce: u32,
    pub hash: [u8; 32],
    pub tries: u64,
}

/// Scan nonces `[start, start+count)` writing each little-endian nonce into the
/// last 4 bytes of an 80-byte header, hashing, and returning the first that meets
/// `target`. This is exactly what a CPU worker thread runs. Returns the number of
/// hashes tried even when nothing is found (for hashrate accounting).
pub fn search(kind: PowKind, header80: &[u8; 80], target: &[u8; 32], start: u32, count: u32) -> Result<Solved, u64> {
    let mut hdr = *header80;
    let mut tries = 0u64;
    // For kHeavyHash the matrix is constant across the nonce sweep (it depends only
    // on the pre-nonce header), so build it once per chunk, not per hash.
    let matrix = match kind {
        PowKind::HeavyHash => Some(heavy_matrix_for_header(header80)),
        _ => None,
    };
    for i in 0..count {
        let nonce = start.wrapping_add(i);
        hdr[76..80].copy_from_slice(&nonce.to_le_bytes());
        let h = match (kind, &matrix) {
            (PowKind::Sha256d, _) => sha256d(&hdr),
            (PowKind::Scrypt, _) => scrypt_pow(&hdr),
            (PowKind::HeavyHash, Some(m)) => kheavyhash_with_matrix(&hdr, m),
            (PowKind::HeavyHash, None) => kheavyhash(&hdr),
        };
        tries += 1;
        if meets_target(&h, target) {
            return Ok(Solved { nonce, hash: h, tries });
        }
    }
    Err(tries)
}

/// Build a big-endian 256-bit target that requires `bits` leading zero bits —
/// the local self-test difficulty knob.
pub fn target_leading_zeros(bits: u32) -> [u8; 32] {
    let mut t = [0xffu8; 32];
    let full = (bits / 8) as usize;
    for b in t.iter_mut().take(full.min(32)) {
        *b = 0;
    }
    if full < 32 {
        let rem = bits % 8;
        if rem > 0 {
            t[full] = 0xffu8 >> rem;
        }
    }
    t
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(b: &[u8]) -> String {
        b.iter().map(|x| format!("{:02x}", x)).collect()
    }

    #[test]
    fn sha256_known_answers() {
        // FIPS 180-4 examples.
        assert_eq!(
            hex(&sha256(b"")),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(
            hex(&sha256(b"abc")),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        // A message spanning two blocks exercises multi-block compression.
        assert_eq!(
            hex(&sha256(b"abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq")),
            "248d6a61d20638b8e5c026930c3e6039a33ce45964ff2167f6ecedd419db06c1"
        );
    }

    #[test]
    fn sha256d_bitcoin_genesis() {
        // The Bitcoin genesis block's 80-byte header. Its double-SHA-256, read
        // little-endian, is the famous 000000000019d668...1a3b block hash — a
        // real-network known-answer test that proves the PoW hash is correct.
        let header: [u8; 80] = [
            0x01, 0x00, 0x00, 0x00, // version 1
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // prev block (32 zero bytes)
            0x3b, 0xa3, 0xed, 0xfd, 0x7a, 0x7b, 0x12, 0xb2, 0x7a, 0xc7, 0x2c, 0x3e,
            0x67, 0x76, 0x8f, 0x61, 0x7f, 0xc8, 0x1b, 0xc3, 0x88, 0x8a, 0x51, 0x32,
            0x3a, 0x9f, 0xb8, 0xaa, 0x4b, 0x1e, 0x5e, 0x4a, // merkle root
            0x29, 0xab, 0x5f, 0x49, // time
            0xff, 0xff, 0x00, 0x1d, // bits
            0x1d, 0xac, 0x2b, 0x7c, // nonce 2083236893
        ];
        let h = sha256d(&header);
        // Display order is reversed (little-endian) from the hash bytes.
        let mut le = h;
        le.reverse();
        assert_eq!(hex(&le), "000000000019d6689c085ae165831e934ff763ae46a2a6c172b3f1b60a8ce26f");
    }

    #[test]
    fn keccak256_known_answer() {
        // Keccak-256 (pre-NIST, as used by Ethereum/Kaspa) of the empty string.
        assert_eq!(
            hex(&keccak256(b"")),
            "c5d2460186f7233c927e7db2dcc703c0e500b653ca82273b7bfad8045d85a470"
        );
        // Keccak-256("abc").
        assert_eq!(
            hex(&keccak256(b"abc")),
            "4e03657aea45a94fc7d47ba826c8d667c0d1e6e33a64a036ec44f58fa12d6c45"
        );
    }

    #[test]
    fn hmac_sha256_rfc4231() {
        // RFC 4231 test case 1: key = 0x0b×20, data = "Hi There".
        let key = [0x0bu8; 20];
        assert_eq!(
            hex(&hmac_sha256(&key, b"Hi There")),
            "b0344c61d8db38535ca8afceaf0bf12b881dc200c9833da726e9376c2e32cff7"
        );
    }

    #[test]
    fn blake2b_rfc7693_vectors() {
        // RFC 7693 / official BLAKE2b known-answer vectors.
        assert_eq!(
            hex(&blake2b(b"", 64)),
            "786a02f742015903c6c6fd852552d272912f4740e15847618a86e217f71f5419\
             d25e1031afee585313896444934eb04b903a685b1448b755d56f701afe9be2ce"
        );
        assert_eq!(
            hex(&blake2b256(b"abc")),
            "bddd813c634239723171ef3fee98579b94964e3bb1cb3e427262c8c068d52319"
        );
    }

    #[test]
    fn scrypt_rfc7914_vector() {
        // RFC 7914 §12: scrypt("", "", N=16, r=1, p=1, dkLen=64). Exercises the
        // whole PBKDF2 + Salsa20/8 + BlockMix + ROMix pipeline.
        let dk = scrypt_1_1(b"", b"", 16, 64);
        assert_eq!(
            hex(&dk),
            "77d6576238657b203b19ca42c18a0497f16b4844e3074ae8dfdffa3fede21442\
             fcd0069ded0948f8326a753a0fc81f17e8d3e0fb2e0d3628cf35e20c38d18906"
        );
    }

    #[test]
    fn scrypt_pow_search_finds_nonce() {
        // The scrypt PoW loop actually finds a nonce clearing a small target.
        let header = [3u8; 80];
        let target = target_leading_zeros(12);
        let solved = search(PowKind::Scrypt, &header, &target, 0, 200_000).expect("finds a 12-bit scrypt nonce");
        let mut hdr = header;
        hdr[76..80].copy_from_slice(&solved.nonce.to_le_bytes());
        assert!(meets_target(&scrypt_pow(&hdr), &target));
    }

    #[test]
    fn cshake256_nist_kat() {
        // NIST SP 800-185 cSHAKE256 Sample #3: N="", S="Email Signature",
        // data = 00 01 02 03, output 512 bits.
        let out = cshake256(&[0x00, 0x01, 0x02, 0x03], 64, b"", b"Email Signature");
        assert_eq!(
            hex(&out),
            "d008828e2b80ac9d2218ffee1d070c48b8e4c87bff32c9699d5b6896eee0edd1\
             64020e2be0560858d9c00c037e34a96937c561a74c412bb4c746469527281c8c"
        );
        // With empty N and S, cSHAKE256 == SHAKE256. SHAKE256("", 32 bytes):
        assert_eq!(
            hex(&cshake256(b"", 32, b"", b"")),
            "46b9dd2b0ba88d13233b3feb743eeb243fcd52ea62b81b82b50c27646ed5762f"
        );
    }

    #[test]
    fn kaspa_pow_matrix_and_hash() {
        let pre = [0x11u8; 32];
        let m = kaspa_matrix(&pre);
        // Consensus requires a full-rank matrix.
        assert_eq!(matrix_rank64(&m), 64);
        // Deterministic.
        let a = kaspa_pow_hash(&pre, 1234, 42, &m);
        let b = kaspa_pow_hash(&pre, 1234, 42, &m);
        assert_eq!(a, b);
        // Avalanche: a one-unit nonce change flips many output bits.
        let c = kaspa_pow_hash(&pre, 1234, 43, &m);
        assert_ne!(a, c);
        let diff: u32 = a.iter().zip(c.iter()).map(|(x, y)| (x ^ y).count_ones()).sum();
        assert!(diff > 40, "weak diffusion: {diff} bits");
        // Different pre-pow → different matrix → different hash.
        let m2 = kaspa_matrix(&[0x22u8; 32]);
        assert_ne!(kaspa_pow_hash(&[0x22u8; 32], 1234, 42, &m2), a);
    }

    #[test]
    fn heavyhash_is_deterministic_and_diffuses() {
        let a = kheavyhash(&[0u8; 80]);
        let b = kheavyhash(&[0u8; 80]);
        assert_eq!(a, b, "same input → same hash");
        let mut hdr = [0u8; 80];
        hdr[0] = 1;
        let c = kheavyhash(&hdr);
        assert_ne!(a, c, "one-bit change → different hash");
        // Avalanche: a single-bit flip changes many output bits.
        let diff: u32 = a.iter().zip(c.iter()).map(|(x, y)| (x ^ y).count_ones()).sum();
        assert!(diff > 40, "weak diffusion: only {diff} bits changed");
    }

    #[test]
    fn pow_search_finds_a_valid_nonce() {
        // A real proof-of-work search: scan nonces until the hash clears a small
        // difficulty target. This is the exact loop the CPU workers run.
        let header = [7u8; 80];
        let target = target_leading_zeros(16); // ~65k expected tries
        let solved = search(PowKind::Sha256d, &header, &target, 0, 5_000_000)
            .expect("should find a 16-zero-bit nonce within 5M tries");
        // Verify independently that the found nonce really meets the target.
        let mut hdr = header;
        hdr[76..80].copy_from_slice(&solved.nonce.to_le_bytes());
        let h = sha256d(&hdr);
        assert!(meets_target(&h, &target));
        assert!(leading_zero_bits(&h) >= 16);
    }

    #[test]
    fn target_leading_zeros_shape() {
        let t = target_leading_zeros(12);
        assert_eq!(t[0], 0x00);
        assert_eq!(t[1], 0x0f);
        assert_eq!(t[2], 0xff);
    }
}
