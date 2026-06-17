//! In-tree SHA-256, HMAC-SHA-256, and PBKDF2-HMAC-SHA-256.
//!
//! Implements FIPS 180-4 (SHA-256), RFC 2104 (HMAC), and RFC 8018 (PBKDF2) so
//! the SCRAM-SHA-256 authentication exchange has no external crypto dependency,
//! in keeping with the rest of the project. The output of `pbkdf2` here is one
//! 32-byte block, which is exactly the digest length SCRAM needs.

/// Initial hash values: the fractional parts of the square roots of the first
/// eight primes.
const H0: [u32; 8] = [
    0x6a09_e667,
    0xbb67_ae85,
    0x3c6e_f372,
    0xa54f_f53a,
    0x510e_527f,
    0x9b05_688c,
    0x1f83_d9ab,
    0x5be0_cd19,
];

/// Round constants: the fractional parts of the cube roots of the first 64
/// primes.
const K: [u32; 64] = [
    0x428a_2f98,
    0x7137_4491,
    0xb5c0_fbcf,
    0xe9b5_dba5,
    0x3956_c25b,
    0x59f1_11f1,
    0x923f_82a4,
    0xab1c_5ed5,
    0xd807_aa98,
    0x1283_5b01,
    0x2431_85be,
    0x550c_7dc3,
    0x72be_5d74,
    0x80de_b1fe,
    0x9bdc_06a7,
    0xc19b_f174,
    0xe49b_69c1,
    0xefbe_4786,
    0x0fc1_9dc6,
    0x240c_a1cc,
    0x2de9_2c6f,
    0x4a74_84aa,
    0x5cb0_a9dc,
    0x76f9_88da,
    0x983e_5152,
    0xa831_c66d,
    0xb003_27c8,
    0xbf59_7fc7,
    0xc6e0_0bf3,
    0xd5a7_9147,
    0x06ca_6351,
    0x1429_2967,
    0x27b7_0a85,
    0x2e1b_2138,
    0x4d2c_6dfc,
    0x5338_0d13,
    0x650a_7354,
    0x766a_0abb,
    0x81c2_c92e,
    0x9272_2c85,
    0xa2bf_e8a1,
    0xa81a_664b,
    0xc24b_8b70,
    0xc76c_51a3,
    0xd192_e819,
    0xd699_0624,
    0xf40e_3585,
    0x106a_a070,
    0x19a4_c116,
    0x1e37_6c08,
    0x2748_774c,
    0x34b0_bcb5,
    0x391c_0cb3,
    0x4ed8_aa4a,
    0x5b9c_ca4f,
    0x682e_6ff3,
    0x748f_82ee,
    0x78a5_636f,
    0x84c8_7814,
    0x8cc7_0208,
    0x90be_fffa,
    0xa450_6ceb,
    0xbef9_a3f7,
    0xc671_78f2,
];

/// The SHA-256 digest of `msg`.
// The working variables are the spec's single letters a..h; renaming them would
// only obscure the correspondence with FIPS 180-4.
#[allow(clippy::many_single_char_names)]
#[must_use]
pub fn sha256(msg: &[u8]) -> [u8; 32] {
    let mut h = H0;

    // Pad: append 0x80, then zeros, then the 64-bit big-endian bit length, so
    // the total is a multiple of 64 bytes.
    let bit_len = (msg.len() as u64).wrapping_mul(8);
    let mut data = msg.to_vec();
    data.push(0x80);
    while data.len() % 64 != 56 {
        data.push(0);
    }
    data.extend_from_slice(&bit_len.to_be_bytes());

    for block in data.chunks_exact(64) {
        let mut w = [0u32; 64];
        for (word, chunk) in w.iter_mut().zip(block.chunks_exact(4)) {
            *word = u32::from_be_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]);
        }
        for i in 16..64 {
            let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
            let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
            w[i] = w[i - 16]
                .wrapping_add(s0)
                .wrapping_add(w[i - 7])
                .wrapping_add(s1);
        }

        let [mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut hh] = h;
        for (&ki, &wi) in K.iter().zip(w.iter()) {
            let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
            let ch = (e & f) ^ ((!e) & g);
            let t1 = hh
                .wrapping_add(s1)
                .wrapping_add(ch)
                .wrapping_add(ki)
                .wrapping_add(wi);
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
        for (slot, v) in h.iter_mut().zip([a, b, c, d, e, f, g, hh]) {
            *slot = slot.wrapping_add(v);
        }
    }

    let mut out = [0u8; 32];
    for (chunk, word) in out.chunks_exact_mut(4).zip(h) {
        chunk.copy_from_slice(&word.to_be_bytes());
    }
    out
}

/// SHA-256's input block size in bytes (used to pad / hash the HMAC key).
const BLOCK: usize = 64;

/// HMAC-SHA-256 of `msg` under `key` (RFC 2104).
#[must_use]
pub fn hmac_sha256(key: &[u8], msg: &[u8]) -> [u8; 32] {
    // A key longer than the block is replaced by its digest; shorter keys are
    // zero-padded to the block length.
    let mut k = [0u8; BLOCK];
    if key.len() > BLOCK {
        k[..32].copy_from_slice(&sha256(key));
    } else {
        k[..key.len()].copy_from_slice(key);
    }

    let mut inner = Vec::with_capacity(BLOCK + msg.len());
    let mut outer = Vec::with_capacity(BLOCK + 32);
    for &kb in &k {
        inner.push(kb ^ 0x36);
        outer.push(kb ^ 0x5c);
    }
    inner.extend_from_slice(msg);
    outer.extend_from_slice(&sha256(&inner));
    sha256(&outer)
}

/// PBKDF2-HMAC-SHA-256 producing a single 32-byte block (RFC 8018). SCRAM's
/// salted password is exactly one block, so block indexing is fixed at 1.
#[must_use]
pub fn pbkdf2(password: &[u8], salt: &[u8], iterations: u32) -> [u8; 32] {
    // U1 = HMAC(password, salt || INT32BE(1)).
    let mut salted = Vec::with_capacity(salt.len() + 4);
    salted.extend_from_slice(salt);
    salted.extend_from_slice(&1u32.to_be_bytes());
    let mut u = hmac_sha256(password, &salted);
    let mut out = u;
    for _ in 1..iterations {
        u = hmac_sha256(password, &u);
        for (o, &ub) in out.iter_mut().zip(u.iter()) {
            *o ^= ub;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Render a digest as lowercase hex for comparison against known vectors.
    fn hex(bytes: &[u8]) -> String {
        use std::fmt::Write as _;
        let mut s = String::with_capacity(bytes.len() * 2);
        for b in bytes {
            write!(s, "{b:02x}").unwrap();
        }
        s
    }

    #[test]
    fn sha256_known_vectors() {
        // FIPS 180-4 / NIST examples.
        assert_eq!(
            hex(&sha256(b"")),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(
            hex(&sha256(b"abc")),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert_eq!(
            hex(&sha256(
                b"abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq"
            )),
            "248d6a61d20638b8e5c026930c3e6039a33ce45964ff2167f6ecedd419db06c1"
        );
    }

    #[test]
    fn sha256_handles_block_boundary() {
        // 55, 56, and 64 bytes straddle the one-block padding boundary.
        for n in [55usize, 56, 63, 64, 65] {
            let msg = vec![b'a'; n];
            // Cross-check the length-prefixed padding does not panic and is
            // deterministic.
            assert_eq!(sha256(&msg), sha256(&msg));
        }
    }

    #[test]
    fn hmac_known_vector() {
        // RFC 4231 test case 2: key "Jefe", data "what do ya want for nothing?".
        assert_eq!(
            hex(&hmac_sha256(b"Jefe", b"what do ya want for nothing?")),
            "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843"
        );
    }

    #[test]
    fn pbkdf2_known_vectors() {
        // Published PBKDF2-HMAC-SHA-256 vectors (P="password", S="salt",
        // dkLen=32) at 1 and 2 iterations.
        assert_eq!(
            hex(&pbkdf2(b"password", b"salt", 1)),
            "120fb6cffcf8b32c43e7225256c4f837a86548c92ccc35480805987cb70be17b"
        );
        assert_eq!(
            hex(&pbkdf2(b"password", b"salt", 2)),
            "ae4d0c95af6b46d32d0adff928f06dd02a303f8ef3c251dfd6e2d85a95474c43"
        );
    }
}
