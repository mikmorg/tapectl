//! `tapectl status`: what is running and how the last sessions ended, read
//! from the session logs alone (issue #393).
//!
//! On a production host the catalog, the keys and the session logs live
//! under the service user's home, and live progress prints only on the
//! terminal that started the command. The operator's own account — or an
//! agent helping them — could not tell a running write's phase or a
//! finished one's outcome without sudo; on 2026-10-01 L6-0001's sealed and
//! confirmed write was invisible until the CTO ran a sudo script, and an
//! agent misread it as having ended early.
//!
//! This command opens **no catalog and no config**: everything it shows is
//! in `<home>/logs/`, which `[ops] group` makes readable to one group
//! (`config::OpsConfig`), so a member of that group sees a live session's
//! phase and progress and the last sessions' outcomes, and nothing else.
//! Every session writes its progress line to its log once per interval
//! (`progress::DEFAULT_INTERVAL`), whatever its display, so the newest
//! `progress:` line is never more than that old.

use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::error::{Result, TapectlError};

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
        // `<RFC 3339 UTC> <message>`; anything else (a tracing line wrapped
        // across lines) is not one of ours.
        let Some((ts, msg)) = line.split_once(' ') else {
            continue;
        };
        if !(ts.len() >= 20 && ts.ends_with('Z') && ts.as_bytes()[4] == b'-') {
            continue;
        }
        s.last_line_at = Some(ts.to_string());
        if let Some(rest) = msg.strip_prefix("session start: ") {
            s.started_at = Some(ts.to_string());
            // `<command> (tapectl <ver>, pid <pid>, display <d>)`
            if let Some((cmd, meta)) = rest.rsplit_once(" (tapectl ") {
                s.command = Some(cmd.to_string());
                let mut parts = meta.trim_end_matches(')').split(", ");
                s.tapectl_version = parts.next().map(str::to_string);
                s.pid = parts
                    .next()
                    .and_then(|p| p.strip_prefix("pid "))
                    .and_then(|p| p.parse().ok());
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
        } else if msg.starts_with("session end after ") {
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

/// The sessions to show: every running one among the newest logs, and the
/// `last` newest that are not running, newest first.
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
    // A running session is among the newest; look a little past `last` so
    // a few finished ones after it cannot hide it.
    let window = last.saturating_add(20);
    let mut out = Vec::new();
    let mut finished = 0;
    for name in names.into_iter().take(window) {
        let path = logs_dir.join(&name);
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
            s.started_at.as_deref().unwrap_or("?"),
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
                s.progress_at.as_deref().unwrap_or("?")
            ));
        }
        out.push_str(&format!(
            "  last log line at {}\n  log:      {}\n",
            s.last_line_at.as_deref().unwrap_or("?"),
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
            s.started_at.as_deref().unwrap_or(&s.session),
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

/// `tapectl status`.
pub fn run(logs_dir: &Path, last: usize, json_output: bool) -> Result<()> {
    let sessions = read_sessions(logs_dir, last)?;
    if json_output {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "logs_dir": logs_dir.display().to_string(),
                "sessions": sessions,
            }))
            .unwrap()
        );
    } else {
        print!("{}", render(logs_dir, &sessions));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

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

    #[test]
    fn an_unreadable_logs_directory_names_the_ops_group() {
        let err = read_sessions(Path::new("/nonexistent/tapectl/logs"), 5)
            .unwrap_err()
            .to_string();
        assert!(err.contains("[ops] group"), "{err}");
    }
}
