-- 012: the cartridge lifecycle is four states; `offsite` was never one of them.
--
-- ADR-0011 ("A cartridge's place is a location; its status is only its fitness
-- to hold data"), issue #148.
--
-- ADR-0010 made `volume init` bind the cartridge it writes, which turned the
-- cartridge lifecycle from a diagram into running code for the first time and
-- exposed two dead ends. `cartridges.status` carried five values in its CHECK
-- constraint and had three writers between them: `cartridge register` and
-- `cartridge mark-erased` set 'available', `compact-finish` sets
-- 'pending_erase'. NOTHING wrote 'retired_permanent', and NOTHING wrote
-- 'offsite' -- both were states a cartridge could be pushed toward and never
-- enter.
--
-- WHY 'offsite' LEAVES RATHER THAN GETTING A WRITER
-- -------------------------------------------------
-- It is not a status. It is a PLACE, and this table has had a place column
-- since 001_initial.sql: `location_id`, with zero readers and zero writers for
-- its entire life. A cartridge is in exactly one place (007_warehouse_locations
-- says so about locations generally), and "offsite" is a location an operator
-- named -- not a state of the medium.
--
-- Keeping both would be two mechanisms for one fact, which is how they drift: a
-- cartridge could be status='offsite' while carrying a volume whose
-- `location_id` said 'home-rack', and nothing anywhere would notice. So
-- location becomes the single mechanism (`cartridge move`, and `volume move`
-- moving the cartridge it is bound to), and the status enum keeps only what is
-- genuinely a fitness-to-hold-data judgement:
--
--     available -- volume init --> in_use -- compact-finish --> pending_erase
--         ^                                                          |
--         +------------------ cartridge mark-erased -----------------+
--         |
--         +--> retired_permanent   (cartridge retire; mark-erased is the way back)
--
-- The 'offsite' -> 'available' mapping below is for CORRECTNESS, not for data:
-- no production path has ever written that value, so in practice it maps
-- nothing. Writing it anyway means this migration cannot fail on a database
-- that got one by hand-editing.
--
-- THE REBUILD, AND THE ORPHANING TRAP IN IT
-- -----------------------------------------
-- SQLite cannot ALTER a CHECK constraint in place, so `cartridges` is rebuilt
-- the same way 003_v2_lifecycle.sql rebuilt `volumes`: create/copy/drop/rename,
-- per SQLite's documented 12-step "Making Other Kinds Of Table Schema Changes"
-- procedure. Two details are load-bearing and easy to get wrong:
--
--   1. The INSERT names `id` EXPLICITLY on both sides. `SELECT *` is
--      unavailable here (the status needs a CASE), and an INSERT that omits
--      `id` would let SQLite assign fresh rowids -- leaving every
--      `cartridge_volumes.cartridge_id` pointing at a row that is now some
--      other cartridge, or none. Silent, and exactly the bytes-to-the-wrong-
--      tape class of bug this table exists to prevent.
--
--   2. The order is CREATE-new, copy, DROP-old, RENAME -- never rename the old
--      table out of the way first. With `legacy_alter_table` OFF (the default),
--      `ALTER TABLE cartridges RENAME TO cartridges_old` would rewrite
--      `cartridge_volumes`' own `REFERENCES cartridges(id)` clause to point at
--      `cartridges_old`, orphaning the FK by construction. Renaming
--      `cartridges_new` INTO the name instead leaves that clause untouched, so
--      it re-binds to the rebuilt table by name -- the same reasoning 003 spells
--      out for the five tables that reference `volumes`.
--
-- `cartridge_volumes` is the only table with a `REFERENCES cartridges(id)`
-- foreign key. It is not touched here. FK ENFORCEMENT must be off for the DROP
-- below (src/db/mod.rs::migrate does that, outside this migration's
-- transaction, because toggling the pragma from inside a migration's own SQL is
-- a documented no-op); this migration is registered with `.foreign_key_check()`
-- so `PRAGMA foreign_key_check` runs before commit, per step 10.
--
-- Every column, type, default and constraint below is otherwise byte-for-byte
-- identical to 001_initial.sql:193 -- only the status CHECK loses 'offsite'.

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
    total_bytes_written   INTEGER DEFAULT 0,
    total_bytes_read      INTEGER DEFAULT 0,
    first_use             TEXT,
    last_use              TEXT,
    error_history         TEXT,
    location_id           INTEGER REFERENCES locations(id),
    created_at            TEXT NOT NULL DEFAULT (datetime('now')),
    notes                 TEXT
);

INSERT INTO cartridges_new (
    id, barcode, media_type, manufacturer, serial_number, tape_length_meters,
    nominal_capacity, status, total_load_count, total_bytes_written,
    total_bytes_read, first_use, last_use, error_history, location_id,
    created_at, notes
)
SELECT
    id, barcode, media_type, manufacturer, serial_number, tape_length_meters,
    nominal_capacity,
    CASE WHEN status = 'offsite' THEN 'available' ELSE status END,
    total_load_count, total_bytes_written,
    total_bytes_read, first_use, last_use, error_history, location_id,
    created_at, notes
FROM cartridges;

DROP TABLE cartridges;

ALTER TABLE cartridges_new RENAME TO cartridges;

-- Every index the old table carried, recreated. 001_initial.sql:379-380 gave it
-- the status and barcode indexes; 011_cartridge_serial_index.sql added the
-- PARTIAL unique index that makes the MAM medium serial an identity (ADR-0010).
-- A rebuild that forgets the 011 index would silently re-admit the duplicate
-- serials that index exists to prevent.
CREATE INDEX idx_cartridges_status  ON cartridges(status);
CREATE INDEX idx_cartridges_barcode ON cartridges(barcode);
CREATE UNIQUE INDEX idx_cartridges_serial_number
    ON cartridges(serial_number)
    WHERE serial_number IS NOT NULL;

-- New (ADR-0011 "Consequences"): `location_id` never had an index, because
-- until now nothing read it. `cartridge list`/`info` join it on every row and
-- `cartridge move` / `volume move` write it, so it is a real access path now.
CREATE INDEX idx_cartridges_location ON cartridges(location_id);
