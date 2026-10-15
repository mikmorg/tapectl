//! `scripts/first-run.sh` step 13 routes `volume init`'s refusals by phrase
//! (issues #446, #469): one phrase sends a SEALED tape to `cartridge erase`,
//! three send the refusals `--force` can override to the overwrite offer, and
//! everything else stops with the refusal's own remedy. The phrases are
//! operator text in `src/volume/write.rs`, so a reworded refusal would silently
//! send a tape down the wrong branch: this test pins both ends.
//!
//! It reads source text, normalising Rust's `\`-newline string continuations,
//! because the refusals are built inside a private decision function.

use std::path::Path;

fn read(rel: &str) -> String {
    std::fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join(rel))
        .unwrap_or_else(|e| panic!("read {rel}: {e}"))
}

/// The refusal text as the binary prints it: a `\` at the end of a source
/// line inside a string literal swallows the newline and the next line's
/// leading whitespace.
fn joined_string_continuations(src: &str) -> String {
    let mut out = String::with_capacity(src.len());
    let mut lines = src.lines().peekable();
    while let Some(line) = lines.next() {
        if let Some(stripped) = line.strip_suffix('\\') {
            out.push_str(stripped);
            if let Some(next) = lines.peek_mut() {
                *next = next.trim_start();
            }
        } else {
            out.push_str(line);
            out.push('\n');
        }
    }
    out
}

/// Each refusal `volume init`/`volume write` can return at the tape contact:
/// the string literals in `write.rs` that begin "refusing to write volume",
/// cut at their closing quote. Test code that merely mentions a phrase (an
/// assertion, a fixture) is not a refusal and is not counted.
fn refusal_literals(joined: &str) -> Vec<String> {
    let opener = "\"refusing to write volume";
    let mut out = Vec::new();
    let mut rest = joined;
    while let Some(at) = rest.find(opener) {
        let body = &rest[at + 1..];
        let mut end = body.len();
        let bytes = body.as_bytes();
        for i in 0..bytes.len() {
            if bytes[i] == b'"' && (i == 0 || bytes[i - 1] != b'\\') {
                end = i;
                break;
            }
        }
        out.push(body[..end].to_string());
        rest = &body[end..];
    }
    out
}

/// (phrase, the step-13 branch it selects)
const ROUTED: &[(&str, &str)] = &[
    ("already carries a SEALED volume", "cartridge erase"),
    ("identifies a DIFFERENT volume", "the --force offer"),
    ("File 0 is EMPTY", "the --force offer"),
    ("is not provably blank", "the --force offer"),
];

/// The lines of step 13 that route on `volume init`'s output: a `grep` over
/// `$INIT_OUT`. Prose (an `explain` paragraph) may use the same words and
/// must not satisfy the test.
fn routing_lines(script: &str) -> String {
    script
        .lines()
        .filter(|l| l.contains("grep") && l.contains("\"$INIT_OUT\""))
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn every_phrase_step_13_routes_on_is_in_a_volume_init_refusal() {
    let script = routing_lines(&read("scripts/first-run.sh"));
    assert!(
        script.lines().count() >= 4,
        "found {} routing line(s) in step 13; expected the SEALED, --force and device_sg ones",
        script.lines().count()
    );
    let refusals = refusal_literals(&joined_string_continuations(&read("src/volume/write.rs")));
    assert!(
        refusals.len() >= 5,
        "found {} refusal literal(s) in write.rs; expected the SEALED, mismatch, EMPTY and \
         two unreadable-File-0 ones at least, so the extraction is not looking where it should",
        refusals.len()
    );
    for (phrase, branch) in ROUTED {
        assert!(
            script.contains(phrase),
            "first-run.sh no longer routes on {phrase:?} ({branch}): update ROUTED with the script"
        );
        assert!(
            refusals.iter().any(|r| r.contains(phrase)),
            "no refusal in src/volume/write.rs says {phrase:?}, so first-run.sh step 13 can never \
             take {branch} for it: reword the refusal back, or change the script and this test together"
        );
    }
}

/// The phrase that sends a tape to `cartridge erase` appears in exactly one
/// refusal, the SEALED one; and the unreadable-File-0 refusals, which cite
/// ADR-0003 too, are not routed there (#446).
#[test]
fn only_the_sealed_refusal_routes_to_cartridge_erase() {
    let refusals = refusal_literals(&joined_string_continuations(&read("src/volume/write.rs")));
    assert_eq!(
        refusals
            .iter()
            .filter(|r| r.contains("already carries a SEALED volume"))
            .count(),
        1,
        "the cartridge-erase phrase must name one refusal only"
    );
    let script = routing_lines(&read("scripts/first-run.sh"));
    assert!(
        !script.contains("ADR-0003"),
        "step 13 routes on \"ADR-0003\" again, which the unreadable-File-0 refusals also cite (#446)"
    );
}

/// The overwrite phrases must not appear in the refusals `--force` cannot
/// override, or the script would offer a flag that changes nothing.
#[test]
fn no_force_phrase_appears_in_a_refusal_force_cannot_override() {
    let refusals = refusal_literals(&joined_string_continuations(&read("src/volume/write.rs")));
    let unforceable: Vec<&String> = refusals
        .iter()
        .filter(|r| r.contains("--force cannot override"))
        .collect();
    // Positive control: the SEALED refusal and the unreadable-File-0-on-a-
    // live-cartridge refusal both say it, so an empty search proves nothing.
    assert!(
        unforceable.len() >= 2,
        "found {} refusal(s) saying \"--force cannot override\"; expected at least the SEALED \
         and the live-cartridge ones, so this search is not looking where it should",
        unforceable.len()
    );
    for line in unforceable {
        for (phrase, branch) in ROUTED.iter().filter(|(_, b)| *b == "the --force offer") {
            assert!(
                !line.contains(phrase),
                "a refusal --force cannot override says {phrase:?}, which routes to {branch}: {line}"
            );
        }
    }
}
