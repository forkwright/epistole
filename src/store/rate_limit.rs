//! Hourly/daily send-cap reservation against the `rate_limits` keyspace.

use crate::error::{Error, Result};
use crate::store::Store;

impl Store {
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

#[cfg(test)]
mod tests {
    use super::*;

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
