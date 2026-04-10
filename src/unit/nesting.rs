use std::path::Path;

use rusqlite::Connection;

use crate::db::queries;
use crate::error::{Result, TapectlError};

/// Check if the given path would create a nesting conflict with existing units.
/// Both parent-contains-child and child-inside-parent are errors.
pub fn check_nesting(conn: &Connection, path: &str) -> Result<()> {
    if let Some(conflict) = queries::check_nesting_conflict(conn, path)? {
        return Err(TapectlError::NestedUnit(conflict));
    }

    // Also check for dotfiles on the filesystem in parent/child dirs,
    // in case the DB is out of sync.
    check_filesystem_nesting(path)?;

    Ok(())
}

/// Walk parent directories looking for an existing .tapectl-unit.toml.
fn check_filesystem_nesting(path: &str) -> Result<()> {
    let target = Path::new(path);

    // Check parents
    let mut current = target.parent();
    while let Some(parent) = current {
        if parent.join(".tapectl-unit.toml").exists() {
            return Err(TapectlError::NestedUnit(format!(
                "{} is inside an existing unit at {}",
                path,
                parent.display()
            )));
        }
        current = parent.parent();
    }

    // Check immediate children (one level deep is enough for detection)
    if let Ok(entries) = std::fs::read_dir(target) {
        for entry in entries.flatten() {
            if entry.path().is_dir() && entry.path().join(".tapectl-unit.toml").exists() {
                return Err(TapectlError::NestedUnit(format!(
                    "existing unit at {} is inside {}",
                    entry.path().display(),
                    path
                )));
            }
        }
    }

    Ok(())
}
