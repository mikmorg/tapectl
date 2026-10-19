//! The Heir Kit generator (ADR-0005, contents ratified in ADR-0009, issue #69).
//!
//! This module produces the *artifacts*. It deliberately stops there: the
//! ceremony that gives them their value — printing, sealing into
//! tamper-evident envelopes, distributing across at least two independent
//! failure domains — is physical and belongs to the operator. Issue #69 sat
//! deferred on the belief that the whole of it needed a person; that covered
//! only the second half.
//!
//! Four artifacts land in the output directory:
//!
//! | file | what it is |
//! |---|---|
//! | `COVER.txt` | The plain-text cover sheet. The artifact with the decades-scale claim: no browser, no tooling, readable with `cat`. Carries the escrow IDENTITY (the public half, in retypable Bech32) and a boxed hand-fill area for the escrow SECRET, which `tapectl init` printed once and nothing on this machine holds (ADR-0005, issue #341). The public half decrypts nothing; the sheet says so and tells the operator to write the secret in. It also names the drives that read the sealed volumes' media generations ([`Generation::can_read`], issue #448), lists the places the catalog shelves them beside a hand-fill "Tapes are at" box, and gives RESTORE.sh's tool list. |
//! | `escrow-kit.html` | The same content, self-contained, with the identity as an inline SVG QR and print styling. Exists so the ceremony is one keystroke, because ADR-0005 requires repeating it after every write session and a ceremony with friction is one that gets skipped. |
//! | `catalog.db.age` | The **full** `tapectl.db`, age-encrypted to the escrow recipient. |
//! | `tapectl-<version>` | A copy of the tapectl program that generated the kit (`std::env::current_exe`), mode 0755, so the heir's one tool for reading around a damaged slice does not depend on a repository still existing (ADR-0012 item 109, #485). The cover sheet and the HTML page name it with its size and sha256. |
//!
//! **Why the full database and not #83's filtered `catalog.db`** (ADR-0009):
//! the filtered schema was designed to ride *on tape*, where the sacred
//! invariant forbids tenant and unit names in plaintext, so it carries no
//! `locations` and no `cartridges`. An heir holding it could enumerate what
//! the archive contains but could not learn which cartridge to fetch or which
//! building it is in — the one question the kit exists to answer. It is safe
//! to escrow the whole database because it holds **no secret material**: only
//! `tenants.public_key` is stored, and every private half is a file under
//! `keys/`.

use std::fs;
use std::path::{Path, PathBuf};

use rusqlite::Connection;

use crate::db::{events, queries};
use crate::error::{Result, TapectlError};
use crate::media::Generation;

/// What a generation produced, for the CLI to render.
#[derive(Debug)]
pub struct KitReport {
    pub out_dir: PathBuf,
    pub cover_txt: PathBuf,
    pub html: PathBuf,
    pub catalog_age: PathBuf,
    /// The copy of the tapectl program that generated the kit (#485), in the
    /// output directory as `tapectl-<version>`.
    pub tapectl_copy: PathBuf,
    /// Its sha256 (hex), read back from the copy, as the cover sheet names it.
    pub tapectl_sha256: String,
    /// Its size in bytes, as the cover sheet names it.
    pub tapectl_bytes: u64,
    /// Size of the encrypted catalog bundle, so the operator can sanity-check
    /// that something real was written before carrying it to a printer.
    pub catalog_bytes: u64,
    pub escrow_public_key: String,
    /// Volumes sealed at generation time. Recorded on the event so the
    /// staleness check has a baseline to compare against.
    pub sealed_volumes: i64,
}

/// Generate the Heir Kit into `out_dir`.
///
/// Refuses when no escrow recipient is registered: ADR-0005 makes the escrow
/// identity the thing the whole kit is addressed to, so a kit without one
/// would be a cover sheet pointing at nothing and an unopenable bundle.
pub fn generate(conn: &Connection, db_path: &Path, out_dir: &Path) -> Result<KitReport> {
    let exe = std::env::current_exe().map_err(|e| {
        TapectlError::Other(format!(
            "cannot find the running tapectl program to copy into the kit: {e}. \
             The kit carries the program that made it (ADR-0012 item 109)."
        ))
    })?;
    generate_with_program(conn, db_path, out_dir, &exe)
}

/// [`generate`] with the program to copy named: the running binary in
/// production, a small file in the tests (a copy of a debug test binary is
/// hundreds of megabytes, once for each test).
pub fn generate_with_program(
    conn: &Connection,
    db_path: &Path,
    out_dir: &Path,
    program: &Path,
) -> Result<KitReport> {
    let escrow = queries::escrow_public_key(conn)?.ok_or_else(|| {
        TapectlError::Other(
            "no escrow recipient is registered, so there is nobody to address the kit to \
             (ADR-0005). Create one with `tapectl key generate --escrow`, or adopt an \
             existing public key with `tapectl key import --escrow <age1...>`."
                .into(),
        )
    })?;

    fs::create_dir_all(out_dir)?;
    // The directory holds an encrypted bundle and a cover sheet naming where
    // the tapes live. Neither is secret material, but both are custody
    // documents, so the directory is not world-readable.
    crate::config::secure_path(out_dir, 0o700);

    let cover_txt = out_dir.join("COVER.txt");
    let html = out_dir.join("escrow-kit.html");
    let catalog_age = out_dir.join("catalog.db.age");

    let catalog_bytes = write_encrypted_catalog(conn, db_path, &catalog_age, &escrow)?;

    // The program that made the kit, beside the sheet that names it (#485).
    // Copied before the sheet is rendered: the sheet carries the sha256 and
    // size of the file as it now lies in the kit, read back from the disk.
    let tapectl_copy = out_dir.join(program_file_name());
    let (tapectl_sha256, tapectl_bytes) = copy_program(program, &tapectl_copy)?;

    let sealed_volumes: i64 = conn.query_row(
        "SELECT COUNT(*) FROM volumes WHERE status = 'sealed'",
        [],
        |r| r.get(0),
    )?;

    let warehouse_deposits: i64 =
        conn.query_row("SELECT COUNT(*) FROM volume_deposits", [], |r| r.get(0))?;

    let generations = sealed_generations(conn)?;
    let places = sealed_places(conn)?;

    let facts = KitFacts {
        escrow_public_key: &escrow,
        sealed_volumes,
        catalog_bytes,
        warehouse_deposits,
        generations: &generations,
        places: &places,
        program: Some(ProgramFact {
            file_name: program_file_name(),
            version: env!("CARGO_PKG_VERSION"),
            arch: std::env::consts::ARCH,
            sha256: tapectl_sha256.clone(),
            bytes: tapectl_bytes,
        }),
    };
    fs::write(&cover_txt, render_cover_text(&facts))?;
    fs::write(&html, render_cover_html(&facts)?)?;

    // Issue #69 / ADR-0009: this event is what makes the staleness check
    // possible. `audit` compares volumes sealed since this timestamp against
    // what the kit could reach, and WARNS (never blocks) when paper has
    // fallen behind tape.
    events::log_event(
        conn,
        "system",
        0,
        None,
        "escrow_kit_generated",
        None,
        None,
        None,
        Some(&format!(
            "sealed_volumes={sealed_volumes} catalog_bytes={catalog_bytes}"
        )),
        None,
    )?;

    Ok(KitReport {
        out_dir: out_dir.to_path_buf(),
        cover_txt,
        html,
        catalog_age,
        tapectl_copy,
        tapectl_sha256,
        tapectl_bytes,
        catalog_bytes,
        escrow_public_key: escrow,
        sealed_volumes,
    })
}

/// The kit's copy of the program is named for the release that made it.
fn program_file_name() -> String {
    format!("tapectl-{}", env!("CARGO_PKG_VERSION"))
}

/// Copy `program` to `dest` as an executable (mode 0755, and failing if that
/// cannot be set: an heir who gets a file that will not run learns nothing
/// from a best-effort chmod), and return the sha256 and size of the file as
/// it now lies in the kit. A half-written copy is removed.
fn copy_program(program: &Path, dest: &Path) -> Result<(String, u64)> {
    use std::os::unix::fs::PermissionsExt;
    let copied = (|| -> Result<(String, u64)> {
        fs::copy(program, dest)?;
        fs::set_permissions(dest, fs::Permissions::from_mode(0o755))?;
        let mut hashing = crate::util::HashingReader::new(fs::File::open(dest)?);
        let bytes = std::io::copy(&mut hashing, &mut std::io::sink())?;
        Ok((hashing.finalize_hex(), bytes))
    })();
    copied.map_err(|e| {
        let _ = fs::remove_file(dest);
        TapectlError::Other(format!(
            "could not copy the tapectl program {} into the kit as {}: {e}",
            program.display(),
            dest.display()
        ))
    })
}

/// Snapshot the live database and encrypt it to the escrow recipient.
///
/// The snapshot goes through `rusqlite`'s online backup rather than a file
/// copy: the source may be mid-transaction under WAL, and copying the `.db`
/// alone would produce a torn database whose `-wal` sidecar is missing.
///
/// The intermediate plaintext is a `NamedTempFile` created **inside
/// `out_dir`** — same filesystem, mode 0600, and removed when the guard
/// drops on every path out, including an error. It is not placed in the
/// system temp dir on purpose: that is a different filesystem (often a
/// tmpfs), and this is the operator's whole catalog in plaintext.
fn write_encrypted_catalog(
    conn: &Connection,
    db_path: &Path,
    out_path: &Path,
    escrow_public_key: &str,
) -> Result<u64> {
    let out_dir = out_path.parent().unwrap_or(Path::new("."));
    let plain = tempfile::Builder::new()
        .prefix(".catalog-snapshot-")
        .suffix(".db")
        .tempfile_in(out_dir)?;

    {
        // `db_path` is accepted rather than derived so a caller with an
        // already-open connection cannot disagree with this function about
        // which database is being escrowed.
        debug_assert!(db_path.exists() || cfg!(test));
        let mut dst = Connection::open(plain.path())?;
        let backup = rusqlite::backup::Backup::new(conn, &mut dst)?;
        backup
            .run_to_completion(100, std::time::Duration::from_millis(10), None)
            .map_err(TapectlError::Database)?;
    }

    let encryptor =
        crate::staging::build_encryptor(std::slice::from_ref(&escrow_public_key.to_string()))?;
    let out_file = fs::File::create(out_path)?;
    let mut writer = encryptor
        .wrap_output(out_file)
        .map_err(|e| TapectlError::Encryption(format!("wrap_output failed: {e}")))?;
    let mut reader = fs::File::open(plain.path())?;
    // Streamed, not read-then-encrypt: the catalog grows with the number of
    // archived files (`files` is one row each), and the
    // H9 class of defect in this repo is exactly whole-object buffering.
    std::io::copy(&mut reader, &mut writer)?;
    writer
        .finish()
        .map_err(|e| TapectlError::Encryption(format!("finish failed: {e}")))?;

    crate::config::secure_path(out_path, 0o600);
    Ok(fs::metadata(out_path)?.len())
}

struct KitFacts<'a> {
    escrow_public_key: &'a str,
    sealed_volumes: i64,
    catalog_bytes: u64,
    /// Recorded warehouse deposits (ADR-0006). When there are none the cover
    /// sheet says nothing about warehouses — a tape-only archive does not
    /// need a paragraph about cloud billing, and every unnecessary paragraph
    /// on a one-page emergency sheet costs attention that the reader may not
    /// have to spare.
    warehouse_deposits: i64,
    /// The media generation of each sealed volume, grouped (#448): what the
    /// reading drive must be able to read. Parsed from `volumes.media_type`.
    generations: &'a [GenerationFact],
    /// Where the catalog says the sealed volumes are (#448), grouped by
    /// place, as a starting point for the hand-filled "Tapes are at" box.
    places: &'a [PlaceFact],
    /// The copy of the tapectl program in the kit (#485); `None` renders no
    /// mention of one (the unit tests of the other paragraphs).
    program: Option<ProgramFact>,
}

/// The tapectl program the kit carries, as the cover sheet names it.
#[derive(Debug, Clone, PartialEq, Eq)]
struct ProgramFact {
    /// `tapectl-<version>`, the file's name in the kit directory.
    file_name: String,
    /// The release that made the kit: `CARGO_PKG_VERSION`, whose tag in the
    /// repository is `v` followed by it.
    version: &'static str,
    /// The machine type the program was built for (`std::env::consts::ARCH`).
    arch: &'static str,
    /// Hex sha256 of the copy in the kit, read back from the disk.
    sha256: String,
    /// Its size in bytes.
    bytes: u64,
}

/// A group of sealed volumes written on one media generation.
#[derive(Debug, Clone, PartialEq, Eq)]
struct GenerationFact {
    /// `Some` when `volumes.media_type` names an LTO generation
    /// ([`Generation::parse`]); `None` for anything else.
    generation: Option<Generation>,
    /// The `media_type` as the catalog stores it, for the `None` case.
    stored: String,
    volumes: i64,
}

/// Sealed volumes the catalog shelves in one place (`None`: no place
/// recorded). Each volume is its label and, when a cartridge is bound, the
/// barcode on its sticker.
#[derive(Debug, Clone, PartialEq, Eq)]
struct PlaceFact {
    place: Option<String>,
    volumes: Vec<(String, Option<String>)>,
}

/// Every drive generation, in release order. `Lto7M8` is a media format an
/// LTO-8 drive writes, never a drive, so it is not here ([`Generation::can_read`]
/// is false for it as a drive). `a_new_generation_must_be_considered_here`
/// fails to compile when a variant is added.
const DRIVE_GENERATIONS: [Generation; 10] = [
    Generation::Lto1,
    Generation::Lto2,
    Generation::Lto3,
    Generation::Lto4,
    Generation::Lto5,
    Generation::Lto6,
    Generation::Lto7,
    Generation::Lto8,
    Generation::Lto9,
    Generation::Lto10,
];

/// Position of `g` in release order, for sorting (`Generation` is
/// deliberately not `Ord`).
fn release_order(g: Generation) -> usize {
    match g {
        Generation::Lto1 => 1,
        Generation::Lto2 => 2,
        Generation::Lto3 => 3,
        Generation::Lto4 => 4,
        Generation::Lto5 => 5,
        Generation::Lto6 => 6,
        Generation::Lto7 => 7,
        Generation::Lto7M8 => 8,
        Generation::Lto8 => 9,
        Generation::Lto9 => 10,
        Generation::Lto10 => 11,
    }
}

/// The media generations of the sealed volumes, grouped, in release order
/// (anything [`Generation::parse`] does not know last, by stored text).
/// `volumes.media_type` is the generation detected at `volume init` (ADR-0010).
fn sealed_generations(conn: &Connection) -> Result<Vec<GenerationFact>> {
    let mut stmt = conn.prepare(
        "SELECT media_type, COUNT(*) FROM volumes
          WHERE status = 'sealed' AND media_type IS NOT NULL AND media_type != ''
          GROUP BY media_type",
    )?;
    let rows = stmt
        .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    let mut out: Vec<GenerationFact> = Vec::new();
    for (stored, volumes) in rows {
        let generation = Generation::parse(&stored);
        // Two spellings of one generation ("LTO6", "LTO-6") are one group.
        if let Some(g) = generation {
            if let Some(existing) = out.iter_mut().find(|f| f.generation == Some(g)) {
                existing.volumes += volumes;
                continue;
            }
        }
        out.push(GenerationFact {
            generation,
            stored,
            volumes,
        });
    }
    out.sort_by(|a, b| {
        let key = |f: &GenerationFact| match f.generation {
            Some(g) => (release_order(g), String::new()),
            None => (usize::MAX, f.stored.clone()),
        };
        key(a).cmp(&key(b))
    });
    Ok(out)
}

/// The sealed volumes by the place the catalog shelves them, named places
/// alphabetically and the volumes with no recorded place last.
fn sealed_places(conn: &Connection) -> Result<Vec<PlaceFact>> {
    let mut stmt = conn.prepare(
        "SELECT l.name, v.label, c.barcode
           FROM volumes v
           LEFT JOIN locations l ON l.id = v.location_id
           LEFT JOIN cartridge_volumes cv ON cv.volume_id = v.id
           LEFT JOIN cartridges c ON c.id = cv.cartridge_id
          WHERE v.status = 'sealed'
          ORDER BY l.name IS NULL, l.name, v.label",
    )?;
    let rows = stmt
        .query_map([], |r| {
            Ok((
                r.get::<_, Option<String>>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, Option<String>>(2)?,
            ))
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    let mut out: Vec<PlaceFact> = Vec::new();
    for (place, label, barcode) in rows {
        match out.last_mut() {
            Some(last) if last.place == place => last.volumes.push((label, barcode)),
            _ => out.push(PlaceFact {
                place,
                volumes: vec![(label, barcode)],
            }),
        }
    }
    Ok(out)
}

/// The drives that can read media of generation `media`, in release order
/// ([`Generation::can_read`]). `pub(crate)`: File 1 (`volume::layout`) says the
/// same thing for the tape it is written on, from this one list.
pub(crate) fn drives_that_read(media: Generation) -> Vec<Generation> {
    DRIVE_GENERATIONS
        .iter()
        .copied()
        .filter(|d| Generation::can_read(*d, media))
        .collect()
}

/// "an LTO-6 or LTO-7 drive", "an LTO-5, LTO-6 or LTO-7 drive".
pub(crate) fn drive_phrase(drives: &[Generation]) -> String {
    let names: Vec<&str> = drives.iter().map(|g| g.as_str()).collect();
    match names.as_slice() {
        [] => "no current drive".to_string(),
        [one] => format!("an {one} drive"),
        [init @ .., last] => format!("an {} or {last} drive", init.join(", ")),
    }
}

/// Lay `items` out after `lead` in lines of at most `width` columns, each
/// continuation line indented `hang` spaces. An item is never split.
fn wrap_items(lead: &str, items: &[String], hang: usize, width: usize) -> String {
    let mut out = String::new();
    let mut line = lead.to_string();
    for (i, item) in items.iter().enumerate() {
        let piece = if i + 1 < items.len() {
            format!("{item},")
        } else {
            item.clone()
        };
        if line.chars().count() + 1 + piece.chars().count() > width {
            out.push_str(&line);
            out.push('\n');
            line = format!("{}{piece}", " ".repeat(hang));
        } else {
            line.push(' ');
            line.push_str(&piece);
        }
    }
    out.push_str(&line);
    out
}

/// The generations paragraph of WHAT TO DO FIRST item 2: which drives read
/// the cartridges the catalog knew when the sheet was made (#448).
fn generations_text(generations: &[GenerationFact]) -> String {
    if generations.is_empty() {
        return "\
   The catalog lists no sealed cartridge, so this sheet cannot say
   which drives read them. The cartridge's label names its
   generation (LTO-6, LTO-7, ...): look up which drives read it in
   the maker's compatibility table.
"
        .to_string();
    }
    let mut lines = String::from(
        "   The tapes the catalog listed when this sheet was made, and
   the drives that read them:

",
    );
    for f in generations {
        match f.generation {
            Some(g) => {
                lines.push_str(&format!(
                    "     {} ({} volume(s)): {}\n",
                    g.as_str(),
                    f.volumes,
                    drive_phrase(&drives_that_read(g)),
                ));
            }
            None => {
                lines.push_str(&format!(
                    "     {} ({} volume(s)): not a generation this sheet knows;\n",
                    f.stored, f.volumes
                ));
                lines.push_str("       look it up in the maker's compatibility table\n");
            }
        }
    }
    lines.push_str(
        "
   A NEWER drive often cannot read an OLDER cartridge: an LTO-8
   drive reads only LTO-7, LTO-7 Type M and LTO-8, for example.
",
    );
    lines
}

/// The "where the tapes are" section: the places the catalog knew, then a
/// hand-fill box (#448). Cross-checked by `escrow_kit_stale`, which warns
/// when a tape moves after the kit was made.
fn places_text(places: &[PlaceFact]) -> String {
    let mut known = String::new();
    if places.is_empty() {
        known.push_str("The catalog lists no sealed cartridge yet.\n");
    } else {
        known.push_str(
            "The catalog knew these places when this sheet was made.
Places change: cross out what is no longer true.

",
        );
        for p in places {
            let items: Vec<String> = p
                .volumes
                .iter()
                .map(|(label, barcode)| match barcode {
                    Some(b) => format!("{label} ({b})"),
                    None => label.clone(),
                })
                .collect();
            let lead = format!(
                "    {}:",
                p.place.as_deref().unwrap_or("(no place recorded)")
            );
            known.push_str(&wrap_items(&lead, &items, 8, 64));
            known.push('\n');
        }
    }
    format!(
        "\
WHERE THE TAPES ARE -- FILL THIS IN
-----------------------------------
{known}
  +------------------------------------------------------------+
  |                                                            |
  |  Tapes are at:                                             |
  |                                                            |
  |  ______________________________________________________    |
  |                                                            |
  |  ______________________________________________________    |
  |                                                            |
  +------------------------------------------------------------+

  Written by: ______________________  Date: ________

"
    )
}

/// The warehouse paragraph, present only when deposits exist.
///
/// `docs/operator-guide.md` records this as an obligation on the kit: the
/// billing fragility and the retrieval wait "are exactly what the printed
/// Heir Kit must say, because an heir will meet this copy with no context at
/// all". Discharged here.
fn warehouse_section(deposits: i64) -> String {
    if deposits == 0 {
        return String::new();
    }
    format!(
        "\
ABOUT THE CLOUD COPIES
----------------------
The catalog records {deposits} copy/copies held by a commercial
storage provider, in addition to the tapes. Two things an heir
must know about those, because they are not obvious:

  * THEY STOP EXISTING IF NOBODY PAYS. A storage account whose
    bills go unpaid is deleted, typically within weeks. The
    tapes have no such dependency -- they keep working in a
    drawer. If you are settling an estate, do not cancel the
    payment method before retrieving the data.

  * RETRIEVAL IS NOT INSTANT. These copies are usually in a
    \"cold\" storage class: requesting a file can take many hours
    before the download will even start, and costs money per
    retrieval. A tape in hand is faster than a cloud copy.

Nobody has re-checked these copies since they were uploaded.
Treat the tapes as the authoritative copy.

"
    )
}

/// The cover sheet, as plain text.
///
/// Written future-self-first, per ADR-0005: the reader may be the operator in
/// twenty years or an heir who has never heard of tapectl, so it opens with
/// what the tapes are and what to do first, not with terminology. Pure so it
/// is testable without touching a filesystem.
///
/// The sheet prints the escrow PUBLIC key only. Until issue #341 it called
/// that value "the key" without which "the tapes cannot be decrypted" — false,
/// a public key decrypts nothing, and the secret half is shown exactly once by
/// `tapectl init` and stored nowhere (ADR-0005). The sheet now says what the
/// printed value is (the identity: WHICH secret is needed) and carries a
/// hand-fill box for the secret, so the ceremony cannot skip the one thing
/// that makes the kit worth anything. The box geometry is pinned by the
/// `the_secret_box_has_exactly_one_cell_per_secret_character` test.
fn render_cover_text(f: &KitFacts) -> String {
    format!(
        "\
================================================================
                    ARCHIVE RECOVERY KIT
================================================================

WHAT THIS IS
------------
Somewhere with this sheet there are one or more magnetic tape
cartridges (LTO). They hold a long-term backup of personal files:
documents, photos, and similar. The tapes are encrypted. Opening
them takes a SECRET that was shown exactly once, on the screen
of the person who set this archive up, and was never stored on
any computer. This sheet does three things:

  1. It names WHICH secret is needed: the escrow identity
     printed below. That printed value opens nothing by itself.
  2. It has a box for the secret to be written in BY HAND.
  3. It has a box for WHERE THE TAPES ARE, to keep current.

With the box filled in, this sheet is the key to the tapes. If
the box is empty, look for the secret on another piece of paper
in the same envelope. Without the secret the tapes cannot be
decrypted by anyone -- including the people who made them.

THE ESCROW IDENTITY (public half -- says WHICH secret is needed)
----------------------------------------------------------------
This is the PUBLIC half of the escrow key pair. It is safe to
show to anyone and it cannot decrypt anything. It is printed so
that a secret you find can be checked against it (see HOW TO
CHECK THE PAIR) and so that a rebuilt machine can register the
same identity (tapectl init --escrow-public-key).

    {key}

It is case-insensitive and has a built-in checksum, so a
mistyped character is rejected rather than silently accepted.
(The HTML version of this sheet also carries it as a QR code.)

THE ESCROW SECRET -- WRITE IT HERE
----------------------------------
`tapectl init` showed the secret ONCE, when this archive was
created. Whoever ran it copies it into this box by hand, in
CAPITALS, one character per cell, and checks it character by
character. It is 74 characters long and always begins
AGE-SECRET-KEY-1 (already printed). After that prefix the
letters B, I and O and the digit 1 never occur.

  +------------------------------------------------------------+
  |                                                            |
  |  AGE-SECRET-KEY-1 _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _  |
  |                                                            |
  |                   _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _  |
  |                                                            |
  |                   _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _      |
  |                                                            |
  +------------------------------------------------------------+

  Written and checked by: ______________________  Date: ________

WITHOUT THE SECRET THIS SHEET OPENS NOTHING. The tapes are
encrypted to it, and so is catalog.db.age in this envelope.

{places}HOW TO CHECK THE PAIR
---------------------
Type the secret, alone, on one line of a file (say escrow.key)
and run:

    age-keygen -y escrow.key

It prints an age1... value. That value must be identical to the
escrow identity printed above. If it is not, either a character
was miscopied (a wrong one is refused with \"invalid checksum\")
or this is the secret of a different archive. The same file is
what every recovery tool takes as its key:

    ./RESTORE.sh --list --key escrow.key
    ./RESTORE.sh --restore --key escrow.key --all --to ./out
    age -d -i escrow.key -o catalog.db catalog.db.age
    tapectl catalog rebuild --from-volume --device /dev/nst0 \\
        --key escrow.key

(/dev/nst0 is only an example: list the drives on the machine you
are using with `ls -l /dev/tape/by-id/` and use yours everywhere.)

One key can open only PART of a tape: a tenant's key opens that
tenant's envelope alone. The escrow key opens every envelope, so
--all with the escrow key is the way to get everything: it
restores every unit, each into its own folder under ./out.
Without --all, RESTORE.sh uses the first envelope the key opens;
if that lists one unit, only that unit is restored and nothing
is said of the rest of the tape. --list first shows what the
tape holds.

A restore needs free disk space of about one unit's size where
--to points (all of them together, with --all): RESTORE.sh
streams each slice from the tape through age into dar, so only
the restored files are written. It measures this before it
reads any slice. Decrypted slices are kept on disk only for
--path (one file or folder) and for a dar older than 2.7.9 (or
than 2.7.21, for a unit with no catalogue); --scratch DIR puts
those on another disk. A tape written by tapectl before 1.1.0
carries an older script that decrypts every slice first and
needs about twice that. Do not restore into /tmp, which is RAM
on many systems.

WHAT ELSE IS IN THIS ENVELOPE
-----------------------------
  catalog.db.age   An encrypted index of what is on the tapes:
                   which files, on which cartridge, in which
                   location. {bytes} bytes. Decrypt it with the
                   SECRET from the box above (see HOW TO CHECK
                   THE PAIR for the command).
{program_entry}
WHAT TO DO FIRST
----------------
1. Do NOT bulk-erase, degauss, or reformat the cartridges.
   They are readable for decades if left alone.

2. You need an LTO tape drive that can READ these cartridges.
{generations}
   The drive connects by SAS (some models by Fibre Channel), so
   the computer needs a matching host adapter (HBA) card; there
   is no USB or SATA tape drive. If you do not have a drive, a
   data-recovery firm does. The tapes are standard hardware;
   nothing about them is proprietary.

3. Every cartridge is SELF-DESCRIBING. Near the start of each
   tape there is a plain-text guide and a script named
   RESTORE.sh that can recover the contents using only common
   tools. You do not need the tapectl program, and you do not
   need this catalog file, to get the data back. Both exist to
   make it easier, not to make it possible.

   RESTORE.sh checks for its tools before it reads the tape and
   names any that is missing. It needs:
     mt     package mt-st (it can set the tape's block size)
     dd, head, truncate, tar, mkfifo, tee, sha256sum
            standard on Linux (coreutils, tar)
     age    age-encryption.org. The files are in the stable age v1
            format, which the `age` program and `rage` both read.
            RESTORE.sh runs a program named `age`: if you have
            only rage, link it under that name.
     dar    dar.linux.free.fr. 2.7.9 or later needs the least
            disk space (see above).
     gpg    package gnupg, only if your dar was built with GPG
            support (Debian's and Ubuntu's is): that dar refuses
            to start without it.

4. If you do have tapectl, the first commands are:

       tapectl restore raw-volume --device /dev/nst0 --to ./out
       tapectl volume identify --device /dev/nst0

   The first works with no database at all. /dev/nst0 is only an
   example: `ls -l /dev/tape/by-id/` lists the drives you have.
{program_hint}
{warehouse}CUSTODY -- FOR WHOEVER MAINTAINS THIS
-------------------------------------
  * Store in a tamper-evident envelope.
  * Keep at least TWO copies in independent failure domains --
    not two shelves in one building.
  * Paper: a UL-350 rated safe. If stored together with tape,
    Class-125 (tape is far less heat-tolerant than paper).
  * REFRESH after each writing session, and when a tape moves
    to another place. The secret opens EVERY tape ever written
    -- it is a recipient of all of them, old and new. What goes
    stale is catalog.db.age in this envelope, and the places
    listed above: they show only the volumes and places that
    existed when this kit was made (see GENERATED). For a tape
    written later, rebuild the catalog from that tape (tapectl
    catalog rebuild --from-volume) or generate a fresh kit.
    Running `tapectl audit` warns when this kit has fallen
    behind.

GENERATED
---------
  Sealed cartridges known at generation time: {sealed}

  This escrow identity is permanent. It is never rotated, and
  rotating the day-to-day keys does not change it -- so the
  identity and the secret on this sheet do not go stale as keys
  change. Only the catalog does.

================================================================
",
        key = f.escrow_public_key,
        bytes = f.catalog_bytes,
        sealed = f.sealed_volumes,
        warehouse = warehouse_section(f.warehouse_deposits),
        generations = generations_text(f.generations),
        places = places_text(f.places),
        program_entry = program_entry(f.program.as_ref()),
        program_hint = program_hint(f.program.as_ref()),
    )
}

/// The list entry for the copy of tapectl in the kit's folder (#485), with
/// the sha256 and size an heir checks the file against. Empty when the kit
/// carries none. The hash is one unbroken line so it can be searched for.
fn program_entry(p: Option<&ProgramFact>) -> String {
    let Some(p) = p else {
        return String::new();
    };
    format!(
        "
  {name:<16} The program that made this kit, a file in the same
                   folder as catalog.db.age (it is not on paper). It
                   is tapectl {version} for Linux on {arch} machines,
                   and its source is at
                   https://github.com/mikmorg/tapectl, tag v{version}.
                   Check the copy against this sheet:

    sha256sum {name}

      sha256  {sha}
      size    {bytes} bytes
",
        name = p.file_name,
        version = p.version,
        arch = p.arch,
        sha = p.sha256,
        bytes = p.bytes,
    )
}

/// What step 4 of WHAT TO DO FIRST adds when the kit carries tapectl.
fn program_hint(p: Option<&ProgramFact>) -> String {
    let Some(p) = p else {
        return String::new();
    };
    format!(
        "
   A copy of tapectl is in this kit's folder: run it as ./{name}
   in place of `tapectl` above. If it will not run on your machine,
   the source link above builds it (File 1 on every tape, in \"tapectl
   itself\", says how).
",
        name = p.file_name,
    )
}

/// The same cover sheet as a self-contained printable page.
///
/// No external resources of any kind — the QR is an inline SVG, the styling
/// is one inline `<style>`. A page that fetched anything would be a page that
/// renders blank in the situation it exists for.
///
/// The QR and the boxed `age1…` at the top are the escrow IDENTITY (public
/// half) and are captioned as such — a black box around a Bech32 string is
/// exactly what an heir would otherwise take for "the key" (issue #341). The
/// hand-fill box for the secret is the one inside the `<pre>` text, so there
/// is a single place to write it on either rendering.
fn render_cover_html(f: &KitFacts) -> Result<String> {
    let qr = qr_svg(f.escrow_public_key)?;
    let text = render_cover_text(f);
    let escaped = html_escape(&text);
    Ok(format!(
        "<!doctype html>
<html lang=\"en\">
<head>
<meta charset=\"utf-8\">
<title>Archive Recovery Kit</title>
<style>
  body {{ font-family: Georgia, 'Times New Roman', serif; margin: 2rem auto;
          max-width: 46rem; line-height: 1.45; }}
  .qr {{ text-align: center; margin: 1.5rem 0; }}
  .qr svg {{ width: 240px; height: 240px; }}
  .label {{ text-align: center; font-weight: bold; letter-spacing: .04em;
            margin: .5rem 0 .25rem; }}
  .caption {{ text-align: center; font-size: .9rem; margin: 0 0 .5rem; }}
  .key {{ font-family: ui-monospace, Menlo, Consolas, monospace;
          font-size: 1.05rem; word-break: break-all; text-align: center;
          border: 2px solid #000; padding: .75rem; margin: 1rem 0; }}
  pre {{ font-family: ui-monospace, Menlo, Consolas, monospace;
         font-size: .82rem; white-space: pre-wrap; }}
  @media print {{ body {{ margin: 0; max-width: none; }} }}
</style>
</head>
<body>
<div class=\"label\">ESCROW IDENTITY (public half)</div>
<div class=\"caption\">Says WHICH secret is needed. It is not the secret and \
decrypts nothing by itself. The secret goes in the box marked WRITE IT HERE \
below.</div>
<div class=\"qr\">{qr}</div>
<div class=\"key\">{key}</div>
<pre>{escaped}</pre>
</body>
</html>
",
        qr = qr,
        key = html_escape(f.escrow_public_key),
        escaped = escaped,
    ))
}

/// Encode `data` as a QR code and render it as a standalone SVG fragment.
fn qr_svg(data: &str) -> Result<String> {
    use qrcode::render::svg;
    use qrcode::QrCode;

    let code = QrCode::new(data.as_bytes())
        .map_err(|e| TapectlError::Other(format!("QR encoding failed: {e}")))?;
    Ok(code
        .render()
        .min_dimensions(240, 240)
        .dark_color(svg::Color("#000000"))
        .light_color(svg::Color("#ffffff"))
        .build())
}

fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    const ESCROW: &str = "age1ql3z7hjy54pw3hyww5ayyfg7zqgvc7w3j2elw8zmrj2kg5sfn9aqmcac8p";

    fn conn_with_escrow() -> Connection {
        // Built through the real helpers, not hand-rolled SQL: the escrow key
        // lives on `encryption_keys` (migration 003), not on `tenants`, and a
        // fixture that guessed otherwise would test nothing.
        let conn = crate::db::open_memory().unwrap();
        let op_id = queries::insert_tenant(&conn, "op", None, true).unwrap();
        queries::insert_escrow_key(&conn, op_id, "op-escrow", ESCROW, ESCROW, None).unwrap();
        conn
    }

    /// A small stand-in for the tapectl binary. `generate` copies the running
    /// program, which under `cargo test` is a test binary of hundreds of
    /// megabytes; these tests name this file to `generate_with_program`.
    fn fake_program() -> tempfile::NamedTempFile {
        let f = tempfile::NamedTempFile::new().unwrap();
        fs::write(f.path(), b"#!/bin/sh\necho a stand-in for tapectl\n").unwrap();
        f
    }

    fn sha256_of(path: &Path) -> String {
        use sha2::Digest;
        format!("{:x}", sha2::Sha256::digest(fs::read(path).unwrap()))
    }

    #[test]
    fn refuses_when_no_escrow_recipient_is_registered() {
        let conn = crate::db::open_memory().unwrap();
        let tmp = TempDir::new().unwrap();
        let err = generate_with_program(
            &conn,
            Path::new("/nonexistent.db"),
            tmp.path(),
            fake_program().path(),
        )
        .expect_err("a kit with nobody to address it to must not be produced");
        let msg = err.to_string();
        assert!(
            msg.contains("key generate --escrow"),
            "the refusal must tell the operator how to fix it; got: {msg}"
        );
    }

    #[test]
    fn produces_all_four_artifacts_and_logs_one_event() {
        let conn = conn_with_escrow();
        let tmp = TempDir::new().unwrap();
        let out = tmp.path().join("kit");

        let report = generate_with_program(
            &conn,
            Path::new("/nonexistent.db"),
            &out,
            fake_program().path(),
        )
        .expect("kit generation");

        assert!(report.cover_txt.exists(), "COVER.txt must exist");
        assert!(report.html.exists(), "the HTML page must exist");
        assert!(report.catalog_age.exists(), "the bundle must exist");
        assert!(
            report.catalog_bytes > 0,
            "an empty bundle is a silent failure, not a kit"
        );

        let events: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM events WHERE action = 'escrow_kit_generated'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(events, 1, "generation must leave exactly one audit event");
    }

    /// The plaintext snapshot is an intermediate. If it survives, the kit
    /// directory holds an unencrypted copy of the whole catalog next to the
    /// encrypted one — which would quietly defeat the encryption.
    #[test]
    fn no_plaintext_snapshot_is_left_behind() {
        let conn = conn_with_escrow();
        let tmp = TempDir::new().unwrap();
        let out = tmp.path().join("kit");
        generate_with_program(
            &conn,
            Path::new("/nonexistent.db"),
            &out,
            fake_program().path(),
        )
        .unwrap();

        let leftovers: Vec<String> = fs::read_dir(&out)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|n| n.contains("catalog-snapshot") || n.ends_with(".db"))
            .collect();
        assert!(
            leftovers.is_empty(),
            "plaintext catalog snapshot left in the kit directory: {leftovers:?}"
        );
    }

    /// The bundle must actually be age ciphertext addressed to the escrow
    /// recipient — not the plaintext database renamed, which would pass every
    /// existence assertion above.
    #[test]
    fn the_bundle_is_real_age_ciphertext_not_a_renamed_database() {
        let conn = conn_with_escrow();
        let tmp = TempDir::new().unwrap();
        let out = tmp.path().join("kit");
        generate_with_program(
            &conn,
            Path::new("/nonexistent.db"),
            &out,
            fake_program().path(),
        )
        .unwrap();

        let bytes = fs::read(out.join("catalog.db.age")).unwrap();
        assert!(
            bytes.starts_with(b"age-encryption.org/"),
            "bundle does not begin with an age header"
        );
        assert!(
            !bytes.starts_with(b"SQLite format 3"),
            "the bundle IS the plaintext database — encryption did not happen"
        );
    }

    /// #485 (ADR-0012 item 109): the kit carries the program that made it, so
    /// the heir's one tool for reading around a damaged slice does not depend
    /// on a repository still existing. The copy is there, it runs (mode 0755),
    /// it is byte for byte the program named, and the cover sheet and the HTML
    /// page name it with the sha256 and size of the file as the kit holds it.
    #[test]
    fn the_kit_carries_the_program_and_both_pages_name_it_with_its_hash() {
        use std::os::unix::fs::PermissionsExt;
        let conn = conn_with_escrow();
        let tmp = TempDir::new().unwrap();
        let out = tmp.path().join("kit");
        let program = fake_program();

        let report =
            generate_with_program(&conn, Path::new("/nonexistent.db"), &out, program.path())
                .expect("kit generation");

        let name = format!("tapectl-{}", env!("CARGO_PKG_VERSION"));
        assert_eq!(report.tapectl_copy, out.join(&name));
        assert!(report.tapectl_copy.is_file(), "the copy is in the kit");
        assert_eq!(
            fs::metadata(&report.tapectl_copy)
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o755,
            "an heir runs it as it lies"
        );
        assert_eq!(
            fs::read(&report.tapectl_copy).unwrap(),
            fs::read(program.path()).unwrap(),
            "the copy is the program, byte for byte"
        );

        // The hash the report and the sheets name is the hash of the file in
        // the kit, computed here independently of the code under test.
        let want = sha256_of(&report.tapectl_copy);
        assert_eq!(report.tapectl_sha256, want);
        assert_eq!(
            report.tapectl_bytes,
            fs::metadata(&report.tapectl_copy).unwrap().len()
        );
        let size_line = format!("size    {} bytes", report.tapectl_bytes);
        for (what, text) in [
            ("COVER.txt", fs::read_to_string(&report.cover_txt).unwrap()),
            ("escrow-kit.html", fs::read_to_string(&report.html).unwrap()),
        ] {
            assert!(text.contains(&name), "{what} names the file {name}");
            assert!(text.contains(&want), "{what} carries the sha256 {want}");
            assert!(text.contains(&size_line), "{what} carries {size_line:?}");
            let flat = flat(&text);
            for needle in [
                "The program that made this kit",
                &format!("for Linux on {} machines", std::env::consts::ARCH),
                "its source is at https://github.com/mikmorg/tapectl",
                &format!("tag v{}", env!("CARGO_PKG_VERSION")),
                &format!("sha256sum {name}"),
                &format!("run it as ./{name} in place of `tapectl` above"),
            ] {
                assert!(flat.contains(needle), "{what} lacks {needle:?}:\n{text}");
            }
        }
    }

    /// The copy is the RUNNING program: `generate` takes no program argument.
    /// (The one test that copies a test binary; every other test names a small
    /// file to `generate_with_program`.)
    #[test]
    fn generate_copies_the_running_program() {
        let conn = conn_with_escrow();
        let tmp = TempDir::new().unwrap();
        let out = tmp.path().join("kit");
        let report = generate(&conn, Path::new("/nonexistent.db"), &out).expect("kit generation");
        let exe = std::env::current_exe().unwrap();
        assert_eq!(report.tapectl_sha256, sha256_of(&exe));
        assert_eq!(report.tapectl_bytes, fs::metadata(&exe).unwrap().len());
    }

    /// A program that cannot be copied is an error that names it, and leaves
    /// no half-written file behind that a sheet could be taken to describe.
    #[test]
    fn a_program_that_cannot_be_copied_refuses_the_kit_by_name() {
        let conn = conn_with_escrow();
        let tmp = TempDir::new().unwrap();
        let out = tmp.path().join("kit");
        let missing = tmp.path().join("no-such-program");
        let err = generate_with_program(&conn, Path::new("/nonexistent.db"), &out, &missing)
            .expect_err("a kit without the program it names must not be produced");
        let msg = err.to_string();
        assert!(msg.contains("could not copy the tapectl program"), "{msg}");
        assert!(msg.contains("no-such-program"), "{msg}");
        assert!(
            !out.join(format!("tapectl-{}", env!("CARGO_PKG_VERSION")))
                .exists(),
            "no partial copy"
        );
        assert!(
            !out.join("COVER.txt").exists(),
            "no cover sheet that names a program that is not there"
        );
    }

    /// With no program to name (the other tests' facts) the sheet says nothing
    /// of one: no file name, no hash, no source line.
    #[test]
    fn a_sheet_with_no_program_says_nothing_of_one() {
        let txt = render_cover_text(&facts_for(&[], &[]));
        assert!(!txt.contains("The program that made this kit"), "{txt}");
        assert!(!txt.contains("sha256sum tapectl-"), "{txt}");
        assert!(!txt.contains("github.com/mikmorg/tapectl"), "{txt}");
        assert!(!txt.contains("A copy of tapectl is in this kit"), "{txt}");
    }

    /// Renamed from `the_cover_sheet_carries_the_key_and_the_custody_rules`
    /// for issue #341: the sheet never carried "the key" — it carries the
    /// escrow PUBLIC key, which decrypts nothing, and until #341 said the
    /// opposite. It now carries the identity, a hand-fill box for the secret,
    /// and the custody rules; the old name would have pinned the false claim.
    #[test]
    fn the_cover_sheet_carries_the_identity_the_secret_box_and_the_custody_rules() {
        let f = KitFacts {
            escrow_public_key: ESCROW,
            sealed_volumes: 3,
            catalog_bytes: 4096,
            warehouse_deposits: 0,
            generations: &[],
            places: &[],
            program: None,
        };
        let txt = render_cover_text(&f);
        assert!(
            txt.contains(ESCROW),
            "the escrow public key must be on the sheet — it is how a found secret is checked"
        );
        for required in [
            "ESCROW IDENTITY",
            "public half",
            "WRITE IT HERE",
            "AGE-SECRET-KEY-1",
            "age-keygen -y",
            "tamper-evident",
            "TWO copies",
            "RESTORE.sh",
            "restore raw-volume",
        ] {
            assert!(
                txt.contains(required),
                "the cover sheet must mention {required:?}"
            );
        }
    }

    /// The exact phrases issue #341 found: each told an heir that the printed
    /// `age1…` was the thing that decrypts the tapes. A blanket ban on "the
    /// key" would be wrong — the sheet legitimately says "the key to the
    /// tapes" about itself once the box is filled — so the pins are the
    /// specific false sentences, with the positive controls in the test above.
    #[test]
    fn the_cover_sheet_never_calls_the_public_key_the_key_that_decrypts() {
        let f = KitFacts {
            escrow_public_key: ESCROW,
            sealed_volumes: 3,
            catalog_bytes: 4096,
            warehouse_deposits: 0,
            generations: &[],
            places: &[],
            program: None,
        };
        let txt = render_cover_text(&f);
        for forbidden in [
            "it is the key.",
            "THE RECOVERY KEY",
            "Without the key printed below",
            "key above",
        ] {
            assert!(
                !txt.contains(forbidden),
                "the cover sheet still says {forbidden:?} about the escrow PUBLIC key (issue #341)"
            );
        }
        assert!(
            txt.contains("opens nothing by itself"),
            "the sheet must say plainly that the printed identity decrypts nothing"
        );
    }

    /// The hand-fill box must hold exactly one age X25519 secret: 74
    /// characters, of which the 16-character prefix `AGE-SECRET-KEY-1` is
    /// pre-printed, leaving 58 cells to write. Measured, not assumed: on this
    /// machine `age-keygen` produces a 74-character secret and refuses a
    /// lowercased or one-character-altered copy (invalid checksum). A box with
    /// the wrong cell count would have the writer run out of room or leave a
    /// gap, and either invites the transcription error the checksum then
    /// catches only at recovery time.
    #[test]
    fn the_secret_box_has_exactly_one_cell_per_secret_character() {
        let f = KitFacts {
            escrow_public_key: ESCROW,
            sealed_volumes: 0,
            catalog_bytes: 1,
            warehouse_deposits: 0,
            generations: &[],
            places: &[],
            program: None,
        };
        let txt = render_cover_text(&f);
        let start = txt
            .find("WRITE IT HERE")
            .expect("the secret section exists");
        let end = txt[start..]
            .find("Written and checked by")
            .expect("the box is closed by the signature line")
            + start;
        let box_lines: Vec<&str> = txt[start..end]
            .lines()
            .filter(|l| l.trim_start().starts_with('|') || l.trim_start().starts_with('+'))
            .collect();
        assert!(
            box_lines.len() >= 5,
            "expected a bordered box of at least two borders and three rows, got {box_lines:?}"
        );
        let widths: std::collections::BTreeSet<usize> =
            box_lines.iter().map(|l| l.chars().count()).collect();
        assert_eq!(
            widths.len(),
            1,
            "every box line must be the same width or the border is ragged: {box_lines:#?}"
        );
        let cells = box_lines
            .iter()
            .map(|l| l.matches('_').count())
            .sum::<usize>();
        assert_eq!(
            cells,
            74 - "AGE-SECRET-KEY-1".len(),
            "the box must have one cell per secret character after the printed prefix"
        );
        assert!(
            box_lines.iter().any(|l| l.contains("AGE-SECRET-KEY-1")),
            "the prefix must be pre-printed inside the box"
        );
    }

    /// `docs/operator-guide.md` records an obligation on this kit: an heir
    /// meeting a cloud copy "with no context at all" must be told the two
    /// things that are not obvious about it — that it is deleted when the
    /// bills stop, and that retrieval is slow and costs money.
    #[test]
    fn the_cover_sheet_warns_about_cloud_copies_when_deposits_exist() {
        let f = KitFacts {
            escrow_public_key: ESCROW,
            sealed_volumes: 2,
            catalog_bytes: 4096,
            warehouse_deposits: 2,
            generations: &[],
            places: &[],
            program: None,
        };
        let txt = render_cover_text(&f);
        assert!(
            txt.contains("IF NOBODY PAYS"),
            "an heir must be told the cloud copy dies when billing stops"
        );
        assert!(
            txt.contains("NOT INSTANT"),
            "an heir must be told cold-storage retrieval is slow"
        );
    }

    /// ...and says nothing at all when there are none. A tape-only archive
    /// does not need a paragraph about cloud billing on a one-page emergency
    /// sheet, where every unnecessary line costs attention.
    #[test]
    fn the_cover_sheet_is_silent_about_cloud_copies_when_there_are_none() {
        let f = KitFacts {
            escrow_public_key: ESCROW,
            sealed_volumes: 2,
            catalog_bytes: 4096,
            warehouse_deposits: 0,
            generations: &[],
            places: &[],
            program: None,
        };
        let txt = render_cover_text(&f);
        assert!(
            !txt.contains("CLOUD"),
            "a tape-only archive must not get a cloud section"
        );
    }

    /// Issue #351: the CUSTODY section told the kit's keeper that a kit made
    /// before newer tapes were written "still opens the older ones and
    /// silently misses the new". False — the escrow identity is a recipient
    /// of every write (ADR-0005), so the secret in the box opens every tape
    /// ever written. What falls behind is only `catalog.db.age`, which the
    /// GENERATED section of the same sheet already said ("Only the catalog
    /// does"). The printed sheet contradicted itself on the one page an heir
    /// relies on; this pins CUSTODY to the truth and to GENERATED.
    #[test]
    fn the_custody_section_says_the_secret_opens_every_tape_and_only_the_catalog_goes_stale() {
        let f = KitFacts {
            escrow_public_key: ESCROW,
            sealed_volumes: 3,
            catalog_bytes: 4096,
            warehouse_deposits: 0,
            generations: &[],
            places: &[],
            program: None,
        };
        let txt = render_cover_text(&f);
        let start = txt
            .find("CUSTODY -- FOR WHOEVER MAINTAINS THIS")
            .expect("the sheet must have a CUSTODY section");
        let end = txt[start..]
            .find("\nGENERATED\n")
            .map(|i| start + i)
            .expect("CUSTODY must be followed by GENERATED");
        let custody_raw = &txt[start..end];
        // The sheet is wrapped at 64 columns; compare prose, not line breaks.
        let custody = custody_raw.split_whitespace().collect::<Vec<_>>().join(" ");

        for required in [
            "opens EVERY tape ever written",
            "catalog.db.age",
            "catalog rebuild --from-volume",
            "generate a fresh kit",
            "tapectl audit",
        ] {
            assert!(
                custody.contains(required),
                "CUSTODY must say {required:?} (issue #351); it says: {custody}"
            );
        }
        for forbidden in ["silently misses", "opens the older ones", "misses the new"] {
            assert!(
                !txt.contains(forbidden),
                "the sheet still says {forbidden:?} — the escrow secret opens every tape; \
                 only the catalog goes stale (issue #351)"
            );
        }
        // Consistent with GENERATED, which must keep saying the same thing.
        assert!(
            txt[end..].contains("Only the catalog does."),
            "GENERATED must still say only the catalog goes stale"
        );
        // The sheet's own width: every CUSTODY line fits the 64-column rule.
        for line in custody_raw.lines() {
            assert!(
                line.chars().count() <= 64,
                "CUSTODY line wider than the sheet's 64 columns: {line:?}"
            );
        }
    }

    /// The sheet's one prose line for a collapsed-whitespace comparison: it
    /// is wrapped at 64 columns, so a phrase can span a line break.
    fn flat(text: &str) -> String {
        text.split_whitespace().collect::<Vec<_>>().join(" ")
    }

    fn facts_for<'a>(generations: &'a [GenerationFact], places: &'a [PlaceFact]) -> KitFacts<'a> {
        KitFacts {
            escrow_public_key: ESCROW,
            sealed_volumes: 2,
            catalog_bytes: 4096,
            warehouse_deposits: 0,
            generations,
            places,
            program: None,
        }
    }

    /// Issue #448 item 1: the only RESTORE.sh example on the sheet used to
    /// restore with no `--all`. RESTORE.sh stops at the first envelope the key
    /// opens, so on a tape with several tenants an heir following the sheet
    /// could stop with one tenant's one unit. The example lists first, then
    /// restores everything, and the sheet says why.
    #[test]
    fn the_example_lists_first_then_restores_everything_with_all() {
        let txt = render_cover_text(&facts_for(&[], &[]));
        let list = txt
            .find("./RESTORE.sh --list --key escrow.key")
            .expect("the example lists the tape first");
        let all = txt
            .find("./RESTORE.sh --restore --key escrow.key --all --to ./out")
            .expect("the example restores everything with --all");
        assert!(list < all, "--list comes before the restore");
        assert!(
            !txt.contains("--restore --key escrow.key --to ./out"),
            "the bare restore example (no --all) is the defect"
        );
        let prose = flat(&txt);
        for required in [
            "One key can open only PART of a tape",
            "a tenant's key opens that tenant's envelope alone",
            "--all with the escrow key is the way to get everything",
        ] {
            assert!(prose.contains(required), "missing {required:?}");
        }
    }

    /// Issue #448 item 3: the 1.1.0 restore streams (RESTORE.sh checks for
    /// about one unit's size, not twice), and `--scratch` matters only for
    /// `--path` and the old-dar fallback.
    #[test]
    fn the_disk_space_paragraph_is_the_streaming_restore_not_the_pre_1_1_0_one() {
        let prose = flat(&render_cover_text(&facts_for(&[], &[])));
        assert!(
            !prose.contains("twice the unit"),
            "the pre-1.1.0 'twice the unit's size' is gone"
        );
        for required in [
            "about one unit's size",
            "streams each slice from the tape through age into dar",
            "only for --path (one file or folder) and for a dar older than 2.7.9",
            "--scratch DIR puts those on another disk",
            "A tape written by tapectl before 1.1.0 carries an older script",
        ] {
            assert!(prose.contains(required), "missing {required:?}: {prose}");
        }
    }

    /// Issue #448 item 4: one tool list, from RESTORE.sh's own prerequisite
    /// loop. The test READS the loop out of the generated script, so a tool
    /// added to it and not to the sheet fails here.
    #[test]
    fn the_tool_list_covers_every_tool_restore_sh_checks_for() {
        // Since 1.2.0 each RESTORE.sh mode checks its own tools (#449 item 2);
        // the set `--restore` checks is the union of them all, and the one
        // list this sheet must cover.
        let script = crate::volume::layout::generate_restore_script_v2("L6-0001", 3);
        let line = script
            .lines()
            .find(|l| l.starts_with("TOOLS_RESTORE=("))
            .expect("RESTORE.sh has a --restore prerequisite set");
        let tools: Vec<&str> = line
            .trim_start_matches("TOOLS_RESTORE=(")
            .split(')')
            .next()
            .unwrap()
            .split_whitespace()
            .collect();
        assert!(
            tools.len() >= 9 && tools.contains(&"mkfifo") && tools.contains(&"tee"),
            "parsed the wrong line: {line:?}"
        );
        assert_eq!(
            tools,
            crate::volume::layout::restore_sh_tools(),
            "the --restore set is the union the table names"
        );
        let txt = render_cover_text(&facts_for(&[], &[]));
        for tool in &tools {
            assert!(
                txt.contains(tool),
                "the cover sheet's tool list lacks {tool:?}, which RESTORE.sh requires"
            );
        }
        let prose = flat(&txt);
        for required in [
            "mt package mt-st",
            "stable age v1 format",
            "`age` program and `rage` both read.",
            "only if your dar was built with GPG support",
        ] {
            assert!(prose.contains(required), "missing {required:?}");
        }
        assert!(
            !prose.contains("(tar, age, dar)"),
            "the three-tool list was the drift"
        );
    }

    /// Issue #448 item 2: "a matching generation" was wrong in the direction
    /// that matters. An LTO-6 cartridge reads in an LTO-6 or LTO-7 drive and
    /// in nothing newer; the sheet renders `Generation::can_read` for each
    /// generation among the sealed volumes, and names the interface.
    #[test]
    fn an_lto6_volume_names_lto6_and_lto7_drives_and_the_interface() {
        let conn = conn_with_escrow();
        conn.execute(
            "INSERT INTO volumes (label, backend_type, backend_name, media_type,
                                  capacity_bytes, status)
             VALUES ('L6-0001', 'lto', 'lto0', 'LTO-6', 2500000000000, 'sealed'),
                    ('L6-0002', 'lto', 'lto0', 'LTO6', 2500000000000, 'sealed'),
                    ('L6-GONE', 'lto', 'lto0', 'LTO-8', 12000000000000, 'retired')",
            [],
        )
        .unwrap();
        let tmp = TempDir::new().unwrap();
        generate_with_program(
            &conn,
            Path::new("/nonexistent.db"),
            tmp.path(),
            fake_program().path(),
        )
        .unwrap();
        let prose = flat(&fs::read_to_string(tmp.path().join("COVER.txt")).unwrap());

        assert!(
            prose.contains("LTO-6 (2 volume(s)): an LTO-6 or LTO-7 drive"),
            "an LTO-6 cartridge reads in an LTO-6 or LTO-7 drive; two spellings are one \
             group: {prose}"
        );
        assert!(
            !prose.contains("LTO-8 ("),
            "a retired volume's generation is not the kit's business: {prose}"
        );
        assert!(!prose.contains("matching generation"));
        for required in [
            "SAS",
            "host adapter (HBA)",
            "A NEWER drive often cannot read an OLDER cartridge",
        ] {
            assert!(prose.contains(required), "missing {required:?}");
        }
    }

    /// The read-compatibility the sheet prints is `Generation::can_read`'s,
    /// listed in release order, and an LTO-8 drive is named only for the
    /// media it reads.
    #[test]
    fn drives_that_read_follow_the_compatibility_table() {
        use Generation::*;
        assert_eq!(drives_that_read(Lto6), [Lto6, Lto7]);
        assert_eq!(drives_that_read(Lto5), [Lto5, Lto6, Lto7]);
        assert_eq!(drives_that_read(Lto7), [Lto7, Lto8]);
        assert_eq!(drives_that_read(Lto7M8), [Lto8]);
        assert_eq!(drives_that_read(Lto8), [Lto8, Lto9]);
        assert_eq!(drives_that_read(Lto10), [Lto10]);
        assert_eq!(drive_phrase(&[Lto6, Lto7]), "an LTO-6 or LTO-7 drive");
        assert_eq!(
            drive_phrase(&[Lto5, Lto6, Lto7]),
            "an LTO-5, LTO-6 or LTO-7 drive"
        );
        assert_eq!(drive_phrase(&[Lto8]), "an LTO-8 drive");
    }

    /// `DRIVE_GENERATIONS` lists every drive generation: a new `Generation`
    /// variant makes this `match` fail to compile, which is the prompt to
    /// decide whether it is a drive and extend the list.
    #[test]
    fn a_new_generation_must_be_considered_here() {
        for g in [
            Generation::Lto1,
            Generation::Lto2,
            Generation::Lto3,
            Generation::Lto4,
            Generation::Lto5,
            Generation::Lto6,
            Generation::Lto7,
            Generation::Lto7M8,
            Generation::Lto8,
            Generation::Lto9,
            Generation::Lto10,
        ] {
            let is_a_drive = match g {
                Generation::Lto1
                | Generation::Lto2
                | Generation::Lto3
                | Generation::Lto4
                | Generation::Lto5
                | Generation::Lto6
                | Generation::Lto7
                | Generation::Lto8
                | Generation::Lto9
                | Generation::Lto10 => true,
                Generation::Lto7M8 => false,
            };
            assert_eq!(DRIVE_GENERATIONS.contains(&g), is_a_drive, "{g}");
        }
        let mut orders: Vec<usize> = DRIVE_GENERATIONS
            .iter()
            .copied()
            .chain([Generation::Lto7M8])
            .map(release_order)
            .collect();
        orders.sort_unstable();
        orders.dedup();
        assert_eq!(orders.len(), 11, "release_order is one-to-one");
    }

    /// With no sealed cartridge (or one whose generation this sheet cannot
    /// read) the sheet says to look the generation up, never nothing.
    #[test]
    fn the_generation_line_falls_back_to_the_makers_table() {
        let prose = flat(&render_cover_text(&facts_for(&[], &[])));
        assert!(prose.contains("look up which drives read it in the maker's compatibility table"));
        let odd = [GenerationFact {
            generation: None,
            stored: "DLT-IV".into(),
            volumes: 1,
        }];
        let prose = flat(&render_cover_text(&facts_for(&odd, &[])));
        assert!(prose.contains("DLT-IV (1 volume(s)): not a generation this sheet knows"));
    }

    /// Issue #448 item 5: "Tapes are at:" is a hand-fill box beside the
    /// secret box, with the places the catalog knows printed above it as a
    /// starting point; the box is as ragged-proof as the secret's.
    #[test]
    fn the_sheet_has_a_tapes_are_at_box_and_lists_the_places_the_catalog_knows() {
        let places = [
            PlaceFact {
                place: Some("home-rack".into()),
                volumes: vec![
                    ("L6-0001".into(), Some("BC0001L6".into())),
                    ("L6-0002".into(), None),
                ],
            },
            PlaceFact {
                place: None,
                volumes: vec![("L6-0003".into(), Some("BC0003L6".into()))],
            },
        ];
        let txt = render_cover_text(&facts_for(&[], &places));
        assert!(txt.contains("WHERE THE TAPES ARE -- FILL THIS IN"));
        assert!(
            txt.contains("home-rack: L6-0001 (BC0001L6), L6-0002"),
            "{txt}"
        );
        assert!(
            txt.contains("(no place recorded): L6-0003 (BC0003L6)"),
            "{txt}"
        );
        let start = txt.find("WHERE THE TAPES ARE").unwrap();
        let end = start + txt[start..].find("Written by:").unwrap();
        let section = &txt[start..end];
        assert!(section.contains("Tapes are at:"));
        let box_lines: Vec<&str> = section
            .lines()
            .filter(|l| l.trim_start().starts_with('|') || l.trim_start().starts_with('+'))
            .collect();
        assert!(box_lines.len() >= 5, "{box_lines:?}");
        let widths: std::collections::BTreeSet<usize> =
            box_lines.iter().map(|l| l.chars().count()).collect();
        assert_eq!(widths.len(), 1, "ragged box: {box_lines:#?}");
        assert!(
            box_lines.iter().any(|l| l.contains("____")),
            "the box has lines to write on"
        );
        // The secret's box is untouched by the second one.
        let secret_at = txt.find("WRITE IT HERE").unwrap();
        let tapes_heading = txt.find("WHERE THE TAPES ARE -- FILL THIS IN").unwrap();
        assert!(
            secret_at < tapes_heading,
            "the secret box comes first, the tapes box beside it"
        );
        assert_eq!(txt.matches("WRITE IT HERE").count(), 1);
    }

    /// The places come from the catalog: sealed volumes, grouped by their
    /// place, the cartridge's barcode beside the label, and a volume no
    /// longer sealed is not listed.
    #[test]
    fn generate_prints_the_places_from_the_catalog() {
        let conn = conn_with_escrow();
        conn.execute_batch(
            "INSERT INTO locations (name) VALUES ('home-rack'), ('offsite');
             INSERT INTO cartridges (barcode, media_type, nominal_capacity, status)
                 VALUES ('BC0001L6', 'LTO-6', 2500000000000, 'in_use'),
                        ('BC0002L6', 'LTO-6', 2500000000000, 'in_use');
             INSERT INTO volumes (label, backend_type, backend_name, media_type,
                                  capacity_bytes, status, location_id)
                 VALUES ('L6-0001', 'lto', 'lto0', 'LTO-6', 1, 'sealed',
                         (SELECT id FROM locations WHERE name = 'home-rack')),
                        ('L6-0002', 'lto', 'lto0', 'LTO-6', 1, 'sealed',
                         (SELECT id FROM locations WHERE name = 'offsite')),
                        ('L6-0003', 'lto', 'lto0', 'LTO-6', 1, 'retired',
                         (SELECT id FROM locations WHERE name = 'offsite'));
             INSERT INTO cartridge_volumes (cartridge_id, volume_id)
                 VALUES ((SELECT id FROM cartridges WHERE barcode = 'BC0001L6'),
                         (SELECT id FROM volumes WHERE label = 'L6-0001')),
                        ((SELECT id FROM cartridges WHERE barcode = 'BC0002L6'),
                         (SELECT id FROM volumes WHERE label = 'L6-0002'));",
        )
        .unwrap();
        let tmp = TempDir::new().unwrap();
        generate_with_program(
            &conn,
            Path::new("/nonexistent.db"),
            tmp.path(),
            fake_program().path(),
        )
        .unwrap();
        let txt = fs::read_to_string(tmp.path().join("COVER.txt")).unwrap();
        assert!(txt.contains("home-rack: L6-0001 (BC0001L6)"), "{txt}");
        assert!(txt.contains("offsite: L6-0002 (BC0002L6)"), "{txt}");
        assert!(!txt.contains("L6-0003"), "a retired volume is not listed");
        let html = fs::read_to_string(tmp.path().join("escrow-kit.html")).unwrap();
        assert!(html.contains("home-rack: L6-0001 (BC0001L6)"));
    }

    /// Long lists wrap inside the sheet's 64 columns without splitting a
    /// volume, and so does every line of the new generation and places
    /// sections.
    #[test]
    fn a_long_place_list_wraps_within_the_sheets_columns() {
        let volumes = (1..=14)
            .map(|n| (format!("L6-{n:04}"), Some(format!("BC{n:04}L6"))))
            .collect();
        let places = [PlaceFact {
            place: Some("offsite".into()),
            volumes,
        }];
        let gens = [GenerationFact {
            generation: Some(Generation::Lto5),
            stored: "LTO-5".into(),
            volumes: 14,
        }];
        let txt = render_cover_text(&facts_for(&gens, &places));
        let section_start = txt.find("WHERE THE TAPES ARE -- FILL").unwrap();
        let section_end = section_start + txt[section_start..].find("Written by:").unwrap();
        let gens_start = txt.find("2. You need an LTO tape drive").unwrap();
        let gens_end = gens_start + txt[gens_start..].find("3. Every cartridge").unwrap();
        for part in [&txt[section_start..section_end], &txt[gens_start..gens_end]] {
            for line in part.lines() {
                assert!(
                    line.chars().count() <= 64,
                    "wider than the sheet's 64 columns: {line:?}"
                );
            }
        }
        let flat_places = flat(&txt);
        for n in 1..=14 {
            assert!(
                flat_places.contains(&format!("L6-{n:04} (BC{n:04}L6)")),
                "volume {n} was lost or split by the wrap"
            );
        }
    }

    /// The page has to render with no network and no filesystem beyond
    /// itself — that is the entire point of printing it.
    #[test]
    fn the_html_page_is_self_contained() {
        let f = KitFacts {
            escrow_public_key: ESCROW,
            sealed_volumes: 0,
            catalog_bytes: 1,
            warehouse_deposits: 0,
            generations: &[],
            places: &[],
            program: None,
        };
        let html = render_cover_html(&f).unwrap();
        assert!(html.contains("<svg"), "the QR must be inlined as SVG");
        assert!(
            html.contains(ESCROW),
            "the escrow public key must be printed as text too"
        );
        // Issue #341: the QR and the boxed age1… are the identity, and the
        // page must say so, and must carry the same hand-fill box as the text.
        assert!(
            html.contains("ESCROW IDENTITY (public half)"),
            "the QR/key block must be captioned as the identity, not the key"
        );
        assert!(
            html.contains("WRITE IT HERE"),
            "the HTML page must carry the hand-fill box for the secret"
        );
        assert!(
            !html.contains("THE RECOVERY KEY"),
            "the HTML page must not call the public key the recovery key"
        );

        // What matters is that nothing is FETCHED. The SVG legitimately
        // carries `xmlns="http://www.w3.org/2000/svg"`, which is an XML
        // namespace identifier and never retrieved — so a blanket ban on
        // "http://" fails on a page that is in fact perfectly offline.
        // Strip the namespace declarations, then forbid the constructs that
        // actually load something.
        let without_ns = html.replace("http://www.w3.org/2000/svg", "");
        for forbidden in ["http://", "https://", "<script", "<link", "src=", "@import"] {
            assert!(
                !without_ns.contains(forbidden),
                "the page must not reference {forbidden:?} — it has to render offline"
            );
        }
    }
}
