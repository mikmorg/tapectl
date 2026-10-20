//! The 1.6.0 acceptance run's synthetic filler, and its rebuild comparison
//! (issue #420; ADR-0012 items 110, 115 and 117). A test fixture: no
//! production command exposes it, and `scripts/short-seal-acceptance.sh` is
//! its only caller.
//!
//! Why it is an example: `cargo test` and `cargo clippy --all-targets` build
//! every example, so this keeps compiling against the crate's own `rusqlite`
//! (bundled SQLite, the catalog's own engine) while no `tapectl` subcommand
//! carries it. `gen_man.rs` is the precedent. For home2 it is built the way
//! the tapectl binary under test is (`cargo build --example
//! short_seal_filler`, statically for musl there) and named to the script as
//! `FIXTURE_BIN`.
//!
//! ## `fill`: one real slice, many rows
//!
//! home2's staging area is smaller than one cartridge, so the run cannot
//! stage 2.5 TB (item 115). Instead the filler unit is staged NORMALLY by
//! `tapectl stage create` from a source a little larger than one slice, so its
//! stage set is real in every respect (unit, snapshot, file rows, dar
//! catalogue, recipients, escrow receipt) and its slice 1 is a full dar slice,
//! age-encrypted to the set's recipients. `fill` then gives that set N slice
//! rows: slice 1 and N-1 copies of it, every one naming the same staged file
//! with the same sizes and hashes, and the set's own later slices renumbered
//! after them. Nothing else in the catalog changes.
//!
//! That passes the checks between staging and the tape without a 2.6 TB
//! staging area:
//! - `volume write`'s whole-set check (`staging::staged_slice_problems`)
//!   compares the row count with `num_slices` (set here) and each row's file
//!   size with its `encrypted_bytes` (the one file, the same figure);
//! - `Layout::validate` under the default `SliceCheck::Size` reads metadata
//!   only (the run never passes `--prewrite-hash`, which would read the file
//!   N times before the tape moves);
//! - `UNIQUE(stage_set_id, slice_number)` constrains numbers, not paths;
//! - the write hashes every slice as it streams it and compares the
//!   recorded `sha256_encrypted`, which every row shares; the front index
//!   lists N files with one hash, which nothing forbids;
//! - `stage create` never sees the filler's size: the script stages every
//!   real unit before `fill` runs.
//!
//! The filler is cut by the early warning and never restored: N copies of
//! one dar slice are no archive. `staging clean --unit <filler> --force`
//! releases it afterwards (a missing path is skipped, so N rows naming one
//! file are one unlink).
//!
//! `--move-to DIR` moves the shared file (copied, checked, then the original
//! removed) to DIR first: on home2 a tmpfs, because the write reads every
//! staged slice through `util::DropBehind`, which evicts it, so N rows of one
//! file on a disk are N cold reads of it.
//!
//! It refuses a catalog that has ever written a tape (any write row, any
//! volume not `initialized`): it is for a throwaway home, never production's.
//!
//! ## `compare`: a rebuilt catalog against the writer's
//!
//! Step 6 of the run rebuilds the catalog from the short tape into a second
//! home (`catalog rebuild --from-volume`). `compare` checks the rows a
//! rebuild must reproduce for that volume: the volume (uuid, status,
//! `ended_after`), each unit on it (name, uuid, tenant), each write's status
//! (`completed` or `cut`, by unit and version), and the slices on the tape
//! (unit, version, slice number, tape position, for every position recorded
//! written or verified). Stage-set slice counts and origins are printed, not
//! judged: a rebuild records what the tape says, which for a cut set is the
//! slices that reached it. Exit 0 when they agree, 1 when any differs.
//!
//! Usage:
//!
//! ```text
//! short_seal_filler fill --home DIR --unit NAME (--slices N | --plan-end BYTES)
//!                   [--allowance BYTES] [--block-size BYTES] [--move-to DIR] [--dry-run]
//! short_seal_filler compare --home DIR --rebuilt DIR --label LABEL
//! ```
//!
//! `fill` prints `KEY=VALUE` lines on stdout for the script, and its account
//! on stderr.

use std::collections::BTreeSet;
use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use rusqlite::{params, Connection, OpenFlags, OptionalExtension};
use sha2::{Digest, Sha256};

/// tapectl's fixed block (`store::TapeStore`): every file on tape is padded
/// to it.
const DEFAULT_BLOCK: u64 = 512 * 1024;

/// What the plan's fixed files take (Files 0-3, the envelopes and their
/// backups, the seal) when `--plan-end` sizes the filler and no
/// `--allowance` is given.
const DEFAULT_ALLOWANCE: u64 = 64 * 1024 * 1024;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let result = match args.first().map(String::as_str) {
        Some("fill") => fill(&args[1..]),
        Some("compare") => compare(&args[1..]),
        _ => Err(anyhow!(
            "usage: short_seal_filler fill --home DIR --unit NAME (--slices N | --plan-end BYTES) \
             [--allowance BYTES] [--block-size BYTES] [--move-to DIR] [--dry-run]\n       \
             short_seal_filler compare --home DIR --rebuilt DIR --label LABEL"
        )),
    };
    match result {
        Ok(code) => std::process::exit(code),
        Err(e) => {
            eprintln!("short_seal_filler: {e:#}");
            std::process::exit(2);
        }
    }
}

/// `--flag VALUE` pairs and bare `--flag`s, in any order.
struct Args {
    pairs: Vec<(String, Option<String>)>,
}

impl Args {
    fn parse(raw: &[String], flags: &[&str], switches: &[&str]) -> Result<Self> {
        let mut pairs = Vec::new();
        let mut it = raw.iter();
        while let Some(a) = it.next() {
            if switches.contains(&a.as_str()) {
                pairs.push((a.clone(), None));
            } else if flags.contains(&a.as_str()) {
                let v = it.next().ok_or_else(|| anyhow!("{a} needs a value"))?;
                pairs.push((a.clone(), Some(v.clone())));
            } else {
                bail!("unknown argument {a:?}");
            }
        }
        Ok(Args { pairs })
    }

    fn get(&self, flag: &str) -> Option<&str> {
        self.pairs
            .iter()
            .rev()
            .find(|(f, _)| f == flag)
            .and_then(|(_, v)| v.as_deref())
    }

    fn need(&self, flag: &str) -> Result<&str> {
        self.get(flag).ok_or_else(|| anyhow!("{flag} is required"))
    }

    fn has(&self, flag: &str) -> bool {
        self.pairs.iter().any(|(f, _)| f == flag)
    }

    fn number(&self, flag: &str) -> Result<Option<u64>> {
        self.get(flag)
            .map(|v| {
                v.parse::<u64>()
                    .with_context(|| format!("{flag} {v:?} is not a plain byte count"))
            })
            .transpose()
    }
}

fn open_catalog(home: &Path, flags: OpenFlags) -> Result<Connection> {
    let db = home.join("tapectl.db");
    if !db.is_file() {
        bail!("{} is not a tapectl catalog (no such file)", db.display());
    }
    let conn = Connection::open_with_flags(&db, flags)
        .with_context(|| format!("opening {}", db.display()))?;
    conn.busy_timeout(Duration::from_secs(30))?;
    Ok(conn)
}

fn padded(bytes: u64, block: u64) -> u64 {
    bytes.div_ceil(block) * block
}

fn sha256_file(path: &Path) -> Result<String> {
    let mut f = fs::File::open(path).with_context(|| format!("opening {}", path.display()))?;
    let mut h = Sha256::new();
    let mut buf = vec![0u8; 4 * 1024 * 1024];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
    }
    Ok(h.finalize().iter().map(|b| format!("{b:02x}")).collect())
}

/// One `stage_slices` row of the filler's set.
struct SliceRow {
    id: i64,
    number: i64,
    size_bytes: i64,
    encrypted_bytes: i64,
    sha256_plain: String,
    sha256_encrypted: String,
    staging_path: Option<String>,
}

fn fill(raw: &[String]) -> Result<i32> {
    let args = Args::parse(
        raw,
        &[
            "--home",
            "--unit",
            "--slices",
            "--plan-end",
            "--allowance",
            "--block-size",
            "--move-to",
        ],
        &["--dry-run"],
    )?;
    let home = PathBuf::from(args.need("--home")?);
    let unit = args.need("--unit")?.to_string();
    let slices = args.number("--slices")?;
    let plan_end = args.number("--plan-end")?;
    let allowance = args.number("--allowance")?.unwrap_or(DEFAULT_ALLOWANCE);
    let block = args.number("--block-size")?.unwrap_or(DEFAULT_BLOCK);
    let dry_run = args.has("--dry-run");
    let move_to = args.get("--move-to").map(PathBuf::from);
    if slices.is_some() == plan_end.is_some() {
        bail!("give exactly one of --slices N and --plan-end BYTES");
    }
    if block == 0 {
        bail!("--block-size must be positive");
    }

    let mut conn = open_catalog(&home, OpenFlags::SQLITE_OPEN_READ_WRITE)?;

    // A throwaway home only: production's catalog has written tapes.
    let writes: i64 = conn.query_row("SELECT COUNT(*) FROM writes", [], |r| r.get(0))?;
    let used: i64 = conn.query_row(
        "SELECT COUNT(*) FROM volumes WHERE status <> 'initialized'",
        [],
        |r| r.get(0),
    )?;
    if writes > 0 || used > 0 {
        bail!(
            "refusing: this catalog has written a tape ({writes} write row(s), {used} volume(s) \
             past 'initialized'). The filler is for a fresh throwaway home, never a catalog \
             anything depends on"
        );
    }

    let sets: Vec<(i64, i64, Option<i64>)> = conn
        .prepare(
            "SELECT ss.id, ss.slice_size, ss.num_slices
             FROM stage_sets ss
             JOIN snapshots s ON s.id = ss.snapshot_id
             JOIN units u ON u.id = s.unit_id
             WHERE u.name = ?1 AND ss.status = 'staged'",
        )?
        .query_map(params![unit], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
        .collect::<std::result::Result<_, _>>()?;
    let (set_id, slice_size, _num_slices) = match sets.as_slice() {
        [one] => *one,
        [] => {
            bail!("unit {unit:?} has no staged stage set: run `tapectl stage create {unit}` first")
        }
        _ => bail!(
            "unit {unit:?} has {} staged stage sets; the filler needs exactly one",
            sets.len()
        ),
    };

    let rows: Vec<SliceRow> = conn
        .prepare(
            "SELECT id, slice_number, size_bytes, encrypted_bytes, sha256_plain,
                    sha256_encrypted, staging_path
             FROM stage_slices WHERE stage_set_id = ?1 ORDER BY slice_number",
        )?
        .query_map(params![set_id], |r| {
            Ok(SliceRow {
                id: r.get(0)?,
                number: r.get(1)?,
                size_bytes: r.get(2)?,
                encrypted_bytes: r.get(3)?,
                sha256_plain: r.get(4)?,
                sha256_encrypted: r.get(5)?,
                staging_path: r.get(6)?,
            })
        })?
        .collect::<std::result::Result<_, _>>()?;
    let Some(template) = rows.first() else {
        bail!("stage set {set_id} of {unit:?} has no slice rows");
    };
    if template.number != 1 {
        bail!(
            "stage set {set_id}'s first slice is number {}, not 1",
            template.number
        );
    }
    if template.size_bytes != slice_size {
        bail!(
            "slice 1 of {unit:?} is {} bytes of dar archive, not the set's full slice size \
             {slice_size}: stage the filler from a source larger than one slice, so its slice 1 \
             is a full slice like every slice but the last of a real unit",
            template.size_bytes
        );
    }
    let src = PathBuf::from(
        template
            .staging_path
            .as_deref()
            .ok_or_else(|| anyhow!("slice 1 of {unit:?} records no staged file"))?,
    );
    let meta = fs::metadata(&src).with_context(|| format!("reading {}", src.display()))?;
    if meta.len() != template.encrypted_bytes as u64 {
        bail!(
            "{} is {} bytes, {} recorded",
            src.display(),
            meta.len(),
            template.encrypted_bytes
        );
    }
    eprintln!("hashing the template slice {} ...", src.display());
    let hash = sha256_file(&src)?;
    if !hash.eq_ignore_ascii_case(&template.sha256_encrypted) {
        bail!(
            "{} hashes {hash}, but the catalog recorded {}: the staged file is not the slice",
            src.display(),
            template.sha256_encrypted
        );
    }
    let tail: &[SliceRow] = &rows[1..];
    for t in tail {
        if t.staging_path.is_none() {
            bail!("slice {} of {unit:?} records no staged file", t.number);
        }
    }

    // Every other staged set's slices, as the write would plan them, split
    // into the units sorting before the filler and after it (`volume
    // write` orders the batch by unit name).
    let others: Vec<(String, i64)> = conn
        .prepare(
            "SELECT u.name, sl.encrypted_bytes
             FROM stage_slices sl
             JOIN stage_sets ss ON ss.id = sl.stage_set_id
             JOIN snapshots s ON s.id = ss.snapshot_id
             JOIN units u ON u.id = s.unit_id
             WHERE ss.status = 'staged' AND ss.id <> ?1 AND sl.staging_path IS NOT NULL",
        )?
        .query_map(params![set_id], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect::<std::result::Result<_, _>>()?;
    let (mut before, mut after) = (0u64, 0u64);
    for (name, bytes) in &others {
        let p = padded(*bytes as u64, block);
        if name.as_str() < unit.as_str() {
            before += p;
        } else {
            after += p;
        }
    }
    let t_padded = padded(template.encrypted_bytes as u64, block);
    let tail_padded: u64 = tail
        .iter()
        .map(|t| padded(t.encrypted_bytes as u64, block))
        .sum();

    let copies = match (slices, plan_end) {
        (Some(n), None) => {
            let n = n as usize;
            if n < rows.len() {
                bail!(
                    "--slices {n} is fewer than the {} the set already has",
                    rows.len()
                );
            }
            (n - tail.len()) as u64
        }
        (None, Some(end)) => {
            let fixed = allowance + before + after + tail_padded;
            if end <= fixed {
                bail!("--plan-end {end} leaves no room: the rest of the plan takes {fixed}");
            }
            let n = (end - fixed) / t_padded;
            if n < 1 {
                bail!("--plan-end {end} leaves room for no full slice ({t_padded} each)");
            }
            n
        }
        _ => unreachable!("checked above"),
    };
    let total = copies as usize + tail.len();
    let filler_bytes = copies * t_padded + tail_padded;
    let plan_estimate = allowance + before + filler_bytes + after;

    // The figures, before anything changes.
    println!("FILLER_UNIT={unit}");
    println!("FILLER_STAGE_SET={set_id}");
    println!("FILLER_SLICES={total}");
    println!("TEMPLATE_ENCRYPTED_BYTES={}", template.encrypted_bytes);
    println!("TEMPLATE_PADDED_BYTES={t_padded}");
    println!("FILLER_PADDED_BYTES={filler_bytes}");
    println!("UNITS_BEFORE_PADDED_BYTES={before}");
    println!("UNITS_AFTER_PADDED_BYTES={after}");
    println!("ALLOWANCE_BYTES={allowance}");
    println!("PLAN_END_ESTIMATE_BYTES={plan_estimate}");
    eprintln!(
        "filler {unit:?} (stage set {set_id}): {total} slices ({copies} rows of slice 1, {} of its \
         own after them), {filler_bytes} bytes on tape; the plan ends near {plan_estimate} bytes",
        tail.len()
    );
    if dry_run {
        eprintln!("--dry-run: nothing changed");
        return Ok(0);
    }

    // Move the shared file first, so every row the transaction writes names
    // where it will be read from. The original goes only after the commit.
    let shared = match &move_to {
        None => src.clone(),
        Some(dir) => {
            let name = src
                .file_name()
                .ok_or_else(|| anyhow!("{} has no file name", src.display()))?;
            let dest = dir.join(name);
            if dest.exists() {
                bail!("{} already exists", dest.display());
            }
            eprintln!("copying {} to {} ...", src.display(), dest.display());
            fs::create_dir_all(dir)?;
            {
                let mut from = fs::File::open(&src)?;
                let mut to = fs::File::create(&dest)?;
                std::io::copy(&mut from, &mut to)?;
                to.flush()?;
                to.sync_all()?;
            }
            let copied = sha256_file(&dest)?;
            if copied != hash {
                let _ = fs::remove_file(&dest);
                bail!("the copy at {} hashes {copied}, not {hash}", dest.display());
            }
            dest
        }
    };
    let shared_text = shared
        .to_str()
        .ok_or_else(|| anyhow!("{} is not UTF-8", shared.display()))?
        .to_string();

    let tx = conn.transaction()?;
    // The set's own later slices move past the copies, highest first so no
    // number is ever held twice.
    let shift = copies as i64 - 1;
    for t in tail.iter().rev() {
        tx.execute(
            "UPDATE stage_slices SET slice_number = ?1 WHERE id = ?2",
            params![t.number + shift, t.id],
        )?;
    }
    tx.execute(
        "UPDATE stage_slices SET staging_path = ?1 WHERE id = ?2",
        params![shared_text, template.id],
    )?;
    for n in 2..=copies as i64 {
        tx.execute(
            "INSERT INTO stage_slices (stage_set_id, slice_number, size_bytes, encrypted_bytes,
                                       sha256_plain, sha256_encrypted, staging_path)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                set_id,
                n,
                template.size_bytes,
                template.encrypted_bytes,
                template.sha256_plain,
                template.sha256_encrypted,
                shared_text,
            ],
        )?;
    }
    let note = format!(
        "short-seal acceptance filler (ADR-0012 item 115): slices 1-{copies} are one staged file, \
         {shared_text}"
    );
    tx.execute(
        "UPDATE stage_sets
         SET num_slices = (SELECT COUNT(*) FROM stage_slices WHERE stage_set_id = ?1),
             total_dar_size = (SELECT SUM(size_bytes) FROM stage_slices WHERE stage_set_id = ?1),
             total_encrypted_size =
                 (SELECT SUM(encrypted_bytes) FROM stage_slices WHERE stage_set_id = ?1),
             notes = CASE WHEN notes IS NULL OR notes = '' THEN ?2 ELSE notes || '; ' || ?2 END
         WHERE id = ?1",
        params![set_id, note],
    )?;
    let (count, enc): (i64, i64) = tx.query_row(
        "SELECT num_slices, total_encrypted_size FROM stage_sets WHERE id = ?1",
        params![set_id],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    if count as usize != total {
        bail!("the set reads {count} slices after the change, not {total}; rolled back");
    }
    tx.commit()?;
    if shared != src {
        fs::remove_file(&src).with_context(|| format!("removing {}", src.display()))?;
    }
    println!("FILLER_FILE={shared_text}");
    eprintln!(
        "stage set {set_id} now has {count} slices, {enc} bytes of ciphertext, slices 1-{copies} \
         reading {shared_text}"
    );
    Ok(0)
}

/// The rows of one catalog a rebuild must reproduce for `label`.
#[derive(Debug, Default)]
struct VolumeRows {
    volume: Option<(Option<String>, String, Option<i64>)>,
    units: BTreeSet<(String, String, String)>,
    writes: BTreeSet<(String, i64, String)>,
    on_tape: BTreeSet<(String, i64, i64, String)>,
    sets: BTreeSet<(String, i64, Option<i64>, Option<String>)>,
}

fn volume_rows(conn: &Connection, label: &str) -> Result<VolumeRows> {
    let mut out = VolumeRows {
        volume: conn
            .query_row(
                "SELECT uuid, status, ended_after FROM volumes WHERE label = ?1",
                params![label],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .optional()?,
        ..Default::default()
    };
    let mut stmt = conn.prepare(
        "SELECT u.name, u.uuid, t.name, s.version, w.status, ss.num_slices, ss.origin
         FROM writes w
         JOIN volumes v ON v.id = w.volume_id
         JOIN stage_sets ss ON ss.id = w.stage_set_id
         JOIN snapshots s ON s.id = w.snapshot_id
         JOIN units u ON u.id = s.unit_id
         JOIN tenants t ON t.id = u.tenant_id
         WHERE v.label = ?1",
    )?;
    let rows = stmt.query_map(params![label], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, String>(2)?,
            r.get::<_, i64>(3)?,
            r.get::<_, String>(4)?,
            r.get::<_, Option<i64>>(5)?,
            r.get::<_, Option<String>>(6)?,
        ))
    })?;
    for row in rows {
        let (name, uuid, tenant, version, status, num_slices, origin) = row?;
        out.units.insert((name.clone(), uuid, tenant));
        out.writes.insert((name.clone(), version, status));
        out.sets.insert((name, version, num_slices, origin));
    }
    let mut stmt = conn.prepare(
        "SELECT u.name, s.version, sl.slice_number, wp.position
         FROM write_positions wp
         JOIN writes w ON w.id = wp.write_id
         JOIN volumes v ON v.id = w.volume_id
         JOIN stage_slices sl ON sl.id = wp.stage_slice_id
         JOIN snapshots s ON s.id = w.snapshot_id
         JOIN units u ON u.id = s.unit_id
         WHERE v.label = ?1 AND wp.status IN ('written', 'verified')",
    )?;
    let rows = stmt.query_map(params![label], |r| {
        Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))
    })?;
    for row in rows {
        out.on_tape.insert(row?);
    }
    Ok(out)
}

/// Print what one side has and the other lacks; true when they agree.
fn same<T: Ord + std::fmt::Debug>(what: &str, a: &BTreeSet<T>, b: &BTreeSet<T>) -> bool {
    let only_a: Vec<&T> = a.difference(b).collect();
    let only_b: Vec<&T> = b.difference(a).collect();
    if only_a.is_empty() && only_b.is_empty() {
        println!("AGREE  {what}: {} row(s)", a.len());
        return true;
    }
    println!(
        "DIFFER {what}: {} in the writer's catalog only, {} in the rebuilt one only",
        only_a.len(),
        only_b.len()
    );
    for r in only_a.iter().take(20) {
        println!("  writer only:  {r:?}");
    }
    for r in only_b.iter().take(20) {
        println!("  rebuilt only: {r:?}");
    }
    false
}

fn compare(raw: &[String]) -> Result<i32> {
    let args = Args::parse(raw, &["--home", "--rebuilt", "--label"], &[])?;
    let label = args.need("--label")?;
    let ro = OpenFlags::SQLITE_OPEN_READ_ONLY;
    let a = volume_rows(&open_catalog(Path::new(args.need("--home")?), ro)?, label)?;
    let b = volume_rows(
        &open_catalog(Path::new(args.need("--rebuilt")?), ro)?,
        label,
    )?;

    let mut ok = true;
    match (&a.volume, &b.volume) {
        (Some(x), Some(y)) if x == y => println!(
            "AGREE  volume {label}: uuid {}, status {}, ended_after {}",
            x.0.as_deref().unwrap_or("-"),
            x.1,
            x.2.map_or("NULL".to_string(), |k| k.to_string())
        ),
        (x, y) => {
            ok = false;
            println!("DIFFER volume {label}: writer {x:?}, rebuilt {y:?}");
        }
    }
    ok &= same(
        "units on the volume (name, uuid, tenant)",
        &a.units,
        &b.units,
    );
    ok &= same("writes (unit, version, status)", &a.writes, &b.writes);
    ok &= same(
        "slices on the tape (unit, version, slice, position)",
        &a.on_tape,
        &b.on_tape,
    );
    println!("stage sets, not judged (unit, version, num_slices, origin):");
    for (name, rows) in [("writer", &a.sets), ("rebuilt", &b.sets)] {
        for r in rows {
            println!("  {name}: {r:?}");
        }
    }
    println!(
        "{}",
        if ok {
            "COMPARE: the rebuilt catalog agrees with the writer's"
        } else {
            "COMPARE: the rebuilt catalog DIFFERS from the writer's"
        }
    );
    Ok(if ok { 0 } else { 1 })
}
