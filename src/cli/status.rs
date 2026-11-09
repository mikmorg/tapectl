//! `tapectl status`: the home screen (ADR-0012 item 213). The home and why
//! it was chosen; what is running and how the last sessions ended, from the
//! session logs (issue #393); the units by state, the volumes due a verify
//! and at most three next commands, from the catalog.
//!
//! On a production host the catalog, the keys and the session logs live
//! under the service user's home, and live progress prints only on the
//! terminal that started the command. The operator's own account — or an
//! agent helping them — could not tell a running write's phase or a
//! finished one's outcome without sudo; on 2026-10-01 L6-0001's sealed and
//! confirmed write was invisible until the CTO ran a sudo script, and an
//! agent misread it as having ended early.
//!
//! The sessions come from `<home>/logs/` alone, which `[ops] group` makes
//! readable to one group (`config::OpsConfig`), so a member of that group
//! sees a live session's phase and progress and the last sessions'
//! outcomes. The catalog stays closed to that group: for an account that
//! may not open it, `status` shows the home and the sessions and says the
//! overview needs the service user, and exits 0, because that is all such
//! an account is meant to see (the 1.9.0 coordinator's OQ9). It exits 2 only
//! when the catalog cannot be read for a reason nobody designed: missing
//! (not initialized), corrupt, or busy (OQ6). Every session writes its
//! progress line to its log once per interval (`progress::DEFAULT_INTERVAL`),
//! whatever its display, so the newest `progress:` line is never more than
//! that old.

use std::path::{Path, PathBuf};

use rusqlite::Connection;
use serde::Serialize;

use crate::config::Config;
use crate::error::{Result, TapectlError, EXIT_ERROR, EXIT_SUCCESS};
use crate::startup::Startup;

/// One session, as its log tells it.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct SessionStatus {
    /// The log's name without `.log` — also the `phase_timings.session`.
    pub session: String,
    pub log: String,
    /// The command line's name for the session, e.g. `volume write L6-0001`.
    pub command: Option<String>,
    pub tapectl_version: Option<String>,
    pub pid: Option<u32>,
    /// UTC, as the log stamps it.
    pub started_at: Option<String>,
    /// UTC timestamp of the last line.
    pub last_line_at: Option<String>,
    /// `state` (and `outcome`, when it ended with one) at the top level.
    #[serde(flatten)]
    pub state: State,
    /// The phase running now (live) or when the log stopped.
    pub phase: Option<String>,
    /// The newest `progress:` line, without the prefix, and when.
    pub progress: Option<String>,
    pub progress_at: Option<String>,
    /// Every finished phase's one-line summary, in order.
    pub phases: Vec<String>,
}

/// How a session stands.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum State {
    /// No end line and its process is alive.
    Running,
    /// It finished and recorded how: `ok`, or the error it failed with.
    Ended { outcome: String },
    /// It ended (an end line) before tapectl recorded outcomes (< this
    /// version), so the log does not say whether it succeeded.
    EndedUnrecorded,
    /// No end line and no such process: killed (`kill -9`, the OOM killer),
    /// crashed or lost power. The catalog's startup sweep recovers whatever
    /// it was doing on the next command.
    Vanished,
    /// No end line, `/proc` shows no such process, and the log was written
    /// within [`STALE_AFTER`]: the writer is most likely running where this
    /// account cannot see it — `/proc` mounted `hidepid`, or another PID
    /// namespace. A session that is really gone is told apart only by time,
    /// so it reads this way until its log goes quiet past that.
    ProcessNotVisible,
}

/// How long a log with no end line may be quiet before a writer this
/// account cannot see in `/proc` is taken for gone: a few progress
/// intervals, since every session writes its progress line once per
/// [`crate::progress::DEFAULT_INTERVAL`] whatever its display.
pub const STALE_AFTER: std::time::Duration =
    std::time::Duration::from_secs(4 * crate::progress::DEFAULT_INTERVAL.as_secs());

/// Read one log's text into a [`SessionStatus`]. `alive` says whether a
/// pid is a running tapectl as `/proc` shows it, and `now` is the time the
/// log's last line is aged against (both injected so the parse is
/// testable).
pub fn parse_log(
    session: &str,
    log: &Path,
    text: &str,
    alive: impl Fn(u32) -> bool,
    now: chrono::DateTime<chrono::Utc>,
) -> SessionStatus {
    let mut s = SessionStatus {
        session: session.to_string(),
        log: log.display().to_string(),
        command: None,
        tapectl_version: None,
        pid: None,
        started_at: None,
        last_line_at: None,
        state: State::Vanished,
        phase: None,
        progress: None,
        progress_at: None,
        phases: Vec::new(),
    };
    let mut open: Vec<String> = Vec::new();
    let mut ended = false;
    let mut result: Option<String> = None;
    let mut exit_code: Option<i32> = None;
    for line in text.lines() {
        let Some((ts, msg)) = split_line(line) else {
            continue;
        };
        s.last_line_at = Some(ts.to_string());
        if let Some(rest) = msg.strip_prefix("session start: ") {
            s.started_at = Some(ts.to_string());
            // `<command> (tapectl <ver>, pid <pid>, display <d>)`
            if let Some((cmd, meta)) = rest.rsplit_once(" (tapectl ") {
                s.command = Some(cmd.to_string());
                // The version itself holds ", " in a stamped build
                // (`1.1.0 (v1.1.0, 2026-10-13)`), so split at ", pid ",
                // never at the first comma: that read `2026-10-13)` as the
                // pid and called every running session killed (home2,
                // 2026-10-14, a live `volume write`).
                let meta = meta.strip_suffix(')').unwrap_or(meta);
                match meta.rsplit_once(", pid ") {
                    Some((version, rest)) => {
                        s.tapectl_version = Some(version.to_string());
                        s.pid = rest.split(", ").next().and_then(|p| p.parse().ok());
                    }
                    None => s.tapectl_version = Some(meta.to_string()),
                }
            } else {
                s.command = Some(rest.to_string());
            }
        } else if let Some(rest) = msg.strip_prefix("phase start: ") {
            let name = rest.split(" (").next().unwrap_or(rest).to_string();
            open.push(name);
        } else if let Some(rest) = msg.strip_prefix("phase end: ") {
            let name = rest.split_whitespace().next().unwrap_or("").to_string();
            if let Some(i) = open.iter().rposition(|p| *p == name) {
                open.remove(i);
            }
            s.phases.push(rest.trim().to_string());
        } else if let Some(rest) = msg.strip_prefix("progress: ") {
            // `progress: done: …` lines are a display summary, not a state.
            if !rest.starts_with("done:") && !rest.starts_with("FAILED:") {
                s.progress = Some(rest.to_string());
                s.progress_at = Some(ts.to_string());
            }
        } else if let Some(rest) = msg.strip_prefix("session result: ") {
            result = Some(rest.to_string());
        } else if let Some(rest) = msg.strip_prefix("session exit with code ") {
            exit_code = rest.split_whitespace().next().and_then(|c| c.parse().ok());
            ended = true;
        } else if is_end_line(msg) {
            ended = true;
        }
    }
    s.phase = open.last().cloned();
    s.state = if ended {
        match (result, exit_code) {
            (Some(r), None | Some(0)) => State::Ended { outcome: r },
            (Some(r), Some(code)) if r == "ok" => State::Ended {
                outcome: format!("exit {code}"),
            },
            (Some(r), Some(_)) => State::Ended { outcome: r },
            (None, Some(code)) => State::Ended {
                outcome: if code == 0 {
                    "ok".to_string()
                } else {
                    format!("exit {code}")
                },
            },
            (None, None) => State::EndedUnrecorded,
        }
    } else if s.pid.is_some_and(&alive) {
        State::Running
    } else if s.pid.is_some() && written_within(s.last_line_at.as_deref(), now, STALE_AFTER) {
        // Issue #393: `/proc` cannot tell a process hidden from this account
        // (hidepid, another PID namespace) from one that is gone; a log
        // still being written can.
        State::ProcessNotVisible
    } else {
        State::Vanished
    };
    s
}

/// One of ours: `<RFC 3339 UTC> <message>`, split into its stamp and
/// message. Anything else (a tracing line wrapped across lines) is not.
fn split_line(line: &str) -> Option<(&str, &str)> {
    let (ts, msg) = line.split_once(' ')?;
    (ts.len() >= 20 && ts.ends_with('Z') && ts.as_bytes()[4] == b'-').then_some((ts, msg))
}

/// Whether `msg` is a session's end line: the session's own, or the one
/// written before a `process::exit`.
fn is_end_line(msg: &str) -> bool {
    msg.starts_with("session end after ") || msg.starts_with("session exit with code ")
}

/// Whether any line of `text` is an end line. `text` may be a log's tail
/// with a partial first line, which matches only if it is a whole line of
/// ours anyway.
fn has_end_line(text: &str) -> bool {
    text.lines()
        .filter_map(split_line)
        .any(|(_, msg)| is_end_line(msg))
}

/// Whether the RFC 3339 stamp `at` is no older than `within` at `now`. An
/// unreadable stamp, or none, is not recent.
fn written_within(
    at: Option<&str>,
    now: chrono::DateTime<chrono::Utc>,
    within: std::time::Duration,
) -> bool {
    let Some(at) = at.and_then(|a| chrono::DateTime::parse_from_rfc3339(a).ok()) else {
        return false;
    };
    let age = now.signed_duration_since(at.with_timezone(&chrono::Utc));
    chrono::Duration::from_std(within).is_ok_and(|w| age <= w)
}

/// Whether `pid` is a running tapectl: `/proc/<pid>` exists and, where its
/// `comm` is readable, names tapectl (a recycled pid running something else
/// is not this session).
pub fn tapectl_is_running(pid: u32) -> bool {
    let proc_dir = PathBuf::from(format!("/proc/{pid}"));
    if !proc_dir.exists() {
        return false;
    }
    match std::fs::read_to_string(proc_dir.join("comm")) {
        Ok(comm) => comm.trim().starts_with("tapectl"),
        Err(_) => true,
    }
}

/// How much of a log's end [`read_sessions`] reads to tell a finished
/// session from one that may be running. The end line is written last, or
/// nearly (a tracing line can land after it); a log with more than this
/// past its end line is read in full, never misjudged.
const TAIL_BYTES: u64 = 16 * 1024;

/// The sessions to show: every running one, whatever its age, and the
/// `last` newest that are not running, newest first.
///
/// Every log is looked at, not only the newest few: a running command can
/// have any number of newer logs after it (its own child sessions, one per
/// unit of a `--jobs` stage; timers; other commands), and the logs are
/// never pruned (ADR-0012 2026-10-07 item 13). So once the finished list is
/// full, a log is read in full only when its tail has no end line.
pub fn read_sessions(logs_dir: &Path, last: usize) -> Result<Vec<SessionStatus>> {
    let entries = std::fs::read_dir(logs_dir).map_err(|e| unreadable(logs_dir, &e))?;
    let mut names: Vec<String> = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|e| unreadable(logs_dir, &e))?;
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.ends_with(".log") {
            names.push(name);
        }
    }
    // Names begin with the UTC start (`20260930T120000Z-…`): name order is
    // start order.
    names.sort();
    names.reverse();
    let mut out = Vec::new();
    let mut finished = 0;
    for name in names {
        let path = logs_dir.join(&name);
        // Once the finished list is full, only a log that may be running
        // is worth reading: one whose tail has no end line. A log that
        // cannot be read is not one (it is named, below, only while the
        // finished list has room).
        if finished >= last && !matches!(read_tail(&path), Ok(tail) if !has_end_line(&tail)) {
            continue;
        }
        let text = match std::fs::read_to_string(&path) {
            Ok(t) => t,
            // A log the reader may not open (created before `[ops] group`
            // was set, so mode 0600) is named, not skipped silently.
            Err(e) => {
                let mut s = parse_log(
                    name.trim_end_matches(".log"),
                    &path,
                    "",
                    |_| false,
                    chrono::Utc::now(),
                );
                s.state = State::Ended {
                    outcome: format!("unreadable: {e}"),
                };
                if finished < last {
                    out.push(s);
                    finished += 1;
                }
                continue;
            }
        };
        let s = parse_log(
            name.trim_end_matches(".log"),
            &path,
            &text,
            tapectl_is_running,
            chrono::Utc::now(),
        );
        if s.is_probably_running() {
            out.push(s);
        } else if finished < last {
            out.push(s);
            finished += 1;
        }
    }
    Ok(out)
}

/// The last [`TAIL_BYTES`] of `path`, lossily decoded: the cut can fall
/// inside a character or a line, and [`has_end_line`] allows for both.
fn read_tail(path: &Path) -> std::io::Result<String> {
    use std::io::{Read, Seek, SeekFrom};
    let mut f = std::fs::File::open(path)?;
    let len = f.metadata()?.len();
    f.seek(SeekFrom::Start(len.saturating_sub(TAIL_BYTES)))?;
    let mut buf = Vec::new();
    f.take(TAIL_BYTES).read_to_end(&mut buf)?;
    Ok(String::from_utf8_lossy(&buf).into_owned())
}

impl SessionStatus {
    /// Running, or a writer this account cannot see whose log is still
    /// being written: listed under `running:` either way.
    fn is_probably_running(&self) -> bool {
        matches!(self.state, State::Running | State::ProcessNotVisible)
    }
}

fn unreadable(dir: &Path, e: &std::io::Error) -> TapectlError {
    TapectlError::Other(format!(
        "cannot read the session logs in {}: {e}. They belong to the user tapectl runs \
         as; another account reads them as a member of the group named by `[ops] group` \
         in that home's config.toml (docs/operator-guide.md, \"Watching from another \
         account\"). Point at the home with --home or TAPECTL_HOME.",
        dir.display()
    ))
}

/// A session's time as the human screen shows times (item 212: one format,
/// labelled UTC), `?` when the log did not say.
fn shown_time(at: Option<&str>) -> String {
    at.map_or_else(|| "?".to_string(), crate::render::table_time)
}

/// The human rendering.
pub fn render(logs_dir: &Path, sessions: &[SessionStatus]) -> String {
    let mut out = format!("session logs: {}\n", logs_dir.display());
    let running: Vec<&SessionStatus> = sessions
        .iter()
        .filter(|s| s.is_probably_running())
        .collect();
    if running.is_empty() {
        out.push_str("running: nothing\n");
    }
    for s in &running {
        out.push_str(&format!(
            "running: {} (pid {}), started {}\n",
            s.command.as_deref().unwrap_or("?"),
            s.pid.map_or("?".to_string(), |p| p.to_string()),
            shown_time(s.started_at.as_deref()),
        ));
        if s.state == State::ProcessNotVisible {
            out.push_str(
                "  process not visible from this account (/proc hides it: hidepid, or \
                 another PID namespace); its log is still being written\n",
            );
        }
        if let Some(p) = &s.phase {
            out.push_str(&format!("  phase:    {p}\n"));
        }
        if let Some(p) = &s.progress {
            out.push_str(&format!(
                "  progress: {p} (at {})\n",
                shown_time(s.progress_at.as_deref())
            ));
        }
        out.push_str(&format!(
            "  last log line at {}\n  log:      {}\n",
            shown_time(s.last_line_at.as_deref()),
            s.log
        ));
    }
    let done: Vec<&SessionStatus> = sessions
        .iter()
        .filter(|s| !s.is_probably_running())
        .collect();
    if !done.is_empty() {
        out.push_str("recent:\n");
    }
    for s in done {
        let outcome = match &s.state {
            State::Ended { outcome } => outcome.clone(),
            State::EndedUnrecorded => "ended (outcome not recorded)".to_string(),
            State::Vanished => "ended with no end line: killed, crashed or power lost".to_string(),
            State::Running | State::ProcessNotVisible => unreachable!(),
        };
        out.push_str(&format!(
            "  {}  {}  — {}\n",
            s.started_at
                .as_deref()
                .map_or_else(|| s.session.clone(), crate::render::table_time),
            s.command.as_deref().unwrap_or("?"),
            outcome
        ));
        if let Some(p) = &s.phase {
            out.push_str(&format!("      stopped in phase {p}\n"));
        }
        for p in &s.phases {
            out.push_str(&format!("      {p}\n"));
        }
    }
    out
}

/// The line an account that may not open the catalog is shown in place of
/// the overview (OQ9).
pub const NEEDS_SERVICE_USER: &str = "the overview needs the service user: tapectl-op status";

/// What `status` could make of the catalog.
#[derive(Debug)]
pub enum Overview {
    /// It read the catalog: the overview.
    Read(Box<HomeScreen>),
    /// This account may not open the catalog (an `[ops] group` member): the
    /// designed view, exit 0.
    NeedsServiceUser,
    /// The catalog could not be read (missing, corrupt, busy, a config that
    /// does not load): why, exit 2.
    Unreadable(String),
}

impl Overview {
    /// OQ6: 0 when `status` read the catalog, or showed the view an account
    /// without it is meant to have; 2 when it could not read it.
    pub fn exit_code(&self) -> i32 {
        match self {
            Overview::Read(_) | Overview::NeedsServiceUser => EXIT_SUCCESS,
            Overview::Unreadable(_) => EXIT_ERROR,
        }
    }
}

/// A volume due a verify (`report verify-status`'s and `audit`'s rules): one
/// that counts as a copy with no passed full readback, or whose newest one
/// is older than the shortest `verify_interval_days` of the units on it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DueVerify {
    pub label: String,
    /// The newest passed full readback, dated by its start; `None` when
    /// there is none.
    pub last_full: Option<String>,
    /// The interval it is past; `None` when it was never read back in full.
    pub interval_days: Option<i64>,
}

/// The catalog's half of the home screen: the units by state, the volumes
/// due a verify and the interrupted write sessions, with what the next
/// commands are built from.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HomeScreen {
    /// Active units.
    pub active: usize,
    /// Pending, Never Archived: no snapshot at all.
    pub never_archived: Vec<String>,
    /// Pending, Dirty: the source no longer matches the snapshot.
    pub dirty: Vec<String>,
    /// Units whose source the dirty scan could not read.
    pub not_scanned: Vec<String>,
    /// Units with a stage set held in staging that still owes a copy.
    pub staged: Vec<String>,
    /// Units below their copy floor (Never Archived ones are pending, not
    /// counted here).
    pub under_copied: Vec<String>,
    /// Units whose policy does not resolve, so their copies cannot be judged.
    pub policy_unresolvable: Vec<String>,
    pub due_verify: Vec<DueVerify>,
    /// Write targets with an interrupted write session.
    pub interrupted: Vec<String>,
    /// The collection each pending unit belongs to, in pending order (name,
    /// collection); a unit in none is absent.
    pub pending_collections: Vec<(String, String)>,
}

impl HomeScreen {
    /// Pending: Never Archived and Dirty (CONTEXT.md).
    pub fn pending(&self) -> usize {
        self.never_archived.len() + self.dirty.len()
    }

    /// At most three next commands, in OQ9's order: an interrupted session,
    /// pending units, staged or under-copied units, volumes due a verify.
    pub fn next_commands(&self) -> Vec<String> {
        let mut next = Vec::new();
        if let Some((first, rest)) = self.interrupted.split_first() {
            let more = if rest.is_empty() {
                String::new()
            } else {
                format!("  ({} more interrupted: {})", rest.len(), rest.join(", "))
            };
            next.push(format!("tapectl volume resume {first}{more}"));
        }
        let pending: Vec<&String> = self.never_archived.iter().chain(&self.dirty).collect();
        if !pending.is_empty() {
            next.push(match self.pending_collections.first() {
                Some((_, collection)) => {
                    format!(
                        "tapectl collection run {collection} --label <NEW-LABEL> --init --full-confirm --move-to <a short location>"
                    )
                }
                // One pending unit is named; several are one flag (item 225).
                None if pending.len() == 1 => format!("tapectl snapshot create {}", pending[0]),
                None => {
                    let shown: Vec<&str> = pending.iter().take(3).map(|n| n.as_str()).collect();
                    let more = if pending.len() > 3 { ", …" } else { "" };
                    format!(
                        "tapectl snapshot create --all-pending  ({} units: {}{more})",
                        pending.len(),
                        shown.join(", ")
                    )
                }
            });
        }
        if !self.staged.is_empty() {
            next.push(
                "tapectl volume write <NEW-LABEL> --init --full-confirm --move-to <a short location>"
                    .to_string(),
            );
        } else if !self.under_copied.is_empty() {
            // Nothing staged to write: the copy floor is met by staging the
            // units again first (a write takes only what is staged).
            let units: Vec<&String> = self.under_copied.iter().collect();
            next.push(format!("tapectl stage create {}", some_names(&units)));
        }
        if let Some(due) = self.due_verify.first() {
            next.push(format!("tapectl volume verify {}", due.label));
        }
        next.truncate(3);
        next
    }
}

/// Up to three names for a command line, and how many more there are.
fn some_names(names: &[&String]) -> String {
    const SHOWN: usize = 3;
    let shown: Vec<&str> = names.iter().take(SHOWN).map(|n| n.as_str()).collect();
    if names.len() <= SHOWN {
        shown.join(" ")
    } else {
        format!("{}  ({} more)", shown.join(" "), names.len() - SHOWN)
    }
}

/// Read the catalog's half of the home screen. `config` is the home's
/// config, loaded as `report` loads it.
pub fn gather(conn: &Connection, config: &Config) -> Result<HomeScreen> {
    let mut screen = HomeScreen::default();
    for row in crate::cli::report::dirty_rows(conn, None, &config.defaults.global_excludes)? {
        screen.active += 1;
        match row.state {
            "new" => screen.never_archived.push(row.name),
            "dirty" => screen.dirty.push(row.name),
            "unreadable" => screen.not_scanned.push(row.name),
            _ => {}
        }
    }
    screen.pending_collections = pending_collections(
        conn,
        config,
        screen.never_archived.iter().chain(&screen.dirty),
    )?;

    let mut staged: Vec<String> = crate::cli::stage::stage_set_rows(
        conn,
        config,
        crate::cli::stage::Shown::Status("staged"),
    )?
    .into_iter()
    .filter(|r| r.owing)
    .map(|r| r.unit)
    .collect();
    staged.dedup();
    screen.staged = staged;

    for row in crate::cli::report::fire_risk_rows(conn, config)? {
        match row.risk {
            crate::cli::report::FireRisk::PolicyUnresolvable(_) => {
                screen.policy_unresolvable.push(row.unit)
            }
            _ if screen.never_archived.contains(&row.unit) => {}
            _ => screen.under_copied.push(row.unit),
        }
    }

    screen.due_verify = due_verify(conn, config.defaults.verify_interval_days)?;
    screen.interrupted = interrupted_write_targets(conn)?;
    Ok(screen)
}

/// The configured collection each of `pending` lies under (by path, as
/// `collection status` finds its units), in `pending`'s order.
fn pending_collections<'a>(
    conn: &Connection,
    config: &Config,
    pending: impl Iterator<Item = &'a String>,
) -> Result<Vec<(String, String)>> {
    let roots: Vec<(String, String)> = config
        .collections
        .iter()
        .filter_map(|c| {
            crate::collection::canonical_root(c)
                .ok()
                .map(|root| (c.name.clone(), root))
        })
        .collect();
    if roots.is_empty() {
        return Ok(Vec::new());
    }
    let mut out = Vec::new();
    for name in pending {
        let Some(unit) = crate::db::queries::get_unit_by_name(conn, name)? else {
            continue;
        };
        let Some(path) = unit.current_path.as_deref() else {
            continue;
        };
        if let Some((collection, _)) = roots
            .iter()
            .find(|(_, root)| crate::collection::is_under_root(path, root))
        {
            out.push((name.clone(), collection.clone()));
        }
    }
    Ok(out)
}

/// The volumes due a verify, the never-read-back first (in label order),
/// then those past their interval, oldest readback first.
///
/// The first half is `report verify-status`'s and `audit`'s `no_full_verify`
/// query ([`crate::policy::evidence::sealed_without_full_verify`]). The
/// second is `audit`'s per-volume clock (`volume_verify_age`): each volume
/// that counts as a copy and holds a completed write or a range, its newest
/// passed full readback dated by its start, against the shortest
/// `verify_interval_days` of the units on it (an archive set's, else
/// `defaults_interval`); past it by whole days, or with a date that does not
/// parse, it is due.
fn due_verify(conn: &Connection, defaults_interval: i64) -> Result<Vec<DueVerify>> {
    let mut due: Vec<DueVerify> = crate::policy::evidence::sealed_without_full_verify(conn, None)?
        .into_iter()
        .map(|label| DueVerify {
            label,
            last_full: None,
            interval_days: None,
        })
        .collect();
    let mut overdue =
        crate::policy::evidence::volumes_past_verify_interval(conn, Some(defaults_interval))?;
    // The longest overdue first, as before the rule was shared.
    overdue.sort_by(|x, y| x.last_full.cmp(&y.last_full).then(x.label.cmp(&y.label)));
    due.extend(overdue.into_iter().map(|v| DueVerify {
        label: v.label,
        last_full: Some(v.last_full),
        interval_days: Some(v.interval_days),
    }));
    Ok(due)
}

/// Write targets with an interrupted write session (`volume resume`
/// continues it). A crashed session's rows were turned `interrupted` by the
/// catalog's open; a live one's stay `in_progress` and show as running.
fn interrupted_write_targets(conn: &Connection) -> Result<Vec<String>> {
    let rows: Vec<(String, String, String)> = conn
        .prepare(
            "SELECT DISTINCT v.label, v.status, v.observed_condition
               FROM volumes v JOIN writes w ON w.volume_id = v.id
              WHERE w.status = 'interrupted'
              ORDER BY v.label",
        )?
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
        .collect::<std::result::Result<_, _>>()?;
    Ok(rows
        .into_iter()
        .filter(|(_, status, condition)| {
            crate::policy::coverage::is_write_target(status, condition)
        })
        .map(|(label, _, _)| label)
        .collect())
}

/// Open the home's catalog for the overview, deciding before SQLite sees the
/// file (which reports a missing file and a forbidden one alike): missing is
/// "not initialized", forbidden is the `[ops] group` view, and anything else
/// that stops the read is an error.
pub fn overview(startup: &Startup) -> Overview {
    let paths = &startup.paths;
    match std::fs::File::open(&paths.db_file) {
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Overview::Unreadable(crate::startup::not_initialized_message(startup));
        }
        Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => {
            return Overview::NeedsServiceUser;
        }
        Err(e) => {
            return Overview::Unreadable(format!(
                "cannot open the catalog {}: {e}",
                paths.db_file.display()
            ));
        }
    }
    // `status` never reaches a drive, so a backend collision does not stop
    // it (the #261 reasoning `report` follows).
    let config = match Config::load_tolerating_backend_ambiguity(&paths.config_file) {
        Ok(c) => c,
        Err(e) => return Overview::Unreadable(format!("failed to load config: {e}")),
    };
    let conn = match crate::db::open(&paths.db_file) {
        Ok(c) => c,
        Err(e) => return Overview::Unreadable(format!("failed to open the catalog: {e}")),
    };
    match gather(&conn, &config) {
        Ok(screen) => Overview::Read(Box::new(screen)),
        Err(e) => Overview::Unreadable(format!("cannot read the catalog: {e}")),
    }
}

/// The catalog half of the human screen.
pub fn render_overview(overview: &Overview) -> String {
    let screen = match overview {
        Overview::Read(screen) => screen,
        Overview::NeedsServiceUser => return format!("{NEEDS_SERVICE_USER}\n"),
        // Said on stderr, as an error.
        Overview::Unreadable(_) => return String::new(),
    };
    use crate::render::plural;
    let mut out = format!("units: {} active\n", screen.active);
    let pending = if screen.pending() == 0 {
        "0".to_string()
    } else {
        format!(
            "{} ({} never archived, {} dirty)",
            screen.pending(),
            screen.never_archived.len(),
            screen.dirty.len()
        )
    };
    let mut rows = vec![
        vec!["pending".to_string(), pending],
        vec![
            "staged".to_string(),
            format!("{}, awaiting a write", screen.staged.len()),
        ],
        vec![
            "under-copied".to_string(),
            format!("{}, below their copy floor", screen.under_copied.len()),
        ],
    ];
    if !screen.not_scanned.is_empty() {
        rows.push(vec![
            "not scanned".to_string(),
            format!(
                "{} (the source could not be read: tapectl report dirty says why)",
                screen.not_scanned.len()
            ),
        ]);
    }
    if !screen.policy_unresolvable.is_empty() {
        rows.push(vec![
            "unjudged".to_string(),
            format!(
                "{} (the policy does not resolve: tapectl audit says why)",
                screen.policy_unresolvable.len()
            ),
        ]);
    }
    out.push_str(&indent(&crate::render::table_with(
        &["state", "units"],
        rows,
        None,
    )));
    if screen.due_verify.is_empty() {
        out.push_str("volumes due a verify: none\n");
    } else {
        out.push_str(&format!(
            "volumes due a verify: {}\n",
            plural(screen.due_verify.len() as i64, "volume", "volumes")
        ));
        let rows = screen
            .due_verify
            .iter()
            .map(|d| {
                let why = match (&d.last_full, d.interval_days) {
                    (Some(at), Some(days)) => format!(
                        "last read back in full {}, past its {days}-day interval",
                        crate::render::table_time(at)
                    ),
                    _ => "never read back in full".to_string(),
                };
                vec![d.label.clone(), why]
            })
            .collect();
        out.push_str(&indent(&crate::render::table(&["volume", "why"], rows)));
    }
    let next = screen.next_commands();
    if next.is_empty() {
        out.push_str("next: nothing to do\n");
    } else {
        out.push_str("next:\n");
        for n in next {
            out.push_str(&format!("  {n}\n"));
        }
    }
    out
}

/// `text`'s lines, each indented two spaces.
fn indent(text: &str) -> String {
    text.lines().map(|l| format!("  {l}\n")).collect()
}

/// The `--json` document: `logs_dir` and `sessions` as before 1.9.0, and the
/// home, the units by state, the volumes due a verify and the next commands
/// (`null` where the catalog was not read, with `note` or `error` saying
/// why).
pub fn overview_json(
    startup: &Startup,
    sessions: &[SessionStatus],
    overview: &Overview,
) -> serde_json::Value {
    let mut doc = serde_json::json!({
        "home": startup.paths.home.display().to_string(),
        "home_source": startup.source.key(),
        "home_pointer": match &startup.source {
            crate::startup::HomeSource::Pointer(p) => Some(p.display().to_string()),
            _ => None,
        },
        "logs_dir": startup.paths.logs_dir.display().to_string(),
        "sessions": sessions,
        "units": null,
        "due_verify": null,
        "interrupted": null,
        "next": null,
    });
    match overview {
        Overview::Read(s) => {
            doc["units"] = serde_json::json!({
                "active": s.active,
                "pending": s.pending(),
                "never_archived": s.never_archived,
                "dirty": s.dirty,
                "not_scanned": s.not_scanned,
                "staged": s.staged,
                "under_copied": s.under_copied,
                "policy_unresolvable": s.policy_unresolvable,
            });
            doc["due_verify"] = s
                .due_verify
                .iter()
                .map(|d| {
                    serde_json::json!({
                        "volume": d.label,
                        "last_full": d.last_full.as_deref().map(crate::render::json_time),
                        "interval_days": d.interval_days,
                    })
                })
                .collect();
            doc["interrupted"] = serde_json::json!(s.interrupted);
            doc["next"] = serde_json::json!(s.next_commands());
        }
        Overview::NeedsServiceUser => doc["note"] = serde_json::json!(NEEDS_SERVICE_USER),
        // The JSON error every command prints under `--json` (ADR-0012 item
        // 212), its two keys beside what `status` could show.
        Overview::Unreadable(why) => {
            if let (Some(doc), serde_json::Value::Object(error)) = (
                doc.as_object_mut(),
                crate::render::json_error(why, EXIT_ERROR),
            ) {
                doc.extend(error);
            }
        }
    }
    doc
}

/// `tapectl status`: its exit code (OQ6).
pub fn run(startup: &Startup, last: usize, json_output: bool) -> Result<i32> {
    let logs_dir = &startup.paths.logs_dir;
    // The home is named first, whatever follows: even when the session logs
    // cannot be read (an account outside the `[ops] group`).
    if !json_output {
        println!("{}", startup.home_line());
    }
    // A home with no logs directory yet has run no session; any other
    // failure to read it is said as before.
    let sessions = match std::fs::symlink_metadata(logs_dir) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Vec::new(),
        _ => read_sessions(logs_dir, last)?,
    };
    let overview = overview(startup);
    if json_output {
        crate::render::print_json(&overview_json(startup, &sessions, &overview));
    } else {
        print!(
            "{}{}",
            render(logs_dir, &sessions),
            render_overview(&overview)
        );
    }
    // The error line goes to stderr under `--json` too, as every command's
    // does (`error::exit_reporting_with`): a script reading stderr sees it.
    if let Overview::Unreadable(why) = &overview {
        eprintln!("error: {why}");
    }
    Ok(overview.exit_code())
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::params;

    const RUNNING: &str = "\
2026-10-01T03:12:40.118Z session start: volume write L6-0001 (tapectl 1.1.0, pid 4242, display Lines)
2026-10-01T03:12:40.200Z phase start: contact-open
2026-10-01T03:12:42.600Z phase end: contact-open        2.4 s
2026-10-01T03:12:42.700Z phase start: write (1.20 TiB)
2026-10-01T03:13:12.700Z progress: write 4.2 GiB of 1.20 TiB (0.3%), 143.0 MiB/s, ETA 2h 26m, elapsed 30 s — file 5 of 43
2026-10-01T03:13:13.000Z wrote file 5 (data_slice): 1.00 GiB in 7.0 s; tape waited 0.1 s for data, queue full 6.2 s
";

    /// Long after every fixture's last line.
    fn later() -> chrono::DateTime<chrono::Utc> {
        "2026-10-02T00:00:00Z".parse().unwrap()
    }

    fn parse(text: &str, alive: bool) -> SessionStatus {
        parse_at(text, alive, later())
    }

    fn parse_at(text: &str, alive: bool, now: chrono::DateTime<chrono::Utc>) -> SessionStatus {
        parse_log(
            "20261001T031240Z-volume-write-L6-0001-4242",
            Path::new("/x.log"),
            text,
            |_| alive,
            now,
        )
    }

    #[test]
    fn a_live_session_shows_its_phase_and_progress() {
        let s = parse(RUNNING, true);
        assert_eq!(s.state, State::Running);
        assert_eq!(s.command.as_deref(), Some("volume write L6-0001"));
        assert_eq!(s.pid, Some(4242));
        assert_eq!(s.tapectl_version.as_deref(), Some("1.1.0"));
        assert_eq!(s.phase.as_deref(), Some("write"));
        assert!(s
            .progress
            .as_deref()
            .unwrap()
            .starts_with("write 4.2 GiB of 1.20 TiB"));
        assert_eq!(s.progress_at.as_deref(), Some("2026-10-01T03:13:12.700Z"));
        assert_eq!(s.phases, ["contact-open        2.4 s"]);
    }

    #[test]
    fn a_session_whose_process_is_gone_says_so() {
        let s = parse(RUNNING, false);
        assert_eq!(s.state, State::Vanished);
        assert_eq!(s.phase.as_deref(), Some("write"), "where it stopped");
        assert!(render(Path::new("/l"), &[s]).contains("killed, crashed or power lost"));
    }

    /// Issue #393: `/proc` hides another user's process under `hidepid`
    /// (and another PID namespace's always), so a pid this account cannot
    /// see is not proof the writer died. While its log is still being
    /// written it is "process not visible", listed as running; once the log
    /// has been quiet for a few progress intervals, it ended with no end
    /// line.
    /// home2, 2026-10-14: a stamped build's version holds ", "
    /// (`1.1.0 (v1.1.0, 2026-10-13)`), and the pid was read from the first
    /// comma on, so a live `volume write` showed as killed.
    #[test]
    fn a_stamped_version_does_not_hide_the_pid() {
        let text = "2026-10-14T12:27:06.939Z session start: volume write L6-0001 \
                    (tapectl 1.1.0 (v1.1.0, 2026-10-13), pid 6109, display Lines)\n\
                    2026-10-14T12:27:07.018Z phase start: write\n";
        let s = parse_log("s", Path::new("/l"), text, |pid| pid == 6109, later());
        assert_eq!(s.pid, Some(6109));
        assert_eq!(
            s.tapectl_version.as_deref(),
            Some("1.1.0 (v1.1.0, 2026-10-13)")
        );
        assert_eq!(s.command.as_deref(), Some("volume write L6-0001"));
        assert!(matches!(s.state, State::Running), "{:?}", s.state);
    }

    /// The header the session writer really writes parses back to this
    /// process's pid: the writer and this parser cannot drift apart again.
    #[test]
    fn the_writers_own_header_parses_back_to_its_pid() {
        let dir = tempfile::tempdir().unwrap();
        {
            let _session = crate::progress::start_session(
                Some(dir.path()),
                "volume write L6-0001",
                crate::progress::Display::Off,
                false,
            );
        }
        let log = std::fs::read_dir(dir.path())
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path();
        let text = std::fs::read_to_string(&log).unwrap();
        let s = parse_log("s", &log, &text, |_| true, later());
        assert_eq!(s.pid, Some(std::process::id()), "{text}");
        assert_eq!(
            s.tapectl_version.as_deref(),
            Some(crate::build_info::VERSION)
        );
    }

    #[test]
    fn a_writer_hidden_from_proc_is_not_called_killed_while_its_log_is_fresh() {
        let last_line: chrono::DateTime<chrono::Utc> = "2026-10-01T03:13:13.000Z".parse().unwrap();
        let fresh = last_line + chrono::Duration::seconds(45);
        let s = parse_at(RUNNING, false, fresh);
        assert_eq!(s.state, State::ProcessNotVisible);
        let text = render(Path::new("/l"), &[s]);
        assert!(text.contains("running: volume write L6-0001"), "{text}");
        assert!(text.contains("process not visible"), "{text}");
        assert!(!text.contains("killed"), "{text}");

        let stale = last_line
            + chrono::Duration::from_std(STALE_AFTER).unwrap()
            + chrono::Duration::seconds(1);
        assert_eq!(parse_at(RUNNING, false, stale).state, State::Vanished);
        // A visible live process is Running whatever the log's age.
        assert_eq!(parse_at(RUNNING, true, stale).state, State::Running);
    }

    #[test]
    fn a_finished_session_reports_its_outcome() {
        let ok = format!(
            "{RUNNING}2026-10-01T05:40:00.000Z phase end: write  2h 27m  1.20 TiB  142.9 MiB/s\n\
             2026-10-01T05:40:01.000Z session result: ok\n\
             2026-10-01T05:40:01.100Z session end after 2h 27m\n"
        );
        let s = parse(&ok, true);
        assert_eq!(
            s.state,
            State::Ended {
                outcome: "ok".into()
            },
            "an end line wins over a recycled pid"
        );
        assert_eq!(s.phase, None);
        assert_eq!(s.phases.len(), 2);

        let failed = format!(
            "{RUNNING}2026-10-01T03:20:00.000Z phase end: write  7m  60 GiB  (failed)\n\
             2026-10-01T03:20:00.100Z session result: failed — stopped by a signal: …\n\
             2026-10-01T03:20:00.200Z session end after 7m\n"
        );
        match parse(&failed, false).state {
            State::Ended { outcome } => assert!(outcome.starts_with("failed"), "{outcome}"),
            other => panic!("{other:?}"),
        }

        // `volume verify` exits 2/3 through `process::exit`, which logs its
        // own line instead of a result.
        let exited =
            format!("{RUNNING}2026-10-01T04:00:00.000Z session exit with code 3 after 47m\n");
        assert_eq!(
            parse(&exited, false).state,
            State::Ended {
                outcome: "exit 3".into()
            }
        );

        // A log from before outcomes were recorded.
        let old = format!("{RUNNING}2026-10-01T05:40:01.100Z session end after 2h 27m\n");
        assert_eq!(parse(&old, false).state, State::EndedUnrecorded);
    }

    #[test]
    fn running_sessions_are_listed_whatever_their_age_and_the_rest_up_to_last() {
        let dir = tempfile::tempdir().unwrap();
        let finished = |cmd: &str| {
            format!(
                "2026-10-01T00:00:00.000Z session start: {cmd} (tapectl 1.1.0, pid 1, display Off)\n\
                 2026-10-01T00:00:01.000Z session result: ok\n\
                 2026-10-01T00:00:01.000Z session end after 1.0 s\n"
            )
        };
        for i in 0..8 {
            std::fs::write(
                dir.path()
                    .join(format!("2026100{i}T000000Z-stage-create-u-1.log")),
                finished("stage create u"),
            )
            .unwrap();
        }
        // The live one: this very process.
        let me = std::process::id();
        std::fs::write(
            dir.path().join("20261009T000000Z-volume-write-L-9.log"),
            format!(
                "2026-10-09T00:00:00.000Z session start: volume write L (tapectl 1.1.0, pid {me}, display Off)\n\
                 2026-10-09T00:00:00.100Z phase start: write (1 GiB)\n"
            ),
        )
        .unwrap();
        std::fs::write(dir.path().join("not-a-log.txt"), "x").unwrap();

        let got = read_sessions(dir.path(), 3).unwrap();
        // This test binary's comm is not "tapectl…" — the live one is
        // only Running where /proc says a tapectl owns the pid.
        let running = got.iter().filter(|s| s.state == State::Running).count();
        assert!(running <= 1);
        assert_eq!(
            got.len(),
            4,
            "the newest 3 that are not running, plus the live one: {got:#?}"
        );
        assert_eq!(
            got.iter()
                .filter(|s| matches!(&s.state, State::Ended { outcome } if outcome == "ok"))
                .count(),
            3
        );
    }

    /// A `collection run --jobs N` logs each unit to a child session whose
    /// name sorts after the command's own; the command, still running (it
    /// writes the tape after the staging), must not be pushed out of the
    /// listing by however many of its children finished after it started.
    #[test]
    fn a_running_session_is_listed_however_many_finished_logs_are_newer() {
        let dir = tempfile::tempdir().unwrap();
        let me = std::process::id();
        // Stamped now: read as Running where /proc names this binary a
        // tapectl, and as ProcessNotVisible (a fresh log) where it does not.
        let now = chrono::Utc::now().format("%Y-%m-%dT%H:%M:%S%.3fZ");
        std::fs::write(
            dir.path()
                .join(format!("20261012T000000Z-collection-run-k-{me}.log")),
            format!(
                "{now} session start: collection run k (tapectl 1.1.0, pid {me}, display Off)\n\
                 {now} phase start: write (1 GiB)\n"
            ),
        )
        .unwrap();
        let last = 5;
        for i in 0..last + 21 {
            std::fs::write(
                dir.path()
                    .join(format!("20261012T0001{i:02}Z-stage-create-u{i}-{me}.log")),
                format!(
                    "2026-10-12T00:01:{i:02}.000Z session start: stage create u{i} (tapectl 1.1.0, pid {me}, display Off)\n\
                     2026-10-12T00:01:{i:02}.500Z session result: ok\n\
                     2026-10-12T00:01:{i:02}.500Z session end after 0.5 s\n"
                ),
            )
            .unwrap();
        }

        let got = read_sessions(dir.path(), last).unwrap();
        let running: Vec<_> = got.iter().filter(|s| s.is_probably_running()).collect();
        assert_eq!(running.len(), 1, "{got:#?}");
        assert_eq!(running[0].command.as_deref(), Some("collection run k"));
        assert_eq!(
            got.iter().filter(|s| !s.is_probably_running()).count(),
            last,
            "the newest {last} finished: {got:#?}"
        );
    }

    /// Past the finished list, a log whose end line sits further back than
    /// the tail that is read is read in full and still counts as finished.
    #[test]
    fn a_finished_log_with_a_long_tail_past_its_end_line_is_not_called_running() {
        let dir = tempfile::tempdir().unwrap();
        let mut text = String::from(
            "2026-10-01T00:00:00.000Z session start: volume verify L (tapectl 1.1.0, pid 1, display Off)\n\
             2026-10-01T00:00:01.000Z session result: ok\n\
             2026-10-01T00:00:01.000Z session end after 1.0 s\n",
        );
        while text.len() as u64 <= 2 * TAIL_BYTES {
            text.push_str("2026-10-01T00:00:01.001Z  INFO tapectl: a late tee line\n");
        }
        std::fs::write(
            dir.path().join("20261001T000000Z-volume-verify-L-1.log"),
            text,
        )
        .unwrap();
        std::fs::write(
            dir.path().join("20261002T000000Z-audit-1.log"),
            "2026-10-02T00:00:00.000Z session start: audit (tapectl 1.1.0, pid 1, display Off)\n\
             2026-10-02T00:00:01.000Z session result: ok\n\
             2026-10-02T00:00:01.000Z session end after 1.0 s\n",
        )
        .unwrap();
        let got = read_sessions(dir.path(), 1).unwrap();
        assert_eq!(got.len(), 1, "{got:#?}");
        assert_eq!(got[0].command.as_deref(), Some("audit"));
    }

    /// The tail check that keeps `status` from reading every finished log in
    /// full finds an end line wherever it sits in the tail, and only one
    /// that is a timestamped line of its own.
    #[test]
    fn the_tail_check_finds_an_end_line_and_only_a_real_one() {
        assert!(has_end_line(&format!(
            "{RUNNING}2026-10-01T05:40:01.000Z session result: ok\n\
             2026-10-01T05:40:01.100Z session end after 2h 27m\n"
        )));
        // A tee line can land after the end line.
        assert!(has_end_line(
            "2026-10-01T05:40:01.100Z session end after 2h 27m\n\
             2026-10-01T05:40:01.101Z  INFO tapectl: late\n"
        ));
        assert!(has_end_line(
            "2026-10-01T05:40:01.100Z session exit with code 4 after 1 s\n"
        ));
        assert!(!has_end_line(RUNNING));
        assert!(!has_end_line(
            "2026-10-01T05:40:01.101Z  INFO x: session end after 3 s\nsession end after 3 s\n"
        ));
    }

    // ---- the home screen (ADR-0012 item 213) ----

    /// A catalog in memory and source folders on disk.
    struct Catalog {
        conn: Connection,
        tenant: i64,
        dir: tempfile::TempDir,
    }

    impl Catalog {
        fn new() -> Self {
            let conn = crate::db::open_memory().unwrap();
            conn.execute(
                "INSERT INTO tenants (name, is_operator, status) VALUES ('t', 0, 'active')",
                [],
            )
            .unwrap();
            let tenant = conn.last_insert_rowid();
            Catalog {
                conn,
                tenant,
                dir: tempfile::tempdir().unwrap(),
            }
        }

        /// An active unit; `with_source` gives it a folder holding one file.
        fn unit(&self, name: &str, with_source: bool) -> i64 {
            let path = with_source.then(|| {
                let p = self.dir.path().join(name);
                std::fs::create_dir_all(&p).unwrap();
                std::fs::write(p.join("f.txt"), "x").unwrap();
                p.display().to_string()
            });
            self.conn
                .execute(
                    "INSERT INTO units (uuid, name, tenant_id, current_path, checksum_mode,
                                        encrypt, status)
                     VALUES (?1, ?1, ?2, ?3, 'mtime_size', 1, 'active')",
                    params![name, self.tenant, path],
                )
                .unwrap();
            self.conn.last_insert_rowid()
        }

        /// Version 1 of `unit`, current; its snapshot id.
        fn snapshot(&self, unit: i64) -> i64 {
            self.conn
                .execute(
                    "INSERT INTO snapshots (unit_id, version, snapshot_type, status, source_path)
                     VALUES (?1, 1, 'full', 'current', '/src')",
                    params![unit],
                )
                .unwrap();
            self.conn.last_insert_rowid()
        }

        /// A staged stage set of `snapshot`; its id.
        fn staged(&self, snapshot: i64) -> i64 {
            self.conn
                .execute(
                    "INSERT INTO stage_sets (snapshot_id, status, slice_size, num_slices)
                     VALUES (?1, 'staged', 1048576, 1)",
                    params![snapshot],
                )
                .unwrap();
            self.conn.last_insert_rowid()
        }

        /// A volume `label` in `status`, condition ok.
        fn volume(&self, label: &str, status: &str) -> i64 {
            self.conn
                .execute(
                    "INSERT INTO volumes (label, backend_type, backend_name, media_type,
                                          capacity_bytes, status, sealed_at)
                     VALUES (?1, 'lto', 'lto0', 'LTO-6', 2500000000000, ?2,
                             CASE WHEN ?2 = 'sealed' THEN '2026-10-21 10:00:00' END)",
                    params![label, status],
                )
                .unwrap();
            self.conn.last_insert_rowid()
        }

        /// A write of `set` to `volume` in `status`.
        fn write(&self, set: i64, volume: i64, status: &str) {
            self.conn
                .execute(
                    "INSERT INTO writes (stage_set_id, snapshot_id, volume_id, status)
                     SELECT id, snapshot_id, ?2, ?3 FROM stage_sets WHERE id = ?1",
                    params![set, volume, status],
                )
                .unwrap();
        }

        /// A passed full readback of `volume`, started `days` ago.
        fn read_back(&self, volume: i64, days: i64) {
            let at = (chrono::Utc::now().naive_utc() - chrono::Duration::days(days))
                .format("%Y-%m-%d %H:%M:%S")
                .to_string();
            self.conn
                .execute(
                    "INSERT INTO verification_sessions
                         (volume_id, verify_type, outcome, started_at, completed_at)
                     VALUES (?1, 'full', 'passed', ?2, ?2)",
                    params![volume, at],
                )
                .unwrap();
        }
    }

    /// Every state the screen counts, from one catalog: a never-archived
    /// unit, a staged one owing its copy (so under-copied too), an
    /// interrupted write, a sealed copy never read back in full and one read
    /// back past its interval.
    #[test]
    fn the_home_screen_counts_units_by_state_and_names_what_is_due() {
        let c = Catalog::new();
        c.unit("fresh", true);
        let held = c.unit("held", false);
        let set = c.staged(c.snapshot(held));
        let copied = c.unit("copied", false);
        let copied_set = c.staged(c.snapshot(copied));
        c.conn
            .execute(
                "UPDATE stage_sets SET status = 'cleaned' WHERE id = ?1",
                params![copied_set],
            )
            .unwrap();
        let never = c.volume("L-NEVER", "sealed");
        c.write(copied_set, never, "completed");
        let stale = c.volume("L-STALE", "sealed");
        c.write(copied_set, stale, "completed");
        c.read_back(stale, 400);
        let fresh_read = c.volume("L-FRESH", "sealed");
        c.write(copied_set, fresh_read, "completed");
        c.read_back(fresh_read, 10);
        let open = c.volume("L-INT", "initialized");
        c.write(set, open, "interrupted");

        let screen = gather(&c.conn, &Config::default()).unwrap();
        assert_eq!(screen.active, 3);
        assert_eq!(screen.never_archived, ["fresh"]);
        assert!(screen.dirty.is_empty(), "{screen:?}");
        assert_eq!(screen.pending(), 1);
        assert_eq!(screen.staged, ["held"]);
        assert_eq!(
            screen.under_copied,
            ["held"],
            "the never-archived unit is pending, not counted twice"
        );
        assert_eq!(screen.interrupted, ["L-INT"]);
        let due: Vec<(&str, Option<i64>)> = screen
            .due_verify
            .iter()
            .map(|d| (d.label.as_str(), d.interval_days))
            .collect();
        assert_eq!(
            due,
            [("L-NEVER", None), ("L-STALE", Some(365))],
            "never read back first, then past the default interval; L-FRESH is not due"
        );

        assert_eq!(
            screen.next_commands(),
            [
                "tapectl volume resume L-INT",
                "tapectl snapshot create fresh",
                "tapectl volume write <NEW-LABEL> --init --full-confirm --move-to <a short location>",
            ],
            "OQ9's order, at most three"
        );

        let text = render_overview(&Overview::Read(Box::new(screen)));
        assert!(text.starts_with("units: 3 active\n"), "{text}");
        assert!(
            text.contains("next:\n  tapectl volume resume L-INT\n"),
            "{text}"
        );
        // Column widths are the render kit's; the words are this screen's.
        let flat = text.split_whitespace().collect::<Vec<_>>().join(" ");
        for want in [
            "pending 1 (1 never archived, 0 dirty)",
            "staged 1, awaiting a write",
            "under-copied 1, below their copy floor",
            "volumes due a verify: 2 volumes",
            "L-NEVER never read back in full",
            "past its 365-day interval",
        ] {
            assert!(flat.contains(want), "{want:?} in:\n{text}");
        }
    }

    /// OQ9's order holds whichever categories are present, and a volume to
    /// verify comes last.
    #[test]
    fn the_next_commands_follow_oq9_order() {
        let screen = |f: &dyn Fn(&mut HomeScreen)| {
            let mut s = HomeScreen::default();
            f(&mut s);
            s.next_commands()
        };
        let due = DueVerify {
            label: "L1".into(),
            last_full: None,
            interval_days: None,
        };
        assert!(screen(&|_| {}).is_empty());
        assert_eq!(
            screen(&|s| {
                s.dirty = vec!["d".into()];
                s.due_verify = vec![due.clone()];
            }),
            ["tapectl snapshot create d", "tapectl volume verify L1"]
        );
        // Under-copied with nothing staged: stage first, a write takes only
        // what is staged.
        assert_eq!(
            screen(&|s| s.under_copied = vec!["a".into(), "b".into()]),
            ["tapectl stage create a b"]
        );
        // A pending unit in a collection: the collection run.
        assert_eq!(
            screen(&|s| {
                s.never_archived = vec!["movies/x".into()];
                s.pending_collections = vec![("movies/x".into(), "movies".into())];
            }),
            ["tapectl collection run movies --label <NEW-LABEL> --init --full-confirm --move-to <a short location>"]
        );
        // Several interrupted sessions: the first, and the others named.
        assert_eq!(
            screen(&|s| s.interrupted = vec!["A".into(), "B".into()]),
            ["tapectl volume resume A  (1 more interrupted: B)"]
        );
        // Every category: the first three, the verify dropped.
        let all = screen(&|s| {
            s.interrupted = vec!["A".into()];
            s.never_archived = vec!["n1".into(), "n2".into(), "n3".into(), "n4".into()];
            s.staged = vec!["st".into()];
            s.due_verify = vec![due.clone()];
        });
        assert_eq!(all.len(), 3, "{all:?}");
        assert_eq!(
            all[1],
            "tapectl snapshot create --all-pending  (4 units: n1, n2, n3, …)"
        );
        assert!(all[2].starts_with("tapectl volume write"), "{all:?}");
    }

    fn startup_at(home: &Path) -> Startup {
        crate::startup::resolve_from(Some(home.to_str().unwrap()), None, None, None, None).unwrap()
    }

    /// OQ6: no catalog is "not initialized", exit 2, naming the path; JSON
    /// keeps `logs_dir` and `sessions` and says why in `error`.
    #[test]
    fn a_missing_catalog_is_not_initialized_and_exits_2() {
        let home = tempfile::tempdir().unwrap();
        let startup = startup_at(home.path());
        let o = overview(&startup);
        assert_eq!(o.exit_code(), EXIT_ERROR);
        match &o {
            Overview::Unreadable(why) => {
                assert!(why.contains("not initialized"), "{why}");
                assert!(why.contains(&home.path().display().to_string()), "{why}");
            }
            other => panic!("{other:?}"),
        }
        assert!(
            !home.path().join("tapectl.db").exists(),
            "status made no catalog"
        );
        let json = overview_json(&startup, &[], &o);
        assert_eq!(
            json["logs_dir"],
            home.path().join("logs").display().to_string()
        );
        assert_eq!(json["sessions"], serde_json::json!([]));
        assert_eq!(json["home_source"], "home_flag");
        assert_eq!(json["exit_code"], EXIT_ERROR);
        assert!(json["units"].is_null() && json["next"].is_null(), "{json}");
    }

    /// OQ9: an account that may not open the catalog is shown its part and
    /// told who sees the rest; that is the designed view, exit 0.
    #[test]
    fn a_catalog_this_account_may_not_open_is_the_ops_view_and_exits_0() {
        use std::os::unix::fs::PermissionsExt;
        if nix::unistd::geteuid().is_root() {
            eprintln!("skipped: root opens a mode-0 file");
            return;
        }
        let home = tempfile::tempdir().unwrap();
        let db = home.path().join("tapectl.db");
        std::fs::write(&db, "").unwrap();
        std::fs::set_permissions(&db, std::fs::Permissions::from_mode(0o000)).unwrap();
        let startup = startup_at(home.path());
        let o = overview(&startup);
        assert!(matches!(o, Overview::NeedsServiceUser), "{o:?}");
        assert_eq!(o.exit_code(), EXIT_SUCCESS);
        assert_eq!(render_overview(&o), format!("{NEEDS_SERVICE_USER}\n"));
        assert!(NEEDS_SERVICE_USER.contains("tapectl-op status"));
        let json = overview_json(&startup, &[], &o);
        assert_eq!(json["note"], NEEDS_SERVICE_USER);
        assert!(json.get("error").is_none(), "{json}");
    }

    #[test]
    fn an_unreadable_logs_directory_names_the_ops_group() {
        let err = read_sessions(Path::new("/nonexistent/tapectl/logs"), 5)
            .unwrap_err()
            .to_string();
        assert!(err.contains("[ops] group"), "{err}");
    }
}
