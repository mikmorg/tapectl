pub mod create;
#[allow(dead_code)]
pub mod restore;
pub mod slice;
pub mod version;

use std::ffi::OsStr;
use std::io::Read;
use std::process::{Child, Command, ExitStatus, Output, Stdio};
use std::time::Duration;

use crate::error::{Result, TapectlError};

/// A `Command` for dar that dies with tapectl (issue #404).
///
/// The child asks the kernel for SIGKILL when its parent goes
/// (`PR_SET_PDEATHSIG`): before this, a `kill -9` of a stage left dar
/// running, still filling staging, with nothing to collect its output. The
/// death signal is tied to the THREAD that spawned the child; every caller
/// waits for dar on the thread that started it ([`Command::output`] or
/// [`run_interruptible`]), so that thread outlives dar. If tapectl died
/// between the fork and the `prctl`, the parent is already someone else,
/// and the child refuses to exec.
pub(crate) fn command(binary: impl AsRef<OsStr>) -> Command {
    use std::os::unix::process::CommandExt;

    let mut cmd = Command::new(binary);
    let parent = std::process::id() as nix::libc::pid_t;
    // SAFETY: the closure runs in the forked child before exec and calls
    // only async-signal-safe functions (`prctl`, `getppid`).
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
                return Err(std::io::Error::other("tapectl exited before dar started"));
            }
            Ok(())
        });
    }
    cmd
}

/// Run `cmd` to completion like [`Command::output`], but stop it when a
/// signal asks tapectl to stop (issue #404): dar is sent SIGTERM, reaped,
/// and this returns [`TapectlError::Interrupted`] with `stopped`'s text.
/// A dar that exits non-zero while a stop is pending — it got the same
/// Ctrl-C from the terminal — is reported the same way, not as a dar
/// failure.
pub(crate) fn run_interruptible(
    cmd: &mut Command,
    stopped: impl FnOnce() -> String,
) -> Result<Output> {
    let mut child = cmd
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| TapectlError::Dar(e.to_string()))?;
    let stdout = drain(child.stdout.take());
    let stderr = drain(child.stderr.take());

    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if crate::signal::is_interrupted() {
            terminate(&mut child);
            let _ = stdout.join();
            let _ = stderr.join();
            return Err(TapectlError::Interrupted(stopped()));
        }
        std::thread::sleep(Duration::from_millis(100));
    };
    let output = Output {
        status,
        stdout: stdout.join().unwrap_or_default(),
        stderr: stderr.join().unwrap_or_default(),
    };
    if !output.status.success() && crate::signal::is_interrupted() {
        return Err(TapectlError::Interrupted(stopped()));
    }
    Ok(output)
}

/// Read a child's pipe to the end on its own thread, so a chatty dar never
/// blocks on a full pipe while the caller polls.
fn drain<R: Read + Send + 'static>(pipe: Option<R>) -> std::thread::JoinHandle<Vec<u8>> {
    std::thread::spawn(move || {
        let mut buf = Vec::new();
        if let Some(mut p) = pipe {
            let _ = p.read_to_end(&mut buf);
        }
        buf
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Issue #404: a child started through [`command`] dies with the
    /// thread that started it. Without the death signal, `sleep 60`
    /// outlives that thread by a minute.
    #[test]
    fn a_dar_command_dies_with_its_parent_thread() {
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let child = command("sleep").arg("60").spawn().unwrap();
            tx.send(child).unwrap();
            // The thread ends here, while `sleep` runs.
        })
        .join()
        .unwrap();
        let mut child = rx.recv().unwrap();
        let started = std::time::Instant::now();
        let status = loop {
            if let Some(status) = child.try_wait().unwrap() {
                break Some(status);
            }
            if started.elapsed() > Duration::from_secs(10) {
                let _ = child.kill();
                let _ = child.wait();
                break None;
            }
            std::thread::sleep(Duration::from_millis(50));
        };
        let status = status.expect("the child outlived the thread that spawned it");
        use std::os::unix::process::ExitStatusExt;
        assert_eq!(status.signal(), Some(nix::libc::SIGKILL), "{status:?}");
    }

    /// Issue #404: a pending stop ends a running child promptly and is
    /// reported as an interruption, not as the child's failure.
    #[test]
    fn run_interruptible_stops_the_child_on_a_signal() {
        crate::signal::interrupt_this_thread(true);
        let started = std::time::Instant::now();
        let err = run_interruptible(command("sleep").arg("60"), || {
            "stopped during the test child".to_string()
        })
        .unwrap_err();
        crate::signal::interrupt_this_thread(false);
        assert!(
            matches!(&err, TapectlError::Interrupted(at) if at == "stopped during the test child"),
            "{err:?}"
        );
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "stopped promptly"
        );

        // Without a stop it is `output()`.
        let out = run_interruptible(
            command("sh").args(["-c", "echo hi; echo err >&2"]),
            || unreachable!(),
        )
        .unwrap();
        assert!(out.status.success());
        assert_eq!(out.stdout, b"hi\n");
        assert_eq!(out.stderr, b"err\n");
    }
}
