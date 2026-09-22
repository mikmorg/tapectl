use std::path::PathBuf;

use clap::Subcommand;

/// Fixed tape block size, matching every other tape-reading command here.
const DEFAULT_BLOCK_SIZE: usize = 512 * 1024;
use rusqlite::{params, Connection};
use serde::Serialize;
use tabled::{Table, Tabled};

use crate::error::{Result, TapectlError};
use crate::volume::rebuild::RebuildReport;

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
        /// Search pattern. Split on non-alphanumerics; each token is
        /// PREFIX-matched and all must match (AND). So "foo bar" finds
        /// paths containing a word starting with foo AND one starting
        /// with bar -- it is not a substring match, and "ar" will not
        /// find "bar".
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

    /// Reconstruct catalog rows by reading a sealed volume — the path back
    /// when the database is gone and there is no backup
    Rebuild {
        /// Rebuild from a sealed tape volume. The only source today; named
        /// rather than implied so a later `--from-export` is additive.
        #[arg(long = "from-volume")]
        from_volume: bool,
        /// Tape device (by-id path). Defaults to the only configured drive;
        /// required when more than one is configured.
        ///
        /// There is still no `/dev/nst0` fallback (ADR-0010): `/dev/nstN`
        /// numbering is not stable across reboots on a host with more than
        /// one drive, and unlike a read-only dump this command writes what
        /// it reads into the catalog — a silent wrong-device default would
        /// file one tape's contents under another's. The sole configured
        /// backend's `device_tape` is not a guess about which drive you
        /// meant; a guessed device number is. Resolve by serial through
        /// `/dev/tape/by-id/`.
        #[arg(long)]
        device: Option<String>,
        /// Operator or escrow secret key file. A tenant key cannot open the
        /// operator envelope and is refused with a pointer at RESTORE.sh.
        #[arg(long)]
        key: PathBuf,
        /// Refuse unless the tape's own reported label matches — a
        /// wrong-tape guard, not a database lookup
        #[arg(long)]
        label: Option<String>,
        /// Tenant to file units under when no tenant envelope on this
        /// cartridge names their owner (only reachable with a damaged or
        /// partial tape)
        #[arg(long, default_value = "recovered")]
        tenant: String,
    },
}

#[derive(Tabled, Serialize)]
struct FileRow {
    /// The real path, unprefixed (issue #236 finding 5). This used to store
    /// `"d "`/`"  "` prefixed onto the path for table display, with
    /// `serialize_trimmed` stripping only whitespace for JSON -- so a
    /// directory's JSON `path` came out as `"d subdir"`, matching nothing on
    /// tape or disk, and indistinguishable from a genuine file literally
    /// named `"d subdir"`. The prefix is now a table-only rendering,
    /// produced by `Self::display_path` from [`Self::is_directory`] below --
    /// exactly how `size`/`modified` already keep their display transform
    /// out of the serialized value.
    #[tabled(rename = "Path", display_with("Self::display_path", self))]
    path: String,
    #[tabled(rename = "Size")]
    size: String,
    /// Additive per CTO decision 2026-09-11 (architecture review C2
    /// follow-up, C2b): a raw value alongside the humanised `size` display
    /// string, so a machine consumer of `--json` can get the exact byte
    /// count instead of parsing `size`'s one-decimal-rounded text back
    /// (issue #205 -- 1_234_567 and 1_240_000 both render `"1.2 MiB"`).
    /// `#[tabled(skip)]`: the table is explicitly out of scope for this
    /// fix, a human reading `catalog ls` wants `size`'s `1.2 KiB`, not a
    /// second raw-bytes column.
    ///
    /// `None` for a directory row, not `Some(0)`. The `files.size_bytes`
    /// column actually stores a literal `0` for a directory (see
    /// `staging/mod.rs`'s manifest walk: `let size = if is_dir { 0 } else
    /// { meta.len() as i64 };`) -- that is a placeholder for "not
    /// measured", not a real content size, and surfacing it as `Some(0)`
    /// would make a directory indistinguishable from a genuine
    /// zero-byte file to a machine consumer. `size` already renders `"-"`
    /// for the same row for the same reason.
    #[tabled(skip)]
    size_bytes: Option<i64>,
    /// Table-only until CTO decision 2026-09-11 (architecture review C2
    /// follow-up, C2b): the raw `modified_at` column is nullable, so this
    /// carries `Option<String>` (the DB's own timestamp string, unchanged)
    /// rather than the empty-string fallback the table used internally.
    #[tabled(rename = "Modified", display_with = "display_opt_string")]
    modified: Option<String>,
    #[tabled(rename = "SHA256")]
    sha256: String,
    /// Additive (issue #236 finding 5): the fact the `"d "` table prefix
    /// used to encode into `path` itself, with no way for a `--json`
    /// consumer to recover it now that `path` holds the real path.
    #[tabled(skip)]
    is_directory: bool,
}

impl FileRow {
    fn display_path(&self) -> String {
        format!(
            "{}{}",
            if self.is_directory { "d " } else { "  " },
            self.path
        )
    }
}

fn display_opt_string(v: &Option<String>) -> String {
    v.clone().unwrap_or_default()
}

/// `catalog ls --json` shape. `modified` was table-only before CTO decision
/// 2026-09-11 (architecture review C2 follow-up, C2b) added it as `Option<String>`.
fn file_rows_to_json(rows: &[FileRow]) -> serde_json::Value {
    serde_json::to_value(rows).unwrap()
}

/// Builds one `FileRow` from the `files` table's raw columns. Extracted
/// (issue #205) out of `Ls`'s `query_map` closure so this mapping --
/// specifically the `size`/`size_bytes` split -- is a function a test can
/// call directly, rather than something only provable by re-typing a
/// literal into the struct. That literal-only testing is exactly what let
/// #205's rounding regression through the existing JSON pin unnoticed: see
/// `pin_file_rows_json_shape`'s own doc comment.
fn file_row(
    raw_path: String,
    size: i64,
    is_dir: bool,
    modified: Option<String>,
    sha256_raw: Option<String>,
) -> FileRow {
    FileRow {
        path: raw_path,
        size: if is_dir {
            "-".into()
        } else {
            crate::util::format_bytes_binary(size)
        },
        size_bytes: if is_dir { None } else { Some(size) },
        modified,
        sha256: sha256_raw
            .map(|s| short_hash(&s))
            .unwrap_or_else(|| "(unstaged)".into()),
        is_directory: is_dir,
    }
}

#[derive(Tabled, Debug, Serialize)]
struct LocationRow {
    #[tabled(rename = "Volume")]
    volume: String,
    /// The volume's CURRENT lifecycle status (issue #57). Without this, a
    /// retired or erased volume was indistinguishable from a sealed one, so
    /// `catalog locate` could send an operator to fetch a cartridge that
    /// cannot serve a restore.
    #[tabled(rename = "Status")]
    status: String,
    /// The volume's `observed_condition` (ADR-0012's 2026-09-17 amendment,
    /// issue #242): "ok" or "quarantined". Since that amendment, a
    /// quarantined tape can still read `status = "sealed"` — a `Serviceable
    /// = NO` row whose `Status` column alone read "sealed" would be
    /// unexplained, exactly the fact-goes-invisible failure the amendment's
    /// "What follows from it" point 1 warns against.
    #[tabled(rename = "Condition")]
    condition: String,
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
    #[tabled(rename = "Serviceable", display_with = "display_serviceable")]
    serviceable: bool,
    /// The warehouse location(s) this volume has been DEPOSITED to
    /// (ADR-0006), or empty. `locate` answers "where do I go to get this
    /// back", and for a deposited volume one of the answers is not a
    /// building. Rendered as a separate column rather than folded into
    /// `Location`, which means "where the cartridge physically sits".
    #[tabled(rename = "Warehouse", display_with = "display_warehouse")]
    #[serde(rename = "warehouse_deposits")]
    warehouse: Vec<String>,
    /// Whether the CURRENT escrow recipient can still recover this volume
    /// (#125). `locate` answers "where do I go to get this back"; for a
    /// volume staged before an escrow swap the honest answer includes "and
    /// not with the escrow key". `-` when no escrow is registered at all;
    /// `?` when the row was rebuilt from a tape that carries no recipient
    /// list (#137) — not covered, but attestable.
    ///
    /// Sourced from `policy::escrow::stage_set_coverage` with
    /// `Scope::UnitAnyVolume`: answered for EVERY volume this listing shows,
    /// retired/erased/quarantined included, because `locate` lists those on
    /// purpose (#57) and the escrow question is about bytes, not custody — a
    /// retired cartridge either opens with the escrow key or it does not.
    /// `-` therefore means exactly one thing here: no escrow recipient is
    /// registered. `?` is a rebuilt row the tape could not vouch for (#137).
    #[tabled(rename = "Escrow")]
    escrow: String,
    /// Per-copy evidence age (issue #196, closing #14's deferred amendment
    /// to #57): when this specific TAPE was last PASSED-verified, not a
    /// claim about the tape's state right now (ADR-0001: the catalog is a
    /// ledger of claims; the tape is authoritative only at contact). `None`
    /// means no passed `volume verify` is on record at all -- rendered as
    /// `"never"` in the table, distinctly from an aged `"<n>d ago"`, so a
    /// never-checked copy cannot be mistaken for a recently-checked one
    /// (git-annex-`whereis` shape: report last-known state, say so).
    ///
    /// Sourced from `policy::evidence::per_volume_verification`, NOT
    /// `remaining_coverage_evidence`: that function gates every row through
    /// `coverage::eligible`, which would silently drop the evidence for
    /// exactly the retired/quarantined/erased volumes `locate` shows on
    /// purpose (#57). This column answers "when was this medium last
    /// checked", independent of whether it still counts as a copy -- the
    /// Status/Serviceable columns already answer that question.
    ///
    /// A warehouse deposit (ADR-0006) is a different evidence class with
    /// its own `Warehouse` column above; it never populates this field,
    /// because a deposit has never been verified by tapectl and never will
    /// be -- every row here comes from a completed tape `write` only.
    #[tabled(rename = "Verified", display_with = "display_verification_age")]
    last_verified: Option<String>,
}

fn display_serviceable(v: &bool) -> String {
    if *v { "yes" } else { "NO" }.to_string()
}

fn display_verification_age(v: &Option<String>) -> String {
    crate::policy::evidence::compact_age(v.as_deref(), chrono::Utc::now().naive_utc())
}

fn display_warehouse(v: &[String]) -> String {
    if v.is_empty() {
        "-".to_string()
    } else {
        v.join(",")
    }
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
/// cannot disagree with them about whether coverage exists. This is a
/// DIFFERENT question from the escrow marker below and deliberately keeps
/// its own source rather than routing through `policy::escrow`.
fn locate_rows(conn: &Connection, unit_id: i64) -> Result<Vec<LocationRow>> {
    let escrow = crate::db::queries::escrow_public_key(conn)?;
    let sealed = crate::policy::coverage::eligible("v");
    let sql = format!(
        "SELECT v.label, v.status, v.observed_condition, COALESCE(l.name, 'unknown'),
                s.version, ss.num_slices, w.completed_at,
                CASE WHEN {sealed} THEN 1 ELSE 0 END,
                (SELECT GROUP_CONCAT(dl.name)
                   FROM volume_deposits d
                   JOIN locations dl ON dl.id = d.location_id
                  WHERE d.volume_id = v.id),
                ss.id
         FROM snapshots s
         JOIN stage_sets ss ON ss.snapshot_id = s.id
         JOIN writes w ON w.stage_set_id = ss.id
         JOIN volumes v ON v.id = w.volume_id
         LEFT JOIN locations l ON l.id = v.location_id
         WHERE s.unit_id = ?1 AND w.status = 'completed'
         ORDER BY s.version DESC, v.label"
    );

    // The escrow marker's source: one call to `policy::escrow`, indexed by
    // `stage_set_id` rather than volume label — a volume can hold more than
    // one of this unit's stage sets bin-packed together, each with its own
    // verdict (e.g. staged before and after an escrow swap). See the
    // `escrow` field's doc comment for what a row absent from this map
    // means.
    let coverage: std::collections::HashMap<i64, crate::policy::escrow::Coverage> = match &escrow {
        Some(pk) => crate::policy::escrow::stage_set_coverage(
            conn,
            crate::policy::escrow::Scope::UnitAnyVolume(unit_id),
            pk,
        )?
        .into_iter()
        .map(|r| (r.stage_set_id, r.coverage))
        .collect(),
        None => std::collections::HashMap::new(),
    };

    // Per-copy evidence age (issue #196): one call to
    // `policy::evidence::per_volume_verification`, indexed by volume label
    // (`volumes.label` is `UNIQUE`, migration 001). Deliberately NOT
    // `remaining_coverage_evidence` -- see that field's doc comment on
    // `LocationRow` for why the eligibility-gated function would silently
    // drop evidence for exactly the retired/quarantined/erased volumes
    // this listing shows on purpose.
    let verification: std::collections::HashMap<String, Option<String>> =
        crate::policy::evidence::per_volume_verification(conn, unit_id)?
            .into_iter()
            .map(|e| (e.volume_label, e.last_verified))
            .collect();

    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt
        .query_map(params![unit_id], |row| {
            let volume: String = row.get(0)?;
            let serviceable: i64 = row.get(7)?;
            let stage_set_id: i64 = row.get(9)?;
            let last_verified = verification.get(&volume).cloned().flatten();
            Ok(LocationRow {
                status: row.get(1)?,
                condition: row.get(2)?,
                location: row.get(3)?,
                version: row.get(4)?,
                slices: row.get::<_, Option<i64>>(5)?.unwrap_or(0),
                written: row.get::<_, Option<String>>(6)?.unwrap_or_default(),
                serviceable: serviceable == 1,
                warehouse: row
                    .get::<_, Option<String>>(8)?
                    .map(|s| s.split(',').map(str::to_string).collect())
                    .unwrap_or_default(),
                escrow: match coverage.get(&stage_set_id) {
                    None => "-".to_string(),
                    Some(crate::policy::escrow::Coverage::Covered) => "yes".to_string(),
                    Some(crate::policy::escrow::Coverage::Unknown) => "?".to_string(),
                    Some(crate::policy::escrow::Coverage::Gap(_)) => "NO".to_string(),
                },
                last_verified,
                volume,
            })
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    Ok(rows)
}

/// `catalog locate --json` shape.
fn location_rows_to_json(rows: &[LocationRow]) -> serde_json::Value {
    serde_json::to_value(rows).unwrap()
}

pub fn run(
    conn: &Connection,
    config: &crate::config::Config,
    command: &CatalogCommands,
    json_output: bool,
    dry_run: bool,
) -> Result<()> {
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
                    Ok(file_row(
                        row.get::<_, String>(0)?,
                        size,
                        is_dir,
                        row.get::<_, Option<String>>(2)?,
                        row.get::<_, Option<String>>(3)?,
                    ))
                })?
                .collect::<std::result::Result<Vec<_>, _>>()?;

            if json_output {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&file_rows_to_json(&rows)).unwrap()
                );
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
                    println!(
                        "  {unit} v{ver}: {path} ({})",
                        crate::util::format_bytes_binary(*size)
                    );
                }
                println!("{} result(s)", rows.len());
            }
        }

        CatalogCommands::Locate { unit } => {
            let unit_row = crate::db::queries::get_unit_by_name(conn, unit)?
                .ok_or_else(|| TapectlError::UnitNotFound(unit.clone()))?;

            let rows = locate_rows(conn, unit_row.id)?;

            if json_output {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&location_rows_to_json(&rows)).unwrap()
                );
            } else if rows.is_empty() {
                println!("unit \"{unit}\" not found on any volume");
            } else {
                println!("{}", Table::new(&rows));
                // Say it in words too — a "NO" in a table column is easy to
                // skim past when you are about to walk to a shelf.
                let unserviceable: Vec<&str> = rows
                    .iter()
                    .filter(|r| !r.serviceable)
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
                // ADR-0004 Tier 1 / ADR-0001: the caveat itself has to reach
                // this output, not just a doc comment (issue #196, #14's
                // deferred amendment to #57) -- git-annex's `whereis` is
                // explicit that it does not contact remotes to verify, and
                // this command is the same shape: "Verified" is the
                // catalog's last-known claim, never a live check performed
                // just now.
                println!(
                    "\nnote: \"Verified\" is this catalog's last-known record of each \
                     copy's most recent PASSED `volume verify` — not a check of the tape \
                     performed just now. \"never\" means no passed verification is on \
                     record, not that the copy is bad; an aged value does not mean the \
                     tape has since failed. Re-run `tapectl volume verify <label>` to \
                     refresh it."
                );
            }
        }

        CatalogCommands::Rebuild {
            from_volume,
            device,
            key,
            label,
            tenant,
        } => {
            if !from_volume {
                return Err(TapectlError::Other(
                    "catalog rebuild needs a source: pass --from-volume to rebuild \
                     from a sealed tape"
                        .to_string(),
                ));
            }
            // Issue #241: a real preview would have to open the drive
            // and chain-walk the whole volume — the exact set of rows a
            // real run would insert is not known any other way.
            if dry_run {
                return Err(crate::cli::refuse_dry_run(
                    "catalog rebuild",
                    "a real preview would have to open the drive and chain-walk the whole \
                     volume to know what rows would be inserted.",
                ));
            }
            // LENIENT (ADR-0010): rebuild is the disaster-recovery read
            // path — the machine running it typically has keys and no
            // `backend add` yet, so the backend is optional and only names
            // the resulting volume row's `backend_name`.
            let (device, backend) = crate::config::resolve_device(config, device.as_deref())?;
            // Issue #166: refuse before the store is opened if this drive
            // cannot read the loaded medium. Proceeds silently with no
            // configured backend — this is the disaster-recovery path this
            // command exists for (ADR-0005).
            // Both of this path's MAM reads (this one and the pre-store read
            // inside `rebuild_from_volume`) are held and journalled against
            // the rebuild's contact (issue #297).
            let reads = crate::tape::mam_journal::MamReads::new(
                conn,
                crate::tape::contact::Operation::CatalogRebuild,
            );
            reads.check_read_contact(config, &device)?;
            let scratch =
                std::env::temp_dir().join(format!("tapectl-rebuild-{}", std::process::id()));
            let report = crate::volume::rebuild::rebuild_from_volume(
                conn,
                config,
                &device,
                DEFAULT_BLOCK_SIZE,
                key,
                label.as_deref(),
                tenant,
                backend.map(|b| b.name.as_str()),
                &scratch,
                &reads,
            );
            // The scratch dir holds decrypted MANIFEST/catalog.db copies —
            // remove it on every path out, success or failure, exactly as
            // `RestoreScratch` does for decrypted slices (#102).
            let _ = std::fs::remove_dir_all(&scratch);
            let report = report?;

            if json_output {
                println!(
                    "{}",
                    serde_json::json!({
                        "label": report.label,
                        "uuid": report.uuid,
                        "envelopes_opened": report.envelopes_opened,
                        "volume_inserted": report.volume_inserted,
                        "tenants": report.tenants,
                        "units": report.units,
                        "snapshots": report.snapshots,
                        "stage_sets": report.stage_sets,
                        "slices": report.slices,
                        "writes": report.writes,
                        "positions": report.positions,
                        "files": report.files,
                        "had_catalog_db": report.had_catalog_db,
                        "tenants_from_catalog_db": report.tenants_from_catalog_db,
                        "receipts_from_tape": report.receipts_from_tape,
                        "attested": report.attested,
                        "key_is_escrow": report.key_is_escrow,
                        "unknown_remaining": report.unknown_remaining,
                        // Issue #236 finding 1: the three distinct arms
                        // `attest_escrow` can leave a stage set unattested
                        // on, so a consumer can tell a permanent Gap
                        // ("not_recipient") apart from a possibly-transient
                        // Unknown ("unreadable"/"unparseable") instead of
                        // reading one collapsed sentence.
                        "escrow_attest_not_recipient": report.escrow_attest_not_recipient,
                        "escrow_attest_unreadable": report.escrow_attest_unreadable,
                        "escrow_attest_unparseable": report.escrow_attest_unparseable,
                        "units_without_tenant_envelope": report.units_without_tenant_envelope,
                        "no_changes": report.is_noop(),
                        "volume_status_mismatch": report.volume_status_mismatch,
                        // Issue #165: additive, every key above unchanged.
                        "cartridge_barcode": report.cartridge_barcode,
                        "cartridge_registered": report.cartridge_registered,
                        "cartridge_bound": report.cartridge_bound,
                        "serial_learned": report.serial_learned,
                        "serial_checked": report.serial_checked,
                        "cartridge_barcode_superseded": report.cartridge_barcode_superseded,
                        "displaced": report
                            .displaced
                            .iter()
                            .map(|d| d.label.as_str())
                            .collect::<Vec<_>>(),
                        // Issue #235: the ADR-0004 impact evidence
                        // `mount_and_record` computes and `catalog rebuild`
                        // used to discard. A sibling key rather than a new
                        // shape for `displaced` above, which stays exactly
                        // the array of labels issue #165 shipped.
                        "displacements": displacements_json(&report),
                        "cartridge_retired": report.cartridge_retired,
                        "unbound_reason": report.unbound_reason,
                        // Issue #242: additive, every key above unchanged.
                        // Since the 2026-09-17 amendment a verify-quarantined
                        // row's `status` stays 'sealed', so it no longer
                        // trips `volume_status_mismatch` above -- this is
                        // the sibling that still surfaces it.
                        "volume_condition_mismatch": report.volume_condition_mismatch,
                    })
                );
            } else {
                println!(
                    "rebuilt from volume \"{}\" (uuid {}), {} envelope(s) opened",
                    report.label, report.uuid, report.envelopes_opened
                );
                if report.is_noop() {
                    println!("  no changes — the catalog already knew this volume");
                } else {
                    println!(
                        "  inserted: {} tenant(s), {} unit(s), {} snapshot(s), {} stage set(s),",
                        report.tenants, report.units, report.snapshots, report.stage_sets
                    );
                    println!(
                        "            {} slice(s), {} write(s), {} position(s), {} file row(s){}",
                        report.slices,
                        report.writes,
                        report.positions,
                        report.files,
                        if report.volume_inserted {
                            ", 1 volume"
                        } else {
                            ""
                        }
                    );
                }
                // Issue #165, outside the is_noop() branch for the same
                // reason as the volume_status_mismatch warning below: an
                // idempotent second run still has a cartridge to name (or a
                // reason it has none), even when nothing new was written.
                match (&report.cartridge_barcode, &report.unbound_reason) {
                    (Some(barcode), _) if report.cartridge_registered => {
                        println!(
                            "  cartridge \"{barcode}\" registered from this tape's own \
                             identity and bound"
                        );
                    }
                    (Some(barcode), _) if report.cartridge_bound => {
                        println!("  bound to cartridge \"{barcode}\"");
                        if report.serial_learned {
                            println!("    medium serial learned onto cartridge \"{barcode}\"");
                        }
                    }
                    (Some(barcode), _) => {
                        println!("  already on cartridge \"{barcode}\" (unchanged)");
                    }
                    (None, Some(reason)) => {
                        println!("  warning: rebuilt unbound — {reason}");
                    }
                    (None, None) => {}
                }
                if let Some(superseded) = &report.cartridge_barcode_superseded {
                    // Issue #236 finding 3: this used to say "`--cartridge`
                    // ... was superseded", naming a flag `CatalogCommands::Rebuild`
                    // does not have. See `cartridge_supersession_note`'s doc.
                    let winner = report.cartridge_barcode.as_deref().unwrap_or("?");
                    println!("{}", cartridge_supersession_note(superseded, winner));
                }
                // Issue #235: the SAME lines `volume init` prints, from the
                // one renderer (`binding::render_displacement`, already run
                // in `volume::rebuild`). This used to print the label alone,
                // so a unit could reach ZERO copies during a rebuild — an
                // irreversible step, ungated by design (ADR-0012) — with
                // nothing saying so. ADR-0004 Tier 1 names withholding that
                // as the option it rejects.
                for d in &report.displaced {
                    for line in &d.warning {
                        println!("  {line}");
                    }
                }
                if report.cartridge_retired {
                    println!(
                        "  note: that cartridge is retired_permanent — the mount is recorded \
                         but its status was left alone (ADR-0011)"
                    );
                }
                // Outside the is_noop() branch on purpose: a second rebuild
                // onto a row that is still non-sealed is a no-op for row
                // counts and must still warn every time (issue #158).
                if let Some(status) = &report.volume_status_mismatch {
                    println!(
                        "  warning: volume \"{}\" was already in this catalog as \"{status}\", \
                         not sealed — the rebuild attached its units to that row and left the \
                         status alone; until it is sealed, nothing on it counts as a copy. Run \
                         `tapectl volume verify {}` to check the tape; the status itself will \
                         not change automatically",
                        report.label, report.label
                    );
                }
                // Issue #242: the sibling warning. Since the 2026-09-17
                // amendment a verify-quarantined row's `status` stays
                // 'sealed', so the warning above never fires for it -- this
                // is the fact that would otherwise go silently invisible.
                if let Some(condition) = &report.volume_condition_mismatch {
                    println!(
                        "  warning: volume \"{}\" was already in this catalog with \
                         observed_condition \"{condition}\" — the rebuild attached its units to \
                         that row and left the condition alone; until it reads \"ok\" again, \
                         nothing on it counts as a copy. Run `tapectl volume verify {}` to check \
                         the tape; the condition itself will not change automatically",
                        report.label, report.label
                    );
                }
                if !report.had_catalog_db {
                    println!(
                        "  note: this tape carries no catalog.db (written before issue #83) — \
                         the restore path is complete, but there is no per-file index and \
                         each snapshot's original source path is unknown"
                    );
                }
                if !report.units_without_tenant_envelope.is_empty() {
                    println!(
                        "  note: no tenant envelope on this cartridge names the owner of {} — \
                         filed under tenant \"{}\"",
                        report.units_without_tenant_envelope.join(", "),
                        tenant
                    );
                }
                println!(
                    "  the slice hashes recorded are the tape's own claim; run \
                     `tapectl volume verify {}` to check them",
                    report.label
                );
                if report.attested > 0 {
                    println!(
                        "  attested: {} stage set(s) — the escrow key decrypted a slice header",
                        report.attested
                    );
                }
                if report.receipts_from_tape > 0 {
                    println!(
                        "  escrow receipts: {} stage set(s) carried theirs on the tape",
                        report.receipts_from_tape
                    );
                }
                if report.unknown_remaining > 0 {
                    // Reachable with or without a registered escrow, so the
                    // remedy must branch on that instead of assuming the
                    // no-escrow path — `key import --escrow` is refused
                    // outright once one is registered (issue #214, finding
                    // 2-4 sibling: same trap as `escrow_identity_findings`).
                    let escrow_registered = crate::db::queries::escrow_public_key(conn)?.is_some();
                    for line in escrow_unknown_lines(&report, escrow_registered) {
                        println!("{line}");
                    }
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
                println!(
                    "  Total:     {}",
                    crate::util::format_bytes_binary(total_size)
                );
                println!("  Volumes:   {volume_count}");
            }
        }
    }
    Ok(())
}

/// The `escrow: ... still report \`?\` (unknown)` lines `catalog rebuild`
/// prints when `report.unknown_remaining > 0`, extracted out of `run`'s
/// match arm (issue #236 finding 1) so the exact wording per cause is a
/// function a test can call directly, rather than something only provable
/// by capturing stdout off a full tape rebuild.
///
/// `attest_escrow` (`volume::rebuild`) leaves a stage set unattested on
/// THREE different arms, and only one of them — the escrow key
/// demonstrably is not a recipient (`escrow_attest_not_recipient`) — is the
/// permanent Gap statement (ADR-0005/#137's `Coverage::Gap`); the other two
/// (`escrow_attest_unreadable`/`escrow_attest_unparseable`) are `Unknown`:
/// the bytes may be perfectly escrow-covered and merely unreadable or
/// unparseable at this position, a different problem with a different
/// remedy. Printing the Gap sentence for all three (the pre-#236 behavior)
/// collapsed `Unknown` into `Gap` at exactly the moment #137 invented the
/// distinction for.
fn escrow_unknown_lines(report: &RebuildReport, escrow_registered: bool) -> Vec<String> {
    let mut lines = Vec::new();
    if !report.key_is_escrow {
        // Reachable with or without a registered escrow, so the remedy must
        // branch on that instead of assuming the no-escrow path — `key
        // import --escrow` is refused outright once one is registered
        // (issue #214, finding 2-4 sibling: same trap as
        // `escrow_identity_findings`). Both remedies name `--from-volume`
        // (issue #236 finding 3 sub-finding): `catalog rebuild` hard-refuses
        // without it (`volume::rebuild`'s pre-transaction guard), so a
        // remedy omitting it sends the operator to a command that refuses.
        if escrow_registered {
            lines.push(format!(
                "  escrow: {} rebuilt stage set(s) on this volume still report `?` (unknown) \
                 — re-run `tapectl catalog rebuild --from-volume --key <the REGISTERED escrow \
                 secret key>` to attest them, or re-stage",
                report.unknown_remaining
            ));
        } else {
            lines.push(format!(
                "  escrow: {} rebuilt stage set(s) on this volume still report `?` (unknown) \
                 — no escrow is registered in this catalog yet: register the ORIGINAL escrow \
                 public key with `tapectl key import --escrow <key>`, then re-run `tapectl \
                 catalog rebuild --from-volume --key <its secret key file>` to attest them, or \
                 re-stage",
                report.unknown_remaining
            ));
        }
        return lines;
    }

    if report.escrow_attest_not_recipient > 0 {
        lines.push(format!(
            "  escrow: {} rebuilt stage set(s) still report `?` (unknown) — the escrow key \
             is not a recipient of their slices (permanent: re-running will not change this)",
            report.escrow_attest_not_recipient
        ));
    }
    if report.escrow_attest_unreadable > 0 {
        lines.push(format!(
            "  escrow: {} rebuilt stage set(s) still report `?` (unknown) — their slice \
             header could not be read; the bytes may still be escrow-covered and merely \
             unreadable at this position — try `tapectl volume verify {}`, a copy on another \
             cartridge, or re-running this rebuild",
            report.escrow_attest_unreadable, report.label
        ));
    }
    if report.escrow_attest_unparseable > 0 {
        lines.push(format!(
            "  escrow: {} rebuilt stage set(s) still report `?` (unknown) — their slice \
             header did not parse; coverage stays unknown",
            report.escrow_attest_unparseable
        ));
    }
    let observed = report.escrow_attest_not_recipient
        + report.escrow_attest_unreadable
        + report.escrow_attest_unparseable;
    if observed == 0 {
        // Every currently-unknown stage set predates this run's attestation
        // attempt (already reported unknown by an earlier rebuild) — there
        // was nothing new for this run to try.
        lines.push(format!(
            "  escrow: {} rebuilt stage set(s) on this volume still report `?` (unknown) — \
             the escrow key found nothing new to attest this run",
            report.unknown_remaining
        ));
    }
    lines
}

/// The `note: ...` line `catalog rebuild` prints when this tape's File 0
/// named one cartridge but the medium's own serial matched a DIFFERENT
/// registered cartridge (ADR-0012's "the loaded tape *is* that other
/// cartridge"), extracted (issue #236 finding 3) so the exact wording is
/// directly assertable.
///
/// `CatalogCommands::Rebuild` (above) has no `--cartridge` flag — only
/// `--from-volume`/`--device`/`--key`/`--label`/`--tenant` — so this note
/// must never name one; `superseded` is File 0's own recorded barcode (an
/// operator claim from write time, never something an operator typed on
/// this command), not something a flag supplied.
fn cartridge_supersession_note(superseded: &str, winner: &str) -> String {
    format!(
        "  note: this tape's File 0 names cartridge \"{superseded}\", but the medium's own \
         serial matches registered cartridge \"{winner}\" — the serial wins (ADR-0012); the \
         mount was recorded against \"{winner}\""
    )
}

/// First 12 characters of a hash, elided — or the whole thing when it is
/// shorter than that.
///
/// Issue #110: this was `&s[..12]`, which PANICS on a string shorter than 12
/// bytes (and, in general, on a byte index that falls mid-UTF-8). Hashes are
/// hex so length is the live risk, and a truncated or hand-edited row should
/// never crash a read-only listing.
fn short_hash(s: &str) -> String {
    match s.get(..12) {
        Some(head) => format!("{head}..."),
        None => s.to_string(),
    }
}

/// `catalog rebuild --json`'s `displacements` array (issue #235): one object
/// per displaced volume, carrying the ADR-0004 impact evidence the text
/// rendering names.
///
/// A function rather than an inline `json!` block so the JSON path is
/// testable without a device — the text path already is, through
/// `RebuildReport::displaced[..].warning`. Both read the same
/// `report.displaced`, which is the point of issue #235.
fn displacements_json(report: &crate::volume::rebuild::RebuildReport) -> serde_json::Value {
    serde_json::Value::Array(
        report
            .displaced
            .iter()
            .map(|d| {
                serde_json::json!({
                    "label": d.label,
                    "units": d.units.iter().map(|u| serde_json::json!({
                        "unit": u.unit_name,
                        "status": u.unit_status,
                        "other_copies": u.other_copies,
                    })).collect::<Vec<_>>(),
                    // Denormalised on purpose: the one fact ADR-0004 Tier 1
                    // is about should not need a filter to find.
                    "zero_copy_units": d.zero_copy_units(),
                })
            })
            .collect(),
    )
}

#[cfg(test)]
mod tests {
    // --- issue #110 item 1: short-hash elision must not panic ---

    #[test]
    fn short_hash_elides_a_full_length_hash() {
        let h = "0123456789abcdef0123456789abcdef";
        assert_eq!(super::short_hash(h), "0123456789ab...");
    }

    /// The actual defect: `&s[..12]` panicked here. A read-only listing must
    /// survive a truncated or hand-edited row.
    #[test]
    fn short_hash_returns_a_short_string_whole_instead_of_panicking() {
        assert_eq!(super::short_hash("abc"), "abc");
        assert_eq!(super::short_hash(""), "");
    }

    /// Exactly 12 is the boundary `..12` gets right and off-by-one gets wrong.
    #[test]
    fn short_hash_handles_the_exact_boundary() {
        assert_eq!(super::short_hash("0123456789ab"), "0123456789ab...");
    }

    /// Byte-slicing also panics mid-UTF-8. Hashes are hex, but the function
    /// is a general string helper and must not be a landmine for a future
    /// caller.
    #[test]
    fn short_hash_does_not_panic_on_multibyte_characters() {
        let s = "ααααααα"; // 7 chars, 14 bytes — byte 12 is mid-character
        let out = super::short_hash(s);
        assert!(!out.is_empty());
    }

    use super::*;
    use crate::db;

    // --- Issue #236 finding 1: `escrow_unknown_lines` must print the cause
    // actually observed, not assert the Gap sentence for every arm. ---

    #[test]
    fn escrow_unknown_lines_names_the_gap_for_a_confirmed_non_recipient() {
        let report = RebuildReport {
            key_is_escrow: true,
            escrow_attest_not_recipient: 2,
            unknown_remaining: 2,
            ..Default::default()
        };
        let lines = escrow_unknown_lines(&report, true);
        assert_eq!(lines.len(), 1, "{lines:?}");
        assert!(
            lines[0].contains("is not a recipient"),
            "the confirmed-Gap arm must say so: {lines:?}"
        );
        assert!(
            lines[0].contains("permanent"),
            "a confirmed non-recipient never changes on re-run: {lines:?}"
        );
    }

    /// The defect this fix is about: an unreadable slice header is an I/O
    /// failure, not proof the escrow key is not a recipient. Pre-#236 this
    /// printed the SAME "not a recipient" sentence `escrow_attest_not_recipient`
    /// gets above -- collapsing Unknown into Gap.
    #[test]
    fn escrow_unknown_lines_never_asserts_gap_for_an_unreadable_header() {
        let report = RebuildReport {
            key_is_escrow: true,
            escrow_attest_unreadable: 1,
            unknown_remaining: 1,
            label: "L6-0004".to_string(),
            ..Default::default()
        };
        let lines = escrow_unknown_lines(&report, true);
        assert_eq!(lines.len(), 1, "{lines:?}");
        assert!(
            !lines[0].contains("is not a recipient"),
            "an unreadable header must never assert the confirmed Gap sentence: {lines:?}"
        );
        assert!(
            lines[0].contains("could not be read"),
            "the unreadable arm must say so: {lines:?}"
        );
        assert!(
            lines[0].contains("volume verify L6-0004"),
            "the remedy must name the actual volume: {lines:?}"
        );
    }

    /// Same shape, the other arm: a header that could not PARSE is also not
    /// proof of non-recipiency.
    #[test]
    fn escrow_unknown_lines_never_asserts_gap_for_an_unparseable_header() {
        let report = RebuildReport {
            key_is_escrow: true,
            escrow_attest_unparseable: 1,
            unknown_remaining: 1,
            ..Default::default()
        };
        let lines = escrow_unknown_lines(&report, true);
        assert_eq!(lines.len(), 1, "{lines:?}");
        assert!(
            !lines[0].contains("is not a recipient"),
            "an unparseable header must never assert the confirmed Gap sentence: {lines:?}"
        );
        assert!(lines[0].contains("did not parse"), "{lines:?}");
    }

    /// All three arms can occur in one run (a damaged patch of tape plus a
    /// genuinely uncovered unit) -- each must get its own line.
    #[test]
    fn escrow_unknown_lines_reports_every_observed_cause_separately() {
        let report = RebuildReport {
            key_is_escrow: true,
            escrow_attest_not_recipient: 1,
            escrow_attest_unreadable: 1,
            escrow_attest_unparseable: 1,
            unknown_remaining: 3,
            label: "L6-0004".to_string(),
            ..Default::default()
        };
        let lines = escrow_unknown_lines(&report, true);
        assert_eq!(lines.len(), 3, "{lines:?}");
    }

    /// The sub-finding: the no-escrow-attempted remedies must name
    /// `--from-volume`, the flag `catalog rebuild` hard-refuses without.
    #[test]
    fn escrow_unknown_lines_remedy_names_from_volume() {
        let report = RebuildReport {
            key_is_escrow: false,
            unknown_remaining: 1,
            ..Default::default()
        };
        for registered in [true, false] {
            let lines = escrow_unknown_lines(&report, registered);
            assert_eq!(lines.len(), 1, "{lines:?}");
            assert!(
                lines[0].contains("catalog rebuild --from-volume"),
                "the remedy must name --from-volume (registered={registered}): {lines:?}"
            );
        }
    }

    // --- Issue #236 finding 3: the supersession note must name only
    // things that run in the state that prints it. ---

    #[test]
    fn cartridge_supersession_note_never_names_a_nonexistent_flag() {
        let note = cartridge_supersession_note("OLD-BARCODE", "NEW-BARCODE");
        assert!(
            !note.contains("--cartridge"),
            "`catalog rebuild` has no --cartridge flag: {note}"
        );
        assert!(note.contains("OLD-BARCODE"), "{note}");
        assert!(note.contains("NEW-BARCODE"), "{note}");
        assert!(note.contains("File 0"), "{note}");
        assert!(note.contains("ADR-0012"), "{note}");
    }

    // --- C2 pins: `catalog ls`/`catalog locate` --json shapes, locked down
    // BEFORE the row structs are retyped (issue: C2 row-listing drift). ---

    /// Issue #236 finding 5: `path` used to store `"d "`/`"  "` prefixed
    /// onto the real path for table display, with `.trim()` clearing it for
    /// JSON -- which strips whitespace only, so a directory row's `"d "`
    /// prefix SURVIVED into JSON (`"path": "d subdir"`, matching nothing on
    /// tape or disk and indistinguishable from a genuine file literally
    /// named that). This pin was updated to keep asserting that broken
    /// value, which was itself part of the defect (a test asserting
    /// yesterday's wrong output is not "pinned", it is stale). `path` now
    /// holds the real path unconditionally and `is_directory` is the
    /// additive fact a `--json` consumer needs to tell the two apart; the
    /// `"d "` prefix survives ONLY in the table, via `Self::display_path`.
    #[test]
    /// This pins the JSON SHAPE — key names, key ordering, null handling —
    /// and nothing else. It feeds `size` AND `size_bytes` in as literals, so
    /// the humaniser is never called and this test cannot catch a unit
    /// change; the formatter itself is pinned in `crate::util`. The literal
    /// moved `KB` -> `KiB` with issue #204 only so the fixture stops
    /// modelling output the code no longer produces. Do not add a unit
    /// assertion here: put it on the formatter, where it can actually fail.
    /// `size_bytes` (issue #205) is additive: `None` for the directory row,
    /// `Some(n)` for the file row -- what production actually maps is
    /// proven separately below, by calling `file_row` directly, since a
    /// literal here proves only that the struct serialises, not that the
    /// mapping into it is correct (the exact gap #205 found).
    fn pin_file_rows_json_shape() {
        let rows = vec![
            FileRow {
                path: "subdir".to_string(),
                size: "-".to_string(),
                size_bytes: None,
                modified: Some("2026-01-01T00:00:00Z".to_string()),
                sha256: "(unstaged)".to_string(),
                is_directory: true,
            },
            FileRow {
                path: "some/file.txt".to_string(),
                size: "1.2 KiB".to_string(),
                size_bytes: Some(1229),
                modified: None,
                sha256: "0123456789ab...".to_string(),
                is_directory: false,
            },
        ];
        let value = file_rows_to_json(&rows);
        assert_eq!(
            serde_json::to_string(&value).unwrap(),
            r#"[{"is_directory":true,"modified":"2026-01-01T00:00:00Z","path":"subdir","sha256":"(unstaged)","size":"-","size_bytes":null},{"is_directory":false,"modified":null,"path":"some/file.txt","sha256":"0123456789ab...","size":"1.2 KiB","size_bytes":1229}]"#
        );
    }

    /// Issue #205: `size_bytes` must carry the exact byte count even when
    /// the humaniser's one-decimal rounding makes two different sizes
    /// render identically -- 1_234_567 and 1_240_000 both round to
    /// `"1.2 MiB"`. Calling `file_row` directly (the real production
    /// mapping, not a literal restated into the struct) is the point: the
    /// existing pin above never calls it and so never called the humaniser
    /// either, which is exactly how a rounding regression got through
    /// unnoticed.
    #[test]
    fn size_bytes_survives_the_humanisers_rounding_collision() {
        let a = file_row("a".to_string(), 1_234_567, false, None, None);
        let b = file_row("b".to_string(), 1_240_000, false, None, None);
        assert_eq!(
            a.size, b.size,
            "both sizes should round to the same display string"
        );
        assert_eq!(a.size_bytes, Some(1_234_567));
        assert_eq!(b.size_bytes, Some(1_240_000));
        assert_ne!(a.size_bytes, b.size_bytes);
    }

    /// A directory row's `size_bytes` is `None`, not `Some(0)`. The `files`
    /// table's `size_bytes` column literally stores `0` for a directory
    /// (`staging/mod.rs`'s manifest walk), which is a placeholder for "not
    /// measured", not a real content size -- `Some(0)` would make a
    /// directory indistinguishable from a genuine empty file to a machine
    /// JSON consumer.
    #[test]
    fn directory_row_has_no_size_bytes() {
        let row = file_row("subdir".to_string(), 0, true, None, None);
        assert_eq!(row.size, "-");
        assert_eq!(row.size_bytes, None);
    }

    /// Issue #236 finding 5: `path` must hold the REAL path unconditionally
    /// -- the `"d "` table marker used to be baked into `path` itself by
    /// `file_row`, so a directory's JSON `path` came out `"d subdir"`,
    /// matching nothing on tape or disk and indistinguishable from a
    /// genuine file literally named that. `is_directory` is the additive
    /// fact a `--json` consumer needs; the marker survives ONLY in
    /// `Self::display_path`, the table-only rendering.
    #[test]
    fn file_row_path_carries_no_table_prefix_is_directory_is_additive() {
        let dir = file_row("subdir".to_string(), 0, true, None, None);
        assert_eq!(dir.path, "subdir", "the real path, not \"d subdir\"");
        assert!(dir.is_directory);
        assert_eq!(dir.display_path(), "d subdir", "the prefix is table-only");

        let file = file_row("some/file.txt".to_string(), 10, false, None, None);
        assert_eq!(file.path, "some/file.txt");
        assert!(!file.is_directory);
        assert_eq!(file.display_path(), "  some/file.txt");
    }

    /// Pins the WHOLE `--json` shape, including `last_verified` (issue
    /// #196) and `condition` (issue #242). Every key present before this
    /// change keeps its exact prior value and position (`serde_json::
    /// Value`'s map is a `BTreeMap`, so keys sort alphabetically) --
    /// `condition` is additive alongside them, per the C2b discipline. One
    /// row is aged, the other `null` (never verified), and one row's
    /// condition is quarantined while its status still reads "sealed" (the
    /// exact shape the amendment is about), so the pin also proves the two
    /// render as different JSON values, not just different table text.
    #[test]
    fn pin_location_rows_json_shape() {
        let rows = vec![
            LocationRow {
                volume: "L6-0001".to_string(),
                status: "sealed".to_string(),
                condition: "ok".to_string(),
                location: "unknown".to_string(),
                version: 1,
                slices: 3,
                written: "2026-07-01T00:00:00Z".to_string(),
                serviceable: true,
                warehouse: vec![],
                escrow: "-".to_string(),
                last_verified: Some("2026-06-01 00:00:00".to_string()),
            },
            LocationRow {
                volume: "L6-0002".to_string(),
                status: "sealed".to_string(),
                condition: "quarantined".to_string(),
                location: "parents-house".to_string(),
                version: 2,
                slices: 0,
                written: String::new(),
                serviceable: false,
                warehouse: vec!["glacier".to_string(), "vault2".to_string()],
                escrow: "?".to_string(),
                last_verified: None,
            },
        ];
        let value = location_rows_to_json(&rows);
        assert_eq!(
            serde_json::to_string(&value).unwrap(),
            r#"[{"condition":"ok","escrow":"-","last_verified":"2026-06-01 00:00:00","location":"unknown","serviceable":true,"slices":3,"status":"sealed","version":1,"volume":"L6-0001","warehouse_deposits":[],"written":"2026-07-01T00:00:00Z"},{"condition":"quarantined","escrow":"?","last_verified":null,"location":"parents-house","serviceable":false,"slices":0,"status":"sealed","version":2,"volume":"L6-0002","warehouse_deposits":["glacier","vault2"],"written":""}]"#
        );
    }

    /// Rule #6's discipline made concrete: every key that existed BEFORE
    /// this change is still present with its old meaning, and the new key
    /// is additive alongside them -- not a replacement for any of them.
    #[test]
    fn json_gains_last_verified_additively_without_disturbing_existing_keys() {
        let rows = vec![LocationRow {
            volume: "L6-0001".to_string(),
            status: "sealed".to_string(),
            condition: "ok".to_string(),
            location: "unknown".to_string(),
            version: 1,
            slices: 3,
            written: "2026-07-01T00:00:00Z".to_string(),
            serviceable: true,
            warehouse: vec![],
            escrow: "-".to_string(),
            last_verified: Some("2026-06-01 00:00:00".to_string()),
        }];
        let value = location_rows_to_json(&rows);
        let obj = value[0].as_object().unwrap();
        assert_eq!(obj.get("volume").unwrap(), "L6-0001");
        assert_eq!(obj.get("status").unwrap(), "sealed");
        assert_eq!(obj.get("location").unwrap(), "unknown");
        assert_eq!(obj.get("version").unwrap(), 1);
        assert_eq!(obj.get("slices").unwrap(), 3);
        assert_eq!(obj.get("written").unwrap(), "2026-07-01T00:00:00Z");
        assert_eq!(obj.get("serviceable").unwrap(), true);
        assert!(obj
            .get("warehouse_deposits")
            .unwrap()
            .as_array()
            .unwrap()
            .is_empty());
        assert_eq!(obj.get("escrow").unwrap(), "-");
        // The new keys (issue #196's last_verified, issue #242's condition).
        assert_eq!(obj.get("last_verified").unwrap(), "2026-06-01 00:00:00");
        assert_eq!(obj.get("condition").unwrap(), "ok");
    }

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

        // Volume 2: the status/condition under test, WITH a location
        // recorded. Issue #242: 'quarantined' is a condition now, not a
        // status -- translate it onto `observed_condition`, leaving
        // `status` at 'sealed'.
        let (status_value, condition_value) = if second_status == "quarantined" {
            ("sealed", "quarantined")
        } else {
            (second_status, "ok")
        };
        conn.execute(
            &format!(
                "INSERT INTO volumes (label, backend_type, backend_name, media_type, capacity_bytes, status, observed_condition, location_id)
                 VALUES ('L6-OTHER', 'lto', 'lto0', 'LTO-6', 2500000000000, '{status_value}', '{condition_value}', {loc_id})"
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
        // Issue #242: 'quarantined' shows up in the CONDITION column now,
        // with `status` reading 'sealed' underneath it -- the status column
        // alone must show the real state for every OTHER status, unchanged.
        if status == "quarantined" {
            assert_eq!(
                other.status, "sealed",
                "status column must show the real state"
            );
            assert_eq!(
                other.condition, "quarantined",
                "condition column must show the real state"
            );
        } else {
            assert_eq!(
                other.status, status,
                "status column must show the real state"
            );
            assert_eq!(other.condition, "ok");
        }
        assert!(
            !other.serviceable,
            "a {status} volume cannot serve a restore (ADR-0004)"
        );

        let sealed = rows
            .iter()
            .find(|r| r.volume == "L6-SEALED")
            .expect("the sealed volume must appear");
        assert!(sealed.serviceable);
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

    /// Issue #196: `locate_rows`'s MERGE of `per_volume_verification` into
    /// each row, not just the query in isolation. A wrong key or a
    /// silently-defaulted lookup would make every row read `never` and
    /// every other test here would still pass -- the JSON pins build
    /// `LocationRow` literals directly, and `policy::evidence`'s own tests
    /// never touch `catalog::locate_rows` at all. This is the one place
    /// that exercises the actual merge, end to end.
    ///
    /// Also pins the `outcome = 'passed'` filter at this layer: L6-SEALED
    /// gets a `'failed'` session and must still render as never-verified,
    /// not pick up the failed attempt's timestamp.
    #[test]
    fn locate_rows_carries_last_verified_from_the_evidence_module_per_volume() {
        let conn = setup_unit_on_two_volumes("loc-verified", "sealed");
        let other_id: i64 = conn
            .query_row("SELECT id FROM volumes WHERE label = 'L6-OTHER'", [], |r| {
                r.get(0)
            })
            .unwrap();
        let sealed_id: i64 = conn
            .query_row(
                "SELECT id FROM volumes WHERE label = 'L6-SEALED'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        conn.execute(
            "INSERT INTO verification_sessions (volume_id, completed_at, outcome)
             VALUES (?1, '2026-08-01 12:00:00', 'passed')",
            params![other_id],
        )
        .unwrap();
        // A FAILED session on the other volume must not count as evidence
        // -- `outcome = 'passed'` lives in the join's ON clause precisely
        // so this row still renders `None`, not the failed attempt's date.
        conn.execute(
            "INSERT INTO verification_sessions (volume_id, completed_at, outcome)
             VALUES (?1, '2026-08-15 00:00:00', 'failed')",
            params![sealed_id],
        )
        .unwrap();

        let rows = locate_rows(&conn, unit_id_of(&conn, "loc-verified")).unwrap();
        assert_eq!(rows.len(), 2);

        let verified = rows.iter().find(|r| r.volume == "L6-OTHER").unwrap();
        assert_eq!(
            verified.last_verified.as_deref(),
            Some("2026-08-01 12:00:00"),
            "L6-OTHER's passed session must surface as its last_verified"
        );

        let never = rows.iter().find(|r| r.volume == "L6-SEALED").unwrap();
        assert_eq!(
            never.last_verified, None,
            "L6-SEALED has only a FAILED session -- it must still render as \
             never-verified, not pick up the failed attempt's timestamp: {never:?}"
        );
    }

    /// Issue #73 / ADR-0006: `locate` answers "where do I go to get this
    /// back". For a deposited volume one of the answers is a warehouse,
    /// and it must be visible as its own column -- never folded into
    /// `Location`, which means where the CARTRIDGE physically sits.
    #[test]
    fn locate_shows_a_volumes_warehouse_deposits() {
        let (conn, unit_id, vol) =
            crate::policy::coverage::tests::setup_unit_with_deposit("active");
        let rows = locate_rows(&conn, unit_id).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(
            rows[0].location, "home",
            "the cartridge is still on a shelf"
        );
        assert_eq!(rows[0].warehouse, vec!["glacier".to_string()]);

        conn.execute(
            "DELETE FROM volume_deposits WHERE volume_id = ?1",
            params![vol],
        )
        .unwrap();
        let rows = locate_rows(&conn, unit_id).unwrap();
        assert!(
            rows[0].warehouse.is_empty(),
            "no deposits renders as an empty list"
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

        let serviceable_count = rows.iter().filter(|r| r.serviceable).count() as i64;

        // Routed through the gates' own expression rather than a seventh
        // hand-written copy of it (issue #73). Note the scope of the
        // claim: `serviceable` counts TAPE rows, while the gate count also
        // includes warehouse deposits (ADR-0006), so these are equal for
        // an ALL-TAPE unit -- which this fixture is, deliberately. Add a
        // deposit here and the two must diverge; that is correct, not a
        // regression.
        let gate_count: i64 = conn
            .query_row(
                &format!(
                    "SELECT {}",
                    crate::policy::coverage::copy_count_expr(
                        &crate::policy::coverage::CoverageQuery {
                            scope: crate::policy::coverage::CoverageScope::Unit {
                                id_expr: "?1",
                                current_only: false,
                            },
                            exclude_volume: None,
                        }
                    )
                ),
                params![unit_id],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            conn.query_row("SELECT COUNT(*) FROM volume_deposits", [], |r| r
                .get::<_, i64>(0))
                .unwrap(),
            0,
            "this equality only holds for an all-tape unit"
        );

        assert_eq!(
            serviceable_count, gate_count,
            "locate's serviceable count must equal the gates' copy count"
        );
        assert_eq!(gate_count, 1, "only the sealed volume counts");
    }

    // --- issue #235: `catalog rebuild --json` carries the zero-copy
    // evidence, not just the displaced label ---

    use crate::volume::rebuild;

    fn report_with_displacement(units: Vec<(&str, &str, i64)>) -> rebuild::RebuildReport {
        rebuild::RebuildReport {
            displaced: vec![rebuild::DisplacedVolume {
                label: "L6-STALE".to_string(),
                units: units
                    .into_iter()
                    .map(|(name, status, other_copies)| rebuild::DisplacedUnit {
                        unit_name: name.to_string(),
                        unit_status: status.to_string(),
                        other_copies,
                    })
                    .collect(),
                warning: vec!["warning: ...".to_string()],
            }],
            ..Default::default()
        }
    }

    #[test]
    fn rebuild_json_names_the_unit_a_displacement_takes_to_zero_copies() {
        let report = report_with_displacement(vec![
            ("archive", "tape_only", 1),
            ("photos", "tape_only", 0),
        ]);
        let json = displacements_json(&report);
        assert_eq!(
            json,
            serde_json::json!([{
                "label": "L6-STALE",
                "units": [
                    {"unit": "archive", "status": "tape_only", "other_copies": 1},
                    {"unit": "photos", "status": "tape_only", "other_copies": 0},
                ],
                "zero_copy_units": ["photos"],
            }])
        );
    }

    /// The discrimination: a displaced volume whose units all keep coverage
    /// elsewhere reports an EMPTY `zero_copy_units`, not every unit on it.
    #[test]
    fn rebuild_json_leaves_zero_copy_units_empty_when_nothing_went_to_zero() {
        let report =
            report_with_displacement(vec![("archive", "tape_only", 1), ("photos", "active", 2)]);
        let json = displacements_json(&report);
        assert_eq!(json[0]["zero_copy_units"], serde_json::json!([]));
        assert_eq!(json[0]["units"].as_array().unwrap().len(), 2);
    }
}
