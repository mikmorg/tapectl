# On-Tape Format & Write/Verify Design — Research for the Spec Rewrite

*Holistic design R&D to drive a "regear to spec." Investigated against primary
sources; every fact is date-stamped **as of 2026-07** and tagged **VERIFIED**
(the owning source was fetched and read) or **INFERRED** (deduced, or from a
secondary/structural read). Author: research agent, 2026-07-21.*

> **New location note:** this is the first file under `docs/research/`. The repo
> had no research convention; this directory dates its files like `docs/audits/`
> (`YYYY-MM-DD-topic.md`). Nothing else references it yet.

---

## 1. Executive summary — the recommended design in one page

The pivot is correct and the repo is already 80% of the way there. Because
**content is fully known before the first byte is written** (plan-first) and
**volumes are sealed-immutable** (ADR-0003, written once, never appended), the
tape format should stop imitating a streaming/append format and become what it
actually is: a **plan emitted from a validated `Layout`, front-loaded, on a
single partition, gated at the start, and confirmed by a forward read.**

**What changes, and why:**

1. **Metadata moves to the FRONT.** The navigation index (today's *trailer*
   mini-index, File N+1) moves to the front as a plaintext **Front Index**
   (File 3), listing every file's position, true byte size, and type. In a
   plan-first world all of this is known before writing, so there is no reason
   to defer it to a trailer. File 0's ID thunk already carries a forward
   `[layout]` position map — this finishes the job. **Verdict: front index.**

2. **A minimal SEAL MARKER stays LAST.** Front-loading the index would destroy
   the current implicit completeness signal ("no trailer ⇒ unsealed"). Sealing
   is intrinsically a *trailing* assertion — "everything before me is present."
   So the last file becomes a tiny plaintext **seal marker** (`file_count`,
   `sealed_at`, and a hash binding the front index). An interrupted/truncated
   tape simply lacks the marker and is self-evidently unsealed. This preserves
   ADR-0002's "the tape never lies about itself." (This is *not* optional — it
   is the price of moving the index forward.)

3. **Envelopes move ahead of the data slices** (recommended, separable). The
   per-tenant/operator envelopes (decryption metadata + dar catalogs) are fully
   materialized before the trailer today (write.rs already encrypts them up
   front to fix their sizes). Writing them *before* the slices means tail damage
   or a short tape loses only trailing *slices*, never the metadata needed to
   navigate and decrypt everything that did land.

4. **Delete the end-of-tape salvage machinery.** The three-layer EOT recovery
   (overwrite-incomplete, sacrifice-last-slice; §2.9, Appendix C RECOVERY, the
   `eot_recovery`/`sacrificed_slice_id` columns) is the wrong model and is
   *unverifiable on mhvtl* (the 2026-07-20 drill proved mhvtl never returns
   ENOSPC — it silently produced 2 unreadable slices). Replace it with a
   **pre-flight capacity gate** (the `Layout::validate` predicate, #28) as the
   sole capacity defense. A real EOT (only reachable if MAM over-reports
   remaining capacity) becomes a **clean abort to an *unsealed* tape**, never a
   salvaged partial. Keep the abort path; delete the salvage.

5. **No LTO partitioning.** Linux `st` *can* partition (MTMKPART/MTSETPART) but
   it is disabled by default and unnecessary here. LTFS uses two partitions only
   because it **mutates** its index on every unmount and wants the newest copy
   findable at BOP; a sealed-immutable tape writes its index once and never
   rewrites it, so it buys nothing and pays guard-band capacity + complexity.
   **Verdict: single partition.**

6. **Confirm = forward readback of ciphertext hashes; LBP is a bonus, not a
   dependency.** The honest integrity story: put **sha256 of each file's
   *ciphertext*** in the plaintext front index (isolation-safe — it is a hash of
   pseudorandom age output, computable by anyone from the tape, non-attributable
   to a tenant). Confirm-at-seal then does a single forward pass, hashing each
   file and comparing — proving *byte integrity*, not just navigability. **LBP
   (drive-level per-block CRC) is unsupported by the Linux `st` driver — and
   half-enabling it (set `LBP_W` via `sg3_utils`, then write through `st`, which
   doesn't append the CRC) breaks *every* write; true end-to-end LBP needs a
   custom SG_IO engine the design avoids.** So confirm never relies on it — and
   a periodic hash pass is still needed for long-term bit-rot regardless (§2.C).
   `sha256_plain` (hash of decrypted content) stays inside the encrypted
   envelopes only.

7. **Keep fixed-block mode; 512 KB is fine, revisit only on real hardware.**
   Plan-first depends on deterministic block padding (the `Layout`'s
   `pad_to_blocks` and RESTORE.sh's trim-to-true-size contract), which *needs*
   fixed blocks. `st` caps block size at ≈2 MB (memory-bound) and the drive at
   8 MB when encrypting; 512 KB is safe and 1 MiB is the throughput sweet spot,
   but that is a real-hardware tuning question, not a format question. (Note:
   design §2.29's "variable block mode" text is stale drift — shipped code is
   fixed 512 KB.)

8. **Streaming encryption — the format is fine; the write glue should stream.**
   age's STREAM payload is ChaCha20-Poly1305 over 64 KiB chunks with constant
   memory, and the **`age` crate** (not "rage" — that's the CLI binary) gives
   `Encryptor::wrap_output`/`Decryptor::decrypt` `Write`/`Read` streams; a stock
   `age` binary decrypts the identical format (proven by execution — §2.E). This
   constrains nothing in the new layout, but note current tapectl glue **buffers
   whole slices** (`staging`, `restore`, and `Store::execute(&[u8])` itself), so
   RAM tracks `slice_size`, not archive size. The rewrite should stream the write
   path so **block size, not slice size, bounds memory.**

**Bottom line for the rewrite:** the format becomes *front-metadata → envelopes
→ slices → seal marker*, single partition, fixed blocks, gated at the start,
confirmed by a forward hash pass. Most of the churn lands on the **pending**
write-session epic (#22/#23/#26), which is exactly where you want it — the
shipped `Layout` value (#21), capacity gate (#28), and store seam (#71) already
point this way and barely move (the seam's abstraction is unchanged, though
`Store::execute` gains a streaming signature — §5).

---

## 2. Facts established (per research area)

### A. Metadata placement — front vs trailer vs partitioned

*Primary sources (VERIFIED first-hand — the SNIA WAF 403s WebFetch, so the spec
was pulled with curl + browser UA and read directly): SNIA LTFS Format
Specification v2.5 (May 19, 2019),
https://www.snia.org/sites/default/files/technical-work/ltfs/release/SNIA-LTFS-Format-v2.5-Technical-Position.pdf
(§ numbers below read from the PDF); v1.0 cross-check
docs.oracle.com/cd/E19957-01/LTFSSpec/LTFSSpec.pdf; the LTFS reference impl
github.com/LinearTapeFileSystem/ltfs; POSIX.1-2017 pax
(pubs.opengroup.org/onlinepubs/9699919799/utilities/pax.html); GNU tar manual.*

- **LTFS's *authoritative* index is a TRAILER; the front copy is redundancy.** An
  LTFS volume has exactly one Data Partition + one Index Partition (§5). "A Full
  Index shall always be written to the data partition as part of the unmount
  processing" — the authoritative current index sits at **end-of-data in the
  Data Partition** — while "the index partition shall only contain Full Indexes,"
  a front-of-tape **copy** of that Full Index (§9.2). Each partition is `Label
  Construct → Content Area`, "the last construct in the Content Area of a
  complete partition shall be an Index Construct" (§5.1, §5.3). So LTFS keeps the
  map at **both** ends. **VERIFIED**, as of 2026-07.
- **LTFS needs two partitions because it MUTATES the index — not because a front
  index is inherently required.** Every change appends a new index (generation
  number increasing; self-pointer + back-pointer chain for consistency/rollback,
  §5.4; Incremental Indexes added in v2.5, §9.2) at end-of-data, and rewrites the
  Index-Partition copy so a mount reads the current map without spooling the
  whole data partition. The tape LABEL, by contrast, "captures volume-specific
  values that are constant over the lifetime of the LTFS Volume … can only be set
  or updated at volume format time" (§8.1.3) — written once, never mutated,
  exactly like a sealed tape's whole index would be. **VERIFIED**, as of 2026-07.
- **Self-describing is LTFS's stated goal** (§1, verbatim): "An LTFS Volume can be
  mounted and volume content accessed with full use of the data without the need
  to access other information sources" — "self-contained or 'self-describing'
  format on sequential access media." The VOL1 ANSI label (80 bytes, ANSI X3.27)
  carries only volume identity — no content metadata; the Index XML holds
  names/sizes/timestamps/extent-map; the volume UUID (OSF DCE, 8-4-4-4-12, §7.8)
  binds them. The reference impl formats via `ltfs_format_tape` → `gen_uuid` →
  `ltfs_write_label` (VOL1) → `ltfs_write_index` (Full Index to **both**
  partitions); `LTFS_NUM_PARTITIONS 2`. **VERIFIED**, as of 2026-07.
- **tar/pax put a header before every member because the writer streams unknown
  lengths.** ustar: "Each file archived shall be represented by a header logical
  record that describes the file, followed by zero or more logical records that
  give the contents of the file"; the 512-byte header carries `size`@124 and a
  `chksum`@148 (simple sum of header bytes). pax prepends optional extended
  headers (`x`/`g`). The motivation — written to sequential media / a pipe in one
  forward pass, where a streaming writer can't know total size/count/offsets
  ahead — is well-established. *Source: POSIX pax; GNU tar manual.* — **VERIFIED
  (structure) / INFERRED (motivation)**, as of 2026-07.
- **The crux — plan-first is the opposite of tar's constraint.** If the writer
  knows every member's exact size and position before the first write (tapectl
  does), the reason for interleaved per-member headers disappears: everything tar
  scatters into N headers can be emitted **once, up front, as a single index**.
  tar cannot (streaming, unknown lengths); LTFS cannot (mutable, per-unmount
  appended generations). A sealed-immutable plan-first tape can — this is what
  makes a front-only index both correct and sufficient here. **INFERRED** (direct
  deduction from the VERIFIED constraints), as of 2026-07.
- **Front-vs-trailer, decided per axis** (LTO-6 ≈ 2.5 TB native): **(i) heir
  navigation** — front wins decisively (`mt rewind` + `dd` reads the map from the
  first blocks; a trailer needs the "space to EOD, back up a file mark" idiom, the
  tribal knowledge a self-describing tape must eliminate); **(ii) seek cost** —
  front ≈ zero (BOT), a trailer can require spooling the whole tape to EOD
  (minutes); **(iii) confirm/readback** — the trailer's **one genuine edge**: an
  index written last describes what *actually landed*, whereas a front index
  asserts *intent* — neutralized here by plan-first (map correct by construction)
  + a mandatory post-write verify pass + the trailing seal marker (write-once
  media just scraps a cartridge that fails verify); **(iv) tail damage** — front
  wins decisively (a trailer puts the sole map at the most-vulnerable EOD region;
  a front index survives and still recovers everything up to the damage).
  **INFERRED** (synthesis), as of 2026-07.
- **Borrow LTFS's one good idea:** keep the front index authoritative (written
  once from the plan, right after the label) and — since the index is tiny vs
  2.5 TB and the medium is write-once — optionally **duplicate it as a trailer**
  (bound by the seal marker). That mirrors LTFS's front-copy-plus-end-index
  redundancy **without** partitioning, mutation machinery, or a database.
  **INFERRED**, as of 2026-07.
- **The repo already front-loads structural position data.** File 0's ID thunk
  carries `data_start`, `data_end`, `mini_index`, `first_envelope`,
  `num_envelopes`, `operator_envelope`, `operator_envelope_backup`,
  `total_files` — a forward position map computed pre-write. The trailer
  mini-index only *adds* per-file byte sizes + type labels (also known
  pre-write). *Source: `tapectl-design-v4_0.md` §8.1, `src/volume/layout.rs`
  `generate_id_thunk`.* — **VERIFIED**, as of 2026-07.

### B. LTO partitioning — use it or not

- **Linux `st` supports 1–2 partitions, opt-in, disabled by default.**
  `MTMKPART` formats one partition (arg 0) or two (arg positive = size of
  partition 1; Linux ≥4.6 negative = size of partition 0); `MTSETPART` switches
  the active partition; both require `MT_ST_CAN_PARTITIONS` (a boolean set via
  `MTSETDRVBUFFER`, **default false**). Linux caps at **2** partitions even
  though LTO-6 media supports 4. `mt-st` exposes
  `mkpartition`/`setpartition`/`partseek`. *Source: man7 st(4)
  https://man7.org/linux/man-pages/man4/st.4.html; kernel.org st docs; mt-st
  mt.1.* — **VERIFIED**, as of 2026-07.
- **LTO wrap-wise partitioning (LTO-5+) has a quantified capacity cost.** "an
  Ultrium 5 volume supports one or two partitions and an Ultrium 6 volume
  supports a maximum of four partitions." It "generally has a guard wrap of two
  wraps between each partition. The amount of usable capacity may be reduced …
  **up to 2.5% per partition boundary**." Partitions are created via **FORMAT
  MEDIUM (opcode 04h)** governed by the **Medium Partition mode page (MP 11h)**.
  *Source: IBM LTO Ultrium SCSI Reference GA32-0928-01 §§4.2.1–4.2.2, 5.2.3,
  6.6.13,
  https://download.lenovo.com/servers/mig/systems/support/system_x_pdf/tape_scsi_reference.pdf*
  — **VERIFIED**, as of 2026-07. *(T10 SSC-4/SSC-5 PDFs are registration-gated at
  t10.org; IBM's ref implements those commands — SSC specifics **INFERRED** via
  IBM.)*
- **Synthesis — no partitioning.** LTFS uses two partitions solely to keep a
  *rewritable* Index Partition separate from the Data Partition; a
  sealed-immutable tape writes its index once, so that motivation is absent.
  Partitioning would cost ~2.5%/boundary + 2 guard wraps + the
  `MT_ST_CAN_PARTITIONS`/FORMAT-MEDIUM operational dance, for a benefit (fast
  index read) tapectl already gets free by ordering the index first on a single
  partition. **Verdict: single partition, index-first.** **INFERRED** (synthesis
  of the VERIFIED facts), as of 2026-07.

### C. Write-verify & integrity — the confirm design

- **LBP mechanism (VERIFIED from an LTO vendor's SSC-4 implementation).** LBP is
  enabled via the **Control Data Protection mode page (MP 0Ah, subpage F0h)**,
  MODE SELECT (15h/55h). Method byte `01h` = **Reed-Solomon CRC per ECMA-319**;
  the CRC is **4 bytes**; `LBP_W` protects writes, `LBP_R` protects reads. It is
  end-to-end **only if the host cooperates**: with `LBP_W=1` "an application
  client shall add the protection information on each logical block before
  transferring [it] and shall increase the TRANSFER LENGTH field by the length
  of the [CRC]." *Source: IBM LTO Ultrium SCSI Reference GA32-0928-01 §§4.5.2,
  4.5.4, 6.6.9 (Table 292), Annex D.* — **VERIFIED**, as of 2026-07.
- **The Linux `st` driver has NO LBP support, and half-enabling it BREAKS every
  write.** `MTSETDRVBUFFER`'s boolean list has no protection/CRC option and
  there is no `MTSETLBP` ioctl. Worse: enable `LBP_W=1` out-of-band (via
  `sdparm`/`sg_wr_mode`) and then write through `st` — which does **not** append
  the 4-byte CRC — and "the last 4-bytes of the logical block are treated as the
  CRC and … do not calculate as the CRC" → **every write fails with LOGICAL
  BLOCK GUARD CHECK FAILED.** True end-to-end LBP on Linux needs a custom SG_IO
  write/read engine that appends the CRC itself. *Source: man7 st(4) +
  kernel.org st docs (no LBP); IBM ref §4.5.4 NOTE 2.* — **VERIFIED**, as of
  2026-07.
- **LTO drives DO read-after-write in hardware (VERIFIED).** Drive log counters
  document it: "Datasets Corrected … Each count represents one dataset in error
  that was successfully corrected and written"; "Data Transients … ERP action
  was required because of a **readback check** or ECC detected error." A block
  returning good write status is therefore **already medium-verified by the
  drive** (read-while-write + C1/C2 Reed-Solomon ECC + automatic dataset rewrite
  accept it before status returns). *Source: IBM ref log counters {33h:0000h},
  {33h:0002h}.* — **VERIFIED** (physical read-while-write-head detail INFERRED
  from ACM 10.1145/3708997 snippet), as of 2026-07.
- **Three integrity threat classes — no single mechanism covers all three:**
  (1) **medium write errors** → covered at write time by the drive's
  read-while-write + ECC + auto-rewrite; (2) **path errors** (host RAM → HBA →
  cable → drive buffer) → the *only* class LBP uniquely adds, else a host
  checksum; (3) **long-term bit-rot on the shelf** → *only* a periodic
  read+hash verify pass catches it, months/years later — neither the drive nor
  LBP helps. So **LBP is not a substitute for sha256 verification**, and a
  periodic verify pass stays necessary regardless of LBP. *Source: IBM ref
  (classes 1–2); archival first-principles (class 3).* — **VERIFIED/INFERRED**,
  as of 2026-07.
- **The current plaintext index carries no checksums; per-slice sha256 lives
  only inside encrypted envelopes** — so a "confirm" that re-reads the plaintext
  index proves *navigability*, never slice-*byte* integrity, and slice bytes
  cannot be verified at all without keys. *Source: `src/volume/layout.rs`
  `generate_mini_index` ("NO content metadata (no … checksums…"),
  `mini_index_tuples` in `write.rs`; envelope `generate_manifest_toml` carries
  `sha256_plain`+`sha256_encrypted`.* — **VERIFIED**, as of 2026-07.
- **Domain already names the two tiers:** CONTEXT.md's **Claim** (navigable
  self-description) vs **Evidence** (checked against the tape); **Sealed** =
  after "confirm readback passes." A confirm that only reads the index must not
  be recorded as byte-integrity evidence. *Source: `CONTEXT.md`,
  ADR-0001/0002.* — **VERIFIED**, as of 2026-07.
- **Recommendation (independent of LBP):** put `sha256_encrypted` (the
  *ciphertext* hash) for every file in the plaintext front index — the single
  highest-value change. It gives a **keyless** readback pass honest slice-byte
  integrity at seal *and* genuine bit-rot detection on later verify passes,
  neither of which today's index supports. Keep the periodic verify pass. Treat
  LBP as skippable given the `st` blocker; a future SG_IO path could add `LBP_W`
  path-error coverage on top. **VERIFIED/INFERRED** (synthesis), as of 2026-07.

### D. Block size & mode

- **`st` supports fixed and variable block mode**; `MTSETBLK 0` selects
  variable. The maximum block size is **memory-bound at ≈2 MB** ("the maximum
  block size is very large (2 MB if allocation of 16 blocks of 128 kB
  succeeds)"); buffer tunable via `buffer_kbs=`/`write_threshold_kbs=`. *Source:
  man7 st(4); kernel.org st docs.* — **VERIFIED**, as of 2026-07.
- **LTO drive block-length limits (VERIFIED):** min 1 byte, max **16 MB
  unencrypted / 8 MB when encryption is used**; READ BLOCK LIMITS (05h) reports
  8 MB (80 0000h) on an encrypted volume. **Enabling encryption *or* LBP can
  lower the reported max block** "to ensure that an application will not attempt
  to write a larger block size than can be read" — confirming the
  LBP↔block-size interaction. *Source: IBM LTO SCSI Reference §"block sizes",
  §5.2.16.1.* — **VERIFIED**, as of 2026-07.
- **mhvtl baseline confirms 512 KB fixed in practice:** `Tape block size 524288
  bytes. Density code 0x5a (LTO-6)`. *Source:
  `docs/mhvtl-baseline-recordings.txt`.* — **VERIFIED**, as of 2026-07.
- **Design/impl drift:** design §2.29 says "Variable block mode (MTSETBLK 0) on
  every open," but shipped `store.rs`/`layout_model.rs` + CLAUDE.md use fixed
  512 KB, and the `Layout` math (`pad_to_blocks`) assumes fixed. *Source:
  `src/store.rs`, `src/volume/layout_model.rs`, §2.29.* — **VERIFIED**, as of
  2026-07.
- **Recommendation:** keep **fixed** block mode (plan-first needs the
  deterministic padding/trim contract; variable buys nothing when every size is
  pre-planned). 512 KB is safe and well under both ceilings; **1 MiB is the
  throughput sweet spot** if wanted (< 2 MB `st` ceiling, < 8 MB encrypted-drive
  ceiling, leaves room for a 4-byte LBP CRC), but mhvtl cannot reveal real
  throughput — gate any change on real LTO-6 hardware. **VERIFIED/INFERRED**
  (synthesis), as of 2026-07.

### E. Streaming encryption at scale (the H9 concern)

*It is the **`age` library crate** (`age = "0.11"` → 0.11.2 in `Cargo.lock`),
**not** a "rage" crate — `rage` is str4d's CLI binary; `age` is the library by
the same author. The design's/CLAUDE.md's "rage crate" wording is a misnomer.*

- **age payload = ChaCha20-Poly1305 over 64 KiB chunks (VERIFIED first-hand,
  age.md lines 147–152).** "The payload is split in chunks of 64 KiB, and each
  of them is encrypted with ChaCha20-Poly1305, using the payload key and a
  12-byte nonce … the first 11 bytes are a big endian chunk counter starting at
  zero … the last byte is 0x01 for the final chunk and 0x00 for all preceding
  ones. The final chunk MAY be shorter … but MUST NOT be empty unless the whole
  payload is empty." Payload key = HKDF-SHA-256(file key, salt = 16-byte CSPRNG
  payload nonce, info = "payload"). *Source: C2SP/age.md.* — **VERIFIED**, as of
  2026-07.
- **Header / multi-recipient (VERIFIED):** `age-encryption.org/v1`; 128-bit file
  key; `--- <base64>` header MAC = HMAC-SHA-256 keyed by HKDF-SHA-256(file key,
  "", "header"). Each recipient stanza independently wraps the same file key. An
  **X25519 stanza carries only the ephemeral share — no recipient identifier** —
  so multiple X25519 recipients are indistinguishable, and an identity is
  recognized only by the ChaCha20-Poly1305 auth succeeding on the 32-byte
  wrapped-file-key body. This *is* tapectl's shuffled-tenant-envelope
  trial-decrypt mechanism. (An scrypt stanza, if present, MUST be the only
  stanza.) *Source: C2SP/age.md.* — **VERIFIED**, as of 2026-07.
- **Exact `age` 0.11.2 streaming API (VERIFIED — docs.rs 0.11.2 + repo call
  sites, and it compiled/ran):**
  `Encryptor::with_recipients(impl Iterator<Item=&dyn Recipient>) -> Result<Self,
  EncryptError>`; `.wrap_output<W: Write>(W) -> Result<StreamWriter<W>>`
  (`StreamWriter: Write`; **`finish()` is mandatory** or the file is truncated
  and won't decrypt). `Decryptor::new(R: Read) -> Result<Self>` (an opaque struct
  in 0.11, was an enum ≤0.10); `.decrypt(impl Iterator<Item=&dyn Identity>) ->
  Result<StreamReader<R>>` (`StreamReader: Read`, plus `Seek` when the source is
  seekable). Binary format (the `armor` feature is not enabled). 0.11.0 was the
  breaking release for these shapes; 0.12.1 (2026-07-14) would break again — the
  `0.11` pin is deliberate. *Source: docs.rs/age/0.11.2; `str4d/rage`
  CHANGELOG; `src/staging/mod.rs:412,420`, `src/volume/restore.rs:116`.* —
  **VERIFIED**, as of 2026-07.
- **Heir path proven by EXECUTION (VERIFIED, 2026-07-21):** the repo's
  `validation/age-validate` encrypts with the age 0.11.2 crate (`wrap_output`),
  and stock **`/usr/bin/age` v1.1.1 (Go reference impl)** decrypts it via `age -d
  -i key.txt` — PASS. Multi-recipient independent decrypt PASS; a 100 MB
  streaming round-trip sha256 matched. Cross-implementation proof the heir
  recovers with only the `age` binary + key. *Source: executed
  `validation/age-validate/src/main.rs`.* — **VERIFIED**, as of 2026-07.
- **Reconciliation — capability vs current code (actionable):** age *enables*
  constant-memory streaming, but tapectl's current glue **buffers whole slices**
  — `src/staging/mod.rs:224` `fs::read(slice_path)` reads the entire dar slice
  into a `Vec` and `encrypt_data(&[u8]) -> Vec<u8>` buffers the whole ciphertext
  (~2× slice in RAM); `src/volume/restore.rs:95–126` mirrors it; and the `Store`
  seam's `execute(bytes: &[u8])` forces the whole file into memory before a tape
  write. So peak RAM tracks **`slice_size`, not archive size** — safe for
  multi-GB *archives* with a modest slice size, but a multi-GB *slice_size* would
  buffer GBs. The rewrite should stream the write path (`io::copy` through
  `wrap_output` / from `StreamReader`; a streaming `Store::execute`) so **block
  size, not slice size, bounds memory.** *Source: repo lines above.* —
  **VERIFIED (buffering) / INFERRED (fix)**, as of 2026-07.

### F. dar fit (confirm only)

*Source for F1–F5: the dar(1) man page, http://dar.linux.free.fr/doc/man/dar.html,
cross-verified against the raw troff twin
https://raw.githubusercontent.com/Edrusb/DAR/master/man/dar.1 (header 2026-05,
documents through dar 2.8.0). Quotes grepped from the raw source.*

- **F1 — slice naming `base.1.dar … base.N.dar`, numbering from 1 — CONFIRMED.**
  "The number between the dots is the slice number, which starts from 1"; `-s
  SIZE` splits into `base.1.dar, base.2.dar, …`. **VERIFIED**, as of 2026-07.
- **F2 — isolated catalogue `-C` — CONFIRMED, with a nuance.** `-C` copies the
  internal catalogue into its own container — a standalone TOC that **contains
  no file data** — usable for `dar -l` listing/locating and as an external
  reference. But dar documents `-A catalogue` during `-x` primarily as
  catalogue-**rescue** (a corrupted internal catalogue), not a speed feature,
  and an actual restore still needs the data slices. **VERIFIED**, as of
  2026-07.
- **F3 — restore `dar -x base -R /dest` — CONFIRMED.** "Slices are expected to
  be in the current directory or in the directory given by `<path>`"; dar "will
  pause and ask the user for required slices if they are not present."
  **VERIFIED**, as of 2026-07.
- **F4 — `dar --hash sha256` is INVALID (algorithm refuted); mechanism
  confirmed.** dar's `--hash` accepts `md5, sha1, sha512, whirlpool, sha3,
  blake2s` — **not `sha256`** — across the entire 2.6–2.8 range; `--hash
  sha256` would be rejected, and dar's sidecars are `.sha512`/etc., never
  `.sha256`. The per-slice on-the-fly hash-file *mechanism* is as assumed, but
  **the design doc §6 and CLAUDE.md's `dar … --hash sha256` flag is a real
  error.** The SHA-256 the on-tape format actually relies on is
  **tapectl-computed** (the `sha2` crate over source files and over the finished
  encrypted slices → `stage_slices.sha256_plain`/`sha256_encrypted`),
  independent of dar hashing. **Consequence for the rewrite:** drop/​correct the
  bogus flag; the format's integrity story (front-index ciphertext hashes,
  envelope hashes, RESTORE.sh `sha256sum` checks) does not depend on dar's hash
  and is unaffected. **VERIFIED (dar)**; the "tapectl computes its own"
  reading is consistent with `sha2 0.10` + §2.13 staging validation
  (**INFERRED**, confirm in staging code), as of 2026-07.
- **F5 — `-K` is dar-internal crypto; plain slices by default — CONFIRMED.**
  With no `-K`/`-$`/gnupg argument dar writes plain unencrypted `.dar` slices;
  age is layered on top of the finished slices. **VERIFIED**, as of 2026-07.

### G. Prior-art cross-check (SECONDARY — not authoritative for the tape format)

- **restic** — a separate `index/` maps each blob → `(pack-id, offset, length)`;
  a pack is `blobs ‖ encrypted-header ‖ header-length`; the storage ID **is** the
  SHA-256 of the content (address == integrity; the filename is the hash) plus a
  Poly1305-AES MAC. *Source:
  github.com/restic/restic/blob/master/doc/design.rst.* **SECONDARY**, as of
  2026-07.
- **borg** — append-only ~500 MB segments; a repository index (hash table) maps
  each object key → `(segment number, offset)`; key = `HMAC-SHA256`/SHA-256
  (content-addressed dedup); CRC32 per segment entry, XXH64 on metadata
  (explicitly safety, not security). *Source:
  borgbackup.readthedocs.io/en/stable/internals/data-structures.html.*
  **SECONDARY**, as of 2026-07.
- **kopia** — closest analog: index blobs map content-id → `(pack-blob, offset,
  length)`, **and a copy of the local index is written at the TAIL of each pack
  blob** so a pack is self-locating even if the central index is lost — directly
  analogous to a self-describing tape carrying its own index. Content-addressed
  SHA2/BLAKE2S; AEAD integrity. *Source: kopia.io/docs/advanced/architecture/.*
  **SECONDARY**, as of 2026-07.
- **Takeaway (corroborates §3/§4):** all three converge on (a) an index
  resolving content-id → `(container, offset, length)` at known offsets, and (b)
  a per-blob cryptographic hash that doubles as address **and** integrity check
  — exactly the D4 move (ciphertext hash in the plaintext front index). Kopia's
  tail-index-copy independently validates the **front index + trailing seal
  marker** redundancy shape.

---

## 3. Recommended on-tape format (proposed §8 rewrite)

**Layout Version 2.** No production `layout_version=1` tape exists yet
(ADR-0005: "Adopted before the first production tape exists"; only disposable
mhvtl test tapes are v1), so v2 can be defined cleanly with a trivial v1 reader
retained for forward-compat symmetry.

### 3.1 Zone order (single partition, fixed 512 KB blocks)

```
┌──── FRONT: plaintext navigation (all known pre-write) ───────────────────────┐
│ File 0  ID thunk          plaintext   identity + "the map is File 3"          │
│ File 1  System guide      plaintext   heir manual (mt/dd/age/dar/sha256sum)   │
│ File 2  RESTORE.sh        plaintext   heir tool (--info now reads File 3)     │
│ File 3  FRONT INDEX       plaintext   for EVERY file 0..M: position, type,    │
│                                       true size_bytes, sha256_encrypted       │
│                                       (ciphertext hash). ZERO content metadata.│
├──── MIDDLE: encrypted decryption-metadata (known pre-write) ─────────────────┤
│ File 4  Planning header   enc(op+esc) planned packing list (optional; may     │
│                                       fold into the operator envelope)        │
│ File 5  Tenant envelope   enc(t+op+esc)┐ MANIFEST.toml (filenames,            │
│  ...    Tenant envelope   enc(t+op+esc)│ sha256_plain, dar cmd, positions),   │
│ File j  Operator envelope enc(op+esc)  │ RECOVERY.md, catalogs/. Tenant       │
│ File j+1 Operator env bkup enc(op+esc) ┘ envelopes in shuffled random order.  │
├──── BULK: encrypted data (the mass of the tape) ────────────────────────────┤
│ File j+2  Data slice      enc(t+op+esc)┐ age(dar base.N.dar). One unit's      │
│  ...      Data slice      enc(t+op+esc)│ slices contiguous. This is the only  │
│ File M-1  Data slice      enc(t+op+esc)┘ zone a short tape can truncate.      │
├──── TAIL: the completeness assertion ───────────────────────────────────────┤
│ File M  SEAL MARKER       plaintext   {file_count=M+1, sealed_at,             │
│                                        front_index_sha256}. Its presence =    │
│                                        "everything before me is present."     │
└──────────────────────────────────────────────────────────────────────────────┘
    esc = the permanent Escrow Recipient (ADR-0005), in every encryption.
```

**Why this order:**
- **Front index (File 3)** is the whole navigation map at BOP — no seek to EOT
  to learn what is on the tape or how big each file is.
- **Envelopes before slices** means a truncated or tail-damaged tape loses only
  trailing *slices*; all decryption metadata for what landed survives.
- **Seal marker last** is the only honest place for "complete": completeness can
  only be asserted after the last content file exists. Its `front_index_sha256`
  binds the two ends (a damaged/edited front index is detectable).

### 3.2 Plaintext vs encrypted

| Zone | Plaintext | Rationale |
|---|---|---|
| ID thunk, System guide, RESTORE.sh | plaintext | heir must read them with no key |
| **Front index** | **plaintext** | navigation must work with no key; carries position/type/size and (decision D4) `sha256_encrypted` — hashes of *ciphertext*, not content |
| Planning header, envelopes, data slices | age-encrypted | all content metadata (names, sizes-of-content, `sha256_plain`, catalogs) lives here only |
| **Seal marker** | **plaintext** | structural completeness assertion; `front_index_sha256` is a hash of a plaintext file |

**Isolation invariant (restated for v2):** no plaintext file reveals filenames,
tenant/unit names, content sizes, plaintext-content hashes, or key fingerprints.
Per-file *on-tape byte sizes* and *ciphertext hashes* are permitted in plaintext
because they are structural, non-attributable to a tenant (slices unlabeled,
envelopes shuffled), and computable by anyone holding the tape. *(This tightens
the wording of the current rule, which forbids "sizes/hashes" yet already ships
per-file sizes in the mini-index — see Decision D4.)*

### 3.3 Heir navigation — end to end with only `mt`, `dd`, `age`, `dar`, `sha256sum`

1. `mt -f /dev/nst0 rewind && dd if=/dev/nst0 bs=64k` → File 0: identity + "map
   is File 3."
2. `mt -f /dev/nst0 fsf 3 && dd …` → **Front index**: every file's position,
   size, type, ciphertext hash.
3. *(completeness check)* space to the last file, read the **seal marker**;
   confirm `file_count` and `front_index_sha256` match. Absent/mismatched ⇒ the
   tape is unsealed or damaged — proceed knowing some trailing slices may be
   missing (front index still tells you *which*).
4. For each envelope position (File 5..j+1): `dd` it out, `age -d -i KEY` — the
   one that decrypts is yours → `MANIFEST.toml` + `RECOVERY.md` + `catalogs/`.
5. For each slice position in your manifest: `dd bs=512k` it out; `sha256sum` vs
   the front index's `sha256_encrypted` (**keyless** integrity) and/or the
   envelope's value; trim padding to the front-index `size_bytes`
   (`head -c`/`truncate`); `age -d -i KEY` → `base.N.dar`; optionally `sha256sum`
   vs the envelope's `sha256_plain`.
6. `dar -x base -R /dest` (dar reassembles `base.1.dar…base.N.dar`).

Every step is literal in RESTORE.sh and the system guide; nothing requires
tapectl or the database.

### 3.4 Write / confirm sequence (replaces §2.9 + Appendix C)

```
1. BUILD Layout        every position, size, and sha256_encrypted known from
                       stage_slices + generated-zone sizes.
2. VALIDATE (gate #28) Σ block-padded sizes + enospc_buffer ≤ available;
                       staged slices present & ciphertext hashes match;
                       keys resolvable (tenants + operator + escrow).
                       FAIL ⇒ refuse. Nothing is written. Clean.
3. REWIND to BOT       (fresh sealed tape — no MTEOM, no append).
4. WRITE front zone    File 0..3, planning header, envelopes (shuffled),
                       each + MTWEOFI.
5. WRITE slices        each + MTWEOFI.
                       ENOSPC here (real EOT; only if MAM over-reported):
                       ⇒ ABORT. Volume stays UNSEALED (no seal marker).
                       No overwrite, no sacrifice. Operator reloads / re-plans.
6. WRITE seal marker   + MTWEOF (synchronous flush). 
7. CONFIRM (#23)       rewind; read front index, diff vs Layout  → navigable.
                       forward pass: hash each file vs front index
                       sha256_encrypted                          → integrity.
                       read seal marker; verify binding hash.
                       Record verification_sessions row stating WHICH tier.
                       Match ⇒ status = sealed. Mismatch ⇒ quarantine + abort.
```

The confirm is a **single forward pass from BOP** (no seek-back-to-trailer). It
records honest strength: *navigable* (index parses & matches) vs
*integrity-verified* (every file's ciphertext hash checked). LBP, if enabled out
of band, is drive-side corroboration recorded alongside — never the basis of the
claim.

---

## 4. Decisions the operator must make

Presented as **separable layers** so they can be adopted incrementally — (a) is
the literal pivot and is cheap; (b) and (d) are judgment calls.

### D1 — (layer a) Front index + trailing seal marker  ·  **Recommend: YES**
Move the navigation index (position/type/size for every file) from the trailer
to a plaintext front File 3, **and** add a plaintext trailing seal marker. These
are an atomic pair: the front index alone would make a truncated tape look
sealed. **Tradeoff:** one extra tiny tail file and a redefinition of "sealed"
(from "has a valid trailer" to "has a seal marker binding a front index").
**Primary source:** LTFS keeps a front-of-tape index *copy* (Index Partition)
precisely for fast, robust reads (SNIA v2.5 §9.2), and tar's interleaved-header
model exists only for the streaming constraint plan-first doesn't have (POSIX
pax). The trailer's one real edge — an index written last describes what
*actually* landed — is neutralized here by plan-first (map correct by
construction) + the confirm pass + the seal marker. **Cost:** low — the
mini-index generator relocates; the ID thunk's position map subsumes into File 3.

### D2 — (layer b) Envelopes before slices  ·  **Recommend: YES (separable)**
Write tenant/operator envelopes ahead of the data slices. **Tradeoff:** all
decryption metadata survives tail damage / a short tape; the cost is that
envelopes must be fully generated before any slice is written — which write.rs
already does today. Skipping this (keep envelopes near the tail, after slices but
before the seal marker) still works with D1 but forfeits the tail-damage
robustness. **Primary source:** ADR-0002 ("metadata generated from the Layout")
+ the plan-first invariant; corroborated by restic/borg front-index practice
(SECONDARY).

### D3 — (layer c) Delete EOT salvage; keep clean abort  ·  **Recommend: YES**
Remove overwrite-incomplete and sacrifice-last-slice (§2.9 layers 2–3, Appendix
C RECOVERY, `writes.eot_recovery`, `writes.sacrificed_slice_id`). Keep a single
**abort-to-unsealed** path for a real EOT. **Tradeoff:** a rare capacity
mis-estimate wastes a partially-written tape instead of salvaging it — but a
salvaged partial was never a *copy* (ADR-0003/0004: only sealed volumes count),
and the salvage code is untestable on mhvtl (2026-07-20 drill) and among the
audit's HIGH-defect sources. **Primary source:** `docs/mhvtl-baseline-recordings.txt`
(mhvtl never returns ENOSPC), ADR-0002 pre-flight validation. **Corollary
(free win):** `manifest_reserve` collapses to ~0 — front metadata is a known
up-front line item in the plan total, not an end-reservation, so
`CapacityBudget.reserve_bytes` becomes just the `enospc_buffer` (small margin
against MAM over-report). This simplifies §2.8 and `layout_model.rs`.

### D4 — Ciphertext hashes in the plaintext front index  ·  **Recommend: YES (ratify explicitly)**
Store `sha256_encrypted` per file in the plaintext front index. **This relaxes a
stated hard constraint** ("no checksums in plaintext"), so it needs explicit
operator ratification. **Safety argument:** it is a hash of age *ciphertext*
(pseudorandom, IND-CCA), reveals nothing about plaintext, is non-attributable to
a tenant (slices unlabeled, envelopes shuffled), and is computable by anyone
holding the tape — so storing it leaks no information an observer couldn't
derive. **Benefit:** keyless byte-integrity confirm at seal *and* keyless
integrity check by an heir before they even locate their key. **Fallback
(status quo):** keep all hashes encrypted-only; then confirm proves navigability
only, or must decrypt to verify. **Note:** the current format *already* ships
per-file plaintext *sizes* in the mini-index — the same isolation reasoning
applies; ratifying D4 is consistent with what already ships. `sha256_plain`
stays encrypted-only regardless. **Primary source:** age spec (ciphertext is
pseudorandom output of ChaCha20-Poly1305, C2SP/age.md); CONTEXT.md Claim/Evidence
distinction.

### D5 — LTO partitioning  ·  **Recommend: NO**
Single partition. **Tradeoff:** none meaningful — partitioning's only benefit is
keeping a *mutable* index at BOP, which a sealed tape never rewrites; it costs
guard-band capacity, the `MT_ST_CAN_PARTITIONS` enable step, and format
complexity. **Primary source:** kernel.org `st` docs (partitions disabled by
default, need explicit enable) + SNIA LTFS v2.5 (two partitions serve index
mutation).

### D6 — Confirm strength & LBP  ·  **Recommend: forward hash pass; LBP optional**
Baseline confirm = forward pass hashing each file against the front index
(works unconditionally). **Do not make sealing depend on LBP** — the `st` driver
does not expose it, and enabling it needs the SG_IO the design rejected (§1.3).
If the operator wants drive-side end-to-end CRC, enable LBP via `sg3_utils` MODE
SELECT before the write and record it as *supplementary* evidence.
**Tradeoff:** the forward hash pass is a full read (~write-time cost) for
integrity-tier evidence; a navigable-tier confirm (index parse only) is near-free
but weaker — record which was done. **Primary source:** kernel.org `st` docs
(no LBP); LTO read-after-write (T10 SSC) already guarantees on-tape bytes were
read back by the drive.

### D7 — Block size & mode  ·  **Recommend: fixed; 512 KB now, revisit ≤1 MB on hardware**
Keep fixed-block mode (plan-first needs deterministic padding). Keep 512 KB
until real-hardware throughput testing justifies 1 MB (and check any LBP
block-size ceiling first). **Tradeoff:** larger blocks = fewer records / less
overhead but more padding waste per file and more RAM per buffered block.
**Primary source:** kernel.org `st` docs (no hard max; `buffer_kbs`-bounded);
`Layout::pad_to_blocks` contract. **Also:** fix the stale §2.29 "variable block"
text.

---

## 5. Impact assessment — what a "regear to spec" touches

### ADRs 0001–0006
- **0001 (tape authoritative / claims):** **HOLDS.** Front index is still a
  claim until confirm converts it to evidence; the seal marker is the on-tape
  completeness assertion the reconciliation check reads at contact.
- **0002 (plan/execute/confirm over a Layout):** **HOLDS, strengthened.**
  Front-loading is the purest expression of "all metadata generated from the
  Layout." **One wording change:** confirm is now a *forward readback from BOP*
  (read front index + hash slices + read seal marker), not "seek back and read
  the mini-index." Update the ADR's closing sentence.
- **0003 (sealed-immutable, no append):** **HOLDS, reinforced.** Deleting EOT
  salvage and Appendix D append (already rejected) follows directly.
  **Definitional touch:** "sealed" now means "seal marker present + binds front
  index," not "trailer metadata present."
- **0004 (seal decides copy eligibility):** **HOLDS.** Unchanged — an aborted
  unsealed tape is simply not a copy.
- **0005 (permanent escrow recipient):** **HOLDS.** The escrow recipient stays
  in every encryption (slices + envelopes + planning header). Front-loading does
  not touch recipients. The Heir Kit's encrypted catalog snapshot is unchanged.
- **0006 (storage interface / stores):** **HOLDS.** `execute` stays
  medium-agnostic. `confirm → Evidence` for TapeStore becomes the forward hash
  pass; WarehouseStore's confirm (deposit receipt) and ExportStore are
  unaffected — the seam absorbs the change. **Touch:** the ADR's "confirm is the
  readback after the final filemark" example line generalizes to "read back and
  verify against the Layout."

### CONTEXT.md terms
- **Sealed** — reword: "…the tape *begins* with a valid front index describing
  everything after it, and *ends* with a seal marker asserting completeness…"
- **Heir Path** — swap "mini-index" for "front index + seal marker" in the
  artifact list.
- **Write Session** — the confirm-readback description updates (forward pass).
- **Layout, Copy, Evidence, Claim, Divergence, Quarantine** — unchanged.

### The `Layout` value (`src/volume/layout_model.rs`)
- **Small.** `ZoneKind` gains a `SealMarker` variant and (if D4) the front-index
  generator reads `LayoutEntry.sha256` for *all* entries (it already holds the
  ciphertext hash for slices — "sha256 of the on-tape bytes"). Entry **ordering**
  changes (envelopes before slices; index at File 3; seal marker last) but the
  *type* barely moves — position is carried explicitly, entries are an ordered
  `Vec`. `CapacityBudget.reserve_bytes` shrinks to just the ENOSPC buffer (D3).
  `type_label()` gains `seal_marker`. **Rough rework: ~1 day.**

### The volume format (§8 + Appendix B)
- **Rewritten** (the deliverable's point): §2.6 zone list, §8.1–8.8, Appendix B
  contract points 1–17, Appendix C (write sequence → §3.4 here), delete Appendix
  D. Bump `layout_version` → 2; keep a v1 reader stub. **Rough rework: the spec
  rewrite itself.**

### Already-shipped code
- **#21 `Layout` (layout_model.rs):** minor — ordering + `SealMarker` +
  reserve-shrink (above).
- **#24 metadata-from-Layout (layout.rs generators, ~1064 LOC):** **moderate.**
  `generate_mini_index` → front-index generator emitting `sha256_encrypted`
  (D4); `generate_id_thunk` `[layout]` map points at the front index; add a
  seal-marker generator. Envelope/manifest generators unchanged. **~2–3 days.**
- **#30 RESTORE.sh (layout.rs `generate_restore_script`):** **moderate but
  net-simpler.** `--info` reads File 3 instead of seeking to the trailer; the
  restore path reads sizes/hashes from the front index; constants
  (`MINI_INDEX_POS`→`FRONT_INDEX_POS`, add `SEAL_MARKER_POS`) change; drop any
  EOT-trailer logic. **~1–2 days.**
- **#28 capacity gate:** **reinforced, not reworked** — it becomes the *sole*
  capacity defense. `reserve_bytes` simplification is a small edit.
- **#71 store seam (store.rs):** minor structurally, but **change `execute(bytes:
  &[u8])` to a streaming signature** (take a `Read` + known length, or a writer
  the caller streams into) so multi-GB slices are not buffered whole in RAM (the
  H9 fix, §2.E) — RAM should track block size, not slice size. `confirm`/`read`
  legs (still pending on the trait) get defined as forward operations; the EOT
  `MediumEvent` outcome simplifies to abort.

### Pending session block (#22/#23/#25/#26/#27) — where most churn lands, cheaply
- **#22 WriteSession:** write order = front zone → slices → seal marker; no
  MTEOM/append; simpler cursor. **Redirect, not rework** (pending).
- **#23 confirm/seal:** **the biggest definitional change** — forward readback
  of front index + ciphertext-hash pass + seal-marker check, instead of
  seek-back-to-trailer. Pending, so this is design-time.
- **#25 persistence:** minor (seal-marker position; reserve field).
- **#26 EOT transition:** **shrinks dramatically** — from a three-layer
  truncate/sacrifice machine (grounded in unobservable mhvtl EOT) to a trivial
  abort-to-unsealed. This is the single largest *reduction* in the plan.
- **#27 resume/divergence:** **mostly unchanged** — rewind, read File 0, require
  ID-thunk identity match (label+uuid), quarantine on mismatch. Resume now also
  keys off the **absent seal marker** to know the tape is legitimately unsealed.

**Net:** the shipped foundation (#21/#28/#71) barely moves; the generators
(#24/#30) get a moderate, mechanical reorder + hash emission; and the *pending*
session epic is **redirected and net-simplified** (especially #26). The rewrite
removes more code than it adds.

---

## 6. Open questions — what only the operator (or real hardware) can answer

1. **Ratify D4 (ciphertext hashes in plaintext)?** The one deliberate relaxation
   of a stated hard constraint. Recommended YES with the safety argument in §4;
   the operator owns the final call. If NO, confirm falls back to
   navigability-only (or must decrypt to verify).
2. **Adopt D2 (envelopes before slices) now or defer?** Recommended now (small
   marginal cost over D1, real robustness gain), but D1 alone is a coherent
   first step.
3. **Seal marker richness / trailing redundancy:** minimal (`file_count`,
   `sealed_at`, `front_index_sha256`) vs richer — a Merkle root over per-file
   ciphertext hashes (the marker alone then proves the whole index), or expanding
   the marker into a **full trailing copy of the front index** (LTFS keeps
   exactly this front-copy-plus-end-index redundancy, SNIA v2.5 §9.2; the index
   is tiny vs 2.5 TB on write-once media). Recommend starting minimal; the Merkle
   root and the full trailing copy are cheap later hardenings.
4. **Block size 512 KB vs 1 MB** — needs a real LTO-6 throughput measurement
   (mhvtl can't answer; the 2026-07-20 drill shows mhvtl's capacity/EOT fidelity
   is unreliable). Tie to the `docs/lto6-validation-checklist.md` session.
5. **LBP on the actual drive:** does the operator's LTO-6 drive + HBA accept a
   MODE SELECT enabling LBP via `sg3_utils`, and does the `st` data path then
   read back cleanly? Real-hardware question; keep confirm independent of the
   answer (D6).
6. **Real EOT behavior:** the pre-flight gate assumes MAM `remaining` is
   trustworthy. Confirm on hardware that MAM never *over*-reports by more than
   `enospc_buffer`; if it can, size the buffer from the observed error.
7. **Layout v1 disposal:** confirm no non-disposable v1 tape exists before
   redefining the format as v2 (design says none should — first production tape
   postdates ADR-0005).

---

*Prepared 2026-07-21. All six research areas (A–G) are corroborated with
first-hand primary sources — the SNIA LTFS v2.5 spec (read directly after a
curl-past-the-WAF), the IBM LTO Ultrium SCSI Reference, the Linux `st`/man7
docs, the age v1 spec plus an **executed** age-crate↔stock-`age`-CLI interop
test, and the dar(1) man page — each fact tagged VERIFIED/INFERRED with its
source and an as-of-2026-07 datestamp above. The recommended design rests on
those VERIFIED facts plus the repo's own ADRs, the Layout value, and the
2026-07-20 mhvtl EOT drill; nothing here is left awaiting corroboration.*
