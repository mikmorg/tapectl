//! The storage interface (ADR-0006). A `Store` executes a Layout's entries at
//! contact and reports back — write via `execute`, read via `read_file`, a
//! pre-flight `capacity` oracle, and `confirm`'s keyless integrity chain walk
//! (`docs/design/volume-format-v2.md` §5). `TapeStore` is the first
//! implementation (LTO via the kernel st driver); `WarehouseStore`/
//! `ExportStore` are peers landing later (#72/#73).
//!
//! The trait is deliberately medium-agnostic — the anti-tape-ism test is that
//! a warehouse upload must fit `execute` without violence, and a deposit
//! receipt must fit `confirm` the same way. `MemStore` is the in-memory peer
//! that exercises the *exact same* `confirm` algorithm as `TapeStore`: the
//! chain walk is factored into one shared function, [`chain_walk`], so it is
//! the real algorithm — not a description of it — that the unit tests below
//! (and later, the T7 synthetic-heir harness) exercise with no tape anywhere.

use std::collections::HashMap;
use std::io::{self, Read, Write};

use sha2::{Digest, Sha256};

use crate::error::{Result, TapectlError};
use crate::pipeline::{self, BufferPool};
use crate::tape::ioctl::{ReadEnd, TapeDevice, TapePosition};
use crate::util::{HashingWriter, TruncatingWriter};
use crate::volume::format;
use crate::volume::layout_model::{pad_to_blocks, Layout, ZoneKind};

/// How thoroughly `confirm` checked the tape
/// (`docs/design/volume-format-v2.md` §5). `Navigable` diffs the front index
/// against the Layout only; `Integrity` additionally hashes every content
/// file's on-tape bytes against the front index's `sha256_encrypted`.
/// Integrity is the seal default (ratified 2026-07-22, §1.2); `--quick` opts
/// down to Navigable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tier {
    Navigable,
    Integrity,
}

impl Tier {
    /// The tier in words, for text an operator reads (issue #357: the
    /// quarantine message printed `Integrity`, Rust's `{:?}`). "full" and
    /// "quick" are the names `volume verify` and `verification_sessions`
    /// already give the two tiers.
    pub fn describe(self) -> &'static str {
        match self {
            Tier::Integrity => "full read-back",
            Tier::Navigable => "quick navigation check",
        }
    }
}

impl Default for Tier {
    /// Integrity is the ratified seal-time default (`--quick` opts down to
    /// Navigable) — `docs/design/v2-open-questions.md` §1.2: at seal time
    /// the staged slices still exist on disk, so a failed confirm costs a
    /// fresh cartridge and hours, not an unrecoverable loss; skipping the
    /// full readback would mean no end-to-end host-to-medium check ever ran
    /// on the sealed artifact.
    fn default() -> Self {
        Tier::Integrity
    }
}

/// What kind of disagreement a [`Mismatch`] reports. Each variant maps to a
/// distinct step of the `volume-format-v2.md` §5 chain walk.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MismatchKind {
    /// The seal marker (last file) is absent, unreadable, or fails to parse.
    /// Per the fail-safe reader precedence (`v2-open-questions.md` §2.5)
    /// this is the *normal* signal for an unsealed tape, never an error.
    SealUnreadable,
    /// The front index (File 3) is unreadable or fails to parse.
    FrontIndexUnreadable,
    /// File 3's own §2.5 self-consistency check found a violation.
    FrontIndexInconsistent,
    /// sha256(File 3's true bytes) != the seal marker's `front_index_sha256`
    /// — the tape's two ends disagree (quarantine-grade).
    FrontIndexDivergesFromSeal,
    /// A front-index entry's `{position, type, size_bytes}` disagrees with
    /// the Layout, or a Layout entry is missing from the front index.
    NavigationDisagreement,
    /// (Integrity tier) a content file's on-tape bytes, truncated to the
    /// front index's claimed size, hash to something other than the front
    /// index's `sha256_encrypted` for that position.
    ///
    /// **The bytes came back and they were wrong.** Only that — a read that
    /// never delivered the bytes at all is [`Self::ContentUnreadable`]
    /// (issue #239).
    ContentHashMismatch,
    /// (Integrity tier) a content file could not be read back in full: the
    /// read itself failed, or fewer bytes came back than the front index
    /// claims. **No hash comparison happened**, so this says nothing about
    /// what the bytes ARE.
    ///
    /// Split out of [`Self::ContentHashMismatch`] by issue #239, which is
    /// also why the two sit next to each other: one `MismatchKind` covered
    /// both "the tape's bytes are wrong" and "this drive could not read
    /// them", and the single kind was classified medium-proving. A drive
    /// that clears File 0, the seal marker and the front index and then
    /// faults partway through a slice — a dirty head, a marginal cable —
    /// therefore quarantined a sound cartridge, which is the exact outcome
    /// ADR-0012's amendment exists to prevent.
    ContentUnreadable,
}

impl MismatchKind {
    /// A stable snake_case name for this kind.
    ///
    /// Written to `verification_results.notes` and to `volume verify
    /// --json` (issue #142), so it is part of what an operator's scripts
    /// read back — `{:?}` would be, too, but silently, and would change the
    /// day someone renames a variant. Spelling it out makes that rename a
    /// visible decision.
    pub fn label(self) -> &'static str {
        match self {
            MismatchKind::SealUnreadable => "seal_unreadable",
            MismatchKind::FrontIndexUnreadable => "front_index_unreadable",
            MismatchKind::FrontIndexInconsistent => "front_index_inconsistent",
            MismatchKind::FrontIndexDivergesFromSeal => "front_index_diverges_from_seal",
            MismatchKind::NavigationDisagreement => "navigation_disagreement",
            MismatchKind::ContentHashMismatch => "content_hash_mismatch",
            MismatchKind::ContentUnreadable => "content_unreadable",
        }
    }

    /// Whether this kind's `expected`/`actual` are genuinely sha256 hex.
    ///
    /// Only these two compare hashes. For every other kind the strings are
    /// sizes, counts or prose, and writing them into
    /// `verification_results.expected_sha256` / `.actual_sha256` would make
    /// those columns lie about their own type — the exact defect issue #142
    /// exists to end, reintroduced one level down.
    ///
    /// [`Self::ContentUnreadable`] is deliberately NOT here, and that is a
    /// correction rather than a new rule (issue #239): its `expected`/
    /// `actual` are `"file readable"` / `"read failed: …"` and
    /// `"{n} on-tape bytes"` / `"only {n} bytes read back"`. Those strings
    /// were being written into the two sha256 columns for as long as the
    /// read-failure producers shared [`Self::ContentHashMismatch`] — the
    /// #142 lie, on both the verify and the confirm path.
    pub fn compares_hashes(self) -> bool {
        matches!(
            self,
            MismatchKind::ContentHashMismatch | MismatchKind::FrontIndexDivergesFromSeal
        )
    }

    /// Whether this kind is evidence that **the medium is bad**, as opposed
    /// to evidence that *this drive could not read it today*
    /// (ADR-0012, amendment of 2026-09-17, issue #234).
    ///
    /// This is the predicate a failed `volume verify` quarantines on, and
    /// the amendment is explicit that the blanket "a failed verify
    /// quarantines the volume" was too strong: `quarantined` is precisely
    /// what makes a volume stop counting as a copy, so a false quarantine
    /// silently takes real coverage to zero, and a dirty drive could condemn
    /// a library one cartridge at a time. `layout-session.md` states the
    /// hazard in terms — quarantining a good tape is "silent corruption, not
    /// a loud failure".
    ///
    /// The ruling: quarantine only on "a checksum mismatch, or an unreadable
    /// block at a position the layout says carries data". Arm by arm:
    ///
    /// - `ContentHashMismatch` — **yes**. The on-tape bytes hash to
    ///   something other than the front index promises: a checksum
    ///   mismatch, the first half of the ruling verbatim. The bytes came
    ///   back and they were wrong, which no drive fault produces — a drive
    ///   that cannot read returns nothing, not a different sha256.
    /// - `ContentUnreadable` — **no** (issue #239), and this arm overrides
    ///   ADR-0012's own second clause, so read this before changing it.
    ///   The amendment's ruling reads "a checksum mismatch, **or an
    ///   unreadable block at a position the layout says carries data**",
    ///   and until #239 this was one kind with `ContentHashMismatch` on the
    ///   strength of that clause. But the ruling's very next sentence says
    ///   "drive and transport errors are reported and do **not**
    ///   quarantine", and the two clauses contradict each other for exactly
    ///   this event: `chain_walk` raises this kind from a raw `Err` out of
    ///   `read_file` and from a short read, neither of which distinguishes
    ///   a bad tape from a dirty head, a marginal cable, a wrong block size
    ///   or a transient SCSI error. Issue #239 rules the second clause
    ///   controls, for the reason the amendment itself gives: `quarantined`
    ///   is what makes a volume stop counting as a copy, so the cost of
    ///   being wrong is asymmetric — a missed quarantine is found by the
    ///   next verify, a false one silently takes real coverage to zero and
    ///   nothing un-quarantines a volume. The short-read half rides along
    ///   on `FrontIndexUnreadable`'s reasoning below: same event, different
    ///   position, same epistemics. **Ratified 2026-09-17 (issue #239):**
    ///   `docs/adr/0012-...md` carries the correction, so this arm is the
    ///   ADR's own position and not an unratified override — the sentence
    ///   that used to stand here said the opposite and was read by the
    ///   2026-09-18 review as an instruction to reinstate the defect
    ///   (issue #266). Do not "fix" this arm back; the 2026-09-18
    ///   amendment then extended the same rule to `confirm`.
    /// - `FrontIndexDivergesFromSeal` — **yes**. The tape's two ends
    ///   disagree about bytes both of them recorded; its own doc has said
    ///   "(quarantine-grade)" since the chain walk was written.
    /// - `FrontIndexInconsistent` — **yes**. File 3's §2.5 self-consistency
    ///   check failed: the tape's own map contradicts itself, which no
    ///   drive fault produces.
    /// - `NavigationDisagreement` — **yes**. The front index and the Layout
    ///   disagree about a file's position, type or size. Nothing about a
    ///   transport error changes what a successfully read index SAYS.
    /// - `FrontIndexUnreadable` — **no**, and this is the crux. A genuine
    ///   I/O or transport error becomes this variant, and so does a short
    ///   read; neither distinguishes a bad tape from a dirty drive, a wrong
    ///   block size or a transient SCSI error. "We could not read it today"
    ///   is not "the bytes are gone".
    /// - `SealUnreadable` — **no**, and never. Its own doc says an absent or
    ///   unparseable seal marker is the *normal* signal for an unsealed
    ///   tape, "never an error".
    ///
    /// Written as an exhaustive `match` with **no wildcard arm** on purpose:
    /// an eighth variant must fail to compile until someone decides which
    /// side of this line it falls on. The decision is a CTO one (ADR-0012),
    /// not a default. That is what surfaced issue #239 — splitting
    /// `ContentUnreadable` out could not be done silently.
    pub fn proves_medium_bad(self) -> bool {
        match self {
            MismatchKind::ContentHashMismatch => true,
            MismatchKind::FrontIndexDivergesFromSeal => true,
            MismatchKind::FrontIndexInconsistent => true,
            MismatchKind::NavigationDisagreement => true,
            MismatchKind::ContentUnreadable => false,
            MismatchKind::FrontIndexUnreadable => false,
            MismatchKind::SealUnreadable => false,
        }
    }
}

/// One disagreement `confirm` found, at a specific tape position. Kept
/// minimal and `Debug`-printable — a report structure, not a control type.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Mismatch {
    /// The tape file position the disagreement concerns.
    pub position: u32,
    pub kind: MismatchKind,
    /// What was expected (a hash, a size, a count, or a plain description).
    pub expected: String,
    /// What was actually found.
    pub actual: String,
}

impl Mismatch {
    /// One mismatch as a line of text, in the shape `volume verify` prints
    /// its own: `position N: <kind> — expected X, found Y`. The kind is
    /// [`MismatchKind::label`], the stable name `--json` also carries, so an
    /// operator can match the two (issue #357: this used to be `{:?}`).
    pub fn describe(&self) -> String {
        format!(
            "position {}: {} — expected {}, found {}",
            self.position,
            self.kind.label(),
            self.expected,
            self.actual
        )
    }
}

/// What `confirm` found.
///
/// `tier` is the tier that was *requested*, not necessarily achieved — it is
/// what `verification_sessions.verify_type` records (Integrity -> `full`,
/// Navigable -> `quick`, ADR-0001). Whether that tier was actually achieved
/// is read off `mismatches`: empty means a clean pass; any entry means the
/// tape failed at (or before) the step that entry describes — including a
/// wholly absent seal marker, reported as a `SealUnreadable` mismatch rather
/// than as an `Err`. Callers (the T6 write session) decide pass/quarantine
/// from `mismatches`, never from `tier` alone.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Evidence {
    pub tier: Tier,
    pub files_checked: u32,
    pub mismatches: Vec<Mismatch>,
}

impl Evidence {
    /// The mismatches that prove **the medium is bad**, in walk order
    /// ([`MismatchKind::proves_medium_bad`], ADR-0012's 2026-09-17
    /// amendment). Empty means either a clean pass or a failure that says
    /// nothing about the tape — a dirty drive, a wrong block size, a
    /// transient SCSI error, a tape not loaded.
    ///
    /// This is the question a failed `volume verify` actually asks, so it
    /// lives next to the classification rather than as an `any()` at the
    /// call site: one predicate, one place, and adding a caller cannot
    /// reintroduce the blanket rule the amendment rejected.
    pub fn medium_evidence(&self) -> Vec<&Mismatch> {
        self.mismatches
            .iter()
            .filter(|m| m.kind.proves_medium_bad())
            .collect()
    }

    /// Whether any mismatch proves the medium is bad — the quarantine
    /// verdict, as a bool. See [`Evidence::medium_evidence`].
    pub fn proves_medium_bad(&self) -> bool {
        self.mismatches.iter().any(|m| m.kind.proves_medium_bad())
    }
}

/// The validate-time capacity oracle (`layout-session.md` validation point 1).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CapacityReport {
    pub usable_bytes: u64,
}

/// A medium that executes a Layout's entries at contact and can attest to
/// what it holds afterward.
pub trait Store {
    /// The usable capacity available for a Layout to fit inside
    /// (`Layout::validate`'s capacity oracle).
    fn capacity(&mut self) -> Result<CapacityReport>;

    /// Stream `len` bytes from `src`, followed by a file mark. `sync`
    /// requests a synchronous (durable) file mark — v2 uses this only for
    /// the seal marker; every other v2 entry uses `sync=false` (the final
    /// flush covers everything written before it; v1's op-envelope sync
    /// marks are a caller choice unrelated to this trait). Returns the
    /// number of bytes committed to the medium, including any block padding.
    /// A full medium is an `Err` — there is no salvage path (ADR-0007); the
    /// caller turns that into a clean abort to an unsealed tape.
    fn execute(&mut self, src: &mut dyn Read, len: u64, sync: bool) -> Result<u64>;

    /// Run the keyless integrity chain walk (`volume-format-v2.md` §5)
    /// against `layout` at the requested `tier`. Never fails with `Err` over
    /// tape *content* — every disagreement, including a wholly absent or
    /// unparseable seal marker, is recorded in the returned [`Evidence`]
    /// rather than propagated (fail-safe precedence, `v2-open-questions.md`
    /// §2.5). An `Err` here means `layout` itself is malformed (no
    /// front-index or seal-marker entry) — a caller bug, not a tape
    /// condition.
    ///
    /// Provided by [`chain_walk`] via [`Self::read_file`] — the algorithm is
    /// medium-agnostic (the walk only needs to read a file at a position),
    /// so every impl gets the identical, real chain-walk code for free
    /// (T6 review finding #1: `TapeStore` and `MemStore` previously
    /// duplicated this exact body). Override only if a medium needs a
    /// genuinely different confirm strategy (e.g. a future `WarehouseStore`'s
    /// deposit receipt, `layout-session.md`'s Store seam) — TapeStore and
    /// MemStore both take the default.
    fn confirm(&mut self, layout: &Layout, tier: Tier) -> Result<Evidence> {
        // Issue #386: every byte read back counts toward the caller's
        // progress phase (`confirm`, `verify`), and the file being read is
        // its current item. No-ops with no progress session.
        let files = layout.entries.len();
        chain_walk(layout, tier, |position, sink| {
            crate::progress::item(format!("file {position} of {files}"));
            let mut counted = crate::progress::CountingWriter(sink);
            self.read_file(position, &mut counted)
        })
    }

    /// Read the tape file at `position` (0-indexed), streaming its bytes
    /// into `sink` as they are read rather than buffering the whole file.
    /// Returns the total bytes read (the on-tape length, padding included —
    /// trimming to the true size is the caller's job).
    fn read_file(&mut self, position: u32, sink: &mut dyn Write) -> Result<u64>;

    /// The first `max_bytes` of the file at `position`, or the whole file if
    /// it is shorter. Returns the byte count delivered.
    ///
    /// The default reads the whole file through a bounded sink — correct on
    /// any store, and what `MemStore` uses. A tape overrides it to stop
    /// reading after the first block(s): `catalog rebuild --key <escrow>`
    /// attests escrow coverage by trial-decrypting one slice *header* per
    /// stage set (#137), and a slice can be tens of gigabytes.
    fn read_file_head(
        &mut self,
        position: u32,
        max_bytes: u64,
        sink: &mut dyn Write,
    ) -> Result<u64> {
        struct Head<'a> {
            inner: &'a mut dyn Write,
            remaining: u64,
            written: u64,
        }
        impl Write for Head<'_> {
            fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
                let take = (buf.len() as u64).min(self.remaining) as usize;
                if take > 0 {
                    self.inner.write_all(&buf[..take])?;
                    self.remaining -= take as u64;
                    self.written += take as u64;
                }
                // Report the whole buffer consumed so the producer keeps
                // going to the file mark; we simply stop forwarding.
                Ok(buf.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                self.inner.flush()
            }
        }
        let mut head = Head {
            inner: sink,
            remaining: max_bytes,
            written: 0,
        };
        self.read_file(position, &mut head)?;
        Ok(head.written)
    }

    /// Position the store for a resumed write session immediately before
    /// tape file `file_index` (0-indexed): the next `execute()` call becomes
    /// that file. Anything previously recorded at or after `file_index` is
    /// discarded — a resumed session repositions only to what its own DB
    /// cursor already confirms is durably written
    /// (`docs/design/layout-session.md`'s two-case cursor rule: BOT if zero
    /// slices were written, else `front_zone_len + written_slices`), never
    /// forward past it. A fresh (non-resumed) session that never reads
    /// anything first would not need this either — it would start writing
    /// from position 0 implicitly, by virtue of never having written
    /// anything yet.
    ///
    /// Issue #27 adds one exception: `write::check_fresh_write_contact`
    /// reads File 0 (and possibly a seal-marker position) before a fresh
    /// `volume_init`/`volume_write`'s first write, which moves `TapeStore`'s
    /// physical head. Both callers therefore call `reposition_for_resume(0)`
    /// once, immediately after a passing check, to undo that probe and
    /// land back at BOT before `execute` ever runs — `file_index = 0` is a
    /// pure "go to BOT" for both `Store` impls, identical to what a fresh
    /// session that skipped the check would have started at anyway.
    ///
    /// No pre-T6 caller needed this (nothing could resume a session before
    /// now), so this is an additive method on an existing trait, not a
    /// behavior change to `execute`/`confirm`/`read_file`/`capacity`.
    ///
    /// For `TapeStore`: rewind + forward-space `file_index` filemarks. On
    /// real tape, writing after forward-spacing to a filemark boundary
    /// begins a new recording there; the exact hardware behavior (does a
    /// fresh EOD orphan what was physically beyond the old one, per
    /// `v2-open-questions.md` §3.2's "stale-tail unreachability"?) is
    /// deferred to the LTO-6 validation session
    /// (`docs/lto6-validation-checklist.md`) like the rest of real
    /// EOT/EOD behavior — mhvtl cannot exercise this either. For `MemStore`:
    /// truncates `files`/`syncs` to `file_index` entries, which is exactly
    /// "discard anything at or after this position" for an in-memory
    /// recording, and is what makes the resume cursor rule unit-testable
    /// with no tape anywhere.
    fn reposition_for_resume(&mut self, file_index: u32) -> Result<()>;
}

/// The §5 chain walk, shared by every `Store` impl's `confirm` so the exact
/// algorithm — not a re-description of it — is what both `TapeStore` (via
/// hardware/mhvtl) and `MemStore` (via the unit tests below and the future
/// T7 synthetic-heir harness) run. `read` streams one tape file's on-tape
/// (still block-padded) bytes by position into the given sink, mirroring
/// `Store::read_file`'s own signature exactly (confirm's default impl is a
/// direct pass-through to it) — any `Err` it returns is folded into the
/// walk's fail-safe verdict rather than propagated — reading past what a
/// store actually holds is exactly the "absent" case for the seal marker,
/// and an ordinary read failure for anything else.
///
/// Buffering is decided HERE, per file, not by the caller (issue #86): the
/// seal marker and the front index (File 3) are parsed as TOML, so their
/// true bytes are genuinely needed — both are small and bounded (a front
/// index is ≈54 KB for a full tape), so buffering them into a `Vec` is
/// correct and cheap. Every other (content) file is only ever hashed, never
/// otherwise inspected — these are the potentially multi-GB slices/
/// envelopes, so they stream through a hash-only sink
/// (`TruncatingWriter<HashingWriter<io::Sink>>`, discarding into
/// `io::sink()`) that never materializes the file, trimming block padding
/// to the front index's claimed `size_bytes` as the bytes arrive exactly
/// like `restore.rs::restore_one_slice_inner`'s ciphertext pass does.
///
/// **Read order (issue #389).** The seal marker is read first and alone — it
/// is the precedence gate (§2.5: absent or unparseable means unsealed, and
/// the walk stops there) — and everything after it is read in ascending
/// position order: at the Integrity tier the files ahead of File 3 (0, 1, 2:
/// the ID thunk, system guide and RESTORE.sh), then File 3, then every
/// content file after it. On a tape that is one locate to the seal, one
/// rewind, and the single forward pass from BOP `volume-format-v2.md` §5
/// describes, where it used to be a rewind and locate per file. The files
/// ahead of File 3 cannot be checked until File 3 says what they should
/// hash to, so they are HELD — they are the small generated front-zone
/// files, bounded by [`AHEAD_OF_INDEX_CAP`] — and checked in step 4 in the
/// front index's own order, so every mismatch, its kind, its order and
/// `files_checked` are exactly what reading them in step 4 would give. A
/// file the Layout does not list ahead of File 3, or one too large to hold,
/// is simply read in step 4 as before.
fn chain_walk<F>(layout: &Layout, tier: Tier, mut read: F) -> Result<Evidence>
where
    F: FnMut(u32, &mut dyn Write) -> Result<u64>,
{
    let seal_entry = layout
        .entries
        .iter()
        .find(|e| matches!(e.kind, ZoneKind::SealMarker))
        .ok_or_else(|| TapectlError::Other("layout has no seal_marker entry".into()))?;
    let fi_entry = layout
        .entries
        .iter()
        .find(|e| matches!(e.kind, ZoneKind::FrontIndex))
        .ok_or_else(|| TapectlError::Other("layout has no front_index entry".into()))?;
    let seal_pos = seal_entry.position as u32;
    let fi_pos = fi_entry.position as u32;
    let Some(fi_true_len) = fi_entry.size_bytes else {
        return Err(TapectlError::Other(
            "layout's front_index entry has no size_bytes".into(),
        ));
    };
    let fi_true_len = fi_true_len as usize;

    let mut mismatches: Vec<Mismatch> = Vec::new();
    let mut files_checked: u32 = 0;

    let evidence = 'walk: {
        // Step 1 (§5.1): read + parse the seal marker (the last file). It is
        // parsed as TOML, so its true bytes are genuinely needed — small and
        // bounded, so buffering into a `Vec` here is correct (issue #86).
        // Absent or unparseable is the normal unsealed signal, never an Err.
        let mut seal_bytes = Vec::new();
        if let Err(e) = read(seal_pos, &mut seal_bytes) {
            mismatches.push(Mismatch {
                position: seal_pos,
                kind: MismatchKind::SealUnreadable,
                expected: "seal marker present and readable".to_string(),
                actual: format!("read failed: {e}"),
            });
            break 'walk Evidence {
                tier,
                files_checked,
                mismatches,
            };
        }
        files_checked += 1;
        let seal_str = String::from_utf8_lossy(&seal_bytes);
        let seal = match format::parse_seal_marker(&seal_str) {
            Ok(s) => s,
            Err(e) => {
                mismatches.push(Mismatch {
                    position: seal_pos,
                    kind: MismatchKind::SealUnreadable,
                    expected: "seal marker parses".to_string(),
                    actual: format!("parse failed: {e}"),
                });
                break 'walk Evidence {
                    tier,
                    files_checked,
                    mismatches,
                };
            }
        };

        // Integrity only: read the files ahead of File 3 now, from BOT, so
        // the rest of the walk is one forward pass (issue #389). Held, not
        // judged — nothing here touches `mismatches` or `files_checked`;
        // step 4 judges them in the front index's order.
        let held = if tier == Tier::Integrity {
            hold_files_ahead_of_index(layout, fi_pos, &mut read)
        } else {
            HashMap::new()
        };

        // Step 2 (§5.2): hash File 3's TRUE bytes; compare to the seal's
        // binding. File 3 is also parsed as TOML in step 3 below, so its
        // true bytes are genuinely needed too — same bounded-size reasoning
        // as the seal marker (issue #86).
        let mut fi_bytes = Vec::new();
        if let Err(e) = read(fi_pos, &mut fi_bytes) {
            mismatches.push(Mismatch {
                position: fi_pos,
                kind: MismatchKind::FrontIndexUnreadable,
                expected: "front index present and readable".to_string(),
                actual: format!("read failed: {e}"),
            });
            break 'walk Evidence {
                tier,
                files_checked,
                mismatches,
            };
        }
        files_checked += 1;
        if fi_true_len > fi_bytes.len() {
            mismatches.push(Mismatch {
                position: fi_pos,
                kind: MismatchKind::FrontIndexUnreadable,
                expected: format!("{fi_true_len} on-tape bytes"),
                actual: format!("only {} bytes read back", fi_bytes.len()),
            });
            break 'walk Evidence {
                tier,
                files_checked,
                mismatches,
            };
        }
        let fi_true_bytes = &fi_bytes[..fi_true_len];
        let fi_hash = sha256_hex(fi_true_bytes);
        if fi_hash != seal.front_index_sha256 {
            mismatches.push(Mismatch {
                position: fi_pos,
                kind: MismatchKind::FrontIndexDivergesFromSeal,
                expected: seal.front_index_sha256.clone(),
                actual: fi_hash,
            });
            // Divergence is quarantine-grade (§2.5), but the walk continues
            // so one confirm call surfaces every disagreement rather than
            // stopping at the first (report, not fail-fast).
        }

        // Step 3 (§5.3): parse File 3, run the §2.5 self-consistency checks,
        // then diff every entry against the Layout = Navigable tier.
        let fi_str = String::from_utf8_lossy(fi_true_bytes);
        let parsed_fi = match format::parse_front_index(&fi_str) {
            Ok(v) => v,
            Err(e) => {
                mismatches.push(Mismatch {
                    position: fi_pos,
                    kind: MismatchKind::FrontIndexUnreadable,
                    expected: "front index parses".to_string(),
                    actual: format!("parse failed: {e}"),
                });
                break 'walk Evidence {
                    tier,
                    files_checked,
                    mismatches,
                };
            }
        };

        for violation in format::validate_consistency(&parsed_fi) {
            mismatches.push(Mismatch {
                position: fi_pos,
                kind: MismatchKind::FrontIndexInconsistent,
                expected: "front index entries are self-consistent (§2.5)".to_string(),
                actual: violation.to_string(),
            });
        }

        for entry in &layout.entries {
            let position = entry.position as u32;
            let Some(claim) = parsed_fi.iter().find(|p| p.position == entry.position) else {
                mismatches.push(Mismatch {
                    position,
                    kind: MismatchKind::NavigationDisagreement,
                    expected: format!("front index lists position {}", entry.position),
                    actual: "missing from front index".to_string(),
                });
                continue;
            };
            if claim.type_label != entry.kind.type_label() {
                mismatches.push(Mismatch {
                    position,
                    kind: MismatchKind::NavigationDisagreement,
                    expected: entry.kind.type_label().to_string(),
                    actual: claim.type_label.clone(),
                });
            }
            // Both File 3's own entry and the seal marker's entry may
            // legitimately omit size_bytes (self-reference / not-yet-known
            // at File-3-build-time — an exclusion rule that may evolve;
            // this diff only flags an outright disagreement, never a bare
            // omission on either side).
            if let (Some(a), Some(b)) = (claim.size_bytes, entry.size_bytes) {
                if a != b {
                    mismatches.push(Mismatch {
                        position,
                        kind: MismatchKind::NavigationDisagreement,
                        expected: format!("size_bytes {b}"),
                        actual: format!("size_bytes {a}"),
                    });
                }
            }
        }

        if tier == Tier::Navigable {
            break 'walk Evidence {
                tier,
                files_checked,
                mismatches,
            };
        }

        // Step 4 (§5.4, Integrity tier only): every file except File 3 and
        // the seal marker, truncated to the front index's claimed size,
        // hashed and compared to the front index's sha256_encrypted.
        for claim in &parsed_fi {
            let position = claim.position as u32;
            if position == fi_pos || position == seal_pos {
                continue;
            }
            let (Some(want_hash), Some(want_size)) = (&claim.sha256_encrypted, claim.size_bytes)
            else {
                mismatches.push(Mismatch {
                    position,
                    kind: MismatchKind::NavigationDisagreement,
                    expected: "front index carries size_bytes + sha256_encrypted".to_string(),
                    actual: "one or both missing for a content file".to_string(),
                });
                continue;
            };

            // Content files are only ever hashed, never otherwise
            // inspected — so unlike the seal marker/front index above, this
            // never buffers the file. `TruncatingWriter` trims to `want_size`
            // (the front index's claimed true length) as bytes arrive,
            // wrapping a `HashingWriter` that discards into `io::sink()` —
            // the same composition `restore.rs::restore_one_slice_inner`
            // uses for its ciphertext pass, just with a sink that has no use
            // for the bytes themselves (issue #86).
            let mut sink = TruncatingWriter::new(HashingWriter::new(io::sink()), want_size);
            // A file held from the pass ahead of File 3 is fed through the
            // identical sink, as `MemStore::read_file` would feed it.
            let read_result = match held.get(&position) {
                Some(Held::Bytes { bytes, read }) => sink
                    .write_all(bytes)
                    .map(|()| *read)
                    .map_err(|e| TapectlError::Other(format!("sink write: {e}")).to_string()),
                Some(Held::Failed(e)) => Err(e.clone()),
                None => read(position, &mut sink).map_err(|e| e.to_string()),
            };
            let hashing = sink.into_inner();

            let n_read = match read_result {
                Ok(n) => n,
                Err(e) => {
                    // Issue #239: a raw I/O or transport error. NOT a hash
                    // disagreement — no hash was ever computed — so it is
                    // not `ContentHashMismatch` and does not quarantine.
                    mismatches.push(Mismatch {
                        position,
                        kind: MismatchKind::ContentUnreadable,
                        expected: "file readable".to_string(),
                        actual: format!("read failed: {e}"),
                    });
                    continue;
                }
            };
            files_checked += 1;

            if want_size > n_read {
                // Issue #239: a short read. Ambiguous between a truncated
                // write and a drive giving up early, and the count alone
                // cannot separate them, so it takes the same side
                // `FrontIndexUnreadable` already takes for the identical
                // event one position over.
                mismatches.push(Mismatch {
                    position,
                    kind: MismatchKind::ContentUnreadable,
                    expected: format!("{want_size} on-tape bytes"),
                    actual: format!("only {n_read} bytes read back"),
                });
                continue;
            }
            let actual_hash = hashing.finalize_hex();
            if &actual_hash != want_hash {
                mismatches.push(Mismatch {
                    position,
                    kind: MismatchKind::ContentHashMismatch,
                    expected: want_hash.clone(),
                    actual: actual_hash,
                });
            }
        }

        Evidence {
            tier,
            files_checked,
            mismatches,
        }
    };

    Ok(evidence)
}

/// The most on-tape bytes `chain_walk` will hold for one file ahead of
/// File 3. Those are the generated front-zone files (ID thunk, system
/// guide, RESTORE.sh): a few kilobytes each, one 512 KiB block apiece on
/// tape. The cap only bounds memory against a Layout or tape that claims
/// otherwise; a file over it is read in step 4 instead, which costs a
/// rewind and changes no verdict.
const AHEAD_OF_INDEX_CAP: u64 = 16 * 1024 * 1024;

/// One file read ahead of File 3, kept for step 4 of the chain walk.
enum Held {
    /// Its on-tape (padded) bytes, as `read` delivered them, and the byte
    /// count `read` returned.
    Bytes { bytes: Vec<u8>, read: u64 },
    /// The read failed; the error, as step 4 would have reported it.
    Failed(String),
}

/// Read the Layout's files ahead of File 3, in position order, into memory
/// for [`chain_walk`]'s step 4 (issue #389). A file the Layout gives no size
/// for, or one over [`AHEAD_OF_INDEX_CAP`], is skipped (step 4 reads it); so
/// is one that turns out larger on tape than the cap.
fn hold_files_ahead_of_index<F>(layout: &Layout, fi_pos: u32, read: &mut F) -> HashMap<u32, Held>
where
    F: FnMut(u32, &mut dyn Write) -> Result<u64>,
{
    let block = layout.block_size.max(1);
    let mut ahead: Vec<u32> = layout
        .entries
        .iter()
        .filter(|e| (e.position as u32) < fi_pos)
        .filter(|e| !matches!(e.kind, ZoneKind::FrontIndex | ZoneKind::SealMarker))
        .filter(|e| {
            e.size_bytes
                .is_some_and(|n| pad_to_blocks(n, block) <= AHEAD_OF_INDEX_CAP)
        })
        .map(|e| e.position as u32)
        .collect();
    ahead.sort_unstable();
    ahead.dedup();

    let mut held = HashMap::new();
    for position in ahead {
        let mut buf = CappedBuffer::default();
        match read(position, &mut buf) {
            Ok(_) if buf.overflowed => {}
            Ok(n) => {
                held.insert(
                    position,
                    Held::Bytes {
                        bytes: buf.bytes,
                        read: n,
                    },
                );
            }
            Err(e) => {
                held.insert(position, Held::Failed(e.to_string()));
            }
        }
    }
    held
}

/// A `Write` that keeps up to [`AHEAD_OF_INDEX_CAP`] bytes and, past that,
/// drops everything and says so — while still accepting every write, so the
/// read under it runs to its filemark and leaves the head where a whole
/// read would.
#[derive(Default)]
struct CappedBuffer {
    bytes: Vec<u8>,
    overflowed: bool,
}

impl Write for CappedBuffer {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if !self.overflowed {
            if (self.bytes.len() + buf.len()) as u64 > AHEAD_OF_INDEX_CAP {
                self.overflowed = true;
                self.bytes = Vec::new();
            } else {
                self.bytes.extend_from_slice(buf);
            }
        }
        Ok(buf.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn sha256_hex(bytes: &[u8]) -> String {
    let mut h = Sha256::new();
    h.update(bytes);
    format!("{:x}", h.finalize())
}

/// The st operations [`TapeStore`] issues — the seam its file cursor is
/// tested through (issue #389). [`TapeDevice`] in production; the unit tests'
/// in-memory tape (`crate::tape::fake`) otherwise, so the repositioning code
/// under test is the real one, not a description of it. Nothing here moves
/// the tape backwards except `rewind`.
pub(crate) trait TapeOps: Send {
    /// The block size the device reads and writes (0: variable-block mode)
    /// — the size of one buffer in [`TapeStore`]'s read pipeline (#390).
    fn block_size(&self) -> usize;
    fn rewind(&self) -> Result<()>;
    fn forward_space_file(&self, count: i32) -> Result<()>;
    /// st's own count of where the head is (`MTIOCGET`). Moves no tape.
    fn position(&self) -> Result<TapePosition>;
    fn write_stream(&mut self, src: &mut dyn Read, len: u64, sync: bool) -> Result<u64>;
    fn read_file_streaming(&mut self, sink: &mut dyn Write) -> Result<(u64, ReadEnd)>;
    fn read_file_head(&mut self, max_bytes: u64, sink: &mut dyn Write) -> Result<(u64, ReadEnd)>;
}

impl TapeOps for TapeDevice {
    fn block_size(&self) -> usize {
        TapeDevice::block_size(self)
    }
    fn rewind(&self) -> Result<()> {
        TapeDevice::rewind(self)
    }
    fn forward_space_file(&self, count: i32) -> Result<()> {
        TapeDevice::forward_space_file(self, count)
    }
    fn position(&self) -> Result<TapePosition> {
        TapeDevice::get_position(self)
    }
    fn write_stream(&mut self, src: &mut dyn Read, len: u64, sync: bool) -> Result<u64> {
        TapeDevice::write_stream(self, src, len, sync)
    }
    fn read_file_streaming(&mut self, sink: &mut dyn Write) -> Result<(u64, ReadEnd)> {
        TapeDevice::read_file_streaming(self, sink)
    }
    fn read_file_head(&mut self, max_bytes: u64, sink: &mut dyn Write) -> Result<(u64, ReadEnd)> {
        TapeDevice::read_file_head(self, max_bytes, sink)
    }
}

/// Where [`TapeStore`] believes the head is, counted in tape files
/// (issue #389).
///
/// Until 1.0.5 every read rewound to BOT and spaced forward `position`
/// filemarks, so a full-tape confirm or verify paid one rewind and one
/// locate per file — about 154 on a full LTO-6, roughly five hours of
/// L6-0001's ten-hour confirm. The cursor lets a read that is already at its
/// file, or ahead of the head, move only forward.
///
/// It is only ever an optimisation over that old rewind: anything that
/// leaves the head somewhere not known for certain sets [`Self::Unknown`],
/// and the next read rewinds exactly as before.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FileCursor {
    /// Not known — after any error, a write, or a read whose end is
    /// ambiguous. The next read rewinds.
    Unknown,
    /// At the start of tape file `n`: nothing of it read yet.
    AtStart(u32),
    /// Inside tape file `n`, before its filemark — where a
    /// [`Store::read_file_head`] that stopped early leaves it. Spacing
    /// forward `k` filemarks from anywhere inside `n` lands at the start of
    /// `n + k`, just as from its start; only re-reading `n` needs a rewind.
    Within(u32),
}

/// The motion a read at some position needs, given the cursor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Move {
    /// Already at the start of the file.
    Stay,
    /// Space forward this many filemarks.
    Forward(u32),
    /// Rewind, then space forward this many filemarks.
    FromBot(u32),
}

impl FileCursor {
    fn plan(self, target: u32) -> Move {
        match self {
            FileCursor::AtStart(n) if n == target => Move::Stay,
            FileCursor::AtStart(n) | FileCursor::Within(n) if target > n => {
                Move::Forward(target - n)
            }
            _ => Move::FromBot(target),
        }
    }

    /// Whether st's own count agrees with this cursor. `mt_fileno` is
    /// maintained by the st driver from what the drive reports, independently
    /// of tapectl's arithmetic, and `-1` when st has lost track.
    fn agrees_with(self, st: TapePosition) -> bool {
        match self {
            FileCursor::Unknown => false,
            FileCursor::AtStart(n) => st.file_number == n as i32 && st.block_number == 0,
            FileCursor::Within(n) => st.file_number == n as i32,
        }
    }

    /// The cursor in words, for the session log.
    fn describe(self) -> String {
        match self {
            FileCursor::Unknown => "unknown".to_string(),
            FileCursor::AtStart(n) => format!("start of file {n}"),
            FileCursor::Within(n) => format!("inside file {n}"),
        }
    }

    /// Where a read of the file at `position` left the head. Only a read
    /// that delivered bytes and then crossed the filemark is certain to be
    /// at the next file: a read that returns nothing at all is either a
    /// filemark-only file or end of data (issue #327: st returns 0 for
    /// both), and the two leave the head in different places.
    fn after_read(position: u32, read: &Result<(u64, ReadEnd)>) -> Self {
        match read {
            Ok((n, ReadEnd::Filemark)) if *n > 0 => FileCursor::AtStart(position.saturating_add(1)),
            Ok((_, ReadEnd::Stopped)) => FileCursor::Within(position),
            _ => FileCursor::Unknown,
        }
    }
}

/// LTO tape via the kernel st driver (fixed 512KB blocks).
///
/// Tracks which tape file the head is at ([`FileCursor`], issue #389) and
/// repositions only when a read needs it: not at all when the head is
/// already at the file, a relative forward space when the file is ahead,
/// and the old rewind-plus-space when it is behind or the position is not
/// known. Before trusting the cursor it checks st's own count (`MTIOCGET`),
/// and repositions from BOT if the two disagree.
pub struct TapeStore {
    dev: Box<dyn TapeOps>,
    usable_bytes: u64,
    cursor: FileCursor,
    /// The read pipeline's buffers (issue #390): [`pipeline::QUEUE_BYTES`]
    /// of tape blocks, allocated as a read first fills the queue and reused
    /// by every read after it.
    pool: BufferPool,
}

/// How much `TapeDevice` reads at a time in variable-block mode — its
/// `read_file_streaming`'s own figure, so a pipeline buffer is one read.
const VARIABLE_BLOCK_READ: usize = 1024 * 1024;

/// How a command opens the drive (issue #407).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OpenMode {
    /// `O_RDONLY`, and nothing set on the drive: every path that only reads —
    /// restore, identify, rebuild, `volume verify`, and a `volume resume`
    /// whose seal is recorded. A cartridge with its write-protect tab set,
    /// the right way to shelve a sealed tape, opens this way.
    ReadOnly,
    /// `O_RDWR`, then hardware compression off: the paths that write. The st
    /// driver refuses this open on a write-protected cartridge (EROFS).
    ReadWrite,
}

impl TapeStore {
    /// Open the drive and rewind to BOT, ready to write File 0. Hardware
    /// compression is disabled best-effort (encrypted data is incompressible;
    /// §2.8) — a drive that rejects the op is only logged, not failed.
    /// `usable_bytes` is the pre-flight capacity oracle's answer (nominal
    /// capacity × the configured usable-capacity factor); the caller
    /// computes it, since only the caller has the config in scope.
    pub fn open(device: &str, block_size: usize, usable_bytes: u64) -> Result<Self> {
        #[cfg(test)]
        if let Some(store) = injected::open(OpenMode::ReadWrite, usable_bytes) {
            return store;
        }
        let dev = TapeDevice::open(device, block_size)?;
        dev.rewind()?;
        if let Err(e) = dev.disable_compression() {
            tracing::warn!(err = %e, "could not disable hardware compression (continuing)");
        }
        Ok(Self::at_bot(Box::new(dev), usable_bytes))
    }

    /// Open the drive read-only, rewound to BOT — for the read paths
    /// (restore, verify, read-slices, rebuild), which only ever call
    /// `read_file`/`read_file_head` (issue #85 migrated the restore read
    /// seam onto this trait). Unlike [`Self::open`], this does not touch
    /// hardware compression (a write-time-only concern) and reports zero
    /// usable capacity, since nothing on a read path ever calls
    /// `capacity()`.
    pub fn open_read(device: &str, block_size: usize) -> Result<Self> {
        #[cfg(test)]
        if let Some(store) = injected::open(OpenMode::ReadOnly, 0) {
            return store;
        }
        let dev = TapeDevice::open_read(device, block_size)?;
        dev.rewind()?;
        Ok(Self::at_bot(Box::new(dev), 0))
    }

    /// [`Self::open_read`] or [`Self::open`], by `mode` — for the one path
    /// that decides at run time (`volume resume`: read-only when the volume's
    /// seal is recorded, so resuming can only re-enter `confirm`, issue
    /// #407). `usable_bytes` is used only by a [`OpenMode::ReadWrite`] open.
    pub fn open_as(
        device: &str,
        block_size: usize,
        mode: OpenMode,
        usable_bytes: u64,
    ) -> Result<Self> {
        match mode {
            OpenMode::ReadOnly => Self::open_read(device, block_size),
            OpenMode::ReadWrite => Self::open(device, block_size, usable_bytes),
        }
    }

    /// A store over a device the caller has just rewound.
    fn at_bot(dev: Box<dyn TapeOps>, usable_bytes: u64) -> Self {
        let chunk = match dev.block_size() {
            0 => VARIABLE_BLOCK_READ,
            n => n,
        };
        Self {
            dev,
            usable_bytes,
            cursor: FileCursor::AtStart(0),
            pool: BufferPool::with_queue_bytes(chunk, pipeline::QUEUE_BYTES),
        }
    }

    /// A store over any [`TapeOps`], rewound to BOT as `open_read` does —
    /// how the tests put the real cursor code over the in-memory tape.
    #[cfg(test)]
    pub(crate) fn from_ops(dev: Box<dyn TapeOps>, usable_bytes: u64) -> Result<Self> {
        dev.rewind()?;
        Ok(Self::at_bot(dev, usable_bytes))
    }

    /// The cursor, if st's own count agrees with it; otherwise
    /// [`FileCursor::Unknown`], so the read repositions from BOT.
    ///
    /// Why ask st at all, when tapectl is the only thing moving this tape
    /// (the st driver refuses a second open, and the SG commands tapectl
    /// issues — MAM, log pages, inquiry — never move it): because a cursor
    /// that is wrong by one reads file N+1 as file N, and in the chain walk
    /// that is a `ContentHashMismatch` or `NavigationDisagreement` — both
    /// `proves_medium_bad`, so a sound cartridge would be quarantined
    /// (ADR-0012's asymmetric cost). `MTIOCGET` moves no tape and costs
    /// microseconds; a disagreement can only cost a rewind, never a read of
    /// the wrong file.
    fn trusted_cursor(&self) -> FileCursor {
        if self.cursor == FileCursor::Unknown {
            return FileCursor::Unknown;
        }
        match self.dev.position() {
            Ok(st) if self.cursor.agrees_with(st) => self.cursor,
            Ok(st) => {
                tracing::warn!(
                    cursor = %self.cursor.describe(),
                    st_file = st.file_number,
                    st_block = st.block_number,
                    "tape position disagrees with the st driver's count; repositioning from BOT"
                );
                FileCursor::Unknown
            }
            Err(e) => {
                tracing::warn!(
                    cursor = %self.cursor.describe(),
                    err = %e,
                    "could not read the st driver's position; repositioning from BOT"
                );
                FileCursor::Unknown
            }
        }
    }

    /// Put the head at the start of tape file `target`, moving it as little
    /// as the cursor allows. The cursor is `Unknown` while the tape moves, so
    /// an error anywhere leaves it there and the next read rewinds.
    fn locate(&mut self, target: u32) -> Result<()> {
        let from = self.trusted_cursor();
        self.cursor = FileCursor::Unknown;
        match from.plan(target) {
            Move::Stay => {}
            Move::Forward(k) => self.dev.forward_space_file(k as i32)?,
            Move::FromBot(k) => {
                self.dev.rewind()?;
                if k > 0 {
                    self.dev.forward_space_file(k as i32)?;
                }
            }
        }
        self.cursor = FileCursor::AtStart(target);
        Ok(())
    }
}

impl Store for TapeStore {
    fn capacity(&mut self) -> Result<CapacityReport> {
        Ok(CapacityReport {
            usable_bytes: self.usable_bytes,
        })
    }

    /// The write path does not use the cursor: a write leaves it `Unknown`,
    /// so the first read after any write rewinds, exactly as before #389.
    fn execute(&mut self, src: &mut dyn Read, len: u64, sync: bool) -> Result<u64> {
        self.cursor = FileCursor::Unknown;
        self.dev.write_stream(src, len, sync)
    }

    // confirm: default trait method (T6 finding #1) — identical to what this
    // impl used to define directly. Its chain walk reads in ascending
    // position order after the seal marker, which is what lets the cursor
    // below make it one forward pass (issue #389).

    /// The tape read runs on its own thread, ahead of `sink` by up to the
    /// pool's capacity (issue #390), so the drive keeps streaming while the
    /// sink hashes or writes to disk. The cursor follows how the TAPE read
    /// ended, whatever the sink did: a sink that fails stops the read at its
    /// next block (an error, so `Unknown`); one that fails after the read
    /// already crossed the filemark leaves the head, truthfully, at the
    /// next file.
    fn read_file(&mut self, position: u32, sink: &mut dyn Write) -> Result<u64> {
        self.locate(position)?;
        let dev = &mut *self.dev;
        let delivered =
            pipeline::read_through(&mut self.pool, |pipe| dev.read_file_streaming(pipe), sink);
        self.cursor = FileCursor::after_read(position, &delivered.produced);
        delivered
            .sink
            .map_err(|e| TapectlError::TapeIo(format!("sink write: {e}")))?;
        delivered.produced.map(|(n, _)| n)
    }

    /// Overrides the default so a tape stops after the first block(s)
    /// instead of streaming a whole slice to a sink that discards it. That
    /// usually leaves the head inside the file ([`FileCursor::Within`]).
    fn read_file_head(
        &mut self,
        position: u32,
        max_bytes: u64,
        sink: &mut dyn Write,
    ) -> Result<u64> {
        self.locate(position)?;
        let read = self.dev.read_file_head(max_bytes, sink);
        self.cursor = FileCursor::after_read(position, &read);
        read.map(|(n, _)| n)
    }

    /// Always rewind + space, whatever the cursor says: this positions the
    /// next WRITE, and is kept exactly as it was before #389.
    fn reposition_for_resume(&mut self, file_index: u32) -> Result<()> {
        self.cursor = FileCursor::Unknown;
        self.dev.rewind()?;
        if file_index > 0 {
            self.dev.forward_space_file(file_index as i32)?;
        }
        self.cursor = FileCursor::AtStart(file_index);
        Ok(())
    }
}

/// An in-memory store: proves the interface is medium-agnostic (the "second
/// store implementable without touching Layout code" acceptance) and lets
/// `confirm` and the write session be unit-tested without a tape. Stores
/// PADDED bytes (zero-filled to `block_size`, mirroring tape semantics) so
/// the truncate-then-hash logic in [`chain_walk`] is exercised identically
/// to `TapeStore`.
pub struct MemStore {
    /// Every file's on-tape (padded) bytes, in write order == position.
    pub files: Vec<Vec<u8>>,
    /// Whether each corresponding file used a synchronous filemark.
    pub syncs: Vec<bool>,
    block_size: usize,
    usable_bytes: u64,
    /// If set, `execute()` fails with a simulated ENOSPC once the
    /// cumulative padded bytes already recorded plus this call's would
    /// exceed the budget. See [`Self::with_enospc_after`].
    enospc_after_bytes: Option<u64>,
}

impl MemStore {
    /// A fresh store with the given block size and an effectively unlimited
    /// capacity; chain with [`Self::with_usable_bytes`] to exercise
    /// capacity-gated tests.
    pub fn new(block_size: usize) -> Self {
        Self {
            files: Vec::new(),
            syncs: Vec::new(),
            block_size,
            usable_bytes: u64::MAX,
            enospc_after_bytes: None,
        }
    }

    /// Override the capacity `capacity()` reports.
    pub fn with_usable_bytes(mut self, usable_bytes: u64) -> Self {
        self.usable_bytes = usable_bytes;
        self
    }

    /// Simulate a medium that runs out of space after `budget` cumulative
    /// padded bytes have been written: the next `execute()` call whose
    /// write would cross that budget fails with a simulated ENOSPC instead
    /// of succeeding, mirroring a real full medium
    /// (`docs/design/layout-session.md`: "on write ENOSPC, stop, leave the
    /// tape unsealed, mark the session aborted"). Without this, `execute()`
    /// can only fail via a genuinely truncated source — this is what makes
    /// the session's ENOSPC clean-abort path unit-testable with no tape
    /// hardware (T6 behavior 3). Distinct from [`Self::with_usable_bytes`]:
    /// that changes what `capacity()` *reports* (the pre-flight validate-time
    /// oracle); this changes what `execute()` *does* mid-write (the rare-miss
    /// backstop past that pre-flight gate, ADR-0007).
    pub fn with_enospc_after(mut self, budget: u64) -> Self {
        self.enospc_after_bytes = Some(budget);
        self
    }
}

impl Store for MemStore {
    fn capacity(&mut self) -> Result<CapacityReport> {
        Ok(CapacityReport {
            usable_bytes: self.usable_bytes,
        })
    }

    fn execute(&mut self, src: &mut dyn Read, len: u64, sync: bool) -> Result<u64> {
        let padded_len = pad_to_blocks(len, self.block_size as u64);
        if let Some(budget) = self.enospc_after_bytes {
            let already_written: u64 = self.files.iter().map(|f| f.len() as u64).sum();
            if already_written + padded_len > budget {
                return Err(TapectlError::Other(format!(
                    "MemStore: simulated ENOSPC — writing {padded_len} more bytes would exceed \
                     the {budget}-byte budget ({already_written} already recorded)"
                )));
            }
        }
        let mut buf = Vec::with_capacity(len as usize);
        src.take(len)
            .read_to_end(&mut buf)
            .map_err(|e| TapectlError::Other(format!("read source: {e}")))?;
        if (buf.len() as u64) < len {
            return Err(TapectlError::Other(format!(
                "source exhausted after {} of {len} declared bytes",
                buf.len()
            )));
        }
        buf.resize(padded_len as usize, 0);
        self.files.push(buf);
        self.syncs.push(sync);
        Ok(padded_len)
    }

    // confirm: default trait method (T6 finding #1) — identical to what this
    // impl used to define directly.

    fn read_file(&mut self, position: u32, sink: &mut dyn Write) -> Result<u64> {
        let bytes = self.files.get(position as usize).ok_or_else(|| {
            TapectlError::Other(format!("no file recorded at position {position}"))
        })?;
        sink.write_all(bytes)
            .map_err(|e| TapectlError::Other(format!("sink write: {e}")))?;
        Ok(bytes.len() as u64)
    }

    fn reposition_for_resume(&mut self, file_index: u32) -> Result<()> {
        self.files.truncate(file_index as usize);
        self.syncs.truncate(file_index as usize);
        Ok(())
    }
}

/// Test-only: a [`FakeTape`](crate::tape::fake::FakeTape) standing in for the
/// drive, so a test can run a command's real `TapeStore::open` /
/// `open_read` and see HOW it opened the drive (issue #407) — and, for a
/// refusal that must come before any tape contact, that it never opened it
/// at all (issue #406).
#[cfg(test)]
pub(crate) mod injected {
    use std::cell::RefCell;

    use super::{OpenMode, TapeStore};
    use crate::error::Result;
    use crate::tape::fake::FakeTape;

    thread_local! {
        static DRIVE: RefCell<Option<FakeTape>> = const { RefCell::new(None) };
    }

    /// While this lives, every `TapeStore::open`/`open_read` on this test's
    /// thread opens the installed fake instead of a device.
    pub(crate) struct InjectedDrive;

    impl InjectedDrive {
        pub(crate) fn install(fake: &FakeTape) -> Self {
            DRIVE.with(|d| *d.borrow_mut() = Some(fake.clone()));
            Self
        }
    }

    impl Drop for InjectedDrive {
        fn drop(&mut self) {
            DRIVE.with(|d| *d.borrow_mut() = None);
        }
    }

    /// The store over the installed fake, opened `mode` — `None` when no
    /// fake is installed, and the caller opens its device as in production.
    pub(super) fn open(mode: OpenMode, usable_bytes: u64) -> Option<Result<TapeStore>> {
        let fake = DRIVE.with(|d| d.borrow().clone())?;
        Some(
            fake.open_as(mode)
                .and_then(|()| TapeStore::from_ops(fake.boxed(), usable_bytes)),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::volume::layout::{generate_front_index, generate_seal_marker, FrontIndexFile};
    use crate::volume::layout_model::{CapacityBudget, ContentSource, Layout, LayoutEntry};
    use std::io::Cursor;
    use std::path::PathBuf;

    const BS: u64 = 512 * 1024;

    // --- basic Store mechanics ------------------------------------------

    #[test]
    fn memstore_records_entries_in_order() {
        let mut s = MemStore::new(BS as usize);
        s.execute(&mut Cursor::new(b"id-thunk".to_vec()), 8, false)
            .unwrap();
        s.execute(&mut Cursor::new(b"slice".to_vec()), 5, false)
            .unwrap();
        s.execute(&mut Cursor::new(b"op-envelope".to_vec()), 11, true)
            .unwrap();
        assert_eq!(s.files.len(), 3);
        assert_eq!(&s.files[0][..8], b"id-thunk");
        assert_eq!(s.syncs, vec![false, false, true]);
    }

    #[test]
    fn memstore_execute_pads_to_block_boundary_and_reports_padded_length() {
        let bs = 4096u64;
        let mut store = MemStore::new(bs as usize);
        let data = vec![7u8; 5000]; // not a multiple of 4096
        let committed = store
            .execute(&mut Cursor::new(data.clone()), data.len() as u64, false)
            .unwrap();
        let expected_padded = pad_to_blocks(data.len() as u64, bs);
        assert_eq!(committed, expected_padded);
        assert_eq!(store.files[0].len(), expected_padded as usize);
        assert_eq!(&store.files[0][..data.len()], &data[..]);
        assert!(store.files[0][data.len()..].iter().all(|&b| b == 0));
    }

    #[test]
    fn memstore_execute_errors_if_source_is_shorter_than_declared_len() {
        let mut store = MemStore::new(4096);
        let short = b"only four".to_vec();
        assert!(store.execute(&mut Cursor::new(short), 100, false).is_err());
    }

    #[test]
    fn memstore_capacity_reports_configured_usable_bytes() {
        let mut store = MemStore::new(4096).with_usable_bytes(123_456);
        assert_eq!(store.capacity().unwrap().usable_bytes, 123_456);
    }

    #[test]
    fn read_file_errors_on_unknown_position() {
        let mut store = MemStore::new(4096);
        let mut sink = Vec::new();
        assert!(store.read_file(0, &mut sink).is_err());
    }

    #[test]
    fn read_file_round_trips_padded_bytes() {
        let mut store = MemStore::new(4096);
        store
            .execute(&mut Cursor::new(b"hello".to_vec()), 5, false)
            .unwrap();
        let mut sink = Vec::new();
        let n = store.read_file(0, &mut sink).unwrap();
        assert_eq!(n, 4096);
        assert_eq!(sink.len(), 4096);
        assert_eq!(&sink[..5], b"hello");
    }

    /// The default `read_file_head`: exactly `max_bytes` of the padded file,
    /// and the whole file when it is shorter than asked.
    #[test]
    fn read_file_head_delivers_a_bounded_prefix() {
        let mut store = MemStore::new(16);
        let payload: Vec<u8> = (0u8..40).collect(); // pads to 48
        store
            .execute(&mut &payload[..], payload.len() as u64, true)
            .unwrap();

        let mut head = Vec::new();
        let n = store.read_file_head(0, 10, &mut head).unwrap();
        assert_eq!(n, 10);
        assert_eq!(head, (0u8..10).collect::<Vec<_>>());

        let mut all = Vec::new();
        let n = store.read_file_head(0, 1_000_000, &mut all).unwrap();
        assert_eq!(n, 48, "shorter than asked: the whole padded file");
        assert_eq!(&all[..40], &payload[..]);
    }

    // --- reposition_for_resume (T6) -------------------------------------

    #[test]
    fn reposition_for_resume_truncates_files_and_syncs_to_the_given_index() {
        let mut store = MemStore::new(4096);
        for b in [b'a', b'b', b'c', b'd'] {
            store.execute(&mut Cursor::new(vec![b]), 1, false).unwrap();
        }
        assert_eq!(store.files.len(), 4);

        store.reposition_for_resume(2).unwrap();
        assert_eq!(store.files.len(), 2, "files at/after index 2 discarded");
        assert_eq!(store.syncs.len(), 2, "syncs at/after index 2 discarded");
        assert_eq!(store.files[0][0], b'a');
        assert_eq!(store.files[1][0], b'b');

        // Writing after reposition continues from that index — the next
        // execute() becomes the new "file 2", exactly like a fresh session
        // starting there.
        store
            .execute(&mut Cursor::new(vec![b'X']), 1, false)
            .unwrap();
        assert_eq!(store.files.len(), 3);
        assert_eq!(store.files[2][0], b'X');
    }

    #[test]
    fn reposition_for_resume_to_zero_discards_everything_restart_from_bot() {
        // The "zero slices written" cursor-rule case: restart from BOT means
        // discarding anything the crashed attempt had written, even front-zone
        // metadata that happened to land before the crash.
        let mut store = MemStore::new(4096);
        store
            .execute(&mut Cursor::new(vec![1u8]), 1, false)
            .unwrap();
        store
            .execute(&mut Cursor::new(vec![2u8]), 1, false)
            .unwrap();
        store.reposition_for_resume(0).unwrap();
        assert!(store.files.is_empty());
        assert!(store.syncs.is_empty());
    }

    // --- ENOSPC injection (T6 behavior 3 support) ------------------------

    #[test]
    fn with_enospc_after_lets_writes_succeed_under_budget() {
        let mut store = MemStore::new(4096).with_enospc_after(4096 * 3);
        for _ in 0..3 {
            assert!(store.execute(&mut Cursor::new(vec![1u8]), 1, false).is_ok());
        }
    }

    #[test]
    fn with_enospc_after_fails_the_write_that_would_cross_the_budget() {
        let mut store = MemStore::new(4096).with_enospc_after(4096 * 2);
        // Each 1-byte write pads to a full 4096-byte block, so the third
        // call would push cumulative bytes to 3*4096 > the 2*4096 budget.
        assert!(store.execute(&mut Cursor::new(vec![1u8]), 1, false).is_ok());
        assert!(store.execute(&mut Cursor::new(vec![1u8]), 1, false).is_ok());
        let err = store.execute(&mut Cursor::new(vec![1u8]), 1, false);
        assert!(err.is_err(), "third write must simulate ENOSPC");
        // The failed call must not have recorded a partial/corrupt entry.
        assert_eq!(store.files.len(), 2);
    }

    // --- Tier::default (T6) ----------------------------------------------

    #[test]
    fn tier_defaults_to_integrity() {
        assert_eq!(Tier::default(), Tier::Integrity);
    }

    // --- confirm / chain_walk, via MemStore -----------------------------

    /// Build a small, self-consistent 6-file fixture (id_thunk, guide,
    /// restore_sh, front_index, one data slice, seal_marker) written into a
    /// `MemStore` by hand — no write session exists yet (T6). Returns the
    /// Layout `confirm` checks against and the store holding the matching
    /// bytes. `seal_hash_override` lets a test bind the seal marker to a
    /// deliberately wrong front-index hash.
    fn build_confirm_fixture(seal_hash_override: Option<&str>) -> (Layout, MemStore) {
        build_confirm_fixture_with(seal_hash_override, 1)
    }

    /// [`build_confirm_fixture`] with `n_slices` data slices at positions
    /// 4.., each a different byte pattern, and the seal marker after them.
    fn build_confirm_fixture_with(
        seal_hash_override: Option<&str>,
        n_slices: usize,
    ) -> (Layout, MemStore) {
        let id_thunk = b"ID THUNK CONTENT".to_vec();
        let guide = b"SYSTEM GUIDE CONTENT".to_vec();
        let restore_sh = b"#!/bin/sh\n# RESTORE.sh CONTENT\n".to_vec();
        // Deliberately not block-aligned.
        let slices: Vec<Vec<u8>> = (0..n_slices)
            .map(|i| vec![0xABu8.wrapping_add(i as u8); 300_000])
            .collect();
        let seal_pos = 4 + n_slices as u32;
        let total_files = seal_pos + 1;

        let id_hash = sha256_hex(&id_thunk);
        let guide_hash = sha256_hex(&guide);
        let restore_hash = sha256_hex(&restore_sh);
        let slice_hashes: Vec<String> = slices.iter().map(|s| sha256_hex(s)).collect();

        // File 3's own content: the front-index and seal-marker entries
        // stay size/hash-less (self-reference / not-yet-written); every
        // other entry carries its real size + on-tape hash.
        let mut fi_files = vec![
            FrontIndexFile {
                position: 0,
                type_label: "id_thunk",
                size_bytes: Some(id_thunk.len() as u64),
                sha256_encrypted: Some(id_hash.clone()),
            },
            FrontIndexFile {
                position: 1,
                type_label: "system_guide",
                size_bytes: Some(guide.len() as u64),
                sha256_encrypted: Some(guide_hash.clone()),
            },
            FrontIndexFile {
                position: 2,
                type_label: "restore_sh",
                size_bytes: Some(restore_sh.len() as u64),
                sha256_encrypted: Some(restore_hash.clone()),
            },
            FrontIndexFile {
                position: 3,
                type_label: "front_index",
                size_bytes: None,
                sha256_encrypted: None,
            },
        ];
        for (i, slice) in slices.iter().enumerate() {
            fi_files.push(FrontIndexFile {
                position: 4 + i as i32,
                type_label: "data_slice",
                size_bytes: Some(slice.len() as u64),
                sha256_encrypted: Some(slice_hashes[i].clone()),
            });
        }
        fi_files.push(FrontIndexFile {
            position: seal_pos as i32,
            type_label: "seal_marker",
            size_bytes: None,
            sha256_encrypted: None,
        });

        let fi_bytes = generate_front_index("FIXT01", &fi_files).into_bytes();
        let fi_hash = sha256_hex(&fi_bytes);
        let bound_hash = seal_hash_override.unwrap_or(&fi_hash);

        // The seal marker's embedded copy is MORE complete: fill in File 3's
        // own size + hash (known now, at seal time) before embedding.
        let mut seal_files = fi_files.clone();
        if let Some(e) = seal_files.iter_mut().find(|f| f.position == 3) {
            e.size_bytes = Some(fi_bytes.len() as u64);
            e.sha256_encrypted = Some(fi_hash.clone());
        }
        let seal_bytes =
            generate_seal_marker("FIXT01", total_files as i32, bound_hash, &seal_files)
                .into_bytes();

        let mut store = MemStore::new(BS as usize);
        store
            .execute(
                &mut Cursor::new(id_thunk.clone()),
                id_thunk.len() as u64,
                false,
            )
            .unwrap();
        store
            .execute(&mut Cursor::new(guide.clone()), guide.len() as u64, false)
            .unwrap();
        store
            .execute(
                &mut Cursor::new(restore_sh.clone()),
                restore_sh.len() as u64,
                false,
            )
            .unwrap();
        store
            .execute(
                &mut Cursor::new(fi_bytes.clone()),
                fi_bytes.len() as u64,
                false,
            )
            .unwrap();
        for slice in &slices {
            store
                .execute(&mut Cursor::new(slice.clone()), slice.len() as u64, false)
                .unwrap();
        }
        store
            .execute(
                &mut Cursor::new(seal_bytes.clone()),
                seal_bytes.len() as u64,
                true,
            )
            .unwrap();

        let mut entries = vec![
            LayoutEntry {
                position: 0,
                kind: ZoneKind::IdThunk,
                size_bytes: Some(id_thunk.len() as u64),
                sha256: Some(id_hash),
                source: ContentSource::Generated,
            },
            LayoutEntry {
                position: 1,
                kind: ZoneKind::SystemGuide,
                size_bytes: Some(guide.len() as u64),
                sha256: Some(guide_hash),
                source: ContentSource::Generated,
            },
            LayoutEntry {
                position: 2,
                kind: ZoneKind::RestoreSh,
                size_bytes: Some(restore_sh.len() as u64),
                sha256: Some(restore_hash),
                source: ContentSource::Generated,
            },
            LayoutEntry {
                position: 3,
                kind: ZoneKind::FrontIndex,
                size_bytes: Some(fi_bytes.len() as u64),
                sha256: Some(fi_hash),
                source: ContentSource::Generated,
            },
        ];
        for (i, slice) in slices.iter().enumerate() {
            entries.push(LayoutEntry {
                position: 4 + i as i32,
                kind: ZoneKind::Slice {
                    stage_slice_id: 1 + i as i64,
                },
                size_bytes: Some(slice.len() as u64),
                sha256: Some(slice_hashes[i].clone()),
                source: ContentSource::Staged(PathBuf::from(format!("/fixture/slice{i}.age"))),
            });
        }
        entries.push(LayoutEntry {
            position: seal_pos as i32,
            kind: ZoneKind::SealMarker,
            size_bytes: Some(seal_bytes.len() as u64),
            sha256: None,
            source: ContentSource::Generated,
        });

        let layout = Layout {
            label: "FIXT01".into(),
            volume_uuid: "uuid-fixt".into(),
            media_type: "LTO-6".into(),
            block_size: BS,
            budget: CapacityBudget {
                available_bytes: 1000 * BS,
                reserve_bytes: BS,
            },
            entries,
        };

        (layout, store)
    }

    /// A `Store` whose `read_file` streams a file in small fixed-size
    /// chunks — via several separate `sink.write_all()` calls — rather than
    /// `MemStore::read_file`'s single whole-buffer `write_all`. Proves
    /// `chain_walk`'s content-file hashing sink correctly accumulates hash
    /// state and respects the true/padding truncation boundary across many
    /// pushes, the way a real tape read (`TapeDevice::read_file_streaming`,
    /// block-sized pushes) actually arrives — `MemStore`'s one-shot
    /// `write_all` cannot exercise this (issue #86). Only `read_file` is
    /// exercised by these tests (via `confirm`'s default trait method); the
    /// other three `Store` methods are unused here.
    struct ChunkedStore {
        files: Vec<Vec<u8>>,
        chunk_size: usize,
    }

    impl Store for ChunkedStore {
        fn capacity(&mut self) -> Result<CapacityReport> {
            unimplemented!("ChunkedStore only exercises read_file via confirm")
        }

        fn execute(&mut self, _src: &mut dyn Read, _len: u64, _sync: bool) -> Result<u64> {
            unimplemented!("ChunkedStore only exercises read_file via confirm")
        }

        fn read_file(&mut self, position: u32, sink: &mut dyn Write) -> Result<u64> {
            let bytes = self.files.get(position as usize).ok_or_else(|| {
                TapectlError::Other(format!("no file recorded at position {position}"))
            })?;
            let mut total = 0u64;
            for chunk in bytes.chunks(self.chunk_size.max(1)) {
                sink.write_all(chunk)
                    .map_err(|e| TapectlError::Other(format!("sink write: {e}")))?;
                total += chunk.len() as u64;
            }
            Ok(total)
        }

        fn reposition_for_resume(&mut self, _file_index: u32) -> Result<()> {
            unimplemented!("ChunkedStore only exercises read_file via confirm")
        }
    }

    #[test]
    fn confirm_hashes_a_multi_chunk_content_file_correctly_via_genuine_streaming() {
        // Reuses build_confirm_fixture's well-formed 6-file layout and
        // on-tape bytes (already proven correct via the MemStore-backed
        // tests below), but re-hosts them in ChunkedStore, whose read_file
        // streams every file in small fixed chunks via several separate
        // write() calls — unlike MemStore's single write_all. This is the
        // genuine proof that chain_walk's content-file hashing sink
        // correctly accumulates state across many pushes and still trims
        // the true/padding boundary correctly when that boundary does NOT
        // land on a push boundary (300_000 is not a multiple of 4096) —
        // MemStore's one-shot write cannot exercise either property.
        let (layout, mem_store) = build_confirm_fixture(None);
        let mut store = ChunkedStore {
            files: mem_store.files,
            chunk_size: 4096, // several times smaller than the 300_000-byte slice
        };

        let evidence = store.confirm(&layout, Tier::Integrity).unwrap();
        assert_eq!(
            evidence.mismatches,
            Vec::new(),
            "genuine multi-push content hashing must still pass: {:?}",
            evidence.mismatches
        );
        assert_eq!(evidence.files_checked, 6);
    }

    #[test]
    fn confirm_detects_corruption_in_a_multi_chunk_content_file_via_genuine_streaming() {
        // Same fixture/store as above, but corrupts the same byte
        // `confirm_detects_content_hash_mismatch_only_at_integrity_tier`
        // does — proving detection still works, at the right position,
        // when the file is delivered via genuine multi-push streaming
        // rather than MemStore's single write.
        let (layout, mem_store) = build_confirm_fixture(None);
        let mut files = mem_store.files;
        files[4][100] ^= 0xFF;
        let mut store = ChunkedStore {
            files,
            chunk_size: 4096,
        };

        let evidence = store.confirm(&layout, Tier::Integrity).unwrap();
        assert_eq!(evidence.mismatches.len(), 1, "{:?}", evidence.mismatches);
        assert_eq!(evidence.mismatches[0].position, 4);
        assert_eq!(
            evidence.mismatches[0].kind,
            MismatchKind::ContentHashMismatch
        );
    }

    #[test]
    fn confirm_ignores_corruption_confined_to_the_padding_tail() {
        // The streaming content-file hash (issue #86) must hash only the
        // TRUE (unpadded) bytes the front index claims, exactly like the
        // old buffered `&bytes[..want_size]` slice it replaces — corruption
        // that lands only in the trailing block-padding region must never
        // surface as a ContentHashMismatch.
        // `confirm_detects_content_hash_mismatch_only_at_integrity_tier`
        // below only ever corrupts within the true region; this is the
        // complementary boundary case, proving the truncate-before-hash
        // contract holds under streaming, not just under whole-buffer
        // slicing.
        let (layout, mut store) = build_confirm_fixture(None);
        let true_len = layout
            .entries
            .iter()
            .find(|e| matches!(e.kind, ZoneKind::Slice { .. }))
            .unwrap()
            .size_bytes
            .unwrap() as usize;
        assert!(
            store.files[4].len() > true_len,
            "fixture must actually be padded for this test to mean anything"
        );
        // Flip a byte well within the padding tail (past the true length).
        let pad_index = true_len + 1000;
        store.files[4][pad_index] ^= 0xFF;

        let evidence = store.confirm(&layout, Tier::Integrity).unwrap();
        assert_eq!(
            evidence.mismatches,
            Vec::new(),
            "corruption confined to block padding must not be flagged: {:?}",
            evidence.mismatches
        );
    }

    // ── issue #234: which failures prove the MEDIUM is bad ──

    /// The classification ADR-0012's 2026-09-17 amendment ruled, pinned
    /// variant by variant.
    ///
    /// Spelled out one per line rather than derived from a helper: this is
    /// the list a CTO ratified, and a test that recomputed it from the same
    /// predicate under test would assert nothing. If a variant moves sides,
    /// this test is where that shows up as a deliberate edit.
    #[test]
    fn proves_medium_bad_classifies_every_kind_as_adr_0012_ruled() {
        let ratified: &[(MismatchKind, bool)] = &[
            // Medium evidence: the bytes on the tape are wrong or gone.
            (MismatchKind::ContentHashMismatch, true),
            (MismatchKind::FrontIndexDivergesFromSeal, true),
            (MismatchKind::FrontIndexInconsistent, true),
            (MismatchKind::NavigationDisagreement, true),
            // NOT medium evidence: "we could not read it today" is not "the
            // bytes are gone" (issue #239 for the content position), and an
            // absent seal is the normal unsealed signal.
            (MismatchKind::ContentUnreadable, false),
            (MismatchKind::FrontIndexUnreadable, false),
            (MismatchKind::SealUnreadable, false),
        ];
        for (kind, expected) in ratified {
            assert_eq!(
                kind.proves_medium_bad(),
                *expected,
                "{} is classified on the wrong side of ADR-0012's line",
                kind.label()
            );
        }

        // COMPLETENESS, so a future variant cannot slip through
        // unclassified. `slot` is exhaustive with no wildcard arm, exactly
        // like `proves_medium_bad` itself: an eighth variant fails to
        // COMPILE here (a new arm is required), a slot past `N` panics, and
        // a variant that has an arm but is missing from `ratified` above
        // fails the assertion. A green run therefore means every variant
        // that exists was ratified one line at a time.
        fn slot(kind: MismatchKind) -> usize {
            match kind {
                MismatchKind::SealUnreadable => 0,
                MismatchKind::FrontIndexUnreadable => 1,
                MismatchKind::FrontIndexInconsistent => 2,
                MismatchKind::FrontIndexDivergesFromSeal => 3,
                MismatchKind::NavigationDisagreement => 4,
                MismatchKind::ContentHashMismatch => 5,
                MismatchKind::ContentUnreadable => 6,
            }
        }
        const N: usize = 7;
        let mut covered = [false; N];
        for (kind, _) in ratified {
            covered[slot(*kind)] = true;
        }
        assert!(
            covered.iter().all(|c| *c),
            "every MismatchKind must be classified above, by hand: {covered:?}"
        );
    }

    fn mismatch_of(kind: MismatchKind) -> Mismatch {
        Mismatch {
            position: 3,
            kind,
            expected: "e".into(),
            actual: "a".into(),
        }
    }

    /// `Evidence`'s verdict is "ANY mismatch proves it", not "all of them" —
    /// a tape that is genuinely rotten does not stop being rotten because
    /// the seal was also unreadable.
    #[test]
    fn evidence_medium_verdict_is_any_not_all() {
        let clean = Evidence {
            tier: Tier::Integrity,
            files_checked: 6,
            mismatches: Vec::new(),
        };
        assert!(!clean.proves_medium_bad());
        assert!(clean.medium_evidence().is_empty());

        let drive_only = Evidence {
            tier: Tier::Integrity,
            files_checked: 2,
            mismatches: vec![
                mismatch_of(MismatchKind::SealUnreadable),
                mismatch_of(MismatchKind::FrontIndexUnreadable),
            ],
        };
        assert!(
            !drive_only.proves_medium_bad(),
            "read failures alone say nothing about the tape"
        );
        assert!(drive_only.medium_evidence().is_empty());

        let mixed = Evidence {
            tier: Tier::Integrity,
            files_checked: 6,
            mismatches: vec![
                mismatch_of(MismatchKind::SealUnreadable),
                mismatch_of(MismatchKind::ContentHashMismatch),
            ],
        };
        assert!(mixed.proves_medium_bad());
        let named = mixed.medium_evidence();
        assert_eq!(named.len(), 1, "only the medium-proving one is named");
        assert_eq!(named[0].kind, MismatchKind::ContentHashMismatch);
    }

    #[test]
    fn confirm_happy_path_has_zero_mismatches_at_integrity_tier() {
        let (layout, mut store) = build_confirm_fixture(None);
        let evidence = store.confirm(&layout, Tier::Integrity).unwrap();
        assert_eq!(evidence.tier, Tier::Integrity);
        assert_eq!(evidence.mismatches, Vec::new(), "{:?}", evidence.mismatches);
        // Seal + File 3 + the 4 remaining content files (id_thunk, guide,
        // restore_sh, slice) are all read during an Integrity pass.
        assert_eq!(evidence.files_checked, 6);
    }

    #[test]
    fn confirm_happy_path_has_zero_mismatches_at_navigable_tier() {
        let (layout, mut store) = build_confirm_fixture(None);
        let evidence = store.confirm(&layout, Tier::Navigable).unwrap();
        assert_eq!(evidence.tier, Tier::Navigable);
        assert_eq!(evidence.mismatches, Vec::new(), "{:?}", evidence.mismatches);
        // Navigable only reads the seal marker and File 3.
        assert_eq!(evidence.files_checked, 2);
    }

    #[test]
    fn confirm_detects_content_hash_mismatch_only_at_integrity_tier() {
        let (layout, mut store) = build_confirm_fixture(None);
        // Flip a byte well within the slice's true (unpadded) region.
        store.files[4][100] ^= 0xFF;

        let nav = store.confirm(&layout, Tier::Navigable).unwrap();
        assert_eq!(
            nav.mismatches,
            Vec::new(),
            "navigable tier must not hash content: {:?}",
            nav.mismatches
        );

        let full = store.confirm(&layout, Tier::Integrity).unwrap();
        assert_eq!(full.mismatches.len(), 1);
        assert_eq!(full.mismatches[0].position, 4);
        assert_eq!(full.mismatches[0].kind, MismatchKind::ContentHashMismatch);
    }

    /// ISSUE #239, producer mapping. The chain walk's three content-file
    /// failure paths used to raise ONE kind, classified medium-proving.
    /// Only the third is evidence about the medium, so only the third may
    /// still carry `ContentHashMismatch`.
    ///
    /// A `MemStore` whose read fails at one position — the partially failing
    /// drive that clears File 0, the seal marker and the front index and
    /// then faults on a slice.
    struct ReadFaultStore {
        inner: MemStore,
        fault_at: u32,
    }

    impl Store for ReadFaultStore {
        fn capacity(&mut self) -> Result<CapacityReport> {
            self.inner.capacity()
        }
        fn execute(&mut self, src: &mut dyn Read, len: u64, sync: bool) -> Result<u64> {
            self.inner.execute(src, len, sync)
        }
        fn read_file(&mut self, position: u32, sink: &mut dyn Write) -> Result<u64> {
            if position == self.fault_at {
                // The shape `TapeDevice::read_file_streaming` produces from
                // a kernel read error (`src/tape/ioctl.rs`).
                return Err(TapectlError::TapeIo(
                    "read: Input/output error (os error 5)".to_string(),
                ));
            }
            self.inner.read_file(position, sink)
        }
        fn reposition_for_resume(&mut self, file_index: u32) -> Result<()> {
            self.inner.reposition_for_resume(file_index)
        }
    }

    #[test]
    fn a_content_read_error_is_content_unreadable_not_a_hash_mismatch() {
        let (layout, mem) = build_confirm_fixture(None);
        let mut store = ReadFaultStore {
            inner: mem,
            fault_at: 4,
        };

        let evidence = store.confirm(&layout, Tier::Integrity).unwrap();
        assert_eq!(evidence.mismatches.len(), 1, "{:?}", evidence.mismatches);
        assert_eq!(evidence.mismatches[0].position, 4);
        assert_eq!(
            evidence.mismatches[0].kind,
            MismatchKind::ContentUnreadable,
            "no hash was ever computed, so this cannot be a hash mismatch"
        );
        assert!(
            !evidence.proves_medium_bad(),
            "a drive fault must not condemn the cartridge"
        );
        assert!(
            !evidence.mismatches[0].kind.compares_hashes(),
            "`expected`/`actual` are prose here, not sha256 hex"
        );
    }

    #[test]
    fn a_short_content_read_is_content_unreadable_not_a_hash_mismatch() {
        let (layout, mut store) = build_confirm_fixture(None);
        let true_len = layout
            .entries
            .iter()
            .find(|e| matches!(e.kind, ZoneKind::Slice { .. }))
            .unwrap()
            .size_bytes
            .unwrap() as usize;
        // Fewer bytes come back than the front index claims, with the
        // claimed hash untouched so nothing else can be what failed.
        store.files[4].truncate(true_len - 1);

        let evidence = store.confirm(&layout, Tier::Integrity).unwrap();
        assert_eq!(evidence.mismatches.len(), 1, "{:?}", evidence.mismatches);
        assert_eq!(evidence.mismatches[0].position, 4);
        assert_eq!(evidence.mismatches[0].kind, MismatchKind::ContentUnreadable);
        assert!(!evidence.proves_medium_bad());
    }

    /// The complement of the two above, and the guard on issue #234: a
    /// genuine hash disagreement keeps the medium-proving kind. Distinct
    /// from `confirm_detects_content_hash_mismatch_only_at_integrity_tier`
    /// in what it asserts — that one pins the tier gate, this one pins that
    /// the #239 split did not take the checksum case with it.
    #[test]
    fn a_genuine_hash_disagreement_still_proves_the_medium_is_bad() {
        let (layout, mut store) = build_confirm_fixture(None);
        store.files[4][100] ^= 0xFF;

        let evidence = store.confirm(&layout, Tier::Integrity).unwrap();
        assert_eq!(evidence.mismatches.len(), 1, "{:?}", evidence.mismatches);
        assert_eq!(
            evidence.mismatches[0].kind,
            MismatchKind::ContentHashMismatch
        );
        assert!(evidence.proves_medium_bad());
        assert!(
            evidence.mismatches[0].kind.compares_hashes(),
            "both sides really are sha256 hex here"
        );
    }

    #[test]
    fn confirm_reports_unsealed_when_seal_file_is_absent() {
        let (layout, mut store) = build_confirm_fixture(None);
        store.files.pop(); // drop the seal marker entirely (position 5 gone)

        let evidence = store.confirm(&layout, Tier::Integrity).unwrap();
        assert_eq!(evidence.files_checked, 0);
        assert_eq!(evidence.mismatches.len(), 1);
        assert_eq!(evidence.mismatches[0].kind, MismatchKind::SealUnreadable);
        assert_eq!(evidence.mismatches[0].position, 5);
    }

    #[test]
    fn confirm_reports_unsealed_when_seal_file_is_garbage() {
        let (layout, mut store) = build_confirm_fixture(None);
        store.files[5] = vec![0xFFu8; 100]; // present, but not parseable TOML

        let evidence = store.confirm(&layout, Tier::Integrity).unwrap();
        assert_eq!(evidence.files_checked, 1, "the seal read itself succeeded");
        assert_eq!(evidence.mismatches.len(), 1);
        assert_eq!(evidence.mismatches[0].kind, MismatchKind::SealUnreadable);
    }

    #[test]
    fn confirm_detects_front_index_divergence_from_seal_binding() {
        let (layout, mut store) = build_confirm_fixture(Some(&"0".repeat(64)));
        let evidence = store.confirm(&layout, Tier::Navigable).unwrap();
        assert!(evidence
            .mismatches
            .iter()
            .any(|m| m.kind == MismatchKind::FrontIndexDivergesFromSeal));
    }

    #[test]
    fn confirm_surfaces_front_index_consistency_violations() {
        let (mut layout, mut store) = build_confirm_fixture(None);

        // Work from File 3's TRUE (unpadded) bytes — store.files[3] is the
        // block-padded tape buffer, and splicing text into the padded NUL
        // tail would corrupt the document instead of the intended entry
        // list. The Layout's own front-index entry carries the true length.
        let fi_true_len = layout
            .entries
            .iter()
            .find(|e| matches!(e.kind, ZoneKind::FrontIndex))
            .unwrap()
            .size_bytes
            .unwrap() as usize;
        let mut s = String::from_utf8(store.files[3][..fi_true_len].to_vec()).unwrap();

        // Duplicate the id_thunk [[files]] block: the document still parses
        // as valid TOML, but now claims position 0 twice — a §2.5 violation
        // the chain walk must surface as a mismatch, not a parse crash.
        let dup_start = s.find("[[files]]").unwrap();
        let dup_end = dup_start + s[dup_start..].find("\n\n[[files]]").unwrap();
        let block = s[dup_start..dup_end].to_string();
        s.push('\n');
        s.push_str(&block);
        let corrupted_true_bytes = s.into_bytes();
        let corrupted_len = corrupted_true_bytes.len() as u64;

        let mut padded = corrupted_true_bytes.clone();
        padded.resize(pad_to_blocks(corrupted_len, BS) as usize, 0);
        store.files[3] = padded;
        if let Some(e) = layout
            .entries
            .iter_mut()
            .find(|e| matches!(e.kind, ZoneKind::FrontIndex))
        {
            e.size_bytes = Some(corrupted_len);
        }

        // Rebuild + re-store the seal marker bound to the corrupted File 3
        // so step 2 (the binding hash) passes and the walk actually reaches
        // the consistency check in step 3.
        let new_hash = sha256_hex(&corrupted_true_bytes);
        let seal_files = vec![FrontIndexFile {
            position: 3,
            type_label: "front_index",
            size_bytes: Some(corrupted_len),
            sha256_encrypted: Some(new_hash.clone()),
        }];
        let seal_true_bytes =
            generate_seal_marker("FIXT01", 6, &new_hash, &seal_files).into_bytes();
        let seal_len = seal_true_bytes.len() as u64;
        let mut seal_padded = seal_true_bytes;
        seal_padded.resize(pad_to_blocks(seal_len, BS) as usize, 0);
        store.files[5] = seal_padded;
        if let Some(e) = layout
            .entries
            .iter_mut()
            .find(|e| matches!(e.kind, ZoneKind::SealMarker))
        {
            e.size_bytes = Some(seal_len);
        }

        let evidence = store.confirm(&layout, Tier::Navigable).unwrap();
        assert!(
            evidence
                .mismatches
                .iter()
                .any(|m| m.kind == MismatchKind::FrontIndexInconsistent),
            "{:?}",
            evidence.mismatches
        );

        // Issue #357: `actual` is what `volume verify` prints as "found" and
        // records in verification_results — the violation in words, never
        // Rust's `{:?}` (`PositionOutOfSequence { index: .., .. }`).
        let found: Vec<&str> = evidence
            .mismatches
            .iter()
            .filter(|m| m.kind == MismatchKind::FrontIndexInconsistent)
            .map(|m| m.actual.as_str())
            .collect();
        assert!(
            found.iter().any(|a| a.contains("claims position 0")),
            "the duplicated position-0 entry must be named in words: {found:?}"
        );
        for actual in &found {
            assert!(
                !actual.contains('{') && !actual.contains("PositionOutOfSequence"),
                "a consistency violation must not reach the operator as Debug output: {actual}"
            );
        }
    }

    #[test]
    fn confirm_errors_if_layout_lacks_a_seal_marker_entry() {
        let mut store = MemStore::new(4096);
        let layout = Layout {
            label: "X".into(),
            volume_uuid: "u".into(),
            media_type: "LTO-6".into(),
            block_size: 4096,
            budget: CapacityBudget {
                available_bytes: 1,
                reserve_bytes: 0,
            },
            entries: vec![LayoutEntry {
                position: 0,
                kind: ZoneKind::IdThunk,
                size_bytes: Some(1),
                sha256: None,
                source: ContentSource::Generated,
            }],
        };
        assert!(store.confirm(&layout, Tier::Navigable).is_err());
    }

    #[test]
    fn confirm_errors_if_layout_lacks_a_front_index_entry() {
        let mut store = MemStore::new(4096);
        let layout = Layout {
            label: "X".into(),
            volume_uuid: "u".into(),
            media_type: "LTO-6".into(),
            block_size: 4096,
            budget: CapacityBudget {
                available_bytes: 1,
                reserve_bytes: 0,
            },
            entries: vec![LayoutEntry {
                position: 0,
                kind: ZoneKind::SealMarker,
                size_bytes: Some(1),
                sha256: None,
                source: ContentSource::Generated,
            }],
        };
        assert!(store.confirm(&layout, Tier::Navigable).is_err());
    }

    // ── issue #389: TapeStore reads forward instead of rewinding per file ──
    //
    // Each test drives the REAL `TapeStore` (cursor, cross-check, confirm's
    // chain walk) over `tape::fake::FakeTape`, which logs every motion. The
    // open's own rewind is in the log, because `open_read` issues it: "one
    // rewind" for a restore means that one.

    use crate::tape::fake::{FakeTape, Op};

    /// A `TapeStore` over a fake tape holding `files`, as `open_read` leaves
    /// it: rewound, with that rewind logged.
    fn tape_over(files: Vec<Vec<u8>>) -> (TapeStore, FakeTape) {
        let fake = FakeTape::with_files(files, BS as usize);
        let store = TapeStore::from_ops(fake.boxed(), u64::MAX).unwrap();
        (store, fake)
    }

    /// `n` one-block files, file `i` filled with byte `i`.
    fn simple_tape(n: usize) -> (TapeStore, FakeTape) {
        tape_over((0..n).map(|i| vec![i as u8; BS as usize]).collect())
    }

    fn read_at(store: &mut TapeStore, position: u32) -> Result<Vec<u8>> {
        let mut out = Vec::new();
        store.read_file(position, &mut out).map(|_| out)
    }

    #[test]
    fn the_cursor_plans_the_least_motion_and_rewinds_when_it_must() {
        use FileCursor::*;
        assert_eq!(AtStart(4).plan(4), Move::Stay);
        assert_eq!(AtStart(4).plan(7), Move::Forward(3));
        assert_eq!(AtStart(4).plan(2), Move::FromBot(2));
        assert_eq!(Within(4).plan(5), Move::Forward(1));
        assert_eq!(
            Within(4).plan(4),
            Move::FromBot(4),
            "a file half-read cannot be re-read without going back"
        );
        assert_eq!(Unknown.plan(0), Move::FromBot(0));
        assert_eq!(Unknown.plan(9), Move::FromBot(9));
    }

    #[test]
    fn only_a_read_that_delivered_bytes_and_crossed_the_filemark_is_trusted() {
        use FileCursor::*;
        assert_eq!(
            FileCursor::after_read(3, &Ok((BS, ReadEnd::Filemark))),
            AtStart(4)
        );
        assert_eq!(
            FileCursor::after_read(3, &Ok((10, ReadEnd::Stopped))),
            Within(3)
        );
        // Issue #327: zero bytes is a filemark-only file OR end of data.
        assert_eq!(
            FileCursor::after_read(3, &Ok((0, ReadEnd::Filemark))),
            Unknown
        );
        assert_eq!(
            FileCursor::after_read(3, &Ok((BS, ReadEnd::EndOfMedium))),
            Unknown
        );
        assert_eq!(
            FileCursor::after_read(3, &Err(TapectlError::TapeIo("read: EIO".into()))),
            Unknown
        );
    }

    #[test]
    fn ascending_reads_space_forward_and_never_rewind() {
        let (mut store, fake) = simple_tape(8);
        for position in [4, 5, 6] {
            let bytes = read_at(&mut store, position).unwrap();
            assert_eq!(bytes, vec![position as u8; BS as usize]);
        }
        assert_eq!(
            fake.ops(),
            vec![
                Op::Rewind,
                Op::Space(4),
                Op::Read(4),
                Op::Read(5),
                Op::Read(6)
            ]
        );
    }

    #[test]
    fn a_read_ahead_of_the_head_spaces_only_the_difference() {
        let (mut store, fake) = simple_tape(8);
        read_at(&mut store, 1).unwrap();
        assert_eq!(read_at(&mut store, 6).unwrap(), vec![6u8; BS as usize]);
        assert_eq!(
            fake.ops(),
            vec![
                Op::Rewind,
                Op::Space(1),
                Op::Read(1),
                Op::Space(4),
                Op::Read(6)
            ]
        );
    }

    #[test]
    fn a_read_behind_the_head_or_of_the_same_file_rewinds() {
        let (mut store, fake) = simple_tape(8);
        read_at(&mut store, 5).unwrap();
        assert_eq!(read_at(&mut store, 2).unwrap(), vec![2u8; BS as usize]);
        assert_eq!(read_at(&mut store, 2).unwrap(), vec![2u8; BS as usize]);
        assert_eq!(
            fake.ops(),
            vec![
                Op::Rewind,
                Op::Space(5),
                Op::Read(5),
                Op::Rewind,
                Op::Space(2),
                Op::Read(2),
                Op::Rewind,
                Op::Space(2),
                Op::Read(2),
            ]
        );
    }

    /// The acceptance criterion: after any read error the next read
    /// repositions from BOT. The failed read leaves the fake's head INSIDE
    /// file 4, so a store that trusted its cursor would hand back the rest
    /// of file 4 as file 5.
    #[test]
    fn after_a_read_error_the_next_read_repositions_from_bot() {
        let fake = FakeTape::with_files(
            (0..8).map(|i| vec![i as u8; 2 * BS as usize]).collect(),
            BS as usize,
        );
        fake.state().fail_reads_at = vec![4];
        let mut store = TapeStore::from_ops(fake.boxed(), u64::MAX).unwrap();

        assert!(read_at(&mut store, 4).is_err());
        assert_eq!(read_at(&mut store, 5).unwrap(), vec![5u8; 2 * BS as usize]);
        assert_eq!(
            fake.ops(),
            vec![
                Op::Rewind,
                Op::Space(4),
                Op::Read(4),
                Op::Rewind,
                Op::Space(5),
                Op::Read(5),
            ]
        );
    }

    #[test]
    fn after_a_failed_space_the_next_read_repositions_from_bot() {
        let (mut store, fake) = simple_tape(4);
        assert!(
            read_at(&mut store, 9).is_err(),
            "no file 9 on a 4-file tape"
        );
        assert_eq!(read_at(&mut store, 1).unwrap(), vec![1u8; BS as usize]);
        assert_eq!(
            fake.ops(),
            vec![
                Op::Rewind,
                Op::Space(9),
                Op::Rewind,
                Op::Space(1),
                Op::Read(1)
            ]
        );
    }

    /// A read that returns nothing is a filemark-only file or end of data,
    /// and only a rewind tells them apart (issue #327). Reading one past the
    /// end then must fail exactly as it did before #389 — a space past end
    /// of data — not return "nothing" again from a guessed position.
    #[test]
    fn a_read_that_returns_nothing_forgets_the_position() {
        let (mut store, fake) = simple_tape(3);
        assert_eq!(read_at(&mut store, 3).unwrap(), Vec::<u8>::new());
        assert!(read_at(&mut store, 4).is_err());
        assert_eq!(
            fake.ops(),
            vec![
                Op::Rewind,
                Op::Space(3),
                Op::Read(3),
                Op::Rewind,
                Op::Space(4)
            ]
        );
    }

    #[test]
    fn a_head_read_leaves_the_head_inside_the_file() {
        let (mut store, fake) = tape_over(
            (0..8)
                .map(|i| vec![i as u8; 3 * BS as usize])
                .collect::<Vec<_>>(),
        );
        let mut head = Vec::new();
        assert_eq!(store.read_file_head(2, 10, &mut head).unwrap(), 10);
        // Forward from inside file 2: one filemark, not a rewind.
        assert_eq!(read_at(&mut store, 3).unwrap(), vec![3u8; 3 * BS as usize]);
        // A second head read of the same file cannot be served from inside it.
        store.read_file_head(5, 10, &mut Vec::new()).unwrap();
        store.read_file_head(5, 10, &mut Vec::new()).unwrap();
        assert_eq!(
            fake.ops(),
            vec![
                Op::Rewind,
                Op::Space(2),
                Op::ReadHead(2),
                Op::Space(1),
                Op::Read(3),
                Op::Space(1),
                Op::ReadHead(5),
                Op::Rewind,
                Op::Space(5),
                Op::ReadHead(5),
            ]
        );
    }

    /// The other half of the `read_file_head` edge: a file shorter than the
    /// budget is read to its filemark, so the head is already at the next
    /// file and spacing one more would skip it.
    #[test]
    fn a_head_read_that_crosses_the_filemark_is_at_the_next_file() {
        let (mut store, fake) = simple_tape(6);
        let n = store.read_file_head(2, 4 * BS, &mut Vec::new()).unwrap();
        assert_eq!(n, BS);
        assert_eq!(read_at(&mut store, 3).unwrap(), vec![3u8; BS as usize]);
        assert_eq!(
            fake.ops(),
            vec![Op::Rewind, Op::Space(2), Op::ReadHead(2), Op::Read(3)]
        );
    }

    /// Something other than this store moved the tape: st's count no longer
    /// matches the cursor, so the cursor is not trusted.
    #[test]
    fn a_cursor_the_st_driver_disagrees_with_is_not_trusted() {
        let (mut store, fake) = simple_tape(8);
        read_at(&mut store, 2).unwrap();
        fake.state().head = (6, 0);
        assert_eq!(read_at(&mut store, 3).unwrap(), vec![3u8; BS as usize]);
        assert_eq!(
            fake.ops(),
            vec![
                Op::Rewind,
                Op::Space(2),
                Op::Read(2),
                Op::Rewind,
                Op::Space(3),
                Op::Read(3),
            ]
        );
    }

    #[test]
    fn a_write_forgets_the_position_and_resume_positioning_always_rewinds() {
        let (mut store, fake) = simple_tape(3);
        read_at(&mut store, 2).unwrap();
        // Already at file 3 by the cursor, and still a rewind + space: this
        // positions a WRITE and is kept exactly as before.
        store.reposition_for_resume(3).unwrap();
        store
            .execute(&mut Cursor::new(vec![9u8; 10]), 10, false)
            .unwrap();
        assert_eq!(read_at(&mut store, 3).unwrap()[..10], [9u8; 10]);
        assert_eq!(
            fake.ops(),
            vec![
                Op::Rewind,
                Op::Space(2),
                Op::Read(2),
                Op::Rewind,
                Op::Space(3),
                Op::Write(3),
                Op::Rewind,
                Op::Space(3),
                Op::Read(3),
            ]
        );
    }

    /// THE acceptance test for confirm: a full Integrity confirm of an
    /// N-file layout on a freshly opened tape is the open's rewind, one
    /// space to the seal marker, one rewind, and a single forward pass —
    /// two rewinds, no other motion — and it reaches the verdict the
    /// position-addressed `MemStore` reaches on the same bytes.
    #[test]
    fn an_integrity_confirm_is_one_locate_to_the_seal_then_one_forward_pass() {
        const SLICES: usize = 12;
        let (layout, mut mem) = build_confirm_fixture_with(None, SLICES);
        let seal = 4 + SLICES as u32;
        let want = mem.confirm(&layout, Tier::Integrity).unwrap();
        assert!(want.mismatches.is_empty(), "{:?}", want.mismatches);
        assert_eq!(want.files_checked, seal + 1);

        let (mut store, fake) = tape_over(mem.files.clone());
        let got = store.confirm(&layout, Tier::Integrity).unwrap();
        assert_eq!(got, want, "the cursor must not change the verdict");

        let mut expected = vec![Op::Rewind, Op::Space(seal), Op::Read(seal), Op::Rewind];
        expected.extend((0..seal).map(Op::Read));
        assert_eq!(fake.ops(), expected);
        assert_eq!(fake.rewinds(), 2);
        assert_eq!(fake.spaces(), 1);
    }

    /// The write session's confirm: the writes leave the cursor unknown, so
    /// the seal read rewinds too — still two rewinds for the whole confirm.
    #[test]
    fn a_confirm_straight_after_the_writes_rewinds_twice() {
        let (layout, mem) = build_confirm_fixture_with(None, 8);
        let seal = 4 + 8;
        let (mut store, fake) = tape_over(Vec::new());
        for file in &mem.files {
            store
                .execute(&mut Cursor::new(file.clone()), file.len() as u64, false)
                .unwrap();
        }
        fake.clear_ops();

        let evidence = store.confirm(&layout, Tier::Integrity).unwrap();
        assert!(evidence.mismatches.is_empty(), "{:?}", evidence.mismatches);
        let mut expected = vec![Op::Rewind, Op::Space(seal), Op::Read(seal), Op::Rewind];
        expected.extend((0..seal).map(Op::Read));
        assert_eq!(fake.ops(), expected);
    }

    #[test]
    fn a_navigable_confirm_reads_the_seal_then_file_3() {
        let (layout, mem) = build_confirm_fixture_with(None, 5);
        let seal = 4 + 5;
        let (mut store, fake) = tape_over(mem.files);
        let evidence = store.confirm(&layout, Tier::Navigable).unwrap();
        assert!(evidence.mismatches.is_empty(), "{:?}", evidence.mismatches);
        assert_eq!(evidence.files_checked, 2);
        assert_eq!(
            fake.ops(),
            vec![
                Op::Rewind,
                Op::Space(seal),
                Op::Read(seal),
                Op::Rewind,
                Op::Space(3),
                Op::Read(3),
            ]
        );
    }

    /// The files held ahead of File 3 are judged in step 4, in the front
    /// index's order, exactly as reading them there would: a corrupt system
    /// guide and a corrupt slice give the two mismatches in position order,
    /// both counted as checked, on `MemStore` and on a tape alike.
    #[test]
    fn files_held_ahead_of_the_index_are_judged_as_if_read_in_step_4() {
        let (layout, mut mem) = build_confirm_fixture_with(None, 3);
        mem.files[1][3] ^= 0xFF; // the system guide, inside its true bytes
        mem.files[5][100] ^= 0xFF; // the second slice
        let evidence = mem.confirm(&layout, Tier::Integrity).unwrap();
        let found: Vec<(u32, MismatchKind)> = evidence
            .mismatches
            .iter()
            .map(|m| (m.position, m.kind))
            .collect();
        assert_eq!(
            found,
            vec![
                (1, MismatchKind::ContentHashMismatch),
                (5, MismatchKind::ContentHashMismatch),
            ]
        );
        // Seal + File 3 + Files 0..2 + three slices: a mismatch was read.
        assert_eq!(evidence.files_checked, 8);

        let (mut store, _fake) = tape_over(mem.files.clone());
        assert_eq!(store.confirm(&layout, Tier::Integrity).unwrap(), evidence);
    }

    /// A drive fault on a file held ahead of File 3 is `ContentUnreadable`
    /// at that file — the same evidence a position-addressed store reports
    /// — and the read after the fault (File 3) repositions from BOT.
    #[test]
    fn a_fault_ahead_of_the_index_is_content_unreadable_and_the_next_read_rewinds() {
        let (layout, mem) = build_confirm_fixture_with(None, 2);
        let seal = 4 + 2;
        let want = ReadFaultStore {
            inner: MemStore {
                files: mem.files.clone(),
                syncs: mem.syncs.clone(),
                block_size: BS as usize,
                usable_bytes: u64::MAX,
                enospc_after_bytes: None,
            },
            fault_at: 2,
        }
        .confirm(&layout, Tier::Integrity)
        .unwrap();
        assert_eq!(want.mismatches.len(), 1, "{:?}", want.mismatches);
        assert_eq!(want.mismatches[0].position, 2);
        assert_eq!(want.mismatches[0].kind, MismatchKind::ContentUnreadable);

        let (mut store, fake) = tape_over(mem.files);
        fake.state().fail_reads_at = vec![2];
        let got = store.confirm(&layout, Tier::Integrity).unwrap();
        assert_eq!(got, want);
        assert_eq!(
            fake.ops(),
            vec![
                Op::Rewind,
                Op::Space(seal),
                Op::Read(seal),
                Op::Rewind,
                Op::Read(0),
                Op::Read(1),
                Op::Read(2),
                Op::Rewind,
                Op::Space(3),
                Op::Read(3),
                Op::Read(4),
                Op::Read(5),
            ]
        );
    }

    /// An unsealed tape still stops at the seal, before anything else is
    /// read: the precedence gate comes first, as it always did.
    #[test]
    fn an_unsealed_tape_is_reported_from_the_seal_read_alone() {
        let (layout, mut mem) = build_confirm_fixture_with(None, 2);
        let seal = 4 + 2;
        mem.files.pop();
        let (mut store, fake) = tape_over(mem.files);
        let evidence = store.confirm(&layout, Tier::Integrity).unwrap();
        // The seal position is end of data here, and a read at end of data
        // returns nothing rather than failing — so the seal "read", and
        // fails to parse, as it does on a real drive.
        assert_eq!(evidence.files_checked, 1);
        assert_eq!(evidence.mismatches.len(), 1);
        assert_eq!(evidence.mismatches[0].kind, MismatchKind::SealUnreadable);
        assert_eq!(
            fake.ops(),
            vec![Op::Rewind, Op::Space(seal), Op::Read(seal)]
        );
    }

    // ── issue #390: TapeStore's reads run on their own thread ──

    /// Corruption is still found through the pipelined tape read: a flipped
    /// byte deep inside a slice is a `ContentHashMismatch` at its position —
    /// the verdict the position-addressed MemStore reaches on the same bytes
    /// — and the motion is still the single forward pass. The fake tape's
    /// blocks are 64 KiB here, so each slice crosses several of the read
    /// queue's buffers and the hash must see every one, in order.
    #[test]
    fn a_pipelined_confirm_still_finds_a_corrupted_slice() {
        const SLICES: usize = 3;
        const SMALL_BLOCK: usize = 64 * 1024;
        let (layout, mut mem) = build_confirm_fixture_with(None, SLICES);
        let seal = 4 + SLICES as u32;
        let tape = |files: Vec<Vec<u8>>| {
            let fake = FakeTape::with_files(files, SMALL_BLOCK);
            let store = TapeStore::from_ops(fake.boxed(), u64::MAX).unwrap();
            (store, fake)
        };

        let (mut store, _) = tape(mem.files.clone());
        assert_eq!(store.pool.chunk(), SMALL_BLOCK);
        let clean = store.confirm(&layout, Tier::Integrity).unwrap();
        assert!(clean.mismatches.is_empty(), "{:?}", clean.mismatches);

        // 300,000-byte slices: this byte is in the slice's fourth block.
        mem.files[5][200_000] ^= 0xFF;
        let want = mem.confirm(&layout, Tier::Integrity).unwrap();
        assert_eq!(want.mismatches.len(), 1, "{:?}", want.mismatches);
        assert_eq!(want.mismatches[0].kind, MismatchKind::ContentHashMismatch);
        assert_eq!(want.mismatches[0].position, 5);

        let (mut store, fake) = tape(mem.files.clone());
        let got = store.confirm(&layout, Tier::Integrity).unwrap();
        assert_eq!(got, want);
        let mut expected = vec![Op::Rewind, Op::Space(seal), Op::Read(seal), Op::Rewind];
        expected.extend((0..seal).map(Op::Read));
        assert_eq!(fake.ops(), expected);
    }

    /// A sink that fails partway through a multi-block file stops the
    /// read, reports the sink's own error as before, and leaves the cursor
    /// unknown — the tape read stopped inside the file — so the next read
    /// rewinds rather than trusting a head the read thread moved.
    #[test]
    fn a_sink_that_fails_mid_read_leaves_the_position_unknown() {
        struct FailsAfter(usize);
        impl Write for FailsAfter {
            fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
                if self.0 == 0 {
                    return Err(io::Error::from_raw_os_error(28));
                }
                self.0 -= 1;
                Ok(buf.len())
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }
        // A two-buffer queue and an eight-block file: the read cannot finish
        // before the sink fails (the production queue is 512 blocks; the
        // mechanism is the same).
        let (mut store, fake) = tape_over(vec![
            vec![0u8; BS as usize],
            vec![1u8; 8 * BS as usize],
            vec![2u8; BS as usize],
        ]);
        store.pool = BufferPool::new(BS as usize, 2);
        let err = store
            .read_file(1, &mut FailsAfter(1))
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("sink write: ") && err.contains("os error 28"),
            "the sink's own error, as before: {err}"
        );
        assert_eq!(
            store.cursor,
            FileCursor::Unknown,
            "the tape read stopped inside the file"
        );
        assert_eq!(read_at(&mut store, 2).unwrap(), vec![2u8; BS as usize]);
        assert_eq!(
            fake.ops(),
            vec![
                Op::Rewind,
                Op::Space(1),
                Op::Read(1),
                Op::Rewind,
                Op::Space(2),
                Op::Read(2)
            ]
        );
    }

    /// The other order: the tape read crosses the filemark before the sink
    /// fails on the last block. The read still fails, with the sink's error
    /// — but the head is truthfully at the next file, so the next read there
    /// moves nothing. (Before #390 the sink failing stopped the tape read
    /// itself short of the filemark, so the position was always lost.)
    #[test]
    fn a_sink_that_fails_after_the_filemark_leaves_the_head_known() {
        struct Fails;
        impl Write for Fails {
            fn write(&mut self, _: &[u8]) -> io::Result<usize> {
                Err(io::Error::from_raw_os_error(28))
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }
        let (mut store, fake) = simple_tape(3);
        // A one-block file: the tape read hands its only block to the queue
        // and reaches the filemark before the sink can refuse that block.
        assert!(store.read_file(1, &mut Fails).is_err());
        assert_eq!(store.cursor, FileCursor::AtStart(2));
        assert_eq!(read_at(&mut store, 2).unwrap(), vec![2u8; BS as usize]);
        assert_eq!(
            fake.ops(),
            vec![Op::Rewind, Op::Space(1), Op::Read(1), Op::Read(2)]
        );
    }

    /// The read queue is the bound, in tape blocks, allocated only as reads
    /// need it — and a whole read, however long, passes through it.
    #[test]
    fn the_tape_stores_read_queue_is_the_pipeline_bound() {
        let blocks = 6;
        let (mut store, _fake) = tape_over(vec![vec![9u8; blocks * BS as usize]]);
        assert_eq!(store.pool.chunk(), BS as usize);
        assert_eq!(store.pool.capacity_bytes(), pipeline::QUEUE_BYTES);
        assert_eq!(store.pool.allocated(), 0);
        assert_eq!(
            read_at(&mut store, 0).unwrap(),
            vec![9u8; blocks * BS as usize]
        );
        assert!(store.pool.allocated() <= blocks);
    }
}
