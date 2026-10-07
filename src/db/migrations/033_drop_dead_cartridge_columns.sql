-- 033: drop the never-written cartridge columns (ADR-0012 amendment
-- 2026-10-07, item 35).
--
-- WHY
-- ---
-- `cartridges.total_bytes_written`, `total_bytes_read` and `error_history`
-- have been in the schema since 001_initial.sql and no release of tapectl has
-- written or read them: no INSERT names them (`cartridge register`, the
-- binding's auto-registration and `catalog rebuild`'s all list their columns
-- and none of these), no UPDATE sets them, no SELECT reads them, the on-tape
-- `catalog.db` (`db::ontape_catalog`) carries no `cartridges` table at all,
-- and `db export`/`db import` copy whatever columns exist. The 2026-09-13
-- post-redesign review found them "written and read by nothing at all". A
-- column nothing writes is one every reader has to decide the meaning of, and
-- one a future writer could adopt silently; the chip's and the drive's wear
-- figures are journalled (ADR-0013, migrations 022/023), not copied here. So
-- they leave, as 026 and 032 removed the states nothing set.
--
-- NO SILENT LOSS (026's rule)
-- ---------------------------
-- Their defaults are 0, 0 and NULL. A row carrying anything else was set by
-- hand, and dropping the column would lose that value without a word. So 033
-- refuses, naming every such row id in one message, and changes nothing; the
-- operator copies the values somewhere if they matter, resets them to their
-- defaults, and runs the command again. NULL counts as unwritten too
-- (migration 015 made a never-written count NULL for the same reason). The
-- refusal is 026's mechanism: a TEMP table whose only trigger RAISEs the
-- message it is given, and one INSERT that hands it a message only when an
-- offender exists. As 026 says of itself: apply this migration only through
-- tapectl, whose bundled SQLite accepts `RAISE(ABORT, <expression>)`.
--
-- THE REBUILD
-- -----------
-- `cartridges` is rebuilt exactly as 012 and 032 rebuilt their tables:
-- CREATE cartridges_new, copy with `id` named on both sides (a copy that let
-- SQLite assign rowids would silently re-point every foreign key), DROP,
-- RENAME cartridges_new INTO the name (never rename the old table away: with
-- `legacy_alter_table` OFF that rewrites every other table's REFERENCES clause
-- to follow it), then recreate its indexes. Every kept column keeps its
-- position, type, default and constraint, `operator_serial` (016's ALTER)
-- included at the end. 027 dropped `idx_cartridges_status`, so the indexes
-- that come back are 012's barcode and location ones and 011's PARTIAL unique
-- serial index -- the one that makes the chip serial an identity (ADR-0010),
-- whose loss would silently re-admit duplicate serials. Their text is
-- restated byte for byte. No trigger or view references `cartridges`.
--
-- The foreign keys into `cartridges` -- from cartridge_volumes and
-- cartridge_contacts -- live on the referencing tables, and neither is
-- touched. FK enforcement is off for the DROP (`migrate_to` turns it off
-- outside the transaction) and the migration is registered with
-- `.foreign_key_check()`, so a lost edge fails the migration instead of
-- landing.

CREATE TEMP TABLE m033_refusal (message TEXT NOT NULL);
CREATE TEMP TRIGGER m033_refuse BEFORE INSERT ON m033_refusal
BEGIN
    SELECT RAISE(ABORT, NEW.message);
END;
INSERT INTO m033_refusal (message)
SELECT
    'migration 033 cannot run: total_bytes_written, total_bytes_read or '
    || 'error_history is set on ' || COUNT(*) || ' cartridge row(s) (id '
    || group_concat(id, ', ') || '). '
    || 'Migration 033 removes these three columns from the schema. No tapectl '
    || 'release has ever written them, so these values were set by hand, and 033 '
    || 'will not drop them silently. Copy them somewhere if they matter, set '
    || 'each named row back to total_bytes_written = 0, total_bytes_read = 0, '
    || 'error_history = NULL, then run the command again. Nothing has been '
    || 'changed.'
FROM (SELECT id FROM cartridges
      WHERE COALESCE(total_bytes_written, 0) != 0
         OR COALESCE(total_bytes_read, 0) != 0
         OR error_history IS NOT NULL
      ORDER BY id)
HAVING COUNT(*) > 0;
DROP TRIGGER m033_refuse;
DROP TABLE m033_refusal;

CREATE TABLE cartridges_new (
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

INSERT INTO cartridges_new (
    id, barcode, media_type, manufacturer, serial_number, tape_length_meters,
    nominal_capacity, status, total_load_count, first_use, last_use,
    location_id, created_at, notes, operator_serial
)
SELECT
    id, barcode, media_type, manufacturer, serial_number, tape_length_meters,
    nominal_capacity, status, total_load_count, first_use, last_use,
    location_id, created_at, notes, operator_serial
FROM cartridges;

DROP TABLE cartridges;

ALTER TABLE cartridges_new RENAME TO cartridges;

CREATE INDEX idx_cartridges_barcode ON cartridges(barcode);
CREATE UNIQUE INDEX idx_cartridges_serial_number
    ON cartridges(serial_number)
    WHERE serial_number IS NOT NULL;
CREATE INDEX idx_cartridges_location ON cartridges(location_id);
