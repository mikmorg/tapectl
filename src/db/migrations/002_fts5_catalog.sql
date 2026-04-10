-- FTS5 full-text search index on file paths for fast catalog search.
-- Populated via triggers on the files table.

CREATE VIRTUAL TABLE IF NOT EXISTS files_fts USING fts5(
    path,
    content='files',
    content_rowid='rowid'
);

-- Triggers to keep FTS index in sync with files table
CREATE TRIGGER IF NOT EXISTS files_ai AFTER INSERT ON files BEGIN
    INSERT INTO files_fts(rowid, path) VALUES (new.rowid, new.path);
END;

CREATE TRIGGER IF NOT EXISTS files_ad AFTER DELETE ON files BEGIN
    INSERT INTO files_fts(files_fts, rowid, path) VALUES('delete', old.rowid, old.path);
END;

CREATE TRIGGER IF NOT EXISTS files_au AFTER UPDATE ON files BEGIN
    INSERT INTO files_fts(files_fts, rowid, path) VALUES('delete', old.rowid, old.path);
    INSERT INTO files_fts(rowid, path) VALUES (new.rowid, new.path);
END;

-- Backfill existing data
INSERT INTO files_fts(rowid, path) SELECT rowid, path FROM files;
