//! The writer's floor (ADR-0012 item 274; #563): GNU tar 1.35 or later,
//! built with ACL support.
//!
//! The version is the early refusal; the proof, on any version, is the
//! stream reader's check of the stream as it passes (`volume-format-v3.md`
//! §3.2). ACL support is found by doing it: one file with an ACL entry for
//! a numeric id is archived with the writer's own flags, and the member's
//! ACL record is read back.

use std::fmt;
use std::io::Read;
use std::path::{Path, PathBuf};

use crate::error::{Result, TapectlError};

use super::writer::{spawn_archive, write_list, CreateParams, TarFinish};

/// The writer's floor: GNU tar 1.35 (item 274).
pub const FLOOR: (u32, u32) = (1, 35);

/// What GNU tar's first `--version` line starts with.
pub const GNU_PREFIX: &str = "tar (GNU tar) ";

/// A tar's version, as its first `--version` line gives it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct TarVersion {
    pub major: u32,
    pub minor: u32,
    pub patch: Option<u32>,
}

impl fmt::Display for TarVersion {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.patch {
            Some(p) => write!(f, "{}.{}.{}", self.major, self.minor, p),
            None => write!(f, "{}.{}", self.major, self.minor),
        }
    }
}

impl TarVersion {
    /// Whether this version is at or above [`FLOOR`]. The minor is
    /// compared as a number: 1.4 is below 1.35.
    pub fn meets_floor(&self) -> bool {
        (self.major, self.minor) >= FLOOR
    }
}

/// Whether the tar stores an ACL as the writer must: once, in the text
/// record, every qualifier a number (§3.1; items 270, 273, 274).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AclSupport {
    /// The probe file's member carried the text record with its entries by
    /// number.
    Yes,
    /// The tar ran, and the member carried no ACL record (a GNU tar built
    /// without libacl), or a record that names an account or lost an entry
    /// (GNU tar below 1.35 writes names with `--numeric-owner`). Refused.
    No(String),
    /// The probe could not tell: the file system refused the ACL, the tar
    /// is not GNU tar, or tar wrote no member for the file. Not a refusal on
    /// its own; the stream's own check is the proof.
    Unknown(String),
}

impl fmt::Display for AclSupport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            AclSupport::Yes => write!(f, "Yes"),
            AclSupport::No(why) => write!(f, "No ({why})"),
            AclSupport::Unknown(why) => write!(f, "Unknown ({why})"),
        }
    }
}

/// What [`probe`] found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TarCheck {
    /// The tar run: the configured path, or the bare name found on `PATH`.
    pub path: PathBuf,
    /// The first line of `tar --version`.
    pub first_line: String,
    /// Whether the first line starts [`GNU_PREFIX`].
    pub gnu: bool,
    /// The version the first line gives, if it gives one.
    pub version: Option<TarVersion>,
    /// GNU tar at or above [`FLOOR`].
    pub meets_floor: bool,
    /// What the probe's archive of one file with an ACL showed.
    pub acl: AclSupport,
}

impl TarCheck {
    /// The refusal to stage with this tar, or `None`: not GNU tar, GNU tar
    /// below the floor, or ACL support `No`. It names what was found and
    /// the floor.
    pub fn refusal(&self) -> Option<String> {
        let floor = format!(
            "the writer's floor is GNU tar {}.{} or later, built with ACL support",
            FLOOR.0, FLOOR.1
        );
        let found = match &self.version {
            Some(v) => v.to_string(),
            None => "of an unreadable version".to_string(),
        };
        if !self.gnu {
            return Some(format!(
                "{} is not GNU tar (it says \"{}\"); {floor}",
                self.path.display(),
                self.first_line
            ));
        }
        if !self.meets_floor {
            return Some(format!(
                "GNU tar {found} at {} is below the writer's floor; {floor}",
                self.path.display()
            ));
        }
        if let AclSupport::No(why) = &self.acl {
            return Some(format!(
                "GNU tar {found} at {} does not store an ACL by number ({why}); {floor}",
                self.path.display()
            ));
        }
        None
    }
}

impl fmt::Display for TarCheck {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(f, "path:         {}", self.path.display())?;
        writeln!(f, "first line:   {}", self.first_line)?;
        writeln!(f, "GNU tar:      {}", if self.gnu { "yes" } else { "no" })?;
        match &self.version {
            Some(v) => writeln!(f, "version:      {v}")?,
            None => writeln!(f, "version:      (none read)")?,
        }
        writeln!(
            f,
            "meets floor:  {} (GNU tar {}.{})",
            if self.meets_floor { "yes" } else { "no" },
            FLOOR.0,
            FLOOR.1
        )?;
        write!(f, "ACL support:  {}", self.acl)
    }
}

/// Read a version at the start of `s`: digits and dots, at least a major
/// and a minor. What follows (a distribution's suffix) is ignored.
fn leading_version(s: &str) -> Option<TarVersion> {
    let lead: String = s
        .chars()
        .take_while(|c| c.is_ascii_digit() || *c == '.')
        .collect();
    let mut parts = lead.split('.');
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next()?.parse().ok()?;
    let patch = parts.next().and_then(|p| p.parse().ok());
    Some(TarVersion {
        major,
        minor,
        patch,
    })
}

/// Parse `tar --version`'s first line: whether it is GNU tar, and the
/// version it gives. For another tar the version is its first word that
/// starts with a digit (informative only: the floor is GNU tar's).
pub fn parse_first_line(line: &str) -> (bool, Option<TarVersion>) {
    if let Some(rest) = line.strip_prefix(GNU_PREFIX) {
        return (true, leading_version(rest.trim_start()));
    }
    let version = line
        .split_whitespace()
        .find(|w| w.starts_with(|c: char| c.is_ascii_digit()))
        .and_then(leading_version);
    (false, version)
}

/// Find `binary` as it will be run: a name with a `/` is taken as given, a
/// bare name is looked up on `PATH` (never a hard-coded `/usr/bin/tar`).
fn resolve(binary: &str) -> Result<PathBuf> {
    if binary.contains('/') {
        let p = PathBuf::from(binary);
        return if p.is_file() {
            Ok(p)
        } else {
            Err(TapectlError::TarNotFound(binary.to_string()))
        };
    }
    let path_var = std::env::var_os("PATH").unwrap_or_default();
    crate::policy::depth_check::resolve_on_path(binary, &path_var)
        .ok_or_else(|| TapectlError::TarNotFound(binary.to_string()))
}

/// Probe `binary`: its path, its `--version` first line, whether it is GNU
/// tar at or above [`FLOOR`], and whether it stores an ACL by number,
/// found by archiving one file with an ACL in a temporary directory made
/// inside `scratch` (removed after).
pub fn probe(binary: &str, scratch: &Path) -> Result<TarCheck> {
    let path = resolve(binary)?;
    let output = super::command(&path)
        .arg("--version")
        .output()
        .map_err(|e| super::writer::spawn_error(binary, e))?;
    let text = if output.stdout.is_empty() {
        String::from_utf8_lossy(&output.stderr).into_owned()
    } else {
        String::from_utf8_lossy(&output.stdout).into_owned()
    };
    let first_line = text.lines().next().unwrap_or("").trim_end().to_string();
    let (gnu, version) = parse_first_line(&first_line);
    let meets_floor = gnu && version.is_some_and(|v| v.meets_floor());
    let acl = if gnu {
        acl_by_doing_it(&path, scratch)
    } else {
        AclSupport::Unknown("not GNU tar: the ACL test runs only with GNU tar's flags".into())
    };
    Ok(TarCheck {
        path,
        first_line,
        gnu,
        version,
        meets_floor,
        acl,
    })
}

/// The file the probe archives.
const PROBE_FILE: &str = "acl-probe";

/// Archive one file with an ACL entry for user 0 and group 0 using the
/// writer's flags, and read back its member's text record.
///
/// The ids are 0 because every host names them (`root`): a tar that writes
/// names writes a name for a named id only, so an id the host does not name
/// would pass such a tar (#563).
fn acl_by_doing_it(tar: &Path, scratch: &Path) -> AclSupport {
    let dir = match tempfile::Builder::new()
        .prefix("tar-probe-")
        .tempdir_in(scratch)
    {
        Ok(d) => d,
        Err(e) => {
            return AclSupport::Unknown(format!(
                "cannot make a directory in {}: {e}",
                scratch.display()
            ))
        }
    };
    let root = dir.path().join("root");
    let file = root.join(PROBE_FILE);
    if let Err(e) = std::fs::create_dir(&root).and_then(|()| std::fs::write(&file, b"probe\n")) {
        return AclSupport::Unknown(format!("cannot make the probe file: {e}"));
    }
    if let Err(e) = set_access_acl(&file) {
        return AclSupport::Unknown(format!(
            "the file system under {} refused an ACL: {e}",
            scratch.display()
        ));
    }
    let list = match write_list(dir.path(), &[PROBE_FILE]) {
        Ok(l) => l,
        Err(e) => return AclSupport::Unknown(format!("cannot write the probe's list: {e}")),
    };
    let Some(binary) = tar.to_str() else {
        return AclSupport::Unknown(format!("tar's path is not UTF-8: {}", tar.display()));
    };
    let params = CreateParams {
        binary,
        root: &root,
        list: &list,
        xattrs: true,
        acls: true,
    };
    let mut run = match spawn_archive(&params) {
        Ok(r) => r,
        Err(e) => return AclSupport::Unknown(format!("cannot run tar: {e}")),
    };
    let mut stream = Vec::new();
    let read = run.take_stdout().read_to_end(&mut stream);
    let finish = run.finish();
    if let Err(e) = read {
        return AclSupport::Unknown(format!("cannot read tar's stream: {e}"));
    }
    let finish = match finish {
        Ok(f) => f,
        Err(e) => return AclSupport::Unknown(format!("tar did not finish: {e}")),
    };
    judge_stream(&stream, &finish)
}

/// The probe's verdict from tar's stream and exit.
fn judge_stream(stream: &[u8], finish: &TarFinish) -> AclSupport {
    let wanted = format!("./{PROBE_FILE}");
    let mut archive = ::tar::Archive::new(stream);
    // Read raw, so each pax extended header comes as an entry of its own
    // and its records are read from it: the member after it is the one
    // they describe.
    let mut record: Option<Option<String>> = None;
    if let Ok(mut entries) = archive.entries().map(|e| e.raw(true)) {
        let mut pending: Option<String> = None;
        for entry in entries.by_ref() {
            let Ok(mut entry) = entry else { break };
            if entry.header().entry_type() == ::tar::EntryType::XHeader {
                let mut data = Vec::new();
                pending = match entry.read_to_end(&mut data) {
                    Ok(_) => pax_record(&data, b"SCHILY.acl.access")
                        .map(|v| String::from_utf8_lossy(v).into_owned()),
                    Err(_) => None,
                };
                continue;
            }
            if entry.path_bytes().as_ref() == wanted.as_bytes() {
                record = Some(pending.take());
                break;
            }
            pending = None;
        }
    }
    match record {
        None => AclSupport::Unknown(format!(
            "tar wrote no member for the probe file (exit {}): {}",
            finish
                .code()
                .map_or_else(|| "by a signal".to_string(), |c| c.to_string()),
            finish.stderr().text().trim()
        )),
        Some(None) => AclSupport::No(
            "it stored no ACL record for a file that has one; a GNU tar built without \
             libacl does this"
                .into(),
        ),
        Some(Some(text)) => match acl_text_is_by_number(&text) {
            Ok(()) => AclSupport::Yes,
            Err(why) => AclSupport::No(why),
        },
    }
}

/// The value of `key` among pax extended-header records (`<length>
/// <key>=<value>\n`, the length counting the whole record). Read by length,
/// never by line: an ACL's text record holds newlines, and the `tar`
/// crate's `PaxExtensions` splits records at every newline, so it loses
/// that record (measured with tar 0.4.45 on GNU tar 1.35's stream).
pub fn pax_record<'a>(mut data: &'a [u8], key: &[u8]) -> Option<&'a [u8]> {
    while !data.is_empty() {
        let space = data.iter().position(|b| *b == b' ')?;
        let len: usize = std::str::from_utf8(&data[..space]).ok()?.parse().ok()?;
        if len <= space + 1 || len > data.len() || data[len - 1] != b'\n' {
            return None;
        }
        let record = &data[space + 1..len - 1];
        let eq = record.iter().position(|b| *b == b'=')?;
        if &record[..eq] == key {
            return Some(&record[eq + 1..]);
        }
        data = &data[len..];
    }
    None
}

/// Check an ACL text record: every qualifier a number, and the probe's
/// named entries (`user:0`, `group:0`) present.
pub fn acl_text_is_by_number(text: &str) -> std::result::Result<(), String> {
    let mut named_user = false;
    let mut named_group = false;
    for entry in text.split(['\n', ',']) {
        let entry = entry.split('#').next().unwrap_or("").trim();
        if entry.is_empty() {
            continue;
        }
        let mut fields = entry.split(':');
        let mut tag = fields.next().unwrap_or("");
        if tag == "default" || tag == "d" {
            tag = fields.next().unwrap_or("");
        }
        let qualifier = fields.next().unwrap_or("");
        let is_user = tag == "user" || tag == "u";
        let is_group = tag == "group" || tag == "g";
        if (is_user || is_group) && !qualifier.is_empty() {
            if !qualifier.bytes().all(|b| b.is_ascii_digit()) {
                return Err(format!(
                    "its ACL record names an account where a number belongs: \"{entry}\""
                ));
            }
            if qualifier == "0" {
                named_user |= is_user;
                named_group |= is_group;
            }
        }
    }
    if named_user && named_group {
        Ok(())
    } else {
        Err(format!(
            "its ACL record lost the entries for user 0 and group 0: \"{}\"",
            text.replace('\n', ",")
        ))
    }
}

/// Give `file` the access ACL `u::rw-,u:0:r--,g::r--,g:0:r--,m::r--,o::---`
/// by writing `system.posix_acl_access` in the kernel's own format
/// (`posix_acl_xattr.h`: a little-endian version 2, then each entry's tag,
/// permissions and id), so the probe needs no `setfacl`.
fn set_access_acl(file: &Path) -> std::io::Result<()> {
    use std::os::unix::ffi::OsStrExt;
    const UNDEFINED: u32 = u32::MAX;
    const USER_OBJ: u16 = 0x01;
    const USER: u16 = 0x02;
    const GROUP_OBJ: u16 = 0x04;
    const GROUP: u16 = 0x08;
    const MASK: u16 = 0x10;
    const OTHER: u16 = 0x20;
    let entries: [(u16, u16, u32); 6] = [
        (USER_OBJ, 6, UNDEFINED),
        (USER, 4, 0),
        (GROUP_OBJ, 4, UNDEFINED),
        (GROUP, 4, 0),
        (MASK, 4, UNDEFINED),
        (OTHER, 0, UNDEFINED),
    ];
    let mut value = 2u32.to_le_bytes().to_vec();
    for (tag, perm, id) in entries {
        value.extend_from_slice(&tag.to_le_bytes());
        value.extend_from_slice(&perm.to_le_bytes());
        value.extend_from_slice(&id.to_le_bytes());
    }
    let path = std::ffi::CString::new(file.as_os_str().as_bytes())
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidInput, e))?;
    // SAFETY: two NUL-terminated strings and a buffer of the length given,
    // all alive across the call.
    let rc = unsafe {
        nix::libc::setxattr(
            path.as_ptr(),
            c"system.posix_acl_access".as_ptr(),
            value.as_ptr().cast(),
            value.len(),
            0,
        )
    };
    if rc == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tar::stand_in;
    use tempfile::TempDir;

    fn v(major: u32, minor: u32, patch: Option<u32>) -> Option<TarVersion> {
        Some(TarVersion {
            major,
            minor,
            patch,
        })
    }

    /// The version table: GNU tar's lines, a distribution's suffix, bsdtar
    /// and busybox, and whether each meets the floor.
    #[test]
    fn version_lines_are_read_and_judged() {
        let table: [(&str, bool, Option<TarVersion>, bool); 9] = [
            ("tar (GNU tar) 1.30", true, v(1, 30, None), false),
            ("tar (GNU tar) 1.34", true, v(1, 34, None), false),
            ("tar (GNU tar) 1.35", true, v(1, 35, None), true),
            ("tar (GNU tar) 1.35-1ubuntu0.1", true, v(1, 35, None), true),
            (
                "tar (GNU tar) 1.34 (Debian 1.34+dfsg-1.2)",
                true,
                v(1, 34, None),
                false,
            ),
            ("tar (GNU tar) 1.35.90", true, v(1, 35, Some(90)), true),
            ("tar (GNU tar) 1.4", true, v(1, 4, None), false),
            (
                "bsdtar 3.6.2 - libarchive 3.6.2 zlib/1.2.13 liblzma/5.4.1 bz2lib/1.0.8",
                false,
                v(3, 6, Some(2)),
                false,
            ),
            ("tar (busybox) 1.36.1", false, v(1, 36, Some(1)), false),
        ];
        for (line, gnu, version, floor) in table {
            let (g, ver) = parse_first_line(line);
            assert_eq!((g, ver), (gnu, version), "{line}");
            let meets = g && ver.is_some_and(|v| v.meets_floor());
            assert_eq!(meets, floor, "{line}");
        }
    }

    fn check(first_line: &str, acl: AclSupport) -> TarCheck {
        let (gnu, version) = parse_first_line(first_line);
        TarCheck {
            path: PathBuf::from("/opt/bin/tar"),
            first_line: first_line.to_string(),
            gnu,
            version,
            meets_floor: gnu && version.is_some_and(|v| v.meets_floor()),
            acl,
        }
    }

    /// The refusal names what was found and the floor, for not GNU tar,
    /// below the floor, and ACL support `No`; `Yes` and `Unknown` at the
    /// floor are not refused.
    #[test]
    fn refusal_names_the_version_and_the_floor() {
        let busybox = check("tar (busybox) 1.36.1", AclSupport::Unknown("x".into()))
            .refusal()
            .unwrap();
        assert!(busybox.contains("is not GNU tar"), "{busybox}");
        assert!(busybox.contains("tar (busybox) 1.36.1"), "{busybox}");
        assert!(busybox.contains("GNU tar 1.35 or later"), "{busybox}");

        let old = check("tar (GNU tar) 1.34", AclSupport::Yes)
            .refusal()
            .unwrap();
        assert!(old.contains("GNU tar 1.34 at /opt/bin/tar"), "{old}");
        assert!(old.contains("below the writer's floor"), "{old}");
        assert!(
            old.contains("GNU tar 1.35 or later, built with ACL support"),
            "{old}"
        );

        let no = check("tar (GNU tar) 1.35", AclSupport::No("no record".into()))
            .refusal()
            .unwrap();
        assert!(no.contains("GNU tar 1.35 at /opt/bin/tar"), "{no}");
        assert!(
            no.contains("does not store an ACL by number (no record)"),
            "{no}"
        );

        assert_eq!(check("tar (GNU tar) 1.35", AclSupport::Yes).refusal(), None);
        assert_eq!(
            check("tar (GNU tar) 1.35", AclSupport::Unknown("tmpfs".into())).refusal(),
            None
        );
    }

    /// The ACL text: numbers pass; a name, or lost named entries, do not.
    #[test]
    fn acl_text_by_number_or_by_name() {
        assert_eq!(
            acl_text_is_by_number(
                "user::rw-\nuser:0:r--\ngroup::r--\ngroup:0:r--\nmask::r--\nother::---"
            ),
            Ok(())
        );
        assert_eq!(
            acl_text_is_by_number("u::rw-,u:0:r--,g::r--,g:0:r--,m::r--,o::---"),
            Ok(())
        );
        let named = acl_text_is_by_number(
            "user::rw-\nuser:root:r--\ngroup::r--\ngroup:root:r--\nmask::r--\nother::---",
        )
        .unwrap_err();
        assert!(named.contains("user:root:r--"), "{named}");
        let lost = acl_text_is_by_number("user::rw-\ngroup::r--\nother::---").unwrap_err();
        assert!(lost.contains("lost the entries"), "{lost}");
        let effective = acl_text_is_by_number(
            "user::rw-\nuser:0:rwx\t#effective:r--\ngroup::r--\ngroup:0:r--\nmask::r--\nother::---",
        );
        assert_eq!(effective, Ok(()));
    }

    /// pax records are read by their length, so a value with newlines is
    /// whole; a record whose length does not fit ends the read.
    #[test]
    fn pax_records_are_read_by_length() {
        let rec = |body: &str| {
            let mut n = body.len() + 3;
            if format!("{n} {body}").len() != n {
                n += 1;
            }
            format!("{n} {body}")
        };
        let data = format!(
            "{}{}",
            rec("mtime=1.5\n"),
            rec("SCHILY.acl.access=user::rw-\nuser:0:r--\nother::---\n")
        );
        assert_eq!(
            pax_record(data.as_bytes(), b"SCHILY.acl.access"),
            Some(&b"user::rw-\nuser:0:r--\nother::---"[..])
        );
        assert_eq!(pax_record(data.as_bytes(), b"mtime"), Some(&b"1.5"[..]));
        assert_eq!(pax_record(data.as_bytes(), b"path"), None);
        assert_eq!(pax_record(b"99 mtime=1\n", b"mtime"), None);
    }

    /// Probe through a stand-in, retrying the ETXTBSY a sibling test
    /// thread's fork can cause.
    fn probe_retrying(binary: &Path, scratch: &Path) -> TarCheck {
        for _ in 0..40 {
            match probe(binary.to_str().unwrap(), scratch) {
                Err(e) if e.to_string().contains("Text file busy") => {
                    std::thread::sleep(std::time::Duration::from_millis(50));
                }
                Ok(c) if matches!(&c.acl, AclSupport::Unknown(w) if w.contains("Text file busy")) =>
                {
                    std::thread::sleep(std::time::Duration::from_millis(50));
                }
                r => return r.unwrap(),
            }
        }
        panic!("the stand-in stayed busy");
    }

    /// The floor refusal with a stand-in that says `tar (GNU tar) 1.34`.
    #[test]
    fn a_gnu_tar_1_34_is_refused_naming_it_and_the_floor() {
        let tmp = TempDir::new().unwrap();
        let bin = stand_in(tmp.path(), "tar", "echo 'tar (GNU tar) 1.34'");
        let c = probe_retrying(&bin, tmp.path());
        assert!(c.gnu);
        assert_eq!(c.version, v(1, 34, None));
        assert!(!c.meets_floor);
        assert_eq!(c.first_line, "tar (GNU tar) 1.34");
        assert_eq!(c.path, bin);
        let refusal = c.refusal().expect("1.34 is refused");
        assert!(refusal.contains("GNU tar 1.34"), "{refusal}");
        assert!(refusal.contains("GNU tar 1.35 or later"), "{refusal}");
        // The stand-in writes no tar stream: the ACL test cannot tell.
        assert!(matches!(c.acl, AclSupport::Unknown(_)), "{:?}", c.acl);
    }

    /// A GNU tar at the floor that stores no ACL record (what one built
    /// without libacl does): ACL support `No`, refused. The stand-in runs
    /// the host's tar without `--acls`.
    #[test]
    fn a_tar_that_stores_no_acl_record_is_no() {
        let tmp = TempDir::new().unwrap();
        let bin = stand_in(
            tmp.path(),
            "tar",
            "if [ \"$1\" = --version ]; then echo 'tar (GNU tar) 1.35'; exit 0; fi\n\
             for a in \"$@\"; do shift; [ \"$a\" = --acls ] || set -- \"$@\" \"$a\"; done\n\
             exec tar \"$@\"",
        );
        let c = probe_retrying(&bin, tmp.path());
        assert!(c.meets_floor);
        assert!(matches!(c.acl, AclSupport::No(_)), "{:?}", c.acl);
        let refusal = c.refusal().expect("no ACL record is refused");
        assert!(
            refusal.contains("does not store an ACL by number"),
            "{refusal}"
        );
    }

    /// Not GNU tar: the ACL test is not run, and the tar is refused.
    #[test]
    fn a_tar_that_is_not_gnu_is_refused() {
        let tmp = TempDir::new().unwrap();
        let bin = stand_in(tmp.path(), "tar", "echo 'tar (busybox) 1.36.1'");
        let c = probe_retrying(&bin, tmp.path());
        assert!(!c.gnu);
        assert!(matches!(c.acl, AclSupport::Unknown(_)));
        assert!(c.refusal().unwrap().contains("is not GNU tar"));
    }

    /// A tar that is not there is `TarNotFound`, by path and by bare name.
    #[test]
    fn a_missing_tar_is_not_found() {
        let tmp = TempDir::new().unwrap();
        for name in ["/nonexistent/bin/tar", "tapectl-no-such-tar-on-path"] {
            match probe(name, tmp.path()) {
                Err(TapectlError::TarNotFound(n)) => assert_eq!(n, name),
                other => panic!("{name}: {other:?}"),
            }
        }
    }

    /// The host's real tar, found on `PATH` by its bare name: GNU tar at
    /// the floor with ACL support `Yes` (GNU tar 1.35 on this host and on
    /// CI's runner). Printed for the run record.
    #[test]
    fn the_hosts_tar_meets_the_floor_with_acl_support() {
        let tmp = TempDir::new().unwrap();
        let c = probe("tar", tmp.path()).unwrap();
        println!("{c}");
        assert!(c.path.is_absolute(), "{}", c.path.display());
        assert!(c.gnu, "{c}");
        assert!(c.meets_floor, "{c}");
        assert_eq!(c.acl, AclSupport::Yes, "{c}");
        assert_eq!(c.refusal(), None);
    }
}
