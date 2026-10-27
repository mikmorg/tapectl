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

/// What a refusal refuses to do: [`cut_stream`]'s, or [`Join`]'s.
const CUT: &str = "cut the archive";
const JOIN: &str = "join the slices";

fn refuse(what: impl std::fmt::Display) -> TapectlError {
    refuse_to(CUT, what)
}

fn refuse_to(verb: &str, what: impl std::fmt::Display) -> TapectlError {
    TapectlError::Dar(format!(
        "dar slice framing: {what} — refusing to {verb} rather than guess \
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
    /// What a refusal refuses to do ([`CUT`] or [`JOIN`]).
    verb: &'static str,
}

impl<R: Read> Recorder<'_, R> {
    fn refuse(&self, what: impl std::fmt::Display) -> TapectlError {
        refuse_to(self.verb, what)
    }

    fn byte(&mut self) -> Result<u8> {
        if self.bytes.len() >= MAX_HEADER {
            return Err(self.refuse(format!("a header longer than {MAX_HEADER} bytes")));
        }
        let mut b = [0u8; 1];
        let verb = self.verb;
        read_full(self.r, &mut b).map_err(|e| match e.kind() {
            io::ErrorKind::UnexpectedEof => refuse_to(verb, "the header ends early"),
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
            return Err(self.refuse(format!(
                "an integer width byte {preamble:#04x} with more than one bit set"
            )));
        }
        let groups = zeros * 8 + preamble.leading_zeros() as usize + 1;
        let width = groups * INFININT_GROUP;
        let value = self.take(width)?;
        let significant = value.iter().skip_while(|&&b| b == 0).count();
        if significant > 8 {
            return Err(refuse_to(
                self.verb,
                format!("an integer {width} bytes wide"),
            ));
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
    parse_header_to(r, CUT)
}

/// [`parse_header`], its refusals saying they refuse to `verb`.
fn parse_header_to<R: Read>(r: &mut R, verb: &'static str) -> Result<Header> {
    let mut rec = Recorder {
        r,
        bytes: Vec::with_capacity(64),
        verb,
    };
    let refuse = |what: String| refuse_to(verb, what);
    if rec.take(MAGIC.len())? != MAGIC {
        return Err(refuse(
            "the data does not start with dar's magic number".to_string(),
        ));
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
                    return Err(refuse("two slice-size fields".to_string()));
                }
                let size = rec.infinint()?;
                if rec.bytes.len() - at != len {
                    return Err(refuse(
                        "a slice-size field whose length disagrees with it".to_string(),
                    ));
                }
                slice_size = Some(size);
            }
            other => return Err(refuse(format!("an unknown header field type {other}"))),
        }
    }
    let data_name_at = data_name_at.ok_or_else(|| refuse("no data-name field".to_string()))?;
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
    // -N: no darrc, whose `all:`/`create:` section could change the slice
    // size or the header this template is cut from (#412 item 9, #465).
    cmd.arg("-N")
        .arg("-c")
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

// ── the join: slices back into the stream (issue #498) ─────────────────

/// Run the installed dar on an empty directory in `work_dir` with its
/// archive on standard output (`dar -c -`), and return the first bytes of
/// what it writes: a template of dar's standard-output header for [`Join`].
/// The mirror of [`template`] — the header is dar's, never hand-encoded.
/// `work_dir` must not be under the staging directory; the archive holds
/// nothing but an empty directory, and nothing of it is written to disk.
pub fn stream_template(dar_binary: &str, work_dir: &Path) -> Result<Vec<u8>> {
    let empty = work_dir.join("empty");
    std::fs::create_dir_all(&empty)?;
    let mut cmd = super::command(dar_binary);
    // -N: no darrc, as for the slice template (#412 item 9, #465).
    cmd.arg("-N")
        .arg("-c")
        .arg("-")
        .arg("-R")
        .arg(&empty)
        .args(["-an", "-D", "-Q"]);
    let out = super::run_interruptible(&mut cmd, || {
        "dar was stopped while writing a stream template".to_string()
    })?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        return Err(TapectlError::Dar(format!(
            "dar could not write a stream template (exit {}): {}",
            out.status,
            stderr.lines().take(5).collect::<Vec<_>>().join("\n")
        )));
    }
    let mut bytes = out.stdout;
    bytes.truncate(MAX_HEADER);
    Ok(bytes)
}

/// dar's standard-output header from [`stream_template`], parsed and
/// checked: a single-slice archive's header (flag `T`, no slice size).
#[derive(Debug, Clone)]
pub struct StreamTemplate(Header);

impl StreamTemplate {
    pub fn new(bytes: &[u8]) -> Result<Self> {
        let header = parse_header_to(&mut &bytes[..], JOIN)?;
        if header.flag != LAST || header.slice_size.is_some() {
            return Err(refuse_to(
                JOIN,
                "the stream template is not a single-slice archive's header (flag 'T', no \
                 slice size)",
            ));
        }
        Ok(Self(header))
    }

    /// The stream's header for an archive whose slice 1 begins with
    /// `slice1`: this template with the slice's internal name and data name
    /// swapped in — the inverse of [`SliceFrame::new`]. The data name is what
    /// dar checks an isolated catalogue against (`-A`).
    fn header_for(&self, slice1: &Header) -> Result<Vec<u8>> {
        if slice1.flag != b'E' || slice1.slice_size.is_none() {
            return Err(refuse_to(
                JOIN,
                format!(
                    "slice 1's header has flag {:?} and {} slice size, not a sliced archive's \
                     ('E', with one)",
                    slice1.flag as char,
                    if slice1.slice_size.is_some() {
                        "a"
                    } else {
                        "no"
                    }
                ),
            ));
        }
        let tmpl = &self.0;
        let mut header = tmpl.bytes.clone();
        header[MAGIC.len()..MAGIC.len() + LABEL_LEN]
            .copy_from_slice(&slice1.bytes[MAGIC.len()..MAGIC.len() + LABEL_LEN]);
        header[tmpl.data_name_at..tmpl.data_name_at + LABEL_LEN]
            .copy_from_slice(&slice1.bytes[slice1.data_name_at..slice1.data_name_at + LABEL_LEN]);
        Ok(header)
    }
}

/// What a [`Join`] has written: slices taken and stream bytes out.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct JoinSummary {
    pub slices: u32,
    pub stream_bytes: u64,
}

/// The inverse of [`cut_stream`]: dar slices, fed in order, written to `out`
/// as the one stream they were cut from — dar's standard-output header (a
/// [`StreamTemplate`] with slice 1's two labels), every slice's payload with
/// its own header and trailer taken off, and the stream's trailer `T`.
///
/// Push-based, so a slice can arrive in pieces from wherever it is read:
/// [`Self::begin`] a slice, [`Self::write`] its bytes (or [`Self::hole`],
/// zeros standing for bytes nobody could give), [`Self::end`] it. Nothing is
/// buffered beyond slice 1's first [`MAX_HEADER`] bytes and the one newest
/// byte of a slice, held back because it may be the trailer: memory does not
/// depend on the slice size.
///
/// Every slice is checked as [`cut_stream`] writes it, and any surprise is a
/// refusal, never a guess: slices numbered from 1 with none skipped; slice
/// 1's header dar's (flag `E`, a slice size); every later slice's header
/// slice 1's, byte for byte; every slice but the last exactly the slice size
/// long, the last no longer; the trailer `N`, or `T` on the last. Bytes
/// given as a hole are not checked — they are not the slice's — but they
/// keep their place, so the stream stays aligned: a hole in a slice's
/// header or trailer is dropped with it, one in its payload goes out as the
/// zeros it is. Only slice 1's header cannot be a hole: it is where the
/// stream's labels come from.
pub struct Join<W> {
    out: W,
    template: StreamTemplate,
    /// Slice 1's header and its slice size, once read: every slice carries
    /// that header.
    frame: Option<(Vec<u8>, u64)>,
    /// The slice being fed, from 1; 0 before the first.
    slice: u32,
    /// Whether a slice is begun and not yet ended.
    open: bool,
    /// Whether the slice being fed is the archive's last.
    last: bool,
    /// Bytes of this slice taken so far, holes included.
    at: u64,
    /// Slice 1's first bytes, until its header can be parsed, and which of
    /// them are holes.
    head: Vec<u8>,
    head_holes: Vec<bool>,
    /// The newest byte of this slice past its header, held back since it may
    /// be the trailer; and whether it is a hole.
    held: Option<(u8, bool)>,
    /// The last slice has ended and the stream's trailer is written.
    ended: bool,
    written: u64,
}

impl<W: io::Write> Join<W> {
    pub fn new(out: W, template: StreamTemplate) -> Self {
        Self {
            out,
            template,
            frame: None,
            slice: 0,
            open: false,
            last: false,
            at: 0,
            head: Vec::new(),
            head_holes: Vec::new(),
            held: None,
            ended: false,
            written: 0,
        }
    }

    fn refuse(&self, what: impl std::fmt::Display) -> TapectlError {
        refuse_to(JOIN, what)
    }

    fn emit(&mut self, bytes: &[u8]) -> Result<()> {
        self.out.write_all(bytes)?;
        self.written += bytes.len() as u64;
        Ok(())
    }

    /// Begin slice `n`; `last` says it is the archive's last.
    pub fn begin(&mut self, n: u32, last: bool) -> Result<()> {
        if self.ended {
            return Err(self.refuse(format!("slice {n} after the archive's last slice")));
        }
        if self.open {
            return Err(self.refuse(format!("slice {n} begun before slice {} ended", self.slice)));
        }
        if Some(n) != self.slice.checked_add(1) {
            return Err(self.refuse(format!(
                "slice {n} where slice {} was due",
                self.slice.saturating_add(1)
            )));
        }
        self.slice = n;
        self.open = true;
        self.last = last;
        self.at = 0;
        self.held = None;
        Ok(())
    }

    /// The next bytes of the slice.
    pub fn write(&mut self, bytes: &[u8]) -> Result<()> {
        self.take(bytes, false)
    }

    /// The next `len` bytes of the slice are a hole: bytes nobody could give
    /// whole (a chunk that did not authenticate), standing as zeros.
    pub fn hole(&mut self, len: u64) -> Result<()> {
        let zeros = [0u8; 16 * 1024];
        let mut left = len;
        while left > 0 {
            let n = left.min(zeros.len() as u64) as usize;
            self.take(&zeros[..n], true)?;
            left -= n as u64;
        }
        Ok(())
    }

    fn take(&mut self, mut bytes: &[u8], hole: bool) -> Result<()> {
        if !self.open {
            return Err(self.refuse("bytes outside any slice"));
        }
        if bytes.is_empty() {
            return Ok(());
        }
        let Some((header, _)) = &self.frame else {
            // Slice 1, its header not yet read.
            self.head.extend_from_slice(bytes);
            self.head_holes.resize(self.head.len(), hole);
            self.at += bytes.len() as u64;
            if self.head.len() >= MAX_HEADER {
                self.read_first_header()?;
            }
            return Ok(());
        };
        let header_len = header.len() as u64;
        if self.at < header_len {
            // The header every slice carries: slice 1's, byte for byte.
            let n = (header_len - self.at).min(bytes.len() as u64) as usize;
            if !hole {
                let from = self.at as usize;
                if bytes[..n] != header[from..from + n] {
                    return Err(
                        self.refuse(format!("slice {}'s header is not slice 1's", self.slice))
                    );
                }
            }
            self.at += n as u64;
            bytes = &bytes[n..];
        }
        self.at += bytes.len() as u64;
        self.payload(bytes, hole)
    }

    /// Payload bytes of the current slice, its header already taken off and
    /// counted in `at`: every one but the newest goes out, which is held back.
    fn payload(&mut self, bytes: &[u8], hole: bool) -> Result<()> {
        let Some((&newest, rest)) = bytes.split_last() else {
            return Ok(());
        };
        if let Some((b, _)) = self.held.take() {
            self.emit(&[b])?;
        }
        self.emit(rest)?;
        self.held = Some((newest, hole));
        Ok(())
    }

    /// Read slice 1's header from what has come of it, write the stream's
    /// header, and pass on what followed it.
    fn read_first_header(&mut self) -> Result<()> {
        let head = std::mem::take(&mut self.head);
        let holes = std::mem::take(&mut self.head_holes);
        // Holes are counted from where they begin: the header parsed, or
        // the bytes the parser may have read, reach into one or not.
        let first_hole = holes.iter().position(|&h| h);
        let header = match parse_header_to(&mut &head[..], JOIN) {
            Ok(h) if first_hole.is_none_or(|at| at >= h.bytes.len()) => h,
            Err(e) if first_hole.is_none_or(|at| at >= MAX_HEADER) => return Err(e),
            _ => {
                return Err(self.refuse(
                    "slice 1's header is a hole (bytes of the slice nobody could give whole), \
                     and it is where the stream's labels come from",
                ))
            }
        };
        let stream_header = self.template.header_for(&header)?;
        let slice_size = header.slice_size.unwrap_or_default();
        let len = header.bytes.len();
        self.emit(&stream_header)?;
        self.frame = Some((header.bytes, slice_size));
        // What followed the header, in runs of hole or not.
        let mut from = len;
        while from < head.len() {
            let hole = holes[from];
            let to = holes[from..]
                .iter()
                .position(|&h| h != hole)
                .map_or(head.len(), |k| from + k);
            self.payload(&head[from..to], hole)?;
            from = to;
        }
        Ok(())
    }

    /// End the slice: its newest byte is its trailer, `N`, or `T` on the
    /// last, after which the stream's own `T` is written.
    pub fn end(&mut self) -> Result<()> {
        if !self.open {
            return Err(self.refuse("a slice ended that was never begun"));
        }
        if self.frame.is_none() {
            // Slice 1, shorter than the longest header there is.
            self.read_first_header()?;
        }
        let n = self.slice;
        let (header_len, slice_size) = self
            .frame
            .as_ref()
            .map(|(h, s)| (h.len() as u64, *s))
            .unwrap_or_default();
        let Some((trailer, hole)) = self.held.take() else {
            return Err(self.refuse(format!("slice {n} ends without a trailer")));
        };
        if self.at < header_len + 1 {
            return Err(self.refuse(format!("slice {n} ends inside its header")));
        }
        if self.at > slice_size || (!self.last && self.at != slice_size) {
            return Err(self.refuse(format!(
                "slice {n} is {} bytes long, where its header's slice size is {slice_size}{}",
                self.at,
                if self.last {
                    ""
                } else {
                    " and it is not the last"
                }
            )));
        }
        let want = if self.last { LAST } else { NOT_LAST };
        if !hole && trailer != want {
            return Err(self.refuse(format!(
                "slice {n} ends with {:?}, not {:?}",
                trailer as char, want as char
            )));
        }
        self.open = false;
        if self.last {
            self.emit(&[LAST])?;
            self.out.flush()?;
            self.ended = true;
        }
        Ok(())
    }

    /// Whether the last slice has ended, and the stream with it.
    pub fn ended(&self) -> bool {
        self.ended
    }

    pub fn summary(&self) -> JoinSummary {
        JoinSummary {
            slices: self.slice,
            stream_bytes: self.written,
        }
    }

    pub fn into_inner(self) -> W {
        self.out
    }
}

/// Join `slices`, in order from slice 1, into `out` ([`Join`]): the stream
/// [`cut_stream`] cut them from, with `stream_template`'s header
/// ([`stream_template`]). The last item is the archive's last slice.
pub fn join_slices<R, I, W>(slices: I, stream_template: &[u8], out: W) -> Result<JoinSummary>
where
    R: Read,
    I: IntoIterator<Item = R>,
    W: io::Write,
{
    let mut join = Join::new(out, StreamTemplate::new(stream_template)?);
    let mut slices = slices.into_iter().peekable();
    let mut buf = vec![0u8; 64 * 1024];
    let mut n: u32 = 0;
    while let Some(mut slice) = slices.next() {
        n = n
            .checked_add(1)
            .ok_or_else(|| refuse_to(JOIN, "more than 2^32 slices"))?;
        join.begin(n, slices.peek().is_none())?;
        loop {
            let got = match slice.read(&mut buf) {
                Ok(0) => break,
                Ok(got) => got,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(e.into()),
            };
            join.write(&buf[..got])?;
        }
        join.end()?;
    }
    if !join.ended() {
        return Err(refuse_to(JOIN, "no slices"));
    }
    Ok(join.summary())
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

    /// `n` bytes of deterministic noise (dar does not compress it away).
    fn noise(n: usize, seed: u64) -> Vec<u8> {
        let mut x: u64 = seed;
        (0..n)
            .map(|_| {
                x ^= x << 13;
                x ^= x >> 7;
                x ^= x << 17;
                x as u8
            })
            .collect()
    }

    /// dar's `-c -` stream of `src`: what `stage create` cuts.
    fn dar_stream_of(src: &Path) -> Vec<u8> {
        run(Command::new("dar").args(["-c", "-", "-R"]).arg(src).args([
            "-an",
            "-D",
            "-Q",
            "-N",
            "--retry-on-change",
            "0",
        ]))
        .stdout
    }

    /// Cut `stream` at `-s size` with tapectl's slicer and with `dar_xform`
    /// into `work`, and assert they agree: the same slices, byte for byte
    /// except the internal name `dar_xform` draws afresh, and no further
    /// slice from `dar_xform`; then `dar -t` accepts tapectl's. Returns the
    /// slice header's length and what the cut did.
    fn cut_like_dar_xform(stream: &[u8], size: &str, work: &Path) -> (usize, CutSummary) {
        let mine = work.join("mine");
        let theirs = work.join("theirs");
        std::fs::create_dir_all(&mine).unwrap();
        std::fs::create_dir_all(&theirs).unwrap();
        let tmpl = template("dar", size, &work.join("tmpl")).unwrap();
        let header_len = parse_header(&mut &tmpl[..]).unwrap().bytes.len();
        let summary = cut_stream(
            stream,
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
        xform.stdin.take().unwrap().write_all(stream).unwrap();
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
        (header_len, summary)
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
        std::fs::write(
            src.join("noise.bin"),
            noise((5 << 20) + 12345, 0x2545_F491_4F6C_DD1D),
        )
        .unwrap();
        std::fs::write(src.join("d/small.txt"), b"small").unwrap();
        let stream = dar_stream_of(&src);

        // The slice size is an infinint in the header, so the header's length
        // follows it: one 4-byte group up to 4 GiB, two above. 1G is the
        // default since ADR-0012's 2026-10-06 amendment, 10G before it.
        for (i, (size, header_len)) in [("1M", 50), ("4M", 50), ("1G", 50), ("10G", 54)]
            .into_iter()
            .enumerate()
        {
            let (found, _) = cut_like_dar_xform(&stream, size, &tmp.path().join(format!("w{i}")));
            assert_eq!(found, header_len, "dar's slice header at -s {size}");
        }
    }

    /// Issue #465 item 8: a payload that ends exactly where a slice does is
    /// asserted by `a_payload_ending_on_a_slice_boundary_marks_the_full_slice_last`
    /// on a hand-made stream; this is the same case from a real `dar -c -`
    /// stream, against `dar_xform` and `dar -t`. dar's last-slice flag sits in
    /// the full slice's trailer, and there is no further, empty slice.
    ///
    /// The slice size is chosen to fit the stream: the source is padded a
    /// byte at a time until the payload (the stream less dar's one header and
    /// its one trailing flag byte) divides by 3, and `-s` is then three
    /// payloads plus the slice header and trailer. A bare number is bytes.
    #[test]
    fn a_real_archive_ending_on_a_slice_boundary_is_cut_like_dar_xform() {
        if Command::new("dar_xform").arg("--version").output().is_err() {
            eprintln!("SKIP boundary test: dar_xform is not on PATH (it ships with dar)");
            return;
        }
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("src");
        std::fs::create_dir_all(&src).unwrap();
        let body = noise(300_000, 0x9E37_79B9_7F4A_7C15);
        let (stream, payload) = (0..64)
            .find_map(|pad| {
                std::fs::write(src.join("data.bin"), &body[..body.len() - 64 + pad]).unwrap();
                let stream = dar_stream_of(&src);
                let header = parse_header(&mut &stream[..]).unwrap().bytes.len();
                let payload = stream.len() - header - 1;
                payload.is_multiple_of(3).then_some((stream, payload))
            })
            .expect("a payload that divides by three within 64 bytes of padding");
        let per_slice = payload / 3;
        // The slice header is 50 bytes below 4 GiB (asserted below), and each
        // slice ends in one flag byte.
        let size = (per_slice + 50 + 1).to_string();

        let (header_len, summary) = cut_like_dar_xform(&stream, &size, tmp.path());
        assert_eq!(header_len, 50, "the arithmetic above assumes this");
        assert_eq!(summary.slices, 3, "three full slices and no empty fourth");
        let last = std::fs::read(tmp.path().join("mine/a.3.dar")).unwrap();
        assert_eq!(last.len(), per_slice + 51, "the last slice is full");
        assert_eq!(*last.last().unwrap(), LAST, "and marked last");
        let second = std::fs::read(tmp.path().join("mine/a.2.dar")).unwrap();
        assert_eq!(*second.last().unwrap(), NOT_LAST);
    }

    // ── the join (#498) ──────────────────────────────────────────────────

    /// A stream template by hand: a single-slice archive's header whose two
    /// labels are unlike any stream's, so a join that kept them would show.
    fn stream_template_by_hand() -> Vec<u8> {
        let mut t = header(b'T', None);
        t[4..14].copy_from_slice(b"TEMPLATE!!");
        let n = t.len();
        t[n - 10..].copy_from_slice(b"TMPLDATA!!");
        t
    }

    fn join_in_memory(slices: &[Vec<u8>]) -> Result<Vec<u8>> {
        let mut out = Vec::new();
        join_slices(
            slices.iter().map(|s| &s[..]),
            &stream_template_by_hand(),
            &mut out,
        )?;
        Ok(out)
    }

    /// A hand-made stream of `payload` bytes, cut into slices of 10 payload
    /// bytes each.
    fn hand_cut(payload: &[u8]) -> (Vec<u8>, Vec<Vec<u8>>, usize) {
        let mut stream = header(b'T', None);
        stream.extend_from_slice(payload);
        stream.push(b'T');
        let hlen = header(b'E', Some(&[0x80, 0, 0, 0, 0])).len();
        let slices = cut_in_memory(&stream, (hlen + 11) as u8);
        (stream, slices, hlen)
    }

    #[test]
    fn joins_hand_made_slices_back_into_the_stream() {
        let payload: Vec<u8> = (0..25u8).collect();
        let (stream, slices, _) = hand_cut(&payload);
        assert_eq!(slices.len(), 3);
        assert_eq!(join_in_memory(&slices).unwrap(), stream);

        // A payload that ends on a slice boundary: the full slice is the last.
        let (stream, slices, _) = hand_cut(&[7u8; 20]);
        assert_eq!(slices.len(), 2);
        assert_eq!(join_in_memory(&slices).unwrap(), stream);
    }

    /// The stream's labels come from slice 1, not the template: its header
    /// is the template's but for them.
    #[test]
    fn the_stream_header_is_the_template_with_slice_1s_labels() {
        let (stream, slices, _) = hand_cut(&[1, 2, 3]);
        let joined = join_in_memory(&slices).unwrap();
        let tmpl = stream_template_by_hand();
        assert_eq!(&joined[4..14], b"0123456789");
        assert_eq!(&joined[tmpl.len() - 10..tmpl.len()], b"DATANAME!!");
        assert_eq!(&joined[..4], &tmpl[..4]);
        assert_eq!(&joined[14..tmpl.len() - 10], &tmpl[14..tmpl.len() - 10]);
        assert_eq!(joined, stream);
    }

    #[test]
    fn a_join_refuses_what_it_does_not_recognise() {
        let payload: Vec<u8> = (0..25u8).collect();
        let (stream, slices, hlen) = hand_cut(&payload);
        let refused = |slices: &[Vec<u8>], want: &str| {
            let err = join_in_memory(slices).unwrap_err().to_string();
            assert!(err.contains("refusing to join"), "{err}");
            assert!(err.contains(want), "{want}: {err}");
        };

        let mut other_header = slices.clone();
        other_header[1][5] ^= 1;
        refused(&other_header, "slice 2's header is not slice 1's");

        let mut short = slices.clone();
        short[1].remove(hlen + 3);
        refused(&short, "slice 2 is 60 bytes long");

        let mut long_last = slices.clone();
        long_last[2].splice(hlen..hlen, [9u8; 20]);
        refused(&long_last, "slice 3 is");

        let mut trailer = slices.clone();
        *trailer[2].last_mut().unwrap() = NOT_LAST;
        refused(&trailer, "slice 3 ends with 'N', not 'T'");

        let mut early_end = slices.clone();
        *early_end[1].last_mut().unwrap() = LAST;
        refused(&early_end, "slice 2 ends with 'T', not 'N'");

        refused(&slices[..2], "slice 2 ends with 'N', not 'T'");
        // The stream itself where slice 1 should be. (Slices 2 and 3 given as
        // 1 and 2 cannot be told by their bytes: every slice carries the same
        // header. Numbering them is the caller's.)
        refused(
            &[stream.clone()],
            "slice 1's header has flag 'T' and no slice size",
        );
        refused(&[], "no slices");

        let mut bad_template = stream_template_by_hand();
        bad_template[14] = b'E';
        let err = join_slices(slices.iter().map(|s| &s[..]), &bad_template, Vec::new())
            .unwrap_err()
            .to_string();
        assert!(err.contains("not a single-slice archive's header"), "{err}");

        // Out of order, and a slice begun inside another.
        let template = StreamTemplate::new(&stream_template_by_hand()).unwrap();
        let mut join = Join::new(Vec::new(), template.clone());
        join.begin(1, false).unwrap();
        join.write(&slices[0]).unwrap();
        join.end().unwrap();
        let err = join.begin(3, true).unwrap_err().to_string();
        assert!(err.contains("slice 3 where slice 2 was due"), "{err}");
        let mut join = Join::new(Vec::new(), template);
        join.begin(1, false).unwrap();
        let err = join.begin(2, false).unwrap_err().to_string();
        assert!(err.contains("before slice 1 ended"), "{err}");
    }

    /// Holes are not the slice's bytes, so they are not checked; they keep
    /// their place, so the stream stays aligned: a slice of zeros from end to
    /// end comes out as its payload's length of zeros, header and trailer
    /// gone. Only slice 1's header cannot be a hole.
    #[test]
    fn holes_keep_their_place_and_only_slice_1s_header_cannot_be_one() {
        let payload: Vec<u8> = (1..=25u8).collect();
        let (stream, slices, _) = hand_cut(&payload);
        let template = StreamTemplate::new(&stream_template_by_hand()).unwrap();
        let mut join = Join::new(Vec::new(), template.clone());
        for (i, slice) in slices.iter().enumerate() {
            join.begin(i as u32 + 1, i + 1 == slices.len()).unwrap();
            if i == 1 {
                join.hole(slice.len() as u64).unwrap();
            } else {
                // In pieces, as a restore delivers them.
                for piece in slice.chunks(7) {
                    join.write(piece).unwrap();
                }
            }
            join.end().unwrap();
        }
        // Slice 2's payload, in the stream: after the stream's own header
        // and slice 1's ten payload bytes.
        let mut want = stream.clone();
        let at = header(b'T', None).len() + 10;
        want[at..at + 10].fill(0);
        assert_eq!(join.into_inner(), want);

        let mut join = Join::new(Vec::new(), template);
        join.begin(1, false).unwrap();
        join.hole(slices[0].len() as u64).unwrap();
        let err = join.end().unwrap_err().to_string();
        assert!(err.contains("slice 1's header is a hole"), "{err}");
    }

    /// The first byte at which `a` and `b` differ, for a readable failure.
    fn assert_same_bytes(a: &[u8], b: &[u8], what: &str) {
        if a != b {
            let at = a.iter().zip(b).position(|(x, y)| x != y);
            panic!(
                "{what}: {} bytes against {}, first difference at {at:?}",
                a.len(),
                b.len()
            );
        }
    }

    /// A tree for the real-dar join tests: noise across several slices, small
    /// files, and a subdirectory.
    fn join_tree(src: &Path) -> Vec<(String, Vec<u8>)> {
        let files = vec![
            (
                "noise.bin".to_string(),
                noise(2_500_000, 0x5DEE_CE66_D1CE_4E5B),
            ),
            ("small.txt".to_string(), b"small".to_vec()),
            (
                "d/deep.bin".to_string(),
                noise(70_000, 0x2545_F491_4F6C_DD1D),
            ),
            ("d/empty".to_string(), Vec::new()),
        ];
        for (path, bytes) in &files {
            let at = src.join(path);
            std::fs::create_dir_all(at.parent().unwrap()).unwrap();
            std::fs::write(at, bytes).unwrap();
        }
        files
    }

    /// `dar -c -` of `src` by `dar`, with tapectl's own create flags
    /// (`create::create_command`, as `stage create` runs it), and the
    /// uncompressed isolated catalogue staging keeps for it.
    fn tapectl_stream_of(dar: &str, src: &Path, work: &Path) -> (Vec<u8>, std::path::PathBuf) {
        use super::super::create;
        let onfly = work.join("onfly");
        let params = create::DarCreateParams {
            dar_binary: dar,
            source_path: src,
            compression: "none",
            exclude_patterns: &[],
            exclude_paths: &[],
            preserve_xattrs: true,
            preserve_fsa: true,
            on_fly_catalogue: &onfly,
        };
        let mut stream = create::spawn_archive(&params).unwrap();
        let mut bytes = Vec::new();
        stream.take_stdout().read_to_end(&mut bytes).unwrap();
        stream.finish().unwrap();
        let catalogue = work.join("catalogue");
        create::reisolate_catalogue(dar, &onfly, &catalogue).unwrap();
        (bytes, catalogue)
    }

    /// Cut `stream` at `-s size` (the slice template from `dar`) into memory.
    fn cut_with(dar: &str, stream: &[u8], size: &str, work: &Path) -> (Vec<Vec<u8>>, usize) {
        let tmpl = template(dar, size, work).unwrap();
        let header_len = parse_header(&mut &tmpl[..]).unwrap().bytes.len();
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
        (slices, header_len)
    }

    /// `dar -x - --sequential-read [-A catalogue]` by `dar`, fed `stream` on
    /// its standard input: its exit status, stdout and stderr. A write dar
    /// closed its input under is not an error: given the catalogue dar stops
    /// where the file data ends (#497).
    fn extract_from_stdin(
        dar: &str,
        stream: &[u8],
        catalogue: Option<&Path>,
        dest: &Path,
    ) -> std::process::Output {
        use std::io::Write;
        let mut cmd = Command::new(dar);
        cmd.args(["-x", "-", "--sequential-read"]);
        if let Some(catalogue) = catalogue {
            cmd.arg("-A").arg(catalogue);
        }
        let mut child = cmd
            .arg("-R")
            .arg(dest)
            .args(["-O", "-Q", "-N"])
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        let mut stdin = child.stdin.take().unwrap();
        let fed = std::thread::scope(|s| {
            let feed = s.spawn(move || stdin.write_all(stream));
            let out = child.wait_with_output().unwrap();
            (feed.join().unwrap(), out)
        });
        match fed.0 {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::BrokenPipe => {}
            Err(e) => panic!("feeding dar: {e}"),
        }
        fed.1
    }

    /// The join's proof, from `create` dar's stream to `read` dar's restore:
    /// tapectl's slices of the stream, joined with `read`'s stream template,
    /// are the stream byte for byte, at every slice size given (1000 bytes is
    /// smaller than the longest header there is; 10G is the 54-byte header);
    /// and `read`, fed the joined stream on its standard input, restores the
    /// tree identically and exits 0, with the catalogue and without.
    fn join_round_trip(create: &str, read: &str) {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("src");
        let files = join_tree(&src);
        let (stream, catalogue) = tapectl_stream_of(create, &src, tmp.path());
        let stream_tmpl = stream_template(read, &tmp.path().join("stream-template")).unwrap();
        let mut joined_once = None;
        for (i, (size, header_len)) in [("1000", 50), ("65536", 50), ("1M", 50), ("10G", 54)]
            .into_iter()
            .enumerate()
        {
            let (slices, found) =
                cut_with(create, &stream, size, &tmp.path().join(format!("t{i}")));
            assert_eq!(found, header_len, "dar's slice header at -s {size}");
            let mut joined = Vec::new();
            let summary =
                join_slices(slices.iter().map(|s| &s[..]), &stream_tmpl, &mut joined).unwrap();
            assert_eq!(summary.slices as usize, slices.len());
            assert_eq!(summary.stream_bytes, stream.len() as u64);
            assert_same_bytes(
                &joined,
                &stream,
                &format!(
                    "{create} -> {read}: the join of {} slices at -s {size}",
                    slices.len()
                ),
            );
            if size == "65536" {
                assert!(slices.len() > 30, "several slices: {}", slices.len());
                joined_once = Some(joined);
            }
        }
        let joined = joined_once.unwrap();
        for with_catalogue in [true, false] {
            let dest = tmp.path().join(format!("dest-{with_catalogue}"));
            std::fs::create_dir_all(&dest).unwrap();
            let out =
                extract_from_stdin(read, &joined, with_catalogue.then_some(&*catalogue), &dest);
            assert!(
                out.status.success(),
                "{read} -x - (catalogue: {with_catalogue}) exited {}: {}{}",
                out.status,
                String::from_utf8_lossy(&out.stdout),
                String::from_utf8_lossy(&out.stderr)
            );
            for (path, bytes) in &files {
                assert_same_bytes(
                    &std::fs::read(dest.join(path)).unwrap(),
                    bytes,
                    &format!("{path}, restored by {read} (catalogue: {with_catalogue})"),
                );
            }
        }
    }

    /// THE proof issue #498's fix rests on: the slices tapectl cuts from
    /// `dar -c -` join back into that stream byte for byte, and dar restores
    /// it from its standard input — the single-stream reader, which does not
    /// have the named-pipe reader's end-of-slice defect.
    ///
    /// `TAPECTL_TEST_OTHER_DAR`, when set to another dar binary, runs the
    /// round trip across the two as well, each way (home2's dar is not this
    /// machine's).
    #[test]
    fn joined_slices_are_the_stream_byte_for_byte_and_dar_restores_it_from_stdin() {
        join_round_trip("dar", "dar");
        if let Ok(other) = std::env::var("TAPECTL_TEST_OTHER_DAR") {
            join_round_trip(&other, "dar");
            join_round_trip("dar", &other);
            join_round_trip(&other, &other);
        }
    }

    /// The template is dar's own: the installed dar's standard-output header
    /// parses as a single-slice archive's, and two runs differ only in the
    /// two labels.
    #[test]
    fn the_stream_template_is_dars_single_slice_header() {
        let tmp = tempfile::tempdir().unwrap();
        let a = stream_template("dar", &tmp.path().join("a")).unwrap();
        let b = stream_template("dar", &tmp.path().join("b")).unwrap();
        let ha = StreamTemplate::new(&a).unwrap().0;
        let hb = StreamTemplate::new(&b).unwrap().0;
        assert_eq!(ha.bytes.len(), hb.bytes.len());
        let differ: Vec<usize> = (0..ha.bytes.len())
            .filter(|&j| ha.bytes[j] != hb.bytes[j])
            .collect();
        assert!(
            differ
                .iter()
                .all(|&j| (4..14).contains(&j)
                    || (ha.data_name_at..ha.data_name_at + 10).contains(&j)),
            "two templates differ outside their labels: {differ:?}"
        );
    }
}
