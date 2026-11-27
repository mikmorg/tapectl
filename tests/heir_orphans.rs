//! RESTORE.sh's way out where nothing reaps orphans (the u26 leg, 2026-11-24).
//!
//! Under `docker run` without `--init`, PID 1 is the test binary, which never
//! reaps the processes orphaned to it: a tool of the read ahead that a kill
//! ended stays a zombie, and a zombie still answers `kill -0` for its process
//! group. On a host, init reaps it and that is never seen. This binary makes
//! itself a child subreaper (`PR_SET_CHILD_SUBREAPER`) and never reaps, so
//! the same holds here, in plain `cargo test` and CI. It is a binary of its
//! own, with one test, because a subreaper is a property of the whole process:
//! in `tests/heir_restore.rs` it would reach every other test running beside
//! it.

#[path = "common/heir/mod.rs"]
mod heir;

use std::time::{Duration, Instant};

use heir::{Heir, Shape, Tape, Who, UNIT_B};

/// The tape position of slice `n` (from 1) of `unit`.
fn slice_pos(t: &Tape, unit: &str, n: usize) -> usize {
    let mut base = 0;
    for u in &t.units {
        if u.name == unit {
            break;
        }
        base += u.slices;
    }
    t.slice_positions()[base + n - 1]
}

/// `/proc/<pid>/stat`'s state and parent, split after the last `) ` (the
/// name may hold spaces and parentheses).
fn state_and_parent(pid: u32) -> Option<(String, u32)> {
    let s = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let rest = &s[s.rfind(") ")? + 2..];
    let mut f = rest.split_whitespace();
    let state = f.next()?.to_string();
    let ppid = f.next()?.parse().ok()?;
    Some((state, ppid))
}

fn pids() -> Vec<u32> {
    std::fs::read_dir("/proc")
        .unwrap()
        .filter_map(|e| e.ok()?.file_name().to_str()?.parse().ok())
        .collect()
}

/// The tools of a read of this run still running: each process whose
/// environment names its directory tape (a zombie has no environment left to
/// read) and whose name is one a read runs.
fn live_of_run(h: &Heir) -> Vec<(u32, String)> {
    let mark = format!("FAKE_TAPE={}", h.root.path().join("state").display());
    pids()
        .into_iter()
        .filter_map(|pid| {
            let env = std::fs::read(format!("/proc/{pid}/environ")).ok()?;
            if !env.split(|b| *b == 0).any(|v| v == mark.as_bytes()) {
                return None;
            }
            let comm = std::fs::read_to_string(format!("/proc/{pid}/comm")).ok()?;
            let comm = comm.trim().to_string();
            ["dd", "sleep", "head", "tee", "sha256sum", "cat", "mt"]
                .contains(&comm.as_str())
                .then_some((pid, comm))
        })
        .collect()
}

/// TERM while a slice is read ahead, where nothing reaps the read's tools once
/// the kill has orphaned them: the run still ends at once (its wait for the
/// read's process group ends when only zombies are left, which hold no
/// device), with no process of the run left running and no work directory.
#[test]
fn a_kill_while_a_slice_is_read_ahead_ends_at_once_where_nothing_reaps_orphans() {
    // SAFETY: prctl with PR_SET_CHILD_SUBREAPER takes plain integers and
    // touches no memory of this process.
    let rc = unsafe { nix::libc::prctl(nix::libc::PR_SET_CHILD_SUBREAPER, 1, 0, 0, 0) };
    assert_eq!(rc, 0, "PR_SET_CHILD_SUBREAPER");
    let me = std::process::id();
    let t = Tape::build(&Shape::two());
    let h = Heir::of(&t);
    let p = slice_pos(&t, UNIT_B, 2);
    let dest = h.dest("out");
    let kb = h.key(Who::TenantB).to_string_lossy().into_owned();
    let slow = format!("{p}:30");
    let args = ["--restore", "--key", &kb, "--to", dest.to_str().unwrap()];
    let mut r = h.spawn(&args, &[("FAKE_DD_SLOW", slow.as_str())]);
    let until = Instant::now() + Duration::from_secs(60);
    while !h.root.path().join("state/dd.slow").exists() {
        assert!(Instant::now() < until, "{}", r.text());
        std::thread::sleep(Duration::from_millis(20));
    }
    // Positive control: slice 2 is being read ahead, slowly.
    assert!(
        live_of_run(&h).iter().any(|(_, c)| c == "sleep"),
        "{:?}",
        live_of_run(&h)
    );
    r.signal("TERM");
    let started = Instant::now();
    let code = loop {
        if let Some(code) = r.try_wait() {
            break code;
        }
        if started.elapsed() > Duration::from_secs(15) {
            let _ = r.child.kill();
            panic!(
                "the run did not end within 15 s of TERM (waiting on zombies?): {}",
                r.text()
            );
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    let text = r.text();
    assert_eq!(code, 143, "{text}");
    // Positive control: the kill orphaned the read's tools to this process,
    // which has not reaped them: the condition docker's PID 1 makes.
    let zombies: Vec<u32> = pids()
        .into_iter()
        .filter(|pid| matches!(state_and_parent(*pid), Some((s, pp)) if s == "Z" && pp == me))
        .collect();
    assert!(
        !zombies.is_empty(),
        "no orphan was left to this process: {text}"
    );
    assert_eq!(live_of_run(&h), vec![], "{text}");
    let left: Vec<String> = std::fs::read_dir(&dest)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|n| n.starts_with(".tapectl-work"))
        .collect();
    assert!(left.is_empty(), "{left:?}: {text}");
}

/// #611 C-4 (ADR-0012 item 313): TERM while the final check runs in pieces
/// (each piece's `sha256sum -c` a stand-in that sleeps first), where nothing
/// reaps orphans: the run ends at once, as a TERM during the one-process check
/// does (143, no work directory), and no `sha256sum` or its `sleep` is left
/// running. The pieces are one job in a process group of its own, which the
/// way out stops as it stops the read ahead (`ahead_kill`).
#[test]
fn a_kill_while_the_check_runs_in_pieces_leaves_no_sha256sum_behind() {
    // SAFETY: as above.
    let rc = unsafe { nix::libc::prctl(nix::libc::PR_SET_CHILD_SUBREAPER, 1, 0, 0, 0) };
    assert_eq!(rc, 0, "PR_SET_CHILD_SUBREAPER");
    let t = Tape::build(&Shape::two());
    let h = Heir::of(&t);
    let real = heir::host_tool("sha256sum").expect("sha256sum");
    h.put_tool(
        "sha256sum",
        &format!(
            "#!/bin/bash\n\
             if [ \"${{1:-}}\" = -c ]; then : >\"$FAKE_TAPE/check.slow\"; sleep 30; fi\n\
             exec \"{}\" \"$@\"\n",
            real.display()
        ),
    );
    let dest = h.dest("out");
    let kb = h.key(Who::TenantB).to_string_lossy().into_owned();
    let args = ["--restore", "--key", &kb, "--to", dest.to_str().unwrap()];
    h.put_tool("nproc", "#!/bin/sh\necho 3\n");
    let mut r = h.spawn(&args, &[]);
    let until = Instant::now() + Duration::from_secs(60);
    while !h.root.path().join("state/check.slow").exists() {
        assert!(Instant::now() < until, "{}", r.text());
        std::thread::sleep(Duration::from_millis(20));
    }
    std::thread::sleep(Duration::from_millis(300));
    // Positive control: the pieces are running, each asleep.
    let live = live_of_run(&h);
    assert!(
        live.iter().filter(|(_, c)| c == "sleep").count() >= 2,
        "{live:?}"
    );
    r.signal("TERM");
    let started = Instant::now();
    let code = loop {
        if let Some(code) = r.try_wait() {
            break code;
        }
        if started.elapsed() > Duration::from_secs(15) {
            let _ = r.child.kill();
            panic!("the run did not end within 15 s of TERM: {}", r.text());
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    let text = r.text();
    assert_eq!(code, 143, "{text}");
    assert_eq!(live_of_run(&h), vec![], "{text}");
    let left: Vec<String> = std::fs::read_dir(&dest)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|n| n.starts_with(".tapectl-work"))
        .collect();
    assert!(left.is_empty(), "{left:?}: {text}");
}

/// Every process of this run still alive (a zombie has no environment left
/// to read), whatever its name.
fn any_of_run(h: &Heir) -> Vec<(u32, String)> {
    let mark = format!("FAKE_TAPE={}", h.root.path().join("state").display());
    pids()
        .into_iter()
        .filter_map(|pid| {
            let env = std::fs::read(format!("/proc/{pid}/environ")).ok()?;
            if !env.split(|b| *b == 0).any(|v| v == mark.as_bytes()) {
                return None;
            }
            let comm = std::fs::read_to_string(format!("/proc/{pid}/comm")).ok()?;
            Some((pid, comm.trim().to_string()))
        })
        .collect()
}

/// A `chmod` that takes 3 s over the work directory, the cleanup's first
/// step on it, and leaves `state/in-cleanup` when it begins: a second signal
/// sent then lands in the cleanup.
fn slow_cleanup(h: &Heir) {
    let real = heir::host_tool("chmod").expect("chmod");
    h.put_tool(
        "chmod",
        &format!(
            "#!/bin/bash\n\
             case \"$1 $2\" in \"-R u+rwx\") : >\"$FAKE_TAPE/in-cleanup\"; sleep 3 ;; esac\n\
             exec \"{}\" \"$@\"\n",
            real.display()
        ),
    );
}

/// The run's end (at most 30 s), its status and what it said.
fn end_of(r: &mut heir::Running) -> (i32, String) {
    let started = Instant::now();
    loop {
        if let Some(code) = r.try_wait() {
            return (code, r.text());
        }
        if started.elapsed() > Duration::from_secs(30) {
            let _ = r.child.kill();
            panic!("the run did not end within 30 s: {}", r.text());
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn wait_for_file(h: &Heir, r: &heir::Running, name: &str) {
    let until = Instant::now() + Duration::from_secs(60);
    while !h.root.path().join("state").join(name).exists() {
        assert!(Instant::now() < until, "no {name}: {}", r.text());
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn work_dirs_in(dest: &std::path::Path) -> Vec<String> {
    std::fs::read_dir(dest)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|n| n.starts_with(".tapectl-work"))
        .collect()
}

/// #611 G2 (ADR-0012 item 313): a second signal during the cleanup does not
/// stop it. Ctrl-C (INT) while a slice is read, then INT again while the
/// cleanup removes the work directory (a `chmod` stand-in that takes 3 s),
/// sent to the script and, separately, to its whole process group as a
/// terminal sends it: the run ends with the first signal's status, the work
/// directory (with the decrypted envelopes) is gone, and no process of the
/// run is left. The cleanup ignores the three signals as its first act, and
/// what it runs inherits that.
#[test]
fn a_second_ctrl_c_during_the_cleanup_does_not_stop_it() {
    let t = Tape::build(&Shape::two());
    let p = slice_pos(&t, UNIT_B, 2);
    let slow = format!("{p}:30");
    for group in [false, true] {
        let h = Heir::of(&t);
        slow_cleanup(&h);
        let dest = h.dest("out");
        let kb = h.key(Who::TenantB).to_string_lossy().into_owned();
        let args = ["--restore", "--key", &kb, "--to", dest.to_str().unwrap()];
        let envs = [("FAKE_DD_SLOW", slow.as_str())];
        let mut r = if group {
            h.spawn_group(&args, &envs)
        } else {
            h.spawn(&args, &envs)
        };
        let send = |r: &heir::Running, sig: &str| {
            if group {
                r.signal_group(sig)
            } else {
                r.signal(sig)
            }
        };
        wait_for_file(&h, &r, "dd.slow");
        // Positive control: the work directory holds the decrypted envelopes.
        let w = work_dirs_in(&dest);
        assert_eq!(w.len(), 1, "{w:?}");
        assert!(
            std::fs::read_dir(dest.join(&w[0])).unwrap().any(|e| e
                .unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with("env.")),
            "{}",
            r.text()
        );
        send(&r, "INT");
        wait_for_file(&h, &r, "in-cleanup");
        send(&r, "INT");
        let (code, text) = end_of(&mut r);
        let what = format!("group {group}: {text}");
        assert_eq!(code, 130, "{what}");
        assert_eq!(work_dirs_in(&dest), Vec::<String>::new(), "{what}");
        assert_eq!(any_of_run(&h), vec![], "{what}");
    }
}

/// #611 G2: a TERM that lands as the cleanup after RESTORE COMPLETE runs
/// (here, while its `chmod` stand-in takes 3 s), to the script or to its
/// group, does not stop it: the run ends 0, as it said, with no work
/// directory and no process of the run left.
#[test]
fn a_term_during_the_cleanup_after_restore_complete_does_not_stop_it() {
    let t = Tape::build(&Shape::two());
    for group in [false, true] {
        let h = Heir::of(&t);
        slow_cleanup(&h);
        let dest = h.dest("out");
        let kb = h.key(Who::TenantB).to_string_lossy().into_owned();
        let args = ["--restore", "--key", &kb, "--to", dest.to_str().unwrap()];
        let mut r = if group {
            h.spawn_group(&args, &[])
        } else {
            h.spawn(&args, &[])
        };
        wait_for_file(&h, &r, "in-cleanup");
        assert!(r.text().contains("RESTORE COMPLETE"), "{}", r.text());
        if group {
            r.signal_group("TERM");
        } else {
            r.signal("TERM");
        }
        let (code, text) = end_of(&mut r);
        let what = format!("group {group}: {text}");
        assert_eq!(code, 0, "{what}");
        assert_eq!(work_dirs_in(&dest), Vec::<String>::new(), "{what}");
        assert_eq!(any_of_run(&h), vec![], "{what}");
    }
}
