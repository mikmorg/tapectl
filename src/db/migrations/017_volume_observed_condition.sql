-- 017: `volumes.status` is the operator's; a medium's condition is its own
-- fact (ADR-0012, amendment "the status column is the operator's; a
-- medium's condition is its own fact", 2026-09-17, issue #242).
--
-- Four writers set `volumes.status = 'quarantined'` unconditionally,
-- overwriting whatever the operator had put there -- including a terminal
-- `retired`. Two of them (`session.rs`'s `IdentityMismatch`/`AlreadySealed`
-- resume-contact failures) are not even facts about the MEDIUM; they are a
-- session-level divergence finding. `observed_condition` is named for that
-- reason rather than `medium_condition`: this column means "this volume is
-- out of service for something tapectl OBSERVED, never for something the
-- operator CHOSE" -- exactly what `quarantined` has always meant. The
-- operator-facing word stays `quarantined`; it moves columns, it is not
-- renamed.
--
-- `quarantined` is retired as a legal `status` value, not merely
-- deprecated: leaving it in the CHECK would let a future writer silently
-- reintroduce the very defect this migration exists to close. SQLite
-- cannot ALTER a CHECK constraint in place, so `volumes` is rebuilt the
-- same way 003_v2_lifecycle.sql rebuilt it the first time (and the way
-- 012_cartridge_lifecycle.sql rebuilt `cartridges`): create/copy/drop/
-- rename, per SQLite's documented 12-step "Making Other Kinds Of Table
-- Schema Changes" procedure. `.foreign_key_check()` is registered on this
-- migration below for the same reason as 003 and 012 -- six tables hold a
-- `REFERENCES volumes(id)` foreign key (cartridge_volumes, volume_movements,
-- writes, verification_sessions, health_logs, volume_deposits -- the last
-- added by 007_warehouse_locations.sql, after 003's header above was
-- written), and a rebuild that renumbered rows would orphan every one of
-- them silently. Every column, type, default and constraint below is
-- otherwise byte-for-byte identical to the table as 004_volume_uuid.sql and
-- 008_drop_volume_storage_class.sql left it (SELECT-* is not used because
-- the status CASE below needs an explicit column list on both sides) --
-- only the status CHECK loses 'quarantined', and `observed_condition` is a
-- new column placed right after `status`, before `first_write` -- adjacent
-- to the column whose CHECK it complements, not appended at the end where
-- 004's own `ALTER TABLE ... ADD COLUMN` landed `uuid` in the live schema.
--
-- MIGRATING EXISTING ROWS
-- ------------------------
-- For a row already `status = 'quarantined'`: `observed_condition` becomes
-- 'quarantined', and `status` is restored to the most recent `events.old_value`
-- where that event recorded the transition INTO quarantine --
-- `quarantine_on_medium_evidence` (the verify path, `src/volume/write.rs`)
-- writes exactly that row: `entity_type = 'volume'`, `field = 'status'`,
-- `new_value = 'quarantined'`, `old_value` the prior status. Where no such
-- event exists, the quarantine came from one of the three `session.rs`
-- write-path writers, which fire only mid-write on a volume that never
-- sealed -- so the fallback is `'initialized'`, NOT `'sealed'`.
--
-- This deliberately narrows this ADR's own point 4 ("status restored to
-- what it was before quarantine where the events row records it, and to
-- 'sealed' where it does not -- a quarantine only ever fired on a volume
-- that was otherwise in service"). That blanket 'sealed' fallback is right
-- for the verify path (a verify only ever runs against an already-sealed
-- volume) but wrong for the three write-path writers, which by
-- construction quarantine a session that never reached `seal`/`confirm`'s
-- success arm -- there is no sealed tape to restore to. `'initialized'` is
-- the honest value for that case, and it is safe post-migration because
-- `observed_condition` (not `status`) is what now blocks the write path
-- (`policy::coverage::is_write_target`) -- a write-path-quarantined row
-- reverting to `initialized` does not silently become writable again.
--
-- The correlated subquery below reads `events` by `entity_id` (the
-- volume's id, preserved verbatim across this rebuild -- see the trap note
-- in 012's header) and takes the MOST RECENT such event by `id` (events.id
-- is an autoincrement primary key, so `ORDER BY id DESC` is "most recent"
-- without depending on `timestamp`'s string collation).
--
-- It ALSO requires `old_value` to be one of the statuses this migration
-- still permits, which is load-bearing and not belt-and-braces. Pre-017
-- `quarantine_on_medium_evidence` had no guard: it read whatever `status`
-- held, wrote 'quarantined' over it, and logged `old_value =
-- previous_status`. Verifying an already-quarantined tape is a supported
-- path -- and a likely one, since a tape that failed a verify is exactly
-- the tape someone verifies again -- so the most recent transition INTO
-- quarantine can legally be 'quarantined' -> 'quarantined'. Restoring
-- that value would write 'quarantined' back into `status`, which the CHECK
-- three dozen lines above has just made illegal: the INSERT fails, the
-- migration aborts, `db::open` fails, and EVERY command fails with it --
-- including the `db fsck --repair` that is meant to be the way out
-- (issue #233). Filtering to the legal set makes that unreachable by
-- construction rather than by argument, and incidentally handles a NULL or
-- hand-edited `old_value` the same way: fall through to the default.
-- Pinned by `test_migration_017_restores_a_twice_quarantined_volume_not_to_quarantined`,
-- which fails with `CHECK constraint failed: status IN (...)` without it.

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
                                            'retired','missing','erased','sealed')),
    observed_condition     TEXT NOT NULL DEFAULT 'ok'
                           CHECK(observed_condition IN ('ok','quarantined')),
    first_write            TEXT,
    last_write             TEXT,
    notes                  TEXT,
    created_at             TEXT NOT NULL DEFAULT (datetime('now')),
    uuid                   TEXT
);

INSERT INTO volumes_new (
    id, label, backend_type, backend_name, media_type, capacity_bytes,
    mam_capacity_bytes, mam_remaining_at_start, bytes_written, num_data_files,
    has_manifest, location_id, status, observed_condition, first_write,
    last_write, notes, created_at, uuid
)
SELECT
    v.id, v.label, v.backend_type, v.backend_name, v.media_type, v.capacity_bytes,
    v.mam_capacity_bytes, v.mam_remaining_at_start, v.bytes_written, v.num_data_files,
    v.has_manifest, v.location_id,
    CASE WHEN v.status = 'quarantined' THEN
        COALESCE(
            (SELECT e.old_value FROM events e
              WHERE e.entity_type = 'volume' AND e.entity_id = v.id
                AND e.field = 'status' AND e.new_value = 'quarantined'
                AND e.old_value IN ('blank','initialized','active','full',
                                    'retired','missing','erased','sealed')
              ORDER BY e.id DESC LIMIT 1),
            'initialized'
        )
    ELSE v.status END,
    CASE WHEN v.status = 'quarantined' THEN 'quarantined' ELSE 'ok' END,
    v.first_write, v.last_write, v.notes, v.created_at, v.uuid
FROM volumes v;

DROP TABLE volumes;

ALTER TABLE volumes_new RENAME TO volumes;

CREATE INDEX idx_volumes_location ON volumes(location_id);
CREATE INDEX idx_volumes_status   ON volumes(status);
CREATE UNIQUE INDEX idx_volumes_uuid ON volumes(uuid);
