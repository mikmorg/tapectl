//! `tapectl backend` — populate `[[backends.lto]]` without hand-editing TOML.
//!
//! #126, the follow-up half of #124's first-run cliff. The immediate half made
//! the missing-backend failure self-explanatory and put a commented example in
//! a fresh `init`; this is the command that fills it in.

use crate::cli::BackendCommands;
use crate::config::{Config, TapectlPaths};
use crate::error::{Result, TapectlError};

pub fn run(
    paths: &TapectlPaths,
    command: &BackendCommands,
    json_output: bool,
    dry_run: bool,
) -> Result<()> {
    match command {
        BackendCommands::Add {
            name,
            device_tape,
            device_sg,
            generation,
            capacity_override,
            enospc_buffer,
        } => add(
            paths,
            name,
            device_tape,
            device_sg,
            generation,
            capacity_override.as_deref(),
            enospc_buffer.as_deref(),
            json_output,
            dry_run,
        ),
    }
}

/// The `[[backends.lto]]` table for these values, as the `field=value`
/// list `config add backends.lto` takes (issue #143): `backend add` writes
/// through the same editor, so the two cannot write different tables.
///
/// Pure, so the fields are testable and the command cannot drift from what
/// the tests assert. `block_size` and `hardware_compression` were
/// deliberately absent here while they still parsed (#118, #121 — offering
/// an operator a knob that does nothing is a false assurance); spec W4 has
/// since deleted both from `LtoBackendConfig` entirely, so a table carrying
/// either would now fail to load. `capacity_override` (ADR-0010) is absent
/// unless explicitly given — a real drive's capacity follows the loaded
/// cartridge's detected generation, not this config.
pub fn backend_fields(
    name: &str,
    device_tape: &str,
    device_sg: &str,
    generation: &str,
    capacity_override: Option<&str>,
    enospc_buffer: Option<&str>,
) -> Vec<String> {
    // Each value quoted as a TOML string, so a name such as `2024` stays a
    // string. `Value::from` escapes what needs escaping.
    let q = |v: &str| toml_edit::Value::from(v).to_string().trim().to_string();
    let mut fields = vec![
        format!("name={}", q(name)),
        format!("device_tape={}", q(device_tape)),
        format!("device_sg={}", q(device_sg)),
        format!("generation={}", q(generation)),
    ];
    if let Some(cap) = capacity_override {
        fields.push(format!("capacity_override={}", q(cap)));
    }
    if let Some(buf) = enospc_buffer {
        fields.push(format!("enospc_buffer={}", q(buf)));
    }
    fields
}

/// Plan adding a backend with `fields` to the config text `original`.
pub fn plan_add(
    original: &str,
    config_file: &std::path::Path,
    fields: Vec<String>,
) -> Result<crate::config_edit::Planned> {
    crate::config_edit::plan(
        original,
        config_file,
        &crate::config_edit::Edit::Add {
            key: "backends.lto".to_string(),
            values: fields,
        },
    )
}

#[allow(clippy::too_many_arguments)]
fn add(
    paths: &TapectlPaths,
    name: &str,
    device_tape: &str,
    device_sg: &str,
    generation: &str,
    capacity_override: Option<&str>,
    enospc_buffer: Option<&str>,
    json_output: bool,
    dry_run: bool,
) -> Result<()> {
    crate::naming::validate_backend_name(name)?;

    // Validated here rather than at the next `Config::load`, so a typo is
    // rejected while the operator is still looking at the command that
    // caused it (#59's boundary-validation rule). The canonical spelling
    // (not the operator's raw one) is what gets written to the file, so
    // `LTO6`/`l6`/`LTO-6` all land the same way.
    //
    // `crate::config::validate_drive_generation` is the same check
    // `Config::load`/`config check` applies to an already-written backend
    // (issue #186) — reused here rather than re-stated, so the two boundaries
    // cannot drift into two different refusal messages. It also refuses
    // `"LTO-7-M8"` specifically: that string parses as a real medium
    // generation, but no drive IS an LTO-7-M8 (ADR-0010 decision 1), so a
    // backend declared that way could never write a single tape.
    crate::config::validate_drive_generation(generation).map_err(TapectlError::Other)?;
    let generation = crate::media::Generation::parse(generation)
        .expect("validate_drive_generation already confirmed this parses")
        .as_str();
    if let Some(cap) = capacity_override {
        crate::staging::parse_size_to_bytes(cap)?;
    }
    if let Some(buf) = enospc_buffer {
        crate::staging::parse_size_to_bytes(buf)?;
    }

    let config = Config::load(&paths.config_file)?;
    if config.backends.lto.iter().any(|b| b.name == name) {
        return Err(TapectlError::Other(format!(
            "a backend named \"{name}\" already exists in {}\n\n\
             Names are how `volume write` picks a drive, so they must be unique. \
             Use a different --name, or edit the existing block.",
            paths.config_file.display()
        )));
    }

    // Issue #174: two backends on one device make `--device` resolution
    // ambiguous (ADR-0010 resolves it by matching `device_tape`, and
    // `resolve_lto_backend`/`resolve_device` both stop at the first hit).
    // Canonicalize both sides via `device_matches` — the same
    // string-equality-then-canonicalize comparison `resolve_lto_backend`
    // already uses — so a `/dev/tape/by-id/...` symlink and the `/dev/nstN`
    // it resolves to are caught as the same drive, not just a literal
    // string match.
    if let Some(existing) = config
        .backends
        .lto
        .iter()
        .find(|b| crate::config::device_matches(&b.device_tape, device_tape))
    {
        return Err(TapectlError::Other(format!(
            "device_tape \"{device_tape}\" is already configured as backend \"{}\" \
             (device_tape \"{}\") in {}\n\n\
             --device resolves by device_tape, so two backends on the same drive \
             make that resolution ambiguous. Use a different --device-tape, or edit \
             the existing block.",
            existing.name,
            existing.device_tape,
            paths.config_file.display()
        )));
    }

    // Devices are checked but never enforced: an operator may configure a
    // drive before plugging it in, and this box is not the only machine the
    // config may be carried to. Same fail-open rule as #97's dar probe —
    // a helpful check must not become a tool that refuses to run.
    for (flag, path) in [("--device-tape", device_tape), ("--device-sg", device_sg)] {
        if !std::path::Path::new(path).exists() {
            eprintln!(
                "warning: {flag} {path} does not exist on this machine. \
                 Writing it anyway; check `ls -l /dev/tape/by-id/` (and `lsscsi -g` \
                 for the sg node) if that is not deliberate."
            );
        }
    }

    // Issue #143: the table is written by the same editor `config add
    // backends.lto` uses — in place, keeping every comment (`Config::save`
    // would drop them all, the commented example `init` writes included),
    // checked as every command loads the file before it is written, and
    // replaced atomically, so a refusal leaves the file as it was. An older
    // config's `lto = []` under `[backends]` (what `init` serialized before
    // the skip_serializing_if; TOML refuses it beside a `[[backends.lto]]`
    // table) becomes the list the table joins.
    let original = std::fs::read_to_string(&paths.config_file)?;
    let planned = plan_add(
        &original,
        &paths.config_file,
        backend_fields(
            name,
            device_tape,
            device_sg,
            generation,
            capacity_override,
            enospc_buffer,
        ),
    )?;

    // Issue #241: every refusal above (bad name/generation/capacity,
    // duplicate name, duplicate device_tape, and the planned edit's own
    // check) is a fact about the request and stays ahead of this return —
    // a dry run must still refuse what the real run would refuse. The
    // device-missing warning above is informational, not a mutation, so it
    // is harmless to have already printed it.
    if dry_run {
        if json_output {
            println!(
                "{}",
                serde_json::json!({"backend": name, "device_tape": device_tape,
                                   "device_sg": device_sg, "generation": generation,
                                   "dry_run": true})
            );
        } else {
            println!(
                "would add backend \"{name}\" to {} ({generation}, tape={device_tape}, \
                 sg={device_sg}) (DRY RUN — no changes made)",
                paths.config_file.display()
            );
        }
        return Ok(());
    }

    crate::config_edit::replace_file(&paths.config_file, &original, &planned.new_text)?;

    if json_output {
        println!(
            "{}",
            serde_json::json!({"backend": name, "device_tape": device_tape,
                               "device_sg": device_sg, "generation": generation,
                               "status": "added"})
        );
    } else {
        println!(
            "backend \"{name}\" added to {} ({generation}, tape={device_tape}, sg={device_sg})",
            paths.config_file.display()
        );
        println!("verify it with: tapectl config check");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p() -> std::path::PathBuf {
        std::path::PathBuf::from("/nonexistent/tapectl-backend-test/config.toml")
    }

    /// The text `backend add` would write over `original`.
    fn added(original: &str, fields: Vec<String>) -> String {
        plan_add(original, &p(), fields)
            .expect("the add plans")
            .new_text
    }

    /// The table must parse back as a real backend — the acceptance bar is
    /// "config check passes against it", and a table that only looks right
    /// is exactly the hand-edited-TOML failure this command removes.
    #[test]
    fn the_generated_table_parses_as_a_backend() {
        let text = added(
            "[dar]\nbinary = \"dar\"\n",
            backend_fields(
                "hp-lto6",
                "/dev/tape/by-id/scsi-ABC-nst",
                "/dev/sg1",
                "LTO-6",
                Some("2.5TB"),
                Some("50M"),
            ),
        );
        let cfg: Config = toml::from_str(&text).expect("table must parse");
        let b = &cfg.backends.lto[0];
        assert_eq!(b.name, "hp-lto6");
        assert_eq!(b.device_tape, "/dev/tape/by-id/scsi-ABC-nst");
        assert_eq!(b.device_sg, "/dev/sg1");
        assert_eq!(b.generation, "LTO-6");
        assert_eq!(b.capacity_override.as_deref(), Some("2.5TB"));
        assert_eq!(b.enospc_buffer, "50M");
    }

    /// Omitted knobs fall back to their serde defaults rather than being
    /// written out as operator choices. `block_size`/`hardware_compression`
    /// are checked for absence still, and now for a stronger reason than
    /// #118/#121: spec W4 deleted both, so emitting either would produce a
    /// table that `Config::load` rejects outright.
    #[test]
    fn inert_knobs_are_absent_and_defaulted() {
        let text = added(
            "[dar]\nbinary = \"dar\"\n",
            backend_fields("b", "/dev/nst0", "/dev/sg1", "LTO-6", None, None),
        );
        assert!(!text.contains("block_size"), "{text}");
        assert!(!text.contains("hardware_compression"), "{text}");
        assert!(!text.contains("enospc_buffer"), "{text}");
        assert!(!text.contains("capacity_override"), "{text}");

        let cfg: Config = toml::from_str(&text).unwrap();
        let b = &cfg.backends.lto[0];
        assert_eq!(b.enospc_buffer, "50M");
        assert!(b.capacity_override.is_none());
    }

    /// The end-to-end failure this command shipped with for about ten
    /// minutes: `init` serialized `lto = []`, and TOML rejects that key
    /// alongside a later `[[backends.lto]]` table as a duplicate — so the
    /// command could not append to the file `init` had just written. No unit
    /// test saw it, because they all built a config with no `[backends]`
    /// table at all. Since #143 the editor turns the empty list into the
    /// list of tables the new one joins.
    #[test]
    fn an_empty_lto_stub_becomes_the_list_the_table_joins() {
        // `checksum_mode` is the "other tables survive" witness — a real,
        // still-live `[defaults]` field whose non-default value must still be
        // exactly what comes back out.
        let before =
            "[dar]\nbinary = \"dar\"\n\n[backends]\nlto = []\n\n[defaults]\nchecksum_mode = \"sha256\"\n";
        let text = added(
            before,
            backend_fields("b", "/dev/nst0", "/dev/sg1", "LTO-6", None, None),
        );
        assert!(!text.contains("lto = []"), "{text}");
        let cfg: Config = toml::from_str(&text).expect("the edited file must parse");
        assert_eq!(cfg.backends.lto.len(), 1);
        assert_eq!(cfg.defaults.checksum_mode, "sha256", "other tables survive");
    }

    /// The commented example `init` writes contains lines that look like
    /// declarations. Changing it would corrupt the operator's guide to the
    /// very thing this command configures.
    #[test]
    fn commented_lines_are_left_alone() {
        let text = "[dar]\nbinary = \"dar\"\n\n# [[backends.lto]]\n# lto = []\n# name = \"lto6\"\n";
        let after = added(
            text,
            backend_fields("b", "/dev/nst0", "/dev/sg1", "LTO-6", None, None),
        );
        for line in text.lines() {
            assert!(after.contains(line), "{line:?} lost:\n{after}");
        }
    }

    /// A fresh `init` no longer writes the stub at all.
    #[test]
    fn a_serialized_default_config_has_no_lto_stub() {
        let text = toml::to_string_pretty(&Config::default()).unwrap();
        assert!(!text.contains("lto = []"), "{text}");
    }

    /// Adding must leave the rest of the file — comments included —
    /// untouched. `Config::save` would drop every one of them, taking the
    /// commented example `init` writes with it.
    #[test]
    fn adding_preserves_comments_already_in_the_file() {
        let original =
            "# operator note: the drive lives in the basement\n[dar]\nbinary = \"dar\"\n";
        let after = added(
            original,
            backend_fields("b", "/dev/nst0", "/dev/sg1", "LTO-6", None, None),
        );
        assert!(after.starts_with(original), "{after}");
        assert!(toml::from_str::<Config>(&after).is_ok());
    }

    /// A name that reads as a TOML number is still written as a string.
    #[test]
    fn a_numeric_name_is_written_as_a_string() {
        let text = added(
            "",
            backend_fields("2024", "/dev/nst0", "/dev/sg1", "LTO-6", None, None),
        );
        assert!(text.contains("name = \"2024\""), "{text}");
    }

    // ---- issue #174: `add` must refuse a second backend on the same device ----

    /// Writes a starter config with one `[[backends.lto]]` entry, returning
    /// the `TapectlPaths` `add()` needs. Not `Config::default()` serialized —
    /// `add()` reads the file back as text (to preserve comments), so the
    /// fixture must be a real file on disk, same as `an_empty_lto_stub_...`
    /// above builds its TOML by hand rather than through `Config::save`.
    fn config_with_one_backend(
        dir: &std::path::Path,
        existing_name: &str,
        existing_device_tape: &str,
    ) -> TapectlPaths {
        let paths = TapectlPaths::new(dir.to_path_buf());
        std::fs::write(
            &paths.config_file,
            format!(
                "[dar]\nbinary = \"dar\"\n\n[[backends.lto]]\n\
                 name = \"{existing_name}\"\ndevice_tape = \"{existing_device_tape}\"\n\
                 device_sg = \"/dev/sg0\"\ngeneration = \"LTO-6\"\n"
            ),
        )
        .unwrap();
        paths
    }

    /// The exact-string case: two backends configured with the identical
    /// literal `device_tape`, no filesystem canonicalization required at
    /// all. The plainest instance of the defect and the floor the
    /// canonicalizing cases must still clear.
    #[test]
    fn add_refuses_a_second_backend_on_the_identical_device_tape_string() {
        let tmp = tempfile::TempDir::new().unwrap();
        let paths = config_with_one_backend(tmp.path(), "drive-a", "/dev/nst0");

        let err = add(
            &paths,
            "drive-b",
            "/dev/nst0",
            "/dev/sg1",
            "LTO-6",
            None,
            None,
            false,
            false,
        )
        .unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("drive-a"), "{msg}");
        assert!(msg.contains("/dev/nst0"), "{msg}");

        // And the file must be untouched — a rejected add is not a partial one.
        let reloaded = Config::load(&paths.config_file).unwrap();
        assert_eq!(reloaded.backends.lto.len(), 1);
    }

    /// The case the issue is actually about: an existing backend registered
    /// by its `/dev/nstN` target, a second `add` naming the same drive by a
    /// by-id symlink to that target. A pure string compare would miss this
    /// — CLAUDE.md tells operators to use by-id paths while `lsscsi`/`mt`
    /// print `/dev/nstN`, so this exact mismatch is the one they will hit.
    #[test]
    fn add_refuses_a_by_id_alias_of_an_already_configured_nst_device() {
        let tmp = tempfile::TempDir::new().unwrap();
        let real = tmp.path().join("nst0");
        std::fs::File::create(&real).unwrap();
        let by_id = tmp.path().join("scsi-XYZZY-nst");
        std::os::unix::fs::symlink(&real, &by_id).unwrap();

        let paths = config_with_one_backend(tmp.path(), "drive-a", real.to_str().unwrap());

        let err = add(
            &paths,
            "drive-b",
            by_id.to_str().unwrap(),
            "/dev/sg1",
            "LTO-6",
            None,
            None,
            false,
            false,
        )
        .unwrap_err();
        assert!(err.to_string().contains("drive-a"), "{err}");
    }

    /// The reverse direction: the existing backend is registered by the
    /// by-id symlink, and the new `add` is attempted with the `/dev/nstN`
    /// target it resolves to.
    #[test]
    fn add_refuses_the_nst_target_of_an_already_configured_by_id_device() {
        let tmp = tempfile::TempDir::new().unwrap();
        let real = tmp.path().join("nst0");
        std::fs::File::create(&real).unwrap();
        let by_id = tmp.path().join("scsi-XYZZY-nst");
        std::os::unix::fs::symlink(&real, &by_id).unwrap();

        let paths = config_with_one_backend(tmp.path(), "drive-a", by_id.to_str().unwrap());

        let err = add(
            &paths,
            "drive-b",
            real.to_str().unwrap(),
            "/dev/sg1",
            "LTO-6",
            None,
            None,
            false,
            false,
        )
        .unwrap_err();
        assert!(err.to_string().contains("drive-a"), "{err}");
    }

    /// A genuinely different device must still be addable — this is a
    /// uniqueness check, not a one-backend-only limiter.
    #[test]
    fn add_still_accepts_a_second_backend_on_a_distinct_device() {
        let tmp = tempfile::TempDir::new().unwrap();
        let other = tmp.path().join("nst1");
        std::fs::File::create(&other).unwrap();
        let paths = config_with_one_backend(tmp.path(), "drive-a", "/dev/nst0");

        add(
            &paths,
            "drive-b",
            other.to_str().unwrap(),
            "/dev/sg1",
            "LTO-6",
            None,
            None,
            false,
            false,
        )
        .unwrap();

        let reloaded = Config::load(&paths.config_file).unwrap();
        assert_eq!(reloaded.backends.lto.len(), 2);
    }

    /// A backend for a device that does not exist yet (a detached drive) is
    /// legitimate and must not be rejected by canonicalization failing —
    /// only an actual match against an existing entry refuses.
    #[test]
    fn add_accepts_a_nonexistent_device_distinct_from_the_existing_one() {
        let tmp = tempfile::TempDir::new().unwrap();
        let paths = config_with_one_backend(tmp.path(), "drive-a", "/dev/nst0");

        add(
            &paths,
            "drive-b",
            "/dev/this-drive-is-not-plugged-in",
            "/dev/sg1",
            "LTO-6",
            None,
            None,
            false,
            false,
        )
        .unwrap();

        let reloaded = Config::load(&paths.config_file).unwrap();
        assert_eq!(reloaded.backends.lto.len(), 2);
    }

    // ---- issue #186: LTO-7-M8 is a medium format, never a drive declaration ----

    fn empty_config(dir: &std::path::Path) -> TapectlPaths {
        let paths = TapectlPaths::new(dir.to_path_buf());
        std::fs::write(&paths.config_file, "[dar]\nbinary = \"dar\"\n").unwrap();
        paths
    }

    /// `Generation::parse` accepts `"LTO-7-M8"` — it is a real medium
    /// generation the cartridge side still needs — but no drive IS an
    /// LTO-7-M8 (ADR-0010 decision 1): `Generation::can_write(Lto7M8, _)` is
    /// `false` for every medium, so a backend declared that way could never
    /// write a single tape and every `volume init` on it would refuse. The
    /// drive that writes Type M cartridges declares `LTO-8`.
    #[test]
    fn add_refuses_lto7_type_m_as_a_drive_generation() {
        let tmp = tempfile::TempDir::new().unwrap();
        let paths = empty_config(tmp.path());

        let err = add(
            &paths,
            "drive-a",
            "/dev/nst0",
            "/dev/sg0",
            "LTO-7-M8",
            None,
            None,
            false,
            false,
        )
        .unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("LTO-8"), "{msg}");
        assert!(
            msg.to_lowercase().contains("cartridge") || msg.contains("Type M"),
            "{msg}"
        );

        // A refused add must not partially write the block.
        let reloaded = Config::load(&paths.config_file).unwrap();
        assert!(reloaded.backends.lto.is_empty());
    }

    /// The drive that writes Type M cartridges is a real, still-accepted
    /// declaration — this must keep working.
    #[test]
    fn add_still_accepts_lto8_as_a_drive_generation() {
        let tmp = tempfile::TempDir::new().unwrap();
        let paths = empty_config(tmp.path());

        add(
            &paths,
            "drive-a",
            "/dev/nst0",
            "/dev/sg0",
            "LTO-8",
            None,
            None,
            false,
            false,
        )
        .unwrap();

        let reloaded = Config::load(&paths.config_file).unwrap();
        assert_eq!(reloaded.backends.lto[0].generation, "LTO-8");
    }

    /// The example list in the "not a recognised generation" message must
    /// not suggest a value that a drive can never legally declare — that
    /// suggestion is exactly how an operator lands on the defect in the
    /// first place.
    #[test]
    fn the_unrecognised_generation_message_does_not_suggest_lto7_type_m() {
        let tmp = tempfile::TempDir::new().unwrap();
        let paths = empty_config(tmp.path());

        let err = add(
            &paths,
            "drive-a",
            "/dev/nst0",
            "/dev/sg0",
            "not-a-generation",
            None,
            None,
            false,
            false,
        )
        .unwrap_err();
        assert!(!err.to_string().contains("LTO-7-M8"), "{err}");
    }
}
