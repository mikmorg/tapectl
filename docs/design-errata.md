# Design-doc errata: how to read tapectl-design-v4_0.md now

`tapectl-design-v4_0.md` remains the implementation reference for everything not
listed here, **read together with `CONTEXT.md` (vocabulary), `docs/adr/`
(decisions), and — for the on-tape format and write session — the two normative
design notes `docs/design/volume-format-v2.md` (byte format, governed by
ADR-0007) and `docs/design/layout-session.md` (state machine)**. Where they
disagree, the ADRs and normative notes and the verdicts recorded on closed
renovation tickets take precedence. This file is the complete list of known
divergences as of 2026-07-21 (on-tape format v2 close); it is maintenance-light
by design — a full v5 rewrite is deliberately deferred until after phase 1
stabilizes the Layout/Session model.

Status meanings: **Rejected** — the design's mechanism will never be built.
**Superseded** — replaced by a newer decision. **Recast** — the design's intent
survives but its mechanism is restructured. **Resolved** — the design
self-conflicted; one side won. **Extended** — still true, with new concepts
layered on.

| Design section | Status | Authority | What changed |
|---|---|---|---|
| Appendix D — Tape append | **Rejected** | ADR-0003 | Sealed volumes are immutable; append is rejected, not deferred. `volume write` refuses non-blank tapes (#27). Resuming an *interrupted, never-sealed* session is not append and remains allowed. |
| §5 CLI — `volume append` | **Rejected** | ADR-0003, #27 | Never implemented; the read-ID-thunk-and-refuse behavior replaces it. |
| §8.1–§8.8 + §2.6 — Volume file formats & zone list | **Superseded** | ADR-0007; `volume-format-v2.md` | The whole on-tape format is redefined as Layout Version 2: a plaintext **front index** (File 3) with per-file position/type/size + ciphertext hashes, **envelopes before slices**, and a trailing plaintext **seal marker**. The v1 mid-tape mini-index and the "8/10-file" labels are gone. `volume-format-v2.md` is authoritative for the bytes. |
| §2.9 + Appendix C — End-of-tape recovery | **Rejected** | ADR-0007, #26 | The three-layer salvage (overwrite-incomplete, sacrifice-last-slice) is deleted, not recast. A real EOT is a **clean abort to an unsealed tape**; the pre-flight capacity gate (#28) is the sole capacity defense. `writes.eot_recovery` and `writes.sacrificed_slice_id` become inert schema reserve (dead columns, never written); the config `manifest_reserve` collapses to the ENOSPC buffer. |
| §2.24 — Signal handling vs §4 — Recovery | **Resolved** | ADR-0002, #25; triage on #17 | The design self-conflicted (§2.24: interrupted resumes; §4: startup converts interrupted→aborted). §2.24 wins: `interrupted` is resumable while the session's Layout stays valid; the startup sweep marks orphaned `in_progress` sessions `interrupted` (crash), never `aborted`. |
| §2.29 — LTO drive access (variable block mode, `MTSETBLK 0`) | **Superseded** | implementation standard; audits #3/#5; #27 doc trail | Fixed 512 KB blocks are the standard everywhere (write path, RESTORE.sh, guide). The ID-thunk instruction bug (`dd bs=64k`) is #29. Hardware compression off (`MTCOMPRESSION 0`) lands with #28. |
| §2.6 — "8-file"/"8-zone" labels | **Superseded** | ADR-0007; `volume-format-v2.md` (see the §8.1–§8.8 row above) | Both the counts and the zone order are stale: the v2 layout front-loads the index (File 3), writes envelopes before slices, and ends with a seal marker. Use `volume-format-v2.md` §1 for the current zone order. |
| §2.3 / §4 — `snapshot_type` differential/incremental, `base_snapshot_id` | **Rejected** (for now) | #12 verdict | Full-only stands. Columns remain inert schema reserve. Reopen triggers and the differential-only pre-agreed shape are recorded on #12. |
| §6 — dar catalog management (XML listing path) | **Superseded** | #12 verdict, #42, #39 | The SQLite walk tables are the catalog source. #42 removes `catalog_xml.rs` + quick-xml (dead: zero `use quick_xml` sites remain). **`extract_catalog`/`catalog_path` are NOT removed** — #39 revived the isolated dar catalogue for selective heir restore, and they are live (`src/staging/mod.rs:188` on every first stage; consumed by `volume/build.rs` and `volume/write.rs` for envelope catalogues). Deleting them breaks the heir path. |
| §2.16 — Encryption & keys | **Extended** | ADR-0005, #68/#69 | A permanent **escrow recipient** participates in every write and is exempt from `key rotate` (which refuses if it is absent). The Heir Kit replaces the doc's `key export --qr` idea (that flag is superseded; see LOW umbrella #66 rider). |
| §2.5 — Cartridges & locations; §2.7 — Export layout | **Extended** | ADR-0006, #71–#73 | Locations gain a kind: physical shelf vs **warehouse** (S3 API, cold storage classes). One storage interface; TapeStore/WarehouseStore/ExportStore are peers. Export output becomes a store on the same seam (its narrow H11 fix is #37 regardless). `volumes.storage_class` (already in schema) becomes meaningful with #73. |
| §7 — Configuration (commented-out S3 backend block) | **Superseded** | ADR-0006, #73 | The future-S3 sketch is replaced by warehouse locations. Decorative keys (`block_size`, `device_tape`, …) are wired-or-deleted by #62; some may be consumed by epic #20 children instead. |
| §3 / §9 — `src/backend/` "deferred" | **Superseded** | ADR-0006, #71 | The store trait is decided, carved during the phase-1 Layout work with TapeStore first — not deferred, and not designed speculatively either. |
| §2.18 — Verification | **Extended** | ADR-0001/0004/0006/0007; #23 | Verification converts claims into evidence; recorded strength must match what ran (`quick` vs `full`, #23). Under v2 confirm is a **forward readback from BOP** — read the seal marker, diff the front index (navigable tier), hash each file vs the front index's ciphertext hashes (integrity tier); `volume-format-v2.md` §5. Warehouse copies carry a distinct evidence class: deposit receipt + provider attestation, aging without refresh. |
| §2.8 — Capacity/MAM | **Unchanged in intent** | #28, ADR-0007 | Listed here only because it is entirely unimplemented today; #28 builds it as specified, gated into the Layout validation step — and under ADR-0007 it is the *sole* capacity defense. Its reserve is just the ENOSPC buffer (no manifest reserve). |
| §8.7/§8.8 — Envelope contents | **Extended (code lags)** | #39, ADR-0007 | The design is right and the implementation is missing dar catalogs + operator `catalog.db`; #39 closes the gap. Under v2 envelopes are written **before** the data slices (D2). Not a supersedence of the contents — listed to prevent misreading the current code as intended. |
| §10 — M7 checklist claims | **Corrected in place** (one box) | audit #5 T6, #64 | This row previously said interrupted-write, ENOSPC, and raw-volume-restore tests "do not exist". Two thirds of that is now stale: the `session.rs` TDD cycles (2026-07-27, a week after this row was written) landed genuine interrupted-write, resume, and fault-injected ENOSPC tests, and corrupted-staging is covered well past the crypto layer. Only **raw-volume restore** remains uncovered, and per #63 that is a missing *command*, not a missing recovery route — the heir path serves it. §10 has been split accordingly rather than blanket-superseded. |
| §1 title framing ("Multi-Tenant") | **Clarified** | intent statement (#2) | Tenants are the one operator's *data classes*, not separate people. Multi-tenant isolation properties still hold and are still tested. |

Everything else in v4.0 — the three-phase pipeline, the volume file formats
(§8.1–§8.6), the schema (§4), bin-packing, policy resolution, compaction,
receipts, labels — remains authoritative as written.
