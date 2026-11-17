# Layout 3: a tar stream in slices of whole members, every unit able to continue, no compatibility

The decisions behind tapectl 2.0.0 and its on-tape format, Layout Version 3, were ruled by
the CTO between 2026-11-13 and 2026-11-17 as ADR-0012 items 235 to 295, one amendment at a
time, each amending the ones before. This record gathers them into one decision, so that a
reader of Layout 3 does not have to read ADR-0012's amendments in order. ADR-0012 is not
edited, and where this record and an item differ, the item wins.
`docs/design/volume-format-v3.md` is the byte format that carries them out;
`docs/design/layout-session.md`'s "Layout 3" section is the write session's side;
`docs/design/v3-implementation-plan.md` is the build.

## Context

A design and defect review of 2026-11-12 found that the dar catalogue in every envelope since
1.1.0 carried the wrong slice layout (#518), and that the heir's script had grown around dar's
behaviour at slice boundaries (#497, #498). The CTO ruled the existing tapes and catalog test
articles (items 235 to 238) and asked for any format change to be made before production
(item 239). Three rounds of questions followed, each with measurements:
`docs/research/2026-11-16-layout-3-before-the-build.md`, #555 to #567, and Stage 0's notes
`docs/research/2026-11-17-t0*.md`.

## Decision

**The format changes now, and nothing of Layout 2 is carried** (items 239, 242, 258). tapectl
2.0.0 reads and writes Layout 3 only. One baseline schema replaces the 41 migrations, and a
1.x catalog is refused by name. A Layout 2 tape is read with tapectl 1.9.2 or its own
RESTORE.sh; 2.0.0 knows it by its File 0 label and reads nothing else of it, and never writes
over it without `cartridge erase` and the typed label (item 269). "Simplify" removes
compatibility, never a feature (items 242, 246, 248).

**The payload is a tar stream** (items 241, 253, 270, 273 to 275). GNU tar on the host writes
one pax stream per staging from tapectl's exact file list, with numeric owners, xattrs, and an
ACL stored once in its text record. The writer's floor is GNU tar 1.35 built with ACL support,
on every host that writes; the production host's tar is built from the upstream release. The
standard is not lowered to an old tool on the host (item 273). tapectl reads the stream as it
passes: it checks every member against the snapshot, hashes every file, and refuses a stream
that holds a name or has lost an ACL. Sockets are left out and reported. dar leaves the data
zone.

**Slices stay, and each holds whole members** (items 240, 282, 283, 293). A slice is one
age-encrypted tape file, made once at staging and the same on every copy, with every rule
defined on a slice (the early warning W1 to W9, the seals, resume, the retry after a refusal,
replacement by sha256). A slice ends at a member's start; a member larger than `slice_size`
has a run of slices to itself. So a slice that holds only whole members is an archive any tar
reads alone; a long member's run is read only with all its slices joined, the first of which
states no `continues`, and a reader tells it by the next slice's `continues = true`. That a slice's size on tape follows the files in it is accepted. A hard-linked
name depends on the slice of the name archived first, and is listed for the repair when that
slice is not read.

**Each staging has a file table and an id map** (items 254, 265, 272, 276 to 278). The file
table lists every member with its kind, slice, offsets, size, sha256 and path. It is kept in the
catalog database, outlives its slices, rides every tape that holds a slice of the staging, and
a write refuses a unit without one. The id map holds every user and group id the stream
carries, with the host's name for it; a restore shows it and never acts on it.

**Compression is optional, per slice** (items 280, 281, 284 to 287). Each slice is compressed
alone, after the cut and before encryption, by tapectl's own library: gzip, xz or zstd, with an
optional level, chosen in the same three layers as today. The design is not sized to the
present corpus (item 281).

**Every unit may continue on the next cartridge** (items 246, 288, 289). The drive's early
warning ends a tape; the slice in flight finishes, and the slices not written stay staged and
go first on the next cartridge. No unit is cut and written again whole, and none opts in. The
queue is filled to the warning, so one unit lies across the end of every full tape. `volume
rewrite` and compaction carry a range.

**The readers** (items 255, 260, 264, 266, 267, 271). Readers give tar whole slices (or, in
tapectl, whole members by byte range) and judge a restore by the per-file sha256 check; none
zero-fills a lost slice or uses `--ignore-zeros`. The heir has one repair: the damaged paths
are restored from another copy, and a range is a source for the files it holds. A restore may
start on any tape and takes the files that tape holds whole; across cartridges the heir's
script streams with one prompt, where "I do not have it" is an answer. The heir's work
directory is inside the destination.

**The format document's choices** D1 to D22 are ratified as proposed (item 290), among them
no `requires` entry on a Layout 3 tape (D1); its open points are settled (items 294, 295): `tables/<uuid8>_v<version>_s<stage_set_id>`
names; `file_table`, `id_map`, `tar_version` and `continues` in the MANIFEST; the on-tape
`catalog.db` carries what `catalog rebuild` reads plus the file table and the id map; no magic
key beside File 0's; Layout 2's inert salvage columns of `writes` dropped; the on-tape
`stage_sets` and `stage_slices` carrying the columns the catalog gains.

**How production starts** (items 243, 244, 247, 248, 251, 262, 263, 268, 291, 292). The on-tape
authorization for Layout 3 runs until a drill passes on the production run. Production is gated
on total completion of every feature and fix, the drill and the adversarial reads; a second
drive does not gate. Real scale is measured on a made tree before the code and on the largest
real unit at the real-drive acceptance. `v2.0.0` is tagged first, installed by a fresh start
with new keys, writes both cartridges, and the drill decides: a pass makes them production, a
failure means a new release and another fresh start. The go was given on 2026-11-17 (item 292).

## Consequences

- The heir needs `mt`, `dd`, `age`, `tar` and `sha256sum`, and the compression tool of a
  compressed unit; no dar.
- A slice's size on tape, and how well a compressed slice packed, are visible without a key.
- One tape of a span gives every whole file on it read alone; a unit read in more than one tar
  run can lose folder times and a hard link, so one run across the swap is the way to read it
  whole.
- Every production host's tar must meet the writer's floor before it writes.
- The 1.x catalog and tapes cannot be upgraded in place: production starts fresh.
- The work is planned task by task in `docs/design/v3-implementation-plan.md` (item 279).

## Items gathered

ADR-0012 items 239 to 295, with 235 to 238 as their context. Superseded within ADR-0012 by
later items, and read only through them: the zero-fill of item 255 (by 264), item 143 (by
260), items 88 and 132's scratch mode (by 267), item 243's end at the install (by 268), the
plain cut of item 241 (by 283), item 9's "never split by the drive", item 82's opt-in, the
`cut` unit of items 99 and 112, item 113's bound, the second half of item 122's order, item
121's freeze of whether a set may span, item 125's `may_span`, items 83, 94, 95, 123, 124, 133
and 147 (by 288 and 289), and
item 253's "before any code" for the largest real unit (by 291).
