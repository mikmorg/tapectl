//! Issue #488: `report events`, `report health` and `volume info` show an HP
//! drive's repositions per 100 MB (page 0x34, parameter 0) beside each
//! write's feed ratio, through the real binary. A write recorded since #488
//! carries the figure in its `write_feed_ratio` event; one recorded before
//! has it decoded from the page 0x34 its contact's sweep journalled, found by
//! the event's `details.contact_id`. Rows are seeded with the writer's own
//! serializer (`Assessment::details_json*`) and the journal's own insert, so
//! a change to either breaks here. Ungated: no device, no tape, no mhvtl.

use std::path::Path;
use std::process::{Command, Output};

use tapectl::db::events;
use tapectl::tape::feed_ratio::{Assessment, FeedRatio, EVENT_ACTION, EVENT_FIELD};
use tapectl::tape::log_pages::{self, HpPerformance, JournalRow};
use tempfile::TempDir;

const NOMEDIA_0X34: &[u8] = include_bytes!("fixtures/sg_logs/hp_lto6_sg0_nomedia/page_0x34.bin");

fn tapectl(home: &Path, args: &[&str]) -> Output {
    let out = Command::new(env!("CARGO_BIN_EXE_tapectl"))
        .arg("--home")
        .arg(home)
        .args(args)
        .output()
        .expect("spawn tapectl");
    assert!(
        out.status.success(),
        "tapectl {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    out
}

fn stdout(home: &Path, args: &[&str]) -> String {
    String::from_utf8(tapectl(home, args).stdout).expect("utf-8")
}

/// A sealed, written volume; a contact on a drive of `vendor`; and that
/// contact's journalled page 0x34 — the real HP LTO-6 capture, 5
/// repositions per 100 MB. Returns `(volume id, contact id)`.
fn written(conn: &rusqlite::Connection, label: &str, vendor: &str) -> (i64, i64) {
    conn.execute(
        "INSERT INTO volumes (label, backend_type, backend_name, media_type, capacity_bytes,
                              status, first_write, last_write)
         VALUES (?1, 'lto', 'lto0', 'LTO-6', 2500000000000, 'sealed',
                 '2026-10-14 08:00:00', '2026-10-14 08:00:00')",
        [label],
    )
    .unwrap();
    let vid = conn.last_insert_rowid();
    conn.execute(
        "INSERT INTO drives (serial, vendor, model) VALUES (?1, ?2, 'Ultrium 6-SCSI')",
        rusqlite::params![format!("SER-{label}"), vendor],
    )
    .unwrap();
    let drive = conn.last_insert_rowid();
    conn.execute(
        "INSERT INTO cartridge_contacts (operation, device, drive_id)
         VALUES ('volume write', '/dev/null', ?1)",
        [drive],
    )
    .unwrap();
    let cid = conn.last_insert_rowid();
    log_pages::insert(
        conn,
        &JournalRow {
            captured_at: "2026-10-14 08:59:00".into(),
            contact_id: Some(cid),
            device_sg: "/dev/sg0".into(),
            device_tape: Some("/dev/nst0".into()),
            trigger: "volume write".into(),
            page_code: 0x34,
            subpage_code: 0,
            ok: true,
            error: None,
            tool_argv: "[\"sg_logs\"]".into(),
            tool_version: None,
            raw: rusqlite::types::Value::Blob(NOMEDIA_0X34.to_vec()),
            decoded: Some("Unable to decode Read forward errors (lto-5) [rfe_]".into()),
            tapectl_version: "test",
        },
    )
    .unwrap();
    (vid, cid)
}

fn record(conn: &rusqlite::Connection, vid: i64, label: &str, details: &str, at: &str) {
    let id = events::log_event(
        conn,
        "volume",
        vid,
        Some(label),
        EVENT_ACTION,
        Some(EVENT_FIELD),
        None,
        Some("1.5050"),
        Some(details),
        None,
    )
    .unwrap();
    conn.execute(
        "UPDATE events SET timestamp = ?1 WHERE id = ?2",
        rusqlite::params![at, id],
    )
    .unwrap();
}

fn seeded_home() -> TempDir {
    let home = TempDir::new().unwrap();
    tapectl(home.path(), &["init", "--operator", "op", "--no-escrow"]);
    let conn = rusqlite::Connection::open(home.path().join("tapectl.db")).unwrap();
    let a = Assessment {
        ratio: FeedRatio::new(1_505, 1_000_000_000).unwrap(),
        suppressed: None,
    };
    // Recorded before #488: the details carry no figure, the journal does.
    let (old, old_contact) = written(&conn, "L6-OLD", "HP");
    record(
        &conn,
        old,
        "L6-OLD",
        &a.details_json(old_contact),
        "2026-10-14 09:00:00",
    );
    // Recorded since #488: the event carries 9 (its journal row says 5, so
    // the reader provably prefers the event's own figure).
    let (new, new_contact) = written(&conn, "L6-NEW", "HP");
    let wear = HpPerformance {
        repositions_per_100mb: Some(9),
        ..HpPerformance::default()
    };
    record(
        &conn,
        new,
        "L6-NEW",
        &a.details_json_with_wear(new_contact, Some(&wear)),
        "2026-10-14 10:00:00",
    );
    // Not an HP drive: the same bytes decode to nothing.
    let (ibm, ibm_contact) = written(&conn, "L6-IBM", "IBM");
    record(
        &conn,
        ibm,
        "L6-IBM",
        &a.details_json(ibm_contact),
        "2026-10-14 11:00:00",
    );
    home
}

#[test]
fn report_events_shows_the_repositions_beside_the_feed_ratio() {
    let home = seeded_home();
    let text = stdout(home.path(), &["report", "events", "--entity", "volume"]);
    let line = |label: &str| {
        text.lines()
            .find(|l| l.contains(&format!("volume/{label} write_feed_ratio")))
            .unwrap_or_else(|| panic!("no {label} line in:\n{text}"))
            .to_string()
    };
    assert_eq!(
        line("L6-OLD"),
        "  2026-10-14 09:00:00 volume/L6-OLD write_feed_ratio.native_per_data_byte: \
         (none) \u{2192} 1.5050, repositions per 100 MB: 5"
    );
    assert!(
        line("L6-NEW").ends_with("\u{2192} 1.5050, repositions per 100 MB: 9"),
        "{text}"
    );
    // Positive control for the clause's absence: the line as it read
    // before #488.
    assert_eq!(
        line("L6-IBM"),
        "  2026-10-14 11:00:00 volume/L6-IBM write_feed_ratio.native_per_data_byte: \
         (none) \u{2192} 1.5050"
    );

    let json: serde_json::Value = serde_json::from_slice(
        &tapectl(
            home.path(),
            &["report", "events", "--entity", "volume", "--json"],
        )
        .stdout,
    )
    .unwrap();
    let ev = |label: &str| {
        json.as_array()
            .unwrap()
            .iter()
            .find(|e| e["entity_label"] == label && e["action"] == EVENT_ACTION)
            .unwrap()
            .clone()
    };
    assert_eq!(ev("L6-OLD")["repositions_per_100mb"], 5);
    assert_eq!(ev("L6-OLD")["repositions_from"], "journal");
    assert!(
        ev("L6-OLD")["details"]
            .get("repositions_per_100mb")
            .is_none(),
        "details stays the row as stored"
    );
    assert_eq!(ev("L6-NEW")["repositions_per_100mb"], 9);
    assert_eq!(ev("L6-NEW")["repositions_from"], "event");
    assert!(ev("L6-IBM").get("repositions_per_100mb").is_none());
}

#[test]
fn report_health_and_volume_info_show_it_too() {
    let home = seeded_home();
    let health = stdout(home.path(), &["report", "health"]);
    assert!(
        health.contains("L6-OLD: 1.5050 (above 1.05")
            && health.contains("), repositions per 100 MB: 5, 2026-10-14 09:00:00"),
        "{health}"
    );
    let info = stdout(home.path(), &["volume", "info", "L6-NEW"]);
    assert!(
        info.contains("), repositions per 100 MB: 9, recorded 2026-10-14 10:00:00\n"),
        "{info}"
    );
    let ibm = stdout(home.path(), &["volume", "info", "L6-IBM"]);
    // No figure, so no clause. (Since ADR-0012 item 170 the verdict itself
    // may say there are "no repositions ... on record", so the clause is
    // looked for, not the word.)
    assert!(!ibm.contains("repositions per 100 MB"), "{ibm}");
    assert!(
        ibm.contains("no cause named: no repositions and no correction jump on record"),
        "positive control: the line is there, without the clause: {ibm}"
    );
}
