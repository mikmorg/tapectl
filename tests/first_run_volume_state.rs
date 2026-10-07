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

/// Run `volume_aborted_seal_recorded <file>` and return what it printed.
fn aborted_seal_recorded_of_file(f: &Path) -> String {
    let out = Command::new("bash")
        .arg("-c")
        .arg(". scripts/lib/volume-state.sh; volume_aborted_seal_recorded \"$1\"")
        .arg("bash")
        .arg(f)
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .output()
        .expect("bash must be available");
    assert!(out.status.success(), "the function itself must not fail");
    String::from_utf8_lossy(&out.stdout).trim().to_string()
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

/// `tapectl volume info <label> --json` for `home`, written to a file.
fn real_volume_info(home: &Path, label: &str) -> std::path::PathBuf {
    let out = Command::new(env!("CARGO_BIN_EXE_tapectl"))
        .args(["--home"])
        .arg(home)
        .args(["volume", "info", label, "--json"])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "volume info: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let f = home.join(format!("info-{label}.json"));
    std::fs::write(&f, &out.stdout).unwrap();
    f
}

/// The classifier's `writes[].status` key, and the seal helper's
/// `sealed_at`, pinned against the real binary: a volume with an
/// `interrupted` session reads `resume`; once that session is `aborted` it
/// reads `other`, and whether its seal is recorded decides what step 13's
/// `other` branch tells the operator (ADR-0012 2026-09-23: a clean full
/// verify, then `volume resume`, re-confirms an aborted session whose seal
/// is recorded).
#[test]
fn the_real_volume_info_json_of_a_written_volume_classifies() {
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
    conn.execute_batch(
        "INSERT INTO tenants (name, is_operator, status) VALUES ('acme', 0, 'active');
         INSERT INTO units (uuid, name, tenant_id, current_path, status)
             VALUES ('uuid-1', 'unit1', (SELECT id FROM tenants WHERE name = 'acme'),
                     '/tmp/unit1', 'active');
         INSERT INTO snapshots (unit_id, version, status, source_path, file_count, total_size)
             VALUES ((SELECT id FROM units WHERE name = 'unit1'), 1, 'staged', '/tmp/unit1', 1, 32);
         INSERT INTO stage_sets (snapshot_id, status, slice_size)
             VALUES ((SELECT id FROM snapshots), 'staged', 524288);
         INSERT INTO volumes (label, backend_type, backend_name, capacity_bytes, status)
             VALUES ('L6-0001', 'lto', 'lto0', 2500000000000, 'initialized');
         INSERT INTO writes (stage_set_id, snapshot_id, volume_id, status)
             VALUES ((SELECT id FROM stage_sets), (SELECT id FROM snapshots),
                     (SELECT id FROM volumes), 'interrupted');",
    )
    .unwrap();
    let f = real_volume_info(home.path(), "L6-0001");
    assert_eq!(state_of_file(&f), "resume");
    assert_eq!(aborted_seal_recorded_of_file(&f), "no", "not aborted");

    conn.execute("UPDATE writes SET status = 'aborted'", [])
        .unwrap();
    let f = real_volume_info(home.path(), "L6-0001");
    assert_eq!(state_of_file(&f), "other");
    assert_eq!(
        aborted_seal_recorded_of_file(&f),
        "no",
        "aborted before its seal: nothing to re-confirm"
    );

    conn.execute("UPDATE volumes SET sealed_at = datetime('now')", [])
        .unwrap();
    let f = real_volume_info(home.path(), "L6-0001");
    assert_eq!(state_of_file(&f), "other");
    assert_eq!(aborted_seal_recorded_of_file(&f), "yes");
}

/// Step 13's `other` branch: an aborted session whose seal is recorded is
/// sent to `volume verify --full` and then `volume resume` (ADR-0012
/// 2026-09-23), not to a new label — that cartridge is sealed and would
/// still count once re-confirmed.
#[test]
fn an_aborted_session_with_its_seal_recorded_is_sent_to_verify_then_resume() {
    let script =
        std::fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("scripts/first-run.sh"))
            .unwrap();
    let other = script
        .split("\n    *)\n")
        .nth(1)
        .and_then(|s| s.split(";;").next())
        .expect("step 13 has an `other` branch");
    assert!(
        other.contains("volume_aborted_seal_recorded \"$VINFO\""),
        "{other}"
    );
    let verify_at = other
        .find("tapectl volume verify $LABEL --device $DEVICE --full")
        .expect("names the full verify");
    let resume_at = other
        .find("tapectl volume resume $LABEL --device $DEVICE")
        .expect("then the resume");
    assert!(verify_at < resume_at, "verify first: {other}");
}

/// The post-write verify: a refusal that the loaded tape is not this
/// volume (`binding::corroborate_volume`'s "wrong tape:"/"wrong
/// cartridge:") says a different cartridge is loaded — checked before the
/// failure is classified, so it is never told to clean the drive.
#[test]
fn a_wrong_cartridge_at_the_post_write_verify_is_named_not_blamed_on_the_drive() {
    let script =
        std::fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("scripts/first-run.sh"))
            .unwrap();
    let after_verify = script
        .split("tc volume verify \"$LABEL\"")
        .nth(1)
        .expect("the post-write verify");
    let wrong_at = after_verify
        .find("grep -E \"wrong (tape|cartridge):\" \"$VERIFY_ERR\"")
        .expect("the verify's stderr is checked for a wrong tape");
    let classify_at = after_verify.find("VERIFY_QUAR=").expect("then classified");
    assert!(wrong_at < classify_at);
    assert!(
        after_verify[wrong_at..classify_at].contains("a different cartridge"),
        "{}",
        &after_verify[wrong_at..classify_at]
    );
    // And the wording it greps is what the binary says.
    let binding = std::fs::read_to_string(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("src/volume/binding.rs"),
    )
    .unwrap();
    assert!(binding.contains("\"wrong tape: this command names volume"));
    assert!(binding.contains("\"wrong cartridge: volume"));
}

/// The `planned` branch's remedy must lead somewhere. `volume abort` turns
/// the session's rows `aborted`, and a label whose sessions are all aborted
/// classifies as `other` (asserted in `everything_else_stops`), which step
/// 13 stops on; and `volume write` on that label could not plan anyway —
/// `writes` is `UNIQUE(stage_set_id, volume_id)` and the aborted row stays.
/// So after the abort the operator must re-run with a NEW label: `volume
/// init` on the same cartridge then displaces the old row to `erased`.
#[test]
fn a_planned_session_is_cleared_and_written_under_a_new_label() {
    // The state the remedy leaves is one step 13 refuses to write again.
    assert_eq!(state_of(&info("initialized", "ok", &["aborted"])), "other");

    let script =
        std::fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("scripts/first-run.sh"))
            .unwrap();
    let planned = script
        .split("\n    planned)\n")
        .nth(1)
        .and_then(|s| s.split(";;").next())
        .expect("step 13 has a `planned` branch");
    assert!(
        planned.contains("tapectl volume abort $LABEL"),
        "the branch names the abort: {planned}"
    );
    assert!(
        !planned.contains("--label $LABEL"),
        "re-running with the same label after the abort is a dead end: {planned}"
    );
    assert!(
        planned.contains("--from 13 --label <new>"),
        "the branch names a new label: {planned}"
    );

    let install =
        std::fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("docs/install.md"))
            .unwrap();
    let flat = install.split_whitespace().collect::<Vec<_>>().join(" ");
    assert!(
        !flat.contains("`tapectl volume abort <label>`, then re-run)"),
        "docs/install.md §5 must not send the operator back to the same label"
    );
    assert!(
        flat.contains("`tapectl volume abort <label>`, then re-run with a new label"),
        "docs/install.md §5 names the new label"
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
