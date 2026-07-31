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

use std::path::Path;

use fjall::{Database, Keyspace, KeyspaceCreateOptions};
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

use crate::error::{Error, Result};

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
    /// Send id - lexicographic timestamp (nanoseconds since epoch).
    pub id: String,
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
    pub send_id: String,
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
    _database: Database,
    /// `subscribers` keyspace handle.
    pub(crate) subscribers: Keyspace,
    /// `sends` keyspace handle.
    pub(crate) sends: Keyspace,
    /// `deliveries` keyspace handle. Held open at startup so the LSM
    /// flush schedule covers it; the first read/write lands in Phase 2
    /// (forkwright/epistole#1) when `/send` walks subscribers and
    /// records per-recipient delivery outcomes.
    #[expect(
        dead_code,
        reason = "Phase 2 (forkwright/epistole#1) wires per-recipient delivery records"
    )]
    pub(crate) deliveries: Keyspace,
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
            _database: database,
            subscribers,
            sends,
            deliveries,
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
    pub fn send_get(&self, send_id: &str) -> Result<Option<Send>> {
        let raw = self
            .sends
            .get(send_id.as_bytes())
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

    /// Insert or replace a subscriber record.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Store`] on a fjall write failure or serde encode
    /// failure (the latter is effectively unreachable for the well-typed
    /// [`Subscriber`] but is reported as a `Store` error for uniformity).
    pub fn subscriber_put(&self, subscriber: &Subscriber) -> Result<()> {
        let key = subscriber.email.to_ascii_lowercase();
        let bytes = serde_json::to_vec(subscriber).map_err(|e| Error::Store {
            reason: format!("encode subscriber {}: {e}", subscriber.email),
        })?;
        self.subscribers
            .insert(key.as_bytes(), bytes)
            .map_err(|e| Error::Store {
                reason: format!("subscriber_put {}: {e}", subscriber.email),
            })
    }

    /// Purge legacy `Pending` subscribers whose `created_at` is older than
    /// `max_age`.
    ///
    /// New `/subscribe` requests no longer create pending rows, but existing
    /// deployments may have stale pre-fix rows. This one-shot cleanup bounds
    /// that legacy state without touching `Active` or `Unsubscribed` rows.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Store`] on fjall read/write failures or JSON decode
    /// failures.
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
