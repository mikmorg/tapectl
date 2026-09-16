//! Lenient config diagnosis for `config check` (issue #173).
//!
//! `Config::load` is strict by design (ADR-0012 / #171) and stays that
//! way — every section carries `#[serde(deny_unknown_fields)]`, and the
//! first problem found aborts the load. That is exactly right for every
//! OTHER command: a broken config should never be silently tolerated. But
//! it means `config check`'s whole job — diagnosing a broken config — was
//! unreachable on exactly the files that need it (`cli::config`'s `Check`
//! arm called `Config::load(...)?`, so a strict-load failure short-circuited
//! before the command body ever ran), and `main.rs`'s own common dispatch
//! path loads the config strictly before ANY subcommand runs at all, so the
//! command could not even be reached that way either.
//!
//! This module is the fix's parse half: read the file leniently and report
//! **every** problem found, not just the first.
//!
//! **The central constraint (read the issue twice): this must never become
//! a second, hand-maintained definition of what a valid config is.** So it
//! borrows validity from the exact same places `Config::load` already gets
//! it from, and never invents a rule of its own:
//!
//! - "Is this key known?" is decided by attempting REAL deserialization
//!   into the REAL [`Config`] type — the same `#[serde(deny_unknown_fields)]`
//!   structs `Config::load` uses. Never a hand-listed set of field names: a
//!   field added to any `Config` substruct tomorrow is automatically
//!   accepted here with no change to this file, because the check IS
//!   `Config`'s own `Deserialize` impl, not a copy of it.
//! - "Is this value legal?" is decided by [`Config::semantic_problems`] —
//!   the exact validators (`validate_compression`, `validate_checksum_mode`,
//!   `validate_log_level`, `validate_log_format`,
//!   `media::Generation::parse`, `staging::parse_size_to_bytes`)
//!   `Config::load` itself calls, just collected instead of
//!   short-circuited at the first hit.
//!
//! [`Config::load`]'s own error is always `problems[0]` when the config is
//! invalid — it carries context this module's blunter "unknown key: x"
//! breakdown does not (exact line/column, and the friendly stale-field
//! remediation text from `stale_lto_fields_message` for a renamed/removed
//! key). The remaining entries are the exhaustive breakdown this module
//! adds. The two can read like near-duplicates for a config with exactly
//! one problem — that is intentional (see [`check`]'s doc), not a bug to
//! dedupe away.

use std::path::Path;

use crate::config::Config;

/// Everything `config check` learned about a config it could not — or
/// could barely — strictly load.
#[derive(Debug, Clone, Default)]
pub struct LenientReport {
    /// Mirrors `Config::load(path).is_ok()` exactly — this module never
    /// second-guesses that verdict in either direction. `problems.is_empty()
    /// == valid` always holds (see the module doc and this file's tests).
    pub valid: bool,
    /// Every problem found, in the order found. `problems[0]` is
    /// `Config::load`'s own error text whenever `valid` is `false`;
    /// `problems` is empty whenever `valid` is `true`.
    pub problems: Vec<String>,
    /// A best-effort `Config`, built by stripping every unknown key this
    /// module found. Used to run the existing depth checks (staging, tape
    /// devices, dar, decorative keys, …) even on a config with real
    /// problems elsewhere. `None` only when nothing usable could be built —
    /// the file is not valid TOML at all, or a non-unknown-field structural
    /// error (a wrong-typed value, a missing required field) stopped the
    /// stripper before it reached a real `Config`. `Some` whenever `valid`
    /// is `true`.
    pub best_effort: Option<Config>,
}

/// Bound on how many unknown keys this will strip-and-retry before giving
/// up — a real config has, at most, a few dozen keys total; this is only a
/// backstop against a pathological input looping forever.
const MAX_STRIP_ITERATIONS: usize = 200;

/// Diagnose a config file at `path`. `content` is passed in (rather than
/// re-read from `path`) only for [`Config::load`]'s benefit — it re-reads
/// `path` itself, and passing the same bytes this function was given keeps
/// the two reads honest if the caller already has the content in hand for
/// other reasons (`config show`, the raw-text scans). A TOCTOU gap between
/// `content` and what `Config::load` reads is possible in principle (the
/// file changing between the two reads) but not a new risk: `cli::config`'s
/// other scans already re-read the file independently for the same reason.
pub fn check(content: &str, path: &Path) -> LenientReport {
    match Config::load(path) {
        Ok(cfg) => LenientReport {
            valid: true,
            problems: Vec::new(),
            best_effort: Some(cfg),
        },
        Err(load_err) => {
            let mut problems = vec![load_err.to_string()];
            let best_effort = strip_and_collect(content, path, &mut problems);
            LenientReport {
                valid: false,
                problems,
                best_effort,
            }
        }
    }
}

/// Repeatedly parse `text` as a [`Config`]; every time deserialization fails
/// on an unknown field, locate it, record it, delete it from `text`, and
/// retry. Once it parses structurally, layer `Config::semantic_problems`
/// (the same value validators `Config::load` uses) on top to catch anything
/// left — a bad `compression` value, an unparseable `slice_size`, and so on.
///
/// Returns the structurally-valid `Config` once one is reached (even if
/// `semantic_problems` found more to report against it), or `None` if
/// stripping cannot make progress — a syntax error, a missing required
/// field, or a wrong-typed value. Nothing is pushed to `problems` in that
/// case: `problems[0]` (`Config::load`'s own error) already said so, and
/// this is exactly the "a config that is not valid TOML reports that, once"
/// case (issue #173's acceptance criterion) — this function contributing a
/// second, near-identical line for the very same root cause would be noise,
/// not a new problem.
fn strip_and_collect(content: &str, path: &Path, problems: &mut Vec<String>) -> Option<Config> {
    let mut text = content.to_string();
    for _ in 0..MAX_STRIP_ITERATIONS {
        match toml::from_str::<Config>(&text) {
            Ok(cfg) => {
                problems.extend(cfg.semantic_problems(path));
                return Some(cfg);
            }
            Err(e) => {
                if !is_unknown_field_error(&e) {
                    return None;
                }
                match strip_offending_key(&mut text, &e) {
                    Some(dotted) => problems.push(format!("unknown key: {dotted}")),
                    None => return None,
                }
            }
        }
    }
    None
}

/// Whether `e` is `#[serde(deny_unknown_fields)]` rejecting a field no
/// struct declares — the ONLY error class this module knows how to repair
/// and keep going past. Checked on `e.message()` (just the message, no
/// "TOML parse error at line N" banner) so this cannot be fooled by a
/// coincidentally-matching value elsewhere in a longer message.
fn is_unknown_field_error(e: &toml::de::Error) -> bool {
    e.message().starts_with("unknown field ")
}

/// Pull the field name out of a toml-rs unknown-field message, e.g.
/// `` unknown field `foobar`, expected one of `a`, `b` `` -> `"foobar"`.
/// toml-rs's tail wording varies with the number of known fields ("expected
/// one of `a`, `b`" vs "expected `a` or `b`" for exactly two), so this only
/// ever depends on the fixed head, never the tail.
fn extract_field_name(message: &str) -> Option<&str> {
    let after = message.strip_prefix("unknown field `")?;
    let end = after.find('`')?;
    Some(&after[..end])
}

/// Locate and delete the single line (or, for a table/array-of-tables
/// header itself being unknown, the whole block) that `err` blames, and
/// return the dotted key path removed, e.g. `"defaults.foobar"`,
/// `"archive_sets[1].bogus"`, or `"packing"` for a whole unknown table.
///
/// `None` when `err` carries no span (toml-rs does not promise one for
/// every error) or no extractable field name — the caller gives up
/// cleanly rather than risk mangling `text` on a guess.
fn strip_offending_key(text: &mut String, err: &toml::de::Error) -> Option<String> {
    let span = err.span()?;
    let field_name = extract_field_name(err.message())?;

    let line_start = text[..span.start].rfind('\n').map_or(0, |i| i + 1);
    let line_end = text[span.end..]
        .find('\n')
        .map_or(text.len(), |i| span.end + i + 1);
    let trimmed = text[line_start..line_end].trim();

    if trimmed.starts_with('[') {
        // The unknown field IS the header itself (`[nonsense]` or
        // `[[archive_sets_typo]]`) — a TOML table header always names its
        // own full dotted path from the root, so the header text alone is
        // the answer; no need to track an enclosing table for this case.
        // Delete the WHOLE block (header through the line before the next
        // header, or EOF) so none of its now-orphaned members get
        // reinterpreted as belonging to whatever table came before it.
        let dotted = trimmed
            .trim_start_matches('[')
            .trim_end_matches(']')
            .trim()
            .to_string();
        let block_end = find_next_header_or_eof(text, line_end);
        text.replace_range(line_start..block_end, "");
        Some(dotted)
    } else {
        // A plain `key = value` line inside a table that IS known — delete
        // just this line and name it against whatever table encloses it.
        let dotted = match enclosing_table_path(text, line_start) {
            Some(table) => format!("{table}.{field_name}"),
            None => field_name.to_string(),
        };
        text.replace_range(line_start..line_end, "");
        Some(dotted)
    }
}

/// Scan forward from byte offset `from` for the start of the next table
/// header line (`[...]` or `[[...]]`, allowing leading whitespace), or
/// `text.len()` if there is none — the end of the block a header at `from`
/// (exclusive) owns.
fn find_next_header_or_eof(text: &str, from: usize) -> usize {
    let mut pos = from;
    loop {
        if pos >= text.len() {
            return text.len();
        }
        let next_nl = text[pos..].find('\n').map_or(text.len(), |i| pos + i + 1);
        if text[pos..next_nl].trim_start().starts_with('[') {
            return pos;
        }
        if next_nl >= text.len() {
            return text.len();
        }
        pos = next_nl;
    }
}

/// Walk every header line in `text[..before]` and return the dotted path of
/// whichever table encloses byte offset `before` — `None` at the top level
/// (no header seen yet). Array-of-tables headers (`[[name]]`) are indexed
/// by occurrence count, e.g. `"archive_sets[1]"` for the second
/// `[[archive_sets]]` block, matching how an operator would point at that
/// entry.
fn enclosing_table_path(text: &str, before: usize) -> Option<String> {
    let mut path: Option<String> = None;
    let mut counts: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    for line in text[..before].lines() {
        let trimmed = line.trim();
        if trimmed.starts_with("[[") {
            let name = trimmed
                .trim_start_matches('[')
                .trim_end_matches(']')
                .trim()
                .to_string();
            let idx = *counts.get(&name).unwrap_or(&0);
            *counts.entry(name.clone()).or_insert(0) += 1;
            path = Some(format!("{name}[{idx}]"));
        } else if trimmed.starts_with('[') {
            let name = trimmed
                .trim_start_matches('[')
                .trim_end_matches(']')
                .trim()
                .to_string();
            path = Some(name);
        }
    }
    path
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn write_config(text: &str) -> (TempDir, std::path::PathBuf) {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("config.toml");
        std::fs::write(&path, text).unwrap();
        (tmp, path)
    }

    // ---- The drift invariant every fixture below must satisfy ----

    fn assert_matches_config_load(content: &str, path: &Path) {
        let report = check(content, path);
        assert_eq!(
            report.problems.is_empty(),
            report.valid,
            "problems.is_empty() must equal valid: {report:?}"
        );
        assert_eq!(
            report.valid,
            Config::load(path).is_ok(),
            "this module's verdict must match Config::load's exactly"
        );
    }

    #[test]
    fn a_clean_config_is_valid_with_no_problems() {
        let (_tmp, path) = write_config("");
        let report = check("", &path);
        assert!(report.valid);
        assert!(report.problems.is_empty());
        assert!(report.best_effort.is_some());
        assert_matches_config_load("", &path);
    }

    #[test]
    fn not_valid_toml_reports_it_exactly_once() {
        let content = "not even toml {{{";
        let (_tmp, path) = write_config(content);
        let report = check(content, &path);
        assert!(!report.valid);
        assert_eq!(
            report.problems.len(),
            1,
            "a syntactically broken file must report its one problem once: {:?}",
            report.problems
        );
        assert!(report.best_effort.is_none());
        assert_matches_config_load(content, &path);
    }

    #[test]
    fn two_unknown_keys_in_the_same_table_are_both_reported() {
        let content = "[defaults]\nfoo = 1\nbar = 2\n";
        let (_tmp, path) = write_config(content);
        let report = check(content, &path);
        assert!(!report.valid);
        let unknown: Vec<&str> = report
            .problems
            .iter()
            .filter(|p| p.starts_with("unknown key: "))
            .map(String::as_str)
            .collect();
        assert!(
            unknown.contains(&"unknown key: defaults.foo"),
            "{:?}",
            report.problems
        );
        assert!(
            unknown.contains(&"unknown key: defaults.bar"),
            "{:?}",
            report.problems
        );
        assert_eq!(unknown.len(), 2, "{:?}", report.problems);
        assert_matches_config_load(content, &path);
    }

    #[test]
    fn an_unknown_whole_table_is_reported_once_and_does_not_spuriously_flag_its_members() {
        let content = "[nonsense]\nx = 1\n\n[defaults]\nslice_size = \"10G\"\n";
        let (_tmp, path) = write_config(content);
        let report = check(content, &path);
        assert!(!report.valid);
        let unknown: Vec<&str> = report
            .problems
            .iter()
            .filter(|p| p.starts_with("unknown key: "))
            .map(String::as_str)
            .collect();
        assert_eq!(
            unknown,
            vec!["unknown key: nonsense"],
            "the whole unknown block must be removed together, not leave 'x' behind \
             to be reported again on its own: {:?}",
            report.problems
        );
        let cfg = report
            .best_effort
            .expect("defaults.slice_size is valid, so a Config must still come out");
        assert_eq!(cfg.defaults.slice_size, "10G");
        assert_matches_config_load(content, &path);
    }

    #[test]
    fn two_array_of_tables_entries_each_get_a_distinct_index() {
        let content = "[[archive_sets]]\nname = \"a\"\nbogus = 1\n\
                        [[archive_sets]]\nname = \"b\"\nbogus2 = 2\n";
        let (_tmp, path) = write_config(content);
        let report = check(content, &path);
        assert!(!report.valid);
        let unknown: Vec<&str> = report
            .problems
            .iter()
            .filter(|p| p.starts_with("unknown key: "))
            .map(String::as_str)
            .collect();
        assert!(
            unknown.contains(&"unknown key: archive_sets[0].bogus"),
            "{:?}",
            report.problems
        );
        assert!(
            unknown.contains(&"unknown key: archive_sets[1].bogus2"),
            "{:?}",
            report.problems
        );
        assert_matches_config_load(content, &path);
    }

    #[test]
    fn a_bad_closed_set_value_alone_is_reported_via_semantic_problems() {
        let content = "[defaults]\ncompression = \"banana\"\n";
        let (_tmp, path) = write_config(content);
        let report = check(content, &path);
        assert!(!report.valid);
        assert!(
            report
                .problems
                .iter()
                .any(|p| p.contains("defaults.compression") && p.contains("banana")),
            "{:?}",
            report.problems
        );
        // No unknown-key noise for a config whose only problem is a bad value.
        assert!(
            !report.problems.iter().any(|p| p.starts_with("unknown key:")),
            "{:?}",
            report.problems
        );
        assert!(report.best_effort.is_some());
        assert_matches_config_load(content, &path);
    }

    #[test]
    fn an_unknown_key_and_a_bad_closed_set_value_are_both_reported_in_one_run() {
        let content = "[defaults]\ncompression = \"banana\"\nfoobar = 1\n";
        let (_tmp, path) = write_config(content);
        let report = check(content, &path);
        assert!(!report.valid);
        assert!(
            report
                .problems
                .iter()
                .any(|p| p.starts_with("unknown key: defaults.foobar")),
            "{:?}",
            report.problems
        );
        assert!(
            report
                .problems
                .iter()
                .any(|p| p.contains("defaults.compression") && p.contains("banana")),
            "{:?}",
            report.problems
        );
        assert_matches_config_load(content, &path);
    }

    #[test]
    fn a_stale_renamed_key_still_gets_config_loads_friendly_remediation_as_the_first_line() {
        // config::stale_lto_fields_message's whole point: a serde "unknown
        // field" message alone would not say WHERE media_type/nominal_capacity
        // went. That text must survive as problems[0].
        let content = "[[backends.lto]]\nname = \"lto1\"\ndevice_tape = \"/dev/nst0\"\n\
                        device_sg = \"/dev/sg0\"\nmedia_type = \"LTO-6\"\n\
                        nominal_capacity = \"2.5TB\"\n";
        let (_tmp, path) = write_config(content);
        let report = check(content, &path);
        assert!(!report.valid);
        assert!(
            report[0].contains("ADR-0010"),
            "expected the friendly remediation as problems[0]: {:?}",
            report.problems
        );
        assert_matches_config_load(content, &path);
    }

    // Indexing helper for the assertion above, since `LenientReport` has no
    // Index impl of its own and adding one for one test isn't worth it.
    impl std::ops::Index<usize> for LenientReport {
        type Output = str;
        fn index(&self, i: usize) -> &str {
            &self.problems[i]
        }
    }

    #[test]
    fn a_missing_required_field_stops_cleanly_with_no_panic_and_no_duplicate_line() {
        // `ArchiveSetConfig::name` has no default — a missing-field error,
        // not an unknown-field one. The stripper cannot repair this, and
        // must not push a second, near-identical line on top of
        // Config::load's own.
        let content = "[[archive_sets]]\nmin_copies = 2\n";
        let (_tmp, path) = write_config(content);
        let report = check(content, &path);
        assert!(!report.valid);
        assert_eq!(
            report.problems.len(),
            1,
            "an unrepairable structural error must not gain a duplicate line: {:?}",
            report.problems
        );
        assert!(report.best_effort.is_none());
        assert_matches_config_load(content, &path);
    }

    #[test]
    fn every_default_config_field_round_trips_clean() {
        // The drift guard: serialize Config::default(), add nothing, expect
        // zero problems. If a future field's own default value fails its
        // own validator, this is where that would show up.
        let content = toml::to_string_pretty(&Config::default()).unwrap();
        let (_tmp, path) = write_config(&content);
        assert_matches_config_load(&content, &path);
        let report = check(&content, &path);
        assert!(report.valid, "{:?}", report.problems);
    }
}
