//! The on-tape envelope `MANIFEST.toml` — one type, both directions
//! (`docs/design/volume-format-v3.md` §1.9).
//!
//! [`Manifest`] is the single struct the writer and the reader use:
//! [`Manifest::to_toml`] is the writer (the literal bytes that land on tape)
//! and [`Manifest::from_toml`] is the reader (serde, reading on past a key it
//! does not know, §1.2 rule 3, #134).
//!
//! The `[manifest]` table ends with `may_end_early = true` (§1.8): every
//! Layout 3 tape may end before the last file its plan lists, and only the
//! seal says whether it did. It decides nothing.
//!
//! **A staging's block** (D6; ADR-0012 item 294): `name`, `uuid`,
//! `snapshot_version`, `stage_set_id`, then `payload = "tar"`,
//! `stream_bytes`, the names of the staging's file table and id map in the
//! envelope (`file_table`, `id_map`; in a tenant envelope only, §1.6),
//! `tar_version` (the writer's `tar --version` first line, for a reader's
//! diagnosis only) and `compression` (only for a compressed staging, §3.5).
//! The slice list lists EVERY slice of the staging, 1 to N, and gives
//! `tape_position` only to the slices this tape's plan holds: a slice the
//! tape does not plan has no `tape_position` line at all
//! ([`ManifestSlice::tape_position`] is `None`; §5.3, item 288). A slice
//! that continues a member begun in the slice before it states `continues =
//! true` as its last key (D22); no other slice states it. No key marks a
//! staging that may continue on another cartridge: every staging may
//! (item 288), and the positions and the seal say what this tape holds.
//!
//! `tape_position` is the file number on tape and is NOT the slice number:
//! slices begin after the envelopes, and the two diverge further on a
//! multi-unit volume.

use serde::Deserialize;

use crate::error::{Result, TapectlError};
use crate::volume::format::toml_string;

/// `payload`'s one value (D6): a staging's slices are cuts of a pax stream.
pub const PAYLOAD_TAR: &str = "tar";

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
    /// `[manifest] may_end_early` (§1.8): the tape this envelope is on may
    /// end before the last file its plan lists. True on every envelope
    /// tapectl writes; false when the key is absent, and [`Manifest::to_toml`]
    /// then writes no line. It decides nothing: the `tape_position`s are the
    /// plan, and the seal's `ended_after` says which of them are on the tape
    /// (§5.1).
    pub may_end_early: bool,
    pub units: Vec<ManifestUnit>,
}

/// One `[[units]]` block (a staging) and its slices (§1.9, D6).
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct ManifestUnit {
    pub name: String,
    pub uuid: String,
    pub snapshot_version: i64,
    pub stage_set_id: i64,
    /// `payload = "tar"` ([`PAYLOAD_TAR`]).
    pub payload: String,
    /// The length of the staging's tar stream in bytes, its tail included.
    pub stream_bytes: i64,
    /// The name of the staging's file table in this envelope
    /// (`tables/<uuid8>_v<version>_s<stage_set_id>.tsv`, §1.6). Stated in a
    /// tenant envelope only; the operator envelope carries the table as
    /// `catalog.db` rows and omits the key (§1.4).
    #[serde(default)]
    pub file_table: Option<String>,
    /// The name of the staging's id map in this envelope (`….ids.tsv`), on
    /// the same terms as [`ManifestUnit::file_table`].
    #[serde(default)]
    pub id_map: Option<String>,
    /// The writer's `tar --version` first line, for a reader's diagnosis
    /// only. Absent when the catalog recorded none.
    #[serde(default)]
    pub tar_version: Option<String>,
    /// The algorithm each slice was compressed with (`gzip`, `xz`, `zstd`),
    /// stated only for a compressed staging (§3.5).
    #[serde(default)]
    pub compression: Option<String>,
    #[serde(default)]
    pub slices: Vec<ManifestSlice>,
}

/// One `[[units.slices]]` block.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct ManifestSlice {
    pub number: i64,
    /// The tape file this slice is planned at, on THIS tape. `None` for a
    /// slice of the staging this tape does not plan: the line is then
    /// absent from the bytes (§1.9).
    #[serde(default)]
    pub tape_position: Option<i64>,
    /// The plaintext length of the slice's cut, before compression (§3.4).
    pub size_bytes: i64,
    /// The tape file's length.
    pub encrypted_bytes: i64,
    /// Of the cut. Present ONLY here: the plaintext front index may never
    /// carry it.
    pub sha256_plain: String,
    pub sha256_encrypted: String,
    /// The slice continues a member begun in the slice before it: the
    /// second and later slices of a long member's run (D22, §3.3). False
    /// when absent, and then [`Manifest::to_toml`] writes no line.
    #[serde(default)]
    pub continues: bool,
}

/// The names of a staging's file table and id map in a tenant envelope
/// (§1.6, ADR-0012 item 294): `tables/<uuid8>_v<version>_s<stage_set_id>.tsv`
/// and `….ids.tsv`, `uuid8` the unit uuid's first eight characters. Two
/// stagings of one version on one tape never collide.
pub fn table_names(unit_uuid: &str, version: i64, stage_set_id: i64) -> (String, String) {
    let uuid8 = unit_uuid.get(..8).unwrap_or(unit_uuid);
    let stem = format!("{TABLES_DIR}{uuid8}_v{version}_s{stage_set_id}");
    (format!("{stem}.tsv"), format!("{stem}.ids.tsv"))
}

/// The envelope directory the file tables and id maps are members of.
pub const TABLES_DIR: &str = "tables/";

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
    #[serde(default)]
    may_end_early: bool,
}

/// The whole `MANIFEST.toml` document shape, for parsing only.
///
/// Deliberately NOT `#[serde(deny_unknown_fields)]`: a reader reads on past
/// a key it does not know (`volume-format-v3.md` §1.2 rule 3, #134). Pinned
/// by a test.
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
    /// land on tape, where §1.9 fixes the order. Do not replace this with a
    /// serializer. Every string is a TOML basic string
    /// ([`crate::volume::format::toml_string`]).
    pub fn to_toml(&self) -> String {
        let mut s = format!(
            "[manifest]\nvolume = {}\ntenant = {}\ncreated_at = {}\n",
            toml_string(&self.volume),
            toml_string(&self.tenant),
            toml_string(&self.created_at),
        );
        if self.may_end_early {
            s.push_str("may_end_early = true\n");
        }
        s.push('\n');

        for unit in &self.units {
            s.push_str(&format!(
                "[[units]]\nname = {}\nuuid = {}\nsnapshot_version = {}\nstage_set_id = {}\n\
                 payload = {}\nstream_bytes = {}\n",
                toml_string(&unit.name),
                toml_string(&unit.uuid),
                unit.snapshot_version,
                unit.stage_set_id,
                toml_string(&unit.payload),
                unit.stream_bytes,
            ));
            for (key, value) in [
                ("file_table", &unit.file_table),
                ("id_map", &unit.id_map),
                ("tar_version", &unit.tar_version),
                ("compression", &unit.compression),
            ] {
                if let Some(value) = value {
                    s.push_str(&format!("{key} = {}\n", toml_string(value)));
                }
            }
            s.push('\n');
            for slice in &unit.slices {
                s.push_str(&format!("[[units.slices]]\nnumber = {}\n", slice.number));
                if let Some(position) = slice.tape_position {
                    s.push_str(&format!("tape_position = {position}\n"));
                }
                s.push_str(&format!(
                    "size_bytes = {}\nencrypted_bytes = {}\nsha256_plain = {}\nsha256_encrypted = {}\n",
                    slice.size_bytes,
                    slice.encrypted_bytes,
                    toml_string(&slice.sha256_plain),
                    toml_string(&slice.sha256_encrypted),
                ));
                if slice.continues {
                    s.push_str("continues = true\n");
                }
                s.push('\n');
            }
        }

        s
    }

    /// Parse an envelope `MANIFEST.toml`, reading on past a key it does not
    /// know (see [`ManifestDoc`]).
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
    /// true` (§1.8). It decides nothing.
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

    fn slice(number: i64, tape_position: Option<i64>, continues: bool) -> ManifestSlice {
        ManifestSlice {
            number,
            tape_position,
            size_bytes: 100 + number,
            encrypted_bytes: 120 + number,
            sha256_plain: format!("{:02}", number).repeat(32),
            sha256_encrypted: format!("{:02}", 50 + number).repeat(32),
            continues,
        }
    }

    /// A tenant envelope's two blocks: a compressed staging whose slices 2
    /// and 3 continue a long member, and an uncompressed one. The first
    /// carries a quote and a backslash in its name and its tar line (the
    /// TOML escaping trap).
    fn two_units() -> Vec<ManifestUnit> {
        let (t1, m1) = table_names("11111111-2222-3333-4444-555555555555", 7, 42);
        let (t2, m2) = table_names("66666666-7777-8888-9999-000000000000", 1, 43);
        vec![
            ManifestUnit {
                name: r#"photos "2019" \ raw"#.to_string(),
                uuid: "11111111-2222-3333-4444-555555555555".to_string(),
                snapshot_version: 7,
                stage_set_id: 42,
                payload: PAYLOAD_TAR.to_string(),
                stream_bytes: 30720,
                file_table: Some(t1),
                id_map: Some(m1),
                tar_version: Some(r#"tar (GNU tar) 1.35 "x" \ y"#.to_string()),
                compression: Some("zstd".to_string()),
                slices: vec![
                    slice(1, Some(9), false),
                    slice(2, Some(10), true),
                    slice(3, Some(11), true),
                ],
            },
            ManifestUnit {
                name: "ledgers/fy24".to_string(),
                uuid: "66666666-7777-8888-9999-000000000000".to_string(),
                snapshot_version: 1,
                stage_set_id: 43,
                payload: PAYLOAD_TAR.to_string(),
                stream_bytes: 10240,
                file_table: Some(t2),
                id_map: Some(m2),
                tar_version: Some("tar (GNU tar) 1.35".to_string()),
                compression: None,
                slices: vec![slice(1, Some(12), false)],
            },
        ]
    }

    fn manifest(units: Vec<ManifestUnit>) -> Manifest {
        Manifest {
            volume: "GOLD01".to_string(),
            tenant: "alpha".to_string(),
            created_at: "2026-11-18T00:00:00+00:00".to_string(),
            may_end_early: true,
            units,
        }
    }

    /// (a) Populated round-trip: what is written parses back to exactly what
    /// was written, quotes and backslashes included.
    #[test]
    fn populated_manifest_round_trips() {
        let m = manifest(two_units());
        let text = m.to_toml();
        let parsed = Manifest::from_toml(&text).expect("round-trip must parse");
        assert_eq!(parsed, m);
    }

    /// (b) `volume-format-v3.md` §1.2 rule 3 (#134): a reader reads on past a
    /// key it does not know, in every table of the manifest, rather than
    /// reject the manifest. A later writer may add one.
    #[test]
    fn a_manifest_reads_on_past_a_key_it_does_not_know() {
        let m = manifest(two_units());
        let current = m.to_toml();
        let later = current
            .replace("[manifest]\n", "[manifest]\nfuture_key = 1\n")
            .replace("[[units]]\n", "[[units]]\nfuture_unit_key = \"x\"\n")
            .replace(
                "[[units.slices]]\n",
                "[[units.slices]]\nfuture_slice_key = true\n",
            );
        for key in ["future_key", "future_unit_key", "future_slice_key"] {
            assert!(later.contains(key), "{key} was not injected:\n{later}");
        }
        let parsed = Manifest::from_toml(&later).expect("unknown keys are read past");
        assert_eq!(parsed, m);
    }

    /// (c) A manifest with no units is well-formed (an empty tenant
    /// envelope), and must not be mistaken for a parse failure.
    #[test]
    fn an_empty_units_manifest_round_trips() {
        let m = manifest(vec![]);
        let parsed = Manifest::from_toml(&m.to_toml()).unwrap();
        assert_eq!(parsed, m);
        assert!(parsed.units.is_empty());
    }

    /// §1.8: the `[manifest]` table ends with the literal `may_end_early =
    /// true`, directly after `created_at`, then the blank line before
    /// `[[units]]`, on a manifest with units and on one without.
    #[test]
    fn the_manifest_table_ends_with_may_end_early() {
        let m = manifest(two_units());
        let text = m.to_toml();
        let head = "[manifest]\n\
                    volume = \"GOLD01\"\n\
                    tenant = \"alpha\"\n\
                    created_at = \"2026-11-18T00:00:00+00:00\"\n\
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
             created_at = \"2026-11-18T00:00:00+00:00\"\n\
             may_end_early = true\n\
             \n"
        );
    }

    /// `may_end_early` reads as a bool: absent or `false` is false (and a
    /// false flag writes no line), and a value that is not a bool is a parse
    /// failure, as any malformed key is.
    #[test]
    fn may_end_early_is_a_bool() {
        let m = manifest(two_units());
        let current = m.to_toml();
        let absent = current.replace("may_end_early = true\n", "");
        assert_ne!(absent, current, "the line was not there to strip");
        let unflagged = Manifest {
            may_end_early: false,
            ..m.clone()
        };
        assert_eq!(Manifest::from_toml(&absent).unwrap(), unflagged);
        assert!(!Manifest::declares_may_end_early(&absent).unwrap());
        assert_eq!(unflagged.to_toml(), absent);
        let not_a_bool = current.replace("may_end_early = true", "may_end_early = 1");
        assert!(Manifest::from_toml(&not_a_bool).is_err());
    }

    /// §1.9 written out by hand (D6, D22; item 294): a tenant envelope's
    /// block of a compressed staging that this tape holds slices 2 and 3 of
    /// (slice 1 is on another tape: no `tape_position` line), slice 3
    /// continuing slice 2's member, the keys in §1.9's order; and an
    /// operator envelope's block, which omits `file_table`, `id_map` and,
    /// uncompressed, `compression`. Both parse to exactly that and render
    /// back byte for byte. No line names `may_span`, `dar` or `catalogs/`.
    #[test]
    fn the_blocks_of_section_1_9_round_trip_byte_for_byte() {
        let text = format!(
            r#"[manifest]
volume = "SPAN01"
tenant = "alpha"
created_at = "2026-11-18T00:00:00+00:00"
may_end_early = true

[[units]]
name = "photos/2019"
uuid = "1a2b3c4d-2222-3333-4444-555555555555"
snapshot_version = 3
stage_set_id = 42
payload = "tar"
stream_bytes = 3145728
file_table = "tables/1a2b3c4d_v3_s42.tsv"
id_map = "tables/1a2b3c4d_v3_s42.ids.tsv"
tar_version = "tar (GNU tar) 1.35"
compression = "xz"

[[units.slices]]
number = 1
size_bytes = 1048576
encrypted_bytes = 1000200
sha256_plain = "{p1}"
sha256_encrypted = "{e1}"

[[units.slices]]
number = 2
tape_position = 9
size_bytes = 1048576
encrypted_bytes = 1000300
sha256_plain = "{p2}"
sha256_encrypted = "{e2}"

[[units.slices]]
number = 3
tape_position = 10
size_bytes = 1048576
encrypted_bytes = 1000400
sha256_plain = "{p3}"
sha256_encrypted = "{e3}"
continues = true

[[units]]
name = "ledgers/fy24"
uuid = "66666666-7777-8888-9999-000000000000"
snapshot_version = 1
stage_set_id = 43
payload = "tar"
stream_bytes = 10240
tar_version = "tar (GNU tar) 1.35"

[[units.slices]]
number = 1
tape_position = 11
size_bytes = 10240
encrypted_bytes = 10500
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

        let parsed = Manifest::from_toml(&text).expect("the blocks parse");
        let ranged = &parsed.units[0];
        assert_eq!(ranged.payload, PAYLOAD_TAR);
        assert_eq!(ranged.stream_bytes, 3_145_728);
        let (t, m) = table_names(&ranged.uuid, ranged.snapshot_version, ranged.stage_set_id);
        assert_eq!(ranged.file_table.as_deref(), Some(t.as_str()));
        assert_eq!(ranged.id_map.as_deref(), Some(m.as_str()));
        assert_eq!(ranged.compression.as_deref(), Some("xz"));
        assert_eq!(
            ranged
                .slices
                .iter()
                .map(|s| (s.number, s.tape_position, s.continues))
                .collect::<Vec<_>>(),
            vec![(1, None, false), (2, Some(9), false), (3, Some(10), true)],
            "an absent tape_position is None; an absent continues is false"
        );
        let operator = &parsed.units[1];
        assert_eq!((&operator.file_table, &operator.id_map), (&None, &None));
        assert_eq!(operator.compression, None);
        assert_eq!(parsed.to_toml(), text, "serialises back byte for byte");
        for gone in ["may_span", "dar", "catalogs/"] {
            assert!(!text.contains(gone), "{gone}: {text}");
        }
    }

    /// Every shape a run of a staging takes on its tapes (§5.3): the head of
    /// it, its tail, a middle tape, a tape holding all of it, and a tape that
    /// planned none. Each round-trips, and states one `tape_position` line
    /// per planned slice and none for the others.
    #[test]
    fn every_run_shape_round_trips() {
        for positions in [
            [Some(9), Some(10), None],
            [None, Some(8), Some(9)],
            [None, Some(8), None],
            [Some(9), Some(10), Some(11)],
            [None, None, None],
        ] {
            let mut unit = two_units()[0].clone();
            for (slice, position) in unit.slices.iter_mut().zip(positions) {
                slice.tape_position = position;
            }
            let m = manifest(vec![unit]);
            let text = m.to_toml();
            assert_eq!(Manifest::from_toml(&text).unwrap(), m, "{text}");
            assert_eq!(
                text.matches("\ntape_position = ").count(),
                positions.iter().flatten().count(),
                "{text}"
            );
            assert_eq!(text.matches("\ncontinues = true\n").count(), 2, "{text}");
        }
    }

    /// `continues` reads as a bool (absent or `false` is false, and renders
    /// no line); a value that is not a bool fails, as a `tape_position` that
    /// is not an integer does.
    #[test]
    fn continues_is_a_bool_and_tape_position_an_integer() {
        let m = manifest(two_units());
        let text = m.to_toml();
        let mut unflagged = m.clone();
        for s in &mut unflagged.units[0].slices {
            s.continues = false;
        }
        let stated_false = text.replace("continues = true", "continues = false");
        assert_ne!(stated_false, text);
        assert_eq!(Manifest::from_toml(&stated_false).unwrap(), unflagged);
        let absent = text.replace("continues = true\n", "");
        assert_eq!(Manifest::from_toml(&absent).unwrap(), unflagged);
        assert_eq!(unflagged.to_toml(), absent);

        let not_a_bool = text.replacen("continues = true", "continues = 1", 1);
        assert!(Manifest::from_toml(&not_a_bool).is_err());
        let not_an_int = text.replace("tape_position = 9\n", "tape_position = \"9\"\n");
        assert_ne!(not_an_int, text);
        assert!(Manifest::from_toml(&not_an_int).is_err());
    }

    /// §1.6's names (item 294): the uuid's first eight characters, the
    /// version and the stage set, so two stagings of one version differ.
    #[test]
    fn a_stagings_tables_are_named_by_uuid8_version_and_stage_set() {
        assert_eq!(
            table_names("1a2b3c4d-2222-3333-4444-555555555555", 3, 42),
            (
                "tables/1a2b3c4d_v3_s42.tsv".to_string(),
                "tables/1a2b3c4d_v3_s42.ids.tsv".to_string()
            )
        );
        assert_ne!(
            table_names("1a2b3c4d-2222-3333-4444-555555555555", 3, 42),
            table_names("1a2b3c4d-2222-3333-4444-555555555555", 3, 43)
        );
    }
}
