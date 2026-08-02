//! The Heir Kit generator (ADR-0005, contents ratified in ADR-0009, issue #69).
//!
//! This module produces the *artifacts*. It deliberately stops there: the
//! ceremony that gives them their value — printing, sealing into
//! tamper-evident envelopes, distributing across at least two independent
//! failure domains — is physical and belongs to the operator. Issue #69 sat
//! deferred on the belief that the whole of it needed a person; that covered
//! only the second half.
//!
//! Three artifacts land in the output directory:
//!
//! | file | what it is |
//! |---|---|
//! | `COVER.txt` | The plain-text cover sheet. The artifact with the decades-scale claim: no browser, no tooling, readable with `cat`. Carries the escrow public key in retypable Bech32. |
//! | `escrow-kit.html` | The same content, self-contained, with the key as an inline SVG QR and print styling. Exists so the ceremony is one keystroke, because ADR-0005 requires repeating it after every write session and a ceremony with friction is one that gets skipped. |
//! | `catalog.db.age` | The **full** `tapectl.db`, age-encrypted to the escrow recipient. |
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

/// What a generation produced, for the CLI to render.
#[derive(Debug)]
pub struct KitReport {
    pub out_dir: PathBuf,
    pub cover_txt: PathBuf,
    pub html: PathBuf,
    pub catalog_age: PathBuf,
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

    let sealed_volumes: i64 = conn.query_row(
        "SELECT COUNT(*) FROM volumes WHERE status = 'sealed'",
        [],
        |r| r.get(0),
    )?;

    let warehouse_deposits: i64 =
        conn.query_row("SELECT COUNT(*) FROM volume_deposits", [], |r| r.get(0))?;

    let facts = KitFacts {
        escrow_public_key: &escrow,
        sealed_volumes,
        catalog_bytes,
        warehouse_deposits,
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
        catalog_bytes,
        escrow_public_key: escrow,
        sealed_volumes,
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
    // archived files (`files`/`manifest_entries` are one row each), and the
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
documents, photos, and similar. This sheet is what makes them
readable. Without the key printed below, the tapes cannot be
decrypted by anyone -- including the people who made them.

Keep this sheet. It is not a receipt; it is the key.

THE RECOVERY KEY
----------------
Type or scan this exactly. It is case-insensitive and has a
built-in checksum, so a mistyped character will be rejected
rather than silently producing the wrong key.

    {key}

(The same key is encoded as a QR code on the HTML version of
this sheet, if that was printed too.)

WHAT ELSE IS IN THIS ENVELOPE
-----------------------------
  catalog.db.age   An encrypted index of what is on the tapes:
                   which files, on which cartridge, in which
                   location. {bytes} bytes. Decrypt it with the
                   key above.

WHAT TO DO FIRST
----------------
1. Do NOT bulk-erase, degauss, or reformat the cartridges.
   They are readable for decades if left alone.

2. You need an LTO tape drive of a matching generation. If you
   do not have one, a data-recovery firm does. The tapes are
   standard hardware; nothing about them is proprietary.

3. Every cartridge is SELF-DESCRIBING. Near the start of each
   tape there is a plain-text guide and a script named
   RESTORE.sh that can recover the contents using only common
   tools (tar, age, dar). You do not need the tapectl program,
   and you do not need this catalog file, to get the data back.
   Both exist to make it easier, not to make it possible.

4. If you do have tapectl, the first commands are:

       tapectl restore raw-volume --device /dev/nst0 --to ./out
       tapectl volume identify --device /dev/nst0

   The first works with no database at all.

{warehouse}CUSTODY -- FOR WHOEVER MAINTAINS THIS
-------------------------------------
  * Store in a tamper-evident envelope.
  * Keep at least TWO copies in independent failure domains --
    not two shelves in one building.
  * Paper: a UL-350 rated safe. If stored together with tape,
    Class-125 (tape is far less heat-tolerant than paper).
  * REFRESH after each writing session. A kit made before newer
    tapes were written still opens the older ones and silently
    misses the new. Running `tapectl audit` will warn when this
    sheet has fallen behind the tapes.

GENERATED
---------
  Sealed cartridges known at generation time: {sealed}

  This key is permanent. It is never rotated, and rotating the
  day-to-day keys does not change it -- so this sheet does not
  go stale as keys change. Only the catalog does.

================================================================
",
        key = f.escrow_public_key,
        bytes = f.catalog_bytes,
        sealed = f.sealed_volumes,
        warehouse = warehouse_section(f.warehouse_deposits),
    )
}

/// The same cover sheet as a self-contained printable page.
///
/// No external resources of any kind — the QR is an inline SVG, the styling
/// is one inline `<style>`. A page that fetched anything would be a page that
/// renders blank in the situation it exists for.
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
  .key {{ font-family: ui-monospace, Menlo, Consolas, monospace;
          font-size: 1.05rem; word-break: break-all; text-align: center;
          border: 2px solid #000; padding: .75rem; margin: 1rem 0; }}
  pre {{ font-family: ui-monospace, Menlo, Consolas, monospace;
         font-size: .82rem; white-space: pre-wrap; }}
  @media print {{ body {{ margin: 0; max-width: none; }} }}
</style>
</head>
<body>
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

    #[test]
    fn refuses_when_no_escrow_recipient_is_registered() {
        let conn = crate::db::open_memory().unwrap();
        let tmp = TempDir::new().unwrap();
        let err = generate(&conn, Path::new("/nonexistent.db"), tmp.path())
            .expect_err("a kit with nobody to address it to must not be produced");
        let msg = err.to_string();
        assert!(
            msg.contains("key generate --escrow"),
            "the refusal must tell the operator how to fix it; got: {msg}"
        );
    }

    #[test]
    fn produces_all_three_artifacts_and_logs_one_event() {
        let conn = conn_with_escrow();
        let tmp = TempDir::new().unwrap();
        let out = tmp.path().join("kit");

        let report = generate(&conn, Path::new("/nonexistent.db"), &out).expect("kit generation");

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
        generate(&conn, Path::new("/nonexistent.db"), &out).unwrap();

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
        generate(&conn, Path::new("/nonexistent.db"), &out).unwrap();

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

    #[test]
    fn the_cover_sheet_carries_the_key_and_the_custody_rules() {
        let f = KitFacts {
            escrow_public_key: ESCROW,
            sealed_volumes: 3,
            catalog_bytes: 4096,
            warehouse_deposits: 0,
        };
        let txt = render_cover_text(&f);
        assert!(txt.contains(ESCROW), "the key itself must be on the sheet");
        for required in [
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
        };
        let txt = render_cover_text(&f);
        assert!(
            !txt.contains("CLOUD"),
            "a tape-only archive must not get a cloud section"
        );
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
        };
        let html = render_cover_html(&f).unwrap();
        assert!(html.contains("<svg"), "the QR must be inlined as SVG");
        assert!(html.contains(ESCROW), "the key must be printed as text too");

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
