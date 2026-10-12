//! `db stats` reports each table's size (issue #310, ADR-0012 amendment
//! 2026-10-07 item 28): the journals are never pruned, so the operator must
//! be able to see which table the catalog's growth is made of — and whether
//! a figure was measured or estimated. The issue's last two pieces: how fast
//! each journal grew over the trailing 90 days, and `db backup` saying how
//! much it copied, since the backup's time is what the growth costs.

use std::path::Path;
use std::process::{Command, Output};

use tempfile::TempDir;

fn run(home: &Path, args: &[&str]) -> Output {
    let out = Command::new(env!("CARGO_BIN_EXE_tapectl"))
        .args(args)
        .env("HOME", home)
        .env_remove("XDG_CONFIG_HOME")
        .env_remove("TAPECTL_HOME")
        .env_remove("TAPECTL_CONFIG")
        .output()
        .expect("failed to spawn tapectl binary");
    assert!(
        out.status.success(),
        "`tapectl {}` failed\nstdout={}\nstderr={}",
        args.join(" "),
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    out
}

/// A home whose `health_logs` is deliberately heavy beside one tenant row.
fn heavy_home() -> TempDir {
    let home = TempDir::new().unwrap();
    run(home.path(), &["init", "--no-escrow"]);
    let conn = tapectl::db::open(&home.path().join(".tapectl").join("tapectl.db")).unwrap();
    let raw = "=== page 0x02 ===\n".repeat(200);
    for _ in 0..200 {
        conn.execute(
            "INSERT INTO health_logs (operation, raw_log) VALUES ('write', ?1)",
            [&raw],
        )
        .unwrap();
    }
    // Rows dated long before the window: weight, but not growth.
    for _ in 0..50 {
        conn.execute(
            "INSERT INTO health_logs (operation, raw_log, logged_at) \
             VALUES ('write', ?1, '2020-01-01 00:00:00')",
            [&raw],
        )
        .unwrap();
    }
    home
}

#[test]
fn db_stats_lists_each_table_largest_first_and_names_the_method() {
    let home = heavy_home();
    let out = run(home.path(), &["db", "stats"]);
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.contains("dbstat"), "the method is named:\n{text}");
    let line_of = |table: &str| {
        text.lines()
            .position(|l| l.split_whitespace().next() == Some(table))
            .unwrap_or_else(|| panic!("no line for {table}:\n{text}"))
    };
    // The positive control: the heavy journal before the one-row table.
    assert!(line_of("health_logs") < line_of("tenants"), "{text}");
}

#[test]
fn db_stats_json_carries_every_table_and_the_method() {
    let home = heavy_home();
    let out = run(home.path(), &["--json", "db", "stats"]);
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).expect("one JSON object");
    assert_eq!(v["size_method"], "dbstat");
    let tables = v["table_sizes"].as_array().expect("table_sizes is a list");
    assert_eq!(tables[0]["name"], "health_logs", "{tables:?}");
    assert_eq!(tables[0]["rows"], 250);
    assert!(tables[0]["bytes"].as_i64().unwrap() > 200 * 3600);
    assert!(tables.iter().any(|t| t["name"] == "tenants"));
    // The keys `db stats --json` had before stay.
    assert!(v["size_bytes"].as_i64().unwrap() > 0);
    assert!(v["tables"].as_i64().unwrap() as usize >= tables.len());
}

#[test]
fn db_stats_names_each_journals_growth_over_the_trailing_90_days() {
    let home = heavy_home();
    let out = run(home.path(), &["db", "stats"]);
    let text = String::from_utf8_lossy(&out.stdout);
    let section = text
        .lines()
        .position(|l| l.starts_with("journal growth"))
        .unwrap_or_else(|| panic!("no journal growth section:\n{text}"));
    let header = text.lines().nth(section).unwrap();
    assert!(header.contains("90 days"), "{header}");
    assert!(
        header.contains("derived"),
        "the bytes are not measured: {header}"
    );
    let logs = text
        .lines()
        .skip(section + 1)
        .find(|l| l.split_whitespace().next() == Some("health_logs"))
        .unwrap_or_else(|| panic!("no health_logs growth line:\n{text}"));
    // 200 rows in the window, not the 250 the table holds.
    assert!(logs.contains(" 200 rows"), "{logs}");
    assert!(logs.contains("/year"), "{logs}");
    // Fastest first: the heavy journal heads the section.
    assert_eq!(
        text.lines()
            .nth(section + 1)
            .unwrap()
            .split_whitespace()
            .next(),
        Some("health_logs"),
        "{text}"
    );
}

#[test]
fn db_stats_json_carries_journal_growth() {
    let home = heavy_home();
    let out = run(home.path(), &["--json", "db", "stats"]);
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).expect("one JSON object");
    let g = &v["journal_growth"];
    assert_eq!(g["trailing_days"], 90, "{g}");
    assert_eq!(
        g["window_days"], 90.0,
        "the catalog has rows from 2020: {g}"
    );
    let journals = g["journals"].as_array().expect("journals is a list");
    assert_eq!(journals.len(), tapectl::db::stats::JOURNALS.len());
    let logs = &journals[0];
    assert_eq!(logs["name"], "health_logs", "{journals:?}");
    assert_eq!(logs["date_column"], "logged_at");
    assert_eq!(logs["rows_in_window"], 200);
    let in_window = logs["bytes_in_window"].as_i64().unwrap();
    assert!(in_window > 200 * 3600, "{logs}");
    assert_eq!(
        logs["bytes_per_year"].as_i64().unwrap(),
        in_window * 365 / 90
    );
    // The window's rows are 4/5 of the table, so its bytes are less than
    // the table's.
    let table = v["table_sizes"].as_array().unwrap()[0]["bytes"]
        .as_i64()
        .unwrap();
    assert!(in_window < table, "{in_window} vs {table}");
}

/// A catalog made a moment ago is paced over one day, not 90, and the text
/// says so; the 90-day home above prints no such note (its positive
/// control).
#[test]
fn db_stats_says_when_the_journals_are_younger_than_the_window() {
    let home = TempDir::new().unwrap();
    run(home.path(), &["init", "--no-escrow"]);
    let out = run(home.path(), &["db", "stats"]);
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(
        text.contains("note: the journals go back only 1.0 days"),
        "{text}"
    );
    let out = run(home.path(), &["--json", "db", "stats"]);
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).expect("one JSON object");
    assert_eq!(v["journal_growth"]["window_days"], 1.0, "{v}");

    let old = heavy_home();
    let text = String::from_utf8_lossy(&run(old.path(), &["db", "stats"]).stdout).into_owned();
    assert!(text.contains("journal growth"), "{text}");
    assert!(!text.contains("note: the journals go back"), "{text}");
}

/// `db backup` prints the size of what it copied, in text and `--json`.
#[test]
fn db_backup_says_how_much_it_copied() {
    let home = heavy_home();
    let dest = home.path().join("backup.db");
    let out = run(
        home.path(),
        &["db", "backup", "--to", dest.to_str().unwrap()],
    );
    let text = String::from_utf8_lossy(&out.stdout);
    let len = std::fs::metadata(&dest).unwrap().len();
    assert!(len > 200 * 3600, "the heavy journal was copied: {len}");
    let shown = tapectl::util::format_bytes_binary(len as i64);
    assert!(
        text.contains(&format!("({shown})")),
        "the size {shown} is printed: {text}"
    );

    let dest2 = home.path().join("backup2.db");
    let out = run(
        home.path(),
        &["--json", "db", "backup", "--to", dest2.to_str().unwrap()],
    );
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).expect("one JSON object");
    assert_eq!(
        v["size_bytes"].as_u64(),
        Some(std::fs::metadata(&dest2).unwrap().len()),
        "{v}"
    );
    assert_eq!(v["keys_bytes"], 0, "{v}");
}

/// With `--include-keys` the keys' size is printed beside their directory.
#[test]
fn db_backup_says_how_much_key_material_it_copied() {
    let home = heavy_home();
    let dest = home.path().join("backup.db");
    let out = run(
        home.path(),
        &[
            "--json",
            "db",
            "backup",
            "--to",
            dest.to_str().unwrap(),
            "--include-keys",
        ],
    );
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).expect("one JSON object");
    let keys = home.path().join("backup.keys");
    let on_disk: u64 = std::fs::read_dir(&keys)
        .unwrap()
        .map(|e| e.unwrap().metadata().unwrap().len())
        .sum();
    assert!(on_disk > 0, "init made the operator's keys");
    assert_eq!(v["keys_bytes"].as_u64(), Some(on_disk), "{v}");

    let dest2 = home.path().join("again.db");
    let out = run(
        home.path(),
        &[
            "db",
            "backup",
            "--to",
            dest2.to_str().unwrap(),
            "--include-keys",
        ],
    );
    let text = String::from_utf8_lossy(&out.stdout);
    let shown = tapectl::util::format_bytes_binary(on_disk as i64);
    assert!(text.contains(&shown), "the keys' size {shown}: {text}");
}
