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

/// Generate the system guide (File 1).
pub fn generate_system_guide(label: &str, total_files: i32) -> String {
    format!(
        r#"# tapectl Archival Volume Recovery Guide

## Volume: {label}

This document describes how to recover data from this tape without
tapectl or its database. All you need is: mt, dd, age, dar, and sha256sum.

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
- `dd` — reading raw data from tape
- `age` (age-encryption.org) — decryption
- `dar` (dar.linux.free.fr) — archive extraction
- `sha256sum` (coreutils) — integrity verification

## Automated Recovery (recommended)

The easiest way to recover is the RESTORE.sh script (File 2):

    mt -f /dev/nst0 rewind && mt -f /dev/nst0 fsf 2
    dd if=/dev/nst0 bs=512k | tr -d '\0' > RESTORE.sh
    chmod +x RESTORE.sh

    # See what's on the tape
    ./RESTORE.sh --info

    # Find your encrypted envelope
    ./RESTORE.sh --find-envelope --key your-key.age.key

    # Full restore to a directory
    ./RESTORE.sh --restore --key your-key.age.key --to /destination

## Manual Recovery Steps

If RESTORE.sh is not available, follow these steps:

1. Set tape to fixed 512KB block mode: `mt -f /dev/nst0 setblk 524288`
2. Read the ID thunk (file 0) and note the layout positions
3. Read the mini-index to get exact byte sizes for each file
4. Read and trial-decrypt tenant envelopes with your key
5. Parse the MANIFEST.toml in your envelope for slice positions
6. For each slice: read from tape, trim to exact size (block padding
   breaks age decryption), verify sha256, decrypt with age
7. Reassemble dar slices: `dar -x restore -R /destination -O -Q`

## Important: Block Padding

This tape uses 512KB (524288 byte) fixed block mode. Every file is
padded with zeros to the next block boundary. Encrypted files (data
slices, envelopes) MUST be trimmed to their exact byte size before
decryption — the padding zeros will cause age to reject the ciphertext.
Exact sizes are in the mini-index (`size_bytes` field).

## Total files on this tape: {total_files}
"#
    )
}

/// Generate RESTORE.sh (File 2) — self-contained emergency restore script.
///
/// Three modes:
/// - `--info`: read ID thunk + mini-index, display tape layout
/// - `--find-envelope --key KEYFILE`: trial-decrypt tenant/operator envelopes
/// - `--restore --key KEYFILE --to DIR [--unit U]`: full restore via dar
pub fn generate_restore_script(label: &str, total_files: i32) -> String {
    r#"#!/usr/bin/env bash
# RESTORE.sh — Emergency restore script for tapectl volume __LABEL__
# This script restores data from this tape WITHOUT tapectl installed.
# It reads the tape layout, finds your encrypted envelope, decrypts
# each data slice, and extracts the dar archive to a directory.
#
# Usage:
#   ./RESTORE.sh --info                                       Show tape layout
#   ./RESTORE.sh --find-envelope --key KEYFILE                Decrypt your envelope
#   ./RESTORE.sh --restore --key KEYFILE --to DIR [--unit U]  Full restore
#
# Requirements: mt, dd, age, dar, sha256sum
# Total files on tape: __TOTAL_FILES__

set -euo pipefail

DEVICE="${TAPE_DEVICE:-/dev/nst0}"
LABEL="__LABEL__"
BLOCK=524288  # 512 KB — tapectl fixed block size

WORK="${TMPDIR:-/tmp}/tapectl-restore-$$"
trap 'rm -rf "$WORK"' EXIT
mkdir -p "$WORK"

die()  { echo "FATAL: $*" >&2; exit 1; }
info() { echo ">>> $*"; }

# ---- prerequisite check ----

for tool in mt dd age sha256sum dar; do
  command -v "$tool" >/dev/null 2>&1 || die "missing required tool: $tool"
done

# ---- tape helpers ----

tape_init() {
  mt -f "$DEVICE" setblk "$BLOCK" 2>/dev/null \
    || die "cannot set block size — is $DEVICE a tape device?"
}

# Read a tape file at position $1 into file $2 (raw bytes, block-padded).
read_tape_raw() {
  local pos=$1 out=$2
  mt -f "$DEVICE" rewind
  [ "$pos" -gt 0 ] && mt -f "$DEVICE" fsf "$pos"
  dd if="$DEVICE" of="$out" bs="$BLOCK" 2>/dev/null
}

# Read a tape file at position $1 into file $2, stripping null padding.
# Use for plaintext files (ID thunk, mini-index) where padding zeros are
# harmless but would confuse text-processing tools.
read_tape_text() {
  local pos=$1 out=$2
  mt -f "$DEVICE" rewind
  [ "$pos" -gt 0 ] && mt -f "$DEVICE" fsf "$pos"
  dd if="$DEVICE" bs="$BLOCK" 2>/dev/null | tr -d '\0' > "$out"
}

# ---- TOML helpers (flat key = value parsing) ----

# Print the value for a TOML key on a "key = value" line.  Strips quotes.
toml_val() {
  local file=$1 key=$2
  awk -v k="$key" '
    $1 == k && $2 == "=" {
      v = $3
      for (i = 4; i <= NF; i++) v = v " " $i
      gsub(/^"/, "", v); gsub(/"$/, "", v)
      print v; exit
    }
  ' "$file"
}

# Parse mini-index [[files]] blocks into lines: position|type|size_bytes
parse_file_list() {
  local file=$1
  awk '
    /^\[\[files\]\]/ {
      if (p != "") print p "|" t "|" s
      p = ""; t = ""; s = ""
    }
    /^position = /   { p = $3 }
    /^type = /       { t = $3; gsub(/"/, "", t) }
    /^size_bytes = / { s = $3 }
    END { if (p != "") print p "|" t "|" s }
  ' "$file"
}

# Look up size_bytes for a given tape file position.
file_size_at() {
  local pos=$1 list=$2
  awk -F'|' -v p="$pos" '$1 == p { print $3; exit }' "$list"
}

# ---- read tape layout (ID thunk + mini-index) ----

read_layout() {
  tape_init

  info "Reading ID thunk (file 0)..."
  read_tape_text 0 "$WORK/id_thunk.txt"

  # Extract TOML body (starts at [volume] section)
  sed -n '/^\[volume\]/,$p' "$WORK/id_thunk.txt" > "$WORK/layout.toml"

  DATA_START=$(toml_val "$WORK/layout.toml" data_start)
  DATA_END=$(toml_val   "$WORK/layout.toml" data_end)
  MINI_IDX=$(toml_val   "$WORK/layout.toml" mini_index)
  FIRST_ENV=$(toml_val  "$WORK/layout.toml" first_envelope)
  NUM_ENV=$(toml_val    "$WORK/layout.toml" num_envelopes)
  OP_ENV=$(toml_val     "$WORK/layout.toml" operator_envelope)
  OP_BAK=$(toml_val     "$WORK/layout.toml" operator_envelope_backup)

  [ -n "$MINI_IDX" ] || die "cannot parse layout from ID thunk"

  info "Reading mini-index (file $MINI_IDX)..."
  read_tape_text "$MINI_IDX" "$WORK/mini_index.txt"

  sed -n '/^\[index\]/,$p' "$WORK/mini_index.txt" > "$WORK/index.toml"
  parse_file_list "$WORK/index.toml" > "$WORK/files.txt"
}

# ---- --info ----

do_info() {
  read_layout

  echo ""
  echo "=== tapectl volume: $LABEL ==="
  echo ""
  echo "Layout:"
  echo "  Data slices:       files $DATA_START .. $DATA_END"
  echo "  Mini-index:        file  $MINI_IDX"
  echo "  Tenant envelopes:  files $FIRST_ENV .. $((FIRST_ENV + NUM_ENV - 1))  ($NUM_ENV total)"
  echo "  Operator envelope: file  $OP_ENV  (backup: $OP_BAK)"
  echo ""
  echo "File map:"
  while IFS='|' read -r pos type size; do
    printf "  %3d  %-22s  %s bytes\n" "$pos" "$type" "$size"
  done < "$WORK/files.txt"
  echo ""
  echo "To decrypt your envelope:"
  echo "  $0 --find-envelope --key YOUR_KEY.age.key"
}

# ---- --find-envelope ----

do_find_envelope() {
  local keyfile=$1

  read_layout

  # Collect envelope positions: tenant envelopes, then operator + backup
  local positions=()
  local i
  for i in $(seq "$FIRST_ENV" $((FIRST_ENV + NUM_ENV - 1))); do
    positions+=("$i")
  done
  positions+=("$OP_ENV" "$OP_BAK")

  local found=0
  for pos in "${positions[@]}"; do
    info "Trying envelope at file $pos..."
    read_tape_raw "$pos" "$WORK/envelope.enc"

    # Trim to exact size — block padding breaks age decryption
    local esize
    esize=$(file_size_at "$pos" "$WORK/files.txt")
    if [ -n "$esize" ] && [ "$esize" -gt 0 ]; then
      truncate -s "$esize" "$WORK/envelope.enc"
    fi

    # Envelopes are age-encrypted tar archives (MANIFEST.toml + RECOVERY.md)
    rm -rf "$WORK/env" && mkdir -p "$WORK/env"
    if age -d -i "$keyfile" < "$WORK/envelope.enc" 2>/dev/null \
       | tar xf - -C "$WORK/env/" 2>/dev/null; then
      found=1
      echo ""
      info "Decrypted envelope at file $pos"
      if [ -f "$WORK/env/MANIFEST.toml" ]; then
        echo ""
        echo "--- MANIFEST.toml ---"
        cat "$WORK/env/MANIFEST.toml"
      fi
      if [ -f "$WORK/env/RECOVERY.md" ]; then
        echo ""
        echo "--- RECOVERY.md ---"
        cat "$WORK/env/RECOVERY.md"
      fi
      break
    fi
  done

  [ "$found" -eq 1 ] || die "no envelope matched the provided key"
  echo ""
  echo "To restore, run:"
  echo "  $0 --restore --key $keyfile --to /your/destination"
}

# ---- --restore ----

do_restore() {
  local keyfile=$1 destdir=$2 target_unit=$3

  mkdir -p "$destdir"

  read_layout

  # Step 1: find and decrypt envelope
  local positions=()
  local i
  for i in $(seq "$FIRST_ENV" $((FIRST_ENV + NUM_ENV - 1))); do
    positions+=("$i")
  done
  positions+=("$OP_ENV" "$OP_BAK")

  local found=0
  for pos in "${positions[@]}"; do
    read_tape_raw "$pos" "$WORK/envelope.enc"
    local esize
    esize=$(file_size_at "$pos" "$WORK/files.txt")
    if [ -n "$esize" ] && [ "$esize" -gt 0 ]; then
      truncate -s "$esize" "$WORK/envelope.enc"
    fi
    rm -rf "$WORK/env" && mkdir -p "$WORK/env"
    if age -d -i "$keyfile" < "$WORK/envelope.enc" 2>/dev/null \
       | tar xf - -C "$WORK/env/" 2>/dev/null; then
      found=1
      info "Decrypted envelope at file $pos"
      break
    fi
  done
  [ "$found" -eq 1 ] || die "no envelope matched the provided key"
  [ -f "$WORK/env/MANIFEST.toml" ] || die "envelope missing MANIFEST.toml"

  local manifest="$WORK/env/MANIFEST.toml"

  # Step 2: identify units in manifest
  local -a unit_names
  while IFS= read -r uname; do
    unit_names+=("$uname")
  done < <(awk '
    /^\[\[units\]\]/ { in_u = 1; next }
    in_u && /^name = / { gsub(/"/, "", $3); print $3; in_u = 0 }
    /^\[/              { in_u = 0 }
  ' "$manifest")

  [ ${#unit_names[@]} -gt 0 ] || die "no units in manifest"

  if [ -z "$target_unit" ]; then
    if [ ${#unit_names[@]} -eq 1 ]; then
      target_unit="${unit_names[0]}"
    else
      echo "Units in this envelope:"
      for u in "${unit_names[@]}"; do echo "  - $u"; done
      die "multiple units found — specify one with --unit NAME"
    fi
  fi

  # Step 3: parse slices for target unit from MANIFEST.toml
  info "Parsing slices for unit: $target_unit"
  awk -v unit="$target_unit" '
    function flush() {
      if (in_s && num != "") print num "|" tpos "|" eb "|" sha
      in_s = 0; num = ""; tpos = ""; eb = ""; sha = ""
    }
    /^\[\[units\]\]/               { in_u = 0; flush() }
    /^name = /                     { gsub(/"/, "", $3); if ($3 == unit) in_u = 1 }
    in_u && /^\[\[units\.slices\]\]/ { flush(); in_s = 1; next }
    in_s && /^number = /           { num = $3 }
    in_s && /^tape_position = /    { tpos = $3 }
    in_s && /^encrypted_bytes = /  { eb = $3 }
    in_s && /^sha256_encrypted = / { gsub(/"/, "", $3); sha = $3 }
    END { flush() }
  ' "$manifest" > "$WORK/slices.txt"

  local nslices
  nslices=$(wc -l < "$WORK/slices.txt")
  [ "$nslices" -gt 0 ] || die "no slices found for unit '$target_unit'"
  info "$nslices slice(s) to read"

  # Step 4: read, verify, decrypt each slice
  local dar_dir="$WORK/dar"
  mkdir -p "$dar_dir"
  local count=0

  while IFS='|' read -r num tpos eb sha; do
    count=$((count + 1))
    info "Slice $count/$nslices — tape file $tpos"

    read_tape_raw "$tpos" "$WORK/slice.enc"

    # Trim to encrypted size — block padding breaks age decryption
    if [ -n "$eb" ] && [ "$eb" -gt 0 ]; then
      truncate -s "$eb" "$WORK/slice.enc"
    fi

    # Verify SHA-256 checksum
    local actual
    actual=$(sha256sum "$WORK/slice.enc" | awk '{print $1}')
    if [ "$actual" != "$sha" ]; then
      die "slice $num checksum MISMATCH (expected ${sha:0:16}…, got ${actual:0:16}…)"
    fi
    info "  checksum verified"

    # Decrypt with age
    age -d -i "$keyfile" < "$WORK/slice.enc" > "$dar_dir/restore.$num.dar" \
      || die "cannot decrypt slice $num — wrong key?"

    local bytes
    bytes=$(wc -c < "$dar_dir/restore.$num.dar")
    info "  decrypted ($((bytes / 1048576)) MB)"
    rm -f "$WORK/slice.enc"

  done < "$WORK/slices.txt"

  # Step 5: extract with dar
  info "Extracting archive to $destdir ..."
  dar -x "$dar_dir/restore" -R "$destdir" -O -Q \
    || die "dar extraction failed"

  rm -rf "$dar_dir"
  echo ""
  info "RESTORE COMPLETE"
  info "Unit '$target_unit' restored to: $destdir"
}

# ---- main ----

case "${1:-}" in
  --info)
    do_info
    ;;
  --find-envelope)
    shift
    [ "${1:-}" = "--key" ] && [ -n "${2:-}" ] \
      || die "usage: $0 --find-envelope --key KEYFILE"
    [ -f "$2" ] || die "key file not found: $2"
    do_find_envelope "$2"
    ;;
  --restore)
    shift
    key="" dest="" unit=""
    while [ $# -gt 0 ]; do
      case "$1" in
        --key)  key="${2:-}";  shift 2 ;;
        --to)   dest="${2:-}"; shift 2 ;;
        --unit) unit="${2:-}"; shift 2 ;;
        *) die "unknown option: $1" ;;
      esac
    done
    [ -n "$key" ]  || die "usage: $0 --restore --key KEYFILE --to DIR [--unit U]"
    [ -n "$dest" ] || die "usage: $0 --restore --key KEYFILE --to DIR [--unit U]"
    [ -f "$key" ]  || die "key file not found: $key"
    do_restore "$key" "$dest" "$unit"
    ;;
  --help|-h)
    echo "RESTORE.sh — Emergency restore for tapectl volume $LABEL"
    echo ""
    echo "Usage:"
    echo "  $0 --info                                       Show tape layout"
    echo "  $0 --find-envelope --key KEYFILE                Decrypt your envelope"
    echo "  $0 --restore --key KEYFILE --to DIR [--unit U]  Full restore"
    echo ""
    echo "Environment:"
    echo "  TAPE_DEVICE   Tape device path (default: /dev/nst0)"
    echo ""
    echo "Requirements: mt, dd, age, dar, sha256sum"
    ;;
  *)
    echo "RESTORE.sh for tapectl volume $LABEL"
    echo "Run '$0 --help' for usage."
    ;;
esac
"#
    .replace("__LABEL__", label)
    .replace("__TOTAL_FILES__", &total_files.to_string())
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

    #[test]
    fn restore_script_has_all_modes() {
        let s = generate_restore_script("VOL01", 27);
        // Block size matches tapectl's 512KB fixed block mode
        assert!(s.contains("BLOCK=524288"));
        // All three command modes
        assert!(s.contains("--info)"));
        assert!(s.contains("--find-envelope)"));
        assert!(s.contains("--restore)"));
        // Key operations
        assert!(s.contains("mt -f \"$DEVICE\" setblk"));
        assert!(s.contains("age -d -i"));
        assert!(s.contains("sha256sum"));
        assert!(s.contains("dar -x"));
        assert!(s.contains("truncate -s"));
        // Envelope is tar archive
        assert!(s.contains("tar xf"));
        assert!(s.contains("MANIFEST.toml"));
    }
}
