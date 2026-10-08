pub mod busy;
pub mod catalog_snapshot;
pub mod events;
pub mod export;
pub mod files;
#[allow(dead_code)]
pub mod models;
pub mod ontape_catalog;
pub mod phase_timings;
pub mod queries;
pub mod stats;

use std::path::Path;

use rusqlite::Connection;
use rusqlite_migration::{Error as MigrationError, Migrations, M};
use tracing::warn;

use crate::error::{Result, TapectlError};

/// Open (or create) the database and run migrations.
///
/// Issue #41: `tapectl.db` holds every filename, path, size, mtime, sha256,
/// and tenant/unit name tapectl has recorded — precisely the plaintext
/// content-metadata index the on-tape format works hard to keep out of
/// plaintext. `Connection::open` sets no mode of its own, so this used to
/// land at whatever the process umask handed out. Tightened here (not just
/// on creation) since this function runs on *every* command invocation —
/// unlike `TapectlPaths::ensure_dirs` (init-only until this same change
/// wired it into `main.rs`'s general dispatch), this is the one fix that
/// reaches an already-initialized `~/.tapectl` on its own. `secure_path`
/// warns rather than fails, so a database this process doesn't own doesn't
/// break every command that touches it.
pub fn open(path: &Path) -> Result<Connection> {
    let mut conn = Connection::open(path)?;
    configure(&conn)?;
    migrate(&mut conn)?;
    recover_orphaned_sessions(&conn, path)?;
    optimize(&conn);
    crate::config::secure_path(path, 0o600);
    // WAL mode (set in `configure`) writes pending pages to `<path>-wal`
    // (and its `-shm` index) — the exact same content as the main db file,
    // just not checkpointed into it yet. Tighten them too, if SQLite has
    // created them by this point; best-effort, since whether they exist
    // yet is a SQLite-internal timing detail this function doesn't control.
    for suffix in ["-wal", "-shm"] {
        let sidecar = sidecar_path(path, suffix);
        if sidecar.exists() {
            crate::config::secure_path(&sidecar, 0o600);
        }
    }
    Ok(conn)
}

/// Build `<path>-wal` / `<path>-shm` the way SQLite itself names them —
/// appended directly to the full filename, not swapping the extension.
fn sidecar_path(path: &Path, suffix: &str) -> std::path::PathBuf {
    let mut os = path.as_os_str().to_os_string();
    os.push(suffix);
    std::path::PathBuf::from(os)
}

/// Open an in-memory database for testing.
#[cfg(test)]
pub fn open_memory() -> Result<Connection> {
    let mut conn = Connection::open_in_memory()?;
    configure(&conn)?;
    migrate(&mut conn)?;
    Ok(conn)
}

/// Open an in-memory database migrated to EXACTLY `version` through the
/// production migration list — for a test whose claim is about one
/// migration's effect and must not drift the moment a later migration
/// legitimately touches the same table (issue #296: 021 rebuilt
/// `health_logs`, and a 019 pin written against "latest" started measuring
/// 021 instead of 019).
#[cfg(test)]
pub(crate) fn open_memory_at_version(version: usize) -> Connection {
    let mut conn = Connection::open_in_memory().unwrap();
    configure(&conn).unwrap();
    conn.pragma_update(None, "foreign_keys", "OFF").unwrap();
    migrations().to_version(&mut conn, version).unwrap();
    conn.pragma_update(None, "foreign_keys", "ON").unwrap();
    conn
}

/// The values `table.status`'s CHECK admits in the LIVE schema (every
/// migration applied), sorted -- for pinning a hand-copied closed set (a
/// `--status` filter's list, a classifier's cases) to the schema instead of
/// to a migration file that a later migration supersedes (issue #362: the
/// `is_write_target` pin read 017's text and kept passing after the CHECK
/// moved on).
#[cfg(test)]
pub(crate) fn live_status_check(table: &str) -> Vec<String> {
    let conn = open_memory().unwrap();
    let sql: String = conn
        .query_row(
            "SELECT sql FROM sqlite_master WHERE type = 'table' AND name = ?1",
            [table],
            |r| r.get(0),
        )
        .unwrap_or_else(|e| panic!("no table {table:?} in the live schema: {e}"));
    let flat = sql.split_whitespace().collect::<Vec<_>>().join(" ");
    let needle = "CHECK(status IN (";
    let start = flat
        .find(needle)
        .unwrap_or_else(|| panic!("no `{needle}` on {table}: {flat}"))
        + needle.len();
    let end = flat[start..]
        .find(')')
        .expect("the status CHECK closes its IN (...) list")
        + start;
    let mut set: Vec<String> = flat[start..end]
        .split(',')
        .map(|t| t.trim().trim_matches('\'').to_string())
        .filter(|t| !t.is_empty())
        .collect();
    set.sort();
    set
}

/// Give the query planner statistics (issue #373): `PRAGMA optimize=0x10002`
/// at open, as SQLite recommends for a connection opened per command. It
/// runs ANALYZE, under SQLite's default analysis limit, only on a table the
/// planner would profit from, and records the result in `sqlite_stat1`.
/// Without statistics SQLite rated `status = ?` as selective as
/// `stage_set_id = ?` and drove the coverage queries quadratic; migration
/// 027 dropped the indexes that made that plan possible, and this is the
/// backstop for the next index that does the same.
///
/// Best-effort and never waiting. ANALYZE writes, so it needs the write
/// lock, and `db::busy`'s rule 1 is that a command which only reads never
/// waits for it. The busy timeout is 0 for this one statement: if another
/// process is writing, the statistics wait for the next open, and a failure
/// of any kind only logs.
fn optimize(conn: &Connection) {
    let run = || -> rusqlite::Result<()> {
        conn.pragma_update(None, "busy_timeout", 0)?;
        let r = conn.execute_batch("PRAGMA optimize=0x10002");
        conn.pragma_update(None, "busy_timeout", BUSY_TIMEOUT_MS)?;
        r
    };
    if let Err(e) = run() {
        tracing::debug!(error = %e, "PRAGMA optimize skipped");
        let _ = conn.pragma_update(None, "busy_timeout", BUSY_TIMEOUT_MS);
    }
}

/// The wait for another connection's lock before `SQLITE_BUSY` (`db::busy`).
const BUSY_TIMEOUT_MS: i64 = 5000;

/// Set WAL mode and other pragmas.
fn configure(conn: &Connection) -> Result<()> {
    conn.pragma_update(None, "journal_mode", "WAL")?;
    conn.pragma_update(None, "foreign_keys", "ON")?;
    conn.pragma_update(None, "busy_timeout", BUSY_TIMEOUT_MS)?;
    Ok(())
}

fn migrations() -> Migrations<'static> {
    Migrations::new(vec![
        M::up(include_str!("migrations/001_initial.sql")),
        M::up(include_str!("migrations/002_fts5_catalog.sql")),
        // 003 rebuilds `volumes` (create/copy/drop/rename) to extend its status CHECK;
        // `.foreign_key_check()` runs `PRAGMA foreign_key_check` before commit (step 10 of
        // SQLite's 12-step schema-change procedure) so any FK violation aborts the migration
        // instead of landing silently. See migrate() below for why FK enforcement also has to
        // be toggled outside this migration's transaction.
        M::up(include_str!("migrations/003_v2_lifecycle.sql")).foreign_key_check(),
        // 004 adds volumes.uuid — a real, independent volume identifier (the v2 ID
        // thunk pairs it with label, and §2.1 seeds the envelope permutation from
        // it). See the migration for why deriving it from the label was rejected.
        M::up(include_str!("migrations/004_volume_uuid.sql")),
        // 005 adds files.file_type/link_target and manifest_entries.file_type/
        // link_target (issue #33/H7): the walk and the validator must agree on
        // link-following semantics, so the walk's classification is recorded and
        // the validator filters its content-validation set on it directly. See
        // the migration for the full defect history.
        M::up(include_str!("migrations/005_file_types.sql")),
        // 006 adds writes.session_dir (issue #25): the pointer to the frozen
        // staging directory a restarted process needs to REHYDRATE an
        // interrupted session's Layout. It cannot be regenerated — see the
        // migration for the `created_at`/`mam_loads` proof.
        M::up(include_str!("migrations/006_write_session_dir.sql")),
        // 007 adds locations.kind, archive_sets.warehouse_copies and the
        // volume_deposits table (issue #73, ADR-0006): a warehouse copy is a
        // RECORDED deposit of an already-sealed volume, not a second volume and
        // not a change of the cartridge's location. See the migration header for
        // why it is not a volume_locations join table.
        M::up(include_str!("migrations/007_warehouse_locations.sql")),
        // 008 drops volumes.storage_class (issue #101): dead since 001, and
        // after 007 it collides by name with volume_deposits.storage_class,
        // which means something different. A cartridge has a media_type, not a
        // storage class. See the migration header.
        M::up(include_str!("migrations/008_drop_volume_storage_class.sql")),
        // 009 adds health_logs.tape_alerts (issue #107): the value has always
        // been parsed from log page 0x2e and then discarded for want of a
        // column. Nullable with no default — NULL means "not recorded", 0
        // means "recorded, none raised". See the migration header.
        M::up(include_str!("migrations/009_health_tape_alerts.sql")),
        // 010 adds stage_sets.origin ('staged' | 'rebuilt') (issue #137): a
        // stage set rebuilt from a tape's envelope has no recorded recipient
        // list, and the escrow predicate must tell "never written down" from
        // "could not have been written down". See the migration header.
        M::up(include_str!("migrations/010_stage_set_origin.sql")),
        // 011 adds a PARTIAL unique index on cartridges.serial_number
        // (ADR-0010): `volume init` binds the cartridge it is writing by
        // matching the loaded medium's MAM serial to that column, so a
        // repeated serial would bind to an arbitrary row. See the migration
        // header for why it is partial.
        M::up(include_str!("migrations/011_cartridge_serial_index.sql")),
        // 012 rebuilds `cartridges` (create/copy/drop/rename) to drop 'offsite'
        // from its status CHECK and to index `location_id` (ADR-0011, issue
        // #148): a cartridge's PLACE is a location, and its status is only its
        // fitness to hold data. `.foreign_key_check()` for the same reason as
        // 003 — `cartridge_volumes` holds a `REFERENCES cartridges(id)` FK, and
        // a rebuild that renumbered rows would orphan it silently. See the
        // migration header for the two traps in the rebuild order.
        M::up(include_str!("migrations/012_cartridge_lifecycle.sql")).foreign_key_check(),
        // 013 rebuilds `manifest_entries` (create/copy/drop/rename) without
        // `has_xattrs`/`has_acls` (issue #149): both were written as literal 0
        // under a comment claiming they were populated on stage, and read by
        // nothing — a column that is always zero misleads more than an absent
        // one. dar owns xattr/ACL handling. `.foreign_key_check()` even though
        // nothing holds a `REFERENCES manifest_entries(id)` FK: the table does
        // hold one OUT (to `manifests`), and the check is cheap insurance that
        // the rebuild carried every row's parent intact. See the migration
        // header.
        M::up(include_str!("migrations/013_drop_manifest_entry_flags.sql")).foreign_key_check(),
        // 014 adds the nullable `cartridge_volumes.identity_source` ('mam' |
        // 'operator'), the provenance of the identity a binding was
        // established under (ADR-0012, ADR-0010's 2026-09-14 amendment, issue
        // #192). It is what File 0's `[media].cartridge_identity_source` is
        // read back from at write time, so the tape and the catalog cannot
        // disagree about which string identifies the cartridge. A plain ADD
        // COLUMN — no rebuild, so no `.foreign_key_check()`; legacy rows stay
        // NULL, which means unknown and never 'mam'. See the migration header
        // for the three sources that were rejected.
        M::up(include_str!(
            "migrations/014_cartridge_binding_identity_source.sql"
        )),
        // Issue #184: data-only backfill. Every stored 0 predates any writer
        // of this column, so each one is "never observed" rather than "zero
        // loads"; NULL is how the new code and the display say that. No
        // `.foreign_key_check()` — this touches no key, only a nullable
        // counter on rows that already exist.
        M::up(include_str!(
            "migrations/015_cartridge_load_count_unknown.sql"
        )),
        // 016 adds `cartridges.operator_serial` (ADR-0012's 2026-09-16
        // amendment, issue #197): `serial_number` is the chip-read identity
        // and after this migration is written ONLY from a MAM read;
        // `operator_serial` is the operator's typed claim, written only by
        // `cartridge register --serial` and `cartridge edit --serial`.
        // `lookup_cartridge` falls back to it only while `serial_number IS
        // NULL`. Plain ADD COLUMN, no rebuild, no backfill -- see the
        // migration header for why existing `serial_number` values are left
        // exactly where they are.
        M::up(include_str!("migrations/016_cartridge_operator_serial.sql")),
        // 017 rebuilds `volumes` (create/copy/drop/rename) to add
        // `observed_condition` and to drop 'quarantined' from the `status`
        // CHECK (ADR-0012's 2026-09-17 amendment "the status column is the
        // operator's; a medium's condition is its own fact", issue #242):
        // four writers set `volumes.status = 'quarantined'` unconditionally,
        // overwriting a terminal operator status like `retired`, and
        // `policy::coverage::eligible` silently stopped counting a copy by
        // moving `status` OFF `sealed` rather than by any dedicated
        // predicate. `.foreign_key_check()` for the same reason as 003 and
        // 012 -- five tables hold a `REFERENCES volumes(id)` FK, and a
        // rebuild that renumbered rows would orphan every one of them
        // silently. See the migration header for the full rationale and the
        // deliberate narrowing of this ADR's own "restore to `sealed`"
        // fallback for rows a write-path (never-sealed) quarantine produced.
        M::up(include_str!("migrations/017_volume_observed_condition.sql")).foreign_key_check(),
        // 018 adds `volumes.sealed_at` (ADR-0012's 2026-09-21 correction "the
        // seal is RECORDED, not inferred", issue #277): the three-condition
        // resume-reconfirm check the amendment above this one added all
        // route through `seal_marker_parses_at`, which returns `false` both
        // for "no marker" and for "the read errored" -- correct for a fresh
        // write to a blank tape, fatal on resume, since the one
        // `MismatchKind` that produces `Inconclusive` in the first place is
        // `SealUnreadable`. Without a recorded fact, "execute finished,
        // seal() never ran" (resume must seal) and "execute finished, seal()
        // ran, confirm was Inconclusive" (resume must never seal) are
        // indistinguishable, and a rule derived from the tape alone gets one
        // of them wrong -- rewriting a physically sealed cartridge, ADR-0003
        // bypassed. Set once, at the single `seal()` call site
        // (`write::finish_session`), never cleared by any confirm outcome.
        // Plain ADD COLUMN, no rebuild -- see the migration header for why
        // none of 017's rebuild machinery is needed here.
        M::up(include_str!("migrations/018_volume_sealed_at.sql")),
        // 019 creates `drives` (ADR-0013 §1 "There is a `drives` table",
        // issue #295): sg_logs pages 0x02/0x03/0x2E are drive-resident
        // counters, and until now the only subject `health_logs` could name
        // was `volume_id` -- so "is it the drive or the tape?", the central
        // question in tape diagnostics, had no column to group by. Keyed on
        // the SCSI Unit Serial Number; no serial means no row, never a
        // composite key invented from vendor + model + device path.
        //
        // It creates a table and touches nothing else -- in particular it
        // adds NO column to `health_logs`. ADR-0013 §3 gives that table
        // exactly ONE rebuild, migration 021 (issue #296), which reaches the
        // drive and the cartridge through `contact_id` rather than through
        // private columns; four drafts in this suite each proposed their own,
        // and four uncoordinated rebuilds of the table holding the schema's
        // largest blobs is how the suite would lose the data it exists to
        // capture. No rebuild here, so no `.foreign_key_check()` -- nothing
        // references `drives` yet; migration 020 will. See the migration
        // header for the full rationale.
        M::up(include_str!("migrations/019_drives.sql")),
        // 020 creates `cartridge_contacts` (ADR-0013 §2 "The contact row is
        // the spine", issue #296): thirteen code paths put a cartridge in a
        // drive and not one wrote a row saying it happened. `cartridge_volumes`
        // is a BINDING record -- `UNIQUE(volume_id)`, one row per volume for
        // the life of that volume -- so ten verifies leave it exactly as
        // `volume init` wrote it, and `cartridges.total_load_count` has
        // exactly one writer, also `volume init`.
        //
        // Every foreign key on it is nullable and each NULL means something
        // different; `identity_reason` is never NULL when `cartridge_id` is,
        // because a NULL with no reason is the data loss this suite exists to
        // stop. `operation` is free TEXT (ADR-0013 §4) with a pinning test,
        // and it is a DIFFERENT vocabulary from `health_logs.operation` --
        // the command verbatim, not what kind of reading a row is.
        //
        // It creates one table and touches nothing else -- in particular it
        // adds NO column to `health_logs`, whose one permitted rebuild is
        // migration 021 (ADR-0013 §3, the second pass of this same issue).
        // No rebuild here, so no `.foreign_key_check()` -- nothing references
        // `cartridge_contacts` yet; migration 022 will. See the migration
        // header for the full rationale, including why `db::open` grows no
        // recovery sweep for `closed_at IS NULL` (issue #98).
        M::up(include_str!("migrations/020_cartridge_contacts.sql")),
        // 021 rebuilds `health_logs` (ADR-0013 §3 "`health_logs` becomes a
        // child of the contact row — in exactly one rebuild", issue #296,
        // pass 2). THE ONLY REBUILD THIS TABLE GETS: four issues in the
        // tape-forensics suite each proposed one, each saying "coordinate
        // with siblings", and migrations do not come back — so every
        // sibling's requirement lands here at once. `contact_id` FK,
        // `volume_id` nullable (a drive-only reading has no volume),
        // `tapectl_version` (§7), and the `operation` CHECK dropped for free
        // TEXT (§4 — it permitted `read` and `clean`, which no code has ever
        // written). `raw_log` and 009's NULL-vs-0 `tape_alerts` distinction
        // both survive untouched, and NOTHING is backfilled.
        //
        // `.foreign_key_check()` for the same reason as 003/012/013/017: this
        // is a create/copy/drop/rename rebuild. Nothing holds an INBOUND
        // reference to `health_logs` — it is a leaf, so this drop can orphan
        // no other table's rows — but it owns three OUTBOUND edges
        // (`volume_id`, `session_id`, `contact_id`) and a rebuild that
        // renumbered rows or lost an edge would leave them dangling in
        // silence. The check is what makes that loud.
        M::up(include_str!("migrations/021_health_logs_contact.sql")).foreign_key_check(),
        // 022 creates `mam_journal` (ADR-0013 §5, issue #297): every MAM read
        // verbatim, because the attributes `MamInfo` does not keep -- the
        // "vendor/serial at last load" ring, the per-load counters -- are
        // overwritten by loading the cartridge. The journal points at the
        // contact (`contact_id`, nullable), never the reverse: one read-path
        // contact takes two reads. Append-only, never pruned, never
        // backfilled. Plain CREATE, touching no other table, so no
        // `.foreign_key_check()`. See the migration header.
        M::up(include_str!("migrations/022_mam_journal.sql")),
        // 023 creates `log_page_journal` (ADR-0013, "Two hazards"; issue
        // #298): every SCSI log page one health sweep read -- page 0x00, then
        // each page it lists, at most once per contact -- as the response
        // bytes verbatim plus the offline decode. Points at the contact, like
        // 022. Append-only, never pruned. Plain CREATE, touching no other
        // table, so no `.foreign_key_check()`. See the migration header.
        M::up(include_str!("migrations/023_log_page_journal.sql")),
        // 024 creates `restores` (ADR-0013 §§2, 5, 7; ADR-0012 amendment
        // 2026-09-24 item 2; issue #306): one row per restore -- unit, file
        // or raw-volume -- written once its contact has opened, on success
        // and on failure alike, with what came back, where to, how it ended,
        // and dar's stdout/stderr VERBATIM (kept as an excerpt on failure
        // and thrown away on success until now). Points at the contact, like
        // 022/023. Append-only, never pruned. Plain CREATE, touching no
        // other table, so no `.foreign_key_check()`. See the migration
        // header.
        M::up(include_str!("migrations/024_restores.sql")),
        // 025 creates `st_stats_journal` (issue #301; ADR-0012 2026-09-24
        // amendment item 2): the st driver's per-device sysfs I/O counters,
        // every file verbatim, read at each contact's open and close so the
        // difference across a contact is a query. Points at the contact, like
        // 022/023. Append-only, never pruned. Plain CREATE, touching no other
        // table, so no `.foreign_key_check()`. See the migration header.
        M::up(include_str!("migrations/025_st_stats_journal.sql")),
        // 026 rebuilds `units`, `snapshots` and `volumes` (create/copy/drop/
        // rename, all three in one migration) to drop the five status values
        // no code has ever written -- units 'retired', snapshots 'superseded'
        // and 'failed', volumes 'blank' and 'missing' -- and `volumes.status`
        // loses its DEFAULT ('blank' was it) so every insert must name a
        // status (issue #362). No remapping: a row in a dropped state was set
        // by hand, and 026 refuses by name rather than guess (see `migrate()`
        // for how that refusal reaches the operator). `.foreign_key_check()`
        // for the same reason as 003/012/017: seventeen foreign keys point
        // into these three tables. See the migration header.
        M::up(include_str!("migrations/026_drop_unwritten_states.sql")).foreign_key_check(),
        // 027 drops the write-only `manifests`/`manifest_entries` tables
        // (issue #372), the two `files` indexes no query plan uses, and the
        // six single-column status indexes that drove the coverage queries
        // quadratic (issue #373); `files_au` becomes `AFTER UPDATE OF path`
        // so a sha256 backfill no longer rewrites the FTS index. No table is
        // rebuilt and no surviving row is touched, so no
        // `.foreign_key_check()`. A guard in 026's style refuses by name if
        // `manifest_entries` holds a path or a sha256 `files` does not.
        // `migrate()` VACUUMs once after it commits. See the migration
        // header.
        M::up(include_str!(
            "migrations/027_drop_manifests_and_dead_indexes.sql"
        )),
        // 028 creates `phase_timings` (issue #386): one row per phase of a
        // long operation -- stage create, volume write/resume, verify --
        // with its duration and bytes, grouped by the session whose log
        // (`<home>/logs/<session>.log`) holds the same phases and their
        // waits. Plain CREATE touching no other table, so no
        // `.foreign_key_check()`; both subject keys are `ON DELETE SET
        // NULL` so a snapshot delete never trips on them. See the header.
        M::up(include_str!("migrations/028_phase_timings.sql")),
        // 029 creates `readback_checkpoints` (issue #410): one row per file a
        // full readback (a write's `--full-confirm`, a full `volume verify`)
        // read back clean, written as the walk goes, so `volume resume
        // --full-confirm` or the next full verify continues an interrupted
        // readback instead of re-reading the whole tape. New table, no row
        // read or converted, plain CREATE, so no `.foreign_key_check()`;
        // rows cascade with their `verification_sessions` row.
        // See the header.
        M::up(include_str!("migrations/029_readback_checkpoints.sql")),
        // 030 stores per-file data once per unit (issue #380 option A,
        // #381; ADR-0012 item 7): `paths` (+ `paths_fts`) and the narrow
        // WITHOUT ROWID `file_versions`, converted from `files` exactly or
        // refused by row id; `files` and `files_fts` are dropped. The new
        // tables reference `units`/`snapshots`/`paths`, hence
        // `.foreign_key_check()`. `migrate()` VACUUMs after it. See the
        // header.
        M::up(include_str!("migrations/030_paths_and_file_versions.sql")).foreign_key_check(),
        // 031 indexes `events(action, timestamp)` (issue #417): `audit`'s
        // Heir Kit check (`MAX(timestamp) ... WHERE action = ?`) walked the
        // whole table. Index only, no rows touched, so no
        // `.foreign_key_check()`.
        M::up(include_str!("migrations/031_events_action_index.sql")),
        // 032 rebuilds `volumes` to drop status 'full' from its CHECK
        // (ADR-0012 amendment 2026-10-07 item 16): nothing has ever written
        // it. 026's shape and 026's rule -- a row carrying it was set by hand
        // and is refused by id, never remapped. `.foreign_key_check()` for
        // the same reason as 026: every table that names a volume points
        // into the rebuilt one. See the header.
        M::up(include_str!("migrations/032_drop_volume_status_full.sql")).foreign_key_check(),
        // 033 rebuilds `cartridges` without `total_bytes_written`,
        // `total_bytes_read` and `error_history` (ADR-0012 amendment
        // 2026-10-07 item 35): nothing has ever written or read them. A row
        // carrying a value other than their default was set by hand and is
        // refused by id, never dropped silently. `.foreign_key_check()`:
        // `cartridge_volumes` and `cartridge_contacts` point into the rebuilt
        // table. See the header.
        M::up(include_str!(
            "migrations/033_drop_dead_cartridge_columns.sql"
        ))
        .foreign_key_check(),
        // 034 keeps dar's report for every `dar -c` a stage set ran
        // (issue #343): one append-only row per run, `stage_set_id` ON
        // DELETE SET NULL with the names kept beside it. A new table, no
        // rows touched. See the header.
        M::up(include_str!("migrations/034_dar_create_reports.sql")),
        // 035 journals the st driver's whole MTIOCGET status at each tape
        // device's open, close and failed command (issue #344), against the
        // contact. A new table, no rows touched. See the header.
        M::up(include_str!("migrations/035_mtget_journal.sql")),
    ])
}

/// The migrations that free most of a catalog: 027 dropped the manifest
/// tables and the dead indexes (issue #372, about half of the production
/// catalog in 2026-09), and 030 replaced `files`/`files_fts` with the
/// interned shape (issue #380). `migrate()` VACUUMs once right after an open
/// that applies either, so the space goes back to the filesystem.
const VACUUM_AFTER_VERSIONS: [i64; 2] = [27, 30];

fn migrate(conn: &mut Connection) -> Result<()> {
    let before: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
    migrate_to(conn, None)?;
    let after: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
    // Issue #372: once, on the open that applies 027 to an existing
    // catalog. VACUUM cannot run inside the migration's transaction, and a
    // fresh catalog (`before == 0`) has nothing to give back. It needs about
    // twice the database in free disk; a failure (a full disk, another
    // process holding the catalog) leaves the space unreclaimed but the
    // schema correct, so it warns rather than failing the command, and the
    // operator can run `sqlite3 tapectl.db VACUUM` later.
    let crossed = VACUUM_AFTER_VERSIONS
        .iter()
        .rev()
        .find(|v| before < **v && after >= **v);
    if let (true, Some(v)) = (before > 0, crossed) {
        if let Err(e) = conn.execute_batch("VACUUM") {
            warn!(
                error = %e,
                "migration {v:03} applied, but the VACUUM that returns the freed space \
                 failed; the catalog is correct, only larger than it needs to be"
            );
        }
    }
    Ok(())
}

/// Run the production migration list up to `target` (`None` = the latest),
/// with the foreign-key handling and error mapping every caller needs.
/// `migrate` is the production entry; tests re-target a claim about one
/// migration with `Some(version)` so a later migration cannot move it.
fn migrate_to(conn: &mut Connection, target: Option<usize>) -> Result<()> {
    // Migration 003 does DROP TABLE volumes while five tables (cartridge_volumes,
    // volume_movements, writes, verification_sessions, health_logs) hold rows with a
    // `REFERENCES volumes(id)` foreign key. Migration 012 does the same to `cartridges`,
    // which `cartridge_volumes` references. `configure()` turns `foreign_keys` ON for this
    // connection, and SQLite refuses to drop a table that other rows still reference while
    // FK enforcement is on.
    //
    // Verified finding: rusqlite_migration 2.5.0 does NOT toggle `PRAGMA foreign_keys` around
    // migrations itself (confirmed by reading the vendored source: `goto_up`/`goto_down` in
    // lib.rs open exactly one transaction per to_latest()/to_version() call and never touch
    // that pragma; grep across the crate's source shows `foreign_keys` mentioned only in doc
    // comments). Those doc comments (`M::foreign_key_check`) explicitly instruct callers to
    // toggle the pragma on the Connection before/after calling `to_latest()`, and warn that
    // toggling it *inside* a migration's SQL is a no-op once a transaction is open -- which it
    // already is by the time that SQL runs. So this has to happen here, outside the crate's
    // transaction, per steps 1 and 12 of SQLite's documented 12-step "Making Other Kinds Of
    // Table Schema Changes" procedure (step 10, the pre-commit foreign_key_check, is covered by
    // `.foreign_key_check()` on the 003 and 012 migrations above).
    conn.pragma_update(None, "foreign_keys", "OFF")?;
    let ms = migrations();
    let run = match target {
        None => ms.to_latest(conn),
        Some(v) => ms.to_version(conn, v),
    };
    let result = run.map_err(|e| {
        // Issue #233: the message is computed before matching on `e` by
        // value below (matching moves it), and the match is on the typed
        // `rusqlite_migration::Error::ForeignKeyCheck` variant, never on
        // this string — a corrupt schema must not get repair advice that
        // does not apply to it.
        let msg = e.to_string();
        match e {
            MigrationError::ForeignKeyCheck(_) => TapectlError::DatabaseNeedsRepair(msg),
            // Issue #362: a migration that refuses on purpose does it with
            // `RAISE(ABORT, <message>)` (026's dropped-state guard), which
            // SQLite reports as SQLITE_CONSTRAINT_TRIGGER. That message is
            // written for the operator, so it is shown as it is --
            // `rusqlite_migration`'s Display would put the whole migration
            // script in front of it. Typed on the extended code, never on
            // the text; a CHECK failure (SQLITE_CONSTRAINT_CHECK) and
            // everything else keep the full `msg` below.
            MigrationError::RusqliteError {
                err: rusqlite::Error::SqliteFailure(failure, Some(raised)),
                ..
            } if failure.extended_code == rusqlite::ffi::SQLITE_CONSTRAINT_TRIGGER => {
                TapectlError::Migration(raised)
            }
            _ => TapectlError::Migration(msg),
        }
    });
    conn.pragma_update(None, "foreign_keys", "ON")?;
    result?;
    warn_030_nulled_mtimes(conn)
}

/// ADR-0012 amendment 2026-10-07, item 9: migration 030 converts a
/// `modified_at` no i64 count of nanoseconds holds (before 1677-09-21 or
/// after 2262-04-11) to a NULL `mtime_ns` instead of refusing, and leaves the
/// rows it did that to in the TEMP table `m030_mtime_nulled`. Read once the
/// migration has committed -- a refusal rolls the table back with
/// everything else, so a warning is never printed for a conversion that did
/// not land -- named in one WARN (the first ten rows, then the count), and
/// dropped. A no-op on every open that did not apply 030 (the table is
/// TEMP: it exists only on the connection that ran the migration).
fn warn_030_nulled_mtimes(conn: &Connection) -> Result<()> {
    let present: bool = conn.query_row(
        "SELECT EXISTS (SELECT 1 FROM sqlite_temp_master
                         WHERE type = 'table' AND name = 'm030_mtime_nulled')",
        [],
        |r| r.get(0),
    )?;
    if !present {
        return Ok(());
    }
    let total: i64 = conn.query_row("SELECT COUNT(*) FROM temp.m030_mtime_nulled", [], |r| {
        r.get(0)
    })?;
    // At most ten rows named, as the refusals name theirs: a catalog with
    // thousands of such files must not print thousands of lines.
    let rows: Vec<String> = conn
        .prepare(
            "SELECT files_id, snapshot_id, path_id, modified_at
               FROM temp.m030_mtime_nulled ORDER BY files_id LIMIT 10",
        )?
        .query_map([], |r| {
            Ok(format!(
                "files row {} (snapshot {}, path id {}, was {})",
                r.get::<_, i64>(0)?,
                r.get::<_, i64>(1)?,
                r.get::<_, i64>(2)?,
                r.get::<_, String>(3)?,
            ))
        })?
        .collect::<rusqlite::Result<_>>()?;
    conn.execute_batch("DROP TABLE temp.m030_mtime_nulled")?;
    if total > 0 {
        let more = if total > rows.len() as i64 {
            format!(", ... ({total} rows)")
        } else {
            String::new()
        };
        warn!(
            "migration 030 recorded no modified time for {} file row(s) whose modified_at is \
             outside 1677-09-21..2262-04-11, the range a nanosecond count holds: {}{}. A walk \
             records none for such a file either, so these units still read as unchanged; \
             nothing else was altered",
            total,
            rows.join("; "),
            more
        );
    }
    Ok(())
}

/// Open the database WITHOUT running migrations — for `db fsck --repair`
/// only (issue #233).
///
/// `.foreign_key_check()` (migrations 003/012/013/017) runs an explicit
/// `PRAGMA foreign_key_check` inside `migrate()`'s transaction, unaffected
/// by the `foreign_keys` enforcement pragma — so there is no way to
/// disable the check and still call `migrate()`. The only way in is to
/// skip `migrate()` entirely and repair against whatever schema is
/// actually on disk. That is safe: `pragma_foreign_key_check` and the
/// repair it drives (`cli::operations::repair_foreign_key_violations`) are
/// schema-agnostic — both read exactly what SQLite itself reports, never a
/// hardcoded table list — so they work identically whether the database
/// is at the latest migration or several behind it (proven in this
/// module's `issue_233_*` tests). The very next ordinary `db::open()` call
/// then migrates the now-clean data forward; `schema_is_current` is how a
/// caller tells whether that still needs to happen.
///
/// Deliberately narrow: no `configure()` (no WAL, no `foreign_keys = ON`
/// — repair wants enforcement OFF, which is `Connection::open`'s own
/// default, and does its own `defer_foreign_keys` inside one transaction
/// regardless), no `recover_orphaned_sessions` (those tables may not exist
/// yet on a pre-migration schema), no permission tightening (the next
/// ordinary open already does that). This connection exists to repair and
/// exit — never hand it to any other command body.
pub fn open_for_repair(path: &Path) -> Result<Connection> {
    let conn = Connection::open(path)?;
    conn.pragma_update(None, "foreign_keys", "OFF")?;
    conn.pragma_update(None, "busy_timeout", 5000)?;
    Ok(conn)
}

/// Whether `conn`'s schema is already at the latest migration (issue
/// #233). `open_for_repair`'s connection never calls `migrate()`, so a
/// database repaired through it stays wherever it started — behind head,
/// possibly several migrations short — until the very next ordinary
/// `db::open()` call completes the migration. `cli::db::run`'s `Fsck` arm
/// uses this to tell the operator that explicitly, rather than let
/// "repaired N rows" read as fully done when a migration is still
/// pending.
pub fn schema_is_current(conn: &Connection) -> Result<bool> {
    let pending = migrations()
        .pending_migrations(conn)
        .map_err(|e| TapectlError::Migration(e.to_string()))?;
    Ok(pending <= 0)
}

/// On startup: detect write sessions orphaned by a crash and mark them
/// resumable, per `docs/design/layout-session.md`'s state table: "Interrupted
/// | SIGINT (clean mark) **or** startup sweep found orphaned `in_progress`
/// (crash). Resumable while the Layout revalidates." A crash is not data
/// loss — the tape may still be fully resumable per the two-case cursor rule
/// (`session::InterruptedSession::resume`) — so this sweep targets
/// `interrupted`, never `aborted` (CONTEXT.md: "Interruption is a Layout
/// transition, not an accident"). `Aborted` is reserved for an explicit
/// operator abandonment, an unrecoverable resume-revalidation failure, or a
/// real EOT — all decided later, inside `session.rs`, never here.
///
/// Only `in_progress` rows are matched (not also `interrupted`, as a
/// pre-T6 version of this sweep did): a row already `interrupted` — from a
/// clean SIGINT mark or a previous run of this same sweep — needs no further
/// action, and re-matching it on every `db::open()` would log a spurious
/// "recovered N sessions" event each time an unresolved interrupted session
/// simply sits there.
///
/// **Lock-aware, all three tables (issues #98, #376).** A row's status alone
/// cannot tell a crashed session from one running right now in another
/// process, and this sweep runs on every `db::open()`, including every
/// read-only command and timer. Until #376 the `writes` and
/// `verification_sessions` arms rewrote every `in_progress` row regardless,
/// so the rule "a row still `in_progress` at open means a live writer"
/// (`docs/design/layout-session.md`'s rehydrate contract, and the `volume
/// abort`/`volume resume` refusals built on it) could never be observed by
/// the command asking. Now each candidate is probed through its lock: a
/// write or verify session through its volume's lock
/// (`staging::lock::volume_session_live`), a stage set through its own
/// (`staging::lock::is_crashed`). Held ⇒ a live process owns it ⇒ left
/// untouched. Free ⇒ crashed ⇒ swept.
///
/// **Reads first, writes only on candidates (issue #377).** Every arm
/// SELECTs, and UPDATEs only when a crashed row exists, so an open with
/// nothing to recover never needs SQLite's write lock — a read-only command
/// opens and runs while another command holds it.
fn recover_orphaned_sessions(conn: &Connection, db_path: &Path) -> Result<()> {
    let mut updated = 0usize;
    for volume_id in in_progress_volumes(conn, "writes", "status")? {
        if !crate::staging::lock::volume_session_live(conn, volume_id) {
            updated += conn.execute(
                "UPDATE writes SET status = 'interrupted'
                 WHERE volume_id = ?1 AND status = 'in_progress'",
                [volume_id],
            )?;
        }
    }
    if updated > 0 {
        warn!(
            count = updated,
            "recovered orphaned write sessions — marked as interrupted (resumable)"
        );
        events::log_event(
            conn,
            "system",
            0,
            None,
            "crash_recovery",
            Some("writes.status"),
            None,
            Some("interrupted"),
            Some(&format!("{updated} sessions")),
            None,
        )?;
    }

    // Issue #98: lock-aware. A `status = 'staging'` row alone cannot tell a
    // crashed stage from one running right now in another process — this
    // sweep runs on *every* `db::open()`, including read-only commands, so
    // a plain status-only UPDATE (the pre-fix version of this sweep) would
    // mark a live invocation's row `'failed'` out from under it. Each
    // candidate row is instead probed via its per-stage-set flock
    // (`staging::lock::is_crashed`): free ⇒ no live holder ⇒ crashed ⇒ mark
    // `'failed'`; held ⇒ a live process owns it ⇒ left untouched.
    //
    // Marking ONLY — this never touches the filesystem beyond the lockfiles
    // `is_crashed` itself opens/probes (and releases immediately). Deleting
    // any staging content here would mean opening the DB for a read (e.g.
    // `report copies`, `catalog ls`) could destroy staging data; actual
    // file cleanup for a `'failed'` set happens later, and only via
    // `staging::clean::clean_staging`.
    let staging_ids: Vec<i64> = {
        let mut stmt = conn.prepare("SELECT id FROM stage_sets WHERE status = 'staging'")?;
        let ids = stmt
            .query_map([], |row| row.get(0))?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        ids
    };

    let mut crashed = 0u64;
    for stage_set_id in staging_ids {
        if crate::staging::lock::is_crashed(db_path, stage_set_id) {
            conn.execute(
                "UPDATE stage_sets SET status = 'failed'
                 WHERE id = ?1 AND status = 'staging'",
                [stage_set_id],
            )?;
            crashed += 1;
        }
    }
    if crashed > 0 {
        warn!(
            count = crashed,
            "recovered orphaned staging sessions — marked as failed"
        );
        events::log_event(
            conn,
            "system",
            0,
            None,
            "crash_recovery",
            Some("stage_sets.status"),
            None,
            Some("failed"),
            Some(&format!("{crashed} sessions")),
            None,
        )?;
    }

    let mut updated = 0usize;
    for volume_id in in_progress_volumes(conn, "verification_sessions", "outcome")? {
        if !crate::staging::lock::volume_session_live(conn, volume_id) {
            updated += conn.execute(
                "UPDATE verification_sessions SET outcome = 'aborted'
                 WHERE volume_id = ?1 AND outcome = 'in_progress'",
                [volume_id],
            )?;
        }
    }
    if updated > 0 {
        warn!(
            count = updated,
            "recovered orphaned verification sessions — marked as aborted"
        );
        events::log_event(
            conn,
            "system",
            0,
            None,
            "crash_recovery",
            Some("verification_sessions.outcome"),
            None,
            Some("aborted"),
            Some(&format!("{updated} sessions")),
            None,
        )?;
    }
    Ok(())
}

/// The volumes with at least one `in_progress` row in `table` (`writes` by
/// `status`, `verification_sessions` by `outcome`) — the sweep's candidates.
/// A plain SELECT, so finding none needs no write lock (issue #377).
fn in_progress_volumes(conn: &Connection, table: &str, column: &str) -> Result<Vec<i64>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT DISTINCT volume_id FROM {table} WHERE {column} = 'in_progress'"
    ))?;
    let ids = stmt
        .query_map([], |row| row.get(0))?
        .collect::<std::result::Result<Vec<i64>, _>>()?;
    Ok(ids)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A file-backed catalog holding one volume with one `in_progress`
    /// write session and one `in_progress` verification session — the
    /// shape a live `volume write` is in during its confirm (issue #376).
    /// Returns `(db_path, volume_id)`; the connection is closed.
    fn live_session_catalog(tmp: &tempfile::TempDir) -> (std::path::PathBuf, i64) {
        let db_path = tmp.path().join("tapectl.db");
        let conn = open(&db_path).unwrap();
        conn.execute_batch(
            "INSERT INTO tenants (name) VALUES ('alpha');
             INSERT INTO units (uuid, name, tenant_id, checksum_mode, encrypt, status)
                 VALUES ('u-1', 'photos', 1, 'mtime_size', 1, 'active');
             INSERT INTO snapshots (unit_id, version, status, source_path, file_count, total_size)
                 VALUES (1, 1, 'staged', '/tmp/photos', 1, 10);
             INSERT INTO stage_sets (snapshot_id, status, slice_size) VALUES (1, 'staged', 524288);
             INSERT INTO volumes (label, backend_type, backend_name, media_type, capacity_bytes, status)
                 VALUES ('L6-0001', 'lto', 'lto0', 'LTO-6', 2500000000000, 'initialized');",
        )
        .unwrap();
        conn.execute(
            "INSERT INTO writes (stage_set_id, snapshot_id, volume_id, status)
             VALUES (1, 1, 1, 'in_progress')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO verification_sessions (volume_id, verify_type, outcome)
             VALUES (1, 'full', 'in_progress')",
            [],
        )
        .unwrap();
        (db_path, 1)
    }

    fn session_states(conn: &Connection) -> (String, String) {
        (
            conn.query_row("SELECT status FROM writes", [], |r| r.get(0))
                .unwrap(),
            conn.query_row("SELECT outcome FROM verification_sessions", [], |r| {
                r.get(0)
            })
            .unwrap(),
        )
    }

    /// Issue #376: with the volume's lock held (another open file
    /// description — the kernel treats it exactly as another process's),
    /// opening the catalog leaves the live session's rows alone. Released —
    /// the positive control — the same open sweeps both.
    #[test]
    fn open_sweeps_a_session_only_once_its_volume_lock_is_free() {
        let tmp = tempfile::TempDir::new().unwrap();
        let (db_path, volume_id) = live_session_catalog(&tmp);

        let holder_conn = Connection::open(&db_path).unwrap();
        let holder =
            crate::staging::lock::acquire_volume(&holder_conn, volume_id, "L6-0001").unwrap();
        let conn = open(&db_path).unwrap();
        assert_eq!(
            session_states(&conn),
            ("in_progress".to_string(), "in_progress".to_string()),
            "a live session must survive another command's open"
        );
        drop(conn);

        drop(holder);
        let conn = open(&db_path).unwrap();
        assert_eq!(
            session_states(&conn),
            ("interrupted".to_string(), "aborted".to_string()),
            "once the lock is free, the open sweeps the crashed session"
        );
    }

    /// Issue #377: opening the catalog needs no write lock when there is
    /// nothing to recover, so a read-only command opens and reads while
    /// another connection holds a write transaction. The holder's lock is
    /// real (the negative control: a write from the second connection
    /// fails busy), and the open finishes well inside its 5-second wait.
    #[test]
    fn open_succeeds_while_another_connection_holds_the_write_lock() {
        let tmp = tempfile::TempDir::new().unwrap();
        let db_path = tmp.path().join("tapectl.db");
        drop(open(&db_path).unwrap());

        let holder = Connection::open(&db_path).unwrap();
        holder.execute_batch("BEGIN IMMEDIATE").unwrap();

        let started = std::time::Instant::now();
        let conn = open(&db_path).expect("a read-only open must not need the write lock");
        assert!(
            started.elapsed() < std::time::Duration::from_secs(2),
            "open waited {:?} — something at open wanted the write lock",
            started.elapsed()
        );
        let _: i64 = conn
            .query_row("SELECT COUNT(*) FROM volumes", [], |r| r.get(0))
            .unwrap();

        conn.pragma_update(None, "busy_timeout", 50).unwrap();
        let write = conn.execute("INSERT INTO tenants (name) VALUES ('x')", []);
        assert!(
            matches!(&write, Err(e) if busy::is_busy(e)),
            "negative control: the holder must really hold the write lock: {write:?}"
        );
        holder.execute_batch("ROLLBACK").unwrap();
    }

    /// Issue #41: `db::open` used to `Connection::open(path)` with no mode
    /// of its own, leaving `tapectl.db` — which holds every filename, path,
    /// size, mtime, sha256, and tenant/unit name tapectl has recorded — at
    /// whatever the process umask handed out (0644 on a stock box). This is
    /// the one fix that reaches an *already-initialized* `~/.tapectl` too:
    /// `db::open` runs on every command invocation, unlike `ensure_dirs`
    /// (which was init-only until this same change wired it into the
    /// general dispatch path in `main.rs`).
    #[test]
    fn open_sets_db_file_mode_to_0600() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::TempDir::new().unwrap();
        let db_path = tmp.path().join("tapectl.db");

        let conn = open(&db_path).unwrap();
        drop(conn);

        let mode = std::fs::metadata(&db_path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "tapectl.db should be 0600, was {mode:o}");
    }

    /// Umask-independent companion to the above (see the analogous
    /// `config::tests::ensure_dirs_tightens_a_pre_existing_loose_directory`
    /// doc comment for why this shape is needed): a *re-opened*,
    /// pre-existing db file at a loose mode must be tightened too, not just
    /// a freshly-created one — `db::open` is called on every invocation
    /// against a file that (pre-fix) already exists from a prior run.
    #[test]
    fn open_tightens_a_pre_existing_loose_db_file() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::TempDir::new().unwrap();
        let db_path = tmp.path().join("tapectl.db");

        // Create it once, then loosen it, simulating a pre-fix database
        // left over from before this change.
        drop(open(&db_path).unwrap());
        std::fs::set_permissions(&db_path, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert_eq!(
            std::fs::metadata(&db_path).unwrap().permissions().mode() & 0o777,
            0o644,
            "fixture must start loose"
        );

        let conn = open(&db_path).unwrap();
        drop(conn);

        let mode = std::fs::metadata(&db_path).unwrap().permissions().mode() & 0o777;
        assert_eq!(
            mode, 0o600,
            "re-opening must tighten a loose db file to 0600"
        );
    }

    #[test]
    fn test_open_memory() {
        let conn = open_memory().unwrap();
        // Verify tables exist
        let count: i64 = conn
            .query_row(
                "SELECT count(*) FROM sqlite_master WHERE type='table' AND name='tenants'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(count, 1);
    }

    #[test]
    fn test_wal_mode() {
        let conn = open_memory().unwrap();
        let mode: String = conn
            .pragma_query_value(None, "journal_mode", |row| row.get(0))
            .unwrap();
        // In-memory databases use "memory" journal mode, but WAL was requested
        assert!(mode == "wal" || mode == "memory");
    }

    // --- Migration 003 (v2 lifecycle: sealed/quarantined + escrow) ---
    // Decision-sheet §3.6 (docs/design/v2-open-questions.md).

    /// Build a connection migrated to exactly the 002 schema (pre-003), the way a real
    /// database created before this migration existed would look. Uses the same `configure`
    /// the production `open()`/`open_memory()` path uses, so `PRAGMA foreign_keys` is really
    /// ON here too -- this is what makes the FK check in
    /// `test_migrate_002_populated_db_to_003_preserves_data_and_fk` a real discriminator
    /// rather than a vacuous pass.
    fn open_memory_at_002() -> Connection {
        let mut conn = Connection::open_in_memory().unwrap();
        configure(&conn).unwrap();
        Migrations::new(vec![
            M::up(include_str!("migrations/001_initial.sql")),
            M::up(include_str!("migrations/002_fts5_catalog.sql")),
        ])
        .to_latest(&mut conn)
        .unwrap();
        conn
    }

    /// Exactly the 003 schema — the reference point for "003 did not change
    /// columns". Must NOT be `open_memory()` (that is *latest*, which includes
    /// 004's deliberate `volumes.uuid` addition and every migration after).
    fn open_memory_at_003() -> Connection {
        let mut conn = Connection::open_in_memory().unwrap();
        configure(&conn).unwrap();
        Migrations::new(vec![
            M::up(include_str!("migrations/001_initial.sql")),
            M::up(include_str!("migrations/002_fts5_catalog.sql")),
            M::up(include_str!("migrations/003_v2_lifecycle.sql")).foreign_key_check(),
        ])
        .to_latest(&mut conn)
        .unwrap();
        conn
    }

    /// (name, type, notnull, dflt_value, pk) for every column, in declaration order.
    fn table_info(
        conn: &Connection,
        table: &str,
    ) -> Vec<(String, String, i64, Option<String>, i64)> {
        conn.prepare(&format!("PRAGMA table_info({table})"))
            .unwrap()
            .query_map([], |r| {
                Ok((
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, i64>(3)?,
                    r.get::<_, Option<String>>(4)?,
                    r.get::<_, i64>(5)?,
                ))
            })
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap()
    }

    /// (referenced table, from-column, to-column) for every outbound foreign
    /// key, sorted. `PRAGMA table_info` reports neither FK nor CHECK
    /// constraints, which is why this exists separately — see
    /// `test_migration_012_changes_no_cartridge_column`'s own note.
    fn foreign_keys_of(conn: &Connection, table: &str) -> Vec<(String, String, String)> {
        let mut fks: Vec<(String, String, String)> = conn
            .prepare(&format!("PRAGMA foreign_key_list({table})"))
            .unwrap()
            .query_map([], |r| {
                Ok((
                    r.get::<_, String>(2)?,
                    r.get::<_, String>(3)?,
                    r.get::<_, String>(4)?,
                ))
            })
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        fks.sort();
        fks
    }

    fn index_names(conn: &Connection, table: &str) -> Vec<String> {
        let mut names: Vec<String> = conn
            .prepare("SELECT name FROM sqlite_master WHERE type='index' AND tbl_name=?1")
            .unwrap()
            .query_map([table], |r| r.get(0))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        names.sort();
        names
    }

    /// (a) A fresh DB migrates cleanly to latest, and encryption_keys gained is_escrow
    /// (default 0); 003's extended CHECK carries every legacy status plus the two new ones.
    /// The CHECK half reads the 003 schema itself, not latest: 017 moved 'quarantined'
    /// out of the column and 026 dropped 'blank'/'missing' (issue #362), so a "latest"
    /// read here would be pinning whatever the newest rebuild happens to say.
    #[test]
    fn test_migration_003_fresh_db_reaches_latest() {
        let conn = open_memory().unwrap();

        let sql: String = open_memory_at_003()
            .query_row(
                "SELECT sql FROM sqlite_master WHERE type='table' AND name='volumes'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        for status in [
            "'blank'",
            "'initialized'",
            "'active'",
            "'full'",
            "'retired'",
            "'missing'",
            "'erased'",
            "'sealed'",
            "'quarantined'",
        ] {
            assert!(
                sql.contains(status),
                "volumes CHECK missing {status}: {sql}"
            );
        }

        let cols = table_info(&conn, "encryption_keys");
        let is_escrow = cols
            .iter()
            .find(|(name, ..)| name == "is_escrow")
            .unwrap_or_else(|| panic!("encryption_keys.is_escrow column missing: {cols:?}"));
        assert_eq!(is_escrow.2, 1, "is_escrow must be NOT NULL");
        assert_eq!(
            is_escrow.3.as_deref(),
            Some("0"),
            "is_escrow must default to 0"
        );
    }

    /// Bulletproof self-check: the rebuilt `volumes` table is column-for-column,
    /// default-for-default, index-for-index identical to the 002 schema except the two
    /// added CHECK values (which PRAGMA table_info can't see, since CHECK isn't part of a
    /// column's structural identity -- exercised separately by the insert-based tests below).
    #[test]
    fn test_migration_003_volumes_columns_and_indexes_unchanged() {
        let conn_002 = open_memory_at_002();
        let cols_002 = table_info(&conn_002, "volumes");
        let idx_002 = index_names(&conn_002, "volumes");

        // Compare against 003 specifically, not `open_memory()` (= latest):
        // migration 004 deliberately ADDS `volumes.uuid`, so latest is the
        // wrong reference point for a "003 changed nothing" assertion.
        let conn_003 = open_memory_at_003();
        let cols_003 = table_info(&conn_003, "volumes");
        let idx_003 = index_names(&conn_003, "volumes");

        assert_eq!(
            cols_002, cols_003,
            "volumes columns/defaults/notnull/pk changed by migration 003"
        );
        assert_eq!(idx_002, idx_003);
        // Two explicit indexes plus the implicit UNIQUE(label) autoindex.
        assert_eq!(
            idx_002,
            vec![
                "idx_volumes_location",
                "idx_volumes_status",
                "sqlite_autoindex_volumes_1",
            ]
        );
    }

    /// (b) + (d): a DB populated at 002-level -- with an 'active' volume row and a row in
    /// every table that FK-references volumes(id) (cartridge_volumes, volume_movements,
    /// writes, verification_sessions, health_logs; five in total per the §3.6 recon) --
    /// migrates cleanly through the real `migrate()` (exercising the actual FK on/off
    /// wrapping), `PRAGMA foreign_key_check` comes back empty, every row is intact, the
    /// row's status is still readable, and `db_fsck` is clean. ('active', not the 'full'
    /// this test seeded until migration 032 dropped it: a row in a dropped state stops
    /// the chain at the migration that drops it, by design.)
    #[test]
    fn test_migrate_002_populated_db_to_003_preserves_data_and_fk() {
        let mut conn = open_memory_at_002();

        conn.execute(
            "INSERT INTO volumes (label, backend_type, backend_name, media_type, capacity_bytes, status)
             VALUES ('V-LEGACY', 'lto', 'lto0', 'LTO-6', 2500000000000, 'active')",
            [],
        )
        .unwrap();
        let vol_id = conn.last_insert_rowid();

        conn.execute(
            "INSERT INTO tenants (name, description, is_operator, status) VALUES ('t1', '', 0, 'active')",
            [],
        )
        .unwrap();
        let tenant_id = conn.last_insert_rowid();
        conn.execute(
            "INSERT INTO units (tenant_id, uuid, name, current_path, status)
             VALUES (?1, 'uuid-u1', 'u1', '/tmp/u1', 'active')",
            [tenant_id],
        )
        .unwrap();
        let unit_id = conn.last_insert_rowid();
        conn.execute(
            "INSERT INTO snapshots (unit_id, version, status, source_path, file_count, total_size)
             VALUES (?1, 1, 'created', '/tmp/u1', 0, 0)",
            [unit_id],
        )
        .unwrap();
        let snap_id = conn.last_insert_rowid();
        conn.execute(
            "INSERT INTO stage_sets (snapshot_id, status, slice_size) VALUES (?1, 'staged', 524288)",
            [snap_id],
        )
        .unwrap();
        let stage_set_id = conn.last_insert_rowid();
        conn.execute(
            "INSERT INTO writes (stage_set_id, snapshot_id, volume_id, status)
             VALUES (?1, ?2, ?3, 'completed')",
            [stage_set_id, snap_id, vol_id],
        )
        .unwrap();
        let write_id = conn.last_insert_rowid();

        conn.execute(
            "INSERT INTO verification_sessions (volume_id, verify_type, outcome)
             VALUES (?1, 'full', 'passed')",
            [vol_id],
        )
        .unwrap();

        conn.execute(
            "INSERT INTO cartridges (barcode, media_type, nominal_capacity, status)
             VALUES ('BC-1', 'LTO-6', 2500000000000, 'in_use')",
            [],
        )
        .unwrap();
        let cart_id = conn.last_insert_rowid();
        conn.execute(
            "INSERT INTO cartridge_volumes (cartridge_id, volume_id) VALUES (?1, ?2)",
            [cart_id, vol_id],
        )
        .unwrap();

        conn.execute("INSERT INTO locations (name) VALUES ('loc1')", [])
            .unwrap();
        let loc_id = conn.last_insert_rowid();
        conn.execute(
            "INSERT INTO volume_movements (volume_id, to_location) VALUES (?1, ?2)",
            [vol_id, loc_id],
        )
        .unwrap();

        conn.execute(
            "INSERT INTO health_logs (volume_id, operation) VALUES (?1, 'write')",
            [vol_id],
        )
        .unwrap();

        // Exercise the real production migrate() -- the actual FK off/on wrapping this
        // migration depends on -- not a hand-rolled call to Migrations::to_latest().
        migrate(&mut conn).unwrap();

        let fk_violations: i64 = conn
            .query_row("SELECT COUNT(*) FROM pragma_foreign_key_check", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(
            fk_violations, 0,
            "PRAGMA foreign_key_check found violations"
        );

        let (label, status): (String, String) = conn
            .query_row(
                "SELECT label, status FROM volumes WHERE id = ?1",
                [vol_id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(label, "V-LEGACY");
        assert_eq!(
            status, "active",
            "(d) the pre-003 row's status must still be readable"
        );

        let writes_vol: i64 = conn
            .query_row(
                "SELECT volume_id FROM writes WHERE id = ?1",
                [write_id],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(writes_vol, vol_id);

        let vs_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM verification_sessions WHERE volume_id = ?1",
                [vol_id],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(vs_count, 1);

        let cv_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM cartridge_volumes WHERE volume_id = ?1",
                [vol_id],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(cv_count, 1);

        let vm_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM volume_movements WHERE volume_id = ?1",
                [vol_id],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(vm_count, 1);

        let hl_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM health_logs WHERE volume_id = ?1",
                [vol_id],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(hl_count, 1);

        let report = crate::cli::operations::db_fsck(&conn, false, false).unwrap();
        assert!(report.integrity_ok, "db fsck integrity check failed");
        assert!(
            report.issues.is_empty(),
            "db fsck found issues: {:?}",
            report.issues
        );
    }

    /// (c) The two new statuses are insertable after migrating.
    #[test]
    fn test_migration_003_new_statuses_insertable() {
        let conn = open_memory().unwrap();
        // 'sealed' is still legal against the LIVE schema. 'quarantined' is
        // deliberately excluded from the insertable set here (issue #242,
        // migration 017): it left `status`'s CHECK entirely and is legal
        // now only as a value of `observed_condition` — see
        // `test_migration_017_observed_condition_is_closed_and_defaults_ok`.
        conn.execute(
            "INSERT INTO volumes (label, backend_type, backend_name, capacity_bytes, status)
             VALUES ('V-sealed', 'lto', 'lto0', 2500000000000, 'sealed')",
            [],
        )
        .unwrap_or_else(|e| panic!("status 'sealed' should be insertable: {e}"));

        let err = conn.execute(
            "INSERT INTO volumes (label, backend_type, backend_name, capacity_bytes, status)
             VALUES ('V-quarantined', 'lto', 'lto0', 2500000000000, 'quarantined')",
            [],
        );
        assert!(
            err.is_err(),
            "'quarantined' is a condition now (issue #242), not a status -- the live \
             schema must reject it as a status value"
        );
    }

    /// The CHECK constraint still rejects unknown values -- proof it wasn't dropped or
    /// widened into a no-op while extending it.
    #[test]
    fn test_migration_003_invalid_status_still_rejected() {
        let conn = open_memory().unwrap();
        let err = conn.execute(
            "INSERT INTO volumes (label, backend_type, backend_name, capacity_bytes, status)
             VALUES ('V-bad', 'lto', 'lto0', 2500000000000, 'not_a_real_status')",
            [],
        );
        assert!(
            err.is_err(),
            "CHECK constraint should reject unknown status values"
        );
    }

    // --- Migration 012 (ADR-0011: the four-state cartridge lifecycle) ---

    /// A connection migrated to exactly the 011 schema — the last point at
    /// which `cartridges.status = 'offsite'` is still a legal value, so a row
    /// carrying it can actually be seeded and then watched through the
    /// rebuild. Mirrors `open_memory_at_002`, including `configure()`, so
    /// `PRAGMA foreign_keys` is genuinely ON for the seeding below and the FK
    /// assertions are real discriminators.
    fn open_memory_at_011() -> Connection {
        let mut conn = Connection::open_in_memory().unwrap();
        configure(&conn).unwrap();
        let mut ms = vec![
            M::up(include_str!("migrations/001_initial.sql")),
            M::up(include_str!("migrations/002_fts5_catalog.sql")),
            M::up(include_str!("migrations/003_v2_lifecycle.sql")).foreign_key_check(),
            M::up(include_str!("migrations/004_volume_uuid.sql")),
            M::up(include_str!("migrations/005_file_types.sql")),
            M::up(include_str!("migrations/006_write_session_dir.sql")),
            M::up(include_str!("migrations/007_warehouse_locations.sql")),
            M::up(include_str!("migrations/008_drop_volume_storage_class.sql")),
            M::up(include_str!("migrations/009_health_tape_alerts.sql")),
            M::up(include_str!("migrations/010_stage_set_origin.sql")),
            M::up(include_str!("migrations/011_cartridge_serial_index.sql")),
        ];
        // 003 drops `volumes` while five tables reference it; same reason as
        // production `migrate()`.
        conn.pragma_update(None, "foreign_keys", "OFF").unwrap();
        Migrations::new(std::mem::take(&mut ms))
            .to_latest(&mut conn)
            .unwrap();
        conn.pragma_update(None, "foreign_keys", "ON").unwrap();
        conn
    }

    /// THE test for this migration, and the one the 12-step procedure exists
    /// to make pass: a populated 011 database — including a cartridge bound to
    /// a volume through `cartridge_volumes`, the one table holding a
    /// `REFERENCES cartridges(id)` foreign key — survives the rebuild with its
    /// row IDS INTACT.
    ///
    /// The join assertion is the real discriminator. An `INSERT INTO
    /// cartridges_new SELECT ...` that omitted `id` would renumber every
    /// cartridge, and `PRAGMA foreign_key_check` alone would NOT necessarily
    /// catch it — with one cartridge, rowid 1 is handed straight back, and the
    /// FK still resolves while pointing at what is, in general, a different
    /// cartridge. So three rows are seeded and the join is checked by barcode.
    #[test]
    fn test_migrate_011_populated_db_to_012_preserves_ids_and_fk() {
        let mut conn = open_memory_at_011();

        conn.execute("INSERT INTO locations (name) VALUES ('home-rack')", [])
            .unwrap();
        let loc_id = conn.last_insert_rowid();

        // Three cartridges so a renumbering cannot coincidentally land every
        // row back on its own id, and one of them carries the doomed
        // 'offsite' status — legal at 011, gone at 012.
        for (bc, status) in [
            ("BC-1", "in_use"),
            ("BC-2", "offsite"),
            ("BC-3", "pending_erase"),
        ] {
            conn.execute(
                "INSERT INTO cartridges (barcode, media_type, nominal_capacity, status, location_id)
                 VALUES (?1, 'LTO-6', 2500000000000, ?2, ?3)",
                rusqlite::params![bc, status, loc_id],
            )
            .unwrap();
        }
        let bc3_id: i64 = conn
            .query_row(
                "SELECT id FROM cartridges WHERE barcode = 'BC-3'",
                [],
                |r| r.get(0),
            )
            .unwrap();

        // Bind the LAST cartridge, so an off-by-one renumbering shows up.
        conn.execute(
            "INSERT INTO volumes (label, backend_type, backend_name, media_type, capacity_bytes, status)
             VALUES ('L6-0001', 'lto', 'lto0', 'LTO-6', 2500000000000, 'sealed')",
            [],
        )
        .unwrap();
        let vol_id = conn.last_insert_rowid();
        conn.execute(
            "INSERT INTO cartridge_volumes (cartridge_id, volume_id) VALUES (?1, ?2)",
            [bc3_id, vol_id],
        )
        .unwrap();

        // The real production migrate(), with its real FK off/on wrapping.
        migrate(&mut conn).unwrap();

        let fk_violations: i64 = conn
            .query_row("SELECT COUNT(*) FROM pragma_foreign_key_check", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(
            fk_violations, 0,
            "PRAGMA foreign_key_check found violations after the cartridges rebuild"
        );

        // The join still names the SAME cartridge — proof the ids survived,
        // not merely that they still resolve to something.
        let joined: String = conn
            .query_row(
                "SELECT c.barcode FROM cartridge_volumes cv
                 JOIN cartridges c ON c.id = cv.cartridge_id
                 WHERE cv.volume_id = ?1",
                [vol_id],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            joined, "BC-3",
            "the rebuild renumbered cartridges and silently re-pointed the join"
        );

        // Every row survived, statuses mapped, location preserved.
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM cartridges", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 3);
        let statuses: Vec<(String, String, Option<i64>)> = conn
            .prepare("SELECT barcode, status, location_id FROM cartridges ORDER BY barcode")
            .unwrap()
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        assert_eq!(
            statuses,
            vec![
                ("BC-1".to_string(), "in_use".to_string(), Some(loc_id)),
                // ADR-0011: 'offsite' is not a status; it maps to 'available'
                // and the place is recorded by location_id, which is retained.
                ("BC-2".to_string(), "available".to_string(), Some(loc_id)),
                (
                    "BC-3".to_string(),
                    "pending_erase".to_string(),
                    Some(loc_id)
                ),
            ]
        );

        let report = crate::cli::operations::db_fsck(&conn, false, false).unwrap();
        assert!(report.integrity_ok, "db fsck integrity check failed");
    }

    // --- Migration 013 (issue #149: the two always-zero manifest flags) ---

    /// A connection migrated to exactly the 012 schema — the last point at
    /// which `manifest_entries.has_xattrs` / `.has_acls` still exist, so rows
    /// carrying them can be seeded and watched through the rebuild.
    fn open_memory_at_012() -> Connection {
        let mut conn = Connection::open_in_memory().unwrap();
        configure(&conn).unwrap();
        let mut ms = vec![
            M::up(include_str!("migrations/001_initial.sql")),
            M::up(include_str!("migrations/002_fts5_catalog.sql")),
            M::up(include_str!("migrations/003_v2_lifecycle.sql")).foreign_key_check(),
            M::up(include_str!("migrations/004_volume_uuid.sql")),
            M::up(include_str!("migrations/005_file_types.sql")),
            M::up(include_str!("migrations/006_write_session_dir.sql")),
            M::up(include_str!("migrations/007_warehouse_locations.sql")),
            M::up(include_str!("migrations/008_drop_volume_storage_class.sql")),
            M::up(include_str!("migrations/009_health_tape_alerts.sql")),
            M::up(include_str!("migrations/010_stage_set_origin.sql")),
            M::up(include_str!("migrations/011_cartridge_serial_index.sql")),
            M::up(include_str!("migrations/012_cartridge_lifecycle.sql")).foreign_key_check(),
        ];
        conn.pragma_update(None, "foreign_keys", "OFF").unwrap();
        Migrations::new(std::mem::take(&mut ms))
            .to_latest(&mut conn)
            .unwrap();
        conn.pragma_update(None, "foreign_keys", "ON").unwrap();
        conn
    }

    /// THE test for 013: a populated 012 database survives the rebuild with
    /// its row IDs and its `manifest_id` parentage intact, and every column
    /// 005 appended (`file_type`, `link_target`) still carries its value.
    ///
    /// Three entries are seeded, and the assertions read the LAST one, so an
    /// off-by-one renumbering cannot coincidentally pass. `file_type` is the
    /// discriminator for the column-ORDER trap specific to this migration:
    /// 005 added it with `ALTER TABLE ... ADD COLUMN`, so it sits AFTER
    /// `has_acls` in the live table. A rebuild that listed the new columns in
    /// 001's order and copied with a bare positional SELECT would shift
    /// `file_type` into `groupname` and lose it, and no row count or FK check
    /// would notice.
    #[test]
    fn test_migrate_012_populated_db_to_013_preserves_ids_and_columns() {
        let mut conn = open_memory_at_012();

        conn.execute(
            "INSERT INTO tenants (name, is_operator, status) VALUES ('t', 0, 'active')",
            [],
        )
        .unwrap();
        let tenant_id = conn.last_insert_rowid();
        conn.execute(
            "INSERT INTO units (uuid, name, tenant_id, current_path, status)
             VALUES ('u-uuid', 'u', ?1, '/tmp/u', 'active')",
            [tenant_id],
        )
        .unwrap();
        let unit_id = conn.last_insert_rowid();
        conn.execute(
            "INSERT INTO snapshots (unit_id, version, source_path, file_count, total_size)
             VALUES (?1, 1, '/tmp/u', 3, 30)",
            [unit_id],
        )
        .unwrap();
        let snapshot_id = conn.last_insert_rowid();
        conn.execute(
            "INSERT INTO manifests (snapshot_id) VALUES (?1)",
            [snapshot_id],
        )
        .unwrap();
        let manifest_id = conn.last_insert_rowid();

        for (path, ft, target) in [
            ("a.txt", "regular", None::<&str>),
            ("b", "dir", None),
            ("c.lnk", "symlink", Some("../a.txt")),
        ] {
            conn.execute(
                "INSERT INTO manifest_entries
                    (manifest_id, path, size_bytes, mtime, is_directory, mode, uid, gid,
                     username, groupname, has_xattrs, has_acls, file_type, link_target)
                 VALUES (?1, ?2, 10, '2026-01-01T00:00:00Z', 0, 420, 1000, 1000,
                         'mike', 'mike', 0, 0, ?3, ?4)",
                rusqlite::params![manifest_id, path, ft, target],
            )
            .unwrap();
        }
        let last_id: i64 = conn
            .query_row(
                "SELECT id FROM manifest_entries WHERE path = 'c.lnk'",
                [],
                |r| r.get(0),
            )
            .unwrap();

        migrate_to(&mut conn, Some(13)).unwrap();

        let fk_violations: i64 = conn
            .query_row("SELECT COUNT(*) FROM pragma_foreign_key_check", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(fk_violations, 0);

        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM manifest_entries", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 3, "the rebuild lost rows");

        let (id, parent, path, ft, target, groupname): (
            i64,
            i64,
            String,
            String,
            Option<String>,
            Option<String>,
        ) = conn
            .query_row(
                "SELECT id, manifest_id, path, file_type, link_target, groupname
                 FROM manifest_entries WHERE path = 'c.lnk'",
                [],
                |r| {
                    Ok((
                        r.get(0)?,
                        r.get(1)?,
                        r.get(2)?,
                        r.get(3)?,
                        r.get(4)?,
                        r.get(5)?,
                    ))
                },
            )
            .unwrap();
        assert_eq!(id, last_id, "the rebuild renumbered manifest_entries");
        assert_eq!(parent, manifest_id);
        assert_eq!(path, "c.lnk");
        assert_eq!(ft, "symlink", "005's appended columns shifted in the copy");
        assert_eq!(target.as_deref(), Some("../a.txt"));
        assert_eq!(groupname.as_deref(), Some("mike"));

        let report = crate::cli::operations::db_fsck(&conn, false, false).unwrap();
        assert!(report.integrity_ok, "db fsck integrity check failed");
    }

    /// The columns are actually GONE, not merely unwritten — the whole point
    /// of the migration. A `SELECT` naming either must now fail.
    #[test]
    fn test_migration_013_removes_the_two_flag_columns() {
        let conn = open_memory_at_version(13);
        let names: Vec<String> = table_info(&conn, "manifest_entries")
            .into_iter()
            .map(|c| c.0)
            .collect();
        assert!(!names.iter().any(|n| n == "has_xattrs"), "{names:?}");
        assert!(!names.iter().any(|n| n == "has_acls"), "{names:?}");
        assert!(
            conn.query_row("SELECT has_xattrs FROM manifest_entries", [], |r| r
                .get::<_, i64>(0))
                .is_err(),
            "has_xattrs still resolves"
        );
    }

    /// The only index the table carried is back. A rebuild that forgot it
    /// would turn every manifest lookup into a full scan of the largest
    /// table in the schema, and nothing else in the suite would notice.
    #[test]
    fn test_migration_013_recreates_the_manifest_index() {
        let conn = open_memory_at_version(13);
        assert_eq!(
            index_names(&conn, "manifest_entries"),
            vec!["idx_manifest_entries_manifest"]
        );
    }

    /// 012 changes the status CHECK and nothing else: every column, type,
    /// default, notnull and pk is identical either side of it.
    ///
    /// Note what this does NOT prove: `PRAGMA table_info` does not report
    /// CHECK constraints, so this test would pass even if the rebuild had
    /// dropped the CHECK entirely. That half is
    /// `test_migration_012_offsite_rejected_four_states_accepted`'s job,
    /// and it is written with a negative assertion for exactly this reason.
    #[test]
    fn test_migration_012_changes_no_cartridge_column() {
        let before = open_memory_at_011();
        let cols_011 = table_info(&before, "cartridges");

        // Exactly the 012 schema, NOT `open_memory()` (latest): migration
        // 016 (ADR-0012 amendment, 2026-09-16; issue #197) adds
        // `cartridges.operator_serial`, a legitimate later change this test
        // must not see -- it exists to prove 012's REBUILD didn't alter any
        // column that already existed in 011, nothing about the schema's
        // current, total shape.
        let after = open_memory_at_012();
        let cols_012 = table_info(&after, "cartridges");

        assert_eq!(
            cols_011, cols_012,
            "cartridges columns/defaults/notnull/pk changed by migration 012"
        );
    }

    /// Every index the old table carried is back, plus the new location one.
    /// A rebuild that forgot 011's partial unique index would silently
    /// re-admit the duplicate medium serials it exists to prevent, and no
    /// column-shape assertion would notice.
    #[test]
    fn test_migration_012_recreates_every_index_and_adds_location() {
        let conn = open_memory_at_version(12);
        assert_eq!(
            index_names(&conn, "cartridges"),
            vec![
                "idx_cartridges_barcode",
                "idx_cartridges_location",
                "idx_cartridges_serial_number",
                "idx_cartridges_status",
                // the implicit UNIQUE(barcode) autoindex
                "sqlite_autoindex_cartridges_1",
            ]
        );

        // And 011's index still ENFORCES, not merely exists.
        conn.execute(
            "INSERT INTO cartridges (barcode, media_type, nominal_capacity, serial_number)
             VALUES ('BC-1', 'LTO-6', 2500000000000, 'SERIAL1')",
            [],
        )
        .unwrap();
        let dup = conn.execute(
            "INSERT INTO cartridges (barcode, media_type, nominal_capacity, serial_number)
             VALUES ('BC-2', 'LTO-6', 2500000000000, 'SERIAL1')",
            [],
        );
        assert!(
            dup.is_err(),
            "the partial unique index on serial_number must survive the rebuild"
        );
        // ...and stay PARTIAL: two NULL serials are still fine.
        for bc in ["BC-3", "BC-4"] {
            conn.execute(
                "INSERT INTO cartridges (barcode, media_type, nominal_capacity)
                 VALUES (?1, 'LTO-6', 2500000000000)",
                rusqlite::params![bc],
            )
            .unwrap();
        }
    }

    /// 'offsite' is gone from the CHECK and the other four still work. The
    /// negative half matters most: a rebuild that dropped the CHECK entirely
    /// would pass every other assertion in this file.
    #[test]
    fn test_migration_012_offsite_rejected_four_states_accepted() {
        let conn = open_memory().unwrap();
        for status in ["available", "in_use", "pending_erase", "retired_permanent"] {
            conn.execute(
                "INSERT INTO cartridges (barcode, media_type, nominal_capacity, status)
                 VALUES (?1, 'LTO-6', 2500000000000, ?2)",
                rusqlite::params![format!("BC-{status}"), status],
            )
            .unwrap_or_else(|e| panic!("status '{status}' should be insertable: {e}"));
        }
        let err = conn.execute(
            "INSERT INTO cartridges (barcode, media_type, nominal_capacity, status)
             VALUES ('BC-offsite', 'LTO-6', 2500000000000, 'offsite')",
            [],
        );
        assert!(
            err.is_err(),
            "'offsite' is a location, not a status (ADR-0011) — the CHECK must reject it"
        );
    }

    /// The last unpinned edge of 012's rebuild (issue #227).
    ///
    /// A create/copy/drop/rename rebuild silently drops whatever the new
    /// table's DDL forgets to restate, and `PRAGMA table_info` reports
    /// neither FK nor CHECK constraints — so
    /// `test_migration_012_changes_no_cartridge_column` would pass just as
    /// happily if the rebuild had dropped `location_id REFERENCES
    /// locations(id)`. The CHECK half of that blind spot is covered by
    /// `test_migration_012_offsite_rejected_four_states_accepted`; this is
    /// the FK half.
    ///
    /// It matters under ADR-0011 specifically: a cartridge's PLACE is a
    /// location, so `location_id` is the column that ADR made load-bearing,
    /// and an unenforced reference is how a cartridge ends up pointing at a
    /// location that no longer exists.
    ///
    /// Two assertions, and the second is the one that would actually catch a
    /// dropped constraint: the enumeration proves the edge is declared, the
    /// insert proves it is ENFORCED.
    #[test]
    fn test_migration_012_preserves_the_cartridge_location_foreign_key() {
        let before = open_memory_at_011();
        let after = open_memory().unwrap();

        let expected = vec![(
            "locations".to_string(),
            "location_id".to_string(),
            "id".to_string(),
        )];
        assert_eq!(
            foreign_keys_of(&before, "cartridges"),
            expected,
            "precondition: 011 declares exactly the one outbound FK"
        );
        assert_eq!(
            foreign_keys_of(&after, "cartridges"),
            expected,
            "012's rebuild must restate `location_id REFERENCES locations(id)` — \
             a rebuild drops any constraint its new DDL omits"
        );

        let err = after.execute(
            "INSERT INTO cartridges (barcode, media_type, nominal_capacity, status, location_id)
             VALUES ('BC-dangling', 'LTO-6', 2500000000000, 'available', 99999)",
            [],
        );
        assert!(
            err.is_err(),
            "the FK must be ENFORCED after the rebuild, not merely declared: \
             a cartridge cannot sit at a location that does not exist (ADR-0011)"
        );
    }

    // --- Migration 017 (ADR-0012's 2026-09-17 amendment: `observed_condition`) ---

    /// A connection migrated to exactly the 016 schema — the last point at
    /// which `volumes.status = 'quarantined'` is still legal and
    /// `observed_condition` does not exist yet, so rows carrying the old
    /// shape can be seeded and watched through the rebuild.
    fn open_memory_at_016() -> Connection {
        let mut conn = Connection::open_in_memory().unwrap();
        configure(&conn).unwrap();
        let mut ms = vec![
            M::up(include_str!("migrations/001_initial.sql")),
            M::up(include_str!("migrations/002_fts5_catalog.sql")),
            M::up(include_str!("migrations/003_v2_lifecycle.sql")).foreign_key_check(),
            M::up(include_str!("migrations/004_volume_uuid.sql")),
            M::up(include_str!("migrations/005_file_types.sql")),
            M::up(include_str!("migrations/006_write_session_dir.sql")),
            M::up(include_str!("migrations/007_warehouse_locations.sql")),
            M::up(include_str!("migrations/008_drop_volume_storage_class.sql")),
            M::up(include_str!("migrations/009_health_tape_alerts.sql")),
            M::up(include_str!("migrations/010_stage_set_origin.sql")),
            M::up(include_str!("migrations/011_cartridge_serial_index.sql")),
            M::up(include_str!("migrations/012_cartridge_lifecycle.sql")).foreign_key_check(),
            M::up(include_str!("migrations/013_drop_manifest_entry_flags.sql")).foreign_key_check(),
            M::up(include_str!(
                "migrations/014_cartridge_binding_identity_source.sql"
            )),
            M::up(include_str!(
                "migrations/015_cartridge_load_count_unknown.sql"
            )),
            M::up(include_str!("migrations/016_cartridge_operator_serial.sql")),
        ];
        conn.pragma_update(None, "foreign_keys", "OFF").unwrap();
        Migrations::new(std::mem::take(&mut ms))
            .to_latest(&mut conn)
            .unwrap();
        conn.pragma_update(None, "foreign_keys", "ON").unwrap();
        conn
    }

    /// A connection migrated to exactly the 017 schema — the last point
    /// before migration 018 (issue #277) adds `volumes.sealed_at`. Needed so
    /// `test_migration_017_changes_no_volume_column` keeps pinning "017
    /// changed nothing but `observed_condition`" on its own terms, rather
    /// than against whatever migration happens to be latest — the same
    /// reason `open_memory_at_016` exists rather than reusing `open_memory()`
    /// for the 016 side of that comparison.
    fn open_memory_at_017() -> Connection {
        let mut conn = Connection::open_in_memory().unwrap();
        configure(&conn).unwrap();
        let mut ms = vec![
            M::up(include_str!("migrations/001_initial.sql")),
            M::up(include_str!("migrations/002_fts5_catalog.sql")),
            M::up(include_str!("migrations/003_v2_lifecycle.sql")).foreign_key_check(),
            M::up(include_str!("migrations/004_volume_uuid.sql")),
            M::up(include_str!("migrations/005_file_types.sql")),
            M::up(include_str!("migrations/006_write_session_dir.sql")),
            M::up(include_str!("migrations/007_warehouse_locations.sql")),
            M::up(include_str!("migrations/008_drop_volume_storage_class.sql")),
            M::up(include_str!("migrations/009_health_tape_alerts.sql")),
            M::up(include_str!("migrations/010_stage_set_origin.sql")),
            M::up(include_str!("migrations/011_cartridge_serial_index.sql")),
            M::up(include_str!("migrations/012_cartridge_lifecycle.sql")).foreign_key_check(),
            M::up(include_str!("migrations/013_drop_manifest_entry_flags.sql")).foreign_key_check(),
            M::up(include_str!(
                "migrations/014_cartridge_binding_identity_source.sql"
            )),
            M::up(include_str!(
                "migrations/015_cartridge_load_count_unknown.sql"
            )),
            M::up(include_str!("migrations/016_cartridge_operator_serial.sql")),
            M::up(include_str!("migrations/017_volume_observed_condition.sql")).foreign_key_check(),
        ];
        conn.pragma_update(None, "foreign_keys", "OFF").unwrap();
        Migrations::new(std::mem::take(&mut ms))
            .to_latest(&mut conn)
            .unwrap();
        conn.pragma_update(None, "foreign_keys", "ON").unwrap();
        conn
    }

    /// A connection migrated to exactly the 020 schema — the last point
    /// before migration 021 (the second pass of issue #296) performs
    /// `health_logs`' one permitted rebuild (ADR-0013 §3).
    ///
    /// Exists for the same reason `open_memory_at_016`/`open_memory_at_017`
    /// do: the "this migration changed nothing else" pins below must hold on
    /// their own terms rather than against whatever migration happens to be
    /// latest. It was equivalent to `open_memory()` for exactly as long as
    /// 020 was the newest migration; 021 has now landed, which is precisely
    /// when it earns its keep — 021 is irreversible, and its verification
    /// standard (#227/#264) needs a before-picture that cannot drift. Every
    /// 021 test below takes its "before" from here.
    fn open_memory_at_020() -> Connection {
        let mut conn = Connection::open_in_memory().unwrap();
        configure(&conn).unwrap();
        let mut ms = vec![
            M::up(include_str!("migrations/001_initial.sql")),
            M::up(include_str!("migrations/002_fts5_catalog.sql")),
            M::up(include_str!("migrations/003_v2_lifecycle.sql")).foreign_key_check(),
            M::up(include_str!("migrations/004_volume_uuid.sql")),
            M::up(include_str!("migrations/005_file_types.sql")),
            M::up(include_str!("migrations/006_write_session_dir.sql")),
            M::up(include_str!("migrations/007_warehouse_locations.sql")),
            M::up(include_str!("migrations/008_drop_volume_storage_class.sql")),
            M::up(include_str!("migrations/009_health_tape_alerts.sql")),
            M::up(include_str!("migrations/010_stage_set_origin.sql")),
            M::up(include_str!("migrations/011_cartridge_serial_index.sql")),
            M::up(include_str!("migrations/012_cartridge_lifecycle.sql")).foreign_key_check(),
            M::up(include_str!("migrations/013_drop_manifest_entry_flags.sql")).foreign_key_check(),
            M::up(include_str!(
                "migrations/014_cartridge_binding_identity_source.sql"
            )),
            M::up(include_str!(
                "migrations/015_cartridge_load_count_unknown.sql"
            )),
            M::up(include_str!("migrations/016_cartridge_operator_serial.sql")),
            M::up(include_str!("migrations/017_volume_observed_condition.sql")).foreign_key_check(),
            M::up(include_str!("migrations/018_volume_sealed_at.sql")),
            M::up(include_str!("migrations/019_drives.sql")),
            M::up(include_str!("migrations/020_cartridge_contacts.sql")),
        ];
        conn.pragma_update(None, "foreign_keys", "OFF").unwrap();
        Migrations::new(std::mem::take(&mut ms))
            .to_latest(&mut conn)
            .unwrap();
        conn.pragma_update(None, "foreign_keys", "ON").unwrap();
        conn
    }

    /// A connection migrated to exactly the 021 schema — the after-picture
    /// for the `health_logs` rebuild's four-part verification below, and the
    /// before-picture whatever migration comes next will need.
    ///
    /// Today it is equivalent to `open_memory()`. It exists anyway, for the
    /// reason `open_memory_at_016`/`open_memory_at_017`/`open_memory_at_020`
    /// each earned in turn: the moment a later migration legitimately touches
    /// this table, a comparison written against "latest" starts silently
    /// measuring that migration instead of this one.
    fn open_memory_at_021() -> Connection {
        let mut conn = Connection::open_in_memory().unwrap();
        configure(&conn).unwrap();
        let mut ms = vec![
            M::up(include_str!("migrations/001_initial.sql")),
            M::up(include_str!("migrations/002_fts5_catalog.sql")),
            M::up(include_str!("migrations/003_v2_lifecycle.sql")).foreign_key_check(),
            M::up(include_str!("migrations/004_volume_uuid.sql")),
            M::up(include_str!("migrations/005_file_types.sql")),
            M::up(include_str!("migrations/006_write_session_dir.sql")),
            M::up(include_str!("migrations/007_warehouse_locations.sql")),
            M::up(include_str!("migrations/008_drop_volume_storage_class.sql")),
            M::up(include_str!("migrations/009_health_tape_alerts.sql")),
            M::up(include_str!("migrations/010_stage_set_origin.sql")),
            M::up(include_str!("migrations/011_cartridge_serial_index.sql")),
            M::up(include_str!("migrations/012_cartridge_lifecycle.sql")).foreign_key_check(),
            M::up(include_str!("migrations/013_drop_manifest_entry_flags.sql")).foreign_key_check(),
            M::up(include_str!(
                "migrations/014_cartridge_binding_identity_source.sql"
            )),
            M::up(include_str!(
                "migrations/015_cartridge_load_count_unknown.sql"
            )),
            M::up(include_str!("migrations/016_cartridge_operator_serial.sql")),
            M::up(include_str!("migrations/017_volume_observed_condition.sql")).foreign_key_check(),
            M::up(include_str!("migrations/018_volume_sealed_at.sql")),
            M::up(include_str!("migrations/019_drives.sql")),
            M::up(include_str!("migrations/020_cartridge_contacts.sql")),
            M::up(include_str!("migrations/021_health_logs_contact.sql")).foreign_key_check(),
        ];
        conn.pragma_update(None, "foreign_keys", "OFF").unwrap();
        Migrations::new(std::mem::take(&mut ms))
            .to_latest(&mut conn)
            .unwrap();
        conn.pragma_update(None, "foreign_keys", "ON").unwrap();
        conn
    }

    /// The schema as of migration 022 — for 023's pin-by-difference test,
    /// for the reason `open_memory_at_021` gives.
    fn open_memory_at_022() -> Connection {
        let mut conn = Connection::open_in_memory().unwrap();
        configure(&conn).unwrap();
        conn.pragma_update(None, "foreign_keys", "OFF").unwrap();
        migrations().to_version(&mut conn, 22).unwrap();
        conn.pragma_update(None, "foreign_keys", "ON").unwrap();
        let applied: i64 = conn
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(applied, 22, "positive control: stopped at 022");
        conn
    }

    /// The schema as of migration 023 — for 024's pin-by-difference test,
    /// for the reason `open_memory_at_021` gives.
    fn open_memory_at_023() -> Connection {
        let mut conn = Connection::open_in_memory().unwrap();
        configure(&conn).unwrap();
        conn.pragma_update(None, "foreign_keys", "OFF").unwrap();
        migrations().to_version(&mut conn, 23).unwrap();
        conn.pragma_update(None, "foreign_keys", "ON").unwrap();
        let applied: i64 = conn
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(applied, 23, "positive control: stopped at 023");
        conn
    }

    // --- Migration 021 (ADR-0013 §3: `health_logs`' ONE permitted rebuild) ---
    //
    // The #227/#264 standard in full, because `PRAGMA table_info` reports
    // NEITHER foreign keys NOR CHECK constraints: a columns-only test would
    // pass just as happily on a rebuild that silently dropped every
    // constraint the table had. That is the #227 lesson and #264 is the proof
    // it gets missed. Four parts, one test each — columns, indexes, foreign
    // keys, CHECK — plus a populated-database test for the data itself.

    /// PART 1 of 4 — COLUMNS. Every pre-021 column survives with its name,
    /// type, notnull, default and pk unchanged.
    ///
    /// The three deliberate changes are pulled out and pinned explicitly
    /// rather than folded into the blanket comparison (017's own handling of
    /// `observed_condition`): `contact_id` and `tapectl_version` are new, and
    /// `volume_id` loses its NOT NULL. Everything else is compared as a
    /// literal before/after equality, so a type or a default that drifted has
    /// nowhere to hide.
    ///
    /// Note what this does NOT prove — and it is most of what the rebuild
    /// risks: `table_info` reports no foreign key and no CHECK, so this test
    /// would pass on a rebuild that dropped `REFERENCES volumes(id)`,
    /// `REFERENCES verification_sessions(id)` and every constraint besides.
    /// Parts 2–4 are those halves.
    #[test]
    fn test_migration_021_changes_no_health_log_column() {
        let before = open_memory_at_020();
        let mut cols_020 = table_info(&before, "health_logs");
        let after = open_memory_at_021();
        let mut cols_021 = table_info(&after, "health_logs");

        // The two new columns, by shape and by position.
        let contact_pos = cols_021
            .iter()
            .position(|c| c.0 == "contact_id")
            .expect("021 must add contact_id");
        let contact = cols_021.remove(contact_pos);
        assert_eq!(
            contact,
            ("contact_id".to_string(), "INTEGER".to_string(), 0, None, 0,),
            "contact_id must be nullable with no default — a pre-021 row \
             genuinely has no contact and nothing backfills one"
        );
        let version_pos = cols_021
            .iter()
            .position(|c| c.0 == "tapectl_version")
            .expect("021 must add tapectl_version (ADR-0013 §7)");
        let version = cols_021.remove(version_pos);
        assert_eq!(
            version,
            (
                "tapectl_version".to_string(),
                "TEXT".to_string(),
                0,
                None,
                0,
            ),
            "tapectl_version must be nullable with no default — a pre-021 row \
             does not know which build wrote it, and stamping today's version \
             on it would be a lie in the column whose job is to say who observed"
        );

        // The one deliberately changed column, pinned on BOTH sides. Pulling
        // it out of only the "after" list would let a rebuild that ALSO
        // changed its type or default slip through.
        let vol_020 = cols_020.remove(
            cols_020
                .iter()
                .position(|c| c.0 == "volume_id")
                .expect("020 must still have volume_id"),
        );
        let vol_021 = cols_021.remove(
            cols_021
                .iter()
                .position(|c| c.0 == "volume_id")
                .expect("021 must still have volume_id"),
        );
        assert_eq!(
            vol_020,
            ("volume_id".to_string(), "INTEGER".to_string(), 1, None, 0),
            "precondition: volume_id was NOT NULL before 021"
        );
        assert_eq!(
            vol_021,
            ("volume_id".to_string(), "INTEGER".to_string(), 0, None, 0),
            "021 makes volume_id NULLABLE and changes nothing else about it — \
             a drive-only reading has no volume (ADR-0013 §§2-3)"
        );

        assert_eq!(
            cols_020, cols_021,
            "021's rebuild changed the name/type/notnull/default/pk of a \
             column that already existed in 020 — in particular `raw_log` and \
             `tape_alerts` must come through untouched (ADR-0013 §3)"
        );
    }

    /// PART 2 of 4 — INDEXES. A create/copy/drop/rename rebuild silently
    /// drops whatever its new DDL forgets to restate, and an index is the
    /// easiest thing to forget because nothing fails without it.
    ///
    /// `health_logs` carries no UNIQUE index and never has (`id INTEGER
    /// PRIMARY KEY` is a rowid alias, which SQLite gives no autoindex), so
    /// there is no uniqueness to prove still ENFORCES — and inventing one
    /// would be worse than useless. The behavioural assertion that actually
    /// belongs here is the OPPOSITE one, and it is not vacuous: many readings
    /// per volume is the whole point of a trend table, so a rebuild that
    /// "helpfully" made `idx_health_volume` unique would destroy it. That is
    /// proved by an insert that must SUCCEED, with `PRAGMA index_list`
    /// confirming neither index is unique.
    #[test]
    fn test_migration_021_recreates_the_volume_index_and_adds_the_contact_one() {
        let conn = open_memory().unwrap();
        assert_eq!(
            index_names(&conn, "health_logs"),
            vec!["idx_health_contact", "idx_health_volume"],
            "001's idx_health_volume must be restated by the rebuild, and 021 \
             adds idx_health_contact for the delta query ADR-0013 §3 names"
        );

        let unique: Vec<(String, i64)> = conn
            .prepare("PRAGMA index_list(health_logs)")
            .unwrap()
            .query_map([], |r| Ok((r.get::<_, String>(1)?, r.get::<_, i64>(2)?)))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        assert_eq!(
            unique.len(),
            2,
            "positive control: index_list saw {unique:?}"
        );
        assert!(
            unique.iter().all(|(_, uniq)| *uniq == 0),
            "no index on health_logs may be UNIQUE — a volume has many \
             readings and that is what the table is for: {unique:?}"
        );

        conn.execute(
            "INSERT INTO volumes (label, backend_type, backend_name, media_type, capacity_bytes, status)
             VALUES ('V-IDX', 'lto', 'lto0', 'LTO-6', 2500000000000, 'active')",
            [],
        )
        .unwrap();
        let vid = conn.last_insert_rowid();
        for _ in 0..2 {
            conn.execute(
                "INSERT INTO health_logs (volume_id, operation) VALUES (?1, 'verify')",
                rusqlite::params![vid],
            )
            .expect("two readings of one volume must both be storable");
        }
    }

    /// PART 3 of 4 — FOREIGN KEYS. `PRAGMA table_info` reports none of these,
    /// so part 1 would pass on a rebuild that dropped every one of them.
    ///
    /// Two assertions per edge, and the second is the one that would catch a
    /// dropped constraint: the enumeration proves the edge is DECLARED, the
    /// insert proves it is ENFORCED. `contact_id` gets the enforcement proof
    /// because it is 021's whole point; `volume_id` gets one too because it is
    /// the edge this rebuild had to restate while changing the column, which
    /// is exactly the shape a rebuild loses.
    #[test]
    fn test_migration_021_preserves_and_adds_the_health_log_foreign_keys() {
        let before = open_memory_at_020();
        let after = open_memory_at_021();

        assert_eq!(
            foreign_keys_of(&before, "health_logs"),
            vec![
                (
                    "verification_sessions".to_string(),
                    "session_id".to_string(),
                    "id".to_string()
                ),
                (
                    "volumes".to_string(),
                    "volume_id".to_string(),
                    "id".to_string()
                ),
            ],
            "precondition: 020 declares exactly the two outbound FKs 001 gave it"
        );
        assert_eq!(
            foreign_keys_of(&after, "health_logs"),
            vec![
                (
                    "cartridge_contacts".to_string(),
                    "contact_id".to_string(),
                    "id".to_string()
                ),
                (
                    "verification_sessions".to_string(),
                    "session_id".to_string(),
                    "id".to_string()
                ),
                (
                    "volumes".to_string(),
                    "volume_id".to_string(),
                    "id".to_string()
                ),
            ],
            "021 adds `contact_id REFERENCES cartridge_contacts(id)` and must \
             restate both existing edges — a rebuild drops any constraint its \
             new DDL omits"
        );

        after
            .execute(
                "INSERT INTO volumes (label, backend_type, backend_name, media_type, capacity_bytes, status)
                 VALUES ('V-FK', 'lto', 'lto0', 'LTO-6', 2500000000000, 'active')",
                [],
            )
            .unwrap();
        let vid = after.last_insert_rowid();

        // Positive control: the same INSERT minus the dangling key works, so
        // the two refusals below are the foreign keys and not a broken
        // statement.
        after
            .execute(
                "INSERT INTO health_logs (volume_id, operation) VALUES (?1, 'verify')",
                rusqlite::params![vid],
            )
            .unwrap();

        assert!(
            after
                .execute(
                    "INSERT INTO health_logs (volume_id, contact_id, operation)
                     VALUES (?1, 99999, 'verify')",
                    rusqlite::params![vid],
                )
                .is_err(),
            "contact_id must be ENFORCED, not merely declared: a reading \
             cannot name a contact that never happened"
        );
        assert!(
            after
                .execute(
                    "INSERT INTO health_logs (volume_id, operation) VALUES (99999, 'verify')",
                    [],
                )
                .is_err(),
            "volume_id is NULLABLE from 021, not unconstrained — a non-NULL \
             value must still be a real volume"
        );
    }

    /// PART 4 of 4 — THE CHECK IS GONE. Behavioural, and the assertion is the
    /// OPPOSITE of 017's: 017 proved a narrowed CHECK still refuses; this
    /// proves a dropped one no longer does.
    ///
    /// ADR-0013 §4. The old constraint was `CHECK(operation IN
    /// ('write','read','verify','clean'))`, a closed vocabulary already wrong
    /// in half its values — `read` and `clean` have never had a writer — and
    /// it made the one value the code needed (`resume`) impossible to write.
    /// Free text replaces it, with a pinning test against what code actually
    /// writes (`tape::health`'s `the_reading_vocabulary_is_what_code_actually_writes`)
    /// standing in for the typo protection.
    ///
    /// `'compact'` rather than `'resume'` deliberately: `resume` is now a
    /// real value with a real writer, and pinning the CHECK's absence on it
    /// would make this test and the rewritten `health::tests` tripwire the
    /// same test twice. `'compact'` is a value nothing writes today, so this
    /// test asserts only what it says it asserts.
    #[test]
    fn test_migration_021_drops_the_operation_check() {
        let insert = "INSERT INTO health_logs (volume_id, operation) VALUES (?1, 'compact')";
        let seed = "INSERT INTO volumes (label, backend_type, backend_name, media_type, capacity_bytes, status)
             VALUES ('V-CHECK', 'lto', 'lto0', 'LTO-6', 2500000000000, 'active')";

        // Positive control: the value really was forbidden one migration ago.
        // Without this, "the insert succeeded" could mean the CHECK is gone
        // OR that it never rejected this value in the first place.
        let before = open_memory_at_020();
        before.execute(seed, []).unwrap();
        let vid_before = before.last_insert_rowid();
        let err = before
            .execute(insert, rusqlite::params![vid_before])
            .expect_err("precondition: 020's CHECK rejects 'compact'");
        assert!(
            err.to_string().contains("CHECK constraint failed"),
            "precondition: the refuser must be the CHECK, got: {err}"
        );

        let after = open_memory_at_021();
        after.execute(seed, []).unwrap();
        let vid = after.last_insert_rowid();
        after
            .execute(insert, rusqlite::params![vid])
            .expect("021 drops the operation CHECK — the vocabulary is free TEXT");
        let stored: String = after
            .query_row(
                "SELECT operation FROM health_logs WHERE volume_id = ?1",
                rusqlite::params![vid],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(stored, "compact", "the value must be stored verbatim");

        // Free text is not OPTIONAL text: a reading that cannot say what kind
        // of reading it is has lost what makes it comparable to another.
        assert!(
            after
                .execute(
                    "INSERT INTO health_logs (volume_id, operation) VALUES (?1, NULL)",
                    rusqlite::params![vid],
                )
                .is_err(),
            "operation must stay NOT NULL"
        );

        // `'resume'` specifically — the value the whole CHECK drop exists
        // for (the #295 hazard). Refused at 020, stored at 021.
        let resume = "INSERT INTO health_logs (volume_id, operation) VALUES (?1, 'resume')";
        let err = before
            .execute(resume, rusqlite::params![vid_before])
            .expect_err("precondition: 020's CHECK rejects 'resume'");
        assert!(
            err.to_string().contains("CHECK constraint failed"),
            "precondition: the refuser must be the CHECK, got: {err}"
        );
        after
            .execute(resume, rusqlite::params![vid])
            .expect("021 must accept 'resume'");

        // `volume_id` NOT NULL is gone too (ADR-0013 §§2-3: a drive-only
        // reading has no volume) — refused at 020, stored at 021.
        let drive_only = "INSERT INTO health_logs (volume_id, operation) VALUES (NULL, 'verify')";
        let err = before
            .execute(drive_only, [])
            .expect_err("precondition: 020's volume_id is NOT NULL");
        assert!(
            err.to_string().contains("NOT NULL constraint failed"),
            "precondition: the refuser must be NOT NULL, got: {err}"
        );
        after
            .execute(drive_only, [])
            .expect("021 must accept a drive-only reading with no volume");
        let drive_only_rows: i64 = after
            .query_row(
                "SELECT COUNT(*) FROM health_logs WHERE volume_id IS NULL",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(drive_only_rows, 1, "the drive-only reading must be stored");
    }

    /// The data half: a populated 020 database through the real production
    /// `migrate()`, proving the rebuild moved every recorded fact and
    /// invented none.
    ///
    /// Mirrors
    /// `test_migrate_016_populated_db_to_017_migrates_quarantine_data_and_preserves_ids_and_fk`.
    /// The three rows are chosen to pin ADR-0013 §3's two survival
    /// requirements as a DISCRIMINATOR rather than an assertion about one
    /// value: `tape_alerts` is NULL on one row, 0 on another and 3 on a
    /// third, so a rebuild that defaulted or backfilled 0 fails on the first
    /// row while still "passing" on the other two. Each carries a distinct
    /// `raw_log`, so a rebuild that dropped or shuffled the column cannot
    /// pass by coincidence.
    ///
    /// Ids are explicit and out of sequence (900/700/800) because a rebuild
    /// that omitted `id` from its copy would renumber them 1/2/3 — which a
    /// test seeded with 1/2/3 could not distinguish from success.
    #[test]
    fn test_migrate_020_populated_db_to_021_preserves_every_recorded_fact() {
        let mut conn = open_memory_at_020();
        conn.execute(
            "INSERT INTO volumes (id, label, backend_type, backend_name, media_type, capacity_bytes, status)
             VALUES (42, 'V-KEEP', 'lto', 'lto0', 'LTO-6', 2500000000000, 'sealed')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO verification_sessions
                (id, volume_id, verify_type, outcome, completed_at, slices_checked, slices_passed, slices_failed)
             VALUES (7, 42, 'full', 'passed', '2026-01-01 00:00:00', 3, 3, 0)",
            [],
        )
        .unwrap();

        // (id, operation, tape_alerts, raw_log, session_id)
        type Seed<'a> = (i64, &'a str, Option<i64>, &'a str, Option<i64>);
        let seeded: Vec<Seed> = vec![
            (900, "write", None, "=== page 0x02 ===\nrow-900", None),
            (
                700,
                "verify",
                Some(0),
                "=== page 0x03 ===\nrow-700",
                Some(7),
            ),
            (800, "write", Some(3), "=== page 0x2e ===\nrow-800", None),
        ];
        for (id, op, alerts, raw, session) in &seeded {
            conn.execute(
                "INSERT INTO health_logs
                    (id, volume_id, session_id, logged_at, operation, total_bytes,
                     total_uncorrected, total_corrected, total_retries, total_rewritten,
                     raw_log, tape_alerts)
                 VALUES (?1, 42, ?2, '2026-01-01 00:00:0' || (?1 % 10), ?3, 1024, 1, 2, 3, 4, ?4, ?5)",
                rusqlite::params![id, session, op, raw, alerts],
            )
            .unwrap();
        }

        // The real production migrate(), with its real FK off/on wrapping and
        // this migration's registered `.foreign_key_check()`.
        migrate(&mut conn).unwrap();

        let fk_violations: i64 = conn
            .query_row("SELECT COUNT(*) FROM pragma_foreign_key_check", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(fk_violations, 0, "the rebuild orphaned a row");

        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM health_logs", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count as usize, seeded.len(), "the rebuild lost rows");

        for (id, op, alerts, raw, session) in &seeded {
            #[allow(clippy::type_complexity)]
            let row: (
                Option<i64>,
                Option<i64>,
                Option<i64>,
                String,
                String,
                Option<i64>,
                Option<i64>,
                Option<i64>,
                Option<i64>,
                Option<i64>,
                Option<String>,
                Option<i64>,
                Option<String>,
            ) = conn
                .query_row(
                    "SELECT volume_id, session_id, contact_id, logged_at, operation,
                            total_bytes, total_uncorrected, total_corrected, total_retries,
                            total_rewritten, raw_log, tape_alerts, tapectl_version
                       FROM health_logs WHERE id = ?1",
                    rusqlite::params![id],
                    |r| {
                        Ok((
                            r.get(0)?,
                            r.get(1)?,
                            r.get(2)?,
                            r.get(3)?,
                            r.get(4)?,
                            r.get(5)?,
                            r.get(6)?,
                            r.get(7)?,
                            r.get(8)?,
                            r.get(9)?,
                            r.get(10)?,
                            r.get(11)?,
                            r.get(12)?,
                        ))
                    },
                )
                .unwrap_or_else(|e| panic!("row {id} did not survive the rebuild: {e}"));

            assert_eq!(row.0, Some(42), "row {id}: volume_id moved");
            assert_eq!(row.1, *session, "row {id}: session_id moved");
            assert_eq!(row.4, *op, "row {id}: operation moved");
            assert_eq!(
                (row.5, row.6, row.7, row.8, row.9),
                (Some(1024), Some(1), Some(2), Some(3), Some(4)),
                "row {id}: a counter column shifted in the copy"
            );
            assert_eq!(
                row.10.as_deref(),
                Some(*raw),
                "row {id}: raw_log is the ONE place this project already honours \
                 the capture-everything standard — losing it is the suite \
                 defeating its own purpose (ADR-0013 §3)"
            );
            assert_eq!(
                row.11, *alerts,
                "row {id}: 009's NULL-vs-0 tape_alerts distinction must survive \
                 verbatim — NULL is 'not recorded', 0 is 'recorded, none raised', \
                 and backfilling 0 asserts the drive reported no alerts about a \
                 collection that never looked"
            );

            // Nothing is backfilled. Unknown must read as unknown.
            assert_eq!(
                row.2, None,
                "row {id}: contact_id must stay NULL — a pre-021 row genuinely \
                 had no contact and correlating one by timestamp would \
                 manufacture a link that reads like an observation"
            );
            assert_eq!(
                row.12, None,
                "row {id}: tapectl_version must stay NULL — a pre-021 row does \
                 not know which build wrote it"
            );
        }

        // The rows are still joinable both ways they were before.
        let joined: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM health_logs h
                   JOIN volumes v ON v.id = h.volume_id
                   JOIN verification_sessions vs ON vs.id = h.session_id",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(joined, 1, "the one session-bearing row must still join");

        let report = crate::cli::operations::db_fsck(&conn, false, false).unwrap();
        assert!(report.integrity_ok, "db fsck integrity check failed");
    }

    /// Migration 020 creates `cartridge_contacts` and NOTHING else
    /// (ADR-0013 §3, issue #296).
    ///
    /// The ADR exists because four drafts in the tape-forensics suite each
    /// independently proposed rebuilding `health_logs`, and four
    /// uncoordinated rebuilds of the table holding the schema's largest
    /// blobs is the most likely way that suite loses the data it was filed
    /// to capture. `health_logs` gets exactly ONE rebuild and it is
    /// migration 021.
    ///
    /// Pinned by DIFFERENCE against the 019 schema rather than by an
    /// absolute list, so a table some later migration legitimately adds
    /// cannot be mistaken for 020's doing.
    /// Migration 022 creates `mam_journal` (and its two indexes) and touches
    /// NOTHING else (issue #297): every other schema object's SQL is
    /// byte-identical to 021's. Pinned by difference, like 020's test.
    #[test]
    fn migration_022_creates_only_mam_journal() {
        fn objects(conn: &Connection) -> Vec<(String, String, Option<String>)> {
            let mut stmt = conn
                .prepare(
                    "SELECT type, name, sql FROM sqlite_master \
                     WHERE name NOT LIKE 'sqlite_%' ORDER BY type, name",
                )
                .unwrap();
            let v = stmt
                .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
                .unwrap()
                .map(|n| n.unwrap())
                .collect();
            v
        }
        let before = objects(&open_memory_at_021());
        // Pinned at 022, not "latest": 023 adds a table, and a comparison
        // against latest would start measuring 023 instead of this one.
        let after = objects(&open_memory_at_022());
        let added: Vec<(&str, &str)> = after
            .iter()
            .filter(|o| !before.contains(o))
            .map(|o| (o.0.as_str(), o.1.as_str()))
            .collect();
        assert_eq!(
            added,
            vec![
                ("index", "idx_mam_journal_contact"),
                ("index", "idx_mam_journal_serial"),
                ("table", "mam_journal"),
            ]
        );
        let changed_or_removed: Vec<&(String, String, Option<String>)> =
            before.iter().filter(|o| !after.contains(o)).collect();
        assert!(
            changed_or_removed.is_empty(),
            "022 must alter no existing object: {changed_or_removed:?}"
        );
    }

    /// Migration 023 creates `log_page_journal` (and its two indexes) and
    /// touches NOTHING else (issue #298). Pinned by difference against 022,
    /// like 022's own test.
    #[test]
    fn migration_023_creates_only_log_page_journal() {
        fn objects(conn: &Connection) -> Vec<(String, String, Option<String>)> {
            let mut stmt = conn
                .prepare(
                    "SELECT type, name, sql FROM sqlite_master \
                     WHERE name NOT LIKE 'sqlite_%' ORDER BY type, name",
                )
                .unwrap();
            let v = stmt
                .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
                .unwrap()
                .map(|n| n.unwrap())
                .collect();
            v
        }
        let before = objects(&open_memory_at_022());
        // Pinned at 023, not "latest": 024 adds a table, and a comparison
        // against latest would start measuring 024 instead of this one.
        let after = objects(&open_memory_at_023());
        let added: Vec<(&str, &str)> = after
            .iter()
            .filter(|o| !before.contains(o))
            .map(|o| (o.0.as_str(), o.1.as_str()))
            .collect();
        assert_eq!(
            added,
            vec![
                ("index", "idx_log_page_journal_contact"),
                ("index", "idx_log_page_journal_page"),
                ("table", "log_page_journal"),
            ]
        );
        let changed_or_removed: Vec<&(String, String, Option<String>)> =
            before.iter().filter(|o| !after.contains(o)).collect();
        assert!(
            changed_or_removed.is_empty(),
            "023 must alter no existing object: {changed_or_removed:?}"
        );
    }

    /// Migration 024 creates `restores` (and its three indexes) and touches
    /// NOTHING else (issue #306). Pinned by difference against 023, like
    /// 023's own test.
    #[test]
    fn migration_024_creates_only_restores() {
        fn objects(conn: &Connection) -> Vec<(String, String, Option<String>)> {
            let mut stmt = conn
                .prepare(
                    "SELECT type, name, sql FROM sqlite_master \
                     WHERE name NOT LIKE 'sqlite_%' ORDER BY type, name",
                )
                .unwrap();
            let v = stmt
                .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
                .unwrap()
                .map(|n| n.unwrap())
                .collect();
            v
        }
        let before = objects(&open_memory_at_023());
        // Pinned at 024, not "latest": 025 adds a table (issue #301).
        let after = objects(&open_memory_at_version(24));
        let added: Vec<(&str, &str)> = after
            .iter()
            .filter(|o| !before.contains(o))
            .map(|o| (o.0.as_str(), o.1.as_str()))
            .collect();
        assert_eq!(
            added,
            vec![
                ("index", "idx_restores_contact"),
                ("index", "idx_restores_unit"),
                ("index", "idx_restores_volume"),
                ("table", "restores"),
            ]
        );
        let changed_or_removed: Vec<&(String, String, Option<String>)> =
            before.iter().filter(|o| !after.contains(o)).collect();
        assert!(
            changed_or_removed.is_empty(),
            "024 must alter no existing object: {changed_or_removed:?}"
        );
    }

    /// Migration 025 creates `st_stats_journal` (and its one index) and
    /// touches NOTHING else (issue #301). Pinned by difference against the
    /// schema one step before it. It read `latest` and `latest - 1` while it
    /// was the newest migration; 026 moved it to the literal 24 -> 25 it
    /// always meant (issue #362).
    #[test]
    fn migration_025_creates_only_st_stats_journal() {
        fn objects(conn: &Connection) -> Vec<(String, String, Option<String>)> {
            let mut stmt = conn
                .prepare(
                    "SELECT type, name, sql FROM sqlite_master \
                     WHERE name NOT LIKE 'sqlite_%' ORDER BY type, name",
                )
                .unwrap();
            let v = stmt
                .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
                .unwrap()
                .map(|n| n.unwrap())
                .collect();
            v
        }
        // Pinned to exactly 024 -> 025 now that 026 is registered after it
        // (issue #362) -- the "latest - 1" form this used would measure 026.
        let latest_conn = open_memory_at_version(25);
        let before = objects(&open_memory_at_version(24));
        let after = objects(&latest_conn);
        let added: Vec<(&str, &str)> = after
            .iter()
            .filter(|o| !before.contains(o))
            .map(|o| (o.0.as_str(), o.1.as_str()))
            .collect();
        assert_eq!(
            added,
            vec![
                ("index", "idx_st_stats_journal_contact"),
                ("table", "st_stats_journal"),
            ]
        );
        let changed_or_removed: Vec<&(String, String, Option<String>)> =
            before.iter().filter(|o| !after.contains(o)).collect();
        assert!(
            changed_or_removed.is_empty(),
            "025 must alter no existing object: {changed_or_removed:?}"
        );

        // The CHECK on `point` is enforced, not merely declared (#227):
        // PRAGMA table_info reports no CHECK. Positive control: the same
        // insert with a legal point is accepted.
        let insert = |point: &str| {
            latest_conn.execute(
                "INSERT INTO st_stats_journal
                     (point, trigger, device, sysfs_dir, stats_json, tapectl_version)
                 VALUES (?1, 'volume verify', '/dev/null', '/x/stats', '{}', 'v')",
                [point],
            )
        };
        assert!(insert("middle").is_err(), "point must be open|close");
        insert("close").expect("positive control: a legal point is accepted");
        // And the foreign key to the spine is enforced.
        let bad_fk = latest_conn.execute(
            "INSERT INTO st_stats_journal
                 (contact_id, point, trigger, device, sysfs_dir, stats_json, tapectl_version)
             VALUES (99999, 'open', 'volume verify', '/dev/null', '/x/stats', '{}', 'v')",
            [],
        );
        assert!(bad_fk.is_err(), "contact_id must reference a real contact");

        let report = crate::cli::operations::db_fsck(&latest_conn, false, false).unwrap();
        assert!(report.integrity_ok, "integrity_check after 025");
        assert!(
            report.issues.is_empty(),
            "db fsck must be clean after 025: {:?}",
            report.issues
        );
    }

    #[test]
    fn migration_020_creates_only_cartridge_contacts() {
        fn table_names(conn: &Connection) -> Vec<String> {
            let mut stmt = conn
                .prepare(
                    "SELECT name FROM sqlite_master WHERE type = 'table' \
                     AND name NOT LIKE 'sqlite_%' ORDER BY name",
                )
                .unwrap();
            let names = stmt
                .query_map([], |r| r.get::<_, String>(0))
                .unwrap()
                .map(|n| n.unwrap())
                .collect();
            names
        }

        let before = table_names(&open_memory_at_019());
        let after = table_names(&open_memory_at_020());

        let added: Vec<&String> = after.iter().filter(|t| !before.contains(t)).collect();
        assert_eq!(
            added,
            vec!["cartridge_contacts"],
            "migration 020 must add exactly one table"
        );
        let removed: Vec<&String> = before.iter().filter(|t| !after.contains(t)).collect();
        assert!(
            removed.is_empty(),
            "migration 020 must remove no table: {removed:?}"
        );
    }

    /// The `health_logs` pin specifically, by column list, mirroring
    /// `tape::drive_identity::tests::migration_019_adds_no_column_to_health_logs`.
    ///
    /// `PRAGMA table_info` is compared on BOTH sides rather than against a
    /// hardcoded list: the #227 lesson is that `table_info` reports neither
    /// foreign keys nor CHECK constraints, so an equality against a literal
    /// list would pass a rebuild that silently dropped 009's NULL-vs-0
    /// `tape_alerts` distinction. Comparing 019's own table against 020's
    /// proves the table was not touched at all, which is the stronger claim.
    #[test]
    fn migration_020_adds_no_column_to_health_logs() {
        fn health_logs_columns(conn: &Connection) -> Vec<(String, String, i64, Option<String>)> {
            let mut stmt = conn.prepare("PRAGMA table_info(health_logs)").unwrap();
            let cols = stmt
                .query_map([], |r| {
                    Ok((
                        r.get::<_, String>(1)?,
                        r.get::<_, String>(2)?,
                        r.get::<_, i64>(3)?,
                        r.get::<_, Option<String>>(4)?,
                    ))
                })
                .unwrap()
                .map(|c| c.unwrap())
                .collect();
            cols
        }

        let before = health_logs_columns(&open_memory_at_019());
        let after = health_logs_columns(&open_memory_at_020());
        assert_eq!(
            before, after,
            "migration 020 must leave health_logs byte-identical to what 019 left \
             behind — its one permitted rebuild is migration 021 (ADR-0013 §3)"
        );
        // Positive control: the comparison above would also "pass" if both
        // sides were empty, i.e. if the table did not exist at all.
        assert!(
            after.iter().any(|(name, _, _, _)| name == "tape_alerts"),
            "positive control: health_logs must actually exist and still carry 009's column"
        );
    }

    /// A connection migrated to exactly the 019 schema — the before-picture
    /// for 020's "changed nothing else" pins above.
    fn open_memory_at_019() -> Connection {
        let mut conn = Connection::open_in_memory().unwrap();
        configure(&conn).unwrap();
        let mut ms = vec![
            M::up(include_str!("migrations/001_initial.sql")),
            M::up(include_str!("migrations/002_fts5_catalog.sql")),
            M::up(include_str!("migrations/003_v2_lifecycle.sql")).foreign_key_check(),
            M::up(include_str!("migrations/004_volume_uuid.sql")),
            M::up(include_str!("migrations/005_file_types.sql")),
            M::up(include_str!("migrations/006_write_session_dir.sql")),
            M::up(include_str!("migrations/007_warehouse_locations.sql")),
            M::up(include_str!("migrations/008_drop_volume_storage_class.sql")),
            M::up(include_str!("migrations/009_health_tape_alerts.sql")),
            M::up(include_str!("migrations/010_stage_set_origin.sql")),
            M::up(include_str!("migrations/011_cartridge_serial_index.sql")),
            M::up(include_str!("migrations/012_cartridge_lifecycle.sql")).foreign_key_check(),
            M::up(include_str!("migrations/013_drop_manifest_entry_flags.sql")).foreign_key_check(),
            M::up(include_str!(
                "migrations/014_cartridge_binding_identity_source.sql"
            )),
            M::up(include_str!(
                "migrations/015_cartridge_load_count_unknown.sql"
            )),
            M::up(include_str!("migrations/016_cartridge_operator_serial.sql")),
            M::up(include_str!("migrations/017_volume_observed_condition.sql")).foreign_key_check(),
            M::up(include_str!("migrations/018_volume_sealed_at.sql")),
            M::up(include_str!("migrations/019_drives.sql")),
        ];
        conn.pragma_update(None, "foreign_keys", "OFF").unwrap();
        Migrations::new(std::mem::take(&mut ms))
            .to_latest(&mut conn)
            .unwrap();
        conn.pragma_update(None, "foreign_keys", "ON").unwrap();
        conn
    }

    /// THE test for this migration. Three volumes, each pinning one of the
    /// three data-migration paths issue #242 specifies:
    ///
    /// - `Q-EVENT`: `status = 'quarantined'` with an `events` row recording
    ///   the transition into quarantine (`field = 'status'`, `new_value =
    ///   'quarantined'`, `old_value = 'sealed'`) — the verify path's shape
    ///   (`quarantine_on_medium_evidence`). Must come out `status =
    ///   'sealed'`, `observed_condition = 'quarantined'`.
    /// - `Q-NOEVENT`: `status = 'quarantined'` with NO such event — the
    ///   write-path writers' shape (`session.rs`, which never sealed). Must
    ///   come out `status = 'initialized'` (the deliberate narrowing of this
    ///   ADR's own "restore to sealed" fallback — see the migration
    ///   header), `observed_condition = 'quarantined'`.
    /// - `Q-SEALED`: an ordinary `sealed` volume, untouched by any of this.
    ///   Must come out unchanged, `observed_condition = 'ok'`.
    ///
    /// `Q-SEALED` also carries a `writes` row and a `cartridge_volumes` row
    /// — the one table holding a `REFERENCES volumes(id)` FK the rebuild
    /// must not renumber (mirrors 011->012's own id-preservation
    /// discriminator).
    #[test]
    fn test_migrate_016_populated_db_to_017_migrates_quarantine_data_and_preserves_ids_and_fk() {
        let mut conn = open_memory_at_016();

        conn.execute(
            "INSERT INTO volumes (label, backend_type, backend_name, media_type, capacity_bytes, status)
             VALUES ('Q-EVENT', 'lto', 'lto0', 'LTO-6', 2500000000000, 'quarantined')",
            [],
        )
        .unwrap();
        let q_event_id = conn.last_insert_rowid();
        // An older, irrelevant event first, so the "most recent" ordering is
        // a real discriminator and not vacuously the only row.
        conn.execute(
            "INSERT INTO events (entity_type, entity_id, entity_label, action, field, old_value, new_value)
             VALUES ('volume', ?1, 'Q-EVENT', 'created', NULL, NULL, NULL)",
            rusqlite::params![q_event_id],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO events (entity_type, entity_id, entity_label, action, field, old_value, new_value)
             VALUES ('volume', ?1, 'Q-EVENT', 'verify_quarantined', 'status', 'sealed', 'quarantined')",
            rusqlite::params![q_event_id],
        )
        .unwrap();

        conn.execute(
            "INSERT INTO volumes (label, backend_type, backend_name, media_type, capacity_bytes, status)
             VALUES ('Q-NOEVENT', 'lto', 'lto0', 'LTO-6', 2500000000000, 'quarantined')",
            [],
        )
        .unwrap();
        let q_noevent_id = conn.last_insert_rowid();

        conn.execute("INSERT INTO locations (name) VALUES ('home-rack')", [])
            .unwrap();
        let loc_id = conn.last_insert_rowid();
        conn.execute(
            "INSERT INTO volumes (label, backend_type, backend_name, media_type, capacity_bytes, status, location_id)
             VALUES ('Q-SEALED', 'lto', 'lto0', 'LTO-6', 2500000000000, 'sealed', ?1)",
            rusqlite::params![loc_id],
        )
        .unwrap();
        let q_sealed_id = conn.last_insert_rowid();

        conn.execute(
            "INSERT INTO cartridges (barcode, media_type, nominal_capacity, status, location_id)
             VALUES ('BC-Q', 'LTO-6', 2500000000000, 'in_use', ?1)",
            rusqlite::params![loc_id],
        )
        .unwrap();
        let cart_id = conn.last_insert_rowid();
        conn.execute(
            "INSERT INTO cartridge_volumes (cartridge_id, volume_id) VALUES (?1, ?2)",
            rusqlite::params![cart_id, q_sealed_id],
        )
        .unwrap();

        conn.execute(
            "INSERT INTO tenants (name, is_operator, status) VALUES ('t', 0, 'active')",
            [],
        )
        .unwrap();
        let tid = conn.last_insert_rowid();
        conn.execute(
            "INSERT INTO units (uuid, name, tenant_id, checksum_mode, encrypt, status)
             VALUES ('u-q', 'q-unit', ?1, 'mtime_size', 1, 'active')",
            rusqlite::params![tid],
        )
        .unwrap();
        let unit_id = conn.last_insert_rowid();
        conn.execute(
            "INSERT INTO snapshots (unit_id, version, snapshot_type, status, source_path)
             VALUES (?1, 1, 'full', 'current', '/src')",
            rusqlite::params![unit_id],
        )
        .unwrap();
        let snap_id = conn.last_insert_rowid();
        conn.execute(
            "INSERT INTO stage_sets (snapshot_id, status, slice_size) VALUES (?1, 'staged', 524288)",
            rusqlite::params![snap_id],
        )
        .unwrap();
        let ss_id = conn.last_insert_rowid();
        conn.execute(
            "INSERT INTO writes (stage_set_id, snapshot_id, volume_id, status)
             VALUES (?1, ?2, ?3, 'completed')",
            rusqlite::params![ss_id, snap_id, q_sealed_id],
        )
        .unwrap();

        // The real production migration path, with its real FK off/on wrapping,
        // stopped at 017 so a later migration cannot move this pin.
        migrate_to(&mut conn, Some(17)).unwrap();

        let fk_violations: i64 = conn
            .query_row("SELECT COUNT(*) FROM pragma_foreign_key_check", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(
            fk_violations, 0,
            "PRAGMA foreign_key_check found violations after the volumes rebuild"
        );

        let read = |id: i64| -> (String, String) {
            conn.query_row(
                "SELECT status, observed_condition FROM volumes WHERE id = ?1",
                rusqlite::params![id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap()
        };
        assert_eq!(
            read(q_event_id),
            ("sealed".to_string(), "quarantined".to_string()),
            "the events row's old_value must restore status; observed_condition carries the fact forward"
        );
        assert_eq!(
            read(q_noevent_id),
            ("initialized".to_string(), "quarantined".to_string()),
            "no events row -> the write-path fallback is 'initialized', never 'sealed' \
             (this ADR's point 4, deliberately narrowed -- see the migration header)"
        );
        assert_eq!(
            read(q_sealed_id),
            ("sealed".to_string(), "ok".to_string()),
            "an ordinary sealed volume must be untouched by the migration"
        );

        // The join still names the SAME volume -- proof the ids survived,
        // not merely that they still resolve to something.
        let joined: String = conn
            .query_row(
                "SELECT v.label FROM cartridge_volumes cv
                 JOIN volumes v ON v.id = cv.volume_id
                 WHERE cv.cartridge_id = ?1",
                rusqlite::params![cart_id],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            joined, "Q-SEALED",
            "the rebuild renumbered volumes and silently re-pointed the join"
        );

        let write_volume_label: String = conn
            .query_row(
                "SELECT v.label FROM writes w JOIN volumes v ON v.id = w.volume_id",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(write_volume_label, "Q-SEALED");

        assert_eq!(
            index_names(&conn, "volumes"),
            vec![
                "idx_volumes_location",
                "idx_volumes_status",
                "idx_volumes_uuid",
                // the implicit UNIQUE(label) autoindex
                "sqlite_autoindex_volumes_1",
            ]
        );

        let err = conn.execute(
            "UPDATE volumes SET status = 'quarantined' WHERE id = ?1",
            rusqlite::params![q_sealed_id],
        );
        assert!(
            err.is_err(),
            "'quarantined' must be rejected as a status value after the rebuild (issue #242)"
        );

        let report = crate::cli::operations::db_fsck(&conn, false, false).unwrap();
        assert!(report.integrity_ok, "db fsck integrity check failed");
    }

    /// **The migration's own CHECK must survive a volume quarantined TWICE.**
    ///
    /// Pre-017 `quarantine_on_medium_evidence` had no guard: it read whatever
    /// `status` held, wrote `'quarantined'` over it, and logged `old_value =
    /// previous_status`. Re-verifying an already-quarantined volume is a
    /// supported path — `volume verify` has always been runnable twice, and
    /// `re_verifying_an_already_quarantined_volume_reports_no_condition_change`
    /// exists precisely because it is — so a pre-017 database can legally hold
    /// `field = 'status'`, `new_value = 'quarantined'`, `old_value =
    /// 'quarantined'` as the MOST RECENT such event.
    ///
    /// A restore query that took simply the most recent event would then write
    /// `'quarantined'` back into `status`, which this very migration has just
    /// made illegal. The INSERT fails the new CHECK, the migration aborts,
    /// `db::open` fails — and every command fails with it, including the
    /// `db fsck --repair` that is supposed to be the way out. That is issue
    /// #233's bricked-database shape, manufactured by the fix for #242, on a
    /// database whose only sin was having a tape verified twice.
    ///
    /// So the restore takes the most recent transition into quarantine **from
    /// a legal status**, never merely the most recent transition into
    /// quarantine.
    #[test]
    fn test_migration_017_restores_a_twice_quarantined_volume_not_to_quarantined() {
        let mut conn = open_memory_at_016();

        conn.execute(
            "INSERT INTO volumes (label, backend_type, backend_name, media_type, capacity_bytes, status)
             VALUES ('Q-TWICE', 'lto', 'lto0', 'LTO-6', 2500000000000, 'quarantined')",
            [],
        )
        .unwrap();
        let q_twice_id = conn.last_insert_rowid();

        // First quarantine: sealed -> quarantined, the ordinary verify shape.
        conn.execute(
            "INSERT INTO events (entity_type, entity_id, entity_label, action, field, old_value, new_value)
             VALUES ('volume', ?1, 'Q-TWICE', 'verify_quarantined', 'status', 'sealed', 'quarantined')",
            rusqlite::params![q_twice_id],
        )
        .unwrap();
        // Second verify of the same already-quarantined tape. The pre-017
        // writer recorded the no-op transition verbatim, so `old_value` is
        // itself 'quarantined' and this row has the higher `events.id`.
        conn.execute(
            "INSERT INTO events (entity_type, entity_id, entity_label, action, field, old_value, new_value)
             VALUES ('volume', ?1, 'Q-TWICE', 'verify_quarantined', 'status', 'quarantined', 'quarantined')",
            rusqlite::params![q_twice_id],
        )
        .unwrap();

        migrate(&mut conn).expect(
                "migration 017 must not restore a status its own CHECK forbids: a volume \
                 quarantined twice carries 'quarantined' as the most recent old_value, and \
                 writing that back aborts the migration and bricks the database (issues #242, #233)",
            );

        let (status, condition): (String, String) = conn
            .query_row(
                "SELECT status, observed_condition FROM volumes WHERE id = ?1",
                rusqlite::params![q_twice_id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(
            (status.as_str(), condition.as_str()),
            ("sealed", "quarantined"),
            "the restore must reach past the no-op re-quarantine event to the real transition"
        );
    }

    /// `observed_condition` itself is a closed set of exactly two values,
    /// defaulting to `'ok'` for a row that never mentions it — the negative
    /// half matters most, matching the discipline every other CHECK test in
    /// this file follows (`test_migration_012_offsite_rejected_...`).
    #[test]
    fn test_migration_017_observed_condition_is_closed_and_defaults_ok() {
        let conn = open_memory().unwrap();
        conn.execute(
            "INSERT INTO volumes (label, backend_type, backend_name, media_type, capacity_bytes, status)
             VALUES ('L6-DEFAULT', 'lto', 'lto0', 'LTO-6', 2500000000000, 'initialized')",
            [],
        )
        .unwrap();
        let default_condition: String = conn
            .query_row(
                "SELECT observed_condition FROM volumes WHERE label = 'L6-DEFAULT'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(default_condition, "ok");

        conn.execute(
            "INSERT INTO volumes (label, backend_type, backend_name, media_type, capacity_bytes, status, observed_condition)
             VALUES ('L6-QUAR', 'lto', 'lto0', 'LTO-6', 2500000000000, 'sealed', 'quarantined')",
            [],
        )
        .unwrap_or_else(|e| panic!("'quarantined' should be a legal observed_condition: {e}"));

        let err = conn.execute(
            "INSERT INTO volumes (label, backend_type, backend_name, media_type, capacity_bytes, status, observed_condition)
             VALUES ('L6-BAD', 'lto', 'lto0', 'LTO-6', 2500000000000, 'sealed', 'sketchy')",
            [],
        );
        assert!(
            err.is_err(),
            "an unknown observed_condition must be rejected"
        );
    }

    /// 012's own standard (issue #227), applied to 017: every column that
    /// existed before the rebuild survives with its name, type, notnull,
    /// default and pk unchanged. `observed_condition` is 017's whole point,
    /// so it cannot be folded into a blanket "nothing changed" comparison
    /// the way 012's analogous test does for `cartridges` — it is pulled out
    /// and its own shape pinned explicitly, then the rest is compared as a
    /// literal before/after equality.
    ///
    /// Note what this does NOT prove: `PRAGMA table_info` does not report
    /// CHECK constraints, so this test would pass even if the rebuild had
    /// dropped the `status` or `observed_condition` CHECK entirely. That half
    /// is `test_migration_017_observed_condition_is_closed_and_defaults_ok`
    /// (and the twice-quarantined / restores-quarantine-data tests for
    /// `status`).
    #[test]
    fn test_migration_017_changes_no_volume_column() {
        let before = open_memory_at_016();
        let cols_016 = table_info(&before, "volumes");

        // Frozen at exactly 017 (issue #277's migration 018 adds
        // `sealed_at` immediately after this one and must not be mistaken
        // for something 017 itself did) — see `open_memory_at_017`'s doc
        // comment.
        let after = open_memory_at_017();
        let mut cols_017 = table_info(&after, "volumes");

        let observed_condition_pos = cols_017
            .iter()
            .position(|c| c.0 == "observed_condition")
            .expect("017 must add observed_condition");
        let observed_condition = cols_017.remove(observed_condition_pos);
        assert_eq!(
            observed_condition,
            (
                "observed_condition".to_string(),
                "TEXT".to_string(),
                1,
                Some("'ok'".to_string()),
                0,
            ),
            "observed_condition must be NOT NULL DEFAULT 'ok'"
        );

        assert_eq!(
            cols_016, cols_017,
            "017's rebuild changed the name/type/notnull/default/pk of a \
             column that already existed in 016"
        );
    }

    /// 012's index standard (issue #227), applied to 017: every index the
    /// pre-rebuild table carried is back, and `idx_volumes_uuid` still
    /// ENFORCES uniqueness, not merely exists. This is the one most worth
    /// pinning explicitly — a duplicate volume uuid breaks
    /// `check_tape_contact`'s File 0 identity match
    /// (`docs/design/layout-session.md`), and a create/copy/drop/rename
    /// rebuild silently drops whatever its new DDL forgets to restate.
    #[test]
    fn test_migration_017_recreates_every_index_and_pins_uuid_uniqueness() {
        let conn = open_memory_at_version(17);
        assert_eq!(
            index_names(&conn, "volumes"),
            vec![
                "idx_volumes_location",
                "idx_volumes_status",
                "idx_volumes_uuid",
                // the implicit UNIQUE(label) autoindex
                "sqlite_autoindex_volumes_1",
            ]
        );

        conn.execute(
            "INSERT INTO volumes (label, backend_type, backend_name, media_type, capacity_bytes, status, uuid)
             VALUES ('V-1', 'lto', 'lto0', 'LTO-6', 2500000000000, 'initialized', '11111111-1111-1111-1111-111111111111')",
            [],
        )
        .unwrap();
        let dup = conn.execute(
            "INSERT INTO volumes (label, backend_type, backend_name, media_type, capacity_bytes, status, uuid)
             VALUES ('V-2', 'lto', 'lto0', 'LTO-6', 2500000000000, 'initialized', '11111111-1111-1111-1111-111111111111')",
            [],
        );
        assert!(
            dup.is_err(),
            "idx_volumes_uuid must remain UNIQUE after the rebuild — a duplicate \
             uuid would break the resume/contact divergence check"
        );
    }

    /// 012's FK standard (issue #227), applied to 017's one OUTBOUND edge:
    /// `volumes.location_id REFERENCES locations(id)`. `PRAGMA table_info`
    /// reports neither FK nor CHECK constraints, so
    /// `test_migration_017_changes_no_volume_column` would pass just as
    /// happily if the rebuild had dropped this reference. Two assertions,
    /// and the second is the one that would actually catch a dropped
    /// constraint: the enumeration proves the edge is declared, the insert
    /// proves it is ENFORCED.
    #[test]
    fn test_migration_017_preserves_the_volumes_location_foreign_key() {
        let before = open_memory_at_016();
        let after = open_memory().unwrap();

        let expected = vec![(
            "locations".to_string(),
            "location_id".to_string(),
            "id".to_string(),
        )];
        assert_eq!(
            foreign_keys_of(&before, "volumes"),
            expected,
            "precondition: 016 declares exactly the one outbound FK"
        );
        assert_eq!(
            foreign_keys_of(&after, "volumes"),
            expected,
            "017's rebuild must restate `location_id REFERENCES locations(id)` — \
             a rebuild drops any constraint its new DDL omits"
        );

        let err = after.execute(
            "INSERT INTO volumes (label, backend_type, backend_name, media_type, capacity_bytes, status, location_id)
             VALUES ('V-dangling', 'lto', 'lto0', 'LTO-6', 2500000000000, 'initialized', 99999)",
            [],
        );
        assert!(
            err.is_err(),
            "the FK must be ENFORCED after the rebuild, not merely declared"
        );
    }

    /// Makes explicit what
    /// `test_migrate_016_populated_db_to_017_migrates_quarantine_data_and_preserves_ids_and_fk`
    /// only shows indirectly (via a `cartridge_volumes` join that still
    /// resolves): the rebuild's `INSERT ... SELECT v.id, ...` copies `id`
    /// verbatim rather than letting SQLite assign fresh rowids. An explicit,
    /// out-of-sequence id (500, not 1) makes this a real discriminator — a
    /// rebuild that dropped `id` from the copy would renumber the sole row
    /// to 1, not merely leave it unchanged by coincidence. Six tables hold a
    /// `REFERENCES volumes(id)` foreign key (see the corrected migration
    /// header); every one of them depends on this.
    #[test]
    fn test_migration_017_preserves_volume_ids_across_rebuild() {
        let mut conn = open_memory_at_016();
        conn.execute(
            "INSERT INTO volumes (id, label, backend_type, backend_name, media_type, capacity_bytes, status)
             VALUES (500, 'ID-PIN', 'lto', 'lto0', 'LTO-6', 2500000000000, 'sealed')",
            [],
        )
        .unwrap();

        migrate(&mut conn).unwrap();

        let after_id: i64 = conn
            .query_row("SELECT id FROM volumes WHERE label = 'ID-PIN'", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(
            after_id, 500,
            "the rebuild must preserve the row's original id, not let SQLite \
             assign a fresh one"
        );
    }

    // --- Migration 026 (issue #362): drop the schema states nothing sets ---

    /// The three tables 026 rebuilds.
    const REBUILT_BY_026: [&str; 3] = ["units", "snapshots", "volumes"];

    /// One cell rendered with its storage class, so `1` and `'1'` differ.
    fn render_cell(v: rusqlite::types::ValueRef<'_>) -> String {
        use rusqlite::types::ValueRef;
        match v {
            ValueRef::Null => "NULL".into(),
            ValueRef::Integer(i) => format!("i:{i}"),
            ValueRef::Real(f) => format!("r:{f}"),
            ValueRef::Text(t) => format!("t:{}", String::from_utf8_lossy(t)),
            ValueRef::Blob(b) => format!("b:{b:02x?}"),
        }
    }

    /// Every row of every table (the FTS shadow tables included), sorted,
    /// keyed by table name. The "nothing else moved" discriminator: a
    /// rebuild that renumbered, dropped, truncated or re-typed a single
    /// cell anywhere shows up as a difference here. `SELECT *`, not
    /// `rowid, *`: `files_fts_config` is WITHOUT ROWID, and every table
    /// whose rowid anything points at declares it as `id`.
    fn every_row(conn: &Connection) -> std::collections::BTreeMap<String, Vec<Vec<String>>> {
        let tables: Vec<String> = conn
            .prepare(
                "SELECT name FROM sqlite_master WHERE type = 'table' \
                 AND name NOT LIKE 'sqlite_%' ORDER BY name",
            )
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        let mut out = std::collections::BTreeMap::new();
        for table in tables {
            let mut stmt = conn.prepare(&format!("SELECT * FROM \"{table}\"")).unwrap();
            let n = stmt.column_count();
            let mut rows: Vec<Vec<String>> = stmt
                .query_map([], |r| {
                    (0..n)
                        .map(|i| r.get_ref(i).map(render_cell))
                        .collect::<rusqlite::Result<Vec<String>>>()
                })
                .unwrap()
                .collect::<rusqlite::Result<_>>()
                .unwrap();
            rows.sort();
            out.insert(table, rows);
        }
        out
    }

    /// (type, name, tbl_name, sql) for every schema object, sorted.
    fn schema_objects(conn: &Connection) -> Vec<(String, String, String, Option<String>)> {
        conn.prepare(
            "SELECT type, name, tbl_name, sql FROM sqlite_master \
             ORDER BY type, name",
        )
        .unwrap()
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap()
    }

    fn user_version(conn: &Connection) -> i64 {
        conn.query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap()
    }

    /// Seed a schema-25 database with rows in `units`, `snapshots` and
    /// `volumes` in EVERY status 026 keeps, every column set to a
    /// non-default value where it has one, out-of-sequence ids (the 017
    /// `500` trick: a copy that dropped `id` would renumber from 1), and a
    /// row in every table holding one of the 17 foreign keys into them.
    fn seed_schema_25(conn: &Connection) {
        conn.execute_batch(
            "INSERT INTO tenants (id, name, is_operator, status) VALUES (3, 'op', 1, 'active');
             INSERT INTO archive_sets (id, name, min_copies) VALUES (4, 'as', 2);
             INSERT INTO locations (id, name, kind) VALUES (5, 'shelf-a', 'shelf');
             INSERT INTO locations (id, name, kind) VALUES (6, 'vault', 'warehouse');
             INSERT INTO tags (id, name) VALUES (7, 'tag');
             INSERT INTO cartridges (id, barcode, media_type, nominal_capacity, status)
                  VALUES (8, 'BC0008', 'LTO-6', 2500000000000, 'in_use');
             INSERT INTO drives (id, serial) VALUES (9, 'DRV0009');

             INSERT INTO units (id, uuid, name, tenant_id, archive_set_id, current_path,
                                checksum_mode, encrypt, status, created_at, last_scanned, notes)
                  VALUES (500, 'uuid-500', 'u-active', 3, 4, '/src/a', 'sha256', 0, 'active',
                          '2026-01-01 00:00:00', '2026-01-02 00:00:00', 'note-500');
             INSERT INTO units (id, uuid, name, tenant_id, status, created_at)
                  VALUES (501, 'uuid-501', 'u-tape-only', 3, 'tape_only', '2026-01-01 00:00:01');
             INSERT INTO units (id, uuid, name, tenant_id, status, created_at)
                  VALUES (502, 'uuid-502', 'u-missing', 3, 'missing', '2026-01-01 00:00:02');

             INSERT INTO snapshots (id, unit_id, version, snapshot_type, status, source_path,
                                    total_size, file_count, created_at, superseded_at, notes)
                  VALUES (600, 500, 1, 'full', 'current', '/src/a', 10, 1,
                          '2026-01-03 00:00:00', '2026-01-04 00:00:00', 'note-600');
             INSERT INTO snapshots (id, unit_id, version, snapshot_type, base_snapshot_id,
                                    status, source_path, created_at)
                  VALUES (601, 500, 2, 'differential', 600, 'reclaimable', '/src/a',
                          '2026-01-03 00:00:01');
             INSERT INTO snapshots (id, unit_id, version, status, source_path, created_at)
                  VALUES (602, 500, 3, 'created', '/src/a', '2026-01-03 00:00:02');
             INSERT INTO snapshots (id, unit_id, version, status, source_path, created_at)
                  VALUES (603, 501, 1, 'staged', '/src/b', '2026-01-03 00:00:03');
             INSERT INTO snapshots (id, unit_id, version, status, source_path, created_at)
                  VALUES (604, 502, 1, 'purged', '/src/c', '2026-01-03 00:00:04');

             INSERT INTO volumes (id, label, backend_type, backend_name, media_type,
                                  capacity_bytes, mam_capacity_bytes, mam_remaining_at_start,
                                  bytes_written, num_data_files, has_manifest, location_id,
                                  status, observed_condition, first_write, last_write, notes,
                                  created_at, uuid, sealed_at)
                  VALUES (700, 'V-SEALED', 'lto', 'lto0', 'LTO-6', 2500000000000,
                          2500002097152, 2400000000000, 123, 4, 1, 5, 'sealed', 'quarantined',
                          '2026-01-05 00:00:00', '2026-01-05 01:00:00', 'note-700',
                          '2026-01-05 00:00:00', 'vol-uuid-700', '2026-01-05 01:00:00');
             INSERT INTO volumes (id, label, backend_type, backend_name, capacity_bytes, status,
                                  created_at)
                  VALUES (701, 'V-INIT', 'lto', 'lto0', 1000, 'initialized', '2026-01-05 00:00:01');
             INSERT INTO volumes (id, label, backend_type, backend_name, capacity_bytes, status,
                                  created_at)
                  VALUES (702, 'V-ACTIVE', 'lto', 'lto0', 1000, 'active', '2026-01-05 00:00:02');
             INSERT INTO volumes (id, label, backend_type, backend_name, capacity_bytes, status,
                                  created_at)
                  VALUES (703, 'V-FULL', 'lto', 'lto0', 1000, 'full', '2026-01-05 00:00:03');
             INSERT INTO volumes (id, label, backend_type, backend_name, capacity_bytes, status,
                                  created_at)
                  VALUES (704, 'V-RETIRED', 'lto', 'lto0', 1000, 'retired', '2026-01-05 00:00:04');
             INSERT INTO volumes (id, label, backend_type, backend_name, capacity_bytes, status,
                                  created_at)
                  VALUES (705, 'V-ERASED', 'lto', 'lto0', 1000, 'erased', '2026-01-05 00:00:05');

             INSERT INTO unit_tags (unit_id, tag_id) VALUES (500, 7);
             INSERT INTO unit_path_history (id, unit_id, path, observed_at)
                  VALUES (800, 500, '/src/a', '2026-01-06 00:00:00');
             INSERT INTO files (id, snapshot_id, path, size_bytes, sha256, is_directory)
                  VALUES (801, 600, 'dir/needle.txt', 10, '00ab00ab00ab00ab00ab00ab00ab00ab00ab00ab00ab00ab00ab00ab00ab00ab', 0);
             INSERT INTO manifests (id, snapshot_id, created_at)
                  VALUES (802, 600, '2026-01-06 00:00:01');
             INSERT INTO stage_sets (id, snapshot_id, status, slice_size, num_slices, created_at)
                  VALUES (803, 600, 'staged', 1024, 1, '2026-01-06 00:00:02');
             INSERT INTO writes (id, stage_set_id, snapshot_id, volume_id, status, created_at)
                  VALUES (804, 803, 600, 700, 'completed', '2026-01-06 00:00:03');
             INSERT INTO cartridge_volumes (id, cartridge_id, volume_id, mounted_at)
                  VALUES (805, 8, 700, '2026-01-06 00:00:04');
             INSERT INTO cartridge_contacts (id, cartridge_id, volume_id, drive_id, operation,
                                             device, opened_at)
                  VALUES (806, 8, 700, 9, 'volume verify', '/dev/null', '2026-01-06 00:00:05');
             INSERT INTO verification_sessions (id, volume_id, started_at, outcome)
                  VALUES (807, 700, '2026-01-06 00:00:06', 'passed');
             INSERT INTO health_logs (id, volume_id, contact_id, session_id, logged_at, operation)
                  VALUES (808, 700, 806, 807, '2026-01-06 00:00:07', 'verify');
             INSERT INTO volume_deposits (id, volume_id, location_id, deposited_at)
                  VALUES (809, 700, 6, '2026-01-06 00:00:08');
             INSERT INTO volume_movements (id, volume_id, from_location, to_location, moved_at)
                  VALUES (810, 700, 5, 6, '2026-01-06 00:00:09');
             INSERT INTO restores (id, contact_id, volume_id, volume_label, unit_id, unit_name,
                                   version, kind, destination, started_at, finished_at,
                                   outcome, tapectl_version)
                  VALUES (811, 806, 700, 'V-SEALED', 500, 'u-active', 1, 'unit', '/restore',
                          '2026-01-06 00:00:10', '2026-01-06 00:00:11', 'ok', 'v');",
        )
        .unwrap();
    }

    /// THE test for migration 026 (issue #362): a populated schema-25
    /// database comes through with every row and every cell intact, every
    /// schema object outside the three rebuilt tables byte-identical, every
    /// index and foreign key restated, and a clean `foreign_key_check` --
    /// and afterwards the five dropped states are refused while every kept
    /// state is still accepted.
    #[test]
    fn test_migrate_025_populated_db_to_026_preserves_every_row_and_drops_five_states() {
        let mut conn = open_memory_at_version(25);
        assert_eq!(user_version(&conn), 25, "precondition: at schema 25");
        seed_schema_25(&conn);

        let rows_before = every_row(&conn);
        let objects_before = schema_objects(&conn);
        let tables: Vec<String> = rows_before.keys().cloned().collect();
        let fks_before: Vec<_> = tables.iter().map(|t| foreign_keys_of(&conn, t)).collect();
        let cols_before: Vec<_> = REBUILT_BY_026
            .iter()
            .map(|t| table_info(&conn, t))
            .collect();

        migrate_to(&mut conn, Some(26))
            .expect("026 must migrate a database carrying only kept states");
        assert_eq!(user_version(&conn), 26, "precondition: stopped at 026");

        // Every row, every cell, every table -- including the three rebuilt
        // ones, whose ids the copy must carry verbatim.
        let rows_after = every_row(&conn);
        assert_eq!(
            rows_before, rows_after,
            "026 must not add, drop, renumber or alter a single row anywhere"
        );
        assert!(
            rows_after["units"].len() == 3
                && rows_after["snapshots"].len() == 5
                && rows_after["volumes"].len() == 6,
            "positive control: the seed really populated the rebuilt tables"
        );

        // Every schema object outside the three rebuilt tables is
        // byte-identical (012's rename trap would rewrite other tables'
        // REFERENCES clauses), and the rebuilt tables' indexes come back
        // with identical SQL.
        let objects_after = schema_objects(&conn);
        let outside = |objs: &[(String, String, String, Option<String>)]| {
            objs.iter()
                .filter(|o| !(o.0 == "table" && REBUILT_BY_026.contains(&o.1.as_str())))
                .cloned()
                .collect::<Vec<_>>()
        };
        assert_eq!(
            outside(&objects_before),
            outside(&objects_after),
            "026 changed a schema object other than the three tables it rebuilds, or \
             failed to recreate one of their indexes exactly"
        );
        for table in REBUILT_BY_026 {
            assert!(
                !index_names(&conn, table).is_empty(),
                "positive control: {table} has indexes to compare"
            );
        }

        // Every foreign key of every table, in and out of the rebuilt ones
        // (all 17 inbound edges live on the referencing tables).
        let fks_after: Vec<_> = tables.iter().map(|t| foreign_keys_of(&conn, t)).collect();
        assert_eq!(fks_before, fks_after, "026 must restate every foreign key");
        let fk_violations: i64 = conn
            .query_row("SELECT COUNT(*) FROM pragma_foreign_key_check", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(fk_violations, 0, "PRAGMA foreign_key_check after 026");
        let report = crate::cli::operations::db_fsck(&conn, false, false).unwrap();
        assert!(report.integrity_ok, "integrity_check after 026");
        assert!(
            report.issues.is_empty(),
            "db fsck must be clean after 026: {:?}",
            report.issues
        );

        // Columns: only `volumes.status` changes, and only by losing its
        // DEFAULT ('blank' was it). NOT NULL with no default: every insert
        // must state a status.
        for (table, before) in REBUILT_BY_026.iter().zip(cols_before) {
            let after = table_info(&conn, table);
            let expected: Vec<_> = before
                .into_iter()
                .map(|c| {
                    if *table == "volumes" && c.0 == "status" {
                        assert_eq!(c.3.as_deref(), Some("'blank'"), "precondition");
                        (c.0, c.1, c.2, None, c.4)
                    } else {
                        c
                    }
                })
                .collect();
            assert_eq!(after, expected, "026 changed a column of {table}");
        }
        let no_status = conn.execute(
            "INSERT INTO volumes (label, backend_type, backend_name, capacity_bytes)
             VALUES ('V-NOSTATUS', 'lto', 'lto0', 1000)",
            [],
        );
        assert!(
            no_status.is_err(),
            "volumes.status has no DEFAULT after 026: an insert must state one"
        );

        // FK enforcement is back on and bites on a rebuilt parent.
        assert!(
            conn.execute("DELETE FROM units WHERE id = 500", [])
                .is_err(),
            "units 500 is referenced; FK enforcement must refuse the delete"
        );
        assert!(
            conn.execute("DELETE FROM volumes WHERE id = 700", [])
                .is_err(),
            "volumes 700 is referenced; FK enforcement must refuse the delete"
        );
        // And the FTS triggers on `files` still index.
        let hits: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM files_fts WHERE files_fts MATCH 'needle'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(hits, 1, "files_fts must still find the seeded file");

        // The five dropped states are refused; every kept state is still
        // accepted (the positive control for each table).
        let set = |table: &str, id: i64, status: &str| {
            conn.execute(
                &format!("UPDATE {table} SET status = ?1 WHERE id = ?2"),
                rusqlite::params![status, id],
            )
        };
        for (table, id, dropped) in [
            ("units", 502, "retired"),
            ("snapshots", 602, "superseded"),
            ("snapshots", 602, "failed"),
            ("volumes", 701, "blank"),
            ("volumes", 701, "missing"),
        ] {
            let err = set(table, id, dropped).expect_err(&format!(
                "{table}.status = '{dropped}' must be refused after 026"
            ));
            assert!(
                err.to_string().contains("CHECK constraint failed"),
                "{table}.status = '{dropped}' must fail the CHECK, got: {err}"
            );
        }
        for status in ["active", "tape_only", "missing"] {
            set("units", 502, status)
                .unwrap_or_else(|e| panic!("units.status '{status}' must stay legal: {e}"));
        }
        for status in ["created", "staged", "current", "reclaimable", "purged"] {
            set("snapshots", 602, status)
                .unwrap_or_else(|e| panic!("snapshots.status '{status}' must stay legal: {e}"));
        }
        for status in [
            "initialized",
            "active",
            "full",
            "retired",
            "erased",
            "sealed",
        ] {
            set("volumes", 701, status)
                .unwrap_or_else(|e| panic!("volumes.status '{status}' must stay legal: {e}"));
        }
    }

    /// Issue #362, P2: no code has ever written any of the five dropped
    /// states, so a row carrying one was put there by hand -- and 026 must
    /// not guess what it should have been. It refuses, loudly, naming the
    /// table and the state, and changes nothing. The message must be the
    /// migration's own words, not SQLite's bare "CHECK constraint failed"
    /// and not `rusqlite_migration`'s dump of the whole 026 script.
    #[test]
    fn test_migration_026_refuses_a_row_in_a_dropped_state_by_name() {
        for (table, id, state) in [
            ("units", 502, "retired"),
            ("snapshots", 602, "superseded"),
            ("snapshots", 602, "failed"),
            ("volumes", 701, "blank"),
            ("volumes", 701, "missing"),
        ] {
            let mut conn = open_memory_at_version(25);
            seed_schema_25(&conn);
            conn.execute(
                &format!("UPDATE {table} SET status = ?1 WHERE id = ?2"),
                rusqlite::params![state, id],
            )
            .unwrap_or_else(|e| panic!("precondition: schema 25 admits '{state}': {e}"));

            let err = migrate(&mut conn)
                .expect_err(&format!("026 must refuse a {table} row in '{state}'"));
            let msg = match &err {
                TapectlError::Migration(m) => m.clone(),
                other => panic!(
                    "a dropped-state row is not an FK problem `db fsck --repair` can fix; \
                     it must be the generic Migration variant, got {other:?}"
                ),
            };
            assert!(
                msg.contains(&format!("{table}.status = '{state}'")),
                "the refusal must name the table and the state: {msg}"
            );
            assert!(
                msg.contains(&format!("id {id}")),
                "the refusal must name the row: {msg}"
            );
            assert!(
                !msg.contains("CREATE TABLE") && !msg.contains("CHECK constraint failed"),
                "the refusal must be 026's own words, not the SQL dump or a bare \
                 constraint failure: {msg}"
            );

            // Nothing moved: still schema 25, the row still says what it said.
            assert_eq!(user_version(&conn), 25, "a refused 026 must roll back");
            let still: String = conn
                .query_row(
                    &format!("SELECT status FROM {table} WHERE id = ?1"),
                    [id],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(still, state, "a refused 026 must not rewrite the row");
        }
    }

    /// Every offender, in every table, is named in the ONE refusal -- an
    /// operator fixing rows by hand should not have to discover them one
    /// failed open at a time. Positive control: the same seed with no
    /// offender migrates (the populated test above).
    #[test]
    fn test_migration_026_refusal_names_every_offending_table_and_state() {
        let mut conn = open_memory_at_version(25);
        seed_schema_25(&conn);
        conn.execute_batch(
            "UPDATE units SET status = 'retired' WHERE id IN (501, 502);
             UPDATE snapshots SET status = 'superseded' WHERE id = 602;
             UPDATE snapshots SET status = 'failed' WHERE id = 603;
             UPDATE volumes SET status = 'blank' WHERE id = 701;
             UPDATE volumes SET status = 'missing' WHERE id = 702;",
        )
        .unwrap();
        let err = migrate(&mut conn).expect_err("026 must refuse");
        let msg = err.to_string();
        for needle in [
            "units.status = 'retired'",
            "snapshots.status = 'superseded'",
            "snapshots.status = 'failed'",
            "volumes.status = 'blank'",
            "volumes.status = 'missing'",
            "2 row(s)",
        ] {
            assert!(msg.contains(needle), "refusal must name {needle:?}: {msg}");
        }
        assert_eq!(user_version(&conn), 25);
    }

    // --- Issue #233: an orphan blocks ordinary open; repair must still run ---

    /// Build a FILE-backed (not `:memory:`) database at exactly the 002
    /// schema — one migration short of 003, the first
    /// `.foreign_key_check()`-decorated migration — carrying one orphan
    /// row planted the way #104's fsck tests do: FK enforcement OFF for
    /// the insert, back ON afterward (re-enabling the pragma does not
    /// retroactively validate rows already there). File-backed because the
    /// whole point of these tests is reopening it as a fresh connection,
    /// the way a real process invocation does — `open`/`open_for_repair`
    /// both take a `&Path`.
    fn write_orphaned_pre_003_db(db_path: &std::path::Path) {
        let mut conn = Connection::open(db_path).unwrap();
        configure(&conn).unwrap();
        Migrations::new(vec![
            M::up(include_str!("migrations/001_initial.sql")),
            M::up(include_str!("migrations/002_fts5_catalog.sql")),
        ])
        .to_latest(&mut conn)
        .unwrap();

        conn.pragma_update(None, "foreign_keys", "OFF").unwrap();
        conn.execute(
            "INSERT INTO units (uuid, name, tenant_id, checksum_mode, encrypt, status)
             VALUES ('u-orphan', 'orphan-unit', 99999, 'mtime_size', 1, 'active')",
            [],
        )
        .unwrap();
        conn.pragma_update(None, "foreign_keys", "ON").unwrap();
    }

    /// THE shape this whole fix depends on, verified directly per issue
    /// #233's own instruction ("verify this shape works before building on
    /// it"): a database that never migrated past 002 and already carries a
    /// dangling `units.tenant_id` makes ordinary `open()` fail outright —
    /// migration 003's whole-database FK check finds it the instant it
    /// runs — and `open()` must name this specific, repairable condition
    /// (`DatabaseNeedsRepair`) rather than the generic `Migration` variant.
    /// `open_for_repair` must nonetheless be able to open that same file,
    /// and `cli::operations::db_fsck(..., true)` must repair it — proving
    /// the repair mechanics (`pragma_foreign_key_check` /
    /// `defer_foreign_keys`) are schema-agnostic and do not depend on being
    /// at the latest migration. The very next ordinary `open()` must then
    /// migrate the now-clean database all the way to head.
    #[test]
    fn issue_233_repair_can_open_and_fix_a_database_ordinary_open_refuses() {
        let tmp = tempfile::TempDir::new().unwrap();
        let db_path = tmp.path().join("orphaned.db");
        write_orphaned_pre_003_db(&db_path);

        let open_err = open(&db_path).expect_err(
            "a pre-existing orphan must block ordinary open — this is the defect's premise, \
             not the part under test",
        );
        assert!(
            matches!(open_err, TapectlError::DatabaseNeedsRepair(_)),
            "open() must name this specific, repairable condition rather than a generic \
             migration failure: got {open_err:?}"
        );

        let repair_conn = open_for_repair(&db_path).expect(
            "db fsck --repair must be able to open a database ordinary open refuses, or \
             repair can never run",
        );
        assert!(
            !schema_is_current(&repair_conn).unwrap(),
            "the repair connection must still read as behind head — it never migrated"
        );
        let report = crate::cli::operations::db_fsck(&repair_conn, true, false)
            .expect("repair must succeed against the unmigrated (002) schema");
        assert_eq!(
            report.repaired, 1,
            "exactly the one planted orphan row should have been deleted"
        );
        drop(repair_conn);

        // After repair, ordinary open must migrate the now-clean database
        // all the way to head.
        let conn = open(&db_path)
            .expect("after repair, ordinary open must migrate the now-clean database to head");
        assert!(schema_is_current(&conn).unwrap());
        assert!(
            table_info(&conn, "volumes")
                .iter()
                .any(|(name, ..)| name == "observed_condition"),
            "migration must have run all the way through 017"
        );
    }

    /// `db::open`'s error for a genuinely corrupt schema (as opposed to a
    /// repairable orphan) must stay the generic `Migration` variant, never
    /// `DatabaseNeedsRepair` — issue #233's own trap: matching every
    /// migration failure the same way would hand an operator with a broken
    /// schema misleading advice about orphan rows.
    #[test]
    fn issue_233_a_non_fk_migration_failure_is_not_reported_as_repairable() {
        let tmp = tempfile::TempDir::new().unwrap();
        let db_path = tmp.path().join("corrupt.db");

        // A schema-version claim of 1 with nothing actually migrated: the
        // next `to_latest()` tries to run migration 002's SQL against a
        // database that never ran 001, which fails on a missing table --
        // a real migration-definition problem, not a foreign key.
        {
            let conn = Connection::open(&db_path).unwrap();
            conn.pragma_update(None, "user_version", 1).unwrap();
        }

        let err = open(&db_path).expect_err("a corrupt/inconsistent schema must fail to open");
        assert!(
            matches!(err, TapectlError::Migration(_)),
            "a non-FK migration failure must stay the generic variant, not \
             DatabaseNeedsRepair: got {err:?}"
        );
    }

    // --- Migration 027 (issues #372, #373) ---

    /// Seed a schema-26 catalog shaped like production: `seed_schema_25`'s
    /// row in every table, then `snapshots` × `per` files, each mirrored in
    /// `manifests`/`manifest_entries` exactly as `snapshot create` and the
    /// stage backfill wrote them before 027 (every other file baselined on
    /// both sides). Returns the number of `files` rows.
    fn seed_schema_26_catalog(conn: &Connection, snapshots: i64, per: i64) -> i64 {
        seed_schema_25(conn);
        assert_eq!(user_version(conn), 26, "precondition: at schema 26");
        // seed_schema_25's 703 is 'full', legal at 26 and refused by 032;
        // a catalog these tests carry to the head must not hold one.
        conn.execute("UPDATE volumes SET status = 'sealed' WHERE id = 703", [])
            .unwrap();
        let tx = conn.unchecked_transaction().unwrap();
        for s in 0..snapshots {
            let sid = 10_000 + s;
            tx.execute(
                "INSERT INTO snapshots (id, unit_id, version, status, source_path, file_count)
                 VALUES (?1, 500, ?2, 'staged', '/src/a', ?3)",
                rusqlite::params![sid, 100 + s, per],
            )
            .unwrap();
            tx.execute(
                "INSERT INTO manifests (id, snapshot_id) VALUES (?1, ?1)",
                [sid],
            )
            .unwrap();
            for f in 0..per {
                let path = format!("album{s}/photo_{f:05}.jpg");
                let sha = (f % 2 == 0).then(|| format!("{:064x}", sid * 100_000 + f));
                tx.execute(
                    "INSERT INTO files (snapshot_id, path, size_bytes, sha256, modified_at,
                                        is_directory, file_type)
                     VALUES (?1, ?2, ?3, ?4, '2026-01-01T00:00:00+00:00', 0, 'regular')",
                    rusqlite::params![sid, path, f, sha],
                )
                .unwrap();
                tx.execute(
                    "INSERT INTO manifest_entries (manifest_id, path, size_bytes, mtime, sha256,
                                                   is_directory, mode, uid, gid, file_type)
                     VALUES (?1, ?2, ?3, '2026-01-01T00:00:00Z', ?4, 0, 420, 1000, 1000,
                             'regular')",
                    rusqlite::params![sid, path, f, sha],
                )
                .unwrap();
            }
        }
        tx.commit().unwrap();
        conn.query_row("SELECT COUNT(*) FROM files", [], |r| r.get(0))
            .unwrap()
    }

    /// `seed_schema_25` inserts against the schema-25 column list and only
    /// kept states, which 026 accepts, so it seeds a schema-26 database too.
    fn open_memory_at_026_seeded(snapshots: i64, per: i64) -> (Connection, i64) {
        let conn = open_memory_at_version(26);
        let n = seed_schema_26_catalog(&conn, snapshots, per);
        (conn, n)
    }

    fn search(conn: &Connection, fts: &str) -> Vec<String> {
        conn.prepare(
            "SELECT f.path FROM files_fts fts JOIN files f ON f.rowid = fts.rowid
             WHERE files_fts MATCH ?1 ORDER BY f.path",
        )
        .unwrap()
        .query_map([fts], |r| r.get(0))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap()
    }

    /// THE test for 027: a populated schema-26 catalog (a few thousand
    /// `files` rows mirrored in `manifest_entries`) migrates with every row
    /// of every surviving table intact, the two manifest tables and the
    /// eight dead indexes gone, and FTS search answering as before.
    #[test]
    fn test_migrate_026_populated_catalog_to_027_keeps_every_file_and_search() {
        let (mut conn, files_before) = open_memory_at_026_seeded(12, 300);
        assert!(
            files_before > 3_000,
            "positive control: {files_before} rows"
        );
        let rows_before = every_row(&conn);
        let hits_before = search(&conn, "00042*");
        assert_eq!(
            hits_before.len(),
            12,
            "positive control: one hit per snapshot"
        );

        migrate_to(&mut conn, Some(27))
            .expect("027 must migrate a catalog whose manifest mirrors files");
        assert_eq!(user_version(&conn), 27);

        let mut expected = rows_before;
        assert!(expected.remove("manifests").is_some_and(|r| r.len() == 13));
        assert!(expected
            .remove("manifest_entries")
            .is_some_and(|r| r.len() as i64 == files_before - 1));
        assert_eq!(
            expected,
            every_row(&conn),
            "027 must not add, drop, renumber or alter a row of any table it keeps \
             (the FTS shadow tables included)"
        );
        for gone in ["manifests", "manifest_entries"] {
            assert!(table_info(&conn, gone).is_empty(), "{gone} must be dropped");
        }

        assert_eq!(search(&conn, "00042*"), hits_before);
        let fts_check = conn.execute(
            "INSERT INTO files_fts(files_fts) VALUES('integrity-check')",
            [],
        );
        assert!(fts_check.is_ok(), "FTS index consistent: {fts_check:?}");

        assert_eq!(
            index_names(&conn, "files"),
            vec!["sqlite_autoindex_files_1"]
        );
        let report = crate::cli::operations::db_fsck(&conn, false, false).unwrap();
        assert!(
            report.integrity_ok && report.issues.is_empty(),
            "{:?}",
            report.issues
        );
    }

    /// Issue #373's migration-replay pin: no single-column status index
    /// survives 001 through the latest migration, and neither do the two
    /// dead `files` indexes (#372).
    #[test]
    fn test_no_status_index_survives_the_migration_chain() {
        let conn = open_memory().unwrap();
        let survivors: Vec<String> = conn
            .prepare(
                "SELECT name FROM sqlite_master WHERE type = 'index'
                 AND (name LIKE 'idx\\_%\\_status' ESCAPE '\\'
                      OR name IN ('idx_files_path', 'idx_files_snapshot',
                                  'idx_manifest_entries_manifest'))",
            )
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        assert!(survivors.is_empty(), "dead indexes survived: {survivors:?}");

        // Positive control: the same query sees them at schema 26.
        let at_26 = open_memory_at_version(26);
        let n: i64 = at_26
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type = 'index'
                 AND name LIKE 'idx\\_%\\_status' ESCAPE '\\'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(n, 6, "the six status indexes exist before 027");
    }

    /// `files_au` fires on a path change only: a sha256 backfill leaves the
    /// FTS index alone, and a rename still reaches search.
    #[test]
    fn test_migration_027_fts_update_trigger_fires_on_path_only() {
        // Pinned to schema 27: 030 replaced `files` and its triggers.
        let conn = open_memory_at_version(27);
        let sql: String = conn
            .query_row(
                "SELECT sql FROM sqlite_master WHERE type = 'trigger' AND name = 'files_au'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert!(sql.contains("AFTER UPDATE OF path ON files"), "{sql}");

        conn.execute_batch(
            "INSERT INTO tenants (id, name, is_operator, status) VALUES (1, 't', 1, 'active');
             INSERT INTO units (id, uuid, name, tenant_id) VALUES (1, 'u', 'u', 1);
             INSERT INTO snapshots (id, unit_id, version, source_path) VALUES (1, 1, 1, '/s');
             INSERT INTO files (snapshot_id, path, size_bytes) VALUES (1, 'old/name.txt', 1);",
        )
        .unwrap();
        let fts_rows = |c: &Connection| -> i64 {
            c.query_row("SELECT COUNT(*) FROM files_fts_docsize", [], |r| r.get(0))
                .unwrap()
        };
        let before = every_row(&conn)["files_fts_data"].clone();
        conn.execute("UPDATE files SET sha256 = 'ab' WHERE snapshot_id = 1", [])
            .unwrap();
        assert_eq!(
            every_row(&conn)["files_fts_data"],
            before,
            "a sha256-only UPDATE must not touch the FTS index"
        );
        assert_eq!(fts_rows(&conn), 1);

        conn.execute(
            "UPDATE files SET path = 'new/place.txt' WHERE snapshot_id = 1",
            [],
        )
        .unwrap();
        assert!(search(&conn, "old*").is_empty());
        assert_eq!(search(&conn, "place*"), vec!["new/place.txt"]);
    }

    /// The guard, finding (a): an entry with no `files` row is refused by
    /// id, and nothing changes.
    #[test]
    fn test_migration_027_refuses_a_manifest_path_files_lacks() {
        let (mut conn, _) = open_memory_at_026_seeded(1, 3);
        conn.execute(
            "INSERT INTO manifest_entries (id, manifest_id, path, size_bytes, mtime)
             VALUES (77001, 10000, 'only/in/manifest.txt', 1, '2026-01-01T00:00:00Z')",
            [],
        )
        .unwrap();
        let err = migrate(&mut conn).expect_err("027 must refuse");
        let msg = match &err {
            TapectlError::Migration(m) => m.clone(),
            other => panic!("expected the generic Migration variant, got {other:?}"),
        };
        assert!(msg.starts_with("migration 027 cannot run: "), "{msg}");
        assert!(
            msg.contains("1 manifest_entries row(s) with no files row"),
            "{msg}"
        );
        assert!(msg.contains("77001"), "{msg}");
        assert!(msg.contains("Nothing has been changed."), "{msg}");
        assert!(
            !msg.contains("DROP TABLE"),
            "the script must not be dumped: {msg}"
        );
        assert_eq!(user_version(&conn), 26, "rolled back");
        assert!(
            !table_info(&conn, "manifest_entries").is_empty(),
            "rolled back"
        );
    }

    /// The guard, finding (b): a baseline only the manifest holds.
    #[test]
    fn test_migration_027_refuses_a_sha256_only_the_manifest_holds() {
        let (mut conn, _) = open_memory_at_026_seeded(1, 3);
        // photo_00001 is un-baselined on both sides by the seed.
        conn.execute(
            "UPDATE manifest_entries SET sha256 = 'feed' WHERE path = 'album0/photo_00001.jpg'",
            [],
        )
        .unwrap();
        let id: i64 = conn
            .query_row(
                "SELECT id FROM manifest_entries WHERE path = 'album0/photo_00001.jpg'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        let msg = migrate(&mut conn).expect_err("027 must refuse").to_string();
        assert!(
            msg.contains("1 manifest_entries row(s) carrying a sha256"),
            "{msg}"
        );
        assert!(msg.contains(&format!("(id {id})")), "{msg}");
        assert_eq!(user_version(&conn), 26, "rolled back");
    }

    /// `db::open` VACUUMs once after applying 027 to an existing catalog, so
    /// the dropped tables' pages go back to the filesystem.
    #[test]
    fn test_open_vacuums_once_when_it_applies_027() {
        let tmp = tempfile::TempDir::new().unwrap();
        let path = tmp.path().join("tapectl.db");
        let size_before = {
            let mut conn = Connection::open(&path).unwrap();
            configure(&conn).unwrap();
            migrate_to(&mut conn, Some(26)).unwrap();
            seed_schema_26_catalog(&conn, 4, 500);
            conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE)")
                .unwrap();
            let pages: i64 = conn
                .query_row("PRAGMA page_count", [], |r| r.get(0))
                .unwrap();
            pages
        };

        let conn = open(&path).unwrap();
        let latest = user_version(&open_memory().unwrap());
        assert!(latest >= 28);
        assert_eq!(
            user_version(&conn),
            latest,
            "027, then 028 (issue #386) and every later migration"
        );
        let freelist: i64 = conn
            .query_row("PRAGMA freelist_count", [], |r| r.get(0))
            .unwrap();
        let pages: i64 = conn
            .query_row("PRAGMA page_count", [], |r| r.get(0))
            .unwrap();
        assert_eq!(freelist, 0, "VACUUM leaves no free pages");
        assert!(
            pages < size_before,
            "the dropped tables' pages were returned: {pages} >= {size_before}"
        );
    }

    // --- Migration 030 (issues #380, #381): paths + file_versions ---

    /// A schema-29 catalog with every shape a `files` row takes in
    /// production: a directory, a hashed regular file, an unhashed one, a
    /// symlink with its target, the NULL `file_type` rows a pre-#381 rebuild
    /// wrote (a file and a directory), the same path in two versions of one
    /// unit, and the same path in another unit. Returns the rows as
    /// `(unit, version, path, file_type after 030, size, sha256, modified_at,
    /// link_target)`, in the old vocabulary.
    #[allow(clippy::type_complexity)]
    fn seed_schema_29_files(
        conn: &Connection,
    ) -> Vec<(
        String,
        i64,
        String,
        String,
        i64,
        Option<String>,
        Option<String>,
        Option<String>,
    )> {
        assert_eq!(user_version(conn), 29, "precondition: at schema 29");
        conn.execute_batch(
            "INSERT INTO tenants (id, name, is_operator, status) VALUES (1, 't', 1, 'active');
             INSERT INTO units (id, uuid, name, tenant_id) VALUES (10, 'u10', 'alpha', 1);
             INSERT INTO units (id, uuid, name, tenant_id) VALUES (11, 'u11', 'bravo', 1);
             INSERT INTO snapshots (id, unit_id, version, source_path) VALUES (20, 10, 1, '/a');
             INSERT INTO snapshots (id, unit_id, version, source_path) VALUES (21, 10, 2, '/a');
             INSERT INTO snapshots (id, unit_id, version, source_path) VALUES (22, 11, 1, '/b');",
        )
        .unwrap();
        let hash = "0123456789abcdef".repeat(4);
        let rows: Vec<(
            i64,
            &str,
            i64,
            Option<&str>,
            i64,
            Option<&str>,
            Option<&str>,
            Option<&str>,
        )> = vec![
            (
                20,
                "docs",
                1,
                Some("dir"),
                0,
                None,
                Some("2026-09-01T12:00:00+00:00"),
                None,
            ),
            (
                20,
                "docs/a.txt",
                0,
                Some("regular"),
                10,
                Some(&hash),
                Some("2026-09-01T12:00:01+00:00"),
                None,
            ),
            (
                20,
                "link",
                0,
                Some("symlink"),
                10,
                None,
                Some("1969-12-31T23:59:59+00:00"),
                Some("docs/a.txt"),
            ),
            (20, "old.bin", 0, None, 7, None, None, None),
            (20, "olddir", 1, None, 0, None, None, None),
            (
                21,
                "docs",
                1,
                Some("dir"),
                0,
                None,
                Some("2026-09-01T12:00:00+00:00"),
                None,
            ),
            (
                21,
                "docs/a.txt",
                0,
                Some("regular"),
                11,
                None,
                Some("2026-09-02T00:00:00+00:00"),
                None,
            ),
            (
                22,
                "docs/a.txt",
                0,
                Some("special"),
                0,
                None,
                Some("2026-09-03T00:00:00+00:00"),
                None,
            ),
        ];
        for (sid, path, is_dir, ft, size, sha, mtime, link) in &rows {
            conn.execute(
                "INSERT INTO files (snapshot_id, path, size_bytes, sha256, modified_at,
                                    is_directory, file_type, link_target)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                rusqlite::params![sid, path, size, sha, mtime, is_dir, ft, link],
            )
            .unwrap();
        }
        rows.into_iter()
            .map(|(sid, path, is_dir, ft, size, sha, mtime, link)| {
                let (unit, version) = match sid {
                    20 => ("alpha", 1),
                    21 => ("alpha", 2),
                    _ => ("bravo", 1),
                };
                let ft = ft.unwrap_or(if is_dir == 1 { "dir" } else { "regular" });
                (
                    unit.to_string(),
                    version,
                    path.to_string(),
                    ft.to_string(),
                    size,
                    sha.map(str::to_string),
                    mtime.map(str::to_string),
                    link.map(str::to_string),
                )
            })
            .collect()
    }

    /// THE test for 030: every `files` row comes through as exactly one
    /// `file_versions` row whose values convert back to the original text,
    /// each path is stored once per unit, the search index holds distinct
    /// paths, and the old tables are gone.
    #[test]
    fn test_migrate_029_catalog_to_030_converts_every_file_row_exactly() {
        let mut conn = open_memory_at_version(29);
        let expected = seed_schema_29_files(&conn);

        migrate_to(&mut conn, Some(30)).expect("030 must convert this catalog");
        assert_eq!(user_version(&conn), 30);

        let mut got: Vec<_> = conn
            .prepare(
                "SELECT u.name, s.version, p.path, fv.kind, fv.size_bytes, fv.sha256,
                        fv.mtime_ns, fv.link_target
                 FROM file_versions fv
                 JOIN paths p ON p.id = fv.path_id
                 JOIN snapshots s ON s.id = fv.snapshot_id
                 JOIN units u ON u.id = s.unit_id AND u.id = p.unit_id",
            )
            .unwrap()
            .query_map([], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, i64>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, files::FileKind>(3)?.as_str().to_string(),
                    r.get::<_, i64>(4)?,
                    files::sha256_column(r.get(5)?),
                    r.get::<_, Option<i64>>(6)?
                        .map(|ns| files::mtime_ns_to_rfc3339(ns).unwrap()),
                    r.get::<_, Option<String>>(7)?,
                ))
            })
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        got.sort();
        let mut want = expected;
        want.sort();
        assert_eq!(got, want, "every row, converted back, equals the original");

        let paths: i64 = conn
            .query_row("SELECT COUNT(*) FROM paths", [], |r| r.get(0))
            .unwrap();
        assert_eq!(
            paths, 6,
            "5 distinct paths in alpha's two versions, 1 in bravo"
        );
        let txt: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM paths_fts WHERE paths_fts MATCH 'txt'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(txt, 2, "one hit per unit's distinct path, not per version");
        conn.execute(
            "INSERT INTO paths_fts(paths_fts) VALUES('integrity-check')",
            [],
        )
        .expect("the search index is consistent");

        for gone in ["files", "files_fts"] {
            assert!(table_info(&conn, gone).is_empty(), "{gone} must be dropped");
        }
        let report = crate::cli::operations::db_fsck(&conn, false, false).unwrap();
        assert!(
            report.integrity_ok && report.issues.is_empty(),
            "{:?}",
            report.issues
        );

        // A new path reaches search through the trigger; a path is never
        // changed in place.
        files::fixture::insert(&conn, 21, "new/zebra.txt", 1, "regular", None);
        let zebra: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM paths_fts WHERE paths_fts MATCH 'zebra'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(zebra, 1);
        assert!(conn
            .execute("UPDATE paths SET path = 'renamed' WHERE path = 'link'", [])
            .is_err());
    }

    /// Each value 030 cannot convert exactly is refused by row id, and the
    /// catalog is left at schema 29 with `files` intact.
    #[test]
    fn test_migration_030_refuses_what_it_cannot_convert_exactly() {
        for (column_sql, needle) in [
            ("sha256 = 'abc'", "sha256 is not 64 lowercase hex"),
            ("sha256 = upper(sha256)", "sha256 is not 64 lowercase hex"),
            (
                "modified_at = '2026-09-01T12:00:01Z'",
                "modified_at is not YYYY-MM-DDTHH:MM:SS+00:00",
            ),
            (
                "modified_at = '2026-09-01 12:00:01'",
                "modified_at is not YYYY-MM-DDTHH:MM:SS+00:00",
            ),
            // A year past 9999 in chrono's signed spelling is not refused
            // (ADR-0012 amendment 2026-10-07 item 9): see
            // `test_migration_030_writes_null_for_an_mtime_no_nanosecond_count_holds`.
            // A signed year in any other spelling still is.
            (
                "modified_at = '+10000-01-01T00:00:00Z'",
                "modified_at is not YYYY-MM-DDTHH:MM:SS+00:00",
            ),
            ("file_type = 'fifo'", "is_directory/file_type"),
            ("file_type = 'dir'", "is_directory/file_type"),
        ] {
            let mut conn = open_memory_at_version(29);
            seed_schema_29_files(&conn);
            let id: i64 = conn
                .query_row(
                    "SELECT id FROM files WHERE snapshot_id = 20 AND path = 'docs/a.txt'",
                    [],
                    |r| r.get(0),
                )
                .unwrap();
            conn.execute(
                &format!("UPDATE files SET {column_sql} WHERE id = ?1"),
                [id],
            )
            .unwrap();

            let err = migrate(&mut conn).expect_err(&format!("030 must refuse {column_sql}"));
            let msg = match &err {
                TapectlError::Migration(m) => m.clone(),
                other => panic!("expected the generic Migration variant, got {other:?}"),
            };
            assert!(msg.starts_with("migration 030 cannot run: "), "{msg}");
            assert!(msg.contains(needle), "{column_sql}: {msg}");
            assert!(msg.contains(&format!("(id {id})")), "{column_sql}: {msg}");
            assert!(msg.contains("Nothing has been changed."), "{msg}");
            // Deleting a non-directory row of a version with a file_count
            // makes that version unstageable (staging's file-list check)
            // and reads the file as added to the next `snapshot create`:
            // the remedy offered is a correction, never a delete.
            assert!(!msg.contains("delete each"), "{msg}");
            assert!(
                msg.contains(
                    "Correct each named files row (a sha256 or modified_at you cannot \
                     recover may be set to NULL; delete only a row whose snapshot no \
                     longer exists)"
                ),
                "{msg}"
            );
            assert_eq!(user_version(&conn), 29, "rolled back");
            let rows: i64 = conn
                .query_row("SELECT COUNT(*) FROM files", [], |r| r.get(0))
                .unwrap();
            assert_eq!(rows, 8, "files intact");
        }
    }

    /// A `MakeWriter` over a shared buffer, so a test can read what a
    /// `tracing` event printed.
    #[derive(Clone, Default)]
    struct CapturedLog(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);

    impl std::io::Write for CapturedLog {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl CapturedLog {
        /// Run `f` with every `tracing` event at WARN and above written here.
        fn capture<T>(&self, f: impl FnOnce() -> T) -> T {
            let writer = self.clone();
            let subscriber = tracing_subscriber::fmt()
                .with_ansi(false)
                .with_max_level(tracing::Level::WARN)
                .with_writer(move || writer.clone())
                .finish();
            tracing::subscriber::with_default(subscriber, f)
        }

        fn text(&self) -> String {
            String::from_utf8(self.0.lock().unwrap().clone()).unwrap()
        }
    }

    /// ADR-0012 amendment 2026-10-07, item 9: a `modified_at` in the walk's
    /// own spelling but outside 1677-09-21..2262-04-11, which no i64 count
    /// of nanoseconds holds, converts to a NULL `mtime_ns` -- what the walk
    /// and the rebuild record for the same file -- and the migration warns,
    /// naming the row, instead of refusing. So does a year past 9999 in the
    /// signed spelling chrono's `to_rfc3339` gave it before 030, which
    /// SQLite's date functions do not read at all. Every other value
    /// converts as before.
    #[test]
    fn test_migration_030_writes_null_for_an_mtime_no_nanosecond_count_holds() {
        for far_off in [
            "2300-01-01T00:00:00+00:00",
            "1601-01-01T00:00:00+00:00",
            "+10000-01-01T00:00:00+00:00",
        ] {
            let mut conn = open_memory_at_version(29);
            seed_schema_29_files(&conn);
            let id: i64 = conn
                .query_row(
                    "SELECT id FROM files WHERE snapshot_id = 20 AND path = 'docs/a.txt'",
                    [],
                    |r| r.get(0),
                )
                .unwrap();
            conn.execute(
                "UPDATE files SET modified_at = ?1 WHERE id = ?2",
                rusqlite::params![far_off, id],
            )
            .unwrap();

            let log = CapturedLog::default();
            log.capture(|| migrate(&mut conn))
                .unwrap_or_else(|e| panic!("030 must convert {far_off}, got {e}"));
            assert!(user_version(&conn) >= 30);

            let mtime = |sid: i64, path: &str| -> Option<i64> {
                conn.query_row(
                    "SELECT fv.mtime_ns FROM file_versions fv JOIN paths p ON p.id = fv.path_id
                     WHERE fv.snapshot_id = ?1 AND p.path = ?2",
                    rusqlite::params![sid, path],
                    |r| r.get(0),
                )
                .unwrap()
            };
            assert_eq!(mtime(20, "docs/a.txt"), None, "{far_off} becomes NULL");
            // The positive control: the rows around it keep their mtimes.
            assert_eq!(
                mtime(20, "docs").map(files::mtime_ns_to_rfc3339),
                Some(Some("2026-09-01T12:00:00+00:00".to_string()))
            );
            assert_eq!(
                mtime(21, "docs/a.txt").map(files::mtime_ns_to_rfc3339),
                Some(Some("2026-09-02T00:00:00+00:00".to_string()))
            );

            let text = log.text();
            assert!(text.contains("WARN"), "{text}");
            assert!(text.contains("migration 030"), "{text}");
            assert!(text.contains(&format!("files row {id} ")), "{text}");
            assert!(text.contains(far_off), "{text}");
            assert!(text.contains("1677-09-21..2262-04-11"), "{text}");

            // The bookkeeping that carried the warning does not outlive it.
            let leftover: i64 = conn
                .query_row(
                    "SELECT COUNT(*) FROM sqlite_temp_master WHERE name LIKE 'm030%'",
                    [],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(leftover, 0);
        }

        // And a catalog with nothing to null migrates without a word.
        let mut conn = open_memory_at_version(29);
        seed_schema_29_files(&conn);
        let log = CapturedLog::default();
        log.capture(|| migrate(&mut conn)).unwrap();
        assert!(!log.text().contains("migration 030"), "{}", log.text());
    }

    /// The WARN names at most ten rows, then the count, as the refusals do:
    /// a catalog with thousands of NTFS-zero-time files prints one line.
    #[test]
    fn test_migration_030_warning_names_at_most_ten_rows() {
        let mut conn = open_memory_at_version(29);
        seed_schema_29_files(&conn);
        let mut ids = Vec::new();
        for i in 0..12 {
            conn.execute(
                "INSERT INTO files (snapshot_id, path, is_directory, file_type, size_bytes,
                                    modified_at)
                 VALUES (22, ?1, 0, 'regular', 1, '1601-01-01T00:00:00+00:00')",
                [format!("far/{i}")],
            )
            .unwrap();
            ids.push(conn.last_insert_rowid());
        }

        let log = CapturedLog::default();
        log.capture(|| migrate(&mut conn)).unwrap();
        let text = log.text();
        assert!(text.contains("for 12 file row(s)"), "{text}");
        for id in &ids[..10] {
            assert!(text.contains(&format!("files row {id} ")), "{text}");
        }
        for id in &ids[10..] {
            assert!(!text.contains(&format!("files row {id} ")), "{text}");
        }
        assert!(text.contains(", ... (12 rows). A walk"), "{text}");
        let nulled: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM file_versions WHERE snapshot_id = 22 AND mtime_ns IS NULL",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(nulled, 12, "every row is converted, not only the ten named");
    }

    /// A `files` row whose snapshot is gone has no unit to intern its path
    /// under: refused by id.
    #[test]
    fn test_migration_030_refuses_a_row_whose_snapshot_is_gone() {
        let mut conn = open_memory_at_version(29);
        seed_schema_29_files(&conn);
        conn.pragma_update(None, "foreign_keys", "OFF").unwrap();
        conn.execute(
            "INSERT INTO files (id, snapshot_id, path, size_bytes) VALUES (9001, 999, 'x', 1)",
            [],
        )
        .unwrap();
        conn.pragma_update(None, "foreign_keys", "ON").unwrap();
        let msg = migrate(&mut conn).unwrap_err().to_string();
        assert!(
            msg.contains("whose snapshot does not exist (id 9001)"),
            "{msg}"
        );
        assert_eq!(user_version(&conn), 29, "rolled back");
    }

    // --- Migration 032 (ADR-0012 amendment 2026-10-07 item 16) ---

    /// `seed_schema_25`'s catalog -- a volume in every status 026 kept, and a
    /// row in every table that references `volumes` -- carried through the
    /// real chain to schema 31, where 032 starts, plus a `phase_timings` row
    /// (028's reference into `volumes`, which the 25 seed predates).
    fn open_memory_at_031_seeded() -> Connection {
        let mut conn = open_memory_at_version(25);
        seed_schema_25(&conn);
        migrate_to(&mut conn, Some(31)).expect("seed_schema_25 migrates to 31");
        assert_eq!(user_version(&conn), 31, "precondition: at schema 31");
        conn.execute_batch(
            "INSERT INTO phase_timings (id, session, operation, volume_id, seq, phase,
                                        started_at, duration_ms, outcome)
                 VALUES (812, 's', 'volume verify', 700, 1, 'read', '2026-01-07', 1, 'ok');",
        )
        .unwrap();
        conn
    }

    /// THE test for 032: `volumes` is rebuilt without 'full' in its status
    /// CHECK, and nothing else moves -- every row of every table, every
    /// schema object outside `volumes`, both of its indexes, every foreign
    /// key in and out, and every column. Afterwards 'full' is refused and
    /// every kept status is still accepted.
    #[test]
    fn test_migrate_031_populated_db_to_032_preserves_every_row_and_drops_full() {
        let mut conn = open_memory_at_031_seeded();
        // 703 is seed_schema_25's 'full' volume, which 032 would refuse.
        conn.execute("UPDATE volumes SET status = 'sealed' WHERE id = 703", [])
            .unwrap();

        let rows_before = every_row(&conn);
        let objects_before = schema_objects(&conn);
        let tables: Vec<String> = rows_before.keys().cloned().collect();
        let fks_before: Vec<_> = tables.iter().map(|t| foreign_keys_of(&conn, t)).collect();
        let cols_before = table_info(&conn, "volumes");
        assert_eq!(
            index_names(&conn, "volumes"),
            vec![
                "idx_volumes_location",
                "idx_volumes_uuid",
                "sqlite_autoindex_volumes_1"
            ],
            "positive control: volumes has its indexes to compare"
        );

        migrate_to(&mut conn, Some(32)).expect("032 must migrate a catalog with no 'full' row");
        assert_eq!(user_version(&conn), 32);

        assert_eq!(
            rows_before,
            every_row(&conn),
            "032 must not add, drop, renumber or alter a single row anywhere"
        );
        assert_eq!(rows_before["volumes"].len(), 6, "positive control");
        let objects_after = schema_objects(&conn);
        let not_volumes = |objs: &[(String, String, String, Option<String>)]| {
            objs.iter()
                .filter(|o| !(o.0 == "table" && o.1 == "volumes"))
                .cloned()
                .collect::<Vec<_>>()
        };
        assert_eq!(
            not_volumes(&objects_before),
            not_volumes(&objects_after),
            "032 changed a schema object other than volumes, or did not recreate one of \
             its indexes exactly"
        );
        let fks_after: Vec<_> = tables.iter().map(|t| foreign_keys_of(&conn, t)).collect();
        assert_eq!(fks_before, fks_after, "032 must restate every foreign key");
        assert_eq!(
            cols_before,
            table_info(&conn, "volumes"),
            "no column changes"
        );
        let report = crate::cli::operations::db_fsck(&conn, false, false).unwrap();
        assert!(
            report.integrity_ok && report.issues.is_empty(),
            "{:?}",
            report.issues
        );
        assert!(
            conn.execute("DELETE FROM volumes WHERE id = 700", [])
                .is_err(),
            "volumes 700 is referenced; FK enforcement must refuse the delete"
        );

        let set = |status: &str| {
            conn.execute(
                "UPDATE volumes SET status = ?1 WHERE id = 701",
                rusqlite::params![status],
            )
        };
        let err = set("full").expect_err("'full' must be refused after 032");
        assert!(err.to_string().contains("CHECK constraint failed"), "{err}");
        for status in ["initialized", "active", "retired", "erased", "sealed"] {
            set(status)
                .unwrap_or_else(|e| panic!("volumes.status '{status}' must stay legal: {e}"));
        }
        assert!(
            conn.execute(
                "INSERT INTO volumes (label, backend_type, backend_name, capacity_bytes)
                 VALUES ('V-NOSTATUS', 'lto', 'lto0', 1000)",
                [],
            )
            .is_err(),
            "volumes.status still has no DEFAULT"
        );
    }

    /// No release has written 'full', so a row carrying it was set by hand,
    /// and 032 refuses by name and row rather than guess (026's rule). The
    /// positive control is the test above: the same seed with 703 moved off
    /// 'full' migrates.
    #[test]
    fn test_migration_032_refuses_a_full_volume_by_name() {
        let mut conn = open_memory_at_031_seeded();
        conn.execute("UPDATE volumes SET status = 'full' WHERE id = 704", [])
            .unwrap();
        let err = migrate(&mut conn).expect_err("032 must refuse a 'full' volume");
        let msg = match &err {
            TapectlError::Migration(m) => m.clone(),
            other => panic!("expected the generic Migration variant, got {other:?}"),
        };
        assert!(msg.starts_with("migration 032 cannot run: "), "{msg}");
        assert!(
            msg.contains("volumes.status = 'full' on 2 row(s) (id 703, 704)"),
            "{msg}"
        );
        assert!(msg.contains("Nothing has been changed."), "{msg}");
        assert!(
            !msg.contains("CREATE TABLE") && !msg.contains("CHECK constraint failed"),
            "032's own words, not the SQL dump: {msg}"
        );
        assert_eq!(user_version(&conn), 31, "a refused 032 rolls back");
        let full: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM volumes WHERE status = 'full'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(full, 2, "a refused 032 rewrites no row");
    }

    // --- Migration 033 (ADR-0012 amendment 2026-10-07 item 35) ---

    /// The three `cartridges` columns no code ever wrote or read.
    const M033_DROPPED: [&str; 3] = ["total_bytes_written", "total_bytes_read", "error_history"];

    /// `open_memory_at_031_seeded`'s catalog carried on to schema 32, plus a
    /// second cartridge with every column 033 keeps set to a non-default
    /// value and an out-of-sequence id (a copy that dropped `id` would
    /// renumber it), shelved at a location and mounted on a volume.
    fn open_memory_at_032_seeded() -> Connection {
        let mut conn = open_memory_at_031_seeded();
        // 703 is seed_schema_25's 'full' volume, which 032 would refuse.
        conn.execute("UPDATE volumes SET status = 'sealed' WHERE id = 703", [])
            .unwrap();
        migrate_to(&mut conn, Some(32)).expect("the seed migrates to 32");
        assert_eq!(user_version(&conn), 32, "precondition: at schema 32");
        conn.execute_batch(
            "INSERT INTO cartridges (id, barcode, media_type, manufacturer, serial_number,
                                     tape_length_meters, nominal_capacity, status,
                                     total_load_count, first_use, last_use, location_id,
                                     created_at, notes, operator_serial)
                 VALUES (870, 'BC0870', 'LTO-6', 'HPE', 'MAM0870', 846, 2500000000000,
                         'pending_erase', 41, '2026-01-01 00:00:00', '2026-01-02 00:00:00',
                         5, '2025-12-31 00:00:00', 'note-870', 'OP0870');
             INSERT INTO cartridge_volumes (id, cartridge_id, volume_id, mounted_at)
                 VALUES (871, 870, 701, '2026-01-06 00:00:09');",
        )
        .unwrap();
        conn
    }

    fn cartridge_columns_kept(conn: &Connection) -> Vec<String> {
        table_info(conn, "cartridges")
            .into_iter()
            .map(|c| c.0)
            .filter(|name| !M033_DROPPED.contains(&name.as_str()))
            .collect()
    }

    /// THE test for 033: `cartridges` is rebuilt without the three dead
    /// columns, and nothing else moves -- every row of every other table,
    /// every kept cartridge cell, every schema object outside the
    /// `cartridges` table itself (its indexes recreated exactly), every
    /// foreign key in and out, and every kept column's type, default,
    /// NOT NULL and key. The status CHECK and the partial unique serial
    /// index still hold afterwards.
    #[test]
    fn test_migrate_032_populated_db_to_033_drops_the_dead_cartridge_columns() {
        let mut conn = open_memory_at_032_seeded();

        let kept = cartridge_columns_kept(&conn);
        let select_kept = format!("SELECT {} FROM cartridges ORDER BY id", kept.join(", "));
        let cartridge_rows = |conn: &Connection| -> Vec<Vec<String>> {
            let mut stmt = conn.prepare(&select_kept).unwrap();
            let n = stmt.column_count();
            stmt.query_map([], |r| {
                (0..n)
                    .map(|i| r.get_ref(i).map(render_cell))
                    .collect::<rusqlite::Result<Vec<String>>>()
            })
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap()
        };
        let mut rows_before = every_row(&conn);
        let cartridges_before = cartridge_rows(&conn);
        assert_eq!(
            cartridges_before.len(),
            2,
            "positive control: two cartridges"
        );
        let objects_before = schema_objects(&conn);
        let tables: Vec<String> = rows_before.keys().cloned().collect();
        let fks_before: Vec<_> = tables.iter().map(|t| foreign_keys_of(&conn, t)).collect();
        let cols_before = table_info(&conn, "cartridges");
        for dropped in M033_DROPPED {
            assert!(
                cols_before.iter().any(|c| c.0 == dropped),
                "positive control: {dropped} exists before 033"
            );
        }
        let indexes_before = index_names(&conn, "cartridges");
        assert_eq!(
            indexes_before,
            vec![
                "idx_cartridges_barcode",
                "idx_cartridges_location",
                "idx_cartridges_serial_number",
                "sqlite_autoindex_cartridges_1"
            ],
            "positive control: cartridges has its indexes to compare"
        );

        migrate_to(&mut conn, Some(33)).expect("033 must migrate a catalog that never set them");
        assert_eq!(user_version(&conn), 33);

        let mut rows_after = every_row(&conn);
        rows_before.remove("cartridges");
        rows_after.remove("cartridges");
        assert_eq!(
            rows_before, rows_after,
            "033 must not add, drop, renumber or alter a row of any other table"
        );
        assert_eq!(
            cartridges_before,
            cartridge_rows(&conn),
            "033 must keep every cartridge, its id and every kept cell"
        );
        let not_cartridges = |objs: &[(String, String, String, Option<String>)]| {
            objs.iter()
                .filter(|o| !(o.0 == "table" && o.1 == "cartridges"))
                .cloned()
                .collect::<Vec<_>>()
        };
        assert_eq!(
            not_cartridges(&objects_before),
            not_cartridges(&schema_objects(&conn)),
            "033 changed a schema object other than the cartridges table, or did not \
             recreate one of its indexes exactly"
        );
        let fks_after: Vec<_> = tables.iter().map(|t| foreign_keys_of(&conn, t)).collect();
        assert_eq!(fks_before, fks_after, "033 must restate every foreign key");
        assert_eq!(
            cols_before
                .into_iter()
                .filter(|c| !M033_DROPPED.contains(&c.0.as_str()))
                .collect::<Vec<_>>(),
            table_info(&conn, "cartridges"),
            "exactly the three columns go; every other keeps its shape and order"
        );
        let report = crate::cli::operations::db_fsck(&conn, false, false).unwrap();
        assert!(
            report.integrity_ok && report.issues.is_empty(),
            "{:?}",
            report.issues
        );
        assert!(
            conn.execute("DELETE FROM cartridges WHERE id = 870", [])
                .is_err(),
            "cartridge 870 is mounted; FK enforcement must refuse the delete"
        );
        let err = conn
            .execute(
                "UPDATE cartridges SET status = 'offsite' WHERE id = 870",
                [],
            )
            .expect_err("the status CHECK survives the rebuild");
        assert!(err.to_string().contains("CHECK constraint failed"), "{err}");
        let err = conn
            .execute(
                "INSERT INTO cartridges (barcode, media_type, nominal_capacity, serial_number)
                 VALUES ('BC-DUP', 'LTO-6', 1, 'MAM0870')",
                [],
            )
            .expect_err("the partial unique serial index survives the rebuild");
        assert!(err.to_string().contains("UNIQUE"), "{err}");
    }

    /// Nothing ever wrote the three columns, so a value other than their
    /// default (0, 0, NULL) was set by hand, and dropping it would lose it
    /// silently: 033 refuses by name and row, as 026, 027 and 032 do. The
    /// positive control is the test above: the same seed at its defaults
    /// migrates.
    #[test]
    fn test_migration_033_refuses_a_hand_set_dead_column_by_row() {
        let mut conn = open_memory_at_032_seeded();
        conn.execute_batch(
            "UPDATE cartridges SET total_bytes_written = 5 WHERE id = 8;
             UPDATE cartridges SET error_history = 'x' WHERE id = 870;",
        )
        .unwrap();
        let err = migrate(&mut conn).expect_err("033 must refuse a hand-set dead column");
        let msg = match &err {
            TapectlError::Migration(m) => m.clone(),
            other => panic!("expected the generic Migration variant, got {other:?}"),
        };
        assert!(msg.starts_with("migration 033 cannot run: "), "{msg}");
        assert!(msg.contains("2 cartridge row(s) (id 8, 870)"), "{msg}");
        assert!(msg.contains("Nothing has been changed."), "{msg}");
        assert_eq!(user_version(&conn), 32, "a refused 033 rolls back");
        let kept: (i64, String) = conn
            .query_row(
                "SELECT (SELECT total_bytes_written FROM cartridges WHERE id = 8),
                        (SELECT error_history FROM cartridges WHERE id = 870)",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(kept, (5, "x".to_string()), "a refused 033 drops nothing");

        // Each column alone refuses too, and NULL is a default.
        for set in [
            "total_bytes_written = NULL, total_bytes_read = 7, error_history = NULL",
            "total_bytes_written = 0, total_bytes_read = 0, error_history = ''",
        ] {
            let mut conn = open_memory_at_032_seeded();
            conn.execute(&format!("UPDATE cartridges SET {set} WHERE id = 8"), [])
                .unwrap();
            let err = migrate(&mut conn).expect_err(set);
            assert!(err.to_string().contains("(id 8)"), "{set}: {err}");
        }
        let mut conn = open_memory_at_032_seeded();
        conn.execute(
            "UPDATE cartridges SET total_bytes_written = NULL, total_bytes_read = NULL",
            [],
        )
        .unwrap();
        migrate(&mut conn).expect("NULL is as unwritten as the default 0");
    }

    /// Migration 029 (issue #410): `readback_checkpoints` exists on a fresh
    /// catalog, refuses a second row for one file of one readback, and its
    /// rows go with their verification session.
    #[test]
    fn test_migration_029_readback_checkpoints_cascade_with_their_session() {
        let conn = open_memory().unwrap();
        conn.execute(
            "INSERT INTO volumes (label, backend_type, backend_name, media_type, capacity_bytes, status)
             VALUES ('CK', 'lto', 'lto0', 'LTO-6', 1, 'initialized')",
            [],
        )
        .unwrap();
        let volume_id = conn.last_insert_rowid();
        conn.execute(
            "INSERT INTO verification_sessions (volume_id, verify_type) VALUES (?1, 'full')",
            [volume_id],
        )
        .unwrap();
        let session_id = conn.last_insert_rowid();
        let insert = |position: i64| {
            conn.execute(
                "INSERT INTO readback_checkpoints (session_id, position, sha256, front_index_sha256)
                 VALUES (?1, ?2, 'aa', 'bb')",
                rusqlite::params![session_id, position],
            )
        };
        insert(4).unwrap();
        assert!(insert(4).is_err(), "one row per file per readback");
        assert!(insert(-1).is_err(), "a position is never negative");
        conn.execute(
            "DELETE FROM verification_sessions WHERE id = ?1",
            [session_id],
        )
        .unwrap();
        let left: i64 = conn
            .query_row("SELECT COUNT(*) FROM readback_checkpoints", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(left, 0);
    }

    /// Issue #417: `audit`'s Heir Kit check — the newest event of one action
    /// — walked all of `events`. Migration 031's index on `(action,
    /// timestamp)` makes it one seek. The plan is read before and after the
    /// migration, so the "before" half is the old behaviour, seen.
    #[test]
    fn the_newest_event_of_an_action_is_an_index_seek() {
        const Q: &str = "EXPLAIN QUERY PLAN \
            SELECT MAX(timestamp) FROM events WHERE action = 'escrow_kit_generated'";
        let plan = |conn: &Connection| -> String {
            let mut stmt = conn.prepare(Q).unwrap();
            let rows: Vec<String> = stmt
                .query_map([], |r| r.get::<_, String>(3))
                .unwrap()
                .collect::<rusqlite::Result<_>>()
                .unwrap();
            rows.join("; ")
        };
        let mut conn = open_memory_at_version(28);
        let before = plan(&conn);
        assert!(
            !before.contains("idx_events_action_timestamp"),
            "positive control, before 031: {before}"
        );
        migrate_to(&mut conn, None).unwrap();
        let after = plan(&conn);
        assert!(
            after.contains("USING COVERING INDEX idx_events_action_timestamp"),
            "after 031: {after}"
        );
    }

    /// `rusqlite_migration` numbers a migration by its position in
    /// [`migrations`], not by its file name, so a file whose prefix is not
    /// its position applies under another number than the one its header,
    /// the docs and every later branch use. Two batches that each took "the
    /// next number" would land that way; this refuses it, and a gap or a
    /// file nothing includes.
    #[test]
    fn every_migration_file_is_named_for_its_position() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/db");
        let source = std::fs::read_to_string(dir.join("mod.rs")).unwrap();
        let body = &source[source.find("fn migrations()").unwrap()..];
        let body = &body[..body.find("\n}\n").unwrap()];
        // Some entries wrap `include_str!(` and its path onto two lines.
        let included: Vec<String> = body
            .match_indices("\"migrations/")
            .map(|(i, m)| {
                let rest = &body[i + m.len()..];
                rest[..rest.find('"').unwrap()].to_string()
            })
            .collect();
        for (i, name) in included.iter().enumerate() {
            assert_eq!(
                name[..3].parse::<usize>().unwrap(),
                i + 1,
                "{name} is migration {} in migrations()",
                i + 1
            );
        }
        let mut on_disk: Vec<String> = std::fs::read_dir(dir.join("migrations"))
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        on_disk.sort();
        assert_eq!(
            on_disk, included,
            "every file, and only those, included in order"
        );
        assert_eq!(
            user_version(&open_memory().unwrap()),
            included.len() as i64,
            "one migration per file"
        );
    }
}
