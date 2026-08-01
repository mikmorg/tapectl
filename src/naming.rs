//! Validation for the operator-supplied identifiers that reach the
//! filesystem: tenant names, unit names, and volume labels (issue #103).
//!
//! # Why this exists
//!
//! Three identifiers the operator types are interpolated into paths:
//!
//! - **Tenant name** → key files. `crypto::keys::key_paths` builds
//!   `{keys_dir}/{tenant_name}-{alias}.age.key`, and
//!   `load_all_identities` finds a tenant's keys by prefix-matching
//!   `{tenant_name}-`. A name containing `/` or `..` writes a **private
//!   key** outside `keys/`; a name containing `-` in the wrong place can
//!   shadow another tenant's prefix scan.
//! - **Unit name** and **volume label** → the read-slices staging
//!   directory. `volume::write` joins
//!   `{staging}/clone-{from_label}-{unit_name}`.
//!
//! This is single-operator software running on a machine the operator
//! owns, so this is not a privilege boundary and is not written as one.
//! The realistic failure is a typo or a paste producing an unreachable or
//! shadowed key — which, in a tool whose whole job is key custody and
//! long-term retrieval, is its own kind of bad. A key written to the wrong
//! place is discovered years later by an heir.
//!
//! # Creation-time only, deliberately
//!
//! Validation runs where a name is **chosen** — `tenant add`, `unit init`,
//! `unit rename`, `volume init` — and never where one is **loaded**. That
//! is what makes this safe to add to an existing install: a name already in
//! the database keeps working forever, because nothing on the read path
//! consults these functions. Validating at load time would turn a
//! previously-working catalog into an unopenable one on upgrade, which is a
//! far worse outcome than the typo being prevented here.
//!
//! # Unit names are not single segments
//!
//! A unit name is legitimately hierarchical — `tv/breaking-bad/s01` is the
//! documented shape, and `collection sync` generates
//! `"{collection}/{path relative to root}"`. So `/` cannot be banned for
//! units; it is validated segment-by-segment instead. Tenant names and
//! volume labels are single segments and reject `/` outright.

use crate::error::{Result, TapectlError};

/// Longest single path segment. Comfortably under every filesystem's own
/// limit (255 on ext4/xfs/btrfs) while leaving room for the suffixes this
/// codebase appends — `-{alias}.age.key`, `clone-{label}-`.
const MAX_SEGMENT: usize = 64;

/// Longest whole unit name, across all segments.
const MAX_UNIT_NAME: usize = 200;

/// Validate one path segment: the shared rule behind all three public
/// functions.
///
/// Allowed: ASCII alphanumerics, `.`, `_`, `-`. Everything else is
/// rejected, including every form of path separator, whitespace, shell
/// metacharacter, and non-ASCII. An allowlist rather than a blocklist on
/// purpose — a blocklist of "dangerous" characters is a guess about which
/// ones matter, and this codebase interpolates these names into shell-free
/// but filesystem-visible contexts where the full set is hard to enumerate.
fn validate_segment(segment: &str, kind: &str, whole: &str) -> Result<()> {
    let reject = |why: &str| -> Result<()> {
        Err(TapectlError::Other(format!(
            "invalid {kind} \"{whole}\": {why}. Allowed: letters, digits, \
             dot, underscore and dash{}",
            if kind == "unit name" {
                ", with / separating path segments"
            } else {
                ""
            }
        )))
    };

    if segment.is_empty() {
        return reject("it has an empty component");
    }
    if segment.len() > MAX_SEGMENT {
        return reject(&format!(
            "the component \"{segment}\" is longer than {MAX_SEGMENT} characters"
        ));
    }
    // `.` and `..` are the path-traversal cases and must be rejected even
    // though every character in them is otherwise allowed.
    if segment == "." || segment == ".." {
        return reject("\".\" and \"..\" are not usable as name components");
    }
    if let Some(bad) = segment
        .chars()
        .find(|c| !(c.is_ascii_alphanumeric() || *c == '.' || *c == '_' || *c == '-'))
    {
        return reject(&format!("the character {bad:?} is not allowed"));
    }
    // A leading dash reads as a CLI flag when the name is later echoed into
    // a command line in a runbook; a leading dot makes the key file hidden,
    // which is exactly the wrong property for something an heir must find.
    if segment.starts_with('-') {
        return reject("a component may not start with a dash");
    }
    if segment.starts_with('.') {
        return reject("a component may not start with a dot");
    }
    Ok(())
}

/// Validate a tenant name. Single segment — a tenant name becomes part of a
/// key *filename*, so `/` is never valid in one.
pub fn validate_tenant_name(name: &str) -> Result<()> {
    validate_segment(name, "tenant name", name)
}

/// Validate a volume label. Single segment, same reasoning as a tenant
/// name: it is joined into the read-slices staging directory name.
pub fn validate_volume_label(label: &str) -> Result<()> {
    validate_segment(label, "volume label", label)
}

/// Validate a unit name, segment by segment.
///
/// Hierarchy is expected (`tv/breaking-bad/s01`), so `/` is a separator
/// rather than a forbidden character — but a leading, trailing or doubled
/// `/` produces an empty segment and is rejected, as is any `..` component.
pub fn validate_unit_name(name: &str) -> Result<()> {
    if name.len() > MAX_UNIT_NAME {
        return Err(TapectlError::Other(format!(
            "invalid unit name \"{name}\": longer than {MAX_UNIT_NAME} characters"
        )));
    }
    if name.is_empty() {
        return Err(TapectlError::Other(
            "invalid unit name: it is empty".to_string(),
        ));
    }
    for segment in name.split('/') {
        validate_segment(segment, "unit name", name)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn err(r: Result<()>) -> String {
        r.expect_err("expected a rejection").to_string()
    }

    // --- the traversal cases this exists for ---------------------------

    /// The motivating case: a tenant name that escapes `keys/` would write
    /// a PRIVATE KEY somewhere else entirely.
    #[test]
    fn a_tenant_name_cannot_contain_a_path_separator() {
        assert!(validate_tenant_name("../../etc/alice").is_err());
        assert!(validate_tenant_name("a/b").is_err());
        assert!(validate_tenant_name("a\\b").is_err());
    }

    #[test]
    fn dot_and_dotdot_are_rejected_everywhere() {
        assert!(validate_tenant_name("..").is_err());
        assert!(validate_tenant_name(".").is_err());
        assert!(validate_unit_name("tv/../etc").is_err());
        assert!(validate_unit_name("..").is_err());
        assert!(validate_volume_label("..").is_err());
    }

    #[test]
    fn a_volume_label_cannot_escape_the_staging_directory() {
        // `volume::write` joins `clone-{label}-{unit}` under staging.
        assert!(validate_volume_label("../../../tmp/evil").is_err());
    }

    // --- unit names are hierarchical -----------------------------------

    /// The documented unit shape must keep working — a rule that banned
    /// `/` outright would reject every real unit in the operator guide.
    #[test]
    fn a_hierarchical_unit_name_is_valid() {
        validate_unit_name("tv/breaking-bad/s01").unwrap();
        validate_unit_name("photos").unwrap();
        validate_unit_name("media/movies/2019/the_film.4k").unwrap();
    }

    #[test]
    fn empty_unit_segments_are_rejected() {
        assert!(validate_unit_name("/leading").is_err());
        assert!(validate_unit_name("trailing/").is_err());
        assert!(validate_unit_name("double//slash").is_err());
        assert!(validate_unit_name("").is_err());
    }

    // --- ordinary names keep working -----------------------------------

    #[test]
    fn ordinary_names_are_accepted() {
        validate_tenant_name("alice").unwrap();
        validate_tenant_name("mike").unwrap();
        validate_tenant_name("ops-team_2").unwrap();
        validate_volume_label("L6-0001").unwrap();
        validate_volume_label("MHVTLR3").unwrap();
    }

    /// Every tenant name and label the test suite, gate script and operator
    /// guide use must survive — otherwise this change breaks the harness it
    /// is supposed to be safe for.
    #[test]
    fn every_name_used_by_the_existing_harnesses_is_still_valid() {
        for t in ["alice", "bob", "op", "mike", "gate-op", "test", "tenant1"] {
            validate_tenant_name(t).unwrap_or_else(|e| panic!("tenant {t}: {e}"));
        }
        for l in ["L6-0001", "L6-0009", "MHVTLR3", "TEST01"] {
            validate_volume_label(l).unwrap_or_else(|e| panic!("label {l}: {e}"));
        }
        for u in ["unit1", "unitA", "tv/show/s01", "tv/breaking-bad/s01"] {
            validate_unit_name(u).unwrap_or_else(|e| panic!("unit {u}: {e}"));
        }
    }

    // --- messages ------------------------------------------------------

    /// The rejection has to tell the operator what IS allowed — a bare
    /// "invalid name" makes them guess, and the unit rule differs from the
    /// other two.
    #[test]
    fn the_message_names_the_offending_input_and_the_rule() {
        let msg = err(validate_tenant_name("bad name"));
        assert!(msg.contains("bad name"), "{msg}");
        assert!(msg.contains("letters, digits"), "{msg}");

        let unit_msg = err(validate_unit_name("bad name"));
        assert!(
            unit_msg.contains("with / separating path segments"),
            "the unit rule differs and the message must say so: {unit_msg}"
        );
    }

    #[test]
    fn a_leading_dash_or_dot_is_rejected() {
        assert!(validate_tenant_name("-alice").is_err());
        assert!(validate_tenant_name(".alice").is_err());
        assert!(validate_unit_name("tv/-show").is_err());
    }

    #[test]
    fn over_long_components_are_rejected() {
        let long = "a".repeat(MAX_SEGMENT + 1);
        assert!(validate_tenant_name(&long).is_err());
        assert!(validate_unit_name(&format!("tv/{long}")).is_err());
        // ...but a long name made of legal-length segments fails on the
        // whole-name bound, not silently pass.
        let many = std::iter::repeat_n("abcdefgh", 40)
            .collect::<Vec<_>>()
            .join("/");
        assert!(validate_unit_name(&many).is_err());
    }
}
