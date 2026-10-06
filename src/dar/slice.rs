//! dar's slice layer, applied by tapectl to dar's archive on standard output
//! (issue #370, ADR-0012 amendment 2026-10-06 item 4).
//!
//! `dar -c -` writes a single-slice archive to stdout and refuses `-s`
//! there, so tapectl cuts the stream into dar slices itself. A sliced dar
//! archive is the same logical stream framed slice by slice
//! (`docs/research/2026-10-06-plaintext-free-staging.md` §3.4, measured
//! against `dar_xform`): every slice is the slice header, a run of payload
//! bytes, and a one-byte trailer — `N` on every slice but the last, `T` on
//! the last. The payload, concatenated, is the stdout stream without its own
//! header and its trailing `T`. The slices this module produces are what
//! `dar_xform -s <size> - <base>` produces from the same stream, apart from
//! the random internal name dar_xform draws afresh; `dar -t`, `dar -x` and
//! RESTORE.sh read them as dar's own.
//!
//! **The header is dar's, never hand-encoded.** [`template`] runs the
//! installed dar on an empty directory with the operator's `-s` string and
//! reads the first slice's header back; [`SliceFrame::new`] parses both that
//! header and the stream's (magic, internal name, flag, extension, TLV
//! list), refuses anything it does not recognise, and swaps the stream's two
//! labels into the template. The slice size therefore stays dar's own
//! parse of the operator's string (issue #59), and its width — 50 bytes of
//! header at `-s 1M`, `4M` and `1G`, 54 at `10G` — is read, not assumed.
//!
//! Wire format, as dar 2.7 writes it (libdar `header.cpp`, measured):
//!
//! ```text
//! magic        4 bytes   00 00 00 7b
//! internal     10 bytes  the random label of this slice set
//! flag         1 byte    'T' last slice (stdout) | 'N' not last | 'E' flag at the end
//! extension    1 byte    'T' a TLV list follows
//! TLV count    infinint
//! TLV*         type: u16 big-endian, length: infinint, value
//!                type 1 = slice size (an infinint), type 3 = data name (10 bytes)
//! ```
//!
//! An infinint is a width preamble — zero or more `00` bytes, then one byte
//! with a single bit set — followed by the value, big-endian, in a whole
//! number of 4-byte groups: `80` = one group, `40` = two, each leading `00`
//! adds eight.

use std::io::{self, Read};
use std::path::Path;

use crate::error::{Result, TapectlError};

const MAGIC: [u8; 4] = [0x00, 0x00, 0x00, 0x7b];
const LABEL_LEN: usize = 10;
const TLV_SLICE_SIZE: u16 = 1;
const TLV_DATA_NAME: u16 = 3;
/// The flag byte a non-terminal slice ends with.
const NOT_LAST: u8 = b'N';
/// The flag byte the last slice — and the stdout stream — ends with.
const LAST: u8 = b'T';
/// dar's infinint groups are this many bytes wide.
const INFININT_GROUP: usize = 4;
/// No header tapectl accepts is longer than this; a longer one is refused,
/// so a corrupt length can never make the parser read unbounded input.
const MAX_HEADER: usize = 4096;

fn refuse(what: impl std::fmt::Display) -> TapectlError {
    TapectlError::Dar(format!(
        "dar slice framing: {what} — refusing to cut the archive rather than guess \
         (a dar version whose slice header tapectl does not know would land here)"
    ))
}

/// One parsed dar slice header (the "sar" header).
#[derive(Debug, Clone)]
struct Header {
    /// The header's bytes, exactly as read.
    bytes: Vec<u8>,
    /// The flag byte: `T`, `N` or `E`.
    flag: u8,
    /// Where the data-name label's 10 bytes sit in `bytes`.
    data_name_at: usize,
    /// The slice size the header records (TLV type 1), if any.
    slice_size: Option<u64>,
}

/// Reads one byte at a time from `r`, recording every byte, so the header
/// parser can consume exactly the header and nothing past it.
struct Recorder<'a, R> {
    r: &'a mut R,
    bytes: Vec<u8>,
}

impl<R: Read> Recorder<'_, R> {
    fn byte(&mut self) -> Result<u8> {
        if self.bytes.len() >= MAX_HEADER {
            return Err(refuse(format!("a header longer than {MAX_HEADER} bytes")));
        }
        let mut b = [0u8; 1];
        read_full(self.r, &mut b).map_err(|e| match e.kind() {
            io::ErrorKind::UnexpectedEof => refuse("the header ends early"),
            _ => TapectlError::Io(e),
        })?;
        self.bytes.push(b[0]);
        Ok(b[0])
    }

    fn take(&mut self, n: usize) -> Result<&[u8]> {
        let start = self.bytes.len();
        for _ in 0..n {
            self.byte()?;
        }
        Ok(&self.bytes[start..])
    }

    /// One infinint, as a u64 (a wider value is refused).
    fn infinint(&mut self) -> Result<u64> {
        let mut zeros = 0usize;
        let preamble = loop {
            let b = self.byte()?;
            if b != 0 {
                break b;
            }
            zeros += 1;
        };
        if preamble.count_ones() != 1 {
            return Err(refuse(format!(
                "an integer width byte {preamble:#04x} with more than one bit set"
            )));
        }
        let groups = zeros * 8 + preamble.leading_zeros() as usize + 1;
        let width = groups * INFININT_GROUP;
        let value = self.take(width)?;
        let significant = value.iter().skip_while(|&&b| b == 0).count();
        if significant > 8 {
            return Err(refuse(format!("an integer {width} bytes wide")));
        }
        Ok(value.iter().fold(0u64, |acc, &b| (acc << 8) | u64::from(b)))
    }
}

/// `read_exact` that retries `EINTR` (a signal handler is installed, issue
/// #404) and reports a short stream as `UnexpectedEof`.
fn read_full<R: Read>(r: &mut R, mut buf: &mut [u8]) -> io::Result<()> {
    while !buf.is_empty() {
        match r.read(buf) {
            Ok(0) => return Err(io::ErrorKind::UnexpectedEof.into()),
            Ok(n) => buf = &mut buf[n..],
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

/// Parse one dar slice header from the front of `r`, consuming exactly it.
fn parse_header<R: Read>(r: &mut R) -> Result<Header> {
    let mut rec = Recorder {
        r,
        bytes: Vec::with_capacity(64),
    };
    if rec.take(MAGIC.len())? != MAGIC {
        return Err(refuse("the data does not start with dar's magic number"));
    }
    rec.take(LABEL_LEN)?;
    let flag = rec.byte()?;
    if !matches!(flag, LAST | NOT_LAST | b'E') {
        return Err(refuse(format!("an unknown slice flag {:?}", flag as char)));
    }
    let extension = rec.byte()?;
    if extension != b'T' {
        return Err(refuse(format!(
            "header extension {:?}, not a TLV list",
            extension as char
        )));
    }
    let count = rec.infinint()?;
    if count > 16 {
        return Err(refuse(format!("{count} header fields")));
    }
    let mut data_name_at = None;
    let mut slice_size = None;
    for _ in 0..count {
        let t = rec.take(2)?;
        let tlv_type = u16::from_be_bytes([t[0], t[1]]);
        let len = rec.infinint()? as usize;
        if len > MAX_HEADER {
            return Err(refuse(format!("a header field {len} bytes long")));
        }
        let at = rec.bytes.len();
        match tlv_type {
            TLV_DATA_NAME => {
                if len != LABEL_LEN || data_name_at.is_some() {
                    return Err(refuse(format!("a data-name field {len} bytes long")));
                }
                rec.take(len)?;
                data_name_at = Some(at);
            }
            TLV_SLICE_SIZE => {
                if slice_size.is_some() {
                    return Err(refuse("two slice-size fields"));
                }
                let size = rec.infinint()?;
                if rec.bytes.len() - at != len {
                    return Err(refuse("a slice-size field whose length disagrees with it"));
                }
                slice_size = Some(size);
            }
            other => return Err(refuse(format!("an unknown header field type {other}"))),
        }
    }
    let data_name_at = data_name_at.ok_or_else(|| refuse("no data-name field"))?;
    Ok(Header {
        bytes: rec.bytes,
        flag,
        data_name_at,
        slice_size,
    })
}

/// The header every slice of one archive carries, and how many payload
/// bytes each slice holds.
#[derive(Debug, Clone)]
pub struct SliceFrame {
    header: Vec<u8>,
    slice_size: u64,
}

impl SliceFrame {
    /// The frame for cutting the stream whose header is `stream_header`,
    /// built from `template` — the first bytes of slice 1 of an archive
    /// the installed dar wrote with the same `-s` ([`template`]).
    fn new(template: &[u8], stream_header: &Header) -> Result<Self> {
        let tmpl = parse_header(&mut &template[..])?;
        if tmpl.flag != b'E' {
            return Err(refuse(format!(
                "the template slice's flag is {:?}, not 'E' (flag at the end)",
                tmpl.flag as char
            )));
        }
        let slice_size = tmpl
            .slice_size
            .ok_or_else(|| refuse("the template slice records no slice size"))?;
        if stream_header.flag != LAST || stream_header.slice_size.is_some() {
            return Err(refuse(
                "dar's standard output is not a single-slice archive (flag 'T', no slice size)",
            ));
        }
        let mut header = tmpl.bytes.clone();
        // The internal name, then the data name, are the stream's: the data
        // name is what dar checks an isolated catalogue against (`-A`).
        header[MAGIC.len()..MAGIC.len() + LABEL_LEN]
            .copy_from_slice(&stream_header.bytes[MAGIC.len()..MAGIC.len() + LABEL_LEN]);
        header[tmpl.data_name_at..tmpl.data_name_at + LABEL_LEN].copy_from_slice(
            &stream_header.bytes
                [stream_header.data_name_at..stream_header.data_name_at + LABEL_LEN],
        );
        if slice_size <= header.len() as u64 + 1 {
            return Err(refuse(format!(
                "a slice size of {slice_size} bytes leaves no room for data after a \
                 {}-byte header",
                header.len()
            )));
        }
        Ok(Self { header, slice_size })
    }

    /// The slice header, identical in every slice.
    pub fn header(&self) -> &[u8] {
        &self.header
    }

    /// The slice size dar parsed from the operator's `-s` string, in bytes.
    pub fn slice_size(&self) -> u64 {
        self.slice_size
    }

    /// Payload bytes in every slice but the last: the slice size less the
    /// header and the one-byte trailer.
    pub fn payload_per_slice(&self) -> u64 {
        self.slice_size - self.header.len() as u64 - 1
    }
}

/// Run the installed dar on an empty directory with slice size `slice_size`
/// (the operator's string, unparsed) inside `work_dir`, and return the first
/// bytes of its first slice: a slice header for [`cut_stream`]. `work_dir`
/// must be empty and must not be under the staging directory; the archive
/// holds nothing but an empty directory.
pub fn template(dar_binary: &str, slice_size: &str, work_dir: &Path) -> Result<Vec<u8>> {
    let empty = work_dir.join("empty");
    std::fs::create_dir_all(&empty)?;
    let base = work_dir.join("template");
    let mut cmd = super::command(dar_binary);
    cmd.arg("-c")
        .arg(&base)
        .arg("-R")
        .arg(&empty)
        .arg("-s")
        .arg(slice_size)
        .args(["-an", "-D", "-Q"]);
    let out = super::run_interruptible(&mut cmd, || {
        "dar was stopped while writing a slice template".to_string()
    })?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        return Err(TapectlError::Dar(format!(
            "dar could not write a slice template at -s {slice_size} (exit {}): {}",
            out.status,
            stderr.lines().take(5).collect::<Vec<_>>().join("\n")
        )));
    }
    let first = work_dir.join("template.1.dar");
    let mut bytes = Vec::new();
    std::fs::File::open(&first)?
        .take(MAX_HEADER as u64)
        .read_to_end(&mut bytes)?;
    Ok(bytes)
}

/// The stream's payload: every byte after its header except the last, which
/// is held back because it is the stream's own trailer (`T`), not data.
pub struct Payload<R> {
    inner: R,
    buf: Box<[u8]>,
    start: usize,
    end: usize,
    eof: bool,
    /// Bytes read from the stream so far, header included.
    consumed: u64,
}

/// Read buffer for the stream: the only plaintext tapectl holds, besides
/// age's own 64 KiB chunk.
const PAYLOAD_BUFFER: usize = 1 << 20;

impl<R: Read> Payload<R> {
    fn new(inner: R, header_len: usize) -> Self {
        Self {
            inner,
            buf: vec![0u8; PAYLOAD_BUFFER].into_boxed_slice(),
            start: 0,
            end: 0,
            eof: false,
            consumed: header_len as u64,
        }
    }

    /// Fill until at least two bytes are buffered or the stream has ended,
    /// so that everything but the last buffered byte is certainly payload.
    fn fill(&mut self) -> io::Result<()> {
        while self.end - self.start < 2 && !self.eof {
            if self.start > 0 {
                self.buf.copy_within(self.start..self.end, 0);
                self.end -= self.start;
                self.start = 0;
            }
            match self.inner.read(&mut self.buf[self.end..]) {
                Ok(0) => self.eof = true,
                Ok(n) => {
                    self.end += n;
                    self.consumed += n as u64;
                }
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) => return Err(e),
            }
        }
        Ok(())
    }

    /// Up to `max` payload bytes, without consuming them.
    fn peek(&mut self, max: usize) -> io::Result<&[u8]> {
        self.fill()?;
        let available = (self.end - self.start).saturating_sub(1).min(max);
        Ok(&self.buf[self.start..self.start + available])
    }

    fn consume(&mut self, n: usize) {
        self.start += n;
    }

    /// Whether the payload is exhausted — only the trailer remains.
    fn at_end(&mut self) -> io::Result<bool> {
        self.fill()?;
        Ok(self.eof && self.end - self.start <= 1)
    }

    /// The stream's last byte, once [`Self::at_end`].
    fn trailer(&self) -> Option<u8> {
        (self.end - self.start == 1).then(|| self.buf[self.start])
    }
}

/// What [`cut_stream`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CutSummary {
    /// Slices produced, numbered from 1.
    pub slices: u32,
    /// Bytes read from the stream, header and trailer included.
    pub stream_bytes: u64,
    /// The slice size dar parsed from the operator's `-s` string.
    pub slice_size: u64,
}

/// Cut dar's stdout `stream` into dar slices framed with `template`'s
/// header ([`template`]).
///
/// For each slice `n` (from 1) it calls `open(n)` for a sink, writes the
/// whole slice into it — header, payload, trailer — and hands the sink to
/// `close(n, sink)`, before slice `n + 1` is opened. Nothing is buffered
/// beyond [`PAYLOAD_BUFFER`]: memory does not depend on the slice size.
/// `tick(stream_bytes)` runs after every read from the stream; an `Err`
/// from it stops the cut, which is how a caller stops for a signal or for a
/// failure elsewhere. Any framing surprise is a refusal, never a guess.
pub fn cut_stream<R, S, O, C, T>(
    stream: R,
    template: &[u8],
    mut open: O,
    mut close: C,
    mut tick: T,
) -> Result<CutSummary>
where
    R: Read,
    S: io::Write,
    O: FnMut(u32) -> Result<S>,
    C: FnMut(u32, S) -> Result<()>,
    T: FnMut(u64) -> Result<()>,
{
    let mut stream = stream;
    let stream_header = parse_header(&mut stream)?;
    let frame = SliceFrame::new(template, &stream_header)?;
    let per_slice = frame.payload_per_slice();
    let mut payload = Payload::new(stream, stream_header.bytes.len());
    tick(payload.consumed)?;

    let mut n: u32 = 0;
    loop {
        n = n
            .checked_add(1)
            .ok_or_else(|| refuse("more than 2^32 slices"))?;
        let mut sink = open(n)?;
        sink.write_all(frame.header())?;
        let mut left = per_slice;
        while left > 0 {
            let want = usize::try_from(left).unwrap_or(usize::MAX);
            let before = payload.consumed;
            let chunk = payload.peek(want)?;
            if chunk.is_empty() {
                break;
            }
            let len = chunk.len();
            sink.write_all(chunk)?;
            payload.consume(len);
            left -= len as u64;
            if payload.consumed != before {
                tick(payload.consumed)?;
            }
        }
        let last = payload.at_end()?;
        if last {
            match payload.trailer() {
                Some(LAST) => {}
                Some(other) => {
                    return Err(refuse(format!(
                        "the stream ends with {:?}, not dar's last-slice flag 'T'",
                        other as char
                    )))
                }
                None => return Err(refuse("the stream ends without a trailer")),
            }
        }
        sink.write_all(&[if last { LAST } else { NOT_LAST }])?;
        close(n, sink)?;
        if last {
            tick(payload.consumed)?;
            return Ok(CutSummary {
                slices: n,
                stream_bytes: payload.consumed,
                slice_size: frame.slice_size(),
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    /// A header built by hand in dar's layout, for the parser's refusals.
    fn header(flag: u8, slice_size: Option<&[u8]>) -> Vec<u8> {
        let mut h = MAGIC.to_vec();
        h.extend_from_slice(b"0123456789");
        h.push(flag);
        h.push(b'T');
        let count = 1 + u8::from(slice_size.is_some());
        h.extend_from_slice(&[0x80, 0, 0, 0, count]);
        if let Some(size) = slice_size {
            h.extend_from_slice(&[0, 1, 0x80, 0, 0, 0, size.len() as u8]);
            h.extend_from_slice(size);
        }
        h.extend_from_slice(&[0, 3, 0x80, 0, 0, 0, 10]);
        h.extend_from_slice(b"DATANAME!!");
        h
    }

    #[test]
    fn parses_a_four_mebibyte_a_one_gibibyte_and_a_ten_gibibyte_header() {
        let one_g = header(b'E', Some(&[0x80, 0x40, 0x00, 0x00, 0x00]));
        let h = parse_header(&mut &one_g[..]).unwrap();
        assert_eq!(h.bytes.len(), 50);
        assert_eq!(h.slice_size, Some(1 << 30));

        let four = header(b'E', Some(&[0x80, 0x00, 0x40, 0x00, 0x00]));
        let h = parse_header(&mut &four[..]).unwrap();
        assert_eq!(h.bytes.len(), 50);
        assert_eq!(h.slice_size, Some(4 << 20));
        assert_eq!(&h.bytes[h.data_name_at..h.data_name_at + 10], b"DATANAME!!");

        let ten = header(
            b'E',
            Some(&[0x40, 0x00, 0x00, 0x00, 0x02, 0x80, 0x00, 0x00, 0x00]),
        );
        let h = parse_header(&mut &ten[..]).unwrap();
        assert_eq!(h.bytes.len(), 54);
        assert_eq!(h.slice_size, Some(10 << 30));
    }

    #[test]
    fn refuses_what_it_does_not_recognise() {
        let mut bad_magic = header(b'T', None);
        bad_magic[3] = 0x7c;
        let mut unknown_field = header(b'T', None);
        let at = unknown_field.len() - 17;
        unknown_field[at + 1] = 9;
        let mut bad_width = header(b'T', None);
        bad_width[16] = 0xC0;
        for (what, bytes) in [
            ("magic", bad_magic),
            ("field type", unknown_field),
            ("integer width", bad_width),
            ("truncated", header(b'T', None)[..20].to_vec()),
        ] {
            let err = parse_header(&mut &bytes[..]).unwrap_err().to_string();
            assert!(
                err.contains("refusing to cut"),
                "{what} must be refused: {err}"
            );
        }
    }

    #[test]
    fn a_slice_size_too_small_for_the_header_is_refused() {
        let tmpl = header(b'E', Some(&[0x80, 0, 0, 0, 40]));
        let stream = parse_header(&mut &header(b'T', None)[..]).unwrap();
        let err = SliceFrame::new(&tmpl, &stream).unwrap_err().to_string();
        assert!(err.contains("leaves no room"), "{err}");
    }

    /// Cut `stream` with a hand-built frame into in-memory slices.
    fn cut_in_memory(stream: &[u8], slice_size: u8) -> Vec<Vec<u8>> {
        let tmpl = header(b'E', Some(&[0x80, 0, 0, 0, slice_size]));
        let mut slices = Vec::new();
        cut_stream(
            stream,
            &tmpl,
            |_| Ok(Vec::new()),
            |_, s| {
                slices.push(s);
                Ok(())
            },
            |_| Ok(()),
        )
        .unwrap();
        slices
    }

    #[test]
    fn cuts_payload_into_full_slices_and_a_short_last_one() {
        let mut stream = header(b'T', None);
        let hlen = header(b'E', Some(&[0x80, 0, 0, 0, 0])).len();
        let payload: Vec<u8> = (0..25u8).collect();
        stream.extend_from_slice(&payload);
        stream.push(b'T');
        // 10 payload bytes per slice.
        let slices = cut_in_memory(&stream, (hlen + 11) as u8);
        assert_eq!(slices.len(), 3);
        let flags: Vec<u8> = slices.iter().map(|s| *s.last().unwrap()).collect();
        assert_eq!(flags, b"NNT");
        let rejoined: Vec<u8> = slices
            .iter()
            .flat_map(|s| s[hlen..s.len() - 1].to_vec())
            .collect();
        assert_eq!(rejoined, payload);
        for s in &slices {
            assert_eq!(&s[4..14], b"0123456789", "the stream's internal name");
            assert_eq!(&s[hlen - 10..hlen], b"DATANAME!!", "the stream's data name");
        }
    }

    #[test]
    fn a_payload_ending_on_a_slice_boundary_marks_the_full_slice_last() {
        // dar_xform does the same (measured, docs/research §3.4 follow-up).
        let mut stream = header(b'T', None);
        let hlen = header(b'E', Some(&[0x80, 0, 0, 0, 0])).len();
        stream.extend_from_slice(&[7u8; 20]);
        stream.push(b'T');
        let slices = cut_in_memory(&stream, (hlen + 11) as u8);
        assert_eq!(slices.len(), 2);
        assert_eq!(*slices[1].last().unwrap(), b'T');
        assert_eq!(slices[1].len(), hlen + 11);
    }

    #[test]
    fn a_stream_not_ending_in_t_is_refused() {
        let mut stream = header(b'T', None);
        stream.extend_from_slice(&[1, 2, 3, b'N']);
        let tmpl = header(b'E', Some(&[0x80, 0, 0, 0, 200]));
        let err = cut_stream(
            &stream[..],
            &tmpl,
            |_| Ok(Vec::new()),
            |_, _| Ok(()),
            |_| Ok(()),
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("not dar's last-slice flag"), "{err}");
    }

    fn run(cmd: &mut Command) -> std::process::Output {
        let out = cmd
            .output()
            .expect("dar must be on PATH (tests/test_dependencies.rs)");
        assert!(
            out.status.success(),
            "{cmd:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        out
    }

    /// The pin a dar upgrade must pass (issue #370): for 1 MiB, 4 MiB, 1 GiB
    /// and 10 GiB slices, tapectl's slices of a real `dar -c -` stream are
    /// `dar_xform`'s slices of the same stream, byte for byte, except the
    /// internal name `dar_xform` draws afresh; and `dar -t` accepts them.
    #[test]
    fn slices_match_dar_xform_at_one_four_and_ten_gib() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("src");
        std::fs::create_dir_all(src.join("d")).unwrap();
        let mut x: u64 = 0x2545_F491_4F6C_DD1D;
        let noise: Vec<u8> = (0..(5 << 20) + 12345)
            .map(|_| {
                x ^= x << 13;
                x ^= x >> 7;
                x ^= x << 17;
                x as u8
            })
            .collect();
        std::fs::write(src.join("noise.bin"), &noise).unwrap();
        std::fs::write(src.join("d/small.txt"), b"small").unwrap();
        let stream = run(Command::new("dar").args(["-c", "-", "-R"]).arg(&src).args([
            "-an",
            "-D",
            "-Q",
            "--retry-on-change",
            "0",
        ]))
        .stdout;

        // The slice size is an infinint in the header, so the header's length
        // follows it: one 4-byte group up to 4 GiB, two above. 1G is the
        // default since ADR-0012's 2026-10-06 amendment, 10G before it.
        for (i, (size, header_len)) in [("1M", 50), ("4M", 50), ("1G", 50), ("10G", 54)]
            .into_iter()
            .enumerate()
        {
            let work = tmp.path().join(format!("w{i}"));
            let mine = work.join("mine");
            let theirs = work.join("theirs");
            std::fs::create_dir_all(&mine).unwrap();
            std::fs::create_dir_all(&theirs).unwrap();
            let tmpl = template("dar", size, &work.join("tmpl")).unwrap();
            assert_eq!(
                parse_header(&mut &tmpl[..]).unwrap().bytes.len(),
                header_len,
                "dar's slice header at -s {size}"
            );
            let summary = cut_stream(
                &stream[..],
                &tmpl,
                |n| Ok(std::fs::File::create(mine.join(format!("a.{n}.dar")))?),
                |_, _| Ok(()),
                |_| Ok(()),
            )
            .unwrap();
            assert_eq!(summary.stream_bytes, stream.len() as u64);

            let mut xform = Command::new("dar_xform")
                .args(["-s", size, "-Q", "-"])
                .arg(theirs.join("a"))
                .stdin(std::process::Stdio::piped())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::piped())
                .spawn()
                .expect("dar_xform ships with dar");
            use std::io::Write;
            xform.stdin.take().unwrap().write_all(&stream).unwrap();
            assert!(xform.wait().unwrap().success(), "dar_xform at {size}");

            for n in 1..=summary.slices {
                let a = std::fs::read(mine.join(format!("a.{n}.dar"))).unwrap();
                let b = std::fs::read(theirs.join(format!("a.{n}.dar"))).unwrap();
                assert_eq!(a.len(), b.len(), "slice {n} at {size}");
                let differ: Vec<usize> = (0..a.len()).filter(|&j| a[j] != b[j]).collect();
                assert!(
                    differ.iter().all(|&j| (4..14).contains(&j)),
                    "slice {n} at {size} differs outside the internal name: {differ:?}"
                );
            }
            assert!(
                !theirs
                    .join(format!("a.{}.dar", summary.slices + 1))
                    .exists(),
                "dar_xform cut {size} into the same number of slices"
            );
            run(Command::new("dar").arg("-t").arg(mine.join("a")).arg("-Q"));
        }
    }
}
