# The forensics record: a contact is the spine, and everything the hardware says is journalled verbatim

The CTO's ruling that opened the tape-forensics suite (2026-09-22):

> *"We should be dumping all MAM info in a journal so we don't lose any potentially
> useful data about tapes, **as an example of the level of record I desire**."*

The operative words are *as an example*. MAM is not the scope — it is the illustration of a
standard that applies to every hardware source tapectl already talks to. The standard is
**capture everything verbatim now, parse it later**: a parser can be added retroactively, but
a cartridge's state in 2026 cannot be re-observed. `health_logs.raw_log`
(`001_initial.sql:327-341`) is the one place the project already honours it.

Twelve issues were drafted against that standard. A coherence critic reviewing them found
that **four independently proposed rebuilding `health_logs`**, each saying "coordinate with
siblings", and nobody owned the coordination. Migrations are forward-only. Four rebuilds of
the table holding the schema's largest blobs is the most likely way this suite loses data —
not by failing to capture it, but by capturing it and then dropping a column in a migration
whose author did not know what a sibling had just added.

This ADR is that coordination. Issue #294 raised the seven questions; all seven were ratified
on 2026-09-22 and are recorded here with their reasoning, because a decision without its
reasoning is re-litigated the first time it is inconvenient.

## 1. There is a `drives` table

Every other record takes a foreign key to it and grows **no drive column of its own**.

There is one drive today, which is exactly when the table is cheap. The moment there is a
second — or the HP LTO-6 is replaced, which is a *when*, not an *if*, for a 2017-manufactured
drive — every historical row must be able to say which machine produced it. Forward-only
schema makes retrofitting that expensive, and it cannot retrofit the rows already written.

This matters more for `sg_logs` than it first appears: pages 0x02/0x03/0x2E are
**drive-resident counters**. They are a property of the machine, read through whichever
cartridge happened to be loaded. Storing them against `volume_id` alone — which is all
`health_logs` can do today — asserts the one attribution we have the least evidence for, and
it makes the central question in tape diagnostics unanswerable: **is it the drive or the
tape?** A rising uncorrected-error count means nothing until you know whether it follows the
medium or the machine, and today neither route can be queried.

The drive's vendor, product and firmware are *already* captured verbatim on every row —
`sg_logs` prints an identity header as its first line and `collect()` concatenates raw page
text into `raw_log`. They are captured and unqueryable, which is the same shape as the
`tape_alerts` loss (#107) and the ECC-parameter loss (#120), with the same remedy: parse what
is already stored. The serial is not in that header; the kernel publishes it separately.

## 2. The contact row is the spine

Every hardware observation happens *during* a contact between a drive and a cartridge, and a
reading that does not know which contact produced it cannot be differenced against its own
pair. Time-correlation across separate tables works right up until two contacts land in the
same second — which is not a hypothetical on a machine that runs a thirteen-scenario
lifecycle suite.

Once `contact_id` exists, four separate problems dissolve at once rather than each needing its
own mechanism:

- "no key joins the opening reading to the closing one" — the obstacle to counter baselines;
- `health_logs.volume_id NOT NULL`, which forbids recording a drive-only reading;
- degenerate keying for an unregistered blank cartridge, which has no volume to hang off;
- every draft's private drive column, which decision 1 already forbids.

A contact knows the drive, the cartridge (nullable — a contact can happen before the
cartridge is identified, which is precisely when a blank is being read), when it opened, when
it closed, what it was for, and how it ended.

## 3. `health_logs` becomes a child of the contact row — in exactly one rebuild

`contact_id` foreign key; `volume_id` becomes **nullable**, because a drive-only reading has
no volume. One rebuild, carrying every sibling's column requirement at once, under the
#227/#264 rebuild-verification standard.

Two things must survive it, and losing either would be the suite defeating its own purpose:

- **`raw_log`** — the only existing honouring of the capture-everything standard.
- **migration 009's NULL-vs-0 `tape_alerts` distinction.** 009 made that column nullable with
  no default *deliberately*: NULL says "not recorded", 0 says "recorded, and there were
  none". A rebuild that backfills 0 asserts "the drive reported no alerts" about collections
  that never looked. `report health` renders NULL as `-` and must keep being able to.

**Making `volume_id` nullable breaks a reader, and the rebuild owns fixing it.**
`report health` (`src/cli/report.rs:1196-1197`) is `FROM health_logs h JOIN volumes v ON v.id
= h.volume_id` — an INNER JOIN. The moment a drive-only reading exists, that report silently
drops it: the row is captured and invisible, which is the failure this suite exists to
prevent, reproduced by the suite's own fix. It is also precisely #293's defect — an output
that cannot distinguish "looked and found nothing" from "never looked" — in a second report,
and the remedy is the same one: drive the query from the table whose rows you must not lose.
Whoever writes migration 021 changes that query in the same commit, or the rebuild ships a
new blind spot.

**What does not become a column.** The counter-baseline work wants scope and delta columns. It
does not get them: with `contact_id`, a delta is the difference between two readings on the
same contact, which is a query, not stored state. Storing a derived figure beside the two
facts it is derived from is how the two disagree later.

**`session_id` keeps exactly the meaning its foreign key already declares** — the
`verification_sessions` row — and nothing may overload it. Four drafts proposed four different
meanings for a column with zero writers; a column that means four things means none.

## 4. The operation vocabulary is free TEXT, with a test pinning the known set

Migrations are forward-only and this vocabulary grows with every new tape-touching command; a
closed CHECK turns each new command into a schema change. A test asserting the observed set
matches an expected list gives the same typo protection at no migration cost — the pattern
#287 established for `LEGAL_VOLUME_STATUSES` against the live schema.

The existing `health_logs.operation` CHECK is the argument against itself: it permits `read`
and `clean`, **neither of which any code has ever written**. A closed vocabulary that is
already wrong in two of its four values is not protecting anything. And one sibling in this
very suite needs a fifth value — a draft making the case against the CHECK by existing.

## 5. The journal points at the contact, never the reverse

`journal.contact_id`, not `contact.journal_id`.

A single read-path command performs **two** MAM reads — `check_read_contact`, then
`loaded_medium_serial` — inside one contact. Giving each contact one `mam_journal_id` cannot
represent that, and collapsing the two reads to make it fit would destroy the thing the
journal exists to record. One-to-many points from the many.

## 6. The migration sequence, assigned

Eight drafts each claimed 019. 018 (`volumes.sealed_at`) is the highest today. The gating set
takes 019–023, in this order, because each depends on the one before it:

| # | what | issue |
|---|---|---|
| **019** | `drives` | #295 |
| **020** | `cartridge_contacts` | #296 |
| **021** | the single `health_logs` rebuild | #296 |
| **022** | `mam_journal` | #297 |
| **023** | the `sg_logs` page journal | #298 |

**The rebuild sits in #296, not #295**, although #295 owns the drives table and the
attribution defect that motivates it. It cannot precede the table it takes its foreign key
from, and the whole point of *one* rebuild is that it happens after every requirement is
known. Decision 6 delegated sequencing to the coordinator; this is that call, and it is the
only place this ADR departs from the order the issues were filed in.

Non-gating siblings (parsing, trend analysis, reporting) take 024 upward in the order they are
implemented. They add no table the gating set depends on — which is the same reason they do
not gate the first write.

## 7. Every journal row records the observer

`tapectl_version` on every row. "Parse it later" requires knowing which build wrote the parsed
columns beside the raw text — a parser bug fixed in a later version is indistinguishable from
a hardware change unless the rows say which parser produced them. The precedent exists:
`tapectl_version` already reaches the tape (`src/volume/write.rs:399`).

## Two hazards this record model must be implemented around

**Reading is not always free.** TapeAlert (log page 0x2E) is described by SSC-3 as cleared
when read. If that is how these drives behave, then a second read within one contact returns
zeros and the first read's evidence is gone — the journal would faithfully record that
nothing was wrong. So the rule is structural, not advisory: **each page is read at most once
per contact, its raw text is journalled, and every consumer reads the journal rather than the
drive.** This binds the work that enumerates page 0x00 and dumps every supported page — it
must not re-read a page health collection already took. The rule is correct whether or not
0x2E is read-to-clear; that it may be is what makes it worth enforcing structurally instead
of by convention. Confirm the behaviour at the real-drive rehearsal.

**Capture stores the number the hardware gave, and its label.** Per #182's ruling, a figure
read off MAM is journalled as the raw integer together with the unit the attribute named it
in, and converted only at display. Capturing a converted value discards the observation in
favour of an interpretation of it, which is the opposite of this ADR's standard — and #182
exists because exactly that conversion is currently done wrong in one place.

## What this does not decide

Parsing the journalled text into queryable columns, trend analysis over contact pairs, and
reporting are all deliberately **out of the gating set**. They can be added at any time from
journalled data. A contact that happens before the journal exists is unrecorded forever —
which is the CTO's own stated standard, and the whole reason the capture half gates the first
production write and the analysis half does not.

## Amendment, 2026-09-23 — read paths take a health reading too (#320)

*Ruled by the CTO on 2026-09-23 ("yes" to #320's recommended option).*

This ADR said **how** a log page is read (at most once per contact, journalled verbatim,
consumers read the journal) but not **which commands** take a reading. Until now only
`volume write`, `volume resume` and `volume verify` swept the log pages and wrote a
`health_logs` row. The read paths took both MAM reads and, since #314, named their drive,
but recorded no error counters.

**Ruled: every read-path contact takes the same post-command sweep.** That means
`restore unit`, `restore file`, `restore raw-volume` and `catalog rebuild --from-volume`,
plus any other command that opens a contact to read tape data. They use the same
`log_pages::sweep`, the same journal and the same `health_logs` row, with a new reading
kind for restore. The operation vocabulary is free text (§4), so no migration follows from
the kind itself.

**Why:** a drive fault shows up on the read path. Read-error counters (page 0x03) taken
during a restore are the most direct evidence this suite has for "is it the drive or the
tape?", and #314 attributed every contact to a drive for the same reason.

**Accepted costs.** One sweep per read-path contact (13 LOG SENSE on mhvtl, 22 on the HP
LTO-6), plus one INQUIRY. If 0x2E proves to be read-to-clear, a restore now consumes the
TapeAlert flags. Nothing is lost, because they are journalled against that contact and
consumers read the journal (§ "Two hazards"). The once-per-contact rule is unchanged: a read
path must not add a second sweep to a contact that already has one.

**Not changed:** a command killed before it finishes takes no sweep, because collection
runs after the command. That remains a known gap in post-command collection. It is not
repaired by guessing.

## Amendment, 2026-09-23 (evening) — every contact sweeps, `volume init` included

*Ruled by the CTO on 2026-09-23 (grilling Q5, "ratify all"), after the real-drive
rehearsal (`docs/runs/2026-09-23-real-drive-rehearsal.md`) showed every `volume init`
contact on the HP LTO-6 with zero `log_page_journal` rows.*

The read-path amendment above listed which commands sweep and left `volume init` out.
**Ruled: the rule is "every contact takes one post-command sweep", with no exceptions
list.** `volume init` is the first contact a cartridge gets on a drive and writes File 0;
its reading is the baseline for that cartridge's life on that drive (page 0x17's load
count and lifetime megabytes are most useful *before* the first write). Cost: one 22-page
sweep per init on the HP. Tracked as #339. The once-per-contact rule is unchanged.

