//! A name that is not UTF-8 (ADR-0012 item 297; #569): a unit whose files are
//! named by bytes is staged by the real `stage create`, written by the real
//! `volume write`, and restored by tapectl's own readers, and every name
//! comes back byte for byte. The names are real ones on this host's
//! filesystem (Linux takes any byte but `/` and NUL).

use std::fs;
use std::os::unix::fs::MetadataExt;

use super::fixture::{assert_restored, listing, os, Home, Item, What};
use super::{RestoreOptions, RestoreReport};
use crate::error::Result;

const UNIT: &str = "u8-unit";
const TENANT: &str = super::fixture::TENANT;

/// What the unit holds: a Latin-1 file, a folder so named with a file in it,
/// a hard link and a symbolic link whose target is such a name, and UTF-8
/// names that hold a backslash, a TAB and an LF (and one with `\xe9` as TEXT:
/// the two escapes must be told apart).
fn items() -> Vec<Item> {
    vec![
        Item::FileB(b"caf\xe9.txt".to_vec(), b"latin one".to_vec()),
        Item::DirB(b"d\xe9".to_vec()),
        Item::FileB(b"d\xe9/in\xff.bin".to_vec(), b"inside a folder".to_vec()),
        Item::HardB(b"h\xe9".to_vec(), b"caf\xe9.txt".to_vec()),
        Item::SymlinkB(b"l\xe9".to_vec(), b"caf\xe9.txt".to_vec()),
        Item::SymlinkB(b"to-bad-target".to_vec(), b"\xfe\xfdgone".to_vec()),
        Item::file("back\\slash.txt", b"a backslash".to_vec()),
        Item::file("literal\\xe9.txt", b"text, not a byte".to_vec()),
        Item::file("tab\there.txt", b"a tab".to_vec()),
        Item::file("lf\nname.txt", b"a newline".to_vec()),
        Item::file("plain-\u{e9}.txt", b"utf-8".to_vec()),
    ]
}

fn written(label: &str) -> Home {
    let mut h = Home::new("64K", None, 1);
    h.unit(TENANT, UNIT, &items());
    h.stage(UNIT);
    h.volume(label);
    h.write(label);
    h
}

fn restore(h: &Home, label: &str, what: What<'_>) -> (Result<RestoreReport>, tempfile::TempDir) {
    let mut bay = h.bay(label, &[]);
    h.restore(UNIT, what, label, &RestoreOptions::default(), &mut bay)
}

/// The catalog and the file table keep each name in the one escaped form,
/// and a name that was UTF-8 with no backslash is as it was.
#[test]
fn the_catalog_and_the_file_table_hold_every_name_escaped() {
    let h = written("U8-CAT");
    let paths: Vec<String> = h
        .conn
        .prepare("SELECT path FROM paths ORDER BY path")
        .unwrap()
        .query_map([], |r| r.get(0))
        .unwrap()
        .collect::<std::result::Result<_, _>>()
        .unwrap();
    for want in [
        "caf\\xe9.txt",
        "d\\xe9",
        "d\\xe9/in\\xff.bin",
        "h\\xe9",
        "l\\xe9",
        // A backslash is written escaped in the catalog (§1.5): `a\b` reads `a\\b`.
        "back\\\\slash.txt",
        // Text that looks like a byte escape is not one.
        "literal\\\\xe9.txt",
        "tab\\there.txt",
        "lf\\nname.txt",
        "plain-\u{e9}.txt",
    ] {
        assert!(paths.iter().any(|p| p == want), "{want:?} not in {paths:?}");
    }
    let rows = h.rows(UNIT);
    for want in ["caf\\xe9.txt", "d\\xe9/in\\xff.bin", "back\\\\slash.txt"] {
        assert!(rows.iter().any(|r| r.path == want), "{want:?} in the table");
    }
    // The symlink targets are in the stream; the catalog's copy is escaped.
    let targets: Vec<String> = h
        .conn
        .prepare("SELECT link_target FROM file_versions WHERE link_target IS NOT NULL ORDER BY 1")
        .unwrap()
        .query_map([], |r| r.get(0))
        .unwrap()
        .collect::<std::result::Result<_, _>>()
        .unwrap();
    assert!(targets.contains(&"caf\\xe9.txt".to_string()), "{targets:?}");
    assert!(
        targets.contains(&"\\xfe\\xfdgone".to_string()),
        "{targets:?}"
    );
}

/// Staged, written and restored by `restore unit`: every name comes back
/// byte for byte, and the hard link is still a link.
#[test]
fn restore_unit_gives_every_name_back_byte_for_byte() {
    let h = written("U8-UNIT");
    let (result, dest) = restore(&h, "U8-UNIT", What::Unit);
    result.expect("a clean restore");
    assert_restored(&h, UNIT, dest.path(), &[]);
    // The listing keys are escaped from the names' bytes: it is a byte-exact
    // comparison, the non-UTF-8 names included. Positive control: they are there.
    let got = listing(dest.path());
    assert!(got.contains_key("caf\\xe9.txt"), "{:?}", got.keys());
    assert_eq!(
        fs::read(dest.path().join(os(b"d\xe9/in\xff.bin"))).unwrap(),
        b"inside a folder"
    );
    assert_eq!(
        fs::read_link(dest.path().join(os(b"to-bad-target"))).unwrap(),
        std::path::PathBuf::from(os(b"\xfe\xfdgone"))
    );
    let (a, b) = (
        fs::metadata(dest.path().join(os(b"caf\xe9.txt"))).unwrap(),
        fs::metadata(dest.path().join(os(b"h\xe9"))).unwrap(),
    );
    assert_eq!(a.ino(), b.ino(), "the hard link is still a hard link");
    // The name with a backslash and the name with `\xe9` as text are two files.
    assert_eq!(
        fs::read(dest.path().join("back\\slash.txt")).unwrap(),
        b"a backslash"
    );
    assert_eq!(
        fs::read(dest.path().join("literal\\xe9.txt")).unwrap(),
        b"text, not a byte"
    );
}

/// `restore file` takes the name in the form `catalog list` prints, and puts
/// the file, named by its bytes, in the destination.
#[test]
fn restore_file_finds_a_name_by_its_escaped_form_and_places_its_bytes() {
    let h = written("U8-FILE");
    for (escaped, placed, bytes) in [
        ("caf\\xe9.txt", &b"caf\xe9.txt"[..], &b"latin one"[..]),
        ("d\\xe9/in\\xff.bin", b"in\xff.bin", b"inside a folder"),
        ("back\\\\slash.txt", b"back\\slash.txt", b"a backslash"),
        ("lf\\nname.txt", b"lf\nname.txt", b"a newline"),
    ] {
        let (result, dest) = restore(&h, "U8-FILE", What::File(escaped));
        result.unwrap_or_else(|e| panic!("{escaped}: {e}"));
        assert_eq!(fs::read(dest.path().join(os(placed))).unwrap(), bytes);
    }
    // A name that is not there is refused before the tape moves, named escaped.
    let (result, _dest) = restore(&h, "U8-FILE", What::File("caf\\xe8.txt"));
    let err = result.unwrap_err().to_string();
    assert!(err.contains("caf\\xe8.txt"), "{err}");
}
