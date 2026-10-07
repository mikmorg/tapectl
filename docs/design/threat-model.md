# Threat model, device boundaries, power, and how the format grows

Issue #394, from the external architecture review of 2026-10-04. The design
choices that review raised are sound, but several were written down nowhere.
This document states them. It decides nothing new: every rule here is either
code that already exists (cited) or a CTO ruling already recorded in ADR-0012
(cited by amendment and item). If a statement here and the code disagree, the
code is the bug report and this document the claim to test it against.

The normative byte format is `volume-format-v2.md`; the session state machine
is `layout-session.md`. This document sits beside them and refers to both.

## 1. Who the adversary is, and is not

tapectl protects an individual's personal archive against **loss, decay and
inheritance**: a disk dies, a cartridge rots, a house burns, the operator dies
and an heir has to read the tapes. It is not built against a state adversary
or against someone who holds a cartridge and wants to rewrite it undetectably.
ADR-0012 (amendment 2026-10-06, item 8) accepts this explicitly when it keeps
encryption X25519-only: "the media are in the operator's physical custody,
the data is personal, and the threat model is loss and inheritance". The same
item names when to revisit it: if a cartridge's custody ever leaves the
operator's control.

| Party | Has | tapectl's stance |
|---|---|---|
| The operator | the catalog, every key, the cartridges | trusted |
| An heir | a cartridge, the Heir Kit (escrow secret, catalog copy) | trusted; must be able to restore with `mt`, `dd`, `age`, `dar`, `tar` and no tapectl |
| A tenant | their own key files | trusted for their own data only; tenant isolation (below) |
| Someone who finds or steals a cartridge | the tape, nothing else | must learn nothing about content |
| Someone who can rewrite a cartridge and put it back | the tape, write access to it | **out of scope** for authenticity; see §1.2 |

### 1.1 Confidentiality: what a tape shows without a key

Settled by the isolation invariant (`volume-format-v2.md` §2): no plaintext file
on a tape reveals file names, tenant or unit names, plaintext-content hashes or
key fingerprints. The plaintext zones (File 0's ID thunk, File 1's guide,
File 2's RESTORE.sh, File 3's front index, the seal marker) carry the label,
the layout, per-file on-tape sizes and per-file **ciphertext** hashes, and
nothing a tenant can be identified by. What they do give away is counted, not
named: how many tenants share the tape (one `tenant_envelope` per tenant), how
many slices, and how big each file is. The review's claim that tenant names
appear in plaintext is false: a `tenant_envelope` entry in the front index is
labelled only by its kind, and a tenant is found by age trial-decryption, not
by a name (`keys-and-recovery.md`, "Who can open what on a tape").
`tests/tenant_isolation.rs` scans raw tape bytes for plaintext leaks, and the
gated plaintext scan (`scripts/plaintext-scan.sh`) does the same on a real
tape with a planted canary as its positive control.

### 1.2 Integrity is not authenticity

Two different properties, and tapectl gives the first, not the second.

**Integrity: damage is detected.** The front index's `sha256_encrypted` per
file and the seal marker's `front_index_sha256` form a keyless chain
(`volume-format-v2.md` §4): anyone with `dd` and `sha256sum` can prove every
byte of a tape is what was written. Confirm, `volume verify` and RESTORE.sh's
`--verify` walk exactly that chain. Bit rot, a truncated write, a misread
block, a tape whose end was lost: all of them break the chain, and the chain
says where.

**The chain is unkeyed, so it does not detect a deliberate rewrite.** Every
hash on the tape is a plain SHA-256 of bytes on the same tape. Someone who can
write the cartridge can replace a file and rewrite the front index and the
seal marker with matching hashes, and the chain will verify. The seal marker
is a structural assertion ("every file before me is present; this volume is
sealed"), not a signature. Nothing on a tape is signed.

What does detect tampering with **content** is age's authenticated
encryption: each slice and envelope is ChaCha20-Poly1305 in 64 KiB chunks
under a header MAC, so a modified ciphertext fails to decrypt. That protects
against changing bytes, not against substitution: age authenticates the
payload to the holder of the file key, not the sender, so anyone who knows a
recipient's **public** key can encrypt a different archive to it and put that
on the tape. The only anchors outside the tape are the catalog (each staged
slice's ciphertext hash, as staged) and its copies: `db backup` and the Heir
Kit's encrypted `catalog.db.age`. The operator envelope's catalog snapshot on
every tape is itself on the tape, so it is only as good as the tape.

Which checks use which anchor:

- **Confirm**, at the end of a write, compares what it reads back against the
  session's own Layout — the catalog's record of what it just wrote.
- **`volume verify`** rebuilds the map from the tape's own File 3 and walks
  the on-tape chain (`write::volume_verify`), so it proves the tape is
  internally consistent and undamaged; it does not compare against the
  catalog's hashes, and a consistent rewrite passes it.
- **RESTORE.sh `--verify`** walks the same on-tape chain, with no catalog.

A check of a tape's front index against the catalog's recorded hashes would
detect a rewrite of any tape this catalog wrote; no command does that today.
Under this threat model it is not needed, and it is noted here so the gap is
a known one.

What an altered plaintext zone can do, and cannot:

- **Can** misdirect: a rewritten front index can point a reader at the wrong
  file, hide a file, or claim a size that cuts a slice short. The effect is a
  failed or refused restore, never a silent mix-up of tenants' data: the
  tenant envelope a key opens names that tenant's own slices, and decryption
  authenticates each one.
- **Can** replace RESTORE.sh, the one piece of code on the tape an heir is
  told to run. Under this threat model that is accepted: whoever can rewrite
  the tape can also destroy it. An heir who doubts a tape can read RESTORE.sh
  before running it (it is plain text), or use the RESTORE.sh of another
  cartridge of the same version.
- **Cannot** reveal content, or make a key open another tenant's data.
- **Cannot** make tapectl write over a sealed volume: ADR-0003's refusals come
  from the catalog's recorded seal (`volumes.sealed_at`), not only from what
  the tape says (`volume_write`'s fact checks).

If custody ever widens beyond the operator (a warehouse copy that could be
swapped, a courier), the remedy is a signature over the seal marker with a
key that never goes on tape; that is a format change (a `requires` feature,
§4) and a CTO decision, not something to add quietly.

## 2. The st / SG boundary

tapectl talks to a drive through two kernel interfaces, and keeps them apart
on purpose.

**The st driver (`/dev/nstN`, `src/tape/ioctl.rs`) does all data I/O and every
movement of the medium.** Reads and writes in fixed 512 KiB blocks, filemarks,
rewinds and spaces are all `MTIOCTOP` ioctls and `read`/`write` on the st
node, so the st driver's own position accounting (file and block number) is
always the result of tapectl's own commands. `TapeDevice::open` and
`open_read` set the block size (`MTSETBLK`) on every open, so a block size
left behind by another program never applies.

**SCSI generic (`/dev/sgN`) is used only to ask the drive and the cartridge
questions**, through three sg3-utils tools and no others:

| Tool | SCSI command | Used for |
|---|---|---|
| `sg_inq` | INQUIRY (standard, and the unit-serial VPD page 0x80) | the drive's identity (`tape::drive_identity`), and the identity header of a health record (`tape::log_pages::inquiry_header`) |
| `sg_logs` | LOG SENSE | the drive's health and error-counter pages (`tape::log_pages`, `tape::health`) — never `--reset`/`--select`, which would issue LOG SELECT and clear counters |
| `sg_read_attr` | READ ATTRIBUTE | the cartridge's MAM: medium serial, density, load count (`tape::mam`) |

None of the three moves the medium or changes the drive's state, so the st
driver's position cannot drift underneath it. tapectl never sends a WRITE
ATTRIBUTE, a MODE SELECT, a LOAD/UNLOAD or a raw CDB over SG, and never runs
`mt` or `sg_raw` itself (the operator's tools; `first-run.sh` and the
lifecycle suite use `mt`, the binary does not). `tests/sg_boundary.rs` pins
both sets — the `sg_*` tools the source names and every program the binary
starts (dar, `bash -n` over RESTORE.sh, `systemctl`, and the three above) —
and the exact `sg_logs` argument list: a new SG tool, a new program, or a
counter-resetting flag on `sg_logs` fails it.

**Filemarks.** Every file of a volume ends with an *immediate* filemark
(`MTWEOFI`: queued, returns at once), so the drive keeps streaming between
files. The seal marker, the last file, ends with a *synchronous* filemark
(`MTWEOF`, WRITE FILEMARKS with IMMED=0), which does not return until the
drive has written everything in its buffer, the seal marker included, to the
medium (`store.rs`'s `execute(…, sync)`: `sync` is true for the seal marker
only — `session::ReadyToSeal::seal` is its one caller with `true`, pinned by
`session.rs`'s test `only_the_seal_marker_is_written_with_a_synchronous_filemark`
over the MemStore's `syncs` record). The catalog records the seal
(`volumes.sealed_at`, `write.rs`) only after that call returns.

## 3. Power: the baseline

ADR-0012 (amendment 2026-10-06, item 14): **no explicit SCSI SYNCHRONIZE
CACHE**. The seal is written with a synchronous filemark (`MTWEOF`), which
flushes the drive's buffer to the medium before the catalog records the seal;
a separate flush command would add nothing.

What each failure costs:

| When the power goes | On the tape | In the catalog | Recovery |
|---|---|---|---|
| During staging | nothing | a stage set not finished | re-run `stage create`; the startup sweep and `staging clean` handle what was left |
| During the write, before the seal | an unsealed tape: the files written so far, no seal marker | the session `in_progress`, swept to `interrupted` on the next open | `volume resume` continues from the frozen staging files after checking File 0's identity and that no seal marker is present (`layout-session.md`, Resume) |
| After the seal, before or during confirm | a sealed tape | `sealed_at` recorded, the session `interrupted` (in the instant between the filemark returning and that record, not yet recorded) | `volume resume` re-confirms it; the seal is never written twice (ADR-0012, 2026-09-21). With the record missing, resume finds this session's own seal on the tape (label, uuid and position all matching) and re-confirms the same way |
| After the seal, with the catalog lost (restored from a backup older than the write) | a sealed tape | no record of the write | not yet automatic: #360 (ruled, built after the first production write). Until then, back up the catalog at the end of every write session |

The baseline for a write host is therefore **a UPS** that carries the host
and the drive long enough for a clean shutdown (systemd's SIGTERM now stops a
long run at its next safe point, issue #404), **the catalog on a disk that
survives** (the home device encrypted, ADR-0012 2026-10-06 item 6), and **a
catalog backup after every write session** (`systemctl start
tapectl-backup.service`, `docs/install.md` §7). A power cut is never a reason
to re-initialise a cartridge: resume first.

## 4. How the format grows

Normative in `volume-format-v2.md` §1.2 (ADR-0012, amendment 2026-10-06,
item 15; issue #384). In short: File 0 and the seal marker carry
`layout_version`, a `magic` and, from 1.1.0, a `requires = [...]` list of
features a reader must understand. Every reader (tapectl's and RESTORE.sh)
checks all three and **refuses a tape that needs a feature it does not
know**, naming it, rather than misreading it; a key it does not know is
ignored unless `requires` names it; a key's meaning never changes. RESTORE.sh
finds the seal by spacing to end of data and stepping back, File 0's pointer
being a cross-check; tapectl's own chain walk takes the seal's position from
the catalog's Layout, authoritative for a tape this catalog wrote
(`volume-format-v2.md` §1.2, rule 6). A change that no `requires` can fence
bumps `layout_version` and `magic`. An heir is covered regardless: every tape
carries the RESTORE.sh of the tapectl that wrote it.

## 5. One cartridge, one recovery unit

Each write session writes one cartridge, and each cartridge is complete in
itself: its own guide, RESTORE.sh, front index, envelopes, slices and seal. No
tape needs another to be read, and losing one costs exactly its own copies.

`collection plan` refuses a unit larger than the per-tape budget
(`collection::selector::OversizedUnit`). The same refusal at `stage create`,
naming the limit for the cartridge's generation, is ruled (ADR-0012,
2026-10-06, item 17) and not yet built (#395): today `stage create` stages
an oversized unit in full, and only `volume write`'s pre-flight capacity gate
refuses it, before any byte reaches the tape. Planned spanning — the planner splitting a unit's slices across
a named set of cartridges — is designed once any unit passes about half a
cartridge, and §4's mechanism lets it arrive without breaking older readers.
A drive running out mid-unit is not a split: mid-write handover was rejected
(item 9), and a later short seal (#420) writes the cut unit whole to the next
cartridge.

**Units are never split by hand**, and datasets are never "partitioned before
ingestion": manual splitting was ruled out (ADR-0012, amendment 2026-09-30).
If a large unit's re-archiving ever costs too much, the answer is #12's
differential-only shape, underneath the unit.
