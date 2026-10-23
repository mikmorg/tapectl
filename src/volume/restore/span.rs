//! A version read across cartridges (#478, tapectl 1.7.0;
//! `volume-format-v2.md` §4.4, ADR-0012 items 89 and 131).
//!
//! A spanning version's slices are held as ranges on several volumes, and a
//! tape that holds a range restores the version only together with tapes
//! that hold its other slices (R7). `restore unit` and `restore file` of such
//! a version are planned here, before the first cartridge is asked for:
//!
//! - **The start.** The restore starts on the cartridge it was named
//!   (`--from`) or found loaded, and that cartridge must hold slice 1: a
//!   range that starts at slice 1 (a tape that holds the version whole is
//!   never routed here; it restores alone, as in 1.6.0). Otherwise it is
//!   refused, naming the cartridges that hold slice 1.
//! - **Every slice somewhere.** A version with any slice on no live volume
//!   is refused, naming the slices and where the catalog last saw them.
//! - **The fewest cartridges, in slice order.** From the start, each next
//!   cartridge is the holder of the next slice that reads furthest, a live
//!   volume that holds the version whole first; a quarantined holder only
//!   when no volume in service holds the slice (the last resort, as in the
//!   repair of a damaged slice). No cartridge is left while it holds the next
//!   slice.
//!
//! Who holds a slice is [`slice_holders::holders_of_set`]'s answer, the same
//! test the repair of a damaged slice asks ([`slice_holders::holders_of`]):
//! a `completed` or `range` write with the slice `written`, at or before the
//! tape's end as the catalog recorded its seal (§4.2 R1 and R5), on a live
//! volume. The plan is the restore's positions, one per slice in slice order,
//! each naming the volume it is read from; the supply asks for each next
//! cartridge as it comes to it.

use std::collections::{BTreeMap, HashMap};

use rusqlite::{params, Connection};

use super::WritePositionInfo;
use crate::error::{Result, TapectlError};
use crate::volume::slice_holders::{self, Holder, Standing};

/// A run of slices a restore reads from one volume: one cartridge of its
/// plan.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Run {
    pub volume_id: i64,
    pub first: i64,
    pub last: i64,
}

impl Run {
    /// `slice 4`, or `slices 4 to 6`.
    pub(crate) fn describe(&self) -> String {
        slices_phrase(self.first, self.last)
    }
}

/// `slice 4`, or `slices 4 to 6`.
pub(crate) fn slices_phrase(first: i64, last: i64) -> String {
    if first == last {
        format!("slice {first}")
    } else {
        format!("slices {first} to {last}")
    }
}

/// The cartridges `positions` are read from, in order: each run of
/// consecutive positions on one volume. One run for a version held whole.
pub(crate) fn runs(positions: &[WritePositionInfo]) -> Vec<Run> {
    let mut runs: Vec<Run> = Vec::new();
    for wp in positions {
        match runs.last_mut() {
            Some(run) if run.volume_id == wp.volume_id => run.last = wp.slice_number,
            _ => runs.push(Run {
                volume_id: wp.volume_id,
                first: wp.slice_number,
                last: wp.slice_number,
            }),
        }
    }
    runs
}

/// `2, 7 to 9`: slice numbers, ascending, as runs.
fn numbers_phrase(numbers: &[i64]) -> String {
    let mut parts: Vec<String> = Vec::new();
    let mut i = 0;
    while i < numbers.len() {
        let mut j = i;
        while j + 1 < numbers.len() && numbers[j + 1] == numbers[j] + 1 {
            j += 1;
        }
        parts.push(if i == j {
            numbers[i].to_string()
        } else {
            format!("{} to {}", numbers[i], numbers[j])
        });
        i = j + 1;
    }
    let noun = if numbers.len() == 1 {
        "slice"
    } else {
        "slices"
    };
    format!("{noun} {}", parts.join(", "))
}

/// One slice of the stage set, as `stage create` recorded it.
struct SliceRow {
    number: i64,
    sha256_plain: String,
    sha256_encrypted: String,
    encrypted_bytes: i64,
}

/// Plan the restore of `stage_set_id` (`unit_name` version `version`) that
/// starts on the volume `start_label`, which holds `start`: its written
/// slices of the set, a range, in tape order. Returns every slice of the set,
/// in slice order, each with the volume and tape file it is read from; or
/// the refusal, before any cartridge is asked for. See the module doc.
pub(super) fn plan_across(
    conn: &Connection,
    unit_name: &str,
    version: i64,
    stage_set_id: i64,
    start_label: &str,
    start: Vec<WritePositionInfo>,
) -> Result<Vec<WritePositionInfo>> {
    if start.is_empty() {
        return Err(TapectlError::Other(format!(
            "no data for unit \"{unit_name}\" on volume \"{start_label}\""
        )));
    }
    let slices = slices_of(conn, stage_set_id)?;
    let on_start: BTreeMap<i64, WritePositionInfo> =
        start.into_iter().map(|wp| (wp.slice_number, wp)).collect();

    // Every live holder of every slice, by volume.
    let held = slice_holders::holders_of_set(conn, stage_set_id)?;
    let mut by_slice: BTreeMap<i64, Vec<Holder>> = BTreeMap::new();
    let mut by_volume: HashMap<i64, BTreeMap<i64, Holder>> = HashMap::new();
    for h in held {
        by_volume
            .entry(h.holder.volume_id)
            .or_default()
            .insert(h.slice_number, h.holder.clone());
        by_slice.entry(h.slice_number).or_default().push(h.holder);
    }

    // Every slice somewhere, before anything else is said.
    let missing: Vec<i64> = slices
        .iter()
        .map(|s| s.number)
        .filter(|n| !on_start.contains_key(n) && !by_slice.contains_key(n))
        .collect();
    if !missing.is_empty() {
        return Err(missing_refusal(
            conn,
            unit_name,
            version,
            stage_set_id,
            &missing,
        )?);
    }

    // The start holds slice 1.
    if !on_start.contains_key(&1) {
        return Err(start_refusal(
            conn,
            unit_name,
            version,
            start_label,
            &on_start,
            by_slice.get(&1).map(Vec::as_slice).unwrap_or_default(),
        ));
    }

    let n = slices.len() as i64;
    let row = |number: i64| slices.iter().find(|s| s.number == number);
    let mut plan: Vec<WritePositionInfo> = Vec::with_capacity(slices.len());
    let mut next = 1;
    // The start cartridge, for as long as it holds the next slice.
    while let Some(wp) = on_start.get(&next) {
        plan.push(wp.clone());
        next += 1;
    }
    while next <= n {
        let candidates = by_slice.get(&next).map(Vec::as_slice).unwrap_or_default();
        // How far a volume reads on from `next`, and whether it holds every
        // slice of the version.
        let reach = |volume_id: i64| -> i64 {
            let held = &by_volume[&volume_id];
            let mut last = next;
            while held.contains_key(&(last + 1)) {
                last += 1;
            }
            last
        };
        let whole = |volume_id: i64| by_volume[&volume_id].len() as i64 == n;
        let Some(pick) = candidates.iter().min_by_key(|h| {
            (
                h.standing != Standing::InService,
                !whole(h.volume_id),
                std::cmp::Reverse(reach(h.volume_id)),
                h.volume_id,
            )
        }) else {
            // The missing check above makes this unreachable; said by name
            // rather than read past.
            return Err(missing_refusal(
                conn,
                unit_name,
                version,
                stage_set_id,
                &[next],
            )?);
        };
        let last = reach(pick.volume_id);
        let held = &by_volume[&pick.volume_id];
        for number in next..=last {
            let (Some(s), Some(h)) = (row(number), held.get(&number)) else {
                // `reach` counts only slices the volume holds; said by name
                // rather than planned around if a catalog ever disagrees.
                return Err(missing_refusal(
                    conn,
                    unit_name,
                    version,
                    stage_set_id,
                    &[number],
                )?);
            };
            plan.push(WritePositionInfo {
                stage_set_id,
                volume_id: h.volume_id,
                slice_number: number,
                position: h.position.to_string(),
                sha256_plain: s.sha256_plain.clone(),
                sha256_encrypted: s.sha256_encrypted.clone(),
                encrypted_bytes: s.encrypted_bytes,
            });
        }
        next = last + 1;
    }
    Ok(plan)
}

/// The stage set's slices, in slice order.
fn slices_of(conn: &Connection, stage_set_id: i64) -> Result<Vec<SliceRow>> {
    let mut stmt = conn.prepare(
        "SELECT slice_number, sha256_plain, sha256_encrypted, encrypted_bytes
           FROM stage_slices WHERE stage_set_id = ?1
          ORDER BY slice_number",
    )?;
    let rows = stmt
        .query_map(params![stage_set_id], |r| {
            Ok(SliceRow {
                number: r.get(0)?,
                sha256_plain: r.get(1)?,
                sha256_encrypted: r.get(2)?,
                encrypted_bytes: r.get(3)?,
            })
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    Ok(rows)
}

/// `L6-0001 (cartridge LTO6-0001, chip serial HUM1)`, and a quarantined
/// holder said to be the last resort.
fn holder_phrase(conn: &Connection, h: &Holder) -> String {
    format!(
        "\"{}\" ({}{})",
        h.label,
        slice_holders::shelf_of(conn, h.volume_id, &h.label).describe(),
        if h.standing == Standing::Quarantined {
            "; quarantined, a last resort"
        } else {
            ""
        }
    )
}

/// The refusal for a restore started on a cartridge that does not hold the
/// version's slice 1 (§4.4: the restore starts on the tape holding slice 1).
fn start_refusal(
    conn: &Connection,
    unit_name: &str,
    version: i64,
    start_label: &str,
    on_start: &BTreeMap<i64, WritePositionInfo>,
    slice_one: &[Holder],
) -> TapectlError {
    let here = match (on_start.keys().next(), on_start.keys().next_back()) {
        (Some(&first), Some(&last)) => slices_phrase(first, last),
        _ => "none of the slices".to_string(),
    };
    let holders: Vec<String> = slice_one.iter().map(|h| holder_phrase(conn, h)).collect();
    let first = slice_one
        .first()
        .map(|h| h.label.as_str())
        .unwrap_or_default();
    TapectlError::Other(format!(
        "unit \"{unit_name}\" version {version} is held across cartridges, and a restore of it \
         starts on the cartridge that holds its slice 1, then reads the others in slice order \
         (volume-format-v2.md §4.4): volume \"{start_label}\" holds only {here} of it. Slice 1 \
         is on {}: start the restore there (`--from {first}`, or load it and give no --from). \
         Nothing was read from tape.",
        holders.join(", ")
    ))
}

/// The refusal for a version with slices on no live volume (ADR-0012 items
/// 89 and 131): which slices, where the catalog last saw them, and what
/// would bring them back.
fn missing_refusal(
    conn: &Connection,
    unit_name: &str,
    version: i64,
    stage_set_id: i64,
    missing: &[i64],
) -> Result<TapectlError> {
    let traces = slice_holders::traces_of_set(conn, stage_set_id)?;
    // What each volume the catalog saw a missing slice on says of it, in
    // volume order of first mention.
    let mut seen: Vec<(String, String, Vec<i64>)> = Vec::new();
    for t in traces.iter().filter(|t| missing.contains(&t.slice_number)) {
        let why = if t.volume_status == "retired" || t.volume_status == "erased" {
            format!("is {}", t.volume_status)
        } else if t.position_status == "pending" {
            "planned it, but its tape ended before it".to_string()
        } else if t.write_status == "cut" {
            "holds it in a write the end of its tape cut, which is never read".to_string()
        } else if t.write_status != "completed" && t.write_status != "range" {
            format!("holds it in a write that is {}", t.write_status)
        } else if t.position_status != "written" {
            format!("records it {}", t.position_status)
        } else {
            format!(
                "is not in service (status {}, condition {})",
                t.volume_status, t.condition
            )
        };
        match seen
            .iter_mut()
            .find(|(label, w, _)| *label == t.label && *w == why)
        {
            Some((_, _, numbers)) => {
                if !numbers.contains(&t.slice_number) {
                    numbers.push(t.slice_number)
                }
            }
            None => seen.push((t.label.clone(), why, vec![t.slice_number])),
        }
    }
    let saw = if seen.is_empty() {
        " The catalog records no tape that ever held them.".to_string()
    } else {
        format!(
            " Where the catalog saw them: {}.",
            seen.iter()
                .map(|(label, why, numbers)| format!(
                    "\"{label}\" {why} ({})",
                    numbers_phrase(numbers)
                ))
                .collect::<Vec<_>>()
                .join("; ")
        )
    };
    let placeholders = missing
        .iter()
        .map(i64::to_string)
        .collect::<Vec<_>>()
        .join(",");
    let staged: i64 = conn.query_row(
        &format!(
            "SELECT COUNT(*) FROM stage_slices
              WHERE stage_set_id = ?1 AND slice_number IN ({placeholders})
                AND staging_path IS NOT NULL AND released_at IS NULL"
        ),
        params![stage_set_id],
        |r| r.get(0),
    )?;
    let next = if staged == missing.len() as i64 {
        " They are still staged: an ordinary `tapectl volume write` writes them onto another \
         cartridge, and the restore can then read them."
    } else {
        " A tape this catalog does not know that holds them is brought into it by `tapectl \
         catalog rebuild --from-volume`."
    };
    Ok(TapectlError::Other(format!(
        "unit \"{unit_name}\" version {version} cannot be restored: {} of it {} on no live \
         volume, and a restore of a version held across cartridges needs every slice, in slice \
         order (volume-format-v2.md §4.4 R7).{saw}{next} Nothing was read from tape.",
        numbers_phrase(missing),
        if missing.len() == 1 { "is" } else { "are" },
    )))
}

/// The refusal for a restore across cartridges with nobody at the terminal
/// to change them (#478): `subject` is held on `runs`, and tapectl swaps no
/// cartridge without a person (`--yes` cannot load one).
pub(super) fn unattended_refusal(conn: &Connection, subject: &str, runs: &[Run]) -> TapectlError {
    let cartridges: Vec<String> = runs
        .iter()
        .map(|r| {
            let label: String = conn
                .query_row(
                    "SELECT label FROM volumes WHERE id = ?1",
                    params![r.volume_id],
                    |row| row.get(0),
                )
                .unwrap_or_else(|_| format!("volume {}", r.volume_id));
            format!(
                "{} from \"{label}\" ({})",
                r.describe(),
                slice_holders::shelf_of(conn, r.volume_id, &label).describe()
            )
        })
        .collect();
    TapectlError::Other(format!(
        "{subject} is held across cartridges, and this restore reads {}. tapectl swaps no \
         cartridge without a person at the terminal, and --yes cannot load one: run it from a \
         terminal and it asks for each cartridge in turn. Nothing was read from tape but File 0.",
        cartridges.join(", then ")
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wp(volume_id: i64, slice_number: i64) -> WritePositionInfo {
        WritePositionInfo {
            stage_set_id: 1,
            volume_id,
            slice_number,
            position: (slice_number + 3).to_string(),
            sha256_plain: String::new(),
            sha256_encrypted: String::new(),
            encrypted_bytes: 0,
        }
    }

    #[test]
    fn runs_group_consecutive_slices_of_one_volume() {
        let plan = [wp(1, 1), wp(1, 2), wp(2, 3), wp(3, 4), wp(3, 5)];
        let got: Vec<(i64, String)> = runs(&plan)
            .iter()
            .map(|r| (r.volume_id, r.describe()))
            .collect();
        assert_eq!(
            got,
            vec![
                (1, "slices 1 to 2".to_string()),
                (2, "slice 3".to_string()),
                (3, "slices 4 to 5".to_string()),
            ]
        );
        assert!(runs(&[]).is_empty());
    }

    #[test]
    fn slice_numbers_read_as_runs() {
        assert_eq!(numbers_phrase(&[7]), "slice 7");
        assert_eq!(numbers_phrase(&[2, 7, 8, 9]), "slices 2, 7 to 9");
        assert_eq!(numbers_phrase(&[1, 2]), "slices 1 to 2");
    }
}
