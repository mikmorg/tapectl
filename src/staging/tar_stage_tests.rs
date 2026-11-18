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
    let id = stage_create_reporting(&conn, &paths, &config, snap_id, false, &mut notices).unwrap();
    let notices = String::from_utf8(notices).unwrap();
    assert!(
        notices.contains("left out: d/s.sock (socket)"),
        "stage create names the socket left out: {notices}"
    );
    let members: Vec<String> = crate::db::stage_files::load(&conn, id)
        .unwrap()
        .into_iter()
        .map(|r| r.path)
        .collect();
    assert!(members.contains(&"keep.txt".to_string()), "{members:?}");
    assert!(members.contains(&"d".to_string()), "{members:?}");
    assert!(
        !members.iter().any(|p| p.ends_with("s.sock")),
        "the socket is no member of the stream: {members:?}"
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

// ---------- the Verify paragraph of T4.5, through `stage create` ----------

use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use rusqlite::params;
use sha2::Digest;

use crate::volume::filetable::{Kind, Row};

/// One staged slice: its number, `continues`, and its row's figures.
#[derive(Debug, Clone)]
struct Staged {
    number: i64,
    continues: bool,
    size: i64,
    sha_plain: String,
    path: String,
}

fn slices_of(conn: &Connection, id: i64) -> Vec<Staged> {
    conn.prepare(
        "SELECT slice_number, continues, size_bytes, sha256_plain, staging_path
           FROM stage_slices WHERE stage_set_id = ?1 ORDER BY slice_number",
    )
    .unwrap()
    .query_map(params![id], |r| {
        Ok(Staged {
            number: r.get(0)?,
            continues: r.get(1)?,
            size: r.get(2)?,
            sha_plain: r.get(3)?,
            path: r.get(4)?,
        })
    })
    .unwrap()
    .collect::<std::result::Result<_, _>>()
    .unwrap()
}

/// A staged slice's age payload (the cut, or the packed frame).
fn decrypt(conn: &Connection, paths: &TapectlPaths, path: &str) -> Vec<u8> {
    let ids = crate::crypto::keys::load_tenant_identities(conn, &paths.keys_dir, "alice").unwrap();
    assert!(!ids.is_empty(), "alice has keys");
    let d = age::Decryptor::new(fs::File::open(path).unwrap()).unwrap();
    let mut r = d
        .decrypt(ids.iter().map(|i| i as &dyn age::Identity))
        .unwrap();
    let mut out = Vec::new();
    r.read_to_end(&mut out).unwrap();
    out
}

fn sha_hex(b: &[u8]) -> String {
    format!("{:x}", sha2::Sha256::digest(b))
}

/// `tar -x` of `stream` into `dest` (created), returning tar's exit and
/// what it said.
fn tar_extract(stream: &[u8], dest: &Path) -> (i32, String) {
    fs::create_dir_all(dest).unwrap();
    let mut child = std::process::Command::new("tar")
        .args(["-x", "-f", "-", "-C"])
        .arg(dest)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    let data = stream.to_vec();
    let feeder = std::thread::spawn(move || {
        let _ = stdin.write_all(&data);
    });
    let out = child.wait_with_output().unwrap();
    feeder.join().unwrap();
    (
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

/// The regular files under `dir`, relative, sorted.
fn files_under(dir: &Path) -> Vec<String> {
    let mut v: Vec<String> = walkdir::WalkDir::new(dir)
        .into_iter()
        .flatten()
        .filter(|e| e.file_type().is_file())
        .map(|e| {
            e.path()
                .strip_prefix(dir)
                .unwrap()
                .to_string_lossy()
                .into_owned()
        })
        .collect();
    v.sort();
    v
}

/// Set `p`'s mtime to a whole second, so GNU tar writes no extended
/// header for it (a short name, no ACL, no xattr): its member is its
/// header and its padded data.
fn whole_second_mtime(p: &Path) {
    let f = fs::File::options().write(true).open(p).unwrap();
    f.set_modified(std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_700_000_000))
        .unwrap();
}

fn mixed_bytes(n: usize, seed: u64) -> Vec<u8> {
    let mut x = seed | 1;
    let mut v = Vec::with_capacity(n);
    while v.len() < n {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        if x % 4 == 0 {
            v.extend_from_slice(b"some words that repeat, ");
        } else {
            v.push(x as u8);
        }
    }
    v.truncate(n);
    v
}

/// The cut (D3, rule 13), through `stage create`: a member of exactly
/// `slice_size` fills an empty slice; one a block larger is a run of two
/// slices, the second stating `continues`; the tail after the run begins a
/// slice of its own. Every slice that is not part of a run is read alone by
/// the real tar at exit 0 and gives exactly the files the table says begin
/// in it; the run's slices, joined, are read alone; the slices joined are
/// tar's stream byte for byte.
#[test]
fn the_cut_through_stage_create_holds_whole_members_and_a_run() {
    let tmp = TempDir::new().unwrap();
    let (conn, paths, mut config, src) = setup_unit_with_excludes(&tmp, vec![]);
    const S: usize = 64 * 1024;
    config.defaults.slice_size = S.to_string();
    let small: Vec<(&str, usize)> = vec![("a1.txt", 3000), ("a2.txt", 7000)];
    for (n, len) in &small {
        fs::write(src.join(n), mixed_bytes(*len, 1)).unwrap();
    }
    // A member is its 512-byte header and its padded data.
    fs::write(src.join("b-exact.bin"), mixed_bytes(S - 512, 2)).unwrap();
    fs::write(src.join("c-run.bin"), mixed_bytes(S, 3)).unwrap();
    for n in ["a1.txt", "a2.txt", "b-exact.bin", "c-run.bin"] {
        whole_second_mtime(&src.join(n));
    }
    let snap = snapshot_create(&conn, "unit1", &config).unwrap();
    let id = stage_create(&conn, &paths, &config, snap, false).unwrap();

    let rows = crate::db::stage_files::load(&conn, id).unwrap();
    let row = |p: &str| rows.iter().find(|r| r.path == p).unwrap().clone();
    assert_eq!(
        row("b-exact.bin").len(),
        S as u64,
        "fixture: exactly slice_size"
    );
    assert_eq!(
        row("c-run.bin").len(),
        S as u64 + 512,
        "fixture: a block more"
    );
    let slices = slices_of(&conn, id);
    let exact = row("b-exact.bin").slice as i64;
    let run = row("c-run.bin").slice as i64;
    assert_eq!(run, exact + 1);
    let by_n = |n: i64| slices.iter().find(|s| s.number == n).unwrap().clone();
    assert_eq!(
        by_n(exact).size,
        S as i64,
        "the exact member fills its slice alone"
    );
    assert!(!by_n(exact).continues);
    assert_eq!(by_n(run).size, S as i64);
    assert!(
        !by_n(run).continues,
        "a run's first slice states no continues"
    );
    assert_eq!(by_n(run + 1).size, 512);
    assert!(
        by_n(run + 1).continues,
        "the run's second slice continues it"
    );
    let tail = by_n(run + 2);
    assert!(!tail.continues);
    assert_eq!(
        tail.number as usize,
        slices.len(),
        "the tail after the run is the last slice"
    );

    // Every cut decrypts to its recorded size and sha256.
    let cuts: Vec<Vec<u8>> = slices
        .iter()
        .map(|s| {
            let c = decrypt(&conn, &paths, &s.path);
            assert_eq!(c.len() as i64, s.size);
            assert_eq!(sha_hex(&c), s.sha_plain);
            c
        })
        .collect();
    let joined: Vec<u8> = cuts.concat();
    let stream_bytes: i64 = conn
        .query_row(
            "SELECT stream_bytes FROM stage_sets WHERE id = ?1",
            params![id],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(joined.len() as i64, stream_bytes);

    // The slices joined are tar's stream, byte for byte: the same tar, the
    // same list, the same files.
    let list_dir = TempDir::new().unwrap();
    let names: Vec<String> = rows.iter().map(|r| r.path.clone()).collect();
    let list = crate::tar::write_list(list_dir.path(), &names).unwrap();
    let mut again = crate::tar::spawn_archive(&crate::tar::CreateParams {
        binary: "tar",
        root: &src,
        list: &list,
        xattrs: true,
        acls: true,
    })
    .unwrap();
    let mut stream = Vec::new();
    again.take_stdout().read_to_end(&mut stream).unwrap();
    assert!(again.finish().unwrap().is_clean());
    assert_eq!(
        sha_hex(&joined),
        sha_hex(&stream),
        "the slices joined are the stream"
    );

    // Each slice outside the run alone, through the real tar.
    for s in &slices {
        if s.number == run || s.number == run + 1 {
            continue;
        }
        let dest = tmp.path().join(format!("alone-{}", s.number));
        let (code, said) = tar_extract(&cuts[(s.number - 1) as usize], &dest);
        assert_eq!(code, 0, "slice {} alone: {said}", s.number);
        let mut want: Vec<String> = rows
            .iter()
            .filter(|r| r.slice as i64 == s.number && r.kind == Kind::File)
            .map(|r| r.path.clone())
            .collect();
        want.sort();
        assert_eq!(files_under(&dest), want, "slice {}", s.number);
    }
    // The run's slices joined, alone.
    let run_bytes = [cuts[(run - 1) as usize].clone(), cuts[run as usize].clone()].concat();
    let dest = tmp.path().join("run");
    let (code, said) = tar_extract(&run_bytes, &dest);
    assert_eq!(code, 0, "the run joined, alone: {said}");
    assert_eq!(files_under(&dest), vec!["c-run.bin".to_string()]);
    assert_eq!(
        sha_hex(&fs::read(dest.join("c-run.bin")).unwrap()),
        sha_hex(&fs::read(src.join("c-run.bin")).unwrap())
    );
}

/// The host's own decompressor for `algorithm`, given `frame`.
fn host_decode(algorithm: &str, frame: &[u8]) -> Vec<u8> {
    let (prog, args): (&str, &[&str]) = match algorithm {
        "gzip" => ("gzip", &["-dc"]),
        "xz" => ("xz", &["-dc"]),
        _ => ("zstd", &["-dcq"]),
    };
    let mut child = std::process::Command::new(prog)
        .args(args)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .unwrap_or_else(|e| panic!("{prog} must be installed to read a {algorithm} slice: {e}"));
    let mut stdin = child.stdin.take().unwrap();
    let data = frame.to_vec();
    let feeder = std::thread::spawn(move || stdin.write_all(&data));
    let out = child.wait_with_output().unwrap();
    feeder.join().unwrap().unwrap();
    out.stdout
}

/// Item 280 (D15), for each algorithm carried: a unit of several slices
/// that holds a file larger than a slice is staged packed; every slice
/// decrypts to one frame the host's own tool decodes to the cut's
/// `sha256_plain` (the decompressor's exit is not the verdict, the hash
/// is); the staging records the algorithm and its level; a level set is
/// recorded; and a unit with `compression` unset beside it is staged
/// unpacked.
#[test]
fn each_algorithm_packs_every_slice_alone_and_the_hosts_tool_reads_it() {
    for (algorithm, level, want_level) in [
        ("gzip", None, 6),
        ("xz", Some(1), 1),
        ("zstd", Some(19), 19),
    ] {
        let tmp = TempDir::new().unwrap();
        let (conn, paths, mut config, src) = setup_unit_with_excludes(&tmp, vec![]);
        config.defaults.slice_size = (64 * 1024).to_string();
        config.defaults.compression = algorithm.to_string();
        config.defaults.compression_level = level;
        fs::write(src.join("big.txt"), mixed_bytes(200 * 1024, 7)).unwrap();
        for i in 0..20 {
            fs::write(src.join(format!("s{i:02}.txt")), mixed_bytes(5000, i)).unwrap();
        }
        let snap = snapshot_create(&conn, "unit1", &config).unwrap();
        let id = stage_create(&conn, &paths, &config, snap, false).unwrap();

        let (compression, recorded_level): (String, Option<i64>) = conn
            .query_row(
                "SELECT compression, compression_level FROM stage_sets WHERE id = ?1",
                params![id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(compression, algorithm);
        assert_eq!(recorded_level, Some(want_level), "{algorithm}");
        let slices = slices_of(&conn, id);
        assert!(slices.len() >= 4, "{algorithm}: several slices");
        assert!(slices.iter().any(|s| s.continues), "{algorithm}: a run");
        let mut packed_total = 0;
        let mut cuts = Vec::new();
        for s in &slices {
            let frame = decrypt(&conn, &paths, &s.path);
            packed_total += frame.len();
            let cut = host_decode(algorithm, &frame);
            assert_eq!(cut.len() as i64, s.size, "{algorithm} slice {}", s.number);
            assert_eq!(sha_hex(&cut), s.sha_plain, "{algorithm} slice {}", s.number);
            cuts.push(cut);
        }
        let cut_total: i64 = slices.iter().map(|s| s.size).sum();
        assert!((packed_total as i64) < cut_total, "{algorithm}: words pack");
        let dest = tmp.path().join("all");
        let (code, said) = tar_extract(&cuts.concat(), &dest);
        assert_eq!(code, 0, "{said}");
        assert_eq!(
            sha_hex(&fs::read(dest.join("big.txt")).unwrap()),
            sha_hex(&fs::read(src.join("big.txt")).unwrap())
        );

        // Beside it, a unit with `compression` unset: its slices are cuts.
        let mut plain_cfg = config.clone();
        plain_cfg.defaults.compression = "none".to_string();
        plain_cfg.defaults.compression_level = None;
        fs::write(src.join("later.txt"), b"a second version").unwrap();
        let snap2 = snapshot_create(&conn, "unit1", &plain_cfg).unwrap();
        let id2 = stage_create(&conn, &paths, &plain_cfg, snap2, false).unwrap();
        let (c2, l2): (String, Option<i64>) = conn
            .query_row(
                "SELECT compression, compression_level FROM stage_sets WHERE id = ?1",
                params![id2],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!((c2.as_str(), l2), ("none", None));
        for s in slices_of(&conn, id2) {
            let cut = decrypt(&conn, &paths, &s.path);
            assert_eq!(sha_hex(&cut), s.sha_plain, "an unpacked slice is its cut");
        }
    }
}

/// Bytes that do not shrink, packed: each slice's frame is a little larger
/// than its cut and within the stated worst growth, and still decodes to it.
#[test]
fn bytes_that_do_not_shrink_are_staged_packed_within_the_stated_growth() {
    for algorithm in ["gzip", "xz", "zstd"] {
        let tmp = TempDir::new().unwrap();
        let (conn, paths, mut config, src) = setup_unit_with_excludes(&tmp, vec![]);
        config.defaults.slice_size = (64 * 1024).to_string();
        config.defaults.compression = algorithm.to_string();
        let mut noise = vec![0u8; 150 * 1024];
        let mut x: u64 = 0x2545_F491_4F6C_DD1D;
        for b in noise.iter_mut() {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            *b = x as u8;
        }
        fs::write(src.join("noise.bin"), &noise).unwrap();
        let snap = snapshot_create(&conn, "unit1", &config).unwrap();
        let id = stage_create(&conn, &paths, &config, snap, false).unwrap();
        for s in slices_of(&conn, id) {
            let frame = decrypt(&conn, &paths, &s.path);
            assert!(
                frame.len() as u64 <= super::files::packed_bound(64 * 1024),
                "{algorithm} slice {}: {} bytes",
                s.number,
                frame.len()
            );
            assert_eq!(sha_hex(&host_decode(algorithm, &frame)), s.sha_plain);
        }
    }
}

/// Item 287: a level outside its algorithm's range is refused, naming the
/// key, before anything is staged; `config load` refuses it in `[defaults]`
/// too.
#[test]
fn a_level_out_of_range_is_refused_naming_the_key() {
    let tmp = TempDir::new().unwrap();
    let (conn, paths, mut config, src) = setup_unit_with_excludes(&tmp, vec![]);
    fs::write(src.join("f.txt"), b"content").unwrap();
    config.defaults.compression = "zstd".to_string();
    config.defaults.compression_level = Some(22);
    let snap = snapshot_create(&conn, "unit1", &config).unwrap();
    let err = stage_create(&conn, &paths, &config, snap, false)
        .expect_err("zstd's levels are 1 to 19")
        .to_string();
    assert!(err.contains("compression_level = 22"), "{err}");
    assert!(err.contains("zstd") && err.contains("1 to 19"), "{err}");
    let n: i64 = conn
        .query_row("SELECT COUNT(*) FROM stage_sets", [], |r| r.get(0))
        .unwrap();
    assert_eq!(n, 0, "nothing was staged");

    let cfg = tmp.path().join("config.toml");
    fs::write(
        &cfg,
        "[defaults]\ncompression = \"gzip\"\ncompression_level = 0\n",
    )
    .unwrap();
    let e = crate::config::Config::load(&cfg).unwrap_err().to_string();
    assert!(e.contains("defaults.compression_level = 0"), "{e}");
    fs::write(
        &cfg,
        "[defaults]\ncompression = \"gzip\"\ncompression_level = 9\n",
    )
    .unwrap();
    assert_eq!(
        crate::config::Config::load(&cfg)
            .unwrap()
            .defaults
            .compression_level,
        Some(9)
    );
}
