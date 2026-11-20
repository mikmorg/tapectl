//! The cutter: a staging's tar stream into slices that each hold whole
//! members (`volume-format-v3.md` §3.3; ADR-0012 item 283; the plan's D3;
//! invariant 13).
//!
//! The stream reader (`tar::stream`) gives the cutter each member's start
//! and whole length before any of its bytes ([`Cutter::member`]), and the
//! stream's tail the same way ([`Cutter::tail`]). The cutter places them in
//! stream order:
//!
//! - a member that fits in what is left of the slice goes into it;
//! - a member that does not fit, and is no larger than `slice_size`, ends
//!   the slice before it and begins the next;
//! - a member larger than `slice_size` ends the slice before it and is cut
//!   at every `slice_size` bytes, its last piece ending where the member
//!   ends: its **run**. The member after it begins a new slice;
//! - the tail is placed as a member is.
//!
//! So a slice begins at a member's start, or continues exactly one member
//! larger than `slice_size`, and the slices joined are the stream. The
//! cutter decides from the length it is given, never looks ahead and never
//! holds a member's bytes: they pass straight to the open slice. A slice is
//! opened only when a byte is placed in it, so none is empty.
//!
//! The geometry is the file table's (`volume::filetable::Row::slices`): a
//! run's pieces are `slice_size` each, the last what remains.

use crate::error::{Result, TapectlError};

/// Where the cutter's slices go.
pub(crate) trait Slices {
    /// Slice `n` (from 1) begins. `continues`: it continues a member begun
    /// in the slice before it (D22).
    fn begin(&mut self, n: u64, continues: bool) -> Result<()>;
    /// The open slice's next bytes.
    fn write(&mut self, bytes: &[u8]) -> Result<()>;
    /// The open slice ends.
    fn end(&mut self, n: u64) -> Result<()>;
}

/// The cutter's state for one stream.
pub(crate) struct Cutter<S> {
    slice_size: u64,
    out: S,
    /// The number of the last slice begun (0 before any).
    n: u64,
    /// Whether slice `n` is open.
    open: bool,
    /// Bytes placed in slice `n`.
    used: u64,
    /// No further member may join slice `n` (it holds the end of a run).
    sealed: bool,
    /// What the member or tail being placed still owes, and whether it is
    /// a run.
    owed: u64,
    run: bool,
}

impl<S: Slices> Cutter<S> {
    /// A cutter at `slice_size`, which must be a positive multiple of 512.
    pub(crate) fn new(slice_size: u64, out: S) -> Result<Self> {
        if slice_size == 0 || !slice_size.is_multiple_of(512) {
            return Err(TapectlError::Other(format!(
                "slice_size {slice_size} is not a positive multiple of 512 bytes"
            )));
        }
        Ok(Self {
            slice_size,
            out,
            n: 0,
            open: false,
            used: 0,
            sealed: false,
            owed: 0,
            run: false,
        })
    }

    fn close(&mut self) -> Result<()> {
        if self.open {
            self.out.end(self.n)?;
            self.open = false;
        }
        Ok(())
    }

    fn begin(&mut self, continues: bool) -> Result<()> {
        self.n += 1;
        self.used = 0;
        self.sealed = false;
        self.open = true;
        self.out.begin(self.n, continues)
    }

    /// Place a member (or the tail) of `len` bytes; returns the slice it
    /// begins in.
    fn place(&mut self, len: u64) -> Result<u64> {
        if self.owed != 0 {
            return Err(TapectlError::Other(format!(
                "the cutter was given a new member with {} bytes of the last one owed",
                self.owed
            )));
        }
        if len == 0 {
            return Err(TapectlError::Other(
                "the cutter was given a member of no bytes".into(),
            ));
        }
        let fits = self.open && !self.sealed && self.used + len <= self.slice_size;
        if !fits {
            self.close()?;
            self.begin(false)?;
        }
        self.owed = len;
        self.run = len > self.slice_size;
        Ok(self.n)
    }

    /// A member begins: its start in the stream and its whole length.
    pub(crate) fn member(&mut self, _start: u64, len: u64) -> Result<u64> {
        self.place(len)
    }

    /// The stream's tail begins.
    pub(crate) fn tail(&mut self, _start: u64, len: u64) -> Result<u64> {
        self.place(len)
    }

    /// The stream's next bytes, those of the member or tail last placed.
    pub(crate) fn bytes(&mut self, mut bytes: &[u8]) -> Result<()> {
        if bytes.len() as u64 > self.owed {
            return Err(TapectlError::Other(format!(
                "the cutter was given {} bytes where the member owes {}",
                bytes.len(),
                self.owed
            )));
        }
        while !bytes.is_empty() {
            if self.used == self.slice_size {
                // Only a run passes the end of a slice.
                debug_assert!(self.run);
                self.close()?;
                self.begin(true)?;
            }
            let room = (self.slice_size - self.used) as usize;
            let take = room.min(bytes.len());
            self.out.write(&bytes[..take])?;
            self.used += take as u64;
            self.owed -= take as u64;
            bytes = &bytes[take..];
        }
        if self.owed == 0 && self.run {
            // The member after a run begins a new slice.
            self.sealed = true;
            self.run = false;
        }
        Ok(())
    }

    /// The stream has ended: close the last slice. Returns how many slices
    /// were made, and the sink.
    pub(crate) fn finish(mut self) -> Result<(u64, S)> {
        if self.owed != 0 {
            return Err(TapectlError::Other(format!(
                "the stream ended with {} bytes of a member owed",
                self.owed
            )));
        }
        self.close()?;
        Ok((self.n, self.out))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::volume::filetable::{Kind, Row};

    #[derive(Default, Debug)]
    struct Mem {
        slices: Vec<(bool, Vec<u8>)>,
        open: bool,
    }

    impl Slices for Mem {
        fn begin(&mut self, n: u64, continues: bool) -> Result<()> {
            assert!(!self.open);
            assert_eq!(n as usize, self.slices.len() + 1);
            self.slices.push((continues, Vec::new()));
            self.open = true;
            Ok(())
        }
        fn write(&mut self, bytes: &[u8]) -> Result<()> {
            assert!(self.open);
            self.slices.last_mut().unwrap().1.extend_from_slice(bytes);
            Ok(())
        }
        fn end(&mut self, n: u64) -> Result<()> {
            assert!(self.open);
            assert_eq!(n as usize, self.slices.len());
            self.open = false;
            Ok(())
        }
    }

    /// Cut members of `lens` (and a tail of `tail`) at `s`; each member's
    /// bytes are its index, so a piece can be traced. Returns the slices,
    /// each member's slice and the rows' geometry.
    fn cut(s: u64, lens: &[u64], tail: u64, chunk: usize) -> (Vec<(bool, Vec<u8>)>, Vec<Row>) {
        let mut c = Cutter::new(s, Mem::default()).unwrap();
        let mut rows = Vec::new();
        let mut at = 0u64;
        for (i, &len) in lens.iter().enumerate() {
            let slice = c.member(at, len).unwrap();
            let data = vec![i as u8; len as usize];
            for piece in data.chunks(chunk) {
                c.bytes(piece).unwrap();
            }
            rows.push(Row {
                kind: Kind::Folder,
                slice,
                start: at,
                end: at + len,
                size: 0,
                sha256: None,
                path: format!("m{i}"),
                link: None,
            });
            at += len;
        }
        if tail > 0 {
            c.tail(at, tail).unwrap();
            c.bytes(&vec![0xff; tail as usize]).unwrap();
        }
        let (n, mem) = c.finish().unwrap();
        assert_eq!(n as usize, mem.slices.len());
        (mem.slices, rows)
    }

    fn sizes(slices: &[(bool, Vec<u8>)]) -> Vec<(bool, usize)> {
        slices.iter().map(|(c, b)| (*c, b.len())).collect()
    }

    /// Every slice begins at a member's start or continues one run, the
    /// slices joined are the stream, and each row's slices are the file
    /// table's geometry.
    fn holds_rule_13(s: u64, slices: &[(bool, Vec<u8>)], rows: &[Row], total: u64) {
        let joined: Vec<u8> = slices.iter().flat_map(|(_, b)| b.clone()).collect();
        assert_eq!(joined.len() as u64, total);
        let mut offset = 0u64;
        let mut starts = Vec::new();
        for (continues, b) in slices {
            assert!(!b.is_empty(), "no empty slice");
            assert!(b.len() as u64 <= s, "no slice past slice_size");
            starts.push((offset, *continues));
            offset += b.len() as u64;
        }
        for (i, (start, continues)) in starts.iter().enumerate() {
            let tail_start = rows.last().map_or(0, |r| r.end);
            let begins_member = rows.iter().any(|r| r.start == *start) || *start == tail_start;
            if *continues {
                // Inside a run: the member around it is longer than s.
                let r = rows.iter().find(|r| r.start < *start && *start < r.end);
                assert!(
                    r.is_some_and(|r| r.len() > s),
                    "slice {} continues a run",
                    i + 1
                );
            } else {
                assert!(begins_member, "slice {} begins at a member's start", i + 1);
            }
        }
        for r in rows {
            let range = r.slices(s);
            let first = starts[(*range.start() - 1) as usize].0;
            assert!(first <= r.start);
            let last = *range.end() as usize;
            assert!(last <= slices.len(), "{r:?}");
            let last_end = starts[last - 1].0 + slices[last - 1].1.len() as u64;
            assert!(last_end >= r.end, "{r:?} ends in its last slice");
            if last < slices.len() {
                assert!(starts[last].0 >= r.end, "{r:?} ends before the next slice");
            }
        }
    }

    #[test]
    fn small_members_share_a_slice_until_the_next_does_not_fit() {
        let (slices, rows) = cut(1024, &[512, 512, 512, 1024, 512], 1024, 100);
        assert_eq!(
            sizes(&slices),
            vec![
                (false, 1024),
                (false, 512),
                (false, 1024),
                (false, 512),
                (false, 1024)
            ]
        );
        assert_eq!(
            rows.iter().map(|r| r.slice).collect::<Vec<_>>(),
            vec![1, 1, 2, 3, 4]
        );
        holds_rule_13(1024, &slices, &rows, 512 * 4 + 1024 + 1024);
    }

    #[test]
    fn a_member_of_exactly_slice_size_fits_an_empty_slice_and_one_block_more_is_a_run() {
        let (slices, rows) = cut(4096, &[4096, 4608, 512], 1024, 4096);
        assert_eq!(
            sizes(&slices),
            vec![(false, 4096), (false, 4096), (true, 512), (false, 1536)]
        );
        assert_eq!(
            rows.iter().map(|r| r.slice).collect::<Vec<_>>(),
            vec![1, 2, 4]
        );
        assert!(!rows[0].is_run(4096));
        assert!(rows[1].is_run(4096));
        holds_rule_13(4096, &slices, &rows, 4096 + 4608 + 512 + 1024);
    }

    #[test]
    fn a_run_ends_the_slice_before_it_and_the_member_after_it_begins_a_new_one() {
        let (slices, rows) = cut(2048, &[512, 5120, 512], 1024, 333);
        assert_eq!(
            sizes(&slices),
            vec![
                (false, 512),
                (false, 2048),
                (true, 2048),
                (true, 1024),
                (false, 1536)
            ]
        );
        assert_eq!(
            rows.iter().map(|r| r.slice).collect::<Vec<_>>(),
            vec![1, 2, 5]
        );
        assert_eq!(rows[1].slices(2048), 2..=4);
        assert!(slices[1].1.iter().all(|b| *b == 1));
        holds_rule_13(2048, &slices, &rows, 512 + 5120 + 512 + 1024);
    }

    /// The tail after a run begins its own slice; a tail larger than a
    /// slice is a run of its own.
    #[test]
    fn the_tail_after_a_run_and_a_tail_that_is_a_run() {
        let (slices, rows) = cut(1024, &[512, 3072], 1024, 4096);
        assert_eq!(
            sizes(&slices),
            vec![
                (false, 512),
                (false, 1024),
                (true, 1024),
                (true, 1024),
                (false, 1024)
            ]
        );
        holds_rule_13(1024, &slices, &rows, 512 + 3072 + 1024);
        let (slices, _) = cut(512, &[512], 10240, 4096);
        assert_eq!(slices.len(), 1 + 20);
        assert!(!slices[1].0 && slices[2..].iter().all(|(c, _)| *c));
    }

    /// The first member a run: no empty slice before it.
    #[test]
    fn a_stream_that_begins_with_a_run_makes_no_empty_slice() {
        let (slices, rows) = cut(1024, &[2560], 1024, 4096);
        assert_eq!(
            sizes(&slices),
            vec![(false, 1024), (true, 1024), (true, 512), (false, 1024)]
        );
        assert_eq!(rows[0].slice, 1);
        holds_rule_13(1024, &slices, &rows, 2560 + 1024);
    }

    #[test]
    fn a_slice_size_that_is_not_a_multiple_of_512_is_refused() {
        assert!(Cutter::new(1000, Mem::default()).is_err());
        assert!(Cutter::new(0, Mem::default()).is_err());
        assert!(Cutter::new(512, Mem::default()).is_ok());
    }

    #[test]
    fn bytes_past_what_a_member_owes_are_refused() {
        let mut c = Cutter::new(1024, Mem::default()).unwrap();
        c.member(0, 512).unwrap();
        assert!(c.bytes(&[0; 513]).is_err());
    }
}
