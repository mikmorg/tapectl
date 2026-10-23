//! Parsers for the plaintext front index (File 3), seal marker (File M), and
//! ID thunk (File 0) — the reader half of `layout.rs`'s `generate_front_index`
//! / `generate_seal_marker` / `generate_id_thunk_v2` writer half. Normative
//! grammar: `docs/design/volume-format-v2.md` §3–4; self-consistency rules:
//! `docs/design/v2-open-questions.md` §2.5.
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
/// copy (`docs/design/volume-format-v2.md` §4).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedSeal {
    pub volume: String,
    pub layout_version: i64,
    pub file_count: i32,
    /// The position of the last file before this seal (`volume-format-v2.md`
    /// §4.1, tapectl 1.6.0): `file_count - 2` on every seal that keeps rule
    /// S1. `None` on every seal before 1.6.0, which is a full seal (rule S3;
    /// [`ParsedSeal::effective_ended_after`] gives its value).
    pub ended_after: Option<u64>,
    pub sealed_at: String,
    pub front_index_sha256: String,
    /// The features a reader must know to read this tape (ADR-0012
    /// amendment item 15). Absent on every tape before 1.1.0 = none.
    pub requires: Vec<String>,
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
            tapectl_version: None,
        }
        .refusal("the seal marker")
    }

    /// Whether this seal's `requires` names [`SHORT_SEAL`]: every seal
    /// tapectl 1.6.0 or later writes, full or short (ADR-0012 item 111).
    /// `volume-format-v2.md` §1.2 rule 6 accepts a seal before the planned
    /// position only when this is true (and the tape's File 0, when it reads,
    /// requires the feature too).
    pub fn requires_short_seal(&self) -> bool {
        self.requires.iter().any(|f| f == SHORT_SEAL)
    }

    /// The last file before this seal: its `ended_after`, or, on a seal that
    /// states none (every seal before 1.6.0), `file_count - 2` (rule S3: a
    /// full seal). `None` only for a `file_count` below 2, which no seal that
    /// keeps §4.1's rules states.
    pub fn effective_ended_after(&self) -> Option<u64> {
        self.ended_after
            .or_else(|| u64::try_from(self.file_count).ok()?.checked_sub(2))
    }

    /// Full, short, or at odds with the plan: [`classify_seal_ending`].
    pub fn ending(&self, planned_file_count: u64) -> SealEnding {
        classify_seal_ending(self, planned_file_count)
    }

    /// `volume-format-v2.md` §4.1's parse rules S1, S2, S4 and S5, the
    /// first one broken. [`parse_seal_marker`] already applies them to every
    /// seal that states `ended_after` or requires [`SHORT_SEAL`]; on a seal
    /// before 1.6.0 they are the caller's to apply (S3 then gives S5 its
    /// position), since no reader before 1.6.0 checked the embedded copy when
    /// it parsed a seal. A seal that breaks one is unparseable (rule S6).
    pub fn check_parse_rules(&self) -> std::result::Result<(), SealRuleViolation> {
        // S2: a seal that requires short-seal states where it ended.
        if self.requires_short_seal() && self.ended_after.is_none() {
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

/// The first of `volume-format-v2.md` §4.1's parse rules a seal breaks. A
/// seal that breaks one is unparseable (rule S6): by the fail-safe precedence
/// (`v2-open-questions.md` §2.5) that is the unsealed signal, never "sealed".
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SealRuleViolation {
    /// S1: `ended_after` is a non-negative integer and `ended_after + 2 ==
    /// file_count`.
    EndedAfterNotFileCountLessTwo { ended_after: i64, file_count: i32 },
    /// S2: a seal whose `requires` names `short-seal` states `ended_after`.
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
            SealRuleViolation::MissingEndedAfter => write!(
                f,
                "rule S2: it requires \"{SHORT_SEAL}\" but states no ended_after"
            ),
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
/// `volume-format-v2.md` §4.1, "Classification".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SealEnding {
    /// The seal's `file_count` is the plan's: it sits at the planned
    /// position M and ended after M-1, the last data slice.
    Full,
    /// The seal's `file_count` is less than the plan's: the tape ended after
    /// `ended_after`, and every planned position past it is not on the tape
    /// (§4.2). Whether a reader accepts it is §1.2 rule 6's question, not
    /// this one's: only on a tape that requires `short-seal`, and only a seal
    /// that does ([`ParsedSeal::requires_short_seal`]).
    Short { ended_after: u64 },
    /// The seal's `file_count` is greater than the plan's (or states no file
    /// before the seal at all): the tape's two ends disagree, as a
    /// `front_index_sha256` mismatch does.
    Disagrees {
        seal_file_count: i32,
        planned_file_count: u64,
    },
}

/// Classify `seal` against the plan's file count (`volume-format-v2.md`
/// §4.1): equal is [`SealEnding::Full`], less is [`SealEnding::Short`],
/// greater is [`SealEnding::Disagrees`]. Pure: it reads nothing, and judges
/// nothing but the two counts. Run it on a seal that parsed, and so keeps
/// rules S1 to S5 when it states `ended_after` or requires `short-seal`.
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
    /// From 1.6.0 (§3). Absent means false.
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
    /// `[index] may_end_early = true` (`volume-format-v2.md` §3, tapectl
    /// 1.6.0): the tape may end before the last file listed, and its seal
    /// says where. False when absent, on every tape before 1.6.0. It decides
    /// nothing for a reader: `requires` and the seal's `ended_after` do.
    pub may_end_early: bool,
    /// The planned files, in position order: on a tape that ended early,
    /// the ones after the seal's `ended_after` are not on it (§4.2).
    pub files: Vec<ParsedIndexEntry>,
}

#[derive(Debug, Deserialize)]
struct SealHeader {
    volume: String,
    layout_version: i64,
    file_count: i64,
    /// From 1.6.0 (§4.1). i64 on the wire so a negative value reaches rule
    /// S1 as a broken rule rather than as a TOML error.
    ended_after: Option<i64>,
    sealed_at: String,
    front_index_sha256: String,
    #[serde(default)]
    requires: Vec<String>,
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
/// in the confirm chain walk (`docs/design/volume-format-v2.md` §5) treat a
/// parse failure as "front index unreadable," never crash on it.
pub fn parse_front_index(raw: &str) -> Result<Vec<ParsedIndexEntry>> {
    Ok(parse_front_index_document(raw)?.files)
}

/// [`parse_front_index`], with the `[index]` table's own keys: the volume
/// label and, from 1.6.0, `may_end_early` (`volume-format-v2.md` §3).
pub fn parse_front_index_document(raw: &str) -> Result<ParsedFrontIndex> {
    let body = toml_body(raw, "[index]", "front index")?;
    let doc: FrontIndexDoc = toml::from_str(body)
        .map_err(|e| TapectlError::Other(format!("front index: TOML parse failed: {e}")))?;
    if doc.index.layout_version != LAYOUT_VERSION {
        return Err(TapectlError::Other(format!(
            "front index: layout_version {} — this tapectl ({}) reads layout {LAYOUT_VERSION} only; \
             read the tape with a newer tapectl or the RESTORE.sh at its file 2",
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
/// In this order (`volume-format-v2.md` §1.2 rule 1, §4.1):
/// 1. A seal this tapectl must not read (a feature it does not know, another
///    layout: [`ParsedSeal::refusal`]) parses, uninterpreted, so that every
///    caller refuses it by name, and a write path's contact check still sees
///    a seal there.
/// 2. A seal that states `ended_after` or requires `short-seal` (every seal
///    1.6.0 writes) keeps the parse rules S1, S2, S4 and S5
///    ([`ParsedSeal::check_parse_rules`]); one that breaks a rule is an `Err`
///    naming it (rule S6).
/// 3. A seal before 1.6.0 (no `ended_after`, no `short-seal`) parses as
///    before: a full seal (rule S3).
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
        requires: doc.seal.requires,
        files: doc.files.into_iter().map(Into::into).collect(),
    };
    if seal.refusal().is_some() {
        return Ok(seal);
    }
    if stated_ended_after.is_none() && !seal.requires_short_seal() {
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

/// A tape file's true bytes: the block padding a tape read returns after
/// them (trailing NULs) removed, `volume-format-v2.md` §4.
pub fn without_nul_padding(raw: &[u8]) -> &[u8] {
    &raw[..raw.iter().rposition(|b| *b != 0).map_or(0, |i| i + 1)]
}

/// What a tape's seal marker proves, judged against the front index it ends
/// (issue #458; `volume-format-v2.md` §4). The ONE judgement: `catalog
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
/// `volume-format-v2.md` §1.2 rule 6) gave: its bytes, or the reason it did
/// not read. `seal_position` is named in the reasons, and judged by nothing.
///
/// The check order, and the wording, are those `adopt_lost` has always
/// refused with: a seal that does not read, one that does not parse (from
/// 1.6.0 that includes one breaking §4.1's rules S1-S5, rule S6), one this
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

/// The layout this tapectl reads (File 0's `layout_version`).
pub const LAYOUT_VERSION: i64 = 2;
/// File 0's `magic` for layout 2.
pub const MAGIC: &str = "tapectl-volume-v2";
/// The feature every tape tapectl 1.6.0 or later writes requires (ADR-0012
/// item 111, `volume-format-v2.md` §1.2): the tape may end before the last
/// file its plan lists, its seal states `ended_after`, and a reader trusts
/// the seal over the plan (§4.1, §4.2).
pub const SHORT_SEAL: &str = "short-seal";

/// The feature a tape states when its plan holds a slice of a spannable
/// stage set (ADR-0012 item 124, #478): a unit may run across tapes, so a
/// `[[units]]` block in its envelopes may be a range (item 125: `may_span =
/// true`, every slice of the set listed, `tape_position` only on the slices
/// this tape plans). It is stated beside [`SHORT_SEAL`], as `requires =
/// ["short-seal", "spanning"]` with the space after the comma (normative:
/// without it RESTORE.sh reads the one feature `short-sealspanning`).
pub const SPANNING: &str = "spanning";

/// The features this tapectl knows how to read (ADR-0012 amendment item 15,
/// `volume-format-v2.md` "How v2 extends"). A tape whose File 0 or seal
/// marker lists anything else under `requires` is refused by name. Empty
/// from 1.1.0 to 1.5.0; 1.6.0 knows [`SHORT_SEAL`] (item 111); 1.7.0 knows
/// [`SPANNING`] too (item 124).
pub const KNOWN_FEATURES: &[&str] = &[SHORT_SEAL, SPANNING];

/// What a tape says about the reader it needs: File 0's `magic`,
/// `layout_version` and `requires` (the seal marker repeats the last two).
/// Every Rust reader of a tape checks [`FormatClaims::refusal`] before it
/// interprets anything else, so a tape a later tapectl wrote with a feature
/// this one does not know is refused, never misread. Absent fields are
/// ABSENT, not defaulted to something refusable: every tape before 1.1.0
/// has no `requires`, and a damaged File 0 may lack the others; neither may
/// be turned away for it (the same rule RESTORE.sh keeps).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FormatClaims {
    pub magic: Option<String>,
    pub layout_version: Option<i64>,
    pub requires: Vec<String>,
    /// The writer's version, for the message only.
    pub tapectl_version: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ClaimsToml {
    magic: Option<String>,
    layout_version: Option<i64>,
    #[serde(default)]
    requires: Vec<String>,
    tapectl_version: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ClaimsDoc {
    volume: ClaimsToml,
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
        requires: doc.volume.requires,
        tapectl_version: doc.volume.tapectl_version,
    })
}

impl FormatClaims {
    /// `None` when this tapectl can read a tape making these claims;
    /// otherwise the sentence that says why not, naming what it would need.
    pub fn refusal(&self, what: &str) -> Option<String> {
        let ours = env!("CARGO_PKG_VERSION");
        let writer = self
            .tapectl_version
            .as_deref()
            .map(|v| format!(" (it was written by tapectl {v})"))
            .unwrap_or_default();
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

/// Refuse File 0 `raw` if it claims a format this tapectl cannot read. A
/// File 0 whose claims do not parse (say, a `requires` that is not a list)
/// is refused too: it cannot be shown to be readable.
pub fn check_id_thunk_readable(raw: &str) -> Result<()> {
    let claims = parse_id_thunk_claims(raw)?;
    match claims.refusal("File 0") {
        Some(why) => Err(TapectlError::Other(why)),
        None => Ok(()),
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
/// (`docs/design/volume-format-v2.md` §1). A tape's own claim about its own
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
/// (`docs/design/volume-format-v2.md` §1.1, ADR-0012).
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
    /// writer's "unknown" (`volume-format-v2.md` §1.1) and reads as `None`:
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

    /// A seal's embedded copy that keeps §4.1's rules S4 and S5 (Files 0-3,
    /// one data slice, the seal), so these fixtures stay seals whether the
    /// generator writes a 1.5.0 seal or a 1.6.0 one.
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

    /// The full seal 1.6.0 writes over [`seal_files`]: `requires =
    /// ["short-seal"]`, `ended_after = 4`.
    fn seal() -> String {
        generate_seal_marker("FC01", 6, "h", &seal_files())
    }

    /// The seal tapectl 1.1.0 to 1.5.0 wrote over [`seal_files`], as a
    /// literal in that grammar (`requires = []`, no `ended_after`): what a
    /// 1.6.0 reader must still read (§1.2 rule 2, §4.1 rule S3).
    fn seal_1_5_0() -> String {
        let mut s = String::from(
            "================================================================\n\
             TAPECTL SEAL MARKER\n\
             ================================================================\n\n\
             Volume: FC01\n\n\
             [seal]\n\
             volume = \"FC01\"\n\
             layout_version = 2\n\
             requires = []\n\
             file_count = 6\n\
             sealed_at = \"2026-10-06T00:00:00Z\"\n\
             front_index_sha256 = \"h\"\n",
        );
        for (position, kind) in [
            (0, "id_thunk"),
            (1, "system_guide"),
            (2, "restore_sh"),
            (3, "front_index"),
            (4, "data_slice"),
        ] {
            s.push_str(&format!(
                "\n[[files]]\nposition = {position}\ntype = \"{kind}\"\nsize_bytes = 10\n\
                 sha256_encrypted = \"h\"\n"
            ));
        }
        s.push_str("\n[[files]]\nposition = 5\ntype = \"seal_marker\"\n");
        s
    }

    /// `doc` (a File 0 or a seal) with its `requires = …` line replaced by
    /// `line`: these fixtures say what they require whatever the generator
    /// writes there (`[]` before 1.6.0, `["short-seal"]` from it).
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
        let front_index = b"[index]\nvolume = \"FC01\"\nlayout_version = 2\n";
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

    /// ADR-0012 item 111: every seal 1.6.0 writes requires `short-seal` and
    /// states `ended_after`, and this tapectl reads it.
    #[test]
    fn a_new_tape_requires_short_seal_and_is_readable() {
        let s = parse_seal_marker(&seal()).unwrap();
        assert_eq!(s.requires, vec![SHORT_SEAL.to_string()]);
        assert!(s.requires_short_seal());
        assert_eq!(s.ended_after, Some(4));
        assert_eq!(s.refusal(), None);
        assert_eq!(s.check_parse_rules(), Ok(()), "the fixture keeps S4 and S5");
    }

    /// ADR-0012 amendment item 15: a 1.1.0 to 1.5.0 tape states `requires =
    /// []` in File 0 and the seal marker, and this tapectl still reads it:
    /// a full seal (rule S3).
    #[test]
    fn a_1_1_0_to_1_5_0_tape_requires_nothing_and_is_readable() {
        let t = set_requires(&thunk(), "requires = []");
        check_id_thunk_readable(&t).unwrap();
        let s = parse_seal_marker(&seal_1_5_0()).unwrap();
        assert!(s.requires.is_empty());
        assert_eq!(s.ended_after, None);
        assert_eq!(s.effective_ended_after(), Some(4));
        assert_eq!(s.refusal(), None);
        assert_eq!(s.check_parse_rules(), Ok(()), "the fixture keeps S4 and S5");
    }

    /// Every tape before 1.1.0 (L6-0001 among them) has no `requires`:
    /// absent means none required.
    #[test]
    fn a_tape_without_requires_is_readable() {
        let t = set_requires(&thunk(), "requires = []").replace("requires = []\n", "");
        assert!(!t.contains("requires"));
        check_id_thunk_readable(&t).unwrap();
        let old = seal_1_5_0().replace("requires = []\n", "");
        assert!(!old.contains("requires"));
        let s = parse_seal_marker(&old).unwrap();
        assert_eq!(s.refusal(), None);
    }

    /// An unknown required feature is refused, by name, in File 0 and in
    /// the seal marker; the message says what would read it. (Before 1.6.0
    /// this test's stand-in for "unknown" was `short-seal`, which 1.6.0
    /// knows: see `file_0_and_seal_requiring_short_seal_are_readable`.)
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

    /// ADR-0012 item 111: 1.6.0 knows `short-seal`. A File 0 that requires
    /// it is readable, and so is a seal that requires it (and keeps §4.1's
    /// rules); a feature beside it that this tapectl does not know is still
    /// refused, naming only that one.
    #[test]
    fn file_0_and_seal_requiring_short_seal_are_readable() {
        assert!(KNOWN_FEATURES.contains(&"short-seal"));
        let t = set_requires(&thunk(), "requires = [\"short-seal\"]");
        check_id_thunk_readable(&t).expect("1.6.0 reads a short-seal tape's File 0");
        assert_eq!(parse_id_thunk_claims(&t).unwrap().refusal("File 0"), None);
        let s = parse_seal_marker(&seal()).expect("and the seal 1.6.0 writes");
        assert_eq!(s.refusal(), None);

        let both = set_requires(&thunk(), "requires = [\"short-seal\", \"unknown-feature\"]");
        let err = check_id_thunk_readable(&both).unwrap_err().to_string();
        assert!(err.contains("\"unknown-feature\""), "{err}");
        assert!(
            !err.contains("requires \"short-seal\""),
            "short-seal is known and is not named as the unknown one: {err}"
        );
    }

    /// ADR-0012 item 124: 1.7.0 knows `spanning`. A File 0 and a seal that
    /// state item 124's normative `requires = ["short-seal", "spanning"]`
    /// (the space after the comma included) are readable, and the seal still
    /// requires short-seal, so rule S2 still binds it. A feature beside the
    /// two that this tapectl does not know is still refused, naming only it.
    #[test]
    fn file_0_and_seal_requiring_short_seal_and_spanning_are_readable() {
        assert_eq!(SPANNING, "spanning");
        assert_eq!(KNOWN_FEATURES, &[SHORT_SEAL, SPANNING]);
        let line = "requires = [\"short-seal\", \"spanning\"]";

        let t = set_requires(&thunk(), line);
        assert!(
            t.contains("\nrequires = [\"short-seal\", \"spanning\"]\n"),
            "{t}"
        );
        check_id_thunk_readable(&t).expect("1.7.0 reads a span tape's File 0");
        let claims = parse_id_thunk_claims(&t).unwrap();
        assert_eq!(claims.requires, vec![SHORT_SEAL, SPANNING]);
        assert_eq!(claims.refusal("File 0"), None);

        let s = parse_seal_marker(&set_requires(&seal(), line))
            .expect("a seal stating both features keeps S1-S6 and parses");
        assert_eq!(s.requires, vec![SHORT_SEAL, SPANNING]);
        assert!(s.requires_short_seal());
        assert_eq!(s.refusal(), None);
        // S2 still binds: requiring short-seal (beside spanning) without
        // `ended_after` breaks it.
        let mut no_ending = s.clone();
        no_ending.ended_after = None;
        assert_eq!(
            no_ending.check_parse_rules(),
            Err(SealRuleViolation::MissingEndedAfter)
        );

        let three = "requires = [\"short-seal\", \"spanning\", \"unknown-feature\"]";
        let err = check_id_thunk_readable(&set_requires(&thunk(), three))
            .unwrap_err()
            .to_string();
        assert!(err.contains("requires \"unknown-feature\" that"), "{err}");
        assert!(
            !err.contains("requires \"spanning\"") && !err.contains("\"spanning\" that"),
            "spanning is known and is not named as the unknown one: {err}"
        );
        let why = parse_seal_marker(&set_requires(&seal(), three))
            .unwrap()
            .refusal()
            .expect("refused");
        assert!(why.contains("requires \"unknown-feature\" that"), "{why}");
    }

    /// Keys this tapectl does not know are ignored unless `requires` names
    /// them (the extension rule), in File 0 and the seal marker. (The seal's
    /// unknown key was `ended_after` before 1.6.0, which now knows it and
    /// holds it to rule S1.)
    #[test]
    fn unknown_keys_that_are_not_required_are_ignored() {
        let t = set_requires(&thunk(), "requires = []\nfuture_key = \"x\"");
        assert!(t.contains("\nfuture_key = \"x\"\n"));
        check_id_thunk_readable(&t).unwrap();
        parse_id_thunk_identity(&t).unwrap();
        let s = parse_seal_marker(&set_requires(
            &seal(),
            "requires = [\"short-seal\"]\nfuture_key = 7",
        ))
        .unwrap();
        assert_eq!(s.refusal(), None);
        let s = parse_seal_marker(&set_requires(
            &seal_1_5_0(),
            "requires = []\nfuture_key = 7",
        ))
        .unwrap();
        assert_eq!(s.refusal(), None);
    }

    /// Another layout or magic is refused with a named error (#384).
    #[test]
    fn another_layout_or_magic_is_refused() {
        let t = thunk().replace("layout_version = 2", "layout_version = 3");
        let err = check_id_thunk_readable(&t).unwrap_err().to_string();
        assert!(err.contains("layout_version = 3"), "{err}");
        let t = thunk().replace("tapectl-volume-v2", "tapectl-volume-v3");
        let err = check_id_thunk_readable(&t).unwrap_err().to_string();
        assert!(err.contains("tapectl-volume-v3"), "{err}");
        let fi = "[index]\nvolume = \"X\"\nlayout_version = 3\n";
        let err = parse_front_index(fi).unwrap_err().to_string();
        assert!(err.contains("layout_version 3"), "{err}");
        let s =
            parse_seal_marker(&seal().replace("layout_version = 2", "layout_version = 3")).unwrap();
        assert!(s.refusal().unwrap().contains("layout_version = 3"));
    }
}

/// tapectl 1.6.0's seal and front-index grammar on the read side
/// (`volume-format-v2.md` §3, §4.1; ADR-0012 items 110-111, #420). The seals
/// here are written from §4.1's grammar as literals, not by
/// `layout::generate_seal_marker`, so they pin the spec's bytes rather than
/// whatever the generator emits.
#[cfg(test)]
mod short_seal_tests {
    use super::*;

    /// File 3's bytes in these fixtures, and the hash every seal here binds.
    const FRONT_INDEX: &[u8] = b"[index]\nvolume = \"SS01\"\nlayout_version = 2\n";
    const SHORT: &str = "[\"short-seal\"]";

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
    /// at `k + 1` (§4.1).
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
                requires: Some(SHORT),
                file_count: m + 1,
                ended_after: Some(m - 1),
                entries,
            }
        }

        /// A short 1.6.0 seal at `k + 1` over `plan(slices)`.
        fn short(slices: i64, k: i64) -> Fixture {
            Fixture {
                requires: Some(SHORT),
                file_count: k + 2,
                ended_after: Some(k),
                entries: short_entries(slices, k),
            }
        }

        /// A seal before 1.6.0 over `plan(slices)`: `requires = []`, no
        /// `ended_after`.
        fn before_1_6_0(slices: i64) -> Fixture {
            let entries = plan(slices);
            Fixture {
                requires: Some("[]"),
                file_count: entries.len() as i64,
                ended_after: None,
                entries,
            }
        }

        /// The seal's bytes, in §4.1's grammar and key order.
        fn text(&self) -> String {
            let mut s = String::from(
                "================================================================\n\
                 TAPECTL SEAL MARKER\n\
                 ================================================================\n\n\
                 Volume: SS01\n\n\
                 [seal]\n\
                 volume = \"SS01\"\n\
                 layout_version = 2\n",
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
                layout_version: 2,
                file_count: self.file_count as i32,
                ended_after: self.ended_after.map(|k| k as u64),
                sealed_at: "2026-10-20T00:00:00Z".into(),
                front_index_sha256: front_index_sha256(),
                requires: match self.requires {
                    Some(SHORT) => vec![SHORT_SEAL.to_string()],
                    _ => Vec::new(),
                },
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
        assert!(seal.requires_short_seal());
        assert_eq!(seal.refusal(), None);
        assert_eq!(seal.check_parse_rules(), Ok(()));
        assert_eq!(seal.ending(13), SealEnding::Full);
    }

    /// §4.1's short seal: entries 0 to K and its own at K+1, `file_count =
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
    /// whatever its `requires` says — with `requires = []`, and with no
    /// `requires` line at all. When it keeps the rules, it parses.
    #[test]
    fn s1_holds_whatever_requires_says() {
        for requires in [Some("[]"), None] {
            let good = Fixture {
                requires,
                ..Fixture::full(4)
            };
            let seal = parse_seal_marker(&good.text()).expect("S1 holds: it parses");
            assert_eq!(seal.ended_after, Some(11));
            assert!(!seal.requires_short_seal());

            Fixture {
                requires,
                ended_after: Some(7),
                ..Fixture::full(4)
            }
            .refused_naming("rule S1");
        }
    }

    /// Rule S2: a seal whose `requires` names `short-seal` states
    /// `ended_after`.
    #[test]
    fn s2_a_seal_requiring_short_seal_states_ended_after() {
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

    /// Rule S3, and the compatibility it keeps: a seal before 1.6.0 (no
    /// `ended_after`, `requires = []` or no `requires` at all) parses as a
    /// full seal that ended after `file_count - 2`. It is not held to the
    /// embedded copy's rules at parse, as no reader before 1.6.0 held it;
    /// [`ParsedSeal::check_parse_rules`] judges it when a caller asks.
    #[test]
    fn s3_a_seal_before_1_6_0_still_parses_as_full() {
        for requires in [Some("[]"), None] {
            let f = Fixture {
                requires,
                ..Fixture::before_1_6_0(4)
            };
            let seal = parse_seal_marker(&f.text()).expect("a 1.5.0 seal parses");
            assert_eq!(seal, f.fields());
            assert_eq!(seal.ended_after, None);
            assert_eq!(seal.effective_ended_after(), Some(11));
            assert_eq!(seal.ending(13), SealEnding::Full);
            assert_eq!(seal.refusal(), None);
            assert_eq!(seal.check_parse_rules(), Ok(()), "S3 gives S5 its position");
        }

        let hollow = Fixture {
            entries: plan(4)[..5].to_vec(),
            ..Fixture::before_1_6_0(4)
        };
        let seal = parse_seal_marker(&hollow.text()).expect("parses as before 1.6.0");
        assert!(matches!(
            seal.check_parse_rules(),
            Err(SealRuleViolation::EntryCount {
                entries: 5,
                file_count: 13
            })
        ));
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

    /// A span tape's seal (ADR-0012 items 124 and 126): `requires =
    /// ["short-seal", "spanning"]`, and the 1.6.0 grammar and rules otherwise.
    /// Full and short, it parses and keeps S1-S6; it classifies against the
    /// plan as a 1.6.0 seal does; breaking S1, S2 or S5 makes it unparseable
    /// exactly as it makes a 1.6.0 seal; and `judge_seal` reads a good one as
    /// sealed. The same seals stating `short-seal` alone parse to the same
    /// fields but the feature.
    #[test]
    fn a_seal_stating_both_features_keeps_s1_to_s6() {
        const SPAN: &str = "[\"short-seal\", \"spanning\"]";
        let both = vec![SHORT_SEAL.to_string(), SPANNING.to_string()];
        let full = Fixture {
            requires: Some(SPAN),
            ..Fixture::full(4)
        };
        let short = Fixture {
            requires: Some(SPAN),
            ..Fixture::short(4, 9)
        };
        assert!(
            full.text().contains("\nrequires = [\"short-seal\", \"spanning\"]\nfile_count = 13\n"),
            "{}",
            full.text()
        );
        for (f, ending) in [
            (&full, SealEnding::Full),
            (&short, SealEnding::Short { ended_after: 9 }),
        ] {
            let seal = parse_seal_marker(&f.text()).expect("a span seal parses");
            assert_eq!(seal.requires, both);
            assert!(seal.requires_short_seal());
            assert_eq!(seal.refusal(), None);
            assert_eq!(seal.check_parse_rules(), Ok(()));
            assert_eq!(seal.ending(13), ending);
            let one = parse_seal_marker(
                &Fixture {
                    requires: Some(SHORT),
                    file_count: f.file_count,
                    ended_after: f.ended_after,
                    entries: f.entries.clone(),
                }
                .text(),
            )
            .unwrap();
            assert_eq!(
                ParsedSeal {
                    requires: both.clone(),
                    ..one
                },
                seal,
                "the same seal but the feature"
            );
        }
        let SealVerdict::Sealed(seal) =
            judge_seal(Ok(short.text().as_bytes()), FRONT_INDEX, 3, 10)
        else {
            panic!("a span short seal that keeps the rules is Sealed")
        };
        assert_eq!(seal.ended_after, Some(9));

        Fixture {
            ended_after: Some(8),
            ..Fixture {
                requires: Some(SPAN),
                ..Fixture::short(4, 9)
            }
        }
        .refused_naming("rule S1");
        Fixture {
            ended_after: None,
            ..Fixture {
                requires: Some(SPAN),
                ..Fixture::full(4)
            }
        }
        .refused_naming("rule S2");
        Fixture {
            requires: Some(SPAN),
            ..Fixture::short(4, 7)
        }
        .refused_naming("rule S5");
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

    /// §1.2 rule 1 comes before §4.1's rules: a seal requiring a feature this
    /// tapectl does not know, beside `short-seal`, parses uninterpreted —
    /// though it breaks S2 — so it is refused by name and not read as
    /// unsealed. The same for another layout version.
    #[test]
    fn a_refused_seal_is_returned_uninterpreted_before_the_rules() {
        let f = Fixture {
            requires: Some("[\"short-seal\", \"unknown-feature\"]"),
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
        .replace("layout_version = 2", "layout_version = 3");
        let seal = parse_seal_marker(&other_layout).expect("parses, uninterpreted");
        assert!(seal.refusal().unwrap().contains("layout_version = 3"));
    }

    /// §4.1 "Classification", the pure function: equal to the plan's count
    /// is full, less is short (S3's `file_count - 2` for a seal stating no
    /// `ended_after`), greater means the two ends disagree, and so does a
    /// count that leaves no file before the seal.
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

        let old = Fixture::before_1_6_0(4).fields();
        assert_eq!(classify_seal_ending(&old, 13), SealEnding::Full);
        assert_eq!(
            classify_seal_ending(&old, 15),
            SealEnding::Short { ended_after: 11 }
        );

        for file_count in [1, 0, -3] {
            let odd = ParsedSeal {
                file_count,
                ended_after: None,
                ..Fixture::before_1_6_0(4).fields()
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

    /// §3: File 3's `[index]` states `may_end_early = true` from 1.6.0, the
    /// line after `layout_version`, and the parser exposes it; the entries
    /// read as before.
    #[test]
    fn a_front_index_declaring_may_end_early_reads_true() {
        let text = "\
This tape may end early. The list below is the plan: every file the
write meant to put on the tape.

[index]
volume = \"SS01\"
layout_version = 2
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

    /// A front index before 1.6.0 has no `may_end_early`: absent is false,
    /// and it parses as it always has. A value that is not a bool is a
    /// parse failure, as any malformed key is.
    #[test]
    fn a_front_index_without_may_end_early_reads_false() {
        let text = "[index]\nvolume = \"OLD01\"\nlayout_version = 2\n\n[[files]]\nposition = 0\ntype = \"id_thunk\"\nsize_bytes = 10\nsha256_encrypted = \"h0\"\n";
        let doc = parse_front_index_document(text).expect("a 1.5.0 File 3 parses");
        assert!(!doc.may_end_early);
        assert_eq!(doc.volume, "OLD01");
        assert_eq!(doc.files.len(), 1);

        let wrong = text.replace(
            "layout_version = 2\n",
            "layout_version = 2\nmay_end_early = 1\n",
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
    /// (`volume-format-v2.md` §3): every file has size+hash except the front
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
        assert_eq!(parsed.layout_version, 2);
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
    /// (`docs/design/volume-format-v2.md` §1.1): absent means UNKNOWN and
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

    /// The writer's `0` is "unknown" (`volume-format-v2.md` §1.1) and must
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
    /// with no `[[files]]` breaks §4.1's rules (S1: `ended_after = -2`; S4:
    /// no entries) and is unparseable, rule S6: the unsealed signal, an
    /// `Err` and never a panic.
    #[test]
    fn a_1_6_0_seal_with_no_files_is_unparseable_not_a_panic() {
        let generated = generate_seal_marker("EMPTY01", 0, "deadbeef", &[]);
        let err = parse_seal_marker(&generated).unwrap_err().to_string();
        assert!(err.contains("rule S1"), "{err}");
    }

    /// The reader's tolerance of a seal before 1.6.0 with no `[[files]]`
    /// tables at all (the `files` key absent) is unchanged: an empty list,
    /// not an error. A literal 1.5.0 seal.
    #[test]
    fn a_1_5_0_seal_with_no_files_parses_as_empty_not_an_error() {
        let old = "[seal]\nvolume = \"EMPTY01\"\nlayout_version = 2\nrequires = []\n\
                   file_count = 0\nsealed_at = \"2026-10-06T00:00:00Z\"\n\
                   front_index_sha256 = \"deadbeef\"\n";
        let parsed = parse_seal_marker(old).expect("parses");
        assert!(parsed.files.is_empty());
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
