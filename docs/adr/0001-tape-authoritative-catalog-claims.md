# The tape is authoritative for its own contents; the catalog is a claims ledger

The audit (docs/audits/2026-07-18-code-quality-drift.md) showed tapectl holds two
replicas of reality — SQLite catalog and tape — with no authority rule, producing a
family of HIGH defects (write-without-reading-tape, retire counting destroyed copies,
verify overstating its strength). We decided: a tape is ground truth about *itself*,
exactly at **contact** (when loaded and read/written). The catalog is never
authoritative — it is a ledger of **claims** about tape contents (trustworthy only with
**evidence**: write receipts, verification sessions, which age), **derivations**
(cross-volume facts like copy counts, computed over claims and only as strong as their
weakest evidence), and operator **assertions** (physical facts like cartridge location,
checked at contact). Divergence detected at contact **quarantines** the volume rather
than silently picking a winner.

Consequences: every tape-writing operation must read the tape's self-description first
and refuse on mismatch; operations that consume derivations (retire, mark-tape-only,
compaction) must state their evidence requirements; verification is the act of
converting claims into evidenced claims, so its recorded strength must match what was
actually performed.

Considered and rejected: catalog-authoritative (matches current code behavior; the
audit is the catalog of its failure modes), and pure "tape is truth" for all facts
(incoherent — no tape can attest to cross-volume aggregates or physical locations).
