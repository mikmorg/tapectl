//! The writer's run: the file list, the command line of
//! `volume-format-v3.md` §3.1 flag by flag, tar on a pipe, and its exit.
//!
//! Nothing here judges the stream. Exit 1 and exit 2 both refuse a staging
//! (§3.2, invariant 7), and so does a stream that fails the stream reader's
//! checks whatever tar's exit was; [`TarStream::finish`] reports, the caller
//! refuses.

use std::ffi::OsString;
use std::io::{self, Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdout, Command, ExitStatus, Stdio};
use std::thread::JoinHandle;
use std::time::Duration;

use crate::error::{Result, TapectlError};

use super::Stderr;

/// The flag that keeps the raw `system.posix_acl_*` record out of the
/// stream (T0.1; ADR-0012 item 296). One argument: the quotes §3.1 shows
/// are the shell's.
pub const XATTRS_EXCLUDE_POSIX_ACL: &str = "--xattrs-exclude=system.posix_acl_*";

/// The pax option fixed by §3.1: extended-header names that do not depend
/// on tar's process id, and no access or change time. One argument.
pub const PAX_OPTION: &str = "--pax-option=exthdr.name=%d/PaxHeaders/%f,delete=atime,delete=ctime";

/// How the file list is spelled in [`recorded_command`]: its real path is a
/// path in the tapectl host's work directory, which tells a reader nothing.
pub const RECORDED_LIST: &str = "<list>";

/// Write the file list for one staging: every name prefixed `./`,
/// NUL-separated (each name followed by a NUL), in the order given, in a
/// new file of mode 0600 in `dir` (§3.1, "The list").
///
/// `dir` is the caller's: the home's work directory, never the staging
/// device, since the names are plaintext. The order is the caller's too
/// (§3.1 sorts the snapshot's entries bytewise; this function keeps what it
/// is given). Each name is relative to the unit's root; an empty name, an
/// absolute one, and one holding a NUL byte are refused, since none can be
/// a member of the unit's stream. The caller removes the file.
pub fn write_list<P: AsRef<Path>>(dir: &Path, entries: &[P]) -> Result<PathBuf> {
    let mut bytes = Vec::new();
    for entry in entries {
        let name = entry.as_ref().as_os_str().as_bytes();
        if name.is_empty() {
            return Err(TapectlError::Tar(
                "the file list for tar cannot hold an empty name".into(),
            ));
        }
        if name[0] == b'/' {
            return Err(TapectlError::Tar(format!(
                "the file list for tar holds names relative to the unit's root, not {:?}",
                entry.as_ref()
            )));
        }
        if name.contains(&0) {
            return Err(TapectlError::Tar(format!(
                "the file list for tar cannot hold a name with a NUL byte: {:?}",
                entry.as_ref()
            )));
        }
        bytes.extend_from_slice(b"./");
        bytes.extend_from_slice(name);
        bytes.push(0);
    }
    let file = tempfile::Builder::new()
        .prefix("tar-list-")
        .permissions(std::os::unix::fs::PermissionsExt::from_mode(0o600))
        .tempfile_in(dir)?;
    let (mut f, path) = file.keep().map_err(|e| TapectlError::Io(e.error))?;
    f.write_all(&bytes)?;
    f.sync_all()?;
    Ok(path)
}

/// What one archive run is given.
#[derive(Debug, Clone, Copy)]
pub struct CreateParams<'a> {
    /// tar, as configured: a bare name is found on `PATH`.
    pub binary: &'a str,
    /// The unit's root: tar runs there (its working directory, so the
    /// argument vector stays §3.1's).
    pub root: &'a Path,
    /// The file list from [`write_list`]. An absolute path, since tar runs
    /// in `root`.
    pub list: &'a Path,
    /// `preserve_xattrs` (D11).
    pub xattrs: bool,
    /// `preserve_acls` (D11).
    pub acls: bool,
}

/// The argument vector after the program name: exactly
/// `volume-format-v3.md` §3.1's, in its order.
///
/// ```text
/// --create --file=- --format=pax --numeric-owner
/// [--xattrs] [--acls] [--xattrs-exclude=system.posix_acl_*]
/// --no-recursion --null --verbatim-files-from --files-from=<list>
/// --pax-option=exthdr.name=%d/PaxHeaders/%f,delete=atime,delete=ctime
/// ```
///
/// `--xattrs` is given with `preserve_xattrs`, `--acls` with
/// `preserve_acls`, and the exclude whenever `--xattrs` is (item 296) and
/// with `--acls` (§3.1's bracket).
///
/// **`preserve_xattrs` off with `preserve_acls` on is refused**, pending a
/// ruling: measured with GNU tar 1.35 on 2026-11-18, `--xattrs-exclude`
/// turns `--xattrs` on, so §3.1's line for that pair (`--acls
/// --xattrs-exclude=…`) stores every `user.*` attribute as
/// `SCHILY.xattr.*`, against §3.2's "with `preserve_xattrs` off the stream
/// simply carries none". Which one stands changes bytes on tape, so it is
/// not chosen here.
pub fn create_args(params: &CreateParams<'_>) -> Result<Vec<OsString>> {
    if !params.list.is_absolute() {
        return Err(TapectlError::Tar(format!(
            "the file list must be named by an absolute path, since tar runs in the \
             unit's root: {}",
            params.list.display()
        )));
    }
    let switches: &[&str] = match (params.xattrs, params.acls) {
        (false, false) => &[],
        (true, false) => &["--xattrs", XATTRS_EXCLUDE_POSIX_ACL],
        (true, true) => &["--xattrs", "--acls", XATTRS_EXCLUDE_POSIX_ACL],
        (false, true) => {
            return Err(TapectlError::Tar(
                "preserve_acls without preserve_xattrs is not supported yet: GNU tar's \
                 --xattrs-exclude turns --xattrs on, so the stream would carry extended \
                 attributes that preserve_xattrs says to leave out (volume-format-v3.md \
                 §3.1 and §3.2 disagree; the ruling is pending)"
                    .into(),
            ))
        }
    };
    let mut args: Vec<OsString> = ["--create", "--file=-", "--format=pax", "--numeric-owner"]
        .iter()
        .map(OsString::from)
        .collect();
    args.extend(switches.iter().map(OsString::from));
    args.extend(
        ["--no-recursion", "--null", "--verbatim-files-from"]
            .iter()
            .map(OsString::from),
    );
    let mut files_from = OsString::from("--files-from=");
    files_from.push(params.list.as_os_str());
    args.push(files_from);
    args.push(OsString::from(PAX_OPTION));
    Ok(args)
}

/// [`super::command`] with [`create_args`], run in the unit's root.
pub fn create_command(params: &CreateParams<'_>) -> Result<Command> {
    let mut cmd = super::command(params.binary);
    cmd.args(create_args(params)?);
    cmd.current_dir(params.root);
    Ok(cmd)
}

/// The command line as text, for the catalog's `tar_command`: the program
/// as configured and each argument quoted, the list written
/// `--files-from=<list>`.
pub fn recorded_command(params: &CreateParams<'_>) -> Result<String> {
    let list_arg = {
        let mut a = OsString::from("--files-from=");
        a.push(params.list.as_os_str());
        a
    };
    let mut parts = vec![format!("{:?}", params.binary)];
    for arg in create_args(params)? {
        if arg == list_arg {
            parts.push(format!("{:?}", format!("--files-from={RECORDED_LIST}")));
        } else {
            parts.push(format!("{arg:?}"));
        }
    }
    Ok(parts.join(" "))
}

/// Start tar for `params`, its stream on a pipe: standard input null,
/// standard output piped, standard error read on a thread into a bounded
/// buffer ([`super::STDERR_KEPT`]).
pub fn spawn_archive(params: &CreateParams<'_>) -> Result<TarStream> {
    let mut cmd = create_command(params)?;
    let tar_command = recorded_command(params)?;
    tracing::info!(command = %tar_command, root = %params.root.display(), "running tar");
    let mut child = cmd
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| spawn_error(params.binary, e))?;
    let stdout = child.stdout.take();
    let stderr = super::drain(child.stderr.take(), super::STDERR_KEPT);
    Ok(TarStream {
        child: Some(child),
        stdout,
        stderr: Some(stderr),
        tar_command,
    })
}

/// The error of a tar that would not start.
pub(super) fn spawn_error(binary: &str, e: io::Error) -> TapectlError {
    if e.kind() == io::ErrorKind::NotFound {
        TapectlError::TarNotFound(binary.to_string())
    } else {
        TapectlError::Tar(format!("cannot run tar ({binary}): {e}"))
    }
}

/// How a tar that ran to the end exited (§3.2; #562).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TarFinish {
    /// Exit 0.
    Clean {
        /// What tar wrote on its standard error (warnings, at exit 0).
        stderr: Stderr,
    },
    /// Exit 1: tar says a file changed as it read it. The staging is
    /// refused.
    FilesChanged {
        /// What tar wrote on its standard error.
        stderr: Stderr,
    },
    /// Exit 2, any other exit, or a signal. The staging is refused.
    Failed {
        /// The exit code, or `None` for a signal.
        code: Option<i32>,
        /// The signal, or `None` for an exit.
        signal: Option<i32>,
        /// What tar wrote on its standard error.
        stderr: Stderr,
    },
}

impl TarFinish {
    /// Map tar's exit status.
    pub fn from_status(status: ExitStatus, stderr: Stderr) -> Self {
        match (status.code(), status.signal()) {
            (Some(0), _) => TarFinish::Clean { stderr },
            (Some(1), _) => TarFinish::FilesChanged { stderr },
            (code, signal) => TarFinish::Failed {
                code,
                signal,
                stderr,
            },
        }
    }

    /// Whether tar exited 0. Anything else refuses a staging.
    pub fn is_clean(&self) -> bool {
        matches!(self, TarFinish::Clean { .. })
    }

    /// What tar wrote on its standard error.
    pub fn stderr(&self) -> &Stderr {
        match self {
            TarFinish::Clean { stderr }
            | TarFinish::FilesChanged { stderr }
            | TarFinish::Failed { stderr, .. } => stderr,
        }
    }

    /// The exit code, `None` after a signal.
    pub fn code(&self) -> Option<i32> {
        match self {
            TarFinish::Clean { .. } => Some(0),
            TarFinish::FilesChanged { .. } => Some(1),
            TarFinish::Failed { code, .. } => *code,
        }
    }
}

/// A running tar: its stream is read from [`TarStream::take_stdout`] as it
/// is written.
///
/// The `Child` is waited on by the thread that holds this value, which
/// spawned it: tar's parent-death signal is tied to that thread
/// ([`super::command`]). Dropping a `TarStream` that was not
/// [`finish`](TarStream::finish)ed stops tar.
pub struct TarStream {
    child: Option<Child>,
    stdout: Option<ChildStdout>,
    stderr: Option<JoinHandle<Stderr>>,
    /// [`recorded_command`] of this run.
    pub tar_command: String,
}

impl TarStream {
    /// tar's standard output: the stream. Taken once.
    pub fn take_stdout(&mut self) -> TarOutput {
        TarOutput {
            inner: self.stdout.take().expect("tar's stdout is taken once"),
        }
    }

    /// The operating-system process id of tar.
    pub fn pid(&self) -> Option<u32> {
        self.child.as_ref().map(Child::id)
    }

    fn stderr(&mut self) -> Stderr {
        self.stderr
            .take()
            .and_then(|h| h.join().ok())
            .unwrap_or_default()
    }

    /// Wait for tar to exit, once its stream has been read to the end, and
    /// report how it exited. A signal to tapectl meanwhile stops tar and is
    /// an error.
    pub fn finish(mut self) -> Result<TarFinish> {
        drop(self.stdout.take());
        let mut child = self.child.take().expect("tar is waited on once");
        let status = loop {
            match child.try_wait() {
                Ok(Some(status)) => break status,
                Ok(None) => {}
                Err(e) => {
                    super::terminate(&mut child);
                    return Err(e.into());
                }
            }
            if crate::signal::is_interrupted() {
                super::terminate(&mut child);
                return Err(TapectlError::Interrupted(
                    "tar was stopped while finishing its stream".into(),
                ));
            }
            std::thread::sleep(Duration::from_millis(20));
        };
        let stderr = self.stderr();
        Ok(TarFinish::from_status(status, stderr))
    }
}

impl Drop for TarStream {
    fn drop(&mut self) {
        drop(self.stdout.take());
        if let Some(mut child) = self.child.take() {
            super::terminate(&mut child);
        }
        let _ = self.stderr();
    }
}

/// How long one wait for tar's output lasts before tapectl checks for a
/// stop.
const STOP_POLL_MS: i32 = 100;

/// The error text a [`TarOutput`] read ends with when tapectl is asked to
/// stop. Not `ErrorKind::Interrupted`, which readers retry.
pub const STOPPED: &str = "stopped by a signal while reading tar's stream";

/// tar's standard output, read so that a signal to tapectl is noticed even
/// while tar writes nothing: each read waits for data at most
/// [`STOP_POLL_MS`] at a time, checking for a stop in between.
pub struct TarOutput {
    inner: ChildStdout,
}

impl Read for TarOutput {
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
                return self.inner.read(buf);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsStr;
    use std::os::unix::fs::PermissionsExt;
    use tempfile::TempDir;

    fn os(v: &[&str]) -> Vec<OsString> {
        v.iter().map(OsString::from).collect()
    }

    fn params<'a>(list: &'a Path, root: &'a Path, xattrs: bool, acls: bool) -> CreateParams<'a> {
        CreateParams {
            binary: "tar",
            root,
            list,
            xattrs,
            acls,
        }
    }

    /// The argument vector, compared whole against §3.1's line for each
    /// pair of switches that is ruled.
    #[test]
    fn create_args_are_section_3_1_line_whole() {
        let list = Path::new("/work/tar-list-x");
        let root = Path::new("/units/u");
        let head = ["--create", "--file=-", "--format=pax", "--numeric-owner"];
        let tail = [
            "--no-recursion",
            "--null",
            "--verbatim-files-from",
            "--files-from=/work/tar-list-x",
            "--pax-option=exthdr.name=%d/PaxHeaders/%f,delete=atime,delete=ctime",
        ];
        let line = |mid: &[&str]| {
            let mut v: Vec<&str> = head.to_vec();
            v.extend_from_slice(mid);
            v.extend_from_slice(&tail);
            os(&v)
        };
        assert_eq!(
            create_args(&params(list, root, true, true)).unwrap(),
            line(&["--xattrs", "--acls", "--xattrs-exclude=system.posix_acl_*"])
        );
        assert_eq!(
            create_args(&params(list, root, true, false)).unwrap(),
            line(&["--xattrs", "--xattrs-exclude=system.posix_acl_*"])
        );
        assert_eq!(
            create_args(&params(list, root, false, false)).unwrap(),
            line(&[])
        );
    }

    /// `preserve_acls` without `preserve_xattrs` is held for a ruling: GNU
    /// tar's `--xattrs-exclude` turns `--xattrs` on (measured), so §3.1's
    /// line for that pair contradicts §3.2.
    #[test]
    fn acls_without_xattrs_is_refused_pending_a_ruling() {
        let err = create_args(&params(Path::new("/w/l"), Path::new("/r"), false, true))
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("preserve_acls without preserve_xattrs"),
            "{err}"
        );
    }

    /// No flag §3.1 or the plan's traps rule out is ever given.
    #[test]
    fn create_args_never_carry_a_ruled_out_flag() {
        for (x, a) in [(true, true), (true, false), (false, false)] {
            let args = create_args(&params(Path::new("/w/l"), Path::new("/r"), x, a)).unwrap();
            for arg in &args {
                let s = arg.to_string_lossy();
                for bad in [
                    "--sparse",
                    "-S",
                    "--one-file-system",
                    "--ignore-failed-read",
                    "--gzip",
                    "--xz",
                    "--zstd",
                    "--bzip2",
                    "--auto-compress",
                    "--use-compress-program",
                    "--directory",
                    "--ignore-zeros",
                ] {
                    assert!(!s.starts_with(bad), "{s} in {args:?}");
                }
            }
        }
    }

    /// A relative list is refused: tar runs in the unit's root.
    #[test]
    fn a_relative_list_is_refused() {
        let err = create_args(&params(Path::new("work/l"), Path::new("/r"), true, true))
            .unwrap_err()
            .to_string();
        assert!(err.contains("absolute path"), "{err}");
    }

    /// The command: the configured program, the vector, the unit's root as
    /// the working directory, and the environment of `tar::command`.
    #[test]
    fn create_command_runs_in_the_root_with_the_vector() {
        let p = params(Path::new("/w/l"), Path::new("/units/u"), true, true);
        let cmd = create_command(&p).unwrap();
        assert_eq!(cmd.get_program(), "tar");
        let args: Vec<OsString> = cmd.get_args().map(OsString::from).collect();
        assert_eq!(args, create_args(&p).unwrap());
        assert_eq!(cmd.get_current_dir(), Some(Path::new("/units/u")));
        let mut envs: Vec<(OsString, Option<OsString>)> = cmd
            .get_envs()
            .map(|(k, v)| (k.to_owned(), v.map(OsStr::to_owned)))
            .collect();
        envs.sort();
        assert_eq!(
            envs,
            vec![
                ("LC_ALL".into(), Some("C".into())),
                ("POSIXLY_CORRECT".into(), None),
                ("TAR_OPTIONS".into(), None),
            ]
        );
    }

    /// The recorded command writes the list as `<list>` and quotes each
    /// word.
    #[test]
    fn recorded_command_names_the_list_by_placeholder() {
        let p = params(
            Path::new("/home/x/work/tar-list-9"),
            Path::new("/u"),
            true,
            true,
        );
        assert_eq!(
            recorded_command(&p).unwrap(),
            "\"tar\" \"--create\" \"--file=-\" \"--format=pax\" \"--numeric-owner\" \
             \"--xattrs\" \"--acls\" \"--xattrs-exclude=system.posix_acl_*\" \
             \"--no-recursion\" \"--null\" \"--verbatim-files-from\" \
             \"--files-from=<list>\" \
             \"--pax-option=exthdr.name=%d/PaxHeaders/%f,delete=atime,delete=ctime\""
        );
    }

    /// The list's bytes: `./` before each name, a NUL after each, in the
    /// order given; mode 0600; in the directory named.
    #[test]
    fn write_list_writes_prefixed_nul_separated_names_mode_0600() {
        let dir = TempDir::new().unwrap();
        let names: Vec<PathBuf> = vec![
            "b".into(),
            "a/c d".into(),
            PathBuf::from(OsStr::from_bytes(b"x\nline\xff")),
            ".tapectl-unit.toml".into(),
        ];
        let path = write_list(dir.path(), &names).unwrap();
        assert_eq!(path.parent(), Some(dir.path()));
        let got = std::fs::read(&path).unwrap();
        assert_eq!(
            got,
            b"./b\0./a/c d\0./x\nline\xff\0./.tapectl-unit.toml\0".to_vec()
        );
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        let empty = write_list::<PathBuf>(dir.path(), &[]).unwrap();
        assert_eq!(std::fs::read(&empty).unwrap(), Vec::<u8>::new());
        assert_ne!(empty, path);
    }

    /// Names that cannot be members are refused.
    #[test]
    fn write_list_refuses_empty_absolute_and_nul_names() {
        let dir = TempDir::new().unwrap();
        for bad in [
            PathBuf::from(""),
            PathBuf::from("/etc/passwd"),
            PathBuf::from(OsStr::from_bytes(b"a\0b")),
        ] {
            assert!(write_list(dir.path(), &[bad.clone()]).is_err(), "{bad:?}");
        }
    }

    /// A tree, its list, and the real tar run on it.
    struct Tree {
        _tmp: TempDir,
        root: PathBuf,
        work: PathBuf,
    }

    fn tree() -> Tree {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().join("root");
        let work = tmp.path().join("work");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::create_dir_all(&work).unwrap();
        Tree {
            _tmp: tmp,
            root,
            work,
        }
    }

    /// Run the real tar over `names` in `root`; the stream and the finish.
    fn run(t: &Tree, names: &[PathBuf], binary: &str) -> (Vec<u8>, TarFinish) {
        let list = write_list(&t.work, names).unwrap();
        let p = CreateParams {
            binary,
            root: &t.root,
            list: &list,
            xattrs: true,
            acls: true,
        };
        let mut s = spawn_archive(&p).unwrap();
        let mut out = Vec::new();
        s.take_stdout().read_to_end(&mut out).unwrap();
        (out, s.finish().unwrap())
    }

    fn member_names(stream: &[u8]) -> Vec<Vec<u8>> {
        let mut a = ::tar::Archive::new(stream);
        a.entries()
            .unwrap()
            .map(|e| e.unwrap().path_bytes().into_owned())
            .collect()
    }

    /// Files named like tar's options are archived as members, never acted
    /// on (#562): the stream's member names are the list, whole.
    #[test]
    fn files_named_like_options_are_members() {
        let t = tree();
        let names: Vec<PathBuf> = vec![
            "--checkpoint=1".into(),
            "-T".into(),
            "--files-from=x".into(),
            "new\nline".into(),
            "plain".into(),
        ];
        for n in &names {
            std::fs::write(t.root.join(n), b"data").unwrap();
        }
        // If `--files-from=x` were acted on, tar would read a list named x.
        std::fs::write(t.root.join("x"), b"./plain\0").unwrap();
        let (stream, finish) = run(&t, &names, "tar");
        assert!(finish.is_clean(), "{finish:?}");
        assert_eq!(
            member_names(&stream),
            vec![
                b"./--checkpoint=1".to_vec(),
                b"./-T".to_vec(),
                b"./--files-from=x".to_vec(),
                b"./new\nline".to_vec(),
                b"./plain".to_vec(),
            ]
        );
    }

    /// Exit 0 is `Clean`, and a folder listed alone is archived alone
    /// (`--no-recursion`). GNU tar names a folder's member with a
    /// trailing `/` (`./d/` for the listed `./d`).
    #[test]
    fn real_tar_on_a_tiny_tree_is_clean() {
        let t = tree();
        std::fs::create_dir(t.root.join("d")).unwrap();
        std::fs::write(t.root.join("d/inside"), b"not listed").unwrap();
        std::fs::write(t.root.join("f"), b"hello").unwrap();
        let (stream, finish) = run(&t, &["d".into(), "f".into()], "tar");
        assert_eq!(finish.code(), Some(0));
        assert!(matches!(finish, TarFinish::Clean { .. }), "{finish:?}");
        assert_eq!(
            member_names(&stream),
            vec![b"./d/".to_vec(), b"./f".to_vec()]
        );
    }

    /// A listed name that does not exist: tar exits 2, which is `Failed`
    /// with tar's own words.
    #[test]
    fn a_listed_name_that_does_not_exist_is_failed() {
        let t = tree();
        std::fs::write(t.root.join("f"), b"hello").unwrap();
        let (_stream, finish) = run(&t, &["f".into(), "missing".into()], "tar");
        match &finish {
            TarFinish::Failed {
                code: Some(2),
                signal: None,
                stderr,
            } => assert!(stderr.text().contains("missing"), "{}", stderr.text()),
            other => panic!("expected Failed with exit 2, got {other:?}"),
        }
        assert!(!finish.is_clean());
    }

    use crate::tar::stand_in;

    /// Spawn through a stand-in, retrying the ETXTBSY a sibling test
    /// thread's fork can cause.
    fn spawn_retrying(p: &CreateParams<'_>) -> TarStream {
        let mut last = None;
        for _ in 0..40 {
            match spawn_archive(p) {
                Err(e) if e.to_string().contains("Text file busy") => {
                    last = Some(e);
                    std::thread::sleep(Duration::from_millis(50));
                }
                r => return r.unwrap(),
            }
        }
        panic!("{:?}", last.unwrap())
    }

    /// Exit 1 is `FilesChanged` (the real tar cannot be made to exit 1 on
    /// demand, so a stand-in exits 1); a signal is `Failed` with the
    /// signal; another exit is `Failed`.
    #[test]
    fn exit_one_is_files_changed_and_a_signal_is_failed() {
        let t = tree();
        let list = write_list(&t.work, &["f"]).unwrap();
        for (name, body, want) in [
            (
                "exit1",
                "echo 'tar: ./f: file changed as we read it' >&2; exit 1",
                (Some(1), None),
            ),
            ("killed", "kill -9 $$", (None, Some(9))),
            ("exit3", "exit 3", (Some(3), None)),
        ] {
            let bin = stand_in(&t.work, name, body);
            let bin = bin.to_str().unwrap();
            let p = CreateParams {
                binary: bin,
                root: &t.root,
                list: &list,
                xattrs: true,
                acls: true,
            };
            let mut s = spawn_retrying(&p);
            let mut out = Vec::new();
            s.take_stdout().read_to_end(&mut out).unwrap();
            let finish = s.finish().unwrap();
            match (&finish, want) {
                (TarFinish::FilesChanged { stderr }, (Some(1), None)) => {
                    assert!(stderr.text().contains("file changed as we read it"))
                }
                (TarFinish::Failed { code, signal, .. }, (c, sig)) => {
                    assert_eq!((*code, *signal), (c, sig), "{name}")
                }
                other => panic!("{name}: {other:?}"),
            }
            assert!(!finish.is_clean());
        }
    }

    /// A tar that is not there is `TarNotFound`, naming it.
    #[test]
    fn a_missing_binary_is_tar_not_found() {
        let t = tree();
        let list = write_list(&t.work, &["f"]).unwrap();
        let p = CreateParams {
            binary: "/nonexistent/tar-for-a-test",
            root: &t.root,
            list: &list,
            xattrs: true,
            acls: true,
        };
        match spawn_archive(&p) {
            Err(TapectlError::TarNotFound(b)) => assert_eq!(b, "/nonexistent/tar-for-a-test"),
            Err(e) => panic!("{e}"),
            Ok(_) => panic!("spawned a missing binary"),
        }
    }

    /// A stop while reading ends the read.
    #[test]
    fn a_stop_ends_the_read() {
        let t = tree();
        let list = write_list(&t.work, &["f"]).unwrap();
        let bin = stand_in(&t.work, "slow", "sleep 5");
        let p = CreateParams {
            binary: bin.to_str().unwrap(),
            root: &t.root,
            list: &list,
            xattrs: true,
            acls: true,
        };
        let mut s = spawn_retrying(&p);
        let mut out = s.take_stdout();
        crate::signal::interrupt_this_thread(true);
        let mut buf = [0u8; 16];
        let err = out.read(&mut buf).unwrap_err();
        let finished = s.finish();
        crate::signal::interrupt_this_thread(false);
        assert_eq!(err.to_string(), STOPPED);
        assert!(matches!(finished, Err(TapectlError::Interrupted(_))));
    }
}
