//! Compression of a slice alone (`volume-format-v3.md` §3.5; ADR-0012 items
//! 280, 284 to 287; the plan's D15; invariant 11).
//!
//! Each slice's cut is compressed alone, after the cut and before it is
//! encrypted, as one complete stream of its algorithm, the way the
//! algorithm's own tool writes it, so that `age -d` and then that tool give
//! the cut back:
//!
//! - gzip: one member, no file name, a zero time (`flate2`, pure Rust);
//! - xz: one `.xz` stream with the CRC64 check (`liblzma`, bundled C);
//! - zstd: one frame with its content checksum on and the content size
//!   stated (`zstd`, bundled C).
//!
//! One thread for each slice. Every packed byte is also decoded again as it
//! passes ([`Packer`]), and the decoded bytes' sha256 and length are given
//! back, so the caller can compare them with the cut's before the slice is
//! kept (D15: what was hashed is what goes to tape). Neither the cut nor the
//! decoded slice is held whole for that check.
//!
//! The one thing held whole is a zstd slice's cut: zstd states the content
//! size in the frame's header, which is written before the first byte of
//! the frame, and a slice's length is known only when the cutter closes it.
//! So a zstd slice's cut is held in memory until the slice closes (never on
//! disk): at most one slice for each unit being staged.

use std::io::{self, Write};

use sha2::Digest;

use crate::error::{Result, TapectlError};
use crate::pass_count::Sha256;

/// An algorithm Layout 3 carries (item 285).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Algorithm {
    Gzip,
    Xz,
    Zstd,
}

impl Algorithm {
    /// The `compression` value, and the MANIFEST's.
    pub(crate) fn name(self) -> &'static str {
        match self {
            Algorithm::Gzip => "gzip",
            Algorithm::Xz => "xz",
            Algorithm::Zstd => "zstd",
        }
    }

    /// The algorithm's own range of levels (D15, determined).
    pub(crate) fn levels(self) -> (u32, u32) {
        match self {
            Algorithm::Gzip => (1, 9),
            Algorithm::Xz => (0, 9),
            Algorithm::Zstd => (1, 19),
        }
    }

    /// The algorithm's own default level (D15).
    pub(crate) fn default_level(self) -> u32 {
        match self {
            Algorithm::Gzip => 6,
            Algorithm::Xz => 6,
            Algorithm::Zstd => 3,
        }
    }
}

/// How a staging's slices are packed: the algorithm and the level.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Packing {
    pub(crate) algorithm: Algorithm,
    pub(crate) level: u32,
}

/// The values of `compression` Layout 3 carries.
pub(crate) const CARRIED: &str = "none, gzip, xz or zstd";

/// The packing for a resolved `compression` and `compression_level`, or
/// `None` for `none`. A value Layout 3 does not carry, and a level outside
/// the algorithm's range, are refused naming the key. A level beside
/// `none` packs nothing and is not judged.
pub(crate) fn packing(compression: &str, level: Option<i64>) -> Result<Option<Packing>> {
    let algorithm = match compression {
        "none" | "" => return Ok(None),
        "gzip" => Algorithm::Gzip,
        "xz" => Algorithm::Xz,
        "zstd" => Algorithm::Zstd,
        other => {
            return Err(TapectlError::Other(format!(
                "compression = \"{other}\" is not one Layout 3 carries: it is {CARRIED} \
                 (ADR-0012 item 285). Set compression to one of those, in the unit's \
                 dotfile, its archive set or [defaults]."
            )))
        }
    };
    let (low, high) = algorithm.levels();
    let level = match level {
        None => algorithm.default_level(),
        Some(l) if l >= i64::from(low) && l <= i64::from(high) => l as u32,
        Some(l) => {
            return Err(TapectlError::Other(format!(
                "compression_level = {l} is outside {}'s levels, {low} to {high} \
                 (its default is {})",
                algorithm.name(),
                algorithm.default_level()
            )))
        }
    };
    Ok(Some(Packing { algorithm, level }))
}

/// The most a slice's packing can add to its cut, as the format states it
/// for each of gzip, xz and zstd: 0.1% of `slice_size` plus 64 KiB
/// (`volume-format-v3.md` §3.5, from T0.5's measurement with a margin).
pub(crate) fn worst_growth(slice_size: u64) -> u64 {
    slice_size.div_ceil(1000) + 64 * 1024
}

/// What the check decoded: the length and the sha256 (hex) of the bytes the
/// packed slice decodes to, and the packed length.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Decoded {
    pub(crate) len: u64,
    pub(crate) sha256: String,
    pub(crate) packed_len: u64,
}

/// The decoded bytes, hashed and counted, never held.
struct Counted {
    hasher: Sha256,
    len: u64,
}

impl Write for Counted {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.hasher.update(buf);
        self.len += buf.len() as u64;
        Ok(buf.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn counted() -> Counted {
    Counted {
        hasher: Sha256::new(),
        len: 0,
    }
}

/// The decoder of the check.
enum Check {
    Gzip(flate2::write::GzDecoder<Counted>),
    Xz(liblzma::write::XzDecoder<Counted>),
    Zstd(zstd::stream::write::Decoder<'static, Counted>),
}

impl Check {
    fn new(algorithm: Algorithm) -> io::Result<Self> {
        Ok(match algorithm {
            Algorithm::Gzip => Check::Gzip(flate2::write::GzDecoder::new(counted())),
            Algorithm::Xz => Check::Xz(liblzma::write::XzDecoder::new(counted())),
            Algorithm::Zstd => Check::Zstd(zstd::stream::write::Decoder::new(counted())?),
        })
    }

    fn write_all(&mut self, buf: &[u8]) -> io::Result<()> {
        match self {
            Check::Gzip(d) => d.write_all(buf),
            Check::Xz(d) => d.write_all(buf),
            Check::Zstd(d) => d.write_all(buf),
        }
    }

    fn finish(self) -> io::Result<Counted> {
        match self {
            Check::Gzip(d) => d.finish(),
            Check::Xz(d) => d.finish(),
            Check::Zstd(d) => d.finish(),
        }
    }
}

/// The packed bytes: on to `out`, and through the check's decoder.
struct Tee<W> {
    out: W,
    check: Check,
    packed: u64,
}

impl<W: Write> Write for Tee<W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let n = self.out.write(buf)?;
        self.check
            .write_all(&buf[..n])
            .map_err(|e| io::Error::other(format!("the packed slice does not decode: {e}")))?;
        self.packed += n as u64;
        Ok(n)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.out.flush()
    }
}

enum Inner<W: Write> {
    Gzip(flate2::write::GzEncoder<Tee<W>>),
    Xz(liblzma::write::XzEncoder<Tee<W>>),
    /// The cut, held until the slice closes (the frame states its size).
    Zstd {
        cut: Vec<u8>,
        level: u32,
        tee: Tee<W>,
    },
}

/// One slice being packed: its cut written in, the packed frame written to
/// `out`, and decoded again as it goes.
pub(crate) struct Packer<W: Write> {
    inner: Inner<W>,
}

impl<W: Write> Packer<W> {
    pub(crate) fn new(packing: Packing, out: W) -> io::Result<Self> {
        let tee = Tee {
            out,
            check: Check::new(packing.algorithm)?,
            packed: 0,
        };
        let inner = match packing.algorithm {
            Algorithm::Gzip => Inner::Gzip(
                flate2::GzBuilder::new()
                    .mtime(0)
                    .write(tee, flate2::Compression::new(packing.level)),
            ),
            Algorithm::Xz => Inner::Xz(liblzma::write::XzEncoder::new(tee, packing.level)),
            Algorithm::Zstd => Inner::Zstd {
                cut: Vec::new(),
                level: packing.level,
                tee,
            },
        };
        Ok(Self { inner })
    }

    /// Finish the frame: returns `out` and what the check decoded.
    pub(crate) fn finish(self) -> io::Result<(W, Decoded)> {
        let tee = match self.inner {
            Inner::Gzip(e) => e.finish()?,
            Inner::Xz(e) => e.finish()?,
            Inner::Zstd { cut, level, tee } => {
                let mut e = zstd::stream::write::Encoder::new(tee, level as i32)?;
                e.include_checksum(true)?;
                e.include_contentsize(true)?;
                e.set_pledged_src_size(Some(cut.len() as u64))?;
                e.write_all(&cut)?;
                drop(cut);
                e.finish()?
            }
        };
        let Tee { out, check, packed } = tee;
        let decoded = check
            .finish()
            .map_err(|e| io::Error::other(format!("the packed slice does not decode: {e}")))?;
        Ok((
            out,
            Decoded {
                len: decoded.len,
                sha256: format!("{:x}", decoded.hasher.finalize()),
                packed_len: packed,
            },
        ))
    }
}

impl<W: Write> Write for Packer<W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        match &mut self.inner {
            Inner::Gzip(e) => e.write(buf),
            Inner::Xz(e) => e.write(buf),
            Inner::Zstd { cut, .. } => {
                cut.extend_from_slice(buf);
                Ok(buf.len())
            }
        }
    }
    fn flush(&mut self) -> io::Result<()> {
        match &mut self.inner {
            Inner::Gzip(e) => e.flush(),
            Inner::Xz(e) => e.flush(),
            Inner::Zstd { .. } => Ok(()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::{Command, Stdio};

    fn sha(b: &[u8]) -> String {
        format!("{:x}", <sha2::Sha256 as Digest>::digest(b))
    }

    fn pack(p: Packing, cut: &[u8]) -> (Vec<u8>, Decoded) {
        let mut packer = Packer::new(p, Vec::new()).unwrap();
        for chunk in cut.chunks(7777) {
            packer.write_all(chunk).unwrap();
        }
        packer.finish().unwrap()
    }

    /// The host's own tool, given the frame on its standard input.
    fn tool_decodes(algorithm: Algorithm, frame: &[u8]) -> Option<Vec<u8>> {
        let (prog, args): (&str, &[&str]) = match algorithm {
            Algorithm::Gzip => ("gzip", &["-dc"]),
            Algorithm::Xz => ("xz", &["-dc"]),
            Algorithm::Zstd => ("zstd", &["-dcq"]),
        };
        let mut child = match Command::new(prog)
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
        {
            Ok(c) => c,
            Err(_) => return None,
        };
        let mut stdin = child.stdin.take().unwrap();
        let data = frame.to_vec();
        let feeder = std::thread::spawn(move || stdin.write_all(&data));
        let out = child.wait_with_output().unwrap();
        feeder.join().unwrap().unwrap();
        assert!(out.status.success(), "{prog} decodes the frame");
        Some(out.stdout)
    }

    fn mixed(n: usize) -> Vec<u8> {
        let mut v = Vec::with_capacity(n);
        let mut x: u32 = 7;
        while v.len() < n {
            x = x.wrapping_mul(1_103_515_245).wrapping_add(12345);
            if (x >> 20) % 3 == 0 {
                v.extend_from_slice(b"the same words again and again ");
            } else {
                v.push((x >> 16) as u8);
            }
        }
        v.truncate(n);
        v
    }

    const ALL: [Algorithm; 3] = [Algorithm::Gzip, Algorithm::Xz, Algorithm::Zstd];

    /// Each algorithm at its default, its lowest and its highest level: the
    /// check decodes the frame to the cut, and so does the host's own tool.
    #[test]
    fn each_frame_is_read_by_the_hosts_tool_and_by_the_check() {
        let cut = mixed(300_000);
        for algorithm in ALL {
            let (low, high) = algorithm.levels();
            for level in [algorithm.default_level(), low, high] {
                let p = Packing { algorithm, level };
                let (frame, decoded) = pack(p, &cut);
                assert_eq!(decoded.len, cut.len() as u64, "{p:?}");
                assert_eq!(decoded.sha256, sha(&cut), "{p:?}");
                assert_eq!(decoded.packed_len, frame.len() as u64, "{p:?}");
                assert!(frame.len() < cut.len(), "{p:?} packs words");
                match tool_decodes(algorithm, &frame) {
                    Some(out) => assert_eq!(sha(&out), sha(&cut), "{p:?}: the tool"),
                    None => eprintln!("skipping the tool for {}: not installed", algorithm.name()),
                }
            }
        }
    }

    /// The frame's fields (D15): a gzip member with no name and a zero
    /// time; an xz stream with the CRC64 check; a zstd frame with its
    /// checksum and its content size.
    #[test]
    fn each_frame_has_the_fields_d15_determines() {
        let cut = mixed(10_000);
        let (gz, _) = pack(
            Packing {
                algorithm: Algorithm::Gzip,
                level: 6,
            },
            &cut,
        );
        assert_eq!(&gz[..3], &[0x1f, 0x8b, 8], "a gzip member, deflate");
        assert_eq!(gz[3] & 0x08, 0, "no file name (FNAME clear)");
        assert_eq!(&gz[4..8], &[0, 0, 0, 0], "a zero time");

        let (xz, _) = pack(
            Packing {
                algorithm: Algorithm::Xz,
                level: 6,
            },
            &cut,
        );
        assert_eq!(&xz[..6], b"\xfd7zXZ\0", "an xz stream");
        assert_eq!(xz[7] & 0x0f, 0x04, "the stream flags name CRC64");

        let (zs, _) = pack(
            Packing {
                algorithm: Algorithm::Zstd,
                level: 3,
            },
            &cut,
        );
        assert_eq!(&zs[..4], &[0x28, 0xb5, 0x2f, 0xfd], "a zstd frame");
        let fhd = zs[4];
        assert_ne!(fhd & 0x04, 0, "the content checksum flag");
        assert!(
            fhd >> 6 != 0 || fhd & 0x20 != 0,
            "the content size is stated"
        );
        assert_eq!(
            zstd::zstd_safe::get_frame_content_size(&zs).unwrap(),
            Some(cut.len() as u64)
        );
    }

    /// Bytes that do not shrink grow by less than the format's worst growth.
    #[test]
    fn bytes_that_do_not_shrink_grow_less_than_the_stated_worst() {
        let mut cut = vec![0u8; 1 << 20];
        let mut x: u64 = 0x9e37_79b9_7f4a_7c15;
        for b in cut.iter_mut() {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            *b = x as u8;
        }
        for algorithm in ALL {
            let (low, high) = algorithm.levels();
            for level in [low, high] {
                let (frame, decoded) = pack(Packing { algorithm, level }, &cut);
                assert_eq!(decoded.sha256, sha(&cut));
                assert!(
                    frame.len() as u64 <= cut.len() as u64 + worst_growth(cut.len() as u64),
                    "{} {level}: {} of {}",
                    algorithm.name(),
                    frame.len(),
                    cut.len()
                );
            }
        }
    }

    /// An empty cut is still one frame the tool reads.
    #[test]
    fn an_empty_cut_is_one_frame() {
        for algorithm in ALL {
            let (frame, decoded) = pack(
                Packing {
                    algorithm,
                    level: algorithm.default_level(),
                },
                &[],
            );
            assert_eq!(decoded.len, 0);
            if let Some(out) = tool_decodes(algorithm, &frame) {
                assert!(out.is_empty());
            }
        }
    }

    #[test]
    fn levels_are_judged_in_each_algorithms_range() {
        assert_eq!(packing("none", Some(99)).unwrap(), None);
        assert_eq!(
            packing("zstd", None).unwrap(),
            Some(Packing {
                algorithm: Algorithm::Zstd,
                level: 3
            })
        );
        assert_eq!(packing("gzip", None).unwrap().unwrap().level, 6);
        assert_eq!(packing("xz", Some(0)).unwrap().unwrap().level, 0);
        for (alg, bad) in [
            ("gzip", 0),
            ("gzip", 10),
            ("xz", -1),
            ("xz", 10),
            ("zstd", 0),
            ("zstd", 20),
        ] {
            let e = packing(alg, Some(bad)).unwrap_err().to_string();
            assert!(e.contains("compression_level"), "{alg} {bad}: {e}");
            assert!(e.contains(&bad.to_string()), "{e}");
        }
        let e = packing("bzip2", None).unwrap_err().to_string();
        assert!(e.contains("compression") && e.contains("bzip2"), "{e}");
    }

    #[test]
    fn the_worst_growth_is_a_tenth_of_a_percent_and_64_kib() {
        assert_eq!(worst_growth(1 << 30), (1u64 << 30).div_ceil(1000) + 65536);
        assert_eq!(worst_growth(0), 65536);
    }
}
