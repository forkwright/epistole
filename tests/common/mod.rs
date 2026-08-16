//! Shared scaffolding for the integration test binaries.
//!
//! Each file under `tests/` compiles as its own crate, so without this
//! module every one of them carries its own copy of the same config
//! builder.

use std::path::PathBuf;
use std::sync::Arc;

use epistole::{
    Config,
    config::{Brand, Smtp},
    mailer::StubMailer,
};
use secrecy::SecretString;

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
