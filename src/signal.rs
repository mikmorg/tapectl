//! Stopping a long run on a signal (issue #404).
//!
//! SIGINT (Ctrl-C), SIGTERM (a shutdown, `systemctl stop`) and SIGHUP (an
//! ssh session dropping outside tmux) all mean the same thing: stop at the
//! next point the work can be resumed or re-run from. Before #404 only
//! SIGINT was handled — `ctrlc` without its `termination` feature left
//! SIGTERM and SIGHUP at their default action, which ended a multi-hour
//! write or stage with no bookkeeping at all.
//!
//! The first signal sets a flag that every long loop checks between items
//! (a slice, a file, a tape entry) and says so on stderr. A second signal
//! means "now": the process exits at once with [`EXIT_SIGNALLED_TWICE`],
//! and the startup sweep (`db::recover_orphaned_sessions`) recovers
//! whatever was in flight, as it does after any crash. The handler runs on
//! `ctrlc`'s own thread, not in signal context, so printing and exiting
//! there are safe.
//!
//! The flag is consumed where an interruption is handled
//! ([`clear_interrupted`]): a stop is reported by the phase it reaches, and
//! does not linger to fire again in a later one.

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use crate::error::{Result, TapectlError};

/// The handler's state. A type rather than two bare statics so its policy
/// can be tested on a private instance: the process-wide one is read by
/// every long loop, and lib tests run in parallel.
struct Signals {
    interrupted: AtomicBool,
    count: AtomicUsize,
}

impl Signals {
    const fn new() -> Self {
        Signals {
            interrupted: AtomicBool::new(false),
            count: AtomicUsize::new(0),
        }
    }

    fn on_signal(&self) -> Action {
        self.interrupted.store(true, Ordering::SeqCst);
        if self.count.fetch_add(1, Ordering::SeqCst) == 0 {
            Action::StopAtSafePoint
        } else {
            Action::ExitNow
        }
    }

    fn is_interrupted(&self) -> bool {
        self.interrupted.load(Ordering::SeqCst)
    }

    fn clear(&self) {
        self.interrupted.store(false, Ordering::SeqCst);
        self.count.store(0, Ordering::SeqCst);
    }
}

static STATE: Signals = Signals::new();

/// The exit status of a run stopped by a second signal: 128 + SIGINT, the
/// shell's convention for "ended by Ctrl-C".
pub const EXIT_SIGNALLED_TWICE: i32 = 130;

/// What the handler does with one signal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    /// The first: set the flag; the work stops at its next safe point.
    StopAtSafePoint,
    /// A second, before the first was handled: exit now.
    ExitNow,
}

/// Install the handler for SIGINT, SIGTERM and SIGHUP. Call once at
/// startup.
pub fn install_handler() {
    ctrlc::set_handler(|| match STATE.on_signal() {
        Action::StopAtSafePoint => eprintln!(
            "tapectl: signal received — stopping at the next point the work can be resumed \
             from. Send it again (Ctrl-C) to stop immediately."
        ),
        Action::ExitNow => {
            eprintln!("tapectl: second signal — stopping now");
            std::process::exit(EXIT_SIGNALLED_TWICE);
        }
    })
    .expect("failed to install the SIGINT/SIGTERM/SIGHUP handler");
}

#[cfg(test)]
thread_local! {
    /// A lib test's stand-in for a signal, on its own thread only — the
    /// process-wide flag would stop every other test running beside it.
    static THREAD_STOP: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Make [`is_interrupted`] answer `on` for the calling thread (lib tests
/// only).
#[cfg(test)]
pub(crate) fn interrupt_this_thread(on: bool) {
    THREAD_STOP.with(|c| c.set(on));
}

/// Whether a signal has asked the run to stop.
pub fn is_interrupted() -> bool {
    #[cfg(test)]
    if THREAD_STOP.with(|c| c.get()) {
        return true;
    }
    STATE.is_interrupted()
}

/// Consume the stop request once it has been handled, so it cannot fire
/// again in a later phase, and so the next signal is a first one again.
pub fn clear_interrupted() {
    #[cfg(test)]
    interrupt_this_thread(false);
    STATE.clear();
}

/// `Err(TapectlError::Interrupted)` naming where the run stopped and how
/// to go on, when a signal has asked it to stop — the check every long loop
/// makes between items. `stopped` is built only when it is needed.
pub fn check(stopped: impl FnOnce() -> String) -> Result<()> {
    if is_interrupted() {
        return Err(TapectlError::Interrupted(stopped()));
    }
    Ok(())
}

/// The warning for a long operation started over ssh outside tmux or
/// screen, or `None` (issue #404). A dropped connection sends SIGHUP: the
/// run now stops cleanly at its next safe point, but it still stops, hours
/// short. `env` reads one environment variable (`std::env::var`, in
/// production), so the decision is testable without touching the process
/// environment.
pub fn unprotected_ssh_warning(
    operation: &str,
    env: impl Fn(&str) -> Option<String>,
) -> Option<String> {
    let set = |k: &str| env(k).is_some_and(|v| !v.is_empty());
    let over_ssh = set("SSH_CONNECTION") || set("SSH_TTY");
    let multiplexed = set("TMUX") || set("STY");
    (over_ssh && !multiplexed).then(|| {
        format!(
            "warning: `{operation}` is running over ssh outside tmux or screen — if the \
             connection drops, the hangup stops it at its next safe point. Start long runs \
             inside `tmux new` (or `screen`)."
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_long_run_over_bare_ssh_is_warned_about_and_one_in_tmux_is_not() {
        let env = |pairs: &'static [(&'static str, &'static str)]| {
            move |k: &str| {
                pairs
                    .iter()
                    .find(|(name, _)| *name == k)
                    .map(|(_, v)| v.to_string())
            }
        };
        let warned = unprotected_ssh_warning(
            "volume write L6-0001",
            env(&[("SSH_CONNECTION", "a b c d")]),
        );
        assert!(warned.unwrap().contains("`volume write L6-0001`"));
        assert!(unprotected_ssh_warning("x", env(&[("SSH_TTY", "/dev/pts/1")])).is_some());
        assert!(unprotected_ssh_warning(
            "x",
            env(&[
                ("SSH_CONNECTION", "a"),
                ("TMUX", "/tmp/tmux-1000/default,1,0")
            ])
        )
        .is_none());
        assert!(
            unprotected_ssh_warning("x", env(&[("SSH_TTY", "/dev/pts/1"), ("STY", "1.pts")]))
                .is_none()
        );
        assert!(
            unprotected_ssh_warning("x", env(&[])).is_none(),
            "local: no warning"
        );
    }

    /// The handler's policy, on a private instance.
    #[test]
    fn the_first_signal_asks_to_stop_and_the_second_exits() {
        let state = Signals::new();
        assert!(!state.is_interrupted());
        assert_eq!(state.on_signal(), Action::StopAtSafePoint);
        assert!(state.is_interrupted());
        assert_eq!(
            state.on_signal(),
            Action::ExitNow,
            "a second signal is 'now'"
        );

        // Consumed: the next signal is a first one again.
        state.clear();
        assert!(!state.is_interrupted());
        assert_eq!(state.on_signal(), Action::StopAtSafePoint);
    }

    #[test]
    fn check_names_where_the_run_stopped() {
        assert!(check(|| unreachable!("not interrupted")).is_ok());
        interrupt_this_thread(true);
        let err = check(|| "stopped before slice 3 of 5".to_string()).unwrap_err();
        interrupt_this_thread(false);
        assert!(
            matches!(&err, TapectlError::Interrupted(at) if at == "stopped before slice 3 of 5"),
            "{err:?}"
        );
        assert!(check(|| unreachable!("cleared")).is_ok());
    }
}
