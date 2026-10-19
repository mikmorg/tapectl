//! The LTO profiling toolkit (issue #479; ADR-0012 2026-10-17 items 96-98 and 100):
//! `scripts/lto-profile.sh`, its report builder and block emitter
//! `scripts/lto-profile-report.py`, and the two wrappers that keep the old entry points.
//!
//! No test here touches a tape. They feed the builder a RECORDED run directory
//! (`tests/fixtures/lto_profile/hp_lto6_run_a`, shaped on the HP LTO-6 figures of 2026-09-23;
//! its README says which are recorded and which synthetic), check the figures and their
//! sources, derive the block, and put the block through the real `config check`. The script's
//! own mechanics on a drive are proven by `scripts/lto-profile.sh` on mhvtl and, for the real
//! drive, by the operator's run on home2.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use serde_json::{json, Value};
use tempfile::TempDir;

fn repo() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn report_py() -> PathBuf {
    repo().join("scripts/lto-profile-report.py")
}

fn run_py(args: &[&str]) -> Output {
    Command::new("python3")
        .arg(report_py())
        .args(args)
        .output()
        .expect(
            "python3 must be on PATH (docs/install.md): the profiling toolkit's helper needs it",
        )
}

fn text(out: &Output) -> String {
    format!(
        "exit={:?}\nstdout={}\nstderr={}",
        out.status.code(),
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    )
}

fn stdout(out: &Output) -> String {
    assert!(out.status.success(), "{}", text(out));
    String::from_utf8(out.stdout.clone()).unwrap()
}

fn copy_dir(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).unwrap();
    for entry in std::fs::read_dir(from).unwrap() {
        let entry = entry.unwrap();
        let dest = to.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            copy_dir(&entry.path(), &dest);
        } else {
            std::fs::copy(entry.path(), dest).unwrap();
        }
    }
}

/// The recorded raw run directory, copied into `tmp`, with its report built.
fn built_run_a(tmp: &TempDir) -> PathBuf {
    let run = tmp.path().join("hp_lto6_run_a");
    copy_dir(
        &repo().join("tests/fixtures/lto_profile/hp_lto6_run_a"),
        &run,
    );
    stdout(&run_py(&["build", run.to_str().unwrap()]));
    run
}

fn profile(run: &Path) -> Value {
    serde_json::from_str(&std::fs::read_to_string(run.join("profile.json")).unwrap()).unwrap()
}

/// A variant of a built report, written as a run directory's `profile.json` under a new name.
fn variant(tmp: &TempDir, name: &str, base: &Value, edit: impl FnOnce(&mut Value)) -> PathBuf {
    let mut p = base.clone();
    p["run"]["id"] = json!(name);
    edit(&mut p);
    let dir = tmp.path().join(name);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("profile.json"),
        serde_json::to_string_pretty(&p).unwrap(),
    )
    .unwrap();
    dir
}

fn emit(runs: &[&Path], extra: &[&str]) -> Output {
    let mut args: Vec<&str> = vec!["emit"];
    args.extend_from_slice(extra);
    let strs: Vec<String> = runs.iter().map(|r| r.display().to_string()).collect();
    args.extend(strs.iter().map(String::as_str));
    run_py(&args)
}

/// The block's comments with their line wraps undone, for phrases the wrapping may split.
fn flat(block: &str) -> String {
    block.replace("\n#   ", " ")
}

fn block_lines(block: &str) -> Vec<&str> {
    block.lines().filter(|l| !l.starts_with('#')).collect()
}

// ---------------------------------------------------------------------------
// The tables stay src/media.rs's.
// ---------------------------------------------------------------------------

/// The helper's generation table is a copy of `src/media.rs` (the authority): density codes, marketed
/// capacities and the write-compatibility matrix must be equal, so a change there that is not made in
/// the script fails here.
#[test]
fn the_helper_s_media_table_equals_src_media_rs() {
    use tapectl::media::Generation;
    let table: Value = serde_json::from_str(&stdout(&run_py(&["media-table"]))).unwrap();
    let gens = table["generations"].as_array().unwrap();
    assert_eq!(gens.len(), 6, "LTO-5 to LTO-9 and the LTO-7 Type M medium");
    for g in gens {
        let name = g["generation"].as_str().unwrap();
        let generation = Generation::parse(name)
            .unwrap_or_else(|| panic!("{name} is not a generation src/media.rs knows"));
        assert_eq!(generation.as_str(), name, "canonical spelling");
        assert_eq!(
            generation.density_code(),
            Some(g["density_code"].as_u64().unwrap() as u8),
            "{name}: density code"
        );
        assert_eq!(
            generation.native_capacity_bytes(),
            g["native_capacity_bytes"].as_u64().unwrap(),
            "{name}: marketed capacity"
        );
    }
    let can_write = table["can_write"].as_object().unwrap();
    for drive in table["drive_generations"].as_array().unwrap() {
        let d = Generation::parse(drive.as_str().unwrap()).unwrap();
        let listed: Vec<&str> = can_write[drive.as_str().unwrap()]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap())
            .collect();
        for g in gens {
            let name = g["generation"].as_str().unwrap();
            let m = Generation::parse(name).unwrap();
            assert_eq!(
                listed.contains(&name),
                Generation::can_write(d, m),
                "{drive} writing {name}: the script's matrix and src/media.rs disagree"
            );
        }
    }
    assert_eq!(
        table["default_fill_ceiling"].as_f64().unwrap(),
        tapectl::config::DEFAULT_FILL_CEILING
    );
}

/// The python drive-generation reader agrees with the bash one `first-run.sh` uses
/// (`scripts/lib/drive-generation.sh`) on every model string either knows.
#[test]
fn the_drive_generation_reader_agrees_with_the_bash_library() {
    let lib = repo().join("scripts/lib/drive-generation.sh");
    for model in [
        "ULT3580-TD6",
        "ULT3580-HH8",
        "ULT3580-TD9",
        "ULTRIUM-TD5",
        "ULTRIUM-HH7",
        "Ultrium 6-SCSI",
        "Ultrium 9-SCSI",
        "ULT3580-TD7-M8",
        "DAT160",
        "",
    ] {
        let bash = Command::new("bash")
            .args([
                "-c",
                "source \"$1\"; drive_generation_from_model \"$2\"",
                "_",
            ])
            .arg(&lib)
            .arg(model)
            .output()
            .unwrap();
        let py = run_py(&["drive-generation", model]);
        assert_eq!(
            String::from_utf8_lossy(&bash.stdout).trim(),
            String::from_utf8_lossy(&py.stdout).trim(),
            "{model:?}: bash and python read the drive generation differently"
        );
        assert_eq!(bash.status.success(), py.status.success(), "{model:?}");
    }
}

#[test]
fn the_helper_s_own_selftest_passes() {
    let out = run_py(&["selftest"]);
    assert!(stdout(&out).contains("selftest: all passed"));
}

// ---------------------------------------------------------------------------
// The report.
// ---------------------------------------------------------------------------

/// The builder turns the raw captures into a report whose every figure names its source, keyed by the
/// drive (vendor, product, firmware, generation) with the medium's generation and maker beside it.
#[test]
fn build_reads_a_recorded_run_directory_into_a_sourced_report() {
    let tmp = TempDir::new().unwrap();
    let run = built_run_a(&tmp);
    let p = profile(&run);
    assert_eq!(p["format"], "tapectl-lto-profile");
    assert_eq!(p["format_version"], 1);
    assert_eq!(p["emulated"]["is_emulated"], false);
    assert_eq!(p["problems"], json!([]));

    let d = &p["drive"];
    assert_eq!(
        (
            d["vendor"].as_str(),
            d["product"].as_str(),
            d["firmware"].as_str(),
            d["generation"].as_str(),
            d["serial"].as_str()
        ),
        (
            Some("HP"),
            Some("Ultrium 6-SCSI"),
            Some("35GD"),
            Some("LTO-6"),
            Some("HUJ808A5L4")
        )
    );
    assert_eq!(d["key"], "HP|Ultrium 6-SCSI|35GD|LTO-6");
    let m = &p["medium"];
    assert_eq!(m["generation"], "LTO-6");
    assert_eq!(m["maker"], "FUJIFILM", "the media maker is recorded");
    assert_eq!(m["serial"], "EW7VWMVKF6");
    assert_eq!(m["nominal_capacity_bytes"], 2_500_000_000_000u64);
    assert!(
        m["generation_source"]
            .as_str()
            .unwrap()
            .contains("Medium density code"),
        "{m}"
    );

    // Every figure names where it came from.
    for (name, f) in p["figures"].as_object().unwrap() {
        assert!(
            f["source"].as_str().is_some_and(|s| !s.is_empty()),
            "figure {name} has no source: {f}"
        );
    }
    let figs = &p["figures"];
    assert_eq!(figs["hardware_compression_as_found"]["value"], "off");
    assert!(figs["hardware_compression_as_found"]["source"]
        .as_str()
        .unwrap()
        .contains("0x0f"));
    // MAM's unit, from the ratio of page 0x17's growth to MAM's decrease: 21,607 MB over 20,607 counts.
    assert_eq!(figs["mam_capacity_unit"]["value"], "MiB (2^20 bytes)");
    assert!(figs["mam_capacity_unit"]["source"]
        .as_str()
        .unwrap()
        .contains("page 0x17"));
    assert_eq!(figs["mam_over_report"]["value"], 0);

    // The end of tape, beside the wear (item 98).
    let ew = &p["early_warning"];
    assert_eq!(ew["observed"], true);
    assert_eq!(ew["bytes_before_first_refusal"], 2_501_995_134_976u64);
    assert_eq!(ew["first_refusal_errno"], "ENOSPC");
    assert!(
        (ew["fraction_of_nominal"].as_f64().unwrap() - 1.000798).abs() < 1e-6,
        "{ew}"
    );
    assert_eq!(ew["native_used_per_data_byte"], 1.0047);
    assert_eq!(ew["mam_remaining_at_warning"], 101_850);
    let pw = &p["post_warning"];
    assert_eq!(pw["reported_ew_to_eop"]["bytes"], 99_008_000_000u64);
    assert_eq!(pw["hard_end"]["reached_hard_end"], true);
    assert_eq!(
        pw["room_bytes"], 99_008_000_000u64,
        "the lower of the measured walk (99,377,741,824) and the drive's figure"
    );
    let snaps = p["wear"]["snapshots"].as_array().unwrap();
    assert_eq!(snaps.len(), 2, "as found and final");
    assert_eq!(snaps[0]["mam"]["load_count"], 2);
    assert_eq!(snaps[0]["mam"]["life_written_mib"], 31_131);
    assert!(snaps[0]["drive_rewrite_counters"]
        .as_object()
        .unwrap()
        .contains_key("write_total_rewrites"));
    assert!(snaps[1]["log_pages"]["0x17"]["Total write retries"].is_number());
    assert_eq!(
        p["wear"]["end_of_tape"]["bytes_before_first_refusal"],
        2_501_995_134_976u64
    );
    // What was not measured says so rather than vanishing.
    let not_measured = p["not_measured"].to_string();
    assert!(
        not_measured.contains("LBP") && not_measured.contains("EOD semantics"),
        "{not_measured}"
    );

    let txt = std::fs::read_to_string(run.join("PROFILE.txt")).unwrap();
    assert!(
        txt.contains("drive key: HP|Ultrium 6-SCSI|35GD|LTO-6"),
        "{txt}"
    );
    assert!(txt.contains("source:"), "{txt}");
    assert!(!txt.contains("EMULATED DEVICE"), "{txt}");
}

// ---------------------------------------------------------------------------
// The block.
// ---------------------------------------------------------------------------

/// A recorded report in, the block out: pinned in full (the date line aside), derived from the one run.
/// The run supports 0.98, and the block says 0.97 (never above the default, ADR-0012 item 98).
#[test]
fn emit_derives_the_block_from_a_recorded_report() {
    let tmp = TempDir::new().unwrap();
    let run = built_run_a(&tmp);
    let out = stdout(&emit(&[&run], &[]));
    let normalised: String = out
        .lines()
        .enumerate()
        .map(|(i, l)| {
            if i == 0 {
                "# tapectl LTO profile block, emitted by scripts/lto-profile.sh --emit on <DATE>"
                    .to_string()
            } else {
                l.to_string()
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
        + "\n";
    let golden =
        std::fs::read_to_string(repo().join("tests/fixtures/lto_profile/expected_block_a.toml"))
            .unwrap();
    assert_eq!(normalised, golden, "the emitted block changed");
    // Each derived value names the run it came from.
    assert!(out.contains("# fill_ceiling from hp_lto6_run_a"), "{out}");
    assert!(
        out.contains("# post_warning_bytes from hp_lto6_run_a"),
        "{out}"
    );
    // The runs support fill_ceiling 0.98 (first refusal at 1.0008 of nominal, less the 0.02 margin, rounded
    // down), but the emitted value is NEVER above the 0.97 default (ADR-0012 item 98) and the comment says
    // so; the room is the drive's 99,008 MB, the lower of that and the walk's 99,377,741,824 bytes.
    assert!(
        flat(&out).contains(
            "the runs support 0.98; kept at the 0.97 default until the wear allowance of ADR-0012 item 98 is fitted"
        ),
        "{out}"
    );
    assert_eq!(
        block_lines(&out),
        vec![
            "[[backends.lto]]",
            "name = \"hp-ultrium-6-scsi\"",
            "device_tape = \"/dev/tape/by-id/scsi-HUJ808A5L4-nst\"",
            "generation = \"LTO-6\"",
            "fill_ceiling = 0.97",
            "post_warning_bytes = \"99008M\"",
        ]
    );
}

/// The block is a config: it loads, its keys read back, and a slice size that cannot be sealed in half
/// the room is refused while 1 GiB (the default) is not.
#[test]
fn the_emitted_block_loads_as_a_config_and_sets_the_slice_cap() {
    let tmp = TempDir::new().unwrap();
    let run = built_run_a(&tmp);
    let block = stdout(&emit(&[&run], &[]));
    let path = tmp.path().join("config.toml");
    std::fs::write(&path, &block).unwrap();
    let cfg = tapectl::config::Config::load(&path).expect("the emitted block loads");
    let b = &cfg.backends.lto[0];
    assert_eq!(
        b.fill_ceiling, 0.97,
        "never above the default, whatever the runs support"
    );
    assert_eq!(b.post_warning_room_bytes().unwrap(), Some(99_008_000_000));
    assert_eq!(b.native_generation().unwrap().as_str(), "LTO-6");
    assert_eq!(cfg.slice_cap_problem("defaults.slice_size", 1 << 30), None);
    assert!(cfg
        .slice_cap_problem("defaults.slice_size", 60_000_000_000)
        .is_some());
}

/// The block pasted into a real config passes the real `tapectl config check` (the debug binary,
/// a temp home), and a slice size over half the room then fails it.
#[test]
fn the_emitted_block_passes_tapectl_config_check() {
    let tmp = TempDir::new().unwrap();
    let run = built_run_a(&tmp);
    let block = stdout(&emit(&[&run], &[]));
    let home = TempDir::new().unwrap();
    let tapectl = |args: &[&str]| {
        Command::new(env!("CARGO_BIN_EXE_tapectl"))
            .args(args)
            .env("HOME", home.path())
            .env_remove("XDG_CONFIG_HOME")
            .env_remove("TAPECTL_HOME")
            .env_remove("TAPECTL_CONFIG")
            .output()
            .unwrap()
    };
    assert!(tapectl(&["init", "--no-escrow"]).status.success());
    let cfg = home.path().join(".tapectl/config.toml");
    let original = std::fs::read_to_string(&cfg).unwrap();
    std::fs::write(&cfg, format!("{original}\n{block}")).unwrap();
    let ok = tapectl(&["config", "check"]);
    assert!(ok.status.success(), "{}", text(&ok));

    // 60 GB slices cannot be sealed in half of 99 GB of room.
    let with_big_slice = std::fs::read_to_string(&cfg).unwrap().replacen(
        "slice_size = \"1G\"",
        "slice_size = \"60G\"",
        1,
    );
    assert!(
        with_big_slice.contains("slice_size = \"60G\""),
        "precondition: init writes [defaults] slice_size"
    );
    std::fs::write(&cfg, with_big_slice).unwrap();
    let bad = tapectl(&["config", "check"]);
    assert!(!bad.status.success(), "{}", text(&bad));
    assert!(
        text(&bad).contains("post_warning_bytes = 99008M"),
        "{}",
        text(&bad)
    );
}

/// Several runs of one drive model combine by the MOST CONSERVATIVE figure, and every run is listed.
#[test]
fn several_runs_combine_by_the_minimum_and_are_all_listed() {
    let tmp = TempDir::new().unwrap();
    let a = built_run_a(&tmp);
    let base = profile(&a);
    // Cartridge B: its warning came at 0.9955 of nominal (less the margin: 0.9755 -> 0.97), 97,500 MB reported.
    let b = variant(&tmp, "hp_lto6_run_b", &base, |p| {
        p["medium"]["serial"] = json!("EW7VWMVKFX");
        p["early_warning"]["bytes_before_first_refusal"] = json!(2_488_750_000_000u64);
        p["early_warning"]["fraction_of_nominal"] = json!(0.9955);
        p["post_warning"]["room_bytes"] = json!(97_500_000_000u64);
    });
    let out = stdout(&emit(&[&a, &b], &[]));
    assert!(out.contains("fill_ceiling = 0.97"), "{out}");
    assert!(out.contains("post_warning_bytes = \"97500M\""), "{out}");
    assert!(
        out.contains("hp_lto6_run_a") && out.contains("hp_lto6_run_b"),
        "both runs are listed: {out}"
    );
    assert!(
        out.contains("# fill_ceiling from hp_lto6_run_b"),
        "the value names the run it came from: {out}"
    );
    assert!(
        out.contains("# post_warning_bytes from hp_lto6_run_b"),
        "{out}"
    );
    assert!(
        flat(&out).contains("the runs support 0.97, no more than the 0.97 default"),
        "{out}"
    );

    // A third cartridge stops short of the default: the ceiling comes DOWN, with a warning.
    let c = variant(&tmp, "hp_lto6_run_c", &base, |p| {
        p["early_warning"]["fraction_of_nominal"] = json!(0.9505);
        p["early_warning"]["bytes_before_first_refusal"] = json!(2_376_250_000_000u64);
    });
    let low = emit(&[&a, &b, &c], &[]);
    let low_out = stdout(&low);
    assert!(low_out.contains("fill_ceiling = 0.93"), "{low_out}");
    assert!(low_out.contains("LOWER than the 0.97 default"), "{low_out}");
    assert!(
        String::from_utf8_lossy(&low.stderr).contains("BELOW"),
        "{}",
        text(&low)
    );
    // The margin is a flag.
    let tight = stdout(&emit(&[&a], &["--margin", "0.005"]));
    assert!(
        block_lines(&tight).contains(&"fill_ceiling = 0.97"),
        "capped at the default: {tight}"
    );
    assert!(
        flat(&tight).contains("the runs support 0.99"),
        "the derived value is still reported: {tight}"
    );
}

#[test]
fn emit_refuses_runs_that_must_not_be_combined() {
    let tmp = TempDir::new().unwrap();
    let a = built_run_a(&tmp);
    let base = profile(&a);
    let other_model = variant(&tmp, "ibm_td8", &base, |p| {
        p["drive"]["vendor"] = json!("IBM");
        p["drive"]["product"] = json!("ULT3580-TD8");
        p["drive"]["generation"] = json!("LTO-8");
    });
    let msg = text(&emit(&[&a, &other_model], &[]));
    assert!(
        msg.contains("refusing to combine runs of different vendor"),
        "{msg}"
    );

    let other_medium = variant(&tmp, "lto5_medium", &base, |p| {
        p["medium"]["generation"] = json!("LTO-5")
    });
    let msg = text(&emit(&[&a, &other_medium], &[]));
    assert!(msg.contains("different medium generation"), "{msg}");

    let other_firmware = variant(&tmp, "fw", &base, |p| {
        p["drive"]["firmware"] = json!("36AB")
    });
    let msg = text(&emit(&[&a, &other_firmware], &[]));
    assert!(
        msg.contains("different drive firmware") && msg.contains("35GD") && msg.contains("36AB"),
        "{msg}"
    );
    let allowed = stdout(&emit(&[&a, &other_firmware], &["--allow-mixed-firmware"]));
    assert!(
        allowed.contains("35GD, 36AB") || allowed.contains("35GD,36AB"),
        "{allowed}"
    );

    let broken = variant(&tmp, "broken", &base, |p| {
        p["problems"] = json!(["a HP drive cannot write LTO-8 media"])
    });
    let msg = text(&emit(&[&a, &broken], &[]));
    assert!(msg.contains("recorded problems"), "{msg}");

    let not_a_report = tmp.path().join("nothing");
    std::fs::create_dir_all(&not_a_report).unwrap();
    assert!(!emit(&[&not_a_report], &[]).status.success());
}

/// An emulated run (mhvtl) contributes nothing to a derived value unless asked: its early warning and
/// its log pages are fiction. The block is still emitted, with the defaults, and says why.
#[test]
fn an_emulated_run_is_ignored_for_every_derived_value() {
    let tmp = TempDir::new().unwrap();
    let a = built_run_a(&tmp);
    let base = profile(&a);
    let mhvtl = variant(&tmp, "mhvtl_run", &base, |p| {
        p["emulated"] = json!({"is_emulated": true, "evidence": ["drive serial 'XYZZY_A4' is an mhvtl serial"]});
        p["early_warning"]["fraction_of_nominal"] = json!(0.0002);
    });
    let out = emit(&[&mhvtl], &[]);
    let block = stdout(&out);
    assert!(
        block.contains("fill_ceiling = 0.97") && block.contains("every run measured an emulator"),
        "{block}"
    );
    assert!(block.contains("# post_warning_bytes: not set"), "{block}");
    assert!(
        !block_lines(&block)
            .iter()
            .any(|l| l.starts_with("post_warning_bytes")),
        "no key is set from an emulator's fiction: {block}"
    );
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("measured an emulator"),
        "{}",
        text(&out)
    );
    // Mixed with a real run, the real one decides.
    let both = stdout(&emit(&[&a, &mhvtl], &[]));
    assert!(
        flat(&both).contains("the runs support 0.98"),
        "the real run decides what is derived: {both}"
    );
    assert!(
        block_lines(&both).contains(&"fill_ceiling = 0.97"),
        "{both}"
    );
    // Trusted, the emulator's 0.02% early warning leaves nothing after the margin: refused, not emitted.
    let trusted = emit(&[&mhvtl], &["--trust-emulated"]);
    assert!(!trusted.status.success(), "{}", text(&trusted));
    assert!(
        text(&trusted).contains("leaves nothing after"),
        "{}",
        text(&trusted)
    );
}

/// A run whose readback FAILED did not prove the bytes it wrote past the warning readable: the walk's
/// figure is set aside, and only the drive's own page 0x0c figure can set the room.
#[test]
fn a_failed_readback_sets_the_measured_walk_aside() {
    let tmp = TempDir::new().unwrap();
    let run = tmp.path().join("hp_lto6_run_a");
    copy_dir(
        &repo().join("tests/fixtures/lto_profile/hp_lto6_run_a"),
        &run,
    );
    std::fs::write(
        run.join("fill/readback.json"),
        r#"{"checks": [{"name": "slice-all-blocks", "status": "FAIL", "detail": "3 of 2048 blocks differ"}]}"#,
    )
    .unwrap();
    // Make the walk the smaller figure, so the effect is visible.
    let result_path = run.join("fill/result.json");
    let mut result: Value =
        serde_json::from_str(&std::fs::read_to_string(&result_path).unwrap()).unwrap();
    result["hard_end"]["bytes_past_first_refusal"] = json!(50_000_000_000u64);
    std::fs::write(&result_path, serde_json::to_string(&result).unwrap()).unwrap();
    stdout(&run_py(&["build", run.to_str().unwrap()]));
    let p = profile(&run);
    assert_eq!(p["post_warning"]["readback"], "FAIL");
    assert_eq!(
        p["post_warning"]["room_bytes"], 99_008_000_000u64,
        "the failed walk's 50 GB does not count"
    );
    assert!(p["post_warning"]["measured_ignored"]
        .as_str()
        .unwrap()
        .contains("readback FAILED"));
}

// ---------------------------------------------------------------------------
// The script and its wrappers.
// ---------------------------------------------------------------------------

fn run_script(name: &str, args: &[&str]) -> Output {
    Command::new("bash")
        .arg(repo().join("scripts").join(name))
        .args(args)
        .output()
        .unwrap()
}

/// `--emit` through the shell entry point is the helper's `emit`.
#[test]
fn the_script_emits_through_its_emit_option() {
    let tmp = TempDir::new().unwrap();
    let run = built_run_a(&tmp);
    let out = run_script("lto-profile.sh", &["--emit", run.to_str().unwrap()]);
    assert!(block_lines(&stdout(&out)).contains(&"fill_ceiling = 0.97"));
}

/// Nothing is touched without the cartridge's name, and a device that is not an st node is refused
/// before any tape command: these are exit 2 on every host, with or without the SCSI tools.
#[test]
fn the_script_refuses_without_consent_or_a_tape_node() {
    let no_name = run_script("lto-profile.sh", &["--device", "/dev/null"]);
    assert_eq!(no_name.status.code(), Some(2), "{}", text(&no_name));
    let msg = text(&no_name);
    assert!(
        msg.contains("--i-will-lose-the-cartridge") || msg.contains("required binary missing"),
        "{msg}"
    );

    let not_tape = run_script(
        "lto-profile.sh",
        &["--device", "/dev/null", "--i-will-lose-the-cartridge", "X"],
    );
    assert_eq!(not_tape.status.code(), Some(2), "{}", text(&not_tape));
    let msg = text(&not_tape);
    // The lock is taken before the node is looked at (as in the scripts this one absorbed), so a gate or
    // a suite running on this host at the same moment answers first.
    assert!(
        msg.contains("not an st tape node")
            || msg.contains("required binary missing")
            || msg.contains("tapectl-drive-poll")
            || msg.contains("another process holds the tape lock"),
        "{msg}"
    );

    let both = run_script(
        "lto-profile.sh",
        &[
            "--quick",
            "--fill-only",
            "--device",
            "/dev/null",
            "--i-will-lose-the-cartridge",
            "X",
        ],
    );
    assert_eq!(both.status.code(), Some(2), "{}", text(&both));
}

/// The old entry points still answer: the fill wrapper hands its arguments to the profiling script in
/// its fill-only mode, the measure wrapper maps `--erase-cartridge` and refuses the consent bypass.
#[test]
fn the_old_entry_points_are_thin_wrappers() {
    let help = run_script("lto6-fill.sh", &["--help"]);
    assert!(
        stdout(&help).contains("lto-profile.sh"),
        "the fill wrapper runs the profiling script"
    );
    let bypass = run_script(
        "lto6-measure.sh",
        &["--erase-cartridge", "X", "--allow-unverified-barcode"],
    );
    assert_eq!(bypass.status.code(), Some(2), "{}", text(&bypass));
    assert!(
        text(&bypass).contains("no longer accepted"),
        "{}",
        text(&bypass)
    );
    let mapped = run_script(
        "lto6-measure.sh",
        &["--erase-cartridge", "X", "--device", "/dev/null"],
    );
    let msg = text(&mapped);
    assert_eq!(mapped.status.code(), Some(2), "{msg}");
    assert!(
        msg.contains("lto-profile:"),
        "the wrapper ran lto-profile.sh with the mapped consent flag: {msg}"
    );
    assert!(
        !msg.contains("is required: this run erases"),
        "--erase-cartridge was mapped to the consent flag: {msg}"
    );
    let src = std::fs::read_to_string(repo().join("scripts/lto6-measure.sh")).unwrap();
    assert!(
        src.lines()
            .filter(|l| !l.trim_start().starts_with('#') && !l.trim().is_empty())
            .count()
            < 30,
        "a thin wrapper"
    );
}
