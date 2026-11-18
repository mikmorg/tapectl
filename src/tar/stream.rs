//! The stream reader: tar's pax stream read as it passes, one member at a
//! time (`volume-format-v3.md` §3.2 and §3.3; ADR-0012 items 253, 254, 256,
//! 270, 272, 274, 277, 283; the plan's D3 and D12; invariants 5, 7, 13).
//!
//! [`read`] is given the stream's bytes in order and changes none of them.
//! It reads the 512-byte blocks itself (the `tar` crate's iterator gives a
//! member's own header, and a member starts at its first extension header)
//! and gives a [`Sink`]:
//!
//! - for each member, **before any of its bytes**, its `start` and its whole
//!   length (its extension headers, its header and its padded data: what
//!   the cutter places, D3); then its bytes, every one, in order; then its
//!   [`Row`], with its kind, offsets, size, sha256, path and, for a hard
//!   link, its target;
//! - for the stream's tail (tar's end-of-archive blocks and the padding to
//!   its record size), its `start` and length, then its bytes.
//!
//! Memory: one member's header blocks and extension records are held until
//! the member's own header is read; a member's data is passed through a
//! fixed buffer of [`CHUNK`] bytes and never held. The tail is all zeros,
//! so it is counted, not held. A 96 GiB member was measured (#555).
//!
//! Each user and group id the stream carries, as a member's owner or group
//! and in an ACL entry, is given to an [`idmap::Collector`] (item 277).
//!
//! Hard links (item 256): the list holds every name; the stream holds the
//! first name with its data and each later name as a link member with
//! none. The link's row is kind `h` with its target's size and sha256
//! (§1.6). To find them without holding a row for every file, the reader
//! remembers only the files the list marks [`Listed::linked`].

use std::collections::HashMap;
use std::fmt;
use std::io::{self, BufRead, BufReader, Read};

use sha2::Digest;

use crate::error::TapectlError;
use crate::idmap::{self, IdKind, IdRow};
use crate::pass_count::Sha256;
use crate::volume::filetable::{Kind, Row};

use super::pax;

/// A tar block.
pub const BLOCK: usize = 512;

/// The buffer a member's data passes through: each [`Sink::bytes`] call
/// of a member's data, or of the tail, is at most this long. A member's
/// header blocks and extension records go to the sink in one call of
/// their own, as long as they are (at most [`MAX_EXTENDED`] and the
/// header blocks around it).
pub const CHUNK: usize = 64 * 1024;

/// The most an extended header's records may hold. GNU tar writes a few
/// hundred bytes (a long name, an ACL, extended attributes); this bounds
/// what one member's records cost, whatever the stream says.
pub const MAX_EXTENDED: u64 = 64 << 20;

/// What the ACL of a listed entry was on disk before tar ran: whether its
/// `system.posix_acl_access` and `system.posix_acl_default` attributes were
/// present (§3.2). Read without following a symbolic link (`lgetxattr`),
/// as tar archives the link itself: a link to a file with an ACL has none.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct AclOnDisk {
    pub access: bool,
    pub default: bool,
}

/// One entry of the snapshot's list, in the list's order (the order tar
/// was given, §3.1).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Listed {
    /// Relative to the unit's root, with no leading `./`; the root itself
    /// is `.` (§1.6).
    pub path: String,
    /// What the snapshot saw. Every name of a hard-linked file is listed as
    /// [`Kind::File`]: the stream decides which is the link.
    pub kind: Kind,
    /// The file's bytes. Compared for a regular file (and a hard link,
    /// whose target has the same bytes); other kinds have no data in the
    /// stream.
    pub size: u64,
    /// The ACL on disk.
    pub acl: AclOnDisk,
    /// The file had more than one name (a link count over 1): its size and
    /// sha256 are remembered, for a later name that the stream holds as a
    /// link to it. A link to a file not so marked is refused.
    pub linked: bool,
}

/// The rules the stream is held to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rules {
    /// `preserve_acls` (D11): a listed entry that had an ACL on disk must
    /// carry the ACL's text record.
    pub acls: bool,
}

/// Each rule a stream is refused by (D12; §3.2).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Rule {
    /// A member is not the next entry of the list, by path, kind or size.
    NotNextEntry,
    /// A header's user or group name field is not empty.
    NameInHeader,
    /// A `uname` or `gname` pax record.
    NameRecord,
    /// An ACL entry's qualifier is not a number.
    AclQualifier,
    /// A listed entry that had an ACL on disk carries no ACL record.
    AclLost,
    /// A global extended header.
    GlobalHeader,
    /// A GNU long-name (or long-link) member.
    GnuLongName,
    /// A header whose checksum is wrong.
    Checksum,
    /// A type the reader does not know.
    UnknownType,
    /// Something other than zeros after the end-of-archive blocks.
    AfterEnd,
    /// The stream ends inside a member, or before its end-of-archive
    /// blocks.
    CutShort,
    /// A listed entry never seen.
    NeverSeen,
    /// A hard link whose target is not a file listed (and marked
    /// [`Listed::linked`]) earlier.
    LinkTarget,
    /// A header or extended header that cannot be read as GNU tar's pax
    /// format writes it.
    Malformed,
}

impl Rule {
    /// The rule in words.
    pub fn describe(self) -> &'static str {
        match self {
            Rule::NotNextEntry => "a member is not the next entry of the list",
            Rule::NameInHeader => "a header names a user or group",
            Rule::NameRecord => "an extended header names a user or group",
            Rule::AclQualifier => "an ACL entry's qualifier is not a number",
            Rule::AclLost => "a file's ACL is missing from the stream",
            Rule::GlobalHeader => "a global extended header",
            Rule::GnuLongName => "a GNU long-name member",
            Rule::Checksum => "a header's checksum is wrong",
            Rule::UnknownType => "a member of a type the reader does not know",
            Rule::AfterEnd => "something other than zeros after the end of the archive",
            Rule::CutShort => "the stream is cut short",
            Rule::NeverSeen => "a listed entry is not in the stream",
            Rule::LinkTarget => "a hard link's target is not a file listed earlier",
            Rule::Malformed => "a header that cannot be read",
        }
    }
}

impl fmt::Display for Rule {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.describe())
    }
}

/// A refusal of the stream: the rule, the path it concerns when one is
/// known, the offset in the stream where it was found, and what was seen.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Refusal {
    pub rule: Rule,
    pub path: Option<String>,
    pub offset: u64,
    pub detail: String,
}

impl fmt::Display for Refusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "the tar stream is refused at byte {}", self.offset)?;
        if let Some(path) = &self.path {
            write!(f, ", {path:?}")?;
        }
        write!(f, ": {}: {}", self.rule, self.detail)
    }
}

/// Why [`read`] stopped.
#[derive(Debug)]
pub enum StreamError {
    /// The stream broke a rule.
    Refused(Refusal),
    /// Reading the stream failed.
    Io(io::Error),
    /// The sink failed.
    Sink(TapectlError),
}

impl fmt::Display for StreamError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            StreamError::Refused(r) => r.fmt(f),
            StreamError::Io(e) => write!(f, "reading the tar stream failed: {e}"),
            StreamError::Sink(e) => e.fmt(f),
        }
    }
}

impl std::error::Error for StreamError {}

impl From<io::Error> for StreamError {
    fn from(e: io::Error) -> Self {
        StreamError::Io(e)
    }
}

impl From<StreamError> for TapectlError {
    fn from(e: StreamError) -> Self {
        match e {
            StreamError::Refused(r) => TapectlError::Tar(r.to_string()),
            StreamError::Io(e) => TapectlError::Io(e),
            StreamError::Sink(e) => e,
        }
    }
}

impl StreamError {
    /// The refusal, if the stream was refused.
    pub fn refusal(&self) -> Option<&Refusal> {
        match self {
            StreamError::Refused(r) => Some(r),
            _ => None,
        }
    }
}

/// Where the stream goes as it is read: the cutter (D3), or anything else
/// that wants the stream and its rows.
pub trait Sink {
    /// A member begins at `start` and is `len` bytes long (its extension
    /// headers, its header and its padded data), given before any of its
    /// bytes. Returns the number of the slice it begins in (§3.3), which
    /// its [`Row`] states.
    fn member(&mut self, start: u64, len: u64) -> crate::error::Result<u64>;

    /// The stream's next bytes, in order: every byte of the stream comes
    /// through here exactly once, a member's after its
    /// [`member`](Sink::member) and the tail's after its
    /// [`tail`](Sink::tail).
    fn bytes(&mut self, bytes: &[u8]) -> crate::error::Result<()>;

    /// The member whose bytes just passed, checked.
    fn row(&mut self, row: Row) -> crate::error::Result<()>;

    /// The stream's tail (the end-of-archive blocks and the padding after
    /// them) begins at `start` and is `len` bytes long. Given once, after
    /// the last member's row and before the tail's bytes, and only when
    /// every check has passed.
    fn tail(&mut self, start: u64, len: u64) -> crate::error::Result<()>;
}

/// What a stream that passed every check was.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Finished {
    /// The stream's length: the file table's `stream_bytes`.
    pub stream_bytes: u64,
    /// Where the tail begins: the last row's `end`.
    pub tail_start: u64,
    /// How many members (rows) it held.
    pub members: u64,
    /// The id map (item 277): every id the stream carries, named.
    pub ids: Vec<IdRow>,
}

/// Read the stream from `input` to its end, checking each member against
/// `list` (in order) and `rules`, giving `sink` each member's announcement,
/// bytes and row, and `ids` each id seen.
pub fn read<R, I, L, S>(
    input: R,
    list: I,
    rules: Rules,
    ids: idmap::Collector<L>,
    sink: &mut S,
) -> Result<Finished, StreamError>
where
    R: Read,
    I: IntoIterator<Item = Listed>,
    L: FnMut(IdKind, u32) -> Option<String>,
    S: Sink + ?Sized,
{
    let mut reader = Reader {
        input: BufReader::with_capacity(CHUNK, input),
        offset: 0,
        list: list.into_iter(),
        rules,
        ids: Some(ids),
        linked: HashMap::new(),
        held: Vec::new(),
        members: 0,
    };
    reader.run(sink)
}

/// A member's extended records, as far as the reader reads them.
#[derive(Debug, Default)]
struct Extended {
    path: Option<Vec<u8>>,
    linkpath: Option<Vec<u8>>,
    size: Option<u64>,
    uid: Option<u64>,
    gid: Option<u64>,
    acl_access: Option<Vec<u8>>,
    acl_default: Option<Vec<u8>>,
    /// The first `uname` or `gname` record's key and value.
    name: Option<(Vec<u8>, Vec<u8>)>,
}

struct Reader<R, I, L> {
    input: BufReader<R>,
    /// Bytes read so far: the offset of the next.
    offset: u64,
    list: I,
    rules: Rules,
    /// Taken when the stream ends.
    ids: Option<idmap::Collector<L>>,
    /// The files marked `linked`: their size and sha256.
    linked: HashMap<String, (u64, [u8; 32])>,
    /// The current member's header blocks, held until its own header.
    held: Vec<u8>,
    members: u64,
}

/// `n` rounded up to whole blocks.
fn padded(n: u64) -> u64 {
    n.div_ceil(BLOCK as u64) * BLOCK as u64
}

/// A header's numeric field: octal digits (leading spaces or NULs, ended
/// by a space or NUL), or GNU's base-256 form when the first byte's high
/// bit is set.
fn numeric(field: &[u8]) -> Option<u64> {
    if field.first().is_some_and(|b| b & 0x80 != 0) {
        if field[0] & 0x40 != 0 {
            return None; // negative
        }
        let mut v: u64 = (field[0] & 0x3f) as u64;
        for b in &field[1..] {
            v = v.checked_mul(256)?.checked_add(*b as u64)?;
        }
        return Some(v);
    }
    let start = field.iter().position(|b| *b != b' ' && *b != 0)?;
    let rest = &field[start..];
    let end = rest
        .iter()
        .position(|b| *b == b' ' || *b == 0)
        .unwrap_or(rest.len());
    let digits = &rest[..end];
    if digits.is_empty() || !digits.iter().all(|b| (b'0'..=b'7').contains(b)) {
        return None;
    }
    if rest[end..].iter().any(|b| *b != b' ' && *b != 0) {
        return None;
    }
    u64::from_str_radix(std::str::from_utf8(digits).ok()?, 8).ok()
}

/// A NUL-ended field's bytes.
fn text(field: &[u8]) -> &[u8] {
    let end = field.iter().position(|b| *b == 0).unwrap_or(field.len());
    &field[..end]
}

/// The ustar header's fields the reader reads (POSIX.1-2001 `ustar`).
struct Header<'a>(&'a [u8; BLOCK]);

impl Header<'_> {
    fn name(&self) -> &[u8] {
        text(&self.0[0..100])
    }
    fn uid(&self) -> Option<u64> {
        numeric(&self.0[108..116])
    }
    fn gid(&self) -> Option<u64> {
        numeric(&self.0[116..124])
    }
    fn size(&self) -> Option<u64> {
        numeric(&self.0[124..136])
    }
    fn typeflag(&self) -> u8 {
        self.0[156]
    }
    fn linkname(&self) -> &[u8] {
        text(&self.0[157..257])
    }
    fn magic(&self) -> &[u8] {
        &self.0[257..263]
    }
    fn uname(&self) -> &[u8] {
        text(&self.0[265..297])
    }
    fn gname(&self) -> &[u8] {
        text(&self.0[297..329])
    }
    fn prefix(&self) -> &[u8] {
        text(&self.0[345..500])
    }
    /// The name, with the prefix field joined when there is one.
    fn full_name(&self) -> Vec<u8> {
        let prefix = self.prefix();
        if prefix.is_empty() {
            self.name().to_vec()
        } else {
            let mut n = prefix.to_vec();
            n.push(b'/');
            n.extend_from_slice(self.name());
            n
        }
    }
    /// The checksum field, and the unsigned sum of the block with that
    /// field taken as spaces.
    fn checksums(&self) -> (Option<u64>, u64) {
        let stated = numeric(&self.0[148..156]);
        let sum: u64 = self
            .0
            .iter()
            .enumerate()
            .map(|(i, b)| if (148..156).contains(&i) { b' ' } else { *b } as u64)
            .sum();
        (stated, sum)
    }
}

/// How a block read ended.
enum Got {
    Block,
    Eof,
    Partial,
}

fn lossy(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

/// A member's name as the file table writes a path (§1.6): one leading
/// `./` taken off, and the trailing `/` GNU tar gives a folder; the root
/// (`././`) is `.`.
fn shown_path(name: &[u8]) -> String {
    let n = name.strip_prefix(b"./").unwrap_or(name);
    let n = match n.strip_suffix(b"/") {
        Some(t) if !t.is_empty() => t,
        _ => n,
    };
    lossy(n)
}

/// Whether a member named `name` (as the stream has it) of `kind` is the
/// listed `path`: tar was given `./<path>`, and names a folder's member
/// with a trailing `/` (`./d/`, the root `././`).
fn names_entry(name: &[u8], path: &str, kind: Kind) -> bool {
    let mut want = b"./".to_vec();
    want.extend_from_slice(path.as_bytes());
    if name == want.as_slice() {
        return true;
    }
    kind == Kind::Folder
        && name.len() == want.len() + 1
        && name.starts_with(&want)
        && name.ends_with(b"/")
}

/// Whether a member of `kind` may stand for a listed entry of `listed`:
/// the same kind, or a hard link for a listed file (the list holds every
/// name of a linked file as a file; the stream holds the later names as
/// links, item 256).
fn kind_fits(kind: Kind, listed: Kind) -> bool {
    kind == listed || (kind == Kind::HardLink && listed == Kind::File)
}

impl<R, I, L> Reader<R, I, L>
where
    R: Read,
    I: Iterator<Item = Listed>,
    L: FnMut(IdKind, u32) -> Option<String>,
{
    fn refuse(&self, rule: Rule, path: Option<String>, detail: impl Into<String>) -> StreamError {
        StreamError::Refused(Refusal {
            rule,
            path,
            offset: self.offset,
            detail: detail.into(),
        })
    }

    /// Read into `buf` until it is full or the stream ends.
    fn fill(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let mut filled = 0;
        while filled < buf.len() {
            match self.input.read(&mut buf[filled..]) {
                Ok(0) => break,
                Ok(n) => filled += n,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) => return Err(e),
            }
        }
        self.offset += filled as u64;
        Ok(filled)
    }

    fn block(&mut self, buf: &mut [u8; BLOCK]) -> io::Result<Got> {
        Ok(match self.fill(buf)? {
            0 => Got::Eof,
            BLOCK => Got::Block,
            _ => Got::Partial,
        })
    }

    fn run<S: Sink + ?Sized>(&mut self, sink: &mut S) -> Result<Finished, StreamError> {
        loop {
            let start = self.offset;
            self.held.clear();
            let mut ext = Extended::default();
            let mut block = [0u8; BLOCK];
            // The member's extension headers, then its own header.
            loop {
                match self.block(&mut block)? {
                    Got::Block => {}
                    Got::Eof => {
                        return Err(self.refuse(
                            Rule::CutShort,
                            ext.path.as_deref().map(shown_path),
                            "the stream ends without its end-of-archive blocks",
                        ))
                    }
                    Got::Partial => {
                        return Err(self.refuse(
                            Rule::CutShort,
                            ext.path.as_deref().map(shown_path),
                            "the stream ends inside a header",
                        ))
                    }
                }
                if block.iter().all(|b| *b == 0) {
                    if !self.held.is_empty() {
                        return Err(self.refuse(
                            Rule::Malformed,
                            ext.path.as_deref().map(shown_path),
                            "an extended header is followed by no member",
                        ));
                    }
                    return self.tail(start, sink);
                }
                self.held.extend_from_slice(&block);
                let header = Header(&block);
                let typeflag = header.typeflag();
                let (stated, sum) = header.checksums();
                if stated != Some(sum) {
                    let path = match typeflag {
                        b'x' | b'g' | b'L' | b'K' => ext.path.as_deref().map(shown_path),
                        _ => Some(shown_path(
                            &ext.path.clone().unwrap_or_else(|| header.full_name()),
                        )),
                    };
                    return Err(self.refuse(
                        Rule::Checksum,
                        path,
                        format!("the header states {stated:?}; its bytes sum to {sum}"),
                    ));
                }
                match typeflag {
                    b'g' => {
                        return Err(self.refuse(
                            Rule::GlobalHeader,
                            None,
                            "a pax global extended header (type `g`)",
                        ))
                    }
                    b'L' | b'K' => {
                        return Err(self.refuse(
                            Rule::GnuLongName,
                            None,
                            format!(
                                "a GNU long-{} member (type `{}`): the stream is not pax",
                                if typeflag == b'L' { "name" } else { "link" },
                                typeflag as char
                            ),
                        ))
                    }
                    b'x' => {
                        self.names_in_header(&header, ext.path.as_deref().map(shown_path))?;
                        self.extended(&header, &mut ext)?;
                    }
                    _ => break,
                }
            }
            self.member(start, &block, ext, sink)?;
        }
    }

    /// Read an extended header's records into `ext`, holding its blocks.
    fn extended(&mut self, header: &Header<'_>, ext: &mut Extended) -> Result<(), StreamError> {
        let path = ext.path.as_deref().map(shown_path);
        let size = header.size().ok_or_else(|| {
            self.refuse(
                Rule::Malformed,
                path.clone(),
                "an extended header's size cannot be read",
            )
        })?;
        if size > MAX_EXTENDED {
            return Err(self.refuse(
                Rule::Malformed,
                path,
                format!(
                    "an extended header of {size} bytes, past the {MAX_EXTENDED} the reader holds"
                ),
            ));
        }
        let at = self.held.len();
        let want = padded(size) as usize;
        self.held.resize(at + want, 0);
        let mut data = std::mem::take(&mut self.held);
        let got = self.fill(&mut data[at..])?;
        self.held = data;
        if got < want {
            return Err(self.refuse(
                Rule::CutShort,
                path,
                "the stream ends inside an extended header",
            ));
        }
        let records = &self.held[at..at + size as usize];
        let mut parsed: Vec<(Vec<u8>, Vec<u8>)> = Vec::new();
        for r in pax::records(records) {
            match r {
                Ok((k, v)) => parsed.push((k.to_vec(), v.to_vec())),
                Err(e) => {
                    return Err(self.refuse(
                        Rule::Malformed,
                        path,
                        format!("an extended header: {e}"),
                    ))
                }
            }
        }
        for (key, value) in parsed {
            let number = |v: &[u8]| pax::number(v);
            match key.as_slice() {
                pax::PATH => ext.path = Some(value),
                pax::LINKPATH => ext.linkpath = Some(value),
                pax::SIZE | pax::UID | pax::GID => {
                    let n = number(&value).ok_or_else(|| {
                        self.refuse(
                            Rule::Malformed,
                            ext.path.as_deref().map(shown_path),
                            format!(
                                "the `{}` record is not a number: {:?}",
                                lossy(&key),
                                lossy(&value)
                            ),
                        )
                    })?;
                    match key.as_slice() {
                        pax::SIZE => ext.size = Some(n),
                        pax::UID => ext.uid = Some(n),
                        _ => ext.gid = Some(n),
                    }
                }
                pax::ACL_ACCESS => ext.acl_access = Some(value),
                pax::ACL_DEFAULT => ext.acl_default = Some(value),
                pax::UNAME | pax::GNAME => {
                    if ext.name.is_none() {
                        ext.name = Some((key, value));
                    }
                }
                _ => {}
            }
        }
        Ok(())
    }

    /// A header's user and group name fields must be empty (item 270:
    /// `--numeric-owner`).
    fn names_in_header(
        &self,
        header: &Header<'_>,
        path: Option<String>,
    ) -> Result<(), StreamError> {
        let (uname, gname) = (header.uname(), header.gname());
        if uname.is_empty() && gname.is_empty() {
            return Ok(());
        }
        Err(self.refuse(
            Rule::NameInHeader,
            path,
            format!(
                "the header's user name is {:?} and its group name {:?}; the writer passes \
                 --numeric-owner, so both are empty",
                lossy(uname),
                lossy(gname)
            ),
        ))
    }

    /// The member whose own header is `block`: check it, announce it, pass
    /// its bytes, give its row.
    fn member<S: Sink + ?Sized>(
        &mut self,
        start: u64,
        block: &[u8; BLOCK],
        ext: Extended,
        sink: &mut S,
    ) -> Result<(), StreamError> {
        let header = Header(block);
        let name = ext.path.clone().unwrap_or_else(|| header.full_name());
        let path = Some(shown_path(&name));
        let kind = match header.typeflag() {
            b'0' | 0 => Kind::File,
            b'1' => Kind::HardLink,
            b'2' => Kind::Symlink,
            b'3' => Kind::CharDevice,
            b'4' => Kind::BlockDevice,
            b'5' => Kind::Folder,
            b'6' => Kind::Fifo,
            t => {
                return Err(self.refuse(
                    Rule::UnknownType,
                    path,
                    format!("a member of type {:?}", t as char),
                ))
            }
        };
        if header.magic() != b"ustar\0" {
            return Err(self.refuse(
                Rule::Malformed,
                path,
                format!(
                    "not a POSIX ustar header (magic {:?})",
                    lossy(header.magic())
                ),
            ));
        }
        self.names_in_header(&header, path.clone())?;
        if let Some((key, value)) = &ext.name {
            return Err(self.refuse(
                Rule::NameRecord,
                path,
                format!(
                    "a `{}` record ({:?}); the stream holds owners by number only",
                    lossy(key),
                    lossy(value)
                ),
            ));
        }
        let size = match ext.size.or_else(|| header.size()) {
            Some(s) => s,
            None => return Err(self.refuse(Rule::Malformed, path, "the size cannot be read")),
        };
        if kind != Kind::File && size != 0 {
            return Err(self.refuse(
                Rule::Malformed,
                path,
                format!(
                    "a member of kind `{}` carries {size} bytes of data",
                    kind.letter()
                ),
            ));
        }
        let (Some(uid), Some(gid)) = (
            ext.uid.or_else(|| header.uid()),
            ext.gid.or_else(|| header.gid()),
        ) else {
            return Err(self.refuse(Rule::Malformed, path, "the owner or group cannot be read"));
        };
        let (Ok(uid), Ok(gid)) = (u32::try_from(uid), u32::try_from(gid)) else {
            return Err(self.refuse(
                Rule::Malformed,
                path,
                format!("an owner {uid} or group {gid} past 32 bits"),
            ));
        };

        // The next entry of the list.
        let listed = match self.list.next() {
            Some(l) => l,
            None => {
                return Err(self.refuse(
                    Rule::NotNextEntry,
                    path,
                    "the list has no entry left for this member",
                ))
            }
        };

        if !names_entry(&name, &listed.path, kind) || !kind_fits(kind, listed.kind) {
            return Err(self.refuse(
                Rule::NotNextEntry,
                path,
                format!(
                    "the member is `{}` {:?}; the list's next entry is `{}` {:?}",
                    kind.letter(),
                    lossy(&name),
                    listed.kind.letter(),
                    listed.path
                ),
            ));
        }

        // A hard link's target.
        let mut target: Option<(String, u64, [u8; 32])> = None;
        if kind == Kind::HardLink {
            let link = ext
                .linkpath
                .clone()
                .unwrap_or_else(|| header.linkname().to_vec());
            let link = shown_path(&link);
            match self.linked.get(&link) {
                Some((size, sha)) => target = Some((link, *size, *sha)),
                None => {
                    return Err(self.refuse(
                        Rule::LinkTarget,
                        path,
                        format!("its target {link:?} is not a file listed earlier"),
                    ))
                }
            }
        }

        let member_size = target.as_ref().map_or(size, |(_, s, _)| *s);
        if matches!(kind, Kind::File | Kind::HardLink) && member_size != listed.size {
            return Err(self.refuse(
                Rule::NotNextEntry,
                path,
                format!(
                    "the member holds {member_size} bytes; the list says {}",
                    listed.size
                ),
            ));
        }

        // The ACL records: every qualifier a number, and none lost. A hard
        // link carries none: its target's member does.
        let mut acl_ids = Vec::new();
        for (which, record) in [("access", &ext.acl_access), ("default", &ext.acl_default)] {
            if let Some(record) = record {
                match pax::acl_ids(record) {
                    Ok(found) => acl_ids.extend(found),
                    Err(e) => {
                        return Err(self.refuse(
                            Rule::AclQualifier,
                            path,
                            format!("its {which} ACL record: {e}"),
                        ))
                    }
                }
            }
        }
        if self.rules.acls && kind != Kind::HardLink {
            for (which, on_disk, record) in [
                ("access", listed.acl.access, &ext.acl_access),
                ("default", listed.acl.default, &ext.acl_default),
            ] {
                if on_disk && record.is_none() {
                    return Err(self.refuse(
                        Rule::AclLost,
                        path,
                        format!(
                            "it had an {which} ACL on disk and its member carries no \
                             SCHILY.acl.{which} record"
                        ),
                    ));
                }
            }
        }

        // Ids: the member's owner and group, and every ACL qualifier.
        let ids = self
            .ids
            .as_mut()
            .expect("the id collector is held until the end");
        ids.see(IdKind::User, uid);
        ids.see(IdKind::Group, gid);
        for (k, id) in acl_ids {
            ids.see(k, id);
        }

        let data_len = if kind == Kind::File { size } else { 0 };
        let len = self.held.len() as u64 + padded(data_len);
        let slice = sink.member(start, len).map_err(StreamError::Sink)?;
        sink.bytes(&self.held).map_err(StreamError::Sink)?;
        let sha = self.data(data_len, path.clone(), sink)?;
        let end = self.offset;
        debug_assert_eq!(end, start + len);

        let row = match kind {
            Kind::File => {
                if listed.linked {
                    self.linked.insert(listed.path.clone(), (size, sha));
                }
                Row {
                    kind,
                    slice,
                    start,
                    end,
                    size,
                    sha256: Some(sha),
                    path: listed.path,
                    link: None,
                }
            }
            Kind::HardLink => {
                let (link, size, sha) = target.expect("a hard link's target was found");
                Row {
                    kind,
                    slice,
                    start,
                    end,
                    size,
                    sha256: Some(sha),
                    path: listed.path,
                    link: Some(link),
                }
            }
            _ => Row {
                kind,
                slice,
                start,
                end,
                size: 0,
                sha256: None,
                path: listed.path,
                link: None,
            },
        };
        self.members += 1;
        sink.row(row).map_err(StreamError::Sink)
    }

    /// Pass a member's data (`size` bytes, then its padding) to the sink,
    /// hashing the data.
    fn data<S: Sink + ?Sized>(
        &mut self,
        size: u64,
        path: Option<String>,
        sink: &mut S,
    ) -> Result<[u8; 32], StreamError> {
        let mut hasher = Sha256::new();
        let mut left = padded(size);
        let mut data_left = size;
        while left > 0 {
            let buf = loop {
                match self.input.fill_buf() {
                    Ok(b) => break b,
                    Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                    Err(e) => return Err(e.into()),
                }
            };
            if buf.is_empty() {
                return Err(self.refuse(
                    Rule::CutShort,
                    path,
                    format!("the stream ends {left} bytes before the member's end"),
                ));
            }
            let n = (buf.len() as u64).min(left) as usize;
            let d = (n as u64).min(data_left) as usize;
            hasher.update(&buf[..d]);
            sink.bytes(&buf[..n]).map_err(StreamError::Sink)?;
            self.input.consume(n);
            self.offset += n as u64;
            left -= n as u64;
            data_left -= d as u64;
        }
        Ok(hasher.finalize().into())
    }

    /// The first zero block has been read at `start`: read the second, then
    /// count the zeros to the stream's end, then give the sink the tail.
    fn tail<S: Sink + ?Sized>(
        &mut self,
        start: u64,
        sink: &mut S,
    ) -> Result<Finished, StreamError> {
        let mut block = [0u8; BLOCK];
        match self.block(&mut block)? {
            Got::Block if block.iter().all(|b| *b == 0) => {}
            Got::Block => {
                // A lone zero block where a header belongs is a header
                // whose checksum (0) is wrong.
                return Err(self.refuse(
                    Rule::Checksum,
                    None,
                    "a lone zero block where a header belongs",
                ));
            }
            Got::Eof | Got::Partial => {
                return Err(self.refuse(
                    Rule::CutShort,
                    None,
                    "the stream ends after one end-of-archive block",
                ))
            }
        }
        loop {
            let buf = loop {
                match self.input.fill_buf() {
                    Ok(b) => break b,
                    Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                    Err(e) => return Err(e.into()),
                }
            };
            if buf.is_empty() {
                break;
            }
            let n = buf.len();
            let nonzero = buf.iter().position(|b| *b != 0);
            if let Some(at) = nonzero {
                self.offset += at as u64;
                return Err(self.refuse(
                    Rule::AfterEnd,
                    None,
                    "a byte that is not zero after the end-of-archive blocks",
                ));
            }
            self.input.consume(n);
            self.offset += n as u64;
        }
        let len = self.offset - start;
        if !len.is_multiple_of(BLOCK as u64) {
            return Err(self.refuse(Rule::CutShort, None, "the tail ends inside a block"));
        }
        if let Some(missing) = self.list.next() {
            return Err(self.refuse(
                Rule::NeverSeen,
                Some(missing.path),
                "the stream ended before this listed entry",
            ));
        }
        sink.tail(start, len).map_err(StreamError::Sink)?;
        static ZEROS: [u8; CHUNK] = [0; CHUNK];
        let mut left = len;
        while left > 0 {
            let n = left.min(CHUNK as u64) as usize;
            sink.bytes(&ZEROS[..n]).map_err(StreamError::Sink)?;
            left -= n as u64;
        }
        let ids = self.ids.take().expect("the stream ends once");
        Ok(Finished {
            stream_bytes: self.offset,
            tail_start: start,
            members: self.members,
            ids: ids.rows(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tar::writer::{create_args, spawn_archive, write_list, CreateParams, TarFinish};
    use std::ffi::{CString, OsString};
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::fs::{FileTypeExt, MetadataExt};
    use std::path::{Path, PathBuf};
    use tempfile::TempDir;

    // ---------- a tree, its list, and the real tar ----------

    struct Tree {
        _tmp: TempDir,
        root: PathBuf,
        work: PathBuf,
    }

    fn tree() -> Tree {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().join("root");
        let work = tmp.path().join("work");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::create_dir_all(&work).unwrap();
        Tree {
            _tmp: tmp,
            root,
            work,
        }
    }

    fn has_xattr(p: &Path, name: &str) -> bool {
        let path = CString::new(p.as_os_str().as_bytes()).unwrap();
        let name = CString::new(name).unwrap();
        // SAFETY: NUL-terminated strings; a null buffer of length 0 asks
        // only for the value's size.
        let rc =
            unsafe { nix::libc::lgetxattr(path.as_ptr(), name.as_ptr(), std::ptr::null_mut(), 0) };
        rc >= 0
    }

    /// The list entry for `name`, as a snapshot makes it: what `lstat`
    /// says, and whether the ACL attributes are there.
    fn listed(root: &Path, name: &str) -> Listed {
        let p = if name == "." {
            root.to_path_buf()
        } else {
            root.join(name)
        };
        let md = std::fs::symlink_metadata(&p).unwrap();
        let ft = md.file_type();
        let kind = if ft.is_file() {
            Kind::File
        } else if ft.is_dir() {
            Kind::Folder
        } else if ft.is_symlink() {
            Kind::Symlink
        } else if ft.is_fifo() {
            Kind::Fifo
        } else if ft.is_char_device() {
            Kind::CharDevice
        } else if ft.is_block_device() {
            Kind::BlockDevice
        } else {
            // A socket: listed as a file of no size, which tar skips.
            Kind::File
        };
        Listed {
            path: name.to_string(),
            kind,
            size: if ft.is_file() { md.len() } else { 0 },
            acl: AclOnDisk {
                access: has_xattr(&p, "system.posix_acl_access"),
                default: has_xattr(&p, "system.posix_acl_default"),
            },
            linked: ft.is_file() && md.nlink() > 1,
        }
    }

    fn list_of(t: &Tree, names: &[&str]) -> Vec<Listed> {
        names.iter().map(|n| listed(&t.root, n)).collect()
    }

    /// The real tar with the writer's own flags; its stream, and that it
    /// exited 0.
    fn tar_stream(t: &Tree, names: &[&str], xattrs: bool, acls: bool) -> Vec<u8> {
        let list = write_list(&t.work, names).unwrap();
        let p = CreateParams {
            binary: "tar",
            root: &t.root,
            list: &list,
            xattrs,
            acls,
        };
        let mut s = spawn_archive(&p).unwrap();
        let mut out = Vec::new();
        s.take_stdout().read_to_end(&mut out).unwrap();
        let finish = s.finish().unwrap();
        assert!(finish.is_clean(), "{finish:?}");
        out
    }

    /// The real tar with the writer's flags changed by `edit`: a flag
    /// taken away, or one put in.
    fn tar_with(t: &Tree, names: &[&str], edit: impl FnOnce(&mut Vec<OsString>)) -> Vec<u8> {
        let list = write_list(&t.work, names).unwrap();
        let p = CreateParams {
            binary: "tar",
            root: &t.root,
            list: &list,
            xattrs: true,
            acls: true,
        };
        let mut args = create_args(&p).unwrap();
        edit(&mut args);
        let out = crate::tar::command("tar")
            .args(&args)
            .current_dir(&t.root)
            .output()
            .unwrap();
        let finish = TarFinish::from_status(out.status, crate::tar::Stderr::default());
        assert!(
            finish.is_clean(),
            "{finish:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        out.stdout
    }

    fn without(flag: &'static str) -> impl FnOnce(&mut Vec<OsString>) {
        move |args| {
            let before = args.len();
            args.retain(|a| a != flag);
            assert_eq!(args.len(), before - 1, "{flag} was in the line");
        }
    }

    fn with(flag: &'static str) -> impl FnOnce(&mut Vec<OsString>) {
        move |args| args.insert(4, OsString::from(flag))
    }

    fn setfacl(args: &[&str], path: &Path) {
        let st = std::process::Command::new("setfacl")
            .args(args)
            .arg(path)
            .status()
            .unwrap();
        assert!(st.success(), "setfacl {args:?} {}", path.display());
    }

    fn sha256sum(path: &Path) -> [u8; 32] {
        let out = std::process::Command::new("sha256sum")
            .arg(path)
            .output()
            .unwrap();
        assert!(out.status.success());
        let hex = String::from_utf8(out.stdout).unwrap();
        crate::db::files::sha256_from_hex(&hex[..64]).unwrap()
    }

    // ---------- a sink that records everything ----------

    #[derive(Debug, PartialEq, Eq)]
    enum Ev {
        Member { start: u64, len: u64, at: u64 },
        Row { row: Row, at: u64 },
        Tail { start: u64, len: u64, at: u64 },
    }

    #[derive(Default)]
    struct Record {
        events: Vec<Ev>,
        bytes: Vec<u8>,
        largest_bytes_call: usize,
    }

    impl Sink for Record {
        fn member(&mut self, start: u64, len: u64) -> crate::error::Result<u64> {
            self.events.push(Ev::Member {
                start,
                len,
                at: self.bytes.len() as u64,
            });
            Ok(1)
        }
        fn bytes(&mut self, bytes: &[u8]) -> crate::error::Result<()> {
            self.largest_bytes_call = self.largest_bytes_call.max(bytes.len());
            self.bytes.extend_from_slice(bytes);
            Ok(())
        }
        fn row(&mut self, row: Row) -> crate::error::Result<()> {
            self.events.push(Ev::Row {
                row,
                at: self.bytes.len() as u64,
            });
            Ok(())
        }
        fn tail(&mut self, start: u64, len: u64) -> crate::error::Result<()> {
            self.events.push(Ev::Tail {
                start,
                len,
                at: self.bytes.len() as u64,
            });
            Ok(())
        }
    }

    impl Record {
        fn rows(&self) -> Vec<&Row> {
            self.events
                .iter()
                .filter_map(|e| match e {
                    Ev::Row { row, .. } => Some(row),
                    _ => None,
                })
                .collect()
        }
        fn row(&self, path: &str) -> &Row {
            self.rows()
                .into_iter()
                .find(|r| r.path == path)
                .unwrap_or_else(|| panic!("no row for {path}"))
        }
    }

    fn no_names(_: IdKind, _: u32) -> Option<String> {
        None
    }

    fn read_with(
        stream: &[u8],
        list: Vec<Listed>,
        acls: bool,
    ) -> Result<(Finished, Record), StreamError> {
        let mut rec = Record::default();
        let fin = read(
            stream,
            list,
            Rules { acls },
            idmap::Collector::with_lookup(no_names),
            &mut rec,
        )?;
        Ok((fin, rec))
    }

    /// The stream-level assertions of the plan's Verify paragraph, on any
    /// stream that passed: every byte passed through unchanged; each row's
    /// `start` is the row before's `end`, the first 0; the rows' ranges are
    /// the stream less its tail; each member was announced, with its whole
    /// length, before its first byte, and its row came just after its last.
    fn assert_shape(stream: &[u8], fin: &Finished, rec: &Record) {
        assert_eq!(rec.bytes, stream, "the reader changed or dropped bytes");
        assert_eq!(fin.stream_bytes, stream.len() as u64);
        let mut next_start = 0;
        let mut open: Option<(u64, u64)> = None;
        let mut rows = 0;
        let mut tail_seen = false;
        for e in &rec.events {
            match e {
                Ev::Member { start, len, at } => {
                    assert!(open.is_none(), "a member announced inside another");
                    assert_eq!(
                        *start, next_start,
                        "a member does not start where the last ended"
                    );
                    assert_eq!(*at, *start, "announced after some of its bytes");
                    open = Some((*start, *len));
                }
                Ev::Row { row, at } => {
                    let (start, len) = open.take().expect("a row with no announcement");
                    assert_eq!(row.start, start);
                    assert_eq!(row.end - row.start, len, "{}: announced length", row.path);
                    assert_eq!(
                        *at, row.end,
                        "{}: the row came before its last byte",
                        row.path
                    );
                    next_start = row.end;
                    rows += 1;
                }
                Ev::Tail { start, len, at } => {
                    assert!(open.is_none());
                    assert_eq!(*start, next_start, "the tail does not follow the last row");
                    assert_eq!(*at, *start);
                    assert_eq!(start + len, stream.len() as u64);
                    assert_eq!(*start, fin.tail_start);
                    tail_seen = true;
                }
            }
        }
        assert!(tail_seen);
        assert_eq!(rows, fin.members);
        // The tail is zeros, at least the two end-of-archive blocks.
        let tail = &stream[fin.tail_start as usize..];
        assert!(tail.len() >= 2 * BLOCK && tail.iter().all(|b| *b == 0));
    }

    // ---------- the positive tests ----------

    const LONG_ONE: usize = 120;

    /// A tree of every kind the plan's Verify names: long names (over 100
    /// and over 255 bytes, and one of short parts over 100 bytes, which
    /// GNU tar's pax format also gives a `path` record), a
    /// nanosecond time, a symbolic link, a hard link pair whose first name
    /// has an ACL, a FIFO, an empty folder, a file with an ACL, a folder
    /// with a default ACL, a file with a `user.` attribute.
    fn every_kind() -> (Tree, Vec<String>) {
        let t = tree();
        let r = &t.root;
        let long = "n".repeat(LONG_ONE);
        let deep_dir = "x".repeat(200);
        let deep = format!("{deep_dir}/{}", "y".repeat(200));
        let split_dir = format!("{}/{}", "m".repeat(60), "k".repeat(60));
        let split = format!("{split_dir}/{}", "z".repeat(60));
        std::fs::write(r.join("a"), b"the first name of a pair\n").unwrap();
        setfacl(&["-m", "u:1:r,g:2:r"], &r.join("a"));
        std::fs::create_dir(r.join("d")).unwrap();
        std::fs::hard_link(r.join("a"), r.join("d/b")).unwrap();
        std::fs::create_dir(r.join("e")).unwrap();
        setfacl(&["-d", "-m", "u:3:rx"], &r.join("e"));
        std::fs::create_dir(r.join("empty")).unwrap();
        nix::unistd::mkfifo(
            &r.join("p"),
            nix::sys::stat::Mode::from_bits_truncate(0o644),
        )
        .unwrap();
        std::os::unix::fs::symlink("a", r.join("s")).unwrap();
        std::fs::write(r.join(&long), b"a long name\n").unwrap();
        std::fs::create_dir_all(r.join(&deep_dir)).unwrap();
        std::fs::write(r.join(&deep), vec![7u8; 1000]).unwrap();
        std::fs::create_dir_all(r.join(&split_dir)).unwrap();
        std::fs::write(r.join(&split), b"split\n").unwrap();
        std::fs::write(r.join("t"), b"nanoseconds\n").unwrap();
        let when = std::time::UNIX_EPOCH + std::time::Duration::new(1_700_000_000, 123_456_789);
        std::fs::File::options()
            .write(true)
            .open(r.join("t"))
            .unwrap()
            .set_modified(when)
            .unwrap();
        std::fs::write(r.join("x-attr"), b"attributes\n").unwrap();
        let path = CString::new(r.join("x-attr").as_os_str().as_bytes()).unwrap();
        // SAFETY: NUL-terminated strings and a buffer of the length given.
        let rc = unsafe {
            nix::libc::setxattr(
                path.as_ptr(),
                c"user.k".as_ptr(),
                b"v".as_ptr().cast(),
                1,
                0,
            )
        };
        assert_eq!(rc, 0, "{}", std::io::Error::last_os_error());
        let mut names: Vec<String> = vec![
            ".".into(),
            "a".into(),
            "d".into(),
            "d/b".into(),
            "e".into(),
            "empty".into(),
            "p".into(),
            "s".into(),
            long,
            deep_dir,
            deep,
            "m".repeat(60),
            split_dir,
            split,
            "t".into(),
            "x-attr".into(),
        ];
        names.sort_by(|a, b| a.as_bytes().cmp(b.as_bytes()));
        // "." sorts first; the root is listed as `.`.
        (t, names)
    }

    fn strs(v: &[String]) -> Vec<&str> {
        v.iter().map(String::as_str).collect()
    }

    /// Every kind, through the real writer: the stream's shape, the rows'
    /// kinds, sizes and hashes (each against `sha256sum`), the hard link's
    /// row, and the ids.
    #[test]
    fn every_kind_is_read_whole() {
        let (t, names) = every_kind();
        let names = strs(&names);
        let stream = tar_stream(&t, &names, true, true);
        let (fin, rec) = read_with(&stream, list_of(&t, &names), true).unwrap();
        assert_shape(&stream, &fin, &rec);
        assert_eq!(fin.members, names.len() as u64);
        let rows = rec.rows();
        let paths: Vec<&str> = rows.iter().map(|r| r.path.as_str()).collect();
        assert_eq!(paths, names);
        for row in &rows {
            let want = listed(&t.root, &row.path);
            let on_disk = if row.path == "." {
                t.root.clone()
            } else {
                t.root.join(&row.path)
            };
            match row.kind {
                Kind::File => {
                    assert_eq!(row.size, want.size, "{}", row.path);
                    assert_eq!(row.sha256, Some(sha256sum(&on_disk)), "{}", row.path);
                    assert_eq!(row.link, None);
                }
                Kind::HardLink => {}
                _ => {
                    assert_eq!(
                        (row.size, row.sha256, &row.link),
                        (0, None, &None),
                        "{}",
                        row.path
                    );
                    assert_eq!(row.kind, want.kind, "{}", row.path);
                }
            }
        }
        let b = rec.row("d/b");
        assert_eq!(b.kind, Kind::HardLink);
        assert_eq!(b.link.as_deref(), Some("a"));
        assert_eq!(b.size, rec.row("a").size);
        assert_eq!(b.sha256, Some(sha256sum(&t.root.join("a"))));
        assert_eq!(rec.row(".").kind, Kind::Folder);
        assert_eq!(rec.row("p").kind, Kind::Fifo);
        assert_eq!(rec.row("s").kind, Kind::Symlink);
        assert_eq!(rec.row("empty").kind, Kind::Folder);
        // Each row's table text is accepted by the file table's own chain.
        let mut chain = crate::volume::filetable::Chain::default();
        for row in &rows {
            chain.accept(row).unwrap();
        }
        // The ids: the owner's, and every ACL qualifier (the access ACL's
        // 1 and 2, the default ACL's 3).
        let me = nix::unistd::getuid().as_raw();
        let my_group = nix::unistd::getgid().as_raw();
        let ids: Vec<(IdKind, u32)> = fin.ids.iter().map(|r| (r.kind, r.id)).collect();
        for want in [
            (IdKind::User, 1),
            (IdKind::Group, 2),
            (IdKind::User, 3),
            (IdKind::User, me),
            (IdKind::Group, my_group),
        ] {
            assert!(ids.contains(&want), "{want:?} not in {ids:?}");
        }
        assert!(
            !ids.contains(&(IdKind::User, 0)) || me == 0,
            "an extended header's own uid 0 was taken for an owner: {ids:?}"
        );
    }

    /// The same, read from tar's pipe as it is written, not from memory.
    #[test]
    fn the_stream_is_read_from_the_pipe() {
        let (t, names) = every_kind();
        let names = strs(&names);
        let list = write_list(&t.work, &names).unwrap();
        let p = CreateParams {
            binary: "tar",
            root: &t.root,
            list: &list,
            xattrs: true,
            acls: true,
        };
        let mut s = spawn_archive(&p).unwrap();
        let mut rec = Record::default();
        let fin = read(
            s.take_stdout(),
            list_of(&t, &names),
            Rules { acls: true },
            idmap::Collector::with_lookup(no_names),
            &mut rec,
        )
        .unwrap();
        assert!(s.finish().unwrap().is_clean());
        let stream = rec.bytes.clone();
        assert_shape(&stream, &fin, &rec);
        assert_eq!(fin.members, names.len() as u64);
    }

    /// A long-named member, a name of short parts and a nanosecond time
    /// come back
    /// from the member's `start..end` alone, with the real tar; from its
    /// own header alone (the trap) the long name is lost.
    #[test]
    fn a_member_restores_from_its_start_alone() {
        let (t, names) = every_kind();
        let names = strs(&names);
        let stream = tar_stream(&t, &names, true, true);
        let (_fin, rec) = read_with(&stream, list_of(&t, &names), true).unwrap();
        let long = "n".repeat(LONG_ONE);
        let deep = format!("{}/{}", "x".repeat(200), "y".repeat(200));
        let split = format!("{}/{}/{}", "m".repeat(60), "k".repeat(60), "z".repeat(60));
        for path in [long.as_str(), deep.as_str(), split.as_str(), "t"] {
            let row = rec.row(path);
            let dest = TempDir::new().unwrap();
            let mut piece = stream[row.start as usize..row.end as usize].to_vec();
            piece.extend_from_slice(&[0u8; 2 * BLOCK]);
            let out = extract(&piece, dest.path());
            assert!(
                out.status.success(),
                "{path}: {}",
                String::from_utf8_lossy(&out.stderr)
            );
            let got = dest.path().join(path);
            assert_eq!(
                std::fs::read(&got).unwrap(),
                std::fs::read(t.root.join(path)).unwrap(),
                "{path}"
            );
            if path == "t" {
                let md = std::fs::metadata(&got).unwrap();
                assert_eq!((md.mtime(), md.mtime_nsec()), (1_700_000_000, 123_456_789));
            }
        }
        // The control: from the long-named member's own header (past its
        // extension header) the name tar makes is the truncated one.
        let row = rec.row(&long);
        let own = row.end as usize - padded(row.size) as usize - BLOCK;
        assert!(
            own > row.start as usize,
            "the member had an extension header"
        );
        let dest = TempDir::new().unwrap();
        let mut piece = stream[own..row.end as usize].to_vec();
        piece.extend_from_slice(&[0u8; 2 * BLOCK]);
        let out = extract(&piece, dest.path());
        assert!(out.status.success());
        assert!(
            !dest.path().join(&long).exists(),
            "the name survived without its record"
        );
    }

    fn extract(piece: &[u8], dest: &Path) -> std::process::Output {
        use std::io::Write;
        let mut child = crate::tar::command("tar")
            .args(["-x", "-f", "-", "-C"])
            .arg(dest)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        child.stdin.take().unwrap().write_all(piece).unwrap();
        child.wait_with_output().unwrap()
    }

    /// Ids past the header's octal field come from the pax `uid` and `gid`
    /// records, and an ACL qualifier past it is collected too; a hard
    /// link's target name over 100 bytes comes from the `linkpath` record.
    #[test]
    fn large_ids_and_a_long_link_target_come_from_records() {
        let t = tree();
        let first = "t".repeat(150);
        std::fs::write(t.root.join(&first), b"linked\n").unwrap();
        std::fs::hard_link(t.root.join(&first), t.root.join("u")).unwrap();
        setfacl(&["-m", "u:4000002:r"], &t.root.join(&first));
        let names = [first.as_str(), "u"];
        let stream = tar_with(&t, &names, |args| {
            args.insert(4, OsString::from("--owner=4000000"));
            args.insert(5, OsString::from("--group=4000001"));
        });
        let (fin, rec) = read_with(&stream, list_of(&t, &names), true).unwrap();
        assert_shape(&stream, &fin, &rec);
        let ids: Vec<(IdKind, u32)> = fin.ids.iter().map(|r| (r.kind, r.id)).collect();
        assert_eq!(
            ids,
            vec![
                (IdKind::User, 4_000_000),
                (IdKind::User, 4_000_002),
                (IdKind::Group, 4_000_001)
            ]
        );
        let u = rec.row("u");
        assert_eq!(u.kind, Kind::HardLink);
        assert_eq!(u.link.as_deref(), Some(first.as_str()));
        assert_eq!(u.sha256, Some(sha256sum(&t.root.join("u"))));
    }

    /// The names the id map records come from the collector's lookup, once
    /// for each id.
    #[test]
    fn the_id_map_names_each_id_once() {
        let t = tree();
        std::fs::write(t.root.join("a"), b"1").unwrap();
        std::fs::write(t.root.join("b"), b"2").unwrap();
        let names = ["a", "b"];
        let stream = tar_stream(&t, &names, true, true);
        let mut asked = Vec::new();
        let mut rec = Record::default();
        let fin = read(
            &stream[..],
            list_of(&t, &names),
            Rules { acls: true },
            idmap::Collector::with_lookup(|k, id| {
                asked.push((k, id));
                Some(format!("{}{id}", k.letter()))
            }),
            &mut rec,
        )
        .unwrap();
        let me = nix::unistd::getuid().as_raw();
        let my_group = nix::unistd::getgid().as_raw();
        assert_eq!(asked, vec![(IdKind::User, me), (IdKind::Group, my_group)]);
        assert_eq!(
            fin.ids,
            vec![
                IdRow {
                    kind: IdKind::User,
                    id: me,
                    name: Some(format!("u{me}"))
                },
                IdRow {
                    kind: IdKind::Group,
                    id: my_group,
                    name: Some(format!("g{my_group}"))
                },
            ]
        );
    }

    /// With `preserve_acls` off and the writer's flags for it, a file with
    /// an ACL on disk passes: no ACL record is asked for.
    #[test]
    fn acls_off_asks_for_no_record() {
        let t = tree();
        std::fs::write(t.root.join("a"), b"1").unwrap();
        setfacl(&["-m", "u:1:r"], &t.root.join("a"));
        let stream = tar_stream(&t, &["a"], true, false);
        let (fin, rec) = read_with(&stream, list_of(&t, &["a"]), false).unwrap();
        assert_shape(&stream, &fin, &rec);
    }

    /// An empty list: tar writes only the tail, and the reader gives no
    /// row and announces the tail.
    #[test]
    fn an_empty_stream_is_its_tail() {
        let t = tree();
        let stream = tar_stream(&t, &[], true, true);
        let (fin, rec) = read_with(&stream, vec![], true).unwrap();
        assert_shape(&stream, &fin, &rec);
        assert_eq!((fin.members, fin.tail_start), (0, 0));
    }

    /// A member larger than the buffer passes in pieces no larger than
    /// [`CHUNK`], and its hash is still the file's.
    #[test]
    fn a_member_larger_than_the_buffer_passes_in_pieces() {
        let t = tree();
        let data: Vec<u8> = (0..(3 * CHUNK + 77)).map(|i| (i % 251) as u8).collect();
        std::fs::write(t.root.join("big"), &data).unwrap();
        let stream = tar_stream(&t, &["big"], true, true);
        let (fin, rec) = read_with(&stream, list_of(&t, &["big"]), true).unwrap();
        assert_shape(&stream, &fin, &rec);
        assert!(rec.largest_bytes_call <= CHUNK);
        assert_eq!(rec.row("big").sha256, Some(sha256sum(&t.root.join("big"))));
    }

    // ---------- the refusals (D12; §3.2; step 5) ----------

    fn refused(r: Result<(Finished, Record), StreamError>) -> Refusal {
        match r {
            Err(StreamError::Refused(r)) => r,
            Err(e) => panic!("not a refusal: {e}"),
            Ok(_) => panic!("the stream was accepted"),
        }
    }

    /// Assert the refusal is by `rule` and names `path`.
    fn assert_refused(r: Result<(Finished, Record), StreamError>, rule: Rule, path: Option<&str>) {
        let refusal = refused(r);
        assert_eq!(refusal.rule, rule, "{refusal}");
        assert_eq!(refusal.path.as_deref(), path, "{refusal}");
        let text = TapectlError::from(StreamError::Refused(refusal.clone())).to_string();
        assert!(text.contains(rule.describe()), "{text}");
        if let Some(p) = path {
            assert!(text.contains(p), "{text}");
        }
    }

    /// Replace the one occurrence of `from` in `stream` by `to`, of the
    /// same length, so no offset moves.
    fn replace_once(stream: &mut [u8], from: &[u8], to: &[u8]) {
        assert_eq!(from.len(), to.len());
        let found: Vec<usize> = stream
            .windows(from.len())
            .enumerate()
            .filter(|(_, w)| *w == from)
            .map(|(i, _)| i)
            .collect();
        assert_eq!(found.len(), 1, "{:?}", String::from_utf8_lossy(from));
        stream[found[0]..found[0] + to.len()].copy_from_slice(to);
    }

    /// Recompute a header block's checksum after a patch.
    fn fix_checksum(h: &mut [u8]) {
        h[148..156].copy_from_slice(b"        ");
        let sum: u32 = h[..BLOCK].iter().map(|b| *b as u32).sum();
        h[148..156].copy_from_slice(format!("{sum:06o}\0 ").as_bytes());
    }

    /// The offset of a member's own header (past its extension headers).
    fn own_header(row: &Row) -> usize {
        let data = if row.kind == Kind::File { row.size } else { 0 };
        row.end as usize - padded(data) as usize - BLOCK
    }

    fn two_files() -> (Tree, Vec<u8>, Vec<Row>) {
        let t = tree();
        std::fs::write(t.root.join("a"), b"first\n").unwrap();
        std::fs::write(t.root.join("b"), vec![9u8; 700]).unwrap();
        // A fixed time with nine nanosecond digits: GNU tar drops trailing
        // zeros from the `mtime` record, so a time read off the clock made
        // its length, and every offset a test computes from it, vary.
        let at = std::time::UNIX_EPOCH + std::time::Duration::new(1_700_000_000, 123_456_789);
        for name in ["a", "b"] {
            std::fs::File::options()
                .write(true)
                .open(t.root.join(name))
                .unwrap()
                .set_modified(at)
                .unwrap();
        }
        nix::unistd::mkfifo(
            &t.root.join("p"),
            nix::sys::stat::Mode::from_bits_truncate(0o644),
        )
        .unwrap();
        let stream = tar_stream(&t, &["a", "b", "p"], true, true);
        let (_, rec) = read_with(&stream, list_of(&t, &["a", "b", "p"]), true).unwrap();
        let rows = rec.rows().into_iter().cloned().collect();
        (t, stream, rows)
    }

    fn socket(path: &Path) -> std::os::unix::net::UnixListener {
        std::os::unix::net::UnixListener::bind(path).unwrap()
    }

    /// A list with a name tar skips (a socket, item 270): tar exits 0
    /// without its member, so the next member is not the listed entry.
    #[test]
    fn a_name_tar_skips_is_not_the_next_entry() {
        let t = tree();
        std::fs::write(t.root.join("a"), b"1").unwrap();
        let _s = socket(&t.root.join("m"));
        std::fs::write(t.root.join("z"), b"2").unwrap();
        let names = ["a", "m", "z"];
        let stream = tar_stream(&t, &names, true, true);
        assert_refused(
            read_with(&stream, list_of(&t, &names), true),
            Rule::NotNextEntry,
            Some("z"),
        );
    }

    /// The same skipped name listed last: never seen.
    #[test]
    fn a_name_tar_skips_listed_last_is_never_seen() {
        let t = tree();
        std::fs::write(t.root.join("a"), b"1").unwrap();
        let _s = socket(&t.root.join("sock"));
        let names = ["a", "sock"];
        let stream = tar_stream(&t, &names, true, true);
        assert_refused(
            read_with(&stream, list_of(&t, &names), true),
            Rule::NeverSeen,
            Some("sock"),
        );
    }

    /// A member whose kind or size is not the listed entry's (the file
    /// was replaced, or grew, between the snapshot and tar), and a member
    /// with no entry left for it.
    #[test]
    fn a_kind_or_size_or_member_not_listed_is_not_the_next_entry() {
        let (t, stream, _) = two_files();
        let mut list = list_of(&t, &["a", "b", "p"]);
        list[1].kind = Kind::Folder;
        assert_refused(
            read_with(&stream, list, true),
            Rule::NotNextEntry,
            Some("b"),
        );
        let mut list = list_of(&t, &["a", "b", "p"]);
        list[1].size += 1;
        assert_refused(
            read_with(&stream, list, true),
            Rule::NotNextEntry,
            Some("b"),
        );
        let mut list = list_of(&t, &["a", "b", "p"]);
        list[2].kind = Kind::File;
        assert_refused(
            read_with(&stream, list, true),
            Rule::NotNextEntry,
            Some("p"),
        );
        let list = list_of(&t, &["a", "b"]);
        assert_refused(
            read_with(&stream, list, true),
            Rule::NotNextEntry,
            Some("p"),
        );
        let mut list = list_of(&t, &["a", "b", "p"]);
        list[0].path = "A".into();
        assert_refused(
            read_with(&stream, list, true),
            Rule::NotNextEntry,
            Some("a"),
        );
    }

    /// The real tar without `--numeric-owner`: the header names the owner
    /// (item 270).
    #[test]
    fn a_name_in_a_header_is_refused() {
        let t = tree();
        std::fs::write(t.root.join("a"), b"1").unwrap();
        let stream = tar_with(&t, &["a"], without("--numeric-owner"));
        // Positive control: the owner is named on this host, so the field
        // is filled.
        let own = <&[u8; BLOCK]>::try_from(&stream[BLOCK * 2..BLOCK * 3]).unwrap();
        assert!(!Header(own).uname().is_empty());
        assert_refused(
            read_with(&stream, list_of(&t, &["a"]), true),
            Rule::NameInHeader,
            Some("a"),
        );
    }

    /// The real tar told to add a `uname` or a `gname` record.
    #[test]
    fn a_name_record_is_refused() {
        for flag in ["--pax-option=uname:=bob", "--pax-option=gname:=bob"] {
            let t = tree();
            std::fs::write(t.root.join("a"), b"1").unwrap();
            let stream = tar_with(&t, &["a"], with(flag));
            assert_refused(
                read_with(&stream, list_of(&t, &["a"]), true),
                Rule::NameRecord,
                Some("a"),
            );
        }
    }

    /// An ACL entry that names an account where a number belongs (GNU tar
    /// below 1.35, #563): the real stream's `user:1:` made `user:d:`.
    #[test]
    fn an_acl_qualifier_that_is_not_a_number_is_refused() {
        for (record, from, to) in [
            ("access", &b"user:1:r--"[..], &b"user:d:r--"[..]),
            ("default", b"user:3:r-x", b"user:x:r-x"),
        ] {
            let t = tree();
            std::fs::write(t.root.join("a"), b"1").unwrap();
            setfacl(&["-m", "u:1:r"], &t.root.join("a"));
            std::fs::create_dir(t.root.join("e")).unwrap();
            setfacl(&["-d", "-m", "u:3:rx"], &t.root.join("e"));
            let names = ["a", "e"];
            let mut stream = tar_stream(&t, &names, true, true);
            read_with(&stream, list_of(&t, &names), true).unwrap();
            replace_once(&mut stream, from, to);
            let path = if record == "access" { "a" } else { "e" };
            assert_refused(
                read_with(&stream, list_of(&t, &names), true),
                Rule::AclQualifier,
                Some(path),
            );
        }
    }

    /// The real tar without `--acls` over a file with an access ACL and a
    /// folder with a default ACL: each ACL is lost (items 273, 274). With
    /// `preserve_acls` off the same streams pass.
    #[test]
    fn a_lost_acl_is_refused() {
        let t = tree();
        std::fs::write(t.root.join("a"), b"1").unwrap();
        setfacl(&["-m", "u:1:r"], &t.root.join("a"));
        std::fs::create_dir(t.root.join("e")).unwrap();
        setfacl(&["-d", "-m", "u:3:rx"], &t.root.join("e"));
        for name in ["a", "e"] {
            let stream = tar_with(&t, &[name], without("--acls"));
            assert_refused(
                read_with(&stream, list_of(&t, &[name]), true),
                Rule::AclLost,
                Some(name),
            );
            read_with(&stream, list_of(&t, &[name]), false).unwrap();
        }
    }

    /// The second name of a hard-linked file with an ACL carries no ACL
    /// record (its target's member does), and is not refused for it.
    #[test]
    fn a_hard_link_carries_no_acl_record_and_passes() {
        let t = tree();
        std::fs::write(t.root.join("a"), b"1").unwrap();
        setfacl(&["-m", "u:1:r"], &t.root.join("a"));
        std::fs::hard_link(t.root.join("a"), t.root.join("b")).unwrap();
        let names = ["a", "b"];
        let stream = tar_stream(&t, &names, true, true);
        let list = list_of(&t, &names);
        assert!(list[1].acl.access, "the link's inode has the ACL");
        let (fin, rec) = read_with(&stream, list, true).unwrap();
        assert_shape(&stream, &fin, &rec);
        assert_eq!(rec.row("b").kind, Kind::HardLink);
    }

    /// The real tar told to write a global extended header.
    #[test]
    fn a_global_header_is_refused() {
        let t = tree();
        std::fs::write(t.root.join("a"), b"1").unwrap();
        let stream = tar_with(&t, &["a"], with("--pax-option=comment=x"));
        assert_eq!(stream[156], b'g', "the control: tar wrote a global header");
        assert_refused(
            read_with(&stream, list_of(&t, &["a"]), true),
            Rule::GlobalHeader,
            None,
        );
    }

    /// The real tar with `--format=gnu` over a long name.
    #[test]
    fn a_gnu_long_name_is_refused() {
        let t = tree();
        let long = "n".repeat(LONG_ONE);
        std::fs::write(t.root.join(&long), b"1").unwrap();
        let stream = tar_with(&t, &[long.as_str()], gnu_format);
        assert_eq!(
            stream[156], b'L',
            "the control: tar wrote a long-name member"
        );
        assert_refused(
            read_with(&stream, list_of(&t, &[long.as_str()]), true),
            Rule::GnuLongName,
            None,
        );
        // A symbolic link with a long target: a GNU long-link member (`K`).
        let t = tree();
        std::os::unix::fs::symlink("q".repeat(LONG_ONE), t.root.join("s")).unwrap();
        let stream = tar_with(&t, &["s"], gnu_format);
        assert_eq!(
            stream[156], b'K',
            "the control: tar wrote a long-link member"
        );
        assert_refused(
            read_with(&stream, list_of(&t, &["s"]), true),
            Rule::GnuLongName,
            None,
        );
    }

    /// The writer's line with `--format=ustar`: the flags only pax takes
    /// go (`--xattrs` and `--acls` make GNU tar write pax whatever the
    /// format asked, and `--pax-option` is refused outside pax; measured).
    fn ustar_format(args: &mut Vec<OsString>) {
        args.retain(|a| {
            let a = a.to_string_lossy();
            !(a.starts_with("--xattrs") || a == "--acls" || a.starts_with("--pax-option"))
        });
        for a in args.iter_mut() {
            if a == "--format=pax" {
                *a = OsString::from("--format=ustar");
            }
        }
    }

    /// The writer's line with `--format=gnu`, likewise.
    fn gnu_format(args: &mut Vec<OsString>) {
        ustar_format(args);
        for a in args.iter_mut() {
            if a == "--format=ustar" {
                *a = OsString::from("--format=gnu");
            }
        }
    }

    /// A name of short parts over 100 bytes in a ustar header with no
    /// extension header (GNU tar's pax format gives it a `path` record
    /// instead, measured): the reader joins the prefix field to the name.
    #[test]
    fn a_name_in_the_prefix_field_is_joined() {
        let t = tree();
        let dir = format!("{}/{}", "m".repeat(60), "k".repeat(60));
        let file = format!("{dir}/{}", "z".repeat(60));
        std::fs::create_dir_all(t.root.join(&dir)).unwrap();
        std::fs::write(t.root.join(&file), b"split\n").unwrap();
        let stream = tar_with(&t, &[file.as_str()], ustar_format);
        assert_ne!(stream[345], 0, "the control: the prefix field is filled");
        let (fin, rec) = read_with(&stream, list_of(&t, &[file.as_str()]), false).unwrap();
        assert_shape(&stream, &fin, &rec);
        assert_eq!(rec.row(&file).sha256, Some(sha256sum(&t.root.join(&file))));
    }

    /// An extension header whose own user name field is filled (checksum
    /// made right) is refused as a name in a header.
    #[test]
    fn a_name_in_an_extension_header_is_refused() {
        let (t, stream, rows) = two_files();
        let x = rows[1].start as usize;
        assert_eq!(stream[x + 156], b'x');
        let mut bad = stream.clone();
        bad[x + 265..x + 268].copy_from_slice(b"bob");
        fix_checksum(&mut bad[x..x + BLOCK]);
        assert_refused(
            read_with(&bad, list_of(&t, &["a", "b", "p"]), true),
            Rule::NameInHeader,
            None,
        );
    }

    /// A byte of a member's own header, or of an extension header, changed
    /// without its checksum; and a zero block where a header belongs that
    /// is not followed by a second.
    #[test]
    fn a_wrong_checksum_is_refused() {
        let (t, stream, rows) = two_files();
        let list = || list_of(&t, &["a", "b", "p"]);
        let b = &rows[1];
        let mut bad = stream.clone();
        bad[own_header(b) + 103] ^= 1; // a mode digit
        assert_refused(read_with(&bad, list(), true), Rule::Checksum, Some("b"));
        let mut bad = stream.clone();
        bad[b.start as usize + 103] ^= 1; // the extension header's
        assert_refused(read_with(&bad, list(), true), Rule::Checksum, None);
        let mut lone = stream[..b.start as usize].to_vec();
        lone.extend_from_slice(&[0u8; BLOCK]);
        lone.extend_from_slice(&stream[b.start as usize..]);
        assert_refused(read_with(&lone, list(), true), Rule::Checksum, None);
    }

    /// A member's type changed to one the reader does not know (checksum
    /// made right); the device types it does know are read as devices.
    #[test]
    fn a_type_it_does_not_know_is_refused() {
        let (t, stream, rows) = two_files();
        let p = own_header(&rows[2]);
        for (flag, kind) in [(b'3', Kind::CharDevice), (b'4', Kind::BlockDevice)] {
            let mut dev = stream.clone();
            dev[p + 156] = flag;
            fix_checksum(&mut dev[p..p + BLOCK]);
            let mut list = list_of(&t, &["a", "b", "p"]);
            list[2].kind = kind;
            let (fin, rec) = read_with(&dev, list, true).unwrap();
            assert_shape(&dev, &fin, &rec);
            assert_eq!(rec.row("p").kind, kind);
        }
        for flag in [b'Z', b'7', b'S', b'D'] {
            let mut bad = stream.clone();
            bad[p + 156] = flag;
            fix_checksum(&mut bad[p..p + BLOCK]);
            assert_refused(
                read_with(&bad, list_of(&t, &["a", "b", "p"]), true),
                Rule::UnknownType,
                Some("p"),
            );
        }
    }

    /// Anything but zeros after the end-of-archive blocks: a second
    /// stream after the first, and one byte in the padding.
    #[test]
    fn only_zeros_may_follow_the_end() {
        let (t, stream, _) = two_files();
        let list = || list_of(&t, &["a", "b", "p"]);
        let mut two = stream.clone();
        two.extend_from_slice(&stream);
        assert_refused(read_with(&two, list(), true), Rule::AfterEnd, None);
        let mut one = stream.clone();
        let last = one.len() - 1;
        one[last] = 1;
        assert_refused(read_with(&one, list(), true), Rule::AfterEnd, None);
    }

    /// A stream cut short: inside an extension header's records, inside a
    /// member's own header, inside its data, inside its padding, exactly
    /// at a member's end with no end-of-archive blocks, after one of them,
    /// and inside the tail's padding.
    #[test]
    fn a_stream_cut_short_is_refused() {
        let (t, stream, rows) = two_files();
        let list = || list_of(&t, &["a", "b", "p"]);
        let b = &rows[1];
        let own = own_header(b);
        let tail = rows[2].end as usize;
        for (at, path) in [
            (b.start as usize + BLOCK + 10, None),
            (own + 100, None),
            (own + BLOCK + 300, Some("b")),
            (own + BLOCK + 700 + 10, Some("b")),
            (b.end as usize, None),
            (tail, None),
            (tail + BLOCK, None),
            (tail + 2 * BLOCK + 100, None),
        ] {
            assert_refused(read_with(&stream[..at], list(), true), Rule::CutShort, path);
        }
    }

    /// A hard link whose target was not marked linked in the list, and one
    /// whose target name is not a listed file.
    #[test]
    fn a_hard_link_to_an_unlisted_target_is_refused() {
        let t = tree();
        std::fs::write(t.root.join("a"), b"1").unwrap();
        std::fs::hard_link(t.root.join("a"), t.root.join("b")).unwrap();
        let names = ["a", "b"];
        let stream = tar_stream(&t, &names, true, true);
        let mut list = list_of(&t, &names);
        list[0].linked = false;
        assert_refused(read_with(&stream, list, true), Rule::LinkTarget, Some("b"));
        let (_, rec) = read_with(&stream, list_of(&t, &names), true).unwrap();
        let own = own_header(rec.row("b"));
        let mut bad = stream.clone();
        assert_eq!(&bad[own + 157..own + 160], b"./a");
        bad[own + 159] = b'q';
        fix_checksum(&mut bad[own..own + BLOCK]);
        assert_refused(
            read_with(&bad, list_of(&t, &names), true),
            Rule::LinkTarget,
            Some("b"),
        );
    }

    /// What cannot be read as GNU tar's pax: a record that does not parse,
    /// a member of a kind with no data that states a size, an extension
    /// header with no member after it.
    #[test]
    fn what_cannot_be_read_is_malformed() {
        let (t, stream, rows) = two_files();
        let list = || list_of(&t, &["a", "b", "p"]);
        let mut bad = stream.clone();
        let at = rows[1].start as usize + BLOCK;
        assert_eq!(&bad[at..at + 8], b"30 mtime");
        bad[at + 1] = b'X';
        assert_refused(read_with(&bad, list(), true), Rule::Malformed, None);
        let p = own_header(&rows[2]);
        let mut bad = stream.clone();
        bad[p + 124..p + 136].copy_from_slice(b"00000001000\0");
        fix_checksum(&mut bad[p..p + BLOCK]);
        assert_refused(read_with(&bad, list(), true), Rule::Malformed, Some("p"));
        let mut bad = stream[..rows[2].start as usize + 2 * BLOCK].to_vec();
        bad.extend_from_slice(&[0u8; 2 * BLOCK]);
        assert_refused(read_with(&bad, list(), true), Rule::Malformed, None);
    }

    // ---------- a member over 8 GiB (gated) ----------

    /// A sink that keeps nothing of the bytes: it counts them, and holds
    /// the announcements and rows.
    #[derive(Default)]
    struct Count {
        bytes: u64,
        largest: usize,
        calls: u64,
        events: Vec<Ev>,
    }

    impl Sink for Count {
        fn member(&mut self, start: u64, len: u64) -> crate::error::Result<u64> {
            self.events.push(Ev::Member {
                start,
                len,
                at: self.bytes,
            });
            Ok(1)
        }
        fn bytes(&mut self, bytes: &[u8]) -> crate::error::Result<()> {
            self.bytes += bytes.len() as u64;
            self.calls += 1;
            if self.calls > 1 {
                self.largest = self.largest.max(bytes.len());
            }
            Ok(())
        }
        fn row(&mut self, row: Row) -> crate::error::Result<()> {
            self.events.push(Ev::Row {
                row,
                at: self.bytes,
            });
            Ok(())
        }
        fn tail(&mut self, start: u64, len: u64) -> crate::error::Result<()> {
            self.events.push(Ev::Tail {
                start,
                len,
                at: self.bytes,
            });
            Ok(())
        }
    }

    /// A member over 8 GiB, from the real writer's pipe: its size comes
    /// from the pax `size` record (the header's octal field cannot hold
    /// it), its sha256 is `sha256sum`'s, it is announced whole before its
    /// first byte, and its data passes in pieces of at most [`CHUNK`]
    /// bytes, never held. The file is sparse, so it costs no disk; tar
    /// reads its holes as zeros (#555 measured 9 and 96 GiB that way).
    #[test]
    #[ignore = "perf suite: set TAPECTL_PERF_TESTS=1 and pass --ignored"]
    fn a_member_over_8_gib_is_read_by_its_size_record() {
        if std::env::var("TAPECTL_PERF_TESTS").is_err() {
            eprintln!("skip: TAPECTL_PERF_TESTS not set");
            return;
        }
        use std::os::unix::fs::FileExt;
        let t = tree();
        let size: u64 = (8 << 30) + 4096 + 3;
        assert!(
            size > 0o77_777_777_777,
            "past the header's octal size field"
        );
        let f = std::fs::File::create(t.root.join("huge")).unwrap();
        f.set_len(size).unwrap();
        f.write_at(b"end", size - 3).unwrap();
        f.write_at(b"start", 0).unwrap();
        drop(f);
        std::fs::write(t.root.join("small"), b"after\n").unwrap();
        let names = ["huge", "small"];
        let list = write_list(&t.work, &names).unwrap();
        let p = CreateParams {
            binary: "tar",
            root: &t.root,
            list: &list,
            xattrs: true,
            acls: true,
        };
        let mut s = spawn_archive(&p).unwrap();
        let mut sink = Count::default();
        let fin = read(
            s.take_stdout(),
            list_of(&t, &names),
            Rules { acls: true },
            idmap::Collector::with_lookup(no_names),
            &mut sink,
        )
        .unwrap();
        assert!(s.finish().unwrap().is_clean());
        assert_eq!(fin.members, 2);
        assert_eq!(sink.bytes, fin.stream_bytes);
        assert!(sink.largest <= CHUNK, "{} bytes in one call", sink.largest);
        let (start, len, at) = match &sink.events[0] {
            Ev::Member { start, len, at } => (*start, *len, *at),
            other => panic!("{other:?}"),
        };
        assert_eq!((start, at), (0, 0));
        let row = match &sink.events[1] {
            Ev::Row { row, at } => {
                assert_eq!(*at, row.end);
                row.clone()
            }
            other => panic!("{other:?}"),
        };
        assert_eq!(row.path, "huge");
        assert_eq!(row.size, size);
        assert_eq!(row.end - row.start, len);
        assert!(len > size);
        assert_eq!(row.sha256, Some(sha256sum(&t.root.join("huge"))));
        match &sink.events[2] {
            Ev::Member { start, .. } => assert_eq!(*start, row.end),
            other => panic!("{other:?}"),
        }
    }
}
