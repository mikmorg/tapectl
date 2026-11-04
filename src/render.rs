//! The render kit of ADR-0012 item 212: the helpers every command's output
//! shares, so a table, a time, a plural, a verdict word and a JSON error
//! look the same wherever they appear.
//!
//! Helpers only. Pure cores (`*_with`) take the terminal facts as arguments;
//! the public wrappers read them from the process.

use std::ffi::OsStr;
use std::io::IsTerminal;

use chrono::{DateTime, NaiveDateTime, SecondsFormat, Utc};
use serde_json::{json, Value};
use tabled::builder::Builder;
use tabled::settings::peaker::PriorityMax;
use tabled::settings::{Padding, Style, Width};

/// `Copy Floor` from `copy floor` or `copy_floor`: every word capitalised.
pub fn title_case(header: &str) -> String {
    header
        .split(|c: char| c == ' ' || c == '_')
        .filter(|w| !w.is_empty())
        .map(|w| {
            let mut cs = w.chars();
            match cs.next() {
                Some(f) => f.to_uppercase().chain(cs.flat_map(char::to_lowercase)).collect(),
                None => String::new(),
            }
        })
        .collect::<Vec<String>>()
        .join(" ")
}

/// Width of stdout's terminal, `None` when stdout is not one (or the width
/// is unknown). Asks the kernel first, then `$COLUMNS`.
pub fn terminal_width() -> Option<usize> {
    if !std::io::stdout().is_terminal() {
        return None;
    }
    nix::ioctl_read_bad!(tiocgwinsz, nix::libc::TIOCGWINSZ, nix::libc::winsize);
    let mut ws = nix::libc::winsize {
        ws_row: 0,
        ws_col: 0,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    // SAFETY: TIOCGWINSZ writes one `winsize` through the pointer we own.
    let ok = unsafe { tiocgwinsz(1, &mut ws) }.is_ok();
    if ok && ws.ws_col > 0 {
        return Some(ws.ws_col as usize);
    }
    std::env::var("COLUMNS")
        .ok()
        .and_then(|c| c.trim().parse::<usize>().ok())
        .filter(|w| *w > 0)
}

/// A borderless table with aligned columns and Title Case headers, fitted to
/// stdout's terminal width when it is one and left alone otherwise.
pub fn table(header: &[&str], rows: Vec<Vec<String>>) -> String {
    table_with(header, rows, terminal_width())
}

/// [`table`] with the width given: `Some(w)` fits the table to `w` columns
/// (the widest columns shrink first, marked with an ellipsis); `None` leaves
/// it as wide as its cells.
pub fn table_with(header: &[&str], rows: Vec<Vec<String>>, width: Option<usize>) -> String {
    let mut b = Builder::default();
    b.push_record(header.iter().map(|h| title_case(h)));
    for r in rows {
        b.push_record(r);
    }
    let mut t = b.build();
    t.with(Style::blank()).with(Padding::new(0, 2, 0, 0));
    if let Some(w) = width {
        t.with(
            Width::truncate(w)
                .suffix("…")
                .priority::<PriorityMax>(),
        );
    }
    t.to_string()
        .lines()
        .map(str::trim_end)
        .collect::<Vec<_>>()
        .join("\n")
}

fn parse_time(s: &str) -> Option<DateTime<Utc>> {
    let s = s.trim();
    if let Ok(dt) = DateTime::parse_from_rfc3339(s) {
        return Some(dt.with_timezone(&Utc));
    }
    NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M:%S%.f")
        .ok()
        .map(|n| n.and_utc())
}

/// `YYYY-MM-DD HH:MM UTC` from an SQLite `YYYY-MM-DD HH:MM:SS` (UTC) or an
/// RFC 3339 value. A value that is neither comes back unchanged.
pub fn table_time(s: &str) -> String {
    match parse_time(s) {
        Some(dt) => dt.format("%Y-%m-%d %H:%M UTC").to_string(),
        None => s.to_string(),
    }
}

/// RFC 3339 with `Z` (`2026-11-04T10:30:00Z`) from the same inputs as
/// [`table_time`]; a value that is neither comes back unchanged.
pub fn json_time(s: &str) -> String {
    match parse_time(s) {
        Some(dt) => dt.to_rfc3339_opts(SecondsFormat::Secs, true),
        None => s.to_string(),
    }
}

/// `1 copy`, `2 copies`, `0 copies`.
pub fn plural(n: i64, singular: &str, plural: &str) -> String {
    format!("{n} {}", if n == 1 { singular } else { plural })
}

/// Whether to colour stdout: it is a terminal and `NO_COLOR` is unset or
/// empty (the rule of `startup::log_ansi`, for stdout).
pub fn color_on(stdout_is_terminal: bool, no_color: Option<&OsStr>) -> bool {
    crate::startup::log_ansi(stdout_is_terminal, no_color)
}

/// The verdict words, and only these, are coloured (OQ19): `ok`, `passed`,
/// `clean` green; `warning` yellow; `FAIL`, `refused`, `error` red.
pub fn verdict_with(word: &str, color: bool) -> String {
    if !color {
        return word.to_string();
    }
    let code = match word {
        "ok" | "passed" | "clean" => "32",
        "warning" => "33",
        "FAIL" | "refused" | "error" => "31",
        _ => return word.to_string(),
    };
    format!("\x1b[{code}m{word}\x1b[0m")
}

/// [`verdict_with`] for this process's stdout and `NO_COLOR`.
pub fn verdict(word: &str) -> String {
    let on = color_on(
        std::io::stdout().is_terminal(),
        std::env::var_os("NO_COLOR").as_deref(),
    );
    verdict_with(word, on)
}

/// Pretty JSON to stdout, one trailing newline.
pub fn print_json(value: &Value) {
    match serde_json::to_string_pretty(value) {
        Ok(s) => println!("{s}"),
        Err(e) => println!("{{\"error\": \"cannot render JSON: {e}\", \"exit_code\": 70}}"),
    }
}

/// Item 212's error object: `{"error": message, "exit_code": N}`.
pub fn json_error(message: &str, exit_code: i32) -> Value {
    json!({ "error": message, "exit_code": exit_code })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rows() -> Vec<Vec<String>> {
        vec![
            vec!["alpha".into(), "1".into(), "a long description here".into()],
            vec!["b".into(), "22".into(), "short".into()],
        ]
    }

    #[test]
    fn title_case_words() {
        assert_eq!(title_case("copy floor"), "Copy Floor");
        assert_eq!(title_case("copy_floor"), "Copy Floor");
        assert_eq!(title_case("NAME"), "Name");
    }

    #[test]
    fn table_is_borderless_aligned_and_unfitted_without_width() {
        let t = table_with(&["name", "n", "description"], rows(), None);
        let lines: Vec<&str> = t.lines().collect();
        assert_eq!(lines.len(), 3);
        assert!(lines[0].starts_with("Name"));
        assert!(!t.contains('|') && !t.contains('-') && !t.contains('+'));
        assert!(t.contains("a long description here"));
        // columns align: the second column starts at the same offset
        let off = |l: &str, needle: &str| l.find(needle).unwrap();
        assert_eq!(off(lines[0], "N  "), off(lines[1], "1  "));
        assert!(lines.iter().all(|l| *l == l.trim_end()));
    }

    #[test]
    fn table_fits_the_width() {
        let t = table_with(&["name", "n", "description"], rows(), Some(24));
        assert!(t.lines().all(|l| l.chars().count() <= 24), "{t}");
        assert!(t.contains('…'));
    }

    #[test]
    fn times() {
        assert_eq!(table_time("2026-11-04 10:30:59"), "2026-11-04 10:30 UTC");
        assert_eq!(table_time("2026-11-04T12:30:00+02:00"), "2026-11-04 10:30 UTC");
        assert_eq!(table_time("2026-11-04T10:30:00Z"), "2026-11-04 10:30 UTC");
        assert_eq!(table_time("never"), "never");
        assert_eq!(json_time("2026-11-04 10:30:59"), "2026-11-04T10:30:59Z");
        assert_eq!(json_time("2026-11-04T12:30:00+02:00"), "2026-11-04T10:30:00Z");
    }

    #[test]
    fn plurals() {
        assert_eq!(plural(1, "copy", "copies"), "1 copy");
        assert_eq!(plural(2, "copy", "copies"), "2 copies");
        assert_eq!(plural(0, "copy", "copies"), "0 copies");
    }

    #[test]
    fn verdicts_colour_only_the_ruled_words() {
        assert_eq!(verdict_with("ok", true), "\x1b[32mok\x1b[0m");
        assert_eq!(verdict_with("passed", true), "\x1b[32mpassed\x1b[0m");
        assert_eq!(verdict_with("clean", true), "\x1b[32mclean\x1b[0m");
        assert_eq!(verdict_with("warning", true), "\x1b[33mwarning\x1b[0m");
        for w in ["FAIL", "refused", "error"] {
            assert_eq!(verdict_with(w, true), format!("\x1b[31m{w}\x1b[0m"));
        }
        assert_eq!(verdict_with("sealed", true), "sealed");
        assert_eq!(verdict_with("ok", false), "ok");
    }

    #[test]
    fn color_rule_matches_log_ansi() {
        assert!(color_on(true, None));
        assert!(color_on(true, Some(OsStr::new(""))));
        assert!(!color_on(true, Some(OsStr::new("1"))));
        assert!(!color_on(false, None));
    }

    #[test]
    fn json_error_shape() {
        let v = json_error("no such unit", 2);
        assert_eq!(v["error"], "no such unit");
        assert_eq!(v["exit_code"], 2);
        assert_eq!(v.as_object().unwrap().len(), 2);
    }
}
