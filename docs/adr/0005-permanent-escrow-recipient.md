# A permanent escrow recipient participates in every write and is never rotated

The security audit (docs/audits/2026-07-20-security-crypto-posture.md, S1) named the
system's heaviest real exposure: every private key lives on one machine, so site loss
destroys the keys and the co-located tapes' recoverability together — while the intent
statement requires decades-scale, heir-capable restore. We decided tapectl gains a
first-class **escrow recipient**: one identity generated once, whose public key is
appended to every future encryption's recipient list (slices and envelopes), exempt
from `key rotate` — and `key rotate` refuses to run if the escrow recipient is absent.
Multi-recipient age already implements the KEK/DEK envelope pattern, so this costs one
stanza per file and no new cryptography. The escrowed artifact is that identity,
printed plain (Bech32's charset and checksum exist for exactly the retype failure
mode), in tamper-evident envelopes across at least two independent failure domains,
alongside a DB snapshot age-encrypted to the escrow recipient itself, refreshed after
each production write session. Adopted before the first production tape exists, so the
mechanism's one weakness — it cannot cover tapes written before it existed — costs
nothing.

Consequences: the escrow identity's secret half is protected by physical custody for
the archive's whole life (a leak compromises the escrow line permanently; the remedy —
swap in a fresh escrow recipient — orphans pre-swap tapes from the new line and is a
deliberate, documented act); operational key rotation never touches the escrow line,
so the printed kit cannot silently go stale; the kit generation command owns printing,
QR encoding, cover instructions, and the encrypted catalog bundle.

Considered and rejected: enforced re-escrow discipline after each rotation (the
staleness failure is silent and partial — paper keeps decrypting old tapes and quietly
misses new ones — so discipline-by-memory is not a mechanism); passphrase-wrapping the
escrowed identity (adds a decades-later forgotten-passphrase failure mode against a
theft threat that tamper-evident custody already covers proportionately for personal
archive data); Shamir splitting (every surveyed age-compatible split tool is stale or
reference-grade, and the future-self-first custody profile has no multi-party
requirement to pay that complexity for); threshold decryption via age-plugin-sss
(every future decrypt would depend on a pre-1.0 single-maintainer plugin surviving
decades, and it only fits ongoing shared custody, not a one-time recovery).
