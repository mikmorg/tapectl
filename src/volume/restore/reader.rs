//! tapectl's one reader: the slices decrypted, the members cut out of them
//! by the file table, each checked as it is fed, and tar run on what passes
//! (`volume-format-v3.md` §7.1; ADR-0012 items 255, 256, 260, 264, 266, 283,
//! 284; the plan's D16).
//!
//! # What it does
//!
//! A restore decrypts the slices it needs, in order, through the
//! [`Supply`] (which replaces a damaged slice from another copy, asks for a
//! cartridge, and records what nobody could give), decodes a compressed
//! slice alone ([`super::decode`]), and writes to one tar run, as the
//! members come, exactly the byte ranges the table gives. There is no spool
//! mode and no scratch copy of a slice: a slice's cut is held in memory for
//! as long as it takes to cut its members out.
//!
//! - **Every member is checked as it is fed.** A member lying inside one
//!   slice is held whole before a byte of it goes to tar, and its data is
//!   hashed against the table's sha256 first. A member that fails is not fed
//!   and is listed as damaged. A member that touches a hole, or whose slice
//!   is bad (the wrong size, a compressed frame that does not decode to its
//!   cut, a cut that does not match `sha256_plain`), is left out and listed.
//! - **A long member's run** (§3.3) cannot be held, so it is fed as its
//!   pieces arrive and hashed as it goes. If a piece is lost after tar has
//!   begun it, the run is cut short, tar ends on it at "Unexpected EOF" with
//!   the partial file on disk, the partial file is removed again, and the
//!   members after it go to a new tar run (item 264: "removed again if tar
//!   had begun it"). The clean path is exactly one tar run.
//! - **Nothing is zero-filled** and tar is never given `--ignore-zeros`
//!   (invariant 4): a lost slice's members are not in what tar reads.
//! - **A hard link is fed only with its target's member, target first**
//!   (item 256): [`filetable::select`] brings the target; a link whose
//!   target is damaged is listed damaged too.
//! - **After tar ends** every restored file is hashed on disk against the
//!   table, unless the restore was given `--no-file-check` (D16). The
//!   restore is judged by that and by what was left out, never by tar's
//!   exit status (invariant 6): tar's own complaints are kept and repeated.

use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Stdio};
use std::thread::JoinHandle;

use sha2::Digest;
use tracing::{info, warn};

use super::supply::Supply;
use super::{decode, RestoreTrace, WritePositionInfo};
use crate::error::{Result, TapectlError};
use crate::pass_count::Sha256;
use crate::progress;
use crate::staging::pack::Algorithm;
use crate::volume::filetable::{Kind, Row};
use crate::volume::restore_record::TarReport;

/// A tar block.
const BLOCK: u64 = 512;

/// What one read of members is given.
pub(super) struct Job<'a> {
    pub unit_name: &'a str,
    /// The members to restore, in stream order (a hard link's target before
    /// it): [`crate::volume::filetable::select`]'s answer.
    pub members: &'a [&'a Row],
    pub slice_size: u64,
    pub compression: Option<Algorithm>,
    /// The slices to read, in slice order, each with the volume it is read
    /// from.
    pub positions: &'a [WritePositionInfo],
    /// Slices on no live volume, by number, each with its reason (D17, item
    /// 266): not in `positions`, and the files in them are listed.
    pub unheld: &'a BTreeMap<i64, String>,
    /// Each slice's offset in the stream: the sum of the `size_bytes` of the
    /// slices before it.
    pub offsets: &'a BTreeMap<i64, u64>,
    pub identities: &'a [age::x25519::Identity],
    pub tar_binary: &'a str,
    /// Where tar extracts to.
    pub dest: &'a Path,
    pub overwrite: bool,
    /// Hash every restored file on disk afterwards (D16).
    pub file_check: bool,
}

/// What a read left.
#[derive(Debug, Default)]
pub(super) struct Outcome {
    /// Members that were left out, retracted, or failed their check on
    /// disk, by path, and why. The repair's work list (item 255).
    pub lost: BTreeMap<String, String>,
    /// Members fed to tar that stand.
    pub restored: usize,
    /// tar's own complaints, repeated (invariant 6: not the verdict).
    pub tar_said: Vec<String>,
}

/// The members left of a read, as it goes.
struct Reading<'j, 'a> {
    job: &'j Job<'a>,
    lost: BTreeMap<String, String>,
    tar: Option<TarRun>,
    /// The long member being fed, if one.
    run: Option<RunState>,
    /// The index (into `job.members`) of the next member that has not begun.
    next: usize,
    /// Members that were fed and stand, for the check on disk.
    fed: Vec<usize>,
    /// Reports of tar runs that ended, the last being the one recorded.
    runs: Vec<TarReport>,
    /// Slices that were not read, and why.
    unavailable: BTreeMap<u64, String>,
    /// Consecutive tar runs that died under a feed.
    deaths: u32,
}

/// A long member's run, in progress.
struct RunState {
    member: usize,
    /// The next slice of the run.
    next_slice: u64,
    hasher: Sha256,
    /// The member's data, as stream offsets.
    data: Range<u64>,
    /// The stream offset of the next byte to feed.
    at: u64,
}

/// One slice's cut, as far as it can be used.
struct Cut {
    bytes: Vec<u8>,
    /// Ranges of the cut (not of `bytes`) that hold nothing authentic.
    lost: Vec<Range<u64>>,
    /// The whole slice is bad, and why.
    bad: Option<String>,
}

impl Cut {
    /// Whether `range` of the cut is all authentic bytes.
    fn holds(&self, range: &Range<u64>) -> bool {
        self.bad.is_none()
            && range.end <= self.bytes.len() as u64
            && !self
                .lost
                .iter()
                .any(|l| l.start < range.end && range.start < l.end)
    }

    /// Why `range` of the cut cannot be used.
    fn why_not(&self, n: u64, range: &Range<u64>) -> String {
        match &self.bad {
            Some(why) => format!("slice {n} is bad: {why}"),
            None if range.end > self.bytes.len() as u64 => {
                format!(
                    "slice {n} ends before it: only {} bytes of it",
                    self.bytes.len()
                )
            }
            None => format!("a damaged part of slice {n} holds it"),
        }
    }
}

/// The cut of a slice as read: the bytes of the cut, decoded when the
/// staging is compressed, with what is lost of it. The slice's own checks
/// (invariant 6 and item 284): its length is its `size_bytes`; a compressed
/// slice's decoded bytes hash to `sha256_plain`, whatever the decoder said
/// of itself.
fn cut_of(
    read: super::supply::SliceRead,
    wp: &WritePositionInfo,
    algorithm: Option<Algorithm>,
) -> Cut {
    let size = wp.size_bytes.max(0) as u64;
    // Nothing was read: the memory for the recorded size could not be had.
    if let Some(why) = read.bad {
        return Cut {
            bytes: Vec::new(),
            lost: Vec::new(),
            bad: Some(why),
        };
    }
    let super::supply::SliceRead { bytes, holes, .. } = read;
    let Some(algorithm) = algorithm else {
        let bad = (bytes.len() as u64 != size && holes.is_empty()).then(|| {
            format!(
                "it holds {} bytes where the catalog records {size}",
                bytes.len()
            )
        });
        return Cut {
            bytes,
            lost: holes,
            bad,
        };
    };
    // A compressed slice: what the frame decodes to before the first hole.
    // Nothing after a bad chunk decodes (item 284), and what comes before it
    // is still the cut's own bytes.
    let frame_end = holes.first().map_or(bytes.len() as u64, |h| h.start);
    let decoded = decode::decode(algorithm, &bytes[..frame_end as usize], size);
    // The frame is not needed again: freed here, before the cut is hashed or
    // any member of it is fed to tar (#571, ADR-0012 item 298).
    drop(bytes);
    if let Some(why) = decoded.unallocatable {
        return Cut {
            bytes: Vec::new(),
            lost: Vec::new(),
            bad: Some(why),
        };
    }
    if holes.is_empty() {
        let mut hasher = Sha256::new();
        hasher.update(&decoded.bytes);
        let got: [u8; 32] = hasher.finalize().into();
        let recorded = crate::db::files::sha256_from_hex(&wp.sha256_plain).ok();
        if !decoded.complete || decoded.bytes.len() as u64 != size {
            return Cut {
                lost: Vec::new(),
                bad: Some(format!(
                    "its {} frame does not decode to the {size} bytes the catalog records",
                    algorithm.name()
                )),
                bytes: decoded.bytes,
            };
        }
        if recorded.is_some_and(|r| r != got) {
            return Cut {
                lost: Vec::new(),
                bad: Some(format!(
                    "its decoded bytes do not hash to the sha256 the catalog records ({})",
                    algorithm.name()
                )),
                bytes: decoded.bytes,
            };
        }
        return Cut {
            bytes: decoded.bytes,
            lost: Vec::new(),
            bad: None,
        };
    }
    let kept = decoded.bytes.len() as u64;
    Cut {
        lost: std::iter::once(kept..size.max(kept)).collect(),
        bytes: decoded.bytes,
        bad: None,
    }
}

/// The file's data, as stream offsets: the last `size` bytes before the
/// member's padding. `None` for a member with no data of its own (a link's
/// member holds none; its row states its target's size).
fn data_of(m: &Row) -> Option<Range<u64>> {
    if m.kind != Kind::File || m.size == 0 {
        return None;
    }
    let padded = m.size.div_ceil(BLOCK) * BLOCK;
    let start = m.end.checked_sub(padded)?;
    (start >= m.start).then_some(start..start + m.size)
}

impl<'j, 'a> Reading<'j, 'a> {
    fn members(&self) -> &'j [&'a Row] {
        self.job.members
    }

    fn lose(&mut self, m: &Row, why: impl Into<String>) {
        let why = why.into();
        self.lost.entry(m.path.clone()).or_insert(why);
    }

    /// The slice a member's bytes are in, as the plan has it.
    fn why_slice(&self, n: u64) -> String {
        let unheld = i64::try_from(n).ok().and_then(|n| self.job.unheld.get(&n));
        self.unavailable
            .get(&n)
            .or(unheld)
            .cloned()
            .unwrap_or_else(|| {
                format!("slice {n} was not read: it is on a cartridge this restore was not given")
            })
    }

    /// Hand `bytes` to tar, starting a run if none is open. `Ok(false)`:
    /// tar stopped under the feed; the run is closed and nothing of it is
    /// known to stand.
    fn put(&mut self, bytes: &[u8]) -> Result<bool> {
        if self.tar.is_none() {
            self.tar = Some(TarRun::spawn(self.job)?);
        }
        let tar = self.tar.as_mut().expect("just started");
        match tar.stdin.as_mut().map(|s| s.write_all(bytes)) {
            Some(Ok(())) => Ok(true),
            _ => {
                // tar stopped reading: it died, or could not make the
                // destination. Its own words say why.
                let report = self.tar.take().expect("open").close(false);
                let said = String::from_utf8_lossy(&report.stderr).trim().to_string();
                self.runs.push(report);
                self.deaths += 1;
                if self.deaths >= 3 {
                    return Err(TapectlError::Other(format!(
                        "tar stopped three times running while it was being fed, so the restore \
                         is given up: {}",
                        if said.is_empty() {
                            "it said nothing".to_string()
                        } else {
                            said
                        }
                    )));
                }
                Ok(false)
            }
        }
    }

    /// End the tar run in progress with its two end-of-archive blocks.
    fn end_run(&mut self) {
        if let Some(mut t) = self.tar.take() {
            let _ = t
                .stdin
                .as_mut()
                .map(|s| s.write_all(&[0u8; 2 * BLOCK as usize]));
            self.runs.push(t.close(true));
        }
    }

    /// End the tar run without its end blocks, in the middle of a member:
    /// tar stops on "Unexpected EOF", and the partial file is removed again.
    fn abort_run(&mut self, m: &Row) {
        if let Some(t) = self.tar.take() {
            self.runs.push(t.close(false));
        }
        let at = self.job.dest.join(crate::volume::index::on_disk(&m.path));
        if let Ok(meta) = std::fs::symlink_metadata(&at) {
            if meta.is_file() {
                if let Err(e) = std::fs::remove_file(&at) {
                    warn!(path = %at.display(), error = %e, "could not remove a partial file");
                }
            }
        }
    }

    /// The members after the last slice read, and a run left unfinished.
    fn close_skipped(&mut self, upto: u64) {
        // A run whose next slice was never read.
        if let Some(run) = self.run.as_ref() {
            if run.next_slice < upto {
                let why = self.why_slice(run.next_slice);
                self.drop_run(&why);
            }
        }
        while self.next < self.members().len() {
            let m = self.members()[self.next];
            if m.last_slice(self.job.slice_size) >= upto {
                break;
            }
            let why = self.why_slice(m.slice);
            self.lose(m, why);
            self.next += 1;
        }
    }

    /// Give up the run in progress. If tar has been given some of it, tar's
    /// run is ended on it and the partial file removed again; if not, the
    /// run in progress is whatever came before, and stands.
    fn drop_run(&mut self, why: &str) {
        if let Some(run) = self.run.take() {
            let m = self.members()[run.member];
            if run.at > m.start {
                self.abort_run(m);
            }
            self.lose(m, why);
        }
    }

    /// Whether the hard link `m`'s target is among the lost.
    fn target_lost(&self, m: &Row) -> Option<String> {
        let target = m.link.as_deref()?;
        self.lost
            .contains_key(target)
            .then(|| format!("it is a hard link to {target}, which is damaged"))
    }

    /// A member that lies in one slice.
    fn whole(&mut self, idx: usize, n: u64, s: u64, cut: &Cut) -> Result<()> {
        let m = self.members()[idx];
        let range = (m.start.saturating_sub(s))..(m.end.saturating_sub(s));
        if m.start < s || !cut.holds(&range) {
            let why = cut.why_not(n, &range);
            self.lose(m, why);
            return Ok(());
        }
        if let Some(why) = self.target_lost(m) {
            self.lose(m, why);
            return Ok(());
        }
        let bytes = &cut.bytes[range.start as usize..range.end as usize];
        // The data is hashed against the table before a byte of the member
        // goes to tar (§7.1).
        if let (Some(data), Some(want)) = (data_of(m), m.sha256) {
            let from = (data.start - m.start) as usize;
            let mut hasher = Sha256::new();
            hasher.update(&bytes[from..from + m.size as usize]);
            let got: [u8; 32] = hasher.finalize().into();
            if got != want {
                self.lose(
                    m,
                    "its bytes in the slice do not hash to the table's sha256",
                );
                return Ok(());
            }
        }
        if self.put(bytes)? {
            self.deaths = 0;
            self.fed.push(idx);
        } else {
            self.lose(m, "tar stopped while it was being fed");
        }
        Ok(())
    }

    /// The piece of a long member's run that lies in slice `n`.
    fn piece(&mut self, idx: usize, n: u64, s: u64, cut: &Cut) -> Result<()> {
        let m = self.members()[idx];
        let from = m.start.max(s);
        let first = n == m.slice;
        if first {
            let data = data_of(m).unwrap_or(m.end..m.end);
            self.run = Some(RunState {
                member: idx,
                next_slice: n,
                hasher: Sha256::new(),
                data,
                at: m.start,
            });
        }
        let Some(run) = self.run.as_ref().filter(|r| r.member == idx) else {
            // The run was given up at an earlier piece.
            return Ok(());
        };
        let expected_end = (m.end).min(s + self.job.slice_size);
        let range = (from - s)..(expected_end - s);
        if from != run.at || !cut.holds(&range) {
            let why = if from != run.at {
                format!("slice {n} does not continue the member where the table says")
            } else {
                cut.why_not(n, &range)
            };
            self.drop_run(&why);
            return Ok(());
        }
        if first {
            if let Some(why) = self.target_lost(m) {
                self.drop_run(&why);
                return Ok(());
            }
        }
        let bytes = &cut.bytes[range.start as usize..range.end as usize];
        if !self.put(bytes)? {
            self.run = None;
            self.lose(m, "tar stopped while it was being fed");
            return Ok(());
        }
        let run = self.run.as_mut().expect("in progress");
        // Hash what lies in the data.
        let lo = run.at.max(run.data.start);
        let hi = (run.at + bytes.len() as u64).min(run.data.end);
        if lo < hi {
            run.hasher
                .update(&bytes[(lo - run.at) as usize..(hi - run.at) as usize]);
        }
        run.at += bytes.len() as u64;
        run.next_slice = n + 1;
        if run.at >= m.end {
            let run = self.run.take().expect("in progress");
            let got: [u8; 32] = run.hasher.finalize().into();
            if m.sha256.is_some_and(|want| want != got) {
                // It went to tar already; it is listed, and the check on
                // disk would say the same.
                self.lose(m, "its bytes in the run do not hash to the table's sha256");
                self.fed.push(idx);
            } else {
                self.deaths = 0;
                self.fed.push(idx);
            }
        }
        Ok(())
    }

    /// One slice, read.
    fn slice(&mut self, wp: &WritePositionInfo, cut: &Cut) -> Result<()> {
        let n = wp.slice_number as u64;
        let Some(&s) = self.job.offsets.get(&wp.slice_number) else {
            return Err(TapectlError::Other(format!(
                "slice {n} has no place in the stream: the catalog's slice list does not hold it"
            )));
        };
        // A run in progress takes its next piece first.
        if let Some(run) = self.run.as_ref() {
            if run.next_slice == n {
                let idx = run.member;
                self.piece(idx, n, s, cut)?;
            }
        }
        while self.next < self.members().len() {
            let idx = self.next;
            let m = self.members()[idx];
            if m.slice > n {
                break;
            }
            if m.last_slice(self.job.slice_size) < n {
                // Its slices went by (not in the plan).
                let why = self.why_slice(m.slice);
                self.lose(m, why);
                self.next += 1;
                continue;
            }
            self.next += 1;
            if m.slice < n {
                // A run begun in a slice before this one: it was taken above
                // or given up, unless its first slice was never read.
                if !self.lost.contains_key(&m.path)
                    && self.run.as_ref().is_none_or(|r| r.member != idx)
                {
                    let why = self.why_slice(m.slice);
                    self.lose(m, why);
                }
                continue;
            }
            if m.is_run(self.job.slice_size) {
                self.piece(idx, n, s, cut)?;
            } else {
                self.whole(idx, n, s, cut)?;
            }
        }
        Ok(())
    }
}

/// Read the job's slices and feed tar the members: the whole of
/// [`Reading`]'s work, and the check on disk after it.
pub(super) fn run(
    job: &Job<'_>,
    supply: &mut Supply<'_, '_, '_>,
    trace: &mut RestoreTrace,
) -> Result<Outcome> {
    std::fs::create_dir_all(job.dest).map_err(|e| {
        TapectlError::Other(format!(
            "cannot make the destination {}: {e}",
            job.dest.display()
        ))
    })?;
    let mut reading = Reading {
        job,
        lost: BTreeMap::new(),
        tar: None,
        run: None,
        next: 0,
        fed: Vec::new(),
        runs: Vec::new(),
        unavailable: BTreeMap::new(),
        deaths: 0,
    };
    supply.set_route(job.positions);
    let total: u64 = job
        .positions
        .iter()
        .map(|wp| wp.encrypted_bytes.max(0) as u64)
        .sum();
    let phase = progress::phase("read", Some(total));
    let mut failure: Option<TapectlError> = None;
    for (i, wp) in job.positions.iter().enumerate() {
        phase.item(format!(
            "{} slice {} of {} (file {})",
            job.unit_name,
            i + 1,
            job.positions.len(),
            wp.position
        ));
        info!(
            slice = wp.slice_number,
            n = i + 1,
            total = job.positions.len(),
            tape_pos = %wp.position,
            "reading slice from tape"
        );
        // Issue #404: between slices. What tar has been fed stays where it
        // is, so a stop leaves a partial restore, said below.
        if let Err(e) = crate::signal::check(|| {
            format!(
                "restore of \"{}\" stopped after reading {i} of {} slices",
                job.unit_name,
                job.positions.len()
            )
        }) {
            failure = Some(e);
            break;
        }
        let n = wp.slice_number as u64;
        reading.close_skipped(n);
        let read = match supply.read_slice(wp, job.identities, job.compression) {
            Ok(read) => read,
            Err(e) => {
                failure = Some(e);
                break;
            }
        };
        trace.slices_read += 1;
        trace.bytes_decrypted += read.authentic_bytes as i64;
        if let Some(why) = read.unavailable.clone() {
            reading.unavailable.insert(n, why);
            continue;
        }
        let cut = cut_of(read, wp, job.compression);
        if let Err(e) = reading.slice(wp, &cut) {
            failure = Some(e);
            break;
        }
    }
    if failure.is_none() {
        phase.done();
        reading.close_skipped(u64::MAX);
        if reading.run.is_some() {
            reading.drop_run("its run ends before its last slice");
        }
    } else {
        drop(phase);
        if let Some(run) = reading.run.take() {
            let m = reading.members()[run.member];
            if run.at > m.start {
                reading.abort_run(m);
            }
        }
    }
    reading.end_run();
    // tar's own complaints, repeated; never the verdict. Recorded before a
    // failure returns too: tar ran and extracted part of the unit, and its
    // last run is what the `restores` row says (#585).
    let mut tar_said = Vec::new();
    for r in &reading.runs {
        let text = String::from_utf8_lossy(&r.stderr);
        for line in text.lines().filter(|l| !l.trim().is_empty()) {
            tar_said.push(line.trim().to_string());
        }
        if r.exit_code != Some(0) {
            tar_said.push(format!(
                "tar exited {}",
                r.exit_code
                    .map_or("on a signal".to_string(), |c| c.to_string())
            ));
        }
    }
    for line in &tar_said {
        warn!("tar: {line}");
        progress::log(&format!("tar: {line}"));
    }
    trace.tar = reading.runs.last().cloned();
    if let Some(e) = failure {
        let partial = |what: String| {
            format!(
                "{what}. tar was extracting the unit into {} as its slices arrived, so that \
                 directory may now hold a partial restore: empty it (or restore again with \
                 --overwrite) before trusting what is there",
                job.dest.display()
            )
        };
        // A stop by signal (#404) stays one, so it exits as one.
        return Err(match e {
            TapectlError::Interrupted(what) => TapectlError::Interrupted(partial(what)),
            other => TapectlError::Other(partial(other.to_string())),
        });
    }

    // Every restored file, hashed on disk (D16).
    let fed: Vec<&Row> = reading.fed.iter().map(|&i| reading.members()[i]).collect();
    let mut lost = std::mem::take(&mut reading.lost);
    if job.file_check {
        disk_check(job.dest, &fed, &mut lost);
    } else {
        info!("restored files not hashed on disk (--no-file-check)");
    }
    let restored = fed.iter().filter(|m| !lost.contains_key(&m.path)).count();
    Ok(Outcome {
        lost,
        restored,
        tar_said,
    })
}

/// Hash every file of `fed` at `root` against the table; a path that is not
/// there, or is not what the table says, joins `lost`.
pub(super) fn disk_check(root: &Path, fed: &[&Row], lost: &mut BTreeMap<String, String>) {
    let phase = progress::phase("verify-files", None);
    for m in fed {
        if lost.contains_key(&m.path) {
            continue;
        }
        phase.item(m.path.clone());
        let at: PathBuf = if m.path == "." {
            root.to_path_buf()
        } else {
            root.join(crate::volume::index::on_disk(&m.path))
        };
        let Ok(meta) = std::fs::symlink_metadata(&at) else {
            lost.insert(m.path.clone(), "not restored".to_string());
            continue;
        };
        match m.kind {
            Kind::File | Kind::HardLink => {
                if !meta.is_file() {
                    lost.insert(
                        m.path.clone(),
                        "restored as something that is not a file".into(),
                    );
                    continue;
                }
                if m.kind == Kind::File && meta.len() != m.size {
                    lost.insert(
                        m.path.clone(),
                        format!(
                            "{} bytes restored where the table says {}",
                            meta.len(),
                            m.size
                        ),
                    );
                    continue;
                }
                let Some(want) = m.sha256 else { continue };
                match hash_file(&at) {
                    Ok(got) if got == want => {}
                    Ok(_) => {
                        lost.insert(
                            m.path.clone(),
                            "does not hash to the table's sha256".to_string(),
                        );
                    }
                    Err(e) => {
                        lost.insert(m.path.clone(), format!("cannot be read back: {e}"));
                    }
                }
            }
            Kind::Symlink if !meta.is_symlink() => {
                lost.insert(
                    m.path.clone(),
                    "restored as something that is not a link".into(),
                );
            }
            Kind::Folder if !meta.is_dir() => {
                lost.insert(
                    m.path.clone(),
                    "restored as something that is not a folder".into(),
                );
            }
            _ => {}
        }
    }
    phase.done();
}

/// The sha256 of the file at `path`.
pub(super) fn hash_file(path: &Path) -> std::io::Result<[u8; 32]> {
    let mut file = std::fs::File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 128 * 1024];
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hasher.finalize().into())
}

/// How much of tar's output is kept.
const KEPT: usize = 64 * 1024;

/// One tar run: `tar --extract` reading the members on its standard input.
struct TarRun {
    child: Child,
    stdin: Option<ChildStdin>,
    stdout: Option<JoinHandle<Vec<u8>>>,
    stderr: Option<JoinHandle<Vec<u8>>>,
    argv: Vec<String>,
}

/// Read `pipe` to its end on a thread, keeping the first [`KEPT`] bytes.
fn drain<R: Read + Send + 'static>(pipe: Option<R>) -> Option<JoinHandle<Vec<u8>>> {
    let mut pipe = pipe?;
    Some(std::thread::spawn(move || {
        let mut kept = Vec::new();
        let mut buf = [0u8; 8192];
        loop {
            match pipe.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    let room = KEPT.saturating_sub(kept.len());
                    kept.extend_from_slice(&buf[..n.min(room)]);
                }
            }
        }
        kept
    }))
}

impl TarRun {
    /// tar's command line for an extract (§7.1, determined): the numeric
    /// owners, the permissions, the extended attributes and ACLs as the
    /// stream has them, and old files kept (or overwritten when the restore
    /// was given `--overwrite`). Never `--ignore-zeros`, never
    /// `--absolute-names`.
    fn args(dest: &Path, overwrite: bool) -> Vec<String> {
        vec![
            "--extract".into(),
            "--file=-".into(),
            format!("--directory={}", dest.display()),
            "--numeric-owner".into(),
            "--preserve-permissions".into(),
            "--xattrs".into(),
            "--acls".into(),
            if overwrite {
                "--overwrite".into()
            } else {
                "--keep-old-files".into()
            },
        ]
    }

    fn spawn(job: &Job<'_>) -> Result<Self> {
        let args = Self::args(job.dest, job.overwrite);
        let mut cmd = crate::tar::command(job.tar_binary);
        cmd.args(&args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = cmd
            .spawn()
            .map_err(|e| TapectlError::Other(format!("cannot run {}: {e}", job.tar_binary)))?;
        let mut argv = vec![job.tar_binary.to_string()];
        argv.extend(args);
        let stdin = child.stdin.take();
        let stdout = drain(child.stdout.take());
        let stderr = drain(child.stderr.take());
        Ok(TarRun {
            child,
            stdin,
            stdout,
            stderr,
            argv,
        })
    }

    /// Close tar's input and collect what it did. `ended`: the archive was
    /// ended properly; otherwise tar stops on the missing end.
    fn close(mut self, _ended: bool) -> TarReport {
        drop(self.stdin.take());
        let status = self.child.wait().ok();
        let stdout = self
            .stdout
            .take()
            .and_then(|h| h.join().ok())
            .unwrap_or_default();
        let stderr = self
            .stderr
            .take()
            .and_then(|h| h.join().ok())
            .unwrap_or_default();
        TarReport {
            argv: self.argv,
            exit_code: status.and_then(|s| s.code()),
            stdout,
            stderr,
        }
    }
}
