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
    /// True for the operator envelope and its backup, which carry every
    /// unit on the volume under the placeholder tenant `"operator"`.
    pub fn is_operator(&self) -> bool {
        self.tenant == "operator"
    }

    /// Render as the on-tape `MANIFEST.toml` bytes.
    ///
    /// Hand-formatted, not `toml::to_string`: a generic TOML serializer
    /// would be free to reorder keys or reformat whitespace, and these bytes
    /// land on tape — `tests/on_tape_golden.rs` pins them exactly (modulo
    /// the one `created_at` line). Do not replace this with a serializer.
    pub fn to_toml(&self) -> String {
        let label = &self.volume;
        let tenant_name = &self.tenant;
        let now = &self.created_at;
        let mut s = format!(
            r#"[manifest]
volume = "{label}"
tenant = "{tenant_name}"
created_at = "{now}"

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
    /// Deliberately tolerant of unknown keys (see [`ManifestDoc`]).
    pub fn from_toml(text: &str) -> Result<Manifest> {
        let doc: ManifestDoc = toml::from_str(text).map_err(|e| {
            TapectlError::Other(format!("envelope MANIFEST.toml: parse failed: {e}"))
        })?;
        Ok(Manifest {
            volume: doc.manifest.volume,
            tenant: doc.manifest.tenant,
            created_at: doc.manifest.created_at,
            units: doc.units,
        })
    }
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

    /// (d) The operator envelope and its backup are recognised by the
    /// placeholder tenant `"operator"`; anything else is not.
    #[test]
    fn is_operator_checks_the_placeholder_tenant() {
        let mut m = Manifest {
            volume: "VOL001".to_string(),
            tenant: "alpha".to_string(),
            created_at: "2026-09-11T00:00:00+00:00".to_string(),
            units: vec![],
        };
        assert!(!m.is_operator());
        m.tenant = "operator".to_string();
        assert!(m.is_operator());
    }
}
