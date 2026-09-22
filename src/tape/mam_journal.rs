//! The MAM journal: every MAM read, verbatim, in `mam_journal` (migration
//! 022, issue #297).
//!
//! ADR-0013's standard is "capture everything verbatim now, parse it later".
//! [`crate::tape::mam::read_mam`] hands back a [`MamCapture`] beside its
//! parse; this module is where a capture becomes a row. It is split the way
//! the hardware forces it to be:
//!
//! - [`JournalRow::from_capture`] — the pure half: given a capture and the
//!   facts about its caller, the row. Tested by value with no hardware.
//! - [`insert`] / [`record`] — the write, fallible and best-effort
//!   respectively. [`record`] warns and returns; **a journal insert never
//!   fails a tape operation**, the same discipline as the contact guard.
//!
//! # The journal points at the contact, and the contact may open later
//!
//! ADR-0013 §5: `mam_journal.contact_id`, never `contact.journal_id`. On the
//! WRITE paths the contact opens immediately after the one MAM read, so the
//! row is written there ([`crate::tape::contact::ContactGuard::journal_mam`]).
//! On the READ paths the command takes TWO reads — `check_read_contact`, then
//! the pre-store read — before its store is open, and the contact opens
//! inside the store seam afterwards. [`MamReads`] holds both captures until
//! that moment ([`crate::tape::contact::ContactSite::open`] drains it), and
//! a command that refuses before any contact opens still journals them, with
//! `contact_id` NULL, when the holder drops. A read that happened is recorded
//! whether or not a contact could be named for it.

use std::cell::RefCell;

use rusqlite::types::Value;
use rusqlite::{params, Connection};
use tracing::warn;

use crate::config::Config;
use crate::error::Result;
use crate::tape::contact::Operation;
use crate::tape::mam::{self, MamCapture};

/// Which of the MAM-reading call sites took a reading — `mam_journal.hook`.
///
/// Five sites, and one read-path contact takes two of them, which is exactly
/// why the journal points at the contact (ADR-0013 §5).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Hook {
    /// `volume::write::volume_init`'s one read.
    VolumeInit,
    /// `volume::write::volume_write`'s one read.
    VolumeWrite,
    /// `volume::write::volume_resume`'s one read.
    VolumeResume,
    /// `media_detect::check_read_contact` — the first of a read path's two.
    CheckReadContact,
    /// `volume::binding::loaded_medium` — the second of a read path's two.
    ///
    /// Spelled `loaded_medium_serial` in the column because that is the name
    /// ADR-0013 §5 and issue #297 give this read; the function was renamed
    /// `loaded_medium` by issue #296 when it began returning the whole
    /// `MamInfo`. The stored string follows the governing record, so a
    /// reader who looks it up there finds it.
    LoadedMediumSerial,
}

impl Hook {
    pub fn as_str(self) -> &'static str {
        match self {
            Hook::VolumeInit => "volume_init",
            Hook::VolumeWrite => "volume_write",
            Hook::VolumeResume => "volume_resume",
            Hook::CheckReadContact => "check_read_contact",
            Hook::LoadedMediumSerial => "loaded_medium_serial",
        }
    }

    /// Every hook, for the pinning test.
    pub const ALL: &'static [Hook] = &[
        Hook::VolumeInit,
        Hook::VolumeWrite,
        Hook::VolumeResume,
        Hook::CheckReadContact,
        Hook::LoadedMediumSerial,
    ];
}

/// One `mam_journal` row, before it is written. Every column except `id`.
#[derive(Debug, Clone, PartialEq)]
pub struct JournalRow {
    pub captured_at: String,
    pub contact_id: Option<i64>,
    pub device_sg: String,
    pub device_tape: Option<String>,
    pub trigger: String,
    pub hook: &'static str,
    pub serial_as_read: Option<String>,
    pub ok: bool,
    pub error: Option<String>,
    /// JSON array text, program first.
    pub tool_argv: String,
    pub tool_version: Option<String>,
    /// stdout as the column stores it: `Text` when valid UTF-8, `Blob` of
    /// the exact bytes otherwise, `Null` when the tool never ran.
    pub raw: Value,
    pub parsed_json: Option<String>,
    pub tapectl_version: &'static str,
}

impl JournalRow {
    /// The row a capture becomes — the pure half of the journal.
    ///
    /// `serial_as_read` and `parsed_json` come from the capture's OWN
    /// stdout, and only when the read succeeded: a failed read's output is
    /// kept verbatim in `raw` but is not parsed as if it were an answer.
    /// `tapectl_version` is not a parameter — there is exactly one build
    /// writing (`tape::health`'s precedent).
    pub fn from_capture(
        contact_id: Option<i64>,
        trigger: &str,
        hook: Hook,
        capture: &MamCapture,
    ) -> JournalRow {
        let ok = capture.ok();
        let text = capture
            .stdout
            .as_deref()
            .map(|b| String::from_utf8_lossy(b).into_owned());
        let (serial_as_read, parsed_json) = match (&text, ok) {
            (Some(t), true) => (
                mam::parse_mam(t).serial,
                Some(mam::journal_attributes(t).to_string()),
            ),
            _ => (None, None),
        };
        let raw = match &capture.stdout {
            None => Value::Null,
            Some(bytes) => match std::str::from_utf8(bytes) {
                Ok(s) => Value::Text(s.to_string()),
                Err(_) => Value::Blob(bytes.clone()),
            },
        };
        JournalRow {
            captured_at: capture.captured_at.clone(),
            contact_id,
            device_sg: capture.device_sg.clone(),
            device_tape: capture.device_tape.clone(),
            trigger: trigger.to_string(),
            hook: hook.as_str(),
            serial_as_read,
            ok,
            error: capture.error.clone(),
            tool_argv: serde_json::to_string(&capture.tool_argv)
                .unwrap_or_else(|_| "[]".to_string()),
            tool_version: capture.tool_version.clone(),
            raw,
            parsed_json,
            tapectl_version: env!("CARGO_PKG_VERSION"),
        }
    }
}

/// Write one row. Fallible — [`record`] is the best-effort wrapper every
/// production caller uses.
pub fn insert(conn: &Connection, row: &JournalRow) -> rusqlite::Result<i64> {
    conn.execute(
        "INSERT INTO mam_journal
             (captured_at, contact_id, device_sg, device_tape, trigger, hook,
              serial_as_read, ok, error, tool_argv, tool_version, raw, parsed_json,
              tapectl_version)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)",
        params![
            row.captured_at,
            row.contact_id,
            row.device_sg,
            row.device_tape,
            row.trigger,
            row.hook,
            row.serial_as_read,
            row.ok,
            row.error,
            row.tool_argv,
            row.tool_version,
            row.raw,
            row.parsed_json,
            row.tapectl_version,
        ],
    )?;
    Ok(conn.last_insert_rowid())
}

/// Journal one capture, best-effort: a failed INSERT warns and returns
/// `None`. Bookkeeping must never become a new way for a tape command to
/// fail.
pub fn record(
    conn: &Connection,
    contact_id: Option<i64>,
    trigger: &str,
    hook: Hook,
    capture: &MamCapture,
) -> Option<i64> {
    let row = JournalRow::from_capture(contact_id, trigger, hook, capture);
    match insert(conn, &row) {
        Ok(id) => Some(id),
        Err(e) => {
            warn!(err = %e, hook = hook.as_str(), trigger, "mam_journal insert failed");
            None
        }
    }
}

/// The MAM captures a read-path command has taken and not yet journalled.
///
/// A read path reads the MAM twice before its store opens, and its contact
/// opens later still, inside the store seam — so the captures are HELD here
/// and written by [`crate::tape::contact::ContactSite::open`] the moment the
/// contact id exists ([`attach`](MamReads::attach)). Anything still held
/// when this drops — the command refused before any contact opened — is
/// journalled then with `contact_id` NULL: the read happened, and a record
/// that silently lost it would be the failure this journal exists to end.
/// The same "close on the way out" shape as `ContactSlot`.
pub struct MamReads<'c> {
    conn: &'c Connection,
    trigger: Operation,
    held: RefCell<Vec<(Hook, MamCapture)>>,
}

impl<'c> MamReads<'c> {
    pub fn new(conn: &'c Connection, trigger: Operation) -> Self {
        MamReads {
            conn,
            trigger,
            held: RefCell::new(Vec::new()),
        }
    }

    /// Hold one capture until the contact opens (or this drops).
    pub fn hold(&self, hook: Hook, capture: MamCapture) {
        self.held.borrow_mut().push((hook, capture));
    }

    /// [`crate::tape::media_detect::check_read_contact`], holding the MAM
    /// capture it took — including when it then refuses.
    pub fn check_read_contact(&self, config: &Config, device: &str) -> Result<()> {
        let (capture, verdict) = crate::tape::media_detect::check_read_contact(config, device);
        if let Some(capture) = capture {
            self.hold(Hook::CheckReadContact, capture);
        }
        verdict
    }

    /// Journal everything held against `contact_id` (NULL for an inert or
    /// absent contact), in the order it was read. Draining: a second call
    /// writes nothing.
    pub fn attach(&self, contact_id: Option<i64>) {
        let held = std::mem::take(&mut *self.held.borrow_mut());
        for (hook, capture) in &held {
            record(self.conn, contact_id, self.trigger.as_str(), *hook, capture);
        }
    }
}

impl Drop for MamReads<'_> {
    fn drop(&mut self) {
        self.attach(None);
    }
}

// ── Reading it back ──────────────────────────────────────────────────────

/// Which journal rows to read back.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Selector {
    /// Every row, including reads no cartridge could be named for.
    All,
    /// A registered cartridge's rows, found by BOTH routes: reads taken in a
    /// contact attributed to it, and reads whose chip serial is its
    /// confirmed `serial_number`. The second route is what finds a read
    /// taken before the cartridge was registered — by query, never by
    /// rewriting the row (no back-fill).
    Cartridge {
        id: i64,
        serial_number: Option<String>,
    },
    /// Rows whose chip reported this serial — for an unregistered medium.
    Serial(String),
}

/// One journal row as read back — everything but `raw`, which
/// [`raw_of`] returns byte for byte.
#[derive(Debug, Clone, serde::Serialize)]
pub struct Entry {
    pub id: i64,
    pub captured_at: String,
    pub contact_id: Option<i64>,
    pub trigger: String,
    pub hook: String,
    pub device_sg: String,
    pub device_tape: Option<String>,
    pub serial_as_read: Option<String>,
    pub ok: bool,
    pub error: Option<String>,
    pub tool_argv: serde_json::Value,
    pub tool_version: Option<String>,
    pub tapectl_version: String,
    /// `parsed_json`, parsed; `null` for a failed read.
    pub parsed: serde_json::Value,
    /// Length of `raw` in bytes; `None` when the tool never ran.
    pub raw_bytes: Option<i64>,
}

const SELECT_ENTRY: &str = "SELECT id, captured_at, contact_id, trigger, hook, device_sg,
            device_tape, serial_as_read, ok, error, tool_argv, tool_version,
            tapectl_version, parsed_json, length(CAST(raw AS BLOB))
       FROM mam_journal";

fn entry_from_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<Entry> {
    let json = |s: Option<String>| {
        s.and_then(|t| serde_json::from_str(&t).ok())
            .unwrap_or(serde_json::Value::Null)
    };
    Ok(Entry {
        id: r.get(0)?,
        captured_at: r.get(1)?,
        contact_id: r.get(2)?,
        trigger: r.get(3)?,
        hook: r.get(4)?,
        device_sg: r.get(5)?,
        device_tape: r.get(6)?,
        serial_as_read: r.get(7)?,
        ok: r.get(8)?,
        error: r.get(9)?,
        tool_argv: json(r.get(10)?),
        tool_version: r.get(11)?,
        tapectl_version: r.get(12)?,
        parsed: json(r.get(13)?),
        raw_bytes: r.get(14)?,
    })
}

/// The rows a selector names, oldest first.
pub fn entries(conn: &Connection, selector: &Selector) -> Result<Vec<Entry>> {
    let (sql, args): (String, Vec<Value>) = match selector {
        Selector::All => (format!("{SELECT_ENTRY} ORDER BY id"), vec![]),
        Selector::Serial(serial) => (
            format!("{SELECT_ENTRY} WHERE serial_as_read = ?1 ORDER BY id"),
            vec![Value::Text(serial.clone())],
        ),
        Selector::Cartridge { id, serial_number } => (
            format!(
                "{SELECT_ENTRY}
                 WHERE contact_id IN (SELECT id FROM cartridge_contacts WHERE cartridge_id = ?1)
                    OR (?2 IS NOT NULL AND serial_as_read = ?2)
                 ORDER BY id"
            ),
            vec![
                Value::Integer(*id),
                serial_number.clone().map_or(Value::Null, Value::Text),
            ],
        ),
    };
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt
        .query_map(rusqlite::params_from_iter(args), entry_from_row)?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    Ok(rows)
}

/// One row's `raw`, byte for byte: `Ok(None)` when the row exists and the
/// tool never ran, an error when no such row exists.
pub fn raw_of(conn: &Connection, id: i64) -> Result<Option<Vec<u8>>> {
    use rusqlite::types::ValueRef;
    use rusqlite::OptionalExtension;
    let found = conn
        .query_row("SELECT raw FROM mam_journal WHERE id = ?1", [id], |r| {
            Ok(match r.get_ref(0)? {
                ValueRef::Text(t) | ValueRef::Blob(t) => Some(t.to_vec()),
                ValueRef::Null => None,
                // Nothing writes a number here; render it rather than lose it.
                other => Some(format!("{other:?}").into_bytes()),
            })
        })
        .optional()?;
    found.ok_or_else(|| crate::error::TapectlError::Other(format!("no MAM journal row {id}")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::process::ExitStatusExt;
    use std::process::{ExitStatus, Output};

    /// A successful capture of `stdout`, through the real pure half of
    /// `read_mam` — not a hand-built struct, so the tests exercise what
    /// production builds.
    pub(crate) fn capture_of(stdout: &str) -> MamCapture {
        mam::capture_from_output(
            "/dev/sg-test",
            "2026-09-22 12:00:00".to_string(),
            Some("version: 1.13 20191220".to_string()),
            Ok(Output {
                status: ExitStatus::from_raw(0),
                stdout: stdout.as_bytes().to_vec(),
                stderr: Vec::new(),
            }),
        )
        .capture
    }

    /// The failed read `sg_read_attr` really produces against a missing sg
    /// node: exit 52 and an `open error` on stderr.
    pub(crate) fn failed_capture() -> MamCapture {
        mam::capture_from_output(
            "/dev/sg-missing",
            "2026-09-22 12:00:01".to_string(),
            None,
            Ok(Output {
                // Wait-status encoding: exit code 52.
                status: ExitStatus::from_raw(52 << 8),
                stdout: Vec::new(),
                stderr: b"open error: /dev/sg-missing: No such file or directory\n".to_vec(),
            }),
        )
        .capture
    }

    type Stored = (
        Option<i64>,
        String,
        String,
        Option<String>,
        i64,
        Option<String>,
        Option<String>,
        Option<String>,
        String,
    );

    fn rows(conn: &Connection) -> Vec<Stored> {
        let mut stmt = conn
            .prepare(
                "SELECT contact_id, trigger, hook, serial_as_read, ok, error, raw,
                        parsed_json, tapectl_version
                   FROM mam_journal ORDER BY id",
            )
            .unwrap();
        let v = stmt
            .query_map([], |r| {
                Ok((
                    r.get(0)?,
                    r.get(1)?,
                    r.get(2)?,
                    r.get(3)?,
                    r.get(4)?,
                    r.get(5)?,
                    r.get(6)?,
                    r.get(7)?,
                    r.get(8)?,
                ))
            })
            .unwrap()
            .map(|r| r.unwrap())
            .collect();
        v
    }

    fn open_contact(conn: &Connection) -> i64 {
        conn.execute(
            "INSERT INTO cartridge_contacts (operation, device) VALUES ('volume identify', '/dev/null')",
            [],
        )
        .unwrap();
        conn.last_insert_rowid()
    }

    // ── migration 022, to the #227/#264 standard ──

    #[test]
    fn migration_022_creates_the_journal_with_its_columns_and_indexes() {
        let conn = crate::db::open_memory().unwrap();
        let cols: Vec<(String, String, i64)> = {
            let mut stmt = conn.prepare("PRAGMA table_info(mam_journal)").unwrap();
            let v = stmt
                .query_map([], |r| Ok((r.get(1)?, r.get(2)?, r.get(3)?)))
                .unwrap()
                .map(|c| c.unwrap())
                .collect();
            v
        };
        let expect =
            |name: &str, ty: &str, notnull: i64| (name.to_string(), ty.to_string(), notnull);
        assert_eq!(
            cols,
            vec![
                expect("id", "INTEGER", 0),
                expect("captured_at", "TEXT", 1),
                expect("contact_id", "INTEGER", 0),
                expect("device_sg", "TEXT", 1),
                expect("device_tape", "TEXT", 0),
                expect("trigger", "TEXT", 1),
                expect("hook", "TEXT", 1),
                expect("serial_as_read", "TEXT", 0),
                expect("ok", "INTEGER", 1),
                expect("error", "TEXT", 0),
                expect("tool_argv", "TEXT", 1),
                expect("tool_version", "TEXT", 0),
                expect("raw", "TEXT", 0),
                expect("parsed_json", "TEXT", 0),
                expect("tapectl_version", "TEXT", 1),
            ],
            "no drive column (ADR-0013 §1): the drive is reached through contact_id"
        );

        let indexes: Vec<String> = {
            let mut stmt = conn
                .prepare(
                    "SELECT name FROM sqlite_master WHERE type = 'index' \
                     AND tbl_name = 'mam_journal' AND name NOT LIKE 'sqlite_%' ORDER BY name",
                )
                .unwrap();
            let v = stmt
                .query_map([], |r| r.get::<_, String>(0))
                .unwrap()
                .map(|n| n.unwrap())
                .collect();
            v
        };
        assert_eq!(
            indexes,
            vec!["idx_mam_journal_contact", "idx_mam_journal_serial"]
        );

        let report = crate::cli::operations::db_fsck(&conn, false, false).unwrap();
        assert!(report.integrity_ok, "integrity_check after 022");
        assert!(report.issues.is_empty(), "{:?}", report.issues);
    }

    /// `PRAGMA table_info` reports neither foreign keys nor CHECKs (the #227
    /// lesson), so the FK is proved enforced by behaviour, with a positive
    /// control, not merely listed.
    #[test]
    fn the_contact_foreign_key_is_enforced_not_merely_declared() {
        let conn = crate::db::open_memory().unwrap();
        let fks: Vec<(String, String)> = {
            let mut stmt = conn
                .prepare("PRAGMA foreign_key_list(mam_journal)")
                .unwrap();
            let v = stmt
                .query_map([], |r| Ok((r.get::<_, String>(2)?, r.get::<_, String>(3)?)))
                .unwrap()
                .map(|x| x.unwrap())
                .collect();
            v
        };
        assert_eq!(
            fks,
            vec![("cartridge_contacts".to_string(), "contact_id".to_string())],
            "one foreign key, to the contact — and no drive FK of its own"
        );

        let capture = capture_of(mam::tests_support::LTO6_SAMPLE);
        let bogus = JournalRow::from_capture(
            Some(99_999),
            "volume identify",
            Hook::CheckReadContact,
            &capture,
        );
        assert!(
            insert(&conn, &bogus).is_err(),
            "a contact_id naming no contact must be refused under foreign_keys=ON"
        );
        // Positive controls: a real contact, and NULL, are both accepted.
        let contact = open_contact(&conn);
        let good = JournalRow::from_capture(
            Some(contact),
            "volume identify",
            Hook::CheckReadContact,
            &capture,
        );
        insert(&conn, &good).expect("a real contact_id is accepted");
        let none =
            JournalRow::from_capture(None, "volume identify", Hook::CheckReadContact, &capture);
        insert(&conn, &none).expect("a NULL contact_id is the documented no-contact case");
        assert_eq!(rows(&conn).len(), 2);
    }

    /// ADR-0013 §4: `trigger` is free TEXT. Proved by behaviour — a value no
    /// command writes is ACCEPTED — with a control that the table does
    /// enforce the CHECK it does have (`ok`), so "accepted" is not a table
    /// that accepts everything.
    #[test]
    fn trigger_has_no_check_constraint() {
        let conn = crate::db::open_memory().unwrap();
        conn.execute(
            "INSERT INTO mam_journal (device_sg, trigger, hook, ok, tool_argv, tapectl_version)
             VALUES ('/dev/sg0', 'a command that does not exist yet', 'volume_init', 1, '[]', 't')",
            [],
        )
        .expect("an unknown trigger must be accepted — no CHECK (ADR-0013 §4)");
        let err = conn.execute(
            "INSERT INTO mam_journal (device_sg, trigger, hook, ok, tool_argv, tapectl_version)
             VALUES ('/dev/sg0', 'volume init', 'volume_init', 7, '[]', 't')",
            [],
        );
        assert!(err.is_err(), "control: the table's CHECK on `ok` is live");
    }

    #[test]
    fn the_hook_vocabulary_is_pinned() {
        let names: Vec<&str> = Hook::ALL.iter().map(|h| h.as_str()).collect();
        assert_eq!(
            names,
            vec![
                "volume_init",
                "volume_write",
                "volume_resume",
                "check_read_contact",
                "loaded_medium_serial",
            ]
        );
    }

    // ── the verbatim guarantee ──

    /// Labels `MamInfo` does not parse survive byte for byte, beside one it
    /// does. The positive control (a parsed label) proves the comparison is
    /// reading the real capture, not an empty column.
    #[test]
    fn raw_is_the_capture_verbatim_including_what_mam_info_discards() {
        let conn = crate::db::open_memory().unwrap();
        let sample = mam::tests_support::LTO6_SAMPLE;
        let id = record(
            &conn,
            None,
            "volume identify",
            Hook::CheckReadContact,
            &capture_of(sample),
        )
        .expect("row written");

        let raw: String = conn
            .query_row("SELECT raw FROM mam_journal WHERE id = ?1", [id], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(raw, sample, "raw is stdout byte for byte, untrimmed");
        // Positive control: a label MamInfo parses is there.
        assert!(raw.contains("Medium serial number: EW7VWMVKF6                      \n"));
        for unparsed in [
            "  Medium manufacture date: 20170824\n",
            "  Total MiB written in medium life: 0\n",
            "  Density vendor/serial number at last load: HP      HUJ808A5L4                      \n",
            "  Density vendor/serial number at load-3: HP      \n",
            "  Vendor specific medium attribute 0x1000: \n 00     02 73 1d 28 47 36 41 43  56 32 58 31 46 55 4a 49    .s.(G6ACV2X1FUJI\n",
        ] {
            assert!(raw.contains(unparsed), "lost from the journal: {unparsed:?}");
        }
    }

    /// Non-UTF-8 stdout is stored as the exact bytes, never lossily.
    #[test]
    fn non_utf8_stdout_is_stored_as_its_exact_bytes() {
        let conn = crate::db::open_memory().unwrap();
        let mut capture = capture_of("x");
        capture.stdout = Some(vec![b'A', 0xff, 0xfe, b'\n']);
        let id = record(&conn, None, "volume write", Hook::VolumeWrite, &capture).unwrap();
        let raw: Vec<u8> = conn
            .query_row("SELECT raw FROM mam_journal WHERE id = ?1", [id], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(raw, vec![b'A', 0xff, 0xfe, b'\n']);
    }

    /// `parsed_json` carries the integer the hardware printed and the unit
    /// its label named — never `MamInfo`'s converted bytes (#182).
    #[test]
    fn parsed_json_records_raw_integers_with_their_labelled_units() {
        let row = JournalRow::from_capture(
            None,
            "volume identify",
            Hook::CheckReadContact,
            &capture_of(mam::tests_support::LTO6_SAMPLE),
        );
        let v: serde_json::Value =
            serde_json::from_str(row.parsed_json.as_deref().unwrap()).unwrap();
        assert_eq!(v["max_capacity"]["value"], 2499053);
        assert_eq!(v["max_capacity"]["unit"], "MiB");
        assert_eq!(
            v["max_capacity"]["label"],
            "Maximum capacity in partition [MiB]"
        );
        assert_eq!(v["remaining_capacity"]["value"], 2499053);
        assert_eq!(v["medium_length"]["value"], 846);
        assert_eq!(v["medium_length"]["unit"], "m");
        assert_eq!(v["load_count"]["value"], 1);
        assert_eq!(v["load_count"]["unit"], serde_json::Value::Null);
        assert_eq!(v["medium_serial"]["value"], "EW7VWMVKF6");
        assert_eq!(v["medium_density_code"]["value"], 0x5a);
        assert_eq!(v["medium_density_code"]["text"], "0x5a");
        assert_eq!(row.serial_as_read.as_deref(), Some("EW7VWMVKF6"));
        assert_eq!(row.tapectl_version, env!("CARGO_PKG_VERSION"));
        assert_eq!(row.tool_argv, r#"["sg_read_attr","/dev/sg-test"]"#);
        assert_eq!(row.tool_version.as_deref(), Some("version: 1.13 20191220"));
        assert_eq!(row.captured_at, "2026-09-22 12:00:00");
    }

    // ── failure, absence ──

    #[test]
    fn a_failed_read_writes_a_row_with_ok_zero_and_its_error() {
        let conn = crate::db::open_memory().unwrap();
        record(
            &conn,
            None,
            "volume write",
            Hook::VolumeWrite,
            &failed_capture(),
        )
        .unwrap();
        // Positive control in the same table: a good read is ok=1 with raw.
        record(
            &conn,
            None,
            "volume write",
            Hook::VolumeWrite,
            &capture_of(mam::tests_support::LTO6_SAMPLE),
        )
        .unwrap();

        let all = rows(&conn);
        assert_eq!(all.len(), 2);
        let (_, _, _, serial, ok, error, raw, parsed, _) = &all[0];
        assert_eq!(*ok, 0);
        assert!(
            !error.as_deref().unwrap_or("").is_empty(),
            "error text recorded"
        );
        assert_eq!(raw.as_deref(), Some(""), "the tool ran and printed nothing");
        assert_eq!(*serial, None);
        assert_eq!(*parsed, None, "a failed read is not parsed as an answer");

        let (_, _, _, serial, ok, error, raw, _, _) = &all[1];
        assert_eq!(*ok, 1);
        assert_eq!(*error, None);
        assert!(!raw.as_deref().unwrap_or("").is_empty());
        assert_eq!(serial.as_deref(), Some("EW7VWMVKF6"));
    }

    #[test]
    fn a_spawn_failure_writes_a_row_with_null_raw() {
        let conn = crate::db::open_memory().unwrap();
        let capture = mam::capture_from_output(
            "/dev/sg0",
            "2026-09-22 12:00:00".into(),
            None,
            Err(std::io::Error::from(std::io::ErrorKind::NotFound)),
        )
        .capture;
        record(&conn, None, "volume init", Hook::VolumeInit, &capture).unwrap();
        let (_, _, _, _, ok, error, raw, _, _) = rows(&conn).remove(0);
        assert_eq!(ok, 0);
        assert!(error.unwrap().contains("spawn failed"));
        assert_eq!(raw, None, "the tool never ran: no stdout at all");
    }

    /// The mhvtl-shaped sample carries no serial; with no contact either,
    /// the row is still written, with every attribution NULL.
    #[test]
    fn no_serial_no_contact_still_writes_a_row() {
        let conn = crate::db::open_memory().unwrap();
        let id = record(
            &conn,
            None,
            "volume identify",
            Hook::LoadedMediumSerial,
            &capture_of(mam::tests_support::MHVTL_SHAPED_SAMPLE),
        );
        assert!(id.is_some());
        let (contact, _, hook, serial, ok, _, raw, _, _) = rows(&conn).remove(0);
        assert_eq!(contact, None);
        assert_eq!(serial, None);
        assert_eq!(ok, 1);
        assert_eq!(hook, "loaded_medium_serial");
        assert_eq!(
            raw.as_deref(),
            Some(mam::tests_support::MHVTL_SHAPED_SAMPLE)
        );
    }

    /// `record` is best-effort: an insert the database refuses warns and
    /// returns `None`, never panics or errors. Positive control: the same
    /// capture with a valid contact writes.
    #[test]
    fn a_refused_insert_is_best_effort() {
        let conn = crate::db::open_memory().unwrap();
        let capture = capture_of(mam::tests_support::LTO6_SAMPLE);
        assert_eq!(
            record(
                &conn,
                Some(424_242),
                "volume verify",
                Hook::CheckReadContact,
                &capture
            ),
            None
        );
        assert!(rows(&conn).is_empty());
        let contact = open_contact(&conn);
        assert!(record(
            &conn,
            Some(contact),
            "volume verify",
            Hook::CheckReadContact,
            &capture
        )
        .is_some());
        assert_eq!(rows(&conn).len(), 1);
    }

    // ── the holder ──

    #[test]
    fn held_reads_attach_to_the_contact_in_order_and_only_once() {
        let conn = crate::db::open_memory().unwrap();
        let contact = open_contact(&conn);
        {
            let reads = MamReads::new(&conn, Operation::VolumeIdentify);
            reads.hold(
                Hook::CheckReadContact,
                capture_of(mam::tests_support::LTO6_SAMPLE),
            );
            reads.hold(
                Hook::LoadedMediumSerial,
                capture_of(mam::tests_support::LTO6_SAMPLE),
            );
            assert!(rows(&conn).is_empty(), "held, not yet written");
            reads.attach(Some(contact));
            reads.attach(Some(contact));
        }
        let all = rows(&conn);
        let got: Vec<(Option<i64>, &str, &str)> = all
            .iter()
            .map(|r| (r.0, r.1.as_str(), r.2.as_str()))
            .collect();
        assert_eq!(
            got,
            vec![
                (Some(contact), "volume identify", "check_read_contact"),
                (Some(contact), "volume identify", "loaded_medium_serial"),
            ],
            "both reads, in order, once each, naming the one contact"
        );
    }

    /// A command that refuses before any contact opens still journals what
    /// it read — with `contact_id` NULL — when the holder drops.
    #[test]
    fn reads_never_attached_are_journalled_on_drop_with_no_contact() {
        let conn = crate::db::open_memory().unwrap();
        {
            let reads = MamReads::new(&conn, Operation::VolumeVerify);
            reads.hold(Hook::CheckReadContact, failed_capture());
        }
        let all = rows(&conn);
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].0, None);
        assert_eq!(all[0].1, "volume verify");
        assert_eq!(all[0].2, "check_read_contact");
    }

    /// The real `check_read_contact` through the holder: a backend on a
    /// device whose sg node does not exist takes a real (failed) read, and
    /// the holder keeps it.
    #[test]
    fn check_read_contact_through_the_holder_keeps_the_capture() {
        let conn = crate::db::open_memory().unwrap();
        let mut config = Config::default();
        config.backends.lto.push(crate::config::LtoBackendConfig {
            name: "lto0".into(),
            device_tape: "/nonexistent/tapectl-journal-nst".into(),
            device_sg: "/nonexistent/tapectl-journal-sg".into(),
            generation: "LTO-6".into(),
            capacity_override: None,
            usable_capacity_factor: 1.0,
            enospc_buffer: "0".into(),
        });
        {
            let reads = MamReads::new(&conn, Operation::VolumeIdentify);
            reads
                .check_read_contact(&config, "/nonexistent/tapectl-journal-nst")
                .expect("nothing detected, so nothing to refuse");
        }
        let (contact, trigger, hook, _, ok, error, _, _, _) = rows(&conn).remove(0);
        assert_eq!(
            (contact, trigger.as_str(), hook.as_str()),
            (None, "volume identify", "check_read_contact")
        );
        assert_eq!(ok, 0);
        assert!(error.is_some());
        let (sg, tape): (String, Option<String>) = conn
            .query_row("SELECT device_sg, device_tape FROM mam_journal", [], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })
            .unwrap();
        assert_eq!(sg, "/nonexistent/tapectl-journal-sg");
        assert_eq!(tape.as_deref(), Some("/nonexistent/tapectl-journal-nst"));
    }
}
