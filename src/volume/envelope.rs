//! Reading a volume envelope back off tape.
//!
//! Envelopes are written by [`crate::volume::build`] as
//! `age(tar{MANIFEST.toml, RECOVERY.md, catalog-*.sql, PLAN.toml?, catalog.db?,
//! index/units.tsv?})`.
//! Until `catalog rebuild` (#136) nothing in Rust ever read one back — the
//! write path packed them and `RESTORE.sh` (bash) unpacked them, so the two
//! halves had never met in one language. This module is that other half.
//!
//! **The manifest is the authoritative slice map, not the front index.** The
//! plaintext front index carries position/type/size/`sha256_encrypted` and,
//! by the sacred invariant, cannot carry a unit name, a filename, a
//! `sha256_plain` or a key fingerprint. Everything needed to rebuild a
//! catalog therefore lives inside the envelope, authenticated by age.
//!
//! **The operator envelope is the access-control boundary, for free.** Its
//! recipient list is operator keys + escrow, never tenant keys
//! (`volume-format-v2.md` §1: `enc(op+esc)`), so "an operator or escrow key
//! is required" needs no key-type introspection anywhere — a tenant key
//! simply fails to open it, and [`OpenError::NoMatchingKey`] says so.

use std::fs::{self, File};
use std::io::{BufWriter, Read, Write};
use std::path::{Path, PathBuf};

use crate::error::{Result, TapectlError};
use crate::store::Store;
use crate::util::{HashingWriter, TruncatingWriter};
use crate::volume::format::ParsedIndexEntry;
pub use crate::volume::manifest::{ManifestSlice, ManifestUnit};

/// The `[manifest]` header of an envelope's `MANIFEST.toml`.
///
/// A local, nested mirror of the flat header fields on
/// [`crate::volume::manifest::Manifest`] (the single on-tape MANIFEST.toml
/// type, shared with the writer). Kept nested here — rather than flattened
/// onto [`EnvelopeManifest`] — because existing callers (`volume::rebuild`)
/// address it as `envelope.manifest.manifest.tenant`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ManifestHeader {
    /// Volume label, as the writing tapectl knew it.
    pub volume: String,
    /// Tenant name — the real tenant for a tenant envelope, and the literal
    /// `"operator"` for the operator envelope and its backup, which list
    /// every unit across all tenants rather than belonging to one.
    ///
    /// **Not a test of which envelope this is** (issue #459): a real tenant
    /// may be named `operator` too. The operator envelope is the one the
    /// front index types `operator_envelope` ([`OpenedEnvelope::is_operator`]).
    pub tenant: String,
    pub created_at: String,
}

/// A parsed envelope `MANIFEST.toml`.
///
/// A nested view over [`crate::volume::manifest::Manifest`], kept in this
/// shape (rather than a plain re-export of `Manifest`) for the existing
/// `.manifest.manifest.tenant`-style call sites in `volume::rebuild`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EnvelopeManifest {
    pub manifest: ManifestHeader,
    pub units: Vec<ManifestUnit>,
}

/// Parse an envelope `MANIFEST.toml`.
///
/// Thin wrapper over [`crate::volume::manifest::Manifest::from_toml`] that
/// reshapes the flat [`crate::volume::manifest::Manifest`] into the nested
/// [`EnvelopeManifest`] this module's callers expect.
pub fn parse_manifest(text: &str) -> Result<EnvelopeManifest> {
    let m = crate::volume::manifest::Manifest::from_toml(text)?;
    Ok(EnvelopeManifest {
        manifest: ManifestHeader {
            volume: m.volume,
            tenant: m.tenant,
            created_at: m.created_at,
        },
        units: m.units,
    })
}

/// An envelope opened off tape.
#[derive(Debug, Clone)]
pub struct OpenedEnvelope {
    pub position: i32,
    pub type_label: String,
    pub manifest: EnvelopeManifest,
    /// The operator envelope's `catalog.db` (issue #83), extracted to disk.
    /// `None` for tenant envelopes, which never carry it.
    pub catalog_db: Option<PathBuf>,
    /// The dar catalogue files (`catalogs/<uuid8>_v<N>.<k>.dar`) of the
    /// operator envelope, extracted to disk (issue #473). Empty for tenant
    /// envelopes: the operator envelope carries every unit's catalogue
    /// (`build::catalogs_for_all`), so reading the tenants' copies as well
    /// would only double the scratch footprint of a rebuild.
    pub catalogues: Vec<ExtractedCatalogue>,
    /// The operator envelope's cross-volume `index/units.tsv` (§1.5, #418
    /// R4), extracted to disk. `None` for a tenant envelope (whose index
    /// files `catalog rebuild` does not need: the operator map covers every
    /// tenant, and a tenant's `paths.tsv` is as large as its files) and for
    /// an operator envelope from before 1.3.0.
    pub index_units: Option<PathBuf>,
}

/// One dar catalogue file taken out of an operator envelope.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExtractedCatalogue {
    /// The member's file name inside `catalogs/`, e.g. `1a2b3c4d_v3.1.dar`.
    pub file_name: String,
    pub path: PathBuf,
}

/// `<stem>.<n>.dar` -> `<stem>` for a catalogue member name, else `None`:
/// the stem a stage set's `catalog_path` ends in
/// (`<home>/catalogs/<uuid8>/s<id>/<uuid8>_v<version>`), which dar takes as
/// the base of `<stem>.1.dar`, `<stem>.2.dar`. Only plain file names are
/// accepted — no separator, no `..` — because the name comes off a tape.
pub(crate) fn catalogue_stem(file_name: &str) -> Option<&str> {
    let plain = !file_name.is_empty()
        && file_name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'));
    if !plain {
        return None;
    }
    let without_dar = file_name.strip_suffix(".dar")?;
    let (stem, number) = without_dar.rsplit_once('.')?;
    let numbered = !number.is_empty() && number.chars().all(|c| c.is_ascii_digit());
    (numbered && !stem.is_empty() && stem != "." && !stem.starts_with('.')).then_some(stem)
}

/// Whether any dar catalogue file exists on disk for a stage set whose
/// `catalog_path` is `catalog_path` (the stem; dar's files are
/// `<stem>.<n>.dar` beside it) — what `build` packs into an envelope, and
/// silently packs nothing of when there is none (issue #473). The same
/// discovery rule as `build::catalog_file_paths`, which is private to the
/// build.
pub(crate) fn catalogue_on_disk(catalog_path: &str) -> bool {
    let base = Path::new(catalog_path);
    let (Some(dir), Some(stem)) = (base.parent(), base.file_name().and_then(|f| f.to_str())) else {
        return false;
    };
    let prefix = format!("{stem}.");
    fs::read_dir(dir).is_ok_and(|rd| {
        rd.flatten().any(|e| {
            let name = e.file_name().to_string_lossy().into_owned();
            name.starts_with(&prefix) && name.ends_with(".dar")
        })
    })
}

impl OpenedEnvelope {
    /// True for the operator envelope and its backup, which carry every unit
    /// on the volume under the placeholder tenant `"operator"`.
    ///
    /// Decided by the front-index entry type this envelope was opened as
    /// (`operator_envelope`, `operator_envelope_backup`), never by the
    /// manifest's tenant string (issue #459): a real tenant may be named
    /// `operator` (`validate_tenant_name` accepts it, and `operator_name_for_init`
    /// falls back to it), and tenant envelopes come first on the tape.
    pub fn is_operator(&self) -> bool {
        matches!(
            crate::volume::layout_model::ZoneKind::from_type_label(&self.type_label),
            Some(
                crate::volume::layout_model::ZoneKind::OperatorEnvelope
                    | crate::volume::layout_model::ZoneKind::OperatorEnvelopeBackup
            )
        )
    }
}

/// Why an envelope could not be opened.
#[derive(Debug)]
pub enum OpenError {
    /// The supplied key is not a recipient. For a tenant envelope this is
    /// routine (an operator key opens all of them, but a tenant key opens
    /// only its own); for the operator envelope it means the key is neither
    /// an operator key nor the escrow key.
    NoMatchingKey,
    /// Anything else: tape read, decrypt, tar, or parse.
    Failed(TapectlError),
}

impl From<TapectlError> for OpenError {
    fn from(e: TapectlError) -> Self {
        OpenError::Failed(e)
    }
}

/// Read one envelope off tape, decrypt it, and unpack what a rebuild needs.
///
/// Streams the ciphertext to `scratch` rather than buffering it: an operator
/// envelope carries `catalog.db` and is not bounded by anything small (the
/// H9 whole-object class, issues #32/#35/#87). The tape's block padding is
/// trimmed on the way out using the front index's `size_bytes`, exactly as
/// `volume::raw::restore_raw` does — an envelope without a recorded size is
/// unreadable as ciphertext and reported as such rather than guessed at.
///
/// The trimmed ciphertext is hashed as it streams and checked against the
/// front index's `sha256_encrypted` before it is decrypted: a mismatch is
/// [`OpenError::Failed`], never [`OpenError::NoMatchingKey`], so "not a
/// recipient" is only ever said of bytes the front index vouches for.
pub fn open_envelope(
    store: &mut dyn Store,
    entry: &ParsedIndexEntry,
    identities: &[age::x25519::Identity],
    scratch: &Path,
) -> std::result::Result<OpenedEnvelope, OpenError> {
    let true_len = entry.size_bytes.ok_or_else(|| {
        OpenError::Failed(TapectlError::Other(format!(
            "front index records no size for the {} at position {} — \
             its block padding cannot be trimmed, so the ciphertext cannot be read",
            entry.type_label, entry.position
        )))
    })?;

    fs::create_dir_all(scratch).map_err(TapectlError::from)?;
    let ct_path = scratch.join(format!("{:04}_{}.age", entry.position, entry.type_label));
    let read_hash = {
        let out = BufWriter::new(File::create(&ct_path).map_err(TapectlError::from)?);
        let mut bounded = TruncatingWriter::new(HashingWriter::new(out), true_len);
        let read = store.read_file(entry.position as u32, &mut bounded);
        let mut hashed = bounded.into_inner();
        read.map_err(OpenError::Failed)?;
        hashed.flush().map_err(TapectlError::from)?;
        hashed.finalize_hex()
    };

    // The front index's hash is checked BEFORE age sees the bytes, because
    // age cannot tell damage from a stranger: a recipient stanza that fails
    // to unwrap is, to age 0.11, a stanza for someone else, so a damaged
    // header comes back as `NoMatchingKeys`. Only bytes that match the front
    // index make "not a recipient" an answer about the key rather than the
    // tape (`volume-format-v2.md` §1.3: a copy that "does not match the
    // front index's hash" turns the reader to its backup, as RESTORE.sh's
    // `try_envelope` does).
    if let Some(expected) = &entry.sha256_encrypted {
        if !read_hash.eq_ignore_ascii_case(expected) {
            let _ = fs::remove_file(&ct_path);
            return Err(OpenError::Failed(TapectlError::Other(format!(
                "the {} at position {} does not match the front index's hash \
                 (read {read_hash}, the front index records {expected}), so it is damaged",
                entry.type_label, entry.position
            ))));
        }
    }

    let result = unpack_envelope(&ct_path, identities, scratch, entry);
    // The ciphertext is a byte-identical copy of what is already on tape;
    // keeping it would double the scratch footprint of a rebuild for nothing.
    let _ = fs::remove_file(&ct_path);
    result
}

fn unpack_envelope(
    ct_path: &Path,
    identities: &[age::x25519::Identity],
    scratch: &Path,
    entry: &ParsedIndexEntry,
) -> std::result::Result<OpenedEnvelope, OpenError> {
    let ct = File::open(ct_path).map_err(TapectlError::from)?;
    let decryptor = age::Decryptor::new(ct)
        .map_err(|e| TapectlError::Encryption(format!("envelope decryptor: {e}")))?;
    let reader = match decryptor.decrypt(identities.iter().map(|id| id as &dyn age::Identity)) {
        Ok(r) => r,
        Err(age::DecryptError::NoMatchingKeys) => return Err(OpenError::NoMatchingKey),
        Err(e) => {
            return Err(OpenError::Failed(TapectlError::Encryption(format!(
                "envelope at position {}: decrypt failed: {e}",
                entry.position
            ))))
        }
    };

    let mut manifest_text: Option<String> = None;
    let mut catalog_db: Option<PathBuf> = None;
    let mut index_units: Option<PathBuf> = None;
    let mut catalogues: Vec<ExtractedCatalogue> = Vec::new();
    let from_operator = matches!(
        crate::volume::layout_model::ZoneKind::from_type_label(&entry.type_label),
        Some(
            crate::volume::layout_model::ZoneKind::OperatorEnvelope
                | crate::volume::layout_model::ZoneKind::OperatorEnvelopeBackup
        )
    );

    let mut archive = tar::Archive::new(reader);
    for member in archive.entries().map_err(TapectlError::from)? {
        let mut member = member.map_err(TapectlError::from)?;
        let name = member
            .path()
            .map_err(TapectlError::from)?
            .to_string_lossy()
            .to_string();
        match name.as_str() {
            "MANIFEST.toml" => {
                let mut s = String::new();
                member.read_to_string(&mut s).map_err(TapectlError::from)?;
                manifest_text = Some(s);
            }
            "catalog.db" => {
                // Extracted rather than read into memory: #83's subset is
                // small next to a slice but is still a whole SQLite file,
                // and rusqlite wants a path anyway.
                let path = scratch.join(format!("{:04}_catalog.db", entry.position));
                let mut out = BufWriter::new(File::create(&path).map_err(TapectlError::from)?);
                std::io::copy(&mut member, &mut out).map_err(TapectlError::from)?;
                catalog_db = Some(path);
            }
            // The cross-volume index's unit map (§1.5), operator envelope
            // only: a rebuild reads it as a to-do list of tapes the catalog
            // lacks. Streamed to disk like `catalog.db`.
            crate::volume::index::UNITS_MEMBER if from_operator => {
                let path = scratch.join(format!("{:04}_index_units.tsv", entry.position));
                let mut out = BufWriter::new(File::create(&path).map_err(TapectlError::from)?);
                std::io::copy(&mut member, &mut out).map_err(TapectlError::from)?;
                out.flush().map_err(TapectlError::from)?;
                index_units = Some(path);
            }
            // The unit's dar catalogue (issue #473), operator envelope only:
            // `catalog rebuild` puts it back where `stage create` would have,
            // so a copy written after a rebuild carries it again. Streamed to
            // disk, never held. A name that is not a plain catalogue file
            // name is skipped like any other member we do not consume.
            other => {
                if let Some(file_name) = other.strip_prefix("catalogs/") {
                    if from_operator && catalogue_stem(file_name).is_some() {
                        let dir = scratch.join(format!("{:04}_catalogs", entry.position));
                        fs::create_dir_all(&dir).map_err(TapectlError::from)?;
                        let path = dir.join(file_name);
                        let mut out =
                            BufWriter::new(File::create(&path).map_err(TapectlError::from)?);
                        std::io::copy(&mut member, &mut out).map_err(TapectlError::from)?;
                        out.flush().map_err(TapectlError::from)?;
                        catalogues.push(ExtractedCatalogue {
                            file_name: file_name.to_string(),
                            path,
                        });
                    }
                }
                // RECOVERY.md, PLAN.toml and the catalog-*.sql helpers are
                // for a human at a terminal, not for a rebuild. Skipped, not
                // an error — an envelope is free to carry more than we
                // consume.
            }
        }
    }

    let manifest_text = manifest_text.ok_or_else(|| {
        OpenError::Failed(TapectlError::Other(format!(
            "envelope at position {} decrypted but carries no MANIFEST.toml",
            entry.position
        )))
    })?;
    let manifest = parse_manifest(&manifest_text).map_err(OpenError::Failed)?;

    Ok(OpenedEnvelope {
        position: entry.position,
        type_label: entry.type_label.clone(),
        manifest,
        catalog_db,
        catalogues,
        index_units,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::volume::layout::{self, ManifestSlice as GenSlice, ManifestUnit as GenUnit};

    fn generated_manifest(tenant: &str) -> String {
        layout::generate_manifest_toml(
            "VOL001",
            tenant,
            &[GenUnit {
                name: "photos/2019".to_string(),
                uuid: "u-1".to_string(),
                snapshot_version: 7,
                stage_set_id: 42,
                dar_version: Some("2.7.20".to_string()),
                dar_command: Some(r#"dar -c "x" -s 10G"#.to_string()),
                slices: vec![GenSlice {
                    number: 1,
                    tape_position: 9,
                    size_bytes: 100,
                    encrypted_bytes: 120,
                    sha256_plain: "aa".repeat(32),
                    sha256_encrypted: "bb".repeat(32),
                }],
            }],
        )
    }

    /// Parsed against the real generator's output, not a hand-typed sample:
    /// a hand-typed one would keep parsing after the generator changed.
    #[test]
    fn the_parser_reads_what_the_generator_writes() {
        let m = parse_manifest(&generated_manifest("alpha")).unwrap();
        assert_eq!(m.manifest.volume, "VOL001");
        assert_eq!(m.manifest.tenant, "alpha");
        assert_eq!(m.units.len(), 1);
        let u = &m.units[0];
        assert_eq!(u.name, "photos/2019");
        assert_eq!(u.snapshot_version, 7);
        assert_eq!(u.dar_version.as_deref(), Some("2.7.20"));
        assert_eq!(u.slices.len(), 1);
        // The two numbers a rebuild must never conflate.
        assert_eq!(u.slices[0].number, 1);
        assert_eq!(u.slices[0].tape_position, 9);
        assert_eq!(u.slices[0].sha256_plain, "aa".repeat(32));
    }

    /// Issue #459: the operator envelope is the one the FRONT INDEX types
    /// `operator_envelope` (or its backup), whatever its manifest's tenant
    /// string; a tenant envelope is never it, even for a tenant that is
    /// literally named `operator`.
    #[test]
    fn the_operator_envelope_is_recognised_by_its_front_index_type_not_its_tenant() {
        let opened = |type_label: &str, tenant: &str| OpenedEnvelope {
            position: 4,
            type_label: type_label.to_string(),
            manifest: parse_manifest(&generated_manifest(tenant)).unwrap(),
            catalog_db: None,
            catalogues: Vec::new(),
            index_units: None,
        };
        assert!(opened("operator_envelope", "operator").is_operator());
        assert!(opened("operator_envelope_backup", "operator").is_operator());
        // Whatever the placeholder says, the entry type decides.
        assert!(opened("operator_envelope", "alpha").is_operator());
        assert!(!opened("tenant_envelope", "operator").is_operator());
        assert!(!opened("tenant_envelope_backup", "operator").is_operator());
        assert!(!opened("tenant_envelope", "alpha").is_operator());
    }

    /// Tapes written before #134 carry `layout_version = 1` in `[manifest]`.
    /// Those are exactly the tapes a disaster rebuild will be reading, so
    /// the parser must ignore the key rather than reject the manifest.
    #[test]
    fn a_manifest_from_before_the_layout_version_removal_still_parses() {
        let current = generated_manifest("alpha");
        let legacy = current.replace("[manifest]\n", "[manifest]\nlayout_version = 1\n");
        assert_ne!(legacy, current, "the legacy key was not injected");
        let m = parse_manifest(&legacy).expect("a pre-#134 manifest must still parse");
        assert_eq!(m.units.len(), 1);
        assert_eq!(m.units[0].slices[0].tape_position, 9);
    }

    /// Issue #473: the catalogue member names an envelope carries
    /// (`catalogs/<uuid8>_v<N>.<k>.dar`) reduce to the stem a stage set's
    /// `catalog_path` ends in; anything that is not a plain file name from a
    /// tape is refused rather than joined onto a path.
    #[test]
    fn a_catalogue_member_name_reduces_to_its_stem_and_only_plain_names_do() {
        assert_eq!(catalogue_stem("1a2b3c4d_v3.1.dar"), Some("1a2b3c4d_v3"));
        assert_eq!(catalogue_stem("1a2b3c4d_v3.12.dar"), Some("1a2b3c4d_v3"));
        for bad in [
            "",
            "1a2b3c4d_v3.dar",
            "1a2b3c4d_v3.x.dar",
            "../1a2b3c4d_v3.1.dar",
            "a/b_v1.1.dar",
            ".hidden_v1.1.dar",
            "..1.dar",
            "1a2b3c4d_v3.1.dar.age",
            "a b_v1.1.dar",
        ] {
            assert_eq!(catalogue_stem(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn catalogue_on_disk_finds_dars_beside_the_stem_and_nothing_else() {
        let dir = tempfile::tempdir().unwrap();
        let stem = dir.path().join("1a2b3c4d_v3");
        let stem = stem.to_str().unwrap();
        assert!(!catalogue_on_disk(stem), "nothing yet");
        fs::write(dir.path().join("1a2b3c4d_v3.txt"), b"x").unwrap();
        fs::write(dir.path().join("1a2b3c4d_v30.1.dar"), b"x").unwrap();
        assert!(
            !catalogue_on_disk(stem),
            "another version's file is not ours"
        );
        fs::write(dir.path().join("1a2b3c4d_v3.1.dar"), b"x").unwrap();
        assert!(catalogue_on_disk(stem));
        assert!(!catalogue_on_disk("/nonexistent/dir/stem"));
    }

    /// A manifest with no units is well-formed (an empty tenant envelope),
    /// and must not be mistaken for a parse failure.
    #[test]
    fn a_manifest_with_no_units_parses_as_empty() {
        let m = parse_manifest(&layout::generate_manifest_toml("VOL001", "alpha", &[])).unwrap();
        assert!(m.units.is_empty());
    }
}
