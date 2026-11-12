# Intent and proportion review — 2026-11-12

**Asked for:** "Review this project's intents and goals and make sure the design is consistent
from the original standpoint, and has sufficient value over complexity to be appropriate over
alternatives", by the CTO on 2026-11-12, the day after handoff (i) finished (master `0946c152`,
home2 on 1.9.2, `keepsake` on L6-0001 and L6-0002).

**What this is.** Every earlier review asked whether the code does what the design says. This
one asks the two questions above them: whether the design is still the one the project set out
with, and whether the whole is worth its size against simpler ways to do the same job. It files
nothing and changes no ruling. Its findings are a decision list for the CTO (§8).

**Method.** Read-only: no cargo, no tape, no production host. The coordinator read the first
commit's design (`tapectl-design-v4_0.md` §1, edited three times since, none in §1), the intent
statement on #2, `docs/design-errata.md` in full, ADR-0012's rulings on tenants, spanning, the
fresh starts and the friends' setup, every release note's opening, `contrib/hosts/home2.profile`
and the run records, and counted the tree. One fresh agent was given the raw facts and no
verdict, and asked for the strongest case for a simpler system, with current prices and tool
status from the web (§6). It corrected two of the coordinator's facts, both checked and taken.
The coordinator built most of what is reviewed here, so §6 exists to give a read that is not
anchored on its own.

## 1. The yardsticks

**The first design (2026-04-10).** `tapectl-design-v4_0.md` §1.4 assumed one standalone LTO-6
drive, a Debian home lab, **30-100 TB** of archival media, **5-20 tenants** and two or more
locations. It chose dar over tar for four reasons (built-in slicing, a file catalogue without
extracting, XML listing, encryption per slice), and rejected LTFS, Bacula/Amanda and GPG by name.

**The intent statement (2026-07-17, #2).** The CTO's resolution, "the yardstick for every
big-change decision":

1. **One human, many hats.** One operator-owner. Tenants are data classes of one person's life;
   no investment is owed to stranger-facing tenancy.
2. **Bulk + core.** Re-acquirable bulk media dominates bytes, a small irreplaceable core
   dominates value. Capacity efficiency for the bulk, paranoid redundancy for the core.
3. **The threat model.** Site loss, future-self failure, media and drive death, online
   compromise. Site loss and future-self failure were the two unmitigated gaps.
4. **Decades, heir-capable.** A competent heir or stranger holding the key envelope restores the
   core from tape alone.
5. **Renovated = THE archive.** Success is the operator trusting tapectl with the full real
   corpus as system of record. New capabilities are admitted only where they serve that trust.

## 2. Consistency with the first design

`docs/design-errata.md` lists 57 divergences from v4.0. Every row names its authority: an ADR, a
numbered ruling or a closed issue. Read against §1.1's capability list:

| v4.0 capability | Today | Authority |
|---|---|---|
| Three-phase pipeline, stage sets | kept | |
| Self-describing volumes, restore with no database and no tapectl | kept; the layout was redone as Layout v2 | ADR-0007 |
| No content metadata in plaintext on tape; per-tenant envelopes | kept | |
| Cartridges tracked apart from volumes; reuse | kept and made real (the chip serial is the identity) | ADR-0010, ADR-0012 |
| Compaction | kept; `volume rewrite` added for one cartridge | item 106 |
| Archive sets, advisory policy audit | kept | |
| sha256 at every stage, verification journal, audit trail, safe Ctrl+C | kept | |
| End-of-tape salvage | rejected; a short seal at the early warning replaces it | ADR-0007, items 110-115 |
| Tape append | rejected | ADR-0003 |
| `volume calibrate` | rejected | #145 |
| Incremental and differential archiving | full-only; differential-only is held in reserve with reopen triggers | #12, 2026-09-30 item 4 |
| S3 backend | rescoped to recorded warehouse deposits of sealed volumes | ADR-0006, #72 |
| Export to Blu-ray or USB as a store | not built; `unit export` copies staged slices only | ADR-0006 (no issue tracks it) |
| Tape-to-tape clone | recast as `volume read-slices` then a write | 2026-04-13 |

**The verdict on consistency: the design is the one the project set out with.** What changed
changed by ruling. The three things v4.0 treated as essential are stronger than v4.0 had them:
restore without the tool, no plaintext metadata, and a catalog that the tape can correct. Five
places are not reconciled. None is a defect in the code; each is the record saying two things.

- **C1. The errata is behind.** Its header says "the complete list of known divergences as of
  2026-10-21 … item 143", and its last rows are from 2026-10-25. Nothing in it mentions the
  cross-volume index (v4.0 §8.7; 1.3.0), `volume rewrite` (§2.10; 1.4.0), Logical Block
  Protection (§2.29 and §1.3's "rejected: full SG_IO"; 1.8.0), the spellings 1.9.0 removed from
  §5, or `tapectl setup` (§1.4; 1.9.1). The first two predate the header's own date.
- **C2. What a tenant is says two things.** The errata's row and `CONTEXT.md`'s first line say
  tenants are "one operator's data classes, not separate people", from intent 1. ADR-0012
  (2026-09-28, item 5) says "tenants are rings of trust held as keys: the operator, then
  `parents`, then `family`". Production has those tenants, and handoff (i) ends by giving each
  ring its key file. The ruling is the later one, and it matches v4.0's own words (tenants "can
  self-restore from tape with their key"). The intent statement, the errata row and the
  glossary's headline were not amended. It matters because intent 1 is what says isolation is
  owed no further investment, and under item 5 isolation is what keeps one ring out of
  another's data.
- **C3. The friends' setup has no place in the intent statement.** Items 227-232 added a second
  installer (`tapectl setup`), a Quick start page and a release binary built by CI, for people
  who are not the operator. Intent 1 says the project "serves a single operator-owner". The
  items are ruled. The yardstick they should have been judged against was neither cited nor
  changed.
- **C4. Half of dar's stated reasons no longer hold, and nothing records why it stays.** v4.0
  §1.2 gave four. Two still hold: the catalogue rides in every envelope, and the XML listing
  came back in 1.1.2 as `stage create`'s check that dar archived every file it hashed (#464).
  Two do not. dar's encryption was never used (age). Its built-in slicing is not used either:
  `dar -c -` refuses `-s`, so since 1.1.0 tapectl cuts the stream itself and re-creates dar's
  slice framing (`src/dar/slice.rs`, mirrored in awk in RESTORE.sh). What dar has cost is a
  source build on home2, a FIFO bug that took two releases (#497, #498) and is still unreported
  upstream (#499), and a program the heir must find in a version that reads these archives.
  The cross-volume index (1.3.0) now lists every path with its sha256, which is most of what
  the heir needed dar's catalogue for. This is the one founding choice the record never
  revisited (§8, D5).
- **C5. The scale the design was sized for is not the scale in use,** and no document says
  which one the project is building for now (§3).

## 3. The assumptions, then and now

| | v4.0 (April) | Production (2026-11-11) |
|---|---|---|
| Data | 30-100 TB | 1.20 TiB: `keepsake`, 27 units, the largest 470.5 GiB |
| Cartridges per copy | 12-40 | 1, about 52% full |
| Tenants | 5-20 | 3 registered, 1 holding data |
| Locations | 2+ | 2 |
| Bulk media (intent 2) | most of the bytes | not registered; no handoff step or backlog entry names it |

Intent 5's test is "the full real corpus". Today tapectl holds the core and none of the bulk.
The two intent gaps of July are closed for the core: a sealed, fully read-back copy is at
`offsite`, and the Heir Kit and the escrow recipient exist. That is the result the intent
statement asked for first.

The repo does not record whether the bulk is still coming, or how much. **That is the fact this
review turns on**, and it is the CTO's (§8, D1).

## 4. What was added after the first production tape, and who uses it

`src/`, with its in-module tests, was about 12,700 lines of Rust on 2026-07-16, 129,000 at
`v1.0.0` (2026-09-30) and 319,000 at `v1.9.2`: about 160,000 lines of code and 160,000 of tests,
beside 61,000 lines of integration tests and 22,000 of scripts. (The split is a heuristic: a
file's lines from its first `#[cfg(test)] mod` on.) Sixty percent of it was written in the six
weeks after the first production tape. The same 1.2 TiB was written in three campaigns in that
time (1.0.0, the fresh start on 1.1.0, the second on 1.9.2): five full cartridge writes.

| Added | Release | The recorded trigger | Code (about) | On tape | Used by production |
|---|---|---|---|---|---|
| Cross-volume index, restore repairs | 1.3.0 | #418 R4, #480 | 700 + RESTORE.sh modes | new envelope members | the index is on both tapes; no repair has been needed |
| `volume rewrite` | 1.4.0 | #483 | 2,200 | none | no |
| Heir Kit carries tapectl | 1.5.0, 1.9.1 | #485, #486 | small | none | yes |
| Short seal | 1.6.0 | #420; item 9 said "later, after home2's writes show how much capacity they really use" | in the session and the tape layer | `requires = ["short-seal"]` | armed on every write; never reached (tapes are half full) |
| Spanning | 1.7.0 | item 90 replaced item 77's trigger (a unit past half a cartridge) with "right after #420, whatever the unit sizes, for the tape fill" | 840 + coverage, session, RESTORE.sh `span_*` | `requires = [.., "spanning"]` on a tape that carries a span | no: off, and the largest unit is 20% of a cartridge |
| `copies_at`, auto-stage | 1.7.1 | #493 | 1,570 | none | no |
| Logical Block Protection | 1.8.0 | #388 | 950 | none | armed at every open |
| RESTORE.sh feeds dar one stream | 1.8.0 | #498, dar's FIFO bug | in RESTORE.sh | re-pin | yes |
| `first-run.sh --upgrade` | 1.8.1 | #505 | scripts + a harness | none | yes, three times |
| The CLI review | 1.9.0 | asked 2026-11-04 | `render.rs`, `renamed.rs` | none | yes |
| `tapectl setup`, CI release binary | 1.9.1 | items 227-232 | 1,650 | none | no recorded user |

Read against intent 5 ("admitted only where they serve that trust"):

- **Serve the core's trust directly:** the heir path's work (the index, the repairs, six fresh
  reads of File 1, the kit's binary), Logical Block Protection, the upgrade path, the CLI review.
- **Serve the bulk, which is not here:** the short seal, spanning, `copies_at` and auto-stage,
  `volume rewrite`. Each is intent 2's "capacity efficiency for the bulk". Each was ruled, and
  spanning's own trigger was replaced with "whatever the unit sizes". They are sound if the bulk
  comes and speculative if it does not.
- **Serve neither:** the friends' setup (C3).

## 5. Complexity, by who carries it

**The heir.** This is the complexity that has to work with no maintainer. RESTORE.sh on tape
was about 1,150 lines at 1.0.0 and about 1,950 at 1.1.0 (estimated from the generator's
source), 4,714 at 1.5.0 and 5,126 at 1.6.0 (the test fixtures), and 7,061 lines (301 KB, 177
functions, 21 options) as the gate wrote it on 2026-11-11, which is what the two production
tapes carry. A third of it was added after 1.5.0, for the short seal, spanning across
cartridges and the single stream into dar; the first two serve tapes these two cartridges are
not.

Three things bound the risk. File 1 carries "Manual Recovery Steps", about 200 lines that do
the same job by hand with `mt`, `dd`, `truncate`, `age`, `tar`, `sha256sum` and `dar`. The gate
runs RESTORE.sh with the real tools on every release. And each tape carries the script of the
tapectl that wrote it, so the script never has to read a newer tape.

Three things do not. The script has already met a changed substrate, twice in October, on
Ubuntu 26.04's coreutils: a `sha256sum` that quotes names hid damaged files from its list
(item 145), and a `tail` on a FIFO hung (item 184). CI caught both before a tag; nothing will
catch the next one in 2040. The manual steps are the fallback for that day, and **no test
executes them**: six fresh readers read them, and the gate and the lifecycle suite run only
the script. And of the heir's ten programs, `dar` and `age` are the two an ordinary install
lacks, and dar is the one whose version matters. The kit carries tapectl; it carries neither
dar nor age, and tapectl's own restore needs dar too.

**The operator.** Small, and it got smaller: one copy loop (`volume write L --init
--full-confirm --move-to X`), `status`, `audit`, `collection sync`, and `first-run.sh`. The
surface behind it is 141 commands and 302 flags.

**The maintainer.** About 160,000 lines, 41 migrations, 13 ADRs of which one holds 234 numbered
rulings, a 67-check tape gate and a whole-chain run of about 80 minutes for any scripts change.
One person cannot hold this without agents. The independent reviewer's phrase for it is that
intent 3's "procedures in one head" became procedures in no head. That is acceptable for the
archive only because of the first design's best decision: the tapes do not need tapectl. If
tapectl were never built again, the two cartridges, the kit and File 1 would still be the
archive. The size of the tool is a cost to changing it, not a threat to what it has written.

**The cost that was real is churn, not size.** Twenty-two tags in six weeks meant five
cartridge writes of the same data, two fresh starts with new keys, and one window in which
`keepsake` had no copy on tape (item 37, accepted). Eight releases between 2026-10-13 and
2026-10-29 changed the bytes tapectl writes to tape, and the golden pins moved in 19 commits. A
format that moves that often is not yet an archive format. It has been still since 1.8.0:
1.8.1 to 1.9.2 changed no byte on tape, and `requires` (1.1.0) means a later tapectl reads
these two tapes or refuses them by name.

## 6. Against the alternatives

The independent reviewer's findings, with its sources. Prices are from third-party pages read on
2026-11-12 and are approximate; where a figure rests on one source it says so.

**Off the shelf, nothing meets the intents.** Each fails the heir, the plaintext rule or the
escrow key:

| Alternative | Where it fails |
|---|---|
| LTFS | Maintained (the reference implementation's 2.4.9.1 is dated 2026-11-11), and it lists HP LTO-5 to 9. The heir needs a FUSE driver stack. Encryption is a key handed to the drive, marked experimental, so there are no ring keys and no escrow recipient. The index holds file names unless every file is an opaque blob. No copy audit and no readback proof. |
| Bareos, Bacula | Bareos "does not encrypt file metadata such as file path names, permissions, ownership". A restore needs the catalog and the daemons. Master keys do give an escrow. |
| restic, borg, rclone crypt | Symmetric keys: every restic key unlocks the same master key, and rclone crypt is a password. A ring means a separate repository and an escrow means a shared secret. The heir needs that tool and its repository format. No tape. |
| Rotated encrypted disks | An unpowered disk needs refreshing, which fails "outlive active maintenance" (the reviewer's inference). The heir's hardware problem is easier than LTO's. |
| Cold cloud alone | The account and its billing must outlive the owner, and its credentials are an online surface (the reviewer's inference). |

So v4.0's rejections stand, and building something was right. That part of the question has a
clear answer.

**The real competitor is a much smaller tapectl.** The reviewer's design for today's scale is
tapectl's own tape layout with tar in place of dar: per unit, `tar | split | age` to 1 GiB
chunks under the ring, operator and escrow public keys; a plaintext README; an encrypted
manifest; a trailing plaintext list of ciphertext hashes as the seal; a `tapes.tsv` in git and
in the kit. It puts that at about 400 lines of bash, and the heir's restore at `dd | age -d`
into `tar x`, with no generated program. At 1.2 TiB on two cartridges it would lose single-file
restore without reading the unit, the single read of the source, automated repair from another
copy, and Logical Block Protection. It would keep every intent.

At the design's scale the reviewer does not stay at 400 lines. Forty to eighty cartridges
tracked in a text file and untested shell is intent 3's future-self failure, so it grants a
catalog, pre-flight packing, identical copies from staged ciphertext, a verify-due list, moves
and a refusal to overwrite a last copy: its estimate is a tested program of 3,000 to 6,000
lines, with rules in place of features (no unit larger than a cartridge, end of tape means
rewrite that tape, compaction means rewrite).

The coordinator's reading of that estimate. It is low for what it lists, because it prices none
of what the real drive taught: the early warning's behaviour, READ POSITION through st, the
drive's own corrections, a FIFO bug in dar, a CI image's coreutils. Those cost what they cost
whoever writes the tool. But the direction is right, and the gap is two orders of magnitude at
today's scale and one at the design's. The catalog, the audit, the consent tiers, the readback
proofs and the rebuild are the part of tapectl that the larger scale earns. Spanning,
`copies_at`, auto-stage, in-place rewrite and a second installer are the part it does not earn
until a named unit or collection needs them.

**Cost is not the argument at today's scale and favours tape at the design's.**

| | 1.3 TB | 50 TB |
|---|---|---|
| LTO-6 media, per copy | 1 cartridge, about $50 | about 21 cartridges, roughly $0.8-1.15k |
| Hard disk, per copy | about $140-165 | about $1,350-1,650 (one source, which reports prices up about 50% in a year) |
| S3 Glacier Deep Archive | about $15 a year; a full restore about $111 | about $594 a year; a full restore about $4,400 |
| Backblaze B2 | about $108 a year | about $4,170 a year |

**The heir's hardest problem is not software.** LTO-6 is read only by LTO-6 and LTO-7 drives.
In twenty years the heir needs one that works, a SAS card and a machine that takes it. File 1
says so. ADR-0006 already models a cold-storage *warehouse* deposit of a sealed volume, which is
ciphertext, and `volume deposit` records one. The run records show none. For the core that is
about $15 a year for a third copy on a medium that needs no drive (§8, D6).

## 7. The verdict

1. **Consistent: yes.** The design is the first design, changed by ruling and recorded. Five
   places in the record are unreconciled (C1 to C5); none is in the code.
2. **Appropriate over the alternatives: yes.** No existing tool gives a restore with no tool
   and no database, no plaintext metadata, and per-ring keys with a permanent escrow recipient.
   What tapectl wrote on the two cartridges is what the intent statement asked for, and it does
   not depend on tapectl surviving.
3. **Value over complexity: for the core of the design, yes; for the amount built, not at
   today's scale.** The pipeline, the format, the heir path, the escrow, the catalog as claims,
   the audit and the consent tiers each answer a named threat. But tapectl is sized for
   30-100 TB and holds 1.2 TiB, sixty percent of it was written after the first production
   tape, and the capacity features of October serve a bulk corpus that is not registered and
   that no plan names. Whether that was foresight or surplus is decided by one fact the repo
   does not hold: whether the bulk is coming (D1).
4. **What to do with that is to stop, not to cut.** The build cost is spent, the unused paths
   are off by default and proven on the real drive, and removing them would be the same churn
   this review counts as the real cost. The format has been still for two weeks and the queue is
   empty except for three items that need the drive or the CTO. This is the point the intent
   statement's fifth line describes: trust comes from the archive standing still and being
   exercised, not from more of it.
5. **The next effort belongs to the heir, not to features.** The remaining risks to intent 4
   are an LTO-6 drive in twenty years, dar on the heir's machine, a 7,000-line script whose
   fallback has never been executed by a test, and a drill that has not been run on these two
   tapes.

## 8. For the CTO

Each is a decision, with the coordinator's recommendation. Nothing here is filed or built.

- **D1. Is the bulk coming to tape through tapectl: about how much, and about when?**
  Everything in §4's middle group turns on it. *Recommended:* answer it in the intent statement
  (a sixth line, or an amended fifth), whatever the answer is.
- **D2. A feature freeze.** No new capability without a registered unit or collection that
  needs it, named in the ruling. Patches for security advisories and for defects met in
  production. *Recommended: yes, whatever D1's answer.* It is intent 5 with a test attached, and
  item 77's first trigger for spanning ("when a registered unit passes about half a cartridge")
  was that test before item 90 replaced it.
- **D3. The bytes on tape stand still.** No re-pin of RESTORE.sh, File 1 or the format without a
  defect met in production or the start of a bulk campaign. *Recommended: yes.*
- **D4. An heir drill on a production cartridge.** `docs/operator-guide.md` asks for one a year
  and none is recorded for L6-0001 or L6-0002. Someone who is not the operator, with only the
  kit: one unit by RESTORE.sh, and one small unit by File 1's manual steps, which have never
  been run end to end. *Recommended: yes, before anything else is built.* It is the only proof
  intent 4 has not had on these two tapes.
- **D5. dar.** Two questions. (a) *Now:* put a static `dar` and `age` beside tapectl in the Heir
  Kit. That is a patch with no byte on tape, and it hands the heir the two programs an ordinary
  install lacks, in versions known to read these tapes. *Recommended: yes.* (b) *Before any bulk campaign:* decide dar or tar on the
  record. Today a change of archiver costs a new `requires` feature and, if wanted, nine hours
  of drive time to rewrite two cartridges; the tapes already written keep their own File 1 and
  RESTORE.sh and are not stranded. After 30 TB it costs the bulk. If the bulk is not coming,
  dar stays and (a) is the whole answer. *Recommended: do not switch now; rule (b) with D1.*
- **D6. A third copy of the core that needs no drive.** A warehouse deposit of one sealed
  volume, as ADR-0006 models it: ciphertext only, about $15 a year at cold-storage rates. It
  answers the drive-in-twenty-years risk, at the cost of an account that must outlive the
  operator and a credential on a host. *Recommended: the CTO's call; worth it for the core.*
- **D7. The record.** (a) Amend the intent statement for item 5's rings of trust, and say
  whether friends are in its scope or whether 1.9.1's setup is finished and frozen. (b) Bring
  `docs/design-errata.md` to item 234 and correct its tenant row and `CONTEXT.md`'s first line
  to match (a). (c) An errata row for C4 once D5 is ruled. *Recommended: (a) is the CTO's
  wording; (b) and (c) are the coordinator's work once (a) and D5 are answered.*

## 9. Not reached

- Whether the bulk is on disk on home2, and how large it is. The review did not look at home2.
- A line-by-line read of items 144 to 234 against v4.0 for C1; the five missing subjects are
  the ones a search of the errata found absent.
- Amanda, borg's key model, and Bareos's location and verify features were not checked.
- AWS's own price table did not render for the reviewer; the Deep Archive figures are from two
  third-party pages.

## Sources for §6

LTFS: `github.com/LinearTapeFileSystem/ltfs` (releases; the wiki page "Encryption Handling in
LTFS (Experimental)"). Bareos: `docs.bareos.org/TasksAndConcepts/DataEncryption.html`. restic:
`restic.readthedocs.io/en/stable/100_references.html`. rclone: `rclone.org/crypt/`. Backblaze:
`backblaze.com/cloud-storage/pricing`. Deep Archive rates ($0.00099 per GB-month, $0.0025 per GB
bulk retrieval, $0.09 then $0.085 per GB egress):
`usage.ai/blogs/aws/storage-cost/glacier-deep-archive-pricing/` and
`cloudburn.io/blog/amazon-s3-pricing`. Hard disks:
`hddhunt.com/blog/best-hard-drive-deals-november-2026/`.
LTO-6 media: three retail listings, undated and in mixed currencies. LTO-6 read compatibility:
`dell.com/support/kbdoc/en-us/000134122`. dar's current release (2.8.5, May 2026):
`dar.sourceforge.io`.
