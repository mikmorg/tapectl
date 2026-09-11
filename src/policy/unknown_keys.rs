//! Advisory scan for keys present in `config.toml` that no struct declares.
//!
//! Distinct from [`crate::policy::decorative`], which surfaces keys that ARE
//! parsed but have no reader yet. These are never parsed at all: serde ignores
//! unknown fields, so a plausible-looking setting can sit in a config for years
//! doing exactly nothing, with `config check` reporting `config: valid`.
//!
//! That is not hypothetical. `[defaults] min_copies` reads like the general
//! copy requirement and is not one — the requirement comes from
//! `min_copies_for_tape_only`, and `min_copies` belongs to an
//! `[[archive_sets]]` entry (issue #129, found when a lifecycle run set it to
//! relax a copy policy and audit went on reporting the old threshold).
//!
//! Advisory only, like every other scan here: it never changes `config check`'s
//! exit code (ADR-0004's spirit — reporting, not blocking).
//!
//! This reads the raw TOML text rather than a `Config`, because by the time
//! serde is done the evidence is gone.

/// One key found in the file that no struct field claims.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnknownKeyHit {
    /// Dotted path as the user would write it, e.g. `defaults.min_copies`.
    pub key: String,
}

/// Fields `DefaultsConfig` actually declares.
///
/// Kept as a literal list, and pinned by a test that fails if `DefaultsConfig`
/// gains or loses a field — otherwise a new setting would be reported as
/// "unknown" the day it is added, which is worse than not scanning at all.
const DEFAULTS_FIELDS: &[&str] = &[
    "slice_size",
    "compression",
    "hash",
    "checksum_mode",
    "encrypt",
    "preserve_xattrs",
    "preserve_acls",
    "preserve_fsa",
    "dirty_on_metadata_change",
    "global_excludes",
    "large_file_warn_threshold",
    "min_copies_for_tape_only",
    "min_locations_for_tape_only",
    "warehouse_copies",
];

/// Every unknown key in the `[defaults]` table of `toml_text`.
///
/// Scoped to `[defaults]` on purpose. A whole-file unknown-key scan would need
/// the full schema of every table and would false-positive on the commented
/// example `init` writes and on any table this list does not track; `[defaults]`
/// is where the confusable knobs live.
pub fn scan(toml_text: &str) -> Vec<UnknownKeyHit> {
    let mut out = Vec::new();
    let mut in_defaults = false;

    for line in toml_text.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with('#') || trimmed.is_empty() {
            continue;
        }
        if trimmed.starts_with('[') {
            // Any new table header ends [defaults]. Matching the header exactly
            // keeps `[defaults.something]` from being treated as the same table.
            in_defaults = trimmed == "[defaults]";
            continue;
        }
        if !in_defaults {
            continue;
        }
        let Some((key, _)) = trimmed.split_once('=') else {
            continue;
        };
        let key = key.trim().trim_matches('"');
        if !key.is_empty() && !DEFAULTS_FIELDS.contains(&key) {
            out.push(UnknownKeyHit {
                key: format!("defaults.{key}"),
            });
        }
    }
    out
}

/// The advisory line for one hit. Pure, so `config check`'s text and `--json`
/// arms cannot drift apart.
pub fn describe(hit: &UnknownKeyHit) -> String {
    if hit.key == "defaults.min_copies" {
        "warning: [defaults].min_copies is not a setting and is silently ignored (#129). \
         The copy requirement comes from [defaults].min_copies_for_tape_only; a per-set \
         override goes on an [[archive_sets]] entry as min_copies. Setting it here has \
         no effect on what `audit` reports."
            .to_string()
    } else {
        format!(
            "warning: [defaults].{} is not a recognised setting and is silently ignored — \
             check the spelling, or remove it.",
            hit.key.trim_start_matches("defaults."),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The #129 case: a setting that reads like the general copy requirement,
    /// is not one, and produced no diagnostic at all.
    #[test]
    fn min_copies_in_defaults_is_reported_with_the_real_knob_named() {
        let hits = scan("[defaults]\nmin_copies = 2\nslice_size = \"10G\"\n");
        assert_eq!(
            hits,
            vec![UnknownKeyHit {
                key: "defaults.min_copies".into()
            }]
        );
        let msg = describe(&hits[0]);
        assert!(msg.contains("min_copies_for_tape_only"), "{msg}");
        assert!(msg.contains("archive_sets"), "{msg}");
    }

    #[test]
    fn every_real_defaults_field_is_accepted() {
        let body: String = DEFAULTS_FIELDS
            .iter()
            .map(|f| format!("{f} = 0\n"))
            .collect();
        assert!(scan(&format!("[defaults]\n{body}")).is_empty());
    }

    /// `min_copies` is legitimate on an archive set, and the commented example
    /// `init` writes must not be mistaken for live configuration.
    #[test]
    fn keys_outside_defaults_and_commented_lines_are_ignored() {
        let text = "\
[[archive_sets]]
name = \"critical\"
min_copies = 3

[defaults]
slice_size = \"10G\"
# min_copies = 9

[packing]
min_copies = 5
";
        assert!(scan(text).is_empty(), "{:?}", scan(text));
    }

    /// A serialized real config must be clean, or `config check` would warn
    /// about the file it just wrote. This is the guard that keeps
    /// DEFAULTS_FIELDS honest when DefaultsConfig gains a field.
    #[test]
    fn a_freshly_serialized_default_config_reports_nothing() {
        let text = toml::to_string_pretty(&crate::config::Config::default()).unwrap();
        assert!(
            scan(&text).is_empty(),
            "DEFAULTS_FIELDS is out of sync with DefaultsConfig: {:?}",
            scan(&text)
        );
    }
}
