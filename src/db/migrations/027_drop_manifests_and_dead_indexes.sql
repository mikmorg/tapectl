-- 027: drop the write-only manifest tables and seven dead indexes; the FTS
-- update trigger fires on a path change only (issues #372, #373).
--
-- NOTHING IS REBUILT. Every statement below is a DROP or a trigger swap, so
-- no row of any surviving table is copied, renumbered or touched, and no
-- foreign key into a surviving table changes. `files` in particular is left
-- exactly as it is (a `files` rebuild is #380/#381, open CTO decisions, and
-- deliberately not bundled here).
--
-- 1. `manifests` and `manifest_entries` (issue #372)
-- -------------------------------------------------
-- Both were written at `snapshot create`, sha256-backfilled at `stage
-- create`, and deleted at `snapshot purge`/`snapshot delete` -- and read by
-- nothing in production. Every other SELECT of them was under #[cfg(test)].
-- `catalog rebuild` never recreated them (the on-tape `catalog.db` has no
-- such table; rebuild writes only `files`), so the disaster-recovery path
-- already ran without them. `manifest_entries` duplicated `files` (path,
-- size, mtime, sha256, file_type, link_target) and added mode/uid/gid, which
-- dar's own catalogue keeps (migration 013's reasoning for has_xattrs and
-- has_acls); username and groupname were NULL on every production row.
--
-- The cost of keeping them: about 28 MiB of a 91 MiB production catalog, and
-- the per-file backfill UPDATE into `manifest_entries` could search only by
-- `manifest_id`, so it scanned the whole manifest once per file -- O(files^2)
-- row visits under SQLite's single write lock (2.3 billion for a 47,540-file
-- unit). With the table gone the backfill is one UPDATE per file on `files`,
-- served by the UNIQUE(snapshot_id, path) index.
--
-- THE GUARD: `manifest_entries` is dropped only when it holds nothing
-- `files` does not. Two findings refuse the migration, naming counts and a
-- few example ids, in 026's style (a TEMP table whose trigger RAISEs the
-- message it is given, so the guard never reaches `sqlite_master`):
--   (a) an entry whose (snapshot, path) has no `files` row -- a path only the
--       manifest knows;
--   (b) an entry carrying a sha256 its `files` row lacks -- a baseline only
--       the manifest knows.
-- Every production writer kept the two tables in step, so neither is
-- expected; a finding means the catalog was edited by hand, and 027 will not
-- guess which table was right. Nothing is changed when it refuses.
-- `RAISE(ABORT, NEW.message)` needs the bundled SQLite (see 026's header);
-- apply this migration only through tapectl.
--
-- 2. `idx_files_path` and `idx_files_snapshot` (issue #372)
-- --------------------------------------------------------
-- `idx_files_path` appears in no production query plan: every `files` query
-- is keyed by snapshot_id, and path search goes through `files_fts`.
-- `idx_files_snapshot` duplicates the leading column of the
-- UNIQUE(snapshot_id, path) autoindex, which serves every query it served.
-- Together about 15 MiB of the production catalog.
--
-- 3. `files_au` fires on `UPDATE OF path` only (issue #372)
-- --------------------------------------------------------
-- 002 made it `AFTER UPDATE ON files`, so every sha256-only backfill UPDATE
-- deleted and re-inserted an unchanged path in `files_fts` (about 5x the
-- cost of the UPDATE itself). The FTS table indexes `path` alone, so a
-- change to any other column cannot make it stale. The body is 002's,
-- verbatim.
--
-- 4. The six single-column status indexes (issue #373)
-- ----------------------------------------------------
-- With no planner statistics SQLite rates `status = ?` as selective as
-- `stage_set_id = ?`, so the coverage expressions (`policy::coverage`) were
-- driven from these indexes: for every current snapshot the correlated
-- subquery scanned every completed write, O(snapshots x writes) per
-- expression. Each status column has at most seven values, so a status-only
-- filter scans these tables cheaply without an index. Dropping only the
-- `writes` and `snapshots` ones is not enough -- the planner then switches
-- to `idx_volumes_status` -- so all six go. The names are the current ones
-- (026 recreated three of them, 012 one). `db::configure` adds `PRAGMA
-- optimize` as the backstop.
--
-- The space this frees is returned to the filesystem by one VACUUM, run from
-- `db::migrate` after this migration commits (VACUUM cannot run inside the
-- migration's transaction). DROP INDEX and DROP TABLE are otherwise instant.

-- The guard.
CREATE TEMP TABLE m027_refusal (message TEXT NOT NULL);
CREATE TEMP TRIGGER m027_refuse BEFORE INSERT ON m027_refusal
BEGIN
    SELECT RAISE(ABORT, NEW.message);
END;
INSERT INTO m027_refusal (message)
SELECT
    'migration 027 cannot run: ' || group_concat(finding, '; ') || '. '
    || 'Migration 027 drops the manifest_entries table, which every tapectl '
    || 'release has kept in step with files, so these rows were set by hand '
    || 'and 027 will not guess which table is right. Make each named '
    || 'manifest_entries row agree with its files row (or delete it), then run '
    || 'the command again. Nothing has been changed.'
FROM (
    SELECT COUNT(*) || ' manifest_entries row(s) with no files row for the same '
           || 'snapshot and path (id ' || group_concat(id, ', ') || ')' AS finding
      FROM (SELECT me.id AS id
              FROM manifest_entries me
              JOIN manifests m ON m.id = me.manifest_id
             WHERE NOT EXISTS (SELECT 1 FROM files f
                                WHERE f.snapshot_id = m.snapshot_id
                                  AND f.path = me.path)
             ORDER BY me.id)
    HAVING COUNT(*) > 0
    UNION ALL
    SELECT COUNT(*) || ' manifest_entries row(s) carrying a sha256 their files '
           || 'row lacks or disagrees with (id ' || group_concat(id, ', ') || ')'
      FROM (SELECT me.id AS id
              FROM manifest_entries me
              JOIN manifests m ON m.id = me.manifest_id
              JOIN files f ON f.snapshot_id = m.snapshot_id AND f.path = me.path
             WHERE me.sha256 IS NOT NULL
               AND (f.sha256 IS NULL OR f.sha256 <> me.sha256)
             ORDER BY me.id)
    HAVING COUNT(*) > 0
)
HAVING COUNT(*) > 0;
DROP TRIGGER m027_refuse;
DROP TABLE m027_refusal;

-- 1. The manifest tables, child first.
DROP TABLE manifest_entries;
DROP TABLE manifests;

-- 2. The two dead `files` indexes.
DROP INDEX idx_files_path;
DROP INDEX idx_files_snapshot;

-- 3. The FTS update trigger, on a path change only.
DROP TRIGGER files_au;
CREATE TRIGGER files_au AFTER UPDATE OF path ON files BEGIN
    INSERT INTO files_fts(files_fts, rowid, path) VALUES('delete', old.rowid, old.path);
    INSERT INTO files_fts(rowid, path) VALUES (new.rowid, new.path);
END;

-- 4. The six single-column status indexes.
DROP INDEX idx_writes_status;
DROP INDEX idx_snapshots_status;
DROP INDEX idx_volumes_status;
DROP INDEX idx_stage_sets_status;
DROP INDEX idx_units_status;
DROP INDEX idx_cartridges_status;
