-- 030: per-file data, stored once per unit (issue #380 option A, issue #381;
-- ADR-0012 amendment 2026-10-06, item 7).
--
-- WHY
-- ---
-- `files` held one row per walked entry for EVERY version, each carrying its
-- full path text, a 64-character hex sha256 and an RFC 3339 mtime string, and
-- `files_fts` indexed every one of those rows. A unit re-versioned ten times
-- stored, and indexed, each unchanged path ten times. `catalog search`
-- matched every version of a file and `catalog stats` scanned every row
-- (#380). The research behind the ruling, with the size analysis, is
-- `docs/research/2026-10-06-plaintext-free-staging.md` §6.4.
--
-- THE NEW SHAPE
-- -------------
-- `paths`          one row per distinct path per unit. `paths_fts` indexes
--                  these, so search and the index grow with DISTINCT paths,
--                  not with versions x paths.
-- `file_versions`  one narrow membership row per (version, path), WITHOUT
--                  ROWID, keyed (snapshot_id, path_id) and with no second
--                  index:
--                    kind        0 dir, 1 regular, 2 symlink, 3 special
--                                (FIFO, socket, device) -- NOT NULL, #381's
--                                CHECK. `is_directory` is `kind = 0`.
--                    size_bytes  as before (lstat's size; 0 for a directory)
--                    mtime_ns    Unix time in nanoseconds, UTC. The walks
--                                record whole seconds, as they always have,
--                                so every value today is a multiple of 1e9.
--                    sha256      32 raw bytes, not 64 hex characters
--                    link_target a symlink's target; NULL otherwise
--
-- dar's catalogue is the authority for permissions, owners, xattrs, ACLs,
-- device numbers and hard links (ADR-0012 item 7): they ride every envelope
-- on tape and stay out of this database. `kind` and `link_target` stay,
-- because dirty detection needs them.
--
-- A path row outlives the versions that held it (a purged or deleted
-- version leaves its paths interned): `file_versions.path_id` is a foreign
-- key with no index of its own, so deleting a path would scan every
-- membership row, and an unreferenced path costs one row and is never shown
-- -- every query reaches a path through a membership row.
--
-- CONVERSION, AND WHAT REFUSES IT
-- -------------------------------
-- Every row converts exactly or the migration refuses, naming the rows and
-- changing nothing (026/027's rule), with one exception below. Refused:
--   (a) a row whose snapshot does not exist (no unit to intern its path
--       under);
--   (b) `is_directory` other than 0/1, a `file_type` outside the four names,
--       or a `file_type` that disagrees with `is_directory`;
--   (c) a sha256 that is not 64 lowercase hex characters (it could not come
--       back as the same text);
--   (d) a `modified_at` that is not the walk's own spelling,
--       `YYYY-MM-DDTHH:MM:SS+00:00` (chrono's `to_rfc3339` of a whole-second
--       UTC time), the only spelling that converts to an integer and back
--       unchanged.
-- (b), (c) and the rest of (d) are hand edits. One far-off modified_at is
-- not: every walk before 030 spelled a file's own mtime, so one past year
-- 9999 was spelled `+10000-...` and is refused by (d) as malformed text.
-- Deleting a row is the remedy only for (a): a version one row short of its
-- `file_count` cannot be staged (staging's file-list check).
--
-- THE EXCEPTION: AN MTIME NO NANOSECOND COUNT HOLDS
-- -------------------------------------------------
-- A `modified_at` in the walk's spelling but outside 1677-09-21..2262-04-11
-- (a file stamped 1601-01-01, the zero time of an NTFS volume, say) is not
-- refused: no nanosecond count in an INTEGER holds it (it would overflow to
-- a REAL), so it converts to a NULL `mtime_ns`, which is what the walk and
-- `catalog rebuild` record for the same file today -- the unit still reads
-- as unchanged. ADR-0012 amendment 2026-10-07 item 9: a refusal here blocked
-- every command on the host for a value nothing is lost by dropping. The
-- rows are kept in the TEMP table `m030_mtime_nulled` (files row id,
-- snapshot, the new path id, the old text); `db::migrate_to` reads it after
-- the migration commits, warns naming every row, and drops it. TEMP, so it
-- is never part of the schema, and rolled back with everything else if any
-- later check refuses.
--
-- The one expected gap is a NULL `file_type`: rows `catalog rebuild` wrote
-- before #381, and the pre-005 rows 005 backfilled. They take the type
-- `is_directory` gives, exactly as 005 did (a pre-005 symlink therefore
-- stays 'regular', which 005 already accepted).
--
-- After the copy, the row counts, the hash count and the mtime count (less
-- the mtimes written as NULL above) must match the source or the migration
-- refuses.
--
-- The search index is built once, by FTS5's 'rebuild', after `paths` is
-- full -- not row by row through a trigger (#413). `db::migrate` VACUUMs
-- after this migration, as after 027: `files` and `files_fts` were most of
-- the file.

CREATE TEMP TABLE m030_refusal (message TEXT NOT NULL);
CREATE TEMP TRIGGER m030_refuse BEFORE INSERT ON m030_refusal
BEGIN
    SELECT RAISE(ABORT, NEW.message);
END;

INSERT INTO m030_refusal (message)
SELECT
    'migration 030 cannot run: ' || group_concat(finding, '; ') || '. '
    || 'Migration 030 converts every files row exactly (paths interned per unit, '
    || 'sha256 as 32 bytes, modified_at as an integer) and will not guess a value '
    || 'it cannot convert. Correct each named files row (a sha256 or modified_at '
    || 'you cannot recover may be set to NULL; delete only a row whose snapshot no '
    || 'longer exists), then run the command again. Nothing has been changed.'
FROM (
    SELECT n || ' files row(s) whose snapshot does not exist (id ' || ids
           || CASE WHEN n > 10 THEN ', ...' ELSE '' END || ')' AS finding
      FROM (SELECT COUNT(*) AS n,
                   (SELECT group_concat(id, ', ') FROM (
                        SELECT f.id FROM files f
                         WHERE NOT EXISTS (SELECT 1 FROM snapshots s WHERE s.id = f.snapshot_id)
                         ORDER BY f.id LIMIT 10)) AS ids
              FROM files f
             WHERE NOT EXISTS (SELECT 1 FROM snapshots s WHERE s.id = f.snapshot_id))
     WHERE n > 0
    UNION ALL
    SELECT n || ' files row(s) whose is_directory/file_type is not one of the '
           || 'known pairs (id ' || ids
           || CASE WHEN n > 10 THEN ', ...' ELSE '' END || ')'
      FROM (SELECT COUNT(*) AS n,
                   (SELECT group_concat(id, ', ') FROM (
                        SELECT id FROM files
                         WHERE is_directory NOT IN (0, 1)
                            OR (file_type IS NOT NULL
                                AND file_type NOT IN ('dir', 'regular', 'symlink', 'special'))
                            OR (file_type IS NOT NULL
                                AND (file_type = 'dir') <> (is_directory = 1))
                         ORDER BY id LIMIT 10)) AS ids
              FROM files
             WHERE is_directory NOT IN (0, 1)
                OR (file_type IS NOT NULL
                    AND file_type NOT IN ('dir', 'regular', 'symlink', 'special'))
                OR (file_type IS NOT NULL
                    AND (file_type = 'dir') <> (is_directory = 1)))
     WHERE n > 0
    UNION ALL
    SELECT n || ' files row(s) whose sha256 is not 64 lowercase hex characters (id '
           || ids || CASE WHEN n > 10 THEN ', ...' ELSE '' END || ')'
      FROM (SELECT COUNT(*) AS n,
                   (SELECT group_concat(id, ', ') FROM (
                        SELECT id FROM files
                         WHERE sha256 IS NOT NULL
                           AND (typeof(sha256) <> 'text' OR length(sha256) <> 64
                                OR sha256 GLOB '*[^0-9a-f]*')
                         ORDER BY id LIMIT 10)) AS ids
              FROM files
             WHERE sha256 IS NOT NULL
               AND (typeof(sha256) <> 'text' OR length(sha256) <> 64
                    OR sha256 GLOB '*[^0-9a-f]*'))
     WHERE n > 0
    UNION ALL
    SELECT n || ' files row(s) whose modified_at is not YYYY-MM-DDTHH:MM:SS+00:00 (id '
           || ids || CASE WHEN n > 10 THEN ', ...' ELSE '' END || ')'
      FROM (SELECT COUNT(*) AS n,
                   (SELECT group_concat(id, ', ') FROM (
                        SELECT id FROM files
                         WHERE modified_at IS NOT NULL
                           AND strftime('%Y-%m-%dT%H:%M:%S+00:00', modified_at)
                               IS NOT modified_at
                         ORDER BY id LIMIT 10)) AS ids
              FROM files
             WHERE modified_at IS NOT NULL
               AND strftime('%Y-%m-%dT%H:%M:%S+00:00', modified_at) IS NOT modified_at)
     WHERE n > 0
)
HAVING COUNT(*) > 0;

-- Issue #381: the type `is_directory` gives, for the rows that carry none.
UPDATE files SET file_type = CASE WHEN is_directory = 1 THEN 'dir' ELSE 'regular' END
 WHERE file_type IS NULL;

CREATE TABLE paths (
    id      INTEGER PRIMARY KEY,
    unit_id INTEGER NOT NULL REFERENCES units(id),
    path    TEXT NOT NULL,
    UNIQUE(unit_id, path)
);

CREATE TABLE file_versions (
    snapshot_id INTEGER NOT NULL REFERENCES snapshots(id),
    path_id     INTEGER NOT NULL REFERENCES paths(id),
    kind        INTEGER NOT NULL CHECK(kind IN (0, 1, 2, 3)),
    size_bytes  INTEGER NOT NULL,
    mtime_ns    INTEGER,
    sha256      BLOB CHECK(sha256 IS NULL
                           OR (typeof(sha256) = 'blob' AND length(sha256) = 32)),
    link_target TEXT,
    PRIMARY KEY (snapshot_id, path_id)
) WITHOUT ROWID;

-- Interned in the order the rows were first written, which is walk order.
INSERT INTO paths (unit_id, path)
SELECT s.unit_id, f.path
  FROM files f JOIN snapshots s ON s.id = f.snapshot_id
 GROUP BY s.unit_id, f.path
 ORDER BY MIN(f.id);

-- The exception above: every remaining modified_at is in the walk's
-- spelling (the guard refused the rest), so this is exactly the set outside
-- the range an i64 of nanoseconds holds.
CREATE TEMP TABLE m030_mtime_nulled AS
SELECT f.id AS files_id, f.snapshot_id, p.id AS path_id, f.modified_at
  FROM files f
  JOIN snapshots s ON s.id = f.snapshot_id
  JOIN paths p ON p.unit_id = s.unit_id AND p.path = f.path
 WHERE f.modified_at IS NOT NULL
   AND CAST(strftime('%s', f.modified_at) AS INTEGER)
       NOT BETWEEN -9223372036 AND 9223372036
 ORDER BY f.id;

INSERT INTO file_versions (snapshot_id, path_id, kind, size_bytes, mtime_ns, sha256,
                           link_target)
SELECT f.snapshot_id,
       p.id,
       CASE f.file_type WHEN 'dir' THEN 0 WHEN 'regular' THEN 1
                        WHEN 'symlink' THEN 2 WHEN 'special' THEN 3 END,
       f.size_bytes,
       CASE WHEN f.id IN (SELECT files_id FROM m030_mtime_nulled) THEN NULL
            ELSE CAST(strftime('%s', f.modified_at) AS INTEGER) * 1000000000 END,
       unhex(f.sha256),
       f.link_target
  FROM files f
  JOIN snapshots s ON s.id = f.snapshot_id
  JOIN paths p ON p.unit_id = s.unit_id AND p.path = f.path
 ORDER BY f.snapshot_id, p.id;

-- The copy is whole, or nothing changes.
INSERT INTO m030_refusal (message)
SELECT 'migration 030 cannot run: the converted rows do not match the source ('
       || (SELECT COUNT(*) FROM files) || ' files rows, '
       || (SELECT COUNT(*) FROM file_versions) || ' converted; '
       || (SELECT COUNT(*) FROM files WHERE sha256 IS NOT NULL) || ' hashes, '
       || (SELECT COUNT(*) FROM file_versions WHERE sha256 IS NOT NULL) || ' converted; '
       || (SELECT COUNT(*) FROM files WHERE modified_at IS NOT NULL) || ' mtimes ('
       || (SELECT COUNT(*) FROM m030_mtime_nulled) || ' out of range), '
       || (SELECT COUNT(*) FROM file_versions WHERE mtime_ns IS NOT NULL) || ' converted; '
       || (SELECT COUNT(*) FROM (SELECT DISTINCT s.unit_id, f.path FROM files f
                                   JOIN snapshots s ON s.id = f.snapshot_id))
       || ' distinct paths, ' || (SELECT COUNT(*) FROM paths) || ' interned). '
       || 'This is a defect in the migration, not in your catalog. Nothing has been changed.'
 WHERE (SELECT COUNT(*) FROM files) <> (SELECT COUNT(*) FROM file_versions)
    OR (SELECT COUNT(*) FROM files WHERE sha256 IS NOT NULL)
       <> (SELECT COUNT(*) FROM file_versions WHERE sha256 IS NOT NULL)
    OR (SELECT COUNT(*) FROM files WHERE modified_at IS NOT NULL)
       - (SELECT COUNT(*) FROM m030_mtime_nulled)
       <> (SELECT COUNT(*) FROM file_versions WHERE mtime_ns IS NOT NULL)
    OR (SELECT COUNT(*) FROM (SELECT DISTINCT s.unit_id, f.path FROM files f
                                JOIN snapshots s ON s.id = f.snapshot_id))
       <> (SELECT COUNT(*) FROM paths);

DROP TRIGGER m030_refuse;
DROP TABLE m030_refusal;

-- Search over distinct paths, built once.
CREATE VIRTUAL TABLE paths_fts USING fts5(path, content='paths', content_rowid='id');
INSERT INTO paths_fts(paths_fts) VALUES('rebuild');

CREATE TRIGGER paths_ai AFTER INSERT ON paths BEGIN
    INSERT INTO paths_fts(rowid, path) VALUES (new.id, new.path);
END;
CREATE TRIGGER paths_ad AFTER DELETE ON paths BEGIN
    INSERT INTO paths_fts(paths_fts, rowid, path) VALUES('delete', old.id, old.path);
END;
-- A path is never renamed in place: a renamed file is a different path, and
-- every version that held the old name still holds it.
CREATE TRIGGER paths_au BEFORE UPDATE OF unit_id, path ON paths BEGIN
    SELECT RAISE(ABORT, 'paths rows are never changed in place; intern the new path instead');
END;

-- The old shape.
DROP TRIGGER files_ai;
DROP TRIGGER files_ad;
DROP TRIGGER files_au;
DROP TABLE files_fts;
DROP TABLE files;
