-- 032: drop volume status 'full' (ADR-0012 amendment 2026-10-07, item 16).
--
-- WHY
-- ---
-- 'full' has been legal in `volumes.status` since 001_initial.sql and has
-- never had a writer: no release of tapectl has set it, before the Layout v2
-- write path or since (`git log -G` over src finds no write of it). It
-- survived 026, which dropped the five other unwritten states (issue #362),
-- only because it was outside that issue. Every reader still had to decide
-- what it meant -- the coverage predicates counted it as sealed-equivalent
-- for "pre-renovation" volumes that never existed -- and a future writer
-- could adopt it silently. So it leaves, as 026's five did, and the legal set
-- becomes: initialized, active, retired, erased, sealed.
--
-- NO REMAPPING (026's rule)
-- -------------------------
-- A row carrying 'full' was put there by hand, and there is no mapping that
-- is right for every such row ('sealed' claims a seal nobody recorded,
-- 'active' an import that never happened). So 032 refuses, naming every
-- offending row id in one message, and changes nothing; the operator edits
-- those rows to a status the new schema allows and runs the command again.
-- The refusal is 026's mechanism: a TEMP table whose only trigger RAISEs the
-- message it is given, and one INSERT that hands it a message only when an
-- offender exists. `src/db/mod.rs::migrate_to` shows a trigger-raised
-- message as it is. The CHECK below remains the backstop. As 026 says of
-- itself: apply this migration only through tapectl, whose bundled SQLite
-- accepts `RAISE(ABORT, <expression>)`.
--
-- THE REBUILD
-- -----------
-- SQLite cannot ALTER a CHECK in place, so `volumes` is rebuilt exactly as
-- 026 rebuilt it: CREATE volumes_new, copy with `id` named on both sides (a
-- copy that let SQLite assign rowids would silently re-point every foreign
-- key), DROP, RENAME volumes_new INTO the name (never rename the old table
-- away: with `legacy_alter_table` OFF that rewrites every other table's
-- REFERENCES clause to follow it), then recreate its indexes. Every column,
-- type, default and constraint is 026's but for the one value; `status`
-- keeps NOT NULL with no DEFAULT. 027 dropped `idx_volumes_status` (issue
-- #373), so only `idx_volumes_location` and the UNIQUE `idx_volumes_uuid`
-- come back. No trigger or view references `volumes`.
--
-- The foreign keys into `volumes` -- from writes, cartridge_volumes,
-- cartridge_contacts, health_logs, verification_sessions, volume_deposits,
-- volume_movements, restores and phase_timings -- live on the referencing
-- tables, and none of them is touched. FK enforcement is off for
-- the DROP (`migrate_to` turns it off outside the transaction) and the
-- migration is registered with `.foreign_key_check()`, so a lost edge fails
-- the migration instead of landing.

CREATE TEMP TABLE m032_refusal (message TEXT NOT NULL);
CREATE TEMP TRIGGER m032_refuse BEFORE INSERT ON m032_refusal
BEGIN
    SELECT RAISE(ABORT, NEW.message);
END;
INSERT INTO m032_refusal (message)
SELECT
    'migration 032 cannot run: volumes.status = ''full'' on ' || COUNT(*)
    || ' row(s) (id ' || group_concat(id, ', ') || '). '
    || 'Migration 032 removes this status from the schema. No tapectl release '
    || 'has ever written it, so these rows were set by hand, and 032 will not '
    || 'guess what they should say. Change each named row to a status the new '
    || 'schema allows -- initialized, active, retired, erased, sealed -- then '
    || 'run the command again. Nothing has been changed.'
FROM (SELECT id FROM volumes WHERE status = 'full' ORDER BY id)
HAVING COUNT(*) > 0;
DROP TRIGGER m032_refuse;
DROP TABLE m032_refusal;

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
                           CHECK(status IN ('initialized','active',
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
CREATE UNIQUE INDEX idx_volumes_uuid ON volumes(uuid);
