-- 036: `cleaning_cartridges` -- the cleaning cartridges an operator uses on a
-- drive (issue #430; ADR-0012 amendment 2026-10-15, item 60).
--
-- WHY A TABLE OF ITS OWN
-- ----------------------
-- A cleaning cartridge is not a cartridge in the sense `cartridges` means.
-- That table is the media that hold data: a status lifecycle (available,
-- in_use, pending_erase, retired_permanent), a nominal capacity, volumes
-- bound to it through `cartridge_volumes`, a place, contacts. A cleaning
-- cartridge has none of those, and giving `cartridges` a second kind would
-- mean rebuilding the table every data tape's identity hangs on -- the
-- rebuild 012 and 033 each did, and the orphaning trap 012's header
-- describes. Item 60 rules `cartridges` is not rebuilt.
--
-- WHAT IS STORED, AND WHAT IS NOT
-- -------------------------------
-- Only the registration: which cleaning cartridge, when it was registered,
-- and how many cleanings it is rated for (an LTO Ultrium universal cleaning
-- cartridge is rated for 50; the application default). `serial` is the
-- cartridge's chip serial where its chip can be read, otherwise whatever
-- the operator uses to tell it apart.
--
-- Everything else is DERIVED, never stored. Each cleaning is an `events` row
-- (`entity_type = 'drive'`, `entity_id` the `drives` row, `action =
-- 'cleaned'`, `field = 'cleaning_cartridge'`, `new_value` the cleaning
-- cartridge's serial, NULL when the operator did not say which); a
-- cartridge's uses are the count of events naming it and a drive's last
-- cleaning the newest event on it. A counter column would be a second copy
-- of the events that can disagree with them (ADR-0013 §3), and the events
-- are append-only and never pruned (ADR-0012 2026-10-07 item 28).
-- `events(action, timestamp)` is already indexed (031).
--
-- A new table, no row read or converted, plain CREATE: no
-- `.foreign_key_check()`. Nothing references it (events name a cleaning
-- cartridge by serial, as they name a volume by label beside its id), so it
-- can be neither orphaned nor an orphan.
CREATE TABLE cleaning_cartridges (
    id            INTEGER PRIMARY KEY,
    -- UNIQUE is the identity rule; the length CHECK stops an empty string
    -- from becoming a nameless cartridge every unnamed cleaning piles into.
    serial        TEXT NOT NULL UNIQUE CHECK(length(serial) > 0),
    registered_at TEXT NOT NULL DEFAULT (datetime('now')),
    rated_uses    INTEGER NOT NULL CHECK(rated_uses > 0)
);
