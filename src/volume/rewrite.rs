//! `tapectl volume rewrite` — compact a tape in place, on the same cartridge,
//! even when it holds the only copy (issue #483; ADR-0012 2026-10-19 item
//! 106).
//!
//! Compaction used to need a second cartridge: `compact-read` stages a
//! volume's live slices, `compact-write` writes them to an initialised
//! destination, `compact-finish` retires the source — and refuses outright
//! when a live slice has no copy on another volume. `volume rewrite SOURCE
//! --label NEW` reclaims the space on the cartridge it is on instead:
//!
//! 1. **Preflight, from the catalog alone** ([`preflight`]): the source is
//!    sealed and counts as a copy, NEW is a free label, the cartridge carries
//!    no other live volume, the drive can write the cartridge's generation,
//!    staging has room for the live slices, and the live slices fit one
//!    tape. The consent the run will need is decided here too, so a run that
//!    can never be consented to (no terminal, no flag) refuses before the
//!    hours-long read, not after it.
//! 2. **Read** the live slices into staging (`compact-read`'s own function)
//!    and **record the hold** ([`pending_rewrites`](crate::volume::rewrite)):
//!    from here nothing releases those staged slices.
//! 3. **Re-read every staged slice from the staging disk** and match its
//!    ciphertext hash. A mismatch refuses, with the source still intact.
//! 4. **Consent.** Every live slice has a copy on another eligible volume:
//!    the usual ADR-0008 Tier-2 gate for a version left below policy (a
//!    terminal asks; `--yes` confirms in advance; no terminal and no `--yes`
//!    refuses). Some live slice has no copy anywhere else: the retire
//!    family's absolute zero floor is amended HERE and for this one command —
//!    the source's label typed at a terminal, or `--only-copy-on-staging
//!    LABEL` for an unattended run ([`crate::cli::consent::confirm_only_copy`],
//!    which `--yes` and `--force` cannot reach: it has no argument for them).
//! 5. **Retire the source, erase its cartridge** (`cartridge erase`'s own
//!    path, with its proofs and refusals), **`volume init NEW`** on the same
//!    cartridge, **write** the staged slices with the FULL confirm, and only
//!    then **release** staging. The window from the erase until NEW has
//!    passed that readback is named in the events and the session log.
//!
//! Every other retire path keeps the absolute floor: `volume retire`,
//! `volume compact-finish`, `cartridge retire` and `cartridge mark-erased`
//! are unchanged and have tests that say so.
//!
//! # The hold
//!
//! A failure after the erase leaves the staged slices staged, possibly as the
//! only copy of a unit. Migration 037's `pending_rewrites` records the
//! rewrite and the stage sets it staged; [`holds`] is the one query every
//! release path asks (`staging::clean::clean_staging`, which `staging
//! clean`, `release_if_covered`, `volume write`/`volume resume` and
//! `collection run` all end in). A set is held while its rewrite is open
//! AND no eligible volume carries a fully read back copy of it
//! ([`coverage::fully_read_back_copy`]) — a fact, so the hold lifts by the
//! passing readback whichever command ran it, and a crash can leave it
//! neither stuck on nor lifted early. `volume rewrite --resume NEW` finishes
//! the rewrite from wherever the catalog says it stopped.
//!
//! # The seam
//!
//! The four steps that touch the drive are [`RewriteSteps`], as
//! `volume write --init`'s are `WriteSteps`: [`DriveRewriteSteps`] is the
//! drive, and a test's stand-in lets every ordering and every failure after
//! the erase be driven with no tape.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use rusqlite::{params, Connection, OptionalExtension};
use tracing::{info, warn};

use crate::config::Config;
use crate::db::events;
use crate::error::{Result, TapectlError};
use crate::policy::coverage;
use crate::progress;
use crate::volume::erase::EraseReport;
use crate::volume::write::{self, CompactReadReport, Initialized, StagedSlice};

// ── The record ──────────────────────────────────────────────────────────────

/// How far a rewrite got — `pending_rewrites.state` (migration 037).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum State {
    /// The slices are staged and nothing destructive has happened.
    Staged,
    /// The source is retired.
    Retired,
    /// The source's cartridge is erased: the window is open.
    Erased,
    /// The destination volume exists.
    Initialized,
    /// The destination is sealed and fully read back; only the release is
    /// left.
    Written,
}

impl State {
    pub fn as_str(self) -> &'static str {
        match self {
            State::Staged => "staged",
            State::Retired => "retired",
            State::Erased => "erased",
            State::Initialized => "initialized",
            State::Written => "written",
        }
    }

    fn parse(s: &str) -> Result<State> {
        Ok(match s {
            "staged" => State::Staged,
            "retired" => State::Retired,
            "erased" => State::Erased,
            "initialized" => State::Initialized,
            "written" => State::Written,
            other => {
                return Err(TapectlError::Other(format!(
                    "pending_rewrites holds a state \"{other}\" this build does not know"
                )))
            }
        })
    }
}

/// How the retirement was consented to — `pending_rewrites.consent`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Consent {
    /// Every live slice had a copy on another eligible volume: the usual
    /// ADR-0008 Tier-2 gate (or none, when no version fell below policy).
    CopiesElsewhere,
    /// Some live slice had no copy elsewhere; the source's label was typed.
    TypedLabel,
    /// The same, by `--only-copy-on-staging`.
    TypedFlag,
}

impl Consent {
    pub fn as_str(self) -> &'static str {
        match self {
            Consent::CopiesElsewhere => "copies_elsewhere",
            Consent::TypedLabel => "typed_label",
            Consent::TypedFlag => "typed_flag",
        }
    }

    fn parse(s: &str) -> Result<Consent> {
        Ok(match s {
            "copies_elsewhere" => Consent::CopiesElsewhere,
            "typed_label" => Consent::TypedLabel,
            "typed_flag" => Consent::TypedFlag,
            other => {
                return Err(TapectlError::Other(format!(
                    "pending_rewrites holds a consent \"{other}\" this build does not know"
                )))
            }
        })
    }
}

/// A pending rewrite as a release path sees it: why a stage set is not being
/// released, and what finishes the rewrite.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hold {
    pub rewrite_id: i64,
    /// The volume being rewritten.
    pub source: String,
    /// The label of the volume that is to carry the slices.
    pub destination: String,
    pub state: State,
}

impl Hold {
    /// The command that finishes the rewrite.
    pub fn resume_command(&self) -> String {
        format!("tapectl volume rewrite --resume {}", self.destination)
    }

    /// One line, for `staging clean` and the release after a write.
    pub fn describe(&self, stage_set_id: i64) -> String {
        format!(
            "stage set {stage_set_id} kept: it is held by the rewrite of volume \"{}\" (at \
             \"{}\", {}), which may leave this staged copy the only one until \"{}\" has \
             passed a full readback; `--force` does not apply. Finish the rewrite with `{}`",
            self.source,
            self.destination,
            self.state.as_str(),
            self.destination,
            self.resume_command()
        )
    }
}

/// Every stage set a pending rewrite holds right now, by stage set id.
///
/// A set is held while its rewrite is open (`finished_at IS NULL`) AND no
/// eligible volume carries a completed, fully read back copy of it
/// ([`coverage::fully_read_back_copy`]). One query, however many candidates
/// the caller has.
pub fn holds(conn: &Connection) -> Result<HashMap<i64, Hold>> {
    let sql = format!(
        "SELECT prs.stage_set_id, r.id, sv.label, r.destination_label, r.state
         FROM pending_rewrite_sets prs
         JOIN pending_rewrites r ON r.id = prs.rewrite_id
         JOIN volumes sv ON sv.id = r.source_volume_id
         WHERE r.finished_at IS NULL AND NOT {}",
        coverage::fully_read_back_copy("prs.stage_set_id")
    );
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt
        .query_map([], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, String>(4)?,
            ))
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    let mut out = HashMap::new();
    for (stage_set_id, rewrite_id, source, destination, state) in rows {
        out.insert(
            stage_set_id,
            Hold {
                rewrite_id,
                source,
                destination,
                state: State::parse(&state)?,
            },
        );
    }
    Ok(out)
}

/// The hold on one stage set, if any.
pub fn hold_on(conn: &Connection, stage_set_id: i64) -> Result<Option<Hold>> {
    Ok(holds(conn)?.remove(&stage_set_id))
}

/// An open rewrite, as the continuation needs it.
#[derive(Debug, Clone)]
struct Pending {
    id: i64,
    source_id: i64,
    source: String,
    destination: String,
    cartridge_serial: Option<String>,
    consent: Consent,
    state: State,
    stage_set_ids: Vec<i64>,
}

fn load_open(
    conn: &Connection,
    where_sql: &str,
    key: rusqlite::types::Value,
) -> Result<Option<Pending>> {
    let row = conn
        .query_row(
            &format!(
                "SELECT r.id, r.source_volume_id, sv.label, r.destination_label,
                        r.cartridge_serial, r.consent, r.state
                 FROM pending_rewrites r JOIN volumes sv ON sv.id = r.source_volume_id
                 WHERE r.finished_at IS NULL AND {where_sql}"
            ),
            params![key],
            |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, i64>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, String>(3)?,
                    r.get::<_, Option<String>>(4)?,
                    r.get::<_, String>(5)?,
                    r.get::<_, String>(6)?,
                ))
            },
        )
        .optional()?;
    let Some((id, source_id, source, destination, cartridge_serial, consent, state)) = row else {
        return Ok(None);
    };
    let stage_set_ids = conn
        .prepare(
            "SELECT stage_set_id FROM pending_rewrite_sets WHERE rewrite_id = ?1
             ORDER BY stage_set_id",
        )?
        .query_map(params![id], |r| r.get::<_, i64>(0))?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    Ok(Some(Pending {
        id,
        source_id,
        source,
        destination,
        cartridge_serial,
        consent: Consent::parse(&consent)?,
        state: State::parse(&state)?,
        stage_set_ids,
    }))
}

fn open_for_destination(conn: &Connection, label: &str) -> Result<Option<Pending>> {
    load_open(conn, "r.destination_label = ?1", label.to_string().into())
}

fn open_for_source(conn: &Connection, source_id: i64) -> Result<Option<Pending>> {
    load_open(conn, "r.source_volume_id = ?1", source_id.into())
}

fn insert_pending(
    conn: &Connection,
    source_id: i64,
    destination: &str,
    cartridge_serial: Option<&str>,
    consent: Consent,
    stage_set_ids: &[i64],
) -> Result<i64> {
    let tx = conn.unchecked_transaction()?;
    tx.execute(
        "INSERT INTO pending_rewrites
            (source_volume_id, destination_label, cartridge_serial, consent)
         VALUES (?1, ?2, ?3, ?4)",
        params![source_id, destination, cartridge_serial, consent.as_str()],
    )?;
    let id = tx.last_insert_rowid();
    for ss in stage_set_ids {
        tx.execute(
            "INSERT INTO pending_rewrite_sets (rewrite_id, stage_set_id) VALUES (?1, ?2)",
            params![id, ss],
        )?;
    }
    tx.commit()?;
    Ok(id)
}

fn set_state(conn: &Connection, id: i64, state: State) -> Result<()> {
    conn.execute(
        "UPDATE pending_rewrites SET state = ?2 WHERE id = ?1",
        params![id, state.as_str()],
    )?;
    Ok(())
}

fn close(conn: &Connection, id: i64, outcome: &str) -> Result<()> {
    conn.execute(
        "UPDATE pending_rewrites SET finished_at = datetime('now'), outcome = ?2 WHERE id = ?1",
        params![id, outcome],
    )?;
    Ok(())
}

// ── Records: events, the session log ────────────────────────────────────────

/// One step of the rewrite, recorded in the audit trail (`events`, on the
/// source volume) and in the session log, in the same words. `step` is the
/// event's action.
fn record_step(
    conn: &Connection,
    source_id: i64,
    source: &str,
    step: &str,
    details: serde_json::Value,
) -> Result<()> {
    let text = details.to_string();
    events::log_event(
        conn,
        "volume",
        source_id,
        Some(source),
        step,
        None,
        None,
        None,
        Some(&text),
        None,
    )?;
    progress::log(&format!("volume rewrite {source}: {step} {text}"));
    info!(volume = source, step, details = %text, "volume rewrite");
    Ok(())
}

/// The window, named: the sentence the consent shows, the events carry and
/// the session log opens it with.
pub fn window_sentence(staging_path: &Path, destination: &str) -> String {
    format!(
        "from the erase until \"{destination}\" seals and passes a full readback, the staging \
         disk at {} holds the only copy of these",
        staging_path.display()
    )
}

// ── Preflight ───────────────────────────────────────────────────────────────

/// What the user asked of one `volume rewrite`.
#[derive(Debug, Clone)]
pub struct RewriteOptions<'a> {
    pub source: &'a str,
    pub destination: &'a str,
    pub device: &'a str,
    /// `--only-copy-on-staging LABEL`: the typed consent, for an unattended
    /// run.
    pub only_copy_on_staging: Option<&'a str>,
    /// The Tier-2 waiver: the global `--yes` or `--force`. It never reaches
    /// the typed consent.
    pub assume_yes: bool,
    /// `--with-staged`: write whatever else is staged beside the rewrite's
    /// slices. Off, the new volume carries the rewrite's slices and nothing
    /// else.
    pub with_staged: bool,
    /// `--keep-staging`: release nothing after the full readback.
    pub keep_staging: bool,
}

/// What a rewrite needs the catalog to say before anything is read.
#[derive(Debug)]
pub(crate) struct Plan {
    pub source_id: i64,
    pub cartridge_barcode: String,
    pub cartridge_serial: Option<String>,
    pub slices: Vec<write::RecoverySlice>,
    pub live_bytes: i64,
    /// Bytes of reclaimable/purged versions on the source: what the rewrite
    /// frees on the cartridge.
    pub reclaimable_bytes: i64,
    /// Live slices with no copy on another eligible volume.
    pub sole: Vec<coverage::SoleSlice>,
    /// `(unit, version)` whose retirement would take the last eligible copy
    /// of a live version though every slice has a copy elsewhere (a copy the
    /// catalog does not count). Together with [`Self::sole`] these are what
    /// the typed consent names.
    pub last_copy_versions: Vec<(String, i64)>,
    /// The Tier-2 facts (empty: nothing falls below policy).
    pub tier2_facts: Vec<String>,
    pub compact_dir: PathBuf,
}

impl Plan {
    /// The typed consent is needed: some live data has no copy elsewhere.
    pub fn needs_typed_consent(&self) -> bool {
        !self.sole.is_empty() || !self.last_copy_versions.is_empty()
    }

    /// `(unit, version, slices, bytes)` for every version the typed consent
    /// names, ascending by unit then version.
    pub fn sole_versions(&self) -> Vec<(String, i64, usize, i64)> {
        let mut map: std::collections::BTreeMap<(String, i64), (usize, i64)> = Default::default();
        for s in &self.sole {
            let e = map.entry((s.unit_name.clone(), s.version)).or_default();
            e.0 += 1;
            e.1 += s.encrypted_bytes;
        }
        for (unit, version) in &self.last_copy_versions {
            map.entry((unit.clone(), *version)).or_default();
        }
        map.into_iter()
            .map(|((u, v), (n, b))| (u, v, n, b))
            .collect()
    }

    /// The consent facts the typed gate shows: the units and versions, the
    /// staging path and the window.
    pub fn typed_facts(&self, source: &str, destination: &str) -> Vec<String> {
        use crate::util::format_bytes_binary as fmt;
        let mut facts = vec![format!(
            "volume \"{source}\" holds the ONLY copy of these units; rewriting it in place \
             erases that copy before its replacement exists:"
        )];
        for (unit, version, slices, bytes) in self.sole_versions() {
            facts.push(if slices > 0 {
                format!(
                    "  unit \"{unit}\" v{version}: {slices} slice(s), {} with no copy on any \
                     other volume",
                    fmt(bytes)
                )
            } else {
                format!(
                    "  unit \"{unit}\" v{version}: the last copy the catalog counts for this \
                     version"
                )
            });
        }
        facts.push(format!(
            "Every slice is read from \"{source}\" into staging at {} and then read back from \
             that disk and matched against its recorded hash before anything is destroyed.",
            self.compact_dir.display()
        ));
        facts.push(format!(
            "THE WINDOW: {}. If the rewrite stops in that window the slices stay staged and \
             nothing will release them; `tapectl volume rewrite --resume {destination}` \
             finishes it.",
            window_sentence(&self.compact_dir, destination)
        ));
        facts
    }
}

/// The catalog-only checks of a rewrite, before the drive is touched. Every
/// refusal here leaves the tape, staging and the catalog unchanged.
pub(crate) fn preflight(
    conn: &Connection,
    config: &Config,
    opts: &RewriteOptions<'_>,
) -> Result<Plan> {
    let plan = plan(conn, config, opts)?;
    // A run that can never be consented to refuses now, before the read.
    check_consent_obtainable(&plan, opts)?;
    Ok(plan)
}

/// [`preflight`] without the consent check: what the catalog says about this
/// rewrite, for `--dry-run` to show and for [`preflight`] to refuse on.
pub(crate) fn plan(conn: &Connection, config: &Config, opts: &RewriteOptions<'_>) -> Result<Plan> {
    let source = opts.source;
    let destination = opts.destination;

    let (source_id, status, condition, media_type, capacity): (
        i64,
        String,
        String,
        Option<String>,
        i64,
    ) = conn
        .query_row(
            "SELECT id, status, observed_condition, media_type, capacity_bytes
             FROM volumes WHERE label = ?1",
            params![source],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
        )
        .map_err(|_| TapectlError::VolumeNotFound(source.to_string()))?;

    if destination.trim().is_empty() {
        return Err(TapectlError::Other(
            "volume rewrite needs --label NEW: the label of the volume that will carry the \
             slices on the same cartridge"
                .into(),
        ));
    }
    if destination == source {
        return Err(TapectlError::Other(format!(
            "volume rewrite refused: --label \"{destination}\" is the volume being rewritten. \
             The source keeps its row (retired, then erased), so the new volume needs a label \
             of its own. Nothing was changed."
        )));
    }
    let exists: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM volumes WHERE label = ?1)",
        params![destination],
        |r| r.get(0),
    )?;
    if exists {
        return Err(TapectlError::Other(format!(
            "volume rewrite refused: a volume labelled \"{destination}\" already exists. \
             `--label` names the NEW volume `volume init` will create on the cartridge. If a \
             rewrite onto it stopped part-way, `tapectl volume rewrite --resume \
             {destination}` finishes it. Nothing was changed."
        )));
    }

    if status != "sealed" || condition != "ok" {
        return Err(TapectlError::Other(format!(
            "volume rewrite refused: volume \"{source}\" is {status}{}, not a sealed volume \
             whose tape is trusted to read. Only a sealed, unquarantined volume is read back \
             into staging and rewritten; a retired or erased one has nothing to rewrite, and \
             a quarantined one should be read from another copy (`tapectl volume read-slices`). \
             Nothing was changed.",
            if condition == "ok" {
                String::new()
            } else {
                format!(" (condition {condition})")
            }
        )));
    }

    // An earlier rewrite of this volume that stopped before it retired
    // anything holds nothing but its staging: it is replaced. One that got
    // further is resumed, never restarted.
    if let Some(open) = open_for_source(conn, source_id)? {
        if open.state != State::Staged {
            return Err(TapectlError::Other(format!(
                "volume rewrite refused: a rewrite of \"{source}\" onto \"{}\" is already \
                 under way ({}). Finish it with `tapectl volume rewrite --resume {}`. Nothing \
                 was changed.",
                open.destination,
                open.state.as_str(),
                open.destination
            )));
        }
    }
    if let Some(open) = open_for_destination(conn, destination)? {
        if open.source_id != source_id {
            return Err(TapectlError::Other(format!(
                "volume rewrite refused: the label \"{destination}\" is already the \
                 destination of an unfinished rewrite of \"{}\". Nothing was changed.",
                open.source
            )));
        }
    }

    // The cartridge: the source's open mount, and nothing else live on it.
    let mount: Option<(i64, String, Option<String>)> = conn
        .query_row(
            "SELECT c.id, c.barcode, c.serial_number FROM cartridge_volumes cv
             JOIN cartridges c ON c.id = cv.cartridge_id
             WHERE cv.volume_id = ?1 AND cv.unmounted_at IS NULL",
            params![source_id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .optional()?;
    let Some((cartridge_id, cartridge_barcode, cartridge_serial)) = mount else {
        return Err(TapectlError::Other(format!(
            "volume rewrite refused: the catalog does not record which cartridge volume \
             \"{source}\" is on, and a rewrite reuses that cartridge (the erase is checked \
             against its chip serial). `tapectl volume identify` with the tape loaded shows \
             what it carries. Nothing was changed."
        )));
    };
    let others: Vec<String> = conn
        .prepare(&format!(
            "SELECT v.label FROM cartridge_volumes cv JOIN volumes v ON v.id = cv.volume_id
             WHERE cv.cartridge_id = ?1 AND cv.unmounted_at IS NULL AND cv.volume_id <> ?2
               AND {} ORDER BY v.label",
            coverage::in_service_or_provisioned("v")
        ))?
        .query_map(params![cartridge_id, source_id], |r| r.get::<_, String>(0))?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    if !others.is_empty() {
        return Err(TapectlError::Other(format!(
            "volume rewrite refused: cartridge {cartridge_barcode} also carries {}, which the \
             catalog still holds as live; erasing the cartridge would erase it too. Retire it \
             first if that is what you mean, or rewrite by `compact-read`/`compact-write` onto \
             another cartridge. Nothing was changed.",
            others
                .iter()
                .map(|l| format!("volume \"{l}\""))
                .collect::<Vec<_>>()
                .join(", ")
        )));
    }

    // The drive can write this cartridge's generation (ADR-0010; no flag
    // reaches it), and what is live fits one tape.
    let backend = crate::config::resolve_lto_backend(config, Some(opts.device))?;
    if let Some(generation) = media_type
        .as_deref()
        .and_then(crate::media::Generation::parse)
    {
        crate::tape::media_detect::check_drive_can_write(backend, generation)?;
    }

    let slices = write::live_recovery_slices(conn, source_id)?;
    if slices.is_empty() {
        return Err(TapectlError::Other(format!(
            "volume rewrite refused: volume \"{source}\" has no live slices — everything on \
             it belongs to reclaimable or purged versions, so there is nothing to write back. \
             `tapectl volume retire {source}` and then `tapectl cartridge erase` frees the \
             cartridge. Nothing was changed."
        )));
    }
    let live_bytes: i64 = slices.iter().map(|s| s.encrypted_bytes).sum();
    let budget_bytes = backend.fill_budget_bytes(capacity.max(0) as u64);
    if live_bytes as u64 > budget_bytes {
        return Err(TapectlError::Other(format!(
            "volume rewrite refused: the live data ({}) does not fit one tape at the {}% fill \
             ceiling (the cartridge's {} capacity gives {}). Nothing was changed.",
            crate::util::format_bytes_binary(live_bytes),
            (backend.fill_ceiling * 100.0).round() as i64,
            crate::util::format_bytes_decimal(capacity),
            crate::util::format_bytes_binary(budget_bytes as i64),
        )));
    }

    // `--with-staged` carries whatever else is staged beside the live
    // slices; it must fit the same tape, or the write after the erase would
    // refuse for capacity.
    if opts.with_staged {
        let live_sets: Vec<i64> = slices.iter().map(|s| s.stage_set_id).collect();
        let extra: i64 = {
            let mut stmt = conn.prepare(
                "SELECT ss.id, COALESCE(SUM(sl.encrypted_bytes), 0)
                 FROM stage_sets ss JOIN stage_slices sl ON sl.stage_set_id = ss.id
                 WHERE ss.status = 'staged' GROUP BY ss.id",
            )?;
            let rows = stmt
                .query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?)))?
                .collect::<std::result::Result<Vec<_>, _>>()?;
            rows.into_iter()
                .filter(|(id, _)| !live_sets.contains(id))
                .map(|(_, bytes)| bytes)
                .sum()
        };
        if (live_bytes + extra) as u64 > budget_bytes {
            return Err(TapectlError::Other(format!(
                "volume rewrite refused: --with-staged adds {} of other staged data to the {}                  of live data, which does not fit one tape at the {}% fill ceiling ({}).                  Nothing was changed.",
                crate::util::format_bytes_binary(extra),
                crate::util::format_bytes_binary(live_bytes),
                (backend.fill_ceiling * 100.0).round() as i64,
                crate::util::format_bytes_binary(budget_bytes as i64),
            )));
        }
    }

    // Staging has room for the live slices, before a block is read.
    let staging_dir = PathBuf::from(&config.staging.directory);
    let compact_dir = staging_dir.join(format!("compact-{source}"));
    write::check_recovery_read_space(
        &staging_dir,
        &compact_dir,
        &slices,
        &format!("the live slices of \"{source}\" for a rewrite"),
    )?;

    let reclaimable_bytes: i64 = conn.query_row(
        "SELECT COALESCE(SUM(sl.encrypted_bytes), 0)
         FROM writes w
         JOIN stage_sets sts ON sts.id = w.stage_set_id
         JOIN snapshots s ON s.id = sts.snapshot_id
         JOIN stage_slices sl ON sl.stage_set_id = sts.id
         WHERE w.volume_id = ?1 AND w.status = 'completed'
           AND s.status IN ('reclaimable', 'purged')",
        params![source_id],
        |r| r.get(0),
    )?;

    // The consent the run will need, decided now.
    let sole = coverage::sole_slices(conn, source_id)?;
    let impacts = crate::cli::operations::retire_impacts(conn, source_id)?;
    let mut last_copy_versions: Vec<(String, i64)> = Vec::new();
    for impact in &impacts {
        for v in &impact.at_stake {
            if v.copies_after == 0
                && !sole
                    .iter()
                    .any(|s| s.unit_name == impact.unit_name && s.version == v.version)
            {
                last_copy_versions.push((impact.unit_name.clone(), v.version));
            }
        }
    }
    let mut tier2_facts = Vec::new();
    if sole.is_empty() && last_copy_versions.is_empty() {
        tier2_facts = crate::cli::operations::below_policy_facts(conn, config, &impacts)?;
        if !tier2_facts.is_empty() {
            // ADR-0004 Tier 1: evidence age for the units that keep
            // coverage, shown where consent is asked and never gating.
            let now = chrono::Utc::now().naive_utc();
            for impact in &impacts {
                if impact.other_copies != 0 {
                    if let Some(line) =
                        crate::policy::evidence::describe(&impact.unit_name, &impact.evidence, now)
                    {
                        tier2_facts.push(line);
                    }
                }
            }
        }
    }

    Ok(Plan {
        source_id,
        cartridge_barcode,
        cartridge_serial,
        slices,
        live_bytes,
        reclaimable_bytes,
        sole,
        last_copy_versions,
        tier2_facts,
        compact_dir,
    })
}

impl Plan {
    /// What `--dry-run` shows: the plan and the consent it would need.
    pub(crate) fn describe(&self, source: &str, destination: &str) -> Vec<String> {
        use crate::util::format_bytes_binary as fmt;
        let mut lines = vec![
            format!(
                "volume rewrite \"{source}\" -> \"{destination}\" on cartridge {}{}",
                self.cartridge_barcode,
                self.cartridge_serial
                    .as_deref()
                    .map(|s| format!(" (chip serial {s})"))
                    .unwrap_or_default()
            ),
            format!(
                "  live data: {} slice(s), {}, read into staging at {}",
                self.slices.len(),
                fmt(self.live_bytes),
                self.compact_dir.display()
            ),
            format!(
                "  freed on the cartridge: {} (reclaimable and purged versions)",
                fmt(self.reclaimable_bytes)
            ),
        ];
        if self.needs_typed_consent() {
            lines.push(
                "  consent: some live data has NO copy on any other volume — the source's \
                 label typed, or --only-copy-on-staging, would be required (--yes does not \
                 give it):"
                    .to_string(),
            );
            lines.extend(self.typed_facts(source, destination));
        } else if self.tier2_facts.is_empty() {
            lines.push(
                "  consent: none needed — every live slice has a copy on another volume and \
                 no version falls below its policy"
                    .to_string(),
            );
        } else {
            lines.push("  consent: the usual Tier-2 prompt (or --yes):".to_string());
            lines.extend(self.tier2_facts.iter().map(|f| format!("    {f}")));
        }
        lines
    }

    pub(crate) fn to_json(&self, source: &str, destination: &str) -> serde_json::Value {
        serde_json::json!({
            "dry_run": true,
            "source": source,
            "destination": destination,
            "cartridge": self.cartridge_barcode,
            "cartridge_serial": self.cartridge_serial,
            "live_slices": self.slices.len(),
            "live_bytes": self.live_bytes,
            "reclaimable_bytes": self.reclaimable_bytes,
            "staging": self.compact_dir.display().to_string(),
            "consent": if self.needs_typed_consent() {
                "typed_label"
            } else if self.tier2_facts.is_empty() {
                "none"
            } else {
                "tier2"
            },
            "only_copy": self.sole_versions().into_iter().map(|(unit, version, slices, bytes)| {
                serde_json::json!({"unit": unit, "version": version, "slices": slices, "bytes": bytes})
            }).collect::<Vec<_>>(),
            "tier2_facts": self.tier2_facts,
        })
    }
}

/// The non-interactive half of the consent, asked before anything is read:
/// a `--only-copy-on-staging` that does not match, or no way to give the
/// typed consent at all, or a Tier-2 prompt with no terminal and no `--yes`.
/// The prompts themselves come after the staged slices are verified.
fn check_consent_obtainable(plan: &Plan, opts: &RewriteOptions<'_>) -> Result<()> {
    let tty = crate::cli::consent::stdin_is_terminal();
    let action = format!(
        "rewrite volume \"{}\" in place onto \"{}\"",
        opts.source, opts.destination
    );
    if plan.needs_typed_consent() {
        if let Some(word) = opts.only_copy_on_staging {
            if word != opts.source {
                // Same refusal, same words, as the gate itself.
                return crate::cli::consent::confirm_only_copy(
                    &action,
                    &[],
                    opts.source,
                    Some(word),
                );
            }
        } else if !tty {
            return crate::cli::consent::confirm_only_copy(
                &action,
                &plan.typed_facts(opts.source, opts.destination),
                opts.source,
                None,
            );
        }
    } else if !plan.tier2_facts.is_empty() && !opts.assume_yes && !tty {
        return crate::cli::consent::confirm(&action, &plan.tier2_facts, false);
    }
    Ok(())
}

// ── The seam ────────────────────────────────────────────────────────────────

/// The steps of a rewrite that touch the drive.
pub trait RewriteSteps {
    /// The quiet-host question `volume write` asks (ADR-0012 2026-09-24 item
    /// 7), asked ONCE here, before the read and long before the erase: the
    /// write after the erase must never be refused over it.
    fn host_preflight(&self, what: &str) -> Result<()>;
    /// What the write would refuse from the catalog and the staged files
    /// alone — a staged set not whole on disk, a tenant or the operator
    /// without a key, a missing escrow recipient — for a write of only
    /// `only_stage_sets` (`None`: every staged set). Asked after the slices
    /// are staged and before anything is retired.
    fn write_preflight(&self, label: &str, only_stage_sets: Option<&[i64]>) -> Result<()>;
    /// `compact-read` the source's live slices into staging.
    fn read(&self, source: &str) -> Result<CompactReadReport>;
    /// `cartridge erase` (short) of the cartridge in the drive, confirmed
    /// with `confirm_label` — the source's label, which must be the label on
    /// the tape's File 0.
    fn erase(&self, confirm_label: &str) -> Result<EraseReport>;
    /// `volume init LABEL` on the loaded cartridge.
    fn init(&self, label: &str) -> Result<Initialized>;
    /// `volume write LABEL` with the FULL confirm, writing only
    /// `only_stage_sets` when given.
    fn write(&self, label: &str, only_stage_sets: Option<&[i64]>) -> Result<()>;
    /// `volume resume LABEL` with the FULL confirm: continue a write that
    /// stopped after its plan.
    fn resume_write(&self, label: &str) -> Result<()>;
    /// The chip serial of the cartridge in the drive, if it can be read.
    fn chip_serial(&self) -> Option<String>;
}

/// [`RewriteSteps`] on the real drive.
pub struct DriveRewriteSteps<'a> {
    pub conn: &'a Connection,
    pub paths: &'a crate::config::TapectlPaths,
    pub config: &'a Config,
    pub device: &'a str,
    pub allow_missing_escrow: bool,
    pub prewrite_hash: bool,
    /// The global `--yes`: it answers `volume write`'s quiet-host question
    /// and nothing else.
    pub assume_yes: bool,
}

impl RewriteSteps for DriveRewriteSteps<'_> {
    fn host_preflight(&self, what: &str) -> Result<()> {
        progress::waited(
            || "the operator to answer the quiet-host check".to_string(),
            || crate::host_check::preflight(&self.config.host_check(), what, self.assume_yes),
        )
    }

    fn write_preflight(&self, label: &str, only_stage_sets: Option<&[i64]>) -> Result<()> {
        write::write_catalog_preflight_in(
            self.conn,
            label,
            self.allow_missing_escrow,
            only_stage_sets,
        )
    }

    fn read(&self, source: &str) -> Result<CompactReadReport> {
        use crate::store::TapeStore;
        use crate::tape::contact::{ContactSite, Medium, Operation};
        // The same fact check every read path runs, before the store opens
        // (issue #166), and the contact `compact-read` makes — recorded as
        // this command's: `volume rewrite`.
        let reads = crate::tape::mam_journal::MamReads::new(self.conn, Operation::VolumeRewrite);
        reads.check_read_contact(self.config, self.device)?;
        let observed = crate::volume::binding::loaded_medium(self.config, self.device, &reads);
        let mut store = TapeStore::open_read(self.device, crate::cli::volume::DEFAULT_BLOCK_SIZE)?;
        write::compact_read(
            self.conn,
            self.config,
            source,
            &mut store,
            ContactSite::new(
                self.config,
                Operation::VolumeRewrite,
                self.device,
                Medium::from_read(observed.as_ref().map(|(b, m)| (*b, m))),
            )
            .with_mam_reads(&reads),
        )
    }

    fn erase(&self, confirm_label: &str) -> Result<EraseReport> {
        crate::volume::erase::cartridge_erase(
            self.conn,
            self.config,
            self.device,
            crate::volume::erase::EraseOptions {
                mode: crate::store::EraseMode::Short,
                confirm_label: Some(confirm_label),
                dry_run: false,
                block_size: crate::cli::volume::DEFAULT_BLOCK_SIZE,
            },
        )
    }

    fn init(&self, label: &str) -> Result<Initialized> {
        // No `--force`: the cartridge was just erased and read back EMPTY,
        // which is `volume init`'s own verified-erase blank proof.
        write::volume_init(
            self.conn,
            self.config,
            label,
            self.device,
            crate::cli::volume::DEFAULT_BLOCK_SIZE,
            false,
            None,
            None,
        )
    }

    fn write(&self, label: &str, only_stage_sets: Option<&[i64]>) -> Result<()> {
        write::volume_write_scoped(
            self.conn,
            self.paths,
            self.config,
            label,
            self.device,
            crate::cli::volume::DEFAULT_BLOCK_SIZE,
            false,
            self.allow_missing_escrow,
            self.prewrite_hash,
            true,
            // The quiet-host question was asked once, at the start (see
            // `host_preflight`); it is not asked again with the cartridge
            // erased and the slices staged.
            true,
            only_stage_sets,
        )
    }

    fn resume_write(&self, label: &str) -> Result<()> {
        write::volume_resume(
            self.conn,
            self.paths,
            self.config,
            label,
            self.device,
            crate::cli::volume::DEFAULT_BLOCK_SIZE,
            self.prewrite_hash,
            true,
        )
    }

    fn chip_serial(&self) -> Option<String> {
        let backend = crate::config::resolve_lto_backend(self.config, Some(self.device)).ok()?;
        crate::tape::media_detect::detect(self.device, &backend.device_sg)
            .mam
            .serial
    }
}

// ── The staged slices, re-read from the disk ────────────────────────────────

/// Re-read every slice `read` staged from the staging disk and match its
/// ciphertext hash and size against what the catalog recorded for it
/// (`stage_slices.sha256_encrypted`, `encrypted_bytes`).
///
/// `compact-read` verified each slice as it streamed off the tape; this is
/// the second look, at the bytes that will be the only copy: what the disk
/// holds now, not what passed through memory. A mismatch refuses before
/// anything destructive, naming the unit, version, slice and path.
pub fn verify_staged(conn: &Connection, staged: &[StagedSlice]) -> Result<()> {
    use crate::pass_count::Sha256;
    use sha2::Digest;
    use std::io::Read;

    let phase = progress::phase(
        "rewrite-verify-staged",
        Some(
            staged
                .iter()
                .map(|s| std::fs::metadata(&s.path).map(|m| m.len()).unwrap_or(0))
                .sum(),
        ),
    );
    for slice in staged {
        let (expected, bytes, unit, version, number): (String, i64, String, i64, i64) = conn
            .query_row(
                "SELECT sl.sha256_encrypted, sl.encrypted_bytes, u.name, s.version, sl.slice_number
                 FROM stage_slices sl
                 JOIN stage_sets ss ON ss.id = sl.stage_set_id
                 JOIN snapshots s ON s.id = ss.snapshot_id
                 JOIN units u ON u.id = s.unit_id
                 WHERE sl.id = ?1",
                params![slice.slice_id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
            )?;
        phase.item(format!("{unit} v{version} slice {number}"));
        let mismatch = |what: String| {
            TapectlError::Other(format!(
                "volume rewrite refused: the staged copy of unit \"{unit}\" v{version} slice \
                 {number} ({}) is not what the catalog recorded — {what}. Nothing destructive \
                 was done: the source volume is untouched and its staging was put back as it \
                 was.",
                slice.path.display()
            ))
        };
        let mut file = std::fs::File::open(&slice.path)
            .map_err(|e| mismatch(format!("it cannot be opened: {e}")))?;
        let mut hasher = Sha256::new();
        let mut buf = vec![0u8; 1 << 20];
        let mut total: i64 = 0;
        loop {
            let n = file
                .read(&mut buf)
                .map_err(|e| mismatch(format!("it cannot be read: {e}")))?;
            if n == 0 {
                break;
            }
            hasher.update(&buf[..n]);
            total += n as i64;
            progress::add_bytes(n as u64);
        }
        let actual = format!("{:x}", hasher.finalize());
        if total != bytes {
            return Err(mismatch(format!(
                "it holds {total} bytes, the catalog recorded {bytes}"
            )));
        }
        if actual != expected {
            return Err(mismatch(format!(
                "its sha256 is {actual}, the catalog recorded {expected}"
            )));
        }
    }
    phase.done();
    Ok(())
}

/// Put staging back as `volume rewrite` found it, for a rewrite that
/// refuses before it destroys anything: each slice file it read is removed,
/// each `staging_path` is restored to what it was, and each stage set it
/// promoted goes back to the status it had.
fn restore_staging(
    conn: &Connection,
    staged: &[StagedSlice],
    prior_status: &HashMap<i64, String>,
    compact_dir: &Path,
) {
    for s in staged {
        let restored = s.prior.clone().filter(|p| Path::new(p) != s.path.as_path());
        if let Err(e) = std::fs::remove_file(&s.path) {
            if e.kind() != std::io::ErrorKind::NotFound {
                warn!(path = %s.path.display(), error = %e, "could not remove a staged slice");
            }
        }
        if let Err(e) = conn.execute(
            "UPDATE stage_slices SET staging_path = ?1 WHERE id = ?2",
            params![restored, s.slice_id],
        ) {
            warn!(slice_id = s.slice_id, error = %e, "could not restore a slice's staging_path");
        }
    }
    for (set, status) in prior_status {
        if let Err(e) = conn.execute(
            "UPDATE stage_sets SET status = ?2 WHERE id = ?1 AND status = 'staged'",
            params![set, status],
        ) {
            warn!(stage_set_id = set, error = %e, "could not restore a stage set's status");
        }
    }
    let _ = std::fs::remove_dir(compact_dir);
}

// ── The command ─────────────────────────────────────────────────────────────

/// What a rewrite did, for the CLI to print.
#[derive(Debug)]
pub struct RewriteReport {
    pub source: String,
    pub destination: String,
    pub consent: Consent,
    pub slices: usize,
    pub bytes: i64,
    /// Bytes of reclaimable/purged versions the rewrite dropped from the
    /// cartridge (0 on a resume, which no longer knows).
    pub reclaimed_bytes: i64,
    pub resumed: bool,
    /// What happened to the staged slices after the full readback.
    pub release: Option<crate::cli::volume::StagingRelease>,
}

/// Keep `err`'s write-family classification (so its exit code stands) and
/// say what the rewrite left behind: the staged slices, held, and the command
/// that finishes it. An error that is not classified, once the tape has been
/// touched, is "interrupted, resumable" (exit 4), not "refused with the
/// medium untouched" (exit 2).
fn stopped(err: TapectlError, tape_touched: bool, note: &str) -> TapectlError {
    let with = |text: String| format!("{text}\n{note}");
    match err {
        TapectlError::WriteInterrupted(m) => TapectlError::WriteInterrupted(with(m)),
        TapectlError::WriteAborted(m) => TapectlError::WriteAborted(with(m)),
        TapectlError::WriteQuarantined(m) => TapectlError::WriteQuarantined(with(m)),
        TapectlError::ConfirmInconclusive(m) => TapectlError::ConfirmInconclusive(with(m)),
        other if tape_touched || other.erase_exit_code().is_some() => {
            TapectlError::WriteInterrupted(with(other.to_string()))
        }
        other => TapectlError::Other(with(other.to_string())),
    }
}

fn resume_note(destination: &str, held: bool) -> String {
    if held {
        format!(
            "The rewrite is NOT finished: its staged slices are held (nothing releases them, \
             `--force` included) until \"{destination}\" has passed a full readback. Load the \
             cartridge and finish it with `tapectl volume rewrite --resume {destination}`."
        )
    } else {
        format!("Finish it with `tapectl volume rewrite --resume {destination}`.")
    }
}

/// `tapectl volume rewrite SOURCE --label NEW`.
pub fn rewrite(
    conn: &Connection,
    config: &Config,
    steps: &dyn RewriteSteps,
    opts: &RewriteOptions<'_>,
) -> Result<RewriteReport> {
    let plan = preflight(conn, config, opts)?;
    let source = opts.source;
    let destination = opts.destination;

    // Everything the run can know before it reads or destroys anything is
    // asked now: the quiet-host question (so the write after the erase is
    // never refused over it) and the chip in the drive (`cartridge erase`
    // refuses without a serial, and the cartridge in the drive must be the
    // one the catalog has the source on).
    steps.host_preflight(&format!("volume rewrite \"{source}\""))?;
    let chip = require_chip(steps, &plan, source)?;

    // A stale record of an earlier rewrite of this volume that never got past
    // staging: nothing destructive happened, so it is simply replaced.
    if let Some(stale) = open_for_source(conn, plan.source_id)? {
        close(conn, stale.id, "abandoned")?;
        record_step(
            conn,
            plan.source_id,
            source,
            "rewrite_abandoned",
            serde_json::json!({"destination": stale.destination, "reason": "replaced by a new rewrite"}),
        )?;
    }

    // What staging looks like now, so a refusal before the erase can put it
    // back: every live set's status.
    let prior_status: HashMap<i64, String> = {
        let mut m = HashMap::new();
        for s in &plan.slices {
            let status: String = conn.query_row(
                "SELECT status FROM stage_sets WHERE id = ?1",
                params![s.stage_set_id],
                |r| r.get(0),
            )?;
            m.insert(s.stage_set_id, status);
        }
        m
    };

    record_step(
        conn,
        plan.source_id,
        source,
        "rewrite_started",
        serde_json::json!({
            "destination": destination,
            "cartridge": plan.cartridge_barcode,
            "slices": plan.slices.len(),
            "live_bytes": plan.live_bytes,
            "reclaimable_bytes": plan.reclaimable_bytes,
            "staging": plan.compact_dir.display().to_string(),
            "only_copy": plan.needs_typed_consent(),
        }),
    )?;

    // 2. Read the live slices into staging.
    let phase = progress::phase("rewrite-read", Some(plan.live_bytes.max(0) as u64));
    let read = steps.read(source)?;
    phase.done();
    if read.staged_sets.is_empty() || read.slices_skipped > 0 {
        return Err(TapectlError::Other(format!(
            "volume rewrite refused: the read of \"{source}\" did not stage every live slice \
             ({} read, {} skipped). Nothing destructive was done.",
            read.slices_read, read.slices_skipped
        )));
    }

    // The hold begins the moment the slices are staged, before anything is
    // checked: until the source is retired it is moot (the source is a fully
    // read back copy), and once it is retired it is the whole point.
    let consent_kind = if plan.needs_typed_consent() {
        if opts.only_copy_on_staging.is_some() {
            Consent::TypedFlag
        } else {
            Consent::TypedLabel
        }
    } else {
        Consent::CopiesElsewhere
    };
    let pending_id = insert_pending(
        conn,
        plan.source_id,
        destination,
        Some(chip.as_str()),
        consent_kind,
        &read.staged_sets,
    )?;

    // Anything that refuses from here to the retirement puts staging back and
    // closes the record: nothing destructive has happened.
    let abandon = |why: &str| {
        restore_staging(conn, &read.staged, &prior_status, &plan.compact_dir);
        let _ = close(conn, pending_id, "abandoned");
        let _ = record_step(
            conn,
            plan.source_id,
            source,
            "rewrite_abandoned",
            serde_json::json!({"destination": destination, "reason": why}),
        );
    };

    // 3. Re-read every staged slice from the disk.
    if let Err(e) = verify_staged(conn, &read.staged) {
        abandon("a staged slice did not match its recorded hash");
        return Err(e);
    }
    record_step(
        conn,
        plan.source_id,
        source,
        "rewrite_staged_verified",
        serde_json::json!({
            "slices": read.staged.len(),
            "bytes": read.bytes_read,
            "staging": plan.compact_dir.display().to_string(),
        }),
    )?;

    // The write that follows the erase must not be able to refuse for
    // anything the catalog and the staged files already show: whole on disk,
    // every key, the escrow recipient.
    let scope: Option<&[i64]> = if opts.with_staged {
        None
    } else {
        Some(read.staged_sets.as_slice())
    };
    if let Err(e) = steps.write_preflight(destination, scope) {
        abandon("the write would be refused");
        return Err(TapectlError::Other(format!(
            "volume rewrite refused: the write of \"{destination}\" would be refused — {e}. \
             Nothing destructive was done and staging was put back as it was."
        )));
    }

    // 4. Consent.
    let action = format!(
        "rewrite volume \"{source}\" in place onto \"{destination}\" (retire it, erase its \
         cartridge, write the staged slices to \"{destination}\")"
    );
    let consented = if plan.needs_typed_consent() {
        crate::cli::consent::confirm_only_copy(
            &action,
            &plan.typed_facts(source, destination),
            source,
            opts.only_copy_on_staging,
        )
    } else if plan.tier2_facts.is_empty() {
        Ok(())
    } else {
        crate::cli::consent::confirm(&action, &plan.tier2_facts, opts.assume_yes)
    };
    if let Err(e) = consented {
        abandon("consent was not given");
        return Err(e);
    }
    record_step(
        conn,
        plan.source_id,
        source,
        "rewrite_consent",
        serde_json::json!({"consent": consent_kind.as_str(), "destination": destination}),
    )?;

    // 5. Retire the source. The volume's session lock is held for the
    // retirement alone: `cartridge erase` takes it again for the volume it
    // finds on the tape, and a lock is never taken twice.
    {
        let _lock = crate::staging::lock::acquire_volume_to_change(
            conn,
            plan.source_id,
            source,
            &format!("rewrite volume \"{source}\""),
        )
        .inspect_err(|_| abandon("the source volume has a live session"))?;
        let tx = conn.unchecked_transaction()?;
        let changed = tx.execute(
            "UPDATE volumes SET status = 'retired' WHERE id = ?1 AND status = 'sealed'",
            params![plan.source_id],
        )?;
        if changed != 1 {
            drop(tx);
            abandon("the source volume changed state");
            return Err(TapectlError::Other(format!(
                "volume rewrite refused: volume \"{source}\" is no longer sealed. Nothing \
                 destructive was done."
            )));
        }
        events::log_event(
            &tx,
            "volume",
            plan.source_id,
            Some(source),
            "retired",
            Some("status"),
            Some("sealed"),
            Some("retired"),
            Some(
                &serde_json::json!({
                    "by": "volume rewrite",
                    "destination": destination,
                    "consent": consent_kind.as_str(),
                })
                .to_string(),
            ),
            None,
        )?;
        crate::cli::operations::free_cartridge_if_last_live(&tx, plan.source_id)?;
        tx.execute(
            "UPDATE pending_rewrites SET state = 'retired' WHERE id = ?1",
            params![pending_id],
        )?;
        tx.commit()?;
    }
    info!(volume = source, "volume rewrite: source retired");

    let pending = open_for_destination(conn, destination)?.ok_or_else(|| {
        TapectlError::Other("volume rewrite: its record vanished after it was written".into())
    })?;
    let mut report = RewriteReport {
        source: source.to_string(),
        destination: destination.to_string(),
        consent: consent_kind,
        slices: read.staged.len(),
        bytes: read.bytes_read,
        reclaimed_bytes: plan.reclaimable_bytes,
        resumed: false,
        release: None,
    };
    finish_from(
        conn,
        config,
        steps,
        pending,
        opts.with_staged,
        opts.keep_staging,
        &mut report,
    )?;
    Ok(report)
}

/// `tapectl volume rewrite --resume NEW`: finish the rewrite whose
/// destination is `destination`, from wherever the catalog says it stopped.
pub fn resume(
    conn: &Connection,
    config: &Config,
    steps: &dyn RewriteSteps,
    destination: &str,
    keep_staging: bool,
) -> Result<RewriteReport> {
    let Some(pending) = open_for_destination(conn, destination)? else {
        let open: Vec<String> = conn
            .prepare(
                "SELECT destination_label FROM pending_rewrites WHERE finished_at IS NULL
                 ORDER BY destination_label",
            )?
            .query_map([], |r| r.get::<_, String>(0))?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        return Err(TapectlError::Other(format!(
            "volume rewrite --resume: no unfinished rewrite is waiting to write \"{destination}\" \
             ({}). A rewrite that finished needs nothing.",
            if open.is_empty() {
                "none is unfinished".to_string()
            } else {
                format!("unfinished: {}", open.join(", "))
            }
        )));
    };
    if pending.state == State::Staged {
        return Err(TapectlError::Other(format!(
            "volume rewrite --resume: the rewrite of \"{}\" stopped before it retired anything, \
             so nothing is lost and there is nothing to resume. Run `tapectl volume rewrite {} \
             --label {destination}` again; its unfinished record is replaced.",
            pending.source, pending.source
        )));
    }
    let mut report = RewriteReport {
        source: pending.source.clone(),
        destination: destination.to_string(),
        consent: pending.consent,
        slices: pending.stage_set_ids.len(),
        bytes: 0,
        reclaimed_bytes: 0,
        resumed: true,
        release: None,
    };
    record_step(
        conn,
        pending.source_id,
        &pending.source,
        "rewrite_resumed",
        serde_json::json!({"destination": destination, "state": pending.state.as_str()}),
    )?;
    finish_from(
        conn,
        config,
        steps,
        pending,
        false,
        keep_staging,
        &mut report,
    )?;
    Ok(report)
}

/// The destination volume's catalog facts, for the continuation.
#[derive(Debug)]
struct DestinationFacts {
    status: String,
    /// Writes of the rewrite's sets in a state `volume resume` can continue.
    resumable_writes: i64,
    /// A completed write of every one of the rewrite's sets is fully read
    /// back on this volume.
    fully_read_back: bool,
    sealed_not_read_back: bool,
    mounted_cartridge: Option<i64>,
}

fn destination_facts(
    conn: &Connection,
    label: &str,
    stage_sets: &[i64],
) -> Result<Option<DestinationFacts>> {
    let row: Option<(i64, String)> = conn
        .query_row(
            "SELECT id, status FROM volumes WHERE label = ?1",
            params![label],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    let Some((id, status)) = row else {
        return Ok(None);
    };
    let resumable_writes: i64 = conn.query_row(
        "SELECT COUNT(*) FROM writes WHERE volume_id = ?1
           AND status IN ('planned', 'in_progress', 'interrupted')",
        params![id],
        |r| r.get(0),
    )?;
    let mut fully = !stage_sets.is_empty();
    let mut sealed_not_read_back = false;
    for ss in stage_sets {
        let (completed, verified): (i64, i64) = conn.query_row(
            "SELECT COUNT(*), COALESCE(SUM(write_verified), 0) FROM writes
             WHERE volume_id = ?1 AND stage_set_id = ?2 AND status = 'completed'",
            params![id, ss],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?;
        if completed == 0 || verified == 0 {
            fully = false;
        }
        if completed > 0 && verified == 0 {
            sealed_not_read_back = true;
        }
    }
    let mounted_cartridge: Option<i64> = conn
        .query_row(
            "SELECT cartridge_id FROM cartridge_volumes
             WHERE volume_id = ?1 AND unmounted_at IS NULL",
            params![id],
            |r| r.get(0),
        )
        .optional()?;
    Ok(Some(DestinationFacts {
        status,
        resumable_writes,
        fully_read_back: fully,
        sealed_not_read_back,
        mounted_cartridge,
    }))
}

/// From the state the catalog is in to a finished rewrite: erase, init,
/// write with the full confirm, close the record, release staging. Used by a
/// fresh rewrite right after it retired the source and by `--resume` from
/// wherever it stopped; every step first asks the catalog whether it was
/// already done.
fn finish_from(
    conn: &Connection,
    config: &Config,
    steps: &dyn RewriteSteps,
    pending: Pending,
    with_staged: bool,
    keep_staging: bool,
    report: &mut RewriteReport,
) -> Result<()> {
    let source = pending.source.clone();
    let destination = pending.destination.clone();
    let note = resume_note(&destination, true);
    let scope: Vec<i64> = pending.stage_set_ids.clone();
    // The session log's view of the window: from here (the erase, or for a
    // resume the next step of it) to the passing full readback.
    let window_phase = progress::phase("rewrite-window", None);

    // ── erase ──
    let source_status: String = conn.query_row(
        "SELECT status FROM volumes WHERE id = ?1",
        params![pending.source_id],
        |r| r.get(0),
    )?;
    let dest = destination_facts(conn, &destination, &scope)?;
    let erase_done = source_status == "erased" || dest.is_some() || pending.state >= State::Erased;
    if !erase_done {
        // A resume may find another cartridge in the drive; the erase checks
        // the tape's File 0 label against the source's, and the chip is
        // checked here against the one the rewrite recorded.
        check_same_cartridge(steps, &pending, "erase")?;
        let window = window_sentence(
            Path::new(&config.staging.directory)
                .join(format!("compact-{source}"))
                .as_path(),
            &destination,
        );
        mark_window_open(conn, &pending, &window)?;
        let erased = steps.erase(&source);
        let erase_report = match erased {
            Ok(r) => r,
            Err(e) => return Err(stopped(e, false, &note)),
        };
        window_phase.item("erased");
        for w in &erase_report.warnings {
            eprintln!("{w}");
        }
        // The erase stands; the catalog must say so, or `volume init` will
        // not take the cartridge.
        let now: String = conn.query_row(
            "SELECT status FROM volumes WHERE id = ?1",
            params![pending.source_id],
            |r| r.get(0),
        )?;
        if now != "erased" {
            return Err(stopped(
                TapectlError::Other(format!(
                    "the cartridge is erased but the catalog still holds volume \"{source}\" as \
                     {now}: {}",
                    erase_report.warnings.join(" ")
                )),
                true,
                &note,
            ));
        }
        set_state(conn, pending.id, State::Erased)?;
        record_step(
            conn,
            pending.source_id,
            &source,
            "rewrite_erased",
            serde_json::json!({"destination": destination, "window": window}),
        )?;
    }

    // ── init ──
    let dest = destination_facts(conn, &destination, &scope)?;
    let dest = match dest {
        Some(d) => d,
        None => {
            check_same_cartridge(steps, &pending, "init")?;
            let init = steps
                .init(&destination)
                .map_err(|e| stopped(e, true, &note))?;
            info!(volume = %destination, id = init.volume_id, "volume rewrite: destination initialised");
            set_state(conn, pending.id, State::Initialized)?;
            record_step(
                conn,
                pending.source_id,
                &source,
                "rewrite_initialized",
                serde_json::json!({"destination": destination, "volume_id": init.volume_id}),
            )?;
            destination_facts(conn, &destination, &scope)?.ok_or_else(|| {
                stopped(
                    TapectlError::Other(format!(
                        "volume \"{destination}\" vanished after its init"
                    )),
                    true,
                    &note,
                )
            })?
        }
    };
    if let (Some(mounted), Some(serial)) =
        (dest.mounted_cartridge, pending.cartridge_serial.as_ref())
    {
        let serial_of_mount: Option<String> = conn
            .query_row(
                "SELECT serial_number FROM cartridges WHERE id = ?1",
                params![mounted],
                |r| r.get(0),
            )
            .optional()?
            .flatten();
        if serial_of_mount.as_deref().is_some_and(|s| s != serial) {
            warn!(
                destination = %destination,
                expected = %serial,
                "volume rewrite: the destination is on another cartridge than the source was"
            );
        }
    }

    // ── write, with the full confirm ──
    if !dest.fully_read_back {
        if dest.sealed_not_read_back && dest.resumable_writes == 0 {
            return Err(stopped(
                TapectlError::Other(format!(
                    "volume \"{destination}\" is sealed but was not fully read back, and the \
                     hold lifts only on a passing full readback: `tapectl volume verify \
                     {destination} --full`"
                )),
                true,
                &note,
            ));
        }
        let writing = if dest.resumable_writes > 0 {
            steps.resume_write(&destination)
        } else if dest.status == "initialized" {
            let only = if with_staged {
                None
            } else {
                Some(scope.as_slice())
            };
            steps.write(&destination, only)
        } else {
            Err(TapectlError::Other(format!(
                "volume \"{destination}\" is {}, not a volume the staged slices can be \
                 written to",
                dest.status
            )))
        };
        if let Err(e) = writing {
            let _ = record_step(
                conn,
                pending.source_id,
                &source,
                "rewrite_stopped",
                serde_json::json!({"destination": destination, "error": e.to_string()}),
            );
            return Err(stopped(e, true, &note));
        }
    }
    let after = destination_facts(conn, &destination, &scope)?;
    if !after.as_ref().is_some_and(|d| d.fully_read_back) {
        return Err(stopped(
            TapectlError::Other(format!(
                "the write to \"{destination}\" returned, but the catalog does not show every \
                 staged set fully read back on it"
            )),
            true,
            &note,
        ));
    }
    set_state(conn, pending.id, State::Written)?;
    record_step(
        conn,
        pending.source_id,
        &source,
        "rewrite_written",
        serde_json::json!({"destination": destination}),
    )?;

    // ── close the window, then release ──
    window_phase.done();
    close(conn, pending.id, "completed")?;
    record_step(
        conn,
        pending.source_id,
        &source,
        "rewrite_window_closed",
        serde_json::json!({
            "destination": destination,
            "note": "the new volume is sealed and fully read back; the staged slices are no longer the only copy",
        }),
    )?;
    report.release = Some(crate::cli::volume::release_staging_after_write(
        conn,
        config,
        &destination,
        keep_staging,
    ));
    Ok(())
}

fn mark_window_open(conn: &Connection, pending: &Pending, window: &str) -> Result<()> {
    conn.execute(
        "UPDATE pending_rewrites SET window_opened_at = COALESCE(window_opened_at, datetime('now'))
         WHERE id = ?1",
        params![pending.id],
    )?;
    record_step(
        conn,
        pending.source_id,
        &pending.source,
        "rewrite_window_opened",
        serde_json::json!({"destination": pending.destination, "window": window}),
    )
}

/// The chip serial of the cartridge in the drive, required: `cartridge
/// erase` refuses without one, and it must be the cartridge the catalog has
/// the source on. Asked before anything is read.
fn require_chip(steps: &dyn RewriteSteps, plan: &Plan, source: &str) -> Result<String> {
    let Some(have) = steps.chip_serial() else {
        return Err(TapectlError::Other(format!(
            "volume rewrite refused: the chip serial of the cartridge in the drive could not \
             be read (the MAM read through device_sg failed: sg permissions, sg3-utils not \
             installed, or a device_sg that is not this drive's). `cartridge erase` refuses \
             without it, and nothing else proves after the erase that the cartridge erased \
             is \"{source}\"'s. `tapectl volume identify` shows what the chip reports. \
             Nothing was changed."
        )));
    };
    match plan.cartridge_serial.as_deref() {
        Some(want) if want != have => Err(TapectlError::Other(format!(
            "volume rewrite refused: the cartridge in the drive reports chip serial {have}, \
             but the catalog has volume \"{source}\" on cartridge {} (serial {want}). Load \
             that cartridge. Nothing was changed.",
            plan.cartridge_barcode
        ))),
        _ => Ok(have),
    }
}

/// The cartridge in the drive is the one the rewrite recorded — by its chip
/// serial. A recorded serial the chip cannot confirm refuses; so does a
/// different one.
fn check_same_cartridge(steps: &dyn RewriteSteps, pending: &Pending, next: &str) -> Result<()> {
    let Some(want) = pending.cartridge_serial.as_deref() else {
        return Ok(());
    };
    match steps.chip_serial() {
        Some(have) if have == want => Ok(()),
        Some(have) => Err(TapectlError::Other(format!(
            "volume rewrite: the cartridge in the drive reports chip serial {have}, but the \
             rewrite of \"{}\" is on the cartridge with serial {want}. Load that cartridge \
             before the {next}. Nothing was changed. {}",
            pending.source,
            resume_note(&pending.destination, true)
        ))),
        None => Err(TapectlError::Other(format!(
            "volume rewrite: the chip serial of the cartridge in the drive could not be read, \
             so it cannot be shown to be the cartridge of the rewrite of \"{}\" (serial \
             {want}). Nothing was changed. {}",
            pending.source,
            resume_note(&pending.destination, true)
        ))),
    }
}

#[cfg(test)]
mod tests {
    //! `volume rewrite` over the catalog and a scripted drive.
    //!
    //! The catalog is real (an in-memory database with the whole schema), and
    //! so are the functions the rewrite calls that need no tape: its
    //! preflight, `compact_read` (over an in-memory tape), the staged-slice
    //! check, the hold, the staging release. The four steps that touch the
    //! drive are [`Fake`], which leaves in the catalog what the real step
    //! leaves (the erase through `record_cartridge_erased`, the init and the
    //! write by the same rows) and can be told to fail at any of them. The
    //! real erase, init and write have their own suites, and the lifecycle
    //! suite's `rewrite-in-place` scenario runs the whole command on mhvtl.
    use super::*;
    use crate::store::{MemStore, Store};
    use crate::tape::contact::{ContactSite, Medium, Operation};
    use crate::volume::erase::{EraseOutcome, MediumKind, Recorded};
    use std::cell::{Cell, RefCell};
    use std::io::Cursor;

    const DEVICE: &str = "/nonexistent/tapectl-rewrite-nst";
    const SERIAL: &str = "CHIP-SERIAL-1";

    struct World {
        conn: Connection,
        _tmp: tempfile::TempDir,
        config: Config,
        tenant: i64,
    }

    fn world(min_copies: i32) -> World {
        let conn = crate::db::open_memory().unwrap();
        let tmp = tempfile::TempDir::new().unwrap();
        let staging = tmp.path().join("staging");
        std::fs::create_dir_all(&staging).unwrap();
        let mut config = Config::default();
        config.staging.directory = staging.to_string_lossy().into_owned();
        config.defaults.min_copies = min_copies;
        config.backends.lto.push(crate::config::LtoBackendConfig {
            name: "lto6".into(),
            device_tape: DEVICE.into(),
            device_sg: "/nonexistent/tapectl-rewrite-sg".into(),
            generation: "LTO-6".into(),
            capacity_override: None,
            fill_ceiling: 1.0,
            enospc_buffer: "0".into(),
        });
        conn.execute(
            "INSERT INTO tenants (name, is_operator, status) VALUES ('t', 0, 'active')",
            [],
        )
        .unwrap();
        let tenant = conn.last_insert_rowid();
        World {
            conn,
            _tmp: tmp,
            config,
            tenant,
        }
    }

    fn sha(bytes: &[u8]) -> String {
        use sha2::Digest;
        format!("{:x}", sha2::Sha256::digest(bytes))
    }

    /// One version of a unit: its snapshot, a `cleaned` stage set (staging
    /// released, as a written copy leaves it) and its slices.
    struct Version {
        unit: String,
        version: i64,
        snapshot_id: i64,
        stage_set_id: i64,
        slice_ids: Vec<i64>,
        data: Vec<Vec<u8>>,
    }

    fn add_version(
        w: &World,
        unit: &str,
        version: i64,
        snapshot_status: &str,
        data: &[Vec<u8>],
    ) -> Version {
        let c = &w.conn;
        let unit_id: i64 = match c
            .query_row("SELECT id FROM units WHERE name = ?1", params![unit], |r| {
                r.get(0)
            })
            .optional()
            .unwrap()
        {
            Some(id) => id,
            None => {
                c.execute(
                    "INSERT INTO units (uuid, name, tenant_id, checksum_mode, encrypt, status)
                     VALUES (?1, ?1, ?2, 'mtime_size', 1, 'active')",
                    params![unit, w.tenant],
                )
                .unwrap();
                c.last_insert_rowid()
            }
        };
        c.execute(
            "INSERT INTO snapshots (unit_id, version, status, source_path, file_count, total_size)
             VALUES (?1, ?2, ?3, '/tmp', 1, 10)",
            params![unit_id, version, snapshot_status],
        )
        .unwrap();
        let snapshot_id = c.last_insert_rowid();
        c.execute(
            "INSERT INTO stage_sets (snapshot_id, status, slice_size, cleaned_at)
             VALUES (?1, 'cleaned', 524288, datetime('now'))",
            params![snapshot_id],
        )
        .unwrap();
        let stage_set_id = c.last_insert_rowid();
        let mut slice_ids = Vec::new();
        for (i, bytes) in data.iter().enumerate() {
            c.execute(
                "INSERT INTO stage_slices (stage_set_id, slice_number, size_bytes,
                                           encrypted_bytes, sha256_plain, sha256_encrypted)
                 VALUES (?1, ?2, ?3, ?3, ?4, ?4)",
                params![stage_set_id, i as i64 + 1, bytes.len() as i64, sha(bytes)],
            )
            .unwrap();
            slice_ids.push(c.last_insert_rowid());
        }
        Version {
            unit: unit.to_string(),
            version,
            snapshot_id,
            stage_set_id,
            slice_ids,
            data: data.to_vec(),
        }
    }

    fn add_volume(w: &World, label: &str, status: &str) -> i64 {
        w.conn
            .execute(
                "INSERT INTO volumes (label, backend_type, backend_name, media_type,
                                      capacity_bytes, status)
                 VALUES (?1, 'lto', 'lto6', 'LTO-6', 100000000, ?2)",
                params![label, status],
            )
            .unwrap();
        w.conn.last_insert_rowid()
    }

    /// A completed write of `v` on `volume_id`, its slices at `positions`.
    fn add_write(w: &World, volume_id: i64, v: &Version, positions: &[u32], verified: bool) {
        w.conn
            .execute(
                "INSERT INTO writes (stage_set_id, snapshot_id, volume_id, status, write_verified)
                 VALUES (?1, ?2, ?3, 'completed', ?4)",
                params![v.stage_set_id, v.snapshot_id, volume_id, verified],
            )
            .unwrap();
        let write_id = w.conn.last_insert_rowid();
        for ((slice_id, bytes), pos) in v.slice_ids.iter().zip(&v.data).zip(positions) {
            w.conn
                .execute(
                    "INSERT INTO write_positions (write_id, stage_slice_id, position, status,
                                                  sha256_on_volume)
                     VALUES (?1, ?2, ?3, 'written', ?4)",
                    params![write_id, slice_id, pos.to_string(), sha(bytes)],
                )
                .unwrap();
        }
    }

    /// Cartridge `barcode` (chip serial [`SERIAL`]) holding `volume_id`.
    fn add_cartridge(w: &World, barcode: &str, volume_id: i64) -> i64 {
        w.conn
            .execute(
                "INSERT INTO cartridges (barcode, media_type, nominal_capacity, status,
                                         serial_number)
                 VALUES (?1, 'LTO-6', 2500000000000, 'in_use', ?2)",
                params![barcode, SERIAL],
            )
            .unwrap();
        let cartridge_id = w.conn.last_insert_rowid();
        w.conn
            .execute(
                "INSERT INTO cartridge_volumes (cartridge_id, volume_id) VALUES (?1, ?2)",
                params![cartridge_id, volume_id],
            )
            .unwrap();
        cartridge_id
    }

    /// The standard rewrite: SRC (sealed, on cartridge CART-1) holds
    /// `keepsake` v1 and `docs` v1 (live), and `old` v1 (reclaimable).
    /// Positions: 4 keepsake/1, 5 keepsake/2, 6 docs/1, 7 old/1.
    struct Scene {
        w: World,
        src: i64,
        cart: i64,
        keepsake: Version,
        docs: Version,
        old: Version,
    }

    fn data(tag: &str, n: usize) -> Vec<Vec<u8>> {
        (0..n)
            .map(|i| {
                format!("{tag} slice {i} ciphertext stand-in, repeated. ")
                    .repeat(20)
                    .into_bytes()
            })
            .collect()
    }

    fn scene(min_copies: i32) -> Scene {
        let w = world(min_copies);
        let keepsake = add_version(&w, "keepsake", 1, "current", &data("keepsake", 2));
        let docs = add_version(&w, "docs", 1, "current", &data("docs", 1));
        let old = add_version(&w, "old", 1, "reclaimable", &data("old", 1));
        let src = add_volume(&w, "SRC", "sealed");
        add_write(&w, src, &keepsake, &[4, 5], true);
        add_write(&w, src, &docs, &[6], true);
        add_write(&w, src, &old, &[7], true);
        let cart = add_cartridge(&w, "CART-1", src);
        Scene {
            w,
            src,
            cart,
            keepsake,
            docs,
            old,
        }
    }

    /// A second sealed, fully read back volume `OTHER` with a copy of `v`.
    fn copy_elsewhere(s: &Scene, v: &Version, positions: &[u32], verified: bool, label: &str) {
        let other = add_volume(&s.w, label, "sealed");
        add_write(&s.w, other, v, positions, verified);
    }

    /// What is on the source tape, by position.
    fn tape_files(s: &Scene) -> Vec<(u32, Vec<u8>)> {
        let mut files = Vec::new();
        for (v, positions) in [
            (&s.keepsake, vec![4u32, 5]),
            (&s.docs, vec![6]),
            (&s.old, vec![7]),
        ] {
            for (bytes, p) in v.data.iter().zip(positions) {
                files.push((p, bytes.clone()));
            }
        }
        files
    }

    /// The scripted drive.
    struct Fake<'a> {
        w: &'a World,
        files: Vec<(u32, Vec<u8>)>,
        source_id: i64,
        cartridge_id: i64,
        log: RefCell<Vec<String>>,
        fail_erase: Cell<bool>,
        fail_init: Cell<bool>,
        fail_write: Cell<bool>,
        /// The fake write is a quick confirm: `write_verified = 0`.
        quick_write: Cell<bool>,
        corrupt_a_staged_slice: Cell<bool>,
        serial: RefCell<Option<String>>,
        /// What the stage sets passed to the write were.
        written_scope: RefCell<Option<Vec<i64>>>,
        /// The catalog-only asks, in order (`host ...`, `write_preflight ...`).
        asked: RefCell<Vec<String>>,
        fail_write_preflight: Cell<bool>,
    }

    impl<'a> Fake<'a> {
        fn new(s: &'a Scene) -> Fake<'a> {
            Fake {
                w: &s.w,
                files: tape_files(s),
                source_id: s.src,
                cartridge_id: s.cart,
                log: RefCell::new(Vec::new()),
                fail_erase: Cell::new(false),
                fail_init: Cell::new(false),
                fail_write: Cell::new(false),
                quick_write: Cell::new(false),
                corrupt_a_staged_slice: Cell::new(false),
                serial: RefCell::new(Some(SERIAL.to_string())),
                written_scope: RefCell::new(None),
                asked: RefCell::new(Vec::new()),
                fail_write_preflight: Cell::new(false),
            }
        }

        fn calls(&self) -> Vec<String> {
            self.log.borrow().clone()
        }
    }

    fn erase_report(serial: &str) -> EraseReport {
        EraseReport {
            device: DEVICE.into(),
            mode: "short",
            chip_serial: Some(serial.into()),
            medium: MediumKind::Tapectl,
            file0: None,
            sealed: None,
            detail: None,
            catalog_volumes: Vec::new(),
            catalog_cartridge: None,
            notes: Vec::new(),
            confirm_label: None,
            outcome: EraseOutcome::Erased,
            medium_after: Some(MediumKind::Empty),
            chip_serial_after: Some(serial.into()),
            recorded: Some(Recorded::default()),
            warnings: Vec::new(),
        }
    }

    impl RewriteSteps for Fake<'_> {
        fn host_preflight(&self, what: &str) -> Result<()> {
            self.asked.borrow_mut().push(format!("host {what}"));
            Ok(())
        }

        fn write_preflight(&self, label: &str, only: Option<&[i64]>) -> Result<()> {
            self.asked.borrow_mut().push(format!(
                "write_preflight {label} {}",
                if only.is_some() { "scoped" } else { "all" }
            ));
            if self.fail_write_preflight.get() {
                return Err(TapectlError::Other(
                    "refusing to write: these staged sets are not whole on disk".into(),
                ));
            }
            Ok(())
        }

        fn read(&self, source: &str) -> Result<CompactReadReport> {
            self.log.borrow_mut().push("read".into());
            let last = self.files.iter().map(|(p, _)| *p).max().unwrap();
            let mut store = MemStore::new(4096);
            for p in 0..=last {
                let bytes = self
                    .files
                    .iter()
                    .find(|(q, _)| *q == p)
                    .map(|(_, b)| b.clone())
                    .unwrap_or_else(|| {
                        if p == 0 {
                            // File 0 states the format this tapectl reads and
                            // nothing about the volume (as write.rs's recovery
                            // read fixtures do): no identity to corroborate.
                            b"[volume]\nmagic = \"tapectl-volume-v2\"\nlayout_version = 2\nrequires = []\n"
                                .to_vec()
                        } else {
                            vec![0u8]
                        }
                    });
                store
                    .execute(&mut Cursor::new(bytes.clone()), bytes.len() as u64, false)
                    .unwrap();
            }
            let report = write::compact_read(
                &self.w.conn,
                &self.w.config,
                source,
                &mut store,
                ContactSite::new(
                    &self.w.config,
                    Operation::VolumeRewrite,
                    DEVICE,
                    Medium::NoBackend,
                ),
            )?;
            if self.corrupt_a_staged_slice.get() {
                let victim = &report.staged[0].path;
                let mut bytes = std::fs::read(victim).unwrap();
                bytes[0] ^= 0xff;
                std::fs::write(victim, bytes).unwrap();
            }
            Ok(report)
        }

        fn erase(&self, confirm_label: &str) -> Result<EraseReport> {
            self.log.borrow_mut().push(format!("erase {confirm_label}"));
            // The real erase refuses a volume the catalog has not given up.
            if !coverage::is_given_up(&self.w.conn, self.source_id)? {
                return Err(TapectlError::Other(
                    "cartridge erase refused: the catalog has not given the volume up".into(),
                ));
            }
            if self.fail_erase.get() {
                return Err(TapectlError::EraseIncomplete(
                    "the erase failed part-way".into(),
                ));
            }
            let tx = self.w.conn.unchecked_transaction().unwrap();
            crate::cli::operations::record_cartridge_erased(
                &tx,
                self.cartridge_id,
                "CART-1",
                "pending_erase",
                &[self.source_id],
                "available",
                None,
            )?;
            tx.commit().unwrap();
            Ok(erase_report(SERIAL))
        }

        fn init(&self, label: &str) -> Result<Initialized> {
            self.log.borrow_mut().push(format!("init {label}"));
            if self.fail_init.get() {
                return Err(TapectlError::Other("volume init refused".into()));
            }
            let id = add_volume(self.w, label, "initialized");
            self.w
                .conn
                .execute(
                    "INSERT INTO cartridge_volumes (cartridge_id, volume_id) VALUES (?1, ?2)",
                    params![self.cartridge_id, id],
                )
                .unwrap();
            self.w
                .conn
                .execute(
                    "UPDATE cartridges SET status = 'in_use' WHERE id = ?1",
                    params![self.cartridge_id],
                )
                .unwrap();
            Ok(Initialized {
                volume_id: id,
                medium: crate::tape::media_detect::MediumDecision {
                    generation: crate::media::Generation::Lto6,
                    generation_source: crate::tape::media_detect::MediaSource::Declared,
                    density_code: None,
                    capacity_bytes: 100_000_000,
                    capacity_source: crate::media::CapacitySource::GenerationTable,
                },
            })
        }

        fn write(&self, label: &str, only: Option<&[i64]>) -> Result<()> {
            self.log.borrow_mut().push(format!("write {label}"));
            *self.written_scope.borrow_mut() = only.map(<[i64]>::to_vec);
            if self.fail_write.get() {
                return Err(TapectlError::WriteInterrupted(
                    "the write stopped (cartridge full)".into(),
                ));
            }
            let (vol_id, _): (i64, String) = self
                .w
                .conn
                .query_row(
                    "SELECT id, status FROM volumes WHERE label = ?1",
                    params![label],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .unwrap();
            let sets: Vec<i64> = match only {
                Some(o) => o.to_vec(),
                None => self
                    .w
                    .conn
                    .prepare("SELECT id FROM stage_sets WHERE status = 'staged'")
                    .unwrap()
                    .query_map([], |r| r.get(0))
                    .unwrap()
                    .collect::<std::result::Result<_, _>>()
                    .unwrap(),
            };
            for ss in sets {
                let snap: i64 = self
                    .w
                    .conn
                    .query_row(
                        "SELECT snapshot_id FROM stage_sets WHERE id = ?1",
                        params![ss],
                        |r| r.get(0),
                    )
                    .unwrap();
                self.w
                    .conn
                    .execute(
                        "INSERT INTO writes (stage_set_id, snapshot_id, volume_id, status, write_verified)
                         VALUES (?1, ?2, ?3, 'completed', ?4)",
                        params![ss, snap, vol_id, !self.quick_write.get()],
                    )
                    .unwrap();
            }
            self.w
                .conn
                .execute(
                    "UPDATE volumes SET status = 'sealed' WHERE id = ?1",
                    params![vol_id],
                )
                .unwrap();
            Ok(())
        }

        fn resume_write(&self, label: &str) -> Result<()> {
            self.log.borrow_mut().push(format!("resume_write {label}"));
            self.write(label, None)
        }

        fn chip_serial(&self) -> Option<String> {
            self.serial.borrow().clone()
        }
    }

    fn opts<'a>(source: &'a str, destination: &'a str) -> RewriteOptions<'a> {
        RewriteOptions {
            source,
            destination,
            device: DEVICE,
            only_copy_on_staging: None,
            assume_yes: false,
            with_staged: false,
            keep_staging: false,
        }
    }

    fn scalar<T: rusqlite::types::FromSql>(c: &Connection, sql: &str) -> T {
        c.query_row(sql, [], |r| r.get(0)).unwrap()
    }

    fn volume_status(c: &Connection, label: &str) -> String {
        c.query_row(
            "SELECT status FROM volumes WHERE label = ?1",
            params![label],
            |r| r.get(0),
        )
        .unwrap()
    }

    fn set_status(c: &Connection, id: i64) -> String {
        c.query_row(
            "SELECT status FROM stage_sets WHERE id = ?1",
            params![id],
            |r| r.get(0),
        )
        .unwrap()
    }

    fn events_of(c: &Connection, action: &str) -> i64 {
        c.query_row(
            "SELECT COUNT(*) FROM events WHERE action = ?1",
            params![action],
            |r| r.get(0),
        )
        .unwrap()
    }

    fn staged_files(config: &Config) -> Vec<PathBuf> {
        fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
            if let Ok(rd) = std::fs::read_dir(dir) {
                for e in rd.flatten() {
                    let p = e.path();
                    if p.is_dir() {
                        walk(&p, out);
                    } else {
                        out.push(p);
                    }
                }
            }
        }
        let mut out = Vec::new();
        walk(Path::new(&config.staging.directory), &mut out);
        out.sort();
        out
    }

    // ── the hold ────────────────────────────────────────────────────────────

    /// Stage set `v` as a rewrite leaves it after the erase: `staged`, its
    /// slice files on the staging disk, the source volume erased and a
    /// pending rewrite onto `dest` naming it.
    fn held_after_erase(s: &Scene, v: &Version, dest: &str) -> Vec<PathBuf> {
        let dir = Path::new(&s.w.config.staging.directory).join("compact-SRC");
        std::fs::create_dir_all(&dir).unwrap();
        let mut paths = Vec::new();
        for (slice_id, bytes) in v.slice_ids.iter().zip(&v.data) {
            let path = dir.join(format!("slice_{slice_id}.dat"));
            std::fs::write(&path, bytes).unwrap();
            s.w.conn
                .execute(
                    "UPDATE stage_slices SET staging_path = ?1 WHERE id = ?2",
                    params![path.to_string_lossy(), slice_id],
                )
                .unwrap();
            paths.push(path);
        }
        s.w.conn
            .execute(
                "UPDATE stage_sets SET status = 'staged' WHERE id = ?1",
                params![v.stage_set_id],
            )
            .unwrap();
        s.w.conn
            .execute(
                "UPDATE volumes SET status = 'erased' WHERE id = ?1",
                params![s.src],
            )
            .unwrap();
        insert_pending(
            &s.w.conn,
            s.src,
            dest,
            Some(SERIAL),
            Consent::TypedFlag,
            &[v.stage_set_id],
        )
        .unwrap();
        paths
    }

    #[test]
    fn a_set_is_held_while_its_rewrite_is_open_and_no_fully_read_back_copy_exists() {
        let s = scene(1);
        held_after_erase(&s, &s.keepsake, "NEW");
        let held = holds(&s.w.conn).unwrap();
        let hold = held.get(&s.keepsake.stage_set_id).expect("the set is held");
        assert_eq!(hold.source, "SRC");
        assert_eq!(hold.destination, "NEW");
        assert_eq!(hold.resume_command(), "tapectl volume rewrite --resume NEW");
        assert_eq!(held.len(), 1, "only the rewrite's own set: {held:?}");
        assert!(hold_on(&s.w.conn, s.docs.stage_set_id).unwrap().is_none());

        // NEW sealed after a QUICK confirm is a copy, but not a fully read
        // back one: the hold stays (item 106 releases after the full readback).
        let new = add_volume(&s.w, "NEW", "sealed");
        add_write(&s.w, new, &s.keepsake, &[4, 5], false);
        assert!(holds(&s.w.conn)
            .unwrap()
            .contains_key(&s.keepsake.stage_set_id));

        // A fully read back write on a volume that is not eligible (quarantined)
        // does not lift it either.
        s.w.conn
            .execute(
                "UPDATE volumes SET observed_condition = 'quarantined' WHERE id = ?1",
                params![new],
            )
            .unwrap();
        s.w.conn
            .execute(
                "UPDATE writes SET write_verified = 1 WHERE volume_id = ?1",
                params![new],
            )
            .unwrap();
        assert!(holds(&s.w.conn)
            .unwrap()
            .contains_key(&s.keepsake.stage_set_id));

        // The full readback of an eligible volume lifts it, whichever command
        // ran the readback.
        s.w.conn
            .execute(
                "UPDATE volumes SET observed_condition = 'ok' WHERE id = ?1",
                params![new],
            )
            .unwrap();
        assert!(holds(&s.w.conn).unwrap().is_empty());
    }

    #[test]
    fn a_closed_rewrite_holds_nothing() {
        let s = scene(1);
        held_after_erase(&s, &s.keepsake, "NEW");
        assert_eq!(holds(&s.w.conn).unwrap().len(), 1);
        close(&s.w.conn, 1, "abandoned").unwrap();
        assert!(holds(&s.w.conn).unwrap().is_empty());
    }

    #[test]
    fn every_release_path_refuses_a_held_set_force_included() {
        use crate::staging::clean::{clean_staging, CleanScope};
        let s = scene(1);
        let files = held_after_erase(&s, &s.keepsake, "NEW");
        // A second staged set, written nowhere and not part of the rewrite:
        // the positive control that `--force` still releases what it may.
        let other = add_version(&s.w, "scratch", 1, "current", &data("scratch", 1));
        let other_path = Path::new(&s.w.config.staging.directory).join("scratch.age");
        std::fs::write(&other_path, &other.data[0]).unwrap();
        s.w.conn
            .execute(
                "UPDATE stage_slices SET staging_path = ?1 WHERE id = ?2",
                params![other_path.to_string_lossy(), other.slice_ids[0]],
            )
            .unwrap();
        s.w.conn
            .execute(
                "UPDATE stage_sets SET status = 'staged' WHERE id = ?1",
                params![other.stage_set_id],
            )
            .unwrap();
        let unit_id: i64 = scalar(&s.w.conn, "SELECT id FROM units WHERE name = 'keepsake'");

        // `staging clean --force` (Whole), and `--unit`/`--unit --version`.
        for (scope, label) in [
            (CleanScope::Units(&[unit_id]), "Units"),
            (
                CleanScope::UnitVersion {
                    unit_id,
                    version: 1,
                },
                "UnitVersion",
            ),
        ] {
            let report = clean_staging(&s.w.conn, &s.w.config, true, scope).unwrap();
            assert_eq!(report.sets_cleaned, 0, "{label}: nothing released");
            assert_eq!(report.held_by_pending_rewrite.len(), 1, "{label}");
            assert_eq!(report.held_by_pending_rewrite[0].0, s.keepsake.stage_set_id);
            assert!(files.iter().all(|f| f.exists()), "{label}: the slices stay");
            assert_eq!(set_status(&s.w.conn, s.keepsake.stage_set_id), "staged");
        }
        let report = clean_staging(&s.w.conn, &s.w.config, true, CleanScope::Whole).unwrap();
        assert_eq!(
            report.held_by_pending_rewrite.len(),
            1,
            "force does not reach the hold: {report:?}"
        );
        assert_eq!(
            report.sets_cleaned, 1,
            "the unheld set was released (positive control)"
        );
        assert!(!other_path.exists());
        assert!(files.iter().all(|f| f.exists()));
        // Without `--force`.
        let report = clean_staging(&s.w.conn, &s.w.config, false, CleanScope::Whole).unwrap();
        assert_eq!(report.sets_cleaned, 0);
        assert!(files.iter().all(|f| f.exists()));
    }

    /// `release_if_covered` — the release `volume write`, `volume resume`
    /// and `collection run` share. Two shapes: the only copy is the staging
    /// disk (policy refuses it on its own), and a copy elsewhere satisfies
    /// `min_copies` while only a quick confirm stands behind it (policy
    /// passes; the hold refuses).
    #[test]
    fn release_if_covered_refuses_a_held_set() {
        use crate::staging::release::release_if_covered;
        let s = scene(1);
        let files = held_after_erase(&s, &s.keepsake, "NEW");
        let unit_id: i64 = scalar(&s.w.conn, "SELECT id FROM units WHERE name = 'keepsake'");

        // The only copy is the staging disk: zero copies against a floor of 1.
        let out = release_if_covered(&s.w.conn, &s.w.config, &[unit_id]).unwrap();
        assert!(out.cleaned.is_none(), "policy retains it");
        assert!(files.iter().all(|f| f.exists()));

        // A copy elsewhere, quick-confirmed: min_copies is met, the hold is not.
        copy_elsewhere(&s, &s.keepsake, &[1, 2], false, "OTHER");
        let out = release_if_covered(&s.w.conn, &s.w.config, &[unit_id]).unwrap();
        let cleaned = out.cleaned.expect("policy passed, so clean_staging ran");
        assert_eq!(
            cleaned.sets_cleaned, 0,
            "the hold refuses what policy allowed"
        );
        assert_eq!(cleaned.held_by_pending_rewrite.len(), 1);
        assert!(files.iter().all(|f| f.exists()));
        assert_eq!(set_status(&s.w.conn, s.keepsake.stage_set_id), "staged");

        // Once that copy has been fully read back, the hold is lifted and the
        // same call releases the set.
        s.w.conn
            .execute("UPDATE writes SET write_verified = 1", [])
            .unwrap();
        let out = release_if_covered(&s.w.conn, &s.w.config, &[unit_id]).unwrap();
        assert_eq!(out.cleaned.unwrap().sets_cleaned, 1);
        assert!(files.iter().all(|f| !f.exists()));
    }

    /// The advisor's trap, as a test: an ordinary two-cartridge compaction
    /// also leaves a staged set whose source is retired. With no rewrite
    /// record it is not held, and `--force` releases it as it always did.
    #[test]
    fn a_compaction_set_with_no_rewrite_record_is_not_held() {
        use crate::staging::clean::{clean_staging, CleanScope};
        let s = scene(1);
        let files = held_after_erase(&s, &s.keepsake, "NEW");
        s.w.conn
            .execute("DELETE FROM pending_rewrite_sets", [])
            .unwrap();
        s.w.conn
            .execute("DELETE FROM pending_rewrites", [])
            .unwrap();
        assert!(holds(&s.w.conn).unwrap().is_empty());
        let report = clean_staging(&s.w.conn, &s.w.config, true, CleanScope::Whole).unwrap();
        assert_eq!(report.sets_cleaned, 1);
        assert!(report.held_by_pending_rewrite.is_empty());
        assert!(files.iter().all(|f| !f.exists()));
    }

    #[test]
    fn snapshot_delete_still_refuses_a_held_sets_snapshot() {
        let s = scene(1);
        held_after_erase(&s, &s.keepsake, "NEW");
        let err = crate::cli::operations::snapshot_delete(&s.w.conn, "keepsake", 1, true, false)
            .expect_err("a snapshot with a completed write cannot be deleted");
        assert!(err.to_string().contains("completed write"), "{err}");
        assert_eq!(set_status(&s.w.conn, s.keepsake.stage_set_id), "staged");
    }

    // ── the staged slices, re-read from the disk ────────────────────────────

    #[test]
    fn verify_staged_names_a_slice_that_changed_on_the_disk() {
        let s = scene(1);
        let files = held_after_erase(&s, &s.keepsake, "NEW");
        let staged = |i: usize| StagedSlice {
            slice_id: s.keepsake.slice_ids[i],
            stage_set_id: s.keepsake.stage_set_id,
            path: files[i].clone(),
            prior: None,
        };
        let both = [staged(0), staged(1)];
        verify_staged(&s.w.conn, &both).expect("positive control: untouched files verify");

        let mut bytes = std::fs::read(&files[1]).unwrap();
        bytes[3] ^= 0x01;
        std::fs::write(&files[1], &bytes).unwrap();
        let msg = verify_staged(&s.w.conn, &both).unwrap_err().to_string();
        assert!(msg.contains("keepsake") && msg.contains("slice 2"), "{msg}");
        assert!(msg.contains("sha256"), "{msg}");

        bytes.pop();
        std::fs::write(&files[1], &bytes).unwrap();
        let msg = verify_staged(&s.w.conn, &both).unwrap_err().to_string();
        assert!(
            msg.contains("bytes"),
            "a short file is named by its size: {msg}"
        );

        std::fs::remove_file(&files[0]).unwrap();
        let msg = verify_staged(&s.w.conn, &both).unwrap_err().to_string();
        assert!(msg.contains("cannot be opened"), "{msg}");
    }

    // ── the preflight ───────────────────────────────────────────────────────

    /// Every refusal here is before the drive is touched and leaves the
    /// catalog, the tape and staging as they were.
    #[test]
    fn preflight_refuses_what_the_catalog_already_knows_cannot_work() {
        let unchanged = |s: &Scene| {
            assert_eq!(volume_status(&s.w.conn, "SRC"), "sealed");
            assert_eq!(
                scalar::<i64>(&s.w.conn, "SELECT COUNT(*) FROM pending_rewrites"),
                0
            );
            assert!(staged_files(&s.w.config).is_empty());
        };
        let refuse = |s: &Scene, source: &str, dest: &str| {
            let mut o = opts(source, dest);
            o.only_copy_on_staging = Some(source);
            let fake = Fake::new(s);
            let err = rewrite(&s.w.conn, &s.w.config, &fake, &o)
                .expect_err("must refuse")
                .to_string();
            assert!(
                fake.calls().is_empty(),
                "no drive step ran: {:?}",
                fake.calls()
            );
            err
        };

        let s = scene(1);
        assert!(refuse(&s, "SRC", "SRC").contains("is the volume being rewritten"));
        assert!(refuse(&s, "NOPE", "NEW").contains("NOPE"));
        add_volume(&s.w, "TAKEN", "initialized");
        assert!(refuse(&s, "SRC", "TAKEN").contains("already exists"));
        unchanged(&s);

        // Not sealed.
        s.w.conn
            .execute(
                "UPDATE volumes SET status = 'retired' WHERE label = 'SRC'",
                [],
            )
            .unwrap();
        assert!(refuse(&s, "SRC", "NEW").contains("not a sealed volume"));
        s.w.conn
            .execute(
                "UPDATE volumes SET status = 'sealed' WHERE label = 'SRC'",
                [],
            )
            .unwrap();
        // Quarantined.
        s.w.conn
            .execute(
                "UPDATE volumes SET observed_condition = 'quarantined' WHERE label = 'SRC'",
                [],
            )
            .unwrap();
        assert!(refuse(&s, "SRC", "NEW").contains("quarantined"));
        s.w.conn
            .execute(
                "UPDATE volumes SET observed_condition = 'ok' WHERE label = 'SRC'",
                [],
            )
            .unwrap();
        unchanged(&s);

        // Another live volume on the cartridge.
        let mate = add_volume(&s.w, "MATE", "sealed");
        s.w.conn
            .execute(
                "INSERT INTO cartridge_volumes (cartridge_id, volume_id) VALUES (?1, ?2)",
                params![s.cart, mate],
            )
            .unwrap();
        let msg = refuse(&s, "SRC", "NEW");
        assert!(
            msg.contains("also carries") && msg.contains("MATE"),
            "{msg}"
        );
        s.w.conn
            .execute(
                "UPDATE volumes SET status = 'erased' WHERE label = 'MATE'",
                [],
            )
            .unwrap();
        unchanged(&s);

        // No cartridge recorded.
        s.w.conn
            .execute(
                "DELETE FROM cartridge_volumes WHERE volume_id = ?1",
                params![s.src],
            )
            .unwrap();
        assert!(refuse(&s, "SRC", "NEW").contains("does not record which cartridge"));
        s.w.conn
            .execute(
                "INSERT INTO cartridge_volumes (cartridge_id, volume_id) VALUES (?1, ?2)",
                params![s.cart, s.src],
            )
            .unwrap();

        // The drive cannot write this generation: an LTO-5 drive, LTO-6 media.
        let mut lto5 = scene(1);
        lto5.w.config.backends.lto[0].generation = "LTO-4".into();
        let msg = refuse(&lto5, "SRC", "NEW");
        assert!(msg.to_lowercase().contains("cannot write"), "{msg}");
        unchanged(&lto5);

        // The live data does not fit one tape at the fill ceiling.
        let mut tiny = scene(1);
        tiny.w.config.backends.lto[0].fill_ceiling = 0.00001;
        assert!(refuse(&tiny, "SRC", "NEW").contains("does not fit one tape"));

        // Nothing live: every version is reclaimable.
        let none = scene(1);
        none.w
            .conn
            .execute("UPDATE snapshots SET status = 'reclaimable'", [])
            .unwrap();
        assert!(refuse(&none, "SRC", "NEW").contains("no live slices"));
    }

    #[test]
    fn preflight_refuses_a_staging_disk_without_room_before_a_block_is_read() {
        let s = scene(1);
        let _free = crate::staging::FreeSpaceOverride::set(10);
        let mut o = opts("SRC", "NEW");
        o.only_copy_on_staging = Some("SRC");
        let fake = Fake::new(&s);
        let msg = rewrite(&s.w.conn, &s.w.config, &fake, &o)
            .unwrap_err()
            .to_string();
        assert!(
            msg.contains("staging directory") && msg.contains("free"),
            "{msg}"
        );
        assert!(fake.calls().is_empty());
        assert_eq!(volume_status(&s.w.conn, "SRC"), "sealed");
    }

    #[test]
    fn an_unfinished_rewrite_of_the_volume_is_resumed_not_restarted() {
        let s = scene(1);
        held_after_erase(&s, &s.keepsake, "NEW");
        s.w.conn
            .execute(
                "UPDATE volumes SET status = 'sealed' WHERE id = ?1",
                params![s.src],
            )
            .unwrap();
        s.w.conn
            .execute("UPDATE pending_rewrites SET state = 'retired'", [])
            .unwrap();
        let mut o = opts("SRC", "ANOTHER");
        o.only_copy_on_staging = Some("SRC");
        let fake = Fake::new(&s);
        let msg = rewrite(&s.w.conn, &s.w.config, &fake, &o)
            .unwrap_err()
            .to_string();
        assert!(
            msg.contains("already under way") && msg.contains("--resume NEW"),
            "{msg}"
        );
        assert!(fake.calls().is_empty());
    }

    // ── the command: consent ────────────────────────────────────────────────

    /// Every live slice has a copy on another eligible volume, and with
    /// `min_copies` 2 the retirement leaves each version at one copy: the
    /// usual ADR-0008 Tier-2 gate. No terminal and no `--yes` refuses before
    /// anything is read; `--yes` confirms in advance.
    #[test]
    fn copies_elsewhere_below_policy_is_the_usual_tier_two_gate() {
        let s = scene(2);
        copy_elsewhere(&s, &s.keepsake, &[1, 2], true, "OTHER-A");
        copy_elsewhere(&s, &s.docs, &[3], true, "OTHER-B");

        // No terminal, no --yes: refuses, naming the version it would leave short.
        let fake = Fake::new(&s);
        let o = opts("SRC", "NEW");
        let msg = rewrite(&s.w.conn, &s.w.config, &fake, &o)
            .unwrap_err()
            .to_string();
        assert!(
            msg.contains("refused: non-interactive session")
                && msg.contains("--yes")
                && msg.contains("below its policy of 2"),
            "{msg}"
        );
        assert!(
            fake.calls().is_empty(),
            "refused before the read: {:?}",
            fake.calls()
        );
        assert_eq!(volume_status(&s.w.conn, "SRC"), "sealed");
        assert_eq!(
            scalar::<i64>(&s.w.conn, "SELECT COUNT(*) FROM pending_rewrites"),
            0
        );

        // --yes (or --force, which the CLI folds into the same flag).
        let mut o = opts("SRC", "NEW");
        o.assume_yes = true;
        let report = rewrite(&s.w.conn, &s.w.config, &fake, &o).unwrap();
        assert_eq!(report.consent, Consent::CopiesElsewhere);
        assert_eq!(fake.calls(), ["read", "erase SRC", "init NEW", "write NEW"]);
        assert_eq!(volume_status(&s.w.conn, "SRC"), "erased");
        assert_eq!(volume_status(&s.w.conn, "NEW"), "sealed");
        let (consent, outcome): (String, String) =
            s.w.conn
                .query_row("SELECT consent, outcome FROM pending_rewrites", [], |r| {
                    Ok((r.get(0)?, r.get(1)?))
                })
                .unwrap();
        assert_eq!(
            (consent.as_str(), outcome.as_str()),
            ("copies_elsewhere", "completed")
        );
        // The new volume carries the live versions and not the reclaimable one.
        let scope = fake.written_scope.borrow().clone().unwrap();
        assert_eq!(
            scope,
            vec![s.keepsake.stage_set_id, s.docs.stage_set_id],
            "only the rewrite's own sets, never `old` (reclaimable)"
        );
        assert_eq!(report.reclaimed_bytes, s.old.data[0].len() as i64);
        // OTHER-x + NEW = 2 copies: policy met, so staging is released.
        assert!(
            staged_files(&s.w.config).is_empty(),
            "{:?}",
            staged_files(&s.w.config)
        );
        assert_eq!(set_status(&s.w.conn, s.keepsake.stage_set_id), "cleaned");
    }

    #[test]
    fn copies_elsewhere_and_within_policy_asks_nothing() {
        let s = scene(1);
        copy_elsewhere(&s, &s.keepsake, &[1, 2], true, "OTHER-A");
        copy_elsewhere(&s, &s.docs, &[3], true, "OTHER-B");
        let fake = Fake::new(&s);
        let report = rewrite(&s.w.conn, &s.w.config, &fake, &opts("SRC", "NEW")).unwrap();
        assert_eq!(report.consent, Consent::CopiesElsewhere);
        assert_eq!(fake.calls().len(), 4);
    }

    /// Some live slice has no copy anywhere else: the amended floor. Not
    /// `--yes`, not `--force` (the CLI folds both into `assume_yes`), not a
    /// wrong label; before anything is read.
    #[test]
    fn a_single_copy_is_refused_without_the_typed_label_whatever_yes_says() {
        let s = scene(1);
        let fake = Fake::new(&s);

        let mut o = opts("SRC", "NEW");
        let msg = rewrite(&s.w.conn, &s.w.config, &fake, &o)
            .unwrap_err()
            .to_string();
        assert!(msg.contains("--only-copy-on-staging SRC"), "{msg}");
        assert!(
            msg.contains("unit \"keepsake\" v1") && msg.contains("unit \"docs\" v1"),
            "the units and versions at stake are named: {msg}"
        );
        assert!(msg.contains("THE WINDOW"), "{msg}");
        assert!(
            msg.contains("holds the only copy of these"),
            "the window sentence: {msg}"
        );
        assert!(
            msg.contains(&s.w.config.staging.directory),
            "the staging path is named: {msg}"
        );

        // --yes / --force (assume_yes) do not pass it.
        o.assume_yes = true;
        let msg = rewrite(&s.w.conn, &s.w.config, &fake, &o)
            .unwrap_err()
            .to_string();
        assert!(
            msg.contains("--yes") && msg.contains("do not give it"),
            "--yes does not stand in for the typed label: {msg}"
        );

        // A wrong label, even with --yes.
        o.only_copy_on_staging = Some("SRC2");
        let msg = rewrite(&s.w.conn, &s.w.config, &fake, &o)
            .unwrap_err()
            .to_string();
        assert!(
            msg.contains("\"SRC2\"")
                && msg.contains("\"SRC\"")
                && msg.contains("Nothing was changed"),
            "{msg}"
        );

        assert!(
            fake.calls().is_empty(),
            "no drive step ran: {:?}",
            fake.calls()
        );
        assert_eq!(volume_status(&s.w.conn, "SRC"), "sealed");
        assert_eq!(
            scalar::<i64>(&s.w.conn, "SELECT COUNT(*) FROM pending_rewrites"),
            0
        );
        assert!(staged_files(&s.w.config).is_empty());
        assert_eq!(
            scalar::<String>(&s.w.conn, "SELECT status FROM cartridges"),
            "in_use"
        );
    }

    #[test]
    fn a_single_copy_rewrite_runs_with_the_label_and_records_the_window() {
        let s = scene(1);
        let fake = Fake::new(&s);
        let mut o = opts("SRC", "NEW");
        o.only_copy_on_staging = Some("SRC");
        let report = rewrite(&s.w.conn, &s.w.config, &fake, &o).unwrap();

        assert_eq!(report.consent, Consent::TypedFlag);
        assert_eq!(
            fake.calls(),
            ["read", "erase SRC", "init NEW", "write NEW"],
            "read, erase, init, write, in that order"
        );
        assert_eq!(volume_status(&s.w.conn, "SRC"), "erased");
        assert_eq!(volume_status(&s.w.conn, "NEW"), "sealed");
        // The same cartridge: the one open mount is NEW's, on CART-1.
        let barcode: String =
            s.w.conn
                .query_row(
                    "SELECT c.barcode FROM cartridge_volumes cv
                 JOIN cartridges c ON c.id = cv.cartridge_id
                 JOIN volumes v ON v.id = cv.volume_id
                 WHERE v.label = 'NEW' AND cv.unmounted_at IS NULL",
                    [],
                    |r| r.get(0),
                )
                .unwrap();
        assert_eq!(barcode, "CART-1");
        // NEW is the only copy and it was fully read back: staging released.
        assert_eq!(set_status(&s.w.conn, s.keepsake.stage_set_id), "cleaned");
        assert!(staged_files(&s.w.config).is_empty());
        assert!(holds(&s.w.conn).unwrap().is_empty());

        // Every step is an event on the source volume, the window named in
        // both of its ends.
        for action in [
            "rewrite_started",
            "rewrite_staged_verified",
            "rewrite_consent",
            "retired",
            "rewrite_window_opened",
            "rewrite_erased",
            "rewrite_initialized",
            "rewrite_written",
            "rewrite_window_closed",
        ] {
            assert_eq!(events_of(&s.w.conn, action), 1, "event {action}");
        }
        let opened: String = scalar(
            &s.w.conn,
            "SELECT details FROM events WHERE action = 'rewrite_window_opened'",
        );
        assert!(
            opened.contains("holds the only copy of these") && opened.contains("\"NEW\""),
            "{opened}"
        );
        let consent: String = scalar(
            &s.w.conn,
            "SELECT details FROM events WHERE action = 'rewrite_consent'",
        );
        assert!(consent.contains("typed_flag"), "{consent}");
        let (state, window): (String, Option<String>) =
            s.w.conn
                .query_row(
                    "SELECT state, window_opened_at FROM pending_rewrites",
                    [],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .unwrap();
        assert_eq!(state, "written");
        assert!(window.is_some(), "the window's opening is stamped");
    }

    // ── the command: a refusal before the erase ─────────────────────────────

    #[test]
    fn a_staged_slice_that_does_not_match_its_hash_refuses_before_the_erase() {
        let s = scene(1);
        let fake = Fake::new(&s);
        fake.corrupt_a_staged_slice.set(true);
        let mut o = opts("SRC", "NEW");
        o.only_copy_on_staging = Some("SRC");
        let msg = rewrite(&s.w.conn, &s.w.config, &fake, &o)
            .unwrap_err()
            .to_string();

        assert!(msg.contains("not what the catalog recorded"), "{msg}");
        assert_eq!(fake.calls(), ["read"], "the erase never ran");
        // Nothing destructive: the source is sealed and its cartridge in use.
        assert_eq!(volume_status(&s.w.conn, "SRC"), "sealed");
        assert_eq!(
            scalar::<String>(&s.w.conn, "SELECT status FROM cartridges"),
            "in_use"
        );
        // Staging was put back: no file, the sets released as they were.
        assert!(
            staged_files(&s.w.config).is_empty(),
            "{:?}",
            staged_files(&s.w.config)
        );
        for v in [&s.keepsake, &s.docs] {
            assert_eq!(set_status(&s.w.conn, v.stage_set_id), "cleaned");
        }
        assert_eq!(
            scalar::<i64>(
                &s.w.conn,
                "SELECT COUNT(*) FROM stage_slices WHERE staging_path IS NOT NULL"
            ),
            0
        );
        // The hold was closed, not left behind.
        assert!(holds(&s.w.conn).unwrap().is_empty());
        let outcome: String = scalar(&s.w.conn, "SELECT outcome FROM pending_rewrites");
        assert_eq!(outcome, "abandoned");
        assert_eq!(events_of(&s.w.conn, "rewrite_abandoned"), 1);
        assert_eq!(events_of(&s.w.conn, "retired"), 0);
    }

    // ── the command: a failure after the erase ──────────────────────────────

    /// The failure that item 106 is about: the write fails after the
    /// cartridge was erased. The slices stay staged, held, with the command
    /// that resumes it named, and no release path will free them.
    #[test]
    fn a_write_that_fails_after_the_erase_leaves_the_slices_held_and_names_the_resume() {
        use crate::staging::clean::{clean_staging, CleanScope};
        let s = scene(1);
        let fake = Fake::new(&s);
        fake.fail_write.set(true);
        let mut o = opts("SRC", "NEW");
        o.only_copy_on_staging = Some("SRC");
        let err = rewrite(&s.w.conn, &s.w.config, &fake, &o).unwrap_err();

        // The error is the write's own, classified (interrupted: exit 4),
        // and names what finishes it.
        let msg = err.to_string();
        assert!(matches!(err, TapectlError::WriteInterrupted(_)), "{msg}");
        assert!(
            msg.contains("tapectl volume rewrite --resume NEW")
                && msg.contains("held")
                && msg.contains("`--force` included"),
            "{msg}"
        );
        assert_eq!(
            crate::error::WriteOutcome::of(&anyhow::Error::from(err)),
            Some(crate::error::WriteOutcome::Interrupted)
        );

        // The destructive part happened; the data did not leave staging.
        assert_eq!(volume_status(&s.w.conn, "SRC"), "erased");
        assert_eq!(volume_status(&s.w.conn, "NEW"), "initialized");
        let files = staged_files(&s.w.config);
        assert_eq!(files.len(), 3, "keepsake x2 and docs x1: {files:?}");
        for v in [&s.keepsake, &s.docs] {
            assert_eq!(set_status(&s.w.conn, v.stage_set_id), "staged");
        }
        let state: String = scalar(&s.w.conn, "SELECT state FROM pending_rewrites");
        assert_eq!(state, "initialized");
        assert_eq!(holds(&s.w.conn).unwrap().len(), 2);
        assert_eq!(events_of(&s.w.conn, "rewrite_stopped"), 1);

        // Nothing releases them: not `staging clean` (force or not), not the
        // release after a write, not the unit-scoped forms.
        let force = clean_staging(&s.w.conn, &s.w.config, true, CleanScope::Whole).unwrap();
        assert_eq!(force.sets_cleaned, 0, "{force:?}");
        assert_eq!(force.held_by_pending_rewrite.len(), 2);
        let unit_ids: Vec<i64> =
            s.w.conn
                .prepare("SELECT id FROM units WHERE name IN ('keepsake', 'docs')")
                .unwrap()
                .query_map([], |r| r.get(0))
                .unwrap()
                .collect::<std::result::Result<_, _>>()
                .unwrap();
        let out =
            crate::staging::release::release_if_covered(&s.w.conn, &s.w.config, &unit_ids).unwrap();
        assert!(out.cleaned.is_none_or(|c| c.sets_cleaned == 0));
        assert_eq!(
            staged_files(&s.w.config).len(),
            3,
            "every slice is still on the disk"
        );

        // Resume completes it and clears the hold only after the full confirm.
        fake.fail_write.set(false);
        let report = resume(&s.w.conn, &s.w.config, &fake, "NEW", false).unwrap();
        assert!(report.resumed);
        assert_eq!(
            fake.calls(),
            ["read", "erase SRC", "init NEW", "write NEW", "write NEW"],
            "the resume wrote; it did not read, erase or init again"
        );
        assert_eq!(volume_status(&s.w.conn, "NEW"), "sealed");
        assert!(holds(&s.w.conn).unwrap().is_empty());
        let outcome: String = scalar(&s.w.conn, "SELECT outcome FROM pending_rewrites");
        assert_eq!(outcome, "completed");
        assert!(
            staged_files(&s.w.config).is_empty(),
            "released after the readback"
        );
        assert_eq!(events_of(&s.w.conn, "rewrite_resumed"), 1);
        assert_eq!(events_of(&s.w.conn, "rewrite_window_closed"), 1);
    }

    #[test]
    fn a_quick_confirm_does_not_end_the_window() {
        let s = scene(1);
        let fake = Fake::new(&s);
        fake.quick_write.set(true);
        let mut o = opts("SRC", "NEW");
        o.only_copy_on_staging = Some("SRC");
        let err = rewrite(&s.w.conn, &s.w.config, &fake, &o)
            .unwrap_err()
            .to_string();
        assert!(err.contains("fully read back"), "{err}");
        assert_eq!(holds(&s.w.conn).unwrap().len(), 2, "still held");
        assert_eq!(staged_files(&s.w.config).len(), 3);
        assert_eq!(events_of(&s.w.conn, "rewrite_window_closed"), 0);

        // Resuming a sealed but not fully read back volume says what to run.
        let err = resume(&s.w.conn, &s.w.config, &fake, "NEW", false)
            .unwrap_err()
            .to_string();
        assert!(err.contains("volume verify NEW --full"), "{err}");

        // The full readback, by whichever command (here: `volume verify
        // --full` marking the writes verified), lifts the hold; resume
        // finishes without writing again.
        s.w.conn
            .execute(
                "UPDATE writes SET write_verified = 1 WHERE status = 'completed'",
                [],
            )
            .unwrap();
        assert!(holds(&s.w.conn).unwrap().is_empty());
        let calls_before = fake.calls().len();
        let report = resume(&s.w.conn, &s.w.config, &fake, "NEW", false).unwrap();
        assert!(report.resumed);
        assert_eq!(fake.calls().len(), calls_before, "no drive step ran");
        assert!(staged_files(&s.w.config).is_empty());
    }

    #[test]
    fn a_failed_erase_and_a_failed_init_are_resumed_from_where_they_stopped() {
        let s = scene(1);
        let fake = Fake::new(&s);
        fake.fail_erase.set(true);
        let mut o = opts("SRC", "NEW");
        o.only_copy_on_staging = Some("SRC");
        let err = rewrite(&s.w.conn, &s.w.config, &fake, &o).unwrap_err();
        // The erase may have touched the tape: interrupted, resumable.
        assert!(matches!(err, TapectlError::WriteInterrupted(_)), "{err}");
        assert!(err.to_string().contains("--resume NEW"));
        assert_eq!(volume_status(&s.w.conn, "SRC"), "retired");
        let state: String = scalar(&s.w.conn, "SELECT state FROM pending_rewrites");
        assert_eq!(state, "retired");
        assert_eq!(holds(&s.w.conn).unwrap().len(), 2);

        // A different cartridge in the drive is refused, with nothing run.
        *fake.serial.borrow_mut() = Some("SOME-OTHER-TAPE".into());
        fake.fail_erase.set(false);
        let before = fake.calls().len();
        let err = resume(&s.w.conn, &s.w.config, &fake, "NEW", false)
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("SOME-OTHER-TAPE") && err.contains(SERIAL),
            "both serials are named: {err}"
        );
        assert_eq!(fake.calls().len(), before);
        *fake.serial.borrow_mut() = None;
        assert!(resume(&s.w.conn, &s.w.config, &fake, "NEW", false)
            .unwrap_err()
            .to_string()
            .contains("could not be read"));

        // The init fails once; the erase is not repeated.
        *fake.serial.borrow_mut() = Some(SERIAL.into());
        fake.fail_init.set(true);
        let err = resume(&s.w.conn, &s.w.config, &fake, "NEW", false).unwrap_err();
        assert!(err.to_string().contains("--resume NEW"));
        assert_eq!(volume_status(&s.w.conn, "SRC"), "erased");
        let state: String = scalar(&s.w.conn, "SELECT state FROM pending_rewrites");
        assert_eq!(state, "erased");
        fake.fail_init.set(false);
        let report = resume(&s.w.conn, &s.w.config, &fake, "NEW", false).unwrap();
        assert!(report.resumed);
        let calls = fake.calls();
        assert_eq!(calls.iter().filter(|c| c.starts_with("erase")).count(), 2);
        assert_eq!(volume_status(&s.w.conn, "NEW"), "sealed");
        assert!(holds(&s.w.conn).unwrap().is_empty());
    }

    #[test]
    fn resume_has_nothing_to_do_without_a_pending_rewrite() {
        let s = scene(1);
        let fake = Fake::new(&s);
        let msg = resume(&s.w.conn, &s.w.config, &fake, "NEW", false)
            .unwrap_err()
            .to_string();
        assert!(msg.contains("no unfinished rewrite"), "{msg}");

        // A record that never got past staging has nothing destructive to resume.
        held_after_erase(&s, &s.keepsake, "NEW");
        s.w.conn
            .execute("UPDATE pending_rewrites SET state = 'staged'", [])
            .unwrap();
        let msg = resume(&s.w.conn, &s.w.config, &fake, "NEW", false)
            .unwrap_err()
            .to_string();
        assert!(msg.contains("nothing is lost"), "{msg}");
    }

    // ── what the new volume carries ─────────────────────────────────────────

    #[test]
    fn the_new_volume_carries_only_the_rewrites_slices_unless_with_staged() {
        let build = || {
            let s = scene(1);
            let extra = add_version(&s.w, "extra", 1, "current", &data("extra", 1));
            s.w.conn
                .execute(
                    "UPDATE stage_sets SET status = 'staged' WHERE id = ?1",
                    params![extra.stage_set_id],
                )
                .unwrap();
            (s, extra)
        };
        let (s, _extra) = build();
        let fake = Fake::new(&s);
        let mut o = opts("SRC", "NEW");
        o.only_copy_on_staging = Some("SRC");
        rewrite(&s.w.conn, &s.w.config, &fake, &o).unwrap();
        let scope = fake.written_scope.borrow().clone();
        assert_eq!(
            scope,
            Some(vec![s.keepsake.stage_set_id, s.docs.stage_set_id]),
            "an unrelated staged set is not written"
        );

        let (s, _extra) = build();
        let fake = Fake::new(&s);
        let mut o = opts("SRC", "NEW");
        o.only_copy_on_staging = Some("SRC");
        o.with_staged = true;
        rewrite(&s.w.conn, &s.w.config, &fake, &o).unwrap();
        assert_eq!(
            *fake.written_scope.borrow(),
            None,
            "--with-staged: no scope"
        );
    }

    #[test]
    fn keep_staging_keeps_the_slices_after_the_full_readback() {
        let s = scene(1);
        let fake = Fake::new(&s);
        let mut o = opts("SRC", "NEW");
        o.only_copy_on_staging = Some("SRC");
        o.keep_staging = true;
        let report = rewrite(&s.w.conn, &s.w.config, &fake, &o).unwrap();
        assert!(matches!(
            report.release,
            Some(crate::cli::volume::StagingRelease::Kept)
        ));
        assert_eq!(staged_files(&s.w.config).len(), 3);
        // The window is closed all the same: the hold lifted by the readback.
        assert!(holds(&s.w.conn).unwrap().is_empty());
    }

    // ── every other retire path keeps the absolute floor ────────────────────

    /// The amendment is for `volume rewrite` alone. On the same single-copy
    /// volume, `volume retire`, `volume compact-finish`, `cartridge retire`
    /// and `cartridge mark-erased` refuse outright, `--yes` and `--force`
    /// both given, and change nothing.
    #[test]
    fn every_other_retire_path_still_refuses_a_single_copy_volume() {
        let s = scene(1);
        let c = &s.w.conn;
        let msg = crate::cli::operations::volume_retire(c, &s.w.config, "SRC", true, false, false)
            .unwrap_err()
            .to_string();
        assert!(msg.contains("LAST eligible copy"), "volume retire: {msg}");
        let msg = write::compact_finish(c, &s.w.config, "SRC", true)
            .unwrap_err()
            .to_string();
        assert!(
            msg.contains("no copy on another volume"),
            "compact-finish: {msg}"
        );
        let msg = crate::cli::operations::cartridge_retire(
            c,
            &s.w.config,
            "CART-1",
            None,
            true,
            true,
            false,
            false,
        )
        .unwrap_err()
        .to_string();
        assert!(
            msg.contains("LAST eligible copy"),
            "cartridge retire: {msg}"
        );
        let msg =
            crate::cli::operations::cartridge_mark_erased(c, "CART-1", true, true, false, false)
                .unwrap_err()
                .to_string();
        assert!(
            msg.contains("LAST eligible copy"),
            "cartridge mark-erased: {msg}"
        );

        assert_eq!(volume_status(c, "SRC"), "sealed");
        assert_eq!(
            scalar::<String>(c, "SELECT status FROM cartridges"),
            "in_use"
        );
        assert_eq!(scalar::<i64>(c, "SELECT COUNT(*) FROM pending_rewrites"), 0);
    }

    // ── everything knowable is asked before anything is destroyed ───────────

    #[test]
    fn the_host_and_the_write_are_asked_about_before_the_read_and_the_retire() {
        let s = scene(1);
        let fake = Fake::new(&s);
        let mut o = opts("SRC", "NEW");
        o.only_copy_on_staging = Some("SRC");
        rewrite(&s.w.conn, &s.w.config, &fake, &o).unwrap();
        assert_eq!(
            *fake.asked.borrow(),
            ["host volume rewrite \"SRC\"", "write_preflight NEW scoped"],
            "the quiet-host question once, and the write's catalog checks for the \
             rewrite's own sets"
        );

        let s = scene(1);
        let fake = Fake::new(&s);
        let mut o = opts("SRC", "NEW");
        o.only_copy_on_staging = Some("SRC");
        o.with_staged = true;
        rewrite(&s.w.conn, &s.w.config, &fake, &o).unwrap();
        assert_eq!(fake.asked.borrow()[1], "write_preflight NEW all");
    }

    /// A write that would be refused for something the catalog and the staged
    /// files already show — a missing key, a staged set not whole, no escrow
    /// recipient — is found after the read and before the retire, with
    /// staging put back.
    #[test]
    fn a_write_that_would_be_refused_is_found_before_anything_is_destroyed() {
        let s = scene(1);
        let fake = Fake::new(&s);
        fake.fail_write_preflight.set(true);
        let mut o = opts("SRC", "NEW");
        o.only_copy_on_staging = Some("SRC");
        let msg = rewrite(&s.w.conn, &s.w.config, &fake, &o)
            .unwrap_err()
            .to_string();
        assert!(
            msg.contains("would be refused") && msg.contains("Nothing destructive"),
            "{msg}"
        );
        assert_eq!(fake.calls(), ["read"]);
        assert_eq!(volume_status(&s.w.conn, "SRC"), "sealed");
        assert!(staged_files(&s.w.config).is_empty());
        assert!(holds(&s.w.conn).unwrap().is_empty());
        assert_eq!(events_of(&s.w.conn, "retired"), 0);
    }

    #[test]
    fn the_cartridge_in_the_drive_must_be_readable_and_the_sources_before_the_read() {
        let s = scene(1);
        let fake = Fake::new(&s);
        let mut o = opts("SRC", "NEW");
        o.only_copy_on_staging = Some("SRC");

        *fake.serial.borrow_mut() = None;
        let msg = rewrite(&s.w.conn, &s.w.config, &fake, &o)
            .unwrap_err()
            .to_string();
        assert!(
            msg.contains("chip serial") && msg.contains("could not be read"),
            "{msg}"
        );

        *fake.serial.borrow_mut() = Some("SOMEONE-ELSES".into());
        let msg = rewrite(&s.w.conn, &s.w.config, &fake, &o)
            .unwrap_err()
            .to_string();
        assert!(
            msg.contains("SOMEONE-ELSES") && msg.contains(SERIAL) && msg.contains("CART-1"),
            "{msg}"
        );
        assert!(
            fake.calls().is_empty(),
            "no drive step ran: {:?}",
            fake.calls()
        );
        assert_eq!(volume_status(&s.w.conn, "SRC"), "sealed");
    }

    #[test]
    fn the_with_staged_fit_check_counts_the_other_staged_data() {
        let s = scene(1);
        let extra = add_version(&s.w, "extra", 1, "current", &data("extra", 3));
        s.w.conn
            .execute(
                "UPDATE stage_sets SET status = 'staged' WHERE id = ?1",
                params![extra.stage_set_id],
            )
            .unwrap();
        let bytes = |v: &Version| v.data.iter().map(Vec::len).sum::<usize>();
        let live = bytes(&s.keepsake) + bytes(&s.docs);
        let extra_bytes = bytes(&extra);
        // A tape that takes the live data and half of the extra data (the
        // volume's capacity is 100 MB in this catalog).
        let mut c = s.w.config.clone();
        c.backends.lto[0].fill_ceiling = (live + extra_bytes / 2) as f64 / 100_000_000.0;

        let mut o = opts("SRC", "NEW");
        o.only_copy_on_staging = Some("SRC");
        plan(&s.w.conn, &c, &o).expect("the live data alone fits");
        o.with_staged = true;
        let msg = plan(&s.w.conn, &c, &o).unwrap_err().to_string();
        assert!(
            msg.contains("--with-staged adds") && msg.contains("does not fit one tape"),
            "{msg}"
        );
    }
}
