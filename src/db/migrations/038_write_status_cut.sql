-- 038: the write status 'cut' and `volumes.ended_after` (issue #420, the
-- short seal; ADR-0012 item 112; `layout-session.md` "Session states and
-- persistence" and "The short ending").
--
-- WHY
-- ---
-- From 1.6.0 a write that meets the drive's early warning seals short: the
-- seal goes right after the data slice the warning landed in (position K),
-- and no slice planned after K is written. Confirm then splits the
-- session's rows. A stage set whose every slice is at or before K is
-- `completed`, as today. Every other stage set of the session -- the one the
-- warning interrupted and every one planned after it, whether or not any of
-- its slices reached the tape -- becomes `cut`:
--
--   * terminal, and never a copy: no coverage predicate counts it (they all
--     match `status = 'completed'`);
--   * its `write_positions` rows stay as written, so the slices that did
--     reach the tape stay recorded (spanning, #478, uses them);
--   * no write for the staging release: it neither counts as a copy nor
--     holds the staging as an unfinished write would. A cut unit stays
--     staged until its version has a `completed` write, and the next
--     ordinary `volume write` writes it whole on another cartridge.
--
-- Considered and not chosen (item 112): deleting the rows and logging an
-- event. A `cut` row exists only on a sealed volume whose `ended_after` is
-- set; that invariant is `audit`'s to check, not a constraint here.
--
-- `volumes.ended_after` is the `ended_after` of a volume's SHORT seal: the
-- last data position the tape holds. It is set only on a short-sealed
-- volume, in the same transaction as `sealed_at`, and is NULL on every other
-- (unsealed, sealed before 1.6.0, or sealed full by 1.6.0, whose ended_after
-- is the Layout's M-1 and is not stored). A plain ADD COLUMN: every existing
-- row reads NULL, which is right for all of them, since no tape before 1.6.0
-- could seal short.
--
-- THE GUARD (026's rule)
-- ----------------------
-- 038 only widens the CHECK, so every row the old CHECK admitted is copied
-- as it is. A row carrying any other status can exist only if someone put it
-- there around the CHECK (a hand edit with `ignore_check_constraints`, or a
-- copied-in table), and there is no mapping that is right for every such row.
-- So 038 refuses, naming every offending status and row id in one message,
-- and changes nothing; the operator edits those rows to a status the schema
-- allows and runs the command again. The refusal is 026's mechanism: a TEMP
-- table whose only trigger RAISEs the message it is given, and one INSERT
-- that hands it a message only when an offender exists. `src/db/mod.rs::
-- migrate_to` shows a trigger-raised message as it is. The new CHECK remains
-- the backstop. As 026 says of itself: apply this migration only through
-- tapectl, whose bundled SQLite accepts `RAISE(ABORT, <expression>)`.
--
-- THE REBUILD
-- -----------
-- SQLite cannot ALTER a CHECK in place, so `writes` is rebuilt as 032
-- rebuilt `volumes`: CREATE writes_new, copy with `id` named on both sides (a
-- copy that let SQLite assign rowids would silently re-point
-- `write_positions.write_id`), DROP, RENAME writes_new INTO the name (never
-- rename the old table away: with `legacy_alter_table` OFF that rewrites
-- every other table's REFERENCES clause to follow it), then recreate its
-- indexes. Every column, type, default and constraint is 001's but for the
-- one value, with 006's `session_dir` in the last position, where its ADD
-- COLUMN put it. `eot_recovery`'s CHECK and `sacrificed_slice_id` are the
-- pre-v2 vocabulary nothing writes any more; they are copied verbatim, since
-- dropping a column is not this migration. The UNIQUE(stage_set_id,
-- volume_id) constraint comes back with its autoindex; 027 dropped
-- `idx_writes_status` (issue #373), so only `idx_writes_stage_set`,
-- `idx_writes_snapshot` and `idx_writes_volume` are recreated. No trigger or
-- view references `writes`.
--
-- The one foreign key into `writes` -- `write_positions.write_id` -- lives on
-- the referencing table and is not touched. FK enforcement is off for the
-- DROP (`migrate_to` turns it off outside the transaction) and the migration
-- is registered with `.foreign_key_check()`, so a lost edge fails the
-- migration instead of landing.

-- The guard.
CREATE TEMP TABLE m038_refusal (message TEXT NOT NULL);
CREATE TEMP TRIGGER m038_refuse BEFORE INSERT ON m038_refusal
BEGIN
    SELECT RAISE(ABORT, NEW.message);
END;
INSERT INTO m038_refusal (message)
SELECT
    'migration 038 cannot run: ' || group_concat(finding, '; ') || '. '
    || 'No tapectl release writes these statuses, and the schema has never '
    || 'allowed them, so these rows were set by hand around its CHECK, and 038 '
    || 'will not guess what they should say. Change each named row to a status '
    || 'the new schema allows -- planned, in_progress, completed, failed, '
    || 'aborted, interrupted, cut -- then run the command again. Nothing has '
    || 'been changed.'
FROM (
    SELECT 'writes.status = ' || quote(status) || ' on ' || COUNT(*)
           || ' row(s) (id ' || group_concat(id, ', ') || ')' AS finding
      FROM (SELECT id, status FROM writes
             WHERE status IS NULL
                OR status NOT IN ('planned', 'in_progress', 'completed',
                                  'failed', 'aborted', 'interrupted')
             ORDER BY id)
     GROUP BY status
)
HAVING COUNT(*) > 0;
DROP TRIGGER m038_refuse;
DROP TABLE m038_refusal;

-- writes: 'cut' joins the status CHECK.
CREATE TABLE writes_new (
    id                  INTEGER PRIMARY KEY,
    stage_set_id        INTEGER NOT NULL REFERENCES stage_sets(id),
    snapshot_id         INTEGER NOT NULL REFERENCES snapshots(id),
    volume_id           INTEGER NOT NULL REFERENCES volumes(id),
    status              TEXT NOT NULL DEFAULT 'planned'
                        CHECK(status IN ('planned','in_progress','completed',
                                         'failed','aborted','interrupted',
                                         'cut')),
    write_verified      INTEGER NOT NULL DEFAULT 0,
    eot_recovery        TEXT CHECK(eot_recovery IN (NULL, 'normal',
                                   'overwrite_incomplete', 'sacrifice_last_slice')),
    sacrificed_slice_id INTEGER REFERENCES stage_slices(id),
    started_at          TEXT,
    completed_at        TEXT,
    notes               TEXT,
    created_at          TEXT NOT NULL DEFAULT (datetime('now')),
    session_dir         TEXT,
    UNIQUE(stage_set_id, volume_id)
);

INSERT INTO writes_new (
    id, stage_set_id, snapshot_id, volume_id, status, write_verified,
    eot_recovery, sacrificed_slice_id, started_at, completed_at, notes,
    created_at, session_dir
)
SELECT
    id, stage_set_id, snapshot_id, volume_id, status, write_verified,
    eot_recovery, sacrificed_slice_id, started_at, completed_at, notes,
    created_at, session_dir
FROM writes;

DROP TABLE writes;

ALTER TABLE writes_new RENAME TO writes;

CREATE INDEX idx_writes_stage_set ON writes(stage_set_id);
CREATE INDEX idx_writes_snapshot ON writes(snapshot_id);
CREATE INDEX idx_writes_volume ON writes(volume_id);

-- volumes: the short seal's ending. NULL on every existing row.
ALTER TABLE volumes ADD COLUMN ended_after INTEGER;
