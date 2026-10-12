//! `TapeStore`'s own count of the tape commands it issues, held to the
//! kernel's count of the same commands on an mhvtl drive (issue #416
//! item 12).
//!
//! The budgets in `src/volume/cost_budget.rs` count motions over the fake
//! tape's log, and `TapeStore::motions` counts them in the store; only a
//! real st driver can say both are counting what the drive was actually
//! asked to do. st counts every command it issues that is not a READ or a
//! WRITE in `/sys/class/scsi_tape/<node>/stats/other_cnt`; between two
//! readings taken with the store open, after its open and before its close,
//! that is the store's rewinds, spaces and filemarks and nothing else
//! (`MTIOCGET`, the cursor's cross-check, is answered by the driver).
//!
//! Gated as `tests/mhvtl_e2e.rs` is: `#[ignore]`, and a skip unless
//! `TAPECTL_MHVTL` is set and `TAPECTL_GATE_TAPE` names an existing node.
//! It WRITES the loaded cartridge from BOT, so `TAPECTL_GATE_TAPE` must be
//! an mhvtl drive; `scripts/mhvtl-device.sh` refuses anything else. Not run
//! by the gate.
//!
//!   TAPECTL_GATE_TAPE=/dev/nstN TAPECTL_MHVTL=1 \
//!       cargo test --test mhvtl_motions -- --ignored --nocapture

use std::path::Path;
use std::process::Command;

use tapectl::store::{Motions, Store, TapeStore};
use tapectl::tape::st_stats;

const BLOCK_SIZE: usize = 512 * 1024;
const FILES: u32 = 10;

fn tape_dev() -> String {
    std::env::var("TAPECTL_GATE_TAPE")
        .expect("TAPECTL_GATE_TAPE must name the mhvtl drive (e.g. /dev/nst3); there is no default")
}

fn mhvtl_enabled() -> bool {
    std::env::var("TAPECTL_MHVTL").is_ok() && Path::new(&tape_dev()).exists()
}

/// Load a generation-matched cartridge through the one script that knows
/// how, as `tests/mhvtl_e2e.rs` does; it refuses a device that is not
/// mhvtl's. A failure is fatal, never a skip.
fn ensure_media() {
    let script = concat!(env!("CARGO_MANIFEST_DIR"), "/scripts/mhvtl-device.sh");
    let out = Command::new(script)
        .args(["--tape", &tape_dev(), "--ensure-media"])
        .output()
        .expect("run scripts/mhvtl-device.sh");
    assert!(
        out.status.success(),
        "mhvtl-device.sh refused {}: {}",
        tape_dev(),
        String::from_utf8_lossy(&out.stderr)
    );
}

/// st's `other_cnt` for the node, now.
fn other_cnt() -> u64 {
    let dir = st_stats::stats_dir(Path::new("/sys/class/scsi_tape"), &tape_dev())
        .expect("a stats directory for the tape node");
    let reading = st_stats::read(&dir).expect("st statistics in sysfs");
    reading
        .files
        .get("other_cnt")
        .expect("other_cnt in the st statistics")
        .trim()
        .parse()
        .expect("other_cnt is a count")
}

/// `later - earlier`, field by field.
fn since(later: Motions, earlier: Motions) -> Motions {
    Motions {
        rewinds: later.rewinds - earlier.rewinds,
        spaces: later.spaces - earlier.spaces,
        filemarks: later.filemarks - earlier.filemarks,
        status_reads: later.status_reads - earlier.status_reads,
    }
}

fn file_bytes(i: u32) -> Vec<u8> {
    // Two whole blocks and part of a third, each file its own byte.
    vec![i as u8 + 1; 2 * BLOCK_SIZE + 1000 * (i as usize + 1)]
}

fn read_at(store: &mut TapeStore, position: u32) -> Vec<u8> {
    let mut out = Vec::new();
    store.read_file(position, &mut out).unwrap();
    out
}

#[test]
#[ignore]
fn mhvtl_tape_store_counts_the_commands_the_kernel_counts() {
    if !mhvtl_enabled() {
        eprintln!("skip: TAPECTL_MHVTL not set or TAPECTL_GATE_TAPE missing");
        return;
    }
    ensure_media();
    let dev = tape_dev();

    // Writes: one filemark each, the last synchronous.
    let mut store = TapeStore::open(&dev, BLOCK_SIZE, u64::MAX).unwrap();
    let (m0, k0) = (store.motions(), other_cnt());
    for i in 0..FILES {
        let bytes = file_bytes(i);
        store
            .execute(&mut &bytes[..], bytes.len() as u64, i == FILES - 1)
            .unwrap();
    }
    let (m1, k1) = (store.motions(), other_cnt());
    drop(store);
    let wrote = since(m1, m0);
    eprintln!("write: store {wrote:?}, kernel other_cnt +{}", k1 - k0);
    assert_eq!(wrote.filemarks, FILES as u64, "positive control: {wrote:?}");
    assert_eq!(
        k1 - k0,
        wrote.commands(),
        "the kernel counted {} commands for a write the store counted as {wrote:?}",
        k1 - k0
    );

    // Reads: forward through every file (no motion but the spaces the
    // cursor needs), then back (a rewind and a space), a head read that
    // stops inside a file, a forward space from inside it, and back again.
    let mut store = TapeStore::open_read(&dev, BLOCK_SIZE).unwrap();
    let (m0, k0) = (store.motions(), other_cnt());
    for i in 0..FILES {
        let len = file_bytes(i).len();
        assert_eq!(read_at(&mut store, i)[..len], file_bytes(i)[..]);
    }
    read_at(&mut store, 2);
    let mut head = Vec::new();
    store.read_file_head(5, 1024, &mut head).unwrap();
    assert_eq!(head, vec![6u8; 1024]);
    read_at(&mut store, 7);
    read_at(&mut store, 1);
    let (m1, k1) = (store.motions(), other_cnt());
    drop(store);
    let read = since(m1, m0);
    eprintln!("read: store {read:?}, kernel other_cnt +{}", k1 - k0);
    assert!(
        read.rewinds >= 2 && read.spaces >= 3 && read.status_reads > 0,
        "positive control: the reads moved the tape back and forward, and the \
         cursor was cross-checked: {read:?}"
    );
    assert_eq!(
        k1 - k0,
        read.commands(),
        "the kernel counted {} commands for reads the store counted as {read:?}",
        k1 - k0
    );
}
