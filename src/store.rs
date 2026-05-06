//! Storage layer. fjall keyspace with three partitions:
//!
//! - `subscribers` - keyed by lowercased email; value is a JSON-encoded
//!   [`Subscriber`] record.
//! - `sends` - keyed by send id (ULID-ish lexicographic timestamp);
//!   value is a JSON-encoded [`Send`] record (subject + rendered HTML +
//!   sent timestamp).
//! - `deliveries` - keyed by `<send_id>/<email>`; value is a
//!   JSON-encoded [`Delivery`] record (status + timestamp + error).
//!
//! Single-writer per keyspace per fjall's contract - out-of-process
//! tools must talk to the running server via HTTP, not open the keyspace
//! directly.

use std::path::Path;

use fjall::{Config as FjallConfig, Keyspace, PartitionCreateOptions, PartitionHandle};
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

use crate::error::{Error, Result};

/// Subscriber lifecycle state. Tokens reference one of these implicitly
/// via their `kind` field - a `confirm` token is only valid against a
/// `Pending` subscriber, an `unsubscribe` token only against `Active`.
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
/// [`Keyspace`] manages its own Arc-shared state).
pub struct Store {
    _keyspace: Keyspace,
    /// `subscribers` partition handle.
    pub(crate) subscribers: PartitionHandle,
    /// `sends` partition handle.
    pub(crate) sends: PartitionHandle,
    /// `deliveries` partition handle. Held open at startup so the LSM
    /// flush schedule covers it; the first read/write lands in Phase 2
    /// (forkwright/epistole#1) when `/send` walks subscribers and
    /// records per-recipient delivery outcomes.
    #[expect(
        dead_code,
        reason = "Phase 2 (forkwright/epistole#1) wires per-recipient delivery records"
    )]
    pub(crate) deliveries: PartitionHandle,
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
        let keyspace = FjallConfig::new(path).open().map_err(|e| Error::Store {
            reason: format!("open keyspace at {}: {e}", path.display()),
        })?;
        let opts = PartitionCreateOptions::default();
        let subscribers = keyspace
            .open_partition("subscribers", opts.clone())
            .map_err(|e| Error::Store {
                reason: format!("subscribers partition: {e}"),
            })?;
        let sends = keyspace
            .open_partition("sends", opts.clone())
            .map_err(|e| Error::Store {
                reason: format!("sends partition: {e}"),
            })?;
        let deliveries = keyspace
            .open_partition("deliveries", opts)
            .map_err(|e| Error::Store {
                reason: format!("deliveries partition: {e}"),
            })?;
        Ok(Self {
            _keyspace: keyspace,
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
        Ok(self.sends.iter().map(|kv| {
            let (_k, v) = kv.map_err(|e| Error::Store {
                reason: format!("sends iter: {e}"),
            })?;
            serde_json::from_slice::<Send>(&v).map_err(|e| Error::Store {
                reason: format!("sends decode: {e}"),
            })
        }))
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
/// re-appending the missing tail. Symlinks resolve to their targets.
fn canonicalize_with_nonexistent(path: &Path) -> std::io::Result<std::path::PathBuf> {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()?.join(path)
    };

    // Walk up to the first existing ancestor.
    let mut existing: &Path = &absolute;
    let mut tail: Vec<&std::ffi::OsStr> = Vec::new();
    loop {
        if existing.exists() {
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

    let mut canonical = std::fs::canonicalize(existing)?;
    for component in tail.iter().rev() {
        canonical.push(component);
    }
    Ok(canonical)
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
}
