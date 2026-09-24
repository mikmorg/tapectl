//! `tapectl host check` — the quiet-host check on its own (ADR-0012,
//! 2026-09-24 amendment, item 7), so `scripts/first-run.sh` step 13 and the
//! operator can ask "is this host quiet enough to write a tape?" at any
//! time, not only from inside `volume write`'s pre-flight. Reads `/proc`
//! and, for configured units, systemd; never the database, the drive or the
//! catalog.

use clap::Subcommand;

use crate::config::{Config, HostCheckConfig};
use crate::error::{Result, EXIT_SUCCESS, EXIT_WARNING};
use crate::host_check::{assess, HostSnapshot, REMEDY};

#[derive(Subcommand, Debug)]
pub enum HostCommands {
    /// Is this host quiet enough to write a tape?
    ///
    /// Reports the load average, available memory, memory and I/O pressure,
    /// and any contender process or systemd unit (`[host_check]` in
    /// config.toml), the same check `volume write` runs before it touches
    /// the drive. Exit 0 when quiet, 1 when anything tripped. Never
    /// refuses anything: it only reports.
    Check {
        /// Also check this systemd unit (repeatable), on top of
        /// `[host_check] contender_units`, e.g. a CI runner's timer.
        #[arg(long = "unit", value_name = "UNIT")]
        units: Vec<String>,
    },
}

/// Run a host subcommand; returns the exit code (0 quiet, 1 findings).
/// `config` is `None` when the tapectl home has no config file yet — the
/// check then runs on every default, since it needs nothing else from an
/// initialised home.
pub fn run(config: Option<&Config>, command: &HostCommands, json_output: bool) -> Result<i32> {
    match command {
        HostCommands::Check { units } => {
            let cfg = config.map(Config::host_check).unwrap_or_default();
            let mut all_units = cfg.contender_units.clone();
            for u in units {
                if !all_units.contains(u) {
                    all_units.push(u.clone());
                }
            }
            let snapshot = HostSnapshot::read_live(&all_units);
            let findings = assess(&snapshot, &cfg);
            if json_output {
                println!("{}", render_json(&snapshot, &cfg, &findings));
            } else {
                print!("{}", render_human(&snapshot, &cfg, &findings));
            }
            Ok(if findings.is_empty() {
                EXIT_SUCCESS
            } else {
                EXIT_WARNING
            })
        }
    }
}

fn opt<T: std::fmt::Display>(v: Option<T>, f: impl FnOnce(T) -> String) -> String {
    v.map(f).unwrap_or_else(|| "not available".into())
}

/// What was measured against what, then the verdict. Pure, for the tests.
pub fn render_human(
    snapshot: &HostSnapshot,
    cfg: &HostCheckConfig,
    findings: &[crate::host_check::Finding],
) -> String {
    let cpus = snapshot.cpus.max(1);
    let mut out = String::new();
    let mut line = |label: &str, measured: String, limit: String| {
        out.push_str(&format!("{label:<18} {measured:<40} {limit}\n"));
    };
    line(
        "load average",
        opt(snapshot.load1, |l| {
            format!("{l:.2} over {cpus} CPUs = {:.2}/CPU", l / cpus as f64)
        }),
        format!("max {:.2}/CPU", cfg.max_load_per_cpu),
    );
    line(
        "available memory",
        opt(snapshot.mem_available_kb, |kb| format!("{} MiB", kb / 1024)),
        format!("min {} MiB", cfg.min_available_mb),
    );
    line(
        "memory pressure",
        opt(snapshot.memory_full_avg60, |v| {
            format!("{v:.2}% full avg60")
        }),
        format!("max {:.2}%", cfg.max_memory_pressure_pct),
    );
    line(
        "I/O pressure",
        opt(snapshot.io_full_avg60, |v| format!("{v:.2}% full avg60")),
        format!("max {:.2}%", cfg.max_io_pressure_pct),
    );
    line(
        "processes",
        if cfg.contender_processes.is_empty() {
            "(none listed)".into()
        } else {
            cfg.contender_processes.join(", ")
        },
        "contender_processes".into(),
    );
    line(
        "units",
        if snapshot.units.is_empty() {
            "(none listed)".into()
        } else {
            snapshot
                .units
                .iter()
                .map(|u| u.name.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        },
        "contender_units".into(),
    );
    if findings.is_empty() {
        out.push_str("host check: quiet — nothing listed above is running or over its limit\n");
    } else {
        for f in findings {
            out.push_str(&f.render());
            out.push('\n');
        }
        out.push_str(REMEDY);
        out.push('\n');
    }
    out
}

fn render_json(
    snapshot: &HostSnapshot,
    cfg: &HostCheckConfig,
    findings: &[crate::host_check::Finding],
) -> serde_json::Value {
    serde_json::json!({
        "quiet": findings.is_empty(),
        "measured": {
            "load1": snapshot.load1,
            "cpus": snapshot.cpus,
            "mem_available_mb": snapshot.mem_available_kb.map(|kb| kb / 1024),
            "memory_full_avg60": snapshot.memory_full_avg60,
            "io_full_avg60": snapshot.io_full_avg60,
            "units": snapshot.units.iter().map(|u| u.name.clone()).collect::<Vec<_>>(),
        },
        "limits": cfg,
        "findings": findings.iter().map(|f| serde_json::json!({
            "what": f.what, "measured": f.measured, "threshold": f.threshold,
        })).collect::<Vec<_>>(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::host_check::Finding;

    fn snapshot() -> HostSnapshot {
        HostSnapshot {
            load1: Some(3.2),
            cpus: 16,
            mem_available_kb: Some(6_789_556),
            memory_full_avg60: Some(1.58),
            io_full_avg60: None,
            processes: vec![],
            units: vec![],
        }
    }

    #[test]
    fn a_quiet_host_says_so_and_shows_what_it_measured() {
        let out = render_human(&snapshot(), &HostCheckConfig::default(), &[]);
        assert!(out.contains("3.20 over 16 CPUs = 0.20/CPU"), "{out}");
        assert!(out.contains("6630 MiB"), "{out}");
        // A missing PSI file is said, not hidden.
        assert!(out.contains("not available"), "{out}");
        assert!(out.contains("host check: quiet"), "{out}");
        assert!(!out.contains(REMEDY), "{out}");
    }

    #[test]
    fn findings_are_listed_with_the_remedy() {
        let f = Finding {
            what: "process \"cargo\"".into(),
            measured: "1 running (pid 7)".into(),
            threshold: "listed in contender_processes".into(),
        };
        let out = render_human(
            &snapshot(),
            &HostCheckConfig::default(),
            std::slice::from_ref(&f),
        );
        assert!(out.contains(&f.render()), "{out}");
        assert!(out.contains(REMEDY), "{out}");
        assert!(!out.contains("host check: quiet"), "{out}");
        let json = render_json(&snapshot(), &HostCheckConfig::default(), &[f]);
        assert_eq!(json["quiet"], false);
        assert_eq!(json["findings"][0]["what"], "process \"cargo\"");
    }
}
