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
