//! The on-tape envelope `MANIFEST.toml` — one type, both directions.
//!
//! Before this module, the writer (`layout::generate_manifest_toml`, a
//! hand-`format!`ed document over `layout::ManifestUnit`/`ManifestSlice`) and
//! the reader (`envelope::parse_manifest`, a serde `Deserialize` over a
//! SECOND, independently-maintained struct family) disagreed on a type:
//! the writer's `tape_position` was `i32`, the reader's `i64`. Issues
//! #134/#136/#137 each touched the reader with no writer-side type to
//! consult. [`Manifest`] is now the single struct both directions use:
//! [`Manifest::to_toml`] is the writer (the literal bytes that land on tape,
//! pinned byte-for-byte by `tests/on_tape_golden.rs`) and
//! [`Manifest::from_toml`] is the reader (serde, tolerant of unknown keys).
//!
//! From tapectl 1.6.0 the `[manifest]` table ends with `may_end_early = true`
//! (`docs/design/volume-format-v2.md` §1.7, ADR-0012 item 111): every 1.6.0
//! tape may end before the last file its plan lists, and only the seal says
//! whether it did. It is [`Manifest::may_end_early`]: the writer states the
//! line when it is true (every envelope 1.6.0 writes, set at
//! `layout::generate_manifest_toml`), the reader takes it as a bool, false
//! when absent (every envelope before 1.6.0), and it decides nothing.
//!
//! `tape_position` is `i64` on both sides now — the reader's original type.
//! It is the file number on tape and is NOT the slice number: slices begin
//! after the envelopes (position 8 on a typical layout), and the two diverge
//! further on a multi-unit volume. Confusing them yields a broken archive —
//! see the `#72` retrieval-guide note.
//!
//! From tapectl 1.7.0 a `[[units]]` block may describe a *range* (ADR-0012
//! items 121, 125): the run of one spannable stage set's slices on this
//! tape. Its block states `may_span = true` directly after `stage_set_id`,
//! lists EVERY slice of the set with its hashes, and gives `tape_position`
//! only to the slices this tape plans; a slice this tape does not plan has
//! no `tape_position` line at all ([`ManifestSlice::tape_position`] is
//! `None`). A whole unit's block states neither, and its bytes are the
//! 1.6.0 bytes exactly (`tests/on_tape_golden.rs`).

use serde::Deserialize;

use crate::error::{Result, TapectlError};

/// A parsed (or about-to-be-written) envelope `MANIFEST.toml`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Manifest {
    /// Volume label, as the writing tapectl knew it.
    pub volume: String,
    /// Tenant name — the real tenant for a tenant envelope, and the literal
    /// `"operator"` for the operator envelope and its backup, which list
    /// every unit across all tenants rather than belonging to one.
    pub tenant: String,
    pub created_at: String,
    /// `[manifest] may_end_early` (`volume-format-v2.md` §1.7): the tape this
    /// envelope is on may end before the last file its plan lists. True on
    /// every envelope tapectl 1.6.0 or later writes; false when the key is
    /// absent (every envelope before 1.6.0), and [`Manifest::to_toml`] then
    /// writes no line, so a false manifest renders the 1.5.0 bytes. It decides
    /// nothing: the `tape_position`s are the plan, and the seal's
    /// `ended_after` says which of them are on the tape (§4.2).
    pub may_end_early: bool,
    pub units: Vec<ManifestUnit>,
}

/// One `[[units]]` block and its slices.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct ManifestUnit {
    pub name: String,
    pub uuid: String,
    pub snapshot_version: i64,
    pub stage_set_id: i64,
    /// `may_span = true` (ADR-0012 items 124-125): this block is a spannable
    /// stage set's, so it lists every slice of the set and the tape may hold
    /// only a range of them. Frozen when the write is planned. False when
    /// the key is absent (every whole unit, and every block before 1.7.0),
    /// and [`Manifest::to_toml`] then writes no line, so a whole unit's block
    /// renders the 1.6.0 bytes.
    #[serde(default)]
    pub may_span: bool,
    #[serde(default)]
    pub dar_version: Option<String>,
    #[serde(default)]
    pub dar_command: Option<String>,
    #[serde(default)]
    pub slices: Vec<ManifestSlice>,
}

/// One `[[units.slices]]` block.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct ManifestSlice {
    pub number: i64,
    /// The tape file this slice is planned at, on THIS tape. `None` only in
    /// a `may_span` block, for a slice of the set this tape does not plan
    /// (ADR-0012 item 125): the line is then absent from the bytes. Every
    /// slice of a whole unit has one.
    #[serde(default)]
    pub tape_position: Option<i64>,
    pub size_bytes: i64,
    pub encrypted_bytes: i64,
    /// Present ONLY here. The on-tape `catalog.db` omits `sha256_plain`, and
    /// the plaintext front index may never carry it.
    pub sha256_plain: String,
    pub sha256_encrypted: String,
}

/// The `[manifest]` header table, as it appears on the wire. A private
/// mirror of [`Manifest`]'s flat header fields — [`Manifest`] itself is not
/// `Deserialize` because its shape (flat) does not match the nested TOML
/// document (`[manifest]` table + top-level `[[units]]` array); this struct
/// and [`ManifestDoc`] below do that mapping once, in [`Manifest::from_toml`].
#[derive(Debug, Deserialize)]
struct ManifestHeaderDoc {
    volume: String,
    tenant: String,
    created_at: String,
    /// From 1.6.0 (§1.7). Absent means false.
    #[serde(default)]
    may_end_early: bool,
}

/// The whole `MANIFEST.toml` document shape, for parsing only.
///
/// Deliberately NOT `#[serde(deny_unknown_fields)]`: tapes written before
/// #134 carry a `layout_version = 1` key in `[manifest]` that no longer
/// exists, and a rebuild must still read those tapes. Pinned by a test.
#[derive(Debug, Deserialize)]
struct ManifestDoc {
    manifest: ManifestHeaderDoc,
    #[serde(default)]
    units: Vec<ManifestUnit>,
}

impl Manifest {
    /// Render as the on-tape `MANIFEST.toml` bytes.
    ///
    /// Hand-formatted, not `toml::to_string`: a generic TOML serializer
    /// would be free to reorder keys or reformat whitespace, and these bytes
    /// land on tape — `tests/on_tape_golden.rs` pins them exactly (modulo
    /// the one `created_at` line). Do not replace this with a serializer.
    ///
    /// The `[manifest]` table's last key is `may_end_early = true` when
    /// [`Manifest::may_end_early`] is set, as it is on every envelope tapectl
    /// 1.6.0 or later writes (`volume-format-v2.md` §1.7), directly after
    /// `created_at` and before the blank line that precedes `[[units]]`.
    ///
    /// A range's block (ADR-0012 item 125) states `may_span = true` directly
    /// after `stage_set_id`, and a slice whose `tape_position` is `None` has
    /// no `tape_position` line. Neither line appears in a whole unit's block.
    pub fn to_toml(&self) -> String {
        let label = &self.volume;
        let tenant_name = &self.tenant;
        let now = &self.created_at;
        let may_end_early = if self.may_end_early {
            "may_end_early = true\n"
        } else {
            ""
        };
        let mut s = format!(
            r#"[manifest]
volume = "{label}"
tenant = "{tenant_name}"
created_at = "{now}"
{may_end_early}
"#
        );

        for unit in &self.units {
            s.push_str(&format!(
                "[[units]]\nname = \"{}\"\nuuid = \"{}\"\nsnapshot_version = {}\nstage_set_id = {}\n",
                unit.name, unit.uuid, unit.snapshot_version, unit.stage_set_id,
            ));
            if unit.may_span {
                s.push_str("may_span = true\n");
            }
            if let Some(ref dar_ver) = unit.dar_version {
                s.push_str(&format!("dar_version = \"{dar_ver}\"\n"));
            }
            if let Some(ref cmd) = unit.dar_command {
                // TOML basic-string escape for the command line.
                let esc = cmd.replace('\\', "\\\\").replace('"', "\\\"");
                s.push_str(&format!("dar_command = \"{esc}\"\n"));
            }
            s.push('\n');
            for slice in &unit.slices {
                s.push_str(&format!("[[units.slices]]\nnumber = {}\n", slice.number));
                if let Some(position) = slice.tape_position {
                    s.push_str(&format!("tape_position = {position}\n"));
                }
                s.push_str(&format!(
                    "size_bytes = {}\nencrypted_bytes = {}\nsha256_plain = \"{}\"\nsha256_encrypted = \"{}\"\n\n",
                    slice.size_bytes,
                    slice.encrypted_bytes,
                    slice.sha256_plain,
                    slice.sha256_encrypted,
                ));
            }
        }

        s
    }

    /// Parse an envelope `MANIFEST.toml`.
    ///
    /// Deliberately tolerant of unknown keys (see [`ManifestDoc`]), and of
    /// an absent `may_end_early` (every envelope before 1.6.0).
    pub fn from_toml(text: &str) -> Result<Manifest> {
        let doc = parse_doc(text)?;
        Ok(Manifest {
            volume: doc.manifest.volume,
            tenant: doc.manifest.tenant,
            created_at: doc.manifest.created_at,
            may_end_early: doc.manifest.may_end_early,
            units: doc.units,
        })
    }

    /// Whether an envelope `MANIFEST.toml` states `[manifest] may_end_early =
    /// true` (`volume-format-v2.md` §1.7): every envelope of a 1.6.0 tape.
    /// False when the key is absent. It decides nothing: a manifest's
    /// `tape_position`s are the plan, and the seal's `ended_after` says which
    /// of them are on the tape (§4.2).
    pub fn declares_may_end_early(text: &str) -> Result<bool> {
        Ok(Self::from_toml(text)?.may_end_early)
    }
}

fn parse_doc(text: &str) -> Result<ManifestDoc> {
    toml::from_str(text)
        .map_err(|e| TapectlError::Other(format!("envelope MANIFEST.toml: parse failed: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn two_units() -> Vec<ManifestUnit> {
        vec![
            ManifestUnit {
                name: "photos/2019".to_string(),
                uuid: "11111111-2222-3333-4444-555555555555".to_string(),
                snapshot_version: 7,
                stage_set_id: 42,
                may_span: false,
                dar_version: Some("2.7.20".to_string()),
                dar_command: Some(r#"dar -c "x" -s 10G \ path"#.to_string()),
                slices: vec![
                    ManifestSlice {
                        number: 1,
                        tape_position: Some(9),
                        size_bytes: 100,
                        encrypted_bytes: 120,
                        sha256_plain: "aa".repeat(32),
                        sha256_encrypted: "bb".repeat(32),
                    },
                    ManifestSlice {
                        number: 2,
                        tape_position: Some(10),
                        size_bytes: 50,
                        encrypted_bytes: 70,
                        sha256_plain: "cc".repeat(32),
                        sha256_encrypted: "dd".repeat(32),
                    },
                ],
            },
            ManifestUnit {
                name: "ledgers/fy24".to_string(),
                uuid: "66666666-7777-8888-9999-000000000000".to_string(),
                snapshot_version: 1,
                stage_set_id: 43,
                may_span: false,
                dar_version: None,
                dar_command: None,
                slices: vec![ManifestSlice {
                    number: 1,
                    tape_position: Some(11),
                    size_bytes: 5,
                    encrypted_bytes: 25,
                    sha256_plain: "ee".repeat(32),
                    sha256_encrypted: "ff".repeat(32),
                }],
            },
        ]
    }

    /// (a) Populated round-trip: a manifest with two units — one carrying a
    /// `dar_command` with both a quote and a backslash (the TOML escaping
    /// trap), one without — parses back to exactly what was written.
    #[test]
    fn populated_manifest_round_trips() {
        let m = Manifest {
            volume: "GOLD01".to_string(),
            tenant: "alpha".to_string(),
            created_at: "2026-09-11T00:00:00+00:00".to_string(),
            may_end_early: true,
            units: two_units(),
        };
        let text = m.to_toml();
        let parsed = Manifest::from_toml(&text).expect("round-trip must parse");
        assert_eq!(parsed, m);
    }

    /// (b) Tapes written before #134 carry `layout_version = 1` in
    /// `[manifest]`. Those are exactly the tapes a disaster rebuild will be
    /// reading, so the parser must ignore the key rather than reject the
    /// manifest.
    #[test]
    fn a_manifest_from_before_the_layout_version_removal_still_parses() {
        let m = Manifest {
            volume: "LAB01".to_string(),
            tenant: "alice".to_string(),
            created_at: "2026-09-11T00:00:00+00:00".to_string(),
            may_end_early: true,
            units: two_units(),
        };
        let current = m.to_toml();
        let legacy = current.replace("[manifest]\n", "[manifest]\nlayout_version = 1\n");
        assert_ne!(legacy, current, "the legacy key was not injected");
        let parsed = Manifest::from_toml(&legacy).expect("a pre-#134 manifest must still parse");
        assert_eq!(parsed, m);
    }

    /// (c) A manifest with no units is well-formed (an empty tenant
    /// envelope), and must not be mistaken for a parse failure.
    #[test]
    fn an_empty_units_manifest_round_trips() {
        let m = Manifest {
            volume: "VOL001".to_string(),
            tenant: "alpha".to_string(),
            created_at: "2026-09-11T00:00:00+00:00".to_string(),
            may_end_early: true,
            units: vec![],
        };
        let parsed = Manifest::from_toml(&m.to_toml()).unwrap();
        assert_eq!(parsed, m);
        assert!(parsed.units.is_empty());
    }

    /// `volume-format-v2.md` §1.7 (ADR-0012 item 111): the `[manifest]`
    /// table ends with the literal `may_end_early = true`, directly after
    /// `created_at`, then the blank line before `[[units]]` as before — on a
    /// manifest with units and on one without.
    #[test]
    fn the_manifest_table_ends_with_may_end_early() {
        let m = Manifest {
            volume: "GOLD01".to_string(),
            tenant: "alpha".to_string(),
            created_at: "2026-09-11T00:00:00+00:00".to_string(),
            may_end_early: true,
            units: two_units(),
        };
        let text = m.to_toml();
        let head = "[manifest]\n\
                    volume = \"GOLD01\"\n\
                    tenant = \"alpha\"\n\
                    created_at = \"2026-09-11T00:00:00+00:00\"\n\
                    may_end_early = true\n\
                    \n\
                    [[units]]\n";
        assert!(text.starts_with(head), "{text}");
        assert_eq!(text.matches("may_end_early").count(), 1, "{text}");
        assert!(Manifest::declares_may_end_early(&text).unwrap());

        let empty = Manifest { units: vec![], ..m }.to_toml();
        assert_eq!(
            empty,
            "[manifest]\n\
             volume = \"GOLD01\"\n\
             tenant = \"alpha\"\n\
             created_at = \"2026-09-11T00:00:00+00:00\"\n\
             may_end_early = true\n\
             \n"
        );
    }

    /// A manifest from before 1.6.0 has no `may_end_early`: it parses to the
    /// same [`Manifest`], and declares false. An explicit false reads false;
    /// a value that is not a bool is a parse failure, as any malformed key is.
    #[test]
    fn a_manifest_without_may_end_early_still_parses() {
        let m = Manifest {
            volume: "LAB01".to_string(),
            tenant: "operator".to_string(),
            created_at: "2026-10-19T00:00:00+00:00".to_string(),
            may_end_early: true,
            units: two_units(),
        };
        let current = m.to_toml();
        let before = current.replace("may_end_early = true\n", "");
        assert_ne!(before, current, "the line was not there to strip");
        let unflagged = Manifest {
            may_end_early: false,
            ..m.clone()
        };
        assert_eq!(
            Manifest::from_toml(&before).expect("a 1.5.0 manifest parses"),
            unflagged
        );
        assert!(!Manifest::declares_may_end_early(&before).unwrap());
        // A false flag writes no line: the 1.5.0 bytes exactly.
        assert_eq!(unflagged.to_toml(), before);

        let stated_false = current.replace("may_end_early = true", "may_end_early = false");
        assert_eq!(Manifest::from_toml(&stated_false).unwrap(), unflagged);
        assert!(!Manifest::declares_may_end_early(&stated_false).unwrap());

        let not_a_bool = current.replace("may_end_early = true", "may_end_early = 1");
        assert!(Manifest::from_toml(&not_a_bool).is_err());
        assert!(Manifest::declares_may_end_early(&not_a_bool).is_err());
    }

    /// The slices of one spannable stage set (ADR-0012 item 125), with
    /// `tape_position` given to the slices `positions` names: one entry per
    /// slice, in slice order, `None` for a slice this tape does not plan.
    fn range_unit(positions: [Option<i64>; 3]) -> ManifestUnit {
        let hashes = [("11", "21"), ("12", "22"), ("13", "23")];
        ManifestUnit {
            name: "photos/2019".to_string(),
            uuid: "11111111-2222-3333-4444-555555555555".to_string(),
            snapshot_version: 7,
            stage_set_id: 42,
            may_span: true,
            dar_version: Some("2.7.20".to_string()),
            dar_command: Some(r#"dar -c "x" -s 10G \ path"#.to_string()),
            slices: (0..3)
                .map(|i| ManifestSlice {
                    number: i as i64 + 1,
                    tape_position: positions[i],
                    size_bytes: 100 + i as i64,
                    encrypted_bytes: 120 + i as i64,
                    sha256_plain: hashes[i].0.repeat(32),
                    sha256_encrypted: hashes[i].1.repeat(32),
                })
                .collect(),
        }
    }

    /// ADR-0012 item 125, written out by hand: a range's block states
    /// `may_span = true` directly after `stage_set_id` (#478 fork X6), lists
    /// EVERY slice of the set with its hashes, and gives `tape_position` only
    /// to the slices this tape plans; a slice this tape does not plan has no
    /// `tape_position` line. It parses to exactly that (an absent position is
    /// `None`) and renders back byte for byte. The whole unit after it keeps
    /// the 1.6.0 block: no `may_span` line, a position on every slice.
    #[test]
    fn a_range_block_round_trips_byte_for_byte() {
        let text = format!(
            r#"[manifest]
volume = "SPAN01"
tenant = "alpha"
created_at = "2026-10-21T00:00:00+00:00"
may_end_early = true

[[units]]
name = "photos/2019"
uuid = "11111111-2222-3333-4444-555555555555"
snapshot_version = 7
stage_set_id = 42
may_span = true
dar_version = "2.7.20"
dar_command = "dar -c \"x\" -s 10G \\ path"

[[units.slices]]
number = 1
tape_position = 9
size_bytes = 100
encrypted_bytes = 120
sha256_plain = "{p1}"
sha256_encrypted = "{e1}"

[[units.slices]]
number = 2
tape_position = 10
size_bytes = 101
encrypted_bytes = 121
sha256_plain = "{p2}"
sha256_encrypted = "{e2}"

[[units.slices]]
number = 3
size_bytes = 102
encrypted_bytes = 122
sha256_plain = "{p3}"
sha256_encrypted = "{e3}"

[[units]]
name = "ledgers/fy24"
uuid = "66666666-7777-8888-9999-000000000000"
snapshot_version = 1
stage_set_id = 43

[[units.slices]]
number = 1
tape_position = 11
size_bytes = 5
encrypted_bytes = 25
sha256_plain = "{p4}"
sha256_encrypted = "{e4}"

"#,
            p1 = "11".repeat(32),
            e1 = "21".repeat(32),
            p2 = "12".repeat(32),
            e2 = "22".repeat(32),
            p3 = "13".repeat(32),
            e3 = "23".repeat(32),
            p4 = "ee".repeat(32),
            e4 = "ff".repeat(32),
        );

        let parsed = Manifest::from_toml(&text).expect("a range block parses");
        let range = &parsed.units[0];
        assert!(range.may_span, "may_span parses");
        assert_eq!(range.slices.len(), 3, "every slice of the set is listed");
        assert_eq!(
            range
                .slices
                .iter()
                .map(|s| s.tape_position)
                .collect::<Vec<_>>(),
            vec![Some(9), Some(10), None],
            "an absent tape_position parses as None"
        );
        assert_eq!(range.slices[2].sha256_encrypted, "23".repeat(32));
        let whole = &parsed.units[1];
        assert!(!whole.may_span, "an absent may_span is false");
        assert_eq!(whole.slices[0].tape_position, Some(11));

        let expected = Manifest {
            volume: "SPAN01".to_string(),
            tenant: "alpha".to_string(),
            created_at: "2026-10-21T00:00:00+00:00".to_string(),
            may_end_early: true,
            units: vec![
                range_unit([Some(9), Some(10), None]),
                two_units()[1].clone(),
            ],
        };
        assert_eq!(parsed, expected);
        assert_eq!(parsed.to_toml(), text, "serialises back byte for byte");
        assert_eq!(text.matches("may_span").count(), 1, "{text}");
    }

    /// Every shape a range takes on its tapes (ADR-0012 items 122, 125):
    /// the head of the set, its tail, a middle tape that planned every slice
    /// (still `may_span`, since the cut is unknown at plan time), and a tape
    /// that planned none. Each round-trips, and states one `tape_position`
    /// line per planned slice and none for the others.
    #[test]
    fn every_range_shape_round_trips() {
        for positions in [
            [Some(9), Some(10), None],
            [None, Some(8), Some(9)],
            [None, Some(8), None],
            [Some(9), Some(10), Some(11)],
            [None, None, None],
        ] {
            let m = Manifest {
                volume: "SPAN01".to_string(),
                tenant: "operator".to_string(),
                created_at: "2026-10-21T00:00:00+00:00".to_string(),
                may_end_early: true,
                units: vec![range_unit(positions)],
            };
            let text = m.to_toml();
            assert_eq!(Manifest::from_toml(&text).unwrap(), m, "{text}");
            assert_eq!(
                text.matches("\ntape_position = ").count(),
                positions.iter().flatten().count(),
                "{text}"
            );
            assert_eq!(text.matches("\nmay_span = true\n").count(), 1, "{text}");
            assert!(
                text.contains("\nstage_set_id = 42\nmay_span = true\ndar_version = "),
                "may_span sits directly after stage_set_id: {text}"
            );
        }
    }

    /// A range block whose every slice is planned differs from the whole
    /// unit's block by the one `may_span = true` line and nothing else; with
    /// `may_span` false it IS the whole unit's block.
    #[test]
    fn a_fully_planned_range_differs_from_a_whole_block_by_one_line() {
        let range = Manifest {
            volume: "SPAN01".to_string(),
            tenant: "alpha".to_string(),
            created_at: "2026-10-21T00:00:00+00:00".to_string(),
            may_end_early: true,
            units: vec![range_unit([Some(9), Some(10), Some(11)])],
        };
        let mut whole = range.clone();
        whole.units[0].may_span = false;
        let whole_text = whole.to_toml();
        assert!(!whole_text.contains("may_span"), "{whole_text}");
        assert_eq!(
            range.to_toml(),
            whole_text.replace(
                "\nstage_set_id = 42\n",
                "\nstage_set_id = 42\nmay_span = true\n"
            )
        );
    }

    /// `may_span` reads as a bool: absent or `false` is false (and renders no
    /// line), and a value that is not a bool is a parse failure, as any
    /// malformed key is. A `tape_position` that is not an integer fails too.
    #[test]
    fn may_span_is_a_bool_and_tape_position_an_integer() {
        let m = Manifest {
            volume: "SPAN01".to_string(),
            tenant: "alpha".to_string(),
            created_at: "2026-10-21T00:00:00+00:00".to_string(),
            may_end_early: true,
            units: vec![range_unit([Some(9), Some(10), None])],
        };
        let text = m.to_toml();
        let mut unflagged = m.clone();
        unflagged.units[0].may_span = false;

        let stated_false = text.replace("may_span = true", "may_span = false");
        assert_ne!(stated_false, text);
        assert_eq!(Manifest::from_toml(&stated_false).unwrap(), unflagged);
        let absent = text.replace("may_span = true\n", "");
        assert_eq!(Manifest::from_toml(&absent).unwrap(), unflagged);
        assert_eq!(unflagged.to_toml(), absent);

        let not_a_bool = text.replace("may_span = true", "may_span = 1");
        assert!(Manifest::from_toml(&not_a_bool).is_err());
        let not_an_int = text.replace("tape_position = 9\n", "tape_position = \"9\"\n");
        assert_ne!(not_an_int, text);
        assert!(Manifest::from_toml(&not_an_int).is_err());
    }
}
