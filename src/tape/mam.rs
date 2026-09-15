//! MAM (Medium Auxiliary Memory) reads via `sg_read_attr` (sg3-utils).
//!
//! Shells out to `sg_read_attr` and parses its human-readable output. All
//! fields are best-effort (`Option`) — sg_read_attr's exact labels vary across
//! sg3-utils versions and virtual/real drives, and mhvtl reports non-physical
//! capacity values, so callers treat MAM's CAPACITY figures as informational
//! and never gate the write on them. The write gate reads
//! `volumes.capacity_bytes`, decided once at `volume init` from the medium's
//! detected generation (ADR-0010 decision 3: "no path reads capacity from
//! config after init").
//!
//! MAM is not merely informational everywhere, though: the medium SERIAL read
//! here is the cartridge's identity (ADR-0012) and the density codes are
//! ADR-0010's first two detection sources, so both genuinely decide
//! behaviour.

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
    pub manufacturer: Option<String>,
    pub length_meters: Option<i64>,
    /// "Medium density code" — the generation the physical medium was
    /// formatted at, per ADR-0010's first (highest-priority) detection
    /// source. `sg_read_attr` reports it as hex with a `0x` prefix, e.g.
    /// `0x5a`.
    pub medium_density_code: Option<u8>,
    /// "Format density code" — ADR-0010's second detection source, used
    /// when the medium density code is unavailable (mhvtl reports only
    /// this one). Same `0x..` hex format.
    pub format_density_code: Option<u8>,
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
        } else if label.eq_ignore_ascii_case("Medium manufacturer") {
            if !value.is_empty() {
                m.manufacturer = Some(value.to_string());
            }
        } else if label.eq_ignore_ascii_case("Medium length [m]") {
            m.length_meters = value.parse::<i64>().ok();
        } else if label.eq_ignore_ascii_case("Medium density code") {
            m.medium_density_code = parse_hex_density(value);
        } else if label.eq_ignore_ascii_case("Format density code") {
            m.format_density_code = parse_hex_density(value);
        }
    }
    m
}

/// Parse a `sg_read_attr` density code value, e.g. `"0x5a"`, into its byte.
fn parse_hex_density(value: &str) -> Option<u8> {
    u8::from_str_radix(
        value
            .trim()
            .trim_start_matches("0x")
            .trim_start_matches("0X"),
        16,
    )
    .ok()
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

    // Captured verbatim from a real HP LTO-6 drive's sg_read_attr output
    // (/scratch/tapectl-lto6-session/recordings/preflight-mam.txt, 2026-09-10
    // preflight session, issue #123). Includes trailing-space padding on
    // some values, as sg_read_attr actually emits — exercises value.trim().
    const LTO6_SAMPLE: &str = "Attribute values:
  Remaining capacity in partition [MiB]: 2499053
  Maximum capacity in partition [MiB]: 2499053
  TapeAlert flags: 0
  Load count: 1
  MAM space remaining [B]: 3062
  Assigning organization: LTO-CVE 
  Format density code: 0x5a
  Initialization count: 0
  Volume identifier: 
  Volume change reference: 0x0
  Density vendor/serial number at last load: HP      HUJ808A5L4                      
  Density vendor/serial number at load-1: HP      
  Density vendor/serial number at load-2: HP      
  Density vendor/serial number at load-3: HP      
  Total MiB written in medium life: 0
  Total MiB read in medium life: 0
  Total MiB written in current/last load: 0
  Total MiB read in current/last load: 0
  Logical position of first encrypted block: <unknown> [ff]
  Logical position of first unencrypted block -
      after first encrypted block: <unknown> [ff]
  Medium manufacturer: FUJIFILM
  Medium serial number: EW7VWMVKF6                      
  Medium length [m]: 846
  Medium width [0.1 mm]: 127
  Assigning organization: LTO-CVE 
  Medium density code: 0x5a
  Medium manufacture date: 20170824
  MAM capacity [B]: 16384
  Medium type: 0x0
  Medium type information: 0x0
  Vendor specific medium attribute 0x1000: 
 00     02 73 1d 28 47 36 41 43  56 32 58 31 46 55 4a 49    .s.(G6ACV2X1FUJI
 10     46 49 4c 4d 00 06 0f e1  00 20 00 00                FILM..... ..
  Vendor specific medium attribute 0x1001: 
 00     02 73 1d 28 47 36 41 43  56 32 58 31 45 57 37 56    .s.(G6ACV2X1EW7V
 10     57 4d 56 4b 46 36 00 20                             WMVKF6. 
";

    #[test]
    fn parses_manufacturer_and_length_from_real_lto6_output() {
        let m = parse_mam(LTO6_SAMPLE);
        assert_eq!(m.manufacturer.as_deref(), Some("FUJIFILM"));
        assert_eq!(m.length_meters, Some(846));
        assert_eq!(m.serial.as_deref(), Some("EW7VWMVKF6"));
        assert_eq!(m.load_count, Some(1));
        assert_eq!(m.max_capacity_bytes, Some(2499053 * MIB));
    }

    /// ADR-0010 detection source 1: the real LTO-6 capture carries both
    /// density fields (medium and format), each `0x5a` — LTO-6's density
    /// code (`Generation::Lto6.density_code()`).
    #[test]
    fn parses_medium_and_format_density_codes_from_real_lto6_output() {
        let m = parse_mam(LTO6_SAMPLE);
        assert_eq!(m.medium_density_code, Some(0x5a));
        assert_eq!(m.format_density_code, Some(0x5a));
    }

    // Shaped after mhvtl's actual `sg_read_attr` output (ADR-0010 detection
    // source 2): mhvtl reports only the format density code, and its
    // manufacturer string is the fixed "linuxVTL" rather than a real vendor.
    //
    // This particular sample carries no "Medium serial number" line, which is
    // what the test below pins. Do NOT read that as "mhvtl exposes no serial"
    // — it does, and stably: ADR-0010's 2026-09-14 correction records
    // `E01001L8_1775794348` from this repo's own recording, and
    // `volume::binding`'s tests bind against it. An unbound volume is a copy
    // the catalog cannot place, so that distinction matters.
    const MHVTL_SHAPED_SAMPLE: &str = "Attribute values:
  Remaining capacity in partition [MiB]: 2400000
  Maximum capacity in partition [MiB]: 2400000
  TapeAlert flags: 0
  Load count: 3
  Format density code: 0x5e
  Medium manufacturer: linuxVTL
  Medium length [m]: 900
";

    #[test]
    fn mhvtl_shaped_sample_has_format_code_but_no_medium_code_or_serial() {
        let m = parse_mam(MHVTL_SHAPED_SAMPLE);
        assert_eq!(m.format_density_code, Some(0x5e));
        assert_eq!(m.medium_density_code, None);
        assert_eq!(m.serial, None);
        assert_eq!(m.manufacturer.as_deref(), Some("linuxVTL"));
        assert_eq!(m.length_meters, Some(900));
    }

    #[test]
    fn mhvtl_sample_has_no_manufacturer_or_length() {
        let m = parse_mam(SAMPLE);
        assert_eq!(m.manufacturer, None);
        assert_eq!(m.length_meters, None);
        // Existing assertions still hold.
        assert_eq!(m.max_capacity_bytes, Some(500 * MIB));
        assert_eq!(m.remaining_bytes, Some(476 * MIB));
        assert_eq!(m.serial.as_deref(), Some("F01030L6_1775794349"));
        assert_eq!(m.load_count, Some(4));
    }
}
