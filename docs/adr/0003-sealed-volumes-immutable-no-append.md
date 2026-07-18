# Sealed volumes are immutable; append is ruled out

A volume becomes **sealed** when its write session's confirm readback passes (ADR-0002):
the tape ends with valid metadata describing everything before it. We decided sealing is
final — a sealed volume is never written again, and tape append (design doc Appendix D)
is explicitly rejected rather than deferred. Rationale: every claim about a sealed
volume is a claim about an immutable object, so verification evidence (ADR-0001) decays
only with the medium, never because of later writes; and a sealed tape is a finished,
self-contained artifact — the simplest possible object to hand an heir. Only sealed
volumes contribute claims to derivations (copy counts, fire-risk, retirement impact);
an unsealed volume is not yet a copy.

The cost is accepted, not ignored: a partial session seals a partial tape. Mitigations
are batching discipline for the bulk-media class (accumulate staged sets, bin-pack full
sessions — nothing in that class is urgent), tolerance of waste for the small
irreplaceable core, and compaction for reclaiming underutilized volumes.

Considered and rejected: append via unseal→extend→reseal (the riskiest possible tape
operation — overwriting at the exact boundary of good data — and every unseal voids the
volume's accumulated verification evidence), and a hybrid accumulating-volume class
(splits the invariant for capacity the intent statement doesn't prioritize). Note:
resuming an *interrupted, never-sealed* session on its own tape is not append and
remains allowed.
