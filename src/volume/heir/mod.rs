//! The heir's script, File 2 of every Layout 3 tape.
//!
//! `RESTORE.sh` is a shell file in the tree and goes on tape byte for byte
//! (`docs/design/volume-format-v3.md` §7.4): nothing is substituted into it.
//! What differs between tapes (the label, the plan, the units) is read from
//! the tape when the script runs. Its awk programs are in the same file.

/// The bytes of File 2, the same on every tape.
pub const RESTORE_SH: &str = include_str!("RESTORE.sh");
