//! Logical Block Protection through the tape layer on an mhvtl LTO-6 drive
//! (#388; ADR-0012 items 162-166 and 174): arming and the probe, protected
//! writes and reads, a block the drive refuses and item 174's way back to the
//! file's start, the page cleared when the device closes, and a page a crash
//! left armed cleared at the next open.
//!
//! **Why a 262,144-byte payload.** mhvtl cannot carry tapectl's 524,292-byte
//! wire block (524,288 and the 4-byte CRC) in one command: its host template
//! caps a command at 128 segments of one page each (`sg_tablesize` 128,
//! `dma_boundary` PAGE_SIZE - 1; build plan §7.2), so st answers EBUSY without
//! sending it. These tests drive the library at 262,144 (a 262,148-byte wire
//! block), which mhvtl carries; a 524,288 open is the probe's negative case.
//! No volume, File 0 or RESTORE.sh is written, so the smaller block is no
//! question of the format. The 524,292-byte path is home2's
//! (`scripts/lbp-acceptance.sh`).
//!
//! **Why a TD6 drive.** mhvtl's TD5 and TD6 personalities speak Reed-Solomon
//! (method 01h), the one tapectl arms (item 162); TD7 and up speak CRC32C, so
//! the gate's TD8 drive falls back on every open, which the gate's
//! `lbp_fallback_recorded` leg proves instead.
//!
//! **What mhvtl does differently** (research note §5, gap 1): it writes a
//! block with a bad CRC to its virtual tape and then refuses the command,
//! where the real drive refuses the block before writing it. The refusal test
//! accepts the refusal at the corrupted block's file or later, and item 174's
//! return to the file's start overwrites whatever mhvtl kept.
//!
//! Gated as `tests/mhvtl_motions.rs` is: `#[ignore]`, a skip unless
//! `TAPECTL_MHVTL` is set and `TAPECTL_GATE_TAPE_TD6` names an existing node,
//! and a failure instead of a skip under `TAPECTL_GATE_REQUIRE_MEDIA`. It
//! WRITES the loaded cartridge from BOT; `scripts/mhvtl-device.sh` refuses a
//! device that is not mhvtl's, and the test refuses one that is not a TD6.
//! Every test leaves the drive at 524,288 bytes with the page at method 0,
//! pass or fail ([`LeaveDrive`]). The gate runs it as its `rust_lbp` leg.
//!
//!   TAPECTL_GATE_TAPE_TD6="$(readlink -f /dev/tape/by-id/scsi-XYZZY_A3-nst)" \
//!       TAPECTL_MHVTL=1 cargo test --test mhvtl_lbp -- --ignored --nocapture
//!
//! One test is ungated: the gate's own reader of the page
//! (`lbp_page_parse` in `scripts/mhvtl-verify-gate.sh`), held to the
//! library's parser on recorded MODE SENSE answers, so the gate's "method 0
//! after every step" can never be a reader that sees 0 everywhere.

use std::io::{self, Read};
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::{Mutex, MutexGuard, OnceLock};

use tapectl::error::TapectlError;
use tapectl::store::{Store, TapeStore, TEST_LBP_CORRUPT_BLOCK};
use tapectl::tape::ioctl::{ReadEnd, TapeDevice};
use tapectl::tape::lbp::{self, LbpReadPolicy, PageControl, Protection, ProtectionPage};

/// The payload these tests write: half the format's block, which mhvtl
/// carries with the CRC.
const PAYLOAD: usize = 256 * 1024;
/// The format's block (`cli::volume::DEFAULT_BLOCK_SIZE`): with the CRC,
/// more than mhvtl carries in one command.
const FORMAT_BLOCK: usize = 512 * 1024;
/// Files per test tape. Each is three blocks ([`file_bytes`]).
const FILES: u32 = 6;
/// The blocks of each file at [`PAYLOAD`].
const FILE_BLOCKS: u64 = 3;
/// The file whose second block the refusal test corrupts.
const HOOKED_FILE: u32 = 1;

fn tape_dev() -> Option<String> {
    std::env::var("TAPECTL_GATE_TAPE_TD6")
        .ok()
        .filter(|d| !d.is_empty())
}

fn dev() -> String {
    tape_dev().expect("TAPECTL_GATE_TAPE_TD6 names the mhvtl TD6 drive")
}

fn mhvtl_enabled() -> bool {
    let on = std::env::var("TAPECTL_MHVTL").is_ok()
        && tape_dev().is_some_and(|d| Path::new(&d).exists());
    // #471: the gate sets TAPECTL_GATE_REQUIRE_MEDIA, so a test that would
    // skip there fails instead: a skip returns early and reports `ok`.
    if !on && std::env::var("TAPECTL_GATE_REQUIRE_MEDIA").is_ok() {
        panic!(
            "TAPECTL_GATE_REQUIRE_MEDIA is set but this test would skip: TAPECTL_MHVTL is unset \
             or TAPECTL_GATE_TAPE_TD6 ({:?}) does not name an existing node",
            tape_dev()
        );
    }
    on
}

/// One test at a time on the one drive (`tests/mhvtl_e2e.rs`'s lock). A
/// panicked test does not wedge the rest.
fn tape_lock() -> MutexGuard<'static, ()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|e| e.into_inner())
}

/// Load a generation-matched cartridge through the one script that knows
/// how; it refuses a device that is not mhvtl's. The drive must be a TD6
/// emulation: Reed-Solomon LBP. A failure is fatal, never a skip.
fn ensure_media() {
    let script = concat!(env!("CARGO_MANIFEST_DIR"), "/scripts/mhvtl-device.sh");
    let out = Command::new(script)
        .args(["--tape", &dev(), "--ensure-media"])
        .output()
        .expect("run scripts/mhvtl-device.sh");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success(),
        "mhvtl-device.sh refused {}: {}",
        dev(),
        String::from_utf8_lossy(&out.stderr)
    );
    let model = stdout
        .lines()
        .find_map(|l| l.strip_prefix("DRIVE_MODEL="))
        .unwrap_or_default();
    assert!(
        model.contains("TD6"),
        "{} is {model}, not a TD6 emulation: these tests need Reed-Solomon LBP (method 01h)",
        dev()
    );
}

/// The page control the library uses for this drive: `sg_modes` and
/// `sg_wr_mode` on the sg node the kernel binds to the st node.
fn control() -> lbp::SgPageControl {
    lbp::control_for(&dev()).unwrap_or_else(|| {
        panic!(
            "no sg node is bound to {} in sysfs: tapectl could not arm LBP on it",
            dev()
        )
    })
}

/// The page now, current values.
fn page_now() -> ProtectionPage {
    control().sense().expect("MODE SENSE of the page")
}

/// The page with protection off (method 0, neither check), or the test
/// fails naming `when`.
fn assert_page_clear(when: &str) {
    let p = page_now();
    assert!(
        p.method() == 0 && p.flags() == 0,
        "{when}: the page reads method {:02X}h, flags {:02X}h ({:02x?}); it must read method 0 \
         with neither check on",
        p.method(),
        p.flags(),
        p.bytes()
    );
}

/// The bits an open of this kind arms under the build's read policy (item
/// 175's switch).
fn bits_for(write_open: bool) -> Option<u8> {
    lbp::LBP_R_POLICY.open_bits(write_open)
}

/// What an open at [`PAYLOAD`] decides, under the build's read policy.
fn protection_for(write_open: bool) -> Protection {
    match bits_for(write_open) {
        Some(bits) => Protection::Lbp {
            reads: bits & lbp::LBP_R != 0,
        },
        None => Protection::NotApplicable,
    }
}

fn mt(args: &[&str]) {
    let st = Command::new("mt")
        .arg("-f")
        .arg(dev())
        .args(args)
        .status()
        .expect("run mt");
    assert!(st.success(), "mt {args:?} on {} failed", dev());
}

/// Pass or fail, the drive is left as the gate and the harnesses expect it:
/// the page at method 0 with neither check on, st at the format's block size,
/// rewound. A page left armed here is a failure the test already reported;
/// this only puts the drive back.
struct LeaveDrive;

impl Drop for LeaveDrive {
    fn drop(&mut self) {
        if let Some(c) = tape_dev().and_then(|d| lbp::control_for(&d)) {
            match c.sense() {
                Ok(p) if p.method() != 0 || p.flags() != 0 => {
                    eprintln!("cleanup: the page read {:02x?}; clearing it", p.bytes());
                    if let Err(e) = c.select(&p.cleared()) {
                        eprintln!("cleanup: could not clear the page: {e:?}");
                    }
                }
                Ok(_) => {}
                Err(e) => eprintln!("cleanup: could not read the page: {e:?}"),
            }
        }
        if let Some(d) = tape_dev() {
            let _ = Command::new("mt")
                .arg("-f")
                .arg(&d)
                .arg("setblk")
                .arg(FORMAT_BLOCK.to_string())
                .status();
            let _ = Command::new("mt").arg("-f").arg(&d).arg("rewind").status();
        }
    }
}

/// Bytes no two files or blocks share (a small xorshift), so a block read
/// from the wrong place, or a CRC checked over the wrong bytes, shows.
fn pattern(len: usize, seed: u64) -> Vec<u8> {
    let mut x = seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1;
    (0..len)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            (x >> 24) as u8
        })
        .collect()
}

/// File `i`: two whole blocks and part of a third ([`FILE_BLOCKS`]).
fn file_bytes(i: u32) -> Vec<u8> {
    pattern(2 * PAYLOAD + 1000 * (i as usize + 1) + 7, u64::from(i) + 1)
}

/// `bytes` as a file on the tape: zero-padded to whole blocks.
fn padded(bytes: &[u8]) -> Vec<u8> {
    let mut v = bytes.to_vec();
    v.resize(bytes.len().div_ceil(PAYLOAD) * PAYLOAD, 0);
    v
}

fn write_files(store: &mut TapeStore, from: u32) {
    for i in from..FILES {
        let bytes = file_bytes(i);
        store
            .execute(&mut &bytes[..], bytes.len() as u64, i == FILES - 1)
            .unwrap_or_else(|e| panic!("write of file {i}: {e}"));
    }
}

/// Every file reads back through the library as written, block padding
/// included: with LBP_R armed (item 162's policy) each block came with the
/// drive's CRC and passed tapectl's check.
fn read_back_all(when: &str) {
    let mut store = TapeStore::open_read(&dev(), PAYLOAD).unwrap();
    assert_eq!(
        store.protection(),
        protection_for(false),
        "{when}: the read open"
    );
    for i in 0..FILES {
        let mut out = Vec::new();
        store
            .read_file(i, &mut out)
            .unwrap_or_else(|e| panic!("{when}: read of file {i}: {e}"));
        assert!(
            out == padded(&file_bytes(i)),
            "{when}: file {i} read back {} bytes that differ from the {} written (padded)",
            out.len(),
            padded(&file_bytes(i)).len()
        );
    }
    drop(store);
    assert_page_clear(&format!("{when}: after the read store closed"));
}

/// The planned file holding logical object `lba` on these tapes: each file
/// is [`FILE_BLOCKS`] blocks and one filemark from BOT — the session's
/// `planned_file_at` for a Layout of equal files.
fn planned_file_at(lba: u64) -> u32 {
    (lba / (FILE_BLOCKS + 1)) as u32
}

/// A source that fails after `at` bytes, as a staging disk can.
struct FailsAfter {
    data: Vec<u8>,
    pos: usize,
    at: usize,
}

impl Read for FailsAfter {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.pos >= self.at {
            return Err(io::Error::other("the source failed (test)"));
        }
        let n = buf.len().min(self.at - self.pos);
        buf[..n].copy_from_slice(&self.data[self.pos..self.pos + n]);
        self.pos += n;
        Ok(n)
    }
}

fn start() -> Option<(MutexGuard<'static, ()>, LeaveDrive)> {
    let g = tape_lock();
    if !mhvtl_enabled() {
        eprintln!("skip: TAPECTL_MHVTL not set or TAPECTL_GATE_TAPE_TD6 missing");
        return None;
    }
    assert!(
        std::env::var_os(TEST_LBP_CORRUPT_BLOCK).is_none(),
        "{TEST_LBP_CORRUPT_BLOCK} must not be set around these tests: the refusal test sets it \
         for its one open"
    );
    ensure_media();
    let leave = LeaveDrive;
    assert_page_clear("before the test");
    Some((g, leave))
}

/// Items 162-165 at the open: a write open arms Reed-Solomon with the checks
/// the policy names and the drive reads it back so; the probe finds a wire
/// block mhvtl can carry and falls back, by cause 8's reason, on one it
/// cannot — the page back as found while that store is still open.
#[test]
#[ignore]
fn mhvtl_lbp_an_open_arms_the_page_and_the_probe_refuses_a_block_mhvtl_cannot_carry() {
    let Some((_g, _leave)) = start() else { return };

    let store = TapeStore::open(&dev(), PAYLOAD, u64::MAX).unwrap();
    assert_eq!(store.protection(), protection_for(true), "the write open");
    let bits = bits_for(true).expect("a write open arms LBP_W under either policy");
    let p = page_now();
    assert!(
        p.armed_as(bits),
        "while the write store is open the page must read Reed-Solomon (01h), length 4, flags \
         {bits:02X}h: {:02x?}",
        p.bytes()
    );
    drop(store);
    assert_page_clear("after the armed write store closed");

    match bits_for(false) {
        Some(bits) => {
            let store = TapeStore::open_read(&dev(), PAYLOAD).unwrap();
            assert_eq!(store.protection(), protection_for(false), "the read open");
            assert!(
                page_now().armed_as(bits),
                "the read open's page: {:02x?}",
                page_now().bytes()
            );
            drop(store);
            assert_page_clear("after the armed read store closed");
        }
        None => eprintln!("the build's read policy arms no read open (item 175's fallback)"),
    }

    // The probe's negative case (F12; §5.1 cause 8): 524,288 + 4 bytes is
    // more than mhvtl carries in one command.
    let store = TapeStore::open(&dev(), FORMAT_BLOCK, u64::MAX).unwrap();
    let wire = FORMAT_BLOCK + lbp::CRC_LEN;
    match store.protection() {
        Protection::Unavailable(reason) => assert!(
            reason.starts_with(&format!("st cannot carry a {wire}-byte block")),
            "the fallback's reason is not the probe's: {reason}"
        ),
        other => panic!("a {wire}-byte wire block on mhvtl must fall back, got {other:?}"),
    }
    assert_page_clear("while the fallen-back store holds the drive");
    drop(store);
    assert_page_clear("after the fallen-back store closed");
}

/// Protected writes read back through the library with LBP_R; with the page
/// clear, the heir's tools read the same blocks plain, and a variable-mode
/// read shows the medium's block is the payload: the CRC never reaches the
/// tape (§8.2 measurement 4, emulated).
#[test]
#[ignore]
fn mhvtl_lbp_protected_writes_read_back_and_the_heir_reads_them_plain() {
    let Some((_g, _leave)) = start() else { return };

    let mut store = TapeStore::open(&dev(), PAYLOAD, u64::MAX).unwrap();
    assert_eq!(store.protection(), protection_for(true), "the write open");
    write_files(&mut store, 0);
    assert!(
        page_now().armed_as(bits_for(true).unwrap()),
        "the page after the writes, store still open: {:02x?}",
        page_now().bytes()
    );
    drop(store);
    assert_page_clear("after the write store closed");

    read_back_all("the protected tape");

    // The heir's view: the page clear (tapectl closed the drive), st's block
    // size the payload, plain dd.
    mt(&["rewind"]);
    mt(&["setblk", &PAYLOAD.to_string()]);
    mt(&["fsf", "2"]);
    let out = Command::new("dd")
        .arg(format!("if={}", dev()))
        .arg(format!("bs={PAYLOAD}"))
        .arg("status=none")
        .stderr(Stdio::inherit())
        .output()
        .expect("run dd");
    assert!(out.status.success(), "dd of file 2 with the page clear");
    assert!(
        out.stdout == padded(&file_bytes(2)),
        "dd read {} bytes of file 2 that are not the {} written",
        out.stdout.len(),
        padded(&file_bytes(2)).len()
    );
    mt(&["rewind"]);
    mt(&["setblk", "0"]);
    let out = Command::new("dd")
        .arg(format!("if={}", dev()))
        .arg(format!("bs={}", 2 * PAYLOAD))
        .arg("count=1")
        .arg("status=none")
        .stderr(Stdio::inherit())
        .output()
        .expect("run dd");
    assert!(out.status.success(), "the variable-mode read of one record");
    assert_eq!(
        out.stdout.len(),
        PAYLOAD,
        "a record on the medium is the payload alone ({PAYLOAD} bytes); {} means the CRC is \
         stored with it",
        out.stdout.len()
    );
    assert_page_clear("after the heir's reads");
}

/// `TAPECTL_TEST_LBP_CORRUPT_BLOCK=n` (F5; the hook takes a list, here of one):
/// one block goes to the drive with a
/// CRC bit flipped, the drive refuses it, and the store says so as
/// [`TapectlError::LbpRefusal`]. Then the steps item 174's retry takes, at
/// the store: the drive's position (READ POSITION) names the file holding the
/// first block it did not take, the store returns to that file's planned
/// start, and the files from there are written again with true CRCs (the
/// one listed block is past), which the drive takes. Every file reads back.
/// The session's own retry over these steps is unit-tested in
/// `volume::session`; this proves the drive and st give it what it relies
/// on.
#[test]
#[ignore]
fn mhvtl_lbp_a_refused_block_is_an_lbp_refusal_and_the_store_writes_on_from_the_files_start() {
    let Some((_g, _leave)) = start() else { return };

    // The second block of HOOKED_FILE, counted from 1 since the open.
    let corrupt = u64::from(HOOKED_FILE) * FILE_BLOCKS + 2;
    // Set for this one open, under the tape lock, and removed before any
    // other store opens (the hook is read in `TapeStore::open` only).
    std::env::set_var(TEST_LBP_CORRUPT_BLOCK, corrupt.to_string());
    let opened = TapeStore::open(&dev(), PAYLOAD, u64::MAX);
    std::env::remove_var(TEST_LBP_CORRUPT_BLOCK);
    let mut store = opened.unwrap();
    assert_eq!(store.protection(), protection_for(true), "the hooked open");

    let mut refused = None;
    for i in 0..FILES {
        let bytes = file_bytes(i);
        match store.execute(&mut &bytes[..], bytes.len() as u64, i == FILES - 1) {
            Ok(_) => {}
            Err(TapectlError::LbpRefusal(text)) => {
                refused = Some((i, text));
                break;
            }
            Err(e) => panic!("file {i}: a refusal under LBP must be LbpRefusal, got {e:?}"),
        }
    }
    let (failed, text) = refused.unwrap_or_else(|| {
        panic!("block {corrupt} went with a flipped CRC and no write was refused")
    });
    eprintln!("refused in file {failed}: {text}");
    assert!(
        failed >= HOOKED_FILE,
        "the refusal came in file {failed}, before the corrupted block's file {HOOKED_FILE}"
    );
    let lba = store
        .logical_position()
        .expect("READ POSITION after the refusal")
        .expect("TapeStore reads the drive's position");
    let from = planned_file_at(lba);
    eprintln!("the drive's position after the refusal: logical object {lba}, in file {from}");
    assert!(
        (HOOKED_FILE..=failed).contains(&from),
        "the drive's position (logical object {lba}, file {from}) is not in a file from the \
         corrupted block's ({HOOKED_FILE}) to the refused write's ({failed}): item 174 could \
         not find where to write on from"
    );
    assert!(
        page_now().armed_as(bits_for(true).unwrap()),
        "the page after the refusal, store still open: {:02x?}",
        page_now().bytes()
    );

    store
        .reposition_for_resume(from)
        .expect("return to the planned start of the file");
    write_files(&mut store, from);
    drop(store);
    assert_page_clear("after the retried write's store closed");

    read_back_all("the tape written on from the refused file's start");
}

/// The close (§4.3): a write that stopped inside a file leaves st owing a
/// filemark; the device writes it while the page that protected the blocks is
/// still armed, then puts the page back. The file holds exactly the blocks
/// written, ended by that filemark, and reads back with LBP_R.
#[test]
#[ignore]
fn mhvtl_lbp_the_close_writes_the_owed_filemark_and_clears_the_page() {
    let Some((_g, _leave)) = start() else { return };

    let data = pattern(5 * PAYLOAD, 99);
    let mut device = TapeDevice::open(&dev(), PAYLOAD).unwrap();
    assert_eq!(device.protection(), protection_for(true), "the write open");
    device.rewind().unwrap();
    let mut src = FailsAfter {
        data: data.clone(),
        pos: 0,
        at: 3 * PAYLOAD + 10,
    };
    let err = device
        .write_stream(&mut src, data.len() as u64, false)
        .expect_err("the source fails inside the fourth block");
    assert!(
        matches!(err, TapectlError::SourceIo(_)),
        "a failed source is the disk's error, not the tape's: {err:?}"
    );
    assert!(
        page_now().armed_as(bits_for(true).unwrap()),
        "the page with the write stopped, device still open: {:02x?}",
        page_now().bytes()
    );
    drop(device);
    assert_page_clear("after the device closed with a block write last");

    let mut device = TapeDevice::open_read(&dev(), PAYLOAD).unwrap();
    assert_eq!(device.protection(), protection_for(false), "the read open");
    device.rewind().unwrap();
    let mut out = Vec::new();
    let (n, end) = device
        .read_file_streaming(&mut out)
        .expect("file 0 reads back");
    assert_eq!(
        n,
        (3 * PAYLOAD) as u64,
        "file 0 holds the three whole blocks written"
    );
    assert_eq!(
        end,
        ReadEnd::Filemark,
        "file 0 ends at the close's filemark, not at the end of data"
    );
    assert!(
        out == data[..3 * PAYLOAD],
        "file 0's bytes are not the blocks written"
    );
    drop(device);
    assert_page_clear("after the read device closed");
}

/// §4.4, item 164: a page a crash left armed (here armed by hand, through the
/// library's own `sg_wr_mode`) is taken over by the next open that can arm
/// and cleared when it closes, never restored as found; an open that cannot
/// arm clears it at once.
#[test]
#[ignore]
fn mhvtl_lbp_a_page_left_armed_is_cleared_by_the_next_open() {
    let Some((_g, _leave)) = start() else { return };
    let both = lbp::LBP_W | lbp::LBP_R;
    let arm_by_hand = || {
        let c = control();
        let found = c.sense().expect("MODE SENSE");
        c.select(&found.with_protection(lbp::METHOD_RS, lbp::RS_INFO_LEN, both))
            .expect("arm the page by hand");
        assert!(
            page_now().armed_as(both),
            "the hand-armed page did not take: {:02x?}",
            page_now().bytes()
        );
    };

    arm_by_hand();
    let store = TapeStore::open_read(&dev(), PAYLOAD).unwrap();
    assert_eq!(
        store.protection(),
        protection_for(false),
        "a read open over a leftover"
    );
    match lbp::LBP_R_POLICY {
        LbpReadPolicy::AllReads => assert!(
            page_now().armed_as(both),
            "the leftover is re-armed for this open: {:02x?}",
            page_now().bytes()
        ),
        LbpReadPolicy::OnlyConfirmOfLbpWrite => {
            assert_page_clear("a read open that arms nothing clears the leftover at once")
        }
    }
    drop(store);
    assert_page_clear("after the open that found a leftover closed");

    arm_by_hand();
    let store = TapeStore::open(&dev(), FORMAT_BLOCK, u64::MAX).unwrap();
    assert!(
        matches!(store.protection(), Protection::Unavailable(_)),
        "a {}-byte wire block on mhvtl falls back: {:?}",
        FORMAT_BLOCK + lbp::CRC_LEN,
        store.protection()
    );
    assert_page_clear("an open that cannot arm clears the leftover before it returns");
    drop(store);
    assert_page_clear("after the fallen-back open closed");
}

/// 388c's finding (#388, F12): after a read that ran off the data, st
/// answers the next read itself, without sending it, so a probe read where
/// the head was came back without the EBUSY that says mhvtl cannot carry a
/// 524,292-byte block: the open armed and its first transfer failed EBUSY
/// (`volume identify` after a failed `dd` exited 2; after `mt rewind`, 0).
/// The open now rewinds before it probes. Here a tape at the format's block
/// (written plain: the open falls back), a plain read run off its end, and
/// then each open must still fall back by cause 8 and read the tape whole.
#[test]
#[ignore]
fn mhvtl_lbp_an_open_after_a_read_off_the_data_still_probes_the_drive() {
    let Some((_g, _leave)) = start() else { return };
    let wire = FORMAT_BLOCK + lbp::CRC_LEN;
    let cannot_carry = |what: &str, p: Protection| match p {
        Protection::Unavailable(reason) => assert!(
            reason.starts_with(&format!("st cannot carry a {wire}-byte block")),
            "{what}: the fallback's reason is not the probe's: {reason}"
        ),
        other => panic!(
            "{what}: a {wire}-byte wire block on mhvtl must fall back by the probe (cause 8), \
             got {other:?}"
        ),
    };
    let files: Vec<Vec<u8>> = (0..2u64)
        .map(|i| pattern(FORMAT_BLOCK + 1000 * (i as usize + 1), 40 + i))
        .collect();
    let padded_to_format = |bytes: &[u8]| {
        let mut v = bytes.to_vec();
        v.resize(bytes.len().div_ceil(FORMAT_BLOCK) * FORMAT_BLOCK, 0);
        v
    };

    let mut store = TapeStore::open(&dev(), FORMAT_BLOCK, u64::MAX).unwrap();
    cannot_carry("the write open, from BOT", store.protection());
    for (i, bytes) in files.iter().enumerate() {
        store
            .execute(&mut &bytes[..], bytes.len() as u64, i == files.len() - 1)
            .unwrap_or_else(|e| panic!("write of file {i}: {e}"));
    }
    drop(store);

    // Off the data: the head is past the last filemark, at the end of data.
    // A plain read there, as `dd` past a tape's end or RESTORE.sh on an
    // unsealed tape makes, leaves st answering reads itself.
    let run_off = |when: &str| {
        let mut node = std::fs::File::open(dev()).expect("open the node for a plain read");
        let mut buf = vec![0u8; FORMAT_BLOCK];
        let answers: Vec<String> = (0..3)
            .map(|_| match node.read(&mut buf) {
                Ok(n) => format!("{n} bytes"),
                Err(e) => format!("{e}"),
            })
            .collect();
        eprintln!("{when}: plain reads at the end of data: {answers:?}");
        assert!(
            answers
                .iter()
                .all(|a| !a.starts_with(&format!("{FORMAT_BLOCK} "))),
            "{when}: a plain read at the end of data returned a block: {answers:?}"
        );
    };

    run_off("before the read open");
    let mut store = TapeStore::open_read(&dev(), FORMAT_BLOCK).unwrap();
    match bits_for(false) {
        Some(_) => cannot_carry(
            "the read open after a read off the data",
            store.protection(),
        ),
        None => assert_eq!(store.protection(), Protection::NotApplicable),
    }
    assert_page_clear("while the read store that fell back holds the drive");
    for (i, bytes) in files.iter().enumerate() {
        let mut out = Vec::new();
        store
            .read_file(i as u32, &mut out)
            .unwrap_or_else(|e| panic!("read of file {i} after the open: {e}"));
        assert!(
            out == padded_to_format(bytes),
            "file {i} read back {} bytes that differ from the {} written (padded)",
            out.len(),
            padded_to_format(bytes).len()
        );
    }
    drop(store);
    assert_page_clear("after the read store closed");

    run_off("before the write open");
    let store = TapeStore::open(&dev(), FORMAT_BLOCK, u64::MAX).unwrap();
    cannot_carry(
        "the write open after a read off the data",
        store.protection(),
    );
    assert_page_clear("while the write store that fell back holds the drive");
    drop(store);
    assert_page_clear("after the write store closed");
}

// ── the gate's page reader, ungated ──────────────────────────────────────────

/// The text of the shell function `name` in the gate script: from `name() {`
/// to the first line that is just `}` (`tests/heir_restore_sh.rs`'s way).
fn gate_function(name: &str) -> String {
    let gate = std::fs::read_to_string(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("scripts/mhvtl-verify-gate.sh"),
    )
    .expect("the gate script");
    let start = gate
        .find(&format!("\n{name}() {{"))
        .unwrap_or_else(|| panic!("the gate has no function {name}"))
        + 1;
    let rest = &gate[start..];
    let end = rest.find("\n}\n").expect("the function ends") + 3;
    rest[..end].to_string()
}

/// Run the gate's `lbp_page_parse` on `resp`: its status and its output.
fn gate_parse(resp: &[u8]) -> (bool, String) {
    use std::io::Write;
    let script = format!("{}\nlbp_page_parse\n", gate_function("lbp_page_parse"));
    let mut child = Command::new("bash")
        .args(["-c", &script])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("run bash");
    child.stdin.take().unwrap().write_all(resp).unwrap();
    let o = child.wait_with_output().unwrap();
    (
        o.status.success(),
        format!(
            "{}{}",
            String::from_utf8_lossy(&o.stdout).trim(),
            String::from_utf8_lossy(&o.stderr).trim()
        ),
    )
}

/// MODE SENSE(10) answers as the two drives give them (`sg_modes --raw`):
/// mhvtl's TD6 with its 8-byte block descriptor and 32-byte page (A3,
/// 2026-10-25), and the HP LTO-6's 8-byte page with SPF set (`4a`), each clear
/// and armed. The gate reads the same method, length and flags as the
/// library's parser on every one, and refuses an answer that holds no
/// Control Data Protection page.
#[test]
fn the_gates_page_reader_reads_what_the_library_reads() {
    let mhvtl = |method: u8, len: u8, flags: u8| {
        let mut r: Vec<u8> = vec![0x00, 0x2c, 0x68, 0x10, 0x00, 0x00, 0x00, 0x08];
        r.extend([0x5a, 0x00, 0x00, 0x00, 0x00, 0x08, 0x00, 0x00]);
        let mut page: Vec<u8> = vec![0x0a, 0xf0, 0x00, 0x1c, method, len, flags, 0x00];
        page.resize(32, 0);
        r.extend(page);
        r
    };
    let hp = |method: u8, len: u8, flags: u8| {
        let mut r: Vec<u8> = vec![0x00, 0x0e, 0x00, 0x10, 0x00, 0x00, 0x00, 0x00];
        r.extend([0x4a, 0xf0, 0x00, 0x04, method, len, flags, 0x00]);
        r
    };
    let cases = [
        (
            "mhvtl clear",
            mhvtl(0x00, 0x04, 0x00),
            "method=00 info_len=04 flags=00",
        ),
        (
            "mhvtl armed",
            mhvtl(0x01, 0x04, 0xc0),
            "method=01 info_len=04 flags=c0",
        ),
        (
            "hp clear",
            hp(0x00, 0x00, 0x00),
            "method=00 info_len=00 flags=00",
        ),
        (
            "hp armed",
            hp(0x01, 0x04, 0xc0),
            "method=01 info_len=04 flags=c0",
        ),
        // Bits of byte 6 beside LBP_W and LBP_R are not the checks.
        (
            "hp write only, RBDP",
            hp(0x01, 0x04, 0xa0),
            "method=01 info_len=04 flags=80",
        ),
    ];
    for (what, resp, want) in cases {
        let lib = ProtectionPage::from_mode_sense10(&resp).unwrap();
        let lib_says = format!(
            "method={:02x} info_len={:02x} flags={:02x}",
            lib.method(),
            lib.info_len(),
            lib.flags()
        );
        assert_eq!(lib_says, want, "{what}: the library");
        let (ok, got) = gate_parse(&resp);
        assert!(ok, "{what}: the gate's reader failed: {got}");
        assert_eq!(
            got, want,
            "{what}: the gate's reader disagrees with the library"
        );
    }
    // Not the page, and too short: refused, never read as method 0.
    let mut other = hp(0, 0, 0);
    other[8] = 0x0f;
    let short: Vec<u8> = vec![0x00, 0x06, 0x00];
    for (what, resp) in [("page 0Fh", other), ("short", short)] {
        let (ok, got) = gate_parse(&resp);
        assert!(
            !ok,
            "{what}: the gate's reader must refuse it, said {got:?}"
        );
    }
}
