# On-tape volume format — Layout Version 3 (normative)

This is the authoritative specification of the bytes tapectl 2.0.0 writes to a
sealed cartridge. It is governed by ADR-0007 and by ADR-0012 items 239 to 293, which
it writes out; where this text and an item disagree, the item wins and this text is
corrected. It pairs with `docs/design/layout-session.md`, which owns the write
session's **state machine, transitions and operational sequence**; this file owns the
**static format, the isolation invariant, the integrity chain and the heir path**. A
fact that touches both is stated once, here or there, and referenced. CONTEXT.md's
vocabulary (Layout, Sealed, Heir Path, Escrow Recipient, Claim, Evidence, Slice,
Range, Stage set) is used without redefinition.

`layout_version = 3`. Layout 3 stands alone: tapectl 2.0.0 reads and writes Layout 3
only (ADR-0012 item 242). A tape of another layout is known by its File 0 label and
nothing else of it is read (§7.6). This document replaces
`docs/design/volume-format-v2.md` for Layout 3; that document describes the tapes
tapectl 1.x wrote and is history here.

Points that the ruled texts do not decide at the level of bytes are marked
**OPEN (Stage 1):** and listed in §10. Nothing is built on an open point until the
CTO answers it.

---

## 1. Zone order (single partition, fixed 512 KiB blocks)

A sealed Layout 3 tape is a single partition of `M+1` tape files, each terminated by a
filemark, in this fixed order:

```
┌──── FRONT: plaintext navigation (all known pre-write, no key needed) ─────────┐
│ File 0  ID thunk        plaintext   identity: label, uuid, layout_version,    │
│                                     requires, total_files, and the positions  │
│                                     of the front index (File 3) and the seal  │
│                                     marker; a [media] block describing the    │
│                                     cartridge it was written on (ADR-0010).   │
│ File 1  System guide    plaintext   heir manual (mt/dd/age/tar/sha256sum).    │
│ File 2  RESTORE.sh      plaintext   heir tool, one file, the same bytes on    │
│                                     every tape (D10).                         │
│ File 3  FRONT INDEX     plaintext   for EVERY file 0..M: position, type,      │
│                                     size_bytes; and sha256_encrypted for      │
│                                     every file EXCEPT File 3 and the seal     │
│                                     marker. ZERO content metadata.            │
├──── MIDDLE: encrypted decryption metadata (all known pre-write) ──────────────┤
│ File 4  Tenant envelope enc(t+op+esc) ┐ MANIFEST.toml (unit blocks, slice    │
│  ...    Tenant envelope enc(t+op+esc) │ lists, positions), RECOVERY.md,       │
│ File j   Operator env    enc(op+esc)  │ tables/ (file table and id map per    │
│ File j+1 Operator backup enc(op+esc)  │ staging), index/, PLAN.toml and       │
│ File j+2 Tenant backup  enc(t+op+esc) │ catalog.db (operator only). Order §2. │
│  ...    Tenant backup   enc(t+op+esc) ┘ Each backup is its primary's bytes.   │
├──── BULK: encrypted data (the mass of the tape) ──────────────────────────────┤
│ File k  Data slice      enc(t+op+esc) ┐ age(the slice: whole members of a     │
│  ...     Data slice     enc(t+op+esc) │ pax stream, §3; compressed alone      │
│ File M-1 Data slice     enc(t+op+esc) ┘ first when the staging says so).      │
├──── TAIL: the completeness assertion ─────────────────────────────────────────┤
│ File M   SEAL MARKER    plaintext   {layout_version, requires, file_count,    │
│                                     ended_after, sealed_at,                   │
│                                     front_index_sha256} + a full embedded     │
│                                     copy of the front index.                  │
└────────────────────────────────────────────────────────────────────────────────┘
    esc = the permanent Escrow Recipient (ADR-0005), in every encryption.
    op  = operator recipients.  t = the owning tenant's recipients.
```

**A tape may end early.** The positions above are the **plan**. When the drive reports
its early warning (CONTEXT.md "Early warning") in data slice K before the last one, the
writer finishes slice K and writes a **short seal** at K+1 instead of File M (§5.2).
Files K+2 to M are then not on the tape. File 0's `seal_marker` and `total_files`,
File 3, and every envelope still describe the plan; the seal says where the tape really
ended (`ended_after`). Every Layout 3 tape may end early; that is part of the layout and
needs no `requires` entry (§1.2).

**A unit may continue on another cartridge.** Every unit's stage set may have only a run
of its slices on a tape: a **range** (CONTEXT.md "Range"; ADR-0012 item 288). Its
`[[units]]` block in each envelope's `MANIFEST.toml` lists every slice of the staging
and gives a tape position only to the slices this tape plans, and the seal's
`ended_after` trims that run as it trims any plan (§5.3). One staging's slices on one
tape are contiguous and in slice order.

**Rationale for the order** (ADR-0007; `docs/research/2026-07-21-ontape-format-and-write-design.md`):

- **Front index (File 3)** puts the whole navigation map at BOP. An heir reads it with
  `mt rewind; mt fsf 3; dd`, with no spooling to end of data.
- **Envelopes before slices** means a truncated or tail-damaged tape loses only trailing
  *slices*; every envelope needed to navigate and decrypt what did land survives.
- **Every envelope has a backup copy** (§1.3), so one unreadable envelope never strands
  the slices it describes.
- **Seal marker last** is the only honest place for "complete": completeness can be
  asserted only after the last content file exists.

### 1.1 File 0 and its `[media]` table (normative)

File 0 is plaintext: a human-readable header that sends the reader to File 1 and names
File 3 as the map, then three TOML tables in this order:

```toml
[volume]
magic = "tapectl-volume-v3"
label = "{label}"
uuid = "{uuid}"
layout_version = 3
requires = []
tapectl_version = "{tapectl_version}"
media_type = "{media_type}"
nominal_capacity_bytes = {nominal_capacity}
mam_capacity_bytes = {mam_capacity}
created_at = "{now}"

[layout]
front_index = 3
seal_marker = {seal_marker}
total_files = {total_files}

[media]
cartridge_manufacturer = "{mam_manufacturer}"
cartridge_serial = "{mam_serial}"
cartridge_identity_source = "{mam|operator}"
tape_length_meters = {mam_length}
load_count_at_write = {mam_loads}
```

- `magic` is `tapectl-volume-v3` (D1). A reader that knows only Layout 2 refuses the tape
  at its magic before it reads `[layout]`, so a Layout 3 File 0 carries nothing a Layout 2
  script could follow (#562).
- **Every string in File 0 is a TOML basic string**, escaped as TOML escapes (`\\`, `\"`,
  and control characters), `[media]`'s strings included (#547).
- `[media]` is the last table and **must stay last**: RESTORE.sh's `toml_val` reader is
  table-blind and first-match-wins, so only a trailing table can gain a key without
  risking a name that shadows one an earlier table owns.

| `[media]` key | Meaning |
|---|---|
| `cartridge_manufacturer` | MAM manufacturer string at write time, `""` if unknown |
| `cartridge_serial` | the cartridge's identity, as `cartridge_identity_source` says |
| `cartridge_identity_source` | how `cartridge_serial` was established: `"mam"` or `"operator"` |
| `tape_length_meters` | MAM length, `0` if unknown |
| `load_count_at_write` | MAM load count at write time, `0` if unknown |

**`cartridge_identity_source` (ADR-0010, ADR-0012).** `"mam"`: `cartridge_serial` is the
serial the cartridge's own chip reported when this volume was bound to it, verifiable
against the medium. `"operator"`: no serial was readable when the volume was bound, the
operator named the cartridge (`volume init --cartridge <barcode>`), and
`cartridge_serial` carries that barcode, verifiable only against the catalog. The value
records how the identity was established at the binding, not what a drive reports at a
later contact. Every Layout 3 File 0 states the key.

### 1.2 `requires`, and keys a reader does not know (normative)

File 0's `[volume]` and the seal marker's `[seal]` state, on the line after
`layout_version`:

```toml
requires = []
```

`requires` lists the features a reader must understand to read this tape correctly. **On
every Layout 3 tape it is empty** (D1; ADR-0012 items 258(a) and 288). The short seal
and a unit that continues on another cartridge are part of Layout 3, not features of it.
The key stays, so that a later feature can fence an older Layout 3 reader. A reader that
knows Layout 3 knows no feature: its list of known features is empty.

**The spelling of a non-empty list is normative**, for a later feature: each feature
double-quoted, two features separated by a comma and one space, no space after `[` or
before `]`. RESTORE.sh reads the list by deleting brackets, quotes and commas and
splitting what is left at spaces.

The rules every reader keeps (tapectl's `format::FormatClaims`, at each File 0 contact of
`restore`, `catalog rebuild`, `volume verify`, `volume read-slices`, compaction's read,
`volume rewrite` and the adoption of a lost write, and in the chain walk on the seal;
RESTORE.sh's `check_magic` and `check_requires`):

1. **Refuse what you do not know, by name.** A tape whose File 0 or seal marker lists a
   feature the reader does not know, or states a `magic` other than `tapectl-volume-v3` or
   a `layout_version` other than 3, is refused before anything else of it is interpreted.
   The refusal names the feature, or the magic and layout it found, and the way to a
   reader that knows it: a newer tapectl, the tapectl that wrote it, or the RESTORE.sh at
   the tape's own File 2. A refusal is never a misread, and in the chain walk it is
   inconclusive, never evidence that the medium is bad. A tapectl volume of another
   layout is §7.6's.
2. **Absent `magic` or `layout_version`** (a damaged File 0) is warned about, not refused:
   the case an heir most needs RESTORE.sh for must never become a hard stop. The seal's
   own `layout_version` and `requires` then decide.
3. **Ignore keys you do not know, unless `requires` names them.** A later writer may add
   keys to any table; a reader that does not know one reads on. A writer that adds
   anything a Layout 3 reader would *misread*, not merely not know, lists a feature for it
   in `requires`. A change no `requires` can fence bumps `layout_version` and `magic`.
4. **A key's meaning never changes.** New meaning, new key.
5. **New keys in File 0 go in a trailing table** (§1.1), with one exception: `requires`
   sits in `[volume]`, because it must be read before anything else is trusted.
6. **The seal marker is the last file, found as such, and never by a block address.**
   The **planned position** is File 0's `[layout] seal_marker` (always `total_files - 1`),
   File 3's `seal_marker` entry, or, for tapectl, the catalog's Layout. A reader finds the
   seal:
   - **at the planned position**, when the tape holds a file there; or
   - **when the medium ends before it**, as the **last file before end of data**: file
     N-1, where N is the number of files the tape holds, counted by the filemarks spaced
     over from the beginning of the tape. A file with no filemark after it is no file: a
     seal counts only once its closing filemark is on the tape.

   Both routes space by filemarks, and they reach the same file. tapectl spaces to the
   planned position; when st reports END OF DATA first, it takes N from st's own file
   count (`MTIOCGET`'s `mt_fileno`) and reads file N-1. RESTORE.sh spaces to end of data
   and back over two filemarks (`mt eod`, `mt bsfm 2`; GNU mt spells end of data `eom`),
   and falls back to File 0's pointer only when mt cannot space to end of data. **No
   reader finds the seal, or any file, by READ POSITION or LOCATE**: READ POSITION never
   answered through st on the production host's HP LTO-6
   (`docs/runs/2026-10-20-lto6-profile-and-lbp.md`,
   `docs/runs/2026-10-28-1.8.0-lbp-acceptance.md`). The one READ POSITION tapectl sends,
   after a block the drive refused with Logical Block Protection armed (ADR-0012 items
   174 and 185), goes to the sg node in the short form; no reader uses it to find a file.

   Which seal a reader accepts:
   - **At the planned position:** a seal whose `file_count` is the planned count (full,
     §5).
   - **Before it:** a seal that keeps §5's parse rules and states `file_count = N` (short,
     §5). It is the normal short ending and draws no warning. Anything else is no seal:
     tapectl reads the tape as unsealed; RESTORE.sh uses the last file and warns that it
     disagrees with File 0.
   - **After it:** never accepted by tapectl. RESTORE.sh warns.

   A read at or before the planned position that fails without st reporting END OF DATA
   at or before it is **unreadable, not absent** (#460): tapectl never reads it as
   "unsealed", and a fresh write refuses the cartridge.

   tapectl's readers that know a volume's `ended_after` (`volumes.ended_after`,
   `layout-session.md`) read its seal at `ended_after + 1` directly, and check that the
   seal states the same `ended_after` (§6).

   **The write path's contact check keeps the same rule.** When the planned position is
   at or past end of data, the contact check reads the last file on the medium before it
   concludes there is no seal, and a seal marker there refuses the write as any seal does
   (ADR-0003).

Unknown front-index `type`s are not refused: a future file type arrives with a
`requires`, which is refused first.

### 1.3 Envelope backup copies (normative)

Every envelope on a Layout 3 tape is on it twice (ADR-0012 item 44).

- **What a backup is.** The primary's exact bytes, copied: the same ciphertext, so the
  same `size_bytes` and the same `sha256_encrypted` in the front index. It is not a
  second encryption (age is randomized per call, and a second ciphertext would be an
  unrelated file, not a copy that can be checked against the first).
- **Where it sits.** The middle zone holds, in order: the tenant envelopes (permuted,
  §2), the operator envelope, the operator envelope's backup, and then one
  `tenant_envelope_backup` per tenant **in the same order as the primaries**. Every
  primary comes before every backup, and each tenant's backup is separated from its
  primary by the operator pair. The first data slice follows the last tenant backup.
- **How it is indexed.** Its own front-index `type`, `tenant_envelope_backup` or
  `operator_envelope_backup`, labelled only by kind like every envelope (§2). A reader
  pairs a backup with its primary by the equal `sha256_encrypted`, never by position
  arithmetic.
- **How readers use it.** A reader opens the primary first, and turns to the backup only
  when the primary does not read, does not match the front index's hash, or is damaged
  under a key that matched it. A backup whose primary opened is skipped. Confirm and
  verify treat a backup like any other file (§6).

### 1.4 The operator envelope's `catalog.db` (normative)

The operator envelope and its backup carry, beside `MANIFEST.toml`, `RECOVERY.md`,
`PLAN.toml`, `tables/` and `index/`, a tar member `catalog.db` (#83): a standalone SQLite
database holding what the catalog knew, at staging time, about **this write's stagings
only**, every tenant's, so it is never in a tenant envelope. `catalog rebuild
--from-volume` reads it (`db::ontape_catalog`), and `RECOVERY.md` shows an operator with
only `sqlite3` how to query it. Tape positions are not in it: `MANIFEST.toml` has them.

**One generation** (D7; ADR-0012 item 258(d)). Its tables are the baseline schema's
(D9, Appendix A) for the rows this write's stagings need: the tenants, units, snapshots
and versions, the stage sets with their slices and recorded recipient lists, the paths
and file versions, and, **for each staging the tape holds, its file table
(`stage_files`) and its id map (`stage_ids`) as the catalog database holds them**
(ADR-0012 items 265, 276; #557, #564). So a rebuild restores both, and a copy written
after a rebuild carries both. `catalog rebuild` takes the file table and the id map from
this file and from nowhere else (T4.9, determined).

**OPEN (Stage 1):** which baseline tables and columns `catalog.db` carries, by name
(T1.5's listing decides the baseline; this file's subset is not named in the ruled
texts). Options: (a) the same tables as the 1.x generation-3 file (`paths`,
`file_versions`, `tenants`, `units`, `snapshots`, `stage_sets`, `stage_slices`) plus
`stage_files` and `stage_ids`; (b) a subset named table by table in Appendix A.

A reader refuses, naming the row, a value outside a column's allowed set; it does not
guess.

### 1.5 The cross-volume index (normative)

Each envelope carries an **index of the archive as the catalog knew it when this tape's
Layout was built** (ADR-0012 items 78-80): which volumes hold which version of each
unit, and, for a tenant, every path.

| Envelope (and its byte-copied backup, §1.3) | Members |
|---|---|
| each tenant envelope | `index/units.tsv` (that tenant's units only) and `index/paths.tsv` |
| the operator envelope | `index/units.tsv` (every tenant's units, with a `tenant` column); no `paths.tsv` |

The members are the last in the envelope's tar. A tenant learns the labels of the tapes
holding its own units and nothing of another tenant's (§2); the operator and escrow keys
see the cross-tenant map.

**Encoding.** UTF-8 text. The first line is a header naming the columns; every further
line is one row; fields are separated by one TAB and rows end with one LF. Inside a
field, four characters are escaped and nothing else: backslash as `\\`, TAB as `\t`, LF
as `\n`, CR as `\r`. An empty field means "not known". A reader finds columns by their
header name and ignores a column it does not know. Rows are sorted as given below, so
the bytes are deterministic for a given catalog. **This escaping is the one rule for the
whole format** (`volume::index::escape`): the file table, the id map and the
damaged-file list use it too.

**`index/units.tsv`**: one row per (version, holding volume). Sorted by `tenant`
(operator only), `unit`, `version`, then `volume`.

| Column | Meaning |
|---|---|
| `tenant` | the tenant's name (operator envelope only) |
| `unit` | the unit's name |
| `unit_uuid` | the unit's uuid |
| `version` | the version's ordinal (CONTEXT.md "Version") |
| `num_slices` | the number of slices of this version's stage set |
| `first_slice`, `last_slice` | the run of those slices this volume holds: `1` and `num_slices` for a whole version, or the first and last slice of a range, one unbroken run in slice order. On the `this-tape` row they are the run this tape's plan holds, which the seal's `ended_after` may end sooner |
| `version_bytes` | the stage set's ciphertext bytes, all slices |
| `volume` | the holding volume's label |
| `volume_uuid` | its uuid (File 0's `uuid`) |
| `cartridge_serial` | the chip serial of the cartridge it was written on; empty when none was read |
| `generation` | the medium's LTO generation (File 0's `media_type`) |
| `state` | `sealed`, `quarantined` (sealed, but a verify proved the medium bad; it may still partly read), or `this-tape` |
| `sealed_at` | when that volume sealed (RFC 3339, UTC); empty for `this-tape` |

**Which versions and holders** (item 80): every current version with at least one
`completed` or `range` write on a sealed, unretired volume, plus every version this write
carries. Its holders are the sealed, unretired volumes holding a `completed` or `range`
write of it, quarantined ones included and marked, and this tape. A version becomes
current only once every slice of a stage set of it is on a sealed volume (item 128,
`layout-session.md`); until then it is listed only on a tape that carries it, with its
earlier holders and their runs. A holder of a range holds only its run, and no row is a
copy on its own (item 127). `units.tsv` names no stage set, so a reader never joins runs
by the index, and takes a slice from another tape only by its hash (§5.3 R8). Where a
cartridge is kept is never on a tape (§2).

**`index/paths.tsv`** (tenant envelopes only, item 79): one row per path of each version
listed in that tenant's `units.tsv`. Sorted by `unit`, `version`, then `path` (bytewise).

| Column | Meaning |
|---|---|
| `unit` | the unit's name |
| `version` | the version's ordinal |
| `kind` | `d` directory, `f` regular file, `l` symlink, `s` special (FIFO, device) |
| `size_bytes` | lstat's size; 0 for a directory |
| `sha256` | the file's content hash as staging recorded it, 64 lowercase hex characters; empty for a directory, a symlink, a special file, where none was recorded, and **for the unit's own dotfile** (D8, #528) |
| `path` | relative to the unit's root, escaped as above |

**Staleness.** An index is as of its own tape's write: it never names a tape written
after it, and a holder it names may since have been retired or erased. The newest tape's
index is the most complete; RESTORE.sh and File 1 say so.

**Who reads it.** RESTORE.sh's `--where UNIT`, `--where --all` and `--find TEXT` (§7.4);
tapectl's `catalog rebuild --from-volume`, which reads the operator envelope's
`units.tsv` as a to-do list of tapes the catalog lacks and never creates a row from it
(item 78; ADR-0001). The per-file check after a restore reads this tape's file table
(§1.6), not `paths.tsv` (item 254, #528).

### 1.6 The file table and the id map (normative)

Each tenant envelope carries, **for each staging the tape's plan holds a slice of**, that
staging's whole file table and id map as text (ADR-0012 items 254, 265, 276; D4, D5).
Every tape whose plan holds a slice of a staging carries that staging's whole table,
whichever slices it holds (#562). The catalog database holds the same rows
(`stage_files`, `stage_ids`, Appendix A), and the operator envelope's `catalog.db`
carries them (§1.4).

**Names.** `tables/<staging>.tsv` (the file table) and `tables/<staging>.ids.tsv` (the id
map), where `<staging>` names a staging as the dar catalogue's member name named it in
Layout 2: the unit's uuid's first eight hex characters, `_v`, and the version
(`tables/1a2b3c4d_v3.tsv`).

**OPEN (Stage 1):** two stagings of one version on one tape (ADR-0012 item 102 allows a
re-stage of a version; item 138 speaks of "a version with two stage sets on one volume")
would share `<staging>`. Options: (a) add the stage set id, `<uuid8>_v<version>_s<stage_set_id>`;
(b) refuse to plan two stagings of one version on one tape.

#### The file table

UTF-8 text, the index's escaping (§1.5). Two header lines, then one tab-separated row for
each member of the staging's tar stream, **in stream order**:

```
#tapectl-file-table 1
#stream_bytes <length of the stream in bytes>
<kind>	<slice>	<start>	<end>	<size>	<sha256>	<path>	[<link target>]
```

- `kind`: `f` regular file, `d` folder, `l` symbolic link, `h` hard link, `p` FIFO, `c`
  character device, `b` block device. A socket is never a member (item 270; §3.1).
- `slice`: the number of the slice in which the member begins (item 283). A member larger
  than `slice_size` also holds the slices after it that its length needs: its **run**
  (§3.3). A slice's own offset in the stream is the `start` of the first member that
  begins in it.
- `start`: the offset in the stream of the member's first extension header. `end`: the
  offset just past the member's last padded block. A row's `start` is the row before's
  `end`; the first is 0 (#562).
- `size`: the file's bytes; 0 for a kind with no data.
- `sha256`: of the file's bytes as they went to tape, 64 lowercase hex characters, for
  `f`; the target's, for `h`; `-` for the others.
- `path`: the member's path relative to the unit's root, as the file list gave it to tar
  without its leading `./`. A reader that names a member to tar spells it with the
  leading `./`, as the stream has it (T0.7).
- `link target`: for `h` only, the path of the name archived first, escaped the same
  way. A symbolic link's target is in the stream, not in the table.

**Rows in stream order; a folder is two ranges** (#562): a folder's own member comes
where the sorted list put it, and its contents follow later members; restoring a folder
whole takes its member and its contents' members. The stream's tail (tar's two zero
blocks and the record padding after them) is no row: it lies between the last row's
`end` and `stream_bytes`.

**What "this tape holds F" means** (#562): every slice from the one in which F begins
(`slice`) to the one in which it ends is on this tape at or before `ended_after`. The
table is part of what the seal withdraws: a row whose slices lie after `ended_after`
describes a member that is not on this tape (§5.1).

#### The id map

UTF-8 text, the index's escaping. One header line, then rows sorted by kind (`u` before
`g`) and then by number:

```
#tapectl-id-map 1
u	<id>	<name>
g	<id>	<name>
```

It holds, for the staging, every user id and group id its stream carries, as a member's
owner or group and in an ACL entry, with the name the host gave that id when the stream
was written, or an empty name when the host knew none (ADR-0012 item 277). An id the
stream does not carry has no row. A tenant envelope's map names only that tenant's
stagings' ids. It is in no plaintext file of the tape (item 276). **A restore shows the
map and never acts on it**: a restore is by number, and no remap by name is built (item
278).

**OPEN (Stage 1):** the id map's header line. D5 gives the rows and no header; the file
table's header is `#tapectl-file-table 1`. Options: (a) `#tapectl-id-map 1`, as written
above; (b) no header line.

### 1.7 The damaged-file list (written by readers, not on the tape)

`.tapectl-damaged-files.txt` is written by RESTORE.sh, and by tapectl's restore, in a
unit's restored directory (`--to` for one unit, `--to/UNIT` for several), for a restore
that left a member out or whose files failed their recorded sha256. Line 1:
`#tapectl-damaged-files`, TAB, `1` (the format), TAB, `unit=` and the unit name, TAB,
`uuid=` and the unit uuid, TAB, `version=` and the version. Every further line is one
path relative to the unit's root, escaped as §1.5 escapes. **It is the heir's one
repair** (ADR-0012 items 255, 264): given to `--paths-from` on another copy of the same
unit and version, it restores only those paths, overwriting only them, and the list is
rewritten with the paths still damaged, or removed. Another version or another unit is
refused. A range is a source for the files whose slices it holds (item 260).

### 1.8 What every Layout 3 tape declares up front (normative)

Everything before the first data slice is written before the writer can know whether
the tape will end early, so every Layout 3 tape declares that it **may**, in every file
that describes the plan, and only the seal says whether it **did** (§5).

| File | Declaration | Where |
|---|---|---|
| File 3 | `may_end_early = true`, and a header paragraph | `[index]`, the line after `layout_version` (§4) |
| each envelope's `MANIFEST.toml` | `may_end_early = true` | `[manifest]`, the line after `created_at` (§1.9) |
| each envelope's `RECOVERY.md` | a section on reading the seal | text (§7.5) |
| File 1 | the same, for the heir | text (§7.5) |
| the seal | `ended_after` | `[seal]` (§5) |

(T4.6, determined: `may_end_early = true` stays in File 3's `[index]` and in every
`[manifest]`, on every Layout 3 tape.) **File 0 is the plan**: its `seal_marker` and
`total_files` are the planned seal position M and the planned count M+1 on every tape. On
a short tape they name a position past end of data; a reader treats them as the plan,
never as damage (§1.2 rule 6). `may_end_early` decides nothing: the seal's `ended_after`
does.

### 1.9 The envelope's `MANIFEST.toml` (normative)

```toml
[manifest]
volume = "{label}"
tenant = "{tenant}"
created_at = "{created_at}"
may_end_early = true

[[units]]
name = "{unit}"
uuid = "{unit_uuid}"
snapshot_version = {version}
stage_set_id = {stage_set_id}
payload = "tar"
stream_bytes = {stream_bytes}
compression = "{gzip|xz|zstd}"      # only for a compressed staging
tar_version = "{the writer's tar --version first line}"

[[units.slices]]
number = 1
tape_position = {position}           # only for a slice this tape plans
continues = true                     # only for a slice that continues a member
size_bytes = {size_bytes}
encrypted_bytes = {encrypted_bytes}
sha256_plain = "{sha256_plain}"
sha256_encrypted = "{sha256_encrypted}"
```

The `#` notes above are for this document; they are not in the file.

- **The block of a staging** (D6). Kept: `name`, `uuid`, `snapshot_version`,
  `stage_set_id`, and the slice list. New: `payload = "tar"`; `stream_bytes`, the length
  of the staging's tar stream; `compression`, named only when the staging is compressed
  (§3.5); the writer's `tar --version` first line, for a reader's diagnosis only; and the
  file table's and id map's names in the envelope.
- **The slice list lists every slice of the staging**, 1 to N, in slice order, each with
  `size_bytes` (the plaintext length of the slice's cut, before compression, §3.4),
  `encrypted_bytes` (the tape file's length), `sha256_plain` (of the cut) and
  `sha256_encrypted` (of the tape file). So any tape that holds a slice of the staging
  gives every slice's ciphertext hash, and a reader can accept another tape by hash
  (§5.3 R8), and a rebuild records every slice.
- **`tape_position` is given only to the slices this tape's plan holds**, as each slice's
  second key. For every other slice the line is absent: never `0` and never empty. The
  slices given a position are one unbroken run in slice order, at consecutive tape
  positions.
- **`continues = true`** is stated on a slice that continues a member begun in the slice
  before it (D22): the second and later slices of a long member's run (§3.3). A slice
  without it begins at a member's start. No plaintext file says which slices are a run.
- `manifest.rs` is one type in both directions and refuses a key it does not know in
  `[[units]]` and `[[units.slices]]` (`deny_unknown_fields`); `RESTORE.sh` reads the
  keys it needs and ignores the rest.

**OPEN (Stage 1):** the key names of D6's "the table's and the id map's names in the
envelope" and of the writer's `tar --version` line. Options for the names: (a) none (a
reader derives `tables/<staging>.tsv` and `.ids.tsv` from `uuid` and
`snapshot_version`, §1.6); (b) `file_table = "tables/…tsv"` and `id_map =
"tables/…ids.tsv"`. For the version line: `tar_version` (as written above) or
`writer_tar`.

**OPEN (Stage 1):** the order of the new keys in `[[units]]` and of `continues` in
`[[units.slices]]`. RESTORE.sh's `toml_val` is table-blind and first-match-wins; none of
the new names collides with another key of the manifest. Options: the order written
above, or `continues` after `sha256_encrypted`.

---

## 2. Plaintext vs encrypted, and the isolation invariant

| Zone | On tape | Why |
|---|---|---|
| ID thunk, system guide, RESTORE.sh | plaintext | heir reads them with no key |
| **Front index (File 3)** | **plaintext** | navigation must work with no key; carries position, type, size and ciphertext hashes only |
| Tenant and operator envelopes, data slices | age-encrypted | *all* content metadata (file names, content sizes, `sha256_plain`, the file tables and id maps, the packing plan, the cross-volume index) lives here and nowhere else |
| **Seal marker (File M)** | **plaintext** | structural completeness assertion; its `front_index_sha256` is a hash of a plaintext file |

**Isolation invariant (normative).** No plaintext file on the tape may reveal file
names, tenant or unit names, plaintext-content hashes (`sha256_plain`), key
fingerprints, or a user or group id or name (invariant 3 of the plan). Permitted in
plaintext: per-file **on-tape byte sizes** and per-file **ciphertext hashes**
(`sha256_encrypted`): structural facts, not attributable to any tenant (no plaintext
file carries a tenant identity; a `data_slice` or `tenant_envelope` entry is labelled
only by kind) and computable by anyone holding the tape. The plaintext hashes exist
**solely to verify tape integrity** (the keyless chain, §5, §6) and are never content
claims.

**Accepted disclosures** (operator ruling 2026-07-22; ADR-0012 items 283 and 284).
- An on-tape size approximates the content size it encloses. At the grain of a unit this
  was accepted on 2026-07-22 (quantized padding was considered and declined).
- **A slice's size on tape follows the files in it** (item 283). A file larger than
  `slice_size` shows its size as the length of its run. A smaller file shows its size
  when it is alone in its slice. Otherwise a slice shows the total of a group of files.
  This is finer than the per-unit signal and is accepted.
- **A compressed slice's size on tape follows how well it packed** (item 284), and is
  accepted.
- File 1's disclosure section states these inferences plainly.

**Envelope ordering.** Because the front index publishes a ciphertext hash per position,
envelope order must not become a side channel that re-attaches a hash to a tenant. The
write path orders the tenant envelopes by a **deterministic permutation seeded by
`volume_uuid`**: deterministic so Layout construction stays reproducible
(`layout-session.md`), seeded so the order is not the raw `tenant_id` sequence. The
tenant backups (§1.3) repeat the primaries' permuted order, and each carries its
primary's hash, so a backup is linkable to its primary and to nothing else.

**The ciphertext-hash relaxation** (decision D4 of 2026-07-21) stands: a hash of age
ciphertext is a hash of pseudorandom AEAD output, reveals nothing about plaintext, is not
attributable to a tenant, and is recomputable by anyone with the tape. `sha256_plain` stays
encrypted-only.

**The invariant is machine-checked.** The plaintext leak scan (`tests/tenant_isolation.rs`,
`tests/format_v3.rs`, and the mhvtl gate's scan) writes a volume with sentinel tenant, unit
and file names, a user name, a group name and an id the test unit alone carries, and scans
every plaintext file (positions `{0, 1, 2, 3}` and the seal, at `ended_after + 1` on a
short tape) for all of them and for the expected `sha256_plain` values, asserting all are
absent. It permits `sha256_encrypted` and on-tape sizes. The needles are never weakened.

**Self-describing: what that promises (normative).** A sealed volume promises two things
and withholds a third:

1. **Data.** Every unit it holds is restorable by its tenant with that tenant's key and
   standard tools (`mt`, `dd`, `age`, `tar`, `sha256sum` and a few coreutils, plus the
   compression tool a compressed staging names, §7) with no tapectl and no database
   (RESTORE.sh, File 2). "Every unit it holds" is every unit whose every slice is at or
   before the seal's `ended_after`. A tape that holds a range of a unit gives, read alone,
   every file whose slices it holds (item 266); the unit whole needs the tapes that hold
   its other slices.
2. **Catalog.** With the operator or escrow key, `catalog rebuild --from-volume`
   reconstructs everything the catalog knew about this write at staging time: volume
   identity, units, snapshots, the slice map and plaintext hashes, tenant ownership, each
   stage set's recorded recipient list, the per-file index, the file tables and the id
   maps (§1.4). A rebuild from one tape of a span records what that tape shows: every
   slice of the staging, and positions only for its own.
3. **Never on the tape:** where the cartridge is, whether it has been verified and when,
   what policy applies to a unit, warehouse deposits, the cleaning-cartridge registry,
   each write's feed ratio, and any fact the catalog knows only because the operator did
   something after the write. Those survive a lost database only through `db backup` and
   the Heir Kit (ADR-0009).

---

## 3. The data zone: the stream, its checks, the cut, a run, a slice on tape, compression

A unit's version is archived as **one pax stream per staging**, written by GNU tar on the
host from tapectl's exact file list (ADR-0012 item 253). tapectl reads the stream as it
passes, checks it, cuts it into **slices** that each hold whole members (item 283), and
age-encrypts each slice alone, after compressing it alone when the staging is compressed
(item 284). No dar is anywhere in the data zone (item 241).

### 3.1 The stream (D2)

**The writer.** GNU tar, at or above **the writer's floor: GNU tar 1.35 built with ACL
support** (item 274). tapectl refuses to stage with a GNU tar below the floor and names
it; the version is only the early refusal, and the proof, on any version, is tapectl's own
check of the stream (§3.2). The floor binds every host that writes (item 274); a host's
tar is built from the upstream release where its system's is older (item 275; the recipe
is `docs/research/2026-11-17-t02-gnu-tar-built-on-debian-10.md`). The heir's readers are
not held to the floor (§7.3).

**The command line**, fixed flag by flag:

```
tar --create --file=- --format=pax \
    --numeric-owner --xattrs --acls --xattrs-exclude='system.posix_acl_*' \
    --no-recursion --null --verbatim-files-from --files-from=<list> \
    --pax-option=exthdr.name=%d/PaxHeaders/%f,delete=atime,delete=ctime
```

- `--format=pax`: the stream is POSIX pax (item 253).
- `--numeric-owner` (item 270): a member's owner and group are stored by number only; the
  header's user and group name fields are empty.
- `--xattrs --acls --xattrs-exclude='system.posix_acl_*'`: extended attributes are kept,
  and an ACL is stored **once**, in the text record (`SCHILY.acl.access`,
  `SCHILY.acl.default`), with every qualifier a number (items 273, 274; measured by T0.1,
  `docs/research/2026-11-17-t01-readers-of-an-acl-stored-once.md`: GNU tar 1.30, 1.34,
  1.35 and bsdtar restore that record with their ordinary flags, and none restores the
  raw `system.posix_acl_*` record with them). The raw record is not stored.
- `--no-recursion --null --verbatim-files-from --files-from=<list>`: tar archives exactly
  the list, and a name that looks like an option is a member, never acted on (#562).
- `--pax-option=exthdr.name=%d/PaxHeaders/%f` is fixed, so the stream's extended-header
  names do not depend on tar's process id (#562). Access and change times are not stored.
- No `--sparse`: a sparse file is stored as its bytes. No compression by tar.

**The list** is tapectl's: the snapshot's own entries, NUL-separated, every name prefixed
`./`, sorted bytewise, folders listed as their own entries, the unit's dotfile included.

**The environment.** tar runs in the unit's root, with `LC_ALL=C`, and with `TAR_OPTIONS`
and `POSIXLY_CORRECT` removed from its environment. If `--directory` is used in place of
the working directory it comes **before** `--files-from`: tar takes it in order, and after
the list it reads the names against the wrong directory and exits 2 (measured).

**What the snapshot leaves out** (D14; items 252, 270). tapectl's matcher is the only one:
tar is given the exact list. A snapshot records and reports what each exclusion pattern
left out, and `init` writes an empty list. **Sockets are left out of every unit** and
reported with the reason "socket": tar skips a socket at exit 0, so a listed socket would
have no member. FIFOs and device nodes are kept.

**Hard links are kept as links** (item 256). The list holds every name; the stream holds
the first name with its data and each later name as a link member. A later name depends on
the slice of the name archived first (items 241, 293).

**No pin or fixture holds data-zone bytes across tar versions** (#562): the streams of two
tar versions differ in bytes. Pins cover the plaintext files and the envelopes' texts
(T6.3), never a slice.

### 3.2 The stream's checks (D12; invariants 5 and 7)

tapectl reads the stream as it passes and changes none of its bytes. **A member starts at
its first extension header**, and its size comes from the pax `size` record when there is
one (#562). For each member, as it passes:

- it is the next entry of the list, by path, kind and size (#562);
- its header's user and group name fields are empty, and it has no `uname` or `gname` pax
  record;
- every qualifier in its ACL record is a number;
- a listed file that had an ACL on disk before tar ran (the `system.posix_acl_access` or
  `system.posix_acl_default` attribute was present) carries the text ACL record;
- its data is hashed (sha256) for a regular file, and every user and group id it carries,
  as owner, group or ACL qualifier, is collected for the id map.

The stream is refused, naming the path and the rule, on: a member that is not the next
entry of the list; a name in a header or a record; a non-numeric ACL qualifier; a lost
ACL; a global extended header; a GNU long-name member; a header whose checksum is wrong; a
type the reader does not know; anything but zeros after the end-of-archive blocks; a stream
that ends inside a member; a listed entry never seen.

After tar exits: **status 0** (tar's exit 1 or 2 refuses the staging, #562); every listed
entry was seen; and each file's device, inode, size, mtime and ctime are what they were
before tar ran. A file changed under tar refuses the staging, whatever tar says
(invariant 7). **A stream hash that differs from the version's recorded sha256 refuses the
staging, for every path but the unit's dotfile** (#562).

The checks precede compression: every check of the stream is made on the stream's own
bytes, and a member's offsets in the file table are of the stream (rule 11).

### 3.3 The cut, and a long member's run (D3; item 283)

The cutter is given each member's `start` and whole length (its extension headers, its
header and its padded data) before any of the member's bytes. It places members in stream
order:

- a member that fits in what is left of the slice goes into it;
- a member that does not fit, and is no larger than `slice_size`, ends the slice before it
  and begins the next;
- a member larger than `slice_size` ends the slice before it and is cut at every
  `slice_size` bytes, its last piece ending where the member ends. Those slices are its
  **run**, and the member after it begins a new slice;
- the stream's tail (tar's end-of-archive blocks and its padding) is placed as a member
  is.

**A slice begins at a member's start, or it continues exactly one member that is larger
than `slice_size`.** No slice begins anywhere else, and the slices joined in order are the
stream, byte for byte (rule 13). A member of exactly `slice_size` fits in an empty slice;
one block more is a run.

`slice_size` is the most a slice's cut holds. It is a multiple of 512 wherever it is set
(`[defaults]`, an archive set, a unit's dotfile; #562). A slice may be shorter than
`slice_size`; the number of slices is at most three for each `slice_size` of stream (a
small file, then a file just over a slice).

What a slice is, measured (T0.7, `docs/research/2026-11-17-t07-readers-on-member-start-slices.md`;
item 293): a slice that does not continue a member, read alone, is a run of whole members
that GNU tar 1.30, 1.34 and 1.35, bsdtar 3.6.2 and BusyBox tar 1.36.1 each read at exit 0,
except that a hard-linked name whose first name lies in another slice is not made (its
first name is not there). A long member's run, joined, is read alone the same way. The
good slices of a staging joined in order, with a bad one left out, are read in one tar run
at exit 0, losing exactly the bad slice's members.

### 3.4 A slice on tape

Each slice is, in order: the cut (§3.3); compressed alone, as one frame, when the staging
is compressed (§3.5); then age-encrypted alone, to the tenant's, the operator's and the
escrow recipients (§1). tapectl does all three in memory; the staging directory holds only
ciphertext (#370). One slice is one tape file.

| Field | Of what |
|---|---|
| `size_bytes`, `sha256_plain` | the cut, before compression |
| `encrypted_bytes`, `sha256_encrypted` | the tape file |
| `continues` | whether the slice continues a member begun in the slice before it (§1.9) |

The front index's `size_bytes` for a data slice is the tape file's (`encrypted_bytes`).

**The bound the early warning's rules are held to.** A compressed slice can be larger on
tape than its cut (§3.5). W8 (§5.2) holds `slice_size` plus the staging's algorithm's
worst growth, plus age's overhead, to half the smallest post-warning room among the
configured drives. A slice whose on-tape size would pass that bound refuses the staging by
name; it is not stored uncompressed in place, because a staging has one algorithm.

**Staging's space estimate** (T4.5, determined). Before tar runs, the space a staging
needs is estimated as the files' bytes plus, for each entry, 2,560 bytes, plus twice the
length of its path in bytes rounded up to 512, plus 4,096 for an entry that has an ACL or
an extended attribute, plus 1,024 for each slice the estimate comes to (measured: about
1,951 bytes of stream for a small file). It never counts on compression to make room.
The space accounting while the stage runs still catches a stream that outgrows it.

### 3.5 Compression (D15; items 280, 284 to 287)

Compression is offered, optionally, for select archives (item 280): `compression` is set
in `[defaults]`, an archive set or a unit's dotfile, the most specific winning, and is
`none` when none is set. Its values are `none`, `gzip`, `xz` and `zstd` (item 285).
`compression_level` sits beside it in the same three layers (item 287).

- **Each slice is compressed alone, after the cut and before it is encrypted** (item 284).
  The stream is never compressed as one whole: a lost slice costs only the files in it.
- **tapectl itself compresses**, with a library for each algorithm built into the binary
  (item 286; T0.6 accepted `flate2`, `liblzma` and `zstd` in the static build,
  `docs/research/2026-11-17-t06-compression-libraries-in-the-static-build.md`).
- **The frame** (D15, determined): one complete stream of the algorithm for each slice, as
  the algorithm's own tool writes and reads it, so that `age -d`, then the tool, gives the
  cut. gzip: one member, with no file name and a zero time in its header. xz: one `.xz`
  stream with the CRC64 check. zstd: one frame with its content checksum on and the content
  size stated. tapectl adds nothing around the frame. A slice is compressed on one thread.
- **The level** is a whole number in the algorithm's own range (gzip 1 to 9, xz 0 to 9,
  zstd 1 to 19). When it is not set the algorithm's own default applies (gzip 6, xz 6,
  zstd 3). A level outside the range is refused, naming the key. The level is recorded
  with the staging in the catalog; no reader needs it, and the MANIFEST does not state it.
- **The MANIFEST names the algorithm** (`compression`, §1.9) for a compressed staging only.
- **The worst growth.** Measured by T0.5 on bytes that do not shrink (random bytes and
  real images, 4 MiB and 1 GiB slices;
  `docs/research/2026-11-17-t05-decompressors-on-a-slice-compressed-alone.md`): gzip
  +0.0162%, xz +0.0065%, zstd +0.0026% of the slice, with a fixed part of a few dozen
  bytes. The format states the worst growth of each algorithm, with a margin over that
  measurement, as **0.1% of `slice_size` plus 64 KiB**, for each of gzip, xz and zstd.
- **The check at staging.** Staging decodes every slice it has just packed and compares
  the sha256 of the decoded bytes with the cut's `sha256_plain`, both taken as the bytes
  pass, before the bytes go on; a difference refuses the staging. Item 253's "each file's
  sha256 from the bytes that go to tape" holds through it.
- **A decompressor's exit status is never the verdict** (invariant 6): BusyBox `unxz`
  exits 0 after writing wrong bytes, and on a slice whose first age chunk is damaged it
  exits 0 on empty input (T0.5). A slice's decoded length against `size_bytes`, its
  `sha256_plain`, and then the per-file check, decide.
- **What an heir needs.** A unit that states `compression` needs that algorithm's tool:
  `gzip`, `xz` (Debian's `xz-utils`) or `zstd`. A stock Debian 10 install holds only gzip;
  xz and zstd are its packages (T0.5). BusyBox reads gzip and xz and has no zstd. An heir
  restoring an uncompressed unit is never asked for one.

---

## 4. The front index (File 3)

Plaintext TOML. File 3 is the **plan**: it is written before any slice, so it lists every
file the write meant to put on the tape, whether or not the tape ends early (§5).

**The `[index]` table**, on every Layout 3 tape:

```toml
[index]
volume = "{label}"
layout_version = 3
may_end_early = true
```

- `may_end_early = true` is the literal line, directly after `layout_version` (§1.8). It
  declares that the tape may end before the last file listed, and that the seal says
  where. It decides nothing for a reader: the seal's `ended_after` does (§5). File 3
  carries no `requires`.
- File 3's human-readable header has, after the paragraph that ends "see the system
  guide." and before the `MACHINE-READABLE DATA` rule, exactly:

  ```
  This tape may end early. The list below is the plan: every file the
  write meant to put on the tape. The seal marker, the LAST file on the
  tape, says where the tape really ended (its ended_after). A file listed
  after that position is not on this tape.
  ```

Then one `[[files]]` entry per planned tape file, in position order:

- `position`: tape file number (0..M). Present for **every** file: the heir can jump
  straight to any file, the seal marker included. On a short tape, positions after the
  seal's `ended_after` are planned files that are not on the tape, and the `seal_marker`
  entry at M is the planned seal (§5.1).
- `type`: `id_thunk`, `system_guide`, `restore_sh`, `front_index`, `tenant_envelope`,
  `operator_envelope`, `operator_envelope_backup`, `tenant_envelope_backup`, `data_slice`,
  `seal_marker`. **No** tenant or unit identity: a `data_slice`, a `tenant_envelope` and a
  `tenant_envelope_backup` entry are unlabelled beyond kind. A slice that continues a
  member is a `data_slice` like any other (§1.9).
- `size_bytes`: the true (pre-block-padding) byte length, so the heir can
  `head -c $size_bytes` to trim the 512 KiB block padding before decrypting. Present for
  every file **except File 3 itself and the seal marker**: File 3's length is
  self-referential, and listing the seal's size here while the seal's embedded copy lists
  File 3's would make each depend on the other's digit count. Both tail files are
  plaintext, filemark-delimited, and recovered by reading the whole tape file and
  stripping trailing zero padding.
- `sha256_encrypted`: hex sha256 of the file's **on-tape bytes** (ciphertext for encrypted
  zones; the plaintext bytes for Files 0, 1 and 2). Present for every file **except File 3
  itself and the seal marker**. For data slices this value is taken verbatim from
  `stage_slices.sha256_encrypted`, computed once at staging; it is not recomputed while
  building the index.

Because File 3 carries content hashes, every envelope and slice is fully materialized and
its `sha256_encrypted` known **before** File 3 is generated: envelopes are encrypted up
front, and slice ciphertext hashes exist from staging.

---

## 5. The seal marker and the integrity chain

Plaintext TOML, one tape file, written last: a `[seal]` table, then a **full embedded copy
of the front index** (`[[files]]` entries in File 3's grammar). Its **presence** is the
assertion "every file before me is present and this is a sealed volume"; its **absence**
means the tape is unsealed (interrupted or aborted: `layout-session.md`). The seal may
come early: a **short seal** at K+1, whose embedded copy stops at itself. Everything this
section says of the seal holds of a short one, over the files it covers.

**The two kinds.** Every seal states `ended_after`, the position of the last file before
it.

| | Position | `file_count` | `ended_after` | Embedded `[[files]]` entries |
|---|---|---|---|---|
| **full seal** | M (planned) | M+1 | M-1 | 0 to M |
| **short seal** | K+1, with K ≤ M-2 | K+2 | K | 0 to K, then its own entry at K+1 |

In the usual case K is the data slice in which the drive's early warning came (§5.2). A
resume after a crash can seal lower, when the medium lost files the catalog had recorded
written (`layout-session.md`, "The short ending").

**Grammar.** One `key = value` per line, in exactly this order, for both kinds:

```toml
[seal]
volume = "{label}"
layout_version = 3
requires = []
file_count = {file_count}
ended_after = {ended_after}
sealed_at = "{sealed_at}"
front_index_sha256 = "{front_index_sha256}"
```

- `requires` repeats File 0's (§1.2): it is the fence when File 0 cannot be read.
- `front_index_sha256` is the hash of File 3's exact bytes (the byte contract below), on
  both kinds. File 3 is the plan and never changes: a short seal binds the same front index
  a full one would have.
- `sealed_at` is fixed-width (`YYYY-MM-DDTHH:MM:SSZ`, 20 bytes).

**OPEN (Stage 1):** D1 says that "the front index's and the seal marker's own magics move
with" File 0's. Neither file has a `magic` key in Layout 2: only File 0 states
`magic = "tapectl-volume-v2"`; File 3 and the seal state `layout_version`. Options: (a)
only File 0 carries a magic, and File 3 and the seal are fenced by `layout_version = 3`,
as written above; (b) add `magic = "tapectl-volume-v3"` to `[index]` and `[seal]`, the
line before `layout_version`.

**The embedded copy.**
- **Full seal:** entries 0 to M. File 3's own entry carries its `size_bytes` and
  `sha256_encrypted`; the seal's own entry, last, carries `position` and `type` only.
- **Short seal:** entries 0 to K copied from File 3, with File 3's own entry filled in the
  same way, then the seal's own entry (`position = K+1`, `type = "seal_marker"`), with no
  `size_bytes` and no `sha256_encrypted`. Entries 0 to K equal File 3's in `position`,
  `type`, `size_bytes` and `sha256_encrypted`, except File 3's own entry, which the copy
  fills in. The entry at K+1 differs from File 3's, which plans a data slice there.

The copy mirrors LTFS's front-copy-plus-end-index redundancy without partitioning: front
damage, and the whole map is recovered from the tail; tail damage, and the tape reads as
unsealed (fail-safe) but stays navigable from the front. The copy is not itself
hash-protected; its per-file claims are validated by hashing the files they describe, and
it must pass the self-consistency rules below before it is trusted.

**The human-readable header** is the same on both kinds, so it does not change a seal's
length:

```
Volume: {label}
This file seals the tape: its presence means every file before it is
present. Its absence means the tape is unsealed (interrupted or aborted).
The tape may end before the last file the front index plans:
ended_after below is the last file before this one, and a file the
front index lists after it is not on this tape. It binds the front
index by the sha256 below. The [[files]] entries are a copy of the
front index (File 3) up to this file, usable if File 3 is damaged —
verify any entry by hashing the file it describes.
```

**Length.** The build step sizes the seal with a placeholder: the full seal, with
`ended_after = M-1` and a fixed-width `sealed_at`. A full seal is exactly the
placeholder's length. A short seal is never longer: its `file_count`, `ended_after` and own
`position` have no more digits, and it carries at least one data-slice entry fewer. The
writer checks that a short seal is no longer than the placeholder, never that it is equal.

**Parse rules** (T4.6, determined: #562's restatement without the `short-seal` entry):

- **S1.** `ended_after` is a non-negative integer and `ended_after + 2 == file_count`. The
  seal's own position is `file_count - 1`, which is `ended_after + 1`. This holds on both
  kinds: it validates a seal, it does not classify one.
- **S2.** Every seal carries `ended_after` and keeps S1.
- **S4.** The embedded copy has exactly `file_count` entries: positions 0 to
  `file_count - 1` in order, one `front_index` entry at 3, one `seal_marker` entry, last.
- **S5.** The embedded entry at position `ended_after` has `type = "data_slice"`. That
  holds on a full seal and on a short one (a writer never seals short before the first data
  slice, §5.2), and fails a seal that ends inside the metadata. Every Layout tapectl writes
  has at least one data slice.
- **S6.** A seal that breaks S1, S2, S4 or S5 is **unparseable**. By the fail-safe
  precedence that is the unsealed signal: such a seal never reads as sealed. A seal with
  no `ended_after` breaks S2.

(Layout 2's S3, a seal with no `ended_after` read as full, goes with compatibility, item
242; the numbering keeps S4 to S6 for the readers that cite them.)

**Classification.** A seal is **full** when its `file_count` equals the plan's count (File
0's `total_files`, File 3's entry count, or the Layout's), and **short** when it is less. A
`file_count` greater than the plan's means the tape's two ends disagree, as a
`front_index_sha256` mismatch does. A reader with neither File 0 nor File 3 cannot classify
the seal and need not: its embedded copy describes what is on the tape.

**No unit identity.** A seal carries positions, kinds, on-tape sizes and ciphertext hashes,
and `ended_after`. Nothing in it names a unit, a tenant or a file, or says which staging
the end of the tape divided; only an envelope the reader can open tells that (§2).

**The `front_index_sha256` byte contract (normative, cross-tool).** The hash is computed
over File 3's **exact generated bytes**: the unpadded TOML text as materialized at build
time, no added trailing newline, no block padding. A reader recovering File 3 from a padded
tape read obtains the same bytes by stripping trailing NUL padding (equivalently
`tr -d '\0'`, since the text contains no interior NULs); tapectl truncates to the Layout's
recorded File 3 size. Seal generation, `Store::confirm` and RESTORE.sh (`--info`,
`--verify`) all use this definition.

**The keyless chain.** The seal marker is the **unhashed root of trust**. From it,
integrity is verifiable with only `dd` and `sha256sum`:

```
seal marker  ──front_index_sha256──►  File 3 (front index)
File 3       ──sha256_encrypted[i]──►  every content file i  (0, 1, 2, envelopes, slices)
```

Hash File 3 and compare it with the seal's `front_index_sha256`; then hash every other
file at or before `ended_after` and compare it with File 3's `sha256_encrypted[i]`. This is
what confirm verifies (§6), and what an heir can re-run years later for bit rot.

### 5.1 What a short tape promises, and what readers do with it (normative)

**What it promises** (rewritten for item 288: no staging is `cut`).
- **It is sealed.** The short seal asserts that every file from 0 to `ended_after` is
  present, and binds File 3 as a full seal does. The keyless chain runs over those files.
- **A staging whose every slice is at or before `ended_after` is on it whole**: restorable
  by the heir path, and the catalog counts the tape as a copy of it
  (`writes.status = 'completed'`).
- **A staging with some slices at or before `ended_after` and some after it is a range on
  this tape** (§5.3): the run of its slices this tape holds. The slices after
  `ended_after` stay staged and are written first on the next cartridge (item 288).
- **A staging none of whose slices is at or before `ended_after`** is not on this tape:
  the plan listed it and the seal withdrew it. The catalog keeps no write row for it on
  this volume (T5.1, determined), and its slices are written on the next cartridge.
- **Every position after `ended_after` is not on this tape.** File 3's hashes for those
  positions describe files that are not on it; no reader compares them with anything.
- **Everything written before the slices is the plan.** File 0, File 3, every envelope's
  `MANIFEST.toml`, `RECOVERY.md`, `PLAN.toml`, `catalog.db`, the file tables and the
  `index/` members list positions after `ended_after`. The seal withdraws those claims. An
  `index/units.tsv` row whose `state` is `this-tape` names the run this tape's plan holds,
  which `ended_after` may end sooner. The indexes of later tapes name only `completed` and
  `range` writes as holders (§1.5).
- **Nothing follows the seal.** End of data comes directly after its closing filemark.

**What readers do** (tapectl and RESTORE.sh alike, on every Layout 3 tape):
- **R1. Learn `ended_after` before reading any data slice.** tapectl takes it from the
  catalog for a volume this catalog sealed (`volumes.ended_after`; NULL means a full seal,
  so the planned M-1), and otherwise reads the seal (§1.2 rule 6).
- **R2. Without a readable seal,** read on as for an unsealed tape (§7.2 step 3). END OF
  DATA met at a planned position is the end of the tape: that file and every later one are
  not on it.
- **R3. Never read a position after `ended_after` for data.** Report it as not on this
  tape.
- **R4. A staging this tape does not hold whole is read by what it holds.** A reader takes
  from this tape the files whose slices are all at or before `ended_after` (§1.6 "this tape
  holds F"), names the rest, and says where its other slices are when the index says
  (§1.5). It never reads a slice after `ended_after`.
- **R5. A second tape is read the same way.** A reader that takes slices or files from
  another tape (the heir's prompt for the next cartridge, the repair from another copy,
  tapectl's read of a slice from another copy) reads that tape's seal first and takes
  nothing from past its `ended_after`.

### 5.2 The writer at the early warning (normative)

Ruled 2026-10-20 (ADR-0012 items 84, 99-100, 110-114), from the st driver's measured
behaviour on the production host's HP LTO-6
(`docs/runs/2026-10-20-lto6-profile-and-lbp.md`), and carried to Layout 3 word for word
(items 240, 288). The session's states and records are in `layout-session.md` ("The short
ending"). In W1 to W9, "§5" means this document's §6, "§4.2" its §5.1, and "before 1.6.0"
the clean abort of ADR-0007.

- **W1. The early warning** is the first block write (`write(2)` of a tape block, as
  opposed to a filemark) that st refuses with ENOSPC (errno 28) in a session. From then on,
  st refuses about one block write in two, at once, without sending it to the drive. Measured
  runs were at most 3 refusals.
- **W2. Retry a refused block.** Write the same buffer again until st accepts it. More than 16
  refusals of one block in a row is the **hard end**: the session aborts (W6).
- **W3. Never retry a filemark.** st's alternation was measured on block writes only: after
  the warning, the immediate filemark, the seal block and the synchronous filemark were each
  accepted at once. A refused filemark is therefore taken as the drive's refusal. A retried
  filemark that had in fact been written would add an empty file and move every later
  position. A refused filemark aborts the session (W6).
- **W4. Finish the entry in flight.** The entry in whose blocks the first refusal came is
  written whole, closing filemark included. Call its position K. Then:
  - **K is a data slice before the last one** (K ≤ M-2): write no further data entry. Write the
    short seal at K+1, with `ended_after = K`.
  - **K is the last data slice** (K = M-1): write the full seal at M, as planned.
  - **K is before the first data slice** (Files 0 to 3, or an envelope): abort (W6). A tape
    that ends inside its metadata holds no unit.
  - **A warning first seen in the seal's own blocks** changes nothing: they are retried like a
    slice's (W2), and the seal stays the kind it was.
- **W5. Record the warning before the seal.** The catalog's `write_early_warning` event is
  committed before the seal's first block goes to the tape (`layout-session.md`).
- **W6. Abort** as before 1.6.0 on the hard end inside a slice or a seal, on a refused
  filemark, or on a warning before the first data slice: no seal, the tape unsealed, the rows
  `aborted`. This is ADR-0007's clean abort. The short seal replaces it only at the early
  warning.
- **W7. The seal is written as every seal is:** the data slice before it ends with its
  immediate filemark, then the seal's blocks, then one synchronous filemark, which flushes the
  drive's buffer before the catalog records the seal.
- **W8. The room past the warning is a slice plus the seal, with a margin.** A slice plus the
  seal must fit in half the smallest post-warning room among the configured drives (ADR-0012
  item 100). `config check` and `stage create` refuse a larger `slice_size`, which is what
  makes W4 safe.
- **W9. Read back what was written past the warning.** A session that saw the warning in a
  data slice (W4's first two cases) reads back the entry at `ended_after` and the seal, and
  hashes the entry against File 3, under every confirm tier (§5; ADR-0012 item 114). A block
  st refused but the drive kept would show there.

**What the plan lists, and the one bound** (items 288, 289; D19, D20). W1 to W9 above
are unchanged. What changes is the plan above them:
- **Every staging may continue on the next cartridge.** None opts in, and none is `cut`
  and written again whole (item 288). The slice in flight at the warning finishes (W4),
  the seal records where the tape ended, and the slices not written stay staged and are
  written first on the next cartridge.
- **On a drive that declares `post_warning_bytes`,** the plan lists slices in order to the
  end of the staging that passes the cartridge's nominal capacity, and the early warning
  ends the tape (item 289): the queue is filled to the warning, and one staging lies across
  the end of every full tape.
- **On a drive that does not,** the plan stops at the last slice that fits under the fill
  ceiling, inside a staging if that is where it falls; the tape is sealed in full there,
  and the slices after it continue on the next cartridge. No write is refused for the
  missing key.
- **An early warning before the plan's end seals short in both cases.** A slice the tape
  did not take has no written position: it is never a copy and is never counted.

The plan's order, the batch and the catalog's records are `layout-session.md`'s
("Spanning", as T1.2 rewrites it from the plan's T5.1).

### 5.3 A range, and every unit continuing (normative)

**What a range is.** The run of one stage set's slices on one volume (CONTEXT.md "Range").
A volume holds **at most one unbroken range of a stage set**, in slice order and at
consecutive positions (item 122). Any staging may be held as ranges (item 288). The
drive's early warning ends a range as it ends any tape (§5.2).

**The writer's half** (items 122, 288, 289; T5.1, determined):
- On each tape the plan lays out first the **owed runs**: for each staging with a `range`
  row on a sealed volume and slices still owed (below its copy floor), its owed slices as
  one unbroken run in slice order, from the lowest owed slice to the highest. Several go
  in the order the write takes them. Then **the queue**, every other staging the write
  takes, in its order. There is no second class and nothing is deferred behind another
  staging.
- **One generation for one copy's span** (item 86): a run that does not start at slice 1 is
  planned only on a cartridge of the generation of the volume whose range ends just before
  it.
- With a copy target of 2 at one place this order gives item 93's layout: tape 1 ends in
  the staging's slices 1 to K; on tape 2 the owed run is the whole staging, so tape 2 holds
  it whole; tape 3 begins with the owed run K+1 to N.

**On tape, a range is known by positions.** Every block lists every slice of its staging
(§1.9) and gives `tape_position` only to the slices this tape's plan holds. There is no
`may_span` key and no `requires` entry (item 288, D1, D6). With a key, a reader takes the
staging's slices that have a `tape_position` at or before the seal's `ended_after`. Then:
- **All N slices:** the tape holds the version **whole** and is a copy of it
  (`writes.status = 'completed'`).
- **Some, not all:** the tape holds a **range**, the run from the lowest of them to the
  highest (`writes.status = 'range'`).
- **None:** the staging is not on this tape (§5.1).

Confirm records this split (`layout-session.md`), and `catalog rebuild` and the adoption
of a lost write record the same split from the tape.

**Without a key, nothing new is readable** (item 126). A range's slices are `data_slice`
entries of File 3 like any other; the front index has no new type, and the seal is §5's.

**What a range promises.**
- Every slice of the range is on the tape, and the keyless chain covers it.
- A range is not a copy on its own. Copies are counted slice by slice, and a version is as
  covered as its least-covered slice (items 76, 127).
- **Read alone, a tape that holds a range gives every file whose slices it holds** (items
  266, 283; §1.6 "this tape holds F"). Only a file whose run lies across the cartridge
  boundary needs both tapes; a hard-linked name whose first name is on another tape is
  listed for the repair (item 293).
- A tape never names a tape written after it (ADR-0001, item 126). A later tape's
  `index/units.tsv` names the earlier holders, with their runs (§1.5).

**Reader rules for a range** (tapectl and RESTORE.sh alike, beside R1 to R5):
- **R6. A slice with no `tape_position` is on another tape.** It is never read from this
  tape and never reported as damage. A slice whose position is after `ended_after` is not
  on this tape either (R3).
- **R7. A unit whole is read across its tapes as one stream** (items 131, 267): slice 1
  first, in slice order, with the one prompt for the next cartridge (§7.4). **A restore may
  start on any tape** and takes the files that tape holds whole (item 266). At the prompt
  "I do not have it" is an answer: the restore goes on with what it has and lists the rest
  with the cartridges that hold it. A file whose run lies across the two cartridges is then
  removed again if tar had begun it, and is listed.
- **R8. Another tape is accepted for slice S by hash alone, without a key.** It supplies
  slice S only when its front index lists a `data_slice` whose `sha256_encrypted` equals
  slice S's in the manifest, at a position at or before that tape's own `ended_after`,
  read from its seal before any slice (R5). A tape with no readable seal, or whose seal
  does not cover that position, is not accepted. The reader then takes slices S+1, S+2 and
  on from that tape for as long as its front index lists each by the same rule. A tape's
  label, its index or another envelope's claims never stand in for the hash. A tape that
  holds the version whole supplies any slice of it.
- **R9. Every slice is checked against the front index of the tape it was read from**
  before it is used.

A unit read in more than one tar run (a tape at a time, into the same destination) can
lose folder modification times, and a hard-linked name read before its first name is not
made (T0.7). So one run across the swap is the way to read a unit whole; a later run on
another tape adds that tape's files and leaves alone the ones already restored and
checked, and the hard-linked names it could not make are restored by the repair (§1.7).

### 5.4 A range in `volume rewrite` and in compaction (normative; D21)

Item 133's refusal of a volume that holds a range is superseded (item 288): most full tapes
hold one. D21, as ratified (item 290): **a rewrite or a compaction carries a volume's live
slices as they are, by ciphertext, in slice order; the new volume's block for that staging
lists every slice of it and gives positions to the ones this volume holds, exactly as a
range written from staging does; whether a slice is live is its version's state; `volume
rewrite`'s typed consent names every version of which the volume is the only holder of some
slice.**

What follows is the coordinator's determination under it (the plan's T5.3):
- **What is carried.** Every slice the source holds a `written` position of, in a
  `completed` or a `range` row, whose version is neither reclaimable nor purged. A source's
  `range` row is carried as the one run it is.
- **The destination's plan** is §5.3's, held at or below the fill ceiling (item 142: a
  rewrite may hold a unit's only copy, and never plans past the ceiling). The slices are
  written as the run they were read as; the stagings keep the source's order.
- **Two sources that hold runs of one staging** go onto one destination only when their
  runs join into one unbroken run. Otherwise they are not paired, and a pairing asked for
  by hand is refused, naming the staging.
- **The destination's rows** come from confirm's split (§5.3): a carried range stays a
  range, and two runs that join into the whole staging become a whole copy.
- **The hold of a rewrite is tested for each slice** (T5.1, determined): a staged slice of
  a staging the rewrite staged is held while the rewrite is open and no eligible volume
  holds that slice at a `written` position in a `completed` or `range` row that was read
  back in full. The rewrite closes when every slice it staged passes that test.
- **An early warning on the destination** leaves a range there: the slices not taken stay
  staged and held, the rewrite stays open, and the stop names the ordinary `volume write`
  with a full readback that writes them to another cartridge.
- **`compact-finish` retires the source** only when every live slice the source holds,
  in either kind of row, is held by another eligible volume.
- **The consent's facts** say, for a version of which the source holds some slices, which
  slices, and name the volumes that hold the rest.

---

## 6. What confirm verifies (byte level)

The write session's **states and sequence** are `layout-session.md`'s; this section fixes
the *cryptographic content* of the confirm pass. Confirm is a **single forward pass from
BOP** (no seek-back: the index is at the front):

1. Read the seal marker (the last file, located by §1.2 rule 6: at `ended_after + 1` when
   the catalog records the volume short). Absent: not sealed, and confirm fails. It is
   parsed by §5's rules.
2. Hash File 3 and compare it with the seal's `front_index_sha256`. A mismatch means the
   tape's two ends disagree: quarantine.
3. Parse File 3 and diff its `{position, type, size_bytes}` entries against the Layout:
   the **navigable** tier. File 3 and the Layout are both the plan, so they are compared
   whole on a short tape too. The seal's `ended_after` must equal the catalog's (the
   session's ending for a write's confirm, `volumes.ended_after` for `volume verify`; NULL
   means the planned M-1). A disagreement is the mismatch kind `ended_after_disagrees`: it
   says nothing about the medium, so the confirm is **inconclusive**, never a quarantine.
4. For every file at or before `ended_after` except File 3 and the seal marker, read and
   hash it and compare it with File 3's `sha256_encrypted[i]`: the **integrity** tier.
   Positions after `ended_after` are not read.

**The readback past the early warning** (item 114). A write session that saw the early
warning in a data slice (W4) always reads back the entry at `ended_after` and the seal, and
hashes that entry against File 3, whatever the tier.

**Which tier runs depends on the caller** (item 1 of 2026-10-06):
- **A write's post-seal confirm is navigable by default** (steps 1 to 3, plus the readback
  past the early warning when there was one), and a passing one seals. `--full-confirm`
  opts in to the integrity tier. A session that saw a Logical Block Protection refusal
  confirms in full (item 186). `audit` (`no_full_verify`) and `report verify-status` name
  every sealed volume with no passed full readback recorded until `volume verify` runs one.
- **`volume verify` runs the integrity tier by default**; `--quick` opts down to the
  navigable tier and is recorded as such.

The `verification_sessions` row records **which tier** was achieved (ADR-0001: recorded
strength matches what ran): integrity is `full`, navigable is `quick`.

**Logical Block Protection is not part of this chain** (items 161-166, 174, 175, 185,
186). tapectl arms the drive's Control Data Protection page for its own open of the drive
and clears it on every exit; st carries each block with its 4-byte CRC, the drive checks it
before writing, and on a read the drive sends its own, which tapectl checks. The CRC is
never stored: the medium's blocks are the same with or without it. A block the drive
refuses is written again, once, from its file's planned start (item 174).
