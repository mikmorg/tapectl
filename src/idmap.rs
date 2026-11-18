//! A staging's id map (`volume-format-v3.md` §1.6; ADR-0012 items 272, 276,
//! 277, 278; the plan's D5 and D13): every user id and group id its stream
//! carries, as a member's owner or group and in an ACL entry, with the name
//! the host gave that id when the stream was written, or none.
//!
//! The stream holds numbers only (item 270); the map holds their names, so
//! that a reader can see who an id was. A restore shows the map and never
//! acts on it (item 278). The catalog's copy is `db::stage_ids`.
//!
//! The names come from the C library's lookup by id (`getpwuid_r`,
//! `getgrgid_r`, through `nix`), each distinct id looked up once
//! ([`Collector`]): a directory service that answers slowly costs one wait
//! per id, never one per member.

use std::collections::BTreeMap;
use std::io::{BufRead, Write};

use nix::unistd::{Gid, Group, Uid, User};

use crate::error::{Result, TapectlError};
use crate::volume::index::{escape, unescape};

/// The map's first line (item 294).
pub const HEADER: &str = "#tapectl-id-map 1";

/// Whose id: a user's or a group's. The order is the text's: `u` before `g`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum IdKind {
    User,
    Group,
}

impl IdKind {
    /// The text's (and the catalog's) letter.
    pub fn letter(self) -> &'static str {
        match self {
            IdKind::User => "u",
            IdKind::Group => "g",
        }
    }

    /// The inverse of [`IdKind::letter`].
    pub fn from_letter(letter: &str) -> Option<Self> {
        match letter {
            "u" => Some(IdKind::User),
            "g" => Some(IdKind::Group),
            _ => None,
        }
    }
}

/// One id of a staging's stream and the name the host gave it. `None`: the
/// host knew no name for it (never the empty string).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IdRow {
    pub kind: IdKind,
    pub id: u32,
    pub name: Option<String>,
}

/// The name this host gives user id `uid`, by the C library's lookup.
/// `None` when it knows none, or the lookup fails (which is warned).
pub fn user_name(uid: u32) -> Option<String> {
    named(
        IdKind::User,
        uid,
        User::from_uid(Uid::from_raw(uid)).map(|u| u.map(|u| u.name)),
    )
}

/// The name this host gives group id `gid`, as [`user_name`].
pub fn group_name(gid: u32) -> Option<String> {
    named(
        IdKind::Group,
        gid,
        Group::from_gid(Gid::from_raw(gid)).map(|g| g.map(|g| g.name)),
    )
}

/// The name of `id` of `kind` on this host.
pub fn lookup(kind: IdKind, id: u32) -> Option<String> {
    match kind {
        IdKind::User => user_name(id),
        IdKind::Group => group_name(id),
    }
}

fn named(kind: IdKind, id: u32, found: nix::Result<Option<String>>) -> Option<String> {
    match found {
        Ok(name) => name.filter(|n| !n.is_empty()),
        Err(e) => {
            tracing::warn!(
                "looking up the name of {} id {id} failed ({e}); the id map records none",
                match kind {
                    IdKind::User => "user",
                    IdKind::Group => "group",
                }
            );
            None
        }
    }
}

/// Gathers a stream's ids as they are seen and names each distinct one
/// once. [`Collector::rows`] gives the map in the text's order.
pub struct Collector<L = fn(IdKind, u32) -> Option<String>> {
    lookup: L,
    seen: BTreeMap<(IdKind, u32), Option<String>>,
}

impl Collector {
    /// A collector that names ids through the C library ([`lookup`]).
    pub fn new() -> Self {
        Collector::with_lookup(lookup)
    }
}

impl Default for Collector {
    fn default() -> Self {
        Collector::new()
    }
}

impl<L: FnMut(IdKind, u32) -> Option<String>> Collector<L> {
    /// A collector that names ids with `lookup` (a test's stand-in).
    pub fn with_lookup(lookup: L) -> Self {
        Collector {
            lookup,
            seen: BTreeMap::new(),
        }
    }

    /// Record that the stream carries `id` of `kind`, looking its name up
    /// the first time only.
    pub fn see(&mut self, kind: IdKind, id: u32) {
        if let std::collections::btree_map::Entry::Vacant(slot) = self.seen.entry((kind, id)) {
            let name = (self.lookup)(kind, id).filter(|n| !n.is_empty());
            slot.insert(name);
        }
    }

    /// The map: every id seen, users before groups, each by number.
    pub fn rows(self) -> Vec<IdRow> {
        self.seen
            .into_iter()
            .map(|((kind, id), name)| IdRow { kind, id, name })
            .collect()
    }
}

fn bad(what: &str, detail: impl std::fmt::Display) -> TapectlError {
    TapectlError::Other(format!("id map: {what}: {detail}"))
}

/// Rows must be sorted by kind and then number, each id once.
fn in_order(before: Option<(IdKind, u32)>, row: &IdRow) -> std::result::Result<(), String> {
    match before {
        Some(b) if b >= (row.kind, row.id) => Err(format!(
            "`{} {}` is not after `{} {}` (sorted by kind, `u` first, then by number, each once)",
            row.kind.letter(),
            row.id,
            b.0.letter(),
            b.1
        )),
        _ => Ok(()),
    }
}

/// Write the map as the format's text: the header line, then one
/// `<kind>\t<id>\t<name>` row for each id, the name through
/// `volume::index::escape` and empty when the host knew none. Rows out of
/// order are refused.
pub fn write<'a, W: Write + ?Sized>(
    w: &mut W,
    rows: impl IntoIterator<Item = &'a IdRow>,
) -> Result<()> {
    writeln!(w, "{HEADER}")?;
    let mut before = None;
    for (i, row) in rows.into_iter().enumerate() {
        in_order(before, row).map_err(|e| bad(&format!("row {}", i + 1), e))?;
        if row.name.as_deref() == Some("") {
            return Err(bad(
                &format!("row {}", i + 1),
                "an empty name is written as none",
            ));
        }
        writeln!(
            w,
            "{}\t{}\t{}",
            row.kind.letter(),
            row.id,
            escape(row.name.as_deref().unwrap_or(""))
        )?;
        before = Some((row.kind, row.id));
    }
    Ok(())
}

/// Parse a map's text, refusing a bad header, an unknown kind, an id that
/// is not a 32-bit number, rows out of order, a bad escape, and a text cut
/// inside a line.
pub fn parse<R: BufRead>(mut r: R) -> Result<Vec<IdRow>> {
    let mut out = Vec::new();
    let mut buf = Vec::new();
    let mut before = None;
    for n in 1.. {
        buf.clear();
        if r.read_until(b'\n', &mut buf)? == 0 {
            if n == 1 {
                return Err(bad("line 1", format!("not `{HEADER}`: the text is empty")));
            }
            break;
        }
        let line = format!("line {n}");
        if buf.pop() != Some(b'\n') {
            return Err(bad(&line, "the text ends inside a line"));
        }
        let text = std::str::from_utf8(&buf).map_err(|_| bad(&line, "not UTF-8"))?;
        if n == 1 {
            if text != HEADER {
                return Err(bad(&line, format!("not `{HEADER}`: {text:?}")));
            }
            continue;
        }
        let fields: Vec<&str> = text.split('\t').collect();
        if fields.len() != 3 {
            return Err(bad(&line, format!("{} field(s), not 3", fields.len())));
        }
        let kind = IdKind::from_letter(fields[0])
            .ok_or_else(|| bad(&line, format!("unknown kind {:?}", fields[0])))?;
        let canonical = !fields[1].is_empty()
            && fields[1].bytes().all(|b| b.is_ascii_digit())
            && (fields[1] == "0" || !fields[1].starts_with('0'));
        let id: u32 = canonical
            .then(|| fields[1].parse().ok())
            .flatten()
            .ok_or_else(|| bad(&line, format!("not an id: {:?}", fields[1])))?;
        let name = unescape(fields[2]).map_err(|e| bad(&line, e))?;
        let row = IdRow {
            kind,
            id,
            name: (!name.is_empty()).then_some(name),
        };
        in_order(before, &row).map_err(|e| bad(&line, e))?;
        before = Some((row.kind, row.id));
        out.push(row);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(kind: IdKind, id: u32, name: Option<&str>) -> IdRow {
        IdRow {
            kind,
            id,
            name: name.map(str::to_string),
        }
    }

    #[test]
    fn this_processs_own_ids_have_names_and_an_id_far_above_any_real_one_has_none() {
        let uid = nix::unistd::getuid().as_raw();
        let gid = nix::unistd::getgid().as_raw();
        let user = user_name(uid).expect("this process's user has a name");
        assert!(!user.is_empty());
        assert!(group_name(gid).is_some(), "this process's group has a name");
        // Not u32::MAX (-1, "no id") nor 65534 (nobody): an id no host gives.
        let huge = 3_999_999_999;
        assert_eq!(user_name(huge), None);
        assert_eq!(group_name(huge), None);
        assert_eq!(lookup(IdKind::User, uid), Some(user));
    }

    #[test]
    fn each_distinct_id_is_looked_up_once_and_the_map_is_in_the_texts_order() {
        let mut asked = Vec::new();
        let mut c = Collector::with_lookup(|kind, id| {
            asked.push((kind, id));
            match (kind, id) {
                (IdKind::User, 1000) => Some("mike".to_string()),
                (IdKind::Group, 0) => Some("root".to_string()),
                (IdKind::Group, 7) => Some(String::new()),
                _ => None,
            }
        });
        for (kind, id) in [
            (IdKind::Group, 7),
            (IdKind::User, 1000),
            (IdKind::Group, 0),
            (IdKind::User, 1000),
            (IdKind::User, 4242),
            (IdKind::Group, 7),
            (IdKind::User, 1000),
        ] {
            c.see(kind, id);
        }
        let rows = c.rows();
        assert_eq!(
            rows,
            vec![
                row(IdKind::User, 1000, Some("mike")),
                row(IdKind::User, 4242, None),
                row(IdKind::Group, 0, Some("root")),
                row(IdKind::Group, 7, None),
            ]
        );
        assert_eq!(asked.len(), 4, "{asked:?}");
    }

    #[test]
    fn the_text_round_trips_with_a_tab_a_newline_and_a_backslash() {
        let rows = vec![
            row(IdKind::User, 0, Some("root")),
            row(IdKind::User, 1000, Some("a\tb")),
            row(IdKind::User, 4242, None),
            row(IdKind::Group, 5, Some("new\nline")),
            row(IdKind::Group, 4_294_967_294, Some("back\\slash")),
        ];
        let mut out = Vec::new();
        write(&mut out, &rows).unwrap();
        let text = String::from_utf8(out).unwrap();
        assert_eq!(
            text,
            "#tapectl-id-map 1\nu\t0\troot\nu\t1000\ta\\tb\nu\t4242\t\n\
             g\t5\tnew\\nline\ng\t4294967294\tback\\\\slash\n"
        );
        assert_eq!(parse(text.as_bytes()).unwrap(), rows);
        assert_eq!(parse(format!("{HEADER}\n").as_bytes()).unwrap(), vec![]);
    }

    #[test]
    fn the_writer_refuses_rows_out_of_order() {
        let mut out = Vec::new();
        let e = write(
            &mut out,
            &[row(IdKind::Group, 1, None), row(IdKind::User, 2, None)],
        )
        .unwrap_err();
        assert!(e.to_string().contains("row 2"), "{e}");
        let e = write(
            &mut out,
            &[row(IdKind::User, 1, None), row(IdKind::User, 1, None)],
        )
        .unwrap_err();
        assert!(e.to_string().contains("each once"), "{e}");
    }

    #[test]
    fn the_parser_refuses_what_the_writer_never_makes() {
        for (text, says) in [
            ("", "empty"),
            ("#tapectl-id-map 2\n", "line 1"),
            ("#tapectl-id-map 1\nx\t1\tn\n", "unknown kind"),
            ("#tapectl-id-map 1\nu\t-1\tn\n", "not an id"),
            ("#tapectl-id-map 1\nu\t01\tn\n", "not an id"),
            ("#tapectl-id-map 1\nu\t4294967296\tn\n", "not an id"),
            ("#tapectl-id-map 1\nu\t1\n", "2 field(s)"),
            ("#tapectl-id-map 1\nu\t1\ta\\q\n", "unknown escape"),
            ("#tapectl-id-map 1\ng\t1\tn\nu\t2\tn\n", "line 3"),
            ("#tapectl-id-map 1\nu\t2\tn\nu\t1\tn\n", "line 3"),
            ("#tapectl-id-map 1\nu\t1\tn", "ends inside a line"),
        ] {
            let e = parse(text.as_bytes()).unwrap_err().to_string();
            assert!(e.contains(says), "{text:?}: {e}");
        }
    }
}
