//! `config set` / `config add` / `config remove` (issue #143, ADR-0012
//! amendment 2026-10-07 item 27): edit config.toml in place.
//!
//! The ruling's four promises, and where each is kept:
//!
//! - **Comments and layout survive.** The file is edited as a `toml_edit`
//!   document, never re-serialized from [`crate::config::Config`] (`Config::save` drops
//!   every comment). Only the key, table or list element named changes.
//! - **An unknown key is refused by name, as every reader refuses it.** No
//!   list of known keys lives here: the edited text is validated by
//!   [`crate::config::Config::from_text`], the body of [`crate::config::Config::load`], so an unknown or
//!   renamed key gets exactly the refusal a reader gives it.
//! - **The result is validated as `config check` would before writing.** The
//!   verdict is [`crate::policy::lenient_config::check_text`]'s on the edited
//!   text. One allowance, so a broken file can be repaired one key at a time
//!   (`config check` lists every problem; an operator fixes them in turn): on
//!   a file that already fails to load, an edit that adds no problem of its
//!   own goes through, and the problems left are named. An edit that adds
//!   one is refused there as everywhere.
//! - **The file is replaced atomically, and a refused edit leaves it
//!   byte-identical.** Nothing is written until the edited text has passed;
//!   then [`replace_file`] writes a temporary file beside it and renames it
//!   over the original.
//!
//! Keys are dotted paths in the spelling `config check` prints:
//! `defaults.slice_size`, `host_check.max_load_per_cpu`. An entry in a list
//! of tables (`[[collections]]`, `[[archive_sets]]`, `[[backends.lto]]`) is
//! selected by its `name`, or by its position from 0:
//! `collections[movies].unit_depth`, `archive_sets[1]`.
//!
//! Values are typed by what the file accepts: a value that reads as a TOML
//! number, boolean, date or array is tried as one first, and as a string if
//! that does not load — so `3` is a number for `staging.jobs` and a string
//! for a collection's `name`. A string the file would take either way is
//! forced by quoting it as TOML (`'"3"'`).

use std::path::Path;

use toml_edit::{ArrayOfTables, DocumentMut, Item, Table, Value};

use crate::error::{Result, TapectlError};
use crate::policy::lenient_config::{check_text, LenientReport};

/// One edit, as the command line gave it.
#[derive(Debug, Clone)]
pub enum Edit {
    /// `config set KEY VALUE`: set one key to one value.
    Set { key: String, value: String },
    /// `config add KEY VALUE...`: append a table to a list of tables (each
    /// VALUE a `field=value`), or values to a list.
    Add { key: String, values: Vec<String> },
    /// `config remove KEY [VALUE...]`: remove a key, a table or one entry of
    /// a list of tables; with VALUEs, remove those values from a list.
    Remove { key: String, values: Vec<String> },
}

impl Edit {
    fn verb(&self) -> &'static str {
        match self {
            Edit::Set { .. } => "set",
            Edit::Add { .. } => "add",
            Edit::Remove { .. } => "remove",
        }
    }
    fn key(&self) -> &str {
        match self {
            Edit::Set { key, .. } | Edit::Add { key, .. } | Edit::Remove { key, .. } => key,
        }
    }
    /// The same edit of `key` (the key the typed one resolved to).
    fn with_key(&self, key: String) -> Edit {
        match self {
            Edit::Set { value, .. } => Edit::Set {
                key,
                value: value.clone(),
            },
            Edit::Add { values, .. } => Edit::Add {
                key,
                values: values.clone(),
            },
            Edit::Remove { values, .. } => Edit::Remove {
                key,
                values: values.clone(),
            },
        }
    }
}

/// An edit that passed, ready to write.
#[derive(Debug, Clone)]
pub struct Planned {
    /// The whole file after the edit.
    pub new_text: String,
    /// `false` when the edit leaves the file as it was (nothing to write).
    pub changed: bool,
    /// What changed, verb first: `set defaults.slice_size = "2G" (was "1G")`.
    pub summary: String,
    /// The same, for `--json`: `action`, `key`, and `value`/`previous`/
    /// `values` as TOML text.
    pub json: serde_json::Value,
    /// Problems the file still has after the edit — only ever non-empty for
    /// a file that already failed to load (see the module doc).
    pub remaining_problems: Vec<String>,
    /// Advisories that do not stop the edit (a backend's device node that
    /// does not exist on this machine, as `drive add` warns).
    pub warnings: Vec<String>,
}

/// Plan `edit` against `original`, the text of the config file at `path`.
/// Pure but for the filesystem probes validation makes (a backend's device
/// canonicalization, as [`crate::config::Config::load`] makes them): nothing is written.
///
/// A whole collection is not added or removed here (ADR-0012 item 206):
/// `config add collections` and `config remove 'collections[NAME]'` are
/// refused by name, pointing at `collection add` (which checks the root, the
/// tenant and the access first) and `collection remove`, which edit the file
/// through [`plan_collection_add`] and [`plan_collection_remove`]. A key of
/// one collection (`collections[movies].exclude`) is edited here as any
/// other.
pub fn plan(original: &str, path: &Path, edit: &Edit) -> Result<Planned> {
    // ADR-0012 item 214: a bare key, and a list of tables with one entry,
    // resolve against the file before anything else; every message below
    // names the key resolved. A file that is not TOML is refused, by name,
    // by `plan_parsed`.
    let segs = match original.parse::<DocumentMut>() {
        Ok(doc) => resolve_key(doc.as_table(), edit.key())?,
        Err(_) => parse_key(edit.key())?,
    };
    let edit = edit.with_key(dotted(&segs));
    if let Some(refusal) = whole_collection_refusal(&edit, &segs) {
        return Err(TapectlError::Other(refusal));
    }
    plan_parsed(original, path, &edit, segs)
}

/// `[[collections]]` table for `fields` (`field=value`, each value TOML
/// text, as `config add` takes them), appended: `collection add`'s write.
/// Checked and laid out as any `config add`.
pub fn plan_collection_add(original: &str, path: &Path, fields: Vec<String>) -> Result<Planned> {
    let edit = Edit::Add {
        key: COLLECTIONS.to_string(),
        values: fields,
    };
    let segs = parse_key(edit.key())?;
    plan_parsed(original, path, &edit, segs)
}

/// The `[[collections]]` table named `name`, taken out with the comments
/// above it kept: `collection remove`'s write.
pub fn plan_collection_remove(original: &str, path: &Path, name: &str) -> Result<Planned> {
    let edit = Edit::Remove {
        key: format!("{COLLECTIONS}[{name}]"),
        values: Vec::new(),
    };
    // Selected by name, never parsed: `collections[2024]` would read as the
    // position 2024, and a table written by hand may be named 2024 (`collection
    // add` refuses such a name).
    let segs = vec![Seg {
        key: COLLECTIONS.to_string(),
        select: Some(Selector::Name(name.to_string())),
    }];
    plan_parsed(original, path, &edit, segs)
}

const COLLECTIONS: &str = "collections";

/// The refusal for an edit of a whole collection, or `None`. Only the
/// list itself and its entries: a key inside one entry is not refused.
fn whole_collection_refusal(edit: &Edit, segs: &[Seg]) -> Option<String> {
    let [only] = segs else {
        return None;
    };
    if only.key != COLLECTIONS {
        return None;
    }
    match edit {
        Edit::Add { .. } if only.select.is_none() => Some(
            "`tapectl config add collections` is refused since 1.9.0: use `tapectl collection \
             add NAME ROOT --tenant TENANT`, which checks the root, the tenant and the access \
             before it writes the table"
                .to_string(),
        ),
        Edit::Remove { .. } => Some(format!(
            "`tapectl config remove '{}'` is refused since 1.9.0: use `tapectl collection \
             remove NAME`, which keeps the collection's units registered and untouched",
            edit.key()
        )),
        _ => None,
    }
}

fn plan_parsed(original: &str, path: &Path, edit: &Edit, segs: Vec<Seg>) -> Result<Planned> {
    let doc: DocumentMut = original.parse().map_err(|e: toml_edit::TomlError| {
        let at = e
            .span()
            .map(|s| format!(" at line {}", original[..s.start].lines().count().max(1)))
            .unwrap_or_default();
        TapectlError::Config(format!(
            "{} is not valid TOML, so it cannot be edited in place — fix it by hand \
             (`tapectl config check` names the problem): {}{at}",
            path.display(),
            e.message().trim()
        ))
    })?;
    let before = check_text(original, path);
    if !matches!(edit, Edit::Remove { .. }) {
        refuse_under_unknown(&before, path, edit, &segs)?;
    }
    match edit {
        Edit::Set { value, .. } => plan_set(&doc, original, path, &before, edit, &segs, value),
        Edit::Add { values, .. } => plan_add(&doc, original, path, &before, edit, &segs, values),
        Edit::Remove { values, .. } => {
            plan_remove(&doc, original, path, &before, edit, &segs, values)
        }
    }
}

// ---------------------------------------------------------------- key paths

/// How one segment of a key picks an entry of a list of tables.
#[derive(Debug, Clone, PartialEq)]
enum Selector {
    Index(usize),
    Name(String),
}

#[derive(Debug, Clone, PartialEq)]
struct Seg {
    key: String,
    select: Option<Selector>,
}

impl std::fmt::Display for Seg {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.select {
            None => write!(f, "{}", self.key),
            Some(Selector::Index(i)) => write!(f, "{}[{i}]", self.key),
            Some(Selector::Name(n)) => write!(f, "{}[{n}]", self.key),
        }
    }
}

fn dotted(segs: &[Seg]) -> String {
    segs.iter()
        .map(|s| s.to_string())
        .collect::<Vec<_>>()
        .join(".")
}

/// `defaults.slice_size`, `collections[movies].root`, `archive_sets[1]`.
/// A selector's brackets may hold dots (a backend named `hp.lto6`).
fn parse_key(key: &str) -> Result<Vec<Seg>> {
    let bad = |why: &str| {
        TapectlError::Config(format!(
            "\"{key}\" is not a config key: {why} (keys look like defaults.slice_size, \
             or collections[<name or 0-based index>].root)"
        ))
    };
    let mut segs = Vec::new();
    let mut chars = key.chars().peekable();
    loop {
        let mut name = String::new();
        while let Some(&c) = chars.peek() {
            if c == '.' || c == '[' {
                break;
            }
            if !(c.is_ascii_alphanumeric() || c == '_' || c == '-') {
                return Err(bad(&format!("'{c}' cannot appear in a key name")));
            }
            name.push(c);
            chars.next();
        }
        if name.is_empty() {
            return Err(bad("a key name is empty"));
        }
        let mut select = None;
        if chars.peek() == Some(&'[') {
            chars.next();
            let mut inner = String::new();
            loop {
                match chars.next() {
                    Some(']') => break,
                    Some(c) => inner.push(c),
                    None => return Err(bad("a '[' is not closed")),
                }
            }
            if inner.is_empty() {
                return Err(bad("an empty [] selects nothing"));
            }
            select = Some(match inner.parse::<usize>() {
                Ok(i) if inner.chars().all(|c| c.is_ascii_digit()) => Selector::Index(i),
                _ => Selector::Name(inner),
            });
        }
        segs.push(Seg { key: name, select });
        match chars.next() {
            None => break,
            Some('.') => continue,
            Some(c) => return Err(bad(&format!("'{c}' after a selector"))),
        }
    }
    Ok(segs)
}

/// The entry of `aot` that `sel` names. A name must match exactly one.
fn select(aot: &ArrayOfTables, sel: &Selector, label: &str) -> Result<usize> {
    match sel {
        Selector::Index(i) if *i < aot.len() => Ok(*i),
        Selector::Index(i) => Err(TapectlError::Config(format!(
            "{label} has {} entr{}, so there is no {label}[{i}] (entries count from 0)",
            aot.len(),
            if aot.len() == 1 { "y" } else { "ies" }
        ))),
        Selector::Name(n) => {
            let hits: Vec<usize> = aot
                .iter()
                .enumerate()
                .filter(|(_, t)| t.get("name").and_then(|i| i.as_str()) == Some(n.as_str()))
                .map(|(i, _)| i)
                .collect();
            match hits.as_slice() {
                [one] => Ok(*one),
                [] => Err(TapectlError::Config(format!(
                    "{label} has no entry named \"{n}\" (it has: {})",
                    names(aot)
                ))),
                many => Err(TapectlError::Config(format!(
                    "{label} has {} entries named \"{n}\" — select one by position: {}",
                    many.len(),
                    many.iter()
                        .map(|i| format!("{label}[{i}]"))
                        .collect::<Vec<_>>()
                        .join(", ")
                ))),
            }
        }
    }
}

fn names(aot: &ArrayOfTables) -> String {
    if aot.is_empty() {
        return "none".to_string();
    }
    aot.iter()
        .enumerate()
        .map(|(i, t)| match t.get("name").and_then(|v| v.as_str()) {
            Some(n) => format!("{n} [{i}]"),
            None => format!("[{i}]"),
        })
        .collect::<Vec<_>>()
        .join(", ")
}

/// Walk `segs` from `cur` to the table they name. With `create`, a missing
/// table is made (implicit: its header is written only once it holds a key).
fn descend<'a>(mut cur: &'a mut Table, segs: &[Seg], create: bool) -> Result<&'a mut Table> {
    for (i, seg) in segs.iter().enumerate() {
        let label = dotted(&segs[..=i]);
        let plain = dotted(&segs[..i])
            .split('.')
            .filter(|s| !s.is_empty())
            .chain(std::iter::once(seg.key.as_str()))
            .collect::<Vec<_>>()
            .join(".");
        if !cur.contains_key(&seg.key) {
            if !create || seg.select.is_some() {
                return Err(TapectlError::Config(format!("{plain} is not in the file")));
            }
            let mut t = Table::new();
            t.set_implicit(true);
            cur.insert(&seg.key, Item::Table(t));
        }
        let item = cur.get_mut(&seg.key).expect("present or just inserted");
        cur = match (&seg.select, item) {
            (None, Item::Table(t)) => t,
            (Some(sel), Item::ArrayOfTables(aot)) => {
                let idx = select(aot, sel, &plain)?;
                aot.get_mut(idx).expect("select returned an index in range")
            }
            (None, Item::ArrayOfTables(_)) => {
                return Err(TapectlError::Config(format!(
                    "{plain} is a list of tables — name one entry: {plain}[<name>] or \
                     {plain}[<index>]"
                )))
            }
            (Some(_), _) => {
                return Err(TapectlError::Config(format!(
                    "{label}: {plain} is not a list of tables, so it has no entries to select"
                )))
            }
            (None, _) => {
                return Err(TapectlError::Config(format!(
                    "{plain} is not a table written as [{plain}] — it holds a value, or an \
                     inline table this command does not edit; edit it by hand"
                )))
            }
        };
    }
    Ok(cur)
}

// ------------------------------------------------------------------ schema
//
// ADR-0012 item 214: a key can be found. A bare key resolves against the
// schema (`post_warning_bytes` is `backends.lto[lto6].post_warning_bytes`
// with one drive), `backends.lto.KEY` names the only drive's key, and a
// typo gets a did-you-mean. What keys exist is never listed here: each
// table's keys are read from the struct's own serde definition (the field
// list `deny_unknown_fields` refuses an unknown key with), so a key added to
// a struct is known here with no change. Only the tables themselves, and how
// each is written, are listed ([`TABLES`]); a test holds that list to
// [`crate::config::Config`]'s own shape.

/// How a table of config.toml is written.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Shape {
    /// `[name]`: one table.
    Table,
    /// `[[name]]`: a list of tables, an entry selected by name or position.
    List,
    /// A table whose keys are the operator's own (location names), each a
    /// value: `[defaults.copies_at]`.
    Map,
}

/// Every table of config.toml, by its dotted name, and how it is written.
const TABLES: &[(&str, Shape)] = &[
    ("", Shape::Table),
    ("dar", Shape::Table),
    ("backends", Shape::Table),
    ("backends.lto", Shape::List),
    ("archive_sets", Shape::List),
    ("archive_sets.copies_at", Shape::Map),
    ("defaults", Shape::Table),
    ("defaults.copies_at", Shape::Map),
    ("staging", Shape::Table),
    ("discovery", Shape::Table),
    ("collections", Shape::List),
    ("compaction", Shape::Table),
    ("logging", Shape::Table),
    ("host_check", Shape::Table),
    ("health", Shape::Table),
    ("ops", Shape::Table),
];

/// The key a probe puts in a table to make serde name the table's keys.
const PROBE_KEY: &str = "__tapectl_probe__";

fn shape_of(table: &str) -> Option<Shape> {
    TABLES.iter().find(|(t, _)| *t == table).map(|(_, s)| *s)
}

/// The keys of `table` (a [`Shape::Table`] or [`Shape::List`] of
/// [`TABLES`]), as the struct behind it declares them: serde's own refusal
/// of an unknown key lists them (`` unknown field `x`, expected one of `a`,
/// `b` ``, or `` expected `a` `` for one), the same message
/// `policy::lenient_config` reads.
fn table_fields(table: &str, shape: Shape) -> Vec<String> {
    let header = match (table, shape) {
        ("", _) => String::new(),
        (t, Shape::List) => format!("[[{t}]]\n"),
        (t, _) => format!("[{t}]\n"),
    };
    match toml::from_str::<crate::config::Config>(&format!("{header}{PROBE_KEY} = 0\n")) {
        Ok(_) => Vec::new(),
        Err(e) => expected_fields(e.message()),
    }
}

/// The names serde's unknown-field message lists after "expected".
fn expected_fields(message: &str) -> Vec<String> {
    let Some(rest) = message
        .strip_prefix("unknown field `")
        .and_then(|r| r.split_once("`, "))
        .map(|(_, tail)| tail)
    else {
        return Vec::new();
    };
    rest.split('`')
        .skip(1)
        .step_by(2)
        .map(str::to_string)
        .collect()
}

/// Every key that holds a value, by its names (`["backends", "lto",
/// "post_warning_bytes"]`). A [`Shape::Map`] is one: its value is the whole
/// map, and its entries are keys under it.
fn value_keys() -> Vec<Vec<String>> {
    let mut out = Vec::new();
    for (table, shape) in TABLES {
        if *shape == Shape::Map {
            continue;
        }
        for field in table_fields(table, *shape) {
            let full = if table.is_empty() {
                field.clone()
            } else {
                format!("{table}.{field}")
            };
            if matches!(shape_of(&full), Some(Shape::Table | Shape::List)) {
                continue;
            }
            let mut names: Vec<String> = table
                .split('.')
                .filter(|s| !s.is_empty())
                .map(str::to_string)
                .collect();
            names.push(field);
            out.push(names);
        }
    }
    out
}

/// What `names` (a key without its selectors) is in the schema.
#[derive(Debug, Clone, PartialEq)]
enum Known {
    /// A key that holds a value.
    Value,
    /// An entry of a [`Shape::Map`]: a location's count.
    MapEntry,
    /// A table (or a list of tables, or one entry of one), with its keys.
    Table(Vec<String>),
    /// Not a key: the first name that is not, and the keys of the table it
    /// was looked for in.
    Unknown { at: usize, siblings: Vec<String> },
}

fn known(names: &[String]) -> Known {
    let mut table = String::new();
    for (i, name) in names.iter().enumerate() {
        let last = i + 1 == names.len();
        let shape = shape_of(&table).unwrap_or(Shape::Table);
        if shape == Shape::Map {
            return if last {
                Known::MapEntry
            } else {
                Known::Unknown {
                    at: i + 1,
                    siblings: Vec::new(),
                }
            };
        }
        let fields = table_fields(&table, shape);
        if !fields.contains(name) {
            return Known::Unknown {
                at: i,
                siblings: fields,
            };
        }
        let child = if table.is_empty() {
            name.clone()
        } else {
            format!("{table}.{name}")
        };
        match shape_of(&child) {
            None | Some(Shape::Map) if last => return Known::Value,
            None => {
                return Known::Unknown {
                    at: i + 1,
                    siblings: Vec::new(),
                }
            }
            Some(s) if last => return Known::Table(table_fields(&child, s)),
            Some(_) => table = child,
        }
    }
    Known::Table(table_fields("", Shape::Table))
}

/// Levenshtein distance, by characters (the sibling of `error.rs`'s, which
/// is that module's own).
fn edit_distance(a: &str, b: &str) -> usize {
    let b: Vec<char> = b.chars().collect();
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    for (i, ca) in a.chars().enumerate() {
        let mut cur = Vec::with_capacity(b.len() + 1);
        cur.push(i + 1);
        for (j, cb) in b.iter().enumerate() {
            let substitute = prev[j] + usize::from(ca != *cb);
            cur.push(substitute.min(prev[j + 1] + 1).min(cur[j] + 1));
        }
        prev = cur;
    }
    prev[b.len()]
}

/// The names in `among` nearest `typed`, ignoring case: within a third of
/// its length (at least one edit, at most three), the nearest only, and
/// none when more than three are equally near.
fn nearest<'a>(typed: &str, among: impl IntoIterator<Item = &'a String>) -> Vec<&'a String> {
    let typed = typed.to_lowercase();
    let limit = (typed.chars().count() / 3).clamp(1, 3);
    let mut best = usize::MAX;
    let mut near: Vec<&String> = Vec::new();
    for name in among {
        let d = edit_distance(&typed, &name.to_lowercase());
        if d == 0 || d > limit {
            continue;
        }
        if d < best {
            best = d;
            near.clear();
        }
        if d == best && !near.contains(&name) {
            near.push(name);
        }
    }
    if near.len() > 3 {
        return Vec::new();
    }
    near.sort_unstable();
    near
}

/// ` — did you mean X?` (or `X or Y`), or nothing.
fn did_you_mean(choices: &[String]) -> String {
    if choices.is_empty() {
        return String::new();
    }
    format!(" — did you mean {}?", choices.join(" or "))
}

/// The keys near `segs` (a key that is not one), spelled with the user's
/// own selectors: the first unknown name replaced by a near key of its
/// table. Empty when nothing is near.
fn suggestions_for(segs: &[Seg]) -> Vec<String> {
    let names: Vec<String> = segs.iter().map(|s| s.key.clone()).collect();
    let Known::Unknown { at, siblings } = known(&names) else {
        return Vec::new();
    };
    nearest(&names[at], &siblings)
        .into_iter()
        .map(|near| {
            let mut fixed = segs.to_vec();
            fixed[at].key = near.clone();
            dotted(&fixed)
        })
        .collect()
}

/// The entries of the list of tables at `segs` in `root`, as a selector
/// each (by name when the entry has one, else by position). Empty when the
/// file has no such list.
fn list_entries(root: &Table, segs: &[Seg]) -> Vec<Selector> {
    let Some((last, parents)) = segs.split_last() else {
        return Vec::new();
    };
    let Some(parent) = steps_of(root, parents).and_then(|path| table_at(root, &path)) else {
        return Vec::new();
    };
    match parent.get(&last.key) {
        Some(Item::ArrayOfTables(aot)) => (0..aot.len())
            .map(|i| {
                let name = aot
                    .get(i)
                    .and_then(|t| t.get("name"))
                    .and_then(|v| v.as_str());
                match name {
                    // A name two entries share selects neither.
                    Some(n) if select(aot, &Selector::Name(n.to_string()), "").is_ok() => {
                        Selector::Name(n.to_string())
                    }
                    _ => Selector::Index(i),
                }
            })
            .collect(),
        _ => Vec::new(),
    }
}

/// What to say when the list of tables `list` (dotted, e.g. `backends.lto`)
/// has no entry to take a key from.
fn no_entries(list: &str, key: &str) -> TapectlError {
    if list == "backends.lto" {
        TapectlError::Config(format!(
            "{key} is a key of a drive, and no drive is configured: add one with `tapectl \
             drive add --name lto6 --device-tape /dev/tape/by-id/<drive>-nst --generation LTO-6`"
        ))
    } else {
        TapectlError::Config(format!(
            "{key} is a key of each [[{list}]] entry, and the file has none"
        ))
    }
}

/// `key` as the operator typed it, resolved to the key it names in the file
/// whose root table is `root` (ADR-0012 item 214):
///
/// - a bare key that is not a top-level table, and not in the file as
///   typed, resolves against the schema: the one key of that name, with
///   each list of tables on its way expanded over the file's entries
///   (`post_warning_bytes` is `backends.lto[lto6].post_warning_bytes` with
///   one drive); several are refused, naming each, and none is refused with
///   a did-you-mean;
/// - a list of tables named without a selector before a key of its entries
///   (`backends.lto.post_warning_bytes`) is its only entry; with none or
///   several, it is refused, naming them.
///
/// A key the schema does not know is not refused here unless it is bare:
/// `config set` and `add` refuse it through the edited file's check (which
/// knows the renamed keys by name), `config remove` takes it out of a file
/// that has it, and `config get` asks [`known`].
fn resolve_key(root: &Table, key: &str) -> Result<Vec<Seg>> {
    let mut segs = parse_key(key)?;
    if let [only] = segs.as_slice() {
        let root_fields = table_fields("", Shape::Table);
        if only.select.is_none() && !root_fields.contains(&only.key) && !root.contains_key(&only.key)
        {
            return resolve_bare(root, &only.key);
        }
    }
    for i in 0..segs.len().saturating_sub(1) {
        if segs[i].select.is_some() {
            continue;
        }
        let names: Vec<&str> = segs[..=i].iter().map(|s| s.key.as_str()).collect();
        let list = names.join(".");
        if shape_of(&list) != Some(Shape::List) {
            continue;
        }
        let entries = list_entries(root, &segs[..=i]);
        match entries.as_slice() {
            [one] => segs[i].select = Some(one.clone()),
            [] => return Err(no_entries(&list, key)),
            many => {
                let named: Vec<String> = many
                    .iter()
                    .map(|sel| {
                        let mut s = segs.clone();
                        s[i].select = Some(sel.clone());
                        dotted(&s)
                    })
                    .collect();
                return Err(TapectlError::Config(format!(
                    "{key} is ambiguous: {list} has {} entries, so name one: {}",
                    many.len(),
                    named.join(", ")
                )));
            }
        }
    }
    Ok(segs)
}

/// A bare key, resolved against the schema ([`resolve_key`]).
fn resolve_bare(root: &Table, name: &str) -> Result<Vec<Seg>> {
    let owners: Vec<Vec<String>> = value_keys()
        .into_iter()
        .filter(|names| names.last().map(String::as_str) == Some(name))
        .collect();
    if owners.is_empty() {
        let mut every: Vec<String> = value_keys()
            .into_iter()
            .filter_map(|names| names.last().cloned())
            .collect();
        every.sort_unstable();
        every.dedup();
        let near: Vec<String> = nearest(name, &every).into_iter().cloned().collect();
        return Err(TapectlError::Config(format!(
            "{name} is not a config key{}",
            did_you_mean(&near)
        )));
    }
    let mut candidates: Vec<Vec<Seg>> = Vec::new();
    let mut empty_list: Option<String> = None;
    for names in &owners {
        let mut expanded: Vec<Vec<Seg>> = vec![Vec::new()];
        let mut table = String::new();
        for n in names {
            table = if table.is_empty() {
                n.clone()
            } else {
                format!("{table}.{n}")
            };
            let list = shape_of(&table) == Some(Shape::List);
            let mut next = Vec::new();
            for prefix in &expanded {
                let mut segs = prefix.clone();
                segs.push(Seg {
                    key: n.clone(),
                    select: None,
                });
                if !list {
                    next.push(segs);
                    continue;
                }
                for sel in list_entries(root, &segs) {
                    let mut s = segs.clone();
                    s.last_mut().expect("just pushed").select = Some(sel);
                    next.push(s);
                }
            }
            if list && next.is_empty() {
                empty_list.get_or_insert(table.clone());
            }
            expanded = next;
        }
        candidates.extend(expanded);
    }
    match candidates.len() {
        1 => Ok(candidates.remove(0)),
        0 => Err(no_entries(
            empty_list.as_deref().unwrap_or_default(),
            name,
        )),
        _ => Err(TapectlError::Config(format!(
            "{name} is ambiguous: it is {} keys in this file, so name one: {}",
            candidates.len(),
            candidates
                .iter()
                .map(|s| dotted(s))
                .collect::<Vec<_>>()
                .join(", ")
        ))),
    }
}

// ------------------------------------------------------------------ values

/// What `raw` may mean, most specific first: a TOML literal that is not a
/// string (number, boolean, date, array), then the text as a string. A raw
/// value already quoted as a TOML string means only that string.
fn candidates(raw: &str) -> Result<Vec<Value>> {
    let mut out = Vec::new();
    if let Ok(mut lit) = raw.trim().parse::<Value>() {
        if lit.is_inline_table() {
            return Err(TapectlError::Config(format!(
                "{raw} is an inline table — set its keys one at a time, or add a table to a \
                 list with `config add <key> field=value ...`"
            )));
        }
        lit.decor_mut().clear();
        if lit.is_str() {
            return Ok(vec![lit]);
        }
        out.push(lit);
    }
    out.push(Value::from(raw));
    Ok(out)
}

/// A value as TOML text, without the spacing around it.
fn toml_text(v: &Value) -> String {
    let mut v = v.clone();
    v.decor_mut().clear();
    v.to_string().trim().to_string()
}

fn same_value(a: &Value, b: &Value) -> bool {
    match (a.as_str(), b.as_str()) {
        (Some(x), Some(y)) => x == y,
        _ => toml_text(a) == toml_text(b),
    }
}

/// Every choice of candidate per slot, fewest string fallbacks first. Ten
/// ambiguous slots is far more than any config table has; past that only
/// "all literal" and "all string" are tried.
fn combos(slots: &[Vec<Value>]) -> Vec<Vec<usize>> {
    let ambiguous: Vec<usize> = (0..slots.len()).filter(|&i| slots[i].len() > 1).collect();
    let masks: Vec<u32> = if ambiguous.len() <= 10 {
        let mut m: Vec<u32> = (0..(1u32 << ambiguous.len())).collect();
        m.sort_by_key(|x| (x.count_ones(), *x));
        m
    } else {
        vec![0, u32::MAX]
    };
    masks
        .into_iter()
        .map(|mask| {
            let mut pick = vec![0usize; slots.len()];
            for (bit, &slot) in ambiguous.iter().enumerate() {
                if bit < 32 && mask & (1 << bit) != 0 {
                    pick[slot] = 1;
                }
            }
            pick
        })
        .collect()
}

// -------------------------------------------------------------- the verdict

/// Whether `after` may be written over a file whose report was `before`:
/// it loads, or (on a file that already failed) it adds no problem.
fn acceptable(before: &LenientReport, after: &LenientReport) -> bool {
    // Issue #479: a slice size the measured post-warning room cannot hold is
    // not a load problem, but an edit must not write one. A cap problem the
    // file already had, unchanged, does not make the edit unacceptable (it
    // may be an unrelated edit, or part of the repair).
    if after
        .slice_cap_problems
        .iter()
        .any(|p| !before.slice_cap_problems.contains(p))
    {
        return false;
    }
    if after.valid {
        return true;
    }
    // Problem 0 is `Config::load`'s own error, which carries line numbers
    // that move with any edit; the rest are its line-free breakdown. With no
    // breakdown (the text did not parse far enough), nothing can be compared.
    if before.valid || before.best_effort.is_none() || after.best_effort.is_none() {
        return false;
    }
    let old = &before.problems[1..];
    after.problems[1..].iter().all(|p| old.contains(p))
}

/// Refuse a `set` or `add` at or under a key the file already has and no
/// reader accepts. [`acceptable`] lets an edit through a broken file when it
/// adds no problem, but an edit inside an unknown table adds none the
/// checker can see — it strips the whole table and reports it once — so
/// `config set nonsense.evil 1` would be written. The allowance is for
/// repairing a file; `config remove` is how an unknown key leaves it.
///
/// Compared without selectors: the checker counts a list's entries by
/// position, the edit may name them, and an entry's unknown key is unknown
/// in every entry.
fn refuse_under_unknown(
    before: &LenientReport,
    path: &Path,
    edit: &Edit,
    segs: &[Seg],
) -> Result<()> {
    fn plain(p: &str) -> String {
        let mut out = String::new();
        let mut depth = 0usize;
        for c in p.chars() {
            match c {
                '[' => depth += 1,
                ']' => depth = depth.saturating_sub(1),
                _ if depth == 0 => out.push(c),
                _ => {}
            }
        }
        out
    }
    let edited = segs
        .iter()
        .map(|s| s.key.as_str())
        .collect::<Vec<_>>()
        .join(".");
    for p in &before.problems {
        let Some(unknown) = p.strip_prefix("unknown key: ") else {
            continue;
        };
        let bare = plain(unknown);
        if edited == bare || edited.starts_with(&format!("{bare}.")) {
            return Err(TapectlError::Config(format!(
                "config {} {} refused ({} is unchanged): unknown key: {unknown} is \
                 already in the file, and no reader accepts it — remove it with \
                 `tapectl config remove {unknown}` (or fix its spelling by hand) instead \
                 of editing under it",
                edit.verb(),
                edit.key(),
                path.display()
            )));
        }
    }
    Ok(())
}

/// Try `apply` with each combination of candidate values until one yields
/// text that may be written; refuse, naming every problem, when none does.
fn search(
    doc: &DocumentMut,
    path: &Path,
    before: &LenientReport,
    edit: &Edit,
    slots: &[Vec<Value>],
    apply: &dyn Fn(&mut DocumentMut, &[Value]) -> Result<()>,
) -> Result<(String, Vec<Value>, LenientReport)> {
    let mut refused: Vec<LenientReport> = Vec::new();
    for pick in combos(slots) {
        let chosen: Vec<Value> = pick
            .iter()
            .enumerate()
            .map(|(slot, &c)| slots[slot][c].clone())
            .collect();
        let mut d = doc.clone();
        apply(&mut d, &chosen)?;
        let text = d.to_string();
        let report = check_text(&text, path);
        if acceptable(before, &report) {
            return Ok((text, chosen, report));
        }
        refused.push(report);
    }
    // The reading to explain: the first whose problem is not only that the
    // value has the wrong type for the key, since another reading of the
    // same text (`5` as text for `compression`) says more; else the first.
    let report = refused
        .iter()
        .find(|r| !is_type_error(r))
        .or(refused.first())
        .expect("combos always yields at least one choice");
    Err(TapectlError::Config(format!(
        "config {} {} refused ({} is unchanged): {}",
        edit.verb(),
        edit.key(),
        path.display(),
        plain_problems(report, before, path, edit).join("; ")
    )))
}

// ------------------------------------------------------- what was wrong

/// Whether `report`'s problem is a value of the wrong type for its key.
fn is_type_error(report: &LenientReport) -> bool {
    !report.valid
        && report
            .problems
            .first()
            .is_some_and(|p| p.contains("invalid type: "))
}

/// The problems of a refused edit, each on one line (ADR-0012 item 214: an
/// error names the key and what was wrong, without the excerpt of the file
/// the TOML parser draws, and names types plainly). The edited file's own
/// problem comes first — `report.problems[0]`, which carries a renamed
/// key's remediation (`policy::lenient_config`'s doc) — with its banner,
/// excerpt and file path taken off; an unknown key is named with a
/// did-you-mean instead of serde's list of every key; the line-free
/// breakdown follows where it says more. On a file that already failed,
/// only the problems this edit adds are named.
fn plain_problems(
    report: &LenientReport,
    before: &LenientReport,
    path: &Path,
    edit: &Edit,
) -> Vec<String> {
    let file = path.display().to_string();
    let strip = |p: &str| -> String {
        let p = p.strip_prefix("configuration error: ").unwrap_or(p);
        p.strip_prefix(&format!("{file}: ")).unwrap_or(p).to_string()
    };
    let new = |p: &String| before.valid || !before.problems.contains(p);
    let mut out: Vec<String> = Vec::new();
    let mut unknown_named = false;
    if let Some(head) = report.problems.first() {
        let (message, line) = without_excerpt(&strip(head));
        if message.starts_with("unknown field `") {
            for p in report.problems[1..].iter().filter(|p| new(p)) {
                if let Some(key) = p.strip_prefix("unknown key: ") {
                    out.push(unknown_key_problem(key, edit));
                    unknown_named = true;
                }
            }
            if !unknown_named {
                let typed = message
                    .strip_prefix("unknown field `")
                    .and_then(|r| r.split('`').next())
                    .unwrap_or_default();
                let fields = expected_fields(&message);
                let near: Vec<String> = nearest(typed, &fields).into_iter().cloned().collect();
                out.push(format!(
                    "{} is not a config key{}",
                    edit.key(),
                    did_you_mean(&near)
                ));
                unknown_named = true;
            }
        } else {
            out.push(plain_message(&message, line.as_deref(), edit));
        }
    }
    for p in report.problems.iter().skip(1).filter(|p| new(p)) {
        if let Some(key) = p.strip_prefix("unknown key: ") {
            let last = key.rsplit('.').next().unwrap_or(key);
            let last = last.split('[').next().unwrap_or(last);
            if unknown_named || out.iter().any(|o| o.contains(last)) {
                continue;
            }
            out.push(unknown_key_problem(key, edit));
            continue;
        }
        let p = strip(p);
        if !out.contains(&p) {
            out.push(p);
        }
    }
    for p in &report.slice_cap_problems {
        if !before.slice_cap_problems.contains(p) {
            out.push(strip(p));
        }
    }
    out
}

/// A TOML parser's error without the excerpt it draws of the file: the
/// message, and the line of the file it pointed at (`key = value`).
fn without_excerpt(text: &str) -> (String, Option<String>) {
    if !text.contains("TOML parse error at line") {
        return (text.trim().to_string(), None);
    }
    let mut line = None;
    let mut message = Vec::new();
    for l in text.lines() {
        let t = l.trim_start();
        if t.starts_with("TOML parse error at line") {
            continue;
        }
        let digits = t.trim_start_matches(|c: char| c.is_ascii_digit());
        if let Some(code) = digits.trim_start().strip_prefix('|') {
            if digits.len() != t.len() && !code.trim().is_empty() {
                line = Some(code.trim().to_string());
            }
            continue;
        }
        if !t.is_empty() {
            message.push(t.to_string());
        }
    }
    (message.join(" "), line)
}

/// "`key` is not a config key", with a did-you-mean, for the unknown key
/// `key` the check names (`archive_sets[0].bogus`): spelled as the edit
/// spelled it when it is the edit's key or under it.
fn unknown_key_problem(key: &str, edit: &Edit) -> String {
    let plain = |k: &str| -> Vec<String> {
        parse_key(k)
            .map(|segs| segs.into_iter().map(|s| s.key).collect())
            .unwrap_or_default()
    };
    let (shown, segs) = match parse_key(edit.key()) {
        Ok(edit_segs) if plain(edit.key()).starts_with(&plain(key)) && !plain(key).is_empty() => {
            (edit.key().to_string(), edit_segs)
        }
        _ => (key.to_string(), parse_key(key).unwrap_or_default()),
    };
    format!(
        "{shown} is not a config key{}",
        did_you_mean(&suggestions_for(&segs))
    )
}

/// serde's words in `message` said plainly, naming the key: "invalid type:
/// string \"lots\", expected i32" is "`defaults.min_copies` takes a whole
/// number, not \"lots\"". `line` is the file's line the parser pointed at,
/// which names the key a field of an added table is. Anything else is
/// returned as it is.
fn plain_message(message: &str, line: Option<&str>, edit: &Edit) -> String {
    let field = line
        .and_then(|l| l.split_once('='))
        .map(|(k, _)| k.trim().trim_matches('"').to_string());
    let key = match (&field, edit) {
        (Some(f), _) if edit.key().rsplit('.').next() == Some(f.as_str()) => edit.key().to_string(),
        (Some(f), Edit::Add { .. }) => format!("{}.{f}", edit.key()),
        (Some(f), _) => f.clone(),
        (None, _) => edit.key().to_string(),
    };
    if let Some(rest) = message.strip_prefix("invalid type: ") {
        if let Some((got, expected)) = rest.split_once(", expected ") {
            let got = match got.split('`').nth(1) {
                Some(inner) => inner.to_string(),
                None => match got.strip_prefix("string ") {
                    Some(s) => s.to_string(),
                    None => match got {
                        "sequence" => "a list".to_string(),
                        "map" => "a table".to_string(),
                        other => other.to_string(),
                    },
                },
            };
            return format!("{key} takes {}, not {got}", plain_type(expected));
        }
    }
    if let Some(rest) = message.strip_prefix("missing field `") {
        let missing = rest.split('`').next().unwrap_or(rest);
        return match edit {
            Edit::Remove { .. } if edit.key().rsplit('.').next() == Some(missing) => {
                format!("{} is required, so it cannot be removed", edit.key())
            }
            _ => format!("{}: {missing} is required", edit.key()),
        };
    }
    message.to_string()
}

/// A serde type name as an operator would say it.
fn plain_type(expected: &str) -> &str {
    match expected.trim() {
        "i8" | "i16" | "i32" | "i64" | "isize" | "u8" | "u16" | "u32" | "u64" | "usize"
        | "an integer" => "a whole number",
        "f32" | "f64" | "a float" => "a number",
        "a boolean" => "true or false",
        "a string" => "text",
        "a sequence" => "a list, such as [\"a\", \"b\"]",
        "a map" => "a table",
        other if other.starts_with("struct ") => "a table",
        other => other,
    }
}

fn finish(
    original: &str,
    text: String,
    after: LenientReport,
    summary: String,
    json: serde_json::Value,
    warnings: Vec<String>,
) -> Planned {
    let remaining_problems = if after.valid {
        Vec::new()
    } else {
        after.problems[1..].to_vec()
    };
    // Issue #479: the file loads, but `config check` still refuses it.
    let mut warnings = warnings;
    warnings.extend(
        after
            .slice_cap_problems
            .iter()
            .map(|p| format!("warning: the file still breaks the slice cap (`tapectl config check` exits non-zero): {p}")),
    );
    Planned {
        changed: text != original,
        new_text: text,
        summary,
        json,
        remaining_problems,
        warnings,
    }
}

// ---------------------------------------------------------- backend fields

/// Whether the table at `table_segs` is a `[[backends.lto]]` entry or the
/// list itself.
fn is_backend_table(table_segs: &[Seg]) -> bool {
    table_segs.len() == 2 && table_segs[0].key == "backends" && table_segs[1].key == "lto"
}

/// The checks `drive add` makes on a drive's fields that loading a config
/// does not (issue #126/#186): the name's spelling, and the generation in
/// its canonical form (`lto6` is written `LTO-6`). A device node that does
/// not exist is a warning, never a refusal — a drive may be configured
/// before it is plugged in. Returns the value to write.
fn backend_field(field: &str, raw: &str, warnings: &mut Vec<String>) -> Result<Option<Value>> {
    let as_string = || -> String {
        match raw.trim().parse::<Value>() {
            Ok(v) if v.is_str() => v.as_str().unwrap_or(raw).to_string(),
            _ => raw.to_string(),
        }
    };
    match field {
        "name" => {
            let s = as_string();
            crate::naming::validate_backend_name(&s)?;
            Ok(Some(Value::from(s)))
        }
        "generation" => {
            let s = as_string();
            crate::config::validate_drive_generation(&s).map_err(TapectlError::Config)?;
            let canonical = crate::media::Generation::parse(&s)
                .expect("validate_drive_generation already confirmed this parses")
                .as_str();
            Ok(Some(Value::from(canonical)))
        }
        "device_tape" | "device_sg" => {
            let s = as_string();
            if !Path::new(&s).exists() {
                warnings.push(format!(
                    "warning: {field} {s} does not exist on this machine. Writing it anyway; \
                     check `ls -l /dev/tape/by-id/` (and `lsscsi -g` for the sg node) if that \
                     is not deliberate."
                ));
            }
            Ok(Some(Value::from(s)))
        }
        _ => Ok(None),
    }
}

// --------------------------------------------------------------------- set

fn plan_set(
    doc: &DocumentMut,
    original: &str,
    path: &Path,
    before: &LenientReport,
    edit: &Edit,
    segs: &[Seg],
    raw: &str,
) -> Result<Planned> {
    let (last, parents) = segs.split_last().expect("parse_key yields at least one");
    if last.select.is_some() {
        return Err(TapectlError::Config(format!(
            "{} names a table — set one of its keys, e.g. {}.<key>",
            edit.key(),
            edit.key()
        )));
    }
    // Structural errors first, against the file as it is.
    let previous: Option<Value> = {
        let mut probe = doc.clone();
        let table = descend(probe.as_table_mut(), parents, true)?;
        match table.get(&last.key) {
            None => None,
            Some(Item::Value(v)) if v.is_inline_table() => {
                return Err(TapectlError::Config(format!(
                    "{} is an inline table — set its keys one at a time",
                    edit.key()
                )))
            }
            Some(Item::Value(v)) => Some(v.clone()),
            Some(Item::ArrayOfTables(_)) => {
                return Err(TapectlError::Config(format!(
                    "{} is a list of tables — use `config add {}` and `config remove {}[<name>]`",
                    edit.key(),
                    edit.key(),
                    edit.key()
                )))
            }
            Some(_) => {
                return Err(TapectlError::Config(format!(
                    "{} is a table — set one of its keys, e.g. {}.<key>",
                    edit.key(),
                    edit.key()
                )))
            }
        }
    };
    let mut warnings = Vec::new();
    let slot = match is_backend_table(parents) {
        true => match backend_field(&last.key, raw, &mut warnings)? {
            Some(v) => vec![v],
            None => candidates(raw)?,
        },
        false => candidates(raw)?,
    };
    let apply = |d: &mut DocumentMut, vals: &[Value]| -> Result<()> {
        let table = descend(d.as_table_mut(), parents, true)?;
        let mut v = vals[0].clone();
        match table.get_mut(&last.key) {
            Some(Item::Value(old)) => {
                // Keep the spacing and any trailing comment on the line.
                *v.decor_mut() = old.decor().clone();
                *old = v;
            }
            _ => {
                table.insert(&last.key, Item::Value(v));
            }
        }
        Ok(())
    };
    let (text, chosen, after) = search(doc, path, before, edit, &[slot], &apply)?;
    let value = toml_text(&chosen[0]);
    let was = match &previous {
        Some(p) => format!("was {}", toml_text(p)),
        None => "was unset".to_string(),
    };
    let summary = format!("set {} = {value} ({was})", edit.key());
    let json = serde_json::json!({
        "action": "set",
        "key": edit.key(),
        "value": value,
        "previous": previous.as_ref().map(toml_text),
    });
    Ok(finish(original, text, after, summary, json, warnings))
}

// --------------------------------------------------------------------- add

fn plan_add(
    doc: &DocumentMut,
    original: &str,
    path: &Path,
    before: &LenientReport,
    edit: &Edit,
    segs: &[Seg],
    raws: &[String],
) -> Result<Planned> {
    let (last, parents) = segs.split_last().expect("parse_key yields at least one");
    if last.select.is_some() {
        return Err(TapectlError::Config(format!(
            "{} names one entry — add to the list itself ({}), or set the entry's keys",
            edit.key(),
            dotted(&segs[..segs.len() - 1])
                .split('.')
                .filter(|s| !s.is_empty())
                .chain(std::iter::once(last.key.as_str()))
                .collect::<Vec<_>>()
                .join(".")
        )));
    }
    if raws.is_empty() {
        return Err(TapectlError::Config(format!(
            "config add {} needs at least one value",
            edit.key()
        )));
    }
    // What is there decides between a table and a value; with nothing
    // there (or an empty `[]`, the stub an older `init` wrote), fields
    // given as `field=value` mean a table.
    let all_fields = raws.iter().all(|r| field_split(r).is_some());
    let as_table = {
        let mut probe = doc.clone();
        let table = descend(probe.as_table_mut(), parents, true)?;
        match table.get(&last.key) {
            None => all_fields,
            Some(Item::ArrayOfTables(_)) => true,
            Some(Item::Value(Value::Array(a))) if a.is_empty() => all_fields,
            Some(Item::Value(Value::Array(_))) => false,
            Some(_) => {
                return Err(TapectlError::Config(format!(
                    "{} is not a list — use `config set` for a single value",
                    edit.key()
                )))
            }
        }
    };
    if as_table {
        plan_add_table(doc, original, path, before, edit, segs, raws)
    } else {
        plan_add_values(doc, original, path, before, edit, segs, raws)
    }
}

fn field_split(raw: &str) -> Option<(&str, &str)> {
    let (k, v) = raw.split_once('=')?;
    let k = k.trim();
    (!k.is_empty()
        && k.chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-'))
    .then_some((k, v))
}

fn plan_add_table(
    doc: &DocumentMut,
    original: &str,
    path: &Path,
    before: &LenientReport,
    edit: &Edit,
    segs: &[Seg],
    raws: &[String],
) -> Result<Planned> {
    let (last, parents) = segs.split_last().expect("parse_key yields at least one");
    let backend = is_backend_table(segs);
    let mut fields: Vec<String> = Vec::new();
    let mut slots: Vec<Vec<Value>> = Vec::new();
    let mut warnings = Vec::new();
    for raw in raws {
        let (k, v) = field_split(raw).ok_or_else(|| {
            TapectlError::Config(format!(
                "\"{raw}\": a table is added as field=value pairs, e.g. name=movies"
            ))
        })?;
        if fields.iter().any(|f| f == k) {
            return Err(TapectlError::Config(format!("{k} is given twice")));
        }
        let slot = match backend {
            true => match backend_field(k, v, &mut warnings)? {
                Some(val) => vec![val],
                None => candidates(v)?,
            },
            false => candidates(v)?,
        };
        fields.push(k.to_string());
        slots.push(slot);
    }
    // An entry is addressed by its name, so a second entry with one already
    // taken could never be selected (`drive add` refuses it too).
    if let Some(i) = fields.iter().position(|f| f == "name") {
        let name = slots[i].last().and_then(|v| v.as_str()).unwrap_or_default();
        let mut probe = doc.clone();
        let table = descend(probe.as_table_mut(), parents, true)?;
        if let Some(Item::ArrayOfTables(aot)) = table.get(&last.key) {
            if let Some(at) = aot
                .iter()
                .position(|t| t.get("name").and_then(|v| v.as_str()) == Some(name))
            {
                return Err(TapectlError::Config(format!(
                    "{} already has an entry named \"{name}\" ({}[{at}]) — names select \
                     entries, so they must be unique; choose another name or edit that entry",
                    edit.key(),
                    edit.key()
                )));
            }
        }
    }
    let apply = |d: &mut DocumentMut, vals: &[Value]| -> Result<()> {
        let parent = descend(d.as_table_mut(), parents, true)?;
        let mut t = Table::new();
        for (k, v) in fields.iter().zip(vals) {
            t.insert(k, Item::Value(v.clone()));
        }
        match parent.get_mut(&last.key) {
            Some(Item::ArrayOfTables(aot)) => aot.push(t),
            _ => {
                let mut aot = ArrayOfTables::new();
                aot.push(t);
                parent.insert(&last.key, Item::ArrayOfTables(aot));
            }
        }
        Ok(())
    };
    let (text, chosen, after) = search(doc, path, before, edit, &slots, &apply)?;
    let name = fields
        .iter()
        .position(|f| f == "name")
        .and_then(|i| chosen[i].as_str().map(str::to_string));
    let entry = match &name {
        Some(n) => format!("{}[{n}]", edit.key()),
        None => format!("an entry to {}", edit.key()),
    };
    let summary = format!("add {entry}");
    let json = serde_json::json!({
        "action": "add",
        "key": edit.key(),
        "table": fields.iter().zip(&chosen)
            .map(|(k, v)| (k.clone(), serde_json::Value::String(toml_text(v))))
            .collect::<serde_json::Map<_, _>>(),
    });
    Ok(finish(original, text, after, summary, json, warnings))
}

fn plan_add_values(
    doc: &DocumentMut,
    original: &str,
    path: &Path,
    before: &LenientReport,
    edit: &Edit,
    segs: &[Seg],
    raws: &[String],
) -> Result<Planned> {
    let (last, parents) = segs.split_last().expect("parse_key yields at least one");
    let slots: Vec<Vec<Value>> = raws.iter().map(|r| candidates(r)).collect::<Result<_>>()?;
    let apply = |d: &mut DocumentMut, vals: &[Value]| -> Result<()> {
        let parent = descend(d.as_table_mut(), parents, true)?;
        if !parent.contains_key(&last.key) {
            parent.insert(
                &last.key,
                Item::Value(Value::Array(toml_edit::Array::new())),
            );
        }
        let arr = parent
            .get_mut(&last.key)
            .and_then(|i| i.as_array_mut())
            .expect("checked or just inserted as an array");
        for v in vals {
            // Lay a new value out like the last one, so a list written one
            // value per line stays that way.
            let mut v = v.clone();
            if let Some(prev) = arr.iter().last() {
                *v.decor_mut() = prev.decor().clone();
            }
            arr.push_formatted(v);
        }
        Ok(())
    };
    let (text, chosen, after) = search(doc, path, before, edit, &slots, &apply)?;
    let shown: Vec<String> = chosen.iter().map(toml_text).collect();
    let summary = format!("add {} to {}", shown.join(", "), edit.key());
    let json = serde_json::json!({"action": "add", "key": edit.key(), "values": shown});
    Ok(finish(original, text, after, summary, json, Vec::new()))
}

// ------------------------------------------------------------------ remove
//
// toml_edit hangs the comment lines above a table header or a key on that
// item, as its prefix. Removing the item whole would take them with it —
// and they are often not its own: init's commented `[[backends.lto]]`
// example sits above `# [host_check]`, so uncommenting that header made the
// drive example its prefix. Before an item goes, the comments above where
// it began are handed to whatever the file prints next (the next header or
// key, or the text after the last table), so the file reads as before less
// the removed lines. Comments inside the removed table go with it.

/// One step of a path to a table: a key, and the entry it selects when the
/// key holds a list of tables.
#[derive(Debug, Clone, PartialEq)]
struct Step {
    key: String,
    index: Option<usize>,
}

/// Something the file prints with a prefix of its own: a table's header,
/// or a `key = value` line in a table.
#[derive(Debug, Clone)]
enum Slot {
    Header(Vec<Step>),
    Key(Vec<Step>, String),
}

/// What a removal takes out of the file.
enum Removed {
    /// A table or one list entry, with everything under it.
    Subtree(Vec<Step>),
    /// One `key = value` line.
    Key(Vec<Step>, String),
}

impl Removed {
    fn takes(&self, slot: &Slot) -> bool {
        match (self, slot) {
            (Removed::Subtree(p), Slot::Header(t) | Slot::Key(t, _)) => t.starts_with(p),
            (Removed::Key(p, k), Slot::Key(t, j)) => p == t && k == j,
            (Removed::Key(..), Slot::Header(_)) => false,
        }
    }
}

/// The steps `segs` take through `root`, with each name selector resolved
/// to its entry's position. `None` when the path is not there as tables;
/// [`descend`] names that problem.
fn steps_of(root: &Table, segs: &[Seg]) -> Option<Vec<Step>> {
    let mut cur = root;
    let mut out = Vec::new();
    for seg in segs {
        cur = match (&seg.select, cur.get(&seg.key)?) {
            (None, Item::Table(t)) => {
                out.push(Step {
                    key: seg.key.clone(),
                    index: None,
                });
                t
            }
            (Some(sel), Item::ArrayOfTables(aot)) => {
                let idx = select(aot, sel, &seg.key).ok()?;
                out.push(Step {
                    key: seg.key.clone(),
                    index: Some(idx),
                });
                aot.get(idx)?
            }
            _ => return None,
        };
    }
    Some(out)
}

/// The table `path` leads to from `root`.
fn table_at<'a>(root: &'a Table, path: &[Step]) -> Option<&'a Table> {
    let mut cur = root;
    for step in path {
        cur = match (step.index, cur.get(&step.key)?) {
            (None, Item::Table(t)) => t,
            (Some(i), Item::ArrayOfTables(aot)) => aot.get(i)?,
            _ => return None,
        };
    }
    Some(cur)
}

/// The headers and keys of `doc` in the order the file prints them — the
/// order `toml_edit`'s own `Display` uses: tables by document position (a
/// table built in memory follows the one before it), each header (unless
/// an implicit table with no values hides it) followed by its keys.
fn render_order(doc: &DocumentMut) -> Vec<Slot> {
    fn walk<'a>(t: &'a Table, path: &mut Vec<Step>, out: &mut Vec<(Vec<Step>, &'a Table)>) {
        if !t.is_dotted() {
            out.push((path.clone(), t));
        }
        for (k, item) in t.iter() {
            match item {
                Item::Table(sub) => {
                    path.push(Step {
                        key: k.to_string(),
                        index: None,
                    });
                    walk(sub, path, out);
                    path.pop();
                }
                Item::ArrayOfTables(aot) => {
                    for (i, sub) in aot.iter().enumerate() {
                        path.push(Step {
                            key: k.to_string(),
                            index: Some(i),
                        });
                        walk(sub, path, out);
                        path.pop();
                    }
                }
                _ => {}
            }
        }
    }
    let mut tables = Vec::new();
    walk(doc.as_table(), &mut Vec::new(), &mut tables);
    let mut last = 0;
    let mut ordered: Vec<(usize, Vec<Step>, &Table)> = tables
        .into_iter()
        .map(|(path, t)| {
            if let Some(pos) = t.position() {
                last = pos;
            }
            (last, path, t)
        })
        .collect();
    ordered.sort_by_key(|(pos, _, _)| *pos);
    let mut out = Vec::new();
    for (_, path, t) in ordered {
        let entry = path.last().is_some_and(|s| s.index.is_some());
        if !path.is_empty() && (entry || !(t.is_implicit() && t.get_values().is_empty())) {
            out.push(Slot::Header(path.clone()));
        }
        for (k, item) in t.iter() {
            if item.is_value() {
                out.push(Slot::Key(path.clone(), k.to_string()));
            }
        }
    }
    out
}

/// Run `f` on the prefix decor of `slot` in `doc`; `false` when the slot is
/// not there.
fn with_prefix(doc: &mut DocumentMut, slot: &Slot, f: impl FnOnce(&mut toml_edit::Decor)) -> bool {
    let (path, key) = match slot {
        Slot::Header(p) => (p, None),
        Slot::Key(p, k) => (p, Some(k)),
    };
    let mut cur = doc.as_table_mut();
    for step in path {
        let Some(item) = cur.get_mut(&step.key) else {
            return false;
        };
        cur = match (step.index, item) {
            (None, Item::Table(t)) => t,
            (Some(i), Item::ArrayOfTables(aot)) => match aot.get_mut(i) {
                Some(t) => t,
                None => return false,
            },
            _ => return false,
        };
    }
    match key {
        None => f(cur.decor_mut()),
        Some(k) => match cur.key_mut(k) {
            Some(mut km) => f(km.leaf_decor_mut()),
            None => return false,
        },
    }
    true
}

fn prefix_text(decor: &toml_edit::Decor) -> Option<String> {
    decor
        .prefix()
        .and_then(|raw| raw.as_str())
        .map(str::to_string)
}

/// Before `removed` leaves `doc`, hand the comments above where it begins to
/// whatever the file prints next. A prefix of blank lines alone is not
/// carried, so removing a table does not leave its spacing behind.
fn keep_leading_comments(doc: &mut DocumentMut, removed: &Removed) {
    let order = render_order(doc);
    let Some(first) = order.iter().position(|s| removed.takes(s)) else {
        return;
    };
    let mut carried = String::new();
    with_prefix(doc, &order[first], |d| {
        carried = prefix_text(d).unwrap_or_default();
    });
    if !carried.contains('#') {
        return;
    }
    match order[first + 1..].iter().find(|s| !removed.takes(s)) {
        Some(next) => {
            // What toml_edit prints for a prefix that was never set.
            let default = match next {
                Slot::Header(_) => "\n",
                Slot::Key(..) => "",
            };
            with_prefix(doc, next, |d| {
                let old = prefix_text(d).unwrap_or_else(|| default.to_string());
                d.set_prefix(format!("{carried}{old}"));
            });
        }
        None => {
            let trailing = doc.trailing().as_str().unwrap_or("").to_string();
            doc.set_trailing(format!("{carried}{trailing}"));
        }
    }
}

fn plan_remove(
    doc: &DocumentMut,
    original: &str,
    path: &Path,
    before: &LenientReport,
    edit: &Edit,
    segs: &[Seg],
    raws: &[String],
) -> Result<Planned> {
    let (last, parents) = segs.split_last().expect("parse_key yields at least one");
    // A key that is not there, and that no reader knows, is most likely a
    // typo: say which key was meant (ADR-0012 item 214).
    if !in_file(doc.as_table(), segs)? {
        let near = suggestions_for(segs);
        if !near.is_empty() {
            return Err(TapectlError::Config(format!(
                "{} is not in {}, so there is nothing to remove{}",
                edit.key(),
                path.display(),
                did_you_mean(&near)
            )));
        }
    }
    let mut d = doc.clone();
    // What goes, worked out against the file as it is; a path that is not
    // there is refused below, by name.
    if raws.is_empty() {
        let removed = steps_of(doc.as_table(), parents).and_then(|mut path| {
            let parent = table_at(doc.as_table(), &path)?;
            match (&last.select, parent.get(&last.key)?) {
                (Some(sel), Item::ArrayOfTables(aot)) => {
                    let idx = select(aot, sel, &last.key).ok()?;
                    path.push(Step {
                        key: last.key.clone(),
                        index: Some(idx),
                    });
                    Some(Removed::Subtree(path))
                }
                (None, Item::Table(_)) => {
                    path.push(Step {
                        key: last.key.clone(),
                        index: None,
                    });
                    Some(Removed::Subtree(path))
                }
                (None, Item::Value(_)) => Some(Removed::Key(path, last.key.clone())),
                _ => None,
            }
        });
        if let Some(removed) = removed {
            keep_leading_comments(&mut d, &removed);
        }
    }
    let summary;
    let json;
    {
        let parent = descend(d.as_table_mut(), parents, false)?;
        let plain = dotted(parents)
            .split('.')
            .filter(|s| !s.is_empty())
            .chain(std::iter::once(last.key.as_str()))
            .collect::<Vec<_>>()
            .join(".");
        let Some(item) = parent.get_mut(&last.key) else {
            return Err(TapectlError::Config(format!(
                "{plain} is not in {}, so there is nothing to remove",
                path.display()
            )));
        };
        match (&last.select, raws.is_empty()) {
            (Some(sel), true) => {
                let Item::ArrayOfTables(aot) = item else {
                    return Err(TapectlError::Config(format!(
                        "{plain} is not a list of tables, so it has no entries to select"
                    )));
                };
                let idx = select(aot, sel, &plain)?;
                aot.remove(idx);
                if aot.is_empty() {
                    parent.remove(&last.key);
                }
                summary = format!("remove {}", edit.key());
                json = serde_json::json!({"action": "remove", "key": edit.key()});
            }
            (Some(_), false) => {
                return Err(TapectlError::Config(format!(
                    "{} names a table; values are removed from a list, e.g. \
                     `config remove defaults.global_excludes '*.bak'`",
                    edit.key()
                )))
            }
            (None, true) => {
                if let Item::ArrayOfTables(aot) = item {
                    return Err(TapectlError::Config(format!(
                        "{plain} is a list of tables — remove one entry at a time: \
                         {plain}[<name>] or {plain}[<index>] (it has: {})",
                        names(aot)
                    )));
                }
                let previous = item.as_value().map(toml_text);
                parent.remove(&last.key);
                summary = match &previous {
                    Some(p) => format!("remove {} (was {p})", edit.key()),
                    None => format!("remove the table {}", edit.key()),
                };
                json = serde_json::json!({"action": "remove", "key": edit.key(),
                                          "previous": previous});
            }
            (None, false) => {
                let Some(arr) = item.as_array_mut() else {
                    return Err(TapectlError::Config(format!(
                        "{plain} is not a list, so values cannot be removed from it — \
                         `config remove {plain}` removes the key"
                    )));
                };
                let first_prefix = arr.get(0).map(|v| v.decor().clone());
                let mut removed = Vec::new();
                for raw in raws {
                    let wanted = candidates(raw)?;
                    let before_len = arr.len();
                    arr.retain(|v| !wanted.iter().any(|w| same_value(v, w)));
                    if arr.len() == before_len {
                        return Err(TapectlError::Config(format!(
                            "{raw} is not in {plain}, so there is nothing to remove"
                        )));
                    }
                    removed.push(raw.clone());
                }
                // Keep the list's opening layout when its first value went.
                if let (Some(decor), Some(first)) = (first_prefix, arr.get_mut(0)) {
                    if let Some(prefix) = decor.prefix() {
                        first.decor_mut().set_prefix(prefix.clone());
                    }
                }
                summary = format!("remove {} from {}", removed.join(", "), edit.key());
                json = serde_json::json!({"action": "remove", "key": edit.key(),
                                          "values": removed});
            }
        }
    }
    let (text, _, after) = search(
        doc,
        path,
        before,
        edit,
        &[vec![Value::from("")]],
        &|out, _| {
            *out = d.clone();
            Ok(())
        },
    )?;
    Ok(finish(original, text, after, summary, json, Vec::new()))
}

// --------------------------------------------------------------------- get

/// Where the value `config get` gives came from (ADR-0012 item 214).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Source {
    /// config.toml sets it.
    File,
    /// config.toml does not set it: the value tapectl uses then, or none at
    /// all for an optional key ([`Got::value`] is `None`).
    Default,
    /// config.toml does not set it, and tapectl works it out from another
    /// key: what from, as a phrase.
    Derived(&'static str),
}

impl Source {
    /// The word `--json` gives: `config.toml`, `default` or `derived`.
    pub fn word(&self) -> &'static str {
        match self {
            Source::File => "config.toml",
            Source::Default => "default",
            Source::Derived(_) => "derived",
        }
    }
}

/// `config get KEY`'s answer.
#[derive(Debug, Clone)]
pub struct Got {
    /// The key it resolved to: `backends.lto[lto6].post_warning_bytes` for
    /// `post_warning_bytes`.
    pub key: String,
    /// The value in force, as JSON; `None` for an optional key nothing sets.
    pub value: Option<serde_json::Value>,
    pub source: Source,
}

/// The value `key` has in `effective` — the config every command loads from
/// the file whose text is `original` — and where it came from: the file,
/// a default, or derived (a drive's `device_sg`, from its `device_tape`,
/// issue #431). `key` resolves as [`resolve_key`] resolves it; a key the
/// schema does not know is refused with a did-you-mean, and a table is
/// refused naming its keys.
pub fn get(original: &str, effective: &crate::config::Config, key: &str) -> Result<Got> {
    let doc: DocumentMut = original
        .parse()
        .map_err(|e: toml_edit::TomlError| TapectlError::Config(e.message().to_string()))?;
    let segs = resolve_key(doc.as_table(), key)?;
    let shown = dotted(&segs);
    let names: Vec<String> = segs.iter().map(|s| s.key.clone()).collect();
    match known(&names) {
        Known::Value | Known::MapEntry => {}
        Known::Table(fields) => {
            let plain = names.join(".");
            return Err(TapectlError::Config(
                if shape_of(&plain) == Some(Shape::List) && segs.last().is_some_and(|s| s.select.is_none()) {
                    format!(
                        "{shown} is a list of tables: get a key of one entry, e.g. \
                         {shown}[<name>].{}",
                        fields.first().map(String::as_str).unwrap_or("name")
                    )
                } else {
                    format!(
                        "{shown} is a table: get one of its keys ({})",
                        fields
                            .iter()
                            .map(|f| format!("{shown}.{f}"))
                            .collect::<Vec<_>>()
                            .join(", ")
                    )
                },
            ));
        }
        Known::Unknown { .. } => {
            return Err(TapectlError::Config(format!(
                "{shown} is not a config key{}",
                did_you_mean(&suggestions_for(&segs))
            )))
        }
    }
    for (i, seg) in segs.iter().enumerate() {
        let plain = names[..=i].join(".");
        if seg.select.is_some() && shape_of(&plain) != Some(Shape::List) {
            return Err(TapectlError::Config(format!(
                "{shown}: {plain} is not a list of tables, so it has no entries to select"
            )));
        }
    }
    let in_file = in_file(doc.as_table(), &segs)?;
    let value = value_in(&effective_json(effective), &segs);
    let source = if in_file {
        Source::File
    } else if value.is_some() && names == ["backends", "lto", "device_sg"] {
        Source::Derived("from device_tape: the sg node the kernel binds to it, read from sysfs")
    } else {
        Source::Default
    };
    Ok(Got {
        key: shown,
        value,
        source,
    })
}

/// Whether the file whose root table is `root` writes the key at `segs`
/// (as a key, a table, or a key of an inline table). An entry a selector
/// names that is not there is refused, naming the entries there are.
fn in_file(root: &Table, segs: &[Seg]) -> Result<bool> {
    let mut cur: &dyn toml_edit::TableLike = root;
    for (i, seg) in segs.iter().enumerate() {
        let last = i + 1 == segs.len();
        let Some(item) = cur.get(&seg.key) else {
            return Ok(false);
        };
        let plain = segs[..=i]
            .iter()
            .map(|s| s.key.as_str())
            .collect::<Vec<_>>()
            .join(".");
        let next: &dyn toml_edit::TableLike = match (&seg.select, item) {
            (Some(sel), Item::ArrayOfTables(aot)) => {
                let idx = select(aot, sel, &plain)?;
                aot.get(idx).expect("select returned an index in range")
            }
            (Some(_), _) => return Ok(false),
            (None, _) if last => return Ok(true),
            (None, it) => match it.as_table_like() {
                Some(t) => t,
                None => return Ok(false),
            },
        };
        if last {
            return Ok(true);
        }
        cur = next;
    }
    Ok(true)
}

/// `effective` as JSON, with the tables that are optional in the file
/// (`[host_check]`, `[health]`) at the values they take when absent.
fn effective_json(effective: &crate::config::Config) -> serde_json::Value {
    let mut c = effective.clone();
    c.host_check = Some(effective.host_check());
    c.health = Some(effective.health());
    serde_json::to_value(&c).unwrap_or(serde_json::Value::Null)
}

/// The value at `segs` in `json`; `None` when absent or null (unset).
fn value_in(json: &serde_json::Value, segs: &[Seg]) -> Option<serde_json::Value> {
    let mut cur = json;
    for seg in segs {
        cur = cur.get(&seg.key)?;
        if let Some(sel) = &seg.select {
            let entries = cur.as_array()?;
            cur = match sel {
                Selector::Index(i) => entries.get(*i)?,
                Selector::Name(n) => entries
                    .iter()
                    .find(|e| e.get("name").and_then(|v| v.as_str()) == Some(n.as_str()))?,
            };
        }
    }
    (!cur.is_null()).then(|| cur.clone())
}

/// A JSON value as TOML text, the way config.toml writes it: `"1G"`,
/// `0.97`, `["*.tmp"]`, `{ offsite = 2 }`.
pub fn toml_text_of(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::String(s) => Value::from(s.as_str()).to_string().trim().to_string(),
        serde_json::Value::Array(items) => format!(
            "[{}]",
            items.iter().map(toml_text_of).collect::<Vec<_>>().join(", ")
        ),
        serde_json::Value::Object(map) => {
            if map.is_empty() {
                return "{}".to_string();
            }
            format!(
                "{{ {} }}",
                map.iter()
                    .map(|(k, v)| format!("{k} = {}", toml_text_of(v)))
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        }
        other => other.to_string(),
    }
}

// ------------------------------------------------------------------- write

/// Replace the file at `path` with `new_text`, atomically: a temporary file
/// beside it, with its mode (and, where allowed, owner), synced and renamed
/// over it. A symlinked config is written through to its target. Refuses,
/// writing nothing, when the file no longer holds `expected` — another edit
/// got there first.
pub fn replace_file(path: &Path, expected: &str, new_text: &str) -> Result<()> {
    use std::io::Write;
    use std::os::unix::fs::MetadataExt;
    let target = std::fs::canonicalize(path)?;
    let current = std::fs::read_to_string(&target)?;
    if current != expected {
        return Err(TapectlError::Config(format!(
            "{} changed while this edit was being made; nothing was written — run the \
             command again",
            path.display()
        )));
    }
    let meta = std::fs::metadata(&target)?;
    let dir = target.parent().unwrap_or_else(|| Path::new("."));
    let mut tmp = tempfile::Builder::new()
        .prefix(".config-edit-")
        .suffix(".tmp")
        .tempfile_in(dir)?;
    tmp.write_all(new_text.as_bytes())?;
    tmp.as_file().set_permissions(meta.permissions())?;
    // Best effort, as `secure_path` is: only root may give a file away, and
    // a file already ours keeps its owner either way.
    let _ = std::os::unix::fs::fchown(tmp.as_file(), Some(meta.uid()), Some(meta.gid()));
    tmp.as_file().sync_all()?;
    tmp.persist(&target)
        .map_err(|e| TapectlError::Io(e.error))?;
    if let Ok(d) = std::fs::File::open(dir) {
        let _ = d.sync_all();
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p() -> std::path::PathBuf {
        std::path::PathBuf::from("/nonexistent/tapectl-config-edit/config.toml")
    }

    #[test]
    fn keys_parse_with_names_and_indexes() {
        let segs = parse_key("backends.lto[hp.lto6].enospc_buffer").unwrap();
        assert_eq!(segs.len(), 3);
        assert_eq!(segs[1].select, Some(Selector::Name("hp.lto6".into())));
        let segs = parse_key("archive_sets[1]").unwrap();
        assert_eq!(segs[0].select, Some(Selector::Index(1)));
        assert!(parse_key("").is_err());
        assert!(parse_key("a..b").is_err());
        assert!(parse_key("a[x").is_err());
        assert!(parse_key("a b").is_err());
    }

    #[test]
    fn a_trailing_comment_on_the_line_survives_a_set() {
        let original = "[defaults]\nslice_size = \"1G\"  # tuned for LTO-6\n";
        let planned = plan(
            original,
            &p(),
            &Edit::Set {
                key: "defaults.slice_size".into(),
                value: "2G".into(),
            },
        )
        .unwrap();
        assert_eq!(
            planned.new_text,
            "[defaults]\nslice_size = \"2G\"  # tuned for LTO-6\n"
        );
    }

    #[test]
    fn a_value_one_per_line_list_stays_one_per_line() {
        let original = "[defaults]\nglobal_excludes = [\n    \"*.nfo\",\n    \"*.tmp\",\n]\n";
        let planned = plan(
            original,
            &p(),
            &Edit::Add {
                key: "defaults.global_excludes".into(),
                values: vec!["*.bak".into()],
            },
        )
        .unwrap();
        assert_eq!(
            planned.new_text,
            "[defaults]\nglobal_excludes = [\n    \"*.nfo\",\n    \"*.tmp\",\n    \"*.bak\",\n]\n"
        );
        let planned = plan(
            &planned.new_text,
            &p(),
            &Edit::Remove {
                key: "defaults.global_excludes".into(),
                values: vec!["*.nfo".into()],
            },
        )
        .unwrap();
        assert_eq!(
            planned.new_text,
            "[defaults]\nglobal_excludes = [\n    \"*.tmp\",\n    \"*.bak\",\n]\n"
        );
    }

    #[test]
    fn a_quoted_value_is_only_a_string() {
        let planned = plan(
            "",
            &p(),
            &Edit::Set {
                key: "dar.binary".into(),
                value: "\"3\"".into(),
            },
        )
        .unwrap();
        assert!(
            planned.new_text.contains("binary = \"3\""),
            "{}",
            planned.new_text
        );
        // Unquoted, a number for a string key falls back to the string.
        let planned = plan(
            "",
            &p(),
            &Edit::Set {
                key: "dar.binary".into(),
                value: "3".into(),
            },
        )
        .unwrap();
        assert!(
            planned.new_text.contains("binary = \"3\""),
            "{}",
            planned.new_text
        );
    }

    #[test]
    fn a_number_that_fails_both_ways_reports_the_literal_reading() {
        let err = plan(
            "",
            &p(),
            &Edit::Set {
                key: "compaction.utilization_threshold".into(),
                value: "7".into(),
            },
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("utilization_threshold"), "{err}");
        assert!(err.contains("unchanged"), "{err}");
    }

    #[test]
    fn replace_file_keeps_the_mode_and_refuses_a_file_that_moved() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::TempDir::new().unwrap();
        let file = dir.path().join("config.toml");
        std::fs::write(&file, "a = 1\n").unwrap();
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o640)).unwrap();

        replace_file(&file, "a = 1\n", "a = 2\n").unwrap();
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "a = 2\n");
        let mode = std::fs::metadata(&file).unwrap().permissions().mode() & 0o7777;
        assert_eq!(mode, 0o640, "the replacement keeps the file's mode");

        // Someone else's edit landed in between: nothing is written.
        let err = replace_file(&file, "a = 1\n", "a = 3\n").unwrap_err();
        assert!(err.to_string().contains("changed"), "{err}");
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "a = 2\n");
        // And no temporary file is left behind.
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
    }

    fn remove(original: &str, key: &str) -> String {
        plan(
            original,
            &p(),
            &Edit::Remove {
                key: key.into(),
                values: Vec::new(),
            },
        )
        .unwrap()
        .new_text
    }

    /// The review's reproduction: init's own template with `[host_check]`
    /// uncommented. The comments above that header were the table's prefix,
    /// and removing the table took the drive example with it.
    #[test]
    fn removing_a_table_keeps_the_comments_above_its_header() {
        let original = format!(
            "[defaults]\nslice_size = \"1G\"\n{}{}",
            crate::config::LTO_BACKEND_EXAMPLE,
            crate::config::HOST_CHECK_EXAMPLE.replacen("# [host_check]", "[host_check]", 1)
        );
        assert!(original.contains("\n[host_check]\n"), "precondition");
        let after = remove(&original, "host_check");
        assert_eq!(after, original.replacen("[host_check]\n", "", 1));
        assert!(after.contains("Tape drive. Uncomment and edit"), "{after}");
        assert!(after.contains("# [[backends.lto]]"), "{after}");
    }

    #[test]
    fn removing_a_key_hands_its_comment_to_what_follows() {
        let original = "[defaults]\n# sized for LTO-6\nslice_size = \"1G\"\nglobal_excludes = []\n";
        assert_eq!(
            remove(original, "defaults.slice_size"),
            "[defaults]\n# sized for LTO-6\nglobal_excludes = []\n"
        );
        // The last key of a table: the comment goes to the next header.
        let original =
            "[defaults]\n# sized for LTO-6\nslice_size = \"1G\"\n\n[staging]\njobs = 2\n";
        assert_eq!(
            remove(original, "defaults.slice_size"),
            "[defaults]\n# sized for LTO-6\n\n[staging]\njobs = 2\n"
        );
    }

    #[test]
    fn removing_the_last_entry_of_a_list_keeps_the_comment_above_it() {
        let head = "[defaults]\nslice_size = \"1G\"\n\n# drive notes, kept\n";
        let original =
            format!("{head}[[collections]]\nname = \"m\"\nroot = \"/x\"\ntenant = \"t\"\n");
        assert_eq!(
            plan_collection_remove(&original, &p(), "m")
                .unwrap()
                .new_text,
            head
        );
    }

    #[test]
    fn removing_one_entry_hands_its_comment_to_the_next() {
        let original = "[defaults]\nslice_size = \"1G\"\n\n# first, kept\n\
                        [[collections]]\nname = \"a\"\nroot = \"/a\"\ntenant = \"t\"\n\n\
                        [[collections]]\nname = \"b\"\nroot = \"/b\"\ntenant = \"t\"\n";
        assert_eq!(
            plan_collection_remove(original, &p(), "a")
                .unwrap()
                .new_text,
            "[defaults]\nslice_size = \"1G\"\n\n# first, kept\n\n\
             [[collections]]\nname = \"b\"\nroot = \"/b\"\ntenant = \"t\"\n"
        );
    }

    /// ADR-0012 item 206: a whole collection is added and removed by
    /// `collection add` and `collection remove`, never by `config add` or
    /// `config remove`, which refuse naming them; a key of one collection
    /// is still `config`'s to edit, and `collection add`'s own write goes
    /// through.
    #[test]
    fn a_whole_collection_is_refused_by_name_and_its_keys_are_not() {
        let original = "[defaults]\nslice_size = \"1G\"\n\n\
                        [[collections]]\nname = \"a\"\nroot = \"/a\"\ntenant = \"t\"\n";
        let add = Edit::Add {
            key: "collections".into(),
            values: vec!["name=b".into(), "root=/b".into(), "tenant=t".into()],
        };
        let err = plan(original, &p(), &add).unwrap_err().to_string();
        assert!(
            err.contains("tapectl collection add NAME ROOT --tenant TENANT"),
            "{err}"
        );
        for key in ["collections[a]", "collections[0]", "collections"] {
            let remove = Edit::Remove {
                key: key.into(),
                values: Vec::new(),
            };
            let err = plan(original, &p(), &remove).unwrap_err().to_string();
            assert!(
                err.contains("tapectl collection remove NAME"),
                "{key}: {err}"
            );
        }

        // A key of one collection is config's.
        let exclude = Edit::Add {
            key: "collections[a].exclude".into(),
            values: vec!["loose.txt".into()],
        };
        let after = plan(original, &p(), &exclude).unwrap().new_text;
        assert!(after.contains("exclude = [\"loose.txt\"]"), "{after}");
        let depth = Edit::Set {
            key: "collections[a].unit_depth".into(),
            value: "2".into(),
        };
        assert!(plan(original, &p(), &depth)
            .unwrap()
            .new_text
            .contains("unit_depth = 2"));

        // `collection add`'s own write.
        let after = plan_collection_add(
            original,
            &p(),
            vec![
                "name=\"2024\"".into(),
                "root=\"/b\"".into(),
                "tenant=\"t\"".into(),
            ],
        )
        .unwrap()
        .new_text;
        assert!(
            after.ends_with("[[collections]]\nname = \"2024\"\nroot = \"/b\"\ntenant = \"t\"\n"),
            "{after}"
        );
    }

    #[test]
    fn whitespace_alone_is_not_carried() {
        let original =
            "[defaults]\nslice_size = \"1G\"\n\n[staging]\njobs = 2\n\n[dar]\nbinary = \"dar\"\n";
        assert_eq!(
            remove(original, "staging"),
            "[defaults]\nslice_size = \"1G\"\n\n[dar]\nbinary = \"dar\"\n"
        );
    }

    #[test]
    fn an_edit_under_an_unknown_key_is_refused_even_on_a_broken_file() {
        for (original, key) in [
            ("[nonsense]\na = 1\n", "nonsense.evil"),
            ("[defaults]\ntypo = 1\n", "defaults.typo"),
            ("[[typos]]\nname = \"x\"\n", "typos[x].a"),
        ] {
            let err = plan(
                original,
                &p(),
                &Edit::Set {
                    key: key.into(),
                    value: "2".into(),
                },
            )
            .unwrap_err()
            .to_string();
            assert!(err.contains("unknown key"), "{key}: {err}");
        }
        let err = plan(
            "[defaults]\ntypo_list = []\n",
            &p(),
            &Edit::Add {
                key: "defaults.typo_list".into(),
                values: vec!["a".into()],
            },
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("defaults.typo_list"), "{err}");
        // Removing the unknown key is what the allowance is for.
        assert_eq!(
            remove("[defaults]\ntypo = 1\n", "defaults.typo"),
            "[defaults]\n"
        );
    }

    // ---- ADR-0012 item 214: the schema, key resolution, plain errors ----

    fn doc(text: &str) -> DocumentMut {
        text.parse().unwrap()
    }

    const ONE_DRIVE: &str = "[[backends.lto]]\nname = \"lto6\"\n\
                             device_tape = \"/dev/tapectl-config-edit-nonexistent\"\n\
                             generation = \"LTO-6\"\n";

    fn two_drives() -> String {
        format!(
            "{ONE_DRIVE}\n[[backends.lto]]\nname = \"spare\"\n\
             device_tape = \"/dev/tapectl-config-edit-nonexistent-2\"\ngeneration = \"LTO-6\"\n"
        )
    }

    fn resolved(text: &str, key: &str) -> Result<String> {
        resolve_key(doc(text).as_table(), key).map(|segs| dotted(&segs))
    }

    /// The tables [`TABLES`] lists are [`crate::config::Config`]'s own, each
    /// written as listed: every key of every listed table that is itself a
    /// table (one, a list, or a map, told apart by what serde says of a
    /// probe) is listed with that shape, and every listed table is a key of
    /// its parent. A table added to `Config` and not here fails this.
    #[test]
    fn the_schema_s_tables_are_config_s_tables() {
        fn probe(text: &str) -> std::result::Result<(), String> {
            toml::from_str::<crate::config::Config>(text)
                .map(|_| ())
                .map_err(|e| e.message().to_string())
        }
        let unknown = format!("unknown field `{PROBE_KEY}`");
        for (table, shape) in TABLES {
            if *shape == Shape::Map {
                continue;
            }
            let fields = table_fields(table, *shape);
            assert!(!fields.is_empty(), "{table:?} names no keys");
            // The header that opens an entry of the parent, for a key under
            // a list of tables.
            let opener = match shape {
                Shape::List => format!("[[{table}]]\n"),
                _ => String::new(),
            };
            for field in fields {
                let full = if table.is_empty() {
                    field.clone()
                } else {
                    format!("{table}.{field}")
                };
                let as_table = probe(&format!("{opener}[{full}]\n{PROBE_KEY} = 0\n"));
                let found = match &as_table {
                    Err(m) if m.starts_with(&unknown) => Some(Shape::Table),
                    Err(m) if m.starts_with("invalid type: map") => {
                        match probe(&format!("{opener}[[{full}]]\n{PROBE_KEY} = 0\n")) {
                            Err(m) if m.starts_with(&unknown) => Some(Shape::List),
                            _ => None,
                        }
                    }
                    // A map takes the probe's key as one of its own.
                    _ => Some(Shape::Map),
                };
                assert_eq!(
                    shape_of(&full),
                    found,
                    "{full}: serde says {as_table:?}; TABLES must list it as {found:?}"
                );
            }
        }
        for (table, _) in TABLES.iter().filter(|(t, _)| !t.is_empty()) {
            let (parent, name) = table.rsplit_once('.').unwrap_or(("", table));
            let shape = shape_of(parent).expect("a listed table's parent is listed");
            assert!(
                table_fields(parent, shape).iter().any(|f| f == name),
                "{table} is not a key of {parent:?}"
            );
        }
        // The keys the ruling names are among the schema's.
        let keys: Vec<String> = value_keys().iter().map(|k| k.join(".")).collect();
        for k in [
            "backends.lto.post_warning_bytes",
            "backends.lto.device_sg",
            "defaults.slice_size",
            "defaults.copies_at",
            "archive_sets.copies_at",
            "collections.exclude",
            "host_check.max_load_per_cpu",
            "ops.group",
        ] {
            assert!(keys.iter().any(|x| x == k), "{k} missing from {keys:?}");
        }
        assert!(!keys.iter().any(|x| x == "backends.lto"), "a table is not a value");
    }

    #[test]
    fn a_bare_key_resolves_to_the_one_key_of_that_name() {
        assert_eq!(
            resolved(ONE_DRIVE, "post_warning_bytes").unwrap(),
            "backends.lto[lto6].post_warning_bytes"
        );
        assert_eq!(
            resolved(ONE_DRIVE, "verify_interval_days").unwrap(),
            "defaults.verify_interval_days"
        );
        assert_eq!(
            resolved("", "max_load_per_cpu").unwrap(),
            "host_check.max_load_per_cpu"
        );
        // A key of several tables is one only while the file has one of
        // them: with an archive set, slice_size could be either.
        assert_eq!(resolved("", "slice_size").unwrap(), "defaults.slice_size");
        let with_set = "[[archive_sets]]\nname = \"cold\"\n";
        let err = resolved(with_set, "slice_size").unwrap_err().to_string();
        assert!(err.contains("ambiguous"), "{err}");
        assert!(err.contains("defaults.slice_size"), "{err}");
        assert!(err.contains("archive_sets[cold].slice_size"), "{err}");

        // Two drives: both named, nothing picked.
        let err = resolved(&two_drives(), "post_warning_bytes")
            .unwrap_err()
            .to_string();
        assert!(err.contains("backends.lto[lto6].post_warning_bytes"), "{err}");
        assert!(err.contains("backends.lto[spare].post_warning_bytes"), "{err}");
        // None: the drive is what is missing.
        let err = resolved("", "post_warning_bytes").unwrap_err().to_string();
        assert!(err.contains("no drive is configured"), "{err}");
        assert!(err.contains("`tapectl drive add"), "{err}");
        // A typo of a bare key.
        let err = resolved(ONE_DRIVE, "post_warning_byte")
            .unwrap_err()
            .to_string();
        assert!(err.contains("did you mean post_warning_bytes?"), "{err}");
    }

    #[test]
    fn a_list_of_tables_with_one_entry_needs_no_selector() {
        assert_eq!(
            resolved(ONE_DRIVE, "backends.lto.post_warning_bytes").unwrap(),
            "backends.lto[lto6].post_warning_bytes"
        );
        let err = resolved(&two_drives(), "backends.lto.enospc_buffer")
            .unwrap_err()
            .to_string();
        assert!(err.contains("backends.lto[lto6].enospc_buffer"), "{err}");
        assert!(err.contains("backends.lto[spare].enospc_buffer"), "{err}");
        let err = resolved("", "backends.lto.enospc_buffer")
            .unwrap_err()
            .to_string();
        assert!(err.contains("drive add"), "{err}");
        let one_set = "[[collections]]\nname = \"movies\"\nroot = \"/m\"\ntenant = \"t\"\n";
        assert_eq!(
            resolved(one_set, "collections.exclude").unwrap(),
            "collections[movies].exclude"
        );
    }

    /// The forms that worked before 1.9.0 resolve to themselves: first-run.sh
    /// sets `backends.lto[NAME].post_warning_bytes`, the collection refusals
    /// point at `collections[NAME].exclude`, and a whole table or list is
    /// still named by its own name.
    #[test]
    fn the_existing_key_forms_resolve_to_themselves() {
        let text = format!(
            "{}\n[[collections]]\nname = \"movies\"\nroot = \"/m\"\ntenant = \"t\"\n\n\
             [typo]\na = 1\n",
            two_drives()
        );
        for key in [
            "backends.lto[lto6].post_warning_bytes",
            "backends.lto[spare].enospc_buffer",
            "backends.lto[0].enospc_buffer",
            "collections[movies].exclude",
            "collections[0].unit_depth",
            "archive_sets[1]",
            "defaults.slice_size",
            "host_check",
            "backends.lto",
            "collections",
            "typo",
        ] {
            assert_eq!(resolved(&text, key).unwrap(), key, "{key}");
        }
        assert_eq!(
            resolved(&text, "backends.lto[hp.lto6].enospc_buffer").unwrap(),
            "backends.lto[hp.lto6].enospc_buffer"
        );
    }

    fn set_err(original: &str, key: &str, value: &str) -> String {
        plan(
            original,
            &p(),
            &Edit::Set {
                key: key.into(),
                value: value.into(),
            },
        )
        .unwrap_err()
        .to_string()
    }

    /// A refused set is one line naming the key and what was wrong: no
    /// excerpt of the file, no serde words for a type, and a typo names the
    /// key meant.
    #[test]
    fn a_refused_set_is_one_line_naming_the_key_and_what_was_wrong() {
        for (key, value, want) in [
            (
                "defaults.slice_sizee",
                "2G",
                "defaults.slice_sizee is not a config key — did you mean defaults.slice_size?",
            ),
            (
                "defaults.min_copies",
                "lots",
                "defaults.min_copies takes a whole number, not \"lots\"",
            ),
            (
                "defaults.encrypt",
                "maybe",
                "defaults.encrypt takes true or false, not \"maybe\"",
            ),
            (
                "compaction.utilization_threshold",
                "half",
                "compaction.utilization_threshold takes a number, not \"half\"",
            ),
            (
                "defalts.slice_size",
                "1G",
                "defalts.slice_size is not a config key — did you mean defaults.slice_size?",
            ),
        ] {
            let err = set_err("[defaults]\nslice_size = \"1G\"\n", key, value);
            assert!(err.contains(want), "{key}: {err}");
            assert!(!err.contains('\n'), "{key}: one line: {err}");
            assert!(!err.contains("TOML parse error"), "{key}: {err}");
            assert!(!err.contains(" | "), "{key}: no excerpt: {err}");
            assert!(!err.contains("expected one of"), "{key}: {err}");
            assert!(err.contains("unchanged"), "{key}: {err}");
        }
        // The text reading of a number explains a closed set better than
        // "takes text".
        let err = set_err("", "defaults.compression", "5");
        assert!(err.contains("defaults.compression"), "{err}");
        assert!(!err.contains("takes text"), "{err}");
        // A size names the forms it takes.
        let err = set_err(ONE_DRIVE, "post_warning_bytes", "plenty");
        assert!(err.contains("post_warning_bytes"), "{err}");
        assert!(err.contains("99008M"), "{err}");
    }

    #[test]
    fn a_bare_key_and_the_one_drive_shorthand_are_set_in_the_drive_s_table() {
        for key in ["post_warning_bytes", "backends.lto.post_warning_bytes"] {
            let planned = plan(
                ONE_DRIVE,
                &p(),
                &Edit::Set {
                    key: key.into(),
                    value: "99008M".into(),
                },
            )
            .unwrap();
            assert!(
                planned
                    .new_text
                    .ends_with("generation = \"LTO-6\"\npost_warning_bytes = \"99008M\"\n"),
                "{key}: {}",
                planned.new_text
            );
            assert_eq!(
                planned.summary,
                "set backends.lto[lto6].post_warning_bytes = \"99008M\" (was unset)"
            );
            assert_eq!(planned.json["key"], "backends.lto[lto6].post_warning_bytes");
        }
    }

    #[test]
    fn a_misspelt_key_gets_a_did_you_mean_from_add_and_remove() {
        let original = "[defaults]\nslice_size = \"1G\"\nglobal_excludes = []\n";
        let err = plan(
            original,
            &p(),
            &Edit::Add {
                key: "defaults.global_exclude".into(),
                values: vec!["*.bak".into()],
            },
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("did you mean defaults.global_excludes?"), "{err}");
        let err = plan(
            original,
            &p(),
            &Edit::Remove {
                key: "defaults.slice_sizee".into(),
                values: Vec::new(),
            },
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("nothing to remove"), "{err}");
        assert!(err.contains("did you mean defaults.slice_size?"), "{err}");
    }

    fn effective(text: &str) -> crate::config::Config {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, text).unwrap();
        crate::config::Config::load(&path).unwrap()
    }

    /// `config get`'s source: the file, a default, unset, or derived — the
    /// last through the sysfs derivation, faked under a temp dir.
    #[test]
    fn get_names_where_each_value_comes_from() {
        let text = format!("[defaults]\nmin_copies = 3\n\n{ONE_DRIVE}post_warning_bytes = \"99008M\"\n");
        let cfg = effective(&text);

        let got = get(&text, &cfg, "defaults.min_copies").unwrap();
        assert_eq!(got.value, Some(serde_json::json!(3)));
        assert_eq!(got.source, Source::File);

        let got = get(&text, &cfg, "slice_size").unwrap();
        assert_eq!(got.key, "defaults.slice_size");
        assert_eq!(got.value, Some(serde_json::json!("1G")));
        assert_eq!(got.source, Source::Default);

        let got = get(&text, &cfg, "post_warning_bytes").unwrap();
        assert_eq!(got.key, "backends.lto[lto6].post_warning_bytes");
        assert_eq!(got.value, Some(serde_json::json!("99008M")));
        assert_eq!(got.source, Source::File);

        let got = get(&text, &cfg, "backends.lto.capacity_override").unwrap();
        assert_eq!(got.value, None, "an optional key nothing sets");
        assert_eq!(got.source, Source::Default);

        // [host_check] is absent: its defaults are in force.
        let got = get(&text, &cfg, "host_check.min_available_mb").unwrap();
        assert_eq!(got.value, Some(serde_json::json!(2048)));
        assert_eq!(got.source, Source::Default);

        // device_sg, derived from device_tape.
        let tmp = tempfile::TempDir::new().unwrap();
        let tape = tmp.path().join("nst-fake-3");
        std::fs::File::create(&tape).unwrap();
        let sysfs = tmp.path().join("sys");
        std::fs::create_dir_all(sysfs.join("nst-fake-3/device/scsi_generic/sg7")).unwrap();
        let text = format!(
            "[[backends.lto]]\nname = \"d\"\ndevice_tape = \"{}\"\ngeneration = \"LTO-6\"\n",
            tape.display()
        );
        let mut cfg: crate::config::Config = toml::from_str(&text).unwrap();
        cfg.fill_derived_sg_nodes_under(&sysfs);
        let got = get(&text, &cfg, "device_sg").unwrap();
        assert_eq!(got.key, "backends.lto[d].device_sg");
        assert_eq!(got.value, Some(serde_json::json!("/dev/sg7")));
        assert!(matches!(got.source, Source::Derived(_)), "{:?}", got.source);
        assert_eq!(got.source.word(), "derived");
        // Written in the file, the same key is the file's.
        let written = format!("{text}device_sg = \"/dev/sg7\"\n");
        let got = get(&written, &cfg, "device_sg").unwrap();
        assert_eq!(got.source, Source::File);
    }

    #[test]
    fn get_refuses_a_table_and_a_typo_naming_the_keys() {
        let cfg = effective(ONE_DRIVE);
        let err = get(ONE_DRIVE, &cfg, "defaults").unwrap_err().to_string();
        assert!(err.contains("is a table"), "{err}");
        assert!(err.contains("defaults.slice_size"), "{err}");
        let err = get(ONE_DRIVE, &cfg, "backends.lto").unwrap_err().to_string();
        assert!(err.contains("list of tables"), "{err}");
        let err = get(ONE_DRIVE, &cfg, "defaults.slice_sizee")
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("defaults.slice_sizee is not a config key — did you mean defaults.slice_size?"),
            "{err}"
        );
        let err = get(ONE_DRIVE, &cfg, "backends.lto[lto6].post_warning_byte")
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("did you mean backends.lto[lto6].post_warning_bytes?"),
            "{err}"
        );
        let err = get(ONE_DRIVE, &cfg, "backends.lto[nope].name")
            .unwrap_err()
            .to_string();
        assert!(err.contains("no entry named \"nope\""), "{err}");
    }

    #[test]
    fn values_print_as_config_toml_writes_them() {
        for (v, want) in [
            (serde_json::json!("1G"), "\"1G\""),
            (serde_json::json!(0.97), "0.97"),
            (serde_json::json!(true), "true"),
            (serde_json::json!(["*.tmp", "*.bak"]), "[\"*.tmp\", \"*.bak\"]"),
            (serde_json::json!({"offsite": 2}), "{ offsite = 2 }"),
        ] {
            assert_eq!(toml_text_of(&v), want);
        }
    }

    #[test]
    fn the_combination_search_prefers_literals() {
        let slots = vec![
            vec![Value::from(1), Value::from("1")],
            vec![Value::from("x")],
            vec![Value::from(true), Value::from("true")],
        ];
        let c = combos(&slots);
        assert_eq!(c.len(), 4);
        assert_eq!(c[0], vec![0, 0, 0]);
        assert_eq!(c[3], vec![1, 0, 1]);
    }
}
