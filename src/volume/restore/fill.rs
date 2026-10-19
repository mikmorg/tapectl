//! What is left of a restore after its slices are read and dar has run, and
//! how another copy of the same version fills it (ADR-0012 items 101 and
//! 102, issue #480).
//!
//! # What was lost
//!
//! A restore that read every slice whole and whose dar exited 0 lost
//! nothing, and costs nothing here. Otherwise three things name the loss,
//! and this module takes all of them:
//!
//! - the files dar said it could not restore intact (`Error while restoring
//!   X : Bad CRC...`, exit 5, which dar prints for a file a hole fell in and
//!   goes on past, measured on 2.7.13);
//! - the files the catalog records for this version
//!   (`file_versions`, which staging fills with every file's sha256) that
//!   are not at the destination at all, because dar stopped, or could not
//!   read the archive around a hole;
//! - the holes themselves, which say the loss was not of the restore's
//!   making.
//!
//! # The fill
//!
//! Two copies of one version may come from different stagings: different
//! stage sets, so different slices and different slice hashes, which
//! slice substitution cannot match, while the files are the same, because
//! staging records every file's sha256 for the version and refuses a
//! re-stage whose hashes disagree. So the lost files are restored from
//! another copy of the SAME unit and the SAME version, and only those paths,
//! reading only the slices that hold them through that stage set's own dar
//! catalogue. Each file taken is checked against the version's recorded
//! sha256 and kept only if it matches: a file that does not is said so and
//! not kept. Other versions are never a source, even where a file is
//! unchanged.
//!
//! The cartridge is asked for as in substitution (by label and chip serial;
//! with no terminal, refused by name, listing the holders), and the copies
//! are tried in turn until the unit is whole or none is left.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::io::Read;

use sha2::Digest;

use super::supply::{Changed, Damage, SliceSink, Supply};
use super::*;
use crate::db::files::FileKind;
use crate::pass_count::Sha256;
use crate::volume::slice_holders::{self, StageCopy, StageHolder, Standing};

/// What the extract left, handed to [`finish`] with the scratch directory
/// still in place.
pub(super) struct Extracted {
    /// Where dar wrote: the destination for a unit, the extract directory
    /// for one file.
    pub root: PathBuf,
    /// The files dar reported it could not restore intact, as dar printed
    /// them, with its reason.
    pub damaged: Vec<(String, String)>,
    /// dar failing other than by reporting files — typically the archive it
    /// could not read around a hole.
    pub dar_failure: Option<TapectlError>,
    /// The slices this restore had to write holes into.
    pub holes: Vec<Damage>,
}

impl Extracted {
    fn is_clean(&self) -> bool {
        self.damaged.is_empty() && self.dar_failure.is_none() && self.holes.is_empty()
    }
}

/// dar's verdict on the extract, read with the holes in mind.
///
/// With no hole written, a dar that failed is a failure, as it always was.
/// With one, dar failing is expected — and dar is also exactly what names
/// the files a hole fell in, so its report is read: exit 5 with `Bad CRC`
/// lines is the files, anything else is dar not reading the archive around
/// the hole.
pub(super) fn judge(
    trace: &RestoreTrace,
    verdict: Result<()>,
    root: PathBuf,
    holes: Vec<Damage>,
) -> Result<Extracted> {
    let report = trace.dar.as_ref();
    let stdout = report
        .map(|r| String::from_utf8_lossy(&r.stdout).into_owned())
        .unwrap_or_default();
    let reported = dar::restore::failed_entries(&stdout);
    let only_crc = !reported.is_empty() && reported.iter().all(|(_, why)| why.contains("CRC"));
    match verdict {
        Ok(()) => Ok(Extracted {
            root,
            damaged: Vec::new(),
            dar_failure: None,
            holes,
        }),
        Err(_) if report.and_then(|r| r.exit_code) == Some(5) && only_crc => Ok(Extracted {
            root,
            damaged: reported,
            dar_failure: None,
            holes,
        }),
        Err(e) if !holes.is_empty() => Ok(Extracted {
            root,
            damaged: reported,
            dar_failure: Some(e),
            holes,
        }),
        Err(e) => Err(e),
    }
}

/// Everything [`finish`] needs that the restore already knows.
pub(super) struct Finish<'x> {
    pub conn: &'x Connection,
    pub config: &'x Config,
    pub identities: &'x [age::x25519::Identity],
    pub unit_name: &'x str,
    pub version: i64,
    pub stage_set_id: i64,
    pub target: RestoreTarget<'x>,
    pub scratch: &'x Path,
}

/// One file the catalog records for the version.
struct Expected {
    path: String,
    kind: FileKind,
    sha256: Option<[u8; 32]>,
    link_target: Option<String>,
}

/// What the fill did and could not do.
#[derive(Default)]
struct Notes {
    /// Files taken from another copy and verified.
    filled: Vec<(String, String)>,
    /// Files a copy offered that did not match the recorded checksum.
    rejected: Vec<String>,
    /// Copies that could not be read or asked for.
    unavailable: Vec<String>,
}

/// The restore's end: nothing lost, or the loss filled from another copy, or
/// an error naming what is still lost. Returns the files filled.
pub(super) fn finish(
    ctx: &Finish<'_>,
    supply: &mut Supply<'_, '_, '_>,
    ex: &Extracted,
) -> Result<Vec<(String, String)>> {
    if ex.is_clean() {
        return Ok(Vec::new());
    }
    let snapshot_id: i64 = ctx.conn.query_row(
        "SELECT snapshot_id FROM stage_sets WHERE id = ?1",
        params![ctx.stage_set_id],
        |r| r.get(0),
    )?;
    let only = match ctx.target {
        RestoreTarget::File { file_path, .. } => Some(file_path),
        RestoreTarget::Unit { .. } => None,
    };
    let expected = expected_entries(ctx.conn, snapshot_id, only)?;

    // What is lost, by path, and why.
    let mut lost: BTreeMap<String, String> = BTreeMap::new();
    for (path, why) in &ex.damaged {
        let relative = Path::new(path)
            .strip_prefix(&ex.root)
            .map_or_else(|_| path.clone(), |p| p.to_string_lossy().into_owned());
        if only.is_none_or(|f| f == relative) {
            lost.insert(relative, why.clone());
        }
    }
    for e in &expected {
        if fs::symlink_metadata(ex.root.join(&e.path)).is_err() {
            lost.entry(e.path.clone())
                .or_insert_with(|| "not restored".to_string());
        }
    }
    if !ex.holes.is_empty() {
        // dar's own verdict cannot be trusted over a hole: measured with a
        // slice's header zeroed, it exits 0 having skipped files, and says
        // nothing of others. So the files a damaged slice can have damaged
        // are checked against the checksums staging recorded.
        suspect_check(ctx, ex, &expected, &mut lost)?;
    }
    if let Some(only) = only {
        // `restore file` extracts just that entry: a dar that failed or
        // reported it is the loss; nothing else is its business.
        if lost.is_empty() && ex.dar_failure.is_some() && !ex.root.join(only).exists() {
            lost.insert(only.to_string(), "not restored".to_string());
        }
    }
    progress::log(&format!(
        "restore of unit \"{}\" v{}: {} file(s) damaged or missing after the extract; {} slice(s) \
         with holes or unproven",
        ctx.unit_name,
        ctx.version,
        lost.len(),
        ex.holes.len()
    ));

    let mut notes = Notes::default();
    if !lost.is_empty() {
        let subject = supply_subject(ctx);
        let outcome = fill(
            ctx,
            supply,
            ex,
            snapshot_id,
            &expected,
            &mut lost,
            &mut notes,
        );
        supply.set_subject(subject);
        outcome?;
    }

    // Whole: nothing is lost, and dar's own failure (if any) lost nothing the
    // catalog says should be there.
    if lost.is_empty() && (ex.dar_failure.is_none() || !expected.is_empty()) {
        if !ex.holes.is_empty() {
            warn!(
                "the slices that had holes or could not be proven held nothing this version's \
                 files needed, and every file that could be checked matched its recorded \
                 checksum; the restore is whole"
            );
        }
        return Ok(notes.filled);
    }
    Err(incomplete(ctx, ex, &lost, &notes))
}

fn supply_subject(ctx: &Finish<'_>) -> String {
    format!("unit \"{}\" v{}", ctx.unit_name, ctx.version)
}

/// Hash the restored files a damaged slice can have damaged — those with
/// data in it, by the stage set's own catalogue, or every file when that is
/// gone — and count a mismatch as lost. A file with no recorded checksum can
/// only be trusted if dar stood up to the hole.
fn suspect_check(
    ctx: &Finish<'_>,
    ex: &Extracted,
    expected: &[Expected],
    lost: &mut BTreeMap<String, String>,
) -> Result<()> {
    let slices: std::collections::BTreeSet<i64> = ex.holes.iter().map(|h| h.slice_number).collect();
    let suspects: Option<std::collections::BTreeSet<String>> =
        match own_catalogue(ctx.conn, ctx.stage_set_id)? {
            Some(catalogue) => {
                dar::restore::entries_in_slices(&ctx.config.dar.binary, &catalogue, &slices)
                    .unwrap_or(None)
            }
            None => None,
        };
    let phase = progress::phase("verify-holes", None);
    for e in expected {
        if lost.contains_key(&e.path) || e.kind != FileKind::Regular {
            continue;
        }
        if suspects.as_ref().is_some_and(|s| !s.contains(&e.path)) {
            continue;
        }
        phase.item(e.path.clone());
        let at = ex.root.join(&e.path);
        match e.sha256 {
            Some(_) => {
                if let Err(why) = verified(&at, Some(e)) {
                    lost.insert(e.path.clone(), format!("damaged by a hole: {why}"));
                }
            }
            None if ex.dar_failure.is_some() => {
                lost.insert(
                    e.path.clone(),
                    "cannot be verified after a hole: the catalog records no checksum".to_string(),
                );
            }
            None => {}
        }
    }
    phase.done();
    Ok(())
}

/// The catalog's non-directory entries of the version (or `only` of them).
fn expected_entries(
    conn: &Connection,
    snapshot_id: i64,
    only: Option<&str>,
) -> Result<Vec<Expected>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT p.path, fv.kind, fv.sha256, fv.link_target
           FROM {} WHERE fv.snapshot_id = ?1 AND fv.kind <> 0
            AND (?2 IS NULL OR p.path = ?2)
          ORDER BY p.path",
        crate::db::files::VERSION_FILES
    ))?;
    let rows = stmt.query_map(params![snapshot_id, only], |r| {
        let sha: Option<Vec<u8>> = r.get(2)?;
        Ok(Expected {
            path: r.get(0)?,
            kind: FileKind::from_code(r.get(1)?).unwrap_or(FileKind::Regular),
            sha256: sha.and_then(|b| <[u8; 32]>::try_from(b.as_slice()).ok()),
            link_target: r.get(3)?,
        })
    })?;
    Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
}

/// Fill `lost` from the other copies of the version, one after another.
fn fill(
    ctx: &Finish<'_>,
    supply: &mut Supply<'_, '_, '_>,
    ex: &Extracted,
    snapshot_id: i64,
    expected: &[Expected],
    lost: &mut BTreeMap<String, String>,
    notes: &mut Notes,
) -> Result<()> {
    let mut copies =
        slice_holders::other_copies_of_version(ctx.conn, snapshot_id, ctx.stage_set_id)?;
    // A copy on an in-service volume before one only a quarantined volume holds.
    copies.sort_by_key(|c| {
        c.holders
            .iter()
            .all(|h| h.standing == Standing::Quarantined)
    });
    for copy in &copies {
        if lost.is_empty() {
            break;
        }
        crate::signal::check(|| {
            format!(
                "restore of unit \"{}\" v{} stopped while filling its lost files from another \
                 copy",
                ctx.unit_name, ctx.version
            )
        })?;
        let Some(holder) = choose_holder(supply, copy) else {
            continue;
        };
        if let Err(e) = fill_from(ctx, supply, ex, copy, holder, expected, lost, notes) {
            if matches!(e, TapectlError::Interrupted(_)) {
                return Err(e);
            }
            warn!(stage_set = copy.stage_set_id, volume = %holder.label, error = %e, "cannot fill from this copy");
            notes.unavailable.push(format!(
                "{} (stage set {}): {e}",
                holder.label, copy.stage_set_id
            ));
        }
    }
    Ok(())
}

/// The volume to read `copy` from: the one in the drive if it holds it, else
/// the first in-service holder.
fn choose_holder<'c>(supply: &Supply<'_, '_, '_>, copy: &'c StageCopy) -> Option<&'c StageHolder> {
    let here = supply.here_volume();
    copy.holders
        .iter()
        .find(|h| Some(h.volume_id) == here)
        .or_else(|| copy.holders.first())
}

#[allow(clippy::too_many_arguments)]
fn fill_from(
    ctx: &Finish<'_>,
    supply: &mut Supply<'_, '_, '_>,
    ex: &Extracted,
    copy: &StageCopy,
    holder: &StageHolder,
    expected: &[Expected],
    lost: &mut BTreeMap<String, String>,
    notes: &mut Notes,
) -> Result<()> {
    let positions = get_write_positions(ctx.conn, copy.stage_set_id, &holder.label)?;
    if positions.is_empty() {
        return Ok(());
    }
    let wanted: Vec<String> = lost.keys().cloned().collect();

    // The slices that hold them, through that stage set's own catalogue
    // when it is still on disk; every slice otherwise.
    let subset = match own_catalogue(ctx.conn, copy.stage_set_id)? {
        Some(catalogue) => {
            match dar::restore::entries_slices(&ctx.config.dar.binary, &catalogue, &wanted) {
                Ok(Some(mut needed)) => {
                    if let Some(last) = positions.iter().map(|wp| wp.slice_number).max() {
                        needed.insert(last);
                    }
                    let pick: Vec<WritePositionInfo> = positions
                        .iter()
                        .filter(|wp| needed.contains(&wp.slice_number))
                        .cloned()
                        .collect();
                    if pick.len() == needed.len() {
                        pick
                    } else {
                        positions.clone()
                    }
                }
                _ => positions.clone(),
            }
        }
        None => positions.clone(),
    };

    if Some(holder.volume_id) != supply.here_volume() {
        if !supply.attended() {
            let list: Vec<String> = copy
                .holders
                .iter()
                .map(|h| {
                    format!(
                        "{} ({})",
                        h.label,
                        slice_holders::shelf_of(ctx.conn, h.volume_id, &h.label).describe()
                    )
                })
                .collect();
            return Err(TapectlError::Other(format!(
                "{} lost file(s) can be filled from another copy of this version, held by {}, \
                 and tapectl swaps no cartridge without a person at the terminal (--yes cannot \
                 load one): run it from a terminal and it will ask for that cartridge, or \
                 restore the lost files from it directly",
                wanted.len(),
                list.join(", ")
            )));
        }
        let why = format!(
            "{} file(s) of unit \"{}\" v{} are damaged or missing after the restore, and this \
             copy of the same version holds the same files.",
            wanted.len(),
            ctx.unit_name,
            ctx.version
        );
        match supply.ensure_loaded(
            holder.volume_id,
            &holder.label,
            holder.standing == Standing::Quarantined,
            why,
            "the same files",
            "go without it (those files stay damaged or missing)",
        )? {
            Changed::Loaded => {}
            Changed::Declined => return Ok(()),
        }
    }

    // Spool the slices, extract just the lost paths, and keep what verifies.
    let dir = ctx.scratch.join(format!("fill-{}", copy.stage_set_id));
    fs::create_dir_all(&dir)?;
    let _dir = super::RestoreScratch(dir.clone());
    supply.set_subject(format!(
        "unit \"{}\" v{} (the copy of stage set {})",
        ctx.unit_name, ctx.version, copy.stage_set_id
    ));
    let damage_before = supply.damage.len();
    let phase = progress::phase("fill-read", None);
    for wp in &subset {
        phase.item(format!(
            "stage set {} slice {}",
            copy.stage_set_id, wp.slice_number
        ));
        supply.read_slice(
            wp,
            ctx.identities,
            SliceSink::File(&dir.join(format!("restore.{}.dar", wp.slice_number))),
        )?;
    }
    phase.done();
    // Holes in this copy only cost the files they fall in: the checksum
    // below decides which. They are not the restore's damage.
    supply.damage.truncate(damage_before);

    let out = dir.join("out");
    let (report, verdict) = dar::restore::extract_files_reported(
        &ctx.config.dar.binary,
        &dir.join("restore"),
        &wanted,
        &out,
    );
    let bad: HashSet<String> = report
        .as_ref()
        .map(|r| dar::restore::failed_entries(&String::from_utf8_lossy(&r.stdout)))
        .unwrap_or_default()
        .into_iter()
        .map(|(p, _)| {
            Path::new(&p)
                .strip_prefix(&out)
                .map_or(p.clone(), |r| r.to_string_lossy().into_owned())
        })
        .collect();
    if let Err(e) = &verdict {
        info!(error = %e, "dar reported on the copy's extract");
    }
    let by_path: HashMap<&str, &Expected> = expected.iter().map(|e| (e.path.as_str(), e)).collect();
    for path in &wanted {
        let source = out.join(path);
        if bad.contains(path) || fs::symlink_metadata(&source).is_err() {
            continue;
        }
        match verified(&source, by_path.get(path.as_str()).copied()) {
            Ok(how) => {
                install(&source, &ex.root.join(path))?;
                supply.note_here(format!(
                    "supplied {path} of unit \"{}\" v{} for the restore",
                    ctx.unit_name, ctx.version
                ));
                info!(file = %path, stage_set = copy.stage_set_id, volume = %holder.label, "file filled from another copy ({how})");
                lost.remove(path);
                notes
                    .filled
                    .push((path.clone(), format!("{}, {how}", holder.label)));
            }
            Err(why) => {
                warn!(file = %path, stage_set = copy.stage_set_id, volume = %holder.label, "{why}");
                notes.rejected.push(format!(
                    "{path}: the copy on {} does not match the version's recorded checksum ({why}); not kept",
                    holder.label
                ));
            }
        }
    }
    Ok(())
}

/// Whether the file at `source` is what the catalog recorded for it: the
/// sha256 of a regular file, the target of a symlink. `Ok` says how it was
/// checked; `Err` why it was refused.
fn verified(source: &Path, expected: Option<&Expected>) -> std::result::Result<String, String> {
    let Some(expected) = expected else {
        return Err("the catalog records nothing for this path".to_string());
    };
    match expected.kind {
        FileKind::Regular => {
            let Some(want) = expected.sha256 else {
                // Nothing recorded to compare with: dar's own CRC, which the
                // extract passed, is all there is.
                return Ok("dar's CRC only: the catalog records no checksum".to_string());
            };
            let mut file = fs::File::open(source).map_err(|e| e.to_string())?;
            let mut hasher = Sha256::new();
            let mut buf = vec![0u8; 128 * 1024];
            loop {
                let n = file.read(&mut buf).map_err(|e| e.to_string())?;
                if n == 0 {
                    break;
                }
                hasher.update(&buf[..n]);
            }
            let got: [u8; 32] = hasher.finalize().into();
            if got == want {
                Ok("sha256 matches".to_string())
            } else {
                Err(format!(
                    "sha256 {} where {} is recorded",
                    crate::db::files::sha256_to_hex(&got),
                    crate::db::files::sha256_to_hex(&want)
                ))
            }
        }
        FileKind::Symlink => {
            let target = fs::read_link(source).map_err(|e| e.to_string())?;
            match &expected.link_target {
                Some(want) if Path::new(want) != target => Err(format!(
                    "points to {} where {want} is recorded",
                    target.display()
                )),
                _ => Ok("link target matches".to_string()),
            }
        }
        FileKind::Dir | FileKind::Special => Ok("present".to_string()),
    }
}

/// Put the verified file where the damaged one is: replacing only that path.
fn install(source: &Path, dest: &Path) -> Result<()> {
    if let Some(parent) = dest.parent() {
        fs::create_dir_all(parent)?;
    }
    if fs::symlink_metadata(dest).is_ok() {
        fs::remove_file(dest)?;
    }
    if fs::rename(source, dest).is_err() {
        let meta = fs::symlink_metadata(source)?;
        super::place_restored_entry(source, &meta, dest)?;
    }
    Ok(())
}

/// The error for a restore that is still short of files.
fn incomplete(
    ctx: &Finish<'_>,
    ex: &Extracted,
    lost: &BTreeMap<String, String>,
    notes: &Notes,
) -> TapectlError {
    const SHOWN: usize = 40;
    let mut lines = vec![format!(
        "restore of unit \"{}\" v{} into {} is INCOMPLETE",
        ctx.unit_name,
        ctx.version,
        ex.root.display()
    )];
    for h in &ex.holes {
        lines.push(format!("  {}", h.describe()));
    }
    if let Some(e) = &ex.dar_failure {
        lines.push(format!(
            "  dar could not read the archive through the holes: {e}"
        ));
    }
    if !lost.is_empty() {
        lines.push(format!("  {} file(s) damaged or missing:", lost.len()));
        for (path, why) in lost.iter().take(SHOWN) {
            lines.push(format!("    {path} ({why})"));
        }
        if lost.len() > SHOWN {
            lines.push(format!(
                "    ... and {} more (all are in the session log)",
                lost.len() - SHOWN
            ));
        }
        for (path, why) in lost {
            progress::log(&format!("lost: {path} ({why})"));
        }
    }
    for r in &notes.rejected {
        lines.push(format!("  {r}"));
    }
    for u in &notes.unavailable {
        lines.push(format!("  another copy could not be used: {u}"));
    }
    if !notes.filled.is_empty() {
        lines.push(format!(
            "  {} file(s) were filled from another copy",
            notes.filled.len()
        ));
    }
    lines.push(
        "  Everything else was restored and stays; damaged files are left as dar wrote them, \
         so do not trust them."
            .to_string(),
    );
    TapectlError::Other(lines.join("\n"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_file_is_checked_against_its_recorded_checksum() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("f");
        fs::write(&file, b"alpha").unwrap();
        let want: [u8; 32] = Sha256::digest(b"alpha").into();
        let expected = |sha| Expected {
            path: "f".into(),
            kind: FileKind::Regular,
            sha256: sha,
            link_target: None,
        };
        assert_eq!(
            verified(&file, Some(&expected(Some(want)))).as_deref(),
            Ok("sha256 matches")
        );
        let other: [u8; 32] = Sha256::digest(b"beta").into();
        let why = verified(&file, Some(&expected(Some(other)))).unwrap_err();
        assert!(
            why.contains("sha256") && why.contains("is recorded"),
            "{why}"
        );
        assert!(verified(&file, Some(&expected(None)))
            .unwrap()
            .contains("CRC only"));
        assert!(verified(&file, None).is_err());
    }
}
