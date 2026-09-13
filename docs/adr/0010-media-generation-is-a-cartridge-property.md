# Media generation is a cartridge property; a drive declares what it can write; capacity follows generation

Until 2026-09-13 the `[[backends.lto]]` block — a *drive* — carried `media_type` and
`nominal_capacity`, and every capacity decision in the tree read them from the **first**
backend entry regardless of `--device` (`volume init`, `volume write`, `resume`, `verify`,
`collection plan`, `volume plan`). Issue #141 made the consequence concrete: an LTO-6 drive
writes LTO-5 media, but a volume on an LTO-5 cartridge was planned as 2.5 TB, recorded as
`LTO-6`, and would have run to a real end-of-tape — a clean abort under ADR-0003, but a wasted
multi-hour write and a catalog row that lies. The design (§2.5, §2.8) had the right nouns
and the wrong owner: the generation of the *medium* is a fact about the cartridge in the
drive, and the drive's only media fact is which generations it can write and read.

**Decision.** Three things move, once, and nothing else in the model changes:

1. **A drive declares its native `generation`** (`[[backends.lto]].generation = "LTO-6"`).
   `media_type` and `nominal_capacity` leave the drive block; the latter survives only as an
   optional `capacity_override`, whose sole legitimate users are virtual drives and the
   microcosm harnesses that make mhvtl pretend a tape is 2400 MB. Stale keys are rejected by
   the parser, by name, so an old config cannot be silently misread as "no override".
2. **The medium's generation is detected from the drive at `volume init`**, not declared:
   MAM *medium density code*, else MAM *format density code*, else the `st` driver's density
   register (`MTIOCGET`). Only when none of those yields a known code does tapectl fall
   back to a declaration — `--media`, then a serial-matched cartridge row, then the drive's
   own generation — and it says so. A `--media` that contradicts a detected code is an error,
   not a hint. A drive that cannot write the detected generation is a hard refusal that
   `--force` does not override: it is a physical fact, not a consent tier (ADR-0008).
3. **Capacity is a function of generation** with two overrides in a fixed order: the drive's
   `capacity_override` (the drive lies, as mhvtl does) → the bound cartridge row's
   `nominal_capacity` (an operator said so at `cartridge register --capacity`) → the
   generation table. It is decided once at `volume init`, stored in `volumes.capacity_bytes`,
   and every later gate (`write`, `resume`, `verify`) reads the volume row. Config is never
   consulted for capacity after init. MAM's reported capacity stays informational, as §2.8
   layer 2 intended and as the mhvtl finding (v2-open-questions §D) requires.

**Init binds the cartridge.** The design's `cartridge_volumes` join and the retire →
`pending_erase` hand-off both existed only in tests: no production path ever wrote the join,
because nothing knew which cartridge it was writing. The MAM medium serial is that
knowledge. `volume init` matches it to `cartridges.serial_number`; failing that it binds the
`--cartridge <barcode>` the operator names and records the serial on that row; failing that
it auto-registers a cartridge whose barcode *is* the serial and says so. A registered row
whose `media_type` disagrees with the detected generation is an error — either the row is
wrong or the wrong tape is loaded, and tapectl cannot tell which. That contradiction is the
*only* new refusal binding introduces: it is a fact error, not a risk judgement.

**Binding adds no second consent gate.** The tempting rule — refuse when the cartridge is
still bound to a live volume, and make `--force` or the retire lifecycle the way past — was
considered and rejected. `volume init` already asks the tape itself: File 0 naming a sealed
volume is refused unless `--force` (ADR-0003, #27), and that is the decision point, made
against the medium's own evidence rather than the catalog's weaker claim about it. An
operator who reached init past File 0 either loaded a blank or erased tape — in which case
the data is already physically gone — or gave `--force`, which is the consent. Demanding a
second override for the same act is the ceremony ADR-0008 warns about, and it would have
made a physical `mt erase` followed by `volume init` — the most ordinary reuse there is —
into a two-command catalog dance that teaches operators to reach for `--force` by reflex.

So init *records* the displacement instead of relitigating it: the open
`cartridge_volumes` mount is closed, the displaced volume moves to `erased`, an events row
says why, and a warning names it together with any unit that just lost its last copy
(`retire_impacts`, already written for `volume retire`). Nothing is blocked, which is
ADR-0004; the catalog stops crediting a copy that no longer exists, which is the failure the
lifecycle suite's own comment predicted ("single-cartridge copy counts may over-credit it");
and `audit` reports the new coverage truthfully on the next run. Where no serial is readable
(mhvtl exposes none) the volume is written unbound with a warning, so the virtual harnesses
lose nothing. `volume write` re-reads the serial and refuses a cartridge that is not the one
init bound — the same wrong-cartridge discipline as the File 0 check, one layer earlier.

**Read paths stay usable without a configured drive.** The strict device→backend resolution
below governs the write paths, which genuinely need the drive's factor, ENOSPC buffer and sg
node. `identify`, `verify`, `read-slices`, `restore` and `catalog rebuild` take an explicit
`--device` as given and treat the backend as optional, because the machine that most needs
them is the rebuilt one that has keys and no `backend add` yet (ADR-0005's DR path). They
need no configured capacity: after init, capacity lives on the volume row.

**Backends resolve by device.** `--device` no longer defaults to `/dev/nst0` — the exact
numbering hazard `docs/lto6-drive-passthrough.md` warns about — and no path reads
`backends.lto.first()` again. With `--device`, the backend is the entry whose `device_tape`
is that path (canonicalised, so a by-id link and its `/dev/nstN` target agree); without it,
the *sole* backend is used, and two or more backends make `--device` mandatory. This is the
fix for multiple physical drives, which the old code could configure but never select.

**What this does not change.** The on-tape format: the ID thunk and MANIFEST keep every
field name and shape (`media_type`, `nominal_capacity_bytes`, the `[media]` MAM block); only
values change, and `tests/on_tape_golden.rs` must stay green without a re-pin (ADR-0007).
The `volumes.media_type` and `cartridges.media_type` columns keep their names and now always
hold a generation string tapectl can parse. Tenants, units, policy, the write session, the
Store trait — untouched.

**Facts encoded, and their sources.** Density codes are the `st` driver's, as listed in
mt-st's `mt.c` (0x42 LTO-2, 0x44 LTO-3, 0x46 LTO-4, 0x58 LTO-5, 0x5A LTO-6, 0x5C LTO-7,
0x5D LTO-7 Type M, 0x5E LTO-8, 0x60 LTO-9; 0x40 is shared with DLT1 and is accepted as
LTO-1 only by declaration). Compatibility is the LTO consortium's published chart
(lto.org/lto-generation-compatibility): generations 1–7 write their own and the prior
generation and read two back; LTO-8 reads and writes LTO-7, LTO-7 Type M and LTO-8; LTO-9
reads and writes LTO-8 and LTO-9 only; LTO-10 is LTO-10 only. Native capacities are the
marketed decimal figures (LTO-5 1.5 TB, LTO-6 2.5 TB, LTO-7 6 TB, Type M 9 TB, LTO-8 12 TB,
LTO-9 18 TB, LTO-10 30 TB). LTO-10 ships in both 30 TB and 40 TB cartridges, which a single
generation figure cannot express; the table carries 30 TB and the cartridge row's
`nominal_capacity` — set at `cartridge register --capacity` and ahead of the table in the
precedence — is how a 40 TB cartridge is declared. Both tables live in one module (`src/media.rs`) with a unit test
per row; they are not derived from a formula, because the formula stopped holding at LTO-8.

**Consequences.** `backend add` takes `--generation` and `--capacity-override`; `cartridge
register` validates `--media-type` and defaults `--capacity` from the table; `collection plan`
and `volume plan` take `--media` for planning a tape that is not loaded, defaulting to the
drive's native generation. Migration 011 adds a partial unique index on
`cartridges.serial_number`. `first-run.sh` stops asking the operator to register the
cartridge by hand — init does it — and asks for the drive's generation instead of a media
type. Binding also makes the **cartridge lifecycle live for the first time**: `in_use` was
never written by any production path, and with it reachable, `retired_permanent` and
`offsite` become states a cartridge can be pushed toward but never enter, because no command
writes them (#148). Two consequences of the same dormancy — `compact-finish`'s
`pending_erase` hand-off and `cartridge mark-erased`'s cascade both queried an always-empty
join table — start working as written. The design's §2.8 capacity model is recast (layer 1 now reads the generation table
rather than config) and §2.5 is extended (the binding the design assigned to `cartridge
register` reading MAM is made by `volume init`); both are recorded in `design-errata.md`.
