//! Issue #424: `scripts/first-run.sh` with `ADOPT_KEYS_FROM` adopts the
//! operator's key in step 7 and each profile tenant's in step 11, dry run
//! first, then for real — no tape at all (steps 7 and 11 never touch one),
//! as yourself (`--no-service-user`), on a throwaway home.
//!
//! The steps run as two invocations (`--from 7 --to 7`, `--from 11 --to
//! 11`): steps 8-10 need a drive. Step 7 mints the escrow identity of the
//! throwaway home (`--mint-escrow`: under `--auto` it mints only when told to,
//! ADR-0012 item 188) and prints its secret once, as it must; failure output
//! here blanks any secret key line.

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use age::secrecy::ExposeSecret;
use tempfile::TempDir;

fn redacted(bytes: &[u8]) -> String {
    let holds_secret = |l: &str| {
        l.match_indices("AGE-SECRET-KEY-1").any(|(i, m)| {
            l[i + m.len()..]
                .chars()
                .take(20)
                .filter(char::is_ascii_alphanumeric)
                .count()
                == 20
        })
    };
    String::from_utf8_lossy(bytes)
        .lines()
        .map(|l| {
            if holds_secret(l) {
                "<secret redacted>"
            } else {
                l
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

struct Rig {
    tmp: TempDir,
}

impl Rig {
    fn new() -> Self {
        let tmp = TempDir::new().unwrap();
        for d in ["keys", "work", "state", "staging", "h"] {
            std::fs::create_dir_all(tmp.path().join(d)).unwrap();
        }
        Rig { tmp }
    }
    fn p(&self, rel: &str) -> PathBuf {
        self.tmp.path().join(rel)
    }
    fn home(&self) -> PathBuf {
        self.p("home")
    }
    /// An identity file in `keys/` as `age-keygen -o` writes one; returns
    /// its public key.
    fn identity(&self, name: &str) -> String {
        let id = age::x25519::Identity::generate();
        let public = id.to_public().to_string();
        let path = self.p(&format!("keys/{name}.key"));
        std::fs::write(
            &path,
            format!(
                "# created: 2026-10-12T00:00:00Z\n# public key: {public}\n{}\n",
                id.to_string().expose_secret()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        public
    }
    fn profile(&self, adopt: bool) -> PathBuf {
        let path = self.p("test.profile");
        let adopt_line = if adopt {
            format!("ADOPT_KEYS_FROM={}\n", self.p("keys").display())
        } else {
            String::new()
        };
        std::fs::write(
            &path,
            format!(
                "HOME_DIR={home}\nTAPECTL={bin}\nSKIP_BUILD=1\nSKIP_TESTS=1\nOPERATOR=opr\n\
                 WORK_DIR={work}\nSTAGING_DIR={staging}\n{adopt_line}\
                 TENANTS=(\n  \"parents|ring two\"\n  \"family|ring three\"\n)\n\
                 CONTENDER_UNITS=()\n",
                home = self.home().display(),
                bin = env!("CARGO_BIN_EXE_tapectl"),
                work = self.p("work").display(),
                staging = self.p("staging").display(),
            ),
        )
        .unwrap();
        path
    }
    fn step(&self, profile: &Path, step: u32) -> Output {
        self.step_with(profile, step, &[])
    }
    /// `extra` holds flags such as `--mint-escrow`: step 7 under `--auto` on a
    /// fresh home mints an escrow identity only when it is given (ADR-0012
    /// item 188).
    fn step_with(&self, profile: &Path, step: u32, extra: &[&str]) -> Output {
        Command::new("bash")
            .arg(Path::new(env!("CARGO_MANIFEST_DIR")).join("scripts/first-run.sh"))
            .arg("--profile")
            .arg(profile)
            .args(["--no-service-user", "--auto"])
            .args(["--from", &step.to_string(), "--to", &step.to_string()])
            .args(extra)
            .env("HOME", self.p("h"))
            .env("USER", "fr-tester")
            .env("XDG_STATE_HOME", self.p("state"))
            .env("TAPECTL_HOME_POINTER", self.p("home-pointer"))
            .env_remove("TAPECTL_HOME")
            .stdin(std::process::Stdio::null())
            .output()
            .expect("spawn first-run.sh")
    }
    fn step_ok(&self, profile: &Path, step: u32) -> String {
        self.step_ok_with(profile, step, &[])
    }
    fn step_ok_with(&self, profile: &Path, step: u32, extra: &[&str]) -> String {
        let o = self.step_with(profile, step, extra);
        assert!(
            o.status.success(),
            "first-run.sh step {step} failed\nstdout:\n{}\nstderr:\n{}",
            redacted(&o.stdout),
            redacted(&o.stderr)
        );
        String::from_utf8_lossy(&o.stdout).into_owned()
    }
    fn keys(&self) -> Vec<(String, String, String)> {
        let conn = rusqlite::Connection::open(self.home().join("tapectl.db")).unwrap();
        let mut stmt = conn
            .prepare(
                "SELECT t.name, k.alias, k.fingerprint FROM encryption_keys k \
                 JOIN tenants t ON t.id = k.tenant_id WHERE k.is_escrow = 0 ORDER BY k.alias",
            )
            .unwrap();
        stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
            .unwrap()
            .map(Result::unwrap)
            .collect()
    }
}

#[test]
fn first_run_adopts_the_operator_and_the_profile_tenants_dry_run_then_real() {
    let rig = Rig::new();
    let opr = rig.identity("opr");
    let parents = rig.identity("parents");
    let family = rig.identity("family");
    let profile = rig.profile(true);

    let out7 = rig.step_ok_with(&profile, 7, &["--mint-escrow"]);
    assert!(
        out7.contains("--dry-run init --operator opr") && out7.contains("--operator-key"),
        "step 7 previews the adoption first:\n{}",
        redacted(out7.as_bytes())
    );
    assert!(out7.contains(&opr), "the dry run names the public key");
    let out11 = rig.step_ok(&profile, 11);
    for (name, public) in [("parents", &parents), ("family", &family)] {
        assert!(
            out11.contains(&format!("--dry-run tenant add {name}")),
            "step 11 previews {name}'s adoption:\n{out11}"
        );
        assert!(out11.contains(&format!("would create tenant \"{name}\"")));
        assert!(out11.contains(public.as_str()));
    }

    let want = vec![
        (
            "family".to_string(),
            "family-primary".to_string(),
            family.clone(),
        ),
        ("opr".to_string(), "opr-primary".to_string(), opr.clone()),
        (
            "parents".to_string(),
            "parents-primary".to_string(),
            parents.clone(),
        ),
    ];
    assert_eq!(rig.keys(), want, "each adopted its key; nothing was minted");
    for name in ["opr", "parents", "family"] {
        let stored = rig.home().join(format!("keys/{name}-primary.age.key"));
        assert_eq!(
            std::fs::read(&stored).unwrap(),
            std::fs::read(rig.p(&format!("keys/{name}.key"))).unwrap(),
            "{name}'s key is stored byte for byte"
        );
        let mode = std::fs::metadata(&stored).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
    }

    // Idempotent: both steps again change nothing.
    let again7 = rig.step_ok(&profile, 7);
    assert!(again7.contains("already initialised"), "{again7}");
    let again11 = rig.step_ok(&profile, 11);
    assert!(
        again11.contains("tenant parents already exists"),
        "{again11}"
    );
    assert_eq!(rig.keys(), want);
}

#[test]
fn a_missing_adopt_file_stops_the_step_before_anything_is_created() {
    let rig = Rig::new();
    rig.identity("opr");
    rig.identity("parents");
    // No family.key.
    let profile = rig.profile(true);
    rig.step_ok_with(&profile, 7, &["--mint-escrow"]);
    let o = rig.step(&profile, 11);
    assert!(!o.status.success(), "step 11 must stop");
    let err = redacted(&o.stderr);
    assert!(err.contains("family.key"), "names the missing file: {err}");
    let names: Vec<String> = rig.keys().into_iter().map(|k| k.0).collect();
    assert_eq!(
        names,
        vec!["opr".to_string()],
        "no tenant is created when one of the profile's files is missing"
    );

    // Step 7 refuses the same way: a home with no operator file creates
    // nothing at all.
    let rig = Rig::new();
    let profile = rig.profile(true);
    let o = rig.step_with(&profile, 7, &["--mint-escrow"]);
    assert!(!o.status.success());
    assert!(redacted(&o.stderr).contains("opr.key"));
    assert!(!rig.home().join("tapectl.db").exists());
}

/// Without `ADOPT_KEYS_FROM` the steps mint, as before (ADR-0012 item 41:
/// the fresh start mints new keys): a primary and a backup for the
/// operator and for each profile tenant, and no dry run of an adoption.
#[test]
fn without_adopt_keys_from_first_run_mints_as_before() {
    let rig = Rig::new();
    let profile = rig.profile(false);
    let out7 = rig.step_ok_with(&profile, 7, &["--mint-escrow"]);
    assert!(
        !out7.contains("--operator-key"),
        "{}",
        redacted(out7.as_bytes())
    );
    let out11 = rig.step_ok(&profile, 11);
    assert!(!out11.contains("--adopt-key"), "{out11}");
    let aliases: Vec<(String, String)> = rig.keys().into_iter().map(|k| (k.0, k.1)).collect();
    let want: Vec<(String, String)> = [
        ("family", "family-backup"),
        ("family", "family-primary"),
        ("opr", "opr-backup"),
        ("opr", "opr-primary"),
        ("parents", "parents-backup"),
        ("parents", "parents-primary"),
    ]
    .iter()
    .map(|(t, a)| (t.to_string(), a.to_string()))
    .collect();
    assert_eq!(aliases, want);
}

/// `ESCROW_PUBLIC_KEY` in a profile: step 7 under `--auto` adopts that
/// escrow identity instead of minting one (ADR-0012 2026-10-12 item 39, the
/// fresh start that keeps the Heir Kit), and says so. The positive control
/// is the mint test above, whose step 7 mints a fresh escrow identity.
#[test]
fn escrow_public_key_in_a_profile_is_adopted_at_init() {
    let rig = Rig::new();
    let profile = rig.profile(false);
    let escrow = age::x25519::Identity::generate().to_public().to_string();
    let mut text = std::fs::read_to_string(&profile).unwrap();
    text.push_str(&format!("ESCROW_PUBLIC_KEY={escrow}\n"));
    std::fs::write(&profile, text).unwrap();

    let out7 = rig.step_ok(&profile, 7);
    assert!(
        out7.contains("adopting the escrow public key from ESCROW_PUBLIC_KEY"),
        "{}",
        redacted(out7.as_bytes())
    );
    let conn = rusqlite::Connection::open(rig.home().join("tapectl.db")).unwrap();
    let registered: Vec<String> = conn
        .prepare("SELECT public_key FROM encryption_keys WHERE is_escrow = 1")
        .unwrap()
        .query_map([], |r| r.get(0))
        .unwrap()
        .map(Result::unwrap)
        .collect();
    assert_eq!(registered, vec![escrow]);
}

fn escrow_keys(home: &Path) -> Vec<String> {
    let conn = rusqlite::Connection::open(home.join("tapectl.db")).unwrap();
    let keys = conn
        .prepare("SELECT public_key FROM encryption_keys WHERE is_escrow = 1")
        .unwrap()
        .query_map([], |r| r.get(0))
        .unwrap()
        .map(Result::unwrap)
        .collect();
    keys
}

/// ADR-0012 item 188: on a fresh home with no escrow public key, step 7 under
/// `--auto` refuses to mint an escrow identity unless `--mint-escrow` is on
/// the command line, and creates nothing; with the flag it mints one (the
/// positive control). The flag and a key to adopt contradict each other.
#[test]
fn auto_mints_an_escrow_identity_only_with_mint_escrow() {
    let rig = Rig::new();
    let profile = rig.profile(false);
    let o = rig.step(&profile, 7);
    let err = redacted(&o.stderr);
    assert_eq!(o.status.code(), Some(1), "{err}");
    assert!(
        err.contains(
            "init would mint a NEW escrow identity, which --auto never does unless told to"
        ) && err.contains("(ADR-0005)")
            && err.contains("set ESCROW_PUBLIC_KEY")
            && err.contains("re-run with --mint-escrow")
            && err.contains("Nothing was created"),
        "{err}"
    );
    assert!(!rig.home().join("tapectl.db").exists(), "init did not run");

    let out7 = rig.step_ok_with(&profile, 7, &["--mint-escrow"]);
    assert!(
        out7.contains("--mint-escrow: init mints a NEW escrow identity"),
        "{}",
        redacted(out7.as_bytes())
    );
    assert_eq!(
        escrow_keys(&rig.home()).len(),
        1,
        "one escrow identity minted"
    );

    let rig = Rig::new();
    let profile = rig.profile(false);
    let escrow = age::x25519::Identity::generate().to_public().to_string();
    let o = rig.step_with(
        &profile,
        7,
        &["--mint-escrow", "--escrow-public-key", &escrow],
    );
    let err = redacted(&o.stderr);
    assert_eq!(o.status.code(), Some(1), "{err}");
    assert!(err.contains("contradict each other"), "{err}");
    assert!(!rig.home().join("tapectl.db").exists());
}

/// ADR-0012 item 189: a config.toml this tapectl cannot load is never
/// regenerated under `--auto` (`confirm` would say yes, and the regeneration
/// drops `[[backends.lto]]` and `[[collections]]`); the file is left as it is.
#[test]
fn auto_never_regenerates_an_unloadable_config() {
    let rig = Rig::new();
    let profile = rig.profile(false);
    rig.step_ok_with(&profile, 7, &["--mint-escrow"]);
    let cfg = rig.home().join("config.toml");
    let broken = format!(
        "unknown_key_for_506 = 1\n{}",
        std::fs::read_to_string(&cfg).unwrap()
    );
    std::fs::write(&cfg, &broken).unwrap();

    let o = rig.step(&profile, 7);
    let out = String::from_utf8_lossy(&o.stdout).into_owned();
    let err = redacted(&o.stderr);
    assert_eq!(o.status.code(), Some(1), "{out}\n{err}");
    assert!(
        out.contains("config.toml exists but this tapectl cannot LOAD it"),
        "the unloadable branch was taken: {out}"
    );
    assert!(
        err.contains(
            "drops its [[backends.lto]] and [[collections]] tables, so --auto never does it"
        ) && err.contains("without --auto"),
        "{err}"
    );
    assert_eq!(
        std::fs::read_to_string(&cfg).unwrap(),
        broken,
        "left as it was"
    );
    let superseded = std::fs::read_dir(rig.home())
        .unwrap()
        .filter_map(Result::ok)
        .any(|e| e.file_name().to_string_lossy().contains("superseded"));
    assert!(
        !superseded,
        "no backup was taken, because nothing was replaced"
    );
}
