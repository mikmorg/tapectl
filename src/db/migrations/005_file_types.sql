-- Classify file entries by filesystem type (issue #33/H7).
--
-- `walk_directory` used to record every non-directory entry as an
-- undifferentiated "file", using `entry.metadata()` (never follows symlinks —
-- `WalkDir::follow_links(false)`) to derive `size_bytes`. For a symlink, lstat
-- reports the length of the *target path string*, not any real content size —
-- but the validator (`check_source_size` in `staging/validate.rs`) used
-- `std::fs::metadata`, which DOES follow, and compared against the *target's*
-- content size instead. Any symlink whose name-target length differed from
-- its target's content size produced a false DIRTY; a broken symlink was
-- reported as a missing source file; opening a FIFO with no writer via
-- `File::open` blocked forever with no timeout.
--
-- The fix: the walk and the validator must agree on link-following semantics
-- (both classify via symlink metadata, never following), and content
-- validation (size check + sha256) applies to regular files only. This
-- column pair records what `walk_directory` actually saw, so the validator
-- filters on that recorded fact directly instead of re-deriving type
-- information of its own that could silently disagree again.
--
-- `file_type` is one of 'dir' / 'regular' / 'symlink' / 'special' (FIFO,
-- socket, block/char device); `link_target` is the raw target string
-- (`std::fs::read_link`) for a symlink, NULL for everything else. Both
-- nullable + backfilled (SQLite's ADD COLUMN can't attach a NOT NULL default
-- derived from another column) rather than dropping or repurposing the
-- existing `is_directory` flag other queries already depend on.
ALTER TABLE files ADD COLUMN file_type TEXT;
ALTER TABLE files ADD COLUMN link_target TEXT;
ALTER TABLE manifest_entries ADD COLUMN file_type TEXT;
ALTER TABLE manifest_entries ADD COLUMN link_target TEXT;

-- Backfill: every existing row predates this distinction, so only the
-- dir/not-dir split survives from `is_directory` — the regular/symlink/
-- special split was never recorded and cannot be recovered after the fact.
-- `link_target` stays NULL for every backfilled row: correctly "unknown",
-- not an empty string standing in for "no target". A pre-existing symlink
-- row is therefore backfilled as 'regular' and will still false-DIRTY until
-- its unit is re-`snapshot create`d — this migration cannot retroactively
-- know which old rows were symlinks.
UPDATE files SET file_type = CASE WHEN is_directory = 1 THEN 'dir' ELSE 'regular' END;
UPDATE manifest_entries SET file_type = CASE WHEN is_directory = 1 THEN 'dir' ELSE 'regular' END;
