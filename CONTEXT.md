# tapectl

Multi-tenant archival storage management: one operator's data classes, archived
to LTO tape and exportable encrypted directories, restorable for decades by an
heir holding only a key envelope.

## Language

### Truth & reconciliation

**Claim**:
A catalog row asserting something about a tape's contents (e.g., "slice 7 of
stage_set S lives at position 12 on L6-0001"). Claims are not truth; they are
beliefs awaiting corroboration.
_Avoid_: record (ambiguous), fact

**Evidence**:
Recorded proof that a claim was checked against the tape it describes — a write
receipt or a verification session. Evidence has an age; claims with stale
evidence are weaker.
_Avoid_: verification (use for the act, not the artifact)

**Derivation**:
A cross-volume fact computed over claims — copy counts, fire-risk, retirement
impact. Never stored as truth; only as reliable as the weakest evidence among
its inputs.
_Avoid_: aggregate, statistic

**Assertion**:
An operator-supplied physical-world fact no medium can attest to: cartridge
location, cartridge↔volume binding. Recorded in the catalog, checked on
contact.

**Contact**:
Any moment a tape is physically in a drive and read or written. The only
moment the tape is authoritative, and therefore the reconciliation event:
every contact corroborates or contradicts outstanding claims and assertions.

**Divergence**:
A contradiction between a tape's self-description and the catalog's claims or
assertions, detected at contact.

**Quarantine**:
The volume state entered on divergence: no operation may rely on the volume's
claims until an operator reconciles them at contact.

### Writing

**Layout**:
The complete enumeration of every file a volume will hold — ID thunk through
operator envelopes, with positions, sizes, and checksums. A value, constructed
and validated before the first byte is written; all on-tape metadata is
generated from it.
_Avoid_: plan (collides with `volume plan` capacity preview), file list

**Write Session**:
One execution of a Layout onto a cartridge at contact, tracked by a cursor.
Interruption and end-of-tape recovery are Layout transitions, not accidents;
a session ends with the tape truthfully describing itself, or with the
catalog knowing exactly why not.
_Avoid_: write (the verb), job

**Sealed**:
The volume state after a session's confirm readback passes: the tape ends with
valid metadata describing everything before it, and will never be written
again. Only sealed volumes contribute claims to derivations. Sealed volumes
are immutable; there is no append.
_Avoid_: closed, finalized

**Unsealed**:
A volume mid-session or after interruption: slices may be on tape but the
trailing metadata is absent or unconfirmed. Not self-describing, not a copy;
resumable while the same session's Layout remains valid.
_Avoid_: open, partial

**Copy**:
A unit's stage_set claim on a sealed, unquarantined, unretired volume — the
unit of coverage in derivations. Seal status decides eligibility; evidence age
qualifies presentation (warnings at destructive moments) but never eligibility.
_Avoid_: backup, replica
