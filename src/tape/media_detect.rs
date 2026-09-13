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
}
