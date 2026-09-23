-- 023: `log_page_journal` -- every SCSI log page read, verbatim (ADR-0013
-- §§1, 5, 7 and "Two hazards"; issue #298).
--
-- WHY
-- ---
-- Until this migration `tape::health::collect` read a hardcoded three log
-- pages (0x02 write errors, 0x03 read errors, 0x2E TapeAlert) and never asked
-- page 0x00 what the drive could report. Everything else the drive would have
-- answered -- temperature, volume statistics, device status, tape usage and
-- capacity, compression, performance -- was never requested and left no
-- trace, and there was no record of WHICH pages a given drive supported at a
-- given time, so even "could we have captured this?" is unanswerable
-- retroactively. ADR-0013's standard is "capture everything verbatim now,
-- parse it later"; this table is that capture for log pages.
--
-- ONE SWEEP PER CONTACT, EACH PAGE READ AT MOST ONCE
-- -------------------------------------------------
-- ADR-0013, "Two hazards": TapeAlert (0x2E) is described by SSC-3 as cleared
-- when read, so a second read inside one contact could return zeros and the
-- first read's evidence would be gone. `tape::log_pages::sweep` therefore
-- reads page 0x00 once, then every page it lists once, each with
-- `sg_logs --raw` (one LOG SENSE, the response bytes exactly); the text
-- decode is done OFFLINE from the stored bytes (`sg_logs --in=- --raw
-- --pdt=1`, no device), and health collection's counters are parsed from
-- that decode. The sweep REPLACES the old three-page read; it does not run
-- beside it. Every consumer reads this journal, never the drive.
--
-- One row per page read per sweep, INCLUDING the page-0x00 row. A page 0x00
-- lists itself; it is not read twice. If page 0x00 itself cannot be read (or
-- its bytes do not parse as a page list), the sweep records that row and
-- falls back to the three pages health collection has always read -- still
-- once each -- so "was this read because the drive listed it?" is answered
-- by query: the contact's page-0x00 row says whether a list existed.
--
-- THE JOURNAL POINTS AT THE CONTACT, NEVER THE REVERSE
-- ---------------------------------------------------
-- ADR-0013 §5, as for `mam_journal` (022): one contact has many rows, so the
-- foreign key lives on the many side. `contact_id` is NULLABLE: a sweep can
-- run when no contact could be named (the contact's own INSERT failed --
-- bookkeeping is best-effort and never refuses a tape command). The rows are
-- written either way; a read that happened is an observation.
--
-- NO DRIVE COLUMN
-- ---------------
-- ADR-0013 §1. These pages are mostly DRIVE-resident counters, which is
-- exactly why the drive matters -- and exactly why it is reached through
-- `contact_id -> cartridge_contacts.drive_id`, never a private column.
-- `device_sg` / `device_tape` are the paths the read was taken through --
-- provenance, like `cartridge_contacts.device`, not drive identity.
--
-- `trigger` IS FREE TEXT
-- ----------------------
-- ADR-0013 §4, for the reason 020 gives. The command verbatim, the same
-- vocabulary as `cartridge_contacts.operation` and `mam_journal.trigger`
-- (`volume write`, `volume resume`, `volume verify`) -- NOT
-- `health_logs.operation`'s reading kind; the two lists may not stand in for
-- each other. Kept on the row although the contact carries it, because a row
-- whose `contact_id` is NULL still has to say what it was taken for.
--
-- `page_code` / `subpage_code`
-- ----------------------------
-- The LOG SENSE page and subpage requested. Subpages are not enumerated
-- (page 0x00 subpage 0x00 lists pages only, SPF clear), so every read asks
-- for subpage 0 and `subpage_code` says so rather than leaving it NULL.
--
-- `raw` IS THE RESPONSE BYTES; `decoded` IS WHAT THIS BUILD'S DECODER SAID
-- -----------------------------------------------------------------------
-- `raw` is `sg_logs --raw`'s stdout byte for byte, always a BLOB: it is
-- binary. NULL when the tool never ran; possibly empty when it ran and
-- failed. `decoded` is the offline decode at capture time -- a convenience
-- and a record of what THIS build's sg_logs made of the bytes, never a
-- substitute for them (sg_logs decodes page 0x37 as "Unable to decode": the
-- page is real, the decoder has nothing for it, and the bytes are kept).
-- NULL when the read failed or the decode could not run. A parser written
-- later reads `raw`, which is why historical rows stay re-parseable.
--
-- `tool_version` is `sg_logs -V`, cached once per process; NULL when it could
-- not be read, which never fails the capture. `tapectl_version` is the build
-- that wrote the row (ADR-0013 §7).
--
-- APPEND-ONLY, NEVER PRUNED, NEVER BACKFILLED
-- ------------------------------------------
-- No code updates or deletes a row. There is no prune, TTL or cap: retention
-- is a separate future issue, and until one decides it, nothing is thrown
-- away. At 13 pages per contact on the reference mhvtl drive (about 2 KiB of
-- raw bytes plus the decode), that is a deliberate cost.
--
-- THIS MIGRATION TOUCHES NO OTHER TABLE
-- -------------------------------------
-- Plain CREATE TABLE, so no `.foreign_key_check()`; nothing references
-- `log_page_journal`. `health_logs` keeps its parsed columns and `raw_log`
-- as the fast path. It changes no on-tape byte.
CREATE TABLE log_page_journal (
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
    page_code        INTEGER NOT NULL CHECK (page_code BETWEEN 0 AND 255),
    subpage_code     INTEGER NOT NULL DEFAULT 0 CHECK (subpage_code BETWEEN 0 AND 255),
    ok               INTEGER NOT NULL CHECK (ok IN (0, 1)),
    error            TEXT,
    -- JSON array, program first: ["sg_logs", "--page=0x2e", "--raw", "/dev/sg1"].
    tool_argv        TEXT NOT NULL,
    tool_version     TEXT,
    raw              BLOB,
    decoded          TEXT,
    tapectl_version  TEXT NOT NULL
);

-- "Every page read during this contact" -- the join from the spine.
CREATE INDEX idx_log_page_journal_contact ON log_page_journal(contact_id);
-- "Every reading of this page, in order" -- the route a retroactive parser
-- takes (e.g. every 0x0D temperature ever recorded).
CREATE INDEX idx_log_page_journal_page ON log_page_journal(page_code, captured_at);
