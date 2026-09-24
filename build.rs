//! Build identity (ADR-0012, 2026-09-24 amendment, item 1).
//!
//! `tapectl --version` and every journal row's `tapectl_version` must name
//! the COMMIT a binary was built from, not just the package version — a
//! parser bug fixed in a later build is otherwise indistinguishable from a
//! hardware change (ADR-0013 §7), and "0.1.0" has been the package version
//! for every commit of this project's life. This script stamps two
//! environment variables into the crate at build time:
//!
//! - `TAPECTL_GIT_DESCRIBE` — `git describe --tags --always --dirty`, or
//!   `unknown` when that cannot be answered (no `git` binary, a source
//!   tarball, a checkout `git` refuses to read). It NEVER fails the build:
//!   an unknown identity is a fact to record, not a reason to have no binary.
//! - `TAPECTL_BUILD_DATE` — the build day in UTC as an RFC 3339 full-date
//!   (`YYYY-MM-DD`). `SOURCE_DATE_EPOCH` is honoured when set, so a
//!   reproducible build gets a reproducible date.
//!
//! `src/build_info.rs` turns them into `VERSION`. What reaches the tape is
//! deliberately NOT this — see `build_info::PKG_VERSION`.
//!
//! Re-run policy: emitting any `rerun-if-changed` switches cargo from "re-run
//! on any package change" to "re-run only on these". The list covers HEAD and
//! the ref it points at (the commit), the index, and the files that compile
//! into the binary (`src`, `tests`, `examples`, `Cargo.*`, `build.rs`). Known
//! gap: `--dirty` reflects ANY tracked file, so an edit confined to e.g.
//! `docs/` or `scripts/` leaves a stale `-dirty` state until the next watched
//! change or commit — accepted, because those files do not change the binary
//! and watching them would recompile the crate on every doc edit.
//! Paths come from `git rev-parse --git-path`, never a literal
//! `.git/HEAD`: in a linked worktree `.git` is a FILE pointing at
//! `.git/worktrees/<name>/`, and per-worktree HEAD lives there while the
//! branch ref lives in the common dir. Only paths that exist are emitted —
//! cargo treats a missing rerun-if-changed path as always stale.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

fn main() {
    let manifest_dir = PathBuf::from(
        std::env::var_os("CARGO_MANIFEST_DIR").expect("cargo always sets CARGO_MANIFEST_DIR"),
    );

    println!(
        "cargo:rustc-env=TAPECTL_GIT_DESCRIBE={}",
        git_describe(&manifest_dir)
    );
    println!("cargo:rustc-env=TAPECTL_BUILD_DATE={}", build_date());

    println!("cargo:rerun-if-env-changed=SOURCE_DATE_EPOCH");
    for rel in [
        "build.rs",
        "Cargo.toml",
        "Cargo.lock",
        "src",
        "tests",
        "examples",
    ] {
        rerun_if_exists(manifest_dir.join(rel));
    }
    for path in git_watch_paths(&manifest_dir) {
        rerun_if_exists(path);
    }
}

/// `git describe --tags --always --dirty`, or `unknown`.
fn git_describe(manifest_dir: &Path) -> String {
    let out = Command::new("git")
        .args(["describe", "--tags", "--always", "--dirty"])
        .current_dir(manifest_dir)
        .output();
    match out {
        Ok(o) if o.status.success() => {
            let s = String::from_utf8_lossy(&o.stdout).trim().to_string();
            if s.is_empty() {
                "unknown".to_string()
            } else {
                s
            }
        }
        _ => "unknown".to_string(),
    }
}

/// The git files the describe string depends on, resolved through
/// `git rev-parse --git-path` so linked worktrees resolve correctly.
fn git_watch_paths(manifest_dir: &Path) -> Vec<PathBuf> {
    let mut wanted = vec![
        "HEAD".to_string(),
        "packed-refs".to_string(),
        "index".to_string(),
    ];
    if let Some(r) = git_stdout(manifest_dir, &["symbolic-ref", "-q", "HEAD"]) {
        wanted.push(r);
    }
    wanted
        .iter()
        .filter_map(|p| git_stdout(manifest_dir, &["rev-parse", "--git-path", p]))
        .map(|p| {
            let p = PathBuf::from(p);
            if p.is_absolute() {
                p
            } else {
                manifest_dir.join(p)
            }
        })
        .collect()
}

fn git_stdout(manifest_dir: &Path, args: &[&str]) -> Option<String> {
    let o = Command::new("git")
        .args(args)
        .current_dir(manifest_dir)
        .output()
        .ok()?;
    if !o.status.success() {
        return None;
    }
    let s = String::from_utf8_lossy(&o.stdout).trim().to_string();
    (!s.is_empty()).then_some(s)
}

fn rerun_if_exists(path: PathBuf) {
    if path.exists() {
        println!("cargo:rerun-if-changed={}", path.display());
    }
}

/// Today in UTC as `YYYY-MM-DD`, from `SOURCE_DATE_EPOCH` when set.
fn build_date() -> String {
    let secs = std::env::var("SOURCE_DATE_EPOCH")
        .ok()
        .and_then(|s| s.trim().parse::<i64>().ok())
        .unwrap_or_else(|| {
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_secs() as i64)
                .unwrap_or(0)
        });
    let (y, m, d) = civil_from_days(secs.div_euclid(86_400));
    format!("{y:04}-{m:02}-{d:02}")
}

/// Proleptic Gregorian civil date from days since 1970-01-01 (Howard
/// Hinnant's `civil_from_days`). No `chrono` here: build scripts get no
/// crate dependencies without coordinator approval, and this is twelve lines.
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}
