-- 025: `st_stats_journal` -- the kernel st driver's per-device I/O counters,
-- verbatim, at each contact's open and close (ADR-0013 §§2, 5, 7; ADR-0012
-- 2026-09-24 amendment item 2; issue #301).
--
-- (Numbered 025 because 024 is assigned to concurrent work. Migrations are
-- POSITIONAL in `db::migrations()`: 024 must be registered ahead of this one
-- before either reaches a persistent database.)
--
-- WHY
-- ---
-- The st driver keeps cumulative counters for every tape device in sysfs,
-- `/sys/class/scsi_tape/<node>/stats/`: on the reference kernel (6.8)
-- `read_byte_cnt read_cnt read_ns write_byte_cnt write_cnt write_ns
-- other_cnt io_ns resid_cnt in_flight`. Bytes moved, commands issued, and
-- NANOSECOND time spent, maintained by the kernel for free -- no SCSI
-- command, no device open -- and until this migration nothing read them.
-- They are the only drive evidence available when the drive will not answer
-- SCSI at all, and `write_ns`/`write_byte_cnt` answer the throughput
-- question that `datetime('now')`'s one-second quantisation cannot.
--
-- TWO READINGS PER CONTACT, NEVER DIFFERENCED HERE
-- -----------------------------------------------
-- The counters are cumulative, so what a contact did is the DIFFERENCE
-- across it -- which one reading cannot give. So a reading is taken at the
-- contact's OPEN (`point = 'open'`, right after its `cartridge_contacts`
-- row is inserted) and at its CLOSE (`point = 'close'`, right before
-- `closed_at` is set). A contact that never closed (`closed_at IS NULL`)
-- has no close reading, for the same reason it has no outcome.
--
-- No delta is stored. ADR-0013 §3: "a delta is the difference between two
-- readings on the same contact, which is a query, not stored state" -- and
-- here, additionally, the counters' reset scope (module reload? rescan?
-- reboot?) is unmeasured, so a stored delta would be an interpretation the
-- evidence does not yet support. The query:
--   CAST(json_extract(c.stats_json, '$.write_byte_cnt') AS INTEGER)
--     - CAST(json_extract(o.stats_json, '$.write_byte_cnt') AS INTEGER)
-- over the open (o) and close (c) rows of one contact_id.
--
-- The readings bracket the CONTACT, not the file descriptor. Where a command
-- opens its store before its contact (verify) or closes the contact before
-- dropping the store, the st driver's own open-time commands or its
-- close-time filemark/rewind fall outside the pair; `in_flight` in each
-- reading says whether I/O was outstanding at that instant.
--
-- `stats_json` IS THE FILES, VERBATIM
-- ----------------------------------
-- A JSON object, file name -> the file's text exactly as read, trailing
-- newline included ({"io_ns": "2009136201991\n", ...}). Every regular file
-- in `stats/` is read -- the set is whatever THIS kernel exposes, which is
-- why it is not a set of columns: a kernel that adds a counter must not need
-- a migration to have it recorded. One row per reading rather than one row
-- per file, because a reading is one instant and the pair must be joined as
-- a unit. JSON-in-a-column is the journals' existing idiom
-- (`mam_journal.parsed_json`, `tool_argv`). A file that could not be read,
-- or was not UTF-8 text, is named in `errors_json` with the error (and the
-- bytes, hex) instead of being silently dropped; NULL when every file read.
--
-- ABSENCE IS NO ROW
-- -----------------
-- A tape node with no `stats/` directory -- a non-st device, a MemStore, a
-- kernel without st statistics -- yields no reading and no row, and never
-- fails the command. `sysfs_dir` records the directory that WAS resolved
-- and read: the node comes from the device path the command was given,
-- canonicalised as `drive_identity` resolves it (on the reference kernel
-- every mode variant -- nst0, nst0a, st0l, ... -- publishes the same
-- per-drive counters).
--
-- NO SCSI, NO IOCTL
-- -----------------
-- Sysfs reads only. The contact guard never holds the store's descriptor and
-- must not open the node itself (the st driver refuses a second concurrent
-- open, and "the guard never reads the medium"), so MTIOCGET's `mtget` is
-- not captured here.
--
-- THE JOURNAL POINTS AT THE CONTACT, NEVER THE REVERSE
-- ---------------------------------------------------
-- ADR-0013 §5, as for 022/023. `contact_id` is NULLABLE: when the contact's
-- own INSERT failed (bookkeeping is best-effort and never refuses a tape
-- command) the reading is still written -- it happened. `trigger` is the
-- command verbatim, the `cartridge_contacts.operation` vocabulary (free
-- TEXT, ADR-0013 §4), kept on the row so a NULL-contact reading still says
-- what it was taken for. No drive column (§1): the drive is reached through
-- `contact_id -> cartridge_contacts.drive_id`.
--
-- `point` IS CHECKED
-- ------------------
-- Unlike `trigger`, `point` is not a growing vocabulary: a contact has an
-- open and a close. It is closed the way `ok IN (0, 1)` is.
--
-- `tapectl_version` is the build that wrote the row (ADR-0013 §7).
--
-- APPEND-ONLY, NEVER PRUNED, NEVER BACKFILLED
-- ------------------------------------------
-- No code updates or deletes a row. Two rows of ~300 bytes per contact.
--
-- THIS MIGRATION TOUCHES NO OTHER TABLE
-- -------------------------------------
-- Plain CREATE TABLE, so no `.foreign_key_check()`; nothing references
-- `st_stats_journal`. It changes no on-tape byte.
CREATE TABLE st_stats_journal (
    id               INTEGER PRIMARY KEY,
    -- When the READING was taken (millisecond precision, UTC), not when the
    -- row was inserted; the default serves only hand inserts.
    captured_at      TEXT NOT NULL DEFAULT (strftime('%Y-%m-%d %H:%M:%f', 'now')),
    -- NULL: no contact could be named (see above).
    contact_id       INTEGER REFERENCES cartridge_contacts(id),
    point            TEXT NOT NULL CHECK (point IN ('open', 'close')),
    -- The command, verbatim. Free TEXT, no CHECK (ADR-0013 §4).
    trigger          TEXT NOT NULL,
    -- The device path the command was given, spelled as given.
    device           TEXT NOT NULL,
    -- The `stats/` directory actually read.
    sysfs_dir        TEXT NOT NULL,
    -- {"<file>": "<text verbatim>", ...}
    stats_json       TEXT NOT NULL,
    -- {"<file>": "<why it is not in stats_json>", ...}; NULL when none.
    errors_json      TEXT,
    tapectl_version  TEXT NOT NULL
);

-- "Both readings of this contact" -- the join from the spine.
CREATE INDEX idx_st_stats_journal_contact ON st_stats_journal(contact_id);
