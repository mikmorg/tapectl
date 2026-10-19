-- 037: `pending_rewrites` and `pending_rewrite_sets` -- the durable record of a
-- `volume rewrite` (issue #483; ADR-0012 amendment 2026-10-19, item 106).
--
-- WHY A RECORD AT ALL
-- -------------------
-- `volume rewrite SOURCE --label NEW` reads a volume's live slices into
-- staging, retires the volume, erases its cartridge and writes the slices
-- back onto the same cartridge as NEW. From the erase until NEW seals and
-- passes a full readback, the staging disk may hold the ONLY copy of some
-- units (item 106's window). Item 106 requires that nothing release those
-- staged slices in that window -- not `staging clean` (with or without
-- `--force`), not the release `volume write`, `volume resume` and
-- `collection run` run after a write -- and that a failure after the erase
-- leave them staged with the command that resumes the write named.
--
-- Nothing already in the catalog can say that. A staged stage set whose
-- source volume is retired or erased is exactly what an ordinary two-cartridge
-- compaction leaves too, and `staging clean --force` is the operator's
-- deliberate way to release those; a guard derived from coverage alone would
-- either miss a rewrite or start holding today's compaction sets against
-- `--force`. The marker has to name the stage sets that belong to the
-- rewrite, and the destination they are owed to.
--
-- WHAT IS STORED, AND WHAT IS DERIVED
-- -----------------------------------
-- `pending_rewrites` is one row per rewrite: the volume being rewritten, the
-- label of the volume that is to carry its slices (`destination_label` -- a
-- label, not an id, because the destination volume does not exist until
-- `volume init` has run), the chip serial of the cartridge being reused (so a
-- resume can refuse a different cartridge), how consent was given, and how far
-- the command got (`state`). `pending_rewrite_sets` names the stage sets the
-- rewrite staged, which are the sets it holds and the only ones its write
-- puts on tape.
--
-- WHETHER A SET IS STILL HELD IS NOT STORED. A set is held while its rewrite
-- is open (`finished_at IS NULL`) AND no eligible volume carries a completed,
-- fully read back copy of it (`writes.status = 'completed'` with
-- `write_verified = 1`, which only a full confirm or a passing full `volume
-- verify` sets -- ADR-0012 2026-10-06 item 24). The guard therefore lifts by
-- the same fact the item names ("released only after the confirm passes"),
-- whichever command produced that fact, and a crash can leave it neither
-- stuck on nor lifted early. `finished_at` and `outcome` only close the
-- record once the command has seen that fact.
--
-- THE ON-TAPE CATALOG IS NOT TOUCHED. `db/ontape_catalog.rs` carries a fixed
-- set of tables and columns (generation 3, `volume-format-v2.md` section 1.4);
-- neither table is in it. Nothing on tape changes.
--
-- A new pair of tables, no row read or converted, plain CREATE: no
-- `.foreign_key_check()`.
CREATE TABLE pending_rewrites (
    id                INTEGER PRIMARY KEY,
    -- The volume being rewritten. Its row is kept (retired, then erased).
    source_volume_id  INTEGER NOT NULL REFERENCES volumes(id),
    -- The label of the volume that will carry the slices.
    destination_label TEXT NOT NULL CHECK(length(destination_label) > 0),
    -- The cartridge being reused, by the chip's serial (NULL: unreadable).
    cartridge_serial  TEXT,
    -- How the retirement was consented to: `copies_elsewhere` (every live
    -- slice has a copy on another eligible volume: the usual ADR-0008 Tier-2
    -- gate), `typed_label` (the source's label typed at a terminal) or
    -- `typed_flag` (`--only-copy-on-staging`, matching it).
    consent           TEXT NOT NULL
                      CHECK(consent IN ('copies_elsewhere', 'typed_label', 'typed_flag')),
    -- How far the command got. `staged`: the slices are staged and verified
    -- and nothing destructive has happened. `retired`: the source is retired.
    -- `erased`: its cartridge is erased (the window is open). `initialized`:
    -- the destination volume exists. `written`: the destination is sealed and
    -- fully read back and only the release is left.
    state             TEXT NOT NULL DEFAULT 'staged'
                      CHECK(state IN ('staged', 'retired', 'erased', 'initialized', 'written')),
    started_at        TEXT NOT NULL DEFAULT (datetime('now')),
    -- When the erase began: the window opens here.
    window_opened_at  TEXT,
    finished_at       TEXT,
    outcome           TEXT CHECK(outcome IN ('completed', 'abandoned')),
    CHECK((finished_at IS NULL) = (outcome IS NULL))
);

-- At most one OPEN rewrite per source and per destination label.
CREATE UNIQUE INDEX idx_pending_rewrites_open_source
    ON pending_rewrites(source_volume_id) WHERE finished_at IS NULL;
CREATE UNIQUE INDEX idx_pending_rewrites_open_destination
    ON pending_rewrites(destination_label) WHERE finished_at IS NULL;

CREATE TABLE pending_rewrite_sets (
    rewrite_id   INTEGER NOT NULL REFERENCES pending_rewrites(id),
    stage_set_id INTEGER NOT NULL REFERENCES stage_sets(id),
    PRIMARY KEY (rewrite_id, stage_set_id)
);

-- "Is this stage set held?" is asked of one set at a time by every release
-- path.
CREATE INDEX idx_pending_rewrite_sets_stage_set
    ON pending_rewrite_sets(stage_set_id);
