# Volume writing is plan/execute/confirm over a reified Layout

The audit showed the write pipeline generating on-tape metadata as a side effect of
execution order (mini-index missing envelope entries; SIGINT recording success with
poisoned positions; no EOT recovery possible). We decided the **Layout** — the complete
enumeration of every file a volume will hold, positions/sizes/checksums included — is a
first-class value constructed and *validated before the first byte* (capacity vs. plan
total + reserve, staged slices present with matching checksums, keys resolvable). A
**Write Session** executes a Layout at contact with a cursor; every on-tape metadata
artifact (mini-index, planning header, envelope manifests) is generated *from the
Layout*, never from what happened to get written. Interruption and end-of-tape recovery
are defined Layout transitions (truncate/resume), after which metadata is regenerated
from the Layout as it stands — the invariant either way: **the tape never lies about
itself**. After the final filemark the session seeks back, reads the mini-index, and
diffs it against the Layout, converting the session's claims into evidence (ADR-0001)
at the same contact.

Considered and rejected: patching the current imperative pipeline (fixes one bug,
leaves the bug class), and plan/execute without the confirm readback (a write session
that produces no evidence is just a claim generator, per ADR-0001).
