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

use sha2::{Digest, Sha256};
use tapectl::volume::format;
use tapectl::volume::layout::{
    generate_manifest_toml, generate_restore_script_v2, generate_seal_marker,
    generate_short_seal_marker, FrontIndexFile, ManifestSlice, ManifestUnit,
};

fn sha256_hex(s: &str) -> String {
    let mut h = Sha256::new();
    h.update(s.as_bytes());
    format!("{:x}", h.finalize())
}

/// The RESTORE.sh a volume labelled GOLD01 with 12 files gets. The script is
/// pure substitution, so its hash is stable across runs and machines.
const RESTORE_SH_SHA256: &str = "a5dc4063cf47eafad4c4111d8a58c610bfc8e130e1468de0dab8d32205ecb39f";

#[test]
fn restore_sh_bytes_are_pinned() {
    let script = generate_restore_script_v2("GOLD01", 12);
    let actual = sha256_hex(&script);
    if RESTORE_SH_SHA256 == "__RESTORE_SHA__" {
        eprintln!("CAPTURE restore_sh sha256 = {actual}");
    }
    assert_eq!(
        actual, RESTORE_SH_SHA256,
        "RESTORE.sh bytes changed. That is an on-tape format change and a CTO decision; \
         do not re-pin without one."
    );
    // The prerequisite check: tar (#349), mt or mt-st, and the FIFO tools
    // the streaming restore needs (1.1.0); per mode since 1.2.0 (#449): each
    // mode checks only what it runs, and --restore's set is the union.
    assert!(
        !script.contains("for tool in dd age"),
        "no mode checks every tool any more"
    );
    for line in [
        "# --info, --verify:\nTOOLS_READ=(dd sha256sum head)\n",
        "# --find-envelope:\nTOOLS_ENVELOPE=(dd age sha256sum head truncate tar)\n",
        "# --list:\nTOOLS_LIST=(dd age dar sha256sum head truncate tar)\n",
        "# --restore --path:\nTOOLS_PATH=(dd age dar sha256sum head truncate tar tee)\n",
        "# --restore, whole units (every tool any mode may run):\nTOOLS_RESTORE=(dd age dar sha256sum head truncate tar mkfifo tee)\n",
        "# --where, --find (the envelopes' index, read as plain text):\nTOOLS_INDEX=(dd age sha256sum head truncate tar)\n",
        "require_tools() { # <tool>...\n",
    ] {
        assert_eq!(script.matches(line).count(), 1, "the prerequisite set {line:?}");
    }
    // Every item of the 2026-09-30 ruling still standing, and of 1.1.0's, is
    // in the pinned bytes.
    for ruled in [
        // 2026-09-30
        "--scratch) scratch=$2 ;;",
        "--no-space-check)\n      SKIP_SPACE_CHECK=1",
        "OUT OF DISK SPACE in $where",
        "check_layout_version \"$(toml_val \"$WORK/thunk.toml\" layout_version)\"",
        "# Requirements: mt (mt-st), dd, age, dar, sha256sum, head, truncate, tar, mkfifo, tee\n",
        "echo \"Requirements: mt (mt-st), dd, age, dar, sha256sum, head, truncate, tar, mkfifo, tee\"",
        // #396: forward-only navigation, cursor checked against mt status
        "\"$MT\" -f \"$DEVICE\" fsf $((pos - from))",
        "elif [ \"$here\" != \"$from 0\" ]; then",
        // #405: an empty --to, dar's skip line, the trailing-only strip
        "check_destination \"$destdir\" \"$scratch_parent\"",
        "--overwrite)\n      OVERWRITE=1",
        "skipped=$(grep 'not restored (user choice)$' \"$1\" 2>/dev/null |",
        "head -c $((size - tail_len + keep)) candidate.raw > candidate.stripped",
        // #412: streaming, -N, --all, every envelope, damage, mt-st
        "local -a dar_opts=(-O -Q -N --sequential-read)",
        // dar 2.7.13 loses a file's tail streaming without the catalogue:
        // -A from the envelope, else stream only on 2.7.21+, else spool;
        // a dar that needs gpg is named before the tape is read
        "[ -z \"$cat\" ] || dar_opts+=(-A \"$cat\")",
        "if [ -n \"$cat\" ] && dar_at_least 2.7.9; then",
        "if dar_at_least 2.7.21; then",
        "  info \"  decrypting the slices to disk first (dar 2.7.21 or later would stream it)\"\n  spool_unit \"$list\" \"$destdir\"\n",
        "grep -i 'INITIALIZATION FAILED FOR GPGME' >/dev/null; then",
        // 1.8.0 (ADR-0012 item 181, #498): one stream on dar's standard
        // input, never a named pipe per slice
        "mkfifo \"$fifos/stream\" \"$fifos/cipher\"\n",
        "check_space \"$destdir\" \"$destdir\" \"$total\" 0",
        "--all)\n      RESTORE_ALL=1",
        "walk_envelopes found_envelope",
        "if grep 'no identity matched' \"$WORK/age.err\" >/dev/null 2>&1; then\n      info \"  key $keyfile did not open the envelope at file $pos\"",
        "MT=mt-st",
        // #418: the envelope's dar catalogues
        "--list)\n  shift",
        "--path) RESTORE_PATHS+=(\"$2\") ;;",
        // ADR-0012 item 15 (#384): requires, the seal at end of data
        "check_requires \"File 0\" \"$WORK/thunk.toml\"",
        "\"$MT\" -f \"$DEVICE\" bsfm 2 2>/dev/null || return 1",
        // 1.2.0 (ADR-0012 2026-10-16 item 71, #449): locale, per-mode tools,
        // the survey behind the lone-unit note, "a key matched" only when one
        // did, the work directory and its estimate
        "\nexport LC_ALL=C\n",
        "require_tools \"${TOOLS_RESTORE[@]}\"",
        "require_tools \"${TOOLS_PATH[@]}\"",
        "walk_envelopes count_other_envelope \"$CHOSEN_POS\"",
        "survey_other_envelopes \"${unit_names[0]}\"",
        "restore_unit() { # <slices file> <destination> <unit> <version>\n",
        "  if [ \"$MATCHED_DAMAGED\" -gt 0 ]; then\n    die \"$headline that could be opened.",
        "    MATCHED_DAMAGED=$((MATCHED_DAMAGED + 1))\n",
        "# WORK is the work directory, made in ${TMPDIR:-/tmp}: TMPDIR moves it.",
        "check_work_space() {",
        "  info \"            (counted from the slices' encrypted size on tape; a compressed unit may need more)\"",
        "  echo \"  TMPDIR        Where the work directory is made (default: /tmp). It holds the\"",
        // 1.3.0 (ADR-0012 2026-10-16 items 78-81, #418 R4): the cross-volume
        // index, read by --where and --find
        "--where)\n  shift",
        "--find)\n  shift",
        "  require_tools \"${TOOLS_INDEX[@]}\"\n  require_key_files\n  do_where \"$where_unit\"",
        "  require_tools \"${TOOLS_INDEX[@]}\"\n  require_key_files\n  do_find \"$find_text\"",
        "  walk_envelopes where_envelope\n",
        "  walk_envelopes find_in_envelope\n",
        "index_banner() {\n",
        "die_no_index() {\n",
        // 1.3.0, widened (ADR-0012 2026-10-17 items 101, 102, #480): a slice
        // from another copy by its sha256, the damaged-file list, the fill
        "--slices-from) SLICES_FROM+=(\"$2\") ;;",
        "--fill-from) FILL_FROM=$2 ;;",
        "    --all)\n      RESTORE_ALL=1\n      shift\n      ;;\n    --swap-tapes)\n      SWAP_TAPES=1",
        "    --swap-tapes)\n      SWAP_TAPES=1\n      shift\n      ;;\n    *) die \"unknown option: $1\n\n       usage: $0 --verify",
        "    --slices-from)\n      [ $# -ge 2 ] || die \"--slices-from needs a directory",
        "check_stale_scratch() { # <directory>...\n",
        "decrypt_slice() { # <ciphertext> <plaintext-out> <slice-number> [bytes]\n",
        "spool_slices() { # <slices file> [<directory> [<file for the slices not found>]]\n",
        "      if [ \"$(dir_sum \"$f\" \"$size\")\" = \"$sha\" ]; then\n",
        "other_tape_map() {\n",
        "swap_fetch() {\n",
        "swap_loop() {\n",
        "    [dD]one | DONE | q | quit)\n",
        "DAMAGED_NAME=.tapectl-damaged-files.txt\n",
        "dar_result() { # <log> <status> <destination> [slices not available]\n",
        "  dar_result \"$WORK/extract.log\" \"$rc\" \"$destdir\"\n}\n\n# Read, check and decrypt the slices listed in $1",
        "  SCRATCH=\"\"\n  dar_result \"$WORK/extract.log\" \"$rc\" \"$destdir\"\n}\n",
        "  dar_result \"$WORK/extract.log\" \"$rc\" \"$destdir\"\n  if [ \"$UNIT_OK\" = 1 ]; then",
        "do_fill() { # <destination> <version given, or empty> <scratch parent, or empty>\n",
        "restore_end() {\n",
        "    info \"RESTORE INCOMPLETE\"\n",
        "  info \"RESTORE COMPLETE\"\n  for line in ${DONE_LINES[@]+\"${DONE_LINES[@]}\"}; do",
        "  finish_pending\n  restore_end\n",
        "dar -x - -R \"$(abs_dir \"$destdir\")\" \"${dar_opts[@]}\" <\"$fifos/stream\"",
        "dar -x \"$SCRATCH/job.$j/restore\" -R \"$(abs_dir \"$destdir\")\"",
        "    do_fill \"$dest\" \"$want\" \"$scratch\"\n",
        "  echo \"  $0 --restore --key KEYFILE --to DIR --unit U --fill-from LIST\"",
        // 1.3.0, widened again (ADR-0012 item 103): the restored files against
        // the sha256 their index records
        "    --no-file-check)\n      FILE_CHECK=0\n      shift\n      ;;\n",
        "index_for_unit() { # <unit> <version> <unit uuid>\n",
        "file_check() { # <destination> [<file of escaped paths to check>]\n",
        "finish_damage() { # <destination> <slices not available, or empty>\n",
        "  file_check \"$root\" \"$WORK/fill.restored\"\n",
        // 1.3.0, widened a third time (the third read of File 1, #477): every
        // unit the index names, with no cap
        "where_every_unit() {\n",
        "  if [ \"$unit\" = --all ]; then\n    where_every_unit\n    return 0\n  fi\n",
        "do_where() { # <unit, or --all for every unit>\n",
        "--where needs a unit name, or --all for every unit the index names",
        // 1.7.0 (ADR-0012 items 124-126, 140): one NOT ON TAPE line on every
        // short tape, and a range read on one tape (volume-format-v2.md 4.4)
        "not_on_tape_line() { # <indent> <first planned file after the end> <last>\n",
        "printf \"%sNOT ON TAPE  files %d to %d  (planned; the tape ended after file %d, and its seal marker is file %d)\\n\"",
        "slice_here() { # <slice number> <its tape file here, or empty>\n",
        "range_text() { # <unit> <version>\n",
        "range_holders() { # <unit> <version>\n",
        "die_range() { # <unit> <version>\n",
        "note_range_units() {\n",
        "list_range_note() { # <unit>\n",
        "note_range_holders() {\n",
        "read -r PICKED_VERSION PICKED_SPAN _ <\"$WORK/picked_version.txt\" 2>/dev/null || true",
        "RANGE_OUT=\"$WORK/range.versions\" awk -v k=\"$k\"",
        "is held across tapes, and this tape holds only part of it:",
        "HELD ACROSS TAPES: this tape holds only some of the slices of these unit(s), so they were not",
        "a range is never",
        "STREAM_TAPES=0\nSPAN_RESTORE=\"\"\n",
        "    --stream-tapes)\n      STREAM_TAPES=1\n      shift\n      ;;\n",
        "slice_bytes() { # <slice number>\n",
        "      slice_bytes \"$num\" |\n",
        "restore_across_tapes() { # <manifest> <unit> <version> <destination>\n",
        "span_pos() { # <slice number>\n",
        "span_ask() {\n",
        "span_stop() {\n",
        "span_accept() {\n",
        "span_swap() { # <slice number>\n",
        "span_spool() {\n",
        "die_range_path() { # <unit> <version>\n",
        "HOLDER_SLICE=1\n",
        "CHK_SLICE=\"$HOLDER_SLICE\"",
        "    if [ -z \"$SPAN_NEED\" ]; then\n      swap_fetch || true\n    elif span_accept; then\n      return 0\n    fi\n",
        "      [ -z \"$SPAN_NEED\" ] || span_stop\n",
        "(other_tape_map && printf '%s|%s\\n' \"$OTHER_END\" \"$OTHER_LABEL\" >\"$WORK/other.vars\") 2>\"$WORK/other.err\" || rc=$?",
        "check_space \"$destdir\" \"$SCRATCH_PARENT\" \"$bytes\" 0",
        "      if read_slice_checked \"$pos\" \"$size\" \"$sha\" \"$SCRATCH/slice.$num.enc\"; then\n",
        "    STREAM_FROM=$SCRATCH\n",
        "  restore_unit \"$WORK/span.all\" \"$destdir\" \"$unit\" \"$ver\"\n",
        "      [ \"$PICKED_RUN_FIRST\" = 1 ] || die_range \"${targets[$i]}\" \"$PICKED_VERSION\"\n",
        "  if [ \"$SPAN_RESTORE\" = scratch ]; then\n    die \"not enough disk space in $1 to restore this unit across tapes:",
        "  [ -z \"$SPAN_RESTORE\" ] || die \"dar ${DAR_VERSION:-(of an unknown version)} cannot read '$unit' across tapes without its",
        "  if ! { [ -n \"$cat\" ] && dar_at_least 2.7.9; } && ! dar_at_least 2.7.21; then\n",
        "    info \"  (rm -rf \\\"$SCRATCH\\\") before running again.\"\n",
        "--stream-tapes reads one unit across tapes: give --unit U, not --all",
        "span_take_here() { # <unit>\n",
        "span_list() { # <unit>\n",
        "span_path_spool() { # <unit> <version> <slices of the version>\n",
        "    span_path_spool \"$unit\" \"$ver\" \"$last\"\n",
        "      [ \"$SALVAGE\" = 0 ] || die_range_path \"${targets[0]}\" \"$PICKED_VERSION\"\n",
        "      span_list \"${targets[0]}\"\n      cp \"$WORK/picked.txt\" \"$WORK/slices.0\"\n",
        "      [ ! -f \"$SCRATCH/slice.$num.enc\" ] || continue\n",
        "  SPAN_RESTORE=path\n  span_spool\n  SPAN_RESTORE=\"\"\n",
        "    decrypt_slice \"$SCRATCH/slice.$num.enc\" \"$SCRATCH/restore.$num.dar\" \"$num\"\n",
        "if ($1 + 0 >= from + 0 && index(have, \" \" $1 \" \") == 0) { n++; num[n] = $1 + 0; sz[n] = $3; sh[n] = $4 }",
        "sealed_other() {\n",
        "  info \"This cartridge identifies as: $(safe_str \"${OTHER_LABEL:-<unreadable>}\")\"\n  sealed_other || return 1\n",
        "    elif [ \"$TAPE_SHORT_SEAL\" = 0 ] && try_seal_copy; then\n      OTHER_END=$SEAL_LAST\n",
        "$2 == \"data_slice\" && $1 ~ /^[0-9]+$/ && $1 + 0 <= end + 0 && ($4 in need) && $3 == need[$4] && !($4 in seen)",
        "  SPAN_OFFERED=1\n  (other_tape_map",
        "  if [ \"$SPAN_OFFERED\" = 1 ]; then\n    die \"$2\n",
        "  if [ \"$SPAN_OFFERED\" = 1 ]; then\n    die \"this tape is layout_version",
        "  SPAN_SWAPPED=1\n}\n",
        "        if ! kill -0 \"$DAR_PID\" 2>/dev/null; then\n          die_dar \"$destdir\"\n          return 0\n        fi\n        span_swap \"$num\"\n",
        "    if [ \"$SPAN_RESTORE\" = stream ] && [ \"$SPAN_SWAPPED\" = 1 ]; then\n",
        "  if [ \"$SPAN_RESTORE\" = path ]; then\n    die \"stopped before slice",
        "nothing is restored without every slice --path needs.",
        "units_table() { # <MANIFEST.toml>\n  local rows name versions nslices bytes part parted=0\n",
        "printf \"    %-32s  %-10s  %6s  %10s  PART: %s\\n\"",
        "!in_head && /^tape_position = / && cur != \"\" { bp++; if (bf == \"\") bf = num; bl = num }",
        "echo \"    PART: a unit held across tapes, of which this tape's plan holds only the slices named (Slices\"",
        "eject_first() {\n",
        "  eject_first\n  info \"Load the cartridge that holds slice $SPAN_NEED into $DEVICE,",
        "      eject_first\n      info \"Load a cartridge that holds copies of these slices into $DEVICE",
        "done or quit STOPS this restore and discards the slices read so far, with the scratch directory:",
        "done or quit STOPS this restore: it does not go on without the slice, and the files dar has",
        "It must hold this version WHOLE: a tape that holds only part of a unit held across tapes is",
        "this run and removes that directory with the slices read: run it in screen or tmux. A run",
        "tmux; only kill -9 or a crash leaves it, to remove by hand.",
        "A unit across tapes (by default) keeps all of its slices there too, still",
        "it, or in --to/UNIT). The tape must hold that version WHOLE: one that holds only part",
        "says why and asks again; done or quit stops it.",
        "      got=$(sha256sum 2>/dev/null <\"$destdir/$UNESC\") || got=\"\"\n",
        "restored_scope() {\n",
        "expand_damaged_dirs() { # <file of escaped paths, rewritten in place>\n",
        "  expand_damaged_dirs \"$WORK/damaged.new\"\n  file_check \"$destdir\"\n",
        "  expand_damaged_dirs \"$WORK/fill.still\"\n",
        "fill_runs() { # <job> <root> <file|dir>\n",
        "  fill_runs \"$j\" \"$root\" file\n  fill_runs \"$j\" \"$root\" dir\n",
        "  [ \"$kind\" = file ] || opt=-n\n",
        "      FILL_KIND+=(dir)\n",
        "      for (i = 1; i <= nfound; i++) above(found[i])\n",
        "      for (j = 1; j <= m; j++) rt[j] = top(rp[j], isdir)\n",
        "    if [ \"$FILL_DIRS\" -gt 0 ]; then\n      info \"  In the $FILL_DIRS listed folder(s) only what was missing was restored: what was there already\"\n",
        // 1.8.0 (ADR-0012 items 181 and 182, #498): the slices joined back
        // into the stream they were cut from, under this dar's own header,
        // the stream opened once and written only by a child; and damage
        // dar never needed, said and not fatal
        "dar -N -c - -R \"$WORK/empty\" -an -D -Q",
        "join_slice() { # <slice number> <1 for the first slice> <1 for the last> <plaintext bytes>\n",
        "join_rest() { # <header bytes> <slice size> <1 for the last slice> <slice number> <plaintext bytes>\n",
        // Round 3 (uutils coreutils 0.10.0, Ubuntu 26.04): every slice cut
        // at its manifest size, no tail -c 1 on a FIFO, no head -c -1
        "  if ! head -c $((plain - h - 1)) 2>/dev/null >&3; then\n",
        // Round 4 (PR #500): late damage decided by how dar ended, after its
        // stream is closed at the damage, never by whether it runs at an instant
        "    if [ \"$STREAM_OPEN\" = 1 ] && { [ \"${st[3]:-1}\" != 0 ] || [ \"$got\" != \"$sha\" ] ||\n",
        "      stream_close\n      dar_reap\n    fi\n    # Damage cannot have reached what dar needed when dar ended well: the slice\n",
        "    if [ -n \"$DAR_RC\" ] && [ \"$DAR_RC\" = 0 ]; then\n      late=1\n    fi\n",
        "  awk -v unit=\"$unit\" -v want=\"$want\" -v plain_out=\"$WORK/picked.plain\" '",
        "  exec 3>\"$fifos/stream\"\n",
        "    (printf T >&3) 2>/dev/null || true\n",
        "late_damage() { # <slice number> <tape file, or empty> age|sha [<sha256 expected> <sha256 got>]\n",
        // A host that cannot join the slices (its head, or its dar's header):
        // one unit on one tape from slices on disk, one across tapes refused
        "stream_check() {\n",
        "  got=$(printf abcdef | { head -c 2 >/dev/null; head -c 1; } 2>/dev/null) || true\n",
        "      echo \"      They are decrypted to disk first instead, and dar reads them from there, as for a dar\"\n",
        "    } >&2\n    spool_unit \"$list\" \"$destdir\"\n    return 0\n",
        "  if ! stream_check; then\n    die \"'$unit' is held across tapes, and its slices cannot be joined into the one stream\n",
    ] {
        assert_eq!(script.matches(ruled).count(), 1, "ruled item {ruled:?}");
    }
    // The two placeholders must both have been substituted.
    assert!(!script.contains("__LABEL__") && !script.contains("__TOTAL_FILES__"));
    assert!(!script.contains("__AWK_") && !script.contains("__TOOLS_"));
    // Each of these twice: the --where program and the --find program.
    assert_eq!(
        script
            .matches("INDEX_CUT=\"$WORK/cut.versions\" INDEX_RANGE=\"$WORK/range.versions\" INDEX_PARTIAL=\"$WORK/partial.flag\"")
            .count(),
        2,
        "the index programs read the range runs"
    );
    // ADR-0012 item 73's two fixed points, moved by item 111 (1.6.0) and by
    // item 124 (1.7.0): the "needs" text names 1.7.0, and short-seal and
    // spanning are the known features.
    assert_eq!(
        script
            .matches("This RESTORE.sh (layout v2, tapectl 1.7.0) cannot read it safely")
            .count(),
        1,
        "die_newer_needed says tapectl 1.7.0"
    );
    assert_eq!(
        script.matches("tapectl 1.").count(),
        1,
        "no other version named"
    );
    assert_eq!(
        script
            .matches("\nKNOWN_FEATURES=\"short-seal spanning\"\n")
            .count(),
        1,
        "RESTORE.sh knows short-seal and spanning and no other feature"
    );
    assert!(script.contains("LABEL=\"GOLD01\""));
    assert!(script.contains("# Total files on tape: 12"));
}

fn golden_units() -> Vec<ManifestUnit> {
    vec![
        ManifestUnit {
            name: "photos/2019".to_string(),
            uuid: "11111111-2222-3333-4444-555555555555".to_string(),
            snapshot_version: 7,
            stage_set_id: 42,
            may_span: false,
            dar_version: Some("2.7.20".to_string()),
            dar_command: Some(r#"dar -c "x" -s 10G \ path"#.to_string()),
            slices: vec![
                ManifestSlice {
                    number: 1,
                    tape_position: Some(9),
                    size_bytes: 100,
                    encrypted_bytes: 120,
                    sha256_plain: "aa".repeat(32),
                    sha256_encrypted: "bb".repeat(32),
                },
                ManifestSlice {
                    number: 2,
                    tape_position: Some(10),
                    size_bytes: 50,
                    encrypted_bytes: 70,
                    sha256_plain: "cc".repeat(32),
                    sha256_encrypted: "dd".repeat(32),
                },
            ],
        },
        ManifestUnit {
            name: "ledgers/fy24".to_string(),
            uuid: "66666666-7777-8888-9999-000000000000".to_string(),
            snapshot_version: 1,
            stage_set_id: 43,
            may_span: false,
            dar_version: None,
            dar_command: None,
            slices: vec![ManifestSlice {
                number: 1,
                tape_position: Some(11),
                size_bytes: 5,
                encrypted_bytes: 25,
                sha256_plain: "ee".repeat(32),
                sha256_encrypted: "ff".repeat(32),
            }],
        },
    ]
}

/// Replace the one volatile line so the rest of the document can be pinned
/// byte-for-byte.
fn normalise_created_at(manifest: &str) -> String {
    manifest
        .lines()
        .map(|l| {
            if l.starts_with("created_at = ") {
                "created_at = \"<normalised>\"".to_string()
            } else {
                l.to_string()
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
        + if manifest.ends_with('\n') { "\n" } else { "" }
}

const MANIFEST_EXPECTED: &str = r##"[manifest]
volume = "GOLD01"
tenant = "alpha"
created_at = "<normalised>"
may_end_early = true

[[units]]
name = "photos/2019"
uuid = "11111111-2222-3333-4444-555555555555"
snapshot_version = 7
stage_set_id = 42
dar_version = "2.7.20"
dar_command = "dar -c \"x\" -s 10G \\ path"

[[units.slices]]
number = 1
tape_position = 9
size_bytes = 100
encrypted_bytes = 120
sha256_plain = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
sha256_encrypted = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"

[[units.slices]]
number = 2
tape_position = 10
size_bytes = 50
encrypted_bytes = 70
sha256_plain = "cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc"
sha256_encrypted = "dddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddd"

[[units]]
name = "ledgers/fy24"
uuid = "66666666-7777-8888-9999-000000000000"
snapshot_version = 1
stage_set_id = 43

[[units.slices]]
number = 1
tape_position = 11
size_bytes = 5
encrypted_bytes = 25
sha256_plain = "eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee"
sha256_encrypted = "ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff"

"##;

#[test]
fn manifest_toml_bytes_are_pinned_except_created_at() {
    let manifest = generate_manifest_toml("GOLD01", "alpha", &golden_units());
    let actual = normalise_created_at(&manifest);
    if MANIFEST_EXPECTED == "__MANIFEST__" {
        eprintln!("CAPTURE manifest =\n{actual}\nCAPTURE end");
    }
    // Exactly one created_at line, and it is RFC 3339.
    let stamps: Vec<&str> = manifest
        .lines()
        .filter(|l| l.starts_with("created_at = "))
        .collect();
    assert_eq!(stamps.len(), 1, "{stamps:?}");
    assert!(
        stamps[0].contains('T') && stamps[0].ends_with('"'),
        "{}",
        stamps[0]
    );
    assert_eq!(
        actual, MANIFEST_EXPECTED,
        "MANIFEST.toml bytes changed (created_at excluded). That is an on-tape format \
         change and a CTO decision; do not re-pin without one."
    );
}

// ── the range block (#478, ADR-0012 item 125) ──────────────────────────────

/// A spannable unit's block as a span tape states it: `may_span = true`
/// after `stage_set_id`, and every slice of the set listed, with
/// `tape_position` only on the slices this tape holds (slice 3 is on another
/// tape). The whole unit after it is written as before. The bytes are the
/// ones `manifest::tests::a_range_block_round_trips_byte_for_byte` reads;
/// the `photos/2019` block alone is 945 bytes.
fn range_units() -> Vec<ManifestUnit> {
    vec![
        ManifestUnit {
            name: "photos/2019".to_string(),
            uuid: "11111111-2222-3333-4444-555555555555".to_string(),
            snapshot_version: 7,
            stage_set_id: 42,
            may_span: true,
            dar_version: Some("2.7.20".to_string()),
            dar_command: Some(r#"dar -c "x" -s 10G \ path"#.to_string()),
            slices: vec![
                ManifestSlice {
                    number: 1,
                    tape_position: Some(9),
                    size_bytes: 100,
                    encrypted_bytes: 120,
                    sha256_plain: "11".repeat(32),
                    sha256_encrypted: "21".repeat(32),
                },
                ManifestSlice {
                    number: 2,
                    tape_position: Some(10),
                    size_bytes: 101,
                    encrypted_bytes: 121,
                    sha256_plain: "12".repeat(32),
                    sha256_encrypted: "22".repeat(32),
                },
                ManifestSlice {
                    number: 3,
                    tape_position: None,
                    size_bytes: 102,
                    encrypted_bytes: 122,
                    sha256_plain: "13".repeat(32),
                    sha256_encrypted: "23".repeat(32),
                },
            ],
        },
        ManifestUnit {
            name: "ledgers/fy24".to_string(),
            uuid: "66666666-7777-8888-9999-000000000000".to_string(),
            snapshot_version: 1,
            stage_set_id: 43,
            may_span: false,
            dar_version: None,
            dar_command: None,
            slices: vec![ManifestSlice {
                number: 1,
                tape_position: Some(11),
                size_bytes: 5,
                encrypted_bytes: 25,
                sha256_plain: "ee".repeat(32),
                sha256_encrypted: "ff".repeat(32),
            }],
        },
    ]
}

const MANIFEST_RANGE_EXPECTED: &str = r##"[manifest]
volume = "SPAN01"
tenant = "alpha"
created_at = "<normalised>"
may_end_early = true

[[units]]
name = "photos/2019"
uuid = "11111111-2222-3333-4444-555555555555"
snapshot_version = 7
stage_set_id = 42
may_span = true
dar_version = "2.7.20"
dar_command = "dar -c \"x\" -s 10G \\ path"

[[units.slices]]
number = 1
tape_position = 9
size_bytes = 100
encrypted_bytes = 120
sha256_plain = "1111111111111111111111111111111111111111111111111111111111111111"
sha256_encrypted = "2121212121212121212121212121212121212121212121212121212121212121"

[[units.slices]]
number = 2
tape_position = 10
size_bytes = 101
encrypted_bytes = 121
sha256_plain = "1212121212121212121212121212121212121212121212121212121212121212"
sha256_encrypted = "2222222222222222222222222222222222222222222222222222222222222222"

[[units.slices]]
number = 3
size_bytes = 102
encrypted_bytes = 122
sha256_plain = "1313131313131313131313131313131313131313131313131313131313131313"
sha256_encrypted = "2323232323232323232323232323232323232323232323232323232323232323"

[[units]]
name = "ledgers/fy24"
uuid = "66666666-7777-8888-9999-000000000000"
snapshot_version = 1
stage_set_id = 43

[[units.slices]]
number = 1
tape_position = 11
size_bytes = 5
encrypted_bytes = 25
sha256_plain = "eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee"
sha256_encrypted = "ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff"

"##;

#[test]
fn manifest_toml_range_block_bytes_are_pinned_except_created_at() {
    let manifest = generate_manifest_toml("SPAN01", "alpha", &range_units());
    let actual = normalise_created_at(&manifest);
    assert_eq!(
        actual, MANIFEST_RANGE_EXPECTED,
        "MANIFEST.toml's range block changed (created_at excluded). That is an on-tape \
         format change and a CTO decision; do not re-pin without one."
    );
}

// ── the short seal (#420, `volume-format-v2.md` §4.1) ──────────────────────

/// The plan the short-seal pin is cut from: GOLD01's twelve files, as the
/// RESTORE.sh pin's volume has (File 0 to 3, two envelopes and their two
/// backups, three data slices, the seal at 11). Sizes and hashes are fixed
/// literals, so the pin covers the seal's generator and nothing else: File 3's
/// own entry carries the size and hash a writer fills in at seal time, and the
/// seal's planned entry is bare.
fn golden_seal_plan() -> Vec<FrontIndexFile> {
    let kinds = [
        "id_thunk",
        "system_guide",
        "restore_sh",
        "front_index",
        "tenant_envelope",
        "operator_envelope",
        "operator_envelope_backup",
        "tenant_envelope_backup",
        "data_slice",
        "data_slice",
        "data_slice",
        "seal_marker",
    ];
    kinds
        .iter()
        .enumerate()
        .map(|(position, &type_label)| {
            let bare = type_label == "seal_marker";
            FrontIndexFile {
                position: position as i32,
                type_label,
                size_bytes: (!bare).then(|| 1_000 + 37 * position as u64),
                sha256_encrypted: (!bare).then(|| format!("{:02x}", 0xa0 + position).repeat(32)),
            }
        })
        .collect()
}

/// The front index's sha256 the golden seals bind (a literal: File 3's text
/// is not this pin's subject).
const GOLDEN_FRONT_INDEX_SHA256: &str =
    "3333333333333333333333333333333333333333333333333333333333333333";

/// The early warning's data slice in the golden plan: the second of three,
/// so the short seal is at 10 and the third slice is cut.
const GOLDEN_ENDED_AFTER: i32 = 9;

/// Replace the seal's one volatile line, as [`normalise_created_at`] does
/// MANIFEST.toml's.
fn normalise_sealed_at(seal: &str) -> String {
    seal.lines()
        .map(|l| {
            if l.starts_with("sealed_at = ") {
                "sealed_at = \"<normalised>\"".to_string()
            } else {
                l.to_string()
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
        + if seal.ends_with('\n') { "\n" } else { "" }
}

/// sha256 of [`SHORT_SEAL_EXPECTED`], the normalised short seal.
const SHORT_SEAL_SHA256: &str = "4e126a221a1d1be7d734296d9122507810cb0719d550b309ee6277ccf18c9195";

const SHORT_SEAL_EXPECTED: &str = r##"================================================================
                    TAPECTL SEAL MARKER
================================================================

Volume: GOLD01
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
volume = "GOLD01"
layout_version = 2
requires = ["short-seal"]
file_count = 11
ended_after = 9
sealed_at = "<normalised>"
front_index_sha256 = "3333333333333333333333333333333333333333333333333333333333333333"

[[files]]
position = 0
type = "id_thunk"
size_bytes = 1000
sha256_encrypted = "a0a0a0a0a0a0a0a0a0a0a0a0a0a0a0a0a0a0a0a0a0a0a0a0a0a0a0a0a0a0a0a0"

[[files]]
position = 1
type = "system_guide"
size_bytes = 1037
sha256_encrypted = "a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1"

[[files]]
position = 2
type = "restore_sh"
size_bytes = 1074
sha256_encrypted = "a2a2a2a2a2a2a2a2a2a2a2a2a2a2a2a2a2a2a2a2a2a2a2a2a2a2a2a2a2a2a2a2"

[[files]]
position = 3
type = "front_index"
size_bytes = 1111
sha256_encrypted = "a3a3a3a3a3a3a3a3a3a3a3a3a3a3a3a3a3a3a3a3a3a3a3a3a3a3a3a3a3a3a3a3"

[[files]]
position = 4
type = "tenant_envelope"
size_bytes = 1148
sha256_encrypted = "a4a4a4a4a4a4a4a4a4a4a4a4a4a4a4a4a4a4a4a4a4a4a4a4a4a4a4a4a4a4a4a4"

[[files]]
position = 5
type = "operator_envelope"
size_bytes = 1185
sha256_encrypted = "a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5"

[[files]]
position = 6
type = "operator_envelope_backup"
size_bytes = 1222
sha256_encrypted = "a6a6a6a6a6a6a6a6a6a6a6a6a6a6a6a6a6a6a6a6a6a6a6a6a6a6a6a6a6a6a6a6"

[[files]]
position = 7
type = "tenant_envelope_backup"
size_bytes = 1259
sha256_encrypted = "a7a7a7a7a7a7a7a7a7a7a7a7a7a7a7a7a7a7a7a7a7a7a7a7a7a7a7a7a7a7a7a7"

[[files]]
position = 8
type = "data_slice"
size_bytes = 1296
sha256_encrypted = "a8a8a8a8a8a8a8a8a8a8a8a8a8a8a8a8a8a8a8a8a8a8a8a8a8a8a8a8a8a8a8a8"

[[files]]
position = 9
type = "data_slice"
size_bytes = 1333
sha256_encrypted = "a9a9a9a9a9a9a9a9a9a9a9a9a9a9a9a9a9a9a9a9a9a9a9a9a9a9a9a9a9a9a9a9"

[[files]]
position = 10
type = "seal_marker"
"##;

/// The short seal's bytes for a fixed input (`volume-format-v2.md` §4.1): the
/// header, the `[seal]` table in its ruled order with `requires =
/// ["short-seal"]` and `ended_after = K` after `file_count = K+2`, and the
/// embedded copy of entries 0 to K with the seal's own bare entry at K+1.
/// `sealed_at` is normalised; everything else is pinned byte for byte.
#[test]
fn short_seal_bytes_are_pinned_except_sealed_at() {
    let plan = golden_seal_plan();
    let k = GOLDEN_ENDED_AFTER as usize;
    let seal = generate_short_seal_marker(
        "GOLD01",
        GOLDEN_ENDED_AFTER,
        GOLDEN_FRONT_INDEX_SHA256,
        &plan[..=k],
    );
    let actual = normalise_sealed_at(&seal);
    if SHORT_SEAL_EXPECTED == "__SHORT_SEAL__" {
        eprintln!(
            "CAPTURE short seal sha256 = {}\nCAPTURE short seal =\n{actual}CAPTURE end",
            sha256_hex(&actual)
        );
    }

    // Exactly one sealed_at line, the fixed-width RFC 3339 second
    // (`YYYY-MM-DDTHH:MM:SSZ`, 20 bytes): the build sizes the seal with a
    // placeholder, and a wider stamp would make the seal longer than it.
    let stamps: Vec<&str> = seal
        .lines()
        .filter(|l| l.starts_with("sealed_at = "))
        .collect();
    assert_eq!(stamps.len(), 1, "{stamps:?}");
    let stamp = stamps[0]
        .trim_start_matches("sealed_at = \"")
        .trim_end_matches('"');
    assert_eq!(stamp.len(), 20, "{stamp}");
    assert!(
        stamp.ends_with('Z') && stamp.as_bytes()[10] == b'T',
        "{stamp}"
    );

    assert_eq!(
        actual, SHORT_SEAL_EXPECTED,
        "the short seal's bytes changed (sealed_at excluded). That is an on-tape format \
         change and a CTO decision; do not re-pin without one."
    );
    assert_eq!(
        sha256_hex(&actual),
        SHORT_SEAL_SHA256,
        "the short seal's hash moved with its bytes"
    );
}

/// What the pinned bytes must mean, so a re-pin of the text above cannot
/// carry a seal the readers reject: it parses by rules S1-S6, states K and
/// K+2, classifies short against the plan's twelve, its copy is File 3's
/// entries 0 to K plus its own bare entry, its header is the full seal's word
/// for word, and it is no longer than the full seal (the placeholder the
/// build sizes, §4.1 "Length").
#[test]
fn the_pinned_short_seal_parses_short_and_fits_the_full_seals_room() {
    let plan = golden_seal_plan();
    let k = GOLDEN_ENDED_AFTER as usize;
    let short = generate_short_seal_marker(
        "GOLD01",
        GOLDEN_ENDED_AFTER,
        GOLDEN_FRONT_INDEX_SHA256,
        &plan[..=k],
    );
    let full = generate_seal_marker(
        "GOLD01",
        plan.len() as i32,
        GOLDEN_FRONT_INDEX_SHA256,
        &plan,
    );

    let parsed = format::parse_seal_marker(&short).expect("the short seal parses (S1-S6)");
    parsed.check_parse_rules().expect("and keeps every rule");
    assert_eq!(parsed.volume, "GOLD01");
    assert_eq!(parsed.layout_version, 2);
    assert_eq!(parsed.requires, vec!["short-seal".to_string()]);
    assert!(parsed.requires_short_seal());
    assert_eq!(parsed.file_count, GOLDEN_ENDED_AFTER + 2, "S1");
    assert_eq!(parsed.ended_after, Some(k as u64), "S1");
    assert_eq!(parsed.front_index_sha256, GOLDEN_FRONT_INDEX_SHA256);
    assert_eq!(
        parsed.ending(plan.len() as u64),
        format::SealEnding::Short {
            ended_after: k as u64
        }
    );
    // The copy: File 3's entries 0 to K, then the seal's own bare entry.
    assert_eq!(parsed.files.len(), k + 2, "S4");
    for (got, want) in parsed.files.iter().zip(&plan[..=k]) {
        assert_eq!(got.position, want.position);
        assert_eq!(got.type_label, want.type_label);
        assert_eq!(got.size_bytes, want.size_bytes);
        assert_eq!(got.sha256_encrypted, want.sha256_encrypted);
    }
    let own = &parsed.files[k + 1];
    assert_eq!(
        (own.position, own.type_label.as_str()),
        (k as i32 + 1, "seal_marker")
    );
    assert_eq!(
        (own.size_bytes, own.sha256_encrypted.as_ref()),
        (None, None)
    );
    assert_eq!(
        parsed.files[k].type_label, "data_slice",
        "S5: it ends after a data slice"
    );

    // The full seal of the same plan: the same header, ended_after M-1.
    let header = |s: &str| s[..s.find("[seal]").expect("a [seal] table")].to_string();
    assert_eq!(header(&short), header(&full), "one header for both kinds");
    let parsed_full = format::parse_seal_marker(&full).expect("the full seal parses");
    assert_eq!(parsed_full.ended_after, Some(plan.len() as u64 - 2));
    assert_eq!(
        parsed_full.ending(plan.len() as u64),
        format::SealEnding::Full
    );
    assert!(
        short.len() <= full.len(),
        "a short seal is never longer than the full seal's placeholder: {} > {}",
        short.len(),
        full.len()
    );
}

// ── a span tape's seal (#478, ADR-0012 items 124 and 126) ───────────────────

/// The span variant of the pin above (`volume-format-v2.md` §4.1, "From 1.7.0
/// the seal of a span tape states `requires = ["short-seal", "spanning"]`"): a
/// short seal stating both features, cut from the same golden plan, parses
/// short by rules S1-S6 and fits a full placeholder stating both, which is how
/// `build` sizes a span tape's seal. Every byte of it but the `requires` line
/// is the pinned short seal's (item 126: nothing new is readable without a
/// key), and the pinned bytes themselves do not move.
#[test]
fn a_span_tapes_short_seal_parses_short_and_fits_a_span_placeholder() {
    use tapectl::volume::layout::{
        generate_seal_marker_requiring, generate_short_seal_marker_requiring, REQUIRES_SPAN,
    };
    let plan = golden_seal_plan();
    let k = GOLDEN_ENDED_AFTER as usize;
    let short = generate_short_seal_marker_requiring(
        "GOLD01",
        GOLDEN_ENDED_AFTER,
        GOLDEN_FRONT_INDEX_SHA256,
        &plan[..=k],
        REQUIRES_SPAN,
    );
    let full = generate_seal_marker_requiring(
        "GOLD01",
        plan.len() as i32,
        GOLDEN_FRONT_INDEX_SHA256,
        &plan,
        REQUIRES_SPAN,
    );

    // The one line that differs from the pin, spelled as §1.2 rules it.
    let normalised = normalise_sealed_at(&short);
    assert_eq!(
        normalised.replace(
            "\nrequires = [\"short-seal\", \"spanning\"]\n",
            "\nrequires = [\"short-seal\"]\n"
        ),
        SHORT_SEAL_EXPECTED,
        "a span tape's short seal is the pinned one but for its requires line"
    );

    let parsed = format::parse_seal_marker(&short).expect("the span short seal parses (S1-S6)");
    parsed.check_parse_rules().expect("and keeps every rule");
    assert_eq!(
        parsed.requires,
        vec!["short-seal".to_string(), "spanning".to_string()]
    );
    assert!(parsed.requires_short_seal());
    assert_eq!(parsed.refusal(), None, "1.7.0 knows both features");
    assert_eq!(parsed.file_count, GOLDEN_ENDED_AFTER + 2, "S1");
    assert_eq!(parsed.ended_after, Some(k as u64), "S1");
    assert_eq!(
        parsed.ending(plan.len() as u64),
        format::SealEnding::Short {
            ended_after: k as u64
        }
    );
    assert_eq!(parsed.files.len(), k + 2, "S4");
    assert_eq!(parsed.files[k].type_label, "data_slice", "S5");

    let parsed_full = format::parse_seal_marker(&full).expect("the span full seal parses");
    parsed_full
        .check_parse_rules()
        .expect("and keeps every rule");
    assert_eq!(parsed_full.requires, parsed.requires);
    assert_eq!(
        parsed_full.ending(plan.len() as u64),
        format::SealEnding::Full
    );
    let header = |s: &str| s[..s.find("[seal]").expect("a [seal] table")].to_string();
    assert_eq!(header(&short), header(&full), "one header for both kinds");
    assert!(
        short.len() <= full.len(),
        "a span tape's short seal is never longer than its full placeholder: {} > {}",
        short.len(),
        full.len()
    );
}
