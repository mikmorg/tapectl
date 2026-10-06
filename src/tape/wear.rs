//! The worn-cartridge warning (issue #391, ADR-0012 2026-10-06 item 11).
//!
//! "Keep the chip's lifetime attributes (#299) and show them, with the
//! drive's rewrite counters, as a worn-cartridge warning in `volume write`'s
//! pre-flight and `cartridge info`. No health-score formula until home2 data
//! sets its thresholds."
//!
//! So this module SHOWS figures and WARNS only on facts the hardware itself
//! raised — never on a threshold tapectl would have to invent:
//!
//! - the chip's own lifetime attributes ([`MamInfo`], issue #299): loads,
//!   initialisations, lifetime MiB written/read, the manufacture date, the
//!   last drives that loaded it;
//! - the drive's lifetime counters FOR THIS CARTRIDGE, page 0x17 (volume
//!   statistics): write retries (the drive's rewrites), read retries,
//!   unrecovered errors, beginning-of-medium passes;
//! - the drive's TapeAlert page 0x2E.
//!
//! A WARNING is printed when the chip's TapeAlert flags are non-zero, when
//! page 0x2E raised a medium flag (`Media`, `Media life`, `Nearing media
//! life`, `Not data grade`), or when page 0x17 counts an unrecovered read
//! or write error on this cartridge. Retry counts are shown, never judged:
//! how many is too many is exactly the threshold home2's data is to set.
//!
//! Every reading comes from what tapectl already journals — the newest
//! successful MAM capture and log-page decodes taken through a contact with
//! this cartridge — or, in `volume write`, the MAM read the write just took.
//! Nothing here touches the drive (ADR-0013: consumers read the journal).

use rusqlite::{params, Connection, OptionalExtension};

use crate::error::Result;
use crate::tape::mam::{parse_mam, MamInfo};

/// Page 0x17's lifetime counters for the cartridge it was read through.
/// Each `None` when the decode did not carry the line.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct VolumeStatistics {
    pub write_retries: Option<i64>,
    pub read_retries: Option<i64>,
    pub unrecovered_write_errors: Option<i64>,
    pub unrecovered_read_errors: Option<i64>,
    pub bom_passes: Option<i64>,
    pub mom_passes: Option<i64>,
    pub lifetime_mb_written: Option<i64>,
    pub lifetime_mb_read: Option<i64>,
}

/// Parse a page 0x17 (volume statistics) decode as sg_logs prints it
/// (`  Total write retries: 0`). Forgiving, like every log-page parser
/// here: an unknown line is skipped.
pub fn parse_volume_statistics(decoded: &str) -> VolumeStatistics {
    let mut v = VolumeStatistics::default();
    for line in decoded.lines() {
        let Some((label, value)) = line.trim().split_once(':') else {
            continue;
        };
        let Ok(n) = value.trim().parse::<i64>() else {
            continue;
        };
        let slot = match label.trim() {
            "Total write retries" => &mut v.write_retries,
            "Total read retries" => &mut v.read_retries,
            "Total unrecovered write errors" => &mut v.unrecovered_write_errors,
            "Total unrecovered read errors" => &mut v.unrecovered_read_errors,
            "Beginning of medium passes" => &mut v.bom_passes,
            "Middle of medium passes" => &mut v.mom_passes,
            "Lifetime megabytes written" => &mut v.lifetime_mb_written,
            "Lifetime megabytes read" => &mut v.lifetime_mb_read,
            _ => continue,
        };
        *slot = Some(n);
    }
    v
}

/// The TapeAlert flags (page 0x2E) that speak about the MEDIUM's wear, by
/// the names sg_logs decodes them with.
pub const MEDIUM_TAPE_ALERTS: &[&str] = &[
    "Media",
    "Media life",
    "Nearing media life",
    "Not data grade",
];

/// The medium flags a page 0x2E decode raised (`<name>: 1`).
pub fn raised_medium_alerts(decoded_0x2e: &str) -> Vec<String> {
    decoded_0x2e
        .lines()
        .filter_map(|line| {
            let (name, value) = line.trim().rsplit_once(": ")?;
            (value.trim() == "1" && MEDIUM_TAPE_ALERTS.contains(&name.trim()))
                .then(|| name.trim().to_string())
        })
        .collect()
}

/// One cartridge's wear figures, each with when it was read.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct Wear {
    /// The chip, and when it was read (`None` for a reading just taken).
    pub mam: Option<(Option<String>, MamInfo)>,
    /// Page 0x17 for this cartridge, and when it was captured.
    pub volume_stats: Option<(String, VolumeStatistics)>,
    /// Page 0x2E's raised medium flags, and when it was captured.
    pub tape_alerts: Option<(String, Vec<String>)>,
}

impl Wear {
    /// The facts that make this a worn-cartridge WARNING — every one raised
    /// by the hardware, none a tapectl threshold. Empty: no warning.
    pub fn warnings(&self) -> Vec<String> {
        let mut w = Vec::new();
        if let Some((_, m)) = &self.mam {
            if let Some(flags) = m.tape_alert_flags.filter(|f| *f != 0) {
                w.push(format!("the chip reports TapeAlert flags {flags:#x}"));
            }
        }
        if let Some((_, alerts)) = &self.tape_alerts {
            if !alerts.is_empty() {
                w.push(format!(
                    "the drive raised TapeAlert {} (page 0x2E)",
                    alerts.join(", ")
                ));
            }
        }
        if let Some((_, v)) = &self.volume_stats {
            let ur = v.unrecovered_read_errors.unwrap_or(0);
            let uw = v.unrecovered_write_errors.unwrap_or(0);
            if ur > 0 || uw > 0 {
                w.push(format!(
                    "the drive counts {uw} unrecovered write and {ur} unrecovered read error(s) \
                     on this cartridge (page 0x17)"
                ));
            }
        }
        w
    }

    /// The report, one line each, indented for `cartridge info` and the
    /// write pre-flight. Says what was NOT recorded rather than staying
    /// silent, so "no reading" is never mistaken for "nothing wrong".
    pub fn lines(&self) -> Vec<String> {
        let opt = |v: Option<i64>| v.map_or_else(|| "unknown".to_string(), |n| n.to_string());
        let mut out = Vec::new();
        match &self.mam {
            Some((when, m)) if m.has_lifetime_attributes() => {
                let when = when
                    .as_deref()
                    .map(|w| format!(", read {w}"))
                    .unwrap_or_default();
                out.push(format!(
                    "chip{when}: {} load(s), initialised {} time(s), manufactured {}",
                    opt(m.load_count),
                    opt(m.initialization_count),
                    m.manufacture_date()
                        .map(|d| d.to_string())
                        .or_else(|| m.manufacture_date_raw.clone())
                        .unwrap_or_else(|| "unknown".into()),
                ));
                out.push(format!(
                    "  lifetime written {} MiB, read {} MiB (the chip's own figures and unit)",
                    opt(m.life_written_mib_raw),
                    opt(m.life_read_mib_raw),
                ));
                let ring: Vec<String> = m
                    .drive_ring
                    .iter()
                    .flatten()
                    .map(|s| {
                        if s.serial.is_empty() {
                            s.vendor.clone()
                        } else {
                            format!("{} {}", s.vendor, s.serial)
                        }
                    })
                    .collect();
                if !ring.is_empty() {
                    out.push(format!("  last drives to load it: {}", ring.join("; ")));
                }
            }
            Some(_) => out.push("chip: reports no lifetime attributes".into()),
            None => out.push("chip: no successful MAM reading recorded".into()),
        }
        match &self.volume_stats {
            Some((when, v)) => out.push(format!(
                "drive, page 0x17 ({when}): write retries {}, read retries {}, unrecovered \
                 write/read errors {}/{}, beginning-of-medium passes {}",
                opt(v.write_retries),
                opt(v.read_retries),
                opt(v.unrecovered_write_errors),
                opt(v.unrecovered_read_errors),
                opt(v.bom_passes),
            )),
            None => out.push("drive, page 0x17: no reading recorded for this cartridge".into()),
        }
        if let Some((when, alerts)) = &self.tape_alerts {
            if alerts.is_empty() {
                out.push(format!("drive, page 0x2E ({when}): no medium flag raised"));
            }
        }
        for w in self.warnings() {
            out.push(format!("WARNING: worn cartridge? {w}"));
        }
        out
    }
}

/// The newest successful decode of `page` journalled through a contact
/// with `cartridge_id`.
fn latest_page(conn: &Connection, cartridge_id: i64, page: u8) -> Result<Option<(String, String)>> {
    Ok(conn
        .query_row(
            "SELECT j.captured_at, j.decoded
               FROM log_page_journal j
               JOIN cartridge_contacts c ON c.id = j.contact_id
              WHERE c.cartridge_id = ?1 AND j.page_code = ?2 AND j.subpage_code = 0
                AND j.ok = 1 AND j.decoded IS NOT NULL
              ORDER BY j.id DESC LIMIT 1",
            params![cartridge_id, page],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?)
}

/// The newest successful MAM capture of this cartridge, by either route
/// the journal knows (its contacts, or its chip serial), re-parsed.
fn latest_mam(
    conn: &Connection,
    cartridge_id: i64,
    serial: Option<&str>,
) -> Result<Option<(Option<String>, MamInfo)>> {
    let row: Option<(String, Option<Vec<u8>>)> = conn
        .query_row(
            "SELECT captured_at, CAST(raw AS BLOB) FROM mam_journal
              WHERE ok = 1 AND raw IS NOT NULL
                AND (contact_id IN (SELECT id FROM cartridge_contacts WHERE cartridge_id = ?1)
                     OR (?2 IS NOT NULL AND serial_as_read = ?2))
              ORDER BY id DESC LIMIT 1",
            params![cartridge_id, serial],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    Ok(row
        .and_then(|(when, raw)| raw.map(|b| (Some(when), parse_mam(&String::from_utf8_lossy(&b))))))
}

/// Everything the journal holds about one registered cartridge's wear.
/// `fresh_mam` is a reading just taken (the write's own), preferred over
/// the journal's.
pub fn for_cartridge(
    conn: &Connection,
    cartridge_id: i64,
    fresh_mam: Option<&MamInfo>,
) -> Result<Wear> {
    let serial: Option<String> = conn
        .query_row(
            "SELECT serial_number FROM cartridges WHERE id = ?1",
            [cartridge_id],
            |r| r.get(0),
        )
        .optional()?
        .flatten();
    let mam = match fresh_mam {
        Some(m) => Some((None, m.clone())),
        None => latest_mam(conn, cartridge_id, serial.as_deref())?,
    };
    let volume_stats = latest_page(conn, cartridge_id, 0x17)?
        .map(|(when, text)| (when, parse_volume_statistics(&text)));
    let tape_alerts = latest_page(conn, cartridge_id, 0x2e)?
        .map(|(when, text)| (when, raised_medium_alerts(&text)));
    Ok(Wear {
        mam,
        volume_stats,
        tape_alerts,
    })
}

/// The cartridge `volume write` is about to write: the one whose chip
/// serial was just read, else the one the volume is bound to.
fn cartridge_for_write(conn: &Connection, volume_id: i64, mam: &MamInfo) -> Result<Option<i64>> {
    if let Some(serial) = mam.serial.as_deref() {
        if let Some(id) = conn
            .query_row(
                "SELECT id FROM cartridges WHERE serial_number = ?1",
                [serial],
                |r| r.get(0),
            )
            .optional()?
        {
            return Ok(Some(id));
        }
    }
    Ok(conn
        .query_row(
            "SELECT cartridge_id FROM cartridge_volumes WHERE volume_id = ?1
              ORDER BY mounted_at DESC LIMIT 1",
            [volume_id],
            |r| r.get(0),
        )
        .optional()?)
}

/// `volume write`'s pre-flight block: the wear lines for the cartridge in
/// the drive, built from the MAM reading the write just took and the
/// journal's newest page 0x17 / 0x2E for that cartridge. Never refuses and
/// never fails the write: a catalog error here is a log warning.
pub fn preflight_lines(conn: &Connection, volume_id: i64, mam: &MamInfo) -> Vec<String> {
    let wear = match cartridge_for_write(conn, volume_id, mam) {
        Ok(Some(id)) => for_cartridge(conn, id, Some(mam)),
        // A cartridge the catalog does not know yet: the chip is all there is.
        Ok(None) => Ok(Wear {
            mam: Some((None, mam.clone())),
            ..Wear::default()
        }),
        Err(e) => Err(e),
    };
    match wear {
        Ok(w) => w.lines(),
        Err(e) => {
            tracing::warn!(err = %e, "worn-cartridge pre-flight: the catalog could not be read");
            Vec::new()
        }
    }
}

/// Print [`preflight_lines`] to stderr, the operator channel the write's
/// other pre-flight notices use.
pub fn preflight_notice(conn: &Connection, volume_id: i64, label: &str, mam: &MamInfo) {
    let lines = preflight_lines(conn, volume_id, mam);
    if lines.is_empty() {
        return;
    }
    eprintln!("volume \"{label}\": cartridge wear (figures only; ADR-0012 sets no threshold yet):");
    for l in lines {
        eprintln!("  {l}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tape::mam::tests_support::LTO6_SAMPLE;

    const HP_0X17: &str = include_str!(
        "../../tests/fixtures/sg_logs/hp_lto6_sg0_fuji_ew7vwmvkf6/page_0x17.decoded.txt"
    );
    const HP_0X2E: &str = include_str!(
        "../../tests/fixtures/sg_logs/hp_lto6_sg0_fuji_ew7vwmvkf6/page_0x2e.decoded.txt"
    );

    #[test]
    fn parses_the_real_hp_volume_statistics_page() {
        let v = parse_volume_statistics(HP_0X17);
        assert_eq!(v.write_retries, Some(0));
        assert_eq!(v.read_retries, Some(0));
        assert_eq!(v.unrecovered_write_errors, Some(0));
        assert_eq!(v.unrecovered_read_errors, Some(0));
        assert_eq!(v.bom_passes, Some(554));
        assert_eq!(v.mom_passes, Some(0));
        assert_eq!(v.lifetime_mb_written, Some(32643));
        assert_eq!(v.lifetime_mb_read, Some(14));
        assert_eq!(parse_volume_statistics(""), VolumeStatistics::default());
    }

    #[test]
    fn medium_alerts_are_the_raised_medium_flags_only() {
        assert!(raised_medium_alerts(HP_0X2E).is_empty());
        let raised = HP_0X2E
            .replace("  Nearing media life: 0", "  Nearing media life: 1")
            .replace("  Cleaning required: 0", "  Cleaning required: 1");
        assert_eq!(raised_medium_alerts(&raised), vec!["Nearing media life"]);
    }

    /// The real cartridge: every figure shown, nothing raised, so no
    /// WARNING line — and the figures are there to read.
    #[test]
    fn a_clean_cartridge_shows_its_figures_and_no_warning() {
        let wear = Wear {
            mam: Some((Some("t".into()), parse_mam(LTO6_SAMPLE))),
            volume_stats: Some(("t".into(), parse_volume_statistics(HP_0X17))),
            tape_alerts: Some(("t".into(), raised_medium_alerts(HP_0X2E))),
        };
        assert!(wear.warnings().is_empty());
        let text = wear.lines().join("\n");
        assert!(
            text.contains("1 load(s), initialised 0 time(s), manufactured 2017-08-24"),
            "{text}"
        );
        assert!(
            text.contains("lifetime written 0 MiB, read 0 MiB"),
            "{text}"
        );
        assert!(
            text.contains("last drives to load it: HP HUJ808A5L4; HP; HP; HP"),
            "{text}"
        );
        assert!(text.contains("beginning-of-medium passes 554"), "{text}");
        assert!(!text.contains("WARNING"), "{text}");
    }

    /// Each hardware-raised fact warns on its own, and only those do —
    /// retries are shown, never judged.
    #[test]
    fn hardware_raised_facts_warn_and_retries_do_not() {
        let mut stats = parse_volume_statistics(HP_0X17);
        stats.write_retries = Some(5000);
        stats.read_retries = Some(900);
        let quiet = Wear {
            volume_stats: Some(("t".into(), stats.clone())),
            ..Wear::default()
        };
        assert!(
            quiet.warnings().is_empty(),
            "retries alone set no threshold"
        );

        stats.unrecovered_read_errors = Some(2);
        let mut chip = parse_mam(LTO6_SAMPLE);
        chip.tape_alert_flags = Some(0x40);
        let worn = Wear {
            mam: Some((None, chip)),
            volume_stats: Some(("t".into(), stats)),
            tape_alerts: Some(("t".into(), vec!["Media life".into()])),
        };
        let w = worn.warnings();
        assert_eq!(w.len(), 3, "{w:?}");
        let text = worn.lines().join("\n");
        assert!(text.contains("WARNING: worn cartridge? the chip reports TapeAlert flags 0x40"));
        assert!(text.contains("TapeAlert Media life (page 0x2E)"), "{text}");
        assert!(
            text.contains("0 unrecovered write and 2 unrecovered read"),
            "{text}"
        );
    }

    /// No reading is SAID, not left silent: "no reading" must never read as
    /// "nothing wrong".
    #[test]
    fn missing_readings_are_named_not_silent() {
        let text = Wear::default().lines().join("\n");
        assert!(
            text.contains("chip: no successful MAM reading recorded"),
            "{text}"
        );
        assert!(
            text.contains("page 0x17: no reading recorded for this cartridge"),
            "{text}"
        );
        let mhvtl = Wear {
            mam: Some((None, MamInfo::default())),
            ..Wear::default()
        };
        assert!(mhvtl.lines()[0].contains("reports no lifetime attributes"));
    }

    /// The journal route end to end: a contact with the cartridge, its MAM
    /// capture and its page 0x17 / 0x2E decodes are what `for_cartridge`
    /// reads back; another cartridge's rows are not.
    #[test]
    fn for_cartridge_reads_this_cartridges_newest_journal_rows() {
        let conn = crate::db::open_memory().unwrap();
        for (id, serial) in [(1, "EW7VWMVKF6"), (2, "OTHER")] {
            conn.execute(
                "INSERT INTO cartridges (id, barcode, media_type, nominal_capacity, serial_number)
                 VALUES (?1, ?2, 'LTO-6', 2500000000000, ?2)",
                params![id, serial],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO cartridge_contacts (id, cartridge_id, operation, device)
                 VALUES (?1, ?1, 'volume verify', '/dev/nst0')",
                [id],
            )
            .unwrap();
        }
        let worn_0x17 = HP_0X17.replace(
            "Total unrecovered read errors: 0",
            "Total unrecovered read errors: 7",
        );
        for (contact, page, text) in [(1, 0x17, HP_0X17), (2, 0x17, worn_0x17.as_str())] {
            conn.execute(
                "INSERT INTO log_page_journal (contact_id, device_sg, trigger, page_code, ok,
                    tool_argv, decoded, tapectl_version)
                 VALUES (?1, '/dev/sg0', 'volume verify', ?2, 1, '[]', ?3, 't')",
                params![contact, page, text],
            )
            .unwrap();
        }
        conn.execute(
            "INSERT INTO mam_journal (contact_id, device_sg, trigger, hook, serial_as_read, ok,
                tool_argv, raw, tapectl_version)
             VALUES (1, '/dev/sg0', 'volume verify', 'check_read_contact', 'EW7VWMVKF6', 1,
                '[]', ?1, 't')",
            [LTO6_SAMPLE],
        )
        .unwrap();

        let mine = for_cartridge(&conn, 1, None).unwrap();
        assert_eq!(
            mine.mam.as_ref().unwrap().1.manufacture_date_raw.as_deref(),
            Some("20170824")
        );
        assert_eq!(mine.volume_stats.as_ref().unwrap().1.bom_passes, Some(554));
        assert!(
            mine.warnings().is_empty(),
            "the other cartridge's 7 errors are not mine"
        );
        assert!(mine.tape_alerts.is_none());

        // Positive control: the other cartridge's own rows do warn.
        let other = for_cartridge(&conn, 2, None).unwrap();
        assert_eq!(other.warnings().len(), 1, "{:?}", other.warnings());
        assert!(other.mam.is_none());
    }

    /// `volume write`'s pre-flight block: the cartridge is found by the
    /// serial the write just read off the chip, its journalled page 0x17
    /// warns, and the chip figures are the fresh reading's. A chip serial
    /// the catalog does not know falls back to the cartridge the volume is
    /// bound to; with neither, the chip reading alone is shown.
    #[test]
    fn preflight_lines_find_the_cartridge_by_its_chip_serial_or_its_binding() {
        let conn = crate::db::open_memory().unwrap();
        conn.execute(
            "INSERT INTO cartridges (id, barcode, media_type, nominal_capacity, serial_number)
             VALUES (1, 'EW7VWMVKF6', 'LTO-6', 2500000000000, 'EW7VWMVKF6')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO volumes (id, label, backend_type, backend_name, capacity_bytes, status)
             VALUES (9, 'L6-0001', 'lto', 'p', 2500000000000, 'initialized')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO cartridge_contacts (id, cartridge_id, operation, device)
             VALUES (1, 1, 'volume verify', '/dev/nst0')",
            [],
        )
        .unwrap();
        let worn = HP_0X17.replace(
            "Total unrecovered write errors: 0",
            "Total unrecovered write errors: 1",
        );
        conn.execute(
            "INSERT INTO log_page_journal (contact_id, device_sg, trigger, page_code, ok,
                tool_argv, decoded, tapectl_version)
             VALUES (1, '/dev/sg0', 'volume verify', 23, 1, '[]', ?1, 't')",
            [&worn],
        )
        .unwrap();

        let chip = parse_mam(LTO6_SAMPLE);
        assert_eq!(chip.serial.as_deref(), Some("EW7VWMVKF6"));
        let text = preflight_lines(&conn, 9, &chip).join("\n");
        assert!(
            text.contains("WARNING: worn cartridge? the drive counts 1 unrecovered write"),
            "{text}"
        );
        assert!(
            text.contains("chip: 1 load(s)"),
            "the fresh chip reading: {text}"
        );

        // An unknown serial: the volume's binding names the cartridge.
        let mut stranger = chip.clone();
        stranger.serial = Some("NOT-REGISTERED".into());
        assert!(!preflight_lines(&conn, 9, &stranger)
            .join("\n")
            .contains("WARNING"));
        conn.execute(
            "INSERT INTO cartridge_volumes (cartridge_id, volume_id) VALUES (1, 9)",
            [],
        )
        .unwrap();
        assert!(preflight_lines(&conn, 9, &stranger)
            .join("\n")
            .contains("WARNING: worn cartridge?"));

        // Neither: the chip alone, and "no reading" said for the drive.
        let text = preflight_lines(&conn, 404, &stranger).join("\n");
        assert!(text.contains("page 0x17: no reading recorded"), "{text}");
    }
}
