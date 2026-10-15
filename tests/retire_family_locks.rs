//! Issue #461: the retire family takes the volume's session lock.
//!
//! `volume retire`, `cartridge retire`, `cartridge mark-erased`,
//! `volume compact-finish` and `volume move` change a volume's status or
//! place. A live `volume write` / `resume` / `verify` on that volume records
//! the status itself when it finishes; before this, a `volume retire` during
//! an hours-long `--full-confirm` moved the volume to `retired` and the
//! confirm then set it back to `sealed`. Each command now refuses, without
//! waiting and with nothing changed, while another process holds the lock,
//! and exits 75 (the "retry later" code).
//!
//! The holder here is a second connection's `acquire_volume`: a `flock` on a
//! second open file description conflicts with the command's even inside one
//! process, exactly as a second process's would.

use rusqlite::{params, Connection};
use tapectl::cli::operations::{cartridge_mark_erased, cartridge_retire, volume_retire};
use tapectl::cli::volume::VolumeCommands;
use tapectl::config::{Config, TapectlPaths};
use tapectl::db;
use tapectl::error::{ErrorContract, TapectlError, EXIT_CATALOG_BUSY};
use tapectl::staging::lock;
use tapectl::volume::write::compact_finish;

const LABEL: &str = "L6-BUSY";
const BARCODE: &str = "BC-BUSY";

struct Fixture {
    _dir: tempfile::TempDir,
    conn: Connection,
    holder: Connection,
    volume_id: i64,
}

fn fixture(volume_status: &str, cartridge_status: &str) -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("tapectl.db");
    let conn = db::open(&path).unwrap();
    let holder = db::open(&path).unwrap();
    conn.execute(
        "INSERT INTO volumes (label, backend_type, backend_name, media_type, capacity_bytes, status)
         VALUES (?1, 'lto', 'lto0', 'LTO-6', 2500000000000, ?2)",
        params![LABEL, volume_status],
    )
    .unwrap();
    let volume_id = conn.last_insert_rowid();
    conn.execute(
        "INSERT INTO cartridges (barcode, media_type, nominal_capacity, status)
         VALUES (?1, 'LTO-6', 2500000000000, ?2)",
        params![BARCODE, cartridge_status],
    )
    .unwrap();
    let cartridge_id = conn.last_insert_rowid();
    conn.execute(
        "INSERT INTO cartridge_volumes (cartridge_id, volume_id, identity_source)
         VALUES (?1, ?2, 'mam')",
        params![cartridge_id, volume_id],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO locations (name, kind) VALUES ('home', 'shelf')",
        [],
    )
    .unwrap();
    Fixture {
        _dir: dir,
        conn,
        holder,
        volume_id,
    }
}

impl Fixture {
    fn hold(&self) -> lock::VolumeLock {
        lock::acquire_volume(&self.holder, self.volume_id, LABEL).unwrap()
    }

    fn volume_status(&self) -> String {
        self.conn
            .query_row(
                "SELECT status FROM volumes WHERE id = ?1",
                [self.volume_id],
                |r| r.get(0),
            )
            .unwrap()
    }

    fn cartridge_status(&self) -> String {
        self.conn
            .query_row(
                "SELECT status FROM cartridges WHERE barcode = ?1",
                [BARCODE],
                |r| r.get(0),
            )
            .unwrap()
    }

    fn location(&self) -> Option<i64> {
        self.conn
            .query_row(
                "SELECT location_id FROM volumes WHERE id = ?1",
                [self.volume_id],
                |r| r.get(0),
            )
            .unwrap()
    }

    fn events(&self) -> i64 {
        self.conn
            .query_row("SELECT COUNT(*) FROM events", [], |r| r.get(0))
            .unwrap()
    }
}

/// The refusal every command makes: `VolumeBusy`, exit 75, the holder named.
fn assert_busy(err: TapectlError, action_part: &str) {
    let text = err.to_string();
    assert!(
        matches!(err, TapectlError::VolumeBusy { .. }),
        "a live session's lock is a busy refusal, not another error: {text}"
    );
    assert!(text.contains(LABEL), "{text}");
    assert!(text.contains(action_part), "names what was refused: {text}");
    assert!(
        text.contains("Nothing was changed"),
        "says nothing changed: {text}"
    );
    // The kernel names the holder; here that is this very process.
    assert!(
        text.contains(&format!("pid {}", std::process::id())),
        "names the session holding the lock: {text}"
    );
    let code = ErrorContract::Ordinary.exit_code(&anyhow::Error::from(err));
    assert_eq!(code, EXIT_CATALOG_BUSY, "retry later, not a verdict");
    assert_eq!(code, 75);
}

#[test]
fn volume_retire_is_refused_while_a_session_holds_the_volume_and_changes_nothing() {
    let f = fixture("initialized", "in_use");
    let held = f.hold();
    let events_before = f.events();

    let err = volume_retire(&f.conn, &Config::default(), LABEL, true, false, false)
        .expect_err("a retire under a live session must be refused");
    assert_busy(err, "retire volume");
    assert_eq!(f.volume_status(), "initialized", "the volume is untouched");
    assert_eq!(f.cartridge_status(), "in_use", "the cartridge is untouched");
    assert_eq!(f.events(), events_before, "no audit row was written");

    // A dry run changes nothing and is not refused.
    volume_retire(&f.conn, &Config::default(), LABEL, true, true, false)
        .expect("a dry run takes no lock");

    // Positive control: with the session gone, the same call retires it.
    drop(held);
    volume_retire(&f.conn, &Config::default(), LABEL, true, false, false)
        .expect("a free lock does not refuse");
    assert_eq!(f.volume_status(), "retired");
}

#[test]
fn cartridge_retire_is_refused_while_a_session_holds_a_volume_on_it() {
    let f = fixture("initialized", "in_use");
    let held = f.hold();
    let events_before = f.events();

    let err = cartridge_retire(
        &f.conn,
        &Config::default(),
        BARCODE,
        None,
        true,
        true,
        false,
        false,
    )
    .expect_err("retiring a cartridge under a live session must be refused");
    assert_busy(err, "retire cartridge");
    assert_eq!(f.cartridge_status(), "in_use");
    assert_eq!(f.volume_status(), "initialized");
    assert_eq!(f.events(), events_before);

    drop(held);
    cartridge_retire(
        &f.conn,
        &Config::default(),
        BARCODE,
        None,
        true,
        true,
        false,
        false,
    )
    .expect("a free lock does not refuse");
    assert_eq!(f.cartridge_status(), "retired_permanent");
    assert_eq!(f.volume_status(), "retired");
}

#[test]
fn cartridge_mark_erased_is_refused_while_a_session_holds_a_volume_on_it() {
    let f = fixture("retired", "pending_erase");
    // A `retired` volume with a session on it cannot happen through the
    // commands, but the lock, not the status, is what this refuses on; a
    // `sealed`/`initialized` volume with a held lock is the same refusal.
    let held = f.hold();
    let events_before = f.events();

    let err = cartridge_mark_erased(&f.conn, BARCODE, true, true, false, false)
        .expect_err("marking a cartridge erased under a live session must be refused");
    assert_busy(err, "mark cartridge");
    assert_eq!(f.cartridge_status(), "pending_erase");
    assert_eq!(f.volume_status(), "retired");
    assert_eq!(f.events(), events_before);

    drop(held);
    cartridge_mark_erased(&f.conn, BARCODE, true, true, false, false)
        .expect("a free lock does not refuse");
    assert_eq!(f.cartridge_status(), "available");
    assert_eq!(f.volume_status(), "erased");
}

#[test]
fn compact_finish_is_refused_while_a_session_holds_the_source() {
    let f = fixture("sealed", "in_use");
    let held = f.hold();
    let events_before = f.events();

    let err = compact_finish(&f.conn, &Config::default(), LABEL, true)
        .expect_err("compact-finish under a live session must be refused");
    assert_busy(err, "compact-finish");
    assert_eq!(f.volume_status(), "sealed");
    assert_eq!(f.cartridge_status(), "in_use");
    assert_eq!(f.events(), events_before);

    drop(held);
    compact_finish(&f.conn, &Config::default(), LABEL, true).expect("a free lock does not refuse");
    assert_eq!(f.volume_status(), "retired");
}

#[test]
fn volume_move_is_refused_while_a_session_holds_the_volume() {
    let f = fixture("initialized", "in_use");
    let held = f.hold();
    let home = f._dir.path().join("home");
    let paths = TapectlPaths {
        home: home.clone(),
        config_file: home.join("config.toml"),
        db_file: f._dir.path().join("tapectl.db"),
        keys_dir: home.join("keys"),
        catalogs_dir: home.join("catalogs"),
        stage_reports_dir: home.join("stage-reports"),
        logs_dir: home.join("logs"),
    };
    let command = VolumeCommands::Move {
        label: LABEL.to_string(),
        to: "home".to_string(),
    };
    let events_before = f.events();

    let err = tapectl::cli::volume::run(
        &f.conn,
        &paths,
        &Config::default(),
        &command,
        false,
        true,
        false,
    )
    .expect_err("moving a volume under a live session must be refused");
    assert_busy(err, "move volume");
    assert_eq!(f.location(), None, "the volume did not move");
    assert_eq!(f.events(), events_before);

    // A dry run is not refused and moves nothing.
    tapectl::cli::volume::run(
        &f.conn,
        &paths,
        &Config::default(),
        &command,
        false,
        true,
        true,
    )
    .expect("a dry run takes no lock");
    assert_eq!(f.location(), None);

    drop(held);
    tapectl::cli::volume::run(
        &f.conn,
        &paths,
        &Config::default(),
        &command,
        false,
        true,
        false,
    )
    .expect("a free lock does not refuse");
    assert!(f.location().is_some(), "now it moved");
}
