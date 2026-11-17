-- 001: the baseline schema of tapectl 2.x (ADR-0012 item 258(b); the plan's
-- D9; `volume-format-v3.md` Appendix A).
--
-- One file replaces the 41 migrations of tapectl 1.x. It is the schema those
-- migrations left behind, written as plain CREATE statements: every table,
-- column (in its old order), default, CHECK, foreign key, index and trigger
-- is what a 1.9.2 catalog carries, and nothing else. The relic row
-- `meta.schema_version` (written once by the first 1.x migration and never
-- moved) is not written; `PRAGMA user_version`, which the migration runner
-- keeps, is the schema's version.
--
-- There is no upgrade from 1.x (item 258(c)): a 1.x catalog is refused by
-- name when it is opened (`db::open`), not migrated.
--
-- Until `v2.0.0` is tagged this file is edited in place and no migration
-- 002 is written (D9). After the tag it is frozen, and a change is a
-- numbered migration again.
--
-- A comment inside a CREATE body is kept by SQLite in `sqlite_master` and
-- shown by `.schema`; a comment between statements is not.

-- ---------------------------------------------------------------------------
-- The mark of a 2.x catalog (D9): SQLite's `application_id`, 0x54504354, the
-- bytes "TPCT". 1.x set none (it reads 0), so `db::open` refuses a database
-- that has tables and lacks it as a 1.x catalog, by name.
-- ---------------------------------------------------------------------------

PRAGMA application_id = 1414546260;

-- ---------------------------------------------------------------------------
-- The catalog's own key-value table. Nothing reads or writes it today.
-- ---------------------------------------------------------------------------

CREATE TABLE meta (
    key   TEXT PRIMARY KEY,
    value TEXT NOT NULL
);

-- ---------------------------------------------------------------------------
-- Tenants and their keys
-- ---------------------------------------------------------------------------

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
    description TEXT,
    is_escrow   INTEGER NOT NULL DEFAULT 0
);

CREATE INDEX idx_encryption_keys_tenant ON encryption_keys(tenant_id);

-- ---------------------------------------------------------------------------
-- Places, and the archive sets' policy layer
-- ---------------------------------------------------------------------------

CREATE TABLE locations (
    id          INTEGER PRIMARY KEY,
    name        TEXT NOT NULL UNIQUE,
    description TEXT,
    created_at  TEXT NOT NULL DEFAULT (datetime('now')),
    kind        TEXT NOT NULL DEFAULT 'shelf'
                CHECK(kind IN ('shelf','warehouse'))
);

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
    updated_at               TEXT NOT NULL DEFAULT (datetime('now')),
    warehouse_copies         INTEGER,
    allow_spanning           INTEGER,
    copies_at                TEXT
);

-- ---------------------------------------------------------------------------
-- Units, their tags and the paths they were seen at
-- ---------------------------------------------------------------------------

CREATE TABLE units (
    id               INTEGER PRIMARY KEY,
    uuid             TEXT NOT NULL UNIQUE,
    name             TEXT NOT NULL UNIQUE,
    tenant_id        INTEGER NOT NULL REFERENCES tenants(id),
    archive_set_id   INTEGER REFERENCES archive_sets(id),
    current_path     TEXT,
    checksum_mode    TEXT NOT NULL DEFAULT 'mtime_size'
                     CHECK(checksum_mode IN ('mtime_size','sha256','sha256_on_archive')),
    encrypt          INTEGER NOT NULL DEFAULT 1,
    status           TEXT NOT NULL DEFAULT 'active'
                     CHECK(status IN ('active','tape_only','missing')),
    created_at       TEXT NOT NULL DEFAULT (datetime('now')),
    last_scanned     TEXT,
    notes            TEXT,
    copies_at_frozen TEXT
);

CREATE INDEX idx_units_uuid ON units(uuid);
CREATE INDEX idx_units_tenant ON units(tenant_id);
CREATE INDEX idx_units_archive_set ON units(archive_set_id);

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

CREATE INDEX idx_unit_path_history_unit ON unit_path_history(unit_id);

-- ---------------------------------------------------------------------------
-- Versions (snapshots) and their files: `paths` interns each path once per
-- unit, `file_versions` is one version's file list, and `paths_fts` is the
-- full-text search over `paths`, kept by the three triggers.
-- ---------------------------------------------------------------------------

CREATE TABLE snapshots (
    id               INTEGER PRIMARY KEY,
    unit_id          INTEGER NOT NULL REFERENCES units(id),
    version          INTEGER NOT NULL,
    snapshot_type    TEXT NOT NULL DEFAULT 'full'
                     CHECK(snapshot_type IN ('full','differential','incremental')),
    base_snapshot_id INTEGER REFERENCES snapshots(id),
    status           TEXT NOT NULL DEFAULT 'created'
                     CHECK(status IN ('created','staged','current',
                                      'reclaimable','purged')),
    source_path      TEXT NOT NULL,
    total_size       INTEGER,
    file_count       INTEGER,
    created_at       TEXT NOT NULL DEFAULT (datetime('now')),
    superseded_at    TEXT,
    notes            TEXT,
    UNIQUE(unit_id, version)
);

CREATE INDEX idx_snapshots_unit ON snapshots(unit_id);

CREATE TABLE paths (
    id      INTEGER PRIMARY KEY,
    unit_id INTEGER NOT NULL REFERENCES units(id),
    path    TEXT NOT NULL,
    UNIQUE(unit_id, path)
);

-- `kind`: 0 a regular file, 1 a directory, 2 a symbolic link, 3 anything
-- else. `mtime_ns`: nanoseconds since the epoch, NULL where none was
-- recorded. `sha256`: 32 raw bytes or NULL.
CREATE TABLE file_versions (
    snapshot_id INTEGER NOT NULL REFERENCES snapshots(id),
    path_id     INTEGER NOT NULL REFERENCES paths(id),
    kind        INTEGER NOT NULL CHECK(kind IN (0, 1, 2, 3)),
    size_bytes  INTEGER NOT NULL,
    mtime_ns    INTEGER,
    sha256      BLOB CHECK(sha256 IS NULL
                           OR (typeof(sha256) = 'blob' AND length(sha256) = 32)),
    link_target TEXT,
    PRIMARY KEY (snapshot_id, path_id)
) WITHOUT ROWID;

CREATE VIRTUAL TABLE paths_fts USING fts5(path, content='paths', content_rowid='id');

CREATE TRIGGER paths_ai AFTER INSERT ON paths BEGIN
    INSERT INTO paths_fts(rowid, path) VALUES (new.id, new.path);
END;

CREATE TRIGGER paths_ad AFTER DELETE ON paths BEGIN
    INSERT INTO paths_fts(paths_fts, rowid, path) VALUES('delete', old.id, old.path);
END;

CREATE TRIGGER paths_au BEFORE UPDATE OF unit_id, path ON paths BEGIN
    SELECT RAISE(ABORT, 'paths rows are never changed in place; intern the new path instead');
END;

-- ---------------------------------------------------------------------------
-- Staging: a stage set is one version staged once, its slices the encrypted
-- pieces waiting on the staging device; `dar_create_reports` keeps what the
-- archiver said about each staging.
-- ---------------------------------------------------------------------------

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
    notes                TEXT,
    origin               TEXT NOT NULL DEFAULT 'staged'
                         CHECK(origin IN ('staged', 'rebuilt'))
);

CREATE INDEX idx_stage_sets_snapshot ON stage_sets(snapshot_id);

CREATE TABLE stage_slices (
    id               INTEGER PRIMARY KEY,
    stage_set_id     INTEGER NOT NULL REFERENCES stage_sets(id),
    slice_number     INTEGER NOT NULL,
    size_bytes       INTEGER NOT NULL,
    encrypted_bytes  INTEGER NOT NULL,
    sha256_plain     TEXT NOT NULL,
    sha256_encrypted TEXT NOT NULL,
    staging_path     TEXT,
    released_at      TEXT,
    UNIQUE(stage_set_id, slice_number)
);

CREATE INDEX idx_stage_slices_stage_set ON stage_slices(stage_set_id);

CREATE TABLE dar_create_reports (
    id                INTEGER PRIMARY KEY,
    captured_at       TEXT NOT NULL DEFAULT (datetime('now')),
    stage_set_id      INTEGER REFERENCES stage_sets(id) ON DELETE SET NULL,
    unit_name         TEXT,
    snapshot_version  INTEGER,
    outcome           TEXT NOT NULL,
    dar_command       TEXT NOT NULL,
    dar_exit_code     INTEGER,
    dar_stderr        BLOB,
    dar_version       TEXT,
    tapectl_version   TEXT NOT NULL
);

CREATE INDEX idx_dar_create_reports_stage_set ON dar_create_reports(stage_set_id);

-- ---------------------------------------------------------------------------
-- Cartridges (the physical medium, known by its chip serial) and volumes
-- (one logical write of a cartridge), and where each is
-- ---------------------------------------------------------------------------

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
                                           'retired_permanent')),
    total_load_count      INTEGER DEFAULT 0,
    first_use             TEXT,
    last_use              TEXT,
    location_id           INTEGER REFERENCES locations(id),
    created_at            TEXT NOT NULL DEFAULT (datetime('now')),
    notes                 TEXT,
    operator_serial       TEXT
);

CREATE INDEX idx_cartridges_barcode ON cartridges(barcode);
CREATE UNIQUE INDEX idx_cartridges_serial_number
    ON cartridges(serial_number)
    WHERE serial_number IS NOT NULL;
CREATE INDEX idx_cartridges_location ON cartridges(location_id);

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
    status                 TEXT NOT NULL
                           CHECK(status IN ('initialized','active',
                                            'retired','erased','sealed')),
    observed_condition     TEXT NOT NULL DEFAULT 'ok'
                           CHECK(observed_condition IN ('ok','quarantined')),
    first_write            TEXT,
    last_write             TEXT,
    notes                  TEXT,
    created_at             TEXT NOT NULL DEFAULT (datetime('now')),
    uuid                   TEXT,
    sealed_at              TEXT,
    ended_after            INTEGER,
    write_lbp              TEXT CHECK (write_lbp IN ('on', 'off', 'mixed')),
    write_lbp_note         TEXT
);

CREATE INDEX idx_volumes_location ON volumes(location_id);
CREATE UNIQUE INDEX idx_volumes_uuid ON volumes(uuid);

CREATE TABLE cartridge_volumes (
    id              INTEGER PRIMARY KEY,
    cartridge_id    INTEGER NOT NULL REFERENCES cartridges(id),
    volume_id       INTEGER NOT NULL REFERENCES volumes(id),
    mounted_at      TEXT NOT NULL DEFAULT (datetime('now')),
    unmounted_at    TEXT,
    identity_source TEXT
                    CHECK (identity_source IS NULL OR identity_source IN ('mam','operator')),
    UNIQUE(volume_id)
);

CREATE INDEX idx_cartridge_volumes_cartridge ON cartridge_volumes(cartridge_id);
CREATE INDEX idx_cartridge_volumes_volume ON cartridge_volumes(volume_id);

CREATE TABLE volume_movements (
    id            INTEGER PRIMARY KEY,
    volume_id     INTEGER NOT NULL REFERENCES volumes(id),
    from_location INTEGER REFERENCES locations(id),
    to_location   INTEGER NOT NULL REFERENCES locations(id),
    moved_at      TEXT NOT NULL DEFAULT (datetime('now')),
    notes         TEXT
);

CREATE TABLE volume_deposits (
    id            INTEGER PRIMARY KEY,
    volume_id     INTEGER NOT NULL REFERENCES volumes(id),
    location_id   INTEGER NOT NULL REFERENCES locations(id),
    deposited_at  TEXT NOT NULL DEFAULT (datetime('now')),
    receipt       TEXT,
    storage_class TEXT,
    notes         TEXT,
    UNIQUE(volume_id, location_id)
);

CREATE INDEX idx_volume_deposits_volume ON volume_deposits(volume_id);

CREATE TABLE cleaning_cartridges (
    id            INTEGER PRIMARY KEY,
    -- UNIQUE is the identity rule; the length CHECK stops an empty string
    -- from becoming a nameless cartridge every unnamed cleaning piles into.
    serial        TEXT NOT NULL UNIQUE CHECK(length(serial) > 0),
    registered_at TEXT NOT NULL DEFAULT (datetime('now')),
    rated_uses    INTEGER NOT NULL CHECK(rated_uses > 0)
);

-- ---------------------------------------------------------------------------
-- Writes: one stage set on one volume, and where each of its slices went
-- ---------------------------------------------------------------------------

CREATE TABLE writes (
    id                  INTEGER PRIMARY KEY,
    stage_set_id        INTEGER NOT NULL REFERENCES stage_sets(id),
    snapshot_id         INTEGER NOT NULL REFERENCES snapshots(id),
    volume_id           INTEGER NOT NULL REFERENCES volumes(id),
    status              TEXT NOT NULL DEFAULT 'planned'
                        CHECK(status IN ('planned','in_progress','completed',
                                         'failed','aborted','interrupted',
                                         'cut','range')),
    write_verified      INTEGER NOT NULL DEFAULT 0,
    eot_recovery        TEXT CHECK(eot_recovery IN (NULL, 'normal',
                                   'overwrite_incomplete', 'sacrifice_last_slice')),
    sacrificed_slice_id INTEGER REFERENCES stage_slices(id),
    started_at          TEXT,
    completed_at        TEXT,
    notes               TEXT,
    created_at          TEXT NOT NULL DEFAULT (datetime('now')),
    session_dir         TEXT,
    may_span            INTEGER NOT NULL DEFAULT 0,
    UNIQUE(stage_set_id, volume_id),
    CHECK(status <> 'range' OR may_span = 1)
);

CREATE INDEX idx_writes_stage_set ON writes(stage_set_id);
CREATE INDEX idx_writes_snapshot ON writes(snapshot_id);
CREATE INDEX idx_writes_volume ON writes(volume_id);

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

CREATE INDEX idx_write_positions_write ON write_positions(write_id);
CREATE INDEX idx_write_positions_stage_slice ON write_positions(stage_slice_id);

-- ---------------------------------------------------------------------------
-- Verification: a session reads a volume back, a result is one slice's
-- verdict, and a checkpoint is how far a full readback got
-- ---------------------------------------------------------------------------

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

CREATE INDEX idx_verification_sessions_volume ON verification_sessions(volume_id);

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

CREATE INDEX idx_verification_results_session ON verification_results(session_id);

CREATE TABLE readback_checkpoints (
    session_id          INTEGER NOT NULL
                        REFERENCES verification_sessions(id) ON DELETE CASCADE,
    position            INTEGER NOT NULL CHECK(position >= 0),
    sha256              TEXT NOT NULL,
    front_index_sha256  TEXT NOT NULL,
    checked_at          TEXT NOT NULL DEFAULT (datetime('now')),
    PRIMARY KEY (session_id, position)
);

-- ---------------------------------------------------------------------------
-- `volume rewrite`: the marker that holds a rewrite's stage sets while the
-- staged slices are the only copy (ADR-0012 item 106)
-- ---------------------------------------------------------------------------

CREATE TABLE pending_rewrites (
    id                INTEGER PRIMARY KEY,
    -- The volume being rewritten. Its row is kept (retired, then erased).
    source_volume_id  INTEGER NOT NULL REFERENCES volumes(id),
    -- The label of the volume that will carry the slices.
    destination_label TEXT NOT NULL CHECK(length(destination_label) > 0),
    -- The cartridge being reused, by the chip's serial (NULL: unreadable).
    cartridge_serial  TEXT,
    -- How the retirement was consented to: `copies_elsewhere` (every live
    -- slice has a copy on another eligible volume: the usual ADR-0008 Tier-2
    -- gate), `typed_label` (the source's label typed at a terminal) or
    -- `typed_flag` (`--only-copy-on-staging`, matching it).
    consent           TEXT NOT NULL
                      CHECK(consent IN ('copies_elsewhere', 'typed_label', 'typed_flag')),
    -- `--with-staged`: the write carries whatever else is staged beside the
    -- rewrite's own sets. Kept so `--resume` writes what the first run meant.
    with_staged       INTEGER NOT NULL DEFAULT 0 CHECK(with_staged IN (0, 1)),
    -- How far the command got. `staged`: the slices are staged and verified
    -- and nothing destructive has happened. `retired`: the source is retired.
    -- `erased`: its cartridge is erased (the window is open). `initialized`:
    -- the destination volume exists. `written`: the destination is sealed and
    -- fully read back and only the release is left.
    state             TEXT NOT NULL DEFAULT 'staged'
                      CHECK(state IN ('staged', 'retired', 'erased', 'initialized', 'written')),
    started_at        TEXT NOT NULL DEFAULT (datetime('now')),
    -- When the erase began: the window opens here.
    window_opened_at  TEXT,
    finished_at       TEXT,
    outcome           TEXT CHECK(outcome IN ('completed', 'abandoned')),
    CHECK((finished_at IS NULL) = (outcome IS NULL))
);

CREATE UNIQUE INDEX idx_pending_rewrites_open_source
    ON pending_rewrites(source_volume_id) WHERE finished_at IS NULL;
CREATE UNIQUE INDEX idx_pending_rewrites_open_destination
    ON pending_rewrites(destination_label) WHERE finished_at IS NULL;

CREATE TABLE pending_rewrite_sets (
    rewrite_id   INTEGER NOT NULL REFERENCES pending_rewrites(id),
    stage_set_id INTEGER NOT NULL REFERENCES stage_sets(id),
    PRIMARY KEY (rewrite_id, stage_set_id)
);

CREATE INDEX idx_pending_rewrite_sets_stage_set
    ON pending_rewrite_sets(stage_set_id);

-- ---------------------------------------------------------------------------
-- Drives, contacts and what each contact read (ADR-0013). A contact is one
-- time a cartridge was in a drive under a tapectl command; the journals keep
-- what was read during it, verbatim. Their `contact_id` is NULL where no
-- contact could be named (the contact's own INSERT failed). A journal's
-- `trigger` is the command, verbatim: free TEXT, no CHECK (ADR-0013 §4).
-- ---------------------------------------------------------------------------

CREATE TABLE drives (
    id            INTEGER PRIMARY KEY,
    -- UNIQUE is the identity rule, NOT NULL is the "no guessed row" rule, and
    -- the length CHECK stops an empty string from becoming a third, nameless
    -- drive that every unidentified contact would then pile into.
    serial        TEXT NOT NULL UNIQUE CHECK(length(serial) > 0),
    vendor        TEXT,
    model         TEXT,
    firmware_rev  TEXT,
    first_seen    TEXT NOT NULL DEFAULT (datetime('now')),
    last_seen     TEXT NOT NULL DEFAULT (datetime('now'))
);

CREATE TABLE cartridge_contacts (
    id                INTEGER PRIMARY KEY,
    -- NULL when the cartridge could not be identified; `identity_reason`
    -- then says which of the reasons it was.
    cartridge_id      INTEGER REFERENCES cartridges(id),
    -- NULL: not every contact has a volume (`volume identify`,
    -- `restore raw-volume`).
    volume_id         INTEGER REFERENCES volumes(id),
    -- NULL when the drive published no serial.
    drive_id          INTEGER REFERENCES drives(id),
    -- The command, verbatim: 'volume init', 'volume verify', ...
    operation         TEXT NOT NULL,
    -- The path as given, by-id or /dev/nstN.
    device            TEXT NOT NULL,
    -- NULL on a DR machine with no `backend add`.
    backend_name      TEXT,
    -- Why `cartridge_id` is NULL; NULL when it is not.
    identity_reason   TEXT,
    -- MAM "Load count" at THIS contact -- an observation, not a running
    -- total, and not a copy of `cartridges.total_load_count`.
    chip_load_count   INTEGER,
    opened_at         TEXT NOT NULL DEFAULT (datetime('now')),
    -- NULL = did not close. Nothing sweeps it.
    closed_at         TEXT,
    -- NULL until closed.
    outcome           TEXT,
    -- Free text: error string, counts, notes.
    detail            TEXT
);

CREATE INDEX idx_cartridge_contacts_cartridge ON cartridge_contacts(cartridge_id, opened_at);
CREATE INDEX idx_cartridge_contacts_volume ON cartridge_contacts(volume_id);

CREATE TABLE health_logs (
    id                  INTEGER PRIMARY KEY,
    -- NULL: a drive-only reading has no volume. A real volume when it is not
    -- NULL.
    volume_id           INTEGER REFERENCES volumes(id),
    -- The contact this reading was taken during (ADR-0013 §2).
    contact_id          INTEGER REFERENCES cartridge_contacts(id),
    -- The `verification_sessions` row, and nothing else (ADR-0013 §3).
    session_id          INTEGER REFERENCES verification_sessions(id),
    logged_at           TEXT NOT NULL DEFAULT (datetime('now')),
    -- Free TEXT (ADR-0013 §4), pinned by a test rather than a CHECK. What
    -- KIND of reading this is -- a DIFFERENT vocabulary from
    -- `cartridge_contacts.operation`, which is the command verbatim.
    operation           TEXT NOT NULL,
    total_bytes         INTEGER,
    total_uncorrected   INTEGER,
    total_corrected     INTEGER,
    total_retries       INTEGER,
    total_rewritten     INTEGER,
    raw_log             TEXT,
    -- Nullable, NO default: NULL is "not recorded"; 0 is "recorded, none
    -- raised".
    tape_alerts         INTEGER,
    -- Which build wrote this row (ADR-0013 §7).
    tapectl_version     TEXT
);

CREATE INDEX idx_health_volume ON health_logs(volume_id);
CREATE INDEX idx_health_contact ON health_logs(contact_id);

CREATE TABLE mam_journal (
    id               INTEGER PRIMARY KEY,
    -- When the READ happened (the capture's clock), not when the row was
    -- inserted; the default serves only hand inserts.
    captured_at      TEXT NOT NULL DEFAULT (datetime('now')),
    contact_id       INTEGER REFERENCES cartridge_contacts(id),
    device_sg        TEXT NOT NULL,
    device_tape      TEXT,
    trigger          TEXT NOT NULL,
    -- The call site that took the read.
    hook             TEXT NOT NULL,
    -- The chip's `Medium serial number`, never an operator claim.
    serial_as_read   TEXT,
    ok               INTEGER NOT NULL CHECK (ok IN (0, 1)),
    error            TEXT,
    -- JSON array, program first: ["sg_read_attr", "/dev/sg1"].
    tool_argv        TEXT NOT NULL,
    tool_version     TEXT,
    raw              TEXT,
    parsed_json      TEXT,
    tapectl_version  TEXT NOT NULL
);

CREATE INDEX idx_mam_journal_contact ON mam_journal(contact_id);
CREATE INDEX idx_mam_journal_serial ON mam_journal(serial_as_read, captured_at);

CREATE TABLE log_page_journal (
    id               INTEGER PRIMARY KEY,
    -- When the READ happened (the capture's clock), not when the row was
    -- inserted; the default serves only hand inserts.
    captured_at      TEXT NOT NULL DEFAULT (datetime('now')),
    contact_id       INTEGER REFERENCES cartridge_contacts(id),
    device_sg        TEXT NOT NULL,
    device_tape      TEXT,
    trigger          TEXT NOT NULL,
    page_code        INTEGER NOT NULL CHECK (page_code BETWEEN 0 AND 255),
    subpage_code     INTEGER NOT NULL DEFAULT 0 CHECK (subpage_code BETWEEN 0 AND 255),
    ok               INTEGER NOT NULL CHECK (ok IN (0, 1)),
    error            TEXT,
    -- JSON array, program first: ["sg_logs", "--page=0x2e", "--raw", "/dev/sg1"].
    tool_argv        TEXT NOT NULL,
    tool_version     TEXT,
    raw              BLOB,
    decoded          TEXT,
    tapectl_version  TEXT NOT NULL
);

CREATE INDEX idx_log_page_journal_contact ON log_page_journal(contact_id);
CREATE INDEX idx_log_page_journal_page ON log_page_journal(page_code, captured_at);

CREATE TABLE st_stats_journal (
    id               INTEGER PRIMARY KEY,
    -- When the READING was taken (millisecond precision, UTC), not when the
    -- row was inserted; the default serves only hand inserts.
    captured_at      TEXT NOT NULL DEFAULT (strftime('%Y-%m-%d %H:%M:%f', 'now')),
    contact_id       INTEGER REFERENCES cartridge_contacts(id),
    point            TEXT NOT NULL CHECK (point IN ('open', 'close')),
    trigger          TEXT NOT NULL,
    -- The device path the command was given, spelled as given.
    device           TEXT NOT NULL,
    -- The `stats/` directory actually read.
    sysfs_dir        TEXT NOT NULL,
    -- {"<file>": "<text verbatim>", ...}
    stats_json       TEXT NOT NULL,
    -- {"<file>": "<why it is not in stats_json>", ...}; NULL when none.
    errors_json      TEXT,
    tapectl_version  TEXT NOT NULL
);

CREATE INDEX idx_st_stats_journal_contact ON st_stats_journal(contact_id);

CREATE TABLE mtget_journal (
    id               INTEGER PRIMARY KEY,
    captured_at      TEXT NOT NULL,
    contact_id       INTEGER REFERENCES cartridge_contacts(id),
    point            TEXT NOT NULL,
    trigger          TEXT NOT NULL,
    device           TEXT NOT NULL,
    command          TEXT,
    errno            INTEGER,
    ok               INTEGER NOT NULL CHECK (ok IN (0, 1)),
    error            TEXT,
    mt_type          INTEGER,
    mt_resid         INTEGER,
    mt_dsreg         INTEGER,
    mt_gstat         INTEGER,
    mt_erreg         INTEGER,
    mt_fileno        INTEGER,
    mt_blkno         INTEGER,
    recovered_since_open INTEGER,
    decoded          TEXT,
    tapectl_version  TEXT NOT NULL
);

CREATE INDEX idx_mtget_journal_contact ON mtget_journal(contact_id);

-- ---------------------------------------------------------------------------
-- Restores: one row per restore opened, on success and on failure alike
-- ---------------------------------------------------------------------------

CREATE TABLE restores (
    id               INTEGER PRIMARY KEY,
    -- NULL: the contact's own INSERT failed.
    contact_id       INTEGER REFERENCES cartridge_contacts(id),
    volume_id        INTEGER REFERENCES volumes(id),
    -- The label asked for (unit/file), or the tape's own claim (raw-volume);
    -- NULL when a raw dump failed before File 0 could say.
    volume_label     TEXT,
    unit_id          INTEGER REFERENCES units(id),
    unit_name        TEXT,
    version          INTEGER,
    -- 'unit', 'file' or 'raw-volume'. Free TEXT, no CHECK (ADR-0013 §4).
    kind             TEXT NOT NULL,
    -- The one path asked for; NULL unless kind = 'file'.
    file_path        TEXT,
    destination      TEXT NOT NULL,
    -- The restore's own clock, in datetime('now')'s spelling.
    started_at       TEXT NOT NULL,
    finished_at      TEXT NOT NULL,
    -- 'ok' or 'failed' -- the contact row's vocabulary.
    outcome          TEXT NOT NULL,
    error            TEXT,
    slices_read      INTEGER,
    bytes_restored   INTEGER,
    files_restored   INTEGER,
    -- JSON array, program first: ["dar", "-x", "/dest/.tapectl-restore-tmp/restore", ...].
    dar_argv         TEXT,
    dar_exit_code    INTEGER,
    dar_stdout       TEXT,
    dar_stderr       TEXT,
    dar_version      TEXT,
    tapectl_version  TEXT NOT NULL
);

CREATE INDEX idx_restores_contact ON restores(contact_id);
CREATE INDEX idx_restores_unit ON restores(unit_name, started_at);
CREATE INDEX idx_restores_volume ON restores(volume_id);

-- ---------------------------------------------------------------------------
-- How long each phase of a long command took (issue #386)
-- ---------------------------------------------------------------------------

CREATE TABLE phase_timings (
    id            INTEGER PRIMARY KEY,
    session       TEXT NOT NULL,
    operation     TEXT NOT NULL,
    volume_id     INTEGER REFERENCES volumes(id) ON DELETE SET NULL,
    stage_set_id  INTEGER REFERENCES stage_sets(id) ON DELETE SET NULL,
    seq           INTEGER NOT NULL,
    phase         TEXT NOT NULL,
    started_at    TEXT NOT NULL,
    duration_ms   INTEGER NOT NULL,
    bytes         INTEGER,
    outcome       TEXT NOT NULL
);

CREATE INDEX idx_phase_timings_volume ON phase_timings(volume_id);
CREATE INDEX idx_phase_timings_stage_set ON phase_timings(stage_set_id);

-- ---------------------------------------------------------------------------
-- The audit trail
-- ---------------------------------------------------------------------------

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

CREATE INDEX idx_events_entity ON events(entity_type, entity_id);
CREATE INDEX idx_events_timestamp ON events(timestamp);
CREATE INDEX idx_events_tenant ON events(tenant_id);
CREATE INDEX idx_events_action_timestamp ON events(action, timestamp);
