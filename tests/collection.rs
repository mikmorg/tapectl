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
    };
    let paths = TapectlPaths::new(home.path().to_path_buf());

    let (report, errors) = sync_collection(&conn, &paths, &lib, true).unwrap();

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
