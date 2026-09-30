-- 028: `phase_timings` -- how long each phase of a long operation took, and
-- how many bytes it moved (issue #386).
--
-- WHY
-- ---
-- L6-0001's `volume write` (2026-09-30) ran for hours with the drive idle.
-- It was the pre-write hash of 1.2 TiB of staged slices, but nothing
-- recorded that: about five hours of that session are still unexplained.
-- Throughput questions (#304, #326, #364) were answered by inference from
-- `writes.started_at`/`completed_at`, which bracket the whole session and
-- say nothing about where inside it the time went. Each phase -- stage
-- create's validate/dar/catalog/encrypt/finalize, a write's contact
-- open/build/pre-write check/positioning/plan/write/seal/confirm/health
-- sweep, a verify's readback -- now leaves one row, so throughput is
-- measured rather than inferred.
--
-- ONE ROW PER PHASE, GROUPED BY SESSION
-- -------------------------------------
-- `session` is the session id, which is also the name of the session log
-- (`<home>/logs/<session>.log`) holding the same phases with their waits,
-- so a row leads to the log that explains it. `operation` is the command
-- (`stage create`, `volume write`, `volume resume`, `volume verify`, ...),
-- free TEXT with no CHECK (ADR-0013 §4). `seq` orders the phases within a
-- session. `phase` is free TEXT for the same reason.
--
-- `started_at` is UTC in `datetime('now')`'s spelling, so it compares as
-- text with every other timestamp in the catalog. `duration_ms` is measured
-- on the monotonic clock, not derived from two wall-clock readings.
-- `bytes` is NULL for a phase that does not count bytes (never 0 for "did
-- not look", migration 009's rule). `outcome` is `ok` or `failed`
-- (`cartridge_contacts.outcome`'s vocabulary): `failed` means an error
-- unwound out of the phase.
--
-- THE SUBJECT KEYS ARE NULLABLE AND LET GO ON DELETE
-- --------------------------------------------------
-- A stage create's rows name its `stage_set_id`; a write's, resume's or
-- verify's name the `volume_id`. `snapshot delete`/`snapshot purge` delete
-- stage sets, and a timing row must never be the reason such a delete
-- fails, so both keys are `ON DELETE SET NULL`: the timing outlives its
-- subject as a measurement of something that happened.
--
-- Plain CREATE, touching no other table, so no `.foreign_key_check()`.
-- The on-tape `catalog.db` is built from an explicit schema
-- (`db::ontape_catalog`), so this table never reaches a tape.

CREATE TABLE phase_timings (
    id            INTEGER PRIMARY KEY,
    session       TEXT NOT NULL,
    operation     TEXT NOT NULL,
    volume_id     INTEGER REFERENCES volumes(id) ON DELETE SET NULL,
    stage_set_id  INTEGER REFERENCES stage_sets(id) ON DELETE SET NULL,
    seq           INTEGER NOT NULL,
    phase         TEXT NOT NULL,
    started_at    TEXT NOT NULL,
    duration_ms   INTEGER NOT NULL,
    bytes         INTEGER,
    outcome       TEXT NOT NULL
);

CREATE INDEX idx_phase_timings_volume ON phase_timings(volume_id);
CREATE INDEX idx_phase_timings_stage_set ON phase_timings(stage_set_id);
