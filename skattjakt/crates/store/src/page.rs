//! Cursor pagination.
//!
//! Every list endpoint is bounded. An unbounded list is a query that is fast
//! for the first customer and an outage for the one with four years of monthly
//! accounts — and it is the shape of response a phone on a mobile network is
//! least able to cope with.
//!
//! Keyset, not `OFFSET`. `OFFSET 10000` makes the database walk ten thousand
//! rows to discard them, so the last page of a long list is the slowest, which
//! is exactly backwards. Keyset also does not skip or duplicate rows when
//! something is inserted while a client is paging — with `OFFSET`, a document
//! uploaded between page one and page two silently shifts every later row.

use base64::Engine;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// How many rows a page holds when the client does not say.
pub const DEFAULT_PAGE_SIZE: i64 = 50;
/// The most a client may ask for.
pub const MAX_PAGE_SIZE: i64 = 200;

/// Where the next page starts.
///
/// A timestamp *and* an id: timestamps collide — two documents uploaded in the
/// same millisecond are ordinary — and a cursor that cannot break the tie
/// either repeats a row or skips one.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Cursor {
    pub at: DateTime<Utc>,
    pub id: Uuid,
}

impl Cursor {
    /// Encodes the cursor for a URL.
    ///
    /// Opaque to clients on purpose. A cursor that visibly contains a timestamp
    /// is a cursor clients will construct by hand, and then the ordering key
    /// can never change without breaking them.
    pub fn encode(&self) -> String {
        let raw = format!("{}|{}", self.at.to_rfc3339(), self.id);
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(raw)
    }

    pub fn decode(encoded: &str) -> Option<Self> {
        let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(encoded)
            .ok()?;
        let text = String::from_utf8(bytes).ok()?;
        let (at, id) = text.split_once('|')?;
        Some(Self {
            at: DateTime::parse_from_rfc3339(at).ok()?.with_timezone(&Utc),
            id: Uuid::parse_str(id).ok()?,
        })
    }
}

/// One page of results.
#[derive(Debug, Clone)]
pub struct Page<T> {
    pub items: Vec<T>,
    /// `None` when this is the last page.
    ///
    /// Determined by fetching one row more than asked for and dropping it —
    /// which answers "is there more" exactly, without the `COUNT(*)` over the
    /// whole table that the obvious implementation reaches for.
    pub next: Option<Cursor>,
}

impl<T> Page<T> {
    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }
}

/// Clamps a client-supplied page size.
///
/// Silently, rather than rejecting. A client asking for 10 000 rows has made a
/// mistake that a 200-row page answers correctly; a 422 turns it into an outage
/// for that screen.
pub fn clamp_limit(requested: Option<i64>) -> i64 {
    requested
        .unwrap_or(DEFAULT_PAGE_SIZE)
        .clamp(1, MAX_PAGE_SIZE)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_cursor_round_trips() {
        let cursor = Cursor {
            at: Utc::now(),
            id: Uuid::new_v4(),
        };
        let decoded = Cursor::decode(&cursor.encode()).unwrap();
        assert_eq!(decoded.id, cursor.id);
        // RFC 3339 keeps microseconds; the comparison is on the encoded form.
        assert_eq!(decoded.encode(), cursor.encode());
    }

    #[test]
    fn a_cursor_is_opaque() {
        let cursor = Cursor {
            at: Utc::now(),
            id: Uuid::new_v4(),
        };
        let encoded = cursor.encode();
        // No visible timestamp, so nobody builds one by hand and pins the
        // ordering key for ever.
        assert!(!encoded.contains('-'));
        assert!(!encoded.contains(':'));
    }

    #[test]
    fn a_malformed_cursor_is_rejected_rather_than_guessed() {
        assert_eq!(Cursor::decode("not base64 at all!!"), None);
        assert_eq!(Cursor::decode(""), None);
        // Valid base64, wrong contents.
        let junk = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode("hello");
        assert_eq!(Cursor::decode(&junk), None);
    }

    #[test]
    fn the_page_size_is_clamped_rather_than_refused() {
        assert_eq!(clamp_limit(None), DEFAULT_PAGE_SIZE);
        assert_eq!(clamp_limit(Some(10_000)), MAX_PAGE_SIZE);
        assert_eq!(clamp_limit(Some(0)), 1);
        assert_eq!(clamp_limit(Some(-5)), 1);
        assert_eq!(clamp_limit(Some(25)), 25);
    }

    #[test]
    fn the_ceiling_is_low_enough_for_a_mobile_network() {
        // A page a phone has to parse and render on a train.
        const { assert!(MAX_PAGE_SIZE <= 200) };
        // And the default is well under it, so the common case is small.
        const { assert!(DEFAULT_PAGE_SIZE < MAX_PAGE_SIZE) };
    }
}
