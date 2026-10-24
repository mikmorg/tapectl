use std::collections::BTreeMap;
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
    /// #478 (ADR-0012 item 82): whether this unit may span tapes by its
    /// slices. `None` means the dotfile is silent and the archive set, then
    /// `[defaults]` (off), decides; `Some(false)` is a deliberate choice that
    /// keeps the unit whole even under an archive set that allows spanning.
    /// No serde `default`, for `warehouse_copies`'s reason (issue #92).
    pub allow_spanning: Option<bool>,
    /// #493 (ADR-0012 items 148, 149, 155): how many copies this unit should
    /// hold at each named location, `[policy.copies_at]`. `None` means the
    /// dotfile is silent and the archive set, then `[defaults]`, decides;
    /// `Some(map)` replaces the lower layers' map whole (item 156), an empty
    /// one included. No serde `default`, for `warehouse_copies`'s reason
    /// (issue #92). Modeled here so a `unit rename`/`unit tag`/`tenant
    /// reassign` round trip keeps it (issue #212's lesson), and `location
    /// rename` rewrites its keys through the same writer.
    pub copies_at: Option<BTreeMap<String, i64>>,
    pub exclude_patterns: Vec<String>,
}

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
// slice_size = "1G"
// warehouse_copies = 0
// allow_spanning = false
//
// [policy.copies_at]
// home = 1
// offsite = 1
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
/// misspelled key (`slize_size`, `min_copies`, `allow_span`, anything not one
/// of the six fields here) is refused by name instead of silently deferring
/// upward forever (ADR-0012).
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
    /// #478 (ADR-0012 item 82). Skipped when unset, so a dotfile that never
    /// named it is rewritten byte for byte as before.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) allow_spanning: Option<bool>,
    /// #493 (ADR-0012 item 155): `[policy.copies_at]`, one `location =
    /// count` line each; TOML's inline `copies_at = { home = 1 }` is the same
    /// table. Last, so it serializes as a `[policy.copies_at]` sub-table after
    /// the plain keys. Skipped when unset, so a dotfile that never named it
    /// is rewritten byte for byte as before. Each count is checked (0 or
    /// more) and each name looked up among the registered locations by
    /// `policy::resolve`, not here.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) copies_at: Option<BTreeMap<String, i64>>,
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

/// The unit's own dotfile, as a path relative to the unit's root.
pub const UNIT_DOTFILE: &str = ".tapectl-unit.toml";

/// Is `rel_path` (relative to the unit's root, as walks and `files` rows
/// spell it) the unit's own dotfile? ADR-0012, 2026-10-07 item 24 (#378):
/// that one file is not content. It is left out of every change comparison
/// on both sides (the Dirty scan and the minting short-circuit through
/// `unit::content_match`, staging's size, sha256 and NEW checks, and `unit
/// check-integrity`), and is still recorded and still archived, so a `unit
/// tag`, `unit rename` or `[policy]` edit mints no Version. Only the
/// unit-root file: a `.tapectl-unit.toml` deeper in the tree is ordinary
/// content.
pub fn is_unit_dotfile(rel_path: &str) -> bool {
    rel_path == UNIT_DOTFILE
}

/// Write dotfile to disk in the design-specified TOML format.
pub fn write_dotfile(path: &Path, data: &UnitDotfile) -> Result<()> {
    let policy = if data.checksum_mode.is_none()
        && data.compression.is_none()
        && data.slice_size.is_none()
        && data.warehouse_copies.is_none()
        && data.allow_spanning.is_none()
        && data.copies_at.is_none()
    {
        None
    } else {
        Some(PolicySection {
            checksum_mode: data.checksum_mode.clone(),
            compression: data.compression.clone(),
            slice_size: data.slice_size.clone(),
            warehouse_copies: data.warehouse_copies,
            allow_spanning: data.allow_spanning,
            copies_at: data.copies_at.clone(),
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
        allow_spanning: wrapper.policy.as_ref().and_then(|p| p.allow_spanning),
        copies_at: wrapper.policy.as_ref().and_then(|p| p.copies_at.clone()),
        exclude_patterns: wrapper.excludes.patterns,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn only_the_unit_root_dotfile_is_the_unit_dotfile() {
        assert!(is_unit_dotfile(".tapectl-unit.toml"));
        assert!(!is_unit_dotfile("sub/.tapectl-unit.toml"));
        assert!(!is_unit_dotfile(".tapectl-unit.toml.bak"));
    }

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
            allow_spanning: None,
            copies_at: None,
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

    /// #478 (ADR-0012 item 82): a dotfile that does not set
    /// `allow_spanning` is rewritten byte for byte as 1.6.0 wrote it. `unit
    /// tag`, `unit rename` and `tenant reassign` all rewrite the file
    /// through `read_dotfile` and `write_dotfile`, so a key the operator
    /// never wrote must not appear, or every such rewrite would change an
    /// existing unit's dotfile (its size, and so its walk stamp in
    /// `collection::fingerprint`). The text is what 1.6.0's `write_dotfile`
    /// produced for this unit.
    #[test]
    fn a_dotfile_without_allow_spanning_is_rewritten_byte_for_byte() {
        const WRITTEN_BY_1_6_0: &str = "\
[unit]
uuid = \"d580a814-ce23-40e7-9c00-93a8fbab9459\"
name = \"family/letters\"
created = \"2026-09-28T21:48:18.261950335+00:00\"
tags = [\"letters\"]
tenant = \"family\"
archive_set = \"critical\"

[policy]
slice_size = \"2G\"
warehouse_copies = 1

[excludes]
patterns = [\"*.iso\"]
";
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join(".tapectl-unit.toml");
        std::fs::write(&path, WRITTEN_BY_1_6_0).unwrap();
        let df = read_dotfile(&path).unwrap();
        write_dotfile(&path, &df).unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), WRITTEN_BY_1_6_0);
    }

    /// #478 (ADR-0012 item 82): `allow_spanning` is a `[policy]` key. Set
    /// alone, it still writes the `[policy]` table (the guard in
    /// `write_dotfile` must count it, or the key would vanish on the next
    /// rewrite), and both values round-trip: `false` is an operator's choice
    /// to keep the unit whole, not the same as silence. Unset, it is not
    /// written.
    #[test]
    fn allow_spanning_round_trips_alone_and_is_not_written_when_unset() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join(".tapectl-unit.toml");
        let mut d = sample();
        d.checksum_mode = None;
        d.compression = None;
        for value in [true, false] {
            d.allow_spanning = Some(value);
            write_dotfile(&path, &d).unwrap();
            let raw = std::fs::read_to_string(&path).unwrap();
            assert!(raw.contains("[policy]"), "{raw}");
            assert!(raw.contains(&format!("allow_spanning = {value}")), "{raw}");
            assert_eq!(read_dotfile(&path).unwrap().allow_spanning, Some(value));
        }
        d.allow_spanning = None;
        write_dotfile(&path, &d).unwrap();
        let raw = std::fs::read_to_string(&path).unwrap();
        assert!(!raw.contains("allow_spanning"), "{raw}");
        assert!(!raw.contains("[policy]"), "{raw}");
        assert!(read_dotfile(&path).unwrap().allow_spanning.is_none());
    }

    /// #478: a misspelled spanning key is refused by name, with the file's
    /// path, never read as "not set" (issue #211's rule).
    #[test]
    fn a_misspelled_allow_spanning_is_refused_by_name() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join(".tapectl-unit.toml");
        for bad in ["allow_span", "allowSpanning"] {
            std::fs::write(
                &path,
                format!(
                    "[unit]\nuuid = \"u-1\"\nname = \"docs\"\n\
                     created = \"2026-01-01T00:00:00Z\"\ntenant = \"alice\"\n\n\
                     [policy]\n{bad} = true\n"
                ),
            )
            .unwrap();
            let err = read_dotfile(&path).unwrap_err().to_string();
            assert!(err.contains(bad), "{bad}: {err}");
            assert!(err.contains(&path.display().to_string()), "{bad}: {err}");
        }
    }

    const UNIT_TABLE: &str = "[unit]\nuuid = \"u-1\"\nname = \"docs\"\n\
                              created = \"2026-01-01T00:00:00Z\"\ntenant = \"alice\"\n\n";

    /// #493 (ADR-0012 item 155): `[policy.copies_at]` with one `location =
    /// count` line each, and TOML's inline `copies_at = { ... }` under
    /// `[policy]`, are the same table and read the same map. A 0 count is
    /// kept: it is a value, not silence.
    #[test]
    fn copies_at_reads_the_same_from_the_sub_table_and_the_inline_form() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join(".tapectl-unit.toml");
        let want: BTreeMap<String, i64> = [("home".to_string(), 1), ("offsite".to_string(), 0)]
            .into_iter()
            .collect();
        for policy in [
            "[policy.copies_at]\nhome = 1\noffsite = 0\n",
            "[policy]\ncopies_at = { home = 1, offsite = 0 }\n",
            "[policy]\nslice_size = \"2G\"\n\n[policy.copies_at]\nhome = 1\noffsite = 0\n",
        ] {
            std::fs::write(&path, format!("{UNIT_TABLE}{policy}")).unwrap();
            let df = read_dotfile(&path).unwrap();
            assert_eq!(df.copies_at.as_ref(), Some(&want), "{policy}");
        }
        std::fs::write(
            &path,
            format!("{UNIT_TABLE}[policy]\nslice_size = \"2G\"\n"),
        )
        .unwrap();
        assert!(
            read_dotfile(&path).unwrap().copies_at.is_none(),
            "unset is None"
        );
    }

    /// #493: set alone, `copies_at` still writes a policy table (the guard in
    /// `write_dotfile` counts it, or the key would vanish on the next
    /// rewrite), as the `[policy.copies_at]` sub-table after any plain
    /// `[policy]` key, and round-trips, an empty map included (an explicit
    /// "no targets", not silence). Unset, nothing is written.
    #[test]
    fn copies_at_round_trips_as_a_sub_table_and_is_not_written_when_unset() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join(".tapectl-unit.toml");
        let mut d = sample();
        d.checksum_mode = None;
        d.compression = None;
        let map: BTreeMap<String, i64> = [("home".to_string(), 1), ("offsite".to_string(), 2)]
            .into_iter()
            .collect();
        d.copies_at = Some(map.clone());
        write_dotfile(&path, &d).unwrap();
        let raw = std::fs::read_to_string(&path).unwrap();
        assert!(
            raw.contains("[policy.copies_at]\nhome = 1\noffsite = 2\n"),
            "{raw}"
        );
        assert_eq!(read_dotfile(&path).unwrap().copies_at, Some(map.clone()));

        d.slice_size = Some("2G".into());
        write_dotfile(&path, &d).unwrap();
        let raw = std::fs::read_to_string(&path).unwrap();
        let plain = raw.find("slice_size").unwrap();
        let sub = raw.find("[policy.copies_at]").unwrap();
        assert!(plain < sub, "plain keys first: {raw}");
        assert_eq!(read_dotfile(&path).unwrap().copies_at, Some(map));

        d.slice_size = None;
        d.copies_at = Some(BTreeMap::new());
        write_dotfile(&path, &d).unwrap();
        assert_eq!(
            read_dotfile(&path).unwrap().copies_at,
            Some(BTreeMap::new()),
            "an empty map is a value: {}",
            std::fs::read_to_string(&path).unwrap()
        );

        d.copies_at = None;
        write_dotfile(&path, &d).unwrap();
        let raw = std::fs::read_to_string(&path).unwrap();
        assert!(!raw.contains("copies_at"), "{raw}");
        assert!(!raw.contains("[policy"), "{raw}");
    }

    /// #493: a count that is not a whole number is refused with the file's
    /// path and the offending line, never read as "not set".
    #[test]
    fn a_copies_at_count_that_is_not_a_whole_number_is_refused_naming_the_file() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join(".tapectl-unit.toml");
        for bad in ["home = 1.5", "home = \"one\"", "home = true"] {
            std::fs::write(&path, format!("{UNIT_TABLE}[policy.copies_at]\n{bad}\n")).unwrap();
            let err = read_dotfile(&path).unwrap_err().to_string();
            assert!(err.contains(&path.display().to_string()), "{bad}: {err}");
            // toml's own error quotes the offending line and points at it.
            assert!(err.contains(bad), "{bad}: {err}");
        }
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
