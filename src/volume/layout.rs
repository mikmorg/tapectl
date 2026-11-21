/// Parameters for the v2 ID thunk (File 0). Layout v2 collapses v1's 18
/// positional arguments to this struct and drops every per-file position
/// field (sheet §2.3): File 0 now carries identity plus two pointers only —
/// `front_index` (always File 3) and `seal_marker` (always the last file).
/// Every other position/type/size fact lives solely in the front index
/// itself, as `[[files]]` entries (see `generate_front_index`).
#[derive(Debug, Clone, Copy)]
pub struct IdThunkV2Params<'a> {
    pub label: &'a str,
    pub uuid: &'a str,
    pub media_type: &'a str,
    /// The writer's PACKAGE version (`CARGO_PKG_VERSION`), never the build
    /// identity `build_info::VERSION` — File 0 is an on-tape byte and
    /// ADR-0012 (2026-09-24 amendment, item 1) adds no writer-commit field
    /// to it. `build_info::tests::on_tape_writer_string_is_the_package_version`
    /// pins the `volume::write` call site.
    pub tapectl_version: &'a str,
    pub nominal_capacity: i64,
    pub mam_capacity: i64,
    pub total_files: i32,
    pub mam_manufacturer: &'a str,
    pub mam_serial: &'a str,
    pub mam_length: i64,
    pub mam_loads: i64,
    /// RFC 3339 generation timestamp, rendered by the caller (`build()`) and
    /// injected rather than read from the clock in here (v2-implementation-plan
    /// T6 review finding #5). Without this, only `system_guide`/`restore_sh`
    /// were testable for build-twice byte-identity — the ID thunk's `created_at`
    /// varied on every call and could never be held constant across two builds
    /// in a test, leaving `layout-session.md`'s "same inputs + same generation
    /// timestamp ⇒ reproducible Layout" clause unverified for this zone. This
    /// also matters for resume, which depends on frozen (not regenerated)
    /// bytes — see `ContentSource::Materialized`'s doc comment.
    pub created_at: &'a str,
    /// How `cartridge_serial` was established — `"mam"` (the cartridge's own
    /// chip reported it) or `"operator"` (no serial was readable when this
    /// volume was bound, so the operator named the cartridge with
    /// `volume init --cartridge <barcode>` and `cartridge_serial` carries
    /// THAT barcode). `docs/design/volume-format-v3.md` §1.1, ADR-0012.
    ///
    /// `None` OMITS the line entirely, and that is load-bearing: absent means
    /// UNKNOWN and must never read as `"mam"`. Every tape written before this
    /// field existed omits it, and those tapes stay readable forever — a
    /// reader that defaulted the absent case to `"mam"` would make all of
    /// them falsely attest a chip-verified serial.
    ///
    /// Injected, like `created_at`, rather than derived from a live MAM read
    /// in here: the value records how the identity was established AT THE
    /// BINDING (read back from `cartridge_volumes.identity_source`), not what
    /// the drive happens to report at this contact. Deriving it from the
    /// contact would make the same physical tape attest different provenance
    /// depending on which drive wrote it.
    pub cartridge_identity_source: Option<&'a str>,
}

/// Generate the v2 ID thunk (File 0) content. Per sheet §2.3 and
/// `docs/design/volume-format-v3.md` §1.1: `[volume]` carries identity
/// (unchanged in spirit from v1, plus a new `uuid` field) and `[layout]`
/// carries ONLY `front_index = 3`, `seal_marker` (the last file), and
/// `total_files` — the v1 `data_start`/`data_end`/`first_envelope`/
/// `num_envelopes`/`mini_index`/operator-position fields are gone: they are
/// superseded by File 3's `[[files]]` entries, which carry position, type,
/// size, and ciphertext hash for every file. The human-readable header keeps
/// v1's structure, but its pointer text now says the map is File 3, not "the
/// next file."
///
/// `volume_init` writes this as a **provisional** identity stamp (positions
/// unknown at init time); the write session rewrites File 0 from BOT with
/// the real `total_files`/`seal_marker` once the layout is built (sheet
/// §2.3 — resume must not try to preserve init's File 0).
///
/// It states `requires = []` ([`REQUIRES`]), as every Layout 3 tape does
/// (D1). [`generate_id_thunk_v2_requiring`] states a list, for a later
/// feature's fence.
pub fn generate_id_thunk_v2(params: &IdThunkV2Params) -> String {
    generate_id_thunk_v2_requiring(params, REQUIRES)
}

/// `requires` on every Layout 3 tape, in File 0 and in the seal: empty (D1;
/// ADR-0012 items 258(a), 288; `volume-format-v3.md` §1.2). The short seal
/// and a unit that continues on another cartridge are part of Layout 3. The
/// key stays, so that a later feature can fence an older Layout 3 reader.
pub const REQUIRES: &[&str] = &[];

/// The `requires` value as File 0 and the seal spell it (`volume-format-v3.md`
/// §1.2, normative for a later feature): each feature double-quoted, two
/// separated by a comma and one space, no space inside the brackets.
/// RESTORE.sh's `check_requires` splits the list at spaces, so `["a","b"]`
/// would read as one unknown feature. An empty list is `[]`.
fn requires_value(features: &[&str]) -> String {
    let quoted: Vec<String> = features.iter().map(|f| format!("\"{f}\"")).collect();
    format!("[{}]", quoted.join(", "))
}

/// File 0 stating `features` in `requires`. Every Layout 3 tape states
/// [`REQUIRES`]; every other byte is [`generate_id_thunk_v2`]'s.
pub fn generate_id_thunk_v2_requiring(params: &IdThunkV2Params, features: &[&str]) -> String {
    let requires = requires_value(features);
    let IdThunkV2Params {
        label,
        uuid,
        media_type,
        tapectl_version,
        nominal_capacity,
        mam_capacity,
        total_files,
        mam_manufacturer,
        mam_serial,
        mam_length,
        mam_loads,
        created_at: now,
        cartridge_identity_source,
    } = *params;
    let seal_marker = total_files - 1;
    let q = crate::volume::format::toml_string;
    // Absent means unknown (§1.1): an unknown provenance emits NO line at
    // all, so a pre-#192 File 0 and a post-#192 one with nothing to say are
    // byte-identical. Rendered as a whole line here (rather than as a value
    // spliced into the template) precisely so `None` can contribute zero
    // bytes — a template with an empty value would still leave the key.
    let identity_source_line = match cartridge_identity_source {
        Some(src) => format!("cartridge_identity_source = {}\n", q(src)),
        None => String::new(),
    };
    // Every string in the TOML half is a TOML basic string (#547,
    // `volume-format-v3.md` §1.1): a hand-typed barcode may hold a quote or
    // a backslash. The human header above it is not TOML and says them as
    // they are.
    let (t_label, t_uuid, t_version, t_media, t_now, t_manufacturer, t_serial) = (
        q(label),
        q(uuid),
        q(tapectl_version),
        q(media_type),
        q(now),
        q(mam_manufacturer),
        q(mam_serial),
    );
    format!(
        r#"================================================================
                     TAPECTL ARCHIVAL VOLUME
================================================================

Label:   {label}
Media:   {media_type}
Created: {now}

This tape contains encrypted archival data managed by tapectl,
an open-source archival storage tool.

>>> COMPLETE INSTRUCTIONS ARE IN THE NEXT FILE ON THIS TAPE. <<<
>>> THE FULL MAP OF THIS TAPE IS FILE 3 (the front index).   <<<

Your drive may not be /dev/nst0. That is an example, not a fact
about this tape. List the tape drives on the machine you are
using and substitute the right one everywhere below:

    ls -l /dev/tape/by-id/

A wrong device reads a DIFFERENT tape, or fails outright.

To read the next file (the full recovery guide):

    mt -f /dev/nst0 setblk 524288
    mt -f /dev/nst0 fsf 1
    dd if=/dev/nst0 bs=512k | tr -d '\0' > GUIDE.md
    less GUIDE.md

If you just read this file and the tape is already positioned
past it, read the next file directly:

    mt -f /dev/nst0 setblk 524288
    dd if=/dev/nst0 bs=512k | tr -d '\0' > GUIDE.md

The guide explains everything: what tools you need, how to find
your encryption key, and how to recover your data step by step.
It is written so that an AI assistant can follow it to help you.

================================================================
              MACHINE-READABLE METADATA (TOML)
================================================================

[volume]
magic = "tapectl-volume-v3"
label = {t_label}
uuid = {t_uuid}
layout_version = 3
requires = {requires}
tapectl_version = {t_version}
media_type = {t_media}
nominal_capacity_bytes = {nominal_capacity}
mam_capacity_bytes = {mam_capacity}
created_at = {t_now}

[layout]
front_index = 3
seal_marker = {seal_marker}
total_files = {total_files}

[media]
cartridge_manufacturer = {t_manufacturer}
cartridge_serial = {t_serial}
{identity_source_line}tape_length_meters = {mam_length}
load_count_at_write = {mam_loads}
"#
    )
}

/// The modes of RESTORE.sh that run external tools, as bits of
/// [`RestoreTool::modes`]. `--info` and `--verify` run the same ones, and so
/// do `--where` (a unit, or `--all` for every unit) and `--find`: they open
/// envelopes (the `--find-envelope` set) and read plain text out of them with
/// `awk`, which is not checked for.
const MODE_READ: u8 = 1; // --info, --verify
const MODE_ENVELOPE: u8 = 2; // --find-envelope
const MODE_LIST: u8 = 4; // --list
const MODE_PATH: u8 = 8; // --restore --path
const MODE_RESTORE: u8 = 16; // --restore
const MODE_INDEX: u8 = 32; // --where UNIT, --where --all, --find

/// A tool RESTORE.sh runs (beyond `mt`, which every mode needs, and the
/// everyday text tools it does not check for), the modes that run it, and
/// what File 1 says it is for. THE one list: RESTORE.sh's per-mode checks,
/// its `# Requirements:` and `--help` lines, File 1's tool list and (through
/// the generated script) the cover sheet's are all drawn from it, so they
/// cannot drift apart again (#448, #449). A tool added to a mode's code path
/// in the script is added here, and `restore_sh_tool_sets_are_drawn_from_one_table`
/// reads the script's own sets back to hold it to that.
struct RestoreTool {
    name: &'static str,
    modes: u8,
    /// File 1's bullet, after the tool's name in backticks.
    role: &'static str,
}

const ALL_MODES: u8 = MODE_READ | MODE_ENVELOPE | MODE_LIST | MODE_PATH | MODE_RESTORE | MODE_INDEX;
const DECRYPTING: u8 = MODE_ENVELOPE | MODE_LIST | MODE_PATH | MODE_RESTORE | MODE_INDEX;

/// In the order RESTORE.sh's `Requirements:` lines list them. The
/// decompressors (`gzip`, `xz`, `zstd`) are not here: the script checks the
/// one a unit names, and only for such a unit (File 1's "Tools Required").
const RESTORE_TOOLS: &[RestoreTool] = &[
    RestoreTool {
        name: "dd",
        modes: ALL_MODES,
        role: "reading raw data from tape (coreutils)",
    },
    RestoreTool {
        name: "age",
        modes: DECRYPTING,
        role: "decryption (age-encryption.org; package age on Debian 11 and later and on Ubuntu 22.04 and later, none on Debian 10, where a release from github.com/FiloSottile/age/releases does). The encrypted files are in the age v1 format, which is stable and which the `age` program and `rage` both read; any age from 1.0.0-rc.1 (March 2021) on opens them, the oldest tried. RESTORE.sh runs a program named `age`, so with only `rage` link it under that name",
    },
    RestoreTool {
        name: "sha256sum",
        modes: ALL_MODES,
        role: "integrity verification (coreutils)",
    },
    RestoreTool {
        name: "head",
        modes: ALL_MODES,
        role: "trimming and hashing exact byte counts (coreutils)",
    },
    RestoreTool {
        name: "tar",
        modes: DECRYPTING,
        role: "unpacking your decrypted envelope, and restoring a unit: the stream a unit is stored as is a tar archive (package tar)",
    },
    RestoreTool {
        name: "tail",
        modes: MODE_PATH,
        role: "cutting one file's bytes out of a slice, for a restore of named paths and for the repair (coreutils)",
    },
];

/// The tools of one mode (or of all of them, with [`ALL_MODES`]), in table order.
fn tools_for(modes: u8) -> Vec<&'static str> {
    RESTORE_TOOLS
        .iter()
        .filter(|t| t.modes & modes != 0)
        .map(|t| t.name)
        .collect()
}

/// Every tool RESTORE.sh may check for beyond `mt`, in the order its
/// `--restore` check lists them: the union of all its modes, and the one list
/// File 1 and the cover sheet render.
pub fn restore_sh_tools() -> Vec<&'static str> {
    tools_for(ALL_MODES)
}

/// Greedy word wrap to `width` columns, every line after the first indented
/// by `hang`.
fn wrap_words(text: &str, width: usize, hang: &str) -> String {
    let mut out = String::new();
    let mut line = String::new();
    for word in text.split_whitespace() {
        if !line.is_empty() && line.chars().count() + 1 + word.chars().count() > width {
            out.push_str(&line);
            out.push('\n');
            line = hang.to_string();
        } else if !line.is_empty() {
            line.push(' ');
        }
        line.push_str(word);
    }
    out.push_str(&line);
    out
}

/// File 1's opening list of tools, drawn from [`RESTORE_TOOLS`] (`mt` first).
fn guide_tool_sentence() -> String {
    let names = restore_sh_tools();
    let (last, init) = names.split_last().expect("RESTORE_TOOLS is not empty");
    wrap_words(
        &format!(
            "tapectl or its database. All you need is: mt, {}, and {last}.",
            init.join(", ")
        ),
        72,
        "",
    )
}

/// "`mt`, `dd`, `sha256sum` and `head`": what `--info` and `--verify` need,
/// from the same table as the script's own check.
fn guide_read_only_tools() -> String {
    let names: Vec<String> = std::iter::once("mt")
        .chain(tools_for(MODE_READ))
        .map(|t| format!("`{t}`"))
        .collect();
    let (last, init) = names.split_last().expect("not empty");
    format!("{} and {last}", init.join(", "))
}

/// File 1's "Tools Required" bullets: `mt`, then every tool of
/// [`RESTORE_TOOLS`] with its role. The file's own bullets follow them.
fn guide_tools_required() -> String {
    let mut bullets = vec![wrap_words(
        "- `mt` (mt-st package) — tape positioning and setting the block size. \
         The plain `mt` that GNU cpio installs cannot set it: RESTORE.sh warns, \
         then reads in whatever mode the drive is in",
        72,
        "  ",
    )];
    for tool in RESTORE_TOOLS {
        bullets.push(wrap_words(
            &format!("- `{}` — {}", tool.name, tool.role),
            72,
            "  ",
        ));
    }
    bullets.join("\n")
}

/// File 1's "The tape drive" section: which drives read THIS tape's media
/// generation (from [`Generation::can_read`], by the same list the cover
/// sheet uses) and the interface. A `media_type` that is not a generation
/// this crate knows gets the general statement.
fn guide_drive_section(media_type: &str) -> String {
    use crate::crypto::escrow_kit::{drive_phrase, drives_that_read};
    let readers = crate::media::Generation::parse(media_type)
        .map(|g| (g, drives_that_read(g)))
        .filter(|(_, drives)| !drives.is_empty());
    let readers_phrase = readers.as_ref().map(|(_, drives)| drive_phrase(drives));
    // The cover sheet's own line for this generation (`generations_text` in
    // the Heir Kit), so the example is the line an heir of THIS tape finds.
    let sheet_line = match &readers {
        Some((g, drives)) => format!("\"{} (2 volume(s)): {}\"", g.as_str(), drive_phrase(drives)),
        None => {
            "\"LTO-n (2 volume(s)): an LTO-n drive\", with the generation in place of n".to_string()
        }
    };
    let which = match readers {
        Some((g, drives)) => format!(
            "This tape is {} (File 0 says so: media_type). A drive reads it only if it \
             is {}: a newer drive often cannot read an older tape (an LTO-8 drive, for \
             example, reads only LTO-7, LTO-7 Type M and LTO-8). That is about THIS \
             tape: another tape of the box may be of another generation (its own \
             File 0 says), and the table below shows which drives read which.",
            g.as_str(),
            drive_phrase(&drives)
        ),
        None => "This tape's media generation is in File 0 (media_type). It decides which \
                 drives can read it: a newer drive often cannot read an older tape (an \
                 LTO-8 drive, for example, reads only LTO-7, LTO-7 Type M and LTO-8), so \
                 look the generation up in the maker's compatibility table. Another \
                 tape of the box may be of another generation, and the table below \
                 shows which drives read which."
            .to_string(),
    };
    // This guide is on the tape, so a reader without a drive cannot have read
    // it: the generation comes off the cartridge or the Heir Kit's sheet.
    let without_drive = format!(
        "You can learn a cartridge's generation without a drive, and before this guide \
         can be read: it is printed on the cartridge (the LTO logo and number on the \
         case, and the last two characters of the barcode label, where L6 is LTO-6), \
         and the Heir Kit's cover sheet lists the generations of the tapes the Heir \
         Kit's catalogue knew when it was made, with the drives that read each, in \
         lines like {sheet_line}. A sheet made when the Heir Kit's catalogue listed \
         no sealed cartridge says so and sends you to the label."
    );
    let interface = "The drive connects by SAS (some models by Fibre Channel), so the \
                     computer needs a matching host adapter (HBA); there is no USB or SATA \
                     tape drive. Fit the pieces together before the day you need them:";
    let drive_for_this_tape = match readers_phrase.as_deref() {
        Some(drives) => format!("{drives} with a SAS interface"),
        None => "a drive that reads this tape's generation, with a SAS interface".to_string(),
    };
    let setup = format!(
        "A setup that works, as one example: {drive_for_this_tape}, a PCIe SAS host \
         adapter in a desktop computer with a free PCIe slot, and the cable and power \
         to match. For an external drive, which has its own power supply: an adapter \
         with an external port (an LSI or Broadcom 9200-8e, which has SFF-8088 ports, or \
         a 9300-8e, which has SFF-8644, on IT firmware) and an external SAS cable whose \
         ends fit the drive and the adapter. For a bare internal drive, in a 5.25-inch \
         bay: an adapter with an internal port (a 9211-8i, SFF-8087, or a 9300-8i, \
         SFF-8643), a cable from that port that ends in an SFF-8482 drive connector, and \
         the drive's power from the computer's power supply, through that connector or \
         from a separate lead (often a 4-pin Molex plug), as the drive's data sheet \
         says. These models are examples, not a recommendation: the connectors are the \
         standards that last, and the data sheets decide."
    );
    let hardware = [
        "SAS comes in generations (3, 6 and 12 Gb/s). A link runs at the highest \
         speed both ends support, so an adapter and a drive of different \
         generations still work together, at the slower speed.",
        "The cable must fit both ends, and the connectors differ: external drives \
         commonly use SFF-8088 or the newer SFF-8644, and an adapter cable joins one \
         to the other. An internal drive commonly has the SAS drive connector \
         (SFF-8482, the one on a SAS disk) and is joined to the adapter's internal \
         port, commonly SFF-8087 or the newer SFF-8643, by a cable that ends in \
         drive connectors; whether the drive takes its power through that connector \
         or from a plug of its own depends on the model. The maker's data sheets for \
         the drive and for the adapter name their connectors, and they, not this \
         text, decide.",
        "A Fibre Channel drive needs a Fibre Channel host adapter (a SAS adapter \
         cannot drive it), a fibre-optic cable with the connectors both ends take \
         (commonly LC), and a link speed both ends support. With a Fibre Channel \
         switch in between, the switch must let the computer see the drive (it is \
         \"zoned\" to it). Linux drives such adapters with the modules `qla2xxx` \
         (QLogic) and `lpfc` (Emulex); once the link is up `lsscsi -g` lists the \
         drive like any other.",
        "Use a plain host adapter (HBA), not a RAID controller. RAID firmware \
         often does not present a tape drive to the operating system at all, and \
         many on-board SAS ports are RAID controllers.",
        "A PCIe SAS adapter in a desktop or server is the dependable way. Do \
         not use a USB bridge: SAS-to-USB bridges for tape drives are rare and \
         not recommended, because a tape drive needs a steady stream of data \
         and an operating system that sees it as an ordinary SCSI tape device \
         (/dev/nst0 and the like). A computer with no free PCIe slot, a \
         laptop say, can use a Thunderbolt PCIe enclosure that holds a SAS \
         adapter: the operating system then sees an ordinary adapter, and that \
         can work, but it is the less certain way, so check that the drive \
         shows up (below) before you rely on it.",
        "Check that the drive shows up (`ls -l /dev/tape/by-id/`, see \"Which \
         device?\") before you load the tape.",
    ];
    let firm = "A data-recovery firm with a drive can read the tape. It needs no key \
                to copy it: every file's raw bytes can be read, and only their meaning \
                is encrypted. Whoever holds the escrow key, or the operator key, can \
                read everything on every tape, for every tenant, so think before \
                handing either to a firm. To use a firm without giving it a key, ask \
                for the raw content of each tape file (File 0 to the last, in the \
                512 KiB blocks of \"Manual Recovery Steps\") and decrypt it yourself; \
                a tenant's key gives a firm that tenant's data and no one else's.";
    let bullets: Vec<String> = hardware
        .iter()
        .map(|b| wrap_words(&format!("- {b}"), 72, "  "))
        .collect();
    format!(
        "## The tape drive\n\n{}\n\n{}\n\n{}\n\n{}\n\n{}\n\n{}\n\n{}\n",
        wrap_words(&which, 72, ""),
        wrap_words(&without_drive, 72, ""),
        guide_box_drives(),
        wrap_words(interface, 72, ""),
        bullets.join("\n"),
        wrap_words(&setup, 72, ""),
        wrap_words(firm, 72, "")
    )
}

/// The cartridge generations File 1's box table lists: what an LTO tape drive
/// of today is likely to be asked to read, in release order.
const BOX_TABLE_MEDIA: [crate::media::Generation; 7] = {
    use crate::media::Generation::*;
    [Lto5, Lto6, Lto7, Lto7M8, Lto8, Lto9, Lto10]
};

/// "LTO-7 Type M" for what `Generation::as_str` calls `LTO-7-M8`.
fn guide_generation_name(g: crate::media::Generation) -> &'static str {
    match g {
        crate::media::Generation::Lto7M8 => "LTO-7 Type M",
        other => other.as_str(),
    }
}

/// "LTO-6 or LTO-7", "LTO-5, LTO-6 or LTO-7": the drive generations in `drives`.
fn guide_drive_list(drives: &[crate::media::Generation]) -> String {
    let names: Vec<&str> = drives.iter().map(|g| g.as_str()).collect();
    match names.as_slice() {
        [] => "no current drive".to_string(),
        [one] => (*one).to_string(),
        [init @ .., last] => format!("{} or {last}", init.join(", ")),
    }
}

/// File 1's table of which drive generation reads which cartridge generation
/// (M7 of the 2026-10-19 read): a box can hold tapes of several generations,
/// and the heir must pick one drive for it. Drawn from
/// [`Generation::can_read`] through the cover sheet's own list, so the two
/// cannot disagree.
fn guide_box_drives() -> String {
    use crate::crypto::escrow_kit::drives_that_read;
    let mut rows = String::from("  Cartridge       Read by a drive of\n");
    for media in BOX_TABLE_MEDIA {
        rows.push_str(&format!(
            "  {:<14}  {}\n",
            guide_generation_name(media),
            guide_drive_list(&drives_that_read(media))
        ));
    }
    let lead = "A box can hold cartridges of several generations, and one drive may \
                not read them all. This is which drive generation reads which \
                cartridge:";
    let after = "No drive reads every generation. An LTO-7 drive reads LTO-5, LTO-6 \
                 and LTO-7 cartridges, an LTO-8 drive reads LTO-7, LTO-7 Type M and \
                 LTO-8 cartridges, and a box that holds both an LTO-6 and an LTO-8 \
                 cartridge needs two drives, one after the other. Pick the single \
                 drive that reads the most of the cartridges you hold, and read those \
                 first, starting with the newest tape it reads: if the newest tape of \
                 the box is not one of them, \"The rest of the box\" says what that \
                 costs. A generation not in the table: the maker's compatibility \
                 table.";
    format!(
        "{}\n\n{}\n{}",
        wrap_words(lead, 72, ""),
        rows,
        wrap_words(after, 72, "")
    )
}

/// File 1's ceiling on disk space (finding 12 of the 2026-10-17 fresh-reader
/// read): everything a tape holds restores to at most the cartridge's native
/// capacity, decimal as the makers state it, unless the units were stored
/// compressed. The generation is File 0's `media_type`; one this crate does
/// not know gets the table instead of a guess.
fn guide_capacity(media_type: &str) -> String {
    let tb = |bytes: u64| -> String {
        if bytes >= 1_000_000_000_000 {
            let t = bytes as f64 / 1e12;
            if t.fract() == 0.0 {
                format!("{} TB", t as u64)
            } else {
                format!("{t} TB")
            }
        } else {
            format!("{} GB", bytes / 1_000_000_000)
        }
    };
    let text = match crate::media::Generation::parse(media_type) {
        Some(g) => format!(
            "The tape's capacity bounds what is stored on it, not what a restore \
             writes: this tape is {}, which holds {} of data as the makers \
             count it (decimal, uncompressed), so everything stored on it takes at \
             most that on the tape. A unit stored uncompressed restores to about \
             that size; a unit stored compressed restores larger, and the capacity \
             is no ceiling for it (\"Compressed units\", below).",
            g.as_str(),
            tb(g.native_capacity_bytes())
        ),
        None => "The tape's capacity bounds what is stored on it, not what a restore \
                 writes: a tape holds at most its generation's native capacity \
                 (LTO-6 2.5 TB, LTO-7 6 TB, LTO-8 12 TB, LTO-9 18 TB, decimal), so \
                 everything stored on it takes at most that on the tape. A unit \
                 stored uncompressed restores to about that size; a unit stored \
                 compressed restores larger, and the capacity is no ceiling for it \
                 (\"Compressed units\", below). The generation is printed on the \
                 cartridge."
            .to_string(),
    };
    wrap_words(&text, 72, "")
}

/// File 1's practical drive section (finding 11 of the 2026-10-17 read):
/// reading the generation off the cartridge before a drive is bought or
/// borrowed, loading and unloading, the write-protect tab, a cleaning
/// cartridge, and a drive that does not show up.
fn guide_drive_basics() -> String {
    let items = [
        "Read the generation printed on the cartridge (L6 on its label is LTO-6) \
         before you buy or borrow a drive, and look at every cartridge in the box: \
         they may be different generations, and a drive that reads one may not read \
         another. This guide is on the tape, which you cannot read without a \
         drive, so take the generation from the cartridge or the Heir Kit's cover \
         sheet (\"The tape drive\").",
        "An external drive, in its own case with its own power supply, is the \
         simplest: it needs only an adapter with an external port and an \
         external SAS cable that fits both ends. A bare internal drive needs \
         more: a place to sit (a 5.25-inch bay in a desktop or server, or an \
         enclosure made for it), power from a power supply, and the internal \
         SAS cable that matches the adapter's connector. A PCIe SAS adapter \
         needs a free PCIe slot, so a laptop or a small PC without one cannot \
         use it: borrow a desktop or a server, or use a Thunderbolt PCIe \
         enclosure (\"The tape drive\").",
        "LSI (now Broadcom) host adapters come with one of two firmwares: \"IT\" \
         (initiator-target: the card hands every device to the operating \
         system untouched) and \"IR\" (integrated RAID: the card can also build \
         simple RAID sets from disks). IT is the one to want for a tape drive, \
         and many adapters sold as plain host adapters already run it, so a \
         card that says (IT) needs nothing. Change nothing unless the drive \
         does not appear. If it does not, and the card runs RAID firmware (IR, \
         or a MegaRAID controller), IT firmware can cure it where the maker \
         offers one for that card (not every RAID controller has one: then use \
         a plain adapter instead); flash it only with the maker's own tool and \
         instructions, because a flash that fails can leave a card unusable. \
         To see what a card is: `lspci -k` \
         names the kernel driver in use under each adapter. `mpt3sas` (`mpt2sas` on \
         older kernels) is the driver of LSI and Broadcom host adapters, and \
         `megaraid_sas` is the driver of their RAID controllers, which are the wrong \
         kind of card. A card on `mpt3sas` can still carry RAID (\"IR\") firmware: \
         the maker's flash tool says which (`sas3flash -list`, or `sas2flash -list` \
         on older cards, prints a Firmware Product ID tagged (IT) or (IR)).",
        "Slide the write-protect tab on the cartridge to the protected position \
         before you load it. Reading needs no writing, and RESTORE.sh never writes \
         to the tape, so nothing can be lost by mistake.",
        "Load the cartridge, then wait until the drive is ready: its lights settle \
         and `mt -f /dev/nst0 status` says ONLINE (DR_OPEN, or \"no medium\", means \
         it is not ready or empty). A cartridge takes a minute or more to thread. \
         To unload, `mt -f /dev/nst0 offline` rewinds the tape and ejects it; wait \
         for the drive to finish before you take it out.",
        "A drive that reads some cartridges and not others, or shows a cleaning \
         light, may need a cleaning cartridge: load it, and the drive cleans and \
         ejects it by itself.",
        "The drive is not in `ls -l /dev/tape/by-id/`: see whether the system sees \
         it at all (`lsscsi -g` if installed, and `dmesg | tail` for the kernel's \
         messages about it). The kernel's tape driver is the module `st` \
         (`lsmod | grep '^st'`, `sudo modprobe st` loads it). The device files \
         belong to the group `tape`: `ls -l /dev/nst0` says, and a user who is not \
         in it gets \"Permission denied\" (`sudo usermod -aG tape YOUR_USER`, then \
         log in again). Running the commands with `sudo` works too, but sudo \
         hides your `TAPE_DEVICE` and `TMPDIR` from the script, which then reads \
         `/dev/nst0` and `/tmp` whatever you set, and `--preserve-env` cannot \
         keep `TMPDIR` (the system's dynamic loader removes it from a \
         set-user-ID program such as sudo before sudo starts: ld.so(8)). Give \
         both after sudo, with `env`: `sudo env TAPE_DEVICE=/dev/nst0 \
         TMPDIR=/tmp ./RESTORE.sh --info`. (`sudo \
         --preserve-env=TAPE_DEVICE` keeps the device alone; sudo 1.8.21 or \
         later has it, and Debian 10 has 1.8.27.) What the script writes is then \
         owned by root.",
    ];
    let bullets: Vec<String> = items
        .iter()
        .map(|b| wrap_words(&format!("- {b}"), 72, "  "))
        .collect();
    format!(
        "## Loading the tape, and a drive that does not show up\n\n{}\n",
        bullets.join("\n")
    )
}

/// Generate the v2 system guide (File 1) — the heir manual for layout v2.
/// Rewrites v1's Quick Reference to the v2 zone order (front index at File 3,
/// tenant envelopes from File 4, seal marker last with an embedded copy of
/// the front index); replaces every "mini-index" reference with File 3 (the
/// v1 mid-tape mini-index no longer exists); adds
/// the accepted size-disclosure statement (§2 "Accepted disclosure") and the
/// three-rung degradation ladder with the zero-strip procedure (sheet §3.3,
/// §3.4).
pub fn generate_system_guide_v2(label: &str, total_files: i32) -> String {
    generate_system_guide_v2_for_media(label, total_files, "")
}

/// The system guide for a tape of a known media generation (`media_type`, as
/// File 0 states it: `LTO-6`, ...). `build` calls this one: the drives that
/// read THIS generation depend on it ([`Generation::can_read`]), and a
/// `media_type` this crate does not recognise (or the empty string, which
/// [`generate_system_guide_v2`] passes) gets the guide's general statement
/// instead of a guess.
pub fn generate_system_guide_v2_for_media(
    label: &str,
    total_files: i32,
    media_type: &str,
) -> String {
    let intro_tools = guide_tool_sentence();
    let tools_required = guide_tools_required();
    let drive_section = guide_drive_section(media_type);
    let drive_basics = guide_drive_basics();
    let capacity = guide_capacity(media_type);
    let read_only = guide_read_only_tools();
    let last_file = total_files - 1;
    format!(
        r#"# tapectl Archival Volume Recovery Guide

## The short way

If everything goes right, these commands do the whole job. Each says which
section explains it; if one fails, or a check does not pass, read that
section, and "Start here" below it. `/dev/nst0` is an example: "Which
device?" says how to find your drive.

Set the drive to the tape's block size and read RESTORE.sh, File 2, off the
tape ("Automated Recovery (recommended)" explains each line):

    mt -f /dev/nst0 setblk 524288
    mt -f /dev/nst0 rewind && mt -f /dev/nst0 fsf 2
    dd if=/dev/nst0 bs=512k | tr -d '\0' > RESTORE.sh
    chmod +x RESTORE.sh

See what is on the tape, with no key ("Automated Recovery (recommended)"):

    ./RESTORE.sh --info

Restore every unit your key opens to an empty directory; `YOUR_KEY.age.key`
is your key file ("Which file is your key"), and the escrow or operator key
opens every unit on the tape, a tenant's key only that tenant's:

    ./RESTORE.sh --restore --key YOUR_KEY.age.key --all --to /destination

Then check that it got everything: "Did the restore get everything?".

## Volume: {label}

This document describes how to recover data from this tape without
{intro_tools}

Besides those it uses `bash`, `awk` and the basic tools every Linux system
has (`sed`, `grep`, `tr`, `cut`, `sort`, `stat`, `df`, `wc`, `od`, `mktemp`),
and, for a unit that was compressed, `gzip`, `xz` or `zstd` ("Tools
Required" says what each is for, and names the few more that only finding
the drive and checking a key use).

## Start here

The whole job in order. Each step names the section that has its commands.
Unit, envelope, index, cover sheet: "Words this guide uses" says what each is.

1. Check your key: "Which file is your key". A tenant's key opens that
   tenant's units only; the operator key and the escrow key open every
   tenant's. With `age` installed (step 2), check it against the record you
   hold of its public half (`age-keygen -y`); with only a key file, the
   check is that it opens an envelope (`--find-envelope`, step 5).
2. Get a drive that reads this tape and a computer it connects to: "The
   tape drive" and "Loading the tape, and a drive that does not show up".
   The programs it needs, with their package names: "Tools Required".
3. Load the tape, find the drive's device name ("Which device?") and read
   RESTORE.sh, File 2, off the tape into a file named `RESTORE.sh`: the first
   commands of "Automated Recovery (recommended)". It is the same file on
   every tape of the box, so one copy serves them all. Set `TAPE_DEVICE` to
   the same device once, so RESTORE.sh reads the drive your `mt` and `dd`
   commands read.
4. Run `--info` on EVERY tape you hold. It needs no key and shows each
   tape's label, when it was sealed and whether it is complete; for a seal
   that is missing or not trusted see "If the seal is missing or not
   trusted".
5. Start from the NEWEST tape (the latest "sealed" date; File 0's
   `created_at` dates an unsealed one). `./RESTORE.sh --where --all --key
   YOUR_KEY.age.key` lists EVERY unit the archive's index names, with its
   newest version and the tapes that hold it, and `./RESTORE.sh --where
   UNIT --key YOUR_KEY.age.key` shows, for a unit, every version and every
   tape that holds it. `--find-envelope` lists the units on this tape.
   They decrypt into `$TMPDIR` (default `/tmp`; if `/tmp` is small, set
   `TMPDIR` to a directory on a real disk first: "Disk space").
   Those listings give each unit's size, so choose the disk now: a Linux
   filesystem (ext4 or xfs; not FAT, exFAT or NTFS) with room for the units
   you will restore: "Disk space". If the newest tape needs a drive you do
   not have, start from the newest your drive reads, and know that the
   newest index is out of reach: get that drive, or say what may be missing
   ("The rest of the box").
6. Restore. From one tape with the escrow or operator key, `--all` restores
   every unit your key opens that is on it ("Automated Recovery
   (recommended)"); check that it did ("Did the restore get everything?").
   From several tapes, restore each unit's NEWEST version once, from a tape
   that holds it, into a directory of its own (`/destination/UNIT`), and
   never an older version first ("The rest of the box" says how). A renamed
   unit has the name each tape shows. Two units of one name (compare the
   uuids `--where --all` prints) go in different directories:
   `/destination/UNIT-` and the first 8 characters of the uuid, each with
   only its own tenant's key. A unit that continues on another cartridge
   (`--where` shows a tape holding a range of it) restores from a tape that
   holds it whole, or starting on any tape that holds some of it, which asks
   for the others: "A unit on more than one cartridge".
7. Check each restore: "Did the restore get everything?". A slice or a file
   that is damaged: "If a slice or a file is damaged".
8. Know when you are done: every tape you hold opened with your key and
   the newest version of every unit any of them lists restored; every
   unit `--where --all` names on the newest readable index, and every unit
   the Heir Kit's catalogue lists on a sealed tape, restored or named as
   held only on tapes you do not have; and any tape your key cannot open
   named, never counted as looked at. No tape names one written after it
   ("The Heir Kit's catalogue"). "The rest of the box" has the whole rule,
   a damaged unit's place in it, and what a box cannot show.

No RESTORE.sh or tools: "Manual Recovery Steps" does the same job by hand.
The tape's map is lost: "If All Else Fails". A slice damaged in the middle,
which RESTORE.sh cannot read around: "tapectl itself".

## Words this guide uses

- **Tape file, "File N"**: a tape holds a numbered series of tape files,
  counted from File 0 and each ended by a filemark. They are not files on
  a disk: `mt` moves from one to the next and one `dd` reads one. "File 3"
  always means tape file 3.
- **ID thunk (File 0)**: the first tape file, plain text, that says which
  tape this is: its `label`, `uuid`, `media_type` (the LTO generation),
  `cartridge_serial` and `tapectl_version`, where the front index and the
  seal marker are (`front_index`, `seal_marker`) and `requires` (a list of
  features a reader must understand; it is empty on every tape of this
  layout). It also prints the commands that read the next file, this guide:
  "File 0's commands" in this guide means those.
- **Unit**: one archived folder tree, the thing you restore by name (for
  example `photos/2019`). A unit can be stored in several versions, one
  each time it was archived again after its contents had changed. Unless
  told otherwise a restore takes the newest version on the tape;
  `--version N` (with `--restore --unit UNIT`, or with `--list`) takes
  version N instead, and `--find-envelope` shows which versions this tape
  holds. Every version is a FULL archive of the unit as it was: the newest
  version alone holds every file, and an older one is never needed to
  complete it. "Every file" means the files the unit had when that version
  was made: a file deleted before then exists only in an older version, so
  restore an older version (`--version N`, into a directory of its own) only
  if you want such files. A version is also called a snapshot:
  MANIFEST.toml states it as `snapshot_version`, RECOVERY.md as "snapshot
  v2" and RESTORE.sh as "version". Versions are numbered 1, 2, 3 ...
  for each unit in the order they were made, so the highest number is the
  newest. A unit is
  identified by its `uuid`: it can be renamed, and tapes from different
  times or installations can use one name for different units, so version
  numbers compare only between archives with the same uuid. A tape's
  envelope names a unit by the name it had when THAT tape was written, so
  a renamed unit has different names on different tapes: every command
  takes a name, and the name to type is the one that tape shows ("A renamed
  unit", in "The rest of the box").
- **Tenant**: an owner of units. One tape can carry several tenants'
  units; each tenant has its own key, and one tenant's key cannot read
  another's data.
- **Operator**: whoever runs the archive. The operator's key opens every
  tenant's data on the tapes written to it (the operator's key is replaced
  from time to time, and an older tape needs the older key). The
  **escrow key**, a permanent spare made when the archive was set up and
  kept apart from the rest, is never replaced: it opens every tenant's data
  on every tape that installation wrote. Whichever of the three you hold,
  it is "your key" in this guide.
- **Installation**: one tapectl setup, with its own catalogue (its database
  of tapes, units and where the tapes are kept) and its own keys (an
  operator key, the tenants' keys and one escrow key). Every tape an
  installation writes is encrypted to that installation's keys, so a tape
  from another installation needs that installation's keys.
- **Heir Kit, cover sheet**: the papers and files the operator prepares
  for the person who inherits the archive and keeps with the tapes (or
  apart from them): a cover sheet (`COVER.txt`, also as a printable page)
  that names the escrow key's public half, where the tapes are kept and
  which drives read them, and `catalog.db.age`, the Heir Kit's catalogue
  (the whole database), encrypted to the escrow key ("The Heir Kit's
  catalogue" opens it). The kit also holds a copy of the tapectl program
  that made it, a file named `tapectl-` and its version ("tapectl itself").
  It records the archive as of the day it was made, which may be before the
  newest tape was written. You may have none.
- **`YOUR_KEY.age.key`**: the one name this guide uses for your key file,
  an age identity file (a text file with a line that starts
  `AGE-SECRET-KEY-`). Replace it with your own file's name, whether it
  holds a tenant, operator or escrow key.
- **Envelope**: a small encrypted tar archive near the front of the tape
  that says what is on it and where. There is one for each tenant, one for
  the operator (which covers every tenant), and a backup copy of each.
  "Your envelope" is an envelope your key opens: a tenant's key opens that
  tenant's envelope only; the operator and escrow keys open all of them.
  Inside are `MANIFEST.toml` (each unit's slices and the tape file each is
  in), `RECOVERY.md` (the steps for your units, written out as commands),
  `index/` (the index, below) and, in a tenant's envelope, `tables/` (each
  unit's file table and id map, below). The operator's envelope also holds
  `catalog.db`, a small database that has the same tables' rows.
- **Slice**: a unit is archived as one tar stream (an ordinary tar
  archive of its files), cut into numbered pieces called slices. A slice
  holds WHOLE files: it begins where a file begins. A file larger than a
  slice is a run of slices of its own, and every slice of the run after the
  first states `continues = true` in MANIFEST.toml. The slices joined in
  order are the stream, byte for byte. Each slice is (compressed, if the
  unit was, then) encrypted on its own and stored as one tape file, and one
  unit's slices lie together on the tape. Restoring a unit means giving tar
  its slices in order. A unit can continue on another cartridge: a tape
  then holds a **range** of it, one unbroken run of its slices in order
  ("A unit on more than one cartridge").
- **File table**: for each unit a text file (`tables/NAME.tsv` in your
  envelope; NAME is the first eight characters of the unit's uuid, `_v` and
  the version number, `_s` and a number) with one row for each file of the
  stream, in stream order: its kind (`f` file, `d` folder, `l` symbolic
  link, `h` hard link, and `p`, `c`, `b` for a FIFO and the two kinds of
  device), the slice it begins in, where it begins and ends in the stream
  (`start`, `end`, in bytes), its size, its sha256 and its path. A restore
  is judged by it: every restored file is hashed and compared with it. A
  path is written as the index writes it: a backslash as `\\`, a TAB as `\t`,
  a newline as `\n`, a carriage return as `\r`.
- **Id map**: for each unit a text file (`tables/NAME.ids.tsv`) that says,
  for every user and group number the unit's files carry, the name that
  number had on the computer that wrote the tape, or none. It is shown and
  never used: "What a restore on another computer does".
- **Index**: two different things. The FRONT index is File 3 (the seal
  marker, the last file, carries a copy): a plain-text map of THIS tape,
  with every tape file's position, type, size and hash. The index, with no
  "front", is the directory `index/` in an envelope: which units, versions
  and paths the archive had when that tape was written, and which tapes
  hold them. `--where` and `--find` read it ("The rest of the box").
- **Catalogue**: this guide never uses the word bare, because it names two
  different things. The Heir Kit's catalogue is the archive's database, as
  of the day the kit was made, kept as `catalog.db.age` ("The Heir Kit's
  catalogue"); an installation's own catalogue is the live database it was
  copied from.
- **Damaged-file list**: `.tapectl-damaged-files.txt`, a file RESTORE.sh
  writes in a unit's restored directory when it left a file out or a file
  failed its check: one path a line, with a header line that names the unit
  and version. It is the one repair's input ("If a slice or a file is
  damaged").

## Which file is your key

An age identity file is a text file with a line `AGE-SECRET-KEY-1...`
(74 characters, in capitals; a file made by `age-keygen` also carries
`#` comment lines, which do no harm). That line is the SECRET half, and the
only thing that decrypts. A line `age1...` is the PUBLIC half (a recipient): it names who a
file was encrypted for and decrypts nothing, so a file holding only that is
not a key. After `AGE-SECRET-KEY-1` the letters B, I, O and the digit 1
never occur, so a doubtful character on paper is one of the others. If
you copied a secret by hand, check the copy: type the line
alone into a file and run `age-keygen -y YOUR_KEY.age.key`; it prints the
public `age1...` that goes with that one key, which must equal the one on
the cover sheet or record you have for THAT key. A character miscopied is
refused with "invalid checksum".

If the key file was itself encrypted with a passphrase (it starts with
`age-encryption.org/v1`, or `-----BEGIN AGE ENCRYPTED FILE-----`), age
asks for the passphrase every time it opens the file, which for a unit of
many slices is every slice. Decrypt it once into a NEW file, `age -d -o
YOUR_KEY.decrypted.age.key PROTECTED_FILE` (PROTECTED_FILE is the file you
were given), and give the new file's name wherever this guide says
`YOUR_KEY.age.key`. Keep PROTECTED_FILE as it is: `age -o` replaces a file
of that name without asking, so never write the result to the name of the
protected file, or of any key you have only one copy of. Delete the new
file when you are done.

A key opens a tape only if the tape was written for it. If the escrow key
opens nothing (RESTORE.sh ends "no envelope on this tape ... opens with the
key given"; `age` itself says "no identity matched any of the recipients"
(`rage`: "No matching keys found") for every envelope), check it first as above; then try the operator key and every
tenant key you hold, one after another or together (`--key` may be
repeated); then ask whether this tape was written by another installation,
whose escrow is a different key. The keys that opened an envelope are
printed with it.

Nothing on a tape says which keys it was written for. `./RESTORE.sh --info`,
File 0 and the front index name no recipient (the plaintext parts of a tape
show its structure only: "What the unencrypted parts of this tape reveal"),
and an age file does not carry the name of the key it is for. So no tape
can be asked which key it wants: try your key on each tape, which
`--find-envelope` does. A key that opens one tape of a box may not open
another, because tapes written at different times were sealed to the keys
of their time, and tapes from another installation to its keys.

**A bad key file, or a tape of another installation?** tapectl refuses to
write without an escrow recipient, so the ESCROW key opens every tape its
own installation wrote, old and new. (An operator or a tenant key does not
promise that: after a key rotation an older tape needs the older key, so try
every key you hold.) For an escrow key, run `age-keygen -y YOUR_KEY.age.key`
and compare with the `age1...` on the cover sheet or in your record:

- It prints an error: the file is not a key, and nothing about any tape is
  learned from it. Type it again from the paper ("invalid checksum" is a
  miscopied character).
- It prints a different `age1...` from the sheet's: this file is not the
  escrow key the sheet names. It may be the operator's or a tenant's key,
  or a good key of another installation; trying it on the tapes (the
  paragraphs above) is the only way to tell.
- It prints the same `age1...`: the file is the right key. A tape it opens
  nothing on, when it opens others, was written by another installation
  (its envelope is not damaged: a damaged one is reported as DAMAGED, and
  its backup copy is read). It needs that installation's own keys. If the
  right key opens NO tape you hold, the cover sheet and these tapes did not
  come from the same archive.
- With no sheet to compare: a key file that opens an envelope on even one
  tape is a good file. One that opens nothing on any tape is a bad file or
  the wrong installation's, and nothing on the tapes says which. Try every
  other key you hold, and write down that you could not tell.

**Which key gives which route.** The escrow key and the operator key open
every envelope and every slice; they give every route in this guide, and
the only ones that give "tapectl itself". A tenant's key opens that
tenant's envelope and slices only: it gives RESTORE.sh and the manual steps
for that tenant's units, and a repair from another copy, and nothing of
tapectl's own restore ("tapectl itself" says why).

## Quick Reference

This volume uses layout 3. Tape files are laid out in this fixed order:

- File 0: ID thunk (this tape's identity; says the map is File 3)
- File 1: This guide
- File 2: RESTORE.sh (automated restore + verify script)
- File 3: FRONT INDEX — every file's position, type, on-tape size, and
  ciphertext hash. Read this first for a full map of the tape.
- File 4 onward: Tenant envelope(s) — one per tenant sharing this tape
- Then: Operator envelope, then Operator envelope backup
- Then: a backup copy of each tenant envelope, in the same order (the
  same bytes: if your envelope will not read, read its backup)
- Then: Data slices (age-encrypted pieces of each unit's tar stream) — one
  unit's slices are stored contiguously
- LAST file (File {last_file} on this tape): SEAL MARKER — asserts the tape
  is complete, and carries a full embedded copy of the front index (used
  if File 3 is damaged). A tape that ended early has its seal marker
  sooner, right after the last data slice written ("Where the tape ended")

The number of the last file is a fact about THIS tape. Another tape of the
box has its own: File 0 of that tape gives it (`total_files`, and
`seal_marker`, the number of its last file).

{drive_section}
{drive_basics}
## Tools Required

{tools_required}
- `bash`, `awk` and the basic coreutils every Linux system has (`sed`,
  `grep`, `tr`, `cut`, `sort`, `stat`, `df`, `wc`, `tail`, `od`,
  `mktemp`): RESTORE.sh is a bash script, and the manual steps below
  use them. Packages: bash; mawk or gawk (either is `awk`); coreutils;
  sed; grep
- `truncate` (coreutils) — only the manual steps use it, to trim block
  padding; RESTORE.sh does not
- `gzip`, `xz` (package xz-utils) or `zstd` — only for a unit that states
  `compression` in MANIFEST.toml, and only the one it names: the tool that
  reads each slice after it is decrypted ("Compressed units"). RESTORE.sh
  checks for it before it reads a slice of such a unit
- Which `tar`: the one your system has. A GNU tar (any version) or `bsdtar`
  restores what a tape holds; a BusyBox `tar` restores file CONTENT only and
  says nothing about what it left out ("What a restore on another computer
  does"). RESTORE.sh says which tar it used and what that tar does not
  restore, at the end of every restore
- Only for finding the drive and checking a key, never for the restore
  itself: `ls`, `lsscsi` (package lsscsi), `lspci` (pciutils), `dmesg`
  (util-linux), `lsmod` and `modprobe` (kmod), `sudo` (sudo) and `usermod`
  (passwd) (to find the drive, load its driver and get permission to use
  it: "Loading the tape, and a drive that does not show up"), and
  `age-keygen`, which comes with `age` ("Which file is your key"). Each is
  named where this guide uses it.
- `sqlite3` (package sqlite3) — optional, only to read the Heir Kit's
  `catalog.db.age` ("The Heir Kit's catalogue"); no restore needs it.

On Debian 11 or later and Ubuntu 22.04 or later every one of these is a
package of the same name or one named above (`apt` installs them): mt-st,
coreutils, tar, age, gzip, xz-utils, zstd, sqlite3, lsscsi, pciutils, kmod
and, for tapectl itself, sg3-utils ("tapectl itself"). Debian 10 has every
one of those packages but `age`: install the others the same way and take
age from a release (github.com/FiloSottile/age/releases has a Linux build
that needs no installation). Package names are Debian's and Ubuntu's; other
systems name the same programs their own way.

RESTORE.sh checks for the tools each mode runs, before it reads the tape,
and names any that is missing: `--info` and `--verify` need only {read_only}.

## Disk space

`/destination` in this guide is a placeholder for an empty directory (or
one that does not exist yet: RESTORE.sh makes it) on a disk with room.
Choose the disk once you have listed the units you will restore
(`--find-envelope` and `--where --all` print each unit's size), not
before: how much room you need depends on which units you restore.

The disk needs a filesystem that keeps what the archive holds: file names
exactly as they were, files of 4 GiB and more, symbolic links and Unix
permissions. A Linux filesystem does (ext4 and xfs are the usual ones).
FAT, exFAT and NTFS do not keep all of that (FAT cannot hold a file of 4
GiB or more, and none of the three keeps Unix permissions and symbolic
links as Linux does), so restore to a Linux disk, and copy the result to
another filesystem afterwards if you must.

**How much room, in one place:** the sum of the sizes of the units you
restore, as `--find-envelope` and `--list` print them ("bytes of stream":
the size of the unit's files as stored, with a little tar framing; this is
the size the restored files take, whether or not the unit was compressed);
and, for the slice RESTORE.sh is working on, twice the largest slice of the
unit, in its work directory. Doing it by hand ("Manual Recovery Steps") is
the same, more: the decrypted slices (the unit's stream size) rest on the
disk beside the restored files (the unit's size again); delete a unit's
slices once it is restored.

{capacity}

RESTORE.sh reads one slice at a time from the tape, checks it, decrypts it
(and decompresses it) into a work directory and gives it to tar, so `--to`
needs room for about the units you restore and one slice's worth more: one
unit's size for one unit, the sum of them with `--all`. The work directory
is a directory named `.tapectl-work.` and some letters INSIDE `--to` (for a
restore), or under `$TMPDIR` (default `/tmp`; set `TMPDIR` to move it) for
the modes that only read; `--work DIR` puts it on another disk. It holds the
decrypted envelopes, the file tables among them: a unit of millions of
files has a table of hundreds of MiB. `/tmp` is a small RAM disk on many
systems (`df -h /tmp` says); if `/tmp` is small, set `TMPDIR` to a directory
on a real disk with some GiB free (`TMPDIR=/big/disk/tmp ./RESTORE.sh ...`).
RESTORE.sh checks the space before it reads a slice and names a full disk
when it happens ("Nothing on the tape is wrong"); `--no-space-check` skips
the check, and `--overwrite` finishes a restore once you have made room.

For a whole box, the destination needs the sum of the sizes of the units
you restore, whichever tapes they come from; the extra room for slices is
needed for one unit at a time.

**Compressed units.** A unit that was stored compressed says so
("Compressed units"), and its slices on the tape are smaller than its
files. The restored size is the unit's "bytes of stream", which
`--find-envelope` and `--list` print, so there is nothing to guess: that is
the room to have.

## Which device?

Every command in this guide says `/dev/nst0`. That is an example, not a
fact about this tape — drive numbering belongs to the machine you are
sitting at, and it changes when hardware is added or the machine reboots.
List the tape drives and substitute the right one throughout:

    ls -l /dev/tape/by-id/

Choose the NON-REWINDING entry: the one whose name ends in `-nst` (it
points at a `/dev/nstN`), not the entry without it (a `/dev/stN`). A
rewinding device rewinds the tape each time a command finishes, so every
`mt fsf` is undone and every read starts again at File 0, which looks
like a damaged tape and is not. The by-id name is the steadiest to use,
because it does not change when the numbering does: it looks like
`/dev/tape/by-id/scsi-XXXXXXXX-nst` (an example: use your own).

With several drives, ask each one whether it has a cartridge loaded:

    mt -f /dev/nst0 status

The one that says `ONLINE` (or does not say `DR_OPEN`) has a cartridge
in it; one that is empty says `DR_OPEN`. Then confirm it is yours: a wrong
device does not always fail loudly: if another tape is loaded in it, the
commands below will succeed and recover the WRONG volume. File 0 names the
volume this tape carries, and `./RESTORE.sh --info` prints it on its
"Tape:" line: compare it with the label on the cartridge you meant. A drive that is
not your own may hold someone else's tape: on a drive you are not sure of,
run only read-only commands (`mt status` is one), and never a rewind, a
read or a write.

`mt` and `dd` take the device in their own words (`mt -f DEVICE`,
`dd if=DEVICE`). RESTORE.sh reads it from the environment variable
`TAPE_DEVICE` (default `/dev/nst0`). Put the variable in front of the
command, on the same line:

    TAPE_DEVICE=/dev/nst1 ./RESTORE.sh --info
    TAPE_DEVICE=/dev/tape/by-id/scsi-XXXXXXXX-nst ./RESTORE.sh --verify

or set it once for the whole shell session with
`export TAPE_DEVICE=/dev/nst1`.

## Automated Recovery (recommended)

The easiest way to recover is the RESTORE.sh script (File 2). It is plain
text, so deleting the zero bytes of the tape's block padding with
`tr -d '\0'`, as below, is safe for it — and for Files 0 to 3 and the
last file, which are plain text too. It is NEVER safe for an encrypted
file, an envelope or a slice: ciphertext holds zero bytes of its own, and
deleting them destroys it (see "If All Else Fails").

If your drive is not `/dev/nst0` ("Which device?"), put your own device in
the `mt` and `dd` lines below, and set `TAPE_DEVICE` to the same device
once, with `export TAPE_DEVICE=/dev/nstN` (a by-id name works too). The
`./RESTORE.sh` lines further down read `TAPE_DEVICE`, and without it they
read `/dev/nst0`, which may be another drive or another tape.

    mt -f /dev/nst0 setblk 524288
    mt -f /dev/nst0 rewind && mt -f /dev/nst0 fsf 2
    dd if=/dev/nst0 bs=512k | tr -d '\0' > RESTORE.sh
    chmod +x RESTORE.sh

File 2 is the same file on every tape, so the one copy reads any tape of
the box. (A volume written by tapectl in another layout has another File 2 and says
so: read it with the RESTORE.sh in its own File 2.)

The first line sets the drive to the tape's 512 KiB blocks. Repeating it
is harmless, and it is not needed if you have just run File 0's commands
(the ones File 0 prints for reading this guide), which set it: the drive
keeps the setting until the tape is unloaded or the drive is reset, and
RESTORE.sh sets it itself. Only mt-st's `mt` can do it (GNU cpio's cannot;
the line then fails, and the rest still works while the drive is in its
default mode, where `dd bs=512k` reads each 512 KiB block whole). If a
`dd` fails, this is the first thing to try. What `dd` says:

- "Cannot allocate memory": the tape's block is larger than the read you
  asked for. Check the command says `bs=512k` (dd's default is 512 bytes),
  set the block size as above, rewind, and read again.
- "Input/output error": the drive could not read. A drive set to another
  fixed block size does this (set 524288 as above); so does a tape of a
  generation this drive cannot read ("The tape drive"), a dirty head (a
  cleaning cartridge is the usual cure), or a damaged tape. Rewind and try
  again, then another drive, and then the other copies of the tape, before
  you decide anything is lost.

Reading a whole tape takes hours, and the drive shows little while it
works: a full LTO-6 tape reads in about four and a half hours (2.5 TB at
about 160 MB/s), a full LTO-8 in about nine. `--verify` and `--all` read
the whole tape; a restore of one unit reads only that unit's slices.

Which tape is in the drive: File 0 says, and so does the first line of
`./RESTORE.sh --info` ("Tape: LABEL"). By hand:

    mt -f /dev/nst0 rewind
    dd if=/dev/nst0 bs=512k | tr -d '\0' | grep -E '^(label|uuid|media_type|cartridge_serial|created_at|tapectl_version) ='

Then, with `YOUR_KEY.age.key` your own key file:

    # See what is on the tape and whether its seal is there ("Seal marker:")
    ./RESTORE.sh --info

    # Keyless integrity check of every file against the front index:
    # no key needed, works even without your envelope
    ./RESTORE.sh --verify

    # Find your encrypted envelope (an operator or escrow key opens
    # every envelope, and the units of each are listed)
    ./RESTORE.sh --find-envelope --key YOUR_KEY.age.key

    # List a unit's files, from its file table (no data slice is read)
    ./RESTORE.sh --list --key YOUR_KEY.age.key --unit UNIT

    # Which other tapes hold a unit, or a file ("The rest of the box"),
    # and every unit the index names, with no limit
    ./RESTORE.sh --where UNIT --key YOUR_KEY.age.key
    ./RESTORE.sh --find TEXT --key YOUR_KEY.age.key
    ./RESTORE.sh --where --all --key YOUR_KEY.age.key

    # Full restore of THIS tape to an EMPTY directory: --all restores
    # the newest version of every unit your key opens, each into
    # /destination/UNIT (a lone unit goes straight into /destination).
    # Each slice is read from the tape, checked, decrypted and given to
    # tar, so /destination needs about those units' size free; the script
    # checks first. A restore that stopped part way is finished by
    # running it again with --overwrite, never over an older version.
    ./RESTORE.sh --restore --key YOUR_KEY.age.key --all --to /destination

    # One unit, into a directory of its own (name it for the unit: two
    # units must never share a directory)
    ./RESTORE.sh --restore --key YOUR_KEY.age.key --unit UNIT --to /destination/UNIT

    # Just one file or folder: reads only the slices that hold it, into the
    # unit's own directory, where a whole-unit restore would put it
    ./RESTORE.sh --restore --key YOUR_KEY.age.key --unit UNIT \
        --path SOME_PATH --to /destination/UNIT

With the ESCROW key or the OPERATOR key (`YOUR_KEY.age.key` is then that
key's file), `--find-envelope` and `--list` show what the tape holds, and
`--all` restores all of it. A single TENANT's key opens only that tenant's
envelope: it restores that tenant's units and nobody else's.

`--all` restores the newest version of every unit that your key opens, on
this tape, each into its own directory `/destination/UNIT` (a single unit
goes straight into `/destination`). With several keys (`--key` repeated) it
is every unit any of them opens. Two units of one NAME (two tenants may
name theirs alike) are one name to RESTORE.sh, and it takes the one with the
higher version: restore each with only its own tenant's key
(`--key YOUR_KEY.age.key`, that tenant's file), into a directory of its own. Without `--all` or
`--unit`, `--restore` takes the units your key opens and, if there is one
unit, restores only it; if there are SEVERAL it will not guess: it prints
them and stops before it reads a slice, and you name one with `--unit UNIT`
(repeat `--unit` for several, each into `/destination/UNIT`) or restore
them all with `--all`. `--restore` takes `--version N` for another version
than the newest.

If the operator envelope and its backup will not read, the envelopes that
do read may be only some tenants', and `--all` then restores only their
units, with the warnings the script printed as the only sign. The script
names each envelope it opened ("The key opens the envelope at file N"). The
check in "Did the restore get everything?" says how to tell.

The script reads the tape forward from one rewind, and says what it
needs if it meets a tape written by another layout or a newer tapectl
(`requires` in File 0).

Quitting a restore (Ctrl-C, a closed terminal, `kill`) ends it cleanly: no
work directory and no partial file is left behind, and the files restored
and checked so far stay.
## A unit on more than one cartridge

Every unit may continue on the next cartridge: a tape that reaches the
drive's early-warning mark finishes the slice it is writing, is sealed, and
the rest of the unit is written first on the next tape ("Where the tape
ended"). Nothing is split inside a slice, and a unit that fits on one tape
is on one tape. A tape that holds only some of a unit's slices holds a
RANGE of it: one unbroken run of its slices, in slice order. A range is not
damage, and it is not a copy of the unit on its own.

What one tape shows. Its envelope's MANIFEST.toml lists EVERY slice of the
unit, 1 to N, each with its `encrypted_bytes` and its `sha256_encrypted`,
and gives a `tape_position` only to the slices this tape's plan holds. A
slice with no `tape_position` is on another tape: it is never at tape file
0. The seal marker's `ended_after` can end the run sooner, as it ends any
tape ("Where the tape ended"), and a slice after it is on another tape
too. `--info` and `--verify` need no key and see the range as data slices
like any other. `--find-envelope` marks such a unit `PART:` and `--list`
says which slices of the unit this tape holds. `--where` and `--find` print
the run of slices each tape holds when it is not the whole version, marked
"a range".

What a tape read alone gives. Because a slice holds whole files, a range
read alone restores every file whose slices are all on it: those files are
restored and checked, and the files that lie across the cartridge boundary
or in slices on another tape are listed in the damaged-file list, with the
tapes the index names for them. A hard-linked name whose first name is on
another tape is listed for the repair. So a restore may START ON ANY TAPE of
a unit that your key opens: the first command below, with the cartridge you
have in the drive.

Prefer a whole copy: if any tape holds the version whole (`--where` prints
no "a range" beside it), restore from that tape alone, as any unit, with no
change of cartridge.

To read a unit across its cartridges, with the tape that holds its first
slice in the drive (a tape never names a tape written after it, so
`./RESTORE.sh --where UNIT --key YOUR_KEY.age.key` on the NEWEST tape
names the tapes and the slices each holds; write them down before you
start, so that you can find each cartridge when it is asked for):

    ./RESTORE.sh --restore --key YOUR_KEY.age.key --unit UNIT --to /destination/UNIT

When the tape in the drive does not hold the next slice, RESTORE.sh says
which slice it needs, names the tapes the index in your envelope gives for
it, tells you to eject the cartridge now in the drive (its eject button, or
`mt -f /dev/nst0 offline` from another terminal: nothing holds the device
open while it waits), and asks for the next cartridge at a prompt. It waits
for as long as you take, and reads your answer from its input, one line:

- ENTER (an empty line), after you have loaded the cartridge that holds the
  slice and waited for the drive to be ready: it reads that cartridge. It
  takes a cartridge only when that tape's front index lists the next
  slice's `sha256_encrypted`, at a position its own seal marker covers; a
  label or an index never stands in for the hash. A blank, unreadable or
  foreign cartridge, or the one already in the drive, is named and the
  prompt asks again.
- `done` and ENTER, or the end of the input (Ctrl-D): "I do not have it".
  The restore goes on with what it has: the files whose slices it read are
  restored and checked, and the files that lie in slices it could not read
  are written to the damaged-file list with the cartridges an index names
  for them. It does not ask again for that unit.
- `quit` and ENTER: stop. The files restored and checked so far stay, a
  file tar had begun is removed, and what was left out is on the
  damaged-file list. RESTORE.sh ends `RESTORE STOPPED`.

The restore across cartridges is one tar run: nothing is kept on disk between
cartridges but the one slice being read. Ctrl-C, a closed terminal or a lost
connection to the computer, and `kill` end it too, and leave no work
directory behind: a restore across tapes can take hours of cartridge
changes, so run it inside `screen` or `tmux`.

A LATER RUN into the same destination, from another tape of the same unit,
adds to the earlier run. Give the same command with the next tape in the
drive and the same `--to`: where `--to` holds the damaged-file list of the
same unit and version, RESTORE.sh restores only the files that tape holds
whole and that do not already pass their sha256 in the unit's file table.
A file that passes is never written again, and the list is rewritten with
what is still missing, or removed. Two limits, both measured: a folder's
modification time is the one it got in the last run that touched it, and a
hard-linked name whose first name was not read in the same run is not made
by that run (the repair, "If a slice or a file is damaged", makes it once
the tape that holds the first name has been read). One run across the
cartridges is the way to read a unit whole.

By hand, across tapes: read each tape's slices of the unit into one
directory, each taken from the tape whose front index lists its
`sha256_encrypted` ("Manual Recovery Steps", step 7, for each tape), and
join them in order before tar sees them.

## What a restore on another computer does

A tape stores the owner and group of every file by NUMBER only: no user or
group name is in the tar stream, and no name is on the tape in the clear.
So a restore never looks a name up, and never maps one. What that means
where the tape is restored:

- Run as an ordinary user, tar restores every file as yours (owner and
  group are set only by root). Run as root, tar sets each file's owner and
  group to the numbers the tape records. On another computer those numbers
  are other accounts, or none: a file then belongs to whoever has that
  number there. Nothing is remapped. If the files should belong to someone
  else, change that after the restore (`chown`), once you know who the
  numbers were.
- An ACL entry (a permission granted to one more user or group than the
  file's own) is stored by number the same way, and tar restores it by
  number, as `--acls` does: the entry follows the number, and on another
  computer it names whoever has that number, or nobody. An ACL, or an
  extended attribute, that the destination file system will not take is
  reported by tar and not restored; BusyBox's tar restores neither the
  owner, the ACL nor any extended attribute, and does not say so.
- The ID MAP says who the numbers were: for each unit, the names the
  computer that wrote the tape gave each user and group number its files
  carry (or none). RESTORE.sh prints it after each unit it restores,
  headed "Who the ids were", and it is the file `tables/NAME.ids.tsv` in
  your envelope. It is information: a restore shows it and never acts on it.
- A tar restores a file's contents, its times and its permission bits as
  an ordinary user, whatever the tar; which tar restores what else:

  GNU tar, any version: content, permission bits, times (a folder's too
  when the unit is read in one run), owner and group by number as root,
  extended attributes and ACLs. `bsdtar` the same. BusyBox `tar`: content
  only.

The extra things a restore may lose are named at its end: "The tar used"
and "What it does not restore". A restore is judged by the per-file check
against the file table (the sha256 of every file), never by tar's exit
status, which is 0 for a BusyBox tar that left files out.

## The rest of the box: other tapes, copies and the index

If you hold more than this one tape, work out first what the others are.
A tape may be a COPY of this one (the same units written again on another
cartridge, so that one can be lost), or hold OTHER units, or hold other
VERSIONS of a unit this tape holds: a unit's older versions need not be
on the same tape as its newest one.

Every tape carries an index inside its envelopes that answers this: the
directory `index/`, whose file `units.tsv` is a table with a row for each
version of each unit and each tape that holds it. For each unit it lists
every version and the tapes that hold it (label, uuid, cartridge serial,
generation, state and when it sealed), and a tenant's envelope also has
`paths.tsv`, which lists every file path. RESTORE.sh reads it:
`./RESTORE.sh --where --all --key YOUR_KEY.age.key` lists EVERY unit the
index names (no limit): its newest version, whether the tape in the drive
holds it, and every tape that holds each version;
`./RESTORE.sh --where UNIT --key YOUR_KEY.age.key` prints every version of
UNIT and every tape that holds each; `./RESTORE.sh --find TEXT --key
YOUR_KEY.age.key` prints which unit, version and tape hold a path
containing some text (the commands of "Automated Recovery
(recommended)").

An index is as of the moment its own tape was written. It never names a
tape written after it, and a tape it names may since have been retired or
erased. So the NEWEST tape's index is the most complete: start with the
newest tape you have (`./RESTORE.sh --info` prints when it was sealed), and
expect an older tape to know less. The index is plain text in the envelope
(`index/` in the unpacked envelope; RECOVERY.md shows how to read it with
`awk`), so it needs no tapectl either.

**Is the box complete?** Nothing on a tape can name tapes written after
it, so the newest tape you hold may not be the newest ever written, and no
tape can tell you that. What you can do is compare. The newest index lists
every unit the installation's catalogue knew when its tape was written,
including units held only on OTHER tapes (`units.tsv` has a row for each
tape that holds each version, and `--where UNIT` prints those rows).
`./RESTORE.sh --where --all --key YOUR_KEY.age.key` prints all of them, with
no limit: for each unit its newest version, whether the tape in the drive
holds it, and every version with each tape that holds it (label, uuid,
cartridge serial, generation, state); the units whose newest version is not
on that tape are named first. List the units the newest index you can read
has and the tapes that hold them, and set that against the tapes in the box:
a holder the index names that you do not have is a tape to look for, and a
unit, or the newest version of a unit, whose every holder is missing is not
in this box. Nobody may be left to ask, so look for the answer on paper
too. Where a tape is kept is written on no tape: the index names a missing
tape only by its label, cartridge serial and uuid, so look for cartridges
labelled so, and for the operator's papers and storage records. The Heir
Kit, if you have one ("Words this guide uses"), records the
archive as of the day it was made, including where the tapes were kept: the
cover sheet's "Tapes are at" box, and the Heir Kit's catalogue in
`catalog.db.age`, which "The Heir Kit's catalogue" opens; one made after
your newest tape was written may name tapes this box lacks.

The whole box, in short:

1. Name every cartridge. Load each in turn and run `./RESTORE.sh --info`
   (any copy of RESTORE.sh will do: it is the same file on every tape):
   its label is "Tape:" (File 0's label is the authority; a sticker on
   the case can have been replaced) and its sealed date is on the "Seal
   marker:" line (a tape with "NOT FOUND" there is unsealed: File 0's
   `created_at`, which the `grep` in "Automated Recovery (recommended)"
   prints, says when it was written). The generation is printed on the
   cartridge (L6 is LTO-6) and stated in File 0 as `media_type`; tapes of
   different generations need different drives ("The tape drive" has a
   table, so that you can choose one drive for the box).
2. Start from the newest tape. `./RESTORE.sh --where --all --key
   YOUR_KEY.age.key` lists every unit its index names, each with its newest
   version and the tapes that hold it; `./RESTORE.sh --find-envelope --key
   YOUR_KEY.age.key` lists the units on the tape in the drive. For one unit
   `./RESTORE.sh --where UNIT --key YOUR_KEY.age.key` prints a "Version N"
   for each version of the unit, with the tapes that hold it; the highest
   N is the newest. If the newest tape needs a drive you do not have, start
   from the newest tape your drive reads, and know that the newest index is
   out of reach: get that drive if you can. If you cannot, say with what you
   restore that units written after the tape you could read may be missing.
3. For each unit restore its newest version from a tape that holds it,
   each unit into a directory of its own so that tapes never overwrite one
   another: `./RESTORE.sh --restore --key YOUR_KEY.age.key --unit UNIT
   --to /destination/UNIT`. `--to` must be empty or new (a directory that
   is not empty is refused, unless it holds the damaged-file list of this
   unit: "A unit on more than one cartridge"), which is why each unit has
   its own directory under /destination and not /destination itself. A tape
   that holds the newest version needs no `--version`: a restore takes the
   newest version on the tape.
4. Do not restore an older version first and then a newer one into the
   same directory: it is refused as not empty, and `--overwrite` would keep
   the files that only the older version had. If a directory already holds
   an older version, move it aside and restore the newer version into an
   empty one. An older version is wanted only if you ask for it
   (`--version N`), and then in a directory of its own.
5. A tape the index names that you do not have: use another holder it
   lists. If no holder of a unit's newest version is in the box, the
   highest version you can reach is the best this box can give, and whoever
   uses it should be told so. A unit all of whose holders are missing
   cannot be restored from this box.
6. You are done only when ALL of these hold; fewer is not done:
   - Every tape in the box has had its envelopes opened with your key
   (`./RESTORE.sh --find-envelope --key YOUR_KEY.age.key`), and the newest
   version of every unit any of them lists is restored: the highest
   version number among the versions with that unit's `uuid`.
   - Every unit the newest readable index names (`--where --all`) is
   restored at its newest version, or you have written down that it is
   held only on tapes you do not have.
   - With a Heir Kit: every unit the Heir Kit's catalogue lists on a tape
   whose status is `sealed` ("Which tapes hold which units", in "The
   Heir Kit's catalogue") is restored at the newest version the kit
   names, or you have written down that it is held only on tapes you do
   not have. A kit made after your newest tape is the one thing you hold
   that can name a tape written after it, so the two checks above do not
   cover this one.
   - A unit whose every copy in the box is damaged counts once it is
   restored as far as its copies allow ("If a slice or a file is
   damaged": RESTORE.sh's one repair, then tapectl's own restore if your
   key allows it), its `.tapectl-damaged-files.txt` is kept beside it,
   and you have written down that the files on that list are lost. An
   older version, restored into a directory of its own, may hold older
   copies of them.
   - A tape your key cannot open is NOT done. `./RESTORE.sh` ends with "no
   envelope on this tape ... opens with the key given", and `--info`,
   which needs no key, shows only the tape's label, dates and map, never
   what it holds. Do not count that tape as looked at: name it (a tape of
   another installation, or a key you do not have) and try every other
   key you hold on it. If none opens it, the box is not done, and you say
   so, by that tape's name.
   - An unsealed tape (no seal marker) is ordered by File 0's
   `created_at`. If the newest tape in the box will not read, the older
   tapes' indexes cannot see what was written after them: say that some
   units may be unknown.
   That is the most a box can show about itself: "Is the box complete?"
   above says what it cannot.

   The newest version of a unit holds the files that version had. A file
   deleted before it was made exists only in an older version: `--list
   --version N` of two versions prints each one's files, and a file only the
   older one lists is the file to restore from it (`--version N`, into a
   directory of its own, with `--path`).

**A renamed unit.** A unit is the same unit by its `uuid`, but every
command takes its NAME, and a tape's envelope knows a unit by the name it
had when THAT tape was written. If the unit was renamed after an older tape
was written, the newest index lists it under the new name (with the older
tape as a holder of an earlier version), and the older tape's envelope still
says the old name. `--unit NEWNAME` finds nothing there: RESTORE.sh says
"unit 'NEWNAME' ... is not in the envelope(s) your key opened" and lists
the units there. That reads like a wrong key and is not one. Take the
unit's uuid from the newest index (`--where --all` prints it as "unit
uuid", beside the name), and on the older tape type the name THAT tape shows
for the same uuid: `./RESTORE.sh --where --all --key YOUR_KEY.age.key` on
that tape prints each name with its full uuid, and the first eight
characters of the uuid are in the name of the unit's file table
(`tables/UUID8_vN_sN.tsv`).

**Two units of one name.** A name is unique inside one installation's
catalogue, but it can recur across tapes: a unit renamed and its old name
used again, or tapes of two installations. They are different units when
their uuids differ (`--where --all` prints each as an entry of its own,
with its uuid). The box procedure puts each unit in `/destination/UNIT`,
and two such units would meet there. RESTORE.sh refuses a `--to` that is not
empty, but restoring by hand into a directory that already holds files
mixes them without a word. Restore each into a directory of its own,
`/destination/UNIT-` followed by the first 8 characters of its uuid, and
with the manual path always into a new, empty directory. (RESTORE.sh takes
a unit by name: for two of one name that are in different tenants' envelopes,
give each restore only its own tenant's key.)

The modes that only read the tape (`--info`, `--verify`, `--find-envelope`,
`--list`, `--where`, `--find`) describe the tape that is in the drive, with
any copy of RESTORE.sh: it is the same file on every tape.

In the index's `state` column, `sealed` is a tape whose write finished and
was sealed, `quarantined` is a sealed tape whose medium a verify proved bad
(it may still read in part, so prefer another holder), and `this-tape` is
the tape the index is on. A tape that was already retired or erased when
the index was written is not listed, and one that is listed may have been
retired or erased since. A cartridge that was erased and written again
carries a NEW volume: File 0 then shows another label and another `uuid`
on the same `cartridge_serial`, and the index row for the old volume names
data that is gone. So match a tape to an index row by its label AND its
uuid (`--where` prints the uuid; File 0 shows it, as the `grep` of "Automated
Recovery (recommended)" prints).

To take stock of a box without relying on the index, load each tape in
turn, run `./RESTORE.sh --info` (its label and whether it was sealed) and
`./RESTORE.sh --find-envelope --key YOUR_KEY.age.key` (its units, their
versions, slices and sizes), and write them down. A unit that appears at
the same version AND with the same unit uuid on two tapes has two copies
only when each of the two tapes holds that version WHOLE: then they are the
same content, and either restores it alone. A tape that holds only part of
a unit across tapes ("A unit on more than one cartridge") is not a copy:
two tapes that each hold part of a unit are ONE copy, and restore it only
together. To tell, run `./RESTORE.sh --list --key YOUR_KEY.age.key --unit
UNIT` on the tape: it says "PART:" when the tape holds only part of that
version, and `--where UNIT` marks such a tape "a range". A name and a
version number alone prove nothing; the uuid is in MANIFEST.toml
(`--find-envelope` does not print it; `--where --all` does). Cartridges are
found by their label; the cartridge's chip serial is in File 0
(`cartridge_serial`) and in the index, and a sticker can have been
replaced.

## The Heir Kit's catalogue (catalog.db.age)

If you hold the Heir Kit ("Words this guide uses"), its file
`catalog.db.age` is the Heir Kit's catalogue, the archive's whole database
(SQLite), encrypted to the ESCROW key alone: a tenant's or the operator's key does
not open it. It can say which tapes there are and where they were kept,
which no tape can. It is as of the day the kit was made. It knows no tape
written after that day; a unit's name, a tape's place or a tape's state may
have changed since; and where it and a tape disagree, the tape is right. It
needs `age` and `sqlite3` (package `sqlite3`; "Tools Required").

Decrypt it, with the escrow key's file as `YOUR_KEY.age.key`, into a NEW
file (`age -o` replaces a file of that name without asking), then see what
it holds:

    age -d -i YOUR_KEY.age.key -o catalog.db catalog.db.age
    sqlite3 catalog.db .tables

Every tape the Heir Kit's catalogue knows, with its cartridge's sticker and chip
serial, generation, state, when it was sealed and where it was kept:

    sqlite3 -header -column catalog.db "SELECT v.label AS tape,
      c.barcode AS sticker, c.serial_number AS chip_serial,
      v.media_type AS generation, v.status,
      v.observed_condition AS condition, v.sealed_at, l.name AS place
      FROM volumes v
      LEFT JOIN cartridge_volumes cv ON cv.volume_id = v.id
      LEFT JOIN cartridges c ON c.id = cv.cartridge_id
      LEFT JOIN locations l ON l.id = v.location_id
      ORDER BY v.label;"

Which tapes hold which units, version by version, with the slices each
holds. To look for one unit, add `AND u.name LIKE '%word%'` before `ORDER
BY`; for one tape, add `AND v.label = 'TAPE'`:

    sqlite3 -header -column catalog.db "SELECT u.name AS unit, u.uuid, s.version,
      v.label AS tape, v.status, v.observed_condition AS condition,
      w.status AS holds,
      (SELECT MIN(sl.slice_number) || '-' || MAX(sl.slice_number)
         FROM write_positions wp JOIN stage_slices sl ON sl.id = wp.stage_slice_id
        WHERE wp.write_id = w.id AND wp.status = 'written') AS slices,
      (SELECT COUNT(*) FROM stage_slices sl
        WHERE sl.stage_set_id = w.stage_set_id) AS of_slices
      FROM writes w
      JOIN stage_sets ss ON ss.id = w.stage_set_id
      JOIN snapshots s ON s.id = ss.snapshot_id
      JOIN units u ON u.id = s.unit_id
      JOIN volumes v ON v.id = w.volume_id
      WHERE w.status IN ('completed', 'range')
      ORDER BY u.name, s.version, v.label;"

`holds` is `completed` for a tape that holds that version whole and `range`
for one that holds only the run `slices` names, of the version's
`of_slices` (a unit on more than one cartridge:
"A unit on more than one cartridge"). A `range` tape is not a copy on its own: the whole version restores only
with every slice, 1 to `of_slices`, from the tapes that hold them, and its
rows together say which tapes those are. Alone it restores the files whose
slices it holds, whole, and lists the rest.

A tape holds a good copy when its `status` is `sealed` and its `condition`
is `ok`. `erased` is a wiped tape and `retired` one taken out of service:
neither is a copy. `quarantined` (a `condition`) is a sealed tape whose
medium a verify proved bad: it may read in part, so prefer another holder. Any other status is a tape that was
never finished, or that tapectl did not write: count on nothing from it.
`tape` is the tape's label, the one File 0 shows. The sticker is the barcode
on the cartridge, which can have been replaced; the chip serial is File 0's
`cartridge_serial`. A unit is listed by the name it had on the day the kit
was made ("A renamed unit", in "The rest of the box"), beside its uuid,
which `--where --all` prints as "unit uuid": match a unit by its uuid. If a query fails
because a table or column is missing, the kit came from another tapectl
release: `sqlite3 catalog.db .schema` shows the tables it has.


## If the seal is missing or not trusted

`./RESTORE.sh --info` prints a "Seal marker:" line, a statement about the
tape as a whole:

- "present ... the tape is complete, every planned file is on it": the seal
  marker (the last file) is there and agrees with the front index. The
  write finished and the two ends of the tape match. Carry on.
- "present ... this tape ended EARLY, after file N": a SEALED tape that ended
  early, which every tape may do. A unit with a slice after file N is on
  this tape in part only: "Where the tape ended".
- "NOT FOUND": there is no valid seal marker. The write never finished: it
  was interrupted or aborted, or the tape ran out of room. Envelopes are
  written before slices, and the front index before both, so the front
  index lists EVERY file the write planned, slices that were never written
  included: such a slice cannot be read, and what is missing is normally the
  last data slices. What was written before the break is intact and checks
  against the index.
- "present but NOT TRUSTED": a seal marker is there, but it does not match
  the front index (or counts more files than File 0's plan), or the front
  index will not read. One of the plain-text files was damaged or replaced
  since the write, and neither can vouch for the other. The tape is read as
  unsealed, and what you can check yourself is each file against the hash
  the front index lists for it, which is what follows.

In both cases go file by file, because each file is checked against its
own recorded hash: `./RESTORE.sh --verify` prints PASS or FAIL for every
tape file, by number; a file that is missing or will not read is a FAIL, and
it ends `VERIFY PASSED` only for a sealed tape every file of which passed.
To find which slices are incomplete, take the tape file numbers of a unit's
slices from its MANIFEST.toml (RECOVERY.md's table of "Tape files" lists
them too) and look them up in that list. RESTORE.sh itself reads the front
index from File 3 and, if that fails, from the seal marker's copy of it, with
a warning (rung 2 of "If All Else Fails").

- A unit whose slices all PASS is complete: restore it as usual. RESTORE.sh
  does not need the seal, and checks every slice again as it restores.
- If every file PASSes against the front index and only the seal
  disagrees, the data is exactly what that index describes (each file
  matches its recorded hash): the fault is in the seal marker or the
  index, it does not matter which for restoring, and restoring is sound.
- A unit with a FAIL slice cannot be restored whole from this tape.
  RESTORE.sh leaves the failed slice out, restores every file whose slices
  are all good, lists the others and ends `RESTORE INCOMPLETE`. Look for
  another copy first ("The rest of the box"; "If a slice or a file is
  damaged" says how the one repair takes the listed files from it); a
  cartridge that failed here may read in another drive. With no other copy,
  the files that lie wholly inside the good slices are restored, and the
  files in the failed slices are lost.

## Where the tape ended

Every tape of this layout may end before the last file the front index
plans: when the drive reported that its medium was nearly full while it
wrote a data slice that was not the last, the write finished that slice and
sealed the tape right after it, and the rest of what it planned is written
first on the next tape ("A unit on more than one cartridge"). File 0, the
front index and the envelopes were written before that, so they describe the
plan (they state `may_end_early = true`, on every tape), and only the seal
marker, the last file on the tape, says where the tape really ended. Its
`ended_after` is the last tape file before the seal marker, and its
`file_count` is `ended_after` plus 2. Every tape file the front index lists
after `ended_after` is not on this tape. A tape that sealed where the plan
said states it too: there it is the file before the planned last.

To read it by hand, with no key (`/dev/nst0` is an example: "Which
device?"):

    mt -f /dev/nst0 setblk 524288
    mt -f /dev/nst0 eod          # GNU mt: eom
    mt -f /dev/nst0 bsfm 2
    dd if=/dev/nst0 bs=512k | tr -d '\0' | grep -E '^(file_count|ended_after) ='

A unit whose slices run past `ended_after` (the `tape_position` of a slice
in MANIFEST.toml, or of the last of its "Tape files" in RECOVERY.md, is
after it) is on this tape in part only: its slices up to `ended_after` are
there and hash correctly, and the rest are on other tapes by design. The
index names those tapes: `./RESTORE.sh --where UNIT --key YOUR_KEY.age.key`
("The rest of the box"). Every unit wholly at or before `ended_after` is on
this tape whole and restores as usual.

RESTORE.sh does all of this by itself, reading the seal marker before any
slice (one wind to the end of the tape and a rewind). `--info` says where
the tape ended, and `--info` and `--verify` print one `NOT ON TAPE` line for
the planned files after `ended_after`, naming the first and the last of
them, which is not a failure:

  NOT ON TAPE  files 12 to 15  (planned; the tape ended after file 11, and its seal marker is file 12)

A slice after `ended_after` is never read from this tape and never reported
as damage. `--list` and `--find-envelope` say which units this tape holds
only part of ("A unit on more than one cartridge").

## Did the restore get everything?

These checks are for ONE restore, of one unit or one tape's worth, from one
tape. They say nothing about the other tapes and units of a box: a box is
complete only when the procedure in "The rest of the box" has been followed
to its end.

A restore went right only if all of these hold:

1. RESTORE.sh ended with `RESTORE COMPLETE` and exit status 0 (`echo $?`
   straight after it). It prints that only when no file was left out and
   every restored file passed its check. `RESTORE INCOMPLETE` (exit status
   1) means some files were left out or failed the check: they are listed,
   and "If a slice or a file is damaged" says how to fill them.
   `RESTORE STOPPED` is a restore you stopped at the prompt. Any other
   ending: read its message, and do not trust what was written.
2. For each unit, its "File check" line says all of its regular files match
   the sha256 in the unit's file table ("File check: all N regular file(s)
   match..."). That check is the verdict, and tar's exit status is not: a
   tar can exit 0 with files missing (BusyBox's does), so RESTORE.sh notes
   what tar said and decides by the hashes. A restore with `--no-file-check`
   has no verdict.
3. The count matches the file table. The regular files a unit's table lists
   are the lines of `--list` whose kind is `f`; that number should equal the
   N of "File check: all N". For a few files, compare sizes and a checksum
   with any record of your own.
4. After `--all`, every unit your key opens was restored: "RESTORE COMPLETE:
   N unit(s) restored" names N, and every unit that `./RESTORE.sh
   --find-envelope --key YOUR_KEY.age.key` lists (once for each unit and
   version, with the envelopes that list it named on its line: a key that
   opens both the operator's and a tenant's envelope sees the same unit in
   both) must be a directory of the `--all` destination (a tape of one unit puts that unit's files in the
   destination itself, and that is the unit's directory). Restore each one
   that is not in a run of its own, `./RESTORE.sh --restore --key
   YOUR_KEY.age.key --unit UNIT --to /destination/UNIT`, and check that run
   the same way.

With `--path`, only the files you asked for are restored, plus the folders
above them as tar makes them, and the check covers the files you named.
After a repair, the check covers the files it was asked for and the list is
rewritten with what is still damaged ("If a slice or a file is damaged").

## If a slice or a file is damaged

RESTORE.sh checks every slice four ways before tar sees it: its hash
against the front index, that age decrypts it, its length after decrypting
(and decompressing) against the manifest, and the hash of its plain cut
against the manifest. It never fills a lost slice with zeros. A slice that
fails any check, or is not on this tape, is LEFT OUT: the files that lie in
it are not restored but written to `.tapectl-damaged-files.txt`, one path a
line, in the unit's directory (`--to` itself when the restore had one unit,
`--to/UNIT` when it had several), and RESTORE.sh restores every other file.
A path is written as the index writes one: a backslash as `\\`, a TAB as
`\t`, a newline as `\n`, a carriage return as `\r`. The first line records
the unit, its uuid and its version. A long file whose run has a bad slice is
left out whole, and the part tar began is removed. After the restore each
regular file is hashed against the file table: a file that does not match,
or is missing, goes on the same list, however tar ended.

**The one repair.** Load another tape that holds the same unit at the same
version, and its slices of the files on the list (a tape that holds only a
range of the unit gives the files whose slices it holds: "A unit on more
than one cartridge"), and give the list to `--paths-from`, with the `--to`
directory of that unit:

    ./RESTORE.sh --restore --key YOUR_KEY.age.key --paths-from /destination/UNIT/.tapectl-damaged-files.txt --to /destination/UNIT

It reads only the slices that hold the listed files, restores exactly them,
over their damaged copies, and nothing else in the directory, and checks each
against the file table. The list is then rewritten with whatever is still
damaged (from yet another copy, run it again) and removed when nothing is. A
hard-linked name whose first name is on the list is made from its first name
once that is restored. Only a tape whose envelope lists the same unit, with
the same uuid, at the same version is used; any other is refused by name,
because another version can differ in the very files that are damaged. A
slice is taken from another tape by its hash, never by a label.

**No other copy.** If a slice fails and no other tape has a copy of it, the
slices that passed are intact, and the files that lie wholly inside them
are restored; the files that touch a failed slice are lost.

By hand, without RESTORE.sh, do the steps of "Manual Recovery Steps" for
the unit, but leave the failed slice out: do not decrypt it, do not put an
empty file in its place, and remove its `slice.N.tar`. If the failed slice
is part of a long file's run (it, or a slice just before it, states
`continues = true`: the slices of one run), leave out the WHOLE run, the
slice that begins the file and every slice after it that continues it, so
that tar is never given the head of a file without its tail. The slices
that are left, joined in order, are a tar stream of whole files, and tar
restores every one of them; the file table's `slice` column says which
files were in the slices you left out.

**A damaged piece inside a slice.** `age` stops at the first damaged 64 KiB
piece of a slice, so RESTORE.sh, and the manual steps, lose the whole
slice. tapectl's own restore reads around a damaged piece and loses only
the files that touch it (in a compressed slice, the files from the damaged
piece to the end of that slice): "tapectl itself", for the operator or
escrow key.

## tapectl itself

Look in the Heir Kit first. The kit holds a copy of the program that made
it, in the folder with `catalog.db.age`: a file named `tapectl-` and its
version, `tapectl-X.Y.Z` below (X.Y.Z such as 2.0.0). The cover sheet
names it with its size and sha256 (check the file against the sheet with
`sha256sum`) and says it is for Linux on one machine type (`uname -m` says
yours). It needs no building: run it as `./tapectl-X.Y.Z` wherever this
section says `tapectl`, after `./tapectl-X.Y.Z --version` has shown it
starts. Copy it to a Linux disk first and run `chmod +x` on it: from a USB
stick formatted FAT or exFAT, or mounted `noexec`, it answers "Permission
denied". Its release must be File 0's `tapectl_version` or newer (File 0
prints it: "Automated Recovery (recommended)"). With no kit, a copy that is
too old, or one that will not run on your machine, read on.

tapectl is open-source software, and its source is at
https://github.com/mikmorg/tapectl: one place to look, and a web address
may move over decades, so search for the name if it is gone. File 0's
`tapectl_version` names the release that wrote this tape; the repository
tags each release `v` followed by that number (such as `v` and 2.0.0); use that
release or a newer one. It is built from source with the Rust toolchain
the repository pins in `rust-toolchain.toml` (`rustup`, from rustup.rs,
installs it), a network connection to download its libraries, and `mt-st`
and `sg3-utils` installed ("Tools Required"); the repository's own
documentation (`docs/keys-and-recovery.md`, runbook C) gives the commands.

**Which key it needs.** tapectl's own restore starts from `tapectl catalog
rebuild --from-volume`, which reads the tape's operator envelope: it needs
the OPERATOR key or the ESCROW secret. With a tenant's key it is refused ("cannot
open the operator envelope"), and tapectl's own restore cannot run without the
rebuilt catalogue. A holder of only a tenant's key has RESTORE.sh, the
manual steps and the one repair from another copy of the tape, and
nothing else; they do not read around a damaged piece of a slice, and
nothing here pretends they do.

The route with the operator or escrow key, in order. ESCROW_PUBLIC is the
`age1...` line of the cover sheet, the ESCROW key's public half
(`age-keygen -y` of the ESCROW secret prints it, and of any other key prints
a different one, which `init` would register as the escrow identity, so use
it only on the escrow secret). OPERATOR_NAME is the operator's name where you
know it (the Heir Kit's catalogue has it); otherwise give any name, your
own say, because the rebuild takes the tenants' names from the tape. Use
your own device ("Which device?"):

    tapectl init --operator OPERATOR_NAME --escrow-public-key ESCROW_PUBLIC
    tapectl catalog rebuild --from-volume --device /dev/nst0 --key YOUR_KEY.age.key
    tapectl restore unit --unit UNIT --device /dev/nst0 --to /destination/UNIT --key YOUR_KEY.age.key

`init` makes a new home for the recovery and registers the escrow PUBLIC
key, which decrypts nothing; run it once. (Run as an ordinary user, `init`
without `--operator` names the operator after your login; run as root,
under `sudo`, it refuses without one.) `catalog rebuild` reads the loaded
tape and puts what it says into tapectl's own catalogue; run it once for
each tape. `restore unit` restores the newest version of a unit
(`--version N` for another) into an empty directory, asks for the cartridges
of a unit on more than one, shows the id map, and reads around a damaged
64 KiB piece of a slice, listing the files it lost.

`--key` names a key file for that one command and nothing more: tapectl
does not copy it into its home or register it, and the escrow secret is
never stored on the machine. Give it on each command; it may be repeated.
The alternative with the operator's key file: adopt it at `init` with
`--operator-key OPERATOR.age.key` beside `--escrow-public-key`, and leave
`--key` off the restore, which then reads the key from its home's `keys/`;
`init` refuses the escrow secret as `--operator-key` (that secret is kept on
paper, apart from every machine). Give `--home DIR` right after `tapectl` (or
set `TAPECTL_HOME`) to name the recovery's home; under `sudo`, which drops
the variable, give `--home DIR`.

## Compressed units

A unit may be stored compressed, whatever else it is. How to tell: the
unit's block in MANIFEST.toml states `compression = "gzip"` (or `"xz"` or
`"zstd"`) and a unit stored uncompressed has no `compression` line;
`--find-envelope` and `--list` print "compressed with" and the name; the
front index, File 0 and the seal say nothing about it. (With the operator or
escrow key, a manual look at MANIFEST.toml takes the envelope of the tenant
that owns the unit: step 5 of "Manual Recovery Steps".)

Each slice is compressed on its own, after the cut and before it is
encrypted, so a lost slice loses only the files in it, and a slice
decrypted and decompressed is exactly the plain cut an uncompressed unit
would have in that place. The tool that reads each algorithm, and the
package that has it: `gzip` (package gzip), `xz` (package xz-utils), `zstd`
(package zstd). Only the one the unit names is needed. The tool's exit
status is never the verdict (a BusyBox `unxz` exits 0 after writing wrong
bytes): the plain cut's length is the slice's `size_bytes` in MANIFEST.toml
and its hash the slice's `sha256_plain`, and the last line below checks
both. By hand, the slice line of "Manual Recovery Steps" (step 7) becomes,
with DECOMPRESS `gzip -dc`, `xz -dc` or `zstd -dc`:

    dd if=/dev/nst0 bs=512k of=slice.NUMBER.age
    truncate -s ENCRYPTED_BYTES slice.NUMBER.age
    echo "SHA256_ENCRYPTED  slice.NUMBER.age" | sha256sum -c - && age -d -i YOUR_KEY.age.key slice.NUMBER.age | DECOMPRESS > slice.NUMBER.tar && rm slice.NUMBER.age
    test $(wc -c < slice.NUMBER.tar) -eq SIZE_BYTES && echo "SHA256_PLAIN  slice.NUMBER.tar" | sha256sum -c -

and everything after it (the join, the check against the file table) is
the same. The room the restore needs is the unit's "bytes of stream", not
its slices' size ("Disk space").

## Manual Recovery Steps

If RESTORE.sh is not available, follow these steps with `mt`, `dd`,
`truncate`, `age`, `tar`, `sha256sum` and the standard tools. They use the
non-rewinding device ("Which device?"; `/dev/nst0` is an example). Each
`dd` that finishes leaves the head at the start of the next file, so `mt
fsf N` moves forward N files from where you are, and `mt rewind` then `mt
fsf N` reaches File N from anywhere. A `dd` that stops with an error
("Input/output error") leaves the head INSIDE that file, not at the start
of the next: after any failed read, `mt rewind` and `mt fsf` to the file
you want next, whatever the counting below says. Read the tape forward
where you can; rewinding is slow.

Deleting the padding zeros with `tr -d '\0'` is for the plain text files
only (Files 0 to 3 and the last). An envelope or a slice is trimmed to its
exact size with `truncate` instead, never with `tr`.

1. Set 512 KiB fixed block mode (mt-st; see above if `mt` cannot):

       mt -f /dev/nst0 setblk 524288

2. Read File 0, the ID thunk. It names the volume, says the map is File 3,
   and says which file is the seal marker (`seal_marker`, the last file):

       mt -f /dev/nst0 rewind
       dd if=/dev/nst0 bs=512k | tr -d '\0' > file0.txt

3. Read File 3, the front index (the head is at File 1 now; skip Files 1
   and 2), and note its hash:

       mt -f /dev/nst0 fsf 2
       dd if=/dev/nst0 bs=512k | tr -d '\0' > front_index.toml
       sha256sum front_index.toml

   Each `[[files]]` entry gives a file's `position`, `type`, exact
   `size_bytes` (without block padding) and `sha256_encrypted`.
4. Read the seal marker, the last file. This tape has {total_files} files,
   numbered 0 to {last_file}, so the last is File {last_file}, unless the
   tape ended early (below):

       mt -f /dev/nst0 eod
       mt -f /dev/nst0 bsfm 2
       dd if=/dev/nst0 bs=512k | tr -d '\0' > seal.toml

   `eod` (GNU mt calls it `eom`) moves to the end of the recorded data,
   which is just past the last file's closing filemark. `bsfm 2` backs over
   that filemark and the one before it and then steps forward over that
   one, which leaves the head at the START of the last file. (Without
   `eod`, `mt -f /dev/nst0 rewind` then `mt -f /dev/nst0 fsf {last_file}`
   reaches the same place on this tape, if it did not end early.) Check that
   `front_index_sha256` in seal.toml equals the sha256sum printed in step 3,
   and that its `file_count` is the number of entries in the front index. On
   a tape that ended early, `file_count` is smaller, the seal marker's
   `ended_after` is the last file before it, and every entry of the front
   index after that is not on the tape: "Where the tape ended". If the seal
   marker is missing, or the two hashes disagree, treat the tape as unsealed
   or damaged: see "If the seal is missing or not trusted". An unsealed tape
   may lack its last data slices (the front index still lists them), and
   every file that is there can be checked against the index.
5. Find and open an envelope. Its entries in the front index have type
   `tenant_envelope` (one for each tenant, the first envelopes on the tape)
   or `operator_envelope` (and a `_backup` for each). Which one to open
   depends on your key. A tenant's key opens that tenant's
   `tenant_envelope` and no other, so the first that decrypts is yours. The
   operator key and the escrow key open EVERY envelope, and the tenant
   envelopes come first: the first that decrypts is then one tenant's, and
   its MANIFEST.toml lists that tenant's units and no one else's, with their
   file tables. The `operator_envelope` lists every unit, but holds the file
   tables only as rows of its `catalog.db`, so open each `tenant_envelope`
   and restore the units of each. For each envelope you try, with POSITION
   (the entry's `position`) and SIZE (its `size_bytes`):

       mt -f /dev/nst0 rewind
       mt -f /dev/nst0 fsf POSITION
       dd if=/dev/nst0 bs=512k of=envelope.raw
       truncate -s SIZE envelope.raw
       sha256sum envelope.raw
       age -d -i YOUR_KEY.age.key envelope.raw > envelope.tar

   The sha256sum must equal that entry's `sha256_encrypted`. age says "no
   identity matched" for an envelope your key was not made for: go on to the
   next. With a tenant's key the first that decrypts is yours. If it is
   damaged or will not read (age says anything else after a key matched), use
   its backup, the `_backup` entry with the same `sha256_encrypted`. Unpack
   it:

       mkdir env && tar xf envelope.tar -C env
       ls env env/tables env/index

   (If neither the envelope nor its backup reads: "If All Else Fails", at
   its end.)

6. `env/MANIFEST.toml` describes the units of the envelope you opened: a
   `[[units]]` block for each (`name`, `uuid`, `snapshot_version`,
   `stream_bytes`, `file_table` and `id_map`, and `compression` when the unit
   was compressed), and under it one `[[units.slices]]` block for each slice,
   with its `number`, `tape_position`, `size_bytes` (the length of the slice's
   PLAIN cut), `encrypted_bytes` (its exact size on the tape),
   `sha256_plain`, `sha256_encrypted` and, for a slice that continues a long
   file, `continues = true`. A unit stored in several versions has a
   `[[units]]` block, with its own slices, for each version. This line lists
   the units and versions:

       grep -E '^(name|uuid|snapshot_version|stream_bytes|file_table|compression) =' env/MANIFEST.toml

   `env/RECOVERY.md` writes the commands of step 7 out for each unit.
   `env/tables/` holds each unit's file table and id map, and `env/index/`
   the index of "The rest of the box".
7. Restore ONE version of ONE unit at a time, each in a directory of its
   own: the slices of every unit are named `slice.N.tar`, so two units in
   one directory would be mixed. If the manifest lists the unit more than
   once, take the block with the highest `snapshot_version`, the newest,
   unless you want an older one. On a tape that ended early (step 4), check
   first that the `tape_position` of the slices you read is at or before the
   seal marker's `ended_after`; a slice with no `tape_position` is on
   another tape ("A unit on more than one cartridge"): read each tape's
   slices of it this way, matching each by its `sha256_encrypted`, all into
   the one directory. Put the tape at the first slice, once. `fsf N` moves
   forward N files from where the head is, so from the start of the tape it
   reaches File N: the `tape_position` of the first slice, from the
   manifest (FIRST_TAPE_POSITION below).

       mt -f /dev/nst0 rewind
       mt -f /dev/nst0 fsf FIRST_TAPE_POSITION

   Then, for each slice of the unit in order of its number, with
   ENCRYPTED_BYTES, SHA256_ENCRYPTED, SHA256_PLAIN, SIZE_BYTES and NUMBER from
   the manifest (each read that finishes leaves the tape at the start of the
   next file, so the next slice needs no spacing unless there is a gap or a
   read failed). For a unit that states `compression`, use the slice line of
   "Compressed units" instead:

       dd if=/dev/nst0 bs=512k of=slice.NUMBER.age
       truncate -s ENCRYPTED_BYTES slice.NUMBER.age
       echo "SHA256_ENCRYPTED  slice.NUMBER.age" | sha256sum -c - && age -d -i YOUR_KEY.age.key slice.NUMBER.age > slice.NUMBER.tar && rm slice.NUMBER.age
       test $(wc -c < slice.NUMBER.tar) -eq SIZE_BYTES && echo "SHA256_PLAIN  slice.NUMBER.tar" | sha256sum -c -

   The third line is one command on purpose: each `&&` stops it at the first
   step that fails, so a slice whose hash is wrong is never decrypted, and
   its encrypted copy is deleted only after it decrypted. If it stops, the
   slice is damaged ("If a slice or a file is damaged"); if `age` was what
   failed, delete the partial `slice.NUMBER.tar` before going on. The last
   line checks the decrypted slice: its length and its hash.

   If a gap lies between two of the unit's slices (another unit's slices,
   say), space over it first. `fsf` takes the number of files to skip, not a
   file number: after reading the slice in File P the head is at the start of
   File P+1, so to reach the slice in File Q, run

       mt -f /dev/nst0 fsf COUNT           # COUNT = Q - P - 1

   (from File 6 to File 9 that is `fsf 2`). Rewinding and `fsf Q`, as above,
   reaches it too and is slower. If a `dd` stopped with an error instead
   ("Input/output error"), the head is inside that slice's file and the
   count above is wrong: `mt rewind`, then `fsf` to the `tape_position` of
   the slice you want next (or of the same slice, to try it again), and
   read on from there. A slice that fails its `sha256sum -c` is
   damaged: do not decrypt it, but read the same slice from another copy of
   the tape. The copy lists it in its own front index under the same
   `sha256_encrypted`, at whatever File that tape puts it, so find it by
   that hash and by nothing else. With no other copy, leave that slice out
   and go on: "If a slice or a file is damaged" says what the rest gives.

   Block padding must be trimmed (`truncate`) before the hash is checked
   and before age sees the file: the padding zeros make age reject the
   ciphertext. Work on a disk with room for all of the unit's decrypted
   slices at once (the unit's "bytes of stream") and for step 8's restored
   files beside them (the same again), on a real disk and not in /tmp, which
   is a small RAM disk on many systems ("Disk space"). RESTORE.sh keeps only
   the envelopes and one slice there.
8. Join the slices in order into ONE tar run, into an empty directory of
   its own (each slice begins where a file begins, and the slices joined are
   the tar stream, so tar needs nothing more). `LC_ALL=C` keeps file names as
   they are, and a `TAR_OPTIONS` in your environment must be empty. With GNU
   tar the flags are these (numbers, not names, for owner and group; with
   another tar, drop the flags it refuses: "What a restore on another
   computer does"):

       mkdir -p /destination
       cat $(ls slice.*.tar | sort -t. -k2,2n) | LC_ALL=C tar -x --numeric-owner -p --xattrs --acls -f - -C /destination

   With several units, give each its own directory (`/destination/UNIT`);
   two units of one name (different uuids) take `/destination/UNIT-` and the
   first 8 characters of the uuid, each a new directory, or their files
   would be mixed without a word. tar keeps nothing of an earlier run and
   restores over what is there, so restore into an empty directory; never
   restore a newer version over an older one. Delete the unit's
   `slice.N.tar` files once it is restored and checked.
9. Check the result against the unit's file table (the check is the
   sha256 of every file; tar's exit status is not a check). NAME is the
   table's name without `.tsv`, as step 5's `ls` shows it (`env/tables/NAME.tsv`,
   TABLE below). It prints nothing when every file passes, and `FAILED` for
   each that does not. A file whose name holds a backslash, a TAB, a newline
   or a carriage return is written escaped in the table and is left out of this
   line: compare it by hand against its row (RESTORE.sh checks it).

       awk -F'\t' '!/^#/ && ($1 == "f" || $1 == "h") && index($7, "\\") == 0 {{ print $6 "  ./" $7 }}' env/tables/TABLE.tsv | (cd /destination && sha256sum -c --quiet)

   Who the user and group numbers were, for the unit (the id map; it is
   shown and never used):

       cat env/tables/TABLE.ids.tsv

**One file, not the whole unit.** Find its row in the unit's file table (the
columns are `kind`, `slice`, `start`, `end`, `size`, `sha256` and `path`, the
path as the table writes it, with `./` left off):

    awk -F'\t' '$7 == "SOME_PATH"' env/tables/TABLE.tsv

then the stream offset where its slice begins, which is the `start` of the
first row whose `slice` is that slice's number (SLICE is the row's `slice`):

    awk -F'\t' -v s=SLICE '!/^#/ && $2 == s {{ print $3; exit }}' env/tables/TABLE.tsv     # SLICE_START

Read that slice into `slice.SLICE.tar` as in step 7, then cut the file's bytes
out of it and give them to tar (START and END are the row's, and SLICE_START
is the second command's answer). Each cut is its own pipeline:

    tail -c +$((START - SLICE_START + 1)) slice.SLICE.tar | head -c $((END - START)) | LC_ALL=C tar -x --numeric-owner -p -f - -C /destination

A file that runs on past the end of its slice (a long file: its `end` is
beyond the end of its slice, which is its `start` plus the `size_bytes` of the
slices from there) is a run of slices: read each slice of the run, and cut from
them joined, FIRST_SLICE_TO_LAST being their names in order:

    cat FIRST_SLICE_TO_LAST | tail -c +$((START - SLICE_START + 1)) | head -c $((END - START)) | LC_ALL=C tar -x --numeric-owner -p -f - -C /destination

A hard link (kind `h`) needs the file it is a name for restored first (its
row's last column names it): restore that file the same way, then the link.

## Important: Block Padding

This tape uses 512KB (524288 byte) fixed block mode. Every file is
padded with zeros to the next block boundary. Encrypted files (data
slices, envelopes) MUST be trimmed to their exact byte size before
decryption — the padding zeros will cause age to reject the ciphertext
("failed to decrypt and authenticate payload chunk" is what age prints for
an untrimmed file). Exact sizes are in File 3, the front index (`size_bytes`
field), and in MANIFEST.toml's `encrypted_bytes` for a slice. A slice's
`size_bytes` in MANIFEST.toml is something else: the length of its PLAIN cut,
after it is decrypted (and decompressed). Never trim an encrypted slice to
that.

## What the unencrypted parts of this tape reveal

Files 0-3 and the seal marker (the last file on the tape) are
plaintext; so is the tape's block structure. Anyone holding the
physical tape can therefore learn a limited set of *structural* facts
— but no content:

- the volume label, creation date, and tapectl version;
- how many tenants share this tape (the number of tenant envelopes,
  each written twice);
- how many data slices there are, and the exact on-tape byte size and
  ciphertext hash of every file (needed so an heir can navigate and
  trim block padding without a key).

They CANNOT learn filenames, unit or tenant names, plaintext-content
checksums, ownership (no user or group name or number is in any plain
file), or any file content — all of that lives only inside the
age-encrypted envelopes and slices. That includes the index of other tapes
and of every path, the file tables and the id maps, which are inside the
envelopes.

**Accepted size disclosure.** Encryption overhead is deterministic and
tapectl archives without compression unless its operator chose it, so an
on-tape size approximates the content size it encloses. A slice holds whole
files and a file larger than a slice is a run of slices of its own, so a
slice's size follows the files in it: a file alone in its slice, or in a run
of its own, shows its size, and if your units are archived one folder per
slice, the size of each data slice effectively discloses that unit's
approximate content size and reveals unit boundaries: someone holding this
tape could in principle correlate slice sizes against publicly known media
sizes. This is a known, accepted trade-off: sizes and ciphertext hashes are
structural facts needed for keyless navigation and integrity checking, and
neither is tied to any tenant or unit name in plaintext.

## If All Else Fails

Normal recovery reads the front index (File 3) to know exactly where
everything is and how big it is. There are two more tries before
giving up — a three-rung degradation ladder:

1. **Front index (File 3)** — the normal path: read File 3 directly
   (`mt -f /dev/nst0 rewind`, then `mt -f /dev/nst0 fsf 3`, then
   `dd if=/dev/nst0 bs=512k | tr -d '\0'`); it lists every file's
   position, type, on-tape size, and ciphertext hash.
2. **Seal-marker embedded copy** — if File 3 itself is damaged or will
   not parse, the LAST file on the tape (the seal marker) carries a
   full embedded copy of the same index, in the same format. Read the
   last file (step 4 of "Manual Recovery Steps": `eod`, then `bsfm 2`)
   and parse its `[[files]]` entries exactly as you would File 3's.
3. **Filemark walk + zero-strip** — if BOTH the front index and the
   seal marker are lost or unreadable, recovery is still possible; see
   below.

RESTORE.sh climbs rungs 1 and 2 by itself: if File 3 will not parse or
fails its own consistency check it warns and uses the seal marker's copy
(RUNG-2). If both are lost it prints this procedure and stops, because
rung 3 cannot be automated.

### Zero-strip procedure (rung 3)

Envelopes are among the first files after the front index. Block
padding can be defeated without knowing the exact size. Never do it with
`tr -d '\0'`, which is for plain text only: it deletes the zero bytes that
are part of the ciphertext.

Which files to try. The envelopes come first, right after File 3 and so
from File 4: one for each tenant, one for the operator, and a backup copy of
each, which is 2 x (tenants + 1) files, at most about eight on the usual
tape of one to three tenants. The data slices follow them. With no map
there are two ways to tell them apart. By size: an envelope is usually
small (a few KiB to some tens of MiB; one for units of millions of files,
which carries a list of every path, can be hundreds of MiB), a slice is a
piece of a unit's archive (often hundreds of MiB to GiB; a unit's last
slice can be small), so size is a hint, not proof. By trying your key and
unpacking what comes out: your key opens your slices as well as your
envelope, so a file that decrypts is not yet an envelope, but an
envelope's decrypted bytes are a tar archive that lists MANIFEST.toml, and a slice's are not. With a
tenant's key, stop at the first file that decrypts to a tar archive listing
MANIFEST.toml: that is your envelope. With the operator or escrow key do
not stop there: the tenants' envelopes come first and each lists only one
tenant's units, so keep going to the operator envelope, the one whose tar
also lists `PLAN.toml` (the operator envelope's plan of the tape: its
units, each with its uuid, slice count and size; a tenant's envelope never
has one), and whose MANIFEST.toml lists every unit on the tape; or restore
the units of every envelope that opens. If the files have grown large
(slices) and none opened, your key is not a recipient of any envelope on this tape: try the
other keys you hold, since an envelope is sealed to the keys active when
the tape was written, and the operator and escrow keys open every one.

1. Rewind and space to File 4 (`mt -f /dev/nst0 rewind`, then
   `mt -f /dev/nst0 fsf 4`), then read candidate files one by one with
   `dd if=/dev/nst0 bs=512k of=candidate.raw`. Each read leaves the tape
   at the start of the next file, so read again without spacing; an
   extra `fsf 1` between reads would skip a file. After a `dd` that failed
   with an error the head is inside that file: `mt rewind` and `fsf` to the
   next candidate instead.
2. Strip only the TRAILING zero bytes from the file you read — the
   block padding. Never delete every zero byte (`tr -d '\0'`): zero
   bytes inside the ciphertext are part of it, and age will reject what
   is left. These four lines keep everything up to the last non-zero
   byte:

       size=$(stat -c %s candidate.raw)
       tail_len=$(tail -c 524288 candidate.raw | wc -c)
       keep=$(tail -c 524288 candidate.raw | od -An -v -tu1 | awk '{{ for (i = 1; i <= NF; i++) {{ n++; if ($i != 0) last = n }} }} END {{ print last + 0 }}')
       head -c $((size - tail_len + keep)) candidate.raw > candidate.stripped

3. Try `age -d -i YOUR_KEY.age.key candidate.stripped > candidate.dec`.
   "no identity matched" (`rage`: "No matching keys found") means the file is
   not sealed to your key: try the next one. Any other error: real ciphertext can end in a zero byte, so
   re-append a single zero byte and retry, up to four times:
   `printf '\0' >> candidate.stripped`, then the `age -d` line again. The
   retries add up, they do not start over: after the first failure one zero
   byte is back, after the second a second one is added (two are back),
   then three, then four, and no more. (Step 2 cannot
   tell the padding from a zero that belongs to the ciphertext, and so
   removes both. About one ciphertext in 256 ends in a zero byte, one in
   65 536 in two, and so on; four retries leave a chance of about one in a
   trillion that more zeros were lost, so an error after four means the
   wrong key or real damage, not the stripping.)
4. Run `tar tf candidate.dec`. A list that begins with MANIFEST.toml is an
   envelope; anything else (a list of a unit's files, or an error that it is
   not a tar archive) is a data slice.
   Once the envelope you want decrypts, unpack it (`mkdir env && tar xf
   candidate.dec -C env`): its MANIFEST.toml gives the exact tape
   position and `encrypted_bytes` for every slice belonging to the units it
   lists. Apply the same read/trim/decrypt procedure to each slice using
   those exact sizes, then join the slices and give them to tar as step 8 of
   "Manual Recovery Steps" does.

**When no envelope reads.** If every copy of every envelope (each one and
its backup) is damaged, the data slices may be intact, but nothing else on
the tape says which of them belong to which unit or in what order: that map
is the envelope's MANIFEST.toml, and this guide gives no way to assemble the
slices without it. Something that holds the same map can supply it: another
copy of this tape (its envelopes carry it), or the Heir Kit's catalogue
("The Heir Kit's catalogue"), whose tables `stage_slices` and
`write_positions` record each slice's number and its tape file, if the kit
was made after this tape was written. With a map, read the slices as step 7
of "Manual Recovery Steps" does.


## Total files on this tape: {total_files}

The files of THIS tape are numbered 0 to {last_file}. File {last_file}, the
last, is the seal marker. Another tape of the box has its own count: File 0
of that tape gives it. That is the plan: a tape that ended early holds
fewer, and its seal marker says where it ended ("Where the tape ended").

"#
    )
}

/// this function). The generator itself is unchanged; only its caller and
/// packaging change in T8.
pub fn generate_planning_header(
    label: &str,
    units: &[(String, String, i64, i64)], // (unit_name, uuid, num_slices, total_bytes)
) -> String {
    let now = chrono::Utc::now().to_rfc3339();
    let mut s = format!(
        r#"[planning]
status = "planned"
volume = "{label}"
planned_at = "{now}"
"#
    );

    // One `[[units]]` header per unit — emitting it once before the loop put
    // every unit's keys into the same table, producing duplicate-key invalid
    // TOML for any multi-unit volume (T14).
    for (name, uuid, slices, bytes) in units {
        s.push_str(&format!(
            r#"
[[units]]
name = "{name}"
uuid = "{uuid}"
num_slices = {slices}
total_bytes = {bytes}
"#
        ));
    }
    s
}

/// One entry in the plaintext **front index** (File 3, layout v2). Navigation is
/// total — every file has `position` and `type` — but `size_bytes` is `None` for
/// the front index's own entry (its length is self-referential), and
/// `sha256_encrypted` is `None` for both the front index itself (self-reference)
/// and the seal marker (not yet written when File 3 is generated). See
/// `docs/design/volume-format-v3.md` §4-4 and ADR-0007.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FrontIndexFile {
    pub position: i32,
    pub type_label: &'static str,
    pub size_bytes: Option<u64>,
    /// sha256 of the file's on-tape bytes (ciphertext for encrypted zones, the
    /// plaintext bytes for Files 0-2). Hex; `None` where excluded (above).
    pub sha256_encrypted: Option<String>,
}

/// Generate the plaintext **front index** (File 3, layout v2). It maps every
/// tape position to its type and on-tape byte size, and carries the sha256 of
/// every file's on-tape (encrypted) bytes except its own and the seal marker's —
/// giving an heir keyless navigation and keyless byte-integrity with only `dd`
/// and `sha256sum` (ADR-0007). It carries NO content metadata: no filenames, no
/// plaintext-content hashes, no tenant/unit names — the isolation invariant of
/// `volume-format-v3.md` §2. The ciphertext hashes are safe in plaintext because
/// they hash pseudorandom age output and are non-attributable (D4).
///
/// File 3 is the PLAN (§3): it is written before any slice, so from 1.6.0 its
/// `[index]` states `may_end_early = true` on every tape, with the header
/// paragraph §3 fixes word for word, and the seal's `ended_after` says where
/// the tape really ended (ADR-0012 item 111). It decides nothing for a reader.
pub fn generate_front_index(label: &str, files: &[FrontIndexFile]) -> String {
    let t_label = crate::volume::format::toml_string(label);
    let mut s = format!(
        r#"================================================================
                    TAPECTL FRONT INDEX
================================================================

Volume: {label}
This file maps every tape position to its type, on-tape byte size, and
the sha256 of its on-tape (encrypted) bytes. It contains NO content
metadata: no filenames, no plaintext checksums, no tenant or unit names.
The trailing seal marker binds this index by hash; see the system guide.

This tape may end early. The list below is the plan: every file the
write meant to put on the tape. The seal marker, the LAST file on the
tape, says where the tape really ended (its ended_after). A file listed
after that position is not on this tape.

================================================================
              MACHINE-READABLE DATA (TOML)
================================================================

[index]
volume = {t_label}
layout_version = 3
may_end_early = true
"#
    );

    append_files_entries(&mut s, files);

    s
}

/// Emit `[[files]]` entries in the line-oriented grammar shared by the front
/// index and the seal marker's embedded copy (one `key = value` per line — the
/// shell-parseable contract RESTORE.sh depends on). One emitter so the two
/// cannot drift.
fn append_files_entries(s: &mut String, files: &[FrontIndexFile]) {
    for f in files {
        s.push_str("\n[[files]]\n");
        s.push_str(&format!("position = {}\n", f.position));
        s.push_str(&format!("type = \"{}\"\n", f.type_label));
        if let Some(sz) = f.size_bytes {
            s.push_str(&format!("size_bytes = {sz}\n"));
        }
        if let Some(h) = &f.sha256_encrypted {
            s.push_str(&format!("sha256_encrypted = \"{h}\"\n"));
        }
    }
}

/// Generate the plaintext **full seal marker** (the last file, layout v2, at
/// the planned position M). Its presence is the completeness assertion —
/// "every file before me is present" — and its `front_index_sha256` binds the
/// front index, making the seal marker the unhashed root of the keyless
/// integrity chain (seal marker → front index → every content file; ADR-0007,
/// `volume-format-v3.md` §5). Its absence means the tape is legitimately
/// unsealed (interrupted or EOT-aborted).
///
/// It is §5's **full seal**: `requires` as File 0 states it (empty on every
/// Layout 3 tape, §1.2) and `ended_after = file_count - 2`, the last data
/// slice. `build` sizes the seal placeholder
/// with this function, and a short seal ([`generate_short_seal_marker`]) is
/// never longer than it.
///
/// `files` is the **embedded full copy of the front index** (ratified
/// 2026-07-22), entries 0 to M: two-ended redundancy — front-of-tape damage
/// recovers the map from the tail; tail damage reads as unsealed but stays
/// navigable from the front. By seal time File 3's bytes are known, so the
/// caller fills in File 3's own `size_bytes` + `sha256_encrypted` (more
/// complete than File 3 itself); only the seal marker's own entry stays
/// hash-less (self-reference). The copy is not hash-protected by anything on
/// the tape — readers validate its per-file claims by hashing the files they
/// describe (`volume-format-v3.md` §5).
pub fn generate_seal_marker(
    label: &str,
    file_count: i32,
    front_index_sha256: &str,
    files: &[FrontIndexFile],
) -> String {
    generate_seal_marker_requiring(label, file_count, front_index_sha256, files, REQUIRES)
}

/// The full seal stating `features` in `requires`, as File 0 of the same tape
/// states them (`volume-format-v3.md` §1.2: the seal repeats File 0's
/// `requires`).
/// `build` sizes the placeholder with it; the seal written at execute time
/// must state the same features, or it is not the placeholder's length.
pub fn generate_seal_marker_requiring(
    label: &str,
    file_count: i32,
    front_index_sha256: &str,
    files: &[FrontIndexFile],
    features: &[&str],
) -> String {
    generate_seal_marker_requiring_at(
        label,
        file_count,
        front_index_sha256,
        files,
        features,
        &seal_time(),
    )
}

/// [`generate_seal_marker_requiring`] with `sealed_at` given instead of read from the
/// clock: a seam for the Layout 3 byte pin (`tests/on_tape_golden.rs`, ADR-0012 item
/// 243). Production callers go through [`generate_seal_marker_requiring`], which passes
/// the clock's time rendered at the fixed width of [`seal_time`]; every other byte is the
/// same.
pub fn generate_seal_marker_requiring_at(
    label: &str,
    file_count: i32,
    front_index_sha256: &str,
    files: &[FrontIndexFile],
    features: &[&str],
    sealed_at: &str,
) -> String {
    let mut s = seal_document(
        label,
        file_count,
        i64::from(file_count) - 2,
        front_index_sha256,
        features,
        sealed_at,
    );
    append_files_entries(&mut s, files);
    s
}

/// Generate the plaintext **short seal marker** (`volume-format-v3.md` §5,
/// tapectl 1.6.0): the seal at K+1 of a tape the drive's early warning ended
/// after data slice K (§5.2 W4), `ended_after = K`, `file_count = K+2`.
///
/// `files_through_k` is File 3's entries 0 to K, with File 3's own entry
/// filled in as for a full seal (its `size_bytes` and `sha256_encrypted`);
/// this appends the seal's own bare entry at K+1 (`position` and `type`
/// only, self-reference). Entry K+1 of File 3 plans a data slice there; the
/// copy says what the tape holds.
///
/// The header is the full seal's, word for word, and the timestamp the same
/// fixed width: a short seal is never longer than the placeholder `build`
/// sized with [`generate_seal_marker`] (§5 "Length"), which
/// `ReadyToSeal::seal` checks.
pub fn generate_short_seal_marker(
    label: &str,
    ended_after: i32,
    front_index_sha256: &str,
    files_through_k: &[FrontIndexFile],
) -> String {
    generate_short_seal_marker_requiring(
        label,
        ended_after,
        front_index_sha256,
        files_through_k,
        REQUIRES,
    )
}

/// The short seal stating `features` in `requires` (a span tape's: ADR-0012
/// item 124). The grammar, rules S1-S6 and every other byte are
/// [`generate_short_seal_marker`]'s (item 126).
pub fn generate_short_seal_marker_requiring(
    label: &str,
    ended_after: i32,
    front_index_sha256: &str,
    files_through_k: &[FrontIndexFile],
    features: &[&str],
) -> String {
    generate_short_seal_marker_requiring_at(
        label,
        ended_after,
        front_index_sha256,
        files_through_k,
        features,
        &seal_time(),
    )
}

/// [`generate_short_seal_marker_requiring`] with `sealed_at` given, the seam for the
/// short seal's byte pin, on the terms of [`generate_seal_marker_requiring_at`].
pub fn generate_short_seal_marker_requiring_at(
    label: &str,
    ended_after: i32,
    front_index_sha256: &str,
    files_through_k: &[FrontIndexFile],
    features: &[&str],
    sealed_at: &str,
) -> String {
    debug_assert!(
        files_through_k
            .last()
            .is_some_and(|f| f.position == ended_after),
        "a short seal's copy runs to ended_after"
    );
    let mut s = seal_document(
        label,
        ended_after.saturating_add(2),
        i64::from(ended_after),
        front_index_sha256,
        features,
        sealed_at,
    );
    append_files_entries(&mut s, files_through_k);
    append_files_entries(
        &mut s,
        &[FrontIndexFile {
            position: ended_after.saturating_add(1),
            type_label: "seal_marker",
            size_bytes: None,
            sha256_encrypted: None,
        }],
    );
    s
}

/// The time a seal is stamped with, from the clock, at the FIXED width its sizing needs.
/// FIXED-WIDTH timestamp, deliberately: the build step sizes the seal
/// marker with a placeholder and the seal step regenerates it with the
/// real `sealed_at`, which is only sound if both render at identical
/// byte length (`v2-open-questions.md` §9). `to_rfc3339()` does NOT
/// guarantee that — it is `SecondsFormat::AutoSi`, which drops
/// trailing-zero fractional digits, so it emits 25/29/32/35-byte strings
/// depending on the nanosecond value (measured on this VM: ~99.89% at
/// 35 bytes, ~0.11% at 32, ~0.0002% at 29). Under that format a real
/// reseal would land on a different width roughly once per ~900 writes
/// and fail its own length-identity check. `SecondsFormat::Secs` with
/// `use_z = true` renders exactly 20 bytes ("2026-07-22T20:09:00Z"),
/// always — second precision is ample for an audit timestamp, and
/// fixed width is what the sizing trick actually requires.
fn seal_time() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

/// The seal marker's human header and `[seal]` table, the same for a full
/// and a short seal (`volume-format-v3.md` §5, "Grammar" and "The
/// human-readable header"), so the two kinds cannot drift apart in anything
/// but the numbers and the embedded copy.
fn seal_document(
    label: &str,
    file_count: i32,
    ended_after: i64,
    front_index_sha256: &str,
    features: &[&str],
    now: &str,
) -> String {
    let requires = requires_value(features);
    let t_label = crate::volume::format::toml_string(label);
    format!(
        r#"================================================================
                    TAPECTL SEAL MARKER
================================================================

Volume: {label}
This file seals the tape: its presence means every file before it is
present. Its absence means the tape is unsealed (interrupted or aborted).
The tape may end before the last file the front index plans:
ended_after below is the last file before this one, and a file the
front index lists after it is not on this tape. It binds the front
index by the sha256 below. The [[files]] entries are a copy of the
front index (File 3) up to this file, usable if File 3 is damaged —
verify any entry by hashing the file it describes.

================================================================
              MACHINE-READABLE DATA (TOML)
================================================================

[seal]
volume = {t_label}
layout_version = 3
requires = {requires}
file_count = {file_count}
ended_after = {ended_after}
sealed_at = "{now}"
front_index_sha256 = "{front_index_sha256}"
"#
    )
}

/// Generate MANIFEST.toml for a tenant envelope.
///
/// A thin wrapper over [`crate::volume::manifest::Manifest::to_toml`] — the
/// single writer/reader type for the on-tape envelope manifest (see
/// `src/volume/manifest.rs`). Kept here, under its original name and
/// signature, because every call site addresses it as `layout::
/// generate_manifest_toml`.
pub fn generate_manifest_toml(label: &str, tenant_name: &str, units: &[ManifestUnit]) -> String {
    generate_manifest_toml_at(label, tenant_name, units, &chrono::Utc::now().to_rfc3339())
}

/// [`generate_manifest_toml`] with `created_at` given instead of read from the clock: a
/// seam for the Layout 3 byte pins (`tests/on_tape_golden.rs`, ADR-0012 item 243).
/// Production callers go through [`generate_manifest_toml`], which passes the clock's
/// `to_rfc3339()` as before; every other byte is the same.
pub fn generate_manifest_toml_at(
    label: &str,
    tenant_name: &str,
    units: &[ManifestUnit],
    created_at: &str,
) -> String {
    let manifest = crate::volume::manifest::Manifest {
        volume: label.to_string(),
        tenant: tenant_name.to_string(),
        created_at: created_at.to_string(),
        // Every envelope of every tape 1.6.0 writes (`volume-format-v3.md`
        // §1.8, ADR-0012 item 111): the tape may end early, and only its seal
        // says whether it did.
        may_end_early: true,
        units: units.to_vec(),
    };
    manifest.to_toml()
}

/// RECOVERY.md's section on the cross-volume index (`volume-format-v3.md`
/// §1.5): what `index/units.tsv` and `index/paths.tsv` hold, where each is,
/// the staleness rule, and one `awk` line for each that reads the text as it
/// is stored (escaped). One section serves both kinds of envelope, so
/// `generate_recovery_md` needs no more arguments: it says which envelope has
/// which member.
const RECOVERY_INDEX_SECTION: &str = r##"## The index of other tapes

This envelope also holds an index of the archive as it was known when
this tape was written, next to MANIFEST.toml:

- `index/units.tsv`: every version of the units it knows of, and the tapes
  that hold each. A tenant's envelope lists that tenant's units; the
  operator's envelope lists every tenant's, with a `tenant` column.
- `index/paths.tsv`: every path of those versions, with its unit and
  version. A tenant's envelope has it; the operator's does not.

A unit that was renamed after this tape was written is listed here under
the name it had then, and a newer tape's index lists it under its newer
name. They are one unit when their `unit_uuid` is the same, so match units by
`unit_uuid`, and on each tape give RESTORE.sh the name that tape shows
(`--unit` takes a name, and an envelope knows a unit by the name it had when
its tape was written). Two units of one name but different `unit_uuid` are
different units: restore each into a directory of its own.

An index is as of the moment its own tape was written. It never names a
tape written after it, and a tape it names may since have been retired or
erased, so the NEWEST tape's index is the most complete. Nothing on a tape
can name tapes written after it, so the newest tape you hold may not be the
newest ever written. `RESTORE.sh --where UNIT`, `--where --all` (every unit
the index names, with no limit: its newest version, whether the tape in the
drive holds it, and every tape that holds each version) and `--find TEXT`
(with your key) read it for you.

Both files are plain text. The first line names the columns, fields are
separated by one TAB, and each row ends in a newline. Inside a field four
characters are escaped and nothing else: a backslash is written `\\`, a TAB
`\t`, a newline `\n` and a carriage return `\r` (a path may hold any of
them). Find a column by its name, and ignore a column you do not know.

`units.tsv` has one row for each version and holding tape: `tenant`
(operator envelope only), `unit`, `unit_uuid`, `version`, `num_slices`,
`first_slice` and `last_slice` (the slices that tape holds), `version_bytes`,
`volume` (the tape's label), `volume_uuid`, `cartridge_serial`,
`generation`, `state` (`sealed`, `quarantined` or `this-tape`) and
`sealed_at`. `paths.tsv` has `unit`, `version`, `kind` (`d` directory, `f`
file, `l` symlink, `s` special), `size_bytes`, `sha256` (the file's content
hash as 64 hex characters; empty for anything but a regular file, and where
none was recorded) and `path`.

These two lines (run them inside `env/`) print the rows for a unit, and the
paths that contain some text. They compare the text as it is stored, so type a backslash, TAB or
newline of a name the way the file writes it:

```bash
U='photos/2019' awk -F'\t' 'NR == 1 { for (i = 1; i <= NF; i++) c[$i] = i; next } $c["unit"] == ENVIRON["U"]' index/units.tsv
T='report' awk -F'\t' 'NR == 1 { for (i = 1; i <= NF; i++) c[$i] = i; next } index($c["path"], ENVIRON["T"])' index/paths.tsv
```

The newest version of a unit holds the files that version had; a file deleted
before it was made exists only in an older version. In a tenant's envelope this
prints the paths version `OLD` of a unit has and version `NEW` lacks (both
versions need rows in this `paths.tsv`: a version held only on another tape has
none here, and the command says so):

```bash
OLD=2 NEW=3 U='photos/2019' awk -F'\t' 'NR == 1 { for (i = 1; i <= NF; i++) c[$i] = i; next } $c["unit"] == ENVIRON["U"] { if ($c["version"] == ENVIRON["NEW"]) { n++; seen[$c["path"]] } else if ($c["version"] == ENVIRON["OLD"]) old[$c["path"]] } END { if (!n) print "no paths for version " ENVIRON["NEW"] > "/dev/stderr"; else for (p in old) if (!(p in seen)) print p }' index/paths.tsv | sort
```

To restore one of them, follow the section of this file for version `OLD` of
the unit, into a directory of its own.

"##;

/// RECOVERY.md's section on where the tape ended (`volume-format-v3.md` §5,
/// §7.5), after the units table and before the first unit's section: the
/// table is the plan, the seal marker says where the tape really ended, the
/// four lines that read it with no key, and what a slice after it means. The
/// same in every envelope.
const RECOVERY_END_SECTION: &str = r##"## Where this tape ended

This tape may end before the last tape file the write planned, because the
drive reached the end of its medium. The table above is the plan, written
before any slice was. A unit that does not fit continues on the next
cartridge: this tape then holds the first slices of it (a range), and the
rest are on another tape.

The seal marker, the last file on the tape, says where it ended: its
`ended_after` is the last tape file before the seal, and every tape file
after `ended_after` is not on this tape. To read it by hand, with no key
(`/dev/nst0` below is an example — run `ls -l /dev/tape/by-id/` and
substitute your own drive, or you may read a different tape):

```bash
mt -f /dev/nst0 setblk 524288
mt -f /dev/nst0 eod          # GNU mt: eom
mt -f /dev/nst0 bsfm 2
dd if=/dev/nst0 bs=512k | tr -d '\0' | grep -E '^(file_count|ended_after) ='
```

When a slice of a unit (its tape file in the table above) is after
`ended_after`, it is on another tape: never read it from this one. The files
whose slices are all here are restored by the steps below with the slices that
are; the others need the other tape. The index (`index/units.tsv`, below)
and `RESTORE.sh --where UNIT` name the tapes that hold them. `RESTORE.sh`
(tape file 2) does all of this by itself: `--info` and `--verify` report where
the tape ended.

"##;

/// RECOVERY.md's section on a damaged slice or file (`volume-format-v3.md`
/// §1.7, §7.4): what a slice that fails is, the one repair from another copy
/// of the unit, and leaving a slice out by hand. It serves every kind of
/// envelope.
const RECOVERY_DAMAGE_SECTION: &str = r##"## If a slice or a file is damaged

Another tape that holds this unit at the same version holds the same slices,
byte for byte, when both tapes were written from one staging of it: its
front index (File 3) lists the same `sha256_encrypted` for each, at whatever
tape file and under whatever number that tape puts it. Only the same unit at
the same version is a copy; another version may differ in the very files that
are damaged.

A slice that fails its `sha256sum -c` above is damaged: do not decrypt it,
and do not put an empty file in its place. `RESTORE.sh` (tape file 2) leaves
a slice that fails out by itself, restores every file whose slices are good,
and writes the paths of the others, one per line (a backslash, TAB, newline
and carriage return escaped as the index escapes them), to
`.tapectl-damaged-files.txt` in the unit's directory. Load a tape that holds
the same unit at the same version and run that list through its one repair,
into the same directory (here `/destination/UNIT` stands for the unit's
directory):

```bash
./RESTORE.sh --restore --key YOUR_KEY.age.key --paths-from /destination/UNIT/.tapectl-damaged-files.txt --to /destination/UNIT
```

It restores exactly the listed files over their damaged copies, leaves
everything else in the directory alone, checks each against the unit's file
table, and refuses a tape that holds another unit or another version.

By hand, with no other copy, leave the failed slice out of the steps above:
remove its `slice.N.tar`, and if it is part of a long file's run (it or a
slice just before it states `continues = true` in MANIFEST.toml) leave out the
whole run, so that tar is never given the head of a file without its tail.
The slices that are left, joined in order, are a tar stream of whole files,
and tar restores every file in them; the file table's `slice` column says
which files were in the slices you left out. A file's sha256 in the table is
the check: the line after the steps prints `FAILED` for each file that is
missing or differs.

"##;

/// The directory name of a unit's work and destination: the stem of its file
/// table, the unit uuid's first eight characters, `_v`, the version, `_s` and
/// the stage set (a unit's name may hold a `/` or a space, so it is never a
/// path). Two stagings of one version on one tape are two blocks that never
/// share a directory (`volume-format-v3.md` 1.6; #596 C-4).
fn unit_dir_name(unit: &ManifestUnit) -> String {
    format!(
        "{}_v{}_s{}",
        unit.uuid.get(..8).unwrap_or(&unit.uuid),
        unit.snapshot_version,
        unit.stage_set_id
    )
}

/// Whether this tape's plan holds only some of the staging's slices: its
/// block gives some slices no `tape_position` (`volume-format-v3.md` §5.3).
/// Layout 3 has no span key (item 288): every staging may continue on
/// another cartridge, and the positions say what this tape holds.
fn held_in_part(unit: &ManifestUnit) -> bool {
    unit.slices.iter().any(|sl| sl.tape_position.is_none())
}

fn slices_here(unit: &ManifestUnit) -> Vec<i64> {
    unit.slices
        .iter()
        .filter_map(|sl| sl.tape_position)
        .collect()
}

/// RECOVERY.md's section on a unit across tapes (`volume-format-v3.md` §5.3,
/// §7.5), after the section on where the tape ended, in an envelope that
/// lists a block this tape holds only some of: which slices of each such unit
/// this tape's plan holds, that the others are on other tapes, what a read of
/// this tape alone gives, and that RESTORE.sh asks for the next cartridge.
/// (`span_proofs` holds the section's title to "a block held in part".)
fn recovery_span_section(units: &[ManifestUnit]) -> String {
    let mut s = String::from(
        "## A unit across tapes\n\n\
         These units continue on another cartridge (a slice of each has no tape\n\
         position in MANIFEST.toml). Each block lists every slice of the unit, and\n\
         this tape's plan holds the slices given a tape position:\n\n\
         | Unit | Snapshot | Slices on this tape | Tape files | Slices in all |\n\
         |------|----------|---------------------|------------|---------------|\n",
    );
    for unit in units.iter().filter(|u| held_in_part(u)) {
        let held: Vec<(i64, i64)> = unit
            .slices
            .iter()
            .filter_map(|sl| sl.tape_position.map(|p| (sl.number, p)))
            .collect();
        let (run, files) = match (held.first(), held.last()) {
            (Some(f), Some(l)) => (format!("{} to {}", f.0, l.0), format!("{}..{}", f.1, l.1)),
            _ => ("none".to_string(), "none".to_string()),
        };
        s.push_str(&format!(
            "| {} | v{} | {run} | {files} | {} |\n",
            unit.name,
            unit.snapshot_version,
            unit.slices.len(),
        ));
    }
    s.push_str(
        "\nThe slices with no tape position, and any after the seal marker's\n\
         `ended_after` (\"Where this tape ended\", above), are on other tapes. A slice\n\
         holds whole files, so this tape alone restores every file whose slices are\n\
         all on it: the unit's block below reads the slices this tape holds, tar\n\
         restores those files, and the check after it names the others. A restore of\n\
         the whole unit needs every slice, 1 to the last, in slice order, from the\n\
         tapes that hold them, each taken from a tape whose front index (File 3)\n\
         lists the slice's `sha256_encrypted` (MANIFEST.toml gives every slice's)\n\
         and by nothing else. `RESTORE.sh` (tape file 2) does it, starting on any\n\
         tape that holds some of the unit: it asks for the next cartridge at a\n\
         prompt (ENTER after you load it, `done` or the end of the input for \"I do\n\
         not have it\", `quit` to stop), and `RESTORE.sh --where UNIT` on the newest\n\
         tape names the tapes that hold the rest.\n\n",
    );
    s
}

/// RECOVERY.md's block of one unit: make its own work directory and its own
/// destination, read each slice this tape holds, join them into one tar run
/// and remove the work directory (#522: no block reads another's slices, and
/// no two units meet in one directory). `decompress` is the tool that reads
/// each slice after it is decrypted, for a compressed unit.
fn recovery_unit_block(unit: &ManifestUnit) -> String {
    let dir = unit_dir_name(unit);
    let work = format!("work/{dir}");
    let decompress = match unit.compression.as_deref() {
        Some("gzip") => " | gzip -dc",
        Some("xz") => " | xz -dc",
        Some("zstd") => " | zstd -dc",
        _ => "",
    };
    let mut s = format!(
        "```bash\n\
         mkdir -p {work} /destination/{dir}\n\
         mt -f /dev/nst0 setblk 524288\n\n\
         # On each `age -d` line, add `-i <file>` for every other key you hold\n\
         # (see \"Which key\" above).\n\n"
    );
    // A slice that continues a file begun on another tape is not read: its
    // first bytes are the middle of a file, and tar must begin at a file.
    let first_here = unit.slices.iter().position(|sl| sl.tape_position.is_some());
    let mut leading_skipped = 0usize;
    if let Some(f) = first_here {
        while unit
            .slices
            .get(f + leading_skipped)
            .is_some_and(|sl| sl.continues && sl.tape_position.is_some())
        {
            leading_skipped += 1;
        }
    }
    let mut joined: Vec<String> = Vec::new();
    let mut next: Option<i64> = None;
    for (i, slice) in unit.slices.iter().enumerate() {
        let Some(tape_position) = slice.tape_position else {
            s.push_str(&format!(
                "# Slice {n} — on another tape (no tape position here): {eb} bytes,\n\
                 # sha256_encrypted {sha}\n\n",
                n = slice.number,
                eb = slice.encrypted_bytes,
                sha = slice.sha256_encrypted,
            ));
            continue;
        };
        if first_here.is_some_and(|f| i >= f && i < f + leading_skipped) {
            s.push_str(&format!(
                "# Slice {n} — tape file {tape_position}: it continues a file that begins on\n\
                 # another tape, so it is not read here\n\n",
                n = slice.number,
            ));
            continue;
        }
        // One rewind per unit (#412): a read leaves the tape at the next
        // file, so only a gap between slices needs a forward space.
        let position = match next {
            None => format!("mt -f /dev/nst0 rewind && mt -f /dev/nst0 fsf {tape_position}\n"),
            Some(n) if tape_position > n => {
                format!("mt -f /dev/nst0 fsf {}\n", tape_position - n)
            }
            Some(n) if tape_position == n => String::new(),
            Some(_) => {
                format!("mt -f /dev/nst0 rewind && mt -f /dev/nst0 fsf {tape_position}\n")
            }
        };
        next = Some(tape_position + 1);
        let n = slice.number;
        s.push_str(&format!(
            "# Slice {n} — tape file {tape_position}, {eb} bytes{cont}\n\
             {position}\
             dd if=/dev/nst0 bs=512k of={work}/slice.{n}.age\n\
             truncate -s {eb} {work}/slice.{n}.age\n\
             echo \"{sha}  {work}/slice.{n}.age\" | sha256sum -c - && age -d -i YOUR_KEY.age.key {work}/slice.{n}.age{decompress} > {work}/slice.{n}.tar && rm {work}/slice.{n}.age\n\
             test $(wc -c < {work}/slice.{n}.tar) -eq {plain_bytes} && echo \"{sha_plain}  {work}/slice.{n}.tar\" | sha256sum -c -\n\n",
            eb = slice.encrypted_bytes,
            sha = slice.sha256_encrypted,
            plain_bytes = slice.size_bytes,
            sha_plain = slice.sha256_plain,
            cont = if slice.continues {
                " (it continues the file begun in the slice before it)"
            } else {
                ""
            },
        ));
        joined.push(format!("{work}/slice.{n}.tar"));
    }
    if joined.is_empty() {
        s.push_str(
            "# No slice of this unit is readable from this tape: see \"Where this tape ended\".\n",
        );
    } else {
        s.push_str(&format!(
            "# The slices, joined in order, are the unit's tar stream. One tar run, into the\n\
             # directory made above (numbers, not names, for owner and group; with another\n\
             # tar than GNU tar, leave out the flags it does not know):\n\
             cat {} | LC_ALL=C tar -x --numeric-owner -p --xattrs --acls -f - -C /destination/{dir}\n\
             rm -r {work}\n",
            joined.join(" "),
        ));
    }
    s.push_str("```\n\n");
    s
}

/// Generate RECOVERY.md for an envelope (`volume-format-v3.md` §7.5). Each
/// unit gets a block that makes a work directory and a destination of its
/// own (#522), reads this tape's slices of it, and joins them into one tar
/// run; a tenant's envelope also gets the check against the unit's file
/// table and the id map. `_catalogue_members` is kept for the callers that
/// pass the bare names of the envelope's extra members; Layout 3 has no
/// per-unit catalogue file to name, so nothing is drawn from it.
pub fn generate_recovery_md(
    label: &str,
    tenant_name: &str,
    units: &[ManifestUnit],
    _catalogue_members: &[String],
) -> String {
    let now = chrono::Utc::now().format("%Y-%m-%d").to_string();
    let mut s = format!(
        "# Recovery Guide for {tenant_name}\n\n\
         Volume: {label}\n\
         Date: {now}\n\n\
         This tape holds age-encrypted pieces of tar archives: each unit is one tar\n\
         stream, cut into slices that begin where a file begins, and each slice is\n\
         encrypted on its own. With your age key(s) and the standard tools (`mt`,\n\
         `dd`, `truncate`, `age`, `tar`, `sha256sum`) you can recover your data by\n\
         hand — no tapectl required. The automated `RESTORE.sh` (tape file 2) does\n\
         the same for you, and checks every restored file against the unit's file\n\
         table; use it if you can.\n\n\
         The commands below are written for the directory this envelope was\n\
         unpacked in (`env/` in the guide's steps): run them from the directory\n\
         that holds `env/`. Each unit's block makes a work directory and a\n\
         destination of its own, so the blocks can be run one after another in\n\
         one place and never read each other's slices.\n\n\
         ## Which key\n\n\
         The key that opened this envelope is\n\
         not necessarily the key that opens the slices below.\n\
         The envelope was sealed when the tape was written; each slice was sealed\n\
         earlier, when it was staged. If the keys were rotated in between, the\n\
         slices need an older key. `age` accepts several keys at once and uses\n\
         whichever one matches, so pass every key you hold:\n\n\
         ```bash\n\
         age -d -i OLD.age.key -i NEW.age.key slice.1.age > slice.1.tar\n\
         ```\n\n\
         Name only key files that exist:\n\
         one unreadable `-i` path fails the whole command.\n\
         If every key fails with `no identity matched any of the recipients` (`rage`:\n\
         `No matching keys found`), the\n\
         slice was sealed to a key you have not supplied — the data is not corrupt.\n\
         `age-keygen -y YOUR_KEY.age.key` shows whether a file is a key at all (it\n\
         prints that one key's public `age1...` line, or an error), and the guide on\n\
         this tape (File 1, \"Which file is your key\") tells a bad key file from a\n\
         tape of another installation.\n\n\
         ## Units in this envelope\n\n\
         | Unit | Snapshot | Slices | Tape files |\n\
         |------|----------|--------|------------|\n"
    );
    for unit in units {
        // A whole unit's every slice has a position. A unit this tape holds
        // part of lists every slice of its set, and only the slices this tape
        // plans have one: a slice with none is on another tape, never at tape
        // file 0, so its row counts and spans only those.
        let here = slices_here(unit);
        let (count, files) = if held_in_part(unit) {
            (
                format!("{} of {}", here.len(), unit.slices.len()),
                match (here.first(), here.last()) {
                    (Some(f), Some(l)) => format!("{f}..{l}"),
                    _ => "none".to_string(),
                },
            )
        } else {
            let (first, last) = match (here.first(), here.last()) {
                (Some(f), Some(l)) => (*f, *l),
                _ => (0, 0),
            };
            (unit.slices.len().to_string(), format!("{first}..{last}"))
        };
        s.push_str(&format!(
            "| {} | v{} | {count} | {files} |\n",
            unit.name, unit.snapshot_version,
        ));
    }
    s.push('\n');
    s.push_str(RECOVERY_END_SECTION);
    if units.iter().any(held_in_part) {
        s.push_str(&recovery_span_section(units));
    }

    for unit in units {
        let dir = unit_dir_name(unit);
        s.push_str(&format!(
            "## {}\n\n\
             UUID: `{}`  ·  snapshot v{}  ·  {} bytes of tar stream",
            unit.name, unit.uuid, unit.snapshot_version, unit.stream_bytes,
        ));
        s.push_str("\n\n");
        if let Some(alg) = unit.compression.as_deref() {
            let (tool, package) = match alg {
                "gzip" => ("gzip -dc", "gzip"),
                "xz" => ("xz -dc", "xz-utils"),
                "zstd" => ("zstd -dc", "zstd"),
                _ => ("the tool of that algorithm", "its package"),
            };
            s.push_str(&format!(
                "Each slice of this unit was compressed with {alg}, after the cut and before\n\
                 it was encrypted: the block below pipes each decrypted slice through\n\
                 `{tool}` (package {package}). The length and the hash it checks are of the\n\
                 decompressed slice.\n\n"
            ));
        }
        if let Some(table) = &unit.file_table {
            s.push_str(&format!(
                "This envelope holds the unit's file table, `{table}` (every file's kind,\n\
                 slice, place in the stream, size, sha256 and path), and its id map,\n\
                 `{ids}` (who each user and group number was on the computer that wrote\n\
                 the tape; shown, never used). A restore here is judged by the table.\n\n",
                ids = unit.id_map.as_deref().unwrap_or("tables/…ids.tsv"),
            ));
        } else {
            s.push_str(
                "This (the operator's) envelope lists the unit but holds its file table only\n\
                 as rows of `catalog.db`; the unit's own tenant's envelope has it as a file.\n\n",
            );
        }
        if held_in_part(unit) {
            s.push_str(
                "This tape holds only some of this unit's slices (the others are on other\n\
                 tapes: \"Where this tape ended\"). The block reads the ones it holds; tar\n\
                 then restores every file whose slices are all here, and says \"Unexpected\n\
                 EOF in archive\" for a file that runs on into a slice on another tape: that\n\
                 file is not whole, and the check says so.\n\n",
            );
        }
        s.push_str(
            "Put the drive in fixed 512KB block mode, then read, trim, verify and\n\
             decrypt each slice, in order: the tape is wound to the first slice\n\
             once, and each read that finishes leaves it at the start of the next\n\
             file. A `dd` that stops with an error (\"Input/output error\") leaves\n\
             the head inside that file instead: `mt rewind`, then `mt fsf` to the\n\
             tape file of the slice you want next, which its `# Slice` line gives.\n\
             `/dev/nst0` below is an example — run `ls -l /dev/tape/by-id/` and\n\
             substitute your own drive, or you may read a different tape. The block\n\
             makes `work/` and a directory for this unit and removes it when the unit is\n\
             restored: it needs room for the whole unit's slices (its stream size) once,\n\
             and the destination needs the same again.\n\n",
        );
        s.push_str(&recovery_unit_block(unit));
        if let Some(table) = &unit.file_table {
            let table = table.trim_start_matches("tables/");
            s.push_str(&format!(
                "Check the unit against its file table: this prints nothing when every file\n\
                 passes and `FAILED` for each that does not (a file whose name holds a\n\
                 backslash, TAB, newline or carriage return is written escaped in the table\n\
                 and is left out of this line; `RESTORE.sh` checks it):\n\n\
                 ```bash\n\
                 awk -F'\\t' '!/^#/ && ($1 == \"f\" || $1 == \"h\") && index($7, \"\\\\\") == 0 {{ print $6 \"  ./\" $7 }}' env/tables/{table} | (cd /destination/{dir} && sha256sum -c --quiet)\n\
                 ```\n\n",
            ));
        }
    }

    s.push_str(RECOVERY_INDEX_SECTION);
    s.push_str(RECOVERY_DAMAGE_SECTION);
    s.push_str(
        "## Troubleshooting\n\n\
         - **age: \"failed to decrypt and authenticate payload chunk\"** (age 1.1.1) — the\n\
           file still has 512KB block padding, or a part of it is damaged. Re-run\n\
           `truncate -s <bytes>` to the exact `encrypted_bytes` shown above (a slice's\n\
           `size_bytes` in MANIFEST.toml is its PLAIN length: never trim to that); if the\n\
           sha256 of the trimmed file matches, the file is whole and the key is the\n\
           question, and if it does not, the slice is damaged.\n\
         - **age: \"failed to read header: parsing age header\"** — what was read is not\n\
           an age file: the wrong tape file, or the head was not where you thought.\n\
           `mt rewind`, `fsf` to the slice's tape file, and read it again.\n\
         - **tar: \"Unexpected EOF in archive\" or \"This does not look like a tar\n\
           archive\"** — the slices given to tar are not whole files in order: one is\n\
           missing, left out, or the first begins in the middle of a file. The check\n\
           against the file table names what is missing; tar's exit status never does.\n\
         - **sha256 mismatch** — re-read the slice from tape; a short read or the wrong\n\
           block mode (must be 512KB fixed) is the usual cause.\n\
         - **age: \"no identity matched any of the recipients\"** (`rage`: \"No matching\n\
           keys found\") — none of the keys\n\
           you passed is the one this slice was sealed to. It may be an older key from\n\
           before a rotation (see \"Which key\"), or the operator key, which can read\n\
           every unit; or the tape may be another installation's. Pass them all with\n\
           repeated `-i`.\n",
    );

    s
}

/// Re-exported so every existing `layout::ManifestUnit` / `layout::
/// ManifestSlice` path keeps compiling. The canonical definitions live in
/// [`crate::volume::manifest`], which both this module's writer and
/// `envelope`'s reader now share. `ManifestSlice::tape_position` is `i64`
/// here (previously `i32` in this module) — the reader's original, wider
/// type; the writer's `i32` was the narrower mistake.
pub use crate::volume::manifest::{ManifestSlice, ManifestUnit};

#[cfg(test)]
mod tests {
    use super::*;

    // ---- what a 1.6.0 tape declares up front (volume-format-v3.md §1.8,
    // ADR-0012 item 111, #420) ----

    /// D1 (`volume-format-v3.md` §1.1, §1.2): File 0 states Layout 3's magic
    /// and `layout_version = 3`, and `requires = []` on the line after it, on
    /// every tape. This tapectl reads its own File 0.
    #[test]
    fn file_0_states_layout_3_and_requires_nothing_after_layout_version() {
        let t = generate_id_thunk_v2(&IdThunkV2Params {
            label: "SS01",
            uuid: "u",
            media_type: "LTO-6",
            tapectl_version: "1.6.0",
            nominal_capacity: 1,
            mam_capacity: 1,
            total_files: 9,
            mam_manufacturer: "m",
            mam_serial: "s",
            mam_length: 1,
            mam_loads: 1,
            created_at: "2026-10-20T00:00:00Z",
            cartridge_identity_source: None,
        });
        assert_eq!(
            t.matches("\nmagic = \"tapectl-volume-v3\"\n").count(),
            1,
            "{t}"
        );
        assert_eq!(
            t.matches("\nlayout_version = 3\nrequires = []\n").count(),
            1,
            "{t}"
        );
        assert_eq!(t.matches("requires").count(), 1, "{t}");
        let claims = crate::volume::format::parse_id_thunk_claims(&t).unwrap();
        assert!(claims.requires.is_empty() && claims.requires_stated);
        assert_eq!(
            claims.refusal("File 0"),
            None,
            "this tapectl reads its own tape"
        );
        // File 0 is the plan: its pointers are the planned seal and count.
        assert!(t.contains("\nseal_marker = 8\ntotal_files = 9\n"), "{t}");
    }

    fn span_thunk_params() -> IdThunkV2Params<'static> {
        IdThunkV2Params {
            label: "SP01",
            uuid: "u",
            media_type: "LTO-6",
            tapectl_version: "1.7.0",
            nominal_capacity: 1,
            mam_capacity: 1,
            total_files: 9,
            mam_manufacturer: "m",
            mam_serial: "s",
            mam_length: 1,
            mam_loads: 1,
            created_at: "2026-10-23T00:00:00Z",
            cartridge_identity_source: None,
        }
    }

    /// `volume-format-v3.md` §1.2, for a later feature's fence: File 0 states
    /// exactly one `requires` line, the line after `layout_version`, spelled
    /// as the spec rules it: each feature quoted, a comma and ONE space
    /// between, no space inside the brackets. Every other byte is the
    /// generator's with `requires = []`, and this tapectl refuses the tape by
    /// name: it knows no feature (D1).
    #[test]
    fn a_later_features_requires_line_is_spelled_as_section_1_2_rules() {
        let p = span_thunk_params();
        let later = generate_id_thunk_v2_requiring(&p, &["one-feature", "another"]);
        assert_eq!(later.matches("requires").count(), 1, "{later}");
        assert_eq!(
            later,
            generate_id_thunk_v2(&p).replace(
                "\nrequires = []\n",
                "\nrequires = [\"one-feature\", \"another\"]\n"
            )
        );
        assert_eq!(
            generate_id_thunk_v2(&p),
            generate_id_thunk_v2_requiring(&p, REQUIRES)
        );
        let claims = crate::volume::format::parse_id_thunk_claims(&later).unwrap();
        assert_eq!(claims.requires, vec!["one-feature", "another"]);
        assert!(claims.refusal("File 0").is_some());
    }

    /// `build` states `requires = []` in File 0 and in the seal's
    /// placeholder, whether or not a unit's block lists slices on other tapes
    /// (D1; item 288: no `spanning` feature, no span key).
    #[test]
    fn build_states_requires_nothing_in_file_0_and_the_placeholder_span_or_not() {
        use crate::volume::build::{
            build, BuildInputs, BuildSlice, BuildUnit, SpanSlice, TenantInfo, UnitSpan,
        };
        use crate::volume::layout_model::{ContentSource, ZoneKind};
        let src = tempfile::tempdir().unwrap();
        let slice = |id: i64, number: i64, bytes: &[u8]| {
            let path = src.path().join(format!("slice-{id}"));
            std::fs::write(&path, bytes).unwrap();
            BuildSlice {
                continues: false,
                slice_id: id,
                slice_number: number,
                size_bytes: bytes.len() as i64,
                encrypted_bytes: bytes.len() as i64,
                sha256_plain: "aa".repeat(32),
                // build() takes the hash as given and never recomputes it.
                sha256_encrypted: format!("{id:064x}"),
                staging_path: path,
            }
        };
        let kp = crate::crypto::keys::generate_keypair();
        let op = crate::crypto::keys::generate_keypair();
        let inputs = |span: bool| {
            let planned = vec![slice(12, 2, b"slice two")];
            let mut all: Vec<SpanSlice> = planned.iter().map(SpanSlice::from).collect();
            all.insert(
                0,
                SpanSlice {
                    continues: false,
                    slice_id: 11,
                    slice_number: 1,
                    size_bytes: 9,
                    encrypted_bytes: 9,
                    sha256_plain: "bb".repeat(32),
                    sha256_encrypted: "cc".repeat(32),
                },
            );
            BuildInputs {
                tables: None,
                label: "SPANB1".into(),
                volume_uuid: "550e8400-e29b-41d4-a716-4466554400c1".into(),
                media_type: "LTO-6".into(),
                tapectl_version: "1.7.0".into(),
                created_at: "2026-10-23T00:00:00Z".into(),
                block_size: 524_288,
                usable_bytes: 100 * 524_288,
                enospc_buffer: 524_288,
                nominal_capacity: 2_400_000_000_000,
                mam_capacity: 0,
                mam_manufacturer: String::new(),
                mam_serial: String::new(),
                cartridge_identity_source: None,
                mam_length: 0,
                mam_loads: 0,
                units: vec![BuildUnit {
                    stage_set_id: 10,
                    snapshot_id: 10,
                    unit_name: "unit-span".into(),
                    unit_uuid: "cccccccc-1111-4222-8333-444444444444".into(),
                    tenant_id: 1,
                    tar_version: None,
                    stream_bytes: 0,
                    compression: None,
                    snapshot_version: 1,
                    slices: if span {
                        planned
                    } else {
                        vec![slice(11, 1, b"slice one"), slice(12, 2, b"slice two")]
                    },
                    span: span.then_some(UnitSpan { slices: all }),
                }],
                tenants: vec![TenantInfo {
                    tenant_id: 1,
                    tenant_name: "alpha".into(),
                    public_keys: vec![kp.public_key.clone()],
                }],
                operator_public_keys: vec![op.public_key.clone()],
                escrow_public_key: None,
                catalog_db_path: None,
                index: None,
            }
        };
        let read_zone = |built: &crate::volume::build::BuiltLayout, want: fn(&ZoneKind) -> bool| {
            let e = built
                .layout
                .entries
                .iter()
                .find(|e| want(&e.kind))
                .expect("the zone");
            match &e.source {
                ContentSource::Materialized(p) => std::fs::read_to_string(p).unwrap(),
                other => panic!("{other:?}"),
            }
        };
        for (span, line) in [(false, "requires = []"), (true, "requires = []")] {
            let session = tempfile::tempdir().unwrap();
            let built = build(&inputs(span), session.path()).unwrap();
            let file0 = read_zone(&built, |k| matches!(k, ZoneKind::IdThunk));
            let seal = read_zone(&built, |k| matches!(k, ZoneKind::SealMarker));
            for (what, text) in [("File 0", &file0), ("the seal placeholder", &seal)] {
                assert_eq!(text.matches("requires = ").count(), 1, "{what}: {text}");
                assert_eq!(
                    text.matches(&format!("\nlayout_version = 3\n{line}\n"))
                        .count(),
                    1,
                    "span {span}, {what}: {text}"
                );
            }
        }
    }

    /// File 3's `[index]` states `may_end_early = true` after
    /// `layout_version`, and its header gains §3's paragraph, word for word,
    /// after "see the system guide." and before the MACHINE-READABLE rule.
    #[test]
    fn file_3_declares_may_end_early_with_the_pinned_paragraph() {
        let files = vec![
            FrontIndexFile {
                position: 3,
                type_label: "front_index",
                size_bytes: None,
                sha256_encrypted: None,
            },
            FrontIndexFile {
                position: 4,
                type_label: "seal_marker",
                size_bytes: None,
                sha256_encrypted: None,
            },
        ];
        let s = generate_front_index("ME01", &files);
        let paragraph =
            "The trailing seal marker binds this index by hash; see the system guide.\n\
                         \n\
                         This tape may end early. The list below is the plan: every file the\n\
                         write meant to put on the tape. The seal marker, the LAST file on the\n\
                         tape, says where the tape really ended (its ended_after). A file listed\n\
                         after that position is not on this tape.\n\
                         \n\
                         ================================================================\n\
                         \x20             MACHINE-READABLE DATA (TOML)\n";
        assert_eq!(s.matches(paragraph).count(), 1, "{s}");
        assert_eq!(
            s.matches("[index]\nvolume = \"ME01\"\nlayout_version = 3\nmay_end_early = true\n")
                .count(),
            1,
            "{s}"
        );
        let doc = crate::volume::format::parse_front_index_document(&s).unwrap();
        assert!(doc.may_end_early);
    }

    /// Every envelope's MANIFEST.toml states `may_end_early = true`, the last
    /// key of `[manifest]`, then the blank line before `[[units]]` (§1.7).
    #[test]
    fn every_manifest_states_may_end_early() {
        let m = generate_manifest_toml(
            "ME02",
            "operator",
            &[ManifestUnit {
                name: "u".into(),
                uuid: "uuid-u".into(),
                snapshot_version: 1,
                stage_set_id: 1,
                payload: "tar".to_string(),
                stream_bytes: 0,
                file_table: None,
                id_map: None,
                tar_version: None,
                compression: None,
                slices: vec![],
            }],
        );
        let created = m.lines().find(|l| l.starts_with("created_at = ")).unwrap();
        assert!(
            m.contains(&format!("{created}\nmay_end_early = true\n\n[[units]]\n")),
            "{m}"
        );
        assert!(crate::volume::manifest::Manifest::declares_may_end_early(&m).unwrap());
    }

    /// ADR-0012 item 141 (with its 2026-10-22 correction of fact: the query
    /// reads the Heir Kit's `catalog.db.age`): File 1's query of which tapes
    /// hold which units lists `range` writes beside whole copies, each with
    /// the run of slices it holds. Run as written against a catalogue of this
    /// tapectl's schema, holding item 93's layout: a range 1-2 on V1, the
    /// whole set on V2 and a range 3-4 on V3; a cut write is not listed.
    #[test]
    fn file_1_heir_query_lists_range_rows_with_their_slice_runs() {
        let g = generate_system_guide_v2_for_media("SP03", 20, "LTO-6");
        let sec = guide_section(&g, "The Heir Kit's catalogue (catalog.db.age)");
        let from = sec
            .find("catalog.db \"SELECT u.name AS unit")
            .expect("the query of which tapes hold which units");
        let sqls = guide_sql(sec);
        let sql = &sqls[1];
        assert!(
            sql.contains("WHERE w.status IN ('completed', 'range')"),
            "{sql}"
        );

        let conn = crate::db::open_memory().unwrap();
        conn.execute_batch(
            "INSERT INTO tenants (id, name, is_operator, status) VALUES (1, 't', 0, 'active');
             INSERT INTO units (id, uuid, name, tenant_id) VALUES (1, 'uu', 'big', 1);
             INSERT INTO snapshots (id, unit_id, version, source_path) VALUES (1, 1, 2, '/s');
             INSERT INTO stage_sets (id, snapshot_id, status, slice_size) VALUES (1, 1, 'staged', 1000);
             INSERT INTO stage_slices (id, stage_set_id, slice_number, size_bytes, encrypted_bytes,
                                       sha256_plain, sha256_encrypted)
               VALUES (1, 1, 1, 10, 20, 'p1', 'e1'), (2, 1, 2, 10, 20, 'p2', 'e2'),
                      (3, 1, 3, 10, 20, 'p3', 'e3'), (4, 1, 4, 10, 20, 'p4', 'e4');
             INSERT INTO volumes (id, label, backend_type, backend_name, capacity_bytes, status)
               VALUES (1, 'V1', 'lto', 'p', 1000, 'sealed'), (2, 'V2', 'lto', 'p', 1000, 'sealed'),
                      (3, 'V3', 'lto', 'p', 1000, 'sealed');
             INSERT INTO writes (id, stage_set_id, snapshot_id, volume_id, status)
               VALUES (1, 1, 1, 1, 'range'), (2, 1, 1, 2, 'completed'),
                      (3, 1, 1, 3, 'range');
             INSERT INTO write_positions (write_id, stage_slice_id, position, status)
               VALUES (1, 1, '10', 'written'), (1, 2, '11', 'written'),
                      (1, 3, '12', 'pending'), (1, 4, '13', 'pending'),
                      (2, 1, '8', 'written'), (2, 2, '9', 'written'),
                      (2, 3, '10', 'written'), (2, 4, '11', 'written'),
                      (3, 3, '4', 'written'), (3, 4, '5', 'written');",
        )
        .unwrap();
        let mut stmt = conn.prepare(sql).expect("the query runs on this schema");
        let rows: Vec<(String, String, String, Option<String>, i64)> = stmt
            .query_map([], |r| {
                Ok((
                    r.get::<_, String>("unit")?,
                    r.get::<_, String>("tape")?,
                    r.get::<_, String>("holds")?,
                    r.get::<_, Option<String>>("slices")?,
                    r.get::<_, i64>("of_slices")?,
                ))
            })
            .unwrap()
            .map(|r| r.unwrap())
            .collect();
        let row = |tape: &str, holds: &str, slices: &str| {
            (
                "big".to_string(),
                tape.to_string(),
                holds.to_string(),
                Some(slices.to_string()),
                4,
            )
        };
        assert_eq!(
            rows,
            vec![
                row("V1", "range", "1-2"),
                row("V2", "completed", "1-4"),
                row("V3", "range", "3-4"),
            ]
        );
        let after = flat_text(&sec[from..]);
        assert!(
            after.contains("`holds` is `completed` for a tape that holds that version whole and `range` for one that holds only the run `slices` names, of the version's `of_slices`"),
            "{after}"
        );
        assert!(
            after.contains("A `range` tape is not a copy on its own"),
            "{after}"
        );
        // Layout 3's rule, not Layout 2's (D17, ADR-0012 item 266; #596): the
        // WHOLE version needs every slice; a range tape alone gives the files
        // whose slices it holds, whole.
        assert!(
            after.contains("the whole version restores only with every slice, 1 to `of_slices`"),
            "{after}"
        );
        assert!(
            after.contains(
                "Alone it restores the files whose slices it holds, whole, and lists the rest"
            ),
            "{after}"
        );
    }

    #[test]
    fn front_index_lists_every_file_with_hashes() {
        let files = vec![
            FrontIndexFile {
                position: 0,
                type_label: "id_thunk",
                size_bytes: Some(500),
                sha256_encrypted: Some("aa00".into()),
            },
            FrontIndexFile {
                position: 3,
                type_label: "front_index",
                size_bytes: None,       // self: length is self-referential
                sha256_encrypted: None, // self: cannot hash itself
            },
            FrontIndexFile {
                position: 4,
                type_label: "data_slice",
                size_bytes: Some(524288),
                sha256_encrypted: Some("bb11".into()),
            },
            FrontIndexFile {
                position: 5,
                type_label: "seal_marker",
                size_bytes: None,
                sha256_encrypted: None, // not yet written when File 3 is built
            },
        ];
        let s = generate_front_index("TEST01", &files);
        let body = &s[s.find("[index]").expect("has [index]")..];
        let parsed: toml::Value = body.parse().expect("TOML parses");
        let idx = parsed.get("index").unwrap();
        assert_eq!(idx.get("volume").unwrap().as_str(), Some("TEST01"));
        assert_eq!(idx.get("layout_version").unwrap().as_integer(), Some(3));

        let arr = parsed.get("files").unwrap().as_array().unwrap();
        assert_eq!(arr.len(), 4);
        // File 0 carries size + hash.
        assert_eq!(arr[0].get("position").unwrap().as_integer(), Some(0));
        assert_eq!(arr[0].get("size_bytes").unwrap().as_integer(), Some(500));
        assert_eq!(
            arr[0].get("sha256_encrypted").unwrap().as_str(),
            Some("aa00")
        );
        // The slice carries size + hash.
        assert_eq!(arr[2].get("size_bytes").unwrap().as_integer(), Some(524288));
        assert_eq!(
            arr[2].get("sha256_encrypted").unwrap().as_str(),
            Some("bb11")
        );
    }

    #[test]
    fn front_index_omits_self_and_seal_marker_hashes() {
        // The hash-chain rule (ADR-0007): the front index carries no hash of
        // itself (self-reference) or the seal marker (not yet written).
        let files = vec![
            FrontIndexFile {
                position: 3,
                type_label: "front_index",
                size_bytes: None,
                sha256_encrypted: None,
            },
            FrontIndexFile {
                position: 5,
                type_label: "seal_marker",
                size_bytes: None,
                sha256_encrypted: None,
            },
        ];
        let s = generate_front_index("TEST01", &files);
        let body = &s[s.find("[index]").unwrap()..];
        let parsed: toml::Value = body.parse().expect("TOML parses");
        let arr = parsed.get("files").unwrap().as_array().unwrap();
        // Both entries are navigable (position + type) but carry neither a
        // size nor a hash.
        for e in arr {
            assert!(e.get("position").is_some());
            assert!(e.get("type").is_some());
            assert!(e.get("size_bytes").is_none());
            assert!(e.get("sha256_encrypted").is_none());
        }
    }

    /// A seal's embedded copy for a plan of `m + 1` files that keeps
    /// `volume-format-v3.md` §5's rules: Files 0-2, File 3 (filled in, as
    /// the seal step fills it), the operator envelope at 4, data slices from
    /// 5 to `m - 1`, the seal's own bare entry at `m`. Sizes and hashes have
    /// production widths (a slice's size has 10 digits, a hash 64), so the
    /// length tests below measure what a real seal measures.
    fn planned_seal_copy(m: i32) -> Vec<FrontIndexFile> {
        assert!(m >= 6, "a plan with at least one data slice");
        (0..=m)
            .map(|position| {
                let type_label = match position {
                    0 => "id_thunk",
                    1 => "system_guide",
                    2 => "restore_sh",
                    3 => "front_index",
                    4 => "operator_envelope",
                    p if p == m => "seal_marker",
                    _ => "data_slice",
                };
                let is_seal = position == m;
                FrontIndexFile {
                    position,
                    type_label,
                    size_bytes: (!is_seal).then(|| 1_073_741_824 + position as u64),
                    sha256_encrypted: (!is_seal).then(|| format!("{position:064x}")),
                }
            })
            .collect()
    }

    /// The copy a short seal at K+1 carries before its own entry: the
    /// plan's entries 0 to K.
    fn short_seal_copy(m: i32, k: i32) -> Vec<FrontIndexFile> {
        planned_seal_copy(m)
            .into_iter()
            .take(k as usize + 1)
            .collect()
    }

    #[test]
    fn seal_marker_binds_front_index() {
        let s = generate_seal_marker("TEST01", 7, "deadbeefcafe", &planned_seal_copy(6));
        let body = &s[s.find("[seal]").expect("has [seal]")..];
        let parsed: toml::Value = body.parse().expect("TOML parses");
        let seal = parsed.get("seal").unwrap();
        assert_eq!(seal.get("volume").unwrap().as_str(), Some("TEST01"));
        assert_eq!(seal.get("layout_version").unwrap().as_integer(), Some(3));
        assert_eq!(seal.get("file_count").unwrap().as_integer(), Some(7));
        assert_eq!(seal.get("ended_after").unwrap().as_integer(), Some(5));
        assert!(
            seal.get("requires").unwrap().as_array().unwrap().is_empty(),
            "D1: a Layout 3 seal states no feature"
        );
        assert_eq!(
            seal.get("front_index_sha256").unwrap().as_str(),
            Some("deadbeefcafe")
        );
        // sealed_at is present and RFC3339-parseable.
        let sealed_at = seal.get("sealed_at").unwrap().as_str().unwrap();
        assert!(chrono::DateTime::parse_from_rfc3339(sealed_at).is_ok());
    }

    /// `volume-format-v3.md` §5 "Grammar": one `key = value` per line, in
    /// exactly this order, on both kinds. RESTORE.sh's line-oriented reads
    /// depend on it, so the lines are pinned as text, not as TOML.
    #[test]
    fn both_seals_write_the_seal_table_in_the_pinned_order() {
        let fi = "ab".repeat(32);
        let full = generate_seal_marker("ORDER1", 9, &fi, &planned_seal_copy(8));
        let short = generate_short_seal_marker("ORDER1", 6, &fi, &short_seal_copy(8, 6));
        for (seal, file_count, ended_after) in [(&full, 9, 7), (&short, 8, 6)] {
            let table: Vec<&str> = seal[seal.find("[seal]").unwrap()..]
                .lines()
                .take_while(|l| !l.is_empty())
                .collect();
            let keys: Vec<&str> = table
                .iter()
                .skip(1)
                .map(|l| l.split(" = ").next().unwrap())
                .collect();
            assert_eq!(
                keys,
                [
                    "volume",
                    "layout_version",
                    "requires",
                    "file_count",
                    "ended_after",
                    "sealed_at",
                    "front_index_sha256"
                ]
            );
            assert_eq!(table[1], "volume = \"ORDER1\"");
            assert_eq!(table[2], "layout_version = 3");
            assert_eq!(table[3], "requires = []");
            assert_eq!(table[4], format!("file_count = {file_count}"));
            assert_eq!(table[5], format!("ended_after = {ended_after}"));
            assert_eq!(table[7], format!("front_index_sha256 = \"{fi}\""));
        }
    }

    /// §5 "The human-readable header": the same on both kinds, so it does
    /// not change a seal's length, and exactly the spec's words.
    #[test]
    fn both_seals_carry_the_specs_header_word_for_word() {
        let header = "================================================================
                    TAPECTL SEAL MARKER
================================================================

Volume: HDR001
This file seals the tape: its presence means every file before it is
present. Its absence means the tape is unsealed (interrupted or aborted).
The tape may end before the last file the front index plans:
ended_after below is the last file before this one, and a file the
front index lists after it is not on this tape. It binds the front
index by the sha256 below. The [[files]] entries are a copy of the
front index (File 3) up to this file, usable if File 3 is damaged —
verify any entry by hashing the file it describes.

================================================================
              MACHINE-READABLE DATA (TOML)
================================================================

[seal]
";
        let full = generate_seal_marker("HDR001", 8, "fi", &planned_seal_copy(7));
        let short = generate_short_seal_marker("HDR001", 5, "fi", &short_seal_copy(7, 5));
        assert!(full.starts_with(header), "{full}");
        assert!(short.starts_with(header), "{short}");
    }

    /// Both kinds round-trip through `format.rs`'s parser (A's reader), keep
    /// rules S1-S5, and classify against the plan as §5 says: the full seal
    /// `Full`, the short one `Short { ended_after: K }`.
    #[test]
    fn the_full_and_the_short_seal_round_trip_through_the_parser() {
        use crate::volume::format::{parse_seal_marker, SealEnding};
        let fi = "cd".repeat(32);
        let m = 9;
        let planned = (m + 1) as u64;

        let full = parse_seal_marker(&generate_seal_marker(
            "RTRIP1",
            m + 1,
            &fi,
            &planned_seal_copy(m),
        ))
        .expect("a full seal parses");
        assert!(full.requires.is_empty(), "D1");
        assert_eq!(full.file_count, m + 1);
        assert_eq!(full.ended_after, Some((m - 1) as u64));
        assert_eq!(full.front_index_sha256, fi);
        assert_eq!(full.files.len(), (m + 1) as usize);
        assert_eq!(full.check_parse_rules(), Ok(()));
        assert_eq!(full.ending(planned), SealEnding::Full);

        for k in 5..=m - 2 {
            let short = parse_seal_marker(&generate_short_seal_marker(
                "RTRIP1",
                k,
                &fi,
                &short_seal_copy(m, k),
            ))
            .unwrap_or_else(|e| panic!("the short seal at {} parses: {e}", k + 1));
            assert!(short.requires.is_empty(), "D1");
            assert_eq!(short.file_count, k + 2);
            assert_eq!(short.ended_after, Some(k as u64));
            assert_eq!(short.front_index_sha256, fi, "it binds the same File 3");
            assert_eq!(short.check_parse_rules(), Ok(()));
            assert_eq!(
                short.ending(planned),
                SealEnding::Short {
                    ended_after: k as u64
                }
            );
            // Entries 0 to K are the plan's; its own entry at K+1 is bare.
            let plan = planned_seal_copy(m);
            assert_eq!(short.files.len(), (k + 2) as usize);
            for (got, want) in short.files.iter().zip(&plan).take(k as usize + 1) {
                assert_eq!(got.position, want.position);
                assert_eq!(got.type_label, want.type_label);
                assert_eq!(got.size_bytes, want.size_bytes);
                assert_eq!(got.sha256_encrypted, want.sha256_encrypted);
            }
            let own = short.files.last().unwrap();
            assert_eq!(own.position, k + 1);
            assert_eq!(own.type_label, "seal_marker");
            assert_eq!(own.size_bytes, None);
            assert_eq!(own.sha256_encrypted, None);
        }
    }

    /// §5 "Length": `build` sizes the seal with the full seal as a
    /// placeholder, and a short seal must never be longer (the writer checks
    /// `<=`, the full seal `==`). Every K from the first data slice to M-2,
    /// over plans whose M crosses a digit boundary (9→10, 99→100, 999→1000),
    /// where the short seal's numbers could be shortest relative to the
    /// full one's.
    #[test]
    fn a_short_seal_is_never_longer_than_the_placeholder_build_sized() {
        let fi = "ef".repeat(32);
        for m in [6, 7, 9, 10, 11, 99, 100, 101, 999, 1000, 1001] {
            let placeholder = generate_seal_marker("LEN001", m + 1, &fi, &planned_seal_copy(m));
            for k in 5..=m - 2 {
                let short = generate_short_seal_marker("LEN001", k, &fi, &short_seal_copy(m, k));
                assert!(
                    short.len() <= placeholder.len(),
                    "M = {m}, K = {k}: the short seal ({} bytes) is longer than the \
                     placeholder ({} bytes)",
                    short.len(),
                    placeholder.len()
                );
            }
        }
    }

    #[test]
    fn seal_marker_timestamp_is_fixed_width() {
        // Load-bearing for the build/seal placeholder-sizing trick
        // (v2-open-questions.md §9): build sizes the seal with a placeholder
        // timestamp, seal() regenerates with the real sealed_at, and the two
        // MUST be byte-length identical. chrono's plain to_rfc3339() is
        // AutoSi (drops trailing-zero fractional digits -> 25/29/32/35-byte
        // outputs), which would break that ~1 write in 900. SecondsFormat::Secs
        // + use_z renders exactly 20 bytes, always.
        let files = planned_seal_copy(7);
        let first = generate_seal_marker("TEST01", 8, "fi", &files);
        let first_short = generate_short_seal_marker("TEST01", 5, "fi", &files[..6]);
        for _ in 0..64 {
            let again = generate_seal_marker("TEST01", 8, "fi", &files);
            assert_eq!(
                first.len(),
                again.len(),
                "seal marker length varies between generations — the placeholder \
                 sizing trick (and therefore seal()) is broken"
            );
            let again = generate_short_seal_marker("TEST01", 5, "fi", &files[..6]);
            assert_eq!(first_short.len(), again.len(), "the short seal too");
        }
        // Pin the exact rendering too, so a future edit to the format is a
        // deliberate act rather than an accident.
        let body = &first[first.find("[seal]").unwrap()..];
        let parsed: toml::Value = body.parse().expect("TOML parses");
        let sealed_at = parsed
            .get("seal")
            .unwrap()
            .get("sealed_at")
            .unwrap()
            .as_str()
            .unwrap();
        assert_eq!(sealed_at.len(), 20, "sealed_at must be exactly 20 bytes");
        assert!(sealed_at.ends_with('Z'), "sealed_at must be Z-suffixed UTC");
        assert!(chrono::DateTime::parse_from_rfc3339(sealed_at).is_ok());
    }

    #[test]
    fn seal_marker_embeds_front_index_copy() {
        // The embedded copy (ratified 2026-07-22) is MORE complete than File 3:
        // by seal time File 3's own size + hash are known, so its entry is
        // filled in; only the seal marker's own entry stays hash-less.
        let files = vec![
            FrontIndexFile {
                position: 0,
                type_label: "id_thunk",
                size_bytes: Some(500),
                sha256_encrypted: Some("aa00".into()),
            },
            FrontIndexFile {
                position: 1,
                type_label: "system_guide",
                size_bytes: Some(600),
                sha256_encrypted: Some("aa01".into()),
            },
            FrontIndexFile {
                position: 2,
                type_label: "restore_sh",
                size_bytes: Some(700),
                sha256_encrypted: Some("aa02".into()),
            },
            FrontIndexFile {
                position: 3,
                type_label: "front_index",
                size_bytes: Some(2048),                // known at seal time
                sha256_encrypted: Some("fi99".into()), // known at seal time
            },
            FrontIndexFile {
                position: 4,
                type_label: "data_slice",
                size_bytes: Some(524288),
                sha256_encrypted: Some("bb11".into()),
            },
            FrontIndexFile {
                position: 5,
                type_label: "seal_marker",
                size_bytes: None,
                sha256_encrypted: None, // self-reference: never hashable
            },
        ];
        let s = generate_seal_marker("TEST01", 6, "fi99", &files);
        let body = &s[s.find("[seal]").unwrap()..];
        let parsed: toml::Value = body.parse().expect("TOML parses");
        let arr = parsed.get("files").unwrap().as_array().unwrap();
        assert_eq!(arr.len(), 6);
        // File 3's entry in the COPY carries size + hash (unlike in File 3).
        assert_eq!(arr[3].get("type").unwrap().as_str(), Some("front_index"));
        assert_eq!(arr[3].get("size_bytes").unwrap().as_integer(), Some(2048));
        assert_eq!(
            arr[3].get("sha256_encrypted").unwrap().as_str(),
            Some("fi99")
        );
        // The seal marker's own entry stays hash-less.
        assert_eq!(arr[5].get("type").unwrap().as_str(), Some("seal_marker"));
        assert!(arr[5].get("sha256_encrypted").is_none());
        // A consistent 1.6.0 seal: it parses by §5's rules.
        assert!(crate::volume::format::parse_seal_marker(&s).is_ok());
        // The embedded copy's grammar matches the front index's byte-for-byte
        // (same emitter): the [[files]] tail of both documents is identical.
        let fi = generate_front_index("TEST01", &files);
        let fi_tail = &fi[fi.find("\n[[files]]").unwrap()..];
        let seal_tail = &s[s.find("\n[[files]]").unwrap()..];
        assert_eq!(fi_tail, seal_tail);
    }

    #[test]
    fn planning_header_embeds_unit_rows() {
        let units = vec![
            ("alpha".to_string(), "uuid-a".to_string(), 3, 10_000),
            ("beta".to_string(), "uuid-b".to_string(), 1, 500),
        ];
        let s = generate_planning_header("LAB01", &units);
        // Must be valid TOML with TWO distinct [[units]] entries — the old
        // single-[[units]] form produced duplicate keys and failed to parse
        // for any multi-unit volume (T14).
        let parsed: toml::Value = s.parse().expect("planning header must be valid TOML");
        assert_eq!(parsed["planning"]["volume"].as_str(), Some("LAB01"));
        let units_arr = parsed["units"].as_array().expect("units array");
        assert_eq!(units_arr.len(), 2);
        assert_eq!(units_arr[0]["name"].as_str(), Some("alpha"));
        assert_eq!(units_arr[0]["num_slices"].as_integer(), Some(3));
        assert_eq!(units_arr[0]["total_bytes"].as_integer(), Some(10_000));
        assert_eq!(units_arr[1]["name"].as_str(), Some("beta"));
        assert_eq!(units_arr[1]["uuid"].as_str(), Some("uuid-b"));
    }

    /// #312: the envelope is sealed at WRITE time and each slice at STAGE
    /// time (`build.rs` vs `staging/mod.rs`), so a `key rotate` between the
    /// two leaves no single key that opens both (#288, on a real tape).
    /// RECOVERY.md is read by someone who has already opened the envelope —
    /// the manual path must not let them conclude that key is the only one
    /// the slices can need. Each claim below was measured against the real
    /// `age` CLI: several `-i` are accepted at once, one unreadable `-i`
    /// path fails the whole command, and a missing key reports "no identity
    /// matched any of the recipients" (it does not fail silently).
    #[test]
    fn recovery_md_says_the_slices_may_need_a_different_key() {
        let units = vec![ManifestUnit {
            name: "alpha".into(),
            uuid: "uuid-a".into(),
            snapshot_version: 1,
            stage_set_id: 1,
            payload: "tar".to_string(),
            stream_bytes: 0,
            file_table: None,
            id_map: None,
            tar_version: None,
            compression: None,
            slices: vec![ManifestSlice {
                continues: false,
                number: 1,
                tape_position: Some(4),
                size_bytes: 1,
                encrypted_bytes: 2,
                sha256_plain: "abc".into(),
                sha256_encrypted: "def".into(),
            }],
        }];
        let s = generate_recovery_md("LAB01", "alice", &units, &[]);
        assert!(
            s.contains("not necessarily the key that opens the slices"),
            "must say the envelope's key may not open the slices"
        );
        assert!(
            s.contains("-i OLD.age.key -i NEW.age.key"),
            "must show age taking several identities at once"
        );
        assert!(
            s.contains("one unreadable `-i` path fails the whole command"),
            "must warn that naming a missing key file fails age outright"
        );
        assert!(
            s.contains("no identity matched any of the recipients"),
            "must name the error the reader will actually see"
        );
        // The old troubleshooting claim was false: age does not fail silently.
        assert!(
            !s.contains("silently fails"),
            "stale 'silently fails' claim"
        );
    }

    /// #130: every heir-facing document hands out literal `mt -f /dev/nst0`
    /// commands, and `/dev/nst0` is a guess about the heir's machine rather
    /// than a fact about the tape. It was wrong on this very dev VM after a
    /// routine reboot, where a real LTO-6 and mhvtl swapped node numbers —
    /// and a wrong device is not reliably loud: with another tape loaded, the
    /// commands succeed and recover the WRONG volume.
    ///
    /// These are frozen plaintext zones that cannot check anything at read
    /// time the way RESTORE.sh now does, so the caveat is all they get. The
    /// issue named two documents; RECOVERY.md is a third with the same defect.
    #[test]
    fn every_heir_document_says_the_device_may_not_be_nst0() {
        let params = IdThunkV2Params {
            label: "DEV130",
            uuid: "11111111-2222-3333-4444-555555555555",
            media_type: "LTO-6",
            tapectl_version: "0.2.0",
            nominal_capacity: 2_500_000_000_000,
            mam_capacity: 2_400_000_000_000,
            total_files: 27,
            mam_manufacturer: "IBM",
            mam_serial: "SERIAL1",
            mam_length: 846,
            mam_loads: 5,
            created_at: "2026-07-22T20:09:00Z",
            cartridge_identity_source: None,
        };
        let units = vec![ManifestUnit {
            name: "alpha".into(),
            uuid: "uuid-a".into(),
            snapshot_version: 2,
            stage_set_id: 7,
            payload: "tar".to_string(),
            stream_bytes: 0,
            file_table: None,
            id_map: None,
            tar_version: None,
            compression: None,
            slices: vec![ManifestSlice {
                continues: false,
                number: 1,
                tape_position: Some(4),
                size_bytes: 1_048_576,
                encrypted_bytes: 1_049_000,
                sha256_plain: "abc".into(),
                sha256_encrypted: "def456".into(),
            }],
        }];

        for (what, doc) in [
            ("ID thunk (File 0)", generate_id_thunk_v2(&params)),
            (
                "system guide (File 1)",
                generate_system_guide_v2("DEV130", 27),
            ),
            (
                "RECOVERY.md",
                generate_recovery_md("DEV130", "alice", &units, &[]),
            ),
        ] {
            // The runnable example stays — an heir needs something to type.
            assert!(
                doc.contains("/dev/nst0"),
                "{what} should keep a concrete example device"
            );
            // But it must say the example may be wrong, and how to find out.
            assert!(
                doc.contains("ls -l /dev/tape/by-id/"),
                "{what} must tell the heir how to list the real drives"
            );
            assert!(
                doc.contains("example"),
                "{what} must mark /dev/nst0 as an example, not a fact"
            );
        }
    }

    /// #134: the envelope manifest used to carry `layout_version = 1` on a v2
    /// tape. It was a v1-era constant the format flip never touched — nothing
    /// has ever read it, and it contradicted the ID thunk, the front index and
    /// the seal marker, all of which say 2. An heir comparing the two would
    /// conclude the tape was inconsistent when it is not.
    ///
    /// Deleted rather than bumped: a field no reader consults cannot be kept
    /// honest, which is the same trap as `meta.schema_version` (#61). This
    /// test exists so it cannot drift back in.
    #[test]
    fn the_envelope_manifest_carries_no_layout_version() {
        let s = generate_manifest_toml("LAB01", "alice", &[]);
        assert!(
            !s.contains("layout_version"),
            "the envelope manifest must not claim a layout version — the tape's \
             own zones are authoritative:\n{s}"
        );
        let parsed: toml::Value = s.parse().expect("manifest must still be valid TOML");
        let m = parsed.get("manifest").expect("[manifest] table survives");
        assert_eq!(m.get("volume").unwrap().as_str(), Some("LAB01"));
        assert_eq!(m.get("tenant").unwrap().as_str(), Some("alice"));
    }

    #[test]
    fn manifest_toml_round_trips_slices() {
        let units = vec![ManifestUnit {
            name: "alpha".into(),
            uuid: "uuid-a".into(),
            snapshot_version: 1,
            stage_set_id: 7,
            payload: "tar".to_string(),
            stream_bytes: 0,
            file_table: None,
            id_map: None,
            tar_version: None,
            compression: None,
            slices: vec![ManifestSlice {
                continues: false,
                number: 1,
                tape_position: Some(4),
                size_bytes: 1_048_576,
                encrypted_bytes: 1_049_000,
                sha256_plain: "abc".into(),
                sha256_encrypted: "def".into(),
            }],
        }];
        let s = generate_manifest_toml("LAB01", "alice", &units);
        let parsed: toml::Value = s.parse().expect("manifest parses as TOML");
        let m = parsed.get("manifest").unwrap();
        assert_eq!(m.get("volume").unwrap().as_str(), Some("LAB01"));
        assert_eq!(m.get("tenant").unwrap().as_str(), Some("alice"));
        let u = &parsed.get("units").unwrap().as_array().unwrap()[0];
        assert_eq!(u.get("name").unwrap().as_str(), Some("alpha"));
        assert_eq!(u.get("payload").unwrap().as_str(), Some("tar"));
        // #39: provenance fields for selective restore.
        assert_eq!(u.get("stage_set_id").unwrap().as_integer(), Some(7));
        assert!(u.get("dar_version").is_none() && u.get("dar_command").is_none());
        let slice = &u.get("slices").unwrap().as_array().unwrap()[0];
        assert_eq!(slice.get("number").unwrap().as_integer(), Some(1));
        assert_eq!(slice.get("tape_position").unwrap().as_integer(), Some(4));
        assert_eq!(slice.get("sha256_plain").unwrap().as_str(), Some("abc"));
    }

    #[test]
    fn id_thunk_v2_parses_with_only_v2_layout_fields() {
        let params = IdThunkV2Params {
            label: "TEST01",
            uuid: "11111111-2222-3333-4444-555555555555",
            media_type: "LTO-6",
            tapectl_version: "0.2.0",
            nominal_capacity: 2_500_000_000_000,
            mam_capacity: 2_400_000_000_000,
            total_files: 27,
            mam_manufacturer: "IBM",
            mam_serial: "SERIAL1",
            mam_length: 846,
            mam_loads: 5,
            created_at: "2026-07-22T20:09:00Z",
            cartridge_identity_source: None,
        };
        let s = generate_id_thunk_v2(&params);
        let toml_start = s.find("[volume]").expect("has [volume] section");
        let body = &s[toml_start..];
        let parsed: toml::Value = body.parse().expect("TOML parses");

        let volume = parsed.get("volume").unwrap();
        assert_eq!(
            volume.get("magic").unwrap().as_str(),
            Some("tapectl-volume-v3")
        );
        assert_eq!(volume.get("label").unwrap().as_str(), Some("TEST01"));
        assert_eq!(
            volume.get("uuid").unwrap().as_str(),
            Some("11111111-2222-3333-4444-555555555555")
        );
        assert_eq!(volume.get("layout_version").unwrap().as_integer(), Some(3));

        // [layout] carries ONLY front_index, seal_marker, total_files (sheet
        // §2.3) — every v1 position field is gone entirely, not just unset.
        let layout = parsed.get("layout").unwrap();
        let layout_table = layout.as_table().expect("[layout] is a table");
        assert_eq!(
            layout_table.len(),
            3,
            "[layout] must carry exactly 3 keys, found: {:?}",
            layout_table.keys().collect::<Vec<_>>()
        );
        assert_eq!(layout.get("front_index").unwrap().as_integer(), Some(3));
        assert_eq!(layout.get("seal_marker").unwrap().as_integer(), Some(26)); // total_files - 1
        assert_eq!(layout.get("total_files").unwrap().as_integer(), Some(27));
        for v1_key in [
            "data_start",
            "data_end",
            "mini_index",
            "first_envelope",
            "num_envelopes",
            "operator_envelope",
            "operator_envelope_backup",
        ] {
            assert!(
                layout.get(v1_key).is_none(),
                "v1 position field '{v1_key}' must be absent from the v2 [layout] table"
            );
        }

        let media = parsed.get("media").unwrap();
        assert_eq!(
            media.get("cartridge_serial").unwrap().as_str(),
            Some("SERIAL1")
        );
    }

    /// File 0 is the first thing a finder reads, and its "read the guide"
    /// command must strip the NUL block padding and nothing else. It is in a
    /// raw string, where `'\\0'` stays two backslashes: `tr -d '\\0'` then
    /// deleted every backslash and every digit 0 and kept the NULs ("2026"
    /// became "226"). Found 2026-09-28 by the docs pass. This runs the
    /// thunk's own `tr` words through a shell, so it tests what the reader
    /// types, not how the source spells it.
    #[test]
    fn id_thunk_guide_command_strips_nuls_and_keeps_zeros() {
        let params = IdThunkV2Params {
            label: "TEST01",
            uuid: "11111111-2222-3333-4444-555555555555",
            media_type: "LTO-6",
            tapectl_version: "0.2.0",
            nominal_capacity: 2_500_000_000_000,
            mam_capacity: 2_400_000_000_000,
            total_files: 27,
            mam_manufacturer: "IBM",
            mam_serial: "SERIAL1",
            mam_length: 846,
            mam_loads: 5,
            created_at: "2026-07-22T20:09:00Z",
            cartridge_identity_source: None,
        };
        let s = generate_id_thunk_v2(&params);
        let tr_cmds: Vec<&str> = s
            .lines()
            .filter(|l| l.contains("GUIDE.md") && l.contains("tr -d"))
            .map(|l| {
                let from = l.find("tr -d").unwrap();
                let to = l[from..].find(" >").map(|i| from + i).unwrap_or(l.len());
                &l[from..to]
            })
            .collect();
        assert_eq!(tr_cmds.len(), 2, "both guide-read commands found: {s}");
        for tr in tr_cmds {
            let out = std::process::Command::new("sh")
                .arg("-c")
                .arg(format!("printf 'Guide 2026 v0.1\\n\\0\\0\\0' | {tr}"))
                .output()
                .expect("sh runs");
            assert_eq!(
                out.stdout, b"Guide 2026 v0.1\n",
                "`{tr}` must remove the NUL padding and keep every 0"
            );
        }
    }

    #[test]
    fn id_thunk_v2_is_byte_identical_across_two_calls_given_the_same_created_at() {
        // T6 review finding #5: before `created_at` was injectable, the ID
        // thunk read the clock internally, so two `generate_id_thunk_v2`
        // calls could never be compared for byte-identity in a test — only
        // `system_guide`/`restore_sh` (2 of ~9 zone kinds) were checkable,
        // leaving layout-session.md's "same inputs + same generation
        // timestamp ⇒ reproducible Layout" clause unverified for this zone.
        // With the timestamp injected, this now holds directly.
        let params = IdThunkV2Params {
            label: "DETERM1",
            uuid: "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee",
            media_type: "LTO-6",
            tapectl_version: "0.2.0",
            nominal_capacity: 2_500_000_000_000,
            mam_capacity: 2_400_000_000_000,
            total_files: 12,
            mam_manufacturer: "IBM",
            mam_serial: "SERIAL9",
            mam_length: 846,
            mam_loads: 1,
            created_at: "2026-07-22T20:09:00Z",
            cartridge_identity_source: None,
        };
        let a = generate_id_thunk_v2(&params);
        let b = generate_id_thunk_v2(&params);
        assert_eq!(
            a, b,
            "id thunk must be byte-identical across two calls with the same created_at"
        );
    }

    /// THE COMPATIBILITY GUARANTEE for every tape already written (issue
    /// #192). `cartridge_identity_source` is emitted only when it is known;
    /// with `None` the ID thunk must be byte-for-byte what it was before the
    /// field existed — not "equivalent TOML", not "the same minus a blank
    /// line". A stray newline here would be invisible to every TOML parser
    /// and would still change File 0's size and hash in the front index, so
    /// the pin is on the exact bytes.
    ///
    /// The constant below was taken from the generator BEFORE the field was
    /// added. It is not a golden-file re-pin of the on-tape format (File 0 is
    /// deliberately unpinned, `tests/on_tape_golden.rs`) — it pins only that
    /// the ABSENT case is unchanged. If a later change to the ID thunk makes
    /// this fail, that later change is what must be justified.
    #[test]
    fn id_thunk_with_no_identity_source_is_byte_identical_to_the_pre_field_output() {
        use sha2::{Digest, Sha256};

        let params = IdThunkV2Params {
            label: "COMPAT1",
            uuid: "11111111-2222-3333-4444-555555555555",
            media_type: "LTO-6",
            tapectl_version: "0.2.0",
            nominal_capacity: 2_500_000_000_000,
            mam_capacity: 2_400_000_000_000,
            total_files: 27,
            mam_manufacturer: "IBM",
            mam_serial: "SERIAL1",
            mam_length: 846,
            mam_loads: 5,
            created_at: "2026-07-22T20:09:00Z",
            cartridge_identity_source: None,
        };
        let rendered = generate_id_thunk_v2(&params);
        // 2026-09-28: the two guide-read commands were corrected from
        // `tr -d '\\0'` (deleted every 0 digit, kept the NULs) to
        // `tr -d '\0'` — see `id_thunk_guide_command_strips_nuls_and_keeps_zeros`.
        // The pin keeps its meaning: undo exactly that correction and the
        // bytes must still be the pre-field output, so nothing else moved.
        let good = "tr -d '\\0' > GUIDE.md";
        assert_eq!(rendered.matches(good).count(), 2, "{rendered}");
        // 1.1.0 (ADR-0012 amendment item 15): File 0 states `requires`, one
        // line after layout_version; Layout 3 states it empty and moves the
        // magic and the layout version (D1). Undo exactly those too.
        let requires = "layout_version = 3\nrequires = []\n";
        assert_eq!(rendered.matches(requires).count(), 1, "{rendered}");
        let magic = "magic = \"tapectl-volume-v3\"\n";
        assert_eq!(rendered.matches(magic).count(), 1, "{rendered}");
        let pre_fix = rendered
            .replace(good, "tr -d '\\\\0' > GUIDE.md")
            .replace(requires, "layout_version = 2\n")
            .replace(magic, "magic = \"tapectl-volume-v2\"\n");
        let mut h = Sha256::new();
        h.update(pre_fix.as_bytes());
        let digest = format!("{:x}", h.finalize());
        assert_eq!(
            digest, "1fb03cbe041201afb2f47c77f60c1504eb94d8ea14df2d53b9ffae8f45fcd373",
            "the absent case must render exactly as it did before \
             cartridge_identity_source existed (apart from the tr correction)"
        );

        // ...and the present case differs by exactly one inserted line, in
        // exactly one place: immediately after `cartridge_serial`, inside
        // `[media]` (`docs/design/volume-format-v3.md` §1.1).
        let with_source = generate_id_thunk_v2(&IdThunkV2Params {
            cartridge_identity_source: Some("mam"),
            ..params
        });
        assert_eq!(
            with_source,
            rendered.replace(
                "cartridge_serial = \"SERIAL1\"\n",
                "cartridge_serial = \"SERIAL1\"\ncartridge_identity_source = \"mam\"\n",
            ),
            "the field must be one line, directly after cartridge_serial, and change \
             nothing else"
        );
    }

    #[test]
    fn system_guide_v2_covers_front_index_seal_disclosure_and_zero_strip() {
        let s = generate_system_guide_v2("LAB01", 42);
        assert!(s.contains("Volume: LAB01"));
        assert!(s.contains("File 3"));
        assert!(s.contains("seal marker"));
        // §2 "Accepted disclosure": the size-inference line must be stated
        // plainly, not hedged away.
        assert!(s.contains("Accepted size disclosure"));
        assert!(s.contains("reveals unit boundaries"));
        // §3.3/§3.4: the zero-strip procedure and the degradation ladder.
        assert!(s.contains("## If All Else Fails"));
        assert!(s.contains("zero-strip"));
        assert!(s.contains("Total files on this tape: 42"));
    }

    #[test]
    fn v2_generators_never_mention_mini_index() {
        // There is no mid-tape mini-index; its facts are in the front index
        // (File 3). Neither v2 generator
        // may reference it under either spelling, in any case.
        let guide = generate_system_guide_v2("MINI1", 12).to_lowercase();
        for needle in ["mini-index", "mini_index"] {
            assert!(
                !guide.contains(needle),
                "v2 guide must not mention '{needle}'"
            );
        }
    }

    #[test]
    fn on_tape_recovery_text_uses_the_write_paths_block_size() {
        // Issue #121: the recovery text an heir reads off the tape must stay
        // in lockstep with the write path's actual block size
        // (`cli::volume::DEFAULT_BLOCK_SIZE`), not a hand-typed literal that
        // could silently drift if the write path's block size ever changes
        // without re-templating these generators. This doesn't replace the
        // literal-asserting tests above (`restore_script_v2_has_all_modes_
        // and_rung2_fallback`'s `BLOCK=524288`, `tenant_recovery_md_has_
        // working_dar_recipe`'s `setblk 524288`) -- it adds the tripwire: if
        // DEFAULT_BLOCK_SIZE ever changes without this text changing too,
        // this test is what catches it.
        let guide = generate_system_guide_v2("LOCK01", 20);
        assert!(guide.contains(&format!(
            "setblk {}",
            crate::cli::volume::DEFAULT_BLOCK_SIZE
        )));
    }

    /// Issue #349: File 1 is where an heir learns what to install before
    /// touching the tape, so it must name `tar` wherever it lists the tools.
    #[test]
    fn the_guide_names_tar_among_the_tools() {
        let guide = generate_system_guide_v2("TOOLS1", 20);
        let intro = guide
            .split_once("All you need is:")
            .and_then(|(_, rest)| rest.split_once("\n\n"))
            .map(|(list, _)| list)
            .expect("the guide states its tool list up front");
        assert!(intro.contains("tar"), "intro tool list omits tar: {intro}");
        let section = guide
            .split_once("## Tools Required")
            .and_then(|(_, rest)| rest.split_once("\n## "))
            .map(|(body, _)| body)
            .expect("the guide has a Tools Required section");
        assert!(
            section.lines().any(|l| l.starts_with("- `tar`")),
            "Tools Required must list tar:\n{section}"
        );
    }

    // ---- the 2026-09-30 heir-path batch (ADR-0012 2026-09-30 amendment) ----

    // ---- #405 part 2: the rung-3 zero-strip recipe ----

    /// The recipe's shell lines, exactly as an heir reads them: every line
    /// from `size=$(stat` through `> candidate.stripped` of `text`.
    fn zero_strip_recipe(text: &str) -> String {
        let lines: Vec<&str> = text.lines().map(str::trim).collect();
        let start = lines
            .iter()
            .position(|l| l.starts_with("size=$(stat"))
            .expect("the recipe starts with size=$(stat ...)");
        let end = lines[start..]
            .iter()
            .position(|l| l.ends_with("> candidate.stripped"))
            .expect("the recipe ends writing candidate.stripped")
            + start;
        lines[start..=end].join("\n") + "\n"
    }

    /// Issue #405: the rung-3 recipe used to be `tr -d '\0'`, which deletes
    /// EVERY zero byte, and real age ciphertext has hundreds of them inside
    /// it; age then fails to authenticate. The recipe now strips only the
    /// trailing block padding. Run here, as printed, on real age ciphertext
    /// with interior zero bytes, padded the way the tape pads it — including
    /// a ciphertext that itself ends in a zero byte, which the "put one back
    /// and retry" step exists for — and an envelope-sized file smaller than
    /// one block. Both the script's text and the guide's are run.
    #[test]
    fn the_zero_strip_recipe_keeps_interior_zero_bytes_and_decrypts() {
        use std::io::{Read, Write};

        let kp = crate::crypto::keys::generate_keypair();
        let identity: age::x25519::Identity = kp.secret_key.parse().unwrap();
        let encrypt = |plain: &[u8]| -> Vec<u8> {
            let enc =
                crate::staging::build_encryptor(std::slice::from_ref(&kp.public_key)).unwrap();
            let mut out = Vec::new();
            let mut w = enc.wrap_output(&mut out).unwrap();
            w.write_all(plain).unwrap();
            w.finish().unwrap();
            out
        };
        let decrypt = |ct: &[u8]| -> Option<Vec<u8>> {
            let d = age::Decryptor::new(ct).ok()?;
            let mut r = d
                .decrypt(std::iter::once(&identity as &dyn age::Identity))
                .ok()?;
            let mut out = Vec::new();
            r.read_to_end(&mut out).ok()?;
            Some(out)
        };

        // Large: hundreds of interior zero bytes. Small: under one block.
        let big: Vec<u8> = (0..200_000u32)
            .map(|i| (i.wrapping_mul(2654435761) >> 13) as u8)
            .collect();
        let small = b"MANIFEST.toml and friends".to_vec();
        let mut cases = vec![
            (big.clone(), encrypt(&big)),
            (small.clone(), encrypt(&small)),
        ];
        // And one whose ciphertext ends in a zero byte (1 in 256 do).
        let ends_in_zero = (0..20_000)
            .map(|_| encrypt(&small))
            .find(|ct| ct.last() == Some(&0))
            .expect("one ciphertext in 20000 ends in a zero byte");
        cases.push((small.clone(), ends_in_zero));
        let interior = cases[0].1[..cases[0].1.len() - 1]
            .iter()
            .filter(|b| **b == 0)
            .count();
        assert!(
            interior > 100,
            "the big case must carry interior zeros: {interior}"
        );
        // Why: the recipe before 1.1.0, `tr -d '\0'`, cannot decrypt it.
        let all_zeros_deleted: Vec<u8> = cases[0].1.iter().copied().filter(|b| *b != 0).collect();
        assert!(
            decrypt(&all_zeros_deleted).is_none(),
            "tr -d destroys the ciphertext"
        );

        let dir = std::env::temp_dir().join(format!("zerostrip-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let guide = generate_system_guide_v2("ZS01", 20);
        for (source, text) in [("guide", guide.as_str())] {
            let recipe = zero_strip_recipe(text);
            assert!(!recipe.contains("tr -d"), "{source}: {recipe}");
            for (i, (plain, ct)) in cases.iter().enumerate() {
                let work = dir.join(format!("case-{source}-{i}"));
                std::fs::create_dir_all(&work).unwrap();
                let mut padded = ct.clone();
                padded.resize(ct.len().div_ceil(524_288) * 524_288, 0);
                std::fs::write(work.join("candidate.raw"), &padded).unwrap();
                let o = std::process::Command::new("bash")
                    .arg("-c")
                    .arg(&recipe)
                    .current_dir(&work)
                    .output()
                    .unwrap();
                assert!(o.status.success(), "{source} case {i}: {o:?}");
                let mut stripped = std::fs::read(work.join("candidate.stripped")).unwrap();
                assert!(
                    stripped.len() <= ct.len() && ct.starts_with(&stripped),
                    "{source} case {i}: the strip must only cut the tail"
                );
                // Step 4: decrypt; on failure put one zero back, up to four times.
                let mut got = decrypt(&stripped);
                for _ in 0..4 {
                    if got.is_some() {
                        break;
                    }
                    stripped.push(0);
                    got = decrypt(&stripped);
                }
                assert_eq!(got.as_deref(), Some(plain.as_slice()), "{source} case {i}");
            }
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Whitespace-flattened, so a phrase wrapped across lines still matches.
    fn flat_text(s: &str) -> String {
        s.split_whitespace().collect::<Vec<_>>().join(" ")
    }

    /// #448's File 1 half (ADR-0012 2026-10-16 item 71): File 1 says which
    /// drives read THIS tape's generation, from `Generation::can_read`, and
    /// the interface. An LTO-6 tape reads in an LTO-6 or LTO-7 drive and in
    /// nothing newer; an LTO-8 tape in an LTO-8 or LTO-9 drive. A media_type
    /// the crate does not know gets the general statement, not a guess.
    #[test]
    fn system_guide_v2_names_the_drives_that_read_this_tapes_generation() {
        let g6 = flat_text(&generate_system_guide_v2_for_media("L6", 20, "LTO-6"));
        assert!(g6.contains("This tape is LTO-6"), "{g6}");
        assert!(g6.contains("only if it is an LTO-6 or LTO-7 drive"), "{g6}");
        assert!(
            !g6.contains("LTO-8 or LTO-9 drive"),
            "not for an LTO-6 tape"
        );
        let g8 = flat_text(&generate_system_guide_v2_for_media("L8", 20, "LTO-8"));
        assert!(g8.contains("This tape is LTO-8"), "{g8}");
        assert!(g8.contains("only if it is an LTO-8 or LTO-9 drive"), "{g8}");
        assert!(
            !g8.contains("LTO-6 or LTO-7 drive"),
            "not for an LTO-8 tape"
        );
        // Type M media is read by an LTO-8 drive alone.
        let m8 = flat_text(&generate_system_guide_v2_for_media("M8", 20, "LTO-7-M8"));
        assert!(m8.contains("only if it is an LTO-8 drive"), "{m8}");

        for (what, g) in [("LTO-6", &g6), ("LTO-8", &g8)] {
            // A newer drive often cannot read an older tape, with the example.
            assert!(
                g.contains("an LTO-8 drive, for example, reads only LTO-7, LTO-7 Type M and LTO-8"),
                "{what}: {g}"
            );
            assert!(
                g.contains("SAS (some models by Fibre Channel)")
                    && g.contains("host adapter (HBA)")
                    && g.contains("no USB or SATA tape drive"),
                "{what}: the interface: {g}"
            );
        }

        // An unrecognised media_type, and the 2-argument entry point: the
        // general statement, never a drive list for a tape it cannot name.
        for unknown in ["", "DLT-IV", "LTO-99"] {
            let g = flat_text(&generate_system_guide_v2_for_media("U", 20, unknown));
            assert!(
                g.contains("maker's compatibility table"),
                "{unknown:?}: {g}"
            );
            assert!(!g.contains("only if it is"), "{unknown:?}: {g}");
            assert!(
                g.contains("SAS"),
                "{unknown:?}: the interface is generation-free"
            );
        }
        assert_eq!(
            generate_system_guide_v2("U", 20),
            generate_system_guide_v2_for_media("U", 20, ""),
        );
    }

    // ---- #477: File 1 for a reader who has never seen the design docs ----

    /// The section of `guide` that starts with the heading `## {title}` and
    /// runs to the next `## ` heading.
    fn guide_section<'a>(guide: &'a str, title: &str) -> &'a str {
        let head = format!("## {title}");
        let from = guide
            .find(&head)
            .unwrap_or_else(|| panic!("no section {head:?}:\n{guide}"));
        let rest = &guide[from + head.len()..];
        let to = rest.find("\n## ").unwrap_or(rest.len());
        &rest[..to]
    }

    /// #477 item 1: which device node, and how to hand it to the script and
    /// to `mt`. A rewinding node undoes every `mt fsf`.
    #[test]
    fn file_1_names_the_non_rewinding_device_and_the_environment_prefix() {
        let g = generate_system_guide_v2_for_media("DEV1", 12, "LTO-6");
        let dev = guide_section(&g, "Which device?");
        let f = flat_text(dev);
        assert!(
            f.contains("Choose the NON-REWINDING entry: the one whose name ends in `-nst`"),
            "{f}"
        );
        assert!(
            f.contains("A rewinding device rewinds the tape each time a command finishes"),
            "{f}"
        );
        // TAPE_DEVICE as an environment prefix on the command's own line.
        assert!(
            dev.lines().any(|l| l
                .trim_start()
                .starts_with("TAPE_DEVICE=/dev/nst1 ./RESTORE.sh --info")),
            "{dev}"
        );
        assert!(
            dev.contains("TAPE_DEVICE=/dev/tape/by-id/scsi-XXXXXXXX-nst ./RESTORE.sh --verify"),
            "{dev}"
        );
        // Every mt in a command line of the guide names its drive with -f (the
        // ladder's `mt fsf 3` had none), and a seek after a rewind is a
        // rewind first.
        for line in g.lines().filter(|l| l.trim_start().starts_with("mt ")) {
            assert!(line.contains("mt -f "), "an mt command without -f: {line}");
        }
        let ladder = guide_section(&g, "If All Else Fails");
        assert!(
            ladder.contains("`mt -f /dev/nst0 rewind`, then `mt -f /dev/nst0 fsf 3`"),
            "{ladder}"
        );
    }

    /// #548 T6.2-1: with several drives, how to tell which holds the tape,
    /// with read-only commands only.
    #[test]
    fn t62_1_file_1_says_how_to_tell_which_drive_holds_the_tape() {
        let g = generate_system_guide_v2_for_media("DEV1", 12, "LTO-6");
        let dev = guide_section(&g, "Which device?");
        let f = flat_text(dev);
        assert!(
            dev.lines()
                .any(|l| l.trim_start().starts_with("mt -f /dev/nst0 status")),
            "{dev}"
        );
        for needle in [
            "has a cartridge loaded",
            "\"Tape:\" line",
            "may hold someone else's tape",
            "only read-only commands",
        ] {
            assert!(f.contains(needle), "{needle:?}: {f}");
        }
    }

    /// #548 T6.2-2: the unit is listed once, so "Did the restore get
    /// everything?" says each listed unit is one, and why a line names more
    /// than one envelope.
    #[test]
    fn t62_2_file_1_says_find_envelope_lists_a_unit_once_with_its_envelopes() {
        let g = generate_system_guide_v2_for_media("DEV1", 12, "LTO-6");
        let f = flat_text(guide_section(&g, "Did the restore get everything?"));
        assert!(
            f.contains("lists (once for each unit and version, with the envelopes that list it named on its line"),
            "{f}"
        );
    }

    /// #548 T6.2-4: a short block at the very top of File 1: the commands
    /// that do the whole job, each pointing at the section that explains it,
    /// and nothing the rest of File 1 does not say (every command line of it
    /// is a line of "Automated Recovery (recommended)").
    #[test]
    fn t62_4_file_1_opens_with_the_short_way_and_its_commands_are_the_recovery_sections() {
        let g = generate_system_guide_v2_for_media("DEV1", 12, "LTO-6");
        let titles: Vec<&str> = g.lines().filter(|l| l.starts_with("## ")).collect();
        assert_eq!(titles[0], "## The short way", "{titles:?}");
        assert!(titles[1].starts_with("## Volume: "), "{titles:?}");
        assert_eq!(titles[2], "## Start here", "{titles:?}");
        let short = guide_section(&g, "The short way");
        let auto = guide_section(&g, "Automated Recovery (recommended)");
        let cmds: Vec<&str> = short
            .lines()
            .filter(|l| l.starts_with("    "))
            .map(str::trim)
            .collect();
        assert_eq!(cmds.len(), 6, "{short}");
        for c in &cmds {
            assert!(
                auto.lines().any(|l| l.trim() == *c),
                "{c:?} is not a line of Automated Recovery"
            );
        }
        let f = flat_text(short);
        for section in [
            "Which device?",
            "Automated Recovery (recommended)",
            "Which file is your key",
            "Did the restore get everything?",
            "Start here",
        ] {
            assert!(f.contains(&format!("\"{section}\"")), "{section}: {f}");
        }
    }

    /// #477 item 6: `eod` and `bsfm 2` explained, and which file is last.
    #[test]
    fn file_1_explains_eod_bsfm_and_says_the_last_file_is_n_minus_1() {
        let g = generate_system_guide_v2_for_media("SEAL1", 27, "LTO-6");
        let f = flat_text(guide_section(&g, "Manual Recovery Steps"));
        assert!(
            f.contains("`eod` (GNU mt calls it `eom`) moves to the end of the recorded data, which is just past the last file's closing filemark"),
            "{f}"
        );
        assert!(
            f.contains("`bsfm 2` backs over that filemark and the one before it and then steps forward over that one, which leaves the head at the START of the last file"),
            "{f}"
        );
        assert!(
            f.contains("This tape has 27 files, numbered 0 to 26, so the last is File 26"),
            "{f}"
        );
        assert!(
            f.contains("`mt -f /dev/nst0 fsf 26` reaches the same place"),
            "{f}"
        );
        let total = flat_text(guide_section(&g, "Total files on this tape: 27"));
        assert!(
            total.contains(
                "The files of THIS tape are numbered 0 to 26. File 26, the last, is the seal marker. Another tape of the box has its own count"
            ),
            "{total}"
        );
        assert!(
            g.contains("LAST file (File 26 on this tape): SEAL MARKER"),
            "{g}"
        );
    }

    /// #477 item 10: the hardware, and what a recovery firm can read.
    #[test]
    fn file_1_says_what_hardware_reads_the_tape_and_what_a_firm_can_read() {
        let g = generate_system_guide_v2_for_media("HW1", 12, "LTO-6");
        let f = flat_text(guide_section(&g, "The tape drive"));
        for needle in [
            "SAS comes in generations (3, 6 and 12 Gb/s)",
            "external drives commonly use SFF-8088 or the newer SFF-8644",
            "Use a plain host adapter (HBA), not a RAID controller",
            "Do not use a USB bridge: SAS-to-USB bridges for tape drives are rare and not recommended",
            "Whoever holds the escrow key, or the operator key, can read everything on every tape, for every tenant",
            "To use a firm without giving it a key, ask for the raw content of each tape file",
            "a tenant's key gives a firm that tenant's data and no one else's",
        ] {
            assert!(f.contains(needle), "{needle:?}: {f}");
        }
    }

    // ---- the 2026-10-17 fresh-reader read of File 1 (ADR-0012 item 81, #477) ----
    //
    // A reader with no design documents read 1.3.0's File 1 and found errors and
    // gaps; each fix is pinned here by the words that fix it. The commands the
    // fixes show are run against the fake tape in tests/heir_restore_sh.rs.

    fn guide17() -> String {
        generate_system_guide_v2_for_media("READ17", 30, "LTO-6")
    }

    // 1. `fsf` is relative: step 7 puts the tape at the first slice once and
    // shows the arithmetic for a gap, and no longer tells the reader to
    // `fsf` an absolute TAPE_POSITION from the middle of the tape.
    #[test]
    fn file_1_fix_1_fsf_is_relative_and_the_gap_arithmetic_is_shown() {
        let g = guide17();
        let manual = guide_section(&g, "Manual Recovery Steps");
        assert!(
            !manual.contains("only to reach a slice you are not at"),
            "the old advice is gone"
        );
        assert!(
            !manual.contains("fsf TAPE_POSITION"),
            "no absolute fsf from where the head is"
        );
        // #485 item 8: the placeholder is the manifest field's name, and the
        // front-index placeholder (`position`) is not confused with it.
        assert!(
            !manual.contains("FIRST_POSITION") && !manual.contains("TAPE_POSITION of"),
            "the position placeholders follow the manifest's `tape_position`"
        );
        let f = flat_text(manual);
        for needle in [
            "`fsf N` moves forward N files from where the head is, so from the start of the tape it reaches File N: the `tape_position` of the first slice, from the manifest (FIRST_TAPE_POSITION below)",
            "mt -f /dev/nst0 fsf FIRST_TAPE_POSITION",
            "`fsf` takes the number of files to skip, not a file number",
            "after reading the slice in File P the head is at the start of File P+1",
            "mt -f /dev/nst0 fsf COUNT # COUNT = Q - P - 1",
            "(from File 6 to File 9 that is `fsf 2`)",
        ] {
            assert!(f.contains(needle), "{needle:?}: {f}");
        }
    }

    // 2. /tmp: said once, in "Disk space", that it may be a small RAM disk and
    // TMPDIR moves the work directory; step 7 agrees and does not contradict.

    // 3. The damaged-tape contradictions: (a) the front index lists the
    // slices an unsealed tape lacks; (b) a disagreement of the ends is
    // reconciled with "restoring is sound"; (c) an escrow-key holder is told
    // how to get the slice table; (d) RESTORE.sh's own fallback is stated.

    // 4. A restore with no --unit and no --all: one unit restores, several are
    // listed and refused, and "the largest envelope" is defined.

    // 5. Versions: the option is named with the modes that take it, and a
    // version is a full archive, so the newest alone is complete; `[InRef]`
    // is explained.

    // 6. The completeness check for --path and --all.

    // 7. The zero-byte retry has a command and is cumulative.
    #[test]
    fn file_1_fix_7_the_zero_byte_retry_is_a_command_and_adds_up() {
        let g = guide17();
        let f = flat_text(guide_section(&g, "If All Else Fails"));
        for needle in [
            "re-append a single zero byte and retry, up to four times",
            "`printf '\\0' >> candidate.stripped`, then the `age -d` line again",
            "The retries add up, they do not start over",
            "then three, then four, and no more",
        ] {
            assert!(f.contains(needle), "{needle:?}: {f}");
        }
    }

    // 8 and 9. The whole box as a short procedure; the index's `state` values.

    // 10. Which file is a key; a passphrase-protected key; the escrow key fails.
    #[test]
    fn file_1_fix_10_the_key_file_is_told_from_a_public_key() {
        let g = guide17();
        let f = flat_text(guide_section(&g, "Which file is your key"));
        for needle in [
            "A line `age1...` is the PUBLIC half (a recipient)",
            "decrypts nothing, so a file holding only that is not a key",
            "`age-keygen -y YOUR_KEY.age.key`",
            "age asks for the passphrase every time it opens the file",
            "`age -d -o YOUR_KEY.decrypted.age.key PROTECTED_FILE`",
            "If the escrow key opens nothing",
            "try the operator key and every tenant key you hold",
        ] {
            assert!(f.contains(needle), "{needle:?}: {f}");
        }
    }

    // 11. Drive and host basics.
    #[test]
    fn file_1_fix_11_drive_and_host_basics() {
        let g = guide17();
        let f = flat_text(guide_section(
            &g,
            "Loading the tape, and a drive that does not show up",
        ));
        for needle in [
            "Read the generation printed on the cartridge (L6 on its label is LTO-6) before you buy or borrow a drive",
            "they may be different generations",
            "A bare internal drive needs more: a place to sit (a 5.25-inch bay in a desktop or server, or an enclosure made for it), power from a power supply, and the internal SAS cable",
            "A PCIe SAS adapter needs a free PCIe slot",
            "IT firmware can cure it where the maker offers one for that card",
            "Slide the write-protect tab on the cartridge to the protected position",
            "`mt -f /dev/nst0 status` says ONLINE",
            "`mt -f /dev/nst0 offline` rewinds the tape and ejects it",
            "may need a cleaning cartridge",
            "`dmesg | tail`",
            "the module `st`",
            "the group `tape`",
            "`sudo usermod -aG tape YOUR_USER`",
        ] {
            assert!(f.contains(needle), "{needle:?}: {f}");
        }
    }

    // 12. Disk space up front: the capacity ceiling, the placeholder, how to
    // read the dar version and what "without its catalogue" means.

    // 13. The opening names the basic tools as well as the listed ones.

    // ADR-0012 items 101 and 102: File 1 and RECOVERY.md each have a section
    // on a damaged slice or file, with the two commands and the rule that only
    // the same unit and version is used.
    // ---- the second fresh-reader read of File 1 (2026-10-19, ADR-0012 item 81, #477) ----
    //
    // Five serious gaps (S1-S5) and nine smaller ones (M6-M14). The commands the
    // fixes show are run in tests/heir_restore_sh.rs; here are the words.

    /// S3: nothing can name a tape written later; the index does list units
    /// held only elsewhere; the box can be compared against the newest index;
    /// the Heir Kit is as old as the day it was made.
    #[test]
    fn reread2_s3_the_guide_does_not_claim_a_box_is_known_complete() {
        let g = guide17();
        let bx = flat_text(guide_section(
            &g,
            "The rest of the box: other tapes, copies and the index",
        ));
        for needle in [
            "**Is the box complete?** Nothing on a tape can name tapes written after it, so the newest tape you hold may not be the newest ever written, and no tape can tell you that",
            "including units held only on OTHER tapes",
            "a holder the index names that you do not have is a tape to look for",
            "a unit, or the newest version of a unit, whose every holder is missing is not in this box",
            "`./RESTORE.sh --where --all --key YOUR_KEY.age.key` prints all of them, with no limit",
            "The Heir Kit, if you have one (\"Words this guide uses\"), records the archive as of the day it was made",
            "one made after your newest tape was written may name tapes this box lacks",
            "You are done only when ALL of these hold; fewer is not done",
            "That is the most a box can show about itself",
            "If no holder of a unit's newest version is in the box, the highest version you can reach is the best this box can give",
        ] {
            assert!(bx.contains(needle), "{needle:?}: {bx}");
        }
        assert!(
            !bx.contains("It never names a tape written after it, and a tape it names may since have been retired or erased. So the NEWEST tape's index is the most complete: start with the newest tape you have (`./RESTORE.sh --info` prints \"Sealed at\"), and expect an older tape to know less. A tape written before 1.3.0 has no index and says so. The index is plain text in the envelope (`index/` in the unpacked envelope; RECOVERY.md shows how to read it with `awk`), so it needs no tapectl either. The whole box")
        );
        let words = flat_text(guide_section(&g, "Words this guide uses"));
        assert!(words.contains("It records the archive as of the day it was made, which may be before the newest tape was written"), "{words}");
    }

    /// M7: the drive table is `Generation::can_read`, row by row, and the
    /// sentence on one drive for a box holds.
    #[test]
    fn reread2_m7_the_box_table_of_drives_follows_the_compatibility_chart() {
        use crate::media::Generation::{self, *};
        let g = guide17();
        let sec = guide_section(&g, "The tape drive");
        let rows: Vec<(String, String)> = sec
            .lines()
            .filter(|l| l.starts_with("  LTO-"))
            .map(|l| {
                let l = l.trim();
                let at = l.find("  ").expect("two columns");
                (l[..at].to_string(), l[at..].trim().to_string())
            })
            .collect();
        assert_eq!(rows.len(), 7, "{rows:#?}");
        let all_drives = [Lto1, Lto2, Lto3, Lto4, Lto5, Lto6, Lto7, Lto8, Lto9, Lto10];
        for (media, read_by) in [
            ("LTO-5", Lto5),
            ("LTO-6", Lto6),
            ("LTO-7", Lto7),
            ("LTO-7 Type M", Lto7M8),
            ("LTO-8", Lto8),
            ("LTO-9", Lto9),
            ("LTO-10", Lto10),
        ] {
            let row = rows.iter().find(|r| r.0 == media).expect(media);
            let want: Vec<&str> = all_drives
                .iter()
                .filter(|d| Generation::can_read(**d, read_by))
                .map(|d| d.as_str())
                .collect();
            let got: Vec<String> = row
                .1
                .replace(" or ", ", ")
                .split(", ")
                .map(str::to_string)
                .collect();
            assert_eq!(got, want, "{media}");
        }
        // The prose: an LTO-7 drive reads 5, 6 and 7; an LTO-8 drive reads 7,
        // 7 Type M and 8; no drive reads both a 6 and an 8.
        let f = flat_text(sec);
        assert!(f.contains("An LTO-7 drive reads LTO-5, LTO-6 and LTO-7 cartridges, an LTO-8 drive reads LTO-7, LTO-7 Type M and LTO-8 cartridges"), "{f}");
        for (drive, reads) in [(Lto7, [Lto5, Lto6, Lto7]), (Lto8, [Lto7, Lto7M8, Lto8])] {
            for m in reads {
                assert!(Generation::can_read(drive, m), "{drive:?} {m:?}");
            }
        }
        assert!(all_drives
            .iter()
            .all(|d| !(Generation::can_read(*d, Lto6) && Generation::can_read(*d, Lto8))));
        assert!(f.contains("needs two drives, one after the other"), "{f}");
        assert!(f.contains("No drive reads every generation"), "{f}");
    }

    /// M8: how to tell a plain host adapter from a RAID one, the drive-side
    /// connector, and what a Fibre Channel drive needs.
    #[test]
    fn reread2_m8_hba_mode_connectors_and_fibre_channel() {
        let g = guide17();
        let load = flat_text(guide_section(
            &g,
            "Loading the tape, and a drive that does not show up",
        ));
        for needle in [
            "\"IT\" (initiator-target: the card hands every device to the operating system untouched)",
            "`lspci -k` names the kernel driver in use under each adapter",
            "`mpt3sas` (`mpt2sas` on older kernels) is the driver of LSI and Broadcom host adapters, and `megaraid_sas` is the driver of their RAID controllers",
            "A card on `mpt3sas` can still carry RAID (\"IR\") firmware: the maker's flash tool says which (`sas3flash -list`, or `sas2flash -list` on older cards, prints a Firmware Product ID tagged (IT) or (IR))",
        ] {
            assert!(load.contains(needle), "{needle:?}: {load}");
        }
        let drive = flat_text(guide_section(&g, "The tape drive"));
        for needle in [
            "An internal drive commonly has the SAS drive connector (SFF-8482, the one on a SAS disk)",
            "the adapter's internal port, commonly SFF-8087 or the newer SFF-8643",
            "The maker's data sheets for the drive and for the adapter name their connectors, and they, not this text, decide",
            "A Fibre Channel drive needs a Fibre Channel host adapter (a SAS adapter cannot drive it)",
            "`qla2xxx` (QLogic) and `lpfc` (Emulex)",
        ] {
            assert!(drive.contains(needle), "{needle:?}: {drive}");
        }
    }

    /// M9: no tape says which keys it was written for.
    #[test]
    fn reread2_m9_nothing_on_a_tape_names_its_keys() {
        let g = guide17();
        let key = flat_text(guide_section(&g, "Which file is your key"));
        for needle in [
            "Nothing on a tape says which keys it was written for",
            "`./RESTORE.sh --info`, File 0 and the front index name no recipient",
            "an age file does not carry the name of the key it is for",
            "So no tape can be asked which key it wants: try your key on each tape, which `--find-envelope` does",
            "A key that opens one tape of a box may not open another",
        ] {
            assert!(key.contains(needle), "{needle:?}: {key}");
        }
        // And it is true of what RESTORE.sh prints and what File 0 holds.
        let thunk = generate_id_thunk_v2(&IdThunkV2Params {
            label: "L",
            uuid: "u",
            media_type: "LTO-6",
            tapectl_version: "1.3.0",
            nominal_capacity: 1,
            mam_capacity: 1,
            total_files: 5,
            mam_manufacturer: "m",
            mam_serial: "s",
            mam_length: 1,
            mam_loads: 1,
            created_at: "t",
            cartridge_identity_source: None,
        });
        for word in ["recipient", "age1", "AGE-"] {
            assert!(
                !thunk.contains(word),
                "File 0 names no key ({word}): {thunk}"
            );
        }
    }

    /// M14: the device named for `mt` and `dd` is the one RESTORE.sh reads.
    #[test]
    fn reread2_m14_the_device_is_set_once_for_the_restore_lines() {
        let g = guide17();
        let auto = flat_text(guide_section(&g, "Automated Recovery (recommended)"));
        for needle in [
            "put your own device in the `mt` and `dd` lines below, and set `TAPE_DEVICE` to the same device once, with `export TAPE_DEVICE=/dev/nstN`",
            "The `./RESTORE.sh` lines further down read `TAPE_DEVICE`, and without it they read `/dev/nst0`, which may be another drive or another tape",
            "(the ones File 0 prints for reading this guide)",
        ] {
            assert!(auto.contains(needle), "{needle:?}: {auto}");
        }
        // It comes before the first RESTORE.sh command.
        let raw = guide_section(&g, "Automated Recovery (recommended)");
        assert!(
            raw.find("export TAPE_DEVICE=/dev/nstN").unwrap()
                < raw.find("./RESTORE.sh --info").unwrap()
        );
        let start = flat_text(guide_section(&g, "Start here"));
        assert!(
            start.contains("Set `TAPE_DEVICE` to the same device once"),
            "{start}"
        );
    }

    // ---- the third fresh-reader read of File 1 (2026-10-19, ADR-0012 item 81, #477) ----
    //
    // Two serious gaps about restoring a whole box (the box could not be
    // enumerated; "done" could be reached without opening a tape) and five cheap
    // ones. The commands are run against the fake tape in tests/heir_restore_sh.rs;
    // here are the words.

    /// M-key: the passphrase-protected key is decrypted to a NEW name, and the
    /// original is kept.
    #[test]
    fn reread3_m_key_the_decrypted_key_has_a_new_name_and_the_original_is_kept() {
        let g = guide17();
        let key = flat_text(guide_section(&g, "Which file is your key"));
        for needle in [
            "Decrypt it once into a NEW file, `age -d -o YOUR_KEY.decrypted.age.key PROTECTED_FILE` (PROTECTED_FILE is the file you were given), and give the new file's name wherever this guide says `YOUR_KEY.age.key`",
            "Keep PROTECTED_FILE as it is: `age -o` replaces a file of that name without asking, so never write the result to the name of the protected file, or of any key you have only one copy of",
            "Delete the new file when you are done",
        ] {
            assert!(key.contains(needle), "{needle:?}: {key}");
        }
        assert!(
            !key.contains("-o YOUR_KEY.age.key PROTECTED_FILE"),
            "the command that could overwrite the only key is gone"
        );
    }

    /// M-sudo: plain sudo drops TAPE_DEVICE and TMPDIR, and `--preserve-env`
    /// cannot keep TMPDIR (the loader removes it from a set-user-ID program),
    /// so the form shown sets both after sudo, with `env`.
    #[test]
    fn reread3_m_sudo_keeps_the_device_and_the_work_directory() {
        let g = guide17();
        let load = flat_text(guide_section(
            &g,
            "Loading the tape, and a drive that does not show up",
        ));
        for needle in [
            "`sudo usermod -aG tape YOUR_USER`, then log in again). Running the commands with `sudo` works too, but sudo hides your `TAPE_DEVICE` and `TMPDIR` from the script, which then reads `/dev/nst0` and `/tmp` whatever you set",
            "`--preserve-env` cannot keep `TMPDIR` (the system's dynamic loader removes it from a set-user-ID program such as sudo before sudo starts: ld.so(8))",
            "Give both after sudo, with `env`: `sudo env TAPE_DEVICE=/dev/nst0 TMPDIR=/tmp ./RESTORE.sh --info`",
            "(`sudo --preserve-env=TAPE_DEVICE` keeps the device alone; sudo 1.8.21 or later has it, and Debian 10 has 1.8.27.)",
            "What the script writes is then owned by root",
        ] {
            assert!(load.contains(needle), "{needle:?}: {load}");
        }
    }

    /// M-tape-facts: what File 1 says about the file count, the last file, the
    /// generation and the capacity is said to be about THIS tape.
    #[test]
    fn reread3_m_tape_facts_are_said_to_be_this_tapes() {
        let g = generate_system_guide_v2_for_media("FACT1", 13, "LTO-6");
        let f = flat_text(&g);
        for needle in [
            "LAST file (File 12 on this tape): SEAL MARKER",
            "The number of the last file is a fact about THIS tape. Another tape of the box has its own: File 0 of that tape gives it (`total_files`, and `seal_marker`, the number of its last file)",
            "This tape has 13 files, numbered 0 to 12, so the last is File 12, unless the tape ended early (below)",
            "reaches the same place on this tape, if it did not end early.)",
            "## Total files on this tape: 13",
            "The files of THIS tape are numbered 0 to 12. File 12, the last, is the seal marker. Another tape of the box has its own count: File 0 of that tape gives it",
            "This tape is LTO-6 (File 0 says so: media_type)",
            "That is about THIS tape: another tape of the box may be of another generation (its own File 0 says), and the table below shows which drives read which",
            "this tape is LTO-6, which holds 2.5 TB of data",
        ] {
            assert!(f.contains(needle), "{needle:?}: {f}");
        }
        // The same for a generation the crate does not know.
        let unknown = flat_text(&generate_system_guide_v2_for_media("U", 13, "DLT-IV"));
        assert!(
            unknown.contains("Another tape of the box may be of another generation"),
            "{unknown}"
        );
    }

    // ---- the fourth read of File 1 (#481, ADR-0012 item 105) ----
    //
    // The third read's seven minor findings, the 1.4.0 re-pin. File 1's text
    // only: RESTORE.sh's bytes (`tests/on_tape_golden.rs`) do not move.

    /// The `sqlite3 ... "SQL"` commands of a guide section, each as its SQL:
    /// a command starts on a line beginning `sqlite3 -header`, opens a double
    /// quote and ends at the line that ends `;"`.
    fn guide_sql(section: &str) -> Vec<String> {
        let mut out = Vec::new();
        let mut cur: Option<String> = None;
        for line in section.lines() {
            let t = line.trim();
            if cur.is_none() {
                if let Some(rest) = t.strip_prefix("sqlite3 -header -column catalog.db \"") {
                    cur = Some(rest.to_string());
                } else {
                    continue;
                }
            } else if let Some(c) = cur.as_mut() {
                c.push(' ');
                c.push_str(t);
            }
            if cur.as_ref().is_some_and(|c| c.ends_with(";\"")) {
                let sql = cur.take().unwrap();
                out.push(sql[..sql.len() - 1].to_string());
            }
        }
        assert!(cur.is_none(), "an unterminated sqlite3 command: {cur:?}");
        out
    }

    /// A catalogue as the kit's `catalog.db.age` holds one (the real schema,
    /// from the real migrations): two tapes, one unit stored in two versions
    /// (the older only on an erased tape), one unit on both tapes.
    fn kit_catalogue() -> rusqlite::Connection {
        let conn = crate::db::open_memory().unwrap();
        conn.execute_batch(
            "INSERT INTO tenants (name, is_operator, status) VALUES ('family', 0, 'active');
             INSERT INTO locations (name) VALUES ('home-rack'), ('bank-box');
             INSERT INTO cartridges (barcode, media_type, serial_number, nominal_capacity, status)
                 VALUES ('BC0001L6', 'LTO-6', 'HUJ808A5L4', 2500000000000, 'in_use'),
                        ('BC0002L6', 'LTO-6', 'HUJ808A5L5', 2500000000000, 'in_use'),
                        ('BC0003L6', 'LTO-6', 'HUJ808A5L6', 2500000000000, 'available');
             INSERT INTO volumes (label, backend_type, backend_name, media_type,
                                  capacity_bytes, status, location_id, sealed_at)
                 VALUES ('L6-0001', 'lto', 'lto0', 'LTO-6', 2500000000000, 'sealed', 1,
                         '2026-10-14 10:00:00'),
                        ('L6-0002', 'lto', 'lto0', 'LTO-6', 2500000000000, 'sealed', 2,
                         '2026-10-15 10:00:00'),
                        ('L6-0003', 'lto', 'lto0', 'LTO-6', 2500000000000, 'erased', NULL, NULL);
             INSERT INTO cartridge_volumes (cartridge_id, volume_id) VALUES (1, 1), (2, 2), (3, 3);
             INSERT INTO units (uuid, name, tenant_id, checksum_mode, encrypt, status)
                 VALUES ('11111111-2222-3333-4444-555555555555', 'photos/2019', 1,
                         'mtime_size', 1, 'active'),
                        ('66666666-2222-3333-4444-555555555555', 'letters', 1,
                         'mtime_size', 1, 'active');
             INSERT INTO snapshots (unit_id, version, snapshot_type, status, source_path)
                 VALUES (1, 1, 'full', 'reclaimable', '/src/photos'),
                        (1, 2, 'full', 'current', '/src/photos'),
                        (2, 1, 'full', 'current', '/src/letters');
             INSERT INTO stage_sets (snapshot_id, status, slice_size)
                 VALUES (1, 'staged', 524288), (2, 'staged', 524288), (3, 'staged', 524288);
             INSERT INTO writes (stage_set_id, snapshot_id, volume_id, status)
                 VALUES (1, 1, 3, 'completed'), (2, 2, 1, 'completed'), (2, 2, 2, 'completed'),
                        (3, 3, 1, 'completed'), (3, 3, 2, 'in_progress');",
        )
        .unwrap();
        conn
    }

    /// Item 1: with nobody left to ask, the Heir Kit's `catalog.db.age` opens
    /// with `age` and `sqlite3`, and the queries in File 1 run against the
    /// kit's real schema and answer "which tapes hold which units".
    #[test]
    fn reread4_item1_the_heir_kits_catalogue_opens_and_the_queries_run() {
        let g = guide17();
        let all = flat_text(&g);
        assert!(
            !all.contains("ask whoever kept the archive"),
            "File 1 no longer assumes someone is left to ask"
        );
        let bx = flat_text(guide_section(
            &g,
            "The rest of the box: other tapes, copies and the index",
        ));
        for needle in [
            "Nobody may be left to ask, so look for the answer on paper too",
            "the cover sheet's \"Tapes are at\" box, and the Heir Kit's catalogue in `catalog.db.age`, which \"The Heir Kit's catalogue\" opens",
        ] {
            assert!(bx.contains(needle), "{needle:?}: {bx}");
        }
        let raw = guide_section(&g, "The Heir Kit's catalogue (catalog.db.age)");
        let f = flat_text(raw);
        for needle in [
            "encrypted to the ESCROW key alone: a tenant's or the operator's key does not open it",
            "It is as of the day the kit was made. It knows no tape written after that day",
            "where it and a tape disagree, the tape is right",
            "It needs `age` and `sqlite3` (package `sqlite3`; \"Tools Required\")",
            "Decrypt it, with the escrow key's file as `YOUR_KEY.age.key`, into a NEW file (`age -o` replaces a file of that name without asking)",
            "A tape holds a good copy when its `status` is `sealed` and its `condition` is `ok`",
            "`erased` is a wiped tape and `retired` one taken out of service: neither is a copy",
            "`quarantined` (a `condition`) is a sealed tape whose medium a verify proved bad",
            "`sqlite3 catalog.db .schema` shows the tables it has",
        ] {
            assert!(f.contains(needle), "{needle:?}: {f}");
        }
        for cmd in [
            "age -d -i YOUR_KEY.age.key -o catalog.db catalog.db.age",
            "sqlite3 catalog.db .tables",
        ] {
            assert!(raw.lines().any(|l| l.trim() == cmd), "{cmd}: {raw}");
        }
        // Tools Required lists sqlite3 as optional, outside RESTORE.sh's table.
        let tools = flat_text(guide_section(&g, "Tools Required"));
        assert!(
            tools.contains("`sqlite3` (package sqlite3) — optional, only to read the Heir Kit's `catalog.db.age`"),
            "{tools}"
        );
        assert!(!restore_sh_tools().contains(&"sqlite3"));

        // The queries run on the real schema and answer the question.
        let sqls = guide_sql(raw);
        assert_eq!(sqls.len(), 2, "{sqls:#?}");
        let conn = kit_catalogue();
        assert_eq!(
            conn.query_row("SELECT COUNT(*) FROM writes", [], |r| r.get::<_, i64>(0))
                .unwrap(),
            5,
            "the fixture was built"
        );
        // 1. every tape, with the sticker, the chip serial and the place.
        let mut stmt = conn.prepare(&sqls[0]).unwrap();
        let cols: Vec<String> = stmt.column_names().map(|c| c.to_string()).collect();
        assert_eq!(
            cols,
            [
                "tape",
                "sticker",
                "chip_serial",
                "generation",
                "status",
                "condition",
                "sealed_at",
                "place"
            ]
        );
        // tape | sticker | chip serial | status | place, one string a row.
        let tapes: Vec<String> = stmt
            .query_map([], |r| {
                let text = |i: usize| -> rusqlite::Result<String> {
                    Ok(r.get::<_, Option<String>>(i)?
                        .unwrap_or_else(|| "(none)".to_string()))
                };
                Ok(format!(
                    "{} | {} | {} | {} | {}",
                    text(0)?,
                    text(1)?,
                    text(2)?,
                    text(4)?,
                    text(7)?
                ))
            })
            .unwrap()
            .collect::<std::result::Result<_, _>>()
            .unwrap();
        assert_eq!(
            tapes,
            [
                "L6-0001 | BC0001L6 | HUJ808A5L4 | sealed | home-rack",
                "L6-0002 | BC0002L6 | HUJ808A5L5 | sealed | bank-box",
                "L6-0003 | BC0003L6 | HUJ808A5L6 | erased | (none)",
            ]
        );
        // 2. which tapes hold which units: only completed (and, from 1.7.0,
        // range) writes, each version on each tape that holds it, the erased
        // tape marked, with what each holds (item 141).
        let mut stmt = conn.prepare(&sqls[1]).unwrap();
        let cols: Vec<String> = stmt.column_names().map(|c| c.to_string()).collect();
        assert_eq!(
            cols,
            [
                "unit",
                "uuid",
                "version",
                "tape",
                "status",
                "condition",
                "holds",
                "slices",
                "of_slices"
            ]
        );
        let rows: Vec<(String, i64, String, String)> = stmt
            .query_map([], |r| Ok((r.get(0)?, r.get(2)?, r.get(3)?, r.get(4)?)))
            .unwrap()
            .collect::<std::result::Result<_, _>>()
            .unwrap();
        // The uuid beside each name (#485, the sixth read): a renamed unit is
        // matched to `--where --all`'s "unit uuid" by it.
        let uuids: Vec<String> = stmt
            .query_map([], |r| r.get(1))
            .unwrap()
            .collect::<std::result::Result<_, _>>()
            .unwrap();
        assert!(uuids.iter().all(|u| u.len() == 36), "{uuids:?}");
        assert_eq!(
            rows,
            [
                ("letters".into(), 1, "L6-0001".into(), "sealed".into()),
                ("photos/2019".into(), 1, "L6-0003".into(), "erased".into()),
                ("photos/2019".into(), 2, "L6-0001".into(), "sealed".into()),
                ("photos/2019".into(), 2, "L6-0002".into(), "sealed".into()),
            ],
            "the in-progress write is not a copy"
        );
        // The two narrowing clauses the text offers, added before ORDER BY.
        for (clause, expect) in [
            ("AND u.name LIKE '%photos%'", 3),
            ("AND v.label = 'L6-0001'", 2),
        ] {
            let narrowed = sqls[1].replace("ORDER BY", &format!("{clause} ORDER BY"));
            let n = conn
                .prepare(&narrowed)
                .unwrap()
                .query_map([], |r| r.get::<_, String>(0))
                .unwrap()
                .count();
            assert_eq!(n, expect, "{clause}");
        }
        assert!(
            f.contains("add `AND u.name LIKE '%word%'` before `ORDER BY`; for one tape, add `AND v.label = 'TAPE'`"),
            "{f}"
        );
        // The Start here and glossary point at it.
        let start = flat_text(guide_section(&g, "Start here"));
        assert!(
            start.contains("every unit the Heir Kit's catalogue lists on a sealed tape, restored or named as held only on tapes you do not have"),
            "{start}"
        );
        let words = flat_text(guide_section(&g, "Words this guide uses"));
        assert!(
            words.contains("(\"The Heir Kit's catalogue\" opens it)"),
            "{words}"
        );
    }

    /// Item 2: a bad key file is told from a tape of another installation.
    #[test]
    fn reread4_item2_a_bad_key_file_is_told_from_a_tape_of_another_installation() {
        let g = guide17();
        let key = flat_text(guide_section(&g, "Which file is your key"));
        for needle in [
            "**A bad key file, or a tape of another installation?** tapectl refuses to write without an escrow recipient, so the ESCROW key opens every tape its own installation wrote, old and new",
            "(An operator or a tenant key does not promise that: after a key rotation an older tape needs the older key, so try every key you hold.)",
            "run `age-keygen -y YOUR_KEY.age.key` and compare with the `age1...` on the cover sheet or in your record",
            "It prints an error: the file is not a key",
            "It prints a different `age1...` from the sheet's: this file is not the escrow key the sheet names. It may be the operator's or a tenant's key, or a good key of another installation; trying it on the tapes (the paragraphs above) is the only way to tell",
            "It prints the same `age1...`: the file is the right key. A tape it opens nothing on, when it opens others, was written by another installation",
            "a damaged one is reported as DAMAGED, and its backup copy is read",
            "If the right key opens NO tape you hold, the cover sheet and these tapes did not come from the same archive",
            "a key file that opens an envelope on even one tape is a good file. One that opens nothing on any tape is a bad file or the wrong installation's, and nothing on the tapes says which",
        ] {
            assert!(key.contains(needle), "{needle:?}: {key}");
        }
        // The glossary no longer says an operator key opens every tape.
        let words = flat_text(guide_section(&g, "Words this guide uses"));
        for needle in [
            "The operator's key opens every tenant's data on the tapes written to it (the operator's key is replaced from time to time, and an older tape needs the older key)",
            "The **escrow key**, a permanent spare made when the archive was set up and kept apart from the rest, is never replaced: it opens every tenant's data on every tape that installation wrote",
        ] {
            assert!(words.contains(needle), "{needle:?}: {words}");
        }
        assert!(
            !words.contains("The operator's key opens every tenant's data on every tape that installation wrote"),
            "{words}"
        );
    }

    /// Item 4: IR and IT are defined, reflashing is conditional, internal and
    /// external drives are told apart, and the bridges are answered plainly.
    #[test]
    fn reread4_item4_the_hardware_questions_are_answered() {
        let g = guide17();
        let load = flat_text(guide_section(
            &g,
            "Loading the tape, and a drive that does not show up",
        ));
        for needle in [
            "LSI (now Broadcom) host adapters come with one of two firmwares: \"IT\" (initiator-target: the card hands every device to the operating system untouched) and \"IR\" (integrated RAID: the card can also build simple RAID sets from disks)",
            "many adapters sold as plain host adapters already run it, so a card that says (IT) needs nothing",
            "Change nothing unless the drive does not appear. If it does not, and the card runs RAID firmware (IR, or a MegaRAID controller), IT firmware can cure it where the maker offers one for that card (not every RAID controller has one: then use a plain adapter instead); flash it only with the maker's own tool and instructions, because a flash that fails can leave a card unusable",
            "An external drive, in its own case with its own power supply, is the simplest: it needs only an adapter with an external port and an external SAS cable that fits both ends",
            "A bare internal drive needs more: a place to sit (a 5.25-inch bay in a desktop or server, or an enclosure made for it), power from a power supply, and the internal SAS cable that matches the adapter's connector",
            "or use a Thunderbolt PCIe enclosure (\"The tape drive\")",
        ] {
            assert!(load.contains(needle), "{needle:?}: {load}");
        }
        assert!(!load.contains("must run the plain"), "{load}");
        let drive = flat_text(guide_section(&g, "The tape drive"));
        for needle in [
            "A PCIe SAS adapter in a desktop or server is the dependable way. Do not use a USB bridge: SAS-to-USB bridges for tape drives are rare and not recommended",
            "a laptop say, can use a Thunderbolt PCIe enclosure that holds a SAS adapter: the operating system then sees an ordinary adapter, and that can work, but it is the less certain way, so check that the drive shows up (below) before you rely on it",
            "Use a plain host adapter (HBA), not a RAID controller",
        ] {
            assert!(drive.contains(needle), "{needle:?}: {drive}");
        }
        assert!(
            !drive.contains("As a rule do not use a USB or Thunderbolt"),
            "{drive}"
        );
        // The bullet the "(below)" points at is the next one.
        let at = drive.find("before you rely on it").unwrap();
        assert!(drive[at..].contains("Check that the drive shows up (`ls -l /dev/tape/by-id/`"));
    }

    /// Item 5: the package for each tool, the oldest age, and no dar, no gpg
    /// and no `dar_version` line (T4.13: the L3-pending test is this one; a
    /// Layout 3 MANIFEST block states no dar, `volume-format-v3.md` §1.9).
    #[test]
    fn reread4_item5_packages_and_the_oldest_age() {
        let g = guide17();
        let tools = flat_text(guide_section(&g, "Tools Required"));
        for needle in [
            "`mt` (mt-st package)",
            "`dd` — reading raw data from tape (coreutils)",
            "`age` — decryption (age-encryption.org; package age on Debian 11 and later and on Ubuntu 22.04 and later, none on Debian 10, where a release from github.com/FiloSottile/age/releases does)",
            "any age from 1.0.0-rc.1 (March 2021) on opens them, the oldest tried",
            "`tar` — unpacking your decrypted envelope, and restoring a unit",
            "`truncate` (coreutils) — only the manual steps use it",
            "`gzip`, `xz` (package xz-utils) or `zstd` — only for a unit that states `compression`",
            "Packages: bash; mawk or gawk (either is `awk`); coreutils; sed; grep",
            "`lsscsi` (package lsscsi), `lspci` (pciutils), `dmesg` (util-linux), `lsmod` and `modprobe` (kmod), `sudo` (sudo) and `usermod` (passwd)",
            "Debian 10 has every one of those packages but `age`",
        ] {
            assert!(tools.contains(needle), "{needle:?}: {tools}");
        }
        // The packages, as one sentence in the order the section lists the tools.
        let sentence = "mt-st, coreutils, tar, age, gzip, xz-utils, zstd, sqlite3, lsscsi, pciutils, kmod and, for tapectl itself, sg3-utils";
        assert!(tools.contains(sentence), "{tools}");
        for gone in ["dar", "gpg", "gnupg", "dar_version", "mkfifo", "tee"] {
            assert!(
                !tools
                    .split(|c: char| !c.is_alphanumeric() && c != '_')
                    .any(|w| w == gone),
                "Layout 3's tools do not name {gone}: {tools}"
            );
        }
        // No MANIFEST block of Layout 3 states a dar: nothing for File 1 to grep.
        let m = ManifestUnit {
            name: "u".into(),
            uuid: "11111111-2222-3333-4444-555555555555".into(),
            snapshot_version: 1,
            stage_set_id: 7,
            payload: "tar".to_string(),
            stream_bytes: 0,
            file_table: None,
            id_map: None,
            tar_version: None,
            compression: None,
            slices: vec![],
        };
        let manifest = generate_manifest_toml("L", "t", std::slice::from_ref(&m));
        assert!(!manifest.contains("dar_"), "{manifest}");
    }

    /// File 1's tool list, its opening sentence and `--info`'s line are drawn
    /// from the one table, and the table is the script's own: the sets
    /// `RESTORE.sh` checks for each mode, read out of it, are the table's.
    #[test]
    fn the_tool_table_is_the_scripts_own_sets() {
        assert_eq!(
            restore_sh_tools(),
            ["dd", "age", "sha256sum", "head", "tar", "tail"]
        );
        let script = crate::volume::heir::RESTORE_SH;
        let set = |name: &str| -> Vec<String> {
            let line = script
                .lines()
                .find(|l| l.starts_with(&format!("{name}=(")))
                .unwrap_or_else(|| panic!("the script has no {name}"));
            line.trim_start_matches(&format!("{name}=("))
                .split(')')
                .next()
                .unwrap()
                .split_whitespace()
                .map(str::to_string)
                .collect()
        };
        let sorted = |mut v: Vec<String>| {
            v.sort();
            v
        };
        let table = |modes: u8| sorted(tools_for(modes).into_iter().map(str::to_string).collect());
        assert_eq!(sorted(set("TOOLS_READ")), table(MODE_READ));
        assert_eq!(sorted(set("TOOLS_ENVELOPE")), table(MODE_ENVELOPE));
        assert_eq!(sorted(set("TOOLS_LIST")), table(MODE_LIST));
        assert_eq!(sorted(set("TOOLS_INDEX")), table(MODE_INDEX));
        assert_eq!(sorted(set("TOOLS_RESTORE")), table(MODE_RESTORE));
        // What a path restore and the repair add: `tail`, once.
        let mut with_paths = set("TOOLS_RESTORE");
        with_paths.extend(set("TOOLS_PATHS"));
        assert_eq!(sorted(with_paths), table(MODE_PATH));

        let g = generate_system_guide_v2_for_media("TOOLS2", 20, "LTO-6");
        let intro = g
            .split_once("All you need is:")
            .and_then(|(_, rest)| rest.split_once("\n\n"))
            .map(|(list, _)| flat_text(list))
            .expect("the guide states its tool list up front");
        assert_eq!(intro, "mt, dd, age, sha256sum, head, tar, and tail.");
        let section = g
            .split_once("## Tools Required")
            .and_then(|(_, rest)| rest.split_once("\n## "))
            .map(|(body, _)| body)
            .expect("a Tools Required section");
        for tool in restore_sh_tools() {
            assert!(
                section
                    .lines()
                    .any(|l| l.starts_with(&format!("- `{tool}`"))),
                "Tools Required lacks a bullet for {tool}:\n{section}"
            );
        }
        let flat = flat_text(section);
        assert!(flat.contains("`mt` (mt-st package)"), "{flat}");
        assert!(flat.contains("cannot set it"), "GNU cpio mt: {flat}");
        assert!(
            flat.contains("`--info` and `--verify` need only `mt`, `dd`, `sha256sum` and `head`"),
            "{flat}"
        );
    }

    /// Every full-restore example names what it restores: `--all`, one
    /// `--unit UNIT`, or the one repair's list. A restore with none of them
    /// would stop at "multiple units found".
    #[test]
    fn the_guides_restore_examples_name_what_they_restore() {
        let g = generate_system_guide_v2_for_media("FULL1", 20, "LTO-6");
        assert!(
            g.contains("./RESTORE.sh --restore --key YOUR_KEY.age.key --all --to /destination\n"),
            "{g}"
        );
        let mut seen = 0;
        for line in g
            .lines()
            .filter(|l| l.starts_with("    ./RESTORE.sh --restore"))
        {
            seen += 1;
            assert!(
                line.contains("--all")
                    || line.contains("--unit UNIT")
                    || line.contains("--paths-from"),
                "a restore example names nothing to restore: {line}"
            );
        }
        assert!(seen >= 4, "{seen}");
        let flat = flat_text(&g);
        assert!(flat.contains("the sum of them with `--all`"), "{flat}");
        assert!(
            flat.contains("`bash`, `awk` and the basic coreutils"),
            "{flat}"
        );
        // One key placeholder throughout (#477).
        assert!(!g.contains("escrow.age.key") && !g.contains("TENANT.age.key"));
    }

    /// #523: the tapectl route needs the operator key or the escrow secret
    /// and says what a tenant-key holder does instead; `age-keygen -y` gives
    /// the escrow identity only from the escrow secret.
    #[test]
    fn file_1_tapectl_route_needs_the_operator_or_escrow_key() {
        let g = generate_system_guide_v2_for_media("KEY523", 20, "LTO-6");
        let itself = flat_text(guide_section(&g, "tapectl itself"));
        for needle in [
            "needs the OPERATOR key or the ESCROW secret",
            "With a tenant's key it is refused",
            "A holder of only a tenant's key has RESTORE.sh, the manual steps and the one repair from another copy of the tape, and nothing else",
            "`age-keygen -y` of the ESCROW secret prints it, and of any other key prints a different one",
        ] {
            assert!(itself.contains(needle), "{needle:?}: {itself}");
        }
        let keys = flat_text(guide_section(&g, "Which file is your key"));
        assert!(
            keys.contains("Which key gives which route"),
            "the key section says what each key gives: {keys}"
        );
        assert!(
            keys.contains("A tenant's key opens that tenant's envelope and slices only"),
            "{keys}"
        );
        // The mention of tapectl's own restore in the damage section names the key.
        let damaged = flat_text(guide_section(&g, "If a slice or a file is damaged"));
        assert!(
            damaged.contains("for the operator or escrow key"),
            "{damaged}"
        );
        assert!(
            !g.contains("the escrow key's included"),
            "the old sentence that sent every key to tapectl is gone"
        );
    }

    /// #526: only messages `age` itself prints (age 1.1.1), and a slice's
    /// size on tape is `encrypted_bytes`, never the manifest's plain
    /// `size_bytes`.
    #[test]
    fn the_texts_name_only_messages_age_prints_and_the_right_size() {
        let g = generate_system_guide_v2_for_media("AGE526", 20, "LTO-6");
        let md = generate_recovery_md("L", "t", &[], &[]);
        for (what, text) in [("File 1", flat_text(&g)), ("RECOVERY.md", flat_text(&md))] {
            assert!(
                text.contains("failed to decrypt and authenticate payload chunk"),
                "{what}"
            );
            assert!(!text.contains("unexpected data"), "{what}");
            assert!(
                text.contains("no identity matched any of the recipients"),
                "{what}"
            );
            // rage, which both texts tell an heir to link as `age`, says it
            // its own way (#596 heir-C3, the `age` crate's err-no-matching-keys).
            assert!(text.contains("No matching keys found"), "{what}");
        }
        let flat = flat_text(&g);
        assert!(
            flat.contains("MANIFEST.toml's `encrypted_bytes` for a slice"),
            "{flat}"
        );
        assert!(
            flat.contains("A slice's `size_bytes` in MANIFEST.toml is something else: the length of its PLAIN cut"),
            "{flat}"
        );
        assert!(
            !flat.contains("exact tape position and byte size"),
            "{flat}"
        );
        let md = flat_text(&md);
        assert!(
            md.contains("`size_bytes` in MANIFEST.toml is its PLAIN length"),
            "{md}"
        );
    }

    /// #563: what a restore on another computer does with an owner or an ACL
    /// entry, and that the id map says who the ids were and is never acted on.
    #[test]
    fn file_1_says_what_a_restore_on_another_computer_does() {
        let g = generate_system_guide_v2_for_media("HOST563", 20, "LTO-6");
        let sec = flat_text(guide_section(&g, "What a restore on another computer does"));
        for needle in [
            "stores the owner and group of every file by NUMBER only",
            "tar restores every file as yours (owner and group are set only by root)",
            "On another computer those numbers are other accounts, or none",
            "Nothing is remapped",
            "An ACL entry",
            "is stored by number the same way",
            "the entry follows the number, and on another computer it names whoever has that number, or nobody",
            "BusyBox's tar restores neither the owner, the ACL nor any extended attribute, and does not say so",
            "The ID MAP says who the numbers were",
            "\"Who the ids were\"",
            "a restore shows it and never acts on it",
            "never by tar's exit status",
        ] {
            assert!(sec.contains(needle), "{needle:?}: {sec}");
        }
        // The id map is named where the glossary defines it.
        let words = flat_text(guide_section(&g, "Words this guide uses"));
        assert!(words.contains("**Id map**"), "{words}");
        assert!(words.contains("**File table**"), "{words}");
    }

    /// A unit's slice list for RECOVERY.md tests: `(number, tape position,
    /// continues)`.
    fn unit_with(
        name: &str,
        uuid: &str,
        compression: Option<&str>,
        slices: &[(i64, Option<i64>, bool)],
    ) -> ManifestUnit {
        ManifestUnit {
            name: name.into(),
            uuid: uuid.into(),
            snapshot_version: 1,
            stage_set_id: 7,
            payload: "tar".to_string(),
            stream_bytes: 1000,
            file_table: Some("tables/11111111_v1_s7.tsv".to_string()),
            id_map: Some("tables/11111111_v1_s7.ids.tsv".to_string()),
            tar_version: None,
            compression: compression.map(str::to_string),
            slices: slices
                .iter()
                .map(|(number, position, continues)| ManifestSlice {
                    number: *number,
                    tape_position: *position,
                    size_bytes: 100 + number,
                    encrypted_bytes: 200 + number,
                    sha256_plain: format!("{number:064x}"),
                    sha256_encrypted: format!("{:064x}", number + 100),
                    continues: *continues,
                })
                .collect(),
        }
    }

    /// #596 C-4: two stagings of one version on one tape (volume-format-v3.md
    /// 1.6: they never collide) are two blocks of one uuid and version. Each
    /// gets the stem of its own file table, `{uuid8}_v{version}_s{stage_set}`,
    /// for its work directory and its destination, so the second tar run does
    /// not extract over the first's tree.
    #[test]
    fn two_stagings_of_one_version_get_work_and_destination_directories_of_their_own() {
        let uuid = "cccccccc-0000-0000-0000-000000000003";
        let first = unit_with("shared", uuid, None, &[(1, Some(10), false)]);
        let mut second = unit_with("shared", uuid, None, &[(1, Some(11), false)]);
        second.stage_set_id = 8;
        second.file_table = Some("tables/cccccccc_v1_s8.tsv".to_string());
        let md = generate_recovery_md("LAB", "alice", &[first, second], &[]);
        for stem in ["cccccccc_v1_s7", "cccccccc_v1_s8"] {
            assert!(
                md.contains(&format!("mkdir -p work/{stem} /destination/{stem}")),
                "{stem}: {md}"
            );
        }
        assert!(
            !md.contains("work/cccccccc_v1 ") && !md.contains("/destination/cccccccc_v1\n"),
            "a directory both blocks share: {md}"
        );
    }

    /// #522: each unit's block names a work directory and a destination of
    /// its own, nothing is shared between two blocks, and a tape that holds
    /// only part of a unit reads the slices it holds: a leading slice that
    /// continues a file begun elsewhere is not read, a slice on another tape
    /// is named and never read, and a compressed unit pipes each slice through
    /// its decompressor.
    #[test]
    fn recovery_md_blocks_have_their_own_directories_and_read_what_this_tape_holds() {
        let a = unit_with(
            "photos/2019",
            "aaaaaaaa-0000-0000-0000-000000000001",
            None,
            &[(1, Some(10), false), (2, Some(11), false)],
        );
        // A range of a compressed unit: slice 1 is on another tape, slice 2
        // continues a file begun there, slices 3 and 4 are whole files.
        let b = unit_with(
            "music",
            "bbbbbbbb-0000-0000-0000-000000000002",
            Some("xz"),
            &[
                (1, None, false),
                (2, Some(20), true),
                (3, Some(21), false),
                (4, Some(22), false),
            ],
        );
        let md = generate_recovery_md("LAB", "alice", &[a.clone(), b.clone()], &[]);
        let sec_a = guide_section_of(&md, "photos/2019");
        let sec_b = guide_section_of(&md, "music");
        assert!(
            sec_a.contains("mkdir -p work/aaaaaaaa_v1_s7 /destination/aaaaaaaa_v1_s7"),
            "{sec_a}"
        );
        assert!(
            sec_b.contains("mkdir -p work/bbbbbbbb_v1_s7 /destination/bbbbbbbb_v1_s7"),
            "{sec_b}"
        );
        assert!(
            !sec_a.contains("bbbbbbbb") && !sec_b.contains("aaaaaaaa"),
            "no block names another's directory"
        );
        for sec in [&sec_a, &sec_b] {
            assert!(
                !sec.contains("restore.") && !sec.contains("mkdir /destination\n"),
                "{sec}"
            );
            assert!(
                sec.contains("rm -r work/"),
                "the work directory is removed: {sec}"
            );
            assert!(
                sec.contains(
                    "LC_ALL=C tar -x --numeric-owner -p --xattrs --acls -f - -C /destination/"
                ),
                "{sec}"
            );
        }
        // The whole unit: both slices, in order, joined.
        assert!(
            sec_a.contains(
                "cat work/aaaaaaaa_v1_s7/slice.1.tar work/aaaaaaaa_v1_s7/slice.2.tar | LC_ALL=C tar"
            ),
            "{sec_a}"
        );
        // The range: slice 1 named, never read; slice 2 not read; 3 and 4 are.
        assert!(sec_b.contains("# Slice 1 — on another tape"), "{sec_b}");
        assert!(
            sec_b.contains("# Slice 2 — tape file 20: it continues a file that begins on"),
            "{sec_b}"
        );
        assert!(
            !sec_b.contains("slice.1.age") && !sec_b.contains("slice.2.age"),
            "{sec_b}"
        );
        assert!(
            sec_b.contains("mt -f /dev/nst0 rewind && mt -f /dev/nst0 fsf 21\n"),
            "the tape is wound to the first slice it reads: {sec_b}"
        );
        assert!(
            sec_b.contains(
                "work/bbbbbbbb_v1_s7/slice.3.age | xz -dc > work/bbbbbbbb_v1_s7/slice.3.tar"
            ),
            "{sec_b}"
        );
        assert!(
            sec_b.contains(
                "cat work/bbbbbbbb_v1_s7/slice.3.tar work/bbbbbbbb_v1_s7/slice.4.tar | LC_ALL=C tar"
            ),
            "{sec_b}"
        );
        // The length and the hash are of the decompressed (plain) slice.
        assert!(
            sec_b.contains("test $(wc -c < work/bbbbbbbb_v1_s7/slice.3.tar) -eq 103"),
            "{sec_b}"
        );
        assert!(
            sec_b.contains(&format!("{:064x}  work/bbbbbbbb_v1_s7/slice.3.tar", 3)),
            "{sec_b}"
        );
        // The section on a unit across tapes is there for a range, and only then.
        assert!(md.contains("## A unit across tapes"), "{md}");
        let whole = generate_recovery_md("LAB", "alice", std::slice::from_ref(&a), &[]);
        assert!(!whole.contains("## A unit across tapes"), "{whole}");
        // Each unit's check names its own table's name.
        assert!(sec_a.contains("env/tables/11111111_v1_s7.tsv"), "{sec_a}");
    }

    /// The section of a RECOVERY.md that begins `## {title}`.
    fn guide_section_of(md: &str, title: &str) -> String {
        guide_section(md, title).to_string()
    }

    // ---- the fifth read of File 1 (round 4 of #481: read4-fixes.md) ----
    //
    // One serious finding (S1) and fourteen minor ones (M2-M15). File 1 and
    // RECOVERY.md text only; the manual block of M12 is run against the fake
    // tape in tests/heir_restore_sh.rs.

    fn box_section(g: &str) -> String {
        flat_text(guide_section(
            g,
            "The rest of the box: other tapes, copies and the index",
        ))
    }

    /// M2: with no envelope copy readable, no false promise.
    #[test]
    fn reread5_m2_no_envelope_reads_promises_nothing_it_cannot_keep() {
        let g = guide17();
        let ladder = flat_text(guide_section(&g, "If All Else Fails"));
        for needle in [
            "**When no envelope reads.** If every copy of every envelope (each one and its backup) is damaged, the data slices may be intact, but nothing else on the tape says which of them belong to which unit or in what order: that map is the envelope's MANIFEST.toml, and this guide gives no way to assemble the slices without it",
            "another copy of this tape (its envelopes carry it), or the Heir Kit's catalogue (\"The Heir Kit's catalogue\"), whose tables `stage_slices` and `write_positions` record each slice's number and its tape file, if the kit was made after this tape was written",
            "With a map, read the slices as step 7 of \"Manual Recovery Steps\" does",
        ] {
            assert!(ladder.contains(needle), "{needle:?}: {ladder}");
        }
        let manual = flat_text(guide_section(&g, "Manual Recovery Steps"));
        assert!(
            manual.contains("(If neither the envelope nor its backup reads: \"If All Else Fails\", at its end.)"),
            "{manual}"
        );
        // The tables the text names exist, with the columns it relies on.
        let conn = crate::db::open_memory().unwrap();
        conn.prepare("SELECT slice_number FROM stage_slices")
            .unwrap();
        conn.prepare("SELECT position, stage_slice_id FROM write_positions")
            .unwrap();
    }

    /// M6: the drive that reads the most, and the newest tape.
    #[test]
    fn reread5_m6_the_newest_tape_the_drive_reads_and_what_a_missing_drive_costs() {
        let g = guide17();
        let start = flat_text(guide_section(&g, "Start here"));
        assert!(
            start.contains("If the newest tape needs a drive you do not have, start from the newest your drive reads, and know that the newest index is out of reach: get that drive, or say what may be missing"),
            "{start}"
        );
        let bx = box_section(&g);
        assert!(
            bx.contains("If the newest tape needs a drive you do not have, start from the newest tape your drive reads, and know that the newest index is out of reach: get that drive if you can. If you cannot, say with what you restore that units written after the tape you could read may be missing"),
            "{bx}"
        );
        let drive = flat_text(guide_section(&g, "The tape drive"));
        assert!(
            drive.contains("read those first, starting with the newest tape it reads: if the newest tape of the box is not one of them, \"The rest of the box\" says what that costs"),
            "{drive}"
        );
    }

    /// M7: where a missing tape is kept is on no tape.
    #[test]
    fn reread5_m7_where_tapes_are_kept_is_written_on_no_tape() {
        let g = guide17();
        let bx = box_section(&g);
        assert!(
            bx.contains("Where a tape is kept is written on no tape: the index names a missing tape only by its label, cartridge serial and uuid, so look for cartridges labelled so, and for the operator's papers and storage records"),
            "{bx}"
        );
        // True of the index: no column says where a tape is.
        let r = flat_text(&generate_recovery_md("LAB01", "alice", &[], &[]));
        assert!(r.contains("`volume` (the tape's label), `volume_uuid`, `cartridge_serial`, `generation`, `state`"), "{r}");
        assert!(!r.contains("`location`") && !r.contains("`place`"), "{r}");
    }

    /// M14: units.tsv and PLAN.toml are defined where they are first used.
    #[test]
    fn reread5_m14_units_tsv_and_plan_toml_are_defined_where_first_used() {
        let g = guide17();
        let first = |word: &str| g.find(word).unwrap_or_else(|| panic!("{word}"));
        let f = flat_text(&g);
        let units_def = f
            .find("`units.tsv` is a table with a row for each version of each unit and each tape that holds it")
            .expect("units.tsv defined");
        let units_first = flat_text(&g[..first("units.tsv")]).len();
        assert!(units_def < units_first + 200, "defined at its first use");
        let plan_def = f
            .find("`PLAN.toml` (the operator envelope's plan of the tape: its units, each with its uuid, slice count and size; a tenant's envelope never has one)")
            .expect("PLAN.toml defined");
        let plan_first = flat_text(&g[..first("PLAN.toml")]).len();
        assert!(plan_def < plan_first + 200, "defined at its first use");
        assert_eq!(g.matches("PLAN.toml").count(), 1);
        // What the definition says is what the writers make.
        let plan = generate_planning_header("L", &[("u".into(), "uuid".into(), 3, 99)]);
        for key in ["name = ", "uuid = ", "num_slices = ", "total_bytes = "] {
            assert!(plan.contains(key), "{plan}");
        }
    }

    // ---- The sixth read's fixes (#485, ADR-0012 item 109, tapectl 1.5.0) ----
    // File 1 and RECOVERY.md text only; the commands they show are run
    // against the fake tape and a real dar in tests/heir_restore_sh.rs.

    /// RECOVERY.md's line for the same question runs, under every awk, on a
    /// paths.tsv of two versions, and says so when the newer version has no
    /// rows rather than listing every path of the older one.
    #[test]
    fn reread6_i2_recovery_md_line_lists_paths_only_the_older_version_has() {
        let s = generate_recovery_md("LAB01", "alice", &[], &[]);
        let line = s
            .lines()
            .find(|l| l.starts_with("OLD=2 NEW=3 U="))
            .unwrap_or_else(|| panic!("no OLD/NEW line:\n{s}"))
            .to_string();
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("index")).unwrap();
        // Columns permuted and one unknown, as the section says to expect.
        std::fs::write(
            dir.path().join("index/paths.tsv"),
            "path\tzz\tunit\tversion\nkeep.txt\tx\tphotos/2019\t2\nkeep.txt\tx\tphotos/2019\t3\n\
             gone.txt\tx\tphotos/2019\t2\nnew.txt\tx\tphotos/2019\t3\na\\tb.txt\tx\tphotos/2019\t2\n\
             other.txt\tx\tdocs\t2\n",
        )
        .unwrap();
        for awk in ["awk", "mawk", "gawk"] {
            if std::process::Command::new(awk)
                .arg("BEGIN{}")
                .output()
                .is_err()
            {
                continue;
            }
            let o = std::process::Command::new("bash")
                .arg("-c")
                .arg(line.replacen("awk", awk, 1))
                .current_dir(dir.path())
                .output()
                .unwrap();
            assert!(o.status.success(), "{awk}: {o:?}");
            assert_eq!(
                String::from_utf8_lossy(&o.stdout),
                "a\\tb.txt\ngone.txt\n",
                "{awk}: the paths only version 2 has, as stored"
            );
            // No rows for the newer version: say so, list nothing.
            let o = std::process::Command::new("bash")
                .arg("-c")
                .arg(line.replacen("awk", awk, 1).replace("NEW=3", "NEW=9"))
                .current_dir(dir.path())
                .output()
                .unwrap();
            assert!(o.stdout.is_empty(), "{awk}: {o:?}");
            assert!(
                String::from_utf8_lossy(&o.stderr).contains("no paths for version 9"),
                "{awk}: {o:?}"
            );
        }
    }

    /// #485 item 6: a working setup for this tape's generation, and where
    /// the generation is read without a drive, in what the cover sheet
    /// really prints (`generations_text`).
    #[test]
    fn reread6_i6_a_setup_that_works_and_the_generation_without_a_drive() {
        let g = flat_text(guide_section(&guide17(), "The tape drive"));
        for needle in [
            "You can learn a cartridge's generation without a drive, and before this guide can be read: it is printed on the cartridge (the LTO logo and number on the case, and the last two characters of the barcode label, where L6 is LTO-6), and the Heir Kit's cover sheet lists the generations of the tapes the Heir Kit's catalogue knew when it was made, with the drives that read each, in lines like \"LTO-6 (2 volume(s)): an LTO-6 or LTO-7 drive\". A sheet made when the Heir Kit's catalogue listed no sealed cartridge says so and sends you to the label",
            "A setup that works, as one example: an LTO-6 or LTO-7 drive with a SAS interface, a PCIe SAS host adapter in a desktop computer with a free PCIe slot, and the cable and power to match",
            "For an external drive, which has its own power supply: an adapter with an external port (an LSI or Broadcom 9200-8e, which has SFF-8088 ports, or a 9300-8e, which has SFF-8644, on IT firmware) and an external SAS cable whose ends fit the drive and the adapter",
            "For a bare internal drive, in a 5.25-inch bay: an adapter with an internal port (a 9211-8i, SFF-8087, or a 9300-8i, SFF-8643), a cable from that port that ends in an SFF-8482 drive connector, and the drive's power from the computer's power supply, through that connector or from a separate lead (often a 4-pin Molex plug), as the drive's data sheet says",
            "These models are examples, not a recommendation: the connectors are the standards that last, and the data sheets decide",
        ] {
            assert!(g.contains(needle), "{needle:?}: {g}");
        }
        // The sample line is the one the cover sheet prints for the generation.
        let g8 = flat_text(guide_section(
            &generate_system_guide_v2_for_media("L8", 20, "LTO-8"),
            "The tape drive",
        ));
        assert!(
            g8.contains("in lines like \"LTO-8 (2 volume(s)): an LTO-8 or LTO-9 drive\""),
            "{g8}"
        );
        assert!(g8.contains(
            "A setup that works, as one example: an LTO-8 or LTO-9 drive with a SAS interface"
        ));
        let unknown = flat_text(guide_section(
            &generate_system_guide_v2_for_media("U", 20, "DLT-IV"),
            "The tape drive",
        ));
        assert!(
            unknown.contains("A setup that works, as one example: a drive that reads this tape's generation, with a SAS interface"),
            "{unknown}"
        );
        assert!(
            unknown.contains(
                "\"LTO-n (2 volume(s)): an LTO-n drive\", with the generation in place of n"
            ),
            "{unknown}"
        );
        // The loading bullet points at the same two sources.
        let load = flat_text(guide_section(
            &guide17(),
            "Loading the tape, and a drive that does not show up",
        ));
        assert!(
            load.contains("This guide is on the tape, which you cannot read without a drive, so take the generation from the cartridge or the Heir Kit's cover sheet (\"The tape drive\")"),
            "{load}"
        );
    }

    /// #485 item 7: after a failed `dd` the head is inside the file, and the
    /// manual steps, the rung-3 reads and RECOVERY.md say to rewind and `fsf`.
    #[test]
    fn reread6_i7_a_failed_dd_leaves_the_head_inside_the_file() {
        let g = guide17();
        let manual = flat_text(guide_section(&g, "Manual Recovery Steps"));
        for needle in [
            "Each `dd` that finishes leaves the head at the start of the next file",
            "A `dd` that stops with an error (\"Input/output error\") leaves the head INSIDE that file, not at the start of the next: after any failed read, `mt rewind` and `mt fsf` to the file you want next, whatever the counting below says",
            "each read that finishes leaves the tape at the start of the next file, so the next slice needs no spacing unless there is a gap or a read failed",
            "If a `dd` stopped with an error instead (\"Input/output error\"), the head is inside that slice's file and the count above is wrong: `mt rewind`, then `fsf` to the `tape_position` of the slice you want next (or of the same slice, to try it again), and read on from there",
        ] {
            assert!(manual.contains(needle), "{needle:?}: {manual}");
        }
        let rung3 = flat_text(guide_section(&g, "If All Else Fails"));
        assert!(
            rung3.contains("After a `dd` that failed with an error the head is inside that file: `mt rewind` and `fsf` to the next candidate instead"),
            "{rung3}"
        );
        let units = vec![ManifestUnit {
            name: "alpha".into(),
            uuid: "uuid-a".into(),
            snapshot_version: 2,
            stage_set_id: 7,
            payload: "tar".to_string(),
            stream_bytes: 0,
            file_table: None,
            id_map: None,
            tar_version: None,
            compression: None,
            slices: vec![ManifestSlice {
                continues: false,
                number: 1,
                tape_position: Some(4),
                size_bytes: 1_048_576,
                encrypted_bytes: 1_049_000,
                sha256_plain: "abc".into(),
                sha256_encrypted: "def456".into(),
            }],
        }];
        let r = flat_text(&generate_recovery_md("LAB01", "alice", &units, &[]));
        assert!(
            r.contains("each read that finishes leaves it at the start of the next file. A `dd` that stops with an error (\"Input/output error\") leaves the head inside that file instead: `mt rewind`, then `mt fsf` to the tape file of the slice you want next, which its `# Slice` line gives"),
            "{r}"
        );
    }

    // ---- The sixth read (1.5.0, #485, ADR-0012 item 109): what it found ----
}
