//! Configuration loader. The on-disk shape is TOML; the parsed struct
//! is the single source of truth at runtime. Secrets are wrapped in
//! [`secrecy::SecretString`] so they zeroize on drop and don't leak via
//! `Debug` / `Display`.

use std::path::{Path, PathBuf};

use secrecy::SecretString;
use serde::Deserialize;

use crate::error::{Error, Result};

#[cfg(test)]
#[path = "config_env_tests.rs"]
mod config_env_tests;

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
    /// Load and parse the TOML config at `path`. Secrets that match
    /// `${VAR}` syntax are looked up from the process environment after
    /// TOML parsing — this keeps the on-disk config free of secrets so
    /// the file can be world-readable (or at least file-system-shared)
    /// while real credentials live in `/etc/epistole.env` (0600 root).
    ///
    /// Substitution is intentionally narrow: only `token_secret`,
    /// `send_auth_token`, and `smtp.password` are env-resolved. Other
    /// fields are taken verbatim. A value that doesn't look like
    /// `${VAR}` is used as-is.
    ///
    /// After resolution, secret strength is validated:
    ///   - `token_secret` — minimum 32 bytes (`HMAC-SHA256` keys should
    ///     match the digest's 32-byte security parameter)
    ///   - `send_auth_token` — minimum 24 bytes
    ///   - blocklist of common placeholder strings (`change-me`,
    ///     `phase-0-stub`, `REPLACE_WITH`, etc.)
    ///
    /// # Errors
    ///
    /// Returns [`Error::Config`] if the file cannot be read or parsed,
    /// if an `${VAR}` reference cannot be resolved, or if a secret
    /// fails the strength check.
    pub fn load(path: &Path) -> Result<Self> {
        let bytes = std::fs::read_to_string(path).map_err(|e| Error::Config {
            reason: format!("read {}: {e}", path.display()),
        })?;
        let mut cfg: Self = toml::from_str(&bytes).map_err(|e| Error::Config {
            reason: format!("parse {}: {e}", path.display()),
        })?;
        cfg.token_secret = resolve_secret_env(&cfg.token_secret, "token_secret")?;
        cfg.send_auth_token = resolve_secret_env(&cfg.send_auth_token, "send_auth_token")?;
        cfg.smtp.password = resolve_secret_env(&cfg.smtp.password, "smtp.password")?;
        validate_secret_strength(&cfg.token_secret, "token_secret", 32)?;
        validate_secret_strength(&cfg.send_auth_token, "send_auth_token", 24)?;
        // Reaudit finding #27: smtp.password skipped strength check.
        // The example config's literal `REPLACE_WITH_POSTMARK_TOKEN`
        // would have slipped past boot. Validate it the same way as
        // the other two secrets. Length floor is lower (16) because
        // SMTP relay tokens are typically shorter than fleet-minted
        // HMAC keys; the blocked-pattern check is the load-bearing
        // gate against operator copy-paste mistakes.
        validate_secret_strength(&cfg.smtp.password, "smtp.password", 16)?;
        Ok(cfg)
    }
}

/// Patterns that operator runbooks have used as fill-the-blank text in
/// `epistole.toml`. Any one appearing in a production secret means the
/// deploy missed the secret-substitution step; the service refuses to
/// start. Extend if a new template adds another pattern.
///
/// Reaudit finding #29 added the runbook-literal patterns: codex's
/// kimi-4 agent demonstrated that `send_auth_token = "<SEND_AUTH_TOKEN
/// from step 4>"` (verbatim from DEPLOY.md) booted the service with
/// the public literal as the bearer token.
const BLOCKED_SECRET_SUBSTRS: &[&str] = &[
    "change-me",
    "changeme",
    "replace_with",
    "replace-me",
    "phase-0-stub",
    "phase0stub",
    "your-secret-here",
    "example",
    "default",
    "placeholder",
    "todo",
    // DEPLOY.md template literals — reaudit #29.
    "from step",
    "<token_secret",
    "<send_auth_token",
    "<smtp_password",
    "<base64_random",
    "<postmark_token",
];

/// Reject placeholder / weak secrets at startup. Any production deploy
/// that ships with a literal `REPLACE_WITH...` or `phase-0-stub...` in
/// the env file should fail to start, not silently accept a guessable
/// `HMAC` key.
fn validate_secret_strength(value: &SecretString, field: &str, min_bytes: usize) -> Result<()> {
    use secrecy::ExposeSecret;
    let raw = value.expose_secret();
    if raw.len() < min_bytes {
        return Err(Error::Config {
            reason: format!(
                "{field}: too short — got {} bytes, minimum {min_bytes}",
                raw.len()
            ),
        });
    }
    let lower = raw.to_ascii_lowercase();
    for pattern in BLOCKED_SECRET_SUBSTRS {
        if lower.contains(pattern) {
            return Err(Error::Config {
                reason: format!(
                    "{field}: contains placeholder pattern '{pattern}' — generate a real secret \
                     (head -c 32 /dev/urandom | base64 -w 0) and set it in /etc/epistole.env"
                ),
            });
        }
    }
    Ok(())
}

/// If `value` is exactly one valid `${VAR}` env reference, look up
/// `VAR` and return its value. Anything else — extra braces, leading or
/// trailing whitespace, malformed name, partial substitution, embedded
/// `${...}` inside a longer literal — is an error.
///
/// Reaudit finding #29: the previous implementation accepted ANY input
/// that didn't strip cleanly to `${...}` as a literal secret. So
/// `${TOKEN_SECRET}}padding` (typo with extra brace), `  ${TOKEN_SECRET}`
/// (whitespace), or `${TOKEN_SECRET} (token)` (operator added a comment)
/// all silently became the literal string used as the HMAC key. With
/// the strict regex below, every malformed reference fails closed.
fn resolve_secret_env(value: &SecretString, field: &str) -> Result<SecretString> {
    use secrecy::ExposeSecret;
    let raw = value.expose_secret();

    // If the value contains `${` ANYWHERE, it must be the entire value
    // and match `^\$\{[A-Z_][A-Z0-9_]*\}$`. Anything else is a deploy
    // mistake.
    if raw.contains("${") {
        if !is_valid_env_ref(raw) {
            return Err(Error::Config {
                reason: format!(
                    "{field}: malformed env reference '{raw}' — expected exactly '${{NAME}}' \
                     where NAME matches [A-Z_][A-Z0-9_]*. Trim whitespace, fix braces, and \
                     don't embed the reference in a larger string."
                ),
            });
        }
        // raw is `${NAME}`; strip and look up.
        let name = raw
            .strip_prefix("${")
            .and_then(|s| s.strip_suffix('}'))
            .unwrap_or(raw);
        let resolved = std::env::var(name).map_err(|_| Error::Config {
            reason: format!("{field}: ${{{name}}} referenced but env var unset"),
        })?;
        if resolved.is_empty() {
            return Err(Error::Config {
                reason: format!("{field}: ${{{name}}} env var is empty"),
            });
        }
        return Ok(SecretString::from(resolved));
    }

    // No `${` in the value — caller passed a literal. Caller chains a
    // strength check (see `validate_secret_strength`) which catches the
    // dangerous-literal cases.
    Ok(value.clone())
}

/// Strict env-reference grammar. `^\$\{[A-Z_][A-Z0-9_]*\}$` — exactly
/// one reference, no surrounding whitespace, no embedded text.
fn is_valid_env_ref(raw: &str) -> bool {
    let Some(name) = raw.strip_prefix("${").and_then(|s| s.strip_suffix('}')) else {
        return false;
    };
    if name.is_empty() {
        return false;
    }
    let mut chars = name.chars();
    let first = chars.next().unwrap_or('\0');
    if !(first.is_ascii_uppercase() || first == '_') {
        return false;
    }
    chars.all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_')
}
