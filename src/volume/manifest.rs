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
//! whether it did. The writer always states it; the reader takes it as a bool,
//! false when absent (every envelope before 1.6.0), and it decides nothing.
//!
//! `tape_position` is `i64` on both sides now — the reader's original type.
//! It is the file number on tape and is NOT the slice number: slices begin
//! after the envelopes (position 8 on a typical layout), and the two diverge
//! further on a multi-unit volume. Confusing them yields a broken archive —
//! see the `#72` retrieval-guide note.

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
    pub units: Vec<ManifestUnit>,
}

/// One `[[units]]` block and its slices.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct ManifestUnit {
    pub name: String,
    pub uuid: String,
    pub snapshot_version: i64,
    pub stage_set_id: i64,
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
    pub tape_position: i64,
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
    /// The `[manifest]` table's last key is `may_end_early = true`, on every
    /// envelope tapectl 1.6.0 or later writes (`volume-format-v2.md` §1.7):
    /// it is a fact of the writer, so it is not a field of [`Manifest`].
    pub fn to_toml(&self) -> String {
        let label = &self.volume;
        let tenant_name = &self.tenant;
        let now = &self.created_at;
        let mut s = format!(
            r#"[manifest]
volume = "{label}"
tenant = "{tenant_name}"
created_at = "{now}"
may_end_early = true

"#
        );

        for unit in &self.units {
            s.push_str(&format!(
                "[[units]]\nname = \"{}\"\nuuid = \"{}\"\nsnapshot_version = {}\nstage_set_id = {}\n",
                unit.name, unit.uuid, unit.snapshot_version, unit.stage_set_id,
            ));
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
                s.push_str(&format!(
                    "[[units.slices]]\nnumber = {}\ntape_position = {}\nsize_bytes = {}\nencrypted_bytes = {}\nsha256_plain = \"{}\"\nsha256_encrypted = \"{}\"\n\n",
                    slice.number, slice.tape_position, slice.size_bytes,
                    slice.encrypted_bytes, slice.sha256_plain, slice.sha256_encrypted,
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
            units: doc.units,
        })
    }

    /// Whether an envelope `MANIFEST.toml` states `[manifest] may_end_early =
    /// true` (`volume-format-v2.md` §1.7): every envelope of a 1.6.0 tape.
    /// False when the key is absent. It decides nothing: a manifest's
    /// `tape_position`s are the plan, and the seal's `ended_after` says which
    /// of them are on the tape (§4.2).
    pub fn declares_may_end_early(text: &str) -> Result<bool> {
        Ok(parse_doc(text)?.manifest.may_end_early)
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
                dar_version: Some("2.7.20".to_string()),
                dar_command: Some(r#"dar -c "x" -s 10G \ path"#.to_string()),
                slices: vec![
                    ManifestSlice {
                        number: 1,
                        tape_position: 9,
                        size_bytes: 100,
                        encrypted_bytes: 120,
                        sha256_plain: "aa".repeat(32),
                        sha256_encrypted: "bb".repeat(32),
                    },
                    ManifestSlice {
                        number: 2,
                        tape_position: 10,
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
                dar_version: None,
                dar_command: None,
                slices: vec![ManifestSlice {
                    number: 1,
                    tape_position: 11,
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
            units: two_units(),
        };
        let current = m.to_toml();
        let before = current.replace("may_end_early = true\n", "");
        assert_ne!(before, current, "the line was not there to strip");
        assert_eq!(
            Manifest::from_toml(&before).expect("a 1.5.0 manifest parses"),
            m
        );
        assert!(!Manifest::declares_may_end_early(&before).unwrap());

        let stated_false = current.replace("may_end_early = true", "may_end_early = false");
        assert_eq!(Manifest::from_toml(&stated_false).unwrap(), m);
        assert!(!Manifest::declares_may_end_early(&stated_false).unwrap());

        let not_a_bool = current.replace("may_end_early = true", "may_end_early = 1");
        assert!(Manifest::from_toml(&not_a_bool).is_err());
        assert!(Manifest::declares_may_end_early(&not_a_bool).is_err());
    }
}
