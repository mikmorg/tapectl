//! Parsers for the plaintext front index (File 3), seal marker (File M), and
//! ID thunk (File 0) — the reader half of `layout.rs`'s `generate_front_index`
//! / `generate_seal_marker` / `generate_id_thunk_v2` writer half. Normative
//! grammar: `docs/design/volume-format-v3.md` §1.1, §1.2, §4 and §5 (Layout
//! Version 3); self-consistency rules: `docs/design/v2-open-questions.md`
//! §2.5, carried unchanged.
//!
//! All three documents share one shape: a human-readable header, then one or
//! more TOML tables (`[volume]`/`[layout]`/`[media]` for the ID thunk,
//! `[index]` for the front index, `[seal]` for the seal marker), the first
//! two of which also carry zero or more `[[files]]` entries in the
//! line-oriented grammar §3.1 fixes as a contract (so RESTORE.sh's grep/awk
//! parsing stays sound). Tape reads come block-padded with trailing zero
//! bytes, which these parsers strip before handing the body to the TOML
//! parser.

use serde::Deserialize;

use crate::error::{Result, TapectlError};

/// One `[[files]]` entry as parsed back from the front index or the seal
/// marker's embedded copy. Mirrors `layout::FrontIndexFile`, but owns its
/// `type_label` (a parsed value can't be `&'static str`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedIndexEntry {
    pub position: i32,
    pub type_label: String,
    pub size_bytes: Option<u64>,
    pub sha256_encrypted: Option<String>,
}

/// The parsed seal marker: its `[seal]` fields plus the embedded front-index
/// copy (`docs/design/volume-format-v3.md` §5).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedSeal {
    pub volume: String,
    pub layout_version: i64,
    pub file_count: i32,
    /// The position of the last file before this seal (`volume-format-v3.md`
    /// §5): `file_count - 2` on every seal that keeps rule S1. Every seal
    /// that parses states it (rule S2); `None` only on a seal returned
    /// uninterpreted because this tapectl must not read it
    /// ([`ParsedSeal::refusal`]).
    pub ended_after: Option<u64>,
    pub sealed_at: String,
    pub front_index_sha256: String,
    /// The features a reader must know to read this tape (ADR-0012
    /// amendment item 15). Empty when the seal states `requires = []`.
    pub requires: Vec<String>,
    /// Whether the seal states `requires` at all. Every seal this tapectl
    /// writes does; one that does not (every seal before 1.1.0) is refused
    /// ([`ParsedSeal::refusal`]; ADR-0012 item 258(d)).
    pub requires_stated: bool,
    pub files: Vec<ParsedIndexEntry>,
}

impl ParsedSeal {
    /// Why this reader must not read the tape this seal marker ends, if it
    /// must not: see [`FormatClaims::refusal`].
    pub fn refusal(&self) -> Option<String> {
        FormatClaims {
            magic: None,
            layout_version: Some(self.layout_version),
            requires: self.requires.clone(),
            requires_stated: self.requires_stated,
            tapectl_version: None,
            label: Some(self.volume.clone()),
        }
        .refusal("the seal marker")
    }

    /// The last file before this seal: its `ended_after` (rule S2: every
    /// seal that parses states one). `None` only on a seal returned
    /// uninterpreted ([`ParsedSeal::refusal`]). Layout 2's rule S3, a seal
    /// with no `ended_after` read as full, is gone with compatibility
    /// (`volume-format-v3.md` §5; ADR-0012 item 242).
    pub fn effective_ended_after(&self) -> Option<u64> {
        self.ended_after
    }

    /// §1.2 rule 6, "Which seal a reader accepts", for a seal found BEFORE
    /// the planned position, at `position`, the last file on the medium: one
    /// that keeps §5's parse rules (it parsed, so it did) and states
    /// `file_count = N` for the N files the tape holds (`position = N-1`).
    /// It is the normal short ending on every Layout 3 tape and needs no
    /// `requires` entry (D1). A seal at or after the planned position is the
    /// caller's to judge.
    pub fn is_short_seal_at(&self, position: u32) -> bool {
        self.refusal().is_none()
            && self.ended_after.is_some()
            && i64::from(self.file_count) == i64::from(position) + 1
    }

    /// Full, short, or at odds with the plan: [`classify_seal_ending`].
    pub fn ending(&self, planned_file_count: u64) -> SealEnding {
        classify_seal_ending(self, planned_file_count)
    }

    /// `volume-format-v3.md` §5's parse rules S1, S2, S4 and S5, the first
    /// one broken. [`parse_seal_marker`] applies them to every seal this
    /// tapectl may read. A seal that breaks one is unparseable (rule S6).
    pub fn check_parse_rules(&self) -> std::result::Result<(), SealRuleViolation> {
        // S2: every seal states where it ended.
        if self.ended_after.is_none() {
            return Err(SealRuleViolation::MissingEndedAfter);
        }
        // S1: ended_after + 2 == file_count, so the seal's own position
        // (file_count - 1) is ended_after + 1.
        if let Some(ended_after) = self.ended_after {
            if ended_after.checked_add(2) != u64::try_from(self.file_count).ok() {
                return Err(SealRuleViolation::EndedAfterNotFileCountLessTwo {
                    ended_after: i64::try_from(ended_after).unwrap_or(i64::MAX),
                    file_count: self.file_count,
                });
            }
        }
        // S4: exactly file_count entries, which keep the §2.5 checks.
        if i64::try_from(self.files.len()).ok() != Some(i64::from(self.file_count)) {
            return Err(SealRuleViolation::EntryCount {
                entries: self.files.len(),
                file_count: self.file_count,
            });
        }
        let violations = validate_consistency(&self.files);
        if !violations.is_empty() {
            return Err(SealRuleViolation::Inconsistent(violations));
        }
        // S5: the entry at ended_after is a data slice. S4 has put entry i
        // at position i, so the list index is the position.
        let ended_after = self.effective_ended_after();
        let found = ended_after
            .and_then(|k| usize::try_from(k).ok())
            .and_then(|k| self.files.get(k))
            .map(|e| e.type_label.clone());
        if found.as_deref() != Some("data_slice") {
            return Err(SealRuleViolation::EndedAfterNotDataSlice {
                ended_after: ended_after.unwrap_or(0),
                found,
            });
        }
        Ok(())
    }
}

/// The first of `volume-format-v3.md` §5's parse rules a seal breaks. A
/// seal that breaks one is unparseable (rule S6): by the fail-safe precedence
/// (`v2-open-questions.md` §2.5) that is the unsealed signal, never "sealed".
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SealRuleViolation {
    /// S1: `ended_after` is a non-negative integer and `ended_after + 2 ==
    /// file_count`.
    EndedAfterNotFileCountLessTwo { ended_after: i64, file_count: i32 },
    /// S2: every seal states `ended_after`.
    MissingEndedAfter,
    /// S4: the embedded copy has exactly `file_count` entries.
    EntryCount { entries: usize, file_count: i32 },
    /// S4: the embedded copy keeps the §2.5 self-consistency checks
    /// ([`validate_consistency`]); every violation found.
    Inconsistent(Vec<ConsistencyViolation>),
    /// S5: the embedded entry at `ended_after` is a `data_slice`. `found` is
    /// the type there, `None` when the copy has no entry at that position.
    EndedAfterNotDataSlice {
        ended_after: u64,
        found: Option<String>,
    },
}

impl std::fmt::Display for SealRuleViolation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SealRuleViolation::EndedAfterNotFileCountLessTwo {
                ended_after,
                file_count,
            } => write!(
                f,
                "rule S1: it states ended_after = {ended_after} and file_count = {file_count}; \
                 ended_after must be a non-negative integer two less than file_count"
            ),
            SealRuleViolation::MissingEndedAfter => {
                write!(f, "rule S2: it states no ended_after")
            }
            SealRuleViolation::EntryCount {
                entries,
                file_count,
            } => write!(
                f,
                "rule S4: its copy of the front index has {entries} entries, but it states \
                 file_count = {file_count}"
            ),
            SealRuleViolation::Inconsistent(violations) => write!(
                f,
                "rule S4: its copy of the front index is inconsistent: {}",
                violations
                    .iter()
                    .map(ToString::to_string)
                    .collect::<Vec<_>>()
                    .join("; ")
            ),
            SealRuleViolation::EndedAfterNotDataSlice { ended_after, found } => match found {
                Some(kind) => write!(
                    f,
                    "rule S5: the file before it (position {ended_after}) is a {kind}, not a \
                     data slice"
                ),
                None => write!(
                    f,
                    "rule S5: its copy of the front index has no entry at position \
                     {ended_after}, the file before it"
                ),
            },
        }
    }
}

/// Where a tape ended, by its seal's `file_count` against the plan's count
/// (File 0's `total_files`, File 3's entry count, or the Layout's):
/// `volume-format-v3.md` §5, "Classification".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SealEnding {
    /// The seal's `file_count` is the plan's: it sits at the planned
    /// position M and ended after M-1, the last data slice.
    Full,
    /// The seal's `file_count` is less than the plan's: the tape ended after
    /// `ended_after`, and every planned position past it is not on the tape
    /// (§5.1). Whether a reader accepts it is §1.2 rule 6's question, not
    /// this one's ([`ParsedSeal::is_short_seal_at`]).
    Short { ended_after: u64 },
    /// The seal's `file_count` is greater than the plan's (or states no file
    /// before the seal at all): the tape's two ends disagree, as a
    /// `front_index_sha256` mismatch does.
    Disagrees {
        seal_file_count: i32,
        planned_file_count: u64,
    },
}

/// Classify `seal` against the plan's file count (`volume-format-v3.md`
/// §5): equal is [`SealEnding::Full`], less is [`SealEnding::Short`],
/// greater is [`SealEnding::Disagrees`]. Pure: it reads nothing, and judges
/// nothing but the two counts. Run it on a seal that parsed, and so keeps
/// rules S1 to S5.
pub fn classify_seal_ending(seal: &ParsedSeal, planned_file_count: u64) -> SealEnding {
    let disagrees = SealEnding::Disagrees {
        seal_file_count: seal.file_count,
        planned_file_count,
    };
    let Ok(seal_file_count) = u64::try_from(seal.file_count) else {
        return disagrees;
    };
    match seal_file_count.cmp(&planned_file_count) {
        std::cmp::Ordering::Equal => SealEnding::Full,
        std::cmp::Ordering::Less => match seal.effective_ended_after() {
            Some(ended_after) => SealEnding::Short { ended_after },
            None => disagrees,
        },
        std::cmp::Ordering::Greater => disagrees,
    }
}

// --- serde-facing shapes -----------------------------------------------
//
// Every integer is deserialized as i64 (TOML's native integer width) and
// narrowed by an explicit `as` cast afterward, sidestepping any question of
// whether the `toml` crate's Deserializer narrows i64 -> i32/u32 on its own.

#[derive(Debug, Deserialize)]
struct FileEntryToml {
    position: i64,
    #[serde(rename = "type")]
    type_label: String,
    size_bytes: Option<i64>,
    sha256_encrypted: Option<String>,
}

impl From<FileEntryToml> for ParsedIndexEntry {
    fn from(t: FileEntryToml) -> Self {
        ParsedIndexEntry {
            position: t.position as i32,
            type_label: t.type_label,
            size_bytes: t.size_bytes.map(|v| v as u64),
            sha256_encrypted: t.sha256_encrypted,
        }
    }
}

#[derive(Debug, Deserialize)]
struct IndexHeader {
    volume: String,
    layout_version: i64,
    /// Every Layout 3 File 3 states it (§1.8). Absent means false.
    #[serde(default)]
    may_end_early: bool,
}

#[derive(Debug, Deserialize)]
struct FrontIndexDoc {
    index: IndexHeader,
    #[serde(default)]
    files: Vec<FileEntryToml>,
}

/// The front index (File 3) with its `[index]` table: what
/// [`parse_front_index_document`] reads. [`parse_front_index`] returns only
/// the `files`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedFrontIndex {
    pub volume: String,
    /// `[index] may_end_early = true` (`volume-format-v3.md` §4, §1.8): the
    /// tape may end before the last file listed, and its seal says where.
    /// False when absent. It decides nothing for a reader: the seal's
    /// `ended_after` does.
    pub may_end_early: bool,
    /// The planned files, in position order: on a tape that ended early,
    /// the ones after the seal's `ended_after` are not on it (§5.1).
    pub files: Vec<ParsedIndexEntry>,
}

#[derive(Debug, Deserialize)]
struct SealHeader {
    volume: String,
    layout_version: i64,
    file_count: i64,
    /// Every seal states it (rule S2). i64 on the wire so a negative value
    /// reaches rule S1 as a broken rule rather than as a TOML error.
    ended_after: Option<i64>,
    sealed_at: String,
    front_index_sha256: String,
    /// Absent is refused by [`ParsedSeal::refusal`], not here, so the seal
    /// still parses and every caller refuses it by name.
    requires: Option<Vec<String>>,
}

#[derive(Debug, Deserialize)]
struct SealDoc {
    seal: SealHeader,
    #[serde(default)]
    files: Vec<FileEntryToml>,
}

/// Strip trailing NUL bytes (tape reads come block-padded) and locate the
/// TOML body starting at `marker` (`"[index]"` or `"[seal]"`) — the same
/// convention `layout.rs`'s own generator tests use to isolate the
/// machine-readable half of the document from the human header above it.
fn toml_body<'a>(raw: &'a str, marker: &str, what: &str) -> Result<&'a str> {
    let stripped = raw.trim_end_matches('\0');
    let idx = stripped
        .find(marker)
        .ok_or_else(|| TapectlError::Other(format!("{what}: no {marker} marker found")))?;
    Ok(&stripped[idx..])
}

/// Parse the plaintext front index (File 3) into its ordered `[[files]]`
/// entries. Absent/malformed input is a normal `Err`, not a panic — callers
/// in the confirm chain walk (`docs/design/volume-format-v3.md` §6) treat a
/// parse failure as "front index unreadable," never crash on it.
pub fn parse_front_index(raw: &str) -> Result<Vec<ParsedIndexEntry>> {
    Ok(parse_front_index_document(raw)?.files)
}

/// [`parse_front_index`], with the `[index]` table's own keys: the volume
/// label and `may_end_early` (`volume-format-v3.md` §4). A `layout_version`
/// other than [`LAYOUT_VERSION`] is refused: File 3 carries no magic, so
/// the layout version is what moves (ADR-0012 item 294).
pub fn parse_front_index_document(raw: &str) -> Result<ParsedFrontIndex> {
    let body = toml_body(raw, "[index]", "front index")?;
    let doc: FrontIndexDoc = toml::from_str(body)
        .map_err(|e| TapectlError::Other(format!("front index: TOML parse failed: {e}")))?;
    if doc.index.layout_version != LAYOUT_VERSION {
        return Err(TapectlError::Other(format!(
            "front index: layout_version {} — this tapectl ({}) reads layout {LAYOUT_VERSION} only; \
             read the tape with the tapectl that wrote it (a File 3 of an older layout is not a \
             newer tape; File 0 names the writer) or with the RESTORE.sh at its file 2",
            doc.index.layout_version,
            env!("CARGO_PKG_VERSION"),
        )));
    }
    Ok(ParsedFrontIndex {
        volume: doc.index.volume,
        may_end_early: doc.index.may_end_early,
        files: doc.files.into_iter().map(Into::into).collect(),
    })
}

/// Parse the plaintext seal marker (the last file: File M, or K+1 on a tape
/// that ended early), including its embedded copy of the front index.
/// Absent/malformed input is a normal `Err` — per the fail-safe reader
/// precedence (`docs/design/v2-open-questions.md` §2.5), an unparseable seal
/// marker is treated exactly like an absent one: the tape reads as unsealed,
/// never as a crash.
///
/// In this order (`volume-format-v3.md` §1.2 rule 1, §5):
/// 1. A seal this tapectl must not read (a feature it does not know, another
///    layout: [`ParsedSeal::refusal`]) parses, uninterpreted, so that every
///    caller refuses it by name, and a write path's contact check still sees
///    a seal there.
/// 2. Every other seal keeps the parse rules S1, S2, S4 and S5
///    ([`ParsedSeal::check_parse_rules`]); one that breaks a rule, a seal
///    with no `ended_after` among them, is an `Err` naming it (rule S6).
pub fn parse_seal_marker(raw: &str) -> Result<ParsedSeal> {
    let body = toml_body(raw, "[seal]", "seal marker")?;
    let doc: SealDoc = toml::from_str(body)
        .map_err(|e| TapectlError::Other(format!("seal marker: TOML parse failed: {e}")))?;
    let stated_ended_after = doc.seal.ended_after;
    let seal = ParsedSeal {
        volume: doc.seal.volume,
        layout_version: doc.seal.layout_version,
        file_count: doc.seal.file_count as i32,
        ended_after: stated_ended_after.and_then(|k| u64::try_from(k).ok()),
        sealed_at: doc.seal.sealed_at,
        front_index_sha256: doc.seal.front_index_sha256,
        requires_stated: doc.seal.requires.is_some(),
        requires: doc.seal.requires.unwrap_or_default(),
        files: doc.files.into_iter().map(Into::into).collect(),
    };
    if seal.refusal().is_some() {
        return Ok(seal);
    }
    let broken = match stated_ended_after {
        // S1's "non-negative": a value u64 cannot hold is no ended_after.
        Some(k) if k < 0 => Some(SealRuleViolation::EndedAfterNotFileCountLessTwo {
            ended_after: k,
            file_count: seal.file_count,
        }),
        _ => seal.check_parse_rules().err(),
    };
    match broken {
        Some(rule) => Err(TapectlError::Other(format!("seal marker: {rule}"))),
        None => Ok(seal),
    }
}

/// `s` as a TOML basic string, quotes included: `\` and `"` escaped, and
/// every control character (DEL among them) as TOML escapes it
/// (`volume-format-v3.md` §1.1, #547: every string tapectl writes into a
/// plaintext TOML file or a MANIFEST.toml is one). Nothing else changes, so
/// a plain string is written as it is.
pub fn toml_string(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\u{8}' => out.push_str("\\b"),
            '\t' => out.push_str("\\t"),
            '\n' => out.push_str("\\n"),
            '\u{c}' => out.push_str("\\f"),
            '\r' => out.push_str("\\r"),
            c if c.is_control() => out.push_str(&format!("\\u{:04X}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// A tape file's true bytes: the block padding a tape read returns after
/// them (trailing NULs) removed, `volume-format-v3.md` §5.
pub fn without_nul_padding(raw: &[u8]) -> &[u8] {
    &raw[..raw.iter().rposition(|b| *b != 0).map_or(0, |i| i + 1)]
}

/// What a tape's seal marker proves, judged against the front index it ends
/// (issue #458; `volume-format-v3.md` §5). The ONE judgement: `catalog
/// rebuild --from-volume` asserts `sealed` only on [`SealVerdict::Sealed`]
/// (ADR-0012: "asserts `sealed` only after reading the seal marker"), and
/// `volume resume`'s adoption of a lost write (#360) asks the same question
/// of the same bytes, so the two can never disagree about what a seal is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SealVerdict {
    /// The seal marker read, parsed, is one this tapectl may read, and its
    /// `front_index_sha256` is the hash of the front index in hand.
    Sealed(ParsedSeal),
    /// No seal marker binds this front index; the string says which part
    /// failed. The tape is an unsealed one (an interrupted or aborted write)
    /// or its seal does not belong to the front index beside it.
    Unsealed(String),
    /// A seal marker that this tapectl must not interpret: it requires a
    /// feature, or a layout version, it does not know. Not "unsealed" — the
    /// tape may well be sealed, by a newer tapectl — so a reader that would
    /// act on it must refuse, as every other reader of the tape does.
    Refused(String),
}

/// Judge the seal marker of the tape whose front index (File 3, `front_index`
/// = its true bytes, padding removed) sits at `front_index_position`.
/// `seal_read` is what reading the file at `seal_position` (File 0's
/// `[layout] seal_marker`, or the last file on a tape that ended before it,
/// `volume-format-v3.md` §1.2 rule 6) gave: its bytes, or the reason it did
/// not read. `seal_position` is named in the reasons, and judged by nothing.
///
/// The check order, and the wording, are those `adopt_lost` has always
/// refused with: a seal that does not read, one that does not parse (from
/// 1.6.0 that includes one breaking §5's rules S1-S5, rule S6), one this
/// tapectl must not read, then one binding a different front index.
pub fn judge_seal(
    seal_read: std::result::Result<&[u8], &str>,
    front_index: &[u8],
    front_index_position: u32,
    seal_position: u32,
) -> SealVerdict {
    use sha2::Digest;
    let raw = match seal_read {
        Ok(raw) => raw,
        Err(why) => {
            return SealVerdict::Unsealed(format!(
                "no seal marker reads at file {seal_position}, where File 0 says it is: {why}"
            ))
        }
    };
    let seal = match parse_seal_marker(&String::from_utf8_lossy(raw)) {
        Ok(seal) => seal,
        Err(e) => {
            return SealVerdict::Unsealed(format!("file {seal_position} is not a seal marker: {e}"))
        }
    };
    if let Some(why) = seal.refusal() {
        return SealVerdict::Refused(why);
    }
    let front_index_sha256 = format!("{:x}", crate::pass_count::Sha256::digest(front_index));
    if seal.front_index_sha256 != front_index_sha256 {
        return SealVerdict::Unsealed(format!(
            "the seal marker binds a front index hashing to {}, but the front index (file \
             {front_index_position}) on this tape hashes to {front_index_sha256}",
            seal.front_index_sha256
        ));
    }
    SealVerdict::Sealed(seal)
}

/// The layout this tapectl reads and writes (File 0's, File 3's and the
/// seal's `layout_version`; D1, ADR-0012 item 242).
pub const LAYOUT_VERSION: i64 = 3;
/// File 0's `magic` for Layout 3 (D1). File 3 and the seal carry no magic
/// (ADR-0012 item 294): their `layout_version` is what moves.
pub const MAGIC: &str = "tapectl-volume-v3";

/// The features this tapectl knows how to read (`volume-format-v3.md` §1.2).
/// Empty: on every Layout 3 tape `requires` is empty (D1; ADR-0012 items
/// 258(a), 288). The short seal and a unit that continues on another
/// cartridge are part of Layout 3, not features of it. The key stays, so
/// that a later feature can fence an older Layout 3 reader: a tape whose
/// File 0 or seal lists any feature is refused by name.
pub const KNOWN_FEATURES: &[&str] = &[];

/// What a tape says about the reader it needs: File 0's `magic`,
/// `layout_version` and `requires` (the seal marker repeats the last two).
/// Every Rust reader of a tape checks [`FormatClaims::refusal`] before it
/// interprets anything else, so a tape a later tapectl wrote with a feature
/// this one does not know is refused, never misread. An absent `magic` or
/// `layout_version` is ABSENT, not refusable: a damaged File 0 may lack
/// them. An absent `requires` is refused: every File 0 and seal this
/// tapectl writes states it, and the tapes that did not (every tape before
/// 1.1.0) are not read (ADR-0012 item 258(d)).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FormatClaims {
    pub magic: Option<String>,
    pub layout_version: Option<i64>,
    pub requires: Vec<String>,
    /// Whether the document states `requires` at all (empty or not).
    pub requires_stated: bool,
    /// The writer's version, for the message only.
    pub tapectl_version: Option<String>,
    /// File 0's label, for the message only (§7.6 names it).
    pub label: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ClaimsToml {
    magic: Option<String>,
    layout_version: Option<i64>,
    requires: Option<Vec<String>>,
    tapectl_version: Option<String>,
    label: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ClaimsDoc {
    volume: ClaimsToml,
}

/// File 0's `[volume]` claims read line by line, without requiring the rest
/// of the document to be TOML (`volume-format-v3.md` §7.6; #587). A Layout 1
/// development tape, or a 1.x File 0 whose `[media]` holds an unescaped `"` or
/// `\` in a barcode, does not parse as a whole; its `[volume]` table still
/// says `magic`. Only `magic`, `layout_version`, `tapectl_version` and `label`
/// are read, and only plain `key = "value"` or `key = N` lines of that table.
fn lenient_volume_claims(raw: &str) -> FormatClaims {
    let stripped = raw.trim_end_matches('\0');
    let mut claims = FormatClaims::default();
    let mut in_volume = false;
    for line in stripped.lines() {
        let line = line.trim();
        if line.starts_with('[') {
            in_volume = line == "[volume]";
            continue;
        }
        if !in_volume {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let value = value.trim();
        let text = || {
            let v = value.strip_prefix('"')?;
            // The closing quote is the line's last: an unescaped one inside
            // does not end the value early.
            let end = v.rfind('"')?;
            Some(v[..end].to_string())
        };
        match key.trim() {
            "magic" => claims.magic = text(),
            "label" => claims.label = text(),
            "tapectl_version" => claims.tapectl_version = text(),
            "layout_version" => {
                claims.layout_version = value.split('#').next().and_then(|n| n.trim().parse().ok());
            }
            _ => {}
        }
    }
    claims
}

/// The one recognition of a tapectl volume of another layout, from File 0's
/// bytes (`volume-format-v3.md` §7.6; ADR-0012 item 269; #560, #587). Every
/// path that classifies File 0 asks this and nothing else.
///
/// `[volume] magic`'s `tapectl-volume-v` prefix is enough, read without
/// requiring the whole body to parse. A File 0 with no `magic` is recognised
/// only when its identity parses and it states a `layout_version` (a damaged
/// File 0 still says its layout, §1.2 rule 2); one that is not tapectl's
/// stays `None`. Our own layout is `None`.
pub fn recognise_other_layout(raw: &str) -> Option<OtherLayout> {
    let claims = parse_id_thunk_claims(raw).unwrap_or_else(|_| lenient_volume_claims(raw));
    if claims
        .magic
        .as_deref()
        .is_some_and(|m| m.starts_with(MAGIC_PREFIX))
        || parse_id_thunk_identity(raw).is_ok()
    {
        claims.other_layout()
    } else {
        None
    }
}

/// Parse File 0's `[volume]` claims. Unknown keys are ignored (they are
/// allowed by the extension rule); a `requires` that is not a list of
/// strings is an `Err`, which callers treat as a refusal.
pub fn parse_id_thunk_claims(raw: &str) -> Result<FormatClaims> {
    let body = toml_body(raw, "[volume]", "id thunk")?;
    let doc: ClaimsDoc = toml::from_str(body)
        .map_err(|e| TapectlError::Other(format!("id thunk: TOML parse failed: {e}")))?;
    Ok(FormatClaims {
        magic: doc.volume.magic,
        layout_version: doc.volume.layout_version,
        requires_stated: doc.volume.requires.is_some(),
        requires: doc.volume.requires.unwrap_or_default(),
        tapectl_version: doc.volume.tapectl_version,
        label: doc.volume.label,
    })
}

/// The prefix of every tapectl File 0's `magic` (`tapectl-volume-v<N>`).
const MAGIC_PREFIX: &str = "tapectl-volume-v";

/// A tapectl volume of a layout this tapectl does not read, as File 0 names
/// it (`volume-format-v3.md` §7.6; ADR-0012 item 269; #560). Read from File 0
/// and nothing else: no envelope, index or seal of that layout is parsed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OtherLayout {
    /// The layout number File 0 states (`layout_version`, else the number in
    /// `magic`): 1, 2, or any later one.
    pub layout: i64,
    pub label: Option<String>,
}

impl OtherLayout {
    /// The sentence every command that meets such a tape says (§7.6).
    pub fn sentence(&self) -> String {
        other_layout_sentence(self.layout, self.label.as_deref())
    }
}

/// "this is a tapectl volume of Layout N, label L; read it with the tapectl
/// that wrote it, or with the RESTORE.sh in its own file 2" (§7.6).
pub fn other_layout_sentence(layout: i64, label: Option<&str>) -> String {
    let label = match label {
        Some(l) => format!("label \"{l}\""),
        None => "no label".to_string(),
    };
    format!(
        "this is a tapectl volume of Layout {layout}, {label}; read it with the tapectl that \
         wrote it, or with the RESTORE.sh in its own file 2"
    )
}

impl FormatClaims {
    /// The layout number these claims state: `layout_version`, else the
    /// number a `tapectl-volume-v<N>` magic carries.
    fn layout_number(&self) -> Option<i64> {
        self.layout_version.or_else(|| {
            self.magic
                .as_deref()?
                .strip_prefix(MAGIC_PREFIX)?
                .parse()
                .ok()
        })
    }

    /// Whether these claims are a tapectl File 0 of a layout other than
    /// [`LAYOUT_VERSION`]: a `magic` that is tapectl's (or absent: a damaged
    /// File 0 still says its `layout_version`, §1.2 rule 2) and a layout
    /// number that is not ours. A File 0 that is not tapectl's, or states no
    /// layout, is `None`.
    pub fn other_layout(&self) -> Option<OtherLayout> {
        if let Some(m) = &self.magic {
            if !m.starts_with(MAGIC_PREFIX) {
                return None;
            }
        }
        let layout = self.layout_number()?;
        (layout != LAYOUT_VERSION).then(|| OtherLayout {
            layout,
            label: self.label.clone(),
        })
    }

    /// `None` when this tapectl can read a tape making these claims;
    /// otherwise the sentence that says why not, naming what it would need.
    pub fn refusal(&self, what: &str) -> Option<String> {
        let ours = env!("CARGO_PKG_VERSION");
        let writer = self
            .tapectl_version
            .as_deref()
            .map(|v| format!(" (it was written by tapectl {v})"))
            .unwrap_or_default();
        // §7.6: a tapectl volume of another layout is named as one, with its
        // label, and the way to a reader that knows it.
        if let Some(other) = self.other_layout() {
            let magic = match &self.magic {
                Some(m) => format!(", magic = \"{m}\""),
                None => String::new(),
            };
            return Some(format!(
                "{what} says layout_version = {}{magic}: {}{writer}. This tapectl ({ours}) \
                 reads layout {LAYOUT_VERSION} only and has read nothing else of it",
                other.layout,
                other.sentence(),
            ));
        }
        if let Some(m) = &self.magic {
            if m != MAGIC {
                return Some(format!(
                    "{what} says magic = \"{m}\", not \"{MAGIC}\": this tapectl ({ours}) \
                     cannot read it{writer}. Read it with a newer tapectl, or with the \
                     RESTORE.sh at the tape's own file 2"
                ));
            }
        }
        if let Some(v) = self.layout_version {
            if v != LAYOUT_VERSION {
                return Some(format!(
                    "{what} says layout_version = {v}: this tapectl ({ours}) reads layout \
                     {LAYOUT_VERSION} only{writer}. Read it with a newer tapectl, or with the \
                     RESTORE.sh at the tape's own file 2"
                ));
            }
        }
        if !self.requires_stated {
            return Some(format!(
                "{what} states no `requires`, which every tape this tapectl ({ours}) writes \
                 states (volume-format-v3.md §1.2){writer}: a damaged tape, or one of another \
                 layout, which this tapectl does not read. Read it with the RESTORE.sh at \
                 the tape's own file 2"
            ));
        }
        let unknown: Vec<&str> = self
            .requires
            .iter()
            .map(String::as_str)
            .filter(|f| !KNOWN_FEATURES.contains(f))
            .collect();
        if !unknown.is_empty() {
            return Some(format!(
                "this tape requires {} that this tapectl ({ours}) does not know ({what} \
                 states requires = {:?}){writer}: it needs a newer tapectl, or the RESTORE.sh \
                 at the tape's own file 2. Nothing has been read past it",
                unknown
                    .iter()
                    .map(|f| format!("\"{f}\""))
                    .collect::<Vec<_>>()
                    .join(", "),
                self.requires,
            ));
        }
        None
    }
}

/// §1.2 rule 2's warning: the keys File 0 does not state among `magic` and
/// `layout_version`, or `None` when it states both.
fn absent_claims_warning(claims: &FormatClaims) -> Option<String> {
    let mut absent = Vec::new();
    if claims.magic.is_none() {
        absent.push("`magic`");
    }
    if claims.layout_version.is_none() {
        absent.push("`layout_version`");
    }
    if absent.is_empty() {
        return None;
    }
    Some(format!(
        "File 0 states no {} (a damaged File 0): this tapectl reads on, and the seal's own \
         `layout_version` and `requires` decide (volume-format-v3.md §1.2 rule 2)",
        absent.join(" and ")
    ))
}

/// Refuse File 0 `raw` if it claims a format this tapectl cannot read. A
/// File 0 whose claims do not parse (say, a `requires` that is not a list)
/// is refused too: it cannot be shown to be readable.
pub fn check_id_thunk_readable(raw: &str) -> Result<()> {
    let claims = match parse_id_thunk_claims(raw) {
        Ok(claims) => claims,
        // A tapectl volume of another layout is named as one even when its
        // body is not this layout's TOML (§7.6; #587).
        Err(e) => match recognise_other_layout(raw) {
            Some(_) => lenient_volume_claims(raw),
            None => return Err(e),
        },
    };
    match claims.refusal("File 0") {
        Some(why) => Err(TapectlError::Other(why)),
        None => {
            // §1.2 rule 2: an absent `magic` or `layout_version` (a damaged
            // File 0) is warned about, not refused; the seal's own
            // `layout_version` and `requires` then decide.
            if let Some(w) = absent_claims_warning(&claims) {
                tracing::warn!("{w}");
            }
            Ok(())
        }
    }
}

/// The two identity fields of the ID thunk (File 0) that resume compares
/// against the Layout it is about to continue — `docs/design/layout-session.md`:
/// "rewind, read file 0, require ID-thunk identity match (label + uuid) —
/// mismatch = divergence = quarantine, not overwrite." Everything else in
/// the ID thunk (`layout::generate_id_thunk_v2`'s `magic`, `layout_version`,
/// `tapectl_version`, capacities, `created_at`, and the whole `[layout]`/
/// `[media]` tables) is either provisional or not part of this specific
/// check, so this struct deliberately carries only what resume needs. The
/// `[layout]` table's own pointers have a sibling parser,
/// [`parse_id_thunk_layout_pointers`], for the one other consumer that
/// needs them (a foreign tape's self-reported seal position, #27).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IdThunkIdentity {
    pub label: String,
    pub uuid: String,
}

#[derive(Debug, Deserialize)]
struct VolumeHeader {
    label: String,
    uuid: String,
}

#[derive(Debug, Deserialize)]
struct IdThunkDoc {
    volume: VolumeHeader,
}

/// Parse the ID thunk (File 0)'s `[volume]` identity. Absent/malformed input
/// is a normal `Err`, matching `parse_front_index`/`parse_seal_marker`'s
/// convention — a caller that cannot read or parse File 0 has NOT thereby
/// confirmed a mismatch (that would wrongly quarantine a session crashed
/// before File 0 ever landed); see `session::InterruptedSession::resume`'s
/// doc comment for how the two cases are told apart. The `[volume]` table
/// carries several other fields (`magic`, `layout_version`, capacities,
/// `created_at`, ...) and the document continues with `[layout]`/`[media]`
/// tables after it — all silently ignored here (no
/// `#[serde(deny_unknown_fields)]`), since this parser only ever needs
/// `label` + `uuid`.
pub fn parse_id_thunk_identity(raw: &str) -> Result<IdThunkIdentity> {
    let body = toml_body(raw, "[volume]", "id thunk")?;
    let doc: IdThunkDoc = toml::from_str(body)
        .map_err(|e| TapectlError::Other(format!("id thunk: TOML parse failed: {e}")))?;
    Ok(IdThunkIdentity {
        label: doc.volume.label,
        uuid: doc.volume.uuid,
    })
}

/// The ID thunk (File 0)'s `[layout]` pointers — `front_index` (always 3),
/// `seal_marker` (the last file — `layout::generate_id_thunk_v2` computes
/// this as `total_files - 1`), and `total_files`
/// (`docs/design/volume-format-v3.md` §1.1). A tape's own claim about its own
/// shape.
///
/// Used by the fresh-write contact check (`session::check_tape_contact`,
/// issue #27) to find a FOREIGN tape's real seal-marker position when its
/// File-0 identity does not match what the caller expected: the caller's own
/// `seal_position` argument is a position in the caller's own (not-yet-
/// written, or for `volume_init` nonexistent) layout, which has no
/// relationship to where a *different* tape's actual seal marker sits.
/// Without consulting the tape's own self-report, a foreign-but-sealed
/// cartridge would present as a plain identity mismatch — which the
/// `--force` override can defeat — silently permitting exactly the
/// overwrite-of-a-sealed-volume ADR-0003 forbids.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IdThunkLayoutPointers {
    pub front_index: i32,
    pub seal_marker: i32,
    pub total_files: i32,
}

#[derive(Debug, Deserialize)]
struct LayoutPointersToml {
    front_index: i64,
    seal_marker: i64,
    total_files: i64,
}

#[derive(Debug, Deserialize)]
struct IdThunkLayoutDoc {
    layout: LayoutPointersToml,
}

/// Parse the ID thunk (File 0)'s `[layout]` pointers. Same fail-safe
/// convention as every other parser here: absent/malformed input is a
/// normal `Err`, never a panic.
pub fn parse_id_thunk_layout_pointers(raw: &str) -> Result<IdThunkLayoutPointers> {
    let body = toml_body(raw, "[layout]", "id thunk layout")?;
    let doc: IdThunkLayoutDoc = toml::from_str(body)
        .map_err(|e| TapectlError::Other(format!("id thunk layout: TOML parse failed: {e}")))?;
    Ok(IdThunkLayoutPointers {
        front_index: doc.layout.front_index as i32,
        seal_marker: doc.layout.seal_marker as i32,
        total_files: doc.layout.total_files as i32,
    })
}

/// The ID thunk (File 0)'s `[volume]` media and capacity facts — everything a
/// `volumes` row needs that is not the label or the uuid.
///
/// A third parser over the same table rather than fields bolted onto
/// [`IdThunkIdentity`], which documents at length that it carries only what
/// resume needs. `catalog rebuild` (#136) is the only consumer: it
/// reconstructs a `volumes` row from the tape's own claim about itself
/// rather than from the local config, so a rebuild on a differently
/// configured machine records the capacity the tape was actually written
/// against.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IdThunkVolumeMeta {
    pub media_type: String,
    pub nominal_capacity_bytes: i64,
    pub mam_capacity_bytes: i64,
}

#[derive(Debug, Deserialize)]
struct VolumeMetaToml {
    media_type: String,
    nominal_capacity_bytes: i64,
    mam_capacity_bytes: i64,
}

#[derive(Debug, Deserialize)]
struct IdThunkVolumeMetaDoc {
    volume: VolumeMetaToml,
}

/// Parse the ID thunk (File 0)'s `[volume]` media and capacity fields. Same
/// fail-safe convention as its siblings: absent or malformed input is a
/// normal `Err`, never a panic.
pub fn parse_id_thunk_volume_meta(raw: &str) -> Result<IdThunkVolumeMeta> {
    let body = toml_body(raw, "[volume]", "id thunk volume meta")?;
    let doc: IdThunkVolumeMetaDoc = toml::from_str(body).map_err(|e| {
        TapectlError::Other(format!("id thunk volume meta: TOML parse failed: {e}"))
    })?;
    Ok(IdThunkVolumeMeta {
        media_type: doc.volume.media_type,
        nominal_capacity_bytes: doc.volume.nominal_capacity_bytes,
        mam_capacity_bytes: doc.volume.mam_capacity_bytes,
    })
}

/// The ID thunk (File 0)'s `[media]` cartridge identity — which physical
/// cartridge this volume was written to, and **how that was established**
/// (`docs/design/volume-format-v3.md` §1.1, ADR-0012).
///
/// A fourth parser over File 0 rather than fields bolted onto a sibling, for
/// the same reason `IdThunkVolumeMeta` is a third: each documents exactly
/// what its one consumer needs. This one answers a question no other parser
/// can — whether `cartridge_serial` is verifiable against the MEDIUM (load
/// the tape, read the MAM, compare) or only against the CATALOG.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IdThunkMedia {
    /// The cartridge's identity string. A chip-reported serial or an
    /// operator-typed barcode — `identity_source` is what says which.
    pub cartridge_serial: String,
    /// `Some("mam")`, `Some("operator")`, or **`None` = UNKNOWN**.
    ///
    /// `None` is not a defaulted `"mam"` and must never be treated as one.
    /// Every tape written before the field existed omits it, and those tapes
    /// stay readable forever; a reader that defaulted the absent case would
    /// make all of them falsely attest a chip-verified serial. A reader that
    /// cannot tell must say so.
    pub cartridge_identity_source: Option<String>,
    /// The cartridge manufacturer `volume init`/`volume write` recorded from
    /// MAM at write time (`layout::generate_id_thunk_v2`'s
    /// `cartridge_manufacturer`), when the tape carries one. `#[serde(default)]`
    /// (issue #165): every `[media]` table `volume init` has ever written
    /// carries this key, but a hand-assembled or future-shrunk thunk that
    /// omits it must still parse — additive, like every other field here.
    /// `catalog rebuild` (#165) is the one consumer: registering a cartridge
    /// from a tape's own claim needs the same fact `cartridge register
    /// --manufacturer` would ask an operator to type.
    pub cartridge_manufacturer: Option<String>,
    /// The cartridge's length in meters, same provenance and same
    /// `#[serde(default)]` reasoning as `cartridge_manufacturer` above.
    pub tape_length_meters: Option<i64>,
}

#[derive(Debug, Deserialize)]
struct MediaToml {
    cartridge_serial: String,
    /// `#[serde(default)]` spelled out rather than leaning on `Option`'s
    /// implicit one: the ABSENT case is the whole point of this field, and it
    /// must be impossible to read this and wonder what a missing key does.
    #[serde(default)]
    cartridge_identity_source: Option<String>,
    /// Additive (issue #165): a `[media]` table that predates this field, or
    /// one hand-assembled without it, must still parse rather than error.
    #[serde(default)]
    cartridge_manufacturer: Option<String>,
    #[serde(default)]
    tape_length_meters: Option<i64>,
}

#[derive(Debug, Deserialize)]
struct IdThunkMediaDoc {
    media: MediaToml,
}

/// Parse the ID thunk (File 0)'s `[media]` cartridge identity. Same fail-safe
/// convention as its three siblings: absent or malformed input is a normal
/// `Err`, never a panic.
pub fn parse_id_thunk_media(raw: &str) -> Result<IdThunkMedia> {
    let body = toml_body(raw, "[media]", "id thunk media")?;
    let doc: IdThunkMediaDoc = toml::from_str(body)
        .map_err(|e| TapectlError::Other(format!("id thunk media: TOML parse failed: {e}")))?;
    Ok(IdThunkMedia {
        cartridge_serial: doc.media.cartridge_serial,
        cartridge_identity_source: doc.media.cartridge_identity_source,
        cartridge_manufacturer: doc.media.cartridge_manufacturer,
        tape_length_meters: doc.media.tape_length_meters,
    })
}

/// File 0's two provenance facts (issue #305): when the write that made this
/// tape was planned (`[volume] created_at`) and the cartridge's MAM load
/// count at that moment (`[media] load_count_at_write`).
///
/// A fifth parser over File 0, for the same reason the other four are
/// separate: each carries what its consumer needs. `catalog rebuild` is the
/// consumer — a tape rebuilt into a new catalog is dated by its own bytes,
/// not by the day of the rebuild.
///
/// Both keys are optional. A thunk written before `[media]` existed (the
/// 2026-07-28 to 2026-09-13 window) has `created_at` and no load count; a
/// damaged or hand-made one may have neither. An absent key is `None`,
/// never a default.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct IdThunkProvenance {
    /// RFC 3339, verbatim. It is taken when `volume write` PLANS the write,
    /// before the tape moves, and a resumed write keeps the first attempt's
    /// (the Layout is rehydrated, never rebuilt). So it is when the write
    /// began, not when its first byte landed — and `sealed_at - created_at`
    /// spans any interruption: wall-clock provenance, never a throughput.
    pub created_at: Option<String>,
    /// The chip's load count when the write was planned. `0` is the
    /// writer's "unknown" (`volume-format-v3.md` §1.1) and reads as `None`:
    /// a tape being written has been loaded at least once, and a stored 0
    /// would be a confident wrong count (#184, migration 015).
    pub load_count_at_write: Option<i64>,
}

#[derive(Debug, Default, Deserialize)]
struct ProvenanceVolumeToml {
    #[serde(default)]
    created_at: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
struct ProvenanceMediaToml {
    #[serde(default)]
    load_count_at_write: Option<i64>,
}

#[derive(Debug, Deserialize)]
struct IdThunkProvenanceDoc {
    volume: ProvenanceVolumeToml,
    #[serde(default)]
    media: Option<ProvenanceMediaToml>,
}

/// Parse File 0's [`IdThunkProvenance`]. Same fail-safe convention as its
/// siblings: no `[volume]` table, or a body that is not TOML, is a normal
/// `Err`; a missing key is `None`.
pub fn parse_id_thunk_provenance(raw: &str) -> Result<IdThunkProvenance> {
    let body = toml_body(raw, "[volume]", "id thunk provenance")?;
    let doc: IdThunkProvenanceDoc = toml::from_str(body)
        .map_err(|e| TapectlError::Other(format!("id thunk provenance: TOML parse failed: {e}")))?;
    Ok(IdThunkProvenance {
        created_at: doc.volume.created_at,
        load_count_at_write: doc
            .media
            .and_then(|m| m.load_count_at_write)
            .filter(|n| *n > 0),
    })
}

/// An on-tape RFC 3339 timestamp (File 0's `created_at`, the seal marker's
/// `sealed_at`) in the catalog's own form, `YYYY-MM-DD HH:MM:SS` UTC — what
/// `datetime('now')` writes, so a rebuilt row's dates sort and compare with
/// every row the live write path wrote. `None` for text that is not
/// RFC 3339: a malformed date is an absence, never a guessed one.
pub fn catalog_timestamp(rfc3339: &str) -> Option<String> {
    chrono::DateTime::parse_from_rfc3339(rfc3339.trim())
        .ok()
        .map(|t| {
            t.with_timezone(&chrono::Utc)
                .format("%Y-%m-%d %H:%M:%S")
                .to_string()
        })
}

/// A violation of the §2.5 front-index self-consistency rules. Cheap checks
/// that turn "subtly wrong map" into a loud, structured report rather than a
/// silent bad read — every violation present is returned, not just the
/// first, matching `Layout::validate`'s report-don't-stop convention.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConsistencyViolation {
    /// The entry at list index `index` should be at position `expected`
    /// (positions must run 0, 1, 2, … with no gaps, dupes, or reordering)
    /// but instead claims `actual`.
    PositionOutOfSequence {
        index: usize,
        expected: i32,
        actual: i32,
    },
    /// Exactly one `front_index` entry is required.
    FrontIndexCount { found: usize },
    /// The (single) `front_index` entry must sit at position 3.
    FrontIndexNotAtThree { position: i32 },
    /// Exactly one `seal_marker` entry is required.
    SealMarkerCount { found: usize },
    /// The (single) `seal_marker` entry must be the last entry in the list.
    SealMarkerNotLast { position: i32, last_index: usize },
    /// Every entry except the front index's own and the seal marker's must
    /// carry `size_bytes` (format §3 — the heir's padding-trim contract; the
    /// two plaintext tail files are filemark-delimited and NUL-strip-parsed,
    /// so neither needs a trim size, and listing the seal's size in File 3
    /// while the seal embeds File 3's size would be a needless mutual-
    /// reference fixpoint). A map that omits a content file's size is
    /// hollow, not merely terse.
    MissingSize { position: i32 },
    /// Every entry except the front index's own and the seal marker's must
    /// carry `sha256_encrypted` (format §3 — the keyless integrity chain).
    MissingHash { position: i32 },
}

/// A violation in words: it reaches an operator as a `volume verify`
/// mismatch's "found" text and as a layout-validation error, where Rust's
/// `{:?}` (`PositionOutOfSequence { index: 2, .. }`) used to stand (issue
/// #357).
impl std::fmt::Display for ConsistencyViolation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ConsistencyViolation::PositionOutOfSequence {
                index,
                expected,
                actual,
            } => write!(
                f,
                "entry {index} claims position {actual}, expected {expected} \
                 (positions must run 0, 1, 2, … with no gaps)"
            ),
            ConsistencyViolation::FrontIndexCount { found } => {
                write!(f, "{found} front_index entries (exactly one is required)")
            }
            ConsistencyViolation::FrontIndexNotAtThree { position } => write!(
                f,
                "the front_index entry is at position {position}, not position 3"
            ),
            ConsistencyViolation::SealMarkerCount { found } => {
                write!(f, "{found} seal_marker entries (exactly one is required)")
            }
            ConsistencyViolation::SealMarkerNotLast {
                position,
                last_index,
            } => write!(
                f,
                "the seal_marker entry is at position {position}, not the last \
                 position ({last_index})"
            ),
            ConsistencyViolation::MissingSize { position } => {
                write!(f, "position {position} carries no size_bytes")
            }
            ConsistencyViolation::MissingHash { position } => {
                write!(f, "position {position} carries no sha256_encrypted")
            }
        }
    }
}

/// Run the §2.5 self-consistency checks over a parsed `[[files]]` list
/// (either the front index or the seal marker's embedded copy — same
/// shape). Returns every violation found; an empty vec means the list is
/// internally consistent (which is *not* the same as "matches the Layout" —
/// that cross-check is the chain walk's job).
pub fn validate_consistency(entries: &[ParsedIndexEntry]) -> Vec<ConsistencyViolation> {
    let mut violations = Vec::new();

    for (i, e) in entries.iter().enumerate() {
        if e.position != i as i32 {
            violations.push(ConsistencyViolation::PositionOutOfSequence {
                index: i,
                expected: i as i32,
                actual: e.position,
            });
        }
    }

    let front_indexes: Vec<&ParsedIndexEntry> = entries
        .iter()
        .filter(|e| e.type_label == "front_index")
        .collect();
    match front_indexes.len() {
        1 => {
            if front_indexes[0].position != 3 {
                violations.push(ConsistencyViolation::FrontIndexNotAtThree {
                    position: front_indexes[0].position,
                });
            }
        }
        found => violations.push(ConsistencyViolation::FrontIndexCount { found }),
    }

    let seal_markers: Vec<&ParsedIndexEntry> = entries
        .iter()
        .filter(|e| e.type_label == "seal_marker")
        .collect();
    match seal_markers.len() {
        1 => {
            let last_index = entries.len().saturating_sub(1);
            if seal_markers[0].position != last_index as i32 {
                violations.push(ConsistencyViolation::SealMarkerNotLast {
                    position: seal_markers[0].position,
                    last_index,
                });
            }
        }
        found => violations.push(ConsistencyViolation::SealMarkerCount { found }),
    }

    // Completeness (format §3 presence rules): a structurally ordered map
    // that omits a content file's size or hash would pass the shape checks
    // above yet be useless to the heir (no padding trim) or to the keyless
    // chain (no hash to verify). Flag every omission — except the two
    // self-referential exclusions the format defines.
    for e in entries {
        let is_front_index = e.type_label == "front_index";
        let is_seal_marker = e.type_label == "seal_marker";
        if !is_front_index && !is_seal_marker && e.size_bytes.is_none() {
            violations.push(ConsistencyViolation::MissingSize {
                position: e.position,
            });
        }
        if !is_front_index && !is_seal_marker && e.sha256_encrypted.is_none() {
            violations.push(ConsistencyViolation::MissingHash {
                position: e.position,
            });
        }
    }

    violations
}

/// Test support for the readers that refuse a tape by its File 0 claims
/// (#476): File 0 with the first `from` replaced by `to`, padded back to the
/// on-tape length it had. Asserts `from` is there — a rewrite that matched
/// nothing would make a refusal test pass or fail for the wrong reason, and
/// doubles as the positive control that the fixture's File 0 says what the
/// test assumes it says (`"\nrequires = []\n"` for every tape from 1.1.0).
#[cfg(test)]
pub(crate) fn rewrite_file0(file0: &[u8], from: &str, to: &str) -> Vec<u8> {
    let text = String::from_utf8_lossy(file0)
        .trim_end_matches('\0')
        .to_string();
    assert!(
        text.contains(from),
        "the fixture's File 0 lacks {from:?}:\n{text}"
    );
    let mut out = text.replacen(from, to, 1).into_bytes();
    assert!(out.len() <= file0.len(), "File 0 outgrew its block");
    out.resize(file0.len(), 0);
    out
}

#[cfg(test)]
mod forward_compat_tests {
    use super::*;
    use crate::volume::layout::{
        generate_id_thunk_v2, generate_seal_marker, FrontIndexFile, IdThunkV2Params,
    };

    fn thunk() -> String {
        generate_id_thunk_v2(&IdThunkV2Params {
            label: "FC01",
            uuid: "u",
            media_type: "LTO-6",
            tapectl_version: "1.1.0",
            nominal_capacity: 1,
            mam_capacity: 1,
            total_files: 6,
            mam_manufacturer: "m",
            mam_serial: "s",
            mam_length: 1,
            mam_loads: 1,
            created_at: "2026-10-06T00:00:00Z",
            cartridge_identity_source: None,
        })
    }

    /// A seal's embedded copy that keeps §5's rules S4 and S5 (Files 0-3,
    /// one data slice, the seal).
    fn seal_files() -> Vec<FrontIndexFile> {
        [
            "id_thunk",
            "system_guide",
            "restore_sh",
            "front_index",
            "data_slice",
            "seal_marker",
        ]
        .into_iter()
        .enumerate()
        .map(|(i, type_label)| FrontIndexFile {
            position: i as i32,
            type_label,
            size_bytes: (type_label != "seal_marker").then_some(10),
            sha256_encrypted: (type_label != "seal_marker").then(|| "h".to_string()),
        })
        .collect()
    }

    /// The full seal this tapectl writes over [`seal_files`]: `requires =
    /// []`, `ended_after = 4`.
    fn seal() -> String {
        generate_seal_marker("FC01", 6, "h", &seal_files())
    }

    /// `doc` (a File 0 or a seal) with its `requires = …` line replaced by
    /// `line`: these fixtures say what they require whatever the generator
    /// writes there.
    fn set_requires(doc: &str, line: &str) -> String {
        let mut done = false;
        let out: Vec<&str> = doc
            .split('\n')
            .map(|l| {
                if !done && l.starts_with("requires = ") {
                    done = true;
                    line
                } else {
                    l
                }
            })
            .collect();
        assert!(done, "the fixture states requires: {doc}");
        out.join("\n")
    }

    /// Issue #458: the one seal judgement. A seal that binds the front index
    /// in hand is `Sealed`; each way it can fail to is `Unsealed`, with the
    /// part that failed named; one this tapectl must not interpret is
    /// `Refused`, never `Unsealed` (it may be a newer tapectl's sealed tape).
    #[test]
    fn judge_seal_tells_sealed_from_unsealed_from_refused() {
        use sha2::Digest;
        let front_index = b"[index]\nvolume = \"FC01\"\nlayout_version = 3\n";
        let hash = format!("{:x}", sha2::Sha256::digest(front_index));
        let bound = generate_seal_marker("FC01", 6, &hash, &seal_files());
        // The tape returns block-padded reads; the judgement is on true bytes.
        let mut padded = bound.clone().into_bytes();
        padded.extend_from_slice(&[0; 40]);

        match judge_seal(Ok(&padded), front_index, 3, 5) {
            SealVerdict::Sealed(seal) => assert_eq!(seal.volume, "FC01"),
            other => panic!("a bound seal is Sealed: {other:?}"),
        }
        let SealVerdict::Unsealed(why) = judge_seal(Err("end of data"), front_index, 3, 5) else {
            panic!("an unread seal is Unsealed")
        };
        assert!(why.contains("no seal marker reads at file 5"), "{why}");
        assert!(why.contains("end of data"), "{why}");
        let SealVerdict::Unsealed(why) = judge_seal(Ok(b"junk"), front_index, 3, 5) else {
            panic!("an unparseable seal is Unsealed")
        };
        assert!(why.contains("file 5 is not a seal marker"), "{why}");
        let SealVerdict::Unsealed(why) = judge_seal(Ok(&padded), b"another index", 3, 5) else {
            panic!("a seal binding another front index is Unsealed")
        };
        assert!(why.contains("binds a front index hashing to"), "{why}");
        assert!(why.contains("front index (file 3)"), "{why}");
        let required = set_requires(&bound, "requires = [\"unknown-feature\"]");
        let SealVerdict::Refused(why) = judge_seal(Ok(required.as_bytes()), front_index, 3, 5)
        else {
            panic!("an unknown required feature is Refused, not Unsealed")
        };
        assert!(why.contains("\"unknown-feature\""), "{why}");
    }

    #[test]
    fn nul_padding_is_stripped_and_only_from_the_end() {
        assert_eq!(without_nul_padding(b"ab\0c\0\0\0"), b"ab\0c");
        assert_eq!(without_nul_padding(b"\0\0"), b"");
        assert_eq!(without_nul_padding(b""), b"");
    }

    /// D1 (`volume-format-v3.md` §1.2): every seal this tapectl writes states
    /// `requires = []` and `ended_after`, and this tapectl reads it.
    #[test]
    fn a_new_tape_requires_nothing_and_is_readable() {
        let s = parse_seal_marker(&seal()).unwrap();
        assert!(s.requires.is_empty() && s.requires_stated);
        assert_eq!(s.ended_after, Some(4));
        assert_eq!(s.refusal(), None);
        assert_eq!(s.check_parse_rules(), Ok(()), "the fixture keeps S4 and S5");
        assert!(thunk().contains("\nlayout_version = 3\nrequires = []\n"));
        check_id_thunk_readable(&thunk()).unwrap();
        assert!(KNOWN_FEATURES.is_empty());
    }

    /// Rule S2 (`volume-format-v3.md` §5): every seal states `ended_after`;
    /// one that does not (Layout 2's rule S3 read it as full) is unparseable,
    /// naming the rule (S6).
    #[test]
    fn a_seal_stating_no_ended_after_is_unparseable() {
        let without = seal().replace("ended_after = 4\n", "");
        assert_ne!(without, seal(), "the line was there to remove");
        let err = parse_seal_marker(&without).unwrap_err().to_string();
        assert!(err.contains("rule S2"), "{err}");
    }

    /// ADR-0012 item 258(d): every File 0 and seal this tapectl writes states
    /// `requires`; one that states none is refused, naming the field, by File
    /// 0's check and by the seal's own refusal. The seal still parses, so
    /// every caller refuses it by name.
    #[test]
    fn a_tape_without_requires_is_refused_naming_the_field() {
        let t = set_requires(&thunk(), "requires = []").replace("requires = []\n", "");
        assert!(!t.contains("requires"));
        let err = check_id_thunk_readable(&t).unwrap_err().to_string();
        assert!(err.contains("File 0 states no `requires`"), "{err}");
        let claims = parse_id_thunk_claims(&t).expect("the claims still parse");
        assert!(!claims.requires_stated);
        let old = seal().replace("requires = []\n", "");
        assert!(!old.contains("requires"));
        let s = parse_seal_marker(&old).expect("parses, to be refused by name");
        let why = s.refusal().expect("refused");
        assert!(
            why.contains("the seal marker states no `requires`"),
            "{why}"
        );
        // The positive control: `requires = []` is stated, and read.
        check_id_thunk_readable(&thunk()).unwrap();
        assert_eq!(parse_seal_marker(&seal()).unwrap().refusal(), None);
    }

    /// An unknown required feature is refused, by name, in File 0 and in
    /// the seal marker; the message says what would read it.
    #[test]
    fn an_unknown_required_feature_is_refused_by_name() {
        let t = set_requires(&thunk(), "requires = [\"unknown-feature\"]");
        let err = check_id_thunk_readable(&t).unwrap_err().to_string();
        assert!(err.contains("\"unknown-feature\""), "{err}");
        assert!(err.contains("newer tapectl"), "{err}");
        assert!(err.contains("tapectl 1.1.0"), "names the writer: {err}");

        let s = parse_seal_marker(&set_requires(&seal(), "requires = [\"unknown-feature\"]"))
            .expect("the seal still PARSES: a write-path contact check must see it as sealed");
        let why = s.refusal().expect("refused");
        assert!(why.contains("\"unknown-feature\""), "{why}");
    }

    /// D1 (ADR-0012 items 258(a), 288): `short-seal` and `spanning` are no
    /// longer known entries. Layout 3 states neither; a File 0 or a seal
    /// that requires one is refused by name, like any feature this tapectl
    /// does not know. Both names, in item 124's spelling, are refused
    /// together.
    #[test]
    fn short_seal_and_spanning_are_no_longer_known_features() {
        for feature in ["short-seal", "spanning"] {
            assert!(!KNOWN_FEATURES.contains(&feature));
            let line = format!("requires = [\"{feature}\"]");
            let err = check_id_thunk_readable(&set_requires(&thunk(), &line))
                .unwrap_err()
                .to_string();
            assert!(
                err.contains(&format!("requires \"{feature}\" that")),
                "{err}"
            );
            let why = parse_seal_marker(&set_requires(&seal(), &line))
                .unwrap()
                .refusal()
                .expect("refused");
            assert!(why.contains(&format!("\"{feature}\"")), "{why}");
        }
        let both = "requires = [\"short-seal\", \"spanning\"]";
        let err = check_id_thunk_readable(&set_requires(&thunk(), both))
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("requires \"short-seal\", \"spanning\" that"),
            "{err}"
        );
    }

    /// Keys this tapectl does not know are ignored unless `requires` names
    /// them (the extension rule, §1.2 rule 3), in File 0 and the seal marker.
    #[test]
    fn unknown_keys_that_are_not_required_are_ignored() {
        let t = set_requires(&thunk(), "requires = []\nfuture_key = \"x\"");
        assert!(t.contains("\nfuture_key = \"x\"\n"));
        check_id_thunk_readable(&t).unwrap();
        parse_id_thunk_identity(&t).unwrap();
        let s = parse_seal_marker(&set_requires(&seal(), "requires = []\nfuture_key = 7")).unwrap();
        assert_eq!(s.refusal(), None);
    }

    /// `volume-format-v3.md` §7.6 (item 269; #560): what is "a tapectl
    /// volume of another layout". Its claims, and the sentence every command
    /// says. Positive control: this tapectl's own File 0 is not one, and a
    /// document that is not tapectl's, or states no layout, is not one.
    /// #587: `magic`'s prefix alone recognises another layout, whatever the
    /// rest of File 0 is. A Layout 1 development tape (no uuid) and a 1.x
    /// File 0 whose barcode broke the TOML are both recognised, with their
    /// label; a File 0 that is not tapectl's, and our own, are not.
    #[test]
    fn another_layout_is_recognised_by_its_magic_whatever_the_body() {
        use foreign_layouts as f;
        let bad = f::layout_2_file_0_bad_barcode();
        assert!(parse_id_thunk_identity(&bad).is_err(), "premise: not TOML");
        assert!(parse_id_thunk_identity(f::LAYOUT_1_FILE_0).is_err());
        for (text, layout) in [(bad.as_str(), 2), (f::LAYOUT_1_FILE_0, 1)] {
            let other = recognise_other_layout(text).expect("another layout");
            assert_eq!(other.layout, layout);
            assert_eq!(other.label.as_deref(), Some("L6-0001"));
            let why = check_id_thunk_readable(text).unwrap_err().to_string();
            assert!(why.contains(&format!("volume of Layout {layout}")), "{why}");
        }
        // Positive controls: the whole-document cases, ours, and not tapectl's.
        assert_eq!(
            recognise_other_layout(f::LAYOUT_2_FILE_0).unwrap().layout,
            2
        );
        assert_eq!(recognise_other_layout(&thunk()), None);
        assert_eq!(recognise_other_layout("not a tapectl tape"), None);
        assert_eq!(
            recognise_other_layout("[volume]\nmagic = \"something-else\"\nlayout_version = 2\n["),
            None
        );
    }

    #[test]
    fn a_tapectl_volume_of_another_layout_is_named_with_its_label() {
        use super::foreign_layouts as f;
        let claims = |t: &str| parse_id_thunk_claims(t).unwrap();
        assert_eq!(claims(&thunk()).other_layout(), None, "our own File 0");
        for (n, text) in [
            (2, f::LAYOUT_2_FILE_0.to_string()),
            (4, f::layout_4_file_0()),
            (
                1,
                f::LAYOUT_2_FILE_0
                    .replace("v2", "v1")
                    .replace("layout_version = 2", "layout_version = 1"),
            ),
        ] {
            let other = claims(&text).other_layout().expect("another layout");
            assert_eq!(other.layout, n);
            assert_eq!(other.label.as_deref(), Some(f::LABEL));
            assert_eq!(
                other.sentence(),
                format!(
                    "this is a tapectl volume of Layout {n}, label \"L6-0001\"; read it with \
                     the tapectl that wrote it, or with the RESTORE.sh in its own file 2"
                )
            );
            let why = check_id_thunk_readable(&text).unwrap_err().to_string();
            assert!(why.contains(&other.sentence()), "{why}");
        }
        // The layout from the magic alone, when layout_version is damaged away.
        let t = f::LAYOUT_2_FILE_0.replace("layout_version = 2\n", "");
        assert_eq!(claims(&t).other_layout().unwrap().layout, 2);
        // Not tapectl's: another magic, whatever number it states.
        let t = f::LAYOUT_2_FILE_0.replace("tapectl-volume-v2", "some-other-format");
        assert_eq!(claims(&t).other_layout(), None);
        // No magic and no layout at all: nothing to name.
        let t = f::LAYOUT_2_FILE_0
            .replace("magic = \"tapectl-volume-v2\"\n", "")
            .replace("layout_version = 2\n", "");
        assert_eq!(claims(&t).other_layout(), None);
        // The seal's refusal names its volume.
        let seal = parse_seal_marker(f::LAYOUT_2_SEAL).unwrap();
        assert!(seal
            .refusal()
            .unwrap()
            .contains("tapectl volume of Layout 2, label \"L6-0001\""));
    }

    /// Another layout or magic is refused with a named error (#384; D1):
    /// a Layout 2 File 0 (`tapectl-volume-v2`, `layout_version = 2`), a
    /// Layout 2 front index and a Layout 2 seal. The positive control: this
    /// tapectl's own Layout 3 documents read.
    #[test]
    fn another_layout_or_magic_is_refused() {
        assert!(thunk().contains("\nmagic = \"tapectl-volume-v3\"\n"));
        check_id_thunk_readable(&thunk()).unwrap();
        let t = thunk().replace("layout_version = 3", "layout_version = 2");
        let err = check_id_thunk_readable(&t).unwrap_err().to_string();
        assert!(err.contains("layout_version = 2"), "{err}");
        let t = thunk().replace("tapectl-volume-v3", "tapectl-volume-v2");
        let err = check_id_thunk_readable(&t).unwrap_err().to_string();
        assert!(err.contains("tapectl-volume-v2"), "{err}");
        let fi = "[index]\nvolume = \"X\"\nlayout_version = 2\n";
        let err = parse_front_index(fi).unwrap_err().to_string();
        assert!(err.contains("layout_version 2"), "{err}");
        parse_front_index("[index]\nvolume = \"X\"\nlayout_version = 3\n").unwrap();
        let s =
            parse_seal_marker(&seal().replace("layout_version = 3", "layout_version = 2")).unwrap();
        assert!(s.refusal().unwrap().contains("layout_version = 2"));
    }

    /// #593 3-4 (`volume-format-v3.md` §1.2 rule 2): a File 0 stating neither
    /// `magic` nor `layout_version` is warned about, not refused, by the one
    /// check every reader makes. A positive control: a whole File 0 is quiet.
    #[test]
    fn a_file_0_with_no_magic_or_layout_version_is_warned_about_not_refused() {
        let damaged = thunk()
            .replace("magic = \"tapectl-volume-v3\"\n", "")
            .replace("layout_version = 3\n", "");
        let (read, heard) = crate::progress::heard::during(|| check_id_thunk_readable(&damaged));
        read.expect("warned about, not refused");
        assert_eq!(heard.count("states no `magic`").0, 1, "{:?}", heard.stderr);
        let (read, heard) = crate::progress::heard::during(|| check_id_thunk_readable(&thunk()));
        read.unwrap();
        assert!(heard.stderr.is_empty(), "{:?}", heard.stderr);
    }

    /// #593 (the remedy string at `format.rs`): a File 3 of an OLDER layout is
    /// not helped by "a newer tapectl"; the remedy names the tapectl that
    /// wrote it (`volume-format-v3.md` §1.2 rule 1).
    #[test]
    fn an_older_file_3_is_not_told_to_find_a_newer_tapectl() {
        let older = "[index]\nvolume = \"X\"\nlayout_version = 2\n";
        let err = parse_front_index(older).unwrap_err().to_string();
        assert!(!err.contains("newer tapectl"), "{err}");
        assert!(err.contains("the tapectl that wrote it"), "{err}");
        assert!(err.contains("RESTORE.sh at its file 2"), "{err}");
    }
}

/// tapectl 1.6.0's seal and front-index grammar on the read side
/// (`volume-format-v3.md` §4, §5; ADR-0012 items 110-111, #420). The seals
/// here are written from §5's grammar as literals, not by
/// `layout::generate_seal_marker`, so they pin the spec's bytes rather than
/// whatever the generator emits.
#[cfg(test)]
mod short_seal_tests {
    use super::*;

    /// File 3's bytes in these fixtures, and the hash every seal here binds.
    const FRONT_INDEX: &[u8] = b"[index]\nvolume = \"SS01\"\nlayout_version = 3\n";
    /// The `requires` every Layout 3 seal states (D1).
    const NONE: &str = "[]";

    fn front_index_sha256() -> String {
        use sha2::Digest;
        format!("{:x}", sha2::Sha256::digest(FRONT_INDEX))
    }

    /// One `[[files]]` entry: position, type, and whether it carries
    /// `size_bytes` and `sha256_encrypted` (every entry of a seal's copy
    /// does, File 3's own included, except the seal's own).
    type Entry = (i64, &'static str, bool);

    /// The plan of a tape with `slices` data slices: Files 0-3, a tenant
    /// envelope, the operator pair, the tenant backup, the slices from 8,
    /// and the seal at M = 8 + slices — as a seal's embedded copy states it.
    fn plan(slices: i64) -> Vec<Entry> {
        let mut kinds = vec![
            "id_thunk",
            "system_guide",
            "restore_sh",
            "front_index",
            "tenant_envelope",
            "operator_envelope",
            "operator_envelope_backup",
            "tenant_envelope_backup",
        ];
        kinds.extend(std::iter::repeat_n("data_slice", slices as usize));
        kinds.push("seal_marker");
        kinds
            .into_iter()
            .enumerate()
            .map(|(i, k)| (i as i64, k, k != "seal_marker"))
            .collect()
    }

    /// A short seal's copy: the plan's entries 0 to `k`, then its own, bare,
    /// at `k + 1` (§5).
    fn short_entries(slices: i64, k: i64) -> Vec<Entry> {
        let mut entries: Vec<Entry> = plan(slices).into_iter().take(k as usize + 1).collect();
        entries.push((k + 1, "seal_marker", false));
        entries
    }

    /// A seal marker as its `[seal]` keys and `[[files]]` copy state it.
    struct Fixture {
        /// The `requires` line's value; `None` writes no line (before 1.1.0).
        requires: Option<&'static str>,
        file_count: i64,
        ended_after: Option<i64>,
        entries: Vec<Entry>,
    }

    impl Fixture {
        /// A full 1.6.0 seal over `plan(slices)`: at M, `ended_after = M-1`.
        fn full(slices: i64) -> Fixture {
            let entries = plan(slices);
            let m = entries.len() as i64 - 1;
            Fixture {
                requires: Some(NONE),
                file_count: m + 1,
                ended_after: Some(m - 1),
                entries,
            }
        }

        /// A short 1.6.0 seal at `k + 1` over `plan(slices)`.
        fn short(slices: i64, k: i64) -> Fixture {
            Fixture {
                requires: Some(NONE),
                file_count: k + 2,
                ended_after: Some(k),
                entries: short_entries(slices, k),
            }
        }

        /// The seal's bytes, in §5's grammar and key order.
        fn text(&self) -> String {
            let mut s = String::from(
                "================================================================\n\
                 TAPECTL SEAL MARKER\n\
                 ================================================================\n\n\
                 Volume: SS01\n\n\
                 [seal]\n\
                 volume = \"SS01\"\n\
                 layout_version = 3\n",
            );
            if let Some(requires) = self.requires {
                s.push_str(&format!("requires = {requires}\n"));
            }
            s.push_str(&format!("file_count = {}\n", self.file_count));
            if let Some(k) = self.ended_after {
                s.push_str(&format!("ended_after = {k}\n"));
            }
            s.push_str("sealed_at = \"2026-10-20T00:00:00Z\"\n");
            s.push_str(&format!(
                "front_index_sha256 = \"{}\"\n",
                front_index_sha256()
            ));
            for (position, kind, sized) in &self.entries {
                s.push_str(&format!(
                    "\n[[files]]\nposition = {position}\ntype = \"{kind}\"\n"
                ));
                if *sized {
                    s.push_str(&format!(
                        "size_bytes = {}\nsha256_encrypted = \"{:064x}\"\n",
                        1000 + position,
                        position
                    ));
                }
            }
            s
        }

        /// The same seal as [`parse_seal_marker`] reads its fields, without
        /// its rules: what [`ParsedSeal::check_parse_rules`] judges.
        fn fields(&self) -> ParsedSeal {
            ParsedSeal {
                volume: "SS01".into(),
                layout_version: 3,
                file_count: self.file_count as i32,
                ended_after: self.ended_after.map(|k| k as u64),
                sealed_at: "2026-10-20T00:00:00Z".into(),
                front_index_sha256: front_index_sha256(),
                requires: Vec::new(),
                requires_stated: self.requires.is_some(),
                files: self
                    .entries
                    .iter()
                    .map(|(position, kind, sized)| ParsedIndexEntry {
                        position: *position as i32,
                        type_label: kind.to_string(),
                        size_bytes: sized.then_some(1000 + *position as u64),
                        sha256_encrypted: sized.then(|| format!("{position:064x}")),
                    })
                    .collect(),
            }
        }

        /// It must not parse, and the error names `rule` (rule S6).
        fn refused_naming(&self, rule: &str) -> String {
            let err = parse_seal_marker(&self.text())
                .expect_err("a seal breaking a parse rule is unparseable (S6)")
                .to_string();
            assert!(err.contains(rule), "names {rule}: {err}");
            assert!(err.starts_with("seal marker: "), "{err}");
            err
        }
    }

    /// The positive control for every rule: a full 1.6.0 seal parses, states
    /// `ended_after = M-1`, and its fields are what the fixture says.
    #[test]
    fn a_full_1_6_0_seal_parses_with_ended_after() {
        let f = Fixture::full(4);
        let seal = parse_seal_marker(&f.text()).expect("a full 1.6.0 seal parses");
        assert_eq!(
            seal,
            f.fields(),
            "the fixture's fields are what parse reads"
        );
        assert_eq!(seal.file_count, 13);
        assert_eq!(seal.ended_after, Some(11));
        assert_eq!(seal.effective_ended_after(), Some(11));
        assert!(seal.requires.is_empty(), "D1");
        assert_eq!(seal.refusal(), None);
        assert_eq!(seal.check_parse_rules(), Ok(()));
        assert_eq!(seal.ending(13), SealEnding::Full);
    }

    /// §5's short seal: entries 0 to K and its own at K+1, `file_count =
    /// K+2`, `ended_after = K`. It parses, padded as a tape read returns it,
    /// at the first data slice, in the middle, and at M-2.
    #[test]
    fn a_short_seal_parses_and_classifies_short() {
        let f = Fixture::short(4, 9);
        let mut padded = f.text().into_bytes();
        padded.resize(padded.len() + 4096, 0);
        let seal = parse_seal_marker(&String::from_utf8(padded).unwrap())
            .expect("a short seal parses, padding and all");
        assert_eq!(seal, f.fields());
        assert_eq!(seal.file_count, 11);
        assert_eq!(seal.ended_after, Some(9));
        assert_eq!(seal.files.len(), 11);
        let own = seal.files.last().unwrap();
        assert_eq!((own.position, own.type_label.as_str()), (10, "seal_marker"));
        assert_eq!(
            (own.size_bytes.as_ref(), own.sha256_encrypted.as_ref()),
            (None, None)
        );
        assert_eq!(seal.ending(13), SealEnding::Short { ended_after: 9 });

        for k in [8, 10] {
            let seal = parse_seal_marker(&Fixture::short(4, k).text())
                .unwrap_or_else(|e| panic!("K = {k}: {e}"));
            assert_eq!(
                seal.ending(13),
                SealEnding::Short {
                    ended_after: k as u64
                }
            );
        }
    }

    /// Rule S1: `ended_after + 2 == file_count`, and `ended_after` is not
    /// negative. One too few, one too many, and a negative value all fail.
    #[test]
    fn s1_ended_after_is_two_less_than_file_count() {
        for (k, file_count) in [(10, 13), (12, 13), (9, 13), (9, 10)] {
            let f = Fixture {
                ended_after: Some(k),
                file_count,
                ..Fixture::full(4)
            };
            f.refused_naming("rule S1");
            assert!(
                matches!(
                    f.fields().check_parse_rules(),
                    Err(SealRuleViolation::EndedAfterNotFileCountLessTwo { .. })
                ),
                "k = {k}, file_count = {file_count}"
            );
        }
        let err = Fixture {
            ended_after: Some(-1),
            ..Fixture::full(4)
        }
        .refused_naming("rule S1");
        assert!(err.contains("ended_after = -1"), "{err}");
    }

    /// Rule S2, second half: a seal that states `ended_after` keeps S1
    /// whatever its `requires` says, `requires = []` included. When it keeps
    /// the rules, it parses. (A seal with no `requires` line at all is
    /// refused before any rule: `a_tape_without_requires_is_refused_naming_the_field`.)
    #[test]
    fn s1_holds_whatever_requires_says() {
        let requires = Some("[]");
        let good = Fixture {
            requires,
            ..Fixture::full(4)
        };
        let seal = parse_seal_marker(&good.text()).expect("S1 holds: it parses");
        assert_eq!(seal.ended_after, Some(11));

        Fixture {
            requires,
            ended_after: Some(7),
            ..Fixture::full(4)
        }
        .refused_naming("rule S1");
    }

    /// Rule S2: every seal states `ended_after` (Layout 2's S3, a seal with
    /// none read as full, is gone, `volume-format-v3.md` §5).
    #[test]
    fn s2_every_seal_states_ended_after() {
        let f = Fixture {
            ended_after: None,
            ..Fixture::full(4)
        };
        f.refused_naming("rule S2");
        assert_eq!(
            f.fields().check_parse_rules(),
            Err(SealRuleViolation::MissingEndedAfter)
        );
    }

    /// Rule S4: the embedded copy has exactly `file_count` entries.
    #[test]
    fn s4_the_copy_has_exactly_file_count_entries() {
        let mut fewer = Fixture::short(4, 9);
        fewer.entries.remove(9);
        fewer.refused_naming("rule S4");
        assert_eq!(
            fewer.fields().check_parse_rules(),
            Err(SealRuleViolation::EntryCount {
                entries: 10,
                file_count: 11
            })
        );

        let mut more = Fixture::full(4);
        more.entries.push((13, "data_slice", true));
        let err = more.refused_naming("rule S4");
        assert!(err.contains("14 entries"), "{err}");
    }

    /// Rule S4: the copy keeps the §2.5 self-consistency checks
    /// ([`validate_consistency`]) — File 3 at position 3, the seal entry
    /// last, positions in order, every content file's size and hash.
    #[test]
    fn s4_the_copy_keeps_the_consistency_checks() {
        let mut moved = Fixture::short(4, 9);
        moved.entries[2].1 = "front_index";
        moved.entries[3].1 = "restore_sh";
        moved.refused_naming("rule S4");
        assert!(matches!(
            moved.fields().check_parse_rules(),
            Err(SealRuleViolation::Inconsistent(v))
                if v.contains(&ConsistencyViolation::FrontIndexNotAtThree { position: 2 })
        ));

        let mut not_last = Fixture::full(4);
        not_last.entries.swap(11, 12);
        not_last.entries[11].0 = 11;
        not_last.entries[12].0 = 12;
        not_last.refused_naming("rule S4");

        let mut hollow = Fixture::short(4, 9);
        hollow.entries[8].2 = false;
        let err = hollow.refused_naming("rule S4");
        assert!(err.contains("position 8 carries no size_bytes"), "{err}");
        assert!(matches!(
            hollow.fields().check_parse_rules(),
            Err(SealRuleViolation::Inconsistent(v))
                if v.contains(&ConsistencyViolation::MissingHash { position: 8 })
        ));

        let mut gap = Fixture::short(4, 9);
        gap.entries[5].0 = 6;
        gap.refused_naming("rule S4");
    }

    /// Rule S5: the entry at `ended_after` is a data slice. A short seal
    /// after an envelope or after the front index fails it, and so does a
    /// full seal whose plan has no data slice; a short seal right after the
    /// first data slice keeps it.
    #[test]
    fn s5_the_file_before_the_seal_is_a_data_slice() {
        for k in [7, 5, 4, 3] {
            let f = Fixture::short(4, k);
            let err = f.refused_naming("rule S5");
            assert!(err.contains(&format!("position {k}")), "{err}");
            assert_eq!(
                f.fields().check_parse_rules(),
                Err(SealRuleViolation::EndedAfterNotDataSlice {
                    ended_after: k as u64,
                    found: Some(plan(4)[k as usize].1.to_string()),
                })
            );
        }
        Fixture::full(0).refused_naming("rule S5");
        parse_seal_marker(&Fixture::short(4, 8).text()).expect("K = the first data slice");
    }

    /// Rule S6, through the one seal judgement: a 1.6.0 seal that breaks a
    /// rule is `Unsealed`, naming the rule, even though it binds the front
    /// index in hand; one that keeps them is `Sealed`, with its
    /// `ended_after`.
    #[test]
    fn s6_a_seal_breaking_a_rule_is_unsealed_never_sealed() {
        let good = Fixture::short(4, 9).text();
        match judge_seal(Ok(good.as_bytes()), FRONT_INDEX, 3, 10) {
            SealVerdict::Sealed(seal) => assert_eq!(seal.ended_after, Some(9)),
            other => panic!("a short seal that keeps the rules is Sealed: {other:?}"),
        }
        let broken = Fixture {
            ended_after: Some(8),
            ..Fixture::short(4, 9)
        }
        .text();
        let SealVerdict::Unsealed(why) = judge_seal(Ok(broken.as_bytes()), FRONT_INDEX, 3, 10)
        else {
            panic!("a seal breaking S1 is Unsealed")
        };
        assert!(why.contains("file 10 is not a seal marker"), "{why}");
        assert!(why.contains("rule S1"), "{why}");
    }

    /// §1.2 rule 1 comes before §5's rules: a seal requiring a feature this
    /// tapectl does not know, beside `short-seal`, parses uninterpreted —
    /// though it breaks S2 — so it is refused by name and not read as
    /// unsealed. The same for another layout version.
    #[test]
    fn a_refused_seal_is_returned_uninterpreted_before_the_rules() {
        let f = Fixture {
            requires: Some("[\"unknown-feature\"]"),
            ended_after: None,
            ..Fixture::full(4)
        };
        let seal = parse_seal_marker(&f.text())
            .expect("the seal still PARSES: a write-path contact check must see it as sealed");
        let why = seal.refusal().expect("refused");
        assert!(why.contains("\"unknown-feature\""), "{why}");
        let text = f.text();
        assert!(matches!(
            judge_seal(Ok(text.as_bytes()), FRONT_INDEX, 3, 12),
            SealVerdict::Refused(_)
        ));

        let other_layout = Fixture {
            ended_after: Some(-5),
            ..Fixture::full(4)
        }
        .text()
        .replace("layout_version = 3", "layout_version = 2");
        let seal = parse_seal_marker(&other_layout).expect("parses, uninterpreted");
        assert!(seal.refusal().unwrap().contains("layout_version = 2"));
    }

    /// §5 "Classification", the pure function: equal to the plan's count is
    /// full, less is short, greater means the two ends disagree, and so does
    /// a seal stating no file before it.
    #[test]
    fn classification_is_full_short_or_disagrees_by_the_plan_count() {
        let full = Fixture::full(4).fields();
        assert_eq!(classify_seal_ending(&full, 13), SealEnding::Full);
        assert_eq!(
            classify_seal_ending(&full, 12),
            SealEnding::Disagrees {
                seal_file_count: 13,
                planned_file_count: 12
            }
        );
        assert_eq!(
            classify_seal_ending(&full, 20),
            SealEnding::Short { ended_after: 11 }
        );

        let short = Fixture::short(4, 9).fields();
        assert_eq!(
            classify_seal_ending(&short, 13),
            SealEnding::Short { ended_after: 9 }
        );
        assert_eq!(classify_seal_ending(&short, 11), SealEnding::Full);

        for file_count in [1, 0, -3] {
            let odd = ParsedSeal {
                file_count,
                ended_after: None,
                ..Fixture::full(4).fields()
            };
            assert_eq!(
                classify_seal_ending(&odd, 13),
                SealEnding::Disagrees {
                    seal_file_count: file_count,
                    planned_file_count: 13
                },
                "file_count = {file_count}"
            );
        }
    }

    /// Every rule violation reaches an operator in words, naming its rule
    /// and no Rust variant.
    #[test]
    fn seal_rule_violations_render_in_words() {
        let all = [
            SealRuleViolation::EndedAfterNotFileCountLessTwo {
                ended_after: 9,
                file_count: 13,
            },
            SealRuleViolation::MissingEndedAfter,
            SealRuleViolation::EntryCount {
                entries: 10,
                file_count: 11,
            },
            SealRuleViolation::Inconsistent(vec![ConsistencyViolation::MissingHash {
                position: 8,
            }]),
            SealRuleViolation::EndedAfterNotDataSlice {
                ended_after: 7,
                found: Some("tenant_envelope_backup".into()),
            },
            SealRuleViolation::EndedAfterNotDataSlice {
                ended_after: 7,
                found: None,
            },
        ];
        for (v, rule) in all.iter().zip(["S1", "S2", "S4", "S4", "S5", "S5"]) {
            let text = v.to_string();
            let debug = format!("{v:?}");
            let variant = debug.split([' ', '{', '(']).next().unwrap();
            assert!(text.starts_with(&format!("rule {rule}: ")), "{text}");
            assert!(!text.contains(variant), "{text:?} names the variant");
            assert!(!text.contains('{') && !text.contains('}'), "{text:?}");
        }
    }

    /// §4: File 3's `[index]` states `may_end_early = true`, the line after
    /// `layout_version`, and the parser exposes it; the entries read as
    /// before.
    #[test]
    fn a_front_index_declaring_may_end_early_reads_true() {
        let text = "\
This tape may end early. The list below is the plan: every file the
write meant to put on the tape.

[index]
volume = \"SS01\"
layout_version = 3
may_end_early = true

[[files]]
position = 0
type = \"id_thunk\"
size_bytes = 10
sha256_encrypted = \"h0\"
";
        let mut padded = text.as_bytes().to_vec();
        padded.resize(padded.len() + 512, 0);
        let padded = String::from_utf8(padded).unwrap();
        let doc = parse_front_index_document(&padded).expect("a 1.6.0 File 3 parses");
        assert!(doc.may_end_early);
        assert_eq!(doc.volume, "SS01");
        assert_eq!(doc.files.len(), 1);
        assert_eq!(parse_front_index(&padded).unwrap(), doc.files);
    }

    /// A front index with no `may_end_early`: absent is false (it decides
    /// nothing, §1.8). A value that is not a bool is a parse failure, as any
    /// malformed key is.
    #[test]
    fn a_front_index_without_may_end_early_reads_false() {
        let text = "[index]\nvolume = \"OLD01\"\nlayout_version = 3\n\n[[files]]\nposition = 0\ntype = \"id_thunk\"\nsize_bytes = 10\nsha256_encrypted = \"h0\"\n";
        let doc = parse_front_index_document(text).expect("it parses");
        assert!(!doc.may_end_early);
        assert_eq!(doc.volume, "OLD01");
        assert_eq!(doc.files.len(), 1);

        let wrong = text.replace(
            "layout_version = 3\n",
            "layout_version = 3\nmay_end_early = 1\n",
        );
        assert!(parse_front_index_document(&wrong).is_err());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::volume::layout::{
        generate_front_index, generate_id_thunk_v2, generate_seal_marker, FrontIndexFile,
        IdThunkV2Params,
    };

    /// A well-formed 6-file layout: id_thunk, guide, restore_sh, front_index,
    /// one data slice, seal_marker — used as the shared fixture for
    /// round-trip and consistency tests. Mirrors the true business rule
    /// (`volume-format-v3.md` §4): every file has size+hash except the front
    /// index's own entry (neither) and the seal marker's entry (size only,
    /// no hash — not yet written when File 3 is built).
    fn sample_files() -> Vec<FrontIndexFile> {
        vec![
            FrontIndexFile {
                position: 0,
                type_label: "id_thunk",
                size_bytes: Some(1000),
                sha256_encrypted: Some("h0".into()),
            },
            FrontIndexFile {
                position: 1,
                type_label: "system_guide",
                size_bytes: Some(2000),
                sha256_encrypted: Some("h1".into()),
            },
            FrontIndexFile {
                position: 2,
                type_label: "restore_sh",
                size_bytes: Some(3000),
                sha256_encrypted: Some("h2".into()),
            },
            FrontIndexFile {
                position: 3,
                type_label: "front_index",
                size_bytes: None,
                sha256_encrypted: None,
            },
            FrontIndexFile {
                position: 4,
                type_label: "data_slice",
                size_bytes: Some(524_288),
                sha256_encrypted: Some("h4".into()),
            },
            FrontIndexFile {
                position: 5,
                type_label: "seal_marker",
                size_bytes: Some(777),
                sha256_encrypted: None,
            },
        ]
    }

    #[test]
    fn front_index_round_trips_through_the_parser() {
        let files = sample_files();
        let generated = generate_front_index("RT01", &files);

        let parsed = parse_front_index(&generated).expect("parses");
        assert_eq!(parsed.len(), files.len());
        for (p, f) in parsed.iter().zip(files.iter()) {
            assert_eq!(p.position, f.position);
            assert_eq!(p.type_label, f.type_label);
            assert_eq!(p.size_bytes, f.size_bytes);
            assert_eq!(p.sha256_encrypted, f.sha256_encrypted);
        }

        // Exclusion rule: File 3's own entry carries neither size nor hash.
        let self_entry = &parsed[3];
        assert_eq!(self_entry.type_label, "front_index");
        assert_eq!(self_entry.size_bytes, None);
        assert_eq!(self_entry.sha256_encrypted, None);

        assert!(validate_consistency(&parsed).is_empty());
    }

    #[test]
    fn seal_marker_round_trips_through_the_parser() {
        let files = sample_files();
        let generated = generate_seal_marker("RT01", files.len() as i32, "deadbeef", &files);

        let parsed = parse_seal_marker(&generated).expect("parses");
        assert_eq!(parsed.volume, "RT01");
        assert_eq!(parsed.layout_version, 3);
        assert_eq!(parsed.file_count, files.len() as i32);
        assert_eq!(parsed.front_index_sha256, "deadbeef");
        assert!(chrono::DateTime::parse_from_rfc3339(&parsed.sealed_at).is_ok());

        assert_eq!(parsed.files.len(), files.len());
        for (p, f) in parsed.files.iter().zip(files.iter()) {
            assert_eq!(p.position, f.position);
            assert_eq!(p.type_label, f.type_label);
            assert_eq!(p.size_bytes, f.size_bytes);
            assert_eq!(p.sha256_encrypted, f.sha256_encrypted);
        }

        assert!(validate_consistency(&parsed.files).is_empty());
    }

    #[test]
    fn embedded_copy_carries_file3_size_and_hash_but_never_its_own() {
        // The embedded copy is MORE complete than File 3 itself: by seal
        // time, File 3's own size+hash are known, so the copy fills them
        // in. Only the seal marker's own entry stays hash-less.
        let mut files = sample_files();
        // Fill in what seal time now knows about File 3.
        files[3].size_bytes = Some(2048);
        files[3].sha256_encrypted = Some("fi-hash".into());

        let generated = generate_seal_marker("RT01", files.len() as i32, "fi-hash", &files);
        let parsed = parse_seal_marker(&generated).expect("parses");

        let fi_entry = &parsed.files[3];
        assert_eq!(fi_entry.type_label, "front_index");
        assert_eq!(fi_entry.size_bytes, Some(2048));
        assert_eq!(fi_entry.sha256_encrypted, Some("fi-hash".into()));

        let seal_entry = parsed.files.last().unwrap();
        assert_eq!(seal_entry.type_label, "seal_marker");
        assert!(seal_entry.sha256_encrypted.is_none());
    }

    #[test]
    fn parser_is_tolerant_of_trailing_block_padding_nuls() {
        let files = sample_files();
        let generated = generate_front_index("RT01", &files);

        // Simulate a tape read: the file's true bytes followed by zero
        // padding out to a block boundary.
        let mut padded = generated.into_bytes();
        padded.resize(padded.len() + 8192, 0);
        let padded_str = String::from_utf8(padded).unwrap();

        let parsed = parse_front_index(&padded_str).expect("parses despite NUL padding");
        assert_eq!(parsed.len(), files.len());
    }

    #[test]
    fn seal_parser_is_tolerant_of_trailing_block_padding_nuls() {
        let files = sample_files();
        let generated = generate_seal_marker("RT01", files.len() as i32, "deadbeef", &files);

        let mut padded = generated.into_bytes();
        padded.resize(padded.len() + 4096, 0);
        let padded_str = String::from_utf8(padded).unwrap();

        let parsed = parse_seal_marker(&padded_str).expect("parses despite NUL padding");
        assert_eq!(parsed.files.len(), files.len());
    }

    #[test]
    fn missing_marker_is_an_err_not_a_panic() {
        assert!(parse_front_index("no toml here at all").is_err());
        assert!(parse_seal_marker("no toml here either").is_err());
        assert!(parse_id_thunk_identity("no toml here at all either").is_err());
    }

    // --- id thunk identity (resume's File-0 check) -----------------------

    fn sample_id_thunk_params<'a>(label: &'a str, uuid: &'a str) -> IdThunkV2Params<'a> {
        IdThunkV2Params {
            label,
            uuid,
            media_type: "LTO-6",
            tapectl_version: "0.2.0",
            nominal_capacity: 2_500_000_000_000,
            mam_capacity: 2_400_000_000_000,
            total_files: 12,
            mam_manufacturer: "IBM",
            mam_serial: "SERIAL1",
            mam_length: 846,
            mam_loads: 5,
            created_at: "2026-07-22T20:09:00Z",
            cartridge_identity_source: None,
        }
    }

    #[test]
    fn id_thunk_identity_round_trips_through_the_parser() {
        let params = sample_id_thunk_params("RT01", "11111111-2222-3333-4444-555555555555");
        let generated = generate_id_thunk_v2(&params);

        let parsed = parse_id_thunk_identity(&generated).expect("parses");
        assert_eq!(parsed.label, "RT01");
        assert_eq!(parsed.uuid, "11111111-2222-3333-4444-555555555555");
    }

    /// #547 (`volume-format-v3.md` §1.1): every string in File 0 is a TOML
    /// basic string, `[media]`'s included. A hand-typed barcode holding a
    /// quote or a backslash, the route a drive with no chip serial takes,
    /// still gives a File 0 every Rust reader parses, and each value comes
    /// back as it was given.
    #[test]
    fn file_0_strings_holding_a_quote_or_a_backslash_read_back_as_given() {
        let serial = "A\"B\\C";
        let manufacturer = "M\"\\x";
        let mut params = sample_id_thunk_params("RT547", "11111111-2222-3333-4444-555555555555");
        params.mam_serial = serial;
        params.mam_manufacturer = manufacturer;
        params.cartridge_identity_source = Some("operator");
        let generated = generate_id_thunk_v2(&params);

        let identity = parse_id_thunk_identity(&generated).expect("File 0 parses");
        assert_eq!(identity.label, "RT547");
        let media = parse_id_thunk_media(&generated).expect("[media] parses");
        assert_eq!(media.cartridge_serial, serial);
        assert_eq!(media.cartridge_manufacturer.as_deref(), Some(manufacturer));
        assert_eq!(media.cartridge_identity_source.as_deref(), Some("operator"));
        check_id_thunk_readable(&generated).expect("its claims read");
        parse_id_thunk_layout_pointers(&generated).expect("its pointers read");
    }

    #[test]
    fn id_thunk_identity_parser_ignores_the_layout_and_media_tables() {
        // The document continues with [layout] and [media] tables (plus
        // several other [volume] keys) after the two fields this parser
        // cares about — none of that may cause a parse failure.
        let params = sample_id_thunk_params("RT02", "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee");
        let generated = generate_id_thunk_v2(&params);
        assert!(generated.contains("[layout]"));
        assert!(generated.contains("[media]"));

        assert!(parse_id_thunk_identity(&generated).is_ok());
    }

    #[test]
    fn id_thunk_identity_parser_is_tolerant_of_trailing_block_padding_nuls() {
        let params = sample_id_thunk_params("RT03", "cccccccc-dddd-eeee-ffff-000000000000");
        let generated = generate_id_thunk_v2(&params);

        let mut padded = generated.into_bytes();
        padded.resize(padded.len() + 4096, 0);
        let padded_str = String::from_utf8(padded).unwrap();

        let parsed = parse_id_thunk_identity(&padded_str).expect("parses despite NUL padding");
        assert_eq!(parsed.label, "RT03");
    }

    // --- id thunk volume meta (the `volumes` row a rebuild reconstructs, #136) --

    #[test]
    fn id_thunk_volume_meta_round_trips_through_the_parser() {
        let params = sample_id_thunk_params("RT06", "22222222-3333-4444-5555-666666666666");
        let generated = generate_id_thunk_v2(&params);

        let parsed = parse_id_thunk_volume_meta(&generated).expect("parses");
        assert_eq!(parsed.media_type, "LTO-6");
        assert_eq!(parsed.nominal_capacity_bytes, 2_500_000_000_000);
        assert_eq!(parsed.mam_capacity_bytes, 2_400_000_000_000);
    }

    #[test]
    fn id_thunk_volume_meta_parser_is_tolerant_of_trailing_block_padding_nuls() {
        let params = sample_id_thunk_params("RT07", "33333333-4444-5555-6666-777777777777");
        let generated = generate_id_thunk_v2(&params);
        let mut padded = generated.into_bytes();
        padded.resize(4096, 0);
        let padded_str = String::from_utf8(padded).unwrap();

        let parsed = parse_id_thunk_volume_meta(&padded_str).expect("parses despite NUL padding");
        assert_eq!(parsed.media_type, "LTO-6");
    }

    #[test]
    fn id_thunk_volume_meta_missing_marker_is_an_err_not_a_panic() {
        assert!(parse_id_thunk_volume_meta("no toml here at all").is_err());
    }

    // --- id thunk media (which cartridge, and how we know — #192) ---------

    #[test]
    fn id_thunk_media_round_trips_both_identity_sources() {
        for source in ["mam", "operator"] {
            let params = IdThunkV2Params {
                cartridge_identity_source: Some(source),
                ..sample_id_thunk_params("RT08", "44444444-5555-6666-7777-888888888888")
            };
            let parsed = parse_id_thunk_media(&generate_id_thunk_v2(&params)).expect("parses");
            assert_eq!(parsed.cartridge_serial, "SERIAL1");
            assert_eq!(parsed.cartridge_identity_source.as_deref(), Some(source));
        }
    }

    /// THE rule that makes every pre-#192 tape safe to read
    /// (`docs/design/volume-format-v3.md` §1.1): absent means UNKNOWN and
    /// never "mam". The input here is a File 0 `[media]` body exactly as the
    /// generator emitted it before the field existed — written as a literal,
    /// not generated, so this cannot silently start testing the new shape.
    #[test]
    fn a_pre_change_file_0_reads_as_unknown_never_as_mam() {
        let legacy = "\
[media]
cartridge_manufacturer = \"IBM\"
cartridge_serial = \"SERIAL1\"
tape_length_meters = 846
load_count_at_write = 5
";
        let parsed = parse_id_thunk_media(legacy).expect("a pre-#192 File 0 must still parse");
        assert_eq!(parsed.cartridge_serial, "SERIAL1");
        assert_eq!(
            parsed.cartridge_identity_source, None,
            "an omitted cartridge_identity_source must read as unknown — defaulting it \
             to \"mam\" would make every tape written before the field existed falsely \
             attest a chip-verified serial"
        );
    }

    // --- id thunk provenance (when written, at which load — #305) --------

    /// Issue #305: File 0's `created_at` and `load_count_at_write` come back
    /// by value through the live generator.
    #[test]
    fn id_thunk_provenance_round_trips_created_at_and_the_load_count() {
        let params = sample_id_thunk_params("RT10", "66666666-7777-8888-9999-000000000000");
        let parsed = parse_id_thunk_provenance(&generate_id_thunk_v2(&params)).expect("parses");
        assert_eq!(parsed.created_at.as_deref(), Some("2026-07-22T20:09:00Z"));
        assert_eq!(parsed.load_count_at_write, Some(5));
    }

    /// The writer's `0` is "unknown" (`volume-format-v3.md` §1.1) and must
    /// not come back as a confident zero (#184). The non-zero case in the
    /// same test proves the parser is not simply `None` for everything.
    #[test]
    fn a_zero_load_count_at_write_reads_as_unknown() {
        let unknown = IdThunkV2Params {
            mam_loads: 0,
            ..sample_id_thunk_params("RT11", "77777777-8888-9999-0000-111111111111")
        };
        let known = IdThunkV2Params {
            mam_loads: 17,
            ..sample_id_thunk_params("RT11", "77777777-8888-9999-0000-111111111111")
        };
        let parse = |p: &IdThunkV2Params| {
            parse_id_thunk_provenance(&generate_id_thunk_v2(p))
                .unwrap()
                .load_count_at_write
        };
        assert_eq!(parse(&unknown), None, "0 is the writer's unknown");
        assert_eq!(parse(&known), Some(17));
    }

    /// The positive control: a File 0 that predates both keys — written as
    /// a LITERAL, so this cannot silently start testing the generator's new
    /// shape — still parses, to `None` twice. One with `[volume]` and no
    /// `[media]` at all (the pre-ADR-0010 window) keeps its `created_at`.
    #[test]
    fn a_file_0_without_the_provenance_keys_parses_to_unknown() {
        let neither = "\
[volume]
magic = \"tapectl-volume-v2\"
label = \"OLD01\"
uuid = \"88888888-9999-0000-1111-222222222222\"
layout_version = 2

[layout]
front_index = 3
seal_marker = 9
total_files = 10

[media]
cartridge_serial = \"SERIAL1\"
";
        assert_eq!(
            parse_id_thunk_provenance(neither).expect("an old File 0 must still parse"),
            IdThunkProvenance::default()
        );
        let no_media = "\
[volume]
label = \"OLD02\"
created_at = \"2026-08-01T10:00:00.123456789+00:00\"

[layout]
front_index = 3
seal_marker = 9
total_files = 10
";
        let p = parse_id_thunk_provenance(no_media).unwrap();
        assert_eq!(
            p.created_at.as_deref(),
            Some("2026-08-01T10:00:00.123456789+00:00")
        );
        assert_eq!(p.load_count_at_write, None);
        assert!(parse_id_thunk_provenance("no toml here at all").is_err());
    }

    /// The catalog stores `datetime('now')`'s shape; an on-tape RFC 3339
    /// value is converted to it in UTC, and text that is not RFC 3339 is an
    /// absence.
    #[test]
    fn on_tape_timestamps_convert_to_the_catalog_form() {
        assert_eq!(
            catalog_timestamp("2026-07-22T20:09:00Z").as_deref(),
            Some("2026-07-22 20:09:00")
        );
        assert_eq!(
            catalog_timestamp("2026-08-01T12:00:00.987654321+02:00").as_deref(),
            Some("2026-08-01 10:00:00")
        );
        assert_eq!(catalog_timestamp("yesterday"), None);
        assert_eq!(catalog_timestamp(""), None);
    }

    /// The seal marker's `sealed_at`, as `generate_seal_marker` writes it,
    /// reaches a caller in the catalog's form.
    #[test]
    fn a_generated_seal_marker_yields_a_catalog_sealed_at() {
        let files = sample_files();
        let seal = generate_seal_marker("RT12", files.len() as i32, &"ab".repeat(32), &files);
        let parsed = parse_seal_marker(&seal).unwrap();
        let catalog = catalog_timestamp(&parsed.sealed_at).expect("RFC 3339");
        assert_eq!(catalog.len(), "2026-07-22 20:09:00".len());
        assert_eq!(catalog_timestamp(&parsed.sealed_at), Some(catalog));
    }

    /// The same, through the live generator: `None` in, nothing out.
    #[test]
    fn id_thunk_media_omits_the_source_entirely_when_it_is_unknown() {
        let params = sample_id_thunk_params("RT09", "55555555-6666-7777-8888-999999999999");
        let generated = generate_id_thunk_v2(&params);
        assert!(
            !generated.contains("cartridge_identity_source"),
            "unknown must emit no key at all, not an empty value"
        );
        let parsed = parse_id_thunk_media(&generated).expect("parses");
        assert!(parsed.cartridge_identity_source.is_none());
    }

    #[test]
    fn id_thunk_media_parser_is_tolerant_of_trailing_block_padding_nuls() {
        let params = IdThunkV2Params {
            cartridge_identity_source: Some("operator"),
            ..sample_id_thunk_params("RT10", "66666666-7777-8888-9999-aaaaaaaaaaaa")
        };
        let mut padded = generate_id_thunk_v2(&params).into_bytes();
        padded.resize(padded.len() + 4096, 0);
        let padded_str = String::from_utf8(padded).unwrap();

        let parsed = parse_id_thunk_media(&padded_str).expect("parses despite NUL padding");
        assert_eq!(
            parsed.cartridge_identity_source.as_deref(),
            Some("operator")
        );
    }

    #[test]
    fn id_thunk_media_missing_marker_is_an_err_not_a_panic() {
        assert!(parse_id_thunk_media("no toml here at all").is_err());
    }

    /// Issue #165 item 1: `catalog rebuild`'s cartridge auto-registration
    /// needs manufacturer + length off the tape, the same facts `cartridge
    /// register --manufacturer/--length` would ask an operator to type. Both
    /// are read straight off the `[media]` table the live generator already
    /// writes.
    #[test]
    fn id_thunk_media_carries_manufacturer_and_length() {
        let params = sample_id_thunk_params("RT11", "77777777-8888-9999-aaaa-bbbbbbbbbbbb");
        let generated = generate_id_thunk_v2(&params);
        let parsed = parse_id_thunk_media(&generated).expect("parses");
        assert_eq!(parsed.cartridge_manufacturer.as_deref(), Some("IBM"));
        assert_eq!(parsed.tape_length_meters, Some(846));
    }

    /// Additive (issue #165): a `[media]` table missing `cartridge_manufacturer`
    /// / `tape_length_meters` entirely — a hand-assembled thunk, or a future
    /// shape that drops them — must still parse, with both fields reading as
    /// `None` rather than failing the whole document.
    #[test]
    fn id_thunk_media_without_manufacturer_or_length_still_parses() {
        let bare = "\
[media]
cartridge_serial = \"SERIAL1\"
";
        let parsed = parse_id_thunk_media(bare).expect(
            "a [media] table missing the new keys \
            entirely must still parse",
        );
        assert_eq!(parsed.cartridge_serial, "SERIAL1");
        assert_eq!(parsed.cartridge_manufacturer, None);
        assert_eq!(parsed.tape_length_meters, None);
    }

    // --- id thunk layout pointers (the foreign-tape seal-position check, #27) --

    #[test]
    fn id_thunk_layout_pointers_round_trip_through_the_parser() {
        let params = sample_id_thunk_params("RT04", "11111111-2222-3333-4444-555555555555");
        let generated = generate_id_thunk_v2(&params);

        let parsed = parse_id_thunk_layout_pointers(&generated).expect("parses");
        assert_eq!(parsed.front_index, 3);
        assert_eq!(parsed.total_files, 12);
        assert_eq!(parsed.seal_marker, 11, "seal_marker = total_files - 1");
    }

    #[test]
    fn id_thunk_layout_pointers_parser_ignores_the_volume_and_media_tables() {
        let params = sample_id_thunk_params("RT05", "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee");
        let generated = generate_id_thunk_v2(&params);
        assert!(parse_id_thunk_layout_pointers(&generated).is_ok());
    }

    #[test]
    fn id_thunk_layout_pointers_parser_is_tolerant_of_trailing_block_padding_nuls() {
        let params = sample_id_thunk_params("RT06", "cccccccc-dddd-eeee-ffff-000000000000");
        let generated = generate_id_thunk_v2(&params);
        let mut padded = generated.into_bytes();
        padded.resize(padded.len() + 4096, 0);
        let padded_str = String::from_utf8(padded).unwrap();

        let parsed =
            parse_id_thunk_layout_pointers(&padded_str).expect("parses despite NUL padding");
        assert_eq!(parsed.total_files, 12);
    }

    #[test]
    fn id_thunk_layout_pointers_missing_marker_is_an_err_not_a_panic() {
        assert!(parse_id_thunk_layout_pointers("no toml here at all").is_err());
    }

    /// Before 1.6.0 an empty copy parsed as an empty list. A 1.6.0 seal
    /// with no `[[files]]` breaks §5's rules (S1: `ended_after = -2`; S4:
    /// no entries) and is unparseable, rule S6: the unsealed signal, an
    /// `Err` and never a panic.
    #[test]
    fn a_1_6_0_seal_with_no_files_is_unparseable_not_a_panic() {
        let generated = generate_seal_marker("EMPTY01", 0, "deadbeef", &[]);
        let err = parse_seal_marker(&generated).unwrap_err().to_string();
        assert!(err.contains("rule S1"), "{err}");
    }

    #[test]
    fn valid_sequence_has_no_consistency_violations() {
        let files = sample_files();
        let generated = generate_front_index("RT01", &files);
        let parsed = parse_front_index(&generated).unwrap();
        assert_eq!(validate_consistency(&parsed), Vec::new());
    }

    #[test]
    fn detects_position_out_of_sequence() {
        let mut files = sample_files();
        files[2].position = 7; // duplicate/gap: was 2, now clashes with nothing but breaks the run
        let generated = generate_front_index("RT01", &files);
        let parsed = parse_front_index(&generated).unwrap();
        let violations = validate_consistency(&parsed);
        assert!(violations.iter().any(|v| matches!(
            v,
            ConsistencyViolation::PositionOutOfSequence {
                index: 2,
                expected: 2,
                actual: 7
            }
        )));
    }

    #[test]
    fn detects_front_index_count_and_position_problems() {
        // Two entries claim to be the front_index type.
        let mut files = sample_files();
        files[4].type_label = "front_index";
        let generated = generate_front_index("RT01", &files);
        let parsed = parse_front_index(&generated).unwrap();
        let violations = validate_consistency(&parsed);
        assert!(violations
            .iter()
            .any(|v| matches!(v, ConsistencyViolation::FrontIndexCount { found: 2 })));
    }

    #[test]
    fn detects_front_index_not_at_position_three() {
        // A single-entry list where the lone front_index sits at position 0
        // instead of 3 — isolates the "wrong position" branch from the
        // "wrong count" branch.
        let files = vec![FrontIndexFile {
            position: 0,
            type_label: "front_index",
            size_bytes: None,
            sha256_encrypted: None,
        }];
        let generated = generate_front_index("RT01", &files);
        let parsed = parse_front_index(&generated).unwrap();
        let violations = validate_consistency(&parsed);
        assert!(violations.iter().any(|v| matches!(
            v,
            ConsistencyViolation::FrontIndexNotAtThree { position: 0 }
        )));
    }

    /// Issue #357: every violation renders in words — no variant or field
    /// name, no `{ .. }` — because it reaches the operator as text.
    #[test]
    fn consistency_violations_render_in_words() {
        let all = [
            ConsistencyViolation::PositionOutOfSequence {
                index: 2,
                expected: 2,
                actual: 5,
            },
            ConsistencyViolation::FrontIndexCount { found: 0 },
            ConsistencyViolation::FrontIndexNotAtThree { position: 1 },
            ConsistencyViolation::SealMarkerCount { found: 2 },
            ConsistencyViolation::SealMarkerNotLast {
                position: 4,
                last_index: 6,
            },
            ConsistencyViolation::MissingSize { position: 7 },
            ConsistencyViolation::MissingHash { position: 8 },
        ];
        for v in &all {
            let text = v.to_string();
            let debug = format!("{v:?}");
            let variant = debug.split([' ', '{']).next().unwrap();
            assert!(!text.contains(variant), "{text:?} names the variant");
            assert!(!text.contains('{') && !text.contains('}'), "{text:?}");
        }
        assert_eq!(
            all[0].to_string(),
            "entry 2 claims position 5, expected 2 (positions must run 0, 1, 2, … with no gaps)"
        );
    }

    #[test]
    fn detects_seal_marker_count_and_position_problems() {
        // Seal marker present but not last.
        let mut files = sample_files();
        files.push(FrontIndexFile {
            position: 6,
            type_label: "data_slice",
            size_bytes: Some(10),
            sha256_encrypted: Some("h6".into()),
        });
        let generated = generate_front_index("RT01", &files);
        let parsed = parse_front_index(&generated).unwrap();
        let violations = validate_consistency(&parsed);
        assert!(violations.iter().any(|v| matches!(
            v,
            ConsistencyViolation::SealMarkerNotLast {
                position: 5,
                last_index: 6
            }
        )));
    }

    #[test]
    fn collects_multiple_simultaneous_violations() {
        // Break the sequence AND duplicate the front_index type in one go —
        // both must be reported, not just the first found.
        let mut files = sample_files();
        files[2].position = 99; // sequence break
        files[4].type_label = "front_index"; // now two front_index entries
        let generated = generate_front_index("RT01", &files);
        let parsed = parse_front_index(&generated).unwrap();
        let violations = validate_consistency(&parsed);
        assert!(violations
            .iter()
            .any(|v| matches!(v, ConsistencyViolation::PositionOutOfSequence { .. })));
        assert!(violations
            .iter()
            .any(|v| matches!(v, ConsistencyViolation::FrontIndexCount { found: 2 })));
        assert!(violations.len() >= 2);
    }

    #[test]
    fn hollow_map_entries_are_flagged() {
        // Completeness (format §3): a content entry missing size_bytes or
        // sha256_encrypted must be flagged — a map that parses and is
        // well-ordered but omits a slice's size/hash is hollow (no padding
        // trim for the heir, nothing for the keyless chain to verify) and
        // must fail loudly at the Navigable tier, not only at Integrity.
        let mut files = sample_files();
        let victim = files
            .iter_mut()
            .find(|f| f.type_label == "data_slice")
            .expect("fixture has a slice");
        let victim_pos = victim.position;
        victim.size_bytes = None;
        victim.sha256_encrypted = None;
        let generated = generate_front_index("RT01", &files);
        let parsed = parse_front_index(&generated).unwrap();
        let violations = validate_consistency(&parsed);
        assert!(violations.contains(&ConsistencyViolation::MissingSize {
            position: victim_pos
        }));
        assert!(violations.contains(&ConsistencyViolation::MissingHash {
            position: victim_pos
        }));
        // The two self-referential exclusions stay exempt: the untouched
        // fixture (front_index entry bare, seal entry hash-less) is clean.
        let clean = validate_consistency(
            &parse_front_index(&generate_front_index("RT01", &sample_files())).unwrap(),
        );
        assert_eq!(clean, Vec::new());
    }
}

/// Recorded bytes of tapectl volumes of other layouts, for the tests of
/// `volume-format-v3.md` §7.6 (#560). They are samples of a FOREIGN format, not
/// fixtures of this tapectl's own staging (plan invariant 8): nothing here is
/// generated by this tapectl, so a change to its writers cannot move them.
#[cfg(test)]
pub(crate) mod foreign_layouts {
    pub const LABEL: &str = "L6-0001";
    pub const UUID: &str = "4d1f0c52-8a3e-4b7a-9c61-0e2f5a7b9d13";

    /// File 0 as tapectl 1.9.2 wrote it for a Layout 2 tape (the text of its
    /// `layout::generate_id_thunk_v2`, `requires` as 1.9.2 stated it).
    pub const LAYOUT_2_FILE_0: &str = r#"================================================================
                     TAPECTL ARCHIVAL VOLUME
================================================================

Label:   L6-0001
Media:   LTO-6
Created: 2026-10-14T09:30:00Z

This tape contains encrypted archival data managed by tapectl,
an open-source archival storage tool.

>>> COMPLETE INSTRUCTIONS ARE IN THE NEXT FILE ON THIS TAPE. <<<
>>> THE FULL MAP OF THIS TAPE IS FILE 3 (the front index).   <<<

Your drive may not be /dev/nst0. That is an example, not a fact
about this tape. List the tape drives on the machine you are
using and substitute the right one everywhere below:

    ls -l /dev/tape/by-id/

A wrong device reads a DIFFERENT tape, or fails outright.

To read the next file (the full recovery guide):

    mt -f /dev/nst0 setblk 524288
    mt -f /dev/nst0 fsf 1
    dd if=/dev/nst0 bs=512k | tr -d '\0' > GUIDE.md
    less GUIDE.md

If you just read this file and the tape is already positioned
past it, read the next file directly:

    mt -f /dev/nst0 setblk 524288
    dd if=/dev/nst0 bs=512k | tr -d '\0' > GUIDE.md

The guide explains everything: what tools you need, how to find
your encryption key, and how to recover your data step by step.
It is written so that an AI assistant can follow it to help you.

================================================================
              MACHINE-READABLE METADATA (TOML)
================================================================

[volume]
magic = "tapectl-volume-v2"
label = "L6-0001"
uuid = "4d1f0c52-8a3e-4b7a-9c61-0e2f5a7b9d13"
layout_version = 2
requires = ["short-seal"]
tapectl_version = "1.9.2"
media_type = "LTO-6"
nominal_capacity_bytes = 2500000000000
mam_capacity_bytes = 2400000000000
created_at = "2026-10-14T09:30:00Z"

[layout]
front_index = 3
seal_marker = 5
total_files = 6

[media]
cartridge_manufacturer = "FUJIFILM"
cartridge_serial = "EW7VWMVKF6"
cartridge_identity_source = "mam"
tape_length_meters = 846
load_count_at_write = 7
"#;

    /// File 0 of a 1.x Layout 2 tape whose operator barcode held a `"` and a
    /// `\`: 1.9.2 interpolated `cartridge_serial = "{serial}"` unescaped, so
    /// the body is not TOML (#547, #587).
    pub fn layout_2_file_0_bad_barcode() -> String {
        let t = LAYOUT_2_FILE_0.replace(
            "cartridge_serial = \"EW7VWMVKF6\"",
            "cartridge_serial = \"EW\"7\\VW\"",
        );
        assert_ne!(t, LAYOUT_2_FILE_0);
        t
    }

    /// File 0 of a pre-1.0 Layout 1 development tape: `magic` and
    /// `layout_version` but no `uuid`, and a body that is not Layout 2's.
    pub const LAYOUT_1_FILE_0: &str = "TAPECTL ARCHIVAL VOLUME\n\n[volume]\n\
        magic = \"tapectl-volume-v1\"\nlabel = \"L6-0001\"\nlayout_version = 1\n\
        created_at = \"2026-05-01T09:30:00Z\"\n\n[layout]\nfront_index = [\n";

    /// The seal marker of that tape, at file 5, as 1.9.2's
    /// `generate_seal_marker` wrote it.
    pub const LAYOUT_2_SEAL: &str = r#"================================================================
                    TAPECTL SEAL MARKER
================================================================

Volume: L6-0001
This file seals the tape: its presence means every file before it is
present. Its absence means the tape is unsealed (interrupted or aborted).

================================================================
              MACHINE-READABLE DATA (TOML)
================================================================

[seal]
volume = "L6-0001"
layout_version = 2
requires = ["short-seal"]
file_count = 6
ended_after = 4
sealed_at = "2026-10-14T11:02:41Z"
front_index_sha256 = "0000000000000000000000000000000000000000000000000000000000000000"

[[files]]
position = 0
type = "id_thunk"
size_bytes = 2113
sha256_encrypted = "1111111111111111111111111111111111111111111111111111111111111111"

[[files]]
position = 5
type = "seal_marker"
"#;

    /// A File 0 of a layout later than this tapectl's: the same keys, the
    /// version and magic moved, and a table this tapectl does not know.
    pub fn layout_4_file_0() -> String {
        let mut t = LAYOUT_2_FILE_0
            .replace("tapectl-volume-v2", "tapectl-volume-v4")
            .replace("layout_version = 2", "layout_version = 4")
            .replace("requires = [\"short-seal\"]", "requires = []");
        t.push_str("\n[future]\nanything = \"a later tapectl wrote this\"\n");
        t
    }

    /// The seal of a layout 4 tape.
    pub fn layout_4_seal() -> String {
        LAYOUT_2_SEAL
            .replace("layout_version = 2", "layout_version = 4")
            .replace("requires = [\"short-seal\"]", "requires = []")
    }
}
