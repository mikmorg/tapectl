//! Staging several units at once (issue #368).
//!
//! `stage create` over several units, `collection run` and first-run's
//! staging step hand their units to [`stage_many`], which runs up to
//! `jobs` of them at the same time, each as an ordinary `stage_create` on its
//! own thread with its own catalog connection.
//!
//! What makes that safe is in `stage_create` itself, so it holds just as
//! well between two separate `tapectl` processes:
//!
//! - **One stage of a unit at a time.** A stage takes the admission lock
//!   (`lock::acquire_admission`) for the moment it checks staging space and
//!   records its stage set, and refuses there when another stage set of the
//!   same snapshot is already staged or is being staged by a live process.
//! - **Space for every stage in flight.** The space check, under the same
//!   lock, counts what the other live stages may still write.
//! - **A busy catalog is waited out** (issue #377's busy policy), never fatal
//!   to a stage that has done its work.
//!
//! What this module adds is the scheduling: the largest units first, so the
//! last to finish is not one huge unit started late; one line per unit as it
//! starts and ends; no new unit started once one has failed (the ones
//! running are let finish: their work is good); and a bound on how many run
//! at once from the memory the host has.

use std::io::Write;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::OnceLock;

use rusqlite::{params, Connection};

use crate::config::{Config, TapectlPaths};
use crate::error::{Result, TapectlError};

/// `[staging] jobs`' default: one unit at a time, as before issue #368.
pub const DEFAULT_JOBS: usize = 1;

/// The most units `[staging] jobs` or `--jobs` may stage at once.
pub const MAX_JOBS: usize = 16;

/// What one stage in flight may hold of the host's memory apart from its
/// compressor: the stream reader's (a member's extension headers, at most
/// `tar::stream::MAX_EXTENDED` = 64 MiB, and a 64 KiB chunk), the slice
/// file's 1 MiB buffer and the bounded hash queues, with room to spare:
/// 192 MiB in all. Packing holds no slice whole (`staging::pack` writes each
/// frame as the cut comes), but a compressed staging adds its encoder's and
/// decoder's memory, which its level decides (#570): [`job_memory`] adds
/// `Packing::memory`, xz's own table giving 674 MiB (+65 MiB to decode) at
/// level 9.
pub const MEMORY_PER_JOB: u64 = 192 << 20;

/// One unit to stage: its name, for the operator, and the snapshot to stage.
#[derive(Debug, Clone)]
pub struct StageJob {
    pub unit_name: String,
    pub snapshot_id: i64,
    /// A first stage of the version, or a `--version` re-stage: what
    /// admission rechecks (issue #368).
    pub admission: super::Admission,
    /// Where to read the unit's files when that is a snapshot of the
    /// collection root rather than the unit's own folder (`collection run`'s
    /// `snapshot_command`, ADR-0012 2026-10-16 item 75). `None` reads the
    /// unit where it lives.
    pub view: Option<super::view::SourceView>,
}

/// How one unit's stage ended.
#[derive(Debug)]
pub struct StageOutcome {
    pub unit_name: String,
    pub snapshot_id: i64,
    /// The stage set made, or why not; `None` when the unit was never
    /// started because another had already failed.
    pub result: Option<Result<i64>>,
}

/// The memory the kernel says is available for new work (`MemAvailable`),
/// in bytes, or `None` where it does not say.
pub(crate) fn mem_available() -> Option<u64> {
    #[cfg(test)]
    if let Some(bytes) = MEMORY_OVERRIDE.with(|m| m.get()) {
        return Some(bytes);
    }
    let info = std::fs::read_to_string("/proc/meminfo").ok()?;
    info.lines()
        .find_map(|l| l.strip_prefix("MemAvailable:"))
        .and_then(|v| v.trim().trim_end_matches("kB").trim().parse::<u64>().ok())
        .map(|kib| kib.saturating_mul(1024))
}

#[cfg(test)]
thread_local! {
    /// Test-only: the available memory `mem_available` reports on this
    /// thread, so a test's job count does not depend on the host's load.
    pub(crate) static MEMORY_OVERRIDE: std::cell::Cell<Option<u64>> =
        const { std::cell::Cell::new(None) };
}

/// What one job holds of the host's memory, and how it packs, for the
/// notice that names them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct JobMemory {
    pub(crate) bytes: u64,
    /// `"zstd level 19"`, or `None` for a job that packs nothing.
    pub(crate) packs: Option<String>,
}

/// A job packing `packing`, or nothing: [`MEMORY_PER_JOB`] plus the
/// compressor's pack and decode memory at its level (#570).
pub(crate) fn job_memory(packing: Option<super::pack::Packing>) -> JobMemory {
    match packing {
        None => JobMemory {
            bytes: MEMORY_PER_JOB,
            packs: None,
        },
        Some(p) => JobMemory {
            bytes: MEMORY_PER_JOB + p.memory(),
            packs: Some(format!("{} level {}", p.algorithm.name(), p.level)),
        },
    }
}

/// One job's memory from its resolved policy. A job whose policy cannot be
/// resolved, or whose compression is refused, is counted as packing nothing:
/// its own `stage_create` refuses it by name before it holds anything.
fn memory_of(conn: &Connection, config: &Config, job: &StageJob) -> JobMemory {
    let resolved = (|| -> Result<Option<super::pack::Packing>> {
        let unit_id: i64 = conn.query_row(
            "SELECT unit_id FROM snapshots WHERE id = ?1",
            params![job.snapshot_id],
            |r| r.get(0),
        )?;
        let name: String = conn.query_row(
            "SELECT name FROM units WHERE id = ?1",
            params![unit_id],
            |r| r.get(0),
        )?;
        let unit = crate::db::queries::get_unit_by_name(conn, &name)?
            .ok_or_else(|| TapectlError::Other("unit not found".into()))?;
        let resolved = crate::policy::resolve(conn, config, &unit)?;
        super::pack::packing(&resolved.compression, resolved.compression_level)
    })();
    job_memory(resolved.unwrap_or(None))
}

/// How many of `jobs` to stage at once for `requested`: at least 1, at most
/// [`MAX_JOBS`] and the number of jobs, and no more than the largest count
/// whose heaviest jobs together fit in `available` bytes of memory: each job
/// is judged by its own figure ([`job_memory`]), and any `n` of them may
/// run together, so the `n` largest must fit. A request cut down by memory
/// is said on `notices`, naming the compression and level of the heaviest
/// jobs when they pack.
pub(crate) fn effective_jobs(
    requested: usize,
    jobs: &[JobMemory],
    available: Option<u64>,
    notices: &mut dyn Write,
) -> usize {
    let wanted = requested.clamp(1, MAX_JOBS).min(jobs.len().max(1));
    let Some(available) = available else {
        return wanted;
    };
    let mut heaviest: Vec<&JobMemory> = jobs.iter().collect();
    heaviest.sort_by(|a, b| b.bytes.cmp(&a.bytes));
    let mut fits = 0;
    let mut total = 0u64;
    for job in heaviest.iter().take(wanted) {
        total = total.saturating_add(job.bytes);
        if total > available {
            break;
        }
        fits += 1;
    }
    let fits = fits.max(1);
    if fits < wanted {
        let each = heaviest.first().map_or(MEMORY_PER_JOB, |j| j.bytes);
        let how = match heaviest.first().and_then(|j| j.packs.as_deref()) {
            Some(packs) => format!(
                "each stage in flight keeps up to {} of its source in the page cache plus its \
                 own buffers, and the heaviest, packing at {packs}, needs {} in all with its \
                 compressor",
                crate::util::format_bytes_binary(MEMORY_PER_JOB as i64),
                crate::util::format_bytes_binary(i64::try_from(each).unwrap_or(i64::MAX)),
            ),
            None => format!(
                "each stage in flight keeps up to {} of its source in the page cache plus its \
                 own buffers",
                crate::util::format_bytes_binary(MEMORY_PER_JOB as i64),
            ),
        };
        let _ = writeln!(
            notices,
            "note: staging {fits} unit(s) at once, not {wanted}: the host has {} of memory \
             available, and {how}",
            crate::util::format_bytes_binary(i64::try_from(available).unwrap_or(i64::MAX)),
        );
        return fits;
    }
    wanted
}

/// The order to start `jobs` in when several run at once: the largest
/// snapshot first (issue #368), ties in the given order.
fn largest_first(conn: &Connection, jobs: &[StageJob]) -> Result<Vec<usize>> {
    let mut sized = Vec::with_capacity(jobs.len());
    for (i, job) in jobs.iter().enumerate() {
        let size: Option<i64> = conn.query_row(
            "SELECT total_size FROM snapshots WHERE id = ?1",
            params![job.snapshot_id],
            |r| r.get(0),
        )?;
        sized.push((i, size.unwrap_or(0)));
    }
    sized.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
    Ok(sized.into_iter().map(|(i, _)| i).collect())
}

/// Stage every one of `jobs`, up to `requested_jobs` at once, and say how
/// each ended, in `jobs`' order.
///
/// One at a time — `requested_jobs` 1, a single unit, or a catalog that is
/// not a file another connection could open — this is exactly the loop it
/// replaces: each unit in the given order on `conn`, stopping at the first
/// failure. Several at a time, each worker thread opens its own connection
/// and its own progress recorder (so each stage set's phase timings are its
/// own), starts the largest units first, and prints a line as each unit
/// starts and ends (`announce`, which is only called with more than one
/// unit). After a failure no unit is started; the ones running finish.
///
/// `notices` takes what is said once, before any stage: a job count cut down
/// by memory. Each stage's own notices (the staging-space question, an
/// `encrypt = false` warning) go to stderr.
#[allow(clippy::too_many_arguments)]
pub fn stage_many(
    conn: &Connection,
    paths: &TapectlPaths,
    config: &Config,
    jobs: &[StageJob],
    requested_jobs: usize,
    assume_yes: bool,
    notices: &mut dyn Write,
    announce: &(dyn Fn(&str) + Sync),
) -> Result<Vec<StageOutcome>> {
    let db_file = super::lock::db_file_of(conn);
    let parallel = if db_file.is_some() && jobs.len() > 1 && requested_jobs > 1 {
        let memory: Vec<JobMemory> = jobs.iter().map(|j| memory_of(conn, config, j)).collect();
        effective_jobs(requested_jobs, &memory, mem_available(), notices)
    } else {
        1
    };
    let total = jobs.len();
    let many = total > 1;
    let results: Vec<OnceLock<Result<i64>>> = jobs.iter().map(|_| OnceLock::new()).collect();
    let started = AtomicUsize::new(0);
    // The command's session (issue #386): one at a time, every stage runs
    // in it. Several at a time, each unit runs in a session of its own
    // beside it (`progress::Handle::start_child`), so the units' phases,
    // waits and stalls never interleave in one log and each stage set's
    // phase timings name a log that exists; the command's log says which
    // unit started and ended when, and where its own log is.
    let parent = crate::progress::handle();
    let say = |line: &str| {
        announce(line);
        parent.log(line);
    };
    let run_one = |c: &Connection, cfg: &Config, i: usize, own_session: bool| -> bool {
        let job = &jobs[i];
        let n = started.fetch_add(1, Ordering::AcqRel) + 1;
        let session =
            own_session.then(|| parent.start_child(&format!("stage create {}", job.unit_name)));
        // Several at once, the tracing lines the command's log tees in from
        // every worker say whose they are.
        let _span =
            own_session.then(|| tracing::info_span!("stage", unit = %job.unit_name).entered());
        if many {
            say(&format!("staging {} ({n} of {total})", job.unit_name));
            if let Some(log) = session.as_ref().and_then(|s| s.log_path()) {
                parent.log(&format!(
                    "{}: its own log is {}",
                    job.unit_name,
                    log.display()
                ));
            }
        }
        #[cfg(test)]
        let _running = super::lock::db_file_of(c).map(|d| in_flight::Running::enter(&d));
        let result = super::stage_create_admitting(
            c,
            paths,
            cfg,
            job.snapshot_id,
            assume_yes,
            job.admission,
            job.view.as_ref(),
            &mut std::io::stderr(),
        );
        let ok = result.is_ok();
        if let Some(session) = &session {
            match &result {
                Ok(_) => session.note_result(Ok(())),
                Err(e) => session.note_result(Err(&e.to_string())),
            }
        }
        drop(session);
        if many {
            match &result {
                Ok(id) => say(&format!("staged {} (stage set {id})", job.unit_name)),
                Err(e) => {
                    // The whole error on stderr; its first line in the
                    // command's log, which keeps one stamped line per event
                    // (the unit's own log has the outcome too).
                    let line = format!("failed {}: {e}", job.unit_name);
                    announce(&line);
                    parent.log(line.lines().next().unwrap_or_default());
                }
            }
        }
        let _ = results[i].set(result);
        ok
    };

    match (parallel, db_file) {
        (p, Some(db_file)) if p > 1 => {
            let order = largest_first(conn, jobs)?;
            let cfg = config.clone();
            let next = AtomicUsize::new(0);
            let failed = AtomicBool::new(false);
            std::thread::scope(|s| {
                for w in 0..p {
                    let (order, next, failed, cfg, db_file, run_one, results) =
                        (&order, &next, &failed, &cfg, &db_file, &run_one, &results);
                    let spawned = std::thread::Builder::new()
                        .name(format!("tapectl-stage-{w}"))
                        .spawn_scoped(s, move || {
                            let mut wconn: Option<Connection> = None;
                            loop {
                                if failed.load(Ordering::Acquire) || crate::signal::is_interrupted()
                                {
                                    break;
                                }
                                let k = next.fetch_add(1, Ordering::AcqRel);
                                let Some(&i) = order.get(k) else { break };
                                if wconn.is_none() {
                                    match crate::db::open(db_file) {
                                        Ok(c) => wconn = Some(c),
                                        Err(e) => {
                                            let _ = results[i].set(Err(e));
                                            failed.store(true, Ordering::Release);
                                            break;
                                        }
                                    }
                                }
                                let c = wconn.as_ref().expect("opened above");
                                if !run_one(c, cfg, i, true) {
                                    failed.store(true, Ordering::Release);
                                }
                            }
                        });
                    if let Err(e) = spawned {
                        tracing::warn!(error = %e, "could not start a staging thread");
                    }
                }
            });
        }
        _ => {
            for i in 0..total {
                if !run_one(conn, config, i, false) {
                    break;
                }
            }
        }
    }

    Ok(jobs
        .iter()
        .zip(results)
        .map(|(job, r)| StageOutcome {
            unit_name: job.unit_name.clone(),
            snapshot_id: job.snapshot_id,
            result: r.into_inner(),
        })
        .collect())
}

/// The error a run of [`stage_many`] ends with when any unit failed: the
/// first failure in the given order, in full, naming every unit that was
/// staged, failed or never started — or `Ok` when all were staged.
pub fn first_failure(outcomes: Vec<StageOutcome>) -> Result<Vec<(String, i64)>> {
    let mut staged = Vec::new();
    let mut failed: Vec<(String, TapectlError)> = Vec::new();
    let mut not_started = Vec::new();
    for o in outcomes {
        match o.result {
            Some(Ok(id)) => staged.push((o.unit_name, id)),
            Some(Err(e)) => failed.push((o.unit_name, e)),
            None => not_started.push(o.unit_name),
        }
    }
    if failed.is_empty() && not_started.is_empty() {
        return Ok(staged);
    }
    if failed.len() == 1 && staged.is_empty() && not_started.is_empty() {
        return Err(failed.remove(0).1);
    }
    let mut lines = Vec::new();
    if !staged.is_empty() {
        let names: Vec<&str> = staged.iter().map(|(n, _)| n.as_str()).collect();
        lines.push(format!("staged: {}", names.join(", ")));
    }
    for (name, e) in failed.iter().skip(1) {
        lines.push(format!("also failed: {name}: {e}"));
    }
    if !not_started.is_empty() {
        lines.push(format!(
            "not started (an earlier unit failed): {}",
            not_started.join(", ")
        ));
    }
    let Some((name, first)) = failed.into_iter().next() else {
        return Err(TapectlError::Other(format!(
            "stopped before every unit was staged\n{}",
            lines.join("\n")
        )));
    };
    if let TapectlError::Interrupted(at) = first {
        return Err(TapectlError::Interrupted(format!(
            "{name}: {at}\n{}",
            lines.join("\n")
        )));
    }
    Err(TapectlError::Other(format!(
        "{name}: {first}\n{}",
        lines.join("\n")
    )))
}

#[cfg(test)]
pub(crate) mod in_flight {
    //! Test-only: how many stages ran at once, per catalog (issue #368).
    use std::collections::HashMap;
    use std::path::{Path, PathBuf};
    use std::sync::Mutex;

    static RUNNING: Mutex<Option<HashMap<PathBuf, (usize, usize)>>> = Mutex::new(None);

    pub(crate) struct Running(PathBuf);

    impl Running {
        pub(crate) fn enter(db: &Path) -> Self {
            let mut m = RUNNING.lock().unwrap();
            let e = m
                .get_or_insert_with(HashMap::new)
                .entry(db.to_path_buf())
                .or_insert((0, 0));
            e.0 += 1;
            e.1 = e.1.max(e.0);
            Self(db.to_path_buf())
        }
    }

    impl Drop for Running {
        fn drop(&mut self) {
            if let Some(e) = RUNNING
                .lock()
                .unwrap()
                .as_mut()
                .and_then(|m| m.get_mut(&self.0))
            {
                e.0 -= 1;
            }
        }
    }

    /// The most stages that ran at once against `db`.
    pub(crate) fn most(db: &Path) -> usize {
        RUNNING
            .lock()
            .unwrap()
            .as_ref()
            .and_then(|m| m.get(db))
            .map_or(0, |e| e.1)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::sync::Mutex;
    use tempfile::TempDir;

    struct Home {
        _tmp: TempDir,
        conn: Connection,
        paths: TapectlPaths,
        config: Config,
    }

    /// A home with an operator, a tenant, an escrow recipient and `units`
    /// units, each `(name, files, bytes per file)`, with a snapshot taken.
    fn home(units: &[(&str, usize, usize)]) -> (Home, Vec<StageJob>) {
        let tmp = TempDir::new().unwrap();
        let home = tmp.path().join("home");
        fs::create_dir_all(&home).unwrap();
        let paths = TapectlPaths::new(home);
        paths.ensure_dirs().unwrap();
        let conn = crate::db::open(&paths.db_file).unwrap();
        let staging = tmp.path().join("staging");
        fs::create_dir_all(&staging).unwrap();
        let mut config = Config::default();
        config.staging.directory = staging.to_string_lossy().into_owned();
        config.defaults.slice_size = "1M".into();
        crate::tenant::add_tenant(&conn, &paths, "op", None, true).unwrap();
        crate::tenant::add_tenant(&conn, &paths, "alice", None, false).unwrap();
        conn.execute(
            "INSERT INTO tenants (name, is_operator, status) VALUES ('escrow-holder', 0, 'active')",
            [],
        )
        .unwrap();
        let holder = conn.last_insert_rowid();
        let kp = crate::crypto::keys::generate_keypair();
        crate::db::queries::insert_escrow_key(
            &conn,
            holder,
            "test-escrow",
            &kp.fingerprint,
            &kp.public_key,
            None,
        )
        .unwrap();
        let mut jobs = Vec::new();
        for (name, files, bytes) in units {
            let src = tmp.path().join("src").join(name);
            fs::create_dir_all(&src).unwrap();
            for f in 0..*files {
                let data: Vec<u8> = (0..*bytes)
                    .map(|j| ((j * 13 + f * 7) % 251) as u8)
                    .collect();
                fs::write(src.join(format!("f{f}.bin")), data).unwrap();
            }
            crate::unit::init_unit(
                &conn,
                &paths,
                src.to_str().unwrap(),
                "alice",
                Some(name),
                &[],
                None,
            )
            .unwrap();
            let snapshot_id = crate::staging::snapshot_create(&conn, name, &config).unwrap();
            jobs.push(StageJob {
                unit_name: name.to_string(),
                snapshot_id,
                admission: crate::staging::Admission::Unstaged,
                view: None,
            });
        }
        (
            Home {
                _tmp: tmp,
                conn,
                paths,
                config,
            },
            jobs,
        )
    }

    /// Per unit: its stage sets' statuses, slice count and stream size, and its
    /// files' recorded sha256s.
    /// `(unit, stage set statuses, slices, (path, sha256) per file)`.
    type Recorded = (String, Vec<String>, i64, Vec<(String, String)>);

    fn recorded(conn: &Connection) -> Vec<Recorded> {
        let units: Vec<(i64, String)> = conn
            .prepare("SELECT id, name FROM units ORDER BY name")
            .unwrap()
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap()
            .collect::<std::result::Result<_, _>>()
            .unwrap();
        units
            .into_iter()
            .map(|(id, name)| {
                let sets: Vec<(String, i64, i64)> = conn
                    .prepare(
                        "SELECT ss.status, COALESCE(ss.num_slices, 0), COALESCE(ss.stream_bytes, 0)
                         FROM stage_sets ss JOIN snapshots s ON s.id = ss.snapshot_id
                         WHERE s.unit_id = ?1 ORDER BY ss.id",
                    )
                    .unwrap()
                    .query_map([id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
                    .unwrap()
                    .collect::<std::result::Result<_, _>>()
                    .unwrap();
                // Not the dotfile: it carries each home's own unit uuid.
                let hashes: Vec<(String, String)> = conn
                    .prepare(&format!(
                        "SELECT p.path, fv.sha256 FROM {}
                         JOIN snapshots s ON s.id = fv.snapshot_id
                         WHERE s.unit_id = ?1 AND fv.kind <> 0
                           AND p.path <> '.tapectl-unit.toml' ORDER BY p.path",
                        crate::db::files::VERSION_FILES
                    ))
                    .unwrap()
                    .query_map([id], |r| {
                        Ok((
                            r.get(0)?,
                            crate::db::files::sha256_column(r.get(1)?).unwrap_or_default(),
                        ))
                    })
                    .unwrap()
                    .collect::<std::result::Result<_, _>>()
                    .unwrap();
                let statuses = sets.iter().map(|s| s.0.clone()).collect();
                let slices = sets.iter().map(|s| s.1).sum();
                // Not the stream size (s.2): the stream records each file's
                // times, which differ between the two trees, at a width that
                // follows their values.
                (name, statuses, slices, hashes)
            })
            .collect()
    }

    const UNITS: [(&str, usize, usize); 4] = [
        ("u-small", 3, 2000),
        ("u-big", 6, 900_000),
        ("u-mid", 10, 150_000),
        ("u-many", 40, 9000),
    ];

    /// Issue #368 acceptance: several units staged at once get exactly the
    /// stage sets, slices and hashes one-at-a-time staging records, and they
    /// really did run at once.
    #[test]
    fn units_staged_at_once_record_what_one_at_a_time_records() {
        let (serial, jobs) = home(&UNITS);
        let out = stage_many(
            &serial.conn,
            &serial.paths,
            &serial.config,
            &jobs,
            1,
            false,
            &mut Vec::new(),
            &|_| {},
        )
        .unwrap();
        assert_eq!(first_failure(out).unwrap().len(), 4);

        let (parallel, jobs) = home(&UNITS);
        MEMORY_OVERRIDE.with(|m| m.set(Some(64 * MEMORY_PER_JOB)));
        let lines = Mutex::new(Vec::new());
        let out = stage_many(
            &parallel.conn,
            &parallel.paths,
            &parallel.config,
            &jobs,
            3,
            false,
            &mut Vec::new(),
            &|l| lines.lock().unwrap().push(l.to_string()),
        )
        .unwrap();
        let staged = first_failure(out).unwrap();
        assert_eq!(
            staged.iter().map(|(n, _)| n.as_str()).collect::<Vec<_>>(),
            ["u-small", "u-big", "u-mid", "u-many"],
            "reported in the order given"
        );
        let want = recorded(&serial.conn);
        assert_eq!(recorded(&parallel.conn), want);
        for (name, statuses, slices, ..) in &want {
            assert_eq!(statuses, &vec!["staged".to_string()], "{name}");
            assert!(*slices >= 1, "{name}");
        }
        assert!(
            in_flight::most(&super::super::lock::db_file_of(&parallel.conn).unwrap()) >= 2,
            "several stages ran at once"
        );
        assert_eq!(
            in_flight::most(&super::super::lock::db_file_of(&serial.conn).unwrap()),
            1
        );
        let lines = lines.into_inner().unwrap();
        // Three workers take the three largest units at once; the smallest
        // waits for the first of them to finish.
        let started: Vec<&str> = lines
            .iter()
            .filter_map(|l| l.strip_prefix("staging "))
            .map(|l| l.split(' ').next().unwrap())
            .collect();
        assert_eq!(started.len(), 4, "{lines:?}");
        assert_eq!(
            started[3], "u-small",
            "the largest units start first: {lines:?}"
        );
        let finished_before_small = lines
            .iter()
            .position(|l| l.starts_with("staging u-small"))
            .map(|at| lines[..at].iter().any(|l| l.starts_with("staged ")));
        assert_eq!(finished_before_small, Some(true), "{lines:?}");
        assert_eq!(
            lines.len(),
            8,
            "a line as each unit starts and ends: {lines:?}"
        );
    }

    /// After a failure no further unit is started; one at a time, the units
    /// after it are reported as not started.
    #[test]
    fn a_failure_stops_further_units_from_starting() {
        let (h, mut jobs) = home(&[("a", 1, 100), ("b", 1, 100)]);
        jobs.insert(
            0,
            StageJob {
                unit_name: "ghost".into(),
                snapshot_id: 999_999,
                admission: crate::staging::Admission::Unstaged,
                view: None,
            },
        );
        let out = stage_many(
            &h.conn,
            &h.paths,
            &h.config,
            &jobs,
            1,
            false,
            &mut Vec::new(),
            &|_| {},
        )
        .unwrap();
        assert!(matches!(out[0].result, Some(Err(_))));
        assert!(out[1].result.is_none() && out[2].result.is_none());
        let err = first_failure(out).unwrap_err().to_string();
        assert!(
            err.starts_with("ghost: ") && err.contains("not started"),
            "{err}"
        );
    }

    #[test]
    fn jobs_are_bounded_by_the_units_the_limit_and_the_memory() {
        let plain = |n: usize| vec![job_memory(None); n];
        let mut notices = Vec::new();
        assert_eq!(effective_jobs(0, &plain(5), None, &mut notices), 1);
        assert_eq!(effective_jobs(4, &plain(2), None, &mut notices), 2);
        assert_eq!(
            effective_jobs(100, &plain(100), None, &mut notices),
            MAX_JOBS
        );
        assert!(notices.is_empty());
        assert_eq!(
            effective_jobs(8, &plain(10), Some(3 * MEMORY_PER_JOB + 1), &mut notices),
            3
        );
        let said = String::from_utf8(notices).unwrap();
        assert!(said.contains("staging 3 unit(s) at once, not 8"), "{said}");
        let mut quiet = Vec::new();
        assert_eq!(
            effective_jobs(2, &plain(10), Some(1), &mut quiet),
            1,
            "never fewer than one"
        );
    }

    /// #570: at xz 9 the memory that holds three jobs at `none` holds one,
    /// the notice names the compression and level, and a mixed queue is
    /// judged by its heaviest jobs.
    #[test]
    fn xz_level_nine_holds_fewer_jobs_and_the_notice_names_it() {
        let xz9 = job_memory(super::super::pack::packing("xz", Some(9)).unwrap());
        assert!(
            xz9.bytes >= MEMORY_PER_JOB + (673 + 64) * (1 << 20),
            "xz's documented 674 MiB to pack and 65 MiB to decode (the man page rounds \
             up) are counted"
        );
        let memory = 3 * MEMORY_PER_JOB + 1;
        let mut quiet = Vec::new();
        let none = vec![job_memory(None); 3];
        assert_eq!(effective_jobs(3, &none, Some(memory), &mut quiet), 3);
        assert!(quiet.is_empty());
        let mut notices = Vec::new();
        let heavy = vec![xz9.clone(); 3];
        assert_eq!(effective_jobs(3, &heavy, Some(memory), &mut notices), 1);
        let said = String::from_utf8(notices).unwrap();
        assert!(
            said.contains("not 3") && said.contains("xz level 9"),
            "{said}"
        );
        // One xz job among plain ones: the plain ones are not held back by
        // it unless the heaviest set no longer fits.
        let mixed = vec![job_memory(None), xz9, job_memory(None)];
        let mut notices = Vec::new();
        assert_eq!(effective_jobs(3, &mixed, Some(memory), &mut notices), 1);
        assert!(String::from_utf8(notices).unwrap().contains("xz level 9"));
    }

    /// Every table figure grows with the level and never falls below the
    /// documented ones the plan names.
    #[test]
    fn the_memory_table_covers_every_level_and_never_shrinks_with_one() {
        use super::super::pack::{packing, Algorithm};
        for alg in [Algorithm::Gzip, Algorithm::Xz, Algorithm::Zstd] {
            let (low, high) = alg.levels();
            let mut last = 0;
            for level in low..=high {
                let p = packing(alg.name(), Some(i64::from(level)))
                    .unwrap()
                    .unwrap();
                assert!(p.memory() >= last, "{} level {level}", alg.name());
                assert!(p.memory() > 0);
                last = p.memory();
            }
        }
    }

    /// #570: the memory that holds three jobs at `none` holds fewer when the
    /// jobs pack at zstd 19 (its own window and tables), and the notice names
    /// the compression and the level. Three small units are staged through
    /// `stage_many`, so what reaches admission is each job's resolved policy.
    #[test]
    fn the_memory_that_holds_three_jobs_at_none_holds_fewer_when_they_pack() {
        let units = [("m-a", 2, 3000), ("m-b", 2, 3000), ("m-c", 2, 3000)];
        let memory = 3 * MEMORY_PER_JOB + 1;
        let run = |compression: &str, level: Option<i64>| {
            let (mut h, jobs) = home(&units);
            h.config.defaults.compression = compression.to_string();
            h.config.defaults.compression_level = level;
            MEMORY_OVERRIDE.with(|m| m.set(Some(memory)));
            let mut notices = Vec::new();
            let out = stage_many(
                &h.conn,
                &h.paths,
                &h.config,
                &jobs,
                3,
                false,
                &mut notices,
                &|_| {},
            )
            .unwrap();
            assert_eq!(first_failure(out).unwrap().len(), 3);
            let db = super::super::lock::db_file_of(&h.conn).unwrap();
            (String::from_utf8(notices).unwrap(), in_flight::most(&db))
        };
        let (said, _) = run("none", None);
        assert!(said.is_empty(), "three at none fit: {said}");
        let (said, most) = run("zstd", Some(19));
        assert!(most < 3, "fewer than three ran at once: {most}");
        assert!(
            said.contains("zstd level 19") && said.contains("not 3"),
            "the notice names the compression and level: {said}"
        );
    }

    /// Every file under `root` and its size: what the hasher reads, each
    /// unit's dotfile included (recorded and archived, though no change
    /// comparison counts it, ADR-0012 2026-10-07 item 24).
    fn source_files(root: &std::path::Path) -> Vec<(std::path::PathBuf, u64)> {
        let mut files: Vec<_> = walkdir::WalkDir::new(root)
            .into_iter()
            .map(|e| e.unwrap())
            .filter(|e| e.file_type().is_file())
            .map(|e| (e.path().to_path_buf(), e.metadata().unwrap().len()))
            .collect();
        files.sort();
        files
    }

    /// `stage create` on tar takes each file's sha256 from the stream
    /// (ADR-0012 item 253): every file of every unit staged is recorded with
    /// the sha256 of the file on disk, the dotfiles too.
    #[test]
    fn every_staged_file_is_recorded_with_the_sha256_of_its_bytes() {
        use sha2::{Digest, Sha256};
        const TREE: [(&str, usize, usize); 3] = [
            ("u-tree", 48, 40_000),
            ("u-wide", 5, 700_000),
            ("u-tiny", 2, 10),
        ];
        let (h, jobs) = home(&TREE);
        let root = fs::canonicalize(h._tmp.path().join("src")).unwrap();
        let out = stage_many(
            &h.conn,
            &h.paths,
            &h.config,
            &jobs,
            1,
            false,
            &mut Vec::new(),
            &|_| {},
        );
        assert_eq!(first_failure(out.unwrap()).unwrap().len(), TREE.len());
        let files = source_files(&root);
        assert_eq!(files.len(), 48 + 5 + 2 + TREE.len(), "with the dotfiles");
        let got = recorded(&h.conn);
        let mut seen = 0;
        for (unit, _, _, hashes) in &got {
            for (path, sha) in hashes {
                let on_disk = fs::read(root.join(unit).join(path)).unwrap();
                assert_eq!(sha, &format!("{:x}", Sha256::digest(&on_disk)), "{path}");
                seen += 1;
            }
        }
        assert_eq!(seen, 48 + 5 + 2, "every file of the snapshots is recorded");
    }

    /// Issue #368 with issue #386: several units staged at once each log to
    /// a session of their own beside the command's, so no two units' phases
    /// interleave in one log; the command's log says when each unit started
    /// and ended and where its log is; each stage set's phase timings are
    /// recorded under the session id of the log that holds them; and
    /// `tapectl status` reads every unit's session and how it ended.
    #[test]
    fn units_staged_at_once_each_log_to_a_session_of_their_own() {
        let (h, jobs) = home(&[("u-a", 4, 300_000), ("u-b", 4, 200_000), ("u-c", 3, 1000)]);
        let logs = h._tmp.path().join("logs");
        fs::create_dir_all(&logs).unwrap();
        MEMORY_OVERRIDE.with(|m| m.set(Some(64 * MEMORY_PER_JOB)));
        let (command_log, staged) = {
            let session = crate::progress::start_session(
                Some(&logs),
                "stage create u-a u-b u-c",
                crate::progress::Display::Off,
                false,
            );
            let out = stage_many(
                &h.conn,
                &h.paths,
                &h.config,
                &jobs,
                3,
                false,
                &mut Vec::new(),
                &|_| {},
            )
            .unwrap();
            session.note_result(Ok(()));
            (
                session.log_path().unwrap().to_path_buf(),
                first_failure(out).unwrap(),
            )
        };
        assert!(
            in_flight::most(&super::super::lock::db_file_of(&h.conn).unwrap()) >= 2,
            "several stages ran at once"
        );

        let text = fs::read_to_string(&command_log).unwrap();
        assert!(
            !text.contains("phase start: archive"),
            "no unit's phases in the command's own log:\n{text}"
        );
        let sessions = crate::cli::status::read_sessions(&logs, 10).unwrap();
        for (unit, stage_set_id) in &staged {
            assert!(text.contains(&format!("staging {unit} (")), "{text}");
            assert!(
                text.contains(&format!("staged {unit} (stage set {stage_set_id})")),
                "{text}"
            );
            let recorded_under: Vec<String> = h
                .conn
                .prepare("SELECT DISTINCT session FROM phase_timings WHERE stage_set_id = ?1")
                .unwrap()
                .query_map([stage_set_id], |r| r.get(0))
                .unwrap()
                .collect::<std::result::Result<_, _>>()
                .unwrap();
            assert_eq!(recorded_under.len(), 1, "{unit}: {recorded_under:?}");
            let log = logs.join(format!("{}.log", recorded_under[0]));
            assert!(
                text.contains(&format!("{unit}: its own log is {}", log.display())),
                "{text}"
            );
            let own = fs::read_to_string(&log).unwrap();
            assert!(own.contains("phase start: archive"), "{unit}:\n{own}");
            for other in staged.iter().map(|(n, _)| n).filter(|n| *n != unit) {
                assert!(
                    !own.contains(other.as_str()),
                    "{unit}'s log names {other}:\n{own}"
                );
            }
            for line in own.lines() {
                let ts = line.split(' ').next().unwrap();
                assert!(
                    chrono::DateTime::parse_from_rfc3339(ts).is_ok(),
                    "every line leads with its timestamp: {line}"
                );
            }
            let s = sessions
                .iter()
                .find(|s| s.session == recorded_under[0])
                .unwrap_or_else(|| panic!("{unit}: no session in {sessions:?}"));
            assert_eq!(s.command.as_deref(), Some(&*format!("stage create {unit}")));
            assert_eq!(
                s.state,
                crate::cli::status::State::Ended {
                    outcome: "ok".into()
                }
            );
            assert!(
                s.phases.iter().any(|p| p.starts_with("archive")),
                "{:?}",
                s.phases
            );
        }
    }

    /// Issue #368: a unit that fails while others are staged beside it
    /// takes nothing from them. The stage running beside it finishes and
    /// stays staged, its slices on disk; no unit starts after the failure;
    /// and the error names the failed unit and the ones that were staged.
    #[test]
    fn a_unit_failing_beside_others_leaves_their_stages_staged() {
        let (h, jobs) = home(&[
            ("u-big", 6, 900_000),
            ("u-bad", 5, 400_000),
            ("u-small", 2, 1000),
        ]);
        // Gone since its snapshot: the stage of u-bad refuses.
        fs::remove_file(h._tmp.path().join("src/u-bad/f3.bin")).unwrap();
        MEMORY_OVERRIDE.with(|m| m.set(Some(64 * MEMORY_PER_JOB)));
        let out = stage_many(
            &h.conn,
            &h.paths,
            &h.config,
            &jobs,
            2,
            false,
            &mut Vec::new(),
            &|_| {},
        )
        .unwrap();
        let big = match &out[0].result {
            Some(Ok(id)) => *id,
            other => panic!("u-big: {other:?}"),
        };
        assert!(matches!(out[1].result, Some(Err(_))), "{:?}", out[1]);
        assert!(
            !matches!(out[2].result, Some(Err(_))),
            "u-small is staged or never started, never failed: {:?}",
            out[2]
        );
        let status: String = h
            .conn
            .query_row("SELECT status FROM stage_sets WHERE id = ?1", [big], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(status, "staged");
        let slices: Vec<String> = h
            .conn
            .prepare("SELECT staging_path FROM stage_slices WHERE stage_set_id = ?1")
            .unwrap()
            .query_map([big], |r| r.get(0))
            .unwrap()
            .collect::<std::result::Result<_, _>>()
            .unwrap();
        assert!(!slices.is_empty());
        for s in &slices {
            assert!(std::path::Path::new(s).is_file(), "{s} is on disk");
        }
        let bad_staged: i64 = h
            .conn
            .query_row(
                "SELECT COUNT(*) FROM stage_sets ss JOIN snapshots s ON s.id = ss.snapshot_id
                 JOIN units u ON u.id = s.unit_id
                 WHERE u.name = 'u-bad' AND ss.status = 'staged'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(bad_staged, 0);
        let err = first_failure(out).unwrap_err().to_string();
        assert!(err.starts_with("u-bad: "), "{err}");
        assert!(err.contains("staged: u-big"), "{err}");
    }
}
