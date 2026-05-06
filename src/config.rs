//! Configuration loader. The on-disk shape is TOML; the parsed struct
//! is the single source of truth at runtime. Secrets are wrapped in
//! [`secrecy::SecretString`] so they zeroize on drop and don't leak via
//! `Debug` / `Display`.

use std::path::{Path, PathBuf};

use secrecy::SecretString;
use serde::Deserialize;

use crate::error::{Error, Result};

/// Server configuration. Loaded from a TOML file at startup; never
/// reloaded - restart the service to apply changes.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// Listen address, e.g. `127.0.0.1:9090`.
    pub bind: String,
    /// fjall keyspace directory. One process per directory.
    pub data_dir: PathBuf,
    /// Public-facing base URL - used to build confirm / unsubscribe links.
    pub base_url: String,
    /// Brand identity for outbound mail.
    pub brand: Brand,
    /// SMTP relay credentials.
    pub smtp: Smtp,
    /// HMAC secret for signing confirm/unsubscribe tokens. Base64-encoded
    /// 32 bytes recommended; never commit; load from environment via the
    /// deploy host's `/etc/epistole.env`.
    pub token_secret: SecretString,
    /// Bearer token required for `/send`. Operator-only.
    pub send_auth_token: SecretString,
}

impl std::fmt::Debug for Config {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Config")
            .field("bind", &self.bind)
            .field("data_dir", &self.data_dir)
            .field("base_url", &self.base_url)
            .field("brand", &self.brand)
            .field("smtp", &self.smtp)
            .field("token_secret", &"<redacted>")
            .field("send_auth_token", &"<redacted>")
            .finish()
    }
}

/// Brand strings stamped onto outbound mail.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Brand {
    /// Display name (used in JSON-LD Organization, mail "From" name, page titles).
    pub name: String,
    /// RFC 5322 mailbox the relay sends from. Must align with SPF/DKIM on the relay.
    pub from_address: String,
    /// Optional Reply-To header for visitor replies (often the contact@ address).
    pub reply_to: Option<String>,
}

/// SMTP relay settings (Postmark / Mailgun / etc.).
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Smtp {
    /// Relay hostname (e.g. `smtp.postmarkapp.com`).
    pub host: String,
    /// Submission port (typically 587 with STARTTLS, or 465 for implicit TLS).
    pub port: u16,
    /// Relay account username (often the same as the API token for Postmark).
    pub username: String,
    /// Relay account password / API token.
    pub password: SecretString,
}

impl std::fmt::Debug for Smtp {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Smtp")
            .field("host", &self.host)
            .field("port", &self.port)
            .field("username", &self.username)
            .field("password", &"<redacted>")
            .finish()
    }
}

impl Config {
    /// Load and parse the TOML config at `path`.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Config`] if the file cannot be read or parsed.
    pub fn load(path: &Path) -> Result<Self> {
        let bytes = std::fs::read_to_string(path).map_err(|e| Error::Config {
            reason: format!("read {}: {e}", path.display()),
        })?;
        let cfg: Self = toml::from_str(&bytes).map_err(|e| Error::Config {
            reason: format!("parse {}: {e}", path.display()),
        })?;
        Ok(cfg)
    }
}
