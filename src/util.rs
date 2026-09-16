//! Small shared utilities that don't belong to any one subsystem.

use std::io::{self, Read, Write};

use sha2::{Digest, Sha256};

/// A `Read` adapter that computes the sha256 of every byte read through it.
///
/// Lets one streaming pass serve two purposes — e.g. feeding a tape write
/// while hashing the same bytes for the tri-layer integrity model's *execute*
/// layer (`docs/design/v2-open-questions.md` §2.4/§9: "execute re-hashes
/// inline on the same streaming read that feeds the tape") — instead of a
/// second read solely to hash. Wrap any `Read` source; drive it to
/// completion through the normal `Read` interface, then call
/// `finalize_hex()`.
pub struct HashingReader<R> {
    inner: R,
    hasher: Sha256,
}

impl<R: Read> HashingReader<R> {
    /// Wrap `inner`, starting from a fresh hash state.
    pub fn new(inner: R) -> Self {
        Self {
            inner,
            hasher: Sha256::new(),
        }
    }

    /// The hex sha256 of every byte read through this adapter so far.
    ///
    /// Non-consuming (clones the internal hasher state), so it is safe to
    /// call after driving the reader to EOF without giving up ownership —
    /// the typical call site streams to completion, then reads this once.
    pub fn finalize_hex(&self) -> String {
        format!("{:x}", self.hasher.clone().finalize())
    }
}

impl<R: Read> Read for HashingReader<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let n = self.inner.read(buf)?;
        if n > 0 {
            self.hasher.update(&buf[..n]);
        }
        Ok(n)
    }
}

/// A `Write` adapter that computes the sha256 of every byte written through
/// it, and counts them. The writer-side counterpart to `HashingReader` —
/// added for the staging H9 fix (issue #35): wrap an output file with this
/// so the ciphertext hash and size are known from the same streaming pass
/// that writes it (e.g. sitting between `age::Encryptor::wrap_output` and
/// the on-disk `.age` file), never a second read-back of the whole file.
/// Wrap any `Write` sink; drive it to completion through the normal `Write`
/// interface (including whatever finalizes the wrapped format, e.g.
/// `age::stream::StreamWriter::finish`, which hands the wrapped writer back
/// out), then call `finalize_hex()` / `bytes_written()`.
pub struct HashingWriter<W> {
    inner: W,
    hasher: Sha256,
    bytes_written: u64,
}

impl<W: Write> HashingWriter<W> {
    /// Wrap `inner`, starting from a fresh hash state and a zero byte count.
    pub fn new(inner: W) -> Self {
        Self {
            inner,
            hasher: Sha256::new(),
            bytes_written: 0,
        }
    }

    /// The hex sha256 of every byte written through this adapter so far.
    ///
    /// Non-consuming (clones the internal hasher state), matching
    /// `HashingReader::finalize_hex` — safe to call after the wrapped
    /// writer has been driven to completion without giving up ownership.
    pub fn finalize_hex(&self) -> String {
        format!("{:x}", self.hasher.clone().finalize())
    }

    /// Total bytes written through this adapter so far.
    pub fn bytes_written(&self) -> u64 {
        self.bytes_written
    }
}

impl<W: Write> Write for HashingWriter<W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let n = self.inner.write(buf)?;
        if n > 0 {
            // Hash/count exactly the `n` bytes the inner writer actually
            // accepted, not the whole `buf` — mirrors `HashingReader`'s
            // same care for a short read, here for a short write.
            self.hasher.update(&buf[..n]);
            self.bytes_written += n as u64;
        }
        Ok(n)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

/// A `Write` adapter that forwards at most the first `limit` bytes it
/// receives to `inner`, silently discarding everything after — trims a
/// stream's trailing bytes (e.g. a tape file's block padding) to a known
/// true length as the bytes arrive, without needing to know in advance
/// which single `write()` call the true/padding boundary falls inside (a
/// real tape read arrives in many `block_size`-sized pushes via
/// `Store::read_file`, not one whole-buffer write).
///
/// Originally added in `volume/restore.rs` for the ciphertext trim (issue
/// #85: mirrors the `&enc_data[..encrypted_bytes]` trim `restore_unit` used
/// to do after the fact, once the whole padded slice sat in a `Vec`);
/// lifted here so `store.rs`'s `chain_walk` content-file hashing and
/// `volume/write.rs`'s `read_slices`/`compact_read` slice staging (issue
/// #86) can share this identical, already-proven boundary logic instead of
/// each growing a second implementation.
pub struct TruncatingWriter<W> {
    inner: W,
    remaining: u64,
}

impl<W: Write> TruncatingWriter<W> {
    /// Wrap `inner`, forwarding at most `limit` bytes to it and discarding
    /// the rest of whatever is written through this adapter.
    pub fn new(inner: W, limit: u64) -> Self {
        Self {
            inner,
            remaining: limit,
        }
    }

    /// Reclaim the wrapped writer once streaming is done.
    pub fn into_inner(self) -> W {
        self.inner
    }
}

impl<W: Write> Write for TruncatingWriter<W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let take = (buf.len() as u64).min(self.remaining) as usize;
        if take > 0 {
            self.inner.write_all(&buf[..take])?;
            self.remaining -= take as u64;
        }
        // Always claim the WHOLE input as "written", even the silently
        // discarded tail — never `Ok(take)`. `Write::write_all`'s default
        // implementation treats a `write()` that returns `Ok(0)` for a
        // non-empty buffer as `ErrorKind::WriteZero`, which is exactly what
        // an "honest" `Ok(take)` triggers on every push once `remaining`
        // hits zero (proven by the tests below failing against that
        // version) — issue #85.
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

// --- Byte-count humanisers (issue #204) ------------------------------------
//
// ADR-0012: "Cartridge capacities are decimal; data sizes are binary; the two
// are named apart." Before this, `cli::catalog::format_size` and
// `cli::collection::format_bytes` were two separately-maintained humanisers
// (one binary, one decimal, both labeled ad hoc), and several other call
// sites divided a decimal-stored `capacity_bytes`/`nominal_capacity` by a
// binary 1024^n and called the result "GB" — not a mislabeling, a wrong
// number (a 2.5 TB LTO-6 cartridge reads back as "2328 GB", `cartridge.rs`
// pre-existing bug; `cli::volume::print_volume_info` and
// `cli::report::report_capacity` had the identical bug, fixed alongside
// this). One pair of functions, not N call sites: every DATA size goes
// through `format_bytes_binary`, every CAPACITY goes through
// `format_bytes_decimal`, and nothing else in the crate re-derives either.

/// Human-readable byte count in BINARY units (KiB/MiB/GiB/TiB, 1024-based).
///
/// For **data sizes**: slice bytes, staged/encrypted bytes, bytes read or
/// written, reclaimable/freed bytes — anything `dar` or the block layer
/// actually measured. ADR-0012: "data sizes are binary, as dar and the
/// block layer count."
///
/// Never call this on a stored capacity (`cartridges.nominal_capacity`,
/// `volumes.capacity_bytes`, `mam_capacity_bytes`) — those are decimal by
/// the same ruling; use [`format_bytes_decimal`]. Dividing a decimal-stored
/// capacity by a binary power understates it by up to ~7% at GB scale
/// (issue #204).
pub fn format_bytes_binary(bytes: i64) -> String {
    const KI: f64 = 1024.0;
    let b = bytes as f64;
    if bytes >= (KI * KI * KI * KI) as i64 {
        format!("{:.2} TiB", b / (KI * KI * KI * KI))
    } else if bytes >= (KI * KI * KI) as i64 {
        format!("{:.1} GiB", b / (KI * KI * KI))
    } else if bytes >= (KI * KI) as i64 {
        format!("{:.1} MiB", b / (KI * KI))
    } else if bytes >= KI as i64 {
        format!("{:.1} KiB", b / KI)
    } else {
        format!("{bytes} B")
    }
}

/// Human-readable byte count in DECIMAL units (KB/MB/GB/TB, 1000-based).
///
/// For **capacities**: `cartridges.nominal_capacity`, `volumes.capacity_bytes`,
/// `mam_capacity_bytes` — decimal by ADR-0012 ruling and stored decimal (the
/// generation table holds LTO-6 as `2_500_000_000_000`). Marketed capacity
/// figures are decimal; rendering one through [`format_bytes_binary`]
/// instead reprints the box's own number wrong, not just under a different
/// label.
///
/// Never call this on a measured data size — `stage create` output, slice
/// bytes, tape reads/writes — those are binary by the same ruling; use
/// [`format_bytes_binary`].
pub fn format_bytes_decimal(bytes: i64) -> String {
    const K: f64 = 1_000.0;
    let b = bytes as f64;
    if bytes >= (K * K * K * K) as i64 {
        format!("{:.2} TB", b / (K * K * K * K))
    } else if bytes >= (K * K * K) as i64 {
        format!("{:.1} GB", b / (K * K * K))
    } else if bytes >= (K * K) as i64 {
        format!("{:.1} MB", b / (K * K))
    } else if bytes >= K as i64 {
        format!("{:.1} KB", b / K)
    } else {
        format!("{bytes} B")
    }
}

/// Render a "written / capacity (pct%)" progress figure the way `volume
/// info` and every `report capacity` view show it. `bytes_written` is a
/// DATA size (binary, [`format_bytes_binary`]); `capacity_bytes` is a
/// CAPACITY (decimal, [`format_bytes_decimal`]) — ADR-0012 requires the two
/// stay "named apart" rather than sharing one unit, so this deliberately
/// prints two different unit families on one line (e.g. "2.1 GiB / 2.5 GB").
/// That is not a bug: it is the whole point of the ruling — an operator who
/// sees matching units here would be seeing a number that was quietly
/// coerced to agree, exactly the failure issue #204 found live in both
/// `volume info` and `report capacity`, which each divided the decimal
/// `capacity_bytes` by a binary 1024^3 and printed the wrong figure as "GB".
///
/// The percentage itself is computed from the raw byte counts, so it is
/// unaffected by which unit either side is displayed in.
pub fn format_capacity_progress(bytes_written: i64, capacity_bytes: i64) -> String {
    let pct = if capacity_bytes > 0 {
        (bytes_written as f64 / capacity_bytes as f64) * 100.0
    } else {
        0.0
    };
    format!(
        "{} / {} ({pct:.1}%)",
        format_bytes_binary(bytes_written),
        format_bytes_decimal(capacity_bytes),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn direct_hash(data: &[u8]) -> String {
        let mut h = Sha256::new();
        h.update(data);
        format!("{:x}", h.finalize())
    }

    #[test]
    fn hashing_reader_matches_direct_hash_on_read_to_end() {
        let data = b"the quick brown fox jumps over the lazy dog, repeated to exceed one buffer, \
                     the quick brown fox jumps over the lazy dog, repeated to exceed one buffer";
        let mut reader = HashingReader::new(Cursor::new(data.to_vec()));
        let mut sink = Vec::new();
        reader.read_to_end(&mut sink).unwrap();

        assert_eq!(sink, data);
        assert_eq!(reader.finalize_hex(), direct_hash(data));
    }

    #[test]
    fn hashing_reader_accumulates_across_small_partial_reads() {
        // Read in chunks smaller than any real buffer to exercise partial
        // reads landing in the hasher exactly once each.
        let data: Vec<u8> = (0..=255u8).collect();
        let mut reader = HashingReader::new(Cursor::new(data.clone()));
        let mut buf = [0u8; 7];
        let mut total = Vec::new();
        loop {
            let n = reader.read(&mut buf).unwrap();
            if n == 0 {
                break;
            }
            total.extend_from_slice(&buf[..n]);
        }
        assert_eq!(total, data);
        assert_eq!(reader.finalize_hex(), direct_hash(&data));
    }

    #[test]
    fn empty_source_hashes_to_the_empty_digest() {
        let mut reader = HashingReader::new(Cursor::new(Vec::<u8>::new()));
        let mut sink = Vec::new();
        reader.read_to_end(&mut sink).unwrap();
        assert_eq!(reader.finalize_hex(), direct_hash(&[]));
    }

    #[test]
    fn finalize_hex_is_stable_when_called_more_than_once() {
        // Non-consuming: calling it twice must not change state or crash.
        let data = b"stable";
        let mut reader = HashingReader::new(Cursor::new(data.to_vec()));
        let mut sink = Vec::new();
        reader.read_to_end(&mut sink).unwrap();
        assert_eq!(reader.finalize_hex(), reader.finalize_hex());
    }

    #[test]
    fn hashing_writer_matches_direct_hash_on_write_all() {
        let data = b"the quick brown fox jumps over the lazy dog, repeated to exceed one buffer, \
                     the quick brown fox jumps over the lazy dog, repeated to exceed one buffer";
        let mut writer = HashingWriter::new(Vec::new());
        writer.write_all(data).unwrap();

        assert_eq!(writer.finalize_hex(), direct_hash(data));
        assert_eq!(writer.bytes_written(), data.len() as u64);
    }

    #[test]
    fn hashing_writer_accumulates_across_small_partial_writes() {
        // Write in chunks smaller than any real buffer to exercise partial
        // writes landing in the hasher exactly once each.
        let data: Vec<u8> = (0..=255u8).collect();
        let mut writer = HashingWriter::new(Vec::new());
        for chunk in data.chunks(7) {
            writer.write_all(chunk).unwrap();
        }
        assert_eq!(writer.finalize_hex(), direct_hash(&data));
        assert_eq!(writer.bytes_written(), data.len() as u64);
    }

    #[test]
    fn empty_sink_hashes_to_the_empty_digest() {
        let writer = HashingWriter::new(Vec::new());
        assert_eq!(writer.finalize_hex(), direct_hash(&[]));
        assert_eq!(writer.bytes_written(), 0);
    }

    #[test]
    fn hashing_writer_finalize_hex_is_stable_when_called_more_than_once() {
        // Non-consuming: calling it twice must not change state or crash.
        let mut writer = HashingWriter::new(Vec::new());
        writer.write_all(b"stable").unwrap();
        assert_eq!(writer.finalize_hex(), writer.finalize_hex());
    }

    #[test]
    fn hashing_writer_passes_bytes_through_to_the_inner_writer_unchanged() {
        // The adapter must be transparent — what comes out the inner
        // writer is exactly what went in, not just a hash side-channel.
        let data = b"payload bytes must reach the inner writer untouched";
        let mut writer = HashingWriter::new(Vec::new());
        writer.write_all(data).unwrap();
        let inner = writer.inner;
        assert_eq!(inner, data);
    }

    // --- TruncatingWriter (moved from volume/restore.rs, issue #86 —
    // shared with store.rs's chain_walk and volume/write.rs's
    // read_slices/compact_read) --------------------------------------------

    #[test]
    fn truncating_writer_passes_bytes_through_up_to_the_limit() {
        let mut w = TruncatingWriter::new(Vec::new(), 5);
        w.write_all(b"hello world").unwrap();
        assert_eq!(w.into_inner(), b"hello");
    }

    #[test]
    fn truncating_writer_forwards_everything_when_limit_exceeds_total_bytes() {
        // Mirrors the original buffered code's fallback branch (declared
        // size >= what's actually there means no trimming happens at all).
        let mut w = TruncatingWriter::new(Vec::new(), 100);
        w.write_all(b"short").unwrap();
        assert_eq!(w.into_inner(), b"short");
    }

    #[test]
    fn truncating_writer_handles_the_boundary_falling_mid_write_across_several_pushes() {
        // Simulates a real tape read arriving in several `block_size`-sized
        // `write_all` pushes (`Store::read_file`/`read_file_streaming`),
        // rather than one whole-buffer write — the limit boundary lands in
        // the MIDDLE of the third push here, not on a push boundary.
        let mut w = TruncatingWriter::new(Vec::new(), 10);
        w.write_all(b"AAAA").unwrap(); // remaining 10 -> 6, all 4 land
        w.write_all(b"BBBB").unwrap(); // remaining 6 -> 2, all 4 land
        w.write_all(b"CCCC").unwrap(); // remaining 2 -> 0, only "CC" lands
        w.write_all(b"DDDD").unwrap(); // remaining stays 0, nothing lands
        assert_eq!(w.into_inner(), b"AAAABBBBCC");
    }

    #[test]
    fn truncating_writer_write_all_never_errors_once_the_limit_is_reached() {
        // The load-bearing behavior (issue #85): `write()` must report the
        // caller's full input length as "written" even when it silently
        // drops bytes past the limit, because `Write::write_all`'s default
        // implementation treats a `write()` that returns `Ok(0)` for a
        // non-empty buffer as `ErrorKind::WriteZero` — exactly what a naive
        // "honest" implementation (report only bytes actually forwarded)
        // triggers on every push once `remaining` hits zero.
        let mut w = TruncatingWriter::new(Vec::new(), 0);
        for _ in 0..5 {
            w.write_all(&[1, 2, 3, 4])
                .expect("write_all must not error once the limit is exhausted");
        }
        assert_eq!(w.into_inner(), Vec::<u8>::new());
    }

    // --- format_bytes_binary / format_bytes_decimal / format_capacity_progress
    // (issue #204) --------------------------------------------------------

    #[test]
    fn binary_formatter_stays_in_bytes_below_one_kib() {
        assert_eq!(format_bytes_binary(0), "0 B");
        assert_eq!(format_bytes_binary(1023), "1023 B");
    }

    #[test]
    fn binary_formatter_boundary_just_under_and_over_one_mib() {
        assert_eq!(format_bytes_binary(1024 * 1024 - 1), "1024.0 KiB");
        assert_eq!(format_bytes_binary(1024 * 1024), "1.0 MiB");
        assert_eq!(format_bytes_binary(1024 * 1024 + 1), "1.0 MiB");
    }

    #[test]
    fn binary_formatter_boundary_just_under_and_over_one_gib() {
        assert_eq!(format_bytes_binary(1024 * 1024 * 1024 - 1), "1024.0 MiB");
        assert_eq!(format_bytes_binary(1024 * 1024 * 1024), "1.0 GiB");
        assert_eq!(format_bytes_binary(1024 * 1024 * 1024 + 1), "1.0 GiB");
    }

    #[test]
    fn binary_formatter_reaches_tib() {
        assert_eq!(
            format_bytes_binary(2 * 1024 * 1024 * 1024 * 1024),
            "2.00 TiB"
        );
    }

    /// The class-2 regression this issue exists to prevent: a cartridge
    /// capacity is stored DECIMAL (the generation table holds LTO-6 as
    /// 2_500_000_000_000). Rendered through the binary formatter it would
    /// read "2328 GiB"-scale (~7% low); through the decimal formatter it
    /// must read back the marketed figure, "2.5 TB" — see
    /// `cli::cartridge::run`'s `CartridgeCommands::Info` arm, whose
    /// pre-existing `cap / (1024*1024*1024)` (line ~406, issue #204) is the
    /// bug this helper is meant to make impossible to repeat.
    #[test]
    fn decimal_formatter_renders_a_stored_lto6_capacity_as_the_marketed_figure() {
        assert_eq!(format_bytes_decimal(2_500_000_000_000), "2.50 TB");
    }

    #[test]
    fn decimal_formatter_boundary_just_under_and_over_one_mb() {
        assert_eq!(format_bytes_decimal(1_000_000 - 1), "1000.0 KB");
        assert_eq!(format_bytes_decimal(1_000_000), "1.0 MB");
        assert_eq!(format_bytes_decimal(1_000_000 + 1), "1.0 MB");
    }

    #[test]
    fn decimal_formatter_boundary_just_under_and_over_one_gb() {
        assert_eq!(format_bytes_decimal(1_000_000_000 - 1), "1000.0 MB");
        assert_eq!(format_bytes_decimal(1_000_000_000), "1.0 GB");
        assert_eq!(format_bytes_decimal(1_000_000_000 + 1), "1.0 GB");
    }

    /// The exact shape `volume info`/`report capacity` print: binary data
    /// size on the left, decimal capacity on the right, deliberately
    /// different unit families on one line (ADR-0012, "named apart").
    #[test]
    fn capacity_progress_names_the_two_sides_apart() {
        // A near-full LTO-6: ~2.4 TiB of real bytes written against a
        // 2.5 TB (decimal) nominal capacity.
        let written = 2_400_000_000_000_i64; // binary side
        let capacity = 2_500_000_000_000_i64; // decimal side, LTO-6
        let line = format_capacity_progress(written, capacity);
        assert!(line.contains("TiB"), "{line}");
        assert!(line.contains("2.50 TB"), "{line}");
        assert!(line.contains("96.0%"), "{line}");
    }

    #[test]
    fn capacity_progress_handles_zero_capacity_without_dividing_by_zero() {
        let line = format_capacity_progress(0, 0);
        assert!(line.contains("0.0%"), "{line}");
    }
}
