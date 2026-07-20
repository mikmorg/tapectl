# One storage interface; tape, warehouse, and export stores are peers

The cloud decision (#11) and the backend-trait decision (#15) resolved together: a
location's configuration selects its **store** — a first-class implementation of one
storage interface — rather than tape being the primary medium with cloud bolted on.
The interface is ADR-0002's seam verbatim: Layout construction and validation are
medium-agnostic; a store *executes a Layout at contact and confirms it*. `TapeStore`
(LTO via st/sg: contact is a drive, confirm is the readback after the final filemark)
and `WarehouseStore` (S3 API: contact is the API, confirm is the deposit receipt —
SHA-256 verified at PUT; retrieval-contact begins with a restore-request, the
load-the-cartridge analog) are both first-class; the export directory output becomes
`ExportStore` (BD/USB) on the same seam. Sealed volumes are the interchange unit
everywhere; a warehouse volume's zones map hot/cold — metadata zones (ID thunk,
guide, RESTORE.sh, mini-index, envelopes) at instant-access class, slices at
GLACIER/DEEP_ARCHIVE class — all through the modern S3 storage-class API (the legacy
Glacier vault API is explicitly out of scope).

Evidence is per-store and ADR-0004 is unchanged: tape evidence comes from physical
re-verification at contact and decays with the medium; warehouse evidence is the
deposit receipt plus provider attestation, aging without refresh (re-verification
costs retrieval and realistically never happens) — named honestly as a different
evidence class, warned about at destructive moments, never gated on. Policy
(archive_set/dotfile, resolved like every other knob) selects which units carry
warehouse copies — LTO is the primary line; the irreplaceable core earns the extra
leg. Encrypted catalog snapshots (ADR-0005's kit artifact) additionally ride S3
instant-access on the same after-each-write-session refresh rule.

Consequences: the trait is carved during the Layout/WriteSession build (phase 1) with
TapeStore as the first implementation, so no tape-isms bake into the seam;
WarehouseStore lands later as a native implementation (uploads may begin life as
rclone/aws-cli procedure, but first-class means tapectl ultimately owns
execute/confirm/restore-request); the heir kit and runbook must state the billing
fragility — a warehouse copy dies weeks after payment stops; tapes are the durable
line.

Considered and rejected: instant-access-only cloud (forfeits the disaster-copy shape
— for a copy retrieved at most once in decades, holding cost dominates and
Deep Archive's profile is correct; the studied tools' Glacier failures stem from
in-repo indexes and repack reads, neither of which tapectl has); cloud as external
practice only with no model presence (leaves warehouse copies invisible to copy
derivations, fire-risk, and audit — the catalog must claim them to reason about
them); a native S3 backend bolted beside the tape path without the shared interface
(recreates the three-diverged-writers problem the audits documented); tape-primary
with warehouse as a special case (the interface is the general shape; primacy is an
operational fact, not an architectural one).
