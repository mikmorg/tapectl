//! What is left of a restore after its slices are read and tar has run, and
//! how another copy of the same version fills it (ADR-0012 items 101, 102,
//! 255 and 260, issue #480; `volume-format-v3.md` §7.1, "The repair").
//!
//! # What was lost
//!
//! A restore that read every slice whole, fed every member and found every
//! restored file as the file table says it should be lost nothing, and costs
//! nothing here. Otherwise the restore's [`super::reader::Outcome`] names the
//! loss, path by path: a member left out because a slice (or a part of one)
//! could not be given, a member whose bytes did not hash as the table says,
//! a file tar did not make or made wrong. The holes themselves say the loss
//! was not of the restore's making.
//!
//! # The repair
//!
//! Two copies of one version may come from different stagings: different
//! stage sets, so different slices and different slice hashes, which slice
//! substitution cannot match, while the files are the same, because staging
//! records every file's sha256 for the version and refuses a re-stage whose
//! hashes disagree. So the lost files are restored from another copy of the
//! SAME unit and the SAME version, and only those paths, reading only the
//! slices that hold them through that stage set's own file table. Each file
//! taken is checked against the version's recorded sha256 and kept only if
//! it matches: a file that does not is said so and not kept. Other versions
//! are never a source, even where a file is unchanged.
//!
//! **A range is a source** for the files whose slices it holds whole (item
//! 260, which replaces item 143): the files of the copy's table that
//! [`crate::volume::filetable::held_whole`] says the range holds are taken
//! from it, and the rest are not asked of it.
//!
//! The cartridge is asked for as in substitution (by label and chip serial;
//! with no terminal, refused by name, listing the holders), and the copies
//! are tried in turn until the unit is whole or none is left.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use super::supply::{Changed, Damage, Supply};
use super::*;
use crate::volume::filetable::{self, Row};
use crate::volume::slice_holders::{self, StageCopy, StageHolder, Standing};

/// What the extract left, handed to [`finish`].
pub(super) struct Extracted {
    /// Where tar wrote: the destination for a unit, the extract directory
    /// for one file.
    pub root: PathBuf,
    /// The members that are not there as the table says, by path, and why.
    pub lost: BTreeMap<String, String>,
    /// The slices this restore had holes in, or could not prove.
    pub holes: Vec<Damage>,
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
    /// The file table of the stage set the restore read: what the version's
    /// files are recorded to be, whichever copy supplies them.
    pub rows: &'x [Row],
    pub options: &'x RestoreOptions,
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
    let mut lost = ex.lost.clone();
    progress::log(&format!(
        "restore of unit \"{}\" v{}: {} file(s) damaged or missing after the extract; {} slice(s) \
         with holes or unproven",
        ctx.unit_name,
        ctx.version,
        lost.len(),
        ex.holes.len()
    ));
    if lost.is_empty() {
        if !ex.holes.is_empty() {
            warn!(
                "the slices that had holes or could not be proven held nothing this version's \
                 files needed, and every file restored matched the table's checksum; the \
                 restore is whole"
            );
        }
        return Ok(Vec::new());
    }

    let mut notes = Notes::default();
    let subject = supply_subject(ctx);
    let outcome = fill(ctx, supply, ex, &mut lost, &mut notes);
    supply.set_subject(subject);
    outcome?;
    if lost.is_empty() {
        return Ok(notes.filled);
    }
    Err(incomplete(ctx, ex, &lost, &notes))
}

fn supply_subject(ctx: &Finish<'_>) -> String {
    format!("unit \"{}\" v{}", ctx.unit_name, ctx.version)
}

/// Fill `lost` from the other copies of the version, one after another.
fn fill(
    ctx: &Finish<'_>,
    supply: &mut Supply<'_, '_, '_>,
    ex: &Extracted,
    lost: &mut BTreeMap<String, String>,
    notes: &mut Notes,
) -> Result<()> {
    let snapshot_id: i64 = ctx.conn.query_row(
        "SELECT snapshot_id FROM stage_sets WHERE id = ?1",
        params![ctx.stage_set_id],
        |r| r.get(0),
    )?;
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
        if let Err(e) = fill_from(ctx, supply, ex, copy, holder, lost, notes) {
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
/// the first in-service whole holder, else the first holder.
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
    lost: &mut BTreeMap<String, String>,
    notes: &mut Notes,
) -> Result<()> {
    let positions = get_write_positions(ctx.conn, copy.stage_set_id, &holder.label)?;
    if positions.is_empty() {
        return Ok(());
    }
    // That copy's own file table: its slices are cut differently from the
    // restore's, and only its table says which hold which file.
    let rows = stage_files::load(ctx.conn, copy.stage_set_id)?;
    if rows.is_empty() {
        notes.unavailable.push(format!(
            "{} (stage set {}) has no file table in this catalog, and a copy is read by its table",
            holder.label, copy.stage_set_id
        ));
        return Ok(());
    }
    let facts = stage_facts(ctx.conn, copy.stage_set_id, &rows)?;
    let wanted_owned: Vec<String> = lost.keys().cloned().collect();
    let wanted: Vec<&str> = wanted_owned.iter().map(String::as_str).collect();
    let mut members = filetable::select(&rows, &wanted);
    if holder.range {
        // Item 260: a range is a source for the files whose slices it holds
        // whole, and for no other.
        let present: BTreeSet<u64> = positions
            .iter()
            .filter_map(|wp| u64::try_from(wp.slice_number).ok())
            .collect();
        let held: BTreeSet<&str> = filetable::held_whole(&rows, facts.slice_size, &present)
            .into_iter()
            .map(|r| r.path.as_str())
            .collect();
        members.retain(|r| held.contains(r.path.as_str()));
    }
    // Only the lost paths matter of what `select` brought (a folder's
    // contents, a hard link's target).
    let asked: BTreeSet<&str> = wanted.iter().copied().collect();
    if !members.iter().any(|m| asked.contains(m.path.as_str())) {
        if holder.range {
            notes.unavailable.push(format!(
                "{} (stage set {}) holds only a range of this version, and none of the lost \
                 files lies whole in it",
                holder.label, copy.stage_set_id
            ));
        }
        return Ok(());
    }
    let needed = filetable::slices_of(members.iter().copied(), facts.slice_size);
    let subset: Vec<WritePositionInfo> = positions
        .into_iter()
        .filter(|wp| u64::try_from(wp.slice_number).is_ok_and(|n| needed.contains(&n)))
        .collect();

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
                lost.len(),
                list.join(", ")
            )));
        }
        let why = format!(
            "{} file(s) of unit \"{}\" v{} are damaged or missing after the restore, and this \
             copy of the same version holds the same files.",
            lost.len(),
            ctx.unit_name,
            ctx.version
        );
        match supply.ensure_loaded(
            holder.volume_id,
            &holder.label,
            holder.standing == Standing::Quarantined,
            why,
            "the same files",
            Some("go without it (those files stay damaged or missing)"),
        )? {
            Changed::Loaded => {}
            Changed::Declined => return Ok(()),
        }
    }

    // Read the slices that hold the lost paths through that copy's table
    // into a directory of their own, and keep what verifies.
    let scratch = super::scratch_dir(
        Path::new(ctx.target.destination()),
        ctx.options.scratch.as_deref(),
    );
    // A restore of a unit makes no scratch directory: this one is made for
    // the fill and removed with it. `restore file`'s is its own.
    let _scratch = (!scratch.exists()).then(|| super::RestoreScratch(scratch.clone()));
    let dir = scratch.join(format!("fill-{}", copy.stage_set_id));
    fs::create_dir_all(&dir)?;
    let _dir = super::RestoreScratch(dir.clone());
    supply.set_subject(format!(
        "unit \"{}\" v{} (the copy of stage set {})",
        ctx.unit_name, ctx.version, copy.stage_set_id
    ));
    let damage_before = supply.damage.len();
    let out = dir.join("out");
    let job = reader::Job {
        unit_name: ctx.unit_name,
        members: &members,
        slice_size: facts.slice_size,
        compression: facts.compression,
        positions: &subset,
        unheld: &BTreeMap::new(),
        offsets: &facts.offsets,
        identities: ctx.identities,
        tar_binary: &ctx.config.tar.binary,
        dest: &out,
        overwrite: false,
        file_check: true,
    };
    let mut scratch_trace = RestoreTrace::default();
    let phase = progress::phase("fill-read", None);
    let read = reader::run(&job, supply, &mut scratch_trace);
    phase.done();
    // Holes in this copy only cost the files they fall in: the table's
    // checksums below decide which. They are not the restore's damage.
    supply.damage.truncate(damage_before);
    let read = read?;

    let by_path: HashMap<&str, &Row> = ctx.rows.iter().map(|r| (r.path.as_str(), r)).collect();
    // Folders the copy gave, `(copy's folder, destination)`: their recorded
    // metadata is applied once everything under them is in (restore-6).
    let mut folders: Vec<(PathBuf, PathBuf)> = Vec::new();
    for path in &wanted {
        let source = out.join(crate::volume::index::on_disk(path));
        if read.lost.contains_key(*path) || fs::symlink_metadata(&source).is_err() {
            continue;
        }
        match verified(&source, by_path.get(*path).copied()) {
            Ok(how) => {
                let dest = ex.root.join(crate::volume::index::on_disk(path));
                // One path that cannot be put in place does not end the
                // fill: it stays lost, with the reason, and the others go on.
                if let Err(e) = install(&source, &dest) {
                    warn!(file = %path, volume = %holder.label, "cannot be installed: {e}");
                    if let Some(why) = lost.get_mut(*path) {
                        why.push_str(&format!(
                            "; the copy on {} holds it, but it could not be put in place: {e}",
                            holder.label
                        ));
                    }
                    continue;
                }
                if by_path.get(*path).is_some_and(|r| r.kind == Kind::Folder) {
                    folders.push((source.clone(), dest));
                }
                supply.note_here(format!(
                    "supplied {path} of unit \"{}\" v{} for the restore",
                    ctx.unit_name, ctx.version
                ));
                info!(file = %path, stage_set = copy.stage_set_id, volume = %holder.label, "file filled from another copy ({how})");
                lost.remove(*path);
                notes
                    .filled
                    .push(((*path).to_string(), format!("{}, {how}", holder.label)));
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
    // The folders last, the deepest first, so a folder whose mode forbids
    // writing has received its files and its own time is not moved by them.
    folders.sort_by_key(|(_, dest)| std::cmp::Reverse(dest.components().count()));
    for (source, dest) in &folders {
        if let Err(e) = apply_folder_metadata(source, dest) {
            warn!(folder = %dest.display(), "the copy's metadata could not be set on the folder: {e}");
        }
    }
    Ok(())
}

/// Give the folder `dest` the mode, owner and modification time of the
/// copy's `source` (restore-6): what tar would have set had the folder's own
/// member come through. The owner is set where the process may (a failed
/// chown for want of permission is not an error), then the mode, since a
/// chown can clear bits.
fn apply_folder_metadata(source: &Path, dest: &Path) -> Result<()> {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    let meta = fs::symlink_metadata(source)?;
    match std::os::unix::fs::chown(dest, Some(meta.uid()), Some(meta.gid())) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => {}
        Err(e) => return Err(e.into()),
    }
    fs::set_permissions(dest, fs::Permissions::from_mode(meta.mode() & 0o7777))?;
    fs::File::open(dest)?.set_modified(meta.modified()?)?;
    Ok(())
}

/// Whether the file at `source` is what the version's table records for it:
/// the sha256 of a regular file or a hard link's content. `Ok` says how it
/// was checked; `Err` why it was refused.
fn verified(source: &Path, expected: Option<&Row>) -> std::result::Result<String, String> {
    let Some(expected) = expected else {
        return Err("the version's table records nothing for this path".to_string());
    };
    match expected.kind {
        Kind::File | Kind::HardLink => {
            let Some(want) = expected.sha256 else {
                return Err("the table records no checksum for it".to_string());
            };
            let got = super::reader::hash_file(source).map_err(|e| e.to_string())?;
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
        Kind::Symlink => {
            // The table does not state a link's target (it is in the
            // stream, which the copy's own tar carried): present is all
            // that can be said of it.
            fs::read_link(source).map_err(|e| e.to_string())?;
            Ok("a link, as the copy's stream carried it".to_string())
        }
        Kind::Folder | Kind::Fifo | Kind::CharDevice | Kind::BlockDevice => {
            Ok("present".to_string())
        }
    }
}

/// Put the verified file where the damaged one is: replacing only that path.
///
/// A folder is not moved and nothing is unlinked in its place (#584): the
/// copy's folder holds its whole subtree, and each lost path under it is
/// installed by its own turn. A folder that is already there is left as it
/// is; one that is not is made, with the copy's permission bits and the
/// owner's write and search bits besides, so the files that follow can be
/// installed in it; [`apply_folder_metadata`] gives it its recorded mode,
/// owner and time after they are (tar sets a folder's mode last for the same
/// reason). A file over a folder in the destination is refused, never
/// removed: nothing is deleted from the destination but a file or a link.
fn install(source: &Path, dest: &Path) -> Result<()> {
    if let Some(parent) = dest.parent() {
        fs::create_dir_all(parent)?;
    }
    let meta = fs::symlink_metadata(source)?;
    if meta.is_dir() {
        match fs::symlink_metadata(dest) {
            Ok(m) if m.is_dir() => return Ok(()),
            Ok(_) => fs::remove_file(dest)?,
            Err(_) => {}
        }
        fs::create_dir(dest)?;
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(
            dest,
            fs::Permissions::from_mode(meta.permissions().mode() | 0o700),
        )?;
        return Ok(());
    }
    if let Ok(m) = fs::symlink_metadata(dest) {
        if m.is_dir() {
            return Err(TapectlError::Other(
                "a folder is there, and a file is not put over it".to_string(),
            ));
        }
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
        "  Everything else was restored and stays; a file that failed its check is left as it \
         was written, so do not trust it, and the files left out are not in the destination. \
         Give the list to another copy of the same version to repair it."
            .to_string(),
    );
    TapectlError::Other(lines.join("\n"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use sha2::Digest;

    fn row(kind: Kind, path: &str, sha: Option<[u8; 32]>) -> Row {
        Row {
            kind,
            slice: 1,
            start: 0,
            end: 1024,
            size: 5,
            sha256: sha,
            path: path.into(),
            link: None,
        }
    }

    /// A file is checked against the checksum the version's table records.
    #[test]
    fn a_file_is_checked_against_its_recorded_checksum() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("f");
        fs::write(&file, b"alpha").unwrap();
        let want: [u8; 32] = sha2::Sha256::digest(b"alpha").into();
        assert_eq!(
            verified(&file, Some(&row(Kind::File, "f", Some(want)))).as_deref(),
            Ok("sha256 matches")
        );
        let other: [u8; 32] = sha2::Sha256::digest(b"beta").into();
        let why = verified(&file, Some(&row(Kind::File, "f", Some(other)))).unwrap_err();
        assert!(
            why.contains("sha256") && why.contains("is recorded"),
            "{why}"
        );
        assert!(verified(&file, Some(&row(Kind::File, "f", None))).is_err());
        assert!(verified(&file, None).is_err());
    }

    /// A hard link's name is checked by its content, against the sha256 its
    /// row states (its target's).
    #[test]
    fn a_hard_links_name_is_checked_by_its_content() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("h");
        fs::write(&file, b"alpha").unwrap();
        let want: [u8; 32] = sha2::Sha256::digest(b"alpha").into();
        assert!(verified(&file, Some(&row(Kind::HardLink, "h", Some(want)))).is_ok());
        let other: [u8; 32] = sha2::Sha256::digest(b"beta").into();
        assert!(verified(&file, Some(&row(Kind::HardLink, "h", Some(other)))).is_err());
    }

    /// #584: a folder is never unlinked and never moved with its subtree. One
    /// that is there (the unit's root `.` among them) is left as it is, and
    /// the paths under it are installed by their own turns; one that is not
    /// there is made empty.
    #[test]
    fn a_folder_is_left_or_made_and_never_unlinked_or_moved_whole() {
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("out");
        fs::create_dir_all(out.join("d")).unwrap();
        fs::write(out.join("d/x"), b"from the copy").unwrap();
        let root = dir.path().join("root");
        fs::create_dir_all(root.join("d")).unwrap();
        fs::write(root.join("d/keep"), b"keep").unwrap();

        // The unit's root, which exists.
        install(&out, &root.join(".")).expect("the root is left as it is");
        assert_eq!(fs::read(root.join("d/keep")).unwrap(), b"keep");
        assert!(!root.join("d/x").exists(), "its subtree is not moved in");

        // A folder that exists keeps what is in it.
        install(&out.join("d"), &root.join("d")).expect("an existing folder is left");
        assert_eq!(fs::read(root.join("d/keep")).unwrap(), b"keep");
        assert!(!root.join("d/x").exists());

        // A folder that is not there is made, empty: its files come later.
        fs::set_permissions(
            out.join("d"),
            std::os::unix::fs::PermissionsExt::from_mode(0o555),
        )
        .unwrap();
        install(&out.join("d"), &root.join("e")).expect("a missing folder is made");
        assert!(root.join("e").is_dir());
        assert_eq!(fs::read_dir(root.join("e")).unwrap().count(), 0);
        fs::write(out.join("f"), b"file").unwrap();
        install(&out.join("f"), &root.join("e/f")).expect("a file goes in the made folder");
        assert_eq!(fs::read(root.join("e/f")).unwrap(), b"file");
        fs::set_permissions(
            out.join("d"),
            std::os::unix::fs::PermissionsExt::from_mode(0o755),
        )
        .unwrap();
    }

    /// A verified file replaces only the damaged path, and a path under a
    /// folder the restore never made gets its folder.
    #[test]
    fn an_installed_file_replaces_only_its_own_path() {
        let dir = tempfile::tempdir().unwrap();
        let from = dir.path().join("from");
        fs::write(&from, b"good").unwrap();
        let root = dir.path().join("root");
        fs::create_dir_all(root.join("d")).unwrap();
        fs::write(root.join("d/bad"), b"BAD").unwrap();
        fs::write(root.join("d/keep"), b"keep").unwrap();
        install(&from, &root.join("d/bad")).unwrap();
        assert_eq!(fs::read(root.join("d/bad")).unwrap(), b"good");
        assert_eq!(fs::read(root.join("d/keep")).unwrap(), b"keep");
        fs::write(&from, b"deep").unwrap();
        install(&from, &root.join("e/f/g")).unwrap();
        assert_eq!(fs::read(root.join("e/f/g")).unwrap(), b"deep");
    }
}
