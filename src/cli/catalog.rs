use clap::Subcommand;
use rusqlite::{params, Connection};
use tabled::{Table, Tabled};

use crate::error::{Result, TapectlError};

#[derive(Subcommand, Debug)]
pub enum CatalogCommands {
    /// List files in a unit's latest snapshot
    Ls {
        /// Unit name
        unit: String,
        /// Snapshot version (default: latest)
        #[arg(long)]
        version: Option<i64>,
    },

    /// Search for files by pattern
    Search {
        /// Search pattern (substring match)
        pattern: String,
        /// Limit results
        #[arg(long, default_value = "50")]
        limit: i64,
    },

    /// Show which volume(s) contain a unit
    Locate {
        /// Unit name
        unit: String,
    },

    /// Show catalog statistics
    Stats,
}

#[derive(Tabled)]
struct FileRow {
    #[tabled(rename = "Path")]
    path: String,
    #[tabled(rename = "Size")]
    size: String,
    #[tabled(rename = "Modified")]
    modified: String,
    #[tabled(rename = "SHA256")]
    sha256: String,
}

#[derive(Tabled, Debug)]
struct LocationRow {
    #[tabled(rename = "Volume")]
    volume: String,
    /// The volume's CURRENT lifecycle status (issue #57). Without this, a
    /// retired, quarantined, or erased volume was indistinguishable from a
    /// sealed one, so `catalog locate` could send an operator to fetch a
    /// cartridge that cannot serve a restore.
    #[tabled(rename = "Status")]
    status: String,
    /// Physical whereabouts — the whole point of "locate". `volumes` has
    /// carried `location_id` since 001_initial.sql; this query never joined
    /// it, so the command answered "which volume" but not "where is it".
    #[tabled(rename = "Location")]
    location: String,
    #[tabled(rename = "Snapshot")]
    version: i64,
    #[tabled(rename = "Slices")]
    slices: i64,
    #[tabled(rename = "Written")]
    written: String,
    /// Whether this volume can actually serve a restore, per ADR-0004's
    /// eligibility rule ("a stage_set claim on a sealed, unquarantined,
    /// unretired volume"). Derived from the same
    /// `policy::coverage::eligible` predicate the destructive gates and
    /// reports use (issue #89), so `locate` can never disagree with them
    /// about whether coverage exists.
    #[tabled(rename = "Serviceable")]
    serviceable: String,
}

/// Where a unit's completed writes live, and whether each can actually
/// serve a restore (issue #57).
///
/// Extracted from the `Locate` arm so it is directly testable, mirroring the
/// `copies_rows`/`dirty_rows` split in `src/cli/report.rs` rather than
/// inventing a second shape.
///
/// `LEFT JOIN locations` is deliberate: `volumes.location_id` is nullable, so
/// a volume with no recorded location must still be listed (as `unknown`)
/// rather than silently dropping out of a command whose entire job is telling
/// the operator where to go.
///
/// Serviceability reuses `policy::coverage::eligible` — the same ADR-0004
/// predicate the destructive gates and reports use (issue #89) — so `locate`
/// cannot disagree with them about whether coverage exists.
fn locate_rows(conn: &Connection, unit_id: i64) -> Result<Vec<LocationRow>> {
    let sealed = crate::policy::coverage::eligible("v");
    let sql = format!(
        "SELECT v.label, v.status, COALESCE(l.name, 'unknown'),
                s.version, ss.num_slices, w.completed_at,
                CASE WHEN {sealed} THEN 1 ELSE 0 END
         FROM snapshots s
         JOIN stage_sets ss ON ss.snapshot_id = s.id
         JOIN writes w ON w.stage_set_id = ss.id
         JOIN volumes v ON v.id = w.volume_id
         LEFT JOIN locations l ON l.id = v.location_id
         WHERE s.unit_id = ?1 AND w.status = 'completed'
         ORDER BY s.version DESC, v.label"
    );
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt
        .query_map(params![unit_id], |row| {
            let serviceable: i64 = row.get(6)?;
            Ok(LocationRow {
                volume: row.get(0)?,
                status: row.get(1)?,
                location: row.get(2)?,
                version: row.get(3)?,
                slices: row.get::<_, Option<i64>>(4)?.unwrap_or(0),
                written: row.get::<_, Option<String>>(5)?.unwrap_or_default(),
                serviceable: if serviceable == 1 { "yes" } else { "NO" }.to_string(),
            })
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    Ok(rows)
}

pub fn run(conn: &Connection, command: &CatalogCommands, json_output: bool) -> Result<()> {
    match command {
        CatalogCommands::Ls { unit, version } => {
            let unit_row = crate::db::queries::get_unit_by_name(conn, unit)?
                .ok_or_else(|| TapectlError::UnitNotFound(unit.clone()))?;

            let snapshot_id: i64 = if let Some(v) = version {
                conn.query_row(
                    "SELECT id FROM snapshots WHERE unit_id = ?1 AND version = ?2",
                    params![unit_row.id, v],
                    |row| row.get(0),
                )
                .map_err(|_| TapectlError::Other(format!("snapshot v{v} not found")))?
            } else {
                conn.query_row(
                    "SELECT id FROM snapshots WHERE unit_id = ?1 ORDER BY version DESC LIMIT 1",
                    params![unit_row.id],
                    |row| row.get(0),
                )
                .map_err(|_| TapectlError::Other("no snapshots found".into()))?
            };

            let mut stmt = conn.prepare(
                "SELECT path, size_bytes, modified_at, sha256, is_directory
                 FROM files WHERE snapshot_id = ?1 ORDER BY path",
            )?;
            let rows: Vec<FileRow> = stmt
                .query_map(params![snapshot_id], |row| {
                    let size: i64 = row.get(1)?;
                    let is_dir: bool = row.get(4)?;
                    Ok(FileRow {
                        path: format!(
                            "{}{}",
                            if is_dir { "d " } else { "  " },
                            row.get::<_, String>(0)?
                        ),
                        size: if is_dir {
                            "-".into()
                        } else {
                            format_size(size)
                        },
                        modified: row.get::<_, Option<String>>(2)?.unwrap_or_default(),
                        sha256: row
                            .get::<_, Option<String>>(3)?
                            .map(|s| format!("{}...", &s[..12]))
                            .unwrap_or_else(|| "(unstaged)".into()),
                    })
                })?
                .collect::<std::result::Result<Vec<_>, _>>()?;

            if json_output {
                let json: Vec<serde_json::Value> = rows
                    .iter()
                    .map(|r| {
                        serde_json::json!({"path": r.path.trim(), "size": r.size, "sha256": r.sha256})
                    })
                    .collect();
                println!("{}", serde_json::to_string_pretty(&json).unwrap());
            } else if rows.is_empty() {
                println!("no files found");
            } else {
                println!("{}", Table::new(rows));
            }
        }

        CatalogCommands::Search { pattern, limit } => {
            // Build an FTS5 MATCH expression: split on non-alphanumeric, prefix-match each
            // token with AND. FTS5 default tokenizer already splits paths this way, so a
            // pattern like "foo/bar" becomes `foo* bar*` which matches 'foo/bar.txt'.
            let tokens: Vec<String> = pattern
                .split(|c: char| !c.is_alphanumeric())
                .filter(|s| !s.is_empty())
                .map(|t| format!("{}*", t.to_lowercase()))
                .collect();

            let mut stmt = conn.prepare(
                "SELECT f.path, f.size_bytes, u.name, s.version
                 FROM files_fts fts
                 JOIN files f ON f.rowid = fts.rowid
                 JOIN snapshots s ON s.id = f.snapshot_id
                 JOIN units u ON u.id = s.unit_id
                 WHERE files_fts MATCH ?1 AND f.is_directory = 0
                 ORDER BY rank
                 LIMIT ?2",
            )?;
            let rows: Vec<(String, i64, String, i64)> = if tokens.is_empty() {
                Vec::new()
            } else {
                stmt.query_map(params![tokens.join(" "), limit], |row| {
                    Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
                })?
                .collect::<std::result::Result<Vec<_>, _>>()?
            };

            if json_output {
                let json: Vec<serde_json::Value> = rows
                    .iter()
                    .map(|(path, size, unit, ver)| {
                        serde_json::json!({"path": path, "size": size, "unit": unit, "version": ver})
                    })
                    .collect();
                println!("{}", serde_json::to_string_pretty(&json).unwrap());
            } else if rows.is_empty() {
                println!("no files matching \"{pattern}\"");
            } else {
                for (path, size, unit, ver) in &rows {
                    println!("  {unit} v{ver}: {path} ({})", format_size(*size));
                }
                println!("{} result(s)", rows.len());
            }
        }

        CatalogCommands::Locate { unit } => {
            let unit_row = crate::db::queries::get_unit_by_name(conn, unit)?
                .ok_or_else(|| TapectlError::UnitNotFound(unit.clone()))?;

            let rows = locate_rows(conn, unit_row.id)?;

            if json_output {
                let json: Vec<serde_json::Value> = rows
                    .iter()
                    .map(|r| {
                        serde_json::json!({
                            "volume": r.volume,
                            "status": r.status,
                            "location": r.location,
                            "version": r.version,
                            "slices": r.slices,
                            "written": r.written,
                            "serviceable": r.serviceable == "yes",
                        })
                    })
                    .collect();
                println!("{}", serde_json::to_string_pretty(&json).unwrap());
            } else if rows.is_empty() {
                println!("unit \"{unit}\" not found on any volume");
            } else {
                println!("{}", Table::new(&rows));
                // Say it in words too — a "NO" in a table column is easy to
                // skim past when you are about to walk to a shelf.
                let unserviceable: Vec<&str> = rows
                    .iter()
                    .filter(|r| r.serviceable != "yes")
                    .map(|r| r.volume.as_str())
                    .collect();
                if !unserviceable.is_empty() {
                    println!(
                        "\nnote: {} cannot serve a restore in their current state \
                         (not sealed — retired, quarantined, erased, or still being written). \
                         Fetching one will not help.",
                        unserviceable.join(", ")
                    );
                }
            }
        }

        CatalogCommands::Stats => {
            let unit_count: i64 =
                conn.query_row("SELECT COUNT(*) FROM units", [], |row| row.get(0))?;
            let snapshot_count: i64 =
                conn.query_row("SELECT COUNT(*) FROM snapshots", [], |row| row.get(0))?;
            let file_count: i64 =
                conn.query_row("SELECT COUNT(*) FROM files", [], |row| row.get(0))?;
            let total_size: i64 = conn.query_row(
                "SELECT COALESCE(SUM(size_bytes), 0) FROM files WHERE is_directory = 0",
                [],
                |row| row.get(0),
            )?;
            let volume_count: i64 =
                conn.query_row("SELECT COUNT(*) FROM volumes", [], |row| row.get(0))?;

            if json_output {
                println!(
                    "{}",
                    serde_json::json!({
                        "units": unit_count,
                        "snapshots": snapshot_count,
                        "files": file_count,
                        "total_size": total_size,
                        "volumes": volume_count,
                    })
                );
            } else {
                println!("Catalog statistics:");
                println!("  Units:     {unit_count}");
                println!("  Snapshots: {snapshot_count}");
                println!("  Files:     {file_count}");
                println!("  Total:     {}", format_size(total_size));
                println!("  Volumes:   {volume_count}");
            }
        }
    }
    Ok(())
}

fn format_size(bytes: i64) -> String {
    if bytes >= 1024 * 1024 * 1024 {
        format!("{:.1} GB", bytes as f64 / (1024.0 * 1024.0 * 1024.0))
    } else if bytes >= 1024 * 1024 {
        format!("{:.1} MB", bytes as f64 / (1024.0 * 1024.0))
    } else if bytes >= 1024 {
        format!("{:.1} KB", bytes as f64 / 1024.0)
    } else {
        format!("{bytes} B")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db;

    /// A unit with completed writes to two volumes: one `sealed`, one in
    /// `second_status`. The second volume is given a physical location; the
    /// first deliberately is not, so the `unknown` fallback is exercised too.
    fn setup_unit_on_two_volumes(name: &str, second_status: &str) -> Connection {
        let conn = db::open_memory().unwrap();
        conn.execute(
            "INSERT INTO tenants (name, is_operator, status) VALUES ('t', 0, 'active')",
            [],
        )
        .unwrap();
        let tid = conn.last_insert_rowid();
        conn.execute(
            "INSERT INTO units (uuid, name, tenant_id, checksum_mode, encrypt, status)
             VALUES (?1, ?2, ?3, 'mtime_size', 1, 'active')",
            params![format!("uuid-{name}"), name, tid],
        )
        .unwrap();
        let unit_id = conn.last_insert_rowid();
        conn.execute(
            "INSERT INTO snapshots (unit_id, version, snapshot_type, status, source_path)
             VALUES (?1, 1, 'full', 'current', '/src')",
            params![unit_id],
        )
        .unwrap();
        let snap_id = conn.last_insert_rowid();
        conn.execute(
            "INSERT INTO stage_sets (snapshot_id, status, slice_size, num_slices)
             VALUES (?1, 'staged', 524288, 3)",
            params![snap_id],
        )
        .unwrap();
        let ss_id = conn.last_insert_rowid();

        conn.execute(
            "INSERT INTO locations (name, description) VALUES ('parents-house', 'offsite')",
            [],
        )
        .unwrap();
        let loc_id = conn.last_insert_rowid();

        // Volume 1: sealed, NO location recorded.
        conn.execute(
            "INSERT INTO volumes (label, backend_type, backend_name, media_type, capacity_bytes, status)
             VALUES ('L6-SEALED', 'lto', 'lto0', 'LTO-6', 2500000000000, 'sealed')",
            [],
        )
        .unwrap();
        let v1 = conn.last_insert_rowid();
        conn.execute(
            "INSERT INTO writes (stage_set_id, snapshot_id, volume_id, status, completed_at)
             VALUES (?1, ?2, ?3, 'completed', '2026-07-01T00:00:00Z')",
            params![ss_id, snap_id, v1],
        )
        .unwrap();

        // Volume 2: the status under test, WITH a location recorded.
        conn.execute(
            &format!(
                "INSERT INTO volumes (label, backend_type, backend_name, media_type, capacity_bytes, status, location_id)
                 VALUES ('L6-OTHER', 'lto', 'lto0', 'LTO-6', 2500000000000, '{second_status}', {loc_id})"
            ),
            [],
        )
        .unwrap();
        let v2 = conn.last_insert_rowid();
        conn.execute(
            "INSERT INTO writes (stage_set_id, snapshot_id, volume_id, status, completed_at)
             VALUES (?1, ?2, ?3, 'completed', '2026-07-02T00:00:00Z')",
            params![ss_id, snap_id, v2],
        )
        .unwrap();

        conn
    }

    fn unit_id_of(conn: &Connection, name: &str) -> i64 {
        conn.query_row("SELECT id FROM units WHERE name = ?1", params![name], |r| {
            r.get(0)
        })
        .unwrap()
    }

    fn assert_second_volume_is_not_serviceable(status: &str) {
        let name = format!("loc-{status}");
        let conn = setup_unit_on_two_volumes(&name, status);
        let rows = locate_rows(&conn, unit_id_of(&conn, &name)).unwrap();

        // Both volumes are still LISTED — locate must not hide a cartridge
        // that physically holds the data; it must label it honestly.
        assert_eq!(rows.len(), 2, "both volumes must be listed, got {rows:?}");

        let other = rows
            .iter()
            .find(|r| r.volume == "L6-OTHER")
            .expect("the non-sealed volume must still appear");
        assert_eq!(
            other.status, status,
            "status column must show the real state"
        );
        assert_eq!(
            other.serviceable, "NO",
            "a {status} volume cannot serve a restore (ADR-0004)"
        );

        let sealed = rows
            .iter()
            .find(|r| r.volume == "L6-SEALED")
            .expect("the sealed volume must appear");
        assert_eq!(sealed.serviceable, "yes");
    }

    #[test]
    fn locate_marks_a_quarantined_volume_unserviceable() {
        assert_second_volume_is_not_serviceable("quarantined");
    }

    #[test]
    fn locate_marks_a_retired_volume_unserviceable() {
        assert_second_volume_is_not_serviceable("retired");
    }

    #[test]
    fn locate_marks_an_erased_volume_unserviceable() {
        assert_second_volume_is_not_serviceable("erased");
    }

    #[test]
    fn locate_reports_physical_location_and_falls_back_to_unknown() {
        // The whole point of "locate" is telling an operator where to go.
        // A volume with no recorded location must still be listed, as
        // `unknown` — not silently dropped by an inner join.
        let conn = setup_unit_on_two_volumes("loc-place", "sealed");
        let rows = locate_rows(&conn, unit_id_of(&conn, "loc-place")).unwrap();

        let placed = rows.iter().find(|r| r.volume == "L6-OTHER").unwrap();
        assert_eq!(placed.location, "parents-house");

        let unplaced = rows.iter().find(|r| r.volume == "L6-SEALED").unwrap();
        assert_eq!(
            unplaced.location, "unknown",
            "a volume with NULL location_id must appear as unknown, not vanish"
        );
    }

    #[test]
    fn locate_agrees_with_the_copy_derivation_about_eligibility() {
        // The property that matters: `locate`'s serviceable flag and the
        // copy-count derivation the destructive gates consume must never
        // disagree about the same volume. Both route through
        // `policy::coverage::eligible` (issue #89) — this pins it.
        let conn = setup_unit_on_two_volumes("loc-agree", "quarantined");
        let unit_id = unit_id_of(&conn, "loc-agree");
        let rows = locate_rows(&conn, unit_id).unwrap();

        let serviceable_count = rows.iter().filter(|r| r.serviceable == "yes").count() as i64;

        let sealed = crate::policy::coverage::eligible("v");
        let gate_count: i64 = conn
            .query_row(
                &format!(
                    "SELECT COUNT(DISTINCT CASE WHEN {sealed} THEN w.volume_id END)
                     FROM snapshots s
                     JOIN stage_sets ss ON ss.snapshot_id = s.id
                     JOIN writes w ON w.stage_set_id = ss.id AND w.status = 'completed'
                     JOIN volumes v ON v.id = w.volume_id
                     WHERE s.unit_id = ?1"
                ),
                params![unit_id],
                |r| r.get(0),
            )
            .unwrap();

        assert_eq!(
            serviceable_count, gate_count,
            "locate's serviceable count must equal the gates' copy count"
        );
        assert_eq!(gate_count, 1, "only the sealed volume counts");
    }
}
