//! Medium generation detection (ADR-0010): what generation the loaded
//! cartridge actually is, independent of what a drive's configured
//! `generation` says it natively writes.
//!
//! Detection order, per the ADR: MAM *medium* density code, then MAM
//! *format* density code, then the `st` driver's density register
//! (`MTIOCGET`). Every step is best-effort — a MAM read failure (a common,
//! expected case: `sg_read_attr` needs a real sg node) is logged at `warn`
//! and treated as "this source gave nothing", never as a hard failure of
//! detection as a whole. `detect` never returns an `Err`; a caller that
//! cannot determine a generation this way falls back to a declaration
//! (`--media`, a bound cartridge row, or the drive's own `generation`) —
//! that fallback is `volume_init`'s job, not this module's.

use tracing::warn;

use crate::error::{Result, TapectlError};
use crate::media::Generation;
use crate::tape::ioctl;
use crate::tape::mam::{self, MamInfo};

/// Which source (if any) produced [`Detected::generation`].
///
/// Named `None` to mirror the ADR's own vocabulary for "no source
/// detected anything" — never glob-import this enum (`use DetectSource::*`)
/// or that variant shadows `Option::None`; every call site in this crate
/// spells it out as `DetectSource::None`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DetectSource {
    MamMedium,
    MamFormat,
    Driver,
    None,
}

/// The outcome of [`detect`].
#[derive(Debug, Clone)]
pub struct Detected {
    /// The recognised generation, if any source produced a code
    /// [`Generation::from_density_code`] understands.
    pub generation: Option<Generation>,
    /// The raw density code byte that produced `generation` (or that was
    /// read but not recognised, if `generation` is `None`).
    pub code: Option<u8>,
    pub source: DetectSource,
    /// The full MAM read (best-effort), for callers that also want the
    /// serial number, capacity, manufacturer, etc.
    pub mam: MamInfo,
}

/// Detect the loaded medium's generation.
///
/// `device_sg` is queried first (MAM, two fields in priority order);
/// `device_tape` is queried only if neither MAM field yielded a recognised
/// generation.
pub fn detect(device_tape: &str, device_sg: &str) -> Detected {
    let mam = match mam::read_mam(device_sg) {
        Ok(m) => m,
        Err(e) => {
            warn!(sg_device = %device_sg, err = %e, "MAM read failed during media detection (continuing)");
            MamInfo::default()
        }
    };

    if let Some(code) = mam.medium_density_code {
        if let Some(generation) = Generation::from_density_code(code) {
            return Detected {
                generation: Some(generation),
                code: Some(code),
                source: DetectSource::MamMedium,
                mam,
            };
        }
    }

    if let Some(code) = mam.format_density_code {
        if let Some(generation) = Generation::from_density_code(code) {
            return Detected {
                generation: Some(generation),
                code: Some(code),
                source: DetectSource::MamFormat,
                mam,
            };
        }
    }

    match ioctl::density_code(device_tape) {
        Ok(Some(code)) => {
            let generation = Generation::from_density_code(code);
            let source = if generation.is_some() {
                DetectSource::Driver
            } else {
                DetectSource::None
            };
            Detected {
                generation,
                code: Some(code),
                source,
                mam,
            }
        }
        Ok(None) => Detected {
            generation: None,
            code: None,
            source: DetectSource::None,
            mam,
        },
        Err(e) => {
            warn!(device_tape = %device_tape, err = %e, "driver density read failed during media detection (continuing)");
            Detected {
                generation: None,
                code: None,
                source: DetectSource::None,
                mam,
            }
        }
    }
}

/// Where [`resolve_media`]'s generation came from — detected off the medium,
/// or, when nothing could be read, which declaration stood in for it.
///
/// The caller prints this: ADR-0010 requires that a fallback SAY it is a
/// fallback ("assuming <gen> (from ...)"), because an assumed generation is
/// an assumed capacity, and an assumed capacity that is too large is exactly
/// the multi-hour wasted write issue #141 is about.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MediaSource {
    /// Read off the medium itself, by the named detection source. The only
    /// non-fallback answer.
    Detected(DetectSource),
    /// `--media` on the command line.
    Declared,
    /// The registered `media_type` of the cartridge whose serial matches the
    /// loaded medium (or which `--cartridge` named). Carries the barcode so
    /// the message can name it.
    CartridgeRow(String),
    /// Last resort: assume the medium is the drive's own native generation.
    /// This is the pre-ADR-0010 behaviour, now reached only when every other
    /// source is silent, and announced when it is.
    DriveGeneration,
}

impl MediaSource {
    /// Did this come off the medium, rather than from someone's word for it?
    pub fn is_detected(&self) -> bool {
        matches!(self, Self::Detected(_))
    }

    /// The parenthetical for the "assuming <gen> (from ...)" warning. Only
    /// meaningful for the fallback variants; `Detected` has nothing to
    /// apologise for and renders as "the medium itself".
    pub fn describe(&self) -> String {
        match self {
            Self::Detected(_) => "the medium itself".to_string(),
            Self::Declared => "--media".to_string(),
            Self::CartridgeRow(barcode) => format!("cartridge {barcode}"),
            Self::DriveGeneration => "the drive's generation".to_string(),
        }
    }
}

/// Decide the loaded medium's generation (ADR-0010, decision 2), purely.
///
/// Detection wins outright. The medium's own density code is a fact about
/// the physical tape; `--media` and a registered `media_type` are claims
/// about it, and a claim that contradicts the fact is an ERROR rather than a
/// hint — either the wrong tape is loaded or the claim is wrong, and tapectl
/// cannot tell which. Only when no source read a recognised density code
/// does a declaration stand in, in the order `--media` → the matched
/// cartridge row → the drive's own generation.
///
/// Arguments:
/// - `det` — what [`detect`] found (`det.code` is only used to make the
///   contradiction message specific).
/// - `declared` — a parsed `--media`.
/// - `row` — the matched cartridge's `(generation, barcode)`, if a row was
///   matched by serial or named by `--cartridge`.
/// - `drive_gen` — the drive's configured native generation.
///
/// Deliberately does NOT apply the `can_write` compatibility rule: that is a
/// separate question about the DRIVE (and a refusal `--force` cannot bypass),
/// not about what the medium is. The caller asks it next.
pub fn resolve_media(
    det: &Detected,
    declared: Option<Generation>,
    row: Option<(Generation, &str)>,
    drive_gen: Generation,
) -> Result<(Generation, MediaSource)> {
    if let Some(detected) = det.generation {
        if let Some(d) = declared {
            if d != detected {
                return Err(TapectlError::Other(format!(
                    "loaded medium is {detected} ({}), --media says {d}. \
                     The medium's own density code is a fact about the tape; \
                     --media is a claim about it. Either the wrong cartridge \
                     is loaded or --media is wrong.",
                    density_phrase(det.code),
                )));
            }
        }
        if let Some((row_gen, barcode)) = row {
            if row_gen != detected {
                return Err(TapectlError::Other(format!(
                    "cartridge {barcode} is registered as {row_gen}, the loaded medium is \
                     {detected} ({}). Either the registration is wrong or the wrong \
                     cartridge is loaded, and tapectl cannot tell which.",
                    density_phrase(det.code),
                )));
            }
        }
        return Ok((detected, MediaSource::Detected(det.source)));
    }

    if let Some(d) = declared {
        return Ok((d, MediaSource::Declared));
    }
    if let Some((row_gen, barcode)) = row {
        return Ok((row_gen, MediaSource::CartridgeRow(barcode.to_string())));
    }
    Ok((drive_gen, MediaSource::DriveGeneration))
}

/// "density 0x5a", or "no density code reported" when nothing was read.
fn density_phrase(code: Option<u8>) -> String {
    match code {
        Some(c) => format!("density 0x{c:02x}"),
        None => "no density code reported".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Neither a real sg node nor a real tape device: every source fails or
    /// finds nothing, and `detect` must still return cleanly rather than
    /// erroring or panicking — this is exactly the disaster-recovery /
    /// mhvtl-absent shape ADR-0010 requires to degrade gracefully.
    #[test]
    fn detect_on_nonexistent_devices_returns_nothing_detected_without_panicking() {
        let d = detect(
            "/nonexistent/tapectl-media-detect-tape",
            "/nonexistent/tapectl-media-detect-sg",
        );
        assert_eq!(d.generation, None);
        assert_eq!(d.code, None);
        assert_eq!(d.source, DetectSource::None);
        assert_eq!(d.mam, MamInfo::default());
    }

    fn detected(generation: Generation, code: u8) -> Detected {
        Detected {
            generation: Some(generation),
            code: Some(code),
            source: DetectSource::MamMedium,
            mam: MamInfo::default(),
        }
    }

    fn undetected() -> Detected {
        Detected {
            generation: None,
            code: None,
            source: DetectSource::None,
            mam: MamInfo::default(),
        }
    }

    // ---- resolve_media: detection wins (ADR-0010 decision 2) ----

    #[test]
    fn a_detected_generation_wins_over_the_drives_own() {
        // Issue #141 in one line: an LTO-5 cartridge in an LTO-6 drive.
        let (gen, src) = resolve_media(
            &detected(Generation::Lto5, 0x58),
            None,
            None,
            Generation::Lto6,
        )
        .unwrap();
        assert_eq!(gen, Generation::Lto5);
        assert_eq!(src, MediaSource::Detected(DetectSource::MamMedium));
        assert!(src.is_detected());
    }

    #[test]
    fn a_media_declaration_that_agrees_with_detection_is_accepted() {
        let (gen, src) = resolve_media(
            &detected(Generation::Lto6, 0x5a),
            Some(Generation::Lto6),
            None,
            Generation::Lto6,
        )
        .unwrap();
        assert_eq!(gen, Generation::Lto6);
        assert!(src.is_detected());
    }

    #[test]
    fn a_media_declaration_that_contradicts_detection_is_an_error() {
        let err = resolve_media(
            &detected(Generation::Lto5, 0x58),
            Some(Generation::Lto6),
            None,
            Generation::Lto6,
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("loaded medium is LTO-5"), "{err}");
        assert!(err.contains("density 0x58"), "{err}");
        assert!(err.contains("--media says LTO-6"), "{err}");
    }

    #[test]
    fn a_cartridge_row_that_contradicts_detection_is_an_error_naming_both() {
        let err = resolve_media(
            &detected(Generation::Lto5, 0x58),
            None,
            Some((Generation::Lto6, "BC001")),
            Generation::Lto6,
        )
        .unwrap_err()
        .to_string();
        assert!(
            err.contains("cartridge BC001 is registered as LTO-6"),
            "{err}"
        );
        assert!(err.contains("the loaded medium is LTO-5"), "{err}");
    }

    #[test]
    fn a_cartridge_row_that_agrees_with_detection_is_accepted() {
        let (gen, src) = resolve_media(
            &detected(Generation::Lto6, 0x5a),
            None,
            Some((Generation::Lto6, "BC001")),
            Generation::Lto6,
        )
        .unwrap();
        assert_eq!(gen, Generation::Lto6);
        assert!(src.is_detected());
    }

    // ---- resolve_media: the declaration ladder, only when nothing is detected ----

    #[test]
    fn undetected_falls_back_to_media_first() {
        let (gen, src) = resolve_media(
            &undetected(),
            Some(Generation::Lto5),
            Some((Generation::Lto6, "BC001")),
            Generation::Lto7,
        )
        .unwrap();
        assert_eq!(gen, Generation::Lto5);
        assert_eq!(src, MediaSource::Declared);
        assert!(!src.is_detected());
        assert_eq!(src.describe(), "--media");
    }

    #[test]
    fn undetected_falls_back_to_the_cartridge_row_next() {
        let (gen, src) = resolve_media(
            &undetected(),
            None,
            Some((Generation::Lto6, "BC001")),
            Generation::Lto7,
        )
        .unwrap();
        assert_eq!(gen, Generation::Lto6);
        assert_eq!(src, MediaSource::CartridgeRow("BC001".into()));
        assert_eq!(src.describe(), "cartridge BC001");
    }

    #[test]
    fn undetected_falls_back_to_the_drive_generation_last() {
        let (gen, src) = resolve_media(&undetected(), None, None, Generation::Lto7).unwrap();
        assert_eq!(gen, Generation::Lto7);
        assert_eq!(src, MediaSource::DriveGeneration);
        assert_eq!(src.describe(), "the drive's generation");
    }

    /// With nothing detected there is no fact to contradict, so a `--media`
    /// that disagrees with a registered row is not an error — `--media` is
    /// simply the higher-priority declaration. The row-vs-medium refusal
    /// ADR-0010 introduces is about a DETECTED medium only.
    #[test]
    fn undetected_media_and_row_may_disagree_without_erroring() {
        let (gen, _) = resolve_media(
            &undetected(),
            Some(Generation::Lto8),
            Some((Generation::Lto6, "BC001")),
            Generation::Lto8,
        )
        .unwrap();
        assert_eq!(gen, Generation::Lto8);
    }

    /// A driver-sourced detection is still a detection: the medium answered.
    #[test]
    fn a_driver_sourced_detection_is_still_detected() {
        let det = Detected {
            generation: Some(Generation::Lto6),
            code: Some(0x5a),
            source: DetectSource::Driver,
            mam: MamInfo::default(),
        };
        let (_, src) = resolve_media(&det, None, None, Generation::Lto8).unwrap();
        assert_eq!(src, MediaSource::Detected(DetectSource::Driver));
        assert!(src.is_detected());
    }
}
