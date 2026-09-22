//! The drive as a first-class noun: who produced a reading (issue #295).
//!
//! `sg_logs` pages 0x02/0x03/0x2E are **drive-resident counters** — a
//! property of the machine, read through whichever cartridge happened to be
//! loaded. `health_logs` names only the volume, so the central question in
//! tape diagnostics — *is it the drive or the tape?* — has no column to
//! group by. ADR-0013 §1 answers it with a `drives` table every other record
//! takes a foreign key to, rather than a private drive column per table.
//!
//! This module is the identity half: the parsers that turn what the kernel
//! and sg3-utils already publish into a `serial`/`vendor`/`model`/`firmware`
//! tuple, and the upsert that records it.
//!
//! **Three independent routes to one identity**, in the order
//! [`read_identity`] consults them:
//!
//! 1. `/sys/class/scsi_tape/<node>/device/{vendor,model,rev}` and
//!    `.../vpd_pg80` — a plain file read the kernel already maintains. No
//!    SCSI command tapectl issues, no device open, and therefore no
//!    interference with the st driver's single-open rule that
//!    `volume_verify` already works around.
//! 2. `sg_inq --page=0x80` on the sg node — the fallback when `vpd_pg80` is
//!    absent (not every driver/drive publishes it; the real HP LTO-6's
//!    behaviour here is unverified, see `tests/fixtures/drive_identity/README.md`).
//! 3. The `sg_logs` identity header — the first line of every page's stdout,
//!    which `health::collect` has concatenated into `health_logs.raw_log` on
//!    every row ever written. It carries vendor/product/firmware (never the
//!    serial) and has been captured-but-unqueryable all along, exactly the
//!    `tape_alerts` shape of issue #107 and the ECC-parameter shape of #120.
//!    Used here only to fill a field sysfs did not yield, never to override
//!    one it did.
//!
//! **What is deliberately NOT an identity source.** MAM's `Density
//! vendor/serial number at last load` is the MEDIUM's lagging memory of a
//! drive, written by the cartridge, not a live read of the machine in front
//! of us. It is a genuine cross-check and belongs to the MAM journal
//! (ADR-0013 §5), never to the primary route.
//!
//! **The parse/IO split** is the one `parse_mam`/`read_mam`
//! ([`crate::tape::mam`]) and `parse_sg_logs_page`/`collect`
//! ([`crate::tape::health`]) already use: every parser here is pure and is
//! unit-tested from a recorded fixture string or byte slice, so no ungated
//! test opens a device node or reads `/sys`.
//!
//! **Identity capture is best-effort and non-fatal**, like the health
//! collection it rides on. A drive that yields no serial produces no row —
//! it is recorded as unknown by its absence, never as a fabricated or
//! inferred identity. That is the same honesty migration 009 gave
//! `tape_alerts`: NULL means "not recorded", and nothing backfills a guess.

use std::path::{Path, PathBuf};
use std::process::Command;

use rusqlite::{params, Connection};

use crate::config::LtoBackendConfig;
use crate::error::Result;

/// Where the kernel publishes per-device SCSI identity.
const SCSI_TAPE_SYSFS_ROOT: &str = "/sys/class/scsi_tape";

/// What one contact learned about the drive it talked to.
///
/// Every field is `Option` for the reason the module doc gives: an
/// unidentifiable drive is recorded as unknown, never guessed. `serial` is
/// the load-bearing one — ADR-0013 §1 keys the row on it, so `None` there
/// means no row at all (see [`upsert`]).
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct DriveIdentity {
    /// SCSI Unit Serial Number (VPD page 0x80). The drive's identity, and
    /// the same string the by-id name carries: `scsi-XYZZY_A1-nst` is serial
    /// `XYZZY_A1`, which is why `CLAUDE.md` can tell an operator to resolve
    /// the real LTO-6 as `scsi-HUJ808A5L4-nst`.
    pub serial: Option<String>,
    pub vendor: Option<String>,
    pub model: Option<String>,
    /// Firmware revision **as observed on this contact**. It legitimately
    /// changes over a drive's life, and a firmware upgrade is exactly the
    /// kind of event a later error-rate change must be correlated against.
    pub firmware_rev: Option<String>,
}

impl DriveIdentity {
    /// Fill only the fields this identity does not already have from the
    /// `sg_logs` identity header (route 3 in the module doc).
    ///
    /// Never overrides a value sysfs yielded: sysfs is the kernel's own
    /// record of the INQUIRY response, the header is sg3-utils' rendering of
    /// it, and when they disagree that disagreement is a finding rather than
    /// something to silently resolve by last-writer-wins.
    pub fn backfill_from_sg_logs_header(&mut self, raw_log: &str) {
        let Some(header) = parse_sg_logs_identity_header(raw_log) else {
            return;
        };
        if self.vendor.is_none() {
            self.vendor = header.vendor;
        }
        if self.model.is_none() {
            self.model = header.model;
        }
        if self.firmware_rev.is_none() {
            self.firmware_rev = header.firmware_rev;
        }
    }
}

// ── Pure parsers ──────────────────────────────────────────────────────────

/// Parse the concatenated `vendor`/`model`/`rev` sysfs triple, in that
/// order — literally what `cat /sys/class/scsi_tape/nstN/device/{vendor,model,rev}`
/// prints, which is how the fixture was recorded.
///
/// Requires **exactly three lines**. Position is the only thing that
/// distinguishes the three fields, so a short read must not let `rev` land
/// in the model slot — [`read_identity`] therefore substitutes an empty line
/// for a file it could not read rather than dropping it.
///
/// Trimming is part of the parse, not an assumption: the kernel returns the
/// SCSI fixed-width fields with their space padding intact (`IBM     `,
/// `ULT3580-TD8     `), and the fixture preserves that padding so the trim is
/// tested. A field that is empty after trimming stays `None`; all three empty
/// yields `None` for the whole triple, since nothing was learned.
pub fn parse_sysfs_triple(raw: &str) -> Option<DriveIdentity> {
    let mut lines: Vec<&str> = raw.split('\n').collect();
    // A trailing newline produces one empty final element; the file's three
    // lines are what matter, not whether it ended with one.
    if lines.last() == Some(&"") {
        lines.pop();
    }
    if lines.len() != 3 {
        return None;
    }
    let field = |s: &str| {
        let t = s.trim();
        (!t.is_empty()).then(|| t.to_string())
    };
    let identity = DriveIdentity {
        serial: None,
        vendor: field(lines[0]),
        model: field(lines[1]),
        firmware_rev: field(lines[2]),
    };
    if identity == DriveIdentity::default() {
        return None;
    }
    Some(identity)
}

/// Parse a raw SCSI VPD page 0x80 (Unit Serial Number) into the serial.
///
/// Layout: byte 0 peripheral qualifier/device type, byte 1 page code (must
/// be `0x80`), bytes 2-3 page length big-endian, then that many bytes of
/// serial, space-padded.
///
/// **Every bound is checked and nothing panics.** These are bytes from a
/// device: a truncated read, a driver that published a different page, or a
/// length field that overruns the buffer must all return `None` so the
/// caller records nothing, which is the same discipline `RESTORE.sh`'s
/// `require_uint` applies to the numbers it reads off tape. `None` is also
/// the answer for a zero-length page and for a serial that is all padding —
/// both are "the drive declined to say", not an identity.
pub fn parse_vpd_page80(bytes: &[u8]) -> Option<String> {
    if bytes.len() < 4 {
        return None;
    }
    if bytes[1] != 0x80 {
        return None;
    }
    let len = u16::from_be_bytes([bytes[2], bytes[3]]) as usize;
    // `.get(..)` rather than an index: a declared length longer than the
    // buffer is exactly the malformed case, and it must not read past the
    // end.
    let serial = bytes.get(4..4 + len)?;
    let serial = String::from_utf8_lossy(serial);
    let serial = serial.trim();
    (!serial.is_empty()).then(|| serial.to_string())
}

/// Parse `sg_inq --page=0x80` output — the fallback route to the serial.
///
/// sg3-utils prints a `Unit serial number: <value>` line under its page
/// banner. Anything else yields `None`.
pub fn parse_sg_inq_serial(raw: &str) -> Option<String> {
    for line in raw.lines() {
        let line = line.trim();
        // The page banner ("VPD INQUIRY: Unit serial number page") contains
        // the same words but no colon-delimited value, so match on the
        // labelled form only.
        if let Some(value) = line.strip_prefix("Unit serial number:") {
            let value = value.trim();
            if !value.is_empty() {
                return Some(value.to_string());
            }
        }
    }
    None
}

/// Parse the `sg_logs` identity header — the first line of every page's
/// stdout, and therefore a line already present in every `health_logs.raw_log`
/// ever written.
///
/// Fields are **whitespace-run delimited, not fixed-column**: sg_logs pads
/// each SCSI field to its width (vendor 8, product 16) and joins them with
/// two spaces, so the separator is always a run of two or more spaces while
/// a single space is part of a field. The two recorded drives prove the
/// widths differ per drive and that a model can contain a space —
/// `IBM       ULT3580-TD8       2160` versus
/// `HP        Ultrium 6-SCSI    35GD` — which is why splitting on columns
/// would get `Ultrium 6-SCSI` wrong.
///
/// Only the first content line is considered, and only if it has exactly
/// three fields. A page body line such as `Write error counter page  [0x2]`
/// also splits into fields, and mistaking it for an identity would invent a
/// drive named "Write error counter page". Lines skipped before the
/// candidate: blanks, `collect()`'s own `=== page 0xNN ===` markers, and a
/// recorded shell echo (`$ sg_logs ...`), which is how the newer fixtures
/// were captured.
pub fn parse_sg_logs_identity_header(raw: &str) -> Option<DriveIdentity> {
    let candidate = raw.lines().find(|line| {
        let t = line.trim();
        !t.is_empty() && !t.starts_with("=== page") && !t.starts_with("$ ")
    })?;
    let fields: Vec<&str> = candidate
        .split("  ")
        .map(str::trim)
        .filter(|f| !f.is_empty())
        .collect();
    if fields.len() != 3 {
        return None;
    }
    Some(DriveIdentity {
        serial: None,
        vendor: Some(fields[0].to_string()),
        model: Some(fields[1].to_string()),
        firmware_rev: Some(fields[2].to_string()),
    })
}

// ── The thin I/O caller ───────────────────────────────────────────────────

/// Read this drive's identity, best-effort, from the backend **the caller
/// already holds**.
///
/// Taking the resolved `LtoBackendConfig` rather than a device string is the
/// whole point: issue #187 was this exact code path resolving the same
/// backend twice — once canonicalised, once by raw string — and silently
/// recording nothing when the two spellings disagreed. There is no lookup
/// here to disagree with anything.
///
/// Never fails: an unreadable sysfs node, a missing `vpd_pg80` and an absent
/// `sg_inq` all just leave fields `None`, and a `DriveIdentity` with no
/// serial records no row ([`upsert`]).
pub fn read_identity(backend: &LtoBackendConfig) -> DriveIdentity {
    let dir = sysfs_device_dir(&backend.device_tape);
    let mut identity = dir
        .as_deref()
        .and_then(read_sysfs_triple)
        .unwrap_or_default();

    identity.serial = dir
        .as_deref()
        .and_then(|d| std::fs::read(d.join("vpd_pg80")).ok())
        .as_deref()
        .and_then(parse_vpd_page80)
        .or_else(|| read_sg_inq_serial(&backend.device_sg));

    identity
}

/// The kernel's sysfs directory for the tape node `device_tape` names.
///
/// Canonicalises first so a by-id symlink — the RECOMMENDED form, per the
/// device-numbering hazard — resolves to the `/dev/nstN` whose basename
/// sysfs is keyed by, the same canonicalise-with-string-fallback dance
/// [`crate::config::device_matches`] already does. A canonicalize error is
/// not propagated: it falls back to the path's own basename, which is
/// correct whenever `device_tape` is already a plain node.
fn sysfs_device_dir(device_tape: &str) -> Option<PathBuf> {
    let canonical = std::fs::canonicalize(device_tape);
    let node = canonical
        .as_deref()
        .unwrap_or_else(|_| Path::new(device_tape))
        .file_name()?
        .to_str()?
        .to_string();
    Some(sysfs_device_dir_for_node(&node))
}

/// Build the sysfs device directory for a tape node name (`nst1`).
/// Split out from [`sysfs_device_dir`] so the path shape is testable
/// without a device node or a `/sys` read.
fn sysfs_device_dir_for_node(node: &str) -> PathBuf {
    Path::new(SCSI_TAPE_SYSFS_ROOT).join(node).join("device")
}

/// Read and parse the three sysfs identity files.
///
/// A file that cannot be read contributes an empty line rather than being
/// dropped — [`parse_sysfs_triple`] identifies its fields by position, so a
/// missing `model` must leave a hole, never shift `rev` into its place.
fn read_sysfs_triple(dir: &Path) -> Option<DriveIdentity> {
    let read = |name: &str| std::fs::read_to_string(dir.join(name)).unwrap_or_default();
    // The TRAILING newline is load-bearing: `parse_sysfs_triple` pops one
    // empty final element as the file's own trailing-newline artifact, so
    // without it an unreadable `rev` -- the LAST slot -- would be popped as
    // that artifact and collapse the triple to two lines, discarding the
    // vendor and model this read did get.
    let raw = format!(
        "{}\n{}\n{}\n",
        read("vendor").trim_end_matches('\n'),
        read("model").trim_end_matches('\n'),
        read("rev").trim_end_matches('\n'),
    );
    parse_sysfs_triple(&raw)
}

/// Fallback serial read: shell out to `sg_inq --page=0x80`.
fn read_sg_inq_serial(sg_device: &str) -> Option<String> {
    let output = Command::new("sg_inq")
        .arg("--page=0x80")
        .arg(sg_device)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    parse_sg_inq_serial(&String::from_utf8_lossy(&output.stdout))
}

// ── The record ────────────────────────────────────────────────────────────

/// Record this contact with the drive, keyed on serial: insert on first
/// sight, refresh on every later one. Returns the `drives.id`, which is what
/// migration 020's `cartridge_contacts` will take its foreign key from
/// (ADR-0013 §1).
///
/// **No serial means no row, and `Ok(None)`.** Not a composite key invented
/// from vendor + model + device path: two identical drives on one host would
/// collide under such a key and silently merge their histories, and a key
/// containing the device path would re-attribute every historical row the
/// day `/dev/nst0` and `/dev/nst1` swap across a reboot — which this VM's
/// own device-numbering hazard says is routine. An unidentifiable drive is
/// unknown, and unknown is recorded by absence.
///
/// The refresh uses `COALESCE(excluded.x, drives.x)` for the three
/// descriptive columns: a contact that yields a serial but no sysfs triple
/// (the `sg_inq` fallback path) must not erase what an earlier, fuller
/// contact recorded. A genuinely CHANGED value still lands — `excluded` is
/// non-NULL then — which is the point of keeping firmware here at all.
/// `first_seen` is never in the SET list; it is the one column that must not
/// move.
pub fn upsert(conn: &Connection, identity: &DriveIdentity) -> Result<Option<i64>> {
    let Some(serial) = identity.serial.as_deref() else {
        return Ok(None);
    };
    conn.execute(
        "INSERT INTO drives (serial, vendor, model, firmware_rev)
         VALUES (?1, ?2, ?3, ?4)
         ON CONFLICT(serial) DO UPDATE SET
             vendor       = COALESCE(excluded.vendor, drives.vendor),
             model        = COALESCE(excluded.model, drives.model),
             firmware_rev = COALESCE(excluded.firmware_rev, drives.firmware_rev),
             last_seen    = datetime('now')",
        params![
            serial,
            identity.vendor,
            identity.model,
            identity.firmware_rev
        ],
    )?;
    let id: i64 = conn.query_row(
        "SELECT id FROM drives WHERE serial = ?1",
        params![serial],
        |row| row.get(0),
    )?;
    Ok(Some(id))
}

#[cfg(test)]
mod tests {
    use super::*;

    // Captured 2026-09-22 from this VM's mhvtl drive at /dev/nst1 (/dev/sg1,
    // by-id `scsi-XYZZY_A1-nst`) — all three describe the SAME drive at the
    // same moment, which is what makes the agreement test below meaningful.
    // Provenance: tests/fixtures/drive_identity/README.md.
    const SYSFS: &str = include_str!("../../tests/fixtures/drive_identity/mhvtl_nst1_sysfs.txt");
    const VPD_PG80_HEX: &str =
        include_str!("../../tests/fixtures/drive_identity/mhvtl_nst1_vpd_pg80.hex");
    const SG_INQ: &str =
        include_str!("../../tests/fixtures/drive_identity/mhvtl_nst1_sg_inq_page80.txt");
    /// Same drive, same moment — the sg_logs capture whose line 2 is the
    /// identity header `collect()` already concatenates into `raw_log`.
    const MHVTL_PAGE_02: &str =
        include_str!("../../tests/fixtures/sg_logs/mhvtl_ibm_td8_page_0x02.txt");
    /// A REAL HP LTO-6, recorded in a different session. Used for header
    /// parsing only — never cross-checked against the mhvtl fixtures above,
    /// which would assert that two different drives are one drive.
    const HP_PAGE_02: &str = include_str!("../../tests/fixtures/sg_logs/hp_lto6_page_0x02.txt");
    /// The older mhvtl capture, whose first line IS the identity header (no
    /// recorded shell echo) — the shape a real `raw_log` has.
    const PAGE_02_NO_ECHO: &str = include_str!("../../tests/fixtures/sg_logs/page_0x02.txt");

    /// Decode the one-line hex the `vpd_pg80` fixture is stored as (it was
    /// captured with `xxd -p`). Test-only: production reads the raw bytes.
    fn hex_to_bytes(hex: &str) -> Vec<u8> {
        let hex: String = hex.chars().filter(|c| !c.is_whitespace()).collect();
        (0..hex.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&hex[i..i + 2], 16).unwrap())
            .collect()
    }

    // ── sysfs triple ──────────────────────────────────────────────────

    #[test]
    fn sysfs_triple_parses_and_trims_the_scsi_padding() {
        let id = parse_sysfs_triple(SYSFS).expect("the recorded triple must parse");
        // Positive controls: the exact recorded values, padding removed.
        assert_eq!(id.vendor.as_deref(), Some("IBM"));
        assert_eq!(id.model.as_deref(), Some("ULT3580-TD8"));
        assert_eq!(id.firmware_rev.as_deref(), Some("2160"));
        assert_eq!(id.serial, None, "the triple never carries a serial");
    }

    #[test]
    fn sysfs_fixture_still_contains_the_padding_the_parse_must_trim() {
        // Guards the fixture itself: if the padding is ever normalised away,
        // the trim above would pass without testing anything.
        assert!(
            SYSFS.contains("IBM     \n"),
            "fixture must preserve the SCSI space padding"
        );
    }

    #[test]
    fn sysfs_triple_of_wrong_line_count_is_none() {
        // A short read must not let `rev` land in the model slot.
        assert_eq!(parse_sysfs_triple("IBM     \nULT3580-TD8     \n"), None);
        assert_eq!(parse_sysfs_triple(""), None);
        assert_eq!(parse_sysfs_triple("IBM\nULT\n2160\nextra\n"), None);
    }

    /// The LAST slot is the one that breaks if the caller's empty-line
    /// substitution and the parser's trailing-newline strip collide: an
    /// unreadable `rev` must leave a hole, not discard the vendor and model
    /// that WERE read. This is the exact string `read_sysfs_triple` builds
    /// in that case.
    #[test]
    fn sysfs_triple_keeps_a_hole_when_the_last_field_was_unreadable() {
        let id = parse_sysfs_triple("IBM\nULT3580-TD8\n\n")
            .expect("an unreadable rev must not discard vendor and model");
        assert_eq!(id.vendor.as_deref(), Some("IBM"));
        assert_eq!(id.model.as_deref(), Some("ULT3580-TD8"));
        assert_eq!(id.firmware_rev, None);
    }

    #[test]
    fn sysfs_triple_of_three_empty_lines_is_none() {
        assert_eq!(parse_sysfs_triple("\n\n\n"), None);
    }

    #[test]
    fn sysfs_triple_keeps_a_hole_where_a_field_was_unreadable() {
        let id = parse_sysfs_triple("IBM\n\n2160\n").expect("two fields is still an identity");
        assert_eq!(id.vendor.as_deref(), Some("IBM"));
        assert_eq!(id.model, None);
        assert_eq!(
            id.firmware_rev.as_deref(),
            Some("2160"),
            "rev must stay in the rev slot, not shift into model"
        );
    }

    // ── VPD page 0x80 ─────────────────────────────────────────────────

    #[test]
    fn vpd_page80_parses_the_recorded_serial() {
        let bytes = hex_to_bytes(VPD_PG80_HEX);
        // Positive control: the exact serial, not merely "some serial".
        assert_eq!(parse_vpd_page80(&bytes).as_deref(), Some("XYZZY_A1"));
    }

    #[test]
    fn vpd_page80_exact_length_buffer_parses_without_overreading() {
        let bytes = hex_to_bytes(VPD_PG80_HEX);
        assert_eq!(bytes.len(), 4 + 10, "fixture is header + a 10-byte serial");
        assert_eq!(parse_vpd_page80(&bytes).as_deref(), Some("XYZZY_A1"));
    }

    #[test]
    fn vpd_page80_short_buffer_is_none_not_a_panic() {
        assert_eq!(parse_vpd_page80(&[]), None);
        assert_eq!(parse_vpd_page80(&[0x01, 0x80, 0x00]), None);
    }

    #[test]
    fn vpd_page80_wrong_page_code_is_none() {
        // 0x83 is Device Identification, a different page entirely; its
        // payload is not a serial and must never be recorded as one.
        let mut bytes = hex_to_bytes(VPD_PG80_HEX);
        bytes[1] = 0x83;
        assert_eq!(parse_vpd_page80(&bytes), None);
    }

    #[test]
    fn vpd_page80_length_overrunning_the_buffer_is_none_not_a_panic() {
        let mut bytes = hex_to_bytes(VPD_PG80_HEX);
        bytes[2] = 0x00;
        bytes[3] = 0x14; // declares 20 bytes; 10 are present
        assert_eq!(parse_vpd_page80(&bytes), None);
    }

    #[test]
    fn vpd_page80_zero_length_or_all_padding_is_none() {
        assert_eq!(parse_vpd_page80(&[0x01, 0x80, 0x00, 0x00]), None);
        assert_eq!(
            parse_vpd_page80(&[0x01, 0x80, 0x00, 0x04, b' ', b' ', b' ', b' ']),
            None,
            "a serial that is all padding is the drive declining to say"
        );
    }

    // ── sg_inq fallback ───────────────────────────────────────────────

    #[test]
    fn sg_inq_parses_the_recorded_serial() {
        assert_eq!(parse_sg_inq_serial(SG_INQ).as_deref(), Some("XYZZY_A1"));
    }

    #[test]
    fn sg_inq_without_a_serial_line_is_none() {
        assert_eq!(
            parse_sg_inq_serial("VPD INQUIRY: Unit serial number page\n"),
            None,
            "the banner names the page but carries no value"
        );
        assert_eq!(parse_sg_inq_serial(""), None);
    }

    // ── sg_logs identity header ───────────────────────────────────────

    #[test]
    fn sg_logs_header_parses_the_mhvtl_drive() {
        let id = parse_sg_logs_identity_header(MHVTL_PAGE_02).expect("header must parse");
        assert_eq!(id.vendor.as_deref(), Some("IBM"));
        assert_eq!(id.model.as_deref(), Some("ULT3580-TD8"));
        assert_eq!(id.firmware_rev.as_deref(), Some("2160"));
    }

    #[test]
    fn sg_logs_header_parses_a_real_hp_lto6_with_a_space_in_its_model() {
        // Different field widths AND a single space inside the model — the
        // reason this parses by whitespace runs rather than fixed columns.
        let id = parse_sg_logs_identity_header(HP_PAGE_02).expect("header must parse");
        assert_eq!(id.vendor.as_deref(), Some("HP"));
        assert_eq!(id.model.as_deref(), Some("Ultrium 6-SCSI"));
        assert_eq!(id.firmware_rev.as_deref(), Some("35GD"));
    }

    #[test]
    fn sg_logs_header_parses_from_a_raw_log_shaped_blob() {
        // The production shape: `collect()` writes its own page marker and
        // then the page's stdout, whose first line is the header.
        let raw_log = format!("=== page 0x02 ===\n{PAGE_02_NO_ECHO}");
        let id = parse_sg_logs_identity_header(&raw_log).expect("header must parse");
        assert_eq!(id.vendor.as_deref(), Some("IBM"));
        assert_eq!(id.firmware_rev.as_deref(), Some("2160"));
    }

    #[test]
    fn sg_logs_body_without_a_header_is_none_not_an_invented_drive() {
        // Negative control with a positive twin above: the same page text
        // minus its identity line must yield nothing, rather than reading
        // "Write error counter page" as a vendor.
        let body: String = PAGE_02_NO_ECHO
            .lines()
            .skip(1)
            .map(|l| format!("{l}\n"))
            .collect();
        assert!(
            body.contains("Write error counter page"),
            "positive control: the body we are feeding is real page text"
        );
        assert_eq!(parse_sg_logs_identity_header(&body), None);
    }

    #[test]
    fn backfill_never_overrides_what_sysfs_already_said() {
        let mut id = parse_sysfs_triple(SYSFS).unwrap();
        id.firmware_rev = None; // as if `rev` were unreadable
        id.backfill_from_sg_logs_header(HP_PAGE_02);
        assert_eq!(
            id.vendor.as_deref(),
            Some("IBM"),
            "sysfs wins over the header"
        );
        assert_eq!(id.model.as_deref(), Some("ULT3580-TD8"));
        assert_eq!(
            id.firmware_rev.as_deref(),
            Some("35GD"),
            "only the missing field is filled"
        );
    }

    // ── The agreement test (issue #295's acceptance) ──────────────────

    /// The sysfs triple, the `vpd_pg80` serial, the `sg_inq` fallback and
    /// the `sg_logs` header were all captured from ONE drive at one moment.
    /// Three independent routes to one identity; their disagreement would
    /// itself be a finding.
    ///
    /// Deliberately does NOT involve `hp_lto6_page_0x02.txt`: that is a
    /// different drive (HP/Ultrium 6-SCSI vs IBM/ULT3580-TD8), and asserting
    /// those agree would assert something false.
    #[test]
    fn every_identity_route_describes_the_same_mhvtl_drive() {
        let sysfs = parse_sysfs_triple(SYSFS).expect("sysfs triple");
        let header = parse_sg_logs_identity_header(MHVTL_PAGE_02).expect("sg_logs header");
        assert_eq!(sysfs.vendor, header.vendor);
        assert_eq!(sysfs.model, header.model);
        assert_eq!(sysfs.firmware_rev, header.firmware_rev);

        let from_vpd = parse_vpd_page80(&hex_to_bytes(VPD_PG80_HEX)).expect("vpd_pg80 serial");
        let from_sg_inq = parse_sg_inq_serial(SG_INQ).expect("sg_inq serial");
        assert_eq!(from_vpd, from_sg_inq);
        assert_eq!(from_vpd, "XYZZY_A1");

        // And the by-id name the operator resolves the drive by carries that
        // same serial verbatim — the third route, per the fixture README.
        assert_eq!(format!("scsi-{from_vpd}-nst"), "scsi-XYZZY_A1-nst");
    }

    // ── sysfs path shape (no `/sys` read, no device node) ─────────────

    #[test]
    fn sysfs_device_dir_for_node_is_the_kernels_layout() {
        assert_eq!(
            sysfs_device_dir_for_node("nst1"),
            Path::new("/sys/class/scsi_tape/nst1/device")
        );
    }

    #[test]
    fn sysfs_device_dir_falls_back_to_the_basename_when_canonicalize_fails() {
        // A path that cannot exist: canonicalize errors and the basename is
        // used, which is correct whenever `device_tape` is already a plain
        // node. Touches no device and no `/sys`.
        assert_eq!(
            sysfs_device_dir("/dev/nst-no-such-node"),
            Some(PathBuf::from(
                "/sys/class/scsi_tape/nst-no-such-node/device"
            ))
        );
    }

    // ── The record ────────────────────────────────────────────────────

    fn identity(serial: Option<&str>, firmware: &str) -> DriveIdentity {
        DriveIdentity {
            serial: serial.map(str::to_string),
            vendor: Some("IBM".into()),
            model: Some("ULT3580-TD8".into()),
            firmware_rev: Some(firmware.into()),
        }
    }

    #[test]
    fn migration_019_applies_from_001_forward_and_fsck_passes() {
        // `open_memory` runs the full ordered migration chain.
        let conn = crate::db::open_memory().unwrap();
        let count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = 'drives'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(count, 1, "migration 019 must create the drives table");

        let report = crate::cli::operations::db_fsck(&conn, false, false).unwrap();
        assert!(report.integrity_ok, "integrity_check after 019");
        assert!(
            report.issues.is_empty(),
            "db fsck must be clean after 019: {:?}",
            report.issues
        );
    }

    #[test]
    fn migration_019_adds_no_column_to_health_logs() {
        // ADR-0013 §3 gives `health_logs` exactly ONE rebuild, and it is
        // migration 021 (issue #296). 019 must leave it byte-identical to
        // what 009 left behind — a drive column here would be the second of
        // the four uncoordinated rebuilds the ADR exists to prevent.
        //
        // Migrated to exactly 019, not "latest": 021 IS that one rebuild and
        // legitimately adds `contact_id`/`tapectl_version`, so a pin against
        // the newest schema would be measuring 021, not 019.
        let conn = crate::db::open_memory_at_version(19);
        let version: i64 = conn
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(version, 19, "positive control: this pin is about 019");
        let mut stmt = conn.prepare("PRAGMA table_info(health_logs)").unwrap();
        let columns: Vec<String> = stmt
            .query_map([], |r| r.get::<_, String>(1))
            .unwrap()
            .map(|c| c.unwrap())
            .collect();
        assert_eq!(
            columns,
            vec![
                "id",
                "volume_id",
                "session_id",
                "logged_at",
                "operation",
                "total_bytes",
                "total_uncorrected",
                "total_corrected",
                "total_retries",
                "total_rewritten",
                "raw_log",
                "tape_alerts",
            ],
            "health_logs must carry exactly 001's columns plus 009's tape_alerts"
        );
    }

    #[test]
    fn upsert_records_the_drive_on_first_sight() {
        let conn = crate::db::open_memory().unwrap();
        let id = upsert(&conn, &identity(Some("XYZZY_A1"), "2160"))
            .unwrap()
            .expect("a serial must produce a row");

        let (serial, vendor, model, firmware): (String, String, String, String) = conn
            .query_row(
                "SELECT serial, vendor, model, firmware_rev FROM drives WHERE id = ?1",
                params![id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .unwrap();
        // Positive control: the recorded serial, not merely a non-NULL one.
        assert_eq!(serial, "XYZZY_A1");
        assert_eq!(vendor, "IBM");
        assert_eq!(model, "ULT3580-TD8");
        assert_eq!(firmware, "2160");
    }

    #[test]
    fn upsert_on_a_second_sighting_updates_firmware_and_keeps_first_seen() {
        let conn = crate::db::open_memory().unwrap();
        let first = upsert(&conn, &identity(Some("XYZZY_A1"), "2160"))
            .unwrap()
            .unwrap();
        let first_seen: String = conn
            .query_row(
                "SELECT first_seen FROM drives WHERE id = ?1",
                params![first],
                |r| r.get(0),
            )
            .unwrap();

        // A firmware upgrade is exactly the event a later error-rate change
        // must be correlated against, so the new value must land.
        let second = upsert(&conn, &identity(Some("XYZZY_A1"), "2170"))
            .unwrap()
            .unwrap();
        assert_eq!(second, first, "one serial is one drive, not two rows");

        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM drives", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 1);

        let (firmware, still_first_seen): (String, String) = conn
            .query_row(
                "SELECT firmware_rev, first_seen FROM drives WHERE id = ?1",
                params![first],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(firmware, "2170");
        assert_eq!(
            still_first_seen, first_seen,
            "first_seen is the one column a later sighting must not move"
        );
    }

    #[test]
    fn upsert_never_erases_a_field_a_later_contact_could_not_read() {
        // The `sg_inq` fallback path yields a serial and nothing else. That
        // must not blank what a fuller contact already recorded.
        let conn = crate::db::open_memory().unwrap();
        upsert(&conn, &identity(Some("XYZZY_A1"), "2160")).unwrap();
        upsert(
            &conn,
            &DriveIdentity {
                serial: Some("XYZZY_A1".into()),
                ..Default::default()
            },
        )
        .unwrap();

        let (vendor, firmware): (Option<String>, Option<String>) = conn
            .query_row(
                "SELECT vendor, firmware_rev FROM drives WHERE serial = 'XYZZY_A1'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(vendor.as_deref(), Some("IBM"));
        assert_eq!(firmware.as_deref(), Some("2160"));
    }

    #[test]
    fn two_serials_are_two_drives() {
        let conn = crate::db::open_memory().unwrap();
        let a = upsert(&conn, &identity(Some("XYZZY_A1"), "2160"))
            .unwrap()
            .unwrap();
        let b = upsert(&conn, &identity(Some("HUJ808A5L4"), "35GD"))
            .unwrap()
            .unwrap();
        assert_ne!(a, b);
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM drives", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 2);
    }

    /// The paired negative for `upsert_records_the_drive_on_first_sight`:
    /// a drive that yields no serial records NO row — never a row keyed on
    /// a guess — **and the health row is still written**. Health collection
    /// must not become conditional on identity succeeding.
    #[test]
    fn a_drive_with_no_serial_records_no_drive_row_and_still_writes_its_health_row() {
        let conn = crate::db::open_memory().unwrap();
        conn.execute(
            "INSERT INTO volumes (label, backend_type, backend_name, media_type, capacity_bytes, status)
             VALUES ('V-NOSERIAL', 'lto', 'lto0', 'LTO-6', 2500000000000, 'active')",
            [],
        )
        .unwrap();
        let vid = conn.last_insert_rowid();

        // Vendor and model are known; the serial is not. ADR-0013 §1: no
        // serial, no row.
        let recorded = upsert(&conn, &identity(None, "2160")).unwrap();
        assert_eq!(recorded, None);
        let drives: i64 = conn
            .query_row("SELECT COUNT(*) FROM drives", [], |r| r.get(0))
            .unwrap();
        assert_eq!(drives, 0, "an unidentified drive is unknown by absence");

        crate::tape::health::record(
            &conn,
            Some(vid),
            None,
            None,
            crate::tape::health::Reading::Verify,
            &crate::tape::health::HealthCounters::default(),
            "raw",
        )
        .unwrap();
        let rows: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM health_logs WHERE volume_id = ?1",
                params![vid],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            rows, 1,
            "the reading is still worth recording without an identified drive"
        );
    }

    /// Migration 019 must not backfill. Pre-019 `health_logs` rows cannot be
    /// attributed to any drive — the machine that produced them was never
    /// recorded — and a guess would read exactly like an observation.
    #[test]
    fn migration_019_leaves_the_drives_table_empty() {
        let conn = crate::db::open_memory().unwrap();
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM drives", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 0);
    }
}
