//! `tapectl backend` — populate `[[backends.lto]]` without hand-editing TOML.
//!
//! #126, the follow-up half of #124's first-run cliff. The immediate half made
//! the missing-backend failure self-explanatory and put a commented example in
//! a fresh `init`; this is the command that fills it in.

use crate::cli::BackendCommands;
use crate::config::{Config, TapectlPaths};
use crate::error::{Result, TapectlError};
use std::io::Write;

pub fn run(paths: &TapectlPaths, command: &BackendCommands, json_output: bool) -> Result<()> {
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
        ),
    }
}

/// The `[[backends.lto]]` block for these values.
///
/// Pure, so the text is testable and the command cannot drift from what the
/// tests assert. `block_size` and `hardware_compression` were deliberately
/// absent here while they still parsed (#118, #121 — offering an operator a
/// knob that does nothing is a false assurance); spec W4 has since deleted
/// both from `LtoBackendConfig` entirely, so a block carrying either would
/// now fail to load. `capacity_override` (ADR-0010) is absent unless
/// explicitly given — a real drive's capacity follows the loaded cartridge's
/// detected generation, not this config.
pub fn backend_block(
    name: &str,
    device_tape: &str,
    device_sg: &str,
    generation: &str,
    capacity_override: Option<&str>,
    enospc_buffer: Option<&str>,
) -> String {
    let mut s = format!(
        "\n[[backends.lto]]\nname = \"{name}\"\ndevice_tape = \"{device_tape}\"\n\
         device_sg = \"{device_sg}\"\ngeneration = \"{generation}\"\n"
    );
    if let Some(cap) = capacity_override {
        s.push_str(&format!("capacity_override = \"{cap}\"\n"));
    }
    if let Some(buf) = enospc_buffer {
        s.push_str(&format!("enospc_buffer = \"{buf}\"\n"));
    }
    s
}

/// Remove a bare `lto = []` from the `[backends]` table.
///
/// Scoped to that table rather than matched anywhere in the file: the key is
/// only meaningful there, and a blind line match would happily delete an
/// identical line out of some other table. Commented lines are left alone —
/// `# lto = []` inside the example block is documentation, not a declaration.
pub fn drop_empty_lto_stub(text: &str) -> String {
    let mut out = Vec::new();
    let mut in_backends = false;
    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with('[') && !trimmed.starts_with("[[") {
            in_backends = trimmed == "[backends]";
        }
        if in_backends && trimmed.replace(' ', "") == "lto=[]" {
            continue;
        }
        out.push(line);
    }
    let mut s = out.join("\n");
    if text.ends_with('\n') {
        s.push('\n');
    }
    s
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
) -> Result<()> {
    crate::naming::validate_backend_name(name)?;

    // Validated here rather than at the next `Config::load`, so a typo is
    // rejected while the operator is still looking at the command that
    // caused it (#59's boundary-validation rule). The canonical spelling
    // (not the operator's raw one) is what gets written to the file, so
    // `LTO6`/`l6`/`LTO-6` all land the same way.
    let generation = crate::media::Generation::parse(generation)
        .ok_or_else(|| {
            TapectlError::Other(format!(
                "{generation:?} is not a recognised LTO generation \
                 (e.g. LTO-6, LTO-7, LTO-7-M8, LTO-8)"
            ))
        })?
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

    // Appended as text, never re-serialized. `Config::save` round-trips
    // through serde, which silently drops every comment in the file —
    // including the commented example `init` writes and anything the
    // operator added. Adding a backend must not quietly rewrite the rest of
    // their config.
    // An older config may carry `lto = []` under `[backends]` — what `init`
    // serialized before the skip_serializing_if. TOML treats that key and a
    // later `[[backends.lto]]` table as duplicate definitions of `lto` and
    // refuses to parse the file at all, so the stub is cleared first.
    let text = std::fs::read_to_string(&paths.config_file)?;
    let cleaned = drop_empty_lto_stub(&text);
    if cleaned != text {
        std::fs::write(&paths.config_file, &cleaned)?;
    }

    let block = backend_block(
        name,
        device_tape,
        device_sg,
        generation,
        capacity_override,
        enospc_buffer,
    );
    let mut f = std::fs::OpenOptions::new()
        .append(true)
        .open(&paths.config_file)?;
    f.write_all(block.as_bytes())?;
    drop(f);

    // Read it back rather than trusting the write: the whole point of the
    // command is that the operator does not have to check the TOML by hand.
    let reloaded = Config::load(&paths.config_file)?;
    let found = reloaded.backends.lto.iter().any(|b| b.name == name);
    if !found {
        return Err(TapectlError::Other(format!(
            "wrote the backend block to {} but reloading the config did not find \
             it — the file may have pre-existing syntax errors. Run `tapectl config check`.",
            paths.config_file.display()
        )));
    }

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

    /// The block must parse back as a real backend — the acceptance bar is
    /// "config check passes against it", and a block that only looks right is
    /// exactly the hand-edited-TOML failure this command removes.
    #[test]
    fn the_generated_block_parses_as_a_backend() {
        let toml = format!(
            "[dar]\nbinary = \"dar\"\n{}",
            backend_block(
                "hp-lto6",
                "/dev/tape/by-id/scsi-ABC-nst",
                "/dev/sg1",
                "LTO-6",
                Some("2.5TB"),
                Some("50M"),
            )
        );
        let cfg: Config = toml::from_str(&toml).expect("block must parse");
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
    /// block that `Config::load` rejects outright.
    #[test]
    fn inert_knobs_are_absent_and_defaulted() {
        let block = backend_block("b", "/dev/nst0", "/dev/sg1", "LTO-6", None, None);
        assert!(!block.contains("block_size"), "{block}");
        assert!(!block.contains("hardware_compression"), "{block}");
        assert!(!block.contains("enospc_buffer"), "{block}");
        assert!(!block.contains("capacity_override"), "{block}");

        let cfg: Config = toml::from_str(&format!("[dar]\nbinary = \"dar\"\n{block}")).unwrap();
        let b = &cfg.backends.lto[0];
        assert_eq!(b.enospc_buffer, "50M");
        assert!(b.capacity_override.is_none());
    }

    /// The end-to-end failure this command shipped with for about ten
    /// minutes: `init` serialized `lto = []`, and TOML rejects that key
    /// alongside a later `[[backends.lto]]` table as a duplicate — so the
    /// command could not append to the file `init` had just written. No unit
    /// test saw it, because they all built a config with no `[backends]`
    /// table at all.
    #[test]
    fn an_empty_lto_stub_is_cleared_so_the_appended_table_parses() {
        let before =
            "[dar]\nbinary = \"dar\"\n\n[backends]\nlto = []\n\n[defaults]\nhash = \"sha256\"\n";
        let block = backend_block("b", "/dev/nst0", "/dev/sg1", "LTO-6", None, None);
        assert!(
            toml::from_str::<Config>(&format!("{before}{block}")).is_err(),
            "precondition: the stub and the table really do collide"
        );

        let after = drop_empty_lto_stub(before);
        assert!(!after.contains("lto = []"));
        let cfg: Config =
            toml::from_str(&format!("{after}{block}")).expect("cleared stub must let it parse");
        assert_eq!(cfg.backends.lto.len(), 1);
        assert_eq!(cfg.defaults.hash, "sha256", "other tables survive");
    }

    /// The commented example `init` writes contains lines that look like
    /// declarations. Deleting from it would corrupt the operator's guide to
    /// the very thing this command configures.
    #[test]
    fn commented_lines_and_other_tables_are_left_alone() {
        let text = "[backends]\n# lto = []\n\n[other]\nlto = []\n";
        assert_eq!(drop_empty_lto_stub(text), text);
    }

    /// A fresh `init` no longer writes the stub at all.
    #[test]
    fn a_serialized_default_config_has_no_lto_stub() {
        let text = toml::to_string_pretty(&Config::default()).unwrap();
        assert!(!text.contains("lto = []"), "{text}");
    }

    /// Appending must leave the rest of the file — comments included —
    /// untouched. `Config::save` would drop every one of them, taking the
    /// commented example `init` writes with it.
    #[test]
    fn appending_preserves_comments_already_in_the_file() {
        let original =
            "# operator note: the drive lives in the basement\n[dar]\nbinary = \"dar\"\n";
        let after = format!(
            "{original}{}",
            backend_block("b", "/dev/nst0", "/dev/sg1", "LTO-6", None, None)
        );
        assert!(after.contains("# operator note: the drive lives in the basement"));
        assert!(toml::from_str::<Config>(&after).is_ok());
    }
}
