/// Integration coverage for `collection sync --dry-run` against a real,
/// generated microcosm collection — `docs/design/v2-implementation-plan.md`
/// T10's test list: "`collection sync --dry-run` against a small generated
/// microcosm collection (use `tests/common/mod.rs`) reports sensibly: new
/// units detected, nothing mutated in dry-run mode."
///
/// The selector's own multi-tape drill (src/collection/selector.rs) is the
/// place synthetic ~600-unit size lists exercise the packing arithmetic at
/// scale; this file's job is the opposite end of the spec — a small, real
/// fixture tree proving the sync/CLI plumbing around it.
mod common;

use clap::Parser;

use common::{generate_collection, MicroSpec};
use tapectl::cli::collection::CollectionCommands;
use tapectl::cli::{Cli, Commands};
use tapectl::collection::sync::sync_collection;
use tapectl::config::{CollectionConfig, TapectlPaths};

#[test]
fn collection_sync_dry_run_against_a_generated_microcosm_collection_reports_sensibly() {
    let home = tempfile::tempdir().unwrap();
    let conn = tapectl::db::open(&home.path().join("tapectl.db")).unwrap();
    conn.execute(
        "INSERT INTO tenants (name, is_operator, status) VALUES ('media', 0, 'active')",
        [],
    )
    .unwrap();

    let root = tempfile::tempdir().unwrap();

    // Small N on purpose: generate_collection tiles real MiB-scale file
    // content per unit (2-15 M each per the microcosm spec), so this stays
    // a plumbing check, not a scale drill — the selector's own test
    // exercises ~600 units, but via synthetic sizes, never real fixtures.
    let spec = MicroSpec {
        n_units: 3,
        seed: 7,
    };
    let fixtures = generate_collection(root.path(), &spec);
    assert_eq!(fixtures.len(), 3);
    for fixture in &fixtures {
        // Sanity-check the generator actually produced microcosm-shaped
        // units (2-15 MiB, one dominant file plus sidecars) before trusting
        // sync's report about them.
        assert!(
            (2 * 1024 * 1024..15 * 1024 * 1024).contains(&fixture.total_bytes),
            "unit {} size {} out of the documented [2 MiB, 15 MiB) range",
            fixture.folder_name,
            fixture.total_bytes
        );
        assert_eq!(fixture.files.len(), 3, "movie + cover + subtitle");
    }

    let lib = CollectionConfig {
        name: "microlib".to_string(),
        root: root.path().to_string_lossy().to_string(),
        tenant: "media".to_string(),
        unit_depth: 1,
        exclude: vec![],
        archive_set: None,
        dotfiles: true,
        snapshot_command: None,
        release_command: None,
    };
    let paths = TapectlPaths::new(home.path().to_path_buf());

    let (report, errors) = sync_collection(&conn, &paths, &lib, true, &[]).unwrap();

    assert!(errors.is_empty(), "unexpected errors: {errors:?}");
    assert_eq!(
        report.created, 3,
        "dry-run must detect all three generated units as new"
    );
    assert_eq!(report.moved, 0);
    assert_eq!(report.reactivated, 0);
    assert_eq!(report.missing, 0);

    // Nothing mutated: no unit rows, no dotfiles written into the
    // generated tree.
    let unit_count: i64 = conn
        .query_row("SELECT COUNT(*) FROM units", [], |r| r.get::<_, i64>(0))
        .unwrap();
    assert_eq!(unit_count, 0, "dry-run must not insert any unit row");
    for fixture in &fixtures {
        let dotfile = root
            .path()
            .join(&fixture.folder_name)
            .join(".tapectl-unit.toml");
        assert!(
            !dotfile.exists(),
            "dry-run must not write a dotfile into {}",
            fixture.folder_name
        );
    }
}

#[test]
fn collection_sync_dry_run_parses_through_the_real_cli() {
    // `CollectionCommands::Sync` declares its own `--dry-run`, distinct from
    // Cli's top-level `global = true` `--dry-run` — both fields happen to
    // share the name `dry_run` but live in different structs. Confirm this
    // parses cleanly and the subcommand-local flag is what actually gets
    // set, rather than inferring it from `cmd.build()` not panicking during
    // man-page generation.
    let cli = Cli::try_parse_from(["tapectl", "collection", "sync", "--dry-run"])
        .expect("`tapectl collection sync --dry-run` must parse");
    match cli.command {
        Commands::Collection { command } => match command {
            CollectionCommands::Sync { dry_run } => {
                assert!(dry_run, "the subcommand-local --dry-run must be set");
            }
            other => panic!("expected CollectionCommands::Sync, got {other:?}"),
        },
        other => panic!("expected Commands::Collection, got {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// ADR-0012 items 206, 207, 212 and 220, through the real binary.
// ---------------------------------------------------------------------------

use std::path::Path;
use std::process::{Command, Output};

fn tapectl(home: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_tapectl"))
        .args(args)
        .env("HOME", home)
        .env_remove("XDG_CONFIG_HOME")
        .env_remove("TAPECTL_HOME")
        .env_remove("TAPECTL_CONFIG")
        .output()
        .expect("failed to spawn tapectl binary")
}

fn shown(out: &Output) -> String {
    format!(
        "exit={:?}\nstdout={}\nstderr={}",
        out.status.code(),
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    )
}

fn stdout(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).into_owned()
}

/// Runs `args` and asserts the exit code.
fn exits(home: &Path, args: &[&str], code: i32) -> Output {
    let out = tapectl(home, args);
    assert_eq!(
        out.status.code(),
        Some(code),
        "`tapectl {}`\n{}",
        args.join(" "),
        shown(&out)
    );
    out
}

fn json(out: &Output) -> serde_json::Value {
    serde_json::from_slice(&out.stdout)
        .unwrap_or_else(|e| panic!("not one JSON document ({e}):\n{}", shown(out)))
}

fn config_text(home: &Path) -> String {
    std::fs::read_to_string(home.join(".tapectl/config.toml")).unwrap()
}

/// A home with tenant `media` and a root of `folders` (each holding one
/// file), the root canonical.
fn home_and_root(folders: &[&str]) -> (tempfile::TempDir, tempfile::TempDir, String) {
    let home = tempfile::tempdir().unwrap();
    exits(home.path(), &["init", "--no-escrow"], 0);
    exits(home.path(), &["tenant", "add", "media"], 0);
    let root = tempfile::tempdir().unwrap();
    for f in folders {
        std::fs::create_dir_all(root.path().join(f)).unwrap();
        std::fs::write(root.path().join(f).join("a.txt"), b"x").unwrap();
    }
    let canonical = root.path().canonicalize().unwrap().display().to_string();
    (home, root, canonical)
}

/// A collection table written by hand, as an older `config add` could:
/// `collection add` refuses each of the broken ones the tests need.
fn append_collection(home: &Path, name: &str, root: &str, tenant: &str) {
    let cfg = home.join(".tapectl/config.toml");
    let text = config_text(home);
    std::fs::write(
        cfg,
        format!("{text}\n[[collections]]\nname = \"{name}\"\nroot = \"{root}\"\ntenant = \"{tenant}\"\n"),
    )
    .unwrap();
}

/// `collection add` refuses, exit 2 and the file untouched, each thing
/// that makes a collection unusable (ADR-0012 item 206).
#[test]
fn collection_add_refuses_each_unusable_collection() {
    let (home, root, canonical) = home_and_root(&["alpha"]);
    let h = home.path();
    exits(
        h,
        &[
            "collection",
            "add",
            "photos",
            &canonical,
            "--tenant",
            "media",
        ],
        0,
    );
    let before = config_text(h);
    let elsewhere = tempfile::tempdir().unwrap();
    let elsewhere = elsewhere
        .path()
        .canonicalize()
        .unwrap()
        .display()
        .to_string();
    let inside = format!("{canonical}/alpha");
    let missing = format!("{elsewhere}/nope");
    let parent = root
        .path()
        .canonicalize()
        .unwrap()
        .parent()
        .unwrap()
        .display()
        .to_string();
    for (args, says) in [
        (
            vec!["my photos", &elsewhere, "--tenant", "media"],
            "invalid collection name \"my photos\"",
        ),
        (
            vec!["2024", &elsewhere, "--tenant", "media"],
            "reads as a position",
        ),
        (
            vec!["photos", &elsewhere, "--tenant", "media"],
            "already configured",
        ),
        (
            vec!["more", "relative/root", "--tenant", "media"],
            "is not an absolute path",
        ),
        (
            vec!["more", &missing, "--tenant", "media"],
            "does not exist",
        ),
        (
            vec!["more", &elsewhere, "--tenant", "medai"],
            "tenant \"medai\" does not exist",
        ),
        (
            vec![
                "more",
                &elsewhere,
                "--tenant",
                "media",
                "--archive-set",
                "cold",
            ],
            "unknown archive set \"cold\"",
        ),
        (
            vec!["more", &inside, "--tenant", "media"],
            "overlaps collection \"photos\"",
        ),
        (
            vec!["more", &parent, "--tenant", "media"],
            "overlaps collection \"photos\"",
        ),
    ] {
        let mut argv = vec!["collection", "add"];
        argv.extend(args.iter().copied());
        let out = exits(h, &argv, 2);
        assert!(
            String::from_utf8_lossy(&out.stderr).contains(says),
            "`{}` should say {says:?}\n{}",
            argv.join(" "),
            shown(&out)
        );
        assert_eq!(before, config_text(h), "`{}` wrote", argv.join(" "));
    }
}

/// `collection add` checks, as the user it runs as, that it can write each
/// unit folder, prints the `setfacl` line that grants it when it cannot,
/// and adds nothing; `--no-dotfiles` needs no write.
#[test]
fn collection_add_prints_the_setfacl_lines_it_cannot_run_itself() {
    use std::os::unix::fs::PermissionsExt;
    let (home, root, canonical) = home_and_root(&["alpha", "bravo"]);
    let h = home.path();
    let alpha = root.path().join("alpha");
    std::fs::set_permissions(&alpha, std::fs::Permissions::from_mode(0o555)).unwrap();
    if std::fs::write(alpha.join("probe"), b"x").is_ok() {
        eprintln!("SKIPPED: a read-only directory does not stop this user (root?)");
        return;
    }
    let before = config_text(h);
    let out = exits(
        h,
        &[
            "collection",
            "add",
            "photos",
            &canonical,
            "--tenant",
            "media",
        ],
        2,
    );
    let user = tapectl::unit::effective_user_name();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains(&format!(
            "sudo find {canonical} -mindepth 1 -maxdepth 1 -type d -exec setfacl -m u:{user}:rwX {{}} +"
        )),
        "{}",
        shown(&out)
    );
    assert!(
        stderr.contains(&format!("cannot write {canonical}/alpha")),
        "{stderr}"
    );
    assert!(stderr.contains("--no-dotfiles"), "{stderr}");
    assert_eq!(before, config_text(h));

    exits(
        h,
        &[
            "collection",
            "add",
            "photos",
            &canonical,
            "--tenant",
            "media",
            "--no-dotfiles",
        ],
        0,
    );
    assert!(config_text(h).contains("dotfiles = false"));
    std::fs::set_permissions(&alpha, std::fs::Permissions::from_mode(0o755)).unwrap();
}

/// `collection add` writes the table and shows the sync it would get; the
/// dry run it shows and the real `collection sync` agree on a folder whose
/// name is not a unit name (ADR-0012 item 220), which is a warning: exit 1.
#[test]
fn collection_add_previews_the_sync_the_real_one_agrees_with() {
    let (home, _root, canonical) = home_and_root(&["alpha", "bravo", "2022 trip"]);
    let h = home.path();
    let out = exits(
        h,
        &[
            "collection",
            "add",
            "photos",
            &canonical,
            "--tenant",
            "media",
        ],
        0,
    );
    let text = stdout(&out);
    assert!(text.contains("added collection \"photos\""), "{text}");
    assert!(
        text.contains("collection \"photos\" (dry run): would create 2; 1 refused"),
        "{text}"
    );
    assert!(text.contains("2 pending (2 never archived)"), "{text}");
    assert!(
        text.contains("tapectl config add 'collections[photos].exclude' '2022 trip'"),
        "{text}"
    );

    let dry = exits(h, &["collection", "sync", "--dry-run"], 1);
    assert!(
        stdout(&dry).contains("would create 2; 1 refused"),
        "{}",
        shown(&dry)
    );
    let real = exits(h, &["collection", "sync"], 1);
    assert!(
        stdout(&real).contains("collection \"photos\": 2 created; 1 refused"),
        "{}",
        shown(&real)
    );
    // Positive control for the exit: with the folder excluded, clean.
    exits(
        h,
        &["config", "add", "collections[photos].exclude", "2022 trip"],
        0,
    );
    let clean = exits(h, &["collection", "sync"], 0);
    assert!(
        stdout(&clean).contains("0 created; 0 refused"),
        "{}",
        shown(&clean)
    );
}

/// `collection remove` takes the table out and leaves the units: a later
/// sync of the collections still configured marks none of them missing,
/// though one of its folders is gone (the coordinator's OQ23).
#[test]
fn collection_remove_leaves_the_units_and_no_sync_marks_them_missing() {
    let (home, root, canonical) = home_and_root(&["alpha", "bravo"]);
    let h = home.path();
    let (_other, other_canonical) = {
        let other = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(other.path().join("x")).unwrap();
        let c = other.path().canonicalize().unwrap().display().to_string();
        (other, c)
    };
    exits(
        h,
        &[
            "collection",
            "add",
            "photos",
            &canonical,
            "--tenant",
            "media",
        ],
        0,
    );
    exits(
        h,
        &[
            "collection",
            "add",
            "other",
            &other_canonical,
            "--tenant",
            "media",
        ],
        0,
    );
    exits(h, &["collection", "sync"], 0);

    let out = exits(h, &["collection", "remove", "photos"], 0);
    assert!(
        stdout(&out).contains("its 2 units stay registered and untouched"),
        "{}",
        shown(&out)
    );
    assert!(!config_text(h).contains("name = \"photos\""));
    std::fs::remove_dir_all(root.path().join("alpha")).unwrap();
    exits(h, &["collection", "sync"], 0);

    let units = json(&exits(h, &["--json", "unit", "list"], 0));
    let status_of = |name: &str| {
        units
            .as_array()
            .unwrap()
            .iter()
            .find(|u| u["name"] == name)
            .unwrap_or_else(|| panic!("{name} is still registered: {units}"))["status"]
            .clone()
    };
    assert_eq!(status_of("photos/alpha"), "active", "never marked missing");
    assert_eq!(status_of("photos/bravo"), "active");
    // Positive control: a folder gone from a collection still configured is.
    std::fs::remove_dir_all(Path::new(&other_canonical).join("x")).unwrap();
    exits(h, &["collection", "sync"], 1);
}

/// One collection with a missing root and one with a misspelled tenant are
/// each that collection's error (exit 2); the good one is still shown and
/// synced, and neither broken one reads as clean (ADR-0012 item 220).
#[test]
fn status_survives_a_missing_root_and_flags_a_misspelled_tenant() {
    let (home, _root, canonical) = home_and_root(&["alpha"]);
    let h = home.path();
    exits(
        h,
        &[
            "collection",
            "add",
            "photos",
            &canonical,
            "--tenant",
            "media",
        ],
        0,
    );
    append_collection(h, "movies", "/nonexistent/tapectl-test/movies", "media");
    let typo_root = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(typo_root.path().join("x")).unwrap();
    let typo = typo_root
        .path()
        .canonicalize()
        .unwrap()
        .display()
        .to_string();
    append_collection(h, "tv", &typo, "medai");

    let out = exits(h, &["collection", "status"], 2);
    let text = stdout(&out);
    assert!(text.contains("collection \"photos\": 0 pending"), "{text}");
    assert!(
        text.contains("collection \"movies\": error: root \"/nonexistent/tapectl-test/movies\" does not exist"),
        "{text}"
    );
    assert!(
        text.contains("collection \"tv\": error: tenant \"medai\" does not exist"),
        "{text}"
    );

    let rows = json(&exits(h, &["--json", "collection", "status"], 2));
    let rows = rows.as_array().unwrap();
    assert_eq!(rows.len(), 3, "{rows:?}");
    assert_eq!(rows[0]["pending"], 0);
    assert!(
        rows[1]["pending"].is_null() && rows[2]["pending"].is_null(),
        "{rows:?}"
    );
    assert_eq!(rows[2]["errors"].as_array().unwrap().len(), 1, "{rows:?}");

    // Sync goes on to the good collection, and registers nothing for tv.
    let out = exits(h, &["collection", "sync"], 2);
    assert!(
        stdout(&out).contains("collection \"photos\": 1 created"),
        "{}",
        shown(&out)
    );
    let units = json(&exits(h, &["--json", "unit", "list"], 0));
    assert_eq!(units.as_array().unwrap().len(), 1, "{units}");
}

/// The JSON keys of ADR-0012 item 207, and the exit codes of item 212: a
/// pending unit is never archived or dirty, a staged one awaits a write,
/// and only a warning (here an entry outside every unit) makes it exit 1.
#[test]
fn status_reports_pending_as_the_glossary_says_and_staged_beside_it() {
    let (home, root, canonical) = home_and_root(&["alpha", "bravo", "charlie"]);
    let h = home.path();
    exits(
        h,
        &[
            "collection",
            "add",
            "photos",
            &canonical,
            "--tenant",
            "media",
        ],
        0,
    );
    exits(h, &["collection", "sync"], 0);
    // charlie: a version staged and owed its copies, as `stage create`
    // leaves it; its folder changed since, so it is dirty too.
    let conn = tapectl::db::open(&h.join(".tapectl/tapectl.db")).unwrap();
    conn.execute_batch(
        "INSERT INTO snapshots (unit_id, version, status, source_path, file_count, total_size)
             SELECT id, 1, 'current', current_path, 0, 0 FROM units WHERE name = 'photos/charlie';
         INSERT INTO stage_sets (snapshot_id, status, slice_size)
             SELECT id, 'staged', 1 FROM snapshots;",
    )
    .unwrap();
    drop(conn);

    let out = exits(h, &["collection", "status"], 0);
    assert!(
        stdout(&out).contains(
            "collection \"photos\": 3 pending (2 never archived, 1 dirty) · 1 staged, awaiting \
             a write · 3 under-copied"
        ),
        "{}",
        shown(&out)
    );
    let rows = json(&exits(h, &["--json", "collection", "status"], 0));
    let photos = &rows[0];
    for (key, n) in [
        ("pending", 3),
        ("never_archived", 2),
        ("dirty", 1),
        ("staged", 1),
        ("under_copied", 3),
        ("missing", 0),
    ] {
        assert_eq!(photos[key], n, "{key}: {photos}");
    }

    let rows = json(&exits(h, &["--json", "collection", "sync", "--dry-run"], 0));
    for (key, n) in [
        ("pending", 3),
        ("never_archived", 2),
        ("dirty", 1),
        ("staged", 1),
    ] {
        assert_eq!(rows[0][key], n, "sync's {key}: {}", rows[0]);
    }

    std::fs::write(root.path().join("loose.txt"), b"x").unwrap();
    let out = exits(h, &["collection", "status"], 1);
    assert!(
        stdout(&out).contains("tapectl config add 'collections[photos].exclude' PATTERN"),
        "{}",
        shown(&out)
    );
}

/// With no collection configured, `sync`, `status` and `plan` print the
/// one shape they print for several, an empty list, under --json, and
/// otherwise name the command that adds one (ADR-0012 items 206, 220).
#[test]
fn with_no_collection_each_command_prints_an_empty_list() {
    let home = tempfile::tempdir().unwrap();
    let h = home.path();
    exits(h, &["init", "--no-escrow"], 0);
    for command in ["sync", "status", "plan"] {
        let out = exits(h, &["--json", "collection", command], 0);
        assert_eq!(json(&out), serde_json::json!([]), "{command}");
        let out = exits(h, &["collection", command], 0);
        assert!(
            stdout(&out).contains("tapectl collection add NAME ROOT --tenant TENANT"),
            "{command}: {}",
            shown(&out)
        );
    }
}

/// Seeds one staged version of `unit` (a catalog row and no slice file),
/// as `stage create` leaves one: `volume plan` reads only the catalog.
fn seed_staged(home: &Path, unit: &str, bytes: i64) {
    let conn = tapectl::db::open(&home.join(".tapectl/tapectl.db")).unwrap();
    conn.execute(
        "INSERT INTO snapshots (unit_id, version, status, source_path, file_count, total_size)
             SELECT id, 1, 'current', current_path, 1, ?2 FROM units WHERE name = ?1",
        rusqlite::params![unit, bytes],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO stage_sets (snapshot_id, status, slice_size, num_slices, total_encrypted_size)
             SELECT s.id, 'staged', 524288, 1, ?2 FROM snapshots s
             JOIN units u ON u.id = s.unit_id WHERE u.name = ?1",
        rusqlite::params![unit, bytes],
    )
    .unwrap();
}

/// ADR-0012 item 214: planning needs no drive. A drive that is not
/// attached (its `device_tape` does not exist, so no `device_sg` can be
/// derived and a write refuses it) plans `volume plan` and `collection
/// plan` alike, by its device or as the only one, and nothing of it is
/// opened; with no drive at all `--generation` plans, and without it
/// `collection plan` names both ways on while `volume plan` shows the plan
/// with no estimate rather than printing it and failing.
#[test]
fn plans_need_no_drive_and_never_open_one() {
    // alpha staged (for `volume plan`), bravo never archived (a batch).
    let (home, _root, canonical) = home_and_root(&["alpha", "bravo"]);
    let h = home.path();
    exits(
        h,
        &[
            "collection",
            "add",
            "photos",
            &canonical,
            "--tenant",
            "media",
        ],
        0,
    );
    exits(h, &["collection", "sync"], 0);
    seed_staged(h, "photos/alpha", 4096);

    // No drive at all.
    let out = exits(h, &["collection", "plan"], 2);
    let text = shown(&out);
    assert!(text.contains("--generation"), "{text}");
    assert!(text.contains("tapectl drive add"), "{text}");
    assert!(text.contains(".tapectl/config.toml"), "{text}");
    let out = exits(h, &["collection", "plan", "--generation", "LTO-6"], 0);
    assert!(stdout(&out).contains("batch 0"), "{}", shown(&out));
    let out = exits(h, &["volume", "plan"], 0);
    assert!(
        stdout(&out).contains("estimated tapes: not estimated: no drive is configured"),
        "{}",
        shown(&out)
    );
    let out = exits(h, &["volume", "plan", "--generation", "LTO-6"], 0);
    assert!(
        stdout(&out).contains("estimated tapes: 1 (at the 97% fill ceiling)"),
        "{}",
        shown(&out)
    );

    // A drive that is not attached: its path does not exist.
    let nst = h.join("no-such-nst").display().to_string();
    let cfg = h.join(".tapectl/config.toml");
    let text = config_text(h);
    std::fs::write(
        &cfg,
        format!(
            "{text}\n[[backends.lto]]\nname = \"lto6\"\ndevice_tape = \"{nst}\"\n\
             generation = \"LTO-6\"\n"
        ),
    )
    .unwrap();
    for device in [None, Some(nst.as_str())] {
        let with = |args: &[&str]| -> Vec<String> {
            let mut v: Vec<String> = args.iter().map(|a| a.to_string()).collect();
            if let Some(d) = device {
                v.extend(["--device".to_string(), d.to_string()]);
            }
            v
        };
        let args = with(&["collection", "plan"]);
        let args: Vec<&str> = args.iter().map(String::as_str).collect();
        let out = exits(h, &args, 0);
        assert!(
            stdout(&out).contains("batch 0"),
            "{device:?}: {}",
            shown(&out)
        );
        let args = with(&["volume", "plan"]);
        let args: Vec<&str> = args.iter().map(String::as_str).collect();
        let out = exits(h, &args, 0);
        assert!(
            stdout(&out).contains("estimated tapes: 1 (at the 97% fill ceiling)"),
            "{device:?}: {}",
            shown(&out)
        );
    }
    assert!(
        !h.join("no-such-nst").exists(),
        "nothing created the device"
    );
}

/// ADR-0012 item 220 (OQ26): `volume plan` plans, by default, the largest
/// copy floor among the staged units it plans, and says whose; `--copies`
/// still sets the number, and says so.
#[test]
fn volume_plan_plans_the_largest_copy_floor_by_default() {
    let (home, root, canonical) = home_and_root(&["alpha", "bravo"]);
    let h = home.path();
    exits(
        h,
        &[
            "collection",
            "add",
            "photos",
            &canonical,
            "--tenant",
            "media",
        ],
        0,
    );
    exits(h, &["collection", "sync"], 0);
    exits(h, &["location", "add", "vault"], 0);
    seed_staged(h, "photos/alpha", 4096);
    seed_staged(h, "photos/bravo", 2048);
    let args = ["--json", "volume", "plan", "--generation", "LTO-6"];

    // Both at `[defaults] min_copies`, 2.
    let plan = json(&exits(h, &args, 0));
    assert_eq!(plan["copies"], 2, "{plan}");
    assert_eq!(plan["copies_from"], "copy floor", "{plan}");

    // bravo's policy asks three copies at the vault: its floor is 3, the
    // largest, though alpha is listed first (it is larger).
    let dotfile = root.path().join("bravo/.tapectl-unit.toml");
    let mut d = tapectl::unit::dotfile::read_dotfile(&dotfile).unwrap();
    d.copies_at = Some([("vault".to_string(), 3)].into_iter().collect());
    tapectl::unit::dotfile::write_dotfile(&dotfile, &d).unwrap();
    let plan = json(&exits(h, &args, 0));
    assert_eq!(plan["copies"], 3, "{plan}");
    assert_eq!(plan["copy_floor_unit"], "photos/bravo", "{plan}");
    let out = exits(h, &["volume", "plan", "--generation", "LTO-6"], 0);
    assert!(
        stdout(&out).contains(
            "volume write plan (3 copies: the copy floor of photos/bravo, the largest among \
             the staged units; --copies plans another number):"
        ),
        "{}",
        shown(&out)
    );

    let plan = json(&exits(
        h,
        &[
            "--json",
            "volume",
            "plan",
            "--copies",
            "5",
            "--generation",
            "LTO-6",
        ],
        0,
    ));
    assert_eq!(plan["copies"], 5, "{plan}");
    assert_eq!(plan["copies_from"], "--copies", "{plan}");
    assert_eq!(plan["copy_floor_unit"], serde_json::Value::Null, "{plan}");
    let out = exits(h, &["volume", "plan", "--copies", "1"], 0);
    assert!(
        stdout(&out).contains("volume write plan (1 copy, as --copies asks):"),
        "{}",
        shown(&out)
    );
}

/// ADR-0012 items 210 and 220, through the binary: `collection run` runs
/// the only collection without its name and refuses to guess among
/// several, naming them; a second `--label` is a usage error naming the
/// rule; a label not initialized names `--init`; and `--init --dry-run`
/// previews the batch it would write to the volume it would initialize,
/// touching nothing.
#[test]
fn collection_run_picks_the_only_collection_and_takes_one_label() {
    let (home, _root, canonical) = home_and_root(&["alpha"]);
    let h = home.path();
    exits(
        h,
        &[
            "collection",
            "add",
            "photos",
            &canonical,
            "--tenant",
            "media",
        ],
        0,
    );
    exits(h, &["collection", "sync"], 0);
    let nst = h.join("no-such-nst").display().to_string();
    let text = config_text(h);
    std::fs::write(
        h.join(".tapectl/config.toml"),
        format!(
            "{text}\n[[backends.lto]]\nname = \"lto6\"\ndevice_tape = \"{nst}\"\n\
             device_sg = \"{nst}-sg\"\ngeneration = \"LTO-6\"\n"
        ),
    )
    .unwrap();

    let out = exits(
        h,
        &["collection", "run", "--label", "L1", "--init", "--dry-run"],
        0,
    );
    let text = stdout(&out);
    for says in [
        "collection \"photos\" batch 0: 1 unit would be staged",
        "photos/alpha",
        "L1, initialized first on the blank cartridge in the drive (--init)",
    ] {
        assert!(text.contains(says), "{says:?}: {}", shown(&out));
    }
    let doc = json(&exits(
        h,
        &[
            "--json",
            "collection",
            "run",
            "--label",
            "L1",
            "--init",
            "--dry-run",
        ],
        0,
    ));
    assert_eq!(doc["collection"], "photos", "{doc}");
    assert_eq!(doc["init"], true, "{doc}");
    assert_eq!(doc["labels"], serde_json::json!(["L1"]), "{doc}");

    let out = exits(h, &["collection", "run", "--label", "L1", "--dry-run"], 2);
    assert!(shown(&out).contains("or pass --init"), "{}", shown(&out));

    let out = exits(
        h,
        &[
            "collection",
            "run",
            "--label",
            "L1",
            "--label",
            "L2",
            "--dry-run",
        ],
        2,
    );
    assert!(
        shown(&out).contains("more than one destination label"),
        "{}",
        shown(&out)
    );

    // A second collection: the run names the one it means.
    let other = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(other.path().join("beta")).unwrap();
    std::fs::write(other.path().join("beta/a.txt"), b"x").unwrap();
    let other_root = other.path().canonicalize().unwrap().display().to_string();
    exits(
        h,
        &[
            "collection",
            "add",
            "movies",
            &other_root,
            "--tenant",
            "media",
        ],
        0,
    );
    let out = exits(
        h,
        &["collection", "run", "--label", "L1", "--init", "--dry-run"],
        2,
    );
    assert!(shown(&out).contains("photos, movies"), "{}", shown(&out));
    let out = exits(
        h,
        &[
            "collection",
            "run",
            "photos",
            "--label",
            "L1",
            "--init",
            "--dry-run",
        ],
        0,
    );
    assert!(stdout(&out).contains("photos/alpha"), "{}", shown(&out));
    let conn = tapectl::db::open(&h.join(".tapectl/tapectl.db")).unwrap();
    let rows: i64 = conn
        .query_row(
            "SELECT (SELECT COUNT(*) FROM volumes) + (SELECT COUNT(*) FROM stage_sets)",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(rows, 0, "a dry run initializes and stages nothing");
}
