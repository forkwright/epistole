//! Shared scaffolding for the integration test binaries.
//!
//! Each file under `tests/` compiles as its own crate, so without this
//! module every one of them carries its own copy of the same config
//! builder.

use std::net::{IpAddr, Ipv4Addr};
use std::path::PathBuf;
use std::sync::Arc;

use epistole::{
    Config,
    config::{Brand, Smtp},
    mailer::StubMailer,
};
use secrecy::SecretString;

/// The reverse-proxy peer address `test_config()` trusts. A request must
/// carry this as its `ConnectInfo` peer (each `tests/*.rs` file builds
/// its own small helper for that — see e.g. `tests/integration.rs`'s
/// `trusted_peer()` — since every file under `tests/` compiles as a
/// separate crate) for `TrustedProxyExtractor` to honor its
/// `X-Forwarded-For`. Any other peer, or no injected `ConnectInfo` at
/// all, is an untrusted direct connection by design — `trusted_proxies`
/// is not a wildcard.
pub const TRUSTED_PROXY_IP: IpAddr = IpAddr::V4(Ipv4Addr::new(198, 51, 100, 1));

/// Config for a tempdir-backed test server.
///
/// The token secret and bearer token are fixed so a test can mint a
/// token directly and present it over HTTP.
pub fn test_config(data_dir: PathBuf) -> Config {
    Config {
        bind: "127.0.0.1:0".to_owned(),
        data_dir,
        base_url: "https://letters.example.com".to_owned(),
        brand: Brand {
            name: "Test Brand".to_owned(),
            from_address: "letters@example.com".to_owned(),
            reply_to: None,
        },
        smtp: Smtp {
            host: "127.0.0.1".to_owned(),
            port: 0,
            username: SecretString::from("user".to_owned()),
            password: SecretString::from("pass".to_owned()),
        },
        token_secret: SecretString::from("test-secret-32-bytes-padding-aaaa".to_owned()),
        send_auth_token: SecretString::from("operator-bearer-test".to_owned()),
        webhook_auth_token: SecretString::from("webhook-bearer-test".to_owned()),
        send_cap_per_hour: 500,
        send_cap_per_day: 2000,
        trusted_proxies: vec![TRUSTED_PROXY_IP.into()],
    }
}

/// A fresh [`StubMailer`] that accepts every message. Each call returns
/// an independent instance — one test's send count never leaks into
/// another's. Returns the concrete type (not `Arc<dyn Mailer>`) so a
/// caller can keep its own clone and later call `.sent_count()`; passing
/// it straight to `router()` coerces to the trait object automatically.
#[must_use]
pub fn test_mailer() -> Arc<StubMailer> {
    Arc::new(StubMailer::accepting())
}
