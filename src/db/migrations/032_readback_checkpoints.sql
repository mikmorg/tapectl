-- 032: `readback_checkpoints` -- which files a full readback has already
-- read back clean, recorded as it goes (issue #410).
--
-- WHY
-- ---
-- A full confirm (`--full-confirm`) or a full `volume verify` reads every
-- file of a cartridge back and hashes it against the front index: ~2.3 h at
-- drive speed on a full LTO-6. Until now neither kept any progress (verify
-- did not even write its `verification_sessions` row until the whole walk
-- returned), so an interruption at 90% (a signal since #404, an ssh drop, a
-- reboot) left nothing, and the next readback read the whole tape again:
-- hours of drive time and an extra pass over the medium.
-- `write_positions.status` has allowed `verified` since 001, and
-- `verification_results` has existed as long, but both name only SLICES
-- (`stage_slice_id NOT NULL`); a readback also checks the envelopes and the
-- generated front-zone files, so neither can hold this.
--
-- ONE ROW PER FILE THAT PASSED
-- ----------------------------
-- A row says: in readback session `session_id`, the file at `position`
-- came back in full and hashed to `sha256`, the front index's claim for it,
-- in a front index whose own bytes hashed to `front_index_sha256`. Only
-- passes are recorded -- a file that failed, or was never reached, is read
-- again. A resumed readback skips a file only when the predecessor session
-- was interrupted (never one that finished, passed or failed), the front
-- index it re-reads hashes to the same `front_index_sha256`, the seal it
-- re-reads binds that index, and the claim for the file is still `sha256`
-- -- so a skipped file is one that was checked against the very claims this
-- readback is checking. The seal marker and File 3 are never recorded and
-- never skipped. A skipped file is recorded again under the resuming
-- session, so a chain of interruptions accumulates.
--
-- Written best-effort during the walk: a row that cannot be written costs
-- one re-read on a later resume, never the readback itself.
--
-- New table, no existing row is read or converted (so nothing can fail to
-- convert), plain CREATE, so no `.foreign_key_check()`. Rows go with their
-- session: `ON DELETE CASCADE`. The on-tape `catalog.db` is built from an
-- explicit schema (`db::ontape_catalog`), so this table never reaches a
-- tape.

CREATE TABLE readback_checkpoints (
    session_id          INTEGER NOT NULL
                        REFERENCES verification_sessions(id) ON DELETE CASCADE,
    position            INTEGER NOT NULL CHECK(position >= 0),
    sha256              TEXT NOT NULL,
    front_index_sha256  TEXT NOT NULL,
    checked_at          TEXT NOT NULL DEFAULT (datetime('now')),
    PRIMARY KEY (session_id, position)
);
