use std::process::Command;

use crate::error::{Result, TapectlError};

/// Minimum required dar version.
pub const MIN_VERSION: (u32, u32) = (2, 6);

#[derive(Debug, Clone)]
pub struct DarVersion {
    pub major: u32,
    pub minor: u32,
    pub patch: u32,
    pub full_string: String,
}

/// Check dar version and return parsed version info.
pub fn check(dar_binary: &str) -> Result<DarVersion> {
    let output = Command::new(dar_binary)
        .arg("--version")
        .output()
        .map_err(|_| TapectlError::DarNotFound(dar_binary.to_string()))?;

    // dar prints version to stdout (or stderr depending on version)
    let text = if output.stdout.is_empty() {
        String::from_utf8_lossy(&output.stderr).to_string()
    } else {
        String::from_utf8_lossy(&output.stdout).to_string()
    };

    let version_line = text
        .lines()
        .find(|l| l.contains("dar version"))
        .unwrap_or(&text);

    let version = parse_version(version_line)?;

    if (version.major, version.minor) < MIN_VERSION {
        return Err(TapectlError::DarVersionTooOld {
            found: format!("{}.{}.{}", version.major, version.minor, version.patch),
            minimum: format!("{}.{}", MIN_VERSION.0, MIN_VERSION.1),
        });
    }

    Ok(version)
}

/// Compression algorithms the local `dar` binary was actually compiled to
/// support, as parsed from `dar -V`'s "compilation time options" block.
///
/// `none` is not tracked here — tapectl omits `-z` entirely for `none`, so
/// no compiled-in support is required for it; callers must treat `none` as
/// always supported regardless of what this struct contains.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DarCapabilities {
    supported: std::collections::HashSet<&'static str>,
}

impl DarCapabilities {
    /// Is `algorithm` usable with this dar binary? `none` is always
    /// supported (no `-z` flag is ever emitted for it). Any algorithm this
    /// parser did not positively see marked `NO` is treated as supported —
    /// see [`parse_capabilities`]'s fail-open contract.
    pub fn supports(&self, algorithm: &str) -> bool {
        algorithm == "none" || self.supported.contains(algorithm)
    }
}

/// Parse `dar -V` / `dar --version` output into the set of compression
/// algorithms the binary actually supports.
///
/// Mapping from tapectl's `-z` values to `dar -V`'s "compilation time
/// options" lines:
/// - `gzip`, `bzip2`, `lzo`, `xz`, `zstd`, `lz4` each map to their
///   same-named `<name> compression (...) : YES|NO` line.
/// - **`lzma` maps to the `xz compression (liblzma)` line.** liblzma
///   provides both algorithms, dar accepts both `-zlzma` and `-zxz`, and
///   `dar -V` has no separate `lzma compression` line — so `lzma`'s
///   supported-ness is read off the `xz` row.
/// - `none` is not represented at all: tapectl never emits `-z` for it, so
///   it requires no compiled-in support and [`DarCapabilities::supports`]
///   always accepts it.
///
/// **Fails open.** This function only records an algorithm as *unsupported*
/// when it positively parsed that algorithm's line and the line says `NO`.
/// If the whole capability block is missing, a given line is absent, or the
/// output is in some other unexpected shape, every algorithm is treated as
/// supported. A validator that hard-rejects on unrecognised `dar -V` output
/// would break every user whose dar build formats `-V` differently, turning
/// a helpful check into a broken tool — so silence or garbage input must
/// never produce a rejection.
pub fn parse_capabilities(version_output: &str) -> DarCapabilities {
    // Same-named line -> same tapectl `-z` value.
    const DIRECT: &[(&str, &str)] = &[
        ("gzip", "gzip"),
        ("bzip2", "bzip2"),
        ("lzo", "lzo"),
        ("xz", "xz"),
        ("zstd", "zstd"),
        ("lz4", "lz4"),
    ];

    let mut supported: std::collections::HashSet<&'static str> =
        DIRECT.iter().map(|(_, alg)| *alg).collect();
    // lzma rides the xz line; assume supported until proven otherwise below.
    supported.insert("lzma");

    for line in version_output.lines() {
        let trimmed = line.trim();
        let Some((label, verdict)) = trimmed.split_once(':') else {
            continue;
        };
        let label = label.trim().to_ascii_lowercase();
        let verdict = verdict.trim();

        // Only a bare "YES"/"NO" (optionally with trailing punctuation) is
        // treated as a verdict — anything else is unexpected shape, and per
        // the fail-open contract we leave that algorithm marked supported.
        let is_no = verdict.eq_ignore_ascii_case("NO");
        if !is_no {
            continue;
        }

        for (needle, alg) in DIRECT {
            if label.starts_with(&format!("{needle} compression")) {
                supported.remove(alg);
            }
        }
        if label.starts_with("xz compression") {
            supported.remove("lzma");
        }
    }

    DarCapabilities { supported }
}

/// Run `dar -V` and parse its compiled-in compression capabilities via
/// [`parse_capabilities`]. Reuses `check()`'s invocation shape — a single
/// `Command::new(dar_binary)` call — rather than a second implementation.
pub fn capabilities(dar_binary: &str) -> Result<DarCapabilities> {
    let output = Command::new(dar_binary)
        .arg("-V")
        .output()
        .map_err(|_| TapectlError::DarNotFound(dar_binary.to_string()))?;

    let text = if output.stdout.is_empty() {
        String::from_utf8_lossy(&output.stderr).to_string()
    } else {
        String::from_utf8_lossy(&output.stdout).to_string()
    };

    Ok(parse_capabilities(&text))
}

fn parse_version(line: &str) -> Result<DarVersion> {
    // "dar version 2.7.13, ..."
    let parts: Vec<&str> = line.split_whitespace().collect();
    let ver_str = parts
        .iter()
        .find(|s| s.contains('.') && s.chars().next().is_some_and(|c| c.is_ascii_digit()))
        .or_else(|| parts.get(2))
        .ok_or_else(|| TapectlError::Dar(format!("cannot parse dar version from: {line}")))?
        .trim_end_matches(',');

    let nums: Vec<u32> = ver_str.split('.').filter_map(|s| s.parse().ok()).collect();

    Ok(DarVersion {
        major: nums.first().copied().unwrap_or(0),
        minor: nums.get(1).copied().unwrap_or(0),
        patch: nums.get(2).copied().unwrap_or(0),
        full_string: ver_str.to_string(),
    })
}

#[cfg(test)]
mod capability_tests {
    use super::*;

    const EVERYTHING_YES: &str = "\
 Using libdar 6.7.1 built with compilation time options:
   gzip compression (libz)      : YES
   bzip2 compression (libbzip2) : YES
   lzo compression (liblzo2)    : YES
   xz compression (liblzma)     : YES
   zstd compression (libzstd)   : YES
   lz4 compression (liblz4)     : YES
";

    const LZO_AND_ZSTD_NO: &str = "\
 Using libdar 6.7.1 built with compilation time options:
   gzip compression (libz)      : YES
   bzip2 compression (libbzip2) : YES
   lzo compression (liblzo2)    : NO
   xz compression (liblzma)     : YES
   zstd compression (libzstd)   : NO
   lz4 compression (liblz4)     : YES
";

    #[test]
    fn line_says_no_is_unsupported() {
        let caps = parse_capabilities(LZO_AND_ZSTD_NO);
        assert!(!caps.supports("lzo"));
        assert!(!caps.supports("zstd"));
    }

    #[test]
    fn line_says_yes_is_supported() {
        let caps = parse_capabilities(EVERYTHING_YES);
        assert!(caps.supports("gzip"));
        assert!(caps.supports("bzip2"));
        assert!(caps.supports("xz"));
        assert!(caps.supports("lz4"));
    }

    #[test]
    fn line_absent_fails_open_as_supported() {
        // Capability block entirely missing (e.g. some other dar -V shape).
        let caps = parse_capabilities("dar version 2.7.13, Copyright (C) 2002-2052 ...\n");
        for alg in ["gzip", "bzip2", "lzo", "xz", "zstd", "lz4", "lzma"] {
            assert!(
                caps.supports(alg),
                "{alg} must fail open when line is absent"
            );
        }
    }

    #[test]
    fn empty_or_garbage_input_fails_open_as_supported() {
        for input in ["", "garbage garbage garbage\nnot text at all: maybe\n"] {
            let caps = parse_capabilities(input);
            for alg in ["gzip", "bzip2", "lzo", "xz", "zstd", "lz4", "lzma"] {
                assert!(
                    caps.supports(alg),
                    "{alg} must fail open on garbage/empty input"
                );
            }
        }
    }

    #[test]
    fn none_is_always_supported_regardless_of_input() {
        let caps = parse_capabilities(LZO_AND_ZSTD_NO);
        assert!(caps.supports("none"));
        let caps_empty = parse_capabilities("");
        assert!(caps_empty.supports("none"));
    }

    #[test]
    fn lzma_follows_the_xz_line() {
        let caps_yes = parse_capabilities(EVERYTHING_YES);
        assert!(caps_yes.supports("lzma"));

        const XZ_NO: &str = "\
 Using libdar 6.7.1 built with compilation time options:
   gzip compression (libz)      : YES
   bzip2 compression (libbzip2) : YES
   lzo compression (liblzo2)    : YES
   xz compression (liblzma)     : NO
   zstd compression (libzstd)   : YES
   lz4 compression (liblz4)     : YES
";
        let caps_no = parse_capabilities(XZ_NO);
        assert!(!caps_no.supports("xz"));
        assert!(
            !caps_no.supports("lzma"),
            "lzma must follow the xz line since dar has no separate lzma row"
        );
    }
}
