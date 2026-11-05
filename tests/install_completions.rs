//! ADR-0012 item 211 (OQ17): `scripts/install-systemd.sh` installs bash
//! completion for `tapectl` and `tapectl-op` (one function, bound to both)
//! and the man pages, each where `TAPECTL_COMPLETION_DIR` and
//! `TAPECTL_MAN_DIR` say, as `TAPECTL_UNIT_DIR` aims the units. Nothing here
//! changes the host: the installer runs `--render-dir` or `--dry-run`, with a
//! `sudo` first on `PATH` that records being called and fails.

use std::path::Path;
use std::process::Command;

use tempfile::TempDir;

/// The installer, from the repository, with `args`, the scratch paths, the
/// built binary as `--tapectl`, and a `sudo` that fails.
fn installer(dir: &Path, args: &[&str]) -> (i32, String) {
    let bin = dir.join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    let sudo = bin.join("sudo");
    std::fs::write(
        &sudo,
        format!(
            "#!/bin/sh\necho \"sudo $*\" >> {}/calls\nexit 97\n",
            dir.display()
        ),
    )
    .unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&sudo, std::fs::Permissions::from_mode(0o755)).unwrap();
    let out = Command::new("bash")
        .arg("scripts/install-systemd.sh")
        .args(args)
        .args(["--user", "tapectl-render-test-nobody", "--home"])
        .arg(dir.join("svc"))
        .args(["--backup-dir"])
        .arg(dir.join("backups"))
        .arg("--tapectl")
        .arg(env!("CARGO_BIN_EXE_tapectl"))
        .env(
            "PATH",
            format!(
                "{}:{}",
                bin.display(),
                std::env::var("PATH").unwrap_or_default()
            ),
        )
        .env("TAPECTL_UNIT_DIR", dir.join("units"))
        .env("TAPECTL_HOME_POINTER", dir.join("home-pointer"))
        .env("TAPECTL_COMPLETION_DIR", dir.join("completions"))
        .env("TAPECTL_MAN_DIR", dir.join("man1"))
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .output()
        .expect("bash must be available");
    (
        out.status.code().unwrap_or(-1),
        format!(
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        ),
    )
}

/// What bash completes after `words` with `script` loaded: the candidates
/// for the last word, through the function `complete` bound to the first.
fn complete(script: &Path, words: &[&str]) -> Vec<String> {
    let line = words.join(" ");
    let out = Command::new("bash")
        .arg("-c")
        .arg(format!(
            r#"source "$1"
read -ra COMP_WORDS <<< "{line}"
[ "${{#COMP_WORDS[@]}}" = {n} ] || COMP_WORDS+=("")
COMP_CWORD=$(( ${{#COMP_WORDS[@]}} - 1 ))
f="$(complete -p "${{COMP_WORDS[0]}}" | sed -n 's/.*-F \([^ ]*\) .*/\1/p')"
"$f" "${{COMP_WORDS[0]}}" "${{COMP_WORDS[COMP_CWORD]}}" "${{COMP_WORDS[COMP_CWORD-1]}}"
printf '%s\n' "${{COMPREPLY[@]}}""#,
            n = words.len()
        ))
        .arg("complete")
        .arg(script)
        .output()
        .unwrap();
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(str::to_string)
        .filter(|l| !l.is_empty())
        .collect()
}

#[test]
fn the_rendered_completion_completes_tapectl_and_tapectl_op_alike() {
    let dir = TempDir::new().unwrap();
    let render = dir.path().join("r");
    let (code, out) = installer(dir.path(), &["--render-dir", render.to_str().unwrap()]);
    assert_eq!(code, 0, "{out}");
    let text = std::fs::read_to_string(render.join("tapectl.bash-completion"))
        .unwrap_or_else(|e| panic!("no completion rendered ({e}): {out}"));
    assert!(out.contains("tapectl-op and the bash completion"), "{out}");
    for name in ["tapectl", "tapectl-op"] {
        assert!(
            text.contains(&format!(
                "complete -F _tapectl -o nosort -o bashdefault -o default {name}\n"
            )),
            "{name} is not bound"
        );
    }
    let script_path = dir.path().join("c.bash");
    std::fs::write(&script_path, &text).unwrap();
    for program in ["tapectl", "tapectl-op"] {
        assert_eq!(
            complete(&script_path, &[program, "drive", "a"]),
            ["add"],
            "{program} drive a"
        );
        // A hidden stub of a removed spelling is never offered.
        let ba = complete(&script_path, &[program, "ba"]);
        assert!(!ba.iter().any(|c| c == "backend"), "{program} ba: {ba:?}");
    }
}

#[test]
fn the_install_plan_puts_completion_and_man_pages_where_the_overrides_say() {
    let dir = TempDir::new().unwrap();
    // A page an earlier release installed for a command this one lacks.
    let man = dir.path().join("man1");
    std::fs::create_dir_all(&man).unwrap();
    std::fs::write(man.join("tapectl-backend-add.1"), "stale").unwrap();
    // And one already up to date.
    let repo_man = Path::new(env!("CARGO_MANIFEST_DIR")).join("docs/man");
    std::fs::copy(
        repo_man.join("tapectl-status.1"),
        man.join("tapectl-status.1"),
    )
    .unwrap();

    let (code, out) = installer(dir.path(), &["--dry-run"]);
    assert_eq!(code, 0, "{out}");
    let completions = dir.path().join("completions");
    for name in ["tapectl", "tapectl-op"] {
        // The source is a temp file; the destination is what matters.
        let dest = completions.join(name).display().to_string();
        assert!(
            out.lines()
                .any(|l| l.contains("would: sudo install -D -m 0644") && l.ends_with(&dest)),
            "no plan to install {dest}:\n{out}"
        );
    }
    let page = man.join("tapectl-drive-add.1").display().to_string();
    assert!(
        out.lines()
            .any(|l| l.contains("would: sudo install -D -m 0644") && l.ends_with(&page)),
        "{out}"
    );
    assert!(
        !out.lines()
            .any(|l| l.ends_with(&man.join("tapectl-status.1").display().to_string())),
        "an unchanged page is left alone:\n{out}"
    );
    assert!(
        out.contains(&format!(
            "would: sudo rm -f {}",
            man.join("tapectl-backend-add.1").display()
        )),
        "the stale page is removed:\n{out}"
    );
    assert!(
        !out.contains("tapectl-op.1"),
        "tapectl-op has no man page:\n{out}"
    );
    assert!(
        std::fs::read_to_string(dir.path().join("calls"))
            .unwrap_or_default()
            .is_empty(),
        "a dry run ran sudo"
    );
}
