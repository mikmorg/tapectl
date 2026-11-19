//! A pax extended header's records, and the ACL text they carry
//! (`volume-format-v3.md` §3.2; ADR-0012 items 270, 274; #563).
//!
//! **Records are read by length, never by line.** Each record is
//! `<length> <key>=<value>\n`, the length counting the whole record in
//! bytes. An ACL's text record (`SCHILY.acl.access`, `SCHILY.acl.default`)
//! holds newlines inside its value, and a value may hold any byte (a name
//! that is not UTF-8 comes with `hdrcharset=BINARY`), so only the length
//! says where a record ends. The `tar` crate's `PaxExtensions` (0.4.45)
//! splits records at every newline and loses the ACL record GNU tar 1.35
//! writes; `crate::tar::probe::pax_record` reads by length for the same
//! reason.

use crate::idmap::IdKind;

/// The keys the stream reader reads.
pub const PATH: &[u8] = b"path";
pub const LINKPATH: &[u8] = b"linkpath";
pub const SIZE: &[u8] = b"size";
pub const UID: &[u8] = b"uid";
pub const GID: &[u8] = b"gid";
pub const UNAME: &[u8] = b"uname";
pub const GNAME: &[u8] = b"gname";
pub const ACL_ACCESS: &[u8] = b"SCHILY.acl.access";
pub const ACL_DEFAULT: &[u8] = b"SCHILY.acl.default";

/// The records of one extended header's data, in order: each `(key,
/// value)`, or the reason the data cannot be read as records.
///
/// The data is the header's `size` bytes, without the padding to the next
/// block. A record whose length is not a decimal number, runs past the
/// data, does not end in a newline or has no `=` is an error, and the
/// iterator ends after it.
pub fn records(data: &[u8]) -> Records<'_> {
    Records {
        data,
        failed: false,
    }
}

/// The iterator [`records`] returns.
pub struct Records<'a> {
    data: &'a [u8],
    failed: bool,
}

impl<'a> Iterator for Records<'a> {
    type Item = Result<(&'a [u8], &'a [u8]), String>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.failed || self.data.is_empty() {
            return None;
        }
        let r = one_record(self.data);
        match r {
            Ok((key, value, rest)) => {
                self.data = rest;
                Some(Ok((key, value)))
            }
            Err(e) => {
                self.failed = true;
                Some(Err(e))
            }
        }
    }
}

type Split<'a> = (&'a [u8], &'a [u8], &'a [u8]);

fn one_record(data: &[u8]) -> Result<Split<'_>, String> {
    let space = data
        .iter()
        .take(20)
        .position(|b| *b == b' ')
        .ok_or_else(|| "a record's length is not followed by a space".to_string())?;
    let digits = &data[..space];
    if digits.is_empty() || !digits.iter().all(u8::is_ascii_digit) {
        return Err(format!(
            "a record's length is not a decimal number: {:?}",
            crate::volume::index::escape_bytes(digits)
        ));
    }
    let len: usize = std::str::from_utf8(digits)
        .ok()
        .and_then(|s| s.parse().ok())
        .ok_or_else(|| "a record's length is too large".to_string())?;
    if len <= space + 1 || len > data.len() {
        return Err(format!(
            "a record's length {len} does not fit the {} bytes left",
            data.len()
        ));
    }
    if data[len - 1] != b'\n' {
        return Err(format!(
            "the record of {len} bytes does not end in a newline"
        ));
    }
    let record = &data[space + 1..len - 1];
    let eq = record
        .iter()
        .position(|b| *b == b'=')
        .ok_or_else(|| "a record has no `=`".to_string())?;
    Ok((&record[..eq], &record[eq + 1..], &data[len..]))
}

/// A record's value as a decimal number (`size`, `uid`, `gid`).
pub fn number(value: &[u8]) -> Option<u64> {
    if value.is_empty() || !value.iter().all(u8::is_ascii_digit) {
        return None;
    }
    std::str::from_utf8(value).ok()?.parse().ok()
}

/// Read an ACL text record (`user::rw-,user:1:r--,group::r--,mask::r--,
/// other::r--`, its entries separated by commas or newlines, `#` starting a
/// comment to the end of its entry): every user and group id it names, in
/// order, or why it is refused.
///
/// An empty qualifier is the owner's or owning group's own entry and names
/// no id. A non-empty one must be all digits and fit a 32-bit id: a name
/// there is what GNU tar below 1.35 writes with `--numeric-owner`, and is
/// refused (items 270, 274). A tag other than `user`, `group`, `mask` and
/// `other` (or their one-letter forms, after an optional `default:`) is
/// refused too, since its qualifier cannot be judged.
pub fn acl_ids(text: &[u8]) -> Result<Vec<(IdKind, u32)>, String> {
    let mut ids = Vec::new();
    for entry in text.split(|b| *b == b'\n' || *b == b',') {
        let entry = match entry.iter().position(|b| *b == b'#') {
            Some(hash) => &entry[..hash],
            None => entry,
        };
        let entry = entry.trim_ascii();
        if entry.is_empty() {
            continue;
        }
        let shown = || crate::volume::index::escape_bytes(entry).into_owned();
        let mut fields = entry.split(|b| *b == b':');
        let mut tag = fields.next().unwrap_or_default();
        if tag == b"default" || tag == b"d" {
            tag = fields.next().unwrap_or_default();
        }
        let qualifier = fields.next().unwrap_or_default().trim_ascii();
        let kind = match tag {
            b"user" | b"u" => Some(IdKind::User),
            b"group" | b"g" => Some(IdKind::Group),
            b"mask" | b"m" | b"other" | b"o" => None,
            _ => return Err(format!("an ACL entry of an unknown kind: \"{}\"", shown())),
        };
        if qualifier.is_empty() {
            continue;
        }
        if !qualifier.iter().all(u8::is_ascii_digit) {
            return Err(format!(
                "an ACL entry names an account where a number belongs: \"{}\"",
                shown()
            ));
        }
        let id: u32 = std::str::from_utf8(qualifier)
            .ok()
            .and_then(|s| s.parse().ok())
            .ok_or_else(|| format!("an ACL entry's id is not a 32-bit id: \"{}\"", shown()))?;
        match kind {
            Some(kind) => ids.push((kind, id)),
            None => {
                return Err(format!(
                    "an ACL entry that takes no qualifier has one: \"{}\"",
                    shown()
                ))
            }
        }
    }
    Ok(ids)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rec(key: &str, value: &[u8]) -> Vec<u8> {
        // The length counts itself: find the fixed point.
        let body = key.len() + 1 + value.len() + 1;
        let mut len = body + 2;
        while len != body + 1 + len.to_string().len() {
            len = body + 1 + len.to_string().len();
        }
        let mut out = format!("{len} {key}=").into_bytes();
        out.extend_from_slice(value);
        out.push(b'\n');
        assert_eq!(out.len(), len);
        out
    }

    /// Records are split by their length, so an ACL's newlines and a
    /// binary name stay inside their values.
    #[test]
    fn records_are_read_by_length() {
        let acl = b"user::rw-\nuser:1:r--\ngroup::r--\nmask::r--\nother::r--\n";
        let mut data = rec("mtime", b"1.5");
        data.extend(rec("SCHILY.acl.access", acl));
        data.extend(rec("path", b"./a\nb\xff=c"));
        let got: Vec<(Vec<u8>, Vec<u8>)> = records(&data)
            .map(|r| r.map(|(k, v)| (k.to_vec(), v.to_vec())).unwrap())
            .collect();
        assert_eq!(
            got,
            vec![
                (b"mtime".to_vec(), b"1.5".to_vec()),
                (ACL_ACCESS.to_vec(), acl.to_vec()),
                (PATH.to_vec(), b"./a\nb\xff=c".to_vec()),
            ]
        );
    }

    /// A malformed record is an error, and the iteration ends there.
    #[test]
    fn malformed_records_are_errors() {
        for bad in [
            &b"x path=a\n"[..],
            b"99 path=a\n",
            b"10 path=abc",
            b"9 pathabc\n",
            b"3 a\n",
            b"nospace",
        ] {
            let all: Vec<_> = records(bad).collect();
            assert_eq!(all.len(), 1, "{:?}", String::from_utf8_lossy(bad));
            assert!(all[0].is_err(), "{:?}", String::from_utf8_lossy(bad));
        }
        let mut data = rec("size", b"5");
        data.extend_from_slice(b"7 junk\n");
        let all: Vec<_> = records(&data).collect();
        assert!(all[0].is_ok() && all[1].is_err() && all.len() == 2);
    }

    #[test]
    fn numbers_are_decimal_digits_only() {
        assert_eq!(number(b"0"), Some(0));
        assert_eq!(number(b"103079215104"), Some(103_079_215_104));
        for bad in [&b""[..], b"-1", b"1.5", b" 1", b"0x10"] {
            assert_eq!(number(bad), None);
        }
    }

    /// The ACL text forms: commas or newlines, the owner's own entries,
    /// `default:`, comments, and every named id collected.
    #[test]
    fn acl_ids_collects_numeric_qualifiers() {
        assert_eq!(
            acl_ids(b"user::rw-,user:1:r--,group::r--,group:2:r--,mask::r--,other::r--").unwrap(),
            vec![(IdKind::User, 1), (IdKind::Group, 2)]
        );
        assert_eq!(
            acl_ids(
                b"user::rwx\nuser:4000000:r-x\t#effective:r-x\ngroup::rwx\nmask::rwx\nother::r-x\n"
            )
            .unwrap(),
            vec![(IdKind::User, 4_000_000)]
        );
        assert_eq!(
            acl_ids(b"default:user:7:r--,d:g:8:r--,u::rw-,o::---").unwrap(),
            vec![(IdKind::User, 7), (IdKind::Group, 8)]
        );
        assert_eq!(acl_ids(b"").unwrap(), vec![]);
    }

    /// A name where a number belongs is refused; so is an id past 32 bits,
    /// a qualifier on an entry that takes none, and an unknown tag.
    #[test]
    fn acl_ids_refuses_names_and_what_it_cannot_judge() {
        for (bad, says) in [
            (&b"user::rw-\nuser:daemon:r--\n"[..], "names an account"),
            (b"group:bin:r--", "names an account"),
            (b"user:4294967296:r--", "not a 32-bit id"),
            (b"mask:1:r--", "takes no qualifier"),
            (b"weird:1:r--", "unknown kind"),
        ] {
            let err = acl_ids(bad).unwrap_err();
            assert!(err.contains(says), "{err}");
        }
    }
}
