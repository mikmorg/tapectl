-- 013: drop `manifest_entries.has_xattrs` / `.has_acls` — always 0, read by
-- nothing, and describing a job that belongs to dar.
--
-- Issue #149. Backwards compatibility is explicitly waived (operator decision,
-- 2026-09-13): a column that is always zero should go rather than mislead.
--
-- WHAT THEY CLAIMED, AND WHAT WAS TRUE
-- ------------------------------------
-- 001_initial.sql gave both columns `DEFAULT 0`, and `src/staging/mod.rs`
-- bound literal `0i32` for each with the comment "populated on stage" next to
-- it. Nothing has ever fulfilled that comment, and nothing has ever SELECTed
-- either column: they were 100% zeros with a promise attached, which is worse
-- than absent, because a reader querying `WHERE has_xattrs = 1` would get a
-- confident, wrong, empty answer.
--
-- They are not being rebuilt as honest columns because tapectl is not the
-- component that knows: dar owns xattr and ACL handling (`--alter=atime`,
-- `--acl`, `--ea` — `defaults.preserve_xattrs` / `.preserve_acls` are passed
-- THROUGH to dar), and dar records what it preserved in its own archive
-- catalog. A second copy of that fact in the manifest could only ever drift
-- from the archive that actually holds the bytes.
--
-- THE REBUILD
-- -----------
-- SQLite has supported `ALTER TABLE ... DROP COLUMN` since 3.35, but this
-- follows 012's create/copy/drop/rename procedure instead, for the same two
-- reasons 012 spells out at length:
--
--   1. The INSERT names `id` EXPLICITLY on both sides. An INSERT that omitted
--      `id` would let SQLite assign fresh rowids. `manifest_entries.id` has no
--      inbound foreign key today, but the table is joined by `manifest_id` and
--      an id renumber is the kind of silent damage that is unrecoverable once
--      committed.
--
--   2. The order is CREATE-new, copy, DROP-old, RENAME — never rename the old
--      table out of the way first. Nothing holds a `REFERENCES
--      manifest_entries(id)` FK (this is the only table in the schema with no
--      inbound reference to worry about), so this ordering is defensive rather
--      than load-bearing here; it is written this way so the next person to
--      copy a migration copies the correct shape.
--
-- Every surviving column, type, default and constraint below is byte-for-byte
-- identical to 001_initial.sql:119 plus 005_file_types.sql's two appended
-- columns (`file_type`, `link_target`) — which MUST be carried, and in that
-- order, since ADD COLUMN appended them after `has_acls`.

CREATE TABLE manifest_entries_new (
    id           INTEGER PRIMARY KEY,
    manifest_id  INTEGER NOT NULL REFERENCES manifests(id),
    path         TEXT NOT NULL,
    size_bytes   INTEGER NOT NULL,
    mtime        TEXT NOT NULL,
    sha256       TEXT,
    is_directory INTEGER NOT NULL DEFAULT 0,
    mode         INTEGER,
    uid          INTEGER,
    gid          INTEGER,
    username     TEXT,
    groupname    TEXT,
    file_type    TEXT,
    link_target  TEXT
);

INSERT INTO manifest_entries_new (
    id, manifest_id, path, size_bytes, mtime, sha256, is_directory,
    mode, uid, gid, username, groupname, file_type, link_target
)
SELECT
    id, manifest_id, path, size_bytes, mtime, sha256, is_directory,
    mode, uid, gid, username, groupname, file_type, link_target
FROM manifest_entries;

DROP TABLE manifest_entries;

ALTER TABLE manifest_entries_new RENAME TO manifest_entries;

-- The one index the old table carried (001_initial.sql:367). A rebuild that
-- forgot it would turn every manifest lookup into a full table scan on the
-- largest table in the schema.
CREATE INDEX idx_manifest_entries_manifest ON manifest_entries(manifest_id);
