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
use crate::dar::version;

/// Result of probing the configured dar binary.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DarCheck {
    /// Nothing exists at the configured path.
    Missing { path: String },
    /// Exists but is not executable.
    NotExecutable { path: String },
    /// Exists, executable, ran, and its version could not be determined
    /// (e.g. `--version` produced unparseable output).
    Unreadable { path: String, detail: String },
    /// Ran and reports a version below the documented minimum.
    TooOld {
        path: String,
        found: String,
        minimum: String,
    },
    /// Ran and meets the documented minimum.
    Ok { path: String, version: String },
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

/// Probe `binary`: existence, executable bit, then `dar --version` via the
/// existing runner in `dar::version` (never a second implementation of it).
///
/// If `binary` contains no path separator (a bare name like `"dar"`), it is
/// first resolved against `PATH` via [`resolve_on_path`] — mirroring what
/// the runtime actually does (`Command::new` in `dar::version::check`
/// resolves bare names via `PATH`/`execvp`). Before this, `check_dar` used
/// `Path::exists` directly on the bare name, which is never true, so a
/// perfectly working `binary = "dar"` was reported `Missing` (issue #119).
/// An absolute or relative path containing `/` is used as-is, unchanged
/// from before.
pub fn check_dar(binary: &str) -> DarCheck {
    let resolved: PathBuf = if binary.contains('/') {
        PathBuf::from(binary)
    } else {
        let path_var = std::env::var_os("PATH").unwrap_or_default();
        match resolve_on_path(binary, &path_var) {
            Some(p) => p,
            None => {
                return DarCheck::Missing {
                    path: binary.to_string(),
                };
            }
        }
    };
    let path = resolved.as_path();
    let path_str = resolved.to_string_lossy().to_string();

    if !path.exists() {
        return DarCheck::Missing { path: path_str };
    }

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let executable = std::fs::metadata(path)
            .map(|m| m.permissions().mode() & 0o111 != 0)
            .unwrap_or(false);
        if !executable {
            return DarCheck::NotExecutable { path: path_str };
        }
    }

    match version::check(&path_str) {
        Ok(v) => DarCheck::Ok {
            path: path_str,
            version: v.full_string,
        },
        Err(crate::error::TapectlError::DarVersionTooOld { found, minimum }) => DarCheck::TooOld {
            path: path_str,
            found,
            minimum,
        },
        Err(e) => DarCheck::Unreadable {
            path: path_str,
            detail: e.to_string(),
        },
    }
}

/// The advisory line for a dar check. Pure — testable by constructing
/// [`DarCheck`] variants directly, no filesystem or subprocess involved.
pub fn describe_dar(check: &DarCheck) -> String {
    match check {
        DarCheck::Missing { path } => format!(
            "warning: dar binary not found at '{path}' — config.dar.binary points nowhere; \
             archiving will fail until this is corrected (the shipped default, \
             /opt/dar/bin/dar, does not exist on most systems)"
        ),
        DarCheck::NotExecutable { path } => {
            format!("warning: dar binary at '{path}' exists but is not executable")
        }
        DarCheck::Unreadable { path, detail } => {
            format!("warning: dar binary at '{path}' could not be version-checked: {detail}")
        }
        DarCheck::TooOld {
            path,
            found,
            minimum,
        } => format!(
            "warning: dar at '{path}' is version {found}, below the required minimum {minimum}"
        ),
        DarCheck::Ok { path, version } => {
            format!(
                "dar: {version} at '{path}' (meets minimum {}.{})",
                version::MIN_VERSION.0,
                version::MIN_VERSION.1
            )
        }
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
        })
        .collect()
}

/// The advisory line for one backend's device check. Pure. Deliberately
/// phrased as a mild note ("not attached"), never "warning" — absence is
/// the normal state when a drive isn't plugged in.
pub fn describe_tape_device(check: &TapeDeviceCheck) -> String {
    let mut missing = Vec::new();
    if !check.device_tape_exists {
        missing.push(check.device_tape.as_str());
    }
    if !check.device_sg_exists {
        missing.push(check.device_sg.as_str());
    }
    if missing.is_empty() {
        format!(
            "backend \"{}\": device_tape and device_sg both present",
            check.backend_name
        )
    } else {
        format!(
            "note: backend \"{}\" device path(s) not present: {} — normal if the drive isn't attached",
            check.backend_name,
            missing.join(", ")
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    // -- describe_dar: pure, no filesystem --

    #[test]
    fn describe_dar_missing_names_the_path() {
        let line = describe_dar(&DarCheck::Missing {
            path: "/opt/dar/bin/dar".to_string(),
        });
        assert!(line.contains("/opt/dar/bin/dar"));
        assert!(line.contains("not found"));
    }

    #[test]
    fn describe_dar_ok_reports_version() {
        let line = describe_dar(&DarCheck::Ok {
            path: "/usr/bin/dar".to_string(),
            version: "2.7.13".to_string(),
        });
        assert!(line.contains("2.7.13"));
        assert!(line.contains("/usr/bin/dar"));
    }

    #[test]
    fn describe_dar_too_old_names_found_and_minimum() {
        let line = describe_dar(&DarCheck::TooOld {
            path: "/usr/bin/dar".to_string(),
            found: "2.5.0".to_string(),
            minimum: "2.6".to_string(),
        });
        assert!(line.contains("2.5.0"));
        assert!(line.contains("2.6"));
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

    #[test]
    fn describe_tape_device_absent_is_a_note_not_a_warning() {
        let line = describe_tape_device(&TapeDeviceCheck {
            backend_name: "lto1".to_string(),
            device_tape: "/dev/nst0".to_string(),
            device_tape_exists: false,
            device_sg: "/dev/sg0".to_string(),
            device_sg_exists: false,
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
        });
        assert!(line.contains("both present"));
    }

    // -- check_dar / check_staging / scan_tape_devices: I/O, exercised with
    // tempfiles/tempdirs rather than mocked, since Path::exists and
    // std::fs::write are the entire surface being tested.

    #[test]
    fn check_dar_missing_path_reports_missing() {
        let check = check_dar("/nonexistent/path/to/dar-that-does-not-exist");
        assert!(matches!(check, DarCheck::Missing { .. }));
    }

    // -- resolve_on_path: pure, exercised with an explicit PATH string so no
    // test mutates the real process environment (issue #119) -- the crate's
    // unit tests run in parallel in one binary, so `set_var` would race.

    #[test]
    fn resolve_on_path_finds_an_executable_bare_name_shim() {
        let tmp = TempDir::new().unwrap();
        let shim_name = format!("fakedar-{}", std::process::id());
        let shim_path = tmp.path().join(&shim_name);
        // Mirror the shape `dar::version::check` parses ("dar version
        // X.Y.Z, ..."), so this also doubles as proof the resolved path is
        // actually usable end to end, not merely present on disk.
        std::fs::write(
            &shim_path,
            "#!/bin/sh\necho 'dar version 2.7.13, Copyright (C) 2002-2023 Denis Corbin'\n",
        )
        .unwrap();
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&shim_path, std::fs::Permissions::from_mode(0o755)).unwrap();
        }

        let path_var = std::ffi::OsString::from(tmp.path());
        let resolved = resolve_on_path(&shim_name, &path_var);
        assert_eq!(resolved, Some(shim_path.clone()));

        let version = crate::dar::version::check(shim_path.to_str().unwrap())
            .expect("shim should parse as a valid dar version");
        assert_eq!(version.full_string, "2.7.13");
    }

    #[test]
    fn resolve_on_path_skips_a_non_executable_match_and_keeps_looking() {
        let tmp1 = TempDir::new().unwrap();
        let tmp2 = TempDir::new().unwrap();
        let name = format!("fakedar-noexec-{}", std::process::id());

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
    fn check_dar_bare_name_absent_from_path_reports_missing() {
        // Doesn't touch the real PATH -- just asserts against a name that
        // (overwhelmingly likely) isn't installed anywhere on it.
        let check = check_dar("definitely-not-here-xyz-tapectl-test");
        assert!(matches!(check, DarCheck::Missing { .. }));
    }

    #[test]
    fn check_dar_bare_name_present_on_path_resolves_like_the_runtime_does() {
        // `dar` is a hard runtime dependency and guaranteed on PATH for this
        // whole test suite (CLAUDE.md; enforced by `tests/test_dependencies.rs`),
        // so this exercises check_dar's PATH-resolution branch end to end
        // against the real environment, complementing the explicit-PATH
        // `resolve_on_path` tests above.
        let check = check_dar("dar");
        match check {
            DarCheck::Ok { path, .. } => {
                assert!(
                    path.contains('/'),
                    "expected a resolved absolute path, got {path}"
                )
            }
            other => panic!("expected 'dar' on PATH to resolve to Ok, got {other:?}"),
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

    #[test]
    fn scan_tape_devices_reports_existence_per_backend_without_opening() {
        let mut config = Config::default();
        config.backends.lto.push(crate::config::LtoBackendConfig {
            name: "lto1".to_string(),
            device_tape: "/dev/nst0-tapectl-test-nonexistent".to_string(),
            device_sg: "/dev/sg-tapectl-test-nonexistent".to_string(),
            media_type: "LTO-6".to_string(),
            nominal_capacity: "2.5T".to_string(),
            usable_capacity_factor: 0.92,
            enospc_buffer: "50M".to_string(),
            block_size: "1M".to_string(),
            hardware_compression: false,
        });
        let hits = scan_tape_devices(&config);
        assert_eq!(hits.len(), 1);
        assert!(!hits[0].device_tape_exists);
        assert!(!hits[0].device_sg_exists);
    }
}
