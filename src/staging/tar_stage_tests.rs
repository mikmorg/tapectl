//! `stage create` on GNU tar (the plan's T4.5; `volume-format-v3.md` §3):
//! tests through `stage_create` itself.

#[cfg(test)]
mod tests {

    use std::fs;
    use std::os::unix::ffi::OsStrExt;

    use tempfile::TempDir;

    use super::super::tests::setup_unit_with_excludes;
    use super::super::*;

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
        let id =
            stage_create_reporting(&conn, &paths, &config, snap_id, false, &mut notices).unwrap();
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

        let err = snapshot_create(&conn, "unit1", &config)
            .expect_err("a name that is not UTF-8 is refused");
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
        let ids =
            crate::crypto::keys::load_tenant_identities(conn, &paths.keys_dir, "alice").unwrap();
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
            if x.is_multiple_of(4) {
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
            .unwrap_or_else(|e| {
                panic!("{prog} must be installed to read a {algorithm} slice: {e}")
            });
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
                    frame.len() as u64 <= super::super::files::packed_bound(64 * 1024),
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

    fn run_ok(cmd: &str, args: &[&std::ffi::OsStr]) -> bool {
        std::process::Command::new(cmd)
            .args(args)
            .status()
            .is_ok_and(|s| s.success())
    }

    /// Through `stage create`: a unit holding a hard-link pair, a symbolic
    /// link, a FIFO, a file with an ACL naming an id the host knows (0,
    /// `root`), a file with a `user.` attribute, and a long name. The file
    /// table's rows match the sha256 of every file on disk; each file's range
    /// of the stream, given to the real tar with tar's two zero blocks, makes
    /// that file; the id map holds the owner, the group and the ACL's id with
    /// the names the host gives them.
    #[test]
    fn a_unit_of_every_kind_stages_and_its_table_restores_each_file_by_range() {
        use std::ffi::OsStr;
        use std::os::unix::fs::MetadataExt;
        let tmp = TempDir::new().unwrap();
        let (conn, paths, mut config, src) = setup_unit_with_excludes(&tmp, vec![]);
        config.defaults.slice_size = (32 * 1024).to_string();
        fs::create_dir_all(src.join("d/e")).unwrap();
        fs::write(src.join("d/first.txt"), mixed_bytes(9000, 11)).unwrap();
        fs::hard_link(src.join("d/first.txt"), src.join("z-second.txt")).unwrap();
        std::os::unix::fs::symlink("d/first.txt", src.join("link")).unwrap();
        nix::unistd::mkfifo(&src.join("d/e/fifo"), nix::sys::stat::Mode::S_IRWXU).unwrap();
        let long = format!("d/e/{}.bin", "n".repeat(180));
        fs::write(src.join(&long), mixed_bytes(70_000, 12)).unwrap();
        fs::write(src.join("acl.txt"), b"a file with an ACL").unwrap();
        fs::write(src.join("xattr.txt"), b"a file with a user attribute").unwrap();
        let acl = run_ok(
            "setfacl",
            &[
                OsStr::new("-m"),
                OsStr::new("u:0:r"),
                src.join("acl.txt").as_os_str(),
            ],
        );
        let xattr = run_ok(
            "setfattr",
            &[
                OsStr::new("-n"),
                OsStr::new("user.tapectl-test"),
                OsStr::new("-v"),
                OsStr::new("v1"),
                src.join("xattr.txt").as_os_str(),
            ],
        );
        assert!(acl, "setfacl must work on the test's file system");
        assert!(xattr, "setfattr must work on the test's file system");

        let snap = snapshot_create(&conn, "unit1", &config).unwrap();
        let id = stage_create(&conn, &paths, &config, snap, false).unwrap();
        let rows = crate::db::stage_files::load(&conn, id).unwrap();
        let get = |p: &str| -> Row {
            rows.iter()
                .find(|r| r.path == p)
                .unwrap_or_else(|| {
                    panic!(
                        "{p} in {:?}",
                        rows.iter().map(|r| &r.path).collect::<Vec<_>>()
                    )
                })
                .clone()
        };
        assert_eq!(get("d").kind, Kind::Folder);
        assert_eq!(get("link").kind, Kind::Symlink);
        assert_eq!(get("d/e/fifo").kind, Kind::Fifo);
        let link = get("z-second.txt");
        assert_eq!(link.kind, Kind::HardLink);
        assert_eq!(link.link.as_deref(), Some("d/first.txt"));
        assert_eq!(get(&long).kind, Kind::File);

        // Every hashed row is the sha256 of the file on disk.
        let mut checked = 0;
        for r in rows.iter().filter(|r| r.kind.hashed()) {
            let on_disk = fs::read(src.join(&r.path)).unwrap();
            assert_eq!(
                r.sha256.unwrap(),
                <[u8; 32]>::from(sha2::Sha256::digest(&on_disk)),
                "{}",
                r.path
            );
            assert_eq!(r.size, on_disk.len() as u64, "{}", r.path);
            checked += 1;
        }
        assert!(checked >= 6, "positive control: {checked} files checked");

        // Each file from its own range of the stream, with the real tar.
        let stream: Vec<u8> = slices_of(&conn, id)
            .iter()
            .map(|s| decrypt(&conn, &paths, &s.path))
            .collect::<Vec<_>>()
            .concat();
        for r in rows.iter().filter(|r| r.kind == Kind::File) {
            let mut piece = stream[r.start as usize..r.end as usize].to_vec();
            piece.extend_from_slice(&[0u8; 1024]);
            let dest = tmp.path().join(format!("one-{}", r.start));
            let (code, said) = tar_extract(&piece, &dest);
            assert_eq!(code, 0, "{}: {said}", r.path);
            assert_eq!(
                sha_hex(&fs::read(dest.join(&r.path)).unwrap()),
                sha_hex(&fs::read(src.join(&r.path)).unwrap()),
                "{} from its range",
                r.path
            );
        }
        // The whole stream keeps the ACL, the attribute, the link and the pair.
        let all = tmp.path().join("all");
        let mut child = std::process::Command::new("tar")
            .args([
                "-x",
                "--acls",
                "--xattrs",
                "--xattrs-include=user.*",
                "-f",
                "-",
                "-C",
            ])
            .arg({
                fs::create_dir_all(&all).unwrap();
                &all
            })
            .stdin(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        child.stdin.take().unwrap().write_all(&stream).unwrap();
        assert!(child.wait().unwrap().success());
        let getfacl = std::process::Command::new("getfacl")
            .args(["-n", "-p"])
            .arg(all.join("acl.txt"))
            .output()
            .unwrap();
        assert!(
            String::from_utf8_lossy(&getfacl.stdout).contains("user:0:r--"),
            "the ACL is restored: {}",
            String::from_utf8_lossy(&getfacl.stdout)
        );
        let getfattr = std::process::Command::new("getfattr")
            .args(["--only-values", "-n", "user.tapectl-test"])
            .arg(all.join("xattr.txt"))
            .output()
            .unwrap();
        assert_eq!(getfattr.stdout, b"v1", "the user attribute is restored");
        assert_eq!(
            fs::metadata(all.join("d/first.txt")).unwrap().ino(),
            fs::metadata(all.join("z-second.txt")).unwrap().ino(),
            "the hard link is a link"
        );
        assert_eq!(
            fs::read_link(all.join("link")).unwrap(),
            Path::new("d/first.txt")
        );

        // The id map: the owner, the group and the ACL's id, named.
        let ids = crate::db::stage_ids::load(&conn, id).unwrap();
        let me = fs::metadata(src.join("acl.txt")).unwrap();
        let has = |kind: crate::idmap::IdKind, n: u32| {
            ids.iter()
                .find(|r| r.kind == kind && r.id == n)
                .unwrap_or_else(|| panic!("{kind:?} {n} in {ids:?}"))
                .clone()
        };
        let owner = has(crate::idmap::IdKind::User, me.uid());
        assert_eq!(owner.name, crate::idmap::user_name(me.uid()));
        has(crate::idmap::IdKind::Group, me.gid());
        let root = has(crate::idmap::IdKind::User, 0);
        assert_eq!(root.name.as_deref(), Some("root"), "the ACL's id, named");
    }

    /// Invariant 7: a file replaced (a new file renamed over it) once tar has
    /// read it refuses the staging, by tapectl's own before-and-after
    /// comparison; nothing is left in staging and no slice is recorded staged.
    #[test]
    fn a_file_replaced_while_tar_reads_the_unit_refuses_the_staging() {
        let tmp = TempDir::new().unwrap();
        let (conn, paths, config, src) = setup_unit_with_excludes(&tmp, vec![]);
        fs::write(src.join("a.txt"), b"the first a").unwrap();
        fs::write(src.join("b.txt"), b"the first b").unwrap();
        let snap = snapshot_create(&conn, "unit1", &config).unwrap();
        let a = src.join("a.txt");
        let fresh = src.join(".fresh");
        super::super::archive::row_hook::on("a.txt", move || {
            fs::write(&fresh, b"the other a").unwrap();
            fs::rename(&fresh, &a).unwrap();
        });
        let err = stage_create(&conn, &paths, &config, snap, false)
            .expect_err("a file replaced under tar refuses the staging")
            .to_string();
        assert!(err.contains("DIRTY") && err.contains("a.txt"), "{err}");
        assert_eq!(
            fs::read(src.join("a.txt")).unwrap(),
            b"the other a",
            "positive control: the hook replaced the file"
        );
        assert!(
            fs::read_dir(&config.staging.directory)
                .unwrap()
                .next()
                .is_none(),
            "nothing is left in staging"
        );
    }

    /// A stand-in for tar: `--version` says `version`; an archive run execs
    /// the real tar, but outside the probe's own directory (whose file is
    /// `acl-probe`) it drops `drop` from its arguments. So the probe sees an
    /// honest tar, and the stream is what tapectl's own check must refuse.
    fn stand_in_tar(dir: &Path, version: &str, drop: Option<&str>) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let real = std::process::Command::new("sh")
            .args(["-c", "command -v tar"])
            .output()
            .unwrap();
        let real = String::from_utf8(real.stdout).unwrap().trim().to_string();
        let p = dir.join("tar-stand-in");
        let filter = match drop {
        None => String::new(),
        Some(arg) => format!(
            "if [ ! -e acl-probe ]; then\n  for a in \"$@\"; do shift; [ \"$a\" = \"{arg}\" ] || set -- \"$@\" \"$a\"; done\nfi\n"
        ),
    };
        fs::write(
        &p,
        format!(
            "#!/bin/sh\nif [ \"$1\" = --version ]; then echo \"tar (GNU tar) {version}\"; exit 0; fi\n{filter}exec {real} \"$@\"\n"
        ),
    )
    .unwrap();
        fs::set_permissions(&p, fs::Permissions::from_mode(0o755)).unwrap();
        p
    }

    /// Stage with the stand-in at `tar`, retrying a fork's ETXTBSY.
    fn stage_with_tar(tar: &Path, with_acl: bool) -> (String, i64) {
        let tmp = TempDir::new().unwrap();
        let (conn, paths, mut config, src) = setup_unit_with_excludes(&tmp, vec![]);
        config.tar.binary = tar.to_string_lossy().into_owned();
        fs::write(src.join("f.txt"), b"content").unwrap();
        if with_acl {
            assert!(run_ok(
                "setfacl",
                &[
                    std::ffi::OsStr::new("-m"),
                    std::ffi::OsStr::new("u:0:r"),
                    src.join("f.txt").as_os_str()
                ]
            ));
        }
        let snap = snapshot_create(&conn, "unit1", &config).unwrap();
        let mut tries = 0;
        let err = loop {
            match stage_create(&conn, &paths, &config, snap, false) {
                Ok(_) => panic!("the stand-in tar must be refused"),
                Err(e) if e.to_string().contains("Text file busy") && tries < 20 => tries += 1,
                Err(e) => break e.to_string(),
            }
        };
        let staged: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM stage_sets WHERE status = 'staged'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        (err, staged)
    }

    /// Item 274: a GNU tar below the floor is refused by name, before anything
    /// is staged.
    #[test]
    fn a_tar_below_the_floor_is_refused_by_name() {
        let dir = TempDir::new().unwrap();
        let tar = stand_in_tar(dir.path(), "1.30", None);
        let (err, staged) = stage_with_tar(&tar, false);
        assert!(
            err.contains("GNU tar 1.30") && err.contains("below the writer's floor"),
            "{err}"
        );
        assert!(err.contains("1.35"), "names the floor: {err}");
        assert_eq!(staged, 0);
    }

    /// Items 270, 274 (the proof, one way): a tar that writes owners' names
    /// passes the version and the probe, and its stream is refused by
    /// tapectl's own check, naming the rule.
    #[test]
    fn a_tar_that_writes_names_is_refused_by_the_streams_check() {
        let dir = TempDir::new().unwrap();
        let tar = stand_in_tar(dir.path(), "1.35", Some("--numeric-owner"));
        let (err, staged) = stage_with_tar(&tar, false);
        assert!(err.contains("names a user or group"), "{err}");
        assert_eq!(staged, 0);
    }

    /// Item 274 (the proof, the other way): a tar that writes no ACL record
    /// for a file that has an ACL on disk is refused by the stream's check.
    #[test]
    fn a_tar_that_writes_no_acl_record_is_refused_by_the_streams_check() {
        let dir = TempDir::new().unwrap();
        let tar = stand_in_tar(dir.path(), "1.35", Some("--acls"));
        let (err, staged) = stage_with_tar(&tar, true);
        assert!(
            err.contains("ACL is missing from the stream") && err.contains("f.txt"),
            "{err}"
        );
        assert_eq!(staged, 0);
    }

    /// Item 287: `compression_level` is resolved through the three layers as
    /// `compression` is, the most specific winning: an archive set's level
    /// over `[defaults]`, a unit's dotfile over its archive set.
    #[test]
    fn compression_level_resolves_dotfile_over_archive_set_over_defaults() {
        let tmp = TempDir::new().unwrap();
        let (conn, paths, mut config, src) = setup_unit_with_excludes(&tmp, vec![]);
        config.defaults.compression = "zstd".to_string();
        config.defaults.compression_level = Some(2);
        conn.execute(
            "INSERT INTO archive_sets (name, compression_level) VALUES ('packed', 5)",
            [],
        )
        .unwrap();
        conn.execute(
            "UPDATE units SET archive_set_id = (SELECT id FROM archive_sets WHERE name = 'packed')
          WHERE name = 'unit1'",
            [],
        )
        .unwrap();
        fs::write(src.join("f.txt"), b"content to pack").unwrap();
        let level_of = |id: i64| -> Option<i64> {
            conn.query_row(
                "SELECT compression_level FROM stage_sets WHERE id = ?1",
                params![id],
                |r| r.get(0),
            )
            .unwrap()
        };
        let snap = snapshot_create(&conn, "unit1", &config).unwrap();
        let id = stage_create(&conn, &paths, &config, snap, false).unwrap();
        assert_eq!(
            level_of(id),
            Some(5),
            "the archive set's level over [defaults]"
        );

        let dotfile = src.join(".tapectl-unit.toml");
        let mut df = crate::unit::dotfile::read_dotfile(&dotfile).unwrap();
        df.compression_level = Some(7);
        crate::unit::dotfile::write_dotfile(&dotfile, &df).unwrap();
        assert_eq!(
            crate::unit::dotfile::read_dotfile(&dotfile)
                .unwrap()
                .compression_level,
            Some(7),
            "a dotfile round trip keeps the level"
        );
        fs::write(src.join("g.txt"), b"a second version").unwrap();
        let snap2 = snapshot_create(&conn, "unit1", &config).unwrap();
        let id2 = stage_create(&conn, &paths, &config, snap2, false).unwrap();
        assert_eq!(
            level_of(id2),
            Some(7),
            "the dotfile's level over its archive set"
        );
    }

    /// #562: `slice_size` is a multiple of 512 wherever it is set: an
    /// `[[archive_sets]]` entry and `[defaults]` at config load, and a unit's
    /// dotfile where its policy is resolved, each naming the key.
    #[test]
    fn a_slice_size_not_a_multiple_of_512_is_refused_at_every_layer() {
        let tmp = TempDir::new().unwrap();
        let cfg = tmp.path().join("config.toml");
        fs::write(&cfg, "[defaults]\nslice_size = \"1000\"\n").unwrap();
        let e = crate::config::Config::load(&cfg).unwrap_err().to_string();
        assert!(
            e.contains("defaults.slice_size") && e.contains("512"),
            "{e}"
        );
        fs::write(
            &cfg,
            "[[archive_sets]]\nname = \"odd\"\nslice_size = \"100000\"\n",
        )
        .unwrap();
        let e = crate::config::Config::load(&cfg).unwrap_err().to_string();
        assert!(
            e.contains("archive_sets[0]") && e.contains("slice_size") && e.contains("512"),
            "{e}"
        );
        fs::write(&cfg, "[defaults]\nslice_size = \"1M\"\n").unwrap();
        crate::config::Config::load(&cfg).expect("a multiple of 512 loads");

        let (conn, _paths, config, src) = setup_unit_with_excludes(&tmp, vec![]);
        let dotfile = src.join(".tapectl-unit.toml");
        let mut df = crate::unit::dotfile::read_dotfile(&dotfile).unwrap();
        df.slice_size = Some("1000".to_string());
        crate::unit::dotfile::write_dotfile(&dotfile, &df).unwrap();
        let unit = crate::db::queries::get_unit_by_name(&conn, "unit1")
            .unwrap()
            .unwrap();
        let e = crate::policy::resolve(&conn, &config, &unit)
            .unwrap_err()
            .to_string();
        assert!(
            e.contains("[policy] slice_size") && e.contains("512"),
            "{e}"
        );
    }
}
