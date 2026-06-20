//! Capability tokens: signed, scoped, expiring grants to specific memories.
//!
//! A node you cannot reach still has to decide who may read what. Shipping the
//! master key to every caller is unthinkable. Instead the authority issues a
//! capability token: a small, unforgeable grant that names a tenant, the exact
//! memory ids it covers, and an expiry. The node verifies the token with a shared
//! secret and needs no callback to a central server, which is what makes it work
//! across a partition.
//!
//! Tokens are authenticated with HMAC-SHA256, built from scratch on the same hash
//! the rest of the engine uses. A token whose tenant, scope, or expiry is altered
//! by even one bit fails verification, and an expired or out-of-scope token is
//! refused.

use crate::authmem::sha256;

const BLOCK: usize = 64;

/// HMAC-SHA256 of `msg` under `key`, from scratch.
#[must_use]
pub fn hmac_sha256(key: &[u8], msg: &[u8]) -> [u8; 32] {
    // Normalize the key to one block.
    let mut block = [0u8; BLOCK];
    if key.len() > BLOCK {
        block[..32].copy_from_slice(&sha256::hash(key));
    } else {
        block[..key.len()].copy_from_slice(key);
    }

    let mut ipad = [0u8; BLOCK];
    let mut opad = [0u8; BLOCK];
    for i in 0..BLOCK {
        ipad[i] = block[i] ^ 0x36;
        opad[i] = block[i] ^ 0x5c;
    }

    let mut inner = Vec::with_capacity(BLOCK + msg.len());
    inner.extend_from_slice(&ipad);
    inner.extend_from_slice(msg);
    let inner_hash = sha256::hash(&inner);

    let mut outer = Vec::with_capacity(BLOCK + 32);
    outer.extend_from_slice(&opad);
    outer.extend_from_slice(&inner_hash);
    sha256::hash(&outer)
}

/// A constant-time equality check, so verification does not leak where two tags
/// first differ.
fn ct_eq(a: &[u8; 32], b: &[u8; 32]) -> bool {
    let mut diff = 0u8;
    for i in 0..32 {
        diff |= a[i] ^ b[i];
    }
    diff == 0
}

/// A capability token: who, what, until when, and a tag binding it all.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Token {
    /// The tenant this token authorizes.
    pub tenant: String,
    /// The memory ids the token grants access to.
    pub scopes: Vec<u64>,
    /// Expiry, as a logical timestamp; the token is invalid once `now >= this`.
    pub expires_at: u64,
    /// The HMAC tag over the fields above.
    pub tag: [u8; 32],
}

/// The canonical, unambiguous byte encoding the tag is computed over.
fn signing_bytes(tenant: &str, scopes: &[u64], expires_at: u64) -> Vec<u8> {
    let mut buf = Vec::new();
    buf.extend_from_slice(&(tenant.len() as u64).to_be_bytes());
    buf.extend_from_slice(tenant.as_bytes());
    buf.extend_from_slice(&(scopes.len() as u64).to_be_bytes());
    for &s in scopes {
        buf.extend_from_slice(&s.to_be_bytes());
    }
    buf.extend_from_slice(&expires_at.to_be_bytes());
    buf
}

/// Issue a token signed with the authority's `key`.
#[must_use]
pub fn issue(key: &[u8], tenant: &str, scopes: &[u64], expires_at: u64) -> Token {
    let tag = hmac_sha256(key, &signing_bytes(tenant, scopes, expires_at));
    Token {
        tenant: tenant.to_string(),
        scopes: scopes.to_vec(),
        expires_at,
        tag,
    }
}

/// Why a token was refused.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Denied {
    /// The tag does not match: the token is forged or altered.
    BadSignature,
    /// The token has expired.
    Expired,
    /// The token does not grant the requested memory.
    OutOfScope,
}

impl std::fmt::Display for Denied {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::BadSignature => write!(f, "forged or altered token (signature mismatch)"),
            Self::Expired => write!(f, "expired token"),
            Self::OutOfScope => write!(f, "token does not cover this memory"),
        }
    }
}

/// Verify that `token` authorizes access to `mem_id` at logical time `now`.
///
/// # Errors
/// Returns [`Denied`] if the token is forged, expired, or out of scope.
pub fn verify(key: &[u8], token: &Token, now: u64, mem_id: u64) -> Result<(), Denied> {
    let expected = hmac_sha256(
        key,
        &signing_bytes(&token.tenant, &token.scopes, token.expires_at),
    );
    if !ct_eq(&expected, &token.tag) {
        return Err(Denied::BadSignature);
    }
    if now >= token.expires_at {
        return Err(Denied::Expired);
    }
    if !token.scopes.contains(&mem_id) {
        return Err(Denied::OutOfScope);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(bytes: &[u8]) -> String {
        use std::fmt::Write as _;
        let mut s = String::new();
        for b in bytes {
            let _ = write!(s, "{b:02x}");
        }
        s
    }

    #[test]
    fn hmac_matches_rfc4231_test_case_1() {
        // RFC 4231: key = 0x0b x20, data = "Hi There".
        let key = [0x0b_u8; 20];
        let tag = hmac_sha256(&key, b"Hi There");
        assert_eq!(
            hex(&tag),
            "b0344c61d8db38535ca8afceaf0bf12b881dc200c9833da726e9376c2e32cff7"
        );
    }

    #[test]
    fn a_valid_token_authorizes_its_scope() {
        let key = b"authority secret";
        let token = issue(key, "acme", &[1, 2, 3], 100);
        assert!(verify(key, &token, 50, 2).is_ok());
    }

    #[test]
    fn an_expired_token_is_refused() {
        let key = b"authority secret";
        let token = issue(key, "acme", &[1], 100);
        assert_eq!(verify(key, &token, 100, 1), Err(Denied::Expired));
        assert_eq!(verify(key, &token, 200, 1), Err(Denied::Expired));
    }

    #[test]
    fn an_out_of_scope_memory_is_refused() {
        let key = b"authority secret";
        let token = issue(key, "acme", &[1, 2, 3], 100);
        assert_eq!(verify(key, &token, 50, 9), Err(Denied::OutOfScope));
    }

    #[test]
    fn a_tampered_token_fails_the_signature() {
        let key = b"authority secret";
        let mut token = issue(key, "acme", &[1, 2, 3], 100);
        // Widen the scope without re-signing.
        token.scopes.push(9);
        assert_eq!(verify(key, &token, 50, 9), Err(Denied::BadSignature));
        // Or extend the expiry.
        let mut t2 = issue(key, "acme", &[1], 100);
        t2.expires_at = 9999;
        assert_eq!(verify(key, &t2, 50, 1), Err(Denied::BadSignature));
    }

    #[test]
    fn a_token_from_a_different_key_is_refused() {
        let token = issue(b"real key", "acme", &[1], 100);
        assert_eq!(
            verify(b"wrong key", &token, 50, 1),
            Err(Denied::BadSignature)
        );
    }
}
