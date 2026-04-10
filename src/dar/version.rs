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
