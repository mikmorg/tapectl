# Seal status decides copy eligibility; evidence age warns but never gates

Destructive operations (volume retire, unit mark-tape-only, compact-finish) consume the
copy derivation: "does other coverage exist?" We decided a **copy** is any stage_set
claim on a sealed, unquarantined, unretired volume — seal status alone decides
eligibility. Evidence age (time since last verification, per the resolved
verification_interval) is *displayed* whenever a destructive operation relies on that
coverage — "coverage for unit X rests on L6-0003, last verified 15y ago" — but it never
blocks and never requires --force. The bitrot/decades threat axis is handled by the
audit layer's verification cadence (advisory, per the project's audit-never-blocks
principle), not by hard gates at destructive moments.

Considered and rejected: evidence-freshness gating (destructive ops count only copies
verified within the interval — stronger against media decay, but imports blocking
behavior into a system whose policy layer is deliberately advisory), verify-on-demand
(physically re-verify other copies at contact before proceeding — operationally absurd
as a default), and full silence (staleness visible only in reports — withholds the one
fact that matters at the irreversible moment).
