# Design-doc errata: how to read tapectl-design-v4_0.md now

`tapectl-design-v4_0.md` remains the implementation reference for everything not
listed here, **read together with `CONTEXT.md` (vocabulary) and `docs/adr/`
(decisions)**. Where they disagree, the ADRs and the verdicts recorded on closed
renovation tickets take precedence. This file is the complete list of known
divergences as of 2026-07-20 (renovation planning close); it is maintenance-light
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
| §2.9 + Appendix C — End-of-tape recovery | **Recast** | ADR-0002, #26 | The three-layer recovery intent survives, but as *Layout transitions* (truncate → regenerate metadata from the Layout as it stands), never as imperative patching mid-pipeline. All on-tape metadata is generated from the Layout, post-transition. |
| §2.24 — Signal handling vs §4 — Recovery | **Resolved** | ADR-0002, #25; triage on #17 | The design self-conflicted (§2.24: interrupted resumes; §4: startup converts interrupted→aborted). §2.24 wins: `interrupted` is resumable while the session's Layout stays valid; the startup sweep marks orphaned `in_progress` sessions `interrupted` (crash), never `aborted`. |
| §2.29 — LTO drive access (variable block mode, `MTSETBLK 0`) | **Superseded** | implementation standard; audits #3/#5; #27 doc trail | Fixed 512 KB blocks are the standard everywhere (write path, RESTORE.sh, guide). The ID-thunk instruction bug (`dd bs=64k`) is #29. Hardware compression off (`MTCOMPRESSION 0`) lands with #28. |
| §2.6 — "8-file"/"8-zone" labels | **Superseded** (labels only) | audit #3 Theme 7 | The enumerated zone list is current; the "8" counts are stale — the implemented layout is the 10-file form. |
| §2.3 / §4 — `snapshot_type` differential/incremental, `base_snapshot_id` | **Rejected** (for now) | #12 verdict | Full-only stands. Columns remain inert schema reserve. Reopen triggers and the differential-only pre-agreed shape are recorded on #12. |
| §6 — dar catalog management (XML listing path) | **Superseded** | #12 verdict, #42 | `catalog_xml.rs` + quick-xml and the write-only `extract_catalog`/`catalog_path` machinery are removed by #42. The SQLite walk tables are the catalog source. |
| §2.16 — Encryption & keys | **Extended** | ADR-0005, #68/#69 | A permanent **escrow recipient** participates in every write and is exempt from `key rotate` (which refuses if it is absent). The Heir Kit replaces the doc's `key export --qr` idea (that flag is superseded; see LOW umbrella #66 rider). |
| §2.5 — Cartridges & locations; §2.7 — Export layout | **Extended** | ADR-0006, #71–#73 | Locations gain a kind: physical shelf vs **warehouse** (S3 API, cold storage classes). One storage interface; TapeStore/WarehouseStore/ExportStore are peers. Export output becomes a store on the same seam (its narrow H11 fix is #37 regardless). `volumes.storage_class` (already in schema) becomes meaningful with #73. |
| §7 — Configuration (commented-out S3 backend block) | **Superseded** | ADR-0006, #73 | The future-S3 sketch is replaced by warehouse locations. Decorative keys (`block_size`, `device_tape`, …) are wired-or-deleted by #62; some may be consumed by epic #20 children instead. |
| §3 / §9 — `src/backend/` "deferred" | **Superseded** | ADR-0006, #71 | The store trait is decided, carved during the phase-1 Layout work with TapeStore first — not deferred, and not designed speculatively either. |
| §2.18 — Verification | **Extended** | ADR-0001/0004/0006; #23 | Verification converts claims into evidence; recorded strength must match what ran (`quick` vs `full`, #23). Warehouse copies carry a distinct evidence class: deposit receipt + provider attestation, aging without refresh. |
| §2.8 — Capacity/MAM | **Unchanged in intent** | #28 | Listed here only because it is entirely unimplemented today; #28 builds it as specified, gated into the Layout validation step. |
| §8.7/§8.8 — Envelope contents | **Unchanged (code lags)** | #39 | The design is right and the implementation is missing dar catalogs + operator `catalog.db`; #39 closes the gap. Not a supersedence — listed to prevent misreading the current code as intended. |
| §10 — M7 checklist claims | **Superseded** (three boxes) | audit #5 T6, #64 | Interrupted-write, ENOSPC, and raw-volume-restore failure-mode tests do not exist (features were absent); #64 corrects the checklist. |
| §1 title framing ("Multi-Tenant") | **Clarified** | intent statement (#2) | Tenants are the one operator's *data classes*, not separate people. Multi-tenant isolation properties still hold and are still tested. |

Everything else in v4.0 — the three-phase pipeline, the volume file formats
(§8.1–§8.6), the schema (§4), bin-packing, policy resolution, compaction,
receipts, labels — remains authoritative as written.
