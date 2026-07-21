//! MAM (Medium Auxiliary Memory) reads via `sg_read_attr` (sg3-utils).
//!
//! Shells out to `sg_read_attr` and parses its human-readable output. All
//! fields are best-effort (`Option`) — sg_read_attr's exact labels vary across
//! sg3-utils versions and virtual/real drives, and mhvtl reports non-physical
//! capacity values, so callers treat MAM as informational and never gate the
//! write on it (the write gate uses the configured nominal capacity).

use std::process::Command;

use crate::error::{Result, TapectlError};

const MIB: i64 = 1024 * 1024;

/// A subset of the cartridge's MAM attributes.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct MamInfo {
    pub max_capacity_bytes: Option<i64>,
    pub remaining_bytes: Option<i64>,
    pub serial: Option<String>,
    pub load_count: Option<i64>,
}

/// Read MAM attributes from the drive's sg device.
pub fn read_mam(sg_device: &str) -> Result<MamInfo> {
    let output = Command::new("sg_read_attr")
        .arg(sg_device)
        .output()
        .map_err(|e| TapectlError::Other(format!("sg_read_attr spawn failed: {e}")))?;
    if !output.status.success() {
        return Err(TapectlError::Other(format!(
            "sg_read_attr exit {}",
            output.status
        )));
    }
    Ok(parse_mam(&String::from_utf8_lossy(&output.stdout)))
}

/// Parse `sg_read_attr`'s human-readable attribute listing.
pub fn parse_mam(raw: &str) -> MamInfo {
    let mut m = MamInfo::default();
    for line in raw.lines() {
        let Some((label, value)) = line.split_once(':') else {
            continue;
        };
        let label = label.trim();
        let value = value.trim();
        if label.eq_ignore_ascii_case("Maximum capacity in partition [MiB]") {
            m.max_capacity_bytes = value.parse::<i64>().ok().map(|v| v.saturating_mul(MIB));
        } else if label.eq_ignore_ascii_case("Remaining capacity in partition [MiB]") {
            m.remaining_bytes = value.parse::<i64>().ok().map(|v| v.saturating_mul(MIB));
        } else if label.eq_ignore_ascii_case("Medium serial number") {
            if !value.is_empty() {
                m.serial = Some(value.to_string());
            }
        } else if label.eq_ignore_ascii_case("Load count") {
            m.load_count = value.parse::<i64>().ok();
        }
    }
    m
}

#[cfg(test)]
mod tests {
    use super::*;

    // Captured from mhvtl's sg_read_attr (docs/mhvtl-baseline-recordings.txt).
    const SAMPLE: &str = "Attribute values:
  Remaining capacity in partition [MiB]: 476
  Maximum capacity in partition [MiB]: 500
  TapeAlert flags: 0
  Load count: 4
  Medium serial number: F01030L6_1775794349
";

    #[test]
    fn parses_capacity_serial_and_loads() {
        let m = parse_mam(SAMPLE);
        assert_eq!(m.max_capacity_bytes, Some(500 * MIB));
        assert_eq!(m.remaining_bytes, Some(476 * MIB));
        assert_eq!(m.serial.as_deref(), Some("F01030L6_1775794349"));
        assert_eq!(m.load_count, Some(4));
    }

    #[test]
    fn missing_fields_are_none_not_error() {
        let m = parse_mam("Attribute values:\n  TapeAlert flags: 0\n");
        assert_eq!(m, MamInfo::default());
    }
}
