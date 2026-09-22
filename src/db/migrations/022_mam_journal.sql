-- 022: `mam_journal` -- every MAM read, verbatim (ADR-0013 §5, §7; issue #297).
--
-- WHY
-- ---
-- `tape::mam::read_mam` runs `sg_read_attr` with no filter, and until this
-- migration kept eight attributes and threw the rest away. What it threw away
-- includes the four-deep "Density vendor/serial number at last load / load-1
-- / load-2 / load-3" ring and the per-load byte counters -- and loading the
-- cartridge OVERWRITES them. There is no catching up later: a cartridge's
-- MAM in 2026 cannot be re-observed in 2031. ADR-0013's standard is
-- "capture everything verbatim now, parse it later"; `raw` is that capture,
-- and `health_logs.raw_log` (migration 021) is its precedent.
--
-- THE JOURNAL POINTS AT THE CONTACT, NEVER THE REVERSE
-- ---------------------------------------------------
-- ADR-0013 §5. A single read-path command performs TWO MAM reads inside ONE
-- contact (`check_read_contact`, then the pre-store read), so one contact has
-- many journal rows and the foreign key lives on the many side.
-- `contact_id` is NULLABLE: a read can happen with no contact to name -- the
-- command refused between the read and the moment its contact would have
-- opened, or the contact's own INSERT failed (bookkeeping is best-effort and
-- never refuses a tape command). The row is written either way; a MAM read
-- is an observation whether or not anything else was recorded around it.
--
-- NO DRIVE COLUMN
-- ---------------
-- ADR-0013 §1: every record reaches the drive through a foreign key, never a
-- private column. A journal row reaches it through `contact_id` ->
-- `cartridge_contacts.drive_id`. `device_sg` / `device_tape` are the paths
-- the read was taken through -- provenance, like `cartridge_contacts.device`,
-- not drive identity. The medium's OWN memory of which drives loaded it (the
-- ring above) is inside `raw`, where it belongs: it is the medium's lagging
-- account, not a live read of the drive in front of us.
--
-- `serial_as_read` IS THE CHIP'S WORD ONLY
-- ---------------------------------------
-- The `Medium serial number` this read reported, trimmed as `parse_mam` trims
-- it; NULL when the read carried none (mhvtl's sample in `tape::mam` has
-- none) or failed. Never `cartridges.operator_serial` (ADR-0012 amendment,
-- issue #197): that is a human's claim, and this table records what the
-- hardware said.
--
-- `trigger` IS FREE TEXT; `hook` NAMES THE CALL SITE
-- -------------------------------------------------
-- ADR-0013 §4, for the reason 020 gives: a closed CHECK would make every new
-- tape-touching command a schema change. `trigger` is the command verbatim,
-- the same vocabulary as `cartridge_contacts.operation` -- kept on this row
-- even though the contact carries it, because a row whose `contact_id` is
-- NULL still has to say what it was taken for. `hook` is which of the five
-- MAM-reading call sites took it (`volume_init`, `volume_write`,
-- `volume_resume`, `check_read_contact`, `loaded_medium_serial`), so the two
-- reads of one read-path contact stay distinguishable.
--
-- `parsed_json` STORES THE NUMBER THE HARDWARE GAVE, AND ITS LABEL
-- ---------------------------------------------------------------
-- ADR-0013, "capture stores the number the hardware gave, and its label";
-- issue #182. Each recognised attribute is `{"label", "value", "unit"}` with
-- `value` the integer as printed and `unit` read out of the label's own
-- `[..]` suffix -- NOT `MamInfo`'s converted `*_bytes`, because whether
-- `[MiB]` is honest is exactly what #182 has not settled.
--
-- `raw` is stdout byte for byte, untrimmed: TEXT when it is valid UTF-8
-- (every recording to date is ASCII), a BLOB of the exact bytes otherwise, so
-- no lossy conversion ever stands in for the observation. NULL when the tool
-- never ran; possibly empty when it ran and failed. `ok = 0` rows carry the
-- spawn error or exit status and stderr in `error`.
--
-- `tool_version` is `sg_read_attr -V`, cached once per process; NULL when it
-- could not be read, which never fails the capture. `tapectl_version` is the
-- build that wrote the row (ADR-0013 §7): a parser fix in a later build must
-- be distinguishable from a hardware change.
--
-- APPEND-ONLY, NEVER PRUNED, NEVER BACKFILLED
-- ------------------------------------------
-- No code updates or deletes a row. There is no prune, TTL or cap: retention
-- is a separate future issue, and until one decides it, nothing is thrown
-- away. Registering or binding a cartridge later changes NO journal row --
-- a cartridge's rows are found by query (`serial_as_read`, or `contact_id`
-- -> `cartridge_contacts.cartridge_id`), never by rewriting what was
-- observed.
--
-- THIS MIGRATION TOUCHES NO OTHER TABLE
-- -------------------------------------
-- Plain CREATE TABLE, so no `.foreign_key_check()`; nothing references
-- `mam_journal`. It changes no on-tape byte: the operator envelope's
-- `catalog.db` (`db::ontape_catalog`) has its own hand-written schema.
CREATE TABLE mam_journal (
    id               INTEGER PRIMARY KEY,
    -- When the READ happened (the capture's clock), not when the row was
    -- inserted; the default serves only hand inserts.
    captured_at      TEXT NOT NULL DEFAULT (datetime('now')),
    -- NULL: no contact could be named (see above).
    contact_id       INTEGER REFERENCES cartridge_contacts(id),
    device_sg        TEXT NOT NULL,
    device_tape      TEXT,
    -- The command, verbatim. Free TEXT, no CHECK (ADR-0013 §4).
    trigger          TEXT NOT NULL,
    -- The call site that took the read.
    hook             TEXT NOT NULL,
    -- The chip's `Medium serial number`, never an operator claim.
    serial_as_read   TEXT,
    ok               INTEGER NOT NULL CHECK (ok IN (0, 1)),
    error            TEXT,
    -- JSON array, program first: ["sg_read_attr", "/dev/sg1"].
    tool_argv        TEXT NOT NULL,
    tool_version     TEXT,
    raw              TEXT,
    parsed_json      TEXT,
    tapectl_version  TEXT NOT NULL
);

-- "Every MAM read taken during this contact" -- the join from the spine.
CREATE INDEX idx_mam_journal_contact ON mam_journal(contact_id);
-- "Every MAM read this chip ever answered, in order" -- the route that works
-- for a cartridge no contact could identify at the time.
CREATE INDEX idx_mam_journal_serial ON mam_journal(serial_as_read, captured_at);
