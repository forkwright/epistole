//! Storage layer. fjall database with three keyspaces:
//!
//! - `subscribers` - keyed by lowercased email; value is a JSON-encoded
//!   [`Subscriber`] record. New confirms create `Active` rows directly;
//!   `Pending` is retained only for legacy pre-stateless-token rows.
//! - `sends` - keyed by send id (ULID-ish lexicographic timestamp);
//!   value is a JSON-encoded [`Send`] record (subject + rendered HTML +
//!   sent timestamp).
//! - `deliveries` - keyed by `<send_id>/<email>`; value is a
//!   JSON-encoded [`Delivery`] record (status + timestamp + error).
//!
//! Single-writer per database per fjall's contract - out-of-process
//! tools must talk to the running server via HTTP, not open the database
//! directly.
//!
//! # Durability boundary
//!
//! fjall's `Keyspace::insert`/`remove` persist with [`PersistMode::Buffer`],
//! which reaches the OS page cache but survives no power loss or OS crash.
//! Returning HTTP success off a buffered write acknowledges a consent
//! decision the store may not hold after a hard reset.
//!
//! Every write therefore states its durability class, and the keyspace
//! handles are private so no caller can bypass that choice:
//!
//! - **Acknowledged** ([`Store::subscriber_put`], [`Store::send_put`]) -
//!   fsynced via [`PersistMode::SyncData`] *before* the call returns, so a
//!   handler that has returned 200 has state the store will still hold
//!   after a power loss.
//! - **Reconstructible** ([`Store::purge_expired_pending`]) - a cleanup no
//!   client is waiting on. A lost purge replays on the next run, so it
//!   syncs once at the end rather than per row.
//!
//! NOTE: fjall journals all keyspaces of one `Database` together, so an
//! acknowledged write also makes every earlier buffered write durable. The
//! classes bound *latency*, not correctness: no acknowledged transition can
//! be reordered behind a buffered one.

use std::path::Path;

use fjall::{Database, Keyspace, KeyspaceCreateOptions, PersistMode};
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

use crate::error::{Error, Result};
use crate::send_id::SendId;

/// Subscriber lifecycle state. Tokens reference one of these implicitly
/// via their `kind` field - a `confirm` token creates or confirms an
/// `Active` subscriber, an `unsubscribe` token only applies to `Active`.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
#[non_exhaustive]
pub enum SubscriberState {
    /// Subscriber submitted email but has not clicked confirm yet.
    Pending,
    /// Subscriber confirmed; receives newsletter sends.
    Active,
    /// Subscriber clicked unsubscribe; excluded from sends.
    Unsubscribed,
}

/// Subscriber record persisted to the `subscribers` partition.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[non_exhaustive]
pub struct Subscriber {
    /// Lowercased email address. Acts as the partition key.
    pub email: String,
    /// Lifecycle state.
    pub state: SubscriberState,
    /// When the subscriber first submitted via `/subscribe`.
    pub created_at: OffsetDateTime,
    /// When the subscriber clicked the confirm link, if ever.
    pub confirmed_at: Option<OffsetDateTime>,
    /// When the subscriber clicked the unsubscribe link, if ever.
    pub unsubscribed_at: Option<OffsetDateTime>,
    /// Consent generation (forkwright/epistole#65). Bumped by
    /// `POST /unsubscribe` on a successful Active -> Unsubscribed
    /// transition. A confirm or unsubscribe token is minted against the
    /// generation read at mint time; a handler only lets the token drive
    /// a transition while that value still matches this field, which is
    /// what distinguishes a token minted before a later consent event
    /// (must stay refused) from one minted after it (must be honored).
    /// `#[serde(default)]` so a row written before this field existed
    /// deserializes at generation 0 — the same value a brand-new address
    /// starts at — with no migration step required.
    #[serde(default)]
    pub generation: u64,
}

impl Subscriber {
    /// Construct a new subscriber record. Useful for tests and
    /// integration callers that seed store state outside the handler
    /// path — `#[non_exhaustive]` blocks the struct-literal form from
    /// outside this crate.
    #[must_use]
    pub fn new(
        email: String,
        state: SubscriberState,
        created_at: OffsetDateTime,
        confirmed_at: Option<OffsetDateTime>,
        unsubscribed_at: Option<OffsetDateTime>,
        generation: u64,
    ) -> Self {
        Self {
            email,
            state,
            created_at,
            confirmed_at,
            unsubscribed_at,
            generation,
        }
    }
}

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

/// Persistence handle. One per process; cloning is cheap (the inner
/// [`Database`] manages its own Arc-shared state).
pub struct Store {
    /// Journal owner. Every keyspace of one `Database` shares it, so this
    /// is the single handle through which a write is made durable.
    database: Database,
    /// `subscribers` keyspace handle.
    subscribers: Keyspace,
    /// `sends` keyspace handle.
    sends: Keyspace,
    /// `deliveries` keyspace handle - one row per `<send_id>/<email>`,
    /// written by `POST /send`'s fan-out and updated in place by the
    /// bounce/complaint webhook.
    deliveries: Keyspace,
    /// `rate_limits` keyspace - one row per hour/day bucket, counting
    /// deliveries attempted in that window. See [`Store::try_reserve_send_slot`].
    rate_limits: Keyspace,
    /// Serializes the read-modify-write on a `rate_limits` row.
    ///
    /// WHY a mutex on top of a single-writer database: fjall's `get` and
    /// `insert` are each atomic individually, but the pair — read the
    /// current count, decide against the cap, write count+1 — is not.
    /// Two concurrent `/send` calls could both read the same
    /// under-the-cap count and both admit, overshooting it by one slot
    /// each. `/send` is a low-QPS operator endpoint, so contention here
    /// is not a throughput concern; correctness of the cap is.
    rate_limit_lock: tokio::sync::Mutex<()>,
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
    /// Open the keyspace at `path`, creating partitions if absent.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Store`] when fjall can't open the directory or
    /// create a partition.
    pub fn open(path: &Path) -> Result<Self> {
        guard_against_nested_keyspace(path)?;
        let database = Database::builder(path).open().map_err(|e| Error::Store {
            reason: format!("open database at {}: {e}", path.display()),
        })?;
        let subscribers = database
            .keyspace("subscribers", KeyspaceCreateOptions::default)
            .map_err(|e| Error::Store {
                reason: format!("subscribers keyspace: {e}"),
            })?;
        let sends = database
            .keyspace("sends", KeyspaceCreateOptions::default)
            .map_err(|e| Error::Store {
                reason: format!("sends keyspace: {e}"),
            })?;
        let deliveries = database
            .keyspace("deliveries", KeyspaceCreateOptions::default)
            .map_err(|e| Error::Store {
                reason: format!("deliveries keyspace: {e}"),
            })?;
        let rate_limits = database
            .keyspace("rate_limits", KeyspaceCreateOptions::default)
            .map_err(|e| Error::Store {
                reason: format!("rate_limits keyspace: {e}"),
            })?;
        Ok(Self {
            database,
            subscribers,
            sends,
            deliveries,
            rate_limits,
            rate_limit_lock: tokio::sync::Mutex::new(()),
        })
    }

    /// Flush the journal to disk with `fdatasync` so writes issued before
    /// this call survive a power loss.
    ///
    /// `SyncData` rather than `SyncAll`: the journal is an append-only file
    /// whose size is the only metadata a reader needs, and `fdatasync`
    /// already persists the metadata required to retrieve the written
    /// bytes. `SyncAll` would add an inode-attribute flush that buys no
    /// extra recovery guarantee here.
    fn persist_acknowledged(&self) -> Result<()> {
        self.database
            .persist(PersistMode::SyncData)
            .map_err(|e| Error::Store {
                reason: format!("persist journal: {e}"),
            })
    }

    /// Look up a subscriber by lowercased email.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Store`] on a fjall read failure.
    pub fn subscriber_get(&self, email: &str) -> Result<Option<Subscriber>> {
        let key = email.to_ascii_lowercase();
        let raw = self
            .subscribers
            .get(key.as_bytes())
            .map_err(|e| Error::Store {
                reason: format!("subscriber_get {email}: {e}"),
            })?;
        match raw {
            None => Ok(None),
            Some(bytes) => serde_json::from_slice(&bytes)
                .map(Some)
                .map_err(|e| Error::Store {
                    reason: format!("decode subscriber {email}: {e}"),
                }),
        }
    }

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

    /// Insert or replace a subscriber record, durably.
    ///
    /// Every caller is a consent transition the visitor is acknowledged
    /// for — `/confirm` flips to `Active`, `/unsubscribe` to
    /// `Unsubscribed` — so this returns only once the record is fsynced.
    /// A handler that has seen `Ok` may render its success page.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Store`] on a fjall write failure, a journal sync
    /// failure, or serde encode failure (the last is effectively
    /// unreachable for the well-typed [`Subscriber`] but is reported as a
    /// `Store` error for uniformity).
    pub fn subscriber_put(&self, subscriber: &Subscriber) -> Result<()> {
        let key = subscriber.email.to_ascii_lowercase();
        let bytes = serde_json::to_vec(subscriber).map_err(|e| Error::Store {
            reason: format!("encode subscriber {}: {e}", subscriber.email),
        })?;
        self.subscribers
            .insert(key.as_bytes(), bytes)
            .map_err(|e| Error::Store {
                reason: format!("subscriber_put {}: {e}", subscriber.email),
            })?;
        self.persist_acknowledged()
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

    /// Purge legacy `Pending` subscribers whose `created_at` is older than
    /// `max_age`.
    ///
    /// New `/subscribe` requests no longer create pending rows, but existing
    /// deployments may have stale pre-fix rows. This one-shot cleanup bounds
    /// that legacy state without touching `Active` or `Unsubscribed` rows.
    ///
    /// Reconstructible rather than acknowledged: no client waits on the
    /// purge, and a run lost to a power cut simply finds the same expired
    /// rows next time. It therefore syncs once at the end, and only when it
    /// deleted something, rather than paying an fsync per row.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Store`] on fjall read/write failures, a journal sync
    /// failure, or JSON decode failures.
    pub fn purge_expired_pending(
        &self,
        now: OffsetDateTime,
        max_age: time::Duration,
    ) -> Result<usize> {
        let mut expired = Vec::new();
        for guard in self.subscribers.iter() {
            let (key, value) = guard.into_inner().map_err(|e| Error::Store {
                reason: format!("subscribers iter: {e}"),
            })?;
            let subscriber: Subscriber =
                serde_json::from_slice(&value).map_err(|e| Error::Store {
                    reason: format!("subscriber decode during pending purge: {e}"),
                })?;
            if subscriber.state == SubscriberState::Pending
                && subscriber.created_at + max_age <= now
            {
                expired.push(key.to_vec());
            }
        }

        let deleted = expired.len();
        for key in expired {
            self.subscribers.remove(key).map_err(|e| Error::Store {
                reason: format!("pending purge remove: {e}"),
            })?;
        }
        if deleted > 0 {
            self.persist_acknowledged()?;
        }
        Ok(deleted)
    }

    /// Every `Active` subscriber. `/send`'s fan-out target.
    ///
    /// Collected eagerly rather than returning an iterator (contrast
    /// [`Store::iter_sends`]): unlike the archive index, which paginates
    /// public-facing history on purpose, a send's whole point is to
    /// reach every current subscriber, so there is no smaller bound to
    /// offer a caller here.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Store`] on a fjall read or decode failure.
    pub fn active_subscribers(&self) -> Result<Vec<Subscriber>> {
        let mut out = Vec::new();
        for guard in self.subscribers.iter() {
            let value = guard.value().map_err(|e| Error::Store {
                reason: format!("active_subscribers iter: {e}"),
            })?;
            let subscriber: Subscriber =
                serde_json::from_slice(&value).map_err(|e| Error::Store {
                    reason: format!("active_subscribers decode: {e}"),
                })?;
            if subscriber.state == SubscriberState::Active {
                out.push(subscriber);
            }
        }
        Ok(out)
    }

    /// Key layout for one delivery row: `<send_id>/<email>`. Keeps every
    /// recipient of one send lexically adjacent, and makes "does this
    /// send_id already have a row for this recipient" ([`Store::delivery_get`])
    /// a single point lookup.
    fn delivery_key(send_id: &SendId, email: &str) -> Vec<u8> {
        format!("{send_id}/{}", email.to_ascii_lowercase()).into_bytes()
    }

    /// Look up one delivery row by `(send_id, email)`.
    ///
    /// `/send`'s per-recipient idempotency check: a `Some` result means
    /// this send_id already attempted this recipient, so a retry must
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

    /// Read a rate-limit bucket's current count. `0` for a bucket never
    /// written (a fresh hour/day, or a fresh keyspace).
    fn rate_get(&self, bucket: &str) -> Result<u64> {
        let raw = self
            .rate_limits
            .get(bucket.as_bytes())
            .map_err(|e| Error::Store {
                reason: format!("rate_get {bucket}: {e}"),
            })?;
        match raw {
            None => Ok(0),
            Some(bytes) => {
                let text = std::str::from_utf8(&bytes).map_err(|e| Error::Store {
                    reason: format!("rate bucket {bucket} is not utf8: {e}"),
                })?;
                text.parse().map_err(|e| Error::Store {
                    reason: format!("rate bucket {bucket} is not a u64: {e}"),
                })
            }
        }
    }

    /// Attempt to reserve one send slot against both the hourly and daily
    /// caps. `hour_bucket` and `day_bucket` identify the current rolling
    /// windows (e.g. `"2026081518"` / `"20260815"`, UTC); the caller
    /// computes them so this method stays free of a clock dependency.
    ///
    /// Returns `Ok(true)` and durably increments both counters when both
    /// are still under their cap. Returns `Ok(false)` and leaves state
    /// untouched when either cap is already at its limit — the caller's
    /// contract is to stop attempting further recipients for this call,
    /// not to retry immediately.
    ///
    /// Reconstructible durability class, not Acknowledged: this is an
    /// anti-abuse budget, not a consent transition or a delivery record.
    /// A count a power-loss rolls back by one merely permits one extra
    /// send next time — the same risk the buffered default already
    /// accepts everywhere durability isn't explicitly upgraded (see the
    /// module docs' durability-class table).
    ///
    /// # Errors
    ///
    /// Returns [`Error::Store`] on a fjall read/write failure.
    pub async fn try_reserve_send_slot(
        &self,
        hour_bucket: &str,
        day_bucket: &str,
        hourly_cap: u64,
        daily_cap: u64,
    ) -> Result<bool> {
        let _guard = self.rate_limit_lock.lock().await;

        let hour_count = self.rate_get(hour_bucket)?;
        let day_count = self.rate_get(day_bucket)?;
        if hour_count >= hourly_cap || day_count >= daily_cap {
            return Ok(false);
        }

        self.rate_limits
            .insert(
                hour_bucket.as_bytes(),
                (hour_count + 1).to_string().into_bytes(),
            )
            .map_err(|e| Error::Store {
                reason: format!("rate increment {hour_bucket}: {e}"),
            })?;
        self.rate_limits
            .insert(
                day_bucket.as_bytes(),
                (day_count + 1).to_string().into_bytes(),
            )
            .map_err(|e| Error::Store {
                reason: format!("rate increment {day_bucket}: {e}"),
            })?;
        Ok(true)
    }
}

/// Refuse to open a keyspace whose path resolves INSIDE another
/// keyspace's `partitions/` subdirectory. This is a fleet footgun (per
/// `feedback_fjall_nested_keyspace_pitfall.md`): nested keyspaces trap
/// lsm-tree's V1-format check on later opens, leaving the data
/// permanently un-readable.
///
/// Reaudit finding #30: the previous implementation walked the lexical
/// path components only, so a symlink like `/tmp/data ->
/// /var/lib/epistole/data/partitions/sends` bypassed the check —
/// `/tmp/data` has no `partitions` ancestor lexically, but the
/// canonical destination does. We now:
///   1. Canonicalize the path's parent (the path itself may not exist
///      yet — fjall creates it). Symlinks resolve to their targets.
///   2. Walk the canonical path's components.
///   3. Reject if any ancestor name is `partitions`.
fn guard_against_nested_keyspace(path: &Path) -> Result<()> {
    // The path may not exist yet (fjall creates it). Canonicalize the
    // nearest existing ancestor and join the remaining path components.
    let canonical = canonicalize_with_nonexistent(path).map_err(|e| Error::Store {
        reason: format!("canonicalize {}: {e}", path.display()),
    })?;

    let mut cur = canonical.parent();
    while let Some(p) = cur {
        if p.file_name().and_then(|n| n.to_str()) == Some("partitions") {
            return Err(Error::Store {
                reason: format!(
                    "refusing to open: {} canonicalizes to {} which is inside a parent \
                     keyspace's partitions/ directory (nested keyspaces trap lsm-tree's \
                     V1-format check; pick a path outside any existing fjall keyspace — \
                     see feedback_fjall_nested_keyspace_pitfall.md)",
                    path.display(),
                    canonical.display()
                ),
            });
        }
        cur = p.parent();
    }
    Ok(())
}

/// Canonicalize `path`, walking up to the nearest existing ancestor and
/// re-appending the missing tail. Symlinks resolve to their targets —
/// including BROKEN symlinks (whose target does not exist), which the
/// previous Phase 1.5.2 implementation silently passed through (audit
/// finding #33).
///
/// Algorithm:
///   1. Make the path absolute.
///   2. Walk parents using `symlink_metadata().is_ok()` rather than
///      `Path::exists()`. The former returns true for a broken
///      symlink (the link itself exists; only its target is missing);
///      the latter returns false because it follows. Without this,
///      a broken symlink slipped past as "doesn't exist" and the
///      stripped name was re-appended literally to the canonical
///      parent, hiding the symlink target from the guard.
///   3. Once we hit an existing ancestor, canonicalize it.
///   4. For each tail component, if it's a symlink, read its target
///      (recursively if needed) and canonicalize the result. If it's
///      a regular not-yet-existing file, append literally.
fn canonicalize_with_nonexistent(path: &Path) -> std::io::Result<std::path::PathBuf> {
    canonicalize_bounded(path, 0)
}

/// Maximum broken-symlink hops resolved before giving up.
///
/// WHY: `std::fs::canonicalize` stops a symlink loop itself with `ELOOP`,
/// but the broken-symlink fallback below re-enters this function with the
/// link's target and so has no such backstop. `a -> b`, `b -> a` with a
/// missing target recurses until the stack is exhausted, which aborts the
/// process inside `Store::open` before any error can be reported. Linux
/// caps a resolution chain at 40 links; matching that bound rejects the
/// same paths the kernel would while keeping every legitimate chain.
const MAX_SYMLINK_HOPS: usize = 40;

fn canonicalize_bounded(path: &Path, hops: usize) -> std::io::Result<std::path::PathBuf> {
    if hops > MAX_SYMLINK_HOPS {
        return Err(std::io::Error::new(
            std::io::ErrorKind::TooManyLinks,
            format!(
                "symlink chain at {} exceeded {MAX_SYMLINK_HOPS} hops — the path is \
                 circular or too deeply linked",
                path.display()
            ),
        ));
    }

    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()?.join(path)
    };

    // Walk up to the first ancestor whose entry exists in the
    // filesystem (regular file/dir OR symlink, broken or not).
    // symlink_metadata does NOT follow symlinks, so a broken symlink
    // counts as existing — we want to inspect it.
    let mut existing: &Path = &absolute;
    let mut tail: Vec<&std::ffi::OsStr> = Vec::new();
    loop {
        if existing.symlink_metadata().is_ok() {
            break;
        }
        match existing.parent() {
            Some(parent) if parent != existing => {
                if let Some(name) = existing.file_name() {
                    tail.push(name);
                }
                existing = parent;
            }
            _ => break,
        }
    }

    // Resolve `existing` itself. If it's a symlink, follow target
    // chains via canonicalize (which fails on broken symlinks — the
    // safer behavior here, since a broken symlink target may not
    // exist YET but will be created by fjall).
    let mut canonical = match std::fs::canonicalize(existing) {
        Ok(p) => p,
        Err(_e) => {
            // Broken symlink at the leaf of the existing chain.
            // Read its target manually + canonicalize the target's
            // PARENT (which must exist for fjall to write there).
            let target = std::fs::read_link(existing)?;
            let resolved_target = if target.is_absolute() {
                target
            } else {
                existing.parent().map(|p| p.join(&target)).unwrap_or(target)
            };
            // Recurse: the target itself may be a chain of symlinks
            // or may not exist yet. The `canonicalize_bounded` call
            // resolves what it can and returns the rest literally.
            canonicalize_bounded(&resolved_target, hops + 1)?
        }
    };
    for component in tail.iter().rev() {
        canonical.push(component);
    }
    Ok(canonical)
}

#[cfg(test)]
mod pending_purge_tests {
    use super::*;

    #[test]
    #[expect(
        clippy::expect_used,
        reason = "test scaffolding - panic on fail is the desired signal"
    )]
    fn expired_legacy_pending_rows_are_purged_without_touching_subscribers() {
        let tmp = tempfile::TempDir::new().expect("tempdir");
        let store = Store::open(tmp.path()).expect("store");
        let now = OffsetDateTime::now_utc();
        let old = now - time::Duration::hours(25);
        let fresh = now - time::Duration::hours(1);

        for subscriber in [
            Subscriber {
                email: "old-pending@example.com".to_owned(),
                state: SubscriberState::Pending,
                created_at: old,
                confirmed_at: None,
                unsubscribed_at: None,
                generation: 0,
            },
            Subscriber {
                email: "fresh-pending@example.com".to_owned(),
                state: SubscriberState::Pending,
                created_at: fresh,
                confirmed_at: None,
                unsubscribed_at: None,
                generation: 0,
            },
            Subscriber {
                email: "active@example.com".to_owned(),
                state: SubscriberState::Active,
                created_at: old,
                confirmed_at: Some(old),
                unsubscribed_at: None,
                generation: 0,
            },
        ] {
            store.subscriber_put(&subscriber).expect("put");
        }

        let purged = store
            .purge_expired_pending(now, time::Duration::hours(24))
            .expect("purge");
        assert_eq!(purged, 1);
        assert!(
            store
                .subscriber_get("old-pending@example.com")
                .expect("read")
                .is_none()
        );
        assert!(
            store
                .subscriber_get("fresh-pending@example.com")
                .expect("read")
                .is_some()
        );
        assert!(
            store
                .subscriber_get("active@example.com")
                .expect("read")
                .is_some()
        );
    }
}

#[cfg(test)]
mod reopen_tests {
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
    fn an_acknowledged_unsubscribe_cannot_resurrect_an_active_subscriber() {
        // The acceptance sentence of forkwright/epistole#69, as a test.
        let now = OffsetDateTime::now_utc();
        let (store, _dir) = write_then_reopen(|store| {
            store
                .subscriber_put(&Subscriber {
                    email: "reader@example.com".to_owned(),
                    state: SubscriberState::Active,
                    created_at: now,
                    confirmed_at: Some(now),
                    unsubscribed_at: None,
                    generation: 0,
                })
                .expect("confirm");
            store
                .subscriber_put(&Subscriber {
                    email: "reader@example.com".to_owned(),
                    state: SubscriberState::Unsubscribed,
                    created_at: now,
                    confirmed_at: Some(now),
                    unsubscribed_at: Some(now),
                    generation: 1,
                })
                .expect("unsubscribe");
        });

        let subscriber = store
            .subscriber_get("reader@example.com")
            .expect("read")
            .expect("subscriber survived reopen");
        assert_eq!(
            subscriber.state,
            SubscriberState::Unsubscribed,
            "a reopen must not resurrect an unsubscribed reader as Active"
        );
        assert!(subscriber.unsubscribed_at.is_some());
    }

    #[test]
    #[expect(
        clippy::expect_used,
        reason = "test scaffolding - panic on fail is the desired signal"
    )]
    fn an_acknowledged_confirm_survives_reopen() {
        let now = OffsetDateTime::now_utc();
        let (store, _dir) = write_then_reopen(|store| {
            store
                .subscriber_put(&Subscriber {
                    email: "confirmed@example.com".to_owned(),
                    state: SubscriberState::Active,
                    created_at: now,
                    confirmed_at: Some(now),
                    unsubscribed_at: None,
                    generation: 0,
                })
                .expect("confirm");
        });

        let subscriber = store
            .subscriber_get("confirmed@example.com")
            .expect("read")
            .expect("subscriber survived reopen");
        assert_eq!(subscriber.state, SubscriberState::Active);
        assert!(subscriber.confirmed_at.is_some());
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
}

#[cfg(test)]
mod store_guard_tests {
    use super::*;

    #[test]
    #[expect(
        clippy::expect_used,
        reason = "test scaffolding - panic on fail is the desired signal"
    )]
    fn rejects_symlink_into_nested_partition_dir() {
        // Reaudit #30 regression: a symlink whose target is inside a
        // parent keyspace's partitions/ directory must be rejected.
        let tmp = tempfile::TempDir::new().expect("tempdir");
        let real_keyspace = tmp.path().join("real-keyspace");
        let real_partition = real_keyspace.join("partitions").join("sends");
        std::fs::create_dir_all(&real_partition).expect("create real");

        let symlink = tmp.path().join("sneaky-link");
        #[cfg(unix)]
        std::os::unix::fs::symlink(&real_partition, &symlink).expect("symlink");

        let result = guard_against_nested_keyspace(&symlink);
        assert!(
            result.is_err(),
            "guard must reject symlink that resolves into a parent partitions/ dir"
        );
        let err = match result {
            Ok(()) => unreachable!("asserted is_err above"),
            Err(e) => e,
        };
        let msg = format!("{err:?}");
        assert!(
            msg.contains("partitions"),
            "error should reference the partitions ancestor, got: {msg}"
        );
    }

    #[test]
    #[expect(
        clippy::expect_used,
        reason = "test scaffolding - panic on fail is the desired signal"
    )]
    fn allows_clean_path() {
        let tmp = tempfile::TempDir::new().expect("tempdir");
        let path = tmp.path().join("subdir-that-does-not-exist-yet");
        guard_against_nested_keyspace(&path).expect("clean path passes");
    }

    #[test]
    #[expect(
        clippy::expect_used,
        reason = "test scaffolding - panic on fail is the desired signal"
    )]
    fn rejects_broken_symlink_into_partitions() {
        // Reaudit #33: a BROKEN symlink (target does not exist) whose
        // target path walks through `partitions/` slipped past the
        // Phase 1.5.2 guard because Path::exists() returns false on
        // broken symlinks. The fix uses symlink_metadata + manual
        // read_link to inspect the target.
        let tmp = tempfile::TempDir::new().expect("tempdir");
        // Target does NOT exist yet — broken symlink.
        let broken_target = tmp
            .path()
            .join("future-keyspace")
            .join("partitions")
            .join("sends");
        let symlink = tmp.path().join("broken-link");
        #[cfg(unix)]
        std::os::unix::fs::symlink(&broken_target, &symlink).expect("symlink");

        let result = guard_against_nested_keyspace(&symlink);
        assert!(
            result.is_err(),
            "guard must reject a broken symlink whose target name walks through partitions/"
        );
        let err = match result {
            Ok(()) => unreachable!("asserted is_err above"),
            Err(e) => e,
        };
        let msg = format!("{err:?}");
        assert!(
            msg.contains("partitions"),
            "error should reference partitions, got: {msg}"
        );
    }

    #[test]
    #[cfg(unix)]
    #[expect(
        clippy::expect_used,
        reason = "test scaffolding - panic on fail is the desired signal"
    )]
    fn rejects_circular_symlink_instead_of_overflowing_the_stack() {
        // #43: the broken-symlink fallback in canonicalize_with_nonexistent
        // re-entered itself with the link target and carried no bound, so
        // `a -> b`, `b -> a` recursed until the stack was exhausted and the
        // process aborted inside Store::open. Reaching this assertion at all
        // means the recursion terminated.
        let tmp = tempfile::TempDir::new().expect("tempdir");
        let a = tmp.path().join("a");
        let b = tmp.path().join("b");
        std::os::unix::fs::symlink(&b, &a).expect("symlink a -> b");
        std::os::unix::fs::symlink(&a, &b).expect("symlink b -> a");

        let result = Store::open(&a);

        assert!(
            result.is_err(),
            "a circular symlink chain must return an error, not abort the process"
        );
    }

    #[test]
    #[cfg(unix)]
    #[expect(
        clippy::expect_used,
        reason = "test scaffolding - panic on fail is the desired signal"
    )]
    fn accepts_a_symlink_chain_shorter_than_the_hop_bound() {
        // The bound must reject cycles without rejecting the legitimate
        // chains a deploy may have, so prove one still resolves.
        let tmp = tempfile::TempDir::new().expect("tempdir");
        let real = tmp.path().join("real-data");
        std::fs::create_dir(&real).expect("create real dir");

        let mut previous = real.clone();
        for hop in 0..8 {
            let link = tmp.path().join(format!("hop-{hop}"));
            std::os::unix::fs::symlink(&previous, &link).expect("symlink hop");
            previous = link;
        }

        guard_against_nested_keyspace(&previous).expect("a bounded chain must pass the guard");
    }
}

#[cfg(test)]
mod delivery_and_rate_tests {
    use super::*;

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

    #[test]
    #[expect(
        clippy::expect_used,
        reason = "test scaffolding - panic on fail is the desired signal"
    )]
    fn active_subscribers_excludes_pending_and_unsubscribed() {
        let tmp = tempfile::TempDir::new().expect("tempdir");
        let store = Store::open(tmp.path()).expect("store");
        let now = OffsetDateTime::now_utc();

        for subscriber in [
            Subscriber {
                email: "pending@example.com".to_owned(),
                state: SubscriberState::Pending,
                created_at: now,
                confirmed_at: None,
                unsubscribed_at: None,
                generation: 0,
            },
            Subscriber {
                email: "active@example.com".to_owned(),
                state: SubscriberState::Active,
                created_at: now,
                confirmed_at: Some(now),
                unsubscribed_at: None,
                generation: 0,
            },
            Subscriber {
                email: "gone@example.com".to_owned(),
                state: SubscriberState::Unsubscribed,
                created_at: now,
                confirmed_at: Some(now),
                unsubscribed_at: Some(now),
                generation: 1,
            },
        ] {
            store.subscriber_put(&subscriber).expect("put");
        }

        let active = store.active_subscribers().expect("active_subscribers");
        assert_eq!(active.len(), 1, "pending and unsubscribed rows leaked in");
        assert_eq!(active[0].email, "active@example.com");
    }

    #[tokio::test]
    #[expect(
        clippy::expect_used,
        reason = "test scaffolding - panic on fail is the desired signal"
    )]
    async fn try_reserve_send_slot_admits_until_the_hourly_cap_then_refuses() {
        let tmp = tempfile::TempDir::new().expect("tempdir");
        let store = Store::open(tmp.path()).expect("store");

        // Hourly cap of 2, daily cap generous - only the hourly cap
        // should be the one that bites.
        assert!(
            store
                .try_reserve_send_slot("h1", "d1", 2, 100)
                .await
                .expect("reserve 1")
        );
        assert!(
            store
                .try_reserve_send_slot("h1", "d1", 2, 100)
                .await
                .expect("reserve 2")
        );
        assert!(
            !store
                .try_reserve_send_slot("h1", "d1", 2, 100)
                .await
                .expect("reserve 3"),
            "a third reservation against a cap of 2 must be refused"
        );
    }

    #[tokio::test]
    #[expect(
        clippy::expect_used,
        reason = "test scaffolding - panic on fail is the desired signal"
    )]
    async fn try_reserve_send_slot_admits_until_the_daily_cap_then_refuses() {
        let tmp = tempfile::TempDir::new().expect("tempdir");
        let store = Store::open(tmp.path()).expect("store");

        // Daily cap of 1 bites even though every call uses a fresh hour
        // bucket (simulating an hour boundary crossed mid-day) - proves
        // the day bucket is enforced independently of the hour bucket.
        assert!(
            store
                .try_reserve_send_slot("h1", "d1", 100, 1)
                .await
                .expect("reserve 1")
        );
        assert!(
            !store
                .try_reserve_send_slot("h2", "d1", 100, 1)
                .await
                .expect("reserve 2"),
            "a second reservation against a daily cap of 1 must be refused \
             even from a different hour bucket"
        );
    }

    #[tokio::test]
    #[expect(
        clippy::expect_used,
        reason = "test scaffolding - panic on fail is the desired signal"
    )]
    async fn a_refused_reservation_does_not_consume_budget() {
        let tmp = tempfile::TempDir::new().expect("tempdir");
        let store = Store::open(tmp.path()).expect("store");

        assert!(
            store
                .try_reserve_send_slot("h1", "d1", 1, 100)
                .await
                .expect("reserve 1")
        );
        assert!(
            !store
                .try_reserve_send_slot("h1", "d1", 1, 100)
                .await
                .expect("refused")
        );
        // A distinct hour bucket (a fresh window) still admits exactly
        // one - if the refused call above had still incremented "h1",
        // this assertion would be unaffected either way, so the load-
        // bearing check is that "h1" itself never grows past the cap:
        // repeating the refused call must keep refusing, not flip to
        // admitting because a phantom increment already happened once.
        assert!(
            !store
                .try_reserve_send_slot("h1", "d1", 1, 100)
                .await
                .expect("still refused")
        );
    }
}
