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
/// something else had moved the tape.
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

/// One unit as staged: its files, and the dar slices built from them.
struct UnitSpec {
    name: &'static str,
    uuid: &'static str,
    version: i64,
    seed: u64,
    files: usize,
    file_len: usize,
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
                        uuid: "11111111-2222-3333-4444-555555555555",
                        version: 2,
                        seed: 1,
                        files: 5,
                        file_len: 150_000,
                    },
                    UnitSpec {
                        name: "docs",
                        uuid: "22222222-3333-4444-5555-666666666666",
                        version: 1,
                        seed: 2,
                        files: 2,
                        file_len: 140_000,
                    },
                ],
            },
            TenantSpec {
                name: "bob",
                units: vec![UnitSpec {
                    name: "ledgers",
                    uuid: "33333333-4444-5555-6666-777777777777",
                    version: 3,
                    seed: 3,
                    files: 2,
                    file_len: 130_000,
                }],
            },
        ];
        build_template(&tenants)
    })
}

fn build_template(tenants: &[TenantSpec]) -> Template {
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR"))
        .join(format!("heir-restore-sh-template-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    for sub in ["tape", "src", "keys", "work"] {
        std::fs::create_dir_all(dir.join(sub)).unwrap();
    }
    for k in ["alice", "bob", "op", "esc", "stranger"] {
        std::fs::write(dir.join("keys").join(format!("{k}.key")), k).unwrap();
    }

    // dar every unit: its slices (plaintext) and its isolated catalogue.
    struct Staged {
        tenant: &'static str,
        unit: ManifestUnit,
        slices: Vec<Vec<u8>>,
        catalogue: Vec<u8>,
        src: PathBuf,
    }
    let mut staged: Vec<Staged> = Vec::new();
    for t in tenants {
        for u in &t.units {
            let src = dir.join("src").join(u.name);
            std::fs::create_dir_all(&src).unwrap();
            for i in 0..u.files {
                std::fs::write(
                    src.join(format!("file{i}.bin")),
                    content(u.seed * 100 + i as u64, u.file_len),
                )
                .unwrap();
            }
            std::fs::create_dir_all(src.join("sub")).unwrap();
            std::fs::write(
                src.join("sub").join("note.txt"),
                format!("unit {}\n", u.name),
            )
            .unwrap();
            let work = dir.join("work").join(u.uuid);
            std::fs::create_dir_all(&work).unwrap();
            let base = work.join("arch");
            run_dar(&[
                "-c",
                base.to_str().unwrap(),
                "-R",
                src.to_str().unwrap(),
                "-s",
                "200k",
                "-Q",
                "-N",
            ]);
            let cat = work.join(format!("{}_v{}", &u.uuid[..8], u.version));
            run_dar(&[
                "-C",
                cat.to_str().unwrap(),
                "-A",
                base.to_str().unwrap(),
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
            let catalogue =
                std::fs::read(work.join(format!("{}_v{}.1.dar", &u.uuid[..8], u.version))).unwrap();
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
            });
        }
    }

    // Positions: 0-3, a tenant envelope each, the operator envelope and its
    // backup, every unit's slices contiguously, the seal marker last.
    let first_env = 4i64;
    let op_pos = first_env + tenants.len() as i64;
    let mut next = op_pos + 2;
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
        let recovery = generate_recovery_md(LABEL, tenant, &units);
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
            add(
                &format!(
                    "catalogs/{}_v{}.1.dar",
                    &s.unit.uuid[..8],
                    s.unit.snapshot_version
                ),
                &s.catalogue,
            );
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
        files.push((
            first_env + i as i64,
            "tenant_envelope",
            envelope(t.name, &format!("{},op,esc", t.name), &mine),
        ));
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
        let t = template();
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
        std::fs::write(self.dir.join("ops"), "").unwrap();
        let mut cmd = Command::new("timeout");
        cmd.args(["-k", "10", "300", "bash"])
            .arg(self.dir.join("RESTORE.sh"))
            .args(args)
            .current_dir(&self.dir)
            .env("PATH", &self.path)
            .env("FAKE_TAPE", &self.dir)
            .env("TAPE_DEVICE", "/dev/nst-fake")
            .env("TMPDIR", self.dir.join("tmp"))
            .env("HOME", self.dir.join("home"));
        for (k, v) in env {
            cmd.env(k, v);
        }
        let o = cmd.output().expect("spawn RESTORE.sh");
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

/// A damaged envelope is not a wrong key. The key matched (age got past the
/// header) but the payload did not decrypt: the script says DAMAGED, and the
/// headline no longer tells the heir to try their OTHER keys.
#[test]
fn a_damaged_envelope_is_reported_as_damage_not_as_a_wrong_key() {
    let h = Heir::new();
    damage_envelope(&h, 4); // alice's tenant envelope
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

    // Control: a key that matches nothing still gets the rotation hint.
    let h = Heir::new();
    let (code, text) = h.run(&["--find-envelope", "--key", &h.key("stranger")]);
    assert_ne!(code, 0, "{text}");
    assert!(text.contains("try your OTHER keys"), "{text}");
    assert!(!text.contains("DAMAGED"), "{text}");
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

/// A 1.1.0 tape says what it requires, and requires nothing.
#[test]
fn a_new_tape_states_an_empty_requires_in_file_0_and_the_seal() {
    let t = template();
    let f0 = std::fs::read_to_string(t.dir.join("tape/0")).unwrap();
    let seal = std::fs::read_to_string(t.dir.join(format!("tape/{}", t.total_files - 1))).unwrap();
    assert!(f0.contains("\nrequires = []\n"), "File 0");
    assert!(seal.contains("\nrequires = []\n"), "seal marker");
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
