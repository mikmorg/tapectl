//! What `collection add` checks before it writes the table (ADR-0012 item
//! 206; the 1.9.0 coordinator's OQ23): that the user tapectl runs as (the
//! service user, under `tapectl-op`) can reach the root, read every folder
//! and file under it, and write each unit folder for its
//! `.tapectl-unit.toml` unless the collection keeps no dotfiles. The
//! kernel answers for that user (`faccessat` with the effective ids), so an
//! ACL counts as it will when `collection sync` and `snapshot create` run.
//!
//! Nothing is granted here: tapectl runs as the user that lacks the access.
//! When something fails, [`Access::grant`] gives the `setfacl` lines, the
//! shape `scripts/first-run.sh`'s `grant_collection` runs, and the command
//! refuses.

use std::path::{Path, PathBuf};

use nix::unistd::AccessFlags;
use walkdir::WalkDir;

use crate::config::CollectionConfig;
use crate::staging::exclude::Excludes;

/// How many failures of one kind are named; the walk stops reading the tree
/// after this many, since the grant covers the rest.
const SHOWN: usize = 5;

/// What the user tapectl runs as may not do under one collection root.
#[derive(Debug, Clone)]
pub struct Access {
    user: String,
    root: PathBuf,
    unit_depth: usize,
    dotfiles: bool,
    /// Directories above the root it may not pass through.
    untraversable: Vec<PathBuf>,
    /// Folders and files under the root it may not read.
    unreadable: Vec<PathBuf>,
    /// Unit folders it may not write a dotfile into.
    unwritable: Vec<PathBuf>,
}

/// Whether this process (its effective ids) may `mode` `path`.
fn may(path: &Path, mode: AccessFlags) -> bool {
    nix::unistd::faccessat(None, path, mode, nix::fcntl::AtFlags::AT_EACCESS).is_ok()
}

/// Check `root` (canonical) for `lib`'s units.
pub fn check(root: &Path, lib: &CollectionConfig) -> Access {
    let unit_depth = lib.unit_depth.max(1);
    let mut access = Access {
        user: crate::unit::effective_user_name(),
        root: root.to_path_buf(),
        unit_depth,
        dotfiles: lib.dotfiles,
        untraversable: Vec::new(),
        unreadable: Vec::new(),
        unwritable: Vec::new(),
    };
    // `/` is passable for every user; each directory below it, down to the
    // root's parent, must be too.
    let mut ancestors: Vec<&Path> = root
        .ancestors()
        .skip(1)
        .filter(|a| a.parent().is_some())
        .collect();
    ancestors.reverse();
    access.untraversable = ancestors
        .into_iter()
        .filter(|a| !may(a, AccessFlags::X_OK))
        .map(Path::to_path_buf)
        .collect();
    if !access.untraversable.is_empty() {
        // Nothing below can be reached to be checked.
        return access;
    }

    let excludes = Excludes::new(root, &lib.exclude);
    let walk = WalkDir::new(root)
        .follow_links(false)
        .into_iter()
        // A unit folder the collection excludes is never a unit, so it is
        // never read.
        .filter_entry(|e| {
            !(e.depth() == unit_depth
                && e.file_type().is_dir()
                && excludes.excludes_unit_dir(e.path()))
        });
    for entry in walk {
        if access.unreadable.len() >= SHOWN {
            break;
        }
        let entry = match entry {
            Ok(entry) => entry,
            Err(err) => {
                let denied = err
                    .io_error()
                    .is_some_and(|io| io.kind() == std::io::ErrorKind::PermissionDenied);
                if let (true, Some(path)) = (denied, err.path()) {
                    access.unreadable.push(path.to_path_buf());
                }
                continue;
            }
        };
        let ft = entry.file_type();
        if ft.is_symlink() {
            // Archived as a link: nothing behind it is read.
            continue;
        }
        let path = entry.path();
        let readable = match ft.is_dir() {
            true => may(path, AccessFlags::R_OK | AccessFlags::X_OK),
            false => may(path, AccessFlags::R_OK),
        };
        if !readable {
            access.unreadable.push(path.to_path_buf());
        }
        if access.dotfiles
            && ft.is_dir()
            && entry.depth() == unit_depth
            && !may(path, AccessFlags::W_OK | AccessFlags::X_OK)
            && access.unwritable.len() < SHOWN
        {
            access.unwritable.push(path.to_path_buf());
        }
    }
    access
}

impl Access {
    /// `None` when the user may do everything the collection needs;
    /// otherwise the refusal: the `setfacl` lines that grant it, to run as
    /// root, then what failed.
    pub fn grant(&self) -> Option<String> {
        if self.untraversable.is_empty() && self.unreadable.is_empty() && self.unwritable.is_empty()
        {
            return None;
        }
        let u = &self.user;
        let root = crate::unit::shell_word(&self.root.display().to_string());
        // What cannot be reached cannot be checked: every grant below it is
        // printed. A tree not wholly read may hide unit folders too.
        let blind = !self.untraversable.is_empty();
        let read = blind || !self.unreadable.is_empty();
        let write = self.dotfiles && (read || !self.unwritable.is_empty());

        let mut lines = Vec::new();
        if blind {
            let dirs: Vec<String> = self
                .untraversable
                .iter()
                .map(|d| crate::unit::shell_word(&d.display().to_string()))
                .collect();
            lines.push(format!("sudo setfacl -m u:{u}:x {}", dirs.join(" ")));
        }
        if read {
            lines.push(format!("sudo setfacl -R -m u:{u}:rX {root}"));
            lines.push(format!("sudo setfacl -R -d -m u:{u}:rX {root}"));
        }
        if write {
            let d = self.unit_depth;
            lines.push(format!(
                "sudo find {root} -mindepth {d} -maxdepth {d} -type d -exec setfacl -m \
                 u:{u}:rwX {{}} +"
            ));
        }

        let mut msg = format!(
            "user {u} cannot {} under {}, and tapectl cannot grant that itself. Grant it as \
             root (POSIX ACLs: owners and modes stay as they are):",
            match (read, write) {
                (true, true) => "read everything and write each unit folder's .tapectl-unit.toml",
                (true, false) => "read everything",
                (false, _) => "write each unit folder's .tapectl-unit.toml",
            },
            self.root.display()
        );
        for line in &lines {
            msg.push_str(&format!("\n  {line}"));
        }
        msg.push_str("\nthen run `tapectl collection add` again.");
        if write && !self.unwritable.is_empty() {
            msg.push_str(
                " For a source tapectl must not write, add it with --no-dotfiles instead: each \
                 unit is then known by its path.",
            );
        }
        msg.push_str(" What failed:");
        let named = |what: &str, paths: &[PathBuf]| -> Vec<String> {
            let mut out: Vec<String> = paths
                .iter()
                .take(SHOWN)
                .map(|p| format!("\n  cannot {what} {}", p.display()))
                .collect();
            if paths.len() >= SHOWN {
                out.push(format!("\n  (the first {SHOWN}; there may be more)"));
            }
            out
        };
        for line in named("pass through", &self.untraversable)
            .into_iter()
            .chain(named("read", &self.unreadable))
            .chain(named("write", &self.unwritable))
        {
            msg.push_str(&line);
        }
        Some(msg)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn lib(root: &Path, dotfiles: bool) -> CollectionConfig {
        CollectionConfig {
            name: "photos".into(),
            root: root.display().to_string(),
            tenant: "t".into(),
            unit_depth: 1,
            exclude: vec!["skip*".into()],
            archive_set: None,
            dotfiles,
            snapshot_command: None,
            release_command: None,
        }
    }

    fn chmod(path: &Path, mode: u32) {
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).unwrap();
    }

    /// Mode bits do not bind root (nor some filesystems): probe the real
    /// precondition, as `staging::clean`'s tests do, before asserting.
    fn modes_bind(dir: &Path) -> bool {
        let probe = dir.join("probe");
        std::fs::create_dir(&probe).unwrap();
        chmod(&probe, 0o500);
        let binds = std::fs::write(probe.join("x"), b"x").is_err();
        chmod(&probe, 0o700);
        std::fs::remove_dir_all(&probe).unwrap();
        binds
    }

    #[test]
    fn a_tree_the_user_may_read_and_write_needs_no_grant() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        std::fs::create_dir_all(root.join("a")).unwrap();
        std::fs::write(root.join("a/f"), b"x").unwrap();
        assert!(check(&root, &lib(&root, true)).grant().is_none());
    }

    /// The four `setfacl` shapes, each only when needed, with the real user
    /// and the real paths; `--no-dotfiles` asks for no write.
    #[test]
    fn each_missing_access_prints_the_grant_that_fixes_it() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        if !modes_bind(&root) {
            eprintln!("SKIPPED: directory modes do not bind this user (root?)");
            return;
        }
        let user = crate::unit::effective_user_name();
        std::fs::create_dir_all(root.join("a")).unwrap();
        std::fs::create_dir_all(root.join("b")).unwrap();
        std::fs::create_dir_all(root.join("skipped")).unwrap();
        chmod(&root.join("a"), 0o555);
        // An excluded folder is never a unit: neither read nor written.
        chmod(&root.join("skipped"), 0o000);

        let grant = check(&root, &lib(&root, true))
            .grant()
            .expect("a is unwritable");
        assert!(
            grant.contains(&format!(
                "sudo find {} -mindepth 1 -maxdepth 1 -type d -exec setfacl -m u:{user}:rwX {{}} +",
                root.display()
            )),
            "{grant}"
        );
        assert!(!grant.contains("-R -m"), "nothing unreadable: {grant}");
        assert!(grant.contains(&format!("cannot write {}", root.join("a").display())));
        assert!(grant.contains("--no-dotfiles"), "{grant}");
        assert!(check(&root, &lib(&root, false)).grant().is_none());

        chmod(&root.join("b"), 0o000);
        let grant = check(&root, &lib(&root, false))
            .grant()
            .expect("b is unreadable");
        assert!(
            grant.contains(&format!(
                "sudo setfacl -R -m u:{user}:rX {}",
                root.display()
            )),
            "{grant}"
        );
        assert!(grant.contains(&format!(
            "sudo setfacl -R -d -m u:{user}:rX {}",
            root.display()
        )));
        assert!(
            !grant.contains("find"),
            "--no-dotfiles: no write grant: {grant}"
        );
        assert!(grant.contains(&format!("cannot read {}", root.join("b").display())));

        for d in ["a", "b", "skipped"] {
            chmod(&root.join(d), 0o755);
        }
    }

    #[test]
    fn an_ancestor_the_user_cannot_pass_through_is_named_with_every_grant() {
        let tmp = tempfile::tempdir().unwrap();
        let top = tmp.path().canonicalize().unwrap();
        if !modes_bind(&top) {
            eprintln!("SKIPPED: directory modes do not bind this user (root?)");
            return;
        }
        let user = crate::unit::effective_user_name();
        let root = top.join("closed").join("photos");
        std::fs::create_dir_all(&root).unwrap();
        chmod(&top.join("closed"), 0o600);
        let grant = check(&root, &lib(&root, true)).grant().expect("blind");
        chmod(&top.join("closed"), 0o755);
        assert!(
            grant.contains(&format!(
                "sudo setfacl -m u:{user}:x {}",
                top.join("closed").display()
            )),
            "{grant}"
        );
        assert!(grant.contains("-R -m"), "{grant}");
        assert!(grant.contains("-exec setfacl"), "{grant}");
    }
}
