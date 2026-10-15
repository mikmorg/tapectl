pub mod create;
pub mod list;
#[allow(dead_code)]
pub mod restore;
pub mod slice;
pub mod version;

use std::ffi::OsStr;
use std::io::Read;
use std::process::{Child, Command, ExitStatus, Output, Stdio};
use std::sync::OnceLock;
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
///
/// It also runs dar in the C locale (`LC_ALL=C`, issue #465): dar's output
/// is parsed in English (`dar::restore`'s skipped-file marker, the exit-11
/// text) and Debian's dar ships French and Swedish translations. gettext
/// ignores `LANGUAGE` under `LC_ALL=C`, so one variable is enough. Every dar
/// spawn goes through here, `create_command` and the rebuilt plain command
/// of its EPERM fallback included.
pub(crate) fn command(binary: impl AsRef<OsStr>) -> Command {
    use std::os::unix::process::CommandExt;

    let mut cmd = Command::new(binary);
    cmd.env("LC_ALL", "C");
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

/// [`command`] for `dar -c`, with every `posix_fadvise` dar makes turned
/// into a no-op that succeeds (issue #441).
///
/// dar advises `POSIX_FADV_DONTNEED` over each source file as it opens it
/// and again as it closes it (strace of dar 2.7.13), and has no option to
/// stop. That evicted the pages the hasher had just read beside it
/// (`staging::validate`, issue #364), so dar read the source from disk a
/// second time — 1.6x to 2.0x the source on vm-desk1. A seccomp filter
/// installed in the child before it execs dar answers dar's `fadvise64`
/// with 0 without running it, and `staging::validate` drops the source's
/// pages itself once both readers have passed them. dar's arguments are
/// untouched.
///
/// It fails open and never fails dar: on a kernel without seccomp filters
/// ([`fadvise_filter_available`]), on an architecture other than x86_64
/// and aarch64, for a dar of another ABI than tapectl's (the filter lets
/// through every call it does not recognise), or when installing it fails
/// in the child, dar runs as it did before, its advice taken. Only `dar -c`
/// reads the source, so only it gets the filter.
pub(crate) fn command_without_fadvise(binary: impl AsRef<OsStr>) -> Command {
    let mut cmd = command(binary);
    if fadvise_filter_available() {
        no_fadvise::install(&mut cmd);
    }
    cmd
}

/// Whether [`command_without_fadvise`] can make dar's `posix_fadvise` a
/// no-op here: probed once per process, and said once in the log.
///
/// The probe is systemd's (`is_seccomp_available`,
/// src/shared/seccomp-util.c): `prctl(PR_SET_SECCOMP, SECCOMP_MODE_FILTER,
/// NULL)` fails with EFAULT where the kernel has seccomp filters — it copies
/// the program in before it checks anything else, so nothing is installed —
/// and with EINVAL where it has none. Any other answer (EPERM from a policy
/// that forbids the call) counts as none.
pub(crate) fn fadvise_filter_available() -> bool {
    static AVAILABLE: OnceLock<bool> = OnceLock::new();
    *AVAILABLE.get_or_init(|| {
        let probed = no_fadvise::probe();
        match &probed {
            Ok(()) => tracing::info!(
                "seccomp filters are available: dar's posix_fadvise is a no-op while it \
                 archives, so the source is read from disk once"
            ),
            Err(why) => tracing::warn!(
                reason = %why,
                "seccomp filters are not available: dar evicts each source file from the \
                 page cache as it reads it, so stage create may read the source from disk \
                 twice"
            ),
        }
        probed.is_ok()
    })
}

/// The seccomp filter of [`command_without_fadvise`].
mod no_fadvise {
    use std::process::Command;

    #[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
    use nix::libc;

    /// `AUDIT_ARCH_X86_64` (linux/audit.h: `EM_X86_64 | __AUDIT_ARCH_64BIT |
    /// __AUDIT_ARCH_LE`, EM_X86_64 = 62 in linux/elf-em.h). The libc crate
    /// has no `AUDIT_ARCH_*`.
    #[cfg(target_arch = "x86_64")]
    const AUDIT_ARCH: u32 = 62 | 0x8000_0000 | 0x4000_0000;
    /// `fadvise64` on x86_64 (asm/unistd_64.h: 221).
    #[cfg(target_arch = "x86_64")]
    const NR_FADVISE64: u32 = libc::SYS_fadvise64 as u32;

    /// `AUDIT_ARCH_AARCH64` (linux/audit.h: `EM_AARCH64 | __AUDIT_ARCH_64BIT |
    /// __AUDIT_ARCH_LE`, EM_AARCH64 = 183 in linux/elf-em.h).
    #[cfg(target_arch = "aarch64")]
    const AUDIT_ARCH: u32 = 183 | 0x8000_0000 | 0x4000_0000;
    /// `fadvise64` on aarch64: asm-generic/unistd.h's `__NR3264_fadvise64`,
    /// 223, which the libc crate does not name for aarch64.
    #[cfg(target_arch = "aarch64")]
    const NR_FADVISE64: u32 = 223;

    #[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
    const LEN: usize = 6;

    /// linux/filter.h's `BPF_STMT`.
    #[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
    fn stmt(code: u32, k: u32) -> libc::sock_filter {
        libc::sock_filter {
            code: code as u16,
            jt: 0,
            jf: 0,
            k,
        }
    }

    /// linux/filter.h's `BPF_JUMP`.
    #[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
    fn jump(code: u32, k: u32, jt: u8, jf: u8) -> libc::sock_filter {
        libc::sock_filter {
            code: code as u16,
            jt,
            jf,
            k,
        }
    }

    /// The filter: a call of tapectl's own architecture to `fadvise64`
    /// returns 0 without running (`SECCOMP_RET_ERRNO` with errno 0); every
    /// other call, and every call of another architecture, runs.
    #[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
    fn program() -> [libc::sock_filter; LEN] {
        use libc::{
            BPF_ABS, BPF_JEQ, BPF_JMP, BPF_K, BPF_LD, BPF_RET, BPF_W, SECCOMP_RET_ALLOW,
            SECCOMP_RET_ERRNO,
        };
        let arch = std::mem::offset_of!(libc::seccomp_data, arch) as u32;
        let nr = std::mem::offset_of!(libc::seccomp_data, nr) as u32;
        [
            stmt(BPF_LD | BPF_W | BPF_ABS, arch),
            // Another architecture: on to the last instruction, ALLOW.
            jump(BPF_JMP | BPF_JEQ | BPF_K, AUDIT_ARCH, 0, 3),
            stmt(BPF_LD | BPF_W | BPF_ABS, nr),
            jump(BPF_JMP | BPF_JEQ | BPF_K, NR_FADVISE64, 0, 1),
            // The errno is 0: the call returns 0, success, and does nothing.
            stmt(BPF_RET | BPF_K, SECCOMP_RET_ERRNO),
            stmt(BPF_RET | BPF_K, SECCOMP_RET_ALLOW),
        ]
    }

    /// See [`super::fadvise_filter_available`].
    #[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
    pub(super) fn probe() -> Result<(), String> {
        // SAFETY: a null program: the kernel fails the call reading it, and
        // installs nothing.
        let r = unsafe {
            libc::prctl(
                libc::PR_SET_SECCOMP,
                libc::SECCOMP_MODE_FILTER as libc::c_ulong,
                0 as libc::c_ulong,
                0 as libc::c_ulong,
                0 as libc::c_ulong,
            )
        };
        let e = std::io::Error::last_os_error();
        match (r, e.raw_os_error()) {
            (r, Some(libc::EFAULT)) if r < 0 => Ok(()),
            (r, _) if r < 0 => Err(format!("prctl(PR_SET_SECCOMP) probe: {e}")),
            _ => Err("prctl(PR_SET_SECCOMP) probe unexpectedly succeeded".into()),
        }
    }

    #[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
    pub(super) fn probe() -> Result<(), String> {
        Err(format!(
            "no filter is built for {}, only x86_64 and aarch64",
            std::env::consts::ARCH
        ))
    }

    /// Install the filter in `cmd`'s child, after [`super::command`]'s own
    /// `pre_exec`.
    #[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
    pub(super) fn install(cmd: &mut Command) {
        use std::os::unix::process::CommandExt;
        // Built here, before the fork, and moved into the closure: the
        // child allocates nothing.
        let mut filter = program();
        // SAFETY: the closure runs in the forked child before exec. It
        // allocates nothing — the program was built before the fork and the
        // `sock_fprog` is on the stack — and calls only `prctl`, a bare
        // system call, async-signal-safe as `command`'s own `prctl` is.
        unsafe {
            cmd.pre_exec(move || {
                let prog = libc::sock_fprog {
                    len: LEN as libc::c_ushort,
                    filter: filter.as_mut_ptr(),
                };
                // An unprivileged process installs a filter only under
                // no_new_privs (seccomp(2)); the kernel refuses
                // PR_SET_NO_NEW_PRIVS unless arguments 3 to 5 are 0.
                if libc::prctl(
                    libc::PR_SET_NO_NEW_PRIVS,
                    1 as libc::c_ulong,
                    0 as libc::c_ulong,
                    0 as libc::c_ulong,
                    0 as libc::c_ulong,
                ) == 0
                {
                    libc::prctl(
                        libc::PR_SET_SECCOMP,
                        libc::SECCOMP_MODE_FILTER as libc::c_ulong,
                        &prog as *const libc::sock_fprog as libc::c_ulong,
                        0 as libc::c_ulong,
                        0 as libc::c_ulong,
                    );
                }
                // Either failing leaves dar as it was, its advice taken:
                // never a reason not to run it.
                Ok(())
            });
        }
    }

    #[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
    pub(super) fn install(_cmd: &mut Command) {}
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

/// Test-only: how much of `path` is in the page cache, as `(resident pages,
/// pages)` (issue #441).
#[cfg(test)]
pub(crate) fn resident_pages(path: &std::path::Path) -> (usize, usize) {
    let map = resident_map(path);
    (map.iter().filter(|r| **r).count(), map.len())
}

/// Test-only: whether each page of `path` is in the page cache —
/// `mincore(2)` over a read-only mapping of it, which faults nothing in
/// (issue #441).
#[cfg(test)]
pub(crate) fn resident_map(path: &std::path::Path) -> Vec<bool> {
    use std::os::fd::AsRawFd;
    let file = std::fs::File::open(path).unwrap();
    let len = file.metadata().unwrap().len() as usize;
    if len == 0 {
        return Vec::new();
    }
    let pages = len.div_ceil(page_size());
    let mut vec = vec![0u8; pages];
    // SAFETY: a fresh read-only shared mapping of a file this function holds
    // open, the length its size; `vec` has one byte per page of it, as
    // `mincore` writes; the mapping is unmapped before returning.
    unsafe {
        let addr = nix::libc::mmap(
            std::ptr::null_mut(),
            len,
            nix::libc::PROT_READ,
            nix::libc::MAP_SHARED,
            file.as_raw_fd(),
            0,
        );
        assert_ne!(addr, nix::libc::MAP_FAILED, "mmap {}", path.display());
        let r = nix::libc::mincore(addr, len, vec.as_mut_ptr());
        let e = std::io::Error::last_os_error();
        nix::libc::munmap(addr, len);
        assert_eq!(r, 0, "mincore {}: {e}", path.display());
    }
    vec.iter().map(|b| b & 1 != 0).collect()
}

/// Test-only: the system's page size.
#[cfg(test)]
pub(crate) fn page_size() -> usize {
    // SAFETY: `sysconf` has no preconditions.
    unsafe { nix::libc::sysconf(nix::libc::_SC_PAGESIZE) as usize }
}

/// Test-only: write `len` bytes to `path`, synced — `POSIX_FADV_DONTNEED`
/// drops clean pages only — and read them back into the page cache.
#[cfg(test)]
pub(crate) fn write_warm(path: &std::path::Path, len: usize) {
    use std::io::Write;
    let data: Vec<u8> = (0..len).map(|i| (i % 251) as u8 + 1).collect();
    let mut f = std::fs::File::create(path).unwrap();
    f.write_all(&data).unwrap();
    f.sync_all().unwrap();
    drop(f);
    warm(path);
}

/// Test-only: read `path` to its end, so its pages are in the page cache.
#[cfg(test)]
pub(crate) fn warm(path: &std::path::Path) {
    let mut f = std::fs::File::open(path).unwrap();
    std::io::copy(&mut f, &mut std::io::sink()).unwrap();
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

    /// Issue #465 items 1 and 2: every way tapectl starts dar reads no darrc
    /// (`-N`: a host's `~/.darrc` or `/etc/darrc` with an `all:`, `create:`
    /// or `isolate:` section would otherwise change slice sizes or the
    /// catalogue, #412 item 9) and runs it in the C locale (`LC_ALL=C`: dar's
    /// output is parsed in English and Debian ships fr and sv translations).
    ///
    /// A stand-in for dar records its environment and arguments, so each
    /// public function that spawns dar is run for real and its command line
    /// read back, not inferred from its source. `create_command` is one of
    /// them, through `spawn_archive`.
    #[test]
    fn every_dar_invocation_reads_no_darrc_and_runs_in_the_c_locale() {
        let tmp = tempfile::TempDir::new().unwrap();
        let log = tmp.path().join("invocations.log");
        let fake = tmp.path().join("dar");
        std::fs::write(
            &fake,
            format!(
                "#!/bin/sh\n\
                 {{ printf 'LC_ALL=%s' \"$LC_ALL\"; for a in \"$@\"; do printf '\\t%s' \"$a\"; done; \
                 printf '\\n'; }} >> '{}'\n\
                 for a in \"$@\"; do [ \"$a\" = --version ] && echo 'dar version 2.7.13, Copyright'; done\n\
                 exit 0\n",
                log.display()
            ),
        )
        .unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
        let dar = fake.to_str().unwrap();
        let (a, b) = (tmp.path().join("a"), tmp.path().join("b"));
        let dest = tmp.path().join("dest");

        // Results are not the point (a stand-in dar archives nothing); the
        // command lines are.
        let _ = version::check(dar);
        let _ = version::capabilities(dar);
        let _ = slice::template(dar, "1M", &tmp.path().join("work"));
        let src = tmp.path().join("src");
        std::fs::create_dir_all(&src).unwrap();
        let params = create::DarCreateParams {
            dar_binary: dar,
            source_path: &src,
            compression: "none",
            exclude_patterns: &[],
            exclude_paths: &[],
            preserve_xattrs: true,
            preserve_fsa: true,
            on_fly_catalogue: &tmp.path().join("onfly"),
        };
        // Read to its end so the stand-in has logged before dar is reaped.
        let mut stream = create::spawn_archive(&params).unwrap();
        let _ = stream.take_stdout().read_to_end(&mut Vec::new());
        let _ = stream.finish();
        let _ = create::test_archive(dar, &a);
        let _ = create::extract_catalog(dar, &a, &b);
        let _ = create::reisolate_catalogue(dar, &a, &b);
        let _ = restore::extract(dar, &a, &dest);
        let _ = restore::extract_reported(dar, &a, &dest, true);
        let _ = restore::extract_file(dar, &a, "x/y", &dest);
        let _ = restore::entry_slices(dar, &a, "x/y");
        if let Ok(seq) = restore::SequentialExtract::spawn(dar, &a, &b, &dest, true) {
            let _ = seq.finish();
        }
        let _ = list::regular_files(dar, &a, &mut |_| {});

        let log = std::fs::read_to_string(&log).unwrap();
        let runs: Vec<Vec<&str>> = log.lines().map(|l| l.split('\t').collect()).collect();
        // check, capabilities, template, spawn_archive (version, create),
        // test_archive, extract_catalog, reisolate_catalogue, extract,
        // extract_reported, extract_file, entry_slices, SequentialExtract,
        // regular_files.
        assert_eq!(runs.len(), 14, "every dar spawn ran once: {log}");
        for run in &runs {
            assert_eq!(run[0], "LC_ALL=C", "{run:?}");
            assert!(run[1..].contains(&"-N"), "no -N: {run:?}");
        }
    }

    /// Issue #465 item 2: `command` sets the locale itself, over whatever
    /// the operator's environment says.
    #[test]
    fn a_dar_command_sets_lc_all_to_c() {
        let cmd = command("dar");
        let lc_all: Vec<_> = cmd
            .get_envs()
            .filter(|(k, _)| *k == "LC_ALL")
            .map(|(_, v)| v)
            .collect();
        assert_eq!(lc_all, [Some(OsStr::new("C"))]);
        assert!(
            create::create_command(&create::DarCreateParams {
                dar_binary: "dar",
                source_path: std::path::Path::new("/src"),
                compression: "none",
                exclude_patterns: &[],
                exclude_paths: &[],
                preserve_xattrs: true,
                preserve_fsa: true,
                on_fly_catalogue: std::path::Path::new("/onfly"),
            })
            .get_envs()
            .any(|(k, v)| k == "LC_ALL" && v == Some(OsStr::new("C"))),
            "the create command goes through the same constructor"
        );
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
