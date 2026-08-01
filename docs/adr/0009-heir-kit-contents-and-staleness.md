# The Heir Kit ships the whole catalog, prints from plain text, and reports its own staleness

ADR-0005 ends by assigning the kit generation command four responsibilities — "printing,
QR encoding, cover instructions, and the encrypted catalog bundle" — without saying what
any of them contain. Issue #69 sat deferred on the belief that the whole of it required a
physical step. That belief covered only half: **printing, tamper-evident envelopes and
distribution across failure domains are physical and remain the operator's; generating the
artifacts is ordinary software**, and its stated dependency (#68, the escrow recipient) has
been closed and live in the write path since the v2 regear. We decided to build the command
and stop at the printed artifact.

**The encrypted bundle is the FULL `tapectl.db`**, not the filtered `catalog.db` that #83
puts in the operator envelope. The two exist for different readers. The filtered schema was
designed to ride *on tape*, where the sacred invariant forbids tenant and unit names in
plaintext, and it therefore carries no `locations` and no `cartridges` — an heir holding it
could enumerate what the archive contains but could not learn which cartridge to fetch or
which building it is in, which is the one question the kit exists to answer. The full
database is safe to escrow because **it holds no secret material**: only `tenants.public_key`
is stored, and every private half is a file under `keys/`. Per ADR-0005 a leak of the escrow
secret already "compromises the escrow line permanently", so the marginal confidentiality
cost of the extra tables is not what decides this.

**The printed artifact is a self-contained HTML page plus a plain-text twin.** The HTML
inlines the QR as SVG and carries print styling, so the ceremony is one keystroke in any
browser — which matters because ADR-0005 requires it be repeated after every production
write session, and a ceremony with friction is a ceremony that gets skipped. `COVER.txt`
carries the same words and the same key in retypable Bech32, and it is the artifact with the
decades-scale claim: it survives every browser being gone and is readable with `cat`. A PDF
was rejected — it costs a heavy dependency in a tree that pins deliberately, and it is less
inspectable than text when the operator wants to verify what was printed.

**Kit staleness is recorded and surfaced advisorily.** ADR-0005 names the exact failure —
"the staleness failure is silent and partial: paper keeps decrypting old tapes and quietly
misses new ones" — and then rejects *enforced re-escrow discipline after each rotation*,
leaving a line printed on the cover sheet as the only defense. That is discipline-by-memory,
which the same paragraph calls "not a mechanism". We close the gap without crossing the
rejection: `key escrow-kit` writes an `events` row, and `audit` gains a check that WARNS
when volumes have been sealed since the last generation. It is advisory — exit 1, never 2 —
so ADR-0004 holds and nothing is blocked. An advisory check is neither enforcement nor
memory, and it fires on the condition that actually matters (new tapes exist that the paper
cannot reach) rather than on a rotation event.

Consequences: the kit command owns no physical step and can be run freely, so refreshing
after a write session costs nothing; the escrowed bundle grows with the catalog rather than
with the archive, so it stays small enough for paper-adjacent media; `audit` grows a seventh
operator-facing check, which is surface the six §2.20 checks did not have; and the QR
encoder is a new dependency, to be pinned and chosen without a transitive image stack.

Considered and rejected: escrowing the filtered `catalog_snapshot` across all stage sets
(shares one code path with the on-tape bundle, but withholds `locations`/`cartridges` — it
would hand an heir an inventory with no way to act on it); dropping `events` from the bundle
(the audit trail is the largest and most incidentally-revealing table, but it is also the
only record of what happened to a tape between writes, and an heir reconstructing a
damaged archive wants it); surfacing staleness only in a `report` command (avoids any
argument that this re-litigates ADR-0005's rejection, but a fact nobody is shown at the
moment it matters is a fact nobody acts on); and printing nothing but the cover sheet, with
tapectl tracking no state at all (honors the rejection literally and leaves the named
failure mode undefended).
