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
    pub checksum_mode: String,
    pub compression: String,
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
//
// [excludes]
// patterns = [...]

#[derive(Serialize, Deserialize)]
struct DotfileToml {
    unit: UnitSection,
    #[serde(default)]
    policy: PolicySection,
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

#[derive(Serialize, Deserialize)]
struct PolicySection {
    #[serde(default = "default_checksum_mode")]
    checksum_mode: String,
    #[serde(default = "default_compression")]
    compression: String,
}

impl Default for PolicySection {
    fn default() -> Self {
        Self {
            checksum_mode: default_checksum_mode(),
            compression: default_compression(),
        }
    }
}

fn default_checksum_mode() -> String {
    "mtime_size".to_string()
}
fn default_compression() -> String {
    "none".to_string()
}

#[derive(Default, Serialize, Deserialize)]
struct ExcludesSection {
    #[serde(default)]
    patterns: Vec<String>,
}

/// Write dotfile to disk in the design-specified TOML format.
pub fn write_dotfile(path: &Path, data: &UnitDotfile) -> Result<()> {
    let wrapper = DotfileToml {
        unit: UnitSection {
            uuid: data.uuid.clone(),
            name: data.name.clone(),
            created: data.created.clone(),
            tags: data.tags.clone(),
            tenant: data.tenant.clone(),
            archive_set: data.archive_set.clone(),
        },
        policy: PolicySection {
            checksum_mode: data.checksum_mode.clone(),
            compression: data.compression.clone(),
        },
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
        checksum_mode: wrapper.policy.checksum_mode,
        compression: wrapper.policy.compression,
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
            checksum_mode: "sha256".into(),
            compression: "lzma".into(),
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
    fn read_applies_defaults_for_missing_policy_and_excludes() {
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
        assert_eq!(r.checksum_mode, "mtime_size");
        assert_eq!(r.compression, "none");
        assert!(r.exclude_patterns.is_empty());
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
