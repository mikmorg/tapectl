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
    /// THAT barcode). `docs/design/volume-format-v2.md` §1.1, ADR-0012.
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
/// `docs/design/volume-format-v2.md` §1: `[volume]` carries identity
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

/// In the order RESTORE.sh's `Requirements:` lines have always listed them.
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
        name: "dar",
        modes: MODE_LIST | MODE_PATH | MODE_RESTORE,
        role: "archive extraction (package dar; dar.linux.free.fr)",
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
        name: "truncate",
        modes: DECRYPTING,
        role: "trimming block padding to exact sizes (coreutils)",
    },
    RestoreTool {
        name: "tar",
        modes: DECRYPTING,
        role: "unpacking your decrypted envelope (it is a tar archive; package tar)",
    },
    RestoreTool {
        name: "mkfifo",
        modes: MODE_RESTORE,
        role: "the named pipes of a restore: one carries a unit's slices to dar, joined back into a single stream, and the other hashes each slice as it passes (coreutils)",
    },
    RestoreTool {
        name: "tee",
        modes: MODE_PATH | MODE_RESTORE,
        role: "hashing a slice as it streams, and keeping dar's output (coreutils)",
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
/// [`RESTORE_TOOLS`] with its role. The file's own `gpg` and dar-version
/// bullets follow them.
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
    let mut rows = String::from("    Cartridge       Read by a drive of\n");
    for media in BOX_TABLE_MEDIA {
        rows.push_str(&format!(
            "    {:<14}  {}\n",
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
/// v1 mid-tape mini-index no longer exists — `volume-format-v2.md` §8); adds
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

## Volume: {label}

This document describes how to recover data from this tape without
{intro_tools}

Besides those it uses `bash`, `awk` and the basic tools every Linux system
has (`sed`, `grep`, `tr`, `cut`, `sort`, `stat`, `df`, `wc`, `tail`, `od`,
`mktemp`), and `gpg` if your dar was built with GPG support ("Tools
Required" says what each is for, and names the few more that only finding
the drive and checking a key use).

## Start here

The whole job in order. Each step names the section that has its commands.
Unit, envelope, index, cover sheet: "Words this guide uses" says what each is.

1. Check your key: "Which file is your key". A tenant's key opens that
   tenant's units only; the operator key and the escrow key open every
   tenant's. With `age` installed (step 2), check it against the cover
   sheet's `age1...` line (`age-keygen -y`); with only a key file, the
   check is that it opens an envelope (`--find-envelope`, step 5).
2. Get a drive that reads this tape and a computer it connects to: "The
   tape drive" and "Loading the tape, and a drive that does not show up".
   The programs it needs, with their package names: "Tools Required".
3. Load the tape, find the drive's device name ("Which device?") and read
   RESTORE.sh, File 2, off the tape into a file named for the tape,
   `RESTORE-{label}.sh`: the first commands of "Automated Recovery
   (recommended)". Set `TAPE_DEVICE` to the same device once, so RESTORE.sh
   reads the drive your `mt` and `dd` commands read.
4. Run `--info` on EVERY tape you hold (this tape's script is
   `./RESTORE-{label}.sh`; this guide writes `./RESTORE.sh` for any tape's).
   It needs no key and shows each tape's label, when it was sealed and its
   `Verdict:`; for anything but SEALED see "If the verdict is UNSEALED or
   DAMAGED". Any tape's script reads the tape in the drive for `--info` and
   `--find-envelope`, one from a tape written by 1.3.0 or later for
   `--where`; a restore needs the script of the tape it reads.
5. Start from the NEWEST tape (the latest "Sealed at"; File 0's
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
   every unit on it ("Automated Recovery (recommended)"); check that it did
   (item 4 of "Did the restore get everything?"). From several tapes,
   restore each unit's NEWEST version once, from a tape that holds it, into
   a directory of its own (`/destination/UNIT`), with that tape's own
   RESTORE.sh, saved as `RESTORE-<label>.sh`, and never an older version
   first ("The rest of the box" says how to do both). A renamed unit has
   the name each tape shows. Two units of one name (compare the uuids
   `--where --all` prints) go in different directories:
   `/destination/UNIT-` and the first 8 characters of the uuid. A unit
   across tapes (`--where` shows a tape holding a range of it) restores from
   a tape that holds it whole, or from the tape that holds its slice 1,
   which asks for the others: "A unit across tapes".
7. Check each restore: "Did the restore get everything?". A slice or a file
   that is damaged: "If a slice or a file is damaged".
8. Know when you are done: every tape you hold opened with your key and
   the newest version of every unit any of them lists restored; every
   unit `--where --all` names on the newest readable index, and every unit
   the Heir Kit's catalogue lists on a sealed tape, restored or named as
   held only on tapes you do not have; and any tape your key cannot open
   named, never counted as looked at. No tape names one written after it,
   and a tape from before 1.3.0 has no index: a kit made later can name
   such tapes ("The Heir Kit's catalogue"). "The rest of the box" has the
   whole rule, a damaged unit's place in it, and what a box cannot show.

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
  seal marker are (`front_index`, `seal_marker`) and `requires` (below). It
  also prints the commands that read the next file, this guide: "File 0's
  commands" in this guide means those.
- **`requires`**: a line in File 0 and in the seal marker that lists the
  features a reader must understand to read the tape correctly. A tape
  written by tapectl 1.6.0 or later states `requires = ["short-seal"]`: the
  tape may end before the last file the front index plans, and its seal
  marker says where ("Where the tape ended"). Older tapes state it empty
  (`requires = []`, from 1.1.0 to 1.5.0) or have no such line, which means
  nothing special is needed. A tape written by tapectl 1.7.0 or later that
  holds a slice of a unit allowed to run across tapes states `requires =
  ["short-seal", "spanning"]`: some slices of that unit may be on other
  tapes ("A unit across tapes"). A RESTORE.sh that meets a feature it does
  not know refuses by name and points to the RESTORE.sh stored on that tape,
  or a newer tapectl.
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
  if you want such files ("Files deleted before the newest version", in "The
  rest of the box", finds them). A version is also called a snapshot:
  MANIFEST.toml states it as `snapshot_version`, RECOVERY.md as "snapshot
  v2" and RESTORE.sh as "snapshot version". Versions are numbered 1, 2, 3 ...
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
  catalogue" opens it). A kit made by tapectl 1.5.0 or later also holds a
  copy of the tapectl program that made it, a file named `tapectl-` and its
  version ("tapectl itself"). It records the archive as of the day it was
  made, which may be before the newest tape was written. You may have none.
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
  `catalogs/` (the units' dar catalogues, below), and on a tape written by
  tapectl 1.3.0 or later `index/` (the index, below).
- **Slice**: a unit is archived by dar in numbered pieces called slices.
  Each slice is encrypted on its own and stored as one tape file, and one
  unit's slices lie together on the tape. Restoring a unit means reading
  all of its slices, in order. A unit allowed to run across tapes (tapectl
  1.7.0 or later) can have its slices on more than one tape: a tape then
  holds a **range** of it, one unbroken run of its slices in order ("A unit
  across tapes").
- **Index**: two different things. The FRONT index is File 3 (the seal
  marker, the last file, carries a copy): a plain-text map of THIS tape,
  with every tape file's position, type, size and hash. The index, with no
  "front", is the directory `index/` in an envelope of a tape written by
  tapectl 1.3.0 or later: which units, versions and paths the archive had
  when that tape was written, and which tapes hold them. `--where` and
  `--find` read it ("The rest of the box").
- **Catalogue**: this guide never uses the word bare, because it names three
  different things. The Heir Kit's catalogue is the archive's database, as
  of the day the kit was made, kept as `catalog.db.age` ("The Heir Kit's
  catalogue"); an installation's own catalogue is the live database it was
  copied from. A dar catalogue (the next entry) is a small file that lists
  one version of one unit. `catalogs/` is the directory of an envelope that
  holds the dar catalogues.
- **dar catalogue**: a small file for each version of a unit, kept in the
  envelope under `catalogs/`. It lists every file of the unit and which
  slice holds it, so `dar -l` can list the unit, and one file can be
  restored, without reading the unit's slices. Its NAME in `catalogs/NAME`
  is the first eight characters of the unit's uuid, then `_v` and the
  version number: version 2 of a unit whose uuid starts `11111111-` is
  `catalogs/11111111_v2.1.dar`, and dar is given the name without `.1.dar`
  (`dar -l catalogs/11111111_v2 -N -Q`). MANIFEST.toml states each unit's
  uuid and version. A dar catalogue's listing marks every entry `[InRef]`,
  dar's word for "the data is in the archive this catalogue was cut from",
  which here means the unit's slices on the tape. It does not mean the unit
  is a difference from another version.

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
public `age1...` that goes with it, which must equal the one on the cover
sheet or record you have. A character miscopied is refused with "invalid
checksum".

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
opens nothing ("no identity matched any of the recipients" for every
envelope), check it first as above; then try the operator key and every
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

## Quick Reference

This volume uses layout v2. Tape files are laid out in this fixed order:

- File 0: ID thunk (this tape's identity; says the map is File 3)
- File 1: This guide
- File 2: RESTORE.sh (automated restore + verify script)
- File 3: FRONT INDEX — every file's position, type, on-tape size, and
  ciphertext hash. Read this first for a full map of the tape.
- File 4 onward: Tenant envelope(s) — one per tenant sharing this tape
- Then: Operator envelope, then Operator envelope backup
- Then: a backup copy of each tenant envelope, in the same order (the
  same bytes: if your envelope will not read, read its backup)
- Then: Data slices (age-encrypted dar archives) — one unit's slices
  are stored contiguously
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
- `gpg` (package gnupg) — only if your `dar` was built with GPG support,
  as Debian's and Ubuntu's is: such a dar refuses to start without it
  ("INITIALIZATION FAILED FOR GPGME"). RESTORE.sh checks and says so.
- dar 2.7.9 or later streams a restore with no disk space for slices;
  an older one, or a unit without its dar catalogue on a dar before 2.7.21,
  is restored by decrypting the slices to disk first (RESTORE.sh
  chooses, and checks the space either way). A unit across tapes is the
  exception: it is fed to dar only through pipes (only its encrypted slices
  are ever kept on disk), so it needs dar 2.7.9 or later with the unit's
  dar catalogue, or 2.7.21 or later, and with an older dar RESTORE.sh
  refuses it before it reads a slice ("A unit across tapes"). Before it
  reads a slice RESTORE.sh also checks that this computer's `head` and
  `dar` can join a unit's slices into the one stream dar reads; when they
  cannot, it says why, restores a unit on one tape from slices decrypted
  to disk instead, and refuses a unit across tapes.
  `dar --version` says which dar you have. A unit's dar catalogue is the
  small file `catalogs/NAME` in
  its envelope ("Words this guide uses"); "without its dar catalogue" means
  the envelope has none for that unit, or dar refuses the one it has
  because it was cut from another run of the same snapshot. RESTORE.sh
  says so when that happens.
- The oldest dar that reads a unit is the one that wrote it, or any newer
  one: dar reads every archive format that existed when it was released
  (its own documentation says so), and an older dar may refuse a format it
  does not know. MANIFEST.toml records the dar that wrote each unit, in its
  `dar_version` line (unpacked envelope: `grep '^dar_version'
  env/MANIFEST.toml`). `--find-envelope` with a tenant's key prints the
  whole MANIFEST.toml, but with an escrow or operator key it prints only a
  table of units: then unpack the operator envelope by hand (steps 5 and 6
  of "Manual Recovery Steps") and read `env/MANIFEST.toml` (the unit's uuid
  alone is in `--where --all`). ("Compressed units", in "Disk space", has
  the short way to one unit's `dar_command`.) If your dar is older than a unit's
  `dar_version`, install that version or a newer one before you give the
  unit up. 2.6 is the oldest dar tapectl writes with, not a promise that
  every dar from 2.6 on reads every unit. Where to get a newer dar: a newer
  release of your distribution may package one (`apt-cache policy dar` says
  which version it offers); otherwise dar's own site, dar.linux.free.fr
  (release notes and source tarballs), and its project at
  sourceforge.net/projects/dar (the same releases). A source tarball builds
  with the commands in its INSTALL and README files (for most releases
  `./configure`, `make` and `make install`, which name the libraries it
  needs). These addresses may move over decades: search for "dar" and its
  author, Denis Corbin, if they are gone.
- Only for finding the drive and checking a key, never for the restore
  itself: `ls`, `lsscsi` (package lsscsi), `lspci` (pciutils), `dmesg`
  (util-linux), `lsmod` and `modprobe` (kmod), `sudo` (sudo) and `usermod`
  (passwd) (to find the drive, load its driver and get permission to use
  it: "Loading the tape, and a drive that does not show up"), and
  `age-keygen`, which comes with `age` ("Which file is your key"). Each is
  named where this guide uses it.
- `sqlite3` (package sqlite3) — optional, only to read the Heir Kit's
  `catalog.db.age` ("The Heir Kit's catalogue"); no restore needs it.

On Debian 11 or later and Ubuntu 22.04 or later one command installs all
of these (`sg3-utils` is for tapectl itself, "tapectl itself"):

    sudo apt install mt-st coreutils tar age dar gnupg sqlite3 lsscsi pciutils kmod sg3-utils

Debian 10 has every one of those packages but `age`: install the others the
same way and take age from a release (github.com/FiloSottile/age/releases
has a Linux build that needs no installation). Package names are Debian's
and Ubuntu's; other systems name the same programs their own way.

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
restore, as `--find-envelope` and `--where` print them (each unit's slices
as stored on tape); three times that, and more for mostly text, when you
cannot tell that a unit was stored uncompressed ("Compressed units",
below), and watch `df` while it runs; and, for the ways of restoring that
keep its slices on disk as well (below), one more unit's size, for the
slices of the unit being restored. The rest of this section says why.

{capacity}

RESTORE.sh streams each slice from the tape through `age` into `dar`, so
`--to` needs room for about the size of the units you restore: one
unit's size for one unit, the sum of them with `--all` (each unit's
slices as stored on tape; a unit that was archived compressed restores
larger: "Compressed units", below). `--find-envelope` lists each unit with
its size, and so does `--where`. Nothing else is large: the decrypted
envelopes (the operator envelope's `catalog.db` and the dar catalogues,
which grow with the number of files) go to a work directory under
`$TMPDIR` (default `/tmp`; set `TMPDIR` to move it). `/tmp` is a small RAM
disk on many systems (`df -h /tmp` says), and no data slice ever goes
there, but a unit of millions of files has a large dar catalogue: if `/tmp`
is small, set `TMPDIR` to a directory on a real disk with some GiB free
(`TMPDIR=/big/disk/tmp ./RESTORE.sh ...`).

Some ways of restoring keep the slices on disk as well, in a scratch
directory inside `--to` unless `--scratch DIR` names another disk, and
then need more room, for a unit's slices beside its restored files: the older-dar fallback above (all of the unit's
slices), `--path` (only the few slices that hold the path), and asking
for a replacement slice (`--slices-from`, `--swap-tapes`: "If a slice or
a file is damaged"); and a unit across tapes, by default: all of its
slices, still encrypted, about the unit's size again beside its restored
files (the sum of its slices' `encrypted_bytes`; `--stream-tapes` keeps
none on disk: "A unit across tapes"). Doing it by hand ("Manual Recovery
Steps") is the
same: the decrypted slices (the unit's size) beside the restored files
(the unit's size, more if it was stored compressed); delete a unit's slices
once it is restored. The script checks the
space before it reads any slice.

For a whole box, the destination needs the sum of the sizes of the units
you restore, whichever tapes they come from; the extra room for slices is
needed for one unit at a time.

**Compressed units.** A unit restores to more than its slices' size if it
was stored compressed, by an amount that depends on the files (text
compresses a lot, photos and video hardly at all). MANIFEST.toml records
the dar command of each unit, in its `dar_command` line (unpacked
envelope: `grep '^dar_command' env/MANIFEST.toml`; with an escrow or
operator key `--find-envelope` prints only a table of units, so unpack the
operator envelope by hand, as under "Tools Required"). An argument that
starts `-z`, as in `\"-zgzip\"` or `\"-zzstd\"`, means the unit was
compressed, and none means it was not. The dar catalogue cannot tell you: `dar -l`
of the dar catalogue in the envelope shows `-----` in its compression
column either way.

The short way to the `dar_command` lines, with an escrow or operator key
and no unpacking of the whole envelope. `--find-envelope` prints "Decrypted
envelope at file N (tenant: operator)" for the operator envelope, the one
that lists every unit; call that N POSITION below. Read File 3 into
`front_index.toml` (step 3 of "Manual Recovery Steps") and run
`grep -A3 '^position = POSITION$' front_index.toml`: the entry it prints has
the `size_bytes` that is SIZE below. Then, with the block size set as in "Manual
Recovery Steps":

    mt -f /dev/nst0 rewind
    mt -f /dev/nst0 fsf POSITION
    dd if=/dev/nst0 bs=512k of=envelope.raw
    truncate -s SIZE envelope.raw
    age -d -i YOUR_KEY.age.key envelope.raw | tar -xOf - MANIFEST.toml | grep -E '^(name|snapshot_version|dar_command) ='

It prints each unit's name and version with its `dar_command`. Without the
last `grep` it prints the whole MANIFEST.toml: every unit with its uuid,
its slices and their tape files, which is what the other places in this
guide that unpack the operator envelope by hand read. If the envelope will
not read, use its backup (step 5 of "Manual Recovery Steps").

"Three times" is a rule of thumb for mixed files, not a ceiling: text,
source code, logs and database dumps compress ten times or more, so a
compressed unit of mostly such files can need far more than three times its
slices' size, and nothing on the tape says how much more. With no
`dar_command` line, or a compressed unit, restore to a disk you can add room
to and watch `df`. RESTORE.sh names a full disk when it happens, and
`--overwrite` finishes the restore once you have made room.

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
because it does not change when the numbering does:

    /dev/tape/by-id/scsi-XXXXXXXX-nst      (an example: use your own)

A wrong device does not always fail loudly: if another tape is loaded in
it, the commands below will succeed and recover the WRONG volume. File 0
names the volume this tape carries; RESTORE.sh checks that for you and
warns.

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
    dd if=/dev/nst0 bs=512k | tr -d '\0' > RESTORE-{label}.sh
    chmod +x RESTORE-{label}.sh

The script is saved under this tape's own name, `RESTORE-{label}.sh`, because
a box has several tapes and each has its own RESTORE.sh: scripts saved under
one name overwrite each other. Everywhere else this guide writes
`./RESTORE.sh`, it means the script of the tape that is in the drive: for
this tape, `./RESTORE-{label}.sh`; for another tape, the file you saved from
it.

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

Then, with `YOUR_KEY.age.key` your own key file:

    # See what's on the tape and its seal verdict
    # (SEALED / UNSEALED / DAMAGED (ends disagree)); the section
    # "If the verdict is UNSEALED or DAMAGED" says what to do
    ./RESTORE.sh --info

    # Keyless integrity check of every file against the front index —
    # no key needed, works even without your envelope
    ./RESTORE.sh --verify

    # Find your encrypted envelope (an operator or escrow key opens
    # every envelope, and the units of each are listed)
    ./RESTORE.sh --find-envelope --key YOUR_KEY.age.key

    # List a unit's files, from the dar catalogue in the envelope
    # (no data slice is read)
    ./RESTORE.sh --list --key YOUR_KEY.age.key --unit UNIT

    # Which other tapes hold a unit, or a file ("The rest of the box"),
    # and every unit the index names, with no limit
    ./RESTORE.sh --where UNIT --key YOUR_KEY.age.key
    ./RESTORE.sh --find some/file --key YOUR_KEY.age.key
    ./RESTORE.sh --where --all --key YOUR_KEY.age.key

    # Full restore of THIS tape to an EMPTY directory: --all restores
    # every unit of one envelope (below), each into /destination/UNIT (a
    # lone unit goes straight into /destination). Each
    # slice streams from the tape through age into dar, so /destination
    # needs about those units' size free; the script checks first.
    # --overwrite finishes a restore that stopped part way, never one over
    # an older version. With other tapes in the box, read "The rest of the
    # box" before this: a second --all into the same /destination is refused.
    ./RESTORE.sh --restore --key YOUR_KEY.age.key --all --to /destination

    # Just one file or folder: reads only the slices that hold it, into the
    # unit's own directory, where a whole-unit restore would put it
    ./RESTORE.sh --restore --key YOUR_KEY.age.key --unit UNIT \
        --path some/file --to /destination/UNIT

With the ESCROW key or the OPERATOR key, which opens every tenant's
envelope (`YOUR_KEY.age.key` is then that key's file), list what the tape
holds first, then restore all of it:

    ./RESTORE.sh --list --key YOUR_KEY.age.key
    ./RESTORE.sh --restore --key YOUR_KEY.age.key --all --to /destination

--all restores every unit of the envelope that lists the most units among
those your key opens, each in /destination/UNIT (a single unit goes
straight into /destination): with the escrow or operator key that is the
operator envelope, which lists every unit on the tape; with a tenant's key
it is that tenant's envelope. It is always ONE envelope, never the union of
several. With several keys (`--key` repeated) it is the biggest envelope any
of them opens, and the first on the tape if two tie: the escrow or operator
key, alone or with others, therefore restores every unit on the tape, but
two tenants' keys together restore only the tenant with more units. Restore
the other tenant in a run of its own, with that tenant's key, into another
empty directory. Without --all or
--unit, --restore takes the first envelope your key opens and, if that
lists one unit, restores only that unit (it tells you what else the key
opened). If that envelope lists SEVERAL units it will not guess: it
prints them and stops before it reads a slice, and you name one with
`--unit UNIT` (repeat `--unit` for several, each into /destination/UNIT)
or restore them all with `--all`. A
single TENANT's key opens only that tenant's envelope: it restores that
tenant's units and nobody else's.

If the operator envelope and its backup will not read, the biggest envelope
that does read may be a tenant's, and --all then restores only that
tenant's units, with the DAMAGED warnings the script printed as the only
sign. The script names the file it used ("Decrypted envelope at file N"). Item 4
of "Did the restore get everything?" is the check, and what to do when
it fails.

The script reads the tape forward from one rewind, and says what it
needs if it meets a tape newer than itself (`requires` in File 0).

## The rest of the box: other tapes, copies and the index

If you hold more than this one tape, work out first what the others are.
A tape may be a COPY of this one (the same units written again on another
cartridge, so that one can be lost), or hold OTHER units, or hold other
VERSIONS of a unit this tape holds: a unit's older versions need not be
on the same tape as its newest one.

A tape written by tapectl 1.3.0 or later carries an index inside its
envelopes that answers this: the directory `index/`, whose file
`units.tsv` is a table with a row for each version of each unit and each
tape that holds it. For each unit it lists every version and the tapes that
hold it (label, uuid, cartridge serial, generation, state and when it
sealed), and a tenant's envelope also has `paths.tsv`, which lists every
file path. RESTORE.sh reads it:

    # EVERY unit the index names (no limit): its newest version, whether
    # the tape in the drive holds it, and every tape that holds each version
    ./RESTORE.sh --where --all --key YOUR_KEY.age.key

    # every version of UNIT, and every tape that holds each
    ./RESTORE.sh --where UNIT --key YOUR_KEY.age.key

    # which unit, version and tape hold a path containing some text
    ./RESTORE.sh --find report --key YOUR_KEY.age.key

An index is as of the moment its own tape was written. It never names a
tape written after it, and a tape it names may since have been retired or
erased. So the NEWEST tape's index is the most complete: start with the
newest tape you have (`./RESTORE.sh --info` prints "Sealed at"), and
expect an older tape to know less. A tape written before 1.3.0 has no
index and says so. The index is plain text in the envelope (`index/` in
the unpacked envelope; RECOVERY.md shows how to read it with `awk`), so it
needs no tapectl either.

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
   (any copy of RESTORE.sh will do for this: see "The modes that only read
   the tape", after this list):
   its label is "Tape identifies as" (File 0's label is the authority; a
   sticker on the case can have been replaced) and its sealed date is
   "Sealed at" (`unknown (tape not sealed)` there means the tape is
   unsealed: File 0's `created_at`, from the `grep` below, says when it
   was written). The generation is printed on the cartridge (L6 is LTO-6)
   and stated in File 0 as `media_type`; tapes of different generations
   need different drives ("The tape drive" has a table, so that you can
   choose one drive for the box).
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
   is not empty is refused), which is why each unit has its own directory
   under /destination and not /destination itself. A tape that holds the
   newest version needs no `--version`: a restore takes the newest version
   on the tape. Use THAT tape's own RESTORE.sh, File 2 of the tape in the
   drive, which reads that tape's format.
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
     damaged": RESTORE.sh, then tapectl's own restore, which saves more of
     a damaged slice), its `.tapectl-damaged-files.txt` is kept beside it,
     and you have written down that the files on that list are lost. An
     older version, restored into a directory of its own, may hold older
     copies of them.
   - A tape your key cannot open is NOT done. `./RESTORE.sh` ends with "no
     envelope matched the key provided" (or "any of the N keys provided"), and
     `--info`, which needs no key, shows only the tape's label, dates and
     map, never what it holds. Do not count that tape as looked at: name it
     (a tape of another installation, or a key you do not have) and try
     every other key you hold on it. If none opens it, the box is not done,
     and you say so, by that tape's name.
   - A tape written before 1.3.0 has no index. It counts by its own
     envelopes alone, and nothing on it names a tape or a unit but its own.
   - An unsealed tape (no "Sealed at") is ordered by File 0's `created_at`.
     If the newest tape in the box will not read, the older tapes' indexes
     cannot see what was written after them: say that some units may be
     unknown.
   That is the most a box can show about itself: "Is the box complete?"
   above says what it cannot.

   The newest version of a unit holds the files that version had. A file
   deleted before it was made exists only in an older version, so restore an
   older version (`--version N`, into a directory of its own) only if you
   want such files.

**Files deleted before the newest version.** Nothing says which files exist
only in an older version, but you can see: list the paths of two versions
and print those the older one has and the newer one lacks. `--list` takes
`--version`, and needs both versions on the tape in the drive and their dar
catalogues in its envelope (a version without one prints "no dar
catalogue"). If the two versions are on different tapes (`--where UNIT`
says which), run each `--list` with its own tape loaded. With versions 3
(the newest) and 2 of UNIT, the last command prints the paths of version 2
that version 3 does not have:

    ./RESTORE.sh --list --key YOUR_KEY.age.key --unit UNIT --version 3 | grep -E '^\[.{{5}}\]\[.\] ' | awk -F'\t' '{{ print $NF }}' > newer.paths
    ./RESTORE.sh --list --key YOUR_KEY.age.key --unit UNIT --version 2 | grep -E '^\[.{{5}}\]\[.\] ' | awk -F'\t' '{{ print $NF }}' > older.paths
    awk 'NR == FNR {{ seen[$0]; next }} !($0 in seen)' newer.paths older.paths

Each entry line of dar's listing ends with the path, after its last TAB (a
path with a TAB in it is cut at the TAB). Nothing printed means version 3
has every path version 2 had. To restore one that is printed, from version
2, into a directory of its own:

    ./RESTORE.sh --restore --key YOUR_KEY.age.key --unit UNIT --version 2 \
        --path PATH --to /destination/UNIT-v2

**A renamed unit.** A unit is the same unit by its `uuid`, but every
command takes its NAME, and a tape's envelope knows a unit by the name it
had when THAT tape was written. If the unit was renamed after an older tape
was written, the newest index lists it under the new name (with the older
tape as a holder of an earlier version), and the older tape's envelope still
says the old name. `--unit NEWNAME` finds nothing there: RESTORE.sh says an
envelope "decrypts but does not list" the name, then ends "no envelope for
unit 'NEWNAME' matched the key provided". That reads like a wrong key and
is not one. Take the unit's uuid from the newest index (`--where --all`
prints it as "unit uuid", beside the name), and on the older tape type the
name THAT tape shows for the same uuid:

    # the names this tape knows, each with the first eight characters of its
    # uuid (catalogs/UUID8_vN); it lists every file too, so it can take a while
    ./RESTORE.sh --list --key YOUR_KEY.age.key | grep '^==='
    # a tape written by 1.3.0 or later: the names with the full uuids
    ./RESTORE.sh --where --all --key YOUR_KEY.age.key

A unit stored without its dar catalogue has no `catalogs/` name to read:
`--find-envelope` with a tenant's key prints the whole MANIFEST.toml, and
"Manual Recovery Steps" (step 6) reads it from an unpacked envelope, each
unit's `name` beside its `uuid`; with an escrow or operator key
`--find-envelope` prints only a table of units (see "Tools Required"), so
take the uuid from `--where --all`, or print the operator envelope's
MANIFEST.toml ("The short way", in "Compressed units").

**Two units of one name.** A name is unique inside one installation's
catalogue, but it can recur across tapes: a unit renamed and its old name
used again, or tapes of two installations. They are different units when their uuids differ
(`--where --all` prints each as an entry of its own, with its uuid). The
box procedure puts each unit in `/destination/UNIT`, and two such units
would meet there. RESTORE.sh refuses a `--to` that is not empty, but the
manual dar path keeps what is already there and exits 0, so it merges them
without a word. Restore each into a directory of its own,
`/destination/UNIT-` followed by the first 8 characters of its uuid, and
with the manual path always into a new, empty directory.

The modes that only read the tape (`--info`, `--verify`, `--find-envelope`,
`--list`, `--where`, `--find`) describe the tape that is in the drive,
whichever tape's RESTORE.sh runs them (`--where` and `--find` need the
script of a tape written by tapectl 1.3.0 or later; an older one answers
`Unknown mode`). A RESTORE.sh written for another
tape prints a "WRONG TAPE?" box first, and for these modes it is only a
reminder that the script was written for another volume: what follows is
about the tape in the drive (the `Tape identifies as:` line, which every mode
prints, is that tape's own label; the heading `--verify` prints names the
script's volume, but it checks the tape in the drive). For a
restore use the loaded tape's own RESTORE.sh, which was written for that
tape's format, and take "WRONG TAPE?" during a restore as a sign that the
script or the tape is the wrong one. To get a tape's own script, load that
tape and run the four commands at the start of "Automated Recovery
(recommended)"; they read File 2 of whatever tape is in the drive. In the
last two put THAT tape's label (the `Tape identifies as:` line of `--info`)
in place of this tape's, so that each tape's script is kept under its own
name, `RESTORE-<label>.sh`.

In the index's `state` column, `sealed` is a tape whose write finished and
was sealed, `quarantined` is a sealed tape whose medium a verify proved bad
(it may still read in part, so prefer another holder), and `this-tape` is
the tape the index is on. A tape that was already retired or erased when
the index was written is not listed, and one that is listed may have been
retired or erased since. A cartridge that was erased and written again
carries a NEW volume: File 0 then shows another label and another `uuid`
on the same `cartridge_serial`, and the index row for the old volume names
data that is gone. So match a tape to an index row by its label AND its
uuid (`--where` prints the uuid; File 0 shows it):

    mt -f /dev/nst0 rewind
    dd if=/dev/nst0 bs=512k | tr -d '\0' | grep -E '^(label|uuid|media_type|cartridge_serial|created_at|tapectl_version) ='

To take stock of a box without relying on the index, load each tape in
turn, run `./RESTORE.sh --info` (its label and when it was sealed) and
`./RESTORE.sh --find-envelope --key YOUR_KEY.age.key` (its units, their
versions, slices and sizes), and write them down. A unit that appears at
the same version AND with the same unit uuid on two tapes has two copies
only when each of the two tapes holds that version WHOLE: then they are the
same content, and either restores it alone. A tape that holds only part of
a unit across tapes ("A unit across tapes") is not a copy: two tapes that
each hold part of a unit are ONE copy, and restore it only together. To
tell, run `./RESTORE.sh --list --key YOUR_KEY.age.key --unit UNIT` on the
tape: it says "HELD ACROSS TAPES" when the tape holds only part of that
version, and `--where UNIT` marks such a tape "a range". The table of
`--find-envelope` marks such a unit `PART:` from the tape's plan alone; a
tape that ended early ("Where the tape ended") can hold less than its plan,
and `--list` and `--where` know it. A name and a version
number alone prove nothing; the uuid is in MANIFEST.toml (`--find-envelope`
with a tenant's key prints it) and, on a tape with an index, in `--where`.
Cartridges are found by their label; the cartridge's chip serial is in
File 0 (`cartridge_serial`) and in the index, and a sticker can have been
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
`of_slices` (a unit across tapes, from tapectl 1.7.0: "A unit across
tapes"). A `range` tape is not a copy on its own: the version restores only
with every slice, 1 to `of_slices`, from the tapes that hold them, and its
rows together say which tapes those are. A kit made before 1.7.0 has no
`range` rows.

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

## If the verdict is UNSEALED or DAMAGED

`./RESTORE.sh --info` prints a `Verdict:` line, a verdict on the tape as a
whole:

- SEALED: the seal marker (the last file) is there and agrees with the
  front index. The write finished and the two ends of the tape match.
  Carry on. A SEALED tape may still have ended early, and `--info` then
  says after which file: "Where the tape ended".
- UNSEALED: there is no valid seal marker. The write never finished: it
  was interrupted or aborted, or the tape ran out of room. Envelopes are
  written before slices, and the front index before both, so the front
  index lists EVERY file the write planned, slices that were never written
  included: such a slice shows as FAIL ("unreadable") below, and what is
  missing is normally the last data slices. What was written before the
  break is intact and checks against the index.
- DAMAGED (ends disagree): a seal marker is there, but it does not match
  the front index, or the front index will not read. One of the two text
  files was damaged or replaced since the write, and neither can vouch for
  the other. What you can check yourself is each file against the hash the
  front index lists for it, which is what follows.

In both cases go file by file, because each file is checked against its
own recorded hash:

    ./RESTORE.sh --verify

It prints PASS or FAIL for every tape file, by number; a file that is
missing or will not read is a FAIL. To find which slices are incomplete,
take the tape file numbers of a unit's slices from its MANIFEST.toml
(`--find-envelope` with a tenant's key prints it, and RECOVERY.md's table
of "Tape files") and look them up in that list. The operator and escrow
keys open several envelopes, and `--find-envelope` then prints only a table
of units: for the slices of every unit, decrypt the operator envelope by
hand (steps 5 and 6 of "Manual Recovery Steps") and read `env/MANIFEST.toml`,
which lists each unit with its slices and their tape files. RESTORE.sh
itself reads the front index from File 3 and, if that fails, from the seal
marker's copy of it, with a warning (rung 2 of "If All Else Fails").

- A unit whose slices all PASS is complete: restore it as usual. RESTORE.sh
  does not need the seal, and checks every slice again as it restores.
  That holds for the tape in the drive when the restore starts. Every LATER
  cartridge, of a unit across tapes, and every cartridge `--swap-tapes`
  reads for a replacement slice, is refused without a readable seal marker:
  a slice is taken from another tape only at a position its own seal says
  that tape holds ("A unit across tapes"). With such a tape as a later part
  of a unit across tapes, read its slices by hand (step 7 of "Manual
  Recovery Steps"), each checked against the `sha256_encrypted` the
  manifest gives, into one directory with the other tapes' slices of the
  unit, and run dar there once every slice is in it. For a slice of a whole
  unit, copy it off with `dd` and give it to `--slices-from DIR` ("If a
  slice or a file is damaged"), which matches by sha256 alone.
- If every file PASSes against the front index and only the seal
  disagrees, the data is exactly what that index describes (each file
  matches its recorded hash): the fault is in the seal marker or the
  index, it does not matter which for restoring, and restoring is sound.
- A unit with a FAIL slice cannot be restored whole from this tape.
  RESTORE.sh stops at the first slice that fails its check and says which.
  `--all` restores the units in their order on the tape, so it stops there
  too: the units before that one are restored, that unit and every unit
  after it are not. Restore each unit after it in a run of its own,
  `--unit UNIT --to /destination/UNIT` (as in item 4 of "Did the restore
  get everything?"), and the damaged one as below.
  Look for another copy first ("The rest of the box"; "If a slice or a
  file is damaged" says how RESTORE.sh takes the failed slice from it); a
  cartridge that failed here may read in another drive. With no other copy,
  the slices that did PASS are intact, and the files that lie wholly
  inside them can still be restored: RESTORE.sh does it (`--swap-tapes`,
  then type `done`), and "No other copy", in "If a slice or a file is
  damaged", has that and the commands by hand. The files in the failed
  slices are lost. Not so for a unit across tapes when no tape you hold has
  that version whole ("A unit across tapes"): RESTORE.sh refuses
  `--swap-tapes` for it, `done` at its prompt stops the restore, and it
  restores nothing of the unit without every slice. "No other copy" names
  the ways that do work for it.

## Where the tape ended

A tape written by tapectl 1.6.0 or later states `requires = ["short-seal"]`
in File 0 and in its seal marker. It means the tape may end before the last
file the front index plans: when the drive reported that its medium was
nearly full while it wrote a data slice that was not the last, the write
finished that slice and sealed the tape right after it. File 0, the front
index and the envelopes were written before that, so they describe the
plan, and only the seal marker, the last file on the tape, says where the
tape really ended. Its `ended_after` is the last tape file before the seal
marker, and its `file_count` is `ended_after` plus 2. Every tape file the
front index lists after `ended_after` is not on this tape. A tape that
sealed where the plan said states it too: there it is the file before the
planned last.

To read it by hand, with no key (`/dev/nst0` is an example: "Which
device?"):

    mt -f /dev/nst0 setblk 524288
    mt -f /dev/nst0 eod          # GNU mt: eom
    mt -f /dev/nst0 bsfm 2
    dd if=/dev/nst0 bs=512k | tr -d '\0' | grep -E '^(file_count|ended_after) ='

A unit whose slices run past `ended_after` (the `tape_position` of its last
slice in MANIFEST.toml, the last of its "Tape files" in RECOVERY.md, is
after it) is not on this tape whole: do not start reading its slices. Its
first slices are there and hash correctly, the rest are not, and it is
restored only from another tape that holds it whole. The index names that
tape: `./RESTORE.sh --where UNIT --key YOUR_KEY.age.key` ("The rest of the
box"). Every unit wholly at or before `ended_after` is on this tape whole
and restores as usual. A unit allowed to run across tapes (its block in
MANIFEST.toml states `may_span = true`) with a slice at or before
`ended_after` is not cut: this tape holds a range of it, and its other
slices are on other tapes by design ("A unit across tapes").

RESTORE.sh does all of this by itself, reading the seal marker before any
slice (one wind to the end of the tape and a rewind). `--info` says where
the tape ended (the verdict stays SEALED), and `--info` and `--verify` print
one `NOT ON TAPE` line for the planned files after `ended_after`, naming
the first and the last of them, which is not a failure:

    NOT ON TAPE  files 12 to 15  (planned; the tape ended after file 11, and its seal marker is file 12)

`--restore` refuses a unit the end of the tape cut, by name (`--all`
restores the others and names it), `--list` says so, and `--where` marks
this tape `CUT` for that version.

## A unit across tapes

From tapectl 1.7.0 a unit that was allowed to run across tapes can have its
slices on more than one tape. A tape that holds only some of them holds a
RANGE: one unbroken run of the unit's slices, in slice order. Such a tape
states `requires = ["short-seal", "spanning"]` in File 0 and its seal
marker, and so does a tape that holds the whole of such a unit, which
restores from that one tape as any unit does. A range is not damage, and it
is not a copy of the unit on its own.

What one tape shows. Its envelope's MANIFEST.toml lists EVERY slice of the
unit, 1 to N, each with its `encrypted_bytes` and its `sha256_encrypted`,
and gives a `tape_position` only to the slices this tape's plan holds. A
slice with no `tape_position` is on another tape: it is never at tape file
0. The seal marker's `ended_after` can end the run sooner, as it ends any
tape ("Where the tape ended"), and a slice after it is on another tape too.
`--info` and `--verify` need no key and see the range as data slices like
any other. `--list` says which slices of the unit this tape holds.
`--restore --all` restores the units this tape holds whole and names each
one it holds only a range of. `--where` and `--find` print the run of
slices each tape holds when it is not the whole version, marked "a range".

What a restore needs: every slice, 1 to N, in slice order, from the tapes
that hold them. dar never runs over one tape's slices alone.

Prefer a whole copy: if any tape holds the version whole (`--where` prints
no "a range" beside it), restore from that tape alone, as any unit, with
no change of cartridge and no scratch. A whole copy written from another
staging run of the same version has slices of other sha256 values: the
prompt below refuses it by hash, though `--where` names it, and it still
restores alone.

If none does, read the version across tapes. Start from the
tape that holds slice 1: `./RESTORE.sh --where UNIT --key YOUR_KEY.age.key`
on the NEWEST tape names the tapes and the slices each holds (a tape never
names a tape written after it, so the first tape of a unit across tapes
cannot name the rest). Before you start, write down every tape it names
and the slices each holds: the restore asks for the next slice by number,
the tape then in the drive can name only tapes written before it, and to
run `--where` on the newest tape again you would have to change cartridges
in the middle of the restore. Load the tape that holds slice 1 and run
THAT tape's RESTORE.sh (saved as `RESTORE-<label>.sh`, as for any tape):

    ./RESTORE.sh --restore --key YOUR_KEY.age.key --unit UNIT --to /destination/UNIT

When the tape's run ends before slice N, it names the slice it needs
next and the tapes an index names for it, and waits for you to load another
cartridge and press ENTER. It takes a cartridge only when that tape's front
index lists the next slice's `sha256_encrypted`, at a position its own seal
marker covers; a label or an index never stands in for the hash. Otherwise
it says why and asks again.

By default it keeps each tape's slices, still encrypted, in a scratch
directory inside `--to` (or `--scratch DIR`), so that disk needs room for
the whole unit's encrypted slices (the sum of their `encrypted_bytes`)
besides the restored files; it checks before it reads a slice, and when the
room is not there it refuses and names `--stream-tapes`. `--stream-tapes`
keeps no slice on disk: it feeds dar from each tape in turn, so it needs
every cartridge at hand, loaded when asked.

Either way it waits at each change of cartridge for as long as you take,
reading your answer from its input, and `quit` stops it. So does `done`:
here it does not go on without the slice, as it does after `--swap-tapes`
("If a slice or a file is damaged"); either one ends the restore and
discards the slices read so far (with `--stream-tapes`, the files dar has
written are only part of the unit). A slice that fails
its hash or will not read is asked for again, by default, from another
cartridge that holds it; with `--stream-tapes` it ends the run.

Ctrl-C, a closed terminal or a lost connection to the computer, and `kill`
end the run too, and REMOVE its scratch directory with every slice read so
far: a restore across tapes can take hours of cartridge changes, so run it
inside `screen` or `tmux`, where a closed window or a lost connection does
not end it. Only a run that had no chance to clean up (`kill -9`, a crash, a
power cut) leaves its scratch directory inside `--to` (or `--scratch DIR`),
named `.tapectl-restore.` and some letters and holding encrypted slices
only: remove it by hand before you run the restore again, which starts
from the beginning. A dar older than
2.7.9 (or than 2.7.21, when the envelope holds no dar catalogue of the unit)
cannot read slices this way, and RESTORE.sh says so before it reads one. It
says so too on a computer whose `head` or `dar` cannot join the slices into
the one stream dar reads, and names which.

`--restore --unit UNIT --path PATH` reads across tapes too, and needs only
the slices that hold the path and the last one. It can start on any tape of
the unit that your key opens: it takes the slices that tape holds first,
then asks for each further cartridge in slice order, taken by hash in the
same way. They wait, still encrypted, in the scratch directory until all
are read, and only then are they decrypted for dar.

## Did the restore get everything?

These checks are for ONE restore, of one unit or one tape's worth, from one
tape. They say nothing about the other tapes and units of a box: a box is
complete only when the procedure in "The rest of the box" has been followed
to its end.

A restore went right only if all of these hold:

1. RESTORE.sh ended with `RESTORE COMPLETE` and exit status 0 (`echo $?`
   straight after it). It prints that only after dar ended with status 0,
   every slice printed "checksum verified against front index", and dar
   skipped no file. `RESTORE INCOMPLETE` (exit status 1) means dar named
   files it could not restore: they are listed, and "If a slice or a file
   is damaged" says how to fill them. Any other ending: read its message,
   and do not trust what was written.
2. dar's own summary, printed at the end, shows nothing failed or skipped:

       150 inode(s) restored
       0 inode(s) not restored (not saved in archive)
       0 inode(s) not restored (overwriting policy decision)
       0 inode(s) ignored (excluded by filters)
       0 inode(s) failed to restore (filesystem error)

   Every line but the first must be 0. "Overwriting policy decision" means
   dar left a file that was already there: the destination was not empty.
   "Ignored (excluded by filters)" above 0 in a whole-unit restore means dar
   skipped files, as it does for a slice that was left out ("No other
   copy" in "If a slice or a file is damaged").
3. The count matches the dar catalogue. The inodes (files and directories)
   a unit's dar catalogue lists are the lines of `--list` that begin with a
   status such as `[Saved]` or `[InRef]`:

       ./RESTORE.sh --list --key YOUR_KEY.age.key --unit UNIT \
           | grep -c -E '^\[.{{5}}\]\[.\] '

   That number should equal the "inode(s) restored" figure of a whole-unit
   restore. For a few files, compare their sizes and a checksum with any
   record of your own. A unit stored without its dar catalogue has no
   listing (`--list` says "no dar catalogue"): for it, items 1 and 2 are
   the check, with RESTORE.sh's sha256 check of every file on a tape
   written since 1.3.0 ("A file fails its hash").
4. After `--all`, every unit of the tape was restored, not only one
   envelope's. `--all` restores the units of ONE envelope, the one that
   lists the most: with the escrow or operator key that is the operator
   envelope, which lists every unit on the tape, but when the operator
   envelope and its backup will not read it is a tenant's, and only that
   tenant's units are restored, while items 1 to 3 still pass. Every unit
   that `./RESTORE.sh --find-envelope --key YOUR_KEY.age.key` lists, under
   any envelope, must be a directory of the `--all` destination (a tape of
   one unit puts that unit's files in the destination itself, and that is
   the unit's directory). Restore each one that is not in a run of its
   own, `./RESTORE.sh --restore --key YOUR_KEY.age.key --unit UNIT --to
   /destination/UNIT` (the script finds the envelope that lists it), and
   check that run the same way.

Two restores print a different summary. With `--all`, or `--unit` given
more than once, dar runs once for each unit and prints one summary for
each: check every one, and compare each against that unit's `--list`. With
`--path`, dar restores only what you asked for, plus the directories above
it (a file `a/b/c.txt` brings `a` and `a/b` with it, so 3 inodes), or
everything under a directory you asked for; "inode(s) ignored (excluded by
filters)" then counts every other inode of the unit, which is expected and
not a failure. What to check there is that "failed to restore" and "not
restored" are 0, and that "restored" is the number of inodes you asked
for, the path and the directories above it. `--fill-from` restores only
the files on its list, and a `--swap-tapes` restore where you typed `done`
leaves out the files of the missing slices, so lines after the first are
above 0 there by design: check instead that every file it did not restore
is on `.tapectl-damaged-files.txt` (item 1).

## If a slice or a file is damaged

Two things can go wrong with a unit that has a good copy somewhere else,
and RESTORE.sh mends both from that copy. A COPY here means the same unit at
the same version written to another tape: its slices are the same bytes as
this tape's, and its front index lists the same sha256 for each.

**A slice fails.** `./RESTORE.sh --verify` says FAIL for it, or a restore
stops with "checksum MISMATCH" or "did not decrypt". Give RESTORE.sh the
slice from another tape. Matching is by that sha256 and nothing else, never
by the number or position of a slice or by a file name:

    # A directory of slice files copied off the other tape, under any names.
    # A copy made with dd as it is, block padding and all, will do.
    ./RESTORE.sh --restore --key YOUR_KEY.age.key --unit UNIT \
        --slices-from DIR --to /destination/UNIT

    # Or let the script ask for the other cartridge once it has read this
    # one (it says which slices it needs, by number and sha256, and waits
    # for ENTER; type done to go without)
    ./RESTORE.sh --restore --key YOUR_KEY.age.key --unit UNIT \
        --swap-tapes --to /destination/UNIT

`--verify` takes the same two options and says whether a copy of each failed
slice was found. `--swap-tapes` reads the other cartridge's seal marker
first, and takes a slice from it only at a position the seal says the tape
holds ("Where the tape ended"): a cartridge whose seal marker cannot be read
gives none. The other cartridge may hold only part of a unit held across
tapes ("A unit across tapes"); it gives the slices it holds. To copy a slice
off the other tape by hand, find its tape file in THAT tape's front index
(File 3) by its sha256 (the entry's `position` is POSITION), then:

    mt -f /dev/nst0 rewind
    mt -f /dev/nst0 fsf POSITION
    dd if=/dev/nst0 bs=512k of=slice-copy.bin

Either option restores slice by slice onto disk, as with an older dar, so
`--to` needs room for the unit's slices beside its restored files ("How much
room, in one place", in "Disk space"; `--scratch DIR` puts the slices on
another disk). Each tape is read forward from one rewind. A slice RESTORE.sh
cannot replace is left out: the files in it are not restored but listed,
below. RESTORE.sh runs the `age` program, which stops at the first damaged
part of a slice, so it cannot save the good parts of a slice; tapectl's own
restore can ("No other copy", below).

**A file fails.** When dar names files it could not restore (a checksum that
failed, a slice that was not there), RESTORE.sh restores the rest, ends
`RESTORE INCOMPLETE` with exit status 1, and writes the paths of those files,
one per line, to `.tapectl-damaged-files.txt` in the unit's directory: `--to`
itself when the restore had one unit (a lone unit goes straight into `--to`,
`--all` included), `--to/UNIT` when it had several. A path is written as
the index writes one: a backslash as `\\`, a TAB as `\t`, a newline as
`\n`, a carriage return as `\r`. The first line records the unit, its uuid
and its version. The box procedure restores each unit with `--to /destination/UNIT`, so its
list is `/destination/UNIT/.tapectl-damaged-files.txt`. A tape that holds ONE
unit, restored with `--all --to /destination`, puts the files straight into
`/destination`: its list is `/destination/.tapectl-damaged-files.txt`, and
that directory is the `--to` below. Load a tape that holds
the same unit at the same version WHOLE (a tape that holds only part of a
unit across tapes is refused: "A unit across tapes") and run ITS
RESTORE.sh, with the list and `--to` of that same unit directory:

    ./RESTORE.sh --restore --key YOUR_KEY.age.key --unit UNIT \
        --fill-from /destination/UNIT/.tapectl-damaged-files.txt \
        --to /destination/UNIT

It reads only the slices that hold those files, restores exactly them, over
their damaged copies, and nothing else in the directory, and dar checks each
file it restores. The list is then rewritten with whatever is still damaged
(from yet another copy, run it again) and removed when nothing is. Only a
tape whose envelope lists the same unit, with the same uuid, at the same
version is used; any other is refused by name, because another version can
differ in the very files that are damaged.

When the slice that holds a folder's own entry is lost, dar names the
folder and restores nothing in it. On a tape written since 1.3.0 the list
names the files the index records under that folder instead, and the fill
restores them like any other. A tape written before 1.3.0 has no index, so
the list names the folder itself, and the fill restores what is missing in
it and overwrites nothing that is there.

**A file fails its hash.** dar checks each file it restores with a short
checksum, not a hash, and two equal flips in a file can cancel: a damaged file
can come back with no error from dar. A tape written since 1.3.0 records the
sha256 of every regular file in the index of its tenant envelope, and after
each restore and each fill RESTORE.sh reads the restored files back from
`--to` and checks them against it with `sha256sum`. A file that does not match,
or is missing, goes on the same list as a file dar named, and the run ends
`RESTORE INCOMPLETE`: fill it from another copy the same way. The check costs
one read of the restored files from disk and none of the tape; with `--path`
only the paths restored are checked. A file with no sha256 recorded is not
checked, and a tape written before 1.3.0 has no index and is not checked at
all; RESTORE.sh says both. `--no-file-check` skips the check.

**No other copy.** If a slice fails and no other tape or directory has a
copy of it, the slices that passed are intact, and the files that lie wholly
inside them can still be restored; the files that touch a failed slice are
lost.

RESTORE.sh does this for you: run the `--swap-tapes` restore above and type
`done` when it asks for another cartridge. It restores the files it can,
lists the others in `.tapectl-damaged-files.txt` ("A file fails", above) and
ends `RESTORE INCOMPLETE`. It needs the room of a replacement-slice restore
("Disk space").

Not for a unit across tapes when no tape you hold has that version whole
("A unit across tapes"): RESTORE.sh takes no `--swap-tapes` or
`--slices-from` for it, and `done` at its prompt stops the restore with
nothing of the unit restored. What does work for such a unit:

- `--restore --unit UNIT --path PATH` restores a folder or a file whose
  slices all avoid the failed one. It reads only the slices that hold the
  path, and the last slice, which must therefore be good; the `dar -l` line
  below shows which slices hold each file.
- tapectl's own restore reads a unit across tapes, asking for each
  cartridge, and reads around a damaged 64 KiB piece of a slice, losing
  only the files in that piece ("tapectl itself").
- By hand, as below: step 7 of "Manual Recovery Steps" reads each tape's
  slices of the unit into the one directory, matched by their
  `sha256_encrypted`, and the failed slice is left out.

By hand, without RESTORE.sh, do the steps of "Manual Recovery Steps" for the
unit, but leave the failed slices out: do not decrypt them, and do not put
empty files in their place. The decrypted slices keep their own numbers
(`restore.1.dar`, `restore.2.dar`, `restore.4.dar` ..., a gap where the
failed slice was); this is the one time a gap in the numbering is right.
Then, into an empty directory:

    dar -x restore -R /destination -O -Q -N > dar.log 2>&1
    echo $?
    grep 'not restored (user choice)' dar.log

dar does not stop at the gap. It restores every file whose slices are all
there. For each of the others it prints `PATH not restored (user choice)`
(the last command lists them) and leaves a file of that name that is EMPTY
or short: that is not the real file, so delete it, or fill it from another
copy later. The exit status is 0 and the summary counts those files under
"ignored (excluded by filters)", so a status of 0 does not mean the unit is
whole. If a directory's own entry lies in a lost slice, dar skips
everything under it ("No file in this directory will be restored"). On a
tape with an index, the `sha256sum` line in RECOVERY.md ("If a slice or a
file is damaged" there) names every restored file that does not match,
the empty leftovers included. `dar -l env/catalogs/NAME -T slice -N -Q`
(NAME as under "Words this guide uses") shows which slices hold each file,
so you can see beforehand what a lost slice costs. If the failed slice is
the LAST one, dar stops at once ("The last file of the set is not present",
exit status 4) and restores nothing; add `-0` (sequential read) and it
restores the files in order up to the first missing slice, then stops with
status 4. The file it was in the middle of is left short and named, and
those after it are not named at all, so compare with the dar catalogue's list.

Neither way can read around a damaged part of a slice: the `age` program
stops at the first damaged piece. tapectl can. tapectl is the program that
wrote this tape (File 0 names its release in `tapectl_version`); it is
open-source software, and its own restore reads around a damaged 64 KiB
piece of a slice, loses only the files in that piece and names them, and
fills files from another copy of the same version. "tapectl itself", next,
says where to get it and what to run; nothing else in this guide needs it.

## tapectl itself

Look in the Heir Kit first. A kit made by tapectl 1.5.0 or later holds a
copy of the program that made it, in the folder with `catalog.db.age`: a
file named `tapectl-` and its version, `tapectl-X.Y.Z` below (such as
`tapectl-1.5.0`). The cover sheet names it with its size and sha256 (check
the file against the sheet with `sha256sum`) and says it is for Linux on
one machine type (`uname -m` says yours). It needs no building: run it as
`./tapectl-X.Y.Z` wherever this section says `tapectl`, after
`./tapectl-X.Y.Z --version` has shown it starts. Copy it to a Linux disk
first and run `chmod +x` on it: from a USB stick formatted FAT or exFAT,
or mounted `noexec`, it answers "Permission denied". Its release must be
File 0's `tapectl_version` or newer (the `grep` of File 0 in "The rest of
the box" prints it); a kit made before the tape was written can hold an
older one. With no kit, a copy that
is too old, or one that will not run on your machine, read on.

tapectl is open-source software, and its source is at
https://github.com/mikmorg/tapectl: one place to look, and a web address
may move over decades, so search for the name if it is gone. File 0's
`tapectl_version` names the release that wrote this tape; the repository
tags each release `v` followed by that number (such as `v1.5.0`); use that release
or a newer one. It is built from source. It needs the Rust toolchain the
repository pins in `rust-toolchain.toml` (`rustup`, from rustup.rs,
installs it on the first `cargo` run, or when you run `rustup toolchain
install` in the checkout; a distribution's own cargo may be too old), a
network connection to download its libraries, and `dar`, `mt-st` and
`sg3-utils` installed ("Tools Required"):

    git clone https://github.com/mikmorg/tapectl
    cd tapectl && git checkout vX.Y.Z      # X.Y.Z: the tapectl_version
    cargo build --release                  # the program: target/release/tapectl

Then use it in a new home made for this recovery: set the environment
variable `TAPECTL_HOME` to an empty directory once (`export
TAPECTL_HOME=/recovery/tapectl-home`), or give `--home DIR` right after
`tapectl` on each command; under `sudo`, which drops the variable, give
`--home DIR`. ESCROW_PUBLIC is the `age1...` line of the cover sheet, the
escrow key's public half (with only the key file, `age-keygen -y
YOUR_KEY.age.key` prints it), and YOUR_KEY.age.key is your key file, the
escrow key's included:

    tapectl init --operator OPERATOR_NAME --escrow-public-key ESCROW_PUBLIC
    tapectl catalog rebuild --from-volume --device /dev/nst0 --key YOUR_KEY.age.key
    tapectl restore unit --unit UNIT --device /dev/nst0 --to /destination/UNIT --key YOUR_KEY.age.key

`init` makes the home and registers the escrow PUBLIC key, which decrypts
nothing; run it once. OPERATOR_NAME is the operator's name where you know it
(the Heir Kit's catalogue has it: `sqlite3 catalog.db "SELECT name FROM
tenants WHERE is_operator = 1;"`); otherwise give any name, your own say,
because the rebuild takes the tenants' names from the tape. (Run as an
ordinary user, `init` without `--operator` names the operator after your
login; run as root, under `sudo`, it refuses without one.) `catalog rebuild` reads the loaded tape and
puts what it says into tapectl's own catalogue (its database, in the new
home); run it once for each tape. `restore unit`
restores the newest version of a unit on the loaded tape into an empty
directory (`--version N` for another). Use your own device ("Which
device?"). The repository's `docs/keys-and-recovery.md` (runbook C) is the
long form.

`--key` names a key file for that one command and nothing more: tapectl
does not copy it into its home or register it, and the escrow secret is
never stored on the machine. Give it on each command, and once for each key
file if the units need more than one (`--key` repeats). With the escrow key
this route does what RESTORE.sh cannot: it reads around a damaged 64 KiB
piece of a slice, loses only the files in that piece and names them.

The alternative, with the operator's key file: adopt it at `init` with
`--operator-key OPERATOR.age.key` beside `--escrow-public-key`, and leave
`--key` off the restore, which then reads the key from its home's `keys/`.
`init` refuses the escrow secret as `--operator-key` (that secret is kept
on paper, apart from every machine); `--key` is how the escrow key is used.

## Manual Recovery Steps

If RESTORE.sh is not available, follow these steps with `mt`, `dd`,
`truncate`, `age`, `tar`, `sha256sum` and `dar`. They use the non-rewinding
device ("Which device?"; `/dev/nst0` is an example). Each `dd` that finishes
leaves the head at the start of the next file, so `mt fsf N` moves forward N
files from where you are, and `mt rewind` then `mt fsf N` reaches File N from
anywhere. A `dd` that stops with an error ("Input/output error") leaves the
head INSIDE that file, not at the start of the next: after any failed read,
`mt rewind` and `mt fsf` to the file you want next, whatever the counting
below says. Read the tape forward where you can; rewinding is slow.

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
   reaches the same place on this tape, if it did not end early.) Check that `front_index_sha256` in seal.toml
   equals the sha256sum printed in step 3, and that its `file_count` is
   the number of entries in the front index. On a tape that ended early
   (File 0 and the seal say `requires = ["short-seal"]`, and `file_count`
   is smaller), the seal marker's `ended_after` is the last file before it,
   and every entry of the front index after that is not on the tape:
   "Where the tape ended". If the seal marker is
   missing, or the two hashes disagree, treat the tape as unsealed or
   damaged: see "If the verdict is UNSEALED or DAMAGED". An unsealed tape
   may lack its last data slices (the front index still lists them), and
   every file that is there can be checked against the index.
5. Find and open an envelope. Its entries in the front index have type
   `tenant_envelope` (one for each tenant, the first envelopes on the tape)
   or `operator_envelope` (and a `_backup` for each). Which one to open
   depends on your key. A tenant's key opens that tenant's
   `tenant_envelope` and no other, so the first that decrypts is yours. The
   operator key and the escrow key open EVERY envelope, and the tenant
   envelopes come first: the first that decrypts is then one tenant's, and
   its MANIFEST.toml lists that tenant's units and no one else's. For all
   the units on the tape open the `operator_envelope` instead, whose
   MANIFEST.toml lists every unit (or open each `tenant_envelope` and
   restore the units of each). For each envelope you try, with POSITION
   (the entry's `position`) and SIZE (its `size_bytes`):

       mt -f /dev/nst0 rewind
       mt -f /dev/nst0 fsf POSITION
       dd if=/dev/nst0 bs=512k of=envelope.raw
       truncate -s SIZE envelope.raw
       sha256sum envelope.raw
       age -d -i YOUR_KEY.age.key envelope.raw > envelope.tar

   The sha256sum must equal that entry's `sha256_encrypted`. age says
   "no identity matched any of the recipients" for an envelope your key
   was not made for: go on to the next. With a tenant's key the first that
   decrypts is yours; with the operator or escrow key you choose, as above.
   If it is damaged or will not read, use its backup, the `_backup` entry
   with the same `sha256_encrypted`. Unpack it:

       mkdir env && tar xf envelope.tar -C env
       ls env env/catalogs env/index

   (If neither the envelope nor its backup reads: "If All Else Fails", at
   its end.)

6. `env/MANIFEST.toml` describes the units of the envelope you opened: a
   `[[units]]` block for each (`name`, `uuid`, `snapshot_version`), and
   under it one `[[units.slices]]` block for each slice, with its `number`,
   `tape_position`, `encrypted_bytes` (its exact size) and
   `sha256_encrypted`. A unit stored in several versions has a `[[units]]`
   block, with its own slices, for each version. This line lists the units
   and versions:

       grep -E '^(name|uuid|snapshot_version) =' env/MANIFEST.toml

   `env/RECOVERY.md` writes the commands of step 7 out for each unit.
   `env/catalogs/` holds each unit's dar catalogue
   (`dar -l env/catalogs/NAME -N -Q` lists the files without the tape,
   NAME as under "Words this guide uses"), and `env/index/` the index of
   "The rest of the box".
7. Restore ONE version of ONE unit at a time, each in a directory of its
   own: the slices of every unit are named `restore.N.dar`, so two units in
   one directory would overwrite each other. If the manifest lists the unit
   more than once, take the block with the highest `snapshot_version`, the
   newest, unless you want an older one. On a tape that ended early (step
   4), check first that the `tape_position` of the unit's last slice is at
   or before the seal marker's `ended_after`: if it is after, the tape does
   not hold the unit whole, so do not start ("Where the tape ended"). A
   block that states `may_span = true` is a unit across tapes: a slice with
   no `tape_position` is on another tape, so read each tape's slices of it
   this way, matching each by its `sha256_encrypted`, all into the one
   directory, and run dar only once every slice, 1 to the last, is there ("A
   unit across tapes"). Put the tape at the first slice,
   once. `fsf N` moves forward N files from where the head is, so from the
   start of the tape it reaches File N: the `tape_position` of the first
   slice, from the manifest (FIRST_TAPE_POSITION below).

       mt -f /dev/nst0 rewind
       mt -f /dev/nst0 fsf FIRST_TAPE_POSITION

   Then, for each slice of the unit in order of its number, with
   ENCRYPTED_BYTES, SHA256_ENCRYPTED and NUMBER from the manifest (each read
   that finishes leaves the tape at the start of the next file, so the next
   slice needs no spacing unless there is a gap or a read failed):

       dd if=/dev/nst0 bs=512k of=restore.NUMBER.dar.age
       truncate -s ENCRYPTED_BYTES restore.NUMBER.dar.age
       echo "SHA256_ENCRYPTED  restore.NUMBER.dar.age" | sha256sum -c - && age -d -i YOUR_KEY.age.key restore.NUMBER.dar.age > restore.NUMBER.dar && rm restore.NUMBER.dar.age

   The last line is one command on purpose: each `&&` stops it at the first
   step that fails, so a slice whose hash is wrong is never decrypted, and
   its encrypted copy is deleted only after it decrypted. If it stops, the
   slice is damaged (below); if `age` was what failed, delete the partial
   `restore.NUMBER.dar` before going on.

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
   and go on: "If a slice or a file is damaged" ("No other copy") says what
   dar then does.

   Block padding must be trimmed (`truncate`) before the hash is checked
   and before age sees the file: the padding zeros make age reject the
   ciphertext. The decrypted files must be named `restore.1.dar`,
   `restore.2.dar`, ... with the slice's number in the manifest as the
   number, and no gaps (but for a slice you must leave out: "No other copy"
   in "If a slice or a file is damaged"): that is the naming dar requires of
   a sliced archive (base name `restore`). Work on a disk with room for all of the
   unit's decrypted slices at once (about the unit's size) and for step 8's
   restored files beside them (the unit's size again, more if it was stored
   compressed: "How much room, in one place", in "Disk space"), on a real disk and not in /tmp, which is a small RAM disk on many systems
   ("Disk space"). RESTORE.sh keeps only the envelopes there, never a slice.
8. Reassemble dar slices into an EMPTY directory, which dar needs to exist
   (`mkdir /destination`): `dar -x restore -R /destination -O -Q -N`. With
   several units, give each its own directory (`/destination/UNIT`); two
   units of one name (different uuids) take `/destination/UNIT-` and the
   first 8 characters of the uuid, each a new directory, or dar would merge
   them without a word. dar keeps any file already there and still reports
   success, so to finish an earlier restore of the same unit that stopped
   part way, add `-w` (overwrite); never use it to restore a newer version
   over an older one.
   `-N` keeps a `~/.darrc` on your machine from changing what is restored.
   Delete the unit's `restore.N.dar` files once it is restored and checked.

Then check the result as under "Did the restore get everything?": dar's
exit status (`echo $?`) is 0 and its summary shows nothing failed, and
the count matches `dar -l env/catalogs/NAME -N -Q`.

## Important: Block Padding

This tape uses 512KB (524288 byte) fixed block mode. Every file is
padded with zeros to the next block boundary. Encrypted files (data
slices, envelopes) MUST be trimmed to their exact byte size before
decryption — the padding zeros will cause age to reject the ciphertext.
Exact sizes are in File 3, the front index (`size_bytes` field).

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
checksums, ownership, or any file content — all of that lives only
inside the age-encrypted envelopes and slices. That includes the index
of other tapes and of every path, which is inside the envelopes.

**Accepted size disclosure.** Encryption overhead is deterministic and
tapectl archives without compression unless its operator chose it, so an
on-tape size approximates the content size it encloses. If your units are archived one
folder per slice — the common case — the size of each data slice
effectively discloses that unit's approximate content size and
reveals unit boundaries: someone holding this tape could in principle
correlate slice sizes against publicly known media sizes. This is a
known, accepted trade-off: sizes and ciphertext hashes are structural
facts needed for keyless navigation and integrity checking, and
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
envelope's decrypted bytes are a tar archive and a slice's are not. With a
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
   "no identity matched" means the file is not sealed to your key: try the
   next one. Any other error: real ciphertext can end in a zero byte, so
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
   envelope; an error that it is not a tar archive means a data slice.
   Once the envelope you want decrypts, unpack it (`mkdir env && tar xf
   candidate.dec -C env`): its MANIFEST.toml gives the exact tape
   position and byte size for every slice belonging to the units it lists.
   Apply the same read/trim/decrypt procedure to each slice using
   those exact sizes, then reassemble with dar as above.

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
/// `docs/design/volume-format-v2.md` §3-4 and ADR-0007.
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
/// `volume-format-v2.md` §2. The ciphertext hashes are safe in plaintext because
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
/// `volume-format-v2.md` §4). Its absence means the tape is legitimately
/// unsealed (interrupted or EOT-aborted).
///
/// From 1.6.0 it is §4.1's **full seal**: `requires = ["short-seal"]` (every
/// 1.6.0 seal, full or short, ADR-0012 item 111) and `ended_after =
/// file_count - 2`, the last data slice. `build` sizes the seal placeholder
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
/// describe (`volume-format-v2.md` §4).
pub fn generate_seal_marker(
    label: &str,
    file_count: i32,
    front_index_sha256: &str,
    files: &[FrontIndexFile],
) -> String {
    generate_seal_marker_requiring(label, file_count, front_index_sha256, files, REQUIRES)
}

/// The full seal stating `features` in `requires`, as File 0 of the same tape
/// states them (`volume-format-v2.md` §4.1: "From 1.7.0 the seal of a span
/// tape states `requires = ["short-seal", "spanning"]`"; ADR-0012 item 124).
/// `build` sizes the placeholder with it; the seal written at execute time
/// must state the same features, or it is not the placeholder's length.
pub fn generate_seal_marker_requiring(
    label: &str,
    file_count: i32,
    front_index_sha256: &str,
    files: &[FrontIndexFile],
    features: &[&str],
) -> String {
    let mut s = seal_document(
        label,
        file_count,
        i64::from(file_count) - 2,
        front_index_sha256,
        features,
    );
    append_files_entries(&mut s, files);
    s
}

/// Generate the plaintext **short seal marker** (`volume-format-v2.md` §4.1,
/// tapectl 1.6.0): the seal at K+1 of a tape the drive's early warning ended
/// after data slice K (§4.3 W4), `ended_after = K`, `file_count = K+2`.
///
/// `files_through_k` is File 3's entries 0 to K, with File 3's own entry
/// filled in as for a full seal (its `size_bytes` and `sha256_encrypted`);
/// this appends the seal's own bare entry at K+1 (`position` and `type`
/// only, self-reference). Entry K+1 of File 3 plans a data slice there; the
/// copy says what the tape holds.
///
/// The header is the full seal's, word for word, and the timestamp the same
/// fixed width: a short seal is never longer than the placeholder `build`
/// sized with [`generate_seal_marker`] (§4.1 "Length"), which
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

/// The seal marker's human header and `[seal]` table, the same for a full
/// and a short seal (`volume-format-v2.md` §4.1, "Grammar" and "The
/// human-readable header"), so the two kinds cannot drift apart in anything
/// but the numbers and the embedded copy.
fn seal_document(
    label: &str,
    file_count: i32,
    ended_after: i64,
    front_index_sha256: &str,
    features: &[&str],
) -> String {
    let requires = requires_value(features);
    // FIXED-WIDTH timestamp, deliberately: the build step sizes the seal
    // marker with a placeholder and the seal step regenerates it with the
    // real `sealed_at`, which is only sound if both render at identical
    // byte length (`v2-open-questions.md` §9). `to_rfc3339()` does NOT
    // guarantee that — it is `SecondsFormat::AutoSi`, which drops
    // trailing-zero fractional digits, so it emits 25/29/32/35-byte strings
    // depending on the nanosecond value (measured on this VM: ~99.89% at
    // 35 bytes, ~0.11% at 32, ~0.0002% at 29). Under that format a real
    // reseal would land on a different width roughly once per ~900 writes
    // and fail its own length-identity check. `SecondsFormat::Secs` with
    // `use_z = true` renders exactly 20 bytes ("2026-07-22T20:09:00Z"),
    // always — second precision is ample for an audit timestamp, and
    // fixed width is what the sizing trick actually requires.
    let now = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
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
    let now = chrono::Utc::now().to_rfc3339();
    let manifest = crate::volume::manifest::Manifest {
        volume: label.to_string(),
        tenant: tenant_name.to_string(),
        created_at: now,
        // Every envelope of every tape 1.6.0 writes (`volume-format-v2.md`
        // §1.7, ADR-0012 item 111): the tape may end early, and only its seal
        // says whether it did.
        may_end_early: true,
        units: units.to_vec(),
    };
    manifest.to_toml()
}

/// The stem of a unit's dar catalogue inside its envelope: the `catalogs/`
/// members are `<stem>.N.dar` (`staging` names them `<uuid8>_v<version>`).
fn catalogue_stem(unit: &ManifestUnit) -> String {
    format!(
        "{}_v{}",
        unit.uuid.get(..8).unwrap_or(&unit.uuid),
        unit.snapshot_version
    )
}

/// Does an envelope whose `catalogs/` members are `members` (bare file names,
/// as `build` puts them in the tar) hold the dar catalogue `stem`?
fn holds_catalogue(members: &[String], stem: &str) -> bool {
    let prefix = format!("{stem}.");
    members
        .iter()
        .any(|m| m.starts_with(&prefix) && m.ends_with(".dar"))
}

/// RECOVERY.md's section on the cross-volume index (`volume-format-v2.md`
/// sec 1.5, tapectl 1.3.0): what `index/units.tsv` and `index/paths.tsv`
/// hold, where each is, the staleness rule, and one `awk` line for each that
/// reads the text as it is stored (escaped). One section serves both kinds of
/// envelope, so `generate_recovery_md` needs no more arguments: it says which
/// envelope has which member. It must not name the dar catalogues
/// (`catalogs/`, `dar -l`): a unit's section does that only when the envelope
/// holds one (#449).
const RECOVERY_INDEX_SECTION: &str = r##"## The index of other tapes

This envelope also holds an index of the archive as it was known when
this tape was written, next to MANIFEST.toml:

- `index/units.tsv`: every version of the units it knows of, and the tapes
  that hold each. A tenant's envelope lists that tenant's units; the
  operator's envelope lists every tenant's, with a `tenant` column.
- `index/paths.tsv`: every path of those versions, with its unit and
  version. A tenant's envelope has it; the operator's does not.

An envelope from a tape written before tapectl 1.3.0 has no `index/`.

A unit that was renamed after this tape was written is listed here under
the name it had then, and a newer tape's index lists it under its newer
name. They are one unit when their `unit_uuid` is the same, so match units by
`unit_uuid`, and on each tape give RESTORE.sh the name that tape shows
(`--unit` takes a name, and an envelope knows a unit by the name it had when
its tape was written). Two units of one name but different `unit_uuid` are
different units: restore each into a directory of its own, such as
`/destination/UNIT-` and the first 8 characters of the uuid.

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

These two lines print the rows for a unit, and the paths that contain some
text. They compare the text as it is stored, so type a backslash, TAB or
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

/// RECOVERY.md's section on where the tape ended (`volume-format-v2.md` §1.7,
/// tapectl 1.6.0, ADR-0012 item 111), after the units table and before the
/// first unit's section: the table is the plan, the seal marker says where
/// the tape really ended, the four lines that read it with no key, and the
/// rule for a unit whose slices run past it. The same in every envelope.
const RECOVERY_END_SECTION: &str = r##"## Where this tape ended

This tape may end before the last tape file the write planned, because the
drive reached the end of its medium. The table above is the plan, written
before any slice was.

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

When the last tape file of a unit (the end of its "Tape files" above) is
after `ended_after`, this tape does not hold the unit whole. Do not start
reading its slices: it is restorable only from another tape. The index
(`index/units.tsv`, below) and `RESTORE.sh --where UNIT` name the tapes that
hold it. `RESTORE.sh` (tape file 2) does all of this by itself: `--info` and
`--verify` report where the tape ended.

"##;

/// RECOVERY.md's section on a damaged slice or file (ADR-0012 items 101 and
/// 102, tapectl 1.3.0): the two RESTORE.sh commands and the rule that only
/// the same unit at the same version is used. Like the index section it
/// serves every kind of envelope, and so it names no dar catalogue (#449).
/// Since the fresh read of File 1 for a span (#478; ADR-0012 items 135 and
/// 143): same slices only within one staging run, no `--slices-from` or
/// `--swap-tapes` for a unit this tape holds part of (with the routes that
/// do work, [`RECOVERY_DAMAGE_SPAN`]), and a fill only from a tape that holds
/// the version whole ([`RECOVERY_DAMAGE_REST`]). This const is the section's
/// head, to the replacement-slice commands.
const RECOVERY_DAMAGE_SECTION: &str = r##"## If a slice or a file is damaged

Another tape that holds this unit at the same version holds the same slices,
byte for byte, when both tapes were written from one staging run of it: its
front index (File 3) lists the same `sha256_encrypted`
for each, at whatever tape file and under whatever number that tape puts
it. Only the same unit at the same version is a copy; another version may
differ in the very files that are damaged. A tape written from another
staging run of the same version holds the same files but other ciphertext:
its slices have other `sha256_encrypted` values, so none of them is ever
taken for a slice of this tape (it is refused by hash), but a tape that
holds the version whole that way restores it alone, with its own
`RESTORE.sh`.

A slice that fails its `sha256sum -c` above is damaged: do not decrypt it.
Take it from the other tape instead, found by its `sha256_encrypted` and by
nothing else. `RESTORE.sh` (tape file 2) does that for you, from slice files
you copied off the other tape (`dd`, block padding and all, under any names)
or from the other cartridge itself, which it asks for after it has read this
one:

```bash
./RESTORE.sh --restore --key YOUR_KEY.age.key --unit UNIT --slices-from DIR --to /destination/UNIT
./RESTORE.sh --restore --key YOUR_KEY.age.key --unit UNIT --swap-tapes --to /destination/UNIT
```

"##;

/// The paragraph of RECOVERY.md's damage section on a unit held across tapes
/// (the fresh read of File 1, #478; `volume-format-v2.md` §4.4): RESTORE.sh
/// takes no `--slices-from` or `--swap-tapes` for a unit this tape holds part
/// of, and the routes that do work with no good copy of a slice. Only in an
/// envelope that lists a block stating `may_span = true`, as
/// [`recovery_span_section`] is.
const RECOVERY_DAMAGE_SPAN: &str = r##"Neither applies to a unit held across tapes that this tape holds only part
of (its block in MANIFEST.toml states `may_span = true` and gives some
slices no tape position): `RESTORE.sh` refuses `--slices-from` and
`--swap-tapes` for it. Its restore across tapes asks for the next tape at a
prompt, and asks there again for a slice that fails, from another cartridge
that holds it (with `--stream-tapes`, a slice that fails ends the run). With
no good copy of that slice anywhere, `RESTORE.sh` restores nothing of the
unit, and `done` at the prompt stops the restore. What does work then:
`--restore --unit UNIT --path PATH` for a folder or a file whose slices all
avoid the failed one (it reads only the slices that hold the path, and the
last slice, which must therefore be good); tapectl's own restore, which
reads a unit across tapes and reads around a damaged 64 KiB piece of a
slice (File 1, "tapectl itself"); and the steps above by hand, for each
tape's slices of the unit, all into one directory, with the failed slice
left out as below.

"##;

/// The rest of RECOVERY.md's damage section, after the replacement-slice
/// commands (and [`RECOVERY_DAMAGE_SPAN`] where it is): no other copy, the
/// damaged-file list and the fill, and the sha256 check of restored files.
const RECOVERY_DAMAGE_REST: &str = r##"With no other copy, leave the failed slice out of the steps above: do not
decrypt it, and do not put an empty file in its place. The other slices keep
their numbers (`restore.1.dar`, `restore.2.dar`, `restore.4.dar` ..., a gap
where the failed one was), and dar, run as above into an empty directory
(`dar -x restore -R /destination -O -Q -N > dar.log 2>&1`), restores every
file whose slices are all there. For each of the others it prints `PATH not
restored (user choice)` (`grep 'not restored (user choice)' dar.log` lists
them) and leaves an EMPTY or short file of that name, which is not the real
file; its exit status is still 0. If a folder's own entry lies in the
failed slice, dar names the folder and restores nothing in it ("No file in
this directory will be restored"). If the failed slice is the last one, dar
stops ("The last file of the set is not present"); add `-0` to restore, in
order, everything before the first missing slice.

If dar says it could not restore some files, `RESTORE.sh` restores the rest,
ends `RESTORE INCOMPLETE`, and writes their paths, one per line (a
backslash, TAB, newline and carriage return escaped as the index escapes
them), to `.tapectl-damaged-files.txt` in the unit's directory (with each
unit restored into `/destination/UNIT`, that is
`/destination/UNIT/.tapectl-damaged-files.txt`; a restore of one unit with
`--all` puts the files straight into `--to`, and the list is
`--to/.tapectl-damaged-files.txt`). Load a tape that holds the
same unit at the same version WHOLE (a tape that holds only some of its
slices is refused) and run that tape's own `RESTORE.sh`, with that
list and that directory: it restores exactly those files over their damaged
copies, leaves everything else in the directory alone, and refuses a tape
that holds another unit or another version:

```bash
./RESTORE.sh --restore --key YOUR_KEY.age.key --unit UNIT --fill-from /destination/UNIT/.tapectl-damaged-files.txt --to /destination/UNIT
```

For a folder dar restored nothing in, the list names the files the index
records under it, and the fill restores them like any other.

dar checks each file it restores with a short checksum, not a hash, so a
damaged file can come back with no error. A tape written since 1.3.0 records
each regular file's sha256 in `index/paths.tsv`, and `RESTORE.sh` checks every
file it restores, and every file it fills, against it: a file that does not
match, or is missing, goes on the same list and the run ends `RESTORE
INCOMPLETE`. It costs one read of the restored files from disk, and
`--no-file-check` skips it. By hand, for a unit and version, in the unpacked
envelope's directory (a name with a backslash, TAB, newline or carriage return
is written escaped there, and this line does not decode it):

```bash
V=1 U='photos/2019' awk -F'\t' 'NR == 1 { for (i = 1; i <= NF; i++) c[$i] = i; next } $c["unit"] == ENVIRON["U"] && $c["version"] == ENVIRON["V"] && $c["sha256"] != "" { print $c["sha256"] "  " $c["path"] }' index/paths.tsv | (cd /destination && sha256sum -c --quiet)
```

"##;

/// The tape positions a unit's MANIFEST block gives, in slice order: every
/// slice of a whole unit, and only the run this tape plans of a unit that may
/// span (ADR-0012 item 125).
/// Whether this tape's plan holds only some of the staging's slices: its
/// block gives some slices no `tape_position` (`volume-format-v3.md` §5.3).
/// Layout 3 has no `may_span` key (item 288): every staging may continue on
/// another cartridge, and the positions say what this tape holds. Until
/// T4.13 rewrites RECOVERY.md, its sections on a unit across tapes are kept
/// for such a block.
fn held_in_part(unit: &ManifestUnit) -> bool {
    unit.slices.iter().any(|sl| sl.tape_position.is_none())
}

fn slices_here(unit: &ManifestUnit) -> Vec<i64> {
    unit.slices
        .iter()
        .filter_map(|sl| sl.tape_position)
        .collect()
}

/// RECOVERY.md's section on a unit across tapes (`volume-format-v2.md` §4.4,
/// "File 1 and RECOVERY.md"; ADR-0012 item 135), after the section on where
/// the tape ended, in an envelope that lists a block stating `may_span =
/// true`: which slices of each such unit this tape's plan holds, that the
/// others are on other tapes, that a restore needs every slice in slice order
/// matched by `sha256_encrypted`, and that RESTORE.sh does it.
fn recovery_span_section(units: &[ManifestUnit]) -> String {
    let mut s = String::from(
        "## A unit across tapes\n\n\
         These units may run across tapes (their blocks in MANIFEST.toml state\n\
         `may_span = true`). Each block lists every slice of the unit, and this\n\
         tape's plan holds the slices given a tape position:\n\n\
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
         `ended_after` (\"Where this tape ended\", above), are on other tapes. A\n\
         restore needs every slice, 1 to the last, in slice order, from the tapes\n\
         that hold them: take each from a tape whose front index (File 3) lists the\n\
         slice's `sha256_encrypted` (MANIFEST.toml gives every slice's, and the unit's\n\
         section below repeats it), and by nothing else. `RESTORE.sh` (tape file 2)\n\
         does it: run it with `--restore --unit UNIT` on the tape that holds slice 1,\n\
         which `RESTORE.sh --where UNIT` on the newest tape names, and it asks for the\n\
         next. The unit's section below gives the steps for the slices this tape\n\
         holds.\n\n",
    );
    s
}

/// Generate RECOVERY.md for an envelope. `catalogue_members` are the bare
/// names of the `catalogs/` members the envelope actually holds: a unit's
/// section names its dar catalogue (`dar -l`, `-A`) only when the envelope
/// carries it (#449). A unit staged without one has no such paragraph.
pub fn generate_recovery_md(
    label: &str,
    tenant_name: &str,
    units: &[ManifestUnit],
    catalogue_members: &[String],
) -> String {
    let now = chrono::Utc::now().format("%Y-%m-%d").to_string();
    let mut s = format!(
        "# Recovery Guide for {tenant_name}\n\n\
         Volume: {label}\n\
         Date: {now}\n\n\
         This tape holds age-encrypted `dar` archives. With your age key(s) and the\n\
         standard tools (`mt`, `dd`, `truncate`, `age`, `dar`, `sha256sum`) you can\n\
         recover your data by hand — no tapectl required. The automated `RESTORE.sh`\n\
         (tape file 2) does exactly these steps for you; use it if you can.\n\n\
         ## Which key\n\n\
         The key that opened this envelope is\n\
         not necessarily the key that opens the slices below.\n\
         The envelope was sealed when the tape was written; each slice was sealed\n\
         earlier, when it was staged. If the keys were rotated in between, the\n\
         slices need an older key. `age` accepts several keys at once and uses\n\
         whichever one matches, so pass every key you hold:\n\n\
         ```bash\n\
         age -d -i OLD.age.key -i NEW.age.key ...\n\
         ```\n\n\
         Name only key files that exist:\n\
         one unreadable `-i` path fails the whole command.\n\
         If every key fails with `no identity matched any of the recipients`, the\n\
         slice was sealed to a key you have not supplied — the data is not corrupt.\n\
         `age-keygen -y YOUR_KEY.age.key` shows whether a file is a key at all (it\n\
         prints the key's public `age1...` line, or an error), and the guide on this\n\
         tape (File 1, \"Which file is your key\") tells a bad key file from a tape\n\
         of another installation.\n\n\
         ## Units in this envelope\n\n\
         | Unit | Snapshot | Slices | Tape files |\n\
         |------|----------|--------|------------|\n"
    );
    for unit in units {
        // A whole unit's every slice has a position. A unit that may span
        // lists every slice of its set, and only the slices this tape plans
        // have one (ADR-0012 item 125): a slice with none is on another tape,
        // never at tape file 0, so its row counts and spans only those.
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
        let stem = catalogue_stem(unit);
        let held = holds_catalogue(catalogue_members, &stem);
        let catalogue = format!("catalogs/{stem}");
        s.push_str(&format!(
            "## {}\n\n\
             UUID: `{}`  ·  snapshot v{}\n\n",
            unit.name, unit.uuid, unit.snapshot_version,
        ));
        if held {
            s.push_str(&format!(
                "This envelope also holds the unit's dar catalogue, `{cat}.1.dar`. It\n\
                 lists every file of the unit without reading the tape:\n\n\
                 ```bash\n\
                 dar -l {cat} -N -Q              # every file, with sizes and dates\n\
                 dar -l {cat} -N -Q -T slice     # which slice holds each file\n\
                 ```\n\n",
                cat = catalogue,
            ));
        }
        s.push_str(
            "Put the drive in fixed 512KB block mode, then read, trim, verify and\n\
             decrypt each slice, in order: the tape is wound to the first slice\n\
             once, and each read that finishes leaves it at the start of the next\n\
             file. A `dd` that stops with an error (\"Input/output error\") leaves\n\
             the head inside that file instead: `mt rewind`, then `mt fsf` to the\n\
             tape file of the slice you want next, which its `# Slice` line gives.\n\
             `/dev/nst0` below is an example — run `ls -l /dev/tape/by-id/` and\n\
             substitute your own drive, or you may read a different tape. Work in\n\
             a directory with room for the whole unit plus one slice: each\n\
             encrypted slice is removed once it is decrypted.\n\n\
             ```bash\n\
             mt -f /dev/nst0 setblk 524288\n\n\
             # On each `age -d` line, add `-i <file>` for every other key you hold\n\
             # (see \"Which key\" above).\n\n",
        );
        let mut next: Option<i64> = None;
        for slice in &unit.slices {
            // A slice with no position is on another tape (ADR-0012 item 125):
            // named, with what matches it there, and never read from this one.
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
            // The number in `restore.N.dar` MUST be dar's slice number, and the
            // slices MUST share the base name `restore` — this is dar's required
            // `base.N.dar` convention. `truncate` trims the 512KB block padding
            // that would otherwise make age reject the ciphertext. One rewind
            // per unit (#412): a read leaves the tape at the next file, so
            // only a gap between slices needs a forward space.
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
            s.push_str(&format!(
                "# Slice {n} — tape file {pos}, {eb} bytes\n\
                 {position}\
                 dd if=/dev/nst0 bs=512k of=restore.{n}.dar.age\n\
                 truncate -s {eb} restore.{n}.dar.age\n\
                 echo \"{sha}  restore.{n}.dar.age\" | sha256sum -c - && age -d -i YOUR_KEY.age.key restore.{n}.dar.age > restore.{n}.dar && rm restore.{n}.dar.age\n\n",
                n = slice.number,
                pos = tape_position,
                eb = slice.encrypted_bytes,
                sha = slice.sha256_encrypted,
            ));
        }
        if held_in_part(unit) {
            s.push_str(
                "# This unit may run across tapes: run dar only once every slice, 1 to the\n\
                 # last, is here as restore.N.dar. Read each one this tape does not hold the\n\
                 # same way from a tape whose front index lists its sha256_encrypted.\n\n",
            );
        }
        s.push_str(
            "# Reassemble and extract all slices (they share the base name `restore`),\n\
             # into an EMPTY directory that exists (dar will not create it):\n\
             mkdir /destination\n\
             dar -x restore -R /destination -O -Q -N\n\
             ```\n\n\
             `-O` ignores stored ownership, needed when restoring as a non-root user.\n\
             `-N` keeps a `~/.darrc` on your machine from changing what is restored.\n\
             dar keeps a file that already exists and still reports success, so\n\
             restore into an empty directory; to finish a restore that stopped part\n\
             way, run the same `dar -x` again with `-w` (overwrite) added.",
        );
        if held {
            s.push_str(&format!(
                " If dar\n\
                 cannot read the catalogue at the end of the last slice, `-A {cat}`\n\
                 gives it this envelope's dar catalogue instead.",
                cat = catalogue,
            ));
        }
        s.push_str("\n\n");
    }

    s.push_str(RECOVERY_INDEX_SECTION);
    s.push_str(RECOVERY_DAMAGE_SECTION);
    if units.iter().any(held_in_part) {
        s.push_str(RECOVERY_DAMAGE_SPAN);
    }
    s.push_str(RECOVERY_DAMAGE_REST);
    s.push_str(
        "## Troubleshooting\n\n\
         - **age: \"unexpected data\" / decryption fails** — the slice still has 512KB\n\
           block padding. Re-run `truncate -s <bytes>` to the exact size shown above.\n\
         - **dar: cannot open the archive** — the decrypted slices must be named\n\
           `restore.1.dar`, `restore.2.dar`, … with no gaps, and extracted with\n\
           `dar -x restore` (base name `restore`, no `.N.dar` suffix in the command).\n\
         - **sha256 mismatch** — re-read the slice from tape; a short read or the wrong\n\
           block mode (must be 512KB fixed) is the usual cause.\n\
         - **age: \"no identity matched any of the recipients\"** — none of the keys\n\
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

    // ---- what a 1.6.0 tape declares up front (volume-format-v2.md §1.7,
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
    /// (D1; item 288: no `spanning` feature, no `may_span` key).
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

    /// RECOVERY.md says where the tape ended, after the units table and
    /// before the first unit's section (§1.7): the table is the plan, the
    /// seal says where, the four lines that read it with the device caution,
    /// the rule for a unit past it, and what RESTORE.sh does.
    #[test]
    fn recovery_md_says_where_the_tape_ended_between_the_table_and_the_first_unit() {
        let units = vec![ManifestUnit {
            name: "alpha".into(),
            uuid: "11111111-aaaa".into(),
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
                tape_position: Some(9),
                size_bytes: 10,
                encrypted_bytes: 30,
                sha256_plain: "p".into(),
                sha256_encrypted: "e".into(),
            }],
        }];
        let r = generate_recovery_md("RE01", "alice", &units, &[]);
        let table = r.find("| alpha | v2 | 1 | 9..9 |").expect("the table");
        let section = r.find("## Where this tape ended").expect("the section");
        let first_unit = r.find("## alpha").expect("the unit's section");
        assert!(table < section && section < first_unit, "{r}");
        let sec = &r[section..first_unit];
        assert!(
            sec.contains(
                "```bash\n\
                 mt -f /dev/nst0 setblk 524288\n\
                 mt -f /dev/nst0 eod          # GNU mt: eom\n\
                 mt -f /dev/nst0 bsfm 2\n\
                 dd if=/dev/nst0 bs=512k | tr -d '\\0' | grep -E '^(file_count|ended_after) ='\n\
                 ```\n"
            ),
            "{sec}"
        );
        let f = flat_text(sec);
        for needle in [
            "This tape may end before the last tape file the write planned, because the drive reached the end of its medium. The table above is the plan, written before any slice was.",
            "The seal marker, the last file on the tape, says where it ended: its `ended_after` is the last tape file before the seal, and every tape file after `ended_after` is not on this tape.",
            "`/dev/nst0` below is an example — run `ls -l /dev/tape/by-id/` and substitute your own drive",
            "When the last tape file of a unit (the end of its \"Tape files\" above) is after `ended_after`, this tape does not hold the unit whole. Do not start reading its slices: it is restorable only from another tape.",
            "The index (`index/units.tsv`, below) and `RESTORE.sh --where UNIT` name the tapes that hold it.",
            "`--info` and `--verify` report where the tape ended.",
        ] {
            assert!(f.contains(needle), "{needle:?}: {f}");
        }
    }

    /// RECOVERY.md in an envelope that lists a unit that may span
    /// (`volume-format-v2.md` §4.4, "File 1 and RECOVERY.md"; ADR-0012 item
    /// 135): its row of the units table counts and spans only the slices this
    /// tape plans; a section after "Where this tape ended" says which slices
    /// of it this tape holds, that the others are on other tapes, that a
    /// restore needs every slice in slice order matched by `sha256_encrypted`,
    /// and that RESTORE.sh does it; and the unit's own steps name each slice on
    /// another tape and never read one at tape file 0. An envelope with no such
    /// block has no such section.
    #[test]
    fn recovery_md_names_a_range_and_never_reads_a_slice_at_file_0() {
        let slice = |number: i64, tape_position: Option<i64>| ManifestSlice {
            continues: false,
            number,
            tape_position,
            size_bytes: 10,
            encrypted_bytes: 30 + number,
            sha256_plain: "p".into(),
            sha256_encrypted: format!("e{number}").repeat(32),
        };
        let whole = ManifestUnit {
            name: "alpha".into(),
            uuid: "11111111-aaaa".into(),
            snapshot_version: 1,
            stage_set_id: 7,
            payload: "tar".to_string(),
            stream_bytes: 0,
            file_table: None,
            id_map: None,
            tar_version: None,
            compression: None,
            slices: vec![slice(1, Some(10)), slice(2, Some(11))],
        };
        let big = ManifestUnit {
            name: "big".into(),
            uuid: "88888888-9999".into(),
            snapshot_version: 2,
            stage_set_id: 8,
            slices: vec![
                slice(1, None),
                slice(2, None),
                slice(3, Some(12)),
                slice(4, Some(13)),
            ],
            ..whole.clone()
        };
        let r = generate_recovery_md("SPR1", "alice", &[whole.clone(), big.clone()], &[]);
        assert!(r.contains("| alpha | v1 | 2 | 10..11 |\n"), "{r}");
        assert!(r.contains("| big | v2 | 2 of 4 | 12..13 |\n"), "{r}");
        let end = r.find("## Where this tape ended").expect("the end section");
        let span = r.find("## A unit across tapes").expect("the span section");
        let first_unit = r.find("## alpha").expect("the first unit's section");
        assert!(end < span && span < first_unit, "{r}");
        let sec = &r[span..first_unit];
        assert!(
            sec.contains("| big | v2 | 3 to 4 | 12..13 | 4 |\n"),
            "{sec}"
        );
        assert!(!sec.contains("| alpha |"), "{sec}");
        let f = flat_text(sec);
        for needle in [
            "These units may run across tapes (their blocks in MANIFEST.toml state `may_span = true`). Each block lists every slice of the unit, and this tape's plan holds the slices given a tape position",
            "The slices with no tape position, and any after the seal marker's `ended_after` (\"Where this tape ended\", above), are on other tapes.",
            "A restore needs every slice, 1 to the last, in slice order, from the tapes that hold them: take each from a tape whose front index (File 3) lists the slice's `sha256_encrypted`",
            "`RESTORE.sh` (tape file 2) does it: run it with `--restore --unit UNIT` on the tape that holds slice 1",
            "The unit's section below gives the steps for the slices this tape holds.",
        ] {
            assert!(f.contains(needle), "{needle:?}: {f}");
        }

        let unit_sec = &r[r.find("## big").expect("big's section")..];
        let unit_sec = &unit_sec[..unit_sec.find("\n## ").unwrap_or(unit_sec.len())];
        for n in [1, 2] {
            assert!(
                unit_sec.contains(&format!(
                    "# Slice {n} — on another tape (no tape position here): {} bytes,\n# sha256_encrypted {}\n",
                    30 + n,
                    format!("e{n}").repeat(32)
                )),
                "{unit_sec}"
            );
            assert!(
                !unit_sec.contains(&format!("restore.{n}.dar.age\n")),
                "{unit_sec}"
            );
        }
        assert!(!unit_sec.contains("tape file 0"), "{unit_sec}");
        assert!(!unit_sec.contains("fsf 0"), "{unit_sec}");
        assert!(
            unit_sec.contains("# Slice 3 — tape file 12, 33 bytes\nmt -f /dev/nst0 rewind && mt -f /dev/nst0 fsf 12\n"),
            "{unit_sec}"
        );
        assert!(
            unit_sec.contains("# Slice 4 — tape file 13, 34 bytes\ndd if="),
            "{unit_sec}"
        );
        assert!(
            unit_sec.contains(
                "# This unit may run across tapes: run dar only once every slice, 1 to the\n"
            ),
            "{unit_sec}"
        );

        let plain = generate_recovery_md("SPR1", "alice", &[whole], &[]);
        assert!(!plain.contains("across tapes"), "{plain}");
    }

    /// File 1 says the same for the heir (§1.7): what `requires =
    /// ["short-seal"]` means (the glossary no longer says the list is empty),
    /// that the seal's `ended_after` says where the tape ended, the four
    /// lines, that a unit past it is not on this tape whole and where to look,
    /// and that RESTORE.sh prints NOT ON TAPE. The manual steps check it.
    #[test]
    fn file_1_says_where_the_tape_ended_and_how_to_read_the_seal() {
        let g = generate_system_guide_v2_for_media("SS02", 20, "LTO-6");
        let words = flat_text(guide_section(&g, "Words this guide uses"));
        assert!(
            words.contains(
                "A tape written by tapectl 1.6.0 or later states `requires = [\"short-seal\"]`"
            ),
            "{words}"
        );
        assert!(!words.contains("on the tapes written so far"), "{words}");
        let sec = guide_section(&g, "Where the tape ended");
        assert!(
            sec.contains(
                "    mt -f /dev/nst0 setblk 524288\n\
                 \x20   mt -f /dev/nst0 eod          # GNU mt: eom\n\
                 \x20   mt -f /dev/nst0 bsfm 2\n\
                 \x20   dd if=/dev/nst0 bs=512k | tr -d '\\0' | grep -E '^(file_count|ended_after) ='\n"
            ),
            "{sec}"
        );
        let f = flat_text(sec);
        for needle in [
            "only the seal marker, the last file on the tape, says where the tape really ended",
            "Its `ended_after` is the last tape file before the seal marker, and its `file_count` is `ended_after` plus 2",
            "Every tape file the front index lists after `ended_after` is not on this tape",
            "is not on this tape whole: do not start reading its slices",
            "`./RESTORE.sh --where UNIT --key YOUR_KEY.age.key`",
            "`--info` and `--verify` print one `NOT ON TAPE` line for the planned files after `ended_after`, naming the first and the last of them, which is not a failure",
            "NOT ON TAPE files 12 to 15 (planned; the tape ended after file 11, and its seal marker is file 12)",
            "`--restore` refuses a unit the end of the tape cut, by name",
        ] {
            assert!(f.contains(needle), "{needle:?}: {f}");
        }
        let manual = flat_text(guide_section(&g, "Manual Recovery Steps"));
        assert!(
            manual.contains("check first that the `tape_position` of the unit's last slice is at or before the seal marker's `ended_after`"),
            "{manual}"
        );
        assert!(
            manual.contains("the seal marker's `ended_after` is the last file before it, and every entry of the front index after that is not on the tape"),
            "{manual}"
        );
        // Every section the new text names is a heading of the guide.
        assert!(g.contains("\n## Which device?\n"), "{g}");
        assert!(g.contains("\n## The rest of the box"), "{g}");
    }

    /// ADR-0012 item 135 (`volume-format-v2.md` §4.4, "File 1 and
    /// RECOVERY.md"): File 1's glossary entry for `requires` names both
    /// features, the glossary says what a range is, and a section on a unit
    /// across tapes says what one tape shows, that a restore needs every slice
    /// 1 to N in slice order, that it starts from the tape holding slice 1,
    /// which `--where` on the newest tape names, that a tape is accepted by
    /// hash, that RESTORE.sh asks for the next cartridge, and what each of its
    /// two modes needs. On every tape 1.7.0 writes.
    #[test]
    fn file_1_says_what_a_unit_across_tapes_is_and_how_it_restores() {
        let g = generate_system_guide_v2_for_media("SP02", 20, "LTO-6");
        let words = flat_text(guide_section(&g, "Words this guide uses"));
        for needle in [
            "A tape written by tapectl 1.7.0 or later that holds a slice of a unit allowed to run across tapes states `requires = [\"short-seal\", \"spanning\"]`: some slices of that unit may be on other tapes (\"A unit across tapes\").",
            "a tape then holds a **range** of it, one unbroken run of its slices in order",
        ] {
            assert!(words.contains(needle), "{needle:?}: {words}");
        }
        let sec = flat_text(guide_section(&g, "A unit across tapes"));
        for needle in [
            "A tape that holds only some of them holds a RANGE: one unbroken run of the unit's slices, in slice order.",
            "states `requires = [\"short-seal\", \"spanning\"]` in File 0 and its seal marker",
            "A range is not damage, and it is not a copy of the unit on its own.",
            "lists EVERY slice of the unit, 1 to N, each with its `encrypted_bytes` and its `sha256_encrypted`, and gives a `tape_position` only to the slices this tape's plan holds",
            "A slice with no `tape_position` is on another tape: it is never at tape file 0.",
            "`--info` and `--verify` need no key and see the range as data slices like any other.",
            "`--list` says which slices of the unit this tape holds.",
            "What a restore needs: every slice, 1 to N, in slice order, from the tapes that hold them. dar never runs over one tape's slices alone.",
            "Start from the tape that holds slice 1: `./RESTORE.sh --where UNIT --key YOUR_KEY.age.key` on the NEWEST tape names the tapes and the slices each holds",
            "it names the slice it needs next and the tapes an index names for it, and waits for you to load another cartridge and press ENTER",
            "It takes a cartridge only when that tape's front index lists the next slice's `sha256_encrypted`, at a position its own seal marker covers",
            "so that disk needs room for the whole unit's encrypted slices (the sum of their `encrypted_bytes`) besides the restored files",
            "when the room is not there it refuses and names `--stream-tapes`",
            "`--stream-tapes` keeps no slice on disk: it feeds dar from each tape in turn, so it needs every cartridge at hand",
            // What L2 built (ADR-0012 item 132, the coordinator's prompt ruling).
            "Either way it waits at each change of cartridge for as long as you take, reading your answer from its input, and `quit` stops it.",
            "A slice that fails its hash or will not read is asked for again, by default, from another cartridge that holds it; with `--stream-tapes` it ends the run.",
            // The fresh read's m2 (#478): the exit trap removes it on Ctrl-C, a
            // hangup or kill; only a run with no chance to clean up leaves it.
            "Only a run that had no chance to clean up (`kill -9`, a crash, a power cut) leaves its scratch directory inside `--to` (or `--scratch DIR`), named `.tapectl-restore.` and some letters and holding encrypted slices only: remove it by hand",
            "A dar older than 2.7.9 (or than 2.7.21, when the envelope holds no dar catalogue of the unit) cannot read slices this way, and RESTORE.sh says so before it reads one.",
            "`--restore --unit UNIT --path PATH` reads across tapes too, and needs only the slices that hold the path and the last one.",
            "It can start on any tape of the unit that your key opens: it takes the slices that tape holds first, then asks for each further cartridge in slice order, taken by hash in the same way.",
            "They wait, still encrypted, in the scratch directory until all are read, and only then are they decrypted for dar.",
        ] {
            assert!(sec.contains(needle), "{needle:?}: {sec}");
        }
        let end = flat_text(guide_section(&g, "Where the tape ended"));
        assert!(
            end.contains(
                "with a slice at or before `ended_after` is not cut: this tape holds a range of it"
            ),
            "{end}"
        );
        let manual = flat_text(guide_section(&g, "Manual Recovery Steps"));
        assert!(
            manual.contains("A block that states `may_span = true` is a unit across tapes: a slice with no `tape_position` is on another tape"),
            "{manual}"
        );
        // The guide is the same on every tape 1.7.0 writes, a span tape or not.
        assert_eq!(g, generate_system_guide_v2_for_media("SP02", 20, "LTO-6"));
    }

    /// The fresh read of File 1 for a span (#478; ADR-0012 items 108 and
    /// 135), S2: the stock-taking rule counted two tapes that each hold part
    /// of a unit as two copies. A tape counts as a copy only when it holds the
    /// version whole; `--list --unit` and `--where` tell, and the table of
    /// `--find-envelope` marks a unit held in part from the plan alone.
    #[test]
    fn fresh_read_478_s2_two_halves_are_one_copy() {
        let g = generate_system_guide_v2_for_media("FR01", 20, "LTO-6");
        let bx = box_section(&g);
        for needle in [
            "A unit that appears at the same version AND with the same unit uuid on two tapes has two copies only when each of the two tapes holds that version WHOLE: then they are the same content, and either restores it alone.",
            "A tape that holds only part of a unit across tapes (\"A unit across tapes\") is not a copy: two tapes that each hold part of a unit are ONE copy, and restore it only together.",
            "run `./RESTORE.sh --list --key YOUR_KEY.age.key --unit UNIT` on the tape: it says \"HELD ACROSS TAPES\" when the tape holds only part of that version, and `--where UNIT` marks such a tape \"a range\".",
            "The table of `--find-envelope` marks such a unit `PART:` from the tape's plan alone; a tape that ended early (\"Where the tape ended\") can hold less than its plan, and `--list` and `--where` know it.",
        ] {
            assert!(bx.contains(needle), "{needle:?}: {bx}");
        }
    }

    /// The fresh read, S3: File 1 promised that with no other copy of a bad
    /// slice RESTORE.sh restores the files wholly inside the good ones
    /// (`--swap-tapes`, then `done`). For a unit across tapes it refuses
    /// `--swap-tapes` and restores nothing without every slice. Both places
    /// say so, and "No other copy" gives the routes that do exist; no partial
    /// restore across tapes is built.
    #[test]
    fn fresh_read_478_s3_no_partial_restore_across_tapes_and_the_routes_that_exist() {
        let g = generate_system_guide_v2_for_media("FR02", 20, "LTO-6");
        let verdict = flat_text(guide_section(&g, "If the verdict is UNSEALED or DAMAGED"));
        assert!(
            verdict.contains("The files in the failed slices are lost. Not so for a unit across tapes when no tape you hold has that version whole (\"A unit across tapes\"): RESTORE.sh refuses `--swap-tapes` for it, `done` at its prompt stops the restore, and it restores nothing of the unit without every slice. \"No other copy\" names the ways that do work for it."),
            "{verdict}"
        );
        let f = flat_text(guide_section(&g, "If a slice or a file is damaged"));
        for needle in [
            "Not for a unit across tapes when no tape you hold has that version whole (\"A unit across tapes\"): RESTORE.sh takes no `--swap-tapes` or `--slices-from` for it, and `done` at its prompt stops the restore with nothing of the unit restored. What does work for such a unit:",
            "- `--restore --unit UNIT --path PATH` restores a folder or a file whose slices all avoid the failed one. It reads only the slices that hold the path, and the last slice, which must therefore be good;",
            "- tapectl's own restore reads a unit across tapes, asking for each cartridge, and reads around a damaged 64 KiB piece of a slice, losing only the files in that piece (\"tapectl itself\").",
            "- By hand, as below: step 7 of \"Manual Recovery Steps\" reads each tape's slices of the unit into the one directory, matched by their `sha256_encrypted`, and the failed slice is left out.",
        ] {
            assert!(f.contains(needle), "{needle:?}: {f}");
        }
    }

    /// The fresh read, m1: at the prompt of a restore across tapes `done` is
    /// `quit` (swap_loop calls span_stop for both), and the exit trap removes
    /// the scratch with the slices read. File 1 taught `done` as "go on
    /// without"; the prompt, the help and File 1 now say that it stops.
    #[test]
    fn fresh_read_478_m1_done_at_the_cross_tape_prompt_stops_the_restore() {
        let g = generate_system_guide_v2_for_media("FR03", 20, "LTO-6");
        let sec = flat_text(guide_section(&g, "A unit across tapes"));
        assert!(
            sec.contains("and `quit` stops it. So does `done`: here it does not go on without the slice, as it does after `--swap-tapes` (\"If a slice or a file is damaged\"); either one ends the restore and discards the slices read so far (with `--stream-tapes`, the files dar has written are only part of the unit)."),
            "{sec}"
        );
    }

    /// The fresh read, m2: "a run that is killed leaves its scratch
    /// directory" was wrong for Ctrl-C and a closed terminal. The exit trap
    /// (cleanup) runs on SIGINT, SIGHUP and SIGTERM and removes SCRATCH with
    /// every slice read; only SIGKILL, a crash or a power cut leaves it. File
    /// 1, the help and the run's own note say which exits leave it, and
    /// advise screen or tmux.
    #[test]
    fn fresh_read_478_m2_which_exits_leave_the_scratch_and_screen_or_tmux() {
        let g = generate_system_guide_v2_for_media("FR05", 20, "LTO-6");
        let sec = flat_text(guide_section(&g, "A unit across tapes"));
        for needle in [
            "Ctrl-C, a closed terminal or a lost connection to the computer, and `kill` end the run too, and REMOVE its scratch directory with every slice read so far",
            "so run it inside `screen` or `tmux`, where a closed window or a lost connection does not end it.",
            "remove it by hand before you run the restore again, which starts from the beginning.",
        ] {
            assert!(sec.contains(needle), "{needle:?}: {sec}");
        }
        assert!(!sec.contains("A run that is killed leaves"), "{sec}");
    }

    /// The fresh read, m3: Disk space and the help listed the restores that
    /// keep slices on disk, but not the default restore across tapes, which
    /// keeps every slice of the unit, still encrypted, in scratch.
    #[test]
    fn fresh_read_478_m3_disk_space_counts_a_unit_across_tapes() {
        let g = generate_system_guide_v2_for_media("FR06", 20, "LTO-6");
        let disk = flat_text(guide_section(&g, "Disk space"));
        assert!(
            disk.contains("and asking for a replacement slice (`--slices-from`, `--swap-tapes`: \"If a slice or a file is damaged\"); and a unit across tapes, by default: all of its slices, still encrypted, about the unit's size again beside its restored files (the sum of its slices' `encrypted_bytes`; `--stream-tapes` keeps none on disk: \"A unit across tapes\")."),
            "{disk}"
        );
    }

    /// The fresh read, m7: Tools Required said an older dar restores by
    /// decrypting the slices to disk first, but a unit across tapes is refused
    /// with such a dar (restore_across_tapes): it says so there.
    #[test]
    fn fresh_read_478_m7_an_older_dar_cannot_restore_a_unit_across_tapes() {
        let g = generate_system_guide_v2_for_media("FR07", 20, "LTO-6");
        let tools = flat_text(guide_section(&g, "Tools Required"));
        assert!(
            tools.contains("A unit across tapes is the exception: it is fed to dar only through pipes (only its encrypted slices are ever kept on disk), so it needs dar 2.7.9 or later with the unit's dar catalogue, or 2.7.21 or later, and with an older dar RESTORE.sh refuses it before it reads a slice (\"A unit across tapes\")."),
            "{tools}"
        );
    }

    /// The fresh read, m6: "RESTORE.sh does not need the seal" is true of the
    /// tape a restore starts on only. Every later cartridge and every
    /// `--swap-tapes` source is refused without a readable seal (R5, R8;
    /// sealed_other), and the guide gives the manual route for such a tape.
    #[test]
    fn fresh_read_478_m6_a_later_cartridge_needs_its_seal() {
        let g = generate_system_guide_v2_for_media("FR08", 20, "LTO-6");
        let verdict = flat_text(guide_section(&g, "If the verdict is UNSEALED or DAMAGED"));
        for needle in [
            "RESTORE.sh does not need the seal, and checks every slice again as it restores. That holds for the tape in the drive when the restore starts.",
            "Every LATER cartridge, of a unit across tapes, and every cartridge `--swap-tapes` reads for a replacement slice, is refused without a readable seal marker: a slice is taken from another tape only at a position its own seal says that tape holds (\"A unit across tapes\").",
            "With such a tape as a later part of a unit across tapes, read its slices by hand (step 7 of \"Manual Recovery Steps\"), each checked against the `sha256_encrypted` the manifest gives, into one directory with the other tapes' slices of the unit, and run dar there once every slice is in it.",
            "For a slice of a whole unit, copy it off with `dd` and give it to `--slices-from DIR` (\"If a slice or a file is damaged\"), which matches by sha256 alone.",
        ] {
            assert!(verdict.contains(needle), "{needle:?}: {verdict}");
        }
    }

    /// The fresh read, m8: a fill takes files only from a tape that holds the
    /// version whole (ADR-0012 item 143; a range is never a fill source), and
    /// File 1, the script's note after a damaged restore and the help now say
    /// so where they send the heir to another tape.
    #[test]
    fn fresh_read_478_m8_a_fill_needs_a_tape_that_holds_the_version_whole() {
        let g = generate_system_guide_v2_for_media("FR09", 20, "LTO-6");
        let f = flat_text(guide_section(&g, "If a slice or a file is damaged"));
        assert!(
            f.contains("Load a tape that holds the same unit at the same version WHOLE (a tape that holds only part of a unit across tapes is refused: \"A unit across tapes\") and run ITS RESTORE.sh, with the list and `--to` of that same unit directory:"),
            "{f}"
        );
    }

    /// The fresh read, m4: "Run THAT tape's RESTORE.sh with `--restore --unit
    /// UNIT`" lacked `--key` and `--to`, which the script requires; and Start
    /// here never mentioned a unit across tapes.
    #[test]
    fn fresh_read_478_m4_the_full_command_and_a_line_in_start_here() {
        let g = generate_system_guide_v2_for_media("FR10", 20, "LTO-6");
        let sec = guide_section(&g, "A unit across tapes");
        assert!(
            sec.contains("\n    ./RESTORE.sh --restore --key YOUR_KEY.age.key --unit UNIT --to /destination/UNIT\n"),
            "{sec}"
        );
        let f = flat_text(sec);
        assert!(
            f.contains("Load the tape that holds slice 1 and run THAT tape's RESTORE.sh (saved as `RESTORE-<label>.sh`, as for any tape):"),
            "{f}"
        );
        assert!(!f.contains("with `--restore --unit UNIT`."), "{f}");
        let start = flat_text(guide_section(&g, "Start here"));
        assert!(
            start.contains("A unit across tapes (`--where` shows a tape holding a range of it) restores from a tape that holds it whole, or from the tape that holds its slice 1, which asks for the others: \"A unit across tapes\"."),
            "{start}"
        );
    }

    /// The fresh read, m5: nothing told the heir to write down the holders
    /// and their slices before a restore across tapes, though the first tape
    /// cannot name the rest and the prompt sends the heir to `--where` on the
    /// newest tape, a swap in the middle of the restore with one drive.
    #[test]
    fn fresh_read_478_m5_write_down_the_holders_before_you_start() {
        let g = generate_system_guide_v2_for_media("FR11", 20, "LTO-6");
        let f = flat_text(guide_section(&g, "A unit across tapes"));
        assert!(
            f.contains("Before you start, write down every tape it names and the slices each holds: the restore asks for the next slice by number, the tape then in the drive can name only tapes written before it, and to run `--where` on the newest tape again you would have to change cartridges in the middle of the restore."),
            "{f}"
        );
    }

    /// The fresh read, m9: nothing said to prefer a whole copy, and a whole
    /// copy from another staging run is refused by hash at a swap prompt
    /// (R8) though `--where` names it, yet restores alone.
    #[test]
    fn fresh_read_478_m9_prefer_a_whole_copy() {
        let g = generate_system_guide_v2_for_media("FR12", 20, "LTO-6");
        let f = flat_text(guide_section(&g, "A unit across tapes"));
        for needle in [
            "Prefer a whole copy: if any tape holds the version whole (`--where` prints no \"a range\" beside it), restore from that tape alone, as any unit, with no change of cartridge and no scratch.",
            "A whole copy written from another staging run of the same version has slices of other sha256 values: the prompt below refuses it by hash, though `--where` names it, and it still restores alone.",
            "If none does, read the version across tapes. Start from the tape that holds slice 1:",
        ] {
            assert!(f.contains(needle), "{needle:?}: {f}");
        }
    }

    /// The fresh read's findings, in RECOVERY.md (in every envelope) as in
    /// File 1 (#478, the coordinator's follow-up): its damage section said a
    /// copy holds the same slices byte for byte (only within one staging
    /// run), offered `--slices-from` and `--swap-tapes` with no exception for
    /// a unit held across tapes (refused by restore_across_tapes and
    /// die_range_path), and sent a fill to any tape of the same version (a
    /// range is never a fill source, item 143). The first and the last are
    /// in every envelope; the paragraph on a unit held across tapes only in
    /// one that lists a block stating `may_span = true`, as the section on a
    /// unit across tapes is. RESTORE.sh embeds none of it.
    #[test]
    fn fresh_read_478_recovery_md_damage_section_says_staging_run_span_and_whole() {
        let slice = |number: i64, tape_position: Option<i64>| ManifestSlice {
            continues: false,
            number,
            tape_position,
            size_bytes: 10,
            encrypted_bytes: 30 + number,
            sha256_plain: "p".into(),
            sha256_encrypted: format!("e{number}").repeat(32),
        };
        let whole = ManifestUnit {
            name: "alpha".into(),
            uuid: "11111111-aaaa".into(),
            snapshot_version: 1,
            stage_set_id: 7,
            payload: "tar".to_string(),
            stream_bytes: 0,
            file_table: None,
            id_map: None,
            tar_version: None,
            compression: None,
            slices: vec![slice(1, Some(10))],
        };
        let big = ManifestUnit {
            name: "big".into(),
            uuid: "88888888-9999".into(),
            stage_set_id: 8,
            slices: vec![slice(1, None), slice(2, Some(11))],
            ..whole.clone()
        };
        let plain = generate_recovery_md("RM01", "alice", std::slice::from_ref(&whole), &[]);
        let span = generate_recovery_md("RM01", "alice", &[whole, big], &[]);
        // In every envelope: 1, byte for byte only within one staging run;
        // 3, a fill only from a tape that holds the version whole.
        for r in [&plain, &span] {
            let sec = flat_text(guide_section(r, "If a slice or a file is damaged"));
            for needle in [
                "Another tape that holds this unit at the same version holds the same slices, byte for byte, when both tapes were written from one staging run of it: its front index (File 3) lists the same `sha256_encrypted` for each",
                "A tape written from another staging run of the same version holds the same files but other ciphertext: its slices have other `sha256_encrypted` values, so none of them is ever taken for a slice of this tape (it is refused by hash), but a tape that holds the version whole that way restores it alone, with its own `RESTORE.sh`.",
                "Load a tape that holds the same unit at the same version WHOLE (a tape that holds only some of its slices is refused) and run that tape's own `RESTORE.sh`",
            ] {
                assert!(sec.contains(needle), "{needle:?}: {sec}");
            }
            assert!(
                !sec.contains("holds the same slices, byte for byte: its front index"),
                "{sec}"
            );
        }
        // 2, in an envelope that lists a unit that may span: no --slices-from
        // or --swap-tapes across tapes, and the routes that work. An envelope
        // with no such block says nothing of tapes held in part, as before.
        let sec = flat_text(guide_section(&span, "If a slice or a file is damaged"));
        for needle in [
            "Neither applies to a unit held across tapes that this tape holds only part of (its block in MANIFEST.toml states `may_span = true` and gives some slices no tape position): `RESTORE.sh` refuses `--slices-from` and `--swap-tapes` for it.",
            "Its restore across tapes asks for the next tape at a prompt, and asks there again for a slice that fails, from another cartridge that holds it (with `--stream-tapes`, a slice that fails ends the run).",
            "With no good copy of that slice anywhere, `RESTORE.sh` restores nothing of the unit, and `done` at the prompt stops the restore.",
            "What does work then: `--restore --unit UNIT --path PATH` for a folder or a file whose slices all avoid the failed one (it reads only the slices that hold the path, and the last slice, which must therefore be good); tapectl's own restore, which reads a unit across tapes and reads around a damaged 64 KiB piece of a slice (File 1, \"tapectl itself\"); and the steps above by hand, for each tape's slices of the unit, all into one directory, with the failed slice left out as below.",
        ] {
            assert!(sec.contains(needle), "{needle:?}: {sec}");
        }
        // It sits between the replacement-slice commands and "With no other copy".
        let cmds = span.find("--swap-tapes --to /destination/UNIT\n```\n\nNeither applies");
        let rest =
            span.find("left out as below.\n\nWith no other copy, leave the failed slice out");
        assert!(cmds.is_some() && rest.is_some(), "{span}");
        assert!(!plain.contains("Neither applies"), "{plain}");
        assert!(
            plain.contains("--swap-tapes --to /destination/UNIT\n```\n\nWith no other copy, leave the failed slice out"),
            "{plain}"
        );
        // File 1 names the section the text points to.
        let g = generate_system_guide_v2_for_media("RM01", 20, "LTO-6");
        assert!(g.contains("\n## tapectl itself\n"));
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
                      (3, 'V3', 'lto', 'p', 1000, 'sealed'), (4, 'V4', 'lto', 'p', 1000, 'sealed');
             INSERT INTO writes (id, stage_set_id, snapshot_id, volume_id, status, may_span)
               VALUES (1, 1, 1, 1, 'range', 1), (2, 1, 1, 2, 'completed', 1),
                      (3, 1, 1, 3, 'range', 1), (4, 1, 1, 4, 'cut', 1);
             INSERT INTO write_positions (write_id, stage_slice_id, position, status)
               VALUES (1, 1, '10', 'written'), (1, 2, '11', 'written'),
                      (1, 3, '12', 'pending'), (1, 4, '13', 'pending'),
                      (2, 1, '8', 'written'), (2, 2, '9', 'written'),
                      (2, 3, '10', 'written'), (2, 4, '11', 'written'),
                      (3, 3, '4', 'written'), (3, 4, '5', 'written'),
                      (4, 1, '20', 'pending');",
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
    /// `volume-format-v2.md` §4.1's rules: Files 0-2, File 3 (filled in, as
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

    /// `volume-format-v2.md` §4.1 "Grammar": one `key = value` per line, in
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

    /// §4.1 "The human-readable header": the same on both kinds, so it does
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
    /// rules S1-S5, and classify against the plan as §4.1 says: the full seal
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

    /// §4.1 "Length": `build` sizes the seal with the full seal as a
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
        // A consistent 1.6.0 seal: it parses by §4.1's rules.
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

    #[test]
    fn tenant_recovery_md_has_working_dar_recipe() {
        // The generated manual recipe must match what the (gate-verified)
        // RESTORE.sh does: truncate to encrypted_bytes, restore.N.dar naming,
        // `dar -x restore`. The old recipe (H2) had none of these.
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
        let s = generate_recovery_md("LAB01", "alice", &units, &[]);
        // Correct commands present:
        assert!(s.contains("mt -f /dev/nst0 setblk 524288"));
        assert!(s.contains("dd if=/dev/nst0 bs=512k of=restore.1.dar.age"));
        assert!(s.contains("truncate -s 1049000 restore.1.dar.age"));
        assert!(s.contains("def456  restore.1.dar.age")); // sha256sum -c line
        assert!(s.contains("> restore.1.dar"));
        assert!(s.contains("dar -x restore -R /destination -O -Q"));
        // Broken forms from H2 must be gone:
        assert!(!s.contains("slice_1.dar"), "old slice_N naming leaked");
        assert!(!s.contains("ARCHIVE_BASE"), "placeholder leaked");
        assert!(!s.contains("bs=64k"));
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
        // `[media]` (`docs/design/volume-format-v2.md` §1.1).
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
        // The v1 mid-tape mini-index is gone in v2 (volume-format-v2.md §8);
        // its facts moved into the front index (File 3). Neither v2 generator
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

    /// File 1's manual dar step and RESTORE.sh's last instruction say to
    /// extract into an EMPTY directory, and how to finish a stopped restore
    /// (#405): dar under -Q keeps an existing file and reports success.
    #[test]
    fn the_manual_dar_step_says_empty_directory_and_w() {
        let guide = generate_system_guide_v2("DAR01", 20);
        let step8 = guide
            .split_once("8. Reassemble dar slices")
            .map(|(_, r)| r.split_once("\n\n").map_or(r, |(a, _)| a))
            .expect("manual step 8");
        assert!(step8.contains("EMPTY directory"), "{step8}");
        assert!(step8.contains("`-w`"), "{step8}");
    }

    /// Issue #412 item 4: RECOVERY.md's by-hand recipe rewound before every
    /// slice and kept both the .age and the .dar of every slice (about three
    /// times the unit on disk). Now: one rewind per unit, consecutive reads
    /// (a forward space only over a gap), each .age removed once decrypted,
    /// and dar with -N into an empty directory, -w to finish a stopped one.
    /// And (#418) it says the envelope's catalogues/ exist and how to use them.
    #[test]
    fn recovery_md_reads_forward_and_keeps_one_slice_of_ciphertext() {
        let slice = |n: i64, pos: i64| ManifestSlice {
            continues: false,
            number: n,
            tape_position: Some(pos),
            size_bytes: 1000,
            encrypted_bytes: 1100 + n,
            sha256_plain: "p".into(),
            sha256_encrypted: format!("sha{n}"),
        };
        let units = vec![ManifestUnit {
            name: "alpha".into(),
            uuid: "abcdef12-0000-0000-0000-000000000000".into(),
            snapshot_version: 3,
            stage_set_id: 7,
            payload: "tar".to_string(),
            stream_bytes: 0,
            file_table: None,
            id_map: None,
            tar_version: None,
            compression: None,
            slices: vec![slice(1, 10), slice(2, 11), slice(3, 14)],
        }];
        let s = generate_recovery_md("LAB01", "alice", &units, &["abcdef12_v3.1.dar".to_string()]);
        assert_eq!(s.matches("mt -f /dev/nst0 rewind").count(), 1, "{s}");
        assert!(
            s.contains("mt -f /dev/nst0 rewind && mt -f /dev/nst0 fsf 10\n"),
            "{s}"
        );
        // 11 follows 10 directly; 14 is two files further on.
        assert_eq!(s.matches("mt -f /dev/nst0 fsf 2\n").count(), 1, "{s}");
        assert_eq!(s.matches("mt -f /dev/nst0 fsf").count(), 2, "{s}");
        for n in 1..=3 {
            assert!(
                s.contains(&format!(
                    "age -d -i YOUR_KEY.age.key restore.{n}.dar.age > restore.{n}.dar && rm restore.{n}.dar.age"
                )),
                "{s}"
            );
        }
        assert!(s.contains("dar -x restore -R /destination -O -Q -N"), "{s}");
        assert!(s.contains("-w"), "{s}");
        assert!(s.contains("EMPTY"), "{s}");
        assert!(s.contains("catalogs/abcdef12_v3.1.dar"), "{s}");
        assert!(s.contains("dar -l catalogs/abcdef12_v3 -N -Q"), "{s}");
        assert!(s.contains("-A catalogs/abcdef12_v3"), "{s}");
    }

    /// #449 comment 2 (ADR-0012 2026-10-16 item 71): RECOVERY.md said every
    /// unit's dar catalogue was in the envelope. It is there only when the
    /// unit was staged with one, so the `dar -l` paragraph and the `-A` hint
    /// belong to the units whose catalogue the envelope holds, and a unit
    /// without one gets neither (RECOVERY.md is not golden-pinned: this test
    /// is its only guard).
    #[test]
    fn recovery_md_names_a_catalogue_only_when_the_envelope_holds_it() {
        let unit = |name: &str, uuid: &str, ver: i64| ManifestUnit {
            name: name.into(),
            uuid: uuid.into(),
            snapshot_version: ver,
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
                tape_position: Some(9),
                size_bytes: 1,
                encrypted_bytes: 2,
                sha256_plain: "p".into(),
                sha256_encrypted: "e".into(),
            }],
        };
        let units = vec![
            unit("with", "aaaaaaaa-0000-0000-0000-000000000000", 2),
            unit("without", "bbbbbbbb-0000-0000-0000-000000000000", 1),
            // The same stem as `with`, another version: its own catalogue.
            unit("with", "aaaaaaaa-0000-0000-0000-000000000000", 3),
        ];
        let members = vec![
            "aaaaaaaa_v2.1.dar".to_string(),
            "aaaaaaaa_v2.2.dar".to_string(),
        ];
        let s = generate_recovery_md("LAB01", "alice", &units, &members);
        let sections: Vec<&str> = s.split("\n## ").collect();
        let section = |header: &str| -> &str {
            sections
                .iter()
                .find(|x| x.starts_with(header))
                .copied()
                .unwrap_or_else(|| panic!("no {header:?} section:\n{s}"))
        };
        // Sections in order: the unit table is under "Units in this envelope".
        let with_v2 = sections
            .iter()
            .find(|x| x.contains("snapshot v2"))
            .copied()
            .unwrap();
        let with_v3 = sections
            .iter()
            .find(|x| x.contains("snapshot v3"))
            .copied()
            .unwrap();
        let without = section("without");
        assert!(
            with_v2.contains("dar -l catalogs/aaaaaaaa_v2 -N -Q"),
            "{with_v2}"
        );
        assert!(
            with_v2.contains("`catalogs/aaaaaaaa_v2.1.dar`"),
            "{with_v2}"
        );
        assert!(with_v2.contains("-A catalogs/aaaaaaaa_v2"), "{with_v2}");
        for (what, sec) in [("without", without), ("the other version", with_v3)] {
            assert!(!sec.contains("dar -l"), "{what} has no dar -l:\n{sec}");
            assert!(
                !sec.contains("catalogs/"),
                "{what} names no catalogue:\n{sec}"
            );
            assert!(!sec.contains("-A "), "{what} has no -A hint:\n{sec}");
            assert!(!sec.contains("also holds the unit"), "{what}:\n{sec}");
            // The recipe itself is untouched.
            assert!(
                sec.contains("dar -x restore -R /destination -O -Q -N"),
                "{sec}"
            );
            assert!(sec.contains("mt -f /dev/nst0 setblk 524288"), "{sec}");
        }
        // And no catalogue at all: no dar -l anywhere in the document.
        let none = generate_recovery_md("LAB01", "alice", &units, &[]);
        assert!(!none.contains("dar -l"), "{none}");
        assert!(!none.contains("catalogs/"), "{none}");
        // A member that only shares a prefix is not the catalogue.
        let close = generate_recovery_md(
            "LAB01",
            "alice",
            &units,
            &["aaaaaaaa_v20.1.dar".to_string()],
        );
        assert!(!close.contains("dar -l"), "{close}");
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

    /// File 1's escrow-key restore lists first and restores with --all, and
    /// says a single tenant's key opens only that tenant's part.
    #[test]
    fn system_guide_v2_shows_the_escrow_restore_as_list_then_all() {
        let g = generate_system_guide_v2_for_media("ESC1", 20, "LTO-6");
        // One placeholder throughout (#477 item 4): the escrow block uses
        // `YOUR_KEY.age.key` too and says what it stands for. Its list comes
        // first and its restore after it (the earlier restore example, in the
        // list of modes, is not the one).
        let list = g
            .find("./RESTORE.sh --list --key YOUR_KEY.age.key\n")
            .expect("the escrow --list example");
        let all = g[list..]
            .find("./RESTORE.sh --restore --key YOUR_KEY.age.key --all --to /destination\n")
            .expect("the escrow --all example, after its --list");
        assert!(all > 0, "list first, then restore");
        assert!(
            !g.contains("escrow.age.key"),
            "no second key placeholder: {g}"
        );
        let flat = flat_text(&g);
        assert!(
            flat.contains("A single TENANT's key opens only that tenant's envelope"),
            "{flat}"
        );
        assert!(
            flat.contains("restores only that unit (it tells you what else the key opened)"),
            "{flat}"
        );
    }

    /// File 1's tool list, its opening sentence and RESTORE.sh's checks come
    /// from one table (#449 item 2): every tool the script may check for is
    /// in both, with the roles the item names (mt-st, gpg, age/rage format).
    /// #448's acceptance read (2026-10-16): a reader who had not seen the
    /// design docs took the headline "Full restore" example, which named no
    /// `--all`, as restoring everything. Every full-restore example names
    /// `--all`, the disk-space figure covers `--all`, and the guide never says
    /// the tape is uncompressed while also saying a compressed unit restores
    /// larger.
    #[test]
    fn the_guides_full_restore_examples_restore_everything_the_key_opens() {
        let g = generate_system_guide_v2_for_media("FULL1", 20, "LTO-6");
        assert!(
            g.contains("./RESTORE.sh --restore --key YOUR_KEY.age.key --all --to /destination\n"),
            "{g}"
        );
        for line in g.lines().filter(|l| l.contains("./RESTORE.sh --restore")) {
            assert!(
                line.contains("--all") || line.contains("--unit UNIT"),
                "a restore example names neither --all nor --unit: {line}"
            );
        }
        let flat = flat_text(&g);
        assert!(flat.contains("the sum of them with `--all`"), "{flat}");
        assert!(flat.contains("every unit on the tape"), "{flat}");
        assert!(!flat.contains("uses no compression"), "{flat}");
        assert!(
            flat.contains("`bash`, `awk` and the basic coreutils"),
            "{flat}"
        );
    }

    #[test]
    fn system_guide_v2_lists_the_tools_restore_sh_checks_for() {
        let g = generate_system_guide_v2_for_media("TOOLS2", 20, "LTO-6");
        let intro = g
            .split_once("All you need is:")
            .and_then(|(_, rest)| rest.split_once("\n\n"))
            .map(|(list, _)| flat_text(list))
            .expect("the guide states its tool list up front");
        let section = g
            .split_once("## Tools Required")
            .and_then(|(_, rest)| rest.split_once("\n## "))
            .map(|(body, _)| body)
            .expect("a Tools Required section");
        for tool in restore_sh_tools() {
            assert!(
                intro.contains(tool),
                "the opening sentence lacks {tool}: {intro}"
            );
            assert!(
                section
                    .lines()
                    .any(|l| l.starts_with(&format!("- `{tool}`"))),
                "Tools Required lacks a bullet for {tool}:\n{section}"
            );
        }
        assert!(
            intro.contains("mt,") && intro.ends_with("and tee."),
            "{intro}"
        );
        let section = flat_text(section);
        assert!(section.contains("`mt` (mt-st package)"), "{section}");
        assert!(section.contains("cannot set it"), "GNU cpio mt: {section}");
        assert!(section.contains("`gpg` (package gnupg)"), "{section}");
        assert!(section.contains("age v1 format"), "{section}");
        assert!(
            section.contains("`age` program and `rage` both read"),
            "{section}"
        );
        assert!(
            section.contains("RESTORE.sh runs a program named `age`"),
            "{section}"
        );
        assert!(
            section
                .contains("`--info` and `--verify` need only `mt`, `dd`, `sha256sum` and `head`"),
            "{section}"
        );
    }

    /// File 1's disk-space statement matches RESTORE.sh since 1.1.0: about
    /// one unit's size, the script streams; --scratch is for --path and the
    /// old-dar fallback.
    #[test]
    fn system_guide_v2_states_one_units_disk_space() {
        let g = flat_text(&generate_system_guide_v2_for_media("DISK1", 20, "LTO-6"));
        assert!(g.contains("## Disk space"), "{g}");
        assert!(g.contains("one unit's size for one unit"), "{g}");
        assert!(
            g.contains("streams each slice from the tape through"),
            "{g}"
        );
        assert!(
            g.contains("`--scratch DIR`") && g.contains("`--path`"),
            "{g}"
        );
        assert!(g.contains("older-dar fallback"), "{g}");
        assert!(g.contains("compressed restores larger"), "{g}");
        assert!(g.contains("`$TMPDIR`"), "the work directory: {g}");
        // The pre-1.1.0 claim, from the cover sheet's first version, is not here.
        assert!(!g.contains("about twice the unit's size free"), "{g}");
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

    /// #477 item 2: `tr -d '\0'` is for plain text. It is used only to fetch
    /// the plaintext files, and wherever the guide uses it, it says it must
    /// never touch ciphertext.
    #[test]
    fn file_1_says_tr_is_for_plain_text_only_where_it_is_used() {
        let g = generate_system_guide_v2_for_media("TR1", 12, "LTO-6");
        let plain = [
            "RESTORE-TR1.sh",
            "file0.txt",
            "front_index.toml",
            "seal.toml",
        ];
        let uses: Vec<&str> = g
            .lines()
            .filter(|l| l.contains("| tr -d '\\0' >"))
            .collect();
        assert_eq!(
            uses.len(),
            4,
            "RESTORE.sh, File 0, File 3, seal marker: {uses:#?}"
        );
        for line in uses {
            assert!(
                plain.iter().any(|name| line.trim_end().ends_with(name)),
                "tr -d on something that is not a plaintext file: {line}"
            );
        }
        let auto = flat_text(guide_section(&g, "Automated Recovery (recommended)"));
        assert!(
            auto.contains("It is NEVER safe for an encrypted file, an envelope or a slice"),
            "{auto}"
        );
        let manual = flat_text(guide_section(&g, "Manual Recovery Steps"));
        assert!(
            manual.contains("An envelope or a slice is trimmed to its exact size with `truncate` instead, never with `tr`"),
            "{manual}"
        );
        let ladder = flat_text(guide_section(&g, "If All Else Fails"));
        assert!(
            ladder.contains("Never do it with `tr -d '\\0'`, which is for plain text only"),
            "{ladder}"
        );
    }

    /// #477 item 3: whether `setblk` comes first, and what dd's errors mean.
    #[test]
    fn file_1_says_when_setblk_is_needed_and_what_a_dd_error_means() {
        let g = generate_system_guide_v2_for_media("BLK1", 12, "LTO-6");
        let auto = guide_section(&g, "Automated Recovery (recommended)");
        let setblk = auto.find("mt -f /dev/nst0 setblk 524288").expect("setblk");
        let fetch = auto
            .find("> RESTORE-BLK1.sh")
            .expect("the fetch of RESTORE.sh");
        assert!(setblk < fetch, "setblk is the first line of the fetch");
        let f = flat_text(auto);
        assert!(
            f.contains("it is not needed if you have just run File 0's commands"),
            "{f}"
        );
        assert!(f.contains("Only mt-st's `mt` can do it"), "{f}");
        // Consistent with RESTORE.sh's own note on a drive in its default mode.
        assert!(
            f.contains("`dd bs=512k` reads each 512 KiB block whole"),
            "{f}"
        );
        assert!(
            f.contains(
                "\"Cannot allocate memory\": the tape's block is larger than the read you asked for"
            ),
            "{f}"
        );
        assert!(f.contains("(dd's default is 512 bytes)"), "{f}");
        assert!(
            f.contains("\"Input/output error\": the drive could not read"),
            "{f}"
        );
        for cause in ["another fixed block size", "a dirty head", "a damaged tape"] {
            assert!(f.contains(cause), "{cause}: {f}");
        }
    }

    /// #477 item 4: each term defined once; one key placeholder everywhere;
    /// what NAME is in `catalogs/NAME`.
    #[test]
    fn file_1_defines_its_words_and_uses_one_key_placeholder() {
        let g = generate_system_guide_v2_for_media("WORDS1", 12, "LTO-6");
        let words = flat_text(guide_section(&g, "Words this guide uses"));
        for term in [
            "**Tape file, \"File N\"**",
            "**Unit**",
            "**Tenant**",
            "**Operator**",
            "**escrow key**",
            "**Envelope**",
            "**Slice**",
            "**dar catalogue**",
        ] {
            assert!(words.contains(term), "{term} is not defined: {words}");
        }
        // "your envelope" means something to an escrow holder.
        assert!(
            words.contains(
                "\"Your envelope\" is an envelope your key opens: a tenant's key opens that \
                 tenant's envelope only; the operator and escrow keys open all of them"
            ),
            "{words}"
        );
        // NAME in catalogs/NAME, with the worked example.
        assert!(
            words.contains(
                "Its NAME in `catalogs/NAME` is the first eight characters of the unit's uuid, \
                 then `_v` and the version number"
            ),
            "{words}"
        );
        assert!(words.contains("`catalogs/11111111_v2.1.dar`"), "{words}");
        // One placeholder for the key file, defined once.
        for stray in [
            "your-key",
            "escrow.age.key",
            "KEYFILE",
            "YOUR_KEY.age.key.age",
        ] {
            assert!(!g.contains(stray), "a second key placeholder {stray:?}");
        }
        assert!(
            words.contains("**`YOUR_KEY.age.key`**: the one name this guide uses"),
            "{words}"
        );
        // Every --key and -i in the guide names that placeholder.
        for line in g.lines() {
            for flag in ["--key ", "age -d -i "] {
                if let Some(at) = line.find(flag) {
                    let value = line[at + flag.len()..]
                        .split_whitespace()
                        .next()
                        .unwrap_or("")
                        .trim_end_matches(['`', ')', ',', '.']);
                    assert!(
                        value == "YOUR_KEY.age.key" || value.is_empty(),
                        "{flag}{value} in: {line}"
                    );
                }
            }
        }
    }

    /// #477 item 5: the manual path has a command at every step, and says the
    /// decrypted slices are `restore.N.dar`.
    #[test]
    fn file_1_manual_path_has_a_command_for_every_step() {
        let g = generate_system_guide_v2_for_media("MAN1", 12, "LTO-6");
        let manual = guide_section(&g, "Manual Recovery Steps");
        for cmd in [
            "mt -f /dev/nst0 setblk 524288",
            "dd if=/dev/nst0 bs=512k | tr -d '\\0' > file0.txt",
            "mt -f /dev/nst0 fsf 2",
            "dd if=/dev/nst0 bs=512k | tr -d '\\0' > front_index.toml",
            "sha256sum front_index.toml",
            "dd if=/dev/nst0 bs=512k | tr -d '\\0' > seal.toml",
            "dd if=/dev/nst0 bs=512k of=envelope.raw",
            "truncate -s SIZE envelope.raw",
            "sha256sum envelope.raw",
            "age -d -i YOUR_KEY.age.key envelope.raw > envelope.tar",
            "mkdir env && tar xf envelope.tar -C env",
            "dd if=/dev/nst0 bs=512k of=restore.NUMBER.dar.age",
            "truncate -s ENCRYPTED_BYTES restore.NUMBER.dar.age",
            "echo \"SHA256_ENCRYPTED  restore.NUMBER.dar.age\" | sha256sum -c -",
            "age -d -i YOUR_KEY.age.key restore.NUMBER.dar.age > restore.NUMBER.dar",
            "dar -x restore -R /destination -O -Q -N",
        ] {
            assert!(
                manual.contains(cmd),
                "the manual path lacks {cmd:?}:\n{manual}"
            );
        }
        let f = flat_text(manual);
        assert!(
            f.contains("The decrypted files must be named `restore.1.dar`, `restore.2.dar`"),
            "{f}"
        );
        assert!(f.contains("(base name `restore`)"), "{f}");
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

    /// #477 item 7: what to do for UNSEALED and for DAMAGED, and how to find
    /// the slices that are incomplete.
    #[test]
    fn file_1_says_what_to_do_for_unsealed_and_damaged() {
        let g = generate_system_guide_v2_for_media("VERD1", 12, "LTO-6");
        let sec = guide_section(&g, "If the verdict is UNSEALED or DAMAGED");
        let f = flat_text(sec);
        for needle in [
            "UNSEALED: there is no valid seal marker. The write never finished",
            "DAMAGED (ends disagree): a seal marker is there, but it does not match the front index",
            "./RESTORE.sh --verify",
            "It prints PASS or FAIL for every tape file, by number",
            "To find which slices are incomplete, take the tape file numbers of a unit's slices from its MANIFEST.toml",
            "A unit whose slices all PASS is complete: restore it as usual",
            "A unit with a FAIL slice cannot be restored whole from this tape",
            "Look for another copy first",
        ] {
            assert!(f.contains(needle), "{needle:?}: {f}");
        }
    }

    /// #477 item 8: how to know the restore got everything.
    #[test]
    fn file_1_gives_a_completeness_check() {
        let g = generate_system_guide_v2_for_media("DONE1", 12, "LTO-6");
        let sec = guide_section(&g, "Did the restore get everything?");
        let f = flat_text(sec);
        assert!(
            f.contains("RESTORE.sh ended with `RESTORE COMPLETE` and exit status 0 (`echo $?`"),
            "{f}"
        );
        for line in [
            "inode(s) restored",
            "0 inode(s) not restored (not saved in archive)",
            "0 inode(s) not restored (overwriting policy decision)",
            "0 inode(s) failed to restore (filesystem error)",
        ] {
            assert!(f.contains(line), "dar's summary line {line:?}: {f}");
        }
        assert!(sec.contains("| grep -c -E '^\\[.{5}\\]\\[.\\] '"), "{sec}");
        assert!(f.contains("The count matches the dar catalogue"), "{f}");
        assert!(f.contains("`--list`"), "{f}");
    }

    /// #477 item 9: the rest of the box, and the index.
    #[test]
    fn file_1_names_the_index_and_how_to_take_stock_of_the_other_tapes() {
        let g = generate_system_guide_v2_for_media("BOX1", 12, "LTO-6");
        let sec = guide_section(&g, "The rest of the box: other tapes, copies and the index");
        let f = flat_text(sec);
        for needle in [
            "A tape may be a COPY of this one",
            "or hold OTHER units, or hold other VERSIONS of a unit this tape holds",
            "./RESTORE.sh --where UNIT --key YOUR_KEY.age.key",
            "./RESTORE.sh --find report --key YOUR_KEY.age.key",
            "An index is as of the moment its own tape was written",
            "It never names a tape written after it",
            "So the NEWEST tape's index is the most complete",
            "A tape written before 1.3.0 has no index and says so",
            "To take stock of a box without relying on the index, load each tape in turn",
            "A unit that appears at the same version AND with the same unit uuid on two tapes has two copies",
        ] {
            assert!(f.contains(needle), "{needle:?}: {f}");
        }
        // The two modes are among the examples of the automated path too.
        let auto = guide_section(&g, "Automated Recovery (recommended)");
        assert!(
            auto.contains("./RESTORE.sh --where UNIT --key YOUR_KEY.age.key"),
            "{auto}"
        );
        assert!(
            auto.contains("./RESTORE.sh --find some/file --key YOUR_KEY.age.key"),
            "{auto}"
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

    /// #477 item 11: where rung 3 stops looking for envelopes, and why up to
    /// four zero bytes.
    #[test]
    fn file_1_says_where_rung_3_stops_and_why_four_zero_bytes() {
        let g = generate_system_guide_v2_for_media("R3-1", 12, "LTO-6");
        let f = flat_text(guide_section(&g, "If All Else Fails"));
        for needle in [
            "which is 2 x (tenants + 1) files, at most about eight on the usual tape of one to three tenants",
            "The data slices follow them",
            "so size is a hint, not proof",
            "an envelope's decrypted bytes are a tar archive and a slice's are not",
            "With a tenant's key, stop at the first file that decrypts to a tar archive listing MANIFEST.toml",
            "Run `tar tf candidate.dec`",
            "re-append a single zero byte and retry, up to four times",
            "Step 2 cannot tell the padding from a zero that belongs to the ciphertext",
            "About one ciphertext in 256 ends in a zero byte, one in 65 536 in two",
            "a chance of about one in a trillion that more zeros were lost",
        ] {
            assert!(f.contains(needle), "{needle:?}: {f}");
        }
    }

    /// 1.3.0: RECOVERY.md names the index members, what they hold, the
    /// staleness rule, and gives one awk line for each that works on the
    /// text as stored. The lines are run here, on escaped rows.
    #[test]
    fn recovery_md_describes_the_index_and_its_awk_lines_work() {
        let s = generate_recovery_md("LAB01", "alice", &[], &[]);
        let sec = s
            .split("\n## ")
            .find(|x| x.starts_with("The index of other tapes"))
            .unwrap_or_else(|| panic!("no index section:\n{s}"));
        let f = flat_text(sec);
        for needle in [
            "`index/units.tsv`: every version of the units it knows of, and the tapes that hold each",
            "`index/paths.tsv`: every path of those versions",
            "A tenant's envelope has it; the operator's does not",
            "An envelope from a tape written before tapectl 1.3.0 has no `index/`",
            "An index is as of the moment its own tape was written. It never names a tape written after it",
            "so the NEWEST tape's index is the most complete",
            "a backslash is written `\\\\`, a TAB `\\t`, a newline `\\n` and a carriage return `\\r`",
            "Find a column by its name, and ignore a column you do not know",
        ] {
            assert!(f.contains(needle), "{needle:?}: {f}");
        }
        // It stays out of the way of the dar-catalogue rules (#449): no
        // catalogue is named when the envelope holds none.
        assert!(!s.contains("catalogs/") && !s.contains("dar -l"), "{s}");

        // Run both lines, on a units.tsv with its columns permuted plus one
        // unknown, and a paths.tsv whose path holds an escaped TAB.
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("index")).unwrap();
        std::fs::write(
            dir.path().join("index/units.tsv"),
            "volume\tunit\tzz_future\tversion\nVOL1\tphotos/2019\tx\t2\nVOL2\tdocs\tx\t1\nVOL3\tphotos/2019\tx\t1\n",
        )
        .unwrap();
        std::fs::write(
            dir.path().join("index/paths.tsv"),
            "unit\tversion\tkind\tsize_bytes\tpath\ndocs\t1\tf\t5\ta\\tb.txt\ndocs\t1\tf\t5\tc.txt\n",
        )
        .unwrap();
        let lines: Vec<&str> = sec
            .lines()
            .filter(|l| l.starts_with("U=") || l.starts_with("T="))
            .collect();
        assert_eq!(lines.len(), 2, "{sec}");
        let run = |line: &str, awk: &str| -> String {
            let o = std::process::Command::new("bash")
                .arg("-c")
                .arg(line.replacen("awk", awk, 1))
                .current_dir(dir.path())
                .output()
                .unwrap();
            assert!(o.status.success(), "{line}: {o:?}");
            String::from_utf8_lossy(&o.stdout).to_string()
        };
        for awk in ["awk", "mawk", "gawk"] {
            if std::process::Command::new(awk)
                .arg("BEGIN{}")
                .output()
                .is_err()
            {
                continue;
            }
            assert_eq!(
                run(lines[0], awk),
                "VOL1\tphotos/2019\tx\t2\nVOL3\tphotos/2019\tx\t1\n",
                "{awk}: the unit line"
            );
            // The path is compared as stored: the TAB is the two characters.
            assert_eq!(
                run(&lines[1].replace("T='report'", "T='a\\tb'"), awk),
                "docs\t1\tf\t5\ta\\tb.txt\n",
                "{awk}: the path line"
            );
        }
    }

    /// ADR-0012 item 103: RECOVERY.md says the file check exists, what it
    /// costs and how to skip it, names the `sha256` column, and its by-hand line
    /// does what it says: it passes files that match the index and names the
    /// ones that do not, under every awk. File 1 says the same of the script.
    #[test]
    fn the_guides_describe_the_file_check_and_the_by_hand_line_works() {
        let r = generate_recovery_md("LAB01", "alice", &[], &[]);
        let f = flat_text(&r);
        for needle in [
            "`sha256` (the file's content hash as 64 hex characters; empty for anything but a regular file, and where none was recorded)",
            "dar checks each file it restores with a short checksum, not a hash",
            "`RESTORE.sh` checks every file it restores, and every file it fills, against it",
            "It costs one read of the restored files from disk",
            "`--no-file-check` skips it",
        ] {
            assert!(f.contains(needle), "{needle:?}: {f}");
        }
        let g = flat_text(&generate_system_guide_v2_for_media("LAB01", 30, "LTO-6"));
        for needle in [
            "**A file fails its hash.**",
            "dar checks each file it restores with a short checksum, not a hash",
            "reads the restored files back from `--to` and checks them against it with `sha256sum`",
            "one read of the restored files from disk and none of the tape",
            "with `--path` only the paths restored are checked",
            "a tape written before 1.3.0 has no index and is not checked at all",
            "`--no-file-check` skips the check",
        ] {
            assert!(g.contains(needle), "{needle:?}: {g}");
        }

        let line = r
            .lines()
            .find(|l| l.starts_with("V=1 U="))
            .unwrap_or_else(|| panic!("no by-hand line:\n{r}"));
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path();
        std::fs::create_dir_all(d.join("index")).unwrap();
        std::fs::create_dir_all(d.join("dest/sub")).unwrap();
        let sha = |bytes: &[u8]| {
            use sha2::{Digest, Sha256};
            Sha256::digest(bytes)
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect::<String>()
        };
        std::fs::write(d.join("dest/a.bin"), b"alpha").unwrap();
        std::fs::write(d.join("dest/sub/b.bin"), b"CHANGED").unwrap();
        std::fs::write(
            d.join("index/paths.tsv"),
            format!(
                "unit\tversion\tkind\tsize_bytes\tsha256\tpath\n\
                 photos/2019\t1\td\t0\t\tsub\n\
                 photos/2019\t1\tf\t5\t{}\ta.bin\n\
                 photos/2019\t1\tf\t5\t{}\tsub/b.bin\n\
                 photos/2019\t1\tf\t5\t\tno-hash\n\
                 photos/2019\t2\tf\t5\t{}\ta.bin\n",
                sha(b"alpha"),
                sha(b"bravo"),
                sha(b"zzz"),
            ),
        )
        .unwrap();
        let line = line.replace("/destination", &d.join("dest").display().to_string());
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
                .current_dir(d)
                .output()
                .unwrap();
            let out = String::from_utf8_lossy(&o.stdout);
            assert!(
                out.contains("sub/b.bin: FAILED") && !out.contains("a.bin: FAILED"),
                "{awk}: only the changed file is named: {out}{}",
                String::from_utf8_lossy(&o.stderr)
            );
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
    #[test]
    fn file_1_fix_2_tmp_is_one_story() {
        let g = guide17();
        let disk = flat_text(guide_section(&g, "Disk space"));
        for needle in [
            "`/tmp` is a small RAM disk on many systems (`df -h /tmp` says)",
            "no data slice ever goes there",
            "set `TMPDIR` to a directory on a real disk",
            "TMPDIR=/big/disk/tmp ./RESTORE.sh",
        ] {
            assert!(disk.contains(needle), "{needle:?}: {disk}");
        }
        let manual = flat_text(guide_section(&g, "Manual Recovery Steps"));
        assert!(
            manual.contains("on a real disk and not in /tmp, which is a small RAM disk on many systems (\"Disk space\")"),
            "{manual}"
        );
        assert!(manual.contains("RESTORE.sh keeps only the envelopes there, never a slice"));
    }

    // 3. The damaged-tape contradictions: (a) the front index lists the
    // slices an unsealed tape lacks; (b) a disagreement of the ends is
    // reconciled with "restoring is sound"; (c) an escrow-key holder is told
    // how to get the slice table; (d) RESTORE.sh's own fallback is stated.
    #[test]
    fn file_1_fix_3_the_damaged_tape_sections_agree_with_themselves() {
        let g = guide17();
        let all = flat_text(&g);
        assert!(
            !all.contains("everything the front index describes is still readable"),
            "the contradicted sentence is gone"
        );
        let sec = flat_text(guide_section(&g, "If the verdict is UNSEALED or DAMAGED"));
        for needle in [
            "the front index lists EVERY file the write planned, slices that were never written included",
            "such a slice shows as FAIL (\"unreadable\") below",
            "neither can vouch for the other",
            "What you can check yourself is each file against the hash the front index lists for it",
            "it does not matter which for restoring, and restoring is sound",
            // (c)
            "The operator and escrow keys open several envelopes, and `--find-envelope` then prints only a table of units",
            "decrypt the operator envelope by hand (steps 5 and 6 of \"Manual Recovery Steps\")",
            "read `env/MANIFEST.toml`",
            // (d)
            "RESTORE.sh itself reads the front index from File 3 and, if that fails, from the seal marker's copy of it, with a warning",
        ] {
            assert!(sec.contains(needle), "{needle:?}: {sec}");
        }
        assert!(
            !sec.contains("do not trust either alone"),
            "the contradicted advice is gone"
        );
        let manual = flat_text(guide_section(&g, "Manual Recovery Steps"));
        assert!(
            manual.contains(
                "An unsealed tape may lack its last data slices (the front index still lists them)"
            ),
            "{manual}"
        );
        let ladder = flat_text(guide_section(&g, "If All Else Fails"));
        assert!(
            ladder.contains("RESTORE.sh climbs rungs 1 and 2 by itself"),
            "{ladder}"
        );
    }

    // 4. A restore with no --unit and no --all: one unit restores, several are
    // listed and refused, and "the largest envelope" is defined.
    #[test]
    fn file_1_fix_4_the_default_restore_and_the_largest_envelope_are_defined() {
        let g = guide17();
        let auto = flat_text(guide_section(&g, "Automated Recovery (recommended)"));
        for needle in [
            "the envelope that lists the most units among those your key opens",
            "with the escrow or operator key that is the operator envelope, which lists every unit on the tape",
            "with a tenant's key it is that tenant's envelope",
            "If that envelope lists SEVERAL units it will not guess: it prints them and stops before it reads a slice",
            "you name one with `--unit UNIT` (repeat `--unit` for several, each into /destination/UNIT) or restore them all with `--all`",
        ] {
            assert!(auto.contains(needle), "{needle:?}: {auto}");
        }
        assert!(
            !auto.contains("largest envelope"),
            "the undefined phrase is gone"
        );
    }

    // 5. Versions: the option is named with the modes that take it, and a
    // version is a full archive, so the newest alone is complete; `[InRef]`
    // is explained.
    #[test]
    fn file_1_fix_5_versions_are_selectable_and_complete() {
        let g = guide17();
        let words = flat_text(guide_section(&g, "Words this guide uses"));
        for needle in [
            "`--version N` (with `--restore --unit UNIT`, or with `--list`) takes version N instead",
            "`--find-envelope` shows which versions this tape holds",
            "Every version is a FULL archive of the unit as it was: the newest version alone holds every file, and an older one is never needed to complete it",
            "A dar catalogue's listing marks every entry `[InRef]`, dar's word for \"the data is in the archive this catalogue was cut from\"",
            "It does not mean the unit is a difference from another version",
        ] {
            assert!(words.contains(needle), "{needle:?}: {words}");
        }
    }

    // 6. The completeness check for --path and --all.
    #[test]
    fn file_1_fix_6_the_completeness_check_covers_path_and_all() {
        let g = guide17();
        let f = flat_text(guide_section(&g, "Did the restore get everything?"));
        for needle in [
            "`RESTORE INCOMPLETE` (exit status 1) means dar named files it could not restore",
            "With `--all`, or `--unit` given more than once, dar runs once for each unit and prints one summary for each",
            "With `--path`, dar restores only what you asked for, plus the directories above it (a file `a/b/c.txt` brings `a` and `a/b` with it, so 3 inodes)",
            "\"inode(s) ignored (excluded by filters)\" then counts every other inode of the unit, which is expected and not a failure",
        ] {
            assert!(f.contains(needle), "{needle:?}: {f}");
        }
    }

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
    #[test]
    fn file_1_fix_8_and_9_the_whole_box_and_the_states() {
        let g = guide17();
        let f = flat_text(guide_section(
            &g,
            "The rest of the box: other tapes, copies and the index",
        ));
        for needle in [
            "The whole box, in short:",
            "its label is \"Tape identifies as\" (File 0's label is the authority; a sticker on the case can have been replaced)",
            "Start from the newest tape.",
            "each unit into a directory of its own so that tapes never overwrite one another",
            "Use THAT tape's own RESTORE.sh, File 2 of the tape in the drive, which reads that tape's format",
            "You are done only when ALL of these hold; fewer is not done",
            "use another holder it lists",
            "`sealed` is a tape whose write finished and was sealed",
            "`quarantined` is a sealed tape whose medium a verify proved bad (it may still read in part, so prefer another holder)",
            "`this-tape` is the tape the index is on. A tape that was already retired or erased when the index was written is not listed, and one that is listed may have been retired or erased since.",
        ] {
            assert!(f.contains(needle), "{needle:?}: {f}");
        }
    }

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
    #[test]
    fn file_1_fix_12_disk_space_is_stated_up_front() {
        let g = guide17();
        let disk = flat_text(guide_section(&g, "Disk space"));
        assert!(
            disk.contains("`/destination` in this guide is a placeholder for an empty directory"),
            "{disk}"
        );
        assert!(
            disk.contains("this tape is LTO-6, which holds 2.5 TB of data as the makers count it (decimal, uncompressed)"),
            "{disk}"
        );
        let g8 = flat_text(&generate_system_guide_v2_for_media("L8", 30, "LTO-8"));
        assert!(
            g8.contains("this tape is LTO-8, which holds 12 TB of data"),
            "{g8}"
        );
        let unknown = flat_text(&generate_system_guide_v2_for_media("U", 30, "DLT-IV"));
        assert!(unknown.contains("LTO-6 2.5 TB, LTO-7 6 TB, LTO-8 12 TB, LTO-9 18 TB"));
        let tools = flat_text(guide_section(&g, "Tools Required"));
        for needle in [
            "`dar --version` says which dar you have",
            "\"without its dar catalogue\" means the envelope has none for that unit, or dar refuses the one it has",
        ] {
            assert!(tools.contains(needle), "{needle:?}: {tools}");
        }
    }

    // 13. The opening names the basic tools as well as the listed ones.
    #[test]
    fn file_1_fix_13_the_opening_names_the_basic_tools() {
        let g = guide17();
        let head = flat_text(&g[..g.find("## Words this guide uses").unwrap()]);
        assert!(
            head.contains("Besides those it uses `bash`, `awk` and the basic tools every Linux system has (`sed`, `grep`, `tr`, `cut`, `sort`, `stat`, `df`, `wc`, `tail`, `od`, `mktemp`), and `gpg` if your dar was built with GPG support"),
            "{head}"
        );
    }

    // ADR-0012 items 101 and 102: File 1 and RECOVERY.md each have a section
    // on a damaged slice or file, with the two commands and the rule that only
    // the same unit and version is used.
    #[test]
    fn file_1_and_recovery_md_say_what_to_do_for_a_damaged_slice_or_file() {
        let g = guide17();
        let f = flat_text(guide_section(&g, "If a slice or a file is damaged"));
        for needle in [
            "./RESTORE.sh --restore --key YOUR_KEY.age.key --unit UNIT \\ --slices-from DIR --to /destination",
            "./RESTORE.sh --restore --key YOUR_KEY.age.key --unit UNIT \\ --swap-tapes --to /destination",
            "./RESTORE.sh --restore --key YOUR_KEY.age.key --unit UNIT \\ --fill-from /destination/UNIT/.tapectl-damaged-files.txt \\ --to /destination/UNIT",
            "Matching is by that sha256 and nothing else, never by the number or position of a slice or by a file name",
            "`RESTORE INCOMPLETE` with exit status 1",
            "Only a tape whose envelope lists the same unit, with the same uuid, at the same version is used",
            "another version can differ in the very files that are damaged",
            "so `--to` needs room for the unit's slices beside its restored files",
            // R2 (#478 L5): a folder dar restored nothing in.
            "When the slice that holds a folder's own entry is lost, dar names the folder and restores nothing in it. On a tape written since 1.3.0 the list names the files the index records under that folder instead, and the fill restores them like any other. A tape written before 1.3.0 has no index, so the list names the folder itself, and the fill restores what is missing in it and overwrites nothing that is there.",
            // volume-format-v2.md 4.2 R5, 4.4 R8 (#478 L3).
            "`--swap-tapes` reads the other cartridge's seal marker first, and takes a slice from it only at a position the seal says the tape holds (\"Where the tape ended\"): a cartridge whose seal marker cannot be read gives none.",
            "The other cartridge may hold only part of a unit held across tapes (\"A unit across tapes\"); it gives the slices it holds.",
        ] {
            assert!(f.contains(needle), "{needle:?}: {f}");
        }
        let unit = ManifestUnit {
            name: "photos/2019".into(),
            uuid: "11111111-2222-3333-4444-555555555555".into(),
            snapshot_version: 2,
            stage_set_id: 1,
            payload: "tar".to_string(),
            stream_bytes: 0,
            file_table: None,
            id_map: None,
            tar_version: None,
            compression: None,
            slices: vec![],
        };
        for members in [vec![], vec!["11111111_v2.1.dar".to_string()]] {
            let r = generate_recovery_md("VOL", "alice", std::slice::from_ref(&unit), &members);
            let sec = flat_text(guide_section(&r, "If a slice or a file is damaged"));
            for needle in [
                "--slices-from DIR --to /destination",
                "--swap-tapes --to /destination",
                "--fill-from /destination/UNIT/.tapectl-damaged-files.txt --to /destination/UNIT",
                "found by its `sha256_encrypted` and by nothing else",
                "refuses a tape that holds another unit or another version",
                "Only the same unit at the same version is a copy",
                // R2 (#478 L5): a folder dar restored nothing in.
                "If a folder's own entry lies in the failed slice, dar names the folder and restores nothing in it (\"No file in this directory will be restored\").",
                "For a folder dar restored nothing in, the list names the files the index records under it, and the fill restores them like any other.",
            ] {
                assert!(sec.contains(needle), "{needle:?}: {sec}");
            }
            if members.is_empty() {
                assert!(
                    !r.contains("catalogs/") && !r.contains("dar -l"),
                    "a unit without a catalogue names none (#449)"
                );
            }
        }
    }
    // ---- the second fresh-reader read of File 1 (2026-10-19, ADR-0012 item 81, #477) ----
    //
    // Five serious gaps (S1-S5) and nine smaller ones (M6-M14). The commands the
    // fixes show are run in tests/heir_restore_sh.rs; here are the words.

    /// S2: a short "Start here" is the first section, and every section it
    /// names is a real heading of the guide.
    #[test]
    fn reread2_s2_start_here_comes_first_and_names_sections_that_exist() {
        let g = guide17();
        let start = g.find("## Start here").expect("a Start here section");
        assert!(start < g.find("## Words this guide uses").unwrap());
        let sec = flat_text(guide_section(&g, "Start here"));
        for needle in [
            "Check your key: \"Which file is your key\". A tenant's key opens that tenant's units only; the operator key and the escrow key open every tenant's",
            "Run `--info` on EVERY tape you hold (this tape's script is `./RESTORE-",
            "this guide writes `./RESTORE.sh` for any tape's",
            "They decrypt into `$TMPDIR` (default `/tmp`; if `/tmp` is small, set `TMPDIR` to a directory on a real disk first: \"Disk space\")",
            "check that it did (item 4 of \"Did the restore get everything?\")",
            "Start from the NEWEST tape",
            "`./RESTORE.sh --where UNIT --key YOUR_KEY.age.key` shows, for a unit, every version and every tape that holds it",
            "restore each unit's NEWEST version once, from a tape that holds it, into a directory of its own (`/destination/UNIT`), with that tape's own RESTORE.sh, saved as `RESTORE-<label>.sh`, and never an older version first",
            "Check each restore: \"Did the restore get everything?\"",
            "Know when you are done",
            "\"Manual Recovery Steps\" does the same job by hand",
            "\"If All Else Fails\"",
        ] {
            assert!(sec.contains(needle), "{needle:?}: {sec}");
        }
        // Every quoted section name is a heading (or a prefix of one).
        let headings: Vec<&str> = g.lines().filter_map(|l| l.strip_prefix("## ")).collect();
        let mut named = 0;
        for quoted in sec.split('"').skip(1).step_by(2) {
            if ["Sealed at", "Verdict:"].contains(&quoted) || quoted.starts_with("`") {
                continue;
            }
            // Only titles: a quoted string that is a heading's start.
            if headings.iter().any(|h| h.starts_with(quoted)) {
                named += 1;
            } else {
                // Quotes that are not section names.
                assert!(
                    ["Sealed at"].contains(&quoted) || quoted.len() > 60,
                    "{quoted:?} is not a heading of the guide: {headings:#?}"
                );
            }
        }
        assert!(named >= 8, "the sequence points to sections: {sec}");
        // Short: a page, not a chapter. 45 lines until the fourth read
        // (#481), which put three things there that a reader needs before
        // they begin (the package list, the disk chosen after the units are
        // listed, and where to look when nobody is left to ask); 52 until the
        // fifth (read4-fixes), which added what a reader needs to start
        // right: which RESTORE.sh a survey takes, the newest tape the drive
        // reads, a key checked without a cover sheet, and two units of one
        // name in different directories; 60 until the fifth read's fixes
        // (#481), which named the script step 3 saved, put `$TMPDIR` before
        // the first listing that fills it, and pointed `--all` at its check;
        // 64 until the fresh read of a span (#478 m4), which told step 6
        // where a unit across tapes restores from.
        assert!(guide_section(&g, "Start here").lines().count() <= 67);
    }

    /// S1: with the escrow or operator key the first envelope that opens is a
    /// tenant's; the manual path and rung 3 say to take the operator envelope.
    #[test]
    fn reread2_s1_the_escrow_key_takes_the_operator_envelope_not_the_first_that_opens() {
        let g = guide17();
        let manual = flat_text(guide_section(&g, "Manual Recovery Steps"));
        for needle in [
            "Which one to open depends on your key",
            "A tenant's key opens that tenant's `tenant_envelope` and no other, so the first that decrypts is yours",
            "The operator key and the escrow key open EVERY envelope, and the tenant envelopes come first: the first that decrypts is then one tenant's, and its MANIFEST.toml lists that tenant's units and no one else's",
            "For all the units on the tape open the `operator_envelope` instead, whose MANIFEST.toml lists every unit",
            "With a tenant's key the first that decrypts is yours; with the operator or escrow key you choose, as above",
            "`env/MANIFEST.toml` describes the units of the envelope you opened",
            "grep -E '^(name|uuid|snapshot_version) =' env/MANIFEST.toml",
        ] {
            assert!(manual.contains(needle), "{needle:?}: {manual}");
        }
        assert!(
            !manual.contains("The first that decrypts is yours."),
            "the unconditional claim is gone"
        );
        assert!(!manual.contains("describes your units"));
        let ladder = flat_text(guide_section(&g, "If All Else Fails"));
        for needle in [
            "With the operator or escrow key do not stop there: the tenants' envelopes come first and each lists only one tenant's units, so keep going to the operator envelope, the one whose tar also lists `PLAN.toml` (the operator envelope's plan of the tape: its units, each with its uuid, slice count and size; a tenant's envelope never has one), and whose MANIFEST.toml lists every unit on the tape",
            "Once the envelope you want decrypts, unpack it",
            "every slice belonging to the units it lists",
        ] {
            assert!(ladder.contains(needle), "{needle:?}: {ladder}");
        }
        assert!(
            !ladder.contains("belonging to your unit(s)"),
            "rung 3 no longer assumes your envelope is the whole tape"
        );
        // The slice-table hint of the verdict section already says the same.
        let verdict = flat_text(guide_section(&g, "If the verdict is UNSEALED or DAMAGED"));
        assert!(
            verdict.contains("decrypt the operator envelope by hand"),
            "{verdict}"
        );
    }

    /// S2: the per-tape checks say they are per tape, and the automated recipe
    /// warns that `--all` is for one tape and that a second one is refused.
    #[test]
    fn reread2_s2_the_checks_are_for_one_tape_and_the_box_is_complete_only_by_its_procedure() {
        let g = guide17();
        let done = flat_text(guide_section(&g, "Did the restore get everything?"));
        for needle in [
            "These checks are for ONE restore, of one unit or one tape's worth, from one tape",
            "They say nothing about the other tapes and units of a box",
            "a box is complete only when the procedure in \"The rest of the box\" has been followed to its end",
        ] {
            assert!(done.contains(needle), "{needle:?}: {done}");
        }
        let auto =
            flat_text(guide_section(&g, "Automated Recovery (recommended)")).replace(" # ", " ");
        for needle in [
            "Full restore of THIS tape to an EMPTY directory",
            "With other tapes in the box, read \"The rest of the box\" before this: a second --all into the same /destination is refused",
            "finishes a restore that stopped part way, never one over an older version",
            "(a single unit goes straight into /destination)",
        ] {
            assert!(auto.contains(needle), "{needle:?}: {auto}");
        }
        let bx = flat_text(guide_section(
            &g,
            "The rest of the box: other tapes, copies and the index",
        ));
        for needle in [
            "`--to` must be empty or new (a directory that is not empty is refused), which is why each unit has its own directory under /destination and not /destination itself",
            "Do not restore an older version first and then a newer one into the same directory: it is refused as not empty, and `--overwrite` would keep the files that only the older version had",
            "If a directory already holds an older version, move it aside and restore the newer version into an empty one",
        ] {
            assert!(bx.contains(needle), "{needle:?}: {bx}");
        }
    }

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

    /// S4: one rule for disk space, no contradiction between "no more than
    /// the capacity" and "about twice".
    #[test]
    fn reread2_s4_disk_space_is_one_rule() {
        let g = guide17();
        let disk = flat_text(guide_section(&g, "Disk space"));
        for needle in [
            "so everything stored on it takes at most that on the tape. A unit stored uncompressed restores to about that size; a unit stored compressed restores larger, and the capacity is no ceiling for it",
            "`--to` needs room for about the size of the units you restore",
            "`--find-envelope` lists each unit with its size, and so does `--where`",
            "need more room, for a unit's slices beside its restored files",
            "the older-dar fallback above (all of the unit's slices), `--path` (only the few slices that hold the path), and asking for a replacement slice",
            "Doing it by hand (\"Manual Recovery Steps\") is the same: the decrypted slices (the unit's size) beside the restored files (the unit's size, more if it was stored compressed); delete a unit's slices once it is restored",
            "For a whole box, the destination needs the sum of the sizes of the units you restore, whichever tapes they come from; the extra room for slices is needed for one unit at a time",
            "**Compressed units.**",
            "The dar catalogue cannot tell you",
            "\"Three times\" is a rule of thumb for mixed files, not a ceiling: text, source code, logs and database dumps compress ten times or more, so a compressed unit of mostly such files can need far more than three times its slices' size, and nothing on the tape says how much more",
        ] {
            assert!(disk.contains(needle), "{needle:?}: {disk}");
        }
        assert!(!disk.contains("need no more than that"), "{disk}");
        assert!(!disk.contains("As a ceiling"), "{disk}");
        let manual = flat_text(guide_section(&g, "Manual Recovery Steps"));
        assert!(
            manual.contains("unit's decrypted slices at once (about the unit's size) and for step 8's restored files beside them (the unit's size again, more if it was stored compressed"),
            "{manual}"
        );
        // The unknown-generation ceiling says the same.
        let unknown = flat_text(&generate_system_guide_v2_for_media("U", 30, "DLT-IV"));
        assert!(
            unknown.contains("so everything stored on it takes at most that on the tape. A unit stored uncompressed restores to about that size; a unit stored compressed restores larger"),
            "{unknown}"
        );
    }

    /// S5: a damaged only copy has commands, by RESTORE.sh and by hand, and
    /// says what tapectl is. The dar behaviour is run in heir_restore_sh.rs.
    #[test]
    fn reread2_s5_a_damaged_only_copy_has_commands_and_says_what_tapectl_is() {
        let g = guide17();
        let verdict = flat_text(guide_section(&g, "If the verdict is UNSEALED or DAMAGED"));
        assert!(
            verdict.contains("the files that lie wholly inside them can still be restored: RESTORE.sh does it (`--swap-tapes`, then type `done`), and \"No other copy\", in \"If a slice or a file is damaged\", has that and the commands by hand"),
            "{verdict}"
        );
        assert!(
            !verdict.contains("but nothing here promises it"),
            "{verdict}"
        );
        let sec = guide_section(&g, "If a slice or a file is damaged");
        let f = flat_text(sec);
        for needle in [
            "**No other copy.**",
            "RESTORE.sh does this for you: run the `--swap-tapes` restore above and type `done` when it asks for another cartridge",
            "lists the others in `.tapectl-damaged-files.txt`",
            "By hand, without RESTORE.sh, do the steps of \"Manual Recovery Steps\" for the unit, but leave the failed slices out",
            "this is the one time a gap in the numbering is right",
            "dar does not stop at the gap",
            "For each of the others it prints `PATH not restored (user choice)`",
            "If the failed slice is the LAST one, dar stops at once (\"The last file of the set is not present\", exit status 4) and restores nothing; add `-0` (sequential read)",
            "tapectl is the program that wrote this tape (File 0 names its release in `tapectl_version`); it is open-source software",
            "reads around a damaged 64 KiB piece of a slice",
            "the `age` program stops at the first damaged piece",
        ] {
            assert!(f.contains(needle), "{needle:?}: {f}");
        }
        assert!(
            sec.lines()
                .any(|l| l.trim() == "dar -x restore -R /destination -O -Q -N > dar.log 2>&1"),
            "{sec}"
        );
        assert!(
            sec.lines()
                .any(|l| l.trim() == "grep 'not restored (user choice)' dar.log"),
            "{sec}"
        );
        assert!(!f.contains("http"), "no URL: {f}");
        // The manual path points here at the slice that fails its hash, and
        // its "no gaps" rule has the one exception.
        let manual = flat_text(guide_section(&g, "Manual Recovery Steps"));
        assert!(
            manual.contains("With no other copy, leave that slice out and go on"),
            "{manual}"
        );
        assert!(
            manual.contains("and no gaps (but for a slice you must leave out"),
            "{manual}"
        );
        let done = flat_text(guide_section(&g, "Did the restore get everything?"));
        assert!(done.contains("\"Ignored (excluded by filters)\" above 0 in a whole-unit restore means dar skipped files"), "{done}");
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
            .filter(|l| l.starts_with("    LTO-"))
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

    /// M10, M11: the terms the guide uses are defined, "File 0's commands" is
    /// one of them, and the tools are listed for what they are needed for.
    #[test]
    fn reread2_m10_m11_the_glossary_and_the_tool_list() {
        let g = guide17();
        let words = flat_text(guide_section(&g, "Words this guide uses"));
        for needle in [
            "**ID thunk (File 0)**: the first tape file, plain text, that says which tape this is: its `label`, `uuid`, `media_type` (the LTO generation), `cartridge_serial` and `tapectl_version`",
            "\"File 0's commands\" in this guide means those",
            "**`requires`**: a line in File 0 and in the seal marker that lists the features a reader must understand to read the tape correctly. A tape written by tapectl 1.6.0 or later states `requires = [\"short-seal\"]`: the tape may end before the last file the front index plans, and its seal marker says where (\"Where the tape ended\"). Older tapes state it empty (`requires = []`, from 1.1.0 to 1.5.0) or have no such line",
            "A version is also called a snapshot: MANIFEST.toml states it as `snapshot_version`, RECOVERY.md as \"snapshot v2\" and RESTORE.sh as \"snapshot version\"",
            "so the highest number is the newest",
            "A unit is identified by its `uuid`",
            "**Installation**: one tapectl setup, with its own catalogue (its database of tapes, units and where the tapes are kept) and its own keys",
            "**Heir Kit, cover sheet**: the papers and files the operator prepares for the person who inherits the archive",
            "`catalog.db.age`, the Heir Kit's catalogue (the whole database), encrypted to the escrow key",
        ] {
            assert!(words.contains(needle), "{needle:?}: {words}");
        }
        let tools = flat_text(guide_section(&g, "Tools Required"));
        for needle in [
            "Only for finding the drive and checking a key, never for the restore itself: `ls`, `lsscsi` (package lsscsi), `lspci` (pciutils), `dmesg` (util-linux), `lsmod` and `modprobe` (kmod), `sudo` (sudo) and `usermod` (passwd)",
            "`age-keygen`, which comes with `age` (\"Which file is your key\")",
            "`wc`, `tail`, `od`",
        ] {
            assert!(tools.contains(needle), "{needle:?}: {tools}");
        }
        // Everything those lines name is something the guide says to run.
        let all = flat_text(&g);
        for tool in [
            "lsscsi",
            "lspci",
            "dmesg",
            "lsmod",
            "modprobe",
            "usermod",
            "age-keygen",
        ] {
            assert!(
                all.matches(tool).count() >= 2,
                "{tool} is named where it is used"
            );
        }
    }

    /// M12: the manual path takes one version of one unit into a directory of
    /// its own, the highest `snapshot_version`.
    #[test]
    fn reread2_m12_manual_steps_take_one_unit_one_version_a_directory() {
        let g = guide17();
        let manual = flat_text(guide_section(&g, "Manual Recovery Steps"));
        for needle in [
            "Restore ONE version of ONE unit at a time, each in a directory of its own: the slices of every unit are named `restore.N.dar`, so two units in one directory would overwrite each other",
            "If the manifest lists the unit more than once, take the block with the highest `snapshot_version`, the newest, unless you want an older one",
            "A unit stored in several versions has a `[[units]]` block, with its own slices, for each version",
            "With several units, give each its own directory (`/destination/UNIT`)",
            "add `-w` (overwrite); never use it to restore a newer version over an older one",
            "Delete the unit's `restore.N.dar` files once it is restored and checked",
        ] {
            assert!(manual.contains(needle), "{needle:?}: {manual}");
        }
    }

    /// M13: the retired-or-erased wording agrees with itself, an erased and
    /// reused cartridge is described, and two tapes are copies only with the
    /// same unit uuid.
    #[test]
    fn reread2_m13_states_erased_cartridges_and_the_uuid() {
        let g = guide17();
        let bx = flat_text(guide_section(
            &g,
            "The rest of the box: other tapes, copies and the index",
        ));
        for needle in [
            "A tape that was already retired or erased when the index was written is not listed, and one that is listed may have been retired or erased since",
            "A cartridge that was erased and written again carries a NEW volume: File 0 then shows another label and another `uuid` on the same `cartridge_serial`, and the index row for the old volume names data that is gone",
            "So match a tape to an index row by its label AND its uuid (`--where` prints the uuid; File 0 shows it)",
            "dd if=/dev/nst0 bs=512k | tr -d '\\0' | grep -E '^(label|uuid|media_type|cartridge_serial|created_at|tapectl_version) ='",
            "A unit that appears at the same version AND with the same unit uuid on two tapes has two copies",
            "A name and a version number alone prove nothing",
        ] {
            assert!(bx.contains(needle), "{needle:?}: {bx}");
        }
        assert!(
            !bx.contains("A retired or erased tape is not listed."),
            "{bx}"
        );
        // The File 0 line has `mt -f` and does not trip the tr-for-plain-text count.
        assert!(
            !g.contains("tr -d '\\0' | grep")
                || g.lines().filter(|l| l.contains("| tr -d '\\0' >")).count() == 4
        );
    }

    /// M6: which RESTORE.sh to use for the survey and for the restore, how to
    /// get another tape's, and whether the WRONG TAPE box matters.
    #[test]
    fn reread2_m6_which_restore_sh_for_which_job() {
        let g = guide17();
        let bx = flat_text(guide_section(
            &g,
            "The rest of the box: other tapes, copies and the index",
        ));
        for needle in [
            "(any copy of RESTORE.sh will do for this: see \"The modes that only read the tape\", after this list)",
            "The modes that only read the tape (`--info`, `--verify`, `--find-envelope`, `--list`, `--where`, `--find`) describe the tape that is in the drive, whichever tape's RESTORE.sh runs them (`--where` and `--find` need the script of a tape written by tapectl 1.3.0 or later; an older one answers `Unknown mode`)",
            "it is only a reminder that the script was written for another volume",
            "the `Tape identifies as:` line, which every mode prints, is that tape's own label; the heading `--verify` prints names the script's volume, but it checks the tape in the drive",
            "For a restore use the loaded tape's own RESTORE.sh, which was written for that tape's format",
            "take \"WRONG TAPE?\" during a restore as a sign that the script or the tape is the wrong one",
            "To get a tape's own script, load that tape and run the four commands at the start of \"Automated Recovery (recommended)\"",
        ] {
            assert!(bx.contains(needle), "{needle:?}: {bx}");
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

    /// RECOVERY.md repeats the staleness rule honestly and carries the
    /// no-other-copy paragraph, without naming a dar catalogue (#449).
    #[test]
    fn reread2_recovery_md_says_the_newest_tape_may_not_be_the_newest_ever() {
        let r = generate_recovery_md("LAB01", "alice", &[], &[]);
        let f = flat_text(&r);
        for needle in [
            "Nothing on a tape can name tapes written after it, so the newest tape you hold may not be the newest ever written",
            "With no other copy, leave the failed slice out of the steps above: do not decrypt it, and do not put an empty file in its place",
            "(`dar -x restore -R /destination -O -Q -N > dar.log 2>&1`)",
            "`PATH not restored (user choice)` (`grep 'not restored (user choice)' dar.log` lists them)",
            "leaves an EMPTY or short file of that name, which is not the real file; its exit status is still 0",
            "If the failed slice is the last one, dar stops (\"The last file of the set is not present\"); add `-0`",
        ] {
            assert!(f.contains(needle), "{needle:?}: {f}");
        }
        assert!(!r.contains("catalogs/") && !r.contains("dar -l"), "{r}");
    }

    // ---- the third fresh-reader read of File 1 (2026-10-19, ADR-0012 item 81, #477) ----
    //
    // Two serious gaps about restoring a whole box (the box could not be
    // enumerated; "done" could be reached without opening a tape) and five cheap
    // ones. The commands are run against the fake tape in tests/heir_restore_sh.rs;
    // here are the words.

    /// S1: every place that tells the reader how to list the box names
    /// `--where --all`, nothing tells them to ask for a name the index lacks,
    /// and the script's own `--help` has the mode.
    #[test]
    fn reread3_s1_the_box_is_listed_by_where_all_with_no_cap() {
        let g = guide17();
        let all = flat_text(&g);
        for gone in ["the first 40", "for a name it does not have"] {
            assert!(!all.contains(gone), "{gone:?} is gone from File 1");
        }
        let start = flat_text(guide_section(&g, "Start here"));
        for needle in [
            "`./RESTORE.sh --where --all --key YOUR_KEY.age.key` lists EVERY unit the archive's index names, with its newest version and the tapes that hold it",
            "`./RESTORE.sh --where UNIT --key YOUR_KEY.age.key` shows, for a unit, every version and every tape that holds it",
            "`--find-envelope` lists the units on this tape",
        ] {
            assert!(start.contains(needle), "{needle:?}: {start}");
        }
        let auto = guide_section(&g, "Automated Recovery (recommended)");
        assert!(
            auto.lines()
                .any(|l| l.trim() == "./RESTORE.sh --where --all --key YOUR_KEY.age.key"),
            "{auto}"
        );
        let raw = guide_section(&g, "The rest of the box: other tapes, copies and the index");
        assert!(
            raw.lines()
                .any(|l| l.trim() == "./RESTORE.sh --where --all --key YOUR_KEY.age.key"),
            "{raw}"
        );
        let bx = flat_text(raw);
        for needle in [
            "`./RESTORE.sh --where --all --key YOUR_KEY.age.key` prints all of them, with no limit: for each unit its newest version, whether the tape in the drive holds it, and every version with each tape that holds it (label, uuid, cartridge serial, generation, state); the units whose newest version is not on that tape are named first",
            "List the units the newest index you can read has and the tapes that hold them, and set that against the tapes in the box",
            "2. Start from the newest tape. `./RESTORE.sh --where --all --key YOUR_KEY.age.key` lists every unit its index names, each with its newest version and the tapes that hold it",
        ] {
            assert!(bx.contains(needle), "{needle:?}: {bx}");
        }
        // RECOVERY.md names it too, in the section that reads the index.
        let r = flat_text(&generate_recovery_md("LAB01", "alice", &[], &[]));
        assert!(
            r.contains("`--where --all` (every unit the index names, with no limit"),
            "{r}"
        );
    }

    /// S2: "done" needs every tape opened with a key, every unit the newest
    /// readable index names restored, an unopenable tape named and not counted,
    /// the no-index and unsealed cases, and the plain statement about what the
    /// newest version holds.
    #[test]
    fn reread3_s2_done_needs_every_tape_opened_and_every_listed_unit_restored() {
        let g = guide17();
        let all = flat_text(&g);
        assert!(
            !all.contains("has been looked at"),
            "the old rule (a tape merely looked at) is gone"
        );
        assert!(!all.contains("(or the best you could reach)"), "{all}");
        let bx = flat_text(guide_section(
            &g,
            "The rest of the box: other tapes, copies and the index",
        ));
        for needle in [
            "6. You are done only when ALL of these hold; fewer is not done:",
            "Every tape in the box has had its envelopes opened with your key (`./RESTORE.sh --find-envelope --key YOUR_KEY.age.key`), and the newest version of every unit any of them lists is restored: the highest version number among the versions with that unit's `uuid`",
            "Every unit the newest readable index names (`--where --all`) is restored at its newest version, or you have written down that it is held only on tapes you do not have",
            "A tape your key cannot open is NOT done.",
            "\"no envelope matched the key provided\" (or \"any of the N keys provided\")",
            "`--info`, which needs no key, shows only the tape's label, dates and map, never what it holds",
            "Do not count that tape as looked at: name it (a tape of another installation, or a key you do not have) and try every other key you hold on it. If none opens it, the box is not done, and you say so, by that tape's name",
            "A tape written before 1.3.0 has no index. It counts by its own envelopes alone, and nothing on it names a tape or a unit but its own",
            "An unsealed tape (no \"Sealed at\") is ordered by File 0's `created_at`",
            "If the newest tape in the box will not read, the older tapes' indexes cannot see what was written after them: say that some units may be unknown",
            "The newest version of a unit holds the files that version had. A file deleted before it was made exists only in an older version, so restore an older version (`--version N`, into a directory of its own) only if you want such files",
            "(`unknown (tape not sealed)` there means the tape is unsealed: File 0's `created_at`, from the `grep` below, says when it was written)",
            "grep -E '^(label|uuid|media_type|cartridge_serial|created_at|tapectl_version) ='",
        ] {
            assert!(bx.contains(needle), "{needle:?}: {bx}");
        }
        // Start here's last step says the same in short, and the glossary says
        // what "every file" means.
        let start = flat_text(guide_section(&g, "Start here"));
        for needle in [
            "Know when you are done: every tape you hold opened with your key and the newest version of every unit any of them lists restored",
            "every unit `--where --all` names on the newest readable index, and every unit the Heir Kit's catalogue lists on a sealed tape, restored or named as held only on tapes you do not have",
            "any tape your key cannot open named, never counted as looked at",
        ] {
            assert!(start.contains(needle), "{needle:?}: {start}");
        }
        let words = flat_text(guide_section(&g, "Words this guide uses"));
        assert!(
            words.contains("\"Every file\" means the files the unit had when that version was made: a file deleted before then exists only in an older version, so restore an older version (`--version N`, into a directory of its own) only if you want such files"),
            "{words}"
        );
    }

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

    /// M-paths: the box procedure's directory (`/destination/UNIT`) is the
    /// one every example uses, and `--all` is one envelope, not the union of
    /// several keys' envelopes.
    #[test]
    fn reread3_m_paths_agree_on_the_unit_directory_and_all_is_one_envelope() {
        let g = guide17();
        let dmg = guide_section(&g, "If a slice or a file is damaged");
        let f = flat_text(dmg);
        for needle in [
            "--fill-from /destination/UNIT/.tapectl-damaged-files.txt \\ --to /destination/UNIT",
            "--slices-from DIR --to /destination/UNIT",
            "--swap-tapes --to /destination/UNIT",
            "The box procedure restores each unit with `--to /destination/UNIT`, so its list is `/destination/UNIT/.tapectl-damaged-files.txt`",
        ] {
            assert!(f.contains(needle), "{needle:?}: {f}");
        }
        assert!(
            !dmg.contains("--fill-from /destination/.tapectl-damaged-files.txt"),
            "{dmg}"
        );
        let auto =
            flat_text(guide_section(&g, "Automated Recovery (recommended)")).replace(" # ", " ");
        for needle in [
            "--unit UNIT \\ --path some/file --to /destination/UNIT",
            "It is always ONE envelope, never the union of several. With several keys (`--key` repeated) it is the biggest envelope any of them opens, and the first on the tape if two tie",
            "the escrow or operator key, alone or with others, therefore restores every unit on the tape, but two tenants' keys together restore only the tenant with more units",
            "Restore the other tenant in a run of its own, with that tenant's key, into another empty directory",
            "Full restore of THIS tape to an EMPTY directory: --all restores every unit of one envelope (below), each into /destination/UNIT",
        ] {
            assert!(auto.contains(needle), "{needle:?}: {auto}");
        }
        // RECOVERY.md agrees.
        let r = flat_text(&generate_recovery_md("LAB01", "alice", &[], &[]));
        for needle in [
            "--unit UNIT --slices-from DIR --to /destination/UNIT",
            "--unit UNIT --swap-tapes --to /destination/UNIT",
            "--fill-from /destination/UNIT/.tapectl-damaged-files.txt --to /destination/UNIT",
            "with each unit restored into `/destination/UNIT`, that is `/destination/UNIT/.tapectl-damaged-files.txt`",
        ] {
            assert!(r.contains(needle), "{needle:?}: {r}");
        }
    }

    /// M-script: the first command saves the script under the tape's own name,
    /// and the guide says what `./RESTORE.sh` means everywhere else.
    #[test]
    fn reread3_m_script_is_saved_under_the_tapes_own_name_from_the_first_command() {
        let g = guide17();
        let auto = guide_section(&g, "Automated Recovery (recommended)");
        let fetch: Vec<&str> = auto
            .lines()
            .filter(|l| l.contains("| tr -d '\\0' >"))
            .collect();
        assert_eq!(
            fetch,
            ["    dd if=/dev/nst0 bs=512k | tr -d '\\0' > RESTORE-READ17.sh"]
        );
        assert!(
            auto.lines()
                .any(|l| l.trim() == "chmod +x RESTORE-READ17.sh"),
            "{auto}"
        );
        assert!(!g.contains("> RESTORE.sh"), "no fetch to the shared name");
        assert!(!g.contains("chmod +x RESTORE.sh"));
        let f = flat_text(auto);
        for needle in [
            "The script is saved under this tape's own name, `RESTORE-READ17.sh`, because a box has several tapes and each has its own RESTORE.sh: scripts saved under one name overwrite each other",
            "Everywhere else this guide writes `./RESTORE.sh`, it means the script of the tape that is in the drive: for this tape, `./RESTORE-READ17.sh`; for another tape, the file you saved from it",
        ] {
            assert!(f.contains(needle), "{needle:?}: {f}");
        }
        let start = flat_text(guide_section(&g, "Start here"));
        assert!(
            start.contains("off the tape into a file named for the tape, `RESTORE-READ17.sh`"),
            "{start}"
        );
        let bx = flat_text(guide_section(
            &g,
            "The rest of the box: other tapes, copies and the index",
        ));
        assert!(
            bx.contains("In the last two put THAT tape's label (the `Tape identifies as:` line of `--info`) in place of this tape's, so that each tape's script is kept under its own name, `RESTORE-<label>.sh`"),
            "{bx}"
        );
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

    /// Item 3: the disk is chosen after the units are listed, on a filesystem
    /// that keeps what the archive holds, and "allow generously" is a number.
    #[test]
    fn reread4_item3_the_disk_is_chosen_after_the_units_are_listed() {
        let g = guide17();
        let raw = guide_section(&g, "Start here");
        let start = flat_text(raw);
        // Step 2 no longer asks for a disk; step 5 does, after the listings.
        let step2 = start.split("3. Load the tape").next().unwrap();
        assert!(
            !step2.contains("disk") && !step2.contains("Disk space"),
            "{step2}"
        );
        let at_listing = start
            .find("lists EVERY unit the archive's index names")
            .unwrap();
        let at_disk = start
            .find("Those listings give each unit's size, so choose the disk now: a Linux filesystem (ext4 or xfs; not FAT, exFAT or NTFS) with room for the units you will restore: \"Disk space\"")
            .expect("the disk is chosen in step 5");
        let at_restore = start.find("6. Restore.").unwrap();
        assert!(at_listing < at_disk && at_disk < at_restore, "{start}");
        let disk = flat_text(guide_section(&g, "Disk space"));
        for needle in [
            "Choose the disk once you have listed the units you will restore (`--find-envelope` and `--where --all` print each unit's size), not before",
            "The disk needs a filesystem that keeps what the archive holds: file names exactly as they were, files of 4 GiB and more, symbolic links and Unix permissions. A Linux filesystem does (ext4 and xfs are the usual ones)",
            "FAT, exFAT and NTFS do not keep all of that (FAT cannot hold a file of 4 GiB or more, and none of the three keeps Unix permissions and symbolic links as Linux does)",
            "three times that, and more for mostly text, when you cannot tell that a unit was stored uncompressed (\"Compressed units\", below), and watch `df` while it runs",
            "With no `dar_command` line, or a compressed unit, restore to a disk you can add room to and watch `df`. RESTORE.sh names a full disk when it happens, and `--overwrite` finishes the restore once you have made room",
        ] {
            assert!(disk.contains(needle), "{needle:?}: {disk}");
        }
        assert!(!disk.contains("Choose it before you start"), "{disk}");
        assert!(!disk.contains("allow generously"), "{disk}");
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

    /// Item 5: the package for each tool, an install line, and the oldest dar
    /// and age.
    #[test]
    #[ignore = "L3-pending T4.13: File 1's packages paragraph greps a MANIFEST block for its dar_version line, which Layout 3's block does not state (D6)"]
    fn reread4_item5_packages_and_the_oldest_dar_and_age() {
        let g = guide17();
        let tools = flat_text(guide_section(&g, "Tools Required"));
        for needle in [
            "`mt` (mt-st package)",
            "`dd` — reading raw data from tape (coreutils)",
            "`age` — decryption (age-encryption.org; package age on Debian 11 and later and on Ubuntu 22.04 and later, none on Debian 10, where a release from github.com/FiloSottile/age/releases does)",
            "any age from 1.0.0-rc.1 (March 2021) on opens them, the oldest tried",
            "`dar` — archive extraction (package dar; dar.linux.free.fr)",
            "`tar` — unpacking your decrypted envelope (it is a tar archive; package tar)",
            "Packages: bash; mawk or gawk (either is `awk`); coreutils; sed; grep",
            "`gpg` (package gnupg)",
            "The oldest dar that reads a unit is the one that wrote it, or any newer one: dar reads every archive format that existed when it was released (its own documentation says so), and an older dar may refuse a format it does not know",
            "MANIFEST.toml records the dar that wrote each unit, in its `dar_version` line (unpacked envelope: `grep '^dar_version' env/MANIFEST.toml`",
            "If your dar is older than a unit's `dar_version`, install that version or a newer one before you give the unit up. 2.6 is the oldest dar tapectl writes with, not a promise that every dar from 2.6 on reads every unit",
            "`lsscsi` (package lsscsi), `lspci` (pciutils), `dmesg` (util-linux), `lsmod` and `modprobe` (kmod), `sudo` (sudo) and `usermod` (passwd)",
            "Debian 10 has every one of those packages but `age`",
        ] {
            assert!(tools.contains(needle), "{needle:?}: {tools}");
        }
        // The one install line names the package of every tool the section
        // lists (the RESTORE.sh tools come from coreutils, tar, age, dar and
        // mt-st; gpg from gnupg; the rest as the section says).
        let section = guide_section(&g, "Tools Required");
        let install = section
            .lines()
            .find(|l| l.trim_start().starts_with("sudo apt install"))
            .expect("an install line");
        let packages: Vec<&str> = install.split_whitespace().skip(3).collect();
        assert_eq!(
            packages,
            [
                "mt-st",
                "coreutils",
                "tar",
                "age",
                "dar",
                "gnupg",
                "sqlite3",
                "lsscsi",
                "pciutils",
                "kmod",
                "sg3-utils"
            ]
        );
        // The dar floor the text states is the one tapectl enforces.
        assert_eq!(crate::dar::version::MIN_VERSION, (2, 6));
        // The MANIFEST line the text greps for is the one the writer emits.
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
        assert!(
            manifest.lines().any(|l| l == "dar_version = \"2.7.20\""),
            "{manifest}"
        );
    }

    /// Item 6: where tapectl comes from and the steps that use it. Every
    /// command line shown parses as the real CLI, and the key rule is stated.
    #[test]
    fn reread4_item6_tapectl_itself_says_where_it_comes_from_and_what_to_run() {
        use clap::Parser;
        let g = guide17();
        let raw = guide_section(&g, "tapectl itself");
        let f = flat_text(raw);
        for needle in [
            "tapectl is open-source software, and its source is at https://github.com/mikmorg/tapectl: one place to look, and a web address may move over decades, so search for the name if it is gone",
            "File 0's `tapectl_version` names the release that wrote this tape; the repository tags each release `v` followed by that number (such as `v1.5.0`); use that release or a newer one",
            "the Rust toolchain the repository pins in `rust-toolchain.toml` (`rustup`, from rustup.rs, installs it on the first `cargo` run, or when you run `rustup toolchain install` in the checkout; a distribution's own cargo may be too old)",
            "`dar`, `mt-st` and `sg3-utils` installed (\"Tools Required\")",
            "`init` makes the home and registers the escrow PUBLIC key, which decrypts nothing; run it once",
            "`catalog rebuild` reads the loaded tape and puts what it says into tapectl's own catalogue (its database, in the new home); run it once for each tape",
            "`restore unit` restores the newest version of a unit on the loaded tape into an empty directory (`--version N` for another)",
            "The repository's `docs/keys-and-recovery.md` (runbook C) is the long form",
            "`--key` names a key file for that one command and nothing more: tapectl does not copy it into its home or register it, and the escrow secret is never stored on the machine. Give it on each command, and once for each key file if the units need more than one (`--key` repeats)",
            "With the escrow key this route does what RESTORE.sh cannot: it reads around a damaged 64 KiB piece of a slice, loses only the files in that piece and names them",
            "The alternative, with the operator's key file: adopt it at `init` with `--operator-key OPERATOR.age.key` beside `--escrow-public-key`, and leave `--key` off the restore, which then reads the key from its home's `keys/`",
            "`init` refuses the escrow secret as `--operator-key` (that secret is kept on paper, apart from every machine); `--key` is how the escrow key is used",
        ] {
            assert!(f.contains(needle), "{needle:?}: {f}");
        }
        // File 0 carries the package version, which is what the tags spell.
        assert_eq!(crate::build_info::PKG_VERSION, env!("CARGO_PKG_VERSION"));
        // The commands are the real CLI's.
        let cmds: Vec<&str> = raw
            .lines()
            .filter(|l| l.starts_with("    tapectl "))
            .map(str::trim)
            .collect();
        assert_eq!(cmds.len(), 3, "{cmds:?}");
        for cmd in &cmds {
            // `restore unit|file|volume` take `--key FILE` (ADR-0012 item 107):
            // every command is parsed as File 1 prints it.
            let argv: Vec<&str> = cmd.split_whitespace().collect();
            crate::cli::Cli::try_parse_from(&argv)
                .unwrap_or_else(|e| panic!("File 1 shows a command the CLI refuses: {cmd}: {e}"));
        }
        assert_eq!(
            cmds[0],
            "tapectl init --operator OPERATOR_NAME --escrow-public-key ESCROW_PUBLIC"
        );
        assert!(cmds[1]
            .contains("catalog rebuild --from-volume --device /dev/nst0 --key YOUR_KEY.age.key"));
        assert!(cmds[2].contains(
            "restore unit --unit UNIT --device /dev/nst0 --to /destination/UNIT --key YOUR_KEY.age.key"
        ));
        // The one-line query for the operator's name runs on the kit's schema.
        let query = "SELECT name FROM tenants WHERE is_operator = 1;";
        assert!(f.contains(query), "{f}");
        let conn = kit_catalogue();
        conn.execute(
            "INSERT INTO tenants (name, is_operator, status) VALUES ('mike', 1, 'active')",
            [],
        )
        .unwrap();
        let name: String = conn.query_row(query, [], |r| r.get(0)).unwrap();
        assert_eq!(name, "mike");
        // The escrow public key is what `init` registers, and the CLI says so.
        let argv = [
            "tapectl",
            "init",
            "--operator",
            "NAME",
            "--escrow-public-key",
            "age1x",
        ];
        crate::cli::Cli::try_parse_from(argv).unwrap();
        for cmd in [
            "git clone https://github.com/mikmorg/tapectl",
            "cargo build --release",
        ] {
            assert!(
                raw.lines().any(|l| l.trim().starts_with(cmd)),
                "{cmd}: {raw}"
            );
        }
        // The damaged-slice section points here and promises no more than this.
        let dmg = flat_text(guide_section(&g, "If a slice or a file is damaged"));
        for needle in [
            "tapectl is the program that wrote this tape (File 0 names its release in `tapectl_version`); it is open-source software",
            "reads around a damaged 64 KiB piece of a slice, loses only the files in that piece and names them, and fills files from another copy of the same version",
            "\"tapectl itself\", next, says where to get it and what to run",
        ] {
            assert!(dmg.contains(needle), "{needle:?}: {dmg}");
        }
        assert!(
            !dmg.contains("http"),
            "the address lives in one place: {dmg}"
        );
        // And the URL is in no other section than this one.
        assert_eq!(g.matches("https://github.com/mikmorg/tapectl").count(), 2);
        let start = flat_text(guide_section(&g, "Start here"));
        assert!(
            start.contains("A slice damaged in the middle, which RESTORE.sh cannot read around: \"tapectl itself\""),
            "{start}"
        );
    }

    /// Item 7: a renamed unit — the name to type is the one the tape shows.
    #[test]
    fn reread4_item7_a_renamed_unit_takes_the_name_the_tape_shows() {
        let g = guide17();
        let bx = guide_section(&g, "The rest of the box: other tapes, copies and the index");
        let f = flat_text(bx);
        for needle in [
            "**A renamed unit.** A unit is the same unit by its `uuid`, but every command takes its NAME, and a tape's envelope knows a unit by the name it had when THAT tape was written",
            "the newest index lists it under the new name (with the older tape as a holder of an earlier version), and the older tape's envelope still says the old name",
            "`--unit NEWNAME` finds nothing there: RESTORE.sh says an envelope \"decrypts but does not list\" the name, then ends \"no envelope for unit 'NEWNAME' matched the key provided\". That reads like a wrong key and is not one",
            "Take the unit's uuid from the newest index (`--where --all` prints it as \"unit uuid\", beside the name), and on the older tape type the name THAT tape shows for the same uuid",
            "A unit stored without its dar catalogue has no `catalogs/` name to read: `--find-envelope` with a tenant's key prints the whole MANIFEST.toml",
        ] {
            assert!(f.contains(needle), "{needle:?}: {f}");
        }
        for cmd in [
            "./RESTORE.sh --list --key YOUR_KEY.age.key | grep '^==='",
            "./RESTORE.sh --where --all --key YOUR_KEY.age.key",
        ] {
            assert!(bx.lines().any(|l| l.trim() == cmd), "{cmd}: {bx}");
        }
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

    /// S1: two units of one name (different uuids) never share a directory.
    #[test]
    fn reread5_s1_two_units_of_one_name_take_different_directories() {
        let g = guide17();
        let bx = box_section(&g);
        for needle in [
            "**Two units of one name.** A name is unique inside one installation's catalogue, but it can recur across tapes: a unit renamed and its old name used again, or tapes of two installations",
            "They are different units when their uuids differ (`--where --all` prints each as an entry of its own, with its uuid)",
            "RESTORE.sh refuses a `--to` that is not empty, but the manual dar path keeps what is already there and exits 0, so it merges them without a word",
            "Restore each into a directory of its own, `/destination/UNIT-` followed by the first 8 characters of its uuid, and with the manual path always into a new, empty directory",
        ] {
            assert!(bx.contains(needle), "{needle:?}: {bx}");
        }
        let start = flat_text(guide_section(&g, "Start here"));
        assert!(
            start.contains("Two units of one name (compare the uuids `--where --all` prints) go in different directories: `/destination/UNIT-` and the first 8 characters of the uuid"),
            "{start}"
        );
        let manual = flat_text(guide_section(&g, "Manual Recovery Steps"));
        assert!(
            manual.contains("two units of one name (different uuids) take `/destination/UNIT-` and the first 8 characters of the uuid, each a new directory, or dar would merge them without a word"),
            "{manual}"
        );
        // The facts it rests on: names are unique inside one catalogue; the
        // script refuses a destination that is not empty; --where --all keys
        // a unit by tenant, name AND uuid, so two such units are two entries.
        let conn = crate::db::open_memory().unwrap();
        conn.execute(
            "INSERT INTO tenants (name, is_operator, status) VALUES ('t', 0, 'active')",
            [],
        )
        .unwrap();
        let add = |uuid: &str| {
            conn.execute(
                "INSERT INTO units (uuid, name, tenant_id, checksum_mode, encrypt, status)
                 VALUES (?1, 'photos', 1, 'mtime_size', 1, 'active')",
                rusqlite::params![uuid],
            )
        };
        add("11111111-2222-3333-4444-555555555555").unwrap();
        assert!(add("66666666-2222-3333-4444-555555555555").is_err());
        let r = flat_text(&generate_recovery_md("LAB01", "alice", &[], &[]));
        assert!(
            r.contains("Two units of one name but different `unit_uuid` are different units: restore each into a directory of its own, such as `/destination/UNIT-` and the first 8 characters of the uuid"),
            "{r}"
        );
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

    /// M3: `--all` uses ONE envelope, and the heir is told to check which.
    #[test]
    fn reread5_m3_all_uses_one_envelope_so_check_which() {
        let g = guide17();
        let auto = flat_text(guide_section(&g, "Automated Recovery (recommended)"));
        for needle in [
            "If the operator envelope and its backup will not read, the biggest envelope that does read may be a tenant's, and --all then restores only that tenant's units, with the DAMAGED warnings the script printed as the only sign",
            "The script names the file it used (\"Decrypted envelope at file N\"). Item 4 of \"Did the restore get everything?\" is the check, and what to do when it fails",
        ] {
            assert!(auto.contains(needle), "{needle:?}: {auto}");
        }
        // The fifth fresh read (#481): the check is in the checklist itself,
        // where an heir who believes the restore finished will look.
        let check = flat_text(guide_section(&g, "Did the restore get everything?"));
        for needle in [
            "4. After `--all`, every unit of the tape was restored, not only one envelope's",
            "when the operator envelope and its backup will not read it is a tenant's, and only that tenant's units are restored, while items 1 to 3 still pass",
            "Every unit that `./RESTORE.sh --find-envelope --key YOUR_KEY.age.key` lists, under any envelope, must be a directory of the `--all` destination",
            "`./RESTORE.sh --restore --key YOUR_KEY.age.key --unit UNIT --to /destination/UNIT` (the script finds the envelope that lists it)",
        ] {
            assert!(check.contains(needle), "{needle:?}: {check}");
        }
        let verdict = flat_text(guide_section(&g, "If the verdict is UNSEALED or DAMAGED"));
        assert!(
            verdict.contains("`--all` restores the units in their order on the tape, so it stops there too: the units before that one are restored, that unit and every unit after it are not"),
            "{verdict}"
        );
    }

    /// M4: ADR-0012 item 107, tapectl's restore and rebuild take `--key`.
    #[test]
    fn reread5_m4_an_escrow_only_heir_can_use_tapectl() {
        let g = guide17();
        let raw = guide_section(&g, "tapectl itself");
        let f = flat_text(raw);
        assert!(!f.contains("never the escrow key:"), "{f}");
        assert!(!f.contains("cannot decrypt"), "{f}");
        assert!(
            !f.contains("this route needs the operator's key file"),
            "{f}"
        );
        for line in [
            "tapectl init --operator OPERATOR_NAME --escrow-public-key ESCROW_PUBLIC",
            "tapectl catalog rebuild --from-volume --device /dev/nst0 --key YOUR_KEY.age.key",
            "tapectl restore unit --unit UNIT --device /dev/nst0 --to /destination/UNIT --key YOUR_KEY.age.key",
        ] {
            assert!(raw.lines().any(|l| l.trim() == line), "{line}: {raw}");
        }
        for needle in [
            "YOUR_KEY.age.key is your key file, the escrow key's included",
            "otherwise give any name, your own say, because the rebuild takes the tenants' names from the tape. (Run as an ordinary user, `init` without `--operator` names the operator after your login; run as root, under `sudo`, it refuses without one.)",
            "The alternative, with the operator's key file",
        ] {
            assert!(f.contains(needle), "{needle:?}: {f}");
        }
        // The operator-key route is still real: the CLI parses it.
        use clap::Parser;
        crate::cli::Cli::try_parse_from([
            "tapectl",
            "init",
            "--operator",
            "NAME",
            "--operator-key",
            "OPERATOR.age.key",
            "--escrow-public-key",
            "age1x",
        ])
        .unwrap();
    }

    /// M5: which RESTORE.sh a survey takes, said up front.
    #[test]
    fn reread5_m5_any_scripts_for_the_survey_and_the_tapes_own_for_a_restore() {
        let g = guide17();
        let start = flat_text(guide_section(&g, "Start here"));
        assert!(
            start.contains("Any tape's script reads the tape in the drive for `--info` and `--find-envelope`, one from a tape written by 1.3.0 or later for `--where`; a restore needs the script of the tape it reads"),
            "{start}"
        );
        let bx = box_section(&g);
        assert!(
            bx.contains("(any copy of RESTORE.sh will do for this: see \"The modes that only read the tape\", after this list)"),
            "{bx}"
        );
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

    /// M8: one disk-size rule, the capacity not a ceiling for a compressed unit.
    #[test]
    fn reread5_m8_the_disk_size_rule_is_stated_once() {
        let g = guide17();
        let disk = flat_text(guide_section(&g, "Disk space"));
        for needle in [
            "**How much room, in one place:** the sum of the sizes of the units you restore, as `--find-envelope` and `--where` print them (each unit's slices as stored on tape); three times that, and more for mostly text, when you cannot tell that a unit was stored uncompressed (\"Compressed units\", below), and watch `df` while it runs; and, for the ways of restoring that keep its slices on disk as well (below), one more unit's size, for the slices of the unit being restored",
            "The tape's capacity bounds what is stored on it, not what a restore writes",
            "a unit stored compressed restores larger, and the capacity is no ceiling for it (\"Compressed units\", below)",
            "\"Three times\" is a rule of thumb for mixed files, not a ceiling",
        ] {
            assert!(disk.contains(needle), "{needle:?}: {disk}");
        }
        // #485 item 4: the factor is stated twice, once as the rule and once
        // as what it is not; "take the three times above (more for mostly
        // text)" is gone, because it read as a bound.
        assert_eq!(disk.matches("three times").count(), 2, "{disk}");
        assert!(!disk.contains("take the three times above"), "{disk}");
        assert!(!disk.contains("As a ceiling"), "{disk}");
        let unknown = flat_text(guide_section(
            &generate_system_guide_v2_for_media("U", 30, "DLT-IV"),
            "Disk space",
        ));
        assert!(
            unknown.contains("the capacity is no ceiling for it (\"Compressed units\", below)"),
            "{unknown}"
        );
    }

    /// M9: a lone unit lands directly in `--to`, so the list is there.
    #[test]
    fn reread5_m9_a_lone_unit_lands_in_to_and_so_does_its_damaged_list() {
        let g = guide17();
        let dmg = flat_text(guide_section(&g, "If a slice or a file is damaged"));
        for needle in [
            "to `.tapectl-damaged-files.txt` in the unit's directory: `--to` itself when the restore had one unit (a lone unit goes straight into `--to`, `--all` included), `--to/UNIT` when it had several",
            "A tape that holds ONE unit, restored with `--all --to /destination`, puts the files straight into `/destination`: its list is `/destination/.tapectl-damaged-files.txt`, and that directory is the `--to` below",
        ] {
            assert!(dmg.contains(needle), "{needle:?}: {dmg}");
        }
        let auto =
            flat_text(guide_section(&g, "Automated Recovery (recommended)")).replace(" # ", " ");
        assert!(
            auto.contains(
                "each into /destination/UNIT (a lone unit goes straight into /destination)"
            ),
            "{auto}"
        );
        let r = flat_text(&generate_recovery_md("LAB01", "alice", &[], &[]));
        assert!(
            r.contains("a restore of one unit with `--all` puts the files straight into `--to`, and the list is `--to/.tapectl-damaged-files.txt`"),
            "{r}"
        );
    }

    /// M10: an escrow-only heir reads dar_command / dar_version / uuid.
    #[test]
    fn reread5_m10_an_escrow_key_reads_the_manifest_by_unpacking_the_operator_envelope() {
        let g = guide17();
        let tools = flat_text(guide_section(&g, "Tools Required"));
        assert!(
            tools.contains("`--find-envelope` with a tenant's key prints the whole MANIFEST.toml, but with an escrow or operator key it prints only a table of units: then unpack the operator envelope by hand (steps 5 and 6 of \"Manual Recovery Steps\") and read `env/MANIFEST.toml` (the unit's uuid alone is in `--where --all`)"),
            "{tools}"
        );
        let disk = flat_text(guide_section(&g, "Disk space"));
        assert!(
            disk.contains("with an escrow or operator key `--find-envelope` prints only a table of units, so unpack the operator envelope by hand, as under \"Tools Required\""),
            "{disk}"
        );
        let bx = box_section(&g);
        assert!(
            bx.contains("with an escrow or operator key `--find-envelope` prints only a table of units (see \"Tools Required\"), so take the uuid from `--where --all`, or print the operator envelope's MANIFEST.toml (\"The short way\", in \"Compressed units\")"),
            "{bx}"
        );
    }

    /// M11: what to do when the installed dar is older than the unit's.
    #[test]
    fn reread5_m11_an_older_dar_than_the_units_is_replaced() {
        let g = guide17();
        let tools = flat_text(guide_section(&g, "Tools Required"));
        assert!(
            tools.contains("If your dar is older than a unit's `dar_version`, install that version or a newer one before you give the unit up. 2.6 is the oldest dar tapectl writes with, not a promise that every dar from 2.6 on reads every unit"),
            "{tools}"
        );
        assert!(!tools.contains("so no unit asks for less"), "{tools}");
    }

    /// M13: the manual tool list names `truncate`.
    #[test]
    fn reread5_m13_the_manual_tool_list_names_truncate() {
        let g = guide17();
        let manual = flat_text(guide_section(&g, "Manual Recovery Steps"));
        assert!(
            manual.contains("follow these steps with `mt`, `dd`, `truncate`, `age`, `tar`, `sha256sum` and `dar`"),
            "{manual}"
        );
        // Every tool a step runs by name is in that list.
        let raw = guide_section(&g, "Manual Recovery Steps");
        let used: std::collections::BTreeSet<&str> = raw
            .lines()
            .map(str::trim)
            .filter(|l| {
                l.starts_with("mt ")
                    || l.starts_with("dd ")
                    || l.starts_with("truncate ")
                    || l.starts_with("age ")
                    || l.starts_with("tar ")
                    || l.starts_with("sha256sum ")
                    || l.starts_with("dar ")
            })
            .filter_map(|l| l.split_whitespace().next())
            .collect();
        for tool in used {
            assert!(manual.contains(&format!("`{tool}`")), "{tool}");
        }
    }

    /// M12: the manual slice command stops at a failed hash.
    #[test]
    fn reread5_m12_the_manual_slice_line_stops_at_a_failed_hash() {
        let g = guide17();
        let manual = guide_section(&g, "Manual Recovery Steps");
        let line = manual
            .lines()
            .map(str::trim)
            .find(|l| l.starts_with("echo \"SHA256_ENCRYPTED"))
            .expect("the hash line");
        assert_eq!(
            line,
            "echo \"SHA256_ENCRYPTED  restore.NUMBER.dar.age\" | sha256sum -c - && age -d -i YOUR_KEY.age.key restore.NUMBER.dar.age > restore.NUMBER.dar && rm restore.NUMBER.dar.age"
        );
        assert!(
            !manual
                .lines()
                .any(|l| l.trim() == "rm restore.NUMBER.dar.age"),
            "rm is never a line of its own"
        );
        let f = flat_text(manual);
        assert!(
            f.contains("The last line is one command on purpose: each `&&` stops it at the first step that fails, so a slice whose hash is wrong is never decrypted, and its encrypted copy is deleted only after it decrypted"),
            "{f}"
        );
        // RECOVERY.md has the same chain.
        let unit = ManifestUnit {
            name: "u".into(),
            uuid: "11111111-2222-3333-4444-555555555555".into(),
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
                size_bytes: 10,
                encrypted_bytes: 20,
                sha256_plain: "aa".into(),
                sha256_encrypted: "bb".into(),
            }],
        };
        let r = generate_recovery_md("LAB01", "alice", &[unit], &[]);
        assert!(
            r.lines().any(|l| l == "echo \"bb  restore.1.dar.age\" | sha256sum -c - && age -d -i YOUR_KEY.age.key restore.1.dar.age > restore.1.dar && rm restore.1.dar.age"),
            "{r}"
        );
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

    /// M15: a key checked without a cover sheet.
    #[test]
    fn reread5_m15_a_key_is_checked_without_a_cover_sheet_by_opening_an_envelope() {
        let g = guide17();
        let start = flat_text(guide_section(&g, "Start here"));
        assert!(
            start.contains("Check your key: \"Which file is your key\". A tenant's key opens that tenant's units only; the operator key and the escrow key open every tenant's. With `age` installed (step 2), check it against the cover sheet's `age1...` line (`age-keygen -y`); with only a key file, the check is that it opens an envelope (`--find-envelope`, step 5)"),
            "{start}"
        );
    }

    // ---- The sixth read's fixes (#485, ADR-0012 item 109, tapectl 1.5.0) ----
    // File 1 and RECOVERY.md text only; the commands they show are run
    // against the fake tape and a real dar in tests/heir_restore_sh.rs.

    /// #485 item 1: "done" is said in step 8 itself to hold over the tapes you
    /// hold, with the two reasons, and Start here still fits its page.
    #[test]
    fn reread6_i1_step_8_says_done_is_over_the_tapes_you_hold() {
        let g = guide17();
        let start = flat_text(guide_section(&g, "Start here"));
        let step8 = &start[start.find("8. Know when you are done").unwrap()..];
        for needle in [
            "No tape names one written after it, and a tape from before 1.3.0 has no index: a kit made later can name such tapes (\"The Heir Kit's catalogue\")",
            "\"The rest of the box\" has the whole rule, a damaged unit's place in it, and what a box cannot show",
        ] {
            assert!(step8.contains(needle), "{needle:?}: {step8}");
        }
        // The caveat's source, "Is the box complete?", is still where it was.
        assert!(
            box_section(&g).contains("Nothing on a tape can name tapes written after it"),
            "the full rule stays in the box section"
        );
        // 67 since the fresh read of a span (#478 m4); the history is in
        // reread2_s2_start_here_comes_first_and_names_sections_that_exist.
        assert!(guide_section(&g, "Start here").lines().count() <= 67);
    }

    /// #485 item 2: a way to see the files only an older version has, with
    /// the options RESTORE.sh really takes (`--list` and `--restore` both
    /// take `--version`; `--restore` takes `--path`).
    #[test]
    fn reread6_i2_files_deleted_before_the_newest_version_can_be_listed() {
        let g = guide17();
        let bx = box_section(&g);
        for needle in [
            "**Files deleted before the newest version.** Nothing says which files exist only in an older version, but you can see: list the paths of two versions and print those the older one has and the newer one lacks. `--list` takes `--version`",
            "needs both versions on the tape in the drive and their dar catalogues in its envelope (a version without one prints \"no dar catalogue\")",
            "./RESTORE.sh --list --key YOUR_KEY.age.key --unit UNIT --version 3 | grep -E '^\\[.{5}\\]\\[.\\] ' | awk -F'\\t' '{ print $NF }' > newer.paths",
            "./RESTORE.sh --list --key YOUR_KEY.age.key --unit UNIT --version 2 | grep -E '^\\[.{5}\\]\\[.\\] ' | awk -F'\\t' '{ print $NF }' > older.paths",
            "awk 'NR == FNR { seen[$0]; next } !($0 in seen)' newer.paths older.paths",
            "Nothing printed means version 3 has every path version 2 had",
            "./RESTORE.sh --restore --key YOUR_KEY.age.key --unit UNIT --version 2 \\ --path PATH --to /destination/UNIT-v2",
        ] {
            assert!(bx.contains(needle), "{needle:?}: {bx}");
        }
        let words = flat_text(guide_section(&g, "Words this guide uses"));
        assert!(
            words.contains("(\"Files deleted before the newest version\", in \"The rest of the box\", finds them)"),
            "{words}"
        );
        // Only tools the guide lists: no comm, no diff.
        let block = guide_section(&g, "The rest of the box: other tapes, copies and the index");
        assert!(
            !block.contains("comm ") && !block.contains("diff "),
            "{block}"
        );
    }

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

    /// #485 item 3: a newer dar has somewhere to come from.
    #[test]
    fn reread6_i3_a_newer_dar_has_a_source() {
        let g = guide17();
        let tools = flat_text(guide_section(&g, "Tools Required"));
        for needle in [
            "Where to get a newer dar: a newer release of your distribution may package one (`apt-cache policy dar` says which version it offers); otherwise dar's own site, dar.linux.free.fr (release notes and source tarballs), and its project at sourceforge.net/projects/dar (the same releases)",
            "A source tarball builds with the commands in its INSTALL and README files (for most releases `./configure`, `make` and `make install`, which name the libraries it needs)",
            "These addresses may move over decades: search for \"dar\" and its author, Denis Corbin, if they are gone",
        ] {
            assert!(tools.contains(needle), "{needle:?}: {tools}");
        }
    }

    /// #485 item 4: an escrow-only heir has a short way to a unit's
    /// `dar_command` (the commands are run on the fake tape in
    /// tests/heir_restore_sh.rs), and "three times" is not offered as a bound.
    #[test]
    fn reread6_i4_the_dar_command_has_a_short_way_and_three_times_is_no_ceiling() {
        let g = guide17();
        let disk = flat_text(guide_section(&g, "Disk space"));
        for needle in [
            "The short way to the `dar_command` lines, with an escrow or operator key and no unpacking of the whole envelope. `--find-envelope` prints \"Decrypted envelope at file N (tenant: operator)\" for the operator envelope, the one that lists every unit; call that N POSITION below. Read File 3 into `front_index.toml` (step 3 of \"Manual Recovery Steps\") and run `grep -A3 '^position = POSITION$' front_index.toml`: the entry it prints has the `size_bytes` that is SIZE below",
            "mt -f /dev/nst0 rewind mt -f /dev/nst0 fsf POSITION dd if=/dev/nst0 bs=512k of=envelope.raw truncate -s SIZE envelope.raw age -d -i YOUR_KEY.age.key envelope.raw | tar -xOf - MANIFEST.toml | grep -E '^(name|snapshot_version|dar_command) ='",
            "It prints each unit's name and version with its `dar_command`. Without the last `grep` it prints the whole MANIFEST.toml",
            "If the envelope will not read, use its backup (step 5 of \"Manual Recovery Steps\")",
            "text, source code, logs and database dumps compress ten times or more",
            "and nothing on the tape says how much more",
        ] {
            assert!(disk.contains(needle), "{needle:?}: {disk}");
        }
        let tools = flat_text(guide_section(&g, "Tools Required"));
        assert!(
            tools.contains("(\"Compressed units\", in \"Disk space\", has the short way to one unit's `dar_command`.)"),
            "{tools}"
        );
    }

    /// #485 item 5, File 1's half: the Heir Kit's copy is the first place
    /// to look for tapectl, before the repository, and the glossary says a
    /// kit may hold one.
    #[test]
    fn reread6_i5_tapectl_itself_looks_in_the_heir_kit_first() {
        let g = guide17();
        let sec = flat_text(guide_section(&g, "tapectl itself"));
        let kit = sec
            .find("Look in the Heir Kit first.")
            .expect("the kit comes first");
        let repo = sec
            .find("tapectl is open-source software, and its source is at")
            .unwrap();
        assert!(kit < repo, "{sec}");
        for needle in [
            "A kit made by tapectl 1.5.0 or later holds a copy of the program that made it, in the folder with `catalog.db.age`: a file named `tapectl-` and its version, `tapectl-X.Y.Z` below (such as `tapectl-1.5.0`)",
            "The cover sheet names it with its size and sha256 (check the file against the sheet with `sha256sum`) and says it is for Linux on one machine type (`uname -m` says yours)",
            "It needs no building: run it as `./tapectl-X.Y.Z` wherever this section says `tapectl`, after `./tapectl-X.Y.Z --version` has shown it starts",
            "Copy it to a Linux disk first and run `chmod +x` on it: from a USB stick formatted FAT or exFAT, or mounted `noexec`, it answers \"Permission denied\"",
            "Its release must be File 0's `tapectl_version` or newer (the `grep` of File 0 in \"The rest of the box\" prints it); a kit made before the tape was written can hold an older one",
            "With no kit, a copy that is too old, or one that will not run on your machine, read on",
        ] {
            assert!(sec.contains(needle), "{needle:?}: {sec}");
        }
        let words = flat_text(guide_section(&g, "Words this guide uses"));
        assert!(
            words.contains("A kit made by tapectl 1.5.0 or later also holds a copy of the tapectl program that made it, a file named `tapectl-` and its version (\"tapectl itself\")"),
            "{words}"
        );
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

    /// #485 item 8: "catalogue" is never bare, the two `NAME`s are two names,
    /// and the position placeholders follow the manifest's field.
    #[test]
    fn reread6_i8_the_overloaded_words_are_told_apart() {
        let g = guide17();
        let words = flat_text(guide_section(&g, "Words this guide uses"));
        for needle in [
            "**Index**: two different things. The FRONT index is File 3 (the seal marker, the last file, carries a copy): a plain-text map of THIS tape, with every tape file's position, type, size and hash. The index, with no \"front\", is the directory `index/` in an envelope of a tape written by tapectl 1.3.0 or later: which units, versions and paths the archive had when that tape was written, and which tapes hold them",
            "**Catalogue**: this guide never uses the word bare, because it names three different things. The Heir Kit's catalogue is the archive's database, as of the day the kit was made, kept as `catalog.db.age` (\"The Heir Kit's catalogue\"); an installation's own catalogue is the live database it was copied from. A dar catalogue (the next entry) is a small file that lists one version of one unit. `catalogs/` is the directory of an envelope that holds the dar catalogues",
        ] {
            assert!(words.contains(needle), "{needle:?}: {words}");
        }
        let start = flat_text(guide_section(&g, "Start here"));
        assert!(
            start.contains(
                "Unit, envelope, index, cover sheet: \"Words this guide uses\" says what each is"
            ),
            "{start}"
        );
        assert!(
            start
                .find("\"Words this guide uses\" says what each is")
                .unwrap()
                < start.find("1. Check your key").unwrap(),
            "the pointer comes before the first word is used"
        );
        // No bare "catalogue": each use follows one of the qualifiers this
        // guide defines (or is the word being defined, or a plural).
        let flat = flat_text(&g);
        let mut at = 0;
        while let Some(i) = flat[at..].find("catalogue") {
            let i = at + i;
            // The 24 characters before it (not bytes: the guide has dashes).
            let before: String = {
                let mut v: Vec<char> = flat[..i].chars().rev().take(24).collect();
                v.reverse();
                v.into_iter().collect()
            };
            let after: String = flat[i + 9..].chars().take(40).collect();
            let ok = [
                "dar ",
                "Kit's ",
                "installation's ",
                "installation's own ",
                "tapectl's own ",
                "its own ",
                "dar-",
                "Catalogue** ",
                "archive this ",
            ]
            .iter()
            .any(|p| before.ends_with(p))
                || flat[i..].starts_with("catalogues");
            assert!(ok, "a bare 'catalogue': ...{before}[catalogue]{after}");
            at = i + 9;
        }
        // The operator's name and the dar catalogue's stem are not both NAME.
        let itself = guide_section(&g, "tapectl itself");
        assert!(itself.contains("--operator OPERATOR_NAME"), "{itself}");
        assert!(!itself.contains("--operator NAME"), "{itself}");
        assert!(
            flat_text(itself).contains("OPERATOR_NAME is the operator's name where you know it"),
            "{itself}"
        );
        // The position placeholders: the front index's `position` is POSITION,
        // the manifest's `tape_position` is FIRST_TAPE_POSITION.
        let manual = flat_text(guide_section(&g, "Manual Recovery Steps"));
        assert!(
            manual.contains("with POSITION (the entry's `position`) and SIZE (its `size_bytes`)"),
            "{manual}"
        );
        assert!(manual.contains("FIRST_TAPE_POSITION") && !manual.contains("FIRST_POSITION"));
        let dmg = flat_text(guide_section(&g, "If a slice or a file is damaged"));
        assert!(
            dmg.contains("by its sha256 (the entry's `position` is POSITION), then:"),
            "{dmg}"
        );
        // RECOVERY.md says "dar catalogue" for the envelope's copy and names
        // the other one by where it lies.
        let unit = ManifestUnit {
            name: "alpha".into(),
            uuid: "aaaaaaaa-0000-0000-0000-000000000000".into(),
            snapshot_version: 2,
            stage_set_id: 1,
            payload: "tar".to_string(),
            stream_bytes: 0,
            file_table: None,
            id_map: None,
            tar_version: None,
            compression: None,
            slices: vec![],
        };
        let r = flat_text(&generate_recovery_md(
            "LAB01",
            "alice",
            &[unit],
            &["aaaaaaaa_v2.1.dar".to_string()],
        ));
        assert!(
            r.contains("If dar cannot read the catalogue at the end of the last slice, `-A catalogs/aaaaaaaa_v2` gives it this envelope's dar catalogue instead"),
            "{r}"
        );
    }

    // ---- The sixth read (1.5.0, #485, ADR-0012 item 109): what it found ----

    /// S1: "done" checks the Heir Kit's catalogue, the one thing an heir holds
    /// that can name a tape written after the newest in the box; S2: a unit
    /// whose every copy is damaged has a done state; and the minors that
    /// changed what an heir types or where they look.
    #[test]
    fn reread7_done_checks_the_kit_and_has_a_place_for_a_damaged_unit() {
        let g = guide17();
        let bx = box_section(&g);
        for needle in [
            "With a Heir Kit: every unit the Heir Kit's catalogue lists on a tape whose status is `sealed` (\"Which tapes hold which units\", in \"The Heir Kit's catalogue\") is restored at the newest version the kit names, or you have written down that it is held only on tapes you do not have",
            "A unit whose every copy in the box is damaged counts once it is restored as far as its copies allow",
            "its `.tapectl-damaged-files.txt` is kept beside it, and you have written down that the files on that list are lost",
            "|tapectl_version) =",
        ] {
            assert!(bx.contains(needle), "{needle:?}: {bx}");
        }
        let start = flat_text(guide_section(&g, "Start here"));
        assert!(
            start.contains(
                "With `age` installed (step 2), check it against the cover sheet's `age1...` line"
            ),
            "{start}"
        );
        // 67 since the fresh read of a span (#478 m4); the history is in
        // reread2_s2_start_here_comes_first_and_names_sections_that_exist.
        assert!(guide_section(&g, "Start here").lines().count() <= 67);
        let kit = flat_text(guide_section(&g, "The Heir Kit's catalogue"));
        assert!(
            kit.contains("SELECT u.name AS unit, u.uuid, s.version"),
            "{kit}"
        );
        assert!(kit.contains("match a unit by its uuid"), "{kit}");
        let check = flat_text(guide_section(&g, "Did the restore get everything?"));
        for needle in [
            "(a tape of one unit puts that unit's files in the destination itself, and that is the unit's directory)",
            "A unit stored without its dar catalogue has no listing",
            "`--fill-from` restores only the files on its list, and a `--swap-tapes` restore where you typed `done` leaves out the files of the missing slices",
        ] {
            assert!(check.contains(needle), "{needle:?}: {check}");
        }
        let verdict = flat_text(guide_section(&g, "If the verdict is UNSEALED or DAMAGED"));
        assert!(
            verdict.contains("Restore each unit after it in a run of its own, `--unit UNIT --to /destination/UNIT`"),
            "{verdict}"
        );
        let words = flat_text(guide_section(&g, "Which file is your key"));
        assert!(
            words.contains(
                "After `AGE-SECRET-KEY-1` the letters B, I, O and the digit 1 never occur"
            ),
            "{words}"
        );
        let itself = flat_text(guide_section(&g, "tapectl itself"));
        for needle in [
            "under `sudo`, which drops the variable, give `--home DIR`",
            "`age-keygen -y YOUR_KEY.age.key` prints it",
            "run as root, under `sudo`, it refuses without one",
            "from a USB stick formatted FAT or exFAT, or mounted `noexec`, it answers \"Permission denied\"",
        ] {
            assert!(itself.contains(needle), "{needle:?}: {itself}");
        }
        assert!(!itself.contains("escrow.key"), "one name for the key file");
        let auto = flat_text(guide_section(&g, "Automated Recovery (recommended)"));
        assert!(
            auto.contains("a full LTO-6 tape reads in about four and a half hours"),
            "{auto}"
        );
    }
}
