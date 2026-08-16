//! Outbound mail transport. [`Mailer`] is the seam between a handler and
//! the wire: production wires [`SmtpMailer`] (a real relay connection),
//! tests wire [`StubMailer`] (an in-memory transport that never opens a
//! socket). Both are constructed once at startup / test-setup and shared
//! behind an `Arc` — [`SmtpMailer`]'s pooled connection is only reused
//! when the same transport instance sends every message.

use std::future::Future;
use std::pin::Pin;

use lettre::transport::smtp::authentication::Credentials;
use lettre::transport::stub::AsyncStubTransport;
use lettre::{AsyncSmtpTransport, AsyncTransport, Message, Tokio1Executor};
use secrecy::ExposeSecret;

use crate::config::Smtp;
use crate::error::{Error, Result};

/// Sends one composed [`Message`] and reports success or a relay-level
/// failure. A handler depends on this trait, never on `lettre`'s
/// transport types directly, so a test can substitute [`StubMailer`]
/// without a live network.
///
/// WHY not a native `async fn` in the trait: `AppState`/[`router`] pass
/// this behind `Arc<dyn Mailer>` so a test can substitute [`StubMailer`]
/// without a live relay — a native `async fn` in a trait is not
/// dyn-compatible (E0038). `send` is desugared by hand to the boxed
/// future a native `async fn` would otherwise expand to; the fleet bans
/// the `async-trait` crate (`RUST.md` "Dependencies") because stable
/// native async-fn-in-trait covers the non-dyn case, but that coverage
/// doesn't reach the dyn-compatible one, so the manual desugar is the
/// standards-compliant way to keep this seam object-safe.
pub trait Mailer: Send + Sync {
    /// Hand `message` to the transport.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Smtp`] when the relay rejects the message or the
    /// connection fails.
    fn send(&self, message: Message) -> Pin<Box<dyn Future<Output = Result<()>> + Send + '_>>;
}

/// Production mailer: a pooled [`AsyncSmtpTransport`] connected to the
/// relay named in [`Smtp`].
pub struct SmtpMailer {
    transport: AsyncSmtpTransport<Tokio1Executor>,
}

impl SmtpMailer {
    /// Build the transport from `smtp`. Connects lazily — the first real
    /// network I/O happens on the first [`Mailer::send`] call, so a
    /// misconfigured host doesn't fail this constructor.
    ///
    /// WHY the port decides the TLS strategy: `AsyncSmtpTransport::relay`
    /// wraps the connection in TLS from the first byte (the
    /// `SMTPS`/implicit-TLS convention, port 465); `starttls_relay`
    /// connects in cleartext and upgrades via the `STARTTLS` command (the
    /// submission convention, port 587, and Postmark/Mailgun's documented
    /// default). `.port()` on the builder only changes which port is
    /// dialed, not which of these two handshakes runs — dialing 465 with
    /// a `STARTTLS`-shaped client (or 587 with an implicit-TLS-shaped
    /// one) fails the handshake against every mainstream relay, so the
    /// choice has to track the configured port rather than default to
    /// one unconditionally.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Smtp`] when the relay hostname fails to resolve
    /// into a TLS-capable transport builder (e.g. an empty or malformed
    /// `host`).
    pub fn from_config(smtp: &Smtp) -> Result<Self> {
        const IMPLICIT_TLS_PORT: u16 = 465;

        let credentials = Credentials::new(
            smtp.username.expose_secret().to_owned(),
            smtp.password.expose_secret().to_owned(),
        );
        let builder = if smtp.port == IMPLICIT_TLS_PORT {
            AsyncSmtpTransport::<Tokio1Executor>::relay(&smtp.host)
        } else {
            AsyncSmtpTransport::<Tokio1Executor>::starttls_relay(&smtp.host)
        }
        .map_err(|e| Error::Smtp {
            reason: format!("build transport for {}: {e}", smtp.host),
        })?;
        let transport: AsyncSmtpTransport<Tokio1Executor> =
            builder.port(smtp.port).credentials(credentials).build();
        Ok(Self { transport })
    }
}

impl Mailer for SmtpMailer {
    fn send(&self, message: Message) -> Pin<Box<dyn Future<Output = Result<()>> + Send + '_>> {
        Box::pin(async move {
            self.transport
                .send(message)
                .await
                .map(|_response| ())
                .map_err(|e| Error::Smtp {
                    reason: format!("relay send: {e}"),
                })
        })
    }
}

/// Test/dev double: records every message in memory instead of opening a
/// socket. Never wired into the production binary — `main.rs` always
/// builds [`SmtpMailer`]. Exposed publicly (like [`crate::Store`]
/// and [`crate::Config`]) so integration tests and out-of-process tools
/// can build an [`crate::AppState`] without a live relay.
pub struct StubMailer {
    transport: AsyncStubTransport,
}

impl StubMailer {
    /// A stub that accepts every message.
    #[must_use]
    pub fn accepting() -> Self {
        Self {
            transport: AsyncStubTransport::new_ok(),
        }
    }

    /// A stub that rejects every message with [`Error::Smtp`] — for
    /// exercising a handler's failure path.
    #[must_use]
    pub fn rejecting() -> Self {
        Self {
            transport: AsyncStubTransport::new_error(),
        }
    }

    /// Count of messages actually handed to [`AsyncTransport::send_raw`]
    /// so far. The idempotency contract for `/send` is that a replayed
    /// `send_id` must not grow this count — asserting on the HTTP
    /// response alone cannot show that, since a handler could return an
    /// identical response while re-sending underneath.
    pub async fn sent_count(&self) -> usize {
        self.transport.messages().await.len()
    }
}

impl Mailer for StubMailer {
    fn send(&self, message: Message) -> Pin<Box<dyn Future<Output = Result<()>> + Send + '_>> {
        Box::pin(async move {
            self.transport
                .send(message)
                .await
                .map(|_ok| ())
                .map_err(|_e| Error::Smtp {
                    reason: "stub mailer configured to reject".to_owned(),
                })
        })
    }
}
