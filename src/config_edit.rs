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
    /// does not exist on this machine, as `backend add` warns).
    pub warnings: Vec<String>,
}

/// Plan `edit` against `original`, the text of the config file at `path`.
/// Pure but for the filesystem probes validation makes (a backend's device
/// canonicalization, as [`crate::config::Config::load`] makes them): nothing is written.
pub fn plan(original: &str, path: &Path, edit: &Edit) -> Result<Planned> {
    let doc: DocumentMut = original.parse().map_err(|e| {
        TapectlError::Config(format!(
            "{} is not valid TOML, so it cannot be edited in place — fix it by hand \
             (`tapectl config check` names the problem): {e}",
            path.display()
        ))
    })?;
    let segs = parse_key(edit.key())?;
    let before = check_text(original, path);
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
    let mut first: Option<LenientReport> = None;
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
        first.get_or_insert(report);
    }
    let report = first.expect("combos always yields at least one choice");
    let problems: Vec<String> = report
        .problems
        .iter()
        .filter(|p| {
            before.valid || p.as_str() == report.problems[0] || !before.problems.contains(p)
        })
        .map(|p| format!("  - {p}"))
        .collect();
    Err(TapectlError::Config(format!(
        "config {} {} refused; {} is unchanged:\n{}",
        edit.verb(),
        edit.key(),
        path.display(),
        problems.join("\n")
    )))
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

/// The checks `backend add` makes on a drive's fields that loading a config
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
    // taken could never be selected (`backend add` refuses it too).
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
    let mut d = doc.clone();
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
