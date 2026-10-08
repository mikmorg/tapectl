//! `db stats` reports each table's size (issue #310, ADR-0012 amendment
//! 2026-10-07 item 28): the journals are never pruned, so the operator must
//! be able to see which table the catalog's growth is made of — and whether
//! a figure was measured or estimated.

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
    assert_eq!(tables[0]["rows"], 200);
    assert!(tables[0]["bytes"].as_i64().unwrap() > 200 * 3600);
    assert!(tables.iter().any(|t| t["name"] == "tenants"));
    // The keys `db stats --json` had before stay.
    assert!(v["size_bytes"].as_i64().unwrap() > 0);
    assert!(v["tables"].as_i64().unwrap() as usize >= tables.len());
}
