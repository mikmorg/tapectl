use std::collections::BTreeMap;

use rusqlite::{params, Connection};

use crate::config::Config;
use crate::db::models::Unit;
use crate::error::{PolicyLayer, Result, TapectlError};

pub mod compression_capability;
pub mod coverage;
pub mod decorative;
pub mod depth_check;
pub mod escrow;
pub mod evidence;
pub mod lenient_config;
pub mod reclaimable;
pub mod shadowing;
pub mod subsumed;
pub mod tape_alerts;
pub mod tenancy;
pub mod unknown_keys;

/// Resolved policy for a unit after 3-level resolution:
/// unit dotfile [policy] > archive_set > system [defaults].
#[derive(Debug, Clone)]
pub struct ResolvedPolicy {
    pub min_copies: i64,
    pub required_locations: Vec<String>,
    pub encrypt: bool,
    pub compression: String,
    pub checksum_mode: String,
    pub slice_size: i64,
    pub verify_interval_days: Option<i64>,
    pub preserve_xattrs: bool,
    pub preserve_acls: bool,
    pub preserve_fsa: bool,
    pub dirty_on_metadata_change: bool,
    /// ADR-0006: how many recorded WAREHOUSE deposits this unit should
    /// carry, on top of its tape copies. 0 (the default) means none are
    /// expected and `audit` says nothing about warehouses for this unit.
    pub warehouse_copies: i64,
    /// #478 (ADR-0012 item 82): whether the unit may span tapes by its
    /// slices. Off unless a layer turns it on (`[defaults]` is false when
    /// the key is absent). Read at use, like every field here: a write that
    /// may span must freeze it when it is planned (`writes.may_span`, item
    /// 121), because the dotfile can change while a span is in progress.
    pub allow_spanning: bool,
    /// #493 (ADR-0012 items 148, 149, 155, 156): how many copies the unit
    /// should hold at each named location. Empty when no layer sets a
    /// target: the unit then behaves exactly as before 1.7.x.
    ///
    /// **The most specific layer that sets the map wins entirely** (item
    /// 156): the dotfile's `[policy.copies_at]`, else the archive set's, else
    /// `[defaults] copies_at`. Maps never merge across layers: a dotfile that
    /// names only `offsite` drops a `home` its archive set named. An empty
    /// map is a setting too (an explicit "no targets" that hides the layers
    /// below), as `allow_spanning = false` is a choice and not silence.
    ///
    /// **An archive set's `required_locations` counts as `{name = 1}` at that
    /// set's layer** (item 156), with the set's own `copies_at` laid over it
    /// name by name: `required_locations = ["home", "offsite"]` with
    /// `copies_at = { home = 2 }` is `{ home = 2, offsite = 1 }`. The set
    /// "sets" the map when its `copies_at` is not NULL or its
    /// `required_locations` names at least one location, so a set that only
    /// requires locations hides `[defaults] copies_at`. `required_locations`
    /// itself is unchanged and still resolved into its own field.
    ///
    /// **The dotfile layer** is the dotfile when it is present, with or
    /// without the key (present without it, the layer is silent and the
    /// frozen value is NOT used); and `units.copies_at_frozen` (item 148,
    /// [`freeze_copies_at`]) when the dotfile is absent or the unit has no
    /// path, so a unit whose source is gone keeps the targets its dotfile
    /// last set instead of silently deferring to its archive set.
    ///
    /// Every count is 0 or more ([`crate::config::validate_copies_at`]), and
    /// every name a `copies_at` key brought into the winning map is a
    /// registered location (item 155), or the unit's policy is
    /// unresolvable, naming the name and the layer. Names folded in from
    /// `required_locations` are checked where they are written (`archive-set
    /// create`/`edit`/`sync`, #348), as before, so a catalog that sets no
    /// `copies_at` resolves exactly as it did, and with no statement more.
    pub copies_at: BTreeMap<String, i64>,
}

impl ResolvedPolicy {
    /// **The** copy floor's base, before the tape-only multiplier (#493,
    /// ADR-0012 item 149): the larger of `min_copies` and the sum of the
    /// per-location targets in [`Self::copies_at`]. `min_copies` stays the
    /// total floor; a map whose targets add up to more raises it, since a
    /// copy is on one shelf and so can meet one location's target only.
    /// With an empty map it is `min_copies`, today's floor exactly.
    ///
    /// Every copy floor goes through this and then
    /// [`coverage::copy_floor`] (which multiplies it for a tape-only unit,
    /// item 46): never read `min_copies` as a floor directly.
    pub fn copy_floor_base(&self) -> i64 {
        let targets = self
            .copies_at
            .values()
            .fold(0i64, |sum, n| sum.saturating_add(*n));
        self.min_copies.max(targets)
    }
}

/// Resolve the effective policy for a unit.
///
/// Resolution order (first non-NULL wins):
/// 1. Unit dotfile [policy] section (read from disk if available)
/// 2. Archive set (from DB via unit.archive_set_id)
/// 3. System defaults (from config.toml)
///
/// `verify_interval_days` is not a dotfile key, so for it the order is
/// archive set, then `[defaults]` (which carries no value unless the
/// operator set one, issue #453), then none.
///
/// Returns `Err` if the unit dotfile's own `[policy]` section (the only
/// layer parsed at USE time, not at config load — see below) is not valid.
/// `config.defaults.slice_size`/`compression`/`checksum_mode` and any
/// `archive_sets` equivalents are already validated at config load /
/// archive-set write time respectively, so in practice this can only fail on
/// a bad operator-authored dotfile value: an invalid `slice_size` string
/// (issue #59), a `compression`/`checksum_mode` outside the closed set
/// `config::validate_compression`/`validate_checksum_mode` accept (issue
/// #213 — the same rule ADR-0012 states for every other closed-set field,
/// applied here for the first time), or an unrecognized `[policy]` key
/// (issue #211 — `PolicySection`'s `#[serde(deny_unknown_fields)]` refuses a
/// misspelled key by name instead of silently deferring upward forever).
///
/// **Every layer now fails loudly rather than falling through (issue #105.)**
/// Each layer used to be wrapped in `if let Ok(..)`, so a database error, a
/// dangling `archive_set_id`, a corrupt `required_locations` JSON, or an
/// unreadable/unparseable dotfile all degraded *silently* to the weaker
/// system defaults — and the unit then read as compliant against a policy
/// its operator never chose. That is strictly worse than a known-violating
/// unit, because the tool cannot tell. `audit` catches the `Err` and reports
/// `policy_unresolvable` as a VIOLATION naming the checks it skipped (the
/// shape #59 established); do not add a second error style here.
///
/// The one deliberate silence: **an ABSENT dotfile is normal** and defers
/// upward (the #92 contract). Only a dotfile that is *present* and cannot be
/// read or parsed is an error. Conflating the two makes every unit without a
/// dotfile start failing.
pub fn resolve(conn: &Connection, config: &Config, unit: &Unit) -> Result<ResolvedPolicy> {
    let defaults = &config.defaults;

    // Start with system defaults
    let mut policy = ResolvedPolicy {
        min_copies: defaults.min_copies as i64,
        required_locations: Vec::new(),
        encrypt: defaults.encrypt,
        compression: defaults.compression.clone(),
        checksum_mode: defaults.checksum_mode.clone(),
        slice_size: crate::staging::parse_size_to_bytes(&defaults.slice_size).map_err(|e| {
            TapectlError::PolicyUnresolvable {
                layer: PolicyLayer::Defaults,
                detail: format!("config.toml [defaults] slice_size is invalid: {e}"),
            }
        })?,
        // Level 3 (issue #453): `[defaults] verify_interval_days`, 365 when
        // the key is absent (ADR-0012 item 75), so a unit always has an
        // interval. An archive set's own value, below, replaces it; a
        // dotfile cannot set the key at all.
        verify_interval_days: Some(defaults.verify_interval_days),
        preserve_xattrs: defaults.preserve_xattrs,
        preserve_acls: defaults.preserve_acls,
        preserve_fsa: defaults.preserve_fsa,
        dirty_on_metadata_change: defaults.dirty_on_metadata_change,
        warehouse_copies: defaults.warehouse_copies,
        allow_spanning: defaults.allow_spanning,
        copies_at: defaults.copies_at.clone(),
    };
    // Which layer set `policy.copies_at`, and the names in it that a
    // `copies_at` key (not `required_locations`) brought, for the
    // registered-location check at the end.
    let mut copies_at_from = CopiesAtSource {
        layer: PolicyLayer::Defaults,
        what: "config.toml's [defaults] copies_at".to_string(),
        keyed: defaults.copies_at.keys().cloned().collect(),
    };

    // Layer 2: Archive set (if unit has one)
    if let Some(as_id) = unit.archive_set_id {
        // A dangling `archive_set_id` is corruption, not a normal state:
        // nothing in tapectl deletes an `archive_sets` row, and `db::open*`
        // runs with `PRAGMA foreign_keys = ON`, so the reference cannot go
        // stale through any supported path. Silently skipping the layer
        // would hand the unit the system defaults it was explicitly moved
        // off — so it is mapped to a message that names the unit and the id
        // rather than propagating rusqlite's bare "Query returned no rows",
        // which an operator cannot act on.
        let row = conn
            .query_row(
                "SELECT min_copies, required_locations, encrypt, compression, checksum_mode,
                    slice_size, verify_interval_days, preserve_xattrs, preserve_acls,
                    preserve_fsa, dirty_on_metadata_change, warehouse_copies, allow_spanning,
                    copies_at, name
             FROM archive_sets WHERE id = ?1",
                params![as_id],
                |row| {
                    Ok((
                        row.get::<_, Option<i64>>(0)?,
                        row.get::<_, Option<String>>(1)?,
                        row.get::<_, Option<i64>>(2)?,
                        row.get::<_, Option<String>>(3)?,
                        row.get::<_, Option<String>>(4)?,
                        row.get::<_, Option<i64>>(5)?,
                        row.get::<_, Option<i64>>(6)?,
                        row.get::<_, Option<i64>>(7)?,
                        row.get::<_, Option<i64>>(8)?,
                        row.get::<_, Option<i64>>(9)?,
                        row.get::<_, Option<i64>>(10)?,
                        row.get::<_, Option<i64>>(11)?,
                        row.get::<_, Option<i64>>(12)?,
                        row.get::<_, Option<String>>(13)?,
                        row.get::<_, String>(14)?,
                    ))
                },
            )
            .map_err(|e| match e {
                rusqlite::Error::QueryReturnedNoRows => TapectlError::PolicyUnresolvable {
                    layer: PolicyLayer::ArchiveSet,
                    detail: format!(
                        "unit \"{}\" references archive_set_id {} but no such archive set \
                         exists — the catalog is inconsistent",
                        unit.name, as_id
                    ),
                },
                other => TapectlError::Database(other),
            })?;

        let (
            min_copies,
            locations_json,
            encrypt,
            compression,
            checksum_mode,
            slice_size,
            verify_days,
            preserve_xattrs,
            preserve_acls,
            preserve_fsa,
            dirty_on_meta,
            warehouse_copies,
            allow_spanning,
            copies_at_json,
            set_name,
        ) = row;

        if let Some(v) = min_copies {
            policy.min_copies = v;
        }
        if let Some(locs) = locations_json {
            // Every writer (`archive-set create`, `edit`, `sync`) stores this
            // via `serde_json::to_string` on a `Vec`, and stores NULL when the
            // operator sets none — so a non-NULL value that will not parse is
            // corruption, never a legitimate shape. Swallowing it yielded an
            // EMPTY vec, i.e. "no locations required", which is precisely the
            // silent downgrade this function no longer performs. (Not named by
            // issue #105; same class, same function.)
            policy.required_locations =
                serde_json::from_str::<Vec<String>>(&locs).map_err(|e| {
                    TapectlError::PolicyUnresolvable {
                        layer: PolicyLayer::ArchiveSet,
                        detail: format!(
                            "archive set {as_id} has an unparseable required_locations value \
                             ({e}); expected a JSON array of location names, found: {locs}"
                        ),
                    }
                })?;
        }
        if let Some(v) = encrypt {
            policy.encrypt = v != 0;
        }
        if let Some(v) = compression {
            policy.compression = v;
        }
        if let Some(v) = checksum_mode {
            policy.checksum_mode = v;
        }
        if let Some(v) = slice_size {
            policy.slice_size = v;
        }
        if let Some(v) = verify_days {
            policy.verify_interval_days = Some(v);
        }
        if let Some(v) = preserve_xattrs {
            policy.preserve_xattrs = v != 0;
        }
        if let Some(v) = preserve_acls {
            policy.preserve_acls = v != 0;
        }
        if let Some(v) = preserve_fsa {
            policy.preserve_fsa = v != 0;
        }
        if let Some(v) = dirty_on_meta {
            policy.dirty_on_metadata_change = v != 0;
        }
        if let Some(v) = warehouse_copies {
            policy.warehouse_copies = v;
        }
        // NULL defers to `[defaults]` (migration 039), never reads as "off".
        if let Some(v) = allow_spanning {
            policy.allow_spanning = v != 0;
        }
        // #493: the set's map is its `required_locations` at one copy each,
        // with its own `copies_at` (migration 040; NULL defers) laid over
        // it. Either one makes this layer set the map (see the field's doc).
        // Every writer stores `serde_json::to_string` of a map, so a value
        // that will not parse is corruption, refused by name like a corrupt
        // `required_locations` above.
        let set_copies_at = copies_at_json
            .map(|text| {
                serde_json::from_str::<BTreeMap<String, i64>>(&text).map_err(|e| {
                    TapectlError::PolicyUnresolvable {
                        layer: PolicyLayer::ArchiveSet,
                        detail: format!(
                            "archive set \"{set_name}\" has an unparseable copies_at value \
                             ({e}); expected a JSON object of location names to copy counts, \
                             found: {text}"
                        ),
                    }
                })
            })
            .transpose()?;
        if set_copies_at.is_some() || !policy.required_locations.is_empty() {
            let mut map: BTreeMap<String, i64> = policy
                .required_locations
                .iter()
                .map(|name| (name.clone(), 1))
                .collect();
            let keyed = set_copies_at
                .as_ref()
                .map(|m| m.keys().cloned().collect())
                .unwrap_or_default();
            map.extend(set_copies_at.unwrap_or_default());
            policy.copies_at = map;
            copies_at_from = CopiesAtSource {
                layer: PolicyLayer::ArchiveSet,
                what: format!("archive set \"{set_name}\"'s copies_at"),
                keyed,
            };
        }
    }

    // Layer 1: Unit dotfile [policy] section (highest priority)
    // Read from disk if the unit has a path
    let mut dotfile_present = false;
    if let Some(ref path) = unit.current_path {
        let dotfile_path = std::path::Path::new(path).join(".tapectl-unit.toml");
        // `exists()` is the whole absent-vs-present split: a unit with no
        // dotfile defers upward in silence (#92). Past this point the file
        // IS there, so both an IO failure and a parse failure are real —
        // including the TOCTOU case where it is removed between the check
        // and the read, which is NOT the same as never having existed and
        // must not be quietly folded back into "absent".
        if dotfile_path.exists() {
            dotfile_present = true;
            let contents = std::fs::read_to_string(&dotfile_path).map_err(|e| {
                TapectlError::PolicyUnresolvable {
                    layer: PolicyLayer::Dotfile,
                    detail: format!(
                        "unit \"{}\" has a .tapectl-unit.toml at {} that cannot be read ({e})",
                        unit.name,
                        dotfile_path.display()
                    ),
                }
            })?;
            let toml =
                contents
                    .parse::<toml::Table>()
                    .map_err(|e| TapectlError::PolicyUnresolvable {
                        layer: PolicyLayer::Dotfile,
                        detail: format!(
                            "unit \"{}\" has a malformed .tapectl-unit.toml at {} ({e})",
                            unit.name,
                            dotfile_path.display()
                        ),
                    })?;

            // Issue #263 route 1: a misspelled top-level table name
            // (`[polcy]` instead of `[policy]`) means `toml.get("policy")`
            // below returns `None`, so this whole layer would silently
            // defer upward as though the operator never wrote a `[policy]`
            // section — the exact same silent-downgrade shape #211 already
            // closed for a misspelled KEY *inside* a correctly-named
            // `[policy]` table, just one level up. Checked against
            // `dotfile::DOTFILE_TOP_LEVEL_TABLES` rather than deserializing
            // the whole document into `DotfileToml` (which would also
            // require a fully valid `[unit]` table this function has no
            // other use for — it never reads `[unit]`, only `[policy]`).
            if let Some(bad_key) = toml
                .keys()
                .find(|k| !crate::unit::dotfile::DOTFILE_TOP_LEVEL_TABLES.contains(&k.as_str()))
            {
                return Err(TapectlError::PolicyUnresolvable {
                    layer: PolicyLayer::Dotfile,
                    detail: format!(
                        "unit \"{}\" has an unrecognized top-level table [{}] in {} \
                         — a .tapectl-unit.toml may only declare [unit], [policy], \
                         and [excludes]",
                        unit.name,
                        bad_key,
                        dotfile_path.display()
                    ),
                });
            }

            if let Some(pol_value) = toml.get("policy") {
                // Issue #211: deserialize straight into `PolicySection`
                // instead of hand-picking keys off the raw table. That
                // struct carries `#[serde(deny_unknown_fields)]`, so a
                // misspelled or non-existent key (`slize_size`, `min_copies`
                // -- the latter belongs to an archive_set, never a unit
                // dotfile) is refused by name here, at the highest-priority
                // layer, instead of silently deferring upward forever
                // (ADR-0012). This is the SAME struct `read_dotfile` uses,
                // so the two paths can never disagree about what a
                // dotfile's `[policy]` table may contain.
                let section: crate::unit::dotfile::PolicySection = pol_value
                    .clone()
                    .try_into()
                    .map_err(|e: toml::de::Error| TapectlError::PolicyUnresolvable {
                        layer: PolicyLayer::Dotfile,
                        detail: format!(
                            "unit \"{}\" has an invalid [policy] section in {} ({e})",
                            unit.name,
                            dotfile_path.display()
                        ),
                    })?;

                // Issue #213: `checksum_mode`/`compression` used to be
                // assigned straight through with no validation, so a typo
                // surfaced hours later as a raw `dar` failure (compression)
                // or an opaque SQLite CHECK-constraint rejection (checksum
                // mode) at stage time. Closed-set-validated here, at the
                // same boundary `config.toml`'s `[defaults]` and
                // `archive-set create/edit` already use the same two
                // validators for (ADR-0012) -- one rule stated once, not
                // three times.
                if let Some(v) = section.checksum_mode {
                    crate::config::validate_checksum_mode(&v).map_err(|e| {
                        TapectlError::PolicyUnresolvable {
                            layer: PolicyLayer::Dotfile,
                            detail: format!(
                                "unit \"{}\" has an invalid [policy] checksum_mode in {} ({e})",
                                unit.name,
                                dotfile_path.display()
                            ),
                        }
                    })?;
                    policy.checksum_mode = v;
                }
                if let Some(v) = section.compression {
                    crate::config::validate_compression(&v).map_err(|e| {
                        TapectlError::PolicyUnresolvable {
                            layer: PolicyLayer::Dotfile,
                            detail: format!(
                                "unit \"{}\" has an invalid [policy] compression in {} ({e})",
                                unit.name,
                                dotfile_path.display()
                            ),
                        }
                    })?;
                    policy.compression = v;
                }
                if let Some(v) = section.slice_size {
                    policy.slice_size = crate::staging::parse_size_to_bytes(&v).map_err(|e| {
                        TapectlError::PolicyUnresolvable {
                            layer: PolicyLayer::Dotfile,
                            detail: format!(
                                "unit \"{}\" has an invalid [policy] slice_size in {} ({e})",
                                unit.name,
                                dotfile_path.display()
                            ),
                        }
                    })?;
                }
                // ADR-0006 / issue #73. Absent means "defer to the archive
                // set", so there is deliberately no default filled in
                // anywhere on the way in (issue #92 -- a filled default is
                // indistinguishable from an operator choice and would
                // silently outrank the archive set).
                if let Some(v) = section.warehouse_copies {
                    policy.warehouse_copies = v;
                }
                // #478: absent defers to the archive set; `false` is a
                // choice, and outranks a set that allows spanning.
                if let Some(v) = section.allow_spanning {
                    policy.allow_spanning = v;
                }
                // #493 (items 155, 156): present, the dotfile's map replaces
                // the lower layers' whole; absent, they stand.
                if let Some(map) = section.copies_at {
                    copies_at_from = CopiesAtSource {
                        layer: PolicyLayer::Dotfile,
                        what: format!("the [policy.copies_at] of {}", dotfile_path.display()),
                        keyed: map.keys().cloned().collect(),
                    };
                    policy.copies_at = map;
                }
            }
        }
    }

    // #493 (item 148): with no dotfile to read, the dotfile layer is what the
    // dotfile last said with the source present (`freeze_copies_at`). Only
    // when ABSENT: a present dotfile without the key is silent, and its
    // frozen value, which the next freeze point clears, must not leak
    // through. Read from the row, so this costs no statement.
    if !dotfile_present {
        if let Some(text) = &unit.copies_at_frozen {
            let map = serde_json::from_str::<BTreeMap<String, i64>>(text).map_err(|e| {
                TapectlError::PolicyUnresolvable {
                    layer: PolicyLayer::Dotfile,
                    detail: format!(
                        "unit \"{}\" has an unparseable frozen copies_at \
                         (units.copies_at_frozen: {e}); expected a JSON object of location \
                         names to copy counts, found: {text}",
                        unit.name
                    ),
                }
            })?;
            copies_at_from = CopiesAtSource {
                layer: PolicyLayer::Dotfile,
                what: format!(
                    "the copies_at its dotfile last set (frozen in the catalog; the \
                     .tapectl-unit.toml{} is not there to read)",
                    unit.current_path
                        .as_deref()
                        .map(|p| format!(" in {p}"))
                        .unwrap_or_default()
                ),
                keyed: map.keys().cloned().collect(),
            };
            policy.copies_at = map;
        }
    }

    check_copies_at(conn, unit, &policy.copies_at, &copies_at_from)?;

    Ok(policy)
}

/// Freeze the unit dotfile layer's own `copies_at` into
/// `units.copies_at_frozen` (#493, ADR-0012 item 148). Returns whether the
/// stored value changed.
///
/// **What is frozen is the DOTFILE layer's own map, never the resolved one**
/// (the coordinator's mechanics ruling): the dotfile is the only layer lost
/// when a unit's source goes, while its archive set and `[defaults]` stay
/// live and are read at use like every other key. [`resolve`] reads the
/// frozen map as the dotfile layer only while the dotfile is absent, and
/// stays read-only: freezing happens here, at the points where tapectl
/// already writes the unit with its source present: `snapshot create`,
/// `stage create` (after its policy resolved, so a bad dotfile never
/// freezes), and `collection sync`'s adoption of a dotfile (which calls
/// [`store_frozen_copies_at`] with the dotfile it already read). `collection
/// run` freezes through its own snapshot and stage of each unit.
///
/// Item 148 freezes "with its dotfile present", so:
/// - **present with `[policy.copies_at]`**: that map is stored (an empty
///   map included: an explicit "no targets");
/// - **present without the key**: NULL is stored, clearing what an earlier
///   freeze left, since the dotfile now sets none;
/// - **absent** (no file in the folder, the folder gone, no path): nothing
///   changes. There is no dotfile to capture, and [`resolve`] reads the
///   frozen map exactly then, so a freeze point must not move what it
///   answers. A unit whose dotfile was deleted keeps the targets it last
///   set until a dotfile says otherwise.
///
/// A dotfile that is present but cannot be read or parsed is an error,
/// naming the file (`dotfile::read_dotfile`), as it already is for every
/// freeze point's own read of it. Names and counts are not checked here:
/// the map is frozen as the dotfile says it, and [`resolve`] refuses a bad
/// one by name, as it would the dotfile itself.
pub fn freeze_copies_at(conn: &Connection, unit: &Unit) -> Result<bool> {
    let Some(path) = unit.current_path.as_deref() else {
        return Ok(false);
    };
    let dotfile_path = std::path::Path::new(path).join(crate::unit::dotfile::UNIT_DOTFILE);
    if !dotfile_path.exists() {
        return Ok(false);
    }
    let dotfile = crate::unit::dotfile::read_dotfile(&dotfile_path)?;
    store_frozen_copies_at(
        conn,
        unit.id,
        &unit.name,
        unit.tenant_id,
        unit.copies_at_frozen.as_deref(),
        dotfile.copies_at.as_ref(),
    )
}

/// Store `copies_at` (a present dotfile's own map, `None` when it sets none)
/// as unit `unit_id`'s frozen dotfile layer, when it differs from `stored`
/// (the row's current `copies_at_frozen`), and record the change as a
/// `copies_at_frozen` event. Returns whether it wrote. The JSON object is
/// `serde_json`'s of a sorted map, so an unchanged map is never rewritten.
pub fn store_frozen_copies_at(
    conn: &Connection,
    unit_id: i64,
    unit_name: &str,
    tenant_id: i64,
    stored: Option<&str>,
    copies_at: Option<&BTreeMap<String, i64>>,
) -> Result<bool> {
    let text = copies_at.map(|m| serde_json::to_string(m).expect("a map of names to counts"));
    if text.as_deref() == stored {
        return Ok(false);
    }
    conn.execute(
        "UPDATE units SET copies_at_frozen = ?1 WHERE id = ?2",
        params![text, unit_id],
    )?;
    crate::db::events::log_event(
        conn,
        "unit",
        unit_id,
        Some(unit_name),
        "copies_at_frozen",
        Some("copies_at_frozen"),
        stored,
        text.as_deref(),
        None,
        Some(tenant_id),
    )?;
    Ok(true)
}

/// Which layer set a resolved `copies_at` map, for [`check_copies_at`].
struct CopiesAtSource {
    layer: PolicyLayer,
    /// The map as an operator finds it, for the message.
    what: String,
    /// The names a `copies_at` key brought into the map. Names folded in
    /// from an archive set's `required_locations` are not here: they were
    /// checked when written (#348).
    keyed: Vec<String>,
}

/// The resolved `copies_at` map's two rules (#493, ADR-0012 items 149 and
/// 155): every count is 0 or more, and every name a `copies_at` key brought
/// is a registered location. Either failing makes the unit's policy
/// unresolvable, naming the unit, the layer and what to fix. The location
/// list is read in ONE statement, and only when there is a name to check,
/// so a catalog that sets no `copies_at` pays nothing for it.
fn check_copies_at(
    conn: &Connection,
    unit: &Unit,
    map: &BTreeMap<String, i64>,
    from: &CopiesAtSource,
) -> Result<()> {
    crate::config::validate_copies_at(map).map_err(|e| TapectlError::PolicyUnresolvable {
        layer: from.layer,
        detail: format!("unit \"{}\": {} {e}", unit.name, from.what),
    })?;
    if from.keyed.is_empty() {
        return Ok(());
    }
    let registered: Vec<String> = conn
        .prepare("SELECT name FROM locations ORDER BY name")?
        .query_map([], |row| row.get(0))?
        .collect::<std::result::Result<_, _>>()?;
    let unknown: Vec<String> = from
        .keyed
        .iter()
        .filter(|name| !registered.contains(name))
        .map(|name| format!("\"{name}\""))
        .collect();
    if unknown.is_empty() {
        return Ok(());
    }
    let known = if registered.is_empty() {
        "no locations are registered yet".to_string()
    } else {
        format!("registered locations: {}", registered.join(", "))
    };
    let what = if unknown.len() == 1 {
        "which is not a registered location"
    } else {
        "which are not registered locations"
    };
    Err(TapectlError::PolicyUnresolvable {
        layer: from.layer,
        detail: format!(
            "unit \"{}\": {} names {}, {what} ({known}). Register a location with \
             `tapectl location add <name>`, or fix the name.",
            unit.name,
            from.what,
            unknown.join(", "),
        ),
    })
}

/// A volume's resolved `verify_interval_days` (ADR-0012 2026-10-07 item 1):
/// the tightest interval among the units written to it, or `None` when no
/// unit has been written to it at all (every unit has an interval since
/// ADR-0012 item 75: its archive set's, else `[defaults]`'s, 365 days when
/// the key is absent).
///
/// The interval is a unit's policy and a volume holds many units, so the
/// volume answers to the strictest of them. Each unit's interval resolves as
/// [`resolve`] resolves it (issue #453): its archive set's, else
/// `default_days` (`[defaults] verify_interval_days`, which the caller reads
/// from its `Config`); a dotfile has no such key. It reads `archive_sets`
/// directly rather than resolving every unit, which would read every unit's
/// dotfile and fail a readback on a bad one. A unit with no archive set, or
/// one whose set carries no interval, takes the default, so a home whose
/// interval lives only in `[defaults]` bounds a continued readback as `audit`
/// judges overdue. Every `writes` row counts, whatever its status (a write's
/// own confirm runs while its rows are still `in_progress`), except a `cut`
/// one (#420): the early end of the tape cut that unit, so the volume holds
/// no copy of it and answers to none of its policy. A sealed volume holding
/// only cut writes has no interval.
pub fn volume_verify_interval_days(
    conn: &Connection,
    volume_id: i64,
    default_days: Option<i64>,
) -> Result<Option<i64>> {
    Ok(conn.query_row(
        "SELECT MIN(COALESCE(a.verify_interval_days, ?2))
           FROM writes w
           JOIN snapshots s ON s.id = w.snapshot_id
           JOIN units u ON u.id = s.unit_id
           LEFT JOIN archive_sets a ON a.id = u.archive_set_id
          WHERE w.volume_id = ?1 AND w.status <> 'cut'",
        params![volume_id, default_days],
        |r| r.get(0),
    )?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::models::Unit;
    use tempfile::TempDir;

    fn fresh_conn() -> Connection {
        // Full ordered migration chain (issue #44) — was a hand-applied
        // 001-only snapshot.
        crate::db::open_memory().unwrap()
    }

    fn make_unit(archive_set_id: Option<i64>, path: Option<String>) -> Unit {
        Unit {
            id: 1,
            uuid: "u".into(),
            name: "unit".into(),
            tenant_id: 1,
            archive_set_id,
            current_path: path,
            checksum_mode: "mtime_size".into(),
            encrypt: true,
            status: "active".into(),
            created_at: "2026-01-01".into(),
            last_scanned: None,
            notes: None,
            copies_at_frozen: None,
        }
    }

    #[test]
    fn resolve_defaults_when_no_archive_set_no_dotfile() {
        let conn = fresh_conn();
        let config = Config::default();
        let unit = make_unit(None, None);

        let p = resolve(&conn, &config, &unit).unwrap();
        assert_eq!(p.min_copies, 2);
        assert_eq!(p.checksum_mode, "mtime_size");
        assert_eq!(p.compression, "none");
        assert!(p.encrypt);
        assert!(p.required_locations.is_empty());
        // 365 days with no archive set and no `[defaults]` key (ADR-0012 item 75).
        assert_eq!(p.verify_interval_days, Some(365));
    }

    /// #493 (ADR-0012 item 149): the copy floor's base is the larger of
    /// `min_copies` and the map's sum. With no map it is `min_copies`
    /// exactly; a map adding up to less leaves `min_copies` the floor; one
    /// adding up to more raises it. A 0 target adds nothing.
    #[test]
    fn the_copy_floor_base_is_the_larger_of_min_copies_and_the_maps_sum() {
        let conn = fresh_conn();
        let mut p = resolve(&conn, &Config::default(), &make_unit(None, None)).unwrap();
        assert!(p.copies_at.is_empty());
        assert_eq!(p.copy_floor_base(), 2, "no map: min_copies, as before");

        p.copies_at = BTreeMap::from([("home".to_string(), 1)]);
        assert_eq!(p.copy_floor_base(), 2, "a sum below min_copies");

        p.copies_at = BTreeMap::from([
            ("home".to_string(), 2),
            ("offsite".to_string(), 1),
            ("garage".to_string(), 0),
        ]);
        assert_eq!(p.copy_floor_base(), 3, "a sum above min_copies raises it");

        p.min_copies = 5;
        assert_eq!(p.copy_floor_base(), 5);
    }

    #[test]
    fn resolve_archive_set_overrides_defaults() {
        let conn = fresh_conn();
        conn.execute(
            "INSERT INTO archive_sets (name, min_copies, required_locations, compression, checksum_mode, verify_interval_days)
             VALUES ('media', 3, '[\"home\",\"offsite\"]', 'lzma', 'sha256', 90)",
            [],
        )
        .unwrap();
        let as_id = conn.last_insert_rowid();

        let config = Config::default();
        let unit = make_unit(Some(as_id), None);
        let p = resolve(&conn, &config, &unit).unwrap();

        assert_eq!(p.min_copies, 3);
        assert_eq!(p.compression, "lzma");
        assert_eq!(p.checksum_mode, "sha256");
        assert_eq!(p.required_locations, vec!["home", "offsite"]);
        assert_eq!(p.verify_interval_days, Some(90));
    }

    /// Issue #453 (ADR-0012 2026-10-15 later, item 66; 2026-10-16 item 75):
    /// `verify_interval_days` resolves archive set, then `[defaults]`, which is
    /// 365 days when the key is absent. A unit with no archive set, or one
    /// whose set carries no interval, gets the defaults value; a set's own
    /// interval wins over it, whether the defaults value is the code default
    /// or an explicit one.
    #[test]
    fn resolve_verify_interval_is_archive_set_then_defaults() {
        let conn = fresh_conn();
        conn.execute(
            "INSERT INTO archive_sets (name, verify_interval_days) VALUES ('tight', 30)",
            [],
        )
        .unwrap();
        let tight = conn.last_insert_rowid();
        conn.execute(
            "INSERT INTO archive_sets (name, min_copies) VALUES ('silent', 3)",
            [],
        )
        .unwrap();
        let silent = conn.last_insert_rowid();

        let mut config = Config::default();
        // No `[defaults]` key: the code default, 365, below a set's own.
        for set in [None, Some(silent), Some(tight)] {
            let p = resolve(&conn, &config, &make_unit(set, None)).unwrap();
            let want = if set == Some(tight) {
                Some(30)
            } else {
                Some(365)
            };
            assert_eq!(p.verify_interval_days, want, "code default, set {set:?}");
        }

        config.defaults.verify_interval_days = 180;
        // An explicit [defaults] value applies where the set has none (or
        // there is no set)...
        for set in [None, Some(silent)] {
            let p = resolve(&conn, &config, &make_unit(set, None)).unwrap();
            assert_eq!(p.verify_interval_days, Some(180), "set {set:?}");
        }
        // ...and the set's own value wins over it, in either direction.
        let p = resolve(&conn, &config, &make_unit(Some(tight), None)).unwrap();
        assert_eq!(p.verify_interval_days, Some(30));
        config.defaults.verify_interval_days = 7;
        let p = resolve(&conn, &config, &make_unit(Some(tight), None)).unwrap();
        assert_eq!(p.verify_interval_days, Some(30), "a looser set still wins");
    }

    /// Issue #453: the dotfile still cannot set `verify_interval_days` — its
    /// `[policy]` table refuses the key by name (issue #211), with or
    /// without a `[defaults]` value to defer to.
    #[test]
    fn resolve_rejects_verify_interval_days_in_a_dotfile_policy_table() {
        let conn = fresh_conn();
        let mut config = Config::default();
        config.defaults.verify_interval_days = 365;
        let tmp = TempDir::new().unwrap();
        std::fs::write(
            tmp.path().join(".tapectl-unit.toml"),
            "[policy]\nverify_interval_days = 30\n",
        )
        .unwrap();
        let unit = make_unit(None, Some(tmp.path().to_str().unwrap().to_string()));
        let err = resolve(&conn, &config, &unit)
            .expect_err("a dotfile [policy] table must not carry verify_interval_days");
        let msg = err.to_string();
        assert!(msg.contains(".tapectl-unit.toml"), "{msg}");
    }

    #[test]
    fn resolve_archive_set_null_fields_inherit_defaults() {
        let conn = fresh_conn();
        // Only min_copies set; everything else NULL must fall through to defaults.
        conn.execute(
            "INSERT INTO archive_sets (name, min_copies) VALUES ('partial', 4)",
            [],
        )
        .unwrap();
        let as_id = conn.last_insert_rowid();

        let config = Config::default();
        let unit = make_unit(Some(as_id), None);
        let p = resolve(&conn, &config, &unit).unwrap();

        assert_eq!(p.min_copies, 4);
        assert_eq!(p.compression, "none"); // from defaults
        assert_eq!(p.checksum_mode, "mtime_size"); // from defaults
    }

    #[test]
    fn resolve_dotfile_overrides_archive_set() {
        let conn = fresh_conn();
        conn.execute(
            "INSERT INTO archive_sets (name, compression, checksum_mode)
             VALUES ('media', 'lzma', 'sha256')",
            [],
        )
        .unwrap();
        let as_id = conn.last_insert_rowid();

        let tmp = TempDir::new().unwrap();
        let unit_path = tmp.path().to_str().unwrap().to_string();
        std::fs::write(
            tmp.path().join(".tapectl-unit.toml"),
            r#"
[policy]
checksum_mode = "sha256_on_archive"
compression = "gzip"
slice_size = "500M"
"#,
        )
        .unwrap();

        let config = Config::default();
        let unit = make_unit(Some(as_id), Some(unit_path));
        let p = resolve(&conn, &config, &unit).unwrap();

        // Dotfile wins. `sha256_on_archive` is deliberately a DIFFERENT
        // valid value from the archive set's `sha256` (issue #213 closed-set
        // validation would now reject the old placeholder `"full_hash"`,
        // which was never a real checksum_mode to begin with).
        assert_eq!(p.checksum_mode, "sha256_on_archive");
        assert_eq!(p.compression, "gzip");
        assert_eq!(p.slice_size, 500 * 1024 * 1024);
    }

    /// ADR-0006 / issue #73: `warehouse_copies` resolves through the same
    /// three layers as every other knob, and each layer only speaks when
    /// it has actually been set.
    #[test]
    fn warehouse_copies_resolves_dotfile_over_archive_set_over_defaults() {
        let conn = fresh_conn();
        let config = Config::default();

        // Layer 3: system default, nothing else set.
        let p = resolve(&conn, &config, &make_unit(None, None)).unwrap();
        assert_eq!(p.warehouse_copies, 0, "default is opt-in, i.e. none");

        // Layer 2: archive set.
        conn.execute(
            "INSERT INTO archive_sets (name, warehouse_copies) VALUES ('core', 2)",
            [],
        )
        .unwrap();
        let as_id = conn.last_insert_rowid();
        let p = resolve(&conn, &config, &make_unit(Some(as_id), None)).unwrap();
        assert_eq!(p.warehouse_copies, 2);

        // A NULL archive-set column must defer, not read as 0.
        conn.execute(
            "INSERT INTO archive_sets (name, min_copies) VALUES ('silent', 3)",
            [],
        )
        .unwrap();
        let silent = conn.last_insert_rowid();
        let mut cfg = Config::default();
        cfg.defaults.warehouse_copies = 1;
        let p = resolve(&conn, &cfg, &make_unit(Some(silent), None)).unwrap();
        assert_eq!(
            p.warehouse_copies, 1,
            "NULL means defer to the next layer, never 0"
        );

        // Layer 1: the unit dotfile outranks the archive set.
        let tmp = TempDir::new().unwrap();
        std::fs::write(
            tmp.path().join(".tapectl-unit.toml"),
            "[policy]\nwarehouse_copies = 4\n",
        )
        .unwrap();
        let unit = make_unit(Some(as_id), Some(tmp.path().to_str().unwrap().to_string()));
        let p = resolve(&conn, &config, &unit).unwrap();
        assert_eq!(p.warehouse_copies, 4);
    }

    /// #478 (ADR-0012 item 82): `allow_spanning` resolves dotfile, then
    /// archive set, then `[defaults]`, off by default, and each layer speaks
    /// only when it was set: a NULL archive-set column defers, and so does
    /// a dotfile without the key.
    #[test]
    fn allow_spanning_resolves_dotfile_over_archive_set_over_defaults() {
        let conn = fresh_conn();
        let dotfile = |text: &str| {
            let tmp = TempDir::new().unwrap();
            std::fs::write(tmp.path().join(".tapectl-unit.toml"), text).unwrap();
            tmp
        };
        let resolved = |config: &Config, set: Option<i64>, dir: Option<&TempDir>| {
            let path = dir.map(|d| d.path().to_str().unwrap().to_string());
            resolve(&conn, config, &make_unit(set, path))
                .unwrap()
                .allow_spanning
        };
        conn.execute(
            "INSERT INTO archive_sets (name, allow_spanning) VALUES ('whole', 0)",
            [],
        )
        .unwrap();
        let whole = conn.last_insert_rowid();
        conn.execute(
            "INSERT INTO archive_sets (name, allow_spanning) VALUES ('span', 1)",
            [],
        )
        .unwrap();
        let span = conn.last_insert_rowid();
        conn.execute(
            "INSERT INTO archive_sets (name, min_copies) VALUES ('silent', 3)",
            [],
        )
        .unwrap();
        let silent = conn.last_insert_rowid();
        let off = Config::default();
        let mut on = Config::default();
        on.defaults.allow_spanning = true;

        // Off when nothing sets it.
        assert!(!resolved(&off, None, None), "default off");
        assert!(
            !resolved(&off, Some(silent), None),
            "a NULL column is not on"
        );
        // `[defaults]` alone, and through a set that does not say.
        assert!(resolved(&on, None, None), "defaults alone");
        assert!(
            resolved(&on, Some(silent), None),
            "a NULL archive-set column defers to [defaults], never reads as off"
        );
        // The archive set over `[defaults]`, either way.
        assert!(resolved(&off, Some(span), None), "set on over defaults off");
        assert!(
            !resolved(&on, Some(whole), None),
            "set off over defaults on"
        );
        // The dotfile over the archive set over `[defaults]`, either way.
        let span_here = dotfile("[policy]\nallow_spanning = true\n");
        let whole_here = dotfile("[policy]\nallow_spanning = false\n");
        let says_nothing = dotfile("[policy]\nslice_size = \"500M\"\n");
        assert!(
            resolved(&off, Some(whole), Some(&span_here)),
            "dotfile true over set false over defaults false"
        );
        assert!(
            !resolved(&on, Some(span), Some(&whole_here)),
            "dotfile false over set true over defaults true"
        );
        assert!(
            resolved(&off, Some(span), Some(&says_nothing)),
            "a dotfile without the key defers to the set"
        );
        assert!(
            resolved(&on, Some(silent), Some(&says_nothing)),
            "...and through a silent set to [defaults]"
        );
    }

    /// #478: a misspelled spanning key in a dotfile makes the unit's policy
    /// unresolvable, naming the key, rather than reading as "not set"
    /// (issue #211's rule).
    #[test]
    fn resolve_refuses_a_misspelled_allow_spanning_in_a_dotfile() {
        let conn = fresh_conn();
        let config = Config::default();
        for bad in ["allow_span", "allowSpanning"] {
            let tmp = TempDir::new().unwrap();
            std::fs::write(
                tmp.path().join(".tapectl-unit.toml"),
                format!("[policy]\n{bad} = true\n"),
            )
            .unwrap();
            let unit = make_unit(None, Some(tmp.path().to_str().unwrap().to_string()));
            let msg = resolve(&conn, &config, &unit).unwrap_err().to_string();
            assert!(msg.contains(bad), "{bad}: {msg}");
            assert!(msg.contains(".tapectl-unit.toml"), "{bad}: {msg}");
        }
    }

    /// A map of location to count, for the `copies_at` tests.
    fn map(pairs: &[(&str, i64)]) -> BTreeMap<String, i64> {
        pairs.iter().map(|(n, c)| (n.to_string(), *c)).collect()
    }

    /// A unit directory whose dotfile holds `text`.
    fn dotfile_dir(text: &str) -> TempDir {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join(".tapectl-unit.toml"), text).unwrap();
        tmp
    }

    fn path_of(dir: &TempDir) -> Option<String> {
        Some(dir.path().to_str().unwrap().to_string())
    }

    /// A catalog with the locations `home`, `offsite` and `vault`.
    fn conn_with_locations() -> Connection {
        let conn = fresh_conn();
        conn.execute_batch(
            "INSERT INTO locations (name) VALUES ('home');
             INSERT INTO locations (name) VALUES ('offsite');
             INSERT INTO locations (name) VALUES ('vault');",
        )
        .unwrap();
        conn
    }

    fn archive_set(
        conn: &Connection,
        name: &str,
        required: Option<&str>,
        copies_at: Option<&str>,
    ) -> i64 {
        conn.execute(
            "INSERT INTO archive_sets (name, required_locations, copies_at) VALUES (?1, ?2, ?3)",
            params![name, required, copies_at],
        )
        .unwrap();
        conn.last_insert_rowid()
    }

    /// #493 (ADR-0012 items 149, 155, 156): `copies_at` resolves at every
    /// layer (`[defaults]`, the archive set, the dotfile, in its sub-table and
    /// inline spellings), and the most specific layer that sets it wins
    /// ENTIRELY: maps never merge, an empty map is a setting, and a layer
    /// that does not say (a NULL column, a dotfile without the key) defers.
    #[test]
    fn copies_at_resolves_at_every_layer_and_the_most_specific_wins_entirely() {
        let conn = conn_with_locations();
        let mut config = Config::default();
        config.defaults.copies_at = map(&[("home", 1), ("vault", 1)]);
        let set = archive_set(&conn, "media", None, Some(r#"{"home":2,"offsite":1}"#));
        let silent = archive_set(&conn, "silent", None, None);
        let empty = archive_set(&conn, "empty", None, Some("{}"));
        let resolved = |set: Option<i64>, dir: Option<&TempDir>| {
            resolve(&conn, &config, &make_unit(set, dir.and_then(path_of)))
                .unwrap()
                .copies_at
        };

        assert_eq!(
            resolved(None, None),
            map(&[("home", 1), ("vault", 1)]),
            "defaults"
        );
        assert_eq!(
            resolved(Some(silent), None),
            map(&[("home", 1), ("vault", 1)]),
            "a NULL archive-set column defers to [defaults]"
        );
        assert_eq!(
            resolved(Some(set), None),
            map(&[("home", 2), ("offsite", 1)]),
            "the set over [defaults], whole: no vault from below"
        );
        assert_eq!(
            resolved(Some(empty), None),
            map(&[]),
            "an empty map is a setting, and hides [defaults]"
        );

        let sub_table = dotfile_dir("[policy.copies_at]\noffsite = 3\n");
        let inline = dotfile_dir("[policy]\ncopies_at = { offsite = 3 }\n");
        for dir in [&sub_table, &inline] {
            assert_eq!(
                resolved(Some(set), Some(dir)),
                map(&[("offsite", 3)]),
                "the dotfile over the set, whole: no home from below"
            );
        }
        let says_nothing = dotfile_dir("[policy]\nslice_size = \"500M\"\n");
        assert_eq!(
            resolved(Some(set), Some(&says_nothing)),
            map(&[("home", 2), ("offsite", 1)]),
            "a dotfile without the key defers to the set"
        );
        assert_eq!(
            resolved(Some(silent), Some(&says_nothing)),
            map(&[("home", 1), ("vault", 1)]),
            "...and through a silent set to [defaults]"
        );
        let none_here = dotfile_dir("[policy.copies_at]\n");
        assert_eq!(
            resolved(Some(set), Some(&none_here)),
            map(&[]),
            "an empty dotfile map hides every layer below"
        );
    }

    /// #493 (item 156): an archive set's `required_locations` counts as
    /// `{name = 1}` at the set's layer, with its own `copies_at` laid over it
    /// name by name. A set that only requires locations still sets the map
    /// (hiding `[defaults]`); one whose list is empty and whose `copies_at` is
    /// NULL does not. `required_locations` itself resolves as before.
    #[test]
    fn required_locations_folds_into_the_archive_set_layers_map() {
        let conn = conn_with_locations();
        let mut config = Config::default();
        config.defaults.copies_at = map(&[("vault", 4)]);
        let required = archive_set(&conn, "required", Some(r#"["home","offsite"]"#), None);
        let both = archive_set(
            &conn,
            "both",
            Some(r#"["home","offsite"]"#),
            Some(r#"{"home":2,"vault":1}"#),
        );
        let empty_list = archive_set(&conn, "empty-list", Some("[]"), None);
        let resolved = |set: i64| resolve(&conn, &config, &make_unit(Some(set), None)).unwrap();

        let p = resolved(required);
        assert_eq!(p.copies_at, map(&[("home", 1), ("offsite", 1)]));
        assert_eq!(p.required_locations, vec!["home", "offsite"], "unchanged");
        assert_eq!(
            resolved(both).copies_at,
            map(&[("home", 2), ("offsite", 1), ("vault", 1)]),
            "the set's copies_at over its required_locations, name by name"
        );
        assert_eq!(
            resolved(empty_list).copies_at,
            map(&[("vault", 4)]),
            "an empty list names no location, so [defaults] stands"
        );

        // The dotfile over a set's folded map, whole.
        let here = dotfile_dir("[policy.copies_at]\nvault = 1\n");
        let p = resolve(&conn, &config, &make_unit(Some(required), path_of(&here))).unwrap();
        assert_eq!(p.copies_at, map(&[("vault", 1)]));
        assert_eq!(p.required_locations, vec!["home", "offsite"], "unchanged");
    }

    /// #493 (item 155): a `copies_at` name with no registered location makes
    /// the unit's policy unresolvable, naming the name and the layer, at
    /// every layer. Names an archive set's `required_locations` brought are
    /// checked where they are written (#348), not here, as before.
    #[test]
    fn an_unknown_location_in_copies_at_is_refused_naming_it_and_the_layer() {
        let conn = conn_with_locations();
        let refused = |config: &Config, unit: &Unit| match resolve(&conn, config, unit) {
            Err(TapectlError::PolicyUnresolvable { layer, detail }) => (layer, detail),
            other => panic!("expected PolicyUnresolvable, got {other:?}"),
        };

        let mut config = Config::default();
        config.defaults.copies_at = map(&[("home", 1), ("atlantis", 1)]);
        let (layer, detail) = refused(&config, &make_unit(None, None));
        assert_eq!(layer, PolicyLayer::Defaults);
        assert!(detail.contains("\"atlantis\""), "{detail}");
        assert!(detail.contains("[defaults] copies_at"), "{detail}");
        assert!(detail.contains("tapectl location add"), "{detail}");
        assert!(
            !detail.contains("\"home\""),
            "only the unknown name: {detail}"
        );

        let config = Config::default();
        let set = archive_set(&conn, "media", None, Some(r#"{"lemuria":1}"#));
        let (layer, detail) = refused(&config, &make_unit(Some(set), None));
        assert_eq!(layer, PolicyLayer::ArchiveSet);
        assert!(detail.contains("\"lemuria\""), "{detail}");
        assert!(detail.contains("archive set \"media\""), "{detail}");

        let dir = dotfile_dir("[policy.copies_at]\nhome = 1\nmu = 1\nhy-brasil = 2\n");
        let (layer, detail) = refused(&config, &make_unit(None, path_of(&dir)));
        assert_eq!(layer, PolicyLayer::Dotfile);
        assert!(detail.contains("\"hy-brasil\", \"mu\""), "{detail}");
        assert!(
            detail.contains("which are not registered locations"),
            "{detail}"
        );
        assert!(detail.contains(".tapectl-unit.toml"), "{detail}");

        let mut gone = make_unit(None, None);
        gone.copies_at_frozen = Some(r#"{"thule":1}"#.into());
        let (layer, detail) = refused(&config, &gone);
        assert_eq!(layer, PolicyLayer::Dotfile);
        assert!(detail.contains("\"thule\""), "{detail}");
        assert!(detail.contains("frozen"), "{detail}");

        // The positive control and the "as before" half: a registered name
        // resolves, and a `required_locations` name is not checked here.
        let fine = archive_set(&conn, "fine", Some(r#"["nowhere"]"#), Some(r#"{"home":1}"#));
        let p = resolve(&conn, &config, &make_unit(Some(fine), None)).unwrap();
        assert_eq!(p.copies_at, map(&[("home", 1), ("nowhere", 1)]));
    }

    /// #493 (item 148, the coordinator's freeze mechanics): the frozen
    /// `units.copies_at_frozen` is the dotfile layer only while the dotfile
    /// is ABSENT (no file in the unit's folder, the folder gone, or no path
    /// at all). A present dotfile always wins over it: with the key, its own
    /// map; without the key, the layer is silent and the frozen value is not
    /// used.
    #[test]
    fn the_frozen_copies_at_is_the_dotfile_layer_only_while_the_dotfile_is_absent() {
        let conn = conn_with_locations();
        let config = Config::default();
        let set = archive_set(&conn, "media", None, Some(r#"{"home":2}"#));
        let resolved = |path: Option<String>| {
            let mut unit = make_unit(Some(set), path);
            unit.copies_at_frozen = Some(r#"{"offsite":1,"vault":1}"#.into());
            resolve(&conn, &config, &unit).unwrap().copies_at
        };
        let frozen = map(&[("offsite", 1), ("vault", 1)]);

        let no_dotfile = TempDir::new().unwrap();
        assert_eq!(
            resolved(path_of(&no_dotfile)),
            frozen,
            "no dotfile in the folder"
        );
        assert_eq!(
            resolved(Some("/nonexistent/tapectl-493".into())),
            frozen,
            "the folder is gone (a tape-only unit)"
        );
        assert_eq!(resolved(None), frozen, "no path at all");

        let says_nothing = dotfile_dir("[policy]\nslice_size = \"500M\"\n");
        assert_eq!(
            resolved(path_of(&says_nothing)),
            map(&[("home", 2)]),
            "a present dotfile without the key is silent: the set's, not the frozen value"
        );
        let says_home = dotfile_dir("[policy.copies_at]\nhome = 1\n");
        assert_eq!(resolved(path_of(&says_home)), map(&[("home", 1)]));

        // NULL frozen with the dotfile absent: the lower layers decide.
        let unit = make_unit(Some(set), path_of(&no_dotfile));
        assert_eq!(
            resolve(&conn, &config, &unit).unwrap().copies_at,
            map(&[("home", 2)])
        );
    }

    /// #493: a negative count (in the dotfile, frozen, or an archive set's
    /// column) and an unparseable stored map are refused by name, never read
    /// as "no targets".
    #[test]
    fn a_bad_copies_at_count_or_stored_map_is_refused_by_layer() {
        let conn = conn_with_locations();
        let config = Config::default();
        let refused = |unit: &Unit| match resolve(&conn, &config, unit) {
            Err(TapectlError::PolicyUnresolvable { layer, detail }) => (layer, detail),
            other => panic!("expected PolicyUnresolvable, got {other:?}"),
        };

        let dir = dotfile_dir("[policy.copies_at]\nhome = -1\n");
        let (layer, detail) = refused(&make_unit(None, path_of(&dir)));
        assert_eq!(layer, PolicyLayer::Dotfile);
        assert!(detail.contains("\"home\" = -1 must be >= 0"), "{detail}");

        let mut gone = make_unit(None, None);
        gone.copies_at_frozen = Some("[not a map".into());
        let (layer, detail) = refused(&gone);
        assert_eq!(layer, PolicyLayer::Dotfile);
        assert!(detail.contains("copies_at_frozen"), "{detail}");

        gone.copies_at_frozen = Some(r#"{"home":-3}"#.into());
        let (layer, detail) = refused(&gone);
        assert_eq!(layer, PolicyLayer::Dotfile);
        assert!(detail.contains("\"home\" = -3"), "{detail}");

        let corrupt = archive_set(&conn, "corrupt", None, Some(r#"["home"]"#));
        let (layer, detail) = refused(&make_unit(Some(corrupt), None));
        assert_eq!(layer, PolicyLayer::ArchiveSet);
        assert!(detail.contains("unparseable copies_at"), "{detail}");
        assert!(detail.contains("\"corrupt\""), "{detail}");

        let negative = archive_set(&conn, "negative", None, Some(r#"{"home":-1}"#));
        let (layer, detail) = refused(&make_unit(Some(negative), None));
        assert_eq!(layer, PolicyLayer::ArchiveSet);
        assert!(detail.contains("\"home\" = -1"), "{detail}");
    }

    /// #493: with no `copies_at` at any layer and no frozen value, a unit's
    /// policy resolves exactly as before: an empty map, every other field as
    /// it was, and `required_locations` folded only into the new map.
    #[test]
    fn with_no_copies_at_anywhere_a_unit_resolves_exactly_as_before() {
        let conn = conn_with_locations();
        let config = Config::default();
        let plain = archive_set(&conn, "plain", None, None);
        let dir = dotfile_dir("[policy]\ncompression = \"gzip\"\n");
        for (set, path) in [
            (None, None),
            (Some(plain), None),
            (Some(plain), path_of(&dir)),
            (None, path_of(&dir)),
        ] {
            let p = resolve(&conn, &config, &make_unit(set, path.clone())).unwrap();
            assert!(p.copies_at.is_empty(), "{set:?} {path:?}");
            assert_eq!(p.min_copies, 2);
            assert!(p.required_locations.is_empty());
        }
    }

    /// Issue #105. This test previously asserted that a dangling
    /// `archive_set_id` **falls back to defaults**, which is the exact
    /// silent downgrade #105 exists to remove. Its own comment shows the
    /// real intent was panic-safety ("resolver must not panic") — an `Err`
    /// satisfies that just as well as an `Ok`, so the assertion is
    /// tightened rather than the behavior loosened: a unit pointed at an
    /// archive set that is not there must be REPORTED, not quietly handed
    /// the weaker system defaults it was deliberately moved off.
    #[test]
    fn resolve_dangling_archive_set_id_errors_instead_of_downgrading() {
        let conn = fresh_conn();
        let config = Config::default();
        let unit = make_unit(Some(999), None);

        let err = resolve(&conn, &config, &unit)
            .expect_err("a dangling archive_set_id must not resolve to system defaults");
        let msg = err.to_string();
        assert!(
            msg.contains("999") && msg.contains("archive_set_id"),
            "the error must name the dangling id so an operator can act on it, \
             not propagate rusqlite's bare \"Query returned no rows\"; got: {msg}"
        );
    }

    /// The absent-vs-present split, which is the one thing #105 must NOT
    /// change: a unit with no dotfile defers upward in silence (#92). If
    /// this ever starts erroring, every unit without a dotfile fails.
    #[test]
    fn resolve_with_no_dotfile_present_is_silent_not_an_error() {
        let conn = fresh_conn();
        let config = Config::default();
        let tmp = TempDir::new().unwrap();
        let unit = make_unit(None, Some(tmp.path().to_str().unwrap().to_string()));

        let p = resolve(&conn, &config, &unit).expect("an absent dotfile defers upward");
        assert_eq!(p.min_copies, config.defaults.min_copies as i64);
    }

    /// A dotfile that is PRESENT but not valid TOML is corruption, and the
    /// resolver must say so rather than handing back system defaults.
    #[test]
    fn resolve_with_a_malformed_dotfile_errors() {
        let conn = fresh_conn();
        let config = Config::default();
        let tmp = TempDir::new().unwrap();
        std::fs::write(
            tmp.path().join(".tapectl-unit.toml"),
            "[policy\nthis is not valid toml = = =\n",
        )
        .unwrap();
        let unit = make_unit(None, Some(tmp.path().to_str().unwrap().to_string()));

        let err = resolve(&conn, &config, &unit)
            .expect_err("a malformed dotfile must not resolve to system defaults");
        let msg = err.to_string();
        assert!(
            msg.contains(".tapectl-unit.toml"),
            "the error must name the file the operator has to fix; got: {msg}"
        );
    }

    /// A corrupt `required_locations` used to yield an EMPTY vec — i.e. "no
    /// locations required" — the same silent downgrade in a third place
    /// (not named by issue #105). Every writer stores a `serde_json`
    /// array or NULL, so an unparseable non-NULL value is only ever
    /// corruption.
    #[test]
    fn resolve_with_corrupt_required_locations_errors_instead_of_requiring_none() {
        let conn = fresh_conn();
        let config = Config::default();
        conn.execute(
            "INSERT INTO archive_sets (name, required_locations) VALUES ('broken', ?1)",
            params!["{not json at all"],
        )
        .unwrap();
        let as_id = conn.last_insert_rowid();
        let unit = make_unit(Some(as_id), None);

        let err = resolve(&conn, &config, &unit).expect_err(
            "corrupt required_locations must not silently resolve to \"none required\"",
        );
        assert!(err.to_string().contains("required_locations"), "got: {err}");
    }

    /// Issue #211: `PolicySection`'s `#[serde(deny_unknown_fields)]` (via
    /// `policy::resolve`'s dotfile layer) must refuse a misspelled or
    /// nonexistent `[policy]` key by name -- not silently ignore it and
    /// defer upward to the archive set/defaults as though the operator had
    /// never written it.
    #[test]
    fn resolve_rejects_an_unknown_dotfile_policy_key() {
        let conn = fresh_conn();
        let config = Config::default();
        let tmp = TempDir::new().unwrap();
        std::fs::write(
            tmp.path().join(".tapectl-unit.toml"),
            "[policy]\nslize_size = \"500M\"\n",
        )
        .unwrap();
        let unit = make_unit(None, Some(tmp.path().to_str().unwrap().to_string()));

        let err = resolve(&conn, &config, &unit).expect_err(
            "a misspelled [policy] key must not silently defer to the archive set/defaults \
             (issue #211) -- it must be refused by name",
        );
        let msg = err.to_string();
        assert!(
            msg.contains(".tapectl-unit.toml"),
            "the error must name the file the operator has to fix; got: {msg}"
        );
    }

    /// Negative control / regression guard for issue #263 route 1: a
    /// misspelled TOP-LEVEL table name (`[polcy]` instead of `[policy]`)
    /// means `toml.get("policy")` below returns `None`, so this whole
    /// layer silently defers upward as though the operator never wrote a
    /// `[policy]` section at all — the exact same silent-downgrade shape
    /// issue #211 already closed for a misspelled KEY *inside* a
    /// correctly-named `[policy]` table, just one level up. A one-letter
    /// slip here today reverts the unit to the default slice size, and
    /// that "moves real on-tape slice boundaries for every unit"
    /// (`resolve_slice_size_string`'s own doc comment).
    #[test]
    fn resolve_rejects_a_misspelled_top_level_table_name() {
        let conn = fresh_conn();
        let config = Config::default();
        let tmp = TempDir::new().unwrap();
        std::fs::write(
            tmp.path().join(".tapectl-unit.toml"),
            "[polcy]\nslice_size = \"500M\"\n",
        )
        .unwrap();
        let unit = make_unit(None, Some(tmp.path().to_str().unwrap().to_string()));

        resolve(&conn, &config, &unit).expect_err(
            "a misspelled top-level table name must not silently defer upward as though \
             the operator never wrote a [policy] section (issue #263)",
        );
    }

    /// Issue #211, the `min_copies` variant named explicitly in the
    /// evidence: a real archive_set-level field, plausible to copy into a
    /// unit dotfile by mistake, is not a legal `[policy]` key there either
    /// and must be refused the same way.
    #[test]
    fn resolve_rejects_min_copies_in_a_dotfile_policy_table() {
        let conn = fresh_conn();
        let config = Config::default();
        let tmp = TempDir::new().unwrap();
        std::fs::write(
            tmp.path().join(".tapectl-unit.toml"),
            "[policy]\nmin_copies = 5\n",
        )
        .unwrap();
        let unit = make_unit(None, Some(tmp.path().to_str().unwrap().to_string()));

        resolve(&conn, &config, &unit).expect_err(
            "min_copies is an archive_set field, not a unit dotfile [policy] key -- it \
             must be refused, not silently ignored (issue #211)",
        );
    }

    /// The rule #211 must NOT break (issue #92): a dotfile can be PRESENT
    /// with a `[policy]` table that sets some keys and leaves others
    /// completely absent. An absent key is a legitimate choice not to
    /// override anything, and must keep deferring upward in silence --
    /// unknown-key rejection must never be confused with "this field was
    /// never set." This is the regression guard that matters most: if it
    /// starts failing, the fix has resurrected the #92 bug one field at a
    /// time.
    #[test]
    fn resolve_leaves_absent_dotfile_policy_keys_deferring_upward() {
        let conn = fresh_conn();
        conn.execute(
            "INSERT INTO archive_sets (name, compression, checksum_mode, warehouse_copies) \
             VALUES ('media', 'lzma', 'sha256', 3)",
            [],
        )
        .unwrap();
        let as_id = conn.last_insert_rowid();

        let tmp = TempDir::new().unwrap();
        // Sets ONLY compression. checksum_mode, slice_size and
        // warehouse_copies are all absent from this table and must defer to
        // the archive set, not error and not silently fill a default.
        std::fs::write(
            tmp.path().join(".tapectl-unit.toml"),
            "[policy]\ncompression = \"gzip\"\n",
        )
        .unwrap();

        let config = Config::default();
        let unit = make_unit(Some(as_id), Some(tmp.path().to_str().unwrap().to_string()));
        let p = resolve(&conn, &config, &unit).expect(
            "a dotfile present with SOME [policy] keys set and others left absent must \
             still resolve -- an absent key is a legitimate silence, not an error (#92)",
        );

        assert_eq!(
            p.compression, "gzip",
            "the key that WAS set in the dotfile wins (dotfile > archive_set)"
        );
        assert_eq!(
            p.checksum_mode, "sha256",
            "the ABSENT checksum_mode key must defer to the archive set, not error and \
             not silently pick a filled-in default"
        );
        assert_eq!(
            p.warehouse_copies, 3,
            "the ABSENT warehouse_copies key must defer to the archive set too"
        );
    }

    /// Issue #213: an invalid `[policy] compression` must be refused at
    /// resolve time -- the same closed-set validation ADR-0012 already
    /// requires for `config.toml [defaults]` and `archive-set create/edit`
    /// -- rather than assigned straight through to surface hours later as a
    /// raw `dar -z` failure mid-`stage create`.
    #[test]
    fn resolve_rejects_an_invalid_dotfile_compression_value() {
        let conn = fresh_conn();
        let config = Config::default();
        let tmp = TempDir::new().unwrap();
        std::fs::write(
            tmp.path().join(".tapectl-unit.toml"),
            "[policy]\ncompression = \"banana\"\n",
        )
        .unwrap();
        let unit = make_unit(None, Some(tmp.path().to_str().unwrap().to_string()));

        let err = resolve(&conn, &config, &unit).expect_err(
            "an invalid [policy] compression must be refused at resolve time (issue #213), \
             not assigned straight through to surface later as a raw dar failure",
        );
        assert!(err.to_string().contains("compression"), "got: {err}");
    }

    /// Issue #213, the `checksum_mode` half: today this surfaces as an
    /// opaque SQLite CHECK-constraint rejection when the unit is finally
    /// written with it -- ADR-0012's other named motivating case.
    #[test]
    fn resolve_rejects_an_invalid_dotfile_checksum_mode_value() {
        let conn = fresh_conn();
        let config = Config::default();
        let tmp = TempDir::new().unwrap();
        std::fs::write(
            tmp.path().join(".tapectl-unit.toml"),
            "[policy]\nchecksum_mode = \"not_a_real_mode\"\n",
        )
        .unwrap();
        let unit = make_unit(None, Some(tmp.path().to_str().unwrap().to_string()));

        let err = resolve(&conn, &config, &unit).expect_err(
            "an invalid [policy] checksum_mode must be refused at resolve time (issue #213), \
             not assigned straight through to surface later as an opaque CHECK-constraint \
             failure",
        );
        assert!(err.to_string().contains("checksum_mode"), "got: {err}");
    }

    /// #420: a volume's verify interval is the strictest of the units it
    /// holds a copy of; a unit the early end of its tape cut is not one of
    /// them, and a volume holding only cut writes has no interval.
    #[test]
    fn a_cut_write_gives_its_volume_no_verify_interval() {
        use crate::volume::restore::short_tape;
        let t = short_tape::short_tape();
        // bravo (cut) asks for a far tighter interval than the default.
        t.conn
            .execute(
                "INSERT INTO archive_sets (name, verify_interval_days) VALUES ('tight', 7)",
                [],
            )
            .unwrap();
        t.conn
            .execute(
                "UPDATE units SET archive_set_id = last_insert_rowid() WHERE name = 'bravo'",
                [],
            )
            .unwrap();
        assert_eq!(
            volume_verify_interval_days(&t.conn, t.volume_id, Some(365)).unwrap(),
            Some(365),
            "alpha's, the unit the tape holds whole"
        );
        // The positive control: were bravo's write a copy, its 7 days would
        // bind the volume.
        t.conn
            .execute("UPDATE writes SET status = 'completed'", [])
            .unwrap();
        assert_eq!(
            volume_verify_interval_days(&t.conn, t.volume_id, Some(365)).unwrap(),
            Some(7)
        );

        let cut_only = short_tape::cut_only_tape();
        assert_eq!(
            volume_verify_interval_days(&cut_only.conn, cut_only.volume_id, Some(365)).unwrap(),
            None
        );
    }
}
