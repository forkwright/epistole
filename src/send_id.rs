//! Newtype for send identifiers.
//!
//! [`SendId`] wraps the ULID minted for each `POST /send` call. Before
//! this type existed, the same identifier passed through the codebase
//! as three independently typed `String`s ([`crate::store::Send::id`],
//! [`crate::store::Delivery::send_id`], and the `/send` JSON reply
//! field) that happened to agree by convention rather than by the
//! compiler. One type now backs all three, plus the `/archive/{send_id}`
//! path parameter.

use std::fmt;
use std::str::FromStr;
use std::sync::{Mutex, PoisonError};

use serde::{Deserialize, Serialize};
use ulid::{Generator, Ulid};

/// Process-wide monotonic ULID source.
///
/// WHY: `Ulid::generate()` draws fresh random low bits on every call, so
/// two ids minted in the same millisecond sort in random order relative
/// to one another — the timestamp prefix ties and the random suffix
/// decides. Send ids are the `sends` keyspace's sort key and therefore
/// the archive's ordering, and because the index is capped a
/// same-millisecond tie decides which send lands on the page at all.
/// A shared [`Generator`] makes every id strictly greater than the last
/// one this process minted, which is what the ordering documented on
/// [`SendId`] has always claimed.
static GENERATOR: Mutex<Generator> = Mutex::new(Generator::new());

/// Identifies one newsletter send. Lexicographically sortable (matches
/// [`Ulid`]'s ordering), so `Ord` on `SendId` sorts sends chronologically
/// without a separate timestamp comparison. Serializes as the canonical
/// 26-character Crockford Base32 string, same as the raw `String` it
/// replaces, so on-disk and wire representations are unchanged.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct SendId(Ulid);

impl SendId {
    /// Mint a fresh id for a new send.
    ///
    /// Strictly greater than every id this process has already minted,
    /// including within a single millisecond.
    #[must_use]
    pub fn generate() -> Self {
        // WHY: a poisoned lock means some other caller panicked mid-mint.
        // The generator's only state is the previous id, which is still a
        // valid floor, so recovering the guard keeps sends working rather
        // than propagating an unrelated panic into every later request.
        let mut generator = GENERATOR.lock().unwrap_or_else(PoisonError::into_inner);
        // WARNING: `generate` errors only when the 80-bit random field
        // would overflow inside one millisecond. The documented recovery
        // steps into the next millisecond and stays monotonic.
        Self(
            generator
                .generate()
                .unwrap_or_else(ulid::Overflow::commit_overflow_increment),
        )
    }
}

impl fmt::Display for SendId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(&self.0, f)
    }
}

/// Parses the canonical 26-character Crockford Base32 form. Rejects
/// anything else - the constructor boundary `RUST/primitive-for-domain-id`
/// asks a domain newtype to have. Used by the `/archive/{send_id}` path
/// extractor to reject malformed ids before a store lookup is attempted.
impl FromStr for SendId {
    type Err = ulid::DecodeError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ulid::from_str(s).map(Self)
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
    fn display_round_trips_through_from_str() {
        let id = SendId::generate();
        let parsed: SendId = id.to_string().parse().expect("parse own output");
        assert_eq!(id, parsed);
    }

    #[test]
    fn from_str_rejects_malformed_ids() {
        assert!("not-a-ulid".parse::<SendId>().is_err());
        assert!("".parse::<SendId>().is_err());
    }

    #[test]
    fn generate_is_strictly_monotonic_within_one_millisecond() {
        // A tight mint loop puts many ids inside the same millisecond,
        // where the timestamp prefix ties and only the low bits order
        // them. `Ulid::generate()` re-randomizes those bits per call, so
        // it fails here; a shared monotonic generator does not.
        let ids: Vec<SendId> = (0..2000).map(|_| SendId::generate()).collect();

        // Anti-vacuity: if every id landed in its own millisecond the
        // ordering assertion below would hold for free and prove nothing.
        let ties = ids
            .windows(2)
            .filter(|p| p[0].0.timestamp_ms() == p[1].0.timestamp_ms())
            .count();
        assert!(
            ties > 0,
            "no two ids shared a millisecond, so this test proves nothing"
        );

        for pair in ids.windows(2) {
            assert!(
                pair[0] < pair[1],
                "send ids must strictly increase, got {} then {}",
                pair[0],
                pair[1]
            );
        }
    }

    #[test]
    #[expect(
        clippy::expect_used,
        reason = "test scaffolding - panic on fail is the desired signal"
    )]
    fn ord_matches_lexicographic_string_order() {
        // Ulid's u128 ordering and its canonical string ordering agree
        // (both are big-endian time-first) - this is what lets
        // `Send::id`/`SendId` sort chronologically without a separate
        // timestamp comparison.
        let earlier: SendId = "00000000000000000000000000".parse().expect("parse");
        let later: SendId = "01ARZ3NDEKTSV4RRFFQ69G5FAV".parse().expect("parse");
        assert!(earlier < later);
        assert_eq!(earlier.to_string(), "00000000000000000000000000");
    }
}
