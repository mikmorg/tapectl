//! Issue #414: `first-run.sh` step 13 can be re-entered after an interrupted
//! write. `scripts/lib/volume-state.sh` reads `tapectl volume info <label>
//! --json` and says what is left to do; the step resumes an interrupted
//! session and runs the post-write block for a sealed volume instead of
//! dying on both.
//!
//! Sources the library the way `first_run_drive_generation.rs` does — the
//! shell function is what the script runs — and builds the `volume info`
//! JSON through the real binary for the fresh case, so the keys it reads
//! are the keys tapectl writes. Ungated: no device, no tape, no mhvtl.

use std::path::Path;
use std::process::Command;

use tempfile::TempDir;

/// Run `volume_step13_state <file>` on `json` and return what it printed.
fn state_of(json: &str) -> String {
    let dir = TempDir::new().unwrap();
    let f = dir.path().join("info.json");
    std::fs::write(&f, json).unwrap();
    state_of_file(&f)
}

fn state_of_file(f: &Path) -> String {
    let out = Command::new("bash")
        .arg("-c")
        .arg(". scripts/lib/volume-state.sh; volume_step13_state \"$1\"")
        .arg("bash")
        .arg(f)
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .output()
        .expect("bash must be available");
    assert!(out.status.success(), "the function itself must not fail");
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

/// A `volume info --json` document with `status`, `condition` and the given
/// `writes[].status` values — the fields the classifier reads, spelled as
/// `cli::volume::VolumeInfo` serialises them.
fn info(status: &str, condition: &str, writes: &[&str]) -> String {
    let writes: Vec<serde_json::Value> = writes
        .iter()
        .map(|s| {
            serde_json::json!({
                "unit": "u", "version": 1, "status": s,
                "started_at": null, "completed_at": null,
                "num_slices": 1, "bytes": 1
            })
        })
        .collect();
    serde_json::json!({
        "label": "L6-0001",
        "status": status,
        "condition": condition,
        "writes": writes,
        "verifications": [],
    })
    .to_string()
}

#[test]
fn an_unwritten_volume_goes_to_the_write() {
    assert_eq!(state_of(&info("initialized", "ok", &[])), "fresh");
}

#[test]
fn an_interrupted_session_goes_to_volume_resume() {
    assert_eq!(
        state_of(&info("initialized", "ok", &["interrupted"])),
        "resume",
        "an `interrupted` writes row is what `volume write` refuses and `volume resume` takes"
    );
    // Several units in one session: one interrupted row is enough, and it
    // wins over the others (`session::resume_admission`'s order).
    assert_eq!(
        state_of(&info(
            "initialized",
            "ok",
            &["completed", "planned", "interrupted"]
        )),
        "resume"
    );
}

/// The two unresolved states `volume resume` itself refuses get their own
/// answer, so the step names the right remedy instead of a resume that fails.
#[test]
fn a_live_or_merely_planned_session_is_not_resumed() {
    // `volume info` opened the catalog, which sweeps a crashed `in_progress`
    // to `interrupted`: one still `in_progress` has a live writer.
    assert_eq!(
        state_of(&info("initialized", "ok", &["in_progress"])),
        "live"
    );
    assert_eq!(
        state_of(&info("initialized", "ok", &["planned"])),
        "planned"
    );
}

#[test]
fn a_sealed_volume_goes_to_the_post_write_block() {
    assert_eq!(
        state_of(&info("sealed", "ok", &["completed", "completed"])),
        "sealed"
    );
}

#[test]
fn everything_else_stops() {
    // A quarantined volume — sealed or not — is never written, resumed or
    // treated as done.
    assert_eq!(
        state_of(&info("sealed", "quarantined", &["completed"])),
        "other"
    );
    assert_eq!(state_of(&info("initialized", "quarantined", &[])), "other");
    for status in ["retired", "erased", "full", "active"] {
        assert_eq!(state_of(&info(status, "ok", &["completed"])), "other");
    }
    // Every session ended `aborted`: neither write nor a plain resume.
    assert_eq!(state_of(&info("initialized", "ok", &["aborted"])), "other");
    // Unreadable input never reads as "fresh".
    assert_eq!(state_of("not json"), "other");
    assert_eq!(state_of(""), "other");
}

/// The keys come from the real binary: a volume created by `volume init`'s
/// catalog half has to read as `fresh` from tapectl's own JSON. Built by
/// inserting the row the way `volume init` leaves it (no drive here).
#[test]
fn the_real_volume_info_json_of_an_unwritten_volume_reads_fresh() {
    let home = TempDir::new().unwrap();
    let init = Command::new(env!("CARGO_BIN_EXE_tapectl"))
        .args(["--home"])
        .arg(home.path())
        .args(["init", "--operator", "op", "--no-escrow"])
        .output()
        .unwrap();
    assert!(
        init.status.success(),
        "init: {}",
        String::from_utf8_lossy(&init.stderr)
    );
    let conn = rusqlite::Connection::open(home.path().join("tapectl.db")).unwrap();
    conn.execute(
        "INSERT INTO volumes (label, backend_type, backend_name, capacity_bytes, status)
         VALUES ('L6-0001', 'lto', 'lto0', 2500000000000, 'initialized')",
        [],
    )
    .unwrap();
    drop(conn);
    let out = Command::new(env!("CARGO_BIN_EXE_tapectl"))
        .args(["--home"])
        .arg(home.path())
        .args(["volume", "info", "L6-0001", "--json"])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "volume info: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let f = home.path().join("info.json");
    std::fs::write(&f, &out.stdout).unwrap();
    assert_eq!(state_of_file(&f), "fresh");

    // And a volume that does not exist is refused with the text step 13
    // reads as "absent".
    let missing = Command::new(env!("CARGO_BIN_EXE_tapectl"))
        .args(["--home"])
        .arg(home.path())
        .args(["volume", "info", "NOPE", "--json"])
        .output()
        .unwrap();
    assert!(!missing.status.success());
    assert!(
        String::from_utf8_lossy(&missing.stderr).contains("volume not found"),
        "first-run.sh greps this text: {}",
        String::from_utf8_lossy(&missing.stderr)
    );
}

/// Step 13 itself: the resume and sealed branches exist and the write path
/// is skipped for them. Before #414 the step had no `volume resume` at all
/// and died on any existing label that was not `initialized`.
#[test]
fn step_13_resumes_and_finishes_instead_of_dying() {
    let script =
        std::fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("scripts/first-run.sh"))
            .unwrap();
    let step13 = script
        .split("# ================================================================ step 13")
        .nth(1)
        .and_then(|s| {
            s.split("# ================================================================ step 14")
                .next()
        })
        .expect("step 13 is delimited");
    assert!(
        step13.contains("volume_step13_state"),
        "step 13 classifies the label"
    );
    assert!(
        step13.contains("run tc volume resume \"$LABEL\""),
        "an interrupted session is resumed"
    );
    assert!(
        step13.contains("if [ \"$VSTATE\" = absent ] || [ \"$VSTATE\" = fresh ]; then"),
        "only an absent or unwritten volume goes through snapshot/stage/init/write"
    );
    let write_at = step13.find("run tc volume write").unwrap();
    let verify_at = step13.find("tc volume verify \"$LABEL\"").unwrap();
    let fi_between = step13[write_at..verify_at].contains("\nfi\n");
    assert!(
        fi_between,
        "the post-write block (verify --full onwards) runs for every state, after the write path closes"
    );
}
