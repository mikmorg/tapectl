-- 007: warehouse locations, the warehouse_copies policy knob, and volume deposits.
--
-- ADR-0006 makes a "warehouse" (cold cloud storage) a first-class LOCATION KIND
-- alongside the physical shelf. Issue #72 was rescoped by CTO decision
-- (docs/design-errata.md): tapectl does NOT move the bytes. An operator copies a
-- sealed volume's bytes to a warehouse by a documented external procedure
-- (rclone/aws-cli) and then RECORDS that copy here. ADR-0006's rejected option
-- "cloud as external practice only with no model presence" is why the catalog has
-- to claim the copy at all: without a row, warehouse copies stay invisible to the
-- copy derivations, fire-risk, and audit, and the catalog cannot reason about them.
--
-- WHY `volume_deposits` IS NOT A `volume_locations` JOIN TABLE
-- ------------------------------------------------------------
-- The obvious "unification" is to drop `volumes.location_id` and give every volume
-- a set of locations. Do not. The two facts are asymmetric:
--
--   * `volumes.location_id` is SINGLE-VALUED on purpose. It means "where the
--     physical cartridge currently sits". A cartridge is in exactly one place, and
--     `volume_movements` records it moving from one to the next. Making it a set
--     would make "where do I go to fetch this tape?" unanswerable.
--
--   * A deposit is an ADDITIONAL copy of the same sealed bytes somewhere else,
--     produced by copying rather than by moving, with a DIFFERENT EVIDENCE CLASS.
--     Tape evidence comes from physical re-verification at contact and decays with
--     the medium; warehouse evidence is the deposit receipt plus provider
--     attestation, "aging without refresh (re-verification costs retrieval and
--     realistically never happens)" (ADR-0006). Folding both into one join table
--     erases that distinction and silently turns a never-re-verified cloud object
--     into something the evidence display would report like a verified cartridge.
--
-- Deposits DO count toward min_copies and toward distinct-location counts (the
-- catalog claims them so it can reason about them); their weaker evidence is
-- surfaced by `policy::evidence`, never by excluding them from counts, and never
-- by gating anything (ADR-0004: advisory, displayed at destructive moments).
--
-- WHAT IS DELIBERATELY ABSENT
-- ---------------------------
--   * No checksum column. tapectl did not perform the upload, so any checksum an
--     operator typed would be a claim about a claim.
--   * No uri/endpoint/bucket/provider/credential columns. `locations.description`
--     is existing free text and is where an operator writes `s3://bucket/prefix`.

ALTER TABLE locations
    ADD COLUMN kind TEXT NOT NULL DEFAULT 'shelf'
    CHECK(kind IN ('shelf','warehouse'));

-- Nullable on purpose, exactly like every other archive_sets policy column: NULL
-- means "defer to the next policy layer", never "0".
ALTER TABLE archive_sets ADD COLUMN warehouse_copies INTEGER;

CREATE TABLE volume_deposits (
    id            INTEGER PRIMARY KEY,
    volume_id     INTEGER NOT NULL REFERENCES volumes(id),
    location_id   INTEGER NOT NULL REFERENCES locations(id),
    deposited_at  TEXT NOT NULL DEFAULT (datetime('now')),
    receipt       TEXT,
    storage_class TEXT,
    notes         TEXT,
    UNIQUE(volume_id, location_id)
);

CREATE INDEX idx_volume_deposits_volume ON volume_deposits(volume_id);
