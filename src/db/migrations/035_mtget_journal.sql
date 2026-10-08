-- 035: `mtget_journal` -- the st driver's MTIOCGET status, verbatim, at
-- each tape device's open and close and after each failed tape command
-- (issue #344, the remainder of #301; ADR-0013 §§2, 4, 7).
--
-- (Numbered 033 on the `forensics` branch; renumbered 035 at integration,
-- after master's 032 and 033.)
--
-- WHY
-- ---
-- `MTIOCGET` fills `struct mtget`: the drive type, `mt_resid` (st puts the
-- PARTITION number there, not a residual count), the density and block
-- size register (`mt_dsreg`), the generic status bits (`mt_gstat`: BOT, EOF,
-- EOT, EOD, write protect, online, door open, and GMT_CLN -- the drive
-- asking to be cleaned), the error register (`mt_erreg`, st's count of
-- recovered errors) and st's file and block number. tapectl asked for it
-- only to learn the position and threw the rest away. What the driver said
-- at the moment a tape command failed is exactly what cannot be asked again
-- afterwards.
--
-- `mt_erreg` IS READ-TO-CLEAR
-- ---------------------------
-- st's MTIOCGET handler ends `STp->recover_reg = 0; /* Clear after read */`
-- (drivers/scsi/st.c), so each reading's soft-error count is what was
-- recovered since the previous MTIOCGET on that drive, by any process.
-- tapectl itself issues one before every tape read it positions for (the
-- file cursor's check against st's own count), not only at the journalled
-- points. So the tape device adds every one of its readings' counts to a
-- tally, and `recovered_since_open` carries it: every recovered error st
-- reported to that device after its open reading, through this one. The
-- open reading's own `mt_erreg` is what accumulated before the device
-- opened. Not counted anywhere: an MTIOCGET from another process (`mt
-- status`), or tapectl's own density/no-medium probes, each on a descriptor
-- of its own opened before any tape device (st refuses a second opener).
--
-- ONE READ WHERE tapectl ALREADY HOLDS THE DESCRIPTOR
-- ---------------------------------------------------
-- st refuses a second opener, so the contact guard cannot read this itself;
-- the tape device reads it on its own descriptor and notes it: once when it
-- opens (`point = 'open'`), after a tape command fails (`'failure'`, naming
-- the command and its errno), and when it closes (`'close'`). st flushes
-- any pending write-behind first, as it does for the position reads.
--
-- THE CONTACT ROW IS THE SPINE
-- ----------------------------
-- `contact_id` names the contact the device was working for (ADR-0013 §2),
-- NULL when that contact's own INSERT failed. Readings are held in memory
-- for the contact open on the calling thread, carried to the worker threads
-- that thread starts (the tape read runs on its own thread under
-- `TapeStore::read_file`), and written when the contact closes; a reading
-- taken while no contact is open is not kept. What lands today: every
-- `failure` reading on every path -- a failed positioning command, and a
-- read that fails partway through a slice during a confirm, verify,
-- restore, read-slices or compact-read; the write paths' `open` reading,
-- because they open their store inside the contact. The read paths
-- (verify, restore, rebuild) open the store BEFORE the contact and drop it
-- after, so they keep no `open` or `close` reading, and no path keeps a
-- `close` reading while the store outlives the contact. Every row that
-- lands names the right contact; widening the window is a change to the
-- call sites, not to this table.
--
-- VERBATIM, THEN DECODED BESIDE IT
-- --------------------------------
-- The seven `struct mtget` fields are stored as the integers the kernel
-- gave; `decoded` is a JSON object of what this build makes of them (the
-- `mt_gstat` bit names, the density code, the block size, the soft error
-- count, the partition), and `tapectl_version` says which build decoded it
-- (§7). `recovered_since_open` is tapectl's tally, NULL when no device kept
-- one. `ok = 0` with `error` set is an MTIOCGET that itself failed: the
-- fields are NULL. `point` and `trigger` are free TEXT (§4).
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
