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
correctly. 1.1.0 knows none, and requires none. The rules every reader keeps
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
   #420, will be `requires = ["short-seal"]` with an `ended_after` field). A
   change no `requires` can fence (a different byte layout of the slices,
   say #370's "tapectl cuts the slices") bumps `layout_version` and `magic`.
4. **A key's meaning never changes.** New meaning, new key.
5. **New keys in File 0 go in a trailing table** (§1.1), with one exception:
   `requires` sits in `[volume]`, because it must be read before anything
   else is trusted and no other table uses the name. A v3 File 0 must not
   carry `front_index`/`seal_marker` as bare integers a v2 RESTORE.sh could
   read and follow.
6. **The seal marker is the last file, found as such.** Readers locate it by
   spacing to end of data and back two filemarks (`mt eod`, `mt bsfm 2`;
   GNU mt spells end of data `eom`), and use File 0's `seal_marker` as a
   cross-check and a fallback, never the only route. RESTORE.sh does; a
   disagreement is named. The Rust chain walk takes the seal's position from
   the catalog's own Layout, which is authoritative for a tape this catalog
   wrote; the write path's contact check still follows File 0's pointer
   (open item, `docs/design/v2-open-questions.md` §10).

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
| `first_slice`, `last_slice` | the range of those slices this volume holds: `1` and `num_slices` for a whole version. Under spanning (ADR-0012 items 82-87, not built) a volume holds one range of a spanning version, and the version is as covered as its least-covered range (item 76) |
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
  given, with the holders of each match. Both read the envelopes the key opens.
- `catalog rebuild --from-volume` reads the operator envelope's `units.tsv` and reports the
  volumes it names that the catalog lacks, as a to-do list for disaster recovery. It never
  creates a row from it (item 78; ADR-0001: a claim about a tape needs contact with that tape).

**Older tapes and readers.** A tape written before 1.3.0 has no `index/` members and is read as
before. A reader that does not know them (RESTORE.sh before 1.3.0; tapectl reads envelope members
by name) ignores them, so the index needs no `requires` entry (§1.2 rule 3).

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
   and it is proven on real hardware.
2. **Catalog.** With the operator or escrow key, `catalog rebuild --from-volume`
   reconstructs everything the catalog knew about this write *at staging
   time*: volume identity, units, snapshots, the slice map and plaintext
   hashes (the envelope `MANIFEST.toml`), tenant ownership, each stage set's
   recorded recipient list and the per-file index (the operator envelope's
   `catalog.db`, §1.4). Tapes written before 2026-09-11 carry an older
   `catalog.db` without ownership or receipts; the rebuild takes ownership
   from the tenant envelopes and reports the receipt as unknown. Tapes
   written before 1.1.0 record no file kinds or link targets; the rebuild
   records their non-directories as regular files.
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
This test is D4's real enforcement — the prose above is only its statement.

## 3. The front index (File 3)

Plaintext TOML. One entry per tape file, in position order:

- `position` — tape file number (0..M). Present for **every** file (navigation is
  total: the heir can jump straight to any file, including the seal marker).
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

Plaintext TOML, one tape file, written last: a `[seal]` section — `{ layout_version,
file_count = M+1, sealed_at (RFC 3339), front_index_sha256 }` — followed by a **full
embedded copy of the front index** (`[[files]]` entries in the same grammar as
File 3). Its **presence** is the assertion "every file before me is present and this
is a sealed volume"; its **absence** means the tape is self-evidently unsealed
(interrupted or aborted — see layout-session.md).

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
detection.

## 5. What "confirm" verifies (byte level)

The write-session **states and sequence** live in layout-session.md; this section
fixes only the *cryptographic content* of the confirm pass, because it is a property
of the format. Confirm is a **single forward pass from BOP** (no seek-back — the
index is at the front):

1. Read the seal marker (last file). Absent ⇒ not sealed; confirm fails.
2. Hash File 3; compare to the seal marker's `front_index_sha256`. Mismatch ⇒ the
   tape's two ends disagree ⇒ quarantine.
3. Parse File 3; diff its `{position, type, size_bytes}` entries against the Layout
   ⇒ **navigable** tier.
4. For every file except File 3 and the seal marker, read and hash it, compare to
   File 3's `sha256_encrypted[i]` ⇒ **integrity** tier.

**Which tier runs depends on the caller** (ADR-0012, amendment 2026-10-06 item 1,
#387; v2-open-questions.md §1.2's reversal and §2.4 L3):

- **A write's post-seal confirm is navigable by default** (steps 1–3: File 3 and the
  seal marker), and a passing one seals. `--full-confirm`, on every command that
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
   at the end of its one ascending pass.
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
5. For each slice position in your manifest, in order: `dd bs=512k` it out;
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
  overwrite-incomplete, sacrifice-last-slice). A real EOT is a **clean abort to an
  unsealed tape** (layout-session.md), never a salvaged partial — a partial was
  never a copy (ADR-0003/0004), and the salvage path is untestable on mhvtl (the
  2026-07-20 drill) and was a source of the audit's HIGH-severity defects. The
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
