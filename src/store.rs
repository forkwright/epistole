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

use crate::error::{Error, Result};

mod path;
mod rate_limit;
mod send;
mod subscriber;

pub use send::{Delivery, DeliveryStatus, Send};
pub use subscriber::{Subscriber, SubscriberState};

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

impl Store {
    /// Open the keyspace at `path`, creating partitions if absent.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Store`] when fjall can't open the directory or
    /// create a partition.
    pub fn open(path: &Path) -> Result<Self> {
        path::guard_against_nested_keyspace(path)?;
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
}
