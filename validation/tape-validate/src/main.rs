//! Milestone 0: tape ioctl validation
//!
//! Tests from the design document:
//! - Open /dev/nst0, set variable block mode (MTSETBLK 0)
//! - Write a test file, write file mark (MTWEOFI)
//! - Read back and compare
//! - MTIOCGET for position
//! - Rewind, seek forward (FSF), verify position
//!
//! Prerequisites:
//!   - mhvtl installed and configured (or real tape drive)
//!   - /dev/nst0 accessible
//!
//! Run with: cargo run --release -- [/dev/nst0]

use std::env;
use std::fs::OpenOptions;
use std::io::{Read, Write};
use std::os::unix::io::AsRawFd;

use sha2::{Digest, Sha256};

// Linux tape ioctl constants from <linux/mtio.h>
const MTIOCTOP: u64 = 0x40086d01; // _IOW('m', 1, struct mtop)
const MTIOCGET: u64 = 0x80306d02; // _IOR('m', 2, struct mtget)

// mtop operations
const MTREW: i16 = 6;    // rewind
const MTWEOF: i16 = 5;   // write EOF mark (synchronous)
const MTWEOFI: i16 = 35; // write EOF mark (immediate, no flush)
const MTSETBLK: i16 = 20; // set block size
const MTFSF: i16 = 1;    // forward space filemark
const MTBSF: i16 = 2;    // backward space filemark

// mtop struct: { i16 mt_op, i16 _pad, i32 mt_count }
#[repr(C)]
struct MtOp {
    mt_op: i16,
    _pad: i16,
    mt_count: i32,
}

// mtget struct (simplified — we only need mt_fileno and mt_blkno)
// Full struct is 48 bytes on x86_64
#[repr(C)]
#[derive(Debug, Default)]
struct MtGet {
    mt_type: i64,
    mt_resid: i64,
    mt_dsreg: i64,
    mt_gstat: i64,
    mt_erreg: i64,
    mt_fileno: i32,
    mt_blkno: i32,
}

fn main() {
    let device = env::args().nth(1).unwrap_or_else(|| "/dev/nst0".to_string());

    println!("=== Milestone 0: tape ioctl validation ===");
    println!("device: {device}\n");

    let mut passed = 0;
    let mut failed = 0;

    // Open device
    print!("test: open device ... ");
    let mut fd = match OpenOptions::new().read(true).write(true).open(&device) {
        Ok(f) => {
            println!("ok (fd={})", f.as_raw_fd());
            passed += 1;
            f
        }
        Err(e) => {
            println!("FAIL - {e}");
            println!("\nCannot open {device}. Ensure mhvtl is set up:");
            println!("  1. Install mhvtl: apt install mhvtl");
            println!("  2. Start service: systemctl start mhvtl");
            println!("  3. Load tape:     mtx -f /dev/sgN load 1 0");
            println!("  4. Verify:        mt -f {device} status");
            std::process::exit(1);
        }
    };

    let raw_fd = fd.as_raw_fd();

    // Set variable block mode
    print!("test: set variable block mode (MTSETBLK 0) ... ");
    if mt_ioctl(raw_fd, MTSETBLK, 0) {
        println!("ok");
        passed += 1;
    } else {
        println!("FAIL");
        failed += 1;
    }

    // Rewind
    print!("test: rewind ... ");
    if mt_ioctl(raw_fd, MTREW, 0) {
        println!("ok");
        passed += 1;
    } else {
        println!("FAIL");
        failed += 1;
    }

    // Check initial position
    print!("test: MTIOCGET initial position ... ");
    if let Some(pos) = mt_get_position(raw_fd) {
        if pos.0 == 0 && pos.1 == 0 {
            println!("ok (file={}, block={})", pos.0, pos.1);
            passed += 1;
        } else {
            println!("FAIL - expected (0,0), got ({}, {})", pos.0, pos.1);
            failed += 1;
        }
    } else {
        println!("FAIL - ioctl failed");
        failed += 1;
    }

    // Write test data (file 0)
    let test_data_0 = generate_test_data(64 * 1024); // 64 KB
    let hash_0 = sha256_hex(&test_data_0);
    print!(
        "test: write file 0 ({} bytes) ... ",
        test_data_0.len()
    );
    match fd.write_all(&test_data_0) {
        Ok(()) => {
            println!("ok");
            passed += 1;
        }
        Err(e) => {
            println!("FAIL - {e}");
            failed += 1;
        }
    }

    // Write file mark (MTWEOFI)
    print!("test: write file mark (MTWEOFI) ... ");
    if mt_ioctl(raw_fd, MTWEOFI, 1) {
        println!("ok");
        passed += 1;
    } else {
        println!("FAIL");
        failed += 1;
    }

    // Write second file (file 1)
    let test_data_1 = generate_test_data(128 * 1024); // 128 KB
    let hash_1 = sha256_hex(&test_data_1);
    print!(
        "test: write file 1 ({} bytes) ... ",
        test_data_1.len()
    );
    match fd.write_all(&test_data_1) {
        Ok(()) => {
            println!("ok");
            passed += 1;
        }
        Err(e) => {
            println!("FAIL - {e}");
            failed += 1;
        }
    }

    // Write final file mark (MTWEOF - synchronous)
    print!("test: write final file mark (MTWEOF) ... ");
    if mt_ioctl(raw_fd, MTWEOF, 1) {
        println!("ok");
        passed += 1;
    } else {
        println!("FAIL");
        failed += 1;
    }

    // Check position after writes
    print!("test: MTIOCGET after writes ... ");
    if let Some(pos) = mt_get_position(raw_fd) {
        println!("ok (file={}, block={})", pos.0, pos.1);
        passed += 1;
    } else {
        println!("FAIL");
        failed += 1;
    }

    // Rewind for read-back
    print!("test: rewind for read-back ... ");
    if mt_ioctl(raw_fd, MTREW, 0) {
        println!("ok");
        passed += 1;
    } else {
        println!("FAIL");
        failed += 1;
    }

    // Read file 0 and verify
    print!("test: read file 0 and verify sha256 ... ");
    let mut read_buf = vec![0u8; test_data_0.len() + 1024]; // extra room
    match fd.read(&mut read_buf) {
        Ok(n) => {
            let read_hash = sha256_hex(&read_buf[..n]);
            if n == test_data_0.len() && read_hash == hash_0 {
                println!("ok ({n} bytes, hash match)");
                passed += 1;
            } else {
                println!(
                    "FAIL - size: {n} vs {}, hash: {} vs {}",
                    test_data_0.len(),
                    &read_hash[..16],
                    &hash_0[..16]
                );
                failed += 1;
            }
        }
        Err(e) => {
            println!("FAIL - {e}");
            failed += 1;
        }
    }

    // Skip to file 1 (forward space filemark)
    print!("test: forward space filemark (MTFSF 1) ... ");
    if mt_ioctl(raw_fd, MTFSF, 1) {
        println!("ok");
        passed += 1;
    } else {
        println!("FAIL");
        failed += 1;
    }

    // Verify position is file 1
    print!("test: verify position at file 1 ... ");
    if let Some(pos) = mt_get_position(raw_fd) {
        if pos.0 == 1 {
            println!("ok (file={}, block={})", pos.0, pos.1);
            passed += 1;
        } else {
            println!("FAIL - expected file=1, got file={}", pos.0);
            failed += 1;
        }
    } else {
        println!("FAIL");
        failed += 1;
    }

    // Read file 1 and verify
    print!("test: read file 1 and verify sha256 ... ");
    let mut read_buf = vec![0u8; test_data_1.len() + 1024];
    match fd.read(&mut read_buf) {
        Ok(n) => {
            let read_hash = sha256_hex(&read_buf[..n]);
            if n == test_data_1.len() && read_hash == hash_1 {
                println!("ok ({n} bytes, hash match)");
                passed += 1;
            } else {
                println!(
                    "FAIL - size: {n} vs {}, hash: {} vs {}",
                    test_data_1.len(),
                    &read_hash[..16],
                    &hash_1[..16]
                );
                failed += 1;
            }
        }
        Err(e) => {
            println!("FAIL - {e}");
            failed += 1;
        }
    }

    // Rewind, seek forward 2, verify at file 2
    print!("test: rewind + seek forward 2 files ... ");
    mt_ioctl(raw_fd, MTREW, 0);
    mt_ioctl(raw_fd, MTFSF, 2);
    if let Some(pos) = mt_get_position(raw_fd) {
        if pos.0 == 2 {
            println!("ok (file={}, block={})", pos.0, pos.1);
            passed += 1;
        } else {
            println!("FAIL - expected file=2, got file={}", pos.0);
            failed += 1;
        }
    } else {
        println!("FAIL");
        failed += 1;
    }

    // Clean up: rewind
    let _ = mt_ioctl(raw_fd, MTREW, 0);

    println!("\n=== Results: {passed} passed, {failed} failed ===");
    if failed > 0 {
        std::process::exit(1);
    }
}

fn mt_ioctl(fd: i32, op: i16, count: i32) -> bool {
    let mtop = MtOp {
        mt_op: op,
        _pad: 0,
        mt_count: count,
    };
    unsafe {
        nix::libc::ioctl(fd, MTIOCTOP, &mtop as *const MtOp) == 0
    }
}

fn mt_get_position(fd: i32) -> Option<(i32, i32)> {
    let mut mtget = MtGet::default();
    let result = unsafe {
        nix::libc::ioctl(fd, MTIOCGET, &mut mtget as *mut MtGet)
    };
    if result == 0 {
        Some((mtget.mt_fileno, mtget.mt_blkno))
    } else {
        None
    }
}

fn generate_test_data(size: usize) -> Vec<u8> {
    use rand::RngCore;
    let mut data = vec![0u8; size];
    rand::rng().fill_bytes(&mut data);
    data
}

fn sha256_hex(data: &[u8]) -> String {
    let hash = Sha256::digest(data);
    hash.iter().map(|b| format!("{b:02x}")).collect()
}
