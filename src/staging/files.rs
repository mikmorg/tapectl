//! Every file `stage create` creates under the staging directory, and the
//! only way it creates one (issue #370, ADR-0012 amendment 2026-10-06
//! item 4: the staging device holds no plaintext).
//!
//! [`StagingDir`] has exactly two ways to create a file:
//!
//! - [`StagingDir::prepare`]'s writability probe, `.tapectl-stage-probe-<pid>`,
//!   holding a fixed string and removed at once;
//! - [`StagingDir::create_slice`], a `{base}.{N}.tar.age` whose bytes are
//!   age ciphertext from its first byte: the slice's cut of the tar stream
//!   is written into an age stream in memory (packed first, in memory, when
//!   the staging is compressed: `staging::pack`) and only its ciphertext
//!   reaches the file.
//!
//! tar never writes under the staging directory either: it writes its
//! stream to standard output (`tar::writer::spawn_archive`, `--file=-`),
//! and its file list is in the tapectl home. So plaintext on the staging
//! device is not something this module avoids writing, it is something it
//! has no way to write. The proof has three layers (ADR-0012, 2026-10-06
//! amendment item 4): this construction, pinned by the tests below
//! (`stage_create` creates no file but through here, and tar's argv names
//! its archive as standard output);
//! an inotify audit of a real stage (`staging::plaintext_audit`); and a
//! raw-image scan with a deleted canary (`scripts/plaintext-scan.sh`).
//!
//! Every other file any command creates under the staging root, and why it
//! carries no plaintext of the archive:
//!
//! | Path under staging | Created by | Contents |
//! |---|---|---|
//! | `sessions/<label>-<uuid>/` ID thunk, system guide, `RESTORE.sh`, front index, seal marker | `volume::build` (`materialize`) | the public zones written to tape in plaintext; no names, by the isolation invariant (`volume-format-v2.md` §2) |
//! | `sessions/<label>-<uuid>/layout.json` | `volume::build` | the Layout: positions, sizes, ciphertext hashes, staging paths of `.age` slices |
//! | `sessions/<label>-<uuid>/` tenant and operator envelopes, operator envelope backup | `volume::build` (`materialize_envelope_streaming`; the backup is a byte copy) | age ciphertext, encrypted as the tar is built |
//! | `clone-<label>-<unit uuid>/slice_<id>.dat`, `compact-<label>/slice_<id>.dat` | `volume read-slices`, `volume compact-read` | `.age` slices copied off tape: ciphertext |
//! | the `TAPECTL_TEST_PAUSE_AFTER_SEAL` marker | a test hook, at the path the variable names | the word `parked` |
//!
//! The write's `catalog.db` and tar's file list, both plaintext, are made
//! under `<home>/tmp` (`staging::home_work_dir`), never here.

use std::fs;
use std::io::{self, BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{sync_channel, SyncSender};
use std::thread::JoinHandle;

use crate::pass_count::Sha256;
use sha2::Digest;

use super::pack::{self, Packer, Packing};
use super::{build_encryptor, make_durable, staging_io_error, EncryptedSliceInfo};
use crate::error::{Result, TapectlError};

/// The staging directory, as `stage create` writes into it.
pub(crate) struct StagingDir {
    root: PathBuf,
}

impl StagingDir {
    /// Make sure `stage create` can use `root` at all: create it if it is
    /// missing, then prove it is writable by creating (and at once removing)
    /// a file in it (issue #354).
    ///
    /// Writability is tested by doing the write, the way `config check`'s
    /// `policy::depth_check::check_staging` does, because mode bits lie under
    /// root, ACLs and read-only mounts. Without the probe an unwritable
    /// directory passed every check here and failed only once staging began.
    pub(crate) fn prepare(root: &Path) -> Result<Self> {
        fs::create_dir_all(root)
            .map_err(|e| staging_io_error("cannot create staging directory", root, e))?;
        let probe = root.join(format!(".tapectl-stage-probe-{}", std::process::id()));
        fs::write(&probe, b"tapectl stage create probe")
            .map_err(|e| staging_io_error("cannot write to staging directory", root, e))?;
        let _ = fs::remove_file(&probe);
        Ok(Self {
            root: root.to_path_buf(),
        })
    }

    /// Create slice `n` of the staging `base` (`staging::archive_base_name`)
    /// as `{base}.{n}.tar.age`, encrypted to `recipients`, packed first when
    /// `packing` says so. Everything written to the returned writer is the
    /// slice's cut; the file receives only its age ciphertext.
    pub(crate) fn create_slice(
        &self,
        base: &str,
        n: u64,
        recipients: &[String],
        packing: Option<Packing>,
    ) -> Result<SliceWriter> {
        if base.is_empty() || base.contains('/') || base.starts_with('.') {
            return Err(TapectlError::Other(format!(
                "refusing a staged slice name built from {base:?}"
            )));
        }
        let path = self.root.join(slice_file_name(base, n));
        // The recipients are parsed before the file exists, so a bad key
        // leaves nothing behind.
        let encryptor = build_encryptor(recipients)?;
        let file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
            .map_err(|e| staging_io_error("cannot create staged slice", &path, e))?;
        SliceWriter::over(path, file, encryptor, packing)
    }
}

/// The suffix of every staged slice's file: a cut of the tar stream, age
/// ciphertext (`staging::clean` finds leftovers by it).
pub(crate) const SLICE_SUFFIX: &str = ".tar.age";

/// `{base}.{n}.tar.age`.
pub(crate) fn slice_file_name(base: &str, n: u64) -> String {
    format!("{base}.{n}{SLICE_SUFFIX}")
}

fn slice_io_error(path: &Path, e: io::Error) -> TapectlError {
    staging_io_error("cannot write staged slice", path, e)
}

/// [`slice_io_error`]'s text as an `io::Error` of the same kind, for the
/// `Write` impl, whose errors travel up as `io::Error` (the slicer's `?`
/// makes them `TapectlError::Io`, which prints its text once). `StorageFull`
/// stays `StorageFull`; the errno is in the text.
fn slice_write_error(path: &Path, e: io::Error) -> io::Error {
    let kind = e.kind();
    io::Error::new(kind, slice_io_error(path, e).to_string())
}

/// sha256 on its own thread, fed copies of the bytes, so hashing the
/// plaintext and the ciphertext runs beside encryption instead of after it
/// (issue #364: one core used to do read, hash, encrypt and hash in turn).
/// The channel is bounded, so at most `HASH_QUEUE` chunks wait.
struct HashThread {
    tx: Option<SyncSender<Vec<u8>>>,
    handle: Option<JoinHandle<String>>,
}

const HASH_QUEUE: usize = 16;

impl HashThread {
    fn spawn() -> Self {
        let (tx, rx) = sync_channel::<Vec<u8>>(HASH_QUEUE);
        let handle = std::thread::spawn(move || {
            let mut h = Sha256::new();
            for chunk in rx {
                h.update(&chunk);
            }
            format!("{:x}", h.finalize())
        });
        Self {
            tx: Some(tx),
            handle: Some(handle),
        }
    }

    fn feed(&mut self, bytes: &[u8]) {
        if let Some(tx) = &self.tx {
            // The receiver only goes away with this struct.
            let _ = tx.send(bytes.to_vec());
        }
    }

    fn finish(mut self) -> String {
        drop(self.tx.take());
        self.handle
            .take()
            .and_then(|h| h.join().ok())
            .unwrap_or_default()
    }
}

impl Drop for HashThread {
    fn drop(&mut self) {
        drop(self.tx.take());
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

/// The file end of a slice: ciphertext only, hashed and counted on its way.
struct CipherSink {
    file: BufWriter<fs::File>,
    hasher: HashThread,
    len: u64,
}

impl Write for CipherSink {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let n = self.file.write(buf)?;
        self.hasher.feed(&buf[..n]);
        self.len += n as u64;
        Ok(n)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.file.flush()
    }
}

/// The age stream of one slice.
type AgeStream = age::stream::StreamWriter<CipherSink>;

/// What a slice's cut goes into: the age stream itself, or a packer whose
/// frame goes into it (`staging::pack`).
enum Body {
    Plain(AgeStream),
    Packed(Box<Packer<AgeStream>>, Packing),
}

/// One staged slice being written: its cut in, age ciphertext to the file.
/// [`SliceWriter::finish`] completes the frame (when packed) and the age
/// stream, syncs the file and its directory entry (issue #409), and returns
/// the slice's sizes and hashes. A packed slice is decoded again as it is
/// packed, and a slice whose frame does not decode to its cut is refused
/// (D15). A writer dropped without finishing, or whose finish fails,
/// removes its file, so a stage that fails partway leaves no half-written
/// `.age` behind.
pub(crate) struct SliceWriter {
    path: PathBuf,
    body: Option<Body>,
    plain: Option<HashThread>,
    plain_len: u64,
}

impl SliceWriter {
    /// A slice named `path` whose ciphertext goes to `file`, already open.
    fn over(
        path: PathBuf,
        file: fs::File,
        encryptor: age::Encryptor,
        packing: Option<Packing>,
    ) -> Result<Self> {
        let sink = CipherSink {
            file: BufWriter::with_capacity(1 << 20, file),
            hasher: HashThread::spawn(),
            len: 0,
        };
        let stream = match encryptor.wrap_output(sink) {
            Ok(stream) => stream,
            Err(e) => {
                let _ = fs::remove_file(&path);
                return Err(slice_io_error(&path, e));
            }
        };
        let body = match packing {
            None => Body::Plain(stream),
            Some(p) => match Packer::new(p, stream) {
                Ok(packer) => Body::Packed(Box::new(packer), p),
                Err(e) => {
                    let _ = fs::remove_file(&path);
                    return Err(slice_io_error(&path, e));
                }
            },
        };
        Ok(SliceWriter {
            path,
            body: Some(body),
            plain: Some(HashThread::spawn()),
            plain_len: 0,
        })
    }

    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    /// Finish the slice: the frame's end when it is packed, then the age
    /// stream's last chunk (without it the file hashes fine but cannot be
    /// decrypted), then a sync of the file and of the directory entry that
    /// names it.
    pub(crate) fn finish(mut self) -> Result<EncryptedSliceInfo> {
        let body = self.body.take().expect("a SliceWriter finishes once");
        let plain = self.plain.take().expect("a SliceWriter finishes once");
        let finished = (|| {
            let (stream, decoded) = match body {
                Body::Plain(stream) => (stream, None),
                Body::Packed(packer, p) => {
                    let (stream, decoded) =
                        packer.finish().map_err(|e| slice_io_error(&self.path, e))?;
                    (stream, Some((decoded, p)))
                }
            };
            let sha256_plain = plain.finish();
            let mut packed_bytes = None;
            if let Some((decoded, p)) = decoded {
                // D15: what was hashed is what goes to tape.
                if decoded.len != self.plain_len || decoded.sha256 != sha256_plain {
                    return Err(TapectlError::Other(format!(
                        "staged slice {}: packed with {} at level {}, it decodes to {} bytes                          of sha256 {}, and its cut was {} bytes of sha256 {}. A slice whose                          packed bytes are not its cut is never kept; nothing was staged.",
                        self.path.display(),
                        p.algorithm.name(),
                        p.level,
                        decoded.len,
                        decoded.sha256,
                        self.plain_len,
                        sha256_plain
                    )));
                }
                packed_bytes = Some(decoded.packed_len);
            }
            let mut sink = stream.finish().map_err(|e| slice_io_error(&self.path, e))?;
            sink.flush().map_err(|e| slice_io_error(&self.path, e))?;
            let CipherSink { file, hasher, len } = sink;
            let file = file
                .into_inner()
                .map_err(|e| slice_io_error(&self.path, e.into_error()))?;
            make_durable(&file, &self.path)?;
            // Issue #417: synced, so its pages are clean; the next read of
            // this slice is a tape write, possibly hours away, and a stage
            // writes a cartridge's worth of them through the page cache.
            crate::util::drop_cached(&file, &self.path);
            Ok(EncryptedSliceInfo {
                plain_size: self.plain_len as i64,
                sha256_plain,
                encrypted_size: len as i64,
                sha256_encrypted: hasher.finish(),
                packed_size: packed_bytes,
            })
        })();
        if finished.is_err() {
            let _ = fs::remove_file(&self.path);
        }
        finished
    }
}

impl Write for SliceWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let body = self
            .body
            .as_mut()
            .ok_or_else(|| io::Error::other("slice already finished"))?;
        // Past the 1 MiB buffer the disk filling up surfaces here, mid-slice
        // (issue #465): name the slice, as `finish` does.
        let n = match body {
            Body::Plain(stream) => stream.write(buf),
            Body::Packed(packer, _) => {
                #[cfg(test)]
                if let Some(bent) = pack_hook::bend(buf) {
                    packer.write_all(&bent)?;
                    if let Some(plain) = self.plain.as_mut() {
                        plain.feed(buf);
                    }
                    self.plain_len += buf.len() as u64;
                    return Ok(buf.len());
                }
                packer.write(buf)
            }
        }
        .map_err(|e| slice_write_error(&self.path, e))?;
        if let Some(plain) = self.plain.as_mut() {
            plain.feed(&buf[..n]);
        }
        self.plain_len += n as u64;
        Ok(n)
    }

    fn flush(&mut self) -> io::Result<()> {
        match self.body.as_mut() {
            Some(Body::Plain(s)) => s.flush().map_err(|e| slice_write_error(&self.path, e)),
            Some(Body::Packed(p, _)) => p.flush().map_err(|e| slice_write_error(&self.path, e)),
            None => Ok(()),
        }
    }
}

impl Drop for SliceWriter {
    fn drop(&mut self) {
        if self.body.take().is_some() {
            let _ = fs::remove_file(&self.path);
        }
    }
}

/// Test-only: bend the bytes a packed slice's packer is given (never the
/// cut's own hash), the way a packer that lost a byte would: the positive
/// control of D15's check.
#[cfg(test)]
pub(crate) mod pack_hook {
    use std::cell::Cell;

    thread_local! {
        static ARMED: Cell<bool> = const { Cell::new(false) };
    }

    /// Flip one bit of the next write a packer on this thread is given.
    pub(crate) fn arm() {
        ARMED.with(|a| a.set(true));
    }

    pub(super) fn bend(buf: &[u8]) -> Option<Vec<u8>> {
        if buf.is_empty() || !ARMED.with(|a| a.replace(false)) {
            return None;
        }
        let mut v = buf.to_vec();
        v[0] ^= 1;
        Some(v)
    }
}

/// The most a slice's packed frame may hold: `slice_size` plus the
/// algorithm's stated worst growth (`volume-format-v3.md` §3.4, §3.5).
pub(crate) fn packed_bound(slice_size: u64) -> u64 {
    slice_size + pack::worst_growth(slice_size)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;

    fn recipient() -> (age::x25519::Identity, Vec<String>) {
        let id = age::x25519::Identity::generate();
        let pk = id.to_public().to_string();
        (id, vec![pk])
    }

    #[test]
    fn a_slice_file_holds_ciphertext_that_decrypts_to_what_was_written() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = StagingDir::prepare(tmp.path()).unwrap();
        let (id, recipients) = recipient();
        let plain: Vec<u8> = (0..300_000u32).map(|i| (i % 251) as u8).collect();
        let mut w = dir.create_slice("abc_v1_s1", 1, &recipients, None).unwrap();
        w.write_all(&plain[..1000]).unwrap();
        w.write_all(&plain[1000..]).unwrap();
        let path = w.path().to_path_buf();
        let info = w.finish().unwrap();

        let bytes = fs::read(&path).unwrap();
        assert!(bytes.starts_with(b"age-encryption.org/v1\n"));
        assert_eq!(info.encrypted_size, bytes.len() as i64);
        assert_eq!(
            info.sha256_encrypted,
            format!("{:x}", Sha256::digest(&bytes))
        );
        assert_eq!(info.plain_size, plain.len() as i64);
        assert_eq!(info.sha256_plain, format!("{:x}", Sha256::digest(&plain)));

        let mut out = Vec::new();
        age::Decryptor::new(&bytes[..])
            .unwrap()
            .decrypt(std::iter::once(&id as &dyn age::Identity))
            .unwrap()
            .read_to_end(&mut out)
            .unwrap();
        assert_eq!(out, plain);
        assert_eq!(
            fs::read_dir(tmp.path()).unwrap().count(),
            1,
            "the probe is gone; only the slice remains"
        );
    }

    /// Issue #417: a finished slice — synced, so clean — is dropped from the
    /// page cache; its next reader is a tape write, maybe hours away.
    #[test]
    fn a_finished_slice_is_dropped_from_the_page_cache() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = StagingDir::prepare(tmp.path()).unwrap();
        let (_id, recipients) = recipient();
        let mut w = dir.create_slice("abc_v1_s1", 1, &recipients, None).unwrap();
        w.write_all(b"slice bytes").unwrap();
        let path = w.path().to_path_buf();
        crate::util::page_cache_log::take();
        w.finish().unwrap();
        assert_eq!(crate::util::page_cache_log::take(), [path]);
    }

    #[test]
    fn an_unfinished_slice_is_removed() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = StagingDir::prepare(tmp.path()).unwrap();
        let (_, recipients) = recipient();
        let mut w = dir.create_slice("abc_v1_s1", 1, &recipients, None).unwrap();
        w.write_all(b"half a slice").unwrap();
        let path = w.path().to_path_buf();
        assert!(path.exists());
        drop(w);
        assert!(!path.exists(), "a dropped, unfinished slice is removed");
    }

    #[test]
    fn a_bad_recipient_key_errors_before_any_file_is_created() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = StagingDir::prepare(tmp.path()).unwrap();
        let err = dir
            .create_slice("abc_v1_s1", 1, &["not-an-age-key".to_string()], None)
            .err()
            .expect("a malformed recipient is refused");
        assert!(err.to_string().contains("invalid public key"), "{err}");
        assert_eq!(fs::read_dir(tmp.path()).unwrap().count(), 0);
    }

    /// Issue #354 (a), kept by #370: the staging filesystem filling up while
    /// a slice is written names the slice and the operation, and the cause
    /// once. The ciphertext goes to `/dev/full`, where every write is ENOSPC;
    /// the slice's own path is a name in the temp directory, so removing it
    /// on failure never touches the device.
    #[test]
    fn a_full_staging_filesystem_names_the_slice_once() {
        let Ok(full) = fs::OpenOptions::new().write(true).open("/dev/full") else {
            eprintln!("skipping: no /dev/full");
            return;
        };
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("abc_v1_s1.1.tar.age");
        let (_, recipients) = recipient();
        let mut w = SliceWriter::over(
            path.clone(),
            full,
            build_encryptor(&recipients).unwrap(),
            None,
        )
        .unwrap();
        w.write_all(&[7u8; 200_000]).unwrap();
        let err = w.finish().err().expect("every write to /dev/full fails");
        let msg = err.to_string();
        assert!(
            msg.contains("cannot write staged slice") && msg.contains(&*path.to_string_lossy()),
            "names the operation and the slice: {msg}"
        );
        assert_eq!(
            msg.matches("No space left on device").count(),
            1,
            "the cause, printed once: {msg}"
        );
        assert!(Path::new("/dev/full").exists(), "the device is untouched");
    }

    /// Issue #465 item 3: past the 1 MiB `BufWriter` the disk filling up
    /// surfaces from `write` itself, in the middle of a slice, and that
    /// error used to reach the operator bare — `No space left on device (os
    /// error 28)`, naming no slice. It names the operation and the slice
    /// once, like the one `finish` raises, and keeps its kind.
    #[test]
    fn a_full_staging_filesystem_mid_slice_names_the_slice_once() {
        let Ok(full) = fs::OpenOptions::new().write(true).open("/dev/full") else {
            eprintln!("skipping: no /dev/full");
            return;
        };
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("abc_v1_s1.1.tar.age");
        let (_, recipients) = recipient();
        let mut w = SliceWriter::over(
            path.clone(),
            full,
            build_encryptor(&recipients).unwrap(),
            None,
        )
        .unwrap();
        // Random-looking bytes: age does not compress, so 4 MiB in is more
        // than the buffer holds whatever the framing adds.
        let chunk: Vec<u8> = (0..65_536u32)
            .map(|i| (i.wrapping_mul(2_654_435_761) >> 13) as u8)
            .collect();
        let mut failed = None;
        for _ in 0..64 {
            if let Err(e) = w.write_all(&chunk) {
                failed = Some(e);
                break;
            }
        }
        let err = failed.expect("a write past the buffer reaches /dev/full and fails");
        assert_eq!(err.kind(), io::ErrorKind::StorageFull, "{err:?}");
        let operator: TapectlError = err.into();
        let msg = operator.to_string();
        assert!(
            msg.contains("cannot write staged slice") && msg.contains(&*path.to_string_lossy()),
            "names the operation and the slice: {msg}"
        );
        assert_eq!(
            msg.matches("No space left on device").count(),
            1,
            "the cause, printed once: {msg}"
        );
    }

    /// The production source of `name`'s body in `file`'s non-test part.
    fn body_of(src: &'static str, name: &str) -> &'static str {
        let prod = src.split("#[cfg(test)]\nmod tests").next().unwrap();
        let start = prod.find(name).unwrap_or_else(|| panic!("no {name}"));
        let end = prod[start..].find("\n}\n").unwrap() + start;
        &prod[start..end]
    }

    /// Issue #370, by construction: `stage_create` creates no file itself —
    /// every file it makes under staging comes from `StagingDir` — and this
    /// module creates files in exactly two places, the probe and the slice.
    #[test]
    fn stage_create_creates_files_only_through_the_staging_dir() {
        const CREATES: [&str; 5] = [
            "fs::write(",
            "File::create(",
            "OpenOptions",
            "fs::copy(",
            "create_new(",
        ];
        let stage = body_of(include_str!("mod.rs"), "fn stage_create_inner(");
        for call in CREATES {
            assert!(
                !stage.contains(call),
                "stage_create_inner must not call {call}: files under staging come \
                 from StagingDir only"
            );
        }
        // The slices come from the cutter's sink (`staging::archive`), which
        // creates nothing itself either.
        let archive = include_str!("archive.rs");
        let archive = archive.split("#[cfg(test)]").next().unwrap();
        for call in CREATES {
            assert!(
                !archive.contains(call),
                "staging::archive must not call {call}: files under staging come \
                 from StagingDir only"
            );
        }
        assert!(
            stage.contains("files::StagingDir::prepare(")
                && stage.contains("archive::StagedSlices")
                && archive.contains(".create_slice("),
            "positive control: stage_create_inner goes through StagingDir"
        );
        let src = include_str!("files.rs");
        let prod = src.split("#[cfg(test)]\nmod tests").next().unwrap();
        let creations: usize = CREATES.iter().map(|c| prod.matches(c).count()).sum();
        let prepare = body_of(src, "pub(crate) fn prepare(");
        let create_slice = body_of(src, "pub(crate) fn create_slice(");
        assert_eq!(prepare.matches("fs::write(").count(), 1, "the probe");
        assert_eq!(
            create_slice.matches("OpenOptions").count()
                + create_slice.matches("create_new(").count(),
            2,
            "the slice, opened create_new"
        );
        assert_eq!(
            creations, 3,
            "files.rs creates nothing else: {creations} calls"
        );
    }

    /// Issue #370, by construction: tar's argv for the archive names no path
    /// under the staging directory: the archive is `--file=-`, standard
    /// output, and it is named once.
    #[test]
    fn tar_is_given_standard_output_as_its_archive() {
        let body = body_of(include_str!("../tar/writer.rs"), "pub fn create_args(");
        assert!(body.contains(r#""--file=-""#), "{body}");
        assert_eq!(body.matches("\"--file=").count(), 1, "one archive argument");
    }

    /// D15's check, its positive control: a packer given a bent byte makes
    /// a frame that decodes to something other than the cut, and the slice
    /// is refused and removed.
    #[test]
    fn a_packed_slice_that_does_not_decode_to_its_cut_is_refused() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = StagingDir::prepare(tmp.path()).unwrap();
        let (_, recipients) = recipient();
        for algorithm in [
            pack::Algorithm::Gzip,
            pack::Algorithm::Xz,
            pack::Algorithm::Zstd,
        ] {
            let p = Packing {
                algorithm,
                level: algorithm.default_level(),
            };
            let mut w = dir
                .create_slice("abc_v1_s1", 1, &recipients, Some(p))
                .unwrap();
            w.write_all(b"a cut whose bytes are packed as they are")
                .unwrap();
            let info = w.finish().unwrap();
            assert!(info.packed_size.is_some());
            fs::remove_file(tmp.path().join("abc_v1_s1.1.tar.age")).unwrap();

            let mut w = dir
                .create_slice("abc_v1_s1", 1, &recipients, Some(p))
                .unwrap();
            let path = w.path().to_path_buf();
            pack_hook::arm();
            w.write_all(b"a cut whose bytes are bent on their way into the packer")
                .unwrap();
            let err = w
                .finish()
                .err()
                .expect("the frame does not decode to the cut");
            let msg = err.to_string();
            assert!(
                msg.contains(algorithm.name()) && msg.contains("decodes to"),
                "{msg}"
            );
            assert!(!path.exists(), "the refused slice is removed");
        }
    }

    /// A packed slice decrypts to a frame the algorithm's own tool reads.
    #[test]
    fn a_packed_slice_decrypts_to_one_frame_of_its_cut() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = StagingDir::prepare(tmp.path()).unwrap();
        let (id, recipients) = recipient();
        let cut: Vec<u8> = b"words again "
            .iter()
            .cycle()
            .take(200_000)
            .copied()
            .collect();
        let p = Packing {
            algorithm: pack::Algorithm::Zstd,
            level: 3,
        };
        let mut w = dir
            .create_slice("abc_v1_s1", 1, &recipients, Some(p))
            .unwrap();
        w.write_all(&cut).unwrap();
        let path = w.path().to_path_buf();
        let info = w.finish().unwrap();
        assert_eq!(info.plain_size, cut.len() as i64);
        assert_eq!(info.sha256_plain, format!("{:x}", Sha256::digest(&cut)));
        let bytes = fs::read(&path).unwrap();
        let mut frame = Vec::new();
        age::Decryptor::new(&bytes[..])
            .unwrap()
            .decrypt(std::iter::once(&id as &dyn age::Identity))
            .unwrap()
            .read_to_end(&mut frame)
            .unwrap();
        assert_eq!(info.packed_size, Some(frame.len() as u64));
        assert!(frame.len() < cut.len() / 10);
        assert_eq!(zstd::decode_all(&frame[..]).unwrap(), cut);
    }

    #[test]
    fn a_name_that_could_leave_the_directory_is_refused() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = StagingDir::prepare(tmp.path()).unwrap();
        let (_, recipients) = recipient();
        for base in ["../x", "a/b", "", ".hidden"] {
            assert!(
                dir.create_slice(base, 1, &recipients, None).is_err(),
                "{base:?}"
            );
        }
    }
}
