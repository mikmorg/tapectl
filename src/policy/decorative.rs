//! Advisory scan for config keys that are parsed but not yet consumed by
//! any write-path code — the `#92`/`#50` precedent (`docs/design-errata.md`):
//! surface a dead knob and never change `config check`'s exit code.
//!
//! **There are currently no such keys, and `scan` returns nothing.** That is
//! the honest state of the config, not an oversight, and the module survives
//! deliberately: it is the mechanism that catches the NEXT key someone adds
//! to `Config` and forgets to wire, and re-adding it later would be more
//! work than leaving one empty `Vec` behind.
//!
//! The three keys this module was built for are GONE from `Config`
//! altogether (spec W4, operator decision 2026-09-13 — tapectl has never
//! been used in production, so a knob that does nothing should be deleted
//! rather than documented forever). They were deleted rather than demoted
//! because in each case the thing the knob claimed to control is not merely
//! unimplemented, it is decided somewhere else and cannot move:
//!
//! - `backends.lto[].block_size` — the write path's block size is a FORMAT
//!   CONSTANT (512 KiB: `collection::plan::BLOCK_SIZE`,
//!   `cli::volume::DEFAULT_BLOCK_SIZE`, `docs/design/volume-format-v2.md`
//!   §1/D7). `src/volume/layout.rs` bakes it into the on-tape recovery text
//!   an heir reads, so a per-drive value could only ever disagree with the
//!   tape it was written to.
//! - `backends.lto[].hardware_compression` — `TapeStore::open`
//!   (`src/store.rs`) calls `dev.disable_compression()` unconditionally on
//!   every write-path open, confirmed landing on real LTO-6 hardware
//!   (`DCE` 1→0) in `docs/lto6-session-journal-2026-09-10.md`. The knob
//!   could never re-enable compression; a `true` value was silently ignored.
//! - `packing.min_free_for_append` — append is rejected outright
//!   (ADR-0003); there is no append path for this knob to gate.
//!
//! A config file still carrying any of the three is now REJECTED by name,
//! with the reason, by `config::stale_lto_fields_message` — a sharper answer
//! than an advisory note, and the reason deleting them is not a loss of
//! operator-facing surface.

use crate::config::Config;

/// One config key that is parsed but has no reader.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DecorativeHit {
    /// Dotted key path, e.g. `"backends.lto[\"lto1\"].some_future_knob"`.
    pub key: String,
}

/// Every decorative-key occurrence in a loaded config.
///
/// Empty today: every key this scan was built for has been deleted from
/// `Config` (see the module doc). A future key with no reader is added here,
/// per configured `[[backends.lto]]` entry if it is per-backend, once
/// globally if it is not.
pub fn scan(_config: &Config) -> Vec<DecorativeHit> {
    Vec::new()
}

/// The advisory line for one hit. Pure, so the wording is testable without
/// a `Config` and so `config check`'s `--json` and text arms can never
/// drift apart.
///
/// Only the generic form survives: the three keys that had bespoke wording
/// no longer exist, and their reasons now live in the load-time rejection
/// (`config::stale_lto_fields_message`) where an operator actually meets
/// them. A future decorative key adds its own arm here.
pub fn describe(hit: &DecorativeHit) -> String {
    format!("note: {} is parsed but not consumed.", hit.key)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::LtoBackendConfig;

    fn backend(name: &str) -> LtoBackendConfig {
        LtoBackendConfig {
            name: name.to_string(),
            device_tape: "/dev/nst0".to_string(),
            device_sg: "/dev/sg0".to_string(),
            generation: "LTO-8".to_string(),
            capacity_override: Some("2.5T".to_string()),
            usable_capacity_factor: 0.92,
            enospc_buffer: "50M".to_string(),
        }
    }

    /// The default config has no decorative keys — spec W4 deleted all
    /// three of this module's original subjects from `Config` rather than
    /// leaving them parsed-and-unread.
    #[test]
    fn a_default_config_reports_nothing() {
        let config = Config::default();
        assert!(scan(&config).is_empty());
    }

    /// The two deleted per-backend keys were reported once per configured
    /// drive, so a multi-backend config is the case that would still show
    /// them if any survived.
    #[test]
    fn backends_no_longer_contribute_any_hits() {
        let mut config = Config::default();
        config.backends.lto.push(backend("lto1"));
        config.backends.lto.push(backend("lto2"));
        assert!(scan(&config).is_empty());
    }

    /// The mechanism still works — this is what a future unwired key gets.
    #[test]
    fn describe_names_the_key_and_says_it_is_unconsumed() {
        let hit = DecorativeHit {
            key: "backends.lto[\"lto1\"].some_future_knob".to_string(),
        };
        let line = describe(&hit);
        assert!(line.contains("some_future_knob"), "{line}");
        assert!(line.contains("not consumed"), "{line}");
    }
}
