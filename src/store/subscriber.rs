//! Subscriber lifecycle: state, record, and CRUD against the
//! `subscribers` keyspace.

use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

use crate::error::{Error, Result};
use crate::store::Store;

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

impl Store {
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
}

#[cfg(test)]
mod tests {
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
}
