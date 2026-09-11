-- 010: record whether a stage set was staged here or rebuilt from a tape.
--
-- Issue #137, CTO decision 2026-09-11 (grilling Q3/Q7). `catalog rebuild`
-- (#136) reconstructs stage_sets rows from a sealed volume's envelope, and no
-- tape written before that decision carries the recipient list — so such a
-- row has key_fingerprints = NULL, and policy::escrow's fail-closed rule
-- reads NULL as "no recorded recipient list", the same words it uses for a
-- corrupt or pre-escrow row that was staged on this machine. The two are not
-- the same thing: one is a claim tapectl never wrote down, the other is a
-- claim it could not have written. audit, catalog locate, report copies and
-- the volume write pre-flight all need to tell them apart, cheaply, per row.
--
-- One small column. This partially revisits #136's "no new column" ruling,
-- which was about rebuild PROVENANCE as an audit trail — that stays an
-- events row. This is a per-row discriminant a predicate must read.
--
-- 'staged'  — created by `stage create` on this machine (every existing row).
-- 'rebuilt' — created by `catalog rebuild` from a tape's envelope.
--
-- A rebuilt row whose key_fingerprints is later filled in by attestation
-- (`catalog rebuild --key <escrow>` trial-decrypting a slice header) keeps
-- origin = 'rebuilt': the receipt was demonstrated, not recorded at staging.
ALTER TABLE stage_sets
    ADD COLUMN origin TEXT NOT NULL DEFAULT 'staged'
        CHECK(origin IN ('staged', 'rebuilt'));
