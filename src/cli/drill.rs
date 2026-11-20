//! `tapectl drill record LABEL` (#548, D18, ADR-0012 items 247 and 290): the
//! catalog's record that an heir drill was done on a volume. One row: when,
//! which volume, which RESTORE.sh (the volume's own `restore_sh_sha256`) and
//! which tar and age the host ran it with. Catalog only: it opens no device.
//! `audit`'s `drill_age` reads the newest row.

use clap::Subcommand;
use rusqlite::{params, Connection, OptionalExtension};

use crate::db::events;
use crate::error::{Result, TapectlError};

#[derive(Subcommand, Debug)]
pub enum DrillCommands {
    /// Record that an heir drill was done on a volume
    ///
    /// Run RESTORE.sh from the volume as an heir would, with the stock tools
    /// alone, then record it here: the row names the volume, the sha256 of
    /// the RESTORE.sh the volume carries (as the catalog recorded it when
    /// the volume was written or rebuilt; empty when it never learned it),
    /// and the first lines of `tar --version` and `age --version` on this
    /// host (empty when a tool is absent). `audit` warns (`drill_age`) when
    /// no drill is recorded or the newest is older than `[defaults]
    /// drill_interval_days` (365 when absent). A volume of any status may be
    /// drilled. Reads no device.
    Record {
        /// Label of the volume drilled
        label: String,
        /// A note kept with the record
        #[arg(long)]
        note: Option<String>,
    },
}

/// What a `drill record` wrote, or would write.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DrillRecord {
    pub id: Option<i64>,
    pub label: String,
    pub script_sha256: Option<String>,
    pub tar_version: Option<String>,
    pub age_version: Option<String>,
    pub note: Option<String>,
}

/// The first line of `<tool> --version`, or `None` when the tool is absent
/// or says nothing.
fn tool_version(tool: &str) -> Option<String> {
    let out = std::process::Command::new(tool)
        .arg("--version")
        .output()
        .ok()?;
    let text = if out.stdout.is_empty() {
        out.stderr
    } else {
        out.stdout
    };
    let line = String::from_utf8_lossy(&text)
        .lines()
        .next()?
        .trim()
        .to_string();
    (!line.is_empty()).then_some(line)
}

/// Record (or, with `dry_run`, only work out) one drill. The tool versions
/// are the caller's, so a test names them.
pub fn record(
    conn: &Connection,
    label: &str,
    note: Option<&str>,
    tar_version: Option<String>,
    age_version: Option<String>,
    dry_run: bool,
) -> Result<DrillRecord> {
    let label = label.trim();
    let row: Option<(i64, Option<String>)> = conn
        .query_row(
            "SELECT id, restore_sh_sha256 FROM volumes WHERE label = ?1",
            params![label],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    let (volume_id, script_sha256) =
        row.ok_or_else(|| TapectlError::VolumeNotFound(label.to_string()))?;
    let mut rec = DrillRecord {
        id: None,
        label: label.to_string(),
        script_sha256,
        tar_version,
        age_version,
        note: note.map(str::to_string),
    };
    if dry_run {
        return Ok(rec);
    }
    conn.execute(
        "INSERT INTO drills (volume_id, script_sha256, tar_version, age_version, note)
         VALUES (?1, ?2, ?3, ?4, ?5)",
        params![
            volume_id,
            rec.script_sha256,
            rec.tar_version,
            rec.age_version,
            rec.note
        ],
    )?;
    let id = conn.last_insert_rowid();
    events::log_created(conn, "drill", id, label, None)?;
    rec.id = Some(id);
    Ok(rec)
}

pub fn run(
    conn: &Connection,
    command: &DrillCommands,
    json_output: bool,
    dry_run: bool,
) -> Result<()> {
    match command {
        DrillCommands::Record { label, note } => {
            let rec = record(
                conn,
                label,
                note.as_deref(),
                tool_version("tar"),
                tool_version("age"),
                dry_run,
            )?;
            if json_output {
                crate::render::print_json(&serde_json::json!({
                    "id": rec.id,
                    "volume": rec.label,
                    "script_sha256": rec.script_sha256,
                    "tar_version": rec.tar_version,
                    "age_version": rec.age_version,
                    "note": rec.note,
                    "dry_run": dry_run,
                }));
            } else {
                let verb = if dry_run {
                    "would be recorded (DRY RUN — no changes made)"
                } else {
                    "recorded"
                };
                println!(
                    "drill on volume \"{}\" {verb}: script {}, {}, {}",
                    rec.label,
                    rec.script_sha256.as_deref().unwrap_or("(not known)"),
                    rec.tar_version.as_deref().unwrap_or("tar (not found)"),
                    rec.age_version.as_deref().unwrap_or("age (not found)"),
                );
                if rec.script_sha256.is_none() {
                    println!(
                        "note: the catalog does not know which RESTORE.sh volume \"{}\" \
                         carries (its row was made without it); the drill is recorded \
                         with no script hash",
                        rec.label
                    );
                }
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn db_with_volume(sha: Option<&str>) -> Connection {
        let conn = crate::db::open_memory().unwrap();
        conn.execute(
            "INSERT INTO volumes (label, backend_type, backend_name, capacity_bytes, status,
                                  restore_sh_sha256)
             VALUES ('L6-0001', 'lto', 'lto0', 1000, 'retired', ?1)",
            params![sha],
        )
        .unwrap();
        conn
    }

    fn rows(conn: &Connection) -> i64 {
        conn.query_row("SELECT COUNT(*) FROM drills", [], |r| r.get(0))
            .unwrap()
    }

    #[test]
    fn a_drill_copies_the_volumes_script_hash_and_the_hosts_tools() {
        let conn = db_with_volume(Some("ab12"));
        let rec = record(
            &conn,
            "L6-0001",
            Some("first"),
            Some("tar (GNU tar) 1.35".into()),
            None,
            false,
        )
        .unwrap();
        assert!(rec.id.is_some());
        let row: (
            String,
            Option<String>,
            Option<String>,
            Option<String>,
            Option<String>,
        ) = conn
            .query_row(
                "SELECT v.label, d.script_sha256, d.tar_version, d.age_version, d.note
                 FROM drills d JOIN volumes v ON v.id = d.volume_id",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
            )
            .unwrap();
        assert_eq!(
            row,
            (
                "L6-0001".into(),
                Some("ab12".into()),
                Some("tar (GNU tar) 1.35".into()),
                None,
                Some("first".into())
            ),
            "a volume of any status; an absent age is NULL, not a refusal"
        );
    }

    #[test]
    fn a_volume_whose_script_is_unknown_is_recorded_with_null() {
        let conn = db_with_volume(None);
        let rec = record(&conn, "L6-0001", None, None, None, false).unwrap();
        assert_eq!(rec.script_sha256, None);
        assert_eq!(rows(&conn), 1);
    }

    #[test]
    fn an_unknown_label_is_refused_by_name_and_writes_nothing() {
        let conn = db_with_volume(None);
        let err = record(&conn, "NOPE", None, None, None, false).unwrap_err();
        assert!(
            matches!(err, TapectlError::VolumeNotFound(ref l) if l == "NOPE"),
            "{err}"
        );
        assert_eq!(rows(&conn), 0);
    }

    #[test]
    fn a_dry_run_reports_and_writes_nothing() {
        let conn = db_with_volume(Some("ab12"));
        let rec = record(&conn, "L6-0001", None, None, None, true).unwrap();
        assert_eq!(rec.id, None);
        assert_eq!(rec.script_sha256.as_deref(), Some("ab12"));
        assert_eq!(rows(&conn), 0);
        let events: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM events WHERE entity_type = 'drill'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(events, 0);
    }
}
