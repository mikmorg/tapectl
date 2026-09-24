-- 024: `restores` -- every restore, its outcome and dar's report verbatim
-- (ADR-0013 §§2, 4, 5, 7; ADR-0012 amendment of 2026-09-24 item 2;
-- issue #306).
--
-- WHY
-- ---
-- Until this migration a restore left no record of itself at all. The
-- contact row (020) said a cartridge was in a drive for `restore unit`, the
-- health row (021, #320) said what the drive's counters read afterwards --
-- and nothing said what the RESTORE did: which unit and version came back,
-- to where, how many bytes, whether it finished, and what dar made of the
-- archive. dar's own report -- the only account of per-file failures,
-- skipped entries, EA/ACL handling, the inode summary -- was read for one
-- marker and thrown away on success, and kept as a five-line excerpt on
-- failure. A restore is the one operation that proves the archive works,
-- and it was the one operation that contributed nothing to the record.
-- ADR-0013's standard is "capture everything verbatim now, parse it later";
-- this table is that capture for the content side.
--
-- THE CONTACT ROW IS THE SPINE
-- ----------------------------
-- ADR-0013 §2, §5: `contact_id` points at the contact the restore was made
-- under, never the reverse. It is NULLABLE for the reason 022 and 023 give
-- -- the contact's own INSERT can fail, and bookkeeping never refuses a
-- tape command -- so a restore row is written whether or not its contact
-- got an id. A row is written ONLY once the contact has opened (the read
-- paths' rule): a refusal BEFORE the drive is touched -- unit not found, a
-- `--version` the volume does not carry, the pre-store medium check, the
-- device failing to open -- is not a restore and leaves no row. Everything
-- after that moment does, on success AND on failure: a restore that failed
-- partway is exactly the one an operator most needs to find later.
--
-- ONE ROW PER RESTORE, `kind` FREE TEXT
-- -------------------------------------
-- `kind` is `unit`, `file` or `raw-volume` -- the `restore` subcommand,
-- free TEXT with no CHECK (ADR-0013 §4), pinned by a test on the values
-- code writes. `restore file` reaches the drive through `restore unit`'s
-- one contact (its `cartridge_contacts.operation` is `restore unit`, by
-- design) and writes ONE row, of kind `file`; the placing of the one
-- requested file happens INSIDE the recorded span, so a "file not found in
-- restored unit" is a `failed` row, not an `ok` row beside a non-zero exit.
-- `file_path` is the path the operator asked for and is NULL for the other
-- kinds. `destination` is the directory the operator named, never a scratch
-- or temp directory.
--
-- THE FOREIGN KEYS ARE NULLABLE, AND THE NAMES ARE KEPT BESIDE THEM
-- -----------------------------------------------------------------
-- `volume_id` -- `restore raw-volume` names no volume (it runs against
--   whatever tape is loaded, ADR-0005), and `restore unit` can name a label
--   the catalog has no row for. `volume_label` is what the operator asked
--   for, or for raw-volume what the tape's own File 0 claimed, so the row
--   still says which tape even when there is no row to point at.
-- `unit_id` -- NULL for raw-volume, and for a unit the catalog no longer
--   has. `unit_name` and `version` are kept as TEXT/INTEGER for the same
--   reason. No code deletes a unit or a volume today, so the references
--   cost nothing and make the join honest when a row exists.
--
-- `outcome` IS `cartridge_contacts.outcome`'s VOCABULARY
-- -----------------------------------------------------
-- `ok` or `failed` (`tape::contact::OUTCOME_OK` / `OUTCOME_FAILED`), never
-- a third spelling. `error` is the command's error string when `failed`,
-- NULL when `ok`. A raw-volume dump whose checksums did not all verify is
-- `failed` with the mismatch count in `error`, as its contact is.
--
-- `bytes_restored`, `files_restored`, `slices_read` -- MEASURED, PER KIND
-- ---------------------------------------------------------------------
-- All nullable: NULL means "not known", which a failure before the figure
-- existed is. Never 0 for "did not look" (migration 009's rule).
--   unit, file  -- `slices_read` is the number of slices decrypted off the
--                  tape; `bytes_restored` their plaintext byte total, as
--                  measured through the hashing writer -- the dar archive's
--                  size, which is what came off the tape, NOT the extracted
--                  tree's size (that is dar's business and is in its report).
--                  `files_restored` is dar's own "N inode(s) restored" count
--                  for `unit` (parsed from `dar_stdout`, NULL when the
--                  summary is not there); for `file` it is 1, the one entry
--                  placed at `destination`.
--   raw-volume  -- `bytes_restored` is the bytes written to disk,
--                  `files_restored` the files dumped; `slices_read` is NULL
--                  (a raw dump does not decrypt).
--
-- dar'S REPORT IS VERBATIM; NULL MEANS dar NEVER RAN
-- --------------------------------------------------
-- `dar_stdout` and `dar_stderr` are dar's streams byte for byte: TEXT when
-- valid UTF-8, a BLOB of the exact bytes otherwise (022's rule for `raw`) --
-- dar prints file names, and a non-UTF-8 name lossily rewritten is not the
-- report. Both are NULL when dar was never invoked (the restore failed
-- before the extract, or the kind is raw-volume) and possibly EMPTY when it
-- ran and said nothing; the two are different facts. `dar_argv` is the
-- command as a JSON array, program first; `dar_exit_code` its exit status
-- (NULL when killed by a signal or never run). A dar that exited 0 but
-- declined to overwrite a file (issue #51) is a `failed` row with a clean
-- exit code and the "not restored (user choice)" lines in `dar_stdout` --
-- which is precisely why the report is kept. `dar_version` is `dar
-- --version` at the time, NULL when dar never ran or it could not be read.
--
-- `tapectl_version` is the build that wrote the row (ADR-0013 §7).
--
-- APPEND-ONLY, NEVER PRUNED, NEVER BACKFILLED
-- ------------------------------------------
-- No code updates or deletes a row. There is no prune, TTL or cap. Restores
-- made before this migration are unrecorded forever -- the CTO's own stated
-- standard, and no row is fabricated for them.
--
-- WHAT A READER QUERIES
-- ---------------------
-- "Every restore of this unit, from which tape, ending how":
--   SELECT r.started_at, r.kind, r.volume_label, r.version, r.outcome,
--          r.bytes_restored, r.files_restored, c.drive_id, c.cartridge_id
--     FROM restores r LEFT JOIN cartridge_contacts c ON c.id = r.contact_id
--    WHERE r.unit_name = ?1 ORDER BY r.started_at;
-- "What did dar say the last time this came back": `SELECT dar_stdout,
-- dar_stderr FROM restores WHERE id = ?1`. `db export` carries the table
-- (it enumerates `sqlite_master`); `db stats` counts it the same way.
--
-- THIS MIGRATION TOUCHES NO OTHER TABLE
-- -------------------------------------
-- Plain CREATE TABLE, so no `.foreign_key_check()`; nothing references
-- `restores`. It changes no on-tape byte: the operator envelope's
-- `catalog.db` (`db::ontape_catalog`) has its own hand-written schema.
CREATE TABLE restores (
    id               INTEGER PRIMARY KEY,
    -- NULL: the contact's own INSERT failed (see above).
    contact_id       INTEGER REFERENCES cartridge_contacts(id),
    volume_id        INTEGER REFERENCES volumes(id),
    -- The label asked for (unit/file), or the tape's own claim (raw-volume);
    -- NULL when a raw dump failed before File 0 could say.
    volume_label     TEXT,
    unit_id          INTEGER REFERENCES units(id),
    unit_name        TEXT,
    version          INTEGER,
    -- 'unit', 'file' or 'raw-volume'. Free TEXT, no CHECK (ADR-0013 §4).
    kind             TEXT NOT NULL,
    -- The one path asked for; NULL unless kind = 'file'.
    file_path        TEXT,
    destination      TEXT NOT NULL,
    -- The restore's own clock, in datetime('now')'s spelling.
    started_at       TEXT NOT NULL,
    finished_at      TEXT NOT NULL,
    -- 'ok' or 'failed' -- the contact row's vocabulary.
    outcome          TEXT NOT NULL,
    error            TEXT,
    slices_read      INTEGER,
    bytes_restored   INTEGER,
    files_restored   INTEGER,
    -- JSON array, program first: ["dar", "-x", "/dest/.tapectl-restore-tmp/restore", ...].
    dar_argv         TEXT,
    dar_exit_code    INTEGER,
    dar_stdout       TEXT,
    dar_stderr       TEXT,
    dar_version      TEXT,
    tapectl_version  TEXT NOT NULL
);

-- "The restore made under this contact" -- the join from the spine.
CREATE INDEX idx_restores_contact ON restores(contact_id);
-- "Every restore of this unit, in order" -- the operator's question.
CREATE INDEX idx_restores_unit ON restores(unit_name, started_at);
-- "Every restore off this volume" -- the evidence a volume still reads.
CREATE INDEX idx_restores_volume ON restores(volume_id);
