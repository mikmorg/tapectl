//! The storage interface (ADR-0006). A `Store` executes a Layout's entries at
//! contact. `TapeStore` is the first implementation (LTO via the kernel st
//! driver); `WarehouseStore`/`ExportStore` are peers landing later (#72/#73).
//!
//! The trait is deliberately medium-agnostic — the anti-tape-ism test is that a
//! warehouse upload must fit `execute` without violence. #71 carves the write
//! seam (`execute`); `read`, `confirm → Evidence`, and the EOT `MediumEvent`
//! outcome grow onto this trait with the WriteSession (#22), confirm/seal
//! (#23), and EOT-transition (#26) work.

use crate::error::Result;
use crate::tape::ioctl::TapeDevice;

/// A medium that executes writes at contact.
pub trait Store {
    /// Write one file's bytes followed by a filemark, returning the number of
    /// bytes committed to the medium (including any block padding). `sync`
    /// requests a synchronous filemark — a durability barrier the writer uses
    /// after the operator envelopes. A full medium (EOT) errors today; #26
    /// turns that into a Layout-transition outcome the session decides on.
    fn execute(&mut self, bytes: &[u8], sync: bool) -> Result<usize>;
}

/// LTO tape via the kernel st driver (fixed 512KB blocks).
pub struct TapeStore {
    dev: TapeDevice,
}

impl TapeStore {
    /// Open the drive and rewind to BOT, ready to write File 0. Hardware
    /// compression is disabled best-effort (encrypted data is incompressible;
    /// §2.8) — a drive that rejects the op is only logged, not failed.
    pub fn open(device: &str, block_size: usize) -> Result<Self> {
        let dev = TapeDevice::open(device, block_size)?;
        dev.rewind()?;
        if let Err(e) = dev.disable_compression() {
            tracing::warn!(err = %e, "could not disable hardware compression (continuing)");
        }
        Ok(Self { dev })
    }
}

impl Store for TapeStore {
    fn execute(&mut self, bytes: &[u8], sync: bool) -> Result<usize> {
        if sync {
            self.dev.write_file_with_sync_mark(bytes)
        } else {
            self.dev.write_file_with_mark(bytes)
        }
    }
}

/// An in-memory store: proves the interface is medium-agnostic (the "second
/// store implementable without touching Layout code" acceptance) and lets the
/// WriteSession be unit-tested without a tape.
#[derive(Default)]
pub struct MemStore {
    /// Every file's bytes, in write order.
    pub files: Vec<Vec<u8>>,
    /// Whether each corresponding file used a synchronous filemark.
    pub syncs: Vec<bool>,
}

impl Store for MemStore {
    fn execute(&mut self, bytes: &[u8], sync: bool) -> Result<usize> {
        self.files.push(bytes.to_vec());
        self.syncs.push(sync);
        Ok(bytes.len())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn memstore_records_entries_in_order() {
        let mut s = MemStore::default();
        assert_eq!(s.execute(b"id-thunk", false).unwrap(), 8);
        assert_eq!(s.execute(b"slice", false).unwrap(), 5);
        assert_eq!(s.execute(b"op-envelope", true).unwrap(), 11);
        assert_eq!(s.files.len(), 3);
        assert_eq!(s.files[0], b"id-thunk");
        assert_eq!(s.syncs, vec![false, false, true]);
    }
}
