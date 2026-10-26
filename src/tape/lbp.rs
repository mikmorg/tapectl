//! Logical Block Protection (#388; ADR-0012 items 161-166, 174, 175): the
//! Reed-Solomon CRC tapectl appends to every block it writes and checks on
//! every block it reads, and the drive's Control Data Protection mode page
//! (0Ah/F0h) that turns it on.
//!
//! **The CRC** is ECMA-319's, the one IBM's LTO SCSI reference prints in
//! Annex D and mhvtl's `usr/utils/reed-solomon.c` copies: a byte-wise shift
//! register over GF(2^8) (polynomial 0x11D), seed 0, no inversion, the four
//! bytes appended big-endian (measured on home2's HP LTO-6, 2026-10-20). It
//! is computed slicing-by-16 over tables built at compile time from the one
//! published table, `T0`; the code is GF(2)-linear, which is what lets the
//! sixteen bytes of a step be looked up independently.
//!
//! **The page** is set through sg3-utils, never with a hand-built CDB
//! (`docs/design/threat-model.md` §2, pinned by `tests/sg_boundary.rs`):
//! `sg_modes` reads it (MODE SENSE) and `sg_wr_mode` writes it back (MODE
//! SENSE, then MODE SELECT), read-modify-write, the PS bit cleared and
//! never saved, so a power cycle always clears it. The sg node is the one
//! the kernel binds to the st node tapectl opened (sysfs), never config's
//! `device_sg`: a MODE SELECT on the wrong node would arm another drive.
//!
//! [`arm`], [`disarm`] and [`clear_leftover`] are the whole life of the page;
//! `tape::ioctl::TapeDevice` calls them from its open and its `Drop`, and
//! nothing else does. They work over [`PageControl`] and two closures, so
//! every refusal and every restore is unit-tested without a drive.

use std::collections::BTreeSet;
use std::io;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

/// The bytes of protection information LBP adds to a block: one RS-CRC.
pub const CRC_LEN: usize = 4;

/// ECMA-319's table, `T0[a]`: the register after the byte `a` from state 0.
/// From mhvtl's `usr/utils/reed-solomon.c` (IBM's LTO SCSI reference, Annex
/// D), as `docs/research/lbp-prototype/lbp.py` carries it. The test
/// `the_table_is_the_gf256_code` derives every entry from the field instead.
const T0: [u32; 256] = [
    0x00000000, 0x38CF3801, 0x70837002, 0x484C4803, 0xE01BE004, 0xD8D4D805, 0x90989006, 0xA857A807,
    0xDD36DD08, 0xE5F9E509, 0xADB5AD0A, 0x957A950B, 0x3D2D3D0C, 0x05E2050D, 0x4DAE4D0E, 0x7561750F,
    0xA76CA710, 0x9FA39F11, 0xD7EFD712, 0xEF20EF13, 0x47774714, 0x7FB87F15, 0x37F43716, 0x0F3B0F17,
    0x7A5A7A18, 0x42954219, 0x0AD90A1A, 0x3216321B, 0x9A419A1C, 0xA28EA21D, 0xEAC2EA1E, 0xD20DD21F,
    0x53D85320, 0x6B176B21, 0x235B2322, 0x1B941B23, 0xB3C3B324, 0x8B0C8B25, 0xC340C326, 0xFB8FFB27,
    0x8EEE8E28, 0xB621B629, 0xFE6DFE2A, 0xC6A2C62B, 0x6EF56E2C, 0x563A562D, 0x1E761E2E, 0x26B9262F,
    0xF4B4F430, 0xCC7BCC31, 0x84378432, 0xBCF8BC33, 0x14AF1434, 0x2C602C35, 0x642C6436, 0x5CE35C37,
    0x29822938, 0x114D1139, 0x5901593A, 0x61CE613B, 0xC999C93C, 0xF156F13D, 0xB91AB93E, 0x81D5813F,
    0xA6ADA640, 0x9E629E41, 0xD62ED642, 0xEEE1EE43, 0x46B64644, 0x7E797E45, 0x36353646, 0x0EFA0E47,
    0x7B9B7B48, 0x43544349, 0x0B180B4A, 0x33D7334B, 0x9B809B4C, 0xA34FA34D, 0xEB03EB4E, 0xD3CCD34F,
    0x01C10150, 0x390E3951, 0x71427152, 0x498D4953, 0xE1DAE154, 0xD915D955, 0x91599156, 0xA996A957,
    0xDCF7DC58, 0xE438E459, 0xAC74AC5A, 0x94BB945B, 0x3CEC3C5C, 0x0423045D, 0x4C6F4C5E, 0x74A0745F,
    0xF575F560, 0xCDBACD61, 0x85F68562, 0xBD39BD63, 0x156E1564, 0x2DA12D65, 0x65ED6566, 0x5D225D67,
    0x28432868, 0x108C1069, 0x58C0586A, 0x600F606B, 0xC858C86C, 0xF097F06D, 0xB8DBB86E, 0x8014806F,
    0x52195270, 0x6AD66A71, 0x229A2272, 0x1A551A73, 0xB202B274, 0x8ACD8A75, 0xC281C276, 0xFA4EFA77,
    0x8F2F8F78, 0xB7E0B779, 0xFFACFF7A, 0xC763C77B, 0x6F346F7C, 0x57FB577D, 0x1FB71F7E, 0x2778277F,
    0x51475180, 0x69886981, 0x21C42182, 0x190B1983, 0xB15CB184, 0x89938985, 0xC1DFC186, 0xF910F987,
    0x8C718C88, 0xB4BEB489, 0xFCF2FC8A, 0xC43DC48B, 0x6C6A6C8C, 0x54A5548D, 0x1CE91C8E, 0x2426248F,
    0xF62BF690, 0xCEE4CE91, 0x86A88692, 0xBE67BE93, 0x16301694, 0x2EFF2E95, 0x66B36696, 0x5E7C5E97,
    0x2B1D2B98, 0x13D21399, 0x5B9E5B9A, 0x6351639B, 0xCB06CB9C, 0xF3C9F39D, 0xBB85BB9E, 0x834A839F,
    0x029F02A0, 0x3A503AA1, 0x721C72A2, 0x4AD34AA3, 0xE284E2A4, 0xDA4BDAA5, 0x920792A6, 0xAAC8AAA7,
    0xDFA9DFA8, 0xE766E7A9, 0xAF2AAFAA, 0x97E597AB, 0x3FB23FAC, 0x077D07AD, 0x4F314FAE, 0x77FE77AF,
    0xA5F3A5B0, 0x9D3C9DB1, 0xD570D5B2, 0xEDBFEDB3, 0x45E845B4, 0x7D277DB5, 0x356B35B6, 0x0DA40DB7,
    0x78C578B8, 0x400A40B9, 0x084608BA, 0x308930BB, 0x98DE98BC, 0xA011A0BD, 0xE85DE8BE, 0xD092D0BF,
    0xF7EAF7C0, 0xCF25CFC1, 0x876987C2, 0xBFA6BFC3, 0x17F117C4, 0x2F3E2FC5, 0x677267C6, 0x5FBD5FC7,
    0x2ADC2AC8, 0x121312C9, 0x5A5F5ACA, 0x629062CB, 0xCAC7CACC, 0xF208F2CD, 0xBA44BACE, 0x828B82CF,
    0x508650D0, 0x684968D1, 0x200520D2, 0x18CA18D3, 0xB09DB0D4, 0x885288D5, 0xC01EC0D6, 0xF8D1F8D7,
    0x8DB08DD8, 0xB57FB5D9, 0xFD33FDDA, 0xC5FCC5DB, 0x6DAB6DDC, 0x556455DD, 0x1D281DDE, 0x25E725DF,
    0xA432A4E0, 0x9CFD9CE1, 0xD4B1D4E2, 0xEC7EECE3, 0x442944E4, 0x7CE67CE5, 0x34AA34E6, 0x0C650CE7,
    0x790479E8, 0x41CB41E9, 0x098709EA, 0x314831EB, 0x991F99EC, 0xA1D0A1ED, 0xE99CE9EE, 0xD153D1EF,
    0x035E03F0, 0x3B913BF1, 0x73DD73F2, 0x4B124BF3, 0xE345E3F4, 0xDB8ADBF5, 0x93C693F6, 0xAB09ABF7,
    0xDE68DEF8, 0xE6A7E6F9, 0xAEEBAEFA, 0x962496FB, 0x3E733EFC, 0x06BC06FD, 0x4EF04EFE, 0x763F76FF,
];

/// `T0` advanced by `k` zero bytes, `k` = 0..15: the slicing tables.
const SLICES: [[u32; 256]; 16] = slicing_tables();

const fn slicing_tables() -> [[u32; 256]; 16] {
    let mut t = [[0u32; 256]; 16];
    t[0] = T0;
    let mut k = 1;
    while k < 16 {
        let mut a = 0;
        while a < 256 {
            let prev = t[k - 1][a];
            t[k][a] = (prev << 8) ^ T0[(prev >> 24) as usize];
            a += 1;
        }
        k += 1;
    }
    t
}

/// The RS-CRC of `data`, continuing from `seed` (0 for a block of its own).
/// Slicing-by-16: sixteen bytes per step, the register folded into the first
/// four, then byte by byte for the rest.
pub fn rs_crc(data: &[u8], seed: u32) -> u32 {
    let t = &SLICES;
    let mut crc = seed;
    let mut chunks = data.chunks_exact(16);
    for c in &mut chunks {
        let x = crc ^ u32::from_be_bytes([c[0], c[1], c[2], c[3]]);
        crc = t[15][(x >> 24) as usize]
            ^ t[14][((x >> 16) & 0xff) as usize]
            ^ t[13][((x >> 8) & 0xff) as usize]
            ^ t[12][(x & 0xff) as usize]
            ^ t[11][c[4] as usize]
            ^ t[10][c[5] as usize]
            ^ t[9][c[6] as usize]
            ^ t[8][c[7] as usize]
            ^ t[7][c[8] as usize]
            ^ t[6][c[9] as usize]
            ^ t[5][c[10] as usize]
            ^ t[4][c[11] as usize]
            ^ t[3][c[12] as usize]
            ^ t[2][c[13] as usize]
            ^ t[1][c[14] as usize]
            ^ t[0][c[15] as usize];
    }
    for &b in chunks.remainder() {
        crc = (crc << 8) ^ T0[((crc >> 24) as u8 ^ b) as usize];
    }
    crc
}

/// Fill the last [`CRC_LEN`] bytes of `block` with the big-endian RS-CRC of
/// the rest: one wire block, ready for the drive.
pub fn protect(block: &mut [u8]) {
    let n = block.len() - CRC_LEN;
    let crc = rs_crc(&block[..n], 0);
    block[n..].copy_from_slice(&crc.to_be_bytes());
}

/// A wire block whose CRC is not the one its bytes compute to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CrcMismatch {
    /// The CRC the block carried: what the drive sent.
    pub got: u32,
    /// The CRC its bytes compute to here.
    pub want: u32,
}

/// Check a wire block read from the drive: its last [`CRC_LEN`] bytes must
/// be the RS-CRC of the rest.
pub fn check(block: &[u8]) -> Result<(), CrcMismatch> {
    if block.len() < CRC_LEN {
        return Err(CrcMismatch { got: 0, want: 0 });
    }
    let n = block.len() - CRC_LEN;
    let got = u32::from_be_bytes([block[n], block[n + 1], block[n + 2], block[n + 3]]);
    let want = rs_crc(&block[..n], 0);
    if got == want {
        Ok(())
    } else {
        Err(CrcMismatch { got, want })
    }
}

// ── the page ────────────────────────────────────────────────────────────────

/// The Control Data Protection page: page code 0Ah …
pub const PAGE_CODE: u8 = 0x0A;
/// … subpage F0h.
pub const SUBPAGE: u8 = 0xF0;
/// LOGICAL BLOCK PROTECTION METHOD 01h: Reed-Solomon CRC (item 162).
pub const METHOD_RS: u8 = 0x01;
/// The protection information length that goes with it: 4 bytes.
pub const RS_INFO_LEN: u8 = CRC_LEN as u8;
/// LBP_W: the drive checks the CRC of every block it is sent to write.
pub const LBP_W: u8 = 0x80;
/// LBP_R: the drive sends the CRC with every block it reads.
pub const LBP_R: u8 = 0x40;

/// One Control Data Protection page, from its page-code byte on, as the
/// drive reported it (`sg_modes --raw`, current values).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProtectionPage {
    bytes: Vec<u8>,
}

impl ProtectionPage {
    /// The page from a MODE SENSE(10) response: the 8-byte header, the
    /// block descriptors it declares, then the page. The page length is
    /// read from the page, and clamped to what the response holds (mhvtl's
    /// header under-reports its mode data length).
    pub fn from_mode_sense10(resp: &[u8]) -> Result<Self, String> {
        if resp.len() < 8 {
            return Err(format!(
                "the MODE SENSE response is {} bytes, shorter than its own header",
                resp.len()
            ));
        }
        let bdl = u16::from_be_bytes([resp[6], resp[7]]) as usize;
        let start = 8 + bdl;
        let page = resp.get(start..).unwrap_or_default();
        Self::from_page_bytes(page)
    }

    /// The page from its own bytes (page code first).
    pub fn from_page_bytes(page: &[u8]) -> Result<Self, String> {
        if page.len() < 8 {
            return Err(format!(
                "the page is {} bytes; a Control Data Protection page is at least 8",
                page.len()
            ));
        }
        if page[0] & 0x3F != PAGE_CODE || page[1] != SUBPAGE {
            return Err(format!(
                "the drive answered page {:02X}h/{:02X}h, not 0Ah/F0h",
                page[0] & 0x3F,
                page[1]
            ));
        }
        let declared = u16::from_be_bytes([page[2], page[3]]) as usize + 4;
        let len = declared.min(page.len());
        Ok(Self {
            bytes: page[..len].to_vec(),
        })
    }

    /// The page's bytes, page code first.
    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }

    /// LOGICAL BLOCK PROTECTION METHOD.
    pub fn method(&self) -> u8 {
        self.bytes[4]
    }

    /// LOGICAL BLOCK PROTECTION INFORMATION LENGTH.
    pub fn info_len(&self) -> u8 {
        self.bytes[5]
    }

    /// The LBP_W and LBP_R bits.
    pub fn flags(&self) -> u8 {
        self.bytes[6] & (LBP_W | LBP_R)
    }

    /// Whether either check is on, whatever the method: a page tapectl did
    /// not leave this way is a leftover (a crashed tapectl, or a hand-run
    /// `sg_wr_mode`).
    pub fn is_armed(&self) -> bool {
        self.flags() != 0
    }

    /// Whether the page reads as Reed-Solomon with exactly `bits` on.
    pub fn armed_as(&self, bits: u8) -> bool {
        self.method() == METHOD_RS && self.info_len() == RS_INFO_LEN && self.flags() == bits
    }

    /// This page with the method, its length and the flag bits set — the
    /// rest of byte 6 and every other byte as the drive reported them — and
    /// PS cleared: a MODE SELECT must not set it, and the page is never
    /// saved. SPF and the page code stay.
    pub fn with_protection(&self, method: u8, info_len: u8, bits: u8) -> Self {
        let mut bytes = self.bytes.clone();
        bytes[0] &= 0x7F;
        bytes[4] = method;
        bytes[5] = info_len;
        bytes[6] = (bytes[6] & !(LBP_W | LBP_R)) | bits;
        Self { bytes }
    }

    /// This page with protection off: method 0, length 0, both checks off.
    pub fn cleared(&self) -> Self {
        self.with_protection(0, 0, 0)
    }

    /// `sg_wr_mode --contents=`'s value: the bytes in hex, comma-separated.
    pub fn contents_arg(&self) -> String {
        self.bytes
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect::<Vec<_>>()
            .join(",")
    }
}

/// The program that reads the page.
pub const SENSE_TOOL: &str = "sg_modes";
/// The program that sets it.
pub const SELECT_TOOL: &str = "sg_wr_mode";

/// The argv of the one MODE SENSE tapectl sends to read the page: current
/// values, raw bytes on stdout.
pub fn sense_argv(sg: &str) -> Vec<String> {
    vec![
        SENSE_TOOL.to_string(),
        "--page=0x0a,0xf0".to_string(),
        "--control=0".to_string(),
        "--raw".to_string(),
        sg.to_string(),
    ]
}

/// The argv of the one MODE SELECT tapectl sends: the Control Data
/// Protection page and no other, no block descriptor (`--dbd`, so st's block
/// length is untouched), PS clear, and never `--save`. Any other page is
/// refused here, before a program could be started.
pub fn select_argv(sg: &str, page: &ProtectionPage) -> Result<Vec<String>, String> {
    let b = page.bytes();
    if b.len() < 8 || b[0] & 0x3F != PAGE_CODE || b[1] != SUBPAGE {
        return Err(format!(
            "refusing a MODE SELECT of page {:02X}h/{:02X}h: tapectl sets the Control Data \
             Protection page (0Ah/F0h) and no other",
            b.first().map_or(0, |c| c & 0x3F),
            b.get(1).copied().unwrap_or(0)
        ));
    }
    let page = ProtectionPage {
        bytes: {
            let mut v = b.to_vec();
            v[0] &= 0x7F;
            v
        },
    };
    Ok(vec![
        SELECT_TOOL.to_string(),
        "--page=0x0a,0xf0".to_string(),
        "--dbd".to_string(),
        format!("--contents={}", page.contents_arg()),
        sg.to_string(),
    ])
}

/// sg3-utils' exit status for ILLEGAL REQUEST (`SG_LIB_CAT_ILLEGAL_REQ`):
/// the drive does not support the page, or refused the value.
const SG_EXIT_ILLEGAL_REQUEST: i32 = 5;
/// sg3-utils' exit status for a device it could not open
/// (`SG_LIB_FILE_ERROR`).
const SG_EXIT_FILE_ERROR: i32 = 15;

/// Why a page command failed, for the fallback's reason (item 165).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PageError {
    /// The program could not be started (sg3-utils missing).
    Spawn(String),
    /// The drive answered ILLEGAL REQUEST.
    IllegalRequest(String),
    /// The sg node could not be opened (permissions).
    Open(String),
    /// Any other failure, or an answer that does not parse.
    Other(String),
}

/// The page as the drive holds it, read and written: `sg_modes` and
/// `sg_wr_mode` on one sg node in production ([`SgPageControl`]), a recorded
/// page in tests.
pub trait PageControl {
    /// The sg node, for messages.
    fn node(&self) -> &str;
    /// MODE SENSE of the page, current values.
    fn sense(&self) -> Result<ProtectionPage, PageError>;
    /// MODE SELECT of `page` (which [`select_argv`] checks).
    fn select(&self, page: &ProtectionPage) -> Result<(), PageError>;
}

/// The real control: sg3-utils on the sg node the kernel binds to the st
/// node.
#[derive(Debug, Clone)]
pub struct SgPageControl {
    sg: String,
}

impl SgPageControl {
    pub fn new(sg: &str) -> Self {
        Self { sg: sg.to_string() }
    }
}

/// Run one pinned argv, stdin closed, stdout and stderr captured.
fn run(argv: &[String]) -> Result<Vec<u8>, PageError> {
    let out = crate::progress::waited(
        || format!("{} on {}", argv[0], argv[argv.len() - 1]),
        || {
            Command::new(&argv[0])
                .args(&argv[1..])
                .stdin(Stdio::null())
                .output()
        },
    )
    .map_err(|e| PageError::Spawn(format!("{} could not be run: {e}", argv[0])))?;
    if out.status.success() {
        return Ok(out.stdout);
    }
    let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
    let text = format!("{} exited {}: {stderr}", argv[0], out.status);
    Err(match out.status.code() {
        Some(SG_EXIT_ILLEGAL_REQUEST) => PageError::IllegalRequest(text),
        Some(SG_EXIT_FILE_ERROR) => PageError::Open(text),
        _ => PageError::Other(text),
    })
}

impl PageControl for SgPageControl {
    fn node(&self) -> &str {
        &self.sg
    }

    fn sense(&self) -> Result<ProtectionPage, PageError> {
        let raw = run(&sense_argv(&self.sg))?;
        ProtectionPage::from_mode_sense10(&raw).map_err(PageError::Other)
    }

    fn select(&self, page: &ProtectionPage) -> Result<(), PageError> {
        let argv = select_argv(&self.sg, page).map_err(PageError::Other)?;
        run(&argv).map(|_| ())
    }
}

/// The page control for the st node `device_tape` (F13): the sg node the
/// kernel binds to it in sysfs, never one named in config. `None` when the
/// binding cannot be proven (a test's path, the sg module not loaded, no
/// such node).
pub fn control_for(device_tape: &str) -> Option<SgPageControl> {
    let sg = crate::tape::drive_identity::derive_sg_for(device_tape)?;
    Some(SgPageControl::new(&sg.display().to_string()))
}

// ── what an open decided ────────────────────────────────────────────────────

/// What a device's open decided about Logical Block Protection: what the
/// write session records (migration 041, `volumes.write_lbp`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum Protection {
    /// The page is armed: the drive checks tapectl's CRC on every block
    /// written (LBP_W), and, when `reads`, sends its CRC with every block
    /// read (LBP_R), which tapectl checks.
    Lbp { reads: bool },
    /// LBP was tried and could not be armed; the reason (item 165). A write
    /// goes on without it, as strong as a write by 1.7.1.
    Unavailable(String),
    /// Not tried: a test double, variable-block mode, or a read open the
    /// read policy leaves unarmed ([`LbpReadPolicy`]).
    #[default]
    NotApplicable,
}

impl Protection {
    /// Whether the drive checks what this device writes.
    pub fn writes_checked(&self) -> bool {
        matches!(self, Protection::Lbp { .. })
    }
}

/// Which reads LBP_R is armed for (ADR-0012 item 175). The build carries
/// both; the probe on the real drive decides which one ships.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LbpReadPolicy {
    /// Every read tapectl makes is checked (item 162): writes, confirms,
    /// verifies, restores and every other reader arm LBP_W and LBP_R.
    AllReads,
    /// The fallback item 175 rules for a drive that returns no valid CRC for
    /// blocks written without LBP: LBP_R only for the confirm of a session
    /// that wrote with LBP, never for `volume verify`, a restore or the
    /// other readers. A write open arms LBP_W alone; the session arms LBP_R
    /// before its confirm ([`crate::store::Store::arm_read_check`]).
    OnlyConfirmOfLbpWrite,
}

/// The read policy this build ships (item 175's switch; item 162's default).
pub const LBP_R_POLICY: LbpReadPolicy = LbpReadPolicy::AllReads;

impl LbpReadPolicy {
    /// The flag bits an open arms: `None` for an open that arms nothing.
    pub fn open_bits(self, write_open: bool) -> Option<u8> {
        match (self, write_open) {
            (LbpReadPolicy::AllReads, _) => Some(LBP_W | LBP_R),
            (LbpReadPolicy::OnlyConfirmOfLbpWrite, true) => Some(LBP_W),
            (LbpReadPolicy::OnlyConfirmOfLbpWrite, false) => None,
        }
    }
}

/// The page as an open armed it: what [`disarm`] puts back.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Armed {
    /// The page as found at open, before tapectl changed it.
    pub found: ProtectionPage,
    /// The page as found was already armed: a leftover, cleared at close
    /// rather than restored.
    pub leftover: bool,
    /// The flag bits armed now.
    pub bits: u8,
}

impl Armed {
    /// The page [`disarm`] writes back: as found, or cleared for a leftover.
    pub fn restore_page(&self) -> ProtectionPage {
        if self.leftover {
            self.found.cleared()
        } else {
            self.found.clone()
        }
    }
}

/// Why an open could not arm the page (item 165's fallback), and whether it
/// found a leftover on the way.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NotArmed {
    pub reason: String,
    pub leftover: bool,
}

/// The reason when no sg node is bound to the st node (§5.1 cause 1).
pub fn reason_no_sg_node(device_tape: &str) -> String {
    format!("no SCSI generic node is bound to {device_tape} (is the sg module loaded?)")
}

/// The reason for a failed MODE SENSE (causes 2 and 3).
fn reason_sense(e: &PageError) -> String {
    match e {
        PageError::Spawn(m) => m.clone(),
        PageError::IllegalRequest(m) | PageError::Other(m) | PageError::Open(m) => {
            format!("the drive has no Control Data Protection page (0Ah/F0h): {m}")
        }
    }
}

/// The reason for a failed MODE SELECT (causes 2, 4 and 5).
fn reason_select(node: &str, e: &PageError) -> String {
    match e {
        PageError::Spawn(m) => m.clone(),
        PageError::Open(m) => format!("sg_wr_mode could not open {node} for writing: {m}"),
        PageError::IllegalRequest(m) => {
            format!("the drive refused Reed-Solomon LBP (method 01h): {m}")
        }
        PageError::Other(m) => format!("the drive's page could not be set: {m}"),
    }
}

/// The reason for a page that does not read back as set (cause 6).
pub fn reason_did_not_take(page: &ProtectionPage) -> String {
    format!(
        "the drive's page did not take: it reads back method {:02X}h, information length {}, \
         flags {:02X}h",
        page.method(),
        page.info_len(),
        page.flags()
    )
}

/// The reason for an st that refused the wire block size (cause 7).
pub fn reason_block_size(wire: usize, e: &str) -> String {
    format!("st refused a {wire}-byte block size: {e}")
}

/// The reason for an st that cannot carry a wire block in one command
/// (cause 8).
pub fn reason_cannot_carry(wire: usize, e: &io::Error) -> String {
    format!("st cannot carry a {wire}-byte block on this host's SCSI path ({e})")
}

/// Whether the probe read's answer means st never sent the request: it
/// could not map a buffer that large (mhvtl's 512 KiB per command; an HBA
/// capped at 512 KiB) or allocate one.
fn probe_not_issued(e: &io::Error) -> bool {
    matches!(
        e.raw_os_error(),
        Some(nix::libc::EBUSY) | Some(nix::libc::EOVERFLOW) | Some(nix::libc::ENOMEM)
    )
}

/// Arm the page with `bits` for one open (§2.2 step 4; items 163, 164):
///
/// 1. MODE SENSE: the page as found. One already armed is a leftover.
/// 2. MODE SELECT of the same page, Reed-Solomon with `bits`, PS clear.
/// 3. MODE SENSE: it must read back exactly so (never trusting the
///    changeable mask; HP applies parameters up to the first error).
/// 4. `set_block_size(wire)`: st sends a block descriptor and no page.
/// 5. `probe(wire)`: one read of a wire block where the head is. Only an
///    answer that says st never sent it ([`probe_not_issued`]) fails; data,
///    a filemark, ENOSPC or EIO all mean the path carried the request. That
///    holds only for a read st sends: after a read that failed or ran off
///    the data, st answers the next one itself, so the caller rewinds first
///    (`TapeDevice::arm_protection`, 388c's finding) and the probe reads at
///    BOT.
///
/// Any failure after the select restores the page (as found, or cleared
/// for a leftover) and the block size `payload`, and is [`NotArmed`]: the
/// open goes on without LBP (item 165).
pub fn arm(
    control: &dyn PageControl,
    bits: u8,
    payload: usize,
    set_block_size: &mut dyn FnMut(usize) -> Result<(), String>,
    probe: &mut dyn FnMut(usize) -> io::Result<usize>,
) -> Result<Armed, NotArmed> {
    let wire = payload + CRC_LEN;
    let found = control.sense().map_err(|e| NotArmed {
        reason: reason_sense(&e),
        leftover: false,
    })?;
    let leftover = found.is_armed();
    let armed = Armed {
        found: found.clone(),
        leftover,
        bits,
    };
    let want = found.with_protection(METHOD_RS, RS_INFO_LEN, bits);
    if let Err(e) = control.select(&want) {
        let reason = reason_select(control.node(), &e);
        return Err(not_armed(control, &armed, reason, None, set_block_size));
    }
    match control.sense() {
        Ok(now) if now.armed_as(bits) => {}
        Ok(now) => {
            let reason = reason_did_not_take(&now);
            return Err(not_armed(control, &armed, reason, None, set_block_size));
        }
        Err(e) => {
            let reason = reason_sense(&e);
            return Err(not_armed(control, &armed, reason, None, set_block_size));
        }
    }
    if let Err(e) = set_block_size(wire) {
        let reason = reason_block_size(wire, &e);
        return Err(not_armed(
            control,
            &armed,
            reason,
            Some(payload),
            set_block_size,
        ));
    }
    match probe(wire) {
        Err(e) if probe_not_issued(&e) => {
            let reason = reason_cannot_carry(wire, &e);
            Err(not_armed(
                control,
                &armed,
                reason,
                Some(payload),
                set_block_size,
            ))
        }
        _ => Ok(armed),
    }
}

/// [`arm`]'s way out: the page put back (HP may have applied part of a
/// refused select), the block size set back to `payload` when it was
/// changed, and the reason. Errors putting things back are not reported:
/// the reason already says LBP is off, and the close clears a page left armed.
fn not_armed(
    control: &dyn PageControl,
    armed: &Armed,
    reason: String,
    payload: Option<usize>,
    set_block_size: &mut dyn FnMut(usize) -> Result<(), String>,
) -> NotArmed {
    let _ = control.select(&armed.restore_page());
    if let Some(payload) = payload {
        let _ = set_block_size(payload);
    }
    NotArmed {
        reason,
        leftover: armed.leftover,
    }
}

/// Change an armed page's flag bits (item 175: LBP_R added for the confirm
/// of a session that wrote with LBP). The method and the block size are
/// already set; nothing moves. On failure the page is put back as it was.
pub fn rearm(control: &dyn PageControl, armed: &mut Armed, bits: u8) -> Result<(), String> {
    let now = control.sense().map_err(|e| reason_sense(&e))?;
    let want = now.with_protection(METHOD_RS, RS_INFO_LEN, bits);
    if let Err(e) = control.select(&want) {
        return Err(reason_select(control.node(), &e));
    }
    match control.sense() {
        Ok(back) if back.armed_as(bits) => {
            armed.bits = bits;
            Ok(())
        }
        Ok(back) => {
            let _ = control.select(&now);
            Err(reason_did_not_take(&back))
        }
        Err(e) => {
            let _ = control.select(&now);
            Err(reason_sense(&e))
        }
    }
}

/// Put the page back as [`arm`] found it (a leftover cleared) and check
/// that it reads back with both checks off and the method found (§4.3 step
/// 3). The caller has flushed the drive's buffer first.
pub fn disarm(control: &dyn PageControl, armed: &Armed) -> Result<(), String> {
    let restore = armed.restore_page();
    control
        .select(&restore)
        .map_err(|e| reason_select(control.node(), &e))?;
    let back = control.sense().map_err(|e| reason_sense(&e))?;
    if back.flags() != restore.flags() || back.method() != restore.method() {
        return Err(reason_did_not_take(&back));
    }
    Ok(())
}

/// For an open that does not arm (variable-block mode, a read the policy
/// leaves unarmed): read the page, and clear it if a leftover armed it
/// (item 164's "clears one a crash left armed at its next touch of the
/// drive"). `Ok(true)` when it cleared one. A drive with no page, or no sg
/// node, has nothing to clear.
pub fn clear_leftover(control: &dyn PageControl) -> Result<bool, String> {
    let found = control.sense().map_err(|e| reason_sense(&e))?;
    if !found.is_armed() {
        return Ok(false);
    }
    let cleared = found.cleared();
    control
        .select(&cleared)
        .map_err(|e| reason_select(control.node(), &e))?;
    Ok(true)
}

// ── the test hook's count ───────────────────────────────────────────────────

/// `TAPECTL_TEST_LBP_CORRUPT_BLOCK`'s blocks (#388, F5): the block writes,
/// counted from 1 since a store opened, whose CRC the hook corrupts. Every
/// block sent counts, re-sent ones too: item 174's retry sends its files
/// again on the same open, so a second number can land inside the retry and
/// provoke the second refusal. One count for `TapeDevice` and for the test
/// double that stands in for a drive (`store::MemStore`), so the two cannot
/// count differently.
///
/// The count is atomic, not a `Cell`, so a store holding it stays `Sync`
/// (the test double sits in shared `static` harnesses); one thread writes.
#[derive(Debug, Default)]
pub struct CorruptBlocks {
    listed: Vec<u64>,
    sent: AtomicU64,
}

impl CorruptBlocks {
    /// The hook for `listed` block writes (none: the hook is off).
    pub fn new(listed: &[u64]) -> Self {
        Self {
            listed: listed.to_vec(),
            sent: AtomicU64::new(0),
        }
    }

    /// Count one more block sent; whether the hook corrupts it.
    pub fn next_block(&self) -> bool {
        let sent = self.sent.fetch_add(1, Ordering::Relaxed) + 1;
        self.listed.contains(&sent)
    }
}

// ── what the operator is told ───────────────────────────────────────────────

/// The st nodes this process has already told about, each with the
/// direction of the open (§5.2: once per process per node). Keyed by
/// direction too, so a read open of a node never swallows a later write
/// open's item-165 warning (`volume rewrite` reads the tape it then writes).
static WARNED: Mutex<BTreeSet<(String, bool)>> = Mutex::new(BTreeSet::new());

/// Whether this is the first time this process tells about `device` for an
/// open in this direction (`write_open`).
fn first_warning(device: &str, write_open: bool) -> bool {
    WARNED
        .lock()
        .map(|mut seen| seen.insert((device.to_string(), write_open)))
        .unwrap_or(true)
}

/// The warning a write open gives when LBP could not be armed (item 165).
pub fn unavailable_warning(device: &str, reason: &str) -> String {
    format!(
        "warning: Logical Block Protection is not available on {device}: {reason}. tapectl \
         writes without it; the write is as strong as one by tapectl 1.7.1 (its sha256 checks \
         are unchanged), and the catalog records that LBP did not run."
    )
}

/// Tell the operator, once per process per st node and direction, that LBP
/// could not be armed: a write open on stderr and in the session log (item
/// 165); a read open in the session log only (F4: it cannot weaken a read).
pub fn note_unavailable(device: &str, reason: &str, write_open: bool) {
    if !first_warning(device, write_open) {
        return;
    }
    if write_open {
        // Once on stderr: this line, never also a tracing event at WARN,
        // whose stderr layer would print it a second time (and a `[logging]`
        // level above WARN would hide it, which item 165's warning must not
        // be).
        let line = unavailable_warning(device, reason);
        crate::progress::stderr_println(&line);
        crate::progress::log(&line);
    } else {
        // The session log alone: never also a tracing event, which main's
        // file layer would tee into the log a second time.
        crate::progress::log(&format!(
            "Logical Block Protection is not available on {device} for this read: {reason}"
        ));
    }
}

/// The note when an open found the page armed by someone else (§4.4):
/// cleared at once by an open that does not arm, or re-armed for this
/// command and cleared when it closes the drive.
pub fn leftover_note(device: &str, node: &str, cleared_now: bool) -> String {
    format!(
        "note: the drive behind {device} ({node}) had its Logical Block Protection page \
         armed when tapectl opened it: a tapectl that crashed or was killed, or a hand-run \
         sg_wr_mode, left it so. Until it is cleared, other programs' reads of this drive \
         (RESTORE.sh, dd) fail with I/O errors, never wrong data. {}",
        if cleared_now {
            "tapectl has cleared it."
        } else {
            "tapectl clears it when this command closes the drive."
        }
    )
}

/// The operator text of an EIO on a write while LBP is armed
/// ([`crate::error::TapectlError::LbpRefusal`]): what failed, and what an
/// EIO means under LBP.
pub fn refusal_text(what: &str, e: &io::Error) -> String {
    format!(
        "{what}: {e}. With Logical Block Protection armed this is how the drive refuses a block \
         whose CRC did not match the bytes it received: a block changed between tapectl and \
         the drive (memory, the HBA, the cable or the drive's buffer). A drive or medium error \
         reads the same through st, and the refused block may be in an earlier file, since st \
         and the drive write behind. The kernel log names a refusal \"logical block guard check \
         failed\"."
    )
}

/// The loud line when the close could not clear the page (§4.3).
pub fn clear_failed_line(device: &str, reason: &str) -> String {
    format!(
        "warning: could not clear the Logical Block Protection page of the drive behind \
         {device} ({reason}): until it is cleared, other programs' reads of this drive \
         (RESTORE.sh, dd) fail with I/O errors, never wrong data. Run any tapectl tape command, \
         which clears it first, or power the drive off and on."
    )
}

#[cfg(test)]
mod tests {
    use super::fake::{FakePage, HP_PAGE};
    use super::*;

    /// The bytewise register, as `lbp.py` and mhvtl compute it: the
    /// reference the slicing kernel is held to.
    fn rs_crc_bytewise(data: &[u8], seed: u32) -> u32 {
        let mut crc = seed;
        for &b in data {
            crc = (crc << 8) ^ T0[((crc >> 24) as u8 ^ b) as usize];
        }
        crc
    }

    /// A small xorshift: random-looking bytes with no dependency.
    fn noise(len: usize, seed: u64) -> Vec<u8> {
        let mut x = seed | 1;
        (0..len)
            .map(|_| {
                x ^= x << 13;
                x ^= x >> 7;
                x ^= x << 17;
                (x >> 24) as u8
            })
            .collect()
    }

    const B1: [u8; 37] = [
        2, 3, 5, 7, 11, 13, 17, 19, 23, 29, 31, 37, 41, 43, 47, 53, 59, 61, 67, 71, 73, 79, 83, 89,
        97, 101, 103, 107, 109, 113, 127, 131, 137, 139, 149, 151, 157,
    ];
    const B2: [u8; 17] = [
        163, 167, 173, 179, 181, 191, 193, 197, 199, 211, 223, 227, 229, 233, 239, 241, 251,
    ];

    /// mhvtl's `usr/utils/validate_crc.c` known answers, which `lbp.py
    /// selftest` reproduced against the HP drive's accepted blocks.
    #[test]
    fn rs_crc_reproduces_mhvtls_vectors() {
        let c1 = rs_crc(&B1, 0);
        assert_eq!(c1, 0x733D_4DCA);
        assert_eq!(rs_crc(&B2, c1), 0x754E_D37E);
        let both: Vec<u8> = B1.iter().chain(B2.iter()).copied().collect();
        assert_eq!(rs_crc(&both, 0), 0x754E_D37E);
    }

    #[test]
    fn slicing_matches_bytewise() {
        for (i, len) in [0usize, 1, 3, 7, 15, 16, 17, 31, 32, 1000, 524_287]
            .into_iter()
            .enumerate()
        {
            let data = noise(len, 0x9E37_79B9 + i as u64);
            for seed in [0u32, 0x733D_4DCA, 0xFFFF_FFFF] {
                assert_eq!(
                    rs_crc(&data, seed),
                    rs_crc_bytewise(&data, seed),
                    "len {len}, seed {seed:#x}"
                );
            }
        }
        for seed in [1u64, 2] {
            let block = noise(524_288, seed);
            assert_eq!(rs_crc(&block, 0), rs_crc_bytewise(&block, 0));
        }
    }

    /// The property slicing relies on: the code is GF(2)-linear.
    #[test]
    fn the_table_is_linear() {
        for a in 0..256usize {
            for b in 0..256usize {
                assert_eq!(T0[a ^ b], T0[a] ^ T0[b], "{a} {b}");
            }
        }
    }

    /// Every entry of the copied table, derived from the field instead:
    /// `T0[a]` is `a` times the generator's coefficients (38h, CFh, 38h,
    /// 01h) in GF(2^8) over x^8+x^4+x^3+x^2+1.
    #[test]
    fn the_table_is_the_gf256_code() {
        fn mul(mut a: u32, mut b: u32) -> u32 {
            let mut r = 0;
            while b != 0 {
                if b & 1 != 0 {
                    r ^= a;
                }
                a <<= 1;
                if a & 0x100 != 0 {
                    a ^= 0x11D;
                }
                b >>= 1;
            }
            r
        }
        for (a, &t) in T0.iter().enumerate() {
            let a = a as u32;
            assert_eq!(
                t,
                mul(a, 0x38) << 24 | mul(a, 0xCF) << 16 | mul(a, 0x38) << 8 | a,
                "T0[{a:#04x}]"
            );
        }
    }

    /// Risk R2, pinned: seed 0 and linearity make an all-zero block with an
    /// all-zero CRC valid, so a fault that zeroes a whole wire buffer
    /// passes LBP. The readback catches it; the threat model says so.
    #[test]
    fn a_zero_block_has_a_zero_crc() {
        assert_eq!(rs_crc(&[0u8; 524_288], 0), 0);
        assert!(check(&[0u8; 4096 + CRC_LEN]).is_ok());
    }

    #[test]
    fn protect_appends_big_endian() {
        let mut block = B1.to_vec();
        block.extend_from_slice(&[0xAA; CRC_LEN]);
        protect(&mut block);
        assert_eq!(&block[..B1.len()], &B1, "the payload is untouched");
        assert_eq!(&block[B1.len()..], &[0x73, 0x3D, 0x4D, 0xCA]);
        assert!(check(&block).is_ok());
    }

    #[test]
    fn check_names_both_crcs() {
        let mut block = B1.to_vec();
        block.extend_from_slice(&[0; CRC_LEN]);
        protect(&mut block);
        let last = block.len() - 1;
        block[last] ^= 0x01;
        assert_eq!(
            check(&block),
            Err(CrcMismatch {
                got: 0x733D_4DCB,
                want: 0x733D_4DCA
            })
        );
        // A flipped payload bit: the CRC carried is the old one.
        let mut block2 = B1.to_vec();
        block2.extend_from_slice(&[0; CRC_LEN]);
        protect(&mut block2);
        block2[0] ^= 0x80;
        let e = check(&block2).unwrap_err();
        assert_eq!(e.got, 0x733D_4DCA);
        assert_ne!(e.want, e.got);
    }

    // ── the page ──

    fn mode_sense10(page: &[u8], descriptor: bool) -> Vec<u8> {
        let mut r = vec![0x00, 0x2c, 0x68, 0x10, 0x00, 0x00, 0x00, 0x00];
        if descriptor {
            r[7] = 8;
            r.extend_from_slice(&[0x5a, 0, 0, 0, 0, 0x08, 0, 0]);
        }
        r.extend_from_slice(page);
        r
    }

    /// mhvtl's TD6 page (MEASURED on A3, 2026-10-25): 32 bytes, SPF clear.
    fn mhvtl_page() -> Vec<u8> {
        let mut p = vec![0x0a, 0xf0, 0x00, 0x1c, 0x00, 0x04, 0x00, 0x00];
        p.resize(32, 0);
        p
    }

    #[test]
    fn the_page_is_found_after_the_header_and_its_descriptors() {
        let hp = ProtectionPage::from_mode_sense10(&mode_sense10(&HP_PAGE, false)).unwrap();
        assert_eq!(hp.bytes(), &HP_PAGE);
        assert_eq!((hp.method(), hp.info_len(), hp.flags()), (0, 0, 0));
        assert!(!hp.is_armed());

        let mhvtl = ProtectionPage::from_mode_sense10(&mode_sense10(&mhvtl_page(), true)).unwrap();
        assert_eq!(mhvtl.bytes().len(), 32);
        assert_eq!((mhvtl.method(), mhvtl.info_len()), (0, 4));
        // A response cut short of the page's declared length keeps what came.
        let short = mode_sense10(&mhvtl_page()[..30], true);
        assert_eq!(
            ProtectionPage::from_mode_sense10(&short)
                .unwrap()
                .bytes()
                .len(),
            30
        );
        // Another page is refused, and so is one too short to hold the flags.
        let mut other = HP_PAGE;
        other[0] = 0x4f;
        assert!(
            ProtectionPage::from_mode_sense10(&mode_sense10(&other, false))
                .unwrap_err()
                .contains("0Fh/F0h")
        );
        assert!(ProtectionPage::from_mode_sense10(&mode_sense10(&HP_PAGE[..6], false)).is_err());
        assert!(ProtectionPage::from_mode_sense10(&[0; 4]).is_err());
    }

    #[test]
    fn arming_sets_the_method_and_both_checks_clears_ps_and_keeps_spf() {
        let mut saved = HP_PAGE;
        saved[0] |= 0x80; // PS: the drive could save it
        let page = ProtectionPage::from_page_bytes(&saved).unwrap();
        let armed = page.with_protection(METHOD_RS, RS_INFO_LEN, LBP_W | LBP_R);
        assert_eq!(
            armed.bytes(),
            &[0x4a, 0xf0, 0x00, 0x04, 0x01, 0x04, 0xc0, 0x00]
        );
        assert!(armed.armed_as(LBP_W | LBP_R) && armed.is_armed());
        assert!(!armed.armed_as(LBP_W));
        assert_eq!(armed.cleared().bytes(), &HP_PAGE);
        assert_eq!(armed.contents_arg(), "4a,f0,00,04,01,04,c0,00");
        // Byte 6's other bits (RBDP) are kept.
        let mut rbdp = HP_PAGE;
        rbdp[6] = 0x20;
        let p = ProtectionPage::from_page_bytes(&rbdp).unwrap();
        assert_eq!(p.with_protection(METHOD_RS, 4, LBP_W).bytes()[6], 0xa0);
    }

    #[test]
    fn the_argv_are_pinned_and_another_page_is_refused() {
        assert_eq!(
            sense_argv("/dev/sg9"),
            [
                "sg_modes",
                "--page=0x0a,0xf0",
                "--control=0",
                "--raw",
                "/dev/sg9"
            ]
        );
        let page = ProtectionPage::from_page_bytes(&HP_PAGE).unwrap();
        assert_eq!(
            select_argv(
                "/dev/sg9",
                &page.with_protection(METHOD_RS, 4, LBP_W | LBP_R)
            )
            .unwrap(),
            [
                "sg_wr_mode",
                "--page=0x0a,0xf0",
                "--dbd",
                "--contents=4a,f0,00,04,01,04,c0,00",
                "/dev/sg9"
            ]
        );
        let other = ProtectionPage {
            bytes: vec![0x4f, 0xf0, 0x00, 0x04, 0, 0, 0, 0],
        };
        assert!(select_argv("/dev/sg9", &other)
            .unwrap_err()
            .contains("0Fh/F0h"));
        let saved = ProtectionPage {
            bytes: vec![0xca, 0xf0, 0x00, 0x04, 0, 0, 0, 0],
        };
        assert!(select_argv("/dev/sg9", &saved).unwrap()[3].starts_with("--contents=4a,"));
    }

    // ── arm, against a recorded page ──

    fn ok_block(_: usize) -> Result<(), String> {
        Ok(())
    }

    /// A probe that read one wire block of data.
    fn data_probe(wire: usize) -> io::Result<usize> {
        Ok(wire)
    }

    #[test]
    fn arming_the_hp_page_reads_it_back_and_probes_the_wire_size() {
        let fake = FakePage::new(&HP_PAGE);
        let mut sizes = Vec::new();
        let mut probed = Vec::new();
        let armed = arm(
            &fake,
            LBP_W | LBP_R,
            524_288,
            &mut |n| {
                sizes.push(n);
                Ok(())
            },
            &mut |n| {
                probed.push(n);
                Ok(n)
            },
        )
        .unwrap();
        assert_eq!(fake.current(), [0x4a, 0xf0, 0, 4, 1, 4, 0xc0, 0]);
        assert_eq!((sizes, probed), (vec![524_292], vec![524_292]));
        assert!(!armed.leftover);
        assert_eq!(armed.restore_page().bytes(), &HP_PAGE);
        assert_eq!(
            fake.ops(),
            ["sense", "select 4a,f0,00,04,01,04,c0,00", "sense"]
        );

        disarm(&fake, &armed).unwrap();
        assert_eq!(fake.current(), HP_PAGE);
    }

    #[test]
    fn arming_the_32_byte_mhvtl_page_keeps_its_length() {
        let fake = FakePage::new(&mhvtl_page());
        let armed = arm(
            &fake,
            LBP_W | LBP_R,
            262_144,
            &mut ok_block,
            &mut data_probe,
        )
        .unwrap();
        let now = fake.current();
        assert_eq!(now.len(), 32);
        assert_eq!(&now[..8], &[0x0a, 0xf0, 0x00, 0x1c, 0x01, 0x04, 0xc0, 0x00]);
        disarm(&fake, &armed).unwrap();
        assert_eq!(fake.current(), mhvtl_page());
    }

    /// Every fallback cause of §5.1 that `arm` decides, by value; each
    /// leaves the page as found and the block size at the payload.
    #[test]
    fn every_refusal_is_named_and_restores_the_page_and_the_block_size() {
        let payload = 524_288;
        let run = |fake: &FakePage,
                   probe: &mut dyn FnMut(usize) -> io::Result<usize>,
                   block_fails: bool| {
            let mut sizes = Vec::new();
            let r = arm(
                fake,
                LBP_W | LBP_R,
                payload,
                &mut |n| {
                    sizes.push(n);
                    if block_fails && n != payload {
                        Err("Invalid argument (os error 22)".to_string())
                    } else {
                        Ok(())
                    }
                },
                probe,
            );
            (r.unwrap_err(), sizes)
        };

        // Cause 2: sg3-utils missing.
        let fake = FakePage::new(&HP_PAGE);
        fake.fail_sense(PageError::Spawn(
            "sg_modes could not be run: No such file or directory (os error 2)".into(),
        ));
        let (e, sizes) = run(&fake, &mut data_probe, false);
        assert_eq!(
            e.reason,
            "sg_modes could not be run: No such file or directory (os error 2)"
        );
        assert!(sizes.is_empty() && fake.selects() == 0, "nothing set");

        // Cause 3: no page.
        let fake = FakePage::new(&HP_PAGE);
        fake.fail_sense(PageError::IllegalRequest(
            "sg_modes exited 5: bad page".into(),
        ));
        let (e, _) = run(&fake, &mut data_probe, false);
        assert_eq!(
            e.reason,
            "the drive has no Control Data Protection page (0Ah/F0h): sg_modes exited 5: bad page"
        );

        // Cause 4: no write access to the sg node.
        let fake = FakePage::new(&HP_PAGE);
        fake.fail_select(PageError::Open(
            "sg_wr_mode exited 15: Permission denied".into(),
        ));
        let (e, sizes) = run(&fake, &mut data_probe, false);
        assert_eq!(
            e.reason,
            "sg_wr_mode could not open /dev/sg-fake for writing: sg_wr_mode exited 15: \
             Permission denied"
        );
        assert_eq!(fake.current(), HP_PAGE);
        assert!(sizes.is_empty());

        // Cause 5: Reed-Solomon refused.
        let fake = FakePage::new(&HP_PAGE);
        fake.fail_select(PageError::IllegalRequest(
            "sg_wr_mode exited 5: invalid field".into(),
        ));
        let (e, _) = run(&fake, &mut data_probe, false);
        assert_eq!(
            e.reason,
            "the drive refused Reed-Solomon LBP (method 01h): sg_wr_mode exited 5: invalid field"
        );
        assert_eq!(fake.current(), HP_PAGE);

        // Cause 6: the select "succeeded" and the page reads back otherwise.
        let fake = FakePage::new(&HP_PAGE);
        fake.ignore_selects_of_flags();
        let (e, _) = run(&fake, &mut data_probe, false);
        assert_eq!(
            e.reason,
            "the drive's page did not take: it reads back method 01h, information length 4, \
             flags 00h"
        );
        assert_eq!(fake.current(), HP_PAGE, "put back as found");

        // Cause 7: st refuses the wire size.
        let fake = FakePage::new(&HP_PAGE);
        let (e, sizes) = run(&fake, &mut data_probe, true);
        assert_eq!(
            e.reason,
            "st refused a 524292-byte block size: Invalid argument (os error 22)"
        );
        assert_eq!(sizes, [524_292, 524_288], "set back to the payload");
        assert_eq!(fake.current(), HP_PAGE);

        // Cause 8: st cannot carry it (mhvtl's EBUSY; EOVERFLOW; ENOMEM).
        for errno in [nix::libc::EBUSY, nix::libc::EOVERFLOW, nix::libc::ENOMEM] {
            let fake = FakePage::new(&HP_PAGE);
            let (e, sizes) = run(
                &fake,
                &mut |_| Err(io::Error::from_raw_os_error(errno)),
                false,
            );
            assert_eq!(
                e.reason,
                format!(
                    "st cannot carry a 524292-byte block on this host's SCSI path ({})",
                    io::Error::from_raw_os_error(errno)
                )
            );
            assert_eq!(sizes, [524_292, 524_288]);
            assert_eq!(fake.current(), HP_PAGE);
            assert!(!e.leftover);
        }
    }

    /// The probe's other answers all mean the request was issued: data, a
    /// filemark (0), a blank check or a block LBP_R cannot return (EIO),
    /// end of medium (ENOSPC).
    #[test]
    fn a_probe_that_was_issued_arms_whatever_it_read() {
        let answers: [fn(usize) -> io::Result<usize>; 4] = [
            |n| Ok(n),
            |_| Ok(0),
            |_| Err(io::Error::from_raw_os_error(nix::libc::EIO)),
            |_| Err(io::Error::from_raw_os_error(nix::libc::ENOSPC)),
        ];
        for mut answer in answers {
            let fake = FakePage::new(&HP_PAGE);
            assert!(arm(&fake, LBP_W | LBP_R, 524_288, &mut ok_block, &mut answer).is_ok());
            assert!(ProtectionPage::from_page_bytes(&fake.current())
                .unwrap()
                .armed_as(LBP_W | LBP_R));
        }
    }

    /// §4.4: a page found armed is a leftover. It is reported, re-armed for
    /// this open, and cleared (not restored) at close; a failed arm clears it.
    #[test]
    fn a_leftover_is_reported_rearmed_and_cleared_at_close() {
        let left = [0x4a, 0xf0, 0x00, 0x04, 0x01, 0x04, 0xc0, 0x00];
        let fake = FakePage::new(&left);
        let armed = arm(
            &fake,
            LBP_W | LBP_R,
            524_288,
            &mut ok_block,
            &mut data_probe,
        )
        .unwrap();
        assert!(armed.leftover);
        disarm(&fake, &armed).unwrap();
        assert_eq!(fake.current(), HP_PAGE, "cleared, not put back armed");

        let fake = FakePage::new(&left);
        let e = arm(&fake, LBP_W | LBP_R, 524_288, &mut ok_block, &mut |_| {
            Err(io::Error::from_raw_os_error(nix::libc::EBUSY))
        })
        .unwrap_err();
        assert!(e.leftover);
        assert_eq!(
            fake.current(),
            HP_PAGE,
            "a failed arm clears a leftover too"
        );
    }

    #[test]
    fn disarm_checks_the_page_reads_back_clear() {
        let fake = FakePage::new(&HP_PAGE);
        let armed = arm(
            &fake,
            LBP_W | LBP_R,
            524_288,
            &mut ok_block,
            &mut data_probe,
        )
        .unwrap();
        fake.ignore_selects_of_flags();
        assert_eq!(
            disarm(&fake, &armed).unwrap_err(),
            "the drive's page did not take: it reads back method 00h, information length 0, \
             flags C0h"
        );
        let fake = FakePage::new(&HP_PAGE);
        let armed = arm(
            &fake,
            LBP_W | LBP_R,
            524_288,
            &mut ok_block,
            &mut data_probe,
        )
        .unwrap();
        fake.fail_select(PageError::Other("sg_wr_mode exited 99: transport".into()));
        assert!(disarm(&fake, &armed)
            .unwrap_err()
            .contains("could not be set: sg_wr_mode exited 99"));
    }

    #[test]
    fn an_open_that_does_not_arm_clears_only_a_leftover() {
        let fake = FakePage::new(&HP_PAGE);
        assert!(!clear_leftover(&fake).unwrap());
        assert_eq!(fake.selects(), 0, "a clean page is left alone");
        let fake = FakePage::new(&[0x4a, 0xf0, 0x00, 0x04, 0x01, 0x04, 0x80, 0x00]);
        assert!(clear_leftover(&fake).unwrap());
        assert_eq!(fake.current(), HP_PAGE);
    }

    /// Item 175's switch, both settings: what an open arms.
    #[test]
    fn the_read_policy_decides_what_an_open_arms() {
        use LbpReadPolicy::*;
        assert_eq!(AllReads.open_bits(true), Some(LBP_W | LBP_R));
        assert_eq!(AllReads.open_bits(false), Some(LBP_W | LBP_R));
        assert_eq!(OnlyConfirmOfLbpWrite.open_bits(true), Some(LBP_W));
        assert_eq!(OnlyConfirmOfLbpWrite.open_bits(false), None);
        assert_eq!(LBP_R_POLICY, AllReads, "item 162's default ships");
    }

    /// Item 175: LBP_R added to a write-armed page for the confirm.
    #[test]
    fn rearming_adds_the_read_check_and_puts_back_a_page_that_did_not_take() {
        let fake = FakePage::new(&HP_PAGE);
        let mut armed = arm(&fake, LBP_W, 524_288, &mut ok_block, &mut data_probe).unwrap();
        assert_eq!(fake.current()[6], 0x80);
        rearm(&fake, &mut armed, LBP_W | LBP_R).unwrap();
        assert_eq!((fake.current()[6], armed.bits), (0xc0, LBP_W | LBP_R));
        disarm(&fake, &armed).unwrap();
        assert_eq!(fake.current(), HP_PAGE);

        let fake = FakePage::new(&HP_PAGE);
        let mut armed = arm(&fake, LBP_W, 524_288, &mut ok_block, &mut data_probe).unwrap();
        fake.ignore_selects_of_flags();
        assert!(rearm(&fake, &mut armed, LBP_W | LBP_R).is_err());
        assert_eq!((fake.current()[6], armed.bits), (0x80, LBP_W));
    }

    #[test]
    fn the_unavailable_warning_is_once_per_node() {
        assert!(first_warning("/dev/nst-lbp-test-a", true));
        assert!(!first_warning("/dev/nst-lbp-test-a", true));
        assert!(first_warning("/dev/nst-lbp-test-b", true));
        assert_eq!(
            unavailable_warning("/dev/nst0", "st refused a 524292-byte block size: x"),
            "warning: Logical Block Protection is not available on /dev/nst0: st refused a \
             524292-byte block size: x. tapectl writes without it; the write is as strong as one \
             by tapectl 1.7.1 (its sha256 checks are unchanged), and the catalog records that \
             LBP did not run."
        );
        assert_eq!(
            reason_no_sg_node("/dev/nst0"),
            "no SCSI generic node is bound to /dev/nst0 (is the sg module loaded?)"
        );
    }

    /// The coordinator's 388b addition 4: a write open's warning goes to
    /// stderr once — `progress::stderr_println`, never also a
    /// `tracing::warn!`, whose stderr layer printed it a second time — and
    /// to the session log.
    #[test]
    fn the_unavailable_warning_is_printed_once_on_stderr() {
        let src = include_str!("lbp.rs");
        let start = src.find("pub fn note_unavailable(").unwrap();
        let end = start + src[start..].find("\n}\n").unwrap();
        let body = &src[start..end];
        assert_eq!(body.matches("stderr_println(").count(), 1, "{body}");
        assert!(!body.contains("tracing::warn!"), "{body}");
        assert!(body.contains("progress::log(&line)"), "{body}");
    }

    /// The coordinator's 388b addition 3: a read open's note (log only, F4)
    /// does not use up the write open's item-165 warning for the same node,
    /// as `volume rewrite` opens it: read first, then write. Each direction
    /// is still told once.
    #[test]
    fn a_read_open_never_swallows_the_write_opens_warning() {
        let node = "/dev/nst-lbp-test-read-then-write";
        assert!(first_warning(node, false), "the read's note");
        assert!(first_warning(node, true), "the write's warning, still due");
        assert!(!first_warning(node, true), "once per write");
        assert!(!first_warning(node, false), "once per read");
    }

    #[test]
    fn no_sg_node_for_a_path_that_is_not_a_tape_node() {
        assert!(control_for("/nonexistent/tapectl-lbp-test/nst0").is_none());
    }
}

/// A recorded page for tests (here and in `tape::ioctl`): MODE SENSE
/// answers the current bytes, MODE SELECT replaces them, every call is
/// logged, and a test can make either fail or make a select not take.
#[cfg(test)]
pub(crate) mod fake {
    use std::sync::{Arc, Mutex};

    use super::{PageControl, PageError, ProtectionPage};

    /// The HP LTO-6's page as `sg_modes --raw` gave it (2026-10-20): 8
    /// bytes, SPF set, PS clear, protection off.
    pub(crate) const HP_PAGE: [u8; 8] = [0x4a, 0xf0, 0x00, 0x04, 0x00, 0x00, 0x00, 0x00];

    #[derive(Default)]
    struct State {
        page: Vec<u8>,
        ops: Vec<String>,
        sense_fails: Option<PageError>,
        select_fails: Option<PageError>,
        /// A select that "succeeds" and changes only the method and length:
        /// HP applies parameters up to the first error.
        ignore_flags: bool,
        selects: usize,
    }

    #[derive(Clone, Default)]
    pub(crate) struct FakePage {
        state: Arc<Mutex<State>>,
        /// Shared with a device's own op log, when a test wants one order.
        pub(crate) log: Option<Arc<Mutex<Vec<String>>>>,
    }

    impl FakePage {
        pub(crate) fn new(page: &[u8]) -> Self {
            Self {
                state: Arc::new(Mutex::new(State {
                    page: page.to_vec(),
                    ..State::default()
                })),
                log: None,
            }
        }

        pub(crate) fn current(&self) -> Vec<u8> {
            self.state.lock().unwrap().page.clone()
        }

        pub(crate) fn ops(&self) -> Vec<String> {
            self.state.lock().unwrap().ops.clone()
        }

        pub(crate) fn selects(&self) -> usize {
            self.state.lock().unwrap().selects
        }

        pub(crate) fn fail_sense(&self, e: PageError) {
            self.state.lock().unwrap().sense_fails = Some(e);
        }

        pub(crate) fn fail_select(&self, e: PageError) {
            self.state.lock().unwrap().select_fails = Some(e);
        }

        pub(crate) fn ignore_selects_of_flags(&self) {
            self.state.lock().unwrap().ignore_flags = true;
        }

        fn record(&self, s: &mut State, op: String) {
            if let Some(log) = &self.log {
                log.lock().unwrap().push(op.clone());
            }
            s.ops.push(op);
        }
    }

    impl PageControl for FakePage {
        fn node(&self) -> &str {
            "/dev/sg-fake"
        }

        fn sense(&self) -> Result<ProtectionPage, PageError> {
            let mut s = self.state.lock().unwrap();
            self.record(&mut s, "sense".to_string());
            if let Some(e) = s.sense_fails.clone() {
                return Err(e);
            }
            ProtectionPage::from_page_bytes(&s.page).map_err(PageError::Other)
        }

        fn select(&self, page: &ProtectionPage) -> Result<(), PageError> {
            let mut s = self.state.lock().unwrap();
            self.record(&mut s, format!("select {}", page.contents_arg()));
            if let Some(e) = s.select_fails.clone() {
                return Err(e);
            }
            s.selects += 1;
            if s.ignore_flags {
                let flags = s.page[6];
                s.page = page.bytes().to_vec();
                s.page[6] = flags;
            } else {
                s.page = page.bytes().to_vec();
            }
            Ok(())
        }
    }
}
