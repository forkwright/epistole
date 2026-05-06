//! HTTP handlers. One module per endpoint - keeps imports light and
//! makes the router declaration in `lib.rs` line-for-line obvious.

pub(crate) mod archive;
pub(crate) mod confirm;
pub(crate) mod send;
pub(crate) mod subscribe;
pub(crate) mod unsubscribe;

/// Liveness probe. Returns 200 OK with a tiny body. Used by Caddy /
/// systemd to decide if the service is up.
pub(crate) async fn healthz() -> &'static str {
    "ok"
}
