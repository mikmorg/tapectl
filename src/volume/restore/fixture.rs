//! The fixture the restore tests share: a tapectl home in which every unit
//! is staged by the real `stage create` (GNU tar, the real cut, age, and
//! the file table), and every tape is written by the real `volume write`
//! over an in-memory medium. Nothing is made by hand but the files of a
//! unit and the damage a test does to a tape (invariant 8: a fixture made
//! another way hid #518 for a month).
//!
//! Modelled on the span proofs' home (`volume::span_proofs`), which is
//! private to its module and whose drive is armed for the early warning;
//! this one writes tapes plain and adds what a restore test asks of a tape:
//! where a slice lies, a byte flipped in it, a scripted operator
//! ([`super::repair_tests::tests::TestBay`]).

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use rusqlite::{params, Connection};
use sha2::Digest;
use tempfile::TempDir;

use super::repair_tests::tests::{Reply, TestBay};
use super::{RestoreOptions, RestoreReport, RestoreTarget};
use crate::cli::cartridge::CartridgeCommands;
use crate::config::{Config, LtoBackendConfig, TapectlPaths};
use crate::error::Result;
use crate::store::{MemStore, Store};
use crate::tape::contact::{ContactSite, ContactSlot, Medium, Operation};
use crate::volume::write::{self, ContactStore};

/// The tape's block size: small, so a fixture's slices are a few blocks.
pub(crate) const BS: usize = 4096;
/// The drive: a path no device is at, so every fact check that reads the
/// hardware finds nothing and the injected store is the medium.
pub(crate) const DEVICE: &str = "/nonexistent/tapectl-restore-fixture-nst";
const DEVICE_SG: &str = "/nonexistent/tapectl-restore-fixture-sg";
const BACKEND: &str = "lto6rst";

pub(crate) const OPERATOR: &str = "quince-operator-main";
pub(crate) const TENANT: &str = "persimmon-tenant-one";
pub(crate) const TENANT_TWO: &str = "persimmon-tenant-two";

/// Deterministic incompressible bytes, so a slice's size follows its files'.
pub(crate) fn noise(len: usize, seed: u32) -> Vec<u8> {
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

/// One thing in a unit's folder.
#[derive(Debug, Clone)]
pub(crate) enum Item {
    /// A regular file: its path (relative to the unit) and bytes.
    File(String, Vec<u8>),
    /// A folder.
    Dir(String),
    /// A symbolic link: its path and what it points at.
    Symlink(String, String),
    /// A hard link: its path and the path of the file it is another name of.
    Hard(String, String),
    /// The same four with names given as bytes, for a name that is not UTF-8
    /// (ADR-0012 item 297, #569): Linux takes any byte but `/` and NUL.
    FileB(Vec<u8>, Vec<u8>),
    DirB(Vec<u8>),
    /// A symbolic link: its path and its target, both bytes.
    SymlinkB(Vec<u8>, Vec<u8>),
    /// A hard link: its path and the path of the file it is another name of.
    HardB(Vec<u8>, Vec<u8>),
}

/// `bytes` as a path.
pub(crate) fn os(bytes: &[u8]) -> &std::ffi::OsStr {
    std::os::unix::ffi::OsStrExt::from_bytes(bytes)
}

impl Item {
    pub(crate) fn file(path: &str, bytes: Vec<u8>) -> Item {
        Item::File(path.to_string(), bytes)
    }
}

/// `n` files `tag-NN.bin` of `size` bytes each, each different.
pub(crate) fn files(tag: &str, n: usize, size: usize, seed: u32) -> Vec<Item> {
    (0..n)
        .map(|i| {
            Item::File(
                format!("{tag}-{:02}.bin", i + 1),
                noise(size, seed + i as u32 * 7919),
            )
        })
        .collect()
}

/// A tapectl home with its operator, a tenant and the escrow recipient, one
/// drive, and the volumes made in it.
pub(crate) struct Home {
    pub(crate) conn: Connection,
    pub(crate) paths: TapectlPaths,
    pub(crate) config: Config,
    pub(crate) sources: PathBuf,
    /// Each medium as its last contact left it.
    pub(crate) tapes: BTreeMap<String, MemStore>,
    _tmp: TempDir,
}

impl Home {
    /// `slice_size` is `[defaults] slice_size` (a multiple of 512);
    /// `compression` is `[defaults] compression` when given; `min_copies`
    /// keeps a unit staged until that many tapes hold it.
    pub(crate) fn new(slice_size: &str, compression: Option<&str>, min_copies: i32) -> Home {
        let tmp = TempDir::new().unwrap();
        let paths = TapectlPaths::new(tmp.path().join("home"));
        paths.ensure_dirs().unwrap();
        let conn = crate::db::open(&paths.db_file).unwrap();
        let staging = tmp.path().join("staging");
        let sources = tmp.path().join("sources");
        fs::create_dir_all(&staging).unwrap();
        fs::create_dir_all(&sources).unwrap();

        let mut config = Config::default();
        config.staging.directory = staging.to_string_lossy().into_owned();
        config.defaults.slice_size = slice_size.to_string();
        config.defaults.min_copies = min_copies;
        if let Some(c) = compression {
            config.defaults.compression = c.to_string();
        }
        config.backends.lto.push(LtoBackendConfig {
            name: BACKEND.into(),
            device_tape: DEVICE.into(),
            device_sg: DEVICE_SG.into(),
            generation: "LTO-6".into(),
            capacity_override: None,
            fill_ceiling: crate::config::DEFAULT_FILL_CEILING,
            enospc_buffer: "0".into(),
            post_warning_bytes: Some("64M".into()),
        });

        crate::tenant::add_tenant(&conn, &paths, OPERATOR, None, true).unwrap();
        for tenant in [TENANT, TENANT_TWO] {
            crate::tenant::add_tenant(&conn, &paths, tenant, None, false).unwrap();
        }
        crate::cli::key::create_escrow_recipient(&conn, &paths, None).unwrap();
        Home {
            conn,
            paths,
            config,
            sources,
            tapes: BTreeMap::new(),
            _tmp: tmp,
        }
    }

    /// `unit init` of a folder made of `items`, for `tenant`.
    pub(crate) fn unit(&mut self, tenant: &str, name: &str, items: &[Item]) {
        let dir = self.sources.join(name);
        fs::create_dir_all(&dir).unwrap();
        for item in items {
            match item {
                Item::File(path, bytes) => {
                    let at = dir.join(path);
                    fs::create_dir_all(at.parent().unwrap()).unwrap();
                    fs::write(at, bytes).unwrap();
                }
                Item::Dir(path) => fs::create_dir_all(dir.join(path)).unwrap(),
                Item::Symlink(path, target) => {
                    let at = dir.join(path);
                    fs::create_dir_all(at.parent().unwrap()).unwrap();
                    std::os::unix::fs::symlink(target, at).unwrap();
                }
                Item::Hard(path, target) => {
                    let at = dir.join(path);
                    fs::create_dir_all(at.parent().unwrap()).unwrap();
                    fs::hard_link(dir.join(target), at).unwrap();
                }
                Item::FileB(path, bytes) => {
                    let at = dir.join(os(path));
                    fs::create_dir_all(at.parent().unwrap()).unwrap();
                    fs::write(at, bytes).unwrap();
                }
                Item::DirB(path) => fs::create_dir_all(dir.join(os(path))).unwrap(),
                Item::SymlinkB(path, target) => {
                    let at = dir.join(os(path));
                    fs::create_dir_all(at.parent().unwrap()).unwrap();
                    std::os::unix::fs::symlink(os(target), at).unwrap();
                }
                Item::HardB(path, target) => {
                    let at = dir.join(os(path));
                    fs::create_dir_all(at.parent().unwrap()).unwrap();
                    fs::hard_link(dir.join(os(target)), at).unwrap();
                }
            }
        }
        crate::unit::init_unit_with_config(
            &self.conn,
            &self.config,
            dir.to_str().unwrap(),
            tenant,
            Some(name),
            &[],
            None,
        )
        .unwrap();
    }

    /// The unit's folder.
    pub(crate) fn source(&self, unit: &str) -> PathBuf {
        self.sources.join(unit)
    }

    /// `snapshot create` then `stage create` of `unit`: its stage set.
    pub(crate) fn stage(&self, unit: &str) -> i64 {
        let snapshot = crate::staging::snapshot_create(&self.conn, unit, &self.config).unwrap();
        crate::staging::stage_create(&self.conn, &self.paths, &self.config, snapshot, true)
            .unwrap_or_else(|e| panic!("stage create {unit}: {e}"))
    }

    /// `cartridge register` of a blank LTO-6 cartridge, then `volume init`
    /// of `label` on it.
    pub(crate) fn volume(&mut self, label: &str) {
        let barcode = format!("{label}-STK");
        crate::cli::cartridge::run(
            &self.conn,
            &self.config,
            &CartridgeCommands::Register {
                barcode: barcode.clone(),
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
        let mut medium = MemStore::new(BS);
        let mut slot = ContactSlot::empty();
        let r = write::volume_init_contacted(
            &self.conn,
            &self.config,
            label,
            DEVICE,
            BS,
            false,
            None,
            Some(&barcode),
            &mut slot,
            ContactStore::Injected(&mut medium),
        );
        slot.finish_result(r)
            .unwrap_or_else(|e| panic!("volume init {label}: {e}"));
        self.tapes.insert(label.to_string(), medium);
    }

    /// `volume write` of `label`: everything staged that needs a copy.
    pub(crate) fn write(&mut self, label: &str) {
        let mut medium = self.tapes.remove(label).expect("the volume was made");
        let mut slot = ContactSlot::empty();
        let r = write::volume_write_contacted(
            &self.conn,
            &self.paths,
            &self.config,
            label,
            DEVICE,
            BS,
            false,
            false,
            false,
            false,
            true,
            &mut slot,
            ContactStore::Injected(&mut medium),
            None,
        );
        let r = slot.finish_result(r);
        self.tapes.insert(label.to_string(), medium);
        r.unwrap_or_else(|e| panic!("volume write {label}: {e}"));
        crate::cli::volume::release_staging_after_write(&self.conn, &self.config, label, false);
    }

    /// The stage set of `unit`'s version `version`, the latest staged.
    pub(crate) fn stage_set(&self, unit: &str, version: i64) -> i64 {
        self.conn
            .query_row(
                "SELECT ss.id FROM stage_sets ss
                   JOIN snapshots s ON s.id = ss.snapshot_id
                   JOIN units u ON u.id = s.unit_id
                  WHERE u.name = ?1 AND s.version = ?2
                  ORDER BY ss.id DESC LIMIT 1",
                params![unit, version],
                |r| r.get(0),
            )
            .unwrap()
    }

    /// How many slices `unit`'s version 1 has.
    pub(crate) fn num_slices(&self, unit: &str) -> i64 {
        self.conn
            .query_row(
                "SELECT COUNT(*) FROM stage_slices WHERE stage_set_id = ?1",
                params![self.stage_set(unit, 1)],
                |r| r.get(0),
            )
            .unwrap()
    }

    /// The tape file of `unit`'s slice `n` (version 1, stage set `set` when
    /// given) on `label`.
    pub(crate) fn position(&self, label: &str, unit: &str, n: i64) -> usize {
        self.position_in(label, self.stage_set(unit, 1), n)
    }

    pub(crate) fn position_in(&self, label: &str, stage_set: i64, n: i64) -> usize {
        self.conn
            .query_row(
                "SELECT CAST(wp.position AS INTEGER) FROM write_positions wp
                   JOIN writes w ON w.id = wp.write_id
                   JOIN volumes v ON v.id = w.volume_id
                   JOIN stage_slices sl ON sl.id = wp.stage_slice_id
                  WHERE v.label = ?1 AND sl.stage_set_id = ?2 AND sl.slice_number = ?3
                    AND wp.status = 'written'",
                params![label, stage_set, n],
                |r| r.get::<_, i64>(0),
            )
            .unwrap_or_else(|e| panic!("{label} holds no slice {n} of stage set {stage_set}: {e}"))
            as usize
    }

    /// Flip every bit of the byte at `offset` of the ciphertext of the tape
    /// file `position` of `label`. Offsets past the age header land in a
    /// chunk's data.
    pub(crate) fn flip(&mut self, label: &str, position: usize, offset: usize) {
        let medium = self.tapes.get_mut(label).expect("a tape");
        medium.files[position][offset] ^= 0xFF;
    }

    /// The members of `unit`'s version 1 in the file table, by path.
    pub(crate) fn rows(&self, unit: &str) -> Vec<crate::volume::filetable::Row> {
        crate::db::stage_files::load(&self.conn, self.stage_set(unit, 1)).unwrap()
    }

    /// H1's scripted bay ([`TestBay`]) over copies of every medium, with
    /// `start` in the drive and an operator who answers by `script`.
    pub(crate) fn bay(&self, start: &str, script: &[Reply]) -> TestBay {
        TestBay {
            carts: self
                .tapes
                .iter()
                .map(|(label, m)| (label.clone(), Box::new(copy_of(m)) as Box<dyn Store>))
                .collect(),
            open: Some(start.to_string()),
            physical: Some(start.to_string()),
            attended: true,
            script: script.iter().copied().collect(),
            asked: Vec::new(),
            releases: 0,
        }
    }

    pub(crate) fn site(&self) -> ContactSite<'_> {
        ContactSite::new(
            &self.config,
            Operation::RestoreUnit,
            DEVICE,
            Medium::NoBackend,
        )
    }

    /// `restore unit` / `restore file` of `unit` v1 into a fresh directory
    /// through `bay`, `--from label`.
    pub(crate) fn restore(
        &self,
        unit: &str,
        what: What<'_>,
        from: &str,
        options: &RestoreOptions,
        bay: &mut TestBay,
    ) -> (Result<RestoreReport>, TempDir) {
        let dest = TempDir::new().unwrap();
        let dest_dir = dest.path().to_string_lossy().into_owned();
        let r = self.restore_into(unit, what, from, options, bay, &dest_dir);
        (r, dest)
    }

    pub(crate) fn restore_into(
        &self,
        unit: &str,
        what: What<'_>,
        from: &str,
        options: &RestoreOptions,
        bay: &mut TestBay,
        dest_dir: &str,
    ) -> Result<RestoreReport> {
        let target = match what {
            What::Unit => RestoreTarget::Unit { dest_dir },
            What::File(file_path) => RestoreTarget::File {
                file_path,
                dest_dir,
            },
        };
        super::restore_unit_from_bay(
            &self.conn,
            &self.paths,
            &self.config,
            unit,
            from,
            1,
            target,
            options,
            bay,
            self.site(),
        )
    }
}

/// What a restore is of: the whole unit, or one file of it.
#[derive(Clone, Copy)]
pub(crate) enum What<'a> {
    Unit,
    File(&'a str),
}

/// A medium's recorded files on a fresh store.
pub(crate) fn copy_of(m: &MemStore) -> MemStore {
    let mut copy = MemStore::new(BS);
    copy.files = m.files.clone();
    copy.syncs = m.syncs.clone();
    copy
}

/// What a folder holds, by path: a regular file by its hash, a link by its
/// target, a folder as such. The unit's own dotfile is in it, as it is in
/// the stream.
pub(crate) fn listing(root: &Path) -> BTreeMap<String, String> {
    fn walk(root: &Path, dir: &Path, out: &mut BTreeMap<String, String>) {
        let Ok(entries) = fs::read_dir(dir) else {
            return;
        };
        for e in entries.flatten() {
            let path = e.path();
            // The escaped form (§1.5): a name is bytes, never converted lossily.
            let name =
                crate::volume::index::escape_os(path.strip_prefix(root).unwrap().as_os_str())
                    .into_owned();
            if name.starts_with(".tapectl-restore-tmp") {
                continue;
            }
            let meta = fs::symlink_metadata(&path).unwrap();
            if meta.is_symlink() {
                out.insert(
                    name,
                    format!(
                        "link {}",
                        crate::volume::index::escape_os(fs::read_link(&path).unwrap().as_os_str())
                    ),
                );
            } else if meta.is_dir() {
                out.insert(name.clone(), "dir".into());
                walk(root, &path, out);
            } else {
                out.insert(
                    name,
                    format!("file {:x}", sha2::Sha256::digest(fs::read(&path).unwrap())),
                );
            }
        }
    }
    let mut out = BTreeMap::new();
    walk(root, root, &mut out);
    out
}

/// `listing` of `restored` equals the unit's source folder's, but for the
/// paths in `except` (which are in neither).
pub(crate) fn assert_restored(h: &Home, unit: &str, restored: &Path, except: &[&str]) {
    let mut want = listing(&h.source(unit));
    let mut got = listing(restored);
    for e in except {
        want.remove(*e);
        got.remove(*e);
    }
    assert_eq!(got, want, "{unit} is not restored as it was");
}
