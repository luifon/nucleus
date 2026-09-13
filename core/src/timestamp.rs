//! One canonical text form for timestamps that SQLite has to *order*.
//!
//! SQLite has no date type: a `TEXT` timestamp is compared byte by byte, so
//! two RFC3339 strings only sort chronologically when they share a zone and a
//! fractional-second width. They don't, in practice — a widget writes local
//! time with an offset (`2026-09-13T11:26:10-03:00`), a Rust service writes
//! `Utc::now().to_rfc3339()` with nanoseconds (`…:10.123456789+00:00`), and
//! `'.' < 'Z'` makes a sub-second stamp sort *before* the whole second it
//! follows.
//!
//! Every producer of a sort-bearing timestamp writes [`now`], and every value
//! arriving from outside goes through [`to_sortable`] on the way in. The
//! result is UTC, millisecond precision, `Z` suffix, fixed width — so
//! lexicographic order is chronological order.

use chrono::{DateTime, SecondsFormat, Utc};

/// Now, in the canonical sortable form.
pub fn now() -> String {
    Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true)
}

/// Parse any RFC3339 stamp — any offset, any fractional precision — and
/// re-emit it in the canonical form.
///
/// Input that doesn't parse is returned trimmed and otherwise untouched: it
/// still compares equal to itself, which is the property idempotent ingest
/// depends on. Sub-millisecond precision is truncated, which is deliberate —
/// anything that close together is a tie, and ties are broken by insertion
/// order, not by the clock.
pub fn to_sortable(raw: &str) -> String {
    let trimmed = raw.trim();
    match DateTime::parse_from_rfc3339(trimmed) {
        Ok(dt) => dt.with_timezone(&Utc).to_rfc3339_opts(SecondsFormat::Millis, true),
        Err(_) => trimmed.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn offsets_and_precisions_collapse_to_one_shape() {
        assert_eq!(to_sortable("2026-09-13T11:26:10-03:00"), "2026-09-13T14:26:10.000Z");
        assert_eq!(to_sortable("2026-09-13T14:26:10Z"), "2026-09-13T14:26:10.000Z");
        assert_eq!(
            to_sortable("2026-09-13T14:26:10.123456789+00:00"),
            "2026-09-13T14:26:10.123Z"
        );
    }

    #[test]
    fn normalizing_twice_changes_nothing() {
        let once = to_sortable("2026-09-13T11:26:10.5-03:00");
        assert_eq!(to_sortable(&once), once);
    }

    #[test]
    fn text_order_matches_time_order_across_zones_and_precisions() {
        // The exact mix that breaks a raw byte comparison: a fraction at the
        // same second, and a local-offset stamp against a UTC one.
        let mut stamps = [
            to_sortable("2026-09-13T14:26:10.500Z"),
            to_sortable("2026-09-13T14:26:10Z"),
            to_sortable("2026-09-13T11:26:09-03:00"),
            to_sortable("2026-09-13T14:26:11.000000001Z"),
        ];
        stamps.sort();
        assert_eq!(
            stamps,
            [
                "2026-09-13T14:26:09.000Z",
                "2026-09-13T14:26:10.000Z",
                "2026-09-13T14:26:10.500Z",
                "2026-09-13T14:26:11.000Z",
            ]
        );
    }

    #[test]
    fn unparseable_input_survives_unchanged() {
        assert_eq!(to_sortable("  not a date  "), "not a date");
        assert_eq!(to_sortable(""), "");
    }

    #[test]
    fn now_is_in_the_canonical_shape() {
        let n = now();
        assert_eq!(to_sortable(&n), n);
        assert!(n.ends_with('Z'), "{n}");
    }
}
