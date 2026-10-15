//! The regular files a dar catalogue lists (issue #464): what `stage create`
//! compares with the files it hashed, to prove dar archived each of them.
//!
//! `dar -l <catalogue> -T xml` is the listing, because it is the one form a
//! program can read back exactly: directories nest as `<Directory
//! name="..">...</Directory>`, a regular file is `<File name=".." ..>`, and
//! every special character in a name is an entity (`& < > " '`), while
//! newlines, tabs and bytes that are not UTF-8 arrive raw, which the default
//! text listing cannot tell from its own layout. The output is read as it is
//! produced and never stored: a unit of a million files lists as hundreds of
//! megabytes of XML, and none of it is written anywhere (ADR-0012 2026-10-06
//! item 4: file names stay off the staging device).
//!
//! No XML crate: the listing has the four tags that matter, and a tag never
//! contains a raw `>` (it is `&gt;` in a name).

use std::io::{self, BufRead, BufReader, Read};
use std::path::Path;
use std::process::Stdio;

use crate::error::{Result, TapectlError};

/// Call `on_file` with the path, relative to the archived root, of every
/// regular file the dar catalogue at `catalogue` (a dar base name, as
/// `dar -l` takes it) lists. A hard link is listed under each of its names.
///
/// Fails when dar cannot list the catalogue, when the listing ends before
/// `</Catalog>`, or when tapectl is asked to stop: nothing is established
/// by a listing that was not read to its end.
pub fn regular_files(
    dar_binary: &str,
    catalogue: &Path,
    on_file: &mut dyn FnMut(&str),
) -> Result<()> {
    let mut child = super::command(dar_binary)
        // -N: no darrc (#412 item 9). -Q: no terminal to ask.
        .arg("-N")
        .arg("-l")
        .arg(catalogue)
        .arg("-T")
        .arg("xml")
        .arg("-Q")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| TapectlError::Dar(format!("cannot run dar: {e}")))?;
    let stderr = super::drain(child.stderr.take());
    let stdout = super::create::DarOutput::over(child.stdout.take().expect("dar's stdout"));
    let scanned = scan(stdout, on_file);
    if scanned.is_err() {
        super::terminate(&mut child);
    }
    let status = child.wait();
    let stderr = stderr.join().unwrap_or_default();
    if crate::signal::is_interrupted() {
        return Err(TapectlError::Interrupted(
            "dar was stopped while listing the stage set's catalogue".into(),
        ));
    }
    let status = status?;
    let excerpt = || {
        String::from_utf8_lossy(&stderr)
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty())
            .take(5)
            .collect::<Vec<_>>()
            .join("\n")
    };
    if !status.success() {
        return Err(TapectlError::Dar(format!(
            "dar -l {} failed (exit {status}): {}",
            catalogue.display(),
            excerpt()
        )));
    }
    scanned.map_err(|e| TapectlError::Dar(format!("reading dar's catalogue listing: {e}")))
}

/// [`regular_files`] over a listing already in hand: the XML of `dar -l -T
/// xml`, read from `listing`.
fn scan<R: Read>(listing: R, on_file: &mut dyn FnMut(&str)) -> io::Result<()> {
    let mut listing = BufReader::with_capacity(1 << 16, listing);
    let mut tag = Vec::new();
    // The directories the listing is inside of, outermost first.
    let mut dirs: Vec<String> = Vec::new();
    let mut path = String::new();
    loop {
        // Between tags: indentation, newlines, nothing of interest.
        if listing.skip_until(b'<')? == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "the listing ended before </Catalog>",
            ));
        }
        tag.clear();
        listing.read_until(b'>', &mut tag)?;
        if tag.last() != Some(&b'>') {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "the listing ended inside a tag",
            ));
        }
        tag.pop();
        let self_closing = tag.last() == Some(&b'/');
        match tag.first() {
            Some(b'/') => match &tag[1..] {
                b"Directory" => {
                    dirs.pop();
                }
                b"Catalog" => return Ok(()),
                _ => {}
            },
            Some(b'?' | b'!') | None => {}
            Some(_) => {
                let element_end = tag
                    .iter()
                    .position(|b| b.is_ascii_whitespace() || *b == b'/')
                    .unwrap_or(tag.len());
                match &tag[..element_end] {
                    b"Directory" if !self_closing => {
                        dirs.push(name_of(&tag).unwrap_or_default());
                    }
                    b"File" => {
                        if let Some(name) = name_of(&tag) {
                            path.clear();
                            for dir in &dirs {
                                path.push_str(dir);
                                path.push('/');
                            }
                            path.push_str(&name);
                            on_file(&path);
                        }
                    }
                    _ => {}
                }
            }
        }
    }
}

/// The `name` attribute of the tag's element, with its entities decoded.
/// Bytes that are not UTF-8 become U+FFFD: tapectl refuses such a name at
/// `snapshot create`, so the one it would have hashed cannot be this one.
fn name_of(tag: &[u8]) -> Option<String> {
    const KEY: &[u8] = b" name=\"";
    let at = tag.windows(KEY.len()).position(|w| w == KEY)? + KEY.len();
    let len = tag[at..].iter().position(|b| *b == b'"')?;
    Some(unescape(&tag[at..at + len]))
}

/// `&amp; &lt; &gt; &quot; &apos;` and numeric references; any other `&` is
/// kept as it is, which cannot match a path tapectl hashed, so it is
/// reported rather than guessed at.
fn unescape(raw: &[u8]) -> String {
    let mut out = Vec::with_capacity(raw.len());
    let mut i = 0;
    while i < raw.len() {
        if raw[i] == b'&' {
            if let Some(end) = raw[i..].iter().take(12).position(|b| *b == b';') {
                let entity = &raw[i + 1..i + end];
                let decoded: Option<char> = match entity {
                    b"amp" => Some('&'),
                    b"lt" => Some('<'),
                    b"gt" => Some('>'),
                    b"quot" => Some('"'),
                    b"apos" => Some('\''),
                    [b'#', b'x' | b'X', hex @ ..] => std::str::from_utf8(hex)
                        .ok()
                        .and_then(|h| u32::from_str_radix(h, 16).ok())
                        .and_then(char::from_u32),
                    [b'#', dec @ ..] => std::str::from_utf8(dec)
                        .ok()
                        .and_then(|d| d.parse::<u32>().ok())
                        .and_then(char::from_u32),
                    _ => None,
                };
                if let Some(c) = decoded {
                    let mut buf = [0u8; 4];
                    out.extend_from_slice(c.encode_utf8(&mut buf).as_bytes());
                    i += end + 1;
                    continue;
                }
            }
        }
        out.push(raw[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn files_of(xml: &[u8]) -> io::Result<Vec<String>> {
        let mut found = Vec::new();
        scan(xml, &mut |p| found.push(p.to_string()))?;
        Ok(found)
    }

    #[test]
    fn files_are_listed_under_their_directories_and_nothing_else_is() {
        let xml = b"<?xml version=\"1.0\" ?>\n\
            <!DOCTYPE Catalog SYSTEM \"dar-catalog.dtd\">\n\
            <Catalog format=\"1.2\">\n\
            <File name=\"top.txt\" size=\"1 o\">\n<Attributes data=\"saved\" />\n</File>\n\
            <Pipe name=\"fifo\">\n<Attributes data=\"saved\" />\n</Pipe>\n\
            <Symlink name=\"link\" target=\"top.txt\">\n</Symlink>\n\
            <Directory name=\"a\">\n<Attributes data=\"saved\" />\n\
            \t<Directory name=\"b\">\n\
            \t\t<File name=\"deep.bin\" size=\"2 o\">\n</File>\n\
            \t</Directory>\n\
            \t<File name=\"mid.txt\" size=\"1 o\">\n</File>\n\
            </Directory>\n\
            <Directory name=\"empty\">\n</Directory>\n\
            <File name=\"after.txt\" size=\"1 o\">\n</File>\n\
            </Catalog>\n";
        assert_eq!(
            files_of(xml).unwrap(),
            ["top.txt", "a/b/deep.bin", "a/mid.txt", "after.txt"]
        );
    }

    /// A name carries every awkward byte a file name can: entities for the
    /// markup characters, raw newlines and tabs, bytes that are not UTF-8.
    #[test]
    fn names_are_decoded_exactly() {
        let xml = b"<Catalog>\n\
            <File name=\"a&lt;b&gt;c&amp;d&quot;e&apos;f.txt\" size=\"0\">\n</File>\n\
            <File name=\"new\nline.txt\" size=\"0\">\n</File>\n\
            <File name=\"tab\tname.txt\" size=\"0\">\n</File>\n\
            <File name=\"x&#65;&#x42;.txt\" size=\"0\">\n</File>\n\
            <File name=\"unknown&entity;.txt\" size=\"0\">\n</File>\n\
            <File name=\"\xe9latin.txt\" size=\"0\">\n</File>\n\
            <Directory name=\"di&lt;r&gt;\">\n<File name=\"in.txt\" size=\"0\">\n</File>\n</Directory>\n\
            </Catalog>\n";
        assert_eq!(
            files_of(xml).unwrap(),
            [
                "a<b>c&d\"e'f.txt",
                "new\nline.txt",
                "tab\tname.txt",
                "xAB.txt",
                "unknown&entity;.txt",
                "\u{fffd}latin.txt",
                "di<r>/in.txt",
            ]
        );
    }

    /// Against real dar, on the archive itself and on the isolated catalogue
    /// `stage create` lists: every regular file under its path, a hard link
    /// under each of its names, and not the directory, symlink or FIFO.
    #[test]
    fn real_dar_lists_the_regular_files_of_an_archive_and_of_its_isolated_catalogue() {
        use std::os::unix::fs::symlink;
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("src");
        let names = [
            "plain.txt",
            "sp ace/inner file.bin",
            "new\nline.txt",
            "q\"uote&amp;<x>'.txt",
            "tab\tname",
            "deep/er/est/file",
            "hard/one",
        ];
        for name in names {
            let path = src.join(name);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, name.as_bytes()).unwrap();
        }
        std::fs::create_dir_all(src.join("hard2")).unwrap();
        std::fs::hard_link(src.join("hard/one"), src.join("hard2/two")).unwrap();
        std::fs::create_dir(src.join("empty")).unwrap();
        symlink("plain.txt", src.join("link")).unwrap();
        nix::unistd::mkfifo(&src.join("fifo"), nix::sys::stat::Mode::S_IRWXU).unwrap();

        let dar = |args: &[&std::ffi::OsStr]| {
            let out = std::process::Command::new("dar")
                .args(args)
                .arg("-N")
                .arg("-Q")
                .output()
                .expect("dar must be on PATH (tests/test_dependencies.rs)");
            assert!(
                out.status.success(),
                "{}",
                String::from_utf8_lossy(&out.stderr)
            );
        };
        let archive = tmp.path().join("archive");
        let isolated = tmp.path().join("isolated");
        dar(&[
            "-c".as_ref(),
            archive.as_os_str(),
            "-R".as_ref(),
            src.as_os_str(),
            "-an".as_ref(),
        ]);
        dar(&[
            "-C".as_ref(),
            isolated.as_os_str(),
            "-A".as_ref(),
            archive.as_os_str(),
            "-znone".as_ref(),
        ]);

        let mut expected: Vec<String> = names.iter().map(|n| n.to_string()).collect();
        expected.push("hard2/two".to_string());
        expected.sort();
        for base in [&archive, &isolated] {
            let mut listed = Vec::new();
            regular_files("dar", base, &mut |p| listed.push(p.to_string())).unwrap();
            listed.sort();
            assert_eq!(listed, expected, "{}", base.display());
        }
    }

    /// dar failing is an error, not an empty list.
    #[test]
    fn a_catalogue_dar_cannot_read_is_an_error() {
        let tmp = tempfile::tempdir().unwrap();
        let err = regular_files("dar", &tmp.path().join("no-such-catalogue"), &mut |_| {})
            .expect_err("nothing to list");
        assert!(err.to_string().contains("dar -l"), "{err}");
    }

    /// A listing that stops early is an error, never a short list: a missing
    /// tail would read as files dar did not archive.
    #[test]
    fn a_listing_that_ends_early_is_an_error() {
        for cut in [
            &b"<Catalog>\n<File name=\"a\" size=\"0\">\n</File>\n"[..],
            &b"<Catalog>\n<File name=\"a\" si"[..],
            &b""[..],
        ] {
            let err = files_of(cut).expect_err("no </Catalog>");
            assert_eq!(err.kind(), io::ErrorKind::UnexpectedEof, "{err}");
        }
    }
}
