//! The quiet-host check (ADR-0012, 2026-09-24 amendment, item 7).
//!
//! "The quiet-host rule is checked, not only stated: step 13 and `volume
//! write`'s pre-flight warn when known contenders are active or the host is
//! loaded or short of memory, and ask for confirmation; never a refusal."
//!
//! Why it matters (`docs/operator-guide.md`, "A quiet host while the tape
//! runs"): an LTO-6 drive stops and restarts whenever the host feeds it
//! slower than about 54 MB/s, and on the real drive a bursty feed cost 48%
//! more tape than a steady one (issue #323). A process killed for memory
//! pressure mid-write costs the cartridge its session.
//!
//! The shape: [`HostSnapshot`] is what was measured, [`assess`] turns it
//! into [`Finding`]s against a [`HostCheckConfig`] — a pure function, so
//! every threshold is tested from synthetic `/proc` text and no test ever
//! reads the real host or spawns a process. [`HostSnapshot::read_live`] is
//! the one place that reads `/proc` and asks systemd, and it only asks
//! systemd about units the config names. [`preflight`] is the consent step
//! `volume write` runs: no findings, no question; findings, the ADR-0008
//! Tier-2 consent helper ([`crate::cli::consent`]) — which `--yes` answers,
//! and which in a non-interactive session without `--yes` declines rather
//! than hanging.

use std::collections::BTreeMap;

use crate::config::HostCheckConfig;
use crate::error::Result;

/// A systemd unit's state as `systemctl show` reported it, or why it could
/// not be asked.
#[derive(Debug, Clone, PartialEq)]
pub struct UnitState {
    pub name: String,
    /// `Ok((load_state, active_state, sub_state))`, or the error text.
    pub state: std::result::Result<(String, String, String), String>,
}

/// What was measured on the host. Every `/proc` figure is optional: a file
/// the kernel does not provide (no PSI, a container) is a check that
/// cannot run, not a finding.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct HostSnapshot {
    /// `/proc/loadavg`'s 1-minute figure.
    pub load1: Option<f64>,
    /// CPUs available to this process.
    pub cpus: usize,
    /// `/proc/meminfo`'s `MemAvailable`, in KiB.
    pub mem_available_kb: Option<u64>,
    /// `/proc/pressure/memory`'s `full avg60`.
    pub memory_full_avg60: Option<f64>,
    /// `/proc/pressure/io`'s `full avg60`.
    pub io_full_avg60: Option<f64>,
    /// `(pid, comm)` of every process, this process and its ancestors
    /// already removed (a `cargo run -- volume write` is not its own
    /// contender).
    pub processes: Vec<(u32, String)>,
    /// Every configured contender unit's state.
    pub units: Vec<UnitState>,
}

/// One reason the host is not quiet.
#[derive(Debug, Clone, PartialEq)]
pub struct Finding {
    /// What was checked, e.g. `"load average"`.
    pub what: String,
    /// What was measured.
    pub measured: String,
    /// The threshold it crossed, or what makes it a contender.
    pub threshold: String,
}

impl Finding {
    pub fn render(&self) -> String {
        format!(
            "host check: {} — {} ({})",
            self.what, self.measured, self.threshold
        )
    }
}

/// The first field of `/proc/loadavg`.
pub fn parse_loadavg(text: &str) -> Option<f64> {
    text.split_whitespace().next()?.parse().ok()
}

/// `MemAvailable` from `/proc/meminfo`, in KiB.
pub fn parse_mem_available_kb(text: &str) -> Option<u64> {
    text.lines().find_map(|line| {
        let rest = line.strip_prefix("MemAvailable:")?;
        rest.split_whitespace().next()?.parse().ok()
    })
}

/// The `full` line's `avg60` from a `/proc/pressure/*` file.
pub fn parse_psi_full_avg60(text: &str) -> Option<f64> {
    let line = text.lines().find(|l| l.starts_with("full "))?;
    line.split_whitespace()
        .find_map(|field| field.strip_prefix("avg60="))?
        .parse()
        .ok()
}

/// The parent pid from `/proc/<pid>/stat`. The command name sits in
/// parentheses and may itself contain spaces or parentheses, so the fields
/// are read from after the LAST `)`: state, then ppid.
pub fn parse_stat_ppid(text: &str) -> Option<u32> {
    let after = &text[text.rfind(')')? + 1..];
    after.split_whitespace().nth(1)?.parse().ok()
}

/// `(LoadState, ActiveState, SubState)` from `systemctl show
/// --property=LoadState,ActiveState,SubState` output.
pub fn parse_systemctl_show(text: &str) -> Option<(String, String, String)> {
    let mut load = None;
    let mut active = None;
    let mut sub = None;
    for line in text.lines() {
        if let Some(v) = line.strip_prefix("LoadState=") {
            load = Some(v.trim().to_string());
        } else if let Some(v) = line.strip_prefix("ActiveState=") {
            active = Some(v.trim().to_string());
        } else if let Some(v) = line.strip_prefix("SubState=") {
            sub = Some(v.trim().to_string());
        }
    }
    Some((load?, active?, sub?))
}

/// A unit competes while it is active — for a timer, while it is armed
/// (`active (waiting)`): the operator guide's remedy is to stop the timer,
/// not merely to wait for its service to finish.
fn unit_is_active(active_state: &str) -> bool {
    matches!(active_state, "active" | "activating" | "reloading")
}

/// Every reason `snapshot` is not a quiet host under `cfg`. Empty means
/// quiet. Pure: no I/O.
pub fn assess(snapshot: &HostSnapshot, cfg: &HostCheckConfig) -> Vec<Finding> {
    let mut findings = Vec::new();

    if let Some(load1) = snapshot.load1 {
        let cpus = snapshot.cpus.max(1);
        let per_cpu = load1 / cpus as f64;
        if per_cpu > cfg.max_load_per_cpu {
            findings.push(Finding {
                what: "load average".into(),
                measured: format!("{load1:.2} over {cpus} CPUs = {per_cpu:.2} per CPU"),
                threshold: format!("max_load_per_cpu {:.2}", cfg.max_load_per_cpu),
            });
        }
    }

    if let Some(kb) = snapshot.mem_available_kb {
        let mb = kb / 1024;
        if mb < cfg.min_available_mb {
            findings.push(Finding {
                what: "available memory".into(),
                measured: format!("{mb} MiB available"),
                threshold: format!("min_available_mb {}", cfg.min_available_mb),
            });
        }
    }

    for (what, value, max, key) in [
        (
            "memory pressure",
            snapshot.memory_full_avg60,
            cfg.max_memory_pressure_pct,
            "max_memory_pressure_pct",
        ),
        (
            "I/O pressure",
            snapshot.io_full_avg60,
            cfg.max_io_pressure_pct,
            "max_io_pressure_pct",
        ),
    ] {
        if let Some(v) = value {
            if v > max {
                findings.push(Finding {
                    what: what.into(),
                    measured: format!("fully stalled {v:.2}% of the last minute"),
                    threshold: format!("{key} {max:.2}"),
                });
            }
        }
    }

    // Grouped by name, in the configured order, so ten `rustc`s are one
    // finding naming ten pids, not ten findings.
    let mut running: BTreeMap<&str, Vec<u32>> = BTreeMap::new();
    for (pid, comm) in &snapshot.processes {
        if cfg.contender_processes.iter().any(|c| c == comm) {
            running.entry(comm.as_str()).or_default().push(*pid);
        }
    }
    for name in &cfg.contender_processes {
        if let Some(pids) = running.remove(name.as_str()) {
            let shown: Vec<String> = pids.iter().take(8).map(|p| p.to_string()).collect();
            let more = if pids.len() > 8 {
                format!(" and {} more", pids.len() - 8)
            } else {
                String::new()
            };
            findings.push(Finding {
                what: format!("process \"{name}\""),
                measured: format!("{} running (pid {}{more})", pids.len(), shown.join(", ")),
                threshold: "listed in contender_processes".into(),
            });
        }
    }

    for unit in &snapshot.units {
        match &unit.state {
            Ok((_, active, sub)) if unit_is_active(active) => findings.push(Finding {
                what: format!("unit {}", unit.name),
                measured: format!("{active} ({sub})"),
                threshold: "listed in contender_units; stop it for the write".into(),
            }),
            Ok(_) => {}
            Err(e) => findings.push(Finding {
                what: format!("unit {}", unit.name),
                measured: format!("could not be checked: {e}"),
                threshold: "listed in contender_units".into(),
            }),
        }
    }

    findings
}

impl HostSnapshot {
    /// Measure this host. Reads `/proc`; runs `systemctl show` once per
    /// entry in `units`, and not at all when it is empty.
    pub fn read_live(units: &[String]) -> Self {
        let read = |p: &str| std::fs::read_to_string(p).ok();
        let excluded = own_ancestry();
        let mut processes = Vec::new();
        if let Ok(entries) = std::fs::read_dir("/proc") {
            for entry in entries.flatten() {
                let Some(pid) = entry
                    .file_name()
                    .to_str()
                    .and_then(|n| n.parse::<u32>().ok())
                else {
                    continue;
                };
                if excluded.contains(&pid) {
                    continue;
                }
                if let Some(comm) = read(&format!("/proc/{pid}/comm")) {
                    processes.push((pid, comm.trim_end_matches('\n').to_string()));
                }
            }
        }
        HostSnapshot {
            load1: read("/proc/loadavg").as_deref().and_then(parse_loadavg),
            cpus: std::thread::available_parallelism()
                .map(|n| n.get())
                .unwrap_or(1),
            mem_available_kb: read("/proc/meminfo")
                .as_deref()
                .and_then(parse_mem_available_kb),
            memory_full_avg60: read("/proc/pressure/memory")
                .as_deref()
                .and_then(parse_psi_full_avg60),
            io_full_avg60: read("/proc/pressure/io")
                .as_deref()
                .and_then(parse_psi_full_avg60),
            processes,
            units: units.iter().map(|u| query_unit(u)).collect(),
        }
    }
}

/// This process and every ancestor up to init.
fn own_ancestry() -> Vec<u32> {
    let mut pids = vec![std::process::id()];
    let mut pid = std::process::id();
    // Bounded: a pid chain is short, and a malformed stat must not loop.
    for _ in 0..64 {
        let Some(ppid) = std::fs::read_to_string(format!("/proc/{pid}/stat"))
            .ok()
            .as_deref()
            .and_then(parse_stat_ppid)
        else {
            break;
        };
        if ppid == 0 || pids.contains(&ppid) {
            break;
        }
        pids.push(ppid);
        pid = ppid;
    }
    pids
}

fn query_unit(unit: &str) -> UnitState {
    let state = std::process::Command::new("systemctl")
        .args([
            "show",
            "--property=LoadState,ActiveState,SubState",
            "--",
            unit,
        ])
        .output()
        .map_err(|e| format!("systemctl: {e}"))
        .and_then(|out| {
            if !out.status.success() {
                return Err(format!(
                    "systemctl show exited {}: {}",
                    out.status,
                    String::from_utf8_lossy(&out.stderr).trim()
                ));
            }
            parse_systemctl_show(&String::from_utf8_lossy(&out.stdout))
                .ok_or_else(|| "systemctl show gave no LoadState/ActiveState/SubState".into())
        });
    UnitState {
        name: unit.to_string(),
        state,
    }
}

/// The findings for this host under `cfg`, with `extra_units` checked too
/// (`host check --unit`).
pub fn check_live(cfg: &HostCheckConfig, extra_units: &[String]) -> Vec<Finding> {
    let mut units = cfg.contender_units.clone();
    for u in extra_units {
        if !units.contains(u) {
            units.push(u.clone());
        }
    }
    assess(&HostSnapshot::read_live(&units), cfg)
}

/// The line that follows the findings wherever they are shown: what to do
/// about them.
pub const REMEDY: &str = "host check: the drive stops and restarts (costing tape) when the host \
     feeds it slower than ~54 MB/s, and a process killed for memory mid-write costs the \
     session — pause what is named above for the duration (`docs/operator-guide.md`, \
     \"A quiet host while the tape runs\")";

/// The pre-flight: measure the host, and when anything trips, ask.
/// `action` names the operation for the prompt (e.g. `volume write
/// "L6-0001"`).
pub fn preflight(cfg: &HostCheckConfig, action: &str, assume_yes: bool) -> Result<()> {
    gate(&check_live(cfg, &[]), action, assume_yes, |a, f, y| {
        crate::cli::consent::confirm(a, f, y)
    })
}

/// [`preflight`] with the findings and the consent step injected.
///
/// No findings: returns without calling `confirm` at all — a quiet host is
/// never asked anything. Findings: each is a fact line for `confirm`, which
/// shows them at the prompt, carries them in its non-interactive refusal,
/// and — because it returns before printing anything when `assume_yes` —
/// they are printed here in that one case, so `--yes` still leaves the
/// findings on the operator's screen and in any log.
pub fn gate(
    findings: &[Finding],
    action: &str,
    assume_yes: bool,
    confirm: impl FnOnce(&str, &[String], bool) -> Result<()>,
) -> Result<()> {
    if findings.is_empty() {
        return Ok(());
    }
    let mut facts: Vec<String> = findings.iter().map(Finding::render).collect();
    facts.push(REMEDY.to_string());
    if assume_yes {
        for fact in &facts {
            eprintln!("{fact}");
        }
    }
    confirm(
        &format!("{action} on a host that is not quiet"),
        &facts,
        assume_yes,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    const LOADAVG_IDLE: &str = "0.52 0.61 0.70 2/1096 2073235\n";
    const LOADAVG_BUSY: &str = "19.84 14.10 9.02 21/1096 2073235\n";
    const MEMINFO_ROOMY: &str = "MemTotal:        9183620 kB\n\
        MemFree:          412345 kB\n\
        MemAvailable:    6789556 kB\n\
        Buffers:          123456 kB\n";
    const MEMINFO_TIGHT: &str = "MemTotal:        9183620 kB\n\
        MemFree:           40000 kB\n\
        MemAvailable:     900000 kB\n";
    const PSI_CALM: &str = "some avg10=6.14 avg60=1.58 avg300=0.41 total=9569285833\n\
        full avg10=6.14 avg60=1.58 avg300=0.38 total=7490187425\n";
    const PSI_STALLED: &str = "some avg10=48.00 avg60=41.20 avg300=12.00 total=1\n\
        full avg10=40.00 avg60=33.50 avg300=10.00 total=1\n";

    fn quiet() -> HostSnapshot {
        HostSnapshot {
            load1: parse_loadavg(LOADAVG_IDLE),
            cpus: 16,
            mem_available_kb: parse_mem_available_kb(MEMINFO_ROOMY),
            memory_full_avg60: parse_psi_full_avg60(PSI_CALM),
            io_full_avg60: parse_psi_full_avg60(PSI_CALM),
            processes: vec![
                (1, "systemd".into()),
                (812, "dockerd".into()),
                (900, "buildkitd".into()),
                (950, "Runner.Listener".into()),
                (1200, "bash".into()),
            ],
            units: vec![],
        }
    }

    #[test]
    fn parsers_read_real_proc_shapes() {
        assert_eq!(parse_loadavg(LOADAVG_BUSY), Some(19.84));
        assert_eq!(parse_mem_available_kb(MEMINFO_ROOMY), Some(6_789_556));
        assert_eq!(parse_psi_full_avg60(PSI_STALLED), Some(33.5));
        // `full`, not `some`: the two lines differ here on purpose.
        assert_eq!(parse_psi_full_avg60(PSI_CALM), Some(1.58));
        assert_eq!(parse_loadavg(""), None);
        assert_eq!(parse_mem_available_kb("MemTotal: 1 kB\n"), None);
        assert_eq!(parse_psi_full_avg60("some avg10=1 avg60=2\n"), None);
    }

    #[test]
    fn stat_ppid_survives_a_command_name_with_spaces_and_parens() {
        assert_eq!(
            parse_stat_ppid("4242 (tokio (rt) w) S 17 4242 4242 0 -1 4194560"),
            Some(17)
        );
        assert_eq!(parse_stat_ppid("1 (systemd) S 0 1 1 0"), Some(0));
        assert_eq!(parse_stat_ppid("garbage"), None);
    }

    #[test]
    fn systemctl_show_output_parses() {
        let text = "LoadState=loaded\nActiveState=active\nSubState=waiting\n";
        assert_eq!(
            parse_systemctl_show(text),
            Some(("loaded".into(), "active".into(), "waiting".into()))
        );
        assert_eq!(parse_systemctl_show("ActiveState=active\n"), None);
    }

    /// The negative control for every test below: the same host with
    /// nothing wrong yields no finding — including the daemons that idle
    /// beside builds all day, which must not trip the default list.
    #[test]
    fn a_quiet_host_yields_no_findings() {
        let findings = assess(&quiet(), &HostCheckConfig::default());
        assert!(findings.is_empty(), "{findings:?}");
    }

    #[test]
    fn high_load_is_a_finding() {
        let mut s = quiet();
        s.load1 = parse_loadavg(LOADAVG_BUSY);
        s.cpus = 8;
        let findings = assess(&s, &HostCheckConfig::default());
        assert_eq!(findings.len(), 1, "{findings:?}");
        assert_eq!(findings[0].what, "load average");
        assert!(
            findings[0].measured.contains("2.48 per CPU"),
            "{findings:?}"
        );
        assert!(findings[0].threshold.contains("max_load_per_cpu 1.00"));
        // The same load over enough CPUs is not saturation.
        s.cpus = 32;
        assert!(assess(&s, &HostCheckConfig::default()).is_empty());
    }

    #[test]
    fn low_available_memory_is_a_finding() {
        let mut s = quiet();
        s.mem_available_kb = parse_mem_available_kb(MEMINFO_TIGHT);
        let findings = assess(&s, &HostCheckConfig::default());
        assert_eq!(findings.len(), 1, "{findings:?}");
        assert_eq!(findings[0].what, "available memory");
        assert!(findings[0].measured.contains("878 MiB"), "{findings:?}");
        assert!(findings[0].threshold.contains("min_available_mb 2048"));
        // 0 turns the check off.
        let cfg = HostCheckConfig {
            min_available_mb: 0,
            ..HostCheckConfig::default()
        };
        assert!(assess(&s, &cfg).is_empty());
    }

    #[test]
    fn a_pressure_line_is_a_finding() {
        let mut s = quiet();
        s.io_full_avg60 = parse_psi_full_avg60(PSI_STALLED);
        let findings = assess(&s, &HostCheckConfig::default());
        assert_eq!(findings.len(), 1, "{findings:?}");
        assert_eq!(findings[0].what, "I/O pressure");
        assert!(findings[0].measured.contains("33.50%"));
        assert!(findings[0].threshold.contains("max_io_pressure_pct"));

        let mut s = quiet();
        s.memory_full_avg60 = parse_psi_full_avg60(PSI_STALLED);
        let findings = assess(&s, &HostCheckConfig::default());
        assert_eq!(findings.len(), 1, "{findings:?}");
        assert_eq!(findings[0].what, "memory pressure");
    }

    #[test]
    fn a_contender_process_is_one_finding_naming_every_pid() {
        let mut s = quiet();
        s.processes.push((3001, "rustc".into()));
        s.processes.push((3002, "rustc".into()));
        s.processes.push((3000, "cargo".into()));
        let findings = assess(&s, &HostCheckConfig::default());
        assert_eq!(findings.len(), 2, "{findings:?}");
        // Configured order, not /proc order.
        assert_eq!(findings[0].what, "process \"cargo\"");
        assert_eq!(findings[1].what, "process \"rustc\"");
        assert!(findings[1]
            .measured
            .starts_with("2 running (pid 3001, 3002"));
        // Exact name match: `cargo-watch` is not `cargo`.
        let mut s = quiet();
        s.processes.push((3003, "cargo-watch".into()));
        assert!(assess(&s, &HostCheckConfig::default()).is_empty());
    }

    #[test]
    fn an_active_contender_unit_is_a_finding_and_an_inactive_or_missing_one_is_not() {
        let mut s = quiet();
        let st = |l: &str, a: &str, sub: &str| Ok((l.into(), a.into(), sub.into()));
        s.units = vec![
            UnitState {
                name: "homorg-db-suite.timer".into(),
                state: st("loaded", "active", "waiting"),
            },
            UnitState {
                name: "homorg-prune-target.timer".into(),
                state: st("loaded", "inactive", "dead"),
            },
            UnitState {
                name: "no-such.timer".into(),
                state: st("not-found", "inactive", "dead"),
            },
            UnitState {
                name: "unaskable.timer".into(),
                state: Err("systemctl: No such file or directory".into()),
            },
        ];
        let findings = assess(&s, &HostCheckConfig::default());
        let whats: Vec<&str> = findings.iter().map(|f| f.what.as_str()).collect();
        assert_eq!(
            whats,
            ["unit homorg-db-suite.timer", "unit unaskable.timer"],
            "{findings:?}"
        );
        assert_eq!(findings[0].measured, "active (waiting)");
        assert!(findings[1].measured.contains("could not be checked"));
    }

    #[test]
    fn missing_proc_files_are_checks_that_cannot_run_not_findings() {
        let s = HostSnapshot {
            cpus: 4,
            ..HostSnapshot::default()
        };
        assert!(assess(&s, &HostCheckConfig::default()).is_empty());
    }

    // ---- the pre-flight's consent, in `cli::consent`'s own test idiom:
    // the TTY answer and the stdin read are injected, never the real ones.

    fn one_finding() -> Vec<Finding> {
        let mut s = quiet();
        s.processes.push((3000, "cargo".into()));
        assess(&s, &HostCheckConfig::default())
    }

    #[test]
    fn a_quiet_host_is_never_asked() {
        gate(&[], "volume write \"L\"", false, |_, _, _| {
            panic!("no finding, so no question")
        })
        .expect("a quiet host proceeds");
    }

    #[test]
    fn a_finding_asks_and_hands_the_consent_every_finding() {
        let findings = one_finding();
        let mut asked = None;
        gate(
            &findings,
            "volume write \"L\"",
            false,
            |action, facts, yes| {
                asked = Some((action.to_string(), facts.to_vec(), yes));
                Ok(())
            },
        )
        .unwrap();
        let (action, facts, yes) = asked.expect("a finding must ask");
        assert!(action.contains("volume write \"L\""), "{action}");
        assert!(!yes);
        assert!(facts[0].contains("process \"cargo\""), "{facts:?}");
        assert_eq!(facts.last().unwrap(), REMEDY);
    }

    #[test]
    fn a_finding_on_a_non_tty_without_yes_declines_with_the_findings_and_never_reads() {
        let findings = one_finding();
        let err = gate(&findings, "volume write \"L\"", false, |a, f, y| {
            crate::cli::consent::confirm_with(a, f, y, false, || {
                panic!("a non-interactive session must never read stdin")
            })
        })
        .expect_err("no one to ask and no --yes: consent is not assumed");
        let msg = err.to_string();
        assert!(msg.contains("--yes"), "{msg}");
        assert!(msg.contains("process \"cargo\""), "{msg}");
    }

    #[test]
    fn yes_answers_it_without_reading() {
        let findings = one_finding();
        gate(&findings, "volume write \"L\"", true, |a, f, y| {
            crate::cli::consent::confirm_with(a, f, y, false, || {
                panic!("--yes must not read stdin")
            })
        })
        .expect("--yes proceeds");
    }

    #[test]
    fn a_declined_prompt_stops_the_write() {
        let findings = one_finding();
        gate(&findings, "volume write \"L\"", false, |a, f, y| {
            crate::cli::consent::confirm_with(a, f, y, true, || Ok("n\n".into()))
        })
        .expect_err("a no at the prompt is a no");
        gate(&findings, "volume write \"L\"", false, |a, f, y| {
            crate::cli::consent::confirm_with(a, f, y, true, || Ok("y\n".into()))
        })
        .expect("a yes at the prompt proceeds");
    }
}
