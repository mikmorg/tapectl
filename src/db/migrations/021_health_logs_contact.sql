-- 021: `health_logs` becomes a child of the contact row -- the ONE rebuild
-- this table gets (ADR-0013 §3, issue #296, pass 2).
--
-- WHY THIS IS THE ONLY REBUILD, AND WHY IT CARRIES EVERYTHING AT ONCE
-- ------------------------------------------------------------------
-- Four issues in the tape-forensics suite independently proposed rebuilding
-- this table, each saying "coordinate with siblings", and nobody owned the
-- coordination. Migrations are forward-only. Four uncoordinated rebuilds of
-- the table holding the schema's largest blobs is the most likely way that
-- suite loses data -- not by failing to capture it, but by capturing it and
-- then dropping a column in a migration whose author did not know what a
-- sibling had just added. ADR-0013 is that coordination and this migration is
-- its cash value: every sibling's requirement lands here, together, once.
--
-- Create/copy/drop/rename, per SQLite's documented 12-step "Making Other
-- Kinds Of Table Schema Changes" procedure -- the same shape 003, 012 and 017
-- used, and for the same reason: SQLite cannot ALTER a CHECK constraint or a
-- NOT NULL away in place. `.foreign_key_check()` is registered on this
-- migration in `db::mod` for the reason 017's header gives: this table holds
-- OUTBOUND references (`volume_id`, `session_id`, and now `contact_id`) and a
-- rebuild that renumbered rows or lost an edge would leave them dangling in
-- silence. Nothing holds an INBOUND reference to `health_logs` -- it is a
-- leaf -- so no other table can be orphaned BY this drop; the check guards
-- the edges this table itself owns.
--
-- `id` is copied VERBATIM (`SELECT h.id, ...`), not left to SQLite to
-- reassign. Nothing references these rows today, but a renumbered append-only
-- record is a falsified one: row 4 becoming row 1 makes an old reading look
-- like the first ever taken.
--
-- `contact_id`: THE POINT OF THE REBUILD
-- --------------------------------------
-- ADR-0013 §2. Every hardware observation happens DURING a contact between a
-- drive and a cartridge, and a reading that cannot name its contact cannot be
-- differenced against its own pair. Before migration 020 there was no key
-- joining an opening reading to its closing one, and no route at all from a
-- health row to the DRIVE that produced it -- which makes the central question
-- in tape diagnostics, "is it the drive or the tape?", unanswerable from this
-- table. sg_logs pages 0x02/0x03/0x2E are drive-resident counters read through
-- whichever cartridge happened to be loaded; attributing them to `volume_id`
-- alone asserts the one attribution there is least evidence for.
--
-- Nullable, and NOT backfilled. Pre-021 rows genuinely have no contact --
-- `cartridge_contacts` did not exist when they were written -- and there is no
-- honest way to invent one. Correlating by timestamp would manufacture a link
-- that reads exactly like an observation, which is the failure mode this whole
-- suite exists to end.
--
-- `volume_id` BECOMES NULLABLE
-- ----------------------------
-- A drive-only reading has no volume (ADR-0013 §§2-3): the counters above are
-- the machine's, and a contact can legitimately happen with a cartridge that
-- has no volume at all -- a blank, or a foreign tape under `volume identify`.
-- `NOT NULL` forbade recording that; it forced every reading to name a volume
-- or not be recorded.
--
-- Being honest about today: NO production path writes a NULL `volume_id` yet.
-- All three writers (`volume write`, `volume resume`, `volume verify`) have a
-- volume in hand. The nullability is for the reading the ADR names and the
-- schema must stop forbidding -- not a claim that one already exists. It keeps
-- its `REFERENCES volumes(id)`, so a non-NULL value is still a real volume.
--
-- AND IT BREAKS A READER, WHICH THIS CHANGE OWNS
-- ----------------------------------------------
-- `report health` was `FROM health_logs h JOIN volumes v ON v.id = h.volume_id`
-- -- an INNER JOIN. The moment a drive-only reading exists, that report
-- silently drops it: captured and invisible, which is the failure this suite
-- exists to prevent, reproduced by the suite's own fix. It is also #293's
-- defect (an output that cannot distinguish "looked and found nothing" from
-- "never looked") in a second report, with the same remedy: drive the query
-- from the table whose rows you must not lose. Fixed alongside this
-- migration (issue #296 pass 2) -- see `cli::report::health_rows`.
--
-- `tapectl_version`: EVERY ROW RECORDS THE OBSERVER
-- -------------------------------------------------
-- ADR-0013 §7. "Parse it later" requires knowing which build wrote the parsed
-- columns beside the raw text: a parser bug fixed in a later version is
-- indistinguishable from a hardware change unless the rows say which parser
-- produced them. `health::record` writes `CARGO_PKG_VERSION` unconditionally
-- -- there is exactly one build doing the writing, so it is not a parameter
-- anything could get wrong. The precedent is `src/volume/write.rs`, where the
-- same string already reaches the tape.
--
-- Nullable, and NOT backfilled: a pre-021 row does not know which build wrote
-- it, and stamping today's version on it would be a lie in the one column
-- whose entire job is to say who was observing.
--
-- THE `operation` CHECK IS DROPPED -- FREE TEXT FROM HERE
-- ------------------------------------------------------
-- ADR-0013 §4. The old constraint was
-- `CHECK(operation IN ('write','read','verify','clean'))` (`001_initial.sql`).
-- It is the argument against itself: `read` and `clean` have NEVER had a
-- writer, so it was a closed vocabulary already wrong in half its values, and
-- a vocabulary wrong in half its values protects nothing. Meanwhile it made
-- the one value the code needed impossible: `volume resume` has always
-- recorded `'write'`, a lie in the record, because writing `'resume'` would
-- fail the CHECK -- and since health collection is best-effort and only warns,
-- it would have silently DROPPED the health row on every resume. That literal
-- flips in the same commit as this migration; it is safe at this moment and
-- was not safe before it.
--
-- Migrations are forward-only and this vocabulary grows with every new
-- tape-touching command, so a closed CHECK turns each new command into a
-- schema change. The replacement is a test asserting the observed set matches
-- what code actually writes (`tape::health`'s
-- `the_reading_vocabulary_is_what_code_actually_writes`, mirroring
-- `tape::contact`'s for `cartridge_contacts.operation` -- two DIFFERENT
-- vocabularies, and neither list may stand in for the other). Same typo
-- protection, no migration cost, and -- unlike a CHECK -- it can tell that a
-- permitted value has no writer, which is how `read` and `clean` should have
-- been caught years ago.
--
-- `operation` stays NOT NULL. Free text is not optional text: a reading that
-- cannot say what kind of reading it is has lost the thing that makes it
-- comparable to another.
--
-- TWO THINGS MUST SURVIVE THIS, AND LOSING EITHER DEFEATS THE SUITE
-- -----------------------------------------------------------------
-- `raw_log` -- copied unchanged. It is the ONE place this project already
-- honours the CTO's capture-everything standard (`001_initial.sql`), and every
-- retroactive parser the non-gating half of this suite will add reads it.
-- Dropping it here would be the capture suite destroying the only capture it
-- already had.
--
-- `tape_alerts` -- copied unchanged, still nullable, still WITH NO DEFAULT,
-- and NO ROW IS BACKFILLED TO 0. Read 009's header: it made this column
-- nullable deliberately, because NULL says "not recorded" and 0 says
-- "recorded, and there were none". A rebuild that defaulted or backfilled 0
-- would assert "the drive reported no alerts" about collections that never
-- looked -- a confident wrong answer about the most directly actionable signal
-- a tape system produces. `report health` renders NULL as "-" and must keep
-- being able to.
--
-- `session_id` KEEPS EXACTLY THE MEANING ITS FOREIGN KEY DECLARES
-- ---------------------------------------------------------------
-- ADR-0013 §3: the `verification_sessions` row, and nothing else. Four drafts
-- in this suite proposed four different meanings for a column that then had
-- zero writers; a column that means four things means none. Issue #295 gave it
-- its one writer (`volume_verify`, carrying `VerifyReport::session_id`), and
-- this rebuild does not touch it beyond restating the edge.
--
-- WHAT IS DELIBERATELY NOT A COLUMN
-- ---------------------------------
-- No scope column and no delta column. ADR-0013 §3: with `contact_id`, a delta
-- is the difference between two readings on one contact, which is a QUERY, not
-- stored state -- and storing a derived figure beside the two facts it is
-- derived from is how the two disagree later.
--
-- No drive column. ADR-0013 §1: every record takes a foreign key to `drives`
-- and grows none of its own. The drive is reached through
-- `contact_id -> cartridge_contacts.drive_id`, which is also how the same
-- reading reaches the CARTRIDGE -- both routes to "is it the drive or the
-- tape?" from one join, which is the point.
--
-- No `mam_journal_id`. ADR-0013 §5: the journal points at the contact, never
-- the reverse. `mam_journal.contact_id` arrives with migration 022 (#297).
--
-- INDEXES
-- -------
-- `idx_health_volume` is restated verbatim -- a rebuild silently drops
-- whatever its new DDL forgets, and 001 has carried this one since the
-- beginning.
--
-- `idx_health_contact` is new, and it is the index the ADR's own mechanism
-- needs: §3 rules that a delta is "the difference between two readings on the
-- same contact", which is a lookup keyed on `contact_id`. Without it that
-- lookup scans a table whose rows carry the schema's largest blobs
-- (`raw_log`), page by page, to find at most two. An index is not a column and
-- costs nothing that the "no derived state" ruling objects to.
--
-- NOTHING IS BACKFILLED, ANYWHERE
-- -------------------------------
-- Unknown must read as unknown. Existing rows cannot be attributed to a
-- contact, a build or an alert count, and guessing is strictly worse than
-- NULL: a guess is indistinguishable from an observation, which is the exact
-- confusion the forensics record exists to end.
--
-- This migration changes no on-tape byte: the operator envelope's on-tape
-- catalog (`db::ontape_catalog`) carries its own independent, hand-written
-- schema with no health, drive or contact tables at all.

CREATE TABLE health_logs_new (
    id                  INTEGER PRIMARY KEY,
    -- NULLABLE from 021: a drive-only reading has no volume. Still a real
    -- volume when it is not NULL.
    volume_id           INTEGER REFERENCES volumes(id),
    -- The contact this reading was taken during (ADR-0013 §2). NULL on every
    -- pre-021 row, which genuinely had no contact to name.
    contact_id          INTEGER REFERENCES cartridge_contacts(id),
    -- The `verification_sessions` row, and nothing else (ADR-0013 §3).
    session_id          INTEGER REFERENCES verification_sessions(id),
    logged_at           TEXT NOT NULL DEFAULT (datetime('now')),
    -- Free TEXT from 021 (ADR-0013 §4), pinned by a test rather than a CHECK.
    -- What KIND of reading this is -- a DIFFERENT vocabulary from
    -- `cartridge_contacts.operation`, which is the command verbatim.
    operation           TEXT NOT NULL,
    total_bytes         INTEGER,
    total_uncorrected   INTEGER,
    total_corrected     INTEGER,
    total_retries       INTEGER,
    total_rewritten     INTEGER,
    -- The capture-everything standard's one existing honouring. Unchanged.
    raw_log             TEXT,
    -- Migration 009's NULL-vs-0 distinction, unchanged: nullable, NO default,
    -- nothing backfilled. NULL is "not recorded"; 0 is "recorded, none
    -- raised".
    tape_alerts         INTEGER,
    -- Which build wrote this row (ADR-0013 §7). NULL on every pre-021 row.
    tapectl_version     TEXT
);

INSERT INTO health_logs_new (
    id, volume_id, contact_id, session_id, logged_at, operation, total_bytes,
    total_uncorrected, total_corrected, total_retries, total_rewritten,
    raw_log, tape_alerts, tapectl_version
)
SELECT
    h.id, h.volume_id, NULL, h.session_id, h.logged_at, h.operation,
    h.total_bytes, h.total_uncorrected, h.total_corrected, h.total_retries,
    h.total_rewritten, h.raw_log, h.tape_alerts, NULL
FROM health_logs h;

DROP TABLE health_logs;

ALTER TABLE health_logs_new RENAME TO health_logs;

CREATE INDEX idx_health_volume ON health_logs(volume_id);
CREATE INDEX idx_health_contact ON health_logs(contact_id);
