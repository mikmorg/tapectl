//! `collection run`'s snapshot hook (ADR-0012 2026-10-16 item 75, issue #455).
//!
//! A unit can be half a cartridge, so one stage can take many hours, and
//! `--retry-on-change 0` means one file touched in those hours refuses the
//! whole stage. The cure for a living source is to read a read-only snapshot
//! of it. A `[[collections]]` table can name the commands that take and drop
//! one:
//!
//! - `snapshot_command` runs once per `collection run`, before any unit is
//!   snapshotted or staged, under `sh -c` as the user tapectl runs as, with
//!   `TAPECTL_COLLECTION` and `TAPECTL_COLLECTION_ROOT` (canonical) in its
//!   environment. It prints one line on stdout: the absolute path of a
//!   read-only snapshot of the root, which mirrors the root's layout.
//! - `release_command` runs when the run's staging is over, whatever came of
//!   it, with the same variables and `TAPECTL_SNAPSHOT_PATH`.
//!
//! Between the two, the run's snapshot and stage read each unit's folder
//! inside the snapshot ([`SourceView`]) while every path they record stays
//! the unit's real one. A snapshot command that fails, or prints anything but
//! one usable path, refuses the run before any unit is read. A release
//! command that fails is a warning: the run's work is done and good, and the
//! operator is told the snapshot may need removing by hand.
//!
//! `collection plan`, `status` and `sync` never run either command, and neither
//! does `config check`, which only validates the keys.

use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use crate::config::CollectionConfig;
use crate::error::{Result, TapectlError};
use crate::staging::view::SourceView;

/// The collection's name, in both commands' environment.
pub const ENV_COLLECTION: &str = "TAPECTL_COLLECTION";
/// The collection's root, canonical, in both commands' environment.
pub const ENV_COLLECTION_ROOT: &str = "TAPECTL_COLLECTION_ROOT";
/// The snapshot's path, in `release_command`'s environment.
pub const ENV_SNAPSHOT_PATH: &str = "TAPECTL_SNAPSHOT_PATH";

/// A snapshot a run took with `snapshot_command`, until [`Held::release`].
#[derive(Debug)]
pub struct Held {
    collection: String,
    root: String,
    snapshot: PathBuf,
    release_command: Option<String>,
}

/// Run the collection's `snapshot_command`, if it has one, and check what it
/// printed. `Ok(None)` for a collection without one: the run reads its units
/// where they live, as ever.
pub fn take_snapshot(lib: &CollectionConfig) -> Result<Option<Held>> {
    let Some(command) = lib.snapshot_command.as_deref() else {
        return Ok(None);
    };
    let root = super::canonical_root(lib)?;
    // The refusal names the command and, when it ran, what it said on stderr.
    let refuse = |what: String, output: Option<&Output>| -> TapectlError {
        TapectlError::Other(format!(
            "collection \"{}\": snapshot_command `{command}` {what}.{}\nThe run is refused \
             before any unit is read; nothing was snapshotted or staged.",
            lib.name,
            output.map(stderr_of).unwrap_or_default()
        ))
    };

    let output = crate::progress::waited(
        || format!("collection \"{}\" snapshot_command", lib.name),
        || run_shell(command, &lib.name, &root, None),
    )
    .map_err(|e| refuse(format!("could not be started: {e}"), None))?;
    if !output.status.success() {
        return Err(refuse(format!("failed ({})", output.status), Some(&output)));
    }
    echo_stderr(&output);

    let stdout = String::from_utf8(output.stdout.clone()).map_err(|_| {
        refuse(
            "printed output that is not valid UTF-8".to_string(),
            Some(&output),
        )
    })?;
    let lines: Vec<&str> = stdout
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .collect();
    let line = match lines.as_slice() {
        [] => {
            return Err(refuse(
                format!(
                    "printed nothing on stdout (it must print one line: the absolute path of a \
                     read-only snapshot of {root})"
                ),
                Some(&output),
            ))
        }
        [one] => *one,
        many => {
            return Err(refuse(
                format!(
                    "printed {} lines on stdout (it must print exactly one: the absolute path \
                     of a read-only snapshot of {root}; send everything else to stderr)",
                    many.len()
                ),
                Some(&output),
            ))
        }
    };
    let snapshot = checked_snapshot_path(line, &root).map_err(|why| refuse(why, Some(&output)))?;

    Ok(Some(Held {
        collection: lib.name.clone(),
        root,
        snapshot,
        release_command: lib.release_command.clone(),
    }))
}

/// `line`, what `snapshot_command` printed, as a snapshot this run may read:
/// absolute, a directory, and not inside the live root (a snapshot of the root
/// that lives in the root would be a unit nested in itself, and ZFS's
/// `.zfs/snapshot` directory must be mounted or bound elsewhere to qualify).
/// The error is what to say after "snapshot_command `...`".
fn checked_snapshot_path(line: &str, root: &str) -> std::result::Result<PathBuf, String> {
    let path = Path::new(line);
    if !path.is_absolute() {
        return Err(format!("printed {line:?}, which is not an absolute path"));
    }
    let canonical = std::fs::canonicalize(path)
        .map_err(|e| format!("printed {line}, which cannot be read: {e}"))?;
    if !canonical.is_dir() {
        return Err(format!("printed {line}, which is not a directory"));
    }
    let as_text = canonical
        .to_str()
        .ok_or_else(|| format!("printed {line}, which is not valid UTF-8"))?;
    if super::is_under_root(as_text, root) {
        return Err(format!(
            "printed {line}, which is inside the live root {root}: the snapshot must be \
             somewhere else (mount or copy it outside the root)"
        ));
    }
    Ok(canonical)
}

impl Held {
    /// Where the run reads each unit from, in place of the live root.
    pub fn view(&self) -> SourceView {
        SourceView::new(&self.root, &self.snapshot)
    }

    /// The snapshot's path, canonical.
    pub fn snapshot_path(&self) -> &Path {
        &self.snapshot
    }

    /// Run `release_command`, if there is one. Best effort: a failure is a
    /// warning, which is said on stderr at once (the run may be about to
    /// return an error that has no room for it) and returned for the report.
    pub fn release(self) -> Option<String> {
        let command = self.release_command.as_deref()?;
        let output = crate::progress::waited(
            || format!("collection \"{}\" release_command", self.collection),
            || run_shell(command, &self.collection, &self.root, Some(&self.snapshot)),
        );
        let problem = match output {
            Ok(out) if out.status.success() => {
                echo_stderr(&out);
                return None;
            }
            Ok(out) => format!("failed ({}).{}", out.status, stderr_of(&out)),
            Err(e) => format!("could not be started: {e}."),
        };
        let warning = format!(
            "warning: collection \"{}\": release_command `{command}` {problem}\nThe snapshot at \
             {} may still exist; remove it by hand.",
            self.collection,
            self.snapshot.display()
        );
        crate::progress::stderr_println(&warning);
        Some(warning)
    }
}

/// `command` under `sh -c`, with the hook environment, no stdin, and its
/// output captured. `snapshot` is set for `release_command` only.
fn run_shell(
    command: &str,
    collection: &str,
    root: &str,
    snapshot: Option<&Path>,
) -> std::io::Result<Output> {
    let mut cmd = Command::new("sh");
    cmd.arg("-c")
        .arg(command)
        .env(ENV_COLLECTION, collection)
        .env(ENV_COLLECTION_ROOT, root)
        .stdin(Stdio::null());
    match snapshot {
        Some(path) => cmd.env(ENV_SNAPSHOT_PATH, path),
        None => cmd.env_remove(ENV_SNAPSHOT_PATH),
    };
    cmd.output()
}

/// What the command wrote to stderr, as lines to add to a message (the text
/// on lines of its own), or nothing when it wrote nothing.
fn stderr_of(output: &Output) -> String {
    let text = String::from_utf8_lossy(&output.stderr);
    let text = text.trim();
    if text.is_empty() {
        String::new()
    } else {
        format!("\nIts stderr:\n{text}")
    }
}

/// A command that succeeded may still have said something on stderr (a
/// snapshot tool's progress); the operator sees it.
fn echo_stderr(output: &Output) {
    for line in String::from_utf8_lossy(&output.stderr).lines() {
        crate::progress::stderr_println(line);
    }
}
