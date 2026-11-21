//! T4.8's proofs: tapectl's one reader, through real stagings on `MemStore`
//! (`volume-format-v3.md` §7.1; the plan's T4.8 "Verify" list). Each unit is
//! staged by the real `stage create` and each tape written by the real
//! `volume write` ([`super::fixture::Home`]); each restore is compared with
//! the source by content, and by owner, mode, times, xattrs and ACL where the
//! test can.

use std::fs;

use super::fixture::{assert_restored, files, listing, Home, Item, What, TENANT};
use super::RestoreOptions;

/// A unit of a few files, a folder with one more, and a symbolic link:
/// several slices at 64K.
fn small_tree() -> Vec<Item> {
    let mut items = files("alpha", 4, 30_000, 1);
    items.push(Item::file("sub/deep.txt", b"deep in a folder\n".to_vec()));
    items.push(Item::Symlink("alpha-link".into(), "alpha-01.bin".into()));
    items
}

fn home_with(unit: &str, items: &[Item], slice_size: &str, compression: Option<&str>) -> Home {
    let mut h = Home::new(slice_size, compression, 1);
    h.unit(TENANT, unit, items);
    h.stage(unit);
    h.volume("RDR-A");
    h.write("RDR-A");
    h
}

/// A whole unit comes back as it was, from a tape written by the real write,
/// restored by the real reader: every file by content, the link as a link,
/// and the folder. The restore is one tar run.
#[test]
fn a_whole_unit_restores_as_it_was() {
    let h = home_with("alpha", &small_tree(), "64K", None);
    assert!(
        h.num_slices("alpha") >= 3,
        "several slices: the cut is exercised"
    );
    let mut bay = h.bay("RDR-A", &[]);
    let (r, dest) = h.restore(
        "alpha",
        What::Unit,
        "RDR-A",
        &RestoreOptions::default(),
        &mut bay,
    );
    let report = r.expect("a clean restore");
    assert_restored(&h, "alpha", dest.path(), &[]);
    assert!(report.filled.is_empty() && report.substitutions.is_empty());
    assert!(bay.asked.is_empty(), "one cartridge, nobody asked");
    assert!(
        fs::symlink_metadata(dest.path().join("alpha-link"))
            .unwrap()
            .is_symlink(),
        "{:?}",
        listing(dest.path())
    );
}

// ── helpers ──

use std::collections::BTreeSet;
use std::path::Path;

use super::repair_tests::tests::Reply;
use super::{plan_units, restore_units_from_bay, unit_requests};

/// The paths an INCOMPLETE restore lists as damaged or missing.
fn lost_paths(err: &str) -> BTreeSet<String> {
    err.lines()
        .skip_while(|l| !l.contains("file(s) damaged or missing:"))
        .skip(1)
        .take_while(|l| l.starts_with("    ") && !l.starts_with("     "))
        .filter_map(|l| l.trim().rsplit_once(" (").map(|(p, _)| p.to_string()))
        .collect()
}

/// Mode and modification time of every regular file of `a` are those of `b`.
fn assert_same_meta(a: &Path, b: &Path) {
    use std::os::unix::fs::MetadataExt;
    for (path, kind) in listing(a) {
        if !kind.starts_with("file ") {
            continue;
        }
        let (x, y) = (
            fs::metadata(a.join(&path)).unwrap(),
            fs::metadata(b.join(&path)).unwrap(),
        );
        assert_eq!(x.mode() & 0o7777, y.mode() & 0o7777, "{path}: mode");
        assert_eq!(x.mtime(), y.mtime(), "{path}: mtime");
    }
}

/// Every restored file but the `lost` ones is in `dest` byte for byte, and
/// no lost one is.
fn assert_all_but(h: &Home, unit: &str, dest: &Path, lost: &BTreeSet<String>) {
    let want = listing(&h.source(unit));
    let got = listing(dest);
    for (path, what) in &want {
        if lost.contains(path) {
            assert!(
                got.get(path) != Some(what),
                "{path} was listed lost, but it is there whole"
            );
        } else {
            assert_eq!(got.get(path), Some(what), "{path} must be restored whole");
        }
    }
}

/// The paths of the members the file table says begin in slice `n`.
fn members_in_slice(h: &Home, unit: &str, n: u64) -> BTreeSet<String> {
    h.rows(unit)
        .into_iter()
        .filter(|r| r.slice == n)
        .map(|r| r.path)
        .collect()
}

// ── the proofs ──

/// One path: only the slices that hold it are read, the file is placed
/// directly in `--to`, and it is byte for byte what was archived.
#[test]
fn one_path_reads_only_its_slices_and_is_placed() {
    let h = home_with("alpha", &small_tree(), "64K", None);
    let rows = h.rows("alpha");
    let row = rows.iter().find(|r| r.path == "sub/deep.txt").unwrap();
    let mut bay = h.bay("RDR-A", &[]);
    let (r, dest) = h.restore(
        "alpha",
        What::File("sub/deep.txt"),
        "RDR-A",
        &RestoreOptions::default(),
        &mut bay,
    );
    let report = r.expect("one path");
    assert_eq!(
        fs::read(dest.path().join("deep.txt")).unwrap(),
        b"deep in a folder\n"
    );
    let needed = row.slices(64 * 1024).count();
    assert_eq!(
        report.slices, needed,
        "only the slices of the file are read"
    );
    assert!(
        (report.slices as i64) < h.num_slices("alpha"),
        "positive control: the unit has more slices than the file needs"
    );
    assert!(bay.asked.is_empty());
}

/// A volume of three units: each into its own directory, in one pass over
/// the one tape, each byte for byte.
#[test]
fn a_volume_of_three_units_restores_each_into_its_own_directory() {
    let mut h = Home::new("64K", None, 1);
    for (i, u) in ["una", "duo", "tre"].iter().enumerate() {
        h.unit(TENANT, u, &files(u, 2, 20_000, 10 * i as u32 + 1));
        h.stage(u);
    }
    h.volume("RDR-V");
    h.write("RDR-V");
    let root = tempfile::TempDir::new().unwrap();
    let names: Vec<String> = ["una", "duo", "tre"]
        .iter()
        .map(|s| s.to_string())
        .collect();
    let requests = unit_requests(&names, &root.path().to_string_lossy()).unwrap();
    let options = RestoreOptions::default();
    let planned = plan_units(&h.conn, &h.config, "RDR-V", &requests, &options).unwrap();
    let mut bay = h.bay("RDR-V", &[]);
    let report = restore_units_from_bay(
        &h.conn,
        &h.paths,
        &h.config,
        "RDR-V",
        &planned,
        &options,
        false,
        &mut bay,
        h.site(),
    )
    .unwrap();
    assert_eq!(report.failed(), 0, "{:?}", report.units);
    assert_eq!(report.units.len(), 3);
    for u in ["una", "duo", "tre"] {
        assert_restored(&h, u, &root.path().join(u), &[]);
    }
}

/// A slice damaged on the cartridge in the drive is read from another copy,
/// by its sha256, asking for the cartridge by label: the restore is whole and
/// says so.
#[test]
fn a_damaged_slice_is_replaced_from_a_second_copy() {
    let mut h = Home::new("64K", None, 2);
    h.unit(TENANT, "alpha", &small_tree());
    h.stage("alpha");
    h.volume("RDR-A");
    h.volume("RDR-B");
    h.write("RDR-A");
    h.write("RDR-B");
    let pos = h.position("RDR-A", "alpha", 2);
    h.flip("RDR-A", pos, 2_000);
    let mut bay = h.bay("RDR-A", &[Reply::Load("RDR-B")]);
    let (r, dest) = h.restore(
        "alpha",
        What::Unit,
        "RDR-A",
        &RestoreOptions::default(),
        &mut bay,
    );
    let report = r.expect("whole from the second copy");
    assert_restored(&h, "alpha", dest.path(), &[]);
    assert_eq!(report.substitutions.len(), 1, "{:?}", report.substitutions);
    assert_eq!(report.substitutions[0].supplied_by, "RDR-B");
    assert_eq!(bay.asked_labels(), ["RDR-B"]);
}

/// A slice no copy can give: the members in it are left out and listed, and
/// every other file is whole. The same unit, undamaged, restores whole (the
/// first test above), so the listing is the damage's and nothing else's.
#[test]
fn a_slice_with_no_copy_leaves_out_exactly_its_members() {
    let mut h = home_with("alpha", &small_tree(), "64K", None);
    let in_two = members_in_slice(&h, "alpha", 2);
    assert!(!in_two.is_empty(), "slice 2 holds members");
    let pos = h.position("RDR-A", "alpha", 2);
    h.flip("RDR-A", pos, 2_000);
    let mut bay = h.bay("RDR-A", &[]);
    let (r, dest) = h.restore(
        "alpha",
        What::Unit,
        "RDR-A",
        &RestoreOptions::default(),
        &mut bay,
    );
    let err = r.expect_err("a slice nobody has").to_string();
    assert!(err.contains("INCOMPLETE"), "{err}");
    assert_eq!(lost_paths(&err), in_two, "{err}");
    assert_all_but(&h, "alpha", dest.path(), &in_two);
    assert!(
        listing(dest.path()).len() > 1,
        "positive control: the other slices' files are there"
    );
}

/// A long member's run with one piece nobody has: that member is left out
/// and listed, its partial file is removed again, and every other file is
/// whole (item 264: "removed again if tar had begun it").
#[test]
fn a_run_with_a_lost_piece_leaves_out_only_its_member() {
    let mut items = vec![Item::file("a-before.txt", noise_bytes(10_000, 1))];
    items.push(Item::file("b-big.bin", noise_bytes(200_000, 2)));
    items.push(Item::file("c-after.txt", noise_bytes(10_000, 3)));
    let mut h = home_with("alpha", &items, "64K", None);
    let rows = h.rows("alpha");
    let big = rows.iter().find(|r| r.path == "b-big.bin").unwrap();
    assert!(big.is_run(64 * 1024), "the big file is a run");
    let piece = big.slice + 1;
    let pos = h.position("RDR-A", "alpha", piece as i64);
    h.flip("RDR-A", pos, 2_000);
    let mut bay = h.bay("RDR-A", &[]);
    let (r, dest) = h.restore(
        "alpha",
        What::Unit,
        "RDR-A",
        &RestoreOptions::default(),
        &mut bay,
    );
    let err = r.expect_err("a piece nobody has").to_string();
    let lost = lost_paths(&err);
    assert_eq!(
        lost,
        BTreeSet::from(["b-big.bin".to_string()]),
        "only the long member: {err}"
    );
    assert!(
        !dest.path().join("b-big.bin").exists(),
        "the partial file is removed again"
    );
    assert_all_but(&h, "alpha", dest.path(), &lost);
    assert_eq!(
        fs::read(dest.path().join("c-after.txt")).unwrap(),
        noise_bytes(10_000, 3),
        "the member after the run is fed to a new tar run"
    );
}

fn noise_bytes(len: usize, seed: u32) -> Vec<u8> {
    super::fixture::noise(len, seed)
}

/// One bad 64 KiB chunk in a slice of many files: only the files that touch
/// it are listed.
#[test]
fn one_bad_chunk_costs_only_the_files_that_touch_it() {
    let items = files("many", 30, 5_000, 5);
    let mut h = home_with("alpha", &items, "256K", None);
    assert_eq!(h.num_slices("alpha"), 1, "one slice of many files");
    let rows = h.rows("alpha");
    let pos = h.position("RDR-A", "alpha", 1);
    // Well inside the ciphertext's second chunk.
    h.flip("RDR-A", pos, 65_536 + 16 + 1_500);
    let mut bay = h.bay("RDR-A", &[]);
    let (r, dest) = h.restore(
        "alpha",
        What::Unit,
        "RDR-A",
        &RestoreOptions::default(),
        &mut bay,
    );
    let err = r.expect_err("a chunk nobody has").to_string();
    let lost = lost_paths(&err);
    assert!(!lost.is_empty(), "{err}");
    assert!(lost.len() < rows.len(), "only some files: {err}");
    // The lost set is exactly the members that overlap one 64 KiB window of
    // the cut.
    let window = |k: u64| -> BTreeSet<String> {
        rows.iter()
            .filter(|r| r.start < (k + 1) * 65_536 && r.end > k * 65_536)
            .map(|r| r.path.clone())
            .collect()
    };
    assert!(
        (0..4).any(|k| window(k) == lost),
        "{lost:?} is not the members of one chunk of the cut"
    );
    assert_all_but(&h, "alpha", dest.path(), &lost);
}

/// The same in a compressed slice: nothing after the bad chunk decodes, so
/// the files from the chunk to the slice's end are listed and the files
/// before it are whole.
#[test]
fn one_bad_chunk_in_a_compressed_slice_costs_the_files_from_it_to_the_end() {
    let items = files("many", 30, 5_000, 5);
    let mut h = home_with("alpha", &items, "256K", Some("zstd"));
    assert_eq!(h.num_slices("alpha"), 1);
    let rows = h.rows("alpha");
    let pos = h.position("RDR-A", "alpha", 1);
    // Inside the third 64 KiB chunk: zstd decodes a block of up to 128 KiB
    // at a time, so the first two chunks' bytes are the ones that decode.
    h.flip("RDR-A", pos, 2 * (65_536 + 16) + 1_500);
    let mut bay = h.bay("RDR-A", &[]);
    let (r, dest) = h.restore(
        "alpha",
        What::Unit,
        "RDR-A",
        &RestoreOptions::default(),
        &mut bay,
    );
    let err = r.expect_err("a chunk nobody has").to_string();
    let lost = lost_paths(&err);
    assert!(!lost.is_empty(), "{err}");
    // A suffix of the slice's members, in stream order: from some member to
    // the last.
    let order: Vec<&str> = rows.iter().map(|r| r.path.as_str()).collect();
    let first = order.iter().position(|p| lost.contains(*p)).unwrap();
    assert!(first > 0, "the files before the chunk are whole: {err}");
    for (i, p) in order.iter().enumerate() {
        assert_eq!(lost.contains(*p), i >= first, "{p}: {err}");
    }
    assert_all_but(&h, "alpha", dest.path(), &lost);
}

/// A compressed unit whole and by path, for each algorithm carried.
#[test]
fn a_compressed_unit_restores_whole_and_by_path_for_each_algorithm() {
    for algorithm in ["gzip", "xz", "zstd"] {
        let h = home_with("alpha", &small_tree(), "64K", Some(algorithm));
        let mut bay = h.bay("RDR-A", &[]);
        let (r, dest) = h.restore(
            "alpha",
            What::Unit,
            "RDR-A",
            &RestoreOptions::default(),
            &mut bay,
        );
        r.unwrap_or_else(|e| panic!("{algorithm}: {e}"));
        assert_restored(&h, "alpha", dest.path(), &[]);
        let mut bay = h.bay("RDR-A", &[]);
        let (r, dest) = h.restore(
            "alpha",
            What::File("sub/deep.txt"),
            "RDR-A",
            &RestoreOptions::default(),
            &mut bay,
        );
        r.unwrap_or_else(|e| panic!("{algorithm} by path: {e}"));
        assert_eq!(
            fs::read(dest.path().join("deep.txt")).unwrap(),
            b"deep in a folder\n",
            "{algorithm}"
        );
    }
}

/// A hard link is fed only with its target's member: when the target's slice
/// is lost the link is listed damaged too (item 256), though its own slice
/// is whole.
#[test]
fn a_hard_link_whose_target_is_damaged_is_listed_damaged_too() {
    let items = vec![
        Item::file("a-target.bin", noise_bytes(40_000, 7)),
        Item::file("b-fill.bin", noise_bytes(60_000, 8)),
        Item::Hard("c-link.bin".into(), "a-target.bin".into()),
    ];
    let mut h = home_with("alpha", &items, "64K", None);
    let rows = h.rows("alpha");
    let target = rows.iter().find(|r| r.path == "a-target.bin").unwrap();
    let link = rows.iter().find(|r| r.path == "c-link.bin").unwrap();
    assert_ne!(target.slice, link.slice, "the link lies in another slice");
    let pos = h.position("RDR-A", "alpha", target.slice as i64);
    h.flip("RDR-A", pos, 2_000);
    let mut bay = h.bay("RDR-A", &[]);
    let (r, dest) = h.restore(
        "alpha",
        What::Unit,
        "RDR-A",
        &RestoreOptions::default(),
        &mut bay,
    );
    let err = r.expect_err("the target is lost").to_string();
    let lost = lost_paths(&err);
    assert!(
        lost.contains("a-target.bin") && lost.contains("c-link.bin"),
        "{err}"
    );
    assert!(
        !dest.path().join("c-link.bin").exists(),
        "no link to nothing"
    );
    assert_eq!(
        fs::read(dest.path().join("b-fill.bin")).unwrap(),
        noise_bytes(60_000, 8),
        "positive control: the file between them is whole"
    );
}

/// A file named like an option is a member and nothing else: tar is given
/// the members on its standard input, never a name on its command line.
#[test]
fn files_named_like_options_are_restored_and_acted_on_by_nobody() {
    let items = vec![
        Item::file("--checkpoint=1", b"one\n".to_vec()),
        Item::file("-rf", b"two\n".to_vec()),
        Item::file("--to-command=touch tapectl-pwned", b"three\n".to_vec()),
        Item::file("plain.txt", b"four\n".to_vec()),
    ];
    let h = home_with("alpha", &items, "64K", None);
    let mut bay = h.bay("RDR-A", &[]);
    let (r, dest) = h.restore(
        "alpha",
        What::Unit,
        "RDR-A",
        &RestoreOptions::default(),
        &mut bay,
    );
    r.expect("option-like names");
    assert_restored(&h, "alpha", dest.path(), &[]);
    assert!(!dest.path().join("tapectl-pwned").exists());
    // And one of them by path.
    let mut bay = h.bay("RDR-A", &[]);
    let (r, dest) = h.restore(
        "alpha",
        What::File("--checkpoint=1"),
        "RDR-A",
        &RestoreOptions::default(),
        &mut bay,
    );
    r.expect("an option-like name by path");
    assert_eq!(
        fs::read(dest.path().join("--checkpoint=1")).unwrap(),
        b"one\n"
    );
}

/// An ordinary user restores a unit: the files come back with their modes
/// and times, owned by whoever restores them (tar changes no owner by name),
/// and the id map is in the report: what the numbers in the stream were
/// called when it was written (item 278). As root the same, with a file
/// owned by an id the host has no name for.
#[test]
fn the_id_map_is_shown_and_no_owner_is_changed_by_name() {
    use std::os::unix::fs::MetadataExt;
    let items = vec![Item::file("owned.txt", b"owned\n".to_vec())];
    let mut h = Home::new("64K", None, 1);
    h.unit(TENANT, "alpha", &items);
    let root = nix::unistd::geteuid().is_root();
    if root {
        std::os::unix::fs::chown(
            h.source("alpha").join("owned.txt"),
            Some(12_345),
            Some(12_346),
        )
        .unwrap();
    }
    h.stage("alpha");
    h.volume("RDR-A");
    h.write("RDR-A");
    let mut bay = h.bay("RDR-A", &[]);
    let (r, dest) = h.restore(
        "alpha",
        What::Unit,
        "RDR-A",
        &RestoreOptions::default(),
        &mut bay,
    );
    let report = r.expect("restored");
    assert_restored(&h, "alpha", dest.path(), &[]);
    assert_same_meta(&h.source("alpha"), dest.path());
    let src = fs::metadata(h.source("alpha").join("owned.txt")).unwrap();
    assert!(
        report
            .id_map
            .iter()
            .any(|row| row.kind == crate::idmap::IdKind::User && row.id == src.uid()),
        "{:?}",
        report.id_map
    );
    assert!(
        report
            .id_map
            .iter()
            .any(|row| row.kind == crate::idmap::IdKind::Group && row.id == src.gid()),
        "{:?}",
        report.id_map
    );
    let restored = fs::metadata(dest.path().join("owned.txt")).unwrap();
    if root {
        assert_eq!((restored.uid(), restored.gid()), (12_345, 12_346));
    } else {
        assert_eq!(restored.uid(), nix::unistd::geteuid().as_raw());
    }
}

/// Two versions of a unit on one tape: a dry run counts only the selected
/// version's slices that hold a member, names a version the tape does not
/// carry, and the CLI previews from the one volume that holds the unit.
fn two_versions() -> Home {
    let mut h = Home::new("64K", None, 1);
    h.unit(TENANT, "alpha", &files("big", 3, 50_000, 1));
    h.stage("alpha");
    for e in fs::read_dir(h.source("alpha")).unwrap().flatten() {
        if e.file_name().to_string_lossy().starts_with("big-") {
            fs::remove_file(e.path()).unwrap();
        }
    }
    fs::write(h.source("alpha").join("small.txt"), b"version two\n").unwrap();
    h.stage("alpha");
    h.volume("RDR-A");
    h.write("RDR-A");
    h
}

/// The slices of `unit`'s version `version` that hold a member.
fn member_slices(h: &Home, unit: &str, version: i64) -> usize {
    let rows = crate::db::stage_files::load(&h.conn, h.stage_set(unit, version)).unwrap();
    rows.iter()
        .flat_map(|r| r.slices(64 * 1024))
        .collect::<BTreeSet<u64>>()
        .len()
}

#[test]
fn a_dry_run_counts_only_the_selected_versions_slices() {
    let h = two_versions();
    let run = |v: Option<i64>| {
        super::restore_unit(
            &h.conn,
            &h.paths,
            &h.config,
            "alpha",
            "RDR-A",
            "/nonexistent/tapectl-dry-run-dest",
            "/nonexistent/tapectl-dry-run-nst",
            524288,
            v,
            true,
            &RestoreOptions::default(),
        )
    };
    let newest = run(None).unwrap();
    assert_eq!(
        (newest.version, newest.slices),
        (2, member_slices(&h, "alpha", 2))
    );
    let v1 = run(Some(1)).unwrap();
    assert_eq!((v1.version, v1.slices), (1, member_slices(&h, "alpha", 1)));
    assert!(
        v1.slices > newest.slices,
        "positive control: the versions differ"
    );
    let err = run(Some(3)).unwrap_err().to_string();
    assert!(err.contains("no version 3"), "{err}");
}

#[test]
fn a_dry_run_through_the_cli_with_no_volume_named_previews_from_the_only_holder() {
    use crate::cli::restore::{run, RestoreCommands};
    let h = two_versions();
    let command = RestoreCommands::Unit {
        unit: "alpha".into(),
        from: None,
        to: "/nonexistent/tapectl-dry-run-dest".into(),
        device: Some("/nonexistent/tapectl-dry-run-nst".into()),
        version: None,
        scratch: None,
        overwrite: false,
        no_space_check: false,
        no_file_check: false,
        key: Vec::new(),
        dry_run: true,
    };
    run(&h.conn, &h.paths, &h.config, &command, true, true)
        .expect("the dry run previews from RDR-A, the only holder");
}

/// A restore is judged by the check on disk and never by tar's exit status
/// (invariant 6): a tar that exits 0 having extracted nothing is an
/// INCOMPLETE restore naming every file, and `--no-file-check` is the one
/// way to skip the check.
#[test]
fn a_tar_that_exits_zero_having_restored_nothing_is_not_a_restore() {
    let mut h = home_with("alpha", &small_tree(), "64K", None);
    let dir = tempfile::TempDir::new().unwrap();
    // BusyBox tar's failure, measured: it reads and drops everything, and
    // exits 0 with files missing.
    let tar = crate::tar::stand_in(dir.path(), "empty-handed-tar", "cat > /dev/null\nexit 0");
    h.config.tar.binary = tar.to_string_lossy().into_owned();
    let mut bay = h.bay("RDR-A", &[]);
    let (r, dest) = h.restore(
        "alpha",
        What::Unit,
        "RDR-A",
        &RestoreOptions::default(),
        &mut bay,
    );
    let err = r.expect_err("nothing was restored").to_string();
    assert!(err.contains("INCOMPLETE"), "{err}");
    let rows = h.rows("alpha");
    let lost = lost_paths(&err);
    for row in rows
        .iter()
        .filter(|r| r.kind != crate::volume::filetable::Kind::Folder)
        .take(10)
    {
        assert!(lost.contains(&row.path), "{} is named: {err}", row.path);
    }
    assert!(listing(dest.path()).is_empty(), "nothing is there");
    // Skipping the check is the one way it is called whole.
    let skip = RestoreOptions {
        no_file_check: true,
        ..RestoreOptions::default()
    };
    let mut bay = h.bay("RDR-A", &[]);
    let (r, _dest) = h.restore("alpha", What::Unit, "RDR-A", &skip, &mut bay);
    r.expect("--no-file-check skips the check on disk");
}

/// What tar says is repeated, not dropped, and its exit status is recorded:
/// an extract that tar cannot finish (a non-empty folder where a file goes,
/// with `--overwrite`) is named by the check, tar's complaint is in the
/// report, and the `restores` row keeps tar's run.
#[test]
fn what_tar_says_is_repeated_and_recorded() {
    let h = home_with("alpha", &small_tree(), "64K", None);
    let dest = tempfile::TempDir::new().unwrap();
    let dest_dir = dest.path().to_string_lossy().into_owned();
    // A folder where a regular file goes.
    fs::create_dir_all(dest.path().join("alpha-02.bin/inside")).unwrap();
    fs::write(dest.path().join("alpha-02.bin/inside/x"), b"x").unwrap();
    let options = RestoreOptions {
        overwrite: true,
        ..RestoreOptions::default()
    };
    let mut bay = h.bay("RDR-A", &[]);
    let r = h.restore_into("alpha", What::Unit, "RDR-A", &options, &mut bay, &dest_dir);
    let err = r
        .expect_err("tar cannot put a file where a folder is")
        .to_string();
    assert!(err.contains("alpha-02.bin"), "{err}");
    let row = crate::volume::restore_record::rows(&h.conn)
        .unwrap()
        .pop()
        .expect("the restore's row");
    assert_eq!(row.outcome, "failed");
    assert!(
        row.tar_argv
            .as_deref()
            .is_some_and(|a| a.contains("--extract")
                && a.contains("--numeric-owner")
                && a.contains("--overwrite")
                && !a.contains("--ignore-zeros")),
        "{:?}",
        row.tar_argv
    );
    assert!(
        row.tar_exit_code.is_some_and(|c| c != 0),
        "{:?}",
        row.tar_exit_code
    );
    assert!(
        row.tar_stderr.as_deref().is_some_and(|e| !e.is_empty()),
        "tar's complaint is kept"
    );
    assert!(row
        .tar_version
        .as_deref()
        .is_some_and(|v| v.contains("tar")));
}

/// #594 4-8: tar that dies in the middle of a long member leaves the member's
/// partial file where tar made it. The member is listed as left out, and the
/// INCOMPLETE text says the files left out are not in the destination, so the
/// partial file must be removed as `abort_run` removes it for a run cut at an
/// unread slice. The stand-in takes the first slice and makes the member's
/// file, then exits under the feed.
#[test]
fn tar_dying_in_the_middle_of_a_long_member_leaves_no_partial_file() {
    let mut h = home_with(
        "alpha",
        &[Item::file("big.bin", super::fixture::noise(400_000, 5))],
        "64K",
        None,
    );
    assert!(h.num_slices("alpha") >= 4, "the member is a run");
    let dir = tempfile::TempDir::new().unwrap();
    let tar = crate::tar::stand_in(
        dir.path(),
        "dying-tar",
        "for a in \"$@\"; do case \"$a\" in\n\
         --version) echo 'tar (dying stand-in)'; exit 0;;\n\
         --directory=*) d=${a#--directory=};;\n\
         esac; done\n\
         echo partial > \"$d/big.bin\"\nhead -c 65536 > /dev/null\nexit 1",
    );
    h.config.tar.binary = tar.to_string_lossy().into_owned();
    let mut bay = h.bay("RDR-A", &[]);
    let (r, dest) = h.restore(
        "alpha",
        What::Unit,
        "RDR-A",
        &RestoreOptions::default(),
        &mut bay,
    );
    let err = r.expect_err("tar died").to_string();
    assert!(err.contains("INCOMPLETE"), "{err}");
    assert!(lost_paths(&err).contains("big.bin"), "{err}");
    assert!(
        fs::symlink_metadata(dest.path().join("big.bin")).is_err(),
        "the partial file is removed: {:?}",
        listing(dest.path())
    );
}

// ── #571: a size that cannot be allocated is a bad slice ──

/// A corrupt catalog row that records a slice's size as more than any host
/// holds makes that slice bad, by name, and leaves out exactly the members
/// that begin in it. The process is not aborted by the allocation, the
/// other slices' files are whole (ADR-0012 item 298). `column` is what the
/// row's corruption is: `size_bytes` (the decoded cut's buffer, and an
/// uncompressed slice's) or `encrypted_bytes` (a compressed slice's frame).
fn a_slice_whose_recorded_size_cannot_be_held_is_bad(
    compression: Option<&str>,
    column: &str,
    said: &str,
) {
    let items = files("many", 12, 20_000, 7);
    let h = home_with("alpha", &items, "64K", compression);
    assert!(h.num_slices("alpha") >= 3, "several slices");
    // The last slice: a size that no later slice's start disagrees with (the
    // catalog is checked against the file table before any tape is read).
    let n = h.num_slices("alpha");
    let held = members_in_slice(&h, "alpha", n as u64);
    assert!(!held.is_empty(), "the last slice holds members");
    let set = h.stage_set("alpha", 1);
    let absurd = i64::MAX / 4;
    h.conn
        .execute(
            &format!(
                "UPDATE stage_slices SET {column} = ?1 WHERE stage_set_id = ?2 AND slice_number = ?3"
            ),
            rusqlite::params![absurd, set, n],
        )
        .unwrap();
    let mut bay = h.bay("RDR-A", &[]);
    let options = RestoreOptions {
        no_space_check: true,
        ..RestoreOptions::default()
    };
    let (r, dest) = h.restore("alpha", What::Unit, "RDR-A", &options, &mut bay);
    let err = r.expect_err("a slice that cannot be held").to_string();
    assert!(err.contains("INCOMPLETE"), "{err}");
    assert!(err.contains(said), "{err}");
    assert!(err.contains(&format!("slice {n} is bad")), "{err}");
    // The reason has parentheses of its own, so read the paths by hand.
    let lost: BTreeSet<String> = err
        .lines()
        .filter(|l| l.starts_with("    ") && !l.starts_with("     "))
        .filter_map(|l| l.trim().split_once(" (").map(|(p, _)| p.to_string()))
        .collect();
    assert!(held.is_subset(&lost), "{held:?} of {lost:?}: {err}");
    assert!(
        lost.len() < items.len(),
        "only the files in that slice: {err}"
    );
    assert_all_but(&h, "alpha", dest.path(), &lost);
    assert!(
        listing(dest.path()).len() > 1,
        "positive control: the other slices' files are there"
    );
}

#[test]
fn an_uncompressed_slice_whose_recorded_size_cannot_be_held_is_bad() {
    a_slice_whose_recorded_size_cannot_be_held_is_bad(None, "size_bytes", "cannot be held in memory");
}

#[test]
fn a_compressed_slice_whose_recorded_cut_cannot_be_held_is_bad() {
    a_slice_whose_recorded_size_cannot_be_held_is_bad(Some("zstd"), "size_bytes", "cannot be held in memory");
}

#[test]
fn a_compressed_slice_whose_recorded_frame_cannot_be_held_is_bad() {
    a_slice_whose_recorded_size_cannot_be_held_is_bad(
        Some("zstd"),
        "encrypted_bytes",
        "cannot be held in memory",
    );
}

/// #594 4-7: an uncompressed slice reserves its recorded cut, so a corrupt
/// `encrypted_bytes` is not refused by `reserve`; the tolerant read of a sole
/// copy then counts every chunk past the tape file's real end as a hole, and
/// the hole's placeholder bytes grew the buffer without bound (an allocation
/// failure aborts the process). A hole may not run past what was reserved: the
/// slice is bad, by name (item 298).
#[test]
fn an_uncompressed_slice_whose_recorded_encrypted_size_runs_past_its_cut_is_bad() {
    a_slice_whose_recorded_size_cannot_be_held_is_bad(None, "encrypted_bytes", "more than");
}

/// The second name of a hard-linked symbolic link (#581, item 299): its row
/// is `l`, the whole restore makes both names symbolic links to one target,
/// and the check passes.
#[test]
fn both_names_of_a_hard_linked_symbolic_link_are_restored() {
    let items = vec![
        Item::file("target.txt", b"what they point at\n".to_vec()),
        Item::Symlink("s1".into(), "target.txt".into()),
        Item::Hard("s2".into(), "s1".into()),
    ];
    let h = home_with("alpha", &items, "64K", None);
    use std::os::unix::fs::MetadataExt;
    let src = h.source("alpha");
    assert_eq!(fs::symlink_metadata(src.join("s1")).unwrap().nlink(), 2);
    let rows = h.rows("alpha");
    for name in ["s1", "s2"] {
        let r = rows.iter().find(|r| r.path == name).unwrap();
        assert_eq!(r.kind, crate::volume::filetable::Kind::Symlink, "{name}");
        assert_eq!((r.size, r.sha256, &r.link), (0, None, &None), "{name}");
    }
    let mut bay = h.bay("RDR-A", &[]);
    let (r, dest) = h.restore(
        "alpha",
        What::Unit,
        "RDR-A",
        &RestoreOptions::default(),
        &mut bay,
    );
    r.expect("both names restore and the check passes");
    for name in ["s1", "s2"] {
        assert_eq!(
            fs::read_link(dest.path().join(name)).unwrap(),
            std::path::Path::new("target.txt"),
            "{name}"
        );
    }
    assert_restored(&h, "alpha", dest.path(), &[]);
}

/// A slice that holds the second name and not the first cannot make that name
/// (item 293's limit): the name is listed for the repair, never claimed.
#[test]
fn the_second_name_of_a_symbolic_link_is_listed_when_the_first_is_lost() {
    let items = vec![
        Item::Symlink("a-s1".into(), "b-fill.bin".into()),
        Item::file("b-fill.bin", noise_bytes(100_000, 9)),
        Item::Hard("c-s2".into(), "a-s1".into()),
    ];
    let mut h = home_with("alpha", &items, "64K", None);
    let rows = h.rows("alpha");
    let first = rows.iter().find(|r| r.path == "a-s1").unwrap();
    let second = rows.iter().find(|r| r.path == "c-s2").unwrap();
    assert_ne!(first.slice, second.slice, "the names lie in two slices");
    let pos = h.position("RDR-A", "alpha", first.slice as i64);
    h.flip("RDR-A", pos, 2_000);
    let mut bay = h.bay("RDR-A", &[]);
    let (r, dest) = h.restore(
        "alpha",
        What::Unit,
        "RDR-A",
        &RestoreOptions::default(),
        &mut bay,
    );
    let err = r.expect_err("the first name is lost").to_string();
    let lost = lost_paths(&err);
    assert!(lost.contains("a-s1") && lost.contains("c-s2"), "{err}");
    assert!(
        fs::symlink_metadata(dest.path().join("c-s2")).is_err(),
        "no claim on a name that was not made"
    );
}
