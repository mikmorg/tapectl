# A cartridge's place is a location; its status is only its fitness to hold data

ADR-0010 makes `volume init` bind the cartridge it writes, which turns the cartridge
lifecycle from a diagram into running code for the first time. That exposed two dead ends
(#148). `cartridges.status` has five values in its CHECK constraint and only three writers:
`register` sets `available`, `mark-erased` returns it to `available`, `compact-finish` sets
`pending_erase` — against a join table no production path had ever written a row into, so it
matched nothing. Nothing anywhere writes `retired_permanent` or `offsite`, and an operator
whose cartridge fails, or who ships one to a relative's house, has no way to say so.

The two dead ends are not the same kind of thing, and fixing them the same way would have
been the mistake.

**`offsite` is not a status — it is a place, and the schema already has one.**
`cartridges.location_id` has existed since the first migration with zero readers and zero
writers. A cartridge is in exactly one place (migration 007 says so about locations
generally), and "offsite" is a location an operator named, not a state of the medium. Two
mechanisms for one fact is how they drift: a cartridge could be `offsite` and carry a volume
whose `location_id` says `home-rack`, and nothing would notice. So `offsite` leaves the CHECK
constraint and **location becomes the single mechanism**:

- `cartridge move <barcode> --to <location>` sets the cartridge's location and that of every
  volume bound to it. The cartridge is the thing that physically moves; the data goes with it.
- `volume move <label> --to <location>` keeps its name and meaning, and now also moves the
  cartridge the volume is bound to. A volume with no cartridge — a warehouse deposit, an
  export — moves alone, exactly as before.

Both write the same two rows, so the cartridge's place and its volumes' places cannot
disagree. `catalog locate` is unchanged and keeps answering from the volume.

**`retired_permanent` is a status, and it needs a writer with a consent gate.** A cartridge
retired for wear or read errors is not a location change and not an erasure: the data may
still be readable, but the medium must never be written again. `cartridge retire <barcode>`
writes it, and it is Tier 2 under ADR-0008 — it removes a physical copy from every coverage
count that policy computes, so the evidence is displayed first and `--force`/`--yes` is
required when any unit is left below its policy. It is the cartridge-level peer of `volume
retire`, which already does exactly this analysis, and it reuses that analysis rather than
growing a second one. A retired cartridge cannot be bound by `volume init`, and that refusal
is a fact error, not a risk judgement: ADR-0010 lets init record a displacement without
consent because File 0 already decided it, but no amount of consent makes a medium you have
declared unfit fit again. The escape is `cartridge mark-erased`, which is the operator saying
they were wrong.

**The lifecycle is therefore four states, each with a writer:**

```
available ──volume init──> in_use ──compact-finish/volume retire──> pending_erase
    ^                                                                     │
    └──────────────────── cartridge mark-erased ──────────────────────────┘
    │
    └──> retired_permanent  (cartridge retire; mark-erased is the only way back)
```

**Consequences.** Migration 012 rebuilds `cartridges` with the four-value CHECK and adds the
index on `location_id` the column never had. `cartridge list`/`info` gain the location
column, which is the question an operator asks of a cartridge most often and could not ask
before. `report fire-risk` and the location-count policy checks read volumes, not cartridges,
so they are untouched — but they stop being able to disagree with the shelf. Nothing about
tenants, units, keys, the write session or the on-tape format changes.
