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
//! (`--generation`, a bound cartridge row, or the drive's own `generation`) —
//! that fallback is `volume_init`'s job, not this module's.

use std::fs::OpenOptions;
use std::io;
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::io::AsRawFd;

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

/// Cheap, NON-BLOCKING pre-check for "is any medium loaded at all", run by a
/// caller BEFORE [`detect`] (issue #152).
///
/// `detect`'s driver-density fallback ([`ioctl::density_code`]) opens the
/// tape node with a plain blocking `open()`. On a drive with no cartridge
/// loaded, the `st` driver's `open()` blocks until its own no-medium timeout
/// expires — observed as a ~2m05s stall between the MAM failure and the
/// driver-density failure on an empty mhvtl drive. That is not a correctness
/// bug (the code falls back to a declared generation and the write later
/// fails safely), but a bare drive is an ordinary operator slip and should be
/// reported in under a second, not after a multi-minute silent hang.
///
/// This does NOT open the tape node the way `ioctl::density_code` or
/// `ioctl::TapeDevice` do. It opens with `O_NONBLOCK` — which is the entire
/// point, since the st driver's no-medium wait is a property of a *blocking*
/// open and an `O_NONBLOCK` open returns immediately regardless of whether a
/// medium is present — and then reads `MTIOCGET`'s `mt_gstat` register for
/// the `GMT_DR_OPEN` bit (`<linux/mtio.h>`: "door open (no tape)"). That is
/// exactly the bit `mt status` reports as `DR_OPEN`, and exactly what
/// `scripts/first-run.sh` already greps `mt status` output for before its own
/// write step — this is an in-process version of the same check, not a new
/// idea.
///
/// The `MTIOCGET` request number and `mtget` layout are duplicated here from
/// `tape::ioctl` (private there) rather than exposed from that module: this
/// probe's entire reason to exist is a DIFFERENT open mode than every
/// `tape::ioctl` caller uses, so sharing code would mean either splitting
/// `tape::ioctl`'s open from its ioctl calls (a real refactor, out of scope
/// for this fix) or making its blocking-open internals `pub` for a caller
/// that specifically must not use them. Duplicating ~10 lines of ioctl
/// plumbing seemed the smaller risk; see the issue #152 report for the
/// alternative considered.
///
/// Best-effort like every other source in this module: `detect` itself never
/// returns an `Err`, and neither does this. If the device can't even be
/// opened non-blockingly, or the ioctl fails outright (wrong path,
/// permissions, a driver that answers `MTIOCGET` oddly), that is a DIFFERENT
/// problem than "no medium" and is logged at `warn` and treated as
/// "couldn't tell" — returning `false` (proceed) so the caller falls through
/// to `detect`'s own slower-but-authoritative sources rather than refusing
/// for a reason that has nothing to do with a missing cartridge.
///
/// Returns `true` only when the drive *positively* reports `DR_OPEN` — no
/// cartridge loaded.
pub fn probe_no_medium(device_tape: &str) -> bool {
    match read_gstat_nonblocking(device_tape) {
        Some(gstat) => gstat_reports_no_medium(gstat),
        None => false,
    }
}

/// `GMT_DR_OPEN(x)`, `<linux/mtio.h>`: `(x) & 0x00040000`, "door open (no
/// tape)". Split out from [`probe_no_medium`] purely so the bit-test logic is
/// callable with a literal `i64` in tests, with no device involved at all.
fn gstat_reports_no_medium(gstat: i64) -> bool {
    gstat & 0x0004_0000 != 0
}

// Duplicated from `tape::ioctl` (private there — see `probe_no_medium`'s doc
// comment for why). `MTIOCGET` from <linux/mtio.h>: `_IOR('m', 2, struct
// mtget)`.
const MTIOCGET: u64 = 0x80306d02;

// Duplicated from `tape::ioctl` (private there): the `mtget` struct shape
// from <linux/mtio.h>. Only `mt_gstat` is read; the rest exist so the ioctl
// writes into a correctly-sized buffer.
#[repr(C)]
#[derive(Default)]
struct MtGet {
    mt_type: i64,
    mt_resid: i64,
    mt_dsreg: i64,
    mt_gstat: i64,
    mt_erreg: i64,
    mt_fileno: i32,
    mt_blkno: i32,
}

/// Open `device_tape` with `O_NONBLOCK` and read `MTIOCGET`'s `mt_gstat`
/// register. `None` on any failure (open or ioctl) — logged at `warn` and
/// otherwise swallowed; see `probe_no_medium`'s doc comment for why a failure
/// here must never look like a positive "no medium" answer.
fn read_gstat_nonblocking(device_tape: &str) -> Option<i64> {
    let file = match OpenOptions::new()
        .read(true)
        .custom_flags(nix::libc::O_NONBLOCK)
        .open(device_tape)
    {
        Ok(f) => f,
        Err(e) => {
            warn!(device_tape = %device_tape, err = %e, "non-blocking open failed during no-medium probe (continuing)");
            return None;
        }
    };
    let mut mtget = MtGet::default();
    let rc = unsafe { nix::libc::ioctl(file.as_raw_fd(), MTIOCGET, &mut mtget as *mut MtGet) };
    if rc != 0 {
        warn!(
            device_tape = %device_tape,
            err = %io::Error::last_os_error(),
            "MTIOCGET failed during no-medium probe (continuing)"
        );
        return None;
    }
    Some(mtget.mt_gstat)
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
    /// `--generation` on the command line.
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
            Self::Declared => "--generation".to_string(),
            Self::CartridgeRow(barcode) => format!("cartridge {barcode}"),
            Self::DriveGeneration => "the drive's generation".to_string(),
        }
    }
}

/// Decide the loaded medium's generation (ADR-0010, decision 2), purely.
///
/// Detection wins outright. The medium's own density code is a fact about
/// the physical tape; `--generation` and a registered `media_type` are claims
/// about it, and a claim that contradicts the fact is an ERROR rather than a
/// hint — either the wrong tape is loaded or the claim is wrong, and tapectl
/// cannot tell which. Only when no source read a recognised density code
/// does a declaration stand in, in the order `--generation` → the matched
/// cartridge row → the drive's own generation.
///
/// Arguments:
/// - `det` — what [`detect`] found (`det.code` is only used to make the
///   contradiction message specific).
/// - `declared` — a parsed `--generation`.
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
    // 1. A declaration may not contradict the medium's own density code.
    if let (Some(detected), Some(d)) = (det.generation, declared) {
        if d != detected {
            return Err(TapectlError::Other(format!(
                "loaded medium is {detected} ({}), --generation says {d}. \
                 The medium's own density code is a fact about the tape; \
                 --generation is a claim about it. Either the wrong \
                 cartridge is loaded or --generation is wrong.",
                density_phrase(det.code),
            )));
        }
    }

    // 2. The ladder: detection, else --generation, else the row, else the drive.
    let (generation, source) = if let Some(detected) = det.generation {
        (detected, MediaSource::Detected(det.source))
    } else if let Some(d) = declared {
        (d, MediaSource::Declared)
    } else if let Some((row_gen, barcode)) = row {
        (row_gen, MediaSource::CartridgeRow(barcode.to_string()))
    } else {
        (drive_gen, MediaSource::DriveGeneration)
    };

    // 3. A matched row must agree with whatever won — this is ADR-0010's
    //    "registered row whose media_type disagrees ... is an error". It is
    //    checked against the RESOLVED generation, not only against a detected
    //    one, so that `--generation LTO-8` on a cartridge registered LTO-6 is
    //    caught too: those are two claims by the same operator that
    //    contradict each other, and tapectl can see the contradiction even
    //    when it cannot see the tape. Vacuous when the row itself won.
    if let Some((row_gen, barcode)) = row {
        if row_gen != generation {
            let what_says = if source.is_detected() {
                format!(
                    "the loaded medium is {generation} ({})",
                    density_phrase(det.code)
                )
            } else {
                format!("--generation says {generation}")
            };
            return Err(TapectlError::Other(format!(
                "cartridge {barcode} is registered as {row_gen}, {what_says}. Either the \
                 registration is wrong or the wrong cartridge is loaded, and tapectl \
                 cannot tell which."
            )));
        }
    }

    Ok((generation, source))
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

    // ---- issue #152: the no-medium pre-check ----

    /// Real LTO-6 `mt_gstat` shapes carry other bits (BOT, ONLINE, ...)
    /// alongside DR_OPEN; the test data mirrors that rather than testing
    /// the bit in isolation.
    #[test]
    fn gstat_reports_no_medium_detects_the_dr_open_bit() {
        // GMT_DR_OPEN alone.
        assert!(gstat_reports_no_medium(0x0004_0000));
        // GMT_DR_OPEN alongside GMT_ONLINE (0x0100_0000) — an empty drive
        // that is otherwise online and ready still reports DR_OPEN.
        assert!(gstat_reports_no_medium(0x0104_0000));
    }

    #[test]
    fn gstat_reports_no_medium_is_false_when_the_bit_is_clear() {
        // A loaded, rewound, online tape: GMT_BOT | GMT_ONLINE, no DR_OPEN.
        assert!(!gstat_reports_no_medium(0x4100_0000));
        assert!(!gstat_reports_no_medium(0));
    }

    #[test]
    fn gstat_reports_no_medium_does_not_confuse_a_neighboring_bit() {
        // GMT_IM_REP_EN (0x0001_0000) is adjacent to DR_OPEN
        // (0x0004_0000) — must not be mistaken for it.
        assert!(!gstat_reports_no_medium(0x0001_0000));
    }

    /// The probe must never look like "no medium" (`true`) when it simply
    /// couldn't read the device at all — a nonexistent path is answered by
    /// `open()` failing outright (ENOENT), instantly, never by blocking; this
    /// pins that `probe_no_medium` treats that as "couldn't tell" (`false`),
    /// not as a positive refusal.
    #[test]
    fn probe_no_medium_on_a_nonexistent_device_is_false_not_a_refusal() {
        assert!(!probe_no_medium(
            "/nonexistent/tapectl-media-detect-probe-device"
        ));
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
        assert!(err.contains("--generation says LTO-6"), "{err}");
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

    /// `--generation` outranks a matched cartridge row. With both present they
    /// must AGREE (see `undetected_media_contradicting_a_registered_row_...`
    /// below), so the priority shows in the reported SOURCE rather than in
    /// the generation: the warning must say "--generation", not "cartridge
    /// BC001", because that is what the operator will go and check.
    #[test]
    fn undetected_falls_back_to_media_first() {
        let (gen, src) = resolve_media(
            &undetected(),
            Some(Generation::Lto6),
            Some((Generation::Lto6, "BC001")),
            Generation::Lto7,
        )
        .unwrap();
        assert_eq!(gen, Generation::Lto6);
        assert_eq!(src, MediaSource::Declared);
        assert!(!src.is_detected());
        assert_eq!(src.describe(), "--generation");
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

    /// Even with nothing detected, `--generation` and a matched cartridge row are
    /// two claims by the same operator, and tapectl CAN see them contradict
    /// each other. The row check therefore runs against the RESOLVED
    /// generation, not only a detected one.
    #[test]
    fn undetected_media_contradicting_a_registered_row_is_an_error() {
        let err = resolve_media(
            &undetected(),
            Some(Generation::Lto8),
            Some((Generation::Lto6, "BC001")),
            Generation::Lto8,
        )
        .unwrap_err()
        .to_string();
        assert!(
            err.contains("cartridge BC001 is registered as LTO-6"),
            "{err}"
        );
        assert!(err.contains("--generation says LTO-8"), "{err}");
    }

    /// ...and the row winning the ladder can never contradict itself.
    #[test]
    fn a_row_that_wins_the_ladder_is_never_self_contradictory() {
        let (gen, src) = resolve_media(
            &undetected(),
            None,
            Some((Generation::Lto6, "BC001")),
            Generation::Lto8,
        )
        .unwrap();
        assert_eq!(gen, Generation::Lto6);
        assert_eq!(src, MediaSource::CartridgeRow("BC001".into()));
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
