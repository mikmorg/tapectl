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

/// [`info`] with `sealed_at` set, as `volume info --json` gives it once the
/// seal is recorded.
fn info_sealed_at(status: &str, condition: &str, writes: &[&str]) -> String {
    let mut v: serde_json::Value = serde_json::from_str(&info(status, condition, writes)).unwrap();
    v["sealed_at"] = serde_json::json!("2026-11-04 20:00:00");
    v.to_string()
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
    // A session aborted AFTER its seal was recorded is never written again
    // (its `volume resume` route is `volume_aborted_seal_recorded`'s).
    assert_eq!(
        state_of(&info_sealed_at("initialized", "ok", &["aborted"])),
        "other"
    );
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
/// reads `fresh` before its seal (issue #401: the next write replaces it) and
/// `other` after, where step 13's `other` branch tells the operator (ADR-0012 2026-09-23: a clean full
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
    // Issue #401: an attempt abandoned before its seal is replaced by the
    // next write, so the label reads `fresh` (the real JSON's `sealed_at` is
    // null here, which the classifier must read as "not sealed").
    assert_eq!(state_of_file(&f), "fresh");
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
        .find("$OP_CMD volume verify $LABEL --device $DEVICE --full")
        .expect("names the full verify");
    let resume_at = other
        .find("$OP_CMD volume resume $LABEL --device $DEVICE")
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

/// The `planned` branch's remedy must lead somewhere (issue #401). `volume
/// abort` turns the session's rows `aborted`; a label whose every session
/// ended `aborted` or `failed` before its seal reads `fresh`, because the
/// next `volume write` replaces the abandoned attempt. So the remedy is the
/// SAME label again, and step 13 writes it (asking first, as for any fresh
/// volume). Before #401 the aborted row blocked the plan and the remedy was
/// a new label; this test pinned that until 1.9.0.
#[test]
fn a_planned_session_is_cleared_and_written_again_under_the_same_label() {
    // The state the remedy leaves is one step 13 writes.
    assert_eq!(state_of(&info("initialized", "ok", &["aborted"])), "fresh");
    assert_eq!(
        state_of(&info("initialized", "ok", &["aborted", "failed"])),
        "fresh"
    );
    // ...but never past a seal, nor beside a session still open or done.
    assert_eq!(
        state_of(&info_sealed_at("initialized", "ok", &["aborted"])),
        "other"
    );
    assert_eq!(
        state_of(&info("initialized", "ok", &["aborted", "completed"])),
        "other"
    );

    let script =
        std::fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("scripts/first-run.sh"))
            .unwrap();
    let planned = script
        .split("\n    planned)\n")
        .nth(1)
        .and_then(|s| s.split(";;").next())
        .expect("step 13 has a `planned` branch");
    assert!(
        // `$OP_CMD`: the command an operator can run in service-user mode (#469).
        planned.contains("$OP_CMD volume abort $LABEL"),
        "the branch names the abort: {planned}"
    );
    assert!(
        planned.contains("--from 13 --label $LABEL"),
        "the branch names the same label: {planned}"
    );
    assert!(
        !planned.contains("--label <new>"),
        "a new label is no longer needed after the abort (#401): {planned}"
    );

    let install =
        std::fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("docs/install.md"))
            .unwrap();
    let flat = install.split_whitespace().collect::<Vec<_>>().join(" ");
    assert!(
        flat.contains("`tapectl volume abort <label>`, then re-run this step with the same label"),
        "docs/install.md §5 names the same label"
    );
    assert!(
        !flat.contains("then re-run with a new label"),
        "docs/install.md §5 must not send the operator to a new label"
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
    // 1.9.0 renamed `host check --unit` to `--systemd-unit` and refuses the
    // old spelling by name (exit 2): the quiet-host loop could never pass.
    assert!(
        step13.contains("HC_ARGS+=(--systemd-unit \"$u\")") && !step13.contains("HC_ARGS+=(--unit"),
        "step 13's host check uses 1.9.0's --systemd-unit"
    );
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

// ---------------------------------------------------------------- step 13, run

/// Step 13 run for real (`--from 13 --to 13 --auto`), with a stub `tapectl`
/// that answers each command from this rig's scenario files and records every
/// call. No drive: `/dev/null` is the device, and the tape lock is the rig's
/// own file (`TAPECTL_TAPE_LOCK`), so a gate on the same host is never in the
/// way. `sudo` is a stub that records and does nothing.
struct Step13 {
    tmp: TempDir,
}

/// The stub `tapectl`. `--home H` is dropped; each command prints what the
/// real one would for what step 13 reads, from `$STUB_SCEN`:
/// `volume-info.json` (absent: "volume not found", exit 2), `init.out` and
/// `init.rc`, `write.rc`, `resume.rc`, `verify-status.json` and
/// `verify-status.rc`, `verify.rc`, `volumes` (the catalog's count).
const STUB_TAPECTL: &str = r#"#!/bin/bash
S="$STUB_SCEN"
printf '%s\n' "$*" >> "$S/calls"
[ "$1" = --home ] && shift 2
rc_of() { cat "$S/$1" 2>/dev/null || echo 0; }
case "$1 $2" in
  "--version "*) echo "tapectl 1.9.0 (a step-13 stub)" ;;
  "catalog stats") echo "{\"volumes\": $(cat "$S/volumes" 2>/dev/null || echo 0)}" ;;
  "location list") echo '[]' ;;
  "drive status")
    case " $* " in *" --json "*) echo '{"tape_node":{"cartridge":"loaded"}}' ;; *) echo "drive: a cartridge is loaded" ;; esac ;;
  "host check") echo "host check: quiet" ;;
  "volume info")
    if [ -f "$S/volume-info.json" ]; then cat "$S/volume-info.json"
    else echo "error: volume not found: $3" >&2; exit 2; fi ;;
  "unit list") echo '[{"name":"keepsake/a"}]' ;;
  "snapshot create") echo "unit \"$3\" is unchanged since v1; no snapshot created" ;;
  "snapshot list") echo '[]' ;;
  "stage list")
    case " $* " in *" --json "*) echo '[{"unit":"keepsake/a"}]' ;; *) echo "keepsake/a v1 staged" ;; esac ;;
  "volume plan") echo "1 tape (an estimate)" ;;
  "volume init") cat "$S/init.out" 2>/dev/null; exit "$(rc_of init.rc)" ;;
  "cartridge erase")
    case " $* " in *" --json "*) echo '{"confirm_label": null}' ;; *) echo "the tape holds a volume this catalog does not know" ;; esac ;;
  "volume write") rc="$(rc_of write.rc)"; [ "$rc" = 0 ] && echo "volume \"$3\" write completed"; exit "$rc" ;;
  "volume resume") rc="$(rc_of resume.rc)"; [ "$rc" = 0 ] && echo "volume \"$3\" write resumed and completed"; exit "$rc" ;;
  "report verify-status") cat "$S/verify-status.json" 2>/dev/null || echo '[]'; exit "$(rc_of verify-status.rc)" ;;
  "volume verify") echo '{"quarantined": false}'; exit "$(rc_of verify.rc)" ;;
  "volume move") echo "volume \"$3\" moved to \"$5\"" ;;
  "audit") echo "audit: copy_count (advisory)"; exit 2 ;;
  "key escrow-kit") mkdir -p "$3" && echo cover > "$3/COVER.txt" ;;
  *) echo "stub tapectl: unexpected: $*" >&2; exit 99 ;;
esac
"#;

const LABEL: &str = "L8-TEST";

impl Step13 {
    fn new() -> Self {
        let tmp = TempDir::new().unwrap();
        for d in ["h", "state/tapectl", "work", "bin", "scen"] {
            std::fs::create_dir_all(tmp.path().join(d)).unwrap();
        }
        let rig = Step13 { tmp };
        rig.exe("bin/tapectl", STUB_TAPECTL);
        rig.exe(
            "bin/sudo",
            &format!(
                "#!/bin/sh\necho \"sudo $*\" >> {}\nexit 0\n",
                rig.p("scen/sudo-calls").display()
            ),
        );
        // Step 13 refuses under --auto without a green rehearsal of this
        // binary: the marker step 12 leaves, keyed by the binary's sha256.
        let sha = Command::new("sha256sum")
            .arg(rig.p("bin/tapectl"))
            .output()
            .expect("sha256sum");
        let sha = String::from_utf8_lossy(&sha.stdout)[..16].to_string();
        std::fs::write(
            rig.p(&format!("state/tapectl/rehearsal-ok-{sha}")),
            "a stub rehearsal\n",
        )
        .unwrap();
        rig
    }
    fn p(&self, rel: &str) -> std::path::PathBuf {
        self.tmp.path().join(rel)
    }
    fn exe(&self, rel: &str, body: &str) {
        use std::os::unix::fs::PermissionsExt;
        std::fs::write(self.p(rel), body).unwrap();
        std::fs::set_permissions(self.p(rel), std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    /// A scenario file the stub reads.
    fn scen(&self, name: &str, body: &str) -> &Self {
        std::fs::write(self.p(&format!("scen/{name}")), body).unwrap();
        self
    }
    /// `first-run.sh --from 13 --to 13 --auto`; the output, and the stub's calls.
    fn run(&self) -> (Output, String) {
        let path = format!(
            "{}:{}",
            self.p("bin").display(),
            std::env::var("PATH").unwrap_or_default()
        );
        let out = Command::new("bash")
            .arg(Path::new(env!("CARGO_MANIFEST_DIR")).join("scripts/first-run.sh"))
            .args(["--no-service-user", "--auto", "--from", "13", "--to", "13"])
            .args(["--work-dir"])
            .arg(self.p("work"))
            .args(["--home"])
            .arg(self.p("home"))
            .args(["--tapectl"])
            .arg(self.p("bin/tapectl"))
            .args([
                "--device",
                "/dev/null",
                "--label",
                LABEL,
                "--location",
                "fr-rack",
            ])
            .env("PATH", path)
            .env("HOME", self.p("h"))
            .env("USER", "fr-tester")
            .env("XDG_STATE_HOME", self.p("state"))
            .env("STUB_SCEN", self.p("scen"))
            .env("TAPECTL_TAPE_LOCK", self.p("tape.lock"))
            .env("TAPECTL_HOME_POINTER", self.p("home-pointer"))
            .env_remove("TAPECTL_HOME")
            .env_remove("TAPECTL_LOCAL_PROFILE")
            .env_remove("SSH_CONNECTION")
            .stdin(std::process::Stdio::null())
            .output()
            .expect("spawn first-run.sh");
        let calls = std::fs::read_to_string(self.p("scen/calls")).unwrap_or_default();
        (out, calls)
    }
}

use std::process::Output;

fn texts(o: &Output) -> (String, String) {
    (
        String::from_utf8_lossy(&o.stdout).into_owned(),
        String::from_utf8_lossy(&o.stderr).into_owned(),
    )
}

/// The calls that ran `words` (e.g. "volume verify"), after the `--home H`.
fn calls_of<'a>(calls: &'a str, words: &str) -> Vec<&'a str> {
    calls
        .lines()
        .map(|l| match l.strip_prefix("--home ") {
            Some(rest) => rest.split_once(' ').map(|(_, r)| r).unwrap_or(""),
            None => l,
        })
        .filter(|l| l.starts_with(words))
        .collect()
}

/// `volume info --json` of a volume in `status` with `writes`.
fn info_json(status: &str, writes: &[&str]) -> String {
    let mut v: serde_json::Value = serde_json::from_str(&info(status, "ok", writes)).unwrap();
    v["label"] = serde_json::json!(LABEL);
    v.to_string()
}

/// init's line for a cartridge it registered (src/volume/write.rs
/// `binding_lines`, ADR-0012 item 219).
fn registered_line() -> String {
    format!(
        "cartridge {LABEL} auto-registered from MAM (chip serial SER-0123; barcode {LABEL}, \
         the volume's label)\nvolume \"{LABEL}\" initialized\n"
    )
}

/// ADR-0012 item 218: one copy loop. The write runs with --full-confirm and
/// --move-to the shelf, no --keep-staging; nothing reads the tape a second
/// time (no `volume verify`), no `stage clean` follows, and the step goes on
/// to the shelf, the audit and the kit. Item 219: init's line is read for the
/// barcode the cartridge took, the label, and no `cartridge relabel` is named.
#[test]
fn step_13_writes_once_and_its_full_confirm_is_the_verify() {
    let rig = Step13::new();
    rig.scen("init.out", &registered_line());
    let (o, calls) = rig.run();
    let (out, err) = texts(&o);
    assert!(o.status.success(), "{out}\n{err}\n{calls}");
    assert_eq!(
        calls_of(&calls, "volume write"),
        vec![format!(
            "volume write {LABEL} --device /dev/null --yes --full-confirm --move-to fr-rack"
        )],
        "{calls}"
    );
    assert!(!calls.contains("--keep-staging"), "{calls}");
    assert!(calls_of(&calls, "volume verify").is_empty(), "{calls}");
    assert!(calls_of(&calls, "stage clean").is_empty(), "{calls}");
    assert!(
        out.contains(&format!(
            "{LABEL} sealed and read back in full: the write's confirm read every file back"
        )),
        "{out}"
    );
    for after in ["volume move", "audit", "key escrow-kit"] {
        assert_eq!(calls_of(&calls, after).len(), 1, "{after}: {calls}");
    }
    assert!(
        out.contains(&format!(
            "cartridge {LABEL} registered from its chip (chip serial SER-0123, its identity); \
             its barcode is {LABEL}: write that on its sticker"
        )),
        "{out}"
    );
    assert!(
        !out.contains("relabel") && !err.contains("relabel"),
        "{out}"
    );
}

/// ADR-0012 item 218: what went wrong is read from the write's exit status
/// (the write family's table), each with its own remedy, and nothing after
/// the write runs: no verify, no kit.
#[test]
fn step_13_reads_how_the_write_ended_from_its_exit_status() {
    let cases = [
        (
            "2",
            "the write refused before anything was written (exit 2)",
        ),
        (
            "3",
            "the write's confirm did not complete (exit 3: the drive or transport, not the medium)",
        ),
        ("4", "write did not seal: interrupted part-way (exit 4)"),
        ("5", "write did not seal: the session aborted (exit 5)"),
        ("6", "the write proved the medium bad (exit 6)"),
        ("75", "the catalog was busy (exit 75)"),
        ("137", "write did not seal (exit 137)"),
    ];
    for (rc, says) in cases {
        let rig = Step13::new();
        rig.scen("init.out", &registered_line())
            .scen("write.rc", rc);
        let (o, calls) = rig.run();
        let (out, err) = texts(&o);
        assert_eq!(o.status.code(), Some(1), "exit {rc}: {out}\n{err}");
        assert!(err.contains(says), "exit {rc}: {err}");
        for none in ["volume verify", "key escrow-kit", "volume move", "audit"] {
            assert!(
                calls_of(&calls, none).is_empty(),
                "exit {rc}, {none}: {calls}"
            );
        }
    }
    // The positive control of the remedies' differences: exit 3 is a sealed
    // tape whose re-run resumes, exit 6 a medium to replace.
    let rig = Step13::new();
    rig.scen("init.out", &registered_line())
        .scen("write.rc", "3");
    let (o, _) = rig.run();
    assert!(texts(&o)
        .0
        .contains("so L8-TEST is untouched and still in service"));
}

/// The resume keeps what the write asked for: --full-confirm and the shelf,
/// no --keep-staging, and it is classified the same way.
#[test]
fn step_13_resumes_with_a_full_confirm_and_reads_its_exit_status() {
    let rig = Step13::new();
    rig.scen(
        "volume-info.json",
        &info_json("initialized", &["interrupted"]),
    )
    .scen("volumes", "1");
    let (o, calls) = rig.run();
    let (out, err) = texts(&o);
    assert!(o.status.success(), "{out}\n{err}\n{calls}");
    assert_eq!(
        calls_of(&calls, "volume resume"),
        vec![format!(
            "volume resume {LABEL} --device /dev/null --full-confirm --move-to fr-rack"
        )],
        "{calls}"
    );
    assert!(calls_of(&calls, "volume write").is_empty(), "{calls}");
    assert!(calls_of(&calls, "volume verify").is_empty(), "{calls}");
    assert!(
        out.contains(&format!(
            "{LABEL} sealed and read back in full: the resume's confirm read every file back"
        )),
        "{out}"
    );

    let rig = Step13::new();
    rig.scen(
        "volume-info.json",
        &info_json("initialized", &["interrupted"]),
    )
    .scen("volumes", "1")
    .scen("resume.rc", "6");
    let (o, calls) = rig.run();
    let (_, err) = texts(&o);
    assert_eq!(o.status.code(), Some(1), "{err}");
    assert!(
        err.contains("the resume proved the medium bad (exit 6)"),
        "{err}"
    );
    assert!(calls_of(&calls, "key escrow-kit").is_empty(), "{calls}");
}

/// A `report verify-status --volume L --json` row.
fn verify_row(kind: &str, outcome: &str, completed: &str) -> serde_json::Value {
    serde_json::json!({
        "volume": LABEL, "never_verified": false, "type": kind, "outcome": outcome,
        "started": completed, "completed": completed, "checked": 4, "passed": 4,
        "failed": 0, "write_lbp": "on", "write_lbp_note": null, "no_full_verify": false,
    })
}

/// OQ15: a sealed re-entry reads the tape again only when no full readback
/// is recorded for it. Recorded (the write's full confirm): `report
/// verify-status` is asked, nothing is read, and the step goes on.
#[test]
fn a_sealed_reentry_with_a_full_readback_recorded_reads_nothing() {
    let rig = Step13::new();
    let rows = serde_json::json!([
        verify_row("quick", "passed", "2026-11-03 09:00:00"),
        verify_row("full", "passed", "2026-11-04 10:00:00"),
    ]);
    rig.scen("volume-info.json", &info_json("sealed", &["completed"]))
        .scen("volumes", "1")
        .scen("verify-status.json", &rows.to_string());
    let (o, calls) = rig.run();
    let (out, err) = texts(&o);
    assert!(o.status.success(), "{out}\n{err}\n{calls}");
    assert_eq!(
        calls_of(&calls, "report verify-status"),
        vec![format!("report verify-status --volume {LABEL} --json")],
        "{calls}"
    );
    for none in [
        "volume verify",
        "volume write",
        "volume resume",
        "volume init",
    ] {
        assert!(calls_of(&calls, none).is_empty(), "{none}: {calls}");
    }
    assert!(
        out.contains(&format!(
            "{LABEL} sealed and read back in full: a full readback passed on 2026-11-04 10:00:00"
        )),
        "{out}"
    );
    for after in ["volume move", "audit", "key escrow-kit"] {
        assert_eq!(calls_of(&calls, after).len(), 1, "{after}: {calls}");
    }
}

/// OQ15, the other way: only quick readbacks recorded (or none), or a report
/// that cannot be read, and the step reads the tape (`volume verify --full`).
#[test]
fn a_sealed_reentry_without_a_full_readback_recorded_runs_the_verify() {
    let quick = serde_json::json!([verify_row("quick", "passed", "2026-11-03 09:00:00")]);
    let failed = serde_json::json!([verify_row("full", "failed", "2026-11-03 09:00:00")]);
    for (rows, rc, says) in [
        (
            quick.to_string(),
            "0",
            "no full readback of L8-TEST is recorded",
        ),
        (
            failed.to_string(),
            "0",
            "no full readback of L8-TEST is recorded",
        ),
        (
            "[]".to_string(),
            "0",
            "no full readback of L8-TEST is recorded",
        ),
        (
            "not json".to_string(),
            "0",
            "report verify-status could not say whether a full readback",
        ),
        (
            "[]".to_string(),
            "2",
            "report verify-status could not say whether a full readback",
        ),
    ] {
        let rig = Step13::new();
        rig.scen("volume-info.json", &info_json("sealed", &["completed"]))
            .scen("volumes", "1")
            .scen("verify-status.json", &rows)
            .scen("verify-status.rc", rc);
        let (o, calls) = rig.run();
        let (out, err) = texts(&o);
        assert!(o.status.success(), "{rows}: {out}\n{err}\n{calls}");
        assert!(out.contains(says), "{rows}: {out}");
        assert_eq!(
            calls_of(&calls, "volume verify"),
            vec![format!(
                "volume verify {LABEL} --device /dev/null --full --json"
            )],
            "{rows}: {calls}"
        );
        assert!(
            out.contains(&format!(
                "{LABEL} sealed and read back in full: volume verify --full passed"
            )),
            "{rows}: {out}"
        );
    }
}

/// ADR-0012 item 216: with THIS catalog's own sealed volume loaded, init's
/// refusal offers no erase, and step 13 says to eject it and load a blank:
/// no `cartridge erase` runs. The control: a sealed volume the catalog does
/// not hold goes to the erase dry run as before.
#[test]
fn this_catalogs_own_sealed_volume_is_ejected_never_erased() {
    let sealed = format!(
        "error: refusing to write volume \"{LABEL}\": the loaded cartridge already carries a \
         SEALED volume — a valid seal marker parses at tape position 12. ADR-0003: sealed \
         volumes are immutable, there is no append, and --force cannot override this. "
    );
    let rig = Step13::new();
    rig.scen(
        "init.out",
        &format!(
            "{sealed}It is this catalog's live copy: L8-0001, a copy of 3 units. Leave it as it \
             is: eject it (`tapectl drive eject`) and load a blank cartridge.\n"
        ),
    )
    .scen("init.rc", "2");
    let (o, calls) = rig.run();
    let (out, err) = texts(&o);
    assert_eq!(o.status.code(), Some(1), "{out}\n{err}");
    assert!(
        err.contains("is this catalog's own volume (above): eject it")
            && err.contains("load a blank cartridge"),
        "{err}"
    );
    assert!(calls_of(&calls, "cartridge erase").is_empty(), "{calls}");
    assert!(calls_of(&calls, "volume write").is_empty(), "{calls}");

    let rig = Step13::new();
    rig.scen(
        "init.out",
        &format!(
            "{sealed}This catalog holds no volume of that identity (`tapectl volume identify` \
             shows what the tape says). To reuse the cartridge: `tapectl cartridge erase` erases \
             the tape in the drive, reads it back and records it.\n"
        ),
    )
    .scen("init.rc", "2");
    let (o, calls) = rig.run();
    let (out, err) = texts(&o);
    assert_eq!(o.status.code(), Some(1), "{out}\n{err}");
    assert!(
        !calls_of(&calls, "cartridge erase").is_empty(),
        "the erase route is still taken for a tape the catalog does not hold: {calls}"
    );
    assert!(!err.contains("is this catalog's own volume"), "{err}");
}
