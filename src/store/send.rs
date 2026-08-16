//! Send + delivery records: CRUD against the `sends` and `deliveries`
//! keyspaces.

use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

use crate::error::{Error, Result};
use crate::send_id::SendId;
use crate::store::Store;

/// One newsletter send. Created by `POST /send`; persisted before
/// delivery so a crash mid-fan-out can resume.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[non_exhaustive]
pub struct Send {
    /// Send id.
    pub id: SendId,
    /// Subject line stamped onto every outbound mail.
    pub subject: String,
    /// Rendered HTML body (markdown was rendered to HTML at send time).
    pub body_html: String,
    /// When the send was recorded (operator-side timestamp).
    pub sent_at: OffsetDateTime,
}

/// Per-recipient delivery outcome. Keyed `<send_id>/<email>` so a range
/// scan over `<send_id>/` returns every delivery for one send.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[non_exhaustive]
pub struct Delivery {
    /// Foreign key to [`Send::id`].
    pub send_id: SendId,
    /// Recipient (lowercased email; matches a [`Subscriber::email`]).
    pub email: String,
    /// Outcome.
    pub status: DeliveryStatus,
    /// When the outcome was recorded.
    pub at: OffsetDateTime,
    /// Relay error message when `status == Failed`.
    pub error: Option<String>,
}

impl Delivery {
    /// Construct a new delivery record. Useful for tests and
    /// integration callers that seed store state outside the handler
    /// path — `#[non_exhaustive]` blocks the struct-literal form from
    /// outside this crate.
    #[must_use]
    pub fn new(
        send_id: SendId,
        email: String,
        status: DeliveryStatus,
        at: OffsetDateTime,
        error: Option<String>,
    ) -> Self {
        Self {
            send_id,
            email,
            status,
            at,
            error,
        }
    }
}

/// Per-recipient delivery state.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
#[non_exhaustive]
pub enum DeliveryStatus {
    /// Send queued; relay has not yet acknowledged.
    Queued,
    /// Relay accepted the message.
    Sent,
    /// Relay rejected the message at send time.
    Failed,
    /// The relay's bounce webhook reported this address undeliverable
    /// after an earlier `Sent` outcome.
    Bounced,
    /// The relay's complaint webhook reported a spam complaint after an
    /// earlier `Sent` outcome.
    Complained,
}

/// Decode one `sends` record from its fjall guard.
///
/// Shared by [`Store::iter_sends`] and [`Store::recent_sends`] so both
/// report identical error text for the same failure.
fn decode_send(guard: fjall::Guard) -> Result<Send> {
    let v = guard.value().map_err(|e| Error::Store {
        reason: format!("sends iter: {e}"),
    })?;
    serde_json::from_slice::<Send>(&v).map_err(|e| Error::Store {
        reason: format!("sends decode: {e}"),
    })
}

impl Store {
    /// Iterate every record in the `sends` partition, oldest first.
    ///
    /// The iterator is lazy, so it costs one record at a time. Callers
    /// that render a response must still bound how many they pull —
    /// prefer [`Store::recent_sends`], which carries the bound.
    ///
    /// # Errors
    ///
    /// Each item resolves to [`Error::Store`] on fjall read or decode
    /// failure.
    pub fn iter_sends(&self) -> Result<impl Iterator<Item = Result<Send>>> {
        Ok(self.sends.iter().map(decode_send))
    }

    /// Read at most `limit` of the most recent sends, newest first.
    ///
    /// WHY: send ids are ULIDs, so the partition is already ordered
    /// oldest-first by key. Iterating in reverse yields newest-first
    /// without sorting, and stopping at `limit` bounds both the decode
    /// work and the peak memory one call costs — the archive index must
    /// not let history size alone decide either.
    ///
    /// Requesting `limit + 1` and checking for the extra record is how a
    /// caller detects that more history exists without a second query.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Store`] on a fjall read or decode failure.
    pub fn recent_sends(&self, limit: usize) -> Result<Vec<Send>> {
        self.sends
            .iter()
            .rev()
            .take(limit)
            .map(decode_send)
            .collect()
    }

    /// Look up one send by its stable send id.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Store`] on a fjall read failure or JSON decode
    /// failure.
    pub fn send_get(&self, send_id: &SendId) -> Result<Option<Send>> {
        let raw = self
            .sends
            .get(send_id.to_string().as_bytes())
            .map_err(|e| Error::Store {
                reason: format!("send_get {send_id}: {e}"),
            })?;
        match raw {
            None => Ok(None),
            Some(bytes) => serde_json::from_slice(&bytes)
                .map(Some)
                .map_err(|e| Error::Store {
                    reason: format!("decode send {send_id}: {e}"),
                }),
        }
    }

    /// Insert or replace a send record, durably.
    ///
    /// `POST /send` returns the `send_id` to the operator, who may treat
    /// it as a handle to an existing send, so the record is fsynced before
    /// this returns.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Store`] on a fjall write failure, a journal sync
    /// failure, or serde encode failure.
    pub fn send_put(&self, send: &Send) -> Result<()> {
        let bytes = serde_json::to_vec(send).map_err(|e| Error::Store {
            reason: format!("encode send {}: {e}", send.id),
        })?;
        self.sends
            .insert(send.id.to_string().as_bytes(), bytes)
            .map_err(|e| Error::Store {
                reason: format!("sends partition write: {e}"),
            })?;
        self.persist_acknowledged()
    }

    /// Key layout for one delivery row: `<send_id>/<email>`. Keeps every
    /// recipient of one send lexically adjacent, and makes "does this
    /// `send_id` already have a row for this recipient" ([`Store::delivery_get`])
    /// a single point lookup.
    fn delivery_key(send_id: &SendId, email: &str) -> Vec<u8> {
        format!("{send_id}/{}", email.to_ascii_lowercase()).into_bytes()
    }

    /// Look up one delivery row by `(send_id, email)`.
    ///
    /// `/send`'s per-recipient idempotency check: a `Some` result means
    /// this `send_id` already attempted this recipient, so a retry must
    /// skip it rather than send (and record) a second time.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Store`] on a fjall read or decode failure.
    pub fn delivery_get(&self, send_id: &SendId, email: &str) -> Result<Option<Delivery>> {
        let key = Self::delivery_key(send_id, email);
        let raw = self.deliveries.get(&key).map_err(|e| Error::Store {
            reason: format!("delivery_get {send_id}/{email}: {e}"),
        })?;
        match raw {
            None => Ok(None),
            Some(bytes) => serde_json::from_slice(&bytes)
                .map(Some)
                .map_err(|e| Error::Store {
                    reason: format!("decode delivery {send_id}/{email}: {e}"),
                }),
        }
    }

    /// Insert or replace one delivery row, durably.
    ///
    /// Acknowledged (fsynced before return), matching [`Store::subscriber_put`]
    /// and [`Store::send_put`]: this row is exactly what
    /// [`Store::delivery_get`]'s idempotency check reads on a retry, so a
    /// row that is merely buffered when the process is answering "did I
    /// already send this" is a row that a power-loss-then-retry can lose,
    /// silently reopening the recipient to a duplicate send. The relay
    /// round-trip this call follows already costs far more than one
    /// fsync, so the extra latency is not a meaningful addition.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Store`] on a fjall write, journal sync, or encode
    /// failure.
    pub fn delivery_put(&self, delivery: &Delivery) -> Result<()> {
        let key = Self::delivery_key(&delivery.send_id, &delivery.email);
        let bytes = serde_json::to_vec(delivery).map_err(|e| Error::Store {
            reason: format!(
                "encode delivery {}/{}: {e}",
                delivery.send_id, delivery.email
            ),
        })?;
        self.deliveries
            .insert(key, bytes)
            .map_err(|e| Error::Store {
                reason: format!("delivery_put {}/{}: {e}", delivery.send_id, delivery.email),
            })?;
        self.persist_acknowledged()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// NOTE: what these tests can and cannot establish.
    ///
    /// They reopen the keyspace and assert the acknowledged transition is
    /// still there, which catches a transition that was never written, was
    /// written to the wrong key, or was undone by a later sweep.
    ///
    /// They do NOT prove the `fdatasync` happened. Buffered writes reach
    /// the OS page cache, so they survive both process exit and `SIGKILL`;
    /// only cutting power to the machine distinguishes `Buffer` from
    /// `SyncData`, and no in-process test can do that. The structural
    /// guarantee that every acknowledged write picks `SyncData` is enforced
    /// instead by `tests/fitness/main.rs`, which fails if a caller reaches
    /// past `Store` to a keyspace handle.
    ///
    /// A graceful drop is required here rather than incidental: fjall holds
    /// a lock file for its single-writer contract, so the first handle must
    /// release before the same path can be opened again.
    ///
    /// The `TempDir` is returned alongside the handle because dropping it
    /// deletes the directory out from under the reopened store.
    #[expect(
        clippy::expect_used,
        reason = "test scaffolding - panic on fail is the desired signal"
    )]
    fn write_then_reopen<W>(write: W) -> (Store, tempfile::TempDir)
    where
        W: FnOnce(&Store),
    {
        let path = tempfile::TempDir::new().expect("tempdir");
        {
            let store = Store::open(path.path()).expect("open");
            write(&store);
        }
        let reopened = Store::open(path.path()).expect("reopen");
        (reopened, path)
    }

    #[test]
    #[expect(
        clippy::expect_used,
        reason = "test scaffolding - panic on fail is the desired signal"
    )]
    fn an_acknowledged_send_survives_reopen() {
        // `POST /send` hands the send id back to the operator, so the
        // record it names must still exist after a restart.
        let now = OffsetDateTime::now_utc();
        let send_id = SendId::generate();
        let (store, _dir) = write_then_reopen(|store| {
            store
                .send_put(&Send {
                    id: send_id,
                    subject: "Issue 1".to_owned(),
                    body_html: "<p>hello</p>".to_owned(),
                    sent_at: now,
                })
                .expect("send_put");
        });

        let send = store
            .send_get(&send_id)
            .expect("read")
            .expect("send survived reopen");
        assert_eq!(send.subject, "Issue 1");
    }

    #[test]
    #[expect(
        clippy::expect_used,
        reason = "test scaffolding - panic on fail is the desired signal"
    )]
    fn delivery_put_then_get_round_trips() {
        let tmp = tempfile::TempDir::new().expect("tempdir");
        let store = Store::open(tmp.path()).expect("store");
        let send_id = SendId::generate();
        let now = OffsetDateTime::now_utc();

        store
            .delivery_put(&Delivery {
                send_id,
                email: "reader@example.com".to_owned(),
                status: DeliveryStatus::Sent,
                at: now,
                error: None,
            })
            .expect("put");

        let got = store
            .delivery_get(&send_id, "reader@example.com")
            .expect("get")
            .expect("row present");
        assert_eq!(got.status, DeliveryStatus::Sent);
    }

    #[test]
    #[expect(
        clippy::expect_used,
        reason = "test scaffolding - panic on fail is the desired signal"
    )]
    fn delivery_get_returns_none_for_an_unrecorded_pair() {
        let tmp = tempfile::TempDir::new().expect("tempdir");
        let store = Store::open(tmp.path()).expect("store");
        let send_id = SendId::generate();
        assert!(
            store
                .delivery_get(&send_id, "nobody@example.com")
                .expect("get")
                .is_none()
        );
    }
}
