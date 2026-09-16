//! Issue #178: `scripts/lib/drive-generation.sh` derives the DRIVE's own
//! generation from its INQUIRY product identification.
//!
//! Shells out to bash and sources the library, the way `tests/mhvtl_e2e.rs`
//! runs `scripts/mhvtl-device.sh` — the shell function is the artifact
//! `first-run.sh` uses, so the shell function is what gets pinned. Ungated:
//! no device, no tape, no mhvtl.

use std::process::Command;

/// Run `drive_generation_from_model <model>` and return (stdout, exit ok).
fn generation_for(model: &str) -> (String, bool) {
    let script = format!(
        ". scripts/lib/drive-generation.sh; drive_generation_from_model {}",
        shell_quote(model)
    );
    let out = Command::new("bash")
        .arg("-c")
        .arg(&script)
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .output()
        .expect("bash must be available");
    (
        String::from_utf8_lossy(&out.stdout).trim().to_string(),
        out.status.success(),
    )
}

fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

#[test]
fn recognised_drive_models_yield_their_own_generation() {
    // IBM, as mhvtl and the LTO-6 passthrough report them; HP/HPE as the real
    // drive does. `TD10` is the reason the pattern takes one-or-more digits:
    // a `[0-9]` pattern would read it as generation 1.
    let table = [
        ("ULT3580-TD6", "LTO-6"),
        ("ULT3580-TD8", "LTO-8"),
        ("ULT3580-HH5", "LTO-5"),
        ("ULT3580-TD10", "LTO-10"),
        ("Ultrium 6-SCSI", "LTO-6"),
        ("Ultrium 9-SCSI", "LTO-9"),
    ];
    for (model, want) in table {
        let (got, ok) = generation_for(model);
        assert!(ok, "{model} must be recognised (rc 0)");
        assert_eq!(got, want, "model {model}");
    }
}

#[test]
fn every_recognised_answer_round_trips_generation_parse() {
    // The string this script writes into config.toml must be one tapectl can
    // read back; otherwise first-run hands `backend add` a value it refuses.
    for model in [
        "ULT3580-TD6",
        "ULT3580-TD8",
        "ULT3580-HH5",
        "Ultrium 6-SCSI",
        "Ultrium 9-SCSI",
    ] {
        let (got, ok) = generation_for(model);
        assert!(ok);
        assert!(
            tapectl::media::Generation::parse(&got).is_some(),
            "{model} produced {got:?}, which Generation::parse rejects"
        );
    }
}

#[test]
fn an_unrecognised_model_yields_nothing_and_fails() {
    // The whole point: first-run must ASK or DIE, never guess. An empty
    // string with rc 0 would be guessed at by the caller's `${VAR:-default}`.
    for model in ["VTL-FOO", "Ultrium-HH", "", "QUANTUM SuperLoader"] {
        let (got, ok) = generation_for(model);
        assert!(
            got.is_empty(),
            "model {model:?} must print nothing, got {got:?}"
        );
        assert!(!ok, "model {model:?} must exit non-zero");
    }
}

#[test]
fn type_m_is_never_a_drive_generation() {
    // LTO-7-M8 is a CARTRIDGE format. `src/media.rs` says `Lto7M8` is never a
    // drive generation and `can_write(Lto7M8, _)` is false for every medium,
    // so a backend declared that way refuses every `volume init` (issue #186).
    for model in ["ULT3580-TD7-M8", "Ultrium 7-M8-SCSI"] {
        let (got, ok) = generation_for(model);
        assert!(
            got.is_empty(),
            "model {model:?} must not yield a generation"
        );
        assert!(!ok);
    }
}
