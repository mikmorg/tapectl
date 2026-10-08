//! TapeAlert (log page 0x2E) decoded from the journalled bytes, by flag
//! code, with what each raised flag tells the operator to do (issue #303).
//!
//! # Codes from the bytes, not from the decode's line order
//!
//! A TapeAlert parameter's code IS its flag number (SSC-3, T10/1611-D,
//! TapeAlert log page: parameter codes 0001h..0040h, one flag each), and it
//! is the only identity a flag has that does not depend on a tool. Until
//! this module, a flag's number was the ordinal of its line in sg_logs'
//! text decode (`report::tape_alert_flag_lines`) — right as long as the
//! drive returns every parameter in order and sg_logs prints one line per
//! parameter, wrong the first time either does not. The journal has kept
//! the page's raw bytes since migration 023 (`log_page_journal.raw`, one
//! read per contact, ADR-0013), so the codes are read from there:
//! [`flags_from_page`]. Nothing here touches a device.
//!
//! # Three verdicts, not one count
//!
//! A count (`health_logs.tape_alerts`) puts "load a cleaning cartridge" and
//! "this cartridge is dying" in one number. [`class_of`] sorts each flag
//! into what it asks of the operator:
//!
//! - **cleaning** — clean the drive; nothing is wrong with the data;
//! - **medium** — the cartridge is failing: copy its data to another
//!   cartridge, then retire it;
//! - **drive** — the drive is failing: stop writing; every tape written
//!   since is suspect;
//! - **other** — the flag does not say which (a read or write failure, a
//!   hard error), or is not a health verdict at all (write protect).
//!
//! The flag numbers and names are SSC-3's TapeAlert table (T10/1611-D);
//! the grouping is tapectl's, by what each flag names, as #303 laid it out —
//! the standard does not group them. [`tests`] pins every classified code's
//! name against sg_logs' own decode of a real mhvtl page, so the numbering
//! cannot drift from the decode the journal also holds.

/// Log page 0x2E.
pub const PAGE: u8 = 0x2e;

/// What a raised flag asks of the operator.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum AlertClass {
    /// The drive is failing.
    Drive,
    /// The cartridge is failing.
    Medium,
    /// The drive wants cleaning.
    Cleaning,
    /// Not said, or not a health verdict.
    Other,
}

impl AlertClass {
    /// The stable name (`--json`).
    pub fn as_str(self) -> &'static str {
        match self {
            AlertClass::Drive => "drive",
            AlertClass::Medium => "medium",
            AlertClass::Cleaning => "cleaning",
            AlertClass::Other => "other",
        }
    }

    /// What the operator does about it, in one phrase.
    pub fn verdict(self) -> &'static str {
        match self {
            AlertClass::Drive => {
                "THE DRIVE is failing: stop writing; every tape written since is suspect"
            }
            AlertClass::Medium => {
                "THE CARTRIDGE is failing: copy its data to another cartridge, then retire it"
            }
            AlertClass::Cleaning => "clean the drive: nothing is wrong with the data",
            AlertClass::Other => "the flag does not say whether the drive or the cartridge",
        }
    }
}

/// The SSC-3 TapeAlert flags tapectl classifies, `(code, name, class)`.
/// Codes not listed are [`AlertClass::Other`].
const CLASSIFIED: &[(u16, &str, AlertClass)] = &[
    (0x04, "Media", AlertClass::Medium),
    (0x07, "Media life", AlertClass::Medium),
    (0x08, "Not data grade", AlertClass::Medium),
    (
        0x0d,
        "Recoverable mechanical cartridge failure",
        AlertClass::Medium,
    ),
    (
        0x0e,
        "Unrecoverable mechanical cartridge failure",
        AlertClass::Medium,
    ),
    (0x0f, "Memory chip in cartridge failure", AlertClass::Medium),
    (0x12, "Tape directory corrupted on load", AlertClass::Medium),
    (0x13, "Nearing media life", AlertClass::Medium),
    (0x14, "Cleaning required", AlertClass::Cleaning),
    (0x15, "Cleaning requested", AlertClass::Cleaning),
    (0x16, "Expired cleaning media", AlertClass::Cleaning),
    (0x17, "Invalid cleaning tape", AlertClass::Cleaning),
    (0x1a, "Cooling fan failing", AlertClass::Drive),
    (0x1b, "Power supply failure", AlertClass::Drive),
    (0x1c, "Power consumption", AlertClass::Drive),
    (0x1d, "Drive maintenance", AlertClass::Drive),
    (0x1e, "Hardware A", AlertClass::Drive),
    (0x1f, "Hardware B", AlertClass::Drive),
    (0x20, "Interface", AlertClass::Drive),
    (0x22, "Microcode update fail", AlertClass::Drive),
    (0x23, "Drive humidity", AlertClass::Drive),
    (0x24, "Drive temperature", AlertClass::Drive),
    (0x25, "Drive voltage", AlertClass::Drive),
    (0x26, "Predictive failure", AlertClass::Drive),
    (0x27, "Diagnostics required", AlertClass::Drive),
    (0x33, "Tape directory invalid at unload", AlertClass::Medium),
    (0x34, "Tape system area write failure", AlertClass::Medium),
    (0x35, "Tape system area read failure", AlertClass::Medium),
    (0x3a, "Firmware failure", AlertClass::Drive),
    (
        0x3b,
        "WORM medium - integrity check failed",
        AlertClass::Medium,
    ),
];

/// What flag `code` asks of the operator.
pub fn class_of(code: u16) -> AlertClass {
    CLASSIFIED
        .iter()
        .find(|(c, _, _)| *c == code)
        .map(|(_, _, class)| *class)
        .unwrap_or(AlertClass::Other)
}

/// SSC-3's name for a classified flag; `None` for the rest (the decode's own
/// name is used for those).
pub fn ssc3_name(code: u16) -> Option<&'static str> {
    CLASSIFIED
        .iter()
        .find(|(c, _, _)| *c == code)
        .map(|(_, name, _)| *name)
}

/// One TapeAlert parameter as the page carried it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FlagParam {
    /// The parameter code: the flag number.
    pub code: u16,
    /// Bit 0 of the parameter's value byte.
    pub raised: bool,
}

/// Every parameter of a raw LOG SENSE page 0x2E, in page order — `None`
/// when `raw` is not a whole 0x2E page (wrong page code, a header that
/// claims more bytes than there are, a parameter cut short), so a damaged
/// capture is never read as "nothing raised".
///
/// The page is a 4-byte header (page code in the low 6 bits of byte 0,
/// subpage, 16-bit page length) then parameters of a 4-byte header (16-bit
/// code, control byte, length) and `length` value bytes. A TapeAlert value
/// is one byte whose bit 0 is the flag (SSC-3); a longer value is read the
/// same way, by its first byte, and an empty one is a flag not raised.
pub fn flags_from_page(raw: &[u8]) -> Option<Vec<FlagParam>> {
    if raw.len() < 4 || raw[0] & 0x3f != PAGE {
        return None;
    }
    let page_len = u16::from_be_bytes([raw[2], raw[3]]) as usize;
    let body = raw.get(4..4 + page_len)?;
    let mut out = Vec::new();
    let mut at = 0;
    while at < body.len() {
        let head = body.get(at..at + 4)?;
        let code = u16::from_be_bytes([head[0], head[1]]);
        let len = head[3] as usize;
        let value = body.get(at + 4..at + 4 + len)?;
        out.push(FlagParam {
            code,
            raised: value.first().is_some_and(|v| v & 0x01 != 0),
        });
        at += 4 + len;
    }
    Some(out)
}

/// The raised flags of a raw page 0x2E, by code, in page order. `None` when
/// the bytes are not a whole page (see [`flags_from_page`]).
pub fn raised_codes(raw: &[u8]) -> Option<Vec<u16>> {
    flags_from_page(raw).map(|p| p.into_iter().filter(|f| f.raised).map(|f| f.code).collect())
}

/// The distinct classes among `codes`, worst first (drive, medium,
/// cleaning, other).
pub fn classes(codes: &[u16]) -> Vec<AlertClass> {
    let mut c: Vec<AlertClass> = codes.iter().map(|&code| class_of(code)).collect();
    c.sort();
    c.dedup();
    c
}

#[cfg(test)]
mod tests {
    use super::*;

    const MHVTL_RAW: &[u8] =
        include_bytes!("../../tests/fixtures/sg_logs/mhvtl_td8_sg1/page_0x2e.bin");
    const MHVTL_DECODED: &str =
        include_str!("../../tests/fixtures/sg_logs/mhvtl_td8_sg1/page_0x2e.decoded.txt");
    const HP_RAW: &[u8] =
        include_bytes!("../../tests/fixtures/sg_logs/hp_lto6_sg0_fuji_ew7vwmvkf6/page_0x2e.bin");

    /// The same page with flags 3 (Hard error) and 5 (Read failure) raised —
    /// #303's fixture recipe, synthetic: the mhvtl capture's bytes with two
    /// value bytes set. No drive has raised a flag for us yet.
    fn raised_3_and_5() -> Vec<u8> {
        let mut raw = MHVTL_RAW.to_vec();
        for code in [3u16, 5] {
            // Parameter n (1-based) starts at 4 + (n-1)*5; its value is +4.
            raw[4 + (code as usize - 1) * 5 + 4] = 1;
        }
        raw
    }

    /// Both real captures carry all 64 flags, codes 1..=64 in order, none
    /// raised. The codes come from the bytes.
    #[test]
    fn a_real_page_lists_sixty_four_flags_by_code_none_raised() {
        for raw in [MHVTL_RAW, HP_RAW] {
            let params = flags_from_page(raw).expect("a whole page");
            assert_eq!(params.len(), 64);
            assert!(params
                .iter()
                .enumerate()
                .all(|(i, p)| p.code == i as u16 + 1));
            assert_eq!(raised_codes(raw), Some(vec![]));
        }
    }

    /// #303's acceptance: a page with flags 3 and 5 raised records WHICH
    /// two, by code. The positive control is the test above: the same page
    /// unraised yields none, so this is not "everything is raised".
    #[test]
    fn raised_flags_come_back_by_their_parameter_codes() {
        assert_eq!(raised_codes(&raised_3_and_5()), Some(vec![3, 5]));
    }

    /// A code the decode's line order cannot give: a drive that returns only
    /// some parameters. Flag 20 alone, as the page's only parameter, is code
    /// 20 — the line-ordinal reading would call it flag 1, "Read warning".
    #[test]
    fn a_sparse_page_keeps_each_flags_own_code() {
        let raw = [0x2e, 0x00, 0x00, 0x05, 0x00, 0x14, 0x60, 0x01, 0x01];
        assert_eq!(raised_codes(&raw), Some(vec![0x14]));
        assert_eq!(class_of(0x14), AlertClass::Cleaning);
    }

    /// A damaged capture is not "nothing raised": a wrong page, a header
    /// claiming more than there is, a parameter cut short are all `None`.
    #[test]
    fn a_damaged_page_is_unknown_not_clean() {
        assert_eq!(flags_from_page(&[]), None);
        assert_eq!(flags_from_page(&[0x02, 0, 0, 0]), None, "page 0x02");
        let mut long = MHVTL_RAW.to_vec();
        long.truncate(100);
        assert_eq!(flags_from_page(&long), None, "page length past the bytes");
        assert_eq!(
            flags_from_page(&[0x2e, 0, 0, 3, 0, 1, 0x60]),
            None,
            "a parameter header cut short"
        );
        // And an empty page is a whole page with no parameters.
        assert_eq!(flags_from_page(&[0x2e, 0, 0, 0]), Some(vec![]));
    }

    /// Every classified code's SSC-3 name is the name sg_logs gives the
    /// parameter at that position in a real decode — so the table and the
    /// decode the journal holds cannot disagree on which flag is which.
    #[test]
    fn the_classified_names_match_sg_logs_decode_by_position() {
        let lines: Vec<&str> = MHVTL_DECODED.lines().skip(1).collect();
        assert_eq!(lines.len(), 64);
        for (code, name, _) in CLASSIFIED {
            let line = lines[*code as usize - 1].trim();
            let decoded = line.rsplit_once(": ").map(|(n, _)| n).unwrap_or(line);
            // sg_logs spells flag 25 "Dual port"; none of ours differ.
            assert_eq!(decoded, *name, "flag {code:#04x}");
        }
    }

    #[test]
    fn classes_are_the_three_verdicts_worst_first() {
        assert_eq!(class_of(0x14), AlertClass::Cleaning);
        assert_eq!(class_of(0x13), AlertClass::Medium);
        assert_eq!(class_of(0x24), AlertClass::Drive);
        assert_eq!(class_of(0x03), AlertClass::Other, "hard error: either");
        assert_eq!(
            classes(&[0x14, 0x13, 0x24, 0x15]),
            vec![AlertClass::Drive, AlertClass::Medium, AlertClass::Cleaning]
        );
        assert!(AlertClass::Medium.verdict().contains("THE CARTRIDGE"));
        assert!(AlertClass::Drive.verdict().contains("THE DRIVE"));
    }
}
