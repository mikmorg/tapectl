//! `config check` depth checks (issue #62): does the config actually work,
//! not just parse? Every check here is advisory — it reports, it never
//! fails `config check`'s exit code, and it never opens a tape device (only
//! `Path::exists` on the configured device paths).
//!
//! Following the [`crate::policy::subsumed`] pattern, I/O-performing `check_*`
//! functions are kept separate from pure `describe_*` functions so the text
//! and `--json` arms of `config check` can never drift, and so the wording
//! is unit-testable without touching a filesystem.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};

use crate::config::Config;

/// Result of probing the configured tar (`[tar] binary`; ADR-0012 item
/// 274): the writer's floor is GNU tar 1.35 built with ACL support.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TarToolCheck {
    /// No tar at the configured path, or none of that name on `PATH`.
    Missing { path: String },
    /// It could not be run or probed.
    Unreadable { path: String, detail: String },
    /// It ran and `stage create` would refuse it: the probe's refusal text.
    Refused {
        path: String,
        first_line: String,
        refusal: String,
    },
    /// It ran and meets the floor.
    Ok {
        path: String,
        first_line: String,
        acl: String,
    },
}

/// Walk `path_var` (a `PATH`-style, `:`-separated list of directories) for
/// the first entry where `<dir>/<binary>` exists and is executable — the
/// same question `Command::new(binary)` answers internally via `execvp`,
/// pulled out as a pure function so it is testable with an explicit `PATH`
/// string rather than mutating the process environment (issue #119: the
/// crate's unit tests run in parallel in one process, so `set_var` would be
/// a footgun here).
///
/// An empty `PATH` segment (e.g. a leading/trailing/doubled `:`) traditionally
/// means "search the current directory" in POSIX `PATH` semantics, but that
/// makes resolution depend on the caller's cwd — surprising for a config
/// checker — so empty segments are skipped here rather than honored.
pub fn resolve_on_path(binary: &str, path_var: &OsStr) -> Option<PathBuf> {
    for dir in std::env::split_paths(path_var) {
        if dir.as_os_str().is_empty() {
            continue;
        }
        let candidate = dir.join(binary);
        if !candidate.is_file() {
            continue;
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let executable = std::fs::metadata(&candidate)
                .map(|m| m.permissions().mode() & 0o111 != 0)
                .unwrap_or(false);
            if !executable {
                continue;
            }
        }
        return Some(candidate);
    }
    None
}

/// Probe `binary` as `stage create` does (`tar::probe`): found on `PATH`
/// when it is a bare name, its first `--version` line, whether it is GNU
/// tar at or above the floor, and whether it stores an ACL by number,
/// found by archiving one file with an ACL in a directory made in
/// `scratch` (removed after). The refusal is the probe's own text, so
/// `config check`, `init` and `stage create` say the same thing.
pub fn check_tar(binary: &str, scratch: &Path) -> TarToolCheck {
    match crate::tar::probe(binary, scratch) {
        Err(crate::error::TapectlError::TarNotFound(_)) => TarToolCheck::Missing {
            path: binary.to_string(),
        },
        Err(e) => TarToolCheck::Unreadable {
            path: binary.to_string(),
            detail: e.to_string(),
        },
        Ok(check) => {
            let path = check.path.to_string_lossy().into_owned();
            match check.refusal() {
                Some(refusal) => TarToolCheck::Refused {
                    path,
                    first_line: check.first_line,
                    refusal,
                },
                None => TarToolCheck::Ok {
                    path,
                    first_line: check.first_line,
                    acl: check.acl.to_string(),
                },
            }
        }
    }
}

/// The advisory line for a tar check. Pure — testable by constructing
/// [`TarToolCheck`] variants directly, no filesystem or subprocess involved.
pub fn describe_tar(check: &TarToolCheck) -> String {
    let (major, minor) = crate::tar::FLOOR;
    match check {
        TarToolCheck::Missing { path } => format!(
            "warning: tar not found at '{path}' — config.tar.binary points nowhere; \
             staging will fail until this is corrected (install GNU tar {major}.{minor} or \
             later built with ACL support, or set [tar] binary to its path)"
        ),
        TarToolCheck::Unreadable { path, detail } => {
            format!("warning: tar at '{path}' could not be probed: {detail}")
        }
        TarToolCheck::Refused { refusal, .. } => {
            format!("warning: stage create would refuse this tar: {refusal}")
        }
        TarToolCheck::Ok {
            path, first_line, ..
        } => format!(
            "tar: {first_line} at '{path}' (meets the writer's floor, GNU tar {major}.{minor} \
             with ACL support)"
        ),
    }
}

/// Result of probing the staging directory for existence and writability.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StagingCheck {
    Missing { path: String },
    NotWritable { path: String, detail: String },
    Writable { path: String },
}

/// Probe `dir`: does it exist, and can a file actually be created and
/// removed in it? Writability is tested by doing the write, not by
/// inspecting permission bits — mode bits lie under root, ACLs, and
/// read-only mounts.
pub fn check_staging(dir: &str) -> StagingCheck {
    let path = Path::new(dir);
    if !path.exists() {
        return StagingCheck::Missing {
            path: dir.to_string(),
        };
    }

    let probe = path.join(format!(".tapectl-config-check-{}", std::process::id()));
    match std::fs::write(&probe, b"tapectl config check probe") {
        Ok(()) => {
            let _ = std::fs::remove_file(&probe);
            StagingCheck::Writable {
                path: dir.to_string(),
            }
        }
        Err(e) => StagingCheck::NotWritable {
            path: dir.to_string(),
            detail: e.to_string(),
        },
    }
}

/// The advisory line for a staging check. Pure.
pub fn describe_staging(check: &StagingCheck) -> String {
    match check {
        StagingCheck::Missing { path } => {
            format!("warning: staging directory '{path}' does not exist")
        }
        StagingCheck::NotWritable { path, detail } => {
            format!("warning: staging directory '{path}' is not writable: {detail}")
        }
        StagingCheck::Writable { path } => format!("staging: '{path}' exists and is writable"),
    }
}

/// Whether the staging filesystem can hold one full cartridge (issue #140).
///
/// The THIRD staging check, beside existence and writability. Those two ask
/// whether staging works at all; this asks whether it is big enough for the
/// job, which is a different failure and a much later one — `stage create`
/// fills staging with encrypted slices before anything
/// reaches tape, so an undersized staging directory fails PARTWAY through a
/// long archive rather than at the start.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StagingSpaceCheck {
    /// Free space is at least one cartridge.
    Sufficient {
        path: String,
        free_bytes: u64,
        tape_bytes: u64,
    },
    /// Free space is less than one cartridge.
    Tight {
        path: String,
        free_bytes: u64,
        tape_bytes: u64,
        /// The generation the figure came from, for the message.
        generation: String,
    },
    /// The filesystem could not be interrogated (staging missing, or a
    /// `statvfs` failure). Silent: the existence/writability checks already
    /// say what is wrong, and repeating it is noise.
    Unknown,
}

/// Free bytes on the filesystem holding `dir`, or `None` if it cannot be
/// interrogated.
///
/// `blocks_available` (not `blocks_free`) and `fragment_size` (not
/// `block_size`): `blocks_available` excludes the reserved-for-root blocks
/// an unprivileged `stage create` cannot use, and POSIX defines the block
/// counts in units of `f_frsize`. Using `f_bsize` — the "preferred I/O
/// size" — happens to give the same answer on ext4 and is wrong in general.
fn free_bytes(dir: &Path) -> Option<u64> {
    let stat = nix::sys::statvfs::statvfs(dir).ok()?;
    Some(stat.blocks_available() as u64 * stat.fragment_size())
}

/// Compare staging's free space against the largest configured drive's
/// planning capacity.
///
/// The LARGEST of several drives, deliberately: the operator can stage for
/// any of them, and warning against the smallest would clear a directory
/// that cannot hold the tape they are about to write.
///
/// Skipped silently (`Unknown`) when no backend is configured — there is no
/// cartridge size to compare against, and inventing one would be a guess.
/// Uses `planning_capacity_bytes(None)`, the ADR-0010 "no cartridge is
/// loaded" figure, which is exactly the situation `config check` is in.
pub fn check_staging_space(config: &Config) -> StagingSpaceCheck {
    let dir = &config.staging.directory;
    let Some(biggest) = config
        .backends
        .lto
        .iter()
        .filter_map(|b| b.planning_capacity_bytes(None).ok().map(|c| (c, b)))
        .max_by_key(|(c, _)| *c)
    else {
        return StagingSpaceCheck::Unknown;
    };
    let (tape_bytes, backend) = biggest;
    let Some(free_bytes) = free_bytes(Path::new(dir)) else {
        return StagingSpaceCheck::Unknown;
    };
    if free_bytes >= tape_bytes {
        StagingSpaceCheck::Sufficient {
            path: dir.clone(),
            free_bytes,
            tape_bytes,
        }
    } else {
        StagingSpaceCheck::Tight {
            path: dir.clone(),
            free_bytes,
            tape_bytes,
            generation: backend.generation.clone(),
        }
    }
}

/// The advisory line for a staging-space check, or `None` when there is
/// nothing to say. Pure.
///
/// Advisory only, like every other line here: it never changes `config
/// check`'s exit code (ADR-0004). A tight staging directory is a real
/// constraint, not an invalid config — an operator who archives one unit at
/// a time is fine with it.
pub fn describe_staging_space(check: &StagingSpaceCheck) -> Option<String> {
    match check {
        StagingSpaceCheck::Unknown => None,
        StagingSpaceCheck::Sufficient {
            path,
            free_bytes,
            tape_bytes,
        } => Some(format!(
            "staging: '{path}' has {} free — enough for one {} cartridge",
            decimal_bytes(*free_bytes),
            decimal_bytes(*tape_bytes),
        )),
        StagingSpaceCheck::Tight {
            path,
            free_bytes,
            tape_bytes,
            generation,
        } => Some(format!(
            "warning: staging '{path}' has {} free; one {generation} cartridge is up to {} — \
             a tape filled in one session will not fit: stage create stops before tar \
             and asks (or refuses) when a unit will not fit",
            decimal_bytes(*free_bytes),
            decimal_bytes(*tape_bytes),
        )),
    }
}

/// Render bytes in DECIMAL units (kB/MB/GB/TB), matching how tape
/// generations are marketed and how `media::Generation` stores them
/// (LTO-6 = 2_500_000_000_000). Using binary units here would render a
/// 2.5 TB cartridge as "2.3 TiB" and invite the operator to think tapectl
/// had mis-read the drive. The crate's one decimal formatter (issue #363:
/// this was a private copy that printed "2.5 TB"/"kB" where every other
/// capacity line prints "2.50 TB"/"KB").
fn decimal_bytes(bytes: u64) -> String {
    crate::util::format_bytes_decimal(i64::try_from(bytes).unwrap_or(i64::MAX))
}

/// Existence of one backend's configured device paths. A mild, informational
/// note, not a warning — a tape device legitimately does not exist when the
/// drive isn't attached, which is the normal state on a dev VM. **Only
/// `Path::exists` is used; the device is never opened.**
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TapeDeviceCheck {
    pub backend_name: String,
    pub device_tape: String,
    pub device_tape_exists: bool,
    pub device_sg: String,
    pub device_sg_exists: bool,
    /// Issue #329: `Some` when sysfs PROVES `device_sg` is not the drive
    /// `device_tape` names (read from sysfs only, never the device).
    pub pairing_problem: Option<String>,
}

/// Probe every configured LTO backend's device paths for existence only.
pub fn scan_tape_devices(config: &Config) -> Vec<TapeDeviceCheck> {
    config
        .backends
        .lto
        .iter()
        .map(|b| TapeDeviceCheck {
            backend_name: b.name.clone(),
            device_tape: b.device_tape.clone(),
            device_tape_exists: Path::new(&b.device_tape).exists(),
            device_sg: b.device_sg.clone(),
            device_sg_exists: Path::new(&b.device_sg).exists(),
            pairing_problem: crate::tape::drive_identity::sg_pairing_problem(b),
        })
        .collect()
}

/// The advisory line for one backend's device check. Pure. Deliberately
/// phrased as a mild note ("not attached"), never "warning" — absence is
/// the normal state when a drive isn't plugged in.
pub fn describe_tape_device(check: &TapeDeviceCheck) -> String {
    // Issue #329: unlike absence, a PROVEN wrong pairing is a warning — a
    // write through this backend is refused until it is fixed.
    if let Some(problem) = &check.pairing_problem {
        return format!("warning: {problem} (volume write refuses this drive until then)");
    }
    let mut missing = Vec::new();
    if !check.device_tape_exists {
        missing.push(check.device_tape.as_str());
    }
    if !check.device_sg_exists {
        missing.push(check.device_sg.as_str());
    }
    if missing.is_empty() {
        format!(
            "drive \"{}\": device_tape and device_sg both present",
            check.backend_name
        )
    } else {
        format!(
            "note: drive \"{}\" device path(s) not present: {} — normal if the drive isn't attached",
            check.backend_name,
            missing.join(", ")
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    // -- describe_tar: pure, no filesystem --

    #[test]
    fn describe_tar_missing_names_the_path_and_the_floor() {
        let line = describe_tar(&TarToolCheck::Missing {
            path: "/usr/local/bin/tar".to_string(),
        });
        assert!(line.contains("/usr/local/bin/tar"));
        assert!(line.contains("not found"));
        assert!(line.contains("GNU tar 1.35"), "{line}");
    }

    #[test]
    fn describe_tar_refused_carries_the_probes_text() {
        let line = describe_tar(&TarToolCheck::Refused {
            path: "/usr/bin/tar".to_string(),
            first_line: "tar (GNU tar) 1.30".to_string(),
            refusal: "GNU tar 1.30 at /usr/bin/tar is below the writer's floor".to_string(),
        });
        assert!(line.contains("would refuse"));
        assert!(line.contains("below the writer's floor"));
    }

    #[test]
    fn describe_tar_ok_reports_its_version() {
        let line = describe_tar(&TarToolCheck::Ok {
            path: "/usr/bin/tar".to_string(),
            first_line: "tar (GNU tar) 1.35".to_string(),
            acl: "Yes".to_string(),
        });
        assert!(line.contains("tar (GNU tar) 1.35") && line.contains("/usr/bin/tar"));
        assert!(!line.contains("warning"));
    }

    // -- describe_staging: pure --

    #[test]
    fn describe_staging_writable_is_not_alarming() {
        let line = describe_staging(&StagingCheck::Writable {
            path: "/mnt/staging".to_string(),
        });
        assert!(!line.contains("warning"));
    }

    #[test]
    fn describe_staging_not_writable_includes_detail() {
        let line = describe_staging(&StagingCheck::NotWritable {
            path: "/mnt/staging".to_string(),
            detail: "Permission denied (os error 13)".to_string(),
        });
        assert!(line.contains("Permission denied"));
    }

    // -- describe_tape_device: pure, mild wording --

    /// Issue #329: a proven pairing problem is the line's whole content,
    /// and it is a warning (a write refuses the backend).
    #[test]
    fn describe_tape_device_names_a_proven_pairing_problem_as_a_warning() {
        let line = describe_tape_device(&TapeDeviceCheck {
            backend_name: "lto1".to_string(),
            device_tape: "/dev/tape/by-id/x-nst".to_string(),
            device_tape_exists: true,
            device_sg: "/dev/sg-wrong".to_string(),
            device_sg_exists: true,
            pairing_problem: Some("drive \"lto1\": device_sg = /dev/sg-wrong ...".into()),
        });
        assert!(line.starts_with("warning: "), "{line}");
        assert!(line.contains("/dev/sg-wrong"), "{line}");
        assert!(line.contains("volume write refuses"), "{line}");
    }

    #[test]
    fn describe_tape_device_absent_is_a_note_not_a_warning() {
        let line = describe_tape_device(&TapeDeviceCheck {
            backend_name: "lto1".to_string(),
            device_tape: "/dev/nst0".to_string(),
            device_tape_exists: false,
            device_sg: "/dev/sg0".to_string(),
            device_sg_exists: false,
            pairing_problem: None,
        });
        assert!(!line.to_lowercase().contains("warning"));
        assert!(line.contains("/dev/nst0"));
        assert!(line.contains("/dev/sg0"));
        assert!(line.contains("isn't attached"));
    }

    #[test]
    fn describe_tape_device_both_present_says_so() {
        let line = describe_tape_device(&TapeDeviceCheck {
            backend_name: "lto1".to_string(),
            device_tape: "/dev/nst0".to_string(),
            device_tape_exists: true,
            device_sg: "/dev/sg0".to_string(),
            device_sg_exists: true,
            pairing_problem: None,
        });
        assert!(line.contains("both present"));
    }

    // -- check_dar / check_staging / scan_tape_devices: I/O, exercised with
    // tempfiles/tempdirs rather than mocked, since Path::exists and
    // std::fs::write are the entire surface being tested.

    #[test]
    fn check_tar_missing_path_reports_missing() {
        let tmp = TempDir::new().unwrap();
        let check = check_tar("/nonexistent/path/to/tar-that-does-not-exist", tmp.path());
        assert!(matches!(check, TarToolCheck::Missing { .. }), "{check:?}");
    }

    // -- resolve_on_path: pure, exercised with an explicit PATH string so no
    // test mutates the real process environment (issue #119) -- the crate's
    // unit tests run in parallel in one binary, so `set_var` would race.

    #[test]
    fn resolve_on_path_finds_an_executable_bare_name_shim() {
        let tmp = TempDir::new().unwrap();
        let shim_name = format!("faketar-{}", std::process::id());
        let shim_path = tmp.path().join(&shim_name);
        std::fs::write(&shim_path, "#!/bin/sh\necho 'tar (GNU tar) 1.35'\n").unwrap();
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&shim_path, std::fs::Permissions::from_mode(0o755)).unwrap();
        }

        let path_var = std::ffi::OsString::from(tmp.path());
        let resolved = resolve_on_path(&shim_name, &path_var);
        assert_eq!(resolved, Some(shim_path.clone()));
    }

    #[test]
    fn resolve_on_path_skips_a_non_executable_match_and_keeps_looking() {
        let tmp1 = TempDir::new().unwrap();
        let tmp2 = TempDir::new().unwrap();
        let name = format!("faketar-noexec-{}", std::process::id());

        // A same-named, non-executable file earlier on PATH must not shadow
        // a real executable later on PATH -- matches `execvp`'s behavior.
        let dud = tmp1.path().join(&name);
        std::fs::write(&dud, "not executable").unwrap();

        let real = tmp2.path().join(&name);
        std::fs::write(&real, "#!/bin/sh\necho hi\n").unwrap();
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&real, std::fs::Permissions::from_mode(0o755)).unwrap();
        }

        let path_var = std::env::join_paths([tmp1.path(), tmp2.path()]).unwrap();
        let resolved = resolve_on_path(&name, &path_var);
        assert_eq!(resolved, Some(real));
    }

    #[test]
    fn resolve_on_path_returns_none_for_a_name_absent_everywhere_on_path() {
        let path_var = std::ffi::OsString::from("/nonexistent-dir-a:/nonexistent-dir-b");
        let resolved = resolve_on_path("definitely-not-here-xyz", &path_var);
        assert_eq!(resolved, None);
    }

    #[test]
    fn check_tar_bare_name_absent_from_path_reports_missing() {
        let tmp = TempDir::new().unwrap();
        let check = check_tar("definitely-not-here-xyz-tapectl-test", tmp.path());
        assert!(matches!(check, TarToolCheck::Missing { .. }), "{check:?}");
    }

    /// GNU tar at the floor is on `PATH` for the suite (tests/test_dependencies.rs):
    /// a bare `tar` resolves like the runtime resolves it, and passes.
    #[test]
    fn check_tar_bare_name_present_on_path_resolves_like_the_runtime_does() {
        let tmp = TempDir::new().unwrap();
        match check_tar("tar", tmp.path()) {
            TarToolCheck::Ok { path, .. } => {
                assert!(path.contains('/'), "a resolved absolute path, got {path}")
            }
            other => panic!("expected 'tar' on PATH to pass the probe, got {other:?}"),
        }
    }

    #[test]
    fn check_staging_missing_dir_reports_missing() {
        let check = check_staging("/nonexistent/staging/dir/for/tapectl/tests");
        assert!(matches!(check, StagingCheck::Missing { .. }));
    }

    #[test]
    fn check_staging_writable_dir_reports_writable_and_leaves_no_probe_file() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path().to_str().unwrap();
        let check = check_staging(dir);
        assert!(matches!(check, StagingCheck::Writable { .. }));
        let leftover: Vec<_> = std::fs::read_dir(tmp.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .collect();
        assert!(leftover.is_empty(), "probe file was not cleaned up");
    }

    // -- check_staging_space / describe_staging_space (issue #140) --

    fn backend_with_capacity(
        name: &str,
        generation: &str,
        cap: &str,
    ) -> crate::config::LtoBackendConfig {
        crate::config::LtoBackendConfig {
            name: name.to_string(),
            device_tape: "/dev/null".to_string(),
            device_sg: "/dev/null".to_string(),
            generation: generation.to_string(),
            capacity_override: Some(cap.to_string()),
            fill_ceiling: 0.92,
            enospc_buffer: "50M".to_string(),
            post_warning_bytes: None,
        }
    }

    /// No drive configured: silent. There is no cartridge size to compare
    /// against and inventing one would be a guess.
    #[test]
    fn staging_space_with_no_backend_is_silent() {
        let tmp = TempDir::new().unwrap();
        let mut config = Config::default();
        config.staging.directory = tmp.path().to_string_lossy().into_owned();
        let check = check_staging_space(&config);
        assert_eq!(check, StagingSpaceCheck::Unknown);
        assert!(describe_staging_space(&check).is_none());
    }

    /// A real LTO-6 (2.5 TB) against a tempdir on this machine: no dev box
    /// has 2.5 TB free on /tmp, so this is the warning case, and it warns
    /// with the numbers in it.
    #[test]
    fn staging_space_smaller_than_a_cartridge_warns_with_both_figures() {
        let tmp = TempDir::new().unwrap();
        let mut config = Config::default();
        config.staging.directory = tmp.path().to_string_lossy().into_owned();
        config
            .backends
            .lto
            .push(backend_with_capacity("lto6", "LTO-6", "2.5T"));

        let check = check_staging_space(&config);
        let StagingSpaceCheck::Tight { tape_bytes, .. } = &check else {
            panic!("expected Tight on a tempdir vs a 2.5 TB cartridge, got {check:?}");
        };
        assert!(*tape_bytes >= 2_000_000_000_000);

        let line = describe_staging_space(&check).unwrap();
        assert!(line.starts_with("warning:"), "{line}");
        assert!(line.contains("LTO-6"), "{line}");
        assert!(line.contains("will not fit"), "{line}");
        assert!(!line.contains("fail partway"), "{line}");
    }

    /// A one-byte "cartridge": any filesystem clears it, so this is the
    /// sufficient case, and it must NOT read as a warning.
    #[test]
    fn staging_space_larger_than_a_cartridge_is_not_a_warning() {
        let tmp = TempDir::new().unwrap();
        let mut config = Config::default();
        config.staging.directory = tmp.path().to_string_lossy().into_owned();
        config
            .backends
            .lto
            .push(backend_with_capacity("tiny", "LTO-6", "1"));

        let check = check_staging_space(&config);
        assert!(
            matches!(check, StagingSpaceCheck::Sufficient { .. }),
            "{check:?}"
        );
        let line = describe_staging_space(&check).unwrap();
        assert!(!line.contains("warning"), "{line}");
    }

    /// Several drives: the LARGEST decides. Warning against the smallest
    /// would clear a directory that cannot hold the tape the operator is
    /// about to write.
    #[test]
    fn staging_space_compares_against_the_largest_configured_drive() {
        let tmp = TempDir::new().unwrap();
        let mut config = Config::default();
        config.staging.directory = tmp.path().to_string_lossy().into_owned();
        config
            .backends
            .lto
            .push(backend_with_capacity("tiny", "LTO-6", "1"));
        config
            .backends
            .lto
            .push(backend_with_capacity("huge", "LTO-9", "18T"));

        let check = check_staging_space(&config);
        let StagingSpaceCheck::Tight { generation, .. } = &check else {
            panic!("the 18 TB drive must decide, got {check:?}");
        };
        assert_eq!(generation, "LTO-9");
    }

    /// A staging directory that does not exist is `Unknown`, not a panic
    /// and not a bogus zero: `check_staging` already reports the absence,
    /// and saying it twice is noise.
    #[test]
    fn staging_space_on_a_missing_directory_is_silent() {
        let mut config = Config::default();
        config.staging.directory = "/nonexistent/tapectl/staging/for/tests".to_string();
        config
            .backends
            .lto
            .push(backend_with_capacity("lto6", "LTO-6", "2.5T"));
        assert_eq!(check_staging_space(&config), StagingSpaceCheck::Unknown);
    }

    /// Decimal units, matching how tape generations are marketed and stored
    /// — a 2.5 TB cartridge rendered as "2.3 TiB" reads like a mis-detection.
    /// Issue #363: through the crate's ONE decimal formatter, so `config
    /// check`'s staging line says "2.50 TB"/"KB" exactly as `cartridge info`
    /// and `collection plan` do (it said "2.5 TB"/"kB" from a private copy).
    #[test]
    fn decimal_bytes_uses_marketing_units_not_binary_ones() {
        for b in [2_500_000_000_000u64, 40_000_000_000, 1_500, 512] {
            assert_eq!(
                decimal_bytes(b),
                crate::util::format_bytes_decimal(b as i64),
                "{b}"
            );
        }
        assert_eq!(decimal_bytes(2_500_000_000_000), "2.50 TB");
        assert_eq!(decimal_bytes(512), "512 B");
    }

    #[test]
    fn scan_tape_devices_reports_existence_per_backend_without_opening() {
        let mut config = Config::default();
        config.backends.lto.push(crate::config::LtoBackendConfig {
            name: "lto1".to_string(),
            device_tape: "/dev/nst0-tapectl-test-nonexistent".to_string(),
            device_sg: "/dev/sg-tapectl-test-nonexistent".to_string(),
            generation: "LTO-8".to_string(),
            capacity_override: Some("2.5T".to_string()),
            fill_ceiling: 0.92,
            enospc_buffer: "50M".to_string(),
            post_warning_bytes: None,
        });
        let hits = scan_tape_devices(&config);
        assert_eq!(hits.len(), 1);
        assert!(!hits[0].device_tape_exists);
        assert!(!hits[0].device_sg_exists);
    }
}
