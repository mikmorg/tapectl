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

/// What one stage in flight may hold of the host's memory: the source it
/// keeps in the page cache between dar's read and the hasher's
/// (`validate::READ_AHEAD_BYTES`), and its own buffers and queues — dar's
/// stream buffer, the slice file's, and the bounded hash queues — with room
/// to spare.
pub const MEMORY_PER_JOB: u64 = super::validate::READ_AHEAD_BYTES + (64 << 20);

/// One unit to stage: its name, for the operator, and the snapshot to stage.
#[derive(Debug, Clone)]
pub struct StageJob {
    pub unit_name: String,
    pub snapshot_id: i64,
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
fn mem_available() -> Option<u64> {
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

/// How many of `units` units to stage at once for `requested`: at least 1,
/// at most [`MAX_JOBS`] and the number of units, and no more than
/// `available` bytes of memory hold at [`MEMORY_PER_JOB`] each. A request
/// cut down by memory is said on `notices`.
pub(crate) fn effective_jobs(
    requested: usize,
    units: usize,
    available: Option<u64>,
    notices: &mut dyn Write,
) -> usize {
    let wanted = requested.clamp(1, MAX_JOBS).min(units.max(1));
    let Some(available) = available else {
        return wanted;
    };
    let fits = usize::try_from(available / MEMORY_PER_JOB)
        .unwrap_or(usize::MAX)
        .max(1);
    if fits < wanted {
        let _ = writeln!(
            notices,
            "note: staging {fits} unit(s) at once, not {wanted}: the host has {} of memory \
             available, and each stage in flight keeps up to {} of its source in the page \
             cache plus its own buffers",
            crate::util::format_bytes_binary(i64::try_from(available).unwrap_or(i64::MAX)),
            crate::util::format_bytes_binary(MEMORY_PER_JOB as i64),
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
        effective_jobs(requested_jobs, jobs.len(), mem_available(), notices)
    } else {
        1
    };
    let total = jobs.len();
    let many = total > 1;
    let results: Vec<OnceLock<Result<i64>>> = jobs.iter().map(|_| OnceLock::new()).collect();
    let started = AtomicUsize::new(0);
    let run_one = |c: &Connection, cfg: &Config, i: usize| -> bool {
        let job = &jobs[i];
        let n = started.fetch_add(1, Ordering::AcqRel) + 1;
        if many {
            announce(&format!("staging {} ({n} of {total})", job.unit_name));
        }
        #[cfg(test)]
        let _running = super::lock::db_file_of(c).map(|d| in_flight::Running::enter(&d));
        let result = super::stage_create_reporting(
            c,
            paths,
            cfg,
            job.snapshot_id,
            assume_yes,
            &mut std::io::stderr(),
        );
        let ok = result.is_ok();
        if many {
            match &result {
                Ok(id) => announce(&format!("staged {} (stage set {id})", job.unit_name)),
                Err(e) => announce(&format!("failed {}: {e}", job.unit_name)),
            }
        }
        let _ = results[i].set(result);
        ok
    };

    match (parallel, db_file) {
        (p, Some(db_file)) if p > 1 => {
            let order = largest_first(conn, jobs)?;
            // The cores are shared: each stage hashes with its share of them
            // (issue #366's threads), never fewer than one.
            let cores = std::thread::available_parallelism()
                .map(std::num::NonZeroUsize::get)
                .unwrap_or(1);
            let mut cfg = config.clone();
            cfg.staging.hash_threads = (cores / p).clamp(1, config.staging.hash_threads.max(1));
            let next = AtomicUsize::new(0);
            let failed = AtomicBool::new(false);
            std::thread::scope(|s| {
                for w in 0..p {
                    let (order, next, failed, cfg, db_file, run_one, results) =
                        (&order, &next, &failed, &cfg, &db_file, &run_one, &results);
                    let spawned = std::thread::Builder::new()
                        .name(format!("tapectl-stage-{w}"))
                        .spawn_scoped(s, move || {
                            let _capture = crate::progress::start_capture();
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
                                if !run_one(c, cfg, i) {
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
                if !run_one(conn, config, i) {
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
        config.dar.binary = "dar".into();
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

    /// Per unit: its stage sets' statuses, slice count and dar size, and its
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
                        "SELECT ss.status, COALESCE(ss.num_slices, 0), COALESCE(ss.total_dar_size, 0)
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
                // Not the dar size (s.2): dar records each file's times, which differ
                // between the two trees, at a width that follows their values.
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
        let mut notices = Vec::new();
        assert_eq!(effective_jobs(0, 5, None, &mut notices), 1);
        assert_eq!(effective_jobs(4, 2, None, &mut notices), 2);
        assert_eq!(effective_jobs(100, 100, None, &mut notices), MAX_JOBS);
        assert!(notices.is_empty());
        assert_eq!(
            effective_jobs(8, 10, Some(3 * MEMORY_PER_JOB + 1), &mut notices),
            3
        );
        let said = String::from_utf8(notices).unwrap();
        assert!(said.contains("staging 3 unit(s) at once, not 8"), "{said}");
        let mut quiet = Vec::new();
        assert_eq!(
            effective_jobs(2, 10, Some(1), &mut quiet),
            1,
            "never fewer than one"
        );
    }
}
