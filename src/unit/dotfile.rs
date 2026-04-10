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

#[derive(Default, Serialize, Deserialize)]
struct PolicySection {
    #[serde(default = "default_checksum_mode")]
    checksum_mode: String,
    #[serde(default = "default_compression")]
    compression: String,
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
