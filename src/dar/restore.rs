use std::path::Path;

use tracing::warn;

use crate::error::{Result, TapectlError};

/// `-O` (`--comparison-field`, ignore-owner) exists precisely because a
/// non-root restore cannot set stored ownership. Warn the operator, so a
/// restore that silently lands everything under the invoking user rather
/// than the archived owners does not go unnoticed (issue #51).
fn warn_if_non_root() {
    if !nix::unistd::geteuid().is_root() {
        warn!(
            "restoring as a non-root user: restored files will be owned by \
             the invoking user, not their archived owners"
        );
    }
}

/// dar's marker, on stdout, for a file it declined to overwrite.
const SKIPPED_MARKER: &str = "not restored (user choice)";

/// Paths dar silently declined to restore because something already
/// existed at the destination.
///
/// EMPIRICAL BASIS (dar 2.7.13, issues #50/#51, ratified 2026-07-31).
/// Under `-Q` there is no terminal, so dar answers its own overwrite
/// prompt with "no" — it leaves the stale file, extracts everything else,
/// and **exits 0**. The operator is told the restore succeeded.
///
/// Two traps make this worth pinning in code:
///
/// 1. The obvious counter is the wrong one. The skip is tallied under
///    `inode(s) ignored (excluded by filters)`, while `inode(s) not
///    restored (overwriting policy decision)` — the line whose name
///    matches the situation exactly — stays **0**. A detector keyed off
///    that counter would never fire.
/// 2. The signal is on **stdout**, not stderr (verified by stream).
///
/// So we key off the per-file line, which names the path and is the only
/// place the specific casualty appears.
pub(crate) fn skipped_paths(stdout: &str) -> Vec<String> {
    stdout
        .lines()
        .filter_map(|line| {
            let line = line.trim();
            line.strip_suffix(SKIPPED_MARKER)
                .map(|path| path.trim().to_string())
                .filter(|path| !path.is_empty())
        })
        .collect()
}

/// Turn a silent partial restore into a loud failure (issue #51).
fn fail_on_skipped(stdout: &[u8], dest: &Path) -> Result<()> {
    let skipped = skipped_paths(&String::from_utf8_lossy(stdout));
    if skipped.is_empty() {
        return Ok(());
    }
    Err(TapectlError::Other(format!(
        "restore into \"{}\" is INCOMPLETE: dar declined to overwrite {} file(s) that already \
         existed, and would otherwise have reported success. The stale copies are still in \
         place — the restored data is NOT what is on tape. Skipped: {}. Restore into an empty \
         directory, or remove those files first.",
        dest.display(),
        skipped.len(),
        skipped.join(", "),
    )))
}

/// dar's report of one `-x` invocation, VERBATIM (issue #306; ADR-0013's
/// "capture everything verbatim now, parse it later", on the content side).
///
/// Both streams byte for byte — dar prints file names, and a non-UTF-8 name
/// lossily rewritten is not the report — plus the argv and the exit status.
/// Handed back on EVERY path the process ran: a clean exit, a non-zero exit,
/// and the exit-0-but-INCOMPLETE case [`skipped_paths`] exists for, where
/// the "not restored (user choice)" lines in `stdout` are the only evidence.
/// Until this existed, success kept nothing and failure kept an excerpt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DarReport {
    /// The command line, program first. Lossily stringified for JSON —
    /// provenance, not evidence; the evidence is the two streams.
    pub argv: Vec<String>,
    /// `None` when dar was killed by a signal.
    pub exit_code: Option<i32>,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}

impl DarReport {
    /// `argv` as a JSON array, the spelling `mam_journal.tool_argv` uses.
    pub fn argv_json(&self) -> String {
        serde_json::Value::from(self.argv.clone()).to_string()
    }

    /// dar's own `N inode(s) restored` summary count, or `None` when the
    /// summary block is not in `stdout` (dar killed, or output not UTF-8).
    ///
    /// A convenience parsed from the verbatim text, never a substitute for
    /// it — and NOT the `inode(s) not restored (overwriting policy
    /// decision)` counter, which [`skipped_paths`] proves cannot be trusted.
    pub fn inodes_restored(&self) -> Option<i64> {
        inodes_restored(std::str::from_utf8(&self.stdout).ok()?)
    }
}

/// The `N inode(s) restored` line of a dar summary block, as a count.
fn inodes_restored(stdout: &str) -> Option<i64> {
    stdout.lines().find_map(|line| {
        line.trim()
            .strip_suffix("inode(s) restored")
            .and_then(|n| n.trim().parse().ok())
    })
}

/// Run `dar -x` with `args` and hand back BOTH its verbatim report and the
/// verdict. The report is `None` only when dar could not be spawned.
///
/// The verdict fails on a non-zero exit, and on the silent-skip case
/// (`fail_on_skipped`) even though dar exited 0.
fn run_extract(
    dar_binary: &str,
    what: &str,
    args: &[std::ffi::OsString],
    dest: &Path,
) -> (Option<DarReport>, Result<()>) {
    let argv = argv_of(dar_binary, args);
    // Issue #404: an extraction can take hours; a signal stops dar.
    let output = match super::run_interruptible(super::command(dar_binary).args(args), || {
        format!(
            "dar was stopped while extracting into {} — what it restored so far is \
             incomplete; run the restore again",
            dest.display()
        )
    }) {
        Ok(o) => o,
        Err(e) => return (None, Err(e)),
    };
    let report = DarReport {
        argv,
        exit_code: output.status.code(),
        stdout: output.stdout,
        stderr: output.stderr,
    };
    let verdict = verdict_of(&report, what, dest);
    (Some(report), verdict)
}

/// The command line as [`DarReport::argv`] records it, program first.
fn argv_of(dar_binary: &str, args: &[std::ffi::OsString]) -> Vec<String> {
    std::iter::once(dar_binary.to_string())
        .chain(args.iter().map(|a| a.to_string_lossy().into_owned()))
        .collect()
}

/// [`run_extract`]'s verdict on a finished dar: a non-zero exit (or a
/// signal) fails, and so does exit 0 with a silent skip (`fail_on_skipped`).
pub(crate) fn verdict_of(report: &DarReport, what: &str, dest: &Path) -> Result<()> {
    if report.exit_code != Some(0) {
        let stderr = String::from_utf8_lossy(&report.stderr);
        return Err(TapectlError::Dar(format!("{what} failed: {stderr}")));
    }
    fail_on_skipped(&report.stdout, dest)
}

/// The oldest dar a restore streams into (issue #411): dar reads a SLICED
/// archive in `--sequential-read` mode only since 2.7.7 ("added support for
/// sequential reading mode of sliced backup, to accommodate tape support used
/// with slices"), and reads one correctly with the help of an isolated
/// catalogue only since 2.7.9 (its ChangeLog: "the unability to properly rely
/// on a isolated catalogue to read (test/extract/diff) an backup in
/// sequential read mode, leading dar to report CRC error"). An older dar —
/// the 2.6 line tapectl still accepts — is restored from spooled slices.
pub const STREAMING_MIN_VERSION: (u32, u32, u32) = (2, 7, 9);

/// Whether `version` can take a restore's slices as a stream
/// ([`STREAMING_MIN_VERSION`]).
pub fn supports_streaming(version: &super::version::DarVersion) -> bool {
    (version.major, version.minor, version.patch) >= STREAMING_MIN_VERSION
}

/// `dar -x - --sequential-read -A <catalogue>`: dar extracting an archive it
/// reads as one stream on its standard input, which the restore writes as it
/// decrypts the slices off the tape (issues #411, #498) — the process, the
/// stream it is fed, and the two threads draining its output.
///
/// **One stream, never a named pipe per slice (#498).** Fed one named pipe
/// per slice (`<base>.N.dar`), dar's slice layer sees a slice's size as 0
/// and finds the slice's end only on a short read; when one of its reads
/// asks for exactly the bytes left in a slice, trailer included, the
/// trailer byte (`N`) comes back as archive data: a one-byte shift, then
/// "Bad CRC" or "invalid FSA … flag" and exit 5, the same way every time for
/// one archive (`sar::inherited_read`, every dar from 2.7.7 to 2.8.6). An
/// archive on standard input is read by dar's single-stream reader, which
/// looks one byte ahead and never takes the last byte as data. So the
/// slices are joined back into the `dar -c -` stream tapectl cut them from
/// ([`super::slice::Join`]: dar's standard-output header with slice 1's
/// labels, every slice's payload with its own header and trailer taken off
/// and checked, the stream's `T`), and that stream is what dar reads.
/// Measured with dar 2.7.13 and 2.7.21, each way round: the join of
/// tapectl's slices is the stream byte for byte, and dar restores it
/// identically, with the catalogue and without.
///
/// **Why the isolated catalogue (`-A`) is always passed.** Measured against
/// dar 2.7.13 (the version this VM and Ubuntu's CI carry): a sliced archive
/// read with `--sequential-read` and nothing else fails on about half of the
/// slice layouts tried — a file whose data runs into the last slice is cut
/// short, dar asks for a slice after the last one, and with `-Q` gives up
/// ("arch.4.dar is required for further operation"). The SAME archives,
/// read the same way with the isolated catalogue given as `-A`, came back
/// identical on every layout tried (slice sizes from 20 KiB to 3 MiB, all
/// seven compressors).
///
/// **With `-A`, dar does not read every byte (#497).** The catalogue tells
/// dar where the file data ends, and it stops reading there: the archive's
/// own trailing catalogue is left unread (the tail of the last slice, and,
/// when that catalogue is bigger than a slice, the last slices whole), dar
/// closes its input, and exits 0. Measured on 2.7.13: 10,000 files in 20 MiB
/// slices left 979,621 of the last slice's 6,419,109 bytes unread, and the
/// restored tree was identical. The restore accepts it: what dar leaves of
/// the stream is still read off the tape, hashed and authenticated slice by
/// slice, its plaintext dropped, and dar's exit status ([`Self::status`],
/// then [`Self::finish`]) says whether the stop was a failure. A catalogue
/// from a DIFFERENT dar run of the same content is refused by dar itself,
/// FATAL, before any file is written ("The archive and the isolated
/// catalogue do not correspond to the same data") — so a wrong catalogue
/// cannot produce a wrong restore.
pub struct SequentialExtract {
    child: std::sync::Mutex<std::process::Child>,
    input: std::sync::Mutex<Input>,
    argv: Vec<String>,
    stdout: Option<std::thread::JoinHandle<Vec<u8>>>,
    stderr: Option<std::thread::JoinHandle<Vec<u8>>>,
    dest: std::path::PathBuf,
}

/// dar's standard input: the archive's stream, joined from its slices as
/// they are fed.
struct Input {
    /// `None` once the input is closed: by tapectl, by a refusal, or because
    /// dar closed it.
    join: Option<super::slice::Join<std::process::ChildStdin>>,
    /// Why tapectl stopped feeding dar, when it refused a slice.
    refused: Option<String>,
}

impl SequentialExtract {
    /// Start `dar -x - --sequential-read -A <catalogue> -R <dest> -O -Q -N
    /// [-w]`, its standard input the archive's stream, which the caller
    /// feeds slice by slice in order ([`Self::begin_slice`], [`Self::feed`],
    /// [`Self::feed_hole`], [`Self::end_slice`]). `stream_template` is the
    /// installed dar's standard-output header
    /// ([`super::slice::stream_template`]); it is checked before dar starts.
    pub fn spawn(
        dar_binary: &str,
        catalogue: &Path,
        dest: &Path,
        overwrite: bool,
        stream_template: &[u8],
    ) -> Result<Self> {
        let template = super::slice::StreamTemplate::new(stream_template)?;
        if let Err(e) = std::fs::create_dir_all(dest) {
            return Err(e.into());
        }
        warn_if_non_root();
        let mut args: Vec<std::ffi::OsString> = vec![
            "-x".into(),
            "-".into(),
            "--sequential-read".into(),
            "-A".into(),
            catalogue.into(),
            "-R".into(),
            dest.into(),
            // -O is `--comparison-field` (ignore-owner): see `extract`.
            "-O".into(),
            "-Q".into(),
            "-N".into(), // no darrc: see `extract_reported` (#412 item 9)
        ];
        if overwrite {
            args.push("-w".into());
        }
        let argv = argv_of(dar_binary, &args);
        // Through `dar::command` (#404): dar dies with tapectl. The restore
        // stops it between slices on a signal ([`Self::kill`]).
        let mut child = super::command(dar_binary)
            .args(&args)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .map_err(|e| TapectlError::Dar(format!("cannot start {dar_binary}: {e}")))?;
        let stdin = child.stdin.take();
        // Drained on threads: dar prints a line per file it declines to
        // overwrite, and a full 64 KiB pipe would stop it mid-extract.
        let drain = |stream: Option<Box<dyn std::io::Read + Send>>| {
            stream.map(|mut s| {
                std::thread::spawn(move || {
                    let mut buf = Vec::new();
                    let _ = s.read_to_end(&mut buf);
                    buf
                })
            })
        };
        let stdout = drain(
            child
                .stdout
                .take()
                .map(|s| Box::new(s) as Box<dyn std::io::Read + Send>),
        );
        let stderr = drain(
            child
                .stderr
                .take()
                .map(|s| Box::new(s) as Box<dyn std::io::Read + Send>),
        );
        Ok(Self {
            child: std::sync::Mutex::new(child),
            input: std::sync::Mutex::new(Input {
                join: stdin.map(|stdin| super::slice::Join::new(stdin, template)),
                refused: None,
            }),
            argv,
            stdout,
            stderr,
            dest: dest.to_path_buf(),
        })
    }

    /// Hand `f` the stream, and say what became of it. `Ok` is taken.
    /// `BrokenPipe` is dar taking no more: dar closed its input (or ended),
    /// the input was closed, or tapectl refused the slice as not the
    /// archive's and closed it ([`Self::refused`] says why). Any other error
    /// is the write failing; the input is closed then too.
    fn with_stream(
        &self,
        f: impl FnOnce(&mut super::slice::Join<std::process::ChildStdin>) -> Result<()>,
    ) -> std::io::Result<()> {
        let mut input = self.input.lock().unwrap_or_else(|p| p.into_inner());
        let Some(join) = input.join.as_mut() else {
            return Err(std::io::ErrorKind::BrokenPipe.into());
        };
        match f(join) {
            Ok(()) => Ok(()),
            Err(TapectlError::Io(e)) => {
                input.join = None;
                Err(e)
            }
            Err(refusal) => {
                let why = refusal.to_string();
                warn!("{why}; tapectl stopped feeding dar there");
                input.refused = Some(why.clone());
                input.join = None;
                Err(std::io::Error::new(std::io::ErrorKind::BrokenPipe, why))
            }
        }
    }

    /// Begin slice `n` of the archive (from 1, none skipped); `last` when it
    /// is the archive's last.
    pub fn begin_slice(&self, n: u32, last: bool) -> std::io::Result<()> {
        self.with_stream(|join| join.begin(n, last))
    }

    /// The slice's next bytes.
    pub fn feed(&self, bytes: &[u8]) -> std::io::Result<()> {
        self.with_stream(|join| join.write(bytes))
    }

    /// The slice's next `len` bytes are a hole: zeros, where nobody could
    /// give the bytes whole. They keep their place in the stream.
    pub fn feed_hole(&self, len: u64) -> std::io::Result<()> {
        self.with_stream(|join| join.hole(len))
    }

    /// End the slice: its trailer is checked, and after the last slice the
    /// stream's own is written.
    pub fn end_slice(&self) -> std::io::Result<()> {
        self.with_stream(|join| join.end())
    }

    /// Close dar's input: dar reads the end of the stream and ends.
    pub fn close_input(&self) {
        let mut input = self.input.lock().unwrap_or_else(|p| p.into_inner());
        input.join = None;
    }

    /// Why tapectl stopped feeding dar, when it refused a slice: the bytes
    /// were not a slice of this archive as tapectl cut it.
    pub fn refused(&self) -> Option<String> {
        let input = self.input.lock().unwrap_or_else(|p| p.into_inner());
        input.refused.clone()
    }

    /// Whether dar has already exited.
    pub fn exited(&self) -> bool {
        let mut child = self.child.lock().unwrap_or_else(|p| p.into_inner());
        !matches!(child.try_wait(), Ok(None))
    }

    /// dar's exit status once it has ended, `None` while it runs. A dar that
    /// stopped reading its input part way is judged by this, never by the
    /// input it closed (#497).
    pub fn status(&self) -> Option<std::process::ExitStatus> {
        let mut child = self.child.lock().unwrap_or_else(|p| p.into_inner());
        child.try_wait().ok().flatten()
    }

    /// Stop dar — a restore that cannot deliver the next slice — and close
    /// its input.
    pub fn kill(&self) {
        {
            let mut child = self.child.lock().unwrap_or_else(|p| p.into_inner());
            if matches!(child.try_wait(), Ok(None)) {
                let _ = child.kill();
            }
        }
        self.close_input();
    }

    /// Close dar's input, wait for dar to end, and hand back its verbatim
    /// report and the verdict ([`extract_reported`]'s, `fail_on_skipped`
    /// included). The input is closed first: a dar still reading it would
    /// otherwise wait for it, and be waited for, forever.
    pub fn finish(mut self) -> (Option<DarReport>, Result<()>) {
        self.close_input();
        let status = {
            let mut child = self.child.lock().unwrap_or_else(|p| p.into_inner());
            child.wait()
        };
        let join = |h: Option<std::thread::JoinHandle<Vec<u8>>>| {
            h.and_then(|h| h.join().ok()).unwrap_or_default()
        };
        let stdout = join(self.stdout.take());
        let stderr = join(self.stderr.take());
        let status = match status {
            Ok(s) => s,
            Err(e) => {
                return (
                    None,
                    Err(TapectlError::Dar(format!("waiting for dar: {e}"))),
                )
            }
        };
        let report = DarReport {
            argv: std::mem::take(&mut self.argv),
            exit_code: status.code(),
            stdout,
            stderr,
        };
        let verdict = verdict_of(&report, "dar -x --sequential-read", &self.dest);
        (Some(report), verdict)
    }
}

/// The slice numbers a direct-mode `dar -x -g <file_path>` reads, as an
/// isolated catalogue lists them (issue #411): every slice the entry and the
/// directories above it are recorded in (`dar -l -Tslicing -g`), WITHOUT the
/// archive's last slice, which dar always opens first for its catalogue and
/// the caller adds.
///
/// The ancestors count: a directory's filesystem attributes are stored in
/// the slice its entry was written to, and dar restores the directories
/// above the requested entry with them — measured on dar 2.7.13, `dar -x -g
/// a/b/file` with only the file's own slices and the last present skips the
/// whole of `a` ("a not restored (user choice)", exit 0); with the ancestors'
/// slices too it is identical.
///
/// `Ok(None)` when the listing does not name `file_path` itself (matched by
/// its exact text at the end of a line, so a name with spaces or tabs is
/// still found), or carries a range this cannot read: the caller then reads
/// every slice, as it always did.
pub fn entry_slices(
    dar_binary: &str,
    catalogue: &Path,
    file_path: &str,
) -> Result<Option<std::collections::BTreeSet<i64>>> {
    let output = super::command(dar_binary)
        .arg("-l")
        .arg(catalogue)
        .arg("-Tslicing")
        .arg("-g")
        .arg(file_path)
        .arg("-Q")
        .arg("-N") // no darrc (#412 item 9)
        .stdin(std::process::Stdio::null())
        .output()
        .map_err(|e| TapectlError::Dar(e.to_string()))?;
    if !output.status.success() {
        return Ok(None);
    }
    Ok(parse_slicing_listing(
        &String::from_utf8_lossy(&output.stdout),
        file_path,
    ))
}

/// [`entry_slices`]'s parser over `dar -l -Tslicing` text, e.g. (`<TAB>` stands for a tab):
///
/// ```text
/// Slice(s)|[Data ][D][ EA  ][FSA][Compr][S]|Permission| Filemane
/// --------+--------------------------------+----------+-----------------------------
/// 1<TAB> [InRef][-]       [-L-][  51%][ ] drwxrwxr-x a
/// 4-5<TAB> [InRef][ ]       [-L-][-----][ ] -rw-rw-r-- a/r20.bin
/// ```
///
/// A row is `<slices>\t <flags> <permission> <path>`; `<slices>` is `N`,
/// `N-M`, comma-separated pieces of those, or EMPTY for an entry with
/// nothing in any slice (a symlink: its target lives in the catalogue).
fn parse_slicing_listing(text: &str, file_path: &str) -> Option<std::collections::BTreeSet<i64>> {
    let mut slices = std::collections::BTreeSet::new();
    let mut named = false;
    let suffix = format!(" {file_path}");
    for line in text.lines() {
        let Some((field, rest)) = line.split_once('\t') else {
            continue;
        };
        if !field
            .chars()
            .all(|c| c.is_ascii_digit() || c == '-' || c == ',')
        {
            continue;
        }
        if rest.ends_with(&suffix) {
            named = true;
        }
        for piece in field.split(',').filter(|p| !p.is_empty()) {
            let (lo, hi) = match piece.split_once('-') {
                Some((lo, hi)) => (lo.parse::<i64>().ok()?, hi.parse::<i64>().ok()?),
                None => {
                    let n = piece.parse::<i64>().ok()?;
                    (n, n)
                }
            };
            if lo < 1 || hi < lo {
                return None;
            }
            slices.extend(lo..=hi);
        }
    }
    named.then_some(slices)
}

/// Every entry's slices in an isolated catalogue's listing (`dar -l
/// -Tslicing`, all entries), for the entries named in `paths` and the
/// directories above them, as [`entry_slices`] reads one — WITHOUT the
/// archive's last slice, which the caller adds. One `dar` run for any number
/// of paths (the fill of ADR-0012 item 102 asks for as many as a unit lost).
///
/// `Ok(None)` when the listing does not name every path, or carries a range
/// this cannot read: the caller then reads every slice.
pub fn entries_slices(
    dar_binary: &str,
    catalogue: &Path,
    paths: &[String],
) -> Result<Option<std::collections::BTreeSet<i64>>> {
    let output = super::command(dar_binary)
        .arg("-l")
        .arg(catalogue)
        .arg("-Tslicing")
        .arg("-Q")
        .arg("-N") // no darrc (#412 item 9)
        .stdin(std::process::Stdio::null())
        .output()
        .map_err(|e| TapectlError::Dar(e.to_string()))?;
    if !output.status.success() {
        return Ok(None);
    }
    Ok(slices_of_paths(
        &String::from_utf8_lossy(&output.stdout),
        paths,
    ))
}

/// Every row of a `dar -l -Tslicing` listing: the entry's path (the text
/// after the flag groups and the permission string) and its slices.
fn slicing_rows(
    text: &str,
) -> Option<std::collections::HashMap<&str, std::collections::BTreeSet<i64>>> {
    let mut by_path = std::collections::HashMap::new();
    for line in text.lines() {
        let Some((field, rest)) = line.split_once('\t') else {
            continue;
        };
        if !field
            .chars()
            .all(|c| c.is_ascii_digit() || c == '-' || c == ',')
        {
            continue;
        }
        // `[Saved][ ]       [-L-][     ][ ] -rw-rw-r-- a/b`: skip the bracketed
        // groups, then the 10-character permission string and its space.
        let mut rest = rest.trim_start();
        while rest.starts_with('[') {
            let end = rest.find(']')?;
            rest = rest[end + 1..].trim_start();
        }
        let path = rest.get(11..)?;
        let mut slices = std::collections::BTreeSet::new();
        for piece in field.split(',').filter(|p| !p.is_empty()) {
            let (lo, hi) = match piece.split_once('-') {
                Some((lo, hi)) => (lo.parse::<i64>().ok()?, hi.parse::<i64>().ok()?),
                None => {
                    let n = piece.parse::<i64>().ok()?;
                    (n, n)
                }
            };
            if lo < 1 || hi < lo {
                return None;
            }
            slices.extend(lo..=hi);
        }
        by_path.insert(path, slices);
    }
    Some(by_path)
}

/// [`entries_slices`]'s parser: the union over `paths` and each one's
/// ancestors.
fn slices_of_paths(text: &str, paths: &[String]) -> Option<std::collections::BTreeSet<i64>> {
    let by_path = slicing_rows(text)?;
    let mut needed = std::collections::BTreeSet::new();
    for path in paths {
        needed.extend(by_path.get(path.as_str())?.iter().copied());
        let mut ancestor = path.as_str();
        while let Some((parent, _)) = ancestor.rsplit_once('/') {
            if let Some(s) = by_path.get(parent) {
                needed.extend(s.iter().copied());
            }
            ancestor = parent;
        }
    }
    Some(needed)
}

/// The entries of an isolated catalogue that have data in any of `slices`:
/// the files a damaged slice can have damaged. `Ok(None)` when the listing
/// cannot be read, and the caller must suspect every file.
pub fn entries_in_slices(
    dar_binary: &str,
    catalogue: &Path,
    slices: &std::collections::BTreeSet<i64>,
) -> Result<Option<std::collections::BTreeSet<String>>> {
    let output = super::command(dar_binary)
        .arg("-l")
        .arg(catalogue)
        .arg("-Tslicing")
        .arg("-Q")
        .arg("-N")
        .stdin(std::process::Stdio::null())
        .output()
        .map_err(|e| TapectlError::Dar(e.to_string()))?;
    if !output.status.success() {
        return Ok(None);
    }
    let text = String::from_utf8_lossy(&output.stdout);
    Ok(slicing_rows(&text).map(|rows| {
        rows.into_iter()
            .filter(|(_, s)| s.iter().any(|n| slices.contains(n)))
            .map(|(p, _)| p.to_string())
            .collect()
    }))
}

/// The files dar's report says it could not restore intact: the
/// `Error while restoring <path> : <reason>` lines it prints to standard
/// output (measured on dar 2.7.13: `Bad CRC, data corruption occurred`, exit
/// code 5, every other file still restored). `path` is as dar printed it,
/// under the `-R` root.
pub fn failed_entries(stdout: &str) -> Vec<(String, String)> {
    stdout
        .lines()
        .filter_map(|line| {
            let rest = line.trim().strip_prefix("Error while restoring ")?;
            let (path, reason) = rest.rsplit_once(" : ")?;
            Some((path.trim().to_string(), reason.trim().to_string()))
        })
        .collect()
}

/// Extract several entries of a dar archive: `dar -x -g <path>...`, the
/// direct-mode read [`extract_file_reported`] makes for one. dar reads only
/// the slices they and the directories above them are in.
pub fn extract_files_reported(
    dar_binary: &str,
    archive_base: &Path,
    file_paths: &[String],
    dest: &Path,
) -> (Option<DarReport>, Result<()>) {
    if let Err(e) = std::fs::create_dir_all(dest) {
        return (None, Err(e.into()));
    }
    warn_if_non_root();
    let mut args: Vec<std::ffi::OsString> =
        vec!["-x".into(), archive_base.into(), "-R".into(), dest.into()];
    for path in file_paths {
        args.push("-g".into());
        args.push(path.into());
    }
    args.extend(["-O".into(), "-Q".into(), "-N".into()]);
    run_extract(dar_binary, "dar -x -g", &args, dest)
}

/// Extract a dar archive to a destination directory.
///
/// Fails if dar skipped any file rather than overwriting it — see
/// [`skipped_paths`] for why that is not detectable the obvious way.
pub fn extract(dar_binary: &str, archive_base: &Path, dest: &Path) -> Result<()> {
    extract_reported(dar_binary, archive_base, dest, false).1
}

/// [`extract`], also handing back dar's [`DarReport`] whenever dar ran —
/// on success and on failure alike — for the `restores` row (issue #306).
///
/// `overwrite` (`restore unit --overwrite`, issue #406) adds dar's `-w`:
/// replace a file that already exists at the destination without asking.
/// Without it dar, having no terminal under `-Q`, keeps the old file and
/// [`fail_on_skipped`] turns that into a failure.
pub fn extract_reported(
    dar_binary: &str,
    archive_base: &Path,
    dest: &Path,
    overwrite: bool,
) -> (Option<DarReport>, Result<()>) {
    if let Some(parent) = dest.parent() {
        if let Err(e) = std::fs::create_dir_all(parent) {
            return (None, Err(e.into()));
        }
    }
    if let Err(e) = std::fs::create_dir_all(dest) {
        return (None, Err(e.into()));
    }
    warn_if_non_root();

    let mut args: Vec<std::ffi::OsString> = vec![
        "-x".into(),
        archive_base.into(),
        "-R".into(),
        dest.into(),
        // -O is `--comparison-field` (ignore-owner), NOT "overwrite" -- it
        // tells dar not to consider stored user/group, which is what lets a
        // non-root restore succeed instead of failing to chown to the
        // archived owner. The old `// overwrite` comment here was simply
        // wrong (issue #51).
        "-O".into(),
        "-Q".into(),
        // -N: read no darrc — an `extract:` section in the restoring
        // host's ~/.darrc could otherwise drop files (issue #412 item 9).
        "-N".into(),
    ];
    if overwrite {
        // `-w` (`--no-warn`): overwrite without asking — the real overwrite
        // flag. Verified against dar 2.7.13: an existing file is replaced
        // and no "not restored (user choice)" line is printed.
        args.push("-w".into());
    }
    run_extract(dar_binary, "dar -x", &args, dest)
}

/// Extract a single file from a dar archive.
///
/// Guarded the same way as [`extract`], and it is the sharper end of the
/// same hazard: the operator asked for exactly one file, so a silent skip
/// means the one thing they requested is the one thing they did not get.
pub fn extract_file(
    dar_binary: &str,
    archive_base: &Path,
    file_path: &str,
    dest: &Path,
) -> Result<()> {
    extract_file_reported(dar_binary, archive_base, file_path, dest).1
}

/// [`extract_file`], also handing back dar's [`DarReport`] whenever dar ran
/// — `restore file`'s extract (issue #406), recorded on its `restores` row
/// like `restore unit`'s. `dar -x -g <file_path>` reads the archive but
/// writes only that entry (and the directories above it) under `dest`.
pub fn extract_file_reported(
    dar_binary: &str,
    archive_base: &Path,
    file_path: &str,
    dest: &Path,
) -> (Option<DarReport>, Result<()>) {
    if let Err(e) = std::fs::create_dir_all(dest) {
        return (None, Err(e.into()));
    }
    warn_if_non_root();

    let args: Vec<std::ffi::OsString> = vec![
        "-x".into(),
        archive_base.into(),
        "-R".into(),
        dest.into(),
        "-g".into(),
        file_path.into(),
        // -O is `--comparison-field` (ignore-owner): see `extract`.
        "-O".into(),
        "-Q".into(),
        "-N".into(), // no darrc: see `extract_reported`
    ];
    run_extract(dar_binary, "dar -x -g", &args, dest)
}

/// Test a dar archive integrity.
pub fn test(dar_binary: &str, archive_base: &Path) -> Result<()> {
    super::create::test_archive(dar_binary, archive_base)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    /// Verbatim capture from dar 2.7.13 (`dar -x arch -R dest -O -Q`, one
    /// colliding file present). Kept literal rather than hand-written:
    /// this parser's whole job is to survive dar's real wording, and the
    /// counter that looks authoritative here is the one that stays 0.
    const REAL_DAR_SKIP_OUTPUT: &str = "\
/tmp/dest/collide.txt not restored (user choice)


 --------------------------------------------
 1 inode(s) restored
    including 0 hard link(s)
 0 inode(s) not restored (not saved in archive)
 0 inode(s) not restored (overwriting policy decision)
 1 inode(s) ignored (excluded by filters)
 0 inode(s) failed to restore (filesystem error)
 0 inode(s) deleted
 --------------------------------------------
 Total number of inode(s) considered: 2
";

    #[test]
    fn detects_a_real_dar_skip_and_names_the_path() {
        let skipped = skipped_paths(REAL_DAR_SKIP_OUTPUT);
        assert_eq!(skipped, vec!["/tmp/dest/collide.txt".to_string()]);
    }

    /// The trap that makes this parser necessary: the summary block claims
    /// `0 inode(s) not restored (overwriting policy decision)` even though
    /// a file was demonstrably not restored. Any detector keyed off that
    /// counter reports a clean restore. Pin it so nobody "simplifies" the
    /// parser into reading the summary.
    #[test]
    fn the_overwriting_policy_counter_is_zero_and_must_not_be_trusted() {
        assert!(
            REAL_DAR_SKIP_OUTPUT.contains("0 inode(s) not restored (overwriting policy decision)")
        );
        assert!(
            !skipped_paths(REAL_DAR_SKIP_OUTPUT).is_empty(),
            "a file WAS skipped despite that counter reading 0"
        );
    }

    #[test]
    fn a_clean_restore_reports_nothing_skipped() {
        let clean = " --------------------------------------------\n \
                      2 inode(s) restored\n \
                      0 inode(s) not restored (overwriting policy decision)\n";
        assert!(skipped_paths(clean).is_empty());
    }

    #[test]
    fn several_skips_are_all_reported() {
        let out = "/d/a.txt not restored (user choice)\n\
                   /d/b.txt not restored (user choice)\n \
                   1 inode(s) restored\n";
        assert_eq!(skipped_paths(out), vec!["/d/a.txt", "/d/b.txt"]);
    }

    #[test]
    fn fail_on_skipped_errors_and_names_destination_and_casualties() {
        let err = fail_on_skipped(REAL_DAR_SKIP_OUTPUT.as_bytes(), Path::new("/tmp/dest"))
            .expect_err("a skipped file must fail the restore");
        let msg = err.to_string();
        assert!(
            msg.contains("/tmp/dest"),
            "must name the destination: {msg}"
        );
        assert!(msg.contains("collide.txt"), "must name the casualty: {msg}");
        assert!(
            msg.contains("INCOMPLETE"),
            "must not read as a warning: {msg}"
        );
    }

    #[test]
    fn fail_on_skipped_passes_a_clean_restore() {
        assert!(fail_on_skipped(b" 2 inode(s) restored\n", Path::new("/tmp/dest")).is_ok());
    }

    // ── `-Tslicing` (issue #411) ──

    /// Verbatim from dar 2.7.13, `dar -l <isolated catalogue> -Tslicing -g
    /// a/b/r20.bin -Q`: the entry, and the directories above it.
    const REAL_SLICING_NESTED: &str = "\
Slice(s)|[Data ][D][ EA  ][FSA][Compr][S]|Permission| Filemane
--------+--------------------------------+----------+-----------------------------
1\t [InRef][-]       [-L-][  51%][ ] drwxrwxr-x a
1\t [InRef][-]       [-L-][  51%][ ] drwxrwxr-x a/b
4-5\t [InRef][ ]       [-L-][-----][ ] -rw-rw-r-- a/b/r20.bin
-----
All displayed files have their data in slice range [1,4-5]
-----
";

    #[test]
    fn slicing_listing_gives_the_entry_and_its_directories_slices() {
        let got = parse_slicing_listing(REAL_SLICING_NESTED, "a/b/r20.bin").unwrap();
        assert_eq!(got.into_iter().collect::<Vec<_>>(), vec![1, 4, 5]);
    }

    /// A name with a space and a tab is matched whole, at the end of its row.
    #[test]
    fn slicing_listing_matches_a_name_with_whitespace() {
        let text = "1\t [InRef][-]       [-L-][  51%][ ] drwxrwxr-x a\n\
                    3\t [InRef][ ]       [-L-][-----][ ] -rw-rw-r-- a/sp ace\ttab.txt\n";
        let got = parse_slicing_listing(text, "a/sp ace\ttab.txt").unwrap();
        assert_eq!(got.into_iter().collect::<Vec<_>>(), vec![1, 3]);
    }

    /// A symlink has no data slice (an empty first field); only its
    /// directory's slice is needed.
    #[test]
    fn slicing_listing_takes_an_entry_with_no_data_slice() {
        let text = "1\t [InRef][-]       [-L-][  51%][ ] drwxrwxr-x a\n\
                    \t [InRef][-]       [---][-----][ ] lrwxrwxrwx a/link1\n";
        let got = parse_slicing_listing(text, "a/link1").unwrap();
        assert_eq!(got.into_iter().collect::<Vec<_>>(), vec![1]);
    }

    /// A listing that does not name the entry — dar printed only the header,
    /// as it does for a path not in the archive — is no answer at all, so
    /// the caller reads every slice.
    #[test]
    fn slicing_listing_without_the_entry_is_no_answer() {
        let text = "Slice(s)|[Data ][D][ EA  ][FSA][Compr][S]|Permission| Filemane\n\
                    -----\nAll displayed files have their data in slice range []\n-----\n";
        assert_eq!(parse_slicing_listing(text, "nonexist"), None);
        assert_eq!(
            parse_slicing_listing(REAL_SLICING_NESTED, "a/b/r2.bin"),
            None,
            "a prefix of another name is not that name"
        );
        assert_eq!(parse_slicing_listing("5-3\t x a\n", "a"), None);
    }

    /// The real dar, end to end: a file in the last slices of an archive
    /// lists its slices off the isolated catalogue, the same way the tool
    /// will ask for them.
    #[test]
    fn real_dar_entry_slices_reads_the_isolated_catalogue() {
        let tmp = tempfile::TempDir::new().unwrap();
        let src = tmp.path().join("src");
        std::fs::create_dir_all(src.join("sub")).unwrap();
        let noise: Vec<u8> = (0..200_000u32)
            .map(|i| (i.wrapping_mul(2_654_435_761) >> 13) as u8)
            .collect();
        std::fs::write(src.join("big.bin"), &noise).unwrap();
        std::fs::write(src.join("sub/late.txt"), b"late").unwrap();
        let base = tmp.path().join("arch");
        assert!(Command::new("dar")
            .arg("-c")
            .arg(&base)
            .arg("-R")
            .arg(&src)
            .args(["-s", "64k", "-Q"])
            .output()
            .unwrap()
            .status
            .success());
        let cat = tmp.path().join("cat");
        super::super::create::extract_catalog("dar", &base, &cat).unwrap();
        let got = entry_slices("dar", &cat, "big.bin").unwrap().unwrap();
        assert!(got.len() >= 3, "big.bin spans several slices: {got:?}");
        assert_eq!(entry_slices("dar", &cat, "no/such").unwrap(), None);
    }

    // ── the verbatim report (issue #306) ──

    /// The count comes from the `inode(s) restored` line and ONLY that line:
    /// the real sample has three other `inode(s) ... restored` counters on
    /// the lines around it, and 1 is the one that is right.
    #[test]
    fn inodes_restored_reads_the_one_summary_line() {
        assert_eq!(inodes_restored(REAL_DAR_SKIP_OUTPUT), Some(1));
        assert_eq!(inodes_restored(" 12 inode(s) restored\n"), Some(12));
        assert_eq!(
            inodes_restored(" 0 inode(s) not restored (overwriting policy decision)\n"),
            None,
            "the counter that cannot be trusted is not read as the count"
        );
        assert_eq!(inodes_restored(""), None);
        let killed = DarReport {
            argv: vec!["dar".into()],
            exit_code: None,
            stdout: b"\xff\xfe".to_vec(),
            stderr: Vec::new(),
        };
        assert_eq!(
            killed.inodes_restored(),
            None,
            "not UTF-8: no count, no panic"
        );
    }

    #[test]
    fn argv_json_is_a_json_array_program_first() {
        let r = DarReport {
            argv: vec!["dar".into(), "-x".into(), "/d/a b".into()],
            exit_code: Some(0),
            stdout: Vec::new(),
            stderr: Vec::new(),
        };
        assert_eq!(r.argv_json(), r#"["dar","-x","/d/a b"]"#);
    }

    /// POSITIVE CONTROL for the `restores` row's report columns: a clean
    /// extract by the real `dar` hands back a report whose stdout is
    /// non-empty and carries the summary, so a row with an empty report is
    /// a capture failure and not "dar says nothing on success". And the
    /// verdict is `Ok` — the report is handed back on the success path,
    /// which is the path that kept nothing before.
    ///
    /// Requires `dar`, like `real_dar_collision_is_reported_as_a_failed_restore`
    /// below (issue #43).
    #[test]
    fn real_dar_clean_extract_hands_back_a_non_empty_report() {
        let dar = "dar";
        let tmp = tempfile::TempDir::new().unwrap();
        let src = tmp.path().join("src");
        let dest = tmp.path().join("dest");
        std::fs::create_dir_all(&src).unwrap();
        std::fs::write(src.join("a.txt"), b"A").unwrap();
        std::fs::write(src.join("b.txt"), b"B").unwrap();
        let archive_base = tmp.path().join("arch");
        let created = Command::new(dar)
            .arg("-c")
            .arg(&archive_base)
            .arg("-R")
            .arg(&src)
            .arg("-Q")
            .output()
            .unwrap();
        assert!(created.status.success(), "dar -c failed in test setup");

        let (report, verdict) = extract_reported(dar, &archive_base, &dest, false);
        verdict.expect("a clean extract");
        let report = report.expect("dar ran, so there is a report");
        assert_eq!(report.exit_code, Some(0));
        assert_eq!(report.argv[0], "dar");
        assert_eq!(report.argv[1], "-x");
        assert!(
            report.argv.iter().any(|a| a == "-N"),
            "-N: no darrc may change a restore (#412): {:?}",
            report.argv
        );
        assert!(!report.stdout.is_empty(), "dar's summary is on stdout");
        assert_eq!(
            report.inodes_restored(),
            Some(2),
            "{:?}",
            String::from_utf8_lossy(&report.stdout)
        );
        assert_eq!(std::fs::read(dest.join("a.txt")).unwrap(), b"A");
    }

    /// Issue #412 item 9: the streaming extract (#411) reads no darrc either.
    /// `true` stands in for dar: only the argv it was handed is under test.
    #[test]
    fn the_sequential_extract_passes_dash_n() {
        let tmp = tempfile::TempDir::new().unwrap();
        let x = SequentialExtract::spawn(
            "true",
            &tmp.path().join("cat"),
            &tmp.path().join("dest"),
            false,
            &super::super::slice::hand_made_stream_template(),
        )
        .unwrap();
        let (report, _) = x.finish();
        let argv = report.expect("it ran").argv;
        assert!(argv.iter().any(|a| a == "-N"), "{argv:?}");
    }

    /// #498: the streamed extract, failing, is reported in dar's own words.
    /// tapectl's slices of one dar run, fed as one stream with the catalogue
    /// of ANOTHER run of the same tree: dar refuses ("do not correspond to the
    /// same data", FATAL), exits non-zero, and both the verdict and the
    /// verbatim report carry its message. tapectl's join took every slice it
    /// was given before dar stopped reading: nothing was refused by tapectl.
    #[test]
    fn a_streamed_extract_that_dar_fails_is_reported_in_dars_own_words() {
        use crate::dar::{create, slice};
        let tmp = tempfile::TempDir::new().unwrap();
        let src = tmp.path().join("src");
        std::fs::create_dir_all(src.join("d")).unwrap();
        let noise: Vec<u8> = (0..300_000u32)
            .map(|i| (i.wrapping_mul(2_654_435_761) >> 13) as u8)
            .collect();
        std::fs::write(src.join("d/noise.bin"), &noise).unwrap();
        std::fs::write(src.join("small.txt"), b"small").unwrap();
        // One dar run of the tree: its slices, as `stage create` cuts them,
        // and its catalogue, as staging keeps it.
        let run = |name: &str| {
            let onfly = tmp.path().join(format!("{name}-onfly"));
            let base = tmp.path().join(name);
            let slices = create::archive_to_files(
                &create::DarCreateParams {
                    dar_binary: "dar",
                    source_path: &src,
                    compression: "none",
                    exclude_patterns: &[],
                    exclude_paths: &[],
                    preserve_xattrs: true,
                    preserve_fsa: true,
                    on_fly_catalogue: &onfly,
                },
                &base,
                "64k",
            )
            .unwrap();
            let catalogue = tmp.path().join(format!("{name}-catalogue"));
            create::reisolate_catalogue("dar", &onfly, &catalogue).unwrap();
            let slices: Vec<Vec<u8>> = (1..=slices)
                .map(|n| std::fs::read(format!("{}.{n}.dar", base.display())).unwrap())
                .collect();
            (slices, catalogue)
        };
        let (slices, _) = run("a");
        let (_, other_catalogue) = run("b");
        assert!(slices.len() >= 3, "several slices: {}", slices.len());
        let template = slice::stream_template("dar", &tmp.path().join("stream")).unwrap();
        let dest = tmp.path().join("dest");
        let x = SequentialExtract::spawn("dar", &other_catalogue, &dest, false, &template).unwrap();
        for (i, bytes) in slices.iter().enumerate() {
            let fed = x
                .begin_slice(i as u32 + 1, i + 1 == slices.len())
                .and_then(|()| x.feed(bytes))
                .and_then(|()| x.end_slice());
            match fed {
                Ok(()) => {}
                // dar refused and closed its input: the rest goes nowhere.
                Err(e) if e.kind() == std::io::ErrorKind::BrokenPipe => break,
                Err(e) => panic!("feeding dar: {e}"),
            }
        }
        assert_eq!(x.refused(), None, "tapectl took every slice it was fed");
        let (report, verdict) = x.finish();
        let report = report.expect("dar ran");
        assert!(
            report.exit_code.is_some_and(|c| c != 0),
            "{:?}",
            report.exit_code
        );
        let said = String::from_utf8_lossy(&report.stderr);
        assert!(
            said.contains("do not correspond to the same data"),
            "dar's own words are in its report: {said}"
        );
        let err = verdict.expect_err("dar failed").to_string();
        assert!(
            err.contains("do not correspond to the same data"),
            "and in the verdict: {err}"
        );
    }

    /// The failure path keeps the report too — the whole point (issue #306:
    /// "kept five lines of stderr on failure and nothing on success").
    #[test]
    fn real_dar_collision_still_hands_back_the_report() {
        let dar = "dar";
        let tmp = tempfile::TempDir::new().unwrap();
        let src = tmp.path().join("src");
        let dest = tmp.path().join("dest");
        std::fs::create_dir_all(&src).unwrap();
        std::fs::create_dir_all(&dest).unwrap();
        std::fs::write(src.join("collide.txt"), b"ARCHIVED").unwrap();
        let archive_base = tmp.path().join("arch");
        assert!(Command::new(dar)
            .arg("-c")
            .arg(&archive_base)
            .arg("-R")
            .arg(&src)
            .arg("-Q")
            .output()
            .unwrap()
            .status
            .success());
        std::fs::write(dest.join("collide.txt"), b"STALE").unwrap();

        let (report, verdict) = extract_reported(dar, &archive_base, &dest, false);
        assert!(verdict.is_err(), "a collision is a failed restore");
        let report = report.expect("dar ran");
        assert_eq!(
            report.exit_code,
            Some(0),
            "dar itself exited clean — that is the trap"
        );
        assert!(
            String::from_utf8_lossy(&report.stdout).contains(SKIPPED_MARKER),
            "the evidence is in the verbatim stdout"
        );
    }

    /// `restore unit --overwrite` (issue #406): the same collision, with
    /// `overwrite`, replaces the stale file with the archived one — dar's
    /// `-w` — and is a clean restore. The test above is its control.
    #[test]
    fn real_dar_overwrite_replaces_a_colliding_file() {
        let dar = "dar";
        let tmp = tempfile::TempDir::new().unwrap();
        let src = tmp.path().join("src");
        let dest = tmp.path().join("dest");
        std::fs::create_dir_all(&src).unwrap();
        std::fs::create_dir_all(&dest).unwrap();
        std::fs::write(src.join("collide.txt"), b"ARCHIVED").unwrap();
        let archive_base = tmp.path().join("arch");
        assert!(Command::new(dar)
            .arg("-c")
            .arg(&archive_base)
            .arg("-R")
            .arg(&src)
            .arg("-Q")
            .output()
            .unwrap()
            .status
            .success());
        std::fs::write(dest.join("collide.txt"), b"STALE").unwrap();

        let (report, verdict) = extract_reported(dar, &archive_base, &dest, true);
        verdict.expect("an overwriting restore of a collision is clean");
        let report = report.expect("dar ran");
        assert!(report.argv.iter().any(|a| a == "-w"), "{:?}", report.argv);
        assert!(!String::from_utf8_lossy(&report.stdout).contains(SKIPPED_MARKER));
        assert_eq!(
            std::fs::read(dest.join("collide.txt")).unwrap(),
            b"ARCHIVED"
        );
    }

    /// End-to-end against the real `dar`, because the tests above only
    /// exercise the parser — they would all still pass if `extract` never
    /// called `fail_on_skipped` at all. This one pins the WIRING, which is
    /// the part that actually protects a restore.
    ///
    /// The mhvtl gate does not cover this: its restore legs extract into
    /// fresh destinations, so they never produce a collision.
    ///
    /// Requires `dar`, like the ten staging-pipeline tests it now sits
    /// alongside (issue #43). This test used to carry its own skip guard,
    /// justified as "keeping the ungated suite free of external-binary
    /// requirements" — a premise the #43 audit disproved: the suite already
    /// required dar in ten other places, and CI had been red for months
    /// because of it. `tests/test_dependencies.rs` now asserts the
    /// dependency once, by name; a bespoke skip here would be the only one
    /// of thirteen dar-dependent tests that quietly passes without dar,
    /// which is worse than either consistent policy.
    #[test]
    fn real_dar_collision_is_reported_as_a_failed_restore() {
        use std::process::Stdio;
        let dar = "dar";
        let have_dar = Command::new(dar)
            .arg("--version")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        assert!(
            have_dar,
            "`dar` not on PATH — see tests/test_dependencies.rs, which reports \
             this dependency once with instructions"
        );

        let tmp = tempfile::TempDir::new().unwrap();
        let src = tmp.path().join("src");
        let dest = tmp.path().join("dest");
        std::fs::create_dir_all(&src).unwrap();
        std::fs::create_dir_all(&dest).unwrap();
        std::fs::write(src.join("collide.txt"), b"ARCHIVED").unwrap();
        std::fs::write(src.join("other.txt"), b"other").unwrap();

        let archive_base = tmp.path().join("arch");
        let created = Command::new(dar)
            .arg("-c")
            .arg(&archive_base)
            .arg("-R")
            .arg(&src)
            .arg("-Q")
            .output()
            .unwrap();
        assert!(created.status.success(), "dar -c failed in test setup");

        // Pre-place a STALE copy of one archived file.
        std::fs::write(dest.join("collide.txt"), b"STALE").unwrap();

        let err = extract(dar, &archive_base, &dest)
            .expect_err("a collision must fail the restore, not report success");
        let msg = err.to_string();
        assert!(
            msg.contains("collide.txt"),
            "must name the file that was not restored: {msg}"
        );

        // And prove the failure was warranted: the stale bytes really did
        // survive, which is exactly what dar reported success for before.
        assert_eq!(
            std::fs::read(dest.join("collide.txt")).unwrap(),
            b"STALE",
            "dar left the stale copy in place -- this is the silent data loss being caught"
        );
    }
}
