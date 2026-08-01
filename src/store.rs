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

/// Per-recipient delivery state.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
#[non_exhaustive]
pub enum DeliveryStatus {
    /// Send queued; relay has not yet acknowledged.
    Queued,
    /// Relay accepted the message.
    Sent,
    /// Relay rejected the message.
    Failed,
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
    /// `deliveries` keyspace handle. Held open at startup so the LSM
    /// flush schedule covers it; the first read/write lands in Phase 2
    /// (forkwright/epistole#1) when `/send` walks subscribers and
    /// records per-recipient delivery outcomes.
    #[expect(
        dead_code,
        reason = "Phase 2 (forkwright/epistole#1) wires per-recipient delivery records"
    )]
    deliveries: Keyspace,
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
        Ok(Self {
            database,
            subscribers,
            sends,
            deliveries,
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

    /// Iterate every record in the `sends` partition. Used by tests +
    /// (Phase 2) the archive page renderer.
    ///
    /// # Errors
    ///
    /// Each item resolves to [`Error::Store`] on fjall read or decode
    /// failure.
    pub fn iter_sends(&self) -> Result<impl Iterator<Item = Result<Send>>> {
        Ok(self.sends.iter().map(|guard| {
            let v = guard.value().map_err(|e| Error::Store {
                reason: format!("sends iter: {e}"),
            })?;
            serde_json::from_slice::<Send>(&v).map_err(|e| Error::Store {
                reason: format!("sends decode: {e}"),
            })
        }))
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
            // or may not exist yet. The `canonicalize_with_nonexistent`
            // call resolves what it can and returns the rest literally.
            canonicalize_with_nonexistent(&resolved_target)?
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
            },
            Subscriber {
                email: "fresh-pending@example.com".to_owned(),
                state: SubscriberState::Pending,
                created_at: fresh,
                confirmed_at: None,
                unsubscribed_at: None,
            },
            Subscriber {
                email: "active@example.com".to_owned(),
                state: SubscriberState::Active,
                created_at: old,
                confirmed_at: Some(old),
                unsubscribed_at: None,
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
                })
                .expect("confirm");
            store
                .subscriber_put(&Subscriber {
                    email: "reader@example.com".to_owned(),
                    state: SubscriberState::Unsubscribed,
                    created_at: now,
                    confirmed_at: Some(now),
                    unsubscribed_at: Some(now),
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
}
