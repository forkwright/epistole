//! `TrustedProxyExtractor` coverage (forkwright/epistole#67):
//! `X-Forwarded-For` is honored only from a peer listed in
//! `trusted_proxies`, never from the header content alone.
//!
//! Every test here builds `/subscribe` requests directly (rather than
//! through `tests/integration.rs`'s `post_form`) because the whole
//! point is controlling the `ConnectInfo` peer per request — trusted,
//! untrusted, or absent.

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::Arc;

use axum::body::Body;
use axum::extract::ConnectInfo;
use axum::http::{Request, StatusCode};
use epistole::{Store, router};
use tempfile::TempDir;
use tower::ServiceExt;

mod common;
use common::{TRUSTED_PROXY_IP, test_config};

/// An untrusted direct peer's address — anything other than
/// `TRUSTED_PROXY_IP`. `test_config()` never lists this in
/// `trusted_proxies`, so a request stamped with it is exactly the
/// "reached epistole without going through the reverse proxy" case
/// forkwright/epistole#67 is about: `X-Forwarded-For` MUST be ignored.
const UNTRUSTED_PEER_A: SocketAddr =
    SocketAddr::new(IpAddr::V4(Ipv4Addr::new(203, 0, 113, 201)), 0);
const UNTRUSTED_PEER_B: SocketAddr =
    SocketAddr::new(IpAddr::V4(Ipv4Addr::new(203, 0, 113, 202)), 0);

/// The `ConnectInfo` extension axum's `into_make_service_with_connect_info`
/// injects in production (see `src/main.rs`). Tests call the router
/// directly via `.oneshot()`, bypassing that wrapper, so
/// `TrustedProxyExtractor` sees no verified peer unless a test injects
/// one — this stands in for "this request arrived through the trusted
/// reverse proxy."
fn trusted_peer() -> ConnectInfo<SocketAddr> {
    ConnectInfo(SocketAddr::new(TRUSTED_PROXY_IP, 0))
}

#[tokio::test]
#[expect(
    clippy::expect_used,
    reason = "test scaffolding - panic on fail is the desired signal"
)]
async fn rate_limit_keys_on_last_xff_entry_only() {
    // Audit findings #5 / #14: rotating X-Forwarded-For chain does NOT
    // bypass per-IP rate limiting. We use a CONSTANT last entry across
    // 8 requests (simulating NPM setting it to the real client IP) but
    // a varying earlier hop (simulating a hostile client trying to spoof
    // its way to a fresh bucket). The 7th request must 429.
    //
    // This is the TRUSTED-proxy path (`.extension(trusted_peer())`):
    // it proves rotating the untrusted PREFIX of an otherwise-honored
    // header doesn't help. `untrusted_peer_rotating_xff_does_not_evade_its_own_rate_limit`
    // below covers the complementary case — a peer that isn't trusted
    // at all, where the header is ignored outright.
    let tmp = TempDir::new().expect("tempdir");
    let store = Arc::new(Store::open(tmp.path()).expect("store"));
    let app = router(store, Arc::new(test_config(tmp.path().to_path_buf())));

    let mut last_status = StatusCode::OK;
    for i in 0..8u32 {
        let body = format!("email=spoofer{i}%40example.com");
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/subscribe")
                    .header("content-type", "application/x-www-form-urlencoded")
                    // Rotating earlier hop, but the LAST entry (NPM-set)
                    // is constant: 198.51.100.99. The extractor MUST
                    // key on .99, not the rotating prefix.
                    .header("x-forwarded-for", format!("10.0.0.{i}, 198.51.100.99"))
                    .extension(trusted_peer())
                    .body(Body::from(body))
                    .expect("req"),
            )
            .await
            .expect("response");
        last_status = resp.status();
    }
    assert_eq!(
        last_status,
        StatusCode::TOO_MANY_REQUESTS,
        "rotating XFF prefix must not bypass per-IP rate limiting"
    );
}

#[tokio::test]
#[expect(
    clippy::expect_used,
    reason = "test scaffolding - panic on fail is the desired signal"
)]
async fn untrusted_peer_rotating_xff_does_not_evade_its_own_rate_limit() {
    // forkwright/epistole#67 required negative-case fixture: a request
    // from an UNTRUSTED peer carrying a forged X-Forwarded-For resolves
    // to the peer's own real address, not the forged one. One direct
    // ("no reverse proxy") client hammers /subscribe, rotating a
    // DIFFERENT spoofed XFF value on every request — if the extractor
    // still honored the header at all, each request would land in a
    // fresh bucket and never 429. It must not: the 7th request from the
    // same real peer still trips the burst limit.
    let tmp = TempDir::new().expect("tempdir");
    let store = Arc::new(Store::open(tmp.path()).expect("store"));
    let app = router(store, Arc::new(test_config(tmp.path().to_path_buf())));

    let mut last_status = StatusCode::OK;
    for i in 0..8u32 {
        let body = format!("email=direct{i}%40example.com");
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/subscribe")
                    .header("content-type", "application/x-www-form-urlencoded")
                    .header("x-forwarded-for", format!("9.9.9.{i}"))
                    .extension(ConnectInfo(UNTRUSTED_PEER_A))
                    .body(Body::from(body))
                    .expect("req"),
            )
            .await
            .expect("response");
        last_status = resp.status();
    }
    assert_eq!(
        last_status,
        StatusCode::TOO_MANY_REQUESTS,
        "an untrusted direct peer rotating X-Forwarded-For must still hit its own rate limit"
    );
}

#[tokio::test]
#[expect(
    clippy::expect_used,
    reason = "test scaffolding - panic on fail is the desired signal"
)]
async fn two_untrusted_peers_sharing_a_forged_xff_get_independent_buckets() {
    // The direct proof that the resolved key is the PEER's real address
    // and not the header value: two different untrusted peers send the
    // exact same (forged) X-Forwarded-For. If the extractor keyed on
    // that header, peer A exhausting its budget would ALSO exhaust
    // peer B's, since they'd share one bucket. It doesn't — each peer
    // gets its own budget, proving the key came from ConnectInfo.
    const SHARED_FORGED_XFF: &str = "9.9.9.9";

    let tmp = TempDir::new().expect("tempdir");
    let store = Arc::new(Store::open(tmp.path()).expect("store"));
    let app = router(store, Arc::new(test_config(tmp.path().to_path_buf())));

    // Peer A burns its whole budget (6 allowed, 7th 429s) under the
    // shared forged header.
    let mut peer_a_last_status = StatusCode::OK;
    for i in 0..7u32 {
        let body = format!("email=a{i}%40example.com");
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/subscribe")
                    .header("content-type", "application/x-www-form-urlencoded")
                    .header("x-forwarded-for", SHARED_FORGED_XFF)
                    .extension(ConnectInfo(UNTRUSTED_PEER_A))
                    .body(Body::from(body))
                    .expect("req"),
            )
            .await
            .expect("response");
        peer_a_last_status = resp.status();
    }
    assert_eq!(
        peer_a_last_status,
        StatusCode::TOO_MANY_REQUESTS,
        "peer A must exhaust its own budget under the shared forged header"
    );

    // Peer B's FIRST request, same forged header, DIFFERENT real peer.
    // If the key were the header value, this would already be 429 —
    // peer A just spent that bucket dry.
    let peer_b_resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/subscribe")
                .header("content-type", "application/x-www-form-urlencoded")
                .header("x-forwarded-for", SHARED_FORGED_XFF)
                .extension(ConnectInfo(UNTRUSTED_PEER_B))
                .body(Body::from("email=b0%40example.com"))
                .expect("req"),
        )
        .await
        .expect("response");
    assert_eq!(
        peer_b_resp.status(),
        StatusCode::OK,
        "peer B must resolve to its OWN real address, not peer A's forged XFF value \
         — a shared bucket here means the extractor is still trusting the header"
    );
}

#[tokio::test]
#[expect(
    clippy::expect_used,
    reason = "test scaffolding - panic on fail is the desired signal"
)]
async fn trusted_peer_with_no_xff_still_rate_limits_on_its_own_address() {
    // The remaining branch: the peer IS a configured trusted proxy, but
    // sends no X-Forwarded-For at all (misconfigured proxy, or a health
    // probe that skipped it). This must not bypass the limiter or 500 —
    // it falls back to keying on the trusted peer's own address.
    let tmp = TempDir::new().expect("tempdir");
    let store = Arc::new(Store::open(tmp.path()).expect("store"));
    let app = router(store, Arc::new(test_config(tmp.path().to_path_buf())));

    let mut last_status = StatusCode::OK;
    for i in 0..7u32 {
        let body = format!("email=noxff{i}%40example.com");
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/subscribe")
                    .header("content-type", "application/x-www-form-urlencoded")
                    .extension(trusted_peer())
                    .body(Body::from(body))
                    .expect("req"),
            )
            .await
            .expect("response");
        last_status = resp.status();
    }
    assert_eq!(
        last_status,
        StatusCode::TOO_MANY_REQUESTS,
        "a trusted peer with no X-Forwarded-For must still be rate-limited, on its own address"
    );
}

#[tokio::test]
#[expect(
    clippy::expect_used,
    reason = "test scaffolding - panic on fail is the desired signal"
)]
async fn request_with_no_connect_info_and_no_xff_is_refused() {
    // The fail-closed floor: with neither a verified peer nor a header
    // to fall back to, the extractor cannot derive a key at all and the
    // governor layer surfaces that as a 500 — never as "let it through
    // unlimited." Production always supplies ConnectInfo (see
    // `into_make_service_with_connect_info` in `src/main.rs`); this is
    // the boundary condition, not a reachable production path.
    let tmp = TempDir::new().expect("tempdir");
    let store = Arc::new(Store::open(tmp.path()).expect("store"));
    let app = router(store, Arc::new(test_config(tmp.path().to_path_buf())));

    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/subscribe")
                .header("content-type", "application/x-www-form-urlencoded")
                .body(Body::from("email=noconnectinfo%40example.com"))
                .expect("req"),
        )
        .await
        .expect("response");
    assert_eq!(resp.status(), StatusCode::INTERNAL_SERVER_ERROR);
}
