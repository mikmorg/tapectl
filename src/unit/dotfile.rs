use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::error::{Result, TapectlError};

/// The public dotfile struct used by the rest of the codebase.
#[derive(Debug, Clone)]
pub struct UnitDotfile {
    pub uuid: String,
    pub name: String,
    pub created: String,
    pub tags: Vec<String>,
    pub tenant: String,
    pub archive_set: Option<String>,
    pub checksum_mode: Option<String>,
    pub compression: Option<String>,
    /// Issue #212: `[policy] slice_size` was read and honoured (by
    /// `policy::resolve` and `staging::resolve_slice_size_string`) since
    /// issue #47, but was never modeled here -- so `read_dotfile` dropped it
    /// and `write_dotfile` re-serialized without it, meaning a round trip
    /// through `unit rename` (read -> mutate name -> write) silently deleted
    /// an operator's deliberate slice-size choice. Same `Option`-with-no-
    /// serde-`default` rule as `warehouse_copies` below: absent means defer
    /// upward (issue #92), never a filled-in value that would look like an
    /// operator choice nobody made.
    pub slice_size: Option<String>,
    /// ADR-0006 / issue #73: how many warehouse deposits this unit should
    /// carry. `None` means the dotfile is SILENT, so the archive set (then
    /// the system default) decides. It is an `Option` with no serde
    /// `default` for the reason issue #92 records: a filled-in default is
    /// indistinguishable from a deliberate operator choice and would
    /// silently outrank the archive-set layer it is supposed to defer to.
    pub warehouse_copies: Option<i64>,
    pub exclude_patterns: Vec<String>,
}

/// Fallback checksum mode used at DB insert sites when a dotfile omits
/// `[policy] checksum_mode` (absent means defer to archive_set/defaults for
/// resolving policy, but the `units.checksum_mode` DB column is non-null).
pub const DEFAULT_CHECKSUM_MODE: &str = "mtime_size";

// ── TOML structure matching design Section 2.2 ──
//
// [unit]
// uuid = "..."
// name = "..."
// created = "..."
// tags = [...]
// tenant = "..."
// archive_set = "..."
//
// [policy]
// checksum_mode = "mtime_size"
// compression = "none"
//
// [excludes]
// patterns = [...]

/// Issue #263 / ADR-0012 line 185 ("One rule: `deny_unknown_fields` on
/// every section"): this is the top-level document shape `read_dotfile`
/// parses into, and until now it was the one section of the three
/// (`DotfileToml`, `UnitSection`, `ExcludesSection`) that had NO
/// `deny_unknown_fields` at all -- only `PolicySection` did (issue #211).
/// A misspelled top-level table name (`[polcy]` instead of `[policy]`) is
/// now refused by name here too, the same way an unknown key inside a
/// correctly-named `[policy]` table already is. This attribute is
/// deserialize-only: it does not change what `write_dotfile` serializes.
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct DotfileToml {
    unit: UnitSection,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    policy: Option<PolicySection>,
    #[serde(default)]
    excludes: ExcludesSection,
}

/// Issue #263: no `deny_unknown_fields` here meant `archive_sett = "x"`
/// (a one-letter typo of `archive_set`) deserialized cleanly to
/// `archive_set: None`, silently detaching a unit from the archive set
/// supplying its `min_copies` (`collection::sync::adopt_dotfile`,
/// `unit::discovery::sync_discovered_unit` both store `read_dotfile`'s
/// `archive_set` straight into `units.archive_set_id`).
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct UnitSection {
    uuid: String,
    name: String,
    created: String,
    #[serde(default)]
    tags: Vec<String>,
    tenant: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    archive_set: Option<String>,
}

/// `pub(crate)` (issue #211): `policy::resolve` deserializes a dotfile's
/// `[policy]` sub-table straight into this type -- rather than hand-picking
/// keys off a raw `toml::Table` the way it used to -- so `#[serde(deny_unknown_fields)]`
/// below is the ONE definition of what a dotfile's `[policy]` table may
/// contain, enforced identically by `read_dotfile` and by the resolver. A
/// misspelled key (`slize_size`, `min_copies`, anything not one of the four
/// fields here) is refused by name instead of silently deferring upward
/// forever (ADR-0012).
#[derive(Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PolicySection {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) checksum_mode: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) compression: Option<String>,
    /// Issue #212. No `#[serde(default)]` -- see `warehouse_copies` below.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) slice_size: Option<String>,
    /// No `#[serde(default)]` -- see `UnitDotfile::warehouse_copies`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) warehouse_copies: Option<i64>,
}

/// Issue #263 -- the headline defect this issue exists to close:
/// `[excludes] pattern = [...]` (singular; the real key is `patterns`)
/// parsed cleanly with no `deny_unknown_fields`, so `read_dotfile` silently
/// returned `exclude_patterns: []` and the material an operator meant to
/// exclude via this dotfile got archived, encrypted, and written to
/// write-once media anyway -- inside a valid sha256, so nothing downstream
/// ever noticed. Hand-editing this table is the DESIGNED workflow (there
/// is no CLI for `[excludes] patterns`), so this is the one place a typo
/// is most likely and least likely to be caught by any other layer.
#[derive(Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ExcludesSection {
    #[serde(default)]
    patterns: Vec<String>,
}

/// The complete set of top-level tables a `.tapectl-unit.toml` may declare
/// (see the format comment above `DotfileToml`). `policy::resolve` and
/// `staging::resolve_slice_size_string` parse a dotfile as a raw
/// `toml::Table` rather than deserializing into `DotfileToml` (they only
/// ever need the `[policy]` sub-table, and requiring a fully valid `[unit]`
/// table to reach it would be a needless coupling) — so `DotfileToml`'s own
/// `#[serde(deny_unknown_fields)]` above never runs for them. This constant
/// is the ONE definition of what a dotfile's top-level shape may contain,
/// checked directly against the raw table's keys, so a misspelled table
/// name (`[polcy]`, `[policies]`) is refused by name there too instead of
/// looking exactly like an absent `[policy]` section and silently
/// deferring upward forever (issue #263 / ADR-0012 line 185).
pub(crate) const DOTFILE_TOP_LEVEL_TABLES: [&str; 3] = ["unit", "policy", "excludes"];

/// Write dotfile to disk in the design-specified TOML format.
pub fn write_dotfile(path: &Path, data: &UnitDotfile) -> Result<()> {
    let policy = if data.checksum_mode.is_none()
        && data.compression.is_none()
        && data.slice_size.is_none()
        && data.warehouse_copies.is_none()
    {
        None
    } else {
        Some(PolicySection {
            checksum_mode: data.checksum_mode.clone(),
            compression: data.compression.clone(),
            slice_size: data.slice_size.clone(),
            warehouse_copies: data.warehouse_copies,
        })
    };

    let wrapper = DotfileToml {
        unit: UnitSection {
            uuid: data.uuid.clone(),
            name: data.name.clone(),
            created: data.created.clone(),
            tags: data.tags.clone(),
            tenant: data.tenant.clone(),
            archive_set: data.archive_set.clone(),
        },
        policy,
        excludes: ExcludesSection {
            patterns: data.exclude_patterns.clone(),
        },
    };

    let content =
        toml::to_string_pretty(&wrapper).map_err(|e| TapectlError::Other(e.to_string()))?;
    std::fs::write(path, content)?;
    Ok(())
}

/// Read and parse a dotfile from disk.
///
/// Issue #285 (ADR-0012's 2026-09-22 amendment, "every dotfile parse error
/// names the file path"): both failure branches below used to drop `path`
/// entirely (`std::fs::read_to_string(path)?`'s bare `io::Error`, and the
/// toml parse's `map_err(|e| TapectlError::Other(e.to_string()))`), so the
/// same typo was reported three different ways by three different callers
/// and `collection plan` named neither the unit nor the file. `path` is now
/// prepended on both branches; the toml error's own line/column detail is
/// kept, not replaced.
pub fn read_dotfile(path: &Path) -> Result<UnitDotfile> {
    let content = std::fs::read_to_string(path)
        .map_err(|e| TapectlError::Other(format!("{}: {e}", path.display())))?;
    let wrapper: DotfileToml = toml::from_str(&content)
        .map_err(|e| TapectlError::Other(format!("{}: {e}", path.display())))?;

    Ok(UnitDotfile {
        uuid: wrapper.unit.uuid,
        name: wrapper.unit.name,
        created: wrapper.unit.created,
        tags: wrapper.unit.tags,
        tenant: wrapper.unit.tenant,
        archive_set: wrapper.unit.archive_set,
        checksum_mode: wrapper
            .policy
            .as_ref()
            .and_then(|p| p.checksum_mode.clone()),
        compression: wrapper.policy.as_ref().and_then(|p| p.compression.clone()),
        slice_size: wrapper.policy.as_ref().and_then(|p| p.slice_size.clone()),
        warehouse_copies: wrapper.policy.as_ref().and_then(|p| p.warehouse_copies),
        exclude_patterns: wrapper.excludes.patterns,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn sample() -> UnitDotfile {
        UnitDotfile {
            uuid: "abc-123".into(),
            name: "photos".into(),
            created: "2026-01-01T00:00:00Z".into(),
            tags: vec!["media".into(), "personal".into()],
            tenant: "alice".into(),
            archive_set: Some("cold".into()),
            checksum_mode: Some("sha256".into()),
            compression: Some("lzma".into()),
            slice_size: None,
            warehouse_copies: None,
            exclude_patterns: vec!["*.tmp".into(), ".cache/".into()],
        }
    }

    #[test]
    fn write_read_round_trip() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join(".tapectl-unit.toml");
        let d = sample();
        write_dotfile(&path, &d).unwrap();
        let r = read_dotfile(&path).unwrap();
        assert_eq!(r.uuid, d.uuid);
        assert_eq!(r.name, d.name);
        assert_eq!(r.created, d.created);
        assert_eq!(r.tags, d.tags);
        assert_eq!(r.tenant, d.tenant);
        assert_eq!(r.archive_set, d.archive_set);
        assert_eq!(r.checksum_mode, d.checksum_mode);
        assert_eq!(r.compression, d.compression);
        assert_eq!(r.exclude_patterns, d.exclude_patterns);
    }

    #[test]
    fn read_leaves_policy_fields_none_when_absent() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join(".tapectl-unit.toml");
        std::fs::write(
            &path,
            r#"
[unit]
uuid = "u-1"
name = "docs"
created = "2026-01-01T00:00:00Z"
tenant = "alice"
"#,
        )
        .unwrap();
        let r = read_dotfile(&path).unwrap();
        assert_eq!(r.uuid, "u-1");
        assert_eq!(r.name, "docs");
        assert_eq!(r.tenant, "alice");
        assert!(r.tags.is_empty());
        assert!(r.archive_set.is_none());
        assert!(
            r.checksum_mode.is_none(),
            "absent [policy] checksum_mode must defer upward (Recast of v4.0 §2.2, issue #92), not fill a default"
        );
        assert!(
            r.compression.is_none(),
            "absent [policy] compression must defer upward (Recast of v4.0 §2.2, issue #92), not fill a default"
        );
        assert!(r.exclude_patterns.is_empty());
    }

    #[test]
    fn read_explicit_policy_compression_round_trips_as_some() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join(".tapectl-unit.toml");
        std::fs::write(
            &path,
            r#"
[unit]
uuid = "u-1"
name = "docs"
created = "2026-01-01T00:00:00Z"
tenant = "alice"

[policy]
compression = "gzip"
"#,
        )
        .unwrap();
        let r = read_dotfile(&path).unwrap();
        assert_eq!(r.compression, Some("gzip".to_string()));
        assert!(r.checksum_mode.is_none());
    }

    #[test]
    fn write_omits_policy_table_when_both_fields_none() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join(".tapectl-unit.toml");
        let mut d = sample();
        d.checksum_mode = None;
        d.compression = None;
        write_dotfile(&path, &d).unwrap();
        let raw = std::fs::read_to_string(&path).unwrap();
        assert!(
            !raw.contains("[policy]"),
            "no [policy] header should be written when both fields are None, got: {raw}"
        );
    }

    /// Issue #92's contract, extended to `warehouse_copies` (issue #73):
    /// an unset knob must not be materialised into the file. A written
    /// `warehouse_copies = 0` is indistinguishable from an operator
    /// deliberately choosing zero, and would silently outrank the archive
    /// set forever after.
    #[test]
    fn write_omits_warehouse_copies_when_unset_and_round_trips_when_set() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join(".tapectl-unit.toml");
        let mut d = sample();
        d.warehouse_copies = None;
        write_dotfile(&path, &d).unwrap();
        let raw = std::fs::read_to_string(&path).unwrap();
        assert!(
            !raw.contains("warehouse_copies"),
            "an unset warehouse_copies must not be written, got: {raw}"
        );
        assert!(read_dotfile(&path).unwrap().warehouse_copies.is_none());

        d.warehouse_copies = Some(2);
        write_dotfile(&path, &d).unwrap();
        assert_eq!(read_dotfile(&path).unwrap().warehouse_copies, Some(2));
    }

    /// `[policy]` must still vanish entirely when EVERY policy knob is
    /// unset -- adding a third field must not resurrect the header.
    #[test]
    fn write_omits_policy_table_when_warehouse_copies_is_the_only_unset_addition() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join(".tapectl-unit.toml");
        let mut d = sample();
        d.checksum_mode = None;
        d.compression = None;
        d.warehouse_copies = None;
        write_dotfile(&path, &d).unwrap();
        let raw = std::fs::read_to_string(&path).unwrap();
        assert!(!raw.contains("[policy]"), "got: {raw}");
    }

    #[test]
    fn write_omits_archive_set_when_none() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join(".tapectl-unit.toml");
        let mut d = sample();
        d.archive_set = None;
        write_dotfile(&path, &d).unwrap();
        let raw = std::fs::read_to_string(&path).unwrap();
        assert!(
            !raw.contains("archive_set"),
            "archive_set should be omitted when None, got: {raw}"
        );
    }

    /// Regression guard for issue #263 route 2: `archive_sett = "x"` (a
    /// one-letter typo of `archive_set`) under `[unit]` must be refused by
    /// name, not silently deserialize to `archive_set: None` and detach the
    /// unit from the archive set supplying its `min_copies`.
    /// `collection::sync::adopt_dotfile` and
    /// `unit::discovery::sync_discovered_unit` both store `read_dotfile`'s
    /// `archive_set` straight into `units.archive_set_id`.
    #[test]
    fn read_rejects_a_misspelled_archive_set_key() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join(".tapectl-unit.toml");
        std::fs::write(
            &path,
            r#"
[unit]
uuid = "u-1"
name = "docs"
created = "2026-01-01T00:00:00Z"
tenant = "alice"
archive_sett = "cold"
"#,
        )
        .unwrap();
        assert!(
            read_dotfile(&path).is_err(),
            "a misspelled archive_set key must be refused, not silently yield \
             archive_set: None (issue #263)"
        );
    }

    /// Issue #285 / ADR-0012's 2026-09-22 amendment: a malformed dotfile's
    /// error must name both the file that broke and the offending key,
    /// since nothing above `read_dotfile` re-adds either. This is the
    /// funnel `collection::fingerprint::pending_units_for_collection`
    /// (issue #285's own fix) relies on to build a refusal an operator can
    /// act on without re-deriving which file is at fault. If this fails,
    /// either failure branch in `read_dotfile` dropped the path again.
    #[test]
    fn read_dotfile_error_names_the_file_path_and_the_bad_key() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join(".tapectl-unit.toml");
        // The real issue #263 typo: `pattern` (singular) instead of
        // `patterns`.
        std::fs::write(
            &path,
            r#"
[unit]
uuid = "u-1"
name = "docs"
created = "2026-01-01T00:00:00Z"
tenant = "alice"

[excludes]
pattern = ["*.tmp"]
"#,
        )
        .unwrap();
        let err = read_dotfile(&path).unwrap_err().to_string();
        assert!(
            err.contains(&path.display().to_string()),
            "error must name the dotfile's own path: {err}"
        );
        assert!(
            err.contains("pattern"),
            "error must name the offending key: {err}"
        );
    }

    #[test]
    fn read_rejects_missing_required_fields() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join(".tapectl-unit.toml");
        // Missing tenant
        std::fs::write(
            &path,
            r#"
[unit]
uuid = "u-1"
name = "docs"
created = "2026-01-01T00:00:00Z"
"#,
        )
        .unwrap();
        assert!(read_dotfile(&path).is_err());
    }
}
