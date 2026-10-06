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

    // ── The chip's lifetime attributes (issue #299, ADR-0012 2026-10-06
    // item 11). Kept, not converted: each is the integer the drive printed,
    // in the unit its label names (`MiB` on every capture so far). Whether
    // MAM's `[MiB]` is honest is #182's open question, so no `* MIB`
    // multiply happens here — the field names carry the unit instead, and a
    // ruling on #182 changes how these are READ, never what was parsed.
    /// "Total MiB written in medium life" — lifetime write wear.
    pub life_written_mib_raw: Option<i64>,
    /// "Total MiB read in medium life" — lifetime read wear.
    pub life_read_mib_raw: Option<i64>,
    /// "Total MiB written in current/last load". Whether this resets per
    /// load or per power-on is not established: shown, never differenced.
    pub load_written_mib_raw: Option<i64>,
    /// "Total MiB read in current/last load"; see `load_written_mib_raw`.
    pub load_read_mib_raw: Option<i64>,
    /// "Initialization count" — how many times the medium was re-initialised.
    pub initialization_count: Option<i64>,
    /// "Medium manufacture date", verbatim (`20170824` on the one real
    /// capture). One sample is one sample: [`MamInfo::manufacture_date`]
    /// derives a date on read, and the raw text is what is kept.
    pub manufacture_date_raw: Option<String>,
    /// "TapeAlert flags" as the chip reports them — `Some(0)` when the line
    /// says 0 (the drive said zero, which is not the same fact as "never
    /// asked").
    pub tape_alert_flags: Option<i64>,
    /// "Density vendor/serial number at last load", `load-1`, `load-2`,
    /// `load-3`: the last four drives that loaded this cartridge, most
    /// recent first. A slot is `Some` whenever its line is present — even
    /// one carrying a vendor and no serial, which is what the real HP
    /// capture shows for three of the four. `None` would claim the chip said
    /// nothing, which is a different fact.
    pub drive_ring: [Option<DriveRingSlot>; 4],
}

/// One slot of the chip's four-deep "Density vendor/serial number at …"
/// ring (issue #299).
///
/// The raw value is kept beside the split because one capture cannot tell
/// "vendor recorded, serial blank" from "the tool padded an empty record":
/// a parser that collapsed them would destroy the evidence that settles it.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct DriveRingSlot {
    /// The value as printed, padding trimmed from both ends.
    pub raw: String,
    /// The first 8 characters (the T10 vendor field), trimmed.
    pub vendor: String,
    /// Everything after them, trimmed — possibly empty.
    pub serial: String,
}

impl DriveRingSlot {
    /// `value` is the text after the label's colon, its one separating
    /// space removed and the padding inside it intact.
    fn parse(value: &str) -> DriveRingSlot {
        let value = value.trim_end();
        let (vendor, serial) = match value.char_indices().nth(8) {
            Some((i, _)) => value.split_at(i),
            None => (value, ""),
        };
        DriveRingSlot {
            raw: value.trim().to_string(),
            vendor: vendor.trim().to_string(),
            serial: serial.trim().to_string(),
        }
    }
}

impl MamInfo {
    /// [`MamInfo::manufacture_date_raw`] read as `YYYYMMDD`, when it is one.
    pub fn manufacture_date(&self) -> Option<chrono::NaiveDate> {
        let raw = self.manufacture_date_raw.as_deref()?.trim();
        chrono::NaiveDate::parse_from_str(raw, "%Y%m%d").ok()
    }

    /// Whether the chip reported any of its lifetime attributes — "this
    /// chip says nothing about wear" (mhvtl) is a different fact from a
    /// chip reporting zeros (a new cartridge).
    pub fn has_lifetime_attributes(&self) -> bool {
        self.life_written_mib_raw.is_some()
            || self.life_read_mib_raw.is_some()
            || self.initialization_count.is_some()
            || self.manufacture_date_raw.is_some()
            || self.drive_ring.iter().any(Option::is_some)
    }
}

/// The program [`read_mam`] runs. One constant, so the argv the journal
/// records and the argv actually spawned cannot drift.
pub const MAM_TOOL: &str = "sg_read_attr";

/// One MAM read, exactly as the hardware and the tool gave it (issue #297,
/// ADR-0013's "capture everything verbatim now, parse it later").
///
/// Deliberately a SIBLING of [`MamInfo`], never a field of it: `MamInfo` is
/// the parse, and it compares equal to `MamInfo::default()` for output that
/// carries none of its attributes. The capture is the observation, and the
/// attributes `MamInfo` does not keep — the second `Assigning organization`,
/// the vendor-specific hex blocks, anything a later drive adds — survive
/// only here. (`MamInfo` has kept the lifetime attributes and the four-deep
/// drive ring since issue #299; this capture kept them before that.)
///
/// No hook, trigger or contact here: which call site took the reading, for
/// which command, inside which contact, are facts about the CALLER, attached
/// when the row is written ([`crate::tape::mam_journal`]).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MamCapture {
    /// When the read happened, as SQLite's `datetime('now')` spells it
    /// (UTC). Taken at the read, not at the INSERT — on the read paths the
    /// row is written only once the contact opens, later.
    pub captured_at: String,
    pub device_sg: String,
    /// The tape node the reading was taken for, when the caller knew one
    /// ([`crate::tape::media_detect::detect`] fills it; `read_mam` alone
    /// only knows the sg node).
    pub device_tape: Option<String>,
    /// The command line actually spawned, program first.
    pub tool_argv: Vec<String>,
    /// `sg_read_attr -V`, cached once per process; `None` when it could not
    /// be read — never a reason to fail the capture.
    pub tool_version: Option<String>,
    /// stdout, byte for byte, untrimmed. `None` when the tool never ran
    /// (spawn failure); `Some` — possibly empty — when it ran, whatever its
    /// exit status.
    pub stdout: Option<Vec<u8>>,
    /// `None` exactly when the read succeeded. Otherwise the spawn error, or
    /// the exit status with the tool's stderr verbatim.
    pub error: Option<String>,
}

impl MamCapture {
    /// Whether the read succeeded: the tool ran, exited 0, and no error was
    /// recorded.
    pub fn ok(&self) -> bool {
        self.error.is_none() && self.stdout.is_some()
    }
}

/// One [`read_mam`]: the capture, and the parse of it.
#[derive(Debug, Clone)]
pub struct MamRead {
    /// The parse. `MamInfo::default()` when the read failed — the same
    /// best-effort absence every caller already handles.
    pub info: MamInfo,
    pub capture: MamCapture,
    /// The failure, for the caller's `warn!` — the same text
    /// `capture.error` records.
    pub error: Option<String>,
}

/// Read MAM attributes from the drive's sg device.
///
/// Infallible by shape: a failed read is a [`MamRead`] whose capture says so
/// (`ok = 0` in the journal), because a failed read is also an observation
/// worth recording. The process-spawning half; everything it decides is in
/// [`capture_from_output`], which the tests drive by value.
pub fn read_mam(sg_device: &str) -> MamRead {
    let captured_at = now_sqlite();
    let output = crate::progress::waited(
        || format!("{MAM_TOOL} on {sg_device}"),
        || Command::new(MAM_TOOL).arg(sg_device).output(),
    );
    capture_from_output(sg_device, captured_at, tool_version(), output)
}

/// Build the capture and the parse from what the process gave — the pure
/// half of [`read_mam`].
pub fn capture_from_output(
    sg_device: &str,
    captured_at: String,
    tool_version: Option<String>,
    output: std::io::Result<std::process::Output>,
) -> MamRead {
    let mut capture = MamCapture {
        captured_at,
        device_sg: sg_device.to_string(),
        device_tape: None,
        tool_argv: vec![MAM_TOOL.to_string(), sg_device.to_string()],
        tool_version,
        stdout: None,
        error: None,
    };
    let info = match output {
        Err(e) => {
            capture.error = Some(format!("{MAM_TOOL} spawn failed: {e}"));
            MamInfo::default()
        }
        Ok(output) => {
            capture.stdout = Some(output.stdout.clone());
            if output.status.success() {
                parse_mam(&String::from_utf8_lossy(&output.stdout))
            } else {
                let stderr = String::from_utf8_lossy(&output.stderr);
                capture.error = Some(if stderr.trim().is_empty() {
                    format!("{MAM_TOOL} exit {}", output.status)
                } else {
                    format!("{MAM_TOOL} exit {}: {stderr}", output.status)
                });
                MamInfo::default()
            }
        }
    };
    MamRead {
        info,
        error: capture.error.clone(),
        capture,
    }
}

/// `sg_read_attr -V`, run at most ONCE per process and cached.
///
/// The version cannot change under a running process, and a contact must
/// not pay for a second spawn to learn it again. A failure to read it is
/// `None`, never an error: the capture is the point, its provenance is a
/// courtesy.
fn tool_version() -> Option<String> {
    static VERSION: std::sync::OnceLock<Option<String>> = std::sync::OnceLock::new();
    VERSION
        .get_or_init(|| {
            let out = Command::new(MAM_TOOL).arg("-V").output().ok()?;
            // sg3-utils prints the version on stderr; accept either stream.
            let text = if out.stdout.is_empty() {
                out.stderr
            } else {
                out.stdout
            };
            let text = String::from_utf8_lossy(&text).trim().to_string();
            (!text.is_empty()).then_some(text)
        })
        .clone()
}

/// The current UTC time in `datetime('now')`'s spelling.
fn now_sqlite() -> String {
    chrono::Utc::now().format("%Y-%m-%d %H:%M:%S").to_string()
}

/// The attributes [`parse_mam`] reads, as the journal's `parsed_json`
/// records them: the number the hardware gave and the unit its label named,
/// NOT `MamInfo`'s converted `*_bytes` figures (ADR-0013, "capture stores the
/// number the hardware gave, and its label"; issue #182 — whether `[MiB]` is
/// honest is unsettled, and a converted figure would bake an interpretation
/// into the record).
///
/// Matched on the label STEM, with the unit read back out of the label's own
/// `[..]` suffix, so an sg3-utils that printed `[MB]` would be recorded as
/// `MB` rather than silently missed or mislabelled.
const JOURNAL_ATTRIBUTES: &[(&str, &str)] = &[
    ("max_capacity", "Maximum capacity in partition"),
    ("remaining_capacity", "Remaining capacity in partition"),
    ("medium_serial", "Medium serial number"),
    ("load_count", "Load count"),
    ("medium_manufacturer", "Medium manufacturer"),
    ("medium_length", "Medium length"),
    ("medium_density_code", "Medium density code"),
    ("format_density_code", "Format density code"),
    // Issue #299: the lifetime attributes `parse_mam` now keeps.
    ("life_written", "Total MiB written in medium life"),
    ("life_read", "Total MiB read in medium life"),
    ("load_written", "Total MiB written in current/last load"),
    ("load_read", "Total MiB read in current/last load"),
    ("initialization_count", "Initialization count"),
    ("manufacture_date", "Medium manufacture date"),
    ("tape_alert_flags", "TapeAlert flags"),
    (
        "ring_last_load",
        "Density vendor/serial number at last load",
    ),
    ("ring_load_1", "Density vendor/serial number at load-1"),
    ("ring_load_2", "Density vendor/serial number at load-2"),
    ("ring_load_3", "Density vendor/serial number at load-3"),
];

/// The journal's `parsed_json` for one raw capture.
///
/// Each recognised attribute becomes `{"label", "value", "unit"}`: `label`
/// verbatim as printed, `unit` from its `[..]` suffix (`null` when it has
/// none), and `value` an integer when the printed value is a decimal
/// integer, the integer a `0x..` density code spells (with the printed text
/// kept as `"text"`), otherwise the trimmed string. A later line with the
/// same label wins, as it does in [`parse_mam`].
pub fn journal_attributes(raw: &str) -> serde_json::Value {
    let mut out = serde_json::Map::new();
    for line in raw.lines() {
        let Some((label, value)) = line.split_once(':') else {
            continue;
        };
        let label = label.trim();
        let (stem, unit) = match label.rsplit_once('[') {
            Some((stem, rest)) if rest.ends_with(']') => {
                (stem.trim(), Some(rest.trim_end_matches(']').to_string()))
            }
            _ => (label, None),
        };
        let Some((key, _)) = JOURNAL_ATTRIBUTES
            .iter()
            .find(|(_, s)| s.eq_ignore_ascii_case(stem))
        else {
            continue;
        };
        let text = value.trim();
        let mut entry = serde_json::Map::new();
        entry.insert("label".into(), label.into());
        if let Ok(n) = text.parse::<i64>() {
            entry.insert("value".into(), n.into());
        } else if key.ends_with("density_code") {
            match parse_hex_density(text) {
                Some(b) => entry.insert("value".into(), i64::from(b).into()),
                None => entry.insert("value".into(), text.into()),
            };
            entry.insert("text".into(), text.into());
        } else {
            entry.insert("value".into(), text.into());
        }
        entry.insert(
            "unit".into(),
            unit.map_or(serde_json::Value::Null, serde_json::Value::from),
        );
        out.insert((*key).to_string(), serde_json::Value::Object(entry));
    }
    serde_json::Value::Object(out)
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
        } else if label.eq_ignore_ascii_case("Total MiB written in medium life") {
            m.life_written_mib_raw = value.parse::<i64>().ok();
        } else if label.eq_ignore_ascii_case("Total MiB read in medium life") {
            m.life_read_mib_raw = value.parse::<i64>().ok();
        } else if label.eq_ignore_ascii_case("Total MiB written in current/last load") {
            m.load_written_mib_raw = value.parse::<i64>().ok();
        } else if label.eq_ignore_ascii_case("Total MiB read in current/last load") {
            m.load_read_mib_raw = value.parse::<i64>().ok();
        } else if label.eq_ignore_ascii_case("Initialization count") {
            m.initialization_count = value.parse::<i64>().ok();
        } else if label.eq_ignore_ascii_case("Medium manufacture date") {
            if !value.is_empty() {
                m.manufacture_date_raw = Some(value.to_string());
            }
        } else if label.eq_ignore_ascii_case("TapeAlert flags") {
            m.tape_alert_flags = parse_int_or_hex(value);
        } else if let Some(slot) = ring_slot(label) {
            // The padding between vendor and serial is meaningful, so the
            // slot is parsed from the value before `trim`.
            let (_, untrimmed) = line.split_once(':').unwrap_or_default();
            let untrimmed = untrimmed.strip_prefix(' ').unwrap_or(untrimmed);
            m.drive_ring[slot] = Some(DriveRingSlot::parse(untrimmed));
        }
    }
    m
}

/// Which drive-ring slot a label names, most recent load first.
fn ring_slot(label: &str) -> Option<usize> {
    const RING: [&str; 4] = [
        "Density vendor/serial number at last load",
        "Density vendor/serial number at load-1",
        "Density vendor/serial number at load-2",
        "Density vendor/serial number at load-3",
    ];
    RING.iter().position(|r| label.eq_ignore_ascii_case(r))
}

/// A decimal integer, or a `0x..` hex one.
fn parse_int_or_hex(value: &str) -> Option<i64> {
    let v = value.trim();
    match v.strip_prefix("0x").or_else(|| v.strip_prefix("0X")) {
        Some(hex) => i64::from_str_radix(hex, 16).ok(),
        None => v.parse::<i64>().ok(),
    }
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

/// Recorded `sg_read_attr` outputs, shared with `tape::mam_journal`'s tests
/// (issue #297) so the journal's verbatim guarantee is proved against the
/// same pinned captures the parser is.
#[cfg(test)]
pub(crate) mod tests_support {
    // Captured verbatim from a real HP LTO-6 drive's sg_read_attr output
    // (/scratch/tapectl-lto6-session/recordings/preflight-mam.txt, 2026-09-10
    // preflight session, issue #123). Includes trailing-space padding on
    // some values, as sg_read_attr actually emits — exercises value.trim().
    pub(crate) const LTO6_SAMPLE: &str = "Attribute values:
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
    pub(crate) const MHVTL_SHAPED_SAMPLE: &str = "Attribute values:
  Remaining capacity in partition [MiB]: 2400000
  Maximum capacity in partition [MiB]: 2400000
  TapeAlert flags: 0
  Load count: 3
  Format density code: 0x5e
  Medium manufacturer: linuxVTL
  Medium length [m]: 900
";
}

#[cfg(test)]
mod tests {
    use super::tests_support::{LTO6_SAMPLE, MHVTL_SHAPED_SAMPLE};
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
        let m = parse_mam("Attribute values:\n  MAM space remaining [B]: 3062\n");
        assert_eq!(m, MamInfo::default());
    }

    // ── the lifetime attributes (issue #299) ──

    /// The controlling distinction: on the real HP capture every lifetime
    /// counter is present and zero, so each parses as `Some(0)` — "the
    /// drive said zero" — never `None`, "we never asked".
    #[test]
    fn lto6_lifetime_attributes_parse_as_present_zeros_not_absent() {
        let m = parse_mam(LTO6_SAMPLE);
        assert_eq!(m.life_written_mib_raw, Some(0));
        assert_eq!(m.life_read_mib_raw, Some(0));
        assert_eq!(m.load_written_mib_raw, Some(0));
        assert_eq!(m.load_read_mib_raw, Some(0));
        assert_eq!(m.initialization_count, Some(0));
        assert_eq!(m.tape_alert_flags, Some(0));
        assert!(m.has_lifetime_attributes());
    }

    /// The positive control for the test above, in ONE fixture: the mhvtl
    /// sample carries no lifetime line, so every lifetime field is `None`
    /// — while its `TapeAlert flags: 0` line IS present and parses as
    /// `Some(0)`. It can pass neither by a blanket default nor by parsing
    /// nothing.
    #[test]
    fn mhvtl_sample_has_tape_alert_flags_but_no_lifetime_attributes() {
        let m = parse_mam(SAMPLE);
        assert_eq!(m.life_written_mib_raw, None);
        assert_eq!(m.life_read_mib_raw, None);
        assert_eq!(m.load_written_mib_raw, None);
        assert_eq!(m.load_read_mib_raw, None);
        assert_eq!(m.initialization_count, None);
        assert_eq!(m.manufacture_date_raw, None);
        assert_eq!(m.drive_ring, [None, None, None, None]);
        assert_eq!(m.tape_alert_flags, Some(0));
        assert!(!m.has_lifetime_attributes());
    }

    /// The drive ring: `at last load` splits into vendor and serial; the
    /// three older slots carry a vendor and no serial, and are PRESENT with
    /// an empty serial — never `None`, which would claim the chip said
    /// nothing. The raw value is kept beside the split.
    #[test]
    fn the_drive_ring_keeps_every_slot_and_its_raw_value() {
        let m = parse_mam(LTO6_SAMPLE);
        let last = m.drive_ring[0].as_ref().expect("last-load slot present");
        assert_eq!(last.vendor, "HP");
        assert_eq!(last.serial, "HUJ808A5L4");
        assert_eq!(last.raw, "HP      HUJ808A5L4");
        for (i, slot) in m.drive_ring.iter().enumerate().skip(1) {
            let slot = slot
                .as_ref()
                .unwrap_or_else(|| panic!("ring slot {i} must be present, not None"));
            assert_eq!(slot.vendor, "HP", "slot {i}");
            assert_eq!(slot.serial, "", "slot {i}");
            assert_eq!(slot.raw, "HP", "slot {i}");
        }
    }

    /// The manufacture date is kept as the raw text; the date is a
    /// separate, derived accessor.
    #[test]
    fn the_manufacture_date_is_kept_raw_and_derived_on_read() {
        let m = parse_mam(LTO6_SAMPLE);
        assert_eq!(m.manufacture_date_raw.as_deref(), Some("20170824"));
        assert_eq!(
            m.manufacture_date(),
            chrono::NaiveDate::from_ymd_opt(2017, 8, 24)
        );
        let odd = MamInfo {
            manufacture_date_raw: Some("2017-08".into()),
            ..MamInfo::default()
        };
        assert_eq!(odd.manufacture_date(), None);
    }

    /// SYNTHETIC, not a recording: distinct non-zero counters, because the
    /// real capture's zeros cannot detect a stray unit conversion
    /// (`0 * 2^20 == 0`). The parsed value must equal the printed integer —
    /// #182 (is MAM's `[MiB]` honest?) stays cheap to settle only while no
    /// converted figure is kept.
    const SYNTHETIC_WORN: &str = "Attribute values:
  TapeAlert flags: 0x40
  Load count: 812
  Initialization count: 3
  Total MiB written in medium life: 41230117
  Total MiB read in medium life: 9876543
  Total MiB written in current/last load: 2301
  Total MiB read in current/last load: 17
";

    #[test]
    fn lifetime_counters_are_the_printed_integers_with_no_unit_conversion() {
        let m = parse_mam(SYNTHETIC_WORN);
        assert_eq!(m.life_written_mib_raw, Some(41_230_117));
        assert_eq!(m.life_read_mib_raw, Some(9_876_543));
        assert_eq!(m.load_written_mib_raw, Some(2301));
        assert_eq!(m.load_read_mib_raw, Some(17));
        assert_eq!(m.initialization_count, Some(3));
        assert_eq!(m.tape_alert_flags, Some(0x40));
        assert_eq!(m.load_count, Some(812));
    }

    /// The journal's `parsed_json` keeps them too, with the unit the label
    /// named.
    #[test]
    fn journal_attributes_keep_the_lifetime_attributes_and_their_unit() {
        let v = journal_attributes(SYNTHETIC_WORN);
        assert_eq!(v["life_written"]["value"], 41_230_117);
        assert_eq!(v["life_written"]["unit"], serde_json::Value::Null);
        assert_eq!(v["initialization_count"]["value"], 3);
        let v = journal_attributes(LTO6_SAMPLE);
        assert_eq!(v["ring_last_load"]["value"], "HP      HUJ808A5L4");
        assert_eq!(v["manufacture_date"]["value"], 20170824);
    }

    // `LTO6_SAMPLE`: the real HP LTO-6 capture, in `tests_support` above.

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

    // `MHVTL_SHAPED_SAMPLE`: in `tests_support` above — it carries no serial.

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

    // ── the capture (issue #297) ──

    fn output(code: i32, stdout: &str, stderr: &str) -> std::process::Output {
        use std::os::unix::process::ExitStatusExt;
        std::process::Output {
            status: std::process::ExitStatus::from_raw(code << 8),
            stdout: stdout.as_bytes().to_vec(),
            stderr: stderr.as_bytes().to_vec(),
        }
    }

    /// The capture is the stdout verbatim; the parse beside it is exactly
    /// what `parse_mam` gives — the two are siblings, and `MamInfo` holds no
    /// raw text.
    #[test]
    fn a_successful_capture_keeps_stdout_verbatim_beside_the_parse() {
        let read =
            capture_from_output("/dev/sg9", "t".into(), None, Ok(output(0, LTO6_SAMPLE, "")));
        assert!(read.capture.ok());
        assert_eq!(read.error, None);
        assert_eq!(read.capture.stdout.as_deref(), Some(LTO6_SAMPLE.as_bytes()));
        assert_eq!(read.capture.tool_argv, vec!["sg_read_attr", "/dev/sg9"]);
        assert_eq!(read.info, parse_mam(LTO6_SAMPLE));
    }

    #[test]
    fn a_failed_capture_records_the_exit_and_stderr_and_parses_nothing() {
        let read = capture_from_output(
            "/dev/sg9",
            "t".into(),
            None,
            Ok(output(
                52,
                "",
                "open error: /dev/sg9: No such file or directory\n",
            )),
        );
        assert!(!read.capture.ok());
        let err = read.capture.error.as_deref().unwrap();
        assert!(err.contains("open error: /dev/sg9"), "{err}");
        assert_eq!(read.error.as_deref(), Some(err));
        assert_eq!(read.info, MamInfo::default());
        // Positive control: the same bytes with exit 0 do parse.
        assert!(
            capture_from_output("/dev/sg9", "t".into(), None, Ok(output(0, SAMPLE, "")))
                .info
                .serial
                .is_some()
        );
    }

    /// Units are read back out of the label, so a label spelling a
    /// different unit is recorded as that unit rather than missed (#182).
    #[test]
    fn journal_attributes_take_the_unit_from_the_label_as_printed() {
        let v = journal_attributes("  Maximum capacity in partition [MB]: 2500000\n");
        assert_eq!(v["max_capacity"]["value"], 2500000);
        assert_eq!(v["max_capacity"]["unit"], "MB");
        // `parse_mam` does not recognise that spelling — which is why the
        // journal must not depend on it.
        assert_eq!(
            parse_mam("  Maximum capacity in partition [MB]: 2500000\n").max_capacity_bytes,
            None
        );
        let empty = journal_attributes("Attribute values:\n  MAM space remaining [B]: 3062\n");
        assert_eq!(empty, serde_json::json!({}));
    }
}
