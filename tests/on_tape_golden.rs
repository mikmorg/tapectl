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
//! text shows the format in the test; a failure writes the generated text to a file under the
//! target directory and names that file. Every Layout 3 pin so far holds its whole text.
//! Pinned here: File 0, the MANIFEST (a whole unit, a range block, the operator envelope), the
//! front index, the seal marker (full and short), the file table's text and the id map's
//! text, File 1 (the guide of an LTO-8 tape and of an LTO-6 tape: it differs by generation)
//! and RECOVERY.md (a tenant's envelope, with every compression in it, and the operator's,
//! with the `catalog.db` section `build()` appends to it), and RESTORE.sh by its sha256.

use tapectl::idmap::{self, IdKind, IdRow};
use tapectl::volume::filetable::{self, Kind, Row};
use tapectl::volume::layout::{
    generate_front_index, generate_id_thunk_v2, generate_manifest_toml_at, generate_recovery_md_at,
    generate_seal_marker_requiring_at, generate_short_seal_marker_requiring_at,
    generate_system_guide_v2_for_media, FrontIndexFile, IdThunkV2Params, REQUIRES,
};
use tapectl::volume::manifest::{ManifestSlice, ManifestUnit};

/// Compare a generated document with its pinned text. On a difference the generated text
/// is written under the target directory and the failure names the file.
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
/// The input is fixed; `generate_manifest_toml_at` is `generate_manifest_toml` (the call
/// `build()` makes) with `created_at` given.
#[test]
fn layout3_manifest_whole_units() {
    let units = vec![
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
    ];
    let text = generate_manifest_toml_at("L8-0001", "alpha", &units, "2026-11-21T10:00:00+00:00");
    assert_pinned_text("manifest-whole-units", &text, MANIFEST_WHOLE_UNITS);
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
    let units = vec![ManifestUnit {
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
    }];
    let text = generate_manifest_toml_at("L8-0002", "alpha", &units, "2026-11-21T10:00:00+00:00");
    assert_pinned_text("manifest-range-block", &text, MANIFEST_RANGE_BLOCK);
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
    let units = vec![
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
    ];
    let text =
        generate_manifest_toml_at("L8-0001", "operator", &units, "2026-11-21T10:00:00+00:00");
    assert_pinned_text(
        "manifest-operator-envelope",
        &text,
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

/// The plan's entries as the seal's embedded copy has them (section 5): File 3's own entry
/// is filled in with its size and hash (the seal is written after File 3 is known); the
/// seal marker's own entry stays bare.
fn seal_copy(through: i32) -> Vec<FrontIndexFile> {
    eleven_file_plan()
        .into_iter()
        .filter(|f| f.position <= through)
        .map(|mut f| {
            if f.position == 3 {
                f.size_bytes = Some(3_100);
                f.sha256_encrypted = Some("3".repeat(64));
            }
            f
        })
        .collect()
}

const SEAL_MARKER_FULL: &str = r##"================================================================
                    TAPECTL SEAL MARKER
================================================================

Volume: L8-0001
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
volume = "L8-0001"
layout_version = 3
requires = []
file_count = 11
ended_after = 9
sealed_at = "2026-11-21T10:05:00Z"
front_index_sha256 = "ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff"

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
size_bytes = 3100
sha256_encrypted = "3333333333333333333333333333333333333333333333333333333333333333"

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

/// A full seal (volume-format-v3.md section 5): `ended_after` is the last data slice, and
/// the copy runs to the seal's own bare entry. `sealed_at` is passed in
/// (`generate_seal_marker_requiring_at`, the seam the clock-reading generator calls).
#[test]
fn layout3_seal_marker_full() {
    let text = generate_seal_marker_requiring_at(
        "L8-0001",
        11,
        &"f".repeat(64),
        &seal_copy(10),
        REQUIRES,
        "2026-11-21T10:05:00Z",
    );
    assert_pinned_text("seal-marker-full", &text, SEAL_MARKER_FULL);
}

const SEAL_MARKER_SHORT: &str = r##"================================================================
                    TAPECTL SEAL MARKER
================================================================

Volume: L8-0001
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
volume = "L8-0001"
layout_version = 3
requires = []
file_count = 10
ended_after = 8
sealed_at = "2026-11-21T10:05:00Z"
front_index_sha256 = "ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff"

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
size_bytes = 3100
sha256_encrypted = "3333333333333333333333333333333333333333333333333333333333333333"

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
type = "seal_marker"
"##;

/// A short seal (section 5.2): the early warning came after data slice 8, so the seal is
/// at 9, `ended_after = 8`, `file_count = 10`; the copy runs to 8 and ends with the seal's
/// own bare entry at 9.
#[test]
fn layout3_seal_marker_short() {
    let text = generate_short_seal_marker_requiring_at(
        "L8-0001",
        8,
        &"f".repeat(64),
        &seal_copy(8),
        REQUIRES,
        "2026-11-21T10:05:00Z",
    );
    assert_pinned_text("seal-marker-short", &text, SEAL_MARKER_SHORT);
}

fn text_of(write: impl FnOnce(&mut Vec<u8>)) -> String {
    let mut out = Vec::new();
    write(&mut out);
    String::from_utf8(out).expect("the format's text is UTF-8")
}

fn member(
    kind: Kind,
    slice: u64,
    (start, end): (u64, u64),
    size: u64,
    hash_byte: Option<u8>,
    path: &str,
    link: Option<&str>,
) -> Row {
    Row {
        kind,
        slice,
        start,
        end,
        size,
        sha256: hash_byte.map(|b| [b; 32]),
        path: path.to_string(),
        link: link.map(str::to_string),
    }
}

/// Tab-separated text: each TAB is written `\t` here so no editor can change it.
const FILE_TABLE: &str = concat!(
    "#tapectl-file-table 1\n",
    "#stream_bytes 23040\n",
    "d\t1\t0\t512\t0\t-\t.\t\n",
    "f\t1\t512\t2048\t1000\taaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\ta.txt\t\n",
    "d\t1\t2048\t2560\t0\t-\tsub\t\n",
    "f\t1\t2560\t4096\t1024\tbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb\tsub/a\\tb\t\n",
    "f\t2\t4096\t10240\t5632\tcccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc\tsub/long.bin\t\n",
    "h\t3\t10240\t10752\t1000\taaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\tsub/again.txt\ta.txt\n",
    "l\t3\t10752\t11264\t0\t-\tsub/ln\t\n",
    "p\t3\t11264\t11776\t0\t-\tpipe\t\n",
    "c\t3\t11776\t12288\t0\t-\tdev/null\t\n",
    "b\t3\t12288\t12800\t0\t-\tdev/loop0\t\n",
);

/// A staging's file table (volume-format-v3.md section 1.6) with one row of every kind a
/// stream holds: the unit's root `.`, folders, a file whose name holds a TAB (written
/// escaped, `\t`), a hard link (its target's size and hash, the link target in the eighth
/// field), a symbolic link, a FIFO and both devices; members in slices 1 to 3, one of them
/// a long file whose run holds slice 2. Every row has eight fields.
#[test]
fn layout3_file_table_text() {
    let rows = vec![
        member(Kind::Folder, 1, (0, 512), 0, None, ".", None),
        member(
            Kind::File,
            1,
            (512, 2_048),
            1_000,
            Some(0xaa),
            "a.txt",
            None,
        ),
        member(Kind::Folder, 1, (2_048, 2_560), 0, None, "sub", None),
        member(
            Kind::File,
            1,
            (2_560, 4_096),
            1_024,
            Some(0xbb),
            "sub/a\\tb",
            None,
        ),
        member(
            Kind::File,
            2,
            (4_096, 10_240),
            5_632,
            Some(0xcc),
            "sub/long.bin",
            None,
        ),
        member(
            Kind::HardLink,
            3,
            (10_240, 10_752),
            1_000,
            Some(0xaa),
            "sub/again.txt",
            Some("a.txt"),
        ),
        member(Kind::Symlink, 3, (10_752, 11_264), 0, None, "sub/ln", None),
        member(Kind::Fifo, 3, (11_264, 11_776), 0, None, "pipe", None),
        member(
            Kind::CharDevice,
            3,
            (11_776, 12_288),
            0,
            None,
            "dev/null",
            None,
        ),
        member(
            Kind::BlockDevice,
            3,
            (12_288, 12_800),
            0,
            None,
            "dev/loop0",
            None,
        ),
    ];
    let text = text_of(|w| {
        filetable::write(w, 12_800 + 10_240, rows.into_iter().map(Ok)).unwrap();
    });
    assert_pinned_text("file-table", &text, FILE_TABLE);
}

/// Tab-separated text: each TAB is written `\t` here so no editor can change it.
const ID_MAP: &str = concat!(
    "#tapectl-id-map 1\n",
    "u\t0\troot\n",
    "u\t1000\tmike\n",
    "u\t4242\t\n",
    "u\t65534\ta\\tb\n",
    "g\t0\troot\n",
    "g\t100\tusers\n",
    "g\t4242\t\n",
);

/// A staging's id map (volume-format-v3.md section 1.6): users before groups, each by
/// number; a name the host did not know written as an empty field; a name holding a TAB
/// written escaped (`\t`).
#[test]
fn layout3_id_map_text() {
    let row = |kind, id, name: Option<&str>| IdRow {
        kind,
        id,
        name: name.map(str::to_string),
    };
    let rows = vec![
        row(IdKind::User, 0, Some("root")),
        row(IdKind::User, 1000, Some("mike")),
        row(IdKind::User, 4242, None),
        row(IdKind::User, 65534, Some("a\\tb")),
        row(IdKind::Group, 0, Some("root")),
        row(IdKind::Group, 100, Some("users")),
        row(IdKind::Group, 4242, None),
    ];
    let text = text_of(|w| idmap::write(w, &rows).unwrap());
    assert_pinned_text("id-map", &text, ID_MAP);
}

const FILE_1_LTO8: &str = r####"# tapectl Archival Volume Recovery Guide

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

## Volume: L8-0001

This document describes how to recover data from this tape without
tapectl or its database. All you need is: mt, dd, age, sha256sum, head,
tar, and tail.

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
- LAST file (File 8 on this tape): SEAL MARKER — asserts the tape
  is complete, and carries a full embedded copy of the front index (used
  if File 3 is damaged). A tape that ended early has its seal marker
  sooner, right after the last data slice written ("Where the tape ended")

The number of the last file is a fact about THIS tape. Another tape of the
box has its own: File 0 of that tape gives it (`total_files`, and
`seal_marker`, the number of its last file).

## The tape drive

This tape is LTO-8 (File 0 says so: media_type). A drive reads it only
if it is an LTO-8 or LTO-9 drive: a newer drive often cannot read an
older tape (an LTO-8 drive, for example, reads only LTO-7, LTO-7 Type M
and LTO-8). That is about THIS tape: another tape of the box may be of
another generation (its own File 0 says), and the table below shows
which drives read which.

You can learn a cartridge's generation without a drive, and before this
guide can be read: it is printed on the cartridge (the LTO logo and
number on the case, and the last two characters of the barcode label,
where L6 is LTO-6), and the Heir Kit's cover sheet lists the generations
of the tapes the Heir Kit's catalogue knew when it was made, with the
drives that read each, in lines like "LTO-8 (2 volume(s)): an LTO-8 or
LTO-9 drive". A sheet made when the Heir Kit's catalogue listed no
sealed cartridge says so and sends you to the label.

A box can hold cartridges of several generations, and one drive may not
read them all. This is which drive generation reads which cartridge:

  Cartridge       Read by a drive of
  LTO-5           LTO-5, LTO-6 or LTO-7
  LTO-6           LTO-6 or LTO-7
  LTO-7           LTO-7 or LTO-8
  LTO-7 Type M    LTO-8
  LTO-8           LTO-8 or LTO-9
  LTO-9           LTO-9
  LTO-10          LTO-10

No drive reads every generation. An LTO-7 drive reads LTO-5, LTO-6 and
LTO-7 cartridges, an LTO-8 drive reads LTO-7, LTO-7 Type M and LTO-8
cartridges, and a box that holds both an LTO-6 and an LTO-8 cartridge
needs two drives, one after the other. Pick the single drive that reads
the most of the cartridges you hold, and read those first, starting with
the newest tape it reads: if the newest tape of the box is not one of
them, "The rest of the box" says what that costs. A generation not in
the table: the maker's compatibility table.

The drive connects by SAS (some models by Fibre Channel), so the
computer needs a matching host adapter (HBA); there is no USB or SATA
tape drive. Fit the pieces together before the day you need them:

- SAS comes in generations (3, 6 and 12 Gb/s). A link runs at the
  highest speed both ends support, so an adapter and a drive of
  different generations still work together, at the slower speed.
- The cable must fit both ends, and the connectors differ: external
  drives commonly use SFF-8088 or the newer SFF-8644, and an adapter
  cable joins one to the other. An internal drive commonly has the SAS
  drive connector (SFF-8482, the one on a SAS disk) and is joined to the
  adapter's internal port, commonly SFF-8087 or the newer SFF-8643, by a
  cable that ends in drive connectors; whether the drive takes its power
  through that connector or from a plug of its own depends on the model.
  The maker's data sheets for the drive and for the adapter name their
  connectors, and they, not this text, decide.
- A Fibre Channel drive needs a Fibre Channel host adapter (a SAS
  adapter cannot drive it), a fibre-optic cable with the connectors both
  ends take (commonly LC), and a link speed both ends support. With a
  Fibre Channel switch in between, the switch must let the computer see
  the drive (it is "zoned" to it). Linux drives such adapters with the
  modules `qla2xxx` (QLogic) and `lpfc` (Emulex); once the link is up
  `lsscsi -g` lists the drive like any other.
- Use a plain host adapter (HBA), not a RAID controller. RAID firmware
  often does not present a tape drive to the operating system at all,
  and many on-board SAS ports are RAID controllers.
- A PCIe SAS adapter in a desktop or server is the dependable way. Do
  not use a USB bridge: SAS-to-USB bridges for tape drives are rare and
  not recommended, because a tape drive needs a steady stream of data
  and an operating system that sees it as an ordinary SCSI tape device
  (/dev/nst0 and the like). A computer with no free PCIe slot, a laptop
  say, can use a Thunderbolt PCIe enclosure that holds a SAS adapter:
  the operating system then sees an ordinary adapter, and that can work,
  but it is the less certain way, so check that the drive shows up
  (below) before you rely on it.
- Check that the drive shows up (`ls -l /dev/tape/by-id/`, see "Which
  device?") before you load the tape.

A setup that works, as one example: an LTO-8 or LTO-9 drive with a SAS
interface, a PCIe SAS host adapter in a desktop computer with a free
PCIe slot, and the cable and power to match. For an external drive,
which has its own power supply: an adapter with an external port (an LSI
or Broadcom 9200-8e, which has SFF-8088 ports, or a 9300-8e, which has
SFF-8644, on IT firmware) and an external SAS cable whose ends fit the
drive and the adapter. For a bare internal drive, in a 5.25-inch bay: an
adapter with an internal port (a 9211-8i, SFF-8087, or a 9300-8i,
SFF-8643), a cable from that port that ends in an SFF-8482 drive
connector, and the drive's power from the computer's power supply,
through that connector or from a separate lead (often a 4-pin Molex
plug), as the drive's data sheet says. These models are examples, not a
recommendation: the connectors are the standards that last, and the data
sheets decide.

A data-recovery firm with a drive can read the tape. It needs no key to
copy it: every file's raw bytes can be read, and only their meaning is
encrypted. Whoever holds the escrow key, or the operator key, can read
everything on every tape, for every tenant, so think before handing
either to a firm. To use a firm without giving it a key, ask for the raw
content of each tape file (File 0 to the last, in the 512 KiB blocks of
"Manual Recovery Steps") and decrypt it yourself; a tenant's key gives a
firm that tenant's data and no one else's.

## Loading the tape, and a drive that does not show up

- Read the generation printed on the cartridge (L6 on its label is
  LTO-6) before you buy or borrow a drive, and look at every cartridge
  in the box: they may be different generations, and a drive that reads
  one may not read another. This guide is on the tape, which you cannot
  read without a drive, so take the generation from the cartridge or the
  Heir Kit's cover sheet ("The tape drive").
- An external drive, in its own case with its own power supply, is the
  simplest: it needs only an adapter with an external port and an
  external SAS cable that fits both ends. A bare internal drive needs
  more: a place to sit (a 5.25-inch bay in a desktop or server, or an
  enclosure made for it), power from a power supply, and the internal
  SAS cable that matches the adapter's connector. A PCIe SAS adapter
  needs a free PCIe slot, so a laptop or a small PC without one cannot
  use it: borrow a desktop or a server, or use a Thunderbolt PCIe
  enclosure ("The tape drive").
- LSI (now Broadcom) host adapters come with one of two firmwares: "IT"
  (initiator-target: the card hands every device to the operating system
  untouched) and "IR" (integrated RAID: the card can also build simple
  RAID sets from disks). IT is the one to want for a tape drive, and
  many adapters sold as plain host adapters already run it, so a card
  that says (IT) needs nothing. Change nothing unless the drive does not
  appear. If it does not, and the card runs RAID firmware (IR, or a
  MegaRAID controller), IT firmware can cure it where the maker offers
  one for that card (not every RAID controller has one: then use a plain
  adapter instead); flash it only with the maker's own tool and
  instructions, because a flash that fails can leave a card unusable. To
  see what a card is: `lspci -k` names the kernel driver in use under
  each adapter. `mpt3sas` (`mpt2sas` on older kernels) is the driver of
  LSI and Broadcom host adapters, and `megaraid_sas` is the driver of
  their RAID controllers, which are the wrong kind of card. A card on
  `mpt3sas` can still carry RAID ("IR") firmware: the maker's flash tool
  says which (`sas3flash -list`, or `sas2flash -list` on older cards,
  prints a Firmware Product ID tagged (IT) or (IR)).
- Slide the write-protect tab on the cartridge to the protected position
  before you load it. Reading needs no writing, and RESTORE.sh never
  writes to the tape, so nothing can be lost by mistake.
- Load the cartridge, then wait until the drive is ready: its lights
  settle and `mt -f /dev/nst0 status` says ONLINE (DR_OPEN, or "no
  medium", means it is not ready or empty). A cartridge takes a minute
  or more to thread. To unload, `mt -f /dev/nst0 offline` rewinds the
  tape and ejects it; wait for the drive to finish before you take it
  out.
- A drive that reads some cartridges and not others, or shows a cleaning
  light, may need a cleaning cartridge: load it, and the drive cleans
  and ejects it by itself.
- The drive is not in `ls -l /dev/tape/by-id/`: see whether the system
  sees it at all (`lsscsi -g` if installed, and `dmesg | tail` for the
  kernel's messages about it). The kernel's tape driver is the module
  `st` (`lsmod | grep '^st'`, `sudo modprobe st` loads it). The device
  files belong to the group `tape`: `ls -l /dev/nst0` says, and a user
  who is not in it gets "Permission denied" (`sudo usermod -aG tape
  YOUR_USER`, then log in again). Running the commands with `sudo` works
  too, but sudo hides your `TAPE_DEVICE` and `TMPDIR` from the script,
  which then reads `/dev/nst0` and `/tmp` whatever you set, and
  `--preserve-env` cannot keep `TMPDIR` (the system's dynamic loader
  removes it from a set-user-ID program such as sudo before sudo starts:
  ld.so(8)). Give both after sudo, with `env`: `sudo env
  TAPE_DEVICE=/dev/nst0 TMPDIR=/tmp ./RESTORE.sh --info`. (`sudo
  --preserve-env=TAPE_DEVICE` keeps the device alone; sudo 1.8.21 or
  later has it, and Debian 10 has 1.8.27.) What the script writes is
  then owned by root.

## Tools Required

- `mt` (mt-st package) — tape positioning and setting the block size.
  The plain `mt` that GNU cpio installs cannot set it: RESTORE.sh warns,
  then reads in whatever mode the drive is in
- `dd` — reading raw data from tape (coreutils)
- `age` — decryption (age-encryption.org; package age on Debian 11 and
  later and on Ubuntu 22.04 and later, none on Debian 10, where a
  release from github.com/FiloSottile/age/releases does). The encrypted
  files are in the age v1 format, which is stable and which the `age`
  program and `rage` both read; any age from 1.0.0-rc.1 (March 2021) on
  opens them, the oldest tried. RESTORE.sh runs a program named `age`,
  so with only `rage` link it under that name
- `sha256sum` — integrity verification (coreutils)
- `head` — trimming and hashing exact byte counts (coreutils)
- `tar` — unpacking your decrypted envelope, and restoring a unit: the
  stream a unit is stored as is a tar archive (package tar)
- `tail` — cutting one file's bytes out of a slice, for a restore of
  named paths and for the repair (coreutils)
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
and names any that is missing: `--info` and `--verify` need only `mt`, `dd`, `sha256sum` and `head`.

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

The tape's capacity bounds what is stored on it, not what a restore
writes: this tape is LTO-8, which holds 12 TB of data as the makers
count it (decimal, uncompressed), so everything stored on it takes at
most that on the tape. A unit stored uncompressed restores to about that
size; a unit stored compressed restores larger, and the capacity is no
ceiling for it ("Compressed units", below).

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
4. Read the seal marker, the last file. This tape has 9 files,
   numbered 0 to 8, so the last is File 8, unless the
   tape ended early (below):

       mt -f /dev/nst0 eod
       mt -f /dev/nst0 bsfm 2
       dd if=/dev/nst0 bs=512k | tr -d '\0' > seal.toml

   `eod` (GNU mt calls it `eom`) moves to the end of the recorded data,
   which is just past the last file's closing filemark. `bsfm 2` backs over
   that filemark and the one before it and then steps forward over that
   one, which leaves the head at the START of the last file. (Without
   `eod`, `mt -f /dev/nst0 rewind` then `mt -f /dev/nst0 fsf 8`
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

       awk -F'\t' '!/^#/ && ($1 == "f" || $1 == "h") && index($7, "\\") == 0 { print $6 "  ./" $7 }' env/tables/TABLE.tsv | (cd /destination && sha256sum -c --quiet)

   Who the user and group numbers were, for the unit (the id map; it is
   shown and never used):

       cat env/tables/TABLE.ids.tsv

**One file, not the whole unit.** Find its row in the unit's file table (the
columns are `kind`, `slice`, `start`, `end`, `size`, `sha256` and `path`, the
path as the table writes it, with `./` left off):

    awk -F'\t' '$7 == "SOME_PATH"' env/tables/TABLE.tsv

then the stream offset where its slice begins, which is the `start` of the
first row whose `slice` is that slice's number (SLICE is the row's `slice`):

    awk -F'\t' -v s=SLICE '!/^#/ && $2 == s { print $3; exit }' env/tables/TABLE.tsv     # SLICE_START

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
       keep=$(tail -c 524288 candidate.raw | od -An -v -tu1 | awk '{ for (i = 1; i <= NF; i++) { n++; if ($i != 0) last = n } } END { print last + 0 }')
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


## Total files on this tape: 9

The files of THIS tape are numbered 0 to 8. File 8, the
last, is the seal marker. Another tape of the box has its own count: File 0
of that tape gives it. That is the plan: a tape that ended early holds
fewer, and its seal marker says where it ended ("Where the tape ended").

"####;

/// File 1, the system guide, of an LTO-8 tape of nine files, as `build()` makes it
/// (`generate_system_guide_v2_for_media(label, total_files, media_type)`). It reads no
/// clock. The drives that read the generation and the capacity sentence come from the
/// media type, so an LTO-6 tape's guide is pinned beside it.
#[test]
fn layout3_file_1_system_guide_lto8() {
    let text = generate_system_guide_v2_for_media("L8-0001", 9, "LTO-8");
    assert_pinned_text("file-1-lto8", &text, FILE_1_LTO8);
}

const FILE_1_LTO6: &str = r####"# tapectl Archival Volume Recovery Guide

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

## Volume: L6-0001

This document describes how to recover data from this tape without
tapectl or its database. All you need is: mt, dd, age, sha256sum, head,
tar, and tail.

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
- LAST file (File 8 on this tape): SEAL MARKER — asserts the tape
  is complete, and carries a full embedded copy of the front index (used
  if File 3 is damaged). A tape that ended early has its seal marker
  sooner, right after the last data slice written ("Where the tape ended")

The number of the last file is a fact about THIS tape. Another tape of the
box has its own: File 0 of that tape gives it (`total_files`, and
`seal_marker`, the number of its last file).

## The tape drive

This tape is LTO-6 (File 0 says so: media_type). A drive reads it only
if it is an LTO-6 or LTO-7 drive: a newer drive often cannot read an
older tape (an LTO-8 drive, for example, reads only LTO-7, LTO-7 Type M
and LTO-8). That is about THIS tape: another tape of the box may be of
another generation (its own File 0 says), and the table below shows
which drives read which.

You can learn a cartridge's generation without a drive, and before this
guide can be read: it is printed on the cartridge (the LTO logo and
number on the case, and the last two characters of the barcode label,
where L6 is LTO-6), and the Heir Kit's cover sheet lists the generations
of the tapes the Heir Kit's catalogue knew when it was made, with the
drives that read each, in lines like "LTO-6 (2 volume(s)): an LTO-6 or
LTO-7 drive". A sheet made when the Heir Kit's catalogue listed no
sealed cartridge says so and sends you to the label.

A box can hold cartridges of several generations, and one drive may not
read them all. This is which drive generation reads which cartridge:

  Cartridge       Read by a drive of
  LTO-5           LTO-5, LTO-6 or LTO-7
  LTO-6           LTO-6 or LTO-7
  LTO-7           LTO-7 or LTO-8
  LTO-7 Type M    LTO-8
  LTO-8           LTO-8 or LTO-9
  LTO-9           LTO-9
  LTO-10          LTO-10

No drive reads every generation. An LTO-7 drive reads LTO-5, LTO-6 and
LTO-7 cartridges, an LTO-8 drive reads LTO-7, LTO-7 Type M and LTO-8
cartridges, and a box that holds both an LTO-6 and an LTO-8 cartridge
needs two drives, one after the other. Pick the single drive that reads
the most of the cartridges you hold, and read those first, starting with
the newest tape it reads: if the newest tape of the box is not one of
them, "The rest of the box" says what that costs. A generation not in
the table: the maker's compatibility table.

The drive connects by SAS (some models by Fibre Channel), so the
computer needs a matching host adapter (HBA); there is no USB or SATA
tape drive. Fit the pieces together before the day you need them:

- SAS comes in generations (3, 6 and 12 Gb/s). A link runs at the
  highest speed both ends support, so an adapter and a drive of
  different generations still work together, at the slower speed.
- The cable must fit both ends, and the connectors differ: external
  drives commonly use SFF-8088 or the newer SFF-8644, and an adapter
  cable joins one to the other. An internal drive commonly has the SAS
  drive connector (SFF-8482, the one on a SAS disk) and is joined to the
  adapter's internal port, commonly SFF-8087 or the newer SFF-8643, by a
  cable that ends in drive connectors; whether the drive takes its power
  through that connector or from a plug of its own depends on the model.
  The maker's data sheets for the drive and for the adapter name their
  connectors, and they, not this text, decide.
- A Fibre Channel drive needs a Fibre Channel host adapter (a SAS
  adapter cannot drive it), a fibre-optic cable with the connectors both
  ends take (commonly LC), and a link speed both ends support. With a
  Fibre Channel switch in between, the switch must let the computer see
  the drive (it is "zoned" to it). Linux drives such adapters with the
  modules `qla2xxx` (QLogic) and `lpfc` (Emulex); once the link is up
  `lsscsi -g` lists the drive like any other.
- Use a plain host adapter (HBA), not a RAID controller. RAID firmware
  often does not present a tape drive to the operating system at all,
  and many on-board SAS ports are RAID controllers.
- A PCIe SAS adapter in a desktop or server is the dependable way. Do
  not use a USB bridge: SAS-to-USB bridges for tape drives are rare and
  not recommended, because a tape drive needs a steady stream of data
  and an operating system that sees it as an ordinary SCSI tape device
  (/dev/nst0 and the like). A computer with no free PCIe slot, a laptop
  say, can use a Thunderbolt PCIe enclosure that holds a SAS adapter:
  the operating system then sees an ordinary adapter, and that can work,
  but it is the less certain way, so check that the drive shows up
  (below) before you rely on it.
- Check that the drive shows up (`ls -l /dev/tape/by-id/`, see "Which
  device?") before you load the tape.

A setup that works, as one example: an LTO-6 or LTO-7 drive with a SAS
interface, a PCIe SAS host adapter in a desktop computer with a free
PCIe slot, and the cable and power to match. For an external drive,
which has its own power supply: an adapter with an external port (an LSI
or Broadcom 9200-8e, which has SFF-8088 ports, or a 9300-8e, which has
SFF-8644, on IT firmware) and an external SAS cable whose ends fit the
drive and the adapter. For a bare internal drive, in a 5.25-inch bay: an
adapter with an internal port (a 9211-8i, SFF-8087, or a 9300-8i,
SFF-8643), a cable from that port that ends in an SFF-8482 drive
connector, and the drive's power from the computer's power supply,
through that connector or from a separate lead (often a 4-pin Molex
plug), as the drive's data sheet says. These models are examples, not a
recommendation: the connectors are the standards that last, and the data
sheets decide.

A data-recovery firm with a drive can read the tape. It needs no key to
copy it: every file's raw bytes can be read, and only their meaning is
encrypted. Whoever holds the escrow key, or the operator key, can read
everything on every tape, for every tenant, so think before handing
either to a firm. To use a firm without giving it a key, ask for the raw
content of each tape file (File 0 to the last, in the 512 KiB blocks of
"Manual Recovery Steps") and decrypt it yourself; a tenant's key gives a
firm that tenant's data and no one else's.

## Loading the tape, and a drive that does not show up

- Read the generation printed on the cartridge (L6 on its label is
  LTO-6) before you buy or borrow a drive, and look at every cartridge
  in the box: they may be different generations, and a drive that reads
  one may not read another. This guide is on the tape, which you cannot
  read without a drive, so take the generation from the cartridge or the
  Heir Kit's cover sheet ("The tape drive").
- An external drive, in its own case with its own power supply, is the
  simplest: it needs only an adapter with an external port and an
  external SAS cable that fits both ends. A bare internal drive needs
  more: a place to sit (a 5.25-inch bay in a desktop or server, or an
  enclosure made for it), power from a power supply, and the internal
  SAS cable that matches the adapter's connector. A PCIe SAS adapter
  needs a free PCIe slot, so a laptop or a small PC without one cannot
  use it: borrow a desktop or a server, or use a Thunderbolt PCIe
  enclosure ("The tape drive").
- LSI (now Broadcom) host adapters come with one of two firmwares: "IT"
  (initiator-target: the card hands every device to the operating system
  untouched) and "IR" (integrated RAID: the card can also build simple
  RAID sets from disks). IT is the one to want for a tape drive, and
  many adapters sold as plain host adapters already run it, so a card
  that says (IT) needs nothing. Change nothing unless the drive does not
  appear. If it does not, and the card runs RAID firmware (IR, or a
  MegaRAID controller), IT firmware can cure it where the maker offers
  one for that card (not every RAID controller has one: then use a plain
  adapter instead); flash it only with the maker's own tool and
  instructions, because a flash that fails can leave a card unusable. To
  see what a card is: `lspci -k` names the kernel driver in use under
  each adapter. `mpt3sas` (`mpt2sas` on older kernels) is the driver of
  LSI and Broadcom host adapters, and `megaraid_sas` is the driver of
  their RAID controllers, which are the wrong kind of card. A card on
  `mpt3sas` can still carry RAID ("IR") firmware: the maker's flash tool
  says which (`sas3flash -list`, or `sas2flash -list` on older cards,
  prints a Firmware Product ID tagged (IT) or (IR)).
- Slide the write-protect tab on the cartridge to the protected position
  before you load it. Reading needs no writing, and RESTORE.sh never
  writes to the tape, so nothing can be lost by mistake.
- Load the cartridge, then wait until the drive is ready: its lights
  settle and `mt -f /dev/nst0 status` says ONLINE (DR_OPEN, or "no
  medium", means it is not ready or empty). A cartridge takes a minute
  or more to thread. To unload, `mt -f /dev/nst0 offline` rewinds the
  tape and ejects it; wait for the drive to finish before you take it
  out.
- A drive that reads some cartridges and not others, or shows a cleaning
  light, may need a cleaning cartridge: load it, and the drive cleans
  and ejects it by itself.
- The drive is not in `ls -l /dev/tape/by-id/`: see whether the system
  sees it at all (`lsscsi -g` if installed, and `dmesg | tail` for the
  kernel's messages about it). The kernel's tape driver is the module
  `st` (`lsmod | grep '^st'`, `sudo modprobe st` loads it). The device
  files belong to the group `tape`: `ls -l /dev/nst0` says, and a user
  who is not in it gets "Permission denied" (`sudo usermod -aG tape
  YOUR_USER`, then log in again). Running the commands with `sudo` works
  too, but sudo hides your `TAPE_DEVICE` and `TMPDIR` from the script,
  which then reads `/dev/nst0` and `/tmp` whatever you set, and
  `--preserve-env` cannot keep `TMPDIR` (the system's dynamic loader
  removes it from a set-user-ID program such as sudo before sudo starts:
  ld.so(8)). Give both after sudo, with `env`: `sudo env
  TAPE_DEVICE=/dev/nst0 TMPDIR=/tmp ./RESTORE.sh --info`. (`sudo
  --preserve-env=TAPE_DEVICE` keeps the device alone; sudo 1.8.21 or
  later has it, and Debian 10 has 1.8.27.) What the script writes is
  then owned by root.

## Tools Required

- `mt` (mt-st package) — tape positioning and setting the block size.
  The plain `mt` that GNU cpio installs cannot set it: RESTORE.sh warns,
  then reads in whatever mode the drive is in
- `dd` — reading raw data from tape (coreutils)
- `age` — decryption (age-encryption.org; package age on Debian 11 and
  later and on Ubuntu 22.04 and later, none on Debian 10, where a
  release from github.com/FiloSottile/age/releases does). The encrypted
  files are in the age v1 format, which is stable and which the `age`
  program and `rage` both read; any age from 1.0.0-rc.1 (March 2021) on
  opens them, the oldest tried. RESTORE.sh runs a program named `age`,
  so with only `rage` link it under that name
- `sha256sum` — integrity verification (coreutils)
- `head` — trimming and hashing exact byte counts (coreutils)
- `tar` — unpacking your decrypted envelope, and restoring a unit: the
  stream a unit is stored as is a tar archive (package tar)
- `tail` — cutting one file's bytes out of a slice, for a restore of
  named paths and for the repair (coreutils)
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
and names any that is missing: `--info` and `--verify` need only `mt`, `dd`, `sha256sum` and `head`.

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

The tape's capacity bounds what is stored on it, not what a restore
writes: this tape is LTO-6, which holds 2.5 TB of data as the makers
count it (decimal, uncompressed), so everything stored on it takes at
most that on the tape. A unit stored uncompressed restores to about that
size; a unit stored compressed restores larger, and the capacity is no
ceiling for it ("Compressed units", below).

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
4. Read the seal marker, the last file. This tape has 9 files,
   numbered 0 to 8, so the last is File 8, unless the
   tape ended early (below):

       mt -f /dev/nst0 eod
       mt -f /dev/nst0 bsfm 2
       dd if=/dev/nst0 bs=512k | tr -d '\0' > seal.toml

   `eod` (GNU mt calls it `eom`) moves to the end of the recorded data,
   which is just past the last file's closing filemark. `bsfm 2` backs over
   that filemark and the one before it and then steps forward over that
   one, which leaves the head at the START of the last file. (Without
   `eod`, `mt -f /dev/nst0 rewind` then `mt -f /dev/nst0 fsf 8`
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

       awk -F'\t' '!/^#/ && ($1 == "f" || $1 == "h") && index($7, "\\") == 0 { print $6 "  ./" $7 }' env/tables/TABLE.tsv | (cd /destination && sha256sum -c --quiet)

   Who the user and group numbers were, for the unit (the id map; it is
   shown and never used):

       cat env/tables/TABLE.ids.tsv

**One file, not the whole unit.** Find its row in the unit's file table (the
columns are `kind`, `slice`, `start`, `end`, `size`, `sha256` and `path`, the
path as the table writes it, with `./` left off):

    awk -F'\t' '$7 == "SOME_PATH"' env/tables/TABLE.tsv

then the stream offset where its slice begins, which is the `start` of the
first row whose `slice` is that slice's number (SLICE is the row's `slice`):

    awk -F'\t' -v s=SLICE '!/^#/ && $2 == s { print $3; exit }' env/tables/TABLE.tsv     # SLICE_START

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
       keep=$(tail -c 524288 candidate.raw | od -An -v -tu1 | awk '{ for (i = 1; i <= NF; i++) { n++; if ($i != 0) last = n } } END { print last + 0 }')
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


## Total files on this tape: 9

The files of THIS tape are numbered 0 to 8. File 8, the
last, is the seal marker. Another tape of the box has its own count: File 0
of that tape gives it. That is the plan: a tape that ended early holds
fewer, and its seal marker says where it ended ("Where the tape ended").

"####;

/// File 1 of an LTO-6 tape of nine files. It differs from the LTO-8 guide in the drives
/// that read the generation (`guide_drive_section`) and the capacity sentence
/// (`guide_capacity`), so it is pinned whole beside it.
#[test]
fn layout3_file_1_system_guide_lto6() {
    let text = generate_system_guide_v2_for_media("L6-0001", 9, "LTO-6");
    assert_pinned_text("file-1-lto6", &text, FILE_1_LTO6);
}

const RECOVERY_TENANT: &str = r####"# Recovery Guide for alpha

Volume: L8-0001
Date: 2026-11-21

This tape holds age-encrypted pieces of tar archives: each unit is one tar
stream, cut into slices that begin where a file begins, and each slice is
encrypted on its own. With your age key(s) and the standard tools (`mt`,
`dd`, `truncate`, `age`, `tar`, `sha256sum`) you can recover your data by
hand — no tapectl required. The automated `RESTORE.sh` (tape file 2) does
the same for you, and checks every restored file against the unit's file
table; use it if you can.

The commands below are written for the directory this envelope was
unpacked in (`env/` in the guide's steps): run them from the directory
that holds `env/`. Each unit's block makes a work directory and a
destination of its own, so the blocks can be run one after another in
one place and never read each other's slices.

## Which key

The key that opened this envelope is
not necessarily the key that opens the slices below.
The envelope was sealed when the tape was written; each slice was sealed
earlier, when it was staged. If the keys were rotated in between, the
slices need an older key. `age` accepts several keys at once and uses
whichever one matches, so pass every key you hold:

```bash
age -d -i OLD.age.key -i NEW.age.key slice.1.age > slice.1.tar
```

Name only key files that exist:
one unreadable `-i` path fails the whole command.
If every key fails with `no identity matched any of the recipients` (`rage`:
`No matching keys found`), the
slice was sealed to a key you have not supplied — the data is not corrupt.
`age-keygen -y YOUR_KEY.age.key` shows whether a file is a key at all (it
prints that one key's public `age1...` line, or an error), and the guide on
this tape (File 1, "Which file is your key") tells a bad key file from a
tape of another installation.

## Units in this envelope

| Unit | Snapshot | Slices | Tape files |
|------|----------|--------|------------|
| photos/2019 | v7 | 2 | 9..10 |
| ledgers/fy24 | v1 | 2 | 11..12 |
| video/2020 | v3 | 1 of 3 | 13..13 |
| docs/letters | v2 | 1 | 14..14 |

## Where this tape ended

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

## A unit across tapes

These units continue on another cartridge (a slice of each has no tape
position in MANIFEST.toml). Each block lists every slice of the unit, and
this tape's plan holds the slices given a tape position:

| Unit | Snapshot | Slices on this tape | Tape files | Slices in all |
|------|----------|---------------------|------------|---------------|
| video/2020 | v3 | 3 to 3 | 13..13 | 3 |

The slices with no tape position, and any after the seal marker's
`ended_after` ("Where this tape ended", above), are on other tapes. A slice
holds whole files, so this tape alone restores every file whose slices are
all on it: the unit's block below reads the slices this tape holds, tar
restores those files, and the check after it names the others. A restore of
the whole unit needs every slice, 1 to the last, in slice order, from the
tapes that hold them, each taken from a tape whose front index (File 3)
lists the slice's `sha256_encrypted` (MANIFEST.toml gives every slice's)
and by nothing else. `RESTORE.sh` (tape file 2) does it, starting on any
tape that holds some of the unit: it asks for the next cartridge at a
prompt (ENTER after you load it, `done` or the end of the input for "I do
not have it", `quit` to stop), and `RESTORE.sh --where UNIT` on the newest
tape names the tapes that hold the rest.

## photos/2019

UUID: `11111111-2222-3333-4444-555555555555`  ·  snapshot v7  ·  40960 bytes of tar stream

This envelope holds the unit's file table, `tables/11111111_v7_s42.tsv` (every file's kind,
slice, place in the stream, size, sha256 and path), and its id map,
`tables/11111111_v7_s42.ids.tsv` (who each user and group number was on the computer that wrote
the tape; shown, never used). A restore here is judged by the table.

Put the drive in fixed 512KB block mode, then read, trim, verify and
decrypt each slice, in order: the tape is wound to the first slice
once, and each read that finishes leaves it at the start of the next
file. A `dd` that stops with an error ("Input/output error") leaves
the head inside that file instead: `mt rewind`, then `mt fsf` to the
tape file of the slice you want next, which its `# Slice` line gives.
`/dev/nst0` below is an example — run `ls -l /dev/tape/by-id/` and
substitute your own drive, or you may read a different tape. The block
makes `work/` and a directory for this unit and removes it when the unit is
restored: it needs room for the whole unit's slices (its stream size) once,
and the destination needs the same again.

```bash
mkdir -p work/11111111_v7_s42 /destination/11111111_v7_s42
mt -f /dev/nst0 setblk 524288

# On each `age -d` line, add `-i <file>` for every other key you hold
# (see "Which key" above).

# Slice 1 — tape file 9, 20512 bytes
mt -f /dev/nst0 rewind && mt -f /dev/nst0 fsf 9
dd if=/dev/nst0 bs=512k of=work/11111111_v7_s42/slice.1.age
truncate -s 20512 work/11111111_v7_s42/slice.1.age
echo "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb  work/11111111_v7_s42/slice.1.age" | sha256sum -c - && age -d -i YOUR_KEY.age.key work/11111111_v7_s42/slice.1.age > work/11111111_v7_s42/slice.1.tar && rm work/11111111_v7_s42/slice.1.age
test $(wc -c < work/11111111_v7_s42/slice.1.tar) -eq 20480 && echo "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa  work/11111111_v7_s42/slice.1.tar" | sha256sum -c -

# Slice 2 — tape file 10, 20512 bytes
dd if=/dev/nst0 bs=512k of=work/11111111_v7_s42/slice.2.age
truncate -s 20512 work/11111111_v7_s42/slice.2.age
echo "dddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddd  work/11111111_v7_s42/slice.2.age" | sha256sum -c - && age -d -i YOUR_KEY.age.key work/11111111_v7_s42/slice.2.age > work/11111111_v7_s42/slice.2.tar && rm work/11111111_v7_s42/slice.2.age
test $(wc -c < work/11111111_v7_s42/slice.2.tar) -eq 20480 && echo "cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc  work/11111111_v7_s42/slice.2.tar" | sha256sum -c -

# The slices, joined in order, are the unit's tar stream. One tar run, into the
# directory made above (numbers, not names, for owner and group; with another
# tar than GNU tar, leave out the flags it does not know):
cat work/11111111_v7_s42/slice.1.tar work/11111111_v7_s42/slice.2.tar | LC_ALL=C tar -x --numeric-owner -p --xattrs --acls -f - -C /destination/11111111_v7_s42
rm -r work/11111111_v7_s42
```

Check the unit against its file table: this prints nothing when every file
passes and `FAILED` for each that does not (a file whose name holds a
backslash, TAB, newline or carriage return is written escaped in the table
and is left out of this line; `RESTORE.sh` checks it):

```bash
awk -F'\t' '!/^#/ && ($1 == "f" || $1 == "h") && index($7, "\\") == 0 { print $6 "  ./" $7 }' env/tables/11111111_v7_s42.tsv | (cd /destination/11111111_v7_s42 && sha256sum -c --quiet)
```

## ledgers/fy24

UUID: `66666666-7777-8888-9999-000000000000`  ·  snapshot v1  ·  31744 bytes of tar stream

Each slice of this unit was compressed with zstd, after the cut and before
it was encrypted: the block below pipes each decrypted slice through
`zstd -dc` (package zstd). The length and the hash it checks are of the
decompressed slice.

This envelope holds the unit's file table, `tables/66666666_v1_s43.tsv` (every file's kind,
slice, place in the stream, size, sha256 and path), and its id map,
`tables/66666666_v1_s43.ids.tsv` (who each user and group number was on the computer that wrote
the tape; shown, never used). A restore here is judged by the table.

Put the drive in fixed 512KB block mode, then read, trim, verify and
decrypt each slice, in order: the tape is wound to the first slice
once, and each read that finishes leaves it at the start of the next
file. A `dd` that stops with an error ("Input/output error") leaves
the head inside that file instead: `mt rewind`, then `mt fsf` to the
tape file of the slice you want next, which its `# Slice` line gives.
`/dev/nst0` below is an example — run `ls -l /dev/tape/by-id/` and
substitute your own drive, or you may read a different tape. The block
makes `work/` and a directory for this unit and removes it when the unit is
restored: it needs room for the whole unit's slices (its stream size) once,
and the destination needs the same again.

```bash
mkdir -p work/66666666_v1_s43 /destination/66666666_v1_s43
mt -f /dev/nst0 setblk 524288

# On each `age -d` line, add `-i <file>` for every other key you hold
# (see "Which key" above).

# Slice 1 — tape file 11, 4100 bytes
mt -f /dev/nst0 rewind && mt -f /dev/nst0 fsf 11
dd if=/dev/nst0 bs=512k of=work/66666666_v1_s43/slice.1.age
truncate -s 4100 work/66666666_v1_s43/slice.1.age
echo "ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff  work/66666666_v1_s43/slice.1.age" | sha256sum -c - && age -d -i YOUR_KEY.age.key work/66666666_v1_s43/slice.1.age | zstd -dc > work/66666666_v1_s43/slice.1.tar && rm work/66666666_v1_s43/slice.1.age
test $(wc -c < work/66666666_v1_s43/slice.1.tar) -eq 20480 && echo "eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee  work/66666666_v1_s43/slice.1.tar" | sha256sum -c -

# Slice 2 — tape file 12, 2300 bytes (it continues the file begun in the slice before it)
dd if=/dev/nst0 bs=512k of=work/66666666_v1_s43/slice.2.age
truncate -s 2300 work/66666666_v1_s43/slice.2.age
echo "2222222222222222222222222222222222222222222222222222222222222222  work/66666666_v1_s43/slice.2.age" | sha256sum -c - && age -d -i YOUR_KEY.age.key work/66666666_v1_s43/slice.2.age | zstd -dc > work/66666666_v1_s43/slice.2.tar && rm work/66666666_v1_s43/slice.2.age
test $(wc -c < work/66666666_v1_s43/slice.2.tar) -eq 11264 && echo "1111111111111111111111111111111111111111111111111111111111111111  work/66666666_v1_s43/slice.2.tar" | sha256sum -c -

# The slices, joined in order, are the unit's tar stream. One tar run, into the
# directory made above (numbers, not names, for owner and group; with another
# tar than GNU tar, leave out the flags it does not know):
cat work/66666666_v1_s43/slice.1.tar work/66666666_v1_s43/slice.2.tar | LC_ALL=C tar -x --numeric-owner -p --xattrs --acls -f - -C /destination/66666666_v1_s43
rm -r work/66666666_v1_s43
```

Check the unit against its file table: this prints nothing when every file
passes and `FAILED` for each that does not (a file whose name holds a
backslash, TAB, newline or carriage return is written escaped in the table
and is left out of this line; `RESTORE.sh` checks it):

```bash
awk -F'\t' '!/^#/ && ($1 == "f" || $1 == "h") && index($7, "\\") == 0 { print $6 "  ./" $7 }' env/tables/66666666_v1_s43.tsv | (cd /destination/66666666_v1_s43 && sha256sum -c --quiet)
```

## video/2020

UUID: `aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee`  ·  snapshot v3  ·  61440 bytes of tar stream

Each slice of this unit was compressed with xz, after the cut and before
it was encrypted: the block below pipes each decrypted slice through
`xz -dc` (package xz-utils). The length and the hash it checks are of the
decompressed slice.

This envelope holds the unit's file table, `tables/aaaaaaaa_v3_s44.tsv` (every file's kind,
slice, place in the stream, size, sha256 and path), and its id map,
`tables/aaaaaaaa_v3_s44.ids.tsv` (who each user and group number was on the computer that wrote
the tape; shown, never used). A restore here is judged by the table.

This tape holds only some of this unit's slices (the others are on other
tapes: "Where this tape ended"). The block reads the ones it holds; tar
then restores every file whose slices are all here, and says "Unexpected
EOF in archive" for a file that runs on into a slice on another tape: that
file is not whole, and the check says so.

Put the drive in fixed 512KB block mode, then read, trim, verify and
decrypt each slice, in order: the tape is wound to the first slice
once, and each read that finishes leaves it at the start of the next
file. A `dd` that stops with an error ("Input/output error") leaves
the head inside that file instead: `mt rewind`, then `mt fsf` to the
tape file of the slice you want next, which its `# Slice` line gives.
`/dev/nst0` below is an example — run `ls -l /dev/tape/by-id/` and
substitute your own drive, or you may read a different tape. The block
makes `work/` and a directory for this unit and removes it when the unit is
restored: it needs room for the whole unit's slices (its stream size) once,
and the destination needs the same again.

```bash
mkdir -p work/aaaaaaaa_v3_s44 /destination/aaaaaaaa_v3_s44
mt -f /dev/nst0 setblk 524288

# On each `age -d` line, add `-i <file>` for every other key you hold
# (see "Which key" above).

# Slice 1 — on another tape (no tape position here): 9000 bytes,
# sha256_encrypted 4444444444444444444444444444444444444444444444444444444444444444

# Slice 2 — on another tape (no tape position here): 9001 bytes,
# sha256_encrypted 5555555555555555555555555555555555555555555555555555555555555555

# Slice 3 — tape file 13: it continues a file that begins on
# another tape, so it is not read here

# No slice of this unit is readable from this tape: see "Where this tape ended".
```

Check the unit against its file table: this prints nothing when every file
passes and `FAILED` for each that does not (a file whose name holds a
backslash, TAB, newline or carriage return is written escaped in the table
and is left out of this line; `RESTORE.sh` checks it):

```bash
awk -F'\t' '!/^#/ && ($1 == "f" || $1 == "h") && index($7, "\\") == 0 { print $6 "  ./" $7 }' env/tables/aaaaaaaa_v3_s44.tsv | (cd /destination/aaaaaaaa_v3_s44 && sha256sum -c --quiet)
```

## docs/letters

UUID: `99999999-8888-7777-6666-555555555555`  ·  snapshot v2  ·  10240 bytes of tar stream

Each slice of this unit was compressed with gzip, after the cut and before
it was encrypted: the block below pipes each decrypted slice through
`gzip -dc` (package gzip). The length and the hash it checks are of the
decompressed slice.

This envelope holds the unit's file table, `tables/99999999_v2_s45.tsv` (every file's kind,
slice, place in the stream, size, sha256 and path), and its id map,
`tables/99999999_v2_s45.ids.tsv` (who each user and group number was on the computer that wrote
the tape; shown, never used). A restore here is judged by the table.

Put the drive in fixed 512KB block mode, then read, trim, verify and
decrypt each slice, in order: the tape is wound to the first slice
once, and each read that finishes leaves it at the start of the next
file. A `dd` that stops with an error ("Input/output error") leaves
the head inside that file instead: `mt rewind`, then `mt fsf` to the
tape file of the slice you want next, which its `# Slice` line gives.
`/dev/nst0` below is an example — run `ls -l /dev/tape/by-id/` and
substitute your own drive, or you may read a different tape. The block
makes `work/` and a directory for this unit and removes it when the unit is
restored: it needs room for the whole unit's slices (its stream size) once,
and the destination needs the same again.

```bash
mkdir -p work/99999999_v2_s45 /destination/99999999_v2_s45
mt -f /dev/nst0 setblk 524288

# On each `age -d` line, add `-i <file>` for every other key you hold
# (see "Which key" above).

# Slice 1 — tape file 14, 3000 bytes
mt -f /dev/nst0 rewind && mt -f /dev/nst0 fsf 14
dd if=/dev/nst0 bs=512k of=work/99999999_v2_s45/slice.1.age
truncate -s 3000 work/99999999_v2_s45/slice.1.age
echo "8888888888888888888888888888888888888888888888888888888888888888  work/99999999_v2_s45/slice.1.age" | sha256sum -c - && age -d -i YOUR_KEY.age.key work/99999999_v2_s45/slice.1.age | gzip -dc > work/99999999_v2_s45/slice.1.tar && rm work/99999999_v2_s45/slice.1.age
test $(wc -c < work/99999999_v2_s45/slice.1.tar) -eq 10240 && echo "7777777777777777777777777777777777777777777777777777777777777777  work/99999999_v2_s45/slice.1.tar" | sha256sum -c -

# The slices, joined in order, are the unit's tar stream. One tar run, into the
# directory made above (numbers, not names, for owner and group; with another
# tar than GNU tar, leave out the flags it does not know):
cat work/99999999_v2_s45/slice.1.tar | LC_ALL=C tar -x --numeric-owner -p --xattrs --acls -f - -C /destination/99999999_v2_s45
rm -r work/99999999_v2_s45
```

Check the unit against its file table: this prints nothing when every file
passes and `FAILED` for each that does not (a file whose name holds a
backslash, TAB, newline or carriage return is written escaped in the table
and is left out of this line; `RESTORE.sh` checks it):

```bash
awk -F'\t' '!/^#/ && ($1 == "f" || $1 == "h") && index($7, "\\") == 0 { print $6 "  ./" $7 }' env/tables/99999999_v2_s45.tsv | (cd /destination/99999999_v2_s45 && sha256sum -c --quiet)
```

## The index of other tapes

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

## If a slice or a file is damaged

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

## Troubleshooting

- **age: "failed to decrypt and authenticate payload chunk"** (age 1.1.1) — the
file still has 512KB block padding, or a part of it is damaged. Re-run
`truncate -s <bytes>` to the exact `encrypted_bytes` shown above (a slice's
`size_bytes` in MANIFEST.toml is its PLAIN length: never trim to that); if the
sha256 of the trimmed file matches, the file is whole and the key is the
question, and if it does not, the slice is damaged.
- **age: "failed to read header: parsing age header"** — what was read is not
an age file: the wrong tape file, or the head was not where you thought.
`mt rewind`, `fsf` to the slice's tape file, and read it again.
- **tar: "Unexpected EOF in archive" or "This does not look like a tar
archive"** — the slices given to tar are not whole files in order: one is
missing, left out, or the first begins in the middle of a file. The check
against the file table names what is missing; tar's exit status never does.
- **sha256 mismatch** — re-read the slice from tape; a short read or the wrong
block mode (must be 512KB fixed) is the usual cause.
- **age: "no identity matched any of the recipients"** (`rage`: "No matching
keys found") — none of the keys
you passed is the one this slice was sealed to. It may be an older key from
before a rotation (see "Which key"), or the operator key, which can read
every unit; or the tape may be another installation's. Pass them all with
repeated `-i`.
"####;

/// RECOVERY.md of a tenant's envelope, as `build()` makes it (`generate_recovery_md` is
/// `generate_recovery_md_at` with the clock's day): four units with a file table and an id
/// map. A whole unit, a whole zstd unit whose last slice continues a member, a unit this tape
/// holds only the last slice of (a range block, xz), and a gzip unit: the text differs by
/// compression in the tool the block pipes each slice through, so each algorithm is here.
#[test]
fn layout3_recovery_md_tenant_envelope() {
    let unit = |name: &str, uuid: &str, version: i64, set: i64, stream: i64, comp: Option<&str>| {
        let short = &uuid[..8];
        ManifestUnit {
            name: name.to_string(),
            uuid: uuid.to_string(),
            snapshot_version: version,
            stage_set_id: set,
            payload: "tar".to_string(),
            stream_bytes: stream,
            file_table: Some(format!("tables/{short}_v{version}_s{set}.tsv")),
            id_map: Some(format!("tables/{short}_v{version}_s{set}.ids.tsv")),
            tar_version: Some("tar (GNU tar) 1.35".to_string()),
            compression: comp.map(str::to_string),
            slices: Vec::new(),
        }
    };
    let mut whole = unit(
        "photos/2019",
        "11111111-2222-3333-4444-555555555555",
        7,
        42,
        40_960,
        None,
    );
    whole.slices = vec![
        slice(1, Some(9), (20_480, 20_512), ("a", "b"), false),
        slice(2, Some(10), (20_480, 20_512), ("c", "d"), false),
    ];
    let mut zstd = unit(
        "ledgers/fy24",
        "66666666-7777-8888-9999-000000000000",
        1,
        43,
        31_744,
        Some("zstd"),
    );
    zstd.slices = vec![
        slice(1, Some(11), (20_480, 4_100), ("e", "f"), false),
        slice(2, Some(12), (11_264, 2_300), ("1", "2"), true),
    ];
    let mut range = unit(
        "video/2020",
        "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee",
        3,
        44,
        61_440,
        Some("xz"),
    );
    range.slices = vec![
        slice(1, None, (20_480, 9_000), ("1", "4"), false),
        slice(2, None, (20_480, 9_001), ("2", "5"), false),
        slice(3, Some(13), (20_480, 9_002), ("3", "6"), true),
    ];
    let mut gzip = unit(
        "docs/letters",
        "99999999-8888-7777-6666-555555555555",
        2,
        45,
        10_240,
        Some("gzip"),
    );
    gzip.slices = vec![slice(1, Some(14), (10_240, 3_000), ("7", "8"), false)];
    let units = vec![whole, zstd, range, gzip];
    let text = generate_recovery_md_at("L8-0001", "alpha", &units, &[], "2026-11-21");
    assert_pinned_text("recovery-tenant", &text, RECOVERY_TENANT);
}

const RECOVERY_OPERATOR: &str = r####"# Recovery Guide for operator

Volume: L8-0001
Date: 2026-11-21

This tape holds age-encrypted pieces of tar archives: each unit is one tar
stream, cut into slices that begin where a file begins, and each slice is
encrypted on its own. With your age key(s) and the standard tools (`mt`,
`dd`, `truncate`, `age`, `tar`, `sha256sum`) you can recover your data by
hand — no tapectl required. The automated `RESTORE.sh` (tape file 2) does
the same for you, and checks every restored file against the unit's file
table; use it if you can.

The commands below are written for the directory this envelope was
unpacked in (`env/` in the guide's steps): run them from the directory
that holds `env/`. Each unit's block makes a work directory and a
destination of its own, so the blocks can be run one after another in
one place and never read each other's slices.

## Which key

The key that opened this envelope is
not necessarily the key that opens the slices below.
The envelope was sealed when the tape was written; each slice was sealed
earlier, when it was staged. If the keys were rotated in between, the
slices need an older key. `age` accepts several keys at once and uses
whichever one matches, so pass every key you hold:

```bash
age -d -i OLD.age.key -i NEW.age.key slice.1.age > slice.1.tar
```

Name only key files that exist:
one unreadable `-i` path fails the whole command.
If every key fails with `no identity matched any of the recipients` (`rage`:
`No matching keys found`), the
slice was sealed to a key you have not supplied — the data is not corrupt.
`age-keygen -y YOUR_KEY.age.key` shows whether a file is a key at all (it
prints that one key's public `age1...` line, or an error), and the guide on
this tape (File 1, "Which file is your key") tells a bad key file from a
tape of another installation.

## Units in this envelope

| Unit | Snapshot | Slices | Tape files |
|------|----------|--------|------------|
| photos/2019 | v7 | 2 | 9..10 |
| ledgers/fy24 | v1 | 1 | 11..11 |

## Where this tape ended

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

## photos/2019

UUID: `11111111-2222-3333-4444-555555555555`  ·  snapshot v7  ·  40960 bytes of tar stream

This (the operator's) envelope lists the unit but holds its file table only
as rows of `catalog.db`; the unit's own tenant's envelope has it as a file.

Put the drive in fixed 512KB block mode, then read, trim, verify and
decrypt each slice, in order: the tape is wound to the first slice
once, and each read that finishes leaves it at the start of the next
file. A `dd` that stops with an error ("Input/output error") leaves
the head inside that file instead: `mt rewind`, then `mt fsf` to the
tape file of the slice you want next, which its `# Slice` line gives.
`/dev/nst0` below is an example — run `ls -l /dev/tape/by-id/` and
substitute your own drive, or you may read a different tape. The block
makes `work/` and a directory for this unit and removes it when the unit is
restored: it needs room for the whole unit's slices (its stream size) once,
and the destination needs the same again.

```bash
mkdir -p work/11111111_v7_s42 /destination/11111111_v7_s42
mt -f /dev/nst0 setblk 524288

# On each `age -d` line, add `-i <file>` for every other key you hold
# (see "Which key" above).

# Slice 1 — tape file 9, 20512 bytes
mt -f /dev/nst0 rewind && mt -f /dev/nst0 fsf 9
dd if=/dev/nst0 bs=512k of=work/11111111_v7_s42/slice.1.age
truncate -s 20512 work/11111111_v7_s42/slice.1.age
echo "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb  work/11111111_v7_s42/slice.1.age" | sha256sum -c - && age -d -i YOUR_KEY.age.key work/11111111_v7_s42/slice.1.age > work/11111111_v7_s42/slice.1.tar && rm work/11111111_v7_s42/slice.1.age
test $(wc -c < work/11111111_v7_s42/slice.1.tar) -eq 20480 && echo "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa  work/11111111_v7_s42/slice.1.tar" | sha256sum -c -

# Slice 2 — tape file 10, 20512 bytes
dd if=/dev/nst0 bs=512k of=work/11111111_v7_s42/slice.2.age
truncate -s 20512 work/11111111_v7_s42/slice.2.age
echo "dddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddd  work/11111111_v7_s42/slice.2.age" | sha256sum -c - && age -d -i YOUR_KEY.age.key work/11111111_v7_s42/slice.2.age > work/11111111_v7_s42/slice.2.tar && rm work/11111111_v7_s42/slice.2.age
test $(wc -c < work/11111111_v7_s42/slice.2.tar) -eq 20480 && echo "cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc  work/11111111_v7_s42/slice.2.tar" | sha256sum -c -

# The slices, joined in order, are the unit's tar stream. One tar run, into the
# directory made above (numbers, not names, for owner and group; with another
# tar than GNU tar, leave out the flags it does not know):
cat work/11111111_v7_s42/slice.1.tar work/11111111_v7_s42/slice.2.tar | LC_ALL=C tar -x --numeric-owner -p --xattrs --acls -f - -C /destination/11111111_v7_s42
rm -r work/11111111_v7_s42
```

## ledgers/fy24

UUID: `66666666-7777-8888-9999-000000000000`  ·  snapshot v1  ·  10240 bytes of tar stream

Each slice of this unit was compressed with zstd, after the cut and before
it was encrypted: the block below pipes each decrypted slice through
`zstd -dc` (package zstd). The length and the hash it checks are of the
decompressed slice.

This (the operator's) envelope lists the unit but holds its file table only
as rows of `catalog.db`; the unit's own tenant's envelope has it as a file.

Put the drive in fixed 512KB block mode, then read, trim, verify and
decrypt each slice, in order: the tape is wound to the first slice
once, and each read that finishes leaves it at the start of the next
file. A `dd` that stops with an error ("Input/output error") leaves
the head inside that file instead: `mt rewind`, then `mt fsf` to the
tape file of the slice you want next, which its `# Slice` line gives.
`/dev/nst0` below is an example — run `ls -l /dev/tape/by-id/` and
substitute your own drive, or you may read a different tape. The block
makes `work/` and a directory for this unit and removes it when the unit is
restored: it needs room for the whole unit's slices (its stream size) once,
and the destination needs the same again.

```bash
mkdir -p work/66666666_v1_s43 /destination/66666666_v1_s43
mt -f /dev/nst0 setblk 524288

# On each `age -d` line, add `-i <file>` for every other key you hold
# (see "Which key" above).

# Slice 1 — tape file 11, 4100 bytes
mt -f /dev/nst0 rewind && mt -f /dev/nst0 fsf 11
dd if=/dev/nst0 bs=512k of=work/66666666_v1_s43/slice.1.age
truncate -s 4100 work/66666666_v1_s43/slice.1.age
echo "ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff  work/66666666_v1_s43/slice.1.age" | sha256sum -c - && age -d -i YOUR_KEY.age.key work/66666666_v1_s43/slice.1.age | zstd -dc > work/66666666_v1_s43/slice.1.tar && rm work/66666666_v1_s43/slice.1.age
test $(wc -c < work/66666666_v1_s43/slice.1.tar) -eq 10240 && echo "eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee  work/66666666_v1_s43/slice.1.tar" | sha256sum -c -

# The slices, joined in order, are the unit's tar stream. One tar run, into the
# directory made above (numbers, not names, for owner and group; with another
# tar than GNU tar, leave out the flags it does not know):
cat work/66666666_v1_s43/slice.1.tar | LC_ALL=C tar -x --numeric-owner -p --xattrs --acls -f - -C /destination/66666666_v1_s43
rm -r work/66666666_v1_s43
```

## The index of other tapes

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

## If a slice or a file is damaged

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

## Troubleshooting

- **age: "failed to decrypt and authenticate payload chunk"** (age 1.1.1) — the
file still has 512KB block padding, or a part of it is damaged. Re-run
`truncate -s <bytes>` to the exact `encrypted_bytes` shown above (a slice's
`size_bytes` in MANIFEST.toml is its PLAIN length: never trim to that); if the
sha256 of the trimmed file matches, the file is whole and the key is the
question, and if it does not, the slice is damaged.
- **age: "failed to read header: parsing age header"** — what was read is not
an age file: the wrong tape file, or the head was not where you thought.
`mt rewind`, `fsf` to the slice's tape file, and read it again.
- **tar: "Unexpected EOF in archive" or "This does not look like a tar
archive"** — the slices given to tar are not whole files in order: one is
missing, left out, or the first begins in the middle of a file. The check
against the file table names what is missing; tar's exit status never does.
- **sha256 mismatch** — re-read the slice from tape; a short read or the wrong
block mode (must be 512KB fixed) is the usual cause.
- **age: "no identity matched any of the recipients"** (`rage`: "No matching
keys found") — none of the keys
you passed is the one this slice was sealed to. It may be an older key from
before a rotation (see "Which key"), or the operator key, which can read
every unit; or the tape may be another installation's. Pass them all with
repeated `-i`.
"####;

/// RECOVERY.md of the operator's envelope, as `build()` makes it before it appends its
/// `catalog.db` section: tenant `operator`, units with no file table and no id map (the
/// catalog.db carries them), and the first unit compressed. The text differs from a tenant's
/// in the paragraph that says the file table is only rows of `catalog.db` and in the missing
/// check against `env/tables/`.
#[test]
fn layout3_recovery_md_operator_envelope() {
    let units = vec![
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
            compression: Some("zstd".to_string()),
            slices: vec![slice(1, Some(11), (10_240, 4_100), ("e", "f"), false)],
        },
    ];
    let text = generate_recovery_md_at("L8-0001", "operator", &units, &[], "2026-11-21");
    assert_pinned_text("recovery-operator", &text, RECOVERY_OPERATOR);
}

const RECOVERY_CATALOG_DB_SECTION: &str = r###"
## Querying catalog.db

This envelope also carries `catalog.db`, a small standalone SQLite database (no tapectl or the source `tapectl.db` needed) covering every unit written to THIS volume, across all tenants — operator-only, never in a tenant envelope. Tables: `tenants`, `units`, `snapshots`, `stage_sets` (with each set's recorded recipient list, `key_fingerprints`), `stage_slices` (with `sha256_plain`), `paths` (each path once per unit, relative to the snapshot's `source_path`) and `file_versions` (one row per entry of each version: `kind` 0 directory, 1 regular file, 2 symlink, 3 special file such as a FIFO or device; `size_bytes`; `mtime_ns`, Unix time in nanoseconds; `sha256` as 32 raw bytes, `lower(hex(sha256))` prints it; `link_target`, a symlink's target), then `stage_files` (each staging's file table: one row for each file of its tar stream, `seq`, `kind`, `slice`, `path_id` into `paths`, `size`, `sha256`, `start` and `end`, and `link_path_id` for a hard link) and `stage_ids` (its id map: `kind` `u` or `g`, `id`, `name`, who each user and group number was on the computer that wrote the tape). Permissions, owners and extended attributes are in each unit's tar stream, not here; a tenant's envelope holds the same file table and id map as the text files in `tables/`. Tape positions are NOT here; MANIFEST.toml beside this file has them. `PRAGMA user_version` on this file mirrors the schema level of the `tapectl.db` it was generated from. With the operator or escrow key, `tapectl catalog rebuild --from-volume` reconstructs the catalog from this envelope. Examples: each unit's versions and their entry counts, then one unit's entries (put its name in place of UNIT):

```bash
sqlite3 catalog.db 'SELECT u.name, s.version, count(fv.path_id) FROM units u JOIN snapshots s ON s.unit_id = u.id LEFT JOIN file_versions fv ON fv.snapshot_id = s.id GROUP BY s.id;'
sqlite3 catalog.db "SELECT p.path, CASE fv.kind WHEN 0 THEN 'dir' WHEN 1 THEN 'regular' WHEN 2 THEN 'symlink' ELSE 'special' END, fv.size_bytes, lower(hex(fv.sha256)), datetime(fv.mtime_ns / 1000000000, 'unixepoch'), fv.link_target FROM file_versions fv JOIN paths p ON p.id = fv.path_id JOIN snapshots s ON s.id = fv.snapshot_id JOIN units u ON u.id = s.unit_id WHERE u.name = 'UNIT' ORDER BY s.version, p.path;"
```
"###;

/// The section `build()` appends to the operator envelope's RECOVERY.md when the envelope
/// carries `catalog.db` (issue #83): on tape after the text pinned above.
#[test]
fn layout3_recovery_md_operator_catalog_db_section() {
    assert_pinned_text(
        "recovery-operator-catalog-db",
        tapectl::volume::build::RECOVERY_CATALOG_DB,
        RECOVERY_CATALOG_DB_SECTION,
    );
}

/// RESTORE.sh's sha256: the heir's script, `src/volume/heir/RESTORE.sh`, which `build()`
/// writes to every tape as File 2 byte for byte (`heir::RESTORE_SH`, `include_str!`). Pinned
/// by its hash, not its text (it is long, and the file in the tree is the text): a change to
/// it is a change to every tape's heir path, a CTO decision, and proved in the `tapectl-u26`
/// container as uid 1000 and by the gate's heir legs before this moves.
const RESTORE_SH_SHA256: &str = "6e15124939e8043bf0adcdb8e964e4ea465e55b4ee5c0cd48c176e80832b0f7f";

#[test]
fn layout3_restore_sh_sha256() {
    use sha2::{Digest, Sha256};
    let script = tapectl::volume::heir::RESTORE_SH;
    let got: String = Sha256::digest(script.as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    if got != RESTORE_SH_SHA256 {
        let path =
            std::path::Path::new(env!("CARGO_TARGET_TMPDIR")).join("golden-restore-sh.actual");
        let _ = std::fs::write(&path, script);
        panic!(
            "the on-tape bytes of RESTORE.sh changed (sha256 {got}, pinned {RESTORE_SH_SHA256}): \
             a CTO decision, never a re-pin by an agent. The script is in {}",
            path.display()
        );
    }
}
