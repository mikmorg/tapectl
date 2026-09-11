//! Reading a volume envelope back off tape.
//!
//! Envelopes are written by [`crate::volume::build`] as
//! `age(tar{MANIFEST.toml, RECOVERY.md, catalog-*.sql, PLAN.toml?, catalog.db?})`.
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
use std::io::{BufWriter, Read};
use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::error::{Result, TapectlError};
use crate::store::Store;
use crate::util::TruncatingWriter;
use crate::volume::format::ParsedIndexEntry;

/// The `[manifest]` header of an envelope's `MANIFEST.toml`.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct ManifestHeader {
    /// Volume label, as the writing tapectl knew it.
    pub volume: String,
    /// Tenant name — the real tenant for a tenant envelope, and the literal
    /// `"operator"` for the operator envelope and its backup, which list
    /// every unit across all tenants rather than belonging to one.
    pub tenant: String,
    pub created_at: String,
}

/// One `[[units.slices]]` block.
///
/// `tape_position` is the file number on tape and is NOT the slice number:
/// slices begin after the envelopes (position 8 on a typical layout), and
/// the two diverge further on a multi-unit volume. Confusing them yields a
/// broken archive — see the `#72` retrieval-guide note.
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

/// A parsed envelope `MANIFEST.toml`.
///
/// Deliberately NOT `deny_unknown_fields`: tapes written before #134 carry a
/// `layout_version = 1` key in `[manifest]` that no longer exists, and a
/// rebuild must read those tapes. Pinned by a test.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct EnvelopeManifest {
    pub manifest: ManifestHeader,
    #[serde(default)]
    pub units: Vec<ManifestUnit>,
}

impl EnvelopeManifest {
    /// True for the operator envelope and its backup, which carry every
    /// unit on the volume under the placeholder tenant `"operator"`.
    pub fn is_operator(&self) -> bool {
        self.manifest.tenant == "operator"
    }
}

/// Parse an envelope `MANIFEST.toml`.
pub fn parse_manifest(text: &str) -> Result<EnvelopeManifest> {
    toml::from_str(text)
        .map_err(|e| TapectlError::Other(format!("envelope MANIFEST.toml: parse failed: {e}")))
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
    {
        let out = BufWriter::new(File::create(&ct_path).map_err(TapectlError::from)?);
        let mut bounded = TruncatingWriter::new(out, true_len);
        store
            .read_file(entry.position as u32, &mut bounded)
            .map_err(OpenError::Failed)?;
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
            // RECOVERY.md, PLAN.toml and the catalog-*.sql helpers are for
            // a human at a terminal, not for a rebuild. Skipped, not an
            // error — an envelope is free to carry more than we consume.
            _ => {}
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
        assert!(!m.is_operator());
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

    #[test]
    fn the_operator_envelope_is_recognised_by_its_placeholder_tenant() {
        assert!(parse_manifest(&generated_manifest("operator"))
            .unwrap()
            .is_operator());
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

    /// A manifest with no units is well-formed (an empty tenant envelope),
    /// and must not be mistaken for a parse failure.
    #[test]
    fn a_manifest_with_no_units_parses_as_empty() {
        let m = parse_manifest(&layout::generate_manifest_toml("VOL001", "alpha", &[])).unwrap();
        assert!(m.units.is_empty());
    }
}
