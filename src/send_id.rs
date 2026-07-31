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

use serde::{Deserialize, Serialize};
use ulid::Ulid;

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
    #[must_use]
    pub fn generate() -> Self {
        Self(Ulid::generate())
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
