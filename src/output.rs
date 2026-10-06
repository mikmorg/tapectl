//! Printing that survives a closed pipe (issue #404's follow-up).
//!
//! `std`'s `println!`/`eprintln!` panic when the write fails, and a write to
//! a pipe whose reader has gone fails with `EPIPE` (Rust ignores `SIGPIPE`,
//! so the write returns the error instead of killing the process). Two ways
//! that reached an operator:
//!
//! - `tapectl catalog ls | head` — `head` exits after ten lines and the next
//!   `println!` panicked with exit 101.
//! - `first-run.sh` runs a write as `run … | tee`. A Ctrl-C kills `tee`, the
//!   write stops cleanly at its next safe point (or even finishes), and its
//!   final line panicked — exit 101, which the script read as "did not seal"
//!   when the catalog had recorded the seal.
//!
//! So every `println!`, `print!`, `eprintln!` and `eprint!` in this crate and
//! in the binary goes through [`stdout`] / [`stderr`] here instead: a write
//! the reader is no longer there for is dropped, and the command goes on to
//! finish and exit with its real status. Any OTHER write error still panics,
//! exactly as `std` does — a full disk under `> file` is not something to
//! hide.
//!
//! The macros are `macro_rules!` shadows of `std`'s, defined in `lib.rs`
//! before every module (and at the top of `main.rs`), so the 600-odd call
//! sites need no change and a new one gets the same treatment without anyone
//! remembering to. Under `cfg(test)` they defer to `std`'s own, so the test
//! harness still captures a lib test's output; the behaviour itself is
//! pinned through the real binary (`tests/cli_smoke.rs`,
//! `a_closed_stdout_pipe_does_not_panic` and its stderr twin).

use std::fmt;
use std::io::{self, Write};

/// `print!`/`println!`: `args` (and a newline when `newline`) to stdout, a
/// closed pipe ignored.
pub fn stdout(args: fmt::Arguments<'_>, newline: bool) {
    let mut out = io::stdout().lock();
    finish("stdout", write(&mut out, args, newline));
}

/// `eprint!`/`eprintln!`: the same, to stderr.
pub fn stderr(args: fmt::Arguments<'_>, newline: bool) {
    let mut err = io::stderr().lock();
    finish("stderr", write(&mut err, args, newline));
}

fn write(w: &mut impl Write, args: fmt::Arguments<'_>, newline: bool) -> io::Result<()> {
    w.write_fmt(args)?;
    if newline {
        w.write_all(b"\n")?;
    }
    Ok(())
}

fn finish(stream: &str, r: io::Result<()>) {
    match r {
        Ok(()) => {}
        Err(e) if is_closed_pipe(&e) => {}
        // `std`'s own message, so nothing else about a real failure changes.
        Err(e) => panic!("failed printing to {stream}: {e}"),
    }
}

/// Whether `e` says the reader of a pipe has gone.
pub fn is_closed_pipe(e: &io::Error) -> bool {
    e.kind() == io::ErrorKind::BrokenPipe
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A writer whose reader has gone.
    struct ClosedPipe;
    impl Write for ClosedPipe {
        fn write(&mut self, _: &[u8]) -> io::Result<usize> {
            Err(io::Error::from(io::ErrorKind::BrokenPipe))
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    /// A writer on a full disk.
    struct Full;
    impl Write for Full {
        fn write(&mut self, _: &[u8]) -> io::Result<usize> {
            Err(io::Error::from_raw_os_error(28))
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn a_closed_pipe_is_dropped() {
        finish(
            "stdout",
            write(&mut ClosedPipe, format_args!("x {}", 1), true),
        );
    }

    #[test]
    #[should_panic(expected = "failed printing to stdout")]
    fn any_other_write_error_still_panics() {
        finish("stdout", write(&mut Full, format_args!("x"), true));
    }

    #[test]
    fn the_newline_follows_the_text() {
        let mut buf = Vec::new();
        write(&mut buf, format_args!("a{}", "b"), true).unwrap();
        write(&mut buf, format_args!("c"), false).unwrap();
        assert_eq!(buf, b"ab\nc");
    }
}
