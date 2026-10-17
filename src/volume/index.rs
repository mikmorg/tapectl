//! The cross-volume index (`docs/design/volume-format-v2.md` §1.5, ADR-0012
//! items 78-80, #418 R4): which volumes hold which version of each unit and,
//! for a tenant, every path of those versions. Written into the envelopes by
//! `volume::build`, read by `RESTORE.sh` and, as a to-do list, by `catalog
//! rebuild --from-volume`.
//!
//! Two files, both tab-separated text:
//!
//! - **`index/units.tsv`**, one row per (version, holding volume). A tenant
//!   envelope carries its own tenant's rows; the operator envelope carries
//!   every tenant's, with a leading `tenant` column. The rows and who holds
//!   them are [`coverage::index_versions`]'s.
//! - **`index/paths.tsv`**, tenant envelopes only: one row per path of each
//!   version listed in that tenant's `units.tsv`.
//!
//! **Built in the tapectl home's work dir, never under staging** (ADR-0012
//! 2026-10-06 item 4): it is every path of the archive in plaintext, until
//! `build()` has streamed it into the encrypted envelopes. `paths.tsv`
//! scales with the files of the listed versions, so it is streamed row by
//! row from the database to the file, and from the file into the envelope;
//! nothing here holds more than one version's rows at a time (and the
//! version's path query is read in index order, so it holds none).
//!
//! The encoding, the four escapes and the sort orders are the format's
//! (§1.5 "Encoding"); the same code reads them back ([`parse_units`],
//! [`parse_paths`]) by header name, ignoring a column it does not know.

use std::borrow::Cow;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fs::File;
use std::io::{self, BufWriter, Write};
use std::path::{Path, PathBuf};

use rusqlite::Connection;

use crate::error::{Result, TapectlError};
use crate::policy::coverage::{self, IndexThisTape, IndexVersionRow};

/// The tar member names, in the tenant and operator envelopes.
pub const UNITS_MEMBER: &str = "index/units.tsv";
pub const PATHS_MEMBER: &str = "index/paths.tsv";

/// `units.tsv`'s columns, tenant form (§1.5). The operator form is
/// `tenant` followed by these.
pub const UNITS_COLUMNS: [&str; 13] = [
    "unit",
    "unit_uuid",
    "version",
    "num_slices",
    "first_slice",
    "last_slice",
    "version_bytes",
    "volume",
    "volume_uuid",
    "cartridge_serial",
    "generation",
    "state",
    "sealed_at",
];

/// `paths.tsv`'s columns (§1.5).
pub const PATHS_COLUMNS: [&str; 5] = ["unit", "version", "kind", "size_bytes", "path"];

// ── escaping ──

/// Write `field` with the four escapes (§1.5): backslash as `\\`, TAB as
/// `\t`, LF as `\n`, CR as `\r`, and nothing else. All four are ASCII, so a
/// byte scan never splits a UTF-8 sequence.
fn write_escaped<W: Write + ?Sized>(w: &mut W, field: &str) -> io::Result<()> {
    let bytes = field.as_bytes();
    let mut start = 0;
    for (i, &b) in bytes.iter().enumerate() {
        let esc: &[u8] = match b {
            b'\\' => b"\\\\",
            b'\t' => b"\\t",
            b'\n' => b"\\n",
            b'\r' => b"\\r",
            _ => continue,
        };
        w.write_all(&bytes[start..i])?;
        w.write_all(esc)?;
        start = i + 1;
    }
    w.write_all(&bytes[start..])
}

/// `field` escaped, as a string: [`write_escaped`] for callers that hold it.
pub fn escape(field: &str) -> Cow<'_, str> {
    if !field.contains(['\\', '\t', '\n', '\r']) {
        return Cow::Borrowed(field);
    }
    let mut out = Vec::with_capacity(field.len() + 8);
    write_escaped(&mut out, field).expect("a Vec does not fail to write");
    Cow::Owned(String::from_utf8(out).expect("escaping ASCII bytes keeps UTF-8 whole"))
}

/// The inverse of [`escape`]. A backslash followed by anything but
/// `\`, `t`, `n` or `r`, or ending the field, is not something the writer
/// makes: refused rather than guessed at.
pub fn unescape(field: &str) -> std::result::Result<String, String> {
    if !field.contains('\\') {
        return Ok(field.to_string());
    }
    let mut out = String::with_capacity(field.len());
    let mut chars = field.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('\\') => out.push('\\'),
            Some('t') => out.push('\t'),
            Some('n') => out.push('\n'),
            Some('r') => out.push('\r'),
            Some(other) => return Err(format!("unknown escape `\\{other}`")),
            None => return Err("a backslash ends the field".to_string()),
        }
    }
    Ok(out)
}

/// A catalog timestamp (`YYYY-MM-DD HH:MM:SS`, UTC, what `datetime('now')`
/// writes) as RFC 3339 UTC (`YYYY-MM-DDTHH:MM:SSZ`), the seal marker's shape.
/// Text already in RFC 3339 is normalised to that shape. Anything else, and
/// the empty string, is "not known": empty.
pub fn rfc3339_from_catalog(text: &str) -> String {
    let text = text.trim();
    let utc = chrono::NaiveDateTime::parse_from_str(text, "%Y-%m-%d %H:%M:%S")
        .map(|t| t.and_utc())
        .or_else(|_| {
            chrono::DateTime::parse_from_rfc3339(text).map(|t| t.with_timezone(&chrono::Utc))
        });
    match utc {
        Ok(t) => t.to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
        Err(_) => String::new(),
    }
}

// ── the files' contents ──

/// `kind` as `paths.tsv` spells it (§1.5): `file_versions.kind` 0-3.
fn kind_letter(kind: i64) -> Result<char> {
    match kind {
        0 => Ok('d'),
        1 => Ok('f'),
        2 => Ok('l'),
        3 => Ok('s'),
        other => Err(TapectlError::Other(format!(
            "cross-volume index: a file_versions row has kind {other}, outside 0-3"
        ))),
    }
}

fn write_header<W: Write + ?Sized>(
    w: &mut W,
    columns: &[&str],
    leading: Option<&str>,
) -> Result<()> {
    let mut first = true;
    for name in leading.into_iter().chain(columns.iter().copied()) {
        if !first {
            w.write_all(b"\t")?;
        }
        first = false;
        w.write_all(name.as_bytes())?;
    }
    w.write_all(b"\n")?;
    Ok(())
}

fn write_units_row<W: Write + ?Sized>(
    w: &mut W,
    operator_form: bool,
    r: &IndexVersionRow,
) -> Result<()> {
    let field = |w: &mut W, s: &str| -> io::Result<()> {
        write_escaped(w, s)?;
        w.write_all(b"\t")
    };
    if operator_form {
        field(w, &r.tenant)?;
    }
    field(w, &r.unit)?;
    field(w, &r.unit_uuid)?;
    write!(w, "{}\t", r.version)?;
    // A whole version holds slices 1..=num_slices; a stage set with none
    // recorded has no range to state (an empty field is "not known").
    if r.num_slices > 0 {
        write!(w, "{}\t1\t{}\t", r.num_slices, r.num_slices)?;
    } else {
        w.write_all(b"\t\t\t")?;
    }
    write!(w, "{}\t", r.version_bytes)?;
    field(w, &r.volume)?;
    field(w, &r.volume_uuid)?;
    field(w, &r.cartridge_serial)?;
    field(w, &r.generation)?;
    field(w, r.state.as_str())?;
    write_escaped(w, &rfc3339_from_catalog(&r.sealed_at))?;
    w.write_all(b"\n")?;
    Ok(())
}

/// `units.tsv` row order (§1.5): `tenant` (operator form), `unit`,
/// `version`, then `volume`; text bytewise, version numeric.
fn units_order(a: &IndexVersionRow, b: &IndexVersionRow) -> std::cmp::Ordering {
    (&a.tenant, &a.unit, a.version, &a.volume).cmp(&(&b.tenant, &b.unit, b.version, &b.volume))
}

/// A writer that counts what passes through it.
struct Counting<'a> {
    inner: &'a mut dyn Write,
    bytes: u64,
}

impl Write for Counting<'_> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let n = self.inner.write(buf)?;
        self.bytes += n as u64;
        Ok(n)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

/// One listed version of one unit, in the order `paths.tsv` lists them.
struct ListedVersion<'a> {
    snapshot_id: i64,
    unit_id: i64,
    unit: &'a str,
    version: i64,
}

/// The statement behind `paths.tsv`, one version at a time: the unit's paths
/// IN INDEX ORDER (`UNIQUE(unit_id, path)`, BINARY collation: bytewise), each
/// looked up in the version by its primary key. `CROSS JOIN` pins `paths` as
/// the outer loop, so SQLite needs no sorter, and a sorter is what it would
/// spill to a temporary file outside the tapectl home, in plaintext, for a
/// version of any size. [`write_paths`] checks the order anyway.
const VERSION_PATHS_SQL: &str = "SELECT fv.kind, fv.size_bytes, p.path
     FROM paths p CROSS JOIN file_versions fv
          ON fv.snapshot_id = ?1 AND fv.path_id = p.id
     WHERE p.unit_id = ?2
     ORDER BY p.path";

/// `paths.tsv` for `versions` (already in `unit`, `version` order): the header,
/// then each version's paths, one row per path. One prepared statement,
/// run once per version; each path row is read once and written once.
fn write_paths<W: Write + ?Sized>(
    conn: &Connection,
    versions: &[ListedVersion<'_>],
    w: &mut W,
) -> Result<()> {
    write_header(w, &PATHS_COLUMNS, None)?;
    let mut stmt = conn.prepare(VERSION_PATHS_SQL)?;
    let mut previous = String::new();
    for v in versions {
        let unit = escape(v.unit);
        let mut rows = stmt.query(rusqlite::params![v.snapshot_id, v.unit_id])?;
        previous.clear();
        let mut first = true;
        while let Some(row) = rows.next()? {
            let kind = kind_letter(row.get(0)?)?;
            let size: i64 = row.get(1)?;
            let path = row.get_ref(2)?.as_str().map_err(|e| {
                TapectlError::Other(format!("cross-volume index: a path is not text: {e}"))
            })?;
            // The contract is a bytewise order (§1.5); a plan that stopped
            // delivering it must fail here, not write an unsorted file.
            if !first && previous.as_str() >= path {
                return Err(TapectlError::Other(format!(
                    "cross-volume index: the catalog returned unit {:?} version {}'s paths \
                     out of order or twice ({previous:?} then {path:?})",
                    v.unit, v.version
                )));
            }
            first = false;
            previous.clear();
            previous.push_str(path);
            w.write_all(unit.as_bytes())?;
            write!(w, "\t{}\t{kind}\t{size}\t", v.version)?;
            write_escaped(w, path)?;
            w.write_all(b"\n")?;
        }
    }
    Ok(())
}

// ── generation ──

/// A file of the index, by what it is for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Member {
    /// The operator envelope's `index/units.tsv`.
    OperatorUnits,
    /// A tenant envelope's `index/units.tsv`.
    TenantUnits(i64),
    /// A tenant envelope's `index/paths.tsv`.
    TenantPaths(i64),
}

/// What [`generate`] wrote: each member's size in bytes.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Generated {
    pub sizes: BTreeMap<Member, u64>,
}

impl Generated {
    /// The plaintext size of every file: what ONE copy of the index adds to
    /// the envelopes before tar and age overhead. Each envelope is on the
    /// tape twice (§1.3), so the tape carries about twice this.
    pub fn total_bytes(&self) -> u64 {
        self.sizes.values().sum()
    }
}

/// The index's files on disk, for `build()`. Built by [`generate_files`] in
/// the tapectl home's work dir.
#[derive(Debug, Clone)]
pub struct IndexFiles {
    /// The operator envelope's `index/units.tsv` (every tenant, no paths).
    pub operator_units: PathBuf,
    /// Each tenant with an envelope on this tape: its `units.tsv` and `paths.tsv`.
    pub tenants: BTreeMap<i64, TenantIndexFiles>,
    pub generated: Generated,
}

#[derive(Debug, Clone)]
pub struct TenantIndexFiles {
    pub units: PathBuf,
    pub paths: PathBuf,
}

/// Generate the index for the write that carries `write_stage_sets`, handing
/// each file to the writer `open` returns for it. Facts: [`coverage::index_versions`];
/// per tenant, the paths of the versions it lists. A tenant gets files only
/// when this write carries one of its units, since only such a tenant has an
/// envelope on this tape; the operator's `units.tsv` lists every tenant.
pub fn generate(
    conn: &Connection,
    this: &IndexThisTape<'_>,
    write_stage_sets: &[i64],
    open: &mut dyn FnMut(Member) -> io::Result<Box<dyn Write>>,
) -> Result<Generated> {
    let mut rows = coverage::index_versions(conn, this, write_stage_sets)?;
    rows.sort_by(units_order);

    let mut generated = Generated::default();
    let mut emit =
        |member: Member, body: &mut dyn FnMut(&mut dyn Write) -> Result<()>| -> Result<()> {
            let mut sink = open(member)?;
            let mut counted = Counting {
                inner: &mut *sink,
                bytes: 0,
            };
            body(&mut counted)?;
            counted.flush()?;
            let bytes = counted.bytes;
            generated.sizes.insert(member, bytes);
            Ok(())
        };

    emit(Member::OperatorUnits, &mut |w| {
        write_header(w, &UNITS_COLUMNS, Some("tenant"))?;
        for r in &rows {
            write_units_row(w, true, r)?;
        }
        Ok(())
    })?;

    // The tenants with an envelope here: those this write carries a unit of.
    let enveloped: BTreeSet<i64> = rows
        .iter()
        .filter(|r| r.state == coverage::IndexHolderState::ThisTape)
        .map(|r| r.tenant_id)
        .collect();
    for tenant_id in enveloped {
        // Isolation by construction: everything below reads this tenant's
        // rows and nothing else's.
        let mine: Vec<&IndexVersionRow> =
            rows.iter().filter(|r| r.tenant_id == tenant_id).collect();
        emit(Member::TenantUnits(tenant_id), &mut |w| {
            write_header(w, &UNITS_COLUMNS, None)?;
            for r in &mine {
                write_units_row(w, false, r)?;
            }
            Ok(())
        })?;

        // The listed versions, once each, in (unit, version) order: `mine`
        // is sorted by (unit, version, volume), so a repeat is adjacent.
        let mut versions: Vec<ListedVersion<'_>> = Vec::new();
        for r in &mine {
            if versions
                .last()
                .is_none_or(|v| v.snapshot_id != r.snapshot_id)
            {
                versions.push(ListedVersion {
                    snapshot_id: r.snapshot_id,
                    unit_id: r.unit_id,
                    unit: &r.unit,
                    version: r.version,
                });
            }
        }
        emit(Member::TenantPaths(tenant_id), &mut |w| {
            write_paths(conn, &versions, w)
        })?;
    }
    Ok(generated)
}

/// [`generate`] into files under `dir` (created if absent): what `volume
/// write` hands `build()`. `dir` belongs in the tapectl home's work dir.
pub fn generate_files(
    conn: &Connection,
    this: &IndexThisTape<'_>,
    write_stage_sets: &[i64],
    dir: &Path,
) -> Result<IndexFiles> {
    std::fs::create_dir_all(dir)?;
    let mut paths: BTreeMap<Member, PathBuf> = BTreeMap::new();
    let generated = generate(conn, this, write_stage_sets, &mut |member| {
        let name = match member {
            Member::OperatorUnits => "operator_units.tsv".to_string(),
            Member::TenantUnits(id) => format!("tenant_{id}_units.tsv"),
            Member::TenantPaths(id) => format!("tenant_{id}_paths.tsv"),
        };
        let path = dir.join(name);
        let file = BufWriter::new(File::create(&path)?);
        paths.insert(member, path);
        Ok(Box::new(file))
    })?;
    let operator_units = paths
        .remove(&Member::OperatorUnits)
        .expect("generate always writes the operator's units.tsv");
    let mut tenants = BTreeMap::new();
    for (member, units) in &paths {
        if let Member::TenantUnits(id) = member {
            let paths_file = paths
                .get(&Member::TenantPaths(*id))
                .expect("a tenant's paths.tsv is written with its units.tsv")
                .clone();
            tenants.insert(
                *id,
                TenantIndexFiles {
                    units: units.clone(),
                    paths: paths_file,
                },
            );
        }
    }
    Ok(IndexFiles {
        operator_units,
        tenants,
        generated,
    })
}

/// The size of the index [`generate`] would write for `write_stage_sets`,
/// by running the real generator into a sink: nothing is written to disk, but
/// every row is read (one pass over the listed versions' paths).
pub fn measure(
    conn: &Connection,
    this: &IndexThisTape<'_>,
    write_stage_sets: &[i64],
) -> Result<Generated> {
    generate(conn, this, write_stage_sets, &mut |_| {
        Ok(Box::new(io::sink()))
    })
}

// ── reading ──

/// A parsed `units.tsv` row. A column the file does not carry reads as empty
/// ("not known"), an unknown column is ignored.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct UnitsRow {
    /// Empty in a tenant form.
    pub tenant: String,
    pub unit: String,
    pub unit_uuid: String,
    pub version: i64,
    pub num_slices: Option<i64>,
    pub first_slice: Option<i64>,
    pub last_slice: Option<i64>,
    pub version_bytes: Option<i64>,
    pub volume: String,
    pub volume_uuid: String,
    pub cartridge_serial: String,
    pub generation: String,
    /// `sealed`, `quarantined` or `this-tape`; kept as written, so a later
    /// writer's new state reads through.
    pub state: String,
    pub sealed_at: String,
}

/// A parsed `paths.tsv` row.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PathsRow {
    pub unit: String,
    pub version: i64,
    /// `d`, `f`, `l` or `s`, as written.
    pub kind: String,
    pub size_bytes: Option<i64>,
    pub path: String,
}

/// The header's column names mapped to their positions, refusing a file that
/// lacks one of `required`.
fn header_columns<'a>(
    file: &str,
    header: &'a str,
    required: &[&str],
) -> Result<HashMap<&'a str, usize>> {
    let map: HashMap<&str, usize> = header
        .split('\t')
        .enumerate()
        .map(|(i, c)| (c, i))
        .collect();
    for name in required {
        if !map.contains_key(name) {
            return Err(TapectlError::Other(format!(
                "{file}: the header has no `{name}` column (it names: {})",
                header.replace('\t', ", ")
            )));
        }
    }
    Ok(map)
}

/// Each data row of `text` as its unescaped fields, with the line number for
/// errors. Every row must have as many fields as the header: an escaped TAB
/// never splits one.
fn rows_of<'a>(
    file: &'a str,
    text: &'a str,
    columns: usize,
) -> impl Iterator<Item = Result<(usize, Vec<String>)>> + 'a {
    text.split('\n')
        .enumerate()
        .skip(1)
        .filter(|(_, line)| !line.is_empty())
        .map(move |(i, line)| {
            let n = i + 1;
            let fields: Vec<&str> = line.split('\t').collect();
            if fields.len() != columns {
                return Err(TapectlError::Other(format!(
                    "{file}: line {n} has {} field(s), the header names {columns}",
                    fields.len()
                )));
            }
            fields
                .into_iter()
                .map(|f| {
                    unescape(f).map_err(|e| TapectlError::Other(format!("{file}: line {n}: {e}")))
                })
                .collect::<Result<Vec<String>>>()
                .map(|f| (n, f))
        })
}

fn int_field(file: &str, line: usize, column: &str, value: &str) -> Result<Option<i64>> {
    if value.is_empty() {
        return Ok(None);
    }
    value.parse().map(Some).map_err(|_| {
        TapectlError::Other(format!(
            "{file}: line {line}: `{column}` is not a number: {value:?}"
        ))
    })
}

/// Parse a `units.tsv` (either form). Columns are found by header name;
/// `unit`, `version` and `volume` must be there.
pub fn parse_units(text: &str) -> Result<Vec<UnitsRow>> {
    const FILE: &str = "index/units.tsv";
    let header = text.split('\n').next().unwrap_or("");
    let cols = header_columns(FILE, header, &["unit", "version", "volume"])?;
    let width = header.split('\t').count();
    let mut out = Vec::new();
    for row in rows_of(FILE, text, width) {
        let (n, f) = row?;
        let text_of = |name: &str| cols.get(name).map(|&i| f[i].clone()).unwrap_or_default();
        let int_of = |name: &str| -> Result<Option<i64>> {
            match cols.get(name) {
                Some(&i) => int_field(FILE, n, name, &f[i]),
                None => Ok(None),
            }
        };
        out.push(UnitsRow {
            tenant: text_of("tenant"),
            unit: text_of("unit"),
            unit_uuid: text_of("unit_uuid"),
            version: int_of("version")?.ok_or_else(|| {
                TapectlError::Other(format!("{FILE}: line {n}: `version` is empty"))
            })?,
            num_slices: int_of("num_slices")?,
            first_slice: int_of("first_slice")?,
            last_slice: int_of("last_slice")?,
            version_bytes: int_of("version_bytes")?,
            volume: text_of("volume"),
            volume_uuid: text_of("volume_uuid"),
            cartridge_serial: text_of("cartridge_serial"),
            generation: text_of("generation"),
            state: text_of("state"),
            sealed_at: text_of("sealed_at"),
        });
    }
    Ok(out)
}

/// Parse a `paths.tsv`. `unit`, `version` and `path` must be there.
pub fn parse_paths(text: &str) -> Result<Vec<PathsRow>> {
    const FILE: &str = "index/paths.tsv";
    let header = text.split('\n').next().unwrap_or("");
    let cols = header_columns(FILE, header, &["unit", "version", "path"])?;
    let width = header.split('\t').count();
    let mut out = Vec::new();
    for row in rows_of(FILE, text, width) {
        let (n, f) = row?;
        let text_of = |name: &str| cols.get(name).map(|&i| f[i].clone()).unwrap_or_default();
        out.push(PathsRow {
            unit: text_of("unit"),
            version: int_field(FILE, n, "version", &text_of("version"))?.ok_or_else(|| {
                TapectlError::Other(format!("{FILE}: line {n}: `version` is empty"))
            })?,
            kind: text_of("kind"),
            size_bytes: int_field(FILE, n, "size_bytes", &text_of("size_bytes"))?,
            path: text_of("path"),
        });
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_four_escapes_round_trip_and_nothing_else_is_escaped() {
        for s in [
            "plain",
            "",
            "a\tb",
            "a\nb",
            "a\rb",
            "a\\b",
            "\\t",
            "\\\\",
            "tab\there\nline\rcr\\slash",
            "trailing\\",
            "uni\u{e9}code \u{1F4FC} stays",
            "\t\n\r\\",
        ] {
            let escaped = escape(s);
            assert!(
                !escaped.contains(['\t', '\n', '\r']),
                "{s:?} escaped still has a separator: {escaped:?}"
            );
            assert_eq!(
                unescape(&escaped).as_deref(),
                Ok(s),
                "{s:?} via {escaped:?}"
            );
        }
        // Exactly these four, spelled as the format spells them.
        assert_eq!(escape("\\"), "\\\\");
        assert_eq!(escape("\t"), "\\t");
        assert_eq!(escape("\n"), "\\n");
        assert_eq!(escape("\r"), "\\r");
        // Nothing else: a quote, a slash, NUL and a non-ASCII character pass through.
        assert!(matches!(escape("a/b \"c\" \u{0}\u{e9}"), Cow::Borrowed(_)));
    }

    #[test]
    fn the_unescaper_refuses_what_the_writer_never_makes() {
        assert!(unescape("a\\xb").unwrap_err().contains("\\x"));
        assert!(unescape("ends\\").is_err());
    }

    #[test]
    fn a_catalog_time_becomes_rfc_3339_utc_and_nonsense_becomes_unknown() {
        assert_eq!(
            rfc3339_from_catalog("2026-10-14 09:08:07"),
            "2026-10-14T09:08:07Z"
        );
        assert_eq!(
            rfc3339_from_catalog("2026-10-14T11:08:07+02:00"),
            "2026-10-14T09:08:07Z"
        );
        assert_eq!(rfc3339_from_catalog(""), "");
        assert_eq!(rfc3339_from_catalog("last tuesday"), "");
    }

    #[test]
    fn the_headers_are_the_formats() {
        assert_eq!(
            UNITS_COLUMNS.join("\t"),
            "unit\tunit_uuid\tversion\tnum_slices\tfirst_slice\tlast_slice\tversion_bytes\t\
             volume\tvolume_uuid\tcartridge_serial\tgeneration\tstate\tsealed_at"
        );
        assert_eq!(
            PATHS_COLUMNS.join("\t"),
            "unit\tversion\tkind\tsize_bytes\tpath"
        );
        let mut operator = Vec::new();
        write_header(&mut operator, &UNITS_COLUMNS, Some("tenant")).unwrap();
        assert_eq!(
            String::from_utf8(operator).unwrap(),
            "tenant\tunit\tunit_uuid\tversion\tnum_slices\tfirst_slice\tlast_slice\t\
             version_bytes\tvolume\tvolume_uuid\tcartridge_serial\tgeneration\tstate\t\
             sealed_at\n"
        );
    }

    fn row(
        unit: &str,
        version: i64,
        volume: &str,
        state: coverage::IndexHolderState,
    ) -> IndexVersionRow {
        IndexVersionRow {
            tenant_id: 2,
            tenant: "alpha".to_string(),
            unit_id: 7,
            unit: unit.to_string(),
            unit_uuid: "0a1b2c3d-0000-4000-8000-000000000007".to_string(),
            snapshot_id: 70 + version,
            version,
            num_slices: 3,
            version_bytes: 1_572_864,
            volume: volume.to_string(),
            volume_uuid: "5e6f7a8b-0000-4000-8000-000000000001".to_string(),
            cartridge_serial: "HUM12345".to_string(),
            generation: "LTO-6".to_string(),
            state,
            sealed_at: if state == coverage::IndexHolderState::ThisTape {
                String::new()
            } else {
                "2026-10-14 09:08:07".to_string()
            },
        }
    }

    #[test]
    fn a_units_row_is_the_formats_row_in_both_forms() {
        let r = row(
            "photos/2019",
            2,
            "L6-0002",
            coverage::IndexHolderState::Sealed,
        );
        let mut tenant = Vec::new();
        write_units_row(&mut tenant, false, &r).unwrap();
        assert_eq!(
            String::from_utf8(tenant).unwrap(),
            "photos/2019\t0a1b2c3d-0000-4000-8000-000000000007\t2\t3\t1\t3\t1572864\t\
             L6-0002\t5e6f7a8b-0000-4000-8000-000000000001\tHUM12345\tLTO-6\tsealed\t\
             2026-10-14T09:08:07Z\n"
        );
        let mut operator = Vec::new();
        write_units_row(&mut operator, true, &r).unwrap();
        assert_eq!(
            String::from_utf8(operator).unwrap(),
            "alpha\tphotos/2019\t0a1b2c3d-0000-4000-8000-000000000007\t2\t3\t1\t3\t1572864\t\
             L6-0002\t5e6f7a8b-0000-4000-8000-000000000001\tHUM12345\tLTO-6\tsealed\t\
             2026-10-14T09:08:07Z\n"
        );
        // This tape: no sealed_at; the field is there and empty.
        let this = row(
            "photos/2019",
            2,
            "L6-0003",
            coverage::IndexHolderState::ThisTape,
        );
        let mut line = Vec::new();
        write_units_row(&mut line, false, &this).unwrap();
        let line = String::from_utf8(line).unwrap();
        assert!(line.ends_with("\tthis-tape\t\n"), "{line:?}");
        assert_eq!(line.matches('\t').count(), 12);
    }

    #[test]
    fn units_round_trip_through_the_parser_with_hostile_names() {
        let mut r = row(
            "odd\tname\nwith\\stuff\r",
            1,
            "L6-0001",
            coverage::IndexHolderState::Quarantined,
        );
        r.tenant = "ten\tant".to_string();
        r.cartridge_serial = String::new();
        let mut bytes = Vec::new();
        write_header(&mut bytes, &UNITS_COLUMNS, Some("tenant")).unwrap();
        write_units_row(&mut bytes, true, &r).unwrap();
        let text = String::from_utf8(bytes).unwrap();
        // Every record is one line, whatever the names hold.
        assert_eq!(text.matches('\n').count(), 2, "{text:?}");
        let parsed = parse_units(&text).unwrap();
        assert_eq!(parsed.len(), 1);
        let p = &parsed[0];
        assert_eq!(p.tenant, "ten\tant");
        assert_eq!(p.unit, "odd\tname\nwith\\stuff\r");
        assert_eq!(p.version, 1);
        assert_eq!(p.num_slices, Some(3));
        assert_eq!((p.first_slice, p.last_slice), (Some(1), Some(3)));
        assert_eq!(p.version_bytes, Some(1_572_864));
        assert_eq!(p.volume, "L6-0001");
        assert_eq!(p.cartridge_serial, "");
        assert_eq!(p.state, "quarantined");
        assert_eq!(p.sealed_at, "2026-10-14T09:08:07Z");
    }

    #[test]
    fn the_parser_finds_columns_by_name_and_ignores_the_ones_it_does_not_know() {
        // A later writer: columns reordered, one added, one the reader never heard of.
        let text = "future\tvolume\tstate\tversion\tunit\n\
                    x\tL6-0009\tsealed\t4\tdocs\n";
        let rows = parse_units(text).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].volume, "L6-0009");
        assert_eq!(rows[0].version, 4);
        assert_eq!(rows[0].unit, "docs");
        assert_eq!(rows[0].state, "sealed");
        assert_eq!(rows[0].num_slices, None, "an absent column is not known");
    }

    #[test]
    fn the_parser_names_what_is_wrong() {
        let err = parse_units("unit\tversion\nu\t1\n")
            .unwrap_err()
            .to_string();
        assert!(err.contains("no `volume` column"), "{err}");
        let err = parse_units("unit\tversion\tvolume\nu\t1\n")
            .unwrap_err()
            .to_string();
        assert!(err.contains("line 2") && err.contains("2 field"), "{err}");
        let err = parse_units("unit\tversion\tvolume\nu\tone\tv\n")
            .unwrap_err()
            .to_string();
        assert!(err.contains("`version` is not a number"), "{err}");
        let err = parse_units("unit\tversion\tvolume\nu\\q\t1\tv\n")
            .unwrap_err()
            .to_string();
        assert!(err.contains("unknown escape"), "{err}");
        assert!(
            parse_units("").is_err(),
            "a file with no header is not an index"
        );
        // A header and no rows is an empty index, not an error.
        assert!(parse_units("unit\tversion\tvolume\n").unwrap().is_empty());
    }

    #[test]
    fn paths_round_trip_through_the_parser() {
        let text = "unit\tversion\tkind\tsize_bytes\tpath\n\
                    docs\t1\td\t0\tsub\n\
                    docs\t1\tf\t12\tsub/a\\tb\\nc\\\\d\n\
                    docs\t1\tl\t9\tsub/link\n";
        let rows = parse_paths(text).unwrap();
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[1].path, "sub/a\tb\nc\\d");
        assert_eq!(rows[1].kind, "f");
        assert_eq!(rows[1].size_bytes, Some(12));
        assert_eq!(rows[2].kind, "l");
    }
    // ── over a catalog ──

    use crate::db::files::FileKind::{Dir, Regular, Special, Symlink};
    use crate::policy::coverage::tests::{IndexCatalog, THIS};

    /// alpha's `photos` (v1 on L6-0001, v2 carried by this write) and a
    /// second alpha unit `notes` carried by this write (its stage sets are the
    /// returned list); bravo's `docs`
    /// (v1 only on L6-0002, a volume nothing of alpha's is on).
    fn two_tenants() -> (IndexCatalog, Vec<i64>) {
        let c = IndexCatalog::new();
        let photos = c.unit("alpha", "photos");
        let notes = c.unit("alpha", "notes");
        let docs = c.unit("bravo", "docs");
        let l1 = c.volume("L6-0001", "sealed", "ok", Some("2026-10-01 10:00:00"));
        let l2 = c.volume(
            "L6-0002",
            "sealed",
            "quarantined",
            Some("2026-10-02 10:00:00"),
        );
        c.bind_serial(l1, "HUM00001");

        let (s1, p1) = c.version(photos, 1, "current", &[1000]);
        c.write(p1, l1, "completed");
        c.paths(s1, &[("a.jpg", Regular, 10), ("album", Dir, 0)]);
        let (s2, p2) = c.version(photos, 2, "staged", &[2000, 500]);
        c.paths(
            s2,
            &[
                ("a.jpg", Regular, 11),
                ("album", Dir, 0),
                ("album/b.jpg", Regular, 12),
                ("link", Symlink, 5),
            ],
        );
        let (sn, pn) = c.version(notes, 1, "staged", &[7]);
        c.paths(sn, &[("todo.txt", Regular, 7), ("pipe", Special, 0)]);
        let (sd, pd) = c.version(docs, 1, "current", &[300]);
        c.write(pd, l2, "completed");
        c.paths(sd, &[("secret-bravo-report.pdf", Regular, 300)]);
        (c, vec![p2, pn])
    }

    fn generate_to(dir: &Path, c: &IndexCatalog, stage_sets: &[i64]) -> IndexFiles {
        generate_files(&c.conn, &THIS, stage_sets, dir).unwrap()
    }

    fn read(path: &Path) -> String {
        std::fs::read_to_string(path).unwrap()
    }

    #[test]
    fn the_files_hold_the_formats_rows_in_the_formats_order() {
        let (c, write) = two_tenants();
        let dir = tempfile::tempdir().unwrap();
        let files = generate_to(dir.path(), &c, &write);
        let alpha = c.tenant("alpha");

        let units = read(&files.tenants[&alpha].units);
        let lines: Vec<&str> = units.lines().collect();
        assert_eq!(lines[0], UNITS_COLUMNS.join("\t"));
        let keys: Vec<(String, i64, String, String)> = parse_units(&units)
            .unwrap()
            .into_iter()
            .map(|r| (r.unit, r.version, r.volume, r.state))
            .collect();
        let want = |u: &str, v: i64, vol: &str, st: &str| {
            (u.to_string(), v, vol.to_string(), st.to_string())
        };
        // Sorted by unit, then version, then volume; notes before photos.
        assert_eq!(
            keys,
            vec![
                want("notes", 1, "L6-0100", "this-tape"),
                want("photos", 1, "L6-0001", "sealed"),
                want("photos", 2, "L6-0100", "this-tape"),
            ]
        );
        // The row for a tape another volume holds: serial and time as written.
        let held = parse_units(&units).unwrap();
        let on_l1 = held.iter().find(|r| r.volume == "L6-0001").unwrap();
        assert_eq!(on_l1.cartridge_serial, "HUM00001");
        assert_eq!(on_l1.sealed_at, "2026-10-01T10:00:00Z");
        assert_eq!(
            (on_l1.num_slices, on_l1.version_bytes),
            (Some(1), Some(1000))
        );
        let v2 = held.iter().find(|r| r.version == 2).unwrap();
        assert_eq!(
            (
                v2.num_slices,
                v2.first_slice,
                v2.last_slice,
                v2.version_bytes
            ),
            (Some(2), Some(1), Some(2), Some(2500))
        );
        assert_eq!(v2.tenant, "", "the tenant form has no tenant column");

        // The operator form: every tenant, tenant column first, sorted by it.
        let op = read(&files.operator_units);
        assert!(op.starts_with("tenant\tunit\t"), "{op}");
        let op_rows = parse_units(&op).unwrap();
        let op_keys: Vec<(&str, &str, i64, &str)> = op_rows
            .iter()
            .map(|r| {
                (
                    r.tenant.as_str(),
                    r.unit.as_str(),
                    r.version,
                    r.volume.as_str(),
                )
            })
            .collect();
        assert_eq!(
            op_keys,
            vec![
                ("alpha", "notes", 1, "L6-0100"),
                ("alpha", "photos", 1, "L6-0001"),
                ("alpha", "photos", 2, "L6-0100"),
                ("bravo", "docs", 1, "L6-0002"),
            ]
        );
        assert_eq!(op_rows[3].state, "quarantined");

        // paths.tsv: every path of each version listed in alpha's units.tsv,
        // sorted by unit, version, then path.
        let paths = read(&files.tenants[&alpha].paths);
        assert_eq!(paths.lines().next().unwrap(), PATHS_COLUMNS.join("\t"));
        let rows: Vec<(String, i64, String, i64, String)> = parse_paths(&paths)
            .unwrap()
            .into_iter()
            .map(|r| (r.unit, r.version, r.kind, r.size_bytes.unwrap(), r.path))
            .collect();
        let p = |u: &str, v: i64, k: &str, n: i64, path: &str| {
            (u.to_string(), v, k.to_string(), n, path.to_string())
        };
        assert_eq!(
            rows,
            vec![
                p("notes", 1, "s", 0, "pipe"),
                p("notes", 1, "f", 7, "todo.txt"),
                p("photos", 1, "f", 10, "a.jpg"),
                p("photos", 1, "d", 0, "album"),
                p("photos", 2, "f", 11, "a.jpg"),
                p("photos", 2, "d", 0, "album"),
                p("photos", 2, "f", 12, "album/b.jpg"),
                p("photos", 2, "l", 5, "link"),
            ]
        );
    }

    #[test]
    fn a_tenants_files_carry_nothing_of_another_tenants_and_the_operators_carry_both() {
        let (c, write) = two_tenants();
        let dir = tempfile::tempdir().unwrap();
        let files = generate_to(dir.path(), &c, &write);
        let alpha = c.tenant("alpha");
        let bravo = c.tenant("bravo");

        // bravo has no unit in this write: no envelope on this tape, no files.
        assert_eq!(
            files.tenants.keys().copied().collect::<Vec<_>>(),
            vec![alpha]
        );
        assert!(files
            .generated
            .sizes
            .contains_key(&Member::TenantUnits(alpha)));
        assert!(!files
            .generated
            .sizes
            .contains_key(&Member::TenantUnits(bravo)));
        assert!(!files
            .generated
            .sizes
            .contains_key(&Member::TenantPaths(bravo)));

        // alpha's two files name none of bravo's: its unit, its file, the
        // volume only bravo's data is on.
        for text in [
            read(&files.tenants[&alpha].units),
            read(&files.tenants[&alpha].paths),
        ] {
            for forbidden in [
                "docs",
                "secret-bravo-report.pdf",
                "L6-0002",
                "uuid-L6-0002",
                "bravo",
            ] {
                assert!(
                    !text.contains(forbidden),
                    "{forbidden} leaked into alpha's index:\n{text}"
                );
            }
        }
        // The operator map has both tenants' units and volumes, and no paths.
        let op = read(&files.operator_units);
        for want in ["alpha", "bravo", "docs", "photos", "L6-0001", "L6-0002"] {
            assert!(
                op.contains(want),
                "{want} missing from the operator map:\n{op}"
            );
        }
        assert!(!op.contains("secret-bravo-report.pdf") && !op.contains("a.jpg"));
    }

    #[test]
    fn two_generations_from_one_catalog_are_byte_identical() {
        let (c, write) = two_tenants();
        let (a, b) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        let (fa, fb) = (
            generate_to(a.path(), &c, &write),
            generate_to(b.path(), &c, &write),
        );
        assert_eq!(read(&fa.operator_units), read(&fb.operator_units));
        for (id, ta) in &fa.tenants {
            let tb = &fb.tenants[id];
            assert_eq!(read(&ta.units), read(&tb.units));
            assert_eq!(read(&ta.paths), read(&tb.paths));
        }
        assert_eq!(fa.generated, fb.generated);
    }

    #[test]
    fn the_measured_size_is_the_written_size() {
        let (c, write) = two_tenants();
        let dir = tempfile::tempdir().unwrap();
        let files = generate_to(dir.path(), &c, &write);
        let measured = measure(&c.conn, &THIS, &write).unwrap();
        assert_eq!(measured, files.generated);
        let on_disk: u64 = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().metadata().unwrap().len())
            .sum();
        assert_eq!(measured.total_bytes(), on_disk);
        assert!(on_disk > 0);
    }

    #[test]
    fn a_version_listed_for_a_live_copy_alone_still_has_its_paths() {
        // photos v1 is on L6-0001 and this write carries only `notes`: the
        // tenant has an envelope here (notes), and photos v1 is listed.
        let (c, _) = two_tenants();
        let notes_set: i64 = c
            .conn
            .query_row(
                "SELECT ss.id FROM stage_sets ss JOIN snapshots s ON s.id = ss.snapshot_id
                 JOIN units u ON u.id = s.unit_id WHERE u.name = 'notes'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        let dir = tempfile::tempdir().unwrap();
        let files = generate_to(dir.path(), &c, &[notes_set]);
        let alpha = c.tenant("alpha");
        let paths = parse_paths(&read(&files.tenants[&alpha].paths)).unwrap();
        let photos_v1: Vec<&str> = paths
            .iter()
            .filter(|r| r.unit == "photos" && r.version == 1)
            .map(|r| r.path.as_str())
            .collect();
        assert_eq!(photos_v1, vec!["a.jpg", "album"]);
        assert!(
            paths
                .iter()
                .all(|r| !(r.unit == "photos" && r.version == 2)),
            "photos v2 is staged and not carried by this write: not listed"
        );
    }

    #[test]
    fn paths_are_sorted_bytewise_and_hostile_names_stay_one_row_each() {
        let c = IndexCatalog::new();
        let u = c.unit("alpha", "odd/unit\tname");
        let (snap, ss) = c.version(u, 1, "staged", &[1]);
        let names = [
            "b", "B", "a b", "a\tb", "a\nb", "a\rb", "a\\b", "a/b", "\u{e9}", "z", "ab",
        ];
        let entries: Vec<(&str, crate::db::files::FileKind, i64)> =
            names.iter().map(|n| (*n, Regular, 1)).collect();
        c.paths(snap, &entries);
        let dir = tempfile::tempdir().unwrap();
        let files = generate_to(dir.path(), &c, &[ss]);
        let text = read(&files.tenants[&c.tenant("alpha")].paths);
        // One line per path plus the header, whatever the names hold.
        assert_eq!(text.matches('\n').count(), names.len() + 1);
        let got: Vec<String> = parse_paths(&text)
            .unwrap()
            .into_iter()
            .map(|r| r.path)
            .collect();
        let mut want: Vec<String> = names.iter().map(|n| n.to_string()).collect();
        want.sort_by(|a, b| a.as_bytes().cmp(b.as_bytes()));
        assert_eq!(got, want);
        assert_eq!(parse_paths(&text).unwrap()[0].unit, "odd/unit\tname");
    }

    #[test]
    fn a_versions_paths_are_read_in_index_order_with_no_sorter() {
        // The plan, not just the output: a sorter would spill plaintext to a
        // temporary file outside the tapectl home. If SQLite stops using the
        // `paths` index for the order, `write_paths` fails loudly; this
        // names the plan.
        let c = IndexCatalog::new();
        let mut stmt = c
            .conn
            .prepare(&format!("EXPLAIN QUERY PLAN {VERSION_PATHS_SQL}"))
            .unwrap();
        let plan: Vec<String> = stmt
            .query_map(rusqlite::params![1, 1], |r| r.get::<_, String>(3))
            .unwrap()
            .map(|r| r.unwrap())
            .collect();
        let plan = plan.join("\n");
        assert!(
            !plan.contains("TEMP B-TREE"),
            "a sorter in the plan:\n{plan}"
        );
        assert!(
            plan.contains("SEARCH p USING"),
            "paths is not read by index:\n{plan}"
        );
        assert!(plan.contains("SEARCH fv USING PRIMARY KEY"), "{plan}");
    }
}
