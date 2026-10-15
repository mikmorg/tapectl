use std::io::{self, Read};
use std::os::fd::AsRawFd;
use std::path::Path;
use std::process::{Child, ChildStdout, Command, Stdio};
use std::thread::JoinHandle;
use std::time::Duration;

use tracing::info;

use crate::error::{Result, TapectlError};

/// Parameters for a dar archive creation.
///
/// There is no archive path: dar writes its archive to standard output
/// (`-c -`) and tapectl frames and encrypts it (`dar::slice`,
/// `staging::files`; issue #370). Nothing here names the staging directory,
/// so dar has no way to write there.
pub struct DarCreateParams<'a> {
    pub dar_binary: &'a str,
    pub source_path: &'a Path,
    pub compression: &'a str,
    pub exclude_patterns: &'a [String],
    pub exclude_paths: &'a [String],
    /// Archive extended attributes (and the POSIX ACLs that are carried as
    /// extended attributes). `false` passes dar `-u "*"`.
    pub preserve_xattrs: bool,
    /// Archive filesystem-specific attributes (Linux chattr flags).
    /// `false` passes dar `--fsa-scope none`.
    pub preserve_fsa: bool,
    /// Where dar writes this archive's isolated catalogue as it runs (`-@`,
    /// on-the-fly isolation; issue #419): a dar base name, so dar writes
    /// `{base}.1.dar`. The catalogue carries this run's data-name label, so
    /// `dar -A` accepts it against this run's slices and no other.
    pub on_fly_catalogue: &'a Path,
}

/// dar's exit code when "some saved files have changed while dar was
/// reading them" (`man dar`, EXIT CODES) — with `--retry-on-change 0`, a
/// file that changed during its read is saved once, marked dirty.
const EXIT_FILES_CHANGED: i32 = 11;

/// The `dar -c -` command for `params`, with dar's `posix_fadvise` a
/// no-op ([`super::command_without_fadvise`], issue #441). The filter is no
/// argument: dar's argv, and the recorded `dar_command`, are what they were.
pub fn create_command(params: &DarCreateParams) -> Command {
    let mut cmd = super::command_without_fadvise(params.dar_binary);
    // The archive goes to standard output (issue #370). dar refuses `-s`
    // there ("Slicing (-s option), is not compatible with archive on
    // standard output"), so the slicing is tapectl's (`dar::slice`).
    cmd.arg("-c").arg("-");
    // No ~/.darrc or /etc/darrc: a `-s` there would break `-c -`, and any
    // other option there would change the archive behind tapectl's back
    // (#412 item 9).
    cmd.arg("-N");
    cmd.arg("-R").arg(params.source_path);

    if params.compression != "none" {
        // dar's `-z` takes an OPTIONAL argument, so getopt only sees it when
        // it is glued to the flag. Passing `-z gzip` as two argv tokens makes
        // dar read the algorithm as a user target and abort with
        // "Given user target(s) could not be found: gzip". Latent until
        // issue #92 made archive-set compression reachable at all.
        cmd.arg(format!("-z{}", params.compression));
    }

    cmd.arg("-an"); // case-insensitive masks
    cmd.arg("-D"); // store excluded dirs as empty
                   // No dar slice hashing (`-3`/`--hash`) — dar hashed every slice but
                   // nothing ever read the `.sha512` files it produced (issues #50/#51).
                   // tapectl's own sha256_plain/sha256_encrypted, computed over the whole
                   // archive before/after encryption, are the real integrity mechanism.
    cmd.arg("-Q"); // quiet (no tty prompt)

    // Extended attributes (issue #347). dar archives every EA by default
    // (EA support is compiled into the system dar), so keeping them needs no
    // flag and dropping them needs the exclusion mask `-u "*"`, which matches
    // every `namespace.name`. On Linux POSIX ACLs ARE extended attributes
    // (`system.posix_acl_*`), so this switch carries them too — which is why
    // `preserve_acls` cannot act on its own (issue #50, `policy::subsumed`).
    //
    // This used to pass `-am` when true and nothing when false. `-am` is
    // `--alter=mask`: it only changes how several -I/-X (and -P/-g, -U/-u)
    // masks combine — ordered, last match wins — and tapectl passes only
    // exclusions, for which both orderings select the same files. It never
    // touched EAs, so both values archived them (issues #50/#51 recorded
    // the fact; #347 made the key honest).
    if !params.preserve_xattrs {
        cmd.arg("-u").arg("*");
    }
    // Filesystem-specific attributes (issue #347). dar's default scope is
    // EVERY FSA family, so passing nothing for false — as this used to —
    // still archived them. `none` is dar's documented "ignore all FSA
    // families". True keeps naming extX (Linux chattr flags), the only
    // family a Linux source carries, so a default archive is byte-for-byte
    // what it was even on a dar built with HFS+ support.
    cmd.arg("--fsa-scope")
        .arg(if params.preserve_fsa { "extX" } else { "none" });

    for pattern in params.exclude_patterns {
        cmd.arg("-X").arg(pattern);
    }
    for path in params.exclude_paths {
        cmd.arg("-P").arg(path);
    }
    // A file that changes while dar reads it is saved once, marked dirty,
    // and dar exits 11, which stage create refuses as DIRTY (ADR-0012,
    // 2026-10-06 amendment item 4). dar's default retries it — on a pipe by
    // appending the file again, so the bytes of every failed attempt would
    // ride to tape as waste.
    cmd.arg("--retry-on-change").arg("0");
    cmd.arg("-@").arg(params.on_fly_catalogue);
    cmd
}

/// `cmd`'s program and arguments in a plain [`super::command`]: the same
/// dar run, without the fadvise filter (issue #441). [`create_command`]
/// sets no environment or directory of its own (`super::command` sets
/// `LC_ALL`, which the plain command sets again), so nothing else is lost.
fn without_fadvise_filter(cmd: &Command) -> Command {
    let mut plain = super::command(cmd.get_program());
    plain.args(cmd.get_args());
    plain
}

/// The command as recorded in `stage_sets.dar_command`, which MANIFEST.toml
/// carries to tape: the on-the-fly catalogue's path is a path on the tapectl
/// host, which says nothing an heir can use, so it is written as
/// `<catalogue>`.
fn recorded_command(cmd: &Command, params: &DarCreateParams) -> String {
    let catalogue = params.on_fly_catalogue.as_os_str();
    let mut parts = vec![format!("{:?}", cmd.get_program())];
    parts.extend(cmd.get_args().map(|a| {
        if a == catalogue {
            "\"<catalogue>\"".to_string()
        } else {
            format!("{a:?}")
        }
    }));
    parts.join(" ")
}

/// A running `dar -c -`: its archive is read from [`DarStream::take_stdout`]
/// as it is produced.
///
/// The `Child` is waited on by the thread that spawned it, which holds this
/// value — dar's parent-death signal is tied to that thread
/// ([`super::command`], issue #404). Dropping a `DarStream` that was not
/// [`finish`](DarStream::finish)ed stops dar.
pub struct DarStream {
    child: Option<Child>,
    stdout: Option<ChildStdout>,
    stderr: Option<JoinHandle<Vec<u8>>>,
    /// `dar --version`'s version line.
    pub dar_version: String,
    /// The command as run, for `stage_sets.dar_command`.
    pub dar_command: String,
}

/// How a `dar -c -` that ran to the end of its archive exited.
#[derive(Debug, PartialEq, Eq)]
pub enum DarFinish {
    /// Exit 0: every file was read unchanged.
    Complete,
    /// Exit 11: at least one file changed while dar read it. The archive
    /// is whole, but the file is saved as it was mid-change.
    FilesChanged {
        /// What dar said about it, a few lines of its stderr.
        detail: String,
    },
}

/// Start `dar -c -` for `params`, its archive on a pipe.
pub fn spawn_archive(params: &DarCreateParams) -> Result<DarStream> {
    let ver = super::version::check(params.dar_binary)?;
    let mut cmd = create_command(params);
    let dar_command = recorded_command(&cmd, params);
    info!(command = %format!("{cmd:?}"), "running dar");
    let spawn = |cmd: &mut Command| {
        cmd.stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
    };
    let mut filtered = super::fadvise_filter_available();
    let mut child = match spawn(&mut cmd) {
        // The filter needs no_new_privs, under which a security module
        // (SELinux, AppArmor) refuses an exec that would change dar's
        // domain, with EPERM. dar runs without the filter then, as it did
        // before issue #441: the filter is never a reason not to stage.
        Err(e) if e.raw_os_error() == Some(nix::libc::EPERM) && filtered => {
            tracing::warn!(
                error = %e,
                "dar could not start under no_new_privs; running it without the \
                 fadvise filter, so it may read the source from disk twice"
            );
            filtered = false;
            spawn(&mut without_fadvise_filter(&cmd))
        }
        spawned => spawned,
    }
    .map_err(|e| TapectlError::Dar(format!("cannot run dar: {e}")))?;
    // The child's own `prctl`s can fail unseen (issue #447): the kernel's
    // record of its seccomp mode is the only report.
    if filtered {
        super::warn_unless_filtered(child.id());
    }
    let stdout = child.stdout.take();
    let stderr = super::drain(child.stderr.take());
    Ok(DarStream {
        child: Some(child),
        stdout,
        stderr: Some(stderr),
        dar_version: ver.full_string,
        dar_command,
    })
}

/// How many bytes process `pid` has read so far (`rchar` in
/// `/proc/<pid>/io`), or `None` where the kernel does not say. For dar this
/// is how far it has read the source, compressed or not.
pub fn bytes_read(pid: u32) -> Option<u64> {
    let io = std::fs::read_to_string(format!("/proc/{pid}/io")).ok()?;
    io.lines()
        .find_map(|l| l.strip_prefix("rchar:"))
        .and_then(|v| v.trim().parse().ok())
}

/// The first few lines of dar's stderr, for an error message.
fn excerpt(stderr: &[u8]) -> String {
    String::from_utf8_lossy(stderr)
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .take(8)
        .collect::<Vec<_>>()
        .join("\n")
}

/// dar's standard output, read so that a signal to tapectl is noticed even
/// while dar writes nothing (issue #404): each read waits for data at most
/// [`STOP_POLL_MS`] at a time, checking for a stop in between. A stop ends
/// the read with [`STOPPED`].
pub struct DarOutput {
    inner: ChildStdout,
}

/// How long one wait for dar's output lasts before tapectl checks for a stop.
const STOP_POLL_MS: i32 = 100;

/// The error text a [`DarOutput`] read ends with when tapectl is asked to
/// stop. Not `ErrorKind::Interrupted`, which readers retry.
pub const STOPPED: &str = "stopped by a signal while reading dar's archive";

impl DarOutput {
    /// A dar's standard output, read with the same stop check as the
    /// archive's ([`super::list`] reads a catalogue listing through it).
    pub(crate) fn over(inner: ChildStdout) -> Self {
        Self { inner }
    }
}

impl Read for DarOutput {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        loop {
            if crate::signal::is_interrupted() {
                return Err(io::Error::other(STOPPED));
            }
            let mut fd = nix::libc::pollfd {
                fd: self.inner.as_raw_fd(),
                events: nix::libc::POLLIN,
                revents: 0,
            };
            // SAFETY: one valid pollfd for an fd this value owns.
            let ready = unsafe { nix::libc::poll(&mut fd, 1, STOP_POLL_MS) };
            if ready < 0 {
                let e = io::Error::last_os_error();
                if e.kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                return Err(e);
            }
            if ready > 0 {
                // Data, or the writer closed the pipe (read returns 0).
                return self.inner.read(buf);
            }
        }
    }
}

impl DarStream {
    /// dar's standard output: the archive. Taken once.
    pub fn take_stdout(&mut self) -> DarOutput {
        DarOutput {
            inner: self.stdout.take().expect("dar's stdout is taken once"),
        }
    }

    /// The operating-system process id of dar.
    pub fn pid(&self) -> Option<u32> {
        self.child.as_ref().map(Child::id)
    }

    fn stderr(&mut self) -> Vec<u8> {
        self.stderr
            .take()
            .and_then(|h| h.join().ok())
            .unwrap_or_default()
    }

    /// Wait for dar to exit, once its archive has been read to the end. A
    /// signal to tapectl meanwhile stops dar (issue #404).
    pub fn finish(self) -> Result<DarFinish> {
        self.finish_reported().0
    }

    /// [`Self::finish`], and dar's report of the run (issue #343): what it
    /// wrote on standard error, verbatim, and how it exited — on every exit.
    pub fn finish_reported(mut self) -> (Result<DarFinish>, DarCreateReport) {
        drop(self.stdout.take());
        let mut child = self.child.take().expect("dar is waited on once");
        let status = loop {
            match child.try_wait() {
                Ok(Some(status)) => break status,
                Ok(None) => {}
                Err(e) => {
                    super::terminate(&mut child);
                    let stderr = self.stderr();
                    let report = self.report(OUTCOME_FAILED, None, stderr);
                    return (Err(e.into()), report);
                }
            }
            if crate::signal::is_interrupted() {
                super::terminate(&mut child);
                let stderr = self.stderr();
                let report = self.report(OUTCOME_STOPPED, None, stderr);
                return (
                    Err(TapectlError::Interrupted(
                        "dar was stopped while finishing its archive".into(),
                    )),
                    report,
                );
            }
            std::thread::sleep(Duration::from_millis(20));
        };
        let stderr = self.stderr();
        let (outcome, result) = match status.code() {
            Some(0) => (OUTCOME_COMPLETE, Ok(DarFinish::Complete)),
            Some(EXIT_FILES_CHANGED) => (
                OUTCOME_FILES_CHANGED,
                Ok(DarFinish::FilesChanged {
                    detail: excerpt(&stderr),
                }),
            ),
            _ if crate::signal::is_interrupted() => (
                OUTCOME_STOPPED,
                Err(TapectlError::Interrupted(
                    "dar was stopped while archiving".into(),
                )),
            ),
            _ => (
                OUTCOME_FAILED,
                Err(TapectlError::Dar(format!(
                    "dar -c failed (exit {status}): {}",
                    excerpt(&stderr)
                ))),
            ),
        };
        let report = self.report(outcome, status.code(), stderr);
        (result, report)
    }

    /// Stop dar because the archive's consumer failed. If dar had already
    /// failed on its own — a short archive is how its failure reaches the
    /// consumer — that failure is returned, as the one to report.
    pub fn abort(self) -> Option<TapectlError> {
        self.abort_reported().0
    }

    /// [`Self::abort`], and dar's report of the run (issue #343).
    pub fn abort_reported(mut self) -> (Option<TapectlError>, DarCreateReport) {
        // Closing the pipe first lets a dar blocked on a write fail with
        // EPIPE instead of waiting out the grace period.
        drop(self.stdout.take());
        let Some(mut child) = self.child.take() else {
            let stderr = self.stderr();
            return (None, self.report(OUTCOME_ABORTED, None, stderr));
        };
        let (failed, code) = match child.try_wait() {
            Ok(Some(status)) if !status.success() => (Some(status), status.code()),
            Ok(Some(status)) => (None, status.code()),
            _ => {
                super::terminate(&mut child);
                (None, None)
            }
        };
        let stderr = self.stderr();
        let error = failed.map(|status| {
            TapectlError::Dar(format!(
                "dar -c failed (exit {status}): {}",
                excerpt(&stderr)
            ))
        });
        let outcome = if error.is_some() {
            OUTCOME_FAILED
        } else {
            OUTCOME_ABORTED
        };
        let report = self.report(outcome, code, stderr);
        (error, report)
    }

    fn report(
        &self,
        outcome: &'static str,
        exit_code: Option<i32>,
        stderr: Vec<u8>,
    ) -> DarCreateReport {
        DarCreateReport {
            outcome,
            command: self.dar_command.clone(),
            exit_code,
            stderr,
            dar_version: self.dar_version.clone(),
        }
    }
}

/// `dar_create_reports.outcome` (migration 034): dar exited 0.
pub const OUTCOME_COMPLETE: &str = "complete";
/// dar exited 11: a file changed while it read it (refused as DIRTY).
pub const OUTCOME_FILES_CHANGED: &str = "files_changed";
/// dar exited with any other code, or could not be waited on.
pub const OUTCOME_FAILED: &str = "failed";
/// The archive's consumer failed and tapectl stopped dar.
pub const OUTCOME_ABORTED: &str = "aborted";
/// A signal to tapectl stopped dar.
pub const OUTCOME_STOPPED: &str = "stopped";

/// What one `dar -c` run reported (issue #343): its standard error verbatim
/// (standard output is the archive), its exit code (`None` when a signal
/// ended it), and how the run ended. Recorded by
/// [`crate::staging::dar_report::record`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DarCreateReport {
    pub outcome: &'static str,
    /// The command as `stage_sets.dar_command`'s dar part records it.
    pub command: String,
    pub exit_code: Option<i32>,
    pub stderr: Vec<u8>,
    pub dar_version: String,
}

impl Drop for DarStream {
    fn drop(&mut self) {
        drop(self.stdout.take());
        if let Some(mut child) = self.child.take() {
            super::terminate(&mut child);
        }
        let _ = self.stderr();
    }
}

/// Test-only: archive `params` into plaintext dar slices `{base}.N.dar` of
/// `slice_size` — the staging pipeline's dar run and framing, minus the
/// encryption — returning the slice count. For tests that read an archive
/// back with `dar -l`/`-x`.
#[cfg(test)]
pub(crate) fn archive_to_files(
    params: &DarCreateParams,
    base: &Path,
    slice_size: &str,
) -> Result<u32> {
    let work = tempfile::tempdir()?;
    let template = super::slice::template(params.dar_binary, slice_size, work.path())?;
    let mut dar = spawn_archive(params)?;
    let stdout = dar.take_stdout();
    let cut = super::slice::cut_stream(
        stdout,
        &template,
        |n| {
            Ok(std::fs::File::create(format!(
                "{}.{n}.dar",
                base.display()
            ))?)
        },
        |_, _| Ok(()),
        |_| Ok(()),
    );
    match cut {
        Ok(summary) => match dar.finish()? {
            DarFinish::Complete => Ok(summary.slices),
            DarFinish::FilesChanged { detail } => Err(TapectlError::Dar(detail)),
        },
        Err(e) => Err(dar.abort().unwrap_or(e)),
    }
}

/// Run dar -t (test archive integrity).
pub fn test_archive(dar_binary: &str, archive_base: &Path) -> Result<()> {
    let output = super::command(dar_binary)
        .arg("-N") // no darrc (issue #412 item 9)
        .arg("-t")
        .arg(archive_base)
        .arg("-Q")
        .output()
        .map_err(|e| TapectlError::Dar(e.to_string()))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(TapectlError::Dar(format!("dar -t failed: {stderr}")));
    }
    Ok(())
}

/// Test-only: isolate the catalogue of the archive at `archive_base` into
/// `catalog_base` (`dar -C … -A …`), for tests that build an archive with
/// dar directly. Staging makes its catalogue on the fly instead
/// ([`reisolate_catalogue`], issue #419).
#[cfg(test)]
pub(crate) fn extract_catalog(
    dar_binary: &str,
    archive_base: &Path,
    catalog_base: &Path,
) -> Result<()> {
    if let Some(parent) = catalog_base.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let output = super::command(dar_binary)
        .arg("-N") // no darrc (issue #412 item 9)
        .arg("-C")
        .arg(catalog_base)
        .arg("-A")
        .arg(archive_base)
        .arg("-Q")
        .output()
        .map_err(|e| TapectlError::Dar(e.to_string()))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(TapectlError::Dar(format!("dar -C failed: {stderr}")));
    }
    Ok(())
}

/// Re-isolate the on-the-fly catalogue at `on_fly_base` (`-@`, which dar
/// always compresses with bzip2 where it can) to an uncompressed one at
/// `catalogue_base` (`dar -C … -A … -znone`) — ADR-0012's 2026-10-06
/// amendment keeps envelope catalogues uncompressed, so an heir's dar needs
/// no bzip2. Isolating an isolated catalogue keeps its data-name label, so
/// the result still rescues (`-A`) the archive it came from (measured,
/// docs/research/2026-10-06-plaintext-free-staging.md §3.4).
pub fn reisolate_catalogue(
    dar_binary: &str,
    on_fly_base: &Path,
    catalogue_base: &Path,
) -> Result<()> {
    let output = super::command(dar_binary)
        // -N: no darrc, whose `isolate:` section could change the catalogue
        // this produces (#412 item 9, #465).
        .arg("-N")
        .arg("-C")
        .arg(catalogue_base)
        .arg("-A")
        .arg(on_fly_base)
        .arg("-znone")
        .arg("-Q")
        .stdin(std::process::Stdio::null())
        .output()
        .map_err(|e| TapectlError::Dar(e.to_string()))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(TapectlError::Dar(format!("dar -C failed: {stderr}")));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn params<'a>(src: &'a Path, on_fly: &'a Path) -> DarCreateParams<'a> {
        DarCreateParams {
            dar_binary: "dar",
            source_path: src,
            compression: "none",
            exclude_patterns: &[],
            exclude_paths: &[],
            preserve_xattrs: true,
            preserve_fsa: true,
            on_fly_catalogue: on_fly,
        }
    }

    /// Issue #370: the archive goes to standard output, a file changing
    /// mid-read is not retried, and the catalogue is isolated on the fly.
    /// No slice size is passed: dar refuses `-s` with `-c -`.
    #[test]
    fn dar_archives_to_stdout_without_retries_or_slicing() {
        let cmd = create_command(&params(Path::new("/src"), Path::new("/home/cat/onfly")));
        let args: Vec<String> = cmd
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert_eq!(&args[..2], ["-c", "-"], "{args:?}");
        let after = |flag: &str| {
            args.iter()
                .position(|a| a == flag)
                .and_then(|i| args.get(i + 1))
                .cloned()
        };
        assert_eq!(after("--retry-on-change").as_deref(), Some("0"));
        assert_eq!(after("-@").as_deref(), Some("/home/cat/onfly"));
        assert_eq!(after("-R").as_deref(), Some("/src"));
        assert!(!args.iter().any(|a| a == "-s"), "{args:?}");
        assert!(args.iter().any(|a| a == "-N"), "{args:?}");
    }

    /// The recorded command (MANIFEST.toml, on tape) names no host path for
    /// the on-the-fly catalogue.
    #[test]
    fn the_recorded_command_hides_the_catalogue_path() {
        let p = params(
            Path::new("/src"),
            Path::new("/home/op/.tapectl/catalogs/ab/s1/x_v1"),
        );
        let rec = recorded_command(&create_command(&p), &p);
        assert!(!rec.contains("/home/op"), "{rec}");
        assert!(rec.contains("\"-@\" \"<catalogue>\""), "{rec}");
        assert!(rec.contains("\"-c\" \"-\" \"-N\""), "{rec}");
    }

    /// Issue #441: dar advises `POSIX_FADV_DONTNEED` over every source file
    /// it opens, evicting what the hasher just read, so dar read the source
    /// from disk a second time. Through dar's create command a child's
    /// `posix_fadvise` is a no-op; through the plain [`super::super::command`]
    /// (the negative control) it evicts the file. dar itself is not run: a
    /// stand-in in its place ignores dar's arguments and makes the call.
    #[test]
    fn the_create_command_keeps_a_childs_fadvise_from_evicting_the_page_cache() {
        let probe = Command::new("python3").arg("--version").output();
        if !matches!(&probe, Ok(o) if o.status.success()) {
            eprintln!("SKIP fadvise filter test: python3 is not on PATH ({probe:?})");
            return;
        }
        let tmp = tempfile::TempDir::new().unwrap();
        let file = tmp.path().join("warm.bin");
        crate::dar::write_warm(&file, 4 << 20);
        let fake_dar = tmp.path().join("fake-dar");
        std::fs::write(
            &fake_dar,
            "#!/usr/bin/env python3\n\
             import os\n\
             fd = os.open(os.environ['TAPECTL_TEST_FADVISE_FILE'], os.O_RDONLY)\n\
             os.posix_fadvise(fd, 0, 0, os.POSIX_FADV_DONTNEED)\n",
        )
        .unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&fake_dar, std::fs::Permissions::from_mode(0o755)).unwrap();
        let run = |mut cmd: Command| {
            let out = cmd
                .env("TAPECTL_TEST_FADVISE_FILE", &file)
                .stdin(Stdio::null())
                .output()
                .unwrap();
            assert!(
                out.status.success(),
                "the stand-in for dar failed: {}",
                String::from_utf8_lossy(&out.stderr)
            );
        };

        let (warmed, pages) = crate::dar::resident_pages(&file);
        if warmed * 4 < pages * 3 {
            eprintln!(
                "SKIP fadvise filter test: only {warmed} of {pages} pages of a file just \
                 read are resident under {}",
                tmp.path().display()
            );
            return;
        }
        // The negative control: without the filter the advice evicts.
        run(super::super::command(&fake_dar));
        let (plain, _) = crate::dar::resident_pages(&file);
        if plain * 4 > pages {
            eprintln!(
                "SKIP fadvise filter test: POSIX_FADV_DONTNEED left {plain} of {pages} \
                 pages resident under {} (a filesystem that keeps them, e.g. tmpfs), so \
                 eviction is not observable here",
                tmp.path().display()
            );
            return;
        }

        if !crate::dar::fadvise_filter_available() {
            eprintln!(
                "SKIP fadvise filter test: this kernel has no seccomp filters, so dar's \
                 create command runs without one (as it did before issue #441)"
            );
            return;
        }
        crate::dar::warm(&file);
        let on_fly = tmp.path().join("onfly");
        let p = DarCreateParams {
            dar_binary: fake_dar.to_str().unwrap(),
            ..params(Path::new("/nonexistent-source"), &on_fly)
        };
        run(create_command(&p));
        let (kept, _) = crate::dar::resident_pages(&file);
        assert!(
            kept * 4 >= pages * 3,
            "through dar's create command the child's DONTNEED must be a no-op: \
             {kept} of {pages} pages resident (plain command: {plain})"
        );
    }

    /// `dar -l -alist-ea` for the archive at `base`: one line per entry,
    /// each followed by its Extended Attribute names; the fourth bracketed
    /// column is the FSA status (`[-L-]` = Linux extX attributes saved,
    /// `[---]` = none).
    fn dar_listing(base: &Path) -> String {
        let out = Command::new("dar")
            .arg("-l")
            .arg(base)
            .args(["-Q", "-alist-ea"])
            .output()
            .expect("dar must be on PATH (tests/test_dependencies.rs)");
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).into_owned()
    }

    /// Archive `src` with the two policy switches, returning the listing.
    fn archive_with(src: &Path, out: &Path, preserve_xattrs: bool, preserve_fsa: bool) -> String {
        std::fs::create_dir_all(out).unwrap();
        let base = out.join("arch");
        let on_fly = out.join("onfly");
        archive_to_files(
            &DarCreateParams {
                preserve_xattrs,
                preserve_fsa,
                ..params(src, &on_fly)
            },
            &base,
            "1G",
        )
        .unwrap();
        dar_listing(&base)
    }

    /// Issue #347: `preserve_xattrs = true` used to add dar's `-am` — mask
    /// ORDERING, unrelated to extended attributes — and `false` left it off,
    /// so extended attributes were archived either way and the key did
    /// nothing. `false` must now drop them; `true` (the positive control)
    /// must keep them.
    #[test]
    fn preserve_xattrs_false_drops_extended_attributes_and_true_keeps_them() {
        let tmp = tempfile::TempDir::new().unwrap();
        let src = tmp.path().join("src");
        std::fs::create_dir_all(&src).unwrap();
        let file = src.join("a.txt");
        std::fs::write(&file, b"hello").unwrap();
        let set = Command::new("setfattr")
            .args(["-n", "user.tapectl_probe", "-v", "1"])
            .arg(&file)
            .status();
        if !matches!(set, Ok(s) if s.success()) {
            eprintln!(
                "SKIP preserve_xattrs behaviour test: setfattr is missing or the temp \
                 filesystem refuses user xattrs ({set:?})"
            );
            return;
        }

        let kept = archive_with(&src, &tmp.path().join("keep"), true, true);
        assert!(
            kept.contains("user.tapectl_probe"),
            "preserve_xattrs = true must archive the attribute:\n{kept}"
        );
        let dropped = archive_with(&src, &tmp.path().join("drop"), false, true);
        assert!(
            !dropped.contains("user.tapectl_probe"),
            "preserve_xattrs = false must not archive the attribute:\n{dropped}"
        );
    }

    /// Issue #347: `preserve_fsa = true` passed `--fsa-scope extX` and
    /// `false` passed nothing — but dar's default scope is every FSA family,
    /// so filesystem-specific attributes were archived either way. `false`
    /// must now pass `--fsa-scope none`; `true` (the positive control) keeps
    /// the extX family.
    #[test]
    fn preserve_fsa_false_drops_filesystem_attributes_and_true_keeps_them() {
        let tmp = tempfile::TempDir::new().unwrap();
        let src = tmp.path().join("src");
        std::fs::create_dir_all(&src).unwrap();
        std::fs::write(src.join("a.txt"), b"hello").unwrap();

        let line = |listing: &str| {
            listing
                .lines()
                .find(|l| l.trim_end().ends_with("a.txt"))
                .unwrap_or_else(|| panic!("a.txt missing from:\n{listing}"))
                .to_string()
        };
        let kept = archive_with(&src, &tmp.path().join("keep"), true, true);
        if !line(&kept).contains("[-L-]") {
            eprintln!(
                "SKIP preserve_fsa behaviour test: this dar or filesystem records no \
                 extX attributes even when asked:\n{kept}"
            );
            return;
        }
        let dropped = archive_with(&src, &tmp.path().join("drop"), true, false);
        assert!(
            line(&dropped).contains("[---]"),
            "preserve_fsa = false must archive no filesystem-specific attributes:\n{dropped}"
        );
    }
}
