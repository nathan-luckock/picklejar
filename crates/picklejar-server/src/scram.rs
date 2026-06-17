//! SCRAM-SHA-256 server-side authentication (RFC 5802 / RFC 7677).
//!
//! The wire server can require a password without ever storing or transmitting
//! it: the client proves knowledge of the password through a challenge-response
//! exchange, and the server keeps only values derived from it (`StoredKey` and
//! `ServerKey`). This module holds the credential derivation and the pure
//! message logic; the I/O loop lives in [`pgwire`](crate::pgwire). Channel
//! binding is not used (the client sends a `n,,` GS2 header), and passwords are
//! taken as raw UTF-8 (no `SASLprep` normalization), a documented simplification.

use crate::sha256::{hmac_sha256, pbkdf2, sha256};

/// How the server authenticates a connection.
#[derive(Debug)]
pub enum Auth {
    /// Accept any user without a password (the default).
    Trust,
    /// Require this account to pass a SCRAM-SHA-256 exchange.
    Scram(Credentials),
}

/// The verifier the server keeps for a SCRAM account. None of these values
/// reveal the account's secret; `stored_key` and `server_key` are one-way
/// derivations of it.
#[derive(Debug)]
pub struct Credentials {
    /// The account name the client must connect as.
    pub username: String,
    /// Per-account random salt.
    pub salt: Vec<u8>,
    /// PBKDF2 iteration count.
    pub iterations: u32,
    /// `H(HMAC(SaltedPassword, "Client Key"))`, used to verify the client proof.
    pub stored_key: [u8; 32],
    /// `HMAC(SaltedPassword, "Server Key")`, used to sign the server's reply.
    pub server_key: [u8; 32],
}

impl Credentials {
    /// Derive a verifier for `username` / `password` with a fresh random salt
    /// and the standard 4096 PBKDF2 iterations.
    #[must_use]
    pub fn new(username: &str, password: &str) -> Self {
        Self::with_salt(username, password, random_bytes(16), 4096)
    }

    /// Derive a verifier with an explicit salt and iteration count (used by
    /// tests against published vectors).
    #[must_use]
    pub fn with_salt(username: &str, password: &str, salt: Vec<u8>, iterations: u32) -> Self {
        let salted = pbkdf2(password.as_bytes(), &salt, iterations);
        let client_key = hmac_sha256(&salted, b"Client Key");
        let stored_key = sha256(&client_key);
        let server_key = hmac_sha256(&salted, b"Server Key");
        Self {
            username: username.to_string(),
            salt,
            iterations,
            stored_key,
            server_key,
        }
    }
}

/// The parsed `client-first-message`.
#[derive(Debug)]
pub struct ClientFirst {
    /// The GS2 header (e.g. `n,,`), echoed back base64-encoded in the client
    /// final message's `c=` attribute.
    pub gs2_header: String,
    /// The bare message (everything after the GS2 header), part of the signed
    /// `AuthMessage`.
    pub bare: String,
    /// The user the client named in `n=` (advisory; the startup `user` governs).
    pub username: String,
    /// The client's random nonce from `r=`.
    pub client_nonce: String,
}

/// Parse a `client-first-message`: `<gs2-header>n=<user>,r=<nonce>`.
#[must_use]
pub fn parse_client_first(msg: &[u8]) -> Option<ClientFirst> {
    let s = core::str::from_utf8(msg).ok()?;
    // The GS2 header is the cbind-flag, an optional authzid, and a trailing
    // comma: two commas precede the bare message.
    let first = s.find(',')?;
    let second = first + 1 + s[first + 1..].find(',')?;
    let gs2_header = s[..=second].to_string();
    let bare = s[second + 1..].to_string();

    let mut username = None;
    let mut client_nonce = None;
    for attr in bare.split(',') {
        if let Some(v) = attr.strip_prefix("n=") {
            username = Some(decode_saslname(v));
        } else if let Some(v) = attr.strip_prefix("r=") {
            client_nonce = Some(v.to_string());
        }
    }
    Some(ClientFirst {
        gs2_header,
        bare,
        username: username?,
        client_nonce: client_nonce?,
    })
}

/// Build the `server-first-message`: `r=<combined-nonce>,s=<b64 salt>,i=<iters>`.
#[must_use]
pub fn server_first(combined_nonce: &str, salt: &[u8], iterations: u32) -> String {
    format!("r={combined_nonce},s={},i={iterations}", b64_encode(salt))
}

/// Verify a `client-final-message` against the credentials and the prior
/// messages.
///
/// On success returns the `server-final-message` (`v=<b64 signature>`) to send;
/// on failure returns an error string for logging.
///
/// # Errors
///
/// Returns `Err` if the message is malformed, the nonce does not match the one
/// the server issued, or the client proof does not verify against `stored_key`.
pub fn verify_client_final(
    creds: &Credentials,
    client_first_bare: &str,
    server_first_msg: &str,
    client_final: &[u8],
    combined_nonce: &str,
) -> Result<String, String> {
    let s = core::str::from_utf8(client_final).map_err(|_| "client final not UTF-8".to_string())?;
    let mut channel = None;
    let mut nonce = None;
    let mut proof = None;
    for attr in s.split(',') {
        if let Some(v) = attr.strip_prefix("c=") {
            channel = Some(v.to_string());
        } else if let Some(v) = attr.strip_prefix("r=") {
            nonce = Some(v.to_string());
        } else if let Some(v) = attr.strip_prefix("p=") {
            proof = Some(v.to_string());
        }
    }
    let channel = channel.ok_or("missing channel binding")?;
    let nonce = nonce.ok_or("missing nonce")?;
    let proof = proof.ok_or("missing proof")?;

    if nonce != combined_nonce {
        return Err("nonce mismatch".to_string());
    }

    // The client-final-message-without-proof is signed alongside the earlier
    // messages; it is the message up to ",p=".
    let without_proof = format!("c={channel},r={nonce}");
    let auth_message = format!("{client_first_bare},{server_first_msg},{without_proof}");

    // ClientSignature = HMAC(StoredKey, AuthMessage); recover ClientKey by
    // XORing the proof, and accept iff H(ClientKey) == StoredKey.
    let client_signature = hmac_sha256(&creds.stored_key, auth_message.as_bytes());
    let proof_bytes = b64_decode(&proof).ok_or("proof not base64")?;
    if proof_bytes.len() != 32 {
        return Err("proof wrong length".to_string());
    }
    let mut client_key = [0u8; 32];
    for (k, (&p, &sig)) in client_key
        .iter_mut()
        .zip(proof_bytes.iter().zip(client_signature.iter()))
    {
        *k = p ^ sig;
    }
    if sha256(&client_key) != creds.stored_key {
        return Err("password authentication failed".to_string());
    }

    // ServerSignature = HMAC(ServerKey, AuthMessage).
    let server_signature = hmac_sha256(&creds.server_key, auth_message.as_bytes());
    Ok(format!("v={}", b64_encode(&server_signature)))
}

/// Decode the SCRAM `=2C` / `=3D` escapes a username may carry (comma and
/// equals). Other text passes through unchanged.
fn decode_saslname(s: &str) -> String {
    s.replace("=2C", ",").replace("=3D", "=")
}

/// Standard base64 alphabet (RFC 4648) with `=` padding.
const B64: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

/// Encode bytes as padded standard base64.
#[must_use]
pub fn b64_encode(data: &[u8]) -> String {
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b0 = u32::from(chunk[0]);
        let b1 = chunk.get(1).copied().map_or(0, u32::from);
        let b2 = chunk.get(2).copied().map_or(0, u32::from);
        let n = (b0 << 16) | (b1 << 8) | b2;
        out.push(B64[(n >> 18) as usize & 0x3f] as char);
        out.push(B64[(n >> 12) as usize & 0x3f] as char);
        out.push(if chunk.len() > 1 {
            B64[(n >> 6) as usize & 0x3f] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            B64[n as usize & 0x3f] as char
        } else {
            '='
        });
    }
    out
}

/// Decode padded or unpadded standard base64, or `None` on an invalid symbol.
#[must_use]
pub fn b64_decode(s: &str) -> Option<Vec<u8>> {
    let mut bits = 0u32;
    let mut nbits = 0u32;
    let mut out = Vec::with_capacity(s.len() / 4 * 3);
    for c in s.bytes() {
        if c == b'=' {
            break;
        }
        let v = u32::try_from(B64.iter().position(|&b| b == c)?).ok()?;
        bits = (bits << 6) | v;
        nbits += 6;
        if nbits >= 8 {
            nbits -= 8;
            out.push(u8::try_from((bits >> nbits) & 0xff).unwrap_or(0));
        }
    }
    Some(out)
}

/// Generate `n` unpredictable bytes for a salt or nonce.
///
/// This is a small xorshift generator seeded from the wall clock and a
/// per-process counter, which is adequate for a SCRAM nonce (its job is to be
/// unique per exchange, not cryptographically random) and a demo-grade salt.
#[must_use]
pub fn random_bytes(n: usize) -> Vec<u8> {
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};

    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let nanos = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| {
        u64::try_from(d.as_nanos() & u128::from(u64::MAX)).unwrap_or(0)
    });
    let seq = COUNTER
        .fetch_add(1, Ordering::Relaxed)
        .wrapping_mul(0x9e37_79b9_7f4a_7c15);
    let mut state = nanos ^ seq ^ 0xd1b5_4a32_d192_ed03;
    if state == 0 {
        state = 0x1234_5678_9abc_def0;
    }

    let mut out = Vec::with_capacity(n);
    while out.len() < n {
        // xorshift64.
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        out.extend_from_slice(&state.to_le_bytes());
    }
    out.truncate(n);
    out
}

/// A printable-ASCII nonce (the SCRAM `r=` value must avoid the `,` separator).
#[must_use]
pub fn nonce() -> String {
    b64_encode(&random_bytes(18)).replace(['+', '/', '='], "A")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_round_trips() {
        for case in [&b""[..], b"f", b"fo", b"foo", b"foob", b"fooba", b"foobar"] {
            let enc = b64_encode(case);
            assert_eq!(b64_decode(&enc).unwrap(), case, "round trip {enc}");
        }
        assert_eq!(b64_encode(b"n,,"), "biws");
    }

    #[test]
    fn rfc7677_full_exchange() {
        // The worked example from RFC 7677, password "pencil".
        let salt = b64_decode("W22ZaJ0SNY7soEsUEjb6gQ==").unwrap();
        let creds = Credentials::with_salt("user", "pencil", salt, 4096);

        let client_first = b"n,,n=user,r=rOprNGfwEbeRWgbNEkqO";
        let cf = parse_client_first(client_first).unwrap();
        assert_eq!(cf.username, "user");
        assert_eq!(cf.client_nonce, "rOprNGfwEbeRWgbNEkqO");
        assert_eq!(cf.gs2_header, "n,,");

        let combined = "rOprNGfwEbeRWgbNEkqO%hvYDpWUa2RaTCAfuxFIlj)hNlF$k0";
        let sfirst = server_first(combined, &creds.salt, creds.iterations);
        assert_eq!(
            sfirst,
            "r=rOprNGfwEbeRWgbNEkqO%hvYDpWUa2RaTCAfuxFIlj)hNlF$k0,\
             s=W22ZaJ0SNY7soEsUEjb6gQ==,i=4096"
        );

        let client_final = b"c=biws,r=rOprNGfwEbeRWgbNEkqO%hvYDpWUa2RaTCAfuxFIlj)hNlF$k0,\
             p=dHzbZapWIk4jUhN+Ute9ytag9zjfMHgsqmmiz7AndVQ=";
        let server_final =
            verify_client_final(&creds, &cf.bare, &sfirst, client_final, combined).unwrap();
        assert_eq!(
            server_final,
            "v=6rriTRBi23WpRR/wtup+mMhUZUn/dB5nLTJRsjl95G4="
        );
    }

    #[test]
    fn wrong_password_is_rejected() {
        let salt = b64_decode("W22ZaJ0SNY7soEsUEjb6gQ==").unwrap();
        let creds = Credentials::with_salt("user", "not-pencil", salt, 4096);
        let cf = parse_client_first(b"n,,n=user,r=rOprNGfwEbeRWgbNEkqO").unwrap();
        let combined = "rOprNGfwEbeRWgbNEkqO%hvYDpWUa2RaTCAfuxFIlj)hNlF$k0";
        let sfirst = server_first(combined, &creds.salt, creds.iterations);
        let client_final = b"c=biws,r=rOprNGfwEbeRWgbNEkqO%hvYDpWUa2RaTCAfuxFIlj)hNlF$k0,\
             p=dHzbZapWIk4jUhN+Ute9ytag9zjfMHgsqmmiz7AndVQ=";
        let err = verify_client_final(&creds, &cf.bare, &sfirst, client_final, combined);
        assert!(err.is_err());
    }
}
