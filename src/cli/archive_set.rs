use clap::{Args, Subcommand};
use rusqlite::types::Value as SqlValue;
use rusqlite::{params, Connection, OptionalExtension};
use serde::Serialize;
use tabled::{Table, Tabled};

use crate::config::Config;
use crate::db::events;
use crate::error::{Result, TapectlError};

/// Syntactic check first, then capability check against the locally
/// installed `dar` binary (issue #97): a value from
/// `crate::config::VALID_COMPRESSION_VALUES` can still be one the local dar
/// was not compiled to support (`lzo`, `zstd`, `lz4`, `lzma` are commonly
/// absent from distro builds), which otherwise only surfaces as a runtime
/// `dar -z` failure at archive time.
///
/// The syntactic half moved to `crate::config::validate_compression` under
/// issue #171 so `[defaults].compression` and an archive_set's `compression`
/// share ONE check instead of `[defaults]` accepting any string while this
/// path validated it — see `crate::config::Config::validate_closed_sets`.
///
/// Fails open on capability-probe trouble: if `dar::version::capabilities`
/// itself errors (binary missing, unreadable, etc.), that is a pre-existing,
/// separately-reported condition (`config check`'s dar depth-check) — this
/// function does not pile a second, redundant error on top, and simply lets
/// the value through so the syntactic check remains authoritative in that
/// case.
fn validate_compression_capability(value: &str, config: &Config) -> Result<()> {
    crate::config::validate_compression(value).map_err(TapectlError::Other)?;

    if let Ok(caps) = crate::dar::version::capabilities(&config.dar.binary) {
        if !caps.supports(value) {
            let supported: Vec<&str> = crate::config::VALID_COMPRESSION_VALUES
                .iter()
                .filter(|alg| caps.supports(alg))
                .copied()
                .collect();
            return Err(TapectlError::Other(format!(
                "compression \"{value}\" is not supported by the local dar binary ({}): \
                 it supports {}",
                config.dar.binary,
                supported.join(", ")
            )));
        }
    }

    Ok(())
}

/// Parse a `--required-locations` value (comma-separated) and refuse any
/// name that is not a registered location (issue #348).
///
/// A policy naming a location nobody registered can never be met, and
/// nothing downstream says why — `audit` just reports the unit short of
/// locations forever. So `create` and `edit` refuse it here, naming each
/// unknown name, what IS registered, and the command that registers one.
/// An empty entry (`"a,,b"`, a trailing comma) is refused the same way: it
/// can only be a typo. Runs before either command's dry-run return, so a dry
/// run refuses exactly what the real run would (#241).
fn parse_required_locations(conn: &Connection, value: &str) -> Result<Vec<String>> {
    let names: Vec<String> = value.split(',').map(|s| s.trim().to_string()).collect();
    if names.iter().any(|n| n.is_empty()) {
        return Err(TapectlError::Other(format!(
            "--required-locations {value:?} contains an empty location name — \
             give a comma-separated list such as \"home-rack,offsite\""
        )));
    }
    let registered: Vec<String> = conn
        .prepare("SELECT name FROM locations ORDER BY name")?
        .query_map([], |row| row.get(0))?
        .collect::<std::result::Result<_, _>>()?;
    let unknown: Vec<String> = names
        .iter()
        .filter(|n| !registered.contains(n))
        .map(|n| format!("\"{n}\""))
        .collect();
    if !unknown.is_empty() {
        let known = if registered.is_empty() {
            "no locations are registered yet".to_string()
        } else {
            format!("registered locations: {}", registered.join(", "))
        };
        let what = if unknown.len() == 1 {
            "which is not a registered location"
        } else {
            "which are not registered locations"
        };
        return Err(TapectlError::Other(format!(
            "--required-locations names {}, {what} ({known}). Register a location \
             first with `tapectl location add <name>`, or fix the spelling.",
            unknown.join(", "),
        )));
    }
    Ok(names)
}

/// The policy columns `archive-set create` and `edit` take as flags — one
/// definition for both (issue #346), so a column one command can set is a
/// column the other can too. Every flag is optional: `create` leaves an
/// omitted one NULL ("defer to `[defaults]`"), `edit` leaves it unchanged.
#[derive(Args, Debug, Default, Clone)]
pub struct PolicyArgs {
    /// Minimum copy count
    #[arg(long)]
    pub min_copies: Option<i64>,
    /// Required locations (comma-separated); each must be a registered
    /// location (`tapectl location add`)
    #[arg(long)]
    pub required_locations: Option<String>,
    /// Encryption enabled
    #[arg(long)]
    pub encrypt: Option<bool>,
    /// Compression mode
    #[arg(long)]
    pub compression: Option<String>,
    /// Checksum mode
    #[arg(long)]
    pub checksum_mode: Option<String>,
    /// Slice size (e.g., "10G", the default)
    #[arg(long)]
    pub slice_size: Option<String>,
    /// Verify interval in days
    #[arg(long)]
    pub verify_interval_days: Option<i64>,
    /// Warehouse copies expected (ADR-0006). Never set means "defer to the
    /// system default".
    #[arg(long)]
    pub warehouse_copies: Option<i64>,
    /// Keep extended attributes, and the POSIX ACLs stored as them
    /// (true/false; false drops them all)
    #[arg(long)]
    pub preserve_xattrs: Option<bool>,
    /// Keep POSIX ACLs (true/false). No effect of its own: ACLs follow
    /// --preserve-xattrs
    #[arg(long)]
    pub preserve_acls: Option<bool>,
    /// Keep filesystem-specific attributes such as chattr flags (true/false)
    #[arg(long)]
    pub preserve_fsa: Option<bool>,
    /// Treat a metadata-only change as making a unit dirty (true/false).
    /// Not read yet: dirty detection compares path, size and mtime only
    #[arg(long)]
    pub dirty_on_metadata_change: Option<bool>,
    /// Description
    #[arg(long, short)]
    pub description: Option<String>,
}

#[derive(Subcommand, Debug)]
pub enum ArchiveSetCommands {
    /// Create a new archive set policy
    Create {
        /// Archive set name
        name: String,
        #[command(flatten)]
        policy: PolicyArgs,
    },

    /// Edit an existing archive set (flags left out are left unchanged)
    Edit {
        /// Archive set name
        name: String,
        #[command(flatten)]
        policy: PolicyArgs,
    },

    /// List archive sets
    List,

    /// Show archive set details
    Info {
        /// Archive set name
        name: String,
    },

    /// Sync archive sets from config.toml (writes only the keys each
    /// [[archive_sets]] table names)
    Sync,
}

/// A value bound for one `archive_sets` column, typed so the audit trail
/// renders an old value the same way as the new one — a boolean column
/// stores 0/1 but is logged `true`/`false`, as `edit` always logged
/// `encrypt`.
#[derive(Debug, Clone, PartialEq)]
enum ColumnValue {
    Int(i64),
    Bool(bool),
    Text(String),
}

impl ColumnValue {
    fn to_sql(&self) -> SqlValue {
        match self {
            ColumnValue::Int(n) => SqlValue::Integer(*n),
            ColumnValue::Bool(b) => SqlValue::Integer(i64::from(*b)),
            ColumnValue::Text(s) => SqlValue::Text(s.clone()),
        }
    }

    fn shown(&self) -> String {
        match self {
            ColumnValue::Int(n) => n.to_string(),
            ColumnValue::Bool(b) => b.to_string(),
            ColumnValue::Text(s) => s.clone(),
        }
    }

    /// A value already stored in this column, rendered as this column's
    /// kind; `None` for NULL.
    fn shown_stored(&self, stored: &SqlValue) -> Option<String> {
        match (self, stored) {
            (_, SqlValue::Null) => None,
            (ColumnValue::Bool(_), SqlValue::Integer(n)) => Some((*n != 0).to_string()),
            (_, SqlValue::Integer(n)) => Some(n.to_string()),
            (_, SqlValue::Real(f)) => Some(f.to_string()),
            (_, SqlValue::Text(s)) => Some(s.clone()),
            (_, SqlValue::Blob(b)) => Some(format!("<{} bytes>", b.len())),
        }
    }
}

/// Every `archive_sets` policy column a command can write, `None` where the
/// command leaves the column alone.
///
/// `create`, `edit` and `sync` all write through this (issue #346). Before
/// it, each had its own column list: `sync` knew seven columns, `create` and
/// `edit` nine, and four keys `[[archive_sets]]` accepts —
/// `preserve_xattrs`, `preserve_acls`, `preserve_fsa`,
/// `dirty_on_metadata_change` — reached no column through any of them.
/// Adding a column here is what makes it writable everywhere.
#[derive(Debug, Default)]
struct PolicyWrite {
    min_copies: Option<i64>,
    required_locations: Option<Vec<String>>,
    encrypt: Option<bool>,
    compression: Option<String>,
    checksum_mode: Option<String>,
    /// Bytes, already parsed.
    slice_size: Option<i64>,
    verify_interval_days: Option<i64>,
    warehouse_copies: Option<i64>,
    preserve_xattrs: Option<bool>,
    preserve_acls: Option<bool>,
    preserve_fsa: Option<bool>,
    dirty_on_metadata_change: Option<bool>,
    description: Option<String>,
}

impl PolicyWrite {
    /// Validate `create`/`edit` flags and turn them into a write. Every
    /// check runs here, before either command's dry-run return, so a dry
    /// run refuses exactly what the real run would (#241).
    fn from_args(conn: &Connection, config: &Config, args: &PolicyArgs) -> Result<Self> {
        if let Some(c) = &args.compression {
            validate_compression_capability(c, config)?;
        }
        if let Some(m) = &args.checksum_mode {
            // ADR-0012 "same treatment" as compression (issue #171): was
            // unvalidated here even though `units.checksum_mode`'s CHECK
            // constraint would reject a bad value anyway, just hours later
            // at unit-write time with a raw SQLite error.
            crate::config::validate_checksum_mode(m).map_err(TapectlError::Other)?;
        }
        let required_locations = args
            .required_locations
            .as_ref()
            .map(|locs| parse_required_locations(conn, locs))
            .transpose()?;
        let slice_size = args
            .slice_size
            .as_ref()
            .map(|s| crate::staging::parse_size_to_bytes(s))
            .transpose()?;
        Ok(Self {
            min_copies: args.min_copies,
            required_locations,
            encrypt: args.encrypt,
            compression: args.compression.clone(),
            checksum_mode: args.checksum_mode.clone(),
            slice_size,
            verify_interval_days: args.verify_interval_days,
            warehouse_copies: args.warehouse_copies,
            preserve_xattrs: args.preserve_xattrs,
            preserve_acls: args.preserve_acls,
            preserve_fsa: args.preserve_fsa,
            dirty_on_metadata_change: args.dirty_on_metadata_change,
            description: args.description.clone(),
        })
    }

    /// The write one `[[archive_sets]]` table asks for: exactly the keys it
    /// names (CTO ruling 2026-09-28 on issue #346 — config.toml wins, but
    /// only for the keys present; a value set by `edit` for a key the table
    /// omits is left alone). `sync` validates compression and checksum mode
    /// for every table before calling this; `slice_size` is parsed here.
    fn from_config(as_cfg: &crate::config::ArchiveSetConfig) -> Result<Self> {
        Ok(Self {
            min_copies: as_cfg.min_copies.map(i64::from),
            required_locations: as_cfg.required_locations.clone(),
            encrypt: as_cfg.encrypt,
            compression: as_cfg.compression.clone(),
            checksum_mode: as_cfg.checksum_mode.clone(),
            slice_size: as_cfg
                .slice_size
                .as_ref()
                .map(|s| crate::staging::parse_size_to_bytes(s))
                .transpose()?,
            verify_interval_days: as_cfg.verify_interval_days.map(i64::from),
            warehouse_copies: None,
            preserve_xattrs: as_cfg.preserve_xattrs,
            preserve_acls: as_cfg.preserve_acls,
            preserve_fsa: as_cfg.preserve_fsa,
            dirty_on_metadata_change: as_cfg.dirty_on_metadata_change,
            description: None,
        })
    }

    /// The columns this write names, in the order `edit` has always applied
    /// and logged them.
    fn columns(&self) -> Vec<(&'static str, ColumnValue)> {
        let mut out = Vec::new();
        let mut push = |column: &'static str, value: Option<ColumnValue>| {
            if let Some(v) = value {
                out.push((column, v));
            }
        };
        push("min_copies", self.min_copies.map(ColumnValue::Int));
        push(
            "required_locations",
            self.required_locations
                .as_ref()
                .map(|locs| ColumnValue::Text(serde_json::to_string(locs).unwrap())),
        );
        push("encrypt", self.encrypt.map(ColumnValue::Bool));
        push(
            "compression",
            self.compression.clone().map(ColumnValue::Text),
        );
        push(
            "checksum_mode",
            self.checksum_mode.clone().map(ColumnValue::Text),
        );
        push("slice_size", self.slice_size.map(ColumnValue::Int));
        push(
            "verify_interval_days",
            self.verify_interval_days.map(ColumnValue::Int),
        );
        push(
            "warehouse_copies",
            self.warehouse_copies.map(ColumnValue::Int),
        );
        push(
            "preserve_xattrs",
            self.preserve_xattrs.map(ColumnValue::Bool),
        );
        push("preserve_acls", self.preserve_acls.map(ColumnValue::Bool));
        push("preserve_fsa", self.preserve_fsa.map(ColumnValue::Bool));
        push(
            "dirty_on_metadata_change",
            self.dirty_on_metadata_change.map(ColumnValue::Bool),
        );
        push(
            "description",
            self.description.clone().map(ColumnValue::Text),
        );
        out
    }

    /// Insert a new set carrying exactly the columns this write names; the
    /// rest stay NULL. Column names come only from [`Self::columns`]'s
    /// fixed literals, never from input.
    fn insert(&self, conn: &Connection, name: &str) -> Result<i64> {
        let columns = self.columns();
        let mut names = vec!["name"];
        let mut values = vec![SqlValue::Text(name.to_string())];
        for (column, value) in &columns {
            names.push(column);
            values.push(value.to_sql());
        }
        let placeholders: Vec<String> = (1..=names.len()).map(|i| format!("?{i}")).collect();
        conn.execute(
            &format!(
                "INSERT INTO archive_sets ({}) VALUES ({})",
                names.join(", "),
                placeholders.join(", ")
            ),
            rusqlite::params_from_iter(values),
        )?;
        Ok(conn.last_insert_rowid())
    }

    /// Write each column this write names onto set `id`, logging one
    /// `action` event per column with its real old and new values (issue
    /// #48 item 5). With `only_changes`, a column that already holds the
    /// new value is skipped and not logged: `sync` re-applies the same
    /// config on every run, and an event per unchanged key per run would
    /// bury the real ones. Returns how many columns were written. The
    /// `format!`ed column names come only from [`Self::columns`]'s fixed
    /// literals, never from input.
    fn apply(
        &self,
        conn: &Connection,
        id: i64,
        name: &str,
        action: &str,
        only_changes: bool,
    ) -> Result<usize> {
        let mut written = 0;
        for (column, value) in self.columns() {
            let stored: SqlValue = conn.query_row(
                &format!("SELECT {column} FROM archive_sets WHERE id = ?1"),
                params![id],
                |row| row.get(0),
            )?;
            let new = value.to_sql();
            if only_changes && stored == new {
                continue;
            }
            conn.execute(
                &format!(
                    "UPDATE archive_sets SET {column} = ?1, updated_at = datetime('now') \
                     WHERE id = ?2"
                ),
                params![new, id],
            )?;
            events::log_field_change(
                conn,
                "archive_set",
                id,
                name,
                action,
                column,
                value.shown_stored(&stored).as_deref(),
                &value.shown(),
                None,
            )?;
            written += 1;
        }
        Ok(written)
    }
}

#[derive(Tabled, Serialize)]
struct ArchiveSetRow {
    #[tabled(rename = "Name")]
    name: String,
    /// Raw `Option<i64>`, not a pre-rendered string (issue #205's audit).
    /// `archive-set info --json` already emitted this same column raw, so
    /// the two subcommands disagreed in TYPE about the same fact — a
    /// consumer reading `min_copies` got `"3"` from one and `3` from the
    /// other. `display_opt_i64` keeps the table's `-` for NULL unchanged.
    #[tabled(rename = "Copies", display_with = "display_opt_i64")]
    min_copies: Option<i64>,
    /// Table-only until CTO decision 2026-09-11 (architecture review C2
    /// follow-up, C2b). `required_locations` is stored as a JSON array
    /// string (or NULL); `None` covers both NULL and an unparseable value
    /// (corruption `policy::resolve` already treats as fatal elsewhere) so
    /// this never silently invents an empty list for "not configured".
    #[tabled(rename = "Locations", display_with = "display_locations")]
    locations: Option<Vec<String>>,
    /// Table-only until CTO decision 2026-09-11 (architecture review C2
    /// follow-up, C2b).
    #[tabled(rename = "Verify Days", display_with = "display_opt_i64")]
    verify_days: Option<i64>,
    #[tabled(rename = "Units")]
    #[serde(rename = "units")]
    unit_count: i64,
}

/// Reproduces the pre-existing table text byte-for-byte: `None` (NULL
/// `required_locations`, or unparseable) renders "-"; `Some` re-serializes
/// to the same compact JSON array text the column stored (`create`/`edit`/
/// `sync` write via `serde_json::to_string`, so round-tripping through
/// `Vec<String>` reproduces it exactly).
fn display_locations(v: &Option<Vec<String>>) -> String {
    match v {
        None => "-".to_string(),
        Some(list) => serde_json::to_string(list).unwrap_or_else(|_| "-".to_string()),
    }
}

fn display_opt_i64(v: &Option<i64>) -> String {
    v.map(|n| n.to_string()).unwrap_or_else(|| "-".to_string())
}

/// `archive-set list --json` shape. `locations`/`verify_days` were table-only
/// until CTO decision 2026-09-11 (architecture review C2 follow-up, C2b).
fn archive_set_rows_to_json(rows: &[ArchiveSetRow]) -> serde_json::Value {
    serde_json::to_value(rows).unwrap()
}

/// Every stored field of one archive set, as `info` shows it. Issue #346:
/// `info` used to leave out `warehouse_copies` and the four
/// preserve/dirty columns, so a value set by `create`, `edit` or `sync` had
/// no command that would show it.
#[derive(Debug, Default)]
struct ArchiveSetInfo {
    name: String,
    description: Option<String>,
    min_copies: Option<i64>,
    /// The stored JSON array text, or NULL.
    required_locations: Option<String>,
    encrypt: Option<i64>,
    compression: Option<String>,
    checksum_mode: Option<String>,
    slice_size: Option<i64>,
    verify_days: Option<i64>,
    warehouse_copies: Option<i64>,
    preserve_xattrs: Option<i64>,
    preserve_acls: Option<i64>,
    preserve_fsa: Option<i64>,
    dirty_on_metadata_change: Option<i64>,
    unit_count: i64,
    created: String,
    updated: String,
}

impl ArchiveSetInfo {
    fn load(conn: &Connection, name: &str) -> Result<Self> {
        let mut info = conn
            .query_row(
                "SELECT a.description, a.min_copies, a.required_locations, a.encrypt,
                        a.compression, a.checksum_mode, a.slice_size, a.verify_interval_days,
                        a.warehouse_copies, a.preserve_xattrs, a.preserve_acls, a.preserve_fsa,
                        a.dirty_on_metadata_change, a.created_at, a.updated_at,
                        (SELECT COUNT(*) FROM units u WHERE u.archive_set_id = a.id)
                 FROM archive_sets a WHERE a.name = ?1",
                params![name],
                |row| {
                    Ok(ArchiveSetInfo {
                        name: String::new(),
                        description: row.get(0)?,
                        min_copies: row.get(1)?,
                        required_locations: row.get(2)?,
                        encrypt: row.get(3)?,
                        compression: row.get(4)?,
                        checksum_mode: row.get(5)?,
                        slice_size: row.get(6)?,
                        verify_days: row.get(7)?,
                        warehouse_copies: row.get(8)?,
                        preserve_xattrs: row.get(9)?,
                        preserve_acls: row.get(10)?,
                        preserve_fsa: row.get(11)?,
                        dirty_on_metadata_change: row.get(12)?,
                        created: row.get(13)?,
                        updated: row.get(14)?,
                        unit_count: row.get(15)?,
                    })
                },
            )
            .optional()?
            .ok_or_else(|| TapectlError::Other(format!("archive set \"{name}\" not found")))?;
        info.name = name.to_string();
        Ok(info)
    }
}

/// `archive-set info --json` shape, aligned with `list`'s (issue #236
/// finding 6): `info` used to emit the raw `required_locations` COLUMN --
/// the JSON-encoded array stored as a plain string -- so `jq
/// '.required_locations[0]'` worked on `list`'s parsed `locations` array
/// and failed on `info`'s string. It also spelled the same fact
/// `verify_interval_days` where `list` says `verify_days`, and emitted
/// `encrypt` as the raw `0`/`1`/`NULL` column instead of a bool. This is
/// the diff's own precedent (`ArchiveSetRow.min_copies`, `ArchiveSetRow`'s
/// doc comment above): when two subcommands disagree about the same fact,
/// the fix is correctness, not preserving either side's exact prior shape.
///
/// Issue #346 adds `warehouse_copies` and the four preserve/dirty columns,
/// each boolean a real bool or `null` (never set — defers to `[defaults]`),
/// exactly like `encrypt`.
fn info_json(info: &ArchiveSetInfo) -> serde_json::Value {
    let locations = info
        .required_locations
        .as_ref()
        .and_then(|s| serde_json::from_str::<Vec<String>>(s).ok());
    let flag = |v: Option<i64>| v.map(|n| n != 0);
    serde_json::json!({
        "name": info.name, "description": info.description, "min_copies": info.min_copies,
        "locations": locations, "encrypt": flag(info.encrypt),
        "compression": info.compression, "checksum_mode": info.checksum_mode,
        "slice_size": info.slice_size, "verify_days": info.verify_days,
        "warehouse_copies": info.warehouse_copies,
        "preserve_xattrs": flag(info.preserve_xattrs),
        "preserve_acls": flag(info.preserve_acls),
        "preserve_fsa": flag(info.preserve_fsa),
        "dirty_on_metadata_change": flag(info.dirty_on_metadata_change),
        "units": info.unit_count,
    })
}

/// `archive-set info`'s text form. `-` is a column never set, which defers
/// to `[defaults]`.
fn info_text(info: &ArchiveSetInfo) -> String {
    let int = |v: Option<i64>| v.map(|n| n.to_string()).unwrap_or_else(|| "-".into());
    let flag = |v: Option<i64>| match v {
        None => "-",
        Some(0) => "no",
        Some(_) => "yes",
    };
    let text = |v: &Option<String>| v.clone().unwrap_or_else(|| "-".into());
    let mut out = format!("Archive set: {}\n", info.name);
    if let Some(d) = &info.description {
        out.push_str(&format!("  Description:      {d}\n"));
    }
    out.push_str(&format!("  Min copies:       {}\n", int(info.min_copies)));
    out.push_str(&format!(
        "  Req. locations:   {}\n",
        text(&info.required_locations)
    ));
    out.push_str(&format!("  Encrypt:          {}\n", flag(info.encrypt)));
    out.push_str(&format!(
        "  Compression:      {}\n",
        text(&info.compression)
    ));
    out.push_str(&format!(
        "  Checksum mode:    {}\n",
        text(&info.checksum_mode)
    ));
    // Binary: `slice_size` is parsed by `staging::parse_size_to_bytes`
    // ("G" => 1024^3), so the figure was always binary and only the label
    // was wrong (issue #204, class 1 — relabel, never re-divide).
    out.push_str(&format!(
        "  Slice size:       {}\n",
        info.slice_size
            .map(crate::util::format_bytes_binary)
            .unwrap_or_else(|| "-".into())
    ));
    out.push_str(&format!(
        "  Verify interval:  {}\n",
        info.verify_days
            .map(|n| format!("{n} days"))
            .unwrap_or_else(|| "-".into())
    ));
    out.push_str(&format!(
        "  Warehouse copies: {}\n",
        int(info.warehouse_copies)
    ));
    out.push_str(&format!(
        "  Preserve xattrs:  {}\n",
        flag(info.preserve_xattrs)
    ));
    out.push_str(&format!(
        "  Preserve ACLs:    {}\n",
        flag(info.preserve_acls)
    ));
    out.push_str(&format!(
        "  Preserve FSA:     {}\n",
        flag(info.preserve_fsa)
    ));
    out.push_str(&format!(
        "  Dirty on metadata change: {}\n",
        flag(info.dirty_on_metadata_change)
    ));
    out.push_str(&format!("  Units using:      {}\n", info.unit_count));
    out.push_str(&format!("  Created:          {}\n", info.created));
    out.push_str(&format!("  Updated:          {}\n", info.updated));
    out
}

pub fn run(
    conn: &Connection,
    config: &Config,
    command: &ArchiveSetCommands,
    json_output: bool,
    dry_run: bool,
) -> Result<()> {
    match command {
        ArchiveSetCommands::Create { name, policy } => {
            let write = PolicyWrite::from_args(conn, config, policy)?;

            // Issue #241: pure precheck-then-INSERT with no policy gate —
            // every validation already ran in `from_args`, so this only adds
            // the name-collision check the UNIQUE constraint would otherwise
            // catch (and stays ahead of the dry-run return, as always).
            if dry_run {
                let taken: Option<i64> = conn
                    .query_row(
                        "SELECT id FROM archive_sets WHERE name = ?1",
                        params![name],
                        |row| row.get(0),
                    )
                    .optional()?;
                if taken.is_some() {
                    return Err(TapectlError::Other(format!(
                        "archive set \"{name}\" already exists"
                    )));
                }
                if json_output {
                    println!("{}", serde_json::json!({"name": name, "dry_run": true}));
                } else {
                    println!("would create archive set \"{name}\" (DRY RUN — no changes made)");
                }
                return Ok(());
            }

            let id = write.insert(conn, name)?;
            events::log_created(conn, "archive_set", id, name, None)?;

            if json_output {
                println!("{}", serde_json::json!({"id": id, "name": name}));
            } else {
                println!("archive set \"{name}\" created (id={id})");
            }
        }

        ArchiveSetCommands::Edit { name, policy } => {
            let write = PolicyWrite::from_args(conn, config, policy)?;
            let id: i64 = conn
                .query_row(
                    "SELECT id FROM archive_sets WHERE name = ?1",
                    params![name],
                    |row| row.get(0),
                )
                .map_err(|_| TapectlError::Other(format!("archive set \"{name}\" not found")))?;

            // Issue #241: every validation and the existence lookup just
            // above already refuse what the real edit would refuse; a dry run
            // stops here rather than running any of the per-field UPDATEs.
            if dry_run {
                if json_output {
                    println!("{}", serde_json::json!({"name": name, "dry_run": true}));
                } else {
                    println!("would edit archive set \"{name}\" (DRY RUN — no changes made)");
                }
                return Ok(());
            }

            // Every field UPDATE plus its audit event runs in one
            // transaction: a failure partway through must not leave some
            // fields changed and others not (issue #48 item 5). Each event
            // records the real old value, read just before its own UPDATE.
            let tx = conn.unchecked_transaction()?;
            write.apply(&tx, id, name, "edited", false)?;
            tx.commit()?;

            if json_output {
                println!("{}", serde_json::json!({"name": name, "updated": true}));
            } else {
                println!("archive set \"{name}\" updated");
            }
        }

        ArchiveSetCommands::List => {
            let mut stmt = conn.prepare(
                "SELECT a.name, a.min_copies, a.required_locations, a.verify_interval_days,
                        (SELECT COUNT(*) FROM units u WHERE u.archive_set_id = a.id) as unit_count
                 FROM archive_sets a ORDER BY a.name",
            )?;
            let rows: Vec<ArchiveSetRow> = stmt
                .query_map([], |row| {
                    Ok(ArchiveSetRow {
                        name: row.get(0)?,
                        min_copies: row.get::<_, Option<i64>>(1)?,
                        locations: row
                            .get::<_, Option<String>>(2)?
                            .and_then(|s| serde_json::from_str::<Vec<String>>(&s).ok()),
                        verify_days: row.get::<_, Option<i64>>(3)?,
                        unit_count: row.get(4)?,
                    })
                })?
                .collect::<std::result::Result<Vec<_>, _>>()?;

            if json_output {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&archive_set_rows_to_json(&rows)).unwrap()
                );
            } else if rows.is_empty() {
                println!("no archive sets defined");
            } else {
                println!("{}", Table::new(rows));
            }
        }

        ArchiveSetCommands::Info { name } => {
            let info = ArchiveSetInfo::load(conn, name)?;
            if json_output {
                println!("{}", info_json(&info));
            } else {
                print!("{}", info_text(&info));
            }
        }

        ArchiveSetCommands::Sync => {
            // Issue #241: a config-to-DB reconciliation (like `unit
            // discover`/`collection sync`) — created/updated/unchanged is
            // decided per entry as it walks `config.archive_sets`, so a
            // faithful preview would duplicate that logic.
            if dry_run {
                return Err(crate::cli::refuse_dry_run(
                    "archive-set sync",
                    "created/updated/unchanged is decided per entry while walking \
                     config.toml; a faithful preview would duplicate that reconciliation.",
                ));
            }

            // Same guard as create/edit: `sync` writes archive_sets rows
            // straight from config.toml, so without this a bogus `compression`
            // in the config file walks past the CLI validation and only fails
            // later at `dar -z` (issue #92). Validated for EVERY entry up
            // front, before any row is written, and the writes below run in
            // one transaction — `sync` is all-or-nothing.
            let mut writes = Vec::with_capacity(config.archive_sets.len());
            for as_cfg in &config.archive_sets {
                let named = |e: TapectlError| {
                    TapectlError::Other(format!("archive set \"{}\": {e}", as_cfg.name))
                };
                if let Some(c) = &as_cfg.compression {
                    validate_compression_capability(c, config).map_err(named)?;
                }
                // ADR-0012 "same treatment" as compression (issue #171).
                if let Some(m) = &as_cfg.checksum_mode {
                    crate::config::validate_checksum_mode(m)
                        .map_err(|e| named(TapectlError::Other(e)))?;
                }
                // Issue #59: a malformed slice_size must not silently become
                // 0 or the wrong magnitude in the DB (parsed in `from_config`).
                writes.push((as_cfg, PolicyWrite::from_config(as_cfg).map_err(named)?));
            }

            // CTO ruling 2026-09-28 (issue #346): config.toml wins, but only
            // for the keys each table names. A key the table omits leaves
            // the column alone — `sync` used to write all seven of its
            // columns for every set, so a value set with `edit` (or at
            // `create`) was wiped to NULL by the next sync whenever the TOML
            // did not repeat it. Each changed column is logged as a
            // `synced` field event; a set with nothing to change is
            // `unchanged` and logs nothing.
            let (mut created, mut updated, mut unchanged) = (0, 0, 0);
            let tx = conn.unchecked_transaction()?;
            for (as_cfg, write) in &writes {
                let existing: Option<i64> = tx
                    .query_row(
                        "SELECT id FROM archive_sets WHERE name = ?1",
                        params![as_cfg.name],
                        |row| row.get(0),
                    )
                    .optional()?;
                match existing {
                    Some(id) => {
                        if write.apply(&tx, id, &as_cfg.name, "synced", true)? > 0 {
                            updated += 1;
                        } else {
                            unchanged += 1;
                        }
                    }
                    None => {
                        let id = write.insert(&tx, &as_cfg.name)?;
                        events::log_created(&tx, "archive_set", id, &as_cfg.name, None)?;
                        created += 1;
                    }
                }
            }
            tx.commit()?;

            if json_output {
                println!(
                    "{}",
                    serde_json::json!({
                        "created": created, "updated": updated, "unchanged": unchanged,
                    })
                );
            } else {
                println!(
                    "sync: {created} created, {updated} updated, {unchanged} unchanged \
                     from config.toml"
                );
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `archive-set list --json` shape (issue: C2 row-listing drift).
    /// `locations`/`verify_days` are additive since CTO decision 2026-09-11
    /// (architecture review C2 follow-up, C2b) -- row0 has real values,
    /// row1 has neither configured (`None`).
    #[test]
    fn pin_archive_set_rows_json_shape() {
        let rows = vec![
            ArchiveSetRow {
                name: "daily".to_string(),
                min_copies: Some(3),
                locations: Some(vec!["home".to_string(), "offsite".to_string()]),
                verify_days: Some(90),
                unit_count: 5,
            },
            ArchiveSetRow {
                name: "ephemeral".to_string(),
                min_copies: None,
                locations: None,
                verify_days: None,
                unit_count: 0,
            },
        ];
        let value = archive_set_rows_to_json(&rows);
        assert_eq!(
            serde_json::to_string(&value).unwrap(),
            r#"[{"locations":["home","offsite"],"min_copies":3,"name":"daily","units":5,"verify_days":90},{"locations":null,"min_copies":null,"name":"ephemeral","units":0,"verify_days":null}]"#
        );
    }

    /// Issue #236 finding 6: `info --json` must agree with `list --json`
    /// about the SAME facts -- a JSON-encoded array as a `Vec<String>`
    /// under the same key (`locations`, not `required_locations`), the
    /// same `verify_days` key, and a real bool for `encrypt` rather than
    /// the raw `0`/`1`/`null` column.
    #[test]
    fn info_json_agrees_with_list_json_about_shared_facts() {
        let value = info_json(&ArchiveSetInfo {
            name: "daily".to_string(),
            min_copies: Some(3),
            required_locations: Some(r#"["home","offsite"]"#.to_string()),
            encrypt: Some(1),
            compression: Some("zstd".to_string()),
            checksum_mode: Some("sha256".to_string()),
            slice_size: Some(1_048_576),
            verify_days: Some(90),
            unit_count: 5,
            ..ArchiveSetInfo::default()
        });
        assert_eq!(value["locations"], serde_json::json!(["home", "offsite"]));
        assert_eq!(value["verify_days"], serde_json::json!(90));
        assert_eq!(value["encrypt"], serde_json::json!(true));
        assert!(
            value.get("required_locations").is_none(),
            "the old string-shaped key must not linger alongside the fixed one: {value}"
        );
        assert!(
            value.get("verify_interval_days").is_none(),
            "the old key name must not linger alongside `verify_days`: {value}"
        );

        // The list-side fixture above (`pin_archive_set_rows_json_shape`)
        // proves `list` renders the identical `locations`/`verify_days`
        // values for the same underlying facts -- same keys, same types.
        let list_value = archive_set_rows_to_json(&[ArchiveSetRow {
            name: "daily".to_string(),
            min_copies: Some(3),
            locations: Some(vec!["home".to_string(), "offsite".to_string()]),
            verify_days: Some(90),
            unit_count: 5,
        }]);
        assert_eq!(value["locations"], list_value[0]["locations"]);
        assert_eq!(value["verify_days"], list_value[0]["verify_days"]);
    }

    /// A `None`/`NULL` archive set must not manufacture a `false` for
    /// `encrypt` -- "not configured" and "configured off" are different
    /// facts, exactly as `min_copies`/`slice_size`/etc. already distinguish
    /// `None` from a real `0`.
    #[test]
    fn info_json_leaves_encrypt_null_when_the_column_is_null() {
        let value = info_json(&ArchiveSetInfo {
            name: "cold".to_string(),
            ..ArchiveSetInfo::default()
        });
        for key in [
            "encrypt",
            "preserve_xattrs",
            "preserve_acls",
            "preserve_fsa",
            "dirty_on_metadata_change",
            "warehouse_copies",
        ] {
            assert_eq!(value[key], serde_json::Value::Null, "{key}: {value}");
        }
    }

    fn fresh_conn() -> Connection {
        crate::db::open_memory().unwrap()
    }

    fn create_cmd(
        name: &str,
        min_copies: Option<i64>,
        compression: Option<&str>,
    ) -> ArchiveSetCommands {
        ArchiveSetCommands::Create {
            name: name.to_string(),
            policy: PolicyArgs {
                min_copies,
                compression: compression.map(|s| s.to_string()),
                ..PolicyArgs::default()
            },
        }
    }

    /// Issue #92: since a dotfile no longer unconditionally shadows the
    /// archive_set's `compression`, a bogus value now actually reaches
    /// `dar -z <value>` at write time — reject it here instead of letting
    /// it surface as a runtime dar failure.
    #[test]
    fn create_rejects_invalid_compression() {
        let conn = fresh_conn();
        let config = Config::default();
        let err = run(
            &conn,
            &config,
            &create_cmd("cold", None, Some("not-a-real-codec")),
            false,
            false,
        )
        .unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("not-a-real-codec"),
            "error must name the invalid value, got: {msg}"
        );
        assert!(
            conn.query_row(
                "SELECT COUNT(*) FROM archive_sets WHERE name = 'cold'",
                [],
                |row| row.get::<_, i64>(0)
            )
            .unwrap()
                == 0,
            "no archive_set row should be created when compression is invalid"
        );
    }

    /// ADR-0012 gives checksum mode the same treatment as compression
    /// (issue #171): a bogus value must be rejected here, not surface as an
    /// opaque SQLite CHECK-constraint failure when a unit is finally
    /// written with it.
    #[test]
    fn create_rejects_invalid_checksum_mode() {
        let conn = fresh_conn();
        let config = Config::default();
        let mut cmd = create_cmd("cold", None, None);
        if let ArchiveSetCommands::Create { policy, .. } = &mut cmd {
            policy.checksum_mode = Some("not-a-real-mode".to_string());
        } else {
            unreachable!("create_cmd always returns Create");
        }
        let err = run(&conn, &config, &cmd, false, false).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("not-a-real-mode"),
            "error must name the invalid value, got: {msg}"
        );
        assert!(
            conn.query_row(
                "SELECT COUNT(*) FROM archive_sets WHERE name = 'cold'",
                [],
                |row| row.get::<_, i64>(0)
            )
            .unwrap()
                == 0,
            "no archive_set row should be created when checksum_mode is invalid"
        );
    }

    /// Writes a fake `dar` executable to a tempdir that answers `-V` with a
    /// synthetic capability block missing `lzo`, and answers `--version`
    /// with a real-looking version line so `validate_compression` (via
    /// `dar::version::check`/`capabilities`, both invoked through the same
    /// `Command::new(dar_binary)` shape) works against it. Real dar on this
    /// dev machine reports YES for every codec (per issue #97's context),
    /// so capability *rejection* can only be exercised against a synthetic
    /// binary, never the real one.
    fn fake_dar_missing_lzo() -> (tempfile::TempDir, String) {
        let tmp = tempfile::TempDir::new().unwrap();
        let path = tmp.path().join("dar");
        std::fs::write(
            &path,
            r#"#!/bin/sh
if [ "$1" = "-V" ]; then
  echo " Using libdar 6.7.1 built with compilation time options:"
  echo "   gzip compression (libz)      : YES"
  echo "   bzip2 compression (libbzip2) : YES"
  echo "   lzo compression (liblzo2)    : NO"
  echo "   xz compression (liblzma)     : YES"
  echo "   zstd compression (libzstd)   : YES"
  echo "   lz4 compression (liblz4)     : YES"
else
  echo "dar version 2.7.13, Copyright (C) 2002-2052 Denis Corbin"
fi
"#,
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        (tmp, path.to_str().unwrap().to_string())
    }

    /// Issue #97: an algorithm from `VALID_COMPRESSION_VALUES` that the
    /// local (synthetic) dar was not compiled to support must be rejected,
    /// and the error must name what the binary DOES support.
    #[test]
    fn create_rejects_syntactically_valid_but_locally_unsupported_compression() {
        let (_tmp, dar_path) = fake_dar_missing_lzo();
        let conn = fresh_conn();
        let mut config = Config::default();
        config.dar.binary = dar_path;

        let err = run(
            &conn,
            &config,
            &create_cmd("cold", None, Some("lzo")),
            false,
            false,
        )
        .unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("lzo"), "must name the rejected value: {msg}");
        assert!(
            msg.contains("gzip"),
            "must name a supported value in the message: {msg}"
        );
        assert_eq!(
            conn.query_row(
                "SELECT COUNT(*) FROM archive_sets WHERE name = 'cold'",
                [],
                |row| row.get::<_, i64>(0)
            )
            .unwrap(),
            0,
            "no archive_set row should be created when compression is unsupported"
        );
    }

    /// Issue #97: an outright invalid name must still hit the syntactic
    /// error path, not the capability-check path — even against a dar
    /// binary that would otherwise reject it for a different reason.
    #[test]
    fn create_rejects_invalid_name_with_syntactic_error_even_with_capability_probe_available() {
        let (_tmp, dar_path) = fake_dar_missing_lzo();
        let conn = fresh_conn();
        let mut config = Config::default();
        config.dar.binary = dar_path;

        let err = run(
            &conn,
            &config,
            &create_cmd("cold", None, Some("bogus")),
            false,
            false,
        )
        .unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("accepted values are"), "got: {msg}");
    }

    /// Issue #97: a syntactically valid, locally-supported algorithm must
    /// still succeed against the synthetic capability-aware dar.
    #[test]
    fn create_accepts_locally_supported_compression() {
        let (_tmp, dar_path) = fake_dar_missing_lzo();
        let conn = fresh_conn();
        let mut config = Config::default();
        config.dar.binary = dar_path;

        run(
            &conn,
            &config,
            &create_cmd("cold", None, Some("gzip")),
            false,
            false,
        )
        .unwrap();
        assert_eq!(
            conn.query_row(
                "SELECT COUNT(*) FROM archive_sets WHERE name = 'cold'",
                [],
                |row| row.get::<_, i64>(0)
            )
            .unwrap(),
            1
        );
    }

    fn add_location(conn: &Connection, name: &str) {
        conn.execute("INSERT INTO locations (name) VALUES (?1)", params![name])
            .unwrap();
    }

    fn create_with_locations(name: &str, locations: &str) -> ArchiveSetCommands {
        let mut cmd = create_cmd(name, None, None);
        if let ArchiveSetCommands::Create { policy, .. } = &mut cmd {
            policy.required_locations = Some(locations.to_string());
        } else {
            unreachable!("create_cmd always returns Create");
        }
        cmd
    }

    fn stored_locations(conn: &Connection, name: &str) -> Option<String> {
        conn.query_row(
            "SELECT required_locations FROM archive_sets WHERE name = ?1",
            params![name],
            |row| row.get(0),
        )
        .unwrap()
    }

    /// Issue #348: `--required-locations` naming a location nobody
    /// registered was accepted, so the policy could never be met and nothing
    /// said why. `create` refuses it by name — before the dry-run return
    /// too, since a dry run must refuse what the real run refuses (#241).
    #[test]
    fn create_refuses_a_required_location_that_is_not_registered() {
        let conn = fresh_conn();
        let config = Config::default();
        add_location(&conn, "home-rack");

        for dry_run in [true, false] {
            let err = run(
                &conn,
                &config,
                &create_with_locations("cold", "home-rack, offsite"),
                false,
                dry_run,
            )
            .unwrap_err()
            .to_string();
            assert!(
                err.contains("\"offsite\""),
                "must name the unknown one: {err}"
            );
            assert!(
                err.contains("home-rack"),
                "must name what IS registered: {err}"
            );
            assert!(
                err.contains("location add"),
                "must say how to fix it: {err}"
            );
        }
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM archive_sets", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count, 0, "a refused create must write nothing");

        // Positive control: the registered name alone is accepted and stored.
        run(
            &conn,
            &config,
            &create_with_locations("cold", "home-rack"),
            false,
            false,
        )
        .unwrap();
        assert_eq!(
            stored_locations(&conn, "cold").as_deref(),
            Some(r#"["home-rack"]"#)
        );
    }

    /// Issue #348, the `edit` half: an unknown name is refused and the
    /// stored list is left exactly as it was.
    #[test]
    fn edit_refuses_a_required_location_that_is_not_registered() {
        let conn = fresh_conn();
        let config = Config::default();
        add_location(&conn, "home-rack");
        add_location(&conn, "bank");
        run(
            &conn,
            &config,
            &create_with_locations("cold", "home-rack"),
            false,
            false,
        )
        .unwrap();

        let edit = |locations: &str| ArchiveSetCommands::Edit {
            name: "cold".to_string(),
            policy: PolicyArgs {
                required_locations: Some(locations.to_string()),
                ..PolicyArgs::default()
            },
        };
        let err = run(&conn, &config, &edit("home-rack,ofsite"), false, false)
            .unwrap_err()
            .to_string();
        assert!(err.contains("\"ofsite\""), "{err}");
        assert_eq!(
            stored_locations(&conn, "cold").as_deref(),
            Some(r#"["home-rack"]"#)
        );

        run(&conn, &config, &edit("home-rack,bank"), false, false).unwrap();
        assert_eq!(
            stored_locations(&conn, "cold").as_deref(),
            Some(r#"["home-rack","bank"]"#)
        );
    }

    fn set_config(name: &str) -> crate::config::ArchiveSetConfig {
        crate::config::ArchiveSetConfig {
            name: name.to_string(),
            min_copies: None,
            required_locations: None,
            encrypt: None,
            compression: None,
            checksum_mode: None,
            verify_interval_days: None,
            slice_size: None,
            preserve_xattrs: None,
            preserve_acls: None,
            preserve_fsa: None,
            dirty_on_metadata_change: None,
        }
    }

    fn column_i64(conn: &Connection, set: &str, column: &str) -> Option<i64> {
        conn.query_row(
            &format!("SELECT {column} FROM archive_sets WHERE name = ?1"),
            params![set],
            |row| row.get(0),
        )
        .unwrap()
    }

    /// Issue #346, the headline defect (CTO ruling 2026-09-28: config.toml
    /// wins, but only for the keys it names). `sync` wrote all seven synced
    /// columns for every set, so a value set with `edit` or `create` and
    /// omitted from the TOML was wiped to NULL by the next sync.
    #[test]
    fn sync_leaves_a_value_the_toml_omits_untouched() {
        let conn = fresh_conn();
        add_location(&conn, "home-rack");
        let mut config = Config::default();
        let mut cold = set_config("cold");
        cold.min_copies = Some(3);
        config.archive_sets.push(cold);

        run(
            &conn,
            &config,
            &create_with_locations("cold", "home-rack"),
            false,
            false,
        )
        .unwrap();
        run(&conn, &config, &ArchiveSetCommands::Sync, false, false).unwrap();
        run(
            &conn,
            &config,
            &ArchiveSetCommands::Edit {
                name: "cold".to_string(),
                policy: PolicyArgs {
                    verify_interval_days: Some(180),
                    ..PolicyArgs::default()
                },
            },
            false,
            false,
        )
        .unwrap();

        run(&conn, &config, &ArchiveSetCommands::Sync, false, false).unwrap();
        assert_eq!(column_i64(&conn, "cold", "verify_interval_days"), Some(180));
        assert_eq!(
            stored_locations(&conn, "cold").as_deref(),
            Some(r#"["home-rack"]"#),
            "a create-time value the TOML omits must survive sync"
        );
        assert_eq!(column_i64(&conn, "cold", "min_copies"), Some(3));

        // Positive control: a key the TOML DOES name overwrites the DB.
        config.archive_sets[0].verify_interval_days = Some(30);
        config.archive_sets[0].min_copies = Some(4);
        run(&conn, &config, &ArchiveSetCommands::Sync, false, false).unwrap();
        assert_eq!(column_i64(&conn, "cold", "verify_interval_days"), Some(30));
        assert_eq!(column_i64(&conn, "cold", "min_copies"), Some(4));
    }

    /// Issue #346: `preserve_xattrs`, `preserve_acls`, `preserve_fsa` and
    /// `dirty_on_metadata_change` were accepted in `[[archive_sets]]` and
    /// never written, so the columns stayed NULL and the set silently
    /// deferred to `[defaults]`. Every accepted key must reach its column,
    /// on the INSERT path and the UPDATE path alike.
    #[test]
    fn sync_persists_every_accepted_archive_sets_key() {
        let conn = fresh_conn();
        let mut config = Config::default();
        let mut media = set_config("media");
        media.preserve_xattrs = Some(false);
        media.preserve_acls = Some(false);
        media.preserve_fsa = Some(false);
        media.dirty_on_metadata_change = Some(true);
        config.archive_sets.push(media);

        run(&conn, &config, &ArchiveSetCommands::Sync, false, false).unwrap();
        for (column, want) in [
            ("preserve_xattrs", 0),
            ("preserve_acls", 0),
            ("preserve_fsa", 0),
            ("dirty_on_metadata_change", 1),
        ] {
            assert_eq!(
                column_i64(&conn, "media", column),
                Some(want),
                "insert: {column}"
            );
        }

        config.archive_sets[0].preserve_fsa = Some(true);
        config.archive_sets[0].dirty_on_metadata_change = Some(false);
        run(&conn, &config, &ArchiveSetCommands::Sync, false, false).unwrap();
        assert_eq!(
            column_i64(&conn, "media", "preserve_fsa"),
            Some(1),
            "update"
        );
        assert_eq!(
            column_i64(&conn, "media", "dirty_on_metadata_change"),
            Some(0),
            "update"
        );
        assert_eq!(column_i64(&conn, "media", "preserve_xattrs"), Some(0));
    }

    /// Issue #346: `create` and `edit` take a flag for every column `sync`
    /// can write — the four preserve/dirty keys had none — and `edit` logs
    /// each with its real old value, booleans as `true`/`false` like
    /// `encrypt`.
    #[test]
    fn create_and_edit_set_the_preserve_and_dirty_columns() {
        let conn = fresh_conn();
        let config = Config::default();
        let mut cmd = create_cmd("media", None, None);
        if let ArchiveSetCommands::Create { policy, .. } = &mut cmd {
            policy.preserve_xattrs = Some(false);
            policy.preserve_acls = Some(false);
            policy.preserve_fsa = Some(true);
            policy.dirty_on_metadata_change = Some(true);
        }
        run(&conn, &config, &cmd, false, false).unwrap();
        for (column, want) in [
            ("preserve_xattrs", 0),
            ("preserve_acls", 0),
            ("preserve_fsa", 1),
            ("dirty_on_metadata_change", 1),
        ] {
            assert_eq!(column_i64(&conn, "media", column), Some(want), "{column}");
        }

        run(
            &conn,
            &config,
            &ArchiveSetCommands::Edit {
                name: "media".to_string(),
                policy: PolicyArgs {
                    preserve_xattrs: Some(true),
                    dirty_on_metadata_change: Some(false),
                    ..PolicyArgs::default()
                },
            },
            false,
            false,
        )
        .unwrap();
        assert_eq!(column_i64(&conn, "media", "preserve_xattrs"), Some(1));
        assert_eq!(
            column_i64(&conn, "media", "dirty_on_metadata_change"),
            Some(0)
        );
        assert_eq!(
            column_i64(&conn, "media", "preserve_acls"),
            Some(0),
            "a flag edit leaves out must be left alone"
        );

        let events: Vec<(String, Option<String>, String)> = conn
            .prepare(
                "SELECT field, old_value, new_value FROM events
                 WHERE entity_type = 'archive_set' AND action = 'edited' ORDER BY field",
            )
            .unwrap()
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))
            .unwrap()
            .collect::<std::result::Result<_, _>>()
            .unwrap();
        assert_eq!(
            events,
            vec![
                (
                    "dirty_on_metadata_change".to_string(),
                    Some("true".to_string()),
                    "false".to_string()
                ),
                (
                    "preserve_xattrs".to_string(),
                    Some("false".to_string()),
                    "true".to_string()
                ),
            ]
        );
    }

    /// `sync` re-applies the same config every run; a set whose columns
    /// already match is `unchanged` and logs nothing, and a changed column
    /// is logged once as a `synced` field event with its old value.
    #[test]
    fn sync_logs_only_the_columns_it_changes() {
        let conn = fresh_conn();
        let mut config = Config::default();
        let mut cold = set_config("cold");
        cold.min_copies = Some(3);
        cold.preserve_fsa = Some(false);
        config.archive_sets.push(cold);

        run(&conn, &config, &ArchiveSetCommands::Sync, false, false).unwrap();
        run(&conn, &config, &ArchiveSetCommands::Sync, false, false).unwrap();
        let synced = |conn: &Connection| -> Vec<(String, Option<String>, String)> {
            conn.prepare(
                "SELECT field, old_value, new_value FROM events
                 WHERE entity_type = 'archive_set' AND action = 'synced' ORDER BY id",
            )
            .unwrap()
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))
            .unwrap()
            .collect::<std::result::Result<_, _>>()
            .unwrap()
        };
        assert!(synced(&conn).is_empty(), "{:?}", synced(&conn));

        config.archive_sets[0].min_copies = Some(4);
        run(&conn, &config, &ArchiveSetCommands::Sync, false, false).unwrap();
        assert_eq!(
            synced(&conn),
            vec![(
                "min_copies".to_string(),
                Some("3".to_string()),
                "4".to_string()
            )]
        );
    }

    /// Issue #346: `info` showed neither `warehouse_copies` nor the four
    /// preserve/dirty columns, so a value set through any writer had no
    /// command that would display it.
    #[test]
    fn info_text_shows_every_stored_field() {
        let text = info_text(&ArchiveSetInfo {
            name: "media".to_string(),
            min_copies: Some(3),
            warehouse_copies: Some(1),
            preserve_xattrs: Some(0),
            preserve_acls: Some(1),
            preserve_fsa: None,
            dirty_on_metadata_change: Some(1),
            verify_days: Some(90),
            ..ArchiveSetInfo::default()
        });
        for line in [
            "  Warehouse copies: 1\n",
            "  Preserve xattrs:  no\n",
            "  Preserve ACLs:    yes\n",
            "  Preserve FSA:     -\n",
            "  Dirty on metadata change: yes\n",
            "  Verify interval:  90 days\n",
            "  Slice size:       -\n",
        ] {
            assert!(text.contains(line), "missing {line:?} in:\n{text}");
        }
    }

    /// Issue #346: the `--slice-size` help example still said `"2400G"`,
    /// the whole-tape default retired in 2026-07 — pinned to the real
    /// default so the two cannot drift apart again.
    #[test]
    fn slice_size_help_names_the_current_default() {
        use clap::CommandFactory;
        let default = crate::config::DefaultsConfig::default().slice_size;
        let cli = crate::cli::Cli::command();
        let archive_set = cli.find_subcommand("archive-set").unwrap();
        for sub in ["create", "edit"] {
            let help = archive_set
                .find_subcommand(sub)
                .unwrap()
                .get_arguments()
                .find(|a| a.get_id() == "slice_size")
                .and_then(|a| a.get_help())
                .map(|h| h.to_string())
                .unwrap_or_default();
            assert!(
                help.contains(&format!("\"{default}\"")),
                "archive-set {sub} --slice-size help {help:?} must name {default}"
            );
        }
    }

    /// Issue #48 item 4: the `unit_count` subqueries in `List`/`Info` were
    /// always correct SQL — they simply had nothing to count, since
    /// nothing wrote `units.archive_set_id`. This proves the count becomes
    /// non-zero once a unit is linked through the REAL writer path
    /// (`unit::init_unit`), not by asserting the SQL is right in the
    /// abstract — "verify, don't assume."
    #[test]
    fn list_and_info_report_nonzero_unit_count_once_a_unit_is_linked() {
        let conn = fresh_conn();
        let tmp = tempfile::TempDir::new().unwrap();
        let home = tmp.path().join("home");
        std::fs::create_dir_all(&home).unwrap();
        let paths = crate::config::TapectlPaths::new(home);
        paths.ensure_dirs().unwrap();
        crate::tenant::add_tenant(&conn, &paths, "alice", None, false).unwrap();

        let config = Config::default();
        run(
            &conn,
            &config,
            &create_cmd("cold", Some(3), None),
            false,
            false,
        )
        .unwrap();

        // The identical shape of subquery `List`/`Info` run, against the
        // pre-link state — proven to read 0, the pre-#48 behavior this fix
        // escapes.
        let count_for_cold = |conn: &Connection| -> i64 {
            conn.query_row(
                "SELECT COUNT(*) FROM units u
                 JOIN archive_sets a ON a.id = u.archive_set_id
                 WHERE a.name = 'cold'",
                [],
                |row| row.get(0),
            )
            .unwrap()
        };
        assert_eq!(count_for_cold(&conn), 0);

        let src = tmp.path().join("src");
        std::fs::create_dir_all(&src).unwrap();
        crate::unit::init_unit(
            &conn,
            &paths,
            src.to_str().unwrap(),
            "alice",
            Some("unit1"),
            &[],
            Some("cold"),
        )
        .unwrap();

        assert_eq!(
            count_for_cold(&conn),
            1,
            "unit_count must become non-zero once a unit is actually linked \
             through the real writer path, now that #48 gives archive_set_id one"
        );

        // The real CLI handlers must also run cleanly end-to-end against
        // this now-linked state (List and Info both run the identical
        // shape of subquery just proven above).
        run(&conn, &config, &ArchiveSetCommands::List, true, false).unwrap();
        run(
            &conn,
            &config,
            &ArchiveSetCommands::Info {
                name: "cold".to_string(),
            },
            true,
            false,
        )
        .unwrap();
    }

    /// Issue #48 item 5: `Edit` used to log one generic "edited" event
    /// with field/old/new all `None`. It must instead log one event PER
    /// changed field, with real old and new values.
    #[test]
    fn edit_emits_per_field_events_with_real_old_and_new_values() {
        let conn = fresh_conn();
        let config = Config::default();
        run(
            &conn,
            &config,
            &create_cmd("cold", Some(2), Some("none")),
            false,
            false,
        )
        .unwrap();

        run(
            &conn,
            &config,
            &ArchiveSetCommands::Edit {
                name: "cold".to_string(),
                policy: PolicyArgs {
                    min_copies: Some(5),
                    compression: Some("lzma".to_string()),
                    ..PolicyArgs::default()
                },
            },
            false,
            false,
        )
        .unwrap();

        let mut stmt = conn
            .prepare(
                "SELECT field, old_value, new_value FROM events
                 WHERE entity_type = 'archive_set' AND action = 'edited'
                 ORDER BY field",
            )
            .unwrap();
        let rows: Vec<(String, Option<String>, String)> = stmt
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))
            .unwrap()
            .collect::<std::result::Result<Vec<_>, _>>()
            .unwrap();

        assert_eq!(
            rows.len(),
            2,
            "exactly the two edited fields must be logged, not one generic event: {rows:?}"
        );
        let min_copies_event = rows.iter().find(|(f, _, _)| f == "min_copies").unwrap();
        assert_eq!(min_copies_event.1.as_deref(), Some("2"));
        assert_eq!(min_copies_event.2, "5");

        let compression_event = rows.iter().find(|(f, _, _)| f == "compression").unwrap();
        assert_eq!(compression_event.1.as_deref(), Some("none"));
        assert_eq!(compression_event.2, "lzma");
    }

    /// Issue #48 item 5: the 8 field UPDATEs used to autocommit
    /// independently — a failure partway through could leave some fields
    /// changed and others not. No column in `archive_sets` has a CHECK
    /// constraint to violate naturally (confirmed against
    /// `db/migrations/001_initial.sql`), so this injects a deterministic
    /// failure via a test-local trigger — the same "injectable failure,
    /// not real fault injection" spirit as `staging::tests`' `FlakyReader`
    /// — that fires only on a sentinel value no real `Edit` invocation
    /// would ever send, scoped to this test's own in-memory connection.
    #[test]
    fn edit_rolls_back_every_field_when_one_update_fails_partway_through() {
        let conn = fresh_conn();
        let config = Config::default();
        run(
            &conn,
            &config,
            &create_cmd("cold", Some(2), None),
            false,
            false,
        )
        .unwrap();
        conn.execute(
            "UPDATE archive_sets SET checksum_mode = 'mtime_size' WHERE name = 'cold'",
            [],
        )
        .unwrap();

        conn.execute_batch(
            "CREATE TRIGGER reject_sentinel_checksum_mode
             BEFORE UPDATE OF checksum_mode ON archive_sets
             WHEN NEW.checksum_mode = 'REJECT_ME_TEST_SENTINEL'
             BEGIN SELECT RAISE(FAIL, 'test-injected failure'); END;",
        )
        .unwrap();

        // min_copies is updated first (field order in `Edit`) and would
        // succeed on its own; checksum_mode is updated later in the SAME
        // transaction and is the poisoned one. If the transaction is truly
        // atomic, min_copies must come back unchanged after Edit errors.
        let result = run(
            &conn,
            &config,
            &ArchiveSetCommands::Edit {
                name: "cold".to_string(),
                policy: PolicyArgs {
                    min_copies: Some(99),
                    checksum_mode: Some("REJECT_ME_TEST_SENTINEL".to_string()),
                    ..PolicyArgs::default()
                },
            },
            false,
            false,
        );
        assert!(
            result.is_err(),
            "the poisoned update must surface as an error"
        );

        let (min_copies, checksum_mode): (i64, String) = conn
            .query_row(
                "SELECT min_copies, checksum_mode FROM archive_sets WHERE name = 'cold'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(
            min_copies, 2,
            "min_copies must be rolled back to its pre-edit value — it must NOT \
             be left at 99 even though its own UPDATE succeeded before the \
             later checksum_mode UPDATE failed in the same transaction"
        );
        assert_eq!(
            checksum_mode, "mtime_size",
            "checksum_mode must be unchanged"
        );

        let event_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM events WHERE entity_type = 'archive_set' AND action = 'edited'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            event_count, 0,
            "no per-field event should survive a rolled-back edit"
        );
    }
}
