//! Reading a volume envelope back off tape.
//!
//! Envelopes are written by [`crate::volume::build`] as
//! `age(tar{MANIFEST.toml, RECOVERY.md, tables/?, PLAN.toml?, catalog.db?,
//! index/?})` (`docs/design/volume-format-v3.md` §1): a tenant envelope
//! carries, for each staging the tape's plan holds a slice of, the staging's
//! file table and id map under `tables/` (§1.6); the operator envelope
//! carries both as rows of its `catalog.db` instead (§1.4).
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
//! (`volume-format-v3.md` §1: `enc(op+esc)`), so "an operator or escrow key
//! is required" needs no key-type introspection anywhere — a tenant key
//! simply fails to open it, and [`OpenError::NoMatchingKey`] says so.

use std::fs::{self, File};
use std::io::{BufReader, BufWriter, Read, Write};
use std::path::{Path, PathBuf};

use crate::error::{Result, TapectlError};
use crate::store::Store;
use crate::util::{HashingWriter, TruncatingWriter};
use crate::volume::format::ParsedIndexEntry;
pub use crate::volume::manifest::{ManifestSlice, ManifestUnit};
use crate::volume::{filetable, manifest};

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
    /// `None` for tenant envelopes, which never carry it, and for an
    /// operator envelope without one, which `catalog rebuild` refuses.
    pub catalog_db: Option<PathBuf>,
    /// A tenant envelope's `tables/` members (§1.6), each extracted to disk
    /// as it streamed past, never held: a staging's file table and id map.
    /// Empty for the operator envelope, which carries them as `catalog.db`
    /// rows ([`crate::db::ontape_catalog`]). Read them through
    /// [`OpenedEnvelope::file_table`] and [`OpenedEnvelope::id_map`].
    pub tables: Vec<ExtractedTable>,
    /// The operator envelope's cross-volume `index/units.tsv` (§1.5, #418
    /// R4), extracted to disk. `None` for a tenant envelope (whose index
    /// files `catalog rebuild` does not need: the operator map covers every
    /// tenant, and a tenant's `paths.tsv` is as large as its files), and
    /// for an operator envelope without one, which `catalog rebuild` refuses.
    pub index_units: Option<PathBuf>,
}

/// One `tables/` member taken out of a tenant envelope.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExtractedTable {
    /// The member's name in the envelope, as its MANIFEST block names it:
    /// `tables/<uuid8>_v<version>_s<stage_set_id>.tsv` or `….ids.tsv`.
    pub member: String,
    pub path: PathBuf,
}

/// A `tables/` member name's file name, when it is a plain one: no
/// separator, no `..`, nothing but letters, digits, `.`, `_` and `-`, not
/// starting with `.`. The name comes off a tape and is joined onto a path.
fn plain_table_name(member: &str) -> Option<&str> {
    let name = member.strip_prefix(manifest::TABLES_DIR)?;
    let plain = !name.is_empty()
        && !name.starts_with('.')
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'));
    plain.then_some(name)
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

    /// Where the member this envelope's block of `stage_set_id` names under
    /// `key` (`file_table` or `id_map`) was extracted to. Refused, naming
    /// the staging, when no block of it is here, the block names no such
    /// member (an operator envelope's never does, §1.9), or the member it
    /// names is not in the envelope.
    fn table_path(
        &self,
        stage_set_id: i64,
        key: &str,
        named: impl Fn(&ManifestUnit) -> Option<&String>,
    ) -> Result<&Path> {
        let what = |why: String| {
            TapectlError::Other(format!(
                "the {} at position {}: stage set {stage_set_id}'s {key}: {why}",
                self.type_label, self.position
            ))
        };
        let unit = self
            .manifest
            .units
            .iter()
            .find(|u| u.stage_set_id == stage_set_id)
            .ok_or_else(|| what("no block of MANIFEST.toml describes it".to_string()))?;
        let member = named(unit).ok_or_else(|| what(format!("its block names no `{key}`")))?;
        self.tables
            .iter()
            .find(|t| &t.member == member)
            .map(|t| t.path.as_path())
            .ok_or_else(|| what(format!("the envelope holds no member {member:?}")))
    }

    /// The file table of `stage_set_id`, read row by row as it streams off
    /// the extracted member (§1.6): [`filetable::Reader`] checks the header,
    /// and each row as it is read. Never held whole.
    pub fn file_table(&self, stage_set_id: i64) -> Result<filetable::Reader<BufReader<File>>> {
        let path = self.table_path(stage_set_id, "file_table", |u| u.file_table.as_ref())?;
        filetable::Reader::new(BufReader::new(File::open(path)?))
    }

    /// The id map of `stage_set_id` (§1.6): every user and group id its
    /// stream carries, with the name the host gave it or none. Small: one
    /// row per distinct id.
    pub fn id_map(&self, stage_set_id: i64) -> Result<Vec<crate::idmap::IdRow>> {
        let path = self.table_path(stage_set_id, "id_map", |u| u.id_map.as_ref())?;
        crate::idmap::parse(BufReader::new(File::open(path)?))
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
    // tape (`volume-format-v3.md` §1.3: a copy that "does not match the
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
    let mut tables: Vec<ExtractedTable> = Vec::new();
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
            // A staging's file table or id map (§1.6), tenant envelope only:
            // streamed to disk, never held (a million members is about 121
            // MB of text). A name that is not a plain file name from a tape
            // is skipped like any other member we do not consume.
            other => {
                if let Some(file_name) = plain_table_name(other).filter(|_| !from_operator) {
                    let dir = scratch.join(format!("{:04}_tables", entry.position));
                    fs::create_dir_all(&dir).map_err(TapectlError::from)?;
                    let path = dir.join(file_name);
                    let mut out = BufWriter::new(File::create(&path).map_err(TapectlError::from)?);
                    std::io::copy(&mut member, &mut out).map_err(TapectlError::from)?;
                    out.flush().map_err(TapectlError::from)?;
                    tables.push(ExtractedTable {
                        member: other.to_string(),
                        path,
                    });
                }
                // RECOVERY.md, PLAN.toml and a tenant's index/ members are
                // for a human at a terminal or for the heir's script, not for
                // this reader. Skipped, not an error: an envelope is free to
                // carry more than we consume.
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
        tables,
        index_units,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::volume::layout::{self, ManifestSlice as GenSlice, ManifestUnit as GenUnit};

    fn gen_unit(stage_set_id: i64, tables: bool) -> GenUnit {
        let uuid = "1a2b3c4d-2222-3333-4444-555555555555";
        let (t, m) = manifest::table_names(uuid, 7, stage_set_id);
        GenUnit {
            name: "photos/2019".to_string(),
            uuid: uuid.to_string(),
            snapshot_version: 7,
            stage_set_id,
            payload: manifest::PAYLOAD_TAR.to_string(),
            stream_bytes: 2048,
            file_table: tables.then_some(t),
            id_map: tables.then_some(m),
            tar_version: Some("tar (GNU tar) 1.35".to_string()),
            compression: None,
            slices: vec![GenSlice {
                number: 1,
                tape_position: Some(9),
                size_bytes: 100,
                encrypted_bytes: 120,
                sha256_plain: "aa".repeat(32),
                sha256_encrypted: "bb".repeat(32),
                continues: false,
            }],
        }
    }

    fn generated_manifest(tenant: &str) -> String {
        layout::generate_manifest_toml("VOL001", tenant, &[gen_unit(42, true)])
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
        assert_eq!(u.payload, "tar");
        assert_eq!(u.tar_version.as_deref(), Some("tar (GNU tar) 1.35"));
        assert_eq!(u.file_table.as_deref(), Some("tables/1a2b3c4d_v7_s42.tsv"));
        assert_eq!(u.slices.len(), 1);
        // The two numbers a rebuild must never conflate.
        assert_eq!(u.slices[0].number, 1);
        assert_eq!(u.slices[0].tape_position, Some(9));
        assert_eq!(u.slices[0].sha256_plain, "aa".repeat(32));
    }

    fn opened(type_label: &str, tenant: &str) -> OpenedEnvelope {
        OpenedEnvelope {
            position: 4,
            type_label: type_label.to_string(),
            manifest: parse_manifest(&generated_manifest(tenant)).unwrap(),
            catalog_db: None,
            tables: Vec::new(),
            index_units: None,
        }
    }

    /// Issue #459: the operator envelope is the one the FRONT INDEX types
    /// `operator_envelope` (or its backup), whatever its manifest's tenant
    /// string; a tenant envelope is never it, even for a tenant that is
    /// literally named `operator`.
    #[test]
    fn the_operator_envelope_is_recognised_by_its_front_index_type_not_its_tenant() {
        assert!(opened("operator_envelope", "operator").is_operator());
        assert!(opened("operator_envelope_backup", "operator").is_operator());
        // Whatever the placeholder says, the entry type decides.
        assert!(opened("operator_envelope", "alpha").is_operator());
        assert!(!opened("tenant_envelope", "operator").is_operator());
        assert!(!opened("tenant_envelope_backup", "operator").is_operator());
        assert!(!opened("tenant_envelope", "alpha").is_operator());
    }

    /// `volume-format-v3.md` §1.2 rule 3 (#134): the envelope's manifest
    /// reader reads on past a key it does not know rather than reject the
    /// manifest. A later writer may add one.
    #[test]
    fn a_manifest_reads_on_past_a_key_it_does_not_know() {
        let current = generated_manifest("alpha");
        let later = current.replace("[manifest]\n", "[manifest]\nfuture_key = 1\n");
        assert_ne!(later, current, "the key was not injected");
        let m = parse_manifest(&later).expect("an unknown key is read past");
        assert_eq!(m.units.len(), 1);
        assert_eq!(m.units[0].slices[0].tape_position, Some(9));
    }

    /// A `tables/` member name is taken off a tape only when it is a plain
    /// file name: it is joined onto a path.
    #[test]
    fn only_a_plain_tables_member_name_is_taken() {
        assert_eq!(
            plain_table_name("tables/1a2b3c4d_v3_s42.tsv"),
            Some("1a2b3c4d_v3_s42.tsv")
        );
        assert_eq!(
            plain_table_name("tables/1a2b3c4d_v3_s42.ids.tsv"),
            Some("1a2b3c4d_v3_s42.ids.tsv")
        );
        for bad in [
            "tables/",
            "tables/../x.tsv",
            "tables/a/b.tsv",
            "tables/.hidden.tsv",
            "tables/a b.tsv",
            "index/units.tsv",
            "1a2b3c4d_v3_s42.tsv",
        ] {
            assert_eq!(plain_table_name(bad), None, "{bad:?}");
        }
    }

    /// One encrypted envelope, as `build` makes one, at `position` of a
    /// `MemStore`, and its front-index entry.
    fn store_envelope(
        store: &mut crate::store::MemStore,
        position: i32,
        type_label: &str,
        members: &[(&str, Vec<u8>)],
        recipient: &str,
    ) -> ParsedIndexEntry {
        let mut builder = tar::Builder::new(Vec::new());
        for (name, bytes) in members {
            let mut header = tar::Header::new_gnu();
            header.set_size(bytes.len() as u64);
            header.set_mode(0o600);
            header.set_cksum();
            builder.append_data(&mut header, name, &bytes[..]).unwrap();
        }
        let tar_bytes = builder.into_inner().unwrap();
        let ct = crate::staging::encrypt_data(&tar_bytes, &[recipient.to_string()]).unwrap();
        if store.files.len() <= position as usize {
            store.files.resize(position as usize + 1, Vec::new());
        }
        store.files[position as usize] = ct.clone();
        use sha2::Digest;
        ParsedIndexEntry {
            position,
            type_label: type_label.to_string(),
            size_bytes: Some(ct.len() as u64),
            sha256_encrypted: Some(format!("{:x}", sha2::Sha256::digest(&ct))),
        }
    }

    /// §1.6 (D4, D5): a tenant envelope's `tables/` members read back as the
    /// staging's file table and id map, streamed off disk, found by the
    /// names its MANIFEST block gives. A block without them, a member the
    /// block names that is not there, and a staging no block describes are
    /// each refused, naming the staging. The operator envelope extracts no
    /// `tables/` member even when one is there.
    #[test]
    fn a_tenant_envelope_gives_a_stagings_file_table_and_id_map() {
        use crate::idmap::{IdKind, IdRow};
        use crate::volume::filetable::{Kind, Row};
        let rows = vec![
            Row {
                kind: Kind::Folder,
                slice: 1,
                start: 0,
                end: 512,
                size: 0,
                sha256: None,
                path: ".".into(),
                link: None,
            },
            Row {
                kind: Kind::File,
                slice: 1,
                start: 512,
                end: 2048,
                size: 1000,
                sha256: Some([7; 32]),
                path: "a\\tb".into(),
                link: None,
            },
        ];
        let mut table = Vec::new();
        filetable::write(&mut table, 3072, rows.iter().cloned().map(Ok)).unwrap();
        let ids = vec![
            IdRow {
                kind: IdKind::User,
                id: 1000,
                name: Some("alice".into()),
            },
            IdRow {
                kind: IdKind::Group,
                id: 4242,
                name: None,
            },
        ];
        let mut map = Vec::new();
        crate::idmap::write(&mut map, &ids).unwrap();

        let key = age::x25519::Identity::generate();
        let recipient = key.to_public().to_string();
        let unit = gen_unit(42, true);
        let manifest_text =
            layout::generate_manifest_toml("VOL001", "alpha", std::slice::from_ref(&unit));
        let mut store = crate::store::MemStore::new(512 * 1024);
        let members = [
            ("MANIFEST.toml", manifest_text.into_bytes()),
            (unit.file_table.as_deref().unwrap(), table.clone()),
            (unit.id_map.as_deref().unwrap(), map.clone()),
        ];
        let entry = store_envelope(&mut store, 4, "tenant_envelope", &members, &recipient);
        let scratch = tempfile::tempdir().unwrap();
        let env = open_envelope(
            &mut store,
            &entry,
            std::slice::from_ref(&key),
            scratch.path(),
        )
        .unwrap_or_else(|e| panic!("opens: {e:?}"));
        let mut reader = env.file_table(42).unwrap();
        assert_eq!(reader.stream_bytes(), 3072);
        let back: Vec<Row> = reader.by_ref().collect::<Result<_>>().unwrap();
        assert_eq!(back, rows);
        assert_eq!(env.id_map(42).unwrap(), ids);

        let err = |e: TapectlError| e.to_string();
        let none = err(env.file_table(7).err().expect("refused"));
        assert!(
            none.contains("stage set 7") && none.contains("no block"),
            "{none}"
        );

        // The operator envelope's block names no table, and an operator
        // envelope is never read for one.
        let op_manifest =
            layout::generate_manifest_toml("VOL001", "operator", &[gen_unit(42, false)]);
        let members = [
            ("MANIFEST.toml", op_manifest.into_bytes()),
            (unit.file_table.as_deref().unwrap(), table),
        ];
        let entry = store_envelope(&mut store, 5, "operator_envelope", &members, &recipient);
        let op = open_envelope(
            &mut store,
            &entry,
            std::slice::from_ref(&key),
            scratch.path(),
        )
        .unwrap_or_else(|e| panic!("opens: {e:?}"));
        assert!(op.tables.is_empty(), "{:?}", op.tables);
        let unnamed = err(op.id_map(42).unwrap_err());
        assert!(unnamed.contains("names no `id_map`"), "{unnamed}");

        // A block that names a member the envelope does not hold.
        let manifest_text =
            layout::generate_manifest_toml("VOL001", "alpha", std::slice::from_ref(&unit));
        let members = [("MANIFEST.toml", manifest_text.into_bytes())];
        let entry = store_envelope(&mut store, 6, "tenant_envelope", &members, &recipient);
        let bare = open_envelope(
            &mut store,
            &entry,
            std::slice::from_ref(&key),
            scratch.path(),
        )
        .unwrap_or_else(|e| panic!("opens: {e:?}"));
        let missing = err(bare.file_table(42).err().expect("refused"));
        assert!(missing.contains("holds no member"), "{missing}");
    }

    /// A manifest with no units is well-formed (an empty tenant envelope),
    /// and must not be mistaken for a parse failure.
    #[test]
    fn a_manifest_with_no_units_parses_as_empty() {
        let m = parse_manifest(&layout::generate_manifest_toml("VOL001", "alpha", &[])).unwrap();
        assert!(m.units.is_empty());
    }
}
