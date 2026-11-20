//! T5.2 (#550; ADR-0012 items 147-160 and 257): two places for the units,
//! and the per-location writes, on tar stagings.
//!
//! Everything is real but the drive, as in `volume::span_proofs`: tenants and
//! the escrow recipient are made as `init` and `tenant add` make them, each
//! unit is a folder registered by `unit init`, `snapshot create` and `stage
//! create` run the real tar and age, each volume is made by the real `volume
//! init` over a [`MemStore`], and each tape is written by the real `volume
//! write` ([`write::volume_write_contacted`], the store injected) followed by
//! the release the CLI runs after it. The shelves are `location add`'s and
//! `volume move`'s. The only rows written by hand are a tape-only unit's
//! status (where `unit mark-tape-only` needs a source that is gone).

use std::fs;
use std::path::PathBuf;

use rusqlite::{params, Connection};
use tempfile::TempDir;

use super::*;
use crate::cli::cartridge::CartridgeCommands;
use crate::cli::location::LocationCommands;
use crate::cli::volume::{StagingKeep, StagingRelease};
use crate::config::LtoBackendConfig;
use crate::store::MemStore;
use crate::tape::contact::ContactSlot;
use crate::volume::write::{self, ContactStore, StagedScope, WriteScope};

const BS: usize = 4096;
const DEVICE: &str = "/nonexistent/tapectl-two-places-nst";
const DEVICE_SG: &str = "/nonexistent/tapectl-two-places-sg";

/// Deterministic incompressible bytes, so tar's output is the input's size.
fn noise(len: usize, seed: u32) -> Vec<u8> {
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

struct Home {
    conn: Connection,
    paths: TapectlPaths,
    config: Config,
    sources: PathBuf,
    tapes: std::collections::BTreeMap<String, MemStore>,
    _tmp: TempDir,
}

impl Home {
    /// A home with the shelves `rack` and `offsite`; `capacity` is the
    /// drive's `capacity_override`. `copies_at` is `[defaults]`'s.
    fn new(
        capacity: Option<&str>,
        slice_size: &str,
        copies_at: Option<&[(&str, i64)]>,
        min_locations: i32,
    ) -> Home {
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
        config.defaults.min_locations = min_locations;
        config.defaults.copies_at = copies_at
            .unwrap_or(&[])
            .iter()
            .map(|(k, v)| (k.to_string(), *v))
            .collect();
        config.compaction.tape_only_safety_multiplier = 1;
        config.backends.lto.push(LtoBackendConfig {
            name: "two".into(),
            device_tape: DEVICE.into(),
            device_sg: DEVICE_SG.into(),
            generation: "LTO-6".into(),
            capacity_override: capacity.map(str::to_string),
            fill_ceiling: crate::config::DEFAULT_FILL_CEILING,
            enospc_buffer: "0".into(),
            post_warning_bytes: Some("64M".into()),
        });
        crate::tenant::add_tenant(&conn, &paths, "op", None, true).unwrap();
        crate::tenant::add_tenant(&conn, &paths, "alice", None, false).unwrap();
        crate::cli::key::create_escrow_recipient(&conn, &paths, None).unwrap();
        for name in ["rack", "offsite"] {
            crate::cli::location::run(
                &conn,
                &LocationCommands::Add {
                    name: name.into(),
                    description: None,
                    kind: "shelf".into(),
                },
                false,
                false,
            )
            .unwrap();
        }
        Home {
            conn,
            paths,
            config,
            sources,
            tapes: Default::default(),
            _tmp: tmp,
        }
    }

    /// `unit init` of a folder of files of `sizes`, then `snapshot create` and
    /// `stage create`: its stage set.
    fn unit_staged(&self, name: &str, sizes: &[usize]) -> i64 {
        let dir = self.sources.join(name);
        fs::create_dir_all(&dir).unwrap();
        for (i, len) in sizes.iter().enumerate() {
            fs::write(
                dir.join(format!("{name}-{i:03}.bin")),
                noise(*len, i as u32 + 3),
            )
            .unwrap();
        }
        crate::unit::init_unit_with_config(
            &self.conn,
            &self.config,
            dir.to_str().unwrap(),
            "alice",
            Some(name),
            &[],
            None,
        )
        .unwrap();
        let snapshot = crate::staging::snapshot_create(&self.conn, name, &self.config).unwrap();
        crate::staging::stage_create(&self.conn, &self.paths, &self.config, snapshot, true).unwrap()
    }

    /// `cartridge register`, then `volume init` of `label`.
    fn volume(&mut self, label: &str) {
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

    /// `volume move`.
    fn shelve(&self, label: &str, location: &str) {
        crate::cli::location::move_volume(&self.conn, label, location, false).unwrap();
    }

    /// `volume write LABEL` (with `scope`), the drive's early warning after
    /// `warn_at` bytes when given, then the release the CLI runs after it
    /// under `keep`.
    fn write(
        &mut self,
        label: &str,
        scope: impl Into<WriteScope>,
        warn_at: Option<u64>,
        keep: StagingKeep,
    ) -> StagingRelease {
        let mut medium = self.tapes.remove(label).expect("the volume was made");
        if let Some(at) = warn_at {
            let mut armed = MemStore::new(BS).with_early_warning_after(at);
            armed.files = medium.files;
            armed.syncs = medium.syncs;
            medium = armed;
        }
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
            scope,
        );
        let r = slot.finish_result(r);
        self.tapes.insert(label.to_string(), medium);
        r.unwrap_or_else(|e| panic!("volume write {label}: {e}"));
        self.release(label, keep)
    }

    fn release(&self, label: &str, keep: StagingKeep) -> StagingRelease {
        crate::cli::volume::release_staging_after_write(&self.conn, &self.config, label, keep)
    }

    /// The statuses of `unit`'s stage sets, in id order.
    fn set_statuses(&self, unit: &str) -> Vec<String> {
        self.conn
            .prepare(
                "SELECT ss.status FROM stage_sets ss
                   JOIN snapshots sn ON sn.id = ss.snapshot_id
                   JOIN units u ON u.id = sn.unit_id
                  WHERE u.name = ?1 ORDER BY ss.id",
            )
            .unwrap()
            .query_map(params![unit], |r| r.get(0))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap()
    }

    /// The staged slice files of `unit` still on the staging device.
    fn staged_files(&self, unit: &str) -> i64 {
        self.conn
            .query_row(
                "SELECT COUNT(*) FROM stage_slices sl
                   JOIN stage_sets ss ON ss.id = sl.stage_set_id
                   JOIN snapshots sn ON sn.id = ss.snapshot_id
                   JOIN units u ON u.id = sn.unit_id
                  WHERE u.name = ?1 AND sl.staging_path IS NOT NULL",
                params![unit],
                |r| r.get(0),
            )
            .unwrap()
    }

    /// What `audit` fires for the home: `(severity, check, unit)`.
    fn audit(&self) -> Vec<(&'static str, String, String)> {
        crate::cli::audit::fired_for_test(&self.conn, &self.config).unwrap()
    }

    /// `audit`'s exit code, as the command returns it.
    fn audit_exit(&self) -> i32 {
        crate::cli::audit::run(&self.conn, &self.config, None, false, true).unwrap()
    }
}

/// The findings about copies and places, whatever else `audit` says of a
/// fixture home (no escrow kit, no full verify: warnings that are not this
/// proof's).
fn about_copies(fired: Vec<(&'static str, String, String)>) -> Vec<(&'static str, String, String)> {
    fired
        .into_iter()
        .filter(|(_, c, _)| {
            matches!(
                c.as_str(),
                "copy_count" | "location_presence" | "location_count"
            )
        })
        .collect()
}

fn has(fired: &[(&'static str, String, String)], severity: &str, check: &str, unit: &str) -> bool {
    fired
        .iter()
        .any(|(s, c, u)| *s == severity && c == check && u == unit)
}

/// The two-place configuration of item 257.
const TWO_PLACES: &[(&str, i64)] = &[("rack", 1), ("offsite", 1)];

/// Both copies of an active unit on the shelf `rack`, `[defaults] copies_at`
/// asking for `rack` and `offsite`: `audit` fires `location_presence` as a
/// violation (exit 2) and `report needs --location offsite` lists the unit in
/// item 158's group 2; one `volume move` of a copy to `offsite` makes `audit`
/// clean (exit 0) and the unit drop out of the report. Returns what the
/// release said after the second copy, under `keep`, and the staging left
/// then and after the move.
fn both_copies_on_one_shelf(keep: StagingKeep) -> (StagingRelease, i64, StagingRelease, i64) {
    let mut h = Home::new(None, "1M", Some(TWO_PLACES), 2);
    h.unit_staged("ledger", &[40_000]);
    h.volume("TWO-A");
    h.volume("TWO-B");
    h.shelve("TWO-A", "rack");
    h.shelve("TWO-B", "rack");

    // Copy one: short of the floor (the larger of min_copies 2 and the
    // targets' sum 2), so its staging is kept whatever the hold.
    let first = h.write("TWO-A", StagedScope::Every, None, keep);
    assert!(
        matches!(first, StagingRelease::Retained { .. }),
        "one of two copies keeps its staging: {first:?}"
    );
    assert_eq!(h.set_statuses("ledger"), ["staged"]);

    // Copy two, on the same shelf.
    let second = h.write("TWO-B", StagedScope::Every, None, keep);
    let staged_after_both = h.staged_files("ledger");

    // The unit has two copies, in one place: `audit` is not clean.
    let fired = h.audit();
    assert!(
        has(&fired, "violation", "location_presence", "ledger"),
        "{fired:?}"
    );
    assert!(
        !fired.iter().any(|(_, c, _)| c == "copy_count"),
        "the copy floor is met: only the place is short: {fired:?}"
    );
    assert_eq!(h.audit_exit(), 2, "a violation");

    // `report needs --location offsite` lists it and says why.
    let rows = crate::cli::report::needs_rows(&h.conn, &h.config, Some("offsite"), false).unwrap();
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert_eq!(rows[0].unit, "ledger");
    assert_ne!(
        rows[0].need,
        Some(crate::policy::coverage::NeedAt::None),
        "{rows:?}"
    );

    // A copy goes to the other place: clean.
    h.shelve("TWO-B", "offsite");
    assert_eq!(about_copies(h.audit()), Vec::new());
    assert_eq!(
        h.audit_exit(),
        1,
        "warnings only (the fixture has no escrow kit or full verify): no violation"
    );
    let rows = crate::cli::report::needs_rows(&h.conn, &h.config, Some("offsite"), false).unwrap();
    assert!(
        rows.iter().all(|r| r
            .need
            .as_ref()
            .is_none_or(|n| *n == crate::policy::coverage::NeedAt::None)),
        "{rows:?}"
    );
    // The release after the move: what `volume write` runs after its
    // `--move-to`, here asked again.
    let after_move = h.release("TWO-B", keep);
    (
        second,
        staged_after_both,
        after_move,
        h.staged_files("ledger"),
    )
}

/// The default hold (item 154, item 217) is the copy floor: with two copies
/// on one shelf the floor of 2 is met and the staging goes, though `audit`
/// finds the place short. The proof of #550's second criterion, which asks
/// for "when staging is released".
#[test]
fn two_copies_on_one_shelf_fail_audit_and_the_default_hold_releases_staging_at_the_floor() {
    let (second, left, _, _) = both_copies_on_one_shelf(StagingKeep::Release);
    assert!(
        matches!(second, StagingRelease::Released(_)),
        "the floor is met: {second:?}"
    );
    assert_eq!(left, 0, "released, both copies on one shelf");
}

/// `--keep-staging-for-locations` holds the staging until each target is met:
/// kept with both copies on `rack`, released once a copy is on `offsite`.
#[test]
fn keep_staging_for_locations_waits_for_the_second_place() {
    let (second, left, after_move, left_after) =
        both_copies_on_one_shelf(StagingKeep::UntilEveryLocation);
    match second {
        StagingRelease::Retained { units, .. } => {
            assert_eq!(units.len(), 1);
            assert_eq!(units[0].unit_name, "ledger");
            assert_eq!(
                units[0]
                    .short_at
                    .iter()
                    .map(|s| s.location.as_str())
                    .collect::<Vec<_>>(),
                ["offsite"],
                "{units:?}"
            );
        }
        other => panic!("kept for offsite: {other:?}"),
    }
    assert!(left > 0, "staging kept while offsite holds no copy");
    assert!(
        matches!(after_move, StagingRelease::Released(_)),
        "{after_move:?}"
    );
    assert_eq!(left_after, 0, "released once both places hold a copy");
}

/// `min_locations` is unchanged (item 46, the 2026-09-29 rename; #550's
/// third criterion): with no `copies_at`, an active unit with both copies on
/// one shelf is clean, and the same unit tape-only is not (`location_count`).
#[test]
fn min_locations_binds_a_tape_only_unit_and_never_an_active_one() {
    let mut h = Home::new(None, "1M", None, 2);
    h.unit_staged("ledger", &[40_000]);
    h.volume("MIN-A");
    h.volume("MIN-B");
    h.shelve("MIN-A", "rack");
    h.shelve("MIN-B", "rack");
    h.write("MIN-A", StagedScope::Every, None, StagingKeep::Release);
    h.write("MIN-B", StagedScope::Every, None, StagingKeep::Release);

    assert_eq!(
        about_copies(h.audit()),
        Vec::new(),
        "active: two copies, one shelf, nothing about copies or places"
    );
    assert_eq!(
        h.audit_exit(),
        1,
        "warnings only (the fixture has no escrow kit or full verify): no violation"
    );

    // Positive control: the same catalog, the unit tape-only (its source is
    // not needed; `unit mark-tape-only` would write exactly this status).
    h.conn
        .execute(
            "UPDATE units SET status = 'tape_only' WHERE name = 'ledger'",
            [],
        )
        .unwrap();
    let fired = h.audit();
    assert!(
        has(&fired, "violation", "location_count", "ledger"),
        "{fired:?}"
    );
    assert_eq!(h.audit_exit(), 2);

    // And a copy at the second place satisfies it.
    h.shelve("MIN-B", "offsite");
    assert_eq!(about_copies(h.audit()), Vec::new());
}

/// `stage for-location` and `volume write --for-location` on a tar staging:
/// a unit with one copy at `rack` and its staging kept is staged for
/// `offsite` as it is (never again), and the write for `offsite` takes
/// exactly that set; after the write moves the tape there the unit is clean.
#[test]
fn stage_for_location_and_a_write_for_location_on_a_tar_staging() {
    let mut h = Home::new(None, "1M", Some(TWO_PLACES), 2);
    let set = h.unit_staged("ledger", &[40_000]);
    h.unit_staged("quiet", &[30_000]);
    h.volume("FOR-A");
    h.volume("FOR-B");
    h.shelve("FOR-A", "rack");
    // `ledger` goes to FOR-A alone: the write takes every staged set, so
    // `quiet` rides too, and both stay staged (1 of floor 2).
    h.write("FOR-A", StagedScope::Every, None, StagingKeep::Release);
    assert_eq!(h.set_statuses("ledger"), ["staged"]);

    // A third unit, new: staged for offsite from its source.
    let plan = super::plan(&h.conn, &h.config, "offsite", "FOR-B", DEVICE).unwrap();
    let planned: Vec<String> = plan.planned.iter().map(|c| c.unit.clone()).collect();
    assert!(planned.contains(&"ledger".to_string()), "{planned:?}");
    let staged = stage(&h.conn, &h.paths, &h.config, &plan, 1, true).unwrap();
    assert!(
        staged.iter().all(|s| *s == Staged::Already),
        "both are staged already, so nothing is staged again: {staged:?}"
    );
    let for_x = staged_sets_for_location(&h.conn, &h.config, "offsite").unwrap();
    assert!(for_x.stage_set_ids().contains(&set), "{for_x:?}");

    let scope = WriteScope {
        staged: StagedScope::ForLocation(for_x.stage_set_ids()),
        moved_to: Some("offsite".into()),
        for_location: Some("offsite".into()),
        keep_staging: false,
        keep_staging_for_locations: true,
    };
    let release = h.write("FOR-B", scope, None, StagingKeep::UntilEveryLocation);
    // The tape is still on no shelf: the move after the write is the CLI's.
    assert!(
        matches!(release, StagingRelease::Retained { .. }),
        "no copy is at offsite until the move: {release:?}"
    );
    h.shelve("FOR-B", "offsite");
    assert_eq!(about_copies(h.audit()), Vec::new());
    assert!(matches!(
        h.release("FOR-B", StagingKeep::UntilEveryLocation),
        StagingRelease::Released(_)
    ));
    assert_eq!(h.staged_files("ledger"), 0);
}

/// A version whose slices lie on two cartridges (a short seal, then the
/// continuation) of a tape-only unit: `stage for-location offsite` plans one
/// `volume read-slices` for each cartridge, each for the slices that cartridge
/// holds, and no cartridge holds them all. This is the read-back the plan
/// asks for in the case After T5.1 makes ordinary.
#[test]
fn a_version_on_two_cartridges_is_read_back_from_each_cartridge_in_turn() {
    // About 3.1 MB of files, 128 KiB slices, on 2.4 MB cartridges whose early
    // warning is at the nominal capacity: the first tape ends inside the unit
    // and the second takes the rest (item 288).
    const CAP: u64 = 2_465_792 + 8 * BS as u64;
    let mut h = Home::new(
        Some(&CAP.to_string()),
        "128K",
        // One copy is the floor, so the span is written once over two
        // tapes (a floor of two would write the whole set to each).
        Some(&[("offsite", 1)]),
        2,
    );
    h.config.defaults.min_copies = 1;
    h.unit_staged("reel", &[775_000; 4]);
    for label in ["SPAN-1", "SPAN-2", "SPAN-3"] {
        h.volume(label);
    }
    h.shelve("SPAN-1", "rack");
    h.shelve("SPAN-2", "rack");
    h.write(
        "SPAN-1",
        StagedScope::Every,
        Some(CAP),
        StagingKeep::Release,
    );
    h.write(
        "SPAN-2",
        StagedScope::Every,
        Some(CAP),
        StagingKeep::Release,
    );

    let holders: Vec<(i64, String)> = {
        let set: i64 = h
            .conn
            .query_row("SELECT id FROM stage_sets", [], |r| r.get(0))
            .unwrap();
        crate::volume::slice_holders::holders_of_set(&h.conn, set)
            .unwrap()
            .into_iter()
            .map(|s| (s.slice_number, s.holder.label))
            .collect()
    };
    let mut labels: Vec<&str> = holders.iter().map(|(_, l)| l.as_str()).collect();
    labels.sort_unstable();
    labels.dedup();
    assert_eq!(
        labels,
        ["SPAN-1", "SPAN-2"],
        "no cartridge holds every slice: {holders:?}"
    );

    // The unit is tape-only (its source gone): it is read back from tape.
    h.conn
        .execute(
            "UPDATE units SET status = 'tape_only' WHERE name = 'reel'",
            [],
        )
        .unwrap();
    let plan = super::plan(&h.conn, &h.config, "offsite", "SPAN-3", DEVICE).unwrap();
    assert_eq!(plan.skipped, Vec::new(), "{:?}", plan.skipped);
    let commands = plan.read_slices();
    {
        let dbg: Vec<(String, String)> = h
            .conn
            .prepare("SELECT sn.status, u.status FROM snapshots sn JOIN units u ON u.id=sn.unit_id")
            .unwrap()
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        let w: Vec<(String, String)> = h
            .conn
            .prepare("SELECT v.label, w.status FROM writes w JOIN volumes v ON v.id=w.volume_id")
            .unwrap()
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        eprintln!(
            "DEBUG planned={:?} skipped={:?} statuses={dbg:?} writes={w:?} nholders={}",
            plan.planned,
            plan.skipped,
            holders.len()
        );
    }
    assert_eq!(
        commands.iter().map(|c| c.from.as_str()).collect::<Vec<_>>(),
        ["SPAN-1", "SPAN-2"],
        "one read-slices for each cartridge, in order"
    );
    let mut all: Vec<i64> = commands.iter().flat_map(|c| c.slices.clone()).collect();
    all.sort_unstable();
    let want: Vec<i64> = holders
        .iter()
        .map(|(n, _)| *n)
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter()
        .collect();
    assert_eq!(all, want, "together they read every slice, each once");
    assert!(commands.iter().all(|c| !c.slices.is_empty()));
    let staged = stage(&h.conn, &h.paths, &h.config, &plan, 1, true).unwrap();
    assert_eq!(staged, [Staged::ReadSlices]);

    // A cartridge that goes missing leaves a slice nothing holds: the unit is
    // skipped, and the words say the cartridges together lack it.
    h.conn
        .execute(
            "UPDATE volumes SET status = 'retired' WHERE label = 'SPAN-2'",
            [],
        )
        .unwrap();
    let plan = super::plan(&h.conn, &h.config, "offsite", "SPAN-3", DEVICE).unwrap();
    assert!(plan.read_slices().is_empty());
    let skipped = plan.skipped.first().expect("the unit is skipped");
    assert_eq!(skipped.unit, "reel");
    assert!(
        skipped.reason.contains("together") && skipped.reason.contains("v1"),
        "{}",
        skipped.reason
    );
}
