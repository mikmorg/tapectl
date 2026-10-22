-- 039: the write status 'range', `writes.may_span`,
-- `stage_slices.released_at` and `archive_sets.allow_spanning` (issue #478,
-- spanning; ADR-0012 items 121, 127, 128 and 129, and 82 for the key).
--
-- WHY
-- ---
-- From 1.7.0 a unit whose policy allows it may span cartridges: its stage
-- set is written as one unbroken run of slices per tape (a *range*), and the
-- drive's early warning, not the plan, ends the run on each tape (items 84,
-- 87, 122). 038's `cut` cannot record that: a cut is never a copy and never
-- holds staging (item 112), while the slices a range wrote are copies of
-- those slices, counted per slice (item 127), and the version they belong to
-- is protected by the retire floor before it is whole anywhere (item 128).
-- So a range has its own status (item 121):
--
--   * `range`: a write of a spannable set that the early warning ended
--     inside it. Terminal. Its `write_positions` rows are `written` up to the
--     tape's ending and `pending` past it, so the range is the row's written
--     positions. It is physically on the tape: `write_reaches_tape` and
--     `has_completed_write` count it (`policy::coverage`).
--   * `writes.may_span`: whether the write was planned as spannable. The
--     dotfile is read at use, so the policy that allowed the span is frozen
--     into the write when it is planned (item 121), and confirm, a rebuild
--     and an adoption read it from here. 0 on every existing row, which is
--     right: no write before 1.7.0 could span. 1 on any other status is
--     legal: a spannable write whose whole set reached the tape is
--     `completed`, and one the warning ended before its first slice is
--     `cut`. A `range` row with may_span = 0 cannot exist, and the table's
--     row CHECK says so.
--
-- `stage_slices.released_at` is when a slice's staged file was released on
-- its own (item 129: staging is released slice by slice once each slice
-- meets the copy floor, so tape 1's slices need not wait for tape 3).
-- `staging_path` goes NULL in the same transaction, and item 129 has the
-- #402 checks accept a missing slice file only when this is set (nothing
-- sets it before the per-slice release is built). NULL on every existing row, which
-- is right: before 1.7.0 a stage set was released whole (`stage_sets.status
-- = 'cleaned'`), never a slice at a time.
--
-- `archive_sets.allow_spanning` is the policy key at the archive-set layer
-- (item 82: it resolves dotfile, then archive set, then `[defaults]`, which
-- is false). Nullable on purpose, like every other archive_sets policy
-- column (007's `warehouse_copies`): NULL means "defer to the next policy
-- layer", never "0".
--
-- THE GUARD (026's rule)
-- ----------------------
-- 039 only widens the status CHECK, so every row 038's CHECK admitted is
-- copied as it is. A row carrying any other status -- `range` included,
-- which no release before this one writes -- can exist only if someone put
-- it there around the CHECK, and there is no mapping that is right for every
-- such row (a `range` row would also need a `may_span` nobody recorded). So
-- 039 refuses, naming every offending status and row id in one message, and
-- changes nothing; the operator edits those rows to a status 038's schema
-- allows and runs the command again. The mechanism is 026's and 038's: a
-- TEMP table whose only trigger RAISEs the message it is given, and one
-- INSERT that hands it a message only when an offender exists.
-- `src/db/mod.rs::migrate_to` shows a trigger-raised message as it is. The
-- new CHECKs remain the backstop. Apply this migration only through tapectl,
-- whose bundled SQLite accepts `RAISE(ABORT, <expression>)`.
--
-- THE REBUILD
-- -----------
-- SQLite cannot ALTER a CHECK in place, so `writes` is rebuilt exactly as
-- 038 rebuilt it: CREATE writes_new, copy with `id` named on both sides (a
-- copy that let SQLite assign rowids would silently re-point
-- `write_positions.write_id`), DROP, RENAME writes_new INTO the name (never
-- rename the old table away: with `legacy_alter_table` OFF that rewrites
-- every other table's REFERENCES clause to follow it), then recreate its
-- indexes. Every column, type, default and constraint is 038's but for the
-- one status value, with `may_span` added last and the row CHECK after the
-- UNIQUE constraint. The copy leaves `may_span` to its DEFAULT. The
-- UNIQUE(stage_set_id, volume_id) constraint comes back with its autoindex,
-- and it stays true (item 122: a volume holds at most one unbroken range of
-- a stage set); `idx_writes_stage_set`, `idx_writes_snapshot` and
-- `idx_writes_volume` are recreated as 038 made them. No trigger or view
-- references `writes`.
--
-- The one foreign key into `writes` -- `write_positions.write_id` -- lives on
-- the referencing table and is not touched. FK enforcement is off for the
-- DROP (`migrate_to` turns it off outside the transaction) and the migration
-- is registered with `.foreign_key_check()`, so a lost edge fails the
-- migration instead of landing.
--
-- The two added columns are plain ADD COLUMNs, with no rebuild: neither table
-- changes a constraint.

-- The guard.
CREATE TEMP TABLE m039_refusal (message TEXT NOT NULL);
CREATE TEMP TRIGGER m039_refuse BEFORE INSERT ON m039_refusal
BEGIN
    SELECT RAISE(ABORT, NEW.message);
END;
INSERT INTO m039_refusal (message)
SELECT
    'migration 039 cannot run: ' || group_concat(finding, '; ') || '. '
    || 'No tapectl release before this one writes these statuses, and the '
    || 'schema has never allowed them, so these rows were set by hand around '
    || 'its CHECK, and 039 will not guess what they should say. Change each '
    || 'named row to a status the schema allows today -- planned, '
    || 'in_progress, completed, failed, aborted, interrupted, cut -- then run '
    || 'the command again. Nothing has been changed.'
FROM (
    SELECT 'writes.status = ' || quote(status) || ' on ' || COUNT(*)
           || ' row(s) (id ' || group_concat(id, ', ') || ')' AS finding
      FROM (SELECT id, status FROM writes
             WHERE status IS NULL
                OR status NOT IN ('planned', 'in_progress', 'completed',
                                  'failed', 'aborted', 'interrupted', 'cut')
             ORDER BY id)
     GROUP BY status
)
HAVING COUNT(*) > 0;
DROP TRIGGER m039_refuse;
DROP TABLE m039_refusal;

-- writes: 'range' joins the status CHECK, and may_span is added.
CREATE TABLE writes_new (
    id                  INTEGER PRIMARY KEY,
    stage_set_id        INTEGER NOT NULL REFERENCES stage_sets(id),
    snapshot_id         INTEGER NOT NULL REFERENCES snapshots(id),
    volume_id           INTEGER NOT NULL REFERENCES volumes(id),
    status              TEXT NOT NULL DEFAULT 'planned'
                        CHECK(status IN ('planned','in_progress','completed',
                                         'failed','aborted','interrupted',
                                         'cut','range')),
    write_verified      INTEGER NOT NULL DEFAULT 0,
    eot_recovery        TEXT CHECK(eot_recovery IN (NULL, 'normal',
                                   'overwrite_incomplete', 'sacrifice_last_slice')),
    sacrificed_slice_id INTEGER REFERENCES stage_slices(id),
    started_at          TEXT,
    completed_at        TEXT,
    notes               TEXT,
    created_at          TEXT NOT NULL DEFAULT (datetime('now')),
    session_dir         TEXT,
    may_span            INTEGER NOT NULL DEFAULT 0,
    UNIQUE(stage_set_id, volume_id),
    CHECK(status <> 'range' OR may_span = 1)
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

-- stage_slices: when a slice's staged file was released on its own. NULL on
-- every existing row.
ALTER TABLE stage_slices ADD COLUMN released_at TEXT;

-- archive_sets: the spanning key at the archive-set layer. NULL defers to the
-- next layer.
ALTER TABLE archive_sets ADD COLUMN allow_spanning INTEGER;
