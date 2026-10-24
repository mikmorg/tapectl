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
    generate_front_index, generate_id_thunk_v2, generate_id_thunk_v2_requiring,
    generate_manifest_toml, generate_recovery_md, generate_restore_script_v2, generate_seal_marker,
    generate_short_seal_marker, generate_system_guide_v2, FrontIndexFile, IdThunkV2Params,
    ManifestSlice, ManifestUnit, REQUIRES_SPAN,
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
///
/// The loaded cartridge is `tape`, or, when the file `loaded` exists, the
/// directory it names (a box of tapes, `Heir::of_box`; the test loads another
/// between prompts). With `FAKE_PROCS` set, each `mt` and `dd` writes a line
/// `start PID ARGS` there when it starts and `end PID` when it ends, so a test
/// can tell whether one was running while the script waited for an answer.
const FAKE_MT: &str = r#"#!/bin/sh
T="$FAKE_TAPE"
D="$T/tape"
[ ! -f "$T/loaded" ] || D="$T/$(cat "$T/loaded")"
if [ -n "${FAKE_PROCS:-}" ]; then
  echo "start $$ mt $*" >>"$FAKE_PROCS"
  trap 'echo "end $$" >>"$FAKE_PROCS"' EXIT
fi
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
count=$(ls "$D" | wc -l)
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
D="$T/tape"
[ ! -f "$T/loaded" ] || D="$T/$(cat "$T/loaded")"
if [ -n "${FAKE_PROCS:-}" ]; then
  echo "start $$ dd $*" >>"$FAKE_PROCS"
  trap 'echo "end $$" >>"$FAKE_PROCS"' EXIT
fi
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
f="$D/$pos"
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
    /// How many slices the unit has, on this tape or not (a span unit's
    /// `positions` are only the run this tape holds).
    pub slices: usize,
}

/// A built fake volume, kept as a template and copied per test.
struct Template {
    dir: PathBuf,
    units: Vec<UnitOnTape>,
    total_files: i32,
    /// The volume's label (File 0's), which its RESTORE.sh is made for.
    label: &'static str,
}

/// Which tapectl wrote a fake tape: what it declares up front (File 0's
/// `requires`, File 3's `may_end_early` and its paragraph, each envelope's
/// `may_end_early`) and what its seal states (volume-format-v2.md §1.7, §4.1;
/// ADR-0012 item 111). The script under test is always this tapectl's.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Era {
    /// 1.1.0 to 1.5.0: `requires = []` everywhere, no `ended_after`, no
    /// `may_end_early`.
    V150,
    /// 1.6.0: `requires = ["short-seal"]` in File 0 and the seal, every seal
    /// states `ended_after`, File 3 and the envelopes state `may_end_early`.
    /// A tape 1.7.0 writes that holds no slice of a unit that may span is
    /// this tape (its Files 1 and 2 aside).
    V160,
    /// 1.7.0, a span tape (ADR-0012 item 124): as 1.6.0, but File 0 and the
    /// seal state `requires = ["short-seal", "spanning"]`.
    V170Span,
}

/// The `requires` line of File 0 and the seal on a span tape (§1.2).
const SPAN_REQUIRES: &str = "requires = [\"short-seal\", \"spanning\"]";

/// File 3's paragraph from 1.6.0 (volume-format-v2.md §3), with the blank line
/// after it.
const MAY_END_EARLY_PARAGRAPH: &str =
    "This tape may end early. The list below is the plan: every file the\n\
     write meant to put on the tape. The seal marker, the LAST file on the\n\
     tape, says where the tape really ended (its ended_after). A file listed\n\
     after that position is not on this tape.\n\n";

/// File 0 as `era` writes it.
fn era_file0(era: Era, text: String) -> String {
    if era == Era::V170Span {
        assert_eq!(
            text.matches(&format!("\n{SPAN_REQUIRES}\n")).count(),
            1,
            "this tapectl's File 0 of a span tape:\n{text}"
        );
        return text;
    }
    let short = "\nrequires = [\"short-seal\"]\n";
    assert_eq!(
        text.matches(short).count(),
        1,
        "this tapectl's File 0:\n{text}"
    );
    match era {
        Era::V160 | Era::V170Span => text,
        Era::V150 => text.replace(short, "\nrequires = []\n"),
    }
}

/// File 3 as `era` writes it.
fn era_front_index(era: Era, text: String) -> String {
    assert_eq!(text.matches(MAY_END_EARLY_PARAGRAPH).count(), 1, "{text}");
    assert_eq!(
        text.matches("\nmay_end_early = true\n").count(),
        1,
        "{text}"
    );
    match era {
        Era::V160 | Era::V170Span => text,
        Era::V150 => text
            .replace(MAY_END_EARLY_PARAGRAPH, "")
            .replace("\nmay_end_early = true\n", "\n"),
    }
}

/// An envelope's MANIFEST.toml as `era` writes it.
fn era_manifest(era: Era, text: String) -> String {
    assert_eq!(
        text.matches("\nmay_end_early = true\n").count(),
        1,
        "{text}"
    );
    match era {
        Era::V160 | Era::V170Span => text,
        Era::V150 => text.replace("\nmay_end_early = true\n", "\n"),
    }
}

/// A seal marker with its `[seal]` table as `era` writes it: 1.6.0's
/// `requires = ["short-seal"]` and `ended_after = file_count - 2` (§4.1, the
/// key after `file_count`), or 1.5.0's `requires = []` and no `ended_after`.
/// Whatever the generator wrote on those two lines is replaced, so a fixture
/// holds its era's bytes whether or not the generator writes 1.6.0 seals yet;
/// the header and the `[[files]]` entries are the generator's.
fn era_seal(era: Era, text: &str) -> String {
    let mut out = String::new();
    let mut in_seal = false;
    let mut requires = 0;
    for line in text.lines() {
        if line == "[seal]" {
            in_seal = true;
        } else if line.starts_with("[[files]]") {
            in_seal = false;
        }
        if in_seal && line.starts_with("requires = ") {
            requires += 1;
            out.push_str(match era {
                Era::V150 => "requires = []\n",
                Era::V160 => "requires = [\"short-seal\"]\n",
                Era::V170Span => "requires = [\"short-seal\", \"spanning\"]\n",
            });
            continue;
        }
        if in_seal && line.starts_with("ended_after = ") {
            continue;
        }
        out.push_str(line);
        out.push('\n');
        if in_seal && era != Era::V150 {
            if let Some(n) = line.strip_prefix("file_count = ") {
                let n: i64 = n.parse().unwrap();
                out.push_str(&format!("ended_after = {}\n", n - 2));
            }
        }
    }
    assert_eq!(requires, 1, "one requires line in [seal]:\n{text}");
    out
}

/// The `[[files]]` entries of the front index on a fake tape directory, and
/// File 3's own bytes (block padding stripped: plain text has no NUL).
fn plan_on_tape(dir: &Path) -> (Vec<FrontIndexFile>, Vec<u8>) {
    let fi: Vec<u8> = std::fs::read(dir.join("3"))
        .unwrap()
        .into_iter()
        .filter(|b| *b != 0)
        .collect();
    let text = String::from_utf8(fi.clone()).unwrap();
    let mut entries: Vec<FrontIndexFile> = Vec::new();
    for line in text.lines() {
        if line == "[[files]]" {
            entries.push(FrontIndexFile {
                position: -1,
                type_label: "",
                size_bytes: None,
                sha256_encrypted: None,
            });
            continue;
        }
        let Some(e) = entries.last_mut() else {
            continue;
        };
        if let Some(v) = line.strip_prefix("position = ") {
            e.position = v.parse().unwrap();
        } else if let Some(v) = line.strip_prefix("type = ") {
            e.type_label = match v.trim_matches('"') {
                "id_thunk" => "id_thunk",
                "system_guide" => "system_guide",
                "restore_sh" => "restore_sh",
                "front_index" => "front_index",
                "tenant_envelope" => "tenant_envelope",
                "operator_envelope" => "operator_envelope",
                "operator_envelope_backup" => "operator_envelope_backup",
                "tenant_envelope_backup" => "tenant_envelope_backup",
                "data_slice" => "data_slice",
                "seal_marker" => "seal_marker",
                other => panic!("no such type {other}"),
            };
        } else if let Some(v) = line.strip_prefix("size_bytes = ") {
            e.size_bytes = Some(v.parse().unwrap());
        } else if let Some(v) = line.strip_prefix("sha256_encrypted = ") {
            e.sha256_encrypted = Some(v.trim_matches('"').to_string());
        }
    }
    for (i, e) in entries.iter().enumerate() {
        assert_eq!(e.position, i as i32, "the plan's entries in order");
    }
    (entries, fi)
}

/// End the fake tape in `dir` early, as a 1.6.0 write that met the drive's
/// early warning in data slice `k` does (volume-format-v2.md §4.1, §4.3): the
/// files up to `k` stay, a short seal is written at `k + 1` (file_count
/// `k + 2`, ended_after `k`, its copy of the front index up to `k` with File
/// 3's own entry filled in, then its own entry), and every planned file after
/// it is gone. File 0, File 3 and the envelopes stay the plan.
fn cut_tape_dir(dir: &Path, label: &str, k: i64) {
    // The seal states File 0's features: a span tape's states both (§4.1).
    let file0 = std::fs::read(dir.join("0")).unwrap();
    let era = if String::from_utf8_lossy(&file0).contains(SPAN_REQUIRES) {
        Era::V170Span
    } else {
        Era::V160
    };
    let (mut plan, fi) = plan_on_tape(dir);
    let last = plan.len() as i64 - 1;
    assert!(k <= last - 2, "a short seal comes before the planned one");
    assert_eq!(plan[k as usize].type_label, "data_slice", "S5");
    let fi_sha = sha256_hex(&fi);
    plan[3].size_bytes = Some(fi.len() as u64);
    plan[3].sha256_encrypted = Some(fi_sha.clone());
    let mut copy: Vec<FrontIndexFile> = plan[..=k as usize].to_vec();
    copy.push(FrontIndexFile {
        position: (k + 1) as i32,
        type_label: "seal_marker",
        size_bytes: None,
        sha256_encrypted: None,
    });
    let seal = era_seal(
        era,
        &generate_seal_marker(label, (k + 2) as i32, &fi_sha, &copy),
    );
    assert!(seal.contains(&format!("\nfile_count = {}\nended_after = {k}\n", k + 2)));
    std::fs::write(dir.join((k + 1).to_string()), padded(seal.into_bytes())).unwrap();
    for p in k + 2..=last {
        std::fs::remove_file(dir.join(p.to_string())).unwrap();
    }
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
/// operator envelope and its backup, and the seal marker. A 1.6.0 tape.
fn template() -> &'static Template {
    static T: OnceLock<Template> = OnceLock::new();
    T.get_or_init(|| build_template_full("main", &main_tenants(), true, true, &[], Era::V160))
}

/// The same volume as a 1.5.0 tapectl wrote it: what this script must still
/// read exactly as before (volume-format-v2.md §6 "Since 1.6.0").
fn template_150() -> &'static Template {
    static T: OnceLock<Template> = OnceLock::new();
    T.get_or_init(|| build_template_full("main150", &main_tenants(), true, true, &[], Era::V150))
}

fn main_tenants() -> Vec<TenantSpec> {
    {
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
        tenants
    }
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

/// A volume whose index names far more units than this tape holds, and more
/// than the 40 the miss-path listing used to stop at (the box scenario of
/// `--where --all`): alice's `docs` and bob's `ledgers` are really on this tape
/// (and `docs` also on OTHER02); every other unit is a [`Phantom`], held on
/// other tapes only. 54 units in all: alice 50 (`docs`, 45 `archive/unit-NNN`,
/// `dup` twice under two uuids, `tens`, and a name holding a TAB, an LF and a
/// backslash) and bob 4 (`ledgers`, `bob/old-N` x 3).
fn box_template() -> &'static Template {
    static T: OnceLock<Template> = OnceLock::new();
    T.get_or_init(|| {
        let real = |name, uuid, version, seed| UnitSpec {
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
                name: "alice",
                units: vec![real("docs", "22222222-3333-4444-5555-666666666666", 1, 12)],
            },
            TenantSpec {
                name: "bob",
                units: vec![real(
                    "ledgers",
                    "33333333-4444-5555-6666-777777777777",
                    3,
                    13,
                )],
            },
        ];
        let mut phantoms = Vec::new();
        for i in 1..=45 {
            phantoms.push(Phantom {
                tenant: "alice",
                name: format!("archive/unit-{i:03}"),
                uuid: format!("aaaaaaaa-0000-4000-8000-{i:012}"),
                // Every third unit has only a version 1; the rest have two.
                versions: if i % 3 == 0 { vec![1] } else { vec![1, 2] },
                // Every tenth has a second, quarantined copy of its newest version.
                quarantined_copy: i % 10 == 0,
            });
        }
        for i in 1..=3 {
            phantoms.push(Phantom {
                tenant: "bob",
                name: format!("bob/old-{i}"),
                uuid: format!("bbbbbbbb-0000-4000-8000-{i:012}"),
                versions: vec![1],
                quarantined_copy: false,
            });
        }
        // One name made twice: two units, and their rows interleave in the file
        // (dup v1 of A, dup v1 of B, dup v2 of A).
        phantoms.push(Phantom {
            tenant: "alice",
            name: "dup".into(),
            uuid: "dddddddd-0000-4000-8000-000000000001".into(),
            versions: vec![1, 2],
            quarantined_copy: false,
        });
        phantoms.push(Phantom {
            tenant: "alice",
            name: "dup".into(),
            uuid: "dddddddd-0000-4000-8000-000000000002".into(),
            versions: vec![1],
            quarantined_copy: false,
        });
        // A name with a TAB, an LF and a backslash: one heading line, shown escaped.
        phantoms.push(Phantom {
            tenant: "alice",
            name: "weird\ttab\nnew\\back".into(),
            uuid: "ffffffff-0000-4000-8000-000000000001".into(),
            versions: vec![1],
            quarantined_copy: false,
        });
        // Versions 2, 9 and 10: the newest by number is 10.
        phantoms.push(Phantom {
            tenant: "alice",
            name: "tens".into(),
            uuid: "eeeeeeee-0000-4000-8000-000000000001".into(),
            versions: vec![2, 9, 10],
            quarantined_copy: false,
        });
        build_template_full("box", &tenants, true, true, &phantoms, Era::V160)
    })
}

// ---- the cross-volume index (volume-format-v2.md 1.5), built from the spec ----

/// One `index/units.tsv` row: a (version, holding volume) pair.
#[derive(Clone)]
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

/// A unit the index names that this tape does not hold: it lives on another
/// tape of the box (the case `--where --all` exists for). Its rows go into its
/// tenant's `units.tsv` and the operator's, one row for each version and
/// holding tape, and no slice of it is on this tape.
struct Phantom {
    tenant: &'static str,
    name: String,
    uuid: String,
    versions: Vec<i64>,
    /// Also held by the quarantined tape (its newest version).
    quarantined_copy: bool,
}

/// `hash_column` false writes the index as a 1.3.0 build from before the
/// `sha256` column wrote it (ADR-0012 item 103 added it).
fn build_template_with(
    tag: &str,
    tenants: &[TenantSpec],
    with_index: bool,
    hash_column: bool,
) -> Template {
    build_template_full(tag, tenants, with_index, hash_column, &[], Era::V160)
}

/// The phantom units' rows: each version on the sealed tape OTHER02, and the
/// newest also on the quarantined tape when the unit says so.
fn phantom_rows(p: &Phantom) -> Vec<IdxRow> {
    let mut rows = Vec::new();
    let newest = p.versions.iter().copied().max().unwrap_or(0);
    for &v in &p.versions {
        let mut row = |volume: &'static str,
                       volume_uuid: &'static str,
                       serial: &'static str,
                       generation: &'static str,
                       state: &'static str,
                       sealed_at: &'static str| {
            rows.push(IdxRow {
                tenant: p.tenant,
                unit: p.name.clone(),
                unit_uuid: p.uuid.clone(),
                version: v,
                num_slices: 3,
                first: 1,
                last: 3,
                bytes: 3_000_000 + v,
                volume,
                volume_uuid,
                serial,
                generation,
                state,
                sealed_at,
            });
        };
        row(
            "OTHER02",
            OTHER_UUID,
            "OTHERSER02",
            "LTO-6",
            "sealed",
            "2026-09-01T10:00:00Z",
        );
        if p.quarantined_copy && v == newest {
            row(
                "QUAR03",
                QUAR_UUID,
                "",
                "LTO-5",
                "quarantined",
                "2026-08-15T09:30:00Z",
            );
        }
    }
    rows
}

fn build_template_full(
    tag: &str,
    tenants: &[TenantSpec],
    with_index: bool,
    hash_column: bool,
    phantoms: &[Phantom],
    era: Era,
) -> Template {
    build_template_spans(tag, tenants, with_index, hash_column, phantoms, era, &[])
}

/// A unit held across tapes on a fake span tape (ADR-0012 items 124-125): its
/// MANIFEST.toml block states `may_span = true` and lists every slice of it,
/// and only the slices `held` (0-based, one unbroken run) are on this tape,
/// each with its `tape_position`. A slice before the run is on OTHER02, which
/// this tape's index names with its run (an index names only tapes written
/// before its own); a slice after it is on no tape the index knows.
#[derive(Clone)]
struct SpanOnTape {
    unit: &'static str,
    held: std::ops::Range<usize>,
}

fn build_template_spans(
    tag: &str,
    tenants: &[TenantSpec],
    with_index: bool,
    hash_column: bool,
    phantoms: &[Phantom],
    era: Era,
    spans: &[SpanOnTape],
) -> Template {
    assert_eq!(
        era == Era::V170Span,
        !spans.is_empty(),
        "a span tape, and only a span tape, holds a unit that may span"
    );
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(format!(
        "heir-restore-sh-template-{tag}-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    for sub in ["tape", "src", "keys", "work"] {
        std::fs::create_dir_all(dir.join(sub)).unwrap();
    }
    write_keys(&dir);
    let staged = stage_units(&dir, tenants, spans);
    lay_out_tape(
        &dir,
        staged,
        with_index,
        hash_column,
        phantoms,
        era,
        spans,
        &THIS_TAPE,
    )
}

fn write_keys(dir: &Path) {
    std::fs::create_dir_all(dir.join("keys")).unwrap();
    for k in ["alice", "bob", "carol", "op", "esc", "stranger"] {
        std::fs::write(dir.join("keys").join(format!("{k}.key")), k).unwrap();
    }
}

/// One unit as staged: its slices (plaintext), its isolated catalogue, its
/// source tree. Every tape laid out from one staging carries the same bytes.
#[derive(Clone)]
struct Staged {
    tenant: &'static str,
    unit: ManifestUnit,
    slices: Vec<Vec<u8>>,
    catalogue: Option<Vec<u8>>,
    src: PathBuf,
    /// The sha256 of each file as staged: what the index records.
    hashes: BTreeMap<String, String>,
}

/// The volume a layout is written as, and what its index says of the tapes
/// written before it.
struct TapeId {
    label: &'static str,
    uuid: &'static str,
    serial: &'static str,
    /// The other holders this tape's index names for a span unit, each with
    /// its run. `None`: OTHER02, holding the slices before this tape's run.
    earlier: Option<Vec<Holder>>,
}

/// A tape an index names as holding a run of a span unit's slices.
#[derive(Clone)]
struct Holder {
    label: &'static str,
    uuid: &'static str,
    serial: &'static str,
    /// The run, 1-based and inclusive.
    first: i64,
    last: i64,
}

/// The volume every single-tape template is: `HEIR01`.
const THIS_TAPE: TapeId = TapeId {
    label: LABEL,
    uuid: THIS_UUID,
    serial: "SERIAL01",
    earlier: None,
};

/// dar every unit: its slices (plaintext) and its isolated catalogue, in `dir`'s
/// `src` and `work`. A unit `spans` names is staged as one that may span.
fn stage_units(dir: &Path, tenants: &[TenantSpec], spans: &[SpanOnTape]) -> Vec<Staged> {
    let span_of = |name: &str| spans.iter().find(|sp| sp.unit == name).cloned();
    let mut staged: Vec<Staged> = Vec::new();
    for t in tenants {
        for u in &t.units {
            let src = dir.join("src").join(u.name);
            // A second version of a unit starts from an empty tree, so a file
            // the first version had can be missing from it.
            let _ = std::fs::remove_dir_all(&src);
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
            let work = dir.join("work").join(format!("{}-v{}", u.uuid, u.version));
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
                    may_span: span_of(u.name).is_some(),
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
    staged
}

/// Lay the staged units out as one tape in `dir/tape`, as `id`: the envelopes,
/// each unit's slices on this tape (a span unit's run, `spans`), the front
/// index and the seal. File 0, the envelopes and the index name `id`.
#[allow(clippy::too_many_arguments)]
fn lay_out_tape(
    dir: &Path,
    mut staged: Vec<Staged>,
    with_index: bool,
    hash_column: bool,
    phantoms: &[Phantom],
    era: Era,
    spans: &[SpanOnTape],
    id: &TapeId,
) -> Template {
    // A span's run, its end clamped to the unit's slices (`usize::MAX` reads
    // as "to the last").
    let span_of = |name: &str| spans.iter().find(|sp| sp.unit == name).cloned();
    let tenants: Vec<&'static str> = {
        let mut v: Vec<&'static str> = Vec::new();
        for s in &staged {
            if !v.contains(&s.tenant) {
                v.push(s.tenant);
            }
        }
        v
    };
    std::fs::create_dir_all(dir.join("tape")).unwrap();

    // Positions: 0-3, a tenant envelope each, the operator envelope and its
    // backup, each tenant envelope's backup in the same order
    // (volume-format-v2.md §1.3), every unit's slices contiguously, the seal
    // marker last.
    let first_env = 4i64;
    let op_pos = first_env + tenants.len() as i64;
    let first_backup = op_pos + 2;
    let mut next = first_backup + tenants.len() as i64;
    let mut slice_cipher: Vec<(i64, Vec<u8>)> = Vec::new();
    let mut held_runs: BTreeMap<String, std::ops::Range<usize>> = BTreeMap::new();
    for s in staged.iter_mut() {
        let span = span_of(&s.unit.name).map(|sp| SpanOnTape {
            held: sp.held.start..sp.held.end.min(s.slices.len()),
            ..sp
        });
        if let Some(sp) = &span {
            assert!(
                !sp.held.is_empty(),
                "{}: the run {:?} of its {} slices",
                s.unit.name,
                sp.held,
                s.slices.len()
            );
            held_runs.insert(s.unit.name.clone(), sp.held.clone());
        }
        for (i, plain) in s.slices.iter().enumerate() {
            let cipher = stub_encrypt(&format!("{},op,esc", s.tenant), "", plain);
            // A span unit's slice outside the run is on another tape: listed,
            // with its hashes, and no tape_position (item 125).
            let here = span.as_ref().is_none_or(|sp| sp.held.contains(&i));
            s.unit.slices.push(ManifestSlice {
                number: i as i64 + 1,
                tape_position: here.then_some(next),
                size_bytes: plain.len() as i64,
                encrypted_bytes: cipher.len() as i64,
                sha256_plain: sha256_hex(plain),
                sha256_encrypted: sha256_hex(&cipher),
            });
            if here {
                slice_cipher.push((next, cipher));
                next += 1;
            }
        }
    }
    let seal_pos = next;
    let total_files = (seal_pos + 1) as i32;

    let envelope = |tenant: &str, recipients: &str, which: &[&Staged]| -> Vec<u8> {
        let units: Vec<ManifestUnit> = which.iter().map(|s| s.unit.clone()).collect();
        let manifest = era_manifest(era, generate_manifest_toml(id.label, tenant, &units));
        let members: Vec<String> = which
            .iter()
            .filter(|s| s.catalogue.is_some())
            .map(|s| format!("{}_v{}.1.dar", &s.unit.uuid[..8], s.unit.snapshot_version))
            .collect();
        let recovery = generate_recovery_md(id.label, tenant, &units, &members);
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
                let mut rows = idx_rows_for(
                    s.tenant,
                    &s.unit.name,
                    &s.unit.uuid,
                    s.unit.snapshot_version,
                    s.unit.slices.len() as i64,
                    bytes,
                );
                rows[0].volume = id.label;
                rows[0].volume_uuid = id.uuid;
                rows[0].serial = id.serial;
                // A span unit (§1.5): the `this-tape` row carries the run this
                // tape's plan holds, and the earlier holders of its slices are
                // the tapes `id` names, each with its run (by default OTHER02,
                // holding the slices before this run).
                if let Some(held) = held_runs.get(&s.unit.name) {
                    let first = held.start as i64 + 1;
                    rows[0].first = first;
                    rows[0].last = held.end as i64;
                    let earlier = match &id.earlier {
                        Some(hs) => hs.clone(),
                        None if first > 1 => vec![Holder {
                            label: "OTHER02",
                            uuid: OTHER_UUID,
                            serial: "OTHERSER02",
                            first: 1,
                            last: first - 1,
                        }],
                        None => Vec::new(),
                    };
                    for h in earlier {
                        rows.push(IdxRow {
                            first: h.first,
                            last: h.last,
                            volume: h.label,
                            volume_uuid: h.uuid,
                            serial: h.serial,
                            state: "sealed",
                            sealed_at: "2026-09-01T10:00:00Z",
                            ..rows[0].clone()
                        });
                    }
                }
                unit_rows.extend(rows);
                path_rows.push((
                    s.unit.name.clone(),
                    s.unit.snapshot_version,
                    idx_paths_for(&s.unit.name, &s.src, &s.hashes),
                ));
            }
            for p in phantoms {
                if tenant == "operator" || tenant == p.tenant {
                    unit_rows.extend(phantom_rows(p));
                }
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
    let thunk_params = IdThunkV2Params {
        label: id.label,
        uuid: id.uuid,
        media_type: "LTO-6",
        tapectl_version: "1.1.0",
        nominal_capacity: 2_500_000_000_000,
        mam_capacity: 2_500_000_000_000,
        total_files,
        mam_manufacturer: "HP",
        mam_serial: id.serial,
        mam_length: 846,
        mam_loads: 1,
        created_at: "2026-10-05T00:00:00Z",
        cartridge_identity_source: Some("mam"),
    };
    // A span tape's File 0 comes from the sibling that states both features,
    // as `build` writes it (ADR-0012 item 124).
    let thunk = if era == Era::V170Span {
        generate_id_thunk_v2_requiring(&thunk_params, REQUIRES_SPAN)
    } else {
        generate_id_thunk_v2(&thunk_params)
    };
    files.push((0, "id_thunk", era_file0(era, thunk).into_bytes()));
    files.push((
        1,
        "system_guide",
        generate_system_guide_v2(id.label, total_files).into_bytes(),
    ));
    files.push((
        2,
        "restore_sh",
        generate_restore_script_v2(id.label, total_files).into_bytes(),
    ));
    for (i, t) in tenants.iter().enumerate() {
        let mine: Vec<&Staged> = staged.iter().filter(|s| s.tenant == *t).collect();
        let env = envelope(t, &format!("{t},op,esc"), &mine);
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
    let fi = era_front_index(era, generate_front_index(id.label, &fi_files)).into_bytes();
    let fi_sha = sha256_hex(&fi);
    let mut seal_files = fi_files.clone();
    seal_files[3].size_bytes = Some(fi.len() as u64);
    seal_files[3].sha256_encrypted = Some(fi_sha.clone());
    let seal = era_seal(
        era,
        &generate_seal_marker(id.label, total_files, &fi_sha, &seal_files),
    )
    .into_bytes();
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
            // The slices on this tape (a span unit's run; every slice of any
            // other unit).
            positions: s
                .unit
                .slices
                .iter()
                .filter_map(|sl| sl.tape_position)
                .collect(),
            sizes: s
                .unit
                .slices
                .iter()
                .filter(|sl| sl.tape_position.is_some())
                .map(|sl| sl.encrypted_bytes as usize)
                .collect(),
            hashes: s
                .unit
                .slices
                .iter()
                .filter(|sl| sl.tape_position.is_some())
                .map(|sl| sl.sha256_encrypted.clone())
                .collect(),
            slices: s.unit.slices.len(),
        })
        .collect();
    Template {
        dir: dir.to_path_buf(),
        units,
        total_files,
        label: id.label,
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
    "chmod",
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
            generate_restore_script_v2(t.label, t.total_files),
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

    /// End this copy's tape early, after data slice `k`, with a short seal
    /// (`cut_tape_dir`).
    fn cut(&self, k: i64) {
        cut_tape_dir(&self.dir.join("tape"), LABEL, k);
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

    /// A box of tapes, each in `cart/<label>`, with `tapes[start]` loaded and
    /// its RESTORE.sh the one that runs (`load` puts another in the drive).
    fn of_box(tapes: &[&Template], start: usize) -> Self {
        let h = Self::of(tapes[start], &[]);
        std::fs::remove_dir_all(h.dir.join("tape")).unwrap();
        for t in tapes {
            copy_dir(&t.dir.join("tape"), &h.dir.join("cart").join(t.label));
        }
        std::fs::write(h.dir.join("loaded"), format!("cart/{}", tapes[start].label)).unwrap();
        h
    }

    /// Put the cartridge `label` in the drive, as a heir does between prompts:
    /// the head at its start, and a line `swap` in `ops`.
    fn load(&self, label: &str) {
        assert!(
            self.dir.join("cart").join(label).is_dir(),
            "no cartridge {label}"
        );
        std::fs::write(self.dir.join("loaded"), format!("cart/{label}")).unwrap();
        std::fs::write(self.dir.join("pos"), "0\n").unwrap();
        use std::io::Write;
        let mut ops = std::fs::OpenOptions::new()
            .append(true)
            .open(self.dir.join("ops"))
            .unwrap();
        writeln!(ops, "swap").unwrap();
    }

    /// The cartridge in the drive.
    fn loaded(&self) -> String {
        std::fs::read_to_string(self.dir.join("loaded")).unwrap()
    }

    /// Put a stub on `PATH` after the copy was made.
    fn stub(&self, name: &str, body: &str) {
        write_exec(&self.dir.join("bin").join(name), body);
    }

    /// Run the script with `args` and answer it as a heir at the drive does:
    /// the run's output is watched, and each time a new line asking for the
    /// next cartridge appears, `on_prompt(n, output so far)` is called (n from
    /// 1). It may load another cartridge, wait, look at the disk; what it
    /// returns is typed in as one line, and None ends the input. Each `mt` and
    /// `dd` logs its start and end to `procs` (`FAKE_PROCS`).
    fn run_prompted(
        &self,
        args: &[&str],
        on_prompt: impl FnMut(usize, &str) -> Option<String>,
    ) -> (i32, String) {
        self.run_prompted_at(args, SPAN_PROMPT, on_prompt)
    }

    /// `run_prompted`, watching for `prompt` (`--swap-tapes` asks in other
    /// words than a restore across tapes).
    fn run_prompted_at(
        &self,
        args: &[&str],
        prompt: &str,
        mut on_prompt: impl FnMut(usize, &str) -> Option<String>,
    ) -> (i32, String) {
        use std::io::Write;
        use std::process::Stdio;
        std::fs::write(self.dir.join("ops"), "").unwrap();
        std::fs::write(self.dir.join("procs"), "").unwrap();
        let out_path = self.dir.join("run.out");
        let out = std::fs::File::create(&out_path).unwrap();
        let mut child = Command::new("timeout")
            .args(["-k", "10", "300", "bash"])
            .arg(self.dir.join("RESTORE.sh"))
            .args(args)
            .current_dir(&self.dir)
            .env("PATH", &self.path)
            .env("FAKE_TAPE", &self.dir)
            .env("FAKE_PROCS", self.dir.join("procs"))
            .env("TAPE_DEVICE", "/dev/nst-fake")
            .env("TMPDIR", self.dir.join("tmp"))
            .env("HOME", self.dir.join("home"))
            .stdin(Stdio::piped())
            .stdout(Stdio::from(out.try_clone().unwrap()))
            .stderr(Stdio::from(out))
            .spawn()
            .expect("spawn RESTORE.sh");
        let mut stdin = child.stdin.take();
        let mut answered = 0;
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(280);
        let code = loop {
            let done = child.try_wait().unwrap();
            let text = std::fs::read_to_string(&out_path).unwrap_or_default();
            let asked = text.matches(prompt).count();
            while answered < asked {
                answered += 1;
                match on_prompt(answered, &text) {
                    Some(line) => {
                        if let Some(w) = stdin.as_mut() {
                            let _ = writeln!(w, "{line}");
                            let _ = w.flush();
                        }
                    }
                    None => stdin = None,
                }
            }
            if let Some(status) = done {
                break status.code().unwrap_or(-1);
            }
            assert!(
                std::time::Instant::now() < deadline,
                "RESTORE.sh {args:?} did not end:\n{text}"
            );
            std::thread::sleep(std::time::Duration::from_millis(20));
        };
        let text = std::fs::read_to_string(&out_path).unwrap();
        assert_ne!(code, 124, "RESTORE.sh {args:?} timed out (hung):\n{text}");
        (code, text)
    }

    /// The `mt` and `dd` runs `procs` shows started and not ended.
    fn running(&self) -> Vec<String> {
        let log = std::fs::read_to_string(self.dir.join("procs")).unwrap_or_default();
        // In order, so a pid used again later in the run is told apart.
        let mut live: BTreeMap<String, String> = BTreeMap::new();
        for l in log.lines() {
            if let Some(rest) = l.strip_prefix("start ") {
                let pid = rest.split(' ').next().unwrap().to_string();
                live.insert(pid, rest.to_string());
            } else if let Some(pid) = l.strip_prefix("end ") {
                live.remove(pid);
            }
        }
        live.into_values().collect()
    }
}

/// The line RESTORE.sh prints when it waits for the cartridge that holds the
/// next slice of a unit held across tapes.
const SPAN_PROMPT: &str = "Load the cartridge that holds slice ";

/// The line before each prompt for another cartridge (the fresh read of File
/// 1, #478 m10), as `Heir::run_prompted_at` sets `TAPE_DEVICE`.
const EJECT_FIRST: &str = "First eject the cartridge now in the drive: its eject button, or mt -f /dev/nst-fake offline in another";

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
    reads_in(&h.ops())
}

fn reads_in(ops: &[String]) -> Vec<i64> {
    ops.iter()
        .filter_map(|l| l.strip_prefix("dd "))
        .filter_map(|p| p.parse().ok())
        .collect()
}

/// R1 (volume-format-v2.md §4.2): on a tape whose File 0 requires
/// `short-seal`, a mode that reads slices or says which units the tape holds
/// reads the seal marker right after File 3, before any envelope or slice:
/// `mt eod`, `mt bsfm 2`, one read of the last file, then a rewind before the
/// next read (the wind the spec accepts). This checks that shape and returns
/// the ops without it, so the rest of the run can be held to the
/// forward-only rule (#396) as before 1.6.0: one rewind, each file once,
/// ascending. The wind must be there: the tapes these tests run on are 1.6.0.
fn without_seal_wind(ops: &[String]) -> Vec<String> {
    let at = ops
        .iter()
        .position(|l| l == "mt eod")
        .unwrap_or_else(|| panic!("no wind to the seal marker:\n{ops:#?}"));
    assert_eq!(
        ops.iter().filter(|l| *l == "mt eod").count(),
        1,
        "one wind to the seal:\n{ops:#?}"
    );
    let before = reads_in(&ops[..at]);
    assert_eq!(
        before.iter().copied().filter(|p| *p > 3).count(),
        0,
        "nothing after File 3 is read before the seal:\n{ops:#?}"
    );
    assert_eq!(
        before.last(),
        Some(&3),
        "the wind comes right after File 3:\n{ops:#?}"
    );
    assert_eq!(
        ops.get(at + 1).map(String::as_str),
        Some("mt bsfm 2"),
        "{ops:#?}"
    );
    assert!(
        ops.get(at + 2).is_some_and(|l| l.starts_with("dd ")),
        "the seal marker is read:\n{ops:#?}"
    );
    let mut end = at + 3;
    if let Some(next) = ops.get(at + 3) {
        assert_eq!(next, "mt rewind", "a rewind after the seal:\n{ops:#?}");
        end += 1;
    }
    let mut rest = ops[..at].to_vec();
    rest.extend_from_slice(&ops[end..]);
    rest
}

/// The run's reads and rewinds with the wind to the seal taken out.
fn walk(h: &Heir) -> (Vec<i64>, usize) {
    let ops = without_seal_wind(&h.ops());
    let rewinds = ops.iter().filter(|l| *l == "mt rewind").count();
    (reads_in(&ops), rewinds)
}

/// A K-slice restore rewinds once, at the start, and then only moves
/// forward: it used to rewind and space from BOT before every file it read.
/// That is a 1.5.0 tape's; a 1.6.0 tape's adds the one wind to the seal
/// before the first envelope (R1), and is forward only after it.
#[test]
fn a_multi_slice_restore_rewinds_once_and_only_spaces_forward() {
    for (era, h) in [
        (Era::V150, Heir::of(template_150(), &[])),
        (Era::V160, Heir::new()),
    ] {
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
        assert_eq!(code, 0, "{era:?}: {text}");
        assert!(same_tree(&photos.src, Path::new(&dest)), "{text}");
        let ops = h.ops();
        let (walked, rewinds, wind) = match era {
            Era::V150 => {
                assert_eq!(h.count("mt eod"), 0, "no seal read before 1.6.0:\n{ops:#?}");
                (reads(&h), h.count("mt rewind"), 0)
            }
            Era::V160 | Era::V170Span => {
                let (r, w) = walk(&h);
                (r, w, 1)
            }
        };
        assert_eq!(
            rewinds, 1,
            "{era:?}: one rewind for the whole restore:\n{ops:#?}"
        );
        // File 0 -> File 3 and envelope -> first slice are the only gaps (and,
        // on a 1.6.0 tape, the start of the tape -> the first envelope after
        // the wind); the slices are contiguous, so K-1 is a generous ceiling.
        assert!(
            h.fsfs() <= 2 + wind + k.saturating_sub(1),
            "{era:?}: forward spaces: {} for {k} slices:\n{ops:#?}",
            h.fsfs()
        );
        // Every slice read exactly once, in ascending order.
        let slices: Vec<i64> = walked
            .into_iter()
            .filter(|p| photos.positions.contains(p))
            .collect();
        assert_eq!(slices, photos.positions, "{era:?}: {ops:#?}");
    }
}

/// --verify on a 1.6.0 tape reads the seal marker right after the front index
/// (R1: it says where the tape ended, before any data slice is hashed), then
/// walks the tape ascending from one more rewind, every file once, and does
/// not read the seal again.
#[test]
fn verify_on_a_1_6_0_tape_reads_the_seal_first_and_then_walks_forward() {
    let h = Heir::new();
    let (code, text) = h.run(&["--verify"]);
    assert_eq!(code, 0, "{text}");
    let ops = h.ops();
    assert_eq!(h.count("mt rewind"), 2, "{ops:#?}");
    let seal = template().total_files as i64 - 1;
    assert_eq!(
        reads(&h).iter().filter(|p| **p == seal).count(),
        1,
        "the seal is read once:\n{ops:#?}"
    );
    let (got, rewinds) = walk(&h);
    assert_eq!(rewinds, 1, "{ops:#?}");
    let mut sorted = got.clone();
    sorted.sort();
    sorted.dedup();
    assert_eq!(got, sorted, "each file once, ascending:\n{ops:#?}");
    assert_eq!(got[..4], [0, 1, 2, 3], "{ops:#?}");
    assert!(!got.contains(&seal), "{ops:#?}");
}

/// --verify on a 1.5.0 tape walks the tape once, ascending: one rewind, every
/// file read once, and the seal marker read last (#412 item 8), not before the
/// walk, exactly as before 1.6.0.
#[test]
fn verify_rewinds_once_and_reads_the_seal_marker_last() {
    let h = Heir::of(template_150(), &[]);
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
        walk(&h).1,
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
    assert_eq!(walk(&h).1, 1, "{:#?}", h.ops());

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

/// `DAR_SPY`, answering the script's version probe (`dar -V`) as dar
/// `version`. How the script restores a unit is chosen from that version
/// (`dar_at_least`), so a test of one way holds on whichever dar is
/// installed; the installed dar still does every extraction. Only the choice
/// follows the claim: a claim of 2.7.21 or later on a dar older than that
/// streams with the old dar's tail loss, so such a test checks the bytes
/// only when the installed dar is new enough itself (`host_dar_at_least`).
fn dar_spy_claiming(version: &str) -> String {
    format!(
        "#!/bin/sh\ncase \"$*\" in -V*) echo ' dar version {version}, Copyright (C) 2002-2023 Denis Corbin'; exit 0 ;; esac\n{}",
        DAR_SPY
            .strip_prefix("#!/bin/sh\n")
            .expect("DAR_SPY is a sh script")
    )
}

/// The dar the old-dar tests claim: Debian's and Ubuntu's before Ubuntu 26.04
/// (2.8.3 there), older than 2.7.21, which fixed sequential reading without a
/// catalogue.
const OLD_DAR: &str = "2.7.13";

/// Is the dar on this host (not a stub) at least `want`, as RESTORE.sh's
/// `dar_at_least` would judge it from `dar -V`?
fn host_dar_at_least(want: (u32, u32, u32)) -> bool {
    let out = Command::new("dar")
        .args(["-V", "-Q", "-N"])
        .env("HOME", "/nonexistent")
        .output()
        .expect("dar must be on PATH (issue #43)");
    let text = String::from_utf8_lossy(&out.stdout).to_string();
    let have: Vec<u32> = text
        .split("dar version ")
        .nth(1)
        .and_then(|rest| rest.split(|c: char| c != '.' && !c.is_ascii_digit()).next())
        .map(|v| v.split('.').filter_map(|p| p.parse().ok()).collect())
        .unwrap_or_default();
    assert_eq!(have.len(), 3, "dar -V names no X.Y.Z version:\n{text}");
    (have[0], have[1], have[2]) >= want
}

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
    assert_eq!(walk(&h).1, 1, "{:#?}", h.ops());
    let got = walk(&h).0;
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
    assert_eq!(walk(&h).1, 1, "{:#?}", h.ops());
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
    assert_eq!(walk(&h).1, 1, "{:#?}", h.ops());

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

/// `t` (File 0 or a seal) with its one `requires = ...` line replaced by
/// `line`.
fn with_requires(t: &str, line: &str) -> String {
    let old = t
        .lines()
        .find(|l| l.starts_with("requires = "))
        .expect("a requires line");
    assert_eq!(t.matches(old).count(), 1, "{t}");
    t.replace(old, line)
}

/// A 1.6.0 tape requires `short-seal`, in File 0 and in the seal, whether
/// it sealed full or short (ADR-0012 item 111), and its seal states
/// `ended_after`. The third place `requires` lives is RESTORE.sh's own
/// `KNOWN_FEATURES`, which names it. A 1.5.0 tape states it empty.
#[test]
fn a_new_tape_states_short_seal_in_file_0_and_the_seal() {
    let t = template();
    let last = t.total_files - 1;
    let f0 = std::fs::read_to_string(t.dir.join("tape/0")).unwrap();
    let seal = std::fs::read_to_string(t.dir.join(format!("tape/{last}"))).unwrap();
    assert!(
        f0.contains("\nlayout_version = 2\nrequires = [\"short-seal\"]\n"),
        "File 0"
    );
    assert!(
        seal.contains("\nrequires = [\"short-seal\"]\n"),
        "seal marker"
    );
    assert!(
        seal.contains(&format!(
            "\nfile_count = {}\nended_after = {}\n",
            t.total_files,
            last - 1
        )),
        "a full seal ends after the last data slice"
    );
    let script = generate_restore_script_v2(LABEL, t.total_files);
    assert_eq!(
        script
            .matches("\nKNOWN_FEATURES=\"short-seal spanning\"\n")
            .count(),
        1,
        "RESTORE.sh knows short-seal and spanning"
    );
    let old = template_150();
    let f0 = std::fs::read_to_string(old.dir.join("tape/0")).unwrap();
    assert!(f0.contains("\nrequires = []\n"), "a 1.5.0 File 0");
}

/// §6 "Since 1.6.0": the text that tells an heir a tape is newer than the
/// script, fixed at "tapectl 1.1.0" since 2026-10-06 item 20, moved to
/// "tapectl 1.6.0" when a new required feature arrived, and to "tapectl
/// 1.7.0" with the next (`spanning`, ADR-0012 items 124 and 135). Pinned in
/// the generated text and in what the heir actually reads when the script
/// refuses a tape.
#[test]
fn the_newer_tape_refusal_says_tapectl_1_7_0() {
    let script = generate_restore_script_v2(LABEL, template().total_files);
    assert_eq!(
        script
            .matches("This RESTORE.sh (layout v2, tapectl 1.7.0) cannot read it safely")
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
        with_requires(t, "requires = [\"unknown-feature\"]")
    });
    let (code, text) = h.run(&["--info"]);
    assert_ne!(code, 0, "{text}");
    assert!(
        text.contains("(layout v2, tapectl 1.7.0) cannot read it safely"),
        "{text}"
    );
}

/// A tape that requires a feature this script does not know is refused,
/// naming the feature and the way to a script that does, before anything
/// else of it is interpreted. Never a misread. (Before 1.6.0 the stand-in
/// for an unknown feature was `short-seal`, which this script knows.)
#[test]
fn a_tape_requiring_an_unknown_feature_is_refused_by_name() {
    let h = Heir::new();
    edit_text_file(&h, 0, |t| {
        with_requires(t, "requires = [\"short-seal\", \"unknown-feature\"]")
    });
    for mode in [vec!["--info"], vec!["--verify"]] {
        let (code, text) = h.run(&mode);
        assert_ne!(code, 0, "{mode:?}: {text}");
        assert!(
            text.contains("requires feature(s) unknown-feature, which this script does not know"),
            "{mode:?}: names it, and only it:\n{text}"
        );
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
        with_requires(t, "requires = [\"short-seal\", \"unknown-feature\"]")
    });
    let (code, text) = h.run(&["--info"]);
    assert_ne!(code, 0, "{text}");
    assert!(
        text.contains("requires feature(s) unknown-feature, which this script does not know"),
        "{text}"
    );
}

/// Keys the script does not know are ignored unless `requires` names them:
/// a future tape may add fields, and a 1.1.0 reader must still read it.
/// (The seal's unknown key was `ended_after` before 1.6.0, which this script
/// now knows and holds to rule S1.)
#[test]
fn unknown_keys_that_are_not_required_are_ignored() {
    let h = Heir::new();
    let seal = template().total_files as i64 - 1;
    edit_text_file(&h, 0, |t| {
        with_requires(t, "requires = [\"short-seal\"]\nfuture_key = \"x\"")
    });
    edit_text_file(&h, seal, |t| {
        with_requires(t, "requires = [\"short-seal\"]\nfuture_key = 7")
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
/// The restore falls back: on a dar older than 2.7.21 to decrypting the
/// slices to disk and extracting directly, which needs no catalogue. It says
/// so, and restores the unit byte for byte. (The old version is claimed by a
/// wrapper, so this way is tested on any installed dar; the installed dar does
/// the extraction. A newer dar streams instead: the next test.)
#[test]
fn a_catalogue_dar_refuses_falls_back_to_slices_on_disk() {
    let h = Heir::with_stubs(&[("dar", &dar_spy_claiming(OLD_DAR))]);
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

/// The same refused catalogue on dar 2.7.21 or later, which reads
/// sequentially without one correctly: the restore drops the catalogue and
/// streams the unit again, with no slice decrypted to disk. The version is
/// claimed by a wrapper, so the choice is tested on any dar; the restored
/// bytes are checked when the installed dar is 2.7.21 or later itself.
#[test]
fn a_catalogue_dar_refuses_on_dar_2_7_21_streams_again_without_it() {
    let h = Heir::with_stubs(&[("dar", &dar_spy_claiming("2.7.21"))]);
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
    assert!(text.contains("refused the envelope's catalogue"), "{text}");
    assert!(!text.contains("decrypting the slices to disk"), "{text}");
    let args = std::fs::read_to_string(h.dir.join("dar.args")).unwrap();
    let calls: Vec<&str> = args.lines().filter(|l| l.starts_with("-x")).collect();
    assert_eq!(calls.len(), 2, "{args}");
    assert!(
        calls[0].contains("--sequential-read") && calls[0].contains("-A "),
        "{args}"
    );
    assert!(
        calls[1].contains("--sequential-read") && !calls[1].contains("-A "),
        "streamed again, without the catalogue: {args}"
    );
    if host_dar_at_least((2, 7, 21)) {
        assert_eq!(code, 0, "{text}");
        assert!(same_tree(&u.src, Path::new(&dest)), "{text}");
    }
}

/// An envelope with no catalogue for the unit: a dar older than 2.7.21
/// (2.7.13 is claimed by a wrapper) cannot stream it safely, so the slices
/// are decrypted to disk first, with the space check that implies (the
/// slices and the files). A newer dar streams it:
/// `no_catalogue_on_dar_2_7_21_still_streams`.
#[test]
fn no_catalogue_on_an_old_dar_restores_from_slices_on_disk() {
    let h = Heir::with_stubs(&[("dar", &dar_spy_claiming(OLD_DAR))]);
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
/// wrapper, so the choice is tested on any dar; the restored bytes are
/// checked when the installed dar is 2.7.21 or later itself.)
#[test]
fn no_catalogue_on_dar_2_7_21_still_streams() {
    let dar =
        "#!/bin/sh\ncase \"$*\" in -V*) echo ' dar version 2.7.21, Copyright'; exit 0 ;; esac\n\
               echo \"$*\" >>\"$FAKE_TAPE/dar.args\"\nexec \"$FAKE_TAPE/hostbin/dar\" \"$@\"\n";
    let h = Heir::with_stubs(&[("dar", dar)]);
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
    let args = std::fs::read_to_string(h.dir.join("dar.args")).unwrap();
    assert!(args.contains("--sequential-read"), "{args}");
    assert!(!args.contains("-A "), "{args}");
    assert!(!text.contains("decrypting the slices to disk"), "{text}");
    if host_dar_at_least((2, 7, 21)) {
        assert_eq!(code, 0, "{text}");
        assert!(same_tree(&u.src, Path::new(&dest)), "{text}");
    }
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
    assert_eq!(walk(&h).1, 1, "{:#?}", h.ops());
    let got = walk(&h).0;
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
/// forward-only rule every mode keeps (#396), past the one wind to the seal a
/// 1.6.0 tape costs first (R1, `without_seal_wind`).
fn assert_forward_only(h: &Heir, what: &str) {
    let ops = h.ops();
    assert_eq!(walk(h).1, 1, "{what}: one rewind:\n{ops:#?}");
    let got = walk(h).0;
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
    let last_read = walk(&h).0.into_iter().max().unwrap();
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
    // It points at the uncapped listing, and prints no cap of its own.
    let f = flat(&text);
    assert!(
        f.contains("each one's newest version and holders:") && f.contains("--where --all --key"),
        "{f}"
    );
    assert!(!f.contains("first 40") && !f.contains("not shown"), "{f}");
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

/// The miss path names every unit of a large index (it stopped at 40), one
/// per line, and points at `--where --all` for their versions and holders.
#[test]
fn a_miss_on_a_large_index_names_every_unit_and_points_at_the_listing() {
    let h = Heir::of(box_template(), &[]);
    let (code, text) = h.run(&["--where", "nonesuch", "--key", &h.key("esc")]);
    assert_eq!(code, 1, "{text}");
    assert!(
        text.contains("No unit named 'nonesuch' is in this index."),
        "{text}"
    );
    let named: Vec<&str> = text
        .lines()
        .filter(|l| l.starts_with("    ") && !l.trim().is_empty())
        .collect();
    // 54 units, 53 distinct names (`dup` is two units): all of them, sorted.
    assert_eq!(named.len(), 53, "{text}");
    assert!(named.contains(&"    archive/unit-045"), "{text}");
    assert!(named.contains(&"    bob/old-3"), "{text}");
    assert!(flat(&text).contains("--where --all --key"), "{text}");
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

/// The unit headings of a `--where --all` listing, in order.
fn all_headings(text: &str) -> Vec<&str> {
    text.lines().filter(|l| l.starts_with("Unit ")).collect()
}

/// The lines of the "NOT on this tape" recap of a `--where --all` listing.
fn all_recap(text: &str) -> Vec<&str> {
    text.lines()
        .skip_while(|l| !l.contains("is NOT on this tape"))
        .skip(1)
        .take_while(|l| l.starts_with("    "))
        .collect()
}

/// One unit of a `--where --all` listing: from its heading line (which starts
/// with `heading`) to the next unit's, blank lines between versions included.
fn all_unit<'a>(text: &'a str, heading: &str) -> Vec<&'a str> {
    let mut lines = text.lines().skip_while(|l| !l.starts_with(heading));
    let mut out: Vec<&str> = lines.next().into_iter().collect();
    out.extend(lines.take_while(|l| !l.starts_with("Unit ")));
    out
}

/// `--where --all` lists EVERY unit the index names, past the 40 the miss path
/// used to stop at, with each unit's newest version and whether this tape holds
/// it; the escrow key reads the operator envelope's map of every tenant.
#[test]
fn where_all_lists_every_unit_the_index_names_past_forty() {
    let h = Heir::of(box_template(), &[]);
    let (code, text) = h.run(&["--where", "--all", "--key", &h.key("esc")]);
    assert_eq!(code, 0, "{text}");
    eprintln!("---- --where --all --key esc (box tape) ----\n{text}----");
    let f = flat(&text);
    assert!(f.contains("Index read from tape: HEIR01 (written "), "{f}");
    assert!(
        f.contains("54 unit(s) in this index; this tape holds the newest version of 2 of them."),
        "{f}"
    );
    let headings = all_headings(&text);
    assert_eq!(headings.len(), 54, "every unit, uncapped:\n{text}");
    // Positive controls past the old cap of 40 (they sort after the first 40).
    assert!(
        headings
            .contains(&"Unit archive/unit-045 (tenant alice): newest version 1, NOT on this tape"),
        "{text}"
    );
    assert!(
        headings.contains(&"Unit bob/old-3 (tenant bob): newest version 1, NOT on this tape"),
        "{text}"
    );
    // The two units really on this tape are marked so, with their newest versions.
    assert!(
        headings.contains(&"Unit docs (tenant alice): newest version 1, on this tape"),
        "{text}"
    );
    assert!(
        headings.contains(&"Unit ledgers (tenant bob): newest version 3, on this tape"),
        "{text}"
    );
    assert_eq!(
        headings
            .iter()
            .filter(|l| l.ends_with(", on this tape"))
            .count(),
        2,
        "{text}"
    );
    assert_eq!(
        headings
            .iter()
            .filter(|l| l.ends_with(", NOT on this tape"))
            .count(),
        52,
        "{text}"
    );
    // A name with a TAB, an LF and a backslash is one heading line, escaped as
    // the index writes it (a raw LF would forge a second line).
    assert!(
        headings.contains(
            &"Unit weird\\ttab\\nnew\\back (tenant alice): newest version 1, NOT on this tape"
        ),
        "{text}"
    );
    assert!(!text.contains("weird\ttab"), "no raw TAB in the output");
    // The newest version of a unit is the highest NUMBER (10, not the 9 a text
    // comparison would pick), and its versions are listed in order.
    assert!(
        headings.contains(&"Unit tens (tenant alice): newest version 10, NOT on this tape"),
        "{text}"
    );
    let at = |needle: &str| {
        text.find(needle)
            .unwrap_or_else(|| panic!("no {needle:?}:\n{text}"))
    };
    assert!(
        at("  Version 2: 3 slice(s)") < at("  Version 9: 3 slice(s)"),
        "{text}"
    );
    assert!(
        at("  Version 9: 3 slice(s)") < at("  Version 10 (newest): 3 slice(s)"),
        "{text}"
    );
    // One name made twice is two units, each with its own newest version and uuid.
    let dups: Vec<&&str> = headings
        .iter()
        .filter(|l| l.starts_with("Unit dup "))
        .collect();
    assert_eq!(dups.len(), 2, "{text}");
    assert!(
        dups.contains(&&"Unit dup (tenant alice): newest version 2, NOT on this tape"),
        "{text}"
    );
    assert!(
        dups.contains(&&"Unit dup (tenant alice): newest version 1, NOT on this tape"),
        "{text}"
    );
    assert!(
        text.contains("unit uuid dddddddd-0000-4000-8000-000000000001"),
        "{text}"
    );
    assert!(
        text.contains("unit uuid dddddddd-0000-4000-8000-000000000002"),
        "{text}"
    );
    // Every holder with the fields a reader needs to find the cartridge.
    let docs = all_unit(&text, "Unit docs (tenant alice)");
    let n = box_template()
        .units
        .iter()
        .find(|u| u.name == "docs")
        .unwrap()
        .positions
        .len();
    assert!(
        docs.iter()
            .any(|l| l.starts_with(&format!("  Version 1 (newest): {n} slice(s), "))),
        "{docs:#?}"
    );
    assert!(
        docs.iter()
            .any(|l| l.trim() == format!("tape HEIR01  (this tape)  slices 1-{n} of {n}")),
        "{docs:#?}"
    );
    assert!(
        docs.iter().any(|l| l.trim()
            == format!("tape OTHER02  (sealed 2026-09-01T10:00:00Z)  slices 1-{n} of {n}")),
        "{docs:#?}"
    );
    let u40 = all_unit(&text, "Unit archive/unit-040 (tenant alice)");
    assert!(
        u40.iter()
            .any(|l| l.contains("tape QUAR03  (QUARANTINED: a verify proved this medium bad")),
        "{u40:#?}"
    );
    assert!(
        u40.contains(
            &format!(
                "        volume uuid {OTHER_UUID}; cartridge serial OTHERSER02; generation LTO-6"
            )
            .as_str()
        ),
        "{u40:#?}"
    );
    // The recap names the 51 units whose newest version is on other tapes, with
    // the tapes that hold it, before the listing.
    let recap = all_recap(&text);
    assert_eq!(recap.len(), 52, "{recap:#?}\n{text}");
    assert!(
        recap.contains(
            &"    archive/unit-040 (tenant alice), version 2, unit uuid aaaaaaaa-0000-4000-8000-000000000040: OTHER02, QUAR03 (QUARANTINED)"
        ),
        "{recap:#?}"
    );
    assert!(
        recap.contains(&"    archive/unit-045 (tenant alice), version 1, unit uuid aaaaaaaa-0000-4000-8000-000000000045: OTHER02"),
        "{recap:#?}"
    );
    assert!(
        recap.contains(&"    tens (tenant alice), version 10, unit uuid eeeeeeee-0000-4000-8000-000000000001: OTHER02"),
        "{recap:#?}"
    );
    assert!(
        text.find("is NOT on this tape").unwrap() < text.find("Unit docs (tenant alice)").unwrap(),
        "the recap comes before the listing"
    );
    // No cap anywhere, in the output or the script.
    assert!(!f.contains("first 40") && !f.contains("not shown"), "{f}");
    // Forward-only, one rewind, envelopes only (the operator envelope ends the walk).
    assert_forward_only(&h, "--where --all with the escrow key");
    let first_slice = box_template().units[0].positions[0];
    assert!(
        walk(&h).0.iter().all(|p| *p < first_slice),
        "only envelopes are read:\n{:#?}",
        h.ops()
    );
}

/// A tenant's key reads that tenant's index: its units only, headed by the
/// tenant named in its envelope; two tenants' keys read both.
#[test]
fn where_all_with_a_tenant_key_lists_that_tenants_units_only() {
    let h = Heir::of(box_template(), &[]);
    let (code, text) = h.run(&["--where", "--all", "--key", &h.key("alice")]);
    assert_eq!(code, 0, "{text}");
    let f = flat(&text);
    assert!(
        f.contains("50 unit(s) in this index; this tape holds the newest version of 1 of them."),
        "{f}"
    );
    let headings = all_headings(&text);
    assert_eq!(headings.len(), 50, "{text}");
    assert!(
        headings.iter().all(|l| l.contains("(tenant alice)")),
        "{text}"
    );
    assert!(
        !text.contains("ledgers") && !text.contains("bob/old") && !text.contains("tenant bob"),
        "another tenant's units stay unnamed: {text}"
    );
    assert!(
        headings.contains(&"Unit docs (tenant alice): newest version 1, on this tape"),
        "{text}"
    );
    assert_forward_only(&h, "--where --all with a tenant key");

    let (code, text) = h.run(&[
        "--where",
        "--all",
        "--key",
        &h.key("alice"),
        "--key",
        &h.key("bob"),
    ]);
    assert_eq!(code, 0, "{text}");
    assert_eq!(all_headings(&text).len(), 54, "{text}");
    assert!(
        flat(&text)
            .contains("54 unit(s) in this index; this tape holds the newest version of 2 of them."),
        "{text}"
    );
    assert_forward_only(&h, "--where --all with two tenant keys");
}

/// A tape with no index exits 1 and says so, like `--where UNIT` and `--find`;
/// a key that opens nothing is the same refusal as every other mode.
#[test]
fn where_all_on_a_tape_without_an_index_says_so() {
    let h = Heir::of(solo_template(), &[]);
    for key in ["bob", "esc"] {
        let (code, text) = h.run(&["--where", "--all", "--key", &h.key(key)]);
        assert_eq!(code, 1, "{key}: {text}");
        let t = flat(&text);
        assert!(t.contains("This tape carries no index."), "{t}");
        assert!(
            t.contains("Tapes written before version 1.3.0 have none"),
            "{t}"
        );
        assert!(!text.contains("unit(s) in this index"), "{text}");
        assert!(!text.contains("No unit named"), "not a miss: {text}");
    }
    let h = Heir::of(box_template(), &[]);
    let (code, text) = h.run(&["--where", "--all", "--key", &h.key("stranger")]);
    assert_eq!(code, 1, "{text}");
    assert!(
        text.contains("no envelope matched the key provided"),
        "{text}"
    );
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
        vec!["--where", "--all", "--key", &h.key("alice")],
        vec!["--where", "--all", "--key", &h.key("op")],
    ] {
        let (code, text) = h.run(&args);
        assert_eq!(code, 0, "{args:?}: {text}");
        assert_forward_only(&h, &format!("{args:?}"));
        // Never beyond the envelopes: no data slice, and no seal marker but
        // the one read that says where a 1.6.0 tape ended.
        let first_slice = template().units[0].positions[0];
        assert!(
            walk(&h).0.iter().all(|p| *p < first_slice),
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
    assert_eq!(walk(&h).1, 1, "{:#?}", h.ops());
}

/// The two modes are in the per-mode tool table: the envelope set (`age`,
/// `truncate`, `tar`) plus the read set, no `dar`, `tee` or `mkfifo`; a host
/// without one is refused before the tape is touched.
#[test]
fn the_index_modes_check_the_tools_they_run_and_no_others() {
    for args in [
        vec!["--where", "docs", "--key", "KEY"],
        vec!["--where", "--all", "--key", "KEY"],
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
        (vec!["--where", "--all"], "usage: "),
        (vec!["--where", "--all", "--key"], "--key needs a value"),
        (
            vec!["--where", "--all", "--bogus"],
            "unknown option: --bogus",
        ),
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
        "--where --all --key KEYFILE [--key K2 ...]",
        "EVERY unit the index names, with no limit",
        "whether this tape holds it, and every version with each tape that holds it",
        "The units whose newest version is not on this tape are named first",
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
    assert!(
        info.contains("--where --all --key YOUR_KEY.age.key"),
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
        &guide_block(&manual, "FIRST_TAPE_POSITION"),
        &[(
            "FIRST_TAPE_POSITION",
            manifest.units[0].slices[0]
                .tape_position
                .expect("a whole unit's slice has a position")
                .to_string(),
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

    // Read 4 (M12): the slice line stops at a failed hash. An heir types the
    // lines one at a time, with no `-e` to save them, so run them that way
    // with a WRONG hash on the first slice: the encrypted copy is kept and
    // nothing is decrypted or deleted. (The same slice, put right again after.)
    {
        let sl = &manifest.units[0].slices[0];
        let good = std::fs::read(work.join(format!("restore.{}.dar", sl.number))).unwrap();
        std::fs::remove_file(work.join(format!("restore.{}.dar", sl.number))).unwrap();
        let (ok, text) = run(
            &guide_block(&manual, "FIRST_TAPE_POSITION"),
            &[(
                "FIRST_TAPE_POSITION",
                sl.tape_position
                    .expect("a whole unit's slice has a position")
                    .to_string(),
            )],
        );
        assert!(ok, "{text}");
        let loose = |line: &str| -> String {
            let line = line
                .replace("SHA256_ENCRYPTED", &"0".repeat(64))
                .replace("ENCRYPTED_BYTES", &sl.encrypted_bytes.to_string())
                .replace("NUMBER", &sl.number.to_string())
                .replace("YOUR_KEY.age.key", &h.key("bob"));
            let o = Command::new("bash")
                .args(["-c", &line])
                .current_dir(&work)
                .env("PATH", &h.path)
                .env("FAKE_TAPE", &h.dir)
                .env("HOME", h.dir.join("home"))
                .output()
                .expect("spawn bash");
            format!(
                "{}{}",
                String::from_utf8_lossy(&o.stdout),
                String::from_utf8_lossy(&o.stderr)
            )
        };
        let mut said = String::new();
        for line in &slice_block {
            said.push_str(&loose(line));
        }
        assert!(said.contains("FAILED"), "the hash check failed: {said}");
        assert!(
            work.join(format!("restore.{}.dar.age", sl.number)).exists(),
            "the encrypted copy is kept when its hash fails: {said}"
        );
        assert!(
            !work.join(format!("restore.{}.dar", sl.number)).exists(),
            "a slice whose hash failed is never decrypted: {said}"
        );
        std::fs::remove_file(work.join(format!("restore.{}.dar.age", sl.number))).unwrap();
        std::fs::write(work.join(format!("restore.{}.dar", sl.number)), good).unwrap();
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
    let seal = era_seal(
        Era::V160,
        &generate_seal_marker(label, total_files, &front_sha, &sealed),
    )
    .into_bytes();
    files.push((3, "front_index", front));
    files.push((seal_pos, "seal_marker", seal));
    let dir = h.dir.join("tape2");
    std::fs::create_dir_all(&dir).unwrap();
    for (pos, _, bytes) in files {
        std::fs::write(dir.join(pos.to_string()), padded(bytes)).unwrap();
    }
}

/// The reads (`dd N`) and rewinds of the ops from `from` to `to` (exclusive),
/// past the wind to the seal a 1.6.0 tape costs first (`without_seal_wind`):
/// both cartridges of these tests are 1.6.0 tapes, and the second one's seal
/// is read before any of its slices (R5).
fn segment(ops: &[String], from: usize, to: usize) -> (Vec<i64>, usize) {
    let seg = without_seal_wind(&ops[from..to]);
    let reads = reads_in(&seg);
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
    assert_eq!(walk(&h).1, 1, "one rewind:\n{:#?}", h.ops());
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

/// The same damaged unit staged without an envelope catalogue: on a dar older
/// than 2.7.21 it restores from slices on disk, and that path ends the same
/// way; a newer dar streams it. A test of either way claims its dar's version
/// with `dar_spy_claiming`.
fn dmg_nocat_bad() -> &'static Template {
    static T: OnceLock<Template> = OnceLock::new();
    T.get_or_init(|| {
        let mut t = dmg_tenants(2, DMG_UUID, &[(2, 50_000)]);
        t[0].units[0].cat = Cat::Absent;
        build_template("dmg-nocat", &t, false)
    })
}

/// On a dar older than 2.7.21 (claimed by a wrapper; the installed dar does
/// the extraction).
#[test]
fn a_restore_from_slices_on_disk_writes_the_list_too() {
    let h = Heir::of(dmg_nocat_bad(), &[("dar", &dar_spy_claiming(OLD_DAR))]);
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

/// The same unit on dar 2.7.21 or later streams without a catalogue, and
/// that path writes the list too. The version is claimed by a wrapper, so the
/// choice is tested on any dar; the outcome is checked when the installed dar
/// is 2.7.21 or later itself (an older one, streaming, loses a file's tail).
#[test]
fn a_streamed_restore_without_a_catalogue_writes_the_list_too() {
    let h = Heir::of(dmg_nocat_bad(), &[("dar", &dar_spy_claiming("2.7.21"))]);
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
    assert!(!text.contains("decrypting the slices to disk"), "{text}");
    let args = std::fs::read_to_string(h.dir.join("dar.args")).unwrap();
    let calls: Vec<&str> = args.lines().filter(|l| l.starts_with("-x")).collect();
    assert_eq!(calls.len(), 1, "{args}");
    assert!(
        calls[0].contains("--sequential-read") && !calls[0].contains("-A "),
        "streamed, without a catalogue: {args}"
    );
    if host_dar_at_least((2, 7, 21)) {
        assert_eq!(code, 1, "{text}");
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
    // The from-disk way is a dar older than 2.7.21's (claimed by a wrapper):
    // a newer dar streams the unit with no catalogue too.
    let old = dar_spy_claiming(OLD_DAR);
    let from_disk: &[(&str, &str)] = &[("dar", &old)];
    for (t, via, stubs) in [
        (dmg_good(), "streamed", &[][..]),
        (dmg_nocat_bad(), "from disk", from_disk),
    ] {
        let h = Heir::of(t, stubs);
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
            assert!(text.contains("decrypting the slices to disk"), "{text}");
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
        &guide_block(&manual, "FIRST_TAPE_POSITION"),
        &[("FIRST_TAPE_POSITION", photos.positions[0].to_string())],
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

// ---- the second fresh read of File 1 (2026-10-19, ADR-0012 item 81, #477) ----
//
// The commands the new text shows are run as written, against the fake tape
// and the real dar, as the earlier ones above are.

/// Run `lines` of the guide in `work` against the fake tape. `errexit` stops at
/// the first failing line (`bash -e`); the commands that expect dar to fail run
/// without it, as a person typing them one at a time would.
fn run_guide(
    h: &Heir,
    work: &Path,
    lines: &[String],
    subs: &[(&str, String)],
    errexit: bool,
) -> (bool, String) {
    let mut script = lines.join("\n");
    for (from, to) in subs {
        script = script.replace(from, to);
    }
    let mut cmd = Command::new("bash");
    if errexit {
        cmd.arg("-e");
    }
    let o = cmd
        .args(["-o", "pipefail", "-c", &script])
        .current_dir(work)
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
}

/// The front index's entries as (position, type, size_bytes, sha256_encrypted),
/// read from the `front_index.toml` that the guide's step 3 wrote into `work`.
fn front_index_entries(work: &Path) -> Vec<(String, String, String, String)> {
    let fi = std::fs::read_to_string(work.join("front_index.toml")).unwrap();
    fi.split("[[files]]")
        .skip(1)
        .map(|chunk| {
            let get = |key: &str| {
                chunk
                    .lines()
                    .find_map(|l| l.strip_prefix(&format!("{key} = ")))
                    .map(|v| v.trim_matches('"').to_string())
                    .unwrap_or_default()
            };
            (
                get("position"),
                get("type"),
                get("size_bytes"),
                get("sha256_encrypted"),
            )
        })
        .collect()
}

/// Steps 1-3 of the guide's manual path: block mode, File 0, File 3. Leaves
/// `file0.txt` and `front_index.toml` in a fresh `work` directory.
fn manual_front(h: &Heir, manual: &str, name: &str) -> PathBuf {
    let work = h.dir.join(name);
    std::fs::create_dir_all(&work).unwrap();
    for needle in ["setblk 524288", "> file0.txt", "> front_index.toml"] {
        let (ok, text) = run_guide(h, &work, &guide_block(manual, needle), &[], true);
        assert!(ok, "{needle}: {text}");
    }
    work
}

/// Step 5 of the guide for one envelope entry: read, trim, check, decrypt and
/// (when it opens) unpack into `work/env`. Returns whether `key` opened it, and
/// the output of the guide's commands.
fn manual_open_envelope(
    h: &Heir,
    manual: &str,
    work: &Path,
    key: &str,
    entry: &(String, String, String, String),
) -> (bool, String) {
    let (pos, _, size, sha) = entry;
    let subs = [
        ("POSITION", pos.clone()),
        ("SIZE", size.clone()),
        ("YOUR_KEY.age.key", h.key(key)),
    ];
    let (ok, text) = run_guide(h, work, &guide_block(manual, "> envelope.tar"), &subs, true);
    if !ok {
        assert!(
            text.contains("no identity matched any of the recipients"),
            "a key that does not open an envelope says so: {text}"
        );
        return (false, text);
    }
    assert!(
        text.contains(sha),
        "the sha256sum printed equals the front index's: {text}"
    );
    let _ = std::fs::remove_dir_all(work.join("env"));
    let (ok, text2) = run_guide(
        h,
        work,
        &guide_block(manual, "tar xf envelope.tar"),
        &[],
        true,
    );
    assert!(ok, "{text2}");
    (true, text)
}

/// The names `grep -E '^(name|uuid|snapshot_version) =' env/MANIFEST.toml`
/// prints, the guide's own line for step 6.
fn manual_unit_names(h: &Heir, manual: &str, work: &Path) -> Vec<String> {
    let (ok, text) = run_guide(
        h,
        work,
        &guide_block(manual, "snapshot_version) ="),
        &[],
        true,
    );
    assert!(ok, "{text}");
    text.lines()
        .filter_map(|l| l.strip_prefix("name = "))
        .map(|v| v.trim_matches('"').to_string())
        .collect()
}

/// S1: with the escrow (or operator) key the first envelope that decrypts is a
/// TENANT's and lists that tenant's units only; the operator envelope lists
/// every unit. A tenant's key does not open the operator envelope. The guide's
/// step 5 and 6 commands are the ones run.
#[test]
fn file_1_manual_path_with_an_escrow_key_opens_the_operator_envelope_for_every_unit() {
    let h = Heir::new();
    let guide = generate_system_guide_v2(LABEL, template().total_files);
    let manual = guide_section(&guide, "Manual Recovery Steps");
    let work = manual_front(&h, &manual, "manual-esc");
    let entries = front_index_entries(&work);

    // In tape order: the first envelope the escrow key opens is a tenant's.
    let mut first_open: Option<Vec<String>> = None;
    let mut operator: Option<Vec<String>> = None;
    for e in &entries {
        if e.1 != "tenant_envelope" && e.1 != "operator_envelope" {
            continue;
        }
        let (opened, _) = manual_open_envelope(&h, &manual, &work, "esc", e);
        assert!(opened, "the escrow key opens every envelope: {e:?}");
        let names = manual_unit_names(&h, &manual, &work);
        if first_open.is_none() {
            assert_eq!(e.1, "tenant_envelope", "tenant envelopes come first");
            first_open = Some(names.clone());
        }
        if e.1 == "operator_envelope" && operator.is_none() {
            operator = Some(names);
        }
    }
    let first = first_open.expect("an envelope opened");
    let all = operator.expect("an operator envelope");
    assert_eq!(first, ["photos/2019", "docs"], "one tenant's units only");
    let mut want: Vec<String> = template().units.iter().map(|u| u.name.clone()).collect();
    want.sort();
    let mut got = all.clone();
    got.sort();
    assert_eq!(got, want, "the operator envelope lists every unit");
    assert!(all.len() > first.len(), "a positive control");

    // (That the operator envelope's tar also lists PLAN.toml, and a tenant's
    // never does, is pinned in volume::build's tests; this fake tape's
    // operator envelope is a bare one.)

    // A tenant's key opens a tenant envelope and never the operator's.
    let work_t = manual_front(&h, &manual, "manual-bob");
    let mut opened = Vec::new();
    for e in &front_index_entries(&work_t) {
        if e.1 != "tenant_envelope" && e.1 != "operator_envelope" {
            continue;
        }
        let (ok, _) = manual_open_envelope(&h, &manual, &work_t, "bob", e);
        if ok {
            opened.push(e.1.clone());
            assert_eq!(
                manual_unit_names(&h, &manual, &work_t),
                ["ledgers"],
                "bob's own units"
            );
        }
    }
    assert!(!opened.is_empty(), "bob opens his own envelope");
    assert!(
        opened.iter().all(|t| t == "tenant_envelope"),
        "bob never opens the operator envelope: {opened:?}"
    );
}

/// S2: the whole-box procedure. `--all` puts several units each into its own
/// directory under `--to`, and a single unit straight into `--to`; a second
/// `--all` into a directory that already holds files is refused before the
/// tape is read, and `--overwrite` would keep a file only the older copy had.
/// The guide's per-unit `--unit UNIT --to /destination/UNIT` is the way
/// through: each lands in a directory of its own.
#[test]
fn file_1_box_procedure_gives_each_unit_a_directory_of_its_own() {
    let h = Heir::new();
    let guide = generate_system_guide_v2(LABEL, template().total_files);
    let sec = flat(&guide_section(
        &guide,
        "The rest of the box: other tapes, copies and the index",
    ));
    for needle in [
        "`--to` must be empty or new (a directory that is not empty is refused)",
        "A tape that holds the newest version needs no `--version`",
        "`--overwrite` would keep the files that only the older version had",
        "move it aside and restore the newer version into an empty one",
    ] {
        assert!(sec.contains(needle), "{needle:?}: {sec}");
    }
    assert!(
        sec.contains(
            "`./RESTORE.sh --restore --key YOUR_KEY.age.key --unit UNIT --to /destination/UNIT`"
        ),
        "the box step names a directory per unit: {sec}"
    );

    // --all: several units, each into --to/UNIT.
    let all_dest = h.sub("box-all");
    let key = h.key("alice");
    let args = ["--restore", "--key", &key, "--all", "--to", &all_dest];
    let (code, text) = h.run(&args);
    assert_eq!(code, 0, "{text}");
    for name in ["docs", "photos/2019"] {
        assert!(
            same_tree(&unit(name).src, &Path::new(&all_dest).join(name)),
            "{name} landed in {all_dest}/{name}"
        );
    }
    // A second --all into the same directory is refused, tape unread.
    let (code, text) = h.run(&args);
    assert_ne!(code, 0, "{text}");
    assert!(text.contains("is not empty"), "{text}");
    assert!(text.contains("Nothing has been read from tape."), "{text}");
    // A single unit goes straight into --to, not --to/UNIT.
    let one = h.sub("box-one");
    let (code, text) = h.run(&["--restore", "--key", &h.key("bob"), "--all", "--to", &one]);
    assert_eq!(code, 0, "{text}");
    assert!(same_tree(&unit("ledgers").src, Path::new(&one)), "{text}");

    // The guide's way through: one directory per unit, each restore its own.
    for name in ["docs", "photos/2019"] {
        let to = h.sub(&format!("box-units/{name}"));
        let (code, text) = h.run(&["--restore", "--key", &key, "--unit", name, "--to", &to]);
        assert_eq!(code, 0, "{name}: {text}");
        assert!(same_tree(&unit(name).src, Path::new(&to)), "{name}");
    }
    // --overwrite replaces what the restore holds and keeps what it does not.
    let to = h.sub("box-units/docs");
    std::fs::write(Path::new(&to).join("only-in-the-older-copy.txt"), "stale").unwrap();
    let (code, text) = h.run(&[
        "--restore",
        "--key",
        &key,
        "--unit",
        "docs",
        "--to",
        &to,
        "--overwrite",
    ]);
    assert_eq!(code, 0, "{text}");
    assert!(
        Path::new(&to).join("only-in-the-older-copy.txt").exists(),
        "--overwrite keeps a file the archive does not hold"
    );
}

/// `--all` restores the units of ONE envelope: the one that lists the most units
/// among those the keys open, not the union. With the escrow or operator key
/// that is the operator envelope (every unit); with a tenant's key it is that
/// tenant's; with several tenant keys it is the biggest tenant's only, and the
/// others need their own run (File 1 says so).
#[test]
fn all_restores_one_envelope_and_not_the_union_of_several_tenant_keys() {
    let h = Heir::new();
    let both = h.sub("both-keys");
    let (code, text) = h.run(&[
        "--restore",
        "--key",
        &h.key("alice"),
        "--key",
        &h.key("bob"),
        "--all",
        "--to",
        &both,
    ]);
    assert_eq!(code, 0, "{text}");
    for name in ["docs", "photos/2019"] {
        assert!(
            same_tree(&unit(name).src, &Path::new(&both).join(name)),
            "{name} (alice has the most units) is restored"
        );
    }
    assert!(
        !Path::new(&both).join("ledgers").exists(),
        "bob's unit is not: --all took one envelope, not both"
    );
    // Bob's key, on its own run into a directory of its own.
    let bobs = h.sub("bob-alone");
    let (code, text) = h.run(&["--restore", "--key", &h.key("bob"), "--all", "--to", &bobs]);
    assert_eq!(code, 0, "{text}");
    assert!(same_tree(&unit("ledgers").src, Path::new(&bobs)), "{text}");
    // An operator key alongside a tenant's: the operator envelope lists the most.
    let everything = h.sub("op-and-alice");
    let (code, text) = h.run(&[
        "--restore",
        "--key",
        &h.key("alice"),
        "--key",
        &h.key("op"),
        "--all",
        "--to",
        &everything,
    ]);
    assert_eq!(code, 0, "{text}");
    for name in ["docs", "photos/2019", "ledgers", "restaged", "nocat"] {
        assert!(
            Path::new(&everything).join(name).is_dir(),
            "{name} is restored by the operator envelope's --all"
        );
    }
}

/// M13/S3: File 0, read with the guide's `dd | tr | grep` line, shows the
/// label, uuid, generation and cartridge serial that tell an erased and
/// rewritten cartridge from the volume the index names; `--where` prints the
/// uuid of each holder; and a name the index lacks lists the units it has.
#[test]
fn file_1_box_commands_show_the_uuid_and_the_units_the_index_lists() {
    let h = Heir::new();
    let guide = generate_system_guide_v2(LABEL, template().total_files);
    let sec = guide_section(
        &guide,
        "The rest of the box: other tapes, copies and the index",
    );
    let block = guide_block(&sec, "grep -E '^(label|uuid");
    let work = h.dir.join("box-file0");
    std::fs::create_dir_all(&work).unwrap();
    let (ok, text) = run_guide(&h, &work, &block, &[], true);
    assert!(ok, "{text}");
    assert!(text.contains(&format!("label = \"{LABEL}\"")), "{text}");
    assert!(text.contains(&format!("uuid = \"{THIS_UUID}\"")), "{text}");
    assert!(text.contains("media_type = "), "{text}");
    assert!(text.contains("cartridge_serial = "), "{text}");
    // The same line dates the tape, which orders an unsealed one.
    assert!(
        text.contains("created_at = \"2026-10-05T00:00:00Z\""),
        "{text}"
    );
    // --where prints the uuid of a holder, for the comparison.
    let (code, text) = h.run(&["--where", "photos/2019", "--key", &h.key("esc")]);
    assert_eq!(code, 0, "{text}");
    assert!(text.contains(&format!("volume uuid {THIS_UUID}")), "{text}");
    // The units the index lists, by the guide's own command: every one, no cap.
    let listing = guide_block(&sec, "--where --all --key YOUR_KEY.age.key");
    assert!(
        listing.contains(&"./RESTORE.sh --where --all --key YOUR_KEY.age.key".to_string()),
        "{listing:#?}"
    );
    let (code, text) = h.run(&["--where", "--all", "--key", &h.key("esc")]);
    assert_eq!(code, 0, "{text}");
    for name in ["docs", "ledgers", "nocat", "photos/2019", "restaged"] {
        assert!(
            text.contains(&format!("Unit {name} (tenant ")),
            "{name}: {text}"
        );
    }
    let script = generate_restore_script_v2(LABEL, template().total_files);
    assert!(
        !script.contains("head -n 40"),
        "no 40-unit cap anywhere in the script"
    );
    // The index names holders besides this tape for units that are on it, and
    // the older version of photos/2019 that is on the other tape only.
    let (code, text) = h.run(&["--where", "photos/2019", "--key", &h.key("alice")]);
    assert_eq!(code, 0, "{text}");
    assert!(text.contains("tape OTHER02"), "{text}");
    assert!(text.contains("  Version 1: "), "{text}");
}

/// M-script and M-sudo: the guide's four fetch commands, run as written, save
/// the script under the tape's own name; the saved file runs; and the `env` form
/// the guide shows for `sudo` (sudo itself is not run here) hands the script its
/// device and work directory.
#[test]
fn file_1_script_is_saved_under_the_tapes_name_and_runs_from_it() {
    use std::os::unix::fs::PermissionsExt;
    let h = Heir::new();
    let guide = generate_system_guide_v2(LABEL, template().total_files);
    let auto = guide_section(&guide, "Automated Recovery (recommended)");
    let fetch = guide_block(&auto, &format!("> RESTORE-{LABEL}.sh"));
    assert_eq!(fetch.len(), 4, "{fetch:#?}");
    assert_eq!(fetch[3], format!("chmod +x RESTORE-{LABEL}.sh"));
    let work = h.dir.join("saved");
    std::fs::create_dir_all(&work).unwrap();
    let (ok, text) = run_guide(&h, &work, &fetch, &[], true);
    assert!(ok, "{text}");
    let saved = work.join(format!("RESTORE-{LABEL}.sh"));
    assert_eq!(
        std::fs::read_to_string(&saved).unwrap(),
        generate_restore_script_v2(LABEL, template().total_files),
        "the guide's commands read File 2 off the tape, byte for byte"
    );
    assert_ne!(
        std::fs::metadata(&saved).unwrap().permissions().mode() & 0o111,
        0,
        "chmod +x"
    );
    assert!(
        !work.join("RESTORE.sh").exists(),
        "nothing under the shared name"
    );
    // The saved name runs, as the guide's `./RESTORE.sh` lines mean it to.
    let (ok, text) = run_guide(
        &h,
        &work,
        &[format!(
            "TAPE_DEVICE=/dev/nst-fake ./RESTORE-{LABEL}.sh --info"
        )],
        &[],
        true,
    );
    assert!(ok, "{text}");
    assert!(text.contains("Verdict: SEALED"), "{text}");
    // The form sudo is given: `env` sets the device and the work directory
    // after sudo's own environment has been cleaned.
    let tmp = h.dir.join("work-tmp");
    std::fs::create_dir_all(&tmp).unwrap();
    let load = flat(&guide_section(
        &guide,
        "Loading the tape, and a drive that does not show up",
    ));
    assert!(
        load.contains("sudo env TAPE_DEVICE=/dev/nst0 TMPDIR=/tmp ./RESTORE.sh --info"),
        "{load}"
    );
    let (ok, text) = run_guide(
        &h,
        &work,
        &[format!(
            "env TAPE_DEVICE=/dev/nst-fake TMPDIR={} ./RESTORE-{LABEL}.sh --find-envelope --key {}",
            tmp.display(),
            h.key("esc")
        )],
        &[],
        true,
    );
    assert!(ok, "{text}");
    assert!(
        text.contains("Tape device:      /dev/nst-fake (from TAPE_DEVICE)"),
        "{text}"
    );
    assert!(text.contains("Decrypted envelope at file"), "{text}");
}

/// The done rule: an unsealed tape has no "Sealed at" (File 0's `created_at`,
/// from the guide's own `grep`, dates it), and a tape the key cannot open ends
/// in the words the rule quotes, with `--info` (no key) saying nothing of what
/// the tape holds.
#[test]
fn file_1_done_rule_words_are_what_the_script_prints() {
    let h = Heir::new();
    let guide = generate_system_guide_v2(LABEL, template().total_files);
    let bx = flat(&guide_section(
        &guide,
        "The rest of the box: other tapes, copies and the index",
    ));
    // An unsealed tape: the seal marker is gone.
    let seal = template().total_files as i64 - 1;
    std::fs::remove_file(h.dir.join("tape").join(seal.to_string())).unwrap();
    let (code, text) = h.run(&["--info"]);
    assert_eq!(code, 0, "{text}");
    assert!(text.contains("Verdict: UNSEALED"), "{text}");
    assert!(
        text.contains("Sealed at: unknown (tape not sealed)"),
        "{text}"
    );
    assert!(
        bx.contains("(`unknown (tape not sealed)` there means the tape is unsealed"),
        "{bx}"
    );
    let sec = guide_section(
        &guide,
        "The rest of the box: other tapes, copies and the index",
    );
    let work = h.dir.join("unsealed-file0");
    std::fs::create_dir_all(&work).unwrap();
    let (ok, text) = run_guide(
        &h,
        &work,
        &guide_block(&sec, "grep -E '^(label|uuid"),
        &[],
        true,
    );
    assert!(ok, "{text}");
    assert!(
        text.contains("created_at = \"2026-10-05T00:00:00Z\""),
        "File 0 dates the unsealed tape: {text}"
    );
    // --info says nothing of what the tape holds: no unit, tenant or key.
    let (_, info) = h.run(&["--info"]);
    for word in ["photos/2019", "ledgers", "alice", "tenant "] {
        assert!(!info.contains(word), "--info names {word}: {info}");
    }
    // A key that opens nothing: the words the rule quotes.
    let (code, text) = h.run(&["--find-envelope", "--key", &h.key("stranger")]);
    assert_eq!(code, 1, "{text}");
    assert!(
        text.contains("no envelope matched the key provided"),
        "{text}"
    );
    let (code, text) = h.run(&[
        "--find-envelope",
        "--key",
        &h.key("stranger"),
        "--key",
        &h.key("stranger"),
    ]);
    assert_eq!(code, 1, "{text}");
    assert!(
        text.contains("no envelope matched any of the 2 keys provided"),
        "{text}"
    );
    assert!(
        bx.contains("\"no envelope matched the key provided\" (or \"any of the N keys provided\")"),
        "{bx}"
    );
}

/// M6: RESTORE.sh written for another tape reads the tape in the drive for the
/// modes that only read it, with the WRONG TAPE box as a reminder; the label it
/// prints for the tape is the loaded tape's own.
#[test]
fn file_1_another_tapes_script_surveys_the_loaded_tape() {
    let h = Heir::new();
    std::fs::write(
        h.dir.join("RESTORE.sh"),
        generate_restore_script_v2("OTHER99", template().total_files),
    )
    .unwrap();
    let (code, text) = h.run(&["--info"]);
    assert_eq!(code, 0, "{text}");
    assert!(text.contains("WRONG TAPE?"), "{text}");
    assert!(
        text.contains(&format!("=== tapectl volume: {LABEL} ===")),
        "{text}"
    );
    assert!(text.contains("Verdict: SEALED"), "{text}");
    let (code, text) = h.run(&["--where", "ledgers", "--key", &h.key("esc")]);
    assert_eq!(code, 0, "{text}");
    assert!(text.contains("WRONG TAPE?"), "{text}");
    assert!(text.contains("Unit ledgers (tenant bob):"), "{text}");
    let (code, text) = h.run(&["--find-envelope", "--key", &h.key("esc")]);
    assert_eq!(code, 0, "{text}");
    assert!(text.contains("ledgers"), "{text}");
    // Every mode prints the loaded tape's own label; --verify's heading is the
    // script's (the guide says so) while it checks the tape in the drive.
    let (code, text) = h.run(&["--verify"]);
    assert_eq!(code, 0, "{text}");
    assert!(text.contains("WRONG TAPE?"), "{text}");
    assert!(
        text.contains(&format!("Tape identifies as: {LABEL}")),
        "{text}"
    );
    assert!(
        text.contains("=== keyless integrity walk: OTHER99 ==="),
        "{text}"
    );
    assert!(!text.contains("FAIL "), "the loaded tape passes: {text}");
}

/// Read slice `n` of `unit` with the guide's step 7 commands, leaving
/// `restore.N.dar` in `dir`: wind to its file, then the guide's slice block.
fn manual_read_slice(
    h: &Heir,
    manual: &str,
    dir: &Path,
    key: &str,
    sl: &tapectl::volume::manifest::ManifestSlice,
) {
    let wind = vec![
        "mt -f /dev/nst0 rewind".to_string(),
        format!(
            "mt -f /dev/nst0 fsf {}",
            sl.tape_position
                .expect("a whole unit's slice has a position")
        ),
    ];
    let (ok, text) = run_guide(h, dir, &wind, &[], true);
    assert!(ok, "{text}");
    let (ok, text) = run_guide(
        h,
        dir,
        &guide_block(manual, "> restore.NUMBER.dar"),
        &[
            ("SHA256_ENCRYPTED", sl.sha256_encrypted.clone()),
            ("ENCRYPTED_BYTES", sl.encrypted_bytes.to_string()),
            ("NUMBER", sl.number.to_string()),
            ("YOUR_KEY.age.key", h.key(key)),
        ],
        true,
    );
    assert!(ok, "slice {}: {text}", sl.number);
}

/// The files (not directories) whose data touches slice `lost`, from the
/// guide's `dar -l CATALOGUE -T slice -N -Q`.
fn files_touching_slice(work: &Path, stem: &str, lost: i64) -> Vec<String> {
    let o = Command::new("dar")
        .args([
            "-l",
            &format!("env/catalogs/{stem}"),
            "-T",
            "slice",
            "-N",
            "-Q",
        ])
        .current_dir(work)
        .output()
        .expect("dar -l");
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    let text = String::from_utf8_lossy(&o.stdout).to_string();
    let mut out = Vec::new();
    for line in text.lines() {
        let Some((range, rest)) = line.split_once('\t') else {
            continue;
        };
        let (lo, hi) = match range.trim().split_once('-') {
            Some((a, b)) => (a.parse::<i64>(), b.parse::<i64>()),
            None => (range.trim().parse::<i64>(), range.trim().parse::<i64>()),
        };
        let (Ok(lo), Ok(hi)) = (lo, hi) else { continue };
        // "[InRef][ ]  [-L-][-----][ ] -rw-rw-r-- sub/file"
        let mut cols = rest.split_whitespace();
        let perm = cols.by_ref().find(|c| c.len() == 10 && c.contains('r'));
        if perm.is_none_or(|p| !p.starts_with('-')) {
            continue;
        }
        let name: Vec<&str> = cols.collect();
        if (lo..=hi).contains(&lost) {
            out.push(name.join(" "));
        }
    }
    out.sort();
    out
}

/// S5: with no other copy of a slice, the guide's by-hand commands restore
/// every file the passing slices hold. dar does not stop at the gap: it names
/// each file it left (`not restored (user choice)`), leaves an empty or short
/// file of that name, exits 0, and counts them under "ignored"; RECOVERY.md's
/// `sha256sum` line then names those leftovers. With the LAST slice gone dar
/// stops (the guide's words), and `-0` restores the files before the break.
#[test]
fn file_1_no_other_copy_restores_what_the_passing_slices_hold() {
    let h = Heir::new();
    let guide = generate_system_guide_v2(LABEL, template().total_files);
    let manual = guide_section(&guide, "Manual Recovery Steps");
    let damaged = guide_section(&guide, "If a slice or a file is damaged");
    let g = flat(&damaged);
    for needle in [
        "leave the failed slices out: do not decrypt them, and do not put empty files in their place",
        "this is the one time a gap in the numbering is right",
        "For each of the others it prints `PATH not restored (user choice)`",
        "leaves a file of that name that is EMPTY or short",
        "The exit status is 0",
        "so a status of 0 does not mean the unit is whole",
        "`dar -l env/catalogs/NAME -T slice -N -Q`",
        "(\"The last file of the set is not present\", exit status 4)",
        "add `-0` (sequential read)",
        "tapectl is the program that wrote this tape",
        "`tapectl_version`",
        "reads around a damaged 64 KiB piece of a slice",
        "type `done` when it asks for another cartridge",
    ] {
        assert!(g.contains(needle), "{needle:?}: {g}");
    }
    assert!(!g.contains("http"), "no URL is given for tapectl: {g}");

    // Alice's envelope, by the guide's step 5, then the manifest.
    let work = manual_front(&h, &manual, "manual-gap");
    let entry = front_index_entries(&work)
        .into_iter()
        .find(|e| {
            e.1 == "tenant_envelope" && manual_open_envelope(&h, &manual, &work, "alice", e).0
        })
        .expect("alice's envelope");
    let _ = entry;
    let manifest = tapectl::volume::manifest::Manifest::from_toml(
        &std::fs::read_to_string(work.join("env/MANIFEST.toml")).unwrap(),
    )
    .unwrap();
    let unit = manifest
        .units
        .iter()
        .find(|u| u.name == "photos/2019")
        .expect("photos/2019 is alice's");
    let n = unit.slices.len();
    assert!(n >= 5, "a unit with a middle to lose: {n} slices");
    let lost = 3i64;
    let stem = format!("{}_v{}", &unit.uuid[..8], unit.snapshot_version);

    // Step 7, leaving slice 3 out: the guide's first-position block, its slice
    // block for each slice that passes, and its `fsf COUNT` over the gap.
    let dir = h.dir.join("gap");
    std::fs::create_dir_all(&dir).unwrap();
    let (ok, text) = run_guide(
        &h,
        &dir,
        &guide_block(&manual, "FIRST_TAPE_POSITION"),
        &[(
            "FIRST_TAPE_POSITION",
            unit.slices[0]
                .tape_position
                .expect("a whole unit's slice has a position")
                .to_string(),
        )],
        true,
    );
    assert!(ok, "{text}");
    let mut head = unit.slices[0]
        .tape_position
        .expect("a whole unit's slice has a position");
    for sl in &unit.slices {
        if sl.number == lost {
            continue;
        }
        let position = sl
            .tape_position
            .expect("a whole unit's slice has a position");
        if position > head {
            let (ok, text) = run_guide(
                &h,
                &dir,
                &guide_block(&manual, "fsf COUNT"),
                &[("COUNT", (position - head).to_string())],
                true,
            );
            assert!(ok, "{text}");
        }
        let (ok, text) = run_guide(
            &h,
            &dir,
            &guide_block(&manual, "> restore.NUMBER.dar"),
            &[
                ("SHA256_ENCRYPTED", sl.sha256_encrypted.clone()),
                ("ENCRYPTED_BYTES", sl.encrypted_bytes.to_string()),
                ("NUMBER", sl.number.to_string()),
                ("YOUR_KEY.age.key", h.key("alice")),
            ],
            true,
        );
        assert!(ok, "slice {}: {text}", sl.number);
        head = position + 1;
    }
    assert!(!dir.join(format!("restore.{lost}.dar")).exists());
    assert!(dir.join("restore.2.dar").exists() && dir.join("restore.4.dar").exists());
    // The catalogue (env/catalogs/..) is read from the envelope's directory.
    std::os::unix::fs::symlink(work.join("env"), dir.join("env")).unwrap();

    // The guide's three lines, into an empty directory.
    let dest = h.sub("gap-restored");
    std::fs::create_dir_all(&dest).unwrap();
    let (_, text) = run_guide(
        &h,
        &dir,
        &guide_block(&damaged, "dar -x restore -R /destination"),
        &[("/destination", dest.clone())],
        false,
    );
    let mut lines = text.lines();
    assert_eq!(lines.next(), Some("0"), "dar exits 0 over the gap: {text}");
    let named: Vec<String> = lines
        .filter_map(|l| l.strip_suffix(" not restored (user choice)"))
        .filter_map(|p| p.strip_prefix(&format!("{dest}/")))
        .map(str::to_string)
        .collect();
    let mut want = files_touching_slice(&dir, &stem, lost);
    let mut got = named.clone();
    want.sort();
    got.sort();
    assert!(!want.is_empty(), "a positive control: files touch slice 3");
    assert_eq!(got, want, "dar names the files whose slices are gone");
    let log = std::fs::read_to_string(dir.join("dar.log")).unwrap();
    let ignored: usize = log
        .lines()
        .find_map(|l| {
            l.trim()
                .strip_suffix(" inode(s) ignored (excluded by filters)")
        })
        .expect("dar's summary line")
        .trim()
        .parse()
        .unwrap();
    assert!(ignored >= named.len(), "counted as ignored: {log}");
    // Every other file is exactly the source; the named ones are empty or short.
    let src = &unit_on_tape("photos/2019").src;
    let mut intact = 0;
    for f in [
        "file0.bin",
        "file1.bin",
        "file2.bin",
        "file3.bin",
        "file4.bin",
    ] {
        let (a, b) = (src.join(f), Path::new(&dest).join(f));
        if named.iter().any(|n| n == f) {
            let len = std::fs::metadata(&b).map(|m| m.len()).unwrap_or(0);
            assert!(
                len < std::fs::metadata(&a).unwrap().len(),
                "{f}: a leftover, not the file"
            );
        } else {
            assert_eq!(
                std::fs::read(&a).unwrap(),
                std::fs::read(&b).unwrap(),
                "{f}"
            );
            intact += 1;
        }
    }
    assert!(intact >= 1, "some file came back whole");

    // RECOVERY.md's sha256sum line names the leftovers (and only those).
    let recovery = std::fs::read_to_string(work.join("env/RECOVERY.md")).unwrap();
    let line = recovery
        .lines()
        .find(|l| l.starts_with("V=1 U="))
        .expect("RECOVERY.md's by-hand line")
        .replace(
            "V=1 U='photos/2019'",
            &format!("V={} U='photos/2019'", unit.snapshot_version),
        )
        .replace("/destination", &dest);
    let (_, text) = run_guide(&h, &work.join("env"), &[line], &[], false);
    let failed: Vec<&str> = text
        .lines()
        .filter_map(|l| l.strip_suffix(": FAILED"))
        .filter(|p| p.ends_with(".bin"))
        .collect();
    let mut failed: Vec<String> = failed.iter().map(|s| s.to_string()).collect();
    failed.sort();
    let mut want_files: Vec<String> = named
        .iter()
        .filter(|n| n.ends_with(".bin"))
        .cloned()
        .collect();
    want_files.sort();
    assert_eq!(
        failed, want_files,
        "the hash check names the leftovers: {text}"
    );

    // The LAST slice gone: dar stops at once; -0 restores up to the break.
    let last = unit.slices.last().unwrap();
    let dir2 = h.dir.join("gap-last");
    std::fs::create_dir_all(&dir2).unwrap();
    for sl in unit.slices.iter().filter(|s| s.number != last.number) {
        manual_read_slice(&h, &manual, &dir2, "alice", sl);
    }
    std::os::unix::fs::symlink(work.join("env"), dir2.join("env")).unwrap();
    let dest2 = h.sub("gap-last-restored");
    std::fs::create_dir_all(&dest2).unwrap();
    let dar = |extra: &str, to: &str| -> (bool, String) {
        let cmd =
            format!("dar -x restore {extra} -R {to} -O -Q -N > dar.log 2>&1; echo $?; cat dar.log");
        run_guide(&h, &dir2, &[cmd], &[], false)
    };
    let (_, text) = dar("", &dest2);
    assert!(
        text.lines().next() != Some("0")
            && text.contains("The last file of the set is not present"),
        "dar stops when the last slice is gone: {text}"
    );
    assert_eq!(
        std::fs::read_dir(&dest2).unwrap().count(),
        0,
        "and restores nothing"
    );
    let dest3 = h.sub("gap-last-sequential");
    std::fs::create_dir_all(&dest3).unwrap();
    let (_, text) = dar("-0", &dest3);
    assert!(
        text.lines().next() != Some("0"),
        "stops at the break: {text}"
    );
    let mut whole = 0;
    let mut absent = 0;
    for f in [
        "file0.bin",
        "file1.bin",
        "file2.bin",
        "file3.bin",
        "file4.bin",
    ] {
        let (a, b) = (src.join(f), Path::new(&dest3).join(f));
        match std::fs::read(&b) {
            Ok(bytes) if bytes == std::fs::read(&a).unwrap() => whole += 1,
            Ok(_) => assert!(
                text.contains(&format!("{f} not restored (user choice)")),
                "a short file is named: {f}: {text}"
            ),
            Err(_) => absent += 1,
        }
    }
    assert!(whole >= 1, "files before the break come back whole: {text}");
    assert!(
        absent >= 1 || text.contains("not restored (user choice)"),
        "{text}"
    );
}

fn unit_on_tape(name: &str) -> UnitOnTape {
    unit(name)
}

/// S4: how to tell a unit was stored compressed. The guide says to look in
/// MANIFEST.toml's `dar_command` for an argument that starts `-z`, written
/// `\"-zgzip\"` there. Here the command is the one tapectl really records
/// (`spawn_archive`), put through the real manifest writer, and the guide's own
/// `grep` run on the result; an uncompressed run has no such argument. And the
/// catalogue cannot tell: its compression column is `-----` for a compressed
/// file, as the guide says.
#[test]
fn file_1_says_how_to_tell_a_compressed_unit_and_it_works() {
    use tapectl::dar::create::{spawn_archive, DarCreateParams};
    use tapectl::volume::layout::generate_manifest_toml;

    let h = Heir::new();
    let guide = generate_system_guide_v2(LABEL, template().total_files);
    let disk = guide_section(&guide, "Disk space");
    let f = flat(&disk);
    for needle in [
        "need more room, for a unit's slices beside its restored files",
        "Doing it by hand (\"Manual Recovery Steps\") is the same",
        "For a whole box, the destination needs the sum of the sizes of the units you restore",
        "**Compressed units.**",
        "`grep '^dar_command' env/MANIFEST.toml`",
        "An argument that starts `-z`, as in `\\\"-zgzip\\\"`",
        "shows `-----` in its compression column either way",
    ] {
        assert!(f.contains(needle), "{needle:?}: {f}");
    }
    assert!(
        !f.contains("need no more than that"),
        "the old ceiling sentence contradicted the spool rule: {f}"
    );
    let grep_line = disk
        .split('`')
        .find(|c| c.starts_with("grep '^dar_command'"))
        .expect("the guide's grep");

    let src = h.dir.join("compress-src");
    std::fs::create_dir_all(&src).unwrap();
    let text: String = "some compressible text line\n".repeat(20_000);
    std::fs::write(src.join("text.txt"), &text).unwrap();
    let recorded = |compression: &str| -> String {
        let catalogue = h.dir.join(format!("fly-{compression}"));
        let mut stream = spawn_archive(&DarCreateParams {
            dar_binary: "dar",
            source_path: &src,
            compression,
            exclude_patterns: &[],
            exclude_paths: &[],
            preserve_xattrs: true,
            preserve_fsa: true,
            on_fly_catalogue: &catalogue,
        })
        .expect("dar -c -");
        let command = stream.dar_command.clone();
        let mut out = stream.take_stdout();
        let mut bytes = Vec::new();
        std::io::Read::read_to_end(&mut out, &mut bytes).unwrap();
        drop(out);
        stream.finish().expect("dar finished");
        if compression != "none" {
            assert!(
                bytes.len() < text.len() / 4,
                "a positive control: the archive is compressed ({} bytes)",
                bytes.len()
            );
        }
        command
    };
    for (compression, expect) in [("gzip", true), ("none", false)] {
        let unit = ManifestUnit {
            name: "u".into(),
            uuid: "11111111-2222-3333-4444-555555555555".into(),
            snapshot_version: 1,
            stage_set_id: 1,
            may_span: false,
            dar_version: Some("2.7.13".into()),
            dar_command: Some(recorded(compression)),
            slices: vec![],
        };
        let work = h.dir.join(format!("compress-{compression}"));
        std::fs::create_dir_all(work.join("env")).unwrap();
        std::fs::write(
            work.join("env/MANIFEST.toml"),
            generate_manifest_toml("V", "t", std::slice::from_ref(&unit)),
        )
        .unwrap();
        let (ok, out) = run_guide(&h, &work, &[grep_line.to_string()], &[], true);
        assert!(ok, "{out}");
        let marked = out.contains(&format!("\\\"-z{compression}\\\""));
        assert_eq!(marked, expect, "{compression}: {out}");
        if !expect {
            assert!(
                !out.contains("\\\"-z"),
                "no -z argument when uncompressed: {out}"
            );
        }
    }

    // The catalogue's compression column is blank of information either way.
    let dir = h.dir.join("compress-cat");
    std::fs::create_dir_all(&dir).unwrap();
    let run = |args: &[&str]| {
        let o = Command::new("dar")
            .args(args)
            .current_dir(&dir)
            .output()
            .unwrap();
        assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
        String::from_utf8_lossy(&o.stdout).to_string()
    };
    run(&[
        "-c",
        "arch",
        "-R",
        src.to_str().unwrap(),
        "-zgzip",
        "-N",
        "-Q",
    ]);
    run(&["-C", "cat", "-A", "arch", "-N", "-Q"]);
    let listing = run(&["-l", "cat", "-N", "-Q"]);
    assert!(
        listing
            .lines()
            .any(|l| l.contains("text.txt") && l.contains("[-----]")),
        "{listing}"
    );
    let real = run(&["-l", "arch", "-N", "-Q"]);
    assert!(
        real.lines()
            .any(|l| l.contains("text.txt") && !l.contains("[-----]")),
        "the archive itself does show a ratio, which the envelope lacks: {real}"
    );
}

// ---- tapectl 1.5.0: the sixth read's commands, run as written (#485) ----
//
// File 1's new text shows commands for three things a fresh reader asked for:
// the files only an older version has (item 2), a unit's `dar_command` from an
// escrow key without unpacking an envelope (item 4), and the way back after a
// failed `dd` (item 7). Each is run here against the fake tape, the real dar
// and the real `grep`, `awk` and `tar`, from the guide's own lines.

/// A volume with two versions of one unit: version 1 holds `keep.txt` and
/// `gone.txt`, version 2 holds `keep.txt` and `new.txt`.
fn versions_template() -> &'static Template {
    static T: OnceLock<Template> = OnceLock::new();
    T.get_or_init(|| {
        let v = |version, seed, names: &'static [&'static str]| UnitSpec {
            cat: Cat::Matched,
            name: "docs",
            uuid: "22222222-3333-4444-5555-666666666666",
            version,
            seed,
            files: names.len(),
            file_len: 30_000,
            names,
            corrupt: &[],
            tamper: &[],
        };
        let tenants = vec![TenantSpec {
            name: "alice",
            units: vec![
                v(1, 1, &["keep.txt", "gone.txt"]),
                v(2, 2, &["keep.txt", "new.txt"]),
            ],
        }];
        build_template("versions", &tenants, true)
    })
}

/// `./RESTORE.sh` in a guide line is this Heir's script, with the work
/// directory and a device name that mean nothing to the fake.
fn this_script(h: &Heir) -> (&'static str, String) {
    (
        "./RESTORE.sh",
        format!(
            "env TMPDIR={} TAPE_DEVICE=/dev/nst-fake bash {}",
            h.dir.join("tmp").display(),
            h.dir.join("RESTORE.sh").display()
        ),
    )
}

/// "Files deleted before the newest version": the guide's three lines list
/// the paths two versions have, and print those only the older one has. The
/// file the newer version lacks is printed and nothing else is; swapped, the
/// last line prints the file only the newer version has (it is directional);
/// and the restore line brings the deleted file back from the older version
/// into a directory of its own, byte for byte.
#[test]
fn file_1_lists_the_files_only_an_older_version_has_and_restores_one() {
    let t = versions_template();
    let h = Heir::of(t, &[]);
    let guide = generate_system_guide_v2(LABEL, t.total_files);
    let bx = guide_section(
        &guide,
        "The rest of the box: other tapes, copies and the index",
    );
    let lists = guide_block(&bx, "newer.paths");
    assert_eq!(lists.len(), 3, "{lists:?}");
    let work = h.dir.join("deleted");
    std::fs::create_dir_all(&work).unwrap();
    // The guide's example says versions 3 and 2; this tape has 2 and 1.
    let versions = |lines: Vec<String>| -> Vec<String> {
        lines
            .into_iter()
            .map(|l| {
                l.replace("--version 2", "--version @OLD")
                    .replace("--version 3", "--version 2")
                    .replace("--version @OLD", "--version 1")
            })
            .collect()
    };
    let subs = |h: &Heir| -> Vec<(&'static str, String)> {
        vec![
            ("UNIT", "docs".to_string()),
            ("YOUR_KEY.age.key", h.key("esc")),
            this_script(h),
        ]
    };
    let (ok, text) = run_guide(&h, &work, &versions(lists[..2].to_vec()), &subs(&h), true);
    assert!(ok, "{text}");
    let newer = std::fs::read_to_string(work.join("newer.paths")).unwrap();
    let older = std::fs::read_to_string(work.join("older.paths")).unwrap();
    for p in ["keep.txt", "new.txt", "sub", "sub/note.txt"] {
        assert!(newer.lines().any(|l| l == p), "newer has {p}: {newer}");
    }
    for p in ["keep.txt", "gone.txt", "sub", "sub/note.txt"] {
        assert!(older.lines().any(|l| l == p), "older has {p}: {older}");
    }
    assert!(!newer.lines().any(|l| l == "gone.txt"), "{newer}");
    assert!(!older.lines().any(|l| l == "new.txt"), "{older}");

    let (ok, printed) = run_guide(&h, &work, &lists[2..], &[], true);
    assert!(ok, "{printed}");
    assert_eq!(printed, "gone.txt\n", "only the deleted file is printed");
    // Directional: the same line with the files swapped prints the new file.
    let swapped = lists[2].replace("newer.paths older.paths", "older.paths newer.paths");
    assert_ne!(swapped, lists[2]);
    let (ok, printed) = run_guide(&h, &work, &[swapped], &[], true);
    assert!(ok, "{printed}");
    assert_eq!(printed, "new.txt\n");

    // To restore the deleted file, from the older version, into a directory
    // of its own.
    let restore = guide_block(&bx, "UNIT-v2");
    let dest = h.sub("old-version");
    let mut restore_subs = vec![("PATH", "gone.txt".to_string())];
    restore_subs.push(("UNIT", "docs".to_string()));
    restore_subs.push(("YOUR_KEY.age.key", h.key("esc")));
    restore_subs.push(("/destination", dest.clone()));
    restore_subs.push(this_script(&h));
    let (ok, text) = run_guide(&h, &work, &versions(restore), &restore_subs, true);
    assert!(ok, "{text}");
    assert_eq!(
        std::fs::read(Path::new(&dest).join("docs-v2/gone.txt")).unwrap(),
        content(101, 30_000),
        "the file only version 1 had comes back from version 1"
    );
}

/// "Compressed units": an escrow-only heir reads each unit's `dar_command`
/// from the operator envelope with the guide's own lines. `--find-envelope`
/// names the operator envelope ("Decrypted envelope at file N (tenant:
/// operator)"), File 3 gives its size, and the five commands print every
/// unit's name, version and `dar_command` without unpacking the envelope.
#[test]
fn file_1_short_way_to_the_dar_command_works_with_the_escrow_key() {
    let h = Heir::new();
    let guide = generate_system_guide_v2(LABEL, template().total_files);
    let disk = guide_section(&guide, "Disk space");
    let manual = guide_section(&guide, "Manual Recovery Steps");
    let work = h.dir.join("dar-command");
    std::fs::create_dir_all(&work).unwrap();

    let (code, text) = h.run(&["--find-envelope", "--key", &h.key("esc")]);
    assert_eq!(code, 0, "{text}");
    let operator_at = text
        .lines()
        .find_map(|l| {
            l.split("Decrypted envelope at file ")
                .nth(1)
                .filter(|rest| rest.contains("(tenant: operator)"))
                .and_then(|rest| rest.split_whitespace().next())
                .map(str::to_string)
        })
        .unwrap_or_else(|| panic!("no operator envelope named:\n{text}"));

    // File 3 into front_index.toml, as step 3 of the manual steps does.
    for needle in ["setblk 524288", "> file0.txt", "> front_index.toml"] {
        let (ok, out) = run_guide(&h, &work, &guide_block(&manual, needle), &[], true);
        assert!(ok, "{needle}: {out}");
    }
    let grep = disk
        .split('`')
        .find(|c| c.starts_with("grep -A3 '^position = POSITION$'"))
        .expect("the guide's grep of the front index")
        .to_string();
    let (ok, entry) = run_guide(
        &h,
        &work,
        &[grep],
        &[("POSITION", operator_at.clone())],
        true,
    );
    assert!(ok, "{entry}");
    assert!(
        entry.contains("type = \"operator_envelope\""),
        "the entry is the operator envelope's: {entry}"
    );
    let size = entry
        .lines()
        .find_map(|l| l.strip_prefix("size_bytes = "))
        .unwrap_or_else(|| panic!("no size_bytes in:\n{entry}"))
        .to_string();

    let (ok, out) = run_guide(
        &h,
        &work,
        &guide_block(&disk, "tar -xOf"),
        &[
            ("POSITION", operator_at),
            ("SIZE", size),
            ("YOUR_KEY.age.key", h.key("esc")),
        ],
        true,
    );
    assert!(ok, "{out}");
    for unit in ["photos/2019", "docs", "ledgers", "restaged", "nocat"] {
        assert!(
            out.lines().any(|l| l == format!("name = \"{unit}\"")),
            "{unit} is listed:\n{out}"
        );
    }
    assert_eq!(
        out.lines()
            .filter(|l| *l == "dar_command = \"dar -c base -R /src -N\"")
            .count(),
        5,
        "a dar_command for each of the five units:\n{out}"
    );
    assert!(
        out.lines().any(|l| l.starts_with("snapshot_version = ")),
        "{out}"
    );
}

/// A `dd` that stops with an error leaves the head INSIDE that file (the fake
/// loses the position, as the st driver does mid-file), so the next slice
/// read straight after is not that slice. The guide's way back is to rewind
/// and `fsf` to the `tape_position` of the slice wanted: that reads it, and
/// the failed slice again.
#[test]
fn file_1_after_a_failed_dd_a_rewind_and_fsf_reads_the_next_slice() {
    let h = Heir::new();
    let guide = generate_system_guide_v2(LABEL, template().total_files);
    let manual = guide_section(&guide, "Manual Recovery Steps");
    let u = unit("photos/2019");
    assert!(u.positions.len() >= 3, "{:?}", u.positions);
    let work = h.dir.join("dd-error");
    std::fs::create_dir_all(&work).unwrap();
    let slice = |i: usize, fail_at: Option<i64>| -> (bool, String) {
        let mut lines: Vec<String> = Vec::new();
        if let Some(p) = fail_at {
            lines.push(format!("export FAKE_DD_FAIL={p}"));
        }
        lines.extend(guide_block(&manual, "> restore.NUMBER.dar"));
        run_guide(
            &h,
            &work,
            &lines,
            &[
                ("SHA256_ENCRYPTED", u.hashes[i].clone()),
                ("ENCRYPTED_BYTES", u.sizes[i].to_string()),
                ("NUMBER", (i + 1).to_string()),
                ("YOUR_KEY.age.key", h.key("alice")),
            ],
            false,
        )
    };
    let go_to = |p: i64| -> (bool, String) {
        run_guide(
            &h,
            &work,
            &[
                "mt -f /dev/nst0 rewind".to_string(),
                format!("mt -f /dev/nst0 fsf {p}"),
            ],
            &[],
            true,
        )
    };

    let (ok, text) = go_to(u.positions[0]);
    assert!(ok, "{text}");
    let (_, text) = slice(0, None);
    assert!(work.join("restore.1.dar").exists(), "slice 1 reads: {text}");

    // Slice 2 fails with an I/O error.
    let (_, text) = slice(1, Some(u.positions[1]));
    assert!(text.contains("Input/output error"), "{text}");
    assert!(!work.join("restore.2.dar").exists(), "{text}");

    // Straight on to slice 3, as the counting would say: the head is lost.
    let (_, text) = slice(2, None);
    assert!(
        !work.join("restore.3.dar").exists(),
        "slice 3 is not read from where the failed read left the head: {text}"
    );
    let _ = std::fs::remove_file(work.join("restore.3.dar.age"));

    // The guide's way back: rewind, `fsf` to the `tape_position` wanted.
    let (ok, text) = go_to(u.positions[2]);
    assert!(ok, "{text}");
    let (_, text) = slice(2, None);
    assert!(work.join("restore.3.dar").exists(), "slice 3 reads: {text}");
    // And the slice that failed can be tried again the same way.
    let (ok, text) = go_to(u.positions[1]);
    assert!(ok, "{text}");
    let (_, text) = slice(1, None);
    assert!(work.join("restore.2.dar").exists(), "slice 2 reads: {text}");
}

// ---- a tape that ended early: the short seal (tapectl 1.6.0; volume-format-v2.md
// §1.2 rule 6, §4.1, §4.2; ADR-0012 items 110-111, #420) ----

/// The main tape, ended early in the first slice of bob's `ledgers`: the
/// drive's early warning came there, so the write finished that slice and
/// sealed the tape after it. `photos/2019` and `docs` lie wholly before the
/// end and are on the tape whole; `ledgers` (its later slices are past the
/// end), `restaged` and `nocat` (planned after it) are cut.
fn short_heir() -> (Heir, i64) {
    let h = Heir::new();
    let ledgers = unit("ledgers");
    assert!(
        ledgers.positions.len() >= 2,
        "a unit the end can cut in two"
    );
    let k = ledgers.positions[0];
    for u in ["photos/2019", "docs"] {
        assert!(unit(u).positions.iter().all(|p| *p <= k), "{u} is whole");
    }
    for u in CUT {
        assert!(unit(u).positions.iter().any(|p| *p > k), "{u} is cut");
    }
    h.cut(k);
    (h, k)
}

/// The units the end of `short_heir`'s tape cut.
const CUT: [&str; 3] = ["ledgers", "restaged", "nocat"];

/// The planned seal position of the main tape (File 0's `seal_marker`).
fn planned_seal() -> i64 {
    template().total_files as i64 - 1
}

/// `--info` on a short tape: the verdict stays SEALED, a line says after which
/// file the tape ended and which files of the plan are not on it, the plan's
/// later files are one NOT ON TAPE line in the file map (ADR-0012 item 140),
/// and the normal short ending draws no warning that the seal is not where
/// File 0 points (rule 6).
#[test]
fn a_short_tape_is_sealed_and_info_says_where_it_ended() {
    let (h, k) = short_heir();
    let m = planned_seal();
    let (code, text) = h.run(&["--info"]);
    assert_eq!(code, 0, "{text}");
    assert!(text.contains("Verdict: SEALED"), "{text}");
    assert!(
        text.contains(&format!(
            "  The tape ended early, after file {k}: its seal marker, the last file, is\n  file {} (a short seal). Files {} to {} of the plan are not on it,",
            k + 1,
            k + 1,
            m - 1
        )),
        "{text}"
    );
    assert!(
        !text.contains("WARNING: File 0 says the seal marker"),
        "the normal short ending draws no warning:\n{text}"
    );
    let marked: Vec<&str> = text
        .lines()
        .filter(|l| l.trim_start().starts_with("NOT ON TAPE"))
        .collect();
    assert_eq!(
        marked,
        vec![format!(
            "  NOT ON TAPE  files {} to {m}  (planned; the tape ended after file {k}, and its seal marker is file {})",
            k + 1,
            k + 1
        )],
        "one line, from K+1 to the planned seal:\n{text}"
    );
    let listed: Vec<i64> = text
        .lines()
        .filter_map(|l| l.strip_prefix("  "))
        .filter_map(|l| l.split_whitespace().next()?.parse().ok())
        .collect();
    assert_eq!(
        listed,
        (0..=k).collect::<Vec<_>>(),
        "the file map lists the files on the tape, and no planned one after them:\n{text}"
    );
    assert_eq!(h.count("mt rewind"), 1, "{:#?}", h.ops());

    // The positive control: the full tape says neither.
    let (code, text) = Heir::new().run(&["--info"]);
    assert_eq!(code, 0, "{text}");
    assert!(!text.contains("ended early"), "{text}");
    assert!(!text.contains("NOT ON TAPE"), "{text}");
}

/// `--verify` on a short tape hashes files 0 to `ended_after`, prints one NOT
/// ON TAPE line for the planned files after it (ADR-0012 item 140), which is
/// not a failure, and reads nothing past the end (R3). The seal is read before
/// any slice (R1), once.
#[test]
fn verify_on_a_short_tape_hashes_what_is_on_it_and_names_the_rest_not_on_tape() {
    let (h, k) = short_heir();
    let m = planned_seal();
    let (code, text) = h.run(&["--verify"]);
    assert_eq!(code, 0, "NOT ON TAPE is not a failure:\n{text}");
    let passes = text.lines().filter(|l| l.starts_with("PASS  file")).count() as i64;
    assert_eq!(
        passes,
        k + 1,
        "files 0 to {k}, File 3 by the seal's binding:\n{text}"
    );
    let gone: Vec<&str> = text
        .lines()
        .filter(|l| l.starts_with("NOT ON TAPE"))
        .collect();
    assert_eq!(
        gone,
        vec![format!(
            "NOT ON TAPE  files {} to {m}  (planned; the tape ended after file {k}, and its seal marker is file {})",
            k + 1,
            k + 1
        )],
        "one line, from K+1 to the planned seal:\n{text}"
    );
    assert!(!text.contains("FAIL"), "{text}");
    assert!(
        text.contains(&format!(
            "VERIFY: PASS — every file on this tape matches the front index. The tape ended early, after\n        file {k} (its seal marker is file {}): the {} planned file(s) marked NOT ON TAPE",
            k + 1,
            m - k
        )),
        "{text}"
    );
    let (walked, rewinds) = walk(&h);
    assert_eq!(rewinds, 1, "{:#?}", h.ops());
    assert!(ascending(&walked), "{walked:?}");
    assert!(
        walked.iter().all(|p| *p <= k),
        "nothing past the end: {walked:?}"
    );
    assert_eq!(
        reads(&h).iter().filter(|p| **p == k + 1).count(),
        1,
        "the seal, once:\n{:#?}",
        h.ops()
    );
}

/// A unit wholly before the end restores from a short tape as from any other.
/// A unit the end cut is refused by name before any slice is read, saying the
/// tape ended before its slices and naming `--where` (R4): the one cut in two
/// (its first slice is on the tape and hashes right), one planned wholly after
/// the end, and `--path` into the one cut in two.
#[test]
fn a_whole_unit_restores_from_a_short_tape_and_a_cut_one_is_refused_by_name() {
    let (h, k) = short_heir();
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
    assert!(text.contains("RESTORE COMPLETE"), "{text}");
    assert!(same_tree(&docs.src, Path::new(&dest)), "{text}");
    assert!(
        text.contains(&format!("This tape ended early, after file {k}:")),
        "{text}"
    );
    let (walked, rewinds) = walk(&h);
    assert_eq!(rewinds, 1, "{:#?}", h.ops());
    assert!(ascending(&walked), "{walked:?}");

    let first_slice = template().units[0].positions[0];
    for (key, name, version) in [("bob", "ledgers", 3), ("carol", "restaged", 4)] {
        let last = *unit(name).positions.last().unwrap();
        let dest = h.sub(&format!("cut-{name}"));
        let (code, text) = h.run(&[
            "--restore",
            "--key",
            &h.key(key),
            "--unit",
            name,
            "--to",
            &dest,
        ]);
        assert_eq!(code, 1, "{name}: {text}");
        for needle in [
            format!("unit '{name}' (version {version}) is not on this tape whole: the tape ended early,"),
            format!("after tape file {k} (its seal marker, the last file, says so)"),
            format!("slices of this version run to tape file {last}."),
            format!("--where '{name}' --key "),
            "No slice has been read.".to_string(),
        ] {
            assert!(text.contains(&needle), "{name}: {needle:?}:\n{text}");
        }
        assert!(
            walk(&h).0.iter().all(|p| *p < first_slice),
            "{name}: no slice is read:\n{:#?}",
            h.ops()
        );
        assert_eq!(
            std::fs::read_dir(&dest).map(|d| d.count()).unwrap_or(0),
            0,
            "{name}: nothing restored"
        );
    }
    let (code, text) = h.run(&[
        "--restore",
        "--key",
        &h.key("bob"),
        "--unit",
        "ledgers",
        "--path",
        "file0.bin",
        "--to",
        &h.sub("cut-path"),
    ]);
    assert_eq!(code, 1, "{text}");
    assert!(
        text.contains("unit 'ledgers' (version 3) is not on this tape whole"),
        "{text}"
    );
    assert!(
        walk(&h).0.iter().all(|p| *p < first_slice),
        "{:#?}",
        h.ops()
    );
}

/// `--all` on a short tape restores every unit the tape holds whole and names
/// the ones the end cut, before RESTORE COMPLETE, with the way to the tapes
/// that hold them. Nothing of a cut unit is read or written.
#[test]
fn all_on_a_short_tape_restores_every_whole_unit_and_names_the_cut_ones() {
    let (h, k) = short_heir();
    let dest = h.dir.join("restored");
    let (code, text) = h.run(&[
        "--restore",
        "--key",
        &h.key("esc"),
        "--all",
        "--to",
        dest.to_str().unwrap(),
    ]);
    assert_eq!(code, 0, "{text}");
    for u in ["photos/2019", "docs"] {
        assert!(same_tree(&unit(u).src, &dest.join(u)), "{u}:\n{text}");
    }
    let note = text
        .find("NOT ON THIS TAPE WHOLE")
        .unwrap_or_else(|| panic!("the cut units are named:\n{text}"));
    let complete = text.find("RESTORE COMPLETE").expect("RESTORE COMPLETE");
    assert!(note < complete, "named before RESTORE COMPLETE:\n{text}");
    let named = &text[note..complete];
    assert!(named.contains(&format!("after file {k}")), "{named}");
    for u in CUT {
        assert!(named.contains(&format!("'{u}' (version")), "{u}: {named}");
        assert!(!dest.join(u).exists(), "{u} is not restored");
    }
    assert!(named.contains("--where UNIT --key "), "{named}");
    let (walked, rewinds) = walk(&h);
    assert_eq!(rewinds, 1, "{:#?}", h.ops());
    assert!(ascending(&walked), "{walked:?}");
    assert!(walked.iter().all(|p| *p <= k), "{walked:?}");
}

/// `--list` still lists a cut unit's files (its dar catalogue is in the
/// envelope, and no slice is read), and says the tape does not hold it whole;
/// a whole unit's listing says nothing of the kind.
#[test]
fn list_on_a_short_tape_lists_a_cut_units_files_and_says_it_is_not_whole() {
    let (h, k) = short_heir();
    let first_slice = template().units[0].positions[0];
    let (code, text) = h.run(&["--list", "--key", &h.key("bob"), "--unit", "ledgers"]);
    assert_eq!(code, 0, "{text}");
    assert!(text.contains("=== ledgers, snapshot version 3"), "{text}");
    assert!(
        text.contains(&format!(
            "NOT ON THIS TAPE WHOLE: the tape ended early, after file {k}, and this version's"
        )),
        "{text}"
    );
    assert!(text.contains("--where 'ledgers' --key "), "{text}");
    assert!(text.contains("file0.bin"), "its files are listed:\n{text}");
    assert!(
        walk(&h).0.iter().all(|p| *p < first_slice),
        "{:#?}",
        h.ops()
    );

    let (code, text) = h.run(&["--list", "--key", &h.key("alice"), "--unit", "docs"]);
    assert_eq!(code, 0, "{text}");
    assert!(!text.contains("NOT ON THIS TAPE WHOLE"), "{text}");
}

/// `--where` on a short tape prints this tape's row of a version it cut as
/// CUT, never as a holder, and `--where --all` does not count the loaded tape
/// as holding it; a version the tape holds whole reads as before. `--find`
/// marks the holder the same way.
#[test]
fn where_and_find_on_a_short_tape_mark_this_tapes_cut_versions() {
    let (h, _k) = short_heir();
    let (code, text) = h.run(&["--where", "ledgers", "--key", &h.key("bob")]);
    assert_eq!(code, 0, "{text}");
    let here = text
        .lines()
        .find(|l| l.contains(&format!("tape {LABEL}  (")))
        .unwrap_or_else(|| panic!("this tape's row:\n{text}"));
    assert!(here.contains("(CUT on this tape: it ended early"), "{here}");
    assert!(!here.contains("(this tape)"), "{here}");
    assert!(
        text.contains("A holder marked CUT on this tape is the loaded tape, which ended early"),
        "{text}"
    );

    let (code, text) = h.run(&["--where", "docs", "--key", &h.key("alice")]);
    assert_eq!(code, 0, "{text}");
    assert!(
        text.contains(&format!("tape {LABEL}  (this tape)")),
        "{text}"
    );
    assert!(!text.contains("CUT on this tape"), "{text}");

    let (code, text) = h.run(&["--where", "--all", "--key", &h.key("esc")]);
    assert_eq!(code, 0, "{text}");
    assert!(
        text.contains("5 unit(s) in this index; this tape holds the newest version of 2 of them."),
        "{text}"
    );
    assert!(
        text.contains("Unit ledgers (tenant bob): newest version 3, NOT on this tape"),
        "{text}"
    );
    assert!(
        text.contains("Unit docs (tenant alice): newest version 1, on this tape"),
        "{text}"
    );
    assert!(
        text.contains(&format!("{LABEL} (CUT on this tape: not held whole)")),
        "the absent list names the cut copy as such:\n{text}"
    );

    let (code, text) = h.run(&["--find", "file0", "--key", &h.key("bob")]);
    assert_eq!(code, 0, "{text}");
    let holders = block(&text, "Tapes holding ledgers version 3:");
    assert!(
        holders
            .iter()
            .any(|l| l.contains("(CUT on this tape: it ended early")),
        "{holders:#?}\n{text}"
    );

    // The positive control: on the full tape the same rows are holders.
    let full = Heir::new();
    let (code, text) = full.run(&["--where", "--all", "--key", &full.key("esc")]);
    assert_eq!(code, 0, "{text}");
    assert!(
        text.contains("5 unit(s) in this index; this tape holds the newest version of 5 of them."),
        "{text}"
    );
    assert!(!text.contains("CUT on this tape"), "{text}");
}

/// `--fill-from` reads the tape in the drive as the other copy, and a tape
/// whose own seal says it ended before the unit's last slice does not hold the
/// unit whole: the fill is refused by name and the list is left as it was
/// (R4, R5).
#[test]
fn a_fill_from_a_tape_that_cut_the_unit_is_refused_and_the_list_kept() {
    let (h, _k) = short_heir();
    let dir = h.dir.join("restored");
    std::fs::create_dir_all(&dir).unwrap();
    let list = dir.join(".tapectl-damaged-files.txt");
    let body = "#tapectl-damaged-files\t1\tunit=ledgers\tuuid=33333333-4444-5555-6666-777777777777\tversion=3\nfile0.bin\n";
    std::fs::write(&list, body).unwrap();
    let (code, text) = h.run(&[
        "--restore",
        "--key",
        &h.key("bob"),
        "--unit",
        "ledgers",
        "--fill-from",
        list.to_str().unwrap(),
        "--to",
        dir.to_str().unwrap(),
    ]);
    assert_eq!(code, 1, "{text}");
    assert!(
        text.contains("unit 'ledgers' (version 3) is not on this tape whole"),
        "{text}"
    );
    assert!(
        text.contains("A fill uses a tape that holds the unit whole. Nothing was changed."),
        "{text}"
    );
    assert_eq!(std::fs::read_to_string(&list).unwrap(), body);
    let first_slice = template().units[0].positions[0];
    assert!(
        walk(&h).0.iter().all(|p| *p < first_slice),
        "{:#?}",
        h.ops()
    );
}

/// `--swap-tapes` reads the second cartridge's seal before any of its slices,
/// and takes nothing from past its `ended_after` (R5): a needed slice that lies
/// after the end of a short copy is not looked for there, and one at or before
/// it is taken, as from any copy.
#[test]
fn swap_tapes_reads_the_second_tapes_seal_first_and_takes_nothing_past_its_end() {
    let t = template();
    let photos = unit("photos/2019");
    let n = photos.positions.len() as i64;
    let all: Vec<Vec<u8>> = (0..photos.positions.len())
        .map(|i| good_slice(t, &photos, i))
        .collect();
    // On the copy the slices lie reversed after a decoy at File 4: slice i
    // (counted from 0) at File 5 + (n - 1 - i).
    let on_copy = |i: i64| 5 + (n - 1 - i);
    for (k2, taken) in [(on_copy(1), true), (on_copy(1) - 1, false)] {
        let h = Heir::new();
        flip_in_tape(&h, photos.positions[1]);
        second_tape(&h, "COPY02", "c0c0c0c0-0002-4000-8000-000000000002", &all);
        cut_tape_dir(&h.dir.join("tape2"), "COPY02", k2);
        let dest = h.sub("restored");
        let (code, text) = run_with(
            &h,
            &restore_args(&h, "photos/2019", &dest),
            &["--swap-tapes"],
            "\ndone\n",
            &[("FAKE_SWAP_ON_SETBLK", "2")],
        );
        assert!(
            text.contains(&format!(
                "This cartridge ended early, after file {k2} (its seal marker says so): nothing after that is read from it."
            )),
            "{k2}: {text}"
        );
        let (_, (second, rewinds)) = tapes_of(&h);
        assert_eq!(rewinds, 1, "{k2}: {:#?}", h.ops());
        assert!(
            second.iter().all(|p| *p <= k2),
            "{k2}: nothing past its end is read: {second:?}"
        );
        if taken {
            assert_eq!(code, 0, "{k2}: {text}");
            assert!(text.contains("RESTORE COMPLETE"), "{text}");
            assert!(same_tree(&photos.src, Path::new(&dest)), "{text}");
        } else {
            assert_eq!(code, 1, "{k2}: {text}");
            assert!(
                text.contains("No file in the front index of this cartridge has the sha256 of a slice still needed"),
                "{text}"
            );
            assert!(text.contains("RESTORE INCOMPLETE"), "{text}");
        }
    }
}

/// The 1.6.0 script reads a 1.5.0 tape exactly as before (§6 "Since 1.6.0"):
/// SEALED with no word of an early end, `--verify` passes every file, and no
/// mode winds to the seal before the envelopes: a restore and `--where` rewind
/// once and never space to end of data.
#[test]
fn the_1_6_0_script_reads_a_1_5_0_tape_as_before() {
    let t = template_150();
    let h = Heir::of(t, &[]);
    let (code, text) = h.run(&["--info"]);
    assert_eq!(code, 0, "{text}");
    assert!(text.contains("Verdict: SEALED"), "{text}");
    assert!(!text.contains("ended early"), "{text}");
    assert!(!text.contains("NOT ON TAPE"), "{text}");
    assert!(!text.contains("WARNING"), "{text}");

    let (code, text) = h.run(&["--verify"]);
    assert_eq!(code, 0, "{text}");
    assert!(
        text.contains("VERIFY: PASS — every file matches the front index."),
        "{text}"
    );
    let passes = text.lines().filter(|l| l.starts_with("PASS  file")).count();
    assert_eq!(passes, t.total_files as usize - 1, "{text}");

    let photos = unit_of(t, "photos/2019");
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
    assert_eq!(h.count("mt eod"), 0, "{:#?}", h.ops());
    assert_eq!(h.count("mt rewind"), 1, "{:#?}", h.ops());
    assert!(!text.contains("ended early"), "{text}");

    let (code, text) = h.run(&["--where", "docs", "--key", &h.key("alice")]);
    assert_eq!(code, 0, "{text}");
    assert!(
        text.contains(&format!("tape {LABEL}  (this tape)")),
        "{text}"
    );
    assert_eq!(h.count("mt eod"), 0, "{:#?}", h.ops());
    assert_eq!(h.count("mt rewind"), 1, "{:#?}", h.ops());
}

/// RESTORE.sh as tapectl 1.5.0 wrote it, byte for byte (the fixture's README
/// says how it was made; the hash is the pin `tests/on_tape_golden.rs` held
/// from 1.3.0 to 1.5.0).
const RESTORE_SH_1_5_0: &str = include_str!("fixtures/restore_sh_1_5_0/RESTORE-GOLD01-12.sh");
const RESTORE_SH_1_5_0_SHA256: &str =
    "95862b49b0621e5970ebd91c49a353231725009e1f527c691cf98b3a116a6315";

/// ADR-0012 item 111: every 1.6.0 tape, full or short, fences out older
/// readers. A 1.5.0 RESTORE.sh refuses it at File 0, by name, in every mode,
/// before it reads anything else of it, and points to the RESTORE.sh on the
/// tape itself. The positive control: the same script reads a 1.5.0 tape.
#[test]
fn the_1_5_0_script_refuses_a_1_6_0_tape_full_or_short_at_file_0_by_name() {
    assert_eq!(
        sha256_hex(RESTORE_SH_1_5_0.as_bytes()),
        RESTORE_SH_1_5_0_SHA256,
        "the fixture is v1.5.0's RESTORE.sh"
    );
    for short in [false, true] {
        let h = Heir::new();
        if short {
            h.cut(unit("ledgers").positions[0]);
        }
        std::fs::write(h.dir.join("RESTORE.sh"), RESTORE_SH_1_5_0).unwrap();
        let alice = h.key("alice");
        let dest = h.sub("restored");
        for args in [
            vec!["--info"],
            vec!["--verify"],
            vec!["--find-envelope", "--key", &alice],
            vec![
                "--restore",
                "--key",
                &alice,
                "--unit",
                "docs",
                "--to",
                &dest,
            ],
            vec!["--where", "docs", "--key", &alice],
        ] {
            let (code, text) = h.run(&args);
            assert_eq!(code, 1, "short {short}, {args:?}: {text}");
            for needle in [
                "this tape requires feature(s) short-seal, which this script does not know",
                "This RESTORE.sh (layout v2, tapectl 1.1.0) cannot read it safely",
                "fsf 2",
            ] {
                assert!(
                    text.contains(needle),
                    "short {short}, {args:?}: {needle:?}:\n{text}"
                );
            }
            assert_eq!(
                reads(&h),
                vec![0],
                "short {short}, {args:?}: File 0 alone is read:\n{:#?}",
                h.ops()
            );
        }
    }
    let h = Heir::of(template_150(), &[]);
    std::fs::write(h.dir.join("RESTORE.sh"), RESTORE_SH_1_5_0).unwrap();
    let (code, text) = h.run(&["--info"]);
    assert_eq!(code, 0, "{text}");
    assert!(text.contains("Verdict: SEALED"), "{text}");
}

/// A 1.5.0 RESTORE.sh given a 1.6.0 tape as its second cartridge
/// (`--swap-tapes`) refuses it by the same rule, at that tape's File 0 (§6
/// "Since 1.6.0").
#[test]
fn the_1_5_0_script_refuses_a_1_6_0_tape_as_its_second_cartridge() {
    let t = template_150();
    let photos = unit_of(t, "photos/2019");
    let h = Heir::of(t, &[]);
    std::fs::write(h.dir.join("RESTORE.sh"), RESTORE_SH_1_5_0).unwrap();
    flip_in_tape(&h, photos.positions[1]);
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
    assert_eq!(code, 1, "{text}");
    let swap = h.ops().iter().position(|l| l == "swap").expect("a swap");
    assert_eq!(
        reads_in(&h.ops()[swap..]),
        vec![0],
        "only the second tape's File 0:\n{:#?}",
        h.ops()
    );
    assert!(
        text.contains("this tape requires feature(s) short-seal, which this script does not know"),
        "{text}"
    );
}

/// §4.1's parse rules: a seal that breaks S1 (`ended_after` is not
/// `file_count - 2`), S2 (it requires short-seal and states no `ended_after`)
/// or S5 (it ends inside the metadata) is no seal, and the tape reads as
/// UNSEALED, the broken rule named (S6).
#[test]
fn a_short_seal_that_breaks_a_parse_rule_is_no_seal() {
    for rule in ["rule S1", "rule S2"] {
        let (h, k) = short_heir();
        let from = format!("\nended_after = {k}\n");
        let to = if rule == "rule S1" {
            format!("\nended_after = {}\n", k - 1)
        } else {
            "\n".to_string()
        };
        edit_text_file(&h, k + 1, |t| t.replace(&from, &to));
        let (code, text) = h.run(&["--info"]);
        assert_eq!(code, 0, "{rule}: {text}");
        assert!(text.contains("Verdict: UNSEALED"), "{rule}: {text}");
        assert!(text.contains(rule), "{rule} is named:\n{text}");
    }

    // S5: a seal that says the tape ended after an envelope.
    let h = Heir::new();
    let tape = h.dir.join("tape");
    let (mut plan, fi) = plan_on_tape(&tape);
    let k = 4i64;
    assert_eq!(plan[k as usize].type_label, "tenant_envelope");
    let fi_sha = sha256_hex(&fi);
    plan[3].size_bytes = Some(fi.len() as u64);
    plan[3].sha256_encrypted = Some(fi_sha.clone());
    let mut copy = plan[..=k as usize].to_vec();
    copy.push(FrontIndexFile {
        position: (k + 1) as i32,
        type_label: "seal_marker",
        size_bytes: None,
        sha256_encrypted: None,
    });
    let seal = era_seal(
        Era::V160,
        &generate_seal_marker(LABEL, (k + 2) as i32, &fi_sha, &copy),
    );
    std::fs::write(tape.join((k + 1).to_string()), padded(seal.into_bytes())).unwrap();
    for p in k + 2..=planned_seal() {
        std::fs::remove_file(tape.join(p.to_string())).unwrap();
    }
    let (code, text) = h.run(&["--info"]);
    assert_eq!(code, 0, "{text}");
    assert!(text.contains("Verdict: UNSEALED"), "{text}");
    assert!(text.contains("rule S5"), "{text}");
}

/// What rule 6 accepts before the planned position is a seal that requires
/// short-seal, on a tape that does. Anything else found there is read the
/// degraded way RESTORE.sh has read a misplaced seal since 1.1.0: used, with
/// the warning that it is not where File 0 points.
#[test]
fn a_seal_before_the_planned_one_is_the_short_ending_only_on_a_short_seal_tape() {
    // A 1.6.0 tape whose early seal does not require short-seal.
    let (h, k) = short_heir();
    edit_text_file(&h, k + 1, |t| {
        with_requires(t, "requires = []").replace(&format!("\nended_after = {k}\n"), "\n")
    });
    let (code, text) = h.run(&["--info"]);
    assert_eq!(code, 0, "{text}");
    assert!(
        text.contains(&format!(
            "WARNING: File 0 says the seal marker is file {}, but the last file on the tape is",
            planned_seal()
        )),
        "{text}"
    );

    // A 1.5.0 tape (its File 0 requires nothing) that ends in a short seal.
    let h = Heir::of(template_150(), &[]);
    let k = unit_of(template_150(), "ledgers").positions[0];
    h.cut(k);
    let (code, text) = h.run(&["--info"]);
    assert_eq!(code, 0, "{text}");
    assert!(
        text.contains("WARNING: File 0 says the seal marker is file"),
        "{text}"
    );
    assert!(!text.contains("ended early"), "{text}");
}

/// R2: a 1.6.0 tape with no readable seal (a write that never sealed, ending in
/// a slice) reads as unsealed: the script says so, a unit whose slices are all
/// on the tape still restores, and `--verify` reads every planned file and
/// fails those past the end of the data.
#[test]
fn a_1_6_0_tape_with_no_seal_reads_as_unsealed() {
    let (h, k) = short_heir();
    std::fs::remove_file(h.dir.join("tape").join((k + 1).to_string())).unwrap();
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
    assert!(
        text.contains(
            "this tape requires short-seal, and no valid seal marker was found as its last file"
        ),
        "{text}"
    );
    assert!(same_tree(&docs.src, Path::new(&dest)), "{text}");

    let (code, text) = h.run(&["--verify"]);
    assert_eq!(code, 1, "{text}");
    assert!(text.contains("this tape is UNSEALED"), "{text}");
    assert!(!text.contains("NOT ON TAPE"), "{text}");
}

/// The short seal as tapectl's own generator writes it
/// (`generate_short_seal_marker`), its bytes untouched: RESTORE.sh finds it
/// as the last file, accepts it as the normal short ending, and reads the
/// tape by it, as it reads the spec's grammar in the tests above.
#[test]
fn the_generators_own_short_seal_is_read_by_restore_sh() {
    let h = Heir::new();
    let k = unit("ledgers").positions[0];
    let m = planned_seal();
    let tape = h.dir.join("tape");
    let (mut plan, fi) = plan_on_tape(&tape);
    let fi_sha = sha256_hex(&fi);
    plan[3].size_bytes = Some(fi.len() as u64);
    plan[3].sha256_encrypted = Some(fi_sha.clone());
    let seal = generate_short_seal_marker(LABEL, k as i32, &fi_sha, &plan[..=k as usize]);
    assert!(
        seal.contains(&format!(
            "\nrequires = [\"short-seal\"]\nfile_count = {}\nended_after = {k}\n",
            k + 2
        )),
        "{seal}"
    );
    std::fs::write(tape.join((k + 1).to_string()), padded(seal.into_bytes())).unwrap();
    for p in k + 2..=m {
        std::fs::remove_file(tape.join(p.to_string())).unwrap();
    }

    let (code, text) = h.run(&["--info"]);
    assert_eq!(code, 0, "{text}");
    assert!(text.contains("Verdict: SEALED"), "{text}");
    assert!(
        text.contains(&format!("The tape ended early, after file {k}:")),
        "{text}"
    );
    assert!(!text.contains("WARNING"), "{text}");

    let (code, text) = h.run(&["--verify"]);
    assert_eq!(code, 0, "{text}");
    let gone: Vec<&str> = text
        .lines()
        .filter(|l| l.starts_with("NOT ON TAPE"))
        .collect();
    assert_eq!(gone.len(), 1, "one line (item 140):\n{text}");
    assert!(
        gone[0].starts_with(&format!("NOT ON TAPE  files {} to {m}  ", k + 1)),
        "{text}"
    );

    let (code, text) = h.run(&[
        "--restore",
        "--key",
        &h.key("bob"),
        "--unit",
        "ledgers",
        "--to",
        &h.sub("cut"),
    ]);
    assert_eq!(code, 1, "{text}");
    assert!(
        text.contains("unit 'ledgers' (version 3) is not on this tape whole"),
        "{text}"
    );
}

/// `--find-envelope` reads no seal on any tape: every envelope lies before the
/// first data slice (§4.2, "unchanged"). One rewind, no wind to the end.
#[test]
fn find_envelope_on_a_short_tape_reads_no_seal() {
    let (h, _k) = short_heir();
    let (code, text) = h.run(&["--find-envelope", "--key", &h.key("bob")]);
    assert_eq!(code, 0, "{text}");
    assert!(text.contains("ledgers"), "{text}");
    assert_eq!(h.count("mt eod"), 0, "{:#?}", h.ops());
    assert_eq!(h.count("mt rewind"), 1, "{:#?}", h.ops());
}

// ---- a unit across tapes, read on one tape (tapectl 1.7.0; volume-format-v2.md
// §1.2, §1.5, §4.4; ADR-0012 items 124-126, 135, 140, 143; #478) ----

const BIG_UUID: &str = "88888888-9999-aaaa-bbbb-cccccccccccc";

/// alice's `big`, a unit allowed to run across tapes: five or more slices.
fn big_spec() -> UnitSpec {
    UnitSpec {
        cat: Cat::Matched,
        name: "big",
        uuid: BIG_UUID,
        version: 2,
        seed: 8,
        files: 4,
        file_len: 120_000,
        names: &[],
        corrupt: &[],
        tamper: &[],
    }
}

fn small_spec(name: &'static str, uuid: &'static str, version: i64, seed: u64) -> UnitSpec {
    UnitSpec {
        cat: Cat::Matched,
        name,
        uuid,
        version,
        seed,
        files: 2,
        file_len: 130_000,
        names: &[],
        corrupt: &[],
        tamper: &[],
    }
}

const DOCS_UUID: &str = "22222222-3333-4444-5555-666666666666";
const LEDGERS_UUID: &str = "33333333-4444-5555-6666-777777777777";

/// The third tape of item 93's layout: `big`'s owed run first, slices 3 to N
/// (slices 1 and 2 are on OTHER02, which its index names), then alice's `docs`
/// and bob's `ledgers` whole. A full seal: the tape holds a range by its plan.
fn span_tail_template() -> &'static Template {
    static T: OnceLock<Template> = OnceLock::new();
    T.get_or_init(|| {
        let tenants = vec![
            TenantSpec {
                name: "alice",
                units: vec![big_spec(), small_spec("docs", DOCS_UUID, 1, 12)],
            },
            TenantSpec {
                name: "bob",
                units: vec![small_spec("ledgers", LEDGERS_UUID, 3, 13)],
            },
        ];
        build_template_spans(
            "spantail",
            &tenants,
            true,
            true,
            &[],
            Era::V170Span,
            &[SpanOnTape {
                unit: "big",
                held: 2..usize::MAX,
            }],
        )
    })
}

/// The first tape of item 93's layout, before its end: bob's `ledgers` and
/// alice's `docs` whole, then `big`, planned whole to its last slice (the
/// spannable set goes last). Uncut it is item 93's middle tape, holding `big`
/// whole; `cut` inside `big` makes it the first tape, holding a range 1..K.
fn span_head_template() -> &'static Template {
    static T: OnceLock<Template> = OnceLock::new();
    T.get_or_init(|| {
        let tenants = vec![
            TenantSpec {
                name: "bob",
                units: vec![small_spec("ledgers", LEDGERS_UUID, 3, 13)],
            },
            TenantSpec {
                name: "alice",
                units: vec![small_spec("docs", DOCS_UUID, 1, 12), big_spec()],
            },
        ];
        build_template_spans(
            "spanhead",
            &tenants,
            true,
            true,
            &[],
            Era::V170Span,
            &[SpanOnTape {
                unit: "big",
                held: 0..usize::MAX,
            }],
        )
    })
}

fn span_unit(t: &Template, name: &str) -> UnitOnTape {
    unit_of(t, name)
}

/// The OTHER02 row and this tape's row `--where big` prints on the tail tape.
fn tail_rows(n: usize) -> (String, String) {
    (
        format!("    tape {LABEL}  (this tape, a range: not the whole version)  slices 3-{n} of {n}"),
        format!(
            "    tape OTHER02  (sealed 2026-09-01T10:00:00Z, a range: not the whole version)  slices 1-2 of {n}"
        ),
    )
}

/// ADR-0012 item 124 on a tape built from the spec: a span tape's File 0 and
/// seal state `requires = ["short-seal", "spanning"]`, and its envelope's
/// block of the unit that may span states `may_span = true`, lists every slice
/// with its hashes, and gives a `tape_position` only to the run this tape
/// holds. `--find-envelope` shows it to the tenant (it prints the manifest).
#[test]
fn a_span_tape_states_spanning_and_lists_every_slice_of_the_span_unit() {
    let t = span_tail_template();
    let big = span_unit(t, "big");
    assert!(
        big.slices >= 5,
        "{big:?}: want a unit of five slices or more"
    );
    assert_eq!(big.positions.len(), big.slices - 2, "slices 3 to N here");
    let last = t.total_files - 1;
    let f0 = std::fs::read_to_string(t.dir.join("tape/0")).unwrap();
    let seal = std::fs::read_to_string(t.dir.join(format!("tape/{last}"))).unwrap();
    assert!(
        f0.contains(&format!("\nlayout_version = 2\n{SPAN_REQUIRES}\n")),
        "File 0"
    );
    assert!(seal.contains(&format!("\n{SPAN_REQUIRES}\n")), "the seal");

    let h = Heir::of(t, &[]);
    let (code, text) = h.run(&["--find-envelope", "--key", &h.key("alice")]);
    assert_eq!(code, 0, "{text}");
    let block = &text[text.find("name = \"big\"").expect("big's block")..];
    let block = &block[..block.find("\n[[units]]").unwrap_or(block.len())];
    assert!(block.contains("\nmay_span = true\n"), "{block}");
    assert_eq!(
        block.matches("\n[[units.slices]]\n").count(),
        big.slices,
        "every slice is listed:\n{block}"
    );
    assert_eq!(
        block.matches("\ntape_position = ").count(),
        big.slices - 2,
        "a position only on the run here:\n{block}"
    );
    assert!(
        block.contains("[[units.slices]]\nnumber = 1\nsize_bytes = "),
        "slice 1 has no tape_position:\n{block}"
    );
}

/// §4.4: `--info` and `--verify` read without a key and see a range only as
/// data slices: the verdict is SEALED, every file on the tape passes, and the
/// slices on other tapes are not in the front index, so nothing names them
/// missing or damaged. Unchanged but for item 140's line, which a full span
/// tape has no use for.
#[test]
fn info_and_verify_see_a_span_tapes_range_as_data_slices() {
    let t = span_tail_template();
    let h = Heir::of(t, &[]);
    let (code, text) = h.run(&["--info"]);
    assert_eq!(code, 0, "{text}");
    assert!(text.contains("Verdict: SEALED"), "{text}");
    for word in ["NOT ON TAPE", "range", "across", "big", "WARNING"] {
        assert!(!text.contains(word), "{word:?}:\n{text}");
    }
    let (code, text) = h.run(&["--verify"]);
    assert_eq!(code, 0, "{text}");
    assert!(
        text.contains("VERIFY: PASS — every file matches the front index."),
        "{text}"
    );
    let passes = text.lines().filter(|l| l.starts_with("PASS  file")).count();
    assert_eq!(passes, t.total_files as usize - 1, "{text}");
    assert!(
        !text.contains("FAIL") && !text.contains("NOT ON TAPE"),
        "{text}"
    );
}

/// `--list` lists a unit held across tapes from its catalogue, as any unit,
/// and says which of its slices this tape holds (§4.4), reading no slice; a
/// whole unit's listing says nothing of the kind.
#[test]
fn list_names_the_run_of_a_unit_held_across_tapes() {
    let t = span_tail_template();
    let big = span_unit(t, "big");
    let n = big.slices;
    let h = Heir::of(t, &[]);
    let (code, text) = h.run(&["--list", "--key", &h.key("alice"), "--unit", "big"]);
    assert_eq!(code, 0, "{text}");
    assert!(text.contains("=== big, snapshot version 2"), "{text}");
    let flat_text = flat(&text);
    for needle in [
        format!("HELD ACROSS TAPES: this tape holds slices 3 to {n} of this version's {n}"),
        format!(
            "(tape files {} to {}); the others are on other tapes.",
            big.positions[0],
            big.positions.last().unwrap()
        ),
        "--where 'big' --key ".to_string(),
    ] {
        assert!(flat_text.contains(&needle), "{needle:?}:\n{text}");
    }
    assert!(text.contains("file0.bin"), "its files are listed:\n{text}");
    let first_slice = big.positions[0];
    assert!(
        walk(&h).0.iter().all(|p| *p < first_slice),
        "no slice is read:\n{:#?}",
        h.ops()
    );

    let (code, text) = h.run(&["--list", "--key", &h.key("alice"), "--unit", "docs"]);
    assert_eq!(code, 0, "{text}");
    assert!(!text.contains("HELD ACROSS TAPES"), "{text}");
}

/// R7 and "Where a restore starts": a whole-unit restore of a unit this tape
/// holds a range of that does not start at slice 1 stops before any slice is
/// read and before dar runs, naming the run here, every slice it needs, and
/// the holders of slice 1 the index names with their runs (OTHER02, not this
/// tape). (`--path` has no such start rule: it reads across tapes, from any of
/// them, and the `--path` tests below prove it.) Nothing is restored. A slice with no tape position is never read, and
/// never as file 0: the run reads File 0 once, at the start, and nothing at
/// or after the first slice.
#[test]
fn a_restore_of_a_range_stops_before_dar_and_names_the_tapes_it_needs() {
    let t = span_tail_template();
    let big = span_unit(t, "big");
    let n = big.slices;
    let first_slice = *t
        .units
        .iter()
        .flat_map(|u| u.positions.iter())
        .min()
        .unwrap();
    let h = Heir::of(t, &[("dar", DAR_SPY)]);
    let alice = h.key("alice");
    let dest = h.sub("restored");
    let args = ["--restore", "--key", &alice, "--unit", "big", "--to", &dest];
    let (code, text) = h.run(&args);
    assert_eq!(code, 1, "{text}");
    let (here, other) = tail_rows(n);
    for needle in [
        "unit 'big' (version 2) is held across tapes, and this tape holds only part of it:"
            .to_string(),
        format!(
            "slices 3 to {n} of its {n}, at tape files {} to {}.",
            big.positions[0],
            big.positions.last().unwrap()
        ),
        format!("every slice, 1 to {n}, in slice order, never from one tape's slices alone."),
        "--where 'big' --key ".to_string(),
        "No slice has been read.".to_string(),
        "and the run on this tape starts at slice 3.".to_string(),
        "names these tapes as holding slice 1".to_string(),
        other,
    ] {
        assert!(text.contains(&needle), "{needle:?}:\n{text}");
    }
    assert!(
        !text.contains(&here),
        "only the holders of slice 1:\n{text}"
    );
    assert_eq!(
        reads(&h).iter().filter(|p| **p == 0).count(),
        1,
        "File 0 once, at the start:\n{:#?}",
        h.ops()
    );
    // Past the wind to the seal (R1), nothing at or after the first slice.
    assert!(
        walk(&h).0.iter().all(|p| *p < first_slice),
        "no slice is read:\n{:#?}",
        h.ops()
    );
    assert!(
        !h.dir.join("dar.args").exists()
            || !std::fs::read_to_string(h.dir.join("dar.args"))
                .unwrap()
                .contains("-x"),
        "dar never extracts"
    );
    assert_eq!(
        std::fs::read_dir(&dest).map(|d| d.count()).unwrap_or(0),
        0,
        "nothing restored"
    );
}

/// §4.4: `--restore --all` restores every unit the tape holds whole and names
/// each unit it holds only a range of, with the `--restore --unit` command
/// that restores it across tapes, before RESTORE COMPLETE. Nothing of the
/// range is read.
#[test]
fn all_on_a_span_tape_restores_the_whole_units_and_names_the_range() {
    let t = span_tail_template();
    let big = span_unit(t, "big");
    let n = big.slices;
    let h = Heir::of(t, &[]);
    let dest = h.dir.join("restored");
    let (code, text) = h.run(&[
        "--restore",
        "--key",
        &h.key("esc"),
        "--all",
        "--to",
        dest.to_str().unwrap(),
    ]);
    assert_eq!(code, 0, "{text}");
    for u in ["docs", "ledgers"] {
        assert!(same_tree(&unit_of(t, u).src, &dest.join(u)), "{u}:\n{text}");
    }
    assert!(!dest.join("big").exists(), "big is not restored");
    let note = text
        .find("HELD ACROSS TAPES: this tape holds only some of the slices of these unit(s)")
        .unwrap_or_else(|| panic!("the range is named:\n{text}"));
    let complete = text.find("RESTORE COMPLETE").expect("RESTORE COMPLETE");
    assert!(note < complete, "named before RESTORE COMPLETE:\n{text}");
    let named = &text[note..complete];
    assert!(
        named.contains(&format!(
            "'big' (version 2): slices 3 to {n} of {n} are on this tape"
        )),
        "{named}"
    );
    assert!(
        named.contains("--unit 'big' --to /your/destination/big"),
        "{named}"
    );
    assert!(named.contains("--where UNIT --key "), "{named}");
    let (walked, rewinds) = walk(&h);
    assert_eq!(rewinds, 1, "{:#?}", h.ops());
    assert!(ascending(&walked), "{walked:?}");
    assert!(
        walked.iter().all(|p| !big.positions.contains(p)),
        "no slice of the range is read: {walked:?}"
    );
}

/// §1.5 and §4.4: `--where` and `--find` print a holder's slice run when it
/// is not the whole version, mark it a range, and a holder of a range does
/// not count on its own as holding the version: `--where --all` does not count
/// this tape for `big`, and names both holders of it with their runs. A unit
/// this tape holds whole reads as before. The positive control: no tape that
/// holds every version whole says "a range".
#[test]
fn where_and_find_print_each_holders_run_and_a_range_is_no_copy() {
    let t = span_tail_template();
    let n = span_unit(t, "big").slices;
    let h = Heir::of(t, &[]);
    let (code, text) = h.run(&["--where", "big", "--key", &h.key("alice")]);
    assert_eq!(code, 0, "{text}");
    let (here, other) = tail_rows(n);
    assert!(text.contains(&here), "{text}");
    assert!(text.contains(&other), "{text}");
    assert!(
        text.contains("A holder marked 'a range' holds only the slices it names of that version"),
        "{text}"
    );
    assert!(!text.contains("CUT on this tape"), "{text}");

    let (code, text) = h.run(&["--where", "docs", "--key", &h.key("alice")]);
    assert_eq!(code, 0, "{text}");
    assert!(
        text.contains(&format!("tape {LABEL}  (this tape)  slices 1-")),
        "{text}"
    );
    assert!(!text.contains("a range"), "{text}");

    let (code, text) = h.run(&["--where", "--all", "--key", &h.key("esc")]);
    assert_eq!(code, 0, "{text}");
    assert!(
        text.contains("3 unit(s) in this index; this tape holds the newest version of 2 of them."),
        "{text}"
    );
    assert!(
        text.contains("Unit big (tenant alice): newest version 2, NOT on this tape"),
        "{text}"
    );
    assert!(
        text.contains(&format!(
            "{LABEL} (this tape: slices 3-{n} only), OTHER02 (slices 1-2 only)"
        )),
        "the absent list names each holder's run:\n{text}"
    );

    let (code, text) = h.run(&["--find", "file0", "--key", &h.key("alice")]);
    assert_eq!(code, 0, "{text}");
    let holders = block(&text, "Tapes holding big version 2:");
    assert!(
        holders.iter().any(|l| l.contains(&here)),
        "{holders:#?}\n{text}"
    );

    for h in [Heir::new(), Heir::of(template_150(), &[])] {
        let (code, text) = h.run(&["--where", "--all", "--key", &h.key("esc")]);
        assert_eq!(code, 0, "{text}");
        assert!(
            !text.contains("a range") && !text.contains("only)"),
            "{text}"
        );
    }
}

/// Item 93's middle tape: a unit that may span, planned and held whole on one
/// tape, is a whole unit there (§4.4 "All N slices"): it restores from this
/// tape alone, and no mode calls it a range.
#[test]
fn a_span_unit_held_whole_restores_from_its_one_tape() {
    let t = span_head_template();
    let big = span_unit(t, "big");
    assert_eq!(big.positions.len(), big.slices, "the whole set is here");
    let h = Heir::of(t, &[]);
    let dest = h.sub("restored");
    let (code, text) = h.run(&[
        "--restore",
        "--key",
        &h.key("alice"),
        "--unit",
        "big",
        "--to",
        &dest,
    ]);
    assert_eq!(code, 0, "{text}");
    assert!(text.contains("RESTORE COMPLETE"), "{text}");
    assert!(same_tree(&big.src, Path::new(&dest)), "{text}");
    let (code, text) = h.run(&["--list", "--key", &h.key("alice"), "--unit", "big"]);
    assert_eq!(code, 0, "{text}");
    assert!(!text.contains("HELD ACROSS TAPES"), "{text}");
    let (code, text) = h.run(&["--where", "big", "--key", &h.key("alice")]);
    assert_eq!(code, 0, "{text}");
    assert!(
        text.contains(&format!("tape {LABEL}  (this tape)  slices 1-")),
        "{text}"
    );
    assert!(!text.contains("a range"), "{text}");
}

/// The first tape of item 93: the early warning came inside `big`, which was
/// planned to its last slice, so the tape holds the range 1..K and the rest of
/// the set is cut off this tape by design, not damage. The tape's index row
/// says the plan's run (1..N); every mode reads the seal and names the run
/// that is really here: `--where` prints this tape's row as a range of 1-K,
/// never CUT, `--list` and `--restore` name it, and `--info` and `--verify`
/// print item 140's one NOT ON TAPE line.
#[test]
fn a_range_the_early_warning_ended_is_named_by_its_run_and_not_as_cut() {
    let t = span_head_template();
    let big = span_unit(t, "big");
    let n = big.slices;
    let h = Heir::of(t, &[]);
    // The warning in big's third slice.
    let k = big.positions[2];
    h.cut(k);
    let m = t.total_files as i64 - 1;

    let (code, text) = h.run(&["--verify"]);
    assert_eq!(code, 0, "{text}");
    let gone: Vec<&str> = text
        .lines()
        .filter(|l| l.starts_with("NOT ON TAPE"))
        .collect();
    assert_eq!(
        gone,
        vec![format!(
            "NOT ON TAPE  files {} to {m}  (planned; the tape ended after file {k}, and its seal marker is file {})",
            k + 1,
            k + 1
        )],
        "{text}"
    );
    let (code, text) = h.run(&["--info"]);
    assert_eq!(code, 0, "{text}");
    assert_eq!(
        text.lines()
            .filter(|l| l.trim_start().starts_with("NOT ON TAPE"))
            .count(),
        1,
        "{text}"
    );

    let (code, text) = h.run(&["--where", "big", "--key", &h.key("alice")]);
    assert_eq!(code, 0, "{text}");
    assert!(
        text.contains(&format!(
            "    tape {LABEL}  (this tape, a range: not the whole version)  slices 1-3 of {n}"
        )),
        "the run the seal leaves, not the plan's 1-{n}:\n{text}"
    );
    assert!(!text.contains("CUT on this tape"), "{text}");

    let (code, text) = h.run(&["--list", "--key", &h.key("alice"), "--unit", "big"]);
    assert_eq!(code, 0, "{text}");
    assert!(
        flat(&text).contains(&format!(
            "HELD ACROSS TAPES: this tape holds slices 1 to 3 of this version's {n} (tape files {} to {k});",
            big.positions[0]
        )),
        "{text}"
    );
    assert!(!text.contains("NOT ON THIS TAPE WHOLE"), "{text}");

    let dest = h.sub("restored");
    let (code, text) = h.run(&[
        "--restore",
        "--key",
        &h.key("alice"),
        "--unit",
        "big",
        "--to",
        &dest,
    ]);
    // This tape holds slice 1: the restore reads its run, 1 to 3, into
    // scratch and asks for slice 4; the input ends, so it stops, restoring
    // nothing (the brief's quit rule).
    assert_eq!(code, 1, "{text}");
    assert!(
        text.contains(&format!(
            "'big' (version 2) is held across tapes: this tape holds slices 1 to 3 of {n}."
        )),
        "{text}"
    );
    assert!(
        text.contains(&format!("needs slice 4 of {n} next")),
        "{text}"
    );
    assert!(
        flat(&text).contains("stopped before slice 4 of 'big' (version 2)"),
        "{text}"
    );
    assert!(flat(&text).contains("nothing of it was restored"), "{text}");
    assert_eq!(std::fs::read_dir(&dest).unwrap().count(), 0, "{text}");

    // docs, wholly before the end, still restores whole.
    let dest = h.sub("restored-docs");
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
    assert!(
        same_tree(&unit_of(t, "docs").src, Path::new(&dest)),
        "{text}"
    );
}

/// Cut detection with a range present (§4.2, §4.4): on the tail tape ended
/// early inside `docs`, `big`'s run lies wholly before the end and is still a
/// range, never CUT, while `docs` and `ledgers` are cut. On the head tape
/// ended before `big`'s first slice, `big` holds no slice here and is cut. With
/// nothing whole on the tape, `--all` names both kinds and restores nothing.
#[test]
fn cut_detection_keeps_a_range_a_range_and_cuts_a_span_set_with_no_slice_here() {
    let t = span_tail_template();
    let n = span_unit(t, "big").slices;
    let h = Heir::of(t, &[]);
    let docs = unit_of(t, "docs");
    h.cut(docs.positions[0]);
    let (code, text) = h.run(&["--where", "big", "--key", &h.key("alice")]);
    assert_eq!(code, 0, "{text}");
    assert!(text.contains(&tail_rows(n).0), "{text}");
    assert!(
        !text.contains("(CUT on this tape: it ended early"),
        "{text}"
    );
    let (code, text) = h.run(&["--where", "docs", "--key", &h.key("alice")]);
    assert_eq!(code, 0, "{text}");
    assert!(text.contains("(CUT on this tape: it ended early"), "{text}");

    let dest = h.dir.join("restored");
    let (code, text) = h.run(&[
        "--restore",
        "--key",
        &h.key("esc"),
        "--all",
        "--to",
        dest.to_str().unwrap(),
    ]);
    assert_eq!(code, 1, "{text}");
    for needle in [
        "NOT ON THIS TAPE WHOLE",
        "'docs' (version 1)",
        "'ledgers' (version 3)",
        "HELD ACROSS TAPES",
        "'big' (version 2): slices 3 to",
        "each is cut by the end of the\n       tape or held across tapes (see above). Nothing was restored.",
    ] {
        assert!(text.contains(needle), "{needle:?}:\n{text}");
    }

    let t = span_head_template();
    let h = Heir::of(t, &[]);
    h.cut(unit_of(t, "docs").positions[0]);
    let (code, text) = h.run(&["--where", "big", "--key", &h.key("alice")]);
    assert_eq!(code, 0, "{text}");
    assert!(text.contains("(CUT on this tape: it ended early"), "{text}");
    assert!(!text.contains("a range"), "{text}");
    let (code, text) = h.run(&[
        "--restore",
        "--key",
        &h.key("alice"),
        "--unit",
        "big",
        "--to",
        &h.sub("cut-big"),
    ]);
    assert_eq!(code, 1, "{text}");
    assert!(
        text.contains("unit 'big' (version 2) has no slice on this tape: the tape ended early,"),
        "{text}"
    );
    assert!(
        flat(&text).contains(
            "It may run across tapes, and is restorable only from the tapes that hold its slices, starting from one that holds slice 1."
        ),
        "{text}"
    );
}

/// ADR-0012 item 143: a fill never takes files from a range; the refusal says
/// why, and the list is left as it was.
#[test]
fn a_fill_from_a_tape_holding_a_range_is_refused_and_the_list_kept() {
    let t = span_tail_template();
    let h = Heir::of(t, &[]);
    let dir = h.dir.join("restored");
    std::fs::create_dir_all(&dir).unwrap();
    let list = dir.join(".tapectl-damaged-files.txt");
    let body =
        format!("#tapectl-damaged-files\t1\tunit=big\tuuid={BIG_UUID}\tversion=2\nfile0.bin\n");
    std::fs::write(&list, &body).unwrap();
    let (code, text) = h.run(&[
        "--restore",
        "--key",
        &h.key("alice"),
        "--unit",
        "big",
        "--fill-from",
        list.to_str().unwrap(),
        "--to",
        dir.to_str().unwrap(),
    ]);
    assert_eq!(code, 1, "{text}");
    assert!(
        text.contains("unit 'big' (version 2) is held across tapes"),
        "{text}"
    );
    assert!(
        flat(&text).contains(
            "A fill takes files only from a tape that holds the version whole, and a range is never a fill source. Nothing was changed."
        ),
        "{text}"
    );
    assert_eq!(std::fs::read_to_string(&list).unwrap(), body);
}

/// RESTORE.sh as tapectl 1.6.0 wrote it, byte for byte (the fixture's README
/// says how it was made; the hash is the pin `tests/on_tape_golden.rs` held in
/// 1.6.0).
const RESTORE_SH_1_6_0: &str = include_str!("fixtures/restore_sh_1_6_0/RESTORE-GOLD01-12.sh");
const RESTORE_SH_1_6_0_SHA256: &str =
    "d9787a4ffdd05ba038416696d9866a9a285fc97509c35692a18dc3a9b68f9e9d";

/// ADR-0012 item 124: a 1.6.0 RESTORE.sh refuses a span tape at File 0, by
/// name, in every mode, before it reads anything else of it, full or short,
/// and points to the RESTORE.sh on the tape itself. The positive control: it
/// still reads a tape 1.7.0 writes that does not span (it requires
/// short-seal alone), and restores from it.
#[test]
fn the_1_6_0_script_refuses_a_span_tape_at_file_0_by_name_and_reads_one_that_does_not_span() {
    assert_eq!(
        sha256_hex(RESTORE_SH_1_6_0.as_bytes()),
        RESTORE_SH_1_6_0_SHA256,
        "the fixture is v1.6.0's RESTORE.sh"
    );
    for short in [false, true] {
        let t = span_tail_template();
        let h = Heir::of(t, &[]);
        if short {
            h.cut(unit_of(t, "docs").positions[0]);
        }
        std::fs::write(h.dir.join("RESTORE.sh"), RESTORE_SH_1_6_0).unwrap();
        let alice = h.key("alice");
        let dest = h.sub("restored");
        for args in [
            vec!["--info"],
            vec!["--verify"],
            vec!["--find-envelope", "--key", &alice],
            vec![
                "--restore",
                "--key",
                &alice,
                "--unit",
                "docs",
                "--to",
                &dest,
            ],
            vec!["--list", "--key", &alice],
            vec!["--where", "docs", "--key", &alice],
        ] {
            let (code, text) = h.run(&args);
            assert_eq!(code, 1, "short {short}, {args:?}: {text}");
            for needle in [
                "this tape requires feature(s) spanning, which this script does not know",
                "This RESTORE.sh (layout v2, tapectl 1.6.0) cannot read it safely",
                "fsf 2",
            ] {
                assert!(
                    text.contains(needle),
                    "short {short}, {args:?}: {needle:?}:\n{text}"
                );
            }
            assert_eq!(
                reads(&h),
                vec![0],
                "short {short}, {args:?}: File 0 alone is read:\n{:#?}",
                h.ops()
            );
        }
    }
    let h = Heir::new();
    std::fs::write(h.dir.join("RESTORE.sh"), RESTORE_SH_1_6_0).unwrap();
    let (code, text) = h.run(&["--info"]);
    assert_eq!(code, 0, "{text}");
    assert!(text.contains("Verdict: SEALED"), "{text}");
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
    assert!(same_tree(&unit("docs").src, Path::new(&dest)), "{text}");
}

/// A 1.6.0 RESTORE.sh given a span tape as its second cartridge
/// (`--swap-tapes`) refuses it by the same rule, at that tape's File 0, and
/// reads nothing else of it.
#[test]
fn the_1_6_0_script_refuses_a_span_tape_as_its_second_cartridge() {
    let t = template();
    let photos = unit("photos/2019");
    let h = Heir::new();
    std::fs::write(h.dir.join("RESTORE.sh"), RESTORE_SH_1_6_0).unwrap();
    flip_in_tape(&h, photos.positions[1]);
    let all: Vec<Vec<u8>> = (0..photos.positions.len())
        .map(|i| good_slice(t, &photos, i))
        .collect();
    second_tape(&h, "COPY02", "c0c0c0c0-0002-4000-8000-000000000002", &all);
    let f0 = h.dir.join("tape2/0");
    let text: String = String::from_utf8(
        std::fs::read(&f0)
            .unwrap()
            .into_iter()
            .filter(|b| *b != 0)
            .collect(),
    )
    .unwrap();
    std::fs::write(
        &f0,
        padded(with_requires(&text, SPAN_REQUIRES).into_bytes()),
    )
    .unwrap();
    let dest = h.sub("restored");
    let (code, text) = run_with(
        &h,
        &restore_args(&h, "photos/2019", &dest),
        &["--swap-tapes"],
        "\n",
        &[("FAKE_SWAP_ON_SETBLK", "2")],
    );
    assert_eq!(code, 1, "{text}");
    let swap = h.ops().iter().position(|l| l == "swap").expect("a swap");
    assert_eq!(
        reads_in(&h.ops()[swap..]),
        vec![0],
        "only the second tape's File 0:\n{:#?}",
        h.ops()
    );
    assert!(
        text.contains("this tape requires feature(s) spanning, which this script does not know"),
        "{text}"
    );
}

/// This script reads the tapes 1.6.0 and 1.5.0 wrote as before: none of the
/// range text appears on them, in any mode that names units.
#[test]
fn the_1_7_0_script_reads_1_6_0_and_1_5_0_tapes_as_before() {
    for t in [template(), template_150()] {
        let h = Heir::of(t, &[]);
        for args in [
            vec!["--info".to_string()],
            vec!["--verify".to_string()],
            vec!["--list".to_string(), "--key".to_string(), h.key("esc")],
            vec![
                "--where".to_string(),
                "--all".to_string(),
                "--key".to_string(),
                h.key("esc"),
            ],
        ] {
            let (code, text) = runv(&h, &args);
            assert_eq!(code, 0, "{args:?}: {text}");
            for word in ["HELD ACROSS TAPES", "a range", "spanning", "NOT ON TAPE"] {
                assert!(!text.contains(word), "{args:?}: {word:?}:\n{text}");
            }
        }
        let dest = h.dir.join("restored");
        let (code, text) = h.run(&[
            "--restore",
            "--key",
            &h.key("esc"),
            "--all",
            "--to",
            dest.to_str().unwrap(),
        ]);
        assert_eq!(code, 0, "{text}");
        assert!(text.contains("RESTORE COMPLETE"), "{text}");
        assert!(!text.contains("HELD ACROSS TAPES"), "{text}");
    }
}

// ---- a unit across tapes, restored across cartridges: RESTORE.sh's two modes
// (volume-format-v2.md §4.4 "RESTORE.sh", R6-R9, §4.2 R5; ADR-0012 items 88, 93,
// 94, 132, 139; #478) ----

/// One tape of a box laid out from one staging of alice's `big`.
struct BoxTape {
    id: TapeId,
    /// The run of `big`'s slices this tape's plan holds (0-based).
    held: std::ops::Range<usize>,
    /// The slice (0-based) in which the drive's early warning ended the tape:
    /// it is sealed short after that slice.
    cut_in: Option<usize>,
}

/// Every tape here is laid out from ONE staging of `big`, so each slice has
/// the same bytes, and the same sha256, on every tape that holds it: tapes
/// staged apart could never be joined by hash (a dar run has its own label).
/// Item 93's two copies over three tapes: SPAN01 (planned 1..N, the early
/// warning in slice 2: a range 1..2), SPAN02 (whole), SPAN03 (the owed run
/// 3..N). Item 94's span over three: SPAN01, SPAN22 (planned 3..N, cut in
/// slice N-1: a range 3..N-1), SPAN23 (N). SPAN11 is SPAN01 again under
/// another label and volume uuid; SPAN33 is SPAN03 with an index that names
/// no other holder. Each tape's index names the tapes written before it, with
/// their runs (a tape never names one written after it).
struct SpanBox {
    n: usize,
    tapes: BTreeMap<&'static str, Template>,
}

impl SpanBox {
    fn t(&self, label: &str) -> &Template {
        &self.tapes[label]
    }

    /// The tapes, by label, for `Heir::of_box`.
    fn of(&self, labels: &[&str]) -> Vec<&Template> {
        labels.iter().map(|l| self.t(l)).collect()
    }

    /// `big` as SPAN02 holds it: every slice, with its size and sha256.
    fn big(&self) -> UnitOnTape {
        unit_of(self.t("SPAN02"), "big")
    }
}

const SPAN_UUIDS: [(&str, &str); 7] = [
    ("SPAN01", "5a000000-0000-4000-8000-000000000001"),
    ("SPAN02", "5a000000-0000-4000-8000-000000000002"),
    ("SPAN03", "5a000000-0000-4000-8000-000000000003"),
    ("SPAN22", "5a000000-0000-4000-8000-000000000022"),
    ("SPAN23", "5a000000-0000-4000-8000-000000000023"),
    ("SPAN11", "5a000000-0000-4000-8000-000000000011"),
    ("SPAN33", "5a000000-0000-4000-8000-000000000033"),
];

fn span_uuid(label: &str) -> &'static str {
    SPAN_UUIDS.iter().find(|(l, _)| *l == label).unwrap().1
}

fn holder(label: &'static str, first: i64, last: i64) -> Holder {
    Holder {
        label,
        uuid: span_uuid(label),
        serial: "SPANSER",
        first,
        last,
    }
}

fn span_box() -> &'static SpanBox {
    static B: OnceLock<SpanBox> = OnceLock::new();
    B.get_or_init(|| {
        let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR"))
            .join(format!("heir-restore-sh-spanbox-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        for sub in ["src", "work"] {
            std::fs::create_dir_all(dir.join(sub)).unwrap();
        }
        let tenants = vec![TenantSpec {
            name: "alice",
            units: vec![big_spec()],
        }];
        let spans = [SpanOnTape {
            unit: "big",
            held: 0..usize::MAX,
        }];
        let staged = stage_units(&dir, &tenants, &spans);
        let n = staged[0].slices.len();
        assert!(n >= 5, "big has {n} slices: want five or more");
        let tape = |label: &'static str,
                    held: std::ops::Range<usize>,
                    cut_in: Option<usize>,
                    earlier: Vec<Holder>| BoxTape {
            id: TapeId {
                label,
                uuid: span_uuid(label),
                serial: "SPANSER",
                earlier: Some(earlier),
            },
            held,
            cut_in,
        };
        let n64 = n as i64;
        let specs = vec![
            tape("SPAN01", 0..n, Some(1), vec![]),
            tape("SPAN02", 0..n, None, vec![holder("SPAN01", 1, 2)]),
            tape(
                "SPAN03",
                2..n,
                None,
                vec![holder("SPAN01", 1, 2), holder("SPAN02", 1, n64)],
            ),
            tape("SPAN22", 2..n, Some(n - 2), vec![holder("SPAN01", 1, 2)]),
            tape(
                "SPAN23",
                n - 1..n,
                None,
                vec![holder("SPAN01", 1, 2), holder("SPAN22", 3, n64 - 1)],
            ),
            tape("SPAN11", 0..n, Some(1), vec![]),
            tape("SPAN33", 2..n, None, vec![]),
        ];
        let mut tapes = BTreeMap::new();
        for spec in specs {
            let tdir = dir.join(spec.id.label);
            write_keys(&tdir);
            let mut fresh = staged.clone();
            for s in fresh.iter_mut() {
                s.unit.slices.clear();
            }
            let t = lay_out_tape(
                &tdir,
                fresh,
                true,
                true,
                &[],
                Era::V170Span,
                &[SpanOnTape {
                    unit: "big",
                    held: spec.held.clone(),
                }],
                &spec.id,
            );
            if let Some(i) = spec.cut_in {
                let big = unit_of(&t, "big");
                cut_tape_dir(
                    &tdir.join("tape"),
                    spec.id.label,
                    big.positions[i - spec.held.start],
                );
            }
            tapes.insert(spec.id.label, t);
        }
        SpanBox { n, tapes }
    })
}

/// The arguments of a restore of `big` into `dest`, with `extra` after them.
fn big_args(h: &Heir, dest: &str, extra: &[&str]) -> Vec<String> {
    let mut v: Vec<String> = [
        "--restore",
        "--key",
        &h.key("alice"),
        "--unit",
        "big",
        "--to",
        dest,
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();
    v.extend(extra.iter().map(|s| s.to_string()));
    v
}

fn strs(v: &[String]) -> Vec<&str> {
    v.iter().map(String::as_str).collect()
}

/// Every file in the heir's directory, the cartridges and tools aside, that
/// holds a slice of `big`: its ciphertext, whole or as the start of a file
/// padded past it. What `--stream-tapes` never leaves on disk.
fn slice_files(h: &Heir, b: &SpanBox) -> Vec<PathBuf> {
    let big = b.big();
    let wanted: Vec<(usize, &String)> = big.sizes.iter().copied().zip(&big.hashes).collect();
    let mut found = Vec::new();
    let mut stack = vec![h.dir.clone()];
    while let Some(d) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&d) else {
            continue;
        };
        for e in rd.flatten() {
            let p = e.path();
            let name = e.file_name();
            let ft = e.file_type().unwrap();
            if ft.is_dir() {
                if !["cart", "keys", "bin", "hostbin"].contains(&name.to_str().unwrap_or("")) {
                    stack.push(p);
                }
                continue;
            }
            if !ft.is_file() {
                continue;
            }
            let Ok(bytes) = std::fs::read(&p) else {
                continue;
            };
            if wanted
                .iter()
                .any(|(size, sha)| bytes.len() >= *size && sha256_hex(&bytes[..*size]) == **sha)
            {
                found.push(p);
            }
        }
    }
    found
}

/// The scratch directory a restore across tapes made inside `dest`, if any,
/// and the slice files in it by number.
fn scratch_slices(dest: &str) -> Option<(PathBuf, BTreeMap<usize, Vec<u8>>)> {
    let d = std::fs::read_dir(dest).ok()?.flatten().find(|e| {
        e.file_name()
            .to_str()
            .is_some_and(|n| n.starts_with(".tapectl-restore."))
    })?;
    let mut slices = BTreeMap::new();
    for e in std::fs::read_dir(d.path()).unwrap().flatten() {
        let name = e.file_name().to_str().unwrap().to_string();
        if let Some(num) = name
            .strip_prefix("slice.")
            .and_then(|r| r.strip_suffix(".enc"))
        {
            slices.insert(num.parse().unwrap(), std::fs::read(e.path()).unwrap());
        }
    }
    Some((d.path(), slices))
}

/// The scratch directory holds slices 1 to `upto` exactly as their tapes do,
/// still encrypted (age's header first), and nothing else.
fn assert_scratch_holds(dest: &str, b: &SpanBox, upto: usize, what: &str) {
    let big = b.big();
    let (dir, slices) =
        scratch_slices(dest).unwrap_or_else(|| panic!("{what}: no scratch directory in {dest}"));
    assert_eq!(
        slices.keys().copied().collect::<Vec<_>>(),
        (1..=upto).collect::<Vec<_>>(),
        "{what}: the slices in {}",
        dir.display()
    );
    for (num, bytes) in &slices {
        assert!(
            bytes.starts_with(b"STUBAGE:"),
            "{what}: slice {num} is ciphertext"
        );
        assert_eq!(
            sha256_hex(bytes),
            big.hashes[num - 1],
            "{what}: slice {num} is the tape's"
        );
    }
}

/// The `dd` runs that wrote into `dest` (`of=` under it), from `procs`.
fn dd_into(h: &Heir, dest: &str) -> Vec<String> {
    std::fs::read_to_string(h.dir.join("procs"))
        .unwrap_or_default()
        .lines()
        .filter(|l| {
            l.starts_with("start ") && l.contains(" dd ") && l.contains(&format!("of={dest}"))
        })
        .map(str::to_string)
        .collect()
}

/// The reads of each cartridge of a run, split where `load` swapped them.
fn reads_by_cartridge(h: &Heir) -> Vec<Vec<i64>> {
    h.ops().split(|l| l == "swap").map(reads_in).collect()
}

/// Item 93 and ADR-0012 item 136 proof 2: of two copies over three tapes, the
/// default mode restores `big` from tapes 1 and 3, byte for byte: tape 1's run
/// 1..2 into scratch, then the prompt, then tape 3 taken by slice 3's sha256
/// and its run 3..N read, then one dar run over the scratch. From tape 2 alone,
/// which holds it whole, it restores with no swap and no scratch, as any unit.
#[test]
fn item_93s_two_copies_restore_from_tapes_1_and_3_or_from_tape_2_alone() {
    let b = span_box();
    let n = b.n;
    let big = b.big();
    let h = Heir::of_box(&b.of(&["SPAN01", "SPAN02", "SPAN03"]), 0);
    let dest = h.sub("restored");
    let args = big_args(&h, &dest, &[]);
    let (code, text) = h.run_prompted(&strs(&args), |k, _| {
        assert_eq!(k, 1, "one cartridge change");
        assert_eq!(h.loaded(), "cart/SPAN01");
        h.load("SPAN03");
        Some(String::new())
    });
    assert_eq!(code, 0, "{text}");
    assert!(text.contains("RESTORE COMPLETE"), "{text}");
    assert!(same_tree(&big.src, Path::new(&dest)), "{text}");
    for needle in [
        format!("'big' (version 2) is held across tapes: this tape holds slices 1 to 2 of {n}."),
        format!(
            "'big' (version 2) needs slice 3 of {n} next: {} bytes, sha256 {}",
            big.sizes[2], big.hashes[2]
        ),
        "This cartridge identifies as: SPAN03".to_string(),
        format!("It gives slice(s) 3 to {n} of 'big'"),
        "Every slice of 'big' is in the scratch directory".to_string(),
    ] {
        assert!(text.contains(&needle), "{needle:?}:\n{text}");
    }
    // Tape 1's index cannot name a tape written after it.
    assert!(
        text.contains("No index this key opened names a tape that holds slice 3"),
        "{text}"
    );
    let by_tape = reads_by_cartridge(&h);
    assert_eq!(by_tape.len(), 2, "{:#?}", h.ops());
    let t1 = unit_of(b.t("SPAN01"), "big");
    let t3 = unit_of(b.t("SPAN03"), "big");
    for p in &t1.positions[..2] {
        assert!(by_tape[0].contains(p), "tape 1's slices: {:#?}", h.ops());
    }
    // Its short seal is at the planned place of slice 3, and is read once,
    // first (R1); nothing past it is.
    assert_eq!(
        by_tape[0].iter().filter(|p| **p == t1.positions[2]).count(),
        1,
        "{:#?}",
        h.ops()
    );
    assert!(
        t1.positions[3..].iter().all(|p| !by_tape[0].contains(p)),
        "nothing past tape 1's end: {:#?}",
        h.ops()
    );
    for p in &t3.positions {
        assert!(by_tape[1].contains(p), "tape 3's slices: {:#?}", h.ops());
    }
    assert!(scratch_slices(&dest).is_none(), "the scratch is gone");

    // ADR-0012 item 139: tape 2, a whole copy, gives slices 3..N after tape
    // 1's spool as well (a tape that holds the version whole gives any slice).
    let h = Heir::of_box(&b.of(&["SPAN01", "SPAN02", "SPAN03"]), 0);
    let dest = h.sub("restored");
    let args = big_args(&h, &dest, &[]);
    let (code, text) = h.run_prompted(&strs(&args), |k, _| {
        assert_eq!(k, 1, "one cartridge change");
        h.load("SPAN02");
        Some(String::new())
    });
    assert_eq!(code, 0, "{text}");
    assert!(same_tree(&big.src, Path::new(&dest)), "{text}");
    assert!(
        text.contains(&format!("It gives slice(s) 3 to {n} of 'big'")),
        "{text}"
    );

    let h = Heir::of_box(&b.of(&["SPAN01", "SPAN02", "SPAN03"]), 1);
    let dest = h.sub("restored");
    let args = big_args(&h, &dest, &[]);
    let (code, text) = h.run_prompted(&strs(&args), |_, text| {
        panic!("tape 2 holds big whole: no prompt\n{text}")
    });
    assert_eq!(code, 0, "{text}");
    assert!(same_tree(&big.src, Path::new(&dest)), "{text}");
    for word in ["held across tapes", "scratch directory", "needs slice"] {
        assert!(!text.contains(word), "{word:?}:\n{text}");
    }
    assert!(
        dd_into(&h, &dest).is_empty(),
        "no slice is written into --to"
    );
    assert_eq!(reads_by_cartridge(&h).len(), 1, "no swap");
}

/// Item 94 and ADR-0012 item 136 proof 8: a span over three tapes, 1..2,
/// 3..N-1 and N. The default mode keeps every slice in scratch across both
/// swaps (all of them there, encrypted, at each prompt); `--stream-tapes` takes
/// the same swaps with one dar run fed through pipes, and no slice is ever on
/// disk: none at either prompt, none after, and no `dd` writes into `--to`.
#[test]
fn a_three_tape_span_restores_across_two_swaps_either_way() {
    let b = span_box();
    let n = b.n;
    let big = b.big();
    for stream in [false, true] {
        let h = Heir::of_box(&b.of(&["SPAN01", "SPAN22", "SPAN23"]), 0);
        let dest = h.sub("restored");
        let extra: &[&str] = if stream { &["--stream-tapes"] } else { &[] };
        let args = big_args(&h, &dest, extra);
        let (code, text) = h.run_prompted(&strs(&args), |k, text| {
            let what = format!("stream {stream}, prompt {k}");
            if stream {
                assert!(
                    slice_files(&h, b).is_empty(),
                    "{what}: {:?}",
                    slice_files(&h, b)
                );
                assert!(scratch_slices(&dest).is_none(), "{what}: no scratch");
            } else {
                assert_scratch_holds(&dest, b, if k == 1 { 2 } else { n - 1 }, &what);
            }
            match k {
                1 => {
                    assert!(
                        text.contains(&format!("needs slice 3 of {n} next")),
                        "{what}"
                    );
                    h.load("SPAN22");
                }
                2 => {
                    assert!(
                        text.contains(&format!("needs slice {n} of {n} next")),
                        "{what}"
                    );
                    h.load("SPAN23");
                }
                _ => panic!("{what}: two swaps\n{text}"),
            }
            Some(String::new())
        });
        assert_eq!(code, 0, "stream {stream}: {text}");
        assert!(text.contains("RESTORE COMPLETE"), "{text}");
        assert!(
            same_tree(&big.src, Path::new(&dest)),
            "stream {stream}: {text}"
        );
        assert!(
            text.contains(&format!("It gives slice(s) 3 to {} of 'big'", n - 1)),
            "{text}"
        );
        assert!(
            text.contains(&format!("It gives slice(s) {n} to {n} of 'big'")),
            "{text}"
        );
        assert_eq!(reads_by_cartridge(&h).len(), 3, "{:#?}", h.ops());
        if stream {
            assert!(text.contains("--stream-tapes: one dar run"), "{text}");
            assert!(!text.contains("scratch directory"), "{text}");
            assert!(slice_files(&h, b).is_empty(), "{:?}", slice_files(&h, b));
            assert!(dd_into(&h, &dest).is_empty(), "{:?}", dd_into(&h, &dest));
        }
    }
}

/// R8, and the coordinator's ruling that a cartridge is never taken by its
/// label: four wrong cartridges, each refused with its reason, the prompt
/// asking again each time, and nothing read from any of them but File 0, the
/// front index and the seal (R5): tape 1 again (its plan lists slice 3, after
/// its end), a tape whose run does not start at slice 3 (SPAN23), a tape whose
/// seal does not cover slice 3 (SPAN11), and a copy of tape 3, label, uuid and
/// all, whose front index lists another sha256 for slice 3. Then tape 3 is
/// taken, tape 1's ciphertext still in scratch, and the restore completes.
#[test]
fn a_cartridge_is_taken_only_by_the_next_slices_hash_at_a_position_its_seal_covers() {
    let b = span_box();
    let n = b.n;
    let big = b.big();
    let h = Heir::of_box(&b.of(&["SPAN01", "SPAN03", "SPAN23", "SPAN11"]), 0);
    // SPAN03X: tape 3 with File 3 listing another sha256 for slice 3.
    let x = h.dir.join("cart/SPAN03X");
    copy_dir(&h.dir.join("cart/SPAN03"), &x);
    let fi: String = String::from_utf8(
        std::fs::read(x.join("3"))
            .unwrap()
            .into_iter()
            .filter(|b| *b != 0)
            .collect(),
    )
    .unwrap();
    assert_eq!(fi.matches(&big.hashes[2]).count(), 1, "{fi}");
    std::fs::write(
        x.join("3"),
        padded(fi.replace(&big.hashes[2], &"e".repeat(64)).into_bytes()),
    )
    .unwrap();
    let offers = ["SPAN01", "SPAN23", "SPAN11", "SPAN03X", "SPAN03"];
    let dest = h.sub("restored");
    let args = big_args(&h, &dest, &[]);
    let (code, text) = h.run_prompted(&strs(&args), |k, _| {
        assert!(k <= offers.len(), "asked too often");
        assert_scratch_holds(&dest, b, 2, &format!("prompt {k}"));
        h.load(offers[k - 1]);
        Some(String::new())
    });
    assert_eq!(code, 0, "{text}");
    assert!(same_tree(&big.src, Path::new(&dest)), "{text}");
    assert_eq!(text.matches(SPAN_PROMPT).count(), 5, "{text}");
    let s3 = &big.hashes[2][..16];
    let t1 = unit_of(b.t("SPAN01"), "big");
    let past = format!(
        "Its front index lists slice 3 (sha256 {s3}...) at tape file {}, but the tape ended after\n>>> file {} (its seal marker says so): that slice is not on it. Nothing is taken from it.",
        t1.positions[2],
        t1.positions[1]
    );
    let none = format!(
        "Its front index lists no data slice with the sha256 of slice 3 ({s3}...), so it does\n>>> not hold that slice. Nothing is taken from it."
    );
    assert_eq!(text.matches(&past).count(), 2, "SPAN01 and SPAN11:\n{text}");
    assert_eq!(
        text.matches(&none).count(),
        2,
        "SPAN23 and SPAN03X:\n{text}"
    );
    assert_eq!(
        text.matches("This cartridge identifies as: SPAN03\n")
            .count(),
        2,
        "{text}"
    );
    assert_eq!(
        text.matches(&format!("It gives slice(s) 3 to {n} of 'big'"))
            .count(),
        1
    );
    // Nothing but File 0, File 3 and the seal of a cartridge not taken.
    let by_tape = reads_by_cartridge(&h);
    assert_eq!(by_tape.len(), 6, "{:#?}", h.ops());
    for (i, reads) in by_tape[1..5].iter().enumerate() {
        let seal = reads
            .iter()
            .copied()
            .filter(|p| *p != 0 && *p != 3)
            .collect::<Vec<_>>();
        assert!(
            seal.len() <= 1,
            "{}: only its seal besides Files 0 and 3: {reads:?}",
            offers[i]
        );
    }
}

/// "Where a restore starts" (X10): started on tape 3, whose run starts at
/// slice 3, the restore refuses before it reads a slice, names the run here,
/// and names the holders of slice 1 its index names (SPAN01's range 1..2 and
/// SPAN02, whole), never this tape. With an index that names no holder of
/// slice 1, it points to `--where` on the newest tape.
#[test]
fn started_on_a_tape_without_slice_1_it_refuses_naming_slice_1s_holders() {
    let b = span_box();
    let n = b.n;
    for (label, named) in [("SPAN03", true), ("SPAN33", false)] {
        let h = Heir::of_box(&b.of(&[label]), 0);
        let first = unit_of(b.t(label), "big").positions[0];
        let dest = h.sub("restored");
        let args = big_args(&h, &dest, &[]);
        let (code, text) = runv(&h, &args);
        assert_eq!(code, 1, "{label}: {text}");
        for needle in [
            format!("slices 3 to {n} of its {n}, at tape files"),
            "and the run on this tape starts at slice 3. Load a tape that".to_string(),
            "--where 'big' --key ".to_string(),
            "No slice has been read.".to_string(),
        ] {
            assert!(text.contains(&needle), "{label}: {needle:?}:\n{text}");
        }
        let rows = [
            format!("    tape SPAN01  (sealed 2026-09-01T10:00:00Z, a range: not the whole version)  slices 1-2 of {n}"),
            format!("    tape SPAN02  (sealed 2026-09-01T10:00:00Z)  slices 1-{n} of {n}"),
        ];
        if named {
            assert!(
                text.contains("names these tapes as holding slice 1"),
                "{text}"
            );
            for r in &rows {
                assert!(text.contains(r), "{r:?}:\n{text}");
            }
        } else {
            assert!(
                text.contains("No index this key opened names a tape that holds slice 1"),
                "{text}"
            );
        }
        assert!(
            !text.contains(&format!("    tape {label}  (this tape")),
            "{text}"
        );
        assert!(!text.contains(SPAN_PROMPT), "{text}");
        assert!(
            walk(&h).0.iter().all(|p| *p < first),
            "{label}: no slice is read:\n{:#?}",
            h.ops()
        );
        assert_eq!(std::fs::read_dir(&dest).map(|d| d.count()).unwrap_or(0), 0);
    }
}

/// Item 132: before any slice is read, the default mode's space check counts
/// the whole version's ciphertext for the scratch besides the files for
/// `--to`; short of it, it refuses, names `--stream-tapes`, and reads no slice
/// (it never chooses the stream itself, item 88). The same room is enough for
/// `--stream-tapes`, which keeps no slice on disk: the positive control.
#[test]
fn the_space_check_counts_the_whole_ciphertext_and_names_stream_tapes() {
    let b = span_box();
    let big = b.big();
    let bytes: usize = big.sizes.iter().sum();
    let kib = (bytes + bytes / 2) as u64 / 1024;
    let h = Heir::of_box(&b.of(&["SPAN01", "SPAN03"]), 0);
    h.stub("df", &df_with_free_kib(kib));
    // The slices on tape 1 (its seal is at the planned place of slice 3).
    let here = unit_of(b.t("SPAN01"), "big").positions[..2].to_vec();
    let dest = h.sub("restored");
    let args = big_args(&h, &dest, &[]);
    let (code, text) = h.run_prompted(&strs(&args), |_, text| panic!("refused first:\n{text}"));
    assert_eq!(code, 1, "{text}");
    let fatal = flat(text.split("FATAL:").nth(1).expect("a FATAL"));
    for needle in [
        "not enough disk space in",
        "to restore this unit across tapes",
        "No slice has been read yet.",
        "the sum of their encrypted_bytes in the manifest",
        "run the same command with --stream-tapes, which keeps no slice on disk",
    ] {
        assert!(fatal.contains(needle), "{needle:?}:\n{fatal}");
    }
    assert!(
        reads(&h).iter().all(|p| !here.contains(p)),
        "no slice is read:\n{:#?}",
        h.ops()
    );
    assert!(scratch_slices(&dest).is_none(), "no scratch was made");

    let args = big_args(&h, &dest, &["--stream-tapes"]);
    let (code, text) = h.run_prompted(&strs(&args), |_, _| {
        h.load("SPAN03");
        Some(String::new())
    });
    assert_eq!(code, 0, "{text}");
    assert!(same_tree(&big.src, Path::new(&dest)), "{text}");
}

/// The coordinator's ruling on the prompt (plan §9): the answer comes from the
/// input, with no time limit, so an answer that comes late is still taken; no
/// `mt` or `dd` is running while the prompt waits, and none starts until the
/// answer is in (the device is opened per command, never held); and, by
/// default, tape 1's ciphertext waits in scratch, untouched, all the while.
#[test]
fn the_prompt_waits_without_a_time_limit_and_holds_no_device() {
    let b = span_box();
    let big = b.big();
    for stream in [false, true] {
        let h = Heir::of_box(&b.of(&["SPAN01", "SPAN03"]), 0);
        let dest = h.sub("restored");
        let extra: &[&str] = if stream { &["--stream-tapes"] } else { &[] };
        let args = big_args(&h, &dest, extra);
        let (code, text) = h.run_prompted(&strs(&args), |_, _| {
            assert!(
                h.running().is_empty(),
                "running at the prompt: {:?}",
                h.running()
            );
            let before = std::fs::read_to_string(h.dir.join("procs")).unwrap();
            assert!(
                before.lines().any(|l| l.contains(" dd ")),
                "the log is written"
            );
            if !stream {
                assert_scratch_holds(&dest, b, 2, "at the prompt");
            }
            std::thread::sleep(std::time::Duration::from_secs(3));
            assert_eq!(
                std::fs::read_to_string(h.dir.join("procs")).unwrap(),
                before,
                "no mt or dd while the prompt waits"
            );
            if !stream {
                assert_scratch_holds(&dest, b, 2, "after the wait");
            }
            h.load("SPAN03");
            Some(String::new())
        });
        assert_eq!(code, 0, "stream {stream}: {text}");
        assert!(
            same_tree(&big.src, Path::new(&dest)),
            "stream {stream}: {text}"
        );
        assert!(
            text.contains("This waits for as long as you take."),
            "{text}"
        );
    }
}

/// Stopping at the prompt (quit, or the end of the input): nothing of the
/// unit is restored and the run fails, saying so; the scratch is removed.
/// With `--stream-tapes`, dar has restored what the slices before held, and
/// the message says those files are only part of the unit.
#[test]
fn quitting_at_the_prompt_restores_nothing_and_says_so() {
    let b = span_box();
    // `done` is `quit` here (the fresh read of File 1, #478 m1): a restore
    // across tapes does not go on without a slice, and the prompt says so.
    for (stream, reply) in [
        (false, Some("quit")),
        (false, Some("done")),
        (false, None),
        (true, Some("quit")),
        (true, Some("done")),
    ] {
        let h = Heir::of_box(&b.of(&["SPAN01", "SPAN03"]), 0);
        let dest = h.sub("restored");
        let extra: &[&str] = if stream { &["--stream-tapes"] } else { &[] };
        let args = big_args(&h, &dest, extra);
        let (code, text) = h.run_prompted(&strs(&args), |_, _| reply.map(str::to_string));
        assert_eq!(code, 1, "{stream} {reply:?}: {text}");
        let says = if stream {
            "done or quit STOPS this restore: it does not go on without the slice, and the files dar has"
        } else {
            "done or quit STOPS this restore and discards the slices read so far, with the scratch directory:"
        };
        assert!(text.contains(says), "{stream} {reply:?}: {text}");
        // And to eject the cartridge in the drive before loading the next (m10).
        let eject = text
            .find(EJECT_FIRST)
            .unwrap_or_else(|| panic!("the eject line: {text}"));
        assert!(eject < text.find(SPAN_PROMPT).unwrap(), "{text}");
        let fatal = flat(text.split("FATAL:").nth(1).expect("a FATAL"));
        assert!(
            fatal.contains("stopped before slice 3 of 'big' (version 2)"),
            "{fatal}"
        );
        if stream {
            assert!(
                fatal.contains("those files are part of the unit only"),
                "{fatal}"
            );
        } else {
            assert!(fatal.contains("nothing of it was restored"), "{fatal}");
            assert_eq!(std::fs::read_dir(&dest).unwrap().count(), 0, "{text}");
        }
        assert!(scratch_slices(&dest).is_none(), "the scratch is removed");
    }
}

/// The pid of the `bash` running this heir's RESTORE.sh (not the `timeout`
/// around it), from `/proc`.
fn script_pid(h: &Heir) -> String {
    let script = h.dir.join("RESTORE.sh");
    for e in std::fs::read_dir("/proc").unwrap().flatten() {
        let Ok(cmd) = std::fs::read(e.path().join("cmdline")) else {
            continue;
        };
        let args: Vec<&[u8]> = cmd.split(|b| *b == 0).collect();
        if args.len() > 1
            && args[0].ends_with(b"bash")
            && args[1] == script.as_os_str().as_encoded_bytes()
        {
            return e.file_name().to_str().unwrap().to_string();
        }
    }
    panic!("no bash running {}", script.display());
}

/// The fresh read of File 1 (#478, m2): File 1 said a killed run leaves its
/// scratch directory, but the exit trap removes it, with every slice read,
/// on Ctrl-C (SIGINT), a closed terminal (SIGHUP) and `kill` (SIGTERM); only
/// a run with no chance to clean up leaves it. The signal comes at the
/// prompt, the run ends before any answer is typed (it is not `quit`), and
/// the scratch is gone. SIGKILL is the control: it leaves the scratch, as
/// File 1 says, holding the slices read.
#[test]
fn ctrl_c_a_hangup_or_kill_at_the_prompt_removes_the_scratch() {
    let b = span_box();
    for sig in ["INT", "HUP", "TERM", "KILL"] {
        let h = Heir::of_box(&b.of(&["SPAN01", "SPAN03"]), 0);
        let dest = h.sub("restored");
        let args = big_args(&h, &dest, &[]);
        let (code, text) = h.run_prompted(&strs(&args), |_, _| {
            assert_scratch_holds(&dest, b, 2, &format!("SIG{sig} at the prompt"));
            let pid = script_pid(&h);
            let sent = Command::new("kill")
                .args([format!("-{sig}"), pid.clone()])
                .status()
                .unwrap();
            assert!(sent.success(), "kill -{sig} {pid}");
            let until = std::time::Instant::now() + std::time::Duration::from_secs(60);
            while Path::new(&format!("/proc/{pid}")).exists() {
                assert!(std::time::Instant::now() < until, "SIG{sig}: still running");
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
            Some(String::new())
        });
        assert_ne!(code, 0, "SIG{sig}: {text}");
        assert!(!text.contains("stopped before slice"), "SIG{sig}: {text}");
        if sig == "KILL" {
            assert_scratch_holds(&dest, b, 2, "after SIGKILL, left for the heir to remove");
            continue;
        }
        assert!(
            scratch_slices(&dest).is_none(),
            "SIG{sig}: the scratch is removed: {text}"
        );
        assert_eq!(
            std::fs::read_dir(&dest).unwrap().count(),
            0,
            "SIG{sig}: {text}"
        );
    }
}

/// What a restore across tapes refuses before it reads a slice, each by name:
/// a dar that cannot read slices through pipes (no decrypted fallback, only
/// ciphertext on disk), `--slices-from`/`--swap-tapes` (with `--path` too:
/// it takes each slice by its sha256 from the cartridges it asks for), a
/// second `--unit`, and the usage errors of `--stream-tapes`, which takes no
/// `--path`.
#[test]
fn what_a_restore_across_tapes_refuses_before_it_reads_a_slice() {
    let b = span_box();
    let first = unit_of(b.t("SPAN01"), "big").positions[0];
    let h = Heir::of_box(&b.of(&["SPAN01"]), 0);
    let dest = h.sub("restored");
    let dir = h.sub("copies");
    std::fs::create_dir_all(&dir).unwrap();
    for (extra, needle) in [
        (
            vec!["--slices-from", dir.as_str()],
            "so it takes no\n       --slices-from or --swap-tapes",
        ),
        (
            vec!["--swap-tapes"],
            "so it takes no\n       --slices-from or --swap-tapes",
        ),
        (
            vec!["--path", "file0.bin", "--swap-tapes"],
            "so it takes no --slices-from or --swap-tapes. Run the same command without them.",
        ),
        (
            vec!["--path", "file0.bin", "--slices-from", dir.as_str()],
            "so it takes no --slices-from or --swap-tapes. Run the same command without them.",
        ),
        (vec!["--unit", "big"], "restore it on its own"),
    ] {
        let args = big_args(&h, &dest, &extra);
        let (code, text) = runv(&h, &args);
        assert_eq!(code, 1, "{extra:?}: {text}");
        assert!(text.contains(needle), "{extra:?}: {needle:?}:\n{text}");
        assert!(
            text.contains("No slice has been read."),
            "{extra:?}: {text}"
        );
        assert!(
            walk(&h).0.iter().all(|p| *p < first),
            "{extra:?}: {:#?}",
            h.ops()
        );
    }
    for (extra, needle) in [
        (
            vec!["--stream-tapes", "--path", "file0.bin"],
            "--stream-tapes reads a whole unit across tapes: it takes no --path",
        ),
        (
            vec!["--stream-tapes", "--swap-tapes"],
            "--stream-tapes takes every slice by its sha256",
        ),
    ] {
        let args = big_args(&h, &dest, &extra);
        let (code, text) = runv(&h, &args);
        assert_eq!(code, 1, "{extra:?}: {text}");
        assert!(text.contains(needle), "{extra:?}: {needle:?}:\n{text}");
        assert!(
            reads(&h).is_empty(),
            "a usage error reads nothing: {:#?}",
            h.ops()
        );
    }
    let (code, text) = h.run(&[
        "--restore",
        "--key",
        &h.key("alice"),
        "--all",
        "--stream-tapes",
        "--to",
        &dest,
    ]);
    assert_eq!(code, 1, "{text}");
    assert!(text.contains("--stream-tapes reads one unit across tapes: give --unit U, not --all"));

    let old =
        "#!/bin/sh\ncase \"$*\" in -V*) echo ' dar version 2.7.8, Copyright'; exit 0 ;; esac\n\
               exec \"$FAKE_TAPE/hostbin/dar\" \"$@\"\n";
    h.stub("dar", old);
    let args = big_args(&h, &dest, &[]);
    let (code, text) = runv(&h, &args);
    assert_eq!(code, 1, "{text}");
    let fatal = flat(text.split("FATAL:").nth(1).expect("a FATAL"));
    for needle in [
        "dar 2.7.8 cannot restore 'big' across tapes.",
        "only encrypted slices are ever kept on disk",
        "dar 2.7.9 or later with the unit's catalogue (the envelope holds it), or dar 2.7.21 or later.",
        "No slice has been read.",
    ] {
        assert!(fatal.contains(needle), "{needle:?}:\n{fatal}");
    }
    assert!(walk(&h).0.iter().all(|p| *p < first), "{:#?}", h.ops());
}

/// §1.5 and §4.4 on item 93's three tapes: `--where big` and `--find` print
/// each holder its index names with its run, this tape's as the run it really
/// holds (tape 1's plan was 1..N, its seal ends it after slice 2), a range
/// marked as one, and the whole copy not.
#[test]
fn where_and_find_on_each_of_item_93s_tapes_print_each_holders_run() {
    let b = span_box();
    let n = b.n;
    let this_range = |l: &str, a: usize, z: usize| {
        format!("    tape {l}  (this tape, a range: not the whole version)  slices {a}-{z} of {n}")
    };
    let other_range = |l: &str, a: usize, z: usize| {
        format!("    tape {l}  (sealed 2026-09-01T10:00:00Z, a range: not the whole version)  slices {a}-{z} of {n}")
    };
    for (label, rows) in [
        ("SPAN01", vec![this_range("SPAN01", 1, 2)]),
        (
            "SPAN02",
            vec![
                format!("    tape SPAN02  (this tape)  slices 1-{n} of {n}"),
                other_range("SPAN01", 1, 2),
            ],
        ),
        (
            "SPAN03",
            vec![
                this_range("SPAN03", 3, n),
                other_range("SPAN01", 1, 2),
                format!("    tape SPAN02  (sealed 2026-09-01T10:00:00Z)  slices 1-{n} of {n}"),
            ],
        ),
    ] {
        let h = Heir::of_box(&b.of(&[label]), 0);
        let (code, text) = h.run(&["--where", "big", "--key", &h.key("alice")]);
        assert_eq!(code, 0, "{label}: {text}");
        let (fcode, found) = h.run(&["--find", "file0", "--key", &h.key("alice")]);
        assert_eq!(fcode, 0, "{label}: {found}");
        let holders = block(&found, "Tapes holding big version 2:");
        for r in &rows {
            assert!(text.contains(r), "{label}: {r:?}:\n{text}");
            assert!(
                holders.iter().any(|l| l == r),
                "{label}: {r:?}:\n{holders:#?}"
            );
        }
        assert_eq!(
            text.matches("    tape ").count(),
            rows.len(),
            "{label}: no other holder:\n{text}"
        );
    }
}

/// The fresh read of File 1 for a span (#478, S2): taking stock with
/// `--find-envelope`, an heir counted two tapes that each hold part of `big`
/// as two copies, because the table gave the full slice count and size on
/// both. It now marks a unit this tape's plan holds only part of (the slices
/// its block gives a `tape_position`, against the block's N), in every
/// envelope's table that lists it, and says once per table what the mark
/// means. Tape 3 of item 93 holds 3..N: marked. Tape 2 holds it whole: not.
/// Tape 1's plan was 1..N and its seal ended it after slice 2: the table
/// reads no seal (`find_envelope_on_a_short_tape_reads_no_seal`), so it shows
/// the plan, unmarked, which is why File 1 sends the heir to `--list --unit`
/// and `--where` to be sure.
#[test]
fn find_envelope_marks_a_unit_this_tapes_plan_holds_only_part_of() {
    let b = span_box();
    let n = b.n;
    let footnote =
        "    PART: a unit held across tapes, of which this tape's plan holds only the slices named";
    let big_rows = |text: &str| -> Vec<String> {
        text.lines()
            .filter(|l| l.starts_with("    big "))
            .map(str::to_string)
            .collect()
    };
    for (label, part) in [
        (
            "SPAN03",
            Some(format!("  PART: version 2, slices 3 to {n} of {n}")),
        ),
        ("SPAN02", None),
        ("SPAN01", None),
    ] {
        let h = Heir::of_box(&b.of(&[label]), 0);
        let (code, text) = h.run(&["--find-envelope", "--key", &h.key("esc")]);
        assert_eq!(code, 0, "{label}: {text}");
        let rows = big_rows(&text);
        assert_eq!(
            rows.len(),
            2,
            "{label}: alice's envelope and the operator's:\n{text}"
        );
        for r in &rows {
            let cols: Vec<&str> = r.split_whitespace().collect();
            assert_eq!(
                &cols[..3],
                &["big", "2", n.to_string().as_str()],
                "{label}: the Slices column counts the whole version: {r:?}"
            );
            match &part {
                Some(p) => assert!(r.ends_with(p.as_str()), "{label}: {r:?}\n{text}"),
                None => assert!(!r.contains("PART"), "{label}: {r:?}\n{text}"),
            }
        }
        let want = if part.is_some() { 2 } else { 0 };
        assert_eq!(text.matches(footnote).count(), want, "{label}:\n{text}");
        assert_eq!(text.matches("PART: ").count(), 2 * want, "{label}:\n{text}");
    }
}

/// The default mode takes a slice that fails from another cartridge: tape 3's
/// slice 4 does not match its front index, so the restore stops reading tape
/// 3 there and asks for slice 4, and tape 2, the whole copy, gives 4..N. Only
/// the slice's sha256 decides, and the restore is byte for byte.
#[test]
fn a_slice_that_fails_is_asked_for_again_from_another_cartridge() {
    let b = span_box();
    let n = b.n;
    let big = b.big();
    let h = Heir::of_box(&b.of(&["SPAN01", "SPAN02", "SPAN03"]), 0);
    let bad = unit_of(b.t("SPAN03"), "big").positions[1];
    let f = h.dir.join(format!("cart/SPAN03/{bad}"));
    let mut bytes = std::fs::read(&f).unwrap();
    bytes[100] ^= 0xff;
    std::fs::write(&f, bytes).unwrap();
    let dest = h.sub("restored");
    let args = big_args(&h, &dest, &[]);
    let (code, text) = h.run_prompted(&strs(&args), |k, _| {
        match k {
            1 => h.load("SPAN03"),
            2 => {
                assert_scratch_holds(&dest, b, 3, "after tape 3's bad slice");
                h.load("SPAN02");
            }
            _ => panic!("two cartridge changes"),
        }
        Some(String::new())
    });
    assert_eq!(code, 0, "{text}");
    assert!(same_tree(&big.src, Path::new(&dest)), "{text}");
    for needle in [
        format!("slice 4 (tape file {bad}) does not match the front index"),
        "another cartridge that holds it is asked for".to_string(),
        format!("needs slice 4 of {n} next"),
        format!("It gives slice(s) 4 to {n} of 'big'"),
    ] {
        assert!(text.contains(&needle), "{needle:?}:\n{text}");
    }
}

/// A cartridge this script cannot read at all (its File 0 requires a feature
/// it does not know) does not end a restore across tapes: it is named, nothing
/// is taken from it, the slices already in scratch stay, and the prompt asks
/// again; the right cartridge then finishes the restore.
#[test]
fn a_cartridge_the_script_cannot_read_is_named_and_the_prompt_asks_again() {
    let b = span_box();
    let n = b.n;
    let big = b.big();
    let h = Heir::of_box(&b.of(&["SPAN01", "SPAN03"]), 0);
    let x = h.dir.join("cart/SPAN03F");
    copy_dir(&h.dir.join("cart/SPAN03"), &x);
    let f0: String = String::from_utf8(
        std::fs::read(x.join("0"))
            .unwrap()
            .into_iter()
            .filter(|b| *b != 0)
            .collect(),
    )
    .unwrap();
    std::fs::write(
        x.join("0"),
        padded(
            with_requires(
                &f0,
                "requires = [\"short-seal\", \"spanning\", \"zz-future\"]",
            )
            .into_bytes(),
        ),
    )
    .unwrap();
    let dest = h.sub("restored");
    let args = big_args(&h, &dest, &[]);
    let (code, text) = h.run_prompted(&strs(&args), |k, _| {
        assert_scratch_holds(&dest, b, 2, &format!("prompt {k}"));
        h.load(["SPAN03F", "SPAN03"][k - 1]);
        Some(String::new())
    });
    assert_eq!(code, 0, "{text}");
    assert!(same_tree(&big.src, Path::new(&dest)), "{text}");
    assert_eq!(text.matches(SPAN_PROMPT).count(), 2, "{text}");
    for needle in [
        "NOTE: this tape requires feature(s) zz-future, which this script does not know",
        "Nothing is taken from this cartridge: its file map cannot be used (see above).",
        &format!("It gives slice(s) 3 to {n} of 'big'"),
    ] {
        assert!(text.contains(needle), "{needle:?}:\n{text}");
    }
    assert!(!text.contains("FATAL"), "{text}");
}

// ---- --restore --path of a unit across tapes (volume-format-v2.md §4.4
// "RESTORE.sh", R8, R9; ADR-0012 items 132, 139; #478 L3) ----

/// The slices that hold each of `big`'s files, 1-based and inclusive, from its
/// dar catalogue (`dar -l -Tslice`, as RESTORE.sh asks it). dar lays files out
/// in the order the directory lists them, so a test asks rather than assumes.
fn big_slice_ranges(b: &SpanBox) -> BTreeMap<String, (usize, usize)> {
    let cat = b
        .t("SPAN02")
        .dir
        .parent()
        .unwrap()
        .join("work")
        .join(format!("{BIG_UUID}-v2"))
        .join(format!("{}_v2", &BIG_UUID[..8]));
    let out = Command::new("dar")
        .args(["-l", cat.to_str().unwrap(), "-N", "-Q", "-Tslice"])
        .env("HOME", "/nonexistent")
        .output()
        .expect("dar must be on PATH (issue #43)");
    assert!(out.status.success(), "{out:?}");
    let mut ranges = BTreeMap::new();
    for line in String::from_utf8_lossy(&out.stdout).lines() {
        let words: Vec<&str> = line.split_whitespace().collect();
        let Some(first) = words.first() else {
            continue;
        };
        let (a, z) = first.split_once('-').unwrap_or((first, first));
        let (Ok(a), Ok(z)) = (a.parse::<usize>(), z.parse::<usize>()) else {
            continue;
        };
        ranges.insert(words.last().unwrap().to_string(), (a, z));
    }
    assert!(ranges.len() >= 4, "{ranges:?}");
    ranges
}

/// A file of `big` whose data lies wholly in slices 1 and 2 (SPAN01's run),
/// so a `--path` of it needs slices 1 and 2 and the last.
fn file_on_tape_1(b: &SpanBox) -> String {
    big_slice_ranges(b)
        .into_iter()
        .find(|(name, (_, z))| name.ends_with(".bin") && *z <= 2)
        .map(|(name, _)| name)
        .expect("a file in slices 1-2: the first one dar wrote")
}

/// Every file of the heir's directory, the cartridges and tools aside, that
/// holds a DECRYPTED slice of `big` (the plaintext after the stub age's
/// 32-byte header), whole or as the start of it. What never sits on disk
/// across a cartridge change.
fn plain_slice_files(h: &Heir, b: &SpanBox) -> Vec<PathBuf> {
    let t = b.t("SPAN02");
    let big = b.big();
    let plains: Vec<Vec<u8>> = (0..big.positions.len())
        .map(|i| good_slice(t, &big, i)[32..].to_vec())
        .collect();
    let mut found = Vec::new();
    let mut stack = vec![h.dir.clone()];
    while let Some(d) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&d) else {
            continue;
        };
        for e in rd.flatten() {
            let p = e.path();
            let name = e.file_name();
            let ft = e.file_type().unwrap();
            if ft.is_dir() {
                if !["cart", "keys", "bin", "hostbin"].contains(&name.to_str().unwrap_or("")) {
                    stack.push(p);
                }
                continue;
            }
            if !ft.is_file() {
                continue;
            }
            let Ok(bytes) = std::fs::read(&p) else {
                continue;
            };
            if plains.iter().any(|pl| {
                let k = pl.len().min(4096);
                bytes.len() >= k && bytes[..k] == pl[..k]
            }) {
                found.push(p);
            }
        }
    }
    found
}

/// At a prompt of a `--path` across tapes: the scratch directory holds only
/// `slice.N.enc` files, each still ciphertext (the stub age's header first),
/// and no decrypted slice is anywhere on disk.
fn assert_only_ciphertext_waits(h: &Heir, b: &SpanBox, dest: &str, what: &str) {
    let (dir, _) =
        scratch_slices(dest).unwrap_or_else(|| panic!("{what}: no scratch directory in {dest}"));
    for e in std::fs::read_dir(&dir).unwrap().flatten() {
        let name = e.file_name().to_str().unwrap().to_string();
        assert!(
            name.starts_with("slice.") && name.ends_with(".enc"),
            "{what}: only encrypted slices wait in {}: {name}",
            dir.display()
        );
        assert!(
            std::fs::read(e.path()).unwrap().starts_with(b"STUBAGE:"),
            "{what}: {name} is ciphertext"
        );
    }
    assert!(
        plain_slice_files(h, b).is_empty(),
        "{what}: a decrypted slice is on disk: {:?}",
        plain_slice_files(h, b)
    );
}

/// §4.4: `--restore --path` of a spanning unit reads the slices its catalogue
/// names for the path, and the last, from whichever tapes hold them. Started on
/// tape 1, a file in slices 1-2 is read from tape 1 with no swap but for the
/// last slice, which item 93's layout puts on tape 3: one prompt, for it, and
/// nothing else of tape 3 is read. The file is byte for byte, and nothing else
/// of the unit is restored.
#[test]
fn a_path_across_tapes_reads_tape_1_and_asks_only_for_the_last_slice() {
    let b = span_box();
    let n = b.n;
    let big = b.big();
    let file = file_on_tape_1(b);
    let h = Heir::of_box(&b.of(&["SPAN01", "SPAN02", "SPAN03"]), 0);
    let dest = h.sub("restored");
    let args = big_args(&h, &dest, &["--path", &file]);
    let mut asked = Vec::new();
    let (code, text) = h.run_prompted(&strs(&args), |k, text| {
        asked.push(k);
        assert!(
            text.contains(&format!("needs slice {n} of {n} next")),
            "prompt {k}:\n{text}"
        );
        assert_only_ciphertext_waits(&h, b, &dest, "at the prompt");
        h.load("SPAN03");
        Some(String::new())
    });
    assert_eq!(code, 0, "{text}");
    assert!(text.contains("RESTORE COMPLETE"), "{text}");
    assert_eq!(
        asked,
        vec![1],
        "one cartridge change, for slice {n}:\n{text}"
    );
    assert_eq!(
        std::fs::read(Path::new(&dest).join(&file)).unwrap(),
        std::fs::read(big.src.join(&file)).unwrap(),
        "{text}"
    );
    let restored: Vec<PathBuf> = rel_files(Path::new(&dest))
        .into_iter()
        .filter(|p| !p.starts_with(".tapectl-damaged-files.txt"))
        .collect();
    assert_eq!(
        restored,
        vec![PathBuf::from(&file)],
        "only the path: {text}"
    );
    for needle in [
        format!("'big' (version 2) is held across tapes: this tape holds slices 1 to 2 of {n}."),
        format!("It gives slice(s) {n} to {n} of 'big'"),
        "Every slice --path needs is in the scratch directory".to_string(),
    ] {
        assert!(text.contains(&needle), "{needle:?}:\n{text}");
    }
    let by_tape = reads_by_cartridge(&h);
    assert_eq!(by_tape.len(), 2, "{:#?}", h.ops());
    let t1 = unit_of(b.t("SPAN01"), "big");
    let t3 = unit_of(b.t("SPAN03"), "big");
    for p in &t1.positions[..2] {
        assert!(by_tape[0].contains(p), "tape 1's slices: {:#?}", h.ops());
    }
    let last = *t3.positions.last().unwrap();
    assert!(
        by_tape[1].contains(&last),
        "tape 3's last slice: {:#?}",
        h.ops()
    );
    assert!(
        t3.positions[..t3.positions.len() - 1]
            .iter()
            .all(|p| !by_tape[1].contains(p)),
        "nothing else of tape 3: {:#?}",
        h.ops()
    );
    assert!(scratch_slices(&dest).is_none(), "the scratch is gone");
}

/// No start rule for `--path` (§4.4): started on tape 3, whose run starts at
/// slice 3, it takes the last slice from the tape it started on first, then
/// asks for slice 1, naming the holders tape 3's index names for it, and takes
/// slices 1 and 2 from tape 1. One prompt, for slice 1, after tape 3's last
/// slice was read.
#[test]
fn a_path_started_on_tape_3_takes_its_last_slice_there_and_asks_for_tape_1() {
    let b = span_box();
    let n = b.n;
    let big = b.big();
    let file = file_on_tape_1(b);
    let t1 = unit_of(b.t("SPAN01"), "big");
    let t3 = unit_of(b.t("SPAN03"), "big");
    let last = *t3.positions.last().unwrap();
    let h = Heir::of_box(&b.of(&["SPAN01", "SPAN02", "SPAN03"]), 2);
    let dest = h.sub("restored");
    let args = big_args(&h, &dest, &["--path", &file]);
    let mut asked = Vec::new();
    let (code, text) = h.run_prompted(&strs(&args), |k, text| {
        let wanted = text
            .lines()
            .filter_map(|l| l.split("needs slice ").nth(1))
            .next_back()
            .and_then(|r| r.split(' ').next())
            .unwrap()
            .to_string();
        asked.push(wanted);
        assert!(
            reads_in(&h.ops()).contains(&last),
            "tape 3's last slice is read before the prompt: {:#?}",
            h.ops()
        );
        assert_only_ciphertext_waits(&h, b, &dest, &format!("prompt {k}"));
        h.load("SPAN01");
        Some(String::new())
    });
    assert_eq!(code, 0, "{text}");
    assert!(text.contains("RESTORE COMPLETE"), "{text}");
    assert_eq!(
        asked,
        vec!["1".to_string()],
        "the prompts, in order:\n{text}"
    );
    assert_eq!(
        std::fs::read(Path::new(&dest).join(&file)).unwrap(),
        std::fs::read(big.src.join(&file)).unwrap(),
        "{text}"
    );
    for needle in [
        format!("'big' (version 2) is held across tapes: this tape holds slices 3 to {n} of {n}."),
        format!("'big' (version 2) needs slice 1 of {n} next"),
        "names these tapes as holding slice 1".to_string(),
        format!("    tape SPAN01  (sealed 2026-09-01T10:00:00Z, a range: not the whole version)  slices 1-2 of {n}"),
        "It gives slice(s) 1 to 2 of 'big'".to_string(),
    ] {
        assert!(text.contains(&needle), "{needle:?}:\n{text}");
    }
    assert!(
        !text.contains("and the run on this tape starts at slice"),
        "{text}"
    );
    let by_tape = reads_by_cartridge(&h);
    assert_eq!(by_tape.len(), 2, "{:#?}", h.ops());
    assert!(by_tape[0].contains(&last), "{:#?}", h.ops());
    assert!(
        t3.positions[..t3.positions.len() - 1]
            .iter()
            .all(|p| !by_tape[0].contains(p)),
        "only the last slice of tape 3: {:#?}",
        h.ops()
    );
    for p in &t1.positions[..2] {
        assert!(by_tape[1].contains(p), "tape 1's slices: {:#?}", h.ops());
    }
}

/// The coordinator's ruling on where `--path`'s slices wait (item 132's reason
/// applied to `--path`): at every prompt only ciphertext is in the scratch
/// directory, `slice.N.enc` and nothing else, and no decrypted slice is on disk
/// anywhere; the fake `dd` logs every read, and none wrote into `--to` but into
/// the scratch directory. Then a version some of whose needed slices are not at
/// hand ends at the quit, with the `--path` wording and nothing restored.
#[test]
fn a_path_across_tapes_keeps_only_ciphertext_until_every_slice_is_read() {
    let b = span_box();
    let n = b.n;
    let file = file_on_tape_1(b);
    let h = Heir::of_box(&b.of(&["SPAN01", "SPAN02", "SPAN03"]), 2);
    let dest = h.sub("restored");
    let args = big_args(&h, &dest, &["--path", &file]);
    let (code, text) = h.run_prompted(&strs(&args), |k, _| {
        let what = format!("prompt {k}");
        assert_only_ciphertext_waits(&h, b, &dest, &what);
        let (dir, slices) = scratch_slices(&dest).unwrap();
        assert_eq!(
            slices.keys().copied().collect::<Vec<_>>(),
            vec![n],
            "{what}: tape 3's last slice waits"
        );
        for line in dd_into(&h, &dest) {
            assert!(
                line.contains(&format!("of={}/slice.", dir.display())),
                "{what}: a dd into --to writes an encrypted slice to scratch only: {line}"
            );
        }
        // Then stop: the tapes that hold slices 1 and 2 are not at hand.
        Some("quit".to_string())
    });
    assert_eq!(code, 1, "{text}");
    let fatal = flat(text.split("FATAL:").nth(1).expect("a FATAL"));
    for needle in [
        format!("stopped before slice 1 of 'big' (version 2), of {n}."),
        "--path restores only from every slice it needs (those that hold the paths, and the last), so nothing was restored".to_string(),
        "--where names them.".to_string(),
    ] {
        assert!(fatal.contains(&needle), "{needle:?}:\n{fatal}");
    }
    assert!(
        text.contains("input) to stop: nothing is restored without every slice --path needs."),
        "{text}"
    );
    assert!(scratch_slices(&dest).is_none(), "the scratch is removed");
    assert_eq!(
        std::fs::read_dir(&dest).unwrap().count(),
        0,
        "nothing restored"
    );
    // The positive control: a decrypted slice on disk is found.
    let probe = h.dir.join("tmp").join("probe.dar");
    std::fs::write(&probe, &good_slice(b.t("SPAN02"), &b.big(), 1)[32..]).unwrap();
    assert_eq!(plain_slice_files(&h, b), vec![probe]);
}

// ---- --swap-tapes and a cartridge's seal (volume-format-v2.md §4.2 R5, §4.4
// R8 and its RESTORE.sh bullet; ADR-0012 items 101, 132, 139; #478 L3) ----

/// What `--swap-tapes` prints when it waits for another cartridge.
const SWAP_PROMPT: &str = "Load a cartridge that holds copies of these slices";

/// A copy of the cartridge `from` in the box as `to`, label, uuid and all.
fn copy_cart(h: &Heir, from: &str, to: &str) -> PathBuf {
    let d = h.dir.join("cart").join(to);
    copy_dir(&h.dir.join("cart").join(from), &d);
    d
}

/// The last file on a cartridge directory: its seal marker, when it has one.
fn last_file(d: &Path) -> i64 {
    std::fs::read_dir(d)
        .unwrap()
        .flatten()
        .filter_map(|e| e.file_name().to_str().and_then(|n| n.parse::<i64>().ok()))
        .max()
        .unwrap()
}

/// Overwrite one file of a cartridge directory with text that is no zone this
/// script reads: a seal marker or a front index that does not parse.
fn garble(d: &Path, pos: i64) {
    std::fs::write(
        d.join(pos.to_string()),
        padded(b"this is not a zone of a tape\n".to_vec()),
    )
    .unwrap();
}

/// R5 and R8 for `--swap-tapes` (§4.4: "`--slices-from` and `--swap-tapes`
/// accept a replacement slice from any tape by R8, a range included"). A
/// restore from tape 2, the whole copy, whose slice 4 fails its hash: tape 1
/// lists slice 4 in its plan, after the end its short seal states, and gives
/// nothing; a copy of tape 3 whose seal does not parse gives nothing either, in
/// span_accept's words; tape 3, which holds a range 3..N under a full seal,
/// gives slice 4 by its sha256 (item 139: any tape that holds the slice). Each
/// refusal reads nothing of the cartridge but File 0, File 3 and its seal.
#[test]
fn swap_tapes_takes_a_slice_from_a_range_only_at_a_position_its_seal_covers() {
    let b = span_box();
    let big = b.big();
    let h = Heir::of_box(&b.of(&["SPAN02", "SPAN01", "SPAN03"]), 0);
    let t1 = unit_of(b.t("SPAN01"), "big");
    let t2 = unit_of(b.t("SPAN02"), "big");
    let t3 = unit_of(b.t("SPAN03"), "big");
    let f = h.dir.join(format!("cart/SPAN02/{}", t2.positions[3]));
    let mut bytes = std::fs::read(&f).unwrap();
    bytes[100] ^= 0xff;
    std::fs::write(&f, bytes).unwrap();
    let s = copy_cart(&h, "SPAN03", "SPAN03S");
    garble(&s, last_file(&s));
    let offers = ["SPAN01", "SPAN03S", "SPAN03"];
    let dest = h.sub("restored");
    let args = big_args(&h, &dest, &["--swap-tapes"]);
    let (code, text) = h.run_prompted_at(&strs(&args), SWAP_PROMPT, |k, _| {
        assert!(k <= offers.len(), "asked too often");
        h.load(offers[k - 1]);
        Some(String::new())
    });
    assert_eq!(code, 0, "{text}");
    assert!(text.contains("RESTORE COMPLETE"), "{text}");
    assert!(same_tree(&big.src, Path::new(&dest)), "{text}");
    assert_eq!(text.matches(SWAP_PROMPT).count(), 3, "{text}");
    // Each prompt says to eject the cartridge in the drive first (#478 m10).
    assert_eq!(text.matches(EJECT_FIRST).count(), 3, "{text}");
    for needle in [
        format!(
            "unit 'big' slice 4, {} bytes, sha256 {}",
            big.sizes[3], big.hashes[3]
        ),
        format!(
            "This cartridge ended early, after file {} (its seal marker says so): nothing after that is read from it.",
            t1.positions[1]
        ),
        "No file in the front index of this cartridge has the sha256 of a slice still needed at a position\n>>> its seal marker covers.".to_string(),
        format!(
            "Its front index lists such a slice at tape file(s) {}, after file {}, where the tape ended\n>>> (its seal marker says so): that is not on it. Nothing is taken from it.",
            t1.positions[3], t1.positions[1]
        ),
        "Its seal marker cannot be read, so nothing is taken from it: a cartridge gives a slice only at a".to_string(),
        format!(
            "Tape file {} of this cartridge lists sha256 {}..., a slice still needed",
            t3.positions[1],
            &big.hashes[3][..16]
        ),
    ] {
        assert!(text.contains(&needle), "{needle:?}:\n{text}");
    }
    let by_tape = reads_by_cartridge(&h);
    assert_eq!(by_tape.len(), 4, "{:#?}", h.ops());
    let seal1 = t1.positions[2];
    assert!(
        by_tape[1].iter().all(|p| [0, 3, seal1].contains(p)),
        "tape 1: File 0, File 3 and its seal only: {:?}",
        by_tape[1]
    );
    assert!(
        by_tape[2].iter().all(|p| !t3.positions.contains(p)),
        "the copy with no seal: no slice: {:?}",
        by_tape[2]
    );
    assert!(by_tape[3].contains(&t3.positions[1]), "{:?}", by_tape[3]);
    assert!(
        t3.positions
            .iter()
            .filter(|p| **p != t3.positions[1])
            .all(|p| !by_tape[3].contains(p)),
        "tape 3: slice 4 only: {:?}",
        by_tape[3]
    );
}

/// R5 for a second cartridge that does not require short-seal (a tape written
/// before 1.6.0): its seal is read first too, now that every second-tape read
/// learns where the tape ends. Sealed, it gives its slices as before; with a
/// seal that does not parse it gives none, says why, and the restore goes on
/// without them.
#[test]
fn swap_tapes_reads_a_1_5_0_second_cartridges_seal_and_takes_nothing_from_one_without() {
    let t = template();
    let photos = unit("photos/2019");
    let all: Vec<Vec<u8>> = (0..photos.positions.len())
        .map(|i| good_slice(t, &photos, i))
        .collect();
    for sealed in [true, false] {
        let h = Heir::new();
        flip_in_tape(&h, photos.positions[1]);
        second_tape(&h, "COPY15", "c0c0c0c0-0015-4000-8000-000000000015", &all);
        let d = h.dir.join("tape2");
        let text_of = |p: &Path| -> String {
            String::from_utf8(
                std::fs::read(p)
                    .unwrap()
                    .into_iter()
                    .filter(|b| *b != 0)
                    .collect(),
            )
            .unwrap()
        };
        let f0 = d.join("0");
        std::fs::write(&f0, padded(era_file0(Era::V150, text_of(&f0)).into_bytes())).unwrap();
        let seal_at = last_file(&d);
        let seal = d.join(seal_at.to_string());
        if sealed {
            std::fs::write(
                &seal,
                padded(era_seal(Era::V150, &text_of(&seal)).into_bytes()),
            )
            .unwrap();
        } else {
            garble(&d, seal_at);
        }
        let dest = h.sub("restored");
        let (code, text) = run_with(
            &h,
            &restore_args(&h, "photos/2019", &dest),
            &["--swap-tapes"],
            "\ndone\n",
            &[("FAKE_SWAP_ON_SETBLK", "2")],
        );
        assert!(
            text.contains("This cartridge identifies as: COPY15"),
            "{text}"
        );
        let swap = h.ops().iter().position(|l| l == "swap").unwrap();
        assert!(
            h.ops()[swap..].iter().any(|l| l == "mt eod"),
            "its seal is looked for at the end of its data: {:#?}",
            h.ops()
        );
        if sealed {
            assert_eq!(code, 0, "{text}");
            assert!(text.contains("RESTORE COMPLETE"), "{text}");
            assert!(same_tree(&photos.src, Path::new(&dest)), "{text}");
            assert!(!text.contains("Its seal marker cannot be read"), "{text}");
        } else {
            assert_eq!(code, 1, "{text}");
            assert!(
                text.contains("Its seal marker cannot be read, so nothing is taken from it"),
                "{text}"
            );
            assert!(text.contains("RESTORE INCOMPLETE"), "{text}");
            let second = reads_in(&h.ops()[swap..]);
            assert!(
                second.iter().all(|p| !(5..seal_at).contains(p)),
                "no slice of it is read: {second:?}"
            );
        }
    }
}

// ---- L2's residuals (#478 L3): a refusal relayed from an offered cartridge,
// a catalogue refused after a swap under --stream-tapes, a cartridge with no
// readable seal, and one taken through rung 2 ----

/// L2 residual (a): a cartridge offered for a unit held across tapes that this
/// script cannot read (a feature it does not know, another layout) is refused
/// in words that fit a restore that goes on: nothing is taken from it, load
/// another; never "use the RESTORE.sh stored on THIS tape", which would name
/// the cartridge just refused as the way on. The prompt asks again each time.
#[test]
fn a_refusal_relayed_from_an_offered_cartridge_says_the_restore_goes_on() {
    let b = span_box();
    let n = b.n;
    let big = b.big();
    let h = Heir::of_box(&b.of(&["SPAN01", "SPAN03"]), 0);
    let text_of = |p: &Path| -> String {
        String::from_utf8(
            std::fs::read(p)
                .unwrap()
                .into_iter()
                .filter(|b| *b != 0)
                .collect(),
        )
        .unwrap()
    };
    let f = copy_cart(&h, "SPAN03", "SPAN03F");
    let f0 = text_of(&f.join("0"));
    std::fs::write(
        f.join("0"),
        padded(
            with_requires(
                &f0,
                "requires = [\"short-seal\", \"spanning\", \"zz-future\"]",
            )
            .into_bytes(),
        ),
    )
    .unwrap();
    let l = copy_cart(&h, "SPAN03", "SPAN03L");
    assert_eq!(f0.matches("\nlayout_version = 2\n").count(), 1, "{f0}");
    std::fs::write(
        l.join("0"),
        padded(
            f0.replace("\nlayout_version = 2\n", "\nlayout_version = 3\n")
                .into_bytes(),
        ),
    )
    .unwrap();
    let offers = ["SPAN03F", "SPAN03L", "SPAN03"];
    let dest = h.sub("restored");
    let args = big_args(&h, &dest, &[]);
    let (code, text) = h.run_prompted(&strs(&args), |k, _| {
        assert!(k <= offers.len(), "asked too often");
        h.load(offers[k - 1]);
        Some(String::new())
    });
    assert_eq!(code, 0, "{text}");
    assert!(same_tree(&big.src, Path::new(&dest)), "{text}");
    assert_eq!(text.matches(SPAN_PROMPT).count(), 3, "{text}");
    let flat_text = flat(&text);
    for needle in [
        "NOTE: this tape requires feature(s) zz-future, which this script does not know",
        "This script cannot read that cartridge safely and will not guess, so nothing is taken from it, and the restore goes on: load another that holds the slice.",
        "NOTE: this tape is layout_version 3; this script reads layout v2 only, so nothing is taken from it, and the restore goes on: load another that holds the slice.",
        "To read that cartridge on its own, use the RESTORE.sh stored on it, at file 2",
    ] {
        assert!(flat_text.contains(needle), "{needle:?}:\n{text}");
    }
    assert_eq!(
        text.matches(
            "Nothing is taken from this cartridge: its file map cannot be used (see above)."
        )
        .count(),
        2,
        "{text}"
    );
    for word in [
        "Use the RESTORE.sh stored on THIS tape",
        "cannot read it safely",
        "FATAL",
    ] {
        assert!(!text.contains(word), "{word:?}:\n{text}");
    }
    assert!(
        text.contains(&format!("It gives slice(s) 3 to {n} of 'big'")),
        "{text}"
    );
    // The positive control: the same tape on its own is refused the usual way.
    let h = Heir::of_box(&b.of(&["SPAN01"]), 0);
    let g = copy_cart(&h, "SPAN01", "SPAN01F");
    let g0 = text_of(&g.join("0"));
    std::fs::write(
        g.join("0"),
        padded(
            with_requires(
                &g0,
                "requires = [\"short-seal\", \"spanning\", \"zz-future\"]",
            )
            .into_bytes(),
        ),
    )
    .unwrap();
    h.load("SPAN01F");
    let (code, text) = h.run(&["--info"]);
    assert_eq!(code, 1, "{text}");
    assert!(
        text.contains("Use the RESTORE.sh stored on THIS tape"),
        "{text}"
    );
}

/// dar -x with the envelope's catalogue (-A): it reads slices 1 to 3 off
/// their pipes and then refuses the catalogue, as dar refuses a re-staged
/// snapshot's (#419), so the refusal comes after the cartridge change slice 3
/// needs. Every other run is the host's dar.
const DAR_REFUSES_AFTER_SLICE_3: &str = r#"#!/usr/bin/env bash
case " $* " in
  *" -x "*" -A "*)
    for n in 1 2 3; do cat "$2.$n.dar" >/dev/null; done
    echo "Error: the archive and the isolated catalogue do not correspond to the same data"
    exit 1 ;;
esac
exec "$FAKE_TAPE/hostbin/dar" "$@"
"#;

/// L2 residual (b): with `--stream-tapes`, a catalogue dar refuses (#419)
/// after a cartridge change ends the run with the #419 text and the way to
/// restore the unit; it never asks for slice 1 again, which is on a cartridge
/// already taken out. Nothing is restored.
#[test]
fn a_catalogue_refused_after_a_swap_ends_a_streamed_restore_with_the_419_text() {
    let b = span_box();
    let h = Heir::of_box(&b.of(&["SPAN01", "SPAN03"]), 0);
    h.stub("dar", DAR_REFUSES_AFTER_SLICE_3);
    let dest = h.sub("restored");
    let args = big_args(&h, &dest, &["--stream-tapes"]);
    let (code, text) = h.run_prompted(&strs(&args), |k, text| {
        assert_eq!(k, 1, "one cartridge change, for slice 3:\n{text}");
        h.load("SPAN03");
        Some(String::new())
    });
    assert_eq!(code, 1, "{text}");
    let fatal = flat(text.split("FATAL:").nth(1).expect("a FATAL"));
    for needle in [
        "dar refused the envelope's catalogue for 'big': it belongs to another staging run of the same snapshot (#419). Nothing was written.",
        "would start again from slice 1, on a cartridge this run has taken out of the drive, so the run ends here.",
        "Run the same command again without --stream-tapes",
    ] {
        assert!(fatal.contains(needle), "{needle:?}:\n{fatal}");
    }
    assert!(text.contains("needs slice 3 of"), "{text}");
    assert!(!text.contains("needs slice 1 of"), "{text}");
    assert!(
        !text.contains("restoring\n      'big' without it"),
        "{text}"
    );
    assert!(rel_files(Path::new(&dest)).is_empty(), "nothing restored");
}

/// L2 residual (c), R8: a cartridge whose seal marker cannot be read gives
/// nothing (ADR-0012 item 132), and says why, and the prompt asks again; a
/// cartridge whose front index (File 3) cannot be read is read by rung 2, the
/// seal's copy of the front index, and is taken by it. Tape 1's ciphertext
/// waits in scratch all the while.
#[test]
fn a_cartridge_with_no_readable_seal_is_refused_and_one_read_by_rung_2_is_taken() {
    let b = span_box();
    let n = b.n;
    let big = b.big();
    let h = Heir::of_box(&b.of(&["SPAN01", "SPAN03"]), 0);
    let s = copy_cart(&h, "SPAN03", "SPAN03S");
    garble(&s, last_file(&s));
    let r = copy_cart(&h, "SPAN03", "SPAN03R");
    garble(&r, 3);
    let offers = ["SPAN03S", "SPAN03R"];
    let dest = h.sub("restored");
    let args = big_args(&h, &dest, &[]);
    let (code, text) = h.run_prompted(&strs(&args), |k, _| {
        assert!(k <= offers.len(), "asked too often");
        assert_scratch_holds(&dest, b, 2, &format!("prompt {k}"));
        h.load(offers[k - 1]);
        Some(String::new())
    });
    assert_eq!(code, 0, "{text}");
    assert!(same_tree(&big.src, Path::new(&dest)), "{text}");
    assert_eq!(text.matches(SPAN_PROMPT).count(), 2, "{text}");
    for needle in [
        "WARNING: this tape requires short-seal, and no valid seal marker was found as its last file",
        "Its seal marker cannot be read, so nothing is taken from it: a cartridge gives a slice only at a",
        "WARNING: the front index of this cartridge is unreadable: using the seal marker's copy of it.",
    ] {
        assert!(text.contains(needle), "{needle:?}:\n{text}");
    }
    assert_eq!(
        text.matches(&format!("It gives slice(s) 3 to {n} of 'big'"))
            .count(),
        1,
        "{text}"
    );
    let t3 = unit_of(b.t("SPAN03"), "big");
    let by_tape = reads_by_cartridge(&h);
    assert_eq!(by_tape.len(), 3, "{:#?}", h.ops());
    assert!(
        by_tape[1].iter().all(|p| !t3.positions.contains(p)),
        "the cartridge with no seal: no slice: {:?}",
        by_tape[1]
    );
    for p in &t3.positions {
        assert!(
            by_tape[2].contains(p),
            "rung 2's cartridge: {:?}",
            by_tape[2]
        );
    }
}
