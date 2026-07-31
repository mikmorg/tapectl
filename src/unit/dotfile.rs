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

#[derive(Serialize, Deserialize)]
struct DotfileToml {
    unit: UnitSection,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    policy: Option<PolicySection>,
    #[serde(default)]
    excludes: ExcludesSection,
}

#[derive(Serialize, Deserialize)]
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

#[derive(Default, Serialize, Deserialize)]
struct PolicySection {
    #[serde(skip_serializing_if = "Option::is_none")]
    checksum_mode: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    compression: Option<String>,
    /// No `#[serde(default)]` -- see `UnitDotfile::warehouse_copies`.
    #[serde(skip_serializing_if = "Option::is_none")]
    warehouse_copies: Option<i64>,
}

#[derive(Default, Serialize, Deserialize)]
struct ExcludesSection {
    #[serde(default)]
    patterns: Vec<String>,
}

/// Write dotfile to disk in the design-specified TOML format.
pub fn write_dotfile(path: &Path, data: &UnitDotfile) -> Result<()> {
    let policy = if data.checksum_mode.is_none()
        && data.compression.is_none()
        && data.warehouse_copies.is_none()
    {
        None
    } else {
        Some(PolicySection {
            checksum_mode: data.checksum_mode.clone(),
            compression: data.compression.clone(),
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
pub fn read_dotfile(path: &Path) -> Result<UnitDotfile> {
    let content = std::fs::read_to_string(path)?;
    let wrapper: DotfileToml =
        toml::from_str(&content).map_err(|e| TapectlError::Other(e.to_string()))?;

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
