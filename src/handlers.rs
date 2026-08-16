//! HTTP handlers. One module per endpoint - keeps imports light and
//! makes the router declaration in `lib.rs` line-for-line obvious.

pub(crate) mod archive;
pub(crate) mod confirm;
pub(crate) mod send;
pub(crate) mod subscribe;
pub(crate) mod unsubscribe;
pub(crate) mod webhooks;

use axum::http::HeaderMap;
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;

/// Liveness probe. Returns 200 OK with a tiny body. Used by Caddy /
/// systemd to decide if the service is up.
pub(crate) async fn healthz() -> &'static str {
    "ok"
}

/// Authenticate a bearer token in `Authorization: Bearer <token>` against
/// `expected`. Shared by `/send` and `/webhooks/delivery-events`, each
/// checked against its own configured secret.
///
/// Hash-then-compare: SHA256 the presented token and the expected token,
/// compare the digests in constant time. This is more robust than the
/// branch-and-equalize approach because the final compare is always
/// against fixed-size 32-byte digests — no length-leak path, no
/// "uniform work on length mismatch" code that future refactors can
/// quietly drop.
pub(crate) fn check_bearer(headers: &HeaderMap, expected: &str) -> bool {
    let presented = headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .unwrap_or("");
    if presented.is_empty() {
        return false;
    }
    let presented_hash = Sha256::digest(presented.as_bytes());
    let expected_hash = Sha256::digest(expected.as_bytes());
    presented_hash.ct_eq(&expected_hash).into()
}
