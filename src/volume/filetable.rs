//! A staging's file table (`volume-format-v3.md` §1.6; ADR-0012 items 254,
//! 264, 265, 283, 293; the plan's D4): one row for each member of the
//! staging's tar stream, in stream order, with the slice it begins in, its
//! offsets in the stream, its size and its sha256.
//!
//! This module is the table's text (written and parsed, both streaming) and
//! the questions readers ask of it: which byte ranges hold some paths
//! ([`ranges`]), which slices some members need ([`slices_of`]), where a
//! slice begins in the stream ([`slice_start`]), which members a set of
//! slices holds whole ([`held_whole`]) and which a lost slice costs
//! ([`members_of`]). The catalog's copy is `db::stage_files`, which stores
//! the same [`Row`]s.
//!
//! **Geometry** (§3.3, D3). A member's length is `end - start` (its
//! extension headers, its header and its padded data), never its `size`. A
//! member no longer than `slice_size` lies in its `slice`; a longer one is a
//! *run*: it begins a slice and holds `ceil(length / slice_size)` slices,
//! each `slice_size` but the last. The rows alone cannot tell a last
//! member's run or a continuation slice's offset, so every question of
//! geometry takes `slice_size`.
//!
//! **Hard links** (items 256, 293). A hard link's own member holds no data:
//! what it names is its target's bytes, the member of the name archived
//! first. So a hard link needs its target's slices too: [`select`] brings
//! the target, [`held_whole`] holds a link only when its target is held,
//! and [`members_of`] counts a link whose target a lost slice costs.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::io::{BufRead, Write};
use std::ops::Range;

use crate::db::files::{sha256_from_hex, sha256_to_hex};
use crate::error::{Result, TapectlError};
use crate::volume::index::{escape, unescape};

/// The table's first line.
pub const HEADER: &str = "#tapectl-file-table 1";

/// The second line's prefix; the stream's length follows it.
pub const STREAM_BYTES: &str = "#stream_bytes ";

/// A member's kind: the text's letter (§1.6). A socket is never a member
/// (item 270).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Kind {
    /// `f`
    File,
    /// `d`
    Folder,
    /// `l`
    Symlink,
    /// `h`
    HardLink,
    /// `p`
    Fifo,
    /// `c`
    CharDevice,
    /// `b`
    BlockDevice,
}

impl Kind {
    /// The text's (and the catalog's) letter.
    pub fn letter(self) -> &'static str {
        match self {
            Kind::File => "f",
            Kind::Folder => "d",
            Kind::Symlink => "l",
            Kind::HardLink => "h",
            Kind::Fifo => "p",
            Kind::CharDevice => "c",
            Kind::BlockDevice => "b",
        }
    }

    /// The inverse of [`Kind::letter`].
    pub fn from_letter(letter: &str) -> Option<Self> {
        Some(match letter {
            "f" => Kind::File,
            "d" => Kind::Folder,
            "l" => Kind::Symlink,
            "h" => Kind::HardLink,
            "p" => Kind::Fifo,
            "c" => Kind::CharDevice,
            "b" => Kind::BlockDevice,
            _ => return None,
        })
    }

    /// Whether the row states a sha256: a file's own, a hard link's
    /// target's. Every other kind has `-`.
    pub fn hashed(self) -> bool {
        matches!(self, Kind::File | Kind::HardLink)
    }
}

/// One member of a staging's stream, as the text and the catalog hold it.
/// `path` and `link` are the paths themselves, unescaped: relative to the
/// unit's root with no leading `./`, the root itself `.`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Row {
    pub kind: Kind,
    /// The slice the member begins in, from 1.
    pub slice: u64,
    /// The offset of the member's first extension header in the stream.
    pub start: u64,
    /// The offset just past the member's last padded block.
    pub end: u64,
    /// The file's bytes; 0 for a kind with no data.
    pub size: u64,
    /// For [`Kind::hashed`] kinds only.
    pub sha256: Option<[u8; 32]>,
    pub path: String,
    /// A hard link's target (the name archived first); `None` for any
    /// other kind.
    pub link: Option<String>,
}

impl Row {
    /// The member's whole length in the stream.
    pub fn len(&self) -> u64 {
        self.end - self.start
    }

    /// Never true of a row the [`Chain`] accepted: a member holds at least
    /// its header.
    pub fn is_empty(&self) -> bool {
        self.end == self.start
    }

    /// The last slice holding the member's bytes: its own `slice`, or the
    /// end of its run when it is longer than `slice_size` (§3.3).
    pub fn last_slice(&self, slice_size: u64) -> u64 {
        if slice_size > 0 && self.len() > slice_size {
            self.slice + self.len().div_ceil(slice_size) - 1
        } else {
            self.slice
        }
    }

    /// Every slice holding the member's bytes.
    pub fn slices(&self, slice_size: u64) -> std::ops::RangeInclusive<u64> {
        self.slice..=self.last_slice(slice_size)
    }

    /// Whether the member is a run (longer than a slice).
    pub fn is_run(&self, slice_size: u64) -> bool {
        self.len() > slice_size
    }
}

/// The rules a sequence of rows keeps (§1.6, #562): the first starts at 0
/// and lies in slice 1, each starts where the one before ended, no slice is
/// lower than the one before's, the hash and the link are stated for the
/// kinds that have them and for no other. The writer, the parser and the
/// catalog's insert all hold rows to it, so no copy of a table breaks it.
#[derive(Debug, Default)]
pub struct Chain {
    rows: u64,
    next_start: u64,
    last_slice: u64,
}

impl Chain {
    /// Accept `row` as the next one, or say what is wrong with it.
    pub fn accept(&mut self, row: &Row) -> std::result::Result<(), String> {
        if row.start != self.next_start {
            return Err(if self.rows == 0 {
                format!("the first member starts at {}, not 0", row.start)
            } else {
                format!(
                    "`start` {} is not the row before's `end` {}",
                    row.start, self.next_start
                )
            });
        }
        if row.end <= row.start {
            return Err(format!(
                "`end` {} is not past `start` {}",
                row.end, row.start
            ));
        }
        if self.rows == 0 && row.slice != 1 {
            return Err(format!("the first member is in slice {}, not 1", row.slice));
        }
        if row.slice < self.last_slice {
            return Err(format!(
                "`slice` {} is lower than the row before's {}",
                row.slice, self.last_slice
            ));
        }
        if row.kind.hashed() != row.sha256.is_some() {
            return Err(if row.kind.hashed() {
                format!("a `{}` row has no sha256", row.kind.letter())
            } else {
                format!("a `{}` row states a sha256", row.kind.letter())
            });
        }
        if (row.kind == Kind::HardLink) != row.link.is_some() {
            return Err(if row.kind == Kind::HardLink {
                "an `h` row names no link target".to_string()
            } else {
                format!("a `{}` row names a link target", row.kind.letter())
            });
        }
        if row.path.is_empty() || row.link.as_deref() == Some("") {
            return Err("an empty path".to_string());
        }
        self.rows += 1;
        self.next_start = row.end;
        self.last_slice = row.slice;
        Ok(())
    }

    /// The `end` of the last row accepted (0 before any).
    pub fn end(&self) -> u64 {
        self.next_start
    }

    /// How many rows were accepted.
    pub fn rows(&self) -> u64 {
        self.rows
    }
}

fn bad(what: &str, detail: impl std::fmt::Display) -> TapectlError {
    TapectlError::Other(format!("file table: {what}: {detail}"))
}

/// Write a staging's table as the format's text, row by row: the two
/// header lines, then each row, every path through `volume::index::escape`.
/// Rows that break the [`Chain`], or end past `stream_bytes`, are refused
/// before they are written. Returns the rows written.
pub fn write<W: Write + ?Sized>(
    w: &mut W,
    stream_bytes: u64,
    rows: impl IntoIterator<Item = Result<Row>>,
) -> Result<u64> {
    writeln!(w, "{HEADER}")?;
    writeln!(w, "{STREAM_BYTES}{stream_bytes}")?;
    let mut chain = Chain::default();
    for row in rows {
        let row = row?;
        let n = chain.rows() + 1;
        chain
            .accept(&row)
            .map_err(|e| bad(&format!("row {n}"), e))?;
        if row.end > stream_bytes {
            return Err(bad(
                &format!("row {n}"),
                format!(
                    "`end` {} is past the stream's {stream_bytes} bytes",
                    row.end
                ),
            ));
        }
        let hash = match &row.sha256 {
            Some(h) => sha256_to_hex(h),
            None => "-".to_string(),
        };
        writeln!(
            w,
            "{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}",
            row.kind.letter(),
            row.slice,
            row.start,
            row.end,
            row.size,
            hash,
            escape(&row.path),
            escape(row.link.as_deref().unwrap_or("")),
        )?;
    }
    Ok(chain.rows())
}

/// A table's text read row by row: [`Reader::new`] reads the two header
/// lines, and the iterator gives each row in stream order, holding each to
/// the [`Chain`]. The first error ends it.
pub struct Reader<R> {
    inner: R,
    stream_bytes: u64,
    line: usize,
    chain: Chain,
    buf: Vec<u8>,
    done: bool,
}

/// One line without its LF, refused when it is not UTF-8 or the text ends
/// without one (a table cut short). `Ok(None)` at the end.
fn next_line<R: BufRead>(r: &mut R, buf: &mut Vec<u8>, line: usize) -> Result<Option<String>> {
    buf.clear();
    if r.read_until(b'\n', buf)? == 0 {
        return Ok(None);
    }
    if buf.pop() != Some(b'\n') {
        return Err(bad(&format!("line {line}"), "the text ends inside a line"));
    }
    String::from_utf8(std::mem::take(buf))
        .map(Some)
        .map_err(|_| bad(&format!("line {line}"), "not UTF-8"))
}

impl<R: BufRead> Reader<R> {
    /// Read and check the header.
    pub fn new(mut inner: R) -> Result<Self> {
        let mut buf = Vec::new();
        let first = next_line(&mut inner, &mut buf, 1)?;
        if first.as_deref() != Some(HEADER) {
            return Err(bad("line 1", format!("not `{HEADER}`: {first:?}")));
        }
        let second = next_line(&mut inner, &mut buf, 2)?.unwrap_or_default();
        let stream_bytes = second
            .strip_prefix(STREAM_BYTES)
            .and_then(|n| parse_number(n).ok())
            .ok_or_else(|| bad("line 2", format!("not `{STREAM_BYTES}<bytes>`: {second:?}")))?;
        Ok(Reader {
            inner,
            stream_bytes,
            line: 2,
            chain: Chain::default(),
            buf,
            done: false,
        })
    }

    /// The stream's length, from the header.
    pub fn stream_bytes(&self) -> u64 {
        self.stream_bytes
    }

    fn row(&mut self, text: &str) -> std::result::Result<Row, String> {
        let fields: Vec<&str> = text.split('\t').collect();
        if fields.len() != 8 {
            return Err(format!("{} field(s), not 8", fields.len()));
        }
        let kind =
            Kind::from_letter(fields[0]).ok_or_else(|| format!("unknown kind {:?}", fields[0]))?;
        let number = |name: &str, value: &str| {
            parse_number(value).map_err(|_| format!("`{name}` is not a number: {value:?}"))
        };
        let sha256 = match fields[5] {
            "-" => None,
            hex => Some(sha256_from_hex(hex).map_err(|e| e.to_string())?),
        };
        let path = unescape(fields[6]).map_err(|e| format!("`path`: {e}"))?;
        let link = unescape(fields[7]).map_err(|e| format!("`link target`: {e}"))?;
        let row = Row {
            kind,
            slice: number("slice", fields[1])?,
            start: number("start", fields[2])?,
            end: number("end", fields[3])?,
            size: number("size", fields[4])?,
            sha256,
            path,
            link: (!link.is_empty()).then_some(link),
        };
        self.chain.accept(&row)?;
        if row.end > self.stream_bytes {
            return Err(format!(
                "`end` {} is past the stream's {} bytes",
                row.end, self.stream_bytes
            ));
        }
        Ok(row)
    }
}

impl<R: BufRead> Iterator for Reader<R> {
    type Item = Result<Row>;

    fn next(&mut self) -> Option<Result<Row>> {
        if self.done {
            return None;
        }
        self.line += 1;
        let line = self.line;
        let text = match next_line(&mut self.inner, &mut self.buf, line) {
            Ok(Some(text)) => text,
            Ok(None) => {
                self.done = true;
                return None;
            }
            Err(e) => {
                self.done = true;
                return Some(Err(e));
            }
        };
        let row = self.row(&text).map_err(|e| bad(&format!("line {line}"), e));
        if row.is_err() {
            self.done = true;
        }
        Some(row)
    }
}

/// A decimal count as the writer spells it: digits only, no sign, no
/// leading zero but for 0 itself.
fn parse_number(text: &str) -> std::result::Result<u64, ()> {
    let canonical = !text.is_empty()
        && text.bytes().all(|b| b.is_ascii_digit())
        && (text == "0" || !text.starts_with('0'));
    if !canonical {
        return Err(());
    }
    text.parse().map_err(|_| ())
}

/// A whole table read into memory: its stream length and its rows.
pub fn read_all<R: BufRead>(r: R) -> Result<(u64, Vec<Row>)> {
    let reader = Reader::new(r)?;
    let stream_bytes = reader.stream_bytes();
    let rows = reader.collect::<Result<Vec<Row>>>()?;
    Ok((stream_bytes, rows))
}

/// Whether `path` is `wanted` or lies under it. `.` (the unit's root) holds
/// every path. The test is on `wanted/`, never on the bare prefix: `d` does
/// not hold `d-x` or `d.txt`.
fn under(path: &str, wanted: &HashSet<&str>) -> bool {
    if wanted.contains(".") || wanted.contains(path) {
        return true;
    }
    let mut at = path.len();
    while let Some(slash) = path[..at].rfind('/') {
        if wanted.contains(&path[..slash]) {
            return true;
        }
        at = slash;
    }
    false
}

/// A path as asked for: no trailing `/` (`d/` is `d`), no leading `./`.
fn asked(path: &str) -> &str {
    let path = path.strip_prefix("./").unwrap_or(path);
    let trimmed = path.trim_end_matches('/');
    if trimmed.is_empty() {
        "."
    } else {
        trimmed
    }
}

/// The members that hold `wanted` paths, in stream order: each wanted
/// path's own member, a wanted folder's contents (which follow later
/// members: a folder is its own member and does not lie beside them), and
/// for each hard link chosen, its target (the name archived first, so it
/// comes before the link). `.` is the whole unit.
pub fn select<'a>(rows: &'a [Row], wanted: &[&str]) -> Vec<&'a Row> {
    let wanted: HashSet<&str> = wanted.iter().map(|w| asked(w)).collect();
    let mut chosen: Vec<bool> = rows.iter().map(|r| under(&r.path, &wanted)).collect();
    let targets: HashSet<&str> = rows
        .iter()
        .zip(&chosen)
        .filter(|(r, &c)| c && r.kind == Kind::HardLink)
        .filter_map(|(r, _)| r.link.as_deref())
        .collect();
    if !targets.is_empty() {
        for (i, row) in rows.iter().enumerate() {
            if row.kind != Kind::HardLink && targets.contains(row.path.as_str()) {
                chosen[i] = true;
            }
        }
    }
    rows.iter()
        .zip(chosen)
        .filter_map(|(r, c)| c.then_some(r))
        .collect()
}

/// The byte ranges of the stream that hold `members` (in stream order),
/// joined at member boundaries: two members that touch are one range.
pub fn join<'a>(members: impl IntoIterator<Item = &'a Row>) -> Vec<Range<u64>> {
    let mut out: Vec<Range<u64>> = Vec::new();
    for row in members {
        match out.last_mut() {
            Some(last) if last.end == row.start => last.end = row.end,
            _ => out.push(row.start..row.end),
        }
    }
    out
}

/// The byte ranges that hold the wanted paths' members ([`select`]),
/// joined at member boundaries (item 264). A wanted folder is at least two
/// ranges when another member lies between it and its contents.
pub fn ranges(rows: &[Row], wanted: &[&str]) -> Vec<Range<u64>> {
    join(select(rows, wanted))
}

/// Every member, as one range (the stream without its tail).
pub fn whole(rows: &[Row]) -> Range<u64> {
    0..rows.last().map_or(0, |r| r.end)
}

/// The slices the given members need: each one's `slice`, and for a member
/// longer than `slice_size` every slice of its run (§3.3). A hard link's
/// target must be among the members for its slices to be counted, as
/// [`select`] gives it.
pub fn slices_of<'a>(rows: impl IntoIterator<Item = &'a Row>, slice_size: u64) -> BTreeSet<u64> {
    let mut out = BTreeSet::new();
    for row in rows {
        out.extend(row.slices(slice_size));
    }
    out
}

/// Slice `n`'s offset in the stream: the `start` of the first member that
/// begins in it, or, for a slice inside a long member's run, the run's
/// start plus a whole `slice_size` for each slice before it. `None` for a
/// slice that holds no member's bytes (one that holds only the stream's
/// tail, or none of this table's).
pub fn slice_start(rows: &[Row], slice_size: u64, n: u64) -> Option<u64> {
    let i = rows.partition_point(|r| r.slice < n);
    if let Some(row) = rows.get(i).filter(|r| r.slice == n) {
        return Some(row.start);
    }
    let before = rows.get(i.checked_sub(1)?)?;
    (before.last_slice(slice_size) >= n).then(|| before.start + (n - before.slice) * slice_size)
}

/// For each hard link, the index of its target's row; `None` when the
/// table names no such member.
fn link_targets(rows: &[Row]) -> Vec<Option<usize>> {
    let wanted: HashSet<&str> = rows.iter().filter_map(|r| r.link.as_deref()).collect();
    let mut at: HashMap<&str, usize> = HashMap::with_capacity(wanted.len());
    if !wanted.is_empty() {
        for (i, row) in rows.iter().enumerate() {
            if row.kind != Kind::HardLink && wanted.contains(row.path.as_str()) {
                at.entry(row.path.as_str()).or_insert(i);
            }
        }
    }
    rows.iter()
        .map(|r| r.link.as_deref().and_then(|l| at.get(l).copied()))
        .collect()
}

/// The members whose every slice is in `slices_present` (items 260, 266):
/// the files a set of slices holds whole. A hard link is held only when
/// its target is too: its bytes are its target's (item 293).
pub fn held_whole<'a>(
    rows: &'a [Row],
    slice_size: u64,
    slices_present: &BTreeSet<u64>,
) -> Vec<&'a Row> {
    let own: Vec<bool> = rows
        .iter()
        .map(|r| r.slices(slice_size).all(|s| slices_present.contains(&s)))
        .collect();
    let targets = link_targets(rows);
    rows.iter()
        .enumerate()
        .filter(|&(i, r)| {
            own[i] && (r.kind != Kind::HardLink || targets[i].is_some_and(|t| own[t]))
        })
        .map(|(_, r)| r)
        .collect()
}

/// The members a lost slice `n` costs: every member with bytes in it (one
/// that begins in it, or a long member whose run holds it), and every hard
/// link whose target is one of them (item 293: such a name is listed for
/// the repair). In stream order.
pub fn members_of(rows: &[Row], slice_size: u64, n: u64) -> Vec<&Row> {
    let hit: Vec<bool> = rows
        .iter()
        .map(|r| r.slices(slice_size).contains(&n))
        .collect();
    let targets = link_targets(rows);
    rows.iter()
        .enumerate()
        .filter(|&(i, _)| hit[i] || targets[i].is_some_and(|t| hit[t]))
        .map(|(_, r)| r)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const SLICE: u64 = 4096;

    /// Rows laid out as the cutter lays them (§3.3), each `(kind, path,
    /// length, link)`: a member that fits goes into the slice, one that does
    /// not begins the next, one longer than `SLICE` is a run.
    fn laid_out(members: &[(Kind, &str, u64, Option<&str>)]) -> Vec<Row> {
        let mut rows = Vec::new();
        let (mut slice, mut used, mut at) = (1u64, 0u64, 0u64);
        for &(kind, path, len, link) in members {
            if used > 0 && (len > SLICE || used + len > SLICE) {
                slice += 1;
                used = 0;
            }
            rows.push(Row {
                kind,
                slice,
                start: at,
                end: at + len,
                size: len.saturating_sub(512),
                sha256: kind.hashed().then(|| [path.len() as u8; 32]),
                path: path.to_string(),
                link: link.map(str::to_string),
            });
            at += len;
            if len > SLICE {
                slice += len.div_ceil(SLICE);
                used = 0;
            } else {
                used += len;
            }
        }
        rows
    }

    fn text_of(stream_bytes: u64, rows: &[Row]) -> String {
        let mut out = Vec::new();
        write(&mut out, stream_bytes, rows.iter().cloned().map(Ok)).unwrap();
        String::from_utf8(out).unwrap()
    }

    fn paths<'a>(rows: impl IntoIterator<Item = &'a Row>) -> Vec<&'a str> {
        rows.into_iter().map(|r| r.path.as_str()).collect()
    }

    #[test]
    fn the_text_round_trips_with_a_tab_a_newline_and_a_backslash() {
        let rows = laid_out(&[
            (Kind::Folder, ".", 512, None),
            (Kind::File, "a\tb", 1024, None),
            (Kind::File, "new\nline", 1024, None),
            (Kind::File, "back\\slash\r", 1536, None),
            (Kind::HardLink, "link\tto", 512, Some("a\tb")),
            (Kind::Symlink, "sym", 512, None),
            (Kind::Fifo, "p", 512, None),
            (Kind::CharDevice, "c", 512, None),
            (Kind::BlockDevice, "b", 512, None),
            (Kind::File, "big", 3 * SLICE + 100, None),
            (Kind::File, "after", 1024, None),
        ]);
        let end = rows.last().unwrap().end + 1024;
        let text = text_of(end, &rows);
        let mut lines = text.lines();
        assert_eq!(lines.next(), Some(HEADER));
        assert_eq!(lines.next(), Some(format!("{STREAM_BYTES}{end}").as_str()));
        // An escaped TAB never splits a field: every row has eight.
        assert!(text.lines().skip(2).all(|l| l.split('\t').count() == 8));
        assert!(text.contains("\ta\\tb\t\n"), "{text}");
        assert!(text.contains("\tnew\\nline\t\n"), "{text}");
        assert!(text.contains("\tback\\\\slash\\r\t\n"), "{text}");
        assert!(text.contains("\tlink\\tto\ta\\tb\n"), "{text}");
        assert!(text.starts_with(&format!(
            "{HEADER}\n{STREAM_BYTES}{end}\nd\t1\t0\t512\t0\t-\t.\t\n"
        )));

        let (stream_bytes, back) = read_all(text.as_bytes()).unwrap();
        assert_eq!(stream_bytes, end);
        assert_eq!(back, rows);
    }

    #[test]
    fn an_empty_table_is_two_header_lines() {
        let text = text_of(10240, &[]);
        assert_eq!(text, format!("{HEADER}\n{STREAM_BYTES}10240\n"));
        assert_eq!(read_all(text.as_bytes()).unwrap(), (10240, vec![]));
    }

    fn refused(text: &str) -> String {
        match read_all(text.as_bytes()) {
            Ok(rows) => panic!("{text:?} was read: {rows:?}"),
            Err(e) => e.to_string(),
        }
    }

    const H: &str = "#tapectl-file-table 1\n#stream_bytes 4096\n";
    const HASH: &str = "0000000000000000000000000000000000000000000000000000000000000000";

    #[test]
    fn the_parser_refuses_what_the_writer_never_makes() {
        let ok = format!("{H}f\t1\t0\t1024\t5\t{HASH}\ta\t\n");
        assert_eq!(read_all(ok.as_bytes()).unwrap().1.len(), 1);

        for (text, says) in [
            (
                "#tapectl-file-table 2\n#stream_bytes 1\n".to_string(),
                "line 1",
            ),
            ("#tapectl-file-table 1\n#stream 1\n".to_string(), "line 2"),
            (
                "#tapectl-file-table 1\n#stream_bytes 01\n".to_string(),
                "line 2",
            ),
            ("".to_string(), "line 1"),
            (format!("{H}x\t1\t0\t1024\t5\t-\ta\t\n"), "unknown kind"),
            (format!("{H}f\t1\t0\t1024\t5\tABC\ta\t\n"), "sha256"),
            (format!("{H}f\t1\t0\t1024\t5\t-\ta\t\n"), "no sha256"),
            (
                format!("{H}d\t1\t0\t512\t0\t{HASH}\ta\t\n"),
                "states a sha256",
            ),
            (
                format!("{H}f\t1\t0\t1024\t5\t{HASH}\ta\\x\t\n"),
                "unknown escape",
            ),
            (
                format!("{H}f\t1\t0\t1024\t5\t{HASH}\ta\\\t\n"),
                "backslash ends",
            ),
            (format!("{H}f\t1\t512\t1024\t5\t{HASH}\ta\t\n"), "not 0"),
            (
                format!("{H}f\t2\t0\t1024\t5\t{HASH}\ta\t\n"),
                "slice 2, not 1",
            ),
            (
                format!("{H}f\t1\t0\t1024\t5\t{HASH}\ta\t\nf\t1\t1536\t2048\t5\t{HASH}\tb\t\n"),
                "not the row before's `end`",
            ),
            (
                format!(
                    "{H}f\t1\t0\t1024\t5\t{HASH}\ta\t\nf\t2\t1024\t1536\t5\t{HASH}\tb\t\n\
                     f\t1\t1536\t2048\t5\t{HASH}\tc\t\n"
                ),
                "lower than the row before's",
            ),
            (
                format!("{H}f\t1\t0\t8192\t5\t{HASH}\ta\t\n"),
                "past the stream",
            ),
            (
                format!("{H}f\t1\t0\t0\t0\t{HASH}\ta\t\n"),
                "not past `start`",
            ),
            (
                format!("{H}h\t1\t0\t512\t0\t{HASH}\ta\t\n"),
                "names no link target",
            ),
            (
                format!("{H}f\t1\t0\t512\t0\t{HASH}\ta\tb\n"),
                "names a link target",
            ),
            (format!("{H}f\t1\t0\t512\t0\t{HASH}\ta\n"), "7 field(s)"),
            (format!("{H}f\t1\t0\t512\t0\t{HASH}\t\t\n"), "empty path"),
            (format!("{H}f\t1\t0\t512\t-1\t{HASH}\ta\t\n"), "`size`"),
            (
                format!("{H}f\t1\t0\t512\t0\t{HASH}\ta\t"),
                "ends inside a line",
            ),
        ] {
            let e = refused(&text);
            assert!(e.contains(says), "{text:?}: {e}");
        }
    }

    #[test]
    fn the_writer_refuses_a_row_that_breaks_the_chain() {
        let mut rows = laid_out(&[(Kind::File, "a", 1024, None), (Kind::File, "b", 1024, None)]);
        rows[1].start += 512;
        let mut out = Vec::new();
        let e = write(&mut out, 1 << 20, rows.into_iter().map(Ok)).unwrap_err();
        assert!(e.to_string().contains("row 2"), "{e}");
    }

    /// The measured shape (#562): `d`, `d-x`, `d.txt`, `d/a` in the sorted
    /// list's order. A folder is its own member, and its contents follow
    /// other members: two ranges.
    #[test]
    fn a_folder_is_two_ranges_and_its_prefix_is_not_its_contents() {
        let rows = laid_out(&[
            (Kind::Folder, "d", 512, None),
            (Kind::File, "d-x", 1024, None),
            (Kind::File, "d.txt", 1024, None),
            (Kind::File, "d/a", 1024, None),
        ]);
        assert_eq!(paths(select(&rows, &["d"])), ["d", "d/a"]);
        assert_eq!(ranges(&rows, &["d"]), vec![0..512, 2560..3584]);
        assert_eq!(ranges(&rows, &["d/"]), ranges(&rows, &["d"]));
        assert_eq!(
            ranges(&rows, &["d-x", "d.txt"]),
            vec![512..2560],
            "touching members join"
        );
        assert_eq!(ranges(&rows, &["d/a"]), vec![2560..3584]);
        assert_eq!(ranges(&rows, &["."]), vec![whole(&rows)]);
        assert_eq!(whole(&rows), 0..3584);
        assert!(ranges(&rows, &["nothing"]).is_empty());
    }

    #[test]
    fn a_hard_link_asked_alone_brings_its_target_first() {
        let rows = laid_out(&[
            (Kind::File, "first", 2048, None),
            (Kind::File, "other", 4096, None),
            (Kind::HardLink, "second", 512, Some("first")),
        ]);
        let chosen = select(&rows, &["second"]);
        assert_eq!(paths(chosen.iter().copied()), ["first", "second"]);
        assert_eq!(ranges(&rows, &["second"]), vec![0..2048, 6144..6656]);
        // `other` (slice 2) lies between them and is not needed.
        assert_eq!(slices_of(chosen, SLICE), BTreeSet::from([1, 3]));
    }

    #[test]
    fn a_long_members_run_is_its_slices_and_a_slice_inside_it_has_an_offset() {
        let rows = laid_out(&[
            (Kind::File, "small", 1024, None),
            (Kind::File, "long", 3 * SLICE + 512, None),
            (Kind::File, "next", 1024, None),
            (Kind::File, "last", 4096, None),
        ]);
        // `long` ends slice 1 before it and runs 2..=5; `next` begins 6.
        assert_eq!(rows[1].slice, 2);
        assert_eq!(rows[1].last_slice(SLICE), 5);
        assert_eq!(
            rows[2].slice, 6,
            "the cutter's next slice is the run's end + 1"
        );
        assert_eq!(slices_of([&rows[1]], SLICE), BTreeSet::from([2, 3, 4, 5]));
        assert_eq!(
            slices_of([&rows[0], &rows[2]], SLICE),
            BTreeSet::from([1, 6])
        );
        // The last member's run, which no later row's slice tells.
        assert_eq!(rows[3].slice, 7);
        assert_eq!(slices_of([&rows[3]], SLICE), BTreeSet::from([7]));

        assert_eq!(slice_start(&rows, SLICE, 1), Some(0));
        assert_eq!(slice_start(&rows, SLICE, 2), Some(1024));
        assert_eq!(slice_start(&rows, SLICE, 3), Some(1024 + SLICE));
        assert_eq!(slice_start(&rows, SLICE, 5), Some(1024 + 3 * SLICE));
        assert_eq!(slice_start(&rows, SLICE, 6), Some(rows[2].start));
        assert_eq!(
            slice_start(&rows, SLICE, 8),
            None,
            "only the tail, or nothing"
        );
        assert_eq!(slice_start(&[], SLICE, 1), None);
    }

    #[test]
    fn held_whole_leaves_out_a_run_with_one_piece_missing() {
        let rows = laid_out(&[
            (Kind::File, "a", 1024, None),
            (Kind::File, "long", 3 * SLICE, None),
            (Kind::File, "b", 1024, None),
        ]);
        // a: 1; long: 2..=4; b: 5.
        let all: BTreeSet<u64> = (1..=5).collect();
        assert_eq!(paths(held_whole(&rows, SLICE, &all)), ["a", "long", "b"]);
        let without_3: BTreeSet<u64> = all.iter().copied().filter(|&s| s != 3).collect();
        assert_eq!(paths(held_whole(&rows, SLICE, &without_3)), ["a", "b"]);
        let without_1: BTreeSet<u64> = (2..=5).collect();
        assert_eq!(paths(held_whole(&rows, SLICE, &without_1)), ["long", "b"]);
    }

    #[test]
    fn a_hard_link_is_held_only_with_its_target() {
        let rows = laid_out(&[
            (Kind::File, "first", 3072, None),
            (Kind::File, "fill", 2048, None),
            (Kind::HardLink, "second", 512, Some("first")),
        ]);
        // first: 1; fill, second: 2.
        assert_eq!(rows[2].slice, 2);
        assert_eq!(
            paths(held_whole(&rows, SLICE, &BTreeSet::from([2]))),
            ["fill"]
        );
        assert_eq!(paths(members_of(&rows, SLICE, 1)), ["first", "second"]);
    }

    #[test]
    fn members_of_a_slice_of_many_and_of_a_piece_of_a_run() {
        let rows = laid_out(&[
            (Kind::Folder, ".", 512, None),
            (Kind::File, "a", 1024, None),
            (Kind::File, "b", 1024, None),
            (Kind::File, "c", 1024, None),
            (Kind::File, "long", 2 * SLICE + 1024, None),
            (Kind::File, "z", 512, None),
        ]);
        assert_eq!(paths(members_of(&rows, SLICE, 1)), [".", "a", "b", "c"]);
        // long: 2..=4.
        for n in 2..=4 {
            assert_eq!(paths(members_of(&rows, SLICE, n)), ["long"], "slice {n}");
        }
        assert_eq!(paths(members_of(&rows, SLICE, 5)), ["z"]);
        assert!(members_of(&rows, SLICE, 6).is_empty());
    }

    /// A member of exactly `slice_size` fits an empty slice; one block more
    /// is a run (§3.3). The member's length decides, never its `size`.
    #[test]
    fn a_member_of_exactly_a_slice_is_no_run() {
        let rows = laid_out(&[
            (Kind::File, "exact", SLICE, None),
            (Kind::File, "over", SLICE + 512, None),
        ]);
        assert_eq!(rows[0].last_slice(SLICE), 1);
        assert!(!rows[0].is_run(SLICE));
        assert_eq!(rows[1].slices(SLICE), 2..=3);
        assert!(rows[1].is_run(SLICE));
    }
}
