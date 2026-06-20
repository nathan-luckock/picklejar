//! Shamir secret sharing: split a memory key so no single node holds it.
//!
//! A memory key that unlocks a tenant's data is the crown jewel. Putting it on
//! one unreachable node is a single point of compromise and a single point of
//! loss. Shamir's scheme splits a secret into `n` shares such that any `k` of
//! them reconstruct it exactly and any `k - 1` reveal nothing at all, in the
//! information-theoretic sense: with fewer than `k` shares every secret is still
//! equally possible.
//!
//! Each secret byte becomes the constant term of a random degree `k - 1`
//! polynomial over the finite field GF(2^8); a share is that polynomial
//! evaluated at a distinct nonzero point. Reconstruction is Lagrange
//! interpolation back to the value at zero. The field is the AES field, built
//! from scratch here.

/// Multiply in GF(2^8) with the AES reduction polynomial (x^8 + x^4 + x^3 + x + 1).
fn gf_mul(mut a: u8, mut b: u8) -> u8 {
    let mut p = 0u8;
    for _ in 0..8 {
        if b & 1 != 0 {
            p ^= a;
        }
        let high = a & 0x80;
        a <<= 1;
        if high != 0 {
            a ^= 0x1b;
        }
        b >>= 1;
    }
    p
}

/// Exponentiation in GF(2^8) by square-and-multiply.
fn gf_pow(mut base: u8, mut exp: u32) -> u8 {
    let mut result = 1u8;
    while exp > 0 {
        if exp & 1 == 1 {
            result = gf_mul(result, base);
        }
        base = gf_mul(base, base);
        exp >>= 1;
    }
    result
}

/// Multiplicative inverse in GF(2^8): `a^254`, since `a^255 == 1` for nonzero a.
fn gf_inv(a: u8) -> u8 {
    gf_pow(a, 254)
}

/// One share of a secret: a distinct x coordinate and the polynomial values at
/// it, one per secret byte.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Share {
    /// The evaluation point (never zero; zero is the secret itself).
    pub x: u8,
    /// The share value for each secret byte.
    pub y: Vec<u8>,
}

/// A tiny deterministic generator so a seed reproduces a split. In use the
/// coefficients must be drawn from real randomness.
struct Rng(u64);
impl Rng {
    #[allow(clippy::cast_possible_truncation)] // deliberately taking one byte
    fn byte(&mut self) -> u8 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        (x >> 24) as u8
    }
}

/// Evaluate a polynomial (given by its coefficients, lowest first) at `x`.
fn eval(coeffs: &[u8], x: u8) -> u8 {
    // Horner's method in the field.
    let mut acc = 0u8;
    for &c in coeffs.iter().rev() {
        acc = gf_mul(acc, x) ^ c;
    }
    acc
}

/// Split `secret` into `n` shares such that any `k` reconstruct it.
///
/// # Panics
/// Panics if `k == 0`, `k > n`, or `n == 0` (a threshold that cannot be met).
#[must_use]
pub fn split(secret: &[u8], k: u8, n: u8, seed: u64) -> Vec<Share> {
    assert!(k >= 1 && n >= k, "need 1 <= k <= n");
    let mut rng = Rng(seed | 1);
    let mut shares: Vec<Share> = (1..=n)
        .map(|x| Share {
            x,
            y: Vec::with_capacity(secret.len()),
        })
        .collect();

    for &byte in secret {
        // Random polynomial with the secret byte as the constant term.
        let mut coeffs = vec![byte];
        for _ in 1..k {
            coeffs.push(rng.byte());
        }
        for share in &mut shares {
            share.y.push(eval(&coeffs, share.x));
        }
    }
    shares
}

/// Reconstruct the secret from a set of shares by interpolating to `x = 0`. Any
/// `k` or more consistent shares recover the secret; fewer recover only noise.
///
/// # Panics
/// Panics if the shares disagree on length or there are none.
#[must_use]
pub fn combine(shares: &[Share]) -> Vec<u8> {
    assert!(!shares.is_empty(), "need at least one share");
    let len = shares[0].y.len();
    assert!(
        shares.iter().all(|s| s.y.len() == len),
        "shares disagree on length"
    );

    let mut secret = Vec::with_capacity(len);
    for byte_idx in 0..len {
        let mut value = 0u8;
        for (j, sj) in shares.iter().enumerate() {
            // Lagrange basis at 0: product over m != j of x_m / (x_m - x_j).
            let mut num = 1u8;
            let mut den = 1u8;
            for (m, sm) in shares.iter().enumerate() {
                if m != j {
                    num = gf_mul(num, sm.x);
                    den = gf_mul(den, sm.x ^ sj.x); // subtraction is XOR
                }
            }
            let basis = gf_mul(num, gf_inv(den));
            value ^= gf_mul(sj.y[byte_idx], basis);
        }
        secret.push(value);
    }
    secret
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn field_inverse_is_consistent() {
        for a in 1u8..=255 {
            assert_eq!(gf_mul(a, gf_inv(a)), 1, "a * a^-1 must be 1");
        }
    }

    #[test]
    fn any_k_of_n_reconstruct_the_secret() {
        let secret = b"this is a 32-byte memory key!!!!";
        let shares = split(secret, 3, 5, 0xABCD_1234);
        // Several distinct 3-subsets all recover the secret.
        for combo in [[0, 1, 2], [0, 2, 4], [1, 3, 4], [2, 3, 4]] {
            let picked: Vec<Share> = combo.iter().map(|&i| shares[i].clone()).collect();
            assert_eq!(combine(&picked), secret, "subset {combo:?} must recover");
        }
        // All five also work.
        assert_eq!(combine(&shares), secret);
    }

    #[test]
    fn fewer_than_k_shares_do_not_reveal_the_secret() {
        let secret = b"top secret";
        let shares = split(secret, 3, 5, 7);
        // Two shares interpolate a different (wrong) constant term.
        let two: Vec<Share> = vec![shares[0].clone(), shares[1].clone()];
        assert_ne!(
            combine(&two),
            secret,
            "k-1 shares must not recover the secret"
        );
    }

    #[test]
    fn shares_are_distinct_and_nonzero_x() {
        let shares = split(b"x", 2, 4, 99);
        for s in &shares {
            assert_ne!(s.x, 0);
        }
        let xs: Vec<u8> = shares.iter().map(|s| s.x).collect();
        let mut sorted = xs.clone();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted.len(), xs.len(), "x coordinates must be distinct");
    }

    #[test]
    fn two_of_two_is_an_xor_split() {
        // A 2-of-2 split of a single byte: each share alone is uniform-looking,
        // and the pair recovers the secret.
        let shares = split(&[0xA5], 2, 2, 0xDEAD);
        assert_eq!(combine(&shares), vec![0xA5]);
    }
}
