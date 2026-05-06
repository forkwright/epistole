//! HMAC-signed time-limited tokens for confirm + unsubscribe links.
//!
//! Wire format: `base64url(payload).base64url(signature)`
//! where `payload = "{kind}|{email}|{exp_unix}"`. Signature is
//! HMAC-SHA256 over the payload with the configured secret.
//!
//! Verification:
//! 1. split on `.`
//! 2. base64-decode both parts
//! 3. recompute HMAC over the payload bytes; constant-time compare
//! 4. parse fields; reject if `kind` mismatches or `exp_unix < now()`
//!
//! No replay-protection by design - confirm/unsubscribe operations are
//! idempotent at the data level.

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use hmac::{Hmac, Mac};
use sha2::Sha256;

use crate::error::{Error, Result};

type HmacSha256 = Hmac<Sha256>;

/// What action the token authorizes. Stored in the payload so a
/// confirm-link can't be replayed as an unsubscribe.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum TokenKind {
    /// Confirm a Pending subscriber - flips state to Active.
    Confirm,
    /// Unsubscribe an Active subscriber - flips state to Unsubscribed.
    Unsubscribe,
}

impl TokenKind {
    fn as_str(self) -> &'static str {
        match self {
            Self::Confirm => "c",
            Self::Unsubscribe => "u",
        }
    }

    fn from_str(s: &str) -> Option<Self> {
        match s {
            "c" => Some(Self::Confirm),
            "u" => Some(Self::Unsubscribe),
            _ => None,
        }
    }
}

/// Decoded token. The wire form is HMAC-signed; once verified, the
/// fields here are trusted.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct Token {
    /// Authorized action.
    pub kind: TokenKind,
    /// Email of the subscriber the token applies to.
    pub email: String,
    /// Expiration time as a Unix timestamp.
    pub exp_unix: i64,
}

impl Token {
    /// Construct a new token. Useful for tests and integration callers
    /// that mint tokens outside the handler path.
    #[must_use]
    pub fn new(kind: TokenKind, email: String, exp_unix: i64) -> Self {
        Self {
            kind,
            email,
            exp_unix,
        }
    }
}

/// Sign + base64-encode a token. `secret` is the configured
/// `token_secret` - must be at least 16 bytes; longer is fine.
///
/// # Errors
///
/// Returns [`Error::Config`] when the secret is empty (a misconfig);
/// otherwise infallible.
pub fn sign(token: &Token, secret: &[u8]) -> Result<String> {
    if secret.is_empty() {
        return Err(Error::Config {
            reason: "token_secret is empty".to_owned(),
        });
    }
    let payload = format!("{}|{}|{}", token.kind.as_str(), token.email, token.exp_unix);
    let mut mac = HmacSha256::new_from_slice(secret).map_err(|e| Error::Config {
        reason: format!("HMAC key: {e}"),
    })?;
    mac.update(payload.as_bytes());
    let sig = mac.finalize().into_bytes();

    let payload_b64 = URL_SAFE_NO_PAD.encode(payload.as_bytes());
    let sig_b64 = URL_SAFE_NO_PAD.encode(sig);
    Ok(format!("{payload_b64}.{sig_b64}"))
}

/// Verify a token. Returns the decoded [`Token`] on success.
///
/// # Errors
///
/// Returns [`Error::InvalidToken`] for any signature mismatch, parse
/// failure, or expiry. The error type is intentionally coarse so the
/// caller cannot distinguish "bad signature" from "expired" from the
/// HTTP response - this protects against subtle timing oracles.
pub fn verify(raw: &str, secret: &[u8], now_unix: i64) -> Result<Token> {
    let (payload_b64, sig_b64) = raw.split_once('.').ok_or(Error::InvalidToken)?;
    let payload_bytes = URL_SAFE_NO_PAD
        .decode(payload_b64)
        .map_err(|_| Error::InvalidToken)?;
    let sig_bytes = URL_SAFE_NO_PAD
        .decode(sig_b64)
        .map_err(|_| Error::InvalidToken)?;

    let mut mac = HmacSha256::new_from_slice(secret).map_err(|_| Error::InvalidToken)?;
    mac.update(&payload_bytes);
    mac.verify_slice(&sig_bytes)
        .map_err(|_| Error::InvalidToken)?;

    let payload = std::str::from_utf8(&payload_bytes).map_err(|_| Error::InvalidToken)?;
    let mut parts = payload.splitn(3, '|');
    let kind = parts
        .next()
        .and_then(TokenKind::from_str)
        .ok_or(Error::InvalidToken)?;
    let email = parts.next().ok_or(Error::InvalidToken)?.to_owned();
    let exp_unix: i64 = parts
        .next()
        .ok_or(Error::InvalidToken)?
        .parse()
        .map_err(|_| Error::InvalidToken)?;

    if exp_unix < now_unix {
        return Err(Error::InvalidToken);
    }
    Ok(Token::new(kind, email, exp_unix))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[expect(
        clippy::expect_used,
        reason = "test scaffolding - panic on fail is the desired signal"
    )]
    fn round_trip_confirm() {
        let secret = b"this-is-only-for-tests-32-bytes!";
        let tok = Token::new(
            TokenKind::Confirm,
            "alice@example.com".into(),
            9_999_999_999,
        );
        let signed = sign(&tok, secret).expect("sign");
        assert!(!signed.is_empty(), "sign produced a non-empty token");
        let verified = verify(&signed, secret, 0).expect("verify");
        assert_eq!(verified, tok);
    }

    #[test]
    #[expect(
        clippy::expect_used,
        reason = "test scaffolding - panic on fail is the desired signal"
    )]
    fn rejects_expired() {
        let secret = b"this-is-only-for-tests-32-bytes!";
        let tok = Token::new(TokenKind::Unsubscribe, "bob@example.com".into(), 100);
        let signed = sign(&tok, secret).expect("sign");
        assert!(verify(&signed, secret, 200).is_err());
    }

    #[test]
    #[expect(
        clippy::expect_used,
        reason = "test scaffolding - panic on fail is the desired signal"
    )]
    fn rejects_tampered() {
        let secret = b"this-is-only-for-tests-32-bytes!";
        let tok = Token::new(
            TokenKind::Confirm,
            "carol@example.com".into(),
            9_999_999_999,
        );
        let signed = sign(&tok, secret).expect("sign");
        // Mutate the last char of the payload - signature now invalid.
        let mut tampered: Vec<char> = signed.chars().collect();
        let dot = tampered.iter().position(|c| *c == '.').unwrap_or(0);
        if dot > 0 {
            let target = dot - 1;
            tampered[target] = if tampered[target] == 'a' { 'b' } else { 'a' };
        }
        let tampered: String = tampered.into_iter().collect();
        assert!(verify(&tampered, secret, 0).is_err());
    }
}
