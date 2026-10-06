//! Every file `stage create` creates under the staging directory, and the
//! only way it creates one (issue #370, ADR-0012 amendment 2026-10-06
//! item 4: the staging device holds no plaintext).
//!
//! [`StagingDir`] has exactly two ways to create a file:
//!
//! - [`StagingDir::prepare`]'s writability probe, `.tapectl-stage-probe-<pid>`,
//!   holding a fixed string and removed at once;
//! - [`StagingDir::create_slice`], a `{base}.{N}.dar.age` whose bytes are
//!   age ciphertext from its first byte: the plaintext slice is written into
//!   an age stream in memory and only its ciphertext reaches the file.
//!
//! dar never writes under the staging directory either: it writes its
//! archive to standard output (`dar::create::DarStream`), and its on-the-fly
//! catalogue into the tapectl home. So plaintext on the staging device is
//! not something this module avoids writing, it is something it has no way
//! to write.

use std::fs;
use std::io::{self, BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{sync_channel, SyncSender};
use std::thread::JoinHandle;

use sha2::{Digest, Sha256};

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

    /// Create slice `n` of the archive `base` (`staging::archive_base_name`)
    /// as `{base}.{n}.dar.age`, encrypted to `recipients`. Everything written
    /// to the returned writer is the plaintext slice; the file receives
    /// only its age ciphertext.
    pub(crate) fn create_slice(
        &self,
        base: &str,
        n: u32,
        recipients: &[String],
    ) -> Result<SliceWriter> {
        if base.is_empty() || base.contains('/') || base.starts_with('.') {
            return Err(TapectlError::Other(format!(
                "refusing a staged slice name built from {base:?}"
            )));
        }
        let path = self.root.join(format!("{base}.{n}.dar.age"));
        // The recipients are parsed before the file exists, so a bad key
        // leaves nothing behind.
        let encryptor = build_encryptor(recipients)?;
        let file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
            .map_err(|e| staging_io_error("cannot create staged slice", &path, e))?;
        SliceWriter::over(path, file, encryptor)
    }
}

fn slice_io_error(path: &Path, e: io::Error) -> TapectlError {
    staging_io_error("cannot write staged slice", path, e)
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

/// One staged slice being written: plaintext in, age ciphertext to the
/// file. [`SliceWriter::finish`] completes the age stream, syncs the file
/// and its directory entry (issue #409), and returns the slice's sizes and
/// hashes. A writer dropped without finishing — or whose finish fails —
/// removes its file, so a stage that fails partway leaves no half-written
/// `.age` behind.
pub(crate) struct SliceWriter {
    path: PathBuf,
    stream: Option<age::stream::StreamWriter<CipherSink>>,
    plain: Option<HashThread>,
    plain_len: u64,
}

impl SliceWriter {
    /// A slice named `path` whose ciphertext goes to `file`, already open.
    fn over(path: PathBuf, file: fs::File, encryptor: age::Encryptor) -> Result<Self> {
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
        Ok(SliceWriter {
            path,
            stream: Some(stream),
            plain: Some(HashThread::spawn()),
            plain_len: 0,
        })
    }

    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    /// Finish the slice: the age stream's last chunk (without it the file
    /// hashes fine but cannot be decrypted), then a sync of the file and of
    /// the directory entry that names it.
    pub(crate) fn finish(mut self) -> Result<EncryptedSliceInfo> {
        let stream = self.stream.take().expect("a SliceWriter finishes once");
        let plain = self.plain.take().expect("a SliceWriter finishes once");
        let finished = (|| {
            let mut sink = stream.finish().map_err(|e| slice_io_error(&self.path, e))?;
            sink.flush().map_err(|e| slice_io_error(&self.path, e))?;
            let CipherSink { file, hasher, len } = sink;
            let file = file
                .into_inner()
                .map_err(|e| slice_io_error(&self.path, e.into_error()))?;
            make_durable(&file, &self.path)?;
            Ok(EncryptedSliceInfo {
                plain_size: self.plain_len as i64,
                sha256_plain: plain.finish(),
                encrypted_size: len as i64,
                sha256_encrypted: hasher.finish(),
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
        let stream = self
            .stream
            .as_mut()
            .ok_or_else(|| io::Error::other("slice already finished"))?;
        let n = stream.write(buf)?;
        if let Some(plain) = self.plain.as_mut() {
            plain.feed(&buf[..n]);
        }
        self.plain_len += n as u64;
        Ok(n)
    }

    fn flush(&mut self) -> io::Result<()> {
        match self.stream.as_mut() {
            Some(s) => s.flush(),
            None => Ok(()),
        }
    }
}

impl Drop for SliceWriter {
    fn drop(&mut self) {
        if self.stream.take().is_some() {
            let _ = fs::remove_file(&self.path);
        }
    }
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
        let mut w = dir.create_slice("abc_v1_s1", 1, &recipients).unwrap();
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

    #[test]
    fn an_unfinished_slice_is_removed() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = StagingDir::prepare(tmp.path()).unwrap();
        let (_, recipients) = recipient();
        let mut w = dir.create_slice("abc_v1_s1", 1, &recipients).unwrap();
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
            .create_slice("abc_v1_s1", 1, &["not-an-age-key".to_string()])
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
        let path = tmp.path().join("abc_v1_s1.1.dar.age");
        let (_, recipients) = recipient();
        let mut w =
            SliceWriter::over(path.clone(), full, build_encryptor(&recipients).unwrap()).unwrap();
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

    #[test]
    fn a_name_that_could_leave_the_directory_is_refused() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = StagingDir::prepare(tmp.path()).unwrap();
        let (_, recipients) = recipient();
        for base in ["../x", "a/b", "", ".hidden"] {
            assert!(dir.create_slice(base, 1, &recipients).is_err(), "{base:?}");
        }
    }
}
