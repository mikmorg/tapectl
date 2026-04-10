-- tapectl initial schema (design v4.0 Section 4)

CREATE TABLE meta (
    key   TEXT PRIMARY KEY,
    value TEXT NOT NULL
);

INSERT INTO meta (key, value) VALUES ('schema_version', '1');

-- TENANTS
CREATE TABLE tenants (
    id          INTEGER PRIMARY KEY,
    name        TEXT NOT NULL UNIQUE,
    description TEXT,
    is_operator INTEGER NOT NULL DEFAULT 0,
    status      TEXT NOT NULL DEFAULT 'active'
                CHECK(status IN ('active','inactive','deleted')),
    created_at  TEXT NOT NULL DEFAULT (datetime('now')),
    notes       TEXT
);

-- ENCRYPTION KEYS (per-tenant)
CREATE TABLE encryption_keys (
    id          INTEGER PRIMARY KEY,
    tenant_id   INTEGER NOT NULL REFERENCES tenants(id),
    alias       TEXT NOT NULL UNIQUE,
    fingerprint TEXT NOT NULL UNIQUE,
    public_key  TEXT NOT NULL,
    key_type    TEXT NOT NULL DEFAULT 'primary'
                CHECK(key_type IN ('primary','backup')),
    is_active   INTEGER NOT NULL DEFAULT 1,
    created_at  TEXT NOT NULL DEFAULT (datetime('now')),
    description TEXT
);

-- ARCHIVE SETS (policy templates)
CREATE TABLE archive_sets (
    id                       INTEGER PRIMARY KEY,
    name                     TEXT NOT NULL UNIQUE,
    description              TEXT,
    min_copies               INTEGER,
    required_locations       TEXT,
    encrypt                  INTEGER,
    compression              TEXT,
    checksum_mode            TEXT,
    slice_size               INTEGER,
    verify_interval_days     INTEGER,
    preserve_xattrs          INTEGER,
    preserve_acls            INTEGER,
    preserve_fsa             INTEGER,
    dirty_on_metadata_change INTEGER,
    created_at               TEXT NOT NULL DEFAULT (datetime('now')),
    updated_at               TEXT NOT NULL DEFAULT (datetime('now'))
);

-- UNITS
CREATE TABLE units (
    id              INTEGER PRIMARY KEY,
    uuid            TEXT NOT NULL UNIQUE,
    name            TEXT NOT NULL UNIQUE,
    tenant_id       INTEGER NOT NULL REFERENCES tenants(id),
    archive_set_id  INTEGER REFERENCES archive_sets(id),
    current_path    TEXT,
    checksum_mode   TEXT NOT NULL DEFAULT 'mtime_size'
                    CHECK(checksum_mode IN ('mtime_size','sha256','sha256_on_archive')),
    encrypt         INTEGER NOT NULL DEFAULT 1,
    status          TEXT NOT NULL DEFAULT 'active'
                    CHECK(status IN ('active','tape_only','missing','retired')),
    created_at      TEXT NOT NULL DEFAULT (datetime('now')),
    last_scanned    TEXT,
    notes           TEXT
);

CREATE TABLE tags (
    id   INTEGER PRIMARY KEY,
    name TEXT NOT NULL UNIQUE
);

CREATE TABLE unit_tags (
    unit_id INTEGER NOT NULL REFERENCES units(id),
    tag_id  INTEGER NOT NULL REFERENCES tags(id),
    PRIMARY KEY (unit_id, tag_id)
);

CREATE TABLE unit_path_history (
    id          INTEGER PRIMARY KEY,
    unit_id     INTEGER NOT NULL REFERENCES units(id),
    path        TEXT NOT NULL,
    observed_at TEXT NOT NULL DEFAULT (datetime('now'))
);

-- SNAPSHOTS (content identity)
CREATE TABLE snapshots (
    id               INTEGER PRIMARY KEY,
    unit_id          INTEGER NOT NULL REFERENCES units(id),
    version          INTEGER NOT NULL,
    snapshot_type    TEXT NOT NULL DEFAULT 'full'
                     CHECK(snapshot_type IN ('full','differential','incremental')),
    base_snapshot_id INTEGER REFERENCES snapshots(id),
    status           TEXT NOT NULL DEFAULT 'created'
                     CHECK(status IN ('created','staged','current','superseded',
                                      'reclaimable','purged','failed')),
    source_path      TEXT NOT NULL,
    total_size       INTEGER,
    file_count       INTEGER,
    created_at       TEXT NOT NULL DEFAULT (datetime('now')),
    superseded_at    TEXT,
    notes            TEXT,
    UNIQUE(unit_id, version)
);

-- MANIFESTS (change detection baseline)
CREATE TABLE manifests (
    id          INTEGER PRIMARY KEY,
    snapshot_id INTEGER NOT NULL REFERENCES snapshots(id),
    created_at  TEXT NOT NULL DEFAULT (datetime('now'))
);

CREATE TABLE manifest_entries (
    id           INTEGER PRIMARY KEY,
    manifest_id  INTEGER NOT NULL REFERENCES manifests(id),
    path         TEXT NOT NULL,
    size_bytes   INTEGER NOT NULL,
    mtime        TEXT NOT NULL,
    sha256       TEXT,
    is_directory INTEGER NOT NULL DEFAULT 0,
    mode         INTEGER,
    uid          INTEGER,
    gid          INTEGER,
    username     TEXT,
    groupname    TEXT,
    has_xattrs   INTEGER DEFAULT 0,
    has_acls     INTEGER DEFAULT 0
);

-- FILE CATALOG (browse/search — sha256 backfilled on first stage)
CREATE TABLE files (
    id              INTEGER PRIMARY KEY,
    snapshot_id     INTEGER NOT NULL REFERENCES snapshots(id),
    path            TEXT NOT NULL,
    size_bytes      INTEGER NOT NULL,
    sha256          TEXT,
    modified_at     TEXT,
    is_directory    INTEGER NOT NULL DEFAULT 0,
    UNIQUE(snapshot_id, path)
);

-- STAGE SETS (each dar+encrypt execution)
CREATE TABLE stage_sets (
    id                   INTEGER PRIMARY KEY,
    snapshot_id          INTEGER NOT NULL REFERENCES snapshots(id),
    status               TEXT NOT NULL DEFAULT 'staging'
                         CHECK(status IN ('staging','staged','failed','cleaned')),
    dar_version          TEXT,
    dar_command          TEXT,
    catalog_path         TEXT,
    slice_size           INTEGER NOT NULL,
    compression          TEXT,
    encrypted            INTEGER NOT NULL DEFAULT 1,
    key_fingerprints     TEXT,
    num_slices           INTEGER,
    total_dar_size       INTEGER,
    total_encrypted_size INTEGER,
    source_validated_at  TEXT,
    staged_at            TEXT,
    cleaned_at           TEXT,
    created_at           TEXT NOT NULL DEFAULT (datetime('now')),
    notes                TEXT
);

-- STAGE SLICES (encrypted artifacts, shared across writes)
CREATE TABLE stage_slices (
    id               INTEGER PRIMARY KEY,
    stage_set_id     INTEGER NOT NULL REFERENCES stage_sets(id),
    slice_number     INTEGER NOT NULL,
    size_bytes       INTEGER NOT NULL,
    encrypted_bytes  INTEGER NOT NULL,
    sha256_plain     TEXT NOT NULL,
    sha256_encrypted TEXT NOT NULL,
    staging_path     TEXT,
    UNIQUE(stage_set_id, slice_number)
);

-- LOCATIONS
CREATE TABLE locations (
    id          INTEGER PRIMARY KEY,
    name        TEXT NOT NULL UNIQUE,
    description TEXT,
    created_at  TEXT NOT NULL DEFAULT (datetime('now'))
);

-- PHYSICAL CARTRIDGES
CREATE TABLE cartridges (
    id                    INTEGER PRIMARY KEY,
    barcode               TEXT NOT NULL UNIQUE,
    media_type            TEXT NOT NULL,
    manufacturer          TEXT,
    serial_number         TEXT,
    tape_length_meters    INTEGER,
    nominal_capacity      INTEGER NOT NULL,
    status                TEXT NOT NULL DEFAULT 'available'
                          CHECK(status IN ('available','in_use','pending_erase',
                                           'retired_permanent','offsite')),
    total_load_count      INTEGER DEFAULT 0,
    total_bytes_written   INTEGER DEFAULT 0,
    total_bytes_read      INTEGER DEFAULT 0,
    first_use             TEXT,
    last_use              TEXT,
    error_history         TEXT,
    location_id           INTEGER REFERENCES locations(id),
    created_at            TEXT NOT NULL DEFAULT (datetime('now')),
    notes                 TEXT
);

-- CARTRIDGE / VOLUME RELATIONSHIP
CREATE TABLE cartridge_volumes (
    id            INTEGER PRIMARY KEY,
    cartridge_id  INTEGER NOT NULL REFERENCES cartridges(id),
    volume_id     INTEGER NOT NULL REFERENCES volumes(id),
    mounted_at    TEXT NOT NULL DEFAULT (datetime('now')),
    unmounted_at  TEXT,
    UNIQUE(volume_id)
);

-- VOLUMES (logical write sessions)
CREATE TABLE volumes (
    id                     INTEGER PRIMARY KEY,
    label                  TEXT NOT NULL UNIQUE,
    backend_type           TEXT NOT NULL,
    backend_name           TEXT NOT NULL,
    media_type             TEXT,
    capacity_bytes         INTEGER NOT NULL,
    mam_capacity_bytes     INTEGER,
    mam_remaining_at_start INTEGER,
    bytes_written          INTEGER NOT NULL DEFAULT 0,
    num_data_files         INTEGER NOT NULL DEFAULT 0,
    has_manifest           INTEGER NOT NULL DEFAULT 0,
    location_id            INTEGER REFERENCES locations(id),
    status                 TEXT NOT NULL DEFAULT 'blank'
                           CHECK(status IN ('blank','initialized','active','full',
                                            'retired','missing','erased')),
    storage_class          TEXT,
    first_write            TEXT,
    last_write             TEXT,
    notes                  TEXT,
    created_at             TEXT NOT NULL DEFAULT (datetime('now'))
);

CREATE TABLE volume_movements (
    id            INTEGER PRIMARY KEY,
    volume_id     INTEGER NOT NULL REFERENCES volumes(id),
    from_location INTEGER REFERENCES locations(id),
    to_location   INTEGER NOT NULL REFERENCES locations(id),
    moved_at      TEXT NOT NULL DEFAULT (datetime('now')),
    notes         TEXT
);

-- WRITES (each volume copy from a stage set)
CREATE TABLE writes (
    id                  INTEGER PRIMARY KEY,
    stage_set_id        INTEGER NOT NULL REFERENCES stage_sets(id),
    snapshot_id         INTEGER NOT NULL REFERENCES snapshots(id),
    volume_id           INTEGER NOT NULL REFERENCES volumes(id),
    status              TEXT NOT NULL DEFAULT 'planned'
                        CHECK(status IN ('planned','in_progress','completed',
                                         'failed','aborted','interrupted')),
    write_verified      INTEGER NOT NULL DEFAULT 0,
    eot_recovery        TEXT CHECK(eot_recovery IN (NULL, 'normal',
                                   'overwrite_incomplete', 'sacrifice_last_slice')),
    sacrificed_slice_id INTEGER REFERENCES stage_slices(id),
    started_at          TEXT,
    completed_at        TEXT,
    notes               TEXT,
    created_at          TEXT NOT NULL DEFAULT (datetime('now')),
    UNIQUE(stage_set_id, volume_id)
);

-- WRITE POSITIONS
CREATE TABLE write_positions (
    id               INTEGER PRIMARY KEY,
    write_id         INTEGER NOT NULL REFERENCES writes(id),
    stage_slice_id   INTEGER NOT NULL REFERENCES stage_slices(id),
    position         TEXT NOT NULL,
    status           TEXT NOT NULL DEFAULT 'pending'
                     CHECK(status IN ('pending','writing','written','verified',
                                      'failed','sacrificed')),
    written_at       TEXT,
    sha256_on_volume TEXT,
    UNIQUE(write_id, stage_slice_id)
);

-- VERIFICATION
CREATE TABLE verification_sessions (
    id              INTEGER PRIMARY KEY,
    volume_id       INTEGER NOT NULL REFERENCES volumes(id),
    started_at      TEXT NOT NULL DEFAULT (datetime('now')),
    completed_at    TEXT,
    verify_type     TEXT NOT NULL DEFAULT 'full'
                    CHECK(verify_type IN ('full','quick')),
    outcome         TEXT NOT NULL DEFAULT 'in_progress'
                    CHECK(outcome IN ('in_progress','passed','failed',
                                      'partial','aborted')),
    slices_checked  INTEGER NOT NULL DEFAULT 0,
    slices_passed   INTEGER NOT NULL DEFAULT 0,
    slices_failed   INTEGER NOT NULL DEFAULT 0,
    slices_skipped  INTEGER NOT NULL DEFAULT 0,
    notes           TEXT
);

CREATE TABLE verification_results (
    id                      INTEGER PRIMARY KEY,
    session_id              INTEGER NOT NULL REFERENCES verification_sessions(id),
    write_position_id       INTEGER NOT NULL REFERENCES write_positions(id),
    stage_slice_id          INTEGER NOT NULL REFERENCES stage_slices(id),
    result                  TEXT NOT NULL
                            CHECK(result IN ('passed','failed_checksum','failed_read',
                                             'failed_decrypt','skipped')),
    expected_sha256         TEXT,
    actual_sha256           TEXT,
    read_errors_corrected   INTEGER DEFAULT 0,
    read_errors_uncorrected INTEGER DEFAULT 0,
    verified_at             TEXT NOT NULL DEFAULT (datetime('now')),
    notes                   TEXT
);

-- HEALTH
CREATE TABLE health_logs (
    id                  INTEGER PRIMARY KEY,
    volume_id           INTEGER NOT NULL REFERENCES volumes(id),
    session_id          INTEGER REFERENCES verification_sessions(id),
    logged_at           TEXT NOT NULL DEFAULT (datetime('now')),
    operation           TEXT NOT NULL
                        CHECK(operation IN ('write','read','verify','clean')),
    total_bytes         INTEGER,
    total_uncorrected   INTEGER,
    total_corrected     INTEGER,
    total_retries       INTEGER,
    total_rewritten     INTEGER,
    raw_log             TEXT
);

-- AUDIT TRAIL
CREATE TABLE events (
    id           INTEGER PRIMARY KEY,
    timestamp    TEXT NOT NULL DEFAULT (datetime('now')),
    entity_type  TEXT NOT NULL,
    entity_id    INTEGER NOT NULL,
    entity_label TEXT,
    action       TEXT NOT NULL,
    field        TEXT,
    old_value    TEXT,
    new_value    TEXT,
    details      TEXT,
    tenant_id    INTEGER REFERENCES tenants(id)
);

-- INDEXES
CREATE INDEX idx_units_uuid ON units(uuid);
CREATE INDEX idx_units_status ON units(status);
CREATE INDEX idx_units_tenant ON units(tenant_id);
CREATE INDEX idx_units_archive_set ON units(archive_set_id);
CREATE INDEX idx_encryption_keys_tenant ON encryption_keys(tenant_id);
CREATE INDEX idx_snapshots_unit ON snapshots(unit_id);
CREATE INDEX idx_snapshots_status ON snapshots(status);
CREATE INDEX idx_files_snapshot ON files(snapshot_id);
CREATE INDEX idx_files_path ON files(path);
CREATE INDEX idx_manifest_entries_manifest ON manifest_entries(manifest_id);
CREATE INDEX idx_stage_sets_snapshot ON stage_sets(snapshot_id);
CREATE INDEX idx_stage_sets_status ON stage_sets(status);
CREATE INDEX idx_stage_slices_stage_set ON stage_slices(stage_set_id);
CREATE INDEX idx_writes_stage_set ON writes(stage_set_id);
CREATE INDEX idx_writes_snapshot ON writes(snapshot_id);
CREATE INDEX idx_writes_volume ON writes(volume_id);
CREATE INDEX idx_writes_status ON writes(status);
CREATE INDEX idx_write_positions_write ON write_positions(write_id);
CREATE INDEX idx_write_positions_stage_slice ON write_positions(stage_slice_id);
CREATE INDEX idx_volumes_location ON volumes(location_id);
CREATE INDEX idx_volumes_status ON volumes(status);
CREATE INDEX idx_cartridges_status ON cartridges(status);
CREATE INDEX idx_cartridges_barcode ON cartridges(barcode);
CREATE INDEX idx_cartridge_volumes_cartridge ON cartridge_volumes(cartridge_id);
CREATE INDEX idx_cartridge_volumes_volume ON cartridge_volumes(volume_id);
CREATE INDEX idx_verification_sessions_volume ON verification_sessions(volume_id);
CREATE INDEX idx_verification_results_session ON verification_results(session_id);
CREATE INDEX idx_health_volume ON health_logs(volume_id);
CREATE INDEX idx_unit_path_history_unit ON unit_path_history(unit_id);
CREATE INDEX idx_events_entity ON events(entity_type, entity_id);
CREATE INDEX idx_events_timestamp ON events(timestamp);
CREATE INDEX idx_events_tenant ON events(tenant_id);
