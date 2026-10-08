-- 034: `dar_create_reports` -- dar's report for every `dar -c` a stage set
-- ran, verbatim (issue #343, the staging half of #306; ADR-0013 §§4, 7).
--
-- (Numbered 032 on the `forensics` branch; renumbered 034 at integration,
-- after master's 032 and 033.)
--
-- WHY
-- ---
-- `stage create` runs `dar -c -` and reads its exit code. What dar SAID --
-- on standard error, since standard output is the archive -- was read for an
-- eight-line excerpt on failure and dropped on every exit: a file it could
-- not read, a file that changed while it read it (exit 11, the DIRTY
-- refusal), an attribute it could not save. Those are the forensic facts an
-- operator needs years later about what a stage set did and did not
-- capture, and the restore side has kept dar's report since migration 024.
-- This table is the same capture for archive creation.
--
-- ONE ROW PER dar RUN, ON EVERY OUTCOME
-- -------------------------------------
-- Written once dar has been spawned and has ended, however it ended:
-- `complete` (exit 0), `files_changed` (exit 11, refused as DIRTY),
-- `failed` (any other exit), `aborted` (the archive's consumer failed and
-- tapectl stopped dar), `stopped` (a signal to tapectl stopped it). Free
-- TEXT with no CHECK (ADR-0013 §4), pinned by a test on the values code
-- writes. A stage that never spawned dar has no row.
--
-- THE STAGE SET MAY GO; THE ROW STAYS
-- -----------------------------------
-- `stage_set_id` is `ON DELETE SET NULL`, as `phase_timings` (028) does:
-- `snapshot purge` deletes stage sets, and the journals are never pruned
-- (ADR-0012, 2026-10-07 item 28). `unit_name` and `snapshot_version` are
-- kept beside it, as `restores` (024) keeps its names, so the row still says
-- whose archive it was after its stage set is gone.
--
-- VERBATIM
-- --------
-- `dar_stderr` is the bytes dar wrote, BLOB, never decoded or trimmed.
-- `dar_command` is the command as `stage_sets.dar_command` records it (the
-- on-the-fly catalogue path written as `<catalogue>`). `dar_exit_code` is
-- NULL when dar was ended by a signal. `tapectl_version` is the observer
-- (ADR-0013 §7).
CREATE TABLE dar_create_reports (
    id                INTEGER PRIMARY KEY,
    captured_at       TEXT NOT NULL DEFAULT (datetime('now')),
    stage_set_id      INTEGER REFERENCES stage_sets(id) ON DELETE SET NULL,
    unit_name         TEXT,
    snapshot_version  INTEGER,
    outcome           TEXT NOT NULL,
    dar_command       TEXT NOT NULL,
    dar_exit_code     INTEGER,
    dar_stderr        BLOB,
    dar_version       TEXT,
    tapectl_version   TEXT NOT NULL
);

CREATE INDEX idx_dar_create_reports_stage_set ON dar_create_reports(stage_set_id);
