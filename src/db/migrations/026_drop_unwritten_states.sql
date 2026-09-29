-- 026: drop the schema states nothing sets (issue #362).
--
-- Five status values have been legal in a CHECK constraint since
-- 001_initial.sql and have never had a writer:
--
--     units.status      'retired'
--     snapshots.status  'superseded', 'failed'
--     volumes.status    'blank', 'missing'
--
-- The evidence: every production write of these three columns names its
-- value, and none names one of these five. `units` is written 'active'
-- (register, rebuild), 'missing' and back to 'active' (`collection sync`),
-- 'tape_only' (`unit mark-tape-only`).
-- `snapshots` is written 'created' (the DEFAULT), 'staged', 'current' (at
-- seal, and by `catalog rebuild`), 'reclaimable', 'purged' -- a newer version
-- never demotes its predecessor, so several 'current' rows per unit is the
-- normal shape and 'superseded' was never reached (`report supersedable`
-- measures releasability through `policy::reclaimable`, not this status).
-- `snapshots` 'failed' is not `stage_sets.status = 'failed'`, which is a
-- different column and stays. `volumes` is written 'initialized' (`volume
-- init`), 'active' (`volume import`), 'sealed' (seal, rebuild), 'retired',
-- 'erased' -- 'blank' was only ever the column DEFAULT, which every
-- production INSERT overrides, and `cartridge unretire` writes back only a
-- status recovered from `events` that is itself in the legal set.
--
-- A value in a CHECK that no code writes is not harmless. Every reader has to
-- decide what it means (`audit`'s scope lists, the coverage predicates'
-- exclusion lists, `--status` filters, the `is_write_target` pin), each
-- decides alone, and a future writer can adopt one silently. So they leave.
-- 'active' and 'full' on `volumes` stay: `volume import` writes 'active', and
-- 'full' is outside this issue.
--
-- NO REMAPPING (the coordinator's P2 ruling)
-- -------------------------------------------
-- 012 mapped its dead 'offsite' onto 'available' so that migration could not
-- fail on a hand-edited database. 026 deliberately does the opposite. A row
-- in one of these five states was put there by hand (or by an import of a
-- hand-edited database), and there is no mapping that is right for every
-- such row: 'retired' could mean "gone" or "done", 'superseded' could mean
-- "still on tape" or "released". Guessing silently would rewrite the
-- operator's own statement about their archive. So 026 refuses, naming every
-- offending table, state and row id in one message, and changes nothing;
-- the operator edits those rows to a state the new schema allows and runs
-- the command again.
--
-- The refusal is raised here, in SQL, before anything is touched: a TEMP
-- table whose only trigger RAISEs the message it is given, and one INSERT
-- that hands it a message only when an offender exists. TEMP, so the guard
-- never appears in `sqlite_master`; dropped again at the end of the check.
-- `src/db/mod.rs::migrate` shows a trigger-raised message as it is, without
-- `rusqlite_migration`'s dump of this whole script in front of it. The
-- CHECK constraints below remain the backstop.
--
-- NO DEFAULT ON volumes.status (the coordinator's P1 ruling)
-- ----------------------------------------------------------
-- 'blank' was the DEFAULT. 'initialized' would be the wrong replacement: it
-- is exactly what `policy::coverage::is_write_target` admits, so a bare
-- INSERT would silently mint a write target. The column becomes NOT NULL
-- with no DEFAULT, and every INSERT must say what the volume is. `units`
-- keeps DEFAULT 'active' and `snapshots` keeps DEFAULT 'created'; both are
-- still legal.
--
-- THE REBUILDS
-- ------------
-- SQLite cannot ALTER a CHECK in place, so all three tables are rebuilt in
-- this one migration, the way 012 rebuilt `cartridges` and 017 rebuilt
-- `volumes`: CREATE *_new, copy with `id` named on both sides (a copy that
-- let SQLite assign rowids would silently re-point every foreign key), DROP,
-- RENAME *_new INTO the name (never rename the old table away: with
-- `legacy_alter_table` OFF that rewrites every other table's REFERENCES
-- clause to follow it), then recreate every index. The copies are straight
-- column copies -- no CASE, per the ruling above.
--
-- Seventeen foreign keys point into these tables and none of them is touched:
-- units <- snapshots, unit_tags, unit_path_history, restores;
-- snapshots <- snapshots (base_snapshot_id), files, manifests, stage_sets,
-- writes; volumes <- writes, cartridge_volumes, cartridge_contacts,
-- health_logs, verification_sessions, volume_deposits, volume_movements,
-- restores. `snapshots_new` names its self-reference `snapshots(id)`, which
-- binds to the rebuilt table once the rename lands. FK enforcement is off
-- for the DROPs (`migrate()` turns it off outside the transaction, as for
-- 003/012/017) and the migration is registered with `.foreign_key_check()`,
-- so a lost edge fails the migration instead of landing.
--
-- No trigger or view references these tables (the only triggers are the
-- `files_fts` ones on `files`, 002). Every column, type, default and
-- constraint below is otherwise identical to the live schema: `units` and
-- `snapshots` as 001_initial.sql made them, `volumes` as 017 rebuilt it plus
-- 018's `sealed_at` in the last position, where its ADD COLUMN put it.

-- The guard.
CREATE TEMP TABLE m026_refusal (message TEXT NOT NULL);
CREATE TEMP TRIGGER m026_refuse BEFORE INSERT ON m026_refusal
BEGIN
    SELECT RAISE(ABORT, NEW.message);
END;
INSERT INTO m026_refusal (message)
SELECT
    'migration 026 cannot run: ' || group_concat(finding, '; ') || '. '
    || 'Migration 026 removes these statuses from the schema. No tapectl '
    || 'release has ever written one, so these rows were set by hand, and 026 '
    || 'will not guess what they should say. Change each named row to a status '
    || 'the new schema allows -- units: active, tape_only, missing; snapshots: '
    || 'created, staged, current, reclaimable, purged; volumes: initialized, '
    || 'active, full, retired, erased, sealed -- then run the command again. '
    || 'Nothing has been changed.'
FROM (
    SELECT 'units.status = ''' || status || ''' on ' || COUNT(*)
           || ' row(s) (id ' || group_concat(id, ', ') || ')' AS finding
      FROM units WHERE status IN ('retired') GROUP BY status
    UNION ALL
    SELECT 'snapshots.status = ''' || status || ''' on ' || COUNT(*)
           || ' row(s) (id ' || group_concat(id, ', ') || ')'
      FROM snapshots WHERE status IN ('superseded', 'failed') GROUP BY status
    UNION ALL
    SELECT 'volumes.status = ''' || status || ''' on ' || COUNT(*)
           || ' row(s) (id ' || group_concat(id, ', ') || ')'
      FROM volumes WHERE status IN ('blank', 'missing') GROUP BY status
)
HAVING COUNT(*) > 0;
DROP TRIGGER m026_refuse;
DROP TABLE m026_refusal;

-- units: 'retired' leaves.
CREATE TABLE units_new (
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
                    CHECK(status IN ('active','tape_only','missing')),
    created_at      TEXT NOT NULL DEFAULT (datetime('now')),
    last_scanned    TEXT,
    notes           TEXT
);

INSERT INTO units_new (
    id, uuid, name, tenant_id, archive_set_id, current_path, checksum_mode,
    encrypt, status, created_at, last_scanned, notes
)
SELECT
    id, uuid, name, tenant_id, archive_set_id, current_path, checksum_mode,
    encrypt, status, created_at, last_scanned, notes
FROM units;

DROP TABLE units;

ALTER TABLE units_new RENAME TO units;

CREATE INDEX idx_units_uuid ON units(uuid);
CREATE INDEX idx_units_status ON units(status);
CREATE INDEX idx_units_tenant ON units(tenant_id);
CREATE INDEX idx_units_archive_set ON units(archive_set_id);

-- snapshots: 'superseded' and 'failed' leave. `superseded_at` stays: it is a
-- column, read by staging, and dropping one is not this issue.
CREATE TABLE snapshots_new (
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

INSERT INTO snapshots_new (
    id, unit_id, version, snapshot_type, base_snapshot_id, status,
    source_path, total_size, file_count, created_at, superseded_at, notes
)
SELECT
    id, unit_id, version, snapshot_type, base_snapshot_id, status,
    source_path, total_size, file_count, created_at, superseded_at, notes
FROM snapshots;

DROP TABLE snapshots;

ALTER TABLE snapshots_new RENAME TO snapshots;

CREATE INDEX idx_snapshots_unit ON snapshots(unit_id);
CREATE INDEX idx_snapshots_status ON snapshots(status);

-- volumes: 'blank' and 'missing' leave, and so does the DEFAULT.
CREATE TABLE volumes_new (
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
                           CHECK(status IN ('initialized','active','full',
                                            'retired','erased','sealed')),
    observed_condition     TEXT NOT NULL DEFAULT 'ok'
                           CHECK(observed_condition IN ('ok','quarantined')),
    first_write            TEXT,
    last_write             TEXT,
    notes                  TEXT,
    created_at             TEXT NOT NULL DEFAULT (datetime('now')),
    uuid                   TEXT,
    sealed_at              TEXT
);

INSERT INTO volumes_new (
    id, label, backend_type, backend_name, media_type, capacity_bytes,
    mam_capacity_bytes, mam_remaining_at_start, bytes_written, num_data_files,
    has_manifest, location_id, status, observed_condition, first_write,
    last_write, notes, created_at, uuid, sealed_at
)
SELECT
    id, label, backend_type, backend_name, media_type, capacity_bytes,
    mam_capacity_bytes, mam_remaining_at_start, bytes_written, num_data_files,
    has_manifest, location_id, status, observed_condition, first_write,
    last_write, notes, created_at, uuid, sealed_at
FROM volumes;

DROP TABLE volumes;

ALTER TABLE volumes_new RENAME TO volumes;

CREATE INDEX idx_volumes_location ON volumes(location_id);
CREATE INDEX idx_volumes_status   ON volumes(status);
CREATE UNIQUE INDEX idx_volumes_uuid ON volumes(uuid);
