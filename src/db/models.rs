use serde::{Deserialize, Serialize};

// ── Tenants ──

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Tenant {
    pub id: i64,
    pub name: String,
    pub description: Option<String>,
    pub is_operator: bool,
    pub status: String,
    pub created_at: String,
    pub notes: Option<String>,
}

// ── Encryption Keys ──

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EncryptionKey {
    pub id: i64,
    pub tenant_id: i64,
    pub alias: String,
    pub fingerprint: String,
    pub public_key: String,
    pub key_type: String,
    pub is_active: bool,
    pub created_at: String,
    pub description: Option<String>,
    /// The permanent escrow recipient (ADR-0005). At most one row across the
    /// whole database ever has this set.
    pub is_escrow: bool,
}

// ── Units ──

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Unit {
    pub id: i64,
    pub uuid: String,
    pub name: String,
    pub tenant_id: i64,
    pub archive_set_id: Option<i64>,
    pub current_path: Option<String>,
    pub checksum_mode: String,
    pub encrypt: bool,
    pub status: String,
    pub created_at: String,
    pub last_scanned: Option<String>,
    pub notes: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Tag {
    pub id: i64,
    pub name: String,
}

// ── Snapshots ──

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Snapshot {
    pub id: i64,
    pub unit_id: i64,
    pub version: i64,
    pub snapshot_type: String,
    pub base_snapshot_id: Option<i64>,
    pub status: String,
    pub source_path: String,
    pub total_size: Option<i64>,
    pub file_count: Option<i64>,
    pub created_at: String,
    pub superseded_at: Option<String>,
    pub notes: Option<String>,
}

// ── Stage Sets / Slices ──

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StageSet {
    pub id: i64,
    pub snapshot_id: i64,
    pub status: String,
    pub dar_version: Option<String>,
    pub dar_command: Option<String>,
    pub catalog_path: Option<String>,
    pub slice_size: i64,
    pub compression: Option<String>,
    pub encrypted: bool,
    pub key_fingerprints: Option<String>,
    pub num_slices: Option<i64>,
    pub total_dar_size: Option<i64>,
    pub total_encrypted_size: Option<i64>,
    pub source_validated_at: Option<String>,
    pub staged_at: Option<String>,
    pub cleaned_at: Option<String>,
    pub created_at: String,
    pub notes: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StageSlice {
    pub id: i64,
    pub stage_set_id: i64,
    pub slice_number: i64,
    pub size_bytes: i64,
    pub encrypted_bytes: i64,
    pub sha256_plain: String,
    pub sha256_encrypted: String,
    pub staging_path: Option<String>,
}

// ── Locations ──

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Location {
    pub id: i64,
    pub name: String,
    pub description: Option<String>,
    /// ADR-0006 location kind: `shelf` (physical whereabouts of a cartridge)
    /// or `warehouse` (cold cloud storage). Migration 007 defaults every
    /// pre-existing row to `shelf`. The endpoint/bucket/prefix lives in
    /// `description` — deliberately no decorative URI columns (issue #73).
    pub kind: String,
    pub created_at: String,
}

// ── Volume deposits (ADR-0006 warehouse copies) ──

/// A RECORDED copy of an already-sealed volume's bytes at a warehouse
/// location. tapectl never moved these bytes: the operator did, by the
/// documented external procedure, and then recorded the fact here (issue
/// #73, `docs/design-errata.md`).
///
/// There is deliberately no checksum field — tapectl did not perform the
/// upload, so a typed-in checksum would be a claim about a claim. What is
/// attestable is which volume, which location, when, and the provider's
/// receipt identifier.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VolumeDeposit {
    pub id: i64,
    pub volume_id: i64,
    pub location_id: i64,
    pub deposited_at: String,
    pub receipt: Option<String>,
    pub storage_class: Option<String>,
    pub notes: Option<String>,
}

// ── Cartridges ──

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Cartridge {
    pub id: i64,
    pub barcode: String,
    pub media_type: String,
    pub manufacturer: Option<String>,
    pub serial_number: Option<String>,
    pub tape_length_meters: Option<i64>,
    pub nominal_capacity: i64,
    pub status: String,
    pub total_load_count: Option<i64>,
    pub total_bytes_written: Option<i64>,
    pub total_bytes_read: Option<i64>,
    pub first_use: Option<String>,
    pub last_use: Option<String>,
    pub error_history: Option<String>,
    pub location_id: Option<i64>,
    pub created_at: String,
    pub notes: Option<String>,
}

// ── Volumes ──

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Volume {
    pub id: i64,
    pub label: String,
    pub backend_type: String,
    pub backend_name: String,
    pub media_type: Option<String>,
    pub capacity_bytes: i64,
    pub mam_capacity_bytes: Option<i64>,
    pub mam_remaining_at_start: Option<i64>,
    pub bytes_written: i64,
    pub num_data_files: i64,
    pub has_manifest: bool,
    pub location_id: Option<i64>,
    pub status: String,
    // `storage_class` was dropped in migration 008 (issue #101): dead since
    // 001, and a cartridge has a `media_type`, not a storage class. The
    // storage class of a *warehouse deposit* lives on `VolumeDeposit`.
    pub first_write: Option<String>,
    pub last_write: Option<String>,
    pub notes: Option<String>,
    pub created_at: String,
}

// ── Writes ──

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Write {
    pub id: i64,
    pub stage_set_id: i64,
    pub snapshot_id: i64,
    pub volume_id: i64,
    pub status: String,
    pub write_verified: bool,
    pub eot_recovery: Option<String>,
    pub sacrificed_slice_id: Option<i64>,
    pub started_at: Option<String>,
    pub completed_at: Option<String>,
    pub notes: Option<String>,
    pub created_at: String,
}

// ── Verification ──

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VerificationSession {
    pub id: i64,
    pub volume_id: i64,
    pub started_at: String,
    pub completed_at: Option<String>,
    pub verify_type: String,
    pub outcome: String,
    pub slices_checked: i64,
    pub slices_passed: i64,
    pub slices_failed: i64,
    pub slices_skipped: i64,
    pub notes: Option<String>,
}

// ── Events ──

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Event {
    pub id: i64,
    pub timestamp: String,
    pub entity_type: String,
    pub entity_id: i64,
    pub entity_label: Option<String>,
    pub action: String,
    pub field: Option<String>,
    pub old_value: Option<String>,
    pub new_value: Option<String>,
    pub details: Option<String>,
    pub tenant_id: Option<i64>,
}
