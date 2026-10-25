//! Native tape consumed per byte of data a `volume write` sent (issue #338,
//! ruled 2026-09-23, grilling Q1; the guard #323 asked for).
//!
//! An LTO drive that cannot get data fast enough — or gets it in bursts —
//! stops, backs up and restarts, and every stop-start burns tape. On the
//! real HP LTO-6 a bursty `openssl` pipe used **1.48** bytes of native
//! capacity per byte of data; tapectl's own steady write loop used
//! **1.000** (`docs/runs/2026-09-23-lto6-capacity-measurement.md`, runs 1
//! and 3) and a steady 155 MB/s fill used **1.0047**
//! (`docs/runs/2026-09-23-real-drive-rehearsal.md`, "The end-of-tape
//! fill"). The ratio is therefore a direct, after-the-fact reading of
//! whether the host fed the drive evenly — and it is computable from what
//! tapectl already journals, with no extra read of the drive.
//!
//! **Numerator: page 0x0c "Native capacity from BOP to EOD", not page
//! 0x17.** Page 0x17's "Total used native capacity [MB]" agrees with it on a
//! rewound tape, but is PARTIAL when read at EOD before a rewind — and the
//! write's post-command sweep runs wherever the store left the head, which
//! after a sealed write is at EOD. 0x0c's BOP→EOD is position-independent:
//! it describes the recorded extent of the tape, not where the head is.
//! Decimal MB (10^6), as sg_logs prints it (ADR-0012: cartridge capacities
//! are decimal).
//!
//! **Denominator: the write's whole Layout, block-padded**
//! ([`crate::volume::layout_model::Layout::on_tape_bytes`]) — every file
//! from the ID thunk to the seal marker, each rounded up to a whole block,
//! which is exactly what the fixed-block driver sends. Run 3 measured
//! 21,488,398,724 bytes as the process's `wchar` over all 14 volume files
//! against 21,487 MB of native capacity, and that pair computes to 1.000
//! here ([`tests::the_run_3_measurement_computes_to_one`]). NOT
//! `volumes.bytes_written`, which is slices only and would inflate the
//! ratio.
//!
//! **It warns; it never refuses and never gates.** The write is already
//! complete and sealed when this runs, and a ratio above the threshold
//! changes nothing about the volume — it tells the operator the host was
//! not quiet (`docs/operator-guide.md`, "A quiet host while the tape
//! runs") and that the next tape will hold less than planned if that
//! continues. The exit code is untouched.
//!
//! **Recorded as an `events` row, not a `health_logs` column.** ADR-0013 §3
//! rules that a derived figure does not become a column beside the facts
//! it derives from ("how the two disagree later"). Both facts are already
//! durable and queryable — the 0x0c decode in `log_page_journal` against
//! the write's contact, the layout on the write — and the ratio is what
//! tapectl SAID about them at the time, which is the audit trail's job.
//! The row is written whenever the ratio is computable, not only when it
//! warns, so run 3's 1.000 is as queryable as a 1.48, and it carries the
//! threshold it was judged against, so a later change to the constant does
//! not rewrite history. `report events` lists it with no change: the
//! action, field and value columns already show it.
//!
//! **Every consumer reads the journal** (migration 023): the 0x0c text is
//! read back from `log_page_journal` by contact id, never from the drive
//! and never from a second sweep. A drive that does not list page 0x0c, or
//! a decode without the line (the no-medium capture has none), makes the
//! ratio `None` — nothing recorded, nothing warned.
//!
//! **Beside it, the drive's own wear figure** (issue #488). An HP drive's
//! page 0x34, parameter 0, counts "tape direction changes per 100 MB
//! written" ([`crate::tape::log_pages::HpPerformance`]). The same sweep that
//! journalled the 0x0c this ratio comes from journalled 0x34 too, so the
//! row's `details` carries it as `repositions_per_100mb` whenever the drive
//! is HP and the page was read; the key is ABSENT otherwise (another
//! vendor's page 0x34 means something else). It is what the drive said after
//! the whole command, confirm's rewinds included: see the page's own
//! documentation for what is and is not known about the figure.
//!
//! **Recording and warning are split** ([`Suppression`]). mhvtl DOES list
//! page 0x0c and reports a BOP→EOD of 500 MB beside "0 GB written" — a
//! figure with no relation to any write, which against a 30 MB gate write
//! is a 16x ratio. On a drive with a `capacity_override` (ADR-0010: virtual
//! drives and the microcosm harnesses, nothing else — a drive that lies
//! about capacity cannot be read for a capacity ratio) the row is still
//! RECORDED, so the gate exercises the whole path and a reader can see the
//! 16x, but the warning is suppressed and the row's `details` says why
//! (`"suppressed": "capacity_override"`). A real drive has no override and
//! warns.
//!
//! **The warning follows the drive's evidence** (ADR-0012 item 170). The
//! 2026-10-22 drive phase (`docs/runs/2026-10-22-488-drive-phase.md`,
//! findings 2-4) wrote 21.5 GB eleven times: the three runs above 1.01 came
//! with page 0x02's "Total times correction algorithm processed" rising by
//! 20,728 to 153,609 while the feed was steady and page 0x34 read 0
//! repositions, and the clean runs raised it by 22 to 141. So the warning
//! names the host only when the drive reports repositions (HP page 0x34
//! above 0, [`Cause::Repositions`]); with none (0, or no figure) and a jump
//! in page 0x02's corrections across the write, it names the drive or the
//! cartridge and suggests a cleaning ([`Cause::Corrections`]); with neither
//! it states the ratio and names no cause ([`Cause::Unattributed`]).
//!
//! **The corrections across the write come from the journal, never a second
//! read** ([`corrections_across_write`]). ADR-0013 reads each page at most
//! once per contact, so the write's own contact holds one page 0x02, read
//! after the write. HP: "All these counters are updated when the data set is
//! physically written. They relate to the current tape and are cleared when
//! the tape is unloaded" (HP-V3 p63; the 2026-09-23 captures agree: the
//! no-medium page 0x02 still held the last tape's counts, and the page read
//! all zeros with a cartridge just loaded that had 32 GB written in its
//! life). Within one load they accumulate, so the write's figure is its
//! reading less the reading of the previous contact on the same drive in the
//! same load (same cartridge, same MAM load count) — `volume init`'s sweep for
//! `volume write --init` — or less zero when the write is the first contact
//! of its load on an HP drive. Anything else is not known, and no correction
//! cause is named.

use rusqlite::{Connection, OptionalExtension};
use tracing::warn;

use crate::db::events;
use crate::error::Result;

/// Warn when the drive used more than this many bytes of native capacity
/// per byte of data sent.
///
/// The measurements it sits between, all on the HP LTO-6 `HUJ808A5L4`:
/// - bursty `openssl enc` pipe, 94.8 MB/s: **1.48**
///   (`docs/runs/2026-09-23-lto6-capacity-measurement.md`, run 1);
/// - `openssl` at 151 MB/s: **1.006** (same, run 2);
/// - tapectl `volume write`, steady 56 MB/s: **1.000** (same, run 3);
/// - steady 155.3 MB/s fill to ENOSPC: **1.0047**
///   (`docs/runs/2026-09-23-real-drive-rehearsal.md`, "The end-of-tape
///   fill").
///
/// 1.05 clears every steady feed by ten times its excess and is a third of
/// the way to nothing towards the one bursty feed measured. It is a
/// planning-margin figure, not a physical constant: 5% of a 2.5 TB tape is
/// 125 GB the plan assumed it had.
pub const FEED_RATIO_WARN_THRESHOLD: f64 = 1.05;

/// sg_logs prints page 0x0c's capacities in decimal megabytes (ADR-0012:
/// cartridge capacities are decimal, data sizes binary).
pub const NATIVE_MB_BYTES: u64 = 1_000_000;

/// The `events.action` the ratio is recorded under.
pub const EVENT_ACTION: &str = "write_feed_ratio";
/// The `events.field` — what `new_value` is a measurement of.
pub const EVENT_FIELD: &str = "native_per_data_byte";

/// The page 0x0c line this module reads, as sg_logs decodes it.
const BOP_TO_EOD_LINE: &str = "Native capacity from BOP to EOD:";

/// Parse "Native capacity from BOP to EOD: N MB" out of a page 0x0c decode.
///
/// `None` when the line is absent — a decode taken with no medium loaded
/// has no capacity lines at all (`hp_lto6_sg0_nomedia/page_0x0c`) — or when
/// the value is not in MB. The unit is checked, not assumed: a decoder that
/// printed GB here would otherwise be read as a thousand times too little
/// tape and never warn.
pub fn parse_native_bop_to_eod_mb(decoded_0x0c: &str) -> Option<u64> {
    decoded_0x0c.lines().find_map(|line| {
        let rest = line.trim().strip_prefix(BOP_TO_EOD_LINE)?;
        rest.trim().strip_suffix(" MB")?.trim().parse().ok()
    })
}

/// One write's tape-consumed-per-data-byte reading.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FeedRatio {
    /// Page 0x0c "Native capacity from BOP to EOD" after the write, decimal MB.
    pub native_bop_to_eod_mb: u64,
    /// The write's whole Layout, block-padded — every byte sent to the drive.
    pub data_bytes: u64,
}

impl FeedRatio {
    /// `None` when `data_bytes` is 0: there is no ratio to a write that sent
    /// nothing, and a division by zero is not a measurement.
    pub fn new(native_bop_to_eod_mb: u64, data_bytes: u64) -> Option<FeedRatio> {
        (data_bytes > 0).then_some(FeedRatio {
            native_bop_to_eod_mb,
            data_bytes,
        })
    }

    /// The numerator in bytes.
    pub fn native_bytes(&self) -> u64 {
        self.native_bop_to_eod_mb * NATIVE_MB_BYTES
    }

    /// Native bytes of tape per byte of data sent.
    pub fn ratio(&self) -> f64 {
        self.native_bytes() as f64 / self.data_bytes as f64
    }

    /// Above [`FEED_RATIO_WARN_THRESHOLD`].
    pub fn exceeds_threshold(&self) -> bool {
        self.ratio() > FEED_RATIO_WARN_THRESHOLD
    }

    /// The warning's first sentence, whatever its cause: both numbers, the
    /// ratio and the threshold.
    fn measurement_text(&self, label: &str) -> String {
        format!(
            "warning: volume \"{label}\": the drive used {native} bytes of native tape \
             ({mb} MB, page 0x0c BOP to EOD) for {data} bytes of data sent -- {ratio:.3} \
             native bytes per data byte, above the {threshold:.2} threshold.",
            native = self.native_bytes(),
            mb = self.native_bop_to_eod_mb,
            data = self.data_bytes,
            ratio = self.ratio(),
            threshold = FEED_RATIO_WARN_THRESHOLD,
        )
    }
}

// ── Page 0x02's corrections across the write (ADR-0012 item 170) ─────────

/// Above this many page 0x02 corrections per GiB written, the excess tape is
/// put down to the drive correcting, not to the feed.
///
/// The 2026-10-22 drive phase (`docs/runs/2026-10-22-488-drive-phase.md`,
/// finding 2), each run 21,490,565,120 bytes of data (20.01 GiB): the eight
/// clean runs raised "Total times correction algorithm processed" by 22 to
/// 141, **1.1 to 7.0 per GiB**; the three runs above 1.01 by 20,728, 99,027
/// and 153,609, **1,036 to 7,675 per GiB**. 100 is 14 times the highest clean
/// run and a tenth of the lowest jump, near the two's geometric middle (85).
/// Per GiB because the denominator is a data size (ADR-0012: data sizes are
/// binary).
pub const CORRECTIONS_JUMP_PER_GIB: f64 = 100.0;

/// The `details` keys the corrections are recorded under, and that `report`
/// reads back. All four are ABSENT, not null, when the corrections across the
/// write are not known; `corrections_baseline_contact_id` is null when the
/// baseline was the load's zero.
pub const CORRECTIONS_KEY: &str = "corrections";
pub const CORRECTIONS_PER_GIB_KEY: &str = "corrections_per_gib";
pub const CORRECTIONS_THRESHOLD_KEY: &str = "corrections_threshold_per_gib";
pub const CORRECTIONS_BASELINE_KEY: &str = "corrections_baseline_contact_id";

const GIB: f64 = (1u64 << 30) as f64;

/// The two page 0x02 parameters this module reads, as sg_logs decodes them.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct WriteCounters {
    /// Parameter 4, sg_logs's "Total times correction algorithm processed";
    /// HP's "Total times error correction processed: number of CCQ sets
    /// rewritten" (HP-V3 p63). The counter the drive phase tabulated.
    pub corrections: i64,
    /// Parameter 5, sg_logs's "Total bytes processed"; HP's "Total data sets
    /// processed: the total number of data sets written" (HP-V3 p63). A
    /// COUNT OF DATA SETS on the HP drive, not bytes, so it is never a
    /// denominator here: it only proves the reading counted a write.
    pub data_sets: i64,
}

impl WriteCounters {
    /// Parse a page 0x02 decode. A decode without the lines reads 0 for
    /// them, as [`crate::tape::health::parse_sg_logs_page`] does.
    pub fn from_decoded_0x02(decoded: &str) -> WriteCounters {
        let p = crate::tape::health::parse_sg_logs_page(0x02, decoded);
        WriteCounters {
            corrections: p.correction_algorithm_invocations,
            data_sets: p.total_bytes_processed,
        }
    }
}

/// Page 0x02's corrections across one write.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Corrections {
    /// "Total times correction algorithm processed" at the write's sweep,
    /// less its baseline.
    pub count: i64,
    /// The write's whole Layout, block-padded: the ratio's denominator.
    pub data_bytes: u64,
    /// The contact whose page 0x02 was subtracted; `None` when the write was
    /// the first contact of its load on an HP drive and the baseline is zero.
    pub baseline_contact_id: Option<i64>,
}

impl Corrections {
    /// Corrections per GiB of data sent.
    pub fn per_gib(&self) -> f64 {
        self.count as f64 / (self.data_bytes as f64 / GIB)
    }

    /// Above [`CORRECTIONS_JUMP_PER_GIB`].
    pub fn is_jump(&self) -> bool {
        self.per_gib() > CORRECTIONS_JUMP_PER_GIB
    }
}

/// The page 0x02 a contact's sweep journalled, parsed. The newest successful
/// decode, as [`journalled_0x0c_decode`] takes page 0x0c's.
fn journalled_0x02(conn: &Connection, contact_id: i64) -> Result<Option<WriteCounters>> {
    let decoded: Option<String> = conn
        .query_row(
            "SELECT decoded FROM log_page_journal
              WHERE contact_id = ?1 AND page_code = 0x02 AND subpage_code = 0
                AND ok = 1 AND decoded IS NOT NULL
              ORDER BY id DESC LIMIT 1",
            [contact_id],
            |r| r.get(0),
        )
        .optional()?;
    Ok(decoded.as_deref().map(WriteCounters::from_decoded_0x02))
}

/// What page 0x02 read before a contact's write, when it is known.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Baseline {
    /// The write is the first contact of its load on an HP drive: zero.
    LoadZero,
    /// The previous contact in the same load, and its reading.
    Contact(i64, WriteCounters),
}

/// A contact's drive, cartridge and MAM load count, each as recorded.
struct ContactLoad {
    drive_id: Option<i64>,
    cartridge_id: Option<i64>,
    chip_load_count: Option<i64>,
}

/// What page 0x02 read before this contact's write ([`Baseline`]), `None`
/// when it is not known.
///
/// The previous contact is the one immediately before this one on the same
/// drive (`cartridge_contacts.drive_id`), never an older one: a contact
/// between them that took no sweep may have written. It is in the same load
/// when it names the same cartridge and the same MAM load count
/// (`chip_load_count`), and then its page 0x02 is the baseline, or nothing is
/// known if it journalled none. A previous contact on another cartridge or
/// load count, or none at all, makes this the first contact of its load: the
/// baseline is zero on an HP drive (HP-V3 p63: "cleared when the tape is
/// unloaded"), and not known on another vendor's, whose page 0x02 has no such
/// statement behind it. A NULL drive, cartridge or load count on either side
/// is not known.
fn baseline_0x02(conn: &Connection, contact_id: i64) -> Result<Option<Baseline>> {
    let row = |r: &rusqlite::Row<'_>| {
        Ok(ContactLoad {
            drive_id: r.get(0)?,
            cartridge_id: r.get(1)?,
            chip_load_count: r.get(2)?,
        })
    };
    let this = conn
        .query_row(
            "SELECT drive_id, cartridge_id, chip_load_count FROM cartridge_contacts
              WHERE id = ?1",
            [contact_id],
            row,
        )
        .optional()?;
    let Some(ContactLoad {
        drive_id: Some(drive_id),
        cartridge_id: Some(cartridge_id),
        chip_load_count: Some(load_count),
    }) = this
    else {
        return Ok(None);
    };
    let vendor: Option<String> = conn
        .query_row("SELECT vendor FROM drives WHERE id = ?1", [drive_id], |r| {
            r.get(0)
        })
        .optional()?
        .flatten();
    let first_of_load =
        crate::tape::log_pages::is_hp_vendor(vendor.as_deref()).then_some(Baseline::LoadZero);
    let previous: Option<(i64, ContactLoad)> = conn
        .query_row(
            "SELECT id, drive_id, cartridge_id, chip_load_count FROM cartridge_contacts
              WHERE drive_id = ?1 AND id < ?2
              ORDER BY id DESC LIMIT 1",
            rusqlite::params![drive_id, contact_id],
            |r| {
                Ok((
                    r.get(0)?,
                    ContactLoad {
                        drive_id: r.get(1)?,
                        cartridge_id: r.get(2)?,
                        chip_load_count: r.get(3)?,
                    },
                ))
            },
        )
        .optional()?;
    let Some((prev, load)) = previous else {
        return Ok(first_of_load);
    };
    Ok(match (load.cartridge_id, load.chip_load_count) {
        (Some(other), _) if other != cartridge_id => first_of_load,
        (Some(_), Some(loads)) if loads != load_count => first_of_load,
        (Some(_), Some(_)) => journalled_0x02(conn, prev)?.map(|c| Baseline::Contact(prev, c)),
        _ => None,
    })
}

/// Page 0x02's corrections across the write this contact made: its sweep's
/// reading less the baseline ([`baseline_0x02`]). `None` when either reading
/// is missing or not known, when the counter went DOWN (the page was cleared
/// between the two readings, so the difference is not this write's), or when
/// the data sets written did not rise (the reading did not count this write
/// — mhvtl reports 0 for every page 0x02 parameter).
pub fn corrections_across_write(
    conn: &Connection,
    contact_id: i64,
    data_bytes: u64,
) -> Result<Option<Corrections>> {
    if data_bytes == 0 {
        return Ok(None);
    }
    let Some(end) = journalled_0x02(conn, contact_id)? else {
        return Ok(None);
    };
    let Some(baseline) = baseline_0x02(conn, contact_id)? else {
        return Ok(None);
    };
    let (baseline_contact_id, start) = match baseline {
        Baseline::Contact(id, counters) => (Some(id), counters),
        Baseline::LoadZero => (None, WriteCounters::default()),
    };
    let count = end.corrections - start.corrections;
    if count < 0 || end.data_sets <= start.data_sets {
        return Ok(None);
    }
    Ok(Some(Corrections {
        count,
        data_bytes,
        baseline_contact_id,
    }))
}

// ── The evidence and the cause it names ──────────────────────────────────

/// What the drive said about one write beyond the ratio: the two figures
/// item 170 judges the cause by.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Evidence {
    /// HP page 0x34's tape direction changes per 100 MB written, from the
    /// same sweep as the ratio; `None` on another vendor's drive, or when
    /// the page was not read.
    pub repositions_per_100mb: Option<u32>,
    /// Page 0x02's corrections across the write; `None` when not known.
    pub corrections: Option<Corrections>,
}

impl Evidence {
    /// The cause this evidence names, judged against today's constant.
    pub fn cause(&self) -> Cause {
        judge_cause(
            self.repositions_per_100mb,
            self.corrections.map(|c| c.per_gib()),
            CORRECTIONS_JUMP_PER_GIB,
        )
    }
}

/// The cause a feed-ratio warning names (ADR-0012 item 170).
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Cause {
    /// The drive reported repositions (HP page 0x34 above 0): it stopped and
    /// backed up, the irregular feed the quiet-host rule is about.
    Repositions,
    /// No repositions reported, and page 0x02's corrections jumped across
    /// the write: the drive or the cartridge, not the host.
    Corrections,
    /// Neither: the ratio is stated and no cause is named.
    Unattributed,
}

/// Item 170's rule, shared by the warning and `report`: repositions above 0
/// name the host; otherwise corrections per GiB above `threshold_per_gib`
/// name the drive or the cartridge; otherwise nothing is named.
pub fn judge_cause(
    repositions_per_100mb: Option<u32>,
    corrections_per_gib: Option<f64>,
    threshold_per_gib: f64,
) -> Cause {
    if repositions_per_100mb.is_some_and(|n| n > 0) {
        Cause::Repositions
    } else if corrections_per_gib.is_some_and(|c| c > threshold_per_gib) {
        Cause::Corrections
    } else {
        Cause::Unattributed
    }
}

/// Why a ratio above the threshold draws no warning. The row is recorded
/// either way; this only gates the stderr line, and is written into the
/// row's `details` so a reader knows why a 16x drew nothing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Suppression {
    /// The drive has a `capacity_override` — virtual drives (mhvtl) and the
    /// microcosm harnesses, and nothing else (ADR-0010). mhvtl's page 0x0c
    /// reports a static BOP→EOD unrelated to any write; a drive that lies
    /// about capacity cannot be read for a capacity ratio.
    CapacityOverride,
}

impl Suppression {
    /// The `details.suppressed` value.
    pub fn as_str(self) -> &'static str {
        match self {
            Suppression::CapacityOverride => "capacity_override",
        }
    }
}

/// One write's ratio together with the verdict on it: recorded always,
/// warned only when above the threshold AND not suppressed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Assessment {
    pub ratio: FeedRatio,
    pub suppressed: Option<Suppression>,
}

impl Assessment {
    /// Whether the operator warning is printed — and what `details.warned`
    /// records, so the row never claims a warning that did not appear.
    pub fn warns(&self) -> bool {
        self.ratio.exceeds_threshold() && self.suppressed.is_none()
    }

    /// The `events.details` JSON: both facts, the verdict, the threshold it
    /// was judged against, and — only when set — why a warning was
    /// suppressed. The key is ABSENT on an unsuppressed row, not null, so a
    /// query for `json_extract(details, '$.suppressed')` finds exactly the
    /// suppressed ones.
    pub fn details_json(&self, contact_id: i64) -> String {
        self.details_json_with_wear(contact_id, None)
    }

    /// [`Assessment::details_json`], plus the drive's own wear figure when
    /// there is one (issue #488): `repositions_per_100mb`, HP page 0x34's
    /// parameter 0, from the same sweep as the ratio. ABSENT, not null, when
    /// `wear` is `None` or carries no parameter 0.
    pub fn details_json_with_wear(
        &self,
        contact_id: i64,
        wear: Option<&crate::tape::log_pages::HpPerformance>,
    ) -> String {
        self.details_json_with_evidence(
            contact_id,
            &Evidence {
                repositions_per_100mb: wear.and_then(|w| w.repositions_per_100mb),
                corrections: None,
            },
        )
    }

    /// [`Assessment::details_json_with_wear`], plus page 0x02's corrections
    /// across the write when they are known (ADR-0012 item 170): the count
    /// (`corrections`), per GiB of data sent (`corrections_per_gib`), the
    /// threshold it was judged against (`corrections_threshold_per_gib`, so a
    /// later change to the constant does not rewrite history), and the
    /// contact whose reading was subtracted (`corrections_baseline_contact_id`,
    /// null for the load's zero). All four ABSENT when not known.
    pub fn details_json_with_evidence(&self, contact_id: i64, evidence: &Evidence) -> String {
        let mut details = serde_json::json!({
            "source_page": "0x0c",
            "native_bop_to_eod_mb": self.ratio.native_bop_to_eod_mb,
            "data_bytes": self.ratio.data_bytes,
            "ratio": self.ratio.ratio(),
            "threshold": FEED_RATIO_WARN_THRESHOLD,
            "warned": self.warns(),
            "contact_id": contact_id,
        });
        if let Some(s) = self.suppressed {
            details["suppressed"] = serde_json::Value::String(s.as_str().to_string());
        }
        if let Some(n) = evidence.repositions_per_100mb {
            details[REPOSITIONS_KEY] = n.into();
        }
        if let Some(c) = evidence.corrections {
            details[CORRECTIONS_KEY] = c.count.into();
            details[CORRECTIONS_PER_GIB_KEY] = c.per_gib().into();
            details[CORRECTIONS_THRESHOLD_KEY] = CORRECTIONS_JUMP_PER_GIB.into();
            details[CORRECTIONS_BASELINE_KEY] = c.baseline_contact_id.into();
        }
        details.to_string()
    }
}

/// One write's assessment and the drive's evidence beside it: what
/// [`assess_and_record`] returns, and what the warning is written from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Assessed {
    pub assessment: Assessment,
    pub evidence: Evidence,
}

impl Assessed {
    /// [`Assessment::warns`].
    pub fn warns(&self) -> bool {
        self.assessment.warns()
    }

    /// The operator warning, one paragraph on stderr: the measurement, then
    /// the cause the evidence names (ADR-0012 item 170), and always that the
    /// volume is complete and correct.
    pub fn warning_text(&self, label: &str) -> String {
        const NEXT_TAPE: &str = "the next tape will hold less than planned if it continues.";
        let measured = self.assessment.ratio.measurement_text(label);
        let ev = &self.evidence;
        match ev.cause() {
            Cause::Repositions => format!(
                "{measured} The drive spent tape repositioning behind an irregular feed; see \
                 the operator guide's quiet-host rule. The volume is complete and correct; this \
                 is a warning about the host, and {NEXT_TAPE}"
            ),
            Cause::Corrections => {
                let c = ev.corrections.expect("a correction cause has corrections");
                let (steady, page_0x34) = match ev.repositions_per_100mb {
                    Some(_) => (
                        " while the feed was steady",
                        "page 0x34: 0 repositions per 100 MB; ",
                    ),
                    None => ("", ""),
                };
                format!(
                    "{measured} The drive corrected heavily{steady} ({page_0x34}page 0x02: {n} \
                     corrections, {per:.1} per GiB written, above {t:.0}); this is about the \
                     drive or the cartridge, not the host: run a cleaning cartridge (`tapectl \
                     drive cleaned` records it), and check the medium if it recurs. The volume is \
                     complete and correct, and {NEXT_TAPE}",
                    n = c.count,
                    per = c.per_gib(),
                    t = CORRECTIONS_JUMP_PER_GIB,
                )
            }
            Cause::Unattributed => {
                let repositions = match ev.repositions_per_100mb {
                    Some(n) => format!("{n} repositions per 100 MB (page 0x34)"),
                    None => "no repositions figure".to_string(),
                };
                let corrections = match ev.corrections {
                    Some(c) => format!(
                        "{} write corrections, {:.1} per GiB, within {:.0} (page 0x02)",
                        c.count,
                        c.per_gib(),
                        CORRECTIONS_JUMP_PER_GIB
                    ),
                    None => "no known count of its write corrections (page 0x02)".to_string(),
                };
                format!(
                    "{measured} No cause is named: the drive reported {repositions} and \
                     {corrections}. The volume is complete and correct, and {NEXT_TAPE}"
                )
            }
        }
    }
}

/// The `details` key the drive's repositions per 100 MB are recorded under
/// (issue #488), and that `report` reads back.
pub const REPOSITIONS_KEY: &str = "repositions_per_100mb";

/// The ratio from a page 0x0c decode (or its absence) and the write's bytes.
/// `None` when there is no decode, no BOP→EOD line in it, or nothing was
/// sent.
pub fn from_decoded_0x0c(decoded_0x0c: Option<&str>, data_bytes: u64) -> Option<FeedRatio> {
    let mb = parse_native_bop_to_eod_mb(decoded_0x0c?)?;
    FeedRatio::new(mb, data_bytes)
}

/// The decoded page 0x0c THIS contact's sweep journalled, if it read one.
///
/// One sweep per contact (migration 023) means at most one such row; the
/// newest is taken should a future change ever leave two. A failed read or
/// a failed decode is `None`: the row exists, but says nothing about
/// capacity.
pub fn journalled_0x0c_decode(conn: &Connection, contact_id: i64) -> Result<Option<String>> {
    Ok(conn
        .query_row(
            "SELECT decoded FROM log_page_journal
              WHERE contact_id = ?1 AND page_code = 0x0c AND subpage_code = 0
                AND ok = 1 AND decoded IS NOT NULL
              ORDER BY id DESC LIMIT 1",
            [contact_id],
            |r| r.get(0),
        )
        .optional()?)
}

/// Compute the write's feed ratio from the journal and record it as an
/// `events` row — the one production entry point, called by `volume write`
/// after its post-command sweep, only for a write that completed (an
/// aborted or interrupted write sent fewer bytes than its Layout, so the
/// denominator would be wrong).
///
/// `None` — nothing recorded, nothing to warn about — when the contact
/// could not be named (the sweep's rows carry a NULL contact id and cannot
/// be attributed), when the sweep journalled no usable page 0x0c, when the
/// decode has no BOP→EOD line, or when nothing was sent. `suppressed` does
/// NOT stop the recording — it is written into the row and gates only the
/// caller's warning ([`Assessment::warns`]). Best-effort throughout: a
/// refused INSERT warns in the log and the assessment is still returned,
/// so the operator warning does not depend on the bookkeeping.
///
/// The evidence beside the ratio (ADR-0012 item 170) is read from the same
/// journal: this sweep's page 0x34 on an HP drive, and page 0x02's
/// corrections across the write ([`corrections_across_write`]). A failed
/// read of either costs only that figure, never the ratio.
pub fn assess_and_record(
    conn: &Connection,
    contact_id: Option<i64>,
    volume_id: i64,
    label: &str,
    data_bytes: u64,
    suppressed: Option<Suppression>,
) -> Option<Assessed> {
    let contact_id = contact_id?;
    let decoded = match journalled_0x0c_decode(conn, contact_id) {
        Ok(d) => d,
        Err(e) => {
            warn!(err = %e, contact_id, "log_page_journal read for page 0x0c failed");
            return None;
        }
    };
    let ratio = from_decoded_0x0c(decoded.as_deref(), data_bytes)?;
    let assessment = Assessment { ratio, suppressed };
    // Issue #488: the same sweep's page 0x34, when the drive is HP. A failed
    // read of it costs only the figure, never the ratio.
    let wear =
        crate::tape::log_pages::journalled_hp_performance(conn, contact_id).unwrap_or_else(|e| {
            warn!(err = %e, contact_id, "log_page_journal read for page 0x34 failed");
            None
        });
    let corrections = corrections_across_write(conn, contact_id, data_bytes).unwrap_or_else(|e| {
        warn!(err = %e, contact_id, "log_page_journal read for page 0x02 failed");
        None
    });
    let evidence = Evidence {
        repositions_per_100mb: wear.and_then(|w| w.repositions_per_100mb),
        corrections,
    };
    if let Err(e) = events::log_event(
        conn,
        "volume",
        volume_id,
        Some(label),
        EVENT_ACTION,
        Some(EVENT_FIELD),
        None,
        Some(&format!("{:.4}", ratio.ratio())),
        Some(&assessment.details_json_with_evidence(contact_id, &evidence)),
        None,
    ) {
        warn!(err = %e, "events insert for the feed ratio failed");
    }
    Some(Assessed {
        assessment,
        evidence,
    })
}

/// The `events.action` a write's capacity use is recorded under (issue #391).
pub const CAPACITY_EVENT_ACTION: &str = "write_capacity_used";
/// Its `events.field`: `new_value` is the data the write sent, as a
/// fraction of the volume's capacity.
pub const CAPACITY_EVENT_FIELD: &str = "fraction_of_capacity";

/// Record how much of the volume's capacity a completed write used (issue
/// #391: "record capacity actually used per write so the ceiling can be
/// tuned"; ADR-0012 2026-10-06 item 16: "tune from home2's recorded
/// capacity use").
///
/// One `events` row per completed write, beside the feed ratio's and for
/// the same reason (ADR-0013 §3: a derived figure is what tapectl said, not
/// a column). `new_value` is the data fraction — the whole block-padded
/// Layout over `volumes.capacity_bytes`, the two figures the fill ceiling
/// compares. `details` carries both, the ceiling the write was planned
/// against, and — when this contact's sweep journalled page 0x0c — the
/// native tape the drive used for it (`native_fraction`), which is what
/// really decides whether a fuller tape would have fit. Best-effort: a
/// refused INSERT is a log warning. Returns the data fraction recorded.
pub fn record_capacity_used(
    conn: &Connection,
    contact_id: Option<i64>,
    volume_id: i64,
    label: &str,
    data_bytes: u64,
    capacity_bytes: u64,
    fill_ceiling: f64,
) -> Option<f64> {
    if capacity_bytes == 0 {
        return None;
    }
    let fraction = data_bytes as f64 / capacity_bytes as f64;
    let native_mb = contact_id
        .and_then(|c| journalled_0x0c_decode(conn, c).ok().flatten())
        .and_then(|d| parse_native_bop_to_eod_mb(&d));
    let mut details = serde_json::json!({
        "data_bytes": data_bytes,
        "capacity_bytes": capacity_bytes,
        "fill_ceiling": fill_ceiling,
        "contact_id": contact_id,
    });
    if let Some(mb) = native_mb {
        details["native_bop_to_eod_mb"] = mb.into();
        details["native_fraction"] = ((mb * NATIVE_MB_BYTES) as f64 / capacity_bytes as f64).into();
    }
    if let Err(e) = events::log_event(
        conn,
        "volume",
        volume_id,
        Some(label),
        CAPACITY_EVENT_ACTION,
        Some(CAPACITY_EVENT_FIELD),
        None,
        Some(&format!("{fraction:.4}")),
        Some(&details.to_string()),
        None,
    ) {
        warn!(err = %e, "events insert for the capacity used failed");
    }
    Some(fraction)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tape::log_pages::{self, JournalRow};

    /// Issue #391: a completed write records its capacity use — the data
    /// fraction, the ceiling it was planned against, and the native tape
    /// the drive used when the sweep journalled page 0x0c.
    #[test]
    fn a_write_records_the_capacity_it_used() {
        let conn = crate::db::open_memory().unwrap();
        conn.execute(
            "INSERT INTO volumes (id, label, backend_type, backend_name, capacity_bytes, status)
             VALUES (7, 'L6-0001', 'lto', 'p', 2500000000000, 'active')",
            [],
        )
        .unwrap();
        let f = record_capacity_used(
            &conn,
            None,
            7,
            "L6-0001",
            2_000_000_000_000,
            2_500_000_000_000,
            0.97,
        );
        assert_eq!(f, Some(0.8));
        let (value, details): (String, String) = conn
            .query_row(
                "SELECT new_value, details FROM events WHERE action = ?1 AND field = ?2",
                [CAPACITY_EVENT_ACTION, CAPACITY_EVENT_FIELD],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(value, "0.8000");
        let d: serde_json::Value = serde_json::from_str(&details).unwrap();
        assert_eq!(d["fill_ceiling"], 0.97);
        assert_eq!(d["capacity_bytes"], 2_500_000_000_000u64);
        assert!(
            d.get("native_fraction").is_none(),
            "no 0x0c journalled: {d}"
        );
        assert_eq!(record_capacity_used(&conn, None, 7, "x", 1, 0, 0.97), None);
    }

    /// The real HP LTO-6 with the FUJIFILM cartridge loaded (2026-09-23):
    /// 23 MB recorded from BOP to EOD.
    const HP_LOADED_0X0C: &str = include_str!(
        "../../tests/fixtures/sg_logs/hp_lto6_sg0_fuji_ew7vwmvkf6/page_0x0c.decoded.txt"
    );
    /// The same drive with no medium: the page decodes, but carries no
    /// capacity lines at all.
    const HP_NOMEDIA_0X0C: &str =
        include_str!("../../tests/fixtures/sg_logs/hp_lto6_sg0_nomedia/page_0x0c.decoded.txt");
    /// The mhvtl drive: it DOES list 0x0c, and prints a BOP→EOD of 500 MB
    /// beside "0 GB written" — a figure with no relation to any write.
    const MHVTL_0X0C: &str =
        include_str!("../../tests/fixtures/sg_logs/mhvtl_td8_sg1/page_0x0c.decoded.txt");

    /// Run 3 of the capacity measurement
    /// (`docs/runs/2026-09-23-lto6-capacity-measurement.md`): `volume
    /// write`, steady 56 MB/s.
    const RUN_3_NATIVE_MB: u64 = 21_487;
    const RUN_3_DATA_BYTES: u64 = 21_488_398_724;

    /// A synthetic page 0x0c decode in the real drive's shape with the
    /// BOP→EOD figure substituted.
    fn synthetic_0x0c(bop_to_eod_mb: u64) -> String {
        HP_LOADED_0X0C
            .lines()
            .map(|l| {
                if l.trim().starts_with(BOP_TO_EOD_LINE) {
                    format!("  {BOP_TO_EOD_LINE} {bop_to_eod_mb} MB")
                } else {
                    l.to_string()
                }
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    // ── The parser, against the real captures ──

    #[test]
    fn parses_bop_to_eod_from_the_real_drive_capture() {
        // Negative control: change the fixture's "23 MB" line and this fails.
        assert_eq!(parse_native_bop_to_eod_mb(HP_LOADED_0X0C), Some(23));
        assert!(
            HP_LOADED_0X0C.contains("Native capacity from BOP to EOD: 23 MB"),
            "positive control: the fixture carries the line being parsed"
        );
    }

    #[test]
    fn a_no_medium_decode_has_no_bop_to_eod_line() {
        // Negative control: a parser that defaulted a missing line to 0
        // would return Some(0) here and later compute a ratio of 0.
        assert!(
            !HP_NOMEDIA_0X0C.contains(BOP_TO_EOD_LINE),
            "positive control: the no-medium fixture really lacks the line"
        );
        assert_eq!(parse_native_bop_to_eod_mb(HP_NOMEDIA_0X0C), None);
    }

    #[test]
    fn the_mhvtl_capture_lists_0x0c_with_a_figure_unrelated_to_any_write() {
        // The brief for #338 assumed mhvtl has no page 0x0c. It does, and
        // its BOP→EOD is 500 MB beside "0 GB" written. This is WHY
        // `volume write` SUPPRESSES the warning on a drive with a
        // `capacity_override` (virtual drives only, ADR-0010) while still
        // recording the row: mhvtl's 500 MB against a 30 MB gate write is
        // 16x on every run — see `a_capacity_override_drive_is_recorded_
        // but_not_warned`. Negative control: if mhvtl ever reports a real
        // BOP→EOD, this assertion is what says the suppression can be
        // revisited.
        assert_eq!(parse_native_bop_to_eod_mb(MHVTL_0X0C), Some(500));
        assert!(MHVTL_0X0C.contains("Data bytes written to media by WRITE commands: 0 GB"));
    }

    #[test]
    fn a_value_not_in_megabytes_is_not_read() {
        // Negative control: drop the unit check and "23 GB" parses as 23 MB
        // — a thousandfold under-read that would never warn.
        assert_eq!(
            parse_native_bop_to_eod_mb("  Native capacity from BOP to EOD: 23 GB\n"),
            None
        );
        assert_eq!(
            parse_native_bop_to_eod_mb("  Native capacity from BOP to EOD: 23\n"),
            None
        );
        assert_eq!(
            parse_native_bop_to_eod_mb("Native capacity from BOP to EOD: 23 MB"),
            Some(23),
            "positive control: an unindented line still parses"
        );
    }

    // ── The ratio ──

    #[test]
    fn a_bursty_feed_at_1_48x_warns() {
        // Run 1's ratio, applied to a synthetic write: 1,000,000,000 data
        // bytes against 1,480 MB of tape. Negative control: raise
        // FEED_RATIO_WARN_THRESHOLD above 1.48 and this fails.
        let data_bytes = 1_000_000_000;
        let decoded = synthetic_0x0c(1_480);
        let r = from_decoded_0x0c(Some(&decoded), data_bytes).expect("computable");
        assert!((r.ratio() - 1.48).abs() < 1e-9, "ratio {}", r.ratio());
        assert!(r.exceeds_threshold(), "1.48 is above 1.05");
    }

    #[test]
    fn a_steady_feed_at_1_00x_does_not_warn() {
        // Negative control: lower FEED_RATIO_WARN_THRESHOLD to 1.0 or below
        // and this fails.
        let data_bytes = 1_000_000_000;
        let decoded = synthetic_0x0c(1_000);
        let r = from_decoded_0x0c(Some(&decoded), data_bytes).expect("computable");
        assert!((r.ratio() - 1.0).abs() < 1e-9);
        assert!(!r.exceeds_threshold(), "1.000 is not above 1.05");
    }

    #[test]
    fn the_rehearsal_fill_at_1_0047x_does_not_warn() {
        // The fourth #323 data point: 2,513,648 MB for 2,501,995,134,976
        // bytes (`docs/runs/2026-09-23-real-drive-rehearsal.md`). A steady
        // real fill must not warn, or the threshold is mis-set.
        let r = FeedRatio::new(2_513_648, 2_501_995_134_976).unwrap();
        assert_eq!(format!("{:.4}", r.ratio()), "1.0047");
        assert!(!r.exceeds_threshold());
    }

    #[test]
    fn page_absent_computes_nothing() {
        // A drive whose sweep read no 0x0c (or read it and it failed):
        // nothing to compute, so nothing to warn. Negative control: a
        // `from_decoded_0x0c` that defaulted the numerator would return
        // Some here.
        assert_eq!(from_decoded_0x0c(None, 1_000_000_000), None);
        // And a page that decoded without the line (no medium).
        assert_eq!(
            from_decoded_0x0c(Some(HP_NOMEDIA_0X0C), 1_000_000_000),
            None
        );
    }

    #[test]
    fn a_write_that_sent_nothing_has_no_ratio() {
        // Negative control: remove the `data_bytes > 0` guard and this is a
        // division by zero (inf), which `exceeds_threshold` would warn on.
        assert_eq!(FeedRatio::new(23, 0), None);
        assert_eq!(from_decoded_0x0c(Some(HP_LOADED_0X0C), 0), None);
    }

    #[test]
    fn the_run_3_measurement_computes_to_one() {
        // The acceptance criterion on issue #338: run 3's journalled pair
        // — 21,487 MB native for 21,488,398,724 bytes sent — computes to
        // 1.000. The exact value is 0.99993; the record prints it to three
        // places, so that is what is pinned. Negative control: change
        // NATIVE_MB_BYTES to 2^20 and this reads 1.049.
        let r = FeedRatio::new(RUN_3_NATIVE_MB, RUN_3_DATA_BYTES).unwrap();
        assert_eq!(format!("{:.3}", r.ratio()), "1.000");
        assert!(!r.exceeds_threshold());
        // And through the parser, from a decode in the drive's own shape.
        let decoded = synthetic_0x0c(RUN_3_NATIVE_MB);
        let via_parser = from_decoded_0x0c(Some(&decoded), RUN_3_DATA_BYTES).unwrap();
        assert_eq!(via_parser, r);
    }

    /// A 1.48 write's [`Assessed`], with `evidence`.
    fn assessed_at_1_48(evidence: Evidence) -> Assessed {
        Assessed {
            assessment: Assessment {
                ratio: FeedRatio::new(1_480, 1_000_000_000).unwrap(),
                suppressed: None,
            },
            evidence,
        }
    }

    #[test]
    fn the_warning_names_both_numbers_the_ratio_and_the_cause() {
        // ADR-0012 item 170: the host is named only when the drive reported
        // repositions (HP page 0x34 above 0).
        let text = assessed_at_1_48(Evidence {
            repositions_per_100mb: Some(3),
            corrections: None,
        })
        .warning_text("V1");
        // Pinned verbatim: this is the operator-facing text, unchanged from
        // before item 170. Negative control: any rewording fails here,
        // deliberately.
        assert_eq!(
            text,
            "warning: volume \"V1\": the drive used 1480000000 bytes of native tape (1480 MB, \
             page 0x0c BOP to EOD) for 1000000000 bytes of data sent -- 1.480 native bytes \
             per data byte, above the 1.05 threshold. The drive spent tape repositioning \
             behind an irregular feed; see the operator guide's quiet-host \
             rule. The volume is complete and correct; this is a warning about the host, and \
             the next tape will hold less than planned if it continues."
        );
        assert!(
            text.starts_with("warning: "),
            "it is a warning, not an error"
        );
    }

    // ── The journal read and the events row ──

    fn contact(conn: &Connection) -> i64 {
        conn.execute(
            "INSERT INTO cartridge_contacts (operation, device) VALUES ('volume write', '/dev/null')",
            [],
        )
        .unwrap();
        conn.last_insert_rowid()
    }

    fn volume(conn: &Connection, label: &str) -> i64 {
        conn.execute(
            "INSERT INTO volumes (label, backend_type, backend_name, media_type, capacity_bytes, status)
             VALUES (?1, 'lto', 'lto0', 'LTO-6', 2500000000000, 'sealed')",
            [label],
        )
        .unwrap();
        conn.last_insert_rowid()
    }

    fn journal_0x0c(conn: &Connection, contact_id: i64, decoded: Option<&str>, ok: bool) {
        let row = JournalRow {
            captured_at: "2026-09-23 14:50:00".into(),
            contact_id: Some(contact_id),
            device_sg: "/dev/sg0".into(),
            device_tape: Some("/dev/nst0".into()),
            trigger: "volume write".into(),
            page_code: 0x0c,
            subpage_code: 0,
            ok,
            error: None,
            tool_argv: "[\"sg_logs\"]".into(),
            tool_version: None,
            raw: rusqlite::types::Value::Blob(vec![0x0c, 0, 0, 0]),
            decoded: decoded.map(str::to_string),
            tapectl_version: crate::build_info::VERSION,
        };
        log_pages::insert(conn, &row).unwrap();
    }

    fn events_for(
        conn: &Connection,
        volume_id: i64,
    ) -> Vec<(String, Option<String>, Option<String>)> {
        let mut stmt = conn
            .prepare(
                "SELECT field, new_value, details FROM events
                  WHERE entity_type = 'volume' AND entity_id = ?1 AND action = ?2",
            )
            .unwrap();
        stmt.query_map(rusqlite::params![volume_id, EVENT_ACTION], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?))
        })
        .unwrap()
        .collect::<std::result::Result<Vec<_>, _>>()
        .unwrap()
    }

    #[test]
    fn assess_reads_this_contacts_0x0c_from_the_journal_and_records_an_event() {
        let conn = crate::db::open_memory().unwrap();
        let vid = volume(&conn, "V1");
        let cid = contact(&conn);
        // ANOTHER contact's 0x0c at 1.00x, journalled first: the read must
        // be by contact id, not "the latest 0x0c row". Negative control:
        // drop `contact_id = ?1` from the query and, with ORDER BY id DESC,
        // this test still passes — so the other contact's row is inserted
        // SECOND below as well.
        let other = contact(&conn);
        journal_0x0c(&conn, other, Some(&synthetic_0x0c(1_000)), true);
        journal_0x0c(&conn, cid, Some(&synthetic_0x0c(1_480)), true);
        journal_0x0c(&conn, other, Some(&synthetic_0x0c(1_000)), true);

        let a = assess_and_record(&conn, Some(cid), vid, "V1", 1_000_000_000, None)
            .expect("a journalled 0x0c with the line is computable")
            .assessment;
        assert_eq!(a.ratio.native_bop_to_eod_mb, 1_480);
        assert!(a.ratio.exceeds_threshold());
        assert!(a.warns(), "a real drive (no suppression) at 1.48 warns");

        let rows = events_for(&conn, vid);
        assert_eq!(rows.len(), 1, "one events row per assessment");
        let (field, new_value, details) = &rows[0];
        assert_eq!(field.as_str(), EVENT_FIELD);
        assert_eq!(new_value.as_deref(), Some("1.4800"));
        let details: serde_json::Value = serde_json::from_str(details.as_deref().unwrap()).unwrap();
        assert_eq!(details["source_page"], "0x0c");
        assert_eq!(details["native_bop_to_eod_mb"], 1_480);
        assert_eq!(details["data_bytes"], 1_000_000_000u64);
        assert_eq!(details["threshold"], FEED_RATIO_WARN_THRESHOLD);
        assert_eq!(details["warned"], true);
        assert_eq!(details["contact_id"], cid);
        assert!(
            details.get("suppressed").is_none(),
            "an unsuppressed row has NO `suppressed` key, not a null one"
        );
    }

    /// Page 0x34 as an HP drive's sweep journals it: `repositions`
    /// repositions per 100 MB, the other four parameters as the real
    /// drive's no-medium capture.
    fn journal_0x34(conn: &Connection, contact_id: i64, repositions: u8) {
        let mut raw =
            include_bytes!("../../tests/fixtures/sg_logs/hp_lto6_sg0_nomedia/page_0x34.bin")
                .to_vec();
        raw[9] = repositions; // parameter 0's value, low byte
        let row = JournalRow {
            captured_at: "2026-09-23 14:50:00".into(),
            contact_id: Some(contact_id),
            device_sg: "/dev/sg0".into(),
            device_tape: Some("/dev/nst0".into()),
            trigger: "volume write".into(),
            page_code: 0x34,
            subpage_code: 0,
            ok: true,
            error: None,
            tool_argv: "[\"sg_logs\"]".into(),
            tool_version: None,
            raw: rusqlite::types::Value::Blob(raw),
            decoded: Some("Unable to decode Read forward errors (lto-5) [rfe_]".into()),
            tapectl_version: crate::build_info::VERSION,
        };
        log_pages::insert(conn, &row).unwrap();
    }

    /// A contact on a drive of `vendor`.
    fn contact_on(conn: &Connection, vendor: &str) -> i64 {
        conn.execute(
            "INSERT INTO drives (serial, vendor, model)
             SELECT 'SER-' || (COUNT(*) + 1), ?1, 'Ultrium 6-SCSI' FROM drives",
            [vendor],
        )
        .unwrap();
        let drive_id = conn.last_insert_rowid();
        conn.execute(
            "INSERT INTO cartridge_contacts (operation, device, drive_id)
             VALUES ('volume write', '/dev/null', ?1)",
            [drive_id],
        )
        .unwrap();
        conn.last_insert_rowid()
    }

    /// Issue #488: on an HP drive the event carries the drive's own wear
    /// figure — page 0x34's repositions per 100 MB from THIS contact's
    /// sweep, not another's. Positive control for the absence below: the
    /// key is there when it should be.
    #[test]
    fn an_hp_write_records_its_repositions_beside_the_ratio() {
        let conn = crate::db::open_memory().unwrap();
        let vid = volume(&conn, "L6-W");
        let cid = contact_on(&conn, "HP");
        let other = contact_on(&conn, "HP");
        journal_0x34(&conn, other, 99);
        journal_0x0c(&conn, cid, Some(&synthetic_0x0c(1_505)), true);
        journal_0x34(&conn, cid, 7);
        journal_0x34(&conn, other, 99);
        assess_and_record(&conn, Some(cid), vid, "L6-W", 1_000_000_000, None).unwrap();
        let details: serde_json::Value =
            serde_json::from_str(events_for(&conn, vid)[0].2.as_deref().unwrap()).unwrap();
        assert_eq!(details[REPOSITIONS_KEY], 7, "{details}");
        assert_eq!(details["ratio"], 1.505);
    }

    /// The figure is absent — the key, not a null — when the drive is not
    /// HP (another vendor's page 0x34 is something else), when its identity
    /// is unknown, or when the sweep journalled no page 0x34. The ratio is
    /// recorded either way.
    #[test]
    fn the_repositions_are_absent_unless_an_hp_drive_journalled_page_0x34() {
        let conn = crate::db::open_memory().unwrap();
        let ibm = contact_on(&conn, "IBM");
        let unknown = contact(&conn);
        let hp_no_page = contact_on(&conn, "HP");
        for (i, cid) in [ibm, unknown, hp_no_page].into_iter().enumerate() {
            let label = format!("L6-A{i}");
            let vid = volume(&conn, &label);
            journal_0x0c(&conn, cid, Some(&synthetic_0x0c(1_000)), true);
            if cid != hp_no_page {
                journal_0x34(&conn, cid, 7);
            }
            assess_and_record(&conn, Some(cid), vid, &label, 1_000_000_000, None).unwrap();
            let rows = events_for(&conn, vid);
            assert_eq!(rows.len(), 1, "the ratio is recorded regardless");
            let details: serde_json::Value =
                serde_json::from_str(rows[0].2.as_deref().unwrap()).unwrap();
            assert!(details.get(REPOSITIONS_KEY).is_none(), "{label}: {details}");
        }
    }

    #[test]
    fn a_steady_write_is_recorded_too_and_not_flagged() {
        // The row is written whenever computable, so run 3's 1.000 is
        // queryable. Negative control: an `assess_and_record` that only
        // wrote on a warning leaves `events` empty here.
        let conn = crate::db::open_memory().unwrap();
        let vid = volume(&conn, "M323A1");
        let cid = contact(&conn);
        journal_0x0c(&conn, cid, Some(&synthetic_0x0c(RUN_3_NATIVE_MB)), true);

        let a = assess_and_record(&conn, Some(cid), vid, "M323A1", RUN_3_DATA_BYTES, None)
            .unwrap()
            .assessment;
        assert!(!a.ratio.exceeds_threshold());
        assert!(!a.warns());
        let rows = events_for(&conn, vid);
        assert_eq!(rows.len(), 1);
        assert_eq!(
            rows[0].1.as_deref(),
            Some("0.9999"),
            "four places, unrounded"
        );
        let details: serde_json::Value =
            serde_json::from_str(rows[0].2.as_deref().unwrap()).unwrap();
        assert_eq!(details["warned"], false);
        assert!(details.get("suppressed").is_none());
    }

    #[test]
    fn a_capacity_override_drive_is_recorded_but_not_warned() {
        // The mhvtl shape, end to end: its static 500 MB against a 30 MB
        // gate write is 16.67x. On a drive with a `capacity_override` the
        // row IS recorded — so the gate exercises the whole path and the
        // 16x is visible to a reader — but no warning is drawn, and the row
        // says why. Negative controls: fold the suppression back into a
        // skip and `events` is empty here; drop `suppressed` from
        // `warns()` and `warned` reads true; write `"suppressed": null`
        // instead of omitting the key and the unsuppressed tests fail.
        let conn = crate::db::open_memory().unwrap();
        let vid = volume(&conn, "GATE1");
        let cid = contact(&conn);
        journal_0x0c(&conn, cid, Some(MHVTL_0X0C), true);

        let data_bytes = 30_000_000;
        let a = assess_and_record(
            &conn,
            Some(cid),
            vid,
            "GATE1",
            data_bytes,
            Some(Suppression::CapacityOverride),
        )
        .expect("recorded even though suppressed")
        .assessment;
        assert_eq!(a.ratio.native_bop_to_eod_mb, 500);
        assert!(
            a.ratio.exceeds_threshold(),
            "positive control: 16x IS above 1.05"
        );
        assert!(!a.warns(), "but a suppressed assessment does not warn");
        assert_eq!(a.suppressed, Some(Suppression::CapacityOverride));

        let rows = events_for(&conn, vid);
        assert_eq!(rows.len(), 1, "recorded, not skipped");
        assert_eq!(rows[0].1.as_deref(), Some("16.6667"));
        let details: serde_json::Value =
            serde_json::from_str(rows[0].2.as_deref().unwrap()).unwrap();
        assert_eq!(details["source_page"], "0x0c");
        assert_eq!(details["native_bop_to_eod_mb"], 500);
        assert_eq!(details["data_bytes"], data_bytes);
        assert_eq!(
            details["warned"], false,
            "the row never claims a warning that did not appear"
        );
        assert_eq!(details["suppressed"], "capacity_override");
    }

    #[test]
    fn a_real_drive_shaped_write_at_1_48x_warns_and_records_it() {
        // The other half of the split: no override (a real drive), 1.48x
        // → warns, and the row says warned=true with no `suppressed`.
        // Negative control: suppress unconditionally and `warns()` is
        // false here.
        let conn = crate::db::open_memory().unwrap();
        let vid = volume(&conn, "REAL1");
        let cid = contact(&conn);
        journal_0x0c(&conn, cid, Some(&synthetic_0x0c(1_480)), true);
        let a = assess_and_record(&conn, Some(cid), vid, "REAL1", 1_000_000_000, None).unwrap();
        assert!(a.warns());
        assert_eq!(a.assessment.suppressed, None);
        assert!(a
            .warning_text("REAL1")
            .starts_with("warning: volume \"REAL1\""));
        let details: serde_json::Value =
            serde_json::from_str(events_for(&conn, vid)[0].2.as_deref().unwrap()).unwrap();
        assert_eq!(details["warned"], true);
        assert!(details.get("suppressed").is_none());
    }

    #[test]
    fn no_journalled_0x0c_records_nothing() {
        // The mhvtl-shaped case the brief describes (a drive that lists no
        // 0x0c) and the failed-read case: no row, no ratio, no event.
        // Negative control: a query without `ok = 1 AND decoded IS NOT
        // NULL` would compute from the failed read's NULL and panic, or a
        // parser defaulting the numerator would record a 0.
        let conn = crate::db::open_memory().unwrap();
        let vid = volume(&conn, "V1");
        let cid = contact(&conn);
        assert_eq!(
            assess_and_record(&conn, Some(cid), vid, "V1", 1_000_000_000, None),
            None
        );
        assert!(
            events_for(&conn, vid).is_empty(),
            "nothing recorded without a page"
        );

        // A 0x0c row whose read failed.
        journal_0x0c(&conn, cid, None, false);
        assert_eq!(
            assess_and_record(&conn, Some(cid), vid, "V1", 1_000_000_000, None),
            None
        );
        assert!(events_for(&conn, vid).is_empty());

        // A 0x0c that decoded without the line (no medium).
        journal_0x0c(&conn, cid, Some(HP_NOMEDIA_0X0C), true);
        assert_eq!(
            assess_and_record(&conn, Some(cid), vid, "V1", 1_000_000_000, None),
            None
        );
        assert!(events_for(&conn, vid).is_empty());

        // Positive control: the same contact with a real line IS computed.
        journal_0x0c(&conn, cid, Some(HP_LOADED_0X0C), true);
        assert!(assess_and_record(&conn, Some(cid), vid, "V1", 23_000_000, None).is_some());
        assert_eq!(events_for(&conn, vid).len(), 1);
    }

    #[test]
    fn an_unnamed_contact_cannot_be_assessed() {
        // The sweep's rows carry a NULL contact id when the contact INSERT
        // failed; they cannot be attributed to this write, so nothing is
        // computed. Negative control: an `assess_and_record` that fell back
        // to "the latest 0x0c row" would find this one.
        let conn = crate::db::open_memory().unwrap();
        let vid = volume(&conn, "V1");
        let row = JournalRow {
            captured_at: "2026-09-23 14:50:00".into(),
            contact_id: None,
            device_sg: "/dev/sg0".into(),
            device_tape: None,
            trigger: "volume write".into(),
            page_code: 0x0c,
            subpage_code: 0,
            ok: true,
            error: None,
            tool_argv: "[\"sg_logs\"]".into(),
            tool_version: None,
            raw: rusqlite::types::Value::Blob(vec![0x0c, 0, 0, 0]),
            decoded: Some(synthetic_0x0c(1_480)),
            tapectl_version: crate::build_info::VERSION,
        };
        log_pages::insert(&conn, &row).unwrap();
        assert_eq!(
            assess_and_record(&conn, None, vid, "V1", 1_000_000_000, None),
            None
        );
        assert!(events_for(&conn, vid).is_empty());
    }

    // ── ADR-0012 item 170: page 0x02's corrections and the cause ──

    /// The real HP LTO-6 with no medium (2026-09-23): the last tape's counts,
    /// 306,488 corrections over 12,820 data sets.
    const HP_NOMEDIA_0X02: &str =
        include_str!("../../tests/fixtures/sg_logs/hp_lto6_sg0_nomedia/page_0x02.decoded.txt");
    /// The same drive with the FUJIFILM cartridge just loaded: all zeros.
    const HP_LOADED_0X02: &str = include_str!(
        "../../tests/fixtures/sg_logs/hp_lto6_sg0_fuji_ew7vwmvkf6/page_0x02.decoded.txt"
    );
    /// mhvtl: every parameter 0, whatever it wrote.
    const MHVTL_0X02: &str =
        include_str!("../../tests/fixtures/sg_logs/mhvtl_td8_sg1/page_0x02.decoded.txt");

    /// One drive-phase run's data (`docs/runs/2026-10-22-488-drive-phase.md`).
    const DRIVE_PHASE_DATA_BYTES: u64 = 21_490_565_120;

    /// A page 0x02 decode in the real drive's shape with the two counters
    /// this module reads substituted.
    fn synthetic_0x02(corrections: i64, data_sets: i64) -> String {
        HP_NOMEDIA_0X02
            .lines()
            .map(|l| match l.trim() {
                t if t.starts_with("Total times correction algorithm processed") => {
                    format!("  Total times correction algorithm processed = {corrections}")
                }
                t if t.starts_with("Total bytes processed") => {
                    format!("  Total bytes processed = {data_sets}")
                }
                _ => l.to_string(),
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn journal_page(conn: &Connection, contact_id: i64, page_code: u8, decoded: &str) {
        let row = JournalRow {
            captured_at: "2026-10-22 22:00:00".into(),
            contact_id: Some(contact_id),
            device_sg: "/dev/sg13".into(),
            device_tape: Some("/dev/nst0".into()),
            trigger: "volume write".into(),
            page_code,
            subpage_code: 0,
            ok: true,
            error: None,
            tool_argv: "[\"sg_logs\"]".into(),
            tool_version: None,
            raw: rusqlite::types::Value::Blob(vec![page_code, 0, 0, 0]),
            decoded: Some(decoded.to_string()),
            tapectl_version: crate::build_info::VERSION,
        };
        log_pages::insert(conn, &row).unwrap();
    }

    fn journal_0x02(conn: &Connection, contact_id: i64, corrections: i64, data_sets: i64) {
        journal_page(
            conn,
            contact_id,
            0x02,
            &synthetic_0x02(corrections, data_sets),
        );
    }

    fn drive(conn: &Connection, vendor: &str) -> i64 {
        conn.execute(
            "INSERT INTO drives (serial, vendor, model)
             SELECT 'DRV-' || (COUNT(*) + 1), ?1, 'Ultrium 6-SCSI' FROM drives",
            [vendor],
        )
        .unwrap();
        conn.last_insert_rowid()
    }

    fn cartridge(conn: &Connection, barcode: &str) -> i64 {
        conn.execute(
            "INSERT INTO cartridges (barcode, media_type, nominal_capacity)
             VALUES (?1, 'LTO-6', 2500000000000)",
            [barcode],
        )
        .unwrap();
        conn.last_insert_rowid()
    }

    /// A contact on `drive`, naming `cartridge` and the chip's load count.
    fn contact_at(
        conn: &Connection,
        operation: &str,
        drive: i64,
        cartridge: Option<i64>,
        loads: Option<i64>,
    ) -> i64 {
        conn.execute(
            "INSERT INTO cartridge_contacts (operation, device, drive_id, cartridge_id,
                                             chip_load_count)
             VALUES (?1, '/dev/nst0', ?2, ?3, ?4)",
            rusqlite::params![operation, drive, cartridge, loads],
        )
        .unwrap();
        conn.last_insert_rowid()
    }

    #[test]
    fn page_0x02_parses_to_corrections_and_data_sets_from_the_real_captures() {
        // Negative control: read "Errors corrected without substantial
        // delay" (920) instead and the first assertion fails.
        assert_eq!(
            WriteCounters::from_decoded_0x02(HP_NOMEDIA_0X02),
            WriteCounters {
                corrections: 306_488,
                data_sets: 12_820,
            }
        );
        assert!(
            HP_NOMEDIA_0X02.contains("Total times correction algorithm processed = 306488"),
            "positive control: the fixture carries the line being parsed"
        );
        assert_eq!(
            WriteCounters::from_decoded_0x02(HP_LOADED_0X02),
            WriteCounters::default(),
            "a cartridge just loaded: the page starts from zero (HP-V3 p63)"
        );
        assert_eq!(
            WriteCounters::from_decoded_0x02(MHVTL_0X02),
            WriteCounters::default()
        );
    }

    #[test]
    fn the_drive_phase_runs_fall_either_side_of_the_jump_threshold() {
        // Finding 2's page 0x02 deltas, each over the same 21.5 GB. Negative
        // control: a threshold of 1,100 per GiB lets R9-L2 (1,036) through;
        // one of 7 flags P0a (7.04).
        let run = |count| Corrections {
            count,
            data_bytes: DRIVE_PHASE_DATA_BYTES,
            baseline_contact_id: None,
        };
        for clean in [141, 52, 118, 47, 35, 24, 23, 22] {
            assert!(!run(clean).is_jump(), "{clean} is a clean run");
        }
        for jump in [99_027, 153_609, 20_728] {
            assert!(run(jump).is_jump(), "{jump} is a jump");
        }
        assert_eq!(format!("{:.2}", run(141).per_gib()), "7.04");
        assert_eq!(format!("{:.1}", run(20_728).per_gib()), "1035.6");
    }

    #[test]
    fn the_corrections_are_the_reading_less_the_previous_contact_in_the_same_load() {
        // `volume write --init`: init's contact, then the write's, one load.
        // An OLDER contact on the same drive and load is not the baseline.
        let conn = crate::db::open_memory().unwrap();
        let d = drive(&conn, "HP");
        let c = cartridge(&conn, "L6-0001");
        let older = contact_at(&conn, "drive poll", d, Some(c), Some(7));
        journal_0x02(&conn, older, 9, 1);
        let init = contact_at(&conn, "volume init", d, Some(c), Some(7));
        journal_0x02(&conn, init, 141, 3);
        let write = contact_at(&conn, "volume write", d, Some(c), Some(7));
        journal_0x02(&conn, write, 141 + 20_728, 8_700);
        let got = corrections_across_write(&conn, write, DRIVE_PHASE_DATA_BYTES)
            .unwrap()
            .expect("a witnessed baseline");
        assert_eq!(
            got,
            Corrections {
                count: 20_728,
                data_bytes: DRIVE_PHASE_DATA_BYTES,
                baseline_contact_id: Some(init),
            }
        );
        assert!(got.is_jump());
    }

    #[test]
    fn the_first_contact_of_a_load_on_an_hp_drive_counts_from_zero() {
        // HP-V3 p63: page 0x02 is "cleared when the tape is unloaded". Three
        // ways a write is the first contact of its load: nothing before it
        // on the drive, another cartridge before it, the same cartridge at an
        // earlier load count.
        let conn = crate::db::open_memory().unwrap();
        let c = cartridge(&conn, "L6-0001");
        let other = cartridge(&conn, "L6-0002");

        let d1 = drive(&conn, "HP");
        let alone = contact_at(&conn, "volume write", d1, Some(c), Some(4));

        let d2 = drive(&conn, "HP");
        let prev = contact_at(&conn, "volume write", d2, Some(other), Some(9));
        journal_0x02(&conn, prev, 50_000, 9_000);
        let after_other = contact_at(&conn, "volume write", d2, Some(c), Some(5));

        let d3 = drive(&conn, "HP");
        let prev = contact_at(&conn, "volume write", d3, Some(c), Some(5));
        journal_0x02(&conn, prev, 50_000, 9_000);
        let reloaded = contact_at(&conn, "volume write", d3, Some(c), Some(6));

        for write in [alone, after_other, reloaded] {
            journal_0x02(&conn, write, 141, 8_700);
            assert_eq!(
                corrections_across_write(&conn, write, DRIVE_PHASE_DATA_BYTES).unwrap(),
                Some(Corrections {
                    count: 141,
                    data_bytes: DRIVE_PHASE_DATA_BYTES,
                    baseline_contact_id: None,
                }),
                "contact {write}"
            );
        }
    }

    #[test]
    fn the_corrections_are_not_known_without_a_witnessed_baseline() {
        let conn = crate::db::open_memory().unwrap();
        let c = cartridge(&conn, "L6-0001");
        let unknown = |conn: &Connection, write| {
            corrections_across_write(conn, write, DRIVE_PHASE_DATA_BYTES).unwrap()
        };

        // Another vendor's first contact of a load: no statement that its
        // page 0x02 clears at unload.
        let ibm = drive(&conn, "IBM");
        let w = contact_at(&conn, "volume write", ibm, Some(c), Some(1));
        journal_0x02(&conn, w, 141, 8_700);
        assert_eq!(unknown(&conn, w), None, "non-HP, first of its load");
        // Positive control: the same drive with a witnessed baseline IS known.
        let w2 = contact_at(&conn, "volume write", ibm, Some(c), Some(1));
        journal_0x02(&conn, w2, 200, 9_000);
        assert_eq!(unknown(&conn, w2).map(|c| c.count), Some(59));

        // The previous contact in the same load journalled no page 0x02 (it
        // took no sweep): nothing older stands in for it.
        let hp = drive(&conn, "HP");
        let swept = contact_at(&conn, "volume init", hp, Some(c), Some(2));
        journal_0x02(&conn, swept, 10, 1);
        let _unswept = contact_at(&conn, "volume init", hp, Some(c), Some(2));
        let w = contact_at(&conn, "volume write", hp, Some(c), Some(2));
        journal_0x02(&conn, w, 20_000, 8_700);
        assert_eq!(unknown(&conn, w), None, "the previous contact has no 0x02");

        // A NULL load count, this side or the previous one; a previous
        // contact naming no cartridge.
        let hp = drive(&conn, "HP");
        let w = contact_at(&conn, "volume write", hp, Some(c), None);
        journal_0x02(&conn, w, 141, 8_700);
        assert_eq!(unknown(&conn, w), None, "no load count on the write");
        let hp = drive(&conn, "HP");
        let prev = contact_at(&conn, "volume init", hp, Some(c), None);
        journal_0x02(&conn, prev, 10, 1);
        let w = contact_at(&conn, "volume write", hp, Some(c), Some(3));
        journal_0x02(&conn, w, 141, 8_700);
        assert_eq!(unknown(&conn, w), None, "no load count on the previous");
        let hp = drive(&conn, "HP");
        let prev = contact_at(&conn, "drive poll", hp, None, None);
        journal_0x02(&conn, prev, 10, 1);
        let w = contact_at(&conn, "volume write", hp, Some(c), Some(3));
        journal_0x02(&conn, w, 141, 8_700);
        assert_eq!(unknown(&conn, w), None, "the previous names no cartridge");

        // The counter went down between the readings, or the data sets did
        // not rise (mhvtl's zeros): the difference is not this write's.
        let hp = drive(&conn, "HP");
        let prev = contact_at(&conn, "volume init", hp, Some(c), Some(4));
        journal_0x02(&conn, prev, 500, 10);
        let w = contact_at(&conn, "volume write", hp, Some(c), Some(4));
        journal_0x02(&conn, w, 100, 8_700);
        assert_eq!(unknown(&conn, w), None, "corrections went down");
        let hp = drive(&conn, "HP");
        let prev = contact_at(&conn, "volume init", hp, Some(c), Some(4));
        journal_page(&conn, prev, 0x02, MHVTL_0X02);
        let w = contact_at(&conn, "volume write", hp, Some(c), Some(4));
        journal_page(&conn, w, 0x02, MHVTL_0X02);
        assert_eq!(unknown(&conn, w), None, "no data set counted");

        // The write's own sweep journalled no page 0x02.
        let hp = drive(&conn, "HP");
        let w = contact_at(&conn, "volume write", hp, Some(c), Some(4));
        assert_eq!(unknown(&conn, w), None, "no 0x02 at the write");
    }

    #[test]
    fn repositions_name_the_host_and_only_repositions_do() {
        // Item 170's rule. Negative controls: name the host on any ratio
        // above the threshold and the second and third lines fail; let a
        // correction jump outrank repositions and the first does.
        let t = CORRECTIONS_JUMP_PER_GIB;
        assert_eq!(judge_cause(Some(2), Some(5_000.0), t), Cause::Repositions);
        assert_eq!(judge_cause(Some(0), Some(5_000.0), t), Cause::Corrections);
        assert_eq!(judge_cause(None, Some(5_000.0), t), Cause::Corrections);
        assert_eq!(judge_cause(Some(0), Some(7.0), t), Cause::Unattributed);
        assert_eq!(judge_cause(None, None, t), Cause::Unattributed);
        assert_eq!(
            judge_cause(Some(0), Some(t), t),
            Cause::Unattributed,
            "at, not above"
        );
    }

    #[test]
    fn each_cause_has_its_own_text() {
        let jump = Corrections {
            count: 99_027,
            data_bytes: 1_000_000_000,
            baseline_contact_id: Some(4),
        };
        let clean = Corrections { count: 5, ..jump };
        let head = "warning: volume \"V1\": the drive used 1480000000 bytes of native tape \
                    (1480 MB, page 0x0c BOP to EOD) for 1000000000 bytes of data sent -- 1.480 \
                    native bytes per data byte, above the 1.05 threshold.";
        // Pinned verbatim: the operator-facing texts. Negative control: any
        // rewording fails here, deliberately.
        let cases = [
            (
                Evidence {
                    repositions_per_100mb: Some(0),
                    corrections: Some(jump),
                },
                " The drive corrected heavily while the feed was steady (page 0x34: 0 \
                 repositions per 100 MB; page 0x02: 99027 corrections, 106329.4 per GiB \
                 written, above 100); this is about the drive or the cartridge, not the host: \
                 run a cleaning cartridge (`tapectl drive cleaned` records it), and check the \
                 medium if it recurs. The volume is complete and correct, and the next tape \
                 will hold less than planned if it continues.",
            ),
            (
                Evidence {
                    repositions_per_100mb: None,
                    corrections: Some(jump),
                },
                " The drive corrected heavily (page 0x02: 99027 corrections, 106329.4 per GiB \
                 written, above 100); this is about the drive or the cartridge, not the host: \
                 run a cleaning cartridge (`tapectl drive cleaned` records it), and check the \
                 medium if it recurs. The volume is complete and correct, and the next tape \
                 will hold less than planned if it continues.",
            ),
            (
                Evidence {
                    repositions_per_100mb: Some(0),
                    corrections: Some(clean),
                },
                " No cause is named: the drive reported 0 repositions per 100 MB (page 0x34) \
                 and 5 write corrections, 5.4 per GiB, within 100 (page 0x02). The volume is \
                 complete and correct, and the next tape will hold less than planned if it \
                 continues.",
            ),
            (
                Evidence::default(),
                " No cause is named: the drive reported no repositions figure and no known \
                 count of its write corrections (page 0x02). The volume is complete and \
                 correct, and the next tape will hold less than planned if it continues.",
            ),
        ];
        for (evidence, tail) in cases {
            let text = assessed_at_1_48(evidence).warning_text("V1");
            assert_eq!(text, format!("{head}{tail}"), "{evidence:?}");
            assert!(
                !text.contains("about the host") && !text.contains("irregular feed"),
                "only repositions name the host: {text}"
            );
        }
    }

    /// An HP drive's `volume write --init`: init's contact, then the write's,
    /// one load; the write's sweep journals pages 0x0c, 0x34 and 0x02.
    fn hp_write_in_one_load(
        conn: &Connection,
        native_mb: u64,
        repositions: u8,
        corrections: i64,
    ) -> (i64, i64, i64) {
        let vid = volume(conn, "L6-SHP");
        let d = drive(conn, "HP");
        let c = cartridge(conn, "EW7VWMVKF6");
        let init = contact_at(conn, "volume init", d, Some(c), Some(3));
        journal_0x02(conn, init, 30, 2);
        let write = contact_at(conn, "volume write", d, Some(c), Some(3));
        journal_0x0c(conn, write, Some(&synthetic_0x0c(native_mb)), true);
        journal_0x34(conn, write, repositions);
        journal_0x02(conn, write, 30 + corrections, 8_700);
        (vid, init, write)
    }

    #[test]
    fn a_correction_jump_with_no_repositions_names_the_drive_and_is_recorded() {
        // R3's shape: 1.0599, 0 repositions, 99,027 corrections.
        let conn = crate::db::open_memory().unwrap();
        let (vid, init, write) = hp_write_in_one_load(&conn, 22_778, 0, 99_027);
        let a = assess_and_record(
            &conn,
            Some(write),
            vid,
            "L6-SHP",
            DRIVE_PHASE_DATA_BYTES,
            None,
        )
        .unwrap();
        assert!(a.warns());
        assert_eq!(a.evidence.cause(), Cause::Corrections);
        let text = a.warning_text("L6-SHP");
        assert!(
            text.contains("The drive corrected heavily while the feed was steady"),
            "{text}"
        );
        assert!(
            text.contains("The volume is complete and correct"),
            "{text}"
        );
        let d: serde_json::Value =
            serde_json::from_str(events_for(&conn, vid)[0].2.as_deref().unwrap()).unwrap();
        assert_eq!(d[CORRECTIONS_KEY], 99_027, "{d}");
        assert_eq!(
            format!("{:.1}", d[CORRECTIONS_PER_GIB_KEY].as_f64().unwrap()),
            "4947.7"
        );
        assert_eq!(d[CORRECTIONS_THRESHOLD_KEY], CORRECTIONS_JUMP_PER_GIB);
        assert_eq!(d[CORRECTIONS_BASELINE_KEY], init);
        assert_eq!(d[REPOSITIONS_KEY], 0, "beside the repositions");
    }

    #[test]
    fn repositions_name_the_host_beside_any_corrections() {
        let conn = crate::db::open_memory().unwrap();
        let (vid, _, write) = hp_write_in_one_load(&conn, 22_778, 4, 99_027);
        let a = assess_and_record(
            &conn,
            Some(write),
            vid,
            "L6-SHP",
            DRIVE_PHASE_DATA_BYTES,
            None,
        )
        .unwrap();
        assert_eq!(a.evidence.cause(), Cause::Repositions);
        assert!(a
            .warning_text("L6-SHP")
            .contains("this is a warning about the host"));
    }

    #[test]
    fn a_clean_write_above_the_ratio_names_no_cause() {
        let conn = crate::db::open_memory().unwrap();
        let (vid, _, write) = hp_write_in_one_load(&conn, 22_778, 0, 141);
        let a = assess_and_record(
            &conn,
            Some(write),
            vid,
            "L6-SHP",
            DRIVE_PHASE_DATA_BYTES,
            None,
        )
        .unwrap();
        assert!(a.warns());
        assert_eq!(a.evidence.cause(), Cause::Unattributed);
        assert!(a.warning_text("L6-SHP").contains("No cause is named"));
        let d: serde_json::Value =
            serde_json::from_str(events_for(&conn, vid)[0].2.as_deref().unwrap()).unwrap();
        assert_eq!(
            d[CORRECTIONS_KEY], 141,
            "a known count is recorded, jump or not"
        );
    }

    #[test]
    fn no_warning_below_the_ratio_threshold_whatever_the_corrections() {
        // R5r's ratio (0.9998) with R5w's corrections: recorded, not warned.
        // Negative control: warn on a correction jump alone and `warns()` is
        // true here.
        let conn = crate::db::open_memory().unwrap();
        let (vid, _, write) = hp_write_in_one_load(&conn, 21_487, 0, 153_609);
        let a = assess_and_record(
            &conn,
            Some(write),
            vid,
            "L6-SHP",
            DRIVE_PHASE_DATA_BYTES,
            None,
        )
        .unwrap();
        assert!(!a.assessment.ratio.exceeds_threshold());
        assert!(
            a.evidence.corrections.unwrap().is_jump(),
            "positive control"
        );
        assert!(!a.warns());
        let d: serde_json::Value =
            serde_json::from_str(events_for(&conn, vid)[0].2.as_deref().unwrap()).unwrap();
        assert_eq!(d["warned"], false);
        assert_eq!(d[CORRECTIONS_KEY], 153_609);
    }

    #[test]
    fn unknown_corrections_record_no_keys() {
        // The keys are ABSENT, not null, when the corrections are not known
        // (here: the write's contact names no drive).
        let conn = crate::db::open_memory().unwrap();
        let vid = volume(&conn, "V1");
        let cid = contact(&conn);
        journal_0x0c(&conn, cid, Some(&synthetic_0x0c(1_480)), true);
        journal_0x02(&conn, cid, 99_027, 400);
        let a = assess_and_record(&conn, Some(cid), vid, "V1", 1_000_000_000, None).unwrap();
        assert_eq!(a.evidence.corrections, None);
        let d: serde_json::Value =
            serde_json::from_str(events_for(&conn, vid)[0].2.as_deref().unwrap()).unwrap();
        for key in [
            CORRECTIONS_KEY,
            CORRECTIONS_PER_GIB_KEY,
            CORRECTIONS_THRESHOLD_KEY,
            CORRECTIONS_BASELINE_KEY,
        ] {
            assert!(d.get(key).is_none(), "{key}: {d}");
        }
    }

    #[test]
    fn the_end_sweep_stays_what_every_reader_reads() {
        // Item 170 adds no read of the drive: the write's contact keeps ONE
        // journal row per page, its post-command sweep's, and every reader
        // of a contact's journal reads that row, never the baseline
        // contact's. Negative controls: a pre-data page 0x02 read journalled
        // against the write's contact makes the per-page count 2; a reader
        // keyed on "the newest row" rather than the contact reads the
        // baseline's 0x0c (inserted LAST below) and its 1,000 MB.
        let conn = crate::db::open_memory().unwrap();
        let (vid, init, write) = hp_write_in_one_load(&conn, 1_505, 0, 99_027);
        journal_0x0c(&conn, init, Some(&synthetic_0x0c(1_000)), true);
        journal_0x34(&conn, init, 99);
        let rows_before: i64 = conn
            .query_row("SELECT COUNT(*) FROM log_page_journal", [], |r| r.get(0))
            .unwrap();
        assess_and_record(&conn, Some(write), vid, "L6-SHP", 1_000_000_000, None).unwrap();
        let rows_after: i64 = conn
            .query_row("SELECT COUNT(*) FROM log_page_journal", [], |r| r.get(0))
            .unwrap();
        assert_eq!(rows_before, rows_after, "the assessment journals nothing");
        let per_page: Vec<(i64, i64)> = conn
            .prepare(
                "SELECT page_code, COUNT(*) FROM log_page_journal WHERE contact_id = ?1
                  GROUP BY page_code ORDER BY page_code",
            )
            .unwrap()
            .query_map([write], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap()
            .collect::<std::result::Result<_, _>>()
            .unwrap();
        assert_eq!(per_page, vec![(0x02, 1), (0x0c, 1), (0x34, 1)]);
        assert_eq!(
            journalled_0x0c_decode(&conn, write)
                .unwrap()
                .as_deref()
                .and_then(parse_native_bop_to_eod_mb),
            Some(1_505)
        );
        assert_eq!(
            log_pages::journalled_hp_performance(&conn, write)
                .unwrap()
                .and_then(|p| p.repositions_per_100mb),
            Some(0)
        );
        assert_eq!(
            journalled_0x02(&conn, write)
                .unwrap()
                .map(|c| c.corrections),
            Some(30 + 99_027)
        );
    }
}
