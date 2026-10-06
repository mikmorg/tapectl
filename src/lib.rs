// Issue #404's follow-up: `println!`, `print!`, `eprintln!` and `eprint!`
// throughout this crate are these shadows of `std`'s, which drop a write to a
// pipe whose reader has gone instead of panicking (`tapectl … | head`, a
// `| tee` killed by Ctrl-C). Defined before every `mod` so their textual
// scope covers the whole crate; see `output` for why and what still panics.
// Under `cfg(test)` `std`'s own stay in force so the harness captures output.
#[cfg(not(test))]
macro_rules! println {
    () => { $crate::output::stdout(format_args!(""), true) };
    ($($arg:tt)*) => { $crate::output::stdout(format_args!($($arg)*), true) };
}
#[cfg(not(test))]
macro_rules! print {
    ($($arg:tt)*) => { $crate::output::stdout(format_args!($($arg)*), false) };
}
#[cfg(not(test))]
macro_rules! eprintln {
    () => { $crate::output::stderr(format_args!(""), true) };
    ($($arg:tt)*) => { $crate::output::stderr(format_args!($($arg)*), true) };
}
#[cfg(not(test))]
#[allow(unused_macros)]
macro_rules! eprint {
    ($($arg:tt)*) => { $crate::output::stderr(format_args!($($arg)*), false) };
}

pub mod build_info;
pub mod cli;
pub mod config;
pub mod crypto;
pub mod db;
pub mod error;
pub mod host_check;
pub mod signal;
pub mod startup;
pub mod tenant;
pub mod unit;

pub mod collection;
pub mod dar;
pub mod staging;
pub mod tape;
pub mod volume;

pub mod media;
pub mod naming;
pub mod output;
pub mod pipeline;
pub mod policy;
pub mod progress;
pub mod store;
pub mod util;
