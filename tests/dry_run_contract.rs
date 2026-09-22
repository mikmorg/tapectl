//! Pins the CLASS half of issue #241 (itself #230's own note): a test that
//! walks every clap leaf and asserts a decision was made about `--dry-run`
//! — HONOUR it or REFUSE it — with none left silently ignoring it.
//!
//! `--dry-run` is `#[arg(long, global = true)]` (`src/cli/mod.rs`), so clap
//! accepts it on EVERY subcommand whether or not that subcommand's own code
//! ever looks at it. Before issue #241, eighteen dispatchers plus ten
//! `volume` arms plus `cartridge register` received the flag (or never
//! received it at all) and mutated anyway — accepting a promise
//! ("Show what would be done without making changes") the command never
//! kept.
//!
//! # What this file proves, and what it does NOT
//!
//! Two tests:
//!
//! 1. [`every_clap_leaf_has_exactly_one_table_verdict`] walks the real clap
//!    tree (`tapectl::cli::Cli::command()`) and checks it against [`TABLE`]
//!    below: every leaf command appears exactly once, and `TABLE` names no
//!    leaf that doesn't exist. This is the regression guard — a NEW
//!    subcommand added later with no entry here fails the build, which is
//!    the whole point: the class cannot silently regrow.
//!
//! 2. [`every_refuses_verdict_refuses_before_doing_anything`] spawns the
//!    real binary for every [`Verdict::Refuses`] entry with `--dry-run` and
//!    asserts it exits non-zero and names the flag in its refusal. Because
//!    every refusal in this codebase is written as the FIRST statement of
//!    its `match` arm (before any lookup, before any device is opened),
//!    `probe_args` need not resolve to anything real — a refusal that
//!    fired after even one lookup would make this test's own dummy labels
//!    a false pass, which is exactly why "first statement of the arm" is
//!    the rule, not a suggestion.
//!
//! **What is NOT proven here:**
//!
//! - [`Verdict::Honours`] entries are NOT re-verified by this file. Their
//!   dry run actually leaving every row untouched is proven by a DB-row
//!   assertion in `tests/dry_run_global.rs` (the shape #230 established:
//!   `ok()`/`db()`/`count()` against a real temp `HOME`) — a static
//!   contract test cannot see whether a preview lied about what it would
//!   do, only whether the command answers the question at all. Claiming
//!   more here would be exactly the "green for the wrong reason" failure
//!   mode this project has hit before.
//! - [`Verdict::ReadOnly`] entries are not probed at all. They were read
//!   during this fix (grepped for `INSERT`/`UPDATE`/`DELETE`/
//!   `conn.execute`/`std::fs::write` in their command body) and confirmed
//!   to perform no write, so accepting `--dry-run` and doing exactly what
//!   they always do already satisfies the promise — asserting that a
//!   read-only command doesn't write is circular, not a contract.
//!
//! Issue #247 closed the last nine leaves that issue #241 had to fence off
//! to a concurrent worker (`db backup`/`export`/`fsck`/`import`/`stats` and
//! the four top-level `main.rs` dispatchers `export`/`import`/`init`/
//! `quick-archive`) — [`TABLE`] carries no "fenced off, not fixed" verdict
//! any more, and the enum variant that spelled one has been removed
//! entirely so it cannot silently regrow: a future leaf this branch cannot
//! fix must invent its own documented escape hatch, not resurrect this one
//! by habit.

use clap::CommandFactory;
use std::collections::HashSet;
use std::process::Command;
use tempfile::TempDir;

/// One clap leaf's dry-run disposition.
#[derive(Debug)]
enum Verdict {
    /// Writes nothing; `--dry-run` is accepted and ignored because ignoring
    /// it is indistinguishable from honouring it. See the module doc.
    ReadOnly,
    /// Refuses `--dry-run` outright, as the first statement of its arm,
    /// before any lookup or resource is touched. `probe_args` is the
    /// minimal syntactically-valid tail (after the leaf's own path words)
    /// that reaches the refusal — values never need to resolve to real
    /// rows or devices.
    Refuses { probe_args: &'static [&'static str] },
    /// Runs its own preview and makes no change — proven in
    /// `tests/dry_run_global.rs`, not here.
    Honours,
}

/// Every clap leaf, one verdict apiece. Order follows the clap tree
/// (alphabetical by subcommand, matching `Cli::command()`'s own listing).
const TABLE: &[(&[&str], Verdict)] = &[
    (&["archive-set", "create"], Verdict::Honours),
    (&["archive-set", "edit"], Verdict::Honours),
    (&["archive-set", "info"], Verdict::ReadOnly),
    (&["archive-set", "list"], Verdict::ReadOnly),
    (
        &["archive-set", "sync"],
        Verdict::Refuses { probe_args: &[] },
    ),
    (&["audit"], Verdict::ReadOnly),
    (&["backend", "add"], Verdict::Honours),
    // cartridge: register is this branch's fix; the other six were already
    // fixed (issue #230, and the ADR-0012 gated-consent work that followed
    // it) before this branch started — confirmed by reading, not re-fixed.
    (&["cartridge", "edit"], Verdict::Honours),
    (&["cartridge", "info"], Verdict::ReadOnly),
    // Issue #297: reads `mam_journal`, writes nothing.
    (&["cartridge", "journal"], Verdict::ReadOnly),
    (&["cartridge", "list"], Verdict::ReadOnly),
    (&["cartridge", "mark-erased"], Verdict::Honours),
    (&["cartridge", "move"], Verdict::Honours),
    (&["cartridge", "register"], Verdict::Honours),
    (&["cartridge", "relabel"], Verdict::Honours),
    (&["cartridge", "retire"], Verdict::Honours),
    (&["cartridge", "unretire"], Verdict::Honours),
    (&["catalog", "locate"], Verdict::ReadOnly),
    (&["catalog", "ls"], Verdict::ReadOnly),
    (
        &["catalog", "rebuild"],
        Verdict::Refuses {
            probe_args: &[
                "--from-volume",
                "--key",
                "/tmp/tapectl-dry-run-contract-nonexistent",
            ],
        },
    ),
    (&["catalog", "search"], Verdict::ReadOnly),
    (&["catalog", "stats"], Verdict::ReadOnly),
    // collection: fixed by issue #230, predating this branch.
    (&["collection", "plan"], Verdict::ReadOnly),
    (&["collection", "run"], Verdict::Honours),
    (&["collection", "status"], Verdict::ReadOnly),
    (&["collection", "sync"], Verdict::Honours),
    (&["completions"], Verdict::ReadOnly),
    (&["config", "check"], Verdict::ReadOnly),
    (&["config", "show"], Verdict::ReadOnly),
    // db: fixed by issue #247, which closed the fence issue #241 had to
    // draw around `src/cli/db.rs` while issue #233 (the `db::open` failure
    // path) ran concurrently.
    (&["db", "backup"], Verdict::Honours),
    (&["db", "export"], Verdict::ReadOnly),
    (&["db", "fsck"], Verdict::Honours),
    // `db import` was ALREADY correct — `cli::db::run` forwarded `dry_run`
    // to `operations::db_import` before this branch started, which reports
    // a preview and returns before any consent prompt or `Connection::open`.
    // It was fenced with its siblings anyway (`db.rs` as a whole was outside
    // #241's file scope), never fixed here, only moved out of `Excluded`.
    (&["db", "import"], Verdict::Honours),
    (&["db", "stats"], Verdict::ReadOnly),
    // export/import/init/quick-archive: top-level `Commands::*` arms in
    // `main.rs` that call `cli::operations::*`/`cmd_init` directly, never a
    // `cli::<mod>::run(...)` dispatch — fixed by issue #247, which extended
    // its scope past #241's "the argument lists of the `cli::<mod>::run(...)`
    // dispatch calls" fence to cover these four call sites too.
    (&["export"], Verdict::Honours),
    (&["import"], Verdict::Honours),
    (&["init"], Verdict::Honours),
    (
        &["key", "escrow-kit"],
        Verdict::Refuses {
            probe_args: &["--out", "/tmp/tapectl-dry-run-contract-nonexistent"],
        },
    ),
    (&["key", "export"], Verdict::ReadOnly),
    (
        &["key", "generate"],
        Verdict::Refuses {
            probe_args: &["--escrow"],
        },
    ),
    // key import: the non-`--escrow` path only reads a public key already
    // handed to it and is honoured (proven in dry_run_global.rs);
    // `--escrow` additionally refuses `--dry-run` outright (singular,
    // irreversible identity registration, same reasoning as `key generate
    // --escrow`) — a stricter behaviour within the same leaf, not
    // re-asserted by the completeness table, which needs one verdict per
    // leaf.
    (&["key", "import"], Verdict::Honours),
    (&["key", "list"], Verdict::ReadOnly),
    (
        &["key", "rotate"],
        Verdict::Refuses {
            probe_args: &["--tenant", "nonexistent-tenant"],
        },
    ),
    // location: fixed by issue #230, predating this branch.
    (&["location", "add"], Verdict::Honours),
    (&["location", "info"], Verdict::ReadOnly),
    (&["location", "list"], Verdict::ReadOnly),
    (&["location", "rename"], Verdict::Honours),
    // Issue #247: the worst of the ten — `operations::quick_archive` took no
    // `dry_run` parameter at all and ended in `volume write`, so
    // `--dry-run` staged a whole unit and sealed a real cartridge. Refused
    // the same way `volume write` is: before `write_device`, so the
    // refusal can never itself open the drive.
    (
        &["quick-archive"],
        Verdict::Refuses {
            probe_args: &[
                "/tmp/tapectl-dry-run-contract-nonexistent",
                "--tenant",
                "nonexistent-tenant",
                "--volume",
                "NOSUCHVOL",
            ],
        },
    ),
    (&["report", "age"], Verdict::ReadOnly),
    (&["report", "capacity"], Verdict::ReadOnly),
    (&["report", "compaction-candidates"], Verdict::ReadOnly),
    (&["report", "copies"], Verdict::ReadOnly),
    (&["report", "dirty"], Verdict::ReadOnly),
    (&["report", "events"], Verdict::ReadOnly),
    (&["report", "fire-risk"], Verdict::ReadOnly),
    (&["report", "health"], Verdict::ReadOnly),
    (&["report", "pending"], Verdict::ReadOnly),
    (&["report", "summary"], Verdict::ReadOnly),
    (&["report", "supersedable"], Verdict::ReadOnly),
    (&["report", "tape-only"], Verdict::ReadOnly),
    (&["report", "verify-status"], Verdict::ReadOnly),
    (
        &["restore", "file"],
        Verdict::Refuses {
            probe_args: &[
                "--file",
                "f",
                "--unit",
                "u",
                "--from",
                "NOSUCHVOL",
                "--to",
                "/tmp/tapectl-dry-run-contract-nonexistent",
            ],
        },
    ),
    (
        &["restore", "raw-volume"],
        Verdict::Refuses {
            probe_args: &["--to", "/tmp/tapectl-dry-run-contract-nonexistent"],
        },
    ),
    // restore unit: its OWN `dry_run` field shares clap's arg id with the
    // global flag (both are named `dry_run`), so the global value unifies
    // into it with no extra plumbing — the same mechanism
    // `dry_run_global.rs`'s `collection sync` characterisation test
    // proves. `restore_unit`'s dry branch returns before the store is even
    // opened. Proven with its own characterisation test in
    // `tests/dry_run_global.rs`.
    (&["restore", "unit"], Verdict::Honours),
    (
        &["snapshot", "create"],
        Verdict::Refuses {
            probe_args: &["nonexistent-unit"],
        },
    ),
    (&["snapshot", "delete"], Verdict::Honours),
    (&["snapshot", "diff"], Verdict::ReadOnly),
    (&["snapshot", "list"], Verdict::ReadOnly),
    (
        &["snapshot", "mark-reclaimable"],
        Verdict::Refuses {
            probe_args: &["nonexistent-unit", "--version", "1"],
        },
    ),
    (&["snapshot", "purge"], Verdict::Honours),
    (
        &["stage", "create"],
        Verdict::Refuses {
            probe_args: &["nonexistent-unit"],
        },
    ),
    (&["stage", "info"], Verdict::ReadOnly),
    (&["stage", "list"], Verdict::ReadOnly),
    (&["staging", "clean"], Verdict::Refuses { probe_args: &[] }),
    (&["staging", "status"], Verdict::ReadOnly),
    (&["tenant", "add"], Verdict::Honours),
    (&["tenant", "delete"], Verdict::Honours),
    (&["tenant", "info"], Verdict::ReadOnly),
    (&["tenant", "list"], Verdict::ReadOnly),
    (&["tenant", "reassign"], Verdict::Honours),
    (&["unit", "check-integrity"], Verdict::ReadOnly),
    (&["unit", "discover"], Verdict::Refuses { probe_args: &[] }),
    (
        &["unit", "init"],
        Verdict::Refuses {
            probe_args: &[
                "/tmp/tapectl-dry-run-contract-nonexistent",
                "--tenant",
                "nonexistent-tenant",
            ],
        },
    ),
    (
        &["unit", "init-bulk"],
        Verdict::Refuses {
            probe_args: &[
                "/tmp/tapectl-dry-run-contract-nonexistent",
                "--tenant",
                "nonexistent-tenant",
            ],
        },
    ),
    (&["unit", "list"], Verdict::ReadOnly),
    (
        &["unit", "mark-tape-only"],
        Verdict::Refuses {
            probe_args: &["nonexistent-unit"],
        },
    ),
    (&["unit", "rename"], Verdict::Honours),
    (&["unit", "status"], Verdict::ReadOnly),
    (&["unit", "tag"], Verdict::Honours),
    (
        &["volume", "abort"],
        Verdict::Refuses {
            probe_args: &["NOSUCHVOL"],
        },
    ),
    (
        &["volume", "compact"],
        Verdict::Refuses {
            probe_args: &["NOSUCHVOL"],
        },
    ),
    (
        &["volume", "compact-finish"],
        Verdict::Refuses {
            probe_args: &["NOSUCHVOL"],
        },
    ),
    (
        &["volume", "compact-read"],
        Verdict::Refuses {
            probe_args: &["NOSUCHVOL"],
        },
    ),
    (
        &["volume", "compact-write"],
        Verdict::Refuses {
            probe_args: &["--destination", "NOSUCHVOL"],
        },
    ),
    (&["volume", "deposit", "add"], Verdict::Honours),
    (&["volume", "deposit", "list"], Verdict::ReadOnly),
    (&["volume", "deposit", "remove"], Verdict::Honours),
    (&["volume", "identify"], Verdict::ReadOnly),
    (&["volume", "info"], Verdict::ReadOnly),
    (
        &["volume", "init"],
        Verdict::Refuses {
            probe_args: &["NOSUCHVOL"],
        },
    ),
    (&["volume", "list"], Verdict::ReadOnly),
    (&["volume", "move"], Verdict::Honours),
    (&["volume", "plan"], Verdict::ReadOnly),
    (
        &["volume", "read-slices"],
        Verdict::Refuses {
            probe_args: &["--from", "NOSUCHVOL", "--unit", "u"],
        },
    ),
    (
        &["volume", "resume"],
        Verdict::Refuses {
            probe_args: &["NOSUCHVOL"],
        },
    ),
    (&["volume", "retire"], Verdict::Honours),
    (
        &["volume", "verify"],
        Verdict::Refuses {
            probe_args: &["NOSUCHVOL"],
        },
    ),
    (
        &["volume", "write"],
        Verdict::Refuses {
            probe_args: &["NOSUCHVOL"],
        },
    ),
];

/// Every leaf (a node with no subcommands of its own) in the real clap
/// tree, as `Vec<String>` path segments.
fn clap_leaves() -> Vec<Vec<String>> {
    fn walk(cmd: &clap::Command, path: Vec<String>, out: &mut Vec<Vec<String>>) {
        let subs: Vec<_> = cmd.get_subcommands().collect();
        if subs.is_empty() {
            if !path.is_empty() {
                out.push(path);
            }
            return;
        }
        for sub in subs {
            let mut p = path.clone();
            p.push(sub.get_name().to_string());
            walk(sub, p, out);
        }
    }
    let root = tapectl::cli::Cli::command();
    let mut out = Vec::new();
    walk(&root, Vec::new(), &mut out);
    out
}

/// The regression guard: a new subcommand with no `TABLE` entry fails the
/// build, and a `TABLE` entry for a removed subcommand does too — the
/// table cannot silently drift from the real clap tree in either
/// direction.
#[test]
fn every_clap_leaf_has_exactly_one_table_verdict() {
    let leaves: HashSet<Vec<String>> = clap_leaves().into_iter().collect();

    let mut table_paths: HashSet<Vec<String>> = HashSet::new();
    for (path, _) in TABLE {
        let p: Vec<String> = path.iter().map(|s| s.to_string()).collect();
        assert!(
            table_paths.insert(p.clone()),
            "TABLE lists `{}` more than once",
            p.join(" ")
        );
    }

    let undecided: Vec<String> = leaves
        .difference(&table_paths)
        .map(|p| p.join(" "))
        .collect();
    assert!(
        undecided.is_empty(),
        "issue #241's contract has no verdict for: {undecided:?} — every clap leaf must \
         appear in TABLE (dry_run_contract.rs) as ReadOnly, Refuses or Honours"
    );

    let stale: Vec<String> = table_paths
        .difference(&leaves)
        .map(|p| p.join(" "))
        .collect();
    assert!(
        stale.is_empty(),
        "TABLE names leaves that no longer exist in the clap tree: {stale:?} — remove the \
         stale entry"
    );
}

fn run_tapectl(home: &std::path::Path, args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_tapectl"))
        .args(args)
        .env("HOME", home)
        .env_remove("XDG_CONFIG_HOME")
        .output()
        .expect("failed to spawn tapectl binary")
}

fn ok(home: &std::path::Path, args: &[&str]) -> std::process::Output {
    let out = run_tapectl(home, args);
    assert!(
        out.status.success(),
        "`tapectl {}` failed ({:?})\nstdout={}\nstderr={}",
        args.join(" "),
        out.status.code(),
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );
    out
}

/// Every [`Verdict::Refuses`] entry, run for real against the compiled
/// binary: `--dry-run` must be refused, before anything else, on a home
/// that has nothing this command could act on. This is what would have
/// caught the pre-fix defect — before issue #241 landed, every one of
/// these commands either mutated silently under `--dry-run` (the arms this
/// branch touches) or was never wired to see the flag at all (the
/// dispatchers this branch touches), and this loop's `assert!` fails on
/// each until the refusal is the first statement of its arm.
#[test]
fn every_refuses_verdict_refuses_before_doing_anything() {
    let home = TempDir::new().unwrap();
    ok(home.path(), &["init"]);

    let mut failures = Vec::new();
    for (path, verdict) in TABLE {
        let Verdict::Refuses { probe_args } = verdict else {
            continue;
        };
        let mut args: Vec<&str> = path.to_vec();
        args.extend_from_slice(probe_args);
        args.push("--dry-run");

        let out = run_tapectl(home.path(), &args);

        if out.status.success() {
            failures.push(format!(
                "`tapectl {} --dry-run` exited 0 — it must refuse, not silently proceed \
                 (stdout={})",
                args.join(" "),
                String::from_utf8_lossy(&out.stdout),
            ));
            continue;
        }

        let stderr = String::from_utf8_lossy(&out.stderr);
        if !stderr.contains("--dry-run") {
            failures.push(format!(
                "`tapectl {}` refused, but its message doesn't name --dry-run — an operator \
                 must be told WHY, not just that it failed: stderr={stderr}",
                args.join(" "),
            ));
        }
    }

    assert!(
        failures.is_empty(),
        "issue #241 class test caught {} command(s) not refusing --dry-run:\n{}",
        failures.len(),
        failures.join("\n")
    );
}
