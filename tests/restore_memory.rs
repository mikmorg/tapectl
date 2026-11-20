//! The restore's peak memory as a function of `slice_size` (#571, T5.10).
//!
//! tapectl's restore reader (`src/volume/restore/reader.rs`, `supply.rs`)
//! decrypts one slice at a time into a `Vec<u8>` (`SliceOut.bytes`) and keeps
//! it whole while it cuts members out by the file table. This measures what
//! that costs on a real staging (the real `stage create`: tar, cut, compressor,
//! age; the real `volume write` over a MemStore; tapectl's own restore),
//! with a counting `#[global_allocator]`: bytes allocated minus bytes freed,
//! the high-water mark, reset just before the restore.
//!
//! What the code holds at once (read, then measured here):
//! - ciphertext: never whole (a pipe into the chunk decryptor);
//! - uncompressed: the plaintext Vec IS the cut (moved, not copied), so one
//!   slice: k = 1, up to 2 with `Vec` growth slack;
//! - compressed: the plaintext Vec is the frame, and `decode::decode` grows a
//!   second Vec to the whole cut while the frame is alive (`cut_of`), so
//!   frame + cut. For a frame near the cut's size (incompressible data, as
//!   here) that is k = 2; `Vec` doubling counts capacity, so a frame a few
//!   bytes over a power of two is held at twice its length: k = 3 when the
//!   cut is a whole power of two (the long member's slices), and 4 at worst;
//!   a compressible cut has a smaller frame, so k falls toward 1;
//! - what is counted is allocated capacity, not touched pages: the slack is
//!   address space the kernel never backs, so resident memory is lower;
//! - members: hashed and fed from a borrowed slice of the cut, never copied; a
//!   member longer than `slice_size` is fed piece by piece from each slice, so
//!   the bound is the slice either way, not the largest member.
//!
//! The allocator is process-wide, so this file is its own test binary (as
//! `pass_budget.rs` is) and its tests take turns.

use std::alloc::{GlobalAlloc, Layout, System};
use std::collections::BTreeMap;
use std::fs;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;

use rusqlite::Connection;
use tempfile::TempDir;

use tapectl::cli::cartridge::CartridgeCommands;
use tapectl::config::{Config, LtoBackendConfig, TapectlPaths};
use tapectl::store::MemStore;
use tapectl::volume::restore::{restore_unit_from_store_for_tests, RestoreOptions};
use tapectl::volume::write::{volume_init_over_store_for_tests, volume_write_over_store_for_tests};

struct Counting;

static LIVE: AtomicUsize = AtomicUsize::new(0);
static PEAK: AtomicUsize = AtomicUsize::new(0);

fn grew(by: usize) {
    let now = LIVE.fetch_add(by, Ordering::Relaxed) + by;
    PEAK.fetch_max(now, Ordering::Relaxed);
}

unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, l: Layout) -> *mut u8 {
        let p = System.alloc(l);
        if !p.is_null() {
            grew(l.size());
        }
        p
    }
    unsafe fn alloc_zeroed(&self, l: Layout) -> *mut u8 {
        let p = System.alloc_zeroed(l);
        if !p.is_null() {
            grew(l.size());
        }
        p
    }
    unsafe fn dealloc(&self, p: *mut u8, l: Layout) {
        System.dealloc(p, l);
        LIVE.fetch_sub(l.size(), Ordering::Relaxed);
    }
    unsafe fn realloc(&self, p: *mut u8, l: Layout, new: usize) -> *mut u8 {
        let q = System.realloc(p, l, new);
        if !q.is_null() {
            if new >= l.size() {
                grew(new - l.size());
            } else {
                LIVE.fetch_sub(l.size() - new, Ordering::Relaxed);
            }
        }
        q
    }
}

#[global_allocator]
static ALLOC: Counting = Counting;

/// Start a measured window: the peak is the most the live bytes rise above
/// what is live now.
fn begin() -> usize {
    let base = LIVE.load(Ordering::Relaxed);
    PEAK.store(base, Ordering::Relaxed);
    base
}

fn peak_since(base: usize) -> usize {
    PEAK.load(Ordering::Relaxed).saturating_sub(base)
}

static TURN: Mutex<()> = Mutex::new(());

const MIB: usize = 1024 * 1024;
const BS: usize = 4096;
const DEVICE: &str = "/nonexistent/tapectl-restore-memory-nst";
const OPERATOR: &str = "quince-operator-main";
const TENANT: &str = "persimmon-tenant-one";

fn noise(len: usize, seed: u32) -> Vec<u8> {
    let mut x = seed.wrapping_mul(2_654_435_761) ^ 0x9E37_79B9;
    (0..len)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 17;
            x ^= x << 5;
            (x >> 11) as u8
        })
        .collect()
}

/// What a case measured.
struct Measured {
    /// The largest cut of any slice, bytes (what a slice holds).
    max_cut: usize,
    slices: usize,
    /// The peak heap of the restore, bytes above what was live before it.
    peak: usize,
}

/// A unit of `sizes`-long noise files staged at `slice_size` with
/// `compression`, written to a MemStore, then restored: its peak heap.
fn measure(slice_size: &str, compression: &str, sizes: &[usize]) -> Measured {
    let tmp = TempDir::new().unwrap();
    let paths = TapectlPaths::new(tmp.path().join("home"));
    paths.ensure_dirs().unwrap();
    let conn: Connection = tapectl::db::open(&paths.db_file).unwrap();
    let staging = tmp.path().join("staging");
    let src = tmp.path().join("src");
    fs::create_dir_all(&staging).unwrap();
    fs::create_dir_all(&src).unwrap();

    let mut config = Config::default();
    config.staging.directory = staging.to_string_lossy().into_owned();
    config.defaults.slice_size = slice_size.to_string();
    config.defaults.compression = compression.to_string();
    config.defaults.min_copies = 1;
    config.backends.lto.push(LtoBackendConfig {
        name: "lto6mem".into(),
        device_tape: DEVICE.into(),
        device_sg: "/nonexistent/tapectl-restore-memory-sg".into(),
        generation: "LTO-6".into(),
        capacity_override: None,
        fill_ceiling: tapectl::config::DEFAULT_FILL_CEILING,
        enospc_buffer: "0".into(),
        post_warning_bytes: Some("256M".into()),
    });
    tapectl::tenant::add_tenant(&conn, &paths, OPERATOR, None, true).unwrap();
    tapectl::tenant::add_tenant(&conn, &paths, TENANT, None, false).unwrap();
    tapectl::cli::key::create_escrow_recipient(&conn, &paths, None).unwrap();

    let mut files = BTreeMap::new();
    for (i, &len) in sizes.iter().enumerate() {
        let name = format!("part-{:04}.bin", i + 1);
        let bytes = noise(len, 17 + i as u32 * 7919);
        fs::write(src.join(&name), &bytes).unwrap();
        files.insert(name, bytes);
    }
    tapectl::unit::init_unit_with_config(
        &conn,
        &config,
        src.to_str().unwrap(),
        TENANT,
        Some("memory"),
        &[],
        None,
    )
    .unwrap();
    // A file written within two seconds of its hash is hashed twice (git's
    // racy case); sleep so the staging is the ordinary one.
    std::thread::sleep(std::time::Duration::from_millis(2_200));
    let snapshot = tapectl::staging::snapshot_create(&conn, "memory", &config).unwrap();
    tapectl::staging::stage_create(&conn, &paths, &config, snapshot, true).unwrap();
    let (slices, max_cut): (i64, i64) = conn
        .query_row(
            "SELECT COUNT(*), MAX(size_bytes) FROM stage_slices",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();

    tapectl::cli::cartridge::run(
        &conn,
        &config,
        &CartridgeCommands::Register {
            barcode: "MEM001-STK".into(),
            generation: "LTO-6".into(),
            capacity: None,
            serial: None,
            notes: None,
        },
        false,
        true,
        false,
    )
    .unwrap();
    let mut tape = MemStore::new(BS);
    volume_init_over_store_for_tests(
        &conn,
        &config,
        "MEM001",
        DEVICE,
        BS,
        Some("MEM001-STK"),
        &mut tape,
    )
    .unwrap();
    volume_write_over_store_for_tests(&conn, &paths, &config, "MEM001", DEVICE, BS, &mut tape)
        .unwrap();

    // The window: only the restore.
    let dest = tmp.path().join("dest");
    fs::create_dir_all(&dest).unwrap();
    let base = begin();
    let report = restore_unit_from_store_for_tests(
        &conn,
        &paths,
        &config,
        "memory",
        "MEM001",
        1,
        &dest,
        &RestoreOptions::default(),
        &mut tape,
    )
    .unwrap();
    let peak = peak_since(base);
    assert!(report.success);
    for (name, bytes) in &files {
        assert!(
            fs::read(dest.join(name)).ok().as_ref() == Some(bytes),
            "{name} restored byte for byte"
        );
    }
    Measured {
        max_cut: max_cut as usize,
        slices: slices as usize,
        peak,
    }
}

/// The two slice sizes on one shape, and the peak stated as `k` slices.
/// `k_lo`/`k_hi`: the multiple of the largest cut the reader's holdings give
/// (see the module doc), plus [`FIXED`] for everything that is not the cut.
fn prove(what: &str, compression: &str, sizes: &[usize], k_lo: f64, k_hi: f64) {
    let _turn = TURN.lock().unwrap_or_else(|e| e.into_inner());
    let small = measure("2M", compression, sizes);
    let large = measure("8M", compression, sizes);
    for (size, m) in [("2M", &small), ("8M", &large)] {
        eprintln!(
            "restore peak | {what} | {compression} | slice_size {size} | {} slices | largest cut \
             {:.2} MiB | peak {:.2} MiB | k = {:.2}",
            m.slices,
            m.max_cut as f64 / MIB as f64,
            m.peak as f64 / MIB as f64,
            m.peak as f64 / m.max_cut as f64
        );
        assert!(m.slices >= 3, "{what}: several full slices ({})", m.slices);
        let cut = m.max_cut as f64;
        assert!(
            m.peak as f64 >= k_lo * cut,
            "{what}/{compression}/{size}: the peak {} holds at least {k_lo} cut(s) of {}",
            m.peak,
            m.max_cut
        );
        assert!(
            m.peak as f64 <= k_hi * cut + FIXED as f64,
            "{what}/{compression}/{size}: the peak {} is within {k_hi} cut(s) of {} + {FIXED}",
            m.peak,
            m.max_cut
        );
    }
    // Scaling: what 6 MiB more slice costs. This is the real check; the absolute
    // bound above (with its constant) only keeps the figures sane.
    let d_peak = large.peak as f64 - small.peak as f64;
    let d_cut = large.max_cut as f64 - small.max_cut as f64;
    let k = d_peak / d_cut;
    eprintln!("restore scaling | {what} | {compression} | d_peak/d_cut = {k:.2}");
    assert!(
        d_cut > 5.0 * MIB as f64,
        "the two sizes differ by about 6 MiB of cut"
    );
    assert!(
        k >= k_lo - 0.05 && k <= k_hi + 0.05,
        "{what}/{compression}: the peak grows {k:.2} bytes per byte of cut, expected within \
         [{k_lo}, {k_hi}]"
    );
}

/// Everything in a restore that is not a slice: catalog rows, buffers, the
/// file table, thread stacks' bookkeeping.
const FIXED: usize = 4 * MIB;

/// Positive control: an allocation of a known size inside the window raises
/// the peak by at least that size, and one freed before the window does not.
#[test]
fn the_counter_sees_an_allocation_inside_the_window() {
    let _turn = TURN.lock().unwrap_or_else(|e| e.into_inner());
    drop(std::hint::black_box(vec![1u8; 40 * MIB]));
    let base = begin();
    let quiet = peak_since(base);
    let v = std::hint::black_box(vec![7u8; 5 * MIB]);
    assert!(v[MIB] == 7);
    let seen = peak_since(base);
    drop(v);
    assert!(quiet < MIB, "nothing allocated, nothing counted: {quiet}");
    assert!(
        (5 * MIB..6 * MIB).contains(&seen),
        "a 5 MiB allocation raises the peak by about 5 MiB: {seen}"
    );
    // The high-water mark survives the free.
    assert!(peak_since(base) >= 5 * MIB);
}

fn many_small() -> Vec<usize> {
    vec![MIB; 16]
}

fn one_long() -> Vec<usize> {
    vec![12 * MIB, 4 * MIB]
}

/// Uncompressed: the plaintext Vec is the cut (moved, no copy). One slice,
/// up to two with growth slack (measured: 1.0 to 1.2).
#[test]
fn uncompressed_many_small_members_peak_is_one_slice() {
    prove("many small", "none", &many_small(), 1.0, 2.0);
}

#[test]
fn uncompressed_one_long_member_peak_is_one_slice() {
    prove("one long", "none", &one_long(), 1.0, 2.0);
}

/// zstd, on noise (frame about the cut's size): the frame and the decoded cut
/// are alive together, 2 to 4 cuts by capacity (measured: 2.0 to 3.0).
#[test]
fn zstd_many_small_members_peak_is_frame_plus_cut() {
    prove("many small", "zstd", &many_small(), 2.0, 4.0);
}

#[test]
fn zstd_one_long_member_peak_is_frame_plus_cut() {
    prove("one long", "zstd", &one_long(), 2.0, 4.0);
}
