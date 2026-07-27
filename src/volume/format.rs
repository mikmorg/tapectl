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
    pub sealed_at: String,
    pub front_index_sha256: String,
    pub files: Vec<ParsedIndexEntry>,
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
    #[allow(dead_code)]
    volume: String,
    #[allow(dead_code)]
    layout_version: i64,
}

#[derive(Debug, Deserialize)]
struct FrontIndexDoc {
    #[allow(dead_code)]
    index: IndexHeader,
    #[serde(default)]
    files: Vec<FileEntryToml>,
}

#[derive(Debug, Deserialize)]
struct SealHeader {
    volume: String,
    layout_version: i64,
    file_count: i64,
    sealed_at: String,
    front_index_sha256: String,
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
    let body = toml_body(raw, "[index]", "front index")?;
    let doc: FrontIndexDoc = toml::from_str(body)
        .map_err(|e| TapectlError::Other(format!("front index: TOML parse failed: {e}")))?;
    Ok(doc.files.into_iter().map(Into::into).collect())
}

/// Parse the plaintext seal marker (File M), including its embedded full
/// copy of the front index. Absent/malformed input is a normal `Err` — per
/// the fail-safe reader precedence (`docs/design/v2-open-questions.md`
/// §2.5), an unparseable seal marker is treated exactly like an absent one:
/// the tape reads as unsealed, never as a crash.
pub fn parse_seal_marker(raw: &str) -> Result<ParsedSeal> {
    let body = toml_body(raw, "[seal]", "seal marker")?;
    let doc: SealDoc = toml::from_str(body)
        .map_err(|e| TapectlError::Other(format!("seal marker: TOML parse failed: {e}")))?;
    Ok(ParsedSeal {
        volume: doc.seal.volume,
        layout_version: doc.seal.layout_version,
        file_count: doc.seal.file_count as i32,
        sealed_at: doc.seal.sealed_at,
        front_index_sha256: doc.seal.front_index_sha256,
        files: doc.files.into_iter().map(Into::into).collect(),
    })
}

/// The two identity fields of the ID thunk (File 0) that resume compares
/// against the Layout it is about to continue — `docs/design/layout-session.md`:
/// "rewind, read file 0, require ID-thunk identity match (label + uuid) —
/// mismatch = divergence = quarantine, not overwrite." Everything else in
/// the ID thunk (`layout::generate_id_thunk_v2`'s `magic`, `layout_version`,
/// `tapectl_version`, capacities, `created_at`, and the whole `[layout]`/
/// `[media]` tables) is either provisional or not part of this specific
/// check, so this struct deliberately carries only what resume needs.
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

    #[test]
    fn empty_files_list_parses_as_empty_not_an_error() {
        // generate_seal_marker with an empty slice emits no [[files]] tables
        // at all — the `files` key is entirely absent from the document.
        let generated = generate_seal_marker("EMPTY01", 0, "deadbeef", &[]);
        let parsed = parse_seal_marker(&generated).expect("parses");
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
