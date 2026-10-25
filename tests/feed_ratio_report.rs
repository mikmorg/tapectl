//! Issue #439: the `write_feed_ratio` event `volume write` records for every
//! write is shown by `volume info` and `report health`, human and `--json`,
//! through the real binary. The reader's own cases (per-row threshold,
//! suppression, what is not a ratio) are unit tests beside it in
//! `src/cli/report.rs`; this file proves the wiring: that both commands call
//! it and print what it returns. Rows are seeded with the writer's own
//! serializer, so a change to what `tape::feed_ratio` records breaks here.
//! Ungated: no device, no tape, no mhvtl.

use std::path::Path;
use std::process::{Command, Output};

use tapectl::db::events;
use tapectl::tape::feed_ratio::{Assessment, FeedRatio, Suppression, EVENT_ACTION, EVENT_FIELD};
use tempfile::TempDir;

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

fn json(home: &Path, args: &[&str]) -> serde_json::Value {
    serde_json::from_slice(&tapectl(home, args).stdout).expect("--json output")
}

/// A volume row; `written` sets `last_write`, the fact `volume info` uses to
/// tell "written, no ratio recorded" from "never written".
fn volume(conn: &rusqlite::Connection, label: &str, written: bool) -> i64 {
    conn.execute(
        "INSERT INTO volumes (label, backend_type, backend_name, media_type, capacity_bytes,
                              status, first_write, last_write)
         VALUES (?1, 'lto', 'lto0', 'LTO-6', 2500000000000, 'sealed', ?2, ?2)",
        rusqlite::params![label, written.then_some("2026-10-14 08:00:00")],
    )
    .unwrap();
    conn.last_insert_rowid()
}

fn record(
    conn: &rusqlite::Connection,
    vid: i64,
    label: &str,
    native_mb: u64,
    suppressed: Option<Suppression>,
    at: &str,
) {
    let a = Assessment {
        ratio: FeedRatio::new(native_mb, 1_000_000_000).unwrap(),
        suppressed,
    };
    let id = events::log_event(
        conn,
        "volume",
        vid,
        Some(label),
        EVENT_ACTION,
        Some(EVENT_FIELD),
        None,
        Some(&format!("{:.4}", a.ratio.ratio())),
        Some(&a.details_json(1)),
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
    let ok = volume(&conn, "L6-OK", true);
    let bad = volume(&conn, "L6-BAD", true);
    let virt = volume(&conn, "L6-VIRT", true);
    volume(&conn, "L6-NONE", true);
    volume(&conn, "L6-NEW", false);
    record(&conn, ok, "L6-OK", 1_005, None, "2026-10-14 09:00:00");
    record(&conn, bad, "L6-BAD", 1_505, None, "2026-10-14 10:00:00");
    record(
        &conn,
        virt,
        "L6-VIRT",
        16_000,
        Some(Suppression::CapacityOverride),
        "2026-10-14 11:00:00",
    );
    home
}

#[test]
fn volume_info_shows_the_write_feed_ratio_or_says_there_is_none() {
    let home = seeded_home();
    let h = home.path();

    let ok = stdout(h, &["volume", "info", "L6-OK"]);
    assert!(
        ok.contains(
            "  Feed ratio:  1.0050 (streamed, within 1.05), recorded 2026-10-14 09:00:00\n"
        ),
        "{ok}"
    );
    let bad = stdout(h, &["volume", "info", "L6-BAD"]);
    // ADR-0012 item 170: no repositions and no corrections recorded, so no
    // cause is named, never the host.
    assert!(
        bad.contains("  Feed ratio:  1.5050 (above 1.05, no cause named"),
        "{bad}"
    );
    let virt = stdout(h, &["volume", "info", "L6-VIRT"]);
    assert!(
        virt.contains("  Feed ratio:  16.0000 (above 1.05, not judged: warning suppressed (capacity_override)"),
        "{virt}"
    );
    // Written, nothing recorded: said. Never written: no line at all.
    let none = stdout(h, &["volume", "info", "L6-NONE"]);
    assert!(none.contains("  Feed ratio:  not recorded"), "{none}");
    let new = stdout(h, &["volume", "info", "L6-NEW"]);
    assert!(!new.contains("Feed ratio"), "{new}");

    let v = json(h, &["--json", "volume", "info", "L6-BAD"]);
    let rows = v["feed_ratios"].as_array().expect("feed_ratios array");
    assert_eq!(rows.len(), 1, "{v}");
    assert_eq!(rows[0]["volume"], "L6-BAD");
    assert_eq!(rows[0]["ratio"], 1.505);
    assert_eq!(rows[0]["threshold"], 1.05);
    assert_eq!(rows[0]["verdict"], "above_threshold");
    assert_eq!(rows[0]["suppressed"], serde_json::Value::Null);
    let virt = json(h, &["--json", "volume", "info", "L6-VIRT"]);
    assert_eq!(
        virt["feed_ratios"][0]["verdict"],
        "above_threshold_suppressed"
    );
    assert_eq!(virt["feed_ratios"][0]["suppressed"], "capacity_override");
    assert_eq!(
        json(h, &["--json", "volume", "info", "L6-NONE"])["feed_ratios"],
        serde_json::json!([])
    );
}

#[test]
fn report_health_lists_uneven_feeds_first_and_narrows_by_volume() {
    let home = seeded_home();
    let h = home.path();

    let text = stdout(h, &["report", "health"]);
    let lines: Vec<&str> = text.lines().collect();
    let head = lines
        .iter()
        .position(|l| l.starts_with("Write feed ratio"))
        .unwrap_or_else(|| panic!("no feed-ratio block:\n{text}"));
    assert!(
        lines[head + 1].starts_with("  !! L6-BAD: 1.5050 (above 1.05"),
        "{text}"
    );
    assert!(
        lines[head + 2].starts_with("     L6-VIRT: 16.0000"),
        "{text}"
    );
    assert!(
        lines[head + 3].starts_with("     L6-OK: 1.0050 (streamed, within 1.05)"),
        "{text}"
    );
    assert_eq!(
        text.matches("!! L6-").count(),
        1,
        "only the uneven feed is flagged:\n{text}"
    );

    let one = stdout(h, &["report", "health", "--volume", "L6-OK"]);
    assert!(one.contains("L6-OK: 1.0050"), "{one}");
    assert!(
        !one.contains("L6-BAD"),
        "--volume narrows the block:\n{one}"
    );

    let v = json(h, &["--json", "report", "health"]);
    let rows = v["feed_ratios"].as_array().expect("feed_ratios array");
    assert_eq!(
        rows.iter()
            .map(|r| r["volume"].as_str().unwrap())
            .collect::<Vec<_>>(),
        ["L6-VIRT", "L6-BAD", "L6-OK"],
        "newest first"
    );
    assert!(v.get("readings").is_some() && v.get("tape_alerts_raised").is_some());
    let one = json(h, &["--json", "report", "health", "--volume", "L6-BAD"]);
    assert_eq!(one["feed_ratios"].as_array().unwrap().len(), 1);
}

#[test]
fn a_catalog_with_no_recorded_ratio_prints_no_block_and_an_empty_array() {
    let home = TempDir::new().unwrap();
    tapectl(home.path(), &["init", "--operator", "op", "--no-escrow"]);
    let text = stdout(home.path(), &["report", "health"]);
    assert!(!text.contains("feed ratio"), "{text}");
    assert_eq!(
        json(home.path(), &["--json", "report", "health"])["feed_ratios"],
        serde_json::json!([])
    );
}

/// `report events --json` carries each event's `details`, parsed when it is
/// JSON: what a command recorded beyond field/old/new (`volume rewrite`'s
/// window in which staging holds the only copy, a feed ratio's assessment)
/// is otherwise readable only from the database.
#[test]
fn report_events_json_carries_each_events_details() {
    let home = seeded_home();
    let ev = json(
        home.path(),
        &["report", "events", "--entity", "volume", "--json"],
    );
    let rows = ev.as_array().expect("an array of events");
    let ratios: Vec<&serde_json::Value> = rows
        .iter()
        .filter(|r| r["action"] == EVENT_ACTION)
        .collect();
    assert_eq!(ratios.len(), 3, "{ev:#}");
    for r in ratios {
        assert!(r["details"].is_object(), "details parsed as JSON: {r:#}");
    }
    let conn = rusqlite::Connection::open(home.path().join("tapectl.db")).unwrap();
    let stored: String = conn
        .query_row(
            "SELECT details FROM events WHERE action = ?1 AND entity_label = 'L6-BAD'",
            [EVENT_ACTION],
            |r| r.get(0),
        )
        .unwrap();
    let bad = rows
        .iter()
        .find(|r| r["entity_label"] == "L6-BAD")
        .expect("L6-BAD's event");
    assert_eq!(
        bad["details"],
        serde_json::from_str::<serde_json::Value>(&stored).unwrap()
    );
}
