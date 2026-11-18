//! `stage create` on GNU tar (the plan's T4.5; `volume-format-v3.md` §3):
//! tests through `stage_create` itself.

use std::fs;
use std::os::unix::ffi::OsStrExt;

use tempfile::TempDir;

use super::tests::setup_unit_with_excludes;
use super::*;

/// T4.4's note, #542: a unit holding a socket stages, and `stage create`
/// says the socket was left out (it is not in the snapshot, so tar is never
/// given it), as `snapshot create` does.
#[test]
fn a_unit_holding_a_socket_stages_and_stage_create_names_it_left_out() {
    let tmp = TempDir::new().unwrap();
    let (conn, paths, config, src) = setup_unit_with_excludes(&tmp, vec![]);
    fs::write(src.join("keep.txt"), b"kept").unwrap();
    fs::create_dir_all(src.join("d")).unwrap();
    let _listener = std::os::unix::net::UnixListener::bind(src.join("d/s.sock")).unwrap();

    let snap_id = snapshot_create(&conn, "unit1", &config).unwrap();
    let mut notices = Vec::new();
    stage_create_reporting(&conn, &paths, &config, snap_id, false, &mut notices).unwrap();
    let notices = String::from_utf8(notices).unwrap();
    assert!(
        notices.contains("left out: d/s.sock (socket)"),
        "stage create names the socket left out: {notices}"
    );
}

/// #542: what a pattern left out is named by `stage create` too.
#[test]
fn stage_create_names_what_a_pattern_left_out() {
    let tmp = TempDir::new().unwrap();
    let (conn, paths, config, src) = setup_unit_with_excludes(&tmp, vec!["*.tmp".to_string()]);
    fs::write(src.join("keep.txt"), b"kept").unwrap();
    fs::write(src.join("scratch.tmp"), b"not kept").unwrap();

    let snap_id = snapshot_create(&conn, "unit1", &config).unwrap();
    let mut notices = Vec::new();
    stage_create_reporting(&conn, &paths, &config, snap_id, false, &mut notices).unwrap();
    let notices = String::from_utf8(notices).unwrap();
    assert!(
        notices.contains("left out: scratch.tmp (pattern *.tmp"),
        "stage create names what the pattern left out: {notices}"
    );
}

/// #569: a file whose name is not valid UTF-8 cannot be carried yet (the
/// list given to tar is written from the catalog's text). It is refused
/// before anything is staged, by the snapshot that would record it, naming
/// the unit and the path.
#[test]
fn a_name_that_is_not_utf8_is_refused_naming_the_unit_and_the_path() {
    let tmp = TempDir::new().unwrap();
    let (conn, _paths, config, src) = setup_unit_with_excludes(&tmp, vec![]);
    fs::write(src.join("plain.txt"), b"plain").unwrap();
    let name = std::ffi::OsStr::from_bytes(b"caf\xe9.txt");
    fs::write(src.join(name), b"latin-1 name").unwrap();

    let err =
        snapshot_create(&conn, "unit1", &config).expect_err("a name that is not UTF-8 is refused");
    let msg = err.to_string();
    assert!(msg.contains("unit \"unit1\""), "names the unit: {msg}");
    assert!(msg.contains("caf\\xE9.txt"), "names the path: {msg}");
    assert!(msg.contains("UTF-8"), "says why: {msg}");
    let n: i64 = conn
        .query_row("SELECT COUNT(*) FROM snapshots", [], |r| r.get(0))
        .unwrap();
    assert_eq!(n, 0, "nothing was recorded");
}

/// #562: `slice_size` is a multiple of 512 wherever it is set. A
/// `[defaults]` value that is not is refused, naming the key.
#[test]
fn a_slice_size_that_is_not_a_multiple_of_512_is_refused_naming_the_key() {
    let tmp = TempDir::new().unwrap();
    let (conn, paths, mut config, src) = setup_unit_with_excludes(&tmp, vec![]);
    config.defaults.slice_size = "1000000".to_string();
    fs::write(src.join("f.txt"), b"content").unwrap();

    let snap_id = snapshot_create(&conn, "unit1", &config).unwrap();
    let err = stage_create(&conn, &paths, &config, snap_id, false)
        .expect_err("a slice_size of 1000000 bytes is not a multiple of 512");
    let msg = err.to_string();
    assert!(msg.contains("slice_size"), "names the key: {msg}");
    assert!(msg.contains("512"), "says the rule: {msg}");
    let n: i64 = conn
        .query_row("SELECT COUNT(*) FROM stage_sets", [], |r| r.get(0))
        .unwrap();
    assert_eq!(n, 0, "nothing was staged");
}
