//! The driver that made `gen1.db` and `gen2.db` (see README.md). It is NOT
//! built from this tree: it was copied into a checkout of the historical
//! commit, as `examples/make_fixture.rs`, and run there, so the file it writes
//! is that commit's own writer's output.
//!
//! It fills a fresh catalog (that commit's `db::open`, all its migrations)
//! with explicit ids — gapped, nowhere near 1 — and hands the stage sets to
//! the writer of the time: `db::catalog_snapshot::build_catalog_snapshot` at
//! b3fe31e (generation 1), `db::ontape_catalog::write` at v1.0.6
//! (generation 2). The two lines marked WRITER are the only difference
//! between the two runs.

use std::path::Path;

fn main() {
    let out = std::env::args().nth(1).expect("usage: make_fixture <out.db>");
    let dir = std::env::temp_dir().join(format!("make-fixture-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let conn = tapectl::db::open(&dir.join("tapectl.db")).unwrap();

    // The walk's own spelling of an mtime (staging::walk_directory, both
    // commits): whole seconds, RFC 3339, `+00:00`.
    let walk_mtime = |secs: i64| {
        chrono::DateTime::from_timestamp(secs, 0)
            .map(|dt| dt.to_rfc3339())
            .unwrap()
    };
    let recent = walk_mtime(1_788_264_000); // 2026-09-01T12:00:00+00:00
    let pre_1970 = walk_mtime(-315_619_200); // 1960-01-01T00:00:00+00:00

    conn.execute_batch(
        "INSERT INTO tenants (id, name, is_operator) VALUES (1, 'operator', 1);
         INSERT INTO tenants (id, name) VALUES (3, 'alice');
         INSERT INTO tenants (id, name) VALUES (5, 'bob');
         INSERT INTO units (id, uuid, name, tenant_id, current_path, status)
             VALUES (12, '0190f5a0-0000-7000-8000-00000000000c', 'photos', 3,
                     '/archive/photos', 'active');
         INSERT INTO units (id, uuid, name, tenant_id, current_path, status)
             VALUES (14, '0190f5a0-0000-7000-8000-00000000000e', 'ledgers', 5,
                     '/archive/ledgers', 'active');
         INSERT INTO snapshots (id, unit_id, version, status, source_path, total_size,
                                file_count)
             VALUES (40, 12, 3, 'staged', '/archive/photos', 11, 3);
         INSERT INTO snapshots (id, unit_id, version, status, source_path, total_size,
                                file_count)
             VALUES (41, 12, 4, 'created', '/archive/photos', 0, 1);
         INSERT INTO snapshots (id, unit_id, version, status, source_path, total_size,
                                file_count)
             VALUES (47, 14, 1, 'staged', '/archive/ledgers', 29, 2);",
    )
    .unwrap();

    let file = |id: i64,
                snapshot_id: i64,
                path: &str,
                size: i64,
                sha256: Option<String>,
                mtime: &str,
                file_type: &str,
                link_target: Option<&str>| {
        conn.execute(
            "INSERT INTO files (id, snapshot_id, path, size_bytes, sha256, modified_at,
                                is_directory, file_type, link_target)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            rusqlite::params![
                id,
                snapshot_id,
                path,
                size,
                sha256,
                mtime,
                (file_type == "dir") as i64,
                file_type,
                link_target
            ],
        )
        .unwrap();
    };
    let hex = |b: &str| b.repeat(32);
    // photos v3 (on the tape): ids 77001-77004, in walk order.
    file(77001, 40, "docs", 0, None, &recent, "dir", None);
    file(77002, 40, "docs/a.txt", 11, Some(hex("a1")), &pre_1970, "regular", None);
    // lstat's size of a symlink is its target's length ("a.txt").
    file(77003, 40, "docs/latest", 5, None, &recent, "symlink", Some("a.txt"));
    file(77004, 40, "docs/pipe", 0, None, &recent, "special", None);
    // photos v4 (NOT on the tape): ids 77005-77009 are the gap.
    for (n, id) in (77005..=77009).enumerate() {
        file(id, 41, &format!("v4/{n}"), 0, None, &recent, "regular", None);
    }
    // ledgers v1 (on the tape): ids 77010-77012.
    file(77010, 47, "fy24.csv", 20, Some(hex("f2")), &recent, "regular", None);
    file(77011, 47, "notes", 0, None, &recent, "dir", None);
    file(77012, 47, "notes/q1.txt", 9, Some(hex("9e")), &recent, "regular", None);

    conn.execute_batch(
        "INSERT INTO stage_sets (id, snapshot_id, status, slice_size, num_slices,
                                 total_dar_size, total_encrypted_size, key_fingerprints)
             VALUES (90, 40, 'staged', 1048576, 2, 1500000, 1500400,
                     '[\"age1alice\",\"age1operator\",\"age1escrow\"]');
         INSERT INTO stage_sets (id, snapshot_id, status, slice_size, num_slices,
                                 total_dar_size, total_encrypted_size, key_fingerprints)
             VALUES (93, 47, 'staged', 1048576, 1, 700, 900,
                     '[\"age1bob\",\"age1operator\",\"age1escrow\"]');",
    )
    .unwrap();
    for (id, ss, n, size, enc, plain, cipher) in [
        (300, 90, 1, 1048576, 1048776, "11", "c1"),
        (301, 90, 2, 451424, 451624, "22", "c2"),
        (305, 93, 1, 700, 900, "33", "c3"),
    ] {
        conn.execute(
            "INSERT INTO stage_slices (id, stage_set_id, slice_number, size_bytes,
                                       encrypted_bytes, sha256_plain, sha256_encrypted,
                                       staging_path)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, '/staging/slice')",
            rusqlite::params![id, ss, n, size, enc, hex(plain), hex(cipher)],
        )
        .unwrap();
    }

    let out = Path::new(&out);
    // WRITER (b3fe31e): tapectl::db::catalog_snapshot::build_catalog_snapshot(&conn, &[90, 93], out).unwrap();
    // WRITER (v1.0.6):  tapectl::db::ontape_catalog::write(&conn, &[90, 93], out).unwrap();
    tapectl::db::ontape_catalog::write(&conn, &[90, 93], out).unwrap();
    std::fs::remove_dir_all(&dir).unwrap();
}
