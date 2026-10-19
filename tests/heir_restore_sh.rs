//! RESTORE.sh — the heir's script, File 2 of every tape — run end to end
//! against a fake tape (issues #396, #405, #412).
//!
//! The script under test is the one `generate_restore_script_v2` generates
//! now, run whole, mode by mode, the way an heir runs it. Three tools are
//! faked on `PATH`:
//!
//! - `mt` and `dd` drive a fake tape: a directory of files `tape/<n>`, each
//!   block-padded as on a real cartridge, and a state file `pos` that holds
//!   the file the head is at the start of (`N`), inside of (`N+`), or `?` when
//!   it is lost. Every `mt` and `dd` call appends one line to `ops`, which is
//!   what the navigation tests count. A `dd` that reads a file to its end
//!   leaves the head at the start of the next one, as the st driver does.
//! - `age`, because the real age CLI is not installed on CI. The stub honours
//!   a 32-byte clear header naming the recipients, `STUBAGE:alice,op,esc`; an
//!   identity file holds one recipient's name. A header carrying ` BAD` fails
//!   the way a damaged payload does, after the key has matched.
//!
//! Everything between them — awk, sha256sum, tar, mkfifo and the real `dar`,
//! which builds the slices here and extracts them in the script — is real.
//!
//! Every run is under `timeout`: the streaming restore has a reader and a
//! writer that can wait on each other, and a regression there must fail this
//! suite, not hang it.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;

use sha2::{Digest, Sha256};
use tapectl::volume::layout::{
    generate_front_index, generate_id_thunk_v2, generate_manifest_toml, generate_recovery_md,
    generate_restore_script_v2, generate_seal_marker, generate_system_guide_v2, FrontIndexFile,
    IdThunkV2Params, ManifestSlice, ManifestUnit,
};

/// The tape's fixed block size; every file on the fake tape is padded to it.
const BLOCK: usize = 524_288;
const LABEL: &str = "HEIR01";

fn sha256_hex(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn padded(mut bytes: Vec<u8>) -> Vec<u8> {
    let len = bytes.len().div_ceil(BLOCK).max(1) * BLOCK;
    bytes.resize(len, 0);
    bytes
}

/// Deterministic, incompressible-looking bytes, with an interior run of NULs
/// so a recipe that strips every NUL would corrupt them.
fn content(seed: u64, len: usize) -> Vec<u8> {
    let mut x = seed
        .wrapping_mul(6364136223846793005)
        .wrapping_add(1442695040888963407);
    let mut out = Vec::with_capacity(len);
    for i in 0..len {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        out.push(if (1000..1040).contains(&i) {
            0
        } else {
            (x >> 24) as u8
        });
    }
    out
}

/// The stub age's ciphertext: a 32-byte clear header naming the recipients,
/// then the plaintext.
fn stub_encrypt(recipients: &str, flag: &str, plain: &[u8]) -> Vec<u8> {
    let mut hdr = format!("STUBAGE:{recipients}");
    if !flag.is_empty() {
        hdr.push(' ');
        hdr.push_str(flag);
    }
    assert!(hdr.len() <= 32, "stub header too long: {hdr}");
    while hdr.len() < 32 {
        hdr.push(' ');
    }
    let mut out = hdr.into_bytes();
    out.extend_from_slice(plain);
    out
}

fn write_exec(path: &Path, body: &str) {
    use std::os::unix::fs::PermissionsExt;
    std::fs::write(path, body).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
}

/// `mt`: rewind, fsf N, status, setblk, --version. `FAKE_MT_GNU=1` makes it
/// GNU cpio's mt (no setblk, its own status wording); `FAKE_MT_STATUS_LIE=1`
/// makes `status` report one file further on than the head really is, as if
/// something else had moved the tape. `FAKE_SWAP_ON_SETBLK=N` puts another
/// cartridge in the drive at the Nth `setblk`: the directory `tape2` becomes
/// `tape`, the head is at the start, and a line `swap` is logged to `ops`
/// (what follows it is the second tape's). A script asking for another
/// cartridge sets the block size again once it has one.
const FAKE_MT: &str = r#"#!/bin/sh
T="$FAKE_TAPE"
op=""; n=""
while [ $# -gt 0 ]; do
  case "$1" in
    -f) shift 2 ;;
    --version)
      if [ -n "${FAKE_MT_GNU:-}" ]; then echo "mt (GNU cpio) 2.15"; else echo "mt-st v. 1.7"; fi
      exit 0 ;;
    *) if [ -z "$op" ]; then op=$1; else n=$1; fi; shift ;;
  esac
done
echo "mt $op${n:+ $n}" >>"$T/ops"
pos=$(cat "$T/pos")
count=$(ls "$T/tape" | wc -l)
case "$op" in
  rewind) echo 0 >"$T/pos" ;;
  eod | eom) [ -z "${FAKE_MT_NO_EOD:-}" ] || exit 2; echo "$count" >"$T/pos" ;;
  bsfm)
    # Back over n filemarks, then forward over the last: the start of the
    # file n-1 before this one (from inside a file, n-1 before it).
    case "$pos" in
      *+) p=${pos%+} ;;
      *[!0-9]*) echo "?" >"$T/pos"; exit 1 ;;
      *) p=$pos ;;
    esac
    to=$(( p - n + 1 ))
    if [ "$to" -lt 0 ]; then echo 0 >"$T/pos"; exit 1; fi
    echo "$to" >"$T/pos" ;;
  fsf)
    case "$pos" in
      *+) to=$(( ${pos%+} + n )) ;;
      *[!0-9]*) echo "?" >"$T/pos"; exit 0 ;;
      *) to=$(( pos + n )) ;;
    esac
    if [ "$to" -gt "$count" ]; then
      echo "?" >"$T/pos"
      echo "/dev/nst0: Input/output error" >&2
      exit 1
    fi
    echo "$to" >"$T/pos" ;;
  status)
    case "$pos" in
      *+) f=${pos%+}; b=1 ;;
      *[!0-9]*) f=-1; b=-1 ;;
      *) f=$pos; b=0 ;;
    esac
    [ -z "${FAKE_MT_STATUS_LIE:-}" ] || f=$((f + 1))
    if [ -n "${FAKE_MT_GNU:-}" ]; then
      printf 'drive type = 114\ndrive status = 1124073472\nsense key error = 0\nresidue count = 0\nfile number = %s\nblock number = %s\n' "$f" "$b"
    else
      echo "SCSI 2 tape drive:"
      echo "File number=$f, block number=$b, partition=0."
      echo "Tape block size 524288 bytes. Density code 0x5a (LTO-6)."
    fi ;;
  setblk)
    if [ -n "${FAKE_MT_GNU:-}" ]; then
      echo "mt: invalid argument 'setblk' for 'operation'" >&2
      exit 2
    fi
    if [ -n "${FAKE_SWAP_ON_SETBLK:-}" ]; then
      c=$(cat "$T/setblk.count" 2>/dev/null || echo 0)
      c=$((c + 1))
      echo "$c" >"$T/setblk.count"
      if [ "$c" = "$FAKE_SWAP_ON_SETBLK" ]; then
        mv "$T/tape" "$T/tape.first"
        mv "$T/tape2" "$T/tape"
        echo 0 >"$T/pos"
        echo swap >>"$T/ops"
      fi
    fi ;;
esac
exit 0
"#;

/// `dd if=<tape> [of=FILE] bs=...`: one tape file per call. `FAKE_DD_FAIL=N`
/// makes the read of file N fail with an I/O error and lose the position.
const FAKE_DD: &str = r#"#!/bin/sh
T="$FAKE_TAPE"
of=""
count=""
for a in "$@"; do
  case "$a" in
    of=*) of=${a#of=} ;;
    count=*) count=${a#count=} ;;
  esac
done
pos=$(cat "$T/pos")
echo "dd $pos" >>"$T/ops"
case "$pos" in
  *[!0-9]*)
    echo "fake dd: the head is not at the start of a file ($pos)" >&2
    echo "?" >"$T/pos"
    if [ -n "$of" ]; then printf 'NOT-A-FILE-START' >"$of"; else printf 'NOT-A-FILE-START'; fi
    exit 0 ;;
esac
if [ "${FAKE_DD_FAIL:-none}" = "$pos" ]; then
  echo "dd: error reading '/dev/nst0': Input/output error" >&2
  echo "?" >"$T/pos"
  exit 1
fi
f="$T/tape/$pos"
if [ ! -f "$f" ]; then echo "?" >"$T/pos"; exit 0; fi
if [ -n "$count" ] && [ "$(wc -c <"$f")" -gt $((count * 524288)) ]; then
  # A bounded read that stops inside the file.
  if [ -n "$of" ]; then head -c $((count * 524288)) "$f" >"$of"; else head -c $((count * 524288)) "$f"; fi
  echo "$pos+" >"$T/pos"
  exit 0
fi
if [ -n "$of" ]; then
  cat "$f" >"$of" || { echo "?" >"$T/pos"; exit 1; }
elif ! cat "$f"; then
  echo "$pos+" >"$T/pos"
  exit 1
fi
echo $((pos + 1)) >"$T/pos"
"#;

/// `age -d [-i ID]... [FILE]`: see the module comment.
const STUB_AGE: &str = r#"#!/usr/bin/env bash
ids=()
in=""
while [ $# -gt 0 ]; do
  case "$1" in
    -d | --decrypt) shift ;;
    -i | --identity) ids+=("$2"); shift 2 ;;
    -o | --output) exec >"$2"; shift 2 ;;
    *) in=$1; shift ;;
  esac
done
[ -z "$in" ] || exec <"$in"
LC_ALL=C IFS= read -r -N 32 hdr || { echo "age: error: failed to read header: unexpected EOF" >&2; exit 1; }
case "$hdr" in
  STUBAGE:*) ;;
  *) echo "age: error: failed to read header: parsing age header: unexpected intro" >&2; exit 1 ;;
esac
rest=${hdr#STUBAGE:}
recips=${rest%% *}
flag=$(printf '%s' "$rest" | awk '{ print $2 }')
for i in "${ids[@]}"; do
  [ -r "$i" ] || { echo "age: error: failed to open file $i" >&2; exit 1; }
  k=$(cat "$i")
  case ",$recips," in
    *",$k,"*)
      if [ "$flag" = BAD ]; then
        head -c 100
        echo "age: error: failed to decrypt and authenticate payload chunk" >&2
        exit 1
      fi
      exec cat ;;
  esac
done
echo "age: error: no identity matched any of the recipients" >&2
exit 1
"#;

/// Which dar catalogue a unit's envelope carries.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Cat {
    /// The isolated catalogue of the very dar run whose slices are on tape.
    Matched,
    /// One from a second dar run of the same content, as a re-staged
    /// snapshot carries (#419): dar refuses it against these slices.
    FromAnotherRun,
    /// None at all.
    Absent,
}

/// One unit as staged: its files, and the dar slices built from them.
struct UnitSpec {
    cat: Cat,
    name: &'static str,
    uuid: &'static str,
    version: i64,
    seed: u64,
    files: usize,
    file_len: usize,
    /// File names, when the unit's files are not `file{i}.bin`.
    names: &'static [&'static str],
    /// Byte flips made in a slice's plaintext BEFORE it is encrypted and
    /// hashed, as (slice index, offset): the damage is in the archive as
    /// staged, so the tape's hashes agree with it and only dar's own CRC
    /// per file sees it.
    corrupt: &'static [(usize, usize)],
    /// Files (by index into the unit's file names) whose bytes are changed in
    /// the source tree AFTER the index's sha256 was taken and BEFORE dar reads
    /// them: dar's own checksum is computed over the changed bytes and so
    /// passes, and only the recorded sha256 sees them. This is the damage
    /// dar's short checksum misses (ADR-0012 item 103).
    tamper: &'static [usize],
}

struct TenantSpec {
    name: &'static str,
    units: Vec<UnitSpec>,
}

/// Where a unit landed on the fake tape, for assertions.
#[derive(Clone, Debug)]
pub struct UnitOnTape {
    pub name: String,
    pub src: PathBuf,
    pub positions: Vec<i64>,
    /// Each slice's exact size on tape (before the block padding), and the
    /// sha256 of those bytes.
    pub sizes: Vec<usize>,
    pub hashes: Vec<String>,
}

/// A built fake volume, kept as a template and copied per test.
struct Template {
    dir: PathBuf,
    units: Vec<UnitOnTape>,
    total_files: i32,
}

fn run_dar(args: &[&str]) {
    let out = Command::new("dar")
        .args(args)
        .env("HOME", "/nonexistent")
        .output()
        .expect("dar must be on PATH (issue #43)");
    assert!(
        out.status.success(),
        "dar {args:?} failed: {}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
}

/// Build the volume every test starts from: tenant alice with two units
/// (a four-slice one and a two-slice one), tenant bob with one, the
/// operator envelope and its backup, and the seal marker.
fn template() -> &'static Template {
    static T: OnceLock<Template> = OnceLock::new();
    T.get_or_init(|| {
        let tenants = vec![
            TenantSpec {
                name: "alice",
                units: vec![
                    UnitSpec {
                        name: "photos/2019",
                        cat: Cat::Matched,
                        uuid: "11111111-2222-3333-4444-555555555555",
                        version: 2,
                        seed: 1,
                        files: 5,
                        file_len: 150_000,
                        names: &[],
                        corrupt: &[],
                        tamper: &[],
                    },
                    UnitSpec {
                        name: "docs",
                        cat: Cat::Matched,
                        uuid: "22222222-3333-4444-5555-666666666666",
                        version: 1,
                        seed: 2,
                        files: 2,
                        file_len: 140_000,
                        names: &[],
                        corrupt: &[],
                        tamper: &[],
                    },
                ],
            },
            TenantSpec {
                name: "bob",
                units: vec![UnitSpec {
                    name: "ledgers",
                    cat: Cat::Matched,
                    uuid: "33333333-4444-5555-6666-777777777777",
                    version: 3,
                    seed: 3,
                    files: 2,
                    file_len: 130_000,
                    names: &[],
                    corrupt: &[],
                    tamper: &[],
                }],
            },
            TenantSpec {
                name: "carol",
                units: vec![
                    UnitSpec {
                        name: "restaged",
                        cat: Cat::FromAnotherRun,
                        uuid: "44444444-5555-6666-7777-888888888888",
                        version: 4,
                        seed: 4,
                        files: 3,
                        file_len: 120_000,
                        names: &[],
                        corrupt: &[],
                        tamper: &[],
                    },
                    UnitSpec {
                        name: "nocat",
                        cat: Cat::Absent,
                        uuid: "55555555-6666-7777-8888-999999999999",
                        version: 1,
                        seed: 5,
                        files: 3,
                        file_len: 110_000,
                        names: &[],
                        corrupt: &[],
                        tamper: &[],
                    },
                ],
            },
        ];
        build_template("main", &tenants, true)
    })
}

/// A second volume, for the lone-unit restore (#449 item 1): the FIRST tenant
/// envelope lists exactly one unit, and the other tenant lists three, so an
/// operator or escrow key opens more than the envelope a default restore
/// uses. Smaller than the main one: only the envelopes are the point.
fn lone_template() -> &'static Template {
    static T: OnceLock<Template> = OnceLock::new();
    T.get_or_init(|| {
        let small = |name, uuid, version, seed| UnitSpec {
            cat: Cat::Matched,
            name,
            uuid,
            version,
            seed,
            files: 2,
            file_len: 110_000,
            names: &[],
            corrupt: &[],
            tamper: &[],
        };
        let tenants = vec![
            TenantSpec {
                name: "bob",
                units: vec![small(
                    "ledgers",
                    "33333333-4444-5555-6666-777777777777",
                    3,
                    13,
                )],
            },
            TenantSpec {
                name: "alice",
                units: vec![
                    small("photos", "11111111-2222-3333-4444-555555555555", 2, 11),
                    small("docs", "22222222-3333-4444-5555-666666666666", 1, 12),
                    small("notes", "66666666-7777-8888-9999-000000000000", 1, 14),
                ],
            },
        ];
        build_template("lone", &tenants, false)
    })
}

/// One tenant with one unit: the operator envelope lists the same unit.
fn solo_template() -> &'static Template {
    static T: OnceLock<Template> = OnceLock::new();
    T.get_or_init(|| {
        let tenants = vec![TenantSpec {
            name: "bob",
            units: vec![UnitSpec {
                cat: Cat::Matched,
                name: "ledgers",
                uuid: "33333333-4444-5555-6666-777777777777",
                version: 3,
                seed: 13,
                files: 2,
                file_len: 110_000,
                names: &[],
                corrupt: &[],
                tamper: &[],
            }],
        }];
        build_template("solo", &tenants, false)
    })
}

// ---- the cross-volume index (volume-format-v2.md 1.5), built from the spec ----

/// One `index/units.tsv` row: a (version, holding volume) pair.
struct IdxRow {
    tenant: &'static str,
    unit: String,
    unit_uuid: String,
    version: i64,
    num_slices: i64,
    first: i64,
    last: i64,
    bytes: i64,
    volume: &'static str,
    volume_uuid: &'static str,
    serial: &'static str,
    generation: &'static str,
    state: &'static str,
    sealed_at: &'static str,
}

/// sec 1.5's escapes, and nothing else: backslash, TAB, LF, CR.
fn idx_esc(s: &str) -> String {
    s.replace('\\', "\\\\")
        .replace('\t', "\\t")
        .replace('\n', "\\n")
        .replace('\r', "\\r")
}

impl IdxRow {
    fn field(&self, col: &str) -> String {
        match col {
            "tenant" => self.tenant.to_string(),
            "unit" => self.unit.clone(),
            "unit_uuid" => self.unit_uuid.clone(),
            "version" => self.version.to_string(),
            "num_slices" => self.num_slices.to_string(),
            "first_slice" => self.first.to_string(),
            "last_slice" => self.last.to_string(),
            "version_bytes" => self.bytes.to_string(),
            "volume" => self.volume.to_string(),
            "volume_uuid" => self.volume_uuid.to_string(),
            "cartridge_serial" => self.serial.to_string(),
            "generation" => self.generation.to_string(),
            "state" => self.state.to_string(),
            "sealed_at" => self.sealed_at.to_string(),
            // A column a later tapectl might add: a reader must ignore it.
            "zz_future" => "ignored".to_string(),
            other => panic!("no such column {other}"),
        }
    }
}

/// sec 1.5's column order for `units.tsv` (with `tenant` for the operator's).
const UNITS_COLUMNS: &[&str] = &[
    "tenant",
    "unit",
    "unit_uuid",
    "version",
    "num_slices",
    "first_slice",
    "last_slice",
    "version_bytes",
    "volume",
    "volume_uuid",
    "cartridge_serial",
    "generation",
    "state",
    "sealed_at",
];
/// The same columns reversed, with an unknown one in the middle: a reader
/// finds columns by header name, so this must read exactly the same.
const UNITS_COLUMNS_PERMUTED: &[&str] = &[
    "sealed_at",
    "state",
    "generation",
    "cartridge_serial",
    "volume_uuid",
    "zz_future",
    "volume",
    "version_bytes",
    "last_slice",
    "first_slice",
    "num_slices",
    "version",
    "unit_uuid",
    "unit",
];

const OTHER_UUID: &str = "bbbbbbbb-1111-4222-8333-444444444444";
const QUAR_UUID: &str = "cccccccc-5555-4666-8777-888888888888";
const THIS_UUID: &str = "a1b2c3d4-e5f6-4789-a123-456789abcdef";

/// Render rows in sec 1.5's sort order (`tenant`, `unit`, `version`,
/// `volume`; bytewise) under the given column order.
fn render_units(mut rows: Vec<IdxRow>, columns: &[&str]) -> String {
    rows.sort_by(|a, b| {
        (a.tenant, &a.unit, a.version, a.volume).cmp(&(b.tenant, &b.unit, b.version, b.volume))
    });
    let mut out = columns.join("\t");
    out.push('\n');
    for r in &rows {
        let cells: Vec<String> = columns.iter().map(|c| idx_esc(&r.field(c))).collect();
        out.push_str(&cells.join("\t"));
        out.push('\n');
    }
    out
}

/// The holders the fixture gives a unit's version: this tape always (`this-tape`,
/// no `sealed_at`); `photos/2019` also on a sealed tape (with an older version
/// only that tape holds) and on a quarantined one with no serial read;
/// `docs` also on the sealed tape.
fn idx_rows_for(
    tenant: &'static str,
    unit: &str,
    uuid: &str,
    version: i64,
    slices: i64,
    bytes: i64,
) -> Vec<IdxRow> {
    let row = |version: i64,
               slices: i64,
               bytes: i64,
               volume: &'static str,
               volume_uuid: &'static str,
               serial: &'static str,
               generation: &'static str,
               state: &'static str,
               sealed_at: &'static str| IdxRow {
        tenant,
        unit: unit.to_string(),
        unit_uuid: uuid.to_string(),
        version,
        num_slices: slices,
        first: 1,
        last: slices,
        bytes,
        volume,
        volume_uuid,
        serial,
        generation,
        state,
        sealed_at,
    };
    let mut rows = vec![row(
        version,
        slices,
        bytes,
        LABEL,
        THIS_UUID,
        "SERIAL01",
        "LTO-6",
        "this-tape",
        "",
    )];
    let other = |version, slices, bytes| {
        row(
            version,
            slices,
            bytes,
            "OTHER02",
            OTHER_UUID,
            "OTHERSER02",
            "LTO-6",
            "sealed",
            "2026-09-01T10:00:00Z",
        )
    };
    if unit == "photos/2019" {
        rows.push(other(version, slices, bytes));
        rows.push(row(
            version,
            slices,
            bytes,
            "QUAR03",
            QUAR_UUID,
            "",
            "LTO-5",
            "quarantined",
            "2026-08-15T09:30:00Z",
        ));
        // An older version of the same unit, on the other tape only.
        rows.push(other(1, 3, 3_000_000));
    }
    if unit == "docs" {
        rows.push(other(version, slices, bytes));
    }
    rows
}

/// One `index/paths.tsv` row: (kind, size, the recorded sha256 as 64 hex
/// characters or empty, path as the unit holds it).
type PathRow = (&'static str, u64, String, String);

/// A tenant's paths for a unit: what is really in its source tree, and the
/// paths the tests look for. A path with a TAB, an LF and a backslash; one
/// with a literal backslash followed by `t` (never a TAB); and, in `docs`,
/// enough matching paths to pass the print limit. A file that is really in
/// the tree records the hash it had when it was staged (`hashes`, taken
/// before a `tamper`); the made-up rows, which the archive does not hold,
/// record none.
fn idx_paths_for(unit: &str, src: &Path, hashes: &BTreeMap<String, String>) -> Vec<PathRow> {
    let mut rows: Vec<PathRow> = Vec::new();
    let hash_of = |rel: &str| hashes.get(rel).cloned().unwrap_or_default();
    for e in std::fs::read_dir(src).unwrap() {
        let e = e.unwrap();
        let name = e.file_name().to_string_lossy().to_string();
        if e.file_type().unwrap().is_dir() {
            rows.push(("d", 0, String::new(), name.clone()));
            for f in std::fs::read_dir(e.path()).unwrap() {
                let f = f.unwrap();
                let rel = format!("{name}/{}", f.file_name().to_string_lossy());
                rows.push(("f", f.metadata().unwrap().len(), hash_of(&rel), rel));
            }
        } else {
            let sha = hash_of(&name);
            rows.push(("f", e.metadata().unwrap().len(), sha, name));
        }
    }
    if unit == "photos/2019" {
        rows.push((
            "f",
            11,
            String::new(),
            "weird\ttab\nnew\\back.txt".to_string(),
        ));
        rows.push(("f", 12, String::new(), "lit\\tera.txt".to_string()));
        rows.push(("l", 0, String::new(), "link-to-file0".to_string()));
    }
    if unit == "docs" {
        for i in 1..=120 {
            rows.push(("f", 100 + i, String::new(), format!("bulk/item-{i:03}.dat")));
        }
    }
    rows
}

/// The columns of a fixture `paths.tsv`, in an order that is not the format's
/// and with one the reader does not know: the script finds columns by their
/// header names (sec 1.5), and every restore and `--find` below reads this.
const PATHS_COLUMNS_PERMUTED: &[&str] = &[
    "sha256",
    "path",
    "zz_future",
    "unit",
    "kind",
    "version",
    "size_bytes",
];

/// `paths.tsv`: sorted by unit, version, then path (bytewise). Without
/// `hash_column` it is the file an index writer from before the hash was added
/// wrote: the same columns, no `sha256`.
fn render_paths(mut units: Vec<(String, i64, Vec<PathRow>)>, hash_column: bool) -> String {
    let columns: Vec<&str> = PATHS_COLUMNS_PERMUTED
        .iter()
        .copied()
        .filter(|c| hash_column || *c != "sha256")
        .collect();
    units.sort_by(|a, b| (&a.0, a.1).cmp(&(&b.0, b.1)));
    let mut out = columns.join("\t");
    out.push('\n');
    for (unit, version, mut rows) in units {
        rows.sort_by(|a, b| a.3.as_bytes().cmp(b.3.as_bytes()));
        for (kind, size, sha, path) in rows {
            let field = |name: &str| -> String {
                match name {
                    "unit" => idx_esc(&unit),
                    "version" => version.to_string(),
                    "kind" => kind.to_string(),
                    "size_bytes" => size.to_string(),
                    "sha256" => sha.clone(),
                    "path" => idx_esc(&path),
                    _ => "x".to_string(),
                }
            };
            let cells: Vec<String> = columns.iter().map(|c| field(c)).collect();
            out.push_str(&cells.join("\t"));
            out.push('\n');
        }
    }
    out
}

fn build_template(tag: &str, tenants: &[TenantSpec], with_index: bool) -> Template {
    build_template_with(tag, tenants, with_index, true)
}

/// `hash_column` false writes the index as a 1.3.0 build from before the
/// `sha256` column wrote it (ADR-0012 item 103 added it).
fn build_template_with(
    tag: &str,
    tenants: &[TenantSpec],
    with_index: bool,
    hash_column: bool,
) -> Template {
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(format!(
        "heir-restore-sh-template-{tag}-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    for sub in ["tape", "src", "keys", "work"] {
        std::fs::create_dir_all(dir.join(sub)).unwrap();
    }
    for k in ["alice", "bob", "carol", "op", "esc", "stranger"] {
        std::fs::write(dir.join("keys").join(format!("{k}.key")), k).unwrap();
    }

    // dar every unit: its slices (plaintext) and its isolated catalogue.
    struct Staged {
        tenant: &'static str,
        unit: ManifestUnit,
        slices: Vec<Vec<u8>>,
        catalogue: Option<Vec<u8>>,
        src: PathBuf,
        /// The sha256 of each file as staged: what the index records.
        hashes: BTreeMap<String, String>,
    }
    let mut staged: Vec<Staged> = Vec::new();
    for t in tenants {
        for u in &t.units {
            let src = dir.join("src").join(u.name);
            std::fs::create_dir_all(&src).unwrap();
            let mut hashes: BTreeMap<String, String> = BTreeMap::new();
            for i in 0..u.files {
                let name = if u.names.is_empty() {
                    format!("file{i}.bin")
                } else {
                    u.names[i].to_string()
                };
                let bytes = content(u.seed * 100 + i as u64, u.file_len);
                hashes.insert(name.clone(), sha256_hex(&bytes));
                std::fs::write(src.join(&name), &bytes).unwrap();
            }
            std::fs::create_dir_all(src.join("sub")).unwrap();
            let note = format!("unit {}\n", u.name);
            hashes.insert("sub/note.txt".to_string(), sha256_hex(note.as_bytes()));
            std::fs::write(src.join("sub").join("note.txt"), note).unwrap();
            // The damage dar cannot see: changed after the hash was taken,
            // before dar reads the file, so dar records and later checks the
            // changed bytes.
            for &i in u.tamper {
                let name = if u.names.is_empty() {
                    format!("file{i}.bin")
                } else {
                    u.names[i].to_string()
                };
                let mut bytes = content(u.seed * 100 + i as u64, u.file_len);
                let mid = bytes.len() / 2;
                bytes[mid] ^= 0x55;
                std::fs::write(src.join(&name), &bytes).unwrap();
            }
            let work = dir.join("work").join(u.uuid);
            std::fs::create_dir_all(&work).unwrap();
            let base = work.join("arch");
            run_dar(&[
                "-c",
                base.to_str().unwrap(),
                "-R",
                src.to_str().unwrap(),
                "-s",
                // A size at which dar 2.7.13 cuts short a file that runs into
                // the last slice when it reads sequentially without -A.
                "100k",
                "-Q",
                "-N",
            ]);
            let cat = work.join(format!("{}_v{}", &u.uuid[..8], u.version));
            let cat_of = if u.cat == Cat::FromAnotherRun {
                // A second dar run of the same content: a new data label.
                let again = work.join("again");
                run_dar(&[
                    "-c",
                    again.to_str().unwrap(),
                    "-R",
                    src.to_str().unwrap(),
                    "-s",
                    "100k",
                    "-Q",
                    "-N",
                ]);
                again
            } else {
                base.clone()
            };
            run_dar(&[
                "-C",
                cat.to_str().unwrap(),
                "-A",
                cat_of.to_str().unwrap(),
                "-Q",
                "-N",
            ]);
            let mut slices = Vec::new();
            for n in 1.. {
                let p = work.join(format!("arch.{n}.dar"));
                if !p.exists() {
                    break;
                }
                slices.push(std::fs::read(p).unwrap());
            }
            for &(idx, off) in u.corrupt {
                slices[idx][off] ^= 0xff;
            }
            let catalogue = (u.cat != Cat::Absent).then(|| {
                std::fs::read(work.join(format!("{}_v{}.1.dar", &u.uuid[..8], u.version))).unwrap()
            });
            staged.push(Staged {
                tenant: t.name,
                unit: ManifestUnit {
                    name: u.name.to_string(),
                    uuid: u.uuid.to_string(),
                    snapshot_version: u.version,
                    stage_set_id: u.version,
                    dar_version: Some("2.7.13".to_string()),
                    dar_command: Some("dar -c base -R /src -N".to_string()),
                    slices: Vec::new(),
                },
                slices,
                catalogue,
                src,
                hashes,
            });
        }
    }

    // Positions: 0-3, a tenant envelope each, the operator envelope and its
    // backup, each tenant envelope's backup in the same order
    // (volume-format-v2.md §1.3), every unit's slices contiguously, the seal
    // marker last.
    let first_env = 4i64;
    let op_pos = first_env + tenants.len() as i64;
    let first_backup = op_pos + 2;
    let mut next = first_backup + tenants.len() as i64;
    let mut slice_cipher: Vec<(i64, Vec<u8>)> = Vec::new();
    for s in staged.iter_mut() {
        for (i, plain) in s.slices.iter().enumerate() {
            let cipher = stub_encrypt(&format!("{},op,esc", s.tenant), "", plain);
            s.unit.slices.push(ManifestSlice {
                number: i as i64 + 1,
                tape_position: next,
                size_bytes: plain.len() as i64,
                encrypted_bytes: cipher.len() as i64,
                sha256_plain: sha256_hex(plain),
                sha256_encrypted: sha256_hex(&cipher),
            });
            slice_cipher.push((next, cipher));
            next += 1;
        }
    }
    let seal_pos = next;
    let total_files = (seal_pos + 1) as i32;

    let envelope = |tenant: &str, recipients: &str, which: &[&Staged]| -> Vec<u8> {
        let units: Vec<ManifestUnit> = which.iter().map(|s| s.unit.clone()).collect();
        let manifest = generate_manifest_toml(LABEL, tenant, &units);
        let members: Vec<String> = which
            .iter()
            .filter(|s| s.catalogue.is_some())
            .map(|s| format!("{}_v{}.1.dar", &s.unit.uuid[..8], s.unit.snapshot_version))
            .collect();
        let recovery = generate_recovery_md(LABEL, tenant, &units, &members);
        let mut tar = tar::Builder::new(Vec::new());
        let mut add = |name: &str, bytes: &[u8]| {
            let mut h = tar::Header::new_gnu();
            h.set_size(bytes.len() as u64);
            h.set_mode(0o600);
            h.set_cksum();
            tar.append_data(&mut h, name, bytes).unwrap();
        };
        add("MANIFEST.toml", manifest.as_bytes());
        add("RECOVERY.md", recovery.as_bytes());
        for s in which {
            if let Some(cat) = &s.catalogue {
                add(
                    &format!(
                        "catalogs/{}_v{}.1.dar",
                        &s.unit.uuid[..8],
                        s.unit.snapshot_version
                    ),
                    cat,
                );
            }
        }
        if with_index {
            // The members follow `catalogs/` (sec 1.5). A tenant's `units.tsv`
            // is in the permuted column order with an unknown column; the
            // operator's is in sec 1.5's order, with `tenant`.
            let mut unit_rows = Vec::new();
            let mut path_rows = Vec::new();
            for s in which {
                let bytes: i64 = s.unit.slices.iter().map(|x| x.encrypted_bytes).sum();
                unit_rows.extend(idx_rows_for(
                    s.tenant,
                    &s.unit.name,
                    &s.unit.uuid,
                    s.unit.snapshot_version,
                    s.unit.slices.len() as i64,
                    bytes,
                ));
                path_rows.push((
                    s.unit.name.clone(),
                    s.unit.snapshot_version,
                    idx_paths_for(&s.unit.name, &s.src, &s.hashes),
                ));
            }
            if tenant == "operator" {
                let mut cols = UNITS_COLUMNS.to_vec();
                cols.push("zz_future");
                add("index/units.tsv", render_units(unit_rows, &cols).as_bytes());
            } else {
                add(
                    "index/units.tsv",
                    render_units(unit_rows, UNITS_COLUMNS_PERMUTED).as_bytes(),
                );
                add(
                    "index/paths.tsv",
                    render_paths(path_rows, hash_column).as_bytes(),
                );
            }
        }
        stub_encrypt(recipients, "", &tar.into_inner().unwrap())
    };

    let mut files: Vec<(i64, &'static str, Vec<u8>)> = Vec::new();
    let thunk = generate_id_thunk_v2(&IdThunkV2Params {
        label: LABEL,
        uuid: "a1b2c3d4-e5f6-4789-a123-456789abcdef",
        media_type: "LTO-6",
        tapectl_version: "1.1.0",
        nominal_capacity: 2_500_000_000_000,
        mam_capacity: 2_500_000_000_000,
        total_files,
        mam_manufacturer: "HP",
        mam_serial: "SERIAL01",
        mam_length: 846,
        mam_loads: 1,
        created_at: "2026-10-05T00:00:00Z",
        cartridge_identity_source: Some("mam"),
    });
    files.push((0, "id_thunk", thunk.into_bytes()));
    files.push((
        1,
        "system_guide",
        generate_system_guide_v2(LABEL, total_files).into_bytes(),
    ));
    files.push((
        2,
        "restore_sh",
        generate_restore_script_v2(LABEL, total_files).into_bytes(),
    ));
    for (i, t) in tenants.iter().enumerate() {
        let mine: Vec<&Staged> = staged.iter().filter(|s| s.tenant == t.name).collect();
        let env = envelope(t.name, &format!("{},op,esc", t.name), &mine);
        files.push((first_env + i as i64, "tenant_envelope", env.clone()));
        files.push((first_backup + i as i64, "tenant_envelope_backup", env));
    }
    let all: Vec<&Staged> = staged.iter().collect();
    let op_env = envelope("operator", "op,esc", &all);
    files.push((op_pos, "operator_envelope", op_env.clone()));
    files.push((op_pos + 1, "operator_envelope_backup", op_env));
    for (pos, cipher) in slice_cipher {
        files.push((pos, "data_slice", cipher));
    }

    let mut fi_files: Vec<FrontIndexFile> = Vec::new();
    for pos in 0..total_files as i64 {
        if pos == 3 {
            fi_files.push(FrontIndexFile {
                position: 3,
                type_label: "front_index",
                size_bytes: None,
                sha256_encrypted: None,
            });
        } else if pos == seal_pos {
            fi_files.push(FrontIndexFile {
                position: pos as i32,
                type_label: "seal_marker",
                size_bytes: None,
                sha256_encrypted: None,
            });
        } else {
            let (_, ty, bytes) = files.iter().find(|(p, _, _)| *p == pos).unwrap();
            fi_files.push(FrontIndexFile {
                position: pos as i32,
                type_label: ty,
                size_bytes: Some(bytes.len() as u64),
                sha256_encrypted: Some(sha256_hex(bytes)),
            });
        }
    }
    let fi = generate_front_index(LABEL, &fi_files).into_bytes();
    let fi_sha = sha256_hex(&fi);
    let mut seal_files = fi_files.clone();
    seal_files[3].size_bytes = Some(fi.len() as u64);
    seal_files[3].sha256_encrypted = Some(fi_sha.clone());
    let seal = generate_seal_marker(LABEL, total_files, &fi_sha, &seal_files).into_bytes();
    files.push((3, "front_index", fi));
    files.push((seal_pos, "seal_marker", seal));

    for (pos, _, bytes) in &files {
        std::fs::write(
            dir.join("tape").join(pos.to_string()),
            padded(bytes.clone()),
        )
        .unwrap();
    }

    let units = staged
        .iter()
        .map(|s| UnitOnTape {
            name: s.unit.name.clone(),
            src: s.src.clone(),
            positions: s.unit.slices.iter().map(|sl| sl.tape_position).collect(),
            sizes: s
                .unit
                .slices
                .iter()
                .map(|sl| sl.encrypted_bytes as usize)
                .collect(),
            hashes: s
                .unit
                .slices
                .iter()
                .map(|sl| sl.sha256_encrypted.clone())
                .collect(),
        })
        .collect();
    Template {
        dir,
        units,
        total_files,
    }
}

fn copy_dir(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).unwrap();
    for e in std::fs::read_dir(from).unwrap() {
        let e = e.unwrap();
        let dest = to.join(e.file_name());
        if e.file_type().unwrap().is_dir() {
            copy_dir(&e.path(), &dest);
        } else {
            std::fs::copy(e.path(), &dest).unwrap();
        }
    }
}

/// The host tools the script (and the stubs) may use.
const HOST_TOOLS: &[&str] = &[
    "bash",
    "sh",
    "env",
    "awk",
    "sed",
    "grep",
    "tr",
    "head",
    "tail",
    "cat",
    "sha256sum",
    "tar",
    "dar",
    "mkfifo",
    "mktemp",
    "rm",
    "rmdir",
    "df",
    "stat",
    "wc",
    "cut",
    "sort",
    "uniq",
    "od",
    "sleep",
    "dirname",
    "basename",
    "mkdir",
    "ls",
    "date",
    "tee",
    "truncate",
    "cp",
    "mv",
    "find",
    "readlink",
    "touch",
    "cmp",
    "timeout",
    "kill",
    "printf",
    "id",
    "xargs",
    "gpg",
    "gpgconf",
    "gpg-agent",
];

fn host_tool(name: &str) -> Option<PathBuf> {
    std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default())
        .map(|d| d.join(name))
        .find(|p| p.is_file())
}

/// One test's copy of the template: its own tape state, stubs and script.
struct Heir {
    _root: tempfile::TempDir,
    dir: PathBuf,
    path: String,
}

impl Heir {
    fn new() -> Self {
        Self::with_stubs(&[])
    }

    /// A fresh copy, with extra or replacement stubs on `PATH` (name, body).
    fn with_stubs(stubs: &[(&str, &str)]) -> Self {
        Self::of(template(), stubs)
    }

    /// A fresh copy of the lone-unit volume (`lone_template`).
    fn lone() -> Self {
        Self::of(lone_template(), &[])
    }

    fn of(t: &Template, stubs: &[(&str, &str)]) -> Self {
        let root = tempfile::Builder::new()
            .prefix("tapectl-heirsh-")
            .tempdir()
            .unwrap();
        let dir = root.path().to_path_buf();
        copy_dir(&t.dir.join("tape"), &dir.join("tape"));
        copy_dir(&t.dir.join("keys"), &dir.join("keys"));
        for sub in ["bin", "tmp", "home"] {
            std::fs::create_dir_all(dir.join(sub)).unwrap();
        }
        std::fs::write(dir.join("pos"), "0\n").unwrap();
        std::fs::write(dir.join("ops"), "").unwrap();
        std::fs::write(
            dir.join("RESTORE.sh"),
            generate_restore_script_v2(LABEL, t.total_files),
        )
        .unwrap();
        let bin = dir.join("bin");
        write_exec(&bin.join("mt"), FAKE_MT);
        write_exec(&bin.join("mt-st"), FAKE_MT);
        write_exec(&bin.join("dd"), FAKE_DD);
        write_exec(&bin.join("age"), STUB_AGE);
        for (name, body) in stubs {
            write_exec(&bin.join(name), body);
        }
        // PATH is the stubs plus a fixed list of the host's own tools, never
        // the host's whole PATH: a host with the real mt-st (this one has it)
        // would otherwise hand the script a real tape tool.
        let host = dir.join("hostbin");
        std::fs::create_dir_all(&host).unwrap();
        for tool in HOST_TOOLS {
            if let Some(p) = host_tool(tool) {
                std::os::unix::fs::symlink(p, host.join(tool)).unwrap();
            }
        }
        let path = format!("{}:{}", bin.display(), host.display());
        Heir {
            _root: root,
            dir,
            path,
        }
    }

    /// Take a stub off `PATH` (the host's own copy is never on it).
    fn without(self, name: &str) -> Self {
        std::fs::remove_file(self.dir.join("bin").join(name)).unwrap();
        self
    }

    /// Take one of the host's own tools off `PATH`, as a minimal system
    /// would not have it (a tool that is a stub goes with `without`).
    fn without_host(self, name: &str) -> Self {
        std::fs::remove_file(self.dir.join("hostbin").join(name)).unwrap();
        self
    }

    fn key(&self, name: &str) -> String {
        self.dir
            .join("keys")
            .join(format!("{name}.key"))
            .display()
            .to_string()
    }

    fn sub(&self, name: &str) -> String {
        self.dir.join(name).display().to_string()
    }

    /// Run the script with `args`; returns (exit code, stdout+stderr).
    fn run(&self, args: &[&str]) -> (i32, String) {
        self.run_env(args, &[])
    }

    fn run_env(&self, args: &[&str], env: &[(&str, &str)]) -> (i32, String) {
        self.run_input(args, env, "")
    }

    /// Run the script with `input` on its standard input (the ENTERs a
    /// `--swap-tapes` run waits for).
    fn run_input(&self, args: &[&str], env: &[(&str, &str)], input: &str) -> (i32, String) {
        use std::io::Write;
        use std::process::Stdio;
        std::fs::write(self.dir.join("ops"), "").unwrap();
        // A second cartridge swapped in by an earlier run goes back first.
        let first = self.dir.join("tape.first");
        if first.exists() {
            std::fs::rename(self.dir.join("tape"), self.dir.join("tape2")).unwrap();
            std::fs::rename(first, self.dir.join("tape")).unwrap();
        }
        let _ = std::fs::remove_file(self.dir.join("setblk.count"));
        let mut cmd = Command::new("timeout");
        cmd.args(["-k", "10", "300", "bash"])
            .arg(self.dir.join("RESTORE.sh"))
            .args(args)
            .current_dir(&self.dir)
            .env("PATH", &self.path)
            .env("FAKE_TAPE", &self.dir)
            .env("TAPE_DEVICE", "/dev/nst-fake")
            .env("TMPDIR", self.dir.join("tmp"))
            .env("HOME", self.dir.join("home"))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        for (k, v) in env {
            cmd.env(k, v);
        }
        let mut child = cmd.spawn().expect("spawn RESTORE.sh");
        {
            let mut stdin = child.stdin.take().unwrap();
            let _ = stdin.write_all(input.as_bytes());
        }
        let o = child.wait_with_output().expect("wait for RESTORE.sh");
        let mut text = String::from_utf8_lossy(&o.stdout).to_string();
        text.push_str(&String::from_utf8_lossy(&o.stderr));
        let code = o.status.code().unwrap_or(-1);
        assert_ne!(code, 124, "RESTORE.sh {args:?} timed out (hung):\n{text}");
        (code, text)
    }

    /// Every `mt`/`dd` call of the last run, in order.
    fn ops(&self) -> Vec<String> {
        std::fs::read_to_string(self.dir.join("ops"))
            .unwrap()
            .lines()
            .map(str::to_string)
            .collect()
    }

    fn count(&self, op: &str) -> usize {
        self.ops().iter().filter(|l| l.as_str() == op).count()
    }

    fn fsfs(&self) -> usize {
        self.ops()
            .iter()
            .filter(|l| l.starts_with("mt fsf"))
            .count()
    }
}

fn unit(name: &str) -> UnitOnTape {
    template()
        .units
        .iter()
        .find(|u| u.name == name)
        .unwrap_or_else(|| panic!("no unit {name} in the template"))
        .clone()
}

fn same_tree(a: &Path, b: &Path) -> bool {
    Command::new("diff")
        .arg("-r")
        .arg(a)
        .arg(b)
        .status()
        .expect("diff")
        .success()
}

// ---- positive controls: the harness itself ----

/// The harness reads its own tape: --info says SEALED and --verify passes
/// every file. Without this, every count below could be a count of nothing.
#[test]
fn the_fake_tape_is_a_sealed_volume_the_script_can_read() {
    let h = Heir::new();
    let (code, text) = h.run(&["--info"]);
    assert_eq!(code, 0, "{text}");
    assert!(text.contains("Verdict: SEALED"), "{text}");
    assert!(
        text.contains(&format!("Tape identifies as: {LABEL}\n")),
        "{text}"
    );

    let (code, text) = h.run(&["--verify"]);
    assert_eq!(code, 0, "{text}");
    assert!(
        text.contains("VERIFY: PASS — every file matches the front index."),
        "{text}"
    );
    let passes = text.lines().filter(|l| l.starts_with("PASS  file")).count();
    // Every file but the seal marker (no hash anywhere) is checked; File 3
    // through the seal binding.
    assert_eq!(passes, template().total_files as usize - 1, "{text}");
    assert!(
        h.ops().iter().any(|l| l.starts_with("dd ")),
        "the tape was read"
    );
}

/// A unit restores from the fake tape through the real dar, byte for byte.
#[test]
fn a_unit_restores_from_the_fake_tape_byte_for_byte() {
    let h = Heir::new();
    let photos = unit("photos/2019");
    assert!(
        photos.positions.len() >= 4,
        "{photos:?}: want a multi-slice unit"
    );
    let dest = h.sub("restored");
    let (code, text) = h.run(&[
        "--restore",
        "--key",
        &h.key("alice"),
        "--unit",
        "photos/2019",
        "--to",
        &dest,
    ]);
    assert_eq!(code, 0, "{text}");
    assert!(text.contains("RESTORE COMPLETE"), "{text}");
    assert!(text.contains("opened with key "), "{text}");
    assert!(same_tree(&photos.src, Path::new(&dest)), "{text}");
}

// ---- #396: forward-only tape navigation ----

/// The positions of every `dd` of the last run, in order.
fn reads(h: &Heir) -> Vec<i64> {
    h.ops()
        .iter()
        .filter_map(|l| l.strip_prefix("dd "))
        .filter_map(|p| p.parse().ok())
        .collect()
}

/// A K-slice restore rewinds once, at the start, and then only moves
/// forward: it used to rewind and space from BOT before every file it read.
#[test]
fn a_multi_slice_restore_rewinds_once_and_only_spaces_forward() {
    let h = Heir::new();
    let photos = unit("photos/2019");
    let k = photos.positions.len();
    let dest = h.sub("restored");
    let (code, text) = h.run(&[
        "--restore",
        "--key",
        &h.key("alice"),
        "--unit",
        "photos/2019",
        "--to",
        &dest,
    ]);
    assert_eq!(code, 0, "{text}");
    assert!(same_tree(&photos.src, Path::new(&dest)), "{text}");
    let ops = h.ops();
    assert_eq!(
        h.count("mt rewind"),
        1,
        "one rewind for the whole restore:\n{ops:#?}"
    );
    // File 0 -> File 3 and envelope -> first slice are the only gaps; the
    // slices themselves are contiguous, so K-1 is a generous ceiling.
    assert!(
        h.fsfs() <= 2 + k.saturating_sub(1),
        "forward spaces: {} for {k} slices:\n{ops:#?}",
        h.fsfs()
    );
    // Every slice read exactly once, in ascending order.
    let slices: Vec<i64> = reads(&h)
        .into_iter()
        .filter(|p| photos.positions.contains(p))
        .collect();
    assert_eq!(slices, photos.positions, "{ops:#?}");
}

/// --verify walks the tape once, ascending: one rewind, every file read
/// once, and the seal marker read last (#412 item 8), not before the walk.
#[test]
fn verify_rewinds_once_and_reads_the_seal_marker_last() {
    let h = Heir::new();
    let (code, text) = h.run(&["--verify"]);
    assert_eq!(code, 0, "{text}");
    let ops = h.ops();
    assert_eq!(h.count("mt rewind"), 1, "{ops:#?}");
    let got = reads(&h);
    let mut sorted = got.clone();
    sorted.sort();
    sorted.dedup();
    assert_eq!(got, sorted, "each file once, ascending:\n{ops:#?}");
    assert_eq!(
        got.last().copied(),
        Some(template().total_files as i64 - 1),
        "the seal marker is read last:\n{ops:#?}"
    );
}

/// --info and --find-envelope read ascending from one rewind too.
#[test]
fn info_and_find_envelope_rewind_once() {
    let h = Heir::new();
    let (code, text) = h.run(&["--info"]);
    assert_eq!(code, 0, "{text}");
    assert_eq!(h.count("mt rewind"), 1, "{:#?}", h.ops());
    let (code, text) = h.run(&["--find-envelope", "--key", &h.key("alice")]);
    assert_eq!(code, 0, "{text}");
    assert_eq!(h.count("mt rewind"), 1, "{:#?}", h.ops());
}

/// Any doubt is a rewind: after a read that failed, the next read starts
/// from BOT again rather than trusting where the head might be.
#[test]
fn a_failed_read_forces_a_rewind_before_the_next_one() {
    let h = Heir::new();
    let seal = template().total_files as i64 - 1;
    let (code, text) = h.run_env(&["--verify"], &[("FAKE_DD_FAIL", "5")]);
    assert_ne!(code, 0, "{text}");
    assert!(text.contains("FAIL  file   5"), "{text}");
    let ops = h.ops();
    let failed = ops
        .iter()
        .position(|l| l == "dd 5")
        .expect("file 5 was read");
    let next_read = ops[failed + 1..]
        .iter()
        .position(|l| l.starts_with("dd "))
        .map(|i| i + failed + 1)
        .expect("the walk went on past file 5");
    assert!(
        ops[failed + 1..next_read].iter().any(|l| l == "mt rewind"),
        "the read after a failure must rewind first:\n{ops:#?}"
    );
    assert!(
        reads(&h).contains(&seal),
        "the walk reached the seal:\n{ops:#?}"
    );
}

/// The cursor is checked against `mt status` before it is trusted (as
/// tapectl checks MTIOCGET since 1.0.5): when the drive says the head is
/// somewhere else, the script rewinds rather than read the wrong file.
#[test]
fn a_cursor_mt_status_disagrees_with_is_not_trusted() {
    let h = Heir::new();
    let (code, text) = h.run_env(&["--verify"], &[("FAKE_MT_STATUS_LIE", "1")]);
    assert!(
        h.count("mt rewind") > 1,
        "a disagreeing status must force a rewind:\n{text}\n{:#?}",
        h.ops()
    );
    // Rewinding instead of trusting the cursor still reads the right files.
    assert_eq!(code, 0, "{text}");
    assert!(text.contains("VERIFY: PASS"), "{text}");
    assert!(
        h.ops().iter().any(|l| l == "mt status"),
        "the cursor is checked against mt status"
    );
}

// ---- #412 item 5: which mt, and dd's own words ----

/// Stock Debian and Ubuntu `mt` is GNU cpio's, which has no `setblk`. The
/// script used to blame the device ("is it a tape device?"); now it names
/// the tool, reads on in the drive's own mode with a warning, and still
/// navigates with GNU mt's `status` wording.
#[test]
fn gnu_mt_is_named_and_the_script_reads_on_with_a_warning() {
    let h = Heir::new().without("mt-st");
    let (code, text) = h.run_env(&["--verify"], &[("FAKE_MT_GNU", "1")]);
    assert_eq!(code, 0, "{text}");
    assert!(text.contains("is not mt-st (mt (GNU cpio) 2.15)"), "{text}");
    assert!(text.contains("install mt-st"), "{text}");
    assert!(!text.contains("a tape device?"), "{text}");
    assert!(text.contains("VERIFY: PASS"), "{text}");
    assert_eq!(
        h.count("mt rewind"),
        1,
        "GNU mt's status is parsed too:\n{:#?}",
        h.ops()
    );
}

/// When mt-st itself refuses setblk, the device is the problem, and the
/// message says so in mt-st's own words.
#[test]
fn mt_st_refusing_setblk_blames_the_device_in_its_own_words() {
    let failing = "#!/bin/sh\ncase \"$*\" in *--version*) echo 'mt-st v. 1.7'; exit 0 ;; esac\n\
                   case \"$*\" in *setblk*) echo '/dev/nst-fake: No medium found' >&2; exit 2 ;; esac\nexit 0\n";
    let h = Heir::with_stubs(&[("mt-st", failing)]);
    let (code, text) = h.run(&["--info"]);
    assert_ne!(code, 0, "{text}");
    assert!(
        text.contains("cannot set the block size on /dev/nst-fake: /dev/nst-fake: No medium found"),
        "{text}"
    );
    assert!(text.contains("mt-st itself refused"), "{text}");
}

/// A dd failure is reported in dd's words, not swallowed: the front index
/// cannot be read, the script falls back to the seal marker's copy, and the
/// note names the I/O error.
#[test]
fn a_failed_tape_read_keeps_dds_message() {
    let h = Heir::new();
    let (code, text) = h.run_env(&["--info"], &[("FAKE_DD_FAIL", "3")]);
    assert_eq!(code, 0, "{text}");
    assert!(
        text.contains(
            "NOTE: reading tape file 3 failed: dd: error reading '/dev/nst0': Input/output error"
        ),
        "{text}"
    );
    assert!(text.contains("RUNG-2"), "{text}");
}

// ---- #405 part 1: a re-run must not report success over stale files ----

/// A destination that already holds files is refused BEFORE the tape is
/// read: dar under -Q answers "no" to every overwrite and exits 0, so a
/// re-run over a half-finished restore used to print RESTORE COMPLETE over
/// truncated files.
#[test]
fn a_non_empty_destination_is_refused_before_the_tape_is_read() {
    let h = Heir::new();
    let dest = h.dir.join("restored");
    std::fs::create_dir_all(&dest).unwrap();
    std::fs::write(dest.join("file0.bin"), vec![7u8; 100]).unwrap();
    let (code, text) = h.run(&[
        "--restore",
        "--key",
        &h.key("alice"),
        "--unit",
        "photos/2019",
        "--to",
        dest.to_str().unwrap(),
    ]);
    assert_ne!(code, 0, "{text}");
    assert!(text.contains("is not empty"), "{text}");
    assert!(
        text.contains("--overwrite"),
        "the way out is named:\n{text}"
    );
    assert!(text.contains("Nothing has been read from tape"), "{text}");
    assert!(
        !h.ops().iter().any(|l| l.starts_with("dd ")),
        "refused before any tape read:\n{:#?}",
        h.ops()
    );
    assert_eq!(
        std::fs::read(dest.join("file0.bin")).unwrap().len(),
        100,
        "nothing was touched"
    );
}

/// --overwrite restores over a partial destination: the truncated file is
/// replaced with the archived one (dar's -w).
#[test]
fn overwrite_replaces_a_truncated_file_from_an_earlier_run() {
    let h = Heir::new();
    let photos = unit("photos/2019");
    let dest = h.dir.join("restored");
    std::fs::create_dir_all(&dest).unwrap();
    std::fs::write(dest.join("file0.bin"), vec![7u8; 100]).unwrap();
    let (code, text) = h.run(&[
        "--restore",
        "--key",
        &h.key("alice"),
        "--unit",
        "photos/2019",
        "--to",
        dest.to_str().unwrap(),
        "--overwrite",
    ]);
    assert_eq!(code, 0, "{text}");
    assert!(text.contains("RESTORE COMPLETE"), "{text}");
    assert!(same_tree(&photos.src, &dest), "{text}");
}

/// The backstop: if dar still declines to overwrite something (a file that
/// appeared in --to during the restore), its "not restored (user choice)"
/// line fails the restore instead of being reported as complete.
#[test]
fn a_file_dar_declined_to_overwrite_fails_the_restore() {
    // The real dar, then the line dar prints for a file it kept.
    let dar = "#!/bin/sh\n\"$FAKE_TAPE/hostbin/dar\" \"$@\" || exit $?\n\
               echo \"$FAKE_TAPE/restored/file0.bin not restored (user choice)\"\n";
    let h = Heir::with_stubs(&[("dar", dar)]);
    let (code, text) = h.run(&[
        "--restore",
        "--key",
        &h.key("alice"),
        "--unit",
        "photos/2019",
        "--to",
        &h.sub("restored"),
    ]);
    assert_ne!(code, 0, "{text}");
    assert!(text.contains("INCOMPLETE"), "{text}");
    assert!(text.contains("file0.bin"), "the casualty is named:\n{text}");
    assert!(!text.contains("RESTORE COMPLETE"), "{text}");
}

/// A scratch directory left by a killed restore may hold decrypted data: it
/// is named and refused, even with --overwrite, before the tape is read.
#[test]
fn a_leftover_scratch_directory_is_refused_by_name() {
    let h = Heir::new();
    let dest = h.dir.join("restored");
    let stale = dest.join(".tapectl-restore.AbC123");
    std::fs::create_dir_all(&stale).unwrap();
    let (code, text) = h.run(&[
        "--restore",
        "--key",
        &h.key("alice"),
        "--unit",
        "photos/2019",
        "--to",
        dest.to_str().unwrap(),
        "--overwrite",
    ]);
    assert_ne!(code, 0, "{text}");
    assert!(text.contains(".tapectl-restore.AbC123"), "{text}");
    assert!(text.contains("DECRYPTED"), "{text}");
    assert!(
        !h.ops().iter().any(|l| l.starts_with("dd ")),
        "{:#?}",
        h.ops()
    );
}

// ---- #412 item 9: dar -N, no darrc ----

/// A `~/.darrc` on the heir's machine must not change what is restored. dar
/// reads `$HOME/.darrc` and `/etc/darrc` unless given -N; this one excludes
/// every .bin file on extract, so without -N the restore would silently be
/// missing most of the unit.
#[test]
fn a_darrc_on_the_heirs_machine_does_not_change_the_restore() {
    let h = Heir::new();
    std::fs::write(
        h.dir.join("home").join(".darrc"),
        "extract:\n-X \"*.bin\"\n",
    )
    .unwrap();
    let photos = unit("photos/2019");
    let dest = h.sub("restored");
    let (code, text) = h.run(&[
        "--restore",
        "--key",
        &h.key("alice"),
        "--unit",
        "photos/2019",
        "--to",
        &dest,
    ]);
    assert_eq!(code, 0, "{text}");
    assert!(
        same_tree(&photos.src, Path::new(&dest)),
        "a .darrc changed the restore:\n{text}"
    );
}

// ---- #412 items 6 and 7: which envelope opened, and what is in it ----

/// Overwrite tape file `pos`'s stub-age header in place (same length), as a
/// damaged envelope: the key still matches, the payload does not decrypt.
fn damage_envelope(h: &Heir, pos: i64) {
    let f = h.dir.join("tape").join(pos.to_string());
    let mut bytes = std::fs::read(&f).unwrap();
    let hdr = String::from_utf8_lossy(&bytes[..32]).trim_end().to_string();
    let mut new = format!("{hdr} BAD");
    while new.len() < 32 {
        new.push(' ');
    }
    bytes[..32].copy_from_slice(new.as_bytes());
    std::fs::write(&f, bytes).unwrap();
}

/// An escrow (or operator) key opens every envelope. --find-envelope used to
/// stop at the first one, a tenant's, and show only that tenant's units;
/// now it reads every envelope and lists every unit it can see, as a table.
#[test]
fn find_envelope_with_the_escrow_key_lists_every_unit() {
    let h = Heir::new();
    let (code, text) = h.run(&["--find-envelope", "--key", &h.key("esc")]);
    assert_eq!(code, 0, "{text}");
    for u in ["photos/2019", "docs", "ledgers"] {
        assert!(
            text.lines().any(|l| l.trim_start().starts_with(u)),
            "unit {u} must be listed:\n{text}"
        );
    }
    assert!(text.contains("Unit"), "a table header:\n{text}");
    assert!(
        text.contains("operator"),
        "the operator envelope is named:\n{text}"
    );
    // The operator envelope's backup holds the same bytes: not read twice.
    assert_eq!(h.count("mt rewind"), 1, "{:#?}", h.ops());
}

/// A tenant's own key: one envelope, its units, and its MANIFEST/RECOVERY
/// as before.
#[test]
fn find_envelope_with_a_tenant_key_shows_that_tenants_envelope() {
    let h = Heir::new();
    let (code, text) = h.run(&["--find-envelope", "--key", &h.key("alice")]);
    assert_eq!(code, 0, "{text}");
    assert!(text.contains("opened with key"), "{text}");
    assert!(text.contains("--- MANIFEST.toml ---"), "{text}");
    assert!(text.contains("--- RECOVERY.md ---"), "{text}");
    assert!(
        text.contains("photos/2019") && text.contains("docs"),
        "{text}"
    );
    assert!(
        !text.contains("ledgers"),
        "bob's unit is not alice's:\n{text}"
    );
}

/// The tape position of the backup of the tenant envelope at `primary`:
/// past the three tenants' primaries and the operator pair
/// (`volume-format-v2.md` §1.3). The template has three tenants.
fn backup_of(primary: i64) -> i64 {
    primary + 3 + 2
}

/// A damaged envelope is not a wrong key. The key matched (age got past the
/// header) but the payload did not decrypt: the script says DAMAGED, and the
/// headline no longer tells the heir to try their OTHER keys. Both copies
/// are damaged here: with one good copy left, the backup is used instead
/// (the tests below).
#[test]
fn a_damaged_envelope_is_reported_as_damage_not_as_a_wrong_key() {
    let h = Heir::new();
    damage_envelope(&h, 4); // alice's tenant envelope
    damage_envelope(&h, backup_of(4)); // and its backup
    let (code, text) = h.run(&["--find-envelope", "--key", &h.key("alice")]);
    assert_ne!(code, 0, "{text}");
    assert!(text.contains("DAMAGED"), "{text}");
    assert!(
        text.contains("does not match the front index"),
        "the keyless hash check names it before any key is tried:\n{text}"
    );
    assert!(
        text.contains("failed to decrypt and authenticate payload chunk"),
        "age's own words:\n{text}"
    );
    assert!(
        !text.contains("try your OTHER keys"),
        "damage is not a key problem:\n{text}"
    );
    assert!(
        text.contains("a key matched, so this is not a wrong key"),
        "a key did match here, and the script says so:\n{text}"
    );

    // Control: a key that matches nothing still gets the rotation hint.
    let h = Heir::new();
    let (code, text) = h.run(&["--find-envelope", "--key", &h.key("stranger")]);
    assert_ne!(code, 0, "{text}");
    assert!(text.contains("try your OTHER keys"), "{text}");
    assert!(!text.contains("DAMAGED"), "{text}");
}

// ---- #418 item 2: every tenant envelope has a backup copy ----

/// The template's own layout: alice's envelope at 4 and its backup after the
/// operator pair, the same bytes under the same front-index hash. Without
/// this, the fallback tests below could pass against a tape with no backup.
#[test]
fn each_tenant_envelope_has_a_byte_identical_backup_after_the_operator_pair() {
    let h = Heir::new();
    let tape = h.dir.join("tape");
    for primary in [4i64, 5, 6] {
        let a = std::fs::read(tape.join(primary.to_string())).unwrap();
        let b = std::fs::read(tape.join(backup_of(primary).to_string())).unwrap();
        assert_eq!(a, b, "file {} must copy file {primary}", backup_of(primary));
    }
    let (code, text) = h.run(&["--info"]);
    assert_eq!(code, 0, "{text}");
    assert!(text.contains("tenant_envelope_backup"), "{text}");
}

/// With the primary readable, the backup is never read: a restore with the
/// tenant's key reads alice's envelope and goes straight to the slices.
#[test]
fn a_backup_is_not_read_when_the_primary_opens() {
    let h = Heir::new();
    let docs = unit("docs");
    let dest = h.sub("restored");
    let (code, text) = h.run(&[
        "--restore",
        "--key",
        &h.key("alice"),
        "--unit",
        "docs",
        "--to",
        &dest,
    ]);
    assert_eq!(code, 0, "{text}");
    assert!(same_tree(&docs.src, Path::new(&dest)), "{text}");
    assert!(
        !reads(&h).contains(&backup_of(4)),
        "the backup was read although the primary opened:\n{:#?}",
        h.ops()
    );
    // And an escrow key, which opens every envelope, reads no backup either.
    let (code, text) = h.run(&["--find-envelope", "--key", &h.key("esc")]);
    assert_eq!(code, 0, "{text}");
    let r = reads(&h);
    for primary in [4i64, 5, 6] {
        assert!(
            !r.contains(&backup_of(primary)),
            "backup of {primary} read:\n{:#?}",
            h.ops()
        );
    }
}

/// A damaged primary: the restore turns to the backup further on, says so,
/// and restores byte for byte, still reading forward from one rewind.
#[test]
fn a_damaged_tenant_envelope_restores_from_its_backup() {
    let h = Heir::new();
    damage_envelope(&h, 4);
    let photos = unit("photos/2019");
    let dest = h.sub("restored");
    let (code, text) = h.run(&[
        "--restore",
        "--key",
        &h.key("alice"),
        "--unit",
        "photos/2019",
        "--to",
        &dest,
    ]);
    assert_eq!(code, 0, "{text}");
    assert!(text.contains("DAMAGED"), "the damage is named:\n{text}");
    assert!(
        text.contains(&format!("File {} is a backup copy", backup_of(4))),
        "{text}"
    );
    assert!(same_tree(&photos.src, Path::new(&dest)), "{text}");
    assert_eq!(h.count("mt rewind"), 1, "{:#?}", h.ops());

    // --find-envelope shows the backup's contents the same way.
    let (code, text) = h.run(&["--find-envelope", "--key", &h.key("alice")]);
    assert_eq!(code, 0, "{text}");
    assert!(text.contains("--- MANIFEST.toml ---"), "{text}");
    assert!(
        text.contains(&format!("Decrypted envelope at file {}", backup_of(4))),
        "{text}"
    );
}

/// A primary that will not read at all (an I/O error, not bad bytes) no
/// longer ends the run: it is DAMAGED, and the backup is read instead.
#[test]
fn an_unreadable_tenant_envelope_restores_from_its_backup() {
    let h = Heir::new();
    let docs = unit("docs");
    let dest = h.sub("restored");
    let (code, text) = h.run_env(
        &[
            "--restore",
            "--key",
            &h.key("alice"),
            "--unit",
            "docs",
            "--to",
            &dest,
        ],
        &[("FAKE_DD_FAIL", "4")],
    );
    assert_eq!(code, 0, "{text}");
    assert!(
        text.contains("NOTE: reading tape file 4 failed"),
        "dd's words are kept:\n{text}"
    );
    assert!(
        text.contains("the envelope at file 4 could not be read from the tape"),
        "{text}"
    );
    assert!(same_tree(&docs.src, Path::new(&dest)), "{text}");
    assert!(reads(&h).contains(&backup_of(4)), "{:#?}", h.ops());
}

// ---- #412 item 1: streaming restore; item 3: --all and repeated --unit ----

/// Rewrite the stub-age recipient list of tape file `pos` in place, keeping
/// its 32-byte header length, as if the slice had been sealed to `to`.
fn reseal(h: &Heir, pos: i64, to: &str) {
    let f = h.dir.join("tape").join(pos.to_string());
    let mut bytes = std::fs::read(&f).unwrap();
    let mut new = format!("STUBAGE:{to}");
    while new.len() < 32 {
        new.push(' ');
    }
    bytes[..32].copy_from_slice(new.as_bytes());
    std::fs::write(&f, bytes).unwrap();
}

/// A dar that records how it was called and whether every slice it was
/// handed is a FIFO, then runs the real one.
const DAR_SPY: &str = "#!/bin/sh\necho \"$*\" >>\"$FAKE_TAPE/dar.args\"\n\
    for a in \"$@\"; do case \"$a\" in */restore) for f in \"$a\".*.dar; do \
    if [ -p \"$f\" ]; then echo fifo >>\"$FAKE_TAPE/dar.slices\"; \
    else echo file >>\"$FAKE_TAPE/dar.slices\"; fi; done ;; esac; done\n\
    exec \"$FAKE_TAPE/hostbin/dar\" \"$@\"\n";

/// The slices go from the tape through age straight into dar: dar reads
/// FIFOs with --sequential-read, nothing decrypted is written to disk, and
/// the space check asks only for the unit's own size (it asked for about
/// twice that, plus a slice, which refused a 470 GiB unit on a 1 TB disk).
#[test]
fn a_restore_streams_slices_into_dar_without_decrypting_to_disk() {
    let h = Heir::with_stubs(&[("dar", DAR_SPY)]);
    let photos = unit("photos/2019");
    let dest = h.sub("restored");
    let (code, text) = h.run(&[
        "--restore",
        "--key",
        &h.key("alice"),
        "--unit",
        "photos/2019",
        "--to",
        &dest,
    ]);
    assert_eq!(code, 0, "{text}");
    assert!(same_tree(&photos.src, Path::new(&dest)), "{text}");
    let args = std::fs::read_to_string(h.dir.join("dar.args")).unwrap();
    assert!(args.contains("--sequential-read"), "{args}");
    assert!(args.contains("-N"), "{args}");
    assert!(
        args.contains("-A ") && args.contains("catalogs/11111111_v2"),
        "dar is given the envelope's catalogue: {args}"
    );
    let kinds = std::fs::read_to_string(h.dir.join("dar.slices")).unwrap();
    assert_eq!(
        kinds.lines().collect::<Vec<_>>(),
        vec!["fifo"; photos.positions.len()],
        "every slice handed to dar is a FIFO"
    );
    assert!(
        text.contains("(the files); "),
        "the space check asks only for the files:\n{text}"
    );
    assert!(!text.contains("decrypted slices"), "{text}");
}

/// A slice no key opens stops the restore with the rotation hint — and does
/// not hang dar waiting for a slice that will never come.
#[test]
fn a_slice_no_key_opens_stops_the_restore_with_the_rotation_hint() {
    let h = Heir::new();
    let photos = unit("photos/2019");
    reseal(&h, photos.positions[1], "carol,op,esc");
    let (code, text) = h.run(&[
        "--restore",
        "--key",
        &h.key("alice"),
        "--key",
        &h.key("bob"),
        "--unit",
        "photos/2019",
        "--to",
        &h.sub("restored"),
    ]);
    assert_ne!(code, 0, "{text}");
    assert!(text.contains("cannot decrypt slice 2"), "{text}");
    assert!(text.contains("none of the 2 key(s) decrypted it"), "{text}");
    assert!(text.contains("key rotation"), "{text}");
    assert!(
        text.contains("no identity matched any of the recipients"),
        "age's own words stay visible:\n{text}"
    );
    assert!(!text.contains("RESTORE COMPLETE"), "{text}");
}

/// Ciphertext that decrypts but does not match the front index (the stub age
/// does not authenticate, so a changed byte gets through it): the restore
/// fails on the slice's hash and says the files already written are suspect.
#[test]
fn a_slice_that_does_not_match_the_front_index_fails_the_restore() {
    let h = Heir::new();
    let photos = unit("photos/2019");
    let f = h.dir.join("tape").join(photos.positions[2].to_string());
    let mut bytes = std::fs::read(&f).unwrap();
    bytes[100] ^= 0xff;
    std::fs::write(&f, bytes).unwrap();
    let (code, text) = h.run(&[
        "--restore",
        "--key",
        &h.key("alice"),
        "--unit",
        "photos/2019",
        "--to",
        &h.sub("restored"),
    ]);
    assert_ne!(code, 0, "{text}");
    assert!(text.contains("slice 3 checksum MISMATCH"), "{text}");
    assert!(!text.contains("RESTORE COMPLETE"), "{text}");
}

/// dar dying part way (here, before it reads anything) ends the restore
/// with dar's own words, instead of the tape side waiting forever to hand
/// it the next slice.
#[test]
fn dar_dying_mid_restore_ends_it_instead_of_hanging() {
    let dar = "#!/bin/sh\necho 'dar: simulated failure' >&2\nexit 2\n";
    let h = Heir::with_stubs(&[("dar", dar)]);
    let (code, text) = h.run(&[
        "--restore",
        "--key",
        &h.key("alice"),
        "--unit",
        "photos/2019",
        "--to",
        &h.sub("restored"),
    ]);
    assert_ne!(code, 0, "{text}");
    assert!(text.contains("dar: simulated failure"), "{text}");
    assert!(text.contains("dar extraction failed"), "{text}");
}

/// A tape read failing mid-unit ends the restore, names the file, and does
/// not leave dar waiting.
#[test]
fn a_tape_read_failing_mid_unit_ends_the_restore() {
    let h = Heir::new();
    let photos = unit("photos/2019");
    let bad = photos.positions[1].to_string();
    let (code, text) = h.run_env(
        &[
            "--restore",
            "--key",
            &h.key("alice"),
            "--unit",
            "photos/2019",
            "--to",
            &h.sub("restored"),
        ],
        &[("FAKE_DD_FAIL", &bad)],
    );
    assert_ne!(code, 0, "{text}");
    assert!(
        text.contains(&format!("reading tape file {bad} failed")),
        "{text}"
    );
    assert!(text.contains("Input/output error"), "{text}");
}

/// --all restores every unit of the envelope in one pass from one rewind,
/// each into its own directory under --to.
#[test]
fn all_restores_every_unit_in_one_ascending_pass() {
    let h = Heir::new();
    let dest = h.dir.join("restored");
    let (code, text) = h.run(&[
        "--restore",
        "--key",
        &h.key("alice"),
        "--all",
        "--to",
        dest.to_str().unwrap(),
    ]);
    assert_eq!(code, 0, "{text}");
    for u in ["photos/2019", "docs"] {
        assert!(same_tree(&unit(u).src, &dest.join(u)), "{u}:\n{text}");
    }
    assert!(!dest.join("ledgers").exists(), "bob's unit is not alice's");
    assert_eq!(h.count("mt rewind"), 1, "{:#?}", h.ops());
    let got = reads(&h);
    let mut sorted = got.clone();
    sorted.sort();
    assert_eq!(got, sorted, "one ascending pass:\n{:#?}", h.ops());
    assert!(
        text.contains("Disk space: needs about"),
        "one combined check:\n{text}"
    );
    assert_eq!(text.matches("Disk space: needs about").count(), 1, "{text}");
}

/// Repeated --unit restores exactly those units; with the escrow key the
/// envelope that lists them all (the operator's) is used.
#[test]
fn repeated_unit_restores_those_units_across_tenants_with_escrow() {
    let h = Heir::new();
    let dest = h.dir.join("restored");
    let (code, text) = h.run(&[
        "--restore",
        "--key",
        &h.key("esc"),
        "--unit",
        "ledgers",
        "--unit",
        "docs",
        "--to",
        dest.to_str().unwrap(),
    ]);
    assert_eq!(code, 0, "{text}");
    for u in ["ledgers", "docs"] {
        assert!(same_tree(&unit(u).src, &dest.join(u)), "{u}:\n{text}");
    }
    assert!(!dest.join("photos/2019").exists(), "{text}");
    assert_eq!(h.count("mt rewind"), 1, "{:#?}", h.ops());
}

// ---- #418 R1 / #412 item 2: the envelope's dar catalogues, read ----

/// --list prints a unit's files from the dar catalogue its envelope carries
/// (catalogs/<uuid8>_v<version>.1.dar), reading no data slice at all.
#[test]
fn list_shows_a_units_files_from_the_envelope_catalogue_without_reading_slices() {
    let h = Heir::new();
    let photos = unit("photos/2019");
    let (code, text) = h.run(&["--list", "--key", &h.key("alice"), "--unit", "photos/2019"]);
    assert_eq!(code, 0, "{text}");
    for f in ["file0.bin", "file4.bin", "sub/note.txt"] {
        assert!(text.contains(f), "{f} must be listed:\n{text}");
    }
    assert!(
        text.contains("catalogs/11111111_v2"),
        "names the catalogue:\n{text}"
    );
    let slices = reads(&h)
        .into_iter()
        .filter(|p| photos.positions.contains(p))
        .count();
    assert_eq!(slices, 0, "no data slice is read:\n{:#?}", h.ops());
}

/// --restore --path restores one file reading only the slices that hold it
/// (and the directories above it) plus the last, which carries dar's own
/// catalogue: the catalogue in the envelope says which (`-T slice`).
#[test]
fn path_restores_one_file_reading_only_its_slices_and_the_last() {
    let h = Heir::new();
    let photos = unit("photos/2019");
    let dest = h.dir.join("one");
    let (code, text) = h.run(&[
        "--restore",
        "--key",
        &h.key("alice"),
        "--unit",
        "photos/2019",
        "--path",
        "sub/note.txt",
        "--to",
        dest.to_str().unwrap(),
    ]);
    assert_eq!(code, 0, "{text}");
    assert_eq!(
        std::fs::read(dest.join("sub/note.txt")).unwrap(),
        std::fs::read(photos.src.join("sub/note.txt")).unwrap()
    );
    assert!(!dest.join("file0.bin").exists(), "only the path asked for");
    let read: Vec<i64> = reads(&h)
        .into_iter()
        .filter(|p| photos.positions.contains(p))
        .collect();
    assert!(
        read.len() < photos.positions.len(),
        "fewer slices than the whole unit: {read:?} of {:?}",
        photos.positions
    );
    assert_eq!(
        read.last(),
        photos.positions.last(),
        "the last slice is read: {read:?}"
    );
    assert_eq!(h.count("mt rewind"), 1, "{:#?}", h.ops());

    // A path the catalogue does not hold is refused before any slice is read.
    let h = Heir::new();
    let (code, text) = h.run(&[
        "--restore",
        "--key",
        &h.key("alice"),
        "--unit",
        "photos/2019",
        "--path",
        "no/such/file",
        "--to",
        &h.sub("none"),
    ]);
    assert_ne!(code, 0, "{text}");
    assert!(text.contains("no/such/file"), "{text}");
    assert!(text.contains("--list"), "points at --list:\n{text}");
    let slices = reads(&h)
        .into_iter()
        .filter(|p| photos.positions.contains(p))
        .count();
    assert_eq!(slices, 0, "{:#?}", h.ops());
}

// ---- forward compatibility (ADR-0012 amendment item 15; #384) ----

/// Rewrite a plaintext zone (File 0 or the seal marker) in place: `edit`
/// maps its NUL-stripped text to new text, which is padded again.
fn edit_text_file(h: &Heir, pos: i64, edit: impl Fn(&str) -> String) {
    let f = h.dir.join("tape").join(pos.to_string());
    let raw = std::fs::read(&f).unwrap();
    let text = String::from_utf8(raw.into_iter().filter(|b| *b != 0).collect()).unwrap();
    let new = edit(&text);
    assert_ne!(new, text, "the edit changed nothing");
    std::fs::write(&f, padded(new.into_bytes())).unwrap();
}

/// A 1.1.0 tape says what it requires, and requires nothing. This holds for
/// 1.2.0 too (ADR-0012 2026-10-16 item 73): the re-pin changes RESTORE.sh's
/// text and not what a reader must understand, so a 1.1.x script reads a
/// 1.2.0 tape and the other way round. The third place `requires` lives is
/// RESTORE.sh's own `KNOWN_FEATURES`, which stays empty.
#[test]
fn a_new_tape_states_an_empty_requires_in_file_0_and_the_seal() {
    let t = template();
    let f0 = std::fs::read_to_string(t.dir.join("tape/0")).unwrap();
    let seal = std::fs::read_to_string(t.dir.join(format!("tape/{}", t.total_files - 1))).unwrap();
    assert!(f0.contains("\nrequires = []\n"), "File 0");
    assert!(seal.contains("\nrequires = []\n"), "seal marker");
    let script = generate_restore_script_v2(LABEL, t.total_files);
    assert_eq!(
        script.matches("\nKNOWN_FEATURES=\"\"\n").count(),
        1,
        "RESTORE.sh knows no feature a tape could require"
    );
}

/// ADR-0012 2026-10-16 item 73, the other fixed point: the text that tells
/// an heir a tape is newer than the script says "tapectl 1.1.0" and keeps
/// saying it in 1.2.0; it moves only by hand when a new required feature
/// arrives (ADR-0012 2026-10-06 item 20). Pinned in the generated text and
/// in what the heir actually reads when the script refuses a tape.
#[test]
fn the_newer_tape_refusal_still_says_tapectl_1_1_0() {
    let script = generate_restore_script_v2(LABEL, template().total_files);
    assert_eq!(
        script
            .matches("This RESTORE.sh (layout v2, tapectl 1.1.0) cannot read it safely")
            .count(),
        1,
        "die_newer_needed's text"
    );
    assert_eq!(
        script.matches("tapectl 1.").count(),
        1,
        "no other tapectl version is named in the script"
    );
    let h = Heir::new();
    edit_text_file(&h, 0, |t| {
        t.replace("requires = []", "requires = [\"short-seal\"]")
    });
    let (code, text) = h.run(&["--info"]);
    assert_ne!(code, 0, "{text}");
    assert!(
        text.contains("(layout v2, tapectl 1.1.0) cannot read it safely"),
        "{text}"
    );
}

/// A tape that requires a feature this script does not know is refused,
/// naming the feature and the way to a script that does, before anything
/// else of it is interpreted. Never a misread.
#[test]
fn a_tape_requiring_an_unknown_feature_is_refused_by_name() {
    let h = Heir::new();
    edit_text_file(&h, 0, |t| {
        t.replace("requires = []", "requires = [\"short-seal\"]")
    });
    for mode in [vec!["--info"], vec!["--verify"]] {
        let (code, text) = h.run(&mode);
        assert_ne!(code, 0, "{mode:?}: {text}");
        assert!(text.contains("short-seal"), "{mode:?}: names it:\n{text}");
        assert!(text.contains("newer"), "{mode:?}: {text}");
        assert!(
            text.contains("fsf 2"),
            "{mode:?}: the way to the tape's own script:\n{text}"
        );
    }

    // The seal marker's requires is honoured the same way.
    let h = Heir::new();
    let seal = template().total_files as i64 - 1;
    edit_text_file(&h, seal, |t| {
        t.replace("requires = []", "requires = [\"short-seal\"]")
    });
    let (code, text) = h.run(&["--info"]);
    assert_ne!(code, 0, "{text}");
    assert!(text.contains("short-seal"), "{text}");
}

/// Keys the script does not know are ignored unless `requires` names them:
/// a future tape may add fields, and a 1.1.0 reader must still read it.
#[test]
fn unknown_keys_that_are_not_required_are_ignored() {
    let h = Heir::new();
    let seal = template().total_files as i64 - 1;
    edit_text_file(&h, 0, |t| {
        t.replace("requires = []", "requires = []\nfuture_key = \"x\"")
    });
    edit_text_file(&h, seal, |t| {
        t.replace("requires = []", "requires = []\nended_after = 7")
    });
    let (code, text) = h.run(&["--info"]);
    assert_eq!(code, 0, "{text}");
    assert!(text.contains("Verdict: SEALED"), "{text}");
    let (code, text) = h.run(&[
        "--restore",
        "--key",
        &h.key("alice"),
        "--unit",
        "docs",
        "--to",
        &h.sub("restored"),
    ]);
    assert_eq!(code, 0, "{text}");
}

/// The seal marker is found by spacing to the end of data and stepping back
/// one file; File 0's pointer is only a cross-check. A wrong pointer is
/// named, and the seal is still found.
#[test]
fn the_seal_is_found_at_end_of_data_when_file_0_points_elsewhere() {
    let h = Heir::new();
    let total = template().total_files;
    edit_text_file(&h, 0, |t| {
        t.replace(
            &format!("seal_marker = {}", total - 1),
            &format!("seal_marker = {}", total - 3),
        )
    });
    let (code, text) = h.run(&["--info"]);
    assert_eq!(code, 0, "{text}");
    assert!(text.contains("Verdict: SEALED"), "{text}");
    assert!(
        text.contains(&format!(
            "File 0 says the seal marker is file {}",
            total - 3
        )),
        "the disagreement is named:\n{text}"
    );
    assert!(h.ops().iter().any(|l| l == "mt eod"), "{:#?}", h.ops());

    // Control: an mt that cannot space to end of data falls back to File 0's
    // pointer, which on an intact tape finds the seal.
    let h = Heir::new();
    let (code, text) = h.run_env(&["--info"], &[("FAKE_MT_NO_EOD", "1")]);
    assert_eq!(code, 0, "{text}");
    assert!(text.contains("Verdict: SEALED"), "{text}");
}

/// An unsealed tape ends in a data slice, which can be 10 GiB: reading "the
/// last file" for the seal must not copy all of it into /tmp.
#[test]
fn an_unsealed_tape_is_not_read_whole_into_tmp_looking_for_a_seal() {
    let h = Heir::new();
    let seal = template().total_files as i64 - 1;
    std::fs::remove_file(h.dir.join("tape").join(seal.to_string())).unwrap();
    // Make the new last file (a slice) larger than the bound: 80 blocks.
    let last = h.dir.join("tape").join((seal - 1).to_string());
    let mut bytes = std::fs::read(&last).unwrap();
    bytes.resize(80 * 524_288, 0);
    std::fs::write(&last, bytes).unwrap();
    let (code, text) = h.run(&["--info"]);
    assert_eq!(code, 0, "{text}");
    assert!(text.contains("Verdict: UNSEALED"), "{text}");
}

/// With an operator or escrow key and no --unit, --list uses the envelope
/// that lists the most units (the operator envelope: all of them), as --all
/// does, not whichever tenant's envelope happens to open first.
#[test]
fn list_with_the_escrow_key_lists_every_unit() {
    let h = Heir::new();
    let (code, text) = h.run(&["--list", "--key", &h.key("esc")]);
    assert_eq!(code, 0, "{text}");
    for u in ["photos/2019", "docs", "ledgers"] {
        assert!(
            text.contains(&format!("=== {u}, snapshot version")),
            "{u}:\n{text}"
        );
    }
}

// ---- dar 2.7.13's sequential-read tail loss, and the fallbacks ----

/// dar 2.7.13 (Debian's and Ubuntu's) reading a sliced archive with
/// --sequential-read and no isolated catalogue cuts short a file that runs
/// into the last slice, for most slice sizes (the template's 100 KiB among
/// them), and then asks for a slice after the last. With the catalogue as -A
/// the same slices restore identically (measured; fixed in dar 2.7.21).
/// So the streaming restore gives dar the envelope's catalogue. This is the
/// installed dar, not a stub: the restore must come back byte for byte.
#[test]
fn a_streamed_restore_on_dar_2_7_13_keeps_the_tail_of_the_last_file() {
    let h = Heir::new();
    // `docs`, at the template's 100 KiB slices, is a layout dar 2.7.13 cuts
    // short without -A (photos/2019 happens not to be).
    let docs = unit("docs");
    let dest = h.sub("restored");
    let (code, text) = h.run(&[
        "--restore",
        "--key",
        &h.key("alice"),
        "--unit",
        "docs",
        "--to",
        &dest,
    ]);
    assert_eq!(code, 0, "{text}");
    assert!(same_tree(&docs.src, Path::new(&dest)), "{text}");
}

/// A re-staged snapshot's envelope can carry the catalogue of a different
/// dar run (#419), which dar refuses, fatally, before it writes anything.
/// The restore falls back: on a dar older than 2.7.21 (this one) to
/// decrypting the slices to disk and extracting directly, which needs no
/// catalogue. It says so, and restores the unit byte for byte.
#[test]
fn a_catalogue_dar_refuses_falls_back_to_slices_on_disk() {
    let h = Heir::with_stubs(&[("dar", DAR_SPY)]);
    let u = unit("restaged");
    let dest = h.sub("restored");
    let (code, text) = h.run(&[
        "--restore",
        "--key",
        &h.key("carol"),
        "--unit",
        "restaged",
        "--to",
        &dest,
    ]);
    assert_eq!(code, 0, "{text}");
    assert!(same_tree(&u.src, Path::new(&dest)), "{text}");
    assert!(text.contains("refused the envelope's catalogue"), "{text}");
    assert!(text.contains("decrypting the slices to disk"), "{text}");
    let args = std::fs::read_to_string(h.dir.join("dar.args")).unwrap();
    let calls: Vec<&str> = args.lines().filter(|l| l.starts_with("-x")).collect();
    assert_eq!(calls.len(), 2, "{args}");
    assert!(
        calls[0].contains("--sequential-read") && calls[0].contains("-A "),
        "{args}"
    );
    assert!(
        !calls[1].contains("--sequential-read"),
        "direct read: {args}"
    );
}

/// An envelope with no catalogue for the unit: dar 2.7.13 cannot stream it
/// safely, so the slices are decrypted to disk first, with the space check
/// that implies (the slices and the files).
#[test]
fn no_catalogue_on_an_old_dar_restores_from_slices_on_disk() {
    let h = Heir::with_stubs(&[("dar", DAR_SPY)]);
    let u = unit("nocat");
    let dest = h.sub("restored");
    let (code, text) = h.run(&[
        "--restore",
        "--key",
        &h.key("carol"),
        "--unit",
        "nocat",
        "--to",
        &dest,
    ]);
    assert_eq!(code, 0, "{text}");
    assert!(same_tree(&u.src, Path::new(&dest)), "{text}");
    assert!(text.contains("decrypting the slices to disk"), "{text}");
    assert!(text.contains("(slices read, then the files)"), "{text}");
    let args = std::fs::read_to_string(h.dir.join("dar.args")).unwrap();
    assert!(!args.contains("--sequential-read"), "{args}");
}

/// dar 2.7.21 and later read sequentially without a catalogue correctly, so
/// there the unit streams even with none. (The version is claimed by a
/// wrapper: only the choice is under test.)
#[test]
fn no_catalogue_on_dar_2_7_21_still_streams() {
    let dar =
        "#!/bin/sh\ncase \"$*\" in -V*) echo ' dar version 2.7.21, Copyright'; exit 0 ;; esac\n\
               echo \"$*\" >>\"$FAKE_TAPE/dar.args\"\nexec \"$FAKE_TAPE/hostbin/dar\" \"$@\"\n";
    let h = Heir::with_stubs(&[("dar", dar)]);
    let _ = h.run(&[
        "--restore",
        "--key",
        &h.key("carol"),
        "--unit",
        "nocat",
        "--to",
        &h.sub("restored"),
    ]);
    let args = std::fs::read_to_string(h.dir.join("dar.args")).unwrap();
    assert!(args.contains("--sequential-read"), "{args}");
    assert!(!args.contains("-A "), "{args}");
}

/// A dar built with GPG support aborts under -Q when no `gpg` is on PATH
/// ("INITIALIZATION FAILED FOR GPGME"). The script asks dar first and names
/// the cause and the fix, before reading the tape, instead of failing at the
/// extract after the slices were read.
#[test]
fn a_dar_that_needs_gpg_is_named_before_the_tape_is_read() {
    let dar = "#!/bin/sh\necho 'Aborting program. User refused to continue while asking: \
               INITIALIZATION FAILED FOR GPGME, missing gpg binary? Retry initializing without \
               gpgme support' >&2\nexit 4\n";
    let h = Heir::with_stubs(&[("dar", dar)]);
    let (code, text) = h.run(&[
        "--restore",
        "--key",
        &h.key("alice"),
        "--unit",
        "docs",
        "--to",
        &h.sub("restored"),
    ]);
    assert_ne!(code, 0, "{text}");
    assert!(text.contains("gpg"), "{text}");
    assert!(text.contains("gnupg"), "names the package:\n{text}");
    assert!(
        !h.ops().iter().any(|l| l.starts_with("dd ")),
        "{:#?}",
        h.ops()
    );
}

// ---- #449 comment 1: the script asks for English ----

/// The script matches dar's English words, and dar ships fr and sv
/// translations: whatever locale the heir's shell is in, the dar it runs must
/// be started with LC_ALL=C. A wrapper on PATH records the locale dar was
/// started under, then runs the real one.
#[test]
fn dar_is_started_in_the_c_locale_whatever_the_heirs_locale() {
    let real = host_tool("dar").expect("dar must be on PATH (issue #43)");
    let wrapper = format!(
        "#!/bin/sh\necho \"LC_ALL=${{LC_ALL-unset}} LANG=${{LANG-unset}}\" >>\"$FAKE_TAPE/dar-locale\"\nexec {} \"$@\"\n",
        real.display()
    );
    let h = Heir::with_stubs(&[("dar", &wrapper)]);
    let (code, text) = h.run_env(
        &[
            "--restore",
            "--key",
            &h.key("alice"),
            "--unit",
            "docs",
            "--to",
            &h.sub("restored"),
        ],
        &[
            ("LC_ALL", "fr_FR.UTF-8"),
            ("LANG", "fr_FR.UTF-8"),
            ("LANGUAGE", "fr"),
        ],
    );
    assert_eq!(code, 0, "{text}");
    let seen = std::fs::read_to_string(h.dir.join("dar-locale")).unwrap();
    assert!(
        seen.lines().count() >= 2 && seen.lines().all(|l| l.starts_with("LC_ALL=C ")),
        "every dar run (the version probe, the extraction) is under LC_ALL=C:\n{seen}"
    );
}

// ---- #449 item 2: each mode checks the tools it runs ----

/// A host that can read a tape but nothing more: no age, dar, tar, mkfifo or
/// tee. --info and --verify run on it, because they run none of those; every
/// mode that decrypts or restores is refused up front, naming the tool and
/// before the tape is touched.
#[test]
fn a_verify_only_host_runs_info_and_verify_and_is_refused_a_restore() {
    let bare = || {
        Heir::new()
            .without("age")
            .without_host("dar")
            .without_host("tar")
            .without_host("mkfifo")
            .without_host("tee")
    };

    let h = bare();
    let (code, text) = h.run(&["--info"]);
    assert_eq!(code, 0, "{text}");
    assert!(text.contains("Verdict: SEALED"), "{text}");
    let (code, text) = h.run(&["--verify"]);
    assert_eq!(code, 0, "{text}");
    assert!(text.contains("VERIFY: PASS"), "{text}");
    assert!(
        !text.contains("missing required tool"),
        "a verify-only host is not refused for tools --verify never runs:\n{text}"
    );

    // Positive control: the same host IS refused where the tool is needed.
    for (mode, tool) in [
        (vec!["--find-envelope", "--key", "KEY"], "age"),
        (vec!["--list", "--key", "KEY"], "age"),
        (vec!["--restore", "--key", "KEY", "--to", "OUT"], "age"),
    ] {
        let args: Vec<String> = mode
            .iter()
            .map(|a| match *a {
                "KEY" => h.key("alice"),
                "OUT" => h.sub("restored"),
                other => other.to_string(),
            })
            .collect();
        let args: Vec<&str> = args.iter().map(String::as_str).collect();
        let (code, text) = h.run(&args);
        assert_eq!(code, 1, "{mode:?}: {text}");
        assert!(
            text.contains(&format!("missing required tool: {tool}")),
            "{mode:?}: names the tool:\n{text}"
        );
        assert!(
            h.ops().is_empty(),
            "{mode:?}: refused before any tape motion or read:\n{:#?}",
            h.ops()
        );
    }
}

/// The modes widen one tool at a time: each is refused for exactly the tool
/// IT runs and no other.
#[test]
fn each_mode_is_refused_for_the_tools_it_runs_and_no_others() {
    let alice = |h: &Heir| h.key("alice");
    let dest = |h: &Heir| h.sub("restored");

    // No dar: --find-envelope runs (it decrypts and unpacks, it does not run
    // dar); --list and --restore need dar.
    let h = Heir::new().without_host("dar");
    let (code, text) = h.run(&["--find-envelope", "--key", &alice(&h)]);
    assert_eq!(code, 0, "{text}");
    assert!(text.contains("photos/2019"), "{text}");
    for args in [
        vec!["--list", "--key", &alice(&h)],
        vec![
            "--restore",
            "--key",
            &alice(&h),
            "--unit",
            "docs",
            "--to",
            &dest(&h),
        ],
    ] {
        let (code, text) = h.run(&args);
        assert_eq!(code, 1, "{args:?}: {text}");
        assert!(text.contains("missing required tool: dar"), "{text}");
        assert!(h.ops().is_empty(), "{:#?}", h.ops());
    }

    // No mkfifo: a whole-unit restore streams through FIFOs and is refused;
    // --path reads a few slices to disk and never makes one.
    let h = Heir::new().without_host("mkfifo");
    let (code, text) = h.run(&[
        "--restore",
        "--key",
        &alice(&h),
        "--unit",
        "docs",
        "--to",
        &dest(&h),
    ]);
    assert_eq!(code, 1, "{text}");
    assert!(text.contains("missing required tool: mkfifo"), "{text}");
    assert!(h.ops().is_empty(), "{:#?}", h.ops());
    let (code, text) = h.run(&[
        "--restore",
        "--key",
        &alice(&h),
        "--unit",
        "docs",
        "--path",
        "sub/note.txt",
        "--to",
        &h.sub("one-file"),
    ]);
    assert_eq!(code, 0, "--path needs no mkfifo:\n{text}");
    assert!(
        std::fs::read_to_string(h.dir.join("one-file/sub/note.txt")).is_ok(),
        "{text}"
    );

    // No tee: --path pipes dar's output through tee, so it is refused too;
    // --find-envelope and --list never use it.
    let h = Heir::new().without_host("tee");
    for args in [
        vec![
            "--restore",
            "--key",
            &alice(&h),
            "--unit",
            "docs",
            "--to",
            &dest(&h),
        ],
        vec![
            "--restore",
            "--key",
            &alice(&h),
            "--unit",
            "docs",
            "--path",
            "sub/note.txt",
            "--to",
            &dest(&h),
        ],
    ] {
        let (code, text) = h.run(&args);
        assert_eq!(code, 1, "{args:?}: {text}");
        assert!(text.contains("missing required tool: tee"), "{text}");
        assert!(h.ops().is_empty(), "{:#?}", h.ops());
    }
    let (code, text) = h.run(&["--list", "--key", &alice(&h), "--unit", "docs"]);
    assert_eq!(code, 0, "--list needs no tee:\n{text}");

    // No mt at all: every mode that reads the tape says so first. (A
    // control: it is the one tool every mode needs.)
    let h = Heir::new().without("mt").without("mt-st");
    for mode in ["--info", "--verify"] {
        let (code, text) = h.run(&[mode]);
        assert_eq!(code, 1, "{mode}: {text}");
        assert!(
            text.contains("missing required tool: mt (package mt-st)"),
            "{mode}: {text}"
        );
    }
}

// ---- #449 item 3: "a key matched" only when one did ----

/// An envelope that is DAMAGED because it will not read, or fails its hash,
/// was never opened by any key, so the script cannot say "a key matched, so
/// this is not a wrong key". The case behind the finding: a tenant's key, an
/// unreadable operator envelope (which that key could never have opened), and
/// a unit the tenant's own envelope does not list. The three counted cases
/// are the three places try_envelope calls an envelope DAMAGED.
#[test]
fn damage_is_not_blamed_on_a_matching_key_when_no_key_matched() {
    let wrong_key_claim = "a key matched, so this is not a wrong key";
    let unreadable_operator = |h: &Heir, env: &[(&str, &str)]| {
        h.run_env(
            &[
                "--restore",
                "--key",
                &h.key("alice"),
                "--unit",
                "ledgers", // bob's, not in alice's envelope
                "--to",
                &h.sub("restored"),
            ],
            env,
        )
    };

    // 1. The operator envelope (file 7) will not read. alice's key never
    //    could have opened it.
    let h = Heir::new();
    let (code, text) = unreadable_operator(&h, &[("FAKE_DD_FAIL", "7")]);
    assert_ne!(code, 0, "{text}");
    assert!(text.contains("DAMAGED"), "{text}");
    assert!(
        !text.contains(wrong_key_claim),
        "no key opened the unreadable envelope:\n{text}"
    );
    assert!(
        text.contains("no key opened"),
        "it says what is known instead:\n{text}"
    );
    assert!(
        text.contains("try them"),
        "a wrong key is still on the table, so the other keys are suggested:\n{text}"
    );

    // 2. The operator envelope reads but fails its front-index hash, and
    //    alice's key does not open it.
    let h = Heir::new();
    for pos in [7, 8] {
        let f = h.dir.join("tape").join(pos.to_string());
        let mut bytes = std::fs::read(&f).unwrap();
        bytes[100] ^= 0xff; // inside the ciphertext, past the stub header
        std::fs::write(&f, bytes).unwrap();
    }
    let (code, text) = unreadable_operator(&h, &[]);
    assert_ne!(code, 0, "{text}");
    assert!(text.contains("does not match the front index"), "{text}");
    assert!(!text.contains(wrong_key_claim), "{text}");
    assert!(text.contains("no key opened"), "{text}");

    // 3. Control: when a key DID match a damaged envelope, it still says so
    //    (alice's own envelope and its backup are damaged after the header).
    let h = Heir::new();
    damage_envelope(&h, 4);
    damage_envelope(&h, backup_of(4));
    let (code, text) = h.run(&["--find-envelope", "--key", &h.key("alice")]);
    assert_ne!(code, 0, "{text}");
    assert!(text.contains(wrong_key_claim), "{text}");
    assert!(!text.contains("no key opened"), "{text}");
}

// ---- #449 item 4: the work directory, described as it is, and counted ----

/// A `df` that reports `kib` KiB available on every filesystem, and logs
/// that it was asked among the tape operations (`ops`), so a test can say
/// WHEN the script measured.
fn df_with_free_kib(kib: u64) -> String {
    format!(
        "#!/bin/sh\necho df >>\"$FAKE_TAPE/ops\"\n\
         echo 'Filesystem 1024-blocks Used Available Capacity Mounted on'\n\
         echo 'fake 99999999 99999999 {kib} 99% /'\n"
    )
}

/// The work directory holds the decrypted envelopes, not "only small files".
/// The script says where it is and that TMPDIR moves it, in its own help and
/// where a full disk is reported; and nothing still calls it small.
#[test]
fn the_work_directory_is_described_as_it_is() {
    let script = generate_restore_script_v2(LABEL, template().total_files);
    assert!(!script.contains("holds only small files"), "the old claim");
    assert!(
        !script.contains("everything else this script reads is small"),
        "the old claim in the disk-full message"
    );
    let h = Heir::new();
    let (code, help) = h.run(&["--help"]);
    assert_eq!(code, 0, "{help}");
    assert!(help.contains("TMPDIR"), "--help names TMPDIR:\n{help}");
    assert!(
        help.contains("decrypted envelopes"),
        "--help says what the work directory holds:\n{help}"
    );

    // A full work directory is named as such, and TMPDIR is the way out.
    let h = Heir::with_stubs(&[(
        "dd",
        "#!/bin/sh\necho 'dd: error writing to /tmp/x: No space left on device' >&2\nexit 1\n",
    )]);
    let (code, text) = h.run(&["--find-envelope", "--key", &h.key("alice")]);
    assert_ne!(code, 0, "{text}");
    assert!(text.contains("OUT OF DISK SPACE"), "{text}");
    assert!(
        text.contains("TMPDIR") && text.contains("decrypted envelopes"),
        "the full disk is told where the envelopes went and how to move them:\n{text}"
    );
}

/// The work directory's own need is estimated from the front index (every
/// envelope is read whole into it, and each one a key opens is kept decrypted)
/// before the first envelope is read; a work directory that cannot hold them
/// is a WARNING naming TMPDIR, not a surprise mid-restore. --no-space-check
/// silences it, like the other space checks.
#[test]
fn a_work_directory_too_small_for_the_envelopes_is_warned_about_before_they_are_read() {
    // 1 KiB free is less than any envelope.
    let tiny = df_with_free_kib(1);
    let h = Heir::with_stubs(&[("df", &tiny)]);
    let (code, text) = h.run(&["--find-envelope", "--key", &h.key("esc")]);
    assert_eq!(code, 0, "only a warning:\n{text}");
    assert!(text.contains("WARNING"), "{text}");
    assert!(
        text.contains("work directory") && text.contains("TMPDIR"),
        "{text}"
    );
    // The measuring happens before the first envelope (file 4) is read.
    let ops = h.ops();
    let measured = ops.iter().position(|l| l == "df").expect("df was asked");
    let first_envelope = ops.iter().position(|l| l == "dd 4").expect("file 4 read");
    assert!(
        measured < first_envelope,
        "measured before the first envelope is read:\n{ops:#?}"
    );

    // Room enough: silence. (The positive control for the warning above.)
    let roomy = df_with_free_kib(1 << 30); // 1 TiB
    let h = Heir::with_stubs(&[("df", &roomy)]);
    let (code, text) = h.run(&["--find-envelope", "--key", &h.key("esc")]);
    assert_eq!(code, 0, "{text}");
    assert!(!text.contains("work directory"), "{text}");

    // --no-space-check skips it on --restore. A df this small would stop the
    // restore at its own check, so only the absence of the work-directory
    // warning is compared.
    let h = Heir::with_stubs(&[("df", &tiny)]);
    let (_, text) = h.run(&[
        "--restore",
        "--key",
        &h.key("alice"),
        "--unit",
        "docs",
        "--to",
        &h.sub("restored"),
        "--no-space-check",
    ]);
    assert!(!text.contains("work directory"), "{text}");
}

/// The destination check counts the slices' size on tape, which is
/// encrypted; a unit stored compressed restores larger. The script says so
/// where it counts and where it refuses, rather than presenting the figure as
/// exact.
#[test]
fn the_space_check_says_its_figure_is_the_encrypted_size() {
    let h = Heir::new();
    let (code, text) = h.run(&[
        "--restore",
        "--key",
        &h.key("alice"),
        "--unit",
        "docs",
        "--to",
        &h.sub("restored"),
    ]);
    assert_eq!(code, 0, "{text}");
    assert!(text.contains("Disk space: needs about"), "{text}");
    let note = "a compressed unit may need more";
    assert!(
        text.contains(note),
        "the pass line carries the caveat:\n{text}"
    );

    // Refused: the same caveat, in the FATAL.
    let small = df_with_free_kib(2);
    let h = Heir::with_stubs(&[("df", &small)]);
    let (code, text) = h.run(&[
        "--restore",
        "--key",
        &h.key("alice"),
        "--unit",
        "docs",
        "--to",
        &h.sub("restored"),
    ]);
    assert_eq!(code, 1, "{text}");
    assert!(text.contains("not enough disk space"), "{text}");
    let fatal = text.split("FATAL:").nth(1).expect("a FATAL");
    assert!(
        fatal.contains("encrypted") && fatal.contains(note),
        "the refusal says what the figure counts:\n{fatal}"
    );
}

// ---- #449 item 1: a lone-unit restore says what else the key opened ----

fn lone_unit(name: &str) -> UnitOnTape {
    lone_template()
        .units
        .iter()
        .find(|u| u.name == name)
        .unwrap_or_else(|| panic!("no unit {name} in the lone template"))
        .clone()
}

/// The sentence-level facts of the notice, found in the output.
const LONE_NOTE: &str = "only 'ledgers' was restored";

/// With no --unit and no --all, the first envelope the key opens decides,
/// and when it lists one unit that unit is restored (ratified; unchanged).
/// An escrow key opens two more envelopes, which list three more units: the
/// script says so BEFORE it says RESTORE COMPLETE, naming --list and --all,
/// so an heir does not stop believing the tape held one unit. Reading the
/// rest of the envelopes costs no rewind: they come before every slice.
#[test]
fn a_lone_unit_restore_says_what_else_the_key_opened() {
    let h = Heir::lone();
    // The harness reads: bob's envelope is first and lists one unit.
    let (code, text) = h.run(&["--find-envelope", "--key", &h.key("esc")]);
    assert_eq!(code, 0, "{text}");
    assert!(
        text.contains("ledgers") && text.contains("photos") && text.contains("notes"),
        "the escrow key opens all of it:\n{text}"
    );

    let dest = h.sub("restored");
    let (code, text) = h.run(&["--restore", "--key", &h.key("esc"), "--to", &dest]);
    assert_eq!(code, 0, "{text}");
    // The default is unchanged: the first envelope's one unit, alone, in --to.
    assert!(
        same_tree(&lone_unit("ledgers").src, Path::new(&dest)),
        "{text}"
    );
    let note = text
        .find(LONE_NOTE)
        .unwrap_or_else(|| panic!("no notice:\n{text}"));
    let complete = text.find("RESTORE COMPLETE").expect("RESTORE COMPLETE");
    assert!(note < complete, "the notice comes first:\n{text}");
    let notice = &text[note..complete];
    assert!(notice.contains("2 other envelope(s)"), "{notice}");
    assert!(notice.contains("3 other unit(s)"), "{notice}");
    assert!(
        notice.contains("--all") && notice.contains("--list"),
        "{notice}"
    );
    assert!(
        notice.contains("(4 units here)"),
        "the largest envelope lists the whole tape, 4 units:\n{notice}"
    );
    // Forward only: one rewind, every read in ascending order.
    assert_eq!(h.count("mt rewind"), 1, "{:#?}", h.ops());
    let got = reads(&h);
    let mut sorted = got.clone();
    sorted.sort();
    assert_eq!(got, sorted, "one ascending pass:\n{:#?}", h.ops());
    // The surveyed envelopes are not kept: only the one used stays decrypted.
    assert!(
        std::fs::read_dir(h.dir.join("tmp"))
            .unwrap()
            .next()
            .is_none(),
        "the work directory is removed at the end"
    );
}

/// No notice when there is nothing to say: --all, --unit, or a key that opens
/// only the envelope it used. And the same notice before the --path form's
/// own RESTORE COMPLETE.
#[test]
fn the_lone_unit_notice_is_only_where_it_applies() {
    let h = Heir::lone();
    let keep = |text: &str| text.contains(LONE_NOTE) || text.contains("other envelope(s)");

    // --unit: the user said what to restore.
    let (code, text) = h.run(&[
        "--restore",
        "--key",
        &h.key("esc"),
        "--unit",
        "ledgers",
        "--to",
        &h.sub("a"),
    ]);
    assert_eq!(code, 0, "{text}");
    assert!(!keep(&text), "{text}");

    // --all: it restores every unit of the largest envelope.
    let (code, text) = h.run(&[
        "--restore",
        "--key",
        &h.key("esc"),
        "--all",
        "--to",
        &h.sub("b"),
    ]);
    assert_eq!(code, 0, "{text}");
    assert!(!keep(&text), "{text}");
    assert!(text.contains("RESTORE COMPLETE"), "{text}");

    // bob's own key opens bob's envelope and nothing else.
    let (code, text) = h.run(&["--restore", "--key", &h.key("bob"), "--to", &h.sub("c")]);
    assert_eq!(code, 0, "{text}");
    assert!(!keep(&text), "{text}");
    assert!(
        same_tree(&lone_unit("ledgers").src, Path::new(&h.sub("c"))),
        "{text}"
    );

    // --path, with no unit named, takes the same lone unit and says the same.
    let (code, text) = h.run(&[
        "--restore",
        "--key",
        &h.key("esc"),
        "--path",
        "sub/note.txt",
        "--to",
        &h.sub("d"),
    ]);
    assert_eq!(code, 0, "{text}");
    let note = text
        .find(LONE_NOTE)
        .unwrap_or_else(|| panic!("no notice:\n{text}"));
    assert!(note < text.find("RESTORE COMPLETE").unwrap(), "{text}");
    assert!(text.contains("--all"), "{text}");
}

/// One tenant, one unit: the operator envelope opens as a second envelope and
/// lists the very unit just restored. The survey runs (the operator envelope
/// is read) and finds nothing to add, so there is no notice.
#[test]
fn an_other_envelope_that_lists_nothing_new_is_not_worth_a_notice() {
    let h = Heir::of(solo_template(), &[]);
    let dest = h.sub("restored");
    let (code, text) = h.run(&["--restore", "--key", &h.key("op"), "--to", &dest]);
    assert_eq!(code, 0, "{text}");
    assert!(
        same_tree(&solo_template().units[0].src, Path::new(&dest)),
        "{text}"
    );
    // Control: the operator envelope (file 5) WAS opened and looked at.
    assert!(reads(&h).contains(&5), "{:#?}", h.ops());
    assert!(
        !text.contains("other envelope(s)") && !text.contains("only 'ledgers' was restored"),
        "{text}"
    );
    assert!(text.contains("RESTORE COMPLETE"), "{text}");
}

/// Where the script refuses to guess (the first envelope lists several units)
/// it stops there: it does not read on through the other envelopes first.
#[test]
fn a_refusal_to_guess_reads_no_further_envelopes() {
    let h = Heir::new();
    let (code, text) = h.run(&["--restore", "--key", &h.key("esc"), "--to", &h.sub("x")]);
    assert_eq!(code, 1, "{text}");
    assert!(text.contains("multiple units found"), "{text}");
    let envelopes_read: Vec<i64> = reads(&h)
        .into_iter()
        .filter(|p| (4..=7).contains(p))
        .collect();
    assert_eq!(envelopes_read, vec![4], "{:#?}", h.ops());
}

/// The survey reads envelopes the restore did not need to read before, so it
/// must not be what fills a small work directory: where the largest remaining
/// envelope would not fit (read whole, and decrypted), it is skipped with a
/// line saying so, and the restore goes on as it always did.
#[test]
fn the_survey_is_skipped_where_the_work_directory_could_not_hold_it() {
    let tiny = df_with_free_kib(1);
    let h2 = Heir::of(lone_template(), &[("df", &tiny)]);
    let dest = h2.sub("restored");
    let (code, text) = h2.run(&[
        "--restore",
        "--key",
        &h2.key("esc"),
        "--to",
        &dest,
        "--no-space-check",
    ]);
    assert_eq!(code, 0, "{text}");
    assert!(
        same_tree(&lone_unit("ledgers").src, Path::new(&dest)),
        "the restore itself is unchanged:\n{text}"
    );
    assert!(
        text.contains("Not looking at the other envelopes"),
        "{text}"
    );
    assert!(!text.contains(LONE_NOTE), "{text}");
    let envelopes: Vec<i64> = reads(&h2)
        .into_iter()
        .filter(|p| (4..=7).contains(p))
        .collect();
    assert_eq!(
        envelopes,
        vec![4],
        "only the envelope used was read:\n{:#?}",
        h2.ops()
    );
}

// ---- 1.3.0: --where and --find, the cross-volume index (sec 1.5) ----

/// Whitespace-flattened, with the `>>>` that prefixes the script's notices
/// dropped, so a sentence the script wraps over several lines matches whole.
fn flat(s: &str) -> String {
    s.split_whitespace()
        .filter(|w| *w != ">>>")
        .collect::<Vec<_>>()
        .join(" ")
}

/// The lines of `text` from the first one containing `from` up to the next
/// blank line: one block of the printed answer.
fn block<'a>(text: &'a str, from: &str) -> Vec<&'a str> {
    text.lines()
        .skip_while(|l| !l.contains(from))
        .take_while(|l| !l.trim().is_empty())
        .collect()
}

/// The files read are strictly ascending and the tape was rewound once: the
/// forward-only rule every mode keeps (#396).
fn assert_forward_only(h: &Heir, what: &str) {
    let ops = h.ops();
    assert_eq!(h.count("mt rewind"), 1, "{what}: one rewind:\n{ops:#?}");
    let got = reads(h);
    let mut sorted = got.clone();
    sorted.sort();
    sorted.dedup();
    assert_eq!(got, sorted, "{what}: each file once, ascending:\n{ops:#?}");
}

/// A tenant key reads that tenant's index: every version of the unit, with
/// its slices and bytes, and every tape holding it, decoded from sec 1.5's
/// columns (found by name, whatever their order).
#[test]
fn where_prints_every_version_and_holder_of_a_unit_from_a_tenant_key() {
    let h = Heir::new();
    let (code, text) = h.run(&["--where", "photos/2019", "--key", &h.key("alice")]);
    assert_eq!(code, 0, "{text}");
    eprintln!("---- --where photos/2019 --key alice ----\n{text}----");
    // Which tape's index, and the staleness rule.
    assert!(
        text.contains("Index read from tape: HEIR01 (written "),
        "{text}"
    );
    let flat_text = flat(&text);
    assert!(
        flat_text.contains("as of the moment its own tape was written"),
        "{flat_text}"
    );
    assert!(
        flat_text.contains("The newest tape you hold has the most complete index"),
        "{flat_text}"
    );

    assert!(text.contains("Unit photos/2019 (tenant alice):"), "{text}");
    // Both versions, the older one held by the other tape only.
    let photos = unit("photos/2019");
    let n = photos.positions.len();
    assert!(text.contains("  Version 1: 3 slice(s), 2.9 MiB (3000000 bytes); unit uuid 11111111-2222-3333-4444-555555555555"), "{text}");
    assert!(
        text.contains(&format!("  Version 2: {n} slice(s), ")),
        "{text}"
    );
    let v2 = block(&text, "Version 2:");
    let v1 = block(&text, "Version 1:");
    // Version 1: one holder.
    assert_eq!(
        v1.iter().filter(|l| l.contains("    tape ")).count(),
        1,
        "{v1:#?}"
    );
    assert!(
        v1.iter()
            .any(|l| l.trim() == "tape OTHER02  (sealed 2026-09-01T10:00:00Z)  slices 1-3 of 3"),
        "{v1:#?}"
    );
    // Version 2: this tape, the sealed one, and the quarantined one, in
    // volume order, each with every field decoded.
    let holders: Vec<&str> = v2
        .iter()
        .copied()
        .filter(|l| l.starts_with("    tape "))
        .collect();
    assert_eq!(
        holders,
        [
            format!("    tape HEIR01  (this tape)  slices 1-{n} of {n}"),
            format!("    tape OTHER02  (sealed 2026-09-01T10:00:00Z)  slices 1-{n} of {n}"),
            format!(
                "    tape QUAR03  (QUARANTINED: a verify proved this medium bad, it may still read in part; sealed 2026-08-15T09:30:00Z)  slices 1-{n} of {n}"
            ),
        ],
        "{v2:#?}"
    );
    assert!(
        v2.contains(
            &format!(
                "        volume uuid {THIS_UUID}; cartridge serial SERIAL01; generation LTO-6"
            )
            .as_str()
        ),
        "{v2:#?}"
    );
    assert!(
        v2.contains(
            &format!(
                "        volume uuid {OTHER_UUID}; cartridge serial OTHERSER02; generation LTO-6"
            )
            .as_str()
        ),
        "{v2:#?}"
    );
    // An empty serial is "not known", not a blank.
    assert!(
        v2.contains(
            &format!(
                "        volume uuid {QUAR_UUID}; cartridge serial not known; generation LTO-5"
            )
            .as_str()
        ),
        "{v2:#?}"
    );
    // A tenant key lists that tenant's units only.
    assert!(!text.contains("ledgers"), "{text}");
    assert_forward_only(&h, "--where");
}

/// The columns are found by header name: alice's `units.tsv` has them reversed
/// with an unknown one in the middle; the operator's is in sec 1.5's order with
/// `tenant`. The same unit read through the two must render the same rows, or
/// "ignored" would be indistinguishable from "never read".
#[test]
fn columns_are_found_by_header_name_and_an_unknown_column_is_ignored() {
    let h = Heir::new();
    let (code, via_tenant) = h.run(&["--where", "photos/2019", "--key", &h.key("alice")]);
    assert_eq!(code, 0, "{via_tenant}");
    let (code, via_operator) = h.run(&["--where", "photos/2019", "--key", &h.key("op")]);
    assert_eq!(code, 0, "{via_operator}");
    let rows = |t: &str| -> Vec<String> {
        t.lines()
            .skip_while(|l| !l.starts_with("Unit photos/2019"))
            .map(str::to_string)
            .collect()
    };
    let (a, b) = (rows(&via_tenant), rows(&via_operator));
    assert!(a.len() > 8, "a positive control needs rows: {a:#?}");
    // Everything from the unit's heading down, but for the lines that say
    // which key read which kind of index.
    let strip = |v: &[String]| -> Vec<String> {
        v.iter()
            .filter(|l| !l.starts_with(">>>"))
            .cloned()
            .collect()
    };
    let (a, b) = (strip(&a), strip(&b));
    // The heading names the tenant either way; the holders are the same.
    assert_eq!(a, b, "tenant file vs operator file");
}

/// The escrow key (and the operator key) read the operator envelope's map of
/// every tenant's units, with a tenant column; another tenant's unit is found.
#[test]
fn where_with_the_escrow_key_reads_the_operator_map_of_every_tenant() {
    let h = Heir::new();
    let (code, text) = h.run(&["--where", "ledgers", "--key", &h.key("esc")]);
    assert_eq!(code, 0, "{text}");
    assert!(text.contains("Unit ledgers (tenant bob):"), "{text}");
    assert!(text.contains("  Version 3: "), "{text}");
    assert!(
        text.contains("    tape HEIR01  (this tape)  slices 1-"),
        "{text}"
    );
    // The map is read from the operator envelope, so the walk ends there:
    // nothing is read after it, and the tape is rewound once.
    assert_forward_only(&h, "--where with the escrow key");
    let last_read = reads(&h).into_iter().max().unwrap();
    assert!(
        last_read < template().units[0].positions[0],
        "no data slice was read: {:#?}",
        h.ops()
    );
}

/// A unit the index does not list is said not to be there, and the units it
/// does list are named; with a tenant key that is that tenant's, and the
/// operator or escrow key is named as the one that opens every tenant's.
#[test]
fn where_names_the_units_the_index_lists_when_it_finds_none() {
    let h = Heir::new();
    let (code, text) = h.run(&["--where", "ledgers", "--key", &h.key("alice")]);
    assert_eq!(code, 1, "{text}");
    assert!(
        text.contains("No unit named 'ledgers' is in this index."),
        "{text}"
    );
    let listed: Vec<&str> = text
        .lines()
        .filter(|l| l.starts_with("    ") && !l.trim().is_empty())
        .collect();
    assert_eq!(listed, ["    docs", "    photos/2019"], "{text}");
    assert!(
        text.contains("the operator or escrow key opens the"),
        "{text}"
    );
    assert!(
        !text.contains("nocat"),
        "another tenant's units stay unnamed: {text}"
    );
    // The escrow key has no such caveat.
    let (code, text) = h.run(&["--where", "nonesuch", "--key", &h.key("esc")]);
    assert_eq!(code, 1, "{text}");
    assert!(text.contains("    ledgers"), "{text}");
    assert!(!text.contains("opens that tenant's index only"), "{text}");
}

/// A tape written before the index existed says so plainly, and says what it
/// can still do; both modes exit 1.
#[test]
fn a_tape_without_an_index_says_so_for_where_and_find() {
    let h = Heir::of(solo_template(), &[]);
    for args in [
        vec!["--where", "ledgers", "--key", &h.key("bob")],
        vec!["--find", "file", "--key", &h.key("bob")],
        vec!["--where", "ledgers", "--key", &h.key("esc")],
    ] {
        let (code, text) = h.run(&args);
        assert_eq!(code, 1, "{args:?}: {text}");
        let t = flat(&text);
        assert!(t.contains("This tape carries no index."), "{t}");
        assert!(
            t.contains("Tapes written before version 1.3.0 have none"),
            "{t}"
        );
        assert!(t.contains("--find-envelope"), "{t}");
        assert!(t.contains("--list"), "{t}");
        assert!(!text.contains("No unit named"), "not a miss: {text}");
    }
}

/// `--find` prints each match as unit, version, kind, size and path, then the
/// tapes holding each version printed.
#[test]
fn find_prints_matching_paths_with_their_holders() {
    let h = Heir::new();
    let (code, text) = h.run(&["--find", "note.txt", "--key", &h.key("alice")]);
    assert_eq!(code, 0, "{text}");
    eprintln!("---- --find note.txt --key alice ----\n{text}----");
    assert!(text.contains("Index read from tape: HEIR01"), "{text}");
    assert!(text.contains("In the index of tenant alice:"), "{text}");
    // `sub/note.txt` is in both of alice's units.
    let matches: Vec<&str> = text
        .lines()
        .filter(|l| l.starts_with("  ") && l.contains(" bytes  "))
        .collect();
    assert_eq!(
        matches,
        [
            "  docs  v1  f  10 bytes  sub/note.txt".to_string(),
            "  photos/2019  v2  f  17 bytes  sub/note.txt".to_string(),
        ],
        "{text}"
    );
    // Each version printed is followed by its holders.
    let docs = block(&text, "Tapes holding docs version 1:");
    assert!(
        docs.iter().any(|l| l.contains("tape HEIR01  (this tape)")),
        "{docs:#?}"
    );
    assert!(
        docs.iter()
            .any(|l| l.contains("tape OTHER02  (sealed 2026-09-01T10:00:00Z)")),
        "{docs:#?}"
    );
    assert!(!docs.iter().any(|l| l.contains("QUAR03")), "{docs:#?}");
    let photos = block(&text, "Tapes holding photos/2019 version 2:");
    assert_eq!(
        photos.iter().filter(|l| l.starts_with("    tape ")).count(),
        3,
        "{photos:#?}"
    );
    // A tenant key finds its own paths only.
    let (code, text) = h.run(&["--find", "file0", "--key", &h.key("bob")]);
    assert_eq!(code, 0, "{text}");
    assert!(text.contains("In the index of tenant bob:"), "{text}");
    assert!(text.contains("  ledgers  v3  f  "), "{text}");
    assert!(!text.contains("photos/2019"), "{text}");
    assert_forward_only(&h, "--find");
}

/// A path may hold a TAB, an LF and a backslash, which the index writes as
/// `\t`, `\n` and `\\`. The text given is matched against the DECODED path, as
/// a literal substring; the row is printed on one line, with TAB and LF shown
/// as `\t` and `\n`.
#[test]
fn find_matches_the_decoded_path_and_prints_it_on_one_line() {
    let h = Heir::new();
    // A real TAB then `tab`, then a real LF: only the decoded path has them.
    let (code, text) = h.run(&["--find", "weird\ttab\nnew", "--key", &h.key("alice")]);
    assert_eq!(code, 0, "{text}");
    let shown: Vec<&str> = text
        .lines()
        .filter(|l| l.contains("weird") && l.contains(" bytes  "))
        .collect();
    assert_eq!(
        shown,
        ["  photos/2019  v2  f  11 bytes  weird\\ttab\\nnew\\back.txt"],
        "one line, TAB and LF made visible:\n{text}"
    );
    assert!(
        !text.contains("weird\t"),
        "no raw TAB in the output: {text:?}"
    );

    // The decoded backslash: a single one matches, the stored pair does not.
    let (code, text) = h.run(&["--find", "new\\back", "--key", &h.key("alice")]);
    assert_eq!(code, 0, "{text}");
    assert!(text.contains("weird\\ttab\\nnew\\back.txt"), "{text}");
    let (code, text) = h.run(&["--find", "new\\\\back", "--key", &h.key("alice")]);
    assert_eq!(code, 1, "the stored form is not the name: {text}");
    assert!(text.contains("No path in this index contains"), "{text}");

    // `lit\tera.txt` is a backslash and a `t`, never a TAB: a decoder that
    // chained its substitutions would turn the stored `\\t` into one.
    let (code, text) = h.run(&["--find", "lit\\tera", "--key", &h.key("alice")]);
    assert_eq!(code, 0, "{text}");
    assert!(
        text.contains("  photos/2019  v2  f  12 bytes  lit\\tera.txt"),
        "{text}"
    );
    let (code, text) = h.run(&["--find", "lit\tera", "--key", &h.key("alice")]);
    assert_eq!(code, 1, "a TAB is not a backslash and a t: {text}");

    // The text is literal, not a pattern.
    let (code, text) = h.run(&["--find", "file.", "--key", &h.key("alice")]);
    assert_eq!(code, 1, "'.' is a dot, not any character: {text}");
    let (code, text) = h.run(&["--find", "FILE0", "--key", &h.key("alice")]);
    assert_eq!(code, 1, "capitals matter: {text}");
}

/// At most 50 matches are printed, with a count of the rest; the holders are
/// printed for what was printed.
#[test]
fn find_caps_its_matches_and_counts_the_rest() {
    let h = Heir::new();
    let (code, text) = h.run(&["--find", "bulk/item-", "--key", &h.key("alice")]);
    assert_eq!(code, 0, "{text}");
    let printed = text
        .lines()
        .filter(|l| l.contains("bulk/item-") && l.contains(" bytes  "))
        .count();
    assert_eq!(printed, 50, "{text}");
    assert!(
        text.contains("  docs  v1  f  101 bytes  bulk/item-001.dat"),
        "{text}"
    );
    assert!(text.contains("bulk/item-050.dat"), "{text}");
    assert!(!text.contains("bulk/item-051.dat"), "{text}");
    let t = flat(&text);
    assert!(
        t.contains("70 more match(es) are not shown: this prints the first 50."),
        "{t}"
    );
    assert!(t.contains("Tapes holding docs version 1:"), "{t}");

    // Under the cap there is no such line.
    let (code, text) = h.run(&["--find", "item-11", "--key", &h.key("alice")]);
    assert_eq!(code, 0, "{text}");
    assert_eq!(
        text.lines()
            .filter(|l| l.contains("bulk/item-") && l.contains(" bytes  "))
            .count(),
        10,
        "{text}"
    );
    assert!(!text.contains("more match(es)"), "{text}");
}

/// An escrow key opens every tenant's envelope, so `--find` searches every
/// tenant, one limit shared by all.
#[test]
fn find_with_the_escrow_key_searches_every_tenant_under_one_limit() {
    let h = Heir::new();
    let (code, text) = h.run(&["--find", "file0.bin", "--key", &h.key("esc")]);
    assert_eq!(code, 0, "{text}");
    for tenant in ["alice", "bob", "carol"] {
        assert!(
            text.contains(&format!("In the index of tenant {tenant}:")),
            "{tenant}: {text}"
        );
    }
    assert!(text.contains("  ledgers  v3  f  "), "{text}");
    assert!(text.contains("  restaged  v4  f  "), "{text}");
    assert_forward_only(&h, "--find with the escrow key");
    // One limit across envelopes: 120 matches in alice's `docs` use it up, and
    // bob's and carol's are counted, not printed.
    let (code, text) = h.run(&["--find", "e", "--key", &h.key("esc")]);
    assert_eq!(code, 0, "{text}");
    let printed = text.lines().filter(|l| l.contains(" bytes  ")).count();
    assert_eq!(printed, 50, "{text}");
    assert!(
        flat(&text).contains("more match(es) are not shown: this prints the first 50."),
        "{text}"
    );
}

/// Both modes read the tape forward from one rewind, as every other mode does,
/// and an unrelated key finds nothing without extra motion.
#[test]
fn the_index_modes_read_forward_from_one_rewind() {
    let h = Heir::new();
    for args in [
        vec!["--where", "docs", "--key", &h.key("alice")],
        vec!["--find", "item-0", "--key", &h.key("alice")],
        vec!["--where", "docs", "--key", &h.key("op")],
        vec!["--find", "item-0", "--key", &h.key("esc")],
    ] {
        let (code, text) = h.run(&args);
        assert_eq!(code, 0, "{args:?}: {text}");
        assert_forward_only(&h, &format!("{args:?}"));
        // Never beyond the envelopes: no data slice, no seal marker.
        let first_slice = template().units[0].positions[0];
        assert!(
            reads(&h).iter().all(|p| *p < first_slice),
            "{args:?}: only envelopes are read:\n{:#?}",
            h.ops()
        );
    }
    // A key that opens nothing is the same refusal as every other mode.
    let (code, text) = h.run(&["--where", "docs", "--key", &h.key("stranger")]);
    assert_eq!(code, 1, "{text}");
    assert!(
        text.contains("no envelope matched the key provided"),
        "{text}"
    );
    assert_eq!(h.count("mt rewind"), 1, "{:#?}", h.ops());
}

/// The two modes are in the per-mode tool table: the envelope set (`age`,
/// `truncate`, `tar`) plus the read set, no `dar`, `tee` or `mkfifo`; a host
/// without one is refused before the tape is touched.
#[test]
fn the_index_modes_check_the_tools_they_run_and_no_others() {
    for args in [
        vec!["--where", "docs", "--key", "KEY"],
        vec!["--find", "x", "--key", "KEY"],
    ] {
        for (missing, host) in [("age", false), ("tar", true), ("truncate", true)] {
            let mut h = Heir::new();
            h = if host {
                h.without_host(missing)
            } else {
                h.without(missing)
            };
            let args: Vec<String> = args
                .iter()
                .map(|a| {
                    if *a == "KEY" {
                        h.key("alice")
                    } else {
                        a.to_string()
                    }
                })
                .collect();
            let args: Vec<&str> = args.iter().map(String::as_str).collect();
            let (code, text) = h.run(&args);
            assert_eq!(code, 1, "{args:?} without {missing}: {text}");
            assert!(
                text.contains(&format!("missing required tool: {missing}")),
                "{args:?}: {text}"
            );
            assert!(
                h.ops().is_empty(),
                "{args:?}: refused before the tape: {:#?}",
                h.ops()
            );
        }
        // No dar, tee or mkfifo is needed.
        let h = Heir::new()
            .without_host("dar")
            .without_host("tee")
            .without_host("mkfifo");
        let args: Vec<String> = args
            .iter()
            .map(|a| {
                if *a == "KEY" {
                    h.key("alice")
                } else {
                    a.to_string()
                }
            })
            .collect();
        let args: Vec<&str> = args.iter().map(String::as_str).collect();
        let (code, text) = h.run(&args);
        assert!(!text.contains("missing required tool"), "{args:?}: {text}");
        assert_eq!(code, 0, "{args:?} runs without dar, tee or mkfifo: {text}");
    }
}

/// Usage errors say what was wrong and read nothing; `--help` documents both
/// modes, the print limit and how a path is shown.
#[test]
fn the_index_modes_have_usage_errors_and_are_in_help() {
    let h = Heir::new();
    for (args, needle) in [
        (vec!["--where"], "--where needs a unit name"),
        (vec!["--where", "--key", "k"], "--where needs a unit name"),
        (vec!["--find"], "--find needs the text to look for"),
        (vec!["--find", ""], "an empty text matches every path"),
        (vec!["--where", "docs"], "usage: "),
        (vec!["--find", "x", "--key"], "--key needs a value"),
        (vec!["--find", "x", "--bogus"], "unknown option: --bogus"),
    ] {
        let (code, text) = h.run(&args);
        assert_eq!(code, 1, "{args:?}: {text}");
        assert!(text.contains(needle), "{args:?}: {text}");
        assert!(h.ops().is_empty(), "{args:?}: {:#?}", h.ops());
    }
    let (code, help) = h.run(&["--help"]);
    assert_eq!(code, 0, "{help}");
    let f = flat(&help);
    for needle in [
        "--where UNIT --key KEYFILE [--key K2 ...]",
        "--find TEXT --key KEYFILE [--key K2 ...]",
        "At most 50 matches are printed",
        "A TAB, newline or carriage return in a path is shown as \\t, \\n or \\r.",
        "the newest tape you hold has the most complete one",
        "A tape written before version 1.3.0 has none",
    ] {
        assert!(f.contains(needle), "--help lacks {needle:?}:\n{help}");
    }
    let (_, info) = h.run(&["--info"]);
    assert!(
        info.contains("--where UNIT --key YOUR_KEY.age.key"),
        "{info}"
    );
}

// ---- 1.3.0: File 1's own commands, run as written (#477) ----

/// The code lines (indented four spaces or more) of the block of `section` that
/// holds a line containing `needle`: a block is a run of such lines.
fn guide_block(section: &str, needle: &str) -> Vec<String> {
    let mut blocks: Vec<Vec<String>> = vec![Vec::new()];
    for line in section.lines() {
        if line.starts_with("    ") && !line.trim().is_empty() {
            blocks.last_mut().unwrap().push(line.trim().to_string());
        } else if !blocks.last().unwrap().is_empty() {
            blocks.push(Vec::new());
        }
    }
    blocks
        .into_iter()
        .find(|b| b.iter().any(|l| l.contains(needle)))
        .unwrap_or_else(|| panic!("no code block with {needle:?} in:\n{section}"))
}

/// A section of the guide: from `## title` to the next `## `.
fn guide_section(guide: &str, title: &str) -> String {
    let head = format!("## {title}");
    let from = guide.find(&head).expect("section") + head.len();
    let rest = &guide[from..];
    rest[..rest.find("\n## ").unwrap_or(rest.len())].to_string()
}

/// File 1's "Manual Recovery Steps", run command by command against the fake
/// tape, as an heir with only `mt`, `dd`, `age`, `tar`, `sha256sum` and `dar`
/// would: File 0, the front index, the seal marker (with `eod` and `bsfm 2`),
/// an envelope the key opens after one it does not, the manifest, the slices
/// named `restore.N.dar`, and dar into an empty directory. The unit that comes
/// out is the unit that went in.
#[test]
fn the_manual_path_in_file_1_restores_a_unit_as_written() {
    let h = Heir::new();
    let guide = generate_system_guide_v2(LABEL, template().total_files);
    let manual = guide_section(&guide, "Manual Recovery Steps");
    let work = h.dir.join("manual");
    std::fs::create_dir_all(&work).unwrap();
    let run = |lines: &[String], subs: &[(&str, String)]| -> (bool, String) {
        let mut script = lines.join("\n");
        for (from, to) in subs {
            script = script.replace(from, to);
        }
        let o = Command::new("bash")
            .args(["-e", "-o", "pipefail", "-c", &script])
            .current_dir(&work)
            .env("PATH", &h.path)
            .env("FAKE_TAPE", &h.dir)
            .env("HOME", h.dir.join("home"))
            .output()
            .expect("spawn bash");
        let text = format!(
            "{}{}",
            String::from_utf8_lossy(&o.stdout),
            String::from_utf8_lossy(&o.stderr)
        );
        (o.status.success(), text)
    };

    // Steps 1-4: block mode, File 0, File 3 with its hash, the seal marker.
    let (ok, text) = run(&guide_block(&manual, "setblk 524288"), &[]);
    assert!(ok, "{text}");
    let (ok, text) = run(&guide_block(&manual, "> file0.txt"), &[]);
    assert!(ok, "{text}");
    let file0 = std::fs::read_to_string(work.join("file0.txt")).unwrap();
    assert!(file0.contains(&format!("label = \"{LABEL}\"")), "{file0}");
    let (ok, text) = run(&guide_block(&manual, "> front_index.toml"), &[]);
    assert!(ok, "{text}");
    let fi_hash = text.split_whitespace().next().unwrap().to_string();
    let (ok, text) = run(&guide_block(&manual, "> seal.toml"), &[]);
    assert!(ok, "eod then bsfm 2 reaches the seal marker: {text}");
    let seal = std::fs::read_to_string(work.join("seal.toml")).unwrap();
    assert!(
        seal.contains(&format!("front_index_sha256 = \"{fi_hash}\"")),
        "the seal binds the front index the manual path read:\n{seal}"
    );
    assert!(
        seal.contains(&format!("file_count = {}", template().total_files)),
        "{seal}"
    );

    // Step 5: the envelope entries, in order. bob's key opens the second.
    let fi = std::fs::read_to_string(work.join("front_index.toml")).unwrap();
    let mut entries: Vec<(String, String, String, String)> = Vec::new();
    for chunk in fi.split("[[files]]").skip(1) {
        let get = |key: &str| {
            chunk
                .lines()
                .find_map(|l| l.strip_prefix(&format!("{key} = ")))
                .map(|v| v.trim_matches('"').to_string())
                .unwrap_or_default()
        };
        entries.push((
            get("position"),
            get("type"),
            get("size_bytes"),
            get("sha256_encrypted"),
        ));
    }
    let envelope_block = guide_block(&manual, "> envelope.tar");
    let mut opened = None;
    for (pos, ty, size, sha) in &entries {
        if ty != "tenant_envelope" && ty != "operator_envelope" {
            continue;
        }
        let (ok, text) = run(
            &envelope_block,
            &[
                ("POSITION", pos.clone()),
                ("SIZE", size.clone()),
                ("YOUR_KEY.age.key", h.key("bob")),
            ],
        );
        if ok {
            assert!(
                text.contains(sha),
                "the sha256sum printed equals the front index's: {text}"
            );
            opened = Some(pos.clone());
            break;
        }
        assert!(
            text.contains("no identity matched any of the recipients"),
            "a key that does not open an envelope says so: {text}"
        );
    }
    assert_eq!(opened.as_deref(), Some("5"), "bob's envelope is the second");
    let (ok, text) = run(&guide_block(&manual, "tar xf envelope.tar"), &[]);
    assert!(ok, "{text}");
    assert!(
        text.contains("catalogs") && text.contains("index"),
        "{text}"
    );

    // Step 6-7: the manifest, then each slice as the guide names the commands.
    let manifest = tapectl::volume::manifest::Manifest::from_toml(
        &std::fs::read_to_string(work.join("env/MANIFEST.toml")).unwrap(),
    )
    .unwrap();
    assert_eq!(manifest.units.len(), 1);
    assert_eq!(manifest.units[0].name, "ledgers");
    // Step 7: the tape is put at the first slice once; each read leaves it at
    // the next file.
    let (ok, text) = run(
        &guide_block(&manual, "FIRST_POSITION"),
        &[(
            "FIRST_POSITION",
            manifest.units[0].slices[0].tape_position.to_string(),
        )],
    );
    assert!(ok, "{text}");
    let slice_block = guide_block(&manual, "> restore.NUMBER.dar");
    for sl in &manifest.units[0].slices {
        let (ok, text) = run(
            &slice_block,
            &[
                ("SHA256_ENCRYPTED", sl.sha256_encrypted.clone()),
                ("ENCRYPTED_BYTES", sl.encrypted_bytes.to_string()),
                ("NUMBER", sl.number.to_string()),
                ("YOUR_KEY.age.key", h.key("bob")),
            ],
        );
        assert!(ok, "slice {}: {text}", sl.number);
        assert!(
            work.join(format!("restore.{}.dar", sl.number)).exists(),
            "restore.N.dar naming"
        );
        assert!(
            !work.join(format!("restore.{}.dar.age", sl.number)).exists(),
            "the encrypted copy is removed once decrypted"
        );
    }

    // Step 8: the guide's dar line, into an empty directory.
    let snippet = |starts: &str| -> String {
        manual
            .split('`')
            .find(|c| c.starts_with(starts))
            .unwrap_or_else(|| panic!("the guide has no `{starts}...` command"))
            .to_string()
    };
    let mkdir_cmd = snippet("mkdir /destination");
    let dar_cmd = snippet("dar -x restore -R /destination -O -Q -N");
    let (ok, text) = run(
        &[mkdir_cmd, dar_cmd],
        &[("/destination", h.sub("manual-restored"))],
    );
    assert!(ok, "{text}");
    assert!(text.contains("inode(s) restored"), "{text}");
    assert!(
        same_tree(&unit("ledgers").src, &h.dir.join("manual-restored")),
        "the unit that came out is the unit that went in"
    );
}

/// File 1's completeness check, run on a real restore: dar's summary says
/// nothing failed, and the number of catalogue entries `--list` shows, counted
/// with the guide's own `grep`, equals the "inode(s) restored" figure.
#[test]
fn the_completeness_check_in_file_1_counts_what_dar_restored() {
    let h = Heir::new();
    let guide = generate_system_guide_v2(LABEL, template().total_files);
    let sec = guide_section(&guide, "Did the restore get everything?");
    let grep_line = sec
        .lines()
        .find(|l| l.contains("grep -c -E"))
        .expect("the guide's count command")
        .trim()
        .to_string();
    let pattern = grep_line
        .split('\'')
        .nth(1)
        .expect("the pattern is single-quoted")
        .to_string();

    let (code, text) = h.run(&[
        "--restore",
        "--key",
        &h.key("alice"),
        "--unit",
        "docs",
        "--to",
        &h.sub("restored"),
    ]);
    assert_eq!(code, 0, "{text}");
    assert!(text.contains("RESTORE COMPLETE"), "{text}");
    let restored: usize = text
        .lines()
        .find_map(|l| l.trim().strip_suffix(" inode(s) restored"))
        .expect("dar's summary line")
        .trim()
        .parse()
        .unwrap();
    for line in [
        "0 inode(s) not restored (not saved in archive)",
        "0 inode(s) not restored (overwriting policy decision)",
        "0 inode(s) ignored (excluded by filters)",
        "0 inode(s) failed to restore (filesystem error)",
    ] {
        assert!(text.contains(line), "the guide quotes dar: {line}\n{text}");
    }

    let (code, listing) = h.run(&["--list", "--key", &h.key("alice"), "--unit", "docs"]);
    assert_eq!(code, 0, "{listing}");
    let counted = listing
        .lines()
        .filter(|l| {
            // grep -E '^\[.{5}\]\[.\] ', as the guide prints it.
            let b = l.as_bytes();
            pattern == "^\\[.{5}\\]\\[.\\] "
                && l.starts_with('[')
                && b.len() > 10
                && b[6] == b']'
                && b[7] == b'['
                && b[9] == b']'
                && b[10] == b' '
        })
        .count();
    assert!(
        restored >= 3,
        "a positive control: {restored} inodes in docs"
    );
    assert_eq!(
        counted, restored,
        "the guide's count equals dar's:\n{listing}"
    );
    // And the real grep agrees with the reading above.
    let real = Command::new("bash")
        .arg("-c")
        .arg(format!("grep -c -E '{pattern}'"))
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .and_then(|mut c| {
            use std::io::Write;
            c.stdin.take().unwrap().write_all(listing.as_bytes())?;
            c.wait_with_output()
        })
        .unwrap();
    assert_eq!(
        String::from_utf8_lossy(&real.stdout).trim(),
        restored.to_string(),
        "grep -c -E agrees"
    );
}

// ---- replacement slices and the damaged-file list (ADR-0012 items 101, 102) ----

/// Every file under `dir`, as paths relative to it.
fn rel_files(dir: &Path) -> Vec<PathBuf> {
    fn walk(root: &Path, at: &Path, out: &mut Vec<PathBuf>) {
        for e in std::fs::read_dir(at).unwrap() {
            let e = e.unwrap();
            if e.file_type().unwrap().is_dir() {
                walk(root, &e.path(), out);
            } else {
                out.push(e.path().strip_prefix(root).unwrap().to_path_buf());
            }
        }
    }
    let mut out = Vec::new();
    walk(dir, dir, &mut out);
    out.sort();
    out
}

/// The files of `src` that `dest` does not hold byte for byte (missing, or
/// different), relative to the roots, sorted.
fn damaged_by_diff(src: &Path, dest: &Path) -> Vec<String> {
    rel_files(src)
        .into_iter()
        .filter(|rel| std::fs::read(dest.join(rel)).ok() != std::fs::read(src.join(rel)).ok())
        .map(|rel| rel.to_string_lossy().to_string())
        .collect()
}

/// The slice at index `idx` of `u`, as another tape holds it: its exact bytes.
fn good_slice(t: &Template, u: &UnitOnTape, idx: usize) -> Vec<u8> {
    let mut bytes = std::fs::read(t.dir.join("tape").join(u.positions[idx].to_string())).unwrap();
    bytes.truncate(u.sizes[idx]);
    assert_eq!(
        sha256_hex(&bytes),
        u.hashes[idx],
        "the template's own slice"
    );
    bytes
}

/// Flip a byte inside the slice at tape file `pos` of this heir's tape. The
/// stub age does not authenticate, so the damage gets through it and meets
/// the front index's hash.
fn flip_in_tape(h: &Heir, pos: i64) {
    let f = h.dir.join("tape").join(pos.to_string());
    let mut bytes = std::fs::read(&f).unwrap();
    bytes[100] ^= 0xff;
    std::fs::write(&f, bytes).unwrap();
}

/// A directory of slice files under names that say nothing about the slice.
fn slices_dir(h: &Heir, name: &str, files: &[(&str, Vec<u8>)]) -> String {
    let d = h.dir.join(name);
    std::fs::create_dir_all(&d).unwrap();
    for (n, bytes) in files {
        std::fs::write(d.join(n), bytes).unwrap();
    }
    d.display().to_string()
}

fn restore_args<'a>(h: &'a Heir, unit: &'a str, dest: &'a str) -> Vec<String> {
    vec![
        "--restore".to_string(),
        "--key".to_string(),
        h.key("alice"),
        "--unit".to_string(),
        unit.to_string(),
        "--to".to_string(),
        dest.to_string(),
    ]
}

fn runv(h: &Heir, args: &[String]) -> (i32, String) {
    let all: Vec<&str> = args.iter().map(String::as_str).collect();
    h.run_input(&all, &[], "")
}

fn run_with(
    h: &Heir,
    args: &[String],
    extra: &[&str],
    input: &str,
    env: &[(&str, &str)],
) -> (i32, String) {
    let mut all: Vec<&str> = args.iter().map(String::as_str).collect();
    all.extend_from_slice(extra);
    h.run_input(&all, env, input)
}

/// A second cartridge in `h`'s `tape2`: copies of `slices` laid out as another
/// copy of the unit would lay them, after a decoy slice and in reverse order,
/// under its own label and volume uuid. Its front index lists every file with
/// its hash, so the script can find a slice by hash and by nothing else.
fn second_tape(h: &Heir, label: &str, uuid: &str, slices: &[Vec<u8>]) {
    let mut files: Vec<(i64, &'static str, Vec<u8>)> = Vec::new();
    let decoy = stub_encrypt("alice,op,esc", "", &content(999, 40_000));
    let mut next = 4i64;
    let mut data: Vec<(i64, &'static str, Vec<u8>)> = vec![(next, "data_slice", decoy)];
    next += 1;
    for s in slices.iter().rev() {
        data.push((next, "data_slice", s.clone()));
        next += 1;
    }
    let seal_pos = next;
    let total_files = (seal_pos + 1) as i32;
    files.push((
        0,
        "id_thunk",
        generate_id_thunk_v2(&IdThunkV2Params {
            label,
            uuid,
            media_type: "LTO-6",
            tapectl_version: "1.3.0",
            nominal_capacity: 2_500_000_000_000,
            mam_capacity: 2_500_000_000_000,
            total_files,
            mam_manufacturer: "HP",
            mam_serial: "SERIAL02",
            mam_length: 846,
            mam_loads: 1,
            created_at: "2026-10-06T00:00:00Z",
            cartridge_identity_source: Some("mam"),
        })
        .into_bytes(),
    ));
    files.push((
        1,
        "system_guide",
        generate_system_guide_v2(label, total_files).into_bytes(),
    ));
    files.push((
        2,
        "restore_sh",
        generate_restore_script_v2(label, total_files).into_bytes(),
    ));
    files.extend(data);
    let mut fi: Vec<FrontIndexFile> = vec![];
    for pos in 0..total_files as i64 {
        if pos == 3 || pos == seal_pos {
            fi.push(FrontIndexFile {
                position: pos as i32,
                type_label: if pos == 3 {
                    "front_index"
                } else {
                    "seal_marker"
                },
                size_bytes: None,
                sha256_encrypted: None,
            });
        } else {
            let (_, ty, bytes) = files.iter().find(|(p, _, _)| *p == pos).unwrap();
            fi.push(FrontIndexFile {
                position: pos as i32,
                type_label: ty,
                size_bytes: Some(bytes.len() as u64),
                sha256_encrypted: Some(sha256_hex(bytes)),
            });
        }
    }
    let front = generate_front_index(label, &fi).into_bytes();
    let front_sha = sha256_hex(&front);
    let mut sealed = fi.clone();
    sealed[3].size_bytes = Some(front.len() as u64);
    sealed[3].sha256_encrypted = Some(front_sha.clone());
    let seal = generate_seal_marker(label, total_files, &front_sha, &sealed).into_bytes();
    files.push((3, "front_index", front));
    files.push((seal_pos, "seal_marker", seal));
    let dir = h.dir.join("tape2");
    std::fs::create_dir_all(&dir).unwrap();
    for (pos, _, bytes) in files {
        std::fs::write(dir.join(pos.to_string()), padded(bytes)).unwrap();
    }
}

/// The reads (`dd N`) and rewinds of the ops from `from` to `to` (exclusive).
fn segment(ops: &[String], from: usize, to: usize) -> (Vec<i64>, usize) {
    let seg = &ops[from..to];
    let reads = seg
        .iter()
        .filter_map(|l| l.strip_prefix("dd "))
        .filter_map(|p| p.parse().ok())
        .collect();
    let rewinds = seg.iter().filter(|l| l.as_str() == "mt rewind").count();
    (reads, rewinds)
}

/// The ops of a `--swap-tapes` run, split where the cartridge changed: the
/// first tape's, and the second's.
fn tapes_of(h: &Heir) -> ((Vec<i64>, usize), (Vec<i64>, usize)) {
    let ops = h.ops();
    let at = ops
        .iter()
        .position(|l| l == "swap")
        .unwrap_or_else(|| panic!("no swap in the ops:\n{ops:#?}"));
    (segment(&ops, 0, at), segment(&ops, at, ops.len()))
}

fn ascending(reads: &[i64]) -> bool {
    reads.windows(2).all(|w| w[0] < w[1])
}

/// A bad slice on the tape is taken from a directory of slice files, matched
/// by the sha256 the front index lists and by nothing else: under a name that
/// belongs to another slice, block-padded as `dd` copies it, with a file of
/// exactly the right size and the wrong bytes passed over by name.
#[test]
fn a_bad_slice_is_replaced_from_a_directory_by_its_hash() {
    let h = Heir::new();
    let t = template();
    let photos = unit("photos/2019");
    flip_in_tape(&h, photos.positions[2]);
    let good = good_slice(t, &photos, 2);
    let mut liar = good.clone();
    liar[7] ^= 1;
    let mut padded_copy = good.clone();
    padded_copy.resize(good.len().div_ceil(BLOCK) * BLOCK, 0);
    let dir = slices_dir(
        &h,
        "copies",
        &[
            ("a-liar.bin", liar),
            ("slice-1.enc", padded_copy),
            ("notes.txt", b"not a slice".to_vec()),
        ],
    );
    let dest = h.sub("restored");
    let (code, text) = run_with(
        &h,
        &restore_args(&h, "photos/2019", &dest),
        &["--slices-from", &dir],
        "",
        &[],
    );
    assert_eq!(code, 0, "{text}");
    assert!(text.contains("RESTORE COMPLETE"), "{text}");
    assert!(
        text.contains("slice 3 (tape file") && text.contains("does not match the front index"),
        "the bad slice is named:\n{text}"
    );
    assert!(
        text.contains(&format!("taking slice 3 from {dir}/slice-1.enc")),
        "the copy is chosen by its hash, whatever its name:\n{text}"
    );
    assert!(
        text.contains("a-liar.bin has the size of slice 3 but not its sha256"),
        "the wrong bytes of the right size are named and not used:\n{text}"
    );
    assert!(same_tree(&photos.src, Path::new(&dest)), "{text}");
    assert_eq!(h.count("mt rewind"), 1, "one rewind:\n{:#?}", h.ops());
    // The positive control: the same bad tape, no directory, stops, and says
    // what would have helped.
    let h2 = Heir::new();
    flip_in_tape(&h2, photos.positions[2]);
    let (code, text) = run_with(
        &h2,
        &restore_args(&h2, "photos/2019", &h2.sub("r")),
        &[],
        "",
        &[],
    );
    assert_ne!(code, 0, "{text}");
    assert!(text.contains("slice 3 checksum MISMATCH"), "{text}");
    assert!(text.contains("--slices-from DIR"), "{text}");
    assert!(text.contains("--swap-tapes"), "{text}");
}

/// A directory with nothing that matches does not replace the slice: the files
/// the slice held are not restored, are listed, and the run ends INCOMPLETE
/// with the other files restored.
#[test]
fn a_directory_with_no_matching_file_leaves_the_files_of_the_slice_listed() {
    let h = Heir::new();
    let t = template();
    let photos = unit("photos/2019");
    flip_in_tape(&h, photos.positions[1]);
    let mut liar = good_slice(t, &photos, 1);
    liar[7] ^= 1;
    let dir = slices_dir(&h, "copies", &[("liar.bin", liar)]);
    let dest = h.sub("restored");
    let (code, text) = run_with(
        &h,
        &restore_args(&h, "photos/2019", &dest),
        &["--slices-from", &dir],
        "",
        &[],
    );
    assert_eq!(code, 1, "{text}");
    assert!(text.contains("RESTORE INCOMPLETE"), "{text}");
    assert!(!text.contains("RESTORE COMPLETE"), "{text}");
    assert!(
        text.contains("has the size of slice 2 but not its sha256"),
        "{text}"
    );
    let list =
        std::fs::read_to_string(Path::new(&dest).join(".tapectl-damaged-files.txt")).unwrap();
    let mut lines = list.lines();
    assert_eq!(
        lines.next().unwrap(),
        "#tapectl-damaged-files\t1\tunit=photos/2019\tuuid=11111111-2222-3333-4444-555555555555\tversion=2"
    );
    let listed: Vec<String> = lines.map(str::to_string).collect();
    let mut want = damaged_by_diff(&photos.src, Path::new(&dest));
    want.retain(|p| p != ".tapectl-damaged-files.txt");
    assert!(
        !want.is_empty(),
        "positive control: the bad slice cost some file"
    );
    let (mut listed, mut want) = (listed, want);
    listed.sort();
    want.sort();
    assert_eq!(
        listed, want,
        "the list is the files that did not come back:\n{text}"
    );
    assert!(
        text.contains("--fill-from"),
        "the fill command is named:\n{text}"
    );
    // Everything else came back.
    assert!(
        rel_files(&photos.src).len() > want.len(),
        "some files were restored"
    );
}

/// A bad slice is read from a second cartridge: --swap-tapes names what it
/// still needs (number and sha256), waits for ENTER, reads just those slices
/// (found by hash, though they lie at other positions and in another order),
/// and goes on. Each tape is read forward from one rewind.
#[test]
fn a_bad_slice_is_replaced_from_a_second_tape_forward_only_on_both() {
    let h = Heir::new();
    let t = template();
    let photos = unit("photos/2019");
    flip_in_tape(&h, photos.positions[1]);
    flip_in_tape(&h, photos.positions[3]);
    let all: Vec<Vec<u8>> = (0..photos.positions.len())
        .map(|i| good_slice(t, &photos, i))
        .collect();
    second_tape(&h, "COPY02", "c0c0c0c0-0002-4000-8000-000000000002", &all);
    let dest = h.sub("restored");
    let (code, text) = run_with(
        &h,
        &restore_args(&h, "photos/2019", &dest),
        &["--swap-tapes"],
        "\n",
        &[("FAKE_SWAP_ON_SETBLK", "2")],
    );
    assert_eq!(code, 0, "{text}");
    assert!(text.contains("RESTORE COMPLETE"), "{text}");
    for i in [1usize, 3] {
        assert!(
            text.contains(&format!(
                "unit 'photos/2019' slice {}, {} bytes, sha256 {}",
                i + 1,
                photos.sizes[i],
                photos.hashes[i]
            )),
            "slice {} is named with its sha256:\n{text}",
            i + 1
        );
    }
    assert!(
        text.contains("Load a cartridge that holds copies"),
        "{text}"
    );
    assert!(
        text.contains("This cartridge identifies as: COPY02"),
        "{text}"
    );
    assert!(same_tree(&photos.src, Path::new(&dest)), "{text}");
    let ((first, first_rewinds), (second, second_rewinds)) = tapes_of(&h);
    assert_eq!(first_rewinds, 1, "tape 1: one rewind:\n{:#?}", h.ops());
    assert!(ascending(&first), "tape 1 is read forward: {first:?}");
    assert_eq!(second_rewinds, 1, "tape 2: one rewind:\n{:#?}", h.ops());
    assert!(ascending(&second), "tape 2 is read forward: {second:?}");
    // File 0, the front index, and just the two slices needed.
    assert_eq!(
        second.len(),
        4,
        "tape 2 reads only what it needs: {second:?}"
    );
    assert_eq!(&second[..2], &[0, 3], "{second:?}");
}

/// The cartridge that was loaded again is told apart from a copy, and
/// "done" goes on without: the files in the missing slice are listed.
#[test]
fn the_same_cartridge_is_not_a_copy_and_done_goes_on_without() {
    let h = Heir::new();
    let t = template();
    let photos = unit("photos/2019");
    flip_in_tape(&h, photos.positions[1]);
    let all: Vec<Vec<u8>> = (0..photos.positions.len())
        .map(|i| good_slice(t, &photos, i))
        .collect();
    // The same volume uuid as the tape being restored.
    second_tape(&h, LABEL, THIS_UUID, &all);
    let dest = h.sub("restored");
    let (code, text) = run_with(
        &h,
        &restore_args(&h, "photos/2019", &dest),
        &["--swap-tapes"],
        "\ndone\n",
        &[("FAKE_SWAP_ON_SETBLK", "2")],
    );
    assert_eq!(code, 1, "{text}");
    assert!(
        text.contains("That is the cartridge this run started with"),
        "{text}"
    );
    assert!(text.contains("Going on without them."), "{text}");
    assert!(text.contains("RESTORE INCOMPLETE"), "{text}");
    assert!(Path::new(&dest).join(".tapectl-damaged-files.txt").exists());
    // And with no input at all the run goes on without the second tape too.
    let h2 = Heir::new();
    flip_in_tape(&h2, photos.positions[1]);
    let (code, text) = run_with(
        &h2,
        &restore_args(&h2, "photos/2019", &h2.sub("r")),
        &["--swap-tapes"],
        "",
        &[],
    );
    assert_eq!(code, 1, "{text}");
    assert!(text.contains("Going on without them."), "{text}");
}

/// With --all, every unit that came back short waits for the one pause: a
/// single request for a cartridge covers all of them.
#[test]
fn all_asks_for_the_second_cartridge_once_for_every_unit_short_of_slices() {
    let h = Heir::new();
    let t = template();
    let photos = unit("photos/2019");
    let docs = unit("docs");
    flip_in_tape(&h, photos.positions[2]);
    flip_in_tape(&h, docs.positions[0]);
    let mut slices: Vec<Vec<u8>> = Vec::new();
    for i in 0..photos.positions.len() {
        slices.push(good_slice(t, &photos, i));
    }
    for i in 0..docs.positions.len() {
        slices.push(good_slice(t, &docs, i));
    }
    second_tape(
        &h,
        "COPY02",
        "c0c0c0c0-0002-4000-8000-000000000002",
        &slices,
    );
    let dest = h.sub("restored");
    let (code, text) = h.run_input(
        &[
            "--restore",
            "--key",
            &h.key("alice"),
            "--all",
            "--to",
            &dest,
            "--swap-tapes",
        ],
        &[("FAKE_SWAP_ON_SETBLK", "2")],
        "\n",
    );
    assert_eq!(code, 0, "{text}");
    assert!(text.contains("RESTORE COMPLETE"), "{text}");
    assert_eq!(
        text.matches("Load a cartridge that holds copies").count(),
        1,
        "one pause for both units:\n{text}"
    );
    assert!(text.contains("unit 'photos/2019' slice 3"), "{text}");
    assert!(text.contains("unit 'docs' slice 1"), "{text}");
    assert!(
        same_tree(&photos.src, &Path::new(&dest).join("photos/2019")),
        "{text}"
    );
    assert!(
        same_tree(&docs.src, &Path::new(&dest).join("docs")),
        "{text}"
    );
    let ((first, first_rewinds), (second, second_rewinds)) = tapes_of(&h);
    assert!(ascending(&first) && first_rewinds == 1, "{first:?}");
    assert!(ascending(&second) && second_rewinds == 1, "{second:?}");
}

/// --path takes a replacement too: the slices that hold the file are read
/// with the bad one taken from the directory.
#[test]
fn a_path_restore_takes_a_replacement_slice_too() {
    let h = Heir::new();
    let t = template();
    let photos = unit("photos/2019");
    // Every slice but the last, which carries the catalogue.
    for pos in &photos.positions[..photos.positions.len() - 1] {
        flip_in_tape(&h, *pos);
    }
    let copies: Vec<(String, Vec<u8>)> = (0..photos.positions.len() - 1)
        .map(|i| (format!("c{i}"), good_slice(t, &photos, i)))
        .collect();
    let files: Vec<(&str, Vec<u8>)> = copies
        .iter()
        .map(|(n, b)| (n.as_str(), b.clone()))
        .collect();
    let dir = slices_dir(&h, "copies", &files);
    let dest = h.sub("restored");
    let (code, text) = h.run(&[
        "--restore",
        "--key",
        &h.key("alice"),
        "--unit",
        "photos/2019",
        "--path",
        "file0.bin",
        "--to",
        &dest,
        "--slices-from",
        &dir,
    ]);
    assert_eq!(code, 0, "{text}");
    assert!(text.contains("RESTORE COMPLETE"), "{text}");
    assert_eq!(
        std::fs::read(Path::new(&dest).join("file0.bin")).unwrap(),
        std::fs::read(photos.src.join("file0.bin")).unwrap(),
        "{text}"
    );
}

/// --verify reports, for a slice that fails, whether a directory holds a copy
/// whose sha256 matches; and with --swap-tapes whether a second cartridge
/// does. The tape itself still fails: its own bytes are damaged.
#[test]
fn verify_says_where_a_good_copy_of_a_failed_slice_is() {
    let h = Heir::new();
    let t = template();
    let photos = unit("photos/2019");
    flip_in_tape(&h, photos.positions[2]);
    let good = good_slice(t, &photos, 2);
    let dir = slices_dir(&h, "copies", &[("whatever.bin", good)]);
    let (code, text) = h.run(&["--verify", "--slices-from", &dir]);
    assert_eq!(code, 1, "{text}");
    assert!(
        text.contains(&format!(
            "FAIL  file {:3}  data_slice                (hash mismatch; a copy whose sha256 matches is {dir}/whatever.bin)",
            photos.positions[2]
        )),
        "{text}"
    );
    assert!(
        text.contains("Every file that failed is a data slice, and a copy of each was found"),
        "{text}"
    );
    // The control: without the directory the same line has no such note.
    let (code, text) = h.run(&["--verify"]);
    assert_eq!(code, 1, "{text}");
    assert!(!text.contains("a copy whose sha256 matches"), "{text}");
    assert!(text.contains("(hash mismatch)"), "{text}");
    // A second cartridge.
    let all: Vec<Vec<u8>> = (0..photos.positions.len())
        .map(|i| good_slice(t, &photos, i))
        .collect();
    second_tape(&h, "COPY02", "c0c0c0c0-0002-4000-8000-000000000002", &all);
    let (code, text) = h.run_input(
        &["--verify", "--swap-tapes"],
        &[("FAKE_SWAP_ON_SETBLK", "2")],
        "\n",
    );
    assert_eq!(code, 1, "{text}");
    assert!(text.contains("checksum verified: a good copy"), "{text}");
    assert!(
        text.contains("Every file that failed is a data slice, and a copy of each was found"),
        "{text}"
    );
    let ((first, first_rewinds), (second, second_rewinds)) = tapes_of(&h);
    assert!(ascending(&first) && first_rewinds == 1, "{first:?}");
    assert!(ascending(&second) && second_rewinds == 1, "{second:?}");
}

#[test]
fn a_missing_slices_from_directory_is_refused_before_the_tape_is_read() {
    let h = Heir::new();
    let (code, text) = h.run(&[
        "--restore",
        "--key",
        &h.key("alice"),
        "--unit",
        "docs",
        "--to",
        &h.sub("restored"),
        "--slices-from",
        &h.sub("no-such-dir"),
    ]);
    assert_ne!(code, 0, "{text}");
    assert!(text.contains("not a directory"), "{text}");
    assert!(text.contains("Nothing has been read from tape"), "{text}");
    assert!(
        !h.ops().iter().any(|l| l.starts_with("dd ")),
        "no tape read:\n{:#?}",
        h.ops()
    );
    let (code, text) = h.run(&["--verify", "--bogus"]);
    assert_ne!(code, 0, "{text}");
    assert!(text.contains("unknown option: --bogus"), "{text}");
}

// ---- the damaged-file list and the fill ----

const DMG_UUID: &str = "77777777-8888-9999-aaaa-bbbbbbbbbbbb";

fn dmg_tenants(
    version: i64,
    uuid: &'static str,
    corrupt: &'static [(usize, usize)],
) -> Vec<TenantSpec> {
    vec![TenantSpec {
        name: "alice",
        units: vec![UnitSpec {
            cat: Cat::Matched,
            name: "ledger",
            uuid,
            version,
            seed: 21,
            files: 5,
            file_len: 150_000,
            names: &[],
            corrupt,
            tamper: &[],
        }],
    }]
}

/// The unit "ledger" version 2 as staged with a byte flipped in its third
/// slice before encryption: the tape's hashes agree with it, so only dar's CRC
/// of a file sees it.
fn dmg_bad() -> &'static Template {
    static T: OnceLock<Template> = OnceLock::new();
    T.get_or_init(|| build_template("dmg-bad", &dmg_tenants(2, DMG_UUID, &[(2, 50_000)]), false))
}

/// Another staging of the same unit and version, damaged in another slice.
fn dmg_other_bad() -> &'static Template {
    static T: OnceLock<Template> = OnceLock::new();
    T.get_or_init(|| {
        build_template(
            "dmg-other",
            &dmg_tenants(2, DMG_UUID, &[(6, 50_000)]),
            false,
        )
    })
}

fn dmg_good() -> &'static Template {
    static T: OnceLock<Template> = OnceLock::new();
    T.get_or_init(|| build_template("dmg-good", &dmg_tenants(2, DMG_UUID, &[]), false))
}

fn dmg_v3() -> &'static Template {
    static T: OnceLock<Template> = OnceLock::new();
    T.get_or_init(|| build_template("dmg-v3", &dmg_tenants(3, DMG_UUID, &[]), false))
}

fn dmg_other_unit() -> &'static Template {
    static T: OnceLock<Template> = OnceLock::new();
    T.get_or_init(|| {
        build_template(
            "dmg-uuid",
            &dmg_tenants(2, "88888888-9999-aaaa-bbbb-cccccccccccc", &[]),
            false,
        )
    })
}

const ODD_NAMES: &[&str] = &[
    "plain.bin",
    "we ird : name.bin",
    "nl\nline.bin",
    "tab\there.bin",
    "back\\slash.bin",
];
/// Byte flips in the first three slices, at offsets that differ in their low
/// bits: dar's per-file check is a width-limited XOR fold, so two flips of the
/// same value a multiple of that width apart cancel and go unseen.
const ODD_FLIPS: &[(usize, usize)] = &[
    (0, 1_000),
    (0, 21_019),
    (0, 41_037),
    (0, 61_055),
    (0, 81_073),
    (1, 1_005),
    (1, 21_024),
    (1, 41_042),
    (1, 61_060),
    (1, 81_078),
    (2, 1_011),
    (2, 21_030),
    (2, 41_048),
    (2, 61_066),
    (2, 81_084),
];

fn odd_tenants(corrupt: &'static [(usize, usize)]) -> Vec<TenantSpec> {
    vec![TenantSpec {
        name: "alice",
        units: vec![UnitSpec {
            cat: Cat::Matched,
            name: "oddly",
            uuid: "99999999-aaaa-bbbb-cccc-dddddddddddd",
            version: 1,
            seed: 31,
            files: 5,
            file_len: 80_000,
            names: ODD_NAMES,
            corrupt,
            tamper: &[],
        }],
    }]
}

fn odd_bad() -> &'static Template {
    static T: OnceLock<Template> = OnceLock::new();
    T.get_or_init(|| build_template("odd-bad", &odd_tenants(ODD_FLIPS), false))
}

fn odd_good() -> &'static Template {
    static T: OnceLock<Template> = OnceLock::new();
    T.get_or_init(|| build_template("odd-good", &odd_tenants(&[]), false))
}

fn unit_of(t: &Template, name: &str) -> UnitOnTape {
    t.units.iter().find(|u| u.name == name).unwrap().clone()
}

fn fill_args(h: &Heir, unit: &str, list: &Path, to: &str) -> Vec<String> {
    vec![
        "--restore".to_string(),
        "--key".to_string(),
        h.key("alice"),
        "--unit".to_string(),
        unit.to_string(),
        "--fill-from".to_string(),
        list.display().to_string(),
        "--to".to_string(),
        to.to_string(),
    ]
}

/// A restore in which dar names files it could not restore (the archive as
/// staged was damaged: the tape's hashes are right) restores the rest, writes
/// the files it could not restore to a list in the unit's directory, and ends
/// INCOMPLETE, naming the fill command.
#[test]
fn a_restore_with_a_damaged_file_writes_the_list_and_is_incomplete() {
    let h = Heir::of(dmg_bad(), &[]);
    let ledger = unit_of(dmg_bad(), "ledger");
    let dest = h.sub("restored");
    let (code, text) = h.run(&[
        "--restore",
        "--key",
        &h.key("alice"),
        "--unit",
        "ledger",
        "--to",
        &dest,
    ]);
    assert_eq!(code, 1, "{text}");
    assert!(
        text.contains("Bad CRC"),
        "dar's own words stay visible:\n{text}"
    );
    assert!(text.contains("RESTORE INCOMPLETE"), "{text}");
    assert!(!text.contains("RESTORE COMPLETE"), "{text}");
    let list_path = Path::new(&dest).join(".tapectl-damaged-files.txt");
    let list = std::fs::read_to_string(&list_path).unwrap();
    let mut lines = list.lines();
    assert_eq!(
        lines.next().unwrap(),
        format!("#tapectl-damaged-files\t1\tunit=ledger\tuuid={DMG_UUID}\tversion=2")
    );
    let listed: Vec<String> = lines.map(str::to_string).collect();
    let mut want = damaged_by_diff(&ledger.src, Path::new(&dest));
    want.retain(|p| p != ".tapectl-damaged-files.txt");
    assert!(!want.is_empty(), "positive control: some file is damaged");
    assert!(want.len() < 5, "and some came back whole: {want:?}");
    assert_eq!(listed, want, "the list is exactly the damaged files");
    assert!(
        text.contains(&format!(
            "--unit ledger --fill-from {} --to {dest}",
            list_path.display()
        )),
        "the fill command is named:\n{text}"
    );
    assert!(text.contains("same unit and version"), "{text}");
}

/// The positive control: a restore with nothing damaged writes no list.
#[test]
fn a_clean_restore_writes_no_list() {
    let h = Heir::of(dmg_good(), &[]);
    let ledger = unit_of(dmg_good(), "ledger");
    let dest = h.sub("restored");
    let (code, text) = h.run(&[
        "--restore",
        "--key",
        &h.key("alice"),
        "--unit",
        "ledger",
        "--to",
        &dest,
    ]);
    assert_eq!(code, 0, "{text}");
    assert!(text.contains("RESTORE COMPLETE"), "{text}");
    assert!(!text.contains("INCOMPLETE"), "{text}");
    assert!(same_tree(&ledger.src, Path::new(&dest)), "{text}");
    assert!(!Path::new(&dest).join(".tapectl-damaged-files.txt").exists());
    // And with a replacement asked for, though nothing needed one.
    let empty = h.sub("empty");
    std::fs::create_dir_all(&empty).unwrap();
    let dest2 = h.sub("restored2");
    let (code, text) = h.run(&[
        "--restore",
        "--key",
        &h.key("alice"),
        "--unit",
        "ledger",
        "--to",
        &dest2,
        "--slices-from",
        &empty,
    ]);
    assert_eq!(code, 0, "{text}");
    assert!(text.contains("RESTORE COMPLETE"), "{text}");
    assert!(same_tree(&ledger.src, Path::new(&dest2)), "{text}");
    assert!(!Path::new(&dest2)
        .join(".tapectl-damaged-files.txt")
        .exists());
}

/// A second tape holding the same unit and version fills exactly the files in
/// the list, over their damaged copies, and nothing else in the directory: a
/// local edit to another file is still there. The list is removed.
#[test]
fn a_fill_from_another_copy_restores_exactly_the_listed_files() {
    let a = Heir::of(dmg_bad(), &[]);
    let ledger = unit_of(dmg_bad(), "ledger");
    let dest = a.sub("restored");
    let (code, text) = a.run(&[
        "--restore",
        "--key",
        &a.key("alice"),
        "--unit",
        "ledger",
        "--to",
        &dest,
    ]);
    assert_eq!(code, 1, "{text}");
    let list = Path::new(&dest).join(".tapectl-damaged-files.txt");
    let damaged = damaged_by_diff(&ledger.src, Path::new(&dest));
    let whole: Vec<String> = rel_files(&ledger.src)
        .into_iter()
        .map(|p| p.to_string_lossy().to_string())
        .filter(|p| !damaged.contains(p))
        .collect();
    assert!(!whole.is_empty() && !damaged.is_empty());
    // A file the restore got right, edited since: a fill must not touch it.
    let edited = Path::new(&dest).join(&whole[0]);
    std::fs::write(&edited, b"edited since the restore").unwrap();

    let b = Heir::of(dmg_good(), &[]);
    let (code, text) = runv(&b, &fill_args(&b, "ledger", &list, &dest));
    assert_eq!(code, 0, "{text}");
    assert!(text.contains("FILL COMPLETE"), "{text}");
    assert!(!list.exists(), "the list is removed when nothing is left");
    for p in &damaged {
        assert_eq!(
            std::fs::read(Path::new(&dest).join(p)).unwrap(),
            std::fs::read(ledger.src.join(p)).unwrap(),
            "{p} was filled"
        );
    }
    assert_eq!(
        std::fs::read(&edited).unwrap(),
        b"edited since the restore",
        "a file not in the list is left alone"
    );
    // Only the listed files were extracted: the tape read what holds them.
    assert!(text.contains("Restoring"), "{text}");
    assert_forward_only(&b, "the fill");
}

/// Two copies from different stagings, each damaged in different files,
/// restore whole through two runs: the restore from one, the fill from the
/// other.
#[test]
fn two_copies_each_damaged_in_different_files_restore_whole_in_two_runs() {
    let a = Heir::of(dmg_bad(), &[]);
    let ledger = unit_of(dmg_bad(), "ledger");
    let dest = a.sub("restored");
    let (code, _) = a.run(&[
        "--restore",
        "--key",
        &a.key("alice"),
        "--unit",
        "ledger",
        "--to",
        &dest,
    ]);
    assert_eq!(code, 1);
    let list = Path::new(&dest).join(".tapectl-damaged-files.txt");
    let first_damage = damaged_by_diff(&ledger.src, Path::new(&dest));

    // The other copy is damaged too, in other files: restored alone it fails.
    let b = Heir::of(dmg_other_bad(), &[]);
    let alone = b.sub("alone");
    let (code, _) = b.run(&[
        "--restore",
        "--key",
        &b.key("alice"),
        "--unit",
        "ledger",
        "--to",
        &alone,
    ]);
    assert_eq!(code, 1);
    let second_damage = damaged_by_diff(&ledger.src, Path::new(&alone));
    assert!(
        first_damage
            .iter()
            .all(|f| f == ".tapectl-damaged-files.txt" || !second_damage.contains(f)),
        "the two copies are damaged in different files: {first_damage:?} / {second_damage:?}"
    );

    let (code, text) = runv(&b, &fill_args(&b, "ledger", &list, &dest));
    assert_eq!(code, 0, "{text}");
    assert!(text.contains("FILL COMPLETE"), "{text}");
    assert!(same_tree(&ledger.src, Path::new(&dest)), "{text}");
}

/// A tape holding the unit at another version, or another unit under the
/// same name, is refused by name, and nothing in the directory changes.
#[test]
fn a_fill_refuses_a_tape_with_another_version_or_another_unit() {
    let a = Heir::of(dmg_bad(), &[]);
    let dest = a.sub("restored");
    let (code, _) = a.run(&[
        "--restore",
        "--key",
        &a.key("alice"),
        "--unit",
        "ledger",
        "--to",
        &dest,
    ]);
    assert_eq!(code, 1);
    let list = Path::new(&dest).join(".tapectl-damaged-files.txt");
    let before_list = std::fs::read(&list).unwrap();
    let ledger = unit_of(dmg_bad(), "ledger");
    let before_tree: Vec<Vec<u8>> = rel_files(Path::new(&dest))
        .iter()
        .map(|p| std::fs::read(Path::new(&dest).join(p)).unwrap())
        .collect();

    let v3 = Heir::of(dmg_v3(), &[]);
    let (code, text) = runv(&v3, &fill_args(&v3, "ledger", &list, &dest));
    assert_ne!(code, 0, "{text}");
    assert!(
        text.contains("holds no version 2 of unit 'ledger'"),
        "{text}"
    );
    assert!(text.contains("same version"), "{text}");

    let other = Heir::of(dmg_other_unit(), &[]);
    let (code, text) = runv(&other, &fill_args(&other, "ledger", &list, &dest));
    assert_ne!(code, 0, "{text}");
    assert!(text.contains("it is not the same unit"), "{text}");
    assert!(
        text.contains("77777777-8888-9999-aaaa-bbbbbbbbbbbb"),
        "{text}"
    );

    // The control: the right tape fills it.
    assert_eq!(std::fs::read(&list).unwrap(), before_list);
    let after_tree: Vec<Vec<u8>> = rel_files(Path::new(&dest))
        .iter()
        .map(|p| std::fs::read(Path::new(&dest).join(p)).unwrap())
        .collect();
    assert_eq!(before_tree, after_tree, "nothing changed");
    let good = Heir::of(dmg_good(), &[]);
    let (code, text) = runv(&good, &fill_args(&good, "ledger", &list, &dest));
    assert_eq!(code, 0, "{text}");
    assert!(same_tree(&ledger.src, Path::new(&dest)), "{text}");
}

/// Names that need the escapes of sec 1.5 (a TAB, a newline, a backslash, and
/// a name holding the very separator dar prints) survive the list and the
/// fill: the list holds each once, escaped, and the fill reads them back.
#[test]
fn the_list_escapes_paths_as_the_index_does_and_the_fill_reads_them_back() {
    let a = Heir::of(odd_bad(), &[]);
    let oddly = unit_of(odd_bad(), "oddly");
    let dest = a.sub("restored");
    let (code, text) = a.run(&[
        "--restore",
        "--key",
        &a.key("alice"),
        "--unit",
        "oddly",
        "--to",
        &dest,
    ]);
    assert_eq!(code, 1, "{text}");
    let list = Path::new(&dest).join(".tapectl-damaged-files.txt");
    let text_list = std::fs::read_to_string(&list).unwrap();
    let listed: Vec<String> = text_list.lines().skip(1).map(str::to_string).collect();
    let mut want: Vec<String> = damaged_by_diff(&oddly.src, Path::new(&dest))
        .into_iter()
        .filter(|p| p != ".tapectl-damaged-files.txt")
        .map(|p| idx_esc(&p))
        .collect();
    want.sort();
    let mut got = listed.clone();
    got.sort();
    assert_eq!(got, want, "{text_list}\n{text}");
    assert!(
        want.len() >= 3,
        "positive control: several are damaged: {want:?}"
    );
    assert!(
        listed
            .iter()
            .any(|l| l.contains("\\n") || l.contains("\\t") || l.contains("\\\\")),
        "an escaped name is in the list: {listed:?}"
    );
    for l in &listed {
        assert!(
            !l.contains('\n') && !l.contains('\t'),
            "one line per path: {l:?}"
        );
    }

    let b = Heir::of(odd_good(), &[]);
    let (code, text) = runv(&b, &fill_args(&b, "oddly", &list, &dest));
    assert_eq!(code, 0, "{text}");
    assert!(text.contains("FILL COMPLETE"), "{text}");
    assert!(same_tree(&oddly.src, Path::new(&dest)), "{text}");
}

/// The end to end of item 101 and 102 together: a bad slice with no copy at
/// hand leaves a list; another copy of the unit fills it.
#[test]
fn a_slice_with_no_copy_leaves_a_list_that_another_copy_fills() {
    let a = Heir::of(dmg_good(), &[]);
    let ledger = unit_of(dmg_good(), "ledger");
    flip_in_tape(&a, ledger.positions[1]);
    let empty = a.sub("empty");
    std::fs::create_dir_all(&empty).unwrap();
    let dest = a.sub("restored");
    let (code, text) = a.run(&[
        "--restore",
        "--key",
        &a.key("alice"),
        "--unit",
        "ledger",
        "--to",
        &dest,
        "--slices-from",
        &empty,
    ]);
    assert_eq!(code, 1, "{text}");
    assert!(text.contains("RESTORE INCOMPLETE"), "{text}");
    assert!(text.contains("Slice(s) not available: 2"), "{text}");
    let list = Path::new(&dest).join(".tapectl-damaged-files.txt");
    assert!(list.exists(), "{text}");

    let b = Heir::of(dmg_other_bad(), &[]);
    let (code, text) = runv(&b, &fill_args(&b, "ledger", &list, &dest));
    assert_eq!(code, 0, "{text}");
    assert!(same_tree(&ledger.src, Path::new(&dest)), "{text}");
    assert!(!list.exists());
}

/// A fill takes only a unit's own list: no --path, no --all, one --unit that
/// is the list's own, and the list must sit where the restore wrote it.
#[test]
fn a_fill_is_refused_for_a_list_that_is_not_the_units_own() {
    let a = Heir::of(dmg_bad(), &[]);
    let dest = a.sub("restored");
    let (code, _) = a.run(&[
        "--restore",
        "--key",
        &a.key("alice"),
        "--unit",
        "ledger",
        "--to",
        &dest,
    ]);
    assert_eq!(code, 1);
    let list = Path::new(&dest).join(".tapectl-damaged-files.txt");
    let g = Heir::of(dmg_good(), &[]);
    let (code, text) = g.run(&[
        "--restore",
        "--key",
        &g.key("alice"),
        "--fill-from",
        &list.display().to_string(),
        "--to",
        &dest,
    ]);
    assert_ne!(code, 0, "{text}");
    assert!(text.contains("give --unit U (once)"), "{text}");
    let (code, text) = runv(&g, &fill_args(&g, "other", &list, &dest));
    assert_ne!(code, 0, "{text}");
    assert!(
        text.contains("this list is for unit 'ledger', version 2"),
        "{text}"
    );
    // A list moved out of the directory it describes.
    let moved = g.dir.join("moved.txt");
    std::fs::copy(&list, &moved).unwrap();
    let (code, text) = runv(&g, &fill_args(&g, "ledger", &moved, &dest));
    assert_ne!(code, 0, "{text}");
    assert!(
        text.contains("keep the list where the restore wrote it"),
        "{text}"
    );
    // A directory in the list is refused, not restored over.
    std::fs::create_dir_all(Path::new(&dest).join("subdir")).unwrap();
    let mut body = std::fs::read_to_string(&list).unwrap();
    body.push_str("subdir\n");
    std::fs::write(&list, body).unwrap();
    let (code, text) = runv(&g, &fill_args(&g, "ledger", &list, &dest));
    assert_ne!(code, 0, "{text}");
    assert!(text.contains("names a directory (subdir)"), "{text}");
    // A path that leaves the unit is refused.
    let mut body = std::fs::read_to_string(&list).unwrap();
    body = body.replace("subdir\n", "../escape\n");
    std::fs::write(&list, body).unwrap();
    let (code, text) = runv(&g, &fill_args(&g, "ledger", &list, &dest));
    assert_ne!(code, 0, "{text}");
    assert!(text.contains("names a path with .. in it"), "{text}");
}

/// The escapes the list uses are the index's, decoded in one scan: a name
/// holding a backslash followed by the letter t comes back as that, not as a
/// TAB.
#[test]
fn the_list_escape_functions_round_trip_the_sec_1_5_escapes() {
    let h = Heir::new();
    let script = h.dir.join("RESTORE.sh");
    let probe = format!(
        r#"source "{}" >/dev/null
for t in 'plain' 'a b : c' $'tab\there' $'nl\nhere' $'cr\rhere' 'back\slash' 'lit\tera' 'two\\slashes' $'mix\\\t\n'; do
  e=$(escape_text "$t")
  unesc_line "$e"
  [ "$UNESC" = "$t" ] || {{ echo "ROUNDTRIP-FAIL [$t] [$e]"; exit 1; }}
  case "$e" in *$'\t'* | *$'\n'* | *$'\r'*) echo "RAW-CONTROL [$e]"; exit 1 ;; esac
done
unesc_line 'lit\\tera'
[ "$UNESC" = 'lit\tera' ] || {{ echo "SCAN-FAIL [$UNESC]"; exit 1; }}
unesc_line 'a\tb'
[ "$UNESC" = $'a\tb' ] || {{ echo "TAB-FAIL"; exit 1; }}
unesc_line 'odd\qescape'
[ "$UNESC" = 'odd\qescape' ] || {{ echo "UNKNOWN-FAIL [$UNESC]"; exit 1; }}
echo ROUNDTRIP-OK
"#,
        script.display()
    );
    let o = Command::new("bash")
        .args(["-c", &probe])
        .env("PATH", &h.path)
        .env("FAKE_TAPE", &h.dir)
        .env("TMPDIR", h.dir.join("tmp"))
        .env("HOME", h.dir.join("home"))
        .output()
        .unwrap();
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&o.stdout),
        String::from_utf8_lossy(&o.stderr)
    );
    assert!(text.contains("ROUNDTRIP-OK"), "{text}");
}

/// The help names the options, and the default failure names them.
#[test]
fn help_names_the_replacement_and_fill_options() {
    let h = Heir::new();
    let (code, text) = h.run(&["--help"]);
    assert_eq!(code, 0, "{text}");
    let flat = flat(&text);
    for want in [
        "--slices-from DIR",
        "--swap-tapes",
        "--fill-from LIST",
        "never by position or by name",
        "about twice the unit's size",
        ".tapectl-damaged-files.txt",
        "RESTORE INCOMPLETE",
        "SAME unit AT THE SAME VERSION",
    ] {
        assert!(flat.contains(want), "--help lacks {want:?}:\n{text}");
    }
}

/// The same damaged unit staged without an envelope catalogue: on this dar it
/// restores from slices on disk, and that path ends the same way.
fn dmg_nocat_bad() -> &'static Template {
    static T: OnceLock<Template> = OnceLock::new();
    T.get_or_init(|| {
        let mut t = dmg_tenants(2, DMG_UUID, &[(2, 50_000)]);
        t[0].units[0].cat = Cat::Absent;
        build_template("dmg-nocat", &t, false)
    })
}

#[test]
fn a_restore_from_slices_on_disk_writes_the_list_too() {
    let h = Heir::of(dmg_nocat_bad(), &[]);
    let ledger = unit_of(dmg_nocat_bad(), "ledger");
    let dest = h.sub("restored");
    let (code, text) = h.run(&[
        "--restore",
        "--key",
        &h.key("alice"),
        "--unit",
        "ledger",
        "--to",
        &dest,
    ]);
    assert_eq!(code, 1, "{text}");
    assert!(text.contains("decrypting the slices to disk"), "{text}");
    assert!(text.contains("RESTORE INCOMPLETE"), "{text}");
    let list =
        std::fs::read_to_string(Path::new(&dest).join(".tapectl-damaged-files.txt")).unwrap();
    let mut listed: Vec<String> = list.lines().skip(1).map(str::to_string).collect();
    let mut want = damaged_by_diff(&ledger.src, Path::new(&dest));
    want.retain(|p| p != ".tapectl-damaged-files.txt");
    listed.sort();
    want.sort();
    assert!(!want.is_empty());
    assert_eq!(listed, want, "{text}");
}

/// A list from an earlier run is out of date once the unit restores whole:
/// the same restore, over its directory with --overwrite, from a good copy
/// removes it. (A list for another version or unit is left alone.)
#[test]
fn a_clean_restore_over_a_damaged_one_removes_the_stale_list() {
    let a = Heir::of(dmg_bad(), &[]);
    let ledger = unit_of(dmg_bad(), "ledger");
    let dest = a.sub("restored");
    let (code, _) = a.run(&[
        "--restore",
        "--key",
        &a.key("alice"),
        "--unit",
        "ledger",
        "--to",
        &dest,
    ]);
    assert_eq!(code, 1);
    let list = Path::new(&dest).join(".tapectl-damaged-files.txt");
    assert!(list.exists());
    let g = Heir::of(dmg_good(), &[]);
    let (code, text) = g.run(&[
        "--restore",
        "--key",
        &g.key("alice"),
        "--unit",
        "ledger",
        "--to",
        &dest,
        "--overwrite",
    ]);
    assert_eq!(code, 0, "{text}");
    assert!(text.contains("RESTORE COMPLETE"), "{text}");
    assert!(text.contains("left nothing damaged"), "{text}");
    assert!(!list.exists());
    assert!(same_tree(&ledger.src, Path::new(&dest)), "{text}");
}

/// What the disk refuses is not a damaged archive: dar's status 5 for a
/// directory that cannot be written ends the run as it always did, with no
/// list, whichever way the unit is restored.
#[test]
fn a_file_system_refusal_is_not_listed_as_damage() {
    use std::os::unix::fs::PermissionsExt;
    for (t, via) in [(dmg_good(), "streamed"), (dmg_nocat_bad(), "from disk")] {
        let h = Heir::of(t, &[]);
        let dest = h.dir.join("readonly");
        std::fs::create_dir_all(&dest).unwrap();
        std::fs::set_permissions(&dest, std::fs::Permissions::from_mode(0o555)).unwrap();
        if std::fs::File::create(dest.join("probe")).is_ok() {
            // Running as root: a read-only directory is no obstacle.
            return;
        }
        let scratch = h.sub("scratch");
        std::fs::create_dir_all(&scratch).unwrap();
        let (code, text) = h.run(&[
            "--restore",
            "--key",
            &h.key("alice"),
            "--unit",
            "ledger",
            "--to",
            &dest.display().to_string(),
            "--overwrite",
            "--scratch",
            &scratch,
        ]);
        std::fs::set_permissions(&dest, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert_ne!(code, 0, "{via}: {text}");
        assert!(
            text.contains("dar extraction failed (exit 5)"),
            "{via}: {text}"
        );
        assert!(text.contains("Permission denied"), "{via}: {text}");
        assert!(!text.contains("RESTORE INCOMPLETE"), "{via}: {text}");
        assert!(!dest.join(".tapectl-damaged-files.txt").exists(), "{via}");
        if via == "from disk" {
            assert!(
                text.contains("the file system refused a file"),
                "{via}: {text}"
            );
        }
    }
}

/// A fill that finds the slices it needs bad on its own tape takes them from
/// a second cartridge, like a restore: the list, the first tape, then the
/// swap.
#[test]
fn a_fill_takes_a_replacement_slice_from_a_second_tape() {
    let a = Heir::of(dmg_bad(), &[]);
    let ledger = unit_of(dmg_bad(), "ledger");
    let dest = a.sub("restored");
    let (code, _) = a.run(&[
        "--restore",
        "--key",
        &a.key("alice"),
        "--unit",
        "ledger",
        "--to",
        &dest,
    ]);
    assert_eq!(code, 1);
    let list = Path::new(&dest).join(".tapectl-damaged-files.txt");

    let good = dmg_good();
    let gl = unit_of(good, "ledger");
    let b = Heir::of(good, &[]);
    // Every data slice but the last is bad on this tape...
    for pos in &gl.positions[..gl.positions.len() - 1] {
        flip_in_tape(&b, *pos);
    }
    // ...and the second cartridge holds good ones.
    let all: Vec<Vec<u8>> = (0..gl.positions.len())
        .map(|i| good_slice(good, &gl, i))
        .collect();
    second_tape(&b, "COPY02", "c0c0c0c0-0002-4000-8000-000000000002", &all);
    let args = fill_args(&b, "ledger", &list, &dest);
    let (code, text) = run_with(
        &b,
        &args,
        &["--swap-tapes"],
        "\n",
        &[("FAKE_SWAP_ON_SETBLK", "2")],
    );
    assert_eq!(code, 0, "{text}");
    assert!(text.contains("FILL COMPLETE"), "{text}");
    assert!(
        text.contains("Load a cartridge that holds copies"),
        "{text}"
    );
    assert!(same_tree(&ledger.src, Path::new(&dest)), "{text}");
    let ((first, first_rewinds), (second, second_rewinds)) = tapes_of(&b);
    assert!(ascending(&first) && first_rewinds == 1, "{first:?}");
    assert!(ascending(&second) && second_rewinds == 1, "{second:?}");
}

/// Finding 1 of the 2026-10-17 read of File 1: `fsf` takes the number of files
/// to skip, not a file number. The guide's gap rule, run as written: read the
/// slice in File P, space by COUNT = Q - P - 1, and the head is at the start
/// of File Q, where the slice's own sha256 proves it (a wrong file fails the
/// check). The control is the old advice, `fsf Q` from where the head is,
/// which lands elsewhere.
#[test]
fn file_1_gap_spacing_reaches_the_slice_it_names() {
    let h = Heir::new();
    let photos = unit("photos/2019");
    assert!(photos.positions.len() >= 4);
    let guide = generate_system_guide_v2(LABEL, template().total_files);
    let manual = guide_section(&guide, "Manual Recovery Steps");
    let work = h.dir.join("gap");
    std::fs::create_dir_all(&work).unwrap();
    let run = |lines: &[String], subs: &[(&str, String)]| -> (bool, String) {
        let mut script = lines.join("\n");
        for (from, to) in subs {
            script = script.replace(from, to);
        }
        let o = Command::new("bash")
            .args(["-e", "-o", "pipefail", "-c", &script])
            .current_dir(&work)
            .env("PATH", &h.path)
            .env("FAKE_TAPE", &h.dir)
            .env("HOME", h.dir.join("home"))
            .output()
            .expect("spawn bash");
        (
            o.status.success(),
            format!(
                "{}{}",
                String::from_utf8_lossy(&o.stdout),
                String::from_utf8_lossy(&o.stderr)
            ),
        )
    };
    let slice = |n: usize| -> Vec<(&'static str, String)> {
        vec![
            ("SHA256_ENCRYPTED", photos.hashes[n - 1].clone()),
            ("ENCRYPTED_BYTES", photos.sizes[n - 1].to_string()),
            ("NUMBER", n.to_string()),
            ("YOUR_KEY.age.key", h.key("alice")),
        ]
    };
    let (ok, text) = run(&guide_block(&manual, "setblk 524288"), &[]);
    assert!(ok, "{text}");
    let (ok, text) = run(
        &guide_block(&manual, "FIRST_POSITION"),
        &[("FIRST_POSITION", photos.positions[0].to_string())],
    );
    assert!(ok, "{text}");
    let slice_block = guide_block(&manual, "> restore.NUMBER.dar");
    let (ok, text) = run(&slice_block, &slice(1));
    assert!(ok, "slice 1: {text}");
    // Slice 2 is skipped: P is slice 1's file, Q slice 3's.
    let (p, q) = (photos.positions[0], photos.positions[2]);
    let count = q - p - 1;
    assert_eq!(count, 1, "the arithmetic the guide shows");
    let (ok, text) = run(
        &guide_block(&manual, "fsf COUNT"),
        &[("COUNT", count.to_string())],
    );
    assert!(ok, "{text}");
    let (ok, text) = run(&slice_block, &slice(3));
    assert!(
        ok,
        "after the guide's spacing the head is at slice 3, whose sha256 says so: {text}"
    );
    assert!(work.join("restore.3.dar").exists());
    // The control: slice 4 is next; spacing to it with the file number, as the
    // old text had it, runs off to a file that is not slice 4.
    let (ok, text) = run(
        &[format!("mt -f /dev/nst0 fsf {}", photos.positions[3])],
        &[],
    );
    assert!(
        !ok || {
            let (ok, _) = run(&slice_block, &slice(4));
            !ok
        },
        "fsf of a file NUMBER from the middle of the tape does not reach that file: {text}"
    );
}

// ---- the gate's helpers for heir_where, heir_find and heir_slice_from_copy ----
//
// scripts/mhvtl-verify-gate.sh needs mhvtl and cannot run here, so the text work
// its three new legs rest on is extracted from the script by name and run on
// this harness's tape: the readers of RESTORE.sh's own output, and the dd
// stand-in that flips one byte of one tape file as it is read.

/// The host's python3 (the gate needs it too), for a stub on the harness PATH.
fn python3_path() -> String {
    host_tool("python3")
        .expect("python3 must be on PATH")
        .display()
        .to_string()
}

/// The text of the shell function `name` in the gate script: from `name() {` to
/// the first line that is just `}`.
fn gate_function(name: &str) -> String {
    let gate = std::fs::read_to_string(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("scripts/mhvtl-verify-gate.sh"),
    )
    .expect("the gate script");
    let start = gate
        .find(&format!("\n{name}() {{"))
        .unwrap_or_else(|| panic!("the gate has no function {name}"))
        + 1;
    let rest = &gate[start..];
    let end = rest.find("\n}\n").expect("the function ends") + 3;
    rest[..end].to_string()
}

/// Run `snippet` in bash with the gate's functions `names` defined, `stdin`
/// on its standard input.
fn run_gate_snippet(
    names: &[&str],
    snippet: &str,
    stdin: &str,
    env: &[(&str, &str)],
) -> (i32, String) {
    use std::io::Write;
    use std::process::Stdio;
    let mut script = String::new();
    for n in names {
        script.push_str(&gate_function(n));
    }
    script.push_str(snippet);
    let mut cmd = Command::new("bash");
    cmd.args(["-c", &script])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for (k, v) in env {
        cmd.env(k, v);
    }
    let mut child = cmd.spawn().unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(stdin.as_bytes())
        .unwrap();
    let o = child.wait_with_output().unwrap();
    (
        o.status.code().unwrap_or(-1),
        format!(
            "{}{}",
            String::from_utf8_lossy(&o.stdout),
            String::from_utf8_lossy(&o.stderr)
        ),
    )
}

#[test]
fn the_gates_readers_find_the_slices_in_restore_shs_real_output() {
    let h = Heir::new();
    let (code, info) = h.run(&["--info"]);
    assert_eq!(code, 0, "{info}");
    let (code, found) = h.run(&["--find-envelope", "--key", &h.key("bob")]);
    assert_eq!(code, 0, "{found}");

    let (code, out) = run_gate_snippet(
        &["heir_positions_of_type"],
        "heir_positions_of_type data_slice",
        &info,
        &[],
    );
    assert_eq!(code, 0, "{out}");
    let got: Vec<i64> = out.split_whitespace().map(|p| p.parse().unwrap()).collect();
    let mut want: Vec<i64> = template()
        .units
        .iter()
        .flat_map(|u| u.positions.clone())
        .collect();
    want.sort();
    assert!(want.len() >= 10, "positive control: many slices");
    assert_eq!(
        got, want,
        "every data slice of the file map, and nothing else:\n{info}"
    );
    let (_, out) = run_gate_snippet(
        &["heir_positions_of_type"],
        "heir_positions_of_type no_such_type",
        &info,
        &[],
    );
    assert_eq!(out.trim(), "", "a type the map lacks yields nothing");

    let ledgers = unit("ledgers");
    let (code, out) = run_gate_snippet(
        &["heir_slice_positions"],
        "heir_slice_positions ledgers",
        &found,
        &[],
    );
    assert_eq!(code, 0, "{out}");
    let got: Vec<i64> = out.split_whitespace().map(|p| p.parse().unwrap()).collect();
    assert_eq!(
        got, ledgers.positions,
        "ledgers' slices, in order:\n{found}"
    );
    let (_, out) = run_gate_snippet(
        &["heir_slice_positions"],
        "heir_slice_positions unit-not-there",
        &found,
        &[],
    );
    assert_eq!(out.trim(), "", "another unit's slices are not listed");
}

/// The stand-in flips a byte of the tape file at FAULT_POS as it is read, and
/// of no other, whether dd writes to a file or to a pipe; it logs each flip.
#[test]
fn the_gates_dd_stand_in_flips_one_tape_file_and_no_other() {
    let h = Heir::with_stubs(&[(
        "python3",
        &format!("#!/bin/sh\nexec {} \"$@\"\n", python3_path()),
    )]);
    let photos = unit("photos/2019");
    let inject = h.dir.join("inject");
    std::fs::create_dir_all(&inject).unwrap();
    let (code, out) = run_gate_snippet(
        &["heir_write_injector"],
        &format!("heir_write_injector {}/dd", inject.display()),
        "",
        &[],
    );
    assert_eq!(code, 0, "{out}");
    let bad = photos.positions[1];
    let log = h.dir.join("fault.log");
    let real_dd = h.dir.join("bin/dd");
    let real_mt = h.dir.join("bin/mt-st");
    // Files 0, 1, ... read in turn through the stand-in, as RESTORE.sh does.
    let read = |pos: i64, to_file: bool| -> Vec<u8> {
        let probe = format!(
            "export PATH={}:{}\n\
             {}\n\
             mt -f /dev/nst-fake rewind\n\
             {}\
             if [ {to_file} = true ]; then dd if=/dev/nst-fake of=$FAKE_TAPE/out.bin bs=512k 2>/dev/null; cat $FAKE_TAPE/out.bin; \
             else dd if=/dev/nst-fake bs=512k 2>/dev/null; fi\n",
            inject.display(),
            h.path,
            "true",
            if pos > 0 {
                format!("mt -f /dev/nst-fake fsf {pos}\n")
            } else {
                String::new()
            },
        );
        let o = Command::new("bash")
            .args(["-c", &probe])
            .env("FAKE_TAPE", &h.dir)
            .env("REAL_DD", &real_dd)
            .env("REAL_MT", &real_mt)
            .env("TAPE_DEVICE", "/dev/nst-fake")
            .env("FAULT_POS", bad.to_string())
            .env("FAULT_LOG", &log)
            .output()
            .unwrap();
        assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
        o.stdout
    };
    let pristine = |pos: i64| std::fs::read(h.dir.join("tape").join(pos.to_string())).unwrap();
    for to_file in [true, false] {
        let _ = std::fs::remove_file(&log);
        // Another file is read as it is.
        let other = photos.positions[0];
        assert_eq!(
            read(other, to_file),
            pristine(other),
            "file {other} untouched"
        );
        assert!(!log.exists(), "nothing was logged for it");
        // The named one has exactly one byte different.
        let got = read(bad, to_file);
        let was = pristine(bad);
        assert_eq!(got.len(), was.len());
        let diff: Vec<usize> = (0..got.len()).filter(|&i| got[i] != was[i]).collect();
        assert_eq!(diff, vec![100_000.min(got.len() - 1)], "to_file={to_file}");
        let logged = std::fs::read_to_string(&log).unwrap();
        assert!(
            logged.contains(&format!("flipped one byte of tape file {bad}")),
            "{logged}"
        );
    }
}

/// The gate's leg, against the fake tape: the stand-in makes one slice bad
/// as it is read, a directory holds the copy's slices under names that say
/// nothing, and RESTORE.sh --slices-from restores the unit; without the
/// directory the same run fails and names the option.
#[test]
fn the_gates_restore_with_the_dd_stand_in_and_a_copys_slices_works() {
    let t = template();
    let ledgers = unit("ledgers");
    let h = Heir::with_stubs(&[(
        "python3",
        &format!("#!/bin/sh\nexec {} \"$@\"\n", python3_path()),
    )]);
    let inject = h.dir.join("inject");
    std::fs::create_dir_all(&inject).unwrap();
    let (code, out) = run_gate_snippet(
        &["heir_write_injector"],
        &format!("heir_write_injector {}/dd", inject.display()),
        "",
        &[],
    );
    assert_eq!(code, 0, "{out}");
    // The copy's slices, as dd copies them off a tape: padded, nameless.
    let copies = h.dir.join("copies");
    std::fs::create_dir_all(&copies).unwrap();
    for (i, pos) in ledgers.positions.iter().enumerate() {
        std::fs::write(
            copies.join(format!("tape-file-{}", 40 + i)),
            std::fs::read(t.dir.join("tape").join(pos.to_string())).unwrap(),
        )
        .unwrap();
    }
    let bad = ledgers.positions[1];
    let log = h.dir.join("fault.log");
    let path = format!("{}:{}", inject.display(), h.path);
    let env: Vec<(&str, String)> = vec![
        ("PATH", path),
        ("REAL_DD", h.dir.join("bin/dd").display().to_string()),
        ("REAL_MT", h.dir.join("bin/mt-st").display().to_string()),
        ("FAULT_POS", bad.to_string()),
        ("FAULT_LOG", log.display().to_string()),
    ];
    let env_refs: Vec<(&str, &str)> = env.iter().map(|(k, v)| (*k, v.as_str())).collect();
    let (code, text) = h.run_env(
        &[
            "--restore",
            "--unit",
            "ledgers",
            "--key",
            &h.key("bob"),
            "--to",
            &h.sub("no-copy"),
        ],
        &env_refs,
    );
    assert_ne!(code, 0, "the bad slice stops the restore: {text}");
    assert!(log.exists(), "the stand-in fired");
    assert!(text.contains("--slices-from DIR"), "{text}");
    let _ = std::fs::remove_file(&log);
    let dest = h.sub("recovered");
    let (code, text) = h.run_env(
        &[
            "--restore",
            "--unit",
            "ledgers",
            "--key",
            &h.key("bob"),
            "--to",
            &dest,
            "--slices-from",
            &copies.display().to_string(),
        ],
        &env_refs,
    );
    assert_eq!(code, 0, "{text}");
    assert!(log.exists(), "the stand-in fired again");
    assert!(text.contains("RESTORE COMPLETE"), "{text}");
    assert!(
        text.contains(&format!(
            "taking slice 2 from {}/tape-file-",
            copies.display()
        )),
        "{text}"
    );
    assert!(same_tree(&ledgers.src, Path::new(&dest)), "{text}");
}

// ---- the restored files against their recorded sha256 (ADR-0012 item 103) ----

const CHK_UUID: &str = "abcdabcd-1111-2222-3333-444444444444";

/// One unit, `checked`, whose five files have the odd names of `ODD_NAMES`
/// (a TAB, a newline, a backslash, spaces and a colon) beside `sub/note.txt`.
/// The index records each file's hash as staged; the files in `tamper` are
/// changed in the source tree after that, before dar reads them, so dar's own
/// checksum is computed over the changed bytes and sees nothing wrong.
fn chk_tenants(tamper: &'static [usize]) -> Vec<TenantSpec> {
    vec![TenantSpec {
        name: "alice",
        units: vec![UnitSpec {
            cat: Cat::Matched,
            name: "checked",
            uuid: CHK_UUID,
            version: 1,
            seed: 41,
            files: 5,
            file_len: 80_000,
            names: ODD_NAMES,
            corrupt: &[],
            tamper,
        }],
    }]
}

/// Indexes into `ODD_NAMES` of the files `chk_bad` changes: every odd name,
/// not `plain.bin`.
const CHK_TAMPERED: &[usize] = &[1, 2, 3, 4];

/// The tape whose archive holds four files that differ from the hashes its own
/// index records.
fn chk_bad() -> &'static Template {
    static T: OnceLock<Template> = OnceLock::new();
    T.get_or_init(|| build_template("chk-bad", &chk_tenants(CHK_TAMPERED), true))
}

/// Another copy of the same unit and version, as staged: its files are the
/// ones the index hashes.
fn chk_good() -> &'static Template {
    static T: OnceLock<Template> = OnceLock::new();
    T.get_or_init(|| build_template("chk-good", &chk_tenants(&[]), true))
}

/// Two tenants, one unit each, both indexed; bob's `bravo-docs` has a changed
/// file. An escrow key restoring both opens the operator envelope, which has no
/// paths.tsv: each unit's check reads its own tenant's envelope.
fn chk_two() -> &'static Template {
    static T: OnceLock<Template> = OnceLock::new();
    T.get_or_init(|| {
        let unit = |name, uuid, seed, tamper| UnitSpec {
            cat: Cat::Matched,
            name,
            uuid,
            version: 1,
            seed,
            files: 3,
            file_len: 60_000,
            names: &[],
            corrupt: &[],
            tamper,
        };
        let tenants = vec![
            TenantSpec {
                name: "alice",
                units: vec![unit(
                    "alpha-docs",
                    "aaaaaaaa-0000-1111-2222-333333333333",
                    51,
                    &[],
                )],
            },
            TenantSpec {
                name: "bob",
                units: vec![unit(
                    "bravo-docs",
                    "bbbbbbbb-0000-1111-2222-333333333333",
                    52,
                    &[1],
                )],
            },
        ];
        build_template("chk-two", &tenants, true)
    })
}

fn damaged_list(dir: &str) -> (String, Vec<String>) {
    let text = std::fs::read_to_string(Path::new(dir).join(".tapectl-damaged-files.txt")).unwrap();
    let mut lines = text.lines();
    let header = lines.next().unwrap().to_string();
    let mut paths: Vec<String> = lines.map(str::to_string).collect();
    paths.sort();
    (header, paths)
}

fn odd_escaped(indexes: &[usize]) -> Vec<String> {
    let mut v: Vec<String> = indexes.iter().map(|&i| idx_esc(ODD_NAMES[i])).collect();
    v.sort();
    v
}

/// A clean restore passes, says how many files it checked, reads the tape
/// exactly as it did before (forward only, one rewind: the check reads the
/// restored files from disk), and the figure counts every regular file of the
/// unit, the ones named with a TAB, a newline and a backslash included.
#[test]
fn a_clean_restore_passes_the_file_check_and_counts_what_it_checked() {
    let h = Heir::of(chk_good(), &[]);
    let checked = unit_of(chk_good(), "checked");
    let dest = h.sub("restored");
    let (code, text) = runv(&h, &restore_args(&h, "checked", &dest));
    assert_eq!(code, 0, "{text}");
    assert!(text.contains("RESTORE COMPLETE"), "{text}");
    assert!(
        text.contains(
            "File check: 6 restored file(s) of 'checked' version 1 match the sha256 recorded in the index"
        ),
        "five files and sub/note.txt, none skipped:\n{text}"
    );
    assert!(same_tree(&checked.src, Path::new(&dest)), "{text}");
    assert!(!Path::new(&dest).join(".tapectl-damaged-files.txt").exists());
    assert_forward_only(&h, "a checked restore");
    let with_check = h.ops();

    // The same restore without the check reads the same tape the same way.
    let h2 = Heir::of(chk_good(), &[]);
    let dest2 = h2.sub("restored");
    let mut args = restore_args(&h2, "checked", &dest2);
    args.push("--no-file-check".to_string());
    let (code, text) = runv(&h2, &args);
    assert_eq!(code, 0, "{text}");
    assert_eq!(h2.ops(), with_check, "the check moves the tape not at all");
}

/// The test of the whole item. Four files come back from dar with no error
/// (dar's checksum is computed over the changed bytes, so it agrees), and the
/// recorded sha256 finds them: the run ends INCOMPLETE and the list names them,
/// each once, escaped as the index escapes a path. A second copy of the same
/// unit and version, as staged, fills exactly those files, checks them, and
/// the restore is whole.
#[test]
fn a_file_dar_cannot_see_is_found_by_its_hash_and_filled_from_another_copy() {
    let bad = Heir::of(chk_bad(), &[]);
    let good_src = unit_of(chk_good(), "checked").src;
    let dest = bad.sub("restored");
    let (code, text) = runv(&bad, &restore_args(&bad, "checked", &dest));
    println!("--- the restore from the tape with four changed files ---\n{text}");
    assert_eq!(code, 1, "{text}");
    assert!(
        !text.contains("Bad CRC") && !text.contains("Error while restoring"),
        "positive control: dar saw nothing wrong:\n{text}"
    );
    assert!(text.contains("RESTORE INCOMPLETE"), "{text}");
    assert!(!text.contains("RESTORE COMPLETE"), "{text}");
    assert!(
        text.contains("File check: 4 of 6 file(s) of 'checked' version 1 do NOT match their recorded sha256, or are missing"),
        "{text}"
    );
    assert!(
        text.contains(
            "Unit 'checked' (version 1): 4 file(s) do not match the sha256 the index records"
        ) && text.contains("dar named none of them"),
        "{text}"
    );
    let (header, listed) = damaged_list(&dest);
    assert_eq!(
        header,
        format!("#tapectl-damaged-files\t1\tunit=checked\tuuid={CHK_UUID}\tversion=1")
    );
    assert_eq!(listed, odd_escaped(CHK_TAMPERED), "{text}");
    let mut by_diff: Vec<String> = damaged_by_diff(&good_src, Path::new(&dest))
        .into_iter()
        .filter(|p| p != ".tapectl-damaged-files.txt")
        .map(|p| idx_esc(&p))
        .collect();
    by_diff.sort();
    assert_eq!(listed, by_diff, "the list is exactly the files that differ");
    assert!(
        listed.iter().any(|l| l.contains("\\n"))
            && listed.iter().any(|l| l.contains("\\t"))
            && listed.iter().any(|l| l.contains("\\\\")),
        "a newline, a TAB and a backslash are in the list, escaped: {listed:?}"
    );
    assert!(text.contains("--fill-from"), "the fill command: {text}");

    // The fill, from a copy whose files are the ones the index hashes.
    let list = Path::new(&dest).join(".tapectl-damaged-files.txt");
    let other = Heir::of(chk_good(), &[]);
    let (code, text) = runv(&other, &fill_args(&other, "checked", &list, &dest));
    println!("--- the fill from another copy ---\n{text}");
    assert_eq!(code, 0, "{text}");
    assert!(
        text.contains("File check: 4 restored file(s) of 'checked' version 1 match the sha256 recorded in the index"),
        "{text}"
    );
    assert!(text.contains("FILL COMPLETE"), "{text}");
    assert!(!list.exists(), "the list is removed");
    assert!(same_tree(&good_src, Path::new(&dest)), "{text}");
}

/// A fill takes its files from a copy, and a copy whose files are not the ones
/// the index hashes does not repair them: the filled files are checked too,
/// stay in the list, and the fill ends INCOMPLETE.
#[test]
fn a_fill_whose_files_still_fail_their_hash_stays_incomplete() {
    let a = Heir::of(chk_bad(), &[]);
    let dest = a.sub("restored");
    let (code, text) = runv(&a, &restore_args(&a, "checked", &dest));
    assert_eq!(code, 1, "{text}");
    let list = Path::new(&dest).join(".tapectl-damaged-files.txt");
    let before = damaged_list(&dest).1;
    assert_eq!(before.len(), 4);

    let again = Heir::of(chk_bad(), &[]);
    let (code, text) = runv(&again, &fill_args(&again, "checked", &list, &dest));
    assert_eq!(code, 1, "{text}");
    assert!(text.contains("FILL INCOMPLETE: 4 file(s)"), "{text}");
    assert!(
        text.contains("do NOT match their recorded sha256"),
        "{text}"
    );
    assert_eq!(damaged_list(&dest).1, before, "the same four are listed");
}

/// `--no-file-check` runs the restore the old way: dar's checksum alone, the
/// changed files come back and the run says COMPLETE, and it says it did not
/// check. (The positive control for the item: without the hash check this
/// damage goes through.)
#[test]
fn no_file_check_skips_it_and_says_so() {
    let h = Heir::of(chk_bad(), &[]);
    let dest = h.sub("restored");
    let mut args = restore_args(&h, "checked", &dest);
    args.push("--no-file-check".to_string());
    let (code, text) = runv(&h, &args);
    assert_eq!(code, 0, "{text}");
    assert!(text.contains("RESTORE COMPLETE"), "{text}");
    assert!(
        text.contains("File check skipped (--no-file-check)"),
        "{text}"
    );
    assert!(!text.contains("File check: "), "{text}");
    assert!(!Path::new(&dest).join(".tapectl-damaged-files.txt").exists());
    let good_src = unit_of(chk_good(), "checked").src;
    assert_eq!(
        damaged_by_diff(&good_src, Path::new(&dest)).len(),
        4,
        "the four changed files were restored without a word"
    );
    // And it is in --help.
    let (_, help) = h.run(&["--help"]);
    assert!(help.contains("--no-file-check"), "{help}");
}

/// A tape written before 1.3.0 has no index: the check is skipped, once for
/// the whole run however many units it restores, with the reason.
#[test]
fn a_tape_with_no_index_skips_the_check_and_says_so_once() {
    let h = Heir::lone();
    let dest = h.sub("restored");
    let (code, text) = h.run(&[
        "--restore",
        "--key",
        &h.key("alice"),
        "--all",
        "--to",
        &dest,
    ]);
    assert_eq!(code, 0, "{text}");
    assert!(text.contains("RESTORE COMPLETE"), "{text}");
    assert_eq!(
        text.matches("this tape carries no index").count(),
        1,
        "said once for three units:\n{text}"
    );
    assert!(
        !text.contains("match the sha256"),
        "no file was checked: {text}"
    );
    assert!(text.contains("version 1.3.0"), "{text}");
}

/// An escrow key restoring every unit opens the operator envelope, which has
/// no paths.tsv: each unit is checked against the tenant envelope that lists
/// it. Bob's unit has a changed file and alice's has none, so the list is
/// bob's alone; the operator key reaching bob's unit by name does the same.
#[test]
fn an_escrow_or_operator_key_checks_against_the_tenant_envelopes_index() {
    let h = Heir::of(chk_two(), &[]);
    let dest = h.sub("restored");
    let (code, text) = h.run(&["--restore", "--key", &h.key("esc"), "--all", "--to", &dest]);
    println!("--- --all with the escrow key ---\n{text}");
    assert_eq!(code, 1, "{text}");
    assert!(
        text.contains("Decrypted envelope at file 6"),
        "the operator envelope is the one in use:\n{text}"
    );
    assert!(
        text.contains("File check: 4 restored file(s) of 'alpha-docs' version 1 match"),
        "three files and sub/note.txt:\n{text}"
    );
    assert!(
        text.contains("File check: 1 of 4 file(s) of 'bravo-docs' version 1 do NOT match"),
        "{text}"
    );
    assert!(text.contains("RESTORE INCOMPLETE"), "{text}");
    assert!(!Path::new(&dest)
        .join("alpha-docs")
        .join(".tapectl-damaged-files.txt")
        .exists());
    let (header, listed) = damaged_list(&format!("{dest}/bravo-docs"));
    assert!(header.contains("unit=bravo-docs"), "{header}");
    assert_eq!(listed, vec!["file1.bin".to_string()]);

    let o = Heir::of(chk_two(), &[]);
    let dest = o.sub("restored");
    let (code, text) = o.run(&[
        "--restore",
        "--key",
        &o.key("op"),
        "--unit",
        "bravo-docs",
        "--to",
        &dest,
    ]);
    assert_eq!(code, 1, "{text}");
    assert_eq!(damaged_list(&dest).1, vec!["file1.bin".to_string()]);
}

/// `--path` checks only what it restored: a changed file elsewhere in the
/// unit is not looked at, a folder is checked file by file, and a changed file
/// asked for by name is found.
#[test]
fn a_path_restore_checks_only_the_paths_it_restored() {
    let h = Heir::of(chk_bad(), &[]);
    let path_args = |dest: &str, path: &str| {
        let mut a = restore_args(&h, "checked", dest);
        a.extend(["--path".to_string(), path.to_string()]);
        a
    };

    let dest = h.sub("one-good");
    let (code, text) = runv(&h, &path_args(&dest, "plain.bin"));
    assert_eq!(code, 0, "{text}");
    assert!(
        text.contains("File check: 1 restored file(s) of 'checked' version 1 match"),
        "the one file asked for, not the four changed ones:\n{text}"
    );

    let dest = h.sub("folder");
    let (code, text) = runv(&h, &path_args(&dest, "sub"));
    assert_eq!(code, 0, "{text}");
    assert!(
        text.contains("File check: 1 restored file(s) of 'checked' version 1 match"),
        "sub/note.txt:\n{text}"
    );

    let dest = h.sub("one-bad");
    let (code, text) = runv(&h, &path_args(&dest, "we ird : name.bin"));
    assert_eq!(code, 1, "{text}");
    assert!(text.contains("RESTORE INCOMPLETE"), "{text}");
    assert_eq!(damaged_list(&dest).1, odd_escaped(&[1]));
    assert!(
        text.contains("File check: 1 of 1 file(s) of 'checked' version 1 do NOT match"),
        "{text}"
    );
}

/// The same unit and version, indexed by an older 1.3.0 writer that wrote no
/// `sha256` column. Nothing can be checked against it, and the script says so
/// instead of calling the files good.
fn chk_old_index() -> &'static Template {
    static T: OnceLock<Template> = OnceLock::new();
    T.get_or_init(|| build_template_with("chk-old", &chk_tenants(CHK_TAMPERED), true, false))
}

#[test]
fn an_index_with_no_sha256_column_checks_nothing_and_says_so() {
    let h = Heir::of(chk_old_index(), &[]);
    let dest = h.sub("restored");
    let (code, text) = runv(&h, &restore_args(&h, "checked", &dest));
    assert_eq!(code, 0, "{text}");
    assert!(text.contains("RESTORE COMPLETE"), "{text}");
    assert!(
        text.contains(
            "the index records no sha256 for the 6 file(s) of 'checked' version 1: not checked"
        ),
        "{text}"
    );
    assert!(!text.contains("match the sha256 recorded"), "{text}");
}

/// A file with no recorded hash is not checked, and the run says how many
/// it passed over: `photos/2019` has two regular files in its index (made-up
/// rows) with no hash.
#[test]
fn files_with_no_recorded_hash_are_counted_not_checked() {
    let h = Heir::new();
    let photos = unit("photos/2019");
    let dest = h.sub("restored");
    let (code, text) = h.run(&[
        "--restore",
        "--key",
        &h.key("alice"),
        "--unit",
        "photos/2019",
        "--to",
        &dest,
    ]);
    assert_eq!(code, 0, "{text}");
    assert!(text.contains("RESTORE COMPLETE"), "{text}");
    assert!(
        text.contains("File check: 6 restored file(s) of 'photos/2019' version 2 match the sha256 recorded in the index (2 more not checked: no sha256 recorded)"),
        "{text}"
    );
    assert!(same_tree(&photos.src, Path::new(&dest)), "{text}");
}

/// A slice that is not there: dar names the files in it, and the check finds
/// the same files missing. Each is on the list once, and a fill from a good
/// copy makes the unit whole.
#[test]
fn a_file_dar_and_the_check_both_find_is_listed_once() {
    let a = Heir::of(chk_good(), &[]);
    let checked = unit_of(chk_good(), "checked");
    flip_in_tape(&a, checked.positions[1]);
    let empty = a.sub("empty");
    std::fs::create_dir_all(&empty).unwrap();
    let dest = a.sub("restored");
    let (code, text) = a.run(&[
        "--restore",
        "--key",
        &a.key("alice"),
        "--unit",
        "checked",
        "--to",
        &dest,
        "--slices-from",
        &empty,
    ]);
    assert_eq!(code, 1, "{text}");
    assert!(text.contains("RESTORE INCOMPLETE"), "{text}");
    assert!(text.contains("File check: "), "{text}");
    assert!(text.contains("do NOT match"), "{text}");
    let (_, listed) = damaged_list(&dest);
    let mut unique = listed.clone();
    unique.dedup();
    assert_eq!(listed, unique, "each file once: {listed:?}");
    let mut by_diff: Vec<String> = damaged_by_diff(&checked.src, Path::new(&dest))
        .into_iter()
        .filter(|p| p != ".tapectl-damaged-files.txt")
        .map(|p| idx_esc(&p))
        .collect();
    by_diff.sort();
    assert_eq!(
        listed, by_diff,
        "everything missing or short is listed:\n{text}"
    );
    assert!(listed.len() >= 2, "a positive control: {listed:?}");

    let list = Path::new(&dest).join(".tapectl-damaged-files.txt");
    let b = Heir::of(chk_good(), &[]);
    let (code, text) = runv(&b, &fill_args(&b, "checked", &list, &dest));
    assert_eq!(code, 0, "{text}");
    assert!(text.contains("FILL COMPLETE"), "{text}");
    assert!(same_tree(&checked.src, Path::new(&dest)), "{text}");
}
