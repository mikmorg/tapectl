-- 011: the MAM medium serial number is how tapectl recognises a cartridge.
--
-- ADR-0010 ("Init binds the cartridge"): `volume init` reads the loaded
-- medium's MAM serial and matches it to `cartridges.serial_number` to decide
-- WHICH physical cartridge it is writing — the knowledge that was missing for
-- the whole life of the `cartridge_volumes` join, which no production path
-- had ever written. Failing a match it auto-registers a cartridge whose
-- barcode IS that serial.
--
-- Both of those make the serial an identity, and an identity that repeats is
-- worse than none: two rows with the same serial mean `volume init` binds to
-- an arbitrary one of them, silently filing this tape's volume under the
-- other cartridge's history.
--
-- PARTIAL (WHERE serial_number IS NOT NULL) because NULL is the normal state
-- of a hand-registered cartridge that has never been loaded, and of every
-- cartridge registered before this migration. SQLite's plain UNIQUE already
-- treats NULLs as distinct, but saying it here documents the intent and keeps
-- the index off the rows that do not carry the identity.
--
-- IF NOT EXISTS so re-running against a database that already has the index
-- (one created by a future `db import` of a newer export) is a no-op rather
-- than an error.
CREATE UNIQUE INDEX IF NOT EXISTS idx_cartridges_serial_number
    ON cartridges(serial_number)
    WHERE serial_number IS NOT NULL;
