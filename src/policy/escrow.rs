//! Whether a written stage set can still be recovered by the CURRENT escrow
//! recipient (ADR-0005).
//!
//! One predicate, one place. `audit`'s `escrow_coverage` check (#125) decides
//! this from `stage_sets.key_fingerprints`, and `catalog locate` and
//! `report copies` now show the same fact — three sites that must never
//! disagree about whether a tape is escrow-recoverable. The codebase has been
//! bitten by the alternative: five inlined copies of a status filter are how
//! #96 happened, and six hand-written location counts are what #73 had to
//! collapse into `policy::coverage`.
//!
//! Reads only the recorded recipient list. No tape, no key material.

/// Why the current escrow key cannot recover this stage set, or `None` if it
/// can.
///
/// **Fails closed.** An absent or unparseable recipient list is reported
/// rather than presumed covered: a stage set that recorded no list cannot be
/// *shown* to be escrow-recoverable, and quietly treating "unknown" as "fine"
/// is how an archive reports itself clean while being unrecoverable.
pub fn gap(fingerprints: Option<&str>, escrow: &str) -> Option<String> {
    match fingerprints {
        None => Some("no recorded recipient list".to_string()),
        Some(json) => match serde_json::from_str::<Vec<String>>(json) {
            Ok(keys) if keys.iter().any(|k| k == escrow) => None,
            Ok(_) => Some("encrypted without the current escrow recipient".to_string()),
            Err(_) => Some("recipient list is unreadable".to_string()),
        },
    }
}

/// The one-word column value for `catalog locate` / `report copies`.
///
/// `-` when no escrow recipient is registered at all: there is nothing to
/// compare against, so claiming either "yes" or "NO" would be a false report.
/// That case is caught at stage time instead (#115).
pub fn marker(fingerprints: Option<&str>, escrow: Option<&str>) -> &'static str {
    match escrow {
        None => "-",
        Some(e) => {
            if gap(fingerprints, e).is_none() {
                "yes"
            } else {
                "NO"
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_list_containing_the_escrow_key_is_covered() {
        let json = r#"["age1alice","age1escrow"]"#;
        assert_eq!(gap(Some(json), "age1escrow"), None);
        assert_eq!(marker(Some(json), Some("age1escrow")), "yes");
    }

    #[test]
    fn a_list_without_it_is_reported_with_a_reason() {
        let json = r#"["age1alice","age1operator"]"#;
        assert_eq!(
            gap(Some(json), "age1escrow").as_deref(),
            Some("encrypted without the current escrow recipient")
        );
        assert_eq!(marker(Some(json), Some("age1escrow")), "NO");
    }

    /// The fail-closed arms. Neither can be shown recoverable, so neither is
    /// allowed to read as covered.
    #[test]
    fn an_absent_or_unreadable_list_fails_closed() {
        assert!(gap(None, "age1escrow").is_some());
        assert!(gap(Some("{not json"), "age1escrow").is_some());
        assert_eq!(marker(None, Some("age1escrow")), "NO");
        assert_eq!(marker(Some("{not json"), Some("age1escrow")), "NO");
    }

    /// With no escrow registered there is no question to answer, and
    /// answering anyway would be a false report in whichever direction.
    #[test]
    fn no_registered_escrow_renders_as_neither_yes_nor_no() {
        assert_eq!(marker(Some(r#"["age1alice"]"#), None), "-");
        assert_eq!(marker(None, None), "-");
    }
}
