-- 033: `mtget_journal` -- the st driver's MTIOCGET status, verbatim, at
-- each tape device's open and close and after each failed tape command
-- (issue #344, the remainder of #301; ADR-0013 §§2, 4, 7).
--
-- (Numbered 033 on the `forensics` branch so the branch's migrations stay
-- contiguous; the coordinator renumbers it at integration.)
--
-- WHY
-- ---
-- `MTIOCGET` fills `struct mtget`: the drive type, the residual count, the
-- density and block size register (`mt_dsreg`), the generic status bits
-- (`mt_gstat`: BOT, EOF, EOT, EOD, write protect, online, door open, and
-- GMT_CLN -- the drive asking to be cleaned), the error register
-- (`mt_erreg`, the soft error count st keeps) and st's file and block
-- number. tapectl asked for it only to learn the position and threw the rest
-- away. What the driver said at the moment a tape command failed is exactly
-- what cannot be asked again afterwards.
--
-- ONE READ WHERE tapectl ALREADY HOLDS THE DESCRIPTOR
-- ---------------------------------------------------
-- st refuses a second opener, so the contact guard cannot read this itself;
-- the tape device reads it on its own descriptor: once when it opens
-- (`point = 'open'`), after a tape command fails (`'failure'`, naming the
-- command and its errno), and when it closes (`'close'`). MTIOCGET is an
-- ioctl answered by the st driver, not a log-page read: it cannot disturb a
-- read-to-clear counter. (st flushes any pending write-behind first, as it
-- does for the position reads tapectl already made.)
--
-- THE CONTACT ROW IS THE SPINE
-- ----------------------------
-- `contact_id` names the contact the device was open under (ADR-0013 §2),
-- NULL when that contact's own INSERT failed. Readings are held in memory
-- by the thread that holds the contact and written when the contact
-- closes; a reading taken while no contact is open on the thread is not
-- kept. That bounds what lands today: the write paths open their store
-- inside the contact, so their `open` reading is kept; the read paths
-- (verify, restore, rebuild) open the store BEFORE the contact and drop it
-- after, so for them only `failure` readings land. Every row that lands
-- names the right contact; widening the window is a change to the call
-- sites, not to this table.
--
-- VERBATIM, THEN DECODED BESIDE IT
-- --------------------------------
-- The seven `struct mtget` fields are stored as the integers the kernel
-- gave; `decoded` is a JSON object of what this build makes of them (the
-- `mt_gstat` bit names, the density code, the block size, the soft error
-- count), and `tapectl_version` says which build decoded it (§7). `ok = 0`
-- with `error` set is an MTIOCGET that itself failed: the fields are NULL.
-- `point` and `trigger` are free TEXT (§4).
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
    decoded          TEXT,
    tapectl_version  TEXT NOT NULL
);

CREATE INDEX idx_mtget_journal_contact ON mtget_journal(contact_id);
