//! Issue #424 (ADR-0012, 2026-10-12, item 42): adopting an existing age
//! keypair for a tenant (`tenant add --adopt-key`) and for the operator at
//! `init` (`init --operator-key`), through the real binary against
//! throwaway homes.
//!
//! The identity files are generated here with the age crate and written the
//! way `age-keygen -o` writes them (two comment lines, then the key), so
//! "byte-identical" is tested against a file that a line-extracting copy
//! would NOT reproduce. No secret is ever printed: failures show stdout and
//! a redacted stderr.

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use age::secrecy::ExposeSecret;
use tempfile::TempDir;

fn tapectl(home: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_tapectl"))
        .arg("--home")
        .arg(home)
        .args(args)
        .env("HOME", home.parent().unwrap())
        .env_remove("TAPECTL_HOME")
        .env_remove("XDG_CONFIG_HOME")
        .stdin(std::process::Stdio::null())
        .output()
        .expect("spawn tapectl")
}

/// Stderr with any line carrying a real secret key blanked, for failure
/// messages. (A hint naming the `AGE-SECRET-KEY-1…` form is kept.)
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

fn ok(home: &Path, args: &[&str]) -> String {
    let o = tapectl(home, args);
    assert!(
        o.status.success(),
        "`tapectl {}` failed\nstdout: {}\nstderr: {}",
        args.join(" "),
        String::from_utf8_lossy(&o.stdout),
        redacted(&o.stderr)
    );
    String::from_utf8_lossy(&o.stdout).into_owned()
}

fn refused(home: &Path, args: &[&str]) -> String {
    let o = tapectl(home, args);
    assert!(
        !o.status.success(),
        "`tapectl {}` should have been refused\nstdout: {}",
        args.join(" "),
        String::from_utf8_lossy(&o.stdout)
    );
    redacted(&o.stderr)
}

/// An identity file as `age-keygen -o` writes one; returns (path, public).
fn identity_file(dir: &Path, name: &str) -> (PathBuf, String, age::x25519::Identity) {
    let id = age::x25519::Identity::generate();
    let public = id.to_public().to_string();
    let path = dir.join(name);
    std::fs::write(
        &path,
        format!(
            "# created: 2026-10-12T00:00:00Z\n# public key: {public}\n{}\n",
            id.to_string().expose_secret()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
    (path, public, id)
}

fn s(p: &Path) -> &str {
    p.to_str().unwrap()
}

fn mode(p: &Path) -> u32 {
    std::fs::metadata(p).unwrap().permissions().mode() & 0o777
}

fn db(home: &Path) -> rusqlite::Connection {
    rusqlite::Connection::open(home.join("tapectl.db")).unwrap()
}

fn count(home: &Path, sql: &str) -> i64 {
    db(home).query_row(sql, [], |r| r.get(0)).unwrap()
}

fn key_files(home: &Path) -> Vec<String> {
    let mut v: Vec<String> = std::fs::read_dir(home.join("keys"))
        .unwrap()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    v.sort();
    v
}

fn dar_available() -> bool {
    Command::new("dar")
        .arg("--version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Acceptance 1: a tenant adopted from an identity file can decrypt a unit
/// staged after adoption, and the key file on disk is byte-identical to the
/// input, with the minted-key mode.
#[test]
fn an_adopted_tenant_decrypts_what_is_staged_after_adoption() {
    assert!(
        dar_available(),
        "`dar` not on PATH — see tests/test_dependencies.rs"
    );
    let tmp = TempDir::new().unwrap();
    let home = tmp.path().join("h");
    let staging = tmp.path().join("staging");
    std::fs::create_dir(&staging).unwrap();
    let src = tmp.path().join("src");
    std::fs::create_dir_all(src.join("sub")).unwrap();
    std::fs::write(src.join("a.txt"), "family letters\n").unwrap();
    std::fs::write(src.join("sub/b.txt"), "photos index\n").unwrap();
    let (key, public, identity) = identity_file(tmp.path(), "family.key");

    ok(&home, &["init", "--operator", "op", "--no-escrow"]);
    // A known escrow public key, so no secret is minted or printed.
    let escrow = age::x25519::Identity::generate().to_public().to_string();
    ok(&home, &["key", "import", "--escrow", &escrow]);
    let cfg = home.join("config.toml");
    let mut config = tapectl::config::Config::load(&cfg).unwrap();
    config.staging.directory = s(&staging).to_string();
    config.save(&cfg).unwrap();

    let out = ok(&home, &["tenant", "add", "family", "--adopt-key", s(&key)]);
    assert!(out.contains("adopted key \"family-primary\""), "{out}");
    assert!(out.contains(&public), "{out}");

    let stored = home.join("keys/family-primary.age.key");
    assert_eq!(
        std::fs::read(&stored).unwrap(),
        std::fs::read(&key).unwrap(),
        "the stored key is the input, byte for byte"
    );
    assert_eq!(
        mode(&stored),
        mode(&home.join("keys/op-primary.age.key")),
        "the minted-key mode"
    );
    assert_eq!(mode(&stored), 0o600);
    assert_eq!(
        std::fs::read_to_string(home.join("keys/family-primary.age.pub")).unwrap(),
        format!("{public}\n")
    );
    assert_eq!(
        count(
            &home,
            "SELECT COUNT(*) FROM encryption_keys k JOIN tenants t ON t.id = k.tenant_id \
             WHERE t.name = 'family'"
        ),
        1,
        "adoption mints nothing"
    );

    ok(
        &home,
        &[
            "unit",
            "init",
            s(&src),
            "--tenant",
            "family",
            "--name",
            "fam1",
        ],
    );
    ok(&home, &["snapshot", "create", "fam1"]);
    ok(&home, &["stage", "create", "fam1"]);

    let fingerprints: String = db(&home)
        .query_row(
            "SELECT key_fingerprints FROM stage_sets ORDER BY id DESC LIMIT 1",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert!(
        fingerprints.contains(&public),
        "the adopted key is a recipient: {fingerprints}"
    );

    let slices: Vec<PathBuf> = walkdir::WalkDir::new(&staging)
        .into_iter()
        .flatten()
        .map(|e| e.into_path())
        .filter(|p| p.to_string_lossy().ends_with(".dar.age"))
        .collect();
    assert!(!slices.is_empty(), "stage create wrote no slice");
    for slice in slices {
        let ct = std::fs::read(&slice).unwrap();
        let decryptor = age::Decryptor::new(&ct[..]).unwrap();
        let mut reader = decryptor
            .decrypt(std::iter::once(&identity as &dyn age::Identity))
            .unwrap_or_else(|e| {
                panic!(
                    "the adopted identity alone cannot open {}: {e}",
                    slice.display()
                )
            });
        let mut plain = Vec::new();
        std::io::Read::read_to_end(&mut reader, &mut plain).unwrap();
        assert!(!plain.is_empty());
    }
}

/// Acceptance 2: every refusal is tested, through the dry run and the real
/// add alike, and a refused adoption leaves no tenant or key row behind.
#[test]
fn every_tenant_add_adopt_key_refusal_leaves_no_row_or_file() {
    let tmp = TempDir::new().unwrap();
    let home = tmp.path().join("h");
    ok(&home, &["init", "--operator", "op", "--no-escrow"]);
    let (escrow_key, escrow_pub, _) = identity_file(tmp.path(), "escrow.key");
    ok(&home, &["key", "import", "--escrow", &escrow_pub]);
    let (taken, _, _) = identity_file(tmp.path(), "taken.key");
    ok(&home, &["tenant", "add", "alice", "--adopt-key", s(&taken)]);

    let not_identity = tmp.path().join("not-identity.key");
    std::fs::write(&not_identity, format!("{escrow_pub}\n")).unwrap();
    let two = tmp.path().join("two.key");
    let a = age::x25519::Identity::generate();
    let b = age::x25519::Identity::generate();
    std::fs::write(
        &two,
        format!(
            "{}\n{}\n",
            a.to_string().expose_secret(),
            b.to_string().expose_secret()
        ),
    )
    .unwrap();

    let tenants = || count(&home, "SELECT COUNT(*) FROM tenants");
    let keys = || count(&home, "SELECT COUNT(*) FROM encryption_keys");
    let before = (tenants(), keys(), key_files(&home));

    let cases: &[(&Path, &str)] = &[
        (&not_identity, "is a public key"),
        (&two, "holds 2 identities"),
        (&taken, "already registered as \"alice-primary\""),
        (&escrow_key, "escrow identity's secret"),
    ];
    for (file, why) in cases {
        let real = refused(&home, &["tenant", "add", "bob", "--adopt-key", s(file)]);
        let dry = refused(
            &home,
            &["--dry-run", "tenant", "add", "bob", "--adopt-key", s(file)],
        );
        assert!(real.contains(why), "want {why:?} in: {real}");
        assert_eq!(real, dry, "the dry run refuses what the add refuses");
        assert_eq!(
            (tenants(), keys(), key_files(&home)),
            before,
            "a refused adoption leaves nothing behind: {real}"
        );
    }

    // The dry run of an adoption that would succeed names the key and
    // writes nothing.
    let (fresh, fresh_pub, _) = identity_file(tmp.path(), "fresh.key");
    let out = ok(
        &home,
        &[
            "--dry-run",
            "tenant",
            "add",
            "bob",
            "--adopt-key",
            s(&fresh),
        ],
    );
    assert!(out.contains("DRY RUN"), "{out}");
    assert!(out.contains(&fresh_pub), "{out}");
    assert_eq!((tenants(), keys(), key_files(&home)), before);

    // The reverse order: an adopted tenant key offered as the escrow
    // identity is refused by name too (a second escrow is refused first
    // here, so this runs in a home without one).
    let home2 = tmp.path().join("h2");
    ok(&home2, &["init", "--operator", "op", "--no-escrow"]);
    let (k, k_pub, _) = identity_file(tmp.path(), "k.key");
    ok(&home2, &["tenant", "add", "carol", "--adopt-key", s(&k)]);
    let msg = refused(&home2, &["key", "import", "--escrow", &k_pub]);
    assert!(msg.contains("\"carol-primary\""), "{msg}");
    assert_eq!(
        count(
            &home2,
            "SELECT COUNT(*) FROM encryption_keys WHERE is_escrow = 1"
        ),
        0
    );
}

/// `init --operator-key`: the operator adopts its key, and the refusals
/// leave no home at all.
#[test]
fn init_adopts_the_operator_key_and_a_refused_init_creates_no_home() {
    let tmp = TempDir::new().unwrap();
    let (key, public, _) = identity_file(tmp.path(), "op.key");
    let escrow = age::x25519::Identity::generate().to_public().to_string();

    // Dry run: names the key, creates nothing.
    let dry_home = tmp.path().join("dry");
    let out = ok(
        &dry_home,
        &[
            "--dry-run",
            "init",
            "--operator",
            "op",
            "--operator-key",
            s(&key),
            "--escrow-public-key",
            &escrow,
        ],
    );
    assert!(out.contains(&public), "{out}");
    assert!(out.contains("nothing minted"), "{out}");
    assert!(!dry_home.exists(), "a dry run creates no home");

    // Refused: the escrow key's own secret as the operator key.
    let (escrow_secret_file, escrow_pub, _) = identity_file(tmp.path(), "escrow.key");
    for dry in [false, true] {
        let home = tmp.path().join(format!("refused-{dry}"));
        let mut args = vec![
            "init",
            "--operator",
            "op",
            "--operator-key",
            s(&escrow_secret_file),
            "--escrow-public-key",
            &escrow_pub,
        ];
        if dry {
            args.insert(0, "--dry-run");
        }
        let msg = refused(&home, &args);
        assert!(msg.contains("escrow identity's secret"), "{msg}");
        assert!(!home.exists(), "a refused init leaves no home: {msg}");
    }
    // Refused: not an identity.
    let bad = tmp.path().join("bad.key");
    std::fs::write(&bad, "hello\n").unwrap();
    let home = tmp.path().join("refused-bad");
    let msg = refused(
        &home,
        &["init", "--operator", "op", "--operator-key", s(&bad)],
    );
    assert!(msg.contains("not a single age X25519 identity"), "{msg}");
    assert!(!home.exists());

    // The real one.
    let home = tmp.path().join("h");
    let out = ok(
        &home,
        &[
            "init",
            "--operator",
            "op",
            "--operator-key",
            s(&key),
            "--escrow-public-key",
            &escrow,
        ],
    );
    assert!(out.contains("adopted \"op-primary\""), "{out}");
    assert_eq!(
        key_files(&home),
        vec![
            "op-escrow.age.pub",
            "op-primary.age.key",
            "op-primary.age.pub"
        ],
        "nothing minted for the operator"
    );
    let stored = home.join("keys/op-primary.age.key");
    assert_eq!(
        std::fs::read(&stored).unwrap(),
        std::fs::read(&key).unwrap()
    );
    assert_eq!(mode(&stored), 0o600);
    let (alias, fp, is_op): (String, String, bool) = db(&home)
        .query_row(
            "SELECT k.alias, k.fingerprint, t.is_operator FROM encryption_keys k \
             JOIN tenants t ON t.id = k.tenant_id WHERE k.is_escrow = 0",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .unwrap();
    assert_eq!(
        (alias.as_str(), fp.as_str(), is_op),
        ("op-primary", public.as_str(), true)
    );
}
