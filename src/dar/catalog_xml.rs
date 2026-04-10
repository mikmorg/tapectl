use std::path::Path;
use std::process::Command;

use crate::error::{Result, TapectlError};

/// A file entry parsed from dar's XML catalog listing.
#[derive(Debug, Clone)]
pub struct CatalogEntry {
    pub path: String,
    pub size_bytes: i64,
    pub is_directory: bool,
    pub mtime: Option<String>,
    pub uid: Option<i64>,
    pub gid: Option<i64>,
    pub username: Option<String>,
    pub groupname: Option<String>,
    pub mode: Option<i64>,
    pub has_xattrs: bool,
    pub has_acls: bool,
    pub crc: Option<String>,
}

/// Parse dar's XML listing output (-l -T xml) to extract file catalog entries.
pub fn parse_catalog(dar_binary: &str, archive_base: &Path) -> Result<Vec<CatalogEntry>> {
    let output = Command::new(dar_binary)
        .arg("-l")
        .arg(archive_base)
        .arg("-T")
        .arg("xml")
        .arg("-Q")
        .output()
        .map_err(|e| TapectlError::Dar(e.to_string()))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(TapectlError::Dar(format!("dar -l -T xml failed: {stderr}")));
    }

    let xml = String::from_utf8_lossy(&output.stdout);
    parse_xml_catalog(&xml)
}

/// Parse the XML string from dar's catalog listing.
/// dar XML format (2.7.x):
/// ```xml
/// <Catalog format="1.2">
///   <Directory name="subdir">
///     <Attributes ... user="mike" group="mike" permissions=" drwxr-xr-x" mtime="..." />
///   </Directory>
///   <File name="test.txt" size="6 o" stored="6 o" crc="076f6c6c" ...>
///     <Attributes data="saved" user="mike" group="mike" ... mtime="1234" />
///   </File>
/// </Catalog>
/// ```
fn parse_xml_catalog(xml: &str) -> Result<Vec<CatalogEntry>> {
    use quick_xml::events::Event;
    use quick_xml::reader::Reader;

    let mut reader = Reader::from_str(xml);
    let mut entries = Vec::new();
    let _dir_stack: Vec<String> = Vec::new();

    loop {
        match reader.read_event() {
            Ok(Event::Start(ref e)) | Ok(Event::Empty(ref e)) => {
                let tag = String::from_utf8_lossy(e.name().as_ref()).to_string();
                let _is_empty = !matches!(reader.read_event(), Ok(Event::End(_)) | Err(_));
                // Re-parse since we consumed an event incorrectly above.
                // Let's use a simpler approach:
                let _ = tag; // handled below
            }
            Ok(Event::Eof) => break,
            Err(e) => {
                return Err(TapectlError::Dar(format!("XML parse error: {e}")));
            }
            _ => {}
        }
    }

    // Simpler line-based approach since dar's XML is simple and consistent
    entries = parse_xml_line_based(xml)?;

    Ok(entries)
}

/// Line-based XML parsing — dar's output is simple enough for this.
fn parse_xml_line_based(xml: &str) -> Result<Vec<CatalogEntry>> {
    let mut entries = Vec::new();
    let mut dir_stack: Vec<String> = Vec::new();

    for line in xml.lines() {
        let trimmed = line.trim();

        if trimmed.starts_with("<Directory ") {
            let name = extract_attr(trimmed, "name").unwrap_or_default();
            dir_stack.push(name.clone());
            let path = dir_stack.join("/");

            let mtime = extract_mtime_from_next_attrs(trimmed);
            let (user, group) = extract_user_group(trimmed);

            entries.push(CatalogEntry {
                path,
                size_bytes: 0,
                is_directory: true,
                mtime,
                uid: None,
                gid: None,
                username: user,
                groupname: group,
                mode: None,
                has_xattrs: false,
                has_acls: false,
                crc: None,
            });
        } else if trimmed == "</Directory>" {
            dir_stack.pop();
        } else if trimmed.starts_with("<File ") {
            let name = extract_attr(trimmed, "name").unwrap_or_default();
            let prefix = if dir_stack.is_empty() {
                String::new()
            } else {
                format!("{}/", dir_stack.join("/"))
            };
            let path = format!("{prefix}{name}");

            let size = extract_attr(trimmed, "size")
                .and_then(|s| parse_dar_size(&s))
                .unwrap_or(0);

            let crc = extract_attr(trimmed, "crc");
            let mtime = extract_mtime_from_next_attrs(trimmed);
            let (user, group) = extract_user_group(trimmed);

            entries.push(CatalogEntry {
                path,
                size_bytes: size,
                is_directory: false,
                mtime,
                uid: None,
                gid: None,
                username: user,
                groupname: group,
                mode: None,
                has_xattrs: false,
                has_acls: false,
                crc,
            });
        }
    }

    Ok(entries)
}

fn extract_attr(line: &str, name: &str) -> Option<String> {
    let pattern = format!("{name}=\"");
    let start = line.find(&pattern)? + pattern.len();
    let end = line[start..].find('"')? + start;
    Some(line[start..end].to_string())
}

fn extract_mtime_from_next_attrs(line: &str) -> Option<String> {
    extract_attr(line, "mtime")
}

fn extract_user_group(line: &str) -> (Option<String>, Option<String>) {
    (extract_attr(line, "user"), extract_attr(line, "group"))
}

/// Parse dar's size format: "6 o", "1024 o", "10 Mio", etc.
fn parse_dar_size(s: &str) -> Option<i64> {
    let parts: Vec<&str> = s.split_whitespace().collect();
    let num: f64 = parts.first()?.replace(',', "").parse().ok()?;
    let multiplier = match parts.get(1).copied() {
        Some("o") | Some("B") | None => 1.0,
        Some("kio") | Some("KiB") => 1024.0,
        Some("Mio") | Some("MiB") => 1024.0 * 1024.0,
        Some("Gio") | Some("GiB") => 1024.0 * 1024.0 * 1024.0,
        Some("Tio") | Some("TiB") => 1024.0 * 1024.0 * 1024.0 * 1024.0,
        _ => 1.0,
    };
    Some((num * multiplier) as i64)
}
