//! Where a restore's slices come from when the cartridge in the drive cannot
//! give one (ADR-0012 item 101, issue #480).
//!
//! A restore reads its unit's slices off one cartridge. Copies are written
//! from one staging, so every copy carries the same ciphertext for a slice
//! (the same `sha256_encrypted`); a slice that fails its hash or will not
//! read on one cartridge is read from another that holds it. This module is
//! that: the [`Supply`] a restore reads every slice through.
//!
//! # What a slice read does
//!
//! 1. **Read it from the cartridge in the drive** — the one the restore was
//!    started on, or the last one a substitution moved to. The slice
//!    streams off the tape into a decrypt thread chunk by chunk
//!    ([`crate::crypto::chunks`]), each 64 KiB chunk authenticated before
//!    it is written to the sink, and the whole ciphertext is hashed on the
//!    way. A clean read ends there; a clean restore costs exactly what it
//!    did before this module.
//! 2. **A chunk that does not authenticate stops the delivery** at the
//!    chunk before it (never a byte of unauthenticated plaintext), and the
//!    tape is still read to its end for the hash. The cause is then known:
//!    a tape that would not read, a hash mismatch (the medium), or a bad
//!    chunk under a matching hash (the slice was bad when it was written, so
//!    every copy has it).
//! 3. **The plaintext already delivered stays delivered.** The next copy is
//!    decrypted from the start but its first `delivered` bytes are skipped:
//!    the ciphertext is identical, so those bytes are the same. That is what
//!    lets a streamed restore swap cartridges mid-slice, with the named pipe
//!    dar reads held open across the swap, and it lets a second copy
//!    supply only what the first could not.
//! 4. **Another cartridge is asked for** — by label, sticker and chip
//!    serial — when a medium fault was the cause and the catalog knows
//!    another holder ([`crate::volume::slice_holders`]). The cartridge in
//!    the drive is kept as long as it holds what is wanted, so several bad
//!    slices in a row cost one swap, not one each. A cartridge that failed
//!    a slice at some offset is tried again only for what lies past that
//!    offset has been delivered by another.
//! 5. **No terminal, no swap**: with nobody to load a cartridge the read
//!    refuses by name, listing the holders, and `--yes` does not stand in
//!    for a person (it cannot load a cartridge).
//! 6. **Nothing can supply it, or the operator declined**: the slice is read
//!    once more from the cartridge in the drive with every bad chunk
//!    written as a hole of zeros ([`Mode::Tolerant`]), the damaged byte
//!    ranges are recorded, and the restore goes on. dar's own per-file CRCs
//!    then name the files the holes fell in. Only what authenticates is ever
//!    written; a hole is zeros.
//!
//! # What proves a slice whole
//!
//! A chunk's tag proves it was encrypted under the key its file's header
//! unwraps, at its index — not that the file is the slice the catalog
//! expected: any other age file to the same recipient authenticates in full.
//! So a slice is whole only by one of these:
//!
//! - **The ciphertext hash matched** (the clean path, and the only one that
//!   hashes nothing more): the bytes are the bytes written, whatever the
//!   reads that made them up, provided every read was the same age file.
//! - **The plaintext hash matched** the checksum recorded for the slice
//!   (`sha256_plain`), when the ciphertext's did not — a copy whose bad bytes
//!   lay in what another copy had already given. A spooled slice is hashed
//!   from its file when asked; a streamed one keeps a running hash from its
//!   second read on (what went down the pipe on the first cannot be hashed
//!   after the fact, so a slice whose first read was cut short and finished
//!   by another is *unproven* when streamed).
//!
//! Anything else is not whole. A read whose file is not the one that began
//! the slice (its header and nonce differ) is refused outright: chunks of
//! two files are never mixed. A plaintext that does not match is the wrong
//! slice — a spooled one is dropped and another copy tried, a streamed one
//! cannot be taken back out of dar and fails the restore, by name. A slice
//! that cannot be shown either way (holes; no recorded checksum; a stream) is
//! **unproven**: the restore records it ([`Damage`]) and judges the files in
//! it one by one against their recorded checksums.
//!
//! # The contacts
//!
//! Every cartridge that is in the drive during a restore is a contact
//! (ADR-0013): its own `cartridge_contacts` row, its MAM reads journalled
//! against it, a health reading when it leaves, and the identity check
//! ([`super::corroborate_at_contact`], including #476's `requires` check)
//! on arrival. The restore's first contact is the caller's; the [`Supply`]
//! closes it when the cartridge leaves and opens one for each that arrives.
//! The closing `detail` of each says which slices it supplied.

use std::collections::HashMap;
use std::io::Write;
use std::ops::Range;
use std::path::Path;

use age::secrecy::zeroize::Zeroizing;
use rusqlite::Connection;
use sha2::Digest;
use tracing::{info, warn};

use super::{open_pipe_for_dar, Detachable, WritePositionInfo};
use crate::config::{Config, LtoBackendConfig};
use crate::crypto::chunks::{self, Next};
use crate::dar;
use crate::error::{Result, TapectlError};
use crate::pass_count::Sha256;
use crate::progress;
use crate::store::{Store, TapeStore};
use crate::tape::contact::{self, ContactGuard, ContactSite, Medium, Operation};
use crate::tape::mam::MamInfo;
use crate::tape::mam_journal::MamReads;
use crate::util::{HashingWriter, TruncatingWriter};
use crate::volume::slice_holders::{self, Holder, Shelf, Standing};

/// How the cartridge in the drive was told to be read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Mode {
    /// Stop delivering at the first chunk that does not authenticate.
    Strict,
    /// Write a hole of zeros for each such chunk and go on.
    Tolerant,
}

/// What the operator answered when asked for a cartridge.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Answer {
    /// It is loaded.
    Loaded,
    /// Do not swap: go on without it, leaving a hole.
    Skip,
    /// Give the restore up here.
    Stop,
}

/// A cartridge the restore asks the operator for.
#[derive(Debug, Clone)]
pub(crate) struct Want {
    /// Why this cartridge: the damaged slice, or the files to fill.
    pub why: String,
    /// What the cartridge is, in the operator's terms: its label, its
    /// sticker and chip serial, and that it holds what is wanted.
    pub holds: String,
    pub label: String,
    /// The drive, as the operator named it.
    pub device: String,
    /// What `Skip` would do, in the prompt's words.
    pub skip_means: &'static str,
}

impl Want {
    /// The lines the operator reads before the answer is asked for.
    pub(crate) fn lines(&self) -> Vec<String> {
        vec![self.why.clone(), self.holds.clone()]
    }
}

/// `L6-0002 holds an identical copy (cartridge ABC, chip serial XYZ).` and,
/// for a volume a verify proved bad, that it is the last resort.
fn holds_line(label: &str, shelf: &Shelf, what: &str, quarantined: bool) -> String {
    let mut line = format!("{label} holds {what} ({}).", shelf.describe());
    if quarantined {
        line.push_str(
            " It is quarantined (a verify proved its medium bad), so it is the last resort.",
        );
    }
    line
}

/// The drive end of a restore: the cartridge in it, and the person at it.
///
/// One trait for both because they are one situation — the drive is released
/// so the cartridge can come out, the operator is asked, and the drive is
/// opened on what went in — and a test scripts the two together. Production
/// is [`DeviceBay`]; a restore handed only a store ([`FixedBay`]) has no
/// changer and no person, so it never swaps.
pub(crate) trait Bay {
    /// The store over the cartridge in the drive.
    fn store(&mut self) -> &mut dyn Store;
    /// Close the device so the cartridge can be taken out (`tapectl drive
    /// eject`, or the button): the st driver allows one open at a time.
    fn release(&mut self);
    /// Open the drive on whatever is loaded now, reading its MAM first as
    /// every read path does ([`crate::volume::binding::loaded_medium`]).
    fn load<'c, 'a>(&mut self, conn: &'c Connection, config: &'a Config)
        -> Result<Arrival<'c, 'a>>;
    /// Whether a person can be asked to change the cartridge.
    fn attended(&self) -> bool;
    /// Show `want` and read the answer.
    fn ask(&mut self, want: &Want) -> Answer;
}

/// What loading a cartridge learned about it before its store opened: the
/// MAM reads (journalled against the contact when it opens) and the medium
/// they described.
pub(crate) struct Arrival<'c, 'a> {
    reads: Option<MamReads<'c>>,
    observed: Option<(&'a LtoBackendConfig, MamInfo)>,
}

impl<'c, 'a> Arrival<'c, 'a> {
    /// A cartridge nothing was read from: no configured drive (the DR
    /// machine), or a test.
    #[cfg(test)]
    pub(crate) fn unread() -> Self {
        Arrival {
            reads: None,
            observed: None,
        }
    }

    pub(crate) fn read(
        reads: MamReads<'c>,
        observed: Option<(&'a LtoBackendConfig, MamInfo)>,
    ) -> Self {
        Arrival {
            reads: Some(reads),
            observed,
        }
    }

    pub(crate) fn with_site<R>(
        &self,
        config: &Config,
        operation: Operation,
        device: &str,
        f: impl FnOnce(&ContactSite<'_>) -> R,
    ) -> R {
        let site = ContactSite::new(
            config,
            operation,
            device,
            Medium::from_read(self.observed.as_ref().map(|(b, m)| (*b, m))),
        );
        match &self.reads {
            Some(reads) => f(&site.with_mam_reads(reads)),
            None => f(&site),
        }
    }
}

/// The restore handed one store and nothing else: the cartridge in it is the
/// only one there will ever be.
#[cfg(test)]
pub(crate) struct FixedBay<'s>(pub &'s mut dyn Store);

#[cfg(test)]
impl Bay for FixedBay<'_> {
    fn store(&mut self) -> &mut dyn Store {
        self.0
    }
    fn release(&mut self) {}
    fn load<'c, 'a>(&mut self, _: &'c Connection, _: &'a Config) -> Result<Arrival<'c, 'a>> {
        Err(TapectlError::Other(
            "this restore has no drive to load another cartridge into".into(),
        ))
    }
    fn attended(&self) -> bool {
        false
    }
    fn ask(&mut self, _: &Want) -> Answer {
        Answer::Stop
    }
}

/// The real drive: swaps by closing the device, asking on the terminal and
/// opening it again, as `volume compact` does between its steps.
pub(crate) struct DeviceBay<'a> {
    pub device: &'a str,
    pub block_size: usize,
    pub operation: Operation,
    /// `None` between a release and the next load.
    pub store: Option<TapeStore>,
}

impl Bay for DeviceBay<'_> {
    fn store(&mut self) -> &mut dyn Store {
        self.store
            .as_mut()
            .expect("the drive is open whenever a restore reads from it")
    }

    fn release(&mut self) {
        self.store = None;
    }

    fn load<'c, 'a>(
        &mut self,
        conn: &'c Connection,
        config: &'a Config,
    ) -> Result<Arrival<'c, 'a>> {
        // The same two reads every read path takes before its store opens
        // (issue #166, #297): refuse a medium this drive cannot read, then
        // learn the cartridge's serial.
        let reads = MamReads::new(conn, self.operation);
        reads.check_read_contact(config, self.device)?;
        let observed = crate::volume::binding::loaded_medium(config, self.device, &reads);
        let phase = progress::phase("drive-open", None);
        self.store = Some(TapeStore::open_read(self.device, self.block_size)?);
        phase.done();
        Ok(Arrival::read(reads, observed))
    }

    fn attended(&self) -> bool {
        crate::cli::consent::stdin_is_terminal()
    }

    fn ask(&mut self, want: &Want) -> Answer {
        for line in want.lines() {
            progress::stderr_println(&line);
        }
        progress::stderr_println(&format!(
            "Unload the cartridge in {} (`tapectl drive eject`), load {}, and press Enter. \
             Type \"skip\" to {}, or \"stop\" to give up (what is restored so far stays).",
            want.device, want.label, want.skip_means
        ));
        let _wait = progress::wait(|| format!("for the operator to load {}", want.label));
        eprint!("> ");
        let _ = std::io::stderr().flush();
        let mut input = String::new();
        match std::io::stdin().read_line(&mut input) {
            Ok(0) | Err(_) => Answer::Stop,
            Ok(_) => parse_answer(&input),
        }
    }
}

/// An operator's line, as an [`Answer`]: Enter (or "loaded", "y") is the
/// cartridge; "skip" goes on without it; anything else stops.
pub(crate) fn parse_answer(line: &str) -> Answer {
    match line.trim().to_ascii_lowercase().as_str() {
        "" | "y" | "yes" | "ok" | "loaded" | "done" => Answer::Loaded,
        "s" | "skip" => Answer::Skip,
        _ => Answer::Stop,
    }
}

/// Where one slice's plaintext goes.
pub(super) enum SliceSink<'a> {
    /// A file in scratch — the spooled restore.
    File(&'a Path),
    /// The named pipe `dar` reads this slice from — the streamed restore.
    Pipe {
        path: &'a Path,
        dar: &'a dar::restore::SequentialExtract,
    },
}

/// One slice's output, open across every attempt to fill it: the file or the
/// named pipe is opened once, and what an earlier cartridge delivered stays
/// in it while a later one supplies the rest.
pub(super) struct SliceOut<'a> {
    sink: SliceSink<'a>,
    file: Option<std::fs::File>,
    slice_number: i64,
    /// Plaintext bytes accounted for so far — authentic or a hole.
    delivered: u64,
    /// The spans of `delivered` that are holes. A hole is entered before it is
    /// written, so one dar interrupted is still on the books.
    holes: Vec<Range<u64>>,
    /// Zero bytes of holes actually written.
    hole_bytes: u64,
    /// The named pipe's reader is gone: dar stopped reading.
    gone: bool,
    /// Reads begun on this slice, from any cartridge. The first hashes no
    /// plaintext: when its ciphertext hash matches, the bytes are proven
    /// identical to what was written and nothing more can be asked of them.
    attempts: u32,
    /// A running sha256 of the plaintext written, kept for a streamed slice
    /// from its second read on: a slice that needs one is no longer on the
    /// clean path, and what went down the pipe cannot be read back. A spooled
    /// slice keeps none — its file is hashed from disk, when asked.
    plain: Option<Sha256>,
    /// Bytes went down the pipe before hashing began, so the whole slice's
    /// plaintext can never be hashed.
    unhashed: bool,
    /// The age file (header and nonce) that delivered this slice's first
    /// bytes. Every later read must be that same file: chunks authenticate
    /// under whatever key their own header unwraps, so a second file to the
    /// same recipient would otherwise mix in silently.
    bound: Option<Vec<u8>>,
}

impl<'a> SliceOut<'a> {
    pub(super) fn new(sink: SliceSink<'a>, slice_number: i64) -> Self {
        SliceOut {
            sink,
            file: None,
            slice_number,
            delivered: 0,
            holes: Vec::new(),
            hole_bytes: 0,
            gone: false,
            attempts: 0,
            plain: None,
            unhashed: false,
            bound: None,
        }
    }

    /// A read of this slice is starting: from the second on, a streamed
    /// slice's plaintext is hashed as it is written (or marked unhashable if
    /// bytes already went down the pipe without it).
    fn begin_attempt(&mut self) {
        self.attempts += 1;
        let streamed = matches!(self.sink, SliceSink::Pipe { .. });
        if self.attempts >= 2 && streamed && self.plain.is_none() && !self.unhashed {
            if self.delivered == 0 {
                self.plain = Some(Sha256::new());
            } else {
                self.unhashed = true;
            }
        }
    }

    /// Tie this slice to the age file whose header and nonce are `header`,
    /// unless it is already tied to one.
    fn bind(&mut self, header: &[u8]) {
        if self.bound.is_none() {
            self.bound = Some(header.to_vec());
        }
    }

    /// sha256 of the plaintext written so far (holes included, as zeros):
    /// the running hash of a streamed slice, or a spooled file read back from
    /// disk. `None` when it cannot be known — bytes went down the pipe
    /// before anything was hashing them.
    fn plain_digest(&self) -> Result<Option<[u8; 32]>> {
        if self.unhashed {
            return Ok(None);
        }
        match (&self.plain, &self.sink) {
            (Some(running), _) => Ok(Some(running.clone().finalize().into())),
            (None, SliceSink::File(path)) => {
                let mut file = std::fs::File::open(path)?;
                let mut hasher = Sha256::new();
                let mut buf = vec![0u8; 128 * 1024];
                loop {
                    let n = std::io::Read::read(&mut file, &mut buf)?;
                    if n == 0 {
                        break;
                    }
                    hasher.update(&buf[..n]);
                }
                Ok(Some(hasher.finalize().into()))
            }
            (None, SliceSink::Pipe { .. }) => Ok(None),
        }
    }

    /// Start a spooled slice over: what was written is not to be kept (it
    /// is not the slice the catalog expects), so no read of it counts.
    fn reset(&mut self) {
        self.discard();
        self.delivered = 0;
        self.holes.clear();
        self.hole_bytes = 0;
        self.plain = None;
        self.unhashed = false;
        self.bound = None;
    }

    fn open(&mut self) -> Result<()> {
        if self.file.is_some() {
            return Ok(());
        }
        let opened = match &self.sink {
            SliceSink::File(path) => std::fs::File::create(path).map_err(Into::into),
            SliceSink::Pipe { path, dar } => open_pipe_for_dar(path, dar, self.slice_number),
        };
        match opened {
            Ok(file) => {
                self.file = Some(file);
                Ok(())
            }
            Err(e) => {
                // dar exited before it opened the pipe.
                self.gone = matches!(e, TapectlError::Dar(_));
                Err(e)
            }
        }
    }

    fn write(&mut self, bytes: &[u8]) -> Result<()> {
        self.open()?;
        let file = self.file.as_mut().expect("opened");
        let slice_number = self.slice_number;
        let gone = &mut self.gone;
        file.write_all(bytes).map_err(|e| {
            if e.kind() == std::io::ErrorKind::BrokenPipe {
                *gone = true;
                TapectlError::Dar(format!(
                    "dar stopped reading slice {slice_number} (it gave up on the archive)"
                ))
            } else {
                TapectlError::Other(format!("slice {slice_number}: {e}"))
            }
        })?;
        if let Some(plain) = self.plain.as_mut() {
            plain.update(bytes);
        }
        self.delivered += bytes.len() as u64;
        Ok(())
    }

    /// A hole of `len` zero bytes: the place of a chunk that did not
    /// authenticate. Never anything else.
    ///
    /// The hole is recorded before it is written: dar, handed zeros where a
    /// slice's header should be, may give up and close the pipe before the
    /// last of them is in, and a slice dar stopped reading at its own hole
    /// is a slice with a hole in it (the restore tells that from dar dying
    /// of its own accord by the damage on the books).
    fn hole(&mut self, len: u64) -> Result<()> {
        self.open()?;
        let start = self.delivered;
        if len > 0 {
            self.holes.push(start..start + len);
        }
        let zeros = [0u8; 16 * 1024];
        let mut left = len;
        while left > 0 {
            let n = left.min(zeros.len() as u64) as usize;
            self.write(&zeros[..n])?;
            self.hole_bytes += n as u64;
            left -= n as u64;
        }
        Ok(())
    }

    fn finish(&mut self) -> Result<()> {
        self.open()?;
        self.file.as_mut().expect("opened").flush()?;
        Ok(())
    }

    /// Remove a spooled slice that is not to be kept.
    fn discard(&mut self) {
        self.file = None;
        if let SliceSink::File(path) = &self.sink {
            let _ = std::fs::remove_file(path);
        }
    }
}

/// How one slice read ended: how much of it is authentic, and where it is
/// not.
#[derive(Debug, Clone, Default)]
pub(super) struct SliceRead {
    /// Plaintext bytes written that authenticated.
    pub authentic_bytes: u64,
    /// dar stopped reading before the slice was all delivered (a streamed
    /// restore whose archive a hole made unreadable): no later slice can
    /// be of use.
    pub dar_gone: bool,
}

/// Why a slice could not be had from the cartridge it was read from.
#[derive(Debug)]
enum Defect {
    /// The tape would not read it.
    Read(TapectlError),
    /// What came off the tape does not hash to what was written.
    Hash { actual: String },
    /// A chunk failed authentication although the hash matched: the slice
    /// was bad when it was written, so every copy is.
    Chunk,
    /// This copy is a different age file (another header and nonce) from the
    /// part of the slice already delivered: chunks of two files are never
    /// mixed into one slice, however well each authenticates.
    OtherFile,
}

impl Defect {
    /// Whether another copy can help: a medium fault, not a slice that was
    /// bad before it was written.
    fn substitutable(&self) -> bool {
        !matches!(self, Defect::Chunk)
    }

    fn describe(&self, wp: &WritePositionInfo) -> String {
        match self {
            Defect::Read(e) => format!("slice {} could not be read: {e}", wp.slice_number),
            Defect::Hash { actual } => format!(
                "slice {} checksum mismatch on tape: expected {}..., got {}...",
                wp.slice_number,
                &wp.sha256_encrypted[..wp.sha256_encrypted.len().min(16)],
                &actual[..actual.len().min(16)],
            ),
            Defect::Chunk => format!(
                "slice {}: a chunk failed authentication under a matching checksum — the slice \
                 was already damaged when it was written",
                wp.slice_number
            ),
            Defect::OtherFile => format!(
                "slice {} on this copy is a different age file (another header and nonce) from \
                 the part of the slice already read, so chunks of the two are never mixed",
                wp.slice_number
            ),
        }
    }
}

/// One slice supplied from a cartridge other than the one it was meant to
/// come from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Substitution {
    pub slice_number: i64,
    /// The volume the slice was read from instead.
    pub supplied_by: String,
    /// The volume it was damaged on.
    pub damaged_on: String,
    pub reason: String,
}

/// A slice the restore could not prove whole: some of its chunks written as
/// holes, or every chunk authentic but the slice not shown to be the one the
/// catalog recorded. Either way the files in it are checked one by one
/// against their recorded checksums before the restore is called whole.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Damage {
    pub slice_number: i64,
    pub volume: String,
    pub reason: String,
    /// Plaintext byte ranges of the slice, written as zeros. Empty for a
    /// slice that is only unproven.
    pub holes: Vec<Range<u64>>,
}

impl Damage {
    /// One line for the restore's account of what it could not vouch for.
    pub(crate) fn describe(&self) -> String {
        if self.holes.is_empty() {
            return format!(
                "slice {} on {}: unproven, not shown to be the slice the catalog recorded — {}",
                self.slice_number, self.volume, self.reason
            );
        }
        let bytes: u64 = self.holes.iter().map(|r| r.end - r.start).sum();
        let ranges: Vec<String> = self
            .holes
            .iter()
            .take(8)
            .map(|r| format!("{}..{}", r.start, r.end))
            .collect();
        format!(
            "slice {} on {}: {} hole(s) of zeros, {bytes} byte(s) in all, at plaintext byte(s) \
             {}{} — {}; the slice as a whole is unproven, since a copy with holes cannot be \
             checked against its recorded checksum",
            self.slice_number,
            self.volume,
            self.holes.len(),
            ranges.join(", "),
            if self.holes.len() > ranges.len() {
                ", ..."
            } else {
                ""
            },
            self.reason
        )
    }
}

/// One cartridge's stay in the drive during a restore.
struct Presence<'c, 'a> {
    volume_id: Option<i64>,
    label: String,
    /// `None` once the contact is closed.
    guard: Option<ContactGuard<'c>>,
    site: PresenceSite<'c, 'a>,
    /// What happened to slices here, for the contact's closing detail.
    notes: Vec<String>,
}

enum PresenceSite<'c, 'a> {
    /// The caller's: the restore's first cartridge.
    Given(ContactSite<'a>),
    /// Opened here, for a cartridge that was swapped in.
    Arrived(Box<Arrival<'c, 'a>>),
}

impl PresenceSite<'_, '_> {
    fn with<R>(
        &self,
        config: &Config,
        operation: Operation,
        device: &str,
        f: impl FnOnce(&ContactSite<'_>) -> R,
    ) -> R {
        match self {
            PresenceSite::Given(site) => f(site),
            PresenceSite::Arrived(arrival) => arrival.with_site(config, operation, device, f),
        }
    }
}

impl Presence<'_, '_> {
    /// Close the contact and take the cartridge's one health reading. The
    /// reading is taken with the cartridge still in the drive.
    fn close(
        &mut self,
        conn: &Connection,
        config: &Config,
        operation: Operation,
        device: &str,
        outcome: Option<&TapectlError>,
    ) -> Option<i64> {
        let guard = self.guard.take()?;
        let contact_id = guard.id();
        let detail = detail_of(&self.notes, outcome);
        guard.finish(
            match outcome {
                None => contact::OUTCOME_OK,
                Some(_) => contact::OUTCOME_FAILED,
            },
            detail.as_deref(),
        );
        self.site.with(config, operation, device, |site| {
            let phase = progress::phase("health-sweep", None);
            crate::volume::write::health_after_read_contact(conn, site, self.volume_id, contact_id);
            phase.done();
        });
        contact_id
    }
}

fn detail_of(notes: &[String], outcome: Option<&TapectlError>) -> Option<String> {
    let mut parts: Vec<String> = Vec::new();
    if let Some(e) = outcome {
        parts.push(e.to_string());
    }
    parts.extend(notes.iter().cloned());
    (!parts.is_empty()).then(|| parts.join("; "))
}

/// Where the next read of a slice is to come from.
enum Choice {
    /// The cartridge in the drive, at this tape file.
    Here(u32),
    /// Another cartridge, which has to be asked for.
    Elsewhere(Holder),
    /// No cartridge the catalog knows of that has not already failed.
    Nowhere,
}

/// How a swap ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Changed {
    /// The cartridge asked for is in the drive and passed its identity check.
    Loaded,
    /// The operator said to go on without it; the cartridge that was in the
    /// drive is back in it.
    Declined,
}

/// What a slice's plaintext shows when its ciphertext hash could not.
///
/// Every chunk authenticating proves only that each was encrypted under some
/// key this identity unwraps: a different age file to the same recipient
/// does that in full. The recorded `sha256_plain` is what binds the bytes to
/// the slice that was written.
#[derive(Debug, PartialEq, Eq)]
enum Proof {
    /// The plaintext hashes to the recorded checksum.
    Whole,
    /// It hashes to something else: authentic bytes, but not this slice.
    Wrong,
    /// Neither can be said, and why.
    Unproven(&'static str),
}

/// Test the plaintext written to `out` against `wp`'s recorded checksum.
fn prove_plaintext(wp: &WritePositionInfo, out: &SliceOut<'_>) -> Result<Proof> {
    let Ok(recorded) = crate::db::files::sha256_from_hex(&wp.sha256_plain) else {
        return Ok(Proof::Unproven(
            "no plaintext checksum is recorded for the slice",
        ));
    };
    Ok(match out.plain_digest()? {
        Some(got) if got == recorded => Proof::Whole,
        Some(_) => Proof::Wrong,
        None => Proof::Unproven(
            "its plaintext was streamed to dar before anything was hashing it (a spooled \
             restore, --spool, can check it)",
        ),
    })
}

/// What the decrypt thread of one attempt ended on.
struct Ended {
    /// The sink's reader is gone (dar exited): nothing more can be written.
    sink_gone: bool,
    /// Strict mode stopped at a chunk that did not authenticate.
    stopped: bool,
    /// The file is a different age file from the one that delivered this
    /// slice's first bytes: nothing of it was written.
    other_file: bool,
    /// The file could not be opened as an age file for these identities.
    open_error: Option<TapectlError>,
}

/// One attempt at a slice, classified.
enum Attempt {
    /// Every chunk the sink still lacked authenticated and was written.
    Complete,
    /// dar stopped reading: the slice cannot be delivered any further.
    SinkGone,
    /// It stopped, and why.
    Stopped(Defect),
    /// Every chunk the sink lacked authenticated and was written, but the
    /// copy does not hash and the plaintext could not be shown to be the
    /// slice (and why). The slice is delivered in full; only a copy that
    /// hashes as written could still prove it.
    Unproven(Defect, &'static str),
}

/// The restore's reads of slices: the cartridge in the drive, what it has
/// supplied, and what to do when it cannot supply one.
pub(crate) struct Supply<'c, 's, 'b> {
    conn: &'c Connection,
    config: &'s Config,
    bay: &'b mut dyn Bay,
    operation: Operation,
    device: String,
    /// `None` only between a cartridge leaving and the next arriving.
    here: Option<Presence<'c, 's>>,
    /// Volumes found bad for a slice (by its hash), and how much of the slice
    /// had been delivered when they were: a volume is asked again only once
    /// another has delivered past that.
    bad: HashMap<(i64, String), u64>,
    /// Volumes whose copy of a slice did not hash as written. Such a copy can
    /// never prove a delivered slice whole, which is all a slice already
    /// delivered in full is waiting for.
    hash_bad: std::collections::HashSet<(i64, String)>,
    /// The tape file the last attempt read on `here`, for a salvage pass.
    last_position: Option<u32>,
    /// Whether a stage set has no other copy anywhere, per cartridge.
    sole: HashMap<(i64, i64), bool>,
    /// What is being restored, for the prompt and the refusal.
    subject: String,
    /// File 0's head, read to identify the loaded volume, for the first
    /// contact check.
    primed: Option<Vec<u8>>,
    pub(crate) substitutions: Vec<Substitution>,
    pub(crate) damage: Vec<Damage>,
}

impl<'c, 's, 'b> Supply<'c, 's, 'b> {
    /// A supply that starts on the cartridge the caller has opened a contact
    /// for.
    pub(crate) fn start(
        conn: &'c Connection,
        bay: &'b mut dyn Bay,
        guard: ContactGuard<'c>,
        volume_id: Option<i64>,
        label: &str,
        site: ContactSite<'s>,
    ) -> Self {
        let operation = site.operation();
        let device = site.device().to_string();
        Supply {
            conn,
            config: site.config(),
            bay,
            operation,
            device,
            here: Some(Presence {
                volume_id,
                label: label.to_string(),
                guard: Some(guard),
                site: PresenceSite::Given(site),
                notes: Vec::new(),
            }),
            bad: HashMap::new(),
            hash_bad: std::collections::HashSet::new(),
            last_position: None,
            sole: HashMap::new(),
            subject: String::new(),
            primed: None,
            substitutions: Vec::new(),
            damage: Vec::new(),
        }
    }

    /// Hand over File 0's head, when the loaded-tape path has read it.
    pub(crate) fn prime(&mut self, file0: Option<Vec<u8>>) {
        self.primed = file0;
    }

    /// The restore's first contact check, on the cartridge it started on
    /// ([`super::corroborate_at_contact`]): a wrong tape is refused before
    /// a scratch directory is made or a key loaded.
    pub(crate) fn corroborate_first(&mut self, label: &str) -> Result<()> {
        let serial = self.here.as_ref().and_then(|p| {
            p.site
                .with(self.config, self.operation, &self.device, |site| {
                    site.medium_serial().map(str::to_string)
                })
        });
        match self.primed.take() {
            Some(bytes) => {
                let mut peeked = crate::volume::loaded::Peeked::primed(self.bay.store(), bytes);
                super::corroborate_at_contact(self.conn, label, &mut peeked, serial.as_deref())
            }
            None => {
                super::corroborate_at_contact(self.conn, label, self.bay.store(), serial.as_deref())
            }
        }
    }

    /// Name what is being restored, for the prompt: `unit "photos" v2`.
    pub(crate) fn set_subject(&mut self, subject: impl Into<String>) {
        self.subject = subject.into();
    }

    /// Whether a person can be asked to change the cartridge.
    pub(crate) fn attended(&self) -> bool {
        self.bay.attended()
    }

    pub(crate) fn here_volume(&self) -> Option<i64> {
        self.here.as_ref().and_then(|p| p.volume_id)
    }

    pub(crate) fn here_label(&self) -> Option<String> {
        self.here.as_ref().map(|p| p.label.clone())
    }

    /// Record `note` against the cartridge in the drive.
    pub(crate) fn note_here(&mut self, note: String) {
        if let Some(p) = self.here.as_mut() {
            p.notes.push(note);
        }
    }

    /// Close the last cartridge's contact with the restore's outcome, and
    /// take its health reading.
    pub(crate) fn finish(mut self, outcome: Option<&TapectlError>) {
        if let Some(mut p) = self.here.take() {
            p.close(
                self.conn,
                self.config,
                self.operation,
                &self.device,
                outcome,
            );
        }
    }

    // ── reading a slice ───────────────────────────────────────────────

    /// Read slice `wp` into `sink`, from the cartridge in the drive or, if
    /// that cannot give it whole, from another that holds it, and as a last
    /// resort with holes for the chunks nobody can give. See the module doc.
    pub(super) fn read_slice(
        &mut self,
        wp: &WritePositionInfo,
        identities: &[age::x25519::Identity],
        sink: SliceSink<'_>,
    ) -> Result<SliceRead> {
        let mut out = SliceOut::new(sink, wp.slice_number);
        match self.fill_slice(wp, identities, &mut out) {
            Ok(()) => {
                out.finish()?;
                Ok(SliceRead {
                    authentic_bytes: out.delivered - out.hole_bytes,
                    dar_gone: out.gone,
                })
            }
            Err(e) => {
                out.discard();
                Err(e)
            }
        }
    }

    /// Whether no other live volume holds any slice of `wp`'s stage set, so
    /// that a damaged slice has nowhere else to come from. Asked once per
    /// stage set and cartridge, with the catalog and never the tape.
    fn sole_copy(&mut self, wp: &WritePositionInfo) -> Result<bool> {
        let Some(volume) = self.here_volume() else {
            return Ok(true);
        };
        if let Some(&sole) = self.sole.get(&(wp.stage_set_id, volume)) {
            return Ok(sole);
        }
        let sole = !slice_holders::other_copies_exist(self.conn, wp.stage_set_id, volume)?;
        self.sole.insert((wp.stage_set_id, volume), sole);
        Ok(sole)
    }

    fn fill_slice(
        &mut self,
        wp: &WritePositionInfo,
        identities: &[age::x25519::Identity],
        out: &mut SliceOut<'_>,
    ) -> Result<()> {
        let mut holders: Option<Vec<Holder>> = None;
        // The first cartridge it failed on and why: what a substitution, a
        // refusal or a hole is described by.
        let mut first: Option<(String, Defect)> = None;
        // The slice is delivered in full and nothing has shown it to be the
        // slice: where, and why. Only a copy that hashes as written can still
        // settle it.
        let mut unproven: Option<(String, String)> = None;
        let mut tried = false;
        // With no other copy anywhere there is nothing to wait for: one pass,
        // holes where the chunks do not authenticate.
        let sole = self.sole_copy(wp)?;
        loop {
            match self.choose(wp, out.delivered, tried, unproven.is_some(), &mut holders)? {
                Choice::Here(position) => {
                    tried = true;
                    self.last_position = Some(position);
                    if sole {
                        break;
                    }
                    let attempt = self.attempt(position, wp, identities, out)?;
                    let why_unproven = match &attempt {
                        Attempt::Unproven(_, why) => Some(*why),
                        _ => None,
                    };
                    match attempt {
                        Attempt::Complete => {
                            if let Some((damaged_on, defect)) = &first {
                                self.supplied(wp, damaged_on, defect);
                            }
                            return Ok(());
                        }
                        Attempt::SinkGone => return Ok(()),
                        Attempt::Stopped(defect) | Attempt::Unproven(defect, _) => {
                            let label = self.here_label().unwrap_or_default();
                            warn!(
                                slice = wp.slice_number,
                                volume = %label,
                                delivered = out.delivered,
                                "{}",
                                defect.describe(wp)
                            );
                            progress::log(&format!("{label}: {}", defect.describe(wp)));
                            self.note_here(format!(
                                "slice {} damaged here ({})",
                                wp.slice_number,
                                short_cause(&defect)
                            ));
                            self.mark_bad(&wp.sha256_encrypted, out.delivered);
                            if matches!(defect, Defect::Hash { .. }) {
                                if let Some(volume) = self.here_volume() {
                                    self.hash_bad.insert((volume, wp.sha256_encrypted.clone()));
                                }
                            }
                            if let Some(why) = why_unproven {
                                unproven = Some((
                                    label.clone(),
                                    format!(
                                        "{label}: {}; every chunk authenticates, but {why}",
                                        defect.describe(wp)
                                    ),
                                ));
                            }
                            let substitutable = defect.substitutable();
                            first.get_or_insert((label, defect));
                            if !substitutable {
                                // Identical on every copy: another cartridge cannot help.
                                break;
                            }
                        }
                    }
                }
                Choice::Elsewhere(holder) => {
                    if !self.bay.attended() {
                        let list = holders.clone().unwrap_or_default();
                        return Err(self.refuse_unattended(wp, first.as_ref(), &list));
                    }
                    let why = self.why_wanted(wp, first.as_ref());
                    match self.change_to(
                        holder.volume_id,
                        &holder.label,
                        holder.standing == Standing::Quarantined,
                        why,
                        "an identical copy of it",
                        "leave a hole where the slice is damaged",
                    )? {
                        Changed::Loaded => {}
                        Changed::Declined => break,
                    }
                }
                Choice::Nowhere => break,
            }
        }
        if let Some((on, reason)) = unproven {
            // Already delivered in full: reading it again could only say the
            // same.
            self.record_damage(wp, on, reason, Vec::new());
            return Ok(());
        }
        self.salvage(wp, identities, out, first)
    }

    /// Nothing can give the rest of the slice whole: read it from the
    /// cartridge in the drive with a hole of zeros for every chunk that does
    /// not authenticate, and record the damage.
    ///
    /// The chunks that do authenticate are written, whatever the copy's
    /// checksum says: each is proven genuine by its own tag, and a hash can
    /// only say that something, somewhere, differs. A tag does not say which
    /// age file a chunk is of: chunks of another file than the one that
    /// began the slice are refused, and a slice every chunk of which
    /// authenticates under a checksum that does not match is whole only if
    /// its plaintext matches the checksum recorded for it. Holes, or no way
    /// to hash the plaintext, leave the slice unproven, and it is said so.
    fn salvage(
        &mut self,
        wp: &WritePositionInfo,
        identities: &[age::x25519::Identity],
        out: &mut SliceOut<'_>,
        first: Option<(String, Defect)>,
    ) -> Result<()> {
        let Some(position) = self.last_position else {
            return Err(TapectlError::Other(format!(
                "slice {} of {} is held by no cartridge the catalog knows of that has not \
                 already failed it",
                wp.slice_number, self.subject
            )));
        };
        let before_holes = out.holes.len();
        let mut raw = self.attempt_raw(position, wp, identities, out, Mode::Tolerant)?;
        let label = self.here_label().unwrap_or_default();
        let hash_ok = raw.hash_ok;
        let here_defect = raw.defect();
        let described = |cause: &Option<(String, Defect)>| {
            cause
                .as_ref()
                .map(|(on, d)| format!("{on}: {}", d.describe(wp)))
        };
        let cause: Option<(String, Defect)> =
            first.or_else(|| here_defect.map(|d| (label.clone(), d)));
        let ended = match raw.ended {
            Ok(ended) => ended,
            Err(sink) => {
                return Err(match described(&cause) {
                    Some(d) => TapectlError::Other(format!("{d}; {sink}")),
                    None => sink,
                })
            }
        };
        if let Some(e) = ended.open_error {
            return Err(TapectlError::Other(format!(
                "{}; and it cannot be read around: {e}",
                described(&cause)
                    .unwrap_or_else(|| format!("slice {} cannot be read", wp.slice_number)),
            )));
        }
        if ended.other_file {
            // The cartridge in the drive is not the file the slice was begun
            // from. One that hashes as recorded is the slice, and a spooled
            // part that came from elsewhere is dropped for it.
            if hash_ok && matches!(out.sink, SliceSink::File(_)) {
                out.reset();
                return self.salvage(wp, identities, out, cause);
            }
            return Err(self.other_file_error(wp, hash_ok, described(&cause)));
        }
        let new_holes: Vec<Range<u64>> = out.holes[before_holes..].to_vec();
        if ended.sink_gone && new_holes.is_empty() {
            // dar stopped reading mid-slice. Over a slice that hashes as
            // written that is dar's own doing (`read_slice` says dar is gone,
            // and the restore says why). Over one that does not, the slice is
            // why, and it goes on the books, as a hole would.
            if !hash_ok {
                let reason = format!(
                    "{}; dar stopped reading it",
                    described(&cause).unwrap_or_else(|| format!("slice {}", wp.slice_number)),
                );
                self.record_damage(wp, label, reason, Vec::new());
            }
            return Ok(());
        }
        if new_holes.is_empty() {
            // Every chunk authenticated after all: chunks other cartridges
            // gave, and this one's own. A copy that hashes as written is the
            // slice; one that does not is the slice only if its plaintext
            // says so.
            let proof = if hash_ok {
                Proof::Whole
            } else {
                prove_plaintext(wp, out)?
            };
            match proof {
                Proof::Whole => {
                    if let Some((on, d)) = &cause {
                        self.supplied(wp, on, d);
                    }
                    return Ok(());
                }
                Proof::Wrong => {
                    if matches!(out.sink, SliceSink::Pipe { .. }) {
                        return Err(self.wrong_in_stream(wp));
                    }
                    // Authentic bytes that are not this slice are no use to
                    // dar: the slice is a hole from end to end.
                    let len = out.delivered;
                    out.reset();
                    out.hole(len)?;
                    let reason = format!(
                        "{}; every chunk authenticates, but the plaintext does not match the \
                         checksum recorded for the slice — it is not the slice the catalog \
                         expects",
                        described(&cause).unwrap_or_else(|| format!("slice {}", wp.slice_number)),
                    );
                    return Ok(self.record_damage(wp, label, reason, out.holes.clone()));
                }
                Proof::Unproven(why) => {
                    let reason = format!(
                        "{}; every chunk authenticates, but {why}",
                        described(&cause).unwrap_or_else(|| format!("slice {}", wp.slice_number)),
                    );
                    return Ok(self.record_damage(wp, label, reason, Vec::new()));
                }
            }
        }
        let reason = described(&cause).unwrap_or_else(|| Defect::Chunk.describe(wp));
        let lost: u64 = new_holes.iter().map(|h| h.end - h.start).sum();
        warn!(
            slice = wp.slice_number,
            volume = %label,
            holes = new_holes.len(),
            bytes = lost,
            "no cartridge can give this slice whole: its damaged chunks are written as holes"
        );
        progress::log(&format!(
            "slice {}: {} hole(s), {lost} byte(s) of zeros where no copy authenticates",
            wp.slice_number,
            new_holes.len()
        ));
        self.record_damage(wp, label, reason, new_holes);
        Ok(())
    }

    /// Put a slice that could not be proven whole on the restore's account.
    fn record_damage(
        &mut self,
        wp: &WritePositionInfo,
        volume: String,
        reason: String,
        holes: Vec<Range<u64>>,
    ) {
        if holes.is_empty() {
            warn!(
                slice = wp.slice_number,
                volume = %volume,
                "{reason}: the slice is unproven, and the files in it are checked one by one"
            );
            progress::log(&format!("slice {}: unproven — {reason}", wp.slice_number));
            self.note_here(format!("slice {} unproven", wp.slice_number));
        } else {
            self.note_here(format!(
                "slice {} written with {} hole(s)",
                wp.slice_number,
                holes.len()
            ));
        }
        self.damage.push(Damage {
            slice_number: wp.slice_number,
            volume,
            reason,
            holes,
        });
    }

    /// A slice was completed by a cartridge other than the one it first
    /// failed on.
    fn supplied(&mut self, wp: &WritePositionInfo, damaged_on: &str, defect: &Defect) {
        let supplier = self.here_label().unwrap_or_default();
        if supplier == damaged_on {
            // Whole from the one cartridge: nothing was substituted.
            return;
        }
        info!(
            slice = wp.slice_number,
            from = %supplier,
            damaged_on = %damaged_on,
            "slice read from another copy"
        );
        progress::log(&format!(
            "slice {} of {} read from {supplier}, a copy of the one damaged on {damaged_on}",
            wp.slice_number, self.subject
        ));
        self.note_here(format!(
            "supplied slice {} of {} for {damaged_on}",
            wp.slice_number, self.subject
        ));
        self.substitutions.push(Substitution {
            slice_number: wp.slice_number,
            supplied_by: supplier,
            damaged_on: damaged_on.to_string(),
            reason: short_cause(defect),
        });
    }

    fn mark_bad(&mut self, sha: &str, delivered: u64) {
        if let Some(volume) = self.here_volume() {
            self.bad.insert((volume, sha.to_string()), delivered);
        }
    }

    /// Where the next read of `wp` comes from.
    fn choose(
        &mut self,
        wp: &WritePositionInfo,
        delivered: u64,
        tried: bool,
        awaiting_proof: bool,
        holders: &mut Option<Vec<Holder>>,
    ) -> Result<Choice> {
        let sha = &wp.sha256_encrypted;
        // A volume is usable for a slice until it failed it, and again once
        // another has delivered past where it failed. A slice already
        // delivered in full awaits only a copy that hashes as written, which
        // a volume that did not hash cannot be.
        let hash_bad = &self.hash_bad;
        let usable = |bad: &HashMap<(i64, String), u64>, volume: i64| {
            bad.get(&(volume, sha.clone()))
                .is_none_or(|&at| delivered > at)
                && !(awaiting_proof && hash_bad.contains(&(volume, sha.clone())))
        };
        let parse = |position: &str| -> Result<u32> {
            position.parse().map_err(|_| {
                TapectlError::Other(format!(
                    "slice {} has no tape position the catalog can read (\"{position}\")",
                    wp.slice_number
                ))
            })
        };
        let Some(here) = self.here_volume() else {
            // A cartridge the catalog has no row for: the slice's own
            // position, once, and nothing to substitute from.
            return Ok(if tried {
                Choice::Nowhere
            } else {
                Choice::Here(parse(&wp.position)?)
            });
        };
        // The cartridge in the drive is the one this position is for: no
        // question for the catalog (a clean restore asks none).
        if here == wp.volume_id && usable(&self.bad, here) {
            return Ok(Choice::Here(parse(&wp.position)?));
        }
        if holders.is_none() {
            *holders = Some(slice_holders::holders_of(self.conn, sha)?);
        }
        let all = holders.as_ref().expect("just loaded");
        if let Some(h) = all.iter().find(|h| h.volume_id == here) {
            if usable(&self.bad, here) {
                return Ok(Choice::Here(h.position));
            }
        }
        Ok(
            match all
                .iter()
                .find(|h| h.volume_id != here && usable(&self.bad, h.volume_id))
            {
                Some(h) => Choice::Elsewhere(h.clone()),
                None => Choice::Nowhere,
            },
        )
    }

    /// Why a cartridge is being asked for, for the prompt.
    fn why_wanted(&self, wp: &WritePositionInfo, first: Option<&(String, Defect)>) -> String {
        match first {
            Some((on, d)) => format!(
                "Slice {} of {} is damaged on {on}: {}.",
                wp.slice_number,
                self.subject,
                d.describe(wp)
            ),
            None => format!(
                "Slice {} of {} is not held by {}.",
                wp.slice_number,
                self.subject,
                self.here_label().unwrap_or_else(|| "this cartridge".into())
            ),
        }
    }

    /// The refusal when another cartridge could give the slice and nobody is
    /// at the terminal to load it.
    fn refuse_unattended(
        &self,
        wp: &WritePositionInfo,
        first: Option<&(String, Defect)>,
        holders: &[Holder],
    ) -> TapectlError {
        let on = self.here_label().unwrap_or_else(|| "the cartridge".into());
        let cause = match first {
            Some((_, d)) => d.describe(wp),
            None => format!("slice {} is not on {on}", wp.slice_number),
        };
        let others: Vec<&Holder> = holders
            .iter()
            .filter(|h| Some(h.volume_id) != self.here_volume())
            .collect();
        let holding: Vec<String> = others
            .iter()
            .map(|h| {
                let shelf = slice_holders::shelf_of(self.conn, h.volume_id, &h.label);
                format!(
                    "{} ({}{})",
                    h.label,
                    shelf.describe(),
                    if h.standing == Standing::Quarantined {
                        "; quarantined, a last resort"
                    } else {
                        ""
                    }
                )
            })
            .collect();
        let first_label = others.first().map(|h| h.label.as_str()).unwrap_or_default();
        TapectlError::Other(format!(
            "slice {} of {} cannot be read from {on} ({cause}), and an identical copy of it is \
             held by {}. tapectl swaps no cartridge without a person at the terminal, and \
             --yes cannot load one: run it from a terminal and it will ask for that cartridge, \
             or restore from it directly (--from {first_label}). Nothing further was written; \
             what was restored before this slice stays where it is",
            wp.slice_number,
            self.subject,
            holding.join(", "),
        ))
    }

    // ── one attempt ───────────────────────────────────────────────────

    /// One strict read of `wp` from the cartridge in the drive, at tape file
    /// `position`, delivering what `out` still lacks.
    fn attempt(
        &mut self,
        position: u32,
        wp: &WritePositionInfo,
        identities: &[age::x25519::Identity],
        out: &mut SliceOut<'_>,
    ) -> Result<Attempt> {
        let delivered_before = out.delivered;
        let mut raw = self.attempt_raw(position, wp, identities, out, Mode::Strict)?;
        // The cause, by the evidence there is: the tape would not read; what
        // it gave does not hash; it hashes and a chunk is bad anyway.
        let hash_ok = raw.hash_ok;
        let defect = raw.defect();
        let ended = match raw.ended {
            Ok(ended) => ended,
            // The sink failed (dar stopped, the disk): not the medium.
            Err(sink) => {
                return Err(match defect {
                    Some(d) => TapectlError::Other(format!("{}; {sink}", d.describe(wp))),
                    None => sink,
                })
            }
        };
        if let Some(e) = ended.open_error {
            return match defect {
                Some(d) => Ok(Attempt::Stopped(d)),
                // The hash matched, so this is not the medium: the keys, or a
                // file that is not age.
                None => Err(e),
            };
        }
        if ended.sink_gone {
            // dar stopped reading, so nothing more can be delivered, from this
            // copy or any other. Over a slice that hashes as written that is
            // dar's own doing; over one that does not, the slice is why, and
            // it goes on the books as unproven.
            if !hash_ok {
                let on = self.here_label().unwrap_or_default();
                let reason = format!(
                    "{on}: {}; dar stopped reading it, so no other copy could be tried (a \
                     spooled restore, --spool, can)",
                    defect.map_or_else(|| format!("slice {}", wp.slice_number), |d| d.describe(wp)),
                );
                self.record_damage(wp, on, reason, Vec::new());
            }
            return Ok(Attempt::SinkGone);
        }
        if ended.other_file {
            if hash_ok {
                // This copy hashes as the catalog recorded: it IS the slice,
                // and the file the part already read came from is not.
                if matches!(out.sink, SliceSink::File(_)) {
                    // Spooled: drop that part and read this copy afresh.
                    out.reset();
                    return self.attempt(position, wp, identities, out);
                }
                return Err(self.other_file_error(wp, true, None));
            }
            return Ok(Attempt::Stopped(Defect::OtherFile));
        }
        if ended.stopped {
            return Ok(Attempt::Stopped(defect.unwrap_or(Defect::Chunk)));
        }
        // Every chunk the sink lacked authenticated. A ciphertext that
        // hashes as written is the slice, and the part an earlier read gave
        // is that same file (the header was checked), so it is whole.
        if hash_ok {
            return Ok(Attempt::Complete);
        }
        let Some(defect) = defect else {
            return Ok(Attempt::Complete);
        };
        // It does not hash, and a chunk's tag says nothing of WHICH age file
        // this is: only the plaintext, against the checksum recorded for the
        // slice, can say the bytes written are the slice.
        match prove_plaintext(wp, out)? {
            Proof::Whole => {
                // The copy's bad bytes lay in what another copy had already
                // given (or in the header's unauthenticated margin): the
                // plaintext is whole, and shown to be.
                info!(
                    slice = wp.slice_number,
                    delivered_before,
                    "{} — every chunk still needed authenticated and the plaintext matches the \
                     recorded checksum, so the slice is whole",
                    defect.describe(wp)
                );
                Ok(Attempt::Complete)
            }
            Proof::Wrong => {
                warn!(
                    slice = wp.slice_number,
                    "{} — every chunk authenticates but the plaintext is not the slice the \
                     catalog recorded",
                    defect.describe(wp)
                );
                if matches!(out.sink, SliceSink::Pipe { .. }) {
                    return Err(self.wrong_in_stream(wp));
                }
                // A spooled slice that is not the slice is not kept: another
                // copy is read afresh.
                out.reset();
                Ok(Attempt::Stopped(defect))
            }
            Proof::Unproven(why) => {
                info!(
                    slice = wp.slice_number,
                    "{} — every chunk authenticates but {why}",
                    defect.describe(wp)
                );
                Ok(Attempt::Unproven(defect, why))
            }
        }
    }

    /// The refusal when the cartridge in the drive holds another age file
    /// than the one that began the slice. `matches_catalog`: this copy's own
    /// checksum is the recorded one, so it is the slice and the part already
    /// read is not.
    fn other_file_error(
        &self,
        wp: &WritePositionInfo,
        matches_catalog: bool,
        cause: Option<String>,
    ) -> TapectlError {
        let verdict = if matches_catalog {
            "this copy's checksum matches the catalog, so the part already read is not the slice"
        } else {
            "neither can be shown to be the slice"
        };
        TapectlError::Other(format!(
            "slice {} of {} cannot be completed: the cartridge in the drive holds a different \
             age file for it (another header and nonce) from the part already read, and chunks \
             of two files are never mixed — {verdict}{}. A streamed restore cannot take back \
             what it has fed to dar: restore again with --spool, or from another cartridge",
            wp.slice_number,
            self.subject,
            cause.map(|d| format!(" ({d})")).unwrap_or_default(),
        ))
    }

    /// The refusal for a streamed slice whose plaintext is not the slice:
    /// the bytes are in dar's input and cannot be taken back.
    fn wrong_in_stream(&self, wp: &WritePositionInfo) -> TapectlError {
        let on = self.here_label().unwrap_or_else(|| "the cartridge".into());
        TapectlError::Other(format!(
            "slice {} of {} on {on}: every chunk authenticates, but the plaintext does not match \
             the checksum recorded for the slice, so this is not the slice the catalog expects \
             (a wrong file at this tape position, or a wrong catalog). It was already streamed \
             into dar, which cannot take it back: restore again with --spool, which can try \
             another copy of the slice, or from another cartridge",
            wp.slice_number, self.subject
        ))
    }

    fn attempt_raw(
        &mut self,
        position: u32,
        wp: &WritePositionInfo,
        identities: &[age::x25519::Identity],
        out: &mut SliceOut<'_>,
        mode: Mode,
    ) -> Result<Raw> {
        out.begin_attempt();
        let (pipe_reader, pipe_writer) = std::io::pipe()?;
        let encrypted = wp.encrypted_bytes.max(0) as u64;
        let store = self.bay.store();
        let out_ref = &mut *out;
        let (read, actual_hash, ended) = std::thread::scope(|s| {
            let decrypt =
                s.spawn(move || decrypt_chunks(pipe_reader, identities, encrypted, mode, out_ref));
            let mut bounded =
                TruncatingWriter::new(HashingWriter::new(Detachable::new(pipe_writer)), encrypted);
            let read = store.read_file(position, &mut progress::CountingWriter(&mut bounded));
            let hashing = bounded.into_inner();
            let actual_hash = hashing.finalize_hex();
            // Closes the pipe: the decrypt thread sees the end of the slice.
            drop(hashing);
            (read, actual_hash, decrypt.join())
        });
        let ended = ended.map_err(|_| {
            TapectlError::Other(format!(
                "slice {}: the decrypt thread panicked",
                wp.slice_number
            ))
        })?;
        Ok(Raw {
            hash_ok: actual_hash == wp.sha256_encrypted,
            actual_hash,
            read: Some(read),
            ended,
        })
    }

    // ── changing the cartridge ───────────────────────────────────────

    /// Make sure the cartridge `label` is in the drive, asking for it if it
    /// is not (the fill of item 102 reads whole stage sets off it).
    pub(crate) fn ensure_loaded(
        &mut self,
        volume_id: i64,
        label: &str,
        quarantined: bool,
        why: String,
        what: &str,
        skip_means: &'static str,
    ) -> Result<Changed> {
        if self.here_volume() == Some(volume_id) {
            return Ok(Changed::Loaded);
        }
        self.change_to(volume_id, label, quarantined, why, what, skip_means)
    }

    /// Ask for `label`'s cartridge: close the one in the drive (its contact,
    /// its health reading), release the device, ask, and on "loaded" open
    /// the drive on what is there and check it is the cartridge asked for.
    fn change_to(
        &mut self,
        volume_id: i64,
        label: &str,
        quarantined: bool,
        why: String,
        what: &str,
        skip_means: &'static str,
    ) -> Result<Changed> {
        let previous = self.here.as_ref().map(|p| (p.volume_id, p.label.clone()));
        let saved_position = self.last_position;
        self.leave();
        let shelf = slice_holders::shelf_of(self.conn, volume_id, label);
        let want = Want {
            why,
            holds: holds_line(label, &shelf, what, quarantined),
            label: label.to_string(),
            device: self.device.clone(),
            skip_means,
        };
        loop {
            match self.bay.ask(&want) {
                Answer::Loaded => match self.arrive(Some(volume_id), label) {
                    Ok(()) => return Ok(Changed::Loaded),
                    Err(e) => {
                        // Not the cartridge asked for (or one this drive
                        // cannot read): say so, and ask again.
                        progress::stderr_println(&format!("{e}"));
                        self.bay.release();
                    }
                },
                Answer::Skip => {
                    let Some((pv, pl)) = previous else {
                        return Err(TapectlError::Other(format!(
                            "restore of {} stopped: no cartridge to go on with",
                            self.subject
                        )));
                    };
                    self.return_to(pv, &pl)?;
                    self.last_position = saved_position;
                    return Ok(Changed::Declined);
                }
                Answer::Stop => {
                    return Err(TapectlError::Other(format!(
                        "restore of {} stopped at the operator's request while waiting for \
                         {label}. What was restored before this point stays where it is",
                        self.subject
                    )))
                }
            }
        }
    }

    /// Go on without the cartridge asked for: the one that was in the drive
    /// is opened again. If something else is in it by now (a cartridge that
    /// was tried and refused), it is asked for back.
    fn return_to(&mut self, volume_id: Option<i64>, label: &str) -> Result<()> {
        loop {
            let e = match self.arrive(volume_id, label) {
                Ok(()) => return Ok(()),
                Err(e) => e,
            };
            progress::stderr_println(&format!("{e}"));
            self.bay.release();
            let shelf = volume_id
                .map(|id| slice_holders::shelf_of(self.conn, id, label))
                .unwrap_or_default();
            let want = Want {
                why: format!("To go on, {label} has to be back in the drive."),
                holds: format!("{label} is {}.", shelf.describe()),
                label: label.to_string(),
                device: self.device.clone(),
                skip_means: "give up (there is nothing else to go on with)",
            };
            if self.bay.ask(&want) != Answer::Loaded {
                return Err(e);
            }
        }
    }

    /// The cartridge in the drive leaves: its contact closes `ok` with what
    /// it supplied, its health is read, and the device is released.
    fn leave(&mut self) {
        if let Some(mut p) = self.here.take() {
            p.close(self.conn, self.config, self.operation, &self.device, None);
        }
        self.bay.release();
        self.last_position = None;
    }

    /// A cartridge has been loaded: open the drive on it, open its contact,
    /// and check it is `label` ([`super::corroborate_at_contact`]: File 0's
    /// label and uuid, the chip serial against the cartridge bound to the
    /// volume, and #476's `requires`). A cartridge that fails is closed
    /// `failed` and the drive released.
    fn arrive(&mut self, volume_id: Option<i64>, label: &str) -> Result<()> {
        let arrival = self.bay.load(self.conn, self.config)?;
        let (guard, serial) =
            arrival.with_site(self.config, self.operation, &self.device, |site| {
                (
                    site.open(self.conn, volume_id),
                    site.medium_serial().map(str::to_string),
                )
            });
        let identified =
            super::corroborate_at_contact(self.conn, label, self.bay.store(), serial.as_deref());
        let mut presence = Presence {
            volume_id,
            label: label.to_string(),
            guard: Some(guard),
            site: PresenceSite::Arrived(Box::new(arrival)),
            notes: Vec::new(),
        };
        match identified {
            Ok(()) => {
                self.here = Some(presence);
                self.last_position = None;
                Ok(())
            }
            Err(e) => {
                presence.close(
                    self.conn,
                    self.config,
                    self.operation,
                    &self.device,
                    Some(&e),
                );
                self.bay.release();
                Err(e)
            }
        }
    }
}

/// What one attempt measured, unclassified.
struct Raw {
    /// The tape read's result, until [`Raw::defect`] takes it.
    read: Option<Result<u64>>,
    hash_ok: bool,
    actual_hash: String,
    /// What the decrypt thread ended on, or the sink failing under it.
    ended: Result<Ended>,
}

impl Raw {
    /// The medium-side cause, by the evidence: the tape would not read, or
    /// what it gave does not hash. `None` when both are fine.
    fn defect(&mut self) -> Option<Defect> {
        match self.read.take() {
            Some(Err(e)) => Some(Defect::Read(e)),
            _ if !self.hash_ok => Some(Defect::Hash {
                actual: self.actual_hash.clone(),
            }),
            _ => None,
        }
    }
}

fn short_cause(defect: &Defect) -> String {
    match defect {
        Defect::Read(e) => format!("would not read: {e}"),
        Defect::Hash { .. } => "checksum mismatch".to_string(),
        Defect::Chunk => "a chunk failed authentication under a matching checksum".to_string(),
        Defect::OtherFile => "a different age file than the part already read".to_string(),
    }
}

/// The decrypt thread of one attempt: ciphertext from `src`, authenticated
/// chunk by chunk, to `out` from where `out` left off.
///
/// `Err` is a failure of the sink (dar stopped reading, the disk): not a
/// matter for another cartridge. A file that will not open as age is not an
/// error here — it is reported in [`Ended`], because whether it is the
/// medium's fault depends on the hash the caller computes.
fn decrypt_chunks(
    src: std::io::PipeReader,
    identities: &[age::x25519::Identity],
    encrypted: u64,
    mode: Mode,
    out: &mut SliceOut<'_>,
) -> Result<Ended> {
    let mut chunks = match chunks::open(src, identities, encrypted) {
        Ok(chunks) => chunks,
        Err(e) => {
            return Ok(Ended {
                sink_gone: false,
                stopped: false,
                other_file: false,
                open_error: Some(e),
            })
        }
    };
    // One slice is one age file. A read whose file is not the one that
    // delivered the first bytes adds nothing: its chunks authenticate under
    // its own key, and prove nothing about the part already written.
    let header = chunks.header().to_vec();
    if out.bound.as_deref().is_some_and(|bound| bound != header) {
        return Ok(Ended {
            sink_gone: false,
            stopped: false,
            other_file: true,
            open_error: None,
        });
    }
    let mut stopped = false;
    let delivered = (|| -> Result<()> {
        out.open()?;
        let skip = out.delivered;
        let mut pos = 0u64;
        let mut buf = Zeroizing::new(Vec::with_capacity(chunks::CHUNK));
        loop {
            let next = chunks
                .next(&mut buf)
                .map_err(|e| TapectlError::TapeIo(format!("slice {}: {e}", out.slice_number)))?;
            match next {
                Next::End => return Ok(()),
                Next::Authentic => {
                    let end = pos + buf.len() as u64;
                    if end > skip {
                        let from = skip.saturating_sub(pos) as usize;
                        out.bind(&header);
                        out.write(&buf[from..])?;
                    }
                    pos = end;
                }
                Next::Damaged { plain_len } => {
                    let end = pos + plain_len as u64;
                    if end > skip {
                        if mode == Mode::Strict {
                            stopped = true;
                            return Ok(());
                        }
                        out.bind(&header);
                        out.hole(end - skip.max(pos))?;
                    }
                    pos = end;
                }
            }
        }
    })();
    match delivered {
        Ok(()) => Ok(Ended {
            sink_gone: false,
            stopped,
            other_file: false,
            open_error: None,
        }),
        // dar stopped reading: not a failure of the medium or the slice.
        Err(_) if out.gone => Ok(Ended {
            sink_gone: true,
            stopped: false,
            other_file: false,
            open_error: None,
        }),
        Err(e) => Err(e),
    }
}
