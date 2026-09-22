-- 020: the contact is the spine of the forensics record (ADR-0013 §2, issue #296).
--
-- Thirteen code paths put a cartridge in a drive and not one of them wrote a
-- row saying it happened. `volume verify` ten times over five years leaves the
-- catalog looking exactly as `volume init` left it on day one.
--
-- WHY `cartridge_volumes` IS NOT THIS TABLE
-- -----------------------------------------
-- It is a BINDING record, and `UNIQUE(volume_id)` (`001_initial.sql:216-224`)
-- is the whole story: one row per volume for the life of that volume.
-- `mounted_at` is the moment the binding was established, not the moment of
-- any later contact; `unmounted_at` is set only when something DISPLACES the
-- volume, not when the drive door opens. Re-asserting the same binding is an
-- explicit no-op (`volume::binding`'s `already_mounted` guard). It answers
-- "which cartridge is this volume on", which is a different question from
-- "when was this cartridge last in a drive, which drive, and how did it end".
--
-- `cartridges.total_load_count` is the near-miss on the other side: it is the
-- CHIP's own counter, correctly SET rather than incremented, but sampled by
-- exactly one caller (`volume init` -> `bind_cartridge`). A tape initialised
-- at load 1 and verified quarterly for five years still reads
-- `total_load_count = 1`. That is worse than absent -- it is a confident wrong
-- answer to the wear question from a column that looks maintained.
-- `chip_load_count` here records the chip's reading AT THIS CONTACT, which is
-- an observation and cannot go stale. (This migration deliberately does NOT
-- change when `cartridges` is refreshed; that is a separate question.)
--
-- WHY THE CONTACT IS THE SPINE, NOT MERELY A LOG
-- ----------------------------------------------
-- ADR-0013 §2. Every hardware observation happens DURING a contact, and a
-- reading that cannot name its contact cannot be differenced against its own
-- pair. Time-correlation across separate tables works right up until two
-- contacts land in the same second, which is not hypothetical on a machine
-- that runs a thirteen-scenario lifecycle suite. Four separate problems
-- dissolve once `contact_id` exists rather than each needing its own
-- mechanism: no key joining an opening reading to its closing one,
-- `health_logs.volume_id NOT NULL` forbidding a drive-only reading,
-- degenerate keying for an unregistered blank, and every draft's private
-- drive column.
--
-- EVERY FOREIGN KEY IS NULLABLE, AND EACH NULL MEANS SOMETHING DIFFERENT
-- ---------------------------------------------------------------------
-- `cartridge_id` -- a contact can happen before the cartridge is identified,
--   which is exactly when a blank is being read, and on the heir/DR path
--   (ADR-0005) no serial is read at all. `identity_reason` says WHICH of
--   those it was, and it is never NULL when `cartridge_id` is: a NULL with no
--   reason is the data loss this suite exists to stop.
-- `volume_id` -- not every contact has one. `volume identify` and
--   `restore raw-volume` run against whatever tape is loaded.
-- `drive_id` -- migration 019's rule, unchanged: a drive that publishes no
--   serial gets no row, and unknown is recorded by absence, never guessed.
--
-- WHAT IS DELIBERATELY NOT HERE
-- -----------------------------
-- No `mam_journal_id`. ADR-0013 §5 rules the journal points at the contact,
-- never the reverse: a single read-path command performs TWO MAM reads
-- (`check_read_contact`, then the pre-store read) inside ONE contact, and one
-- column cannot hold two readings. `mam_journal.contact_id` arrives with
-- migration 022 (issue #297); nothing is added here for it, and a forward
-- reference to a table two migrations away would not resolve anyway.
--
-- No drive column. ADR-0013 §1: every record takes a foreign key to `drives`
-- and grows none of its own. In particular the MAM ring's "vendor/serial at
-- last load" is NOT recorded here -- it is the MEDIUM's lagging memory of a
-- drive, not a live read of the one in front of us, so it belongs in the MAM
-- journal as a cross-check.
--
-- `device` and `backend_name` stay, and are not drive identity: they record
-- HOW this contact was made, which is contact provenance. `backend_name` is a
-- config NAME (issue #151) and is NULL on a DR machine with keys and no
-- `backend add`; `device` is the path as the operator gave it, by-id or
-- `/dev/nstN`, recorded verbatim because the string actually used is
-- evidence, not a key -- this VM's device numbering is not stable across
-- reboots.
--
-- `operation` IS FREE TEXT
-- ------------------------
-- ADR-0013 §4. Migrations are forward-only and this vocabulary grows with
-- every new tape-touching command; a closed CHECK turns each new command into
-- a schema change. `health_logs.operation`'s own CHECK is the argument
-- against itself -- it permits `read` and `clean`, neither of which any code
-- has ever written. A test pinning the observed set gives the same typo
-- protection at no migration cost (`tape::contact`'s
-- `the_operation_vocabulary_is_what_code_actually_writes`).
--
-- Note there are TWO `operation` vocabularies in this schema and they are not
-- the same: `health_logs.operation` is what KIND of reading a row is
-- (`write`/`verify`); this one is the command VERBATIM (`volume init`,
-- `volume verify`, `restore raw-volume`, ...). Neither list may stand in for
-- the other.
--
-- `closed_at IS NULL` MEANS "DID NOT CLOSE", AND NOTHING SWEEPS IT
-- ---------------------------------------------------------------
-- `outcome` stays NULL with it. There is deliberately no recovery sweep in
-- `db::open` (ADR-0013's ruling for this issue): `recover_orphaned_sessions`
-- runs on EVERY `db::open()` including read-only commands, and issue #98 is
-- the scar -- a status-only sweep there marked a LIVE invocation's staging
-- row `failed` out from under it, which is why staging now probes a
-- per-stage-set flock. A contact has no such lock to probe, so a naive sweep
-- would mark the contact of the command that is running right now. Telling
-- "crashed" from "still running" is a real question and belongs with the
-- non-gating trend work, where a mechanism can be designed rather than
-- assumed.
--
-- APPEND-ONLY, AND NOTHING IS BACKFILLED
-- --------------------------------------
-- Like `health_logs` and `verification_sessions`. The existing
-- `cartridge_volumes.mounted_at` rows are real bindings and stay where they
-- are; they are not contacts and copying them in as if they were would
-- fabricate history that reads exactly like an observation.
--
-- THIS MIGRATION TOUCHES NO OTHER TABLE
-- -------------------------------------
-- In particular it adds NO column to `health_logs`. ADR-0013 §3 gives that
-- table exactly ONE rebuild and it is migration 021 -- the second pass of
-- this same issue -- which reaches the drive and the cartridge through
-- `contact_id` on this table rather than through private columns.
-- `tape::contact`'s `migration_020_adds_no_column_to_health_logs` and
-- `db::tests::migration_020_creates_only_cartridge_contacts` pin that.
--
-- Plain CREATE TABLE -- no rebuild of anything, so no `.foreign_key_check()`
-- (unlike migrations 003/012/013/017, which dropped and recreated a table
-- other rows referenced). Nothing references this table yet; migration 022
-- will.
--
-- This table does not appear anywhere on tape: the operator envelope's
-- on-tape catalog (`db::ontape_catalog`) carries its own independent,
-- hand-written schema with no health, drive or contact tables at all, so this
-- migration changes no on-tape byte.
CREATE TABLE cartridge_contacts (
    id                INTEGER PRIMARY KEY,
    -- NULL when the cartridge could not be identified; `identity_reason`
    -- then says which of the five reasons it was.
    cartridge_id      INTEGER REFERENCES cartridges(id),
    -- NULL: not every contact has a volume (`volume identify`,
    -- `restore raw-volume`).
    volume_id         INTEGER REFERENCES volumes(id),
    -- NULL when the drive published no serial (migration 019's rule).
    drive_id          INTEGER REFERENCES drives(id),
    -- The command, verbatim: 'volume init', 'volume verify', ...
    operation         TEXT NOT NULL,
    -- The path as given, by-id or /dev/nstN.
    device            TEXT NOT NULL,
    -- NULL on a DR machine with no `backend add`.
    backend_name      TEXT,
    -- Why `cartridge_id` is NULL; NULL when it is not.
    identity_reason   TEXT,
    -- MAM "Load count" at THIS contact -- an observation, not a running
    -- total, and not a copy of `cartridges.total_load_count`.
    chip_load_count   INTEGER,
    opened_at         TEXT NOT NULL DEFAULT (datetime('now')),
    -- NULL = did not close. Nothing sweeps it; see above.
    closed_at         TEXT,
    -- NULL until closed.
    outcome           TEXT,
    -- Free text: error string, counts, notes.
    detail            TEXT
);

-- "Every contact with this cartridge, in order" -- the wear and correlation
-- question, and the one a 2031 operator holding a tape with two uncorrected
-- read errors actually asks.
CREATE INDEX idx_cartridge_contacts_cartridge ON cartridge_contacts(cartridge_id, opened_at);
-- "Every contact with this volume" -- the join `report` and `audit` will
-- reach for, and the one that makes a volume's verify history findable
-- without scanning.
CREATE INDEX idx_cartridge_contacts_volume ON cartridge_contacts(volume_id);
