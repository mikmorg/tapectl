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
//! The volume filter is [`crate::policy::coverage::in_service`] and
//! `encrypted = 1`, as for escrow coverage's reporting scopes
//! (`policy::escrow`): the question is about bytes tapectl still accounts
//! for, and a stage set written in the clear is `encryption`'s finding.

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
/// `None` when the list is absent or unreadable: not this check's to judge
/// (see the module header).
pub fn readable_by(fingerprints: Option<&str>, tenant_keys: &[String]) -> Option<bool> {
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

/// The written stage sets of `unit_id` (completed writes on in-service
/// volumes, encrypted) that none of `tenant_keys` can open, as
/// `(stage_set_id, version, volume_label)`, in volume order.
pub fn unreadable_stage_sets(
    conn: &Connection,
    unit_id: i64,
    tenant_keys: &[String],
) -> Result<Vec<(i64, i64, String)>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT ss.id, s.version, v.label, ss.key_fingerprints
         FROM writes w
         JOIN stage_sets ss ON ss.id = w.stage_set_id
         JOIN snapshots s ON s.id = ss.snapshot_id
         JOIN volumes v ON v.id = w.volume_id
         WHERE w.status = 'completed' AND ss.encrypted = 1 AND {}
           AND s.unit_id = ?1
         ORDER BY v.label, ss.id",
        crate::policy::coverage::in_service("v")
    ))?;
    let rows = stmt
        .query_map(params![unit_id], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, i64>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, Option<String>>(3)?,
            ))
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    Ok(rows
        .into_iter()
        .filter(|(.., fps)| readable_by(fps.as_deref(), tenant_keys) == Some(false))
        .map(|(id, version, label, _)| (id, version, label))
        .collect())
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

/// Every place `unit`'s tenancy disagrees with `units.tenant_id`: its
/// dotfile (an `active` unit only: the directory of a `tape_only` or
/// `missing` unit is not the unit's to read), then each written stage set
/// the owning tenant cannot open.
pub fn disagreements(conn: &Connection, unit: &Unit) -> Result<Vec<Disagreement>> {
    let tenant_name: String = conn.query_row(
        "SELECT name FROM tenants WHERE id = ?1",
        params![unit.tenant_id],
        |r| r.get(0),
    )?;
    let mut out = Vec::new();
    if unit.status == "active" {
        if let Some((path, dotfile_tenant)) = dotfile_tenant(unit) {
            if dotfile_tenant != tenant_name {
                out.push(Disagreement::Dotfile {
                    path,
                    dotfile_tenant,
                });
            }
        }
    }
    let keys = tenant_public_keys(conn, unit.tenant_id)?;
    for (stage_set_id, version, volume_label) in unreadable_stage_sets(conn, unit.id, &keys)? {
        out.push(Disagreement::StageSet {
            stage_set_id,
            version,
            volume_label,
        });
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

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
        assert_eq!(readable_by(Some(r#"["age1old"]"#), &[]), Some(false));
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
            exclude_patterns: vec![],
        };
        let path = tmp.path().join(crate::unit::dotfile::UNIT_DOTFILE);
        crate::unit::dotfile::write_dotfile(&path, &df).unwrap();
        let unit = crate::db::queries::get_unit_by_name(&conn, "one")
            .unwrap()
            .unwrap();

        assert_eq!(
            disagreements(&conn, &unit).unwrap(),
            vec![Disagreement::Dotfile {
                path: path.display().to_string(),
                dotfile_tenant: "alice".into(),
            }]
        );

        let missing = Unit {
            status: "missing".into(),
            ..unit.clone()
        };
        assert!(disagreements(&conn, &missing).unwrap().is_empty());

        df.tenant = "bob".into();
        crate::unit::dotfile::write_dotfile(&path, &df).unwrap();
        assert!(disagreements(&conn, &unit).unwrap().is_empty());
    }
}
