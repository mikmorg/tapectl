# On-tape volume format — Layout Version 2 (normative)

This is the authoritative specification of the bytes tapectl writes to a sealed
cartridge. It **supersedes** `tapectl-design-v4_0.md` §2.6 (zone list), §8.1–§8.8
(file formats), and Appendix B/C/D (contract points, write sequence, append), and
is governed by **ADR-0007**. It pairs with `docs/design/layout-session.md`, which
owns the write-session **state machine, transitions, and operational sequence**;
this file owns the **static format, the isolation invariant, the integrity chain,
and the heir path**. Where a fact touches both, it is stated once here or there and
referenced, never restated (so the wording cannot drift). CONTEXT.md vocabulary
(Layout, Sealed, Heir Path, Escrow Recipient, Claim, Evidence) is used without
redefinition.

`layout_version = 2`. No production `layout_version = 1` tape is expected to exist
(the first production write postdates ADR-0005; only disposable mhvtl test tapes are
v1). A trivial v1 reader stub is retained so the format degrades gracefully if a v1
tape is ever found — but v2 is defined cleanly, not as a migration of v1.

---

## 1. Zone order (single partition, fixed 512 KB blocks)

A sealed v2 tape is a single partition of `M+1` tape files (each terminated by a
filemark), in this fixed order:

```
┌──── FRONT: plaintext navigation (all known pre-write, no key needed) ─────────┐
│ File 0  ID thunk        plaintext   identity: label, uuid, layout_version,    │
│                                     total_files, and the positions of the     │
│                                     front index (File 3) and the seal marker; │
│                                     plus a [media] block describing the        │
│                                     cartridge it was written on (ADR-0010).   │
│                                     "The full map is File 3."                  │
│ File 1  System guide    plaintext   heir manual (mt/dd/age/dar/sha256sum).    │
│ File 2  RESTORE.sh      plaintext   heir tool; --info reads File 3.           │
│ File 3  FRONT INDEX     plaintext   for EVERY file 0..M: position, type,      │
│                                     size_bytes; and sha256_encrypted for      │
│                                     every file EXCEPT File 3 and the seal      │
│                                     marker. ZERO content metadata.            │
├──── MIDDLE: encrypted decryption metadata (all known pre-write) ──────────────┤
│ File 4  Tenant envelope enc(t+op+esc) ┐ MANIFEST.toml (filenames,             │
│  ...    Tenant envelope enc(t+op+esc) │ sha256_plain, dar command, slice      │
│ File j   Operator env    enc(op+esc)  │ positions), RECOVERY.md, catalogs/,   │
│ File j+1 Operator backup enc(op+esc)  │ PLAN.toml (op only). Order per §2.    │
│ File j+2 Tenant backup  enc(t+op+esc) │ Each backup is its primary's bytes,   │
│  ...    Tenant backup   enc(t+op+esc) ┘ copied (§1.3).                        │
├──── BULK: encrypted data (the mass of the tape) ──────────────────────────────┤
│ File k  Data slice      enc(t+op+esc) ┐ age(dar base.N.dar). One unit's       │
│  ...     Data slice     enc(t+op+esc) │ slices contiguous. The ONLY zone a    │
│ File M-1 Data slice     enc(t+op+esc) ┘ short/damaged tape can truncate.      │
├──── TAIL: the completeness assertion ─────────────────────────────────────────┤
│ File M   SEAL MARKER    plaintext   {layout_version, file_count = M+1,        │
│                                     sealed_at, front_index_sha256} + a full   │
│                                     embedded copy of the front index. Its     │
│                                     presence = "everything before me is here."│
└────────────────────────────────────────────────────────────────────────────────┘
    esc = the permanent Escrow Recipient (ADR-0005) — in every encryption.
    op  = operator recipients.  t = the owning tenant's recipients.
```

**A tape that ended early (since 1.6.0, §4.1–§4.3).** The positions above are
the **plan**. When the drive reports its early warning (CONTEXT.md "Early
warning") in data slice K before the last one, the writer finishes slice K and
writes a **short seal** at K+1 instead of File M. Files K+2 to M are then not on
the tape. File 0's `seal_marker` and `total_files`, File 3, and every envelope
still describe the plan; the short seal says where the tape really ended
(`ended_after = K`). A short tape loses only trailing slices, as the bulk-zone
note says, and every unit wholly at or before K is on it whole (§4.2).

**A unit across tapes (since 1.7.0, §4.4).** A unit that allows spanning may have
only a run of its stage set's slices on a tape: a **range** (CONTEXT.md "Range").
Its `[[units]]` block in each envelope's `MANIFEST.toml` lists every slice of the
set and gives a tape position only to the slices this tape plans, and the seal's
`ended_after` trims that run as it trims any plan. Such a tape states `requires =
["short-seal", "spanning"]` (§1.2). The zones are unchanged: one unit's slices on
one tape are still contiguous and in slice order.

**Rationale for the order** (full derivation in ADR-0007 / the research note
`docs/research/2026-07-21-ontape-format-and-write-design.md`):

- **Front index (File 3)** puts the whole navigation map at BOP. An heir reads it
  with `mt rewind; mt fsf 3; dd` — no spooling to end-of-data to learn what is on
  the tape or how large each file is. This replaces the v1 mid-tape "mini-index."
- **Envelopes before slices** (decision D2) means a truncated or tail-damaged tape
  loses only trailing *slices*; every envelope needed to navigate and decrypt what
  did land survives.
- **Every envelope has a backup copy** (§1.3): the operator envelope since v2's
  first write, each tenant envelope since 1.1.0 (#418 item 2, ADR-0012
  2026-10-12 item 44), so one unreadable or damaged envelope never strands the
  slices it describes.
- **Seal marker last** is the only honest place for "complete" — completeness can
  be asserted only after the last content file exists. It is not optional: it is the
  price of front-loading the index (a front index alone would make a truncated tape
  look sealed).

### 1.1 File 0's `[media]` table (normative)

`[media]` is the last table in File 0 and **must stay last**: RESTORE.sh's
`toml_val` reader is table-blind and first-match-wins across the tables its
`sed` concatenates, so only a trailing table can gain a key without risking a
name that shadows one an earlier table owns.

| Key | Meaning |
|---|---|
| `cartridge_manufacturer` | MAM manufacturer string at write time, `""` if unknown |
| `cartridge_serial` | the cartridge's identity — see `cartridge_identity_source` |
| `cartridge_identity_source` | **how `cartridge_serial` was established**: `"mam"` or `"operator"` |
| `tape_length_meters` | MAM length, `0` if unknown |
| `load_count_at_write` | MAM load count at write time, `0` if unknown |

**`cartridge_identity_source` (ADR-0010, ADR-0012).** A reader — the heir path
included — cannot otherwise tell a chip-reported serial from a barcode somebody
typed, and so cannot tell whether `cartridge_serial` is verifiable against the
medium itself or only against the catalog.

- `"mam"` — `cartridge_serial` is the serial the cartridge's own chip reported
  when this volume was bound to it. Verifiable against the medium: load the
  tape, read the MAM, compare.
- `"operator"` — no serial was readable when this volume was bound, so the
  operator named the cartridge (`volume init --cartridge <barcode>`) and
  `cartridge_serial` carries **that barcode**. A barcode is a relabelable
  sticker (ADR-0012), so this string is verifiable only against the catalog,
  and only for as long as nobody re-labels the cartridge.
- **Absent means unknown, and never means `"mam"`.** Every tape written before
  the field existed omits it, and those tapes stay readable forever; a reader
  that defaulted the absent case to `"mam"` would make all of them falsely
  attest a chip-verified serial. A reader that cannot tell must say so.

The value records how the identity was established **at the binding**, not what
the drive happens to report at this contact: a cartridge whose chip could not be
read at `volume init` is `"operator"` on every volume of that binding, even if a
later drive reads its MAM cleanly. Corroborating the loaded medium against that
record at each contact is a separate concern, and belongs to the catalog rather
than to these bytes.

### 1.2 How v2 extends: `requires`, and keys a reader does not know (normative)

Ruled 2026-10-06 (ADR-0012 amendment item 15); closes #384. Tapes written by
tapectl 1.1.0 and later state, in File 0's `[volume]` and in the seal
marker's `[seal]`, one line after `layout_version`:

```toml
requires = []
```

`requires` lists the features a reader must understand to read this tape
correctly. 1.1.0 knows none, and requires none.

**From 1.6.0, every tape requires `short-seal`** (ADR-0012 item 111, #420).
File 0's `[volume]` and the seal's `[seal]` both state it, on every tape 1.6.0
or later writes, whether the tape sealed full or short:

```toml
requires = ["short-seal"]
```

`short-seal` means: this tape may end before the last file its plan lists;
its seal states `ended_after`, the last file before the seal; and a reader
finds the seal by rule 6 and trusts it over the plan (§4.1, §4.2).

**From 1.7.0, a tape whose plan holds a slice of a spannable set also
requires `spanning`** (ADR-0012 item 124, #478). A *spannable set* is a stage
set whose write may span tapes. Whether it may is frozen into its `writes` row
when the write is planned (`writes.may_span`, ADR-0012 item 121;
`layout-session.md`, "Spanning"), and a `volume rewrite` never sets it
(ADR-0012 item 142). Every tape whose plan holds at least one slice of such a
set states, in File 0's `[volume]` and in the seal's `[seal]`:

```toml
requires = ["short-seal", "spanning"]
```

It does so whether the tape's plan holds the set whole (item 93's middle tape)
or a run of it, and whether or not the tape ended before any of the set's
slices: the fence follows the plan, because the plan is all that is known when
File 0 is written. Every other tape 1.7.0 writes states `requires =
["short-seal"]`, as 1.6.0 did. Fencing every 1.7.0 tape, and fencing only the
tapes that continue a span, were considered and not chosen (item 124).

`spanning` means: a `[[units]]` block of this tape's `MANIFEST.toml` may state
`may_span = true`, and such a block lists slices that are not on this tape,
with no `tape_position` (§4.4). A run of a set's slices that starts after
slice 1, or that `ended_after` ends before the set's last slice, is then a
range of a version held across tapes: not damage, and not a cut unit. A reader
that does not know the feature would misread a slice with no position
(tapectl 1.6.0's manifest reader requires every `tape_position`), so the
feature fences it (rule 3).

**The spelling of the line is normative** (ADR-0012 item 124). Each feature
is double-quoted. Two features are separated by a comma and one space, in the
order shown. There is no space after `[` or before `]`, and the line is the
one after `layout_version`, as before. RESTORE.sh reads the list by deleting
brackets, quotes and commas and splitting what is left at spaces
(`check_requires`), so without the space it would read the one feature
`short-sealspanning` and refuse the tape by that name.

A reader that knows the features lists them: `KNOWN_FEATURES = &["short-seal",
"spanning"]` in `format.rs` and `KNOWN_FEATURES="short-seal spanning"` in
RESTORE.sh, from 1.7.0 (1.6.0 lists `short-seal` alone).

**A reader that does not know `short-seal`, or `spanning` on a span tape,
refuses the tape at File 0, by name** (rule 1). RESTORE.sh has since 1.1.0,
and so has every tapectl reader rule 1 lists, the restore and verify readers
since #476 (1.3.0): tapectl's
`FormatClaims::refusal` and RESTORE.sh's `check_requires` name the feature and
send the reader to the RESTORE.sh at the tape's own file 2, or to a newer
tapectl. It never half-reads the tape. When File 0 is unreadable, the same
refusal comes from the seal, which states the same `requires` (rule 1 checks
both). The fence holds from 1.1.0 on: a reader before 1.1.0 predates
`requires` and does not check it. The full and the short seal are fenced
alike; fencing only short tapes was considered and not chosen (item 111).

The rules every reader keeps
— tapectl's (`format::FormatClaims`, checked by `restore raw-volume`,
`catalog rebuild`, and, since #476, `restore unit`, `restore file`, `restore volume`,
`volume verify`, `volume read-slices`, `volume compact-read` (and `volume compact`'s
first step) and `volume resume`'s adoption of a lost write, each at its File 0
contact from the one read it already takes; the confirm/verify chain walk checks the
seal's) and RESTORE.sh's
(`check_magic`, `check_requires`):

1. **Refuse what you do not know, by name.** A tape whose File 0 or seal
   marker lists a feature the reader does not know, or states a `magic`
   other than `tapectl-volume-v2` or a `layout_version` other than 2, is
   refused before anything else of it is interpreted. The refusal names the
   feature and the way to a reader that knows it: a newer tapectl, or the
   RESTORE.sh at the tape's own file 2. A refusal is never a misread, and in
   the chain walk it is inconclusive (`SealUnreadable`), never evidence that
   the medium is bad.
2. **Absent means none.** No tape before 1.1.0 (L6-0001 among them) carries
   `requires`; it requires nothing. An absent `magic` or `layout_version`
   (a damaged File 0) is warned about, not refused — the case an heir most
   needs this script for must never become a hard stop.
3. **Ignore keys you do not know, unless `requires` names them.** A later
   writer may add keys to any table; a reader that does not know one reads
   on. A writer that adds anything a 1.1.0 reader would *misread* — not
   merely not know — lists a feature for it in `requires` (the short seal,
   #420, is `requires = ["short-seal"]` with an `ended_after` field: 1.6.0,
   §4.1; a range, #478, is `spanning`, with slices listed without a
   `tape_position`: 1.7.0, §4.4). A
   change no `requires` can fence (a different byte layout of the slices,
   say #370's "tapectl cuts the slices") bumps `layout_version` and `magic`.
4. **A key's meaning never changes.** New meaning, new key.
5. **New keys in File 0 go in a trailing table** (§1.1), with one exception:
   `requires` sits in `[volume]`, because it must be read before anything
   else is trusted and no other table uses the name. A v3 File 0 must not
   carry `front_index`/`seal_marker` as bare integers a v2 RESTORE.sh could
   read and follow.
6. **The seal marker is the last file, found as such, and never by a block
   address** (rewritten for 1.6.0: ADR-0012 items 110-111, #420). The
   **planned position** is File 0's `[layout] seal_marker` (always
   `total_files - 1`), File 3's `seal_marker` entry, or, for tapectl, the
   catalog's Layout. A reader finds the seal:
   - **at the planned position**, when the tape holds a file there; or
   - **when the medium ends before it**, as the **last file before end of
     data**: file N-1, where N is the number of files the tape holds, counted
     by the filemarks spaced over from the beginning of the tape. A file with
     no filemark after it is no file: a seal counts only once its closing
     filemark is on the tape.

   Both routes space by filemarks, and they reach the same file. tapectl
   spaces to the planned position; when st reports END OF DATA first, it takes
   N from st's own file count (`MTIOCGET`'s `mt_fileno`, the count
   `Store::reposition_at_most` and `Store::eod_at_or_before` already read) and
   reads file N-1. RESTORE.sh spaces to end of data and back over two
   filemarks (`mt eod`, `mt bsfm 2`; GNU mt spells end of data `eom`), which
   leaves the head at the start of that same last file, and it falls back to
   File 0's pointer only when mt cannot space to end of data. **No reader
   finds the seal, or any file, by READ POSITION or LOCATE** (`MTIOCPOS`,
   `mt tell`, `mt seek`, and the SCSI commands behind them): READ POSITION
   never answered through st on home2's HP LTO-6
   (`docs/runs/2026-10-20-lto6-profile-and-lbp.md`).

   Which seal a reader accepts:
   - **At the planned position:** a seal whose `file_count` is the planned
     count (§4.1 "full").
   - **Before it:** only on a tape that requires `short-seal` (File 0's
     `requires`, or the seal's own when File 0 is unreadable), and only a seal
     that itself requires `short-seal`, keeps §4.1's parse rules and states
     `file_count = N`. Anything else is no seal. tapectl then reads the tape as
     unsealed. RESTORE.sh keeps its degraded reading for an heir: it uses the
     last file and warns that it disagrees with File 0, as it has since 1.1.0.
     On a tape that requires `short-seal`, a seal before the planned position
     is the normal short ending and draws no warning.
   - **After it:** never accepted by tapectl. RESTORE.sh warns, as since 1.1.0.

   A read at or before the planned position that fails without st reporting
   END OF DATA at or before it is **unreadable, not absent** (#460): tapectl
   never reads it as "unsealed", and a fresh write refuses the cartridge.

   tapectl's catalog readers that know a volume's `ended_after`
   (`volumes.ended_after`, `layout-session.md`) read its seal at
   `ended_after + 1` directly, and check that the seal states the same
   `ended_after` (§5).

   **The write path's contact check keeps the same rule.** Before 1.6.0 it
   read only File 0's pointer, and END OF DATA there read as "no seal": on a
   short-sealed cartridge that would let `--force` overwrite a sealed volume
   (ADR-0003). From 1.6.0, when the planned position is at or past end of
   data, the contact check reads the last file on the medium before it
   concludes there is no seal, and a seal marker there refuses the write as
   any seal does. This closes `docs/design/v2-open-questions.md` §10's open
   item.

Unknown front-index `type`s are not refused by the 1.1.0 readers: a future
file type arrives with a `requires`, which is refused first.

### 1.3 Envelope backup copies (normative)

Ruled 2026-10-12 (ADR-0012 item 44, bringing #418 item 2 forward from 1.2.0;
the 2026-10-06 ruling of item 22 had deferred it). Every envelope on a tape
written by tapectl 1.1.0 or later is on it twice.

- **What a backup is.** The primary's exact bytes, copied: the same
  ciphertext, so the same `size_bytes` and the same `sha256_encrypted` in the
  front index. It is not a second encryption (age is randomized per call, and
  a second ciphertext of the same plaintext would be an unrelated file, not a
  copy that can be checked against the first).
- **Where it sits.** The middle zone holds, in order: the tenant envelopes
  (permuted, §2), the operator envelope, the operator envelope's backup, and
  then one `tenant_envelope_backup` per tenant **in the same order as the
  primaries**. So the operator pair keeps the place it has had since v2's
  first write, every primary comes before every backup, and each tenant's
  backup is separated from its primary by the operator pair (the largest
  envelopes on the tape), not written straight after it. The first data slice
  follows the last tenant backup.
- **How it is indexed.** Its own front-index `type`, `tenant_envelope_backup`
  (as the operator's is `operator_envelope_backup`), labelled only by kind
  like every envelope (§2). A reader pairs a backup with its primary by the
  equal `sha256_encrypted`, never by position arithmetic.
- **How readers use it.** A reader opens the primary first, and turns to the
  backup only when the primary does not read, does not match the front
  index's hash, or is damaged under a key that matched it. A backup whose
  primary opened is skipped, not opened again. Confirm and verify treat a
  backup like any other file: the integrity tier hashes both copies (§5).
- **Older tapes.** A tape without tenant backups (anything written before
  1.1.0) is read as before; there is simply no second copy to turn to. A
  reader that does not know `tenant_envelope_backup` (a 1.0.x RESTORE.sh)
  ignores those entries and reads the primaries exactly as it always did, so
  this needs no `requires` entry (§1.2 rule 3).

### 1.4 The operator envelope's `catalog.db` (normative)

The operator envelope and its backup carry, beside `MANIFEST.toml`,
`RECOVERY.md`, `PLAN.toml` and the dar catalogues, a tar member `catalog.db`
(#83): a standalone SQLite database holding what the catalog knew, at staging
time, about **this write's stage sets only** — every tenant's, so it is never
in a tenant envelope. `catalog rebuild --from-volume` reads it
(`db::ontape_catalog`), and `RECOVERY.md` shows an operator with only
`sqlite3` how to query it. Tape positions are not in it: `MANIFEST.toml` has
them. `PRAGMA user_version` is the schema level of the `tapectl.db` it was
built from, not this file's own shape.

**Generations, told apart by shape.** No stamp says which shape a file has
(declined 2026-09-11: the probe is the reader, and a stamp would be a second
source of truth). A reader probes, and refuses a file whose markers disagree,
naming what it found:

| Generation | Written by | Per-file tables | Also carries |
|---|---|---|---|
| 1 | #83's landing (2026-07) to 2026-09-11 | `files` | — |
| 2 | 2026-09-11 to 1.1.0 | `files` | `tenants`, `stage_sets.key_fingerprints`, `stage_slices.sha256_plain` |
| 3 | 1.1.0 and later (ADR-0012 2026-10-12 item 48, #418 R3) | `paths`, `file_versions` | as generation 2 |

Generation 3 is recognised by `file_versions` and `paths` with no `files`;
generations 1 and 2 by `files` with no `file_versions`, and between them by
`tenants` together with `key_fingerprints` (both present: 2; both absent: 1).

**Generation 1 and 2 `files`**: one row per walked entry: `id` (the writing
catalog's own `files.id`, copied verbatim and never renumbered: one run per
version in walk order, not necessarily from 1, with gaps between versions),
`snapshot_id`, `path`, `size_bytes`, `sha256` (64 lowercase hex characters),
`modified_at` (RFC 3339 text, `YYYY-MM-DDTHH:MM:SS+00:00`), `is_directory`
(0/1). Nothing else of a file's type: a rebuild from these tapes records a
symlink or a special file as a regular file, because the tape cannot say
otherwise (#381).

**Generation 3** takes the main catalog's narrow shape (migration 030) and
records each entry's kind and link target, so a rebuilt catalog keeps them:

```sql
CREATE TABLE paths (
    id      INTEGER PRIMARY KEY,   -- the writing catalog's own path id
    unit_id INTEGER NOT NULL,      -- units.id in this file
    path    TEXT NOT NULL          -- relative to the snapshot's source_path
);
CREATE TABLE file_versions (
    snapshot_id INTEGER NOT NULL,  -- snapshots.id in this file
    path_id     INTEGER NOT NULL,  -- paths.id
    kind        INTEGER NOT NULL CHECK(kind IN (0, 1, 2, 3)),
                                   -- 0 dir, 1 regular, 2 symlink,
                                   -- 3 special (FIFO, socket, device)
    size_bytes  INTEGER NOT NULL,  -- lstat's size; 0 for a directory
    mtime_ns    INTEGER,           -- Unix time in nanoseconds, UTC
    sha256      BLOB CHECK(sha256 IS NULL
                           OR (typeof(sha256) = 'blob' AND length(sha256) = 32)),
    link_target TEXT,              -- a symlink's target; NULL otherwise
    PRIMARY KEY (snapshot_id, path_id)
) WITHOUT ROWID;
```

- `paths` holds only the paths this write's versions use, under the writing
  catalog's ids. `path_id` order is the order the writing catalog first
  interned each path: walk order for a unit's first version; in a later
  version, paths new to the unit follow all earlier ones (nothing renumbers
  them). Nothing depends on a version's walk order.
- `paths` has **no `UNIQUE(unit_id, path)` index**, unlike the main catalog:
  nothing on the tape looks a file up by name, a rebuild interns paths into
  its own catalog's index, and the index would store every path twice. The
  file is read whole or one version at a time, along the primary key.
- No full-text index and no triggers. Permissions, owners, xattrs, ACLs,
  device numbers and hard links stay out, as in the main catalog: the dar
  catalogue beside it in the envelope is their authority.
- A reader of generation 3 refuses, naming the row, a `kind` outside 0–3 or a
  `sha256` that is not 32 bytes; it does not guess.

A tapectl from before 1.1.0 refuses a generation-3 file (it finds no
`files` table) rather than misreading it, and nothing but tapectl reads the
file, so the change needs no `requires` entry (§1.2 rule 3). Rebuild keeps
reading generations 1 and 2 for as long as any such tape exists.

### 1.5 The cross-volume index (normative, tapectl 1.3.0)

Ruled 2026-10-16 (ADR-0012 items 78-80, #418 R4). Before it, each tape described only itself:
the operator envelope's `catalog.db` (§1.4) holds this write's stage sets and nothing else, so
no tape said where a unit's other copies or older versions were. A tape written by 1.3.0 or
later carries an **index of the archive as the catalog knew it when this tape's Layout was
built**: which volumes hold which version of each unit, and, for a tenant, every path.

**Where it is.**

| Envelope (and its byte-copied backup, §1.3) | Members |
|---|---|
| each tenant envelope | `index/units.tsv` (that tenant's units only) and `index/paths.tsv` |
| the operator envelope | `index/units.tsv` (every tenant's units, with a `tenant` column); no `paths.tsv` |

The members are the last in the tar, after `catalogs/` and, in the operator envelope,
`catalog.db`. A tenant gets its two files only when it has an envelope on that tape. A tenant learns the labels of the tapes holding its
own units and nothing of another tenant's (§2); the operator and escrow keys see the
cross-tenant map.

**Encoding.** UTF-8 text. The first line is a header naming the columns; every further line is
one row; fields are separated by one TAB and rows end with one LF. Inside a field, four
characters are escaped and nothing else: backslash as `\\`, TAB as `\t`, LF as `\n`, CR as `\r`
(a path may contain any of them). An empty field means "not known". A reader finds columns by
their header name and ignores a column it does not know, so columns can be added without a
`requires` entry. Rows are sorted as given below, so the bytes are deterministic for a given
catalog.

**`index/units.tsv`**: one row per (version, holding volume). Sorted by `tenant` (operator only),
`unit`, `version`, then `volume`.

| Column | Meaning |
|---|---|
| `tenant` | the tenant's name (operator envelope only) |
| `unit` | the unit's name |
| `unit_uuid` | the unit's uuid |
| `version` | the version's ordinal (CONTEXT.md "Version") |
| `num_slices` | the number of slices of this version's stage set |
| `first_slice`, `last_slice` | the run of those slices this volume holds: `1` and `num_slices` for a whole version. For a range (1.7.0, ADR-0012 items 121-126, §4.4), the first and the last slice of that volume's run, which is one unbroken run in slice order. On another volume's row they are the slices the catalog records `written` there. On the `this-tape` row they are the run this tape's plan holds, which the seal's `ended_after` may end sooner (§4.2, §4.4) |
| `version_bytes` | the stage set's ciphertext bytes, all slices |
| `volume` | the holding volume's label |
| `volume_uuid` | its uuid (File 0's `uuid`) |
| `cartridge_serial` | the chip serial of the cartridge it was written on; empty when none was read |
| `generation` | the medium's LTO generation (File 0's `media_type`), so the reader knows which drives read it (`media.rs`) |
| `state` | `sealed`, `quarantined` (sealed, but a verify proved the medium bad; it may still partly read), or `this-tape` (the tape this index is on) |
| `sealed_at` | when that volume sealed (RFC 3339, UTC); empty for `this-tape` |

A stage set with no slices recorded states `num_slices`, `first_slice`, `last_slice` and
`version_bytes` as empty fields. A version with two stage sets on one volume has one row, the
lower stage set's.

**Which versions and holders** (item 80): every current version with at least one completed
write on a sealed, unretired volume, plus every version this write carries. Its holders are the
sealed, unretired volumes holding a completed write of it, quarantined ones included and marked,
and this tape. Unsealed, retired and erased volumes are not listed; reclaimable and purged
versions are not listed. Where a cartridge is kept is never on a tape (§2): find it from the
label or serial.

**Ranges** (1.7.0, ADR-0012 items 121, 126 and 141). From 1.7.0 a `range` write counts where a
completed one does: the versions are every current version with at least one `completed` or
`range` write on a sealed, unretired volume, plus every version this write carries, and the
holders are the sealed, unretired volumes holding a `completed` or `range` write of it. A spanning
version becomes current only once every slice of a stage set of it is on a sealed volume (item
128, `layout-session.md`). Until then it is listed only on a tape that carries it, where its
earlier holders are listed with their runs; a tape written meanwhile that does not carry it does
not list it. A holder of a range holds only its run, and no row is a copy on its own (item 127).
`units.tsv` names no stage set, so two stagings of one version (item 102) are not told apart in
it: a reader never joins runs by the index, and takes a slice from another tape only by its hash
(§4.4 R8). File 1's query of the Heir Kit's catalogue lists `range` writes with their slice runs
beside whole copies (item 141, §4.4).

**`index/paths.tsv`** (tenant envelopes only, item 79): one row per path of each version listed
in that tenant's `units.tsv`. Sorted by `unit`, `version`, then `path` (bytewise).

| Column | Meaning |
|---|---|
| `unit` | the unit's name |
| `version` | the version's ordinal |
| `kind` | `d` directory, `f` regular file, `l` symlink, `s` special (FIFO, socket, device) |
| `size_bytes` | lstat's size; 0 for a directory |
| `sha256` | the file's content hash as staging recorded it, 64 lowercase hex characters; empty for a directory, a symlink, a special file, or where none was recorded (1.3.0, ADR-0012 item 103: RESTORE.sh checks every restored regular file against it) |
| `path` | relative to the unit's root, escaped as above |

**Staleness.** An index is as of its own tape's write: it never names a tape written after it,
and a holder it names may since have been retired or erased. The newest tape's index is the most
complete; RESTORE.sh and File 1 say so.

**Size.** Uncapped: `paths.tsv` grows with the files of the listed versions (about 100 MB per
million paths, written twice per tape with the backup). The capacity gate counts envelopes as it
counts every file. `volume plan` prints an estimate of it: no this-tape volume exists yet,
everything staged is counted as one tape, and the figure is plaintext before tar and age (the
tape carries it twice, §1.3).

**Who reads it.**
- RESTORE.sh: `--where UNIT` prints the rows of `units.tsv` for a unit (every version and the
  tapes holding it); `--find PATH` prints the rows of `paths.tsv` whose path contains the text
  given, with the holders of each match. `--where --all` prints every unit the index names,
  with no limit: for each unit (keyed by tenant, name and unit uuid) its newest version by number,
  whether the loaded tape holds it, and every version's holders, after a summary and the units
  whose newest version the loaded tape does not hold. All three read the envelopes the key opens.
  From 1.7.0 they print a holder's slice run when it is not the whole version, and a holder of a
  range does not count on its own as holding the version (§4.4).
- RESTORE.sh `--restore` and `--fill-from`: after each unit is extracted, read the `sha256`
  column of the `paths.tsv` of the tenant envelope that lists the unit and version (an escrow or
  operator key finds it among the envelopes the walk opened; the operator envelope has none), check
  each restored regular file with `sha256sum`, and put a mismatch, or a listed file that is
  missing, on the damaged-file list (§1.6; ADR-0012 item 103). A file with an empty `sha256` is not
  checked; a tape with no index is not checked at all. `--no-file-check` skips it.
- `catalog rebuild --from-volume` reads the operator envelope's `units.tsv` and reports the
  volumes it names that the catalog lacks, as a to-do list for disaster recovery. It never
  creates a row from it (item 78; ADR-0001: a claim about a tape needs contact with that tape).
  From 1.7.0 the to-do list also names a spanning version's slices the rebuilt tapes do not
  hold, and the tapes that hold them where an index names them (item 130, §4.4).

**Older tapes and readers.** A tape written before 1.3.0 has no `index/` members and is read as
before. A reader that does not know them (RESTORE.sh before 1.3.0; tapectl reads envelope members
by name) ignores them, so the index needs no `requires` entry (§1.2 rule 3).

### 1.6 RESTORE.sh's damaged-file list (written by RESTORE.sh, not on the tape)

`.tapectl-damaged-files.txt` is written by RESTORE.sh (1.3.0, ADR-0012 items 102-103) in a
unit's restored directory (`--to` for one unit, `--to/UNIT` for several), for a restore whose dar
run named files it could not restore or whose files failed their recorded sha256. Line 1:
`#tapectl-damaged-files`, TAB, `1` (the format), TAB, `unit=` and the unit name, TAB, `uuid=` and
the unit uuid, TAB, `version=` and the version. Every further line is one path relative to the
unit's root, escaped as §1.5 escapes. A fill (`--fill-from LIST`) uses a tape only if its envelope
lists the same unit uuid at the same version, restores only the listed paths, and rewrites the
list with the paths still damaged, or removes it.

### 1.7 What a 1.6.0 tape declares up front (normative, tapectl 1.6.0)

Ruled 2026-10-20 (ADR-0012 item 111, #420). Everything before the first data slice is written
before the writer can know whether the tape will end early, so every 1.6.0 tape declares that it
**may**, in every file that describes the plan, and only the seal says whether it **did**
(§4.1). These declarations are the same on a tape that seals full and on one that seals short.

| File | Declaration | Where |
|---|---|---|
| File 0 | `requires = ["short-seal"]` | `[volume]`, the line after `layout_version` (§1.2) |
| File 3 | `may_end_early = true`, and a header paragraph | `[index]`, the line after `layout_version` (§3) |
| each envelope's `MANIFEST.toml` | `may_end_early = true` | `[manifest]`, the line after `created_at` (below) |
| each envelope's `RECOVERY.md` | a section on reading the seal | text (below) |
| File 1 | the same, for the heir | text (below) |
| the seal | `requires = ["short-seal"]` and `ended_after` | `[seal]` (§4.1) |

**File 0 is the plan.** Its `[layout] seal_marker` and `total_files` are the planned seal
position M and the planned count M+1 on every tape. On a short tape they name a position past
end of data; a reader treats them as the plan, never as damage (§1.2 rule 6). Nothing else in
File 0 changes, its human-readable header included.

**MANIFEST.toml.** From 1.6.0 the `[manifest]` table ends with one more line:

```toml
[manifest]
volume = "{label}"
tenant = "{tenant}"
created_at = "{created_at}"
may_end_early = true
```

- The line is the literal `may_end_early = true`, the last key of `[manifest]`, followed by the
  blank line that precedes `[[units]]` as today. It is in every envelope of every 1.6.0 tape: each
  tenant envelope, the operator envelope, and their backups (byte copies, §1.3).
- The name is File 3's, for the one fact. It collides with no `[[units]]` or `[[units.slices]]`
  key, which matters because RESTORE.sh reads `[manifest]` with its table-blind,
  first-match-wins `toml_val`.
- Absent means false: every envelope before 1.6.0.
- **It decides nothing.** A manifest's `tape_position` values are the plan. The seal's
  `ended_after` decides which of them are on the tape (§4.2). tapectl's `Manifest` reads the key
  as a bool (absent: false) and still ignores keys it does not know (#134). A 1.5.0 reader would
  ignore it, but it never reaches it: File 0 refuses the tape first.
- `MANIFEST.toml`'s golden pin moves with this line (item 111).

**RECOVERY.md.** Each envelope's RECOVERY.md gains a section, after the "Units in this envelope"
table and before the first unit's section, that says:
1. The tape may end before the last tape file the write planned, because the drive reached the
   end of its medium. The units table is the plan, written before any slice was.
2. The seal marker, the last file on the tape, says where it ended: its `ended_after` is the last
   tape file before the seal. Every tape file after `ended_after` is not on this tape.
3. How to read it by hand, with no key:

   ```bash
   mt -f /dev/nst0 setblk 524288
   mt -f /dev/nst0 eod          # GNU mt: eom
   mt -f /dev/nst0 bsfm 2
   dd if=/dev/nst0 bs=512k | tr -d '\0' | grep -E '^(file_count|ended_after) ='
   ```

   with the `/dev/nst0` caution the rest of RECOVERY.md gives.
4. The rule for a unit: when the last tape file of a unit (its "Tape files" column) is after
   `ended_after`, this tape does not hold the unit whole. Do not start reading its slices: it is
   restorable only from another tape. The index (`index/units.tsv`, §1.5) and RESTORE.sh's
   `--where` name the tapes that hold it. (From 1.7.0, a unit whose block states `may_span =
   true` may hold a range here instead, restorable with the tapes that hold its other slices:
   §4.4, and the range section below.)
5. RESTORE.sh does all of this by itself: `--info` and `--verify` report where the tape ended.

**File 1** (the system guide) says the same for the heir: what `requires = ["short-seal"]` means
(its glossary entry for `requires` stops saying the list is empty); that the seal's
`ended_after` says where the tape ended; how to read it by hand (the four lines above); that a
unit whose slices run past it is not on this tape whole and where to look for it; and that
RESTORE.sh marks the planned files past the end `NOT ON TAPE` (§4.2; from 1.7.0 in one line,
ADR-0012 item 140).

**What a span tape declares (1.7.0, ADR-0012 items 124-126 and 135).** A tape whose plan holds
a slice of a spannable set (§1.2) keeps every declaration of the table above and adds these:

| File | Declaration | Where |
|---|---|---|
| File 0 | `requires = ["short-seal", "spanning"]` | `[volume]`, the line after `layout_version` (§1.2) |
| each envelope's `MANIFEST.toml` that lists a spannable set | `may_span = true`; every slice of the set listed; `tape_position` only on the slices this tape plans | that set's `[[units]]` block, the line after `stage_set_id` (§4.4) |
| each such envelope's `RECOVERY.md` | a section on a unit across tapes | text (§4.4) |
| the seal | the same `requires` as File 0 | `[seal]` (§4.1) |

File 3 declares nothing new: it has no new entry type, and its `[index]` table and header are
1.6.0's. The seal's grammar and rules S1-S6 are 1.6.0's too; only its `requires` value follows
File 0's (item 126). File 1 gains its 1.7.0 text on every tape 1.7.0 writes, span tape or not
(§4.4).

## 2. Plaintext vs encrypted, and the isolation invariant

| Zone | On tape | Why |
|---|---|---|
| ID thunk, system guide, RESTORE.sh | plaintext | heir reads them with no key |
| **Front index (File 3)** | **plaintext** | navigation must work with no key; carries position/type/size and ciphertext hashes only |
| Tenant/operator envelopes, data slices | age-encrypted | *all* content metadata (filenames, content sizes, `sha256_plain`, dar catalogs, the packing plan, and since 1.3.0 the cross-volume index of §1.5) lives here and nowhere else |
| **Seal marker (File M)** | **plaintext** | structural completeness assertion; its `front_index_sha256` is a hash of a plaintext file |

**Isolation invariant (v2, normative).** No plaintext file on the tape may reveal:
filenames, tenant or unit names, plaintext-content hashes (`sha256_plain`), or key
fingerprints. Permitted in plaintext: per-file **on-tape byte sizes** and per-file
**ciphertext hashes** (`sha256_encrypted`) — structural facts, not attributable to
any tenant (no plaintext file carries a tenant identity; a `data_slice` or
`tenant_envelope` entry is labeled only by kind) and computable by anyone holding
the tape. Roles are strict: the plaintext hashes exist **solely to verify tape
integrity** (the keyless chain, §4–§5) and are never content claims; content hashes
and all content metadata live only inside the encrypted envelopes and the catalog —
which itself rides each volume encrypted (the operator envelope's catalog snapshot,
#83) and survives the machine via the Heir Kit (#69).

**How a data slice is made (2026-10-06, ADR-0012 amendment item 4).** dar writes
its archive to standard output (`dar -c - … --retry-on-change 0`); tapectl cuts
that stream into dar slices of `slice_size` and age-encrypts each one in memory,
so the staging directory only ever holds ciphertext. Each slice is a dar slice
exactly as dar frames its own: dar's slice header (read from a slice the
installed dar wrote at the same `-s`, with this archive's two labels), the
payload, and the one-byte `N`/`T` trailer, as `dar_xform -s <size> - base`
would cut the same stream. So `age -d` still yields a `base.N.dar` that
`dar -t`/`-x` read with no tapectl, and nothing in this document changes. The
recorded `dar_command` says so.

**Self-describing — what that promises (normative, 2026-09-11).** The phrase
is used throughout this repo and was defined nowhere; #136 was the first
attempt to reconstruct the *operator's* view from tape, which is how the
boundary became visible. A sealed volume promises two things and withholds
a third:

1. **Data.** Every unit on it is restorable by its tenant with that tenant's
   key and standard tools — `mt`, `dd`, `age`, `dar`, `sha256sum` and a few
   coreutils (§6) — with no tapectl and no database (`RESTORE.sh`, tape file 2). This is the heir path
   and it is proven on real hardware. On a tape that ended early, "every unit on it" is every
   unit wholly at or before the seal's `ended_after` (§4.2). A spanning version of which a tape
   holds only a range (1.7.0) is restorable from that tape together with the tapes that hold its
   other slices, and from no tape alone (§4.4).
2. **Catalog.** With the operator or escrow key, `catalog rebuild --from-volume`
   reconstructs everything the catalog knew about this write *at staging
   time*: volume identity, units, snapshots, the slice map and plaintext
   hashes (the envelope `MANIFEST.toml`), tenant ownership, each stage set's
   recorded recipient list and the per-file index (the operator envelope's
   `catalog.db`, §1.4). Tapes written before 2026-09-11 carry an older
   `catalog.db` without ownership or receipts; the rebuild takes ownership
   from the tenant envelopes and reports the receipt as unknown. Tapes
   written before 1.1.0 record no file kinds or link targets; the rebuild
   records their non-directories as regular files. A rebuild from one tape
   of a span records what that tape shows: every slice of the set, and
   positions only for its own (1.7.0, §4.4).
3. **Never on the tape:** where the cartridge is, whether it has been
   verified and when, what policy applies to a unit, warehouse deposits, the
   cleaning-cartridge registry and its events, each write's feed ratio
   (1.1.1), and any fact the catalog knows only because the operator did
   something after the write. Those survive a lost database only through `db backup` and the
   Heir Kit (ADR-0009).

**Accepted disclosure (operator ruling, 2026-07-22).** Encryption overhead is
deterministic and compression is off, so an on-tape size approximates the content
size it encloses — and at fine unit granularity (one folder = one unit = one slice,
the media-library shape) the size column effectively discloses per-unit content
sizes and unit boundaries, which a correlator could in principle match against
publicly known media sizes. The operator reviewed this size-fingerprint channel and
**accepted it** (out of threat model; quantized padding was considered and declined
as weak mitigation for real capacity cost). The system guide's disclosure section
must state the inference plainly (rides the #22 guide regeneration). An earlier
wording of this invariant forbade "plaintext-content sizes" while permitting
"on-tape sizes" — a distinction that collapses at one-slice-per-unit; this ruling
replaces it.

**Envelope ordering.** Because the front index publishes a ciphertext hash *per
position*, envelope order must not become a side channel that re-attaches a hash to
a tenant. Non-attributability holds regardless (no tenant identity is in plaintext,
and each hash is over pseudorandom age output), but to also defeat positional
correlation the write path SHOULD order the tenant envelopes by a **deterministic
permutation seeded by `volume_uuid`** — deterministic so Layout construction stays
reproducible (layout-session.md), UUID-seeded so the order is not the raw
`tenant_id` sequence. *Status:* the shipped v1 write path orders by `tenant_id`; the
permutation lands with the v2 write flip (#22/#24). Until then the invariant above
still holds — the drift is positional-correlation hardening, not a plaintext leak.
The tenant backups (§1.3) repeat the primaries' permuted order, and each carries
its primary's hash, so a backup is linkable to its primary and to nothing else:
the pairing says which two files are one envelope, never whose.

This tightens — and deliberately relaxes one clause of — the v1 rule ("no
sizes/hashes in plaintext"), which the v1 format already violated by shipping
plaintext per-file sizes in the mini-index. The relaxation (ciphertext hashes in
plaintext) is **decision D4**, ratified 2026-07-21: a hash of age ciphertext is a
hash of pseudorandom AEAD output, reveals nothing about plaintext, is not
attributable to a tenant, and is recomputable by anyone with the tape — so storing
it leaks nothing an observer could not already derive. `sha256_plain` (the hash of
*decrypted* content) stays encrypted-only.

**The invariant is machine-checked.** `tests/mhvtl_e2e.rs::mhvtl_no_plaintext_tenant_metadata`
(mhvtl-gated) writes a volume with sentinel tenant/unit names and scans every
plaintext file for leaks. For v2 it MUST be updated to: (a) treat
`{0, 1, 2, 3, seal_marker}` as the plaintext positions; (b) scan plaintext files
for the sentinel **filenames and the expected `sha256_plain` hex** as well as
tenant/unit names, asserting all are absent; (c) permit `sha256_encrypted` and
on-tape sizes to appear; and (d) reflect the v2 order (envelopes precede slices).
This test is D4's real enforcement — the prose above is only its statement. On a
short tape (§4.1) the plaintext positions are `{0, 1, 2, 3, ended_after + 1}`, and
the scan covers the short seal like a full one (#420).

## 3. The front index (File 3)

Plaintext TOML. File 3 is the **plan**: it is written before any slice, so it lists every
file the write meant to put on the tape, whether or not the tape ends early (§4.1).

**The `[index]` table.** `volume`, then `layout_version = 2`, then, from 1.6.0, on every
tape 1.6.0 writes (ADR-0012 item 111):

```toml
[index]
volume = "{label}"
layout_version = 2
may_end_early = true
```

- `may_end_early = true` is the literal line, directly after `layout_version`. It declares
  that the tape may end before the last file listed, and that the seal says where. Absent
  means false (every tape before 1.6.0).
- **It decides nothing for a reader.** `requires` (File 0, the seal) and the seal's
  `ended_after` do (§1.2, §4.1). File 3 carries no `requires`: no reader before 1.6.0 checks
  it there, so it could fence nothing.
- The human-readable header of a 1.6.0 File 3 gains one paragraph, after the paragraph that
  ends "see the system guide." and before the `MACHINE-READABLE DATA` rule, exactly:

  ```
  This tape may end early. The list below is the plan: every file the
  write meant to put on the tape. The seal marker, the LAST file on the
  tape, says where the tape really ended (its ended_after). A file listed
  after that position is not on this tape.
  ```

Then one entry per planned tape file, in position order:

- `position` — tape file number (0..M). Present for **every** file (navigation is
  total: the heir can jump straight to any file, including the seal marker). On a
  short tape, positions after the seal's `ended_after` are planned files that are not
  on the tape, and the `seal_marker` entry at M is the planned seal (§4.2).
- `type` — on-tape zone kind: `id_thunk`, `system_guide`, `restore_sh`,
  `front_index`, `tenant_envelope`, `operator_envelope`,
  `operator_envelope_backup`, `tenant_envelope_backup` (1.1.0, §1.3),
  `data_slice`, `seal_marker`. **No** tenant/unit identity — a `data_slice`, a
  `tenant_envelope` and a `tenant_envelope_backup` entry are unlabeled beyond
  kind.
- `size_bytes` — the true (pre-block-padding) byte length, so the heir can
  `head -c $size_bytes` to trim the 512 KB block padding before decrypting. Present
  for every file **except File 3 itself and the seal marker** (corrected
  2026-07-22: an earlier draft excluded only File 3). File 3's length is
  self-referential, and listing the seal marker's size here while the seal's
  embedded copy lists File 3's size would create a needless mutual-reference
  fixpoint (each size depends on the other's digit count). Neither exclusion
  costs a reader anything: both tail files are plaintext, filemark-delimited,
  and recovered by reading the whole tape file and stripping trailing zero
  padding — the trim contract exists for the *encrypted* files.
- `sha256_encrypted` — hex sha256 of the file's **on-tape bytes** (ciphertext for
  encrypted zones; the plaintext bytes for File 0/1/2). Present for every file
  **except File 3 itself** (self-reference) **and the seal marker** (not yet written
  when File 3 is emitted). For data slices this value is taken verbatim from
  `stage_slices.sha256_encrypted`, computed once at stage time — it is **not**
  recomputed while building the index (finding ③: no double read of the slice bulk).

Because File 3 carries content hashes, every envelope and slice must be fully
materialized and its `sha256_encrypted` known **before** File 3 is generated. This
is already true: write.rs encrypts envelopes up front (to fix their sizes) and slice
ciphertext hashes exist from stage time. Plaintext File 0/1/2 are small and hashed
directly when the index is built.

## 4. The seal marker (File M) and the integrity chain

Plaintext TOML, one tape file, written last: a `[seal]` section — `{ volume,
layout_version, requires, file_count = M+1, sealed_at (RFC 3339), front_index_sha256 }`,
and from 1.6.0 `ended_after` (§4.1) — followed by a **full embedded copy of the front
index** (`[[files]]` entries in the same grammar as File 3). Its **presence** is the
assertion "every file before me is present and this is a sealed volume"; its **absence**
means the tape is self-evidently unsealed (interrupted or aborted — see
layout-session.md). From 1.6.0 the seal may come early: a **short seal** at K+1, whose
embedded copy stops at itself (§4.1). Everything this section says of the seal holds of
a short one, over the files it covers.

**The embedded copy** (ratified 2026-07-22) mirrors LTFS's front-copy-plus-end-index
redundancy without partitioning: damage at either end of the tape is survivable —
front damage → recover the whole map from the tail; tail damage → the tape reads as
unsealed (fail-safe) but stays fully navigable from the front. The copy is strictly
*more* complete than File 3: by seal time File 3's bytes are known, so the copy
carries File 3's own `size_bytes` + `sha256_encrypted` (only the seal marker's own
entry stays hash-less — self-reference). The copy is not itself hash-protected
(nothing on the tape hashes the last file); its per-file claims are validated the
same way File 3's are — by hashing the files they describe — and it must pass the
§5 self-consistency checks before being trusted.

**The `front_index_sha256` byte contract (normative — cross-tool):** the hash is
computed over File 3's **exact generated bytes** — the unpadded TOML text as
materialized at build time, no added trailing newline, no block padding. A reader
recovering File 3 from a padded tape read obtains the same bytes by stripping
trailing NUL padding (equivalently `tr -d '\0'`, since the generated text contains
no interior NULs); the Rust side truncates to the Layout's recorded File-3 size.
Seal generation, `Store::confirm`, and RESTORE.sh (`--info`/`--verify`) MUST all
use this definition — a divergence here fails only at the cross-tool parity test,
with no earlier signal.

The seal marker is the **unhashed root of trust**. Nothing on the tape carries the
seal marker's own hash (it is last, tiny, and self-delimiting). From it, integrity is
a keyless chain, verifiable with only `dd` + `sha256sum`:

```
seal marker  ──front_index_sha256──►  File 3 (front index)
File 3       ──sha256_encrypted[i]──►  every content file i  (0,1,2, envelopes, slices)
```

Verifying the chain proves byte-integrity of the entire tape without any key:
hash File 3, compare to the seal marker's `front_index_sha256`; then for every other
file, hash it and compare to File 3's `sha256_encrypted[i]`. This is what "confirm"
verifies (§5) and what an heir can independently re-run years later for bit-rot
detection. On a short tape the chain covers files 0 to `ended_after` (§4.2).

### 4.1 The full seal and the short seal (normative, tapectl 1.6.0)

Ruled 2026-10-20 (ADR-0012 items 9, 99, 110-115; #420). From 1.6.0 every seal states
`ended_after`, the position of the last file before it. A seal is one of two kinds:

| | Position | `file_count` | `ended_after` | Embedded `[[files]]` entries |
|---|---|---|---|---|
| **full seal** | M (planned) | M+1 | M-1 | 0 to M, as before 1.6.0 |
| **short seal** | K+1, with K ≤ M-2 | K+2 | K | 0 to K, then its own entry at K+1 |

Normatively, `ended_after` is the position of the last file before the seal. In the usual
case K is the data slice in which the drive's early warning came (§4.3). A resume after a
crash can seal lower, when the medium lost files the catalog had recorded written
(`layout-session.md`, "The short ending").

**Grammar.** One `key = value` per line (the line-oriented contract of
`v2-open-questions.md` §3.1), in exactly this order, for both kinds:

```toml
[seal]
volume = "{label}"
layout_version = 2
requires = {requires}
file_count = {file_count}
ended_after = {ended_after}
sealed_at = "{sealed_at}"
front_index_sha256 = "{front_index_sha256}"
```

- `ended_after` is the one new key. It sits directly after `file_count`, which it is
  bound to (rule S1 below).
- `requires = ["short-seal"]` is on **every** 1.6.0 seal, full or short, as it is in File
  0 (§1.2). The seal has always repeated File 0's `layout_version` and `requires`. It is
  the fence when File 0 cannot be read. From 1.7.0 the seal of a span tape states
  `requires = ["short-seal", "spanning"]`, File 0's line, spelled as §1.2 rules (ADR-0012
  item 124). No other line of the grammar changes for a span tape (item 126).
- `front_index_sha256` is the hash of File 3's exact bytes (the byte contract below), on
  both kinds. File 3 is the plan and never changes: a short seal binds the same front index
  a full one would have.
- `sealed_at` is the fixed-width timestamp (`YYYY-MM-DDTHH:MM:SSZ`, 20 bytes) as before.

**The embedded copy.**
- **Full seal:** entries 0 to M, exactly as before 1.6.0. File 3's own entry carries its
  `size_bytes` and `sha256_encrypted`; the seal's own entry, last, carries `position` and
  `type` only.
- **Short seal:** entries 0 to K copied from File 3, with File 3's own entry filled in the
  same way, then the seal's own entry:

  ```toml
  [[files]]
  position = {K+1}
  type = "seal_marker"
  ```

  with no `size_bytes` and no `sha256_encrypted` (self-reference, as on a full seal). So
  entries 0 to K equal File 3's entries 0 to K in `position`, `type`, `size_bytes` and
  `sha256_encrypted`, except File 3's own entry, which the copy fills in. The entry at K+1
  differs from File 3's: File 3 plans a data slice there.

**The human-readable header** is the same on both kinds, so it does not change a seal's
length. From 1.6.0 it reads, exactly:

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

**Length.** The build step sizes the seal with a placeholder (`v2-open-questions.md` §9):
the full seal, with `ended_after = M-1` and a fixed-width `sealed_at`. A full seal must be
exactly the placeholder's length, as before 1.6.0. A short seal is never longer: its
`file_count` (K+2 ≤ M+1) and `ended_after` (K ≤ M-1) have no more digits, its own entry's
`position` (K+1 ≤ M) has no more digits, and it carries at least one data-slice entry fewer.
The writer checks that a short seal is no longer than the placeholder, never that it is
equal. The placeholder states the tape's `requires` (§1.2), as the real seal will, so on a span
tape the second feature's bytes are in both.

**Parse rules.** Every reader that knows `short-seal` (`format.rs`, RESTORE.sh) keeps these:

- **S1.** `ended_after` is a non-negative integer and `ended_after + 2 == file_count`. The
  seal's own position is `file_count - 1`, which is `ended_after + 1`. This holds on both
  kinds: it validates a seal, it does not classify one.
- **S2.** A seal whose `requires` names `short-seal` carries `ended_after`. A seal that
  carries `ended_after` keeps S1, whatever its `requires` says.
- **S3.** A seal with no `ended_after` (every seal before 1.6.0) is a full seal that ended
  after `file_count - 2`.
- **S4.** The embedded copy has exactly `file_count` entries: positions 0 to
  `file_count - 1` in order, one `front_index` entry at 3, one `seal_marker` entry, last
  (the self-consistency checks of `v2-open-questions.md` §2.5, unchanged).
- **S5.** The embedded entry at position `ended_after` has `type = "data_slice"`. That holds
  on a full seal (M-1 is the last data slice) and on a short one (a writer never seals short
  before the first data slice, §4.3), and fails a seal that ends inside the metadata. Every
  Layout tapectl writes has at least one data slice: `volume write` refuses a session with
  nothing staged.
- **S6.** A seal that breaks S1, S2, S4 or S5 is **unparseable**. By the fail-safe
  precedence (`v2-open-questions.md` §2.5) that is the unsealed signal: such a seal never
  reads as sealed.

**Classification.** A seal is **full** when its `file_count` equals the plan's count (File
0's `total_files`, File 3's entry count, or the Layout's), and **short** when it is less. A
`file_count` greater than the plan's means the tape's two ends disagree, as a
`front_index_sha256` mismatch does. A reader with neither File 0 nor File 3 cannot classify
the seal and needs not: its embedded copy describes what is on the tape.

**No unit identity.** A short seal carries what a full seal carries: positions, kinds,
on-tape sizes and ciphertext hashes. It adds one position, `ended_after`. Nothing in it
names a unit, a tenant or a file, or says which unit the end of the tape cut; only an
envelope the reader can open tells that (§2). The leak scan covers it (§2).

### 4.2 What a short tape promises, and what readers do with it (normative)

**What it promises.**
- **It is sealed.** The short seal asserts that every file from 0 to `ended_after` is
  present, and binds File 3 as a full seal does. The keyless chain runs over those files:
  seal → File 3 → every file at or before `ended_after` except File 3.
- **Every unit wholly at or before `ended_after` is on it whole**: each such unit's every
  slice is at a position at or before `ended_after`. It is restorable by the heir path, and
  the catalog counts it as a Copy (`writes.status = 'completed'`).
- **Every position after `ended_after` is not on this tape.** A unit with any slice planned
  after `ended_after` is **cut** (CONTEXT.md "Cut"): the unit the early warning interrupted
  and every unit planned after it. Nothing of a cut unit counts on this tape, and no reader
  restores it from this tape, even though its slices at or before `ended_after` are present
  and hash correctly. The catalog records its row `cut` (ADR-0012 item 112), and the unit is
  written whole on another cartridge (item 9). From 1.7.0 a spannable set (its block states
  `may_span = true`) with at least one slice at or before `ended_after` is not cut: this tape
  holds a range of it (§4.4).
- **Everything written before the slices is the plan.** File 0, File 3, every envelope's
  `MANIFEST.toml`, `RECOVERY.md`, `PLAN.toml` and `catalog.db`, and the `index/` members
  (§1.5) list the cut units and the positions after `ended_after`. The seal withdraws those
  claims. In particular, an `index/units.tsv` row whose `state` is `this-tape` names a cut
  version that this tape does not hold. A reader checks a version's slices against
  `ended_after` before it calls this tape a holder. The indexes of later tapes never name a
  cut write as a holder: §1.5 lists only completed writes (from 1.7.0, completed and `range`
  writes).
- File 3's hashes for positions after `ended_after` describe files that are not on this tape.
  No reader compares them with anything on it.
- **Nothing follows the seal.** End of data comes directly after its closing filemark.

**What readers do on a tape that requires `short-seal`** (tapectl and RESTORE.sh alike):
- **R1. Learn `ended_after` before reading any data slice.** tapectl takes it from the
  catalog for a volume this catalog sealed (`volumes.ended_after`; NULL means a full seal, so
  the planned M-1), and otherwise reads the seal (§1.2 rule 6). A full seal gives
  `ended_after = M-1`: nothing is withdrawn.
- **R2. Without a readable seal,** read on as for an unsealed tape (§6 step 3). END OF DATA
  met at a planned position is the end of the tape: that file and every later one are not on
  it.
- **R3. Never read a position after `ended_after` for data.** Report it as not on this tape.
- **R4. Name a cut unit; never restore it from this tape.** Say where its copies are when the
  index says (§1.5). A range is not a cut unit: §4.4's rules R6-R9 read it.
- **R5. A second tape is read the same way.** A reader that takes slices or files from
  another tape (RESTORE.sh's `--swap-tapes`, `--slices-from` and `--fill-from`, tapectl's
  read-from-another-copy, #480) reads that tape's seal first and takes nothing from past its
  `ended_after`.

RESTORE.sh's modes, in 1.6.0:
- `--info`: the verdict stays SEALED for both kinds. A short seal adds a line saying that the
  tape ended after file K and that files K+1 to M-1 of the plan are not on it.
- `--verify`: hashes files 0 to `ended_after` and prints `NOT ON TAPE` for each planned
  position after it. A `NOT ON TAPE` line is not a failure.
- `--restore`, with or without `--path`: refuses a cut unit by name, saying the tape ended
  before its slices and naming `--where`. `--all` restores every whole unit and names the cut
  ones.
- `--list`: still lists a cut unit's files (its catalogue is in the envelope, no slice is
  read), and says the unit is not on this tape whole.
- `--where` and `--where --all`: a `this-tape` row of a cut version is printed as cut, not as
  a holder, and the loaded tape does not count as holding that version.
- `--find-envelope`: unchanged. Every envelope is before the first data slice.

From 1.7.0, `--info`'s file map and `--verify` print **one** `NOT ON TAPE` line for the planned
files past the end of every short tape, span or not, instead of one line per position (ADR-0012
item 140). The line names the first and the last planned position after `ended_after` (K+1 and
M, the planned seal entry, which the file map and `--verify` list; `--info`'s sentence above
counts the planned files to M-1) and says that the seal is file K+1. It is still not a failure.
`--verify` prints it unindented and `--info` indents it as its file map:

```
NOT ON TAPE  files K+1 to M  (planned; the tape ended after file K, and its seal marker is file K+1)
```

RESTORE.sh's other modes on a range are §4.4's.

tapectl, in 1.6.0:
- The chain walk (confirm, `volume verify`) covers files 0 to `ended_after` (§5).
- Every reader that reads slices by the catalog (`restore`, `volume read-slices`, `volume
  compact-read`, `volume rewrite`) never reads a `cut` write, so none reads a cut unit from
  this tape. (`restore` and `volume rewrite` read only `completed` writes; `volume read-slices`
  keeps its 1.5.0 rule of any written position, minus `cut` ones.) `restore volume` names the
  cut units. (From 1.7.0 `restore` also reads `range` writes, across cartridges: §4.4.)
- `restore raw-volume` reports the positions after `ended_after` as not on this tape. On a
  tape with no seal it accepts, END OF DATA proven by the drive at a planned position ends the
  dump there (R2): that position and every later one are not on the tape (`--json`:
  `end_of_data_at`). Any other read failure fails the dump.
- `catalog rebuild` and the adoption of a lost write (`volume::adopt_lost`) locate the seal
  by §1.2 rule 6. They record `completed` only for a stage set wholly at or before
  `ended_after`, and `cut` for every other. (From 1.7.0 a spannable set takes §4.4's split:
  `completed`, `range` or `cut`.)

### 4.3 The writer at the early warning (normative)

Ruled 2026-10-20 (ADR-0012 items 84, 99-100, 110-114), from the st driver's measured
behaviour on home2's HP LTO-6 (`docs/runs/2026-10-20-lto6-profile-and-lbp.md`). The session's
states and records are in `layout-session.md` ("The short ending"); this section fixes what
reaches the tape.

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

A plan may reach past the fill ceiling. It may list whole units up to 100% of the cartridge's
nominal capacity, but only when the drive declares `post_warning_bytes` (ADR-0012 item 113).
Units planned past the warning's point are cut (§4.2). Without that key the plan stops at the
fill ceiling. An unexpected early warning still seals short.

From 1.7.0 a tape that holds a spannable set plans the rest of that set past item 113's bound,
and the early warning ends its range (ADR-0012 items 84 and 123): W4 applies unchanged, and the
slice in flight finishes before the short seal. This too needs `post_warning_bytes`. Without it,
a spannable set that would pass the fill ceiling is refused, and the refusal names the key.
Whole units planned after a span's tail keep item 113's bound. The plan order is
`layout-session.md`'s ("Spanning").

### 4.4 A range (normative, tapectl 1.7.0)

Ruled 2026-10-16 to 2026-10-21 (ADR-0012 items 82-95 and 121-143, #478). A unit that allows
spanning may have its version's slices on more than one tape. The run of one stage set's slices
on one volume is a **range** (CONTEXT.md "Range"). A volume holds at most one unbroken range of
a stage set, in slice order and at consecutive positions (item 122). Only a spannable set
(§1.2: its write was planned with `writes.may_span` set) is ever held as ranges. The writer's
side, the plan order and the catalog's records, is `layout-session.md`'s ("Spanning"). The drive's
early warning ends a range as it ends any tape (§4.3, item 84).

**Without a key, nothing new is readable** (item 126). A range's slices are `data_slice` entries
of File 3 like any other. The front index has no new entry type, and the seal, its grammar,
rules S1-S6 and the short seal's golden pin are 1.6.0's. The one plaintext difference is
`spanning` in the `requires` of File 0 and the seal (§1.2). It says that the tape's plan holds a
slice of a set that may span, never which set, which slices or whose. The leak scan (§2) covers
a span tape as it covers any other.

**The `[[units]]` block of a spannable set** (items 124-125). Every envelope that lists the set
(tenant and operator envelopes, and their backups, which are byte copies, §1.3) carries this
block on every tape whose plan holds a slice of the set. On a tape whose plan holds slices K+1
to N, it reads:

```toml
[[units]]
name = "{unit}"
uuid = "{unit_uuid}"
snapshot_version = {version}
stage_set_id = {stage_set_id}
may_span = true
dar_version = "{dar_version}"
dar_command = "{dar_command}"

[[units.slices]]
number = 1
size_bytes = {size_bytes}
encrypted_bytes = {encrypted_bytes}
sha256_plain = "{sha256_plain}"
sha256_encrypted = "{sha256_encrypted}"

(slices 2 to K the same way, with no tape_position)

[[units.slices]]
number = {K+1}
tape_position = {position}
size_bytes = {size_bytes}
encrypted_bytes = {encrypted_bytes}
sha256_plain = "{sha256_plain}"
sha256_encrypted = "{sha256_encrypted}"

(slices K+2 to N the same way, each with its tape_position)
```

- `may_span = true` is the literal line, directly after `stage_set_id` and before the optional
  `dar_version` and `dar_command`. It is in the block of every spannable set on every tape whose
  plan holds a slice of it, whether that plan holds the whole set or a run of it. Absent means
  false. A block without it is a whole-unit block, byte-identical to 1.6.0's, and every slice in
  it has a `tape_position` (item 125).
- The block lists **every slice of the stage set**, 1 to N, in slice order, each with its
  `size_bytes`, `encrypted_bytes`, `sha256_plain` and `sha256_encrypted`. So any tape of a span
  gives every slice's ciphertext hash: a reader can accept any other tape of the span by hash
  (item 88), and a rebuild records every slice (item 130).
- **`tape_position` is given only to the slices this tape's plan holds**, as each slice's second
  key, where a whole block has it. For every other slice the line is absent: never `0` (File 0's
  position) and never empty. The slices given a position are one unbroken run in slice order, at
  consecutive tape positions.
- The name `may_span` collides with no other key of the manifest, so RESTORE.sh's table-blind
  `toml_val` and its awk readers cannot take it for another (as for `may_end_early`, §1.7).
- `[manifest]` is 1.6.0's, `may_end_early = true` included. A second pinned case in
  `tests/on_tape_golden.rs` covers a range block (item 125), and the pin of whole blocks does not
  move.

**What a tape holds of a spannable set.** With a key, a reader takes the set's slices that have
a `tape_position` at or before the seal's `ended_after` (item 126; on a full seal `ended_after`
is M-1, §4.1). Then:
- **All N slices of the set:** the tape holds the version **whole**, as it holds a whole unit.
  The version restores from this tape alone, and the tape is a copy of it
  (`writes.status = 'completed'`). Item 93's middle tape is this case.
- **Some, not all:** the tape holds a **range**, the run from the lowest of them to the highest
  (`writes.status = 'range'`). The other slices are on other tapes: those with no
  `tape_position` were never planned on this one, and those after `ended_after` were planned but
  the tape ended first.
- **None:** the set is **cut** on this tape (§4.2), and nothing of it is on the tape.

A block without `may_span` keeps §4.2's rule: whole when its last slice is at or before
`ended_after`, cut otherwise. This split is what confirm records (`layout-session.md`, "Confirm
of a short ending"). `catalog rebuild` and the adoption of a lost write record the same split,
so a rebuild records what confirm did (ADR-0012 item 136's proof 1 expects a `completed` row on
item 93's middle tape). Item 130's "the row is `range` when the block says `may_span`" is read
this way: `range` when the tape holds some of the set's slices, `completed` when it holds all,
`cut` when it holds none.

**What a range promises.**
- Every slice of the range is on the tape, and the keyless chain covers it (seal, File 3, each
  slice: §4).
- A range is not a copy on its own. Copies are counted slice by slice, and a version is as
  covered as its least-covered slice (items 76 and 127; `layout-session.md`). A tape that holds
  a range restores the version only together with tapes that hold its other slices.
- A tape never names a tape written after it (ADR-0001, item 126). The first tape of a span says
  only that the rest of the set is on a later tape: its envelope lists the slices, and its index
  cannot say where they are. A later tape's `index/units.tsv` names the earlier holders, with
  their runs (§1.5).

**Reader rules for a range** (tapectl and RESTORE.sh alike, beside §4.2's R1-R5):
- **R6. A slice with no `tape_position` is on another tape.** It is never read from this tape
  and never reported as damage. A slice whose position is after `ended_after` is not on this
  tape either (R3).
- **R7. A range is restored only with the version's other slices.** A reader restores a spanning
  version from one tape that holds it whole, or from several tapes in slice order, slice 1
  first. It never runs dar over one tape's range alone.
- **R8. Another tape is accepted for slice S by hash alone, without a key.** It supplies slice S
  only when its front index (File 3) lists a `data_slice` whose `sha256_encrypted` equals slice
  S's in the manifest, at a position at or before that tape's own `ended_after`, read from its
  seal before any slice (R5). A tape with no readable seal, or whose seal does not cover that
  position, is not accepted (ADR-0012 item 132). The reader then takes slices S+1, S+2 and on
  from that tape for as long as its front index lists each by the same rule. A tape's label, its
  index or another envelope's claims never stand in for the hash. The tape that supplies a slice
  need not hold a range: a tape that holds the version whole supplies any slice of it (item
  139).
- **R9. Every slice is checked against the front index of the tape it was read from** before it
  is used, as every slice is (§4).

**RESTORE.sh** (items 88, 132 and 140; ADR-0012 item 135 authorizes the re-pin):
- **Where a restore starts.** A spanning unit is restored (`--restore`, with `--unit NAME` or as
  the lone unit) starting from a tape that holds its slice 1: one that holds the version whole,
  or one whose range starts at slice 1. Started on a tape whose range starts after slice 1,
  RESTORE.sh refuses before it reads a slice, names the first slice this tape holds, and names
  the holders of slice 1 when an index the key opens names them (`--where`). The tapes are then
  read in slice order. A tape that holds the version whole restores it alone, as any whole
  unit, with no swap and no scratch.
- **The default mode keeps ciphertext in scratch** (item 132). For each tape in turn, RESTORE.sh
  reads that tape's slices of the version, checks each by R9, and keeps it still encrypted in
  the scratch directory (inside `--to`, or `--scratch DIR`, the area `--path` already uses). When
  the tape's run ends before slice N, it names the slice it needs next and the holders that an
  index it has opened names, and waits for the heir to load another cartridge and press ENTER,
  as `--swap-tapes` does. It accepts that cartridge only by R8, and otherwise says why and asks
  again. When slices 1 to N are all in scratch, it decrypts them in slice order into one dar run,
  through pipes as on one tape (§6 step 5), so only ciphertext is ever at rest.
  - Before the first slice is read, the space check counts the version's whole ciphertext for
    the scratch (the sum of every slice's `encrypted_bytes` in the manifest), as well as the
    files for `--to`. When the room is not there it refuses and names `--stream-tapes`. It never
    chooses the stream itself: item 88 leaves that to the heir.
  - The mode is one run of the script. It waits at each swap for as long as the heir takes, and
    a run that is killed leaves its scratch directory to be removed by hand, as any killed
    restore does.
- **`--stream-tapes`** feeds one dar run through pipes across the swaps instead: no slice is
  kept on disk, and the script pauses at each tape's end for the next cartridge, accepted by R8.
  Whether dar waits on a pipe across a cartridge change, for as long as a swap takes, is
  unverified. The mhvtl gate's span leg settles it (items 132 and 136).
- `--restore --path` of a spanning unit reads the slices its catalogue names for the path, and
  the last, in slice order, from whichever tapes hold them, accepting each tape by R8.
- `--restore --all` restores every unit this tape holds whole, and names each spanning unit of
  which it holds only a range, with the `--restore --unit` command that restores it across tapes
  (item 88).
- `--list` lists a spanning unit's files from its catalogue, as for any unit, and says which of
  its slices this tape holds.
- `--where` and `--find` print a holder's slice run when it is not the whole version. A holder of
  a range does not count on its own as holding the version, as a cut `this-tape` row does not
  (§4.2).
- `--slices-from` and `--swap-tapes` (#480) accept a replacement slice from any tape by R8, a
  range included. `--fill-from` takes files only from a tape that holds the version whole: a
  range is never a fill source, and the refusal says why (item 143).
- `--info` and `--verify` read without a key and see a range only as data slices. They are
  unchanged but for item 140's one `NOT ON TAPE` line (§4.2).

**tapectl** (items 89, 101, 121 and 127-133):
- `restore unit` and `restore file` of a spanning version choose the fewest cartridges in slice
  order, a live volume that holds the version whole first. They ask for each cartridge by label,
  sticker and chip serial (the `supply` machinery and `holders_of`, which include `range` writes
  from 1.7.0) and stream across the swaps (item 131). `--from` must name a cartridge that holds
  slice 1; with no `--from`, the loaded tape must hold it. A version with any slice on no live
  volume is refused, by name, before the first cartridge is asked for. `--spool` goes through
  scratch space first instead, as it does on one tape.
- `restore volume` restores the whole units on one tape and names the spanning units of which
  the tape holds only a range (item 89).
- A damaged slice is read from another holder of the same slice, a range included (item 101). A
  damaged file is filled only from a whole copy (item 143).
- `catalog rebuild --from-volume` of one tape of a span records what that tape shows (item 130):
  every slice row of the set from the manifest, the write `completed`, `range` or `cut` by the
  split above, and positions only for the slices this tape's plan holds. The version reads no
  copies until its other tapes are rebuilt, and stays as confirm would leave it (item 128,
  `layout-session.md`). The to-do list names the slices the rebuilt tapes do not hold, and their
  tapes when a newer index names them (§1.5). The adoption of a lost write (#360) accepts a
  `range` row.
- `volume verify` marks a `range` write read back in full as it marks a `completed` one (item
  127). `volume rewrite` refuses a volume that holds a range, naming the range (item 133), and
  never writes a range itself (item 142). Compaction (`volume compact-read`, `volume
  compact-finish`, `volume compact`) refuses a volume that holds a range in the same way.

**File 1 and RECOVERY.md** (item 135):
- **File 1**, on every tape 1.7.0 writes (any tape of a box may be part of a span): its glossary
  entry for `requires` names both features. A section on a unit across tapes says what a range
  is; that a restore needs every slice from 1 to N, in slice order; how RESTORE.sh's two modes do
  it and what each needs (scratch for the unit's whole ciphertext, or the cartridges at hand for
  the stream); that the restore starts from the tape holding slice 1, which `--where` on the
  newest tape names; and that a tape is accepted by hash. Its query of the Heir Kit's catalogue
  lists `range` writes, with their slice runs, beside whole copies (item 141).
- **RECOVERY.md**, in an envelope that lists a block stating `may_span = true`: a section on a
  unit across tapes, after the section on reading the seal (§1.7). It says which slices of each
  such unit this tape's plan holds; that the slices with no tape position, and those after
  `ended_after`, are on other tapes; that a restore needs every slice, in slice order, from the
  tapes that hold them, matched by the slices' `sha256_encrypted`; and that RESTORE.sh does it.
  The unit's own section gives the manual steps for the slices this tape holds.

## 5. What "confirm" verifies (byte level)

The write-session **states and sequence** live in layout-session.md; this section
fixes only the *cryptographic content* of the confirm pass, because it is a property
of the format. Confirm is a **single forward pass from BOP** (no seek-back — the
index is at the front):

1. Read the seal marker (the last file, located by §1.2 rule 6: at `ended_after + 1`
   when the catalog records the volume short). Absent ⇒ not sealed; confirm fails.
   It is parsed by §4.1's rules S1–S6.
2. Hash File 3; compare to the seal marker's `front_index_sha256`. Mismatch ⇒ the
   tape's two ends disagree ⇒ quarantine.
3. Parse File 3; diff its `{position, type, size_bytes}` entries against the Layout
   ⇒ **navigable** tier. File 3 and the Layout are both the plan, so they are
   compared whole on a short tape too.
   - *(1.6.0)* The seal's `ended_after` must equal the catalog's: the session's
     ending for a write's confirm, `volumes.ended_after` for `volume verify` (NULL
     means the planned M-1). A disagreement is the mismatch kind
     `ended_after_disagrees`. It says nothing about the medium, so the confirm is
     **inconclusive** (never a quarantine), as for any mismatch that does not prove
     the medium bad.
4. For every file at or before `ended_after` except File 3 and the seal marker, read
   and hash it, compare to File 3's `sha256_encrypted[i]` ⇒ **integrity** tier.
   Positions after `ended_after` are not read (§4.2).

**The readback past the early warning** (1.6.0, ADR-0012 item 114). A write session
that saw the early warning in a data slice (§4.3 W4) always reads back the entry at
`ended_after` and the seal, and hashes that entry against File 3, whatever the tier.
At most one slice and the seal. Under the integrity tier this is already part of
step 4; under the navigable tier it is the one content file the confirm hashes. A
session that did not see the warning confirms as before.

**Which tier runs depends on the caller** (ADR-0012, amendment 2026-10-06 item 1,
#387; v2-open-questions.md §1.2's reversal and §2.4 L3):

- **A write's post-seal confirm is navigable by default** (steps 1–3: File 3 and the
  seal marker, plus the readback past the early warning above when there was one), and a
  passing one seals. `--full-confirm`, on every command that
  writes a volume, opts in to the integrity tier. What the default gives up is L3 at
  write time — the one host → HBA → drive → medium check (with LBP unreachable
  through `st`, the readback hash is the only control spanning that chain) — so
  `audit` (`no_full_verify`) and `report verify-status` name every sealed volume with
  no passed full readback recorded until `volume verify` runs one.
- **`volume verify` runs the integrity tier by default**; `--quick` opts down to the
  navigable tier and is recorded honestly. A quick verify does not stand in for a
  full one.

The integrity tier was the seal default from 2026-07-22 to 2026-10-06, on the
asymmetry of when a bad copy is found: at seal time the staged slices still exist on
disk, so a failed readback costs a fresh cartridge and hours — discovered years
later, the source may be gone. That argument is why `--full-confirm` exists and why
the operator guide asks for a full `volume verify` soon after each write, while
the staged slices are still held (`--keep-staging` on the write that completes a
unit's copies: since 1.1.1 that write releases staging itself otherwise; on home2
a full readback cost ~2.3 h per tape). The tiers map onto the existing
`verification_sessions.verify_type` column: integrity → `full`, navigable → `quick`
(no schema change).

The `verification_sessions` row records **which tier** was achieved, per ADR-0001
(recorded strength must match what ran). Drive-level Logical Block Protection is
not part of this chain — the Linux `st` data path does not expose it (ADR-0007); if
enabled out of band it is recorded only as supplementary evidence, never as the
basis of the seal.

## 6. Heir path — end to end with `mt`, `dd`, `age`, `dar`, `sha256sum` (and `head`, `truncate`, `tar`; RESTORE.sh also uses `mkfifo` and `tee`)

No database, no tapectl, no operator. This is the normative Heir Path (CONTEXT.md);
RESTORE.sh and the system guide implement it literally. **The tape is read
forward from one rewind** (#396, 1.1.0): a read leaves the head at the start of
the next file, so a reader spaces forward from where it is and rewinds only for
a file behind it (RESTORE.sh keeps a cursor and checks it against `mt status`;
any doubt is a rewind). The manual steps below read in that order.

1. `mt -f /dev/nst0 setblk 524288 && mt -f /dev/nst0 rewind && dd if=/dev/nst0 bs=512k`
   → File 0: identity, `requires` (§1.2), and "the map is File 3."
2. `mt -f /dev/nst0 fsf 2 && dd if=/dev/nst0 bs=512k` → **front index**: every
   file's position, type, size, and ciphertext hash.
3. *(completeness)* space to the last file (`mt eod`, `mt bsfm 2`, §1.2 rule 6)
   and read the **seal marker**; check `file_count` and that `front_index_sha256`
   matches File 3's hash. Absent or mismatched ⇒ the tape is unsealed or damaged —
   proceed knowing some trailing *slices* may be missing (the front index still
   says exactly which). If File 3 itself is unreadable, the seal marker's
   **embedded copy** provides the same map (validate its per-file hashes against
   the files themselves before trust). RESTORE.sh `--verify` reads the seal last,
   at the end of its one ascending pass. On a tape whose File 0 requires
   `short-seal` (1.6.0), read the seal **before any slice**: its `ended_after`
   says where the tape ended, and a file after it is not on this tape (§4.2).
4. For each envelope position: `dd` it out, check its hash against the front
   index, `age -d -i KEY` — the one that decrypts is yours ⇒ `MANIFEST.toml` +
   `RECOVERY.md` + `catalogs/` (each unit's isolated dar catalogue,
   `<uuid8>_v<version>.1.dar`: `dar -l` lists the unit's files and `-T slice`
   which slice holds each, with no slice read; #418). age's "no identity
   matched" means the key is not a recipient; any other failure after a key
   matched means the envelope is damaged. A damaged or unreadable envelope
   has a backup copy further on (§1.3: the `*_backup` entry with the same
   `sha256_encrypted`): read that one instead. RESTORE.sh does this by
   itself, and skips a backup whose primary already opened.
5. For each slice position in your manifest, in order (first checking that the
   unit's last slice is at or before the seal's `ended_after`; if it is not, the
   unit is cut and not on this tape whole, §4.2; from 1.7.0, a block stating
   `may_span = true` may hold only a range here, and its other slices come from
   other tapes, §4.4): `dd bs=512k` it out;
   `head -c size_bytes` to trim padding; `sha256sum` vs the front index's
   `sha256_encrypted` (**keyless** integrity) and/or the envelope's value;
   `age -d -i KEY` → `base.N.dar`. RESTORE.sh streams this: `dd | head | tee
   (sha256sum) | age -d > FIFO`, with every key given to age at once, so nothing
   decrypted is written to disk but the restored files.
6. `dar -x base -R /dest -O -Q -N` into an empty directory (`-N`: no darrc on
   the heir's machine changes what is restored; dar keeps an existing file and
   reports success, so a re-run over a stopped restore needs `-w`). RESTORE.sh
   runs dar with `--sequential-read` over the FIFOs, refuses a non-empty `--to`
   unless `--overwrite`, and fails on dar's "not restored (user choice)".
   For one file, `--path` reads only the slices the catalogue names for it and
   the last.

**Since 1.2.0** (ADR-0012 2026-10-16 items 71–73; the golden pin of RESTORE.sh
moved, MANIFEST.toml's did not). RESTORE.sh changed in these ways, and a tape
written before 1.2.0 carries the earlier script:

- **Tools, per mode.** A mode checks the tools it runs when it starts, before the
  first tape read, and `mt` (or `mt-st`) for every mode that reads the tape. One
  table (`RESTORE_TOOLS` in `layout.rs`) draws the script's five sets and File 1's
  list: `--info` and `--verify` need `dd`, `sha256sum` and `head`; `--find-envelope`
  adds `age`, `truncate` and `tar`; `--list` adds `dar`; `--restore --path` adds
  `tee`; a whole-unit `--restore` needs `tee` and `mkfifo` too. A bare invocation,
  `--help` and an unknown mode check nothing. Before, every mode checked every tool.
- **`export LC_ALL=C`**, right after `set -euo pipefail`, so the greps of dar's
  English output hold in any locale.
- **DAMAGED wording.** The message says "a key matched" only when a key did. When
  no key opened a damaged envelope, it says that and suggests other keys.
- **The work directory** is described as it is: the decrypted envelopes, made in
  `${TMPDIR:-/tmp}` (`--help` has a `TMPDIR` entry). Before any envelope is read,
  a WARNING says so when the free space there looks too small (the largest
  envelope plus the sum of all, from the front index); `--no-space-check` silences
  it. The `--to` figure counts the slices' encrypted size on tape, so a unit
  stored compressed may need more.
- **A lone-unit restore** (no `--unit`, no `--all`, one unit in the first envelope
  that opens) restores it as before. Before any slice is read it then reads the
  remaining envelopes forward only, and just before `RESTORE COMPLETE` says how
  many other envelopes the key opened and how many other units they list, naming
  `--list` and `--all` into a new directory. It says nothing when they add no
  units, and skips the look, with a line saying so, when the work directory could
  not hold the largest remaining envelope.
- **File 1** gains "The tape drive" (which drives read this tape's generation: an
  LTO-6 tape reads in an LTO-6 or LTO-7 drive, an LTO-8 tape in an LTO-8 or LTO-9
  drive, and a general statement for any other; the interface is SAS, some models
  Fibre Channel, through a host adapter, never USB or SATA) and "Disk space" (one
  unit's size, what `--scratch` is for, where the envelopes go) sections, gives the
  escrow restore as `--list` then `--restore --all` with the single-tenant-key note,
  and draws its tool bullets from the same table. RECOVERY.md names a unit's dar
  catalogue (the `dar -l` paragraph and the `-A` hint) only when the envelope holds it.
- **Fixed points (item 73).** RESTORE.sh's "this tape needs tapectl ≥" text stays
  "tapectl 1.1.0" and `requires` stays empty, so a 1.1.x RESTORE.sh reads a 1.2.0
  tape and a 1.2.0 one reads a 1.1.x tape.

**Since 1.3.0** (ADR-0012 2026-10-16/17 items 78-81, 101-102; the golden pin of RESTORE.sh
moved, MANIFEST.toml's did not). Two modes read the cross-volume index of §1.5: `--where UNIT`
and `--find TEXT`. Both take the `--find-envelope` tool set (one new row of `RESTORE_TOOLS`,
`TOOLS_INDEX`), read forward from one rewind, and exit 1 on a tape with no index. A tenant key
reads that tenant's `index/`, an operator or escrow key the operator envelope's `units.tsv`.
`--find` prints at most 50 matches and counts the rest. RECOVERY.md gained a section on
`index/`. File 1 was rewritten for #477's eleven findings: a glossary and one key placeholder
(`YOUR_KEY.age.key`), the non-rewinding device, `TAPE_DEVICE` as an environment prefix, commands
for every manual step, the UNSEALED and DAMAGED sections, a completeness check, "The rest of the
box", hardware, and rung 3's stopping point. Fixed points (item 81): `requires` stays empty and
`die_newer_needed` stays "tapectl 1.1.0".

**Since 1.4.0** (ADR-0012 items 105 and 107, #481; no golden pin moves: RESTORE.sh and
MANIFEST.toml are byte-identical to 1.3.0). File 1 and RECOVERY.md text only. File 1 gained "The
Heir Kit's catalogue" (opening `catalog.db.age` with the escrow key, as of the day the kit was made,
with two `sqlite3` queries), "tapectl itself" (the repository https://github.com/mikmorg/tapectl,
the release tag, `cargo build`, `init` with the escrow PUBLIC key, `catalog rebuild --from-volume
--key`, `restore unit --key`, so an escrow-only heir gets tapectl's chunk salvage and file fill),
telling a bad key file from a tape of another installation, "A renamed unit" and "Two units of one
name" (a directory per unit uuid where names recur across tapes), "When no envelope reads",
package names with one `apt` line and the oldest age tried (1.0.0-rc.1, linking age's release
page), IR and IT defined, internal versus external drives and the USB/Thunderbolt answer, the disk
chosen after the units are listed on a Linux filesystem, one disk-space rule, and a manual slice
step that stops on a failed hash. After the fifth read (still 1.4.0), "Did the restore get
everything?" checks that `--all` restored every unit `--find-envelope` lists.

**Since 1.5.0** (ADR-0012 item 109, #485; no golden pin moves). File 1 and RECOVERY.md text
only. File 1 gained: "done" in Start here holds over the tapes held; "Files deleted before the
newest version" (`--list --version` twice, then `grep` and `awk`); where to get a newer dar; in
"Compressed units", a short way to a unit's `dar_command` with an escrow or operator key and
"three times" as a rule of thumb, not a ceiling; in "The tape drive", a cartridge's generation
read without a drive and one working setup; after a failed `dd`, a rewind and `fsf` to the
wanted position; glossary entries "Index" and "Catalogue" and the placeholders `OPERATOR_NAME` and
`FIRST_TAPE_POSITION`; and "tapectl itself" sends the heir to the Heir Kit's copy of the program
first (a kit made by 1.5.0 or later carries `tapectl-<version>`, named on its cover sheet with its
sha256). After the sixth read, "done" also checks the Heir Kit's catalogue, and a unit whose
every copy is damaged has a done state. "Start here" may run to 64 lines.

**Since 1.6.0** (ADR-0012 items 110-115, #420; the golden pins of RESTORE.sh, File 0 and
MANIFEST.toml move). A tape may end early, at the drive's early warning, with a short seal
(§4.1–§4.3).
- **On tape.** File 0 and the seal state `requires = ["short-seal"]` on every tape (§1.2).
  Every seal states `ended_after` (§4.1). File 3's `[index]` and every envelope's
  `[manifest]` state `may_end_early = true`, and File 3 has a header paragraph that says so
  (§1.7, §3). RECOVERY.md and File 1 explain the seal's `ended_after` and how to read it by
  hand (§1.7).
- **RESTORE.sh.** `KNOWN_FEATURES="short-seal"`, and the "this tape needs tapectl" text
  (`die_newer_needed`, fixed at "tapectl 1.1.0" since 2026-10-06 item 20) moves to "tapectl
  1.6.0", because a new required feature arrived. A seal before File 0's pointer is the normal
  short ending on a tape that requires `short-seal`, and draws no warning (§1.2 rule 6).
  `establish_files` reads the seal after File 3 and before any slice. The modes behave as §4.2
  says: `--info` names where the tape ended, `--verify` prints `NOT ON TAPE` past it,
  `--restore` and `--list` refuse a cut unit by name, `--where` marks cut versions, and a second
  tape's seal is read first.
- **Compatibility.** 1.6.0 reads every tape before it: a seal with no `ended_after` is a full
  seal (§4.1 S3), and a tape that does not require `short-seal` never ends early. Every reader
  that checks `requires` (RESTORE.sh since 1.1.0, tapectl's readers as §1.2 rule 1 lists them)
  refuses every 1.6.0 tape at File 0 and names `short-seal` (§1.2). So the Heir Kit's copy of
  tapectl must be 1.6.0 or later once a 1.6.0 tape exists, and the kit is regenerated when
  1.6.0 is installed (item 111). A 1.5.0 tape's
  own RESTORE.sh refuses a 1.6.0 tape given to it as a second tape (`--swap-tapes`,
  `--fill-from`) by the same rule.

**Since 1.7.0** (ADR-0012 items 121-143, #478; RESTORE.sh's golden pin moves, and a second
MANIFEST.toml case is pinned). A unit that allows spanning may have its slices on more than one
tape, as ranges (§4.4).
- **On tape.** A tape whose plan holds a slice of a spannable set states `requires =
  ["short-seal", "spanning"]` in File 0 and the seal, spelled as §1.2 rules; every other tape
  states `["short-seal"]` as in 1.6.0 (item 124). A spannable set's `[[units]]` block states
  `may_span = true`, lists every slice of the set, and gives `tape_position` only to the slices
  the tape plans (item 125). RECOVERY.md gains a section on a unit across tapes, and File 1 a
  glossary entry and a section on it, with range writes in its Heir Kit query (items 135 and
  141; §1.7, §4.4). File 3, the seal grammar, the short seal's pin and whole-unit blocks do not
  move (items 126 and 135).
- **RESTORE.sh.** `KNOWN_FEATURES="short-seal spanning"`, and the "this tape needs tapectl" text
  (`die_newer_needed`) moves to "tapectl 1.7.0", because a new required feature arrived. A
  spanning unit restores across tapes from the tape holding its slice 1: by default through
  scratch space holding only ciphertext, or with `--stream-tapes` through pipes, each further
  tape accepted by slice hash (§4.4). `--where`, `--find`, `--list` and `--restore --all` name
  ranges. Every short tape gets one `NOT ON TAPE` line for its planned files past the end,
  instead of one per file (item 140, §4.2).
- **Compatibility.** 1.7.0 reads every tape before it. A tape with no spannable set states what a
  1.6.0 tape states and holds only whole blocks, so a 1.6.0 reader reads it. A 1.6.0 reader
  refuses a span tape at File 0 and names `spanning` (§1.2), and its RESTORE.sh refuses one given
  to it as a second tape by the same rule. The Heir Kit is regenerated when 1.7.0 is installed
  (item 135).

## 7. Block mode

Fixed-block mode, 512 KB (decision D7). Plan-first depends on deterministic block
padding — the Layout's `pad_to_blocks` and RESTORE.sh's trim-to-`size_bytes`
contract both require a fixed block size; variable-block mode (v4 design §2.29,
stale drift) buys nothing when every size is pre-planned. 512 KB is safe under both
the `st` ~2 MB memory ceiling and the drive's 8 MB encrypted-block ceiling; 1 MiB is
the throughput sweet spot but is a real-hardware tuning question, gated on the
LTO-6 validation session, not a format question.

## 8. What v2 removes

- The v1 **mid-tape mini-index** — its position/type/size data moves to File 3 and
  gains ciphertext hashes; its generator is repurposed, not deleted.
- The three-layer **end-of-tape salvage** (v4 §2.9 / Appendix C:
  overwrite-incomplete, sacrifice-last-slice). Until 1.6.0 a real EOT was a **clean
  abort to an unsealed tape** (layout-session.md), never a salvaged partial — a partial
  was never a copy (ADR-0003/0004), and the salvage path is untestable on mhvtl (the
  2026-07-20 drill) and was a source of the audit's HIGH-severity defects. Since 1.6.0
  the drive's early warning ends the tape with a **short seal** (§4.1–§4.3). That is
  not the salvage: nothing is overwritten or sacrificed, no unit is split, and only
  whole units count. (From 1.7.0 a unit that allows spanning may end a tape as a range
  (§4.4); it counts slice by slice and needs its other tapes, and nothing of it is
  overwritten or sacrificed either.) The hard end inside a slice is still the clean abort. The
  `writes.eot_recovery` and `writes.sacrificed_slice_id` columns become inert schema
  reserve (documented dead in `docs/design-errata.md`; not dropped, per the repo's
  inert-reserve convention).
- The **manifest reserve**. Front metadata is a known up-front line item in the
  plan total, not an end-reservation, so `CapacityBudget.reserve_bytes` collapses to
  just the small ENOSPC buffer (a margin against MAM over-reporting remaining
  capacity). This is an active code change (config `manifest_reserve` →
  `enospc_buffer`), not doc-only.
- **The planning header as a separate tape file** (resolved 2026-07-22 — the
  "may fold" option in earlier drafts of this spec, taken). Its unique v1 value
  was being written *early*, before the slices; envelopes-first supersedes that,
  and its content (the planned packing list) duplicates what the operator
  envelope carries. It survives as a **`PLAN.toml` member of the operator
  envelope** (same recipients: op + escrow); the standalone zone, its generator
  position, and `ZoneKind::PlanningHeader` are removed at the write flip.
- **Appendix D tape append** — already rejected by ADR-0003; reaffirmed.
