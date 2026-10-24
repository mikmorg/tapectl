//! Whether a unit's tenancy agrees in the three places it is recorded
//! (#383): `units.tenant_id` (the catalog, the authority — ADR-0012,
//! 2026-10-07 item 24), the `tenant` in the unit's `.tapectl-unit.toml`, and
//! each written stage set's recipient list (`stage_sets.key_fingerprints`).
//!
//! Nothing reconciled them. `tenant reassign` moves only the catalog row; the
//! tapes already written stay encrypted to the old tenant's keys (plus the
//! operator's and the escrow's, which is why tapectl's own restore and a DR
//! rebuild still work), and the dotfile kept the old name until the reassign
//! rewrote it. This module answers, advisorily, where the three disagree, and
//! names each disagreement; `audit`'s `tenancy` check is its reader. It
//! reads the recorded recipient list and the dotfile only: no tape, no key
//! material, no format change.
//!
//! **Readable by the owning tenant** means the recorded list names any key
//! the unit's current tenant holds, active or retired: a retired key still
//! opens what it was a recipient of. A list that is absent (a rebuilt row
//! whose tape carried none, or a pre-escrow row) or unreadable is not a
//! tenancy disagreement: `escrow_coverage` already names it, in words that
//! say what to do about it, and this check does not report it twice.
//!
//! **A tenant the catalog holds no key for is not judged either**, as
//! `policy::escrow` answers "unknown" rather than "gap" when it could not
//! have known. `catalog rebuild` files units under tenants it creates with
//! no `encryption_keys` rows (the tape carries recipients, not whose they
//! are), while each stage set comes back with its recipient list: "the
//! catalog lists none of this tenant's keys" is then not "this tenant cannot
//! open the copy", and naming every copy in the archive as one to re-stage
//! would be the wrong action, and an expensive one. `key import` of the
//! tenant's public key is what makes its copies judgeable. A tenant holding
//! only some of its keys (a partial import) is still judged, and `audit`'s
//! action names `key import` first.
//!
//! The volume filter is [`crate::policy::coverage::in_service`] and
//! `encrypted = 1`, as for escrow coverage's reporting scopes
//! (`policy::escrow`): the question is about bytes tapectl still accounts
//! for, and a stage set written in the clear is `encryption`'s finding.

use std::collections::BTreeMap;
use std::path::Path;

use rusqlite::{params, Connection};

use crate::db::models::Unit;
use crate::error::Result;

/// One place a unit's tenancy disagrees with the catalog's.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Disagreement {
    /// The unit's dotfile names another tenant than the catalog does.
    Dotfile {
        /// The dotfile's path.
        path: String,
        /// The tenant the dotfile names.
        dotfile_tenant: String,
    },
    /// A written stage set none of the owning tenant's keys can open.
    StageSet {
        stage_set_id: i64,
        version: i64,
        volume_label: String,
    },
}

/// Does the recorded recipient list `fingerprints` (a JSON array of age
/// recipients, as `stage create` writes it) name any of `tenant_keys`?
/// `None` when the list is absent or unreadable, or when `tenant_keys` is
/// empty (the catalog holds no key for the tenant: a rebuilt one before
/// `key import`): not this check's to judge (see the module header).
pub fn readable_by(fingerprints: Option<&str>, tenant_keys: &[String]) -> Option<bool> {
    if tenant_keys.is_empty() {
        return None;
    }
    let keys: Vec<String> = serde_json::from_str(fingerprints?).ok()?;
    Some(keys.iter().any(|k| tenant_keys.contains(k)))
}

/// Every public key `tenant_id` holds, active or retired.
pub fn tenant_public_keys(conn: &Connection, tenant_id: i64) -> Result<Vec<String>> {
    let mut stmt = conn.prepare("SELECT public_key FROM encryption_keys WHERE tenant_id = ?1")?;
    let keys = stmt
        .query_map(params![tenant_id], |r| r.get(0))?
        .collect::<std::result::Result<Vec<String>, _>>()?;
    Ok(keys)
}

/// A written stage set: `(stage_set_id, version, volume_label,
/// key_fingerprints)`.
pub type WrittenSet = (i64, i64, String, Option<String>);

/// The written stage sets (completed writes on in-service volumes,
/// encrypted, and `range` writes, whose slices are on the volume too: #478)
/// of `unit_id`, or of every unit when `None`, keyed by unit, in volume
/// order: one statement whatever the unit count, so `audit` reads them once
/// for all units (`tests/catalog_budget.rs`).
pub fn written_stage_sets(
    conn: &Connection,
    unit_id: Option<i64>,
) -> Result<BTreeMap<i64, Vec<WrittenSet>>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT s.unit_id, ss.id, s.version, v.label, ss.key_fingerprints
         FROM writes w
         JOIN stage_sets ss ON ss.id = w.stage_set_id
         JOIN snapshots s ON s.id = ss.snapshot_id
         JOIN volumes v ON v.id = w.volume_id
         WHERE w.status IN ('completed', 'range') AND ss.encrypted = 1 AND {}
           AND (?1 IS NULL OR s.unit_id = ?1)
         ORDER BY v.label, ss.id",
        crate::policy::coverage::in_service("v")
    ))?;
    let mut out: BTreeMap<i64, Vec<WrittenSet>> = BTreeMap::new();
    for row in stmt.query_map(params![unit_id], |r| {
        Ok((
            r.get::<_, i64>(0)?,
            (
                r.get::<_, i64>(1)?,
                r.get::<_, i64>(2)?,
                r.get::<_, String>(3)?,
                r.get::<_, Option<String>>(4)?,
            ),
        ))
    })? {
        let (unit, set) = row?;
        out.entry(unit).or_default().push(set);
    }
    Ok(out)
}

/// Those of `written` that none of `tenant_keys` can open, as
/// `(stage_set_id, version, volume_label)`.
fn unreadable(written: &[WrittenSet], tenant_keys: &[String]) -> Vec<(i64, i64, String)> {
    written
        .iter()
        .filter(|(.., fps)| readable_by(fps.as_deref(), tenant_keys) == Some(false))
        .map(|(id, version, label, _)| (*id, *version, label.clone()))
        .collect()
}

/// The written stage sets of `unit_id` (completed and `range` writes on
/// in-service volumes, encrypted) that none of `tenant_keys` can open, as
/// `(stage_set_id, version, volume_label)`, in volume order.
pub fn unreadable_stage_sets(
    conn: &Connection,
    unit_id: i64,
    tenant_keys: &[String],
) -> Result<Vec<(i64, i64, String)>> {
    let written = written_stage_sets(conn, Some(unit_id))?;
    Ok(written
        .get(&unit_id)
        .map(|w| unreadable(w, tenant_keys))
        .unwrap_or_default())
}

/// How many written copies of `from_tenant`'s units none of `to_tenant`'s
/// keys opens: what a `tenant reassign` from one to the other leaves its
/// destination unable to read (#383). `None` when the catalog holds no key
/// for `to_tenant` (a rebuilt tenant before `key import`): not judged, as
/// [`readable_by`] says.
pub fn copies_unreadable_after_reassign(
    conn: &Connection,
    from_tenant: i64,
    to_tenant: i64,
) -> Result<Option<usize>> {
    let keys = tenant_public_keys(conn, to_tenant)?;
    if keys.is_empty() {
        return Ok(None);
    }
    let mut stmt = conn.prepare("SELECT id FROM units WHERE tenant_id = ?1")?;
    let units = stmt
        .query_map(params![from_tenant], |r| r.get::<_, i64>(0))?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    let mut n = 0;
    for unit_id in units {
        n += unreadable_stage_sets(conn, unit_id, &keys)?.len();
    }
    Ok(Some(n))
}

/// The tenant the unit's dotfile names, with the dotfile's path, when the
/// unit has a directory and a dotfile that reads. A unit with no dotfile
/// (`dotfiles = false` collections) or an unreadable one has nothing to
/// compare here; an unreadable dotfile is named by the commands that read
/// it.
fn dotfile_tenant(unit: &Unit) -> Option<(String, String)> {
    let dir = unit.current_path.as_deref()?;
    let path = Path::new(dir).join(crate::unit::dotfile::UNIT_DOTFILE);
    if !path.is_file() {
        return None;
    }
    let df = crate::unit::dotfile::read_dotfile(&path).ok()?;
    Some((path.display().to_string(), df.tenant))
}

/// Every tenant's name and public keys, by id: what [`disagreements`]
/// judges against, read once for a whole audit rather than per unit (two
/// statements, whatever the unit count; `tests/catalog_budget.rs`).
pub fn tenant_directory(conn: &Connection) -> Result<BTreeMap<i64, (String, Vec<String>)>> {
    let mut out: BTreeMap<i64, (String, Vec<String>)> = BTreeMap::new();
    let mut stmt = conn.prepare("SELECT id, name FROM tenants")?;
    for row in stmt.query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)))? {
        let (id, name) = row?;
        out.insert(id, (name, Vec::new()));
    }
    let mut stmt = conn
        .prepare("SELECT tenant_id, public_key FROM encryption_keys WHERE tenant_id IS NOT NULL")?;
    for row in stmt.query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)))? {
        let (id, key) = row?;
        if let Some((_, keys)) = out.get_mut(&id) {
            keys.push(key);
        }
    }
    Ok(out)
}

/// Every place `unit`'s tenancy disagrees with `units.tenant_id`: its
/// dotfile (an `active` unit only: the directory of a `tape_only` or
/// `missing` unit is not the unit's to read), then each written stage set
/// the owning tenant cannot open. `tenant_name` and `keys` are the owning
/// tenant's, from [`tenant_directory`]; `written` is the unit's own entry
/// from [`written_stage_sets`]. Neither is read here, so an audit judges
/// every unit from two whole-catalog reads.
pub fn disagreements(
    unit: &Unit,
    tenant_name: &str,
    keys: &[String],
    written: &[WrittenSet],
) -> Vec<Disagreement> {
    let mut out = Vec::new();
    if unit.status == "active" {
        if let Some((path, dotfile_tenant)) = dotfile_tenant(unit) {
            if dotfile_tenant != *tenant_name {
                out.push(Disagreement::Dotfile {
                    path,
                    dotfile_tenant,
                });
            }
        }
    }
    for (stage_set_id, version, volume_label) in unreadable(written, keys) {
        out.push(Disagreement::StageSet {
            stage_set_id,
            version,
            volume_label,
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn judge(conn: &Connection, unit: &Unit) -> Vec<Disagreement> {
        let tenants = tenant_directory(conn).unwrap();
        let (name, keys) = &tenants[&unit.tenant_id];
        let written = written_stage_sets(conn, None).unwrap();
        let mine = written.get(&unit.id).map(Vec::as_slice).unwrap_or_default();
        disagreements(unit, name, keys, mine)
    }

    #[test]
    fn a_list_naming_any_tenant_key_is_readable() {
        let keys = vec!["age1old".to_string(), "age1new".to_string()];
        assert_eq!(
            readable_by(Some(r#"["age1op","age1old","age1escrow"]"#), &keys),
            Some(true)
        );
        assert_eq!(
            readable_by(Some(r#"["age1op","age1other","age1escrow"]"#), &keys),
            Some(false)
        );
        assert_eq!(readable_by(None, &keys), None, "absent: not judged");
        assert_eq!(readable_by(Some("not json"), &keys), None);
        assert_eq!(
            readable_by(Some(r#"["age1old"]"#), &[]),
            None,
            "a tenant the catalog holds no key for (a rebuilt catalog): not judged"
        );
    }

    /// What a reassign from `acme` to `other` leaves `other` unable to
    /// open: the sealed copy encrypted to acme's key only.
    #[test]
    fn a_reassign_counts_the_copies_the_destination_cannot_open() {
        let conn = crate::db::open_memory().unwrap();
        for (name, key) in [("acme", "age1acme"), ("other", "age1other")] {
            conn.execute(
                "INSERT INTO tenants (name, is_operator, status) VALUES (?1, 0, 'active')",
                params![name],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO encryption_keys (tenant_id, alias, fingerprint, public_key)
                 VALUES (last_insert_rowid(), ?1, ?1, ?2)",
                params![name, key],
            )
            .unwrap();
        }
        let id = |name: &str| -> i64 {
            conn.query_row(
                "SELECT id FROM tenants WHERE name = ?1",
                params![name],
                |r| r.get(0),
            )
            .unwrap()
        };
        conn.execute(
            "INSERT INTO units (uuid, name, tenant_id, status) VALUES ('u', 'one', ?1, 'active')",
            params![id("acme")],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO snapshots (unit_id, version, snapshot_type, status, source_path)
             VALUES (last_insert_rowid(), 1, 'full', 'current', '/src')",
            [],
        )
        .unwrap();
        let snap = conn.last_insert_rowid();
        conn.execute(
            "INSERT INTO volumes (label, backend_type, backend_name, media_type, capacity_bytes, status)
             VALUES ('V1', 'lto', 'lto0', 'LTO-6', 2500000000000, 'sealed')",
            [],
        )
        .unwrap();
        let vol = conn.last_insert_rowid();
        let write = |list: &str| {
            conn.execute(
                "INSERT INTO stage_sets (snapshot_id, status, slice_size, encrypted, key_fingerprints)
                 VALUES (?1, 'staged', 524288, 1, ?2)",
                params![snap, list],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO writes (stage_set_id, snapshot_id, volume_id, status)
                 VALUES (last_insert_rowid(), ?1, ?2, 'completed')",
                params![snap, vol],
            )
            .unwrap();
        };
        write(r#"["age1acme","age1op"]"#);
        assert_eq!(
            copies_unreadable_after_reassign(&conn, id("acme"), id("other")).unwrap(),
            Some(1)
        );
        assert_eq!(
            copies_unreadable_after_reassign(&conn, id("acme"), id("acme")).unwrap(),
            Some(0)
        );
        write(r#"["age1acme","age1other","age1op"]"#);
        assert_eq!(
            copies_unreadable_after_reassign(&conn, id("acme"), id("other")).unwrap(),
            Some(1),
            "only the copy encrypted without other's key"
        );

        // #478: a range of a spannable set is on its volume too, encrypted
        // to its set's list, so it counts as a written copy here.
        conn.execute(
            "INSERT INTO stage_sets (snapshot_id, status, slice_size, encrypted, key_fingerprints)
             VALUES (?1, 'staged', 524288, 1, '[\"age1acme\",\"age1op\"]')",
            params![snap],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO writes (stage_set_id, snapshot_id, volume_id, status, may_span)
             VALUES (last_insert_rowid(), ?1, ?2, 'range', 1)",
            params![snap, vol],
        )
        .unwrap();
        assert_eq!(
            copies_unreadable_after_reassign(&conn, id("acme"), id("other")).unwrap(),
            Some(2),
            "a range other cannot open is a copy it cannot open"
        );

        // A destination the catalog holds no key for (a rebuilt tenant
        // before `key import`) is not judged: not "0", not "every copy".
        conn.execute(
            "INSERT INTO tenants (name, is_operator, status) VALUES ('keyless', 0, 'active')",
            [],
        )
        .unwrap();
        assert_eq!(
            copies_unreadable_after_reassign(&conn, id("acme"), id("keyless")).unwrap(),
            None
        );
    }

    /// An active unit whose dotfile names another tenant than the catalog
    /// is a disagreement; once the dotfile is corrected it is not, and a
    /// `missing` unit's directory is never read.
    #[test]
    fn a_dotfile_naming_another_tenant_disagrees() {
        let conn = crate::db::open_memory().unwrap();
        for name in ["alice", "bob"] {
            conn.execute(
                "INSERT INTO tenants (name, is_operator, status) VALUES (?1, 0, 'active')",
                params![name],
            )
            .unwrap();
        }
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().to_string_lossy().to_string();
        conn.execute(
            "INSERT INTO units (uuid, name, tenant_id, current_path, status)
             VALUES ('u-1', 'one', (SELECT id FROM tenants WHERE name = 'bob'), ?1, 'active')",
            params![dir],
        )
        .unwrap();
        let mut df = crate::unit::dotfile::UnitDotfile {
            uuid: "u-1".into(),
            name: "one".into(),
            created: "2026-01-01T00:00:00Z".into(),
            tags: vec![],
            tenant: "alice".into(),
            archive_set: None,
            checksum_mode: None,
            compression: None,
            slice_size: None,
            warehouse_copies: None,
            allow_spanning: None,
            copies_at: None,
            exclude_patterns: vec![],
        };
        let path = tmp.path().join(crate::unit::dotfile::UNIT_DOTFILE);
        crate::unit::dotfile::write_dotfile(&path, &df).unwrap();
        let unit = crate::db::queries::get_unit_by_name(&conn, "one")
            .unwrap()
            .unwrap();

        assert_eq!(
            judge(&conn, &unit),
            vec![Disagreement::Dotfile {
                path: path.display().to_string(),
                dotfile_tenant: "alice".into(),
            }]
        );

        let missing = Unit {
            status: "missing".into(),
            ..unit.clone()
        };
        assert!(judge(&conn, &missing).is_empty());

        df.tenant = "bob".into();
        crate::unit::dotfile::write_dotfile(&path, &df).unwrap();
        assert!(judge(&conn, &unit).is_empty());
    }
}
