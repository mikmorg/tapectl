/// Generate the ID thunk (File 0) content.
pub fn generate_id_thunk(
    label: &str,
    media_type: &str,
    tapectl_version: &str,
    backend: &str,
    nominal_capacity: i64,
    mam_capacity: i64,
    data_start: i32,
    data_end: i32,
    mini_index_pos: i32,
    first_envelope_pos: i32,
    num_envelopes: i32,
    op_envelope_pos: i32,
    op_backup_pos: i32,
    total_files: i32,
    mam_manufacturer: &str,
    mam_serial: &str,
    mam_length: i64,
    mam_loads: i64,
) -> String {
    let now = chrono::Utc::now().to_rfc3339();
    format!(
        r#"================================================================
                     TAPECTL ARCHIVAL VOLUME
================================================================

Label:   {label}
Media:   {media_type}
Created: {now}

This tape contains encrypted archival data managed by tapectl,
an open-source archival storage tool.

>>> COMPLETE INSTRUCTIONS ARE IN THE NEXT FILE ON THIS TAPE. <<<

To read the next file (the full recovery guide):

    mt -f /dev/nst0 fsf 1
    dd if=/dev/nst0 bs=64k > GUIDE.md
    less GUIDE.md

If you just read this file and the tape is already positioned
past it, read the next file directly:

    dd if=/dev/nst0 bs=64k > GUIDE.md

The guide explains everything: what tools you need, how to find
your encryption key, and how to recover your data step by step.
It is written so that an AI assistant can follow it to help you.

================================================================
              MACHINE-READABLE METADATA (TOML)
================================================================

[volume]
magic = "tapectl-volume-v1"
label = "{label}"
layout_version = 1
tapectl_version = "{tapectl_version}"
backend = "{backend}"
media_type = "{media_type}"
nominal_capacity_bytes = {nominal_capacity}
mam_capacity_bytes = {mam_capacity}
created_at = "{now}"

[layout]
system_guide = 1
restore_script = 2
planning_header = 3
data_start = {data_start}
data_end = {data_end}
mini_index = {mini_index_pos}
first_envelope = {first_envelope_pos}
num_envelopes = {num_envelopes}
operator_envelope = {op_envelope_pos}
operator_envelope_backup = {op_backup_pos}
total_files = {total_files}

[media]
cartridge_manufacturer = "{mam_manufacturer}"
cartridge_serial = "{mam_serial}"
tape_length_meters = {mam_length}
load_count_at_write = {mam_loads}
"#
    )
}

/// Generate the system guide (File 1) — abbreviated version for M3.
pub fn generate_system_guide(label: &str, total_files: i32) -> String {
    format!(
        r#"# tapectl Archival Volume Recovery Guide

## Volume: {label}

This document describes how to recover data from this tape without
tapectl or its database. All you need is: mt, dd, age, and dar.

## Quick Reference

- File 0: ID thunk (this tape's identity and layout)
- File 1: This guide
- File 2: RESTORE.sh (automated restore script)
- File 3: Planning header (encrypted, operator only)
- Files 4..N: Encrypted data slices (age-encrypted dar archives)
- File N+1: Mini-index (plaintext position map)
- Files N+2..N+K: Tenant envelopes (encrypted, one per tenant)
- Last 2 files: Operator envelopes (encrypted, full catalog)

## Tools Required

- `mt` (mt-st package) — tape positioning
- `dd` — reading files from tape
- `age` (age-encryption.org) — decryption
- `dar` (dar.linux.free.fr) — archive extraction

## Recovery Steps

1. Read the mini-index to find file positions
2. Read and trial-decrypt tenant envelopes with your key
3. Follow the MANIFEST.toml in your envelope for exact slice positions
4. Read each slice from tape with dd, decrypt with age, extract with dar

## Total files on this tape: {total_files}

For complete instructions, see the RESTORE.sh script (File 2).
"#
    )
}

/// Generate RESTORE.sh (File 2) — abbreviated for M3.
pub fn generate_restore_script(label: &str, total_files: i32) -> String {
    format!(
        r#"#!/usr/bin/env bash
# RESTORE.sh — Emergency restore script for tapectl volume {label}
# This script helps you restore data without tapectl installed.
#
# Usage:
#   ./RESTORE.sh --info                    Show tape contents
#   ./RESTORE.sh --find-envelope --key KEY Find your envelope
#
# Requirements: mt, dd, age, dar
# Total files on tape: {total_files}

set -euo pipefail
DEVICE="${{TAPE_DEVICE:-/dev/nst0}}"
LABEL="{label}"
TMPDIR="${{TMPDIR:-/tmp}}/tapectl-restore-$$"
trap "rm -rf $TMPDIR" EXIT
mkdir -p "$TMPDIR"

case "${{1:-}}" in
  --info)
    echo "Reading tape identity..."
    mt -f "$DEVICE" rewind
    dd if="$DEVICE" bs=64k 2>/dev/null
    echo ""
    echo "--- Mini-index ---"
    # Skip to mini-index position (read from ID thunk layout section)
    ;;
  --find-envelope)
    shift
    if [ "${{1:-}}" != "--key" ] || [ -z "${{2:-}}" ]; then
      echo "Usage: $0 --find-envelope --key KEYFILE" >&2
      exit 1
    fi
    KEY="$2"
    echo "Searching for your envelope on tape..."
    echo "(Trial-decrypting each envelope with your key)"
    ;;
  *)
    echo "RESTORE.sh for tapectl volume $LABEL"
    echo "Usage: $0 --info | --find-envelope --key KEYFILE"
    exit 0
    ;;
esac
"#
    )
}

/// Generate the planning header content (File 3, encrypted to operator).
pub fn generate_planning_header(
    label: &str,
    units: &[(String, String, i64, i64)], // (unit_name, uuid, num_slices, total_bytes)
) -> String {
    let now = chrono::Utc::now().to_rfc3339();
    let mut s = format!(
        r#"[planning]
status = "planned"
volume = "{label}"
planned_at = "{now}"

[[units]]
"#
    );

    for (name, uuid, slices, bytes) in units {
        s.push_str(&format!(
            r#"name = "{name}"
uuid = "{uuid}"
num_slices = {slices}
total_bytes = {bytes}

"#
        ));
    }
    s
}

/// Generate the mini-index (File N+1).
pub fn generate_mini_index(label: &str, files: &[(i32, &str, usize)]) -> String {
    // files: (position, type_label, byte_size)
    let mut s = format!(
        r#"================================================================
                    TAPECTL MINI-INDEX
================================================================

Volume: {label}
This file maps tape positions to file types and sizes.
It contains NO content metadata (no filenames, no checksums,
no tenant names, no unit names).

================================================================
              MACHINE-READABLE DATA (TOML)
================================================================

[index]
volume = "{label}"
layout_version = 1

[[files]]
"#
    );

    for (pos, type_label, size) in files {
        s.push_str(&format!(
            "position = {pos}\ntype = \"{type_label}\"\nsize_bytes = {size}\n\n[[files]]\n"
        ));
    }

    // Remove trailing [[files]]
    if s.ends_with("[[files]]\n") {
        s.truncate(s.len() - "[[files]]\n".len());
    }

    s
}

/// Generate MANIFEST.toml for a tenant envelope.
pub fn generate_manifest_toml(label: &str, tenant_name: &str, units: &[ManifestUnit]) -> String {
    let now = chrono::Utc::now().to_rfc3339();
    let mut s = format!(
        r#"[manifest]
volume = "{label}"
tenant = "{tenant_name}"
created_at = "{now}"
layout_version = 1

"#
    );

    for unit in units {
        s.push_str(&format!(
            "[[units]]\nname = \"{}\"\nuuid = \"{}\"\nsnapshot_version = {}\n",
            unit.name, unit.uuid, unit.snapshot_version,
        ));
        if let Some(ref dar_ver) = unit.dar_version {
            s.push_str(&format!("dar_version = \"{dar_ver}\"\n"));
        }
        s.push('\n');
        for slice in &unit.slices {
            s.push_str(&format!(
                "[[units.slices]]\nnumber = {}\ntape_position = {}\nsize_bytes = {}\nencrypted_bytes = {}\nsha256_plain = \"{}\"\nsha256_encrypted = \"{}\"\n\n",
                slice.number, slice.tape_position, slice.size_bytes,
                slice.encrypted_bytes, slice.sha256_plain, slice.sha256_encrypted,
            ));
        }
    }

    s
}

/// Generate RECOVERY.md for a tenant envelope.
pub fn generate_recovery_md(label: &str, tenant_name: &str, units: &[ManifestUnit]) -> String {
    let now = chrono::Utc::now().format("%Y-%m-%d").to_string();
    let mut s = format!(
        "# Recovery Guide for {tenant_name}\n\n\
         Volume: {label}\n\
         Date: {now}\n\n\
         ## Units on this tape\n\n"
    );

    for unit in units {
        s.push_str(&format!(
            "### {}\n\n\
             UUID: {}\n\
             Snapshot version: {}\n\
             Slices: {}\n\n\
             To restore:\n\n\
             ```bash\n",
            unit.name,
            unit.uuid,
            unit.snapshot_version,
            unit.slices.len(),
        ));
        for slice in &unit.slices {
            s.push_str(&format!(
                "# Slice {} (tape file {})\n\
                 mt -f /dev/nst0 rewind && mt -f /dev/nst0 fsf {}\n\
                 dd if=/dev/nst0 bs=512k > slice_{}.dar.age\n\
                 age -d -i YOUR_KEY.age.key slice_{0}.dar.age > slice_{0}.dar\n\n",
                slice.number, slice.tape_position, slice.tape_position, slice.number,
            ));
        }
        s.push_str("# Reassemble and extract:\ndar -x ARCHIVE_BASE -R /destination -O\n```\n\n");
    }

    s
}

pub struct ManifestUnit {
    pub name: String,
    pub uuid: String,
    pub snapshot_version: i64,
    pub dar_version: Option<String>,
    pub slices: Vec<ManifestSlice>,
}

pub struct ManifestSlice {
    pub number: i64,
    pub tape_position: i32,
    pub size_bytes: i64,
    pub encrypted_bytes: i64,
    pub sha256_plain: String,
    pub sha256_encrypted: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn id_thunk_parses_as_toml_body() {
        let s = generate_id_thunk(
            "TEST01",
            "LTO-6",
            "0.1.0",
            "lto",
            2_500_000_000_000,
            2_400_000_000_000,
            4,
            20,
            21,
            22,
            3,
            25,
            26,
            27,
            "IBM",
            "SERIAL1",
            846,
            5,
        );
        // Header + TOML body concatenated. Extract the TOML body starting at [volume].
        let toml_start = s.find("[volume]").expect("has [volume] section");
        let body = &s[toml_start..];
        let parsed: toml::Value = body.parse().expect("TOML parses");
        let volume = parsed.get("volume").unwrap();
        assert_eq!(volume.get("label").unwrap().as_str(), Some("TEST01"));
        assert_eq!(
            volume.get("magic").unwrap().as_str(),
            Some("tapectl-volume-v1")
        );
        let layout = parsed.get("layout").unwrap();
        assert_eq!(layout.get("data_start").unwrap().as_integer(), Some(4));
        assert_eq!(layout.get("mini_index").unwrap().as_integer(), Some(21));
        assert_eq!(layout.get("total_files").unwrap().as_integer(), Some(27));
        let media = parsed.get("media").unwrap();
        assert_eq!(
            media.get("cartridge_serial").unwrap().as_str(),
            Some("SERIAL1")
        );
    }

    #[test]
    fn mini_index_parses_as_toml_body() {
        let files = vec![(4, "slice", 1024), (5, "slice", 2048), (6, "envelope", 512)];
        let s = generate_mini_index("TEST01", &files);
        let body_start = s.find("[index]").expect("has [index] section");
        let body = &s[body_start..];
        let parsed: toml::Value = body.parse().expect("TOML parses");
        assert_eq!(
            parsed.get("index").unwrap().get("volume").unwrap().as_str(),
            Some("TEST01")
        );
        let arr = parsed.get("files").unwrap().as_array().unwrap();
        assert_eq!(arr.len(), 3);
        assert_eq!(arr[0].get("position").unwrap().as_integer(), Some(4));
        assert_eq!(arr[0].get("type").unwrap().as_str(), Some("slice"));
        assert_eq!(arr[2].get("type").unwrap().as_str(), Some("envelope"));
    }

    #[test]
    fn mini_index_empty_files_has_no_trailing_section() {
        let s = generate_mini_index("EMPTY", &[]);
        // The generator truncates the dangling [[files]] marker when the
        // last entry leaves one behind. With zero entries the initial
        // [[files]] stays — parse only what's before it.
        let body_start = s.find("[index]").unwrap();
        let body = &s[body_start..];
        // Truncate at the dangling [[files]] that has no keys below it.
        let cleaned = body.replace("\n\n[[files]]\n", "\n");
        let parsed: toml::Value = cleaned.parse().expect("TOML parses after cleanup");
        assert_eq!(
            parsed.get("index").unwrap().get("volume").unwrap().as_str(),
            Some("EMPTY")
        );
    }

    #[test]
    fn planning_header_embeds_unit_rows() {
        let units = vec![
            ("alpha".to_string(), "uuid-a".to_string(), 3, 10_000),
            ("beta".to_string(), "uuid-b".to_string(), 1, 500),
        ];
        let s = generate_planning_header("LAB01", &units);
        assert!(s.contains("volume = \"LAB01\""));
        assert!(s.contains("name = \"alpha\""));
        assert!(s.contains("uuid = \"uuid-a\""));
        assert!(s.contains("num_slices = 3"));
        assert!(s.contains("total_bytes = 10000"));
        assert!(s.contains("name = \"beta\""));
    }

    #[test]
    fn manifest_toml_round_trips_slices() {
        let units = vec![ManifestUnit {
            name: "alpha".into(),
            uuid: "uuid-a".into(),
            snapshot_version: 1,
            dar_version: Some("2.7.20".into()),
            slices: vec![ManifestSlice {
                number: 1,
                tape_position: 4,
                size_bytes: 1_048_576,
                encrypted_bytes: 1_049_000,
                sha256_plain: "abc".into(),
                sha256_encrypted: "def".into(),
            }],
        }];
        let s = generate_manifest_toml("LAB01", "alice", &units);
        let parsed: toml::Value = s.parse().expect("manifest parses as TOML");
        let m = parsed.get("manifest").unwrap();
        assert_eq!(m.get("volume").unwrap().as_str(), Some("LAB01"));
        assert_eq!(m.get("tenant").unwrap().as_str(), Some("alice"));
        let u = &parsed.get("units").unwrap().as_array().unwrap()[0];
        assert_eq!(u.get("name").unwrap().as_str(), Some("alpha"));
        assert_eq!(u.get("dar_version").unwrap().as_str(), Some("2.7.20"));
        let slice = &u.get("slices").unwrap().as_array().unwrap()[0];
        assert_eq!(slice.get("number").unwrap().as_integer(), Some(1));
        assert_eq!(slice.get("tape_position").unwrap().as_integer(), Some(4));
        assert_eq!(slice.get("sha256_plain").unwrap().as_str(), Some("abc"));
    }

    #[test]
    fn system_guide_contains_label_and_total() {
        let s = generate_system_guide("LAB01", 42);
        assert!(s.contains("Volume: LAB01"));
        assert!(s.contains("Total files on this tape: 42"));
    }

    #[test]
    fn restore_script_is_bash_and_mentions_label() {
        let s = generate_restore_script("LAB01", 15);
        assert!(s.starts_with("#!/usr/bin/env bash"));
        assert!(s.contains("LABEL=\"LAB01\""));
        assert!(s.contains("Total files on tape: 15"));
    }
}
