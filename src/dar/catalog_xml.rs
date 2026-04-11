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
// dar's XML output is line-oriented and consistent across 2.6/2.7, so a
// line-based parser is simpler and more robust than a quick-xml state machine.
fn parse_xml_catalog(xml: &str) -> Result<Vec<CatalogEntry>> {
    parse_xml_line_based(xml)
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

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE_XML: &str = r#"
<Catalog format="1.2">
  <Directory name="subdir">
    <Attributes user="mike" group="mike" permissions=" drwxr-xr-x" mtime="1700000000" />
    <File name="inside.txt" size="6 o" stored="6 o" crc="076f6c6c">
      <Attributes data="saved" user="mike" group="mike" mtime="1700000100" />
    </File>
  </Directory>
  <File name="top.bin" size="10 Mio" crc="abcdef01">
    <Attributes data="saved" user="root" group="wheel" mtime="1700000200" />
  </File>
</Catalog>
"#;

    #[test]
    fn parse_directory_and_file() {
        let entries = parse_xml_catalog(SAMPLE_XML).unwrap();
        assert_eq!(entries.len(), 3);

        let subdir = &entries[0];
        assert!(subdir.is_directory);
        assert_eq!(subdir.path, "subdir");

        let inside = &entries[1];
        assert!(!inside.is_directory);
        assert_eq!(inside.path, "subdir/inside.txt");
        assert_eq!(inside.size_bytes, 6);
        assert_eq!(inside.crc.as_deref(), Some("076f6c6c"));

        let top = &entries[2];
        assert!(!top.is_directory);
        assert_eq!(top.path, "top.bin");
        assert_eq!(top.size_bytes, 10 * 1024 * 1024);
        assert_eq!(top.crc.as_deref(), Some("abcdef01"));
    }

    #[test]
    fn parse_empty_catalog() {
        let xml = r#"<Catalog format="1.2"></Catalog>"#;
        let entries = parse_xml_catalog(xml).unwrap();
        assert!(entries.is_empty());
    }

    #[test]
    fn parse_dar_size_units() {
        assert_eq!(parse_dar_size("6 o"), Some(6));
        assert_eq!(parse_dar_size("1024 o"), Some(1024));
        assert_eq!(parse_dar_size("10 Mio"), Some(10 * 1024 * 1024));
        assert_eq!(parse_dar_size("2 Gio"), Some(2 * 1024 * 1024 * 1024));
        assert_eq!(parse_dar_size("1 TiB"), Some(1024i64.pow(4)));
        assert_eq!(parse_dar_size("not a number"), None);
    }

    #[test]
    fn extract_attr_basic() {
        let line = r#"<File name="test.txt" size="6 o" crc="abc" />"#;
        assert_eq!(extract_attr(line, "name").as_deref(), Some("test.txt"));
        assert_eq!(extract_attr(line, "size").as_deref(), Some("6 o"));
        assert_eq!(extract_attr(line, "crc").as_deref(), Some("abc"));
        assert_eq!(extract_attr(line, "missing"), None);
    }

    #[test]
    fn nested_directories() {
        let xml = r#"
<Catalog format="1.2">
  <Directory name="a">
    <Attributes user="u" group="g" />
    <Directory name="b">
      <Attributes user="u" group="g" />
      <File name="deep.txt" size="1 o">
        <Attributes user="u" group="g" />
      </File>
    </Directory>
  </Directory>
</Catalog>
"#;
        let entries = parse_xml_catalog(xml).unwrap();
        let deep = entries
            .iter()
            .find(|e| e.path.ends_with("deep.txt"))
            .expect("deep file present");
        assert_eq!(deep.path, "a/b/deep.txt");
    }
}
