//! `tapectl drive poll` — the drive's health read on a schedule (ADR-0012,
//! 2026-10-07 item 29; issue #309).
//!
//! Between a write and the next verify a drive can go months unobserved:
//! the audit timer is device-blind by design (`PrivateDevices=true`), and
//! the only `health_logs` writers were commands an operator runs at the
//! drive. The poll is the other half — a read-only command its own timer
//! runs daily — and it does exactly what a contact's post-command health
//! reading does, with no tape command around it:
//!
//! 1. one MAM read through the sg node (`READ ATTRIBUTE`, no motion) — it
//!    answers only when a cartridge is loaded, and a failed read is itself
//!    journalled;
//! 2. a contact of its own (`cartridge_contacts.operation = 'drive poll'`),
//!    naming the cartridge when the chip's serial is a registered one and
//!    otherwise none — a DRIVE-ONLY reading, with the reason recorded;
//! 3. one log-page sweep (page 0x00, then every page it lists, each once),
//!    every page journalled verbatim against the contact
//!    (`log_page_journal`), then one `health_logs` row of kind `poll` with
//!    `volume_id` NULL — the poll never reads the tape, so it cannot vouch
//!    for which volume is on it; the cartridge on its contact is the
//!    attribution, and `audit` (`tape_alert`) follows it to the live
//!    volume the catalog has on that cartridge.
//!
//! **What it never does:** open the tape node, move the tape, load or eject
//! a cartridge, or write anything to a medium. Everything it sends goes to
//! the backend's `device_sg` (LOG SENSE, READ ATTRIBUTE, INQUIRY); the
//! drive's identity and the st statistics come from sysfs.
//!
//! **Read-to-clear.** Page 0x2E may clear as it is read, so a page the poll
//! reads must reach the catalog, and a poll that cannot record what it
//! reads must not read it:
//!
//! - **Before the sweep**, the poll proves the catalog writable (it takes
//!   the write lock once, then opens its contact). A busy catalog is
//!   [`TapectlError::CatalogBusy`] — exit 75, "the next poll reads it" — and
//!   any other refusal (read-only, wrong owner, a full disk), or a contact
//!   that could not be recorded, is an error (exit 2, `/fail`); either way
//!   NO page is read.
//! - **After the sweep**, every journal row and the reading's `health_logs`
//!   row is written under the busy policy (`db::busy::retry`), and anything
//!   that still could not be written is named in
//!   [`PollReport::unrecorded`]: the CLI prints what was read — the alerts
//!   included, the only copy left — and exits 2 naming it, never 0 and
//!   never 75 (the wrapper sends `/fail`).
//!
//! The journal rows are written before anything here looks at what they
//! say, and the poll runs only under the drive lock
//! (`staging::lock::try_hold_drive`, taken by the CLI before the catalog is
//! even opened), which every tape command holds from its contact to its
//! exit, so a poll never takes the alerts a command's own sweep is owed.

use std::cell::RefCell;
use std::path::Path;

use rusqlite::{params, Connection, OptionalExtension};

use crate::config::{Config, LtoBackendConfig};
use crate::db::busy::{self, BusyPolicy};
use crate::error::{Result, TapectlError};
use crate::tape::contact::{ContactSite, Medium, Operation, OUTCOME_FAILED, OUTCOME_OK};
use crate::tape::drive_identity::{self, DriveIdentity};
use crate::tape::health::{self, Reading};
use crate::tape::log_pages::{self, LogSource};
use crate::tape::mam::{self, MamRead};
use crate::tape::mam_journal::Hook;

/// Where the poll's hardware answers come from. `Probe::default()` — the
/// production value — asks the drive; a test supplies each answer.
#[derive(Default)]
pub struct Probe<'a> {
    pub log_source: Option<&'a RefCell<dyn LogSource + 'a>>,
    pub identity: Option<DriveIdentity>,
    pub mam: Option<MamRead>,
    pub sysfs_root: Option<&'a Path>,
    /// How long the post-sweep writes wait out a busy catalog. `None`, the
    /// production value, is [`BusyPolicy::DEFAULT`].
    pub busy: Option<BusyPolicy>,
}

/// What one poll recorded, and what the drive reported.
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize)]
pub struct PollReport {
    pub backend: String,
    pub device_sg: String,
    /// The `cartridge_contacts` row, `None` only when its INSERT failed.
    pub contact_id: Option<i64>,
    /// The drive's serial, when it gave one.
    pub drive_serial: Option<String>,
    /// The chip's serial, when a cartridge answered the MAM read.
    pub medium_serial: Option<String>,
    /// The registered cartridge that serial names (its barcode).
    pub cartridge: Option<String>,
    /// The live volumes the catalog has on that cartridge — the catalog's
    /// record, not something this poll read from the tape.
    pub volumes: Vec<String>,
    /// Log pages read and journalled successfully, and attempted and failed.
    pub pages_read: usize,
    pub pages_failed: usize,
    /// The TapeAlert flags raised on page 0x2E, by name. Empty for a 0x2E
    /// that raised nothing, and for one that was not read
    /// (`tape_alerts` then says which).
    pub raised_alerts: Vec<String>,
    /// `health_logs.tape_alerts` for this reading: `None` when no 0x2E
    /// decode contributed.
    pub tape_alerts: Option<i64>,
    /// "Total uncorrected errors" on pages 0x02 + 0x03 — the drive's own
    /// count of unrecovered errors. `None` when either page was not read.
    pub uncorrected: Option<i64>,
    /// What this poll read but could not write to the catalog (a log page's
    /// journal row, the reading's `health_logs` row), one line each, with
    /// the catalog's error. Not empty: the reading is lost but for this
    /// report, and the poll fails (`cli::drive::exit_code`).
    pub unrecorded: Vec<String>,
}

impl PollReport {
    /// Whether the drive reported a problem: a raised TapeAlert flag, or an
    /// unrecovered error. Either is a `/fail` for the poll's wrapper.
    pub fn drive_reported_problem(&self) -> bool {
        self.tape_alerts.unwrap_or(0) > 0
            || !self.raised_alerts.is_empty()
            || self.uncorrected.unwrap_or(0) > 0
    }

    /// The reasons, one line each, for [`PollReport::drive_reported_problem`].
    pub fn problems(&self) -> Vec<String> {
        let mut out = Vec::new();
        if !self.raised_alerts.is_empty() {
            out.push(format!(
                "TapeAlert raised: {}",
                self.raised_alerts.join(", ")
            ));
        } else if let Some(n @ 1..) = self.tape_alerts {
            out.push(format!("{n} TapeAlert flag(s) raised"));
        }
        if let Some(n @ 1..) = self.uncorrected {
            out.push(format!(
                "the drive reports {n} unrecovered (uncorrected) error(s) on log pages 0x02/0x03"
            ));
        }
        out
    }
}

/// The TapeAlert flags a decoded page 0x2E raised, `<number> <name>`, in
/// page order — through `report health`'s one flag parser, so the poll and
/// the report can never name a flag differently.
fn raised_tape_alerts(decoded_0x2e: &str) -> Vec<String> {
    crate::cli::report::raised_tape_alert_flags(decoded_0x2e)
        .into_iter()
        .map(|(n, name)| format!("{n} {name}"))
        .collect()
}

/// Poll the drive `backend` names: one MAM read, one contact, one log-page
/// sweep, every row written before anything is judged.
///
/// Fails, reading no page, when the catalog cannot record the poll: busy
/// ([`TapectlError::CatalogBusy`]) or refusing writes (any other error).
/// Past that point it returns a report: a failed page read is a journal row
/// that says so, a sweep that read nothing is [`PollReport::pages_read`]
/// `== 0`, and a row that could not be written is in
/// [`PollReport::unrecorded`] — the caller judges all three.
///
/// The caller must already hold the drive lock (`staging::lock::
/// try_hold_drive`).
pub fn poll(
    conn: &Connection,
    config: &Config,
    backend: &LtoBackendConfig,
    probe: Probe<'_>,
) -> Result<PollReport> {
    let policy = probe.busy.unwrap_or(BusyPolicy::DEFAULT);
    // Writable, before anything is read: one take of the write lock (the
    // connection's own busy_timeout is the wait), rolled back at once. The
    // contact's INSERT below is the proof; this is what tells "busy" (75,
    // the next poll reads it) from "refused" (2), because the contact guard
    // swallows its own INSERT error by design.
    match busy::immediate_tx(conn) {
        Ok(tx) => drop(tx),
        Err(e) if busy::is_busy_error(&e) => {
            return Err(TapectlError::CatalogBusy(format!(
                "drive poll: another tapectl command is writing the catalog ({e}); nothing \
                 was read from {} — the next poll will read it",
                backend.device_sg
            )))
        }
        Err(e) => {
            return Err(TapectlError::Other(format!(
                "drive poll: the catalog refuses writes ({e}), so nothing read from {} \
                 could be recorded; nothing was read",
                backend.device_sg
            )))
        }
    }

    let mut mam_read = probe
        .mam
        .unwrap_or_else(|| mam::read_mam(&backend.device_sg));
    mam_read.capture.device_tape = Some(backend.device_tape.clone());

    let mut site = ContactSite::new(
        config,
        Operation::DrivePoll,
        &backend.device_tape,
        Medium::Observed {
            backend,
            mam: &mam_read.info,
        },
    );
    if let Some(identity) = probe.identity.as_ref() {
        site = site.with_drive_identity(identity);
    }
    if let Some(root) = probe.sysfs_root {
        site = site.with_sysfs_root(root);
    }
    let guard = site.open(conn, None);
    let Some(contact_id) = guard.id() else {
        // The INSERT failed although the write lock was taken above (the
        // catalog is read-only to this user, or filled in between): a page
        // read now would have no contact to land on, and 0x2E may not
        // survive its read.
        guard.finish(OUTCOME_FAILED, None);
        return Err(TapectlError::Other(format!(
            "drive poll: its contact could not be recorded in the catalog (the warning above \
             says why), so nothing was read from {}",
            backend.device_sg
        )));
    };
    guard.journal_mam(Operation::DrivePoll, Hook::DrivePoll, &mam_read.capture);

    // The sweep: the ONE log-page reader. An injected source stands in for
    // the drive entirely, INQUIRY included.
    let (sweep, header) = match probe.log_source {
        Some(source) => (log_pages::sweep(&mut *source.borrow_mut()), None),
        None => (
            log_pages::sweep(&mut log_pages::SgLogs::new(&backend.device_sg)),
            log_pages::inquiry_header(&backend.device_sg),
        ),
    };
    let identity = match probe.identity {
        Some(identity) => identity,
        None => drive_identity::read_identity(backend),
    };
    let drive_serial = identity.serial.clone();
    let unrecorded = record(
        conn,
        policy,
        contact_id,
        backend,
        &sweep,
        header.as_deref(),
        identity,
    );

    // Judged only now, from what was journalled above.
    let pages_read = sweep.captures.iter().filter(|c| c.ok()).count();
    let pages_failed = sweep.captures.len() - pages_read;
    let counters = sweep.health(None).map(|(c, _)| c).unwrap_or_default();
    let raised_alerts = sweep
        .page(0x2e)
        .and_then(|c| c.decoded.as_deref())
        .map(raised_tape_alerts)
        .unwrap_or_default();

    let medium_serial = mam_read.info.serial.clone();
    let (cartridge, volumes) = match medium_serial.as_deref() {
        Some(serial) => cartridge_and_volumes(conn, serial),
        None => (None, Vec::new()),
    };

    let report = PollReport {
        backend: backend.name.clone(),
        device_sg: backend.device_sg.clone(),
        contact_id: Some(contact_id),
        drive_serial,
        medium_serial,
        cartridge,
        volumes,
        pages_read,
        pages_failed,
        raised_alerts,
        tape_alerts: counters.tape_alerts,
        uncorrected: counters.total_uncorrected,
        unrecorded,
    };
    let detail = format!(
        "{pages_read} page(s) read, {pages_failed} failed{}{}",
        match report.problems() {
            p if p.is_empty() => String::new(),
            p => format!("; {}", p.join("; ")),
        },
        match report.unrecorded.len() {
            0 => String::new(),
            n => format!("; {n} row(s) not recorded"),
        }
    );
    let outcome = if pages_read == 0 || !report.unrecorded.is_empty() {
        OUTCOME_FAILED
    } else {
        OUTCOME_OK
    };
    guard.finish(outcome, Some(&detail));
    Ok(report)
}

/// Write the sweep to the catalog: every page's journal row, then the
/// reading's `health_logs` row (kind `poll`, no volume), then the drive.
/// The rows `volume::write::record_sweep_and_health` writes for a command's
/// reading, but each journal and health INSERT waits out a busy catalog
/// (`policy`), and a failure is RETURNED, one line each, not only warned: a
/// command's sweep is bookkeeping beside its real work, and the poll's
/// sweep is all of its work.
fn record(
    conn: &Connection,
    policy: BusyPolicy,
    contact_id: i64,
    backend: &LtoBackendConfig,
    sweep: &log_pages::Sweep,
    header: Option<&str>,
    mut identity: DriveIdentity,
) -> Vec<String> {
    let mut unrecorded = Vec::new();
    let trigger = Operation::DrivePoll.as_str();
    for capture in &sweep.captures {
        let row = log_pages::JournalRow::from_capture(
            Some(contact_id),
            trigger,
            Some(&backend.device_tape),
            capture,
        );
        if let Err(e) = busy::retry(policy, "a drive poll's log page", || {
            Ok(log_pages::insert(conn, &row)?)
        }) {
            unrecorded.push(format!(
                "page 0x{:02x}'s journal row ({e})",
                capture.page_code
            ));
        }
    }
    let collected = sweep.health(header);
    if let Some((counters, raw)) = &collected {
        if let Err(e) = busy::retry(policy, "a drive poll's health reading", || {
            health::record(
                conn,
                None,
                Some(contact_id),
                None,
                Reading::Poll,
                counters,
                raw,
            )
        }) {
            unrecorded.push(format!("the reading's health_logs row ({e})"));
        }
        identity.backfill_from_sg_logs_header(raw);
    }
    // The drive, best-effort as every reading's is: which drive answered is
    // an addition to the record, and the contact already names it.
    crate::volume::write::record_health_and_drive(
        conn,
        None,
        Some(contact_id),
        None,
        Reading::Poll,
        None,
        identity,
        &backend.device_tape,
    );
    unrecorded
}

/// The registered cartridge `serial` names (its barcode), and the live
/// volumes the catalog has bound to it.
fn cartridge_and_volumes(conn: &Connection, serial: &str) -> (Option<String>, Vec<String>) {
    let found: Option<(i64, String)> = conn
        .query_row(
            "SELECT id, barcode FROM cartridges WHERE serial_number = ?1",
            params![serial],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()
        .ok()
        .flatten();
    let Some((cartridge_id, barcode)) = found else {
        return (None, Vec::new());
    };
    let sql = format!(
        "SELECT v.label FROM cartridge_volumes cv JOIN volumes v ON v.id = cv.volume_id
          WHERE cv.cartridge_id = ?1 AND {}
          ORDER BY v.label",
        crate::policy::coverage::in_service_or_provisioned("v")
    );
    let volumes = conn
        .prepare(&sql)
        .and_then(|mut stmt| {
            stmt.query_map(params![cartridge_id], |r| r.get::<_, String>(0))?
                .collect::<rusqlite::Result<Vec<_>>>()
        })
        .unwrap_or_default();
    (Some(barcode), volumes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tape::log_pages::tests::FixtureSource;
    use std::os::unix::process::ExitStatusExt;

    fn backend() -> LtoBackendConfig {
        LtoBackendConfig {
            name: "lto0".to_string(),
            device_tape: "/nonexistent/poll-test/nst0".to_string(),
            device_sg: "/nonexistent/poll-test/sg0".to_string(),
            generation: "LTO-6".to_string(),
            capacity_override: None,
            fill_ceiling: 0.95,
            enospc_buffer: "1GiB".to_string(),
        }
    }

    fn config() -> Config {
        let mut c = Config::default();
        c.backends.lto.push(backend());
        c
    }

    fn drive() -> DriveIdentity {
        DriveIdentity {
            vendor: Some("HP".into()),
            model: Some("Ultrium 6-SCSI".into()),
            firmware_rev: Some("35GD".into()),
            serial: Some("HUJ808A5L4".into()),
        }
    }

    /// A MAM read as `sg_read_attr` would answer it: `Some(stdout)` for a
    /// loaded cartridge, `None` for an empty drive (exit 2, "Not ready").
    fn mam(stdout: Option<&str>) -> MamRead {
        let out = match stdout {
            Some(s) => std::process::Output {
                status: std::process::ExitStatus::from_raw(0),
                stdout: s.as_bytes().to_vec(),
                stderr: Vec::new(),
            },
            None => std::process::Output {
                status: std::process::ExitStatus::from_raw(2 << 8),
                stdout: Vec::new(),
                stderr: b"Read attribute failed: Not ready, medium not present".to_vec(),
            },
        };
        mam::capture_from_output(
            "/nonexistent/poll-test/sg0",
            "2026-10-07 09:00:00".into(),
            None,
            Ok(out),
        )
    }

    /// The mhvtl fixture's offline decode of `page`.
    fn fixture_decode(page: u8) -> String {
        crate::tape::log_pages::tests::fixture_pages()
            .into_iter()
            .find(|(p, _, _)| *p == page)
            .unwrap()
            .2
            .to_string()
    }

    fn run(conn: &Connection, source: FixtureSource, mam_read: MamRead) -> PollReport {
        let cell = RefCell::new(source);
        poll(
            conn,
            &config(),
            &backend(),
            Probe {
                log_source: Some(&cell),
                identity: Some(drive()),
                mam: Some(mam_read),
                sysfs_root: Some(Path::new("/nonexistent/poll-test/sys")),
                busy: None,
            },
        )
        .expect("a writable catalog records the poll")
    }

    /// The empty drive (the acceptance's tapeless case): a contact with no
    /// cartridge and the reason why, the MAM failure journalled, every page
    /// journalled, and one `poll` reading with no volume — recorded, and
    /// clean.
    #[test]
    fn an_empty_drive_records_a_drive_only_reading() {
        let conn = crate::db::open_memory().unwrap();
        let report = run(&conn, FixtureSource::default(), mam(None));
        let cid = report.contact_id.expect("a contact was opened");

        type Row = (
            Option<i64>,
            Option<i64>,
            Option<i64>,
            String,
            Option<String>,
            Option<String>,
        );
        let (cartridge, volume, drive, op, reason, outcome): Row = conn
            .query_row(
                "SELECT cartridge_id, volume_id, drive_id, operation, identity_reason, outcome
                   FROM cartridge_contacts WHERE id = ?1",
                [cid],
                |r| {
                    Ok((
                        r.get(0)?,
                        r.get(1)?,
                        r.get(2)?,
                        r.get(3)?,
                        r.get(4)?,
                        r.get(5)?,
                    ))
                },
            )
            .unwrap();
        assert_eq!(op, "drive poll");
        assert_eq!((cartridge, volume), (None, None), "a drive-only contact");
        assert_eq!(
            reason.as_deref(),
            Some(crate::tape::contact::REASON_NO_MEDIUM_SERIAL)
        );
        assert!(drive.is_some(), "the drive is named");
        assert_eq!(outcome.as_deref(), Some(OUTCOME_OK));

        let (ok, hook, trigger): (i64, String, String) = conn
            .query_row(
                "SELECT ok, hook, trigger FROM mam_journal WHERE contact_id = ?1",
                [cid],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap();
        assert_eq!(
            (ok, hook.as_str(), trigger.as_str()),
            (0, "drive_poll", "drive poll"),
            "the failed MAM read is itself a journal row"
        );

        crate::tape::log_pages::tests::assert_one_reading_of_kind(
            &conn,
            cid,
            "drive poll",
            "poll",
            None,
            crate::tape::log_pages::tests::LISTED.len(),
        );
        assert_eq!(
            report.pages_read,
            crate::tape::log_pages::tests::LISTED.len()
        );
        assert_eq!(report.tape_alerts, Some(0));
        assert!(
            !report.drive_reported_problem(),
            "positive control: a clean reading is not a problem: {report:?}"
        );
    }

    /// The positive control: a known cartridge loaded — the contact names
    /// it, and the report names the live volume the catalog has on it.
    #[test]
    fn a_loaded_known_cartridge_is_attributed() {
        let conn = crate::db::open_memory().unwrap();
        conn.execute(
            "INSERT INTO cartridges (barcode, media_type, serial_number, nominal_capacity)
             VALUES ('CART01L6', 'LTO-6', 'EW7VWMVKF6', 2500000000000)",
            [],
        )
        .unwrap();
        let cart = conn.last_insert_rowid();
        conn.execute(
            "INSERT INTO volumes (label, backend_type, backend_name, media_type,
                                  capacity_bytes, status)
             VALUES ('L6-0001', 'lto', 'lto0', 'LTO-6', 2500000000000, 'sealed')",
            [],
        )
        .unwrap();
        let vol = conn.last_insert_rowid();
        conn.execute(
            "INSERT INTO cartridge_volumes (cartridge_id, volume_id) VALUES (?1, ?2)",
            params![cart, vol],
        )
        .unwrap();

        let report = run(
            &conn,
            FixtureSource::default(),
            mam(Some(mam::tests_support::LTO6_SAMPLE)),
        );
        let (cartridge, reason): (Option<i64>, Option<String>) = conn
            .query_row(
                "SELECT cartridge_id, identity_reason FROM cartridge_contacts WHERE id = ?1",
                [report.contact_id.unwrap()],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!((cartridge, reason), (Some(cart), None));
        assert_eq!(report.cartridge.as_deref(), Some("CART01L6"));
        assert_eq!(report.volumes, vec!["L6-0001".to_string()]);
        let health_volume: Option<i64> = conn
            .query_row("SELECT volume_id FROM health_logs", [], |r| r.get(0))
            .unwrap();
        assert_eq!(
            health_volume, None,
            "the poll read no File 0, so the reading names the cartridge's contact, not a volume"
        );
    }

    /// The negative control, by name: a raised flag on 0x2E is a problem,
    /// and so is an unrecovered error — and the page's bytes are in the
    /// journal before the report says so.
    #[test]
    fn a_raised_tape_alert_or_an_unrecovered_error_is_a_problem() {
        let conn = crate::db::open_memory().unwrap();
        let mut src = FixtureSource::default();
        let p2e = fixture_decode(0x2e)
            .replace("  Media: 0", "  Media: 1")
            .replace("  Cleaning required: 0", "  Cleaning required: 1");
        src.text.insert(0x2e, p2e);
        let report = run(&conn, src, mam(None));
        assert_eq!(
            report.raised_alerts,
            vec!["4 Media", "20 Cleaning required"],
            "named by SSC-3 flag number and name"
        );
        assert_eq!(report.tape_alerts, Some(2));
        assert!(report.drive_reported_problem());
        assert_eq!(
            report.problems(),
            vec!["TapeAlert raised: 4 Media, 20 Cleaning required".to_string()]
        );
        let journalled: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM log_page_journal
                  WHERE page_code = 46 AND ok = 1 AND raw IS NOT NULL AND contact_id = ?1",
                [report.contact_id.unwrap()],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(journalled, 1, "the 0x2E read is persisted");

        let conn = crate::db::open_memory().unwrap();
        let mut src = FixtureSource::default();
        let p03 = fixture_decode(0x03).replace(
            "Total uncorrected errors = 0",
            "Total uncorrected errors = 4",
        );
        assert!(
            p03.contains("Total uncorrected errors = 4"),
            "fixture edit took"
        );
        src.text.insert(0x03, p03);
        let report = run(&conn, src, mam(None));
        assert_eq!(report.uncorrected, Some(4));
        assert!(report.raised_alerts.is_empty());
        assert!(report.drive_reported_problem());
        assert_eq!(report.problems().len(), 1, "{:?}", report.problems());
    }

    /// A poll's exit code as the CLI computes it, and the source it read
    /// through — so a test can ask which pages the "drive" was asked for.
    fn run_keeping(
        conn: &Connection,
        source: FixtureSource,
    ) -> (crate::error::Result<i32>, FixtureSource) {
        let cell = RefCell::new(source);
        let code = poll(
            conn,
            &config(),
            &backend(),
            Probe {
                log_source: Some(&cell),
                identity: Some(drive()),
                mam: Some(mam(None)),
                sysfs_root: Some(Path::new("/nonexistent/poll-test/sys")),
                busy: None,
            },
        )
        .and_then(|report| crate::cli::drive::exit_code(&report));
        (code, cell.into_inner())
    }

    /// A catalog the poll cannot write to (read-only, wrong owner, a full
    /// disk): nothing could record what it reads, and page 0x2E may clear
    /// on read — so it reads NO page, and does not exit 0.
    #[test]
    fn an_unwritable_catalog_reads_no_page() {
        let conn = crate::db::open_memory().unwrap();
        conn.execute_batch("PRAGMA query_only = 1").unwrap();
        let (code, source) = run_keeping(&conn, FixtureSource::default());
        assert!(
            source.reads.is_empty(),
            "no page may be read that cannot be journalled: {:?}",
            source.reads
        );
        let err = code.expect_err("an unrecorded poll is never a success");
        assert!(
            !crate::db::busy::is_busy_error(&err),
            "a read-only catalog is not 'busy, retry later': {err}"
        );
    }

    /// A catalog another writer holds: the poll reads nothing and says
    /// "busy" — the 75 `main` gives a busy catalog, which the wrapper
    /// treats as "the next poll will read it".
    #[test]
    fn a_busy_catalog_reads_no_page_and_says_busy() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tapectl.db");
        let conn = crate::db::open(&path).unwrap();
        let holder = crate::db::open(&path).unwrap();
        holder.execute_batch("BEGIN IMMEDIATE").unwrap();
        conn.busy_timeout(std::time::Duration::ZERO).unwrap();
        let (code, source) = run_keeping(&conn, FixtureSource::default());
        holder.execute_batch("ROLLBACK").unwrap();
        assert!(source.reads.is_empty(), "{:?}", source.reads);
        let err = code.expect_err("a busy catalog is never a success");
        assert!(
            crate::db::busy::is_busy_error(&err),
            "busy, so `main` exits 75: {err}"
        );
    }

    /// The contact opened, but page 0x2E's journal row was refused: the
    /// page was read (and may be cleared), so the poll must fail naming it
    /// — never a quiet 0, and never "busy" (the wrapper does not ping on
    /// 75, and this reading is lost).
    #[test]
    fn a_page_read_but_not_journalled_fails_naming_it() {
        let conn = crate::db::open_memory().unwrap();
        conn.execute_batch(
            "CREATE TEMP TRIGGER refuse_0x2e BEFORE INSERT ON main.log_page_journal
               WHEN NEW.page_code = 46
             BEGIN SELECT RAISE(ABORT, 'disk full'); END;",
        )
        .unwrap();
        let (code, source) = run_keeping(&conn, FixtureSource::default());
        assert_eq!(
            source.reads.get(&0x2e),
            Some(&1),
            "positive control: the hazard happened, 0x2E was read"
        );
        let err = code.expect_err("a lost 0x2E is never a success");
        assert!(!crate::db::busy::is_busy_error(&err), "{err}");
        assert!(err.to_string().contains("0x2e"), "names the page: {err}");
    }

    /// Likewise the reading's `health_logs` row — what `audit` and `report
    /// health` read: refused, the poll fails saying so.
    #[test]
    fn a_reading_whose_health_row_is_refused_fails() {
        let conn = crate::db::open_memory().unwrap();
        conn.execute_batch(
            "CREATE TEMP TRIGGER refuse_health BEFORE INSERT ON main.health_logs
             BEGIN SELECT RAISE(ABORT, 'disk full'); END;",
        )
        .unwrap();
        let (code, _) = run_keeping(&conn, FixtureSource::default());
        let err = code.expect_err("an unrecorded reading is never a success");
        assert!(!crate::db::busy::is_busy_error(&err), "{err}");
        assert!(err.to_string().contains("health"), "{err}");
    }

    /// A sweep that read nothing (the sg node unreadable) is still a
    /// recorded contact — closed `failed` — with every failed read
    /// journalled, and no reading.
    #[test]
    fn a_sweep_that_reads_nothing_is_recorded_as_failed() {
        let conn = crate::db::open_memory().unwrap();
        let mut src = FixtureSource::default();
        for p in crate::tape::log_pages::tests::LISTED {
            src.fail.insert(p);
        }
        src.fail.insert(0x00);
        let report = run(&conn, src, mam(None));
        assert_eq!(report.pages_read, 0);
        assert!(report.pages_failed > 0);
        let outcome: String = conn
            .query_row(
                "SELECT outcome FROM cartridge_contacts WHERE id = ?1",
                [report.contact_id.unwrap()],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(outcome, OUTCOME_FAILED);
        let rows: i64 = conn
            .query_row("SELECT COUNT(*) FROM health_logs", [], |r| r.get(0))
            .unwrap();
        assert_eq!(rows, 0, "no page, no reading");
    }
}
