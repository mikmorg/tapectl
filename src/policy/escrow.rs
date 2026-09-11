//! Whether a written stage set can still be recovered by the CURRENT escrow
//! recipient (ADR-0005).
//!
//! One predicate, one place. `audit`'s `escrow_coverage` check (#125) decides
//! this from `stage_sets.key_fingerprints`, and `catalog locate`,
//! `report copies` and the `volume write` pre-flight show the same fact —
//! sites that must never disagree about whether a tape is escrow-recoverable.
//! The codebase has been bitten by the alternative: five inlined copies of a
//! status filter are how #96 happened, and six hand-written location counts
//! are what #73 had to collapse into `policy::coverage`.
//!
//! Reads only the recorded recipient list and the row's origin. No tape, no
//! key material.
//!
//! # Three answers, not two (#137)
//!
//! A `NULL` recipient list means one of two different things, and the
//! difference matters to the operator reading the finding:
//!
//! * the row was **staged here** and the list was never written down — a
//!   pre-escrow or corrupt row. Fail closed: that is a **gap**.
//! * the row was **rebuilt from a tape** (`catalog rebuild`, #136) and the
//!   tape carried no recipient list to copy. tapectl could not have written
//!   it down. That is **unknown** — still not covered, still fail-closed for
//!   every gate, but reported in words that say what to do about it rather
//!   than accusing the row of something nobody could have prevented.
//!
//! `stage_sets.origin` (migration 010) is the discriminant.

/// Where a `stage_sets` row came from — the `origin` column.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Origin {
    /// `stage create` on this machine. Every row before migration 010.
    Staged,
    /// `catalog rebuild` from a sealed volume's envelope (#136).
    Rebuilt,
}

impl Origin {
    /// Parse the column. Anything other than the literal `rebuilt` is
    /// `Staged`: the column is `NOT NULL` with a `CHECK`, so this is only
    /// ever asked of the two values, and the conservative reading of a
    /// surprise is the one that fails closed.
    pub fn parse(column: &str) -> Origin {
        if column == "rebuilt" {
            Origin::Rebuilt
        } else {
            Origin::Staged
        }
    }
}

/// Can the current escrow recipient recover this stage set?
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Coverage {
    /// The recorded list names the current escrow key.
    Covered,
    /// Rebuilt from a tape that carries no recipient list. Not covered —
    /// every gate treats this exactly like [`Coverage::Gap`] — but the
    /// remedy is different and the wording says so.
    Unknown,
    /// Not covered, with the reason.
    Gap(String),
}

/// The words for [`Coverage::Unknown`], in one place so `audit`, the write
/// pre-flight and `report copies` say the same thing.
pub const UNKNOWN_REASON: &str = "coverage unknown — rebuilt from a tape that carries no \
recipient list; attest it with `catalog rebuild --key <escrow key>`, or re-stage";

/// Classify one stage set.
///
/// **Fails closed.** An absent or unparseable recipient list is never
/// presumed covered: a stage set that recorded no list cannot be *shown* to
/// be escrow-recoverable, and quietly treating "unknown" as "fine" is how an
/// archive reports itself clean while being unrecoverable. `Unknown` is a
/// different *explanation*, not a different *verdict*.
pub fn classify(fingerprints: Option<&str>, origin: Origin, escrow: &str) -> Coverage {
    match fingerprints {
        None => match origin {
            Origin::Rebuilt => Coverage::Unknown,
            Origin::Staged => Coverage::Gap("no recorded recipient list".to_string()),
        },
        Some(json) => match serde_json::from_str::<Vec<String>>(json) {
            Ok(keys) if keys.iter().any(|k| k == escrow) => Coverage::Covered,
            Ok(_) => Coverage::Gap("encrypted without the current escrow recipient".to_string()),
            Err(_) => Coverage::Gap("recipient list is unreadable".to_string()),
        },
    }
}

/// Why the current escrow key cannot recover this stage set, or `None` if it
/// can. `Unknown` renders as [`UNKNOWN_REASON`].
pub fn gap(fingerprints: Option<&str>, origin: Origin, escrow: &str) -> Option<String> {
    match classify(fingerprints, origin, escrow) {
        Coverage::Covered => None,
        Coverage::Unknown => Some(UNKNOWN_REASON.to_string()),
        Coverage::Gap(reason) => Some(reason),
    }
}

/// The one-word column value for `catalog locate` / `report copies`.
///
/// `-` when no escrow recipient is registered at all: there is nothing to
/// compare against, so claiming either "yes" or "NO" would be a false report.
/// That case is caught at stage time instead (#115). `?` is a rebuilt row the
/// tape could not vouch for — see the module header.
pub fn marker(fingerprints: Option<&str>, origin: Origin, escrow: Option<&str>) -> &'static str {
    match escrow {
        None => "-",
        Some(e) => match classify(fingerprints, origin, e) {
            Coverage::Covered => "yes",
            Coverage::Unknown => "?",
            Coverage::Gap(_) => "NO",
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_list_containing_the_escrow_key_is_covered() {
        let json = r#"["age1alice","age1escrow"]"#;
        assert_eq!(gap(Some(json), Origin::Staged, "age1escrow"), None);
        assert_eq!(
            marker(Some(json), Origin::Staged, Some("age1escrow")),
            "yes"
        );
    }

    #[test]
    fn a_list_without_it_is_reported_with_a_reason() {
        let json = r#"["age1alice","age1operator"]"#;
        assert_eq!(
            gap(Some(json), Origin::Staged, "age1escrow").as_deref(),
            Some("encrypted without the current escrow recipient")
        );
        assert_eq!(marker(Some(json), Origin::Staged, Some("age1escrow")), "NO");
    }

    /// The fail-closed arms. Neither can be shown recoverable, so neither is
    /// allowed to read as covered.
    #[test]
    fn an_absent_or_unreadable_list_fails_closed() {
        assert!(gap(None, Origin::Staged, "age1escrow").is_some());
        assert!(gap(Some("{not json"), Origin::Staged, "age1escrow").is_some());
        assert_eq!(marker(None, Origin::Staged, Some("age1escrow")), "NO");
        assert_eq!(
            marker(Some("{not json"), Origin::Staged, Some("age1escrow")),
            "NO"
        );
    }

    /// With no escrow registered there is no question to answer, and
    /// answering anyway would be a false report in whichever direction.
    #[test]
    fn no_registered_escrow_renders_as_neither_yes_nor_no() {
        assert_eq!(marker(Some(r#"["age1alice"]"#), Origin::Staged, None), "-");
        assert_eq!(marker(None, Origin::Staged, None), "-");
    }

    /// #137: a rebuilt row with no list is a different EXPLANATION, not a
    /// different VERDICT. Still not covered, but says so in words that name
    /// the remedy, and renders as `?` rather than `NO`.
    #[test]
    fn a_rebuilt_row_with_no_list_is_unknown_not_a_gap() {
        assert_eq!(
            classify(None, Origin::Rebuilt, "age1escrow"),
            Coverage::Unknown
        );
        assert_eq!(
            gap(None, Origin::Rebuilt, "age1escrow").as_deref(),
            Some(UNKNOWN_REASON)
        );
        assert_eq!(marker(None, Origin::Rebuilt, Some("age1escrow")), "?");
        // The same NULL, staged here, is the old fail-closed gap.
        assert!(matches!(
            classify(None, Origin::Staged, "age1escrow"),
            Coverage::Gap(_)
        ));
    }

    /// Once attested (the list is filled in), origin no longer matters: a
    /// rebuilt row that names the key is covered like any other.
    #[test]
    fn an_attested_rebuilt_row_is_covered() {
        let json = r#"["age1escrow"]"#;
        assert_eq!(
            classify(Some(json), Origin::Rebuilt, "age1escrow"),
            Coverage::Covered
        );
        assert_eq!(
            marker(Some(json), Origin::Rebuilt, Some("age1escrow")),
            "yes"
        );
        // ...and a rebuilt row naming the WRONG key is a gap, not unknown.
        assert!(matches!(
            classify(Some(r#"["age1old"]"#), Origin::Rebuilt, "age1escrow"),
            Coverage::Gap(_)
        ));
    }

    #[test]
    fn origin_parses_conservatively() {
        assert_eq!(Origin::parse("rebuilt"), Origin::Rebuilt);
        assert_eq!(Origin::parse("staged"), Origin::Staged);
        assert_eq!(Origin::parse("anything else"), Origin::Staged);
    }
}
