//! Advisory scan for keys present in `config.toml` that no struct declares.
//!
//! Distinct from [`crate::policy::decorative`], which surfaces keys that ARE
//! parsed but have no reader yet. These are never parsed at all. Since issue
//! #171 an unknown key is a hard load error, so `config check` already reports
//! it as a problem; what this scan adds is the REMEDIATION for keys whose
//! history is known.
//!
//! The keys with a history are the two issue #348 renamed:
//! `min_copies_for_tape_only` → `min_copies` and `min_locations_for_tape_only`
//! → `min_locations` (the list lives in `config::RENAMED_DEFAULTS_KEYS`, shared
//! with the load-time refusal). Before that rename, issue #129 had the inverse
//! problem — `[defaults] min_copies` read like the copy requirement and was
//! not one; it now is.
//!
//! Advisory only, like every other scan here: it never changes `config check`'s
//! exit code (ADR-0004's spirit — reporting, not blocking).
//!
//! This reads the raw TOML text rather than a `Config`, because by the time
//! serde is done the evidence is gone.

/// One key found in the file that no struct field claims.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnknownKeyHit {
    /// Dotted path as the user would write it, e.g.
    /// `defaults.min_copies_for_tape_only`.
    pub key: String,
}

/// Fields `DefaultsConfig` actually declares.
///
/// Kept as a literal list, and pinned by a test that fails if `DefaultsConfig`
/// gains or loses a field — otherwise a new setting would be reported as
/// "unknown" the day it is added, which is worse than not scanning at all.
/// (That pin is exactly what caught `hash`'s removal, issue #172: it had no
/// reader at all and is gone from `DefaultsConfig`, not merely unlisted here.)
const DEFAULTS_FIELDS: &[&str] = &[
    "slice_size",
    "compression",
    "checksum_mode",
    "encrypt",
    "preserve_xattrs",
    "preserve_acls",
    "preserve_fsa",
    "dirty_on_metadata_change",
    "global_excludes",
    "large_file_warn_threshold",
    "min_copies",
    "min_locations",
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
    // NOT "silently ignored" — that was true before #171 made unknown keys a
    // hard load error. Saying it now contradicts the refusal printed inches
    // above this line in the same `config check` output, and would send an
    // operator looking for a subtle behaviour change when the config simply
    // will not load. What this adds over the generic refusal is the
    // REMEDIATION, which is the only reason the scan survives #173.
    let key = hit.key.trim_start_matches("defaults.");
    if let Some((old, new)) = crate::config::RENAMED_DEFAULTS_KEYS
        .iter()
        .find(|(old, _)| *old == key)
    {
        format!(
            "[defaults].{old} was renamed to [defaults].{new} (#348) — the config will not \
             load while the old name is present. Rename the key; its meaning and value \
             are unchanged."
        )
    } else {
        format!(
            "[defaults].{} is not a recognised setting — the config will not load while \
             it is present; check the spelling, or remove it.",
            hit.key.trim_start_matches("defaults."),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Issue #348: the two renamed keys are reported with the new name to
    /// use, and the new names themselves — `min_copies` was #129's
    /// not-a-setting — are real fields now and report nothing.
    #[test]
    fn the_old_tape_only_names_are_reported_with_the_new_key_named() {
        let hits = scan(
            "[defaults]\nmin_copies_for_tape_only = 2\nmin_locations_for_tape_only = 2\n\
             slice_size = \"10G\"\n",
        );
        assert_eq!(
            hits,
            vec![
                UnknownKeyHit {
                    key: "defaults.min_copies_for_tape_only".into()
                },
                UnknownKeyHit {
                    key: "defaults.min_locations_for_tape_only".into()
                },
            ]
        );
        let msg = describe(&hits[0]);
        assert!(msg.contains("renamed to [defaults].min_copies "), "{msg}");
        let msg = describe(&hits[1]);
        assert!(
            msg.contains("renamed to [defaults].min_locations "),
            "{msg}"
        );

        assert!(scan("[defaults]\nmin_copies = 2\nmin_locations = 2\n").is_empty());
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
