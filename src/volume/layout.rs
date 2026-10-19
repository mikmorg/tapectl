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
pub fn generate_id_thunk_v2(params: &IdThunkV2Params) -> String {
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
    // Absent means unknown (§1.1): an unknown provenance emits NO line at
    // all, so a pre-#192 File 0 and a post-#192 one with nothing to say are
    // byte-identical. Rendered as a whole line here (rather than as a value
    // spliced into the template) precisely so `None` can contribute zero
    // bytes — a template with an empty value would still leave the key.
    let identity_source_line = match cartridge_identity_source {
        Some(src) => format!("cartridge_identity_source = \"{src}\"\n"),
        None => String::new(),
    };
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
magic = "tapectl-volume-v2"
label = "{label}"
uuid = "{uuid}"
layout_version = 2
requires = []
tapectl_version = "{tapectl_version}"
media_type = "{media_type}"
nominal_capacity_bytes = {nominal_capacity}
mam_capacity_bytes = {mam_capacity}
created_at = "{now}"

[layout]
front_index = 3
seal_marker = {seal_marker}
total_files = {total_files}

[media]
cartridge_manufacturer = "{mam_manufacturer}"
cartridge_serial = "{mam_serial}"
{identity_source_line}tape_length_meters = {mam_length}
load_count_at_write = {mam_loads}
"#
    )
}

/// The modes of RESTORE.sh that run external tools, as bits of
/// [`RestoreTool::modes`]. `--info` and `--verify` run the same ones, and so
/// do `--where` and `--find`: they open envelopes (the `--find-envelope` set)
/// and read plain text out of them with `awk`, which is not checked for.
const MODE_READ: u8 = 1; // --info, --verify
const MODE_ENVELOPE: u8 = 2; // --find-envelope
const MODE_LIST: u8 = 4; // --list
const MODE_PATH: u8 = 8; // --restore --path
const MODE_RESTORE: u8 = 16; // --restore
const MODE_INDEX: u8 = 32; // --where, --find

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
        role: "reading raw data from tape",
    },
    RestoreTool {
        name: "age",
        modes: DECRYPTING,
        role: "decryption (age-encryption.org). The encrypted files are in the age v1 format, which is stable and which the `age` program and `rage` both read; RESTORE.sh runs a program named `age`, so with only `rage` link it under that name",
    },
    RestoreTool {
        name: "dar",
        modes: MODE_LIST | MODE_PATH | MODE_RESTORE,
        role: "archive extraction (dar.linux.free.fr)",
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
        role: "unpacking your decrypted envelope (it is a tar archive)",
    },
    RestoreTool {
        name: "mkfifo",
        modes: MODE_RESTORE,
        role: "the named pipes a restore streams each slice through into dar (coreutils)",
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
    let which = match readers {
        Some((g, drives)) => format!(
            "This tape is {} (File 0 says so: media_type). A drive reads it only if it \
             is {}: a newer drive often cannot read an older tape (an LTO-8 drive, for \
             example, reads only LTO-7, LTO-7 Type M and LTO-8).",
            g.as_str(),
            drive_phrase(&drives)
        ),
        None => "This tape's media generation is in File 0 (media_type). It decides which \
                 drives can read it: a newer drive often cannot read an older tape (an \
                 LTO-8 drive, for example, reads only LTO-7, LTO-7 Type M and LTO-8), so \
                 look the generation up in the maker's compatibility table."
            .to_string(),
    };
    let interface = "The drive connects by SAS (some models by Fibre Channel), so the \
                     computer needs a matching host adapter (HBA); there is no USB or SATA \
                     tape drive. Fit the pieces together before the day you need them:";
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
        "As a rule do not use a USB or Thunderbolt enclosure or bridge. A tape \
         drive needs a steady stream of data and an operating system that sees \
         it as an ordinary SCSI tape device (/dev/nst0 and the like); a bridge \
         that does not is no use here. A PCIe SAS adapter in a desktop or \
         server is the dependable way.",
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
        "## The tape drive\n\n{}\n\n{}\n\n{}\n\n{}\n\n{}\n",
        wrap_words(&which, 72, ""),
        guide_box_drives(),
        wrap_words(interface, 72, ""),
        bullets.join("\n"),
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
                 first. A generation not in the table: the maker's compatibility \
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
            "As a ceiling: this tape is {}, which holds {} of data as the makers \
             count it (decimal, uncompressed), so everything on it takes at most \
             that on the tape. Restored, its files take about as much room as they \
             took on the tape, and more for a unit that was stored compressed \
             (\"Compressed units\", below).",
            g.as_str(),
            tb(g.native_capacity_bytes())
        ),
        None => "As a ceiling: a tape holds at most its generation's native capacity \
                 (LTO-6 2.5 TB, LTO-7 6 TB, LTO-8 12 TB, LTO-9 18 TB, decimal), so \
                 everything on it takes at most that on the tape. Restored, its files \
                 take about as much room as they took on the tape, and more for a \
                 unit that was stored compressed (\"Compressed units\", below). The \
                 generation is printed on the cartridge."
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
         another.",
        "A bare internal drive needs power from a power supply and the internal SAS \
         cable that matches the adapter's connector; an adapter with external ports \
         needs an external drive or enclosure on the other end of an external \
         cable. A PCIe SAS adapter needs a free PCIe slot, so a laptop or a small \
         PC without one cannot use it: borrow a desktop or a server.",
        "An adapter sold with LSI or Broadcom RAID firmware must run the plain \
         \"IT\" (initiator-target, pass-through) firmware, not the RAID one, or the \
         drive does not appear as a tape drive. To see what a card is: `lspci -k` \
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
         log in again; or run the commands with `sudo`).",
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

1. Check your key: "Which file is your key". A tenant's key opens that
   tenant's units only; the operator key and the escrow key open every
   tenant's.
2. Get a drive that reads this tape and a computer it connects to: "The
   tape drive" and "Loading the tape, and a drive that does not show up".
   Choose a disk with room for the restored files before you start:
   "Disk space".
3. Load the tape, find the drive's device name ("Which device?") and read
   RESTORE.sh, File 2, off the tape: the first commands of "Automated
   Recovery (recommended)". Set `TAPE_DEVICE` to the same device once, so
   RESTORE.sh reads the drive your `mt` and `dd` commands read.
4. Run `./RESTORE.sh --info` on EVERY tape you hold, one after another. It
   needs no key and shows each tape's label, when it was sealed and its
   `Verdict:`; for anything but SEALED see "If the verdict is UNSEALED or
   DAMAGED". With more than one tape, "The rest of the box" says what the
   others are.
5. Start from the NEWEST tape (the latest "Sealed at"). `./RESTORE.sh
   --find-envelope --key YOUR_KEY.age.key` lists the units on it, and
   `./RESTORE.sh --where UNIT --key YOUR_KEY.age.key` shows, for a unit,
   every version and every tape that holds it.
6. Restore. From one tape with the escrow or operator key, `--all` restores
   every unit on it ("Automated Recovery (recommended)"). From several
   tapes, restore each unit's NEWEST version once, from a tape that holds
   it, into a directory of its own (`/destination/UNIT`), with that tape's
   own RESTORE.sh, and never an older version first ("The rest of the
   box").
7. Check each restore: "Did the restore get everything?". A slice or a file
   that is damaged: "If a slice or a file is damaged".
8. Know when you are done: the units the newest index lists are all
   restored, and every tape in the box has been looked at. That is all a
   box can show you about itself; "The rest of the box" says what it cannot.

RESTORE.sh or its tools are not available: "Manual Recovery Steps" does the
same job by hand. The tape's map is lost: "If All Else Fails".

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
  features a reader must understand to read the tape correctly. It is empty
  (`requires = []`) on the tapes written so far (older tapes have no such
  line), which means nothing special is needed. A RESTORE.sh that meets a feature it does not know
  refuses by name and points to the RESTORE.sh stored on that tape, or a
  newer tapectl.
- **Unit**: one archived folder tree, the thing you restore by name (for
  example `photos/2019`). A unit can be stored in several versions, one
  each time it was archived again after its contents had changed. Unless
  told otherwise a restore takes the newest version on the tape;
  `--version N` (with `--restore --unit UNIT`, or with `--list`) takes
  version N instead, and `--find-envelope` shows which versions this tape
  holds. Every version is a FULL archive of the unit as it was: the newest
  version alone holds every file, and an older one is never needed to
  complete it. A version is also called a snapshot: MANIFEST.toml states it
  as `snapshot_version`, RECOVERY.md as "snapshot v2" and RESTORE.sh as
  "snapshot version". Versions are numbered 1, 2, 3 ... for each unit in
  the order they were made, so the highest number is the newest. A unit is
  identified by its `uuid`: it can be renamed, and tapes from different
  times or installations can use one name for different units, so version
  numbers compare only between archives with the same uuid.
- **Tenant**: an owner of units. One tape can carry several tenants'
  units; each tenant has its own key, and one tenant's key cannot read
  another's data.
- **Operator**: whoever runs the archive. The operator's key opens every
  tenant's data on every tape that installation wrote. So does the
  **escrow key**, a permanent spare made when the archive was set up and
  kept apart from the rest. Whichever of the three you hold, it is "your
  key" in this guide.
- **Installation**: one tapectl setup, with its own catalogue and its own
  keys (an operator key, the tenants' keys and one escrow key). Every tape
  an installation writes is encrypted to that installation's keys, so a
  tape from another installation needs that installation's keys.
- **Heir Kit, cover sheet**: the papers and files the operator prepares
  for the person who inherits the archive and keeps with the tapes (or
  apart from them): a cover sheet (`COVER.txt`, also as a printable page)
  that names the escrow key's public half, where the tapes are kept and
  which drives read them, and `catalog.db.age`, the whole catalogue
  encrypted to the escrow key. It records the archive as of the day it was
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
  `catalogs/`, and on a tape written by tapectl 1.3.0 or later `index/`
  (see "The rest of the box").
- **Slice**: a unit is archived by dar in numbered pieces called slices.
  Each slice is encrypted on its own and stored as one tape file, and one
  unit's slices lie together on the tape. Restoring a unit means reading
  all of its slices, in order.
- **dar catalogue**: a small file for each version of a unit, kept in the
  envelope under `catalogs/`. It lists every file of the unit and which
  slice holds it, so `dar -l` can list the unit, and one file can be
  restored, without reading the unit's slices. Its NAME in `catalogs/NAME`
  is the first eight characters of the unit's uuid, then `_v` and the
  version number: version 2 of a unit whose uuid starts `11111111-` is
  `catalogs/11111111_v2.1.dar`, and dar is given the name without `.1.dar`
  (`dar -l catalogs/11111111_v2 -N -Q`). MANIFEST.toml states each unit's
  uuid and version. A catalogue's listing marks every entry `[InRef]`,
  dar's word for "the data is in the archive this catalogue was cut from",
  which here means the unit's slices on the tape. It does not mean the unit
  is a difference from another version.

## Which file is your key

An age identity file is a text file with a line `AGE-SECRET-KEY-1...`
(74 characters, in capitals; a file made by `age-keygen` also carries
`#` comment lines, which do no harm). That line is the SECRET half, and the
only thing that decrypts. A line `age1...` is the PUBLIC half (a recipient): it names who a
file was encrypted for and decrypts nothing, so a file holding only that is
not a key. If you copied a secret by hand, check the copy: type the line
alone into a file and run `age-keygen -y YOUR_KEY.age.key`; it prints the
public `age1...` that goes with it, which must equal the one on the cover
sheet or record you have. A character miscopied is refused with "invalid
checksum".

If the key file was itself encrypted with a passphrase (it starts with
`age-encryption.org/v1`, or `-----BEGIN AGE ENCRYPTED FILE-----`), age
asks for the passphrase every time it opens the file, which for a unit of
many slices is every slice. Decrypt it once, `age -d -o YOUR_KEY.age.key
PROTECTED_FILE`, work with the result, and delete it afterwards.

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
  if File 3 is damaged)

{drive_section}
{drive_basics}
## Tools Required

{tools_required}
- `bash`, `awk` and the basic coreutils every Linux system has (`sed`,
  `grep`, `tr`, `cut`, `sort`, `stat`, `df`, `wc`, `tail`, `od`,
  `mktemp`): RESTORE.sh is a bash script, and the manual steps below
  use them
- `gpg` (package gnupg) — only if your `dar` was built with GPG support,
  as Debian's and Ubuntu's is: such a dar refuses to start without it
  ("INITIALIZATION FAILED FOR GPGME"). RESTORE.sh checks and says so.
- dar 2.7.9 or later streams a restore with no disk space for slices;
  an older one, or a unit without its catalogue on a dar before 2.7.21,
  is restored by decrypting the slices to disk first (RESTORE.sh
  chooses, and checks the space either way). `dar --version` says which
  dar you have. A unit's catalogue is the small file `catalogs/NAME` in
  its envelope ("Words this guide uses"); "without its catalogue" means
  the envelope has none for that unit, or dar refuses the one it has
  because it was cut from another run of the same snapshot. RESTORE.sh
  says so when that happens.
- Only for finding the drive and checking a key, never for the restore
  itself: `ls`, `lsscsi`, `lspci`, `dmesg`, `lsmod`, `modprobe`, `sudo` and
  `usermod` (to find the drive, load its driver and get permission to use
  it: "Loading the tape, and a drive that does not show up"), and
  `age-keygen`, which comes with `age` ("Which file is your key"). Each is
  named where this guide uses it.

RESTORE.sh checks for the tools each mode runs, before it reads the tape,
and names any that is missing: `--info` and `--verify` need only {read_only}.

## Disk space

`/destination` in this guide is a placeholder for an empty directory (or
one that does not exist yet: RESTORE.sh makes it) on a disk with room.
Choose it before you start.

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
there, but a unit of millions of files has a large catalogue: if `/tmp`
is small, set `TMPDIR` to a directory on a real disk with some GiB free
(`TMPDIR=/big/disk/tmp ./RESTORE.sh ...`).

Some ways of restoring keep the slices on disk as well, in a scratch
directory inside `--to` unless `--scratch DIR` names another disk, and
then need more room, up to about TWICE a unit's size (its slices, then
its restored files): the older-dar fallback above (all of the unit's
slices), `--path` (only the few slices that hold the path), and asking
for a replacement slice (`--slices-from`, `--swap-tapes`: "If a slice or
a file is damaged"). Doing it by hand ("Manual Recovery Steps") is the
same: the decrypted slices and the restored files, about twice the unit's
size; delete a unit's slices once it is restored. The script checks the
space before it reads any slice.

For a whole box, the destination needs the sum of the sizes of the units
you restore, whichever tapes they come from; the extra room for slices is
needed for one unit at a time.

**Compressed units.** A unit restores to more than its slices' size if it
was stored compressed, by an amount that depends on the files (text
compresses a lot, photos and video hardly at all). MANIFEST.toml records
the dar command of each unit, in its `dar_command` line (unpacked
envelope: `grep '^dar_command' env/MANIFEST.toml`; `--find-envelope` with
a tenant's key prints the whole MANIFEST.toml). An argument that starts
`-z`, as in `\"-zgzip\"` or `\"-zzstd\"`, means the unit was compressed,
and none means it was not. The dar catalogue cannot tell you: `dar -l`
of the catalogue in the envelope shows `-----` in its compression column
either way. With no `dar_command` line, allow generously.

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
    dd if=/dev/nst0 bs=512k | tr -d '\0' > RESTORE.sh
    chmod +x RESTORE.sh

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

    # Which other tapes hold a unit, or a file ("The rest of the box")
    ./RESTORE.sh --where UNIT --key YOUR_KEY.age.key
    ./RESTORE.sh --find some/file --key YOUR_KEY.age.key

    # Full restore of THIS tape to an EMPTY directory: --all restores
    # every unit your key opens, each into /destination/UNIT. Each slice
    # streams from the tape through age into dar, so /destination needs
    # about those units' size free; the script checks first. --overwrite
    # finishes a restore that stopped part way, never one over an older
    # version. With other tapes in the box, read "The rest of the box"
    # before this: a second --all into the same /destination is refused.
    ./RESTORE.sh --restore --key YOUR_KEY.age.key --all --to /destination

    # Just one file or folder: reads only the slices that hold it
    ./RESTORE.sh --restore --key YOUR_KEY.age.key --unit UNIT \
        --path some/file --to /destination

With the ESCROW key or the OPERATOR key, which opens every tenant's
envelope (`YOUR_KEY.age.key` is then that key's file), list what the tape
holds first, then restore all of it:

    ./RESTORE.sh --list --key YOUR_KEY.age.key
    ./RESTORE.sh --restore --key YOUR_KEY.age.key --all --to /destination

--all restores every unit of the envelope that lists the most units among
those your key opens, each in /destination/UNIT (a single unit goes
straight into /destination): with the escrow or operator key that is the
operator envelope, which lists every unit on the tape; with a tenant's key
it is that tenant's envelope. Without --all or
--unit, --restore takes the first envelope your key opens and, if that
lists one unit, restores only that unit (it tells you what else the key
opened). If that envelope lists SEVERAL units it will not guess: it
prints them and stops before it reads a slice, and you name one with
`--unit UNIT` (repeat `--unit` for several, each into /destination/UNIT)
or restore them all with `--all`. A
single TENANT's key opens only that tenant's envelope: it restores that
tenant's units and nobody else's.

The script reads the tape forward from one rewind, and says what it
needs if it meets a tape newer than itself (`requires` in File 0).

## The rest of the box: other tapes, copies and the index

If you hold more than this one tape, work out first what the others are.
A tape may be a COPY of this one (the same units written again on another
cartridge, so that one can be lost), or hold OTHER units, or hold other
VERSIONS of a unit this tape holds: a unit's older versions need not be
on the same tape as its newest one.

A tape written by tapectl 1.3.0 or later carries an index inside its
envelopes that answers this. For each unit it lists every version and the
tapes that hold it (label, uuid, cartridge serial, generation, state and
when it sealed), and a tenant's envelope also lists every file path.
RESTORE.sh reads it:

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
every unit the archive's catalogue knew when its tape was written,
including units held only on OTHER tapes (`units.tsv` has a row for each
tape that holds each version, and `--where UNIT` prints those rows). List
the units the newest index has and the tapes that hold them, and set that
against the tapes in the box: a holder the index names that you do not have
is a tape to look for, and a unit, or the newest version of a unit, whose
every holder is missing is not in this box. To see every unit the index
lists, ask `--where` for a name it does not have: it answers that the name
is not in the index and prints the units it does list (the first 40).
Then ask whoever kept the archive. The Heir Kit, if you have one ("Words
this guide uses"), records the archive as of the day it was made, including
where the tapes are kept; one made after your newest tape was written may
name tapes this box lacks.

The whole box, in short:

1. Name every cartridge. Load each in turn and run `./RESTORE.sh --info`
   (any copy of RESTORE.sh will do for this: see the end of this list):
   its label is "Tape identifies as" (File 0's label is the authority; a
   sticker on the case can have been replaced) and its sealed date is
   "Sealed at". The generation is printed on the cartridge (L6 is LTO-6)
   and stated in File 0 as `media_type`; tapes of different generations
   need different drives ("The tape drive" has a table, so that you can
   choose one drive for the box).
2. Start from the newest tape. List its units with `./RESTORE.sh
   --find-envelope --key YOUR_KEY.age.key`, then run `./RESTORE.sh --where
   UNIT --key YOUR_KEY.age.key` for each unit you want. `--where` prints a
   "Version N" for each version of the unit, with the tapes that hold it;
   the highest N is the newest.
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
6. You are done when every unit the newest index lists is restored at its
   newest version (or the best you could reach) and every tape in the box
   has been looked at. That is the most a box can show about itself:
   "Is the box complete?" above says what it cannot.

The modes that only read the tape (`--info`, `--verify`, `--find-envelope`,
`--list`, `--where`, `--find`) describe the tape that is in the drive,
whichever tape's RESTORE.sh runs them. A RESTORE.sh written for another
tape prints a "WRONG TAPE?" box first, and for these modes it is only a
reminder that the script was written for another volume: what follows is
about the tape in the drive (the `Tape identifies as:` line, which every mode
prints, is that tape's own label; the heading `--verify` prints names the
script's volume, but it checks the tape in the drive). For a
restore use the loaded tape's own RESTORE.sh, which was written for that
tape's format, and take "WRONG TAPE?" during a restore as a sign that the
script or the tape is the wrong one. To get a tape's own script, load that
tape and run the four commands at the start of "Automated Recovery
(recommended)"; they read File 2 of whatever tape is in the drive. Keep
each tape's script under its own name.

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
    dd if=/dev/nst0 bs=512k | tr -d '\0' | grep -E '^(label|uuid|media_type|cartridge_serial) ='

To take stock of a box without relying on the index, load each tape in
turn, run `./RESTORE.sh --info` (its label and when it was sealed) and
`./RESTORE.sh --find-envelope --key YOUR_KEY.age.key` (its units, their
versions, slices and sizes), and write them down. A unit that appears at
the same version AND with the same unit uuid on two tapes has two copies:
they are the same content, and either restores it. A name and a version
number alone prove nothing; the uuid is in MANIFEST.toml (`--find-envelope`
with a tenant's key prints it) and, on a tape with an index, in `--where`.
Cartridges are found by their label; the cartridge's chip serial is in
File 0 (`cartridge_serial`) and in the index, and a sticker can have been
replaced.

## If the verdict is UNSEALED or DAMAGED

`./RESTORE.sh --info` prints a `Verdict:` line, a verdict on the tape as a
whole:

- SEALED: the seal marker (the last file) is there and agrees with the
  front index. The write finished and the two ends of the tape match.
  Carry on.
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
- If every file PASSes against the front index and only the seal
  disagrees, the data is exactly what that index describes (each file
  matches its recorded hash): the fault is in the seal marker or the
  index, it does not matter which for restoring, and restoring is sound.
- A unit with a FAIL slice cannot be restored whole from this tape.
  RESTORE.sh stops at the first slice that fails its check and says which.
  Look for another copy first ("The rest of the box"; "If a slice or a
  file is damaged" says how RESTORE.sh takes the failed slice from it); a
  cartridge that failed here may read in another drive. With no other copy,
  the slices that did PASS are intact, and the files that lie wholly
  inside them can be restored by hand: "If a slice or a file is damaged"
  has the commands under "No other copy". The files in the failed slices
  are lost.

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
3. The count matches the catalogue. The inodes (files and directories) a
   unit's catalogue lists are the lines of `--list` that begin with a status
   such as `[Saved]` or `[InRef]`:

       ./RESTORE.sh --list --key YOUR_KEY.age.key --unit UNIT \
           | grep -c -E '^\[.{{5}}\]\[.\] '

   That number should equal the "inode(s) restored" figure of a whole-unit
   restore. For a few files, compare their sizes and a checksum with any
   record of your own.

Two restores print a different summary. With `--all`, or `--unit` given
more than once, dar runs once for each unit and prints one summary for
each: check every one, and compare each against that unit's `--list`. With
`--path`, dar restores only what you asked for, plus the directories above
it (a file `a/b/c.txt` brings `a` and `a/b` with it, so 3 inodes), or
everything under a directory you asked for; "inode(s) ignored (excluded by
filters)" then counts every other inode of the unit, which is expected and
not a failure. What to check there is that "failed to restore" and "not
restored" are 0, and that "restored" is the number of inodes you asked
for, the path and the directories above it.

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
        --slices-from DIR --to /destination

    # Or let the script ask for the other cartridge once it has read this
    # one (it says which slices it needs, by number and sha256, and waits
    # for ENTER; type done to go without)
    ./RESTORE.sh --restore --key YOUR_KEY.age.key --unit UNIT \
        --swap-tapes --to /destination

`--verify` takes the same two options and says whether a copy of each failed
slice was found. To copy a slice off the other tape by hand, find its tape
file in THAT tape's front index (File 3) by its sha256, then:

    mt -f /dev/nst0 rewind
    mt -f /dev/nst0 fsf POSITION
    dd if=/dev/nst0 bs=512k of=slice-copy.bin

Either option restores slice by slice onto disk, as with an older dar, so
`--to` needs about twice the unit's size (`--scratch DIR` puts the slices on
another disk). Each tape is read forward from one rewind. A slice RESTORE.sh
cannot replace is left out: the files in it are not restored but listed,
below. RESTORE.sh runs the `age` program, which stops at the first damaged
part of a slice, so it cannot save the good parts of a slice; tapectl's own
restore can ("No other copy", below).

**A file fails.** When dar names files it could not restore (a checksum that
failed, a slice that was not there), RESTORE.sh restores the rest, ends
`RESTORE INCOMPLETE` with exit status 1, and writes the paths of those files,
one per line, to `.tapectl-damaged-files.txt` in the unit's directory (`--to`
for one unit, `--to/UNIT` for several). A path is written as the index writes
one: a backslash as `\\`, a TAB as `\t`, a newline as `\n`, a carriage
return as `\r`. The first line records the unit, its uuid and its version. Load a tape that holds the same
unit at the same version and run ITS RESTORE.sh:

    ./RESTORE.sh --restore --key YOUR_KEY.age.key --unit UNIT \
        --fill-from /destination/.tapectl-damaged-files.txt --to /destination

It reads only the slices that hold those files, restores exactly them, over
their damaged copies, and nothing else in the directory, and dar checks each
file it restores. The list is then rewritten with whatever is still damaged
(from yet another copy, run it again) and removed when nothing is. Only a
tape whose envelope lists the same unit, with the same uuid, at the same
version is used; any other is refused by name, because another version can
differ in the very files that are damaged.

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
those after it are not named at all, so compare with the catalogue's list.

Neither way can read around a damaged part of a slice: the `age` program
stops at the first damaged piece. tapectl can. tapectl is the program that wrote this tape (File 0 names its release
in `tapectl_version`); it is open-source software, and its own restore
reads around a damaged 64 KiB piece of a slice, loses only the files in that
piece and names them. To use it you need a copy of tapectl and a catalogue
rebuilt from the tape with the operator or escrow key (`tapectl catalog
rebuild --from-volume`); nothing else in this guide needs it.

## Manual Recovery Steps

If RESTORE.sh is not available, follow these steps with `mt`, `dd`, `age`,
`tar`, `sha256sum` and `dar`. They use the non-rewinding device ("Which
device?"; `/dev/nst0` is an example). Each `dd` leaves the head at the
start of the next file, so `mt fsf N` moves forward N files from where you
are, and `mt rewind` then `mt fsf N` reaches File N from anywhere. Read the
tape forward where you can; rewinding is slow.

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
   numbered 0 to {last_file}, so the last is File {last_file}:

       mt -f /dev/nst0 eod
       mt -f /dev/nst0 bsfm 2
       dd if=/dev/nst0 bs=512k | tr -d '\0' > seal.toml

   `eod` (GNU mt calls it `eom`) moves to the end of the recorded data,
   which is just past the last file's closing filemark. `bsfm 2` backs over
   that filemark and the one before it and then steps forward over that
   one, which leaves the head at the START of the last file. (Without
   `eod`, `mt -f /dev/nst0 rewind` then `mt -f /dev/nst0 fsf {last_file}`
   reaches the same place.) Check that `front_index_sha256` in seal.toml
   equals the sha256sum printed in step 3, and that its `file_count` is
   the number of entries in the front index. If the seal marker is
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
   restore the units of each). For each envelope you try, with POSITION and
   SIZE from its entry:

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
   newest, unless you want an older one. Put the tape at the first slice,
   once. `fsf N` moves forward N files from where the head is, so from the
   start of the tape it reaches File N: the TAPE_POSITION of the first
   slice, from the manifest.

       mt -f /dev/nst0 rewind
       mt -f /dev/nst0 fsf FIRST_POSITION

   Then, for each slice of the unit in order of its number, with
   ENCRYPTED_BYTES, SHA256_ENCRYPTED and NUMBER from the manifest (each read
   leaves the tape at the start of the next file, so the next slice needs no
   spacing unless there is a gap):

       dd if=/dev/nst0 bs=512k of=restore.NUMBER.dar.age
       truncate -s ENCRYPTED_BYTES restore.NUMBER.dar.age
       echo "SHA256_ENCRYPTED  restore.NUMBER.dar.age" | sha256sum -c -
       age -d -i YOUR_KEY.age.key restore.NUMBER.dar.age > restore.NUMBER.dar
       rm restore.NUMBER.dar.age

   If a gap lies between two of the unit's slices (another unit's slices,
   say), space over it first. `fsf` takes the number of files to skip, not a
   file number: after reading the slice in File P the head is at the start of
   File P+1, so to reach the slice in File Q, run

       mt -f /dev/nst0 fsf COUNT           # COUNT = Q - P - 1

   (from File 6 to File 9 that is `fsf 2`). Rewinding and `fsf Q`, as above,
   reaches it too and is slower. A slice that fails its `sha256sum -c` is
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
   unit's decrypted slices at once (about the unit's size, and as much
   again for step 8's restored files: about twice the unit's size in all),
   on a real disk and not in /tmp, which is a small RAM disk on many systems
   ("Disk space"). RESTORE.sh keeps only the envelopes there, never a slice.
8. Reassemble dar slices into an EMPTY directory, which dar needs to exist
   (`mkdir /destination`): `dar -x restore -R /destination -O -Q -N`. With
   several units, give each its own directory (`/destination/UNIT`). dar
   keeps any file already there and still reports success, so to finish an
   earlier restore of the same unit that stopped part way, add `-w`
   (overwrite); never use it to restore a newer version over an older one.
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
also lists `PLAN.toml` (a tenant's never does), and whose MANIFEST.toml
lists every unit on the tape; or restore the units of every envelope that
opens. If the files have grown large (slices) and none
opened, your key is not a recipient of any envelope on this tape: try the
other keys you hold, since an envelope is sealed to the keys active when
the tape was written, and the operator and escrow keys open every one.

1. Rewind and space to File 4 (`mt -f /dev/nst0 rewind`, then
   `mt -f /dev/nst0 fsf 4`), then read candidate files one by one with
   `dd if=/dev/nst0 bs=512k of=candidate.raw`. Each read leaves the tape
   at the start of the next file, so read again without spacing; an
   extra `fsf 1` between reads would skip a file.
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

## Total files on this tape: {total_files}

The files are numbered 0 to {last_file}. File {last_file}, the last, is
the seal marker.
"#
    )
}

/// Generate RESTORE.sh v2 (File 2) — self-contained emergency restore +
/// verify script for layout v2. Four modes per sheet §10:
/// - `--info`: read File 0 + File 3 (front index) + the last file (seal
///   marker); print the layout table and the exact §2.5 verdict (SEALED /
///   UNSEALED / DAMAGED (ends disagree)) — never trusts the seal marker's
///   mere presence (§2.6): the verdict always re-derives from the hash chain.
/// - `--verify`: keyless integrity walk — every file's on-tape bytes (after
///   trimming to File 3's `size_bytes`) hashed and compared to File 3's
///   `sha256_encrypted`; File 3 itself checked against the seal binding.
///   Per-file PASS/FAIL lines, nonzero exit on any FAIL.
/// - `--find-envelope --key K [--key K2 ...]`: trial-decrypt envelope
///   positions (found by type in the file map), as v1.
/// - `--restore --key K [--key K2 ...] --to DIR [--unit U] [--version N]`: slice positions/sizes come from
///   the decrypted MANIFEST, cross-checked against the file map's
///   `size_bytes`/`sha256_encrypted` before each slice is trusted/decrypted.
///
/// Every mode that needs a file map applies the degradation ladder (sheet
/// §3.4): rung 1 is File 3 itself; rung 2 (if File 3 fails to parse or fails
/// its self-consistency check, §2.5) falls back to the seal marker's
/// embedded `[[files]]` copy, loudly warned; rung 3 (both gone) prints the
/// manual zero-strip procedure (§3.3) and exits — there is nothing left to
/// automate. Parsing stays line-oriented (`grep`/`awk`/`sed` over
/// `key = value` lines, the §3.1 grammar contract); every value read from a
/// plaintext tape metadata zone goes through `require_uint` before it
/// reaches arithmetic, `fsf`, or `seq` (S2 hardening, carried over from v1).
/// Tools used: mt, and the ones in `RESTORE_TOOLS` (each mode checks only
/// those it runs), plus standard coreutils (awk/sed/grep/tr) — no TOML
/// collection, per the grammar contract.
/// **`--key` is repeatable (issue #288, CTO ruling 2026-09-22).** A volume's
/// tenant envelope is sealed at WRITE time to the tenant's then-current public
/// keys (`build.rs`), while its data slices were sealed at STAGE time
/// (`staging/mod.rs`). A `key rotate` landing between the two puts envelope
/// and slices on different key generations, and then NO single key opens both
/// — measured on a real tape. tapectl itself is unaffected because it
/// trial-decrypts with every tenant and operator key, independently for the
/// envelope and for the slices; RESTORE.sh took one `--key`, so the heir path
/// was the only one that failed. It now mirrors tapectl: repeated `--key`,
/// each tried on its own for the envelope and for each slice (remembering the
/// key that opened the previous slice, since a wrong key costs a re-read of
/// tape-sized ciphertext). Every key path is checked for existence BEFORE the
/// first tape read — `age -d -i good -i missing` fails outright rather than
/// skipping the bad one.
///
/// **CARRIED FIX — issue #218, CTO ruling 2026-09-17: done, 2026-09-22.** The
/// decrypted-slice progress line read `MB` for a 1048576-divided figure, which
/// ADR-0012 forbids (capacities decimal, data sizes binary, the two named
/// apart). #204 fixed the class across ~40 CLI sites and deliberately left
/// this one, because RESTORE.sh is on-tape content pinned by
/// `RESTORE_SH_SHA256` in `tests/on_tape_golden.rs` and the ruling was to
/// batch it onto the next substantive change rather than spend a re-pin on a
/// label. #288 is that change, so the line now reads `MiB`.
///
/// **A quoting defect went with it (issue #291).** Two of the three
/// "no envelope opened" exits quoted the tape label as `'\''VOL-A'\''` — the
/// idiom for embedding a quote inside a SINGLE-quoted string, in a string
/// that is double-quoted, where a bare `'` is already literal. Bash printed
/// the backslashes verbatim, so the one line telling a reader in a disaster
/// whether they hold the wrong cartridge rendered as garbage. The third exit
/// (the `--unit` one, which is the ordinary way to reach it) carried no
/// context at all. All three now route through one `die_no_envelope` helper
/// so they cannot drift apart again.
///
/// This note lives on the FUNCTION, not inside the template: a comment added
/// within the string becomes shell comment lines in the generated script and
/// changes its hash. I did exactly that first, and the golden test caught it —
/// which is the test working as designed.
pub fn generate_restore_script_v2(label: &str, total_files: i32) -> String {
    use crate::volume::restore_script::{
        AWK_CHECK_FILE_LIST, AWK_DAR_DAMAGED, AWK_FILE_CHECK_FAILED, AWK_FILE_CHECK_LIST,
        AWK_FILE_CHECK_LISTED, AWK_FILL_LISTING, AWK_FIND_ENVELOPE, AWK_INDEX_COMMON,
        AWK_INDEX_FIND, AWK_INDEX_WHERE, AWK_MANIFEST_HAS_UNIT, AWK_MT_POSITION,
        AWK_PARSE_FILE_LIST, AWK_SELECT_VERSION, AWK_UNITS_TABLE, AWK_UNIT_LIST, AWK_UNIT_UUID,
    };

    let script = r#"#!/usr/bin/env bash
# RESTORE.sh — Emergency restore script for tapectl volume __LABEL__ (layout v2)
# This script restores data from this tape WITHOUT tapectl installed.
# It reads the tape's front index (File 3), the seal marker (last file),
# finds your encrypted envelope, decrypts each data slice, and extracts
# the dar archive to a directory.
#
# Usage:
#   ./RESTORE.sh --info                                       Show tape layout + seal verdict
#   ./RESTORE.sh --verify                                     Keyless integrity check (no key needed)
#   ./RESTORE.sh --find-envelope --key KEYFILE [--key K2 ...] Decrypt your envelope
#   ./RESTORE.sh --restore --key KEYFILE [--key K2 ...] --to DIR
#                [--unit U [--unit U2 ...] | --all] [--version N]
#                [--no-space-check] [--overwrite] [--no-file-check] [--scratch DIR]
#   ./RESTORE.sh --restore --key KEYFILE --to DIR --unit U --path P [--path P2 ...]
#                [--version N] [--scratch DIR]                One file or folder
#   ./RESTORE.sh --list --key KEYFILE [--unit U] [--version N]  A unit's files
#   ./RESTORE.sh --restore ... [--slices-from DIR ...] [--swap-tapes]   A bad slice from another copy
#   ./RESTORE.sh --restore --key KEYFILE --to DIR --unit U --fill-from LIST   Files a restore could not restore
#   ./RESTORE.sh --verify [--slices-from DIR ...] [--swap-tapes]
#   ./RESTORE.sh --where UNIT --key KEYFILE [--key K2 ...]       Which tapes hold UNIT
#   ./RESTORE.sh --find TEXT --key KEYFILE [--key K2 ...]        Which unit and tape hold a path
#
# --list and --path read the dar catalogue each envelope carries
# (catalogs/<uuid8>_v<version>.1.dar): --list reads no data slice at all, and
# --path reads only the slices that hold what it asks for, plus the last.
# --to must be empty or new: dar keeps a file that is already there and
# reports success, so a restore over an earlier one cannot be trusted.
# --overwrite replaces such files instead, to finish a restore that stopped.
# With more than one unit (--all, or --unit repeated) each unit goes to its
# own directory, --to/UNIT.
#
# --where and --find read the cross-volume index that a tape written since
# version 1.3.0 carries in its envelopes (index/units.tsv, and index/paths.tsv
# in each tenant envelope): --where lists every version of a unit and the tapes
# holding it, --find lists the paths that contain a piece of text and the tapes
# holding them. An index is as of the moment its own tape was written, so the
# newest tape you hold has the most complete one. They read this tape only,
# forward from one rewind, like every other mode.
#
# --key may be repeated. An envelope and the slices it describes can need
# different keys after a key rotation, so every key is tried independently.
#
# A slice that fails its hash, or will not read, can be taken from another copy
# of the unit (ADR-0012 item 101). The copy's slice has the same sha256 in its
# front index, and that is the only way one is matched: --slices-from DIR names
# a directory of slice files copied off another tape (any names), and
# --swap-tapes asks for another cartridge after this one has been read. Either
# one restores the way an older dar does, slice by slice onto disk (about twice
# the unit's size), because a stream cannot take a slice back from dar.
#
# A restore whose dar run names files it could not restore (a failed CRC, a
# damaged slice) writes their paths to .tapectl-damaged-files.txt in the unit's
# directory (ADR-0012 item 102) and ends RESTORE INCOMPLETE. Another tape that
# holds the same unit at the same version fills exactly those files:
#   ./RESTORE.sh --restore --key KEYFILE --unit U --fill-from LIST --to DIR
#
# dar checks each file it restores with a short checksum, not a hash, so a
# damaged file can come back with no error. A tape written since version 1.3.0
# records the sha256 of every regular file in its index, and after each restore
# and each fill this script reads the restored files back and checks them
# against it with sha256sum (ADR-0012 item 103). A file that does not match, or
# is missing, goes on the damaged-file list like one dar named, and the run ends
# RESTORE INCOMPLETE. That costs one read of the restored files from disk, not
# a tape read; --no-file-check skips it. A tape written before 1.3.0 has no
# index to check against, and the script says so.
#
# Disk space: --restore streams each slice from the tape through age into dar,
# so nothing decrypted is written to disk but the restored files themselves:
# --to needs about the unit's size free (counted from the slices' encrypted
# size on tape: a compressed unit may need more). The envelopes are the other
# thing on disk: they go to the work directory in ${TMPDIR:-/tmp}, decrypted,
# and TMPDIR moves it. dar is given the unit's catalogue from the envelope
# (dar 2.7.13 loses a file's tail streaming without one).
# Where streaming is not safe (no usable catalogue and a dar before 2.7.21,
# or a dar before 2.7.9) the slices are decrypted to disk first, as 1.0.x
# did, in a scratch directory inside --to or --scratch DIR. The script
# measures the space before it reads any slice.
#
# Requirements: mt (mt-st), __TOOLS_ALL__
# Each mode checks only the tools it runs (see the prerequisite check below).
# Total files on tape: __TOTAL_FILES__
#
# Degradation ladder (see the system guide, File 1, "If All Else Fails"):
#   rung 1 — File 3, the front index (normal path)
#   rung 2 — the seal marker's embedded front-index copy (if File 3 fails)
#   rung 3 — manual zero-strip recovery (if both fail; this script cannot
#            automate it, but prints the procedure)

set -euo pipefail

# This script matches the words of dar and of other tools (dar's skip line,
# the GPG start-up failure, a full disk). dar and coreutils ship translations,
# and a translated message would not match: ask for English in any locale.
export LC_ALL=C

DEVICE="${TAPE_DEVICE:-/dev/nst0}"
LABEL="__LABEL__"
BLOCK=524288 # 512 KB — tapectl fixed block size

# Every --key given on the command line, in order. A volume's envelope and the
# data slices it describes can be sealed to DIFFERENT key generations: the
# envelope is built when the volume is written, each slice when it was staged.
# A key rotation between the two leaves no single key that opens both, so each
# key here is tried independently for the envelope and for every slice.
KEYS=()

# WORK is the work directory, made in ${TMPDIR:-/tmp}: TMPDIR moves it. It
# holds the tape's text zones, the FIFOs a restore streams slices through, and
# the ENVELOPES: each is read whole into it, and each one a key opens is kept
# there decrypted (its MANIFEST.toml, the dar catalogues of its units and, in
# the operator's, catalog.db) until the run ends; --all keeps every one it
# opens. These grow with the number of files on the tape. A data slice never
# lands here — a slice is up to 10 GiB and /tmp is RAM on many systems.
# SCRATCH is for the one mode that keeps slices on disk (--path, which reads
# only a few), a directory created inside --to or --scratch once its size is
# known.
SCRATCH=""
SCRATCH_PARENT=""
SKIP_SPACE_CHECK=0
OVERWRITE=0
# --slices-from DIR (repeatable), --swap-tapes and --fill-from LIST (1.3.0).
# SALVAGE is 1 when either of the first two was given.
SLICES_FROM=()
SWAP_TAPES=0
SALVAGE=0
FILL_FROM=""

umask 077 # decrypted plaintext and temp files must not be world-readable
WORK="$(mktemp -d "${TMPDIR:-/tmp}/tapectl-restore.XXXXXX")" ||
  {
    echo "FATAL: cannot create temporary directory" >&2
    exit 1
  }
# Background jobs of a streaming restore (dar, and a slice's hash) are
# killed on any way out: a dar left waiting on a FIFO would wait forever.
BG_PIDS=()
cleanup() {
  local p
  for p in ${BG_PIDS[@]+"${BG_PIDS[@]}"}; do
    disown "$p" 2>/dev/null || true
    kill -KILL "$p" 2>/dev/null || true
  done
  rm -rf "$WORK"
  [ -z "$SCRATCH" ] || rm -rf "$SCRATCH"
}
trap cleanup EXIT

die() {
  echo "FATAL: $*" >&2
  exit 1
}
info() { echo ">>> $*"; }

# The provided keys as one line, so a failure names every key that was tried.
keys_list() {
  local k out=""
  for k in ${KEYS[@]+"${KEYS[@]}"}; do
    out="$out $k"
  done
  echo "${out# }"
}

# "the key provided" / "any of the 3 keys provided" — the count is what tells a
# reader whether they forgot to pass one.
keys_phrase() {
  if [ "${#KEYS[@]}" -eq 1 ]; then
    echo "the key provided"
  else
    echo "any of the ${#KEYS[@]} keys provided"
  fi
}

# The keys back as command-line arguments, for the hints this script prints.
keys_args() {
  local k out=""
  for k in ${KEYS[@]+"${KEYS[@]}"}; do
    out="$out --key $k"
  done
  echo "${out# }"
}

# Check every key path BEFORE the first tape read. `age -d -i good -i missing`
# fails outright rather than skipping the bad one, so a mistyped path must be
# caught here and not discovered minutes into a restore.
require_key_files() {
  local k
  for k in ${KEYS[@]+"${KEYS[@]}"}; do
    [ -n "$k" ] || die "--key needs a value"
    [ -f "$k" ] || die "key file not found: $k"
  done
}

# Reject a value read from a plaintext tape metadata zone (ID thunk, front
# index, seal marker) that is not a plain non-negative integer, BEFORE it is
# used in `$(( ))` arithmetic, `fsf`, or `seq`. Those zones are unauthenticated
# — anyone holding the tape can rewrite them without any key — so a crafted
# value such as 'a[$(cmd)]' would otherwise execute inside arithmetic
# expansion. Called in the current shell (not a subshell) so `die` halts the
# whole script. (MANIFEST.toml values are NOT re-checked here: they come from
# an age-authenticated envelope, a different trust tier — tampering there
# breaks decryption itself.)
require_uint() {
  local name=$1 val=$2
  case "$val" in
  "" | *[!0-9]*)
    die "tape layout value '$name' is not a non-negative integer: '$val' — the tape may be damaged or tampered"
    ;;
  esac
}

# Make a string read from an unauthenticated plaintext tape zone safe to ECHO.
# Same trust tier as require_uint's callers: anyone holding the tape can write
# these bytes. This one is never used in arithmetic — the danger is terminal
# control sequences (a crafted label could hide or forge output). Keep only
# printable ASCII, cap the length, and mark it if anything was dropped.
safe_str() {
  local raw=$1 clean
  clean=$(printf '%s' "$raw" | LC_ALL=C tr -cd '[:alnum:] ._:@/+-' | cut -c1-64)
  [ "$clean" = "$raw" ] || clean="$clean (sanitized)"
  printf '%s' "$clean"
}

# Non-fatal cousin of require_uint: true/false, never exits. Used in soft
# parse/consistency checks where a bad value should degrade the reader to the
# next rung of the ladder (or to an UNSEALED/DAMAGED verdict) instead of
# crashing the whole script.
is_uint() {
  case "$1" in
  "" | *[!0-9]*) return 1 ;;
  *) return 0 ;;
  esac
}

# A byte count for messages: GiB, MiB or KiB with one decimal, so a small unit
# does not read as "needs 0.0 GiB, 0.0 GiB free".
size_str() {
  awk -v b="$1" 'BEGIN {
    if (b >= 1073741824) printf "%.1f GiB", b / 1073741824
    else if (b >= 1048576) printf "%.1f MiB", b / 1048576
    else printf "%.1f KiB", b / 1024
  }'
}

# A failed write must say why. Out of disk space is the likely cause on a
# restore, and it must never read as a tape fault or a wrong key: the loop that
# tries each key used to report a full disk as "key did not decrypt".
die_io() { # <what failed> <file holding the tool's stderr> <directory written to>
  local what=$1 errf=$2 where=$3 detail
  detail=$(grep -v -e 'records in$' -e 'records out$' -e 'bytes.*copied' "$errf" 2>/dev/null | tail -n 3 || true)
  if grep -qi 'no space left' "$errf" 2>/dev/null; then
    die "$what: OUT OF DISK SPACE in $where
       ${detail}
       This is not a tape or key problem. Free space there and run the same
       command again, with --overwrite if --to now holds part of the unit.
       --restore writes only the restored files to --to (--path also keeps
       the few slices it reads in a scratch directory, inside --to unless
       --scratch names another disk); everything else goes to the work
       directory in ${TMPDIR:-/tmp}: the decrypted envelopes, which grow with
       the number of files on the tape. If that is what filled, set TMPDIR to
       a directory on a larger disk."
  fi
  die "$what
       ${detail:-(no error text)}"
}

# Free space in bytes on the filesystem holding directory $1; empty if df
# cannot say.
free_bytes() {
  df -Pk -- "$1" 2>/dev/null | awk 'NR == 2 && $4 ~ /^[0-9]+$/ { printf "%.0f\n", $4 * 1024 }'
}

die_space() { # <directory> <bytes needed> <bytes free>
  die "not enough disk space in $1 to restore:
       it needs about $(size_str "$2"), and $(size_str "$3") is free.
       No slice has been read yet. A restore streams each slice from the tape
       through age into dar, so it needs room for the restored files only,
       about the size of the unit's slices. That figure is the slices' size
       on tape, encrypted: a unit that was stored compressed restores larger,
       and a compressed unit may need more than this. --path also keeps the
       slices it reads on disk until dar has taken the files from them
       (--scratch DIR puts those on another disk).
       Free space there, or choose a larger disk with --to.
       (--no-space-check skips this check, for a filesystem that holds more than
       df reports, such as a compressed or thin-provisioned one.)"
}

# What the figures above count. dar does not say how large a unit restores
# before it runs, so the check counts the slices as they sit on tape.
space_note() {
  info "            (counted from the slices' encrypted size on tape; a compressed unit may need more)"
}

# Refuse BEFORE the first slice is read when the restore cannot fit. Running
# out of space mid-restore used to kill the script hours in, silently or as a
# key error. A streamed restore asks only for the files (scratch 0); --path
# asks for the slices it keeps in scratch as well. With both on one
# filesystem (unless stat proves otherwise) the two needs add up.
check_space() { # <destination> <scratch dir> <bytes of files> <bytes of scratch>
  local dest=$1 scr=$2 need_dest=$3 need_scr=$4
  if [ "$SKIP_SPACE_CHECK" = 1 ]; then
    info "Disk space not checked (--no-space-check)"
    return 0
  fi
  local scr_free dest_free
  dest_free=$(free_bytes "$dest")
  if ! is_uint "$dest_free"; then
    echo "WARNING: cannot measure free disk space (df failed); continuing without the check." >&2
    return 0
  fi
  if [ "$need_scr" -eq 0 ]; then
    info "Disk space: needs about $(size_str "$need_dest") in $dest (the files); $(size_str "$dest_free") free"
    space_note
    [ "$dest_free" -ge "$need_dest" ] || die_space "$dest" "$need_dest" "$dest_free"
    return 0
  fi
  scr_free=$(free_bytes "$scr")
  if ! is_uint "$scr_free"; then
    echo "WARNING: cannot measure free disk space (df failed); continuing without the check." >&2
    return 0
  fi
  local scr_dev dest_dev
  scr_dev=$(stat -c %d -- "$scr" 2>/dev/null || true)
  dest_dev=$(stat -c %d -- "$dest" 2>/dev/null || true)
  if [ -z "$scr_dev" ] || [ "$scr_dev" = "$dest_dev" ]; then
    local need=$((need_scr + need_dest))
    info "Disk space: needs about $(size_str "$need") in $dest (slices read, then the files); $(size_str "$dest_free") free"
    space_note
    [ "$dest_free" -ge "$need" ] || die_space "$dest" "$need" "$dest_free"
  else
    info "Disk space: needs about $(size_str "$need_scr") in $scr (slices read); $(size_str "$scr_free") free"
    info "            and about $(size_str "$need_dest") in $dest (the files); $(size_str "$dest_free") free"
    space_note
    [ "$scr_free" -ge "$need_scr" ] || die_space "$scr" "$need_scr" "$scr_free"
    [ "$dest_free" -ge "$need_dest" ] || die_space "$dest" "$need_dest" "$dest_free"
  fi
}

# Refuse, BEFORE the first tape read, a destination this restore could not
# be trusted to fill (#405). dar under -Q answers "no" to every overwrite
# question and exits 0, so a re-run over a restore that stopped part way (a
# full disk, a power cut, Ctrl-C) used to report RESTORE COMPLETE over the
# truncated files it left. And a scratch directory left by a killed restore
# may hold decrypted data, so it is named and never silently reused.
check_stale_scratch() { # <directory>...
  local d stale
  for d in "$@"; do
    [ -d "$d" ] || continue
    for stale in "$d"/.tapectl-restore.*; do
      [ -e "$stale" ] || continue
      die "refusing to restore: $stale is the scratch directory of an earlier
       restore that was killed or lost power, and it may hold DECRYPTED archive
       data. Remove it (rm -rf \"$stale\") and run the same command again.
       Nothing has been read from tape."
    done
  done
}

check_destination() { # <destination> <scratch parent>
  local dest=$1 scr=$2
  if [ -e "$dest" ] && [ ! -d "$dest" ]; then
    die "--to $dest exists and is not a directory. Nothing has been read from tape."
  fi
  check_stale_scratch "$dest" "$scr"
  [ -d "$dest" ] || return 0
  [ -n "$(ls -A "$dest")" ] || return 0
  if [ "$OVERWRITE" = 1 ]; then
    info "--to $dest is not empty; --overwrite replaces every file this unit restores"
    return 0
  fi
  die "--to $dest is not empty.
       dar keeps a file that is already there and still reports success, so a
       restore into a directory that holds files cannot be trusted: after an
       earlier restore that stopped part way, the truncated files would stay.
       Restore into an empty or new directory, or, to finish an earlier restore
       of the same unit, run the same command again with --overwrite, which
       replaces every file the unit restores.
       Nothing has been read from tape."
}

# The backstop behind check_destination: a file that appeared in --to while
# the restore ran is still kept by dar, which says so only in this line on
# its stdout, and exits 0.
die_if_dar_skipped() { # <dar output> <destination>
  local skipped
  skipped=$(grep 'not restored (user choice)$' "$1" 2>/dev/null |
    sed 's/ not restored (user choice)$//' | head -n 20 || true)
  [ -z "$skipped" ] && return 0
  die "the restore into $2 is INCOMPLETE: dar declined to overwrite file(s)
       that already existed there, and reported success anyway. The old copies
       are still in place, so they are NOT what is on tape. Skipped:
$skipped
       Restore into an empty directory, or run again with --overwrite."
}

# The dar on this machine: its version, and whether it can start at all.
# A dar built with GPG support asks, at startup, whether to carry on when
# the gpg program is missing; under -Q it answers no and aborts. `dar -V`
# meets the same question, so asking it first names the problem before the
# tape is read rather than after every slice has been.
DAR_VERSION=""
dar_check() {
  local out
  out=$(dar -V -Q -N 2>&1 </dev/null || true)
  if printf '%s\n' "$out" | grep -i 'INITIALIZATION FAILED FOR GPGME' >/dev/null; then
    die "dar cannot start here: it was built with GPG support and needs the gpg
       program on PATH (package gnupg; on Debian/Ubuntu: apt install gnupg).
       dar's own words: $(printf '%s\n' "$out" | grep -i 'INITIALIZATION FAILED FOR GPGME' | head -n 1)
       Nothing has been read from tape."
  fi
  DAR_VERSION=$(printf '%s\n' "$out" | sed -n 's/.*dar version \([0-9][0-9]*\.[0-9][0-9]*\.[0-9][0-9]*\).*/\1/p' | head -n 1)
  if [ -z "$DAR_VERSION" ]; then
    echo "WARNING: cannot tell which dar this is (dar -V said nothing usable); restoring the" >&2
    echo "         safe way, decrypting each unit's slices to disk before dar reads them." >&2
  fi
}

# Is this dar at least version $1 (X.Y.Z)? False when the version is unknown.
dar_at_least() {
  [ -n "$DAR_VERSION" ] || return 1
  awk -v have="$DAR_VERSION" -v want="$1" 'BEGIN {
    split(have, a, "."); split(want, b, ".")
    for (i = 1; i <= 3; i++) {
      if (a[i] + 0 > b[i] + 0) exit 0
      if (a[i] + 0 < b[i] + 0) exit 1
    }
    exit 0
  }'
}

# ---- prerequisite check ----

# What each mode runs, besides mt (every mode that reads the tape). A mode
# checks its own tools when it starts, before the first tape read, so a host
# that can only read and hash a tape is not refused for tools a restore needs.
# --info, --verify:
TOOLS_READ=(__TOOLS_READ__)
# --find-envelope:
TOOLS_ENVELOPE=(__TOOLS_ENVELOPE__)
# --list:
TOOLS_LIST=(__TOOLS_LIST__)
# --restore --path:
TOOLS_PATH=(__TOOLS_PATH__)
# --restore, whole units (every tool any mode may run):
TOOLS_RESTORE=(__TOOLS_RESTORE__)
# --where, --find (the envelopes' index, read as plain text):
TOOLS_INDEX=(__TOOLS_INDEX__)

require_tools() { # <tool>...
  local tool
  command -v mt >/dev/null 2>&1 || command -v mt-st >/dev/null 2>&1 ||
    die "missing required tool: mt (package mt-st)"
  for tool in "$@"; do
    command -v "$tool" >/dev/null 2>&1 || die "missing required tool: $tool"
  done
}

# ---- tape helpers ----

# The mt this script drives. mt-st is the one that can set the block size;
# stock Debian and Ubuntu install GNU cpio's mt as plain `mt` when mt-st is
# absent, and that one has no setblk. tape_init chooses.
MT=mt

tape_init() {
  if command -v mt-st >/dev/null 2>&1; then
    MT=mt-st
  fi
  local ver
  ver=$("$MT" --version 2>&1 | head -n 1 || true)
  if "$MT" -f "$DEVICE" setblk "$BLOCK" 2>"$WORK/mt.err"; then
    return 0
  fi
  local why
  why=$(head -n 1 "$WORK/mt.err" 2>/dev/null || true)
  case "$ver $why" in
  *mt-st*)
    die "cannot set the block size on $DEVICE: ${why:-mt failed}
       mt-st itself refused, so the device is the problem: is $DEVICE a tape
       drive, with a cartridge loaded? List the drives: ls -l /dev/tape/by-id/"
    ;;
  *"GNU cpio"* | *"invalid argument"* | *setblk*)
    echo "WARNING: '$MT' is not mt-st (${ver:-unknown version}) and cannot set the block size." >&2
    echo "         Reading $DEVICE in whatever mode the drive is in. In variable-block mode, the" >&2
    echo "         st driver's default, dd bs=512k reads each 512 KiB tape block whole and this" >&2
    echo "         works. If a read fails, install mt-st (apt install mt-st) and run again." >&2
    ;;
  *)
    die "cannot set the block size on $DEVICE with '$MT' (${ver:-unknown version}): ${why:-mt failed}
       This script needs mt-st (package mt-st). If $MT is mt-st, check that
       $DEVICE is a tape drive with a cartridge loaded: ls -l /dev/tape/by-id/"
    ;;
  esac
}

# CUR is the tape file the head sits at the START of, or empty when that is
# not known for certain (#396). It is 0 after a rewind and pos+1 after a read
# that took file pos through its filemark; a failed mt or dd, or a read that
# stopped inside a file, empties it. seek_to moves only forward from a known
# CUR and rewinds whenever it is unknown or behind the target, so any doubt
# costs a rewind and never a read of the wrong file. Before 1.1.0 every read
# rewound to the start of the tape, which on LTO costs about two minutes.
CUR=""
MT_STATUS_WARNED=0

# Where the drive says the head is, as "FILE BLOCK", or nothing when mt
# status cannot be read or parsed (mt-st and GNU mt word it differently).
mt_position() {
  "$MT" -f "$DEVICE" status 2>/dev/null | awk '__AWK_MT_POSITION__' || true
}

# Put the head at the start of tape file $1. Returns 1 if mt fails.
seek_to() {
  local pos=$1 from=$CUR here
  CUR=""
  if [ -n "$from" ]; then
    # Trust CUR only if the drive agrees, as tapectl checks MTIOCGET.
    here=$(mt_position)
    if [ -z "$here" ]; then
      if [ "$MT_STATUS_WARNED" = 0 ]; then
        echo "NOTE: '$MT status' gave no file number this script can read; rewinding before every read (slower, still correct)." >&2
        MT_STATUS_WARNED=1
      fi
      from=""
    elif [ "$here" != "$from 0" ]; then
      echo "NOTE: the drive reports file/block $here, not the start of file $from; rewinding to be sure." >&2
      from=""
    fi
  fi
  if [ -n "$from" ] && [ "$pos" -eq "$from" ]; then
    :
  elif [ -n "$from" ] && [ "$pos" -gt "$from" ]; then
    "$MT" -f "$DEVICE" fsf $((pos - from)) 2>/dev/null || return 1
  else
    "$MT" -f "$DEVICE" rewind 2>/dev/null || return 1
    if [ "$pos" -gt 0 ]; then
      "$MT" -f "$DEVICE" fsf "$pos" 2>/dev/null || return 1
    fi
  fi
  CUR=$pos
}

# dd's own complaint, for a NOTE: not its record counts.
dd_detail() {
  grep -v -e 'records in$' -e 'records out$' -e 'bytes.*copied' "$1" 2>/dev/null | tail -n 2 || true
}

# Read tape file at position $1 into file $2 (raw bytes, block-padded). Dies
# on failure, naming the cause — used once a source is already trusted
# (post-ladder). dd's stderr used to go to /dev/null, so a full disk here ended
# the script with no message at all.
read_tape_raw() {
  local pos=$1 out=$2
  seek_to "$pos" || die "cannot position the tape at file $pos ($MT failed)"
  if ! dd if="$DEVICE" of="$out" bs="$BLOCK" 2>"$WORK/dd.err"; then
    CUR=""
    die_io "reading tape file $pos failed" "$WORK/dd.err" "$(dirname "$out")"
  fi
  if [ -s "$out" ]; then CUR=$((pos + 1)); else CUR=""; fi
}

# Same, but never exits: returns 1 on any failure (missing position, I/O
# error, or an empty read). Used wherever "absent" must be distinguished from
# "present but wrong" — the ladder's rung selection relies on this instead of
# letting `set -e` kill the script. dd's complaint is kept as a NOTE.
try_read_tape_raw() {
  local pos=$1 out=$2
  seek_to "$pos" || return 1
  if ! dd if="$DEVICE" of="$out" bs="$BLOCK" 2>"$WORK/dd.err"; then
    CUR=""
    echo "NOTE: reading tape file $pos failed: $(dd_detail "$WORK/dd.err")" >&2
    return 1
  fi
  if [ -s "$out" ]; then
    CUR=$((pos + 1))
  else
    CUR=""
    return 1
  fi
}

# sha256 of tape file $1, first $2 bytes (all of it when $2 is empty), hashed
# as it streams off the tape: --verify used to copy each file into WORK first,
# twice, and a 10 GiB slice in a RAM-backed /tmp made a good tape FAIL as
# "unreadable". Returns 1 when the file cannot be reached or read, or yields no
# bytes at all. The hash is left in TAPE_SHA, not printed: called as
# $(hash_tape_file ...) it would run in a subshell, and the cursor it moves
# would be lost with it.
EMPTY_SHA256=e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855
TAPE_SHA=""
hash_tape_file() {
  local pos=$1 size=$2 sum
  TAPE_SHA=""
  seek_to "$pos" || return 1
  CUR=""
  if [ -n "$size" ]; then
    # head stops at the exact size; cat drains the block padding after it,
    # so dd never takes a SIGPIPE that pipefail would read as a tape fault,
    # and the head ends past the filemark.
    sum=$(dd if="$DEVICE" bs="$BLOCK" 2>"$WORK/dd.err" | {
      head -c "$size" | sha256sum
      cat >/dev/null
    }) || return 1
  else
    sum=$(dd if="$DEVICE" bs="$BLOCK" 2>"$WORK/dd.err" | sha256sum) || return 1
  fi
  sum=${sum%% *}
  [ -n "$sum" ] && [ "$sum" != "$EMPTY_SHA256" ] || return 1
  CUR=$((pos + 1))
  TAPE_SHA=$sum
}

# Keep a copy of tape file $1 (at most $3 bytes of it) in file $2 while
# passing it: --verify reads Files 1 and 2 on its way to File 3 and checks
# them once File 3 says what they should be, instead of rewinding for them.
# Returns 1 if the file cannot be read.
stash_tape_file() {
  local pos=$1 out=$2 max=$3
  seek_to "$pos" || return 1
  CUR=""
  dd if="$DEVICE" bs="$BLOCK" 2>"$WORK/dd.err" | {
    head -c "$max" >"$out"
    cat >/dev/null
  } || return 1
  [ -s "$out" ] || return 1
  CUR=$((pos + 1))
}

# The most of a file before the front index that --verify keeps while passing
# it (Files 0-2 are a few KiB each; this only bounds a damaged tape).
STASH_MAX=$((64 * BLOCK))

# sha256 of the first $2 bytes (all, when $2 is empty) of tape file $1 from
# what --verify kept of it while passing; returns 1 when nothing was kept, or
# too little, so the caller reads the tape instead.
stashed_sha() {
  local f="$WORK/stash.$1" size=$2 have
  [ -f "$f" ] || return 1
  have=$(wc -c <"$f")
  if [ -n "$size" ]; then
    [ "$have" -ge "$size" ] || return 1
    head -c "$size" "$f" | sha256sum | awk '{print $1}'
  else
    [ "$have" -lt "$STASH_MAX" ] || return 1
    sha256sum "$f" | awk '{print $1}'
  fi
}

# ---- TOML helpers (flat key = value parsing; sec 3.1 grammar contract) ----

# Print the value for a TOML key on a "key = value" line. Strips quotes.
toml_val() {
  local file=$1 key=$2
  awk -v k="$key" '
    $1 == k && $2 == "=" {
      v = $3
      for (i = 4; i <= NF; i++) v = v " " $i
      gsub(/^"/, "", v); gsub(/"$/, "", v)
      print v; exit
    }
  ' "$file"
}

# Parse [[files]] blocks (front index or the seal marker's embedded copy)
# into lines: position|type|size_bytes|sha256_encrypted. Either field may be
# empty (front index's own entry has neither; the seal marker's own entry
# has neither; everything else has both).
parse_front_index_entries() {
  local file=$1
  awk '__AWK_PARSE_FILE_LIST__' "$file"
}

# Front-index self-consistency check (sec 2.5): positions strictly
# increasing from 0 (a total index has no gaps), exactly one front_index
# entry and it is at position 3, exactly one seal_marker entry and it is
# last. Optional $2 = the expected total file count; when given, the entry
# count must match it too. A subtly wrong map fails loudly (nonzero exit)
# rather than silently misleading recovery. No `exit` appears in the main
# per-record block (awk runs END even after a mid-block `exit`) — flags are
# accumulated and only checked in END, where `exit` really does terminate.
check_file_list_consistency() {
  local list=$1 expected_total=${2:-}
  awk -F'|' -v expected="$expected_total" '__AWK_CHECK_FILE_LIST__' "$list"
}

# Look up size_bytes / sha256_encrypted for a given tape file position.
file_size_at() { awk -F'|' -v p="$1" '$1 == p { print $3; exit }' "$2"; }
file_hash_at() { awk -F'|' -v p="$1" '$1 == p { print $4; exit }' "$2"; }
file_type_at() { awk -F'|' -v p="$1" '$1 == p { print $2; exit }' "$2"; }

# List envelope positions in on-tape order (the file list is already
# position-sorted, and the format's fixed zone order puts the tenant
# envelope(s), then the operator envelope and its backup, then each tenant
# envelope's backup: every primary before every backup).
envelope_positions() {
  awk -F'|' '__AWK_FIND_ENVELOPE__' "$1"
}

# ---- bootstrap (File 0 — identity + pointers only in layout v2) ----

bootstrap_thunk() {
  tape_init
  info "Reading ID thunk (file 0)..."
  # Kept raw as well: --verify checks File 0's bytes against the front index
  # without going back for them. The text is plaintext TOML with no NULs of
  # its own, so deleting every NUL removes exactly the block padding.
  read_tape_raw 0 "$WORK/stash.0"
  tr -d '\0' <"$WORK/stash.0" >"$WORK/id_thunk.txt"
  sed -n '/^\[volume\]/,$p' "$WORK/id_thunk.txt" >"$WORK/thunk.toml"

  check_layout_version "$(toml_val "$WORK/thunk.toml" layout_version)"
  check_magic "$(toml_val "$WORK/thunk.toml" magic)"
  check_requires "File 0" "$WORK/thunk.toml"

  FRONT_INDEX=$(toml_val "$WORK/thunk.toml" front_index)
  SEAL_MARKER=$(toml_val "$WORK/thunk.toml" seal_marker)
  TOTAL_FILES=$(toml_val "$WORK/thunk.toml" total_files)
  CREATED=$(toml_val "$WORK/thunk.toml" created_at)

  require_uint front_index "$FRONT_INDEX"
  require_uint seal_marker "$SEAL_MARKER"
  require_uint total_files "$TOTAL_FILES"

  # Which volume is ACTUALLY in the drive. $LABEL is baked into this script at
  # write time; it says which tape the script was made for, and is printed even
  # when a different tape is loaded. Only the thunk says what is really there.
  TAPE_LABEL=$(safe_str "$(toml_val "$WORK/thunk.toml" label)")
  check_tape_identity
}

# This script reads layout v2 only. Every tape carries the script that reads
# it at file 2, so a tape of another layout is refused with the way to that
# script, before anything of it is interpreted. A missing value (a damaged
# thunk) warns and reads on as v2: a damaged thunk, the case where an heir
# needs this script most, must never become a hard stop.
check_layout_version() { # <layout_version as File 0 states it>
  local v=$1
  if [ -z "$v" ]; then
    echo "WARNING: File 0 states no layout_version (damaged?); reading this tape as layout v2." >&2
    return 0
  fi
  [ "$v" != 2 ] || return 0
  die "this tape is layout_version $(safe_str "$v"); this script reads layout v2 only.
       Use the RESTORE.sh stored on THIS tape — every tape carries the script
       that reads it, at file 2:
         mt -f $DEVICE rewind && mt -f $DEVICE fsf 2
         dd if=$DEVICE bs=512k | tr -d '\\0' > RESTORE.sh"
}

# The features this script knows how to read (volume-format-v2.md, "How v2
# extends"). A tape whose File 0 or seal marker names anything else under
# `requires` is refused by name: a later tapectl that changes something a
# reader must understand says so there, and this script must stop rather than
# misread it. An absent `requires` (every tape before 1.1.0) requires nothing,
# and any key a table carries that this script does not know is ignored
# unless `requires` names it.
KNOWN_FEATURES=""

die_newer_needed() { # <what> <detail>
  die "$2
       This RESTORE.sh (layout v2, tapectl 1.1.0) cannot read it safely, and
       will not guess. Use the RESTORE.sh stored on THIS tape — every tape
       carries the script that reads it, at file 2:
         mt -f $DEVICE rewind && mt -f $DEVICE fsf 2
         dd if=$DEVICE bs=512k | tr -d '\\0' > RESTORE.sh
       or a newer tapectl ($1 names what it needs)."
}

check_magic() { # <magic as File 0 states it>
  local m=$1
  [ -z "$m" ] && return 0
  [ "$m" = "tapectl-volume-v2" ] && return 0
  die_newer_needed "File 0" "this tape's File 0 says magic = $(safe_str "$m"), not tapectl-volume-v2."
}

check_requires() { # <what> <toml file>
  local what=$1 file=$2 raw list f unknown=""
  raw=$(toml_val "$file" requires)
  [ -z "$raw" ] && return 0
  case "$raw" in
  \[*\]) ;;
  *) die_newer_needed "$what" "$what states requires = $(safe_str "$raw"), which this script cannot parse." ;;
  esac
  list=$(printf '%s' "$raw" | tr -d '[]",' | tr -s ' ' ' ')
  for f in $list; do
    case " $KNOWN_FEATURES " in
    *" $f "*) ;;
    *) unknown="$unknown $(safe_str "$f")" ;;
    esac
  done
  [ -z "$unknown" ] && return 0
  die_newer_needed "$what" "this tape requires feature(s)${unknown}, which this script does not know
       ($what states requires = $(safe_str "$raw")). It was written by a newer
       tapectl (File 0: tapectl_version $(safe_str "$(toml_val "$WORK/thunk.toml" tapectl_version)"))."
}

# Always say which device was read and what was found there. A silent default
# device is how a wrong-tape read disguises itself as a key or data problem:
# with the wrong cartridge loaded, --info happily describes it while
# --find-envelope reports that no envelope matched any key given, which reads
# as "your key is wrong" or "your archive is gone" when it means neither.
#
# This WARNS and continues rather than exiting. Everything here except $LABEL is
# generic, so running this script against a sibling tape is legitimate recovery
# — and a damaged thunk (the case where an heir needs this script most) must
# never be turned into a hard stop.
check_tape_identity() {
  info "Tape device:      $DEVICE${TAPE_DEVICE:+ (from TAPE_DEVICE)}"
  info "Tape identifies as: ${TAPE_LABEL:-<unreadable>}"
  # The second cartridge of --swap-tapes is a different tape on purpose.
  [ "${OTHER_TAPE:-0}" = 0 ] || return 0
  if [ -n "$TAPE_LABEL" ] && [ "$TAPE_LABEL" != "$LABEL" ]; then
    echo "" >&2
    echo "!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!" >&2
    echo "  WRONG TAPE? This script was written for volume '$LABEL'," >&2
    echo "  but the tape in $DEVICE identifies itself as '$TAPE_LABEL'." >&2
    echo "" >&2
    echo "  Reading continues, but keys and units from '$LABEL' will NOT" >&2
    echo "  be found on this tape. If that is not what you intended:" >&2
    echo "    - load the '$LABEL' cartridge, or" >&2
    echo "    - point this script at the right drive:" >&2
    echo "        TAPE_DEVICE=/dev/nstN $0 ..." >&2
    echo "  Drives on this machine: ls -l /dev/tape/by-id/" >&2
    echo "!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!" >&2
    echo "" >&2
  fi
}

# ---- rung 1: the front index (File 3) ----

# On success sets FRONT_INDEX_HASH and leaves the parsed, validated entry
# list at $WORK/files_from_index.txt. Never exits — callers test it with `if`
# or `&&`.
try_front_index() {
  try_read_tape_raw "$FRONT_INDEX" "$WORK/front_index.raw" || return 1
  tr -d '\0' <"$WORK/front_index.raw" >"$WORK/front_index.stripped"
  grep -q '^\[index\]' "$WORK/front_index.stripped" || return 1
  sed -n '/^\[index\]/,$p' "$WORK/front_index.stripped" >"$WORK/index.toml"
  parse_front_index_entries "$WORK/index.toml" >"$WORK/files_from_index.txt"
  [ -s "$WORK/files_from_index.txt" ] || return 1
  check_file_list_consistency "$WORK/files_from_index.txt" "$TOTAL_FILES" || return 1
  FRONT_INDEX_HASH=$(sha256sum "$WORK/front_index.stripped" | awk '{print $1}')
  return 0
}

# ---- rung 2: the seal marker's embedded front-index copy ----

# On success sets SEAL_FRONT_INDEX_SHA256, SEAL_FILE_COUNT, SEAL_SEALED_AT and
# leaves the parsed, validated embedded copy at $WORK/files_from_seal.txt.
# Returns 1 when there is no usable seal; exits only for a seal that requires
# a feature this script does not know, which must never read as unsealed.
try_seal_copy() {
  local at="" n
  if read_last_file "$WORK/seal.raw" && seal_parses "$WORK/seal.raw"; then
    at=$((SEAL_FILE_COUNT - 1))
    if [ "$SEAL_MARKER" != "$at" ]; then
      echo "WARNING: File 0 says the seal marker is file $SEAL_MARKER, but the last file on the tape is" >&2
      echo "         a seal marker for $SEAL_FILE_COUNT files (file $at). Using the last file." >&2
    fi
  else
    # No end-of-data route (an mt without eod, or the last file is not a
    # seal): File 0's pointer, read with the same bound.
    seek_to "$SEAL_MARKER" || return 1
    read_bounded "$WORK/seal.raw" || return 1
    seal_parses "$WORK/seal.raw" || return 1
  fi
  check_requires "the seal marker" "$WORK/seal.toml"
  parse_front_index_entries "$WORK/seal.toml" >"$WORK/files_from_seal.txt"
  [ -s "$WORK/files_from_seal.txt" ] || return 1
  check_file_list_consistency "$WORK/files_from_seal.txt" "$SEAL_FILE_COUNT" || return 1
  return 0
}

# Read at most STASH_MAX bytes of the file under the head into $1: enough
# for any seal marker, and it never copies a 10 GiB data slice into /tmp.
# The head is left unknown (it may stop inside the file). Returns 1 if
# nothing was read.
read_bounded() { # <out>
  CUR=""
  dd if="$DEVICE" of="$1" bs="$BLOCK" count=$((STASH_MAX / BLOCK)) 2>"$WORK/dd.err" || {
    echo "NOTE: reading the last file failed: $(dd_detail "$WORK/dd.err")" >&2
    return 1
  }
  [ -s "$1" ]
}

# The last file on the tape, found the way the seal marker is defined — the
# last file — rather than by File 0's pointer alone (#384): space to the end
# of data, then back over two filemarks to the start of the file before
# them. mt-st calls end of data `eod`, GNU mt `eom`. Returns 1 if mt cannot.
read_last_file() { # <out>
  CUR=""
  "$MT" -f "$DEVICE" eod 2>/dev/null || "$MT" -f "$DEVICE" eom 2>/dev/null || return 1
  "$MT" -f "$DEVICE" bsfm 2 2>/dev/null || return 1
  read_bounded "$1"
}

# Does $1 hold a seal marker this script can use? Sets SEAL_FRONT_INDEX_SHA256,
# SEAL_FILE_COUNT, SEAL_SEALED_AT and $WORK/seal.toml.
seal_parses() { # <raw file>
  tr -d '\0' <"$1" >"$WORK/seal.stripped"
  grep '^\[seal\]' "$WORK/seal.stripped" >/dev/null 2>&1 || return 1
  sed -n '/^\[seal\]/,$p' "$WORK/seal.stripped" >"$WORK/seal.toml"
  SEAL_FRONT_INDEX_SHA256=$(toml_val "$WORK/seal.toml" front_index_sha256)
  SEAL_FILE_COUNT=$(toml_val "$WORK/seal.toml" file_count)
  SEAL_SEALED_AT=$(toml_val "$WORK/seal.toml" sealed_at)
  [ -n "$SEAL_FRONT_INDEX_SHA256" ] || return 1
  is_uint "$SEAL_FILE_COUNT" || return 1
  [ "$SEAL_FILE_COUNT" -gt 0 ]
}

# ---- rung 3: nothing left to parse ----

zero_strip_instructions() {
  cat <<'ZSEOF'
FATAL: both the front index (File 3) and the seal marker's embedded copy
are unreadable or fail their self-consistency check. The navigation map is
lost at both ends of the tape (degradation ladder rung 3).

Manual "zero-strip" recovery is still possible: envelopes are among the
first files after the front index, and block padding can be defeated
without knowing exact sizes:

  1. mt -f "$DEVICE" setblk 524288
     mt -f "$DEVICE" rewind && mt -f "$DEVICE" fsf 4
  2. Read one candidate file. Each read leaves the tape at the start of the
     next file, so repeat this step for each file in turn, moving forward:
       dd if="$DEVICE" bs=524288 of=candidate.raw
  3. Strip only the TRAILING zero bytes, which are block padding. Zero bytes
     inside the ciphertext are part of it: deleting every zero byte (as
     tr -d does) destroys the file. These four lines keep everything up to
     the last non-zero byte:
       size=$(stat -c %s candidate.raw)
       tail_len=$(tail -c 524288 candidate.raw | wc -c)
       keep=$(tail -c 524288 candidate.raw | od -An -v -tu1 | awk '{ for (i = 1; i <= NF; i++) { n++; if ($i != 0) last = n } } END { print last + 0 }')
       head -c $((size - tail_len + keep)) candidate.raw > candidate.stripped
  4. Try: age -d -i YOUR_KEY.age.key candidate.stripped > candidate.dec
     "no identity matched" means this file is not sealed to your key: go
     back to step 2 for the next file. Any other error: real ciphertext can
     end in a zero byte, so put one back and retry, up to four times:
       printf '\0' >> candidate.stripped
  5. Once a candidate decrypts, it is your tenant (or operator) envelope:
     untar it and read MANIFEST.toml for the exact tape position and byte
     size of every slice belonging to your unit(s); read each slice (steps
     1-2, with fsf to its position), trim it to that exact size with
     truncate -s SIZE instead of zero-stripping, decrypt it to restore.N.dar,
     then, in an empty directory: dar -x restore -R /destination -O -Q -N

See the system guide (File 1), "If All Else Fails", for the full narrative.
ZSEOF
}

# ---- establish the trusted file map (rung 1 -> rung 2 -> rung 3) ----

# Sets FILES_TXT and FILES_SOURCE on success; on total failure prints the
# zero-strip procedure and exits (used by modes that cannot proceed at all
# without a map: --find-envelope, --restore; --info and --verify apply the
# same ladder but handle a total failure themselves so they can still report
# a verdict).
establish_files() {
  bootstrap_thunk

  if try_front_index; then
    FILES_TXT="$WORK/files_from_index.txt"
    FILES_SOURCE="front_index"
    return 0
  fi

  echo "WARNING: front index (file $FRONT_INDEX) is unreadable or failed its self-consistency check — falling back to the seal marker's embedded copy (degradation ladder RUNG-2, volume-format-v2.md sec 4)." >&2

  if try_seal_copy; then
    FILES_TXT="$WORK/files_from_seal.txt"
    FILES_SOURCE="seal_embedded_copy(RUNG-2)"
    echo "WARNING: using the seal marker's embedded copy as the file map — the front of the tape may be damaged; treat this recovery as degraded." >&2
    return 0
  fi

  zero_strip_instructions >&2
  exit 1
}

# ---- --info ----

do_info() {
  bootstrap_thunk

  local fi_ok=0 seal_ok=0
  try_front_index && fi_ok=1
  try_seal_copy && seal_ok=1

  local verdict
  if [ "$seal_ok" -ne 1 ]; then
    verdict="UNSEALED"
  elif [ "$fi_ok" -ne 1 ] || [ "$FRONT_INDEX_HASH" != "$SEAL_FRONT_INDEX_SHA256" ]; then
    verdict="DAMAGED (ends disagree)"
  else
    verdict="SEALED"
  fi

  local table_file="" table_source=""
  if [ "$fi_ok" -eq 1 ]; then
    table_file="$WORK/files_from_index.txt"
    table_source="front index (file $FRONT_INDEX)"
  elif [ "$seal_ok" -eq 1 ]; then
    table_file="$WORK/files_from_seal.txt"
    table_source="seal marker's embedded copy (RUNG-2 — front index unreadable)"
  fi

  echo ""
  # The tape's own label, not this script's — the map below describes what is
  # actually loaded. Identical in the normal case; honest in the wrong-tape one.
  echo "=== tapectl volume: ${TAPE_LABEL:-$LABEL} ==="
  echo ""
  echo "Verdict: $verdict"
  case "$verdict" in
  SEALED)
    echo "  Front index (file $FRONT_INDEX) hash matches the seal marker's binding."
    ;;
  UNSEALED)
    echo "  No valid seal marker was found — this tape was never sealed, or the"
    echo "  write was interrupted or aborted. Trailing data slices may be missing."
    ;;
  "DAMAGED (ends disagree)")
    echo "  The seal marker is present but the front index does not match it (or"
    echo "  is unreadable). Never trust the seal marker's presence alone — see"
    echo "  the system guide."
    ;;
  esac
  echo ""

  if [ -n "$table_file" ]; then
    echo "File map (source: $table_source):"
    while IFS='|' read -r pos type size hash; do
      printf "  %3d  %-24s  %8s bytes  %s\n" "$pos" "$type" "${size:--}" "${hash:+${hash:0:16}...}"
    done <"$table_file"
  else
    echo "No usable file map: both the front index and the seal marker's"
    echo "embedded copy are unreadable."
    echo ""
    zero_strip_instructions
  fi

  echo ""
  echo "Sealed at: ${SEAL_SEALED_AT:-unknown (tape not sealed)}"
  echo ""
  echo "To decrypt your envelope:"
  echo "  $0 --find-envelope --key YOUR_KEY.age.key"
  echo "To check tape integrity without any key:"
  echo "  $0 --verify"
  echo "To find the other tapes that hold a unit, or the tape that holds a file:"
  echo "  $0 --where UNIT --key YOUR_KEY.age.key"
  echo "  $0 --find TEXT --key YOUR_KEY.age.key"
}

# ---- --verify ----

# A file that failed the walk. A data slice also says whether a copy of it is
# in a --slices-from directory (matched by its sha256 alone); one that is not
# is remembered for the second cartridge --swap-tapes asks for.
VERIFY_BAD=()
VERIFY_COVERED=0
VERIFY_OTHER=0
verify_failed() { # <position> <type> <bytes> <sha256> <what>
  local pos=$1 type=$2 size=$3 hash=$4 what=$5 extra=""
  if [ "$type" = data_slice ] && [ -n "$size" ]; then
    if dir_find "$pos" "$size" "$hash"; then
      extra="; a copy whose sha256 matches is $DIR_HIT"
      VERIFY_COVERED=$((VERIFY_COVERED + 1))
    else
      VERIFY_BAD+=("$pos|$size|$hash")
    fi
  else
    VERIFY_OTHER=$((VERIFY_OTHER + 1))
  fi
  printf "FAIL  file %3d  %-24s  (%s%s)\n" "$pos" "$type" "$what" "$extra"
}

verify_needed() { # job|number|bytes|sha256|label, for each failed slice with no copy yet
  local e pos size hash
  for e in ${VERIFY_BAD[@]+"${VERIFY_BAD[@]}"}; do
    IFS='|' read -r pos size hash <<<"$e"
    echo "v|$pos|$size|$hash|tape file $pos"
  done
}

do_verify() {
  bootstrap_thunk
  [ "$SALVAGE" = 0 ] || salvage_init

  # One pass, ascending, from a single rewind (#396). Files 1 and 2 lie
  # between File 0 and the front index, so their bytes are kept as the head
  # passes them and checked once the front index says what they should be.
  # The seal marker is the last file on the tape, so it is read last (#412),
  # after the walk, with the same verdict as before. Only when the front index
  # cannot be used is the seal read first, for its copy of the map.
  local p
  for p in 1 2; do
    stash_tape_file "$p" "$WORK/stash.$p" "$STASH_MAX" || rm -f "$WORK/stash.$p"
  done

  local fi_ok=0 seal_ok=0 files_file=""
  if try_front_index; then
    fi_ok=1
    files_file="$WORK/files_from_index.txt"
  elif try_seal_copy; then
    seal_ok=1
    echo "WARNING: front index unreadable — verifying against the seal marker's embedded copy (degradation ladder RUNG-2)." >&2
    files_file="$WORK/files_from_seal.txt"
  else
    zero_strip_instructions >&2
    exit 1
  fi
  FILES_TXT=$files_file
  FILES_SOURCE=verify

  echo "=== keyless integrity walk: $LABEL ==="
  echo ""

  local overall_ok=1

  while IFS='|' read -r pos type size hash; do
    # Entries with no hash are self-referential (front_index's own entry in
    # the front-index-sourced list, and the seal marker's own entry always)
    # — nothing on the tape hashes them, so there is nothing to compare.
    [ -z "$hash" ] && continue

    require_uint "position(@$type)" "$pos"
    [ -z "$size" ] || require_uint "size_bytes(@$pos)" "$size"

    local actual=""
    if ! actual=$(stashed_sha "$pos" "$size"); then
      if hash_tape_file "$pos" "$size"; then
        actual=$TAPE_SHA
      else
        verify_failed "$pos" "$type" "$size" "$hash" "unreadable"
        overall_ok=0
        continue
      fi
    fi

    if [ "$actual" = "$hash" ]; then
      printf "PASS  file %3d  %-24s\n" "$pos" "$type"
    else
      verify_failed "$pos" "$type" "$size" "$hash" "hash mismatch"
      overall_ok=0
    fi
  done <"$files_file"

  # The seal marker, last. File 3 vs the seal binding is the one entry the
  # per-file loop above cannot check when the list came from the front index
  # itself (its own entry carries no hash there — self-reference).
  if [ "$fi_ok" -eq 1 ] && try_seal_copy; then
    seal_ok=1
  fi
  if [ "$seal_ok" -eq 1 ] && [ "$fi_ok" -eq 1 ]; then
    if [ "$FRONT_INDEX_HASH" = "$SEAL_FRONT_INDEX_SHA256" ]; then
      printf "PASS  file %3d  %-24s  (matches seal binding)\n" "$FRONT_INDEX" "front_index"
    else
      printf "FAIL  file %3d  %-24s  (seal binding mismatch)\n" "$FRONT_INDEX" "front_index"
      overall_ok=0
    fi
  elif [ "$seal_ok" -eq 1 ]; then
    printf "FAIL  file %3d  %-24s  (unreadable/unparseable)\n" "$FRONT_INDEX" "front_index"
    overall_ok=0
  else
    echo "WARNING: no valid seal marker — this tape is UNSEALED; completeness cannot be confirmed. Only the files above were checked." >&2
  fi

  echo ""
  if [ "$overall_ok" -eq 1 ]; then
    echo "VERIFY: PASS — every file matches the front index."
  else
    if [ "$SWAP_TAPES" = 1 ] && [ ${#VERIFY_BAD[@]} -gt 0 ]; then
      VERIFY_ONLY=1
      swap_loop
    fi
    echo "VERIFY: FAIL — one or more files do not match. See FAIL lines above." >&2
    if [ "$SALVAGE" = 1 ]; then
      if [ "$VERIFY_OTHER" -eq 0 ] && [ ${#VERIFY_BAD[@]} -eq 0 ]; then
        echo "        Every file that failed is a data slice, and a copy of each was found (in a --slices-from" >&2
        echo "        directory or on a cartridge you loaded): --restore with the same options takes them from there." >&2
      else
        echo "        $VERIFY_OTHER failed file(s) are not data slices, and ${#VERIFY_BAD[@]} failed data slice(s) have no copy yet." >&2
      fi
    fi
    exit 1
  fi
}

# ---- envelopes ----

# The three "no envelope opened" exits share one shape (issues #288, #291):
# which keys were tried, what tape is actually in the drive, and the rotation
# fact that makes a correct-looking key fail on a correct tape. They live here,
# in one function, because the --unit exit — the one an heir is most likely to
# hit — had already drifted into a bare one-liner with none of it. When an
# envelope a key DID match turned out damaged (#412), that is said instead of
# the rotation hint: it is not a key problem.
die_no_envelope() { # [unit]
  local headline="no envelope matched $(keys_phrase)"
  if [ -n "${1:-}" ]; then
    headline="no envelope for unit '$1' matched $(keys_phrase)"
  fi
  if [ "$MATCHED_DAMAGED" -gt 0 ]; then
    die "$headline that could be opened.
       $DAMAGED_ENVELOPES envelope(s) on this tape are DAMAGED (see the WARNING
       lines above): a key matched, so this is not a wrong key. Another copy of
       this volume, or the operator or escrow key (which open the operator
       envelope and its backup), can still recover the data.
       Keys tried: $(keys_list)"
  fi
  if [ "$DAMAGED_ENVELOPES" -gt 0 ]; then
    die "$headline that could be opened.
       $DAMAGED_ENVELOPES envelope(s) on this tape are DAMAGED (see the WARNING
       lines above), but no key opened any of them: they could not be read or
       failed their hash, so this is damage, a key that is not a recipient, or
       both. Another copy of this volume, or the operator or escrow key (which
       open the operator envelope and its backup), can still recover the data.
       Tape in $DEVICE identifies as '${TAPE_LABEL:-<unreadable>}'; this script is for '$LABEL'.
       If you hold other keys, try them: an envelope is sealed with the key
       that was active when THIS tape was written.
       Keys tried: $(keys_list)"
  fi
  die "$headline
       Tape in $DEVICE identifies as '${TAPE_LABEL:-<unreadable>}'; this script is for '$LABEL'.
       If those differ, you have the wrong cartridge or the wrong drive
       (set TAPE_DEVICE=/dev/nstN) — not necessarily the wrong key.
       If they match, try your OTHER keys: an envelope is sealed with the key
       that was active when THIS tape was written, so a key issued after it
       will not open it. --find-envelope reports which keys do.
       Keys tried: $(keys_list)"
}

# Read the envelope at tape file $1 and try each key on it, on its own
# (#288). Returns 0 when a key opened it (ENV_KEY names the key; the files
# are in $WORK/env), 1 when no key matched it, 2 when it is DAMAGED: a key
# matched but the payload would not decrypt or unpack (#412). age names the
# difference — "no identity matched" is a key that is not a recipient — and
# the front index's keyless hash says so before any key is tried.
ENV_KEY=""
DAMAGED_ENVELOPES=0
# How many of those a key MATCHED (age got past the header, the payload would
# not decrypt). Only then is "not a wrong key" true; an envelope that would not
# read, or failed its hash with no key opening it, says nothing about the keys.
MATCHED_DAMAGED=0
try_envelope() { # <position>
  local pos=$1 esize ehash actual keyfile mismatch=0
  ENV_KEY=""
  # A file that will not read is damaged too, not the end of the run: its
  # backup further on holds the same bytes. A full disk here is not the
  # tape's fault, and no backup would fare better, so that still stops.
  : >"$WORK/dd.err"
  if ! try_read_tape_raw "$pos" "$WORK/envelope.enc"; then
    if grep -i 'no space left' "$WORK/dd.err" >/dev/null 2>&1; then
      die_io "reading tape file $pos failed" "$WORK/dd.err" "$WORK"
    fi
    DAMAGED_ENVELOPES=$((DAMAGED_ENVELOPES + 1))
    echo "WARNING: the envelope at file $pos could not be read from the tape: it is DAMAGED" >&2
    return 2
  fi
  esize=$(file_size_at "$pos" "$FILES_TXT")
  ehash=$(file_hash_at "$pos" "$FILES_TXT")
  if [ -n "$esize" ]; then
    require_uint "size_bytes(@$pos)" "$esize"
    [ "$esize" -gt 0 ] && truncate -s "$esize" "$WORK/envelope.enc"
  fi
  if [ -n "$ehash" ]; then
    actual=$(sha256sum "$WORK/envelope.enc" | awk '{print $1}')
    if [ "$actual" != "$ehash" ]; then
      mismatch=1
      echo "WARNING: the envelope at file $pos does not match the front index: it is DAMAGED" >&2
      echo "         (sha256 ${actual:0:16}..., the front index says ${ehash:0:16}...). Trying it anyway." >&2
    fi
  fi
  for keyfile in ${KEYS[@]+"${KEYS[@]}"}; do
    rm -rf "$WORK/env" && mkdir -p "$WORK/env"
    if age -d -i "$keyfile" <"$WORK/envelope.enc" 2>"$WORK/age.err" |
      tar xf - -C "$WORK/env/" 2>"$WORK/tar.err"; then
      ENV_KEY=$keyfile
      return 0
    fi
    if grep 'no identity matched' "$WORK/age.err" >/dev/null 2>&1; then
      info "  key $keyfile did not open the envelope at file $pos"
      continue
    fi
    # The key matched (age got past the header), so no other key can do
    # better: these bytes are damaged.
    DAMAGED_ENVELOPES=$((DAMAGED_ENVELOPES + 1))
    MATCHED_DAMAGED=$((MATCHED_DAMAGED + 1))
    echo "WARNING: key $keyfile matched the envelope at file $pos, but the envelope is DAMAGED:" >&2
    cat "$WORK/age.err" "$WORK/tar.err" 2>/dev/null | sed 's/^/         /' >&2 || true
    return 2
  done
  if [ "$mismatch" = 1 ]; then
    DAMAGED_ENVELOPES=$((DAMAGED_ENVELOPES + 1))
    return 2
  fi
  return 1
}

# The work directory's own need, as far as the front index tells it: every
# envelope is read whole into $WORK (block padded), and each one a key opens
# is kept there decrypted, about its size again, until the run ends. Which
# envelopes a key opens is not known before it tries, so this is an upper
# bound (the largest, read whole, plus every envelope kept) and only a
# WARNING; a full disk is named when it happens (die_io), and TMPDIR moves
# the work directory. The sizes come from a plaintext zone, so the sum is
# done in awk, where a crafted figure cannot overflow shell arithmetic.
check_work_space() {
  [ "$SKIP_SPACE_CHECK" = 0 ] || return 0
  local need free
  need=$(awk -F'|' -v blk="$BLOCK" '
    ($2 == "tenant_envelope" || $2 == "operator_envelope") && $3 ~ /^[0-9]+$/ {
      sum += $3
      if ($3 + 0 > max) max = $3 + 0
    }
    END { if (sum > 0) printf "%.0f\n", sum + max + blk }
  ' "$FILES_TXT")
  [ -n "$need" ] || return 0
  free=$(free_bytes "$WORK")
  is_uint "$free" || return 0
  if ! awk -v have="$free" -v want="$need" 'BEGIN { exit (have + 0 >= want + 0) ? 0 : 1 }'; then
    echo "WARNING: the work directory $WORK has $(size_str "$free") free, and reading the" >&2
    echo "         envelopes on this tape may need up to $(size_str "$need") there: each envelope is read whole," >&2
    echo "         and each one a key opens stays decrypted until the run ends. If it fills, the run" >&2
    echo "         stops and says so; set TMPDIR to a directory on a larger disk to move the work" >&2
    echo "         directory (a key that opens one small envelope needs far less than this)." >&2
  fi
}

# Walk the envelopes in tape order — forward only — trying the keys on each,
# and call "$1 <position>" for each that opens; it returns 1 to stop the
# walk. A file whose bytes equal one already tried (an envelope's backup, by
# its front-index hash) is skipped unless that one was damaged or would not
# read: then the backup is what is read instead. With a second argument the
# walk RESUMES after that position (a walk a callback stopped), remembering
# what the first one tried.
WALK_SEEN=" "
walk_envelopes() { # <on-open callback> [<resume after this position>]
  local cb=$1 after=${2:-} pos h rc ty
  if [ -z "$after" ]; then
    WALK_SEEN=" "
    check_work_space
  fi
  while IFS= read -r pos; do
    require_uint envelope_position "$pos"
    if [ -n "$after" ] && [ "$pos" -le "$after" ]; then
      continue
    fi
    h=$(file_hash_at "$pos" "$FILES_TXT")
    case "$WALK_SEEN" in
    *" $h "*)
      [ -z "$h" ] || {
        info "File $pos holds the same bytes as an envelope already tried (its backup): skipped"
        continue
      }
      ;;
    esac
    info "Trying envelope at file $pos..."
    rc=0
    try_envelope "$pos" || rc=$?
    [ "$rc" = 2 ] || [ -z "$h" ] || WALK_SEEN="$WALK_SEEN$h "
    if [ "$rc" = 0 ]; then
      ty=$(file_type_at "$pos" "$FILES_TXT")
      case "$ty" in
      *_backup) info "File $pos is a backup copy, read because the envelope it copies could not be used" ;;
      esac
      rm -rf "$WORK/opened.$pos"
      mv "$WORK/env" "$WORK/opened.$pos"
      "$cb" "$pos" || break
    fi
  done < <(envelope_positions "$FILES_TXT")
}

# One line per unit an envelope lists: unit, versions, slices, size.
units_table() { # <MANIFEST.toml>
  printf "    %-32s  %-10s  %6s  %10s\n" "Unit" "Versions" "Slices" "Size"
  awk '__AWK_UNITS_TABLE__' "$1" | while IFS='|' read -r name versions nslices bytes; do
    printf "    %-32s  %-10s  %6s  %10s\n" "$name" "$versions" "$nslices" "$(size_str "$bytes")"
  done
}

# ---- --find-envelope ----

FOUND_ENVELOPES=()
found_envelope() { # <position>
  local pos=$1 m="$WORK/opened.$1/MANIFEST.toml" who
  FOUND_ENVELOPES+=("$pos")
  who=$(safe_str "$(toml_val "$m" tenant 2>/dev/null)")
  echo ""
  info "Decrypted envelope at file $pos${who:+ (tenant: $who)}"
  info "  opened with key $ENV_KEY"
  if [ -f "$m" ]; then
    units_table "$m"
  fi
  return 0
}

do_find_envelope() {
  establish_files

  # Every envelope, not just the first that opens (#412): an operator or
  # escrow key opens them all, and the operator envelope lists every unit.
  walk_envelopes found_envelope
  [ "${#FOUND_ENVELOPES[@]}" -gt 0 ] || die_no_envelope

  if [ "${#FOUND_ENVELOPES[@]}" -eq 1 ]; then
    local d="$WORK/opened.${FOUND_ENVELOPES[0]}"
    if [ -f "$d/MANIFEST.toml" ]; then
      echo ""
      echo "--- MANIFEST.toml ---"
      cat "$d/MANIFEST.toml"
    fi
    if [ -f "$d/RECOVERY.md" ]; then
      echo ""
      echo "--- RECOVERY.md ---"
      cat "$d/RECOVERY.md"
    fi
  else
    echo ""
    echo "${#FOUND_ENVELOPES[@]} envelopes opened. The operator envelope (an operator or escrow"
    echo "key opens it) lists every unit on this tape; --restore --unit NAME finds the"
    echo "envelope that holds NAME, and --all restores every unit of the largest one."
  fi
  echo ""
  echo "To restore, run:"
  echo "  $0 --restore $(keys_args) --to /your/destination"
}

# ---- --restore ----

# Does an envelope MANIFEST.toml list a given unit under [[units]]? Exit 0 if
# so. Used by --restore to pick the right envelope for a universal key (#127).
manifest_has_unit() { # <manifest_path> <unit_name>
  awk -v u="$2" '__AWK_MANIFEST_HAS_UNIT__' "$1"
}

# A failed slice must say whether the keys or the bytes are to blame (#288).
die_slice_keys() { # <slice number>
  die "cannot decrypt slice $1 — none of the ${#KEYS[@]} key(s) decrypted it
       Keys tried: $(keys_list)
       The key that opened the envelope need not be the key that opens the
       slices: the envelope is sealed when the volume is written, each slice
       when it was staged, so a key rotation between the two puts them on
       different key generations. Supply the older key as well — repeat
       --key once per key.
       If this is the right key, the slice itself may be damaged (a damaged
       header reads this way too): --verify checks it without a key, and
       --slices-from DIR or --swap-tapes take it from another copy."
}

# Every --key as age identities. A slice is decrypted ONCE, with all of them:
# age tries each against the slice's recipients itself. Streamed from tape,
# a slice cannot be re-read for a second key, so the per-key retry of 1.0.x
# (which re-read a decrypted-to-disk slice per key) is gone; age's own
# "no identity matched any of the recipients" still names a missing key.
AGE_IDS=()
set_age_ids() {
  local k
  AGE_IDS=()
  for k in ${KEYS[@]+"${KEYS[@]}"}; do
    AGE_IDS+=(-i "$k")
  done
}

# Decrypt one slice file to another (--path, which keeps the few slices it
# needs on disk). A full disk is named as one, never as a key problem. With a
# size, only the first that many bytes of the file are the slice (a copy made
# with dd carries the tape's block padding after them).
decrypt_slice() { # <ciphertext> <plaintext-out> <slice-number> [bytes]
  local in=$1 out=$2 num=$3 size=${4:-}
  if [ -z "$size" ]; then
    if age -d "${AGE_IDS[@]}" <"$in" >"$out" 2>"$WORK/age.err"; then
      return 0
    fi
  else
    if head -c "$size" -- "$in" | age -d "${AGE_IDS[@]}" >"$out" 2>"$WORK/age.err"; then
      return 0
    fi
  fi
  cat "$WORK/age.err" >&2 || true
  if grep -i 'no space left' "$WORK/age.err" >/dev/null 2>&1; then
    rm -f "$out"
    die_io "decrypting slice $num failed" "$WORK/age.err" "$(dirname "$out")"
  fi
  rm -f "$out"
  if grep 'no identity matched' "$WORK/age.err" >/dev/null 2>&1; then
    die_slice_keys "$num"
  fi
  die "slice $num did not decrypt: a key matched it, so its bytes are damaged."
}

# Wait for background job $1, polling; if dar ($DAR_PID) exits first, return
# 3, so the caller can stop feeding a dar that is gone.
wait_feed() { # <pid>
  while kill -0 "$1" 2>/dev/null; do
    if ! kill -0 "$DAR_PID" 2>/dev/null; then
      return 3
    fi
    sleep 0.2 2>/dev/null || sleep 1
  done
  return 0
}

# dar is gone: say why, with its own words. One exit is not an error: dar
# refusing the envelope's catalogue as not this archive's (a re-staged
# snapshot, #419), which it does before writing anything. That sets
# STREAM_REFUSED and returns, and the caller restores another way.
STREAM_REFUSED=0
die_dar() { # <destination>
  local rc=0
  wait "$DAR_PID" 2>/dev/null || rc=$?
  DAR_PID=""
  CUR=""
  if grep 'do not correspond to the same data' "$WORK/extract.log" >/dev/null 2>&1; then
    STREAM_REFUSED=1
    return 0
  fi
  cat "$WORK/extract.log" >&2 2>/dev/null || true
  if grep -i 'no space left' "$WORK/extract.log" >/dev/null 2>&1; then
    die_io "dar extraction failed" "$WORK/extract.log" "$1"
  fi
  die "dar extraction failed (exit $rc) — dar's own message is above."
}

# Stream one unit's slices off the tape, through age, into dar (#412 item 1).
# dar reads the slices from FIFOs with --sequential-read, so nothing decrypted
# is written to disk except the restored files: the 1.0.x restore decrypted
# every slice to disk first and needed about twice the unit's size free. Each
# slice's ciphertext is hashed as it passes and checked against the front
# index, and age authenticates every 64 KiB as it decrypts, so damaged bytes
# stop the restore rather than reach dar. Whatever fails first is named: dar
# (its own output), the tape read (dd's), or age (a missing key, or damage).
stream_unit() { # <slices file: num|pos|size|sha per line> <destination> [catalogue]
  local list=$1 destdir=$2 cat=${3:-} fifos="$WORK/stream" i n
  STREAM_REFUSED=0
  local -a nums=() poss=() sizes=() shas=()
  local num tpos size sha
  while IFS='|' read -r num tpos size sha; do
    nums+=("$num")
    poss+=("$tpos")
    sizes+=("$size")
    shas+=("$sha")
  done <"$list"
  n=${#nums[@]}
  rm -rf "$fifos" && mkdir -p "$fifos"
  for num in "${nums[@]}"; do
    mkfifo "$fifos/restore.$num.dar"
  done
  mkfifo "$fifos/cipher"

  local -a dar_opts=(-O -Q -N --sequential-read)
  [ "$OVERWRITE" = 0 ] || dar_opts+=(-w)
  # The isolated catalogue, when there is one: dar 2.7.13 (Debian's and
  # Ubuntu's) reading sliced FIFOs sequentially without it cuts short a file
  # that runs into the last slice, for most slice sizes; with it the restore
  # is exact (measured with #411; fixed in dar 2.7.21).
  [ -z "$cat" ] || dar_opts+=(-A "$cat")
  dar -x "$fifos/restore" -R "$(abs_dir "$destdir")" "${dar_opts[@]}" >"$WORK/extract.log" 2>&1 </dev/null &
  DAR_PID=$!
  BG_PIDS+=("$DAR_PID")

  local hpid wpid st got
  for ((i = 0; i < n; i++)); do
    num=${nums[$i]} tpos=${poss[$i]} size=${sizes[$i]} sha=${shas[$i]}
    info "Slice $((i + 1))/$n — tape file $tpos"
    if ! kill -0 "$DAR_PID" 2>/dev/null; then
      die_dar "$destdir"
      return 0
    fi
    seek_to "$tpos" || die "cannot position the tape at file $tpos ($MT failed)"
    CUR=""
    sha256sum <"$fifos/cipher" >"$WORK/slice.sha" 2>/dev/null &
    hpid=$!
    BG_PIDS+=("$hpid")
    : >"$WORK/feed.status"
    (
      # Not -e here: the pipeline's statuses are the result, written below.
      set +e
      dd if="$DEVICE" bs="$BLOCK" 2>"$WORK/dd.err" </dev/null |
        {
          head -c "$size"
          cat >/dev/null
        } | tee "$fifos/cipher" |
        age -d "${AGE_IDS[@]}" 2>"$WORK/age.err" >"$fifos/restore.$num.dar"
      echo "${PIPESTATUS[*]}" >"$WORK/feed.status"
    ) </dev/null &
    wpid=$!
    if ! wait_feed "$wpid"; then
      # dar is gone. A feed that is opening, or about to open, this slice's
      # FIFO waits for a reader that will never come. Open it from the
      # other end in the background and close it at once: the feed's open
      # completes, its next write breaks the pipe, and it ends.
      (: <"$fifos/restore.$num.dar") 2>/dev/null &
      local opener=$!
      BG_PIDS+=("$opener")
      wait "$wpid" 2>/dev/null || true
      wait "$hpid" 2>/dev/null || true
      kill -KILL "$opener" 2>/dev/null || true
      die_dar "$destdir"
      return 0
    fi
    wait "$wpid" 2>/dev/null || true
    wait "$hpid" 2>/dev/null || true
    read -r -a st <"$WORK/feed.status" || true
    # st: dd, the head/drain group, tee, age.
    if [ "${st[0]:-1}" != 0 ]; then
      echo "NOTE: a slice that will not read can come from another copy of the unit: see --slices-from and --swap-tapes in --help." >&2
      die_io "reading tape file $tpos failed" "$WORK/dd.err" "$WORK"
    fi
    if [ "${st[3]:-1}" != 0 ]; then
      # age's own complaint about the slice (a damaged chunk, a header no key
      # opens) is what the heir needs to read, even when dar, seeing the slice
      # cut short, has already died of it. Only an age that failed to WRITE
      # (dar gone first) hands the word to dar.
      if ! grep -E 'decrypt|authenticat|no identity matched|header|MAC' "$WORK/age.err" >/dev/null 2>&1 &&
        ! kill -0 "$DAR_PID" 2>/dev/null; then
        die_dar "$destdir"
        return 0
      fi
      cat "$WORK/age.err" >&2 || true
      if grep 'no identity matched' "$WORK/age.err" >/dev/null 2>&1; then
        die_slice_keys "$num"
      fi
      die "slice $num (tape file $tpos) did not decrypt: a key matched it, so its bytes are
       damaged. Files already restored to $destdir from this unit may be incomplete.
$(replace_hint)"
    fi
    if [ "${st[1]:-1}" != 0 ] || [ "${st[2]:-1}" != 0 ]; then
      if ! kill -0 "$DAR_PID" 2>/dev/null; then
        die_dar "$destdir"
        return 0
      fi
      die "slice $num (tape file $tpos) could not be passed to dar (status ${st[*]})"
    fi
    CUR=$((tpos + 1))
    got=$(awk '{print $1}' "$WORK/slice.sha")
    if [ "$got" != "$sha" ]; then
      die "slice $num checksum MISMATCH (expected ${sha:0:16}…, got ${got:0:16}…)
       The slice does not match the front index. dar has already taken what
       decrypted of it: do not trust the files restored to $destdir.
$(replace_hint)"
    fi
    info "  checksum verified against front index"
  done

  local rc=0
  wait "$DAR_PID" 2>/dev/null || rc=$?
  DAR_PID=""
  if [ "$rc" != 0 ] && grep 'do not correspond to the same data' "$WORK/extract.log" >/dev/null 2>&1; then
    STREAM_REFUSED=1
    return 0
  fi
  dar_result "$WORK/extract.log" "$rc" "$destdir"
}

# Read, check and decrypt the slices listed in $1 as restore.N.dar, one
# encrypted slice on disk at a time, into directory $2 (default SCRATCH). A
# slice that fails stops the run, unless a replacement was asked for
# (SALVAGE): then it is looked for, by its sha256, in the --slices-from
# directories, and one not found is written to the file $3 (number | tape file
# | bytes | sha256) for the second tape, and the rest are read on.
spool_slices() { # <slices file> [<directory> [<file for the slices not found>]]
  local list=$1 into=${2:-$SCRATCH} missing=${3:-} num tpos size sha actual i=0 n
  [ -z "$missing" ] || : >"$missing"
  n=$(wc -l <"$list")
  while IFS='|' read -r num tpos size sha; do
    i=$((i + 1))
    info "Slice $num ($i/$n) — tape file $tpos"
    if [ "$SALVAGE" = 1 ]; then
      if read_slice_checked "$tpos" "$size" "$sha" "$SCRATCH/slice.enc"; then
        info "  checksum verified against front index"
        decrypt_slice "$SCRATCH/slice.enc" "$into/restore.$num.dar" "$num"
        rm -f "$SCRATCH/slice.enc"
      else
        info "  slice $num (tape file $tpos) $SLICE_WHY"
        if dir_find "$num" "$size" "$sha"; then
          info "  taking slice $num from $DIR_HIT (its sha256 is the one the front index lists)"
          decrypt_slice "$DIR_HIT" "$into/restore.$num.dar" "$num" "$size"
        else
          echo "$num|$tpos|$size|$sha" >>"$missing"
          info "  no copy of slice $num in the --slices-from directories"
        fi
      fi
      continue
    fi
    read_tape_raw "$tpos" "$SCRATCH/slice.enc"
    truncate -s "$size" "$SCRATCH/slice.enc"
    actual=$(sha256sum "$SCRATCH/slice.enc" | awk '{print $1}')
    [ "$actual" = "$sha" ] ||
      die "slice $num checksum MISMATCH (expected ${sha:0:16}…, got ${actual:0:16}…)
$(replace_hint)"
    info "  checksum verified against front index"
    decrypt_slice "$SCRATCH/slice.enc" "$into/restore.$num.dar" "$num"
    rm -f "$SCRATCH/slice.enc"
  done <"$list"
}

# The way 1.0.x restored, for a dar that cannot stream a unit safely: every
# slice decrypted to disk first, then dar reads them directly (not
# sequentially), which needs no catalogue. Needs room for the slices as well
# as the files, checked before the first slice is read.
spool_unit() { # <slices file> <destination>
  local list=$1 destdir=$2 bytes=0 num tpos size sha
  while IFS='|' read -r num tpos size sha; do
    bytes=$((bytes + size))
  done <"$list"
  check_space "$destdir" "$SCRATCH_PARENT" "$bytes" "$((bytes + BLOCK))"
  SCRATCH="$(mktemp -d "$SCRATCH_PARENT/.tapectl-restore.XXXXXX")" ||
    die "cannot create a scratch directory in $SCRATCH_PARENT"
  spool_slices "$list"
  local -a dar_opts=(-O -Q -N)
  local rc=0
  [ "$OVERWRITE" = 0 ] || dar_opts+=(-w)
  dar -x "$SCRATCH/restore" -R "$(abs_dir "$destdir")" "${dar_opts[@]}" >"$WORK/extract.log" 2>&1 </dev/null || rc=$?
  rm -rf "$SCRATCH"
  SCRATCH=""
  dar_result "$WORK/extract.log" "$rc" "$destdir"
}

# Restore one unit the best way this dar allows. Streaming needs the unit's
# isolated catalogue from the envelope with dar 2.7.9 or later (sliced
# sequential reading with a catalogue works from there), or no catalogue with
# dar 2.7.21 or later (which fixed reading without one). A catalogue dar
# refuses (#419) is dropped, and anything else is restored from slices
# decrypted to disk first.
restore_unit_plain() { # <slices file> <destination> <unit> <version>
  local list=$1 destdir=$2 unit=$3 ver=$4 cat
  cat=$(catalogue_path "$CHOSEN_ENV" "$unit" "$ver")
  if [ -n "$cat" ] && dar_at_least 2.7.9; then
    stream_unit "$list" "$destdir" "$cat"
    [ "$STREAM_REFUSED" = 1 ] || return 0
    echo "NOTE: dar refused the envelope's catalogue for '$unit': it belongs to another" >&2
    echo "      staging run of the same snapshot (#419). Nothing was written; restoring" >&2
    echo "      '$unit' without it." >&2
  fi
  if dar_at_least 2.7.21; then
    stream_unit "$list" "$destdir" ""
    [ "$STREAM_REFUSED" = 0 ] || die "dar refused to read '$unit' — see its message above"
    return 0
  fi
  info "dar ${DAR_VERSION:-(unknown version)} cannot stream '$unit' safely without its catalogue;"
  info "  decrypting the slices to disk first (dar 2.7.21 or later would stream it)"
  spool_unit "$list" "$destdir"
}

# One unit, whichever way: streamed, from slices on disk, or (when a
# replacement slice was asked for) from slices on disk with the bad ones
# replaced. UNIT_OK says whether dar named files it could not restore.
restore_unit() { # <slices file> <destination> <unit> <version>
  local list=$1 destdir=$2 unit=$3 ver=$4
  UNIT_NAME=$unit
  UNIT_VER=$ver
  UNIT_OK=1
  UNIT_UUID=$(unit_uuid_of "$CHOSEN_ENV" "$unit" "$ver")
  if [ "$SALVAGE" = 1 ]; then
    salvage_unit "$list" "$destdir" "$unit" "$ver"
    return 0
  fi
  restore_unit_plain "$list" "$destdir" "$unit" "$ver"
  if [ "$UNIT_OK" = 1 ]; then
    DONE_LINES+=("Unit '$unit' restored to: $destdir")
  fi
}

# The envelope --restore uses (walk_envelopes calls this for each that
# opens): the first that lists every unit asked for (#127: an escrow key opens
# them all, and the first may be another tenant's), or with --all the one
# that lists the most units — for an operator or escrow key, the operator
# envelope, which lists every unit on the tape.
RESTORE_UNITS=()
RESTORE_ALL=0
CHOSEN_ENV=""
CHOSEN_COUNT=0
CHOSEN_KEY=""
CHOSEN_POS=""
pick_for_restore() { # <position>
  local pos=$1 m="$WORK/opened.$1/MANIFEST.toml" u count
  [ -f "$m" ] || return 0
  if [ "$RESTORE_ALL" = 1 ]; then
    count=$(awk '__AWK_UNIT_LIST__' "$m" | wc -l)
    if [ -z "$CHOSEN_ENV" ] || [ "$count" -gt "$CHOSEN_COUNT" ]; then
      CHOSEN_ENV="$WORK/opened.$pos" CHOSEN_COUNT=$count CHOSEN_KEY=$ENV_KEY CHOSEN_POS=$pos
    fi
    return 0
  fi
  for u in ${RESTORE_UNITS[@]+"${RESTORE_UNITS[@]}"}; do
    if ! manifest_has_unit "$m" "$u"; then
      info "Envelope at file $pos decrypts but does not list '$u'; continuing..."
      return 0
    fi
  done
  CHOSEN_ENV="$WORK/opened.$pos" CHOSEN_KEY=$ENV_KEY CHOSEN_POS=$pos
  return 1
}

# A restore that names no unit and no --all restores the one unit the first
# envelope its key opens lists, when it lists exactly one (the default is
# deliberate). The same key may open more envelopes that list more units, and
# an heir who sees RESTORE COMPLETE would take the tape for that one unit. So,
# before a single slice is read, the envelopes after the one used are looked
# at, only to count what they list besides it. They all come before every
# slice, so this moves the tape forward only; each is dropped once counted.
LONE_UNIT=""
OTHER_ENVELOPES=0
OTHER_UNITS=0
OTHER_BIGGEST=0
count_other_envelope() { # <position>
  local pos=$1 m="$WORK/opened.$1/MANIFEST.toml" n
  if [ -f "$m" ]; then
    OTHER_ENVELOPES=$((OTHER_ENVELOPES + 1))
    awk '__AWK_UNIT_LIST__' "$m" >"$WORK/listed.txt"
    cat "$WORK/listed.txt" >>"$WORK/other_units.txt"
    n=$(wc -l <"$WORK/listed.txt")
    [ "$n" -le "$OTHER_BIGGEST" ] || OTHER_BIGGEST=$n
  fi
  rm -rf "$WORK/opened.$pos"
  return 0
}

survey_other_envelopes() { # <the unit being restored>
  local unit=$1 need free
  # The survey reads envelopes the restore itself would not have read, so it
  # must not be what fills a small work directory: the largest one after the
  # envelope in use is read whole and, if a key opens it, decrypted. Without
  # room for that the survey is skipped, whatever --no-space-check says.
  need=$(awk -F'|' -v after="$CHOSEN_POS" -v blk="$BLOCK" '
    ($2 == "tenant_envelope" || $2 == "operator_envelope") && $1 + 0 > after + 0 && $3 ~ /^[0-9]+$/ {
      if ($3 + 0 > max) max = $3 + 0
    }
    END { if (max > 0) printf "%.0f\n", 2 * max + blk }
  ' "$FILES_TXT")
  [ -n "$need" ] || return 0
  free=$(free_bytes "$WORK")
  if is_uint "$free" && ! awk -v have="$free" -v want="$need" 'BEGIN { exit (have + 0 >= want + 0) ? 0 : 1 }'; then
    info "Not looking at the other envelopes: the work directory has $(size_str "$free") free and the"
    info "  largest needs about $(size_str "$need") (set TMPDIR to a larger disk to see what else this key opens)."
    return 0
  fi
  LONE_UNIT=$unit
  : >"$WORK/other_units.txt"
  info "Looking at the other envelopes this key opens, to tell you at the end what else is on this tape..."
  walk_envelopes count_other_envelope "$CHOSEN_POS"
  OTHER_UNITS=$(U="$unit" awk '$0 != ENVIRON["U"] && !seen[$0]++' "$WORK/other_units.txt" | wc -l)
}

# Said just before RESTORE COMPLETE, and only when the survey found units the
# restore did not cover.
note_other_units() {
  if [ -z "$LONE_UNIT" ] || [ "$OTHER_UNITS" -eq 0 ]; then
    return 0
  fi
  echo ""
  info "NOTE: only '$LONE_UNIT' was restored: no --unit or --all said what to restore, so the"
  info "      first envelope this key opened decided. This key opened $OTHER_ENVELOPES other envelope(s)"
  info "      on this tape, which list $OTHER_UNITS other unit(s) besides it."
  info "      See what each lists:   $0 --list $(keys_args)"
  info "      Restore every unit of the envelope that lists the most ($OTHER_BIGGEST units here), each into"
  info "      --to/UNIT (use another, empty directory):"
  info "        $0 --restore $(keys_args) --all --to /another/destination"
}

# Pick the slices of one unit (one version) from the manifest into $3, checked
# against the file map before any slice is read. Sets PICKED_VERSION and
# PICKED_BYTES.
PICKED_VERSION=""
PICKED_BYTES=0
pick_slices() { # <manifest> <unit> <out> [version]
  local manifest=$1 unit=$2 out=$3 want=${4:-} num tpos meb msha isize ihash
  # A volume can carry the SAME unit more than once — two snapshot versions,
  # two stage sets. Collecting every matching [[units]] block concatenated
  # both versions' slices and handed the mix to dar (issue #131). So: buffer
  # per block and emit exactly one version's slices.
  awk -v unit="$unit" -v want="$want" '__AWK_SELECT_VERSION__' "$manifest" >"$WORK/picked.txt" 2>"$WORK/picked_version.txt"
  PICKED_VERSION=$(tr -d ' \n' <"$WORK/picked_version.txt" 2>/dev/null)
  if [ ! -s "$WORK/picked.txt" ]; then
    if [ -n "$want" ]; then
      die "unit '$unit' has no version $want on this volume
       --find-envelope --key KEYFILE lists the versions here; --info is
       keyless and cannot see them. Omit --version for the newest."
    fi
    die "no slices found for unit '$unit'"
  fi
  : >"$out"
  PICKED_BYTES=0
  while IFS='|' read -r num tpos meb msha; do
    isize=$(file_size_at "$tpos" "$FILES_TXT")
    ihash=$(file_hash_at "$tpos" "$FILES_TXT")
    if [ -z "$isize" ] || [ -z "$ihash" ]; then
      die "slice $num of '$unit' (tape file $tpos) has no data_slice entry in the file map — refusing to trust the envelope manifest alone"
    fi
    require_uint "front_index_size(@$tpos)" "$isize"
    if [ -n "$meb" ] && [ "$meb" != "$isize" ]; then
      die "slice $num of '$unit' size mismatch: envelope manifest says $meb bytes, file map says $isize bytes — tape may be tampered or damaged"
    fi
    if [ -n "$msha" ] && [ "$msha" != "$ihash" ]; then
      die "slice $num of '$unit' hash mismatch: envelope manifest and file map disagree — tape may be tampered or damaged"
    fi
    echo "$num|$tpos|$isize|$ihash" >>"$out"
    PICKED_BYTES=$((PICKED_BYTES + isize))
  done <"$WORK/picked.txt"
}

do_restore() {
  local destdir=$1 want_version=${2:-} scratch_parent=${3:-}

  [ -n "$scratch_parent" ] || scratch_parent=$destdir
  check_destination "$destdir" "$scratch_parent"
  dar_check
  mkdir -p "$destdir" "$scratch_parent"
  SCRATCH_PARENT=$scratch_parent
  RESTORE_TO=$destdir
  set_age_ids

  establish_files
  [ "$SALVAGE" = 0 ] || salvage_init

  # Step 1: the envelope (see pick_for_restore). Each key is tried on its own
  # at each position (#288).
  walk_envelopes pick_for_restore
  local wanted="${RESTORE_UNITS[*]-}"
  [ -n "$CHOSEN_ENV" ] || die_no_envelope "${wanted// /, }"
  info "Decrypted envelope at file $CHOSEN_POS"
  info "  opened with key $CHOSEN_KEY"
  local manifest="$CHOSEN_ENV/MANIFEST.toml"
  [ -f "$manifest" ] || die "envelope missing MANIFEST.toml"

  # Step 2: which units.
  local -a unit_names=() targets=()
  local uname
  while IFS= read -r uname; do
    unit_names+=("$uname")
  done < <(awk '__AWK_UNIT_LIST__' "$manifest")
  [ ${#unit_names[@]} -gt 0 ] || die "no units in manifest"

  if [ "$RESTORE_ALL" = 1 ]; then
    targets=("${unit_names[@]}")
  elif [ ${#RESTORE_UNITS[@]} -gt 0 ]; then
    targets=("${RESTORE_UNITS[@]}")
  elif [ ${#unit_names[@]} -eq 1 ]; then
    targets=("${unit_names[0]}")
    survey_other_envelopes "${unit_names[0]}"
  else
    echo "Units in this envelope:"
    for uname in "${unit_names[@]}"; do echo "  - $uname"; done
    die "multiple units found — specify one with --unit NAME (repeat it for several), or --all"
  fi
  if [ -n "$want_version" ] && [ ${#targets[@]} -gt 1 ]; then
    die "--version applies to one unit; give a single --unit with it"
  fi
  if [ ${#RESTORE_PATHS[@]} -gt 0 ]; then
    [ ${#targets[@]} -eq 1 ] || die "--path needs exactly one unit: give one --unit with it"
    info "Parsing slices for unit: ${targets[0]}"
    pick_slices "$manifest" "${targets[0]}" "$WORK/slices.0" "$want_version"
    info "Restoring from '${targets[0]}' snapshot version ${PICKED_VERSION:-unknown}"
    restore_paths "$WORK/slices.0" "$CHOSEN_ENV" "${targets[0]}" "$PICKED_VERSION" "$destdir" "$scratch_parent"
    restore_end
    return 0
  fi

  # Step 3: each unit's slices, checked against the file map, before any is
  # read. Always say which version: a volume can hold several, and silently
  # picking one is how an heir restores the wrong data believing it current.
  local i total=0 first
  local -a order_lines=() versions=()
  for ((i = 0; i < ${#targets[@]}; i++)); do
    info "Parsing slices for unit: ${targets[$i]}"
    pick_slices "$manifest" "${targets[$i]}" "$WORK/slices.$i" "$want_version"
    versions[$i]=$PICKED_VERSION
    info "Restoring '${targets[$i]}' snapshot version ${PICKED_VERSION:-unknown} ($(wc -l <"$WORK/slices.$i") slice(s))"
    total=$((total + PICKED_BYTES))
    first=$(head -n 1 "$WORK/slices.$i" | cut -d'|' -f2)
    order_lines+=("$first|$i")
  done

  # Step 4: room for all of it, measured once, before any slice is read.
  check_space "$destdir" "$destdir" "$total" 0

  # Step 5: the units in tape order, one forward pass. One unit goes straight
  # into --to; several go to --to/UNIT each.
  local line dest_i
  while IFS= read -r line; do
    i=${line#*|}
    if [ ${#targets[@]} -eq 1 ]; then
      dest_i=$destdir
    else
      dest_i="$destdir/${targets[$i]}"
      mkdir -p "$dest_i"
    fi
    info "Extracting '${targets[$i]}' to $dest_i ..."
    restore_unit "$WORK/slices.$i" "$dest_i" "${targets[$i]}" "${versions[$i]}"
  done < <(printf '%s\n' "${order_lines[@]}" | sort -t'|' -k1,1n)

  finish_pending
  restore_end
}

# ---- the envelope's dar catalogues (--list, --path; #418) ----

# Every envelope carries, under catalogs/, the isolated dar catalogue of each
# unit it lists, named <first 8 of the unit uuid>_v<snapshot version>. With
# it dar lists a unit's files, and says which slice holds each, without
# reading the tape. Prints the catalogue's base path, or dies.
catalogue_path() { # <envelope dir> <unit> <version>: the base path, or nothing
  local env=$1 unit=$2 ver=$3 uuid
  uuid=$(awk -v unit="$unit" -v want="$ver" '__AWK_UNIT_UUID__' "$env/MANIFEST.toml")
  [ -n "$uuid" ] || return 0
  [ -f "$env/catalogs/${uuid:0:8}_v$ver.1.dar" ] || return 0
  echo "$env/catalogs/${uuid:0:8}_v$ver"
}

catalogue_for() { # <envelope dir> <unit> <version>
  local env=$1 unit=$2 ver=$3 uuid base
  uuid=$(awk -v unit="$unit" -v want="$ver" '__AWK_UNIT_UUID__' "$env/MANIFEST.toml")
  [ -n "$uuid" ] || die "the envelope lists no uuid for unit '$unit' version $ver"
  base="$env/catalogs/${uuid:0:8}_v$ver"
  [ -f "$base.1.dar" ] || die "the envelope carries no dar catalogue for '$unit' version $ver
       (expected catalogs/${uuid:0:8}_v$ver.1.dar). --restore without --path
       restores the whole unit, which needs no catalogue."
  echo "$base"
}

# The units --list and --path work on: those named, else the envelope's only
# one, else (for --list) every unit in it.
units_of_envelope() { # <manifest>
  awk '__AWK_UNIT_LIST__' "$1"
}

do_list() {
  local want_version=${1:-} manifest u base
  set_age_ids
  dar_check
  establish_files
  # No unit named: the envelope listing the most units, as --all chooses
  # (with an operator or escrow key, the operator envelope: every unit).
  [ ${#RESTORE_UNITS[@]} -gt 0 ] || RESTORE_ALL=1
  walk_envelopes pick_for_restore
  local wanted="${RESTORE_UNITS[*]-}"
  [ -n "$CHOSEN_ENV" ] || die_no_envelope "${wanted// /, }"
  info "Decrypted envelope at file $CHOSEN_POS"
  info "  opened with key $CHOSEN_KEY"
  manifest="$CHOSEN_ENV/MANIFEST.toml"
  [ -f "$manifest" ] || die "envelope missing MANIFEST.toml"
  local -a targets=()
  if [ ${#RESTORE_UNITS[@]} -gt 0 ]; then
    targets=("${RESTORE_UNITS[@]}")
  else
    while IFS= read -r u; do targets+=("$u"); done < <(units_of_envelope "$manifest")
  fi
  for u in "${targets[@]}"; do
    pick_slices "$manifest" "$u" "$WORK/list.slices" "$want_version"
    base=$(catalogue_path "$CHOSEN_ENV" "$u" "$PICKED_VERSION")
    echo ""
    if [ -z "$base" ]; then
      echo "=== $u, snapshot version $PICKED_VERSION — no dar catalogue in this envelope ==="
      echo "    (it can still be restored whole; --list and --path need the catalogue)"
      continue
    fi
    echo "=== $u, snapshot version $PICKED_VERSION — catalogs/${base##*/} ==="
    dar -l "$base" -N -Q || die "dar could not list the catalogue of '$u'"
  done
  echo ""
  echo "To restore one of these, give its path as listed:"
  echo "  $0 --restore $(keys_args) --unit UNIT --path PATH --to /your/destination"
}

# --restore --path: dar's catalogue says which slices hold each path and the
# directories above it (dar -l -T slice); those, and the last slice, which
# carries the archive's own catalogue, are read from the tape, checked,
# decrypted into scratch, and dar extracts just the paths from them.
RESTORE_PATHS=()
restore_paths() { # <slices file> <envelope dir> <unit> <version> <destination> <scratch parent>
  local list=$1 env=$2 unit=$3 ver=$4 destdir=$5 scr=$6 base p range lo hi last
  local -a g=()
  for p in "${RESTORE_PATHS[@]}"; do
    p=${p#/}
    p=${p#./}
    g+=(-g "$p")
  done
  base=$(catalogue_for "$env" "$unit" "$ver")
  dar -l "$base" -N -Q -Tslice "${g[@]}" >"$WORK/slicing.txt" 2>&1 ||
    die "dar could not read the catalogue of '$unit': $(tail -n 3 "$WORK/slicing.txt")"
  range=$(sed -n 's/.*slice range \[\([0-9-]*\)\].*/\1/p' "$WORK/slicing.txt" | head -n 1)
  if [ -z "$range" ]; then
    die "'$unit' version $ver holds nothing at: ${RESTORE_PATHS[*]}
       A path is relative to the unit's root, exactly as --list prints it:
         $0 --list $(keys_args) --unit '$unit'
       No slice has been read."
  fi
  lo=${range%-*}
  hi=${range#*-}
  require_uint slice_range "$lo"
  require_uint slice_range "$hi"
  last=$(tail -n 1 "$list" | cut -d'|' -f1)
  awk -F'|' -v lo="$lo" -v hi="$hi" -v last="$last" \
    '($1 + 0 >= lo + 0 && $1 + 0 <= hi + 0) || $1 == last' "$list" >"$WORK/path.slices"
  local total=0 n num tpos size sha
  n=$(wc -l <"$WORK/path.slices")
  while IFS='|' read -r num tpos size sha; do
    total=$((total + size))
  done <"$WORK/path.slices"
  info "'${RESTORE_PATHS[*]}' is in slice(s) $lo-$hi of $last; reading $n of them"
  check_space "$destdir" "$scr" "$total" "$total"
  UNIT_NAME=$unit
  UNIT_VER=$ver
  UNIT_OK=1
  UNIT_UUID=$(unit_uuid_of "$env" "$unit" "$ver")
  if [ "$SALVAGE" = 1 ]; then
    # The same slices, in a job of their own, with a bad one replaced.
    ensure_scratch
    local j=$JOB_N
    JOB_N=$((JOB_N + 1))
    JOB_KIND[$j]=unit
    JOB_DEST[$j]=$destdir
    JOB_UNIT[$j]=$unit
    JOB_VER[$j]=$ver
    JOB_UUID[$j]=$UNIT_UUID
    EXTRACT_G=("${g[@]}")
    mkdir -p "$SCRATCH/job.$j"
    spool_slices "$WORK/path.slices" "$SCRATCH/job.$j" "$WORK/job.$j.missing"
    if [ -s "$WORK/job.$j.missing" ] && [ "$SWAP_TAPES" = 1 ]; then
      PENDING+=("$j")
      finish_pending
    else
      job_run "$j"
    fi
    return 0
  fi
  SCRATCH="$(mktemp -d "$scr/.tapectl-restore.XXXXXX")" ||
    die "cannot create a scratch directory in $scr"
  spool_slices "$WORK/path.slices"
  local -a dar_opts=(-O -Q -N)
  local rc=0
  [ "$OVERWRITE" = 0 ] || dar_opts+=(-w)
  info "Extracting ${RESTORE_PATHS[*]} to $destdir ..."
  dar -x "$SCRATCH/restore" -R "$(abs_dir "$destdir")" "${g[@]}" "${dar_opts[@]}" >"$WORK/extract.log" 2>&1 </dev/null || rc=$?
  rm -rf "$SCRATCH"
  SCRATCH=""
  dar_result "$WORK/extract.log" "$rc" "$destdir"
  if [ "$UNIT_OK" = 1 ]; then
    DONE_LINES+=("${RESTORE_PATHS[*]} of '$unit' restored to: $destdir")
  fi
}

# ---- replacement slices and the damaged-file list (1.3.0; ADR-0012 items 101, 102) ----

# A slice that fails its ciphertext hash, or will not read, can be taken from
# another copy of the same unit. Copies are written from one staging, so the
# copy's slice has the same sha256_encrypted in its own front index, and that
# hash is the only thing a replacement is matched by: not its position, not its
# file name. --slices-from DIR offers slice files copied off another tape;
# --swap-tapes asks for another cartridge once this one has been read. Either
# one makes the restore keep each slice on disk in the scratch area, decrypted
# (as the older-dar fallback does), because a stream cannot take back a slice
# dar has already consumed.

# The absolute form of directory $1, with no trailing slash. dar prints the -R
# it was given word for word in its messages, so the damaged-file list is cut
# from an absolute root this script chose.
abs_dir() { # <directory>
  local d=$1
  case "$d" in
  /*) ;;
  *) d="$PWD/$d" ;;
  esac
  while [ "${#d}" -gt 1 ] && [ "${d%/}" != "$d" ]; do
    d=${d%/}
  done
  printf '%s' "$d"
}

# A text as sec 1.5 of the format escapes a path: a backslash is written as
# two, a TAB as \t, a newline as \n, a carriage return as \r, and nothing else
# changes. Character by character, so no escape is escaped twice.
escape_text() { # <text>
  local s=$1 out="" c
  while [ -n "$s" ]; do
    c=${s:0:1}
    s=${s:1}
    case "$c" in
    "\\") out="$out\\\\" ;;
    $'\t') out="$out\\t" ;;
    $'\n') out="$out\\n" ;;
    $'\r') out="$out\\r" ;;
    *) out="$out$c" ;;
    esac
  done
  printf '%s' "$out"
}

# The reverse, in one left-to-right scan (chained substitutions would turn the
# text of a backslash followed by t into a TAB). The result is in UNESC, not
# printed, because a path may end in a newline that $(...) would drop.
UNESC=""
unesc_line() { # <escaped text>
  local s=$1 out="" d
  while [[ $s == *\\* ]]; do
    out="$out${s%%\\*}"
    s=${s#*\\}
    d=${s:0:1}
    s=${s:1}
    case "$d" in
    t) out="$out"$'\t' ;;
    n) out="$out"$'\n' ;;
    r) out="$out"$'\r' ;;
    "\\") out="$out\\" ;;
    *) out="$out\\$d" ;;
    esac
  done
  UNESC="$out$s"
}

# The directories named by --slices-from must exist before any tape is read;
# asking for a replacement turns SALVAGE on.
check_slices_from() {
  local d
  for d in ${SLICES_FROM[@]+"${SLICES_FROM[@]}"}; do
    [ -d "$d" ] || die "--slices-from $d: not a directory. Nothing has been read from tape."
  done
  if [ ${#SLICES_FROM[@]} -gt 0 ] || [ "$SWAP_TAPES" = 1 ]; then
    SALVAGE=1
  fi
}

# What a failure of a slice says when no replacement was offered.
replace_hint() {
  cat <<'HINTEOF'
       Another copy of this unit holds the same slice, byte for byte: the front
       index of its tape lists the same sha256 for it. Run the same command
       again, with --overwrite if --to now holds part of the unit, and
         --slices-from DIR   a directory of slice files copied off another tape
                             (any names: a file is used only if its sha256 is the
                             one the front index here lists), or
         --swap-tapes        this script asks for another cartridge, once it has
                             read this one, and reads just the slices it needs.
       --help and File 1 say more.
HINTEOF
}

# The sha256 of the first $2 bytes of file $1, remembered for the run: the
# slices of one unit share a size, so every file in a directory is a candidate
# for each of them and is hashed once, not once per slice.
dir_sum() { # <file> <bytes>
  local key="$1|$2" sum
  sum=$(K="$key" awk -F'\t' '$1 == ENVIRON["K"] { print $2; exit }' "$WORK/dir.sums" 2>/dev/null || true)
  if [ -z "$sum" ]; then
    sum=$(head -c "$2" -- "$1" | sha256sum | awk '{print $1}')
    printf '%s\t%s\n' "$key" "$sum" >>"$WORK/dir.sums"
  fi
  printf '%s' "$sum"
}

# Is a slice with this sha256 in one of the --slices-from directories? A file
# is a candidate by its size alone (the slice's exact size, or that rounded up
# to the tape block as dd copies it off a tape) and is USED only if the sha256
# of its first <bytes> bytes is the one wanted. The file is left in DIR_HIT.
DIR_HIT=""
DIR_NOTED=""
dir_find() { # <slice number> <bytes> <sha256>
  local num=$1 size=$2 sha=$3 padded d f fsz
  DIR_HIT=""
  [ ${#SLICES_FROM[@]} -gt 0 ] || return 1
  padded=$(((size + BLOCK - 1) / BLOCK * BLOCK))
  for d in "${SLICES_FROM[@]}"; do
    for f in "$d"/* "$d"/.[!.]*; do
      [ -f "$f" ] || continue
      fsz=$(stat -c %s -- "$f" 2>/dev/null || true)
      [ "$fsz" = "$size" ] || [ "$fsz" = "$padded" ] || continue
      if [ "$(dir_sum "$f" "$size")" = "$sha" ]; then
        DIR_HIT=$f
        return 0
      fi
      case "$DIR_NOTED" in
      *"|$f|"*) ;;
      *)
        DIR_NOTED="$DIR_NOTED|$f|"
        echo "NOTE: $f has the size of slice $num but not its sha256: it is not used." >&2
        ;;
      esac
    done
  done
  return 1
}

# Read the slice at tape file $1 into $4, trimmed to its $2 bytes, and check it
# against the sha256 $3 the front index states. Never exits on a bad slice:
# returns 1 and says why in SLICE_WHY (a full disk still stops the run).
SLICE_WHY=""
read_slice_checked() { # <tape file> <bytes> <sha256> <out>
  local pos=$1 size=$2 sha=$3 out=$4 actual
  SLICE_WHY=""
  : >"$WORK/dd.err"
  if ! try_read_tape_raw "$pos" "$out"; then
    if grep -i 'no space left' "$WORK/dd.err" >/dev/null 2>&1; then
      die_io "reading tape file $pos failed" "$WORK/dd.err" "$(dirname "$out")"
    fi
    rm -f "$out"
    SLICE_WHY="would not read"
    return 1
  fi
  truncate -s "$size" "$out"
  actual=$(sha256sum "$out" | awk '{print $1}')
  if [ "$actual" != "$sha" ]; then
    rm -f "$out"
    SLICE_WHY="does not match the front index (sha256 ${actual:0:16}..., the index says ${sha:0:16}...)"
    return 1
  fi
}

# ---- the unit being restored, and what became of it ----

UNIT_NAME=""
UNIT_VER=""
UNIT_UUID=""
UNIT_OK=1
INCOMPLETE=()
DONE_LINES=()
RESTORE_TO=""
DAMAGED_NAME=.tapectl-damaged-files.txt

# The uuid of one version of a unit, from its envelope's manifest.
unit_uuid_of() { # <envelope dir> <unit> <version>
  awk -v unit="$2" -v want="$3" '__AWK_UNIT_UUID__' "$1/MANIFEST.toml"
}

# The first line of a damaged-file list: its magic, its format (1), then the
# unit's name (escaped as a path is), its uuid and its version, TAB-separated.
# A fill run uses a tape only if it lists the same unit uuid AND version.
damaged_header() { # <unit> <uuid> <version>
  printf '%s\t%s\tunit=%s\tuuid=%s\tversion=%s\n' '#tapectl-damaged-files' 1 "$(escape_text "$1")" "$2" "$3"
}

# One unit's note for the end of a run that did not get everything: how many
# files, where the list is, and the command that fills them from another copy.
damage_note() { # <count> <list file> <slices not available, or empty>
  local n=$1 list=$2 gone=${3:-} to=${RESTORE_TO:-DIR}
  if [ "${CHECK_ADDED:-0}" -eq 0 ]; then
    echo "Unit '$UNIT_NAME' (version $UNIT_VER): $n file(s) were not restored."
  elif [ "$CHECK_ADDED" -eq "$n" ]; then
    echo "Unit '$UNIT_NAME' (version $UNIT_VER): $n file(s) do not match the sha256 the index records for them, or are missing."
    echo "  dar named none of them: it checks a file with a short checksum, and this script checks it with a hash."
  else
    echo "Unit '$UNIT_NAME' (version $UNIT_VER): $n file(s) were not restored intact."
    echo "  $CHECK_ADDED of them were written without a dar error but do not match the sha256 the index records"
    echo "  for them, or are missing: dar checks a file with a short checksum, and this script checks it with a hash."
  fi
  if [ -n "$gone" ]; then
    echo "  Slice(s) not available: $gone. The files in them are among those listed."
  fi
  echo "  They are listed, one path per line, in"
  echo "    $list"
  echo "  Another copy of this same unit and version holds them. Load that tape and run ITS"
  echo "  RESTORE.sh (File 2 of that tape), which restores only those files and overwrites only them:"
  echo "    ./RESTORE.sh --restore $(keys_args) --unit $(printf '%q' "$UNIT_NAME") --fill-from $(printf '%q' "$list") --to $(printf '%q' "$to")"
  echo "  A tape that does not hold this unit at this version is refused: no other version is used."
}

# Write the list of files dar could not restore into the unit's directory and
# note the unit as incomplete.
record_damage() { # <destination> <file of escaped paths> <slices not available, or empty>
  local destdir=$1 paths=$2 gone=${3:-} n list
  list="$destdir/$DAMAGED_NAME"
  n=$(wc -l <"$paths")
  {
    damaged_header "$UNIT_NAME" "$UNIT_UUID" "$UNIT_VER"
    cat "$paths"
  } >"$list"
  UNIT_OK=0
  INCOMPLETE+=("$(damage_note "$n" "$list" "$gone")")
}

# A list left by an earlier run of this very unit and version is out of date
# once the unit restores whole: drop it, so a clean directory has no list.
clear_stale_list() { # <destination>
  local list="$1/$DAMAGED_NAME" first=""
  [ -f "$list" ] || return 0
  IFS= read -r first <"$list" || true
  case "$first" in
  *$'\t'"uuid=$UNIT_UUID"$'\t'"version=$UNIT_VER")
    rm -f "$list"
    info "Removed $list: this restore of '$UNIT_NAME' left nothing damaged"
    ;;
  esac
}

# ---- the restored files against their recorded sha256 (1.3.0; ADR-0012 item 103) ----

# dar checks each file it restores with a short checksum, not a hash: two equal
# byte flips can cancel, and a damaged file comes back with no error. A tape
# written since 1.3.0 records the sha256 of every regular file in its tenant
# envelope's index/paths.tsv, and once a unit is extracted each file is read
# back from --to and checked against it with sha256sum. The envelope is on disk
# already and the files are the ones just restored: the tape is not touched. A
# file that does not match, or a listed file that is not there, goes on the
# damaged-file list, so --fill-from can take it from another copy. A file with
# no recorded hash is not checked, and a tape with no index (written before
# 1.3.0) is not checked at all; both are said. --no-file-check skips it.
FILE_CHECK=1
CHECK_NOTED=" "
CHECK_INDEX=""
CHECK_ANY_INDEX=0
CHECK_BAD="$WORK/check.bad"
CHECK_FAILED=0
CHECK_UNNAMED=0
CHECK_ADDED=0

# True when $1 was said before in this run; else remembers it and is false.
said_before() { # <key>
  case "$CHECK_NOTED" in
  *" $1 "*) return 0 ;;
  esac
  CHECK_NOTED="$CHECK_NOTED$1 "
  return 1
}

# A path as the lists print it: one line, with no control character a
# terminal would act on (the lists already hold a TAB, newline or carriage
# return as \t, \n and \r).
printable_path() { # <escaped path>
  printf '%s' "$1" | LC_ALL=C tr -d '\000-\010\013\014\016-\037\177'
}

# The paths.tsv, among the envelopes this run has open, that lists this
# version of this unit: the envelope in use first, then the others the walk
# opened (an escrow or operator key restores from the operator envelope, which
# has no paths.tsv; the tenant envelope that lists the unit was opened before
# it and is still on disk). Sets CHECK_INDEX ("" when there is none) and
# CHECK_ANY_INDEX (1 when any open envelope carries an index member at all).
index_for_unit() { # <unit> <version> <unit uuid>
  local d
  CHECK_INDEX=""
  CHECK_ANY_INDEX=0
  for d in "$CHOSEN_ENV" "$WORK"/opened.*; do
    [ -d "$d" ] || continue
    if [ -f "$d/index/units.tsv" ] || [ -f "$d/index/paths.tsv" ]; then
      CHECK_ANY_INDEX=1
    fi
    [ -z "$CHECK_INDEX" ] || continue
    [ -f "$d/index/paths.tsv" ] && [ -f "$d/index/units.tsv" ] || continue
    if CHK_UNIT="$1" CHK_VER="$2" CHK_UUID="$3" awk '__AWK_INDEX_COMMON__ __AWK_FILE_CHECK_LISTED__' "$d/index/units.tsv"; then
      CHECK_INDEX="$d/index/paths.tsv"
    fi
  done
}

# Check the regular files of UNIT_NAME, version UNIT_VER, that were restored
# into $1, against the index. With $2, a file of escaped paths, only those
# (a fill). With --path, only what was asked for. The files that fail are left
# in $CHECK_BAD, one per line and escaped as the index escapes a path; their
# number is CHECK_FAILED, and CHECK_UNNAMED counts failures that sha256sum
# reported and this script could not name.
file_check() { # <destination> [<file of escaped paths to check>]
  local destdir=$1 only=${2:-} p whole=0
  local rows=0 files=0 checked=0 nspecial=0 nohash=0 named=0 unmatched=0 stray=0
  : >"$CHECK_BAD"
  CHECK_FAILED=0
  CHECK_UNNAMED=0
  if [ "$FILE_CHECK" = 0 ]; then
    said_before off || {
      info "File check skipped (--no-file-check): the restored files are checked by dar's own checksum only,"
      info "  not against the sha256 recorded for them in the index."
    }
    return 0
  fi
  if [ -n "$only" ] && [ ! -s "$only" ]; then
    return 0
  fi
  index_for_unit "$UNIT_NAME" "$UNIT_VER" "$UNIT_UUID"
  if [ -z "$CHECK_INDEX" ]; then
    if [ "$CHECK_ANY_INDEX" = 0 ]; then
      said_before no-index || {
        info "File check: this tape carries no index (a tape written before version 1.3.0 has none), so the"
        info "  restored files are checked by dar's own checksum only, not against a recorded sha256."
      }
    else
      info "File check: no envelope this key opened lists '$(visible_text "$UNIT_NAME")' version $UNIT_VER with its files, so they are"
      info "  not checked against a recorded sha256 (the key of the tenant who owns the unit opens the envelope that does)."
    fi
    return 0
  fi
  # --path: dar restored these paths and, for a directory, what is under it.
  if [ -z "$only" ] && [ ${#RESTORE_PATHS[@]} -gt 0 ]; then
    : >"$WORK/check.only"
    for p in "${RESTORE_PATHS[@]}"; do
      p=${p#/}
      p=${p#./}
      while [ "${p%/}" != "$p" ]; do
        p=${p%/}
      done
      if [ -z "$p" ]; then
        whole=1
        break
      fi
      printf '%s\n' "$(escape_text "$p")" >>"$WORK/check.only"
    done
    [ "$whole" = 1 ] || only="$WORK/check.only"
  fi
  if ! CHK_UNIT="$UNIT_NAME" CHK_VER="$UNIT_VER" CHK_ONLY="$only" CHK_SUMS="$WORK/check.sums" \
    CHK_SPECIAL="$WORK/check.special" CHK_COUNTS="$WORK/check.counts" \
    awk '__AWK_INDEX_COMMON__ __AWK_FILE_CHECK_LIST__' "$CHECK_INDEX"; then
    echo "WARNING: this script could not make the list of files to check from the index in the envelope (it has no" >&2
    echo "         unit, version or path column, or the work directory $WORK is full; TMPDIR moves it):" >&2
    echo "         the restored files of '$(visible_text "$UNIT_NAME")' are not checked against a recorded sha256." >&2
    return 0
  fi
  read -r rows files checked nspecial nohash <"$WORK/check.counts" || true
  if [ "$checked" -eq 0 ]; then
    if [ "$nohash" -gt 0 ]; then
      info "File check: the index records no sha256 for the $nohash file(s) of '$(visible_text "$UNIT_NAME")' version $UNIT_VER: not checked."
    fi
    return 0
  fi
  # Names that sha256sum -c takes as they stand: one pass, quiet, so only a
  # failure is printed (a mismatch, or a file it could not open).
  if [ "$checked" -gt "$nspecial" ]; then
    (cd "$destdir" && sha256sum -c --quiet) <"$WORK/check.sums" >"$WORK/check.out" 2>"$WORK/check.err" || true
    CHK_REPORT="$WORK/check.out" CHK_BAD="$WORK/check.named" CHK_COUNTS="$WORK/check.named.counts" \
      awk '__AWK_FILE_CHECK_FAILED__' "$WORK/check.sums"
    read -r named unmatched stray <"$WORK/check.named.counts" || true
    cat "$WORK/check.named" >>"$CHECK_BAD"
    if [ "$unmatched" -gt 0 ] || [ "$stray" -gt 0 ]; then
      CHECK_UNNAMED=$((unmatched + stray))
      echo "WARNING: sha256sum's report could not be matched to the files for $CHECK_UNNAMED line(s):" >&2
      head -n 5 "$WORK/check.out" "$WORK/check.err" 2>/dev/null | sed 's/^/         /' >&2 || true
    fi
  fi
  # A name with a backslash, a newline or a carriage return, one file at a
  # time (sha256sum prints and reads such a name in an escaped form that
  # differs between versions; hashing the file from standard input has no name
  # to escape).
  if [ "$nspecial" -gt 0 ]; then
    local want esc got
    while IFS=$'\t' read -r want esc; do
      unesc_line "$esc"
      got=$({ sha256sum <"$destdir/$UNESC"; } 2>/dev/null) || got=""
      got=${got%% *}
      [ "$got" = "$want" ] || printf '%s\n' "$esc" >>"$CHECK_BAD"
    done <"$WORK/check.special"
  fi
  CHECK_FAILED=$(wc -l <"$CHECK_BAD")
  local what="'$(visible_text "$UNIT_NAME")' version $UNIT_VER"
  if [ "$CHECK_FAILED" -eq 0 ] && [ "$CHECK_UNNAMED" -eq 0 ]; then
    if [ "$nohash" -gt 0 ]; then
      info "File check: $checked restored file(s) of $what match the sha256 recorded in the index ($nohash more not checked: no sha256 recorded)"
    else
      info "File check: $checked restored file(s) of $what match the sha256 recorded in the index"
    fi
    return 0
  fi
  info "File check: $CHECK_FAILED of $checked file(s) of $what do NOT match their recorded sha256, or are missing:"
  head -n 10 "$CHECK_BAD" | while IFS= read -r p; do
    info "    $(printable_path "$p")"
  done
  if [ "$CHECK_FAILED" -gt 10 ]; then
    info "    ... and $((CHECK_FAILED - 10)) more (all are in the damaged-file list)"
  fi
}

# What a finished dar extraction (log $1, exit status $2, into $3) means.
# Complete: status 0 and nothing skipped. Damage: status 5, which dar uses when
# a file could not be restored, and it named the files (a failed CRC, a slice
# cut short): they go in the unit's list and the run goes on to the next unit,
# ending INCOMPLETE. $4 is the slices that were not available: with a slice
# missing, a file dar skipped (user choice) is a file that was in it, not one
# that was already there. Anything else is fatal, as it has always been.
# Either way the restored files are then checked against the index
# (file_check): the files dar named ($WORK/damaged.new, possibly none) and the
# ones the check finds go in one list.
dar_result() { # <log> <status> <destination> [slices not available]
  local log=$1 rc=$2 destdir=$3 gone=${4:-} errs=0 skipped=0 unmapped=0 failed=0 skips=0 n
  UNIT_OK=1
  cat "$log"
  if [ "$rc" != 0 ] && grep -i 'no space left' "$log" >/dev/null 2>&1; then
    die_io "dar extraction failed" "$log" "$destdir"
  fi
  [ -z "$gone" ] || skips=1
  : >"$WORK/damaged.new"
  if [ "$rc" = 0 ] && [ "$skips" = 0 ]; then
    die_if_dar_skipped "$log" "$destdir"
    finish_damage "$destdir" ""
    return 0
  fi
  if [ "$rc" != 0 ] && [ "$rc" != 5 ]; then
    die "dar extraction failed (exit $rc) — dar's own message is above"
  fi
  # Status 5 is also what dar says when the disk, not the archive, refused a
  # file. Those are not damaged files: a fill from another tape would meet the
  # same refusal. They stop the run, as ever.
  if [ "$rc" = 5 ] && grep -E ' : (Permission denied|Operation not permitted|Read-only file system|No such file or directory|Not a directory|Is a directory|File name too long|Too many open files|Too many levels of symbolic links|Disk quota exceeded|File exists|Input/output error|Cannot allocate memory)$' "$log" >/dev/null 2>&1; then
    die "dar extraction failed (exit $rc): the file system refused a file — dar's own message is above.
       This is not a damaged archive, so no list of damaged files is written."
  fi
  DAMAGED_ROOT="$(abs_dir "$destdir")" DAMAGED_SKIPS="$skips" DAMAGED_COUNTS="$WORK/damaged.counts" \
    awk '__AWK_DAR_DAMAGED__' "$log" >"$WORK/damaged.new"
  read -r errs skipped unmapped failed <"$WORK/damaged.counts" || true
  n=$(wc -l <"$WORK/damaged.new")
  if [ "$n" -eq 0 ]; then
    if [ "$rc" != 0 ]; then
      die "dar extraction failed (exit $rc) — dar's own message is above"
    fi
    echo "NOTE: slice(s) $gone were not available, yet dar names no file it could not restore." >&2
    finish_damage "$destdir" ""
    return 0
  fi
  if [ "$unmapped" -gt 0 ] || { [ "$rc" = 5 ] && [ "$errs" -ne "$failed" ]; }; then
    echo "WARNING: dar reported $failed file(s) failed and $skipped skipped, and this script could name" >&2
    echo "         $((errs + skipped - unmapped)) of them; the list written below may be short. Read dar's messages above." >&2
  fi
  finish_damage "$destdir" "$gone"
}

# One unit's list, from the files dar named and the files the check finds: the
# list is written when it holds any file, and a list left by an earlier run is
# removed when it does not.
finish_damage() { # <destination> <slices not available, or empty>
  local destdir=$1 gone=${2:-} dar_n all_n
  file_check "$destdir"
  dar_n=$(wc -l <"$WORK/damaged.new")
  awk '!seen[$0]++' "$WORK/damaged.new" "$CHECK_BAD" >"$WORK/damaged.all"
  all_n=$(wc -l <"$WORK/damaged.all")
  CHECK_ADDED=$((all_n - dar_n))
  if [ "$all_n" -gt 0 ]; then
    record_damage "$destdir" "$WORK/damaged.all" "$gone"
  elif [ "$CHECK_UNNAMED" -gt 0 ]; then
    UNIT_OK=0
    INCOMPLETE+=("Unit '$UNIT_NAME' (version $UNIT_VER): $CHECK_UNNAMED file(s) failed the sha256 check and this script could not name them (see the WARNING above); no list is written.")
  else
    clear_stale_list "$destdir"
  fi
}

# ---- salvage mode: a unit's slices on disk, bad ones replaced ----

JOB_N=0
JOB_KIND=()
JOB_DEST=()
JOB_UNIT=()
JOB_VER=()
JOB_UUID=()
PENDING=()
EXTRACT_G=()
TAPE1_UUID=""
VERIFY_ONLY=0

ensure_scratch() {
  if [ -z "$SCRATCH" ]; then
    SCRATCH="$(mktemp -d "$SCRATCH_PARENT/.tapectl-restore.XXXXXX")" ||
      die "cannot create a scratch directory in $SCRATCH_PARENT"
  fi
}

# Called once the first tape's File 0 has been read: remember which cartridge
# it was, so a second tape that is the same one can be told apart.
salvage_init() {
  TAPE1_UUID=$(toml_val "$WORK/thunk.toml" uuid)
}

# The numbers of the slices job $1 still lacks, for a message.
missing_numbers() { # <job>
  awk -F'|' '{ printf "%s%s", sep, $1; sep = ", " }' "$WORK/job.$1.missing"
}

# A unit, in salvage mode. Its slices come off this tape one at a time into a
# directory of its own in the scratch area; a slice that fails is looked for in
# the --slices-from directories. With --swap-tapes a unit still short of slices
# waits for the second cartridge while the next units are read; without it dar
# is given what there is.
salvage_unit() { # <slices file> <destination> <unit> <version>
  local list=$1 destdir=$2 unit=$3 ver=$4 j bytes=0 num tpos size sha
  while IFS='|' read -r num tpos size sha; do
    bytes=$((bytes + size))
  done <"$list"
  check_space "$destdir" "$SCRATCH_PARENT" "$bytes" "$((bytes + BLOCK))"
  ensure_scratch
  j=$JOB_N
  JOB_N=$((JOB_N + 1))
  JOB_KIND[$j]=unit
  JOB_DEST[$j]=$destdir
  JOB_UNIT[$j]=$unit
  JOB_VER[$j]=$ver
  JOB_UUID[$j]=$UNIT_UUID
  mkdir -p "$SCRATCH/job.$j"
  spool_slices "$list" "$SCRATCH/job.$j" "$WORK/job.$j.missing"
  if [ -s "$WORK/job.$j.missing" ] && [ "$SWAP_TAPES" = 1 ]; then
    PENDING+=("$j")
    info "'$unit' is still short of slice(s) $(missing_numbers "$j"); it waits for the second cartridge"
    return 0
  fi
  job_run "$j"
}

job_run() { # <job>
  case "${JOB_KIND[$1]}" in
  fill) fill_extract "$1" ;;
  *) job_extract "$1" ;;
  esac
}

# dar reads a unit's decrypted slices from its directory in the scratch area.
job_extract() { # <job>
  local j=$1 destdir unit gone="" rc=0
  destdir=${JOB_DEST[$j]}
  unit=${JOB_UNIT[$j]}
  UNIT_NAME=$unit UNIT_VER=${JOB_VER[$j]} UNIT_UUID=${JOB_UUID[$j]}
  if [ -s "$WORK/job.$j.missing" ]; then
    gone=$(missing_numbers "$j")
    print_needs "$j"
    echo "NOTE: restoring '$unit' without slice(s) $gone: the files in them will be listed, not restored." >&2
  fi
  local -a dar_opts=(-O -Q -N)
  [ "$OVERWRITE" = 0 ] || dar_opts+=(-w)
  info "The slices are on disk; dar extracts '$unit' to $destdir ..."
  dar -x "$SCRATCH/job.$j/restore" -R "$(abs_dir "$destdir")" "${dar_opts[@]}" ${EXTRACT_G[@]+"${EXTRACT_G[@]}"} >"$WORK/extract.log" 2>&1 </dev/null || rc=$?
  rm -rf "$SCRATCH/job.$j"
  if [ -n "$gone" ] && [ "$rc" != 0 ] && [ "$rc" != 5 ]; then
    cat "$WORK/extract.log" >&2
    die "dar could not restore '$unit' without slice(s) $gone (exit $rc). The first and the last slice of a
       unit cannot be left out: they hold the archive's header and its catalogue."
  fi
  dar_result "$WORK/extract.log" "$rc" "$destdir" "$gone"
  if [ "$UNIT_OK" = 1 ]; then
    if [ ${#RESTORE_PATHS[@]} -gt 0 ]; then
      DONE_LINES+=("${RESTORE_PATHS[*]} of '$unit' restored to: $destdir")
    else
      DONE_LINES+=("Unit '$unit' restored to: $destdir")
    fi
  fi
}

# The slices a pending job is still short of: job|number|bytes|sha256|label.
needed_slices() {
  local j
  for j in ${PENDING[@]+"${PENDING[@]}"}; do
    U="${JOB_UNIT[$j]}" awk -F'|' -v j="$j" -v q="'" '{ print j "|" $1 "|" $3 "|" $4 "|unit " q ENVIRON["U"] q " slice " $1 }' "$WORK/job.$j.missing"
  done
}

# What is still needed, by sha256: the figure to look for in another tape's
# front index, and the number to find it by in this unit.
print_needs() { # <job: its missing file is read, or - to print $WORK/needed.txt>
  local job num size sha label
  echo ""
  info "Slices still needed. Another copy holds each one as the same bytes, listed under the same sha256"
  info "in its front index (the number is only this tape's numbering of the unit):"
  if [ "$1" = - ]; then
    while IFS='|' read -r job num size sha label; do
      info "  $label, $size bytes, sha256 $sha"
    done <"$WORK/needed.txt"
  else
    while IFS='|' read -r num _ size sha; do
      info "  unit '${JOB_UNIT[$1]}' slice $num, $size bytes, sha256 $sha"
    done <"$WORK/job.$1.missing"
  fi
}

# The cartridge now in the drive, as far as this script needs it: its label,
# and its file map in $WORK/other.files (position|type|size|sha256 per line).
# Everything the first tape established is put back afterwards.
OTHER_TAPE=0
OTHER_LABEL=""
other_tape_map() {
  local k_fi=$FRONT_INDEX k_seal=$SEAL_MARKER k_total=$TOTAL_FILES k_label=$TAPE_LABEL
  local k_created=${CREATED:-} k_files=$FILES_TXT k_src=${FILES_SOURCE:-} uuid ok=0
  cp "$FILES_TXT" "$WORK/tape1.files"
  OTHER_TAPE=1
  CUR=""
  bootstrap_thunk
  OTHER_TAPE=0
  OTHER_LABEL=$TAPE_LABEL
  uuid=$(toml_val "$WORK/thunk.toml" uuid)
  if [ -n "$uuid" ] && [ "$uuid" = "$TAPE1_UUID" ]; then
    info "That is the cartridge this run started with (volume uuid $(safe_str "$uuid")). It cannot hold a good copy of its own bad slices."
  elif try_front_index; then
    cp "$WORK/files_from_index.txt" "$WORK/other.files"
    ok=1
  elif try_seal_copy; then
    echo "WARNING: the front index of this cartridge is unreadable: using the seal marker's copy of it." >&2
    cp "$WORK/files_from_seal.txt" "$WORK/other.files"
    ok=1
  else
    info "Neither the front index of this cartridge nor the seal marker's copy of it can be read."
  fi
  FRONT_INDEX=$k_fi SEAL_MARKER=$k_seal TOTAL_FILES=$k_total TAPE_LABEL=$k_label CREATED=$k_created
  FILES_SOURCE=$k_src
  cp "$WORK/tape1.files" "$k_files"
  [ "$ok" = 1 ]
}

# Read the cartridge now in the drive for the slices still needed (the file
# $WORK/needed.txt), by sha256 only, in the order they lie on it: one rewind,
# then forward. In restore mode each is read into the scratch area, checked
# against the sha256 the first tape's front index gave, and decrypted into the
# slice directory of every job short of it; in verify mode it is only hashed.
swap_fetch() {
  local pos sha size job num rest
  if ! (tape_init) 2>"$WORK/swap.err"; then
    echo "NOTE: the drive is not ready: $(head -n 1 "$WORK/swap.err" 2>/dev/null)" >&2
    echo "      Wait until the cartridge is loaded, then press ENTER again." >&2
    return 1
  fi
  other_tape_map || return 1
  info "This cartridge identifies as: $(safe_str "${OTHER_LABEL:-<unreadable>}")"
  awk -F'|' '
    FILENAME == ARGV[1] { need[$4] = $3; next }
    $2 == "data_slice" && ($4 in need) && $3 == need[$4] && !($4 in seen) { seen[$4] = 1; print $1 "|" $4 "|" $3 }
  ' "$WORK/needed.txt" "$WORK/other.files" >"$WORK/fetch.plan"
  if [ ! -s "$WORK/fetch.plan" ]; then
    info "No file in the front index of this cartridge has the sha256 of a slice still needed."
    return 1
  fi
  : >"$WORK/fetched.txt"
  while IFS='|' read -r pos sha size; do
    require_uint position "$pos"
    require_uint size_bytes "$size"
    info "Tape file $pos of this cartridge lists sha256 ${sha:0:16}..., a slice still needed"
    if [ "$VERIFY_ONLY" = 1 ]; then
      if hash_tape_file "$pos" "$size" && [ "$TAPE_SHA" = "$sha" ]; then
        info "  checksum verified: a good copy"
        echo "$sha" >>"$WORK/fetched.txt"
      else
        echo "NOTE: tape file $pos of this cartridge does not hash to that sha256 either." >&2
      fi
      continue
    fi
    if read_slice_checked "$pos" "$size" "$sha" "$SCRATCH/fetch.enc"; then
      info "  checksum verified against the first tape's front index"
      while IFS='|' read -r job num _ rest; do
        [ "${rest%%|*}" = "$sha" ] || continue
        decrypt_slice "$SCRATCH/fetch.enc" "$SCRATCH/job.$job/restore.$num.dar" "$num"
      done <"$WORK/needed.txt"
      rm -f "$SCRATCH/fetch.enc"
      echo "$sha" >>"$WORK/fetched.txt"
    else
      echo "NOTE: tape file $pos of this cartridge $SLICE_WHY: it is not used." >&2
    fi
  done <"$WORK/fetch.plan"
  for job in ${PENDING[@]+"${PENDING[@]}"}; do
    awk -F'|' 'FILENAME == ARGV[1] { got[$1] = 1; next } !($4 in got)' "$WORK/fetched.txt" "$WORK/job.$job.missing" >"$WORK/job.$job.missing.new"
    mv "$WORK/job.$job.missing.new" "$WORK/job.$job.missing"
  done
  if [ "$VERIFY_ONLY" = 1 ]; then
    local e
    local -a keep=()
    for e in ${VERIFY_BAD[@]+"${VERIFY_BAD[@]}"}; do
      grep -qxF -- "${e##*|}" "$WORK/fetched.txt" || keep+=("$e")
    done
    VERIFY_BAD=(${keep[@]+"${keep[@]}"})
  fi
}

# Ask for cartridges until nothing pending is still short or the heir says to
# go on without. The first tape has been read to its end by now; nothing here
# moves it back.
swap_loop() {
  local reply
  while true; do
    if [ "$VERIFY_ONLY" = 1 ]; then
      verify_needed >"$WORK/needed.txt"
    else
      needed_slices >"$WORK/needed.txt"
    fi
    [ -s "$WORK/needed.txt" ] || return 0
    print_needs -
    info "Load a cartridge that holds copies of these slices into $DEVICE (any tape whose front index lists"
    info "the sha256 values above; it need not be a copy of the whole tape), wait until the drive is ready,"
    info "then press ENTER. Type done and ENTER (or end the input) to go on without them."
    reply=quit
    IFS= read -r reply || reply=quit
    case "$reply" in
    [dD]one | DONE | q | quit)
      info "Going on without them."
      return 0
      ;;
    esac
    swap_fetch || true
  done
}

# After the last unit has been read from the first tape: the second cartridge
# for the units still short, then dar for each of them.
finish_pending() {
  local j
  [ ${#PENDING[@]} -gt 0 ] || return 0
  swap_loop
  for j in "${PENDING[@]}"; do
    job_run "$j"
  done
  PENDING=()
}

# The end of a restore: COMPLETE only when no unit came back short.
restore_end() {
  local line
  note_other_units
  echo ""
  if [ ${#INCOMPLETE[@]} -gt 0 ]; then
    info "RESTORE INCOMPLETE"
    for line in "${INCOMPLETE[@]}"; do
      printf '%s\n' "$line" | sed 's/^/>>> /'
    done
    for line in ${DONE_LINES[@]+"${DONE_LINES[@]}"}; do
      info "$line"
    done
    exit 1
  fi
  info "RESTORE COMPLETE"
  for line in ${DONE_LINES[@]+"${DONE_LINES[@]}"}; do
    info "$line"
  done
}

# ---- --restore --fill-from LIST: the files an earlier restore could not restore ----

# Restore, from the tape in the drive, exactly the files a damaged-file list
# names, over the damaged or missing copies in the directory the list sits in,
# and nothing else there. The tape is used only if its envelope lists the same
# unit with the same uuid at the same version as the list's header; dar's own
# checks confirm each file it restores. The list is rewritten with whatever is
# still damaged, and removed when nothing is.
FILL_ESC=()
FILL_PATH=()
do_fill() { # <destination> <version given, or empty> <scratch parent, or empty>
  local destdir=$1 want_version=${2:-} scr=${3:-} list=$FILL_FROM
  local first magic fmt f_unit f_uuid f_ver unit uuid ver line p root listdir todir sub
  [ -f "$list" ] || die "--fill-from: no such file: $list"
  [ -d "$destdir" ] || die "--to $destdir does not exist: a fill restores into the directory the earlier restore wrote"
  first=""
  IFS= read -r first <"$list" || true
  magic="" fmt="" f_unit="" f_uuid="" f_ver=""
  IFS=$'\t' read -r magic fmt f_unit f_uuid f_ver _ <<<"$first" || true
  [ "$magic" = '#tapectl-damaged-files' ] ||
    die "$list is not a list of damaged files written by this script: its first line is not the header"
  [ "$fmt" = 1 ] || die "$list is a list of format $(safe_str "$fmt"); this script reads format 1"
  case "$f_unit:$f_uuid:$f_ver" in
  unit=*:uuid=?*:version=?*) ;;
  *) die "$list: the header does not name a unit, its uuid and its version" ;;
  esac
  unesc_line "${f_unit#unit=}"
  unit=$UNESC
  uuid=${f_uuid#uuid=}
  ver=${f_ver#version=}
  require_uint version "$ver"
  case "$uuid" in
  *[!0-9a-fA-F-]*) die "$list: the unit uuid in the header is not a uuid" ;;
  esac
  if [ ${#RESTORE_UNITS[@]} -ne 1 ] || [ "${RESTORE_UNITS[0]}" != "$unit" ]; then
    die "this list is for unit '$(visible_text "$unit")', version $ver: give --unit '$(visible_text "$unit")' (and only that unit)"
  fi
  if [ -n "$want_version" ] && [ "$want_version" != "$ver" ]; then
    die "this list is for version $ver of '$(visible_text "$unit")', and --version says $want_version: a fill uses the list's version only"
  fi

  # The directory the list sits in is the unit's directory: --to itself for a
  # restore of one unit, --to/UNIT for several.
  listdir=$(cd "$(dirname -- "$list")" && pwd -P) || die "cannot enter the directory of $list"
  todir=$(cd "$destdir" && pwd -P) || die "cannot enter --to $destdir"
  root=""
  if [ "$listdir" = "$todir" ]; then
    root=$todir
  else
    sub=$(cd "$todir/$unit" 2>/dev/null && pwd -P) || sub=""
    if [ -n "$sub" ] && [ "$listdir" = "$sub" ]; then
      root=$sub
    fi
  fi
  [ -n "$root" ] ||
    die "$list is not in --to ($todir) or in --to/UNIT: keep the list where the restore wrote it,
       in the unit's own directory, and give the same --to"
  [ "$(basename -- "$list")" = "$DAMAGED_NAME" ] || echo "NOTE: the list is named $(basename -- "$list"), not $DAMAGED_NAME; it is rewritten under the name it has." >&2

  FILL_ESC=()
  FILL_PATH=()
  : >"$WORK/fill.wanted"
  while IFS= read -r line || [ -n "$line" ]; do
    [ -n "$line" ] || continue
    unesc_line "$line"
    p=$UNESC
    case "$p" in
    /*) die "$list names an absolute path ($(visible_text "$p")): a list names paths inside the unit" ;;
    esac
    case "/$p/" in
    */../*) die "$list names a path with .. in it ($(visible_text "$p")): a list names paths inside the unit" ;;
    esac
    if [ -d "$root/$p" ] && [ ! -L "$root/$p" ]; then
      die "$list names a directory ($(visible_text "$p")): a fill restores files, and restoring a directory
       would overwrite everything under it. Remove the line from the list."
    fi
    FILL_ESC+=("$line")
    FILL_PATH+=("$p")
    printf '%s\n' "$line" >>"$WORK/fill.wanted"
  done < <(tail -n +2 "$list")
  [ ${#FILL_PATH[@]} -gt 0 ] || die "$list names no file"
  info "Filling ${#FILL_PATH[@]} file(s) of unit '$(visible_text "$unit")', version $ver, in $root"

  SCRATCH_PARENT=${scr:-$root}
  mkdir -p "$SCRATCH_PARENT"
  check_stale_scratch "$root" "$SCRATCH_PARENT"
  dar_check
  set_age_ids
  establish_files
  [ "$SALVAGE" = 0 ] || salvage_init

  walk_envelopes pick_for_restore
  [ -n "$CHOSEN_ENV" ] || die_no_envelope "$(visible_text "$unit")"
  info "Decrypted envelope at file $CHOSEN_POS"
  info "  opened with key $CHOSEN_KEY"
  local manifest="$CHOSEN_ENV/MANIFEST.toml" here
  [ -f "$manifest" ] || die "envelope missing MANIFEST.toml"
  here=$(unit_uuid_of "$CHOSEN_ENV" "$unit" "$ver")
  if [ -z "$here" ]; then
    echo "This tape lists the unit '$(visible_text "$unit")' as:" >&2
    units_table "$manifest" >&2
    die "this tape holds no version $ver of unit '$(visible_text "$unit")'. A fill uses the same unit at the same version
       only (another version may differ in the very files that are damaged). Nothing was changed."
  fi
  if [ "$here" != "$uuid" ]; then
    die "this tape's version $ver of '$(visible_text "$unit")' has unit uuid $(safe_str "$here"), and the list says $(safe_str "$uuid"):
       it is not the same unit. Nothing was changed."
  fi
  local base
  base=$(catalogue_path "$CHOSEN_ENV" "$unit" "$ver")
  [ -n "$base" ] || die "this tape's envelope carries no dar catalogue for '$(visible_text "$unit")' version $ver, so a fill cannot
       tell which slices hold the files. Restore the whole unit into another directory instead
       (--restore --unit NAME --to NEWDIR) and copy the files from there."

  # Which of the files this version's catalogue holds, and in which slices.
  dar -l "$base" -N -Q -Tslice 2>"$WORK/fill.dar.err" |
    FILL_WANTED="$WORK/fill.wanted" awk '__AWK_FILL_LISTING__' >"$WORK/fill.found" ||
    die "dar could not read the catalogue of '$(visible_text "$unit")': $(tail -n 3 "$WORK/fill.dar.err")"
  awk -F'\t' '$1 == "P" { print $2 }' "$WORK/fill.found" >"$WORK/fill.present"
  awk -F'\t' '$1 == "S" { print $2 }' "$WORK/fill.found" | sort -n >"$WORK/fill.slices"
  awk 'FILENAME == ARGV[1] { a[$0] = 1; next } !($0 in a)' "$WORK/fill.present" "$WORK/fill.wanted" >"$WORK/fill.absent"
  if [ -s "$WORK/fill.absent" ]; then
    echo "NOTE: $(wc -l <"$WORK/fill.absent") listed path(s) are not in this tape's catalogue of the version; they stay in the list:" >&2
    sed 's/^/        /' "$WORK/fill.absent" | head -n 20 >&2
  fi
  if [ ! -s "$WORK/fill.present" ]; then
    die "none of the listed files is in this tape's version $ver of '$(visible_text "$unit")'. The list is unchanged."
  fi

  pick_slices "$manifest" "$unit" "$WORK/slices.0" "$ver"
  local last
  last=$(tail -n 1 "$WORK/slices.0" | cut -d'|' -f1)
  awk -F'|' -v last="$last" 'FILENAME == ARGV[1] { need[$1] = 1; next } ($1 in need) || $1 == last' "$WORK/fill.slices" "$WORK/slices.0" >"$WORK/fill.slice.list"
  local total=0 n num tpos size sha
  n=$(wc -l <"$WORK/fill.slice.list")
  while IFS='|' read -r num tpos size sha; do
    total=$((total + size))
  done <"$WORK/fill.slice.list"
  info "The files lie in $((n - 1)) slice(s) of $last; reading those and the last"
  check_space "$root" "$SCRATCH_PARENT" "$total" "$total"
  ensure_scratch

  local j=$JOB_N
  JOB_N=$((JOB_N + 1))
  JOB_KIND[$j]=fill
  JOB_DEST[$j]=$root
  JOB_UNIT[$j]=$unit
  JOB_VER[$j]=$ver
  JOB_UUID[$j]=$uuid
  mkdir -p "$SCRATCH/job.$j"
  FILL_HEADER=$first
  spool_slices "$WORK/fill.slice.list" "$SCRATCH/job.$j" "$WORK/job.$j.missing"
  if [ -s "$WORK/job.$j.missing" ] && [ "$SWAP_TAPES" = 1 ]; then
    PENDING+=("$j")
    finish_pending
  else
    job_run "$j"
  fi
  fill_end "$list" "$root"
}

FILL_HEADER=""
FILL_BAD="$WORK/fill.bad"

# dar over the files of a fill, 500 paths to a run, always overwriting (these
# are the files to replace) and never anything else: -g names a path and
# takes no pattern. What dar reports it could not restore, and every path it
# was never asked for because it is not in the catalogue, is still damaged.
fill_extract() { # <job>
  local j=$1 root i k rc run=0 errs skipped unmapped failed
  root=$(abs_dir "${JOB_DEST[$j]}")
  UNIT_NAME=${JOB_UNIT[$j]} UNIT_VER=${JOB_VER[$j]} UNIT_UUID=${JOB_UUID[$j]}
  : >"$FILL_BAD"
  local gone=""
  if [ -s "$WORK/job.$j.missing" ]; then
    gone=$(missing_numbers "$j")
    print_needs "$j"
    echo "NOTE: filling without slice(s) $gone: the files in them stay in the list." >&2
  fi
  awk 'FILENAME == ARGV[1] { a[$0] = 1; next } ($0 in a) { print FNR }' "$WORK/fill.present" "$WORK/fill.wanted" >"$WORK/fill.rows"
  local -a g=()
  local row
  k=0
  while true; do
    g=()
    while [ ${#g[@]} -lt 1000 ] && IFS= read -r row <&3; do
      g+=(-g "${FILL_PATH[$((row - 1))]}")
    done
    [ ${#g[@]} -gt 0 ] || break
    run=$((run + 1))
    rc=0
    info "Restoring $((${#g[@]} / 2)) file(s) over their damaged copies ..."
    dar -x "$SCRATCH/job.$j/restore" -R "$root" -O -Q -N -w "${g[@]}" >"$WORK/fill.$run.log" 2>&1 </dev/null || rc=$?
    cat "$WORK/fill.$run.log"
    if [ "$rc" != 0 ] && grep -i 'no space left' "$WORK/fill.$run.log" >/dev/null 2>&1; then
      die_io "dar extraction failed" "$WORK/fill.$run.log" "$root"
    fi
    if [ "$rc" != 0 ] && [ "$rc" != 5 ]; then
      die "dar extraction failed (exit $rc) — dar's own message is above. The list is unchanged."
    fi
    DAMAGED_ROOT=$root DAMAGED_SKIPS=1 DAMAGED_COUNTS="$WORK/damaged.counts" \
      awk '__AWK_DAR_DAMAGED__' "$WORK/fill.$run.log" >>"$FILL_BAD"
    errs=0 skipped=0 unmapped=0 failed=0
    read -r errs skipped unmapped failed <"$WORK/damaged.counts" || true
    if [ "$unmapped" -gt 0 ] || [ "$failed" -ne "$errs" ]; then
      echo "WARNING: dar reported $failed file(s) failed and this script could name $errs; a file it could not" >&2
      echo "         name is missing from the rewritten list. Read dar's messages above." >&2
    fi
  done 3<"$WORK/fill.rows"
  rm -rf "$SCRATCH/job.$j"
}

# After the fill: the list now holds the files still damaged, or is gone.
fill_end() { # <list> <root>
  local list=$1 root=$2 n
  awk '
    FILENAME == ARGV[1] { bad[$0] = 1; ord[++k] = $0; next }
    FILENAME == ARGV[2] { if (!($0 in bad)) { bad[$0] = 1; ord[++k] = $0 } next }
    { w[$0] = 1; if (($0 in bad) && !($0 in out)) { out[$0] = 1; print } }
    END { for (i = 1; i <= k; i++) if (!(ord[i] in out)) { out[ord[i]] = 1; print ord[i] } }
  ' "$WORK/fill.absent" "$FILL_BAD" "$WORK/fill.wanted" >"$WORK/fill.still"
  # What dar restored without complaint is checked against the index too
  # (ADR-0012 item 103): a file that fails stays in the list.
  awk 'FILENAME == ARGV[1] { s[$0] = 1; next } !($0 in s)' "$WORK/fill.still" "$WORK/fill.wanted" >"$WORK/fill.restored"
  file_check "$root" "$WORK/fill.restored"
  if [ "$CHECK_UNNAMED" -gt 0 ] && [ "$CHECK_FAILED" -eq 0 ]; then
    cp "$WORK/fill.wanted" "$WORK/fill.still"
  elif [ "$CHECK_FAILED" -gt 0 ]; then
    awk '!seen[$0]++' "$WORK/fill.still" "$CHECK_BAD" >"$WORK/fill.still.new"
    mv "$WORK/fill.still.new" "$WORK/fill.still"
  fi
  n=$(wc -l <"$WORK/fill.still")
  echo ""
  if [ "$n" -eq 0 ]; then
    rm -f "$list"
    info "FILL COMPLETE: every listed file of '$(visible_text "$UNIT_NAME")' was restored from this tape into $root"
    info "  (dar checked each one). $list is removed."
    return 0
  fi
  {
    printf '%s\n' "$FILL_HEADER"
    cat "$WORK/fill.still"
  } >"$list"
  info "FILL INCOMPLETE: $n file(s) of '$(visible_text "$UNIT_NAME")' are still damaged or missing. The list now holds only those:"
  info "  $list"
  info "Fill them from another tape holding this unit at version $UNIT_VER, the same way."
  exit 1
}

# ---- the cross-volume index (--where, --find; volume-format-v2.md 1.5) ----

# A tape written since version 1.3.0 carries, in each tenant envelope,
# index/units.tsv (that tenant's units: every version and every tape holding it)
# and index/paths.tsv (every path of those versions); the operator envelope
# carries index/units.tsv for every tenant. Both are tab-separated text with a
# header line naming the columns, and both are read here with awk, so they need
# no more than the tools that open an envelope. The index is as of the moment
# its own tape was written: it never names a tape written after it, and a tape
# it names may since have been retired or erased.
INDEX_OPENED=0
INDEX_HAD=0

index_banner() {
  echo ""
  info "Index read from tape: ${TAPE_LABEL:-<unreadable>} (written $(safe_str "${CREATED:-unknown}"))"
  info "An index is as of the moment its own tape was written. It never names a tape written"
  info "after it, and a tape it names may since have been retired or erased. The newest tape"
  info "you hold has the most complete index: if this is not it, run this on that one."
}

# A tape with no index says so and points at what it can do. Exit 1: the answer
# is not on this tape.
die_no_index() {
  echo ""
  info "This tape carries no index. Tapes written before version 1.3.0 have none, and nothing"
  info "on this tape says where a unit's other versions or copies are."
  info "What this tape can tell you about itself:"
  info "  $0 --find-envelope $(keys_args)     the units, versions and slices it holds"
  info "  $0 --list $(keys_args)              the files of a unit (add --unit NAME)"
  info "For another tape, load it and run its own RESTORE.sh the same way; a tape written"
  info "since version 1.3.0 can answer for all the tapes written before it."
  exit 1
}

# --where: copy out the units.tsv of each envelope a key opens, then remove the
# envelope (an escrow key opens them all, and a paths.tsv can be large). An
# operator envelope's units.tsv lists every tenant's units, so the walk stops
# at it; the tenant envelopes come before it on the tape.
WHERE_FILES=()
WHERE_TENANTS=()
WHERE_OPERATOR=""
where_envelope() { # <position>
  local pos=$1 d="$WORK/opened.$1" ty who=""
  INDEX_OPENED=$((INDEX_OPENED + 1))
  ty=$(file_type_at "$pos" "$FILES_TXT")
  if [ -f "$d/index/units.tsv" ]; then
    INDEX_HAD=$((INDEX_HAD + 1))
    mv "$d/index/units.tsv" "$WORK/where.$pos.tsv"
    case "$ty" in
    operator_envelope*)
      WHERE_OPERATOR="$WORK/where.$pos.tsv"
      rm -rf "$d"
      return 1
      ;;
    esac
    who=$(toml_val "$d/MANIFEST.toml" tenant 2>/dev/null || true)
    WHERE_FILES+=("$WORK/where.$pos.tsv")
    WHERE_TENANTS+=("$(safe_str "$who")")
  fi
  rm -rf "$d"
  return 0
}

do_where() { # <unit>
  local unit=$1 i rc found=0 f
  establish_files
  walk_envelopes where_envelope
  [ "$INDEX_OPENED" -gt 0 ] || die_no_envelope
  index_banner
  echo ""
  if [ -n "$WHERE_OPERATOR" ]; then
    WHERE_FILES=("$WHERE_OPERATOR")
    WHERE_TENANTS=("")
  fi
  [ "$INDEX_HAD" -gt 0 ] || die_no_index
  : >"$WORK/where.known"
  for ((i = 0; i < ${#WHERE_FILES[@]}; i++)); do
    f=${WHERE_FILES[$i]}
    rc=0
    WHERE_UNIT="$unit" WHERE_KNOWN="$WORK/where.known" WHERE_TENANT="${WHERE_TENANTS[$i]}" \
      awk '__AWK_INDEX_COMMON__ __AWK_INDEX_WHERE__' "$f" || rc=$?
    case "$rc" in
    0) found=1 ;;
    1) ;;
    *) echo "WARNING: an index in this tape's envelopes has no unit or version column, so this script cannot read it." >&2 ;;
    esac
  done
  if [ "$found" = 0 ]; then
    echo ""
    info "No unit named '$unit' is in this index."
    if [ -s "$WORK/where.known" ]; then
      info "It lists these units:"
      sort -u "$WORK/where.known" | head -n 40 | sed 's/^/    /'
    fi
    if [ -z "$WHERE_OPERATOR" ]; then
      info "A tenant's key opens that tenant's index only; the operator or escrow key opens the"
      info "index of every tenant."
    fi
    exit 1
  fi
  echo ""
  info "A holder marked 'this tape' is the tape you have loaded. For any other, find the cartridge"
  info "by its label or its chip serial, load it, and use the RESTORE.sh stored on that tape."
}

# --find: the matches of each tenant envelope's paths.tsv, kept in a file so the
# banner can come first. One limit is shared by every envelope.
FIND_TEXT=""
FIND_LEFT=50
FIND_SHOWN=0
FIND_TOTAL=0
find_in_envelope() { # <position>
  local pos=$1 d="$WORK/opened.$1" u who shown total
  INDEX_OPENED=$((INDEX_OPENED + 1))
  if [ -f "$d/index/units.tsv" ] || [ -f "$d/index/paths.tsv" ]; then
    INDEX_HAD=$((INDEX_HAD + 1))
  fi
  if [ -f "$d/index/paths.tsv" ]; then
    u="$d/index/units.tsv"
    [ -f "$u" ] || u=/dev/null
    who=$(toml_val "$d/MANIFEST.toml" tenant 2>/dev/null || true)
    : >"$WORK/find.counts"
    FIND_TEXT="$FIND_TEXT" FIND_LIMIT="$FIND_LEFT" FIND_UNITS="$u" FIND_COUNTS="$WORK/find.counts" \
      FIND_WHO="tenant $(safe_str "$who")" \
      awk '__AWK_INDEX_COMMON__ __AWK_INDEX_FIND__' "$d/index/paths.tsv" >>"$WORK/find.out" || {
      echo "WARNING: the index in the envelope at file $pos has no unit, version or path column, so this script cannot read it." >&2
      : >"$WORK/find.counts"
    }
    shown=0
    total=0
    read -r shown total <"$WORK/find.counts" || true
    if is_uint "$shown" && is_uint "$total"; then
      FIND_SHOWN=$((FIND_SHOWN + shown))
      FIND_TOTAL=$((FIND_TOTAL + total))
      FIND_LEFT=$((FIND_LEFT - shown))
    fi
  fi
  rm -rf "$d"
  return 0
}

# The text given, with a TAB, newline or carriage return written out, so a
# message about it stays on its own line.
visible_text() { # <text>
  local t=$1
  t=${t//$'\t'/\\t}
  t=${t//$'\n'/\\n}
  t=${t//$'\r'/\\r}
  printf '%s' "$t"
}

do_find() { # <text>
  FIND_TEXT=$1
  : >"$WORK/find.out"
  establish_files
  walk_envelopes find_in_envelope
  [ "$INDEX_OPENED" -gt 0 ] || die_no_envelope
  index_banner
  [ "$INDEX_HAD" -gt 0 ] || die_no_index
  if [ "$FIND_TOTAL" -eq 0 ]; then
    echo ""
    info "No path in this index contains '$(visible_text "$FIND_TEXT")'. (The text is matched as it stands, not as a"
    info "pattern, and capitals matter. A path is relative to its unit's root.)"
    exit 1
  fi
  echo ""
  info "Paths containing '$(visible_text "$FIND_TEXT")' (unit, version, kind, size, path; kind is d directory,"
  info "f file, l symlink, s special). A TAB, newline or carriage return in a path is shown as"
  info "\\t, \\n or \\r."
  echo ""
  cat "$WORK/find.out"
  echo ""
  if [ "$FIND_TOTAL" -gt "$FIND_SHOWN" ]; then
    info "$((FIND_TOTAL - FIND_SHOWN)) more match(es) are not shown: this prints the first $FIND_SHOWN. Give more of"
    info "the path to narrow it."
  fi
  info "To restore one of these from the tape that holds it:"
  info "  $0 --restore $(keys_args) --unit UNIT --path PATH --to /your/destination"
}

# ---- main ----

case "${1:-}" in
--info)
  require_tools "${TOOLS_READ[@]}"
  do_info
  ;;
--verify)
  shift
  while [ $# -gt 0 ]; do
    case "$1" in
    --slices-from)
      [ $# -ge 2 ] || die "--slices-from needs a directory

       usage: $0 --verify [--slices-from DIR ...] [--swap-tapes]"
      SLICES_FROM+=("$2")
      shift 2
      ;;
    --swap-tapes)
      SWAP_TAPES=1
      shift
      ;;
    *) die "unknown option: $1

       usage: $0 --verify [--slices-from DIR ...] [--swap-tapes]" ;;
    esac
  done
  check_slices_from
  require_tools "${TOOLS_READ[@]}"
  do_verify
  ;;
--find-envelope)
  shift
  # --key may be repeated; every key is tried against every envelope (#288).
  KEYS=()
  while [ $# -gt 0 ]; do
    case "$1" in
    --key)
      [ -n "${2:-}" ] || die "usage: $0 --find-envelope --key KEYFILE"
      KEYS+=("$2")
      shift 2
      ;;
    *) die "usage: $0 --find-envelope --key KEYFILE" ;;
    esac
  done
  [ ${#KEYS[@]} -gt 0 ] || die "usage: $0 --find-envelope --key KEYFILE"
  require_tools "${TOOLS_ENVELOPE[@]}"
  require_key_files
  do_find_envelope
  ;;
--restore)
  shift
  KEYS=()
  dest="" want="" scratch=""
  # Every flag but the switches takes a value. `shift 2` on a TRAILING bare
  # flag fails because $# is 1, `set -e` fires, and the script exits 1 having
  # printed nothing at all — the usage check below is never reached. Check the
  # arity first and say which flag was short (#133).
  while [ $# -gt 0 ]; do
    case "$1" in
    --key | --to | --unit | --version | --scratch | --path | --slices-from | --fill-from)
      [ $# -ge 2 ] || die "$1 needs a value

       usage: $0 --restore --key KEYFILE --to DIR [--unit U | --all] [--version N]"
      case "$1" in
      --key) KEYS+=("$2") ;;
      --to) dest=$2 ;;
      --unit) RESTORE_UNITS+=("$2") ;;
      --version) want=$2 ;;
      --scratch) scratch=$2 ;;
      --path) RESTORE_PATHS+=("$2") ;;
      --slices-from) SLICES_FROM+=("$2") ;;
      --fill-from) FILL_FROM=$2 ;;
      esac
      shift 2
      ;;
    --all)
      RESTORE_ALL=1
      shift
      ;;
    --swap-tapes)
      SWAP_TAPES=1
      shift
      ;;
    --no-space-check)
      SKIP_SPACE_CHECK=1
      shift
      ;;
    --overwrite)
      OVERWRITE=1
      shift
      ;;
    --no-file-check)
      FILE_CHECK=0
      shift
      ;;
    *) die "unknown option: $1" ;;
    esac
  done
  [ ${#KEYS[@]} -gt 0 ] || die "usage: $0 --restore --key KEYFILE --to DIR [--unit U | --all] [--version N]"
  [ -n "$dest" ] || die "usage: $0 --restore --key KEYFILE --to DIR [--unit U | --all] [--version N]"
  if [ "$RESTORE_ALL" = 1 ] && [ ${#RESTORE_UNITS[@]} -gt 0 ]; then
    die "--all restores every unit; give it without --unit"
  fi
  check_slices_from
  if [ -n "$FILL_FROM" ]; then
    [ "$RESTORE_ALL" = 0 ] || die "--fill-from fills the files of one unit: give --unit U, not --all"
    [ ${#RESTORE_PATHS[@]} -eq 0 ] || die "--fill-from restores the files its list names: it takes no --path"
    [ ${#RESTORE_UNITS[@]} -eq 1 ] || die "--fill-from needs the unit its list is for: give --unit U (once)"
  fi
  if [ ${#RESTORE_PATHS[@]} -gt 0 ]; then
    require_tools "${TOOLS_PATH[@]}"
  else
    require_tools "${TOOLS_RESTORE[@]}"
  fi
  require_key_files
  [ -z "$want" ] || require_uint version "$want"
  if [ -n "$FILL_FROM" ]; then
    do_fill "$dest" "$want" "$scratch"
  else
    do_restore "$dest" "$want" "$scratch"
  fi
  ;;
--list)
  shift
  KEYS=()
  want=""
  while [ $# -gt 0 ]; do
    case "$1" in
    --key | --unit | --version)
      [ $# -ge 2 ] || die "$1 needs a value

       usage: $0 --list --key KEYFILE [--unit U] [--version N]"
      case "$1" in
      --key) KEYS+=("$2") ;;
      --unit) RESTORE_UNITS+=("$2") ;;
      --version) want=$2 ;;
      esac
      shift 2
      ;;
    *) die "unknown option: $1" ;;
    esac
  done
  [ ${#KEYS[@]} -gt 0 ] || die "usage: $0 --list --key KEYFILE [--unit U] [--version N]"
  require_tools "${TOOLS_LIST[@]}"
  require_key_files
  [ -z "$want" ] || require_uint version "$want"
  do_list "$want"
  ;;
--where)
  shift
  if [ $# -lt 1 ] || [ "$1" = --key ]; then
    die "--where needs a unit name

       usage: $0 --where UNIT --key KEYFILE [--key K2 ...]"
  fi
  where_unit=$1
  shift
  KEYS=()
  while [ $# -gt 0 ]; do
    case "$1" in
    --key)
      [ $# -ge 2 ] || die "--key needs a value

       usage: $0 --where UNIT --key KEYFILE [--key K2 ...]"
      KEYS+=("$2")
      shift 2
      ;;
    *) die "unknown option: $1" ;;
    esac
  done
  [ ${#KEYS[@]} -gt 0 ] || die "usage: $0 --where UNIT --key KEYFILE [--key K2 ...]"
  require_tools "${TOOLS_INDEX[@]}"
  require_key_files
  do_where "$where_unit"
  ;;
--find)
  shift
  if [ $# -lt 1 ] || [ "$1" = --key ]; then
    die "--find needs the text to look for

       usage: $0 --find TEXT --key KEYFILE [--key K2 ...]"
  fi
  [ -n "$1" ] || die "--find needs the text to look for: an empty text matches every path"
  find_text=$1
  shift
  KEYS=()
  while [ $# -gt 0 ]; do
    case "$1" in
    --key)
      [ $# -ge 2 ] || die "--key needs a value

       usage: $0 --find TEXT --key KEYFILE [--key K2 ...]"
      KEYS+=("$2")
      shift 2
      ;;
    *) die "unknown option: $1" ;;
    esac
  done
  [ ${#KEYS[@]} -gt 0 ] || die "usage: $0 --find TEXT --key KEYFILE [--key K2 ...]"
  require_tools "${TOOLS_INDEX[@]}"
  require_key_files
  do_find "$find_text"
  ;;
--help | -h)
  echo "RESTORE.sh — Emergency restore for tapectl volume $LABEL (layout v2)"
  echo ""
  echo "Usage:"
  echo "  $0 --info                                       Show tape layout + seal verdict"
  echo "  $0 --verify [--slices-from DIR ...] [--swap-tapes]   Keyless integrity check"
  echo "  $0 --find-envelope --key KEYFILE [--key K2 ...]  Decrypt your envelope"
  echo "  $0 --restore --key KEYFILE [--key K2 ...] --to DIR"
  echo "               [--unit U [--unit U2 ...] | --all] [--version N]"
  echo "               [--no-space-check] [--overwrite] [--no-file-check] [--slices-from DIR ...] [--swap-tapes]"
  echo "      Full restore. Without --version the NEWEST version of the unit on"
  echo "      this volume is restored. To see which versions this tape holds,"
  echo "      run --find-envelope --key KEYFILE: snapshot_version lives in the"
  echo "      encrypted envelope manifest, so --info cannot report it."
  echo "      --all restores every unit of the envelope (--unit repeated, the"
  echo "      ones named), in one pass along the tape, each into --to/UNIT."
  echo "  $0 --restore --key KEYFILE --to DIR --unit U --path P [--path P2 ...]"
  echo "               [--version N] [--scratch DIR]"
  echo "      Restore only P (a file or folder, as --list prints it), reading"
  echo "      just the slices that hold it and the last. Those wait in a scratch"
  echo "      directory inside --to, or in --scratch DIR."
  echo "  $0 --list --key KEYFILE [--unit U] [--version N]"
  echo "      A unit's files, from the dar catalogue in the envelope: no slice"
  echo "      is read."
  echo "  $0 --where UNIT --key KEYFILE [--key K2 ...]"
  echo "      Which tapes hold UNIT: every version of it in the cross-volume index"
  echo "      this tape carries, with its slices and bytes and each tape holding it"
  echo "      (label, volume uuid, cartridge serial, generation, state, when it"
  echo "      sealed). The operator or escrow key reads the operator envelope's"
  echo "      map of every tenant's units; a tenant's key reads that tenant's."
  echo "  $0 --find TEXT --key KEYFILE [--key K2 ...]"
  echo "      Which unit, version and tape hold a file: every path in each tenant"
  echo "      envelope's index that contains TEXT (the characters as they stand,"
  echo "      not a pattern; capitals matter), with unit, version, kind, size, and"
  echo "      then the tapes holding that version. At most 50 matches are printed,"
  echo "      with a count of the rest. A TAB, newline or carriage return in a path"
  printf '%s\n' '      is shown as \t, \n or \r.'
  echo "      An index is as of the moment its own tape was written, so the newest"
  echo "      tape you hold has the most complete one. A tape written before"
  echo "      version 1.3.0 has none, and these modes say so. Both exit 1 when"
  echo "      nothing matched or the tape has no index."
  echo ""
  echo "  --key may be repeated, and each key is tried on its own for the"
  echo "      envelope and for every slice: a key rotation between staging a"
  echo "      unit and writing the volume seals the two to different key"
  echo "      generations, so no single key opens both."
  echo ""
  echo "  Disk space: each slice streams from the tape through age into dar,"
  echo "      so --to needs room for the restored files only, about the unit's"
  echo "      size (counted from the slices' encrypted size on tape: a compressed"
  echo "      unit may need more). The script checks before it reads any slice. Where this"
  echo "      dar cannot stream a unit safely (no catalogue in the envelope and"
  echo "      dar older than 2.7.21, or dar older than 2.7.9) its slices are"
  echo "      decrypted to disk first, in --scratch DIR (default: inside --to)."
  echo "  A dar built with GPG support needs gpg (package gnupg) to start."
  echo "  --slices-from DIR   (--restore, --verify; repeat it for more directories) A directory of"
  echo "      slice files copied off another tape that holds a copy of the unit, under any names."
  echo "      When a slice of this tape fails its sha256 or will not read, a file in DIR is used in"
  echo "      its place only if the sha256 of its first bytes is the one this tape's front index"
  echo "      lists for that slice: never by position or by name. A copy made with dd, still"
  echo "      padded to the tape block, will do. A file of the right size but another hash is"
  echo "      named and not used."
  echo "  --swap-tapes   (--restore, --verify) When slices are still missing after this tape has"
  echo "      been read, say which (their number and sha256), wait for ENTER with another cartridge"
  echo "      in the drive, and read just those slices from it: any tape whose front index lists the"
  echo "      same sha256 values. Each tape is read forward from one rewind. Type done and ENTER to"
  echo "      go on without them. Needs the input to be a terminal, or a line of input per swap."
  echo "      Either option restores the way an older dar does, a slice at a time onto disk, so --to"
  echo "      needs about twice the unit's size (--scratch DIR puts the slices on another disk)."
  echo "      Without them a slice that fails stops the restore, and says so."
  echo "  A restore in which dar names files it could not restore (a failed CRC, a slice it was"
  echo "      not given) writes their paths, one per line, to .tapectl-damaged-files.txt in the"
  echo "      unit's directory, restores the rest, and ends RESTORE INCOMPLETE with exit status 1."
  echo "  $0 --restore --key KEYFILE --to DIR --unit U --fill-from LIST"
  echo "      With another tape that holds the SAME unit AT THE SAME VERSION: restore only the"
  echo "      files LIST names, over their damaged copies, and nothing else in the directory. The"
  echo "      first line of the list records the unit, its uuid and its version, and a tape that"
  echo "      lists another is refused; no other version is ever used. dar checks each file, and so"
  echo "      does the sha256 check below. The list is rewritten with what is still damaged and"
  echo "      removed when nothing is. Give --to the same directory as the restore (the list is in"
  echo "      it, or in --to/UNIT)."
  echo "  --no-file-check   (--restore, with --fill-from too) dar checks each file it restores with a"
  echo "      short checksum, so a damaged file can come back with no error. A tape written since"
  echo "      version 1.3.0 records the sha256 of every regular file in its index, and after each"
  echo "      restore and fill this script reads the restored files back and checks them against it"
  echo "      (with --path, only the paths restored). A file that does not match, or is missing, goes"
  echo "      on the damaged-file list like one dar named, so --fill-from can take it from another"
  echo "      copy, and the run ends RESTORE INCOMPLETE. The cost is one read of the restored files"
  echo "      from disk; the tape is not read again. --no-file-check skips it. A file with no sha256"
  echo "      recorded is not checked, and a tape written before 1.3.0 has no index: both are said."
  echo "  --no-space-check   Skip that check (a compressed or thin-provisioned"
  echo "      filesystem can hold more than df reports)."
  echo "  --overwrite   --to must otherwise be empty or new: dar keeps a file"
  echo "      that is already there and reports success. --overwrite replaces"
  echo "      such files, to finish a restore of the same unit that stopped."
  echo ""
  echo "Environment:"
  echo "  TAPE_DEVICE   Tape device path (default: /dev/nst0)"
  echo "  TMPDIR        Where the work directory is made (default: /tmp). It holds the"
  echo "      decrypted envelopes (the operator envelope's catalog.db, the dar"
  echo "      catalogues), which grow with the number of files on the tape; --all"
  echo "      keeps every envelope it opens. No data slice goes there."
  echo ""
  echo "Requirements: mt (mt-st), __TOOLS_ALL__"
  ;;
"")
  echo "RESTORE.sh for tapectl volume $LABEL"
  echo "Run '$0 --help' for usage."
  ;;
*)
  # Exit 2, not 0. A typo'd mode used to print this and report success, so a
  # wrapper or cron job around the heir path read "restore worked" (#133).
  echo "RESTORE.sh for tapectl volume $LABEL" >&2
  echo "Unknown mode: $1" >&2
  echo "Run '$0 --help' for usage." >&2
  exit 2
  ;;
esac
"#;

    let result = script
        .replace("__AWK_PARSE_FILE_LIST__", AWK_PARSE_FILE_LIST)
        .replace("__AWK_CHECK_FILE_LIST__", AWK_CHECK_FILE_LIST)
        .replace("__AWK_FIND_ENVELOPE__", AWK_FIND_ENVELOPE)
        .replace("__AWK_MANIFEST_HAS_UNIT__", AWK_MANIFEST_HAS_UNIT)
        .replace("__AWK_UNIT_LIST__", AWK_UNIT_LIST)
        .replace("__AWK_SELECT_VERSION__", AWK_SELECT_VERSION)
        .replace("__AWK_MT_POSITION__", AWK_MT_POSITION)
        .replace("__AWK_UNITS_TABLE__", AWK_UNITS_TABLE)
        .replace("__AWK_UNIT_UUID__", AWK_UNIT_UUID)
        .replace("__AWK_INDEX_COMMON__", AWK_INDEX_COMMON)
        .replace("__AWK_INDEX_WHERE__", AWK_INDEX_WHERE)
        .replace("__AWK_INDEX_FIND__", AWK_INDEX_FIND)
        .replace("__AWK_DAR_DAMAGED__", AWK_DAR_DAMAGED)
        .replace("__AWK_FILL_LISTING__", AWK_FILL_LISTING)
        .replace("__AWK_FILE_CHECK_LISTED__", AWK_FILE_CHECK_LISTED)
        .replace("__AWK_FILE_CHECK_LIST__", AWK_FILE_CHECK_LIST)
        .replace("__AWK_FILE_CHECK_FAILED__", AWK_FILE_CHECK_FAILED)
        .replace("__TOOLS_READ__", &tools_for(MODE_READ).join(" "))
        .replace("__TOOLS_ENVELOPE__", &tools_for(MODE_ENVELOPE).join(" "))
        .replace("__TOOLS_LIST__", &tools_for(MODE_LIST).join(" "))
        .replace("__TOOLS_PATH__", &tools_for(MODE_PATH).join(" "))
        .replace("__TOOLS_RESTORE__", &tools_for(MODE_RESTORE).join(" "))
        .replace("__TOOLS_INDEX__", &tools_for(MODE_INDEX).join(" "))
        .replace("__TOOLS_ALL__", &restore_sh_tools().join(", "))
        .replace("__LABEL__", label)
        .replace("__TOTAL_FILES__", &total_files.to_string());

    // No named-fragment placeholder may survive assembly — a stray one would
    // mean a placeholder in the template does not match any const's name (or
    // vice versa), and would ship an inert `__AWK_...__` token straight onto
    // tape. Debug-only: `no_awk_placeholder_survives_assembly` (mod tests)
    // covers the release-mode case unconditionally.
    debug_assert!(
        !result.contains("__AWK_") && !result.contains("__TOOLS_"),
        "a placeholder was not substituted in generate_restore_script_v2"
    );

    result
}

/// Generate the planning header content — pre-v2 this was written as a
/// standalone tape file (File 3, encrypted to operator); at the v2 flip its
/// content becomes the `PLAN.toml` member of the operator envelope tar (same
/// recipients: operator + escrow) instead of a separate tape file
/// (`docs/design/volume-format-v2.md` §8 "What v2 removes" — the standalone
/// zone and `ZoneKind::PlanningHeader` are removed by the write flip, not by
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
pub fn generate_front_index(label: &str, files: &[FrontIndexFile]) -> String {
    let mut s = format!(
        r#"================================================================
                    TAPECTL FRONT INDEX
================================================================

Volume: {label}
This file maps every tape position to its type, on-tape byte size, and
the sha256 of its on-tape (encrypted) bytes. It contains NO content
metadata: no filenames, no plaintext checksums, no tenant or unit names.
The trailing seal marker binds this index by hash; see the system guide.

================================================================
              MACHINE-READABLE DATA (TOML)
================================================================

[index]
volume = "{label}"
layout_version = 2
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

/// Generate the plaintext **seal marker** (the last file, layout v2). Its
/// presence is the completeness assertion — "every file before me is present" —
/// and its `front_index_sha256` binds the front index, making the seal marker
/// the unhashed root of the keyless integrity chain (seal marker → front index →
/// every content file; ADR-0007, `volume-format-v2.md` §4). Its absence means
/// the tape is legitimately unsealed (interrupted or EOT-aborted).
///
/// `files` is the **embedded full copy of the front index** (ratified
/// 2026-07-22): two-ended redundancy — front-of-tape damage recovers the map
/// from the tail; tail damage reads as unsealed but stays navigable from the
/// front. By seal time File 3's bytes are known, so the caller fills in File 3's
/// own `size_bytes` + `sha256_encrypted` (more complete than File 3 itself);
/// only the seal marker's own entry stays hash-less (self-reference). The copy
/// is not hash-protected by anything on the tape — readers validate its per-file
/// claims by hashing the files they describe (`volume-format-v2.md` §4).
pub fn generate_seal_marker(
    label: &str,
    file_count: i32,
    front_index_sha256: &str,
    files: &[FrontIndexFile],
) -> String {
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
    let mut s = format!(
        r#"================================================================
                    TAPECTL SEAL MARKER
================================================================

Volume: {label}
This file seals the tape: its presence means every file before it is
present. Its absence means the tape is unsealed (interrupted or aborted).
It binds the front index by the sha256 below. The [[files]] entries are
a full copy of the front index (File 3), usable if File 3 is damaged —
verify any entry by hashing the file it describes.

================================================================
              MACHINE-READABLE DATA (TOML)
================================================================

[seal]
volume = "{label}"
layout_version = 2
requires = []
file_count = {file_count}
sealed_at = "{now}"
front_index_sha256 = "{front_index_sha256}"
"#
    );
    append_files_entries(&mut s, files);
    s
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

An index is as of the moment its own tape was written. It never names a
tape written after it, and a tape it names may since have been retired or
erased, so the NEWEST tape's index is the most complete. Nothing on a tape
can name tapes written after it, so the newest tape you hold may not be the
newest ever written. `RESTORE.sh --where UNIT` and `--find TEXT` (with your
key) read it for you.

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

"##;

/// RECOVERY.md's section on a damaged slice or file (ADR-0012 items 101 and
/// 102, tapectl 1.3.0): the two RESTORE.sh commands and the rule that only
/// the same unit at the same version is used. Like the index section it
/// serves every kind of envelope, and so it names no dar catalogue (#449).
const RECOVERY_DAMAGE_SECTION: &str = r##"## If a slice or a file is damaged

Another tape that holds this unit at the same version holds the same slices,
byte for byte: its front index (File 3) lists the same `sha256_encrypted`
for each, at whatever tape file and under whatever number that tape puts
it. Only the same unit at the same version is a copy; another version may
differ in the very files that are damaged.

A slice that fails its `sha256sum -c` above is damaged: do not decrypt it.
Take it from the other tape instead, found by its `sha256_encrypted` and by
nothing else. `RESTORE.sh` (tape file 2) does that for you, from slice files
you copied off the other tape (`dd`, block padding and all, under any names)
or from the other cartridge itself, which it asks for after it has read this
one:

```bash
./RESTORE.sh --restore --key YOUR_KEY.age.key --unit UNIT --slices-from DIR --to /destination
./RESTORE.sh --restore --key YOUR_KEY.age.key --unit UNIT --swap-tapes --to /destination
```

With no other copy, leave the failed slice out of the steps above: do not
decrypt it, and do not put an empty file in its place. The other slices keep
their numbers (`restore.1.dar`, `restore.2.dar`, `restore.4.dar` ..., a gap
where the failed one was), and dar, run as above into an empty directory
(`dar -x restore -R /destination -O -Q -N > dar.log 2>&1`), restores every
file whose slices are all there. For each of the others it prints `PATH not
restored (user choice)` (`grep 'not restored (user choice)' dar.log` lists
them) and leaves an EMPTY or short file of that name, which is not the real
file; its exit status is still 0. If the failed slice is the last one, dar
stops ("The last file of the set is not present"); add `-0` to restore, in
order, everything before the first missing slice.

If dar says it could not restore some files, `RESTORE.sh` restores the rest,
ends `RESTORE INCOMPLETE`, and writes their paths, one per line (a
backslash, TAB, newline and carriage return escaped as the index escapes
them), to `.tapectl-damaged-files.txt` in the unit's directory. Load a tape
that holds the same unit at the same version and run that tape's own
`RESTORE.sh`: it restores exactly those files over their damaged copies,
leaves everything else in the directory alone, and refuses a tape that holds
another unit or another version:

```bash
./RESTORE.sh --restore --key YOUR_KEY.age.key --unit UNIT --fill-from /destination/.tapectl-damaged-files.txt --to /destination
```

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
         slice was sealed to a key you have not supplied — the data is not corrupt.\n\n\
         ## Units in this envelope\n\n\
         | Unit | Snapshot | Slices | Tape files |\n\
         |------|----------|--------|------------|\n"
    );
    for unit in units {
        let (first, last) = match (unit.slices.first(), unit.slices.last()) {
            (Some(f), Some(l)) => (f.tape_position, l.tape_position),
            _ => (0, 0),
        };
        s.push_str(&format!(
            "| {} | v{} | {} | {}..{} |\n",
            unit.name,
            unit.snapshot_version,
            unit.slices.len(),
            first,
            last,
        ));
    }
    s.push('\n');

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
             once, and each read leaves it at the start of the next file.\n\
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
            // The number in `restore.N.dar` MUST be dar's slice number, and the
            // slices MUST share the base name `restore` — this is dar's required
            // `base.N.dar` convention. `truncate` trims the 512KB block padding
            // that would otherwise make age reject the ciphertext. One rewind
            // per unit (#412): a read leaves the tape at the next file, so
            // only a gap between slices needs a forward space.
            let position = match next {
                None => format!(
                    "mt -f /dev/nst0 rewind && mt -f /dev/nst0 fsf {}\n",
                    slice.tape_position
                ),
                Some(n) if slice.tape_position > n => {
                    format!("mt -f /dev/nst0 fsf {}\n", slice.tape_position - n)
                }
                Some(n) if slice.tape_position == n => String::new(),
                Some(_) => format!(
                    "mt -f /dev/nst0 rewind && mt -f /dev/nst0 fsf {}\n",
                    slice.tape_position
                ),
            };
            next = Some(slice.tape_position + 1);
            s.push_str(&format!(
                "# Slice {n} — tape file {pos}, {eb} bytes\n\
                 {position}\
                 dd if=/dev/nst0 bs=512k of=restore.{n}.dar.age\n\
                 truncate -s {eb} restore.{n}.dar.age\n\
                 echo \"{sha}  restore.{n}.dar.age\" | sha256sum -c -\n\
                 age -d -i YOUR_KEY.age.key restore.{n}.dar.age > restore.{n}.dar && rm restore.{n}.dar.age\n\n",
                n = slice.number,
                pos = slice.tape_position,
                eb = slice.encrypted_bytes,
                sha = slice.sha256_encrypted,
            ));
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
                 cannot read the last slice's catalogue, `-A {cat}` gives it this\n\
                 envelope's copy instead.",
                cat = catalogue,
            ));
        }
        s.push_str("\n\n");
    }

    s.push_str(RECOVERY_INDEX_SECTION);
    s.push_str(RECOVERY_DAMAGE_SECTION);
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
           every unit. Pass them all with repeated `-i`.\n",
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
    use crate::volume::restore_script::{
        AWK_CHECK_FILE_LIST, AWK_DAR_DAMAGED, AWK_FILE_CHECK_FAILED, AWK_FILE_CHECK_LIST,
        AWK_FILE_CHECK_LISTED, AWK_FILL_LISTING, AWK_FIND_ENVELOPE, AWK_INDEX_COMMON,
        AWK_INDEX_FIND, AWK_INDEX_WHERE, AWK_MANIFEST_HAS_UNIT, AWK_MT_POSITION,
        AWK_PARSE_FILE_LIST, AWK_SELECT_VERSION, AWK_UNITS_TABLE, AWK_UNIT_LIST, AWK_UNIT_UUID,
    };

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
        assert_eq!(idx.get("layout_version").unwrap().as_integer(), Some(2));

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

    #[test]
    fn seal_marker_binds_front_index() {
        let s = generate_seal_marker("TEST01", 7, "deadbeefcafe", &[]);
        let body = &s[s.find("[seal]").expect("has [seal]")..];
        let parsed: toml::Value = body.parse().expect("TOML parses");
        let seal = parsed.get("seal").unwrap();
        assert_eq!(seal.get("volume").unwrap().as_str(), Some("TEST01"));
        assert_eq!(seal.get("layout_version").unwrap().as_integer(), Some(2));
        assert_eq!(seal.get("file_count").unwrap().as_integer(), Some(7));
        assert_eq!(
            seal.get("front_index_sha256").unwrap().as_str(),
            Some("deadbeefcafe")
        );
        // sealed_at is present and RFC3339-parseable.
        let sealed_at = seal.get("sealed_at").unwrap().as_str().unwrap();
        assert!(chrono::DateTime::parse_from_rfc3339(sealed_at).is_ok());
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
        let files = vec![FrontIndexFile {
            position: 0,
            type_label: "id_thunk",
            size_bytes: Some(10),
            sha256_encrypted: Some("aa".into()),
        }];
        let first = generate_seal_marker("TEST01", 2, "fi", &files);
        for _ in 0..64 {
            let again = generate_seal_marker("TEST01", 2, "fi", &files);
            assert_eq!(
                first.len(),
                again.len(),
                "seal marker length varies between generations — the placeholder \
                 sizing trick (and therefore seal()) is broken"
            );
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
                position: 3,
                type_label: "front_index",
                size_bytes: Some(2048),                // known at seal time
                sha256_encrypted: Some("fi99".into()), // known at seal time
            },
            FrontIndexFile {
                position: 4,
                type_label: "seal_marker",
                size_bytes: None,
                sha256_encrypted: None, // self-reference: never hashable
            },
        ];
        let s = generate_seal_marker("TEST01", 5, "fi99", &files);
        let body = &s[s.find("[seal]").unwrap()..];
        let parsed: toml::Value = body.parse().expect("TOML parses");
        let arr = parsed.get("files").unwrap().as_array().unwrap();
        assert_eq!(arr.len(), 3);
        // File 3's entry in the COPY carries size + hash (unlike in File 3).
        assert_eq!(arr[1].get("type").unwrap().as_str(), Some("front_index"));
        assert_eq!(arr[1].get("size_bytes").unwrap().as_integer(), Some(2048));
        assert_eq!(
            arr[1].get("sha256_encrypted").unwrap().as_str(),
            Some("fi99")
        );
        // The seal marker's own entry stays hash-less.
        assert_eq!(arr[2].get("type").unwrap().as_str(), Some("seal_marker"));
        assert!(arr[2].get("sha256_encrypted").is_none());
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
            dar_version: Some("2.7.20".into()),
            dar_command: Some("dar -c base -R /src".into()),
            slices: vec![ManifestSlice {
                number: 1,
                tape_position: 4,
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
            dar_version: None,
            dar_command: None,
            slices: vec![ManifestSlice {
                number: 1,
                tape_position: 4,
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
            dar_version: None,
            dar_command: None,
            slices: vec![ManifestSlice {
                number: 1,
                tape_position: 4,
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
            dar_version: Some("2.7.20".into()),
            dar_command: Some("dar -c base -R /src".into()),
            slices: vec![ManifestSlice {
                number: 1,
                tape_position: 4,
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
        assert_eq!(u.get("dar_version").unwrap().as_str(), Some("2.7.20"));
        // #39: provenance fields for selective restore.
        assert_eq!(u.get("stage_set_id").unwrap().as_integer(), Some(7));
        assert_eq!(
            u.get("dar_command").unwrap().as_str(),
            Some("dar -c base -R /src")
        );
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
            Some("tapectl-volume-v2")
        );
        assert_eq!(volume.get("label").unwrap().as_str(), Some("TEST01"));
        assert_eq!(
            volume.get("uuid").unwrap().as_str(),
            Some("11111111-2222-3333-4444-555555555555")
        );
        assert_eq!(volume.get("layout_version").unwrap().as_integer(), Some(2));

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
        // 1.1.0 (ADR-0012 amendment item 15): File 0 states `requires = []`,
        // one line after layout_version. Undo exactly that line too.
        let requires = "layout_version = 2\nrequires = []\n";
        assert_eq!(rendered.matches(requires).count(), 1, "{rendered}");
        let pre_fix = rendered
            .replace(good, "tr -d '\\\\0' > GUIDE.md")
            .replace(requires, "layout_version = 2\n");
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
    fn restore_script_v2_is_valid_bash() {
        // T1 floor, v2: the generated emergency script must at least parse.
        let s = generate_restore_script_v2("SYN01", 20);
        let mut f = tempfile::NamedTempFile::new().unwrap();
        std::io::Write::write_all(&mut f, s.as_bytes()).unwrap();
        let out = std::process::Command::new("bash")
            .arg("-n")
            .arg(f.path())
            .output()
            .expect("run bash -n");
        assert!(
            out.status.success(),
            "RESTORE.sh v2 failed bash -n: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    #[test]
    fn restore_script_v2_has_all_modes_and_rung2_fallback() {
        let s = generate_restore_script_v2("VOL01", 27);
        // Block size matches tapectl's 512KB fixed block mode
        assert!(s.contains("BLOCK=524288"));
        // All four command modes (v2 adds --verify to v1's three)
        assert!(s.contains("--info)"));
        assert!(s.contains("--verify)"));
        assert!(s.contains("--find-envelope)"));
        assert!(s.contains("--restore)"));
        // Key operations
        assert!(s.contains("mt -f \"$DEVICE\" setblk"));
        assert!(s.contains("age -d -i"));
        assert!(s.contains("sha256sum"));
        assert!(s.contains("dar -x"));
        assert!(s.contains("truncate -s"));
        assert!(s.contains("head -c"));
        // Envelope is tar archive
        assert!(s.contains("tar xf"));
        assert!(s.contains("MANIFEST.toml"));
        // Exact §2.5 verdict tokens, tied to the actual assignment sites (not
        // just an incidental substring of one another).
        assert!(s.contains("verdict=\"SEALED\""));
        assert!(s.contains("verdict=\"UNSEALED\""));
        assert!(s.contains("verdict=\"DAMAGED (ends disagree)\""));
        // Rung-2 fallback: File 3 unreadable/inconsistent falls back to the
        // seal marker's embedded copy, loudly warned (sheet §3.4).
        assert!(s.contains("RUNG-2"));
        assert!(s.contains("degradation ladder"));
    }

    #[test]
    fn restore_script_v2_is_hardened() {
        let s = generate_restore_script_v2("HARD2", 20);
        // S2: every plaintext-tape value from the ID thunk is
        // integer-validated before use — v2's field set (front_index /
        // seal_marker / total_files) replaces v1's data_start/mini_index/etc.
        assert!(s.contains("require_uint()"));
        for key in ["front_index", "seal_marker", "total_files"] {
            assert!(
                s.contains(&format!("require_uint {key} ")),
                "missing require_uint for {key}"
            );
        }
        // S7: no predictable temp path; mktemp + restrictive umask instead.
        assert!(s.contains("mktemp -d"));
        assert!(s.contains("umask 077"));
        assert!(
            !s.contains("tapectl-restore-$$"),
            "predictable /tmp path leaked"
        );
    }

    #[test]
    fn v2_generators_never_mention_mini_index() {
        // The v1 mid-tape mini-index is gone in v2 (volume-format-v2.md §8);
        // its facts moved into the front index (File 3). Neither v2 generator
        // may reference it under either spelling, in any case.
        let guide = generate_system_guide_v2("MINI1", 12).to_lowercase();
        let script = generate_restore_script_v2("MINI1", 12).to_lowercase();
        for needle in ["mini-index", "mini_index"] {
            assert!(
                !guide.contains(needle),
                "v2 guide must not mention '{needle}'"
            );
            assert!(
                !script.contains(needle),
                "v2 restore script must not mention '{needle}'"
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
        let script = generate_restore_script_v2("LOCK01", 20);
        assert!(guide.contains(&format!(
            "setblk {}",
            crate::cli::volume::DEFAULT_BLOCK_SIZE
        )));
        assert!(script.contains(&format!("BLOCK={}", crate::cli::volume::DEFAULT_BLOCK_SIZE)));
    }

    /// RESTORE.sh sanitizes the tape's label before echoing it (the ID thunk is
    /// unauthenticated), and then compares it against the label baked into the
    /// script. Those two things are only safe together while `safe_str`'s
    /// allowlist is a SUPERSET of what `validate_volume_label` accepts: if a
    /// legitimate label were altered by sanitizing, it could never equal
    /// `$LABEL`, and every correct tape would raise a permanent "WRONG TAPE"
    /// warning — and now also fail the gate's heir_info assertion.
    ///
    /// This runs the real `safe_str` out of the real generated script, so
    /// widening `validate_segment` without widening the allowlist fails here.
    #[test]
    fn safe_str_never_alters_a_label_that_tapectl_would_accept() {
        use std::io::Write;
        use std::process::{Command, Stdio};

        // The widest label validate_volume_label accepts: every permitted
        // character class, at exactly the maximum length, and not starting with
        // a dash or dot.
        let mut label = String::from("Az9");
        while label.len() < 64 {
            label.push_str("._-Az9");
        }
        label.truncate(64);
        crate::naming::validate_volume_label(&label)
            .expect("test fixture must itself be a valid volume label");

        let script = generate_restore_script_v2("PROBE1", 20);
        let body = script
            .split_once("safe_str() {")
            .and_then(|(_, rest)| rest.split_once("\n}"))
            .map(|(b, _)| b)
            .expect("generated RESTORE.sh must define safe_str()");

        let mut child = Command::new("bash")
            .args(["-s", "--", &label])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .expect("spawn bash");
        write!(
            child.stdin.as_mut().unwrap(),
            "safe_str() {{{body}\n}}\nsafe_str \"$1\"\n"
        )
        .unwrap();
        drop(child.stdin.take());
        let out = child.wait_with_output().expect("bash ran");
        let got = String::from_utf8_lossy(&out.stdout).to_string();
        assert_eq!(
            got,
            label,
            "safe_str altered a legitimate {}-char label — every correct tape \
             would now warn WRONG TAPE. Widen the tr allowlist in RESTORE.sh to \
             cover validate_segment, or narrow validate_segment.",
            label.len()
        );
    }

    /// Issue #131: a volume can carry the SAME unit twice — two snapshot
    /// versions, two stage sets. RESTORE.sh used to collect every matching
    /// [[units]] block, concatenating both versions' slices and handing the mix
    /// to dar. When the versions had different recipients (a `tenant reassign`
    /// between them) the first slice was one the key could not open, and the
    /// failure read as "wrong key" — on the heir path, where there is no DB to
    /// fall back on.
    ///
    /// Drives the real awk out of the real generated script, so the selection
    /// rule cannot rot independently of the script that ships.
    #[test]
    fn restore_sh_picks_one_version_of_a_unit_present_twice() {
        use std::process::{Command, Stdio};

        let program = AWK_SELECT_VERSION;

        // Built through the real writer, not hand-typed: a hand-typed sample
        // would keep parsing after the writer's shape changed underneath it.
        let unit =
            |name: &str, version: i64, tape_position: i64, eb: i64, sha: &str| ManifestUnit {
                name: name.to_string(),
                uuid: format!("uuid-{name}-{version}"),
                snapshot_version: version,
                stage_set_id: version,
                dar_version: None,
                dar_command: None,
                slices: vec![ManifestSlice {
                    number: 1,
                    tape_position,
                    size_bytes: 1,
                    encrypted_bytes: eb,
                    sha256_plain: "x".repeat(64),
                    sha256_encrypted: sha.to_string(),
                }],
            };
        let manifest = generate_manifest_toml(
            "MULTI1",
            "alice",
            &[
                unit("photos", 1, 20, 111, "aaa"),
                unit("docs", 1, 21, 222, "bbb"),
                unit("photos", 2, 30, 333, "ccc"),
            ],
        );
        let dir = std::env::temp_dir().join(format!("tapectl-awk-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let man = dir.join("MANIFEST.toml");
        std::fs::write(&man, manifest).unwrap();
        let prog = dir.join("sel.awk");
        std::fs::write(&prog, program).unwrap();

        let run = |unit: &str, want: &str| -> String {
            let mut c = Command::new("awk")
                .args([
                    "-v",
                    &format!("unit={unit}"),
                    "-v",
                    &format!("want={want}"),
                    "-f",
                    prog.to_str().unwrap(),
                    man.to_str().unwrap(),
                ])
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
                .spawn()
                .expect("spawn awk");
            let _ = c.stdin.take();
            let o = c.wait_with_output().unwrap();
            String::from_utf8_lossy(&o.stdout).to_string()
        };

        // Newest version by default — NOT both versions concatenated.
        let newest = run("photos", "");
        assert_eq!(
            newest.lines().count(),
            1,
            "expected exactly one version's slices, got:\n{newest}"
        );
        assert!(newest.contains("|30|"), "expected v2's slice:\n{newest}");
        assert!(
            !newest.contains("|20|"),
            "v1's slice must not be mixed in:\n{newest}"
        );

        // An explicit older version is selectable.
        let v1 = run("photos", "1");
        assert!(v1.contains("|20|"), "expected v1's slice:\n{v1}");
        assert!(!v1.contains("|30|"), "v2 must not leak in:\n{v1}");

        // A unit present once is unaffected, and a missing version yields
        // nothing (the caller turns that into a clear error).
        assert!(run("docs", "").contains("|21|"));
        assert!(run("photos", "9").trim().is_empty());

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// #133: the sibling the #131 fix did not cover. `--restore` without
    /// `--unit` collects the unit names from the manifest and auto-selects
    /// when there is exactly one. A unit stored in two snapshot versions has
    /// two `[[units]]` blocks, so it was listed twice and took the "multiple
    /// units" branch — telling the heir to disambiguate between a name and
    /// itself. `do_find_envelope` suggests exactly that no-`--unit` form, so
    /// the script's own advice walked into it.
    ///
    /// The #131 test runs the version-selecting awk standalone with an
    /// explicit `unit=`, which is precisely why it never saw this.
    #[test]
    fn restore_sh_lists_a_unit_stored_twice_only_once() {
        use std::process::{Command, Stdio};

        let program = AWK_UNIT_LIST;

        // Built through the real writer, not hand-typed (see the sibling
        // #131 test above for why).
        let unit = |name: &str, version: i64| ManifestUnit {
            name: name.to_string(),
            uuid: format!("uuid-{name}-{version}"),
            snapshot_version: version,
            stage_set_id: version,
            dar_version: None,
            dar_command: None,
            slices: vec![],
        };
        let manifest = generate_manifest_toml(
            "MULTI2",
            "alice",
            &[unit("photos", 1), unit("photos", 2), unit("docs", 1)],
        );
        let dir = std::env::temp_dir().join(format!("tapectl-names-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let man = dir.join("MANIFEST.toml");
        std::fs::write(&man, manifest).unwrap();
        let prog = dir.join("names.awk");
        std::fs::write(&prog, program).unwrap();

        let out = Command::new("awk")
            .args(["-f", prog.to_str().unwrap(), man.to_str().unwrap()])
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .output()
            .expect("spawn awk");
        let names: Vec<String> = String::from_utf8_lossy(&out.stdout)
            .lines()
            .map(str::to_string)
            .collect();

        assert_eq!(
            names,
            vec!["photos".to_string(), "docs".to_string()],
            "a unit stored in two versions must be named once, in first-seen order"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// #135: a `name` key outside the `[[units]]` head must not retarget the
    /// version selector. Nothing emits one today — `naming.rs` also bans the
    /// characters that would break the field split — so this guards the
    /// direction of travel, not a live defect. The failure it prevents is the
    /// worst kind: the selector silently switches units mid-block and the
    /// restore succeeds with part of the data.
    #[test]
    fn a_name_key_outside_the_units_head_does_not_retarget_the_selector() {
        use std::process::{Command, Stdio};

        let program = AWK_SELECT_VERSION;

        // A slice table carrying its own `name`, as some future field might.
        let manifest = "\
[[units]]
name = \"photos\"
snapshot_version = 1

[[units.slices]]
number = 1
name = \"decoy\"
tape_position = 20
encrypted_bytes = 111
sha256_encrypted = \"aaa\"

[[units.slices]]
number = 2
tape_position = 21
encrypted_bytes = 222
sha256_encrypted = \"bbb\"
";
        let dir = std::env::temp_dir().join(format!("tapectl-guard-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let man = dir.join("MANIFEST.toml");
        std::fs::write(&man, manifest).unwrap();
        let prog = dir.join("sel.awk");
        std::fs::write(&prog, program).unwrap();

        let out = Command::new("awk")
            .args([
                "-v",
                "unit=photos",
                "-v",
                "want=",
                "-f",
                prog.to_str().unwrap(),
                man.to_str().unwrap(),
            ])
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .output()
            .expect("spawn awk");
        let got = String::from_utf8_lossy(&out.stdout).to_string();

        assert_eq!(
            got.lines().count(),
            2,
            "both slices must survive a decoy name key:\n{got}"
        );
        assert!(got.contains("|20|") && got.contains("|21|"), "{got}");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The generated script must be syntactically valid bash. Cheap, and it
    /// catches the trap this file keeps setting: the awk programs are
    /// single-quoted in the shell, so one apostrophe anywhere inside one —
    /// including in a comment — ends the program early and breaks the script
    /// that gets frozen onto tape. `bash -n` parses without executing.
    #[test]
    fn the_generated_script_parses_as_bash() {
        use std::io::Write;
        use std::process::{Command, Stdio};

        let script = generate_restore_script_v2("SYNTAX1", 20);
        let mut child = Command::new("bash")
            .arg("-n")
            .stdin(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn bash -n");
        child
            .stdin
            .as_mut()
            .unwrap()
            .write_all(script.as_bytes())
            .unwrap();
        let out = child.wait_with_output().unwrap();
        assert!(
            out.status.success(),
            "generated RESTORE.sh is not valid bash:\n{}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    /// Architecture review 2026-09-11 (candidate C6): every named `awk`
    /// fragment must reassemble into the generated script byte-for-byte —
    /// if a placeholder's boundaries or a const's content were ever off by
    /// one character, this fails here instead of only as an opaque
    /// `on_tape_golden` hash mismatch.
    #[test]
    fn every_named_awk_fragment_is_present_verbatim_in_the_assembled_script() {
        let script = generate_restore_script_v2("FRAG01", 20);
        for (name, fragment) in [
            ("AWK_PARSE_FILE_LIST", AWK_PARSE_FILE_LIST),
            ("AWK_CHECK_FILE_LIST", AWK_CHECK_FILE_LIST),
            ("AWK_FIND_ENVELOPE", AWK_FIND_ENVELOPE),
            ("AWK_MANIFEST_HAS_UNIT", AWK_MANIFEST_HAS_UNIT),
            ("AWK_UNIT_LIST", AWK_UNIT_LIST),
            ("AWK_SELECT_VERSION", AWK_SELECT_VERSION),
            ("AWK_MT_POSITION", AWK_MT_POSITION),
            ("AWK_UNITS_TABLE", AWK_UNITS_TABLE),
            ("AWK_UNIT_UUID", AWK_UNIT_UUID),
            ("AWK_INDEX_COMMON", AWK_INDEX_COMMON),
            ("AWK_INDEX_WHERE", AWK_INDEX_WHERE),
            ("AWK_INDEX_FIND", AWK_INDEX_FIND),
            ("AWK_DAR_DAMAGED", AWK_DAR_DAMAGED),
            ("AWK_FILL_LISTING", AWK_FILL_LISTING),
            ("AWK_FILE_CHECK_LISTED", AWK_FILE_CHECK_LISTED),
            ("AWK_FILE_CHECK_LIST", AWK_FILE_CHECK_LIST),
            ("AWK_FILE_CHECK_FAILED", AWK_FILE_CHECK_FAILED),
        ] {
            assert!(
                script.contains(fragment),
                "{name} is not a verbatim substring of the assembled script"
            );
        }
    }

    /// Run an awk program (`AWK_INDEX_COMMON` first when `common`) over `input`
    /// with `env`, under every awk this host has, and return what each prints
    /// with its exit status. The three must agree.
    fn run_awk(
        common: bool,
        program: &str,
        input: &std::path::Path,
        env: &[(&str, String)],
    ) -> (i32, String) {
        use std::process::Command;
        let text = if common {
            format!("{AWK_INDEX_COMMON} {program}")
        } else {
            program.to_string()
        };
        let mut seen: Vec<(i32, String)> = Vec::new();
        for awk in ["awk", "mawk", "gawk"] {
            let mut cmd = Command::new(awk);
            cmd.arg(&text).arg(input);
            for (k, v) in env {
                cmd.env(k, v);
            }
            let Ok(o) = cmd.output() else { continue };
            seen.push((
                o.status.code().unwrap_or(-1),
                String::from_utf8_lossy(&o.stdout).to_string(),
            ));
        }
        assert!(!seen.is_empty(), "no awk on this host");
        assert!(
            seen.windows(2).all(|w| w[0] == w[1]),
            "awks differ: {seen:?}"
        );
        seen.remove(0)
    }

    /// ADR-0012 item 103: the three fragments of the file check, on text as the
    /// index writes it (names holding a TAB, a newline and a backslash), under
    /// every awk on the host. Which version an envelope lists; what a version
    /// asks to be checked; and which names `sha256sum -c --quiet` named, read
    /// from its REAL output for a changed file, a missing one and a name that
    /// looks like the checker's own words.
    #[test]
    fn the_file_check_fragments_read_the_index_and_the_checkers_report() {
        use std::process::{Command, Stdio};
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path();
        let hash = |n: u8| format!("{n:02x}").repeat(32);

        // Which version an envelope lists: by decoded name, version and, when
        // both sides state one, the uuid.
        let units = d.join("units.tsv");
        std::fs::write(
            &units,
            "volume\tunit\tunit_uuid\tzz_future\tversion\nV1\tphotos/2019\tuuid-a\tx\t2\nV2\tdocs\tuuid-b\tx\t1\nV3\ta\\tb\t\tx\t1\n",
        )
        .unwrap();
        let listed = |unit: &str, ver: &str, uuid: &str| {
            run_awk(
                true,
                AWK_FILE_CHECK_LISTED,
                &units,
                &[
                    ("CHK_UNIT", unit.to_string()),
                    ("CHK_VER", ver.to_string()),
                    ("CHK_UUID", uuid.to_string()),
                ],
            )
            .0
        };
        assert_eq!(listed("photos/2019", "2", "uuid-a"), 0);
        assert_eq!(listed("photos/2019", "2", ""), 0, "no uuid to compare");
        assert_eq!(listed("photos/2019", "1", "uuid-a"), 1, "another version");
        assert_eq!(
            listed("photos/2019", "2", "uuid-z"),
            1,
            "another unit of the name"
        );
        assert_eq!(
            listed("a\tb", "1", "uuid-q"),
            0,
            "the row states no uuid; the name is decoded"
        );
        assert_eq!(listed("nothing", "1", ""), 1);
        let bad = d.join("bad.tsv");
        std::fs::write(&bad, "volume\tversion\nV1\t1\n").unwrap();
        assert_eq!(
            run_awk(true, AWK_FILE_CHECK_LISTED, &bad, &[]).0,
            2,
            "a file with no unit column cannot be read"
        );

        // What a version asks to be checked.
        let paths = d.join("paths.tsv");
        let h1 = hash(0xab);
        let h2 = hash(0xcd);
        std::fs::write(
            &paths,
            format!(
                "unit\tversion\tkind\tsize_bytes\tsha256\tpath\n\
                 docs\t1\td\t0\t\tsub\n\
                 docs\t1\tf\t5\t{h1}\tsub/a\\tb\n\
                 docs\t1\tf\t5\t{h2}\tc\\\\d.txt\n\
                 docs\t1\tf\t5\t{h1}\tl\\nf.txt\n\
                 docs\t1\tf\t7\t\tnohash\n\
                 docs\t1\tf\t7\tnot-a-hash\tbadhash\n\
                 docs\t1\tl\t7\t{h1}\ta-link\n\
                 docs\t2\tf\t7\t{h1}\tother\n\
                 docs\t1\tf\t7\t{h1}\t../escape\n\
                 docs\t1\tf\t7\t{h1}\t/absolute\n\
                 docs\t1\tf\t7\t{h2}\tplain\n"
            ),
        )
        .unwrap();
        let outs = |n: &str| d.join(n);
        let env = |only: &str| -> Vec<(&'static str, String)> {
            vec![
                ("CHK_UNIT", "docs".into()),
                ("CHK_VER", "1".into()),
                ("CHK_ONLY", only.into()),
                ("CHK_SUMS", outs("sums").display().to_string()),
                ("CHK_SPECIAL", outs("special").display().to_string()),
                ("CHK_COUNTS", outs("counts").display().to_string()),
            ]
        };
        let (code, _) = run_awk(true, AWK_FILE_CHECK_LIST, &paths, &env(""));
        assert_eq!(code, 0);
        // rows of the version; regular files; checked; of those special; no hash
        assert_eq!(
            std::fs::read_to_string(outs("counts")).unwrap().trim(),
            "10 8 4 2 4",
            "badhash, nohash, ../escape and /absolute are never hashed"
        );
        assert_eq!(
            std::fs::read_to_string(outs("sums")).unwrap(),
            format!("{h1}  sub/a\tb\n{h2}  plain\n"),
            "a TAB stays in the name; no backslash or newline reaches this list"
        );
        assert_eq!(
            std::fs::read_to_string(outs("special")).unwrap(),
            format!("{h2}\tc\\\\d.txt\n{h1}\tl\\nf.txt\n"),
            "a backslash or a newline is kept in its escaped form"
        );

        // Only some paths: a name, and a folder with what is under it.
        let only = d.join("only");
        std::fs::write(&only, "plain\nsub\n").unwrap();
        run_awk(
            true,
            AWK_FILE_CHECK_LIST,
            &paths,
            &env(only.to_str().unwrap()),
        );
        assert_eq!(
            std::fs::read_to_string(outs("sums")).unwrap(),
            format!("{h1}  sub/a\tb\n{h2}  plain\n")
        );
        assert_eq!(std::fs::read_to_string(outs("special")).unwrap(), "");
        std::fs::write(&only, "pla\nsu\n").unwrap();
        run_awk(
            true,
            AWK_FILE_CHECK_LIST,
            &paths,
            &env(only.to_str().unwrap()),
        );
        assert_eq!(
            std::fs::read_to_string(outs("sums")).unwrap(),
            "",
            "a prefix of a name is not the name"
        );

        // An index written before the column existed records no hash at all.
        let old = d.join("old.tsv");
        std::fs::write(
            &old,
            "unit\tversion\tkind\tsize_bytes\tpath\ndocs\t1\td\t0\tsub\ndocs\t1\tf\t5\tsub/a\n",
        )
        .unwrap();
        run_awk(true, AWK_FILE_CHECK_LIST, &old, &env(""));
        assert_eq!(
            std::fs::read_to_string(outs("counts")).unwrap().trim(),
            "2 1 0 0 1"
        );
        assert_eq!(std::fs::read_to_string(outs("sums")).unwrap(), "");
        assert_eq!(
            run_awk(true, AWK_FILE_CHECK_LIST, &bad, &env("")).0,
            2,
            "no path column"
        );

        // The checker's report, from the real sha256sum: a changed file, two
        // missing ones, and a file whose name ends in the checker's own words.
        let tree = d.join("tree");
        std::fs::create_dir_all(tree.join("sub")).unwrap();
        let sha = |bytes: &[u8]| {
            let mut c = Command::new("sha256sum")
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .spawn()
                .unwrap();
            use std::io::Write;
            c.stdin.take().unwrap().write_all(bytes).unwrap();
            let o = c.wait_with_output().unwrap();
            String::from_utf8_lossy(&o.stdout)[..64].to_string()
        };
        let files: [(&str, &[u8], &[u8]); 5] = [
            ("a b", b"one", b"one"),
            ("ok", b"five", b"five"),
            ("sub/f", b"four", b"WRONG"),
            ("tab\tname", b"two", b"two"),
            ("x: FAILED", b"three", b"three"),
        ];
        let mut list = String::new();
        for (name, on_disk, recorded) in files {
            std::fs::write(tree.join(name), on_disk).unwrap();
            list.push_str(&format!("{}  {name}\n", sha(recorded)));
        }
        list.push_str(&format!("{}  gone\n{}  z-last\n", hash(1), hash(2)));
        std::fs::write(outs("list"), &list).unwrap();
        let out = Command::new("sha256sum")
            .args(["-c", "--quiet"])
            .current_dir(&tree)
            .stdin(std::fs::File::open(outs("list")).unwrap())
            .output()
            .unwrap();
        std::fs::write(outs("report"), &out.stdout).unwrap();
        let fenv = |report: &std::path::Path| -> Vec<(&'static str, String)> {
            vec![
                ("CHK_REPORT", report.display().to_string()),
                ("CHK_BAD", outs("named").display().to_string()),
                ("CHK_COUNTS", outs("fcounts").display().to_string()),
            ]
        };
        run_awk(
            false,
            AWK_FILE_CHECK_FAILED,
            &outs("list"),
            &fenv(&outs("report")),
        );
        assert_eq!(
            std::fs::read_to_string(outs("named")).unwrap(),
            "sub/f\ngone\nz-last\n"
        );
        assert_eq!(
            std::fs::read_to_string(outs("fcounts")).unwrap().trim(),
            "3 0 0"
        );
        // A failure the list does not hold cannot be named, and is counted.
        std::fs::write(
            outs("short"),
            list.replace(&format!("{}  gone\n", hash(1)), ""),
        )
        .unwrap();
        run_awk(
            false,
            AWK_FILE_CHECK_FAILED,
            &outs("short"),
            &fenv(&outs("report")),
        );
        assert_eq!(
            std::fs::read_to_string(outs("named")).unwrap(),
            "sub/f\nz-last\n",
            "a failure the list lacks does not hide the ones after it"
        );
        assert_eq!(
            std::fs::read_to_string(outs("fcounts")).unwrap().trim(),
            "2 1 0"
        );
        std::fs::write(
            outs("noise"),
            "sha256sum: WARNING: 1 computed checksum did NOT match\n",
        )
        .unwrap();
        run_awk(
            false,
            AWK_FILE_CHECK_FAILED,
            &outs("list"),
            &fenv(&outs("noise")),
        );
        assert_eq!(
            std::fs::read_to_string(outs("fcounts")).unwrap().trim(),
            "0 0 1",
            "a line that is no failure is counted, not matched"
        );
    }

    /// ADR-0012 item 102: the fragment that reads dar's messages, on dar's
    /// REAL output for names that break a line-by-line reading: a name holding
    /// the very separator dar prints, a newline, a TAB and a backslash. Each
    /// comes out once, escaped as sec 1.5 escapes a path; the count it names
    /// equals the one in dar's own summary. Needs `dar` and `awk` (issue #43).
    #[test]
    fn the_damaged_file_reader_names_every_file_dar_could_not_restore() {
        use std::process::Command;
        let work = tempfile::tempdir().unwrap();
        let dir = work.path();
        let src = dir.join("src");
        std::fs::create_dir_all(&src).unwrap();
        let names = [
            "plain.bin",
            "we ird : name.bin",
            "nl\nline.bin",
            "tab\there.bin",
            "back\\slash.bin",
        ];
        for (i, n) in names.iter().enumerate() {
            let body: Vec<u8> = (0..50_000u32)
                .map(|k| (k.wrapping_mul(2654435761).wrapping_add(i as u32 * 97) >> 13) as u8)
                .collect();
            std::fs::write(src.join(n), body).unwrap();
        }
        let dar = |args: &[&str]| {
            Command::new("dar")
                .args(args)
                .env("HOME", "/nonexistent")
                .output()
                .expect("dar must be on PATH (issue #43)")
        };
        let arch = dir.join("arch");
        let o = dar(&[
            "-c",
            arch.to_str().unwrap(),
            "-R",
            src.to_str().unwrap(),
            "-s",
            "50k",
            "-Q",
            "-N",
        ]);
        assert!(o.status.success());
        // Damage every slice but the last (it holds the catalogue), in
        // several places.
        let mut n = 1;
        while dir.join(format!("arch.{}.dar", n + 1)).exists() {
            let p = dir.join(format!("arch.{n}.dar"));
            let mut b = std::fs::read(&p).unwrap();
            for off in [1_000usize, 10_007, 20_015, 30_031, 40_063] {
                b[off] ^= 0xff;
            }
            std::fs::write(&p, b).unwrap();
            n += 1;
        }
        assert!(n > 2, "positive control: several slices were damaged");
        let out = dir.join("out");
        std::fs::create_dir_all(&out).unwrap();
        let o = dar(&[
            "-x",
            arch.to_str().unwrap(),
            "-R",
            out.to_str().unwrap(),
            "-O",
            "-Q",
            "-N",
        ]);
        assert_eq!(o.status.code(), Some(5), "dar says some files failed");
        let log = format!(
            "{}{}",
            String::from_utf8_lossy(&o.stdout),
            String::from_utf8_lossy(&o.stderr)
        );
        let log_path = dir.join("dar.log");
        std::fs::write(&log_path, &log).unwrap();
        let counts = dir.join("counts");
        let o = Command::new("awk")
            .arg(AWK_DAR_DAMAGED)
            .arg(&log_path)
            .env("DAMAGED_ROOT", &out)
            .env("DAMAGED_SKIPS", "0")
            .env("DAMAGED_COUNTS", &counts)
            .output()
            .expect("awk");
        assert!(o.status.success());
        let mut listed: Vec<String> = String::from_utf8_lossy(&o.stdout)
            .lines()
            .map(str::to_string)
            .collect();
        let esc = |s: &str| {
            s.replace('\\', "\\\\")
                .replace('\t', "\\t")
                .replace('\n', "\\n")
                .replace('\r', "\\r")
        };
        let mut want: Vec<String> = names
            .iter()
            .filter(|n| std::fs::read(out.join(n)).ok() != std::fs::read(src.join(n)).ok())
            .map(|n| esc(n))
            .collect();
        assert!(want.len() >= 3, "positive control: {want:?}\n{log}");
        listed.sort();
        want.sort();
        assert_eq!(listed, want, "{log}");
        let c = std::fs::read_to_string(&counts).unwrap();
        let nums: Vec<usize> = c.split_whitespace().map(|x| x.parse().unwrap()).collect();
        assert_eq!(nums[0], listed.len(), "errors named: {c}");
        assert_eq!(nums[2], 0, "none left unnamed: {c}");
        assert_eq!(nums[3], listed.len(), "dar's own failed count: {c}");
        assert!(
            listed.iter().any(|l| l == "we ird : name.bin"),
            "a name holding the separator keeps its whole name: {listed:?}"
        );

        // The other form dar prints: a slice that was never there.
        let clean = dir.join("clean");
        std::fs::create_dir_all(&clean).unwrap();
        let o = dar(&[
            "-c",
            clean.join("arch").to_str().unwrap(),
            "-R",
            src.to_str().unwrap(),
            "-s",
            "50k",
            "-Q",
            "-N",
        ]);
        assert!(o.status.success());
        std::fs::remove_file(clean.join("arch.2.dar")).unwrap();
        let out2 = dir.join("out2");
        std::fs::create_dir_all(&out2).unwrap();
        let o = dar(&[
            "-x",
            clean.join("arch").to_str().unwrap(),
            "-R",
            out2.to_str().unwrap(),
            "-O",
            "-Q",
            "-N",
        ]);
        let log2 = format!(
            "{}{}",
            String::from_utf8_lossy(&o.stdout),
            String::from_utf8_lossy(&o.stderr)
        );
        std::fs::write(&log_path, &log2).unwrap();
        let run = |skips: &str| -> Vec<String> {
            let o = Command::new("awk")
                .arg(AWK_DAR_DAMAGED)
                .arg(&log_path)
                .env("DAMAGED_ROOT", &out2)
                .env("DAMAGED_SKIPS", skips)
                .env("DAMAGED_COUNTS", &counts)
                .output()
                .expect("awk");
            String::from_utf8_lossy(&o.stdout)
                .lines()
                .map(str::to_string)
                .collect()
        };
        let mut skipped = run("1");
        let mut want2: Vec<String> = names
            .iter()
            .filter(|n| std::fs::read(out2.join(n)).ok() != std::fs::read(src.join(n)).ok())
            .map(|n| esc(n))
            .collect();
        assert!(
            !want2.is_empty(),
            "positive control: a slice's files are missing\n{log2}"
        );
        skipped.sort();
        want2.sort();
        assert_eq!(skipped, want2, "{log2}");
        assert!(
            run("0").is_empty(),
            "without a missing slice, 'user choice' means a file that was already there"
        );
    }

    /// The template substitutes each `__AWK_*__` placeholder for its named
    /// const before `__LABEL__`/`__TOTAL_FILES__`; a stray one surviving
    /// would mean a placeholder and a const's name drifted apart, and would
    /// ship an inert token straight onto tape.
    #[test]
    fn no_awk_placeholder_survives_assembly() {
        let script = generate_restore_script_v2("FRAG02", 20);
        assert!(
            !script.contains("__AWK_"),
            "an __AWK_*__ placeholder was not substituted"
        );
    }

    /// #135's near-miss: every named fragment is single-quoted in the
    /// generated shell script, so a `'` anywhere inside one — including
    /// inside an awk comment — would end the quoting early and corrupt
    /// every RESTORE.sh built from it.
    #[test]
    fn no_named_fragment_contains_an_apostrophe() {
        for (name, fragment) in [
            ("AWK_PARSE_FILE_LIST", AWK_PARSE_FILE_LIST),
            ("AWK_CHECK_FILE_LIST", AWK_CHECK_FILE_LIST),
            ("AWK_FIND_ENVELOPE", AWK_FIND_ENVELOPE),
            ("AWK_MANIFEST_HAS_UNIT", AWK_MANIFEST_HAS_UNIT),
            ("AWK_UNIT_LIST", AWK_UNIT_LIST),
            ("AWK_SELECT_VERSION", AWK_SELECT_VERSION),
            ("AWK_MT_POSITION", AWK_MT_POSITION),
            ("AWK_UNITS_TABLE", AWK_UNITS_TABLE),
            ("AWK_UNIT_UUID", AWK_UNIT_UUID),
            ("AWK_INDEX_COMMON", AWK_INDEX_COMMON),
            ("AWK_INDEX_WHERE", AWK_INDEX_WHERE),
            ("AWK_INDEX_FIND", AWK_INDEX_FIND),
            ("AWK_DAR_DAMAGED", AWK_DAR_DAMAGED),
            ("AWK_FILL_LISTING", AWK_FILL_LISTING),
            ("AWK_FILE_CHECK_LISTED", AWK_FILE_CHECK_LISTED),
            ("AWK_FILE_CHECK_LIST", AWK_FILE_CHECK_LIST),
            ("AWK_FILE_CHECK_FAILED", AWK_FILE_CHECK_FAILED),
        ] {
            assert!(!fragment.contains('\''), "{name} contains an apostrophe");
        }
    }

    /// #133 defects 2 and 4, at the process boundary: how the script answers
    /// argv. Both are silent-success failures, which no assertion on the
    /// script's *text* would catch, so this runs the real thing.
    ///
    /// Every path here returns before any tape I/O — the mode dispatch and the
    /// argument loop both sit ahead of it — so this needs no device.
    #[test]
    fn restore_sh_reports_bad_invocations_instead_of_exiting_quietly() {
        use std::os::unix::fs::PermissionsExt;
        use std::process::Command;

        let dir = std::env::temp_dir().join(format!("tapectl-argv-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let sh = dir.join("RESTORE.sh");
        std::fs::write(&sh, generate_restore_script_v2("ARGV1", 20)).unwrap();

        // The script's prerequisite loop refuses to run without mt/age/dar, and
        // CI has none of them — it died with "missing required tool: age"
        // before reaching the dispatch under test. Stub every required tool on
        // PATH instead of installing tape software into CI: argv handling has
        // nothing to do with those binaries, and none of the paths exercised
        // here ever runs one. `command -v` only checks for an executable file,
        // so the stubs are never executed either.
        let bin = dir.join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        for tool in [
            "mt",
            "dd",
            "age",
            "dar",
            "sha256sum",
            "head",
            "truncate",
            "tar",
        ] {
            let stub = bin.join(tool);
            std::fs::write(&stub, "#!/bin/sh\nexit 0\n").unwrap();
            std::fs::set_permissions(&stub, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let path = format!(
            "{}:{}",
            bin.display(),
            std::env::var("PATH").unwrap_or_default()
        );

        // Invoked as `bash RESTORE.sh`, not exec'd. A script written moments
        // earlier and then exec'd hits ETXTBSY whenever a sibling test thread
        // forks while this file's write fd is still open in the parent — the
        // classic fork/exec race, and nothing to do with what is being tested.
        let run = |args: &[&str]| -> (i32, String) {
            let o = Command::new("bash")
                .arg(&sh)
                .args(args)
                .env("PATH", &path)
                .output()
                .expect("spawn script");
            let mut text = String::from_utf8_lossy(&o.stdout).to_string();
            text.push_str(&String::from_utf8_lossy(&o.stderr));
            (o.status.code().unwrap_or(-1), text)
        };

        // Bare invocation is a friendly no-op and stays successful.
        let (code, text) = run(&[]);
        assert_eq!(code, 0, "bare invocation should succeed:\n{text}");
        assert!(text.contains("--help"), "should point at --help:\n{text}");

        // A typo'd mode must NOT look like a completed restore to a wrapper.
        let (code, text) = run(&["--resore"]);
        assert_eq!(code, 2, "an unknown mode must exit 2:\n{text}");
        assert!(text.contains("Unknown mode"), "should say so:\n{text}");

        // A trailing bare flag used to fail `shift 2` under `set -e` and exit
        // 1 having printed NOTHING. The message is the whole point.
        let (code, text) = run(&["--restore", "--key"]);
        assert_ne!(code, 0, "a flag with no value must fail:\n{text}");
        assert!(
            text.contains("--key needs a value"),
            "a short flag must name itself, not exit silently:\n{text:?}"
        );

        // --info is keyless and cannot see snapshot_version; --help must send
        // the heir to the command that can (#133 defect 3).
        let (_, help) = run(&["--help"]);
        assert!(
            help.contains("run --find-envelope --key KEYFILE"),
            "--help must point version discovery at --find-envelope:\n{help}"
        );

        // #288: --help must say --key repeats, and why. An heir who does not
        // know that a rotation splits envelope and slices across two key
        // generations has no reason to try passing a second key.
        assert!(
            help.contains("--key may be repeated"),
            "--help must document repeated --key:\n{help}"
        );
        assert!(
            help.contains("key rotation"),
            "--help must say WHY --key repeats:\n{help}"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Lay out a directory holding the generated script plus stub versions of
    /// every tool its prerequisite loop demands, and return the script path
    /// and a `PATH` that finds the stubs.
    ///
    /// `mt` and `dd` are not inert stubs: they touch `$TAPECTL_TEST_SENTINEL`.
    /// That is how a test can tell "refused before any tape read" from
    /// "refused after reading tape", which is the whole point of validating
    /// key paths up front.
    #[cfg(test)]
    fn stubbed_script_dir(tag: &str, label: &str) -> (std::path::PathBuf, String) {
        use std::os::unix::fs::PermissionsExt;

        let dir = std::env::temp_dir().join(format!("tapectl-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let sh = dir.join("RESTORE.sh");
        std::fs::write(&sh, generate_restore_script_v2(label, 20)).unwrap();

        let bin = dir.join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        // `mt-st` too: the script prefers it by name, and the host's real one
        // must never be reached from a test.
        for tool in [
            "mt",
            "mt-st",
            "dd",
            "age",
            "dar",
            "sha256sum",
            "head",
            "truncate",
            "tar",
        ] {
            let body = if tool == "mt" || tool == "mt-st" || tool == "dd" {
                "#!/bin/sh\nif [ -n \"${TAPECTL_TEST_SENTINEL:-}\" ]; then \
                 : >\"$TAPECTL_TEST_SENTINEL\"; fi\nexit 0\n"
            } else {
                "#!/bin/sh\nexit 0\n"
            };
            let stub = bin.join(tool);
            std::fs::write(&stub, body).unwrap();
            std::fs::set_permissions(&stub, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let path = format!(
            "{}:{}",
            bin.display(),
            std::env::var("PATH").unwrap_or_default()
        );
        (dir, path)
    }

    /// Issue #288, CTO ruling 2026-09-22: `--key` repeats, and every key path
    /// is validated BEFORE the first tape read.
    ///
    /// The discriminating cases are the ORDERING ones. Old single-key code
    /// accepted `--key a --key b` too — `--find-envelope` ignored the trailing
    /// pair and `--restore` simply overwrote — so "two keys are accepted"
    /// proves nothing on its own. What the old code could not do is notice a
    /// bad path that is not the last one: it checked exactly one file.
    ///
    /// `age -d -i good.key -i missing.key` fails outright rather than skipping
    /// the bad identity, so a mistyped path has to be caught here and not
    /// minutes into a restore — hence the sentinel, and hence the positive
    /// control that proves the sentinel can fire at all.
    #[test]
    fn restore_sh_takes_repeated_keys_and_names_a_missing_one_before_reading_tape() {
        use std::process::Command;

        let (dir, path) = stubbed_script_dir("multikey", "MULTI1");
        let sh = dir.join("RESTORE.sh");
        let sentinel = dir.join("tape-was-read");

        let good = dir.join("good.age.key");
        let good2 = dir.join("good2.age.key");
        std::fs::write(&good, "AGE-SECRET-KEY-1PLACEHOLDER\n").unwrap();
        std::fs::write(&good2, "AGE-SECRET-KEY-1PLACEHOLDER2\n").unwrap();
        let missing = dir.join("typo.age.key");

        let run = |args: &[&str]| -> (i32, String, bool) {
            let _ = std::fs::remove_file(&sentinel);
            let o = Command::new("bash")
                .arg(&sh)
                .args(args)
                .env("PATH", &path)
                .env("TAPECTL_TEST_SENTINEL", &sentinel)
                .output()
                .expect("spawn script");
            let mut text = String::from_utf8_lossy(&o.stdout).to_string();
            text.push_str(&String::from_utf8_lossy(&o.stderr));
            (o.status.code().unwrap_or(-1), text, sentinel.exists())
        };

        let missing_s = missing.display().to_string();
        let good_s = good.display().to_string();
        let good2_s = good2.display().to_string();

        // --find-envelope: the bad path is the SECOND one. Old code never
        // looked at it.
        let (code, text, read) = run(&["--find-envelope", "--key", &good_s, "--key", &missing_s]);
        assert_ne!(code, 0, "a missing key file must fail:\n{text}");
        assert!(
            text.contains(&format!("key file not found: {missing_s}")),
            "the missing key must be named:\n{text}"
        );
        assert!(
            !read,
            "no tape may be read before the key paths are checked"
        );

        // --restore: the bad path is the FIRST one. Old code checked only the
        // last `--key` it saw and would have proceeded.
        let dest = dir.join("dest");
        let dest_s = dest.display().to_string();
        let (code, text, read) = run(&[
            "--restore",
            "--key",
            &missing_s,
            "--key",
            &good_s,
            "--to",
            &dest_s,
        ]);
        assert_ne!(code, 0, "a missing key file must fail:\n{text}");
        assert!(
            text.contains(&format!("key file not found: {missing_s}")),
            "the missing key must be named:\n{text}"
        );
        assert!(
            !read,
            "no tape may be read before the key paths are checked"
        );

        // Positive control: with every key present, argument parsing accepts
        // the repetition and the script goes on to touch the tape. Without
        // this, "sentinel absent" could just mean the stub never runs.
        let (_, text, read) = run(&["--find-envelope", "--key", &good_s, "--key", &good2_s]);
        assert!(
            !text.contains("key file not found"),
            "two valid keys must both pass validation:\n{text}"
        );
        assert!(
            !text.contains("usage:"),
            "repeated --key must be accepted, not rejected as usage:\n{text}"
        );
        assert!(
            read,
            "control: with valid keys the script must reach the tape:\n{text}"
        );

        let (_, text, read) = run(&[
            "--restore",
            "--key",
            &good_s,
            "--key",
            &good2_s,
            "--to",
            &dest_s,
        ]);
        assert!(
            !text.contains("key file not found") && !text.contains("usage:"),
            "--restore must accept repeated --key:\n{text}"
        );
        assert!(
            read,
            "control: with valid keys --restore must reach the tape:\n{text}"
        );

        // Zero keys is still a usage error, in both modes.
        let (code, text, _) = run(&["--find-envelope"]);
        assert_ne!(code, 0, "no --key at all must fail:\n{text}");
        assert!(
            text.contains("usage: "),
            "no --key must print usage:\n{text}"
        );
        let (code, text, _) = run(&["--restore", "--to", &dest_s]);
        assert_ne!(code, 0, "no --key at all must fail:\n{text}");
        assert!(
            text.contains("usage: "),
            "no --key must print usage:\n{text}"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Issue #291 + the quoting defect: all three "no envelope opened" exits
    /// must carry the unit (where there is one), the device/label sentence and
    /// the key-rotation sentence — and must RENDER, which the template text
    /// alone cannot prove.
    ///
    /// The exits sit behind `establish_files`, which reads tape, so argv alone
    /// cannot reach them. Sourcing the script defines its functions without
    /// running a mode (with no positional args the trailing `case` takes the
    /// `""` branch, which does not exit), and `die_no_envelope` can then be
    /// called directly with the globals set the way a real failure would leave
    /// them.
    #[test]
    fn the_three_no_envelope_exits_render_unit_label_and_the_rotation_hint() {
        use std::process::Command;

        let (dir, path) = stubbed_script_dir("noenv", "GOLD01");
        let sh = dir.join("RESTORE.sh");

        let call = |snippet: &str| -> (i32, String) {
            let prog = format!(
                "source \"{}\"\nDEVICE=/dev/nst1\nTAPE_LABEL=VOL-A\nKEYS=(k1 k2 k3)\n{snippet}\n",
                sh.display()
            );
            let o = Command::new("bash")
                .arg("-c")
                .arg(&prog)
                .env("PATH", &path)
                .output()
                .expect("spawn bash -c");
            let mut text = String::from_utf8_lossy(&o.stdout).to_string();
            text.push_str(&String::from_utf8_lossy(&o.stderr));
            (o.status.code().unwrap_or(-1), text)
        };

        // The shape every one of the three must have.
        let shared = |text: &str, what: &str| {
            assert!(
                text.contains("Tape in /dev/nst1 identifies as 'VOL-A'"),
                "{what}: the device/label sentence must render with plain \
                 quotes:\n{text}"
            );
            assert!(
                text.contains("this script is for 'GOLD01'"),
                "{what}: the script's own label must render:\n{text}"
            );
            assert!(
                !text.contains("'\\''"),
                "{what}: the single-quote-inside-single-quotes idiom is \
                 literal inside a double-quoted string and must not appear \
                 in the output:\n{text}"
            );
            assert!(
                text.contains("wrong cartridge or the wrong drive"),
                "{what}: must say it may be the wrong cartridge:\n{text}"
            );
            assert!(
                text.contains("sealed with the key"),
                "{what}: must name the rotation cause (#288):\n{text}"
            );
            assert!(
                text.contains("Keys tried: k1 k2 k3"),
                "{what}: must name every key tried:\n{text}"
            );
        };

        // Exit 1 — do_find_envelope. Exit 2 — do_restore with no --unit.
        // Both are the no-unit form.
        let (code, text) = call("die_no_envelope");
        assert_ne!(code, 0, "die_no_envelope must exit non-zero:\n{text}");
        assert!(
            text.contains("no envelope matched any of the 3 keys provided"),
            "the headline must count the keys:\n{text}"
        );
        shared(&text, "no-unit form");

        // Exit 3 — do_restore WITH --unit. This is the one an heir actually
        // hits, and the one that used to be a bare one-liner (#291).
        let (code, text) = call("die_no_envelope photos");
        assert_ne!(code, 0, "die_no_envelope must exit non-zero:\n{text}");
        assert!(
            text.contains("no envelope for unit 'photos' matched any of the 3 keys provided"),
            "the --unit form must still name the unit:\n{text}"
        );
        shared(&text, "unit form");

        // A single key reads as a singular, not "any of the 1 keys".
        let prog = format!(
            "source \"{}\"\nDEVICE=/dev/nst1\nTAPE_LABEL=VOL-A\nKEYS=(only.key)\ndie_no_envelope\n",
            sh.display()
        );
        let o = Command::new("bash")
            .arg("-c")
            .arg(&prog)
            .env("PATH", &path)
            .output()
            .expect("spawn bash -c");
        let text = String::from_utf8_lossy(&o.stderr).to_string();
        assert!(
            text.contains("no envelope matched the key provided"),
            "one key must read as a singular:\n{text}"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Issue #288 at the slice, executed: every key reaches age at once (a
    /// streamed slice cannot be re-read for a second key, 1.1.0, #412), so a
    /// slice sealed to ANY of them opens; age's own stderr stays visible; and
    /// "no key works" still fails with the rotation hint.
    ///
    /// The real `age` CLI is not a test dependency here, so the stub decides
    /// by key NAME: what is under test is the script, not the cryptography.
    #[test]
    fn a_slice_is_decrypted_with_every_key_at_once() {
        use std::os::unix::fs::PermissionsExt;
        use std::process::Command;

        let (dir, path) = stubbed_script_dir("slicekeys", "SLICEK");
        let sh = dir.join("RESTORE.sh");

        // An `age` that succeeds when any -i names $AGE_GOOD, records every
        // call's keys on one line in $AGE_TRIED, and otherwise fails the way
        // the real one does.
        let age_stub = dir.join("bin").join("age");
        std::fs::write(
            &age_stub,
            "#!/bin/sh\nkeys=\"\"\nok=0\nwhile [ $# -gt 0 ]; do\n  case \"$1\" in\n  \
             -i) keys=\"$keys $2\"; [ \"$2\" = \"$AGE_GOOD\" ] && ok=1; shift 2 ;;\n  \
             *) shift ;;\n  esac\ndone\n\
             echo \"$keys\" >>\"$AGE_TRIED\"\nif [ $ok = 1 ]; then\n  cat\n  exit 0\nfi\n\
             echo \"age: error: no identity matched any of the recipients\" >&2\nexit 1\n",
        )
        .unwrap();
        std::fs::set_permissions(&age_stub, std::fs::Permissions::from_mode(0o755)).unwrap();

        let d = dir.display();
        let run = |prog: String| -> (i32, String) {
            let o = Command::new("bash")
                .arg("-c")
                .arg(&prog)
                .env("PATH", &path)
                .output()
                .expect("spawn bash -c");
            let mut text = String::from_utf8_lossy(&o.stdout).to_string();
            text.push_str(&String::from_utf8_lossy(&o.stderr));
            (o.status.code().unwrap_or(-1), text)
        };

        let (code, text) = run(format!(
            "source \"{sh}\"\nKEYS=(kA kB kC)\nset_age_ids\nexport AGE_GOOD=kB\n\
             export AGE_TRIED={d}/tried\n: >\"$AGE_TRIED\"\n\
             printf payload >{d}/in\n\
             decrypt_slice {d}/in {d}/out1 1\n\
             echo \"TRIED: $(cat \"$AGE_TRIED\")\"\n\
             echo \"OUT1: $(cat {d}/out1)\"\n",
            sh = sh.display()
        ));
        assert_eq!(code, 0, "a key that works must succeed:\n{text}");
        assert!(
            text.contains("TRIED:  kA kB kC\n"),
            "one age call, every key:\n{text}"
        );
        assert!(text.contains("OUT1: payload"), "{text}");

        let (code, text) = run(format!(
            "source \"{sh}\"\nKEYS=(kA kB kC)\nset_age_ids\nexport AGE_GOOD=none\n\
             export AGE_TRIED={d}/tried2\n: >\"$AGE_TRIED\"\n\
             printf payload >{d}/in\n\
             decrypt_slice {d}/in {d}/out3 7\n",
            sh = sh.display()
        ));
        assert_ne!(code, 0, "no working key must still fail:\n{text}");
        assert!(text.contains("cannot decrypt slice 7"), "{text}");
        assert!(text.contains("Keys tried: kA kB kC"), "{text}");
        assert!(text.contains("key rotation between the two"), "{text}");
        assert!(
            text.contains("no identity matched any of the recipients"),
            "age's own stderr must not be suppressed:\n{text}"
        );
        assert!(!dir.join("out3").exists(), "no partial plaintext is left");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The three exits must keep going through the one helper. A future edit
    /// that inlines a message again would pass the rendering test above (which
    /// calls the helper directly) while re-opening #291, so pin the call sites
    /// and pin the absence of the old single-key wording.
    #[test]
    fn every_no_envelope_exit_routes_through_the_shared_helper() {
        let s = generate_restore_script_v2("ROUTE1", 20);
        assert!(
            s.contains("die_no_envelope() {"),
            "the shared helper must exist"
        );
        // do_find_envelope's exit, and do_restore's (one call, covering both
        // its --unit and no---unit branches via the argument).
        assert!(
            s.contains("|| die_no_envelope\n"),
            "do_find_envelope must use the helper"
        );
        assert!(
            s.contains("die_no_envelope \"${wanted// /, }\""),
            "do_restore must use the helper and pass the unit(s)"
        );
        assert!(
            !s.contains("matched the provided key"),
            "the old single-key wording must be gone from every exit"
        );
        // The rendering defect must not survive anywhere in the script.
        assert!(
            !s.contains("'\\''"),
            "the '\\'' idiom is literal inside a double-quoted string"
        );
    }

    /// Issue #288's other half, as 1.1.0 streams slices (#412): every slice
    /// goes to age with EVERY key at once — the restore and --path alike —
    /// and a slice no key opens still names the keys and the rotation fact.
    #[test]
    fn every_slice_is_offered_every_key() {
        let s = generate_restore_script_v2("SLICE1", 20);
        assert!(
            s.contains("AGE_IDS+=(-i \"$k\")"),
            "every key becomes an -i"
        );
        assert!(
            s.contains("age -d \"${AGE_IDS[@]}\" 2>\"$WORK/age.err\" >\"$fifos/restore.$num.dar\""),
            "the streamed slice is decrypted with every key"
        );
        assert!(
            s.contains("if age -d \"${AGE_IDS[@]}\" <\"$in\" >\"$out\""),
            "--path's on-disk slices too"
        );
        assert!(!s.contains("SLICE_KEY"), "no per-key retry is left");
        assert!(s.contains("key rotation between the two puts them on"));
        assert!(s.contains("Keys tried: $(keys_list)"));
        // #218 (ADR-0012): no binary figure is labelled decimal.
        assert!(!s.contains("$((bytes / 1048576)) MB"));
        assert!(
            s.contains("--restore $(keys_args) --to /your/destination"),
            "the restore hint must echo all the keys"
        );
    }

    /// Find `name` on the test process's own PATH, for building a PATH that
    /// holds exactly the tools a test chooses and nothing else.
    fn host_tool(name: &str) -> std::path::PathBuf {
        std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default())
            .map(|d| d.join(name))
            .find(|p| p.is_file())
            .unwrap_or_else(|| panic!("test host has no `{name}` on PATH"))
    }

    /// Issue #349, CTO ruling 2026-09-28: RESTORE.sh unpacks the decrypted
    /// envelope with `tar xf -`, so an heir without `tar` must be told so up
    /// front by the prerequisite check, not meet a confusing failure after
    /// the tape has already been read and decrypted. Since 1.2.0 the check
    /// belongs to each mode (#449 item 2): `--restore` runs tar, and a bare
    /// invocation, `--help` and a typo'd mode run nothing and need nothing.
    ///
    /// Executed, not grepped: the script runs on a PATH holding every other
    /// tool the check demands (inert stubs, plus the real `mktemp`/`rm` the
    /// preamble uses) and no `tar`. The positive control adds a `tar` stub to
    /// the same PATH and must get past the check — without it, a failure
    /// here could just mean the stripped PATH broke something else.
    #[test]
    fn restore_sh_refuses_up_front_when_tar_is_missing() {
        use std::os::unix::fs::PermissionsExt;
        use std::process::Command;

        let dir = std::env::temp_dir().join(format!("tapectl-notar-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let bin = dir.join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        let sh = dir.join("RESTORE.sh");
        std::fs::write(&sh, generate_restore_script_v2("NOTAR1", 20)).unwrap();
        for tool in ["mt"].into_iter().chain(restore_sh_tools()) {
            if tool == "tar" {
                continue;
            }
            let stub = bin.join(tool);
            std::fs::write(&stub, "#!/bin/sh\nexit 0\n").unwrap();
            std::fs::set_permissions(&stub, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        for tool in ["mktemp", "rm"] {
            std::os::unix::fs::symlink(host_tool(tool), bin.join(tool)).unwrap();
        }
        let bash = host_tool("bash");
        let run = |args: &[&str]| -> (i32, String) {
            let o = Command::new(&bash)
                .arg(&sh)
                .args(args)
                .env("PATH", &bin)
                .output()
                .expect("spawn script");
            let mut text = String::from_utf8_lossy(&o.stdout).to_string();
            text.push_str(&String::from_utf8_lossy(&o.stderr));
            (o.status.code().unwrap_or(-1), text)
        };
        let restore = ["--restore", "--key", "k.key", "--to", "out"];

        let (code, text) = run(&restore);
        assert_eq!(code, 1, "no tar on PATH must be a fatal exit:\n{text}");
        assert!(
            text.contains("missing required tool: tar"),
            "the prerequisite check must name tar:\n{text}"
        );

        // A mode that runs no tar does not ask for it: --info and --verify
        // read and hash, and get as far as the (stub) tape.
        for mode in ["--info", "--verify"] {
            let (_, text) = run(&[mode]);
            assert!(
                !text.contains("missing required tool"),
                "{mode} runs no tar:\n{text}"
            );
        }
        let (code, text) = run(&[]);
        assert_eq!(code, 0, "a bare invocation needs no tool:\n{text}");
        let (code, _) = run(&["--help"]);
        assert_eq!(code, 0, "--help needs no tool");

        // Positive control: the same PATH plus tar gets past the check (the
        // key file does not exist, which is the next thing the mode asks).
        let stub = bin.join("tar");
        std::fs::write(&stub, "#!/bin/sh\nexit 0\n").unwrap();
        std::fs::set_permissions(&stub, std::fs::Permissions::from_mode(0o755)).unwrap();
        let (code, text) = run(&restore);
        assert_eq!(
            code, 1,
            "control: it then stops at the missing key:\n{text}"
        );
        assert!(!text.contains("missing required tool"), "{text}");
        assert!(text.contains("key file not found"), "{text}");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// #449 item 2, the one source of truth: RESTORE.sh's per-mode sets are
    /// read back out of the generated script and compared to the table they
    /// come from. The union is what `--restore` checks, what File 1 and the
    /// cover sheet list, and what the Requirements lines say; every smaller
    /// set is drawn from it; and the sets widen the way the modes do.
    #[test]
    fn restore_sh_tool_sets_are_drawn_from_one_table() {
        let s = generate_restore_script_v2("TOOLSET", 20);
        assert!(!s.contains("__TOOLS_"), "a placeholder survived assembly");
        let set = |name: &str| -> Vec<String> {
            let line = s
                .lines()
                .find(|l| l.starts_with(&format!("{name}=(")))
                .unwrap_or_else(|| panic!("no {name} line"));
            line.trim_start_matches(&format!("{name}=("))
                .split(')')
                .next()
                .unwrap()
                .split_whitespace()
                .map(str::to_string)
                .collect()
        };
        let union: Vec<String> = restore_sh_tools().iter().map(|t| t.to_string()).collect();
        assert_eq!(
            union,
            [
                "dd",
                "age",
                "dar",
                "sha256sum",
                "head",
                "truncate",
                "tar",
                "mkfifo",
                "tee"
            ],
            "the union, in the order RESTORE.sh's Requirements lines have always listed it"
        );
        assert_eq!(set("TOOLS_RESTORE"), union, "--restore checks the union");
        let (read, envelope, list, path) = (
            set("TOOLS_READ"),
            set("TOOLS_ENVELOPE"),
            set("TOOLS_LIST"),
            set("TOOLS_PATH"),
        );
        for (name, sub) in [
            ("read", &read),
            ("envelope", &envelope),
            ("list", &list),
            ("path", &path),
        ] {
            for tool in sub {
                assert!(union.contains(tool), "{name}: {tool} is not in the union");
            }
        }
        // The widening: --info/--verify read and hash; --find-envelope adds
        // what unpacks an envelope; --list adds dar; --path adds tee;
        // --restore adds mkfifo.
        assert_eq!(read, ["dd", "sha256sum", "head"]);
        assert_eq!(
            envelope,
            ["dd", "age", "sha256sum", "head", "truncate", "tar"]
        );
        assert_eq!(
            list,
            ["dd", "age", "dar", "sha256sum", "head", "truncate", "tar"]
        );
        assert!(path.contains(&"tee".to_string()) && !path.contains(&"mkfifo".to_string()));
        assert!(path.iter().all(|t| list.contains(t) || t == "tee"));
        // --where and --find (1.3.0) open envelopes and read plain text out of
        // them: the --find-envelope set exactly, no dar, tee or mkfifo.
        assert_eq!(set("TOOLS_INDEX"), envelope, "the index modes' tools");
        // The two Requirements lines say the union, mt first.
        let req = format!("mt (mt-st), {}", union.join(", "));
        assert!(s.contains(&format!("# Requirements: {req}\n")));
        assert!(s.contains(&format!("echo \"Requirements: {req}\"")));
        // The prerequisite loop is gone: no mode checks tools it does not run.
        assert!(!s.contains("for tool in dd age"), "the every-mode loop");
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

    /// A directory holding the generated script and a `bin/` of stubs that
    /// shadow the host's tools, one `(name, body)` per stub. Everything not
    /// stubbed (sha256sum, head, df, stat, awk, mktemp, ...) is the host's
    /// real tool, which these tests need: they execute the script's own
    /// helpers rather than grep for them.
    #[cfg(test)]
    fn heir_harness(tag: &str, stubs: &[(&str, &str)]) -> (std::path::PathBuf, String) {
        use std::os::unix::fs::PermissionsExt;

        let dir = std::env::temp_dir().join(format!("tapectl-heir-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let bin = dir.join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        std::fs::write(
            dir.join("RESTORE.sh"),
            generate_restore_script_v2("HEIR01", 20),
        )
        .unwrap();
        // The prerequisite loop demands these; inert unless a test says more.
        // `mt-st` too: the script prefers it by name, and the host's real one
        // must never be reached from a test.
        let mut all: Vec<(&str, &str)> = vec![
            ("mt", "#!/bin/sh\nexit 0\n"),
            ("mt-st", "#!/bin/sh\nexit 0\n"),
            ("dd", "#!/bin/sh\nexit 0\n"),
            ("age", "#!/bin/sh\nexit 0\n"),
            ("dar", "#!/bin/sh\nexit 0\n"),
        ];
        all.retain(|(n, _)| !stubs.iter().any(|(m, _)| m == n));
        all.extend_from_slice(stubs);
        for (name, body) in all {
            let stub = bin.join(name);
            std::fs::write(&stub, body).unwrap();
            std::fs::set_permissions(&stub, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let path = format!(
            "{}:{}",
            bin.display(),
            std::env::var("PATH").unwrap_or_default()
        );
        (dir, path)
    }

    /// Source the harness's script (defining its functions without running a
    /// mode) and run `snippet` in the same shell.
    #[cfg(test)]
    fn heir_run(dir: &std::path::Path, path: &str, snippet: &str) -> (i32, String) {
        let prog = format!(
            "source \"{}\" >/dev/null\n{snippet}\n",
            dir.join("RESTORE.sh").display()
        );
        let o = std::process::Command::new("bash")
            .arg("-c")
            .arg(&prog)
            .env("PATH", path)
            .output()
            .expect("spawn bash -c");
        let mut text = String::from_utf8_lossy(&o.stdout).to_string();
        text.push_str(&String::from_utf8_lossy(&o.stderr));
        (o.status.code().unwrap_or(-1), text)
    }

    /// A `df -Pk` stub reporting `kib` KiB available on every path.
    #[cfg(test)]
    fn df_stub(kib: u64) -> String {
        format!(
            "#!/bin/sh\necho 'Filesystem 1024-blocks Used Available Capacity Mounted on'\n\
             echo \"/dev/x 999999999999 0 {kib} 0% /\"\n"
        )
    }

    /// The space check refuses a restore that cannot fit, names the fix, and
    /// says no slice has been read; with room it passes and says how much it
    /// needs. 1.1.0 streams slices into dar (#412), so a restore needs room
    /// for the files only — the unit's size, no longer about twice it plus a
    /// slice, which refused a 470 GiB unit on a 1 TB disk.
    #[test]
    fn restore_space_check_refuses_a_unit_that_cannot_fit() {
        let gib: u64 = 1 << 30;
        let total = 20 * gib;

        // 8 GiB free: refused.
        let small = df_stub(8 * (1 << 20));
        let (dir, path) = heir_harness("space-small", &[("df", &small)]);
        let dest = dir.join("dest");
        std::fs::create_dir_all(&dest).unwrap();
        let call = format!("check_space \"{0}\" \"{0}\" {total} 0", dest.display());
        let (code, text) = heir_run(&dir, &path, &call);
        assert_ne!(code, 0, "a unit that cannot fit must be refused:\n{text}");
        assert!(text.contains("not enough disk space in"), "{text}");
        assert!(text.contains("No slice has been read yet"), "{text}");
        assert!(
            text.contains("--no-space-check"),
            "the escape must be named:\n{text}"
        );
        assert!(
            text.contains("it needs about 20.0 GiB, and 8.0 GiB is free"),
            "the files only:\n{text}"
        );

        // Positive control: 25 GiB free holds a 20 GiB unit (1.0.x asked 50).
        let big = df_stub(25 * (1 << 20));
        let (dir2, path2) = heir_harness("space-big", &[("df", &big)]);
        let dest2 = dir2.join("dest");
        std::fs::create_dir_all(&dest2).unwrap();
        let call = format!(
            "check_space \"{0}\" \"{0}\" {total} 0 && echo PASSED",
            dest2.display()
        );
        let (code, text) = heir_run(&dir2, &path2, &call);
        assert_eq!(code, 0, "{text}");
        assert!(text.contains("PASSED"), "{text}");
        assert!(
            text.contains("Disk space: needs about 20.0 GiB in") && text.contains("(the files)"),
            "{text}"
        );

        // --no-space-check skips it, even when it would refuse.
        let call = format!(
            "SKIP_SPACE_CHECK=1\ncheck_space \"{0}\" \"{0}\" {total} 0 && echo PASSED",
            dest.display()
        );
        let (code, text) = heir_run(&dir, &path, &call);
        assert_eq!(code, 0, "{text}");
        assert!(
            text.contains("not checked (--no-space-check)") && text.contains("PASSED"),
            "{text}"
        );

        // A df that cannot answer warns and carries on: the check must never be
        // what stops an heir.
        let (dir3, path3) = heir_harness("space-nodf", &[("df", "#!/bin/sh\nexit 1\n")]);
        let call = format!("check_space /x /x {total} 0 && echo PASSED");
        let (code, text) = heir_run(&dir3, &path3, &call);
        assert_eq!(code, 0, "{text}");
        assert!(
            text.contains("cannot measure free disk space") && text.contains("PASSED"),
            "{text}"
        );

        for d in [dir, dir2, dir3] {
            let _ = std::fs::remove_dir_all(&d);
        }
    }

    /// Sizes in the space messages pick their unit: a small unit must not read
    /// as "needs about 0.0 GiB, and 0.0 GiB is free" (seen on a real tape).
    #[test]
    fn restore_space_messages_name_small_sizes_in_small_units() {
        let (dir, path) = heir_harness("sizes", &[]);
        let (code, text) = heir_run(
            &dir,
            &path,
            "size_str 5000; echo; size_str 3145728; echo; size_str 53687091200",
        );
        assert_eq!(code, 0, "{text}");
        assert_eq!(text, "4.9 KiB\n3.0 MiB\n50.0 GiB", "{text}");

        // And in the refusal itself: 64 KiB free against a 3 MiB unit.
        let df = df_stub(64);
        let (dir2, path2) = heir_harness("sizes-refuse", &[("df", &df)]);
        let (code, text) = heir_run(&dir2, &path2, "check_space /d /d 3145728 0");
        assert_ne!(code, 0, "{text}");
        assert!(
            text.contains("it needs about 3.0 MiB, and 64.0 KiB is free"),
            "{text}"
        );
        for d in [dir, dir2] {
            let _ = std::fs::remove_dir_all(&d);
        }
    }

    /// --path keeps the slices it reads on disk (scratch). With --scratch on
    /// another filesystem each disk is asked only for its own share: 21 GiB
    /// free on each passes 20 GiB of slices and 20 GiB of files that one
    /// 21 GiB disk holding both could not.
    #[test]
    fn restore_space_check_splits_the_need_across_two_filesystems() {
        let gib: u64 = 1 << 30;
        let df = df_stub(21 * (1 << 20));
        // stat reports a different device per directory name.
        let stat = "#!/bin/sh\ncase \"$*\" in *scr*) echo 11 ;; *) echo 22 ;; esac\n";
        let (dir, path) = heir_harness("space-split", &[("df", &df), ("stat", stat)]);
        let call = format!(
            "check_space /d/dest /d/scr {} {} && echo PASSED",
            20 * gib,
            20 * gib
        );
        let (code, text) = heir_run(&dir, &path, &call);
        assert_eq!(code, 0, "{text}");
        assert!(text.contains("PASSED"), "{text}");
        assert!(text.contains("in /d/scr (slices read)"), "{text}");
        assert!(text.contains("in /d/dest (the files)"), "{text}");

        // Control: the same disks as ONE filesystem must refuse.
        let same = "#!/bin/sh\necho 11\n";
        let (dir2, path2) = heir_harness("space-same", &[("df", &df), ("stat", same)]);
        let (code, text) = heir_run(&dir2, &path2, &call);
        assert_ne!(code, 0, "one 21 GiB disk cannot hold both:\n{text}");
        for d in [dir, dir2] {
            let _ = std::fs::remove_dir_all(&d);
        }
    }

    /// The check runs after the versions are picked and BEFORE the first slice
    /// is read; a full restore streams its slices from the tape into dar
    /// through FIFOs, and no slice lands in WORK (which is /tmp).
    #[test]
    fn restore_measures_space_before_the_first_slice_and_streams_into_dar() {
        let s = generate_restore_script_v2("ORDER1", 20);
        let check = s
            .find("check_space \"$destdir\" \"$destdir\" \"$total\" 0")
            .expect("do_restore calls check_space");
        let picked = s
            .find("snapshot version ${PICKED_VERSION:-unknown}")
            .unwrap();
        let first_read = s
            .find("restore_unit \"$WORK/slices.$i\"")
            .expect("slice read");
        // dar is given the envelope's catalogue when it streams: dar 2.7.13
        // loses a file's tail streaming without one.
        assert!(s.contains("[ -z \"$cat\" ] || dar_opts+=(-A \"$cat\")"));
        assert!(
            picked < check && check < first_read,
            "order: version, space, slices"
        );
        assert!(s.contains("mkfifo \"$fifos/restore.$num.dar\""));
        assert!(s.contains("local -a dar_opts=(-O -Q -N --sequential-read)"));
        assert!(!s.contains("$WORK/slice.enc") && !s.contains("$WORK/dar/"));
        // --verify hashes as it streams; it copies nothing to WORK.
        assert!(
            !s.contains("$WORK/chk."),
            "--verify must not copy tape files to WORK"
        );
        // The trap kills a dar left waiting on a FIFO, and removes scratch.
        assert!(s.contains("trap cleanup EXIT"));
        assert!(s.contains("kill -KILL \"$p\""));
        assert!(s.contains("[ -z \"$SCRATCH\" ] || rm -rf \"$SCRATCH\""));
    }

    /// A full disk while age writes a slice is reported as a full disk, on
    /// the first key, instead of every key failing in turn and the script
    /// blaming a key rotation.
    #[test]
    fn a_full_disk_while_decrypting_is_not_reported_as_a_wrong_key() {
        let age_full = "#!/bin/sh\necho 'age: error: failed to write output: write /x: no space left on device' >&2\nexit 1\n";
        let (dir, path) = heir_harness("age-full", &[("age", age_full)]);
        let snippet = format!(
            "KEYS=(k1 k2)\nset_age_ids\n: >\"{0}/in\"\ndecrypt_slice \"{0}/in\" \"{0}/out\" 3",
            dir.display()
        );
        let (code, text) = heir_run(&dir, &path, &snippet);
        assert_ne!(code, 0, "{text}");
        assert!(
            text.contains("decrypting slice 3 failed: OUT OF DISK SPACE"),
            "{text}"
        );
        assert!(text.contains("This is not a tape or key problem"), "{text}");
        assert!(
            !text.contains("none of the"),
            "must not blame the keys:\n{text}"
        );
        assert!(
            !text.contains("key k2"),
            "a full disk must stop at the first key:\n{text}"
        );
        assert!(
            !dir.join("out").exists(),
            "the partial plaintext is removed"
        );

        // Control: a genuinely wrong key still walks every key and ends on the
        // rotation hint.
        let age_wrong =
            "#!/bin/sh\necho 'age: error: no identity matched any of the recipients' >&2\nexit 1\n";
        let (dir2, path2) = heir_harness("age-wrong", &[("age", age_wrong)]);
        let snippet = format!(
            "KEYS=(k1 k2)\nset_age_ids\n: >\"{0}/in\"\ndecrypt_slice \"{0}/in\" \"{0}/out\" 3",
            dir2.display()
        );
        let (code, text) = heir_run(&dir2, &path2, &snippet);
        assert_ne!(code, 0, "{text}");
        assert!(text.contains("Keys tried: k1 k2"), "{text}");
        assert!(text.contains("none of the 2 key(s) decrypted it"), "{text}");
        assert!(
            text.contains("no identity matched"),
            "age's own stderr stays visible:\n{text}"
        );
        for d in [dir, dir2] {
            let _ = std::fs::remove_dir_all(&d);
        }
    }

    /// A full disk while dd copies a slice off the tape names the cause; it
    /// used to end the script with no message at all (dd's stderr went to
    /// /dev/null under `set -e`).
    #[test]
    fn a_full_disk_while_reading_a_slice_names_the_cause() {
        let dd_full = "#!/bin/sh\necho \"dd: error writing 'x': No space left on device\" >&2\n\
                       echo '3+0 records in' >&2\necho '2+0 records out' >&2\nexit 1\n";
        let (dir, path) = heir_harness("dd-full", &[("dd", dd_full)]);
        let snippet = format!("read_tape_raw 7 \"{}/slice.enc\"", dir.display());
        let (code, text) = heir_run(&dir, &path, &snippet);
        assert_ne!(code, 0, "{text}");
        assert!(
            text.contains("reading tape file 7 failed: OUT OF DISK SPACE in"),
            "{text}"
        );
        assert!(
            text.contains("No space left on device"),
            "dd's own line is kept:\n{text}"
        );
        assert!(
            !text.contains("records in"),
            "dd's record counts are noise:\n{text}"
        );

        // Control: a dd that succeeds returns quietly.
        let (dir2, path2) = heir_harness("dd-ok", &[]);
        let snippet = format!(
            "read_tape_raw 7 \"{}/slice.enc\" && echo READ",
            dir2.display()
        );
        let (code, text) = heir_run(&dir2, &path2, &snippet);
        assert_eq!(code, 0, "{text}");
        assert!(text.contains("READ"), "{text}");
        for d in [dir, dir2] {
            let _ = std::fs::remove_dir_all(&d);
        }
    }

    /// --verify's hash of a tape file: exactly the first `size` bytes, taken
    /// as the file streams off the tape, with the block padding behind it
    /// drained so dd never dies of SIGPIPE (which pipefail would report as an
    /// unreadable file). The padding here is far larger than a pipe buffer, so
    /// a missing drain fails this test.
    #[test]
    fn verify_hashes_a_tape_file_as_it_streams() {
        use sha2::{Digest, Sha256};

        let dir0 = std::env::temp_dir().join(format!("tapectl-heir-tape-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir0);
        std::fs::create_dir_all(&dir0).unwrap();
        let payload: Vec<u8> = (0..300_000u32).map(|i| (i % 251) as u8).collect();
        let mut on_tape = payload.clone();
        on_tape.resize(payload.len() + 4 * 1024 * 1024, 0); // block padding
        let tape = dir0.join("tapefile");
        std::fs::write(&tape, &on_tape).unwrap();
        let want = format!("{:x}", Sha256::digest(&payload));
        let want_padded = format!("{:x}", Sha256::digest(&on_tape));

        let dd = format!("#!/bin/sh\ncat \"{}\"\n", tape.display());
        let (dir, path) = heir_harness("stream", &[("dd", &dd)]);
        let (code, text) = heir_run(
            &dir,
            &path,
            &format!("hash_tape_file 5 {} && echo \"$TAPE_SHA\"", payload.len()),
        );
        assert_eq!(code, 0, "{text}");
        assert!(
            text.contains(&want),
            "the hash of exactly size_bytes:\n{text}"
        );
        // No size: the whole file, padding included (as before).
        let (code, text) = heir_run(&dir, &path, "hash_tape_file 5 '' && echo \"$TAPE_SHA\"");
        assert_eq!(code, 0, "{text}");
        assert!(text.contains(&want_padded), "{text}");

        // Nothing off the tape, or a dd that fails, is unreadable (return 1).
        let (dir2, path2) = heir_harness("stream-empty", &[("dd", "#!/bin/sh\nexit 0\n")]);
        let (code, text) = heir_run(&dir2, &path2, "hash_tape_file 5 100 || echo UNREADABLE");
        assert_eq!(code, 0, "{text}");
        assert!(text.contains("UNREADABLE"), "{text}");
        let (dir3, path3) = heir_harness("stream-fail", &[("dd", "#!/bin/sh\nexit 1\n")]);
        let (code, text) = heir_run(&dir3, &path3, "hash_tape_file 5 100 || echo UNREADABLE");
        assert_eq!(code, 0, "{text}");
        assert!(text.contains("UNREADABLE"), "{text}");
        for d in [dir0, dir, dir2, dir3] {
            let _ = std::fs::remove_dir_all(&d);
        }
    }

    /// The script reads layout v2 only: another layout is refused, before
    /// anything of it is interpreted, with the way to the tape's own script; a
    /// missing value (a damaged thunk) warns and carries on; a crafted value is
    /// sanitised before it is echoed.
    #[test]
    fn restore_sh_refuses_another_layout_and_warns_on_a_missing_one() {
        let (dir, path) = heir_harness("layout", &[]);
        let (code, text) = heir_run(&dir, &path, "check_layout_version 3");
        assert_ne!(code, 0, "{text}");
        assert!(
            text.contains("this tape is layout_version 3; this script reads layout v2 only"),
            "{text}"
        );
        assert!(
            text.contains("fsf 2") && text.contains("tr -d '\\0' > RESTORE.sh"),
            "{text}"
        );

        let (code, text) = heir_run(&dir, &path, "check_layout_version '' && echo GO");
        assert_eq!(code, 0, "{text}");
        assert!(
            text.contains("states no layout_version") && text.contains("GO"),
            "{text}"
        );

        let (code, text) = heir_run(&dir, &path, "check_layout_version 2 && echo GO");
        assert_eq!(code, 0, "{text}");
        assert_eq!(text.trim(), "GO", "a v2 tape reads silently:\n{text}");

        let (code, text) = heir_run(&dir, &path, "check_layout_version $'3\\e[2J'");
        assert_ne!(code, 0, "{text}");
        assert!(
            text.contains("(sanitized)") && !text.contains('\u{1b}'),
            "{text}"
        );

        // bootstrap_thunk runs it before the pointers it would misread.
        let s = generate_restore_script_v2("ORDER2", 20);
        let guard = s.find("check_layout_version \"$(toml_val").unwrap();
        let pointers = s.find("FRONT_INDEX=$(toml_val").unwrap();
        assert!(guard < pointers);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// --restore accepts --all, repeated --unit, --overwrite and
    /// --no-space-check; --scratch belongs to --path; the help and header say
    /// what the space is for.
    #[test]
    fn restore_sh_documents_its_options_and_the_space_it_needs() {
        let s = generate_restore_script_v2("DOC01", 20);
        assert!(s.contains("--unit) RESTORE_UNITS+=(\"$2\") ;;"));
        assert!(s.contains("--all)\n      RESTORE_ALL=1"));
        assert!(s.contains("--overwrite)\n      OVERWRITE=1"));
        assert!(s.contains("--no-space-check)\n      SKIP_SPACE_CHECK=1"));
        assert!(s.contains("do_restore \"$dest\" \"$want\""));
        let req = "mt (mt-st), dd, age, dar, sha256sum, head, truncate, tar, mkfifo, tee";
        assert!(s.contains(&format!("# Requirements: {req}\n")));
        assert!(s.contains(&format!("echo \"Requirements: {req}\"")));
        assert!(s.contains("# Disk space: --restore streams each slice from the tape"));
        let guide = generate_system_guide_v2("DOC01", 20);
        assert!(
            guide.contains("not in /tmp"),
            "the manual steps warn off /tmp"
        );
    }

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

        let (dir, path) = heir_harness("zerostrip", &[]);
        let (_, script_text) = heir_run(&dir, &path, "zero_strip_instructions");
        let guide = generate_system_guide_v2("ZS01", 20);
        for (source, text) in [
            ("RESTORE.sh", script_text.as_str()),
            ("guide", guide.as_str()),
        ] {
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
        let s = generate_restore_script_v2("DAR01", 20);
        assert!(
            s.contains("in an empty directory: dar -x restore"),
            "rung-3 text"
        );
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
            number: n,
            tape_position: pos,
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
            dar_version: None,
            dar_command: None,
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

    /// #449 comment 1 (ADR-0012 2026-10-16 item 71): RESTORE.sh greps dar's
    /// English output (`not restored (user choice)$`, `do not correspond to
    /// the same data`, `INITIALIZATION FAILED FOR GPGME`, `no space left`)
    /// and dar ships fr and sv translations, so the script asks for English
    /// in any locale. The export comes right after `set -euo pipefail`, ahead
    /// of every function, variable and tool call that follows.
    #[test]
    fn restore_sh_exports_lc_all_c_before_it_runs_any_tool() {
        let s = generate_restore_script_v2("LOCALE1", 20);
        assert_eq!(s.matches("\nexport LC_ALL=C\n").count(), 1, "exactly once");
        let export = s.find("\nexport LC_ALL=C\n").unwrap();
        let strict = s.find("\nset -euo pipefail\n").unwrap();
        assert!(strict < export, "after the strict-mode line");
        for first in [
            "\nDEVICE=",
            "\nWORK=\"$(mktemp",
            "\ndie() {",
            "\ndar_check() {",
        ] {
            let at = s.find(first).unwrap_or_else(|| panic!("{first:?} missing"));
            assert!(export < at, "LC_ALL is exported before {first:?}");
        }
        // The words it greps are still the English ones.
        for word in [
            "not restored (user choice)$",
            "do not correspond to the same data",
            "INITIALIZATION FAILED FOR GPGME",
            "no space left",
        ] {
            assert!(s.contains(word), "{word:?}");
        }
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
            dar_version: None,
            dar_command: None,
            slices: vec![ManifestSlice {
                number: 1,
                tape_position: 9,
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
    /// Every awk an heir might have reads the index programs the same way
    /// (1.3.0): the decoder and the visible form use no gawk extension, so the
    /// output of `--where` and `--find` is byte-for-byte the same under every
    /// awk this host has. A positive control: at least one awk ran, and its
    /// output holds what the escapes must decode to.
    #[test]
    fn the_index_programs_read_the_same_under_every_awk_on_the_host() {
        use std::process::Command;

        let dir = tempfile::tempdir().unwrap();
        // Columns reversed with an unknown one, so lookup is by name; a unit
        // name and a path with the four escapes.
        let units = "sealed_at\tstate\tzz_future\tgeneration\tcartridge_serial\tvolume_uuid\tvolume\tversion_bytes\tlast_slice\tfirst_slice\tnum_slices\tversion\tunit_uuid\tunit\n\
                     \tthis-tape\tx\tLTO-6\tSER1\tuuid-1\tVOL1\t4096\t3\t1\t3\t2\tu-1\tphotos\\ttab\n\
                     2026-09-01T10:00:00Z\tquarantined\tx\tLTO-5\t\tuuid-2\tVOL2\t4096\t3\t1\t3\t2\tu-1\tphotos\\ttab\n\
                     \tthis-tape\tx\tLTO-6\tSER1\tuuid-1\tVOL1\t1048576\t1\t1\t1\t1\tu-2\tplain\n";
        let paths = "unit\tversion\tkind\tsize_bytes\tpath\n\
                     photos\\ttab\t2\tf\t5\ta\\tb\\nc\\\\d\\\\te.txt\n\
                     photos\\ttab\t2\td\t0\tdir\n\
                     plain\t1\tf\t7\tnote.txt\n";
        let uf = dir.path().join("units.tsv");
        let pf = dir.path().join("paths.tsv");
        std::fs::write(&uf, units).unwrap();
        std::fs::write(&pf, paths).unwrap();
        let where_prog = format!("{AWK_INDEX_COMMON} {AWK_INDEX_WHERE}");
        let find_prog = format!("{AWK_INDEX_COMMON} {AWK_INDEX_FIND}");

        let awks: &[(&str, &[&str])] = &[
            ("awk", &[]),
            ("gawk", &[]),
            ("mawk", &[]),
            ("nawk", &[]),
            ("original-awk", &[]),
            ("busybox", &["awk"]),
        ];
        let mut outputs: Vec<(String, String)> = Vec::new();
        for (bin, pre) in awks {
            let run =
                |prog: &str, file: &std::path::Path, env: &[(&str, &str)]| -> Option<String> {
                    let mut cmd = Command::new(bin);
                    cmd.args(*pre).arg(prog).arg(file).env("LC_ALL", "C");
                    for (k, v) in env {
                        cmd.env(k, v);
                    }
                    let o = cmd.output().ok()?;
                    Some(format!(
                        "exit {:?}\n{}{}",
                        o.status.code(),
                        String::from_utf8_lossy(&o.stdout),
                        String::from_utf8_lossy(&o.stderr)
                    ))
                };
            let known = dir.path().join(format!("known-{bin}"));
            let counts = dir.path().join(format!("counts-{bin}"));
            // The escaped unit name, given as the DECODED text (a real TAB).
            let Some(w) = run(
                &where_prog,
                &uf,
                &[
                    ("WHERE_UNIT", "photos\ttab"),
                    ("WHERE_KNOWN", known.to_str().unwrap()),
                    ("WHERE_TENANT", "alice"),
                ],
            ) else {
                continue;
            };
            let f = run(
                &find_prog,
                &pf,
                &[
                    // a real TAB, then a letter, then a real LF: the decoded path
                    ("FIND_TEXT", "a\tb\nc\\d"),
                    ("FIND_LIMIT", "50"),
                    ("FIND_UNITS", uf.to_str().unwrap()),
                    ("FIND_COUNTS", counts.to_str().unwrap()),
                    ("FIND_WHO", "tenant alice"),
                ],
            )
            .unwrap();
            let known_txt = std::fs::read_to_string(&known).unwrap_or_default();
            let counts_txt = std::fs::read_to_string(&counts).unwrap_or_default();
            outputs.push((
                bin.to_string(),
                format!("{w}--\n{f}--\n{known_txt}--\n{counts_txt}"),
            ));
        }
        assert!(!outputs.is_empty(), "no awk found on this host");
        eprintln!(
            "index programs compared under: {:?}",
            outputs.iter().map(|(n, _)| n.as_str()).collect::<Vec<_>>()
        );
        let (first_name, first) = &outputs[0];
        // What the escapes must decode to, once, under the first awk.
        assert!(
            first.contains("Unit photos\\ttab (tenant alice):"),
            "{first}"
        );
        assert!(
            first.contains("  Version 2: 3 slice(s), 4.0 KiB (4096 bytes); unit uuid u-1"),
            "{first}"
        );
        assert!(
            first.contains("    tape VOL1  (this tape)  slices 1-3 of 3"),
            "{first}"
        );
        assert!(first.contains("QUARANTINED"), "{first}");
        assert!(
            first.contains("cartridge serial not known; generation LTO-5"),
            "{first}"
        );
        assert!(
            first.contains("  photos\\ttab  v2  f  5 bytes  a\\tb\\nc\\d\\te.txt\n"),
            "{first}"
        );
        assert!(
            first.contains("1 1\n"),
            "one match printed, one matched: {first}"
        );
        for (name, out) in &outputs[1..] {
            assert_eq!(out, first, "{name} differs from {first_name}");
        }
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
        let plain = ["RESTORE.sh", "file0.txt", "front_index.toml", "seal.toml"];
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
        let fetch = auto.find("> RESTORE.sh").expect("the fetch of RESTORE.sh");
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
            total
                .contains("The files are numbered 0 to 26. File 26, the last, is the seal marker."),
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
        assert!(f.contains("The count matches the catalogue"), "{f}");
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
            "As a rule do not use a USB or Thunderbolt enclosure or bridge",
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
        let f = flat_text(manual);
        for needle in [
            "`fsf N` moves forward N files from where the head is, so from the start of the tape it reaches File N",
            "mt -f /dev/nst0 fsf FIRST_POSITION",
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
            "A catalogue's listing marks every entry `[InRef]`, dar's word for \"the data is in the archive this catalogue was cut from\"",
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
            "You are done when every unit the newest index lists is restored at its newest version",
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
            "`age -d -o YOUR_KEY.age.key PROTECTED_FILE`",
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
            "A bare internal drive needs power from a power supply and the internal SAS cable",
            "A PCIe SAS adapter needs a free PCIe slot",
            "must run the plain \"IT\"",
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
            "\"without its catalogue\" means the envelope has none for that unit, or dar refuses the one it has",
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
            "./RESTORE.sh --restore --key YOUR_KEY.age.key --unit UNIT \\ --fill-from /destination/.tapectl-damaged-files.txt --to /destination",
            "Matching is by that sha256 and nothing else, never by the number or position of a slice or by a file name",
            "`RESTORE INCOMPLETE` with exit status 1",
            "Only a tape whose envelope lists the same unit, with the same uuid, at the same version is used",
            "another version can differ in the very files that are damaged",
            "so `--to` needs about twice the unit's size",
        ] {
            assert!(f.contains(needle), "{needle:?}: {f}");
        }
        let unit = ManifestUnit {
            name: "photos/2019".into(),
            uuid: "11111111-2222-3333-4444-555555555555".into(),
            snapshot_version: 2,
            stage_set_id: 1,
            dar_version: None,
            dar_command: None,
            slices: vec![],
        };
        for members in [vec![], vec!["11111111_v2.1.dar".to_string()]] {
            let r = generate_recovery_md("VOL", "alice", std::slice::from_ref(&unit), &members);
            let sec = flat_text(guide_section(&r, "If a slice or a file is damaged"));
            for needle in [
                "--slices-from DIR --to /destination",
                "--swap-tapes --to /destination",
                "--fill-from /destination/.tapectl-damaged-files.txt --to /destination",
                "found by its `sha256_encrypted` and by nothing else",
                "refuses a tape that holds another unit or another version",
                "Only the same unit at the same version is a copy",
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
            "Run `./RESTORE.sh --info` on EVERY tape you hold",
            "Start from the NEWEST tape",
            "`./RESTORE.sh --where UNIT --key YOUR_KEY.age.key` shows, for a unit, every version and every tape that holds it",
            "restore each unit's NEWEST version once, from a tape that holds it, into a directory of its own (`/destination/UNIT`), with that tape's own RESTORE.sh, and never an older version first",
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
        // Short: a page, not a chapter.
        assert!(guide_section(&g, "Start here").lines().count() <= 45);
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
            "With the operator or escrow key do not stop there: the tenants' envelopes come first and each lists only one tenant's units, so keep going to the operator envelope, the one whose tar also lists `PLAN.toml` (a tenant's never does), and whose MANIFEST.toml lists every unit on the tape",
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
            "ask `--where` for a name it does not have: it answers that the name is not in the index and prints the units it does list (the first 40)",
            "The Heir Kit, if you have one (\"Words this guide uses\"), records the archive as of the day it was made",
            "one made after your newest tape was written may name tapes this box lacks",
            "You are done when every unit the newest index lists is restored at its newest version (or the best you could reach) and every tape in the box has been looked at",
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
            "so everything on it takes at most that on the tape. Restored, its files take about as much room as they took on the tape, and more for a unit that was stored compressed",
            "`--to` needs room for about the size of the units you restore",
            "`--find-envelope` lists each unit with its size, and so does `--where`",
            "need more room, up to about TWICE a unit's size (its slices, then its restored files)",
            "the older-dar fallback above (all of the unit's slices), `--path` (only the few slices that hold the path), and asking for a replacement slice",
            "Doing it by hand (\"Manual Recovery Steps\") is the same: the decrypted slices and the restored files, about twice the unit's size; delete a unit's slices once it is restored",
            "For a whole box, the destination needs the sum of the sizes of the units you restore, whichever tapes they come from; the extra room for slices is needed for one unit at a time",
            "**Compressed units.**",
            "The dar catalogue cannot tell you",
            "With no `dar_command` line, allow generously",
        ] {
            assert!(disk.contains(needle), "{needle:?}: {disk}");
        }
        assert!(!disk.contains("need no more than that"), "{disk}");
        let manual = flat_text(guide_section(&g, "Manual Recovery Steps"));
        assert!(
            manual.contains("about the unit's size, and as much again for step 8's restored files: about twice the unit's size in all"),
            "{manual}"
        );
        // The unknown-generation ceiling says the same.
        let unknown = flat_text(&generate_system_guide_v2_for_media("U", 30, "DLT-IV"));
        assert!(
            unknown.contains("so everything on it takes at most that on the tape. Restored, its files take about as much room as they took on the tape"),
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
            verdict.contains("the files that lie wholly inside them can be restored by hand: \"If a slice or a file is damaged\" has the commands under \"No other copy\""),
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
            "must run the plain \"IT\" (initiator-target, pass-through) firmware",
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
            "**`requires`**: a line in File 0 and in the seal marker that lists the features a reader must understand to read the tape correctly. It is empty (`requires = []`)",
            "A version is also called a snapshot: MANIFEST.toml states it as `snapshot_version`, RECOVERY.md as \"snapshot v2\" and RESTORE.sh as \"snapshot version\"",
            "so the highest number is the newest",
            "A unit is identified by its `uuid`",
            "**Installation**: one tapectl setup, with its own catalogue and its own keys",
            "**Heir Kit, cover sheet**: the papers and files the operator prepares for the person who inherits the archive",
            "`catalog.db.age`, the whole catalogue encrypted to the escrow key",
        ] {
            assert!(words.contains(needle), "{needle:?}: {words}");
        }
        let tools = flat_text(guide_section(&g, "Tools Required"));
        for needle in [
            "Only for finding the drive and checking a key, never for the restore itself: `ls`, `lsscsi`, `lspci`, `dmesg`, `lsmod`, `modprobe`, `sudo` and `usermod`",
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
            "dd if=/dev/nst0 bs=512k | tr -d '\\0' | grep -E '^(label|uuid|media_type|cartridge_serial) ='",
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
            "(any copy of RESTORE.sh will do for this: see the end of this list)",
            "The modes that only read the tape (`--info`, `--verify`, `--find-envelope`, `--list`, `--where`, `--find`) describe the tape that is in the drive, whichever tape's RESTORE.sh runs them",
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
}
