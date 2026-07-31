//! Advisory scan for config keys that are parsed but not yet consumed by
//! any write-path code — the `#92`/`#50` precedent (`docs/design-errata.md`):
//! surface a dead knob, never delete operator-facing surface, and never
//! change `config check`'s exit code.
//!
//! Each key here is inert *by decision*, not by oversight, and each has an
//! owner:
//! - `backends.lto[].block_size` — the write path hardcodes a fixed 512 KiB
//!   block (`collection::plan::BLOCK_SIZE`, `cli::volume::DEFAULT_BLOCK_SIZE`);
//!   `docs/design/v2-open-questions.md` §5 lists 512K-vs-1M as a deferred
//!   **hardware** question (LTO-6 validation), and epic #20 child work may
//!   consume this field once that's answered.
//! - `backends.lto[].hardware_compression` — `docs/design-errata.md` §2.29
//!   records that `MTCOMPRESSION 0` lands with #28; nothing calls
//!   `MTCOMPRESSION` today.
//! - `packing.min_free_for_append` — append is rejected outright (ADR-0003);
//!   there is no append path for this knob to gate.
//!
//! (`packing.manifest_reserve` from the v4.0 draft was actually removed from
//! `Config` in the v2 T10 config cleanup — it collapsed into the ENOSPC
//! buffer, per the §2.9 errata row — so there is no live field left to
//! surface for it.)

use crate::config::Config;

/// One config key that is parsed but has no reader.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DecorativeHit {
    /// Dotted key path, e.g. `"backends.lto[\"lto1\"].block_size"`.
    pub key: String,
}

/// Every decorative-key occurrence in a loaded config.
///
/// `block_size` and `hardware_compression` are per-backend, so one hit is
/// reported per configured `[[backends.lto]]` entry. `min_free_for_append`
/// is a single top-level `[packing]` field, so it reports once regardless
/// of backend count — including when there are zero backends configured,
/// since the field still exists and is still unread.
pub fn scan(config: &Config) -> Vec<DecorativeHit> {
    let mut out = Vec::new();

    for backend in &config.backends.lto {
        out.push(DecorativeHit {
            key: format!("backends.lto[\"{}\"].block_size", backend.name),
        });
        out.push(DecorativeHit {
            key: format!("backends.lto[\"{}\"].hardware_compression", backend.name),
        });
    }

    out.push(DecorativeHit {
        key: "packing.min_free_for_append".to_string(),
    });

    out
}

/// The advisory line for one hit. Pure, so the wording is testable without
/// a `Config` and so `config check`'s `--json` and text arms can never
/// drift apart.
pub fn describe(hit: &DecorativeHit) -> String {
    if hit.key.ends_with(".block_size") {
        format!(
            "note: {} is parsed but not consumed — the write path uses a fixed 512 KiB block \
             unconditionally. 512K-vs-1M is a deferred hardware question \
             (docs/design/v2-open-questions.md §5); epic #20 may wire this once LTO-6 validation \
             answers it.",
            hit.key
        )
    } else if hit.key.ends_with(".hardware_compression") {
        format!(
            "note: {} is parsed but not consumed — nothing issues MTCOMPRESSION today. \
             docs/design-errata.md §2.29 tracks `MTCOMPRESSION 0` landing with issue #28.",
            hit.key
        )
    } else if hit.key == "packing.min_free_for_append" {
        "note: packing.min_free_for_append is parsed but not consumed — append is rejected \
         outright (ADR-0003), so there is no append path for this knob to gate. Inert by \
         decision, not oversight."
            .to_string()
    } else {
        format!("note: {} is parsed but not consumed.", hit.key)
    }
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
            media_type: "LTO-6".to_string(),
            nominal_capacity: "2.5T".to_string(),
            usable_capacity_factor: 0.92,
            enospc_buffer: "50M".to_string(),
            block_size: "1M".to_string(),
            hardware_compression: false,
        }
    }

    #[test]
    fn no_backends_still_reports_min_free_for_append() {
        let config = Config::default();
        let hits = scan(&config);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].key, "packing.min_free_for_append");
    }

    #[test]
    fn each_backend_reports_block_size_and_hardware_compression() {
        let mut config = Config::default();
        config.backends.lto.push(backend("lto1"));
        config.backends.lto.push(backend("lto2"));

        let hits = scan(&config);
        // 2 keys per backend * 2 backends + 1 global = 5
        assert_eq!(hits.len(), 5);
        assert!(hits
            .iter()
            .any(|h| h.key == "backends.lto[\"lto1\"].block_size"));
        assert!(hits
            .iter()
            .any(|h| h.key == "backends.lto[\"lto2\"].hardware_compression"));
        assert!(hits.iter().any(|h| h.key == "packing.min_free_for_append"));
    }

    #[test]
    fn describe_block_size_cites_open_questions_and_epic_20() {
        let hit = DecorativeHit {
            key: "backends.lto[\"lto1\"].block_size".to_string(),
        };
        let line = describe(&hit);
        assert!(line.contains("§5"));
        assert!(line.contains("#20"));
    }

    #[test]
    fn describe_hardware_compression_cites_28_and_errata() {
        let hit = DecorativeHit {
            key: "backends.lto[\"lto1\"].hardware_compression".to_string(),
        };
        let line = describe(&hit);
        assert!(line.contains("#28"));
        assert!(line.contains("MTCOMPRESSION"));
    }

    #[test]
    fn describe_min_free_for_append_cites_adr_0003() {
        let hit = DecorativeHit {
            key: "packing.min_free_for_append".to_string(),
        };
        let line = describe(&hit);
        assert!(line.contains("ADR-0003"));
        assert!(line.contains("append"));
    }
}
