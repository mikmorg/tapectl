-- v2 lifecycle: sealed/quarantined volume statuses (ADR-0007) + escrow key flag (ADR-0005).
-- Per decision-sheet §3.6 (docs/design/v2-open-questions.md).
--
-- SQLite cannot ALTER a CHECK constraint in place, so `volumes` is rebuilt: create
-- volumes_new with the extended status CHECK, copy every row across, drop the old
-- table, rename the new one into place, recreate its two indexes. Every column,
-- default, and constraint below is otherwise byte-for-byte identical to the table
-- in 001_initial.sql (which migration 002 never touched) -- only the CHECK list
-- gains 'sealed' and 'quarantined'.
--
-- Five tables hold a `REFERENCES volumes(id)` foreign key: cartridge_volumes,
-- volume_movements, writes, verification_sessions, health_logs. Those FK clauses
-- bind to the table by name, so after the rename below they automatically re-bind
-- to the rebuilt `volumes` -- none of the five need to change and none are touched
-- here.
--
-- Foreign key ENFORCEMENT must be off for the DROP TABLE below to succeed while
-- those five tables hold rows referencing volumes(id) (confirmed: rusqlite_migration
-- does not do this itself -- see src/db/mod.rs::migrate and the commit message for
-- the verified finding). This migration is registered in migrations() with
-- `.foreign_key_check()` so `PRAGMA foreign_key_check` runs before commit, per step
-- 10 of SQLite's documented 12-step "Making Other Kinds Of Table Schema Changes"
-- procedure (steps 1/12 -- disable/re-enable -- live in migrate(), outside this
-- migration's transaction, since toggling the pragma from inside a migration's own
-- SQL is a documented no-op).

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
    status                 TEXT NOT NULL DEFAULT 'blank'
                           CHECK(status IN ('blank','initialized','active','full',
                                            'retired','missing','erased','sealed',
                                            'quarantined')),
    storage_class          TEXT,
    first_write            TEXT,
    last_write             TEXT,
    notes                  TEXT,
    created_at             TEXT NOT NULL DEFAULT (datetime('now'))
);

INSERT INTO volumes_new SELECT * FROM volumes;

DROP TABLE volumes;

ALTER TABLE volumes_new RENAME TO volumes;

CREATE INDEX idx_volumes_location ON volumes(location_id);
CREATE INDEX idx_volumes_status   ON volumes(status);

-- escrow flag (ADR-0005): plain ADD COLUMN, no rebuild needed -- encryption_keys has no
-- CHECK change; its key_type CHECK('primary','backup') is left completely untouched.
ALTER TABLE encryption_keys ADD COLUMN is_escrow INTEGER NOT NULL DEFAULT 0;
