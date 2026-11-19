//! GNU tar, the writer of Layout 3's data zone (`volume-format-v3.md` §3.1;
//! ADR-0012 items 253, 270, 273, 274).
//!
//! tapectl gives tar its exact file list and reads the pax stream tar writes
//! on its standard output. This module runs tar: the list
//! ([`writer::write_list`]), the command line, fixed flag by flag
//! ([`writer::create_args`]), the run and its exit ([`writer::spawn_archive`],
//! [`writer::TarFinish`]), and the writer's floor ([`probe::probe`]). It
//! also reads the stream as it passes and checks it (§3.2): the stream
//! reader ([`stream::read`], with the pax records read by length in
//! [`pax`]).
//!
//! Inside this module the `tar` crate is spelled `::tar`, since this module
//! is `crate::tar`.

pub mod pax;
pub mod probe;
pub mod stream;
pub mod writer;

use std::ffi::OsStr;
use std::io::Read;
use std::process::{Child, Command, ExitStatus};
use std::thread::JoinHandle;
use std::time::Duration;

pub use probe::{probe, AclSupport, TarCheck, TarVersion, FLOOR};
pub use stream::{
    read as read_stream, AclOnDisk, Finished, Listed, Refusal, Rule, Rules, Sink, StreamError,
};
pub use writer::{
    create_args, create_command, recorded_command, spawn_archive, write_list, CreateParams,
    TarFinish, TarOutput, TarStream,
};

/// Environment variables removed from every tar tapectl runs (§3.1, "The
/// environment"): `TAR_OPTIONS` is read by GNU tar as if its words were on
/// the command line, before the command line's own, and `POSIXLY_CORRECT`
/// changes how it parses options. Either would make the stream something
/// other than §3.1's line says.
pub const REMOVED_ENV: [&str; 2] = ["TAR_OPTIONS", "POSIXLY_CORRECT"];

/// A `Command` for tar that dies with tapectl, in the C locale, with
/// [`REMOVED_ENV`] removed from its environment.
///
/// The child asks the kernel for SIGKILL when the thread that spawned it
/// goes (`PR_SET_PDEATHSIG`), as the archiver's command did in 1.x (issue #404): a killed
/// stage leaves no tar filling a pipe nobody reads. If tapectl died between
/// the fork and the `prctl`, the child refuses to exec. `LC_ALL=C` keeps
/// tar's messages in English and its name handling byte-wise. Every tar
/// tapectl runs goes through here: the archive, `--version`, and the
/// probe's archive of one file.
pub fn command(binary: impl AsRef<OsStr>) -> Command {
    use std::os::unix::process::CommandExt;

    let mut cmd = Command::new(binary);
    cmd.env("LC_ALL", "C");
    for var in REMOVED_ENV {
        cmd.env_remove(var);
    }
    let parent = std::process::id() as nix::libc::pid_t;
    // SAFETY: the closure runs in the forked child before exec and calls
    // only async-signal-safe functions (`prctl`, `getppid`); it allocates
    // nothing (the error is built from a raw errno).
    unsafe {
        cmd.pre_exec(move || {
            if nix::libc::prctl(
                nix::libc::PR_SET_PDEATHSIG,
                nix::libc::SIGKILL as nix::libc::c_ulong,
            ) != 0
            {
                return Err(std::io::Error::last_os_error());
            }
            if nix::libc::getppid() != parent {
                return Err(std::io::Error::from_raw_os_error(nix::libc::ESRCH));
            }
            Ok(())
        });
    }
    cmd
}

/// How much of tar's standard error is kept. The rest is read and
/// dropped, so tar never blocks on a full pipe.
pub const STDERR_KEPT: usize = 64 * 1024;

/// What tar wrote on its standard error: the first [`STDERR_KEPT`] bytes,
/// and how many more there were.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Stderr {
    /// The bytes kept, from the start.
    pub kept: Vec<u8>,
    /// The bytes read past [`STDERR_KEPT`] and dropped.
    pub dropped: u64,
}

impl Stderr {
    /// The kept text, lossily decoded, with a line saying how much was
    /// dropped when anything was.
    pub fn text(&self) -> String {
        let mut text = String::from_utf8_lossy(&self.kept).into_owned();
        if self.dropped > 0 {
            if !text.ends_with('\n') && !text.is_empty() {
                text.push('\n');
            }
            text.push_str(&format!(
                "[{} more bytes of tar's standard error not kept]",
                self.dropped
            ));
        }
        text
    }
}

/// Read `pipe` to its end on a thread, keeping at most `cap` bytes.
fn drain<R: Read + Send + 'static>(pipe: Option<R>, cap: usize) -> JoinHandle<Stderr> {
    std::thread::spawn(move || {
        let mut out = Stderr::default();
        let Some(mut p) = pipe else {
            return out;
        };
        let mut buf = [0u8; 8192];
        loop {
            match p.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    let room = cap.saturating_sub(out.kept.len());
                    let take = room.min(n);
                    out.kept.extend_from_slice(&buf[..take]);
                    out.dropped += (n - take) as u64;
                }
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                Err(_) => break,
            }
        }
        out
    })
}

/// SIGTERM, a few seconds' grace, then SIGKILL; always reaped.
fn terminate(child: &mut Child) -> Option<ExitStatus> {
    let pid = child.id() as nix::libc::pid_t;
    // SAFETY: `kill` on our own, not yet reaped, child.
    unsafe {
        nix::libc::kill(pid, nix::libc::SIGTERM);
    }
    for _ in 0..50 {
        if let Ok(Some(status)) = child.try_wait() {
            return Some(status);
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    let _ = child.kill();
    child.wait().ok()
}

/// Test-only: an executable `#!/bin/sh` stand-in for tar in `dir`. A
/// script written and then exec'd can hit ETXTBSY while a sibling test
/// thread forks; callers retry on "Text file busy".
#[cfg(test)]
pub(crate) fn stand_in(dir: &std::path::Path, name: &str, body: &str) -> std::path::PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let p = dir.join(name);
    std::fs::write(&p, format!("#!/bin/sh\n{body}\n")).unwrap();
    std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
    p
}

#[cfg(test)]
mod tests {
    use super::*;

    /// tar runs in the C locale with `TAR_OPTIONS` and `POSIXLY_CORRECT`
    /// removed (§3.1): a removal shows in `get_envs` as the key with no
    /// value.
    #[test]
    fn command_sets_the_c_locale_and_removes_tar_options_and_posixly_correct() {
        let cmd = command("tar");
        let envs: Vec<(String, Option<String>)> = cmd
            .get_envs()
            .map(|(k, v)| {
                (
                    k.to_string_lossy().into_owned(),
                    v.map(|v| v.to_string_lossy().into_owned()),
                )
            })
            .collect();
        let mut want = vec![
            ("LC_ALL".to_string(), Some("C".to_string())),
            ("POSIXLY_CORRECT".to_string(), None),
            ("TAR_OPTIONS".to_string(), None),
        ];
        want.sort();
        let mut got = envs.clone();
        got.sort();
        assert_eq!(got, want);
    }

    /// The drain keeps the first `cap` bytes and counts the rest, reading
    /// to the end either way.
    #[test]
    fn drain_keeps_the_first_bytes_and_counts_the_rest() {
        let data: Vec<u8> = (0..100u8).collect();
        let got = drain(Some(std::io::Cursor::new(data.clone())), 30)
            .join()
            .unwrap();
        assert_eq!(got.kept, data[..30].to_vec());
        assert_eq!(got.dropped, 70);
        assert!(got
            .text()
            .ends_with("[70 more bytes of tar's standard error not kept]"));
        let all = drain(Some(std::io::Cursor::new(data.clone())), 1000)
            .join()
            .unwrap();
        assert_eq!(all.kept, data);
        assert_eq!(all.dropped, 0);
    }
}
