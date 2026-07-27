-- Persist a real volume UUID (T8 escalation, decided 2026-07-27).
--
-- The v2 ID thunk carries `[volume] uuid` alongside `label`, and §2.1 seeds the
-- tenant-envelope permutation from it — but `volumes` had no uuid column, so
-- the T8 flip had to derive one from the label as a labelled stop-gap.
--
-- Deriving it from the label makes the uuid a redundant restatement of the
-- label: the ID thunk's identity pair collapses to a single fact, and resume's
-- identity check (`layout-session.md`: "require ID-thunk identity match (label
-- + uuid) — mismatch = divergence = quarantine") can no longer distinguish
-- "same label, different physical volume" — a relabelled cartridge, or a label
-- reused after a retire. That is precisely the divergence ADR-0001 and contact
-- discipline exist to catch, so the uuid must be an independent identifier.
-- `units.uuid` is the existing precedent: generated once, persisted, carried in
-- the dotfile.
--
-- Added nullable (SQLite cannot ADD COLUMN NOT NULL without a constant
-- default). Existing rows are backfilled here; `volume_init` generates a v4 for
-- new rows; `volume_write` self-heals a NULL by generating and persisting once,
-- so DB fixtures that INSERT without a uuid keep working.
ALTER TABLE volumes ADD COLUMN uuid TEXT;

-- Backfill existing rows with uuid-shaped random values (one randomblob per
-- row, sliced 8-4-4-4-12). These are pre-renovation test volumes; the value
-- only needs to be unique and stable from here on.
UPDATE volumes
SET uuid = lower(
        substr(hex(randomblob(16)), 1, 8) || '-' ||
        substr(hex(randomblob(16)), 1, 4) || '-' ||
        substr(hex(randomblob(16)), 1, 4) || '-' ||
        substr(hex(randomblob(16)), 1, 4) || '-' ||
        substr(hex(randomblob(16)), 1, 12)
    )
WHERE uuid IS NULL;

CREATE UNIQUE INDEX idx_volumes_uuid ON volumes(uuid);
