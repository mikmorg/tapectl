//! Golden pins for two on-tape documents whose BYTES must not move while
//! their generators are restructured (architecture review 2026-09-11,
//! candidates C3 and C6): the envelope `MANIFEST.toml` and `RESTORE.sh`.
//!
//! Written by the coordinator BEFORE the refactors were dispatched, so a
//! worker cannot move bytes and re-pin in the same change. If one of these
//! fails, the on-tape format changed — that is a CTO decision (bytes on tape
//! are forever), never something to fix by updating the constant here.
//!
//! **RE-PINNED ONCE, 2026-09-22, under an explicit CTO ruling on issue #288.**
//! The rule above held: the worker that moved the bytes was fenced out of this
//! file and reported the new hash rather than writing it, and the constant was
//! updated by the coordinator after reading the generated script. The ruling
//! covers exactly three changes, batched deliberately onto one re-pin so the
//! habit is not trained by cosmetic ones: repeated `--key` so an heir holding
//! a keyring can restore a volume whose envelope and slices sit on different
//! key generations (#288); the three "no envelope opened" exits all carrying
//! the tape's identity and the rotation cause, with the quoting defect that
//! made two of them print `'\''VOL-A'\''` fixed (#291); and the carried
//! `MB` -> `MiB` that #218 ruled should ride the next substantive change.
//! The CTO's own note on the timing: this is the cheapest moment the change
//! will ever be available, because after the first production write an
//! improved heir script means tapes in the archive disagree with each other
//! about their own recovery instructions.
//!
//! This paragraph is the record, not a precedent. The next failure of this
//! test is a CTO decision again.
//!
//! **RE-PINNED A SECOND TIME, 2026-09-29, under the CTO's ruling on issue
//! #349**, made the day before the first production write, for the same
//! reason the first re-pin gave: this is the cheapest moment the change will
//! ever be available. RESTORE.sh runs `tar xf -` to unpack an envelope but did
//! not check for `tar` up front, so an heir on a minimal system met a late,
//! confusing failure. The ruling covers exactly one line — `tar` appended to
//! the prerequisite loop — and the test proved nothing else moved: undoing
//! that one word reproduced the previous pin (`bb29026c…`).
//!
//! **RE-PINNED A THIRD TIME, 2026-09-30, under the CTO's ruling recorded in
//! ADR-0012's 2026-09-30 amendment**, made while the first production
//! collection was still staging and before its first tape was written. The
//! pre-production structural review found that `--restore` decrypted a whole
//! unit into `${TMPDIR:-/tmp}` before dar ran, with no space check: on a
//! machine whose /tmp is RAM an heir could restore almost none of that tape's
//! bytes, and running out of space ended the script with no message or was
//! reported as a wrong key. The ruling covers one batch, taken on one re-pin:
//! slices decrypted into a scratch directory inside `--to` (or `--scratch
//! DIR`); a space check before the first slice is read (`--no-space-check`
//! skips it); out-of-space failures named as such; `--verify` hashing each
//! tape file as it streams instead of copying it to /tmp; any layout_version
//! other than 2 refused with the way to the tape's own script (a missing one
//! warns); and `tar` in the two Requirements lines (#363). The test checks
//! each item is present. The previous pin was `299a34c0…`.
//!
//! **RE-PINNED A FOURTH TIME, 2026-10-06, for tapectl 1.1.0, under the CTO's
//! rulings recorded in ADR-0012's 2026-10-06 amendments** (the minor version
//! moves because generated on-tape bytes change). One batch, one re-pin,
//! reviewed by the CTO: forward-only tape navigation with a cursor checked
//! against `mt status` (#396); a non-empty `--to` refused unless
//! `--overwrite`, dar's skip line fatal, and the rung-3 zero-strip recipe
//! stripping only trailing padding (#405); streaming restore into dar through
//! FIFOs, `--all` and repeated `--unit`, `--find-envelope` listing every
//! envelope, damage told apart from a wrong key, `mt-st` detection, the seal
//! read last, and dar `-N` (#412), with the envelope's dar catalogue given to
//! a streaming dar (2.7.13 loses a file's tail without it) and a fall-back
//! to slices on disk, and a dar that needs `gpg` named up front; `--list`
//! and `--path` reading the dar catalogues the envelopes already carry
//! (#418); and `magic`/`requires`
//! checks with the seal found at end of data (ADR-0012 amendment item 15,
//! #384). The test checks each item is present. The previous pin was
//! `6b64da4b…`.
//!
//! Re-pinned 2026-10-12 under ADR-0012 2026-10-12 item 44 (within the
//! 2026-10-07 on-tape authorization): every tenant envelope has a backup
//! copy on tape (#418 item 2, `volume-format-v2.md` §1.3), which RESTORE.sh
//! walks, reading a backup only when its primary is damaged or does not
//! read. The previous pin was `e8ca5902…`.
//!
//! **RE-PINNED 2026-10-16 for tapectl 1.2.0, under the CTO's "Approve re-pin"
//! (ADR-0012 2026-10-16 items 71-74, #449 and #448's File 1 half),** bounded by
//! item 72: this pin only, ending when v1.2.0 is installed. Five changes to
//! RESTORE.sh's bytes (File 1 and RECOVERY.md also change that day, but they
//! are not pinned here): a lone-unit restore (no `--unit`, no `--all`) says,
//! before RESTORE COMPLETE, how many other envelopes its key opened and how
//! many other units they list, and names `--list` and `--all` (it reads the
//! remaining envelopes first, forward only, and skips that where the work
//! directory could not hold the largest); each mode checks the tools it runs
//! (`TOOLS_READ`, `TOOLS_ENVELOPE`, `TOOLS_LIST`, `TOOLS_PATH`,
//! `TOOLS_RESTORE`, drawn from one table in `layout.rs`) instead of every tool
//! in every mode; the DAMAGED message says "a key matched" only when one did;
//! the work directory is described as it is (the decrypted envelopes, in
//! `${TMPDIR:-/tmp}`), with a pre-read estimate and a note that the space
//! check counts the slices' encrypted size; and `export LC_ALL=C`. The
//! Requirements lines and every other ruled item below are byte-identical.
//! Two things did not move (item 73) and are asserted below: "tapectl 1.1.0" in
//! `die_newer_needed`, and `requires` (empty, `KNOWN_FEATURES=""`). The
//! MANIFEST.toml pin did not move. The previous pin was `2a09fe98…`.
//!
//! **RE-PINNED 2026-10-17 for tapectl 1.3.0, under the CTO's rulings in
//! ADR-0012's 2026-10-16 amendment (items 78-81, #418 R4),** bounded by item 81:
//! this pin only, ending when v1.3.0 is installed. RESTORE.sh gains two modes
//! and nothing it already did changes (the diff against the previous script is
//! additions alone): `--where UNIT` prints the rows of the cross-volume index's
//! `index/units.tsv` for a unit (every version, its slices and bytes, and each
//! tape holding it), from the operator envelope with an operator or escrow key
//! and the tenant's with a tenant key; `--find TEXT` prints the paths of each
//! tenant envelope's `index/paths.tsv` that contain a literal piece of text
//! (matched on the decoded path, at most 50 printed, then the holders of each
//! version printed); a tape with no index says so and exits 1. Both read
//! forward from one rewind, take their tools from one new row of the per-mode
//! table (`TOOLS_INDEX`, the `--find-envelope` set), use three named awk
//! fragments (`AWK_INDEX_COMMON`, `AWK_INDEX_WHERE`, `AWK_INDEX_FIND`) that read
//! the same under gawk, mawk, nawk and busybox awk, and are in `--help`, the
//! header comment and `--info`'s closing hints. The `Requirements:` lines, the
//! other four `TOOLS_*` sets and every ruled item below are byte-identical. The
//! two fixed points of item 81 did not move and are asserted below: "tapectl
//! 1.1.0" in `die_newer_needed` (the only "tapectl 1." in the script), and
//! `requires` (empty, `KNOWN_FEATURES=""`). The MANIFEST.toml pin did not move.
//! The previous pin was `6c7bb948…`.
//!
//! **The 1.3.0 pin WIDENED, 2026-10-17, under the CTO's rulings in ADR-0012's
//! 2026-10-17 amendment (items 101 and 102, #480),** within the same bound:
//! this pin only, ending when v1.3.0 is installed (item 81 says the re-pin
//! widens to carry RESTORE.sh's half of both). The first 1.3.0 pin
//! (`36c970aa…`, `--where` and `--find`) was ADDITIONS ALONE; this one is `594da829…`. This widening is
//! not, and says so. ADDED: `--slices-from DIR` (repeatable) and `--swap-tapes`
//! on `--restore` and `--verify`, which take a slice that fails its ciphertext
//! hash or will not read from another copy of the unit, matched by its
//! `sha256_encrypted` from the front index and by nothing else (a `dd` copy,
//! block padding and all, is trimmed to the size the index states), from a
//! directory or from a second cartridge read forward from one rewind after the
//! first (`other_tape_map`, `swap_fetch`, `swap_loop`); a slice still missing
//! is left out and listed, never reported complete; `--fill-from LIST` on
//! `--restore`, which restores only the files a `.tapectl-damaged-files.txt`
//! names, from a tape whose envelope lists the same unit uuid at the same
//! version, overwriting only those files (`do_fill`); `dar_result`, which reads
//! the files dar says it could not restore (`AWK_DAR_DAMAGED`, a new named
//! awk fragment, as is `AWK_FILL_LISTING`) into that list; and the `--help`,
//! header-comment and hint text for all of it. CHANGED, and the reason a
//! reader should look: (1) the end of every restore is now `restore_end`,
//! which prints `RESTORE COMPLETE` only when no unit came back short and
//! otherwise `RESTORE INCOMPLETE` with exit status 1 (it was printed
//! unconditionally after the last unit); (2) the dar tail of `stream_unit`,
//! `spool_unit` and `restore_paths` goes through `dar_result`: status 5 with
//! the files named is a list and INCOMPLETE, a file-system refusal (permission
//! denied, read-only, disk quota) and every other status stay fatal as before;
//! (3) every dar gets an absolute `-R`, because dar prints it word for word in
//! its messages; (4) `check_destination` is split, the stale-scratch refusal
//! becoming `check_stale_scratch` so a fill, whose directory is full by
//! definition, bypasses only the emptiness refusal; (5) `decrypt_slice` takes an
//! optional byte count and `spool_slices` a target directory and a file for
//! the slices not found; (6) `--verify` has an option parser and names, for a
//! failed data slice, whether a copy is in a `--slices-from` directory or on a
//! second cartridge; (7) `stream_unit` reports an age failure that names a
//! decrypt, an authentication or a header as the slice's before asking whether
//! dar is gone (a damaged chunk shortens the slice and dar can have died of it
//! first, which hid the slice behind dar's exit); (8) a bad slice's failure
//! names `--slices-from` and `--swap-tapes`, and the rejected-key message says a
//! damaged slice reads that way too. A restore with neither option is
//! unchanged except for those messages and the dar tail above. The two fixed
//! points did not move and are asserted below: "tapectl 1.1.0" in
//! `die_newer_needed`, and `requires` (empty, `KNOWN_FEATURES=""`). The
//! MANIFEST.toml pin did not move. The previous pin was `36c970aa…`.
//!
//! **The 1.3.0 pin WIDENED AGAIN, 2026-10-19, under ADR-0012 item 103 (the
//! heir checks every restored file against its recorded sha256, #418),**
//! within the same bound: this pin only, ending when v1.3.0 is installed. The
//! previous pin was `594da829…`; this one is `072b0c6e…`. dar's per-file check
//! is a short checksum where equal byte flips cancel, so a damaged file could
//! come back with no error; the tenant envelope's `index/paths.tsv` now carries
//! each regular file's recorded sha256 (a `sha256` column between `size_bytes`
//! and `path`, found by its header name; a reader that does not know the column
//! ignores it, so it needs no `requires` entry), and RESTORE.sh checks the
//! files it restored against it. ADDED: `--no-file-check` on `--restore`
//! (`--fill-from` too); `file_check`, which reads the restored files back from
//! `--to` and checks them with one `sha256sum -c --quiet` pass over the unit's
//! hashed regular files (a name holding a backslash, a newline or a carriage
//! return is hashed alone, from standard input, because `sha256sum` escapes
//! such a name differently between versions); `index_for_unit`, which finds the
//! `paths.tsv` that lists the unit and version among the envelopes the run has
//! open (the one in use first; an escrow or operator key restores from the
//! operator envelope, which has none, and the tenant envelope that lists the
//! unit was opened before it); `finish_damage`; three named awk fragments,
//! `AWK_FILE_CHECK_LISTED`, `AWK_FILE_CHECK_LIST` and `AWK_FILE_CHECK_FAILED`;
//! and the header-comment, `--help` and message text for all of it. CHANGED:
//! `dar_result` now always ends in `finish_damage`, which runs the check and
//! unions the files dar named with the ones the check finds into one
//! damaged-file list, so `record_damage` and `clear_stale_list` are reached
//! through it only; `damage_note` has three wordings, by how many files the
//! check added; `fill_end` checks the files the fill restored and keeps a
//! failing one in the list. No tool was added: `sha256sum` was in every mode's
//! set already, and the `TOOLS_*` lines did not move. A tape with no index
//! (written before 1.3.0), or a file with no sha256 recorded, is not checked,
//! and the script says so. The tape is not touched by the check (it reads the
//! restored files and the envelope already on disk). The fixed points did not
//! move and are asserted below: "tapectl 1.1.0" in `die_newer_needed`,
//! `requires` (empty) and `KNOWN_FEATURES=""`. The MANIFEST.toml pin did not
//! move.
//!
//! **The 1.3.0 pin WIDENED A THIRD TIME, 2026-10-19, under the CTO's ruling
//! on the third fresh read of File 1 ("fix the two, then tag", #477),** within
//! the same bound: this pin only, ending when v1.3.0 is installed. The previous
//! pin was `072b0c6e…`; this one is `95862b49…`. A reader who held a whole box
//! could not list it: `--find-envelope` shows only the loaded tape's units, and
//! the only way to see the index was `--where` with a name it lacked, which
//! printed "the first 40". ADDED: `--where --all`, a form of `--where` (a bare
//! `--where` stays a usage error): `do_where` takes `--all` and calls
//! `where_every_unit`, which reads each `index/units.tsv` whole through a new
//! named awk fragment, `AWK_INDEX_ALL`, and prints EVERY unit the index names
//! with no limit: the summary, the units whose newest version this tape does not
//! hold (with the tapes that do), then each unit with its newest version (the
//! highest by number, keyed by tenant, name and unit uuid, so a name made twice
//! is two units), whether this tape holds it, and every version with each
//! holder as `--where` prints them. It reads the same envelopes with the same
//! keys as `--where UNIT` (the operator envelope's map for an escrow or operator
//! key, each tenant's for tenant keys), reads forward from one rewind, and
//! exits 1 on a tape with no index. It takes its tools from the same per-mode
//! row as `--where` and `--find` (`TOOLS_INDEX`); no tool was added. The usage
//! line, the header comment, `--help` and `--info`'s closing hints name it.
//! CHANGED: the `--where` usage and error messages say `UNIT|--all`; the miss
//! path of `--where UNIT` no longer pipes the units it names through
//! `head -n 40` (it names every one, and points at `--where --all`). The
//! `Requirements:` lines, the five `TOOLS_*` sets and every ruled item below are
//! byte-identical. The fixed points did not move and are asserted below:
//! "tapectl 1.1.0" in `die_newer_needed`, `requires` (empty) and
//! `KNOWN_FEATURES=""`. The MANIFEST.toml pin did not move.
//!
//! **RE-PINNED 2026-10-20 for tapectl 1.6.0, under ADR-0012 item 116** (the CTO
//! pre-authorized the moves items 111-115 name, and nothing else; the bound ends
//! when v1.6.0 is installed on home2). The previous RESTORE.sh pin was
//! `95862b49…` (kept byte for byte as `tests/fixtures/restore_sh_1_5_0/`); this
//! one is `d9787a4f…`. Every hunk of the diff is the short seal's reading
//! (`volume-format-v2.md` §4.1, §4.2): `KNOWN_FEATURES="short-seal"` and
//! "tapectl 1.6.0" in `die_newer_needed` (item 73's fixed points, moved by item
//! 111); the seal's parse rules S1, S2 and S5; on a tape that requires
//! short-seal, the seal read right after File 3 by spacing to the end of the data
//! (R1), the file map trimmed to `ended_after`, NOT ON TAPE for the planned files
//! after it under `--info` and `--verify` (R3), a cut unit refused or named and
//! never restored (R4), `--where`/`--find` marking this tape's cut versions, and
//! a second cartridge's seal read first under `--swap-tapes` (R5). A 1.5.0 tape
//! reads exactly as before. The MANIFEST.toml pin moved by one line,
//! `may_end_early = true` after `created_at` (item 111).
//!
//! **RE-PINNED 2026-10-23 for tapectl 1.7.0, under ADR-0012 item 135** (the CTO
//! pre-authorized the moves items 121-134 name, and item 140 within them; the
//! bound ends when v1.7.0 is installed on home2). The previous RESTORE.sh pin was
//! `d9787a4f…` (kept byte for byte as `tests/fixtures/restore_sh_1_6_0/`); this
//! one is `16a21522…`. Every hunk of the diff is spanning's reading on one tape
//! (`volume-format-v2.md` §4.4) or item 140's one line: `KNOWN_FEATURES="short-seal
//! spanning"` and "tapectl 1.7.0" in `die_newer_needed` (item 73's fixed points,
//! moved by item 124); a slice with no `tape_position` never read and never taken
//! for file 0 (R6); a version classed whole, range or cut from which of its slices
//! are here; a range refused by `--restore` and `--path` before any slice is read,
//! named by `--all` and `--list` with its run, never a fill source, and printed
//! with each holder's run by `--where` and `--find` (R7, item 126); and one NOT ON
//! TAPE line for the planned files after the end of every short tape, under
//! `--info` and `--verify` (item 140). A 1.6.0 or 1.5.0 tape reads as before. Neither
//! MANIFEST.toml pin moved.
//!
//! **Moved again the same day, still under item 135**, for the read across
//! cartridges (`16a21522…` to `7f1b7db0…`; §4.4 "Where a restore starts", "The
//! default mode keeps ciphertext in scratch", "`--stream-tapes`"; items 88, 132,
//! 139). `--restore --unit` of a spanning unit starts only on a tape holding slice
//! 1 and names slice 1's holders otherwise; by default each tape's slices wait in
//! scratch still encrypted, with the space check counting the whole ciphertext
//! and naming `--stream-tapes`; `--stream-tapes` feeds one dar run across the swaps;
//! the next cartridge is taken only by R8 (its seal read first, then the slice's
//! sha256 and size at a position the seal covers); and the one swap prompt reads
//! its answer from the input with no time limit and no device held open. Every
//! substring pinned before still occurs as often as it did, and no `TOOLS_*` line
//! moved.
//!
//! **Moved a third time that day, still under item 135**, for the rest of §4.4's
//! RESTORE.sh bullets (`7f1b7db0…` to `39d2535e…`; "`--restore --path`" and
//! "`--slices-from` and `--swap-tapes`"; R5, R8, R9; items 132 and 139).
//! `--restore --path` of a spanning unit reads the slices it needs (the paths'
//! `-Tslice` range and the last) from whichever tapes hold them, the tape in the
//! drive first, each still encrypted in scratch and checked against its own
//! tape's front index, and decrypts them only once all are there; it has no
//! start rule. `--swap-tapes` reads every offered cartridge's seal first and
//! takes a slice only at a position the seal covers, a range tape included;
//! `--slices-from` stays hash-only. A refusal relayed from an offered cartridge
//! says the restore goes on, and with `--stream-tapes` a #419 refusal after a
//! swap ends the run. Two substrings pinned before were replaced by the code
//! they pinned (`OTHER_END` for every second tape; `die_range_path` only with a
//! salvage flag), and their replacements are pinned; every other substring
//! still occurs as often as it did, and no `TOOLS_*` line moved.
//!
//! **Moved a fourth time that day, still under item 135**, for the fresh read
//! of File 1 that item 135 asks for before the tag (`39d2535e…` to
//! `707c5c87…`; ADR-0012 item 108's stopping rule; its two serious findings).
//! `--find-envelope`'s unit table marks a unit this tape's plan holds only part
//! of (`PART`: its block, item 125, gives a `tape_position` to fewer slices than
//! it lists), so two tapes that each hold part of a unit are not read as two
//! copies. The cross-tape prompts say to eject the cartridge in the drive first,
//! and that `done` or `quit` stops the restore and discards the slices read; the
//! notes and `--help` say an early end removes the scratch directory (screen or
//! tmux), that a restore across tapes keeps its slices in scratch by default, and
//! that a fill needs a tape holding the version WHOLE. Text only but for the
//! table's mark; every first line of a prompt, refusal and verdict is unchanged,
//! every substring pinned before still occurs as often as it did, and no
//! `TOOLS_*` line moved.
//!
//! **Moved a fifth time, 2026-10-24, under ADR-0012 item 145** (outside item
//! 135's list; the CTO's own ruling): RESTORE.sh's damage handling (`707c5c87…`
//! to `1cd5a405…`). R1: the per-file check no longer parses `sha256sum -c`'s
//! report (a `sha256sum` that shell-quotes a name with a space, a colon or a TAB
//! left that damaged file off the list, and a fill then said FILL COMPLETE): each
//! file is hashed on its own from standard input, so no name is read back, and
//! `AWK_FILE_CHECK_FAILED`, `CHECK_UNNAMED` and the "could not be matched" warning
//! are gone, which removes the one-pass `sha256sum -c` pin. R2: a folder whose own
//! entry was in the lost slice is replaced on the list by what the index records
//! under it (`expand_damaged_dirs`), and with no index the fill restores what is
//! missing in it without overwriting (`fill_runs`, `dar -n`); the fill also reads
//! the slices of every folder above a listed path, without which dar restores
//! nothing inside such a folder. Every first line of a prompt, refusal and verdict
//! is unchanged, every other substring keeps its count, and no `TOOLS_*` line moved.
//!
//! **Moved a sixth time, 2026-10-26, under ADR-0012 item 176** (#497; the CTO's
//! own ruling, before the `v1.7.0` tag): `stream_unit` (`1cd5a405…` to
//! `25505925…`). Given the isolated catalogue (`-A`), dar stops reading where
//! the file data ends and exits 0, leaving the archive's own trailing catalogue
//! unread; a tail larger than a pipe's buffer used to end a whole restore
//! `FATAL: dar extraction failed (exit 0)`. What dar leaves of a slice, and any
//! slice after it, is now read to its end, hashed and authenticated by age into
//! nothing (`{ cat >"$sink"; cat >/dev/null; }` after age), dar's exit status is
//! taken once (`dar_reap`, `DAR_RC`) and decides, and a dar that failed is still
//! named by `die_dar`. Every first line of a prompt, refusal and verdict is
//! unchanged, every other substring keeps its count, and no `TOOLS_*` line moved.
//!
//! **Moved a seventh time, 2026-10-27, under ADR-0012 item 181** (#498, with
//! item 182; the CTO's one authorized re-pin for 1.8.0): `stream_unit`
//! (`25505925…` to `7285a4ff…`). dar 2.7.7 to 2.8.6 can take a slice's
//! trailer byte for data through a named pipe per slice, so the slices are
//! joined back into the `dar -c -` stream (`stream_check`, `join_slice`,
//! `join_rest`, the `AWK_DAR_HEADER` fragment) and fed to `dar -x -` through one
//! FIFO held open as fd 3; a host whose `head` or dar cannot join them spools a
//! unit on one tape and refuses one across tapes before any slice. Damage found
//! after dar ended well is `late_damage`'s note, and the per-file check decides
//! (item 182). The two needles that named the per-slice FIFOs name the stream's
//! lines instead, `spool_unit`'s names the old-dar path beside a needle for the
//! fallback, and every other substring pinned before keeps its count; no
//! `TOOLS_*` line moved.
//!
//! **Moved an eighth time, 2026-10-28, under ADR-0012 item 184** (#498; a fix
//! to item 181 before the `v1.8.0` tag): `join_rest` (`7285a4ff…` to
//! `dee817ab…`). Ubuntu 26.04's default coreutils, uutils 0.10.0, never returns
//! from `tail -c 1` on a FIFO that carries under 64 KiB, and the last slice's
//! trailer was read that way, so a restore hung there. Every slice is now cut
//! at the plaintext size the envelope's manifest gives it (`size_bytes`, written
//! beside the slice list by `AWK_SELECT_VERSION`'s `plain_out`), with `head -c`
//! alone: no `tail` on a FIFO, no `head -c -1`, and `stream_check` no longer
//! tests it. The `mkfifo` needle loses the `trail` FIFO, the `join_slice` and
//! `join_rest` needles take the size, the `head -c -1` needle is gone, and two
//! name the cut and the `awk` call; every other substring keeps its count, and
//! no `TOOLS_*` line moved.
//!
//! **Moved a ninth time, 2026-10-28, under ADR-0012 item 184** (#498; a fix
//! to item 182 before the `v1.8.0` tag): `stream_unit` (`dee817ab…` to
//! `a5dc4063…`). Damage found while dar's stream was open was ruled FATAL
//! unless dar had already closed it, and uutils coreutils 0.10.0 enlarges the
//! pipes it writes to 1 MiB, so on Ubuntu 26.04 the join wrote past the damage
//! before dar closed anything and damage dar never needed ended the restore
//! (PR #500's CI). Now the stream is closed at the damage and dar is waited
//! for: dar ending 0 needed nothing past it (item 182's note, the per-file
//! check decides), any other end is FATAL with the texts it had. Three needles
//! pin the rule; every other substring keeps its count, and no `TOOLS_*` line
//! moved.
//!
//! `MANIFEST.toml` carries a `created_at` timestamp; that one line is
//! normalised before comparison and is the only thing allowed to vary.
//!
//! **A THIRD PIN, ADDED 2026-10-20 for tapectl 1.6.0 (#420):** the short seal
//! marker (`volume-format-v2.md` §4.1), so the format of the seal a tape ends
//! with when the drive's early warning comes cannot drift silently. It was
//! added as a new pin, not a re-pin: no seal was pinned before. Its
//! `sealed_at` line is normalised as MANIFEST.toml's `created_at` is, and is
//! the only thing allowed to vary. The same rule holds: if it fails, the
//! on-tape format changed, which is a CTO decision.
//!
//! **2026-11-18, T4.0 of the 2.0.0 plan: the Layout 2 pins are removed**, each in a
//! commit of its own under ADR-0012 item 243 (RESTORE.sh, the MANIFEST range block, the
//! MANIFEST, the short seal and the two tests that parsed it). The history above is kept.
//! Layout 3's pins come in T6.3 (`docs/design/v3-implementation-plan.md`), each in a commit
//! of its own, and pin no data-zone bytes made by tar (#562).
//!
//! **2026-11-21, T6.3 of the 2.0.0 plan: the Layout 3 pins begin here**, under ADR-0012
//! item 243 (the on-tape authorization), each in a commit of its own. Each is made by the
//! real generator the write path calls, fed fixed inputs (a fixed uuid, label, dates, keys
//! and stage set), so the bytes are the same on every run and every host. No pin holds
//! data-zone bytes made by tar (two tar versions differ; #562) or age ciphertext (age draws
//! fresh keys and nonces). **If one of these fails, the on-tape format changed: that is a
//! CTO decision (bytes on tape are forever), never something to fix by updating the
//! constant here**, and a pin moves only in a commit of its own. A pin that holds a whole
//! text shows the format in the test; one that holds a sha256 writes the generated text to
//! a file under the target directory when it fails and names that file.

use tapectl::volume::layout::{
    generate_front_index, generate_id_thunk_v2, FrontIndexFile, IdThunkV2Params,
};
use tapectl::volume::manifest::{Manifest, ManifestSlice, ManifestUnit};

/// Compare a generated document with its pinned text. On a difference the generated text
/// is written under the target directory and the failure names the file.
#[allow(dead_code)]
fn assert_pinned_text(name: &str, actual: &str, expected: &str) {
    if actual != expected {
        let path =
            std::path::Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("golden-{name}.actual"));
        let _ = std::fs::write(&path, actual);
        panic!(
            "the on-tape bytes of {name} changed: a CTO decision, never a re-pin by an agent. \
             The generated text is in {}",
            path.display()
        );
    }
}

/// Compare a generated document's sha256 with its pin; on a difference the generated text
/// is written under the target directory and the failure names the file.
#[allow(dead_code)]
fn assert_pinned_sha256(name: &str, actual: &str, expected_hex: &str) {
    use sha2::{Digest, Sha256};
    let got = format!("{:x}", Sha256::digest(actual.as_bytes()));
    if got != expected_hex {
        let path =
            std::path::Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("golden-{name}.actual"));
        let _ = std::fs::write(&path, actual);
        panic!(
            "the on-tape bytes of {name} changed: a CTO decision, never a re-pin by an agent. \
             sha256 {got}, pinned {expected_hex}; the generated text is in {}",
            path.display()
        );
    }
}

const FILE_0: &str = r##"================================================================
                     TAPECTL ARCHIVAL VOLUME
================================================================

Label:   L8-0001
Media:   LTO-8
Created: 2026-11-21T10:00:00+00:00

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
label = "L8-0001"
uuid = "0a1b2c3d-4e5f-4a6b-8c7d-9e0f1a2b3c4d"
layout_version = 3
requires = []
tapectl_version = "2.0.0"
media_type = "LTO-8"
nominal_capacity_bytes = 12000000000000
mam_capacity_bytes = 11999999000000
created_at = "2026-11-21T10:00:00+00:00"

[layout]
front_index = 3
seal_marker = 8
total_files = 9

[media]
cartridge_manufacturer = "FUJIFILM"
cartridge_serial = "ABC123"
cartridge_identity_source = "mam"
tape_length_meters = 960
load_count_at_write = 7
"##;

#[test]
fn layout3_file_0_id_thunk() {
    let params = IdThunkV2Params {
        label: "L8-0001",
        uuid: "0a1b2c3d-4e5f-4a6b-8c7d-9e0f1a2b3c4d",
        media_type: "LTO-8",
        tapectl_version: "2.0.0",
        nominal_capacity: 12_000_000_000_000,
        mam_capacity: 11_999_999_000_000,
        total_files: 9,
        mam_manufacturer: "FUJIFILM",
        mam_serial: "ABC123",
        mam_length: 960,
        mam_loads: 7,
        created_at: "2026-11-21T10:00:00+00:00",
        cartridge_identity_source: Some("mam"),
    };
    assert_pinned_text("file-0", &generate_id_thunk_v2(&params), FILE_0);
}

fn slice(
    number: i64,
    tape_position: Option<i64>,
    sizes: (i64, i64),
    hashes: (&str, &str),
    continues: bool,
) -> ManifestSlice {
    ManifestSlice {
        number,
        tape_position,
        size_bytes: sizes.0,
        encrypted_bytes: sizes.1,
        sha256_plain: hashes.0.repeat(64),
        sha256_encrypted: hashes.1.repeat(64),
        continues,
    }
}

const MANIFEST_WHOLE_UNITS: &str = r##"[manifest]
volume = "L8-0001"
tenant = "alpha"
created_at = "2026-11-21T10:00:00+00:00"
may_end_early = true

[[units]]
name = "photos/2019"
uuid = "11111111-2222-3333-4444-555555555555"
snapshot_version = 7
stage_set_id = 42
payload = "tar"
stream_bytes = 40960
file_table = "tables/11111111_v7_s42.tsv"
id_map = "tables/11111111_v7_s42.ids.tsv"
tar_version = "tar (GNU tar) 1.35"

[[units.slices]]
number = 1
tape_position = 9
size_bytes = 20480
encrypted_bytes = 20512
sha256_plain = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
sha256_encrypted = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"

[[units.slices]]
number = 2
tape_position = 10
size_bytes = 20480
encrypted_bytes = 20512
sha256_plain = "cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc"
sha256_encrypted = "dddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddd"

[[units]]
name = "ledgers/fy24"
uuid = "66666666-7777-8888-9999-000000000000"
snapshot_version = 1
stage_set_id = 43
payload = "tar"
stream_bytes = 31744
file_table = "tables/66666666_v1_s43.tsv"
id_map = "tables/66666666_v1_s43.ids.tsv"
tar_version = "tar (GNU tar) 1.35"
compression = "zstd"

[[units.slices]]
number = 1
tape_position = 11
size_bytes = 20480
encrypted_bytes = 4100
sha256_plain = "eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee"
sha256_encrypted = "ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff"

[[units.slices]]
number = 2
tape_position = 12
size_bytes = 11264
encrypted_bytes = 2300
sha256_plain = "1111111111111111111111111111111111111111111111111111111111111111"
sha256_encrypted = "2222222222222222222222222222222222222222222222222222222222222222"
continues = true

"##;

/// A tenant envelope's two whole units: every key of a block (the first, uncompressed),
/// and a compressed staging whose second slice continues a member (the second).
/// The input is fixed; `Manifest::to_toml` is the writer `generate_manifest_toml` calls.
#[test]
fn layout3_manifest_whole_units() {
    let manifest = Manifest {
        volume: "L8-0001".to_string(),
        tenant: "alpha".to_string(),
        created_at: "2026-11-21T10:00:00+00:00".to_string(),
        may_end_early: true,
        units: vec![
            ManifestUnit {
                name: "photos/2019".to_string(),
                uuid: "11111111-2222-3333-4444-555555555555".to_string(),
                snapshot_version: 7,
                stage_set_id: 42,
                payload: "tar".to_string(),
                stream_bytes: 40_960,
                file_table: Some("tables/11111111_v7_s42.tsv".to_string()),
                id_map: Some("tables/11111111_v7_s42.ids.tsv".to_string()),
                tar_version: Some("tar (GNU tar) 1.35".to_string()),
                compression: None,
                slices: vec![
                    slice(1, Some(9), (20_480, 20_512), ("a", "b"), false),
                    slice(2, Some(10), (20_480, 20_512), ("c", "d"), false),
                ],
            },
            ManifestUnit {
                name: "ledgers/fy24".to_string(),
                uuid: "66666666-7777-8888-9999-000000000000".to_string(),
                snapshot_version: 1,
                stage_set_id: 43,
                payload: "tar".to_string(),
                stream_bytes: 31_744,
                file_table: Some("tables/66666666_v1_s43.tsv".to_string()),
                id_map: Some("tables/66666666_v1_s43.ids.tsv".to_string()),
                tar_version: Some("tar (GNU tar) 1.35".to_string()),
                compression: Some("zstd".to_string()),
                slices: vec![
                    slice(1, Some(11), (20_480, 4_100), ("e", "f"), false),
                    slice(2, Some(12), (11_264, 2_300), ("1", "2"), true),
                ],
            },
        ],
    };
    assert_pinned_text(
        "manifest-whole-units",
        &manifest.to_toml(),
        MANIFEST_WHOLE_UNITS,
    );
}

const MANIFEST_RANGE_BLOCK: &str = r##"[manifest]
volume = "L8-0002"
tenant = "alpha"
created_at = "2026-11-21T10:00:00+00:00"
may_end_early = true

[[units]]
name = "photos/2019"
uuid = "11111111-2222-3333-4444-555555555555"
snapshot_version = 7
stage_set_id = 42
payload = "tar"
stream_bytes = 61440
file_table = "tables/11111111_v7_s42.tsv"
id_map = "tables/11111111_v7_s42.ids.tsv"
tar_version = "tar (GNU tar) 1.35"
compression = "xz"

[[units.slices]]
number = 1
size_bytes = 20480
encrypted_bytes = 9000
sha256_plain = "1111111111111111111111111111111111111111111111111111111111111111"
sha256_encrypted = "4444444444444444444444444444444444444444444444444444444444444444"

[[units.slices]]
number = 2
size_bytes = 20480
encrypted_bytes = 9001
sha256_plain = "2222222222222222222222222222222222222222222222222222222222222222"
sha256_encrypted = "5555555555555555555555555555555555555555555555555555555555555555"

[[units.slices]]
number = 3
tape_position = 4
size_bytes = 20480
encrypted_bytes = 9002
sha256_plain = "3333333333333333333333333333333333333333333333333333333333333333"
sha256_encrypted = "6666666666666666666666666666666666666666666666666666666666666666"
continues = true

"##;

/// A range block (volume-format-v3.md section 5.3): the staging continues on this tape
/// from slice 3, so slices 1 and 2 are listed with no `tape_position` and slice 3, which
/// continues a member begun in slice 2, is the run's first slice here.
#[test]
fn layout3_manifest_range_block() {
    let manifest = Manifest {
        volume: "L8-0002".to_string(),
        tenant: "alpha".to_string(),
        created_at: "2026-11-21T10:00:00+00:00".to_string(),
        may_end_early: true,
        units: vec![ManifestUnit {
            name: "photos/2019".to_string(),
            uuid: "11111111-2222-3333-4444-555555555555".to_string(),
            snapshot_version: 7,
            stage_set_id: 42,
            payload: "tar".to_string(),
            stream_bytes: 61_440,
            file_table: Some("tables/11111111_v7_s42.tsv".to_string()),
            id_map: Some("tables/11111111_v7_s42.ids.tsv".to_string()),
            tar_version: Some("tar (GNU tar) 1.35".to_string()),
            compression: Some("xz".to_string()),
            slices: vec![
                slice(1, None, (20_480, 9_000), ("1", "4"), false),
                slice(2, None, (20_480, 9_001), ("2", "5"), false),
                slice(3, Some(4), (20_480, 9_002), ("3", "6"), true),
            ],
        }],
    };
    assert_pinned_text(
        "manifest-range-block",
        &manifest.to_toml(),
        MANIFEST_RANGE_BLOCK,
    );
}

const MANIFEST_OPERATOR_ENVELOPE: &str = r##"[manifest]
volume = "L8-0001"
tenant = "operator"
created_at = "2026-11-21T10:00:00+00:00"
may_end_early = true

[[units]]
name = "photos/2019"
uuid = "11111111-2222-3333-4444-555555555555"
snapshot_version = 7
stage_set_id = 42
payload = "tar"
stream_bytes = 40960
tar_version = "tar (GNU tar) 1.35"

[[units.slices]]
number = 1
tape_position = 9
size_bytes = 20480
encrypted_bytes = 20512
sha256_plain = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
sha256_encrypted = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"

[[units.slices]]
number = 2
tape_position = 10
size_bytes = 20480
encrypted_bytes = 20512
sha256_plain = "cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc"
sha256_encrypted = "dddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddd"

[[units]]
name = "ledgers/fy24"
uuid = "66666666-7777-8888-9999-000000000000"
snapshot_version = 1
stage_set_id = 43
payload = "tar"
stream_bytes = 10240

[[units.slices]]
number = 1
tape_position = 11
size_bytes = 10240
encrypted_bytes = 10272
sha256_plain = "eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee"
sha256_encrypted = "ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff"

"##;

/// The operator envelope's manifest (section 1.4): tenant `operator`, and no `file_table`,
/// `id_map` (the catalog.db carries them), and a block with no `tar_version`.
#[test]
fn layout3_manifest_operator_envelope() {
    let manifest = Manifest {
        volume: "L8-0001".to_string(),
        tenant: "operator".to_string(),
        created_at: "2026-11-21T10:00:00+00:00".to_string(),
        may_end_early: true,
        units: vec![
            ManifestUnit {
                name: "photos/2019".to_string(),
                uuid: "11111111-2222-3333-4444-555555555555".to_string(),
                snapshot_version: 7,
                stage_set_id: 42,
                payload: "tar".to_string(),
                stream_bytes: 40_960,
                file_table: None,
                id_map: None,
                tar_version: Some("tar (GNU tar) 1.35".to_string()),
                compression: None,
                slices: vec![
                    slice(1, Some(9), (20_480, 20_512), ("a", "b"), false),
                    slice(2, Some(10), (20_480, 20_512), ("c", "d"), false),
                ],
            },
            ManifestUnit {
                name: "ledgers/fy24".to_string(),
                uuid: "66666666-7777-8888-9999-000000000000".to_string(),
                snapshot_version: 1,
                stage_set_id: 43,
                payload: "tar".to_string(),
                stream_bytes: 10_240,
                file_table: None,
                id_map: None,
                tar_version: None,
                compression: None,
                slices: vec![slice(1, Some(11), (10_240, 10_272), ("e", "f"), false)],
            },
        ],
    };
    assert_pinned_text(
        "manifest-operator-envelope",
        &manifest.to_toml(),
        MANIFEST_OPERATOR_ENVELOPE,
    );
}

fn file(
    position: i32,
    type_label: &'static str,
    size_bytes: Option<u64>,
    hash_digit: Option<&str>,
) -> FrontIndexFile {
    FrontIndexFile {
        position,
        type_label,
        size_bytes,
        sha256_encrypted: hash_digit.map(|d| d.repeat(64)),
    }
}

/// The plan of an eleven-file tape: File 0 to the seal marker at 10, one of each kind of
/// file the format lists (volume-format-v3.md section 4). File 3 (the index itself) and the
/// seal marker carry no `size_bytes` and no `sha256_encrypted`. Used by the front-index and
/// the seal-marker pins.
fn eleven_file_plan() -> Vec<FrontIndexFile> {
    vec![
        file(0, "id_thunk", Some(1_996), Some("0")),
        file(1, "system_guide", Some(41_212), Some("1")),
        file(2, "restore_sh", Some(180_000), Some("2")),
        file(3, "front_index", None, None),
        file(4, "operator_envelope", Some(30_720), Some("4")),
        file(5, "operator_envelope_backup", Some(30_720), Some("5")),
        file(6, "tenant_envelope", Some(20_480), Some("6")),
        file(7, "tenant_envelope_backup", Some(20_480), Some("7")),
        file(8, "data_slice", Some(20_512), Some("8")),
        file(9, "data_slice", Some(2_300), Some("9")),
        file(10, "seal_marker", None, None),
    ]
}

const FRONT_INDEX: &str = r##"================================================================
                    TAPECTL FRONT INDEX
================================================================

Volume: L8-0001
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
volume = "L8-0001"
layout_version = 3
may_end_early = true

[[files]]
position = 0
type = "id_thunk"
size_bytes = 1996
sha256_encrypted = "0000000000000000000000000000000000000000000000000000000000000000"

[[files]]
position = 1
type = "system_guide"
size_bytes = 41212
sha256_encrypted = "1111111111111111111111111111111111111111111111111111111111111111"

[[files]]
position = 2
type = "restore_sh"
size_bytes = 180000
sha256_encrypted = "2222222222222222222222222222222222222222222222222222222222222222"

[[files]]
position = 3
type = "front_index"

[[files]]
position = 4
type = "operator_envelope"
size_bytes = 30720
sha256_encrypted = "4444444444444444444444444444444444444444444444444444444444444444"

[[files]]
position = 5
type = "operator_envelope_backup"
size_bytes = 30720
sha256_encrypted = "5555555555555555555555555555555555555555555555555555555555555555"

[[files]]
position = 6
type = "tenant_envelope"
size_bytes = 20480
sha256_encrypted = "6666666666666666666666666666666666666666666666666666666666666666"

[[files]]
position = 7
type = "tenant_envelope_backup"
size_bytes = 20480
sha256_encrypted = "7777777777777777777777777777777777777777777777777777777777777777"

[[files]]
position = 8
type = "data_slice"
size_bytes = 20512
sha256_encrypted = "8888888888888888888888888888888888888888888888888888888888888888"

[[files]]
position = 9
type = "data_slice"
size_bytes = 2300
sha256_encrypted = "9999999999999999999999999999999999999999999999999999999999999999"

[[files]]
position = 10
type = "seal_marker"
"##;

#[test]
fn layout3_front_index() {
    assert_pinned_text(
        "front-index",
        &generate_front_index("L8-0001", &eleven_file_plan()),
        FRONT_INDEX,
    );
}
