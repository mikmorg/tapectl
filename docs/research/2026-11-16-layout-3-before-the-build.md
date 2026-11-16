# Layout 3 before the build: the three measurements, and what the design leans on

**Asked for** by the CTO on 2026-11-14, after the design of tapectl 2.0.0 was ruled (ADR-0012
items 235 to 263) and before any go: "Review the trickiest areas of design and do some deep
thinking or research if necessary now before we continue. Keep all of your findings in GitHub
issues or document files as recommended."

**What this is.** Measurements and reasoning, no code, no spec, no pin. It runs Phase 0 of #554
(the three things item 253 was ratified without) and tests what the ruled design leans on
hardest: GNU tar as the writer, a tar program as the heir's reader, and the file table of item
254. Every fact is tagged **MEASURED** (the command and versions are given) or **REASONED**.
Findings that change a ruled item, a recorded reading or the plan are filed as issues (§9).

**Tools.** GNU tar 1.35 (this host), GNU tar 1.30 (`debian:10` container, the production
host's version), GNU tar 1.34 (`debian:12`), bsdtar 3.6.2 (libarchive 3.6.2, `debian:12`),
busybox 1.36.1 tar, GNU coreutils 9.4, uutils coreutils 0.10.0 (the Ubuntu 26.04 image, as uid
1000), busybox head and tail, age 1.1.1, Python 3 `tarfile`. Files were megabytes unless a
size is given. Nothing touched a tape, the repository's code or the production host.

**Who ran what.** A sub-agent ran, and the coordinator did not repeat: GNU tar 1.30's fidelity
and the cross-reading of §3, bsdtar as root, the option-like names, the files that change
while they are read, the special files and the xattr and ACL records of §5. The coordinator
ran: the scale test, bsdtar as an ordinary user, everything in §4, the determinism, `fadvise`
and hard-link probes of §5, §6 and §7's sizes. §8 is an independent reader's, read and not
run. Both reports are comments on #562, whole.

**The create command under test**, as item 253 has it:

```
(cd TREE && find . -mindepth 1 -print0 | LC_ALL=C sort -z |
  tar --create --format=pax --no-recursion --null -T - --xattrs --acls \
      --pax-option=delete=atime,delete=ctime -f -)
```

**One probe was cut short.** A sub-agent piped a 9 GiB stream out of a container; docker logs a
container's standard output to disk, and /scratch fell to 3.6 GB before it was stopped. The
large-member cases through bsdtar and busybox, and repeat tries of two timed cases, were not
finished and are listed in §10. Containers after that ran with `--log-driver none` on data
under 10 MB.

## 1. The result

- **Nothing measured reopens item 253.** GNU tar 1.30 writes and reads everything the test
  asked of 1.35 (§3). bsdtar reads the stream and restores xattrs, ACLs and nanosecond times,
  as root and as an ordinary user (§3). A 96 GiB member came back identical through 96 cut,
  encrypted and rejoined pieces (§3). Scale is shown that far and no further: one large member
  of zeros and 50,100 small ones, not hundreds of gigabytes of real files.
- **One recorded default should change, and it goes back to the CTO: readers should not
  zero-fill a lost slice.** A zero-filled slice makes every tar tested stop there and exit 0
  with the later files missing; with `--ignore-zeros` GNU tar goes on, and extracts the
  contents of any tar file that sits inside the unit as if they were the unit's own files.
  Feeding tar exact byte ranges from item 254's file table, joined at member boundaries,
  restores everything that is restorable with exit status 0 on every tar tested (§4, #556).
- **tar's exit status is not enough for the writer.** GNU tar 1.35 exits 0, silently, when a
  file is replaced or deleted while it is being read. tapectl's own check of each file before
  and after stays (§5).
- **Rules for the Layout 3 format document come out measured** (§5, §6; #562): `--null` and
  what it protects against; the exit statuses; the pax header name; what "where a member
  starts" means. Two more change what is archived (owner names, sockets) and one changes ruled
  heir behaviour (the work directory), so they are asked (#561).
- **An independent reader found twelve places where the ruled design has no rule yet or a
  recorded reading does not hold** (§8). The CTO's word is owed on eight questions, in six
  issues (§9, questions 26 to 33).

## 2. What was asked of each tool

| | GNU tar 1.35 | GNU tar 1.30 | bsdtar 3.6.2 | busybox 1.36.1 tar |
|---|---|---|---|---|
| writes the stream | yes | yes | not asked | not asked |
| reads an undamaged stream | yes | yes | yes | yes |
| restores xattrs, ACLs, nanosecond mtime | yes | yes | yes with `--xattrs --acls -p`, as root and as uid 1000; a plain `-x` restores the time only | **no, silently** (content, names, links only) |
| a zero-filled slice, plain `-x` | stops, **exit 0** | stops, **exit 0** | stops, **exit 0** | stops, **exit 0** |
| byte ranges from the file table | exit 0, all correct | exit 0, all correct | exit 0, all correct | exit 0, all correct |

## 3. Phase 0: the three measurements of item 253

**GNU tar 1.30.** MEASURED (`debian:10`; attributes set and compared on the host, since the
image has no `setfacl` and its package archive is retired). Create and extract in the
container, as root and as uid 1000, of a tree with: a `user.*` xattr, a file ACL, a directory
with access and default ACLs, a hard link, a member name of 514 bytes, a name containing a
newline, an mtime of `03:04:05.123456789`, a symlink, an empty directory. `diff -r
--no-dereference` identical; `getfattr`, `getfacl`, `stat` identical; the two names of the hard
link share an inode. A 9 GiB sparse member followed by a small one, through a pipe: listed at
9,663,676,416 bytes, its sha256 equal to that many zero bytes, the small member after it
intact. Streams made by 1.35 are read by 1.30 and the reverse, with every attribute.

**bsdtar as a reader.** MEASURED (3.6.2, root, `bsdtar -x --xattrs --acls -p`, and with `-p`
alone): reads the streams of 1.30 and of 1.35, exit 0, tree identical (long name, newline name,
symlink, hard link, empty directory), the xattr, the access ACL and the default ACL restored,
mtime to the nanosecond. The small member after a 9 GiB one was read correctly. As uid 1000
with the same flags: exit 0, tree identical, the xattr, both ACLs and the nanosecond time
restored. As uid 1000 with a plain `bsdtar -x`: content and the nanosecond time, no xattr, no
ACL. Not measured: the 9 GiB member's hash through bsdtar.

**Scale.** MEASURED (GNU tar 1.35, age 1.1.1; pipe-only, under the build lock in a window the
broker gave; /scratch unchanged): one 96 GiB member between two small ones,
`tar --create ... | split -b 1G --filter='age -r KEY | age -d -i KEY' | tar -x -O big.bin |
sha256sum`. Every stage exited 0. The member's sha256 equals the source's; the member after it
came out intact. 745 seconds for the pipeline. The reference reader of §6 placed the last
member's start at byte 103,079,218,688, from a pax `size` record. MEASURED separately: 50,100
members of 100 bytes make a 102.6 MB stream in 2 seconds, 1,951 bytes of overhead a member.
**What this does not show:** hundreds of gigabytes of real files in millions of members at
once; the 1,000,000-file figures below are multiplied from 50,100. Phase 0's scale line in
#554 is ticked as partial for that reason.

## 4. The reader's primitive

Item 241 records a limit ("a lost slice costs the files in it only when a reader zero-fills it
to its exact size") and #554 carried "every reader zero-fills a lost slice to its exact size"
as a default not objected to. Both came from one GNU tar run with `-i`. Measured wider, the
zero-fill is the wrong primitive.

**E1. A zero-filled slice, plain `tar -x`.** MEASURED (six 1 MB files, bytes 3 MiB to 4 MiB
replaced by zeros): GNU tar 1.35, GNU tar 1.30, bsdtar 3.6.2 and busybox tar each extract the
files before the hole, stop, and **exit 0**. Two zero blocks are tar's end-of-archive mark.
The two files after the hole are silently absent, and the file that ends in the hole is written
with other bytes than the source's (compared, GNU tar 1.35 and busybox).

**E2. With `--ignore-zeros`.** MEASURED (GNU tar 1.35): exit 2, the file after the hole is
extracted. So the zero-fill needs `-i`, which bsdtar and busybox were not shown to have.

**E3. `-i` and a tar file inside the unit.** MEASURED (GNU tar 1.35; a unit of `a-first.bin`,
`b-old-backup.tar` (an ordinary tar of four files) and `c-last.bin`; a 64 KiB hole over the
headers of `b-old-backup.tar`): `tar -x -i` extracts `payload/inner1.bin`, `payload/inner3.bin`
and `payload/INNER-NOTE.txt` into the destination. They are members of the inner archive, not
of the unit. A resynchronising reader cannot tell a member header from file content that looks
like one, and backups inside backups are ordinary in a family archive.

**E4. Byte ranges from the file table.** MEASURED. A reference reader (§6) lists each member's
start and end in the stream. Feeding tar exactly the bytes of whole members, with nothing
between them:

| Request | What is fed | GNU 1.35 | GNU 1.30 | bsdtar | busybox |
|---|---|---|---|---|---|
| the whole unit | one range, without the end-of-archive blocks | exit 0, all files identical | same | same | same |
| a unit with slice 4 lost | every member that does not touch the lost bytes, joined end to start | exit 0, all but the two touched files, identical | same | same | same |
| one file | its range | exit 0, identical | same | same | same |
| a folder | the folder's own member and the run of members under it | exit 0, identical | same | same | same |
| a member name of 438 bytes; a name with a newline | its range | exit 0, identical | same | same | same |
| a hard link whose target's member was dropped, the link kept | the rest | **fails**, exit 2 ("Cannot hard link"); the other files identical | exit 2 | exit 1 | exit 1 |
| the same, the link dropped with its target | the rest | exit 0 | exit 0 | exit 0 | exit 0 |
| a run that ends inside a member | bytes up to the middle of a file | exit 2 ("Unexpected EOF"); the earlier files identical; the file in flight left at 399,872 bytes | exit 2, same | exit 1; the file left at full size with other bytes | exit 1; left at 400,000 bytes |

The members that touch a lost slice are known from the table before any byte is read, so the
damaged-file list of item 255 is exact. The exit status is 0 exactly when whole members were
fed and every link had its target; a run that ends inside a member exits non-zero and leaves
that member on disk partial or wrong, for the per-file check to catch.

**E5. Cutting a range out of a pipe.** MEASURED. `... | tail -c +START | head -c LENGTH | tar
-x`, with the stream arriving on a pipe, gives the same results under GNU coreutils, busybox
head and tail, and uutils 0.10.0 (Ubuntu 26.04, uid 1000). But two `head -c` in a row on ONE
pipe do not: busybox `head -c` reads ahead, and the second reader gets other bytes (GNU and
uutils are exact). So each range is its own pipeline.

**E6. One file from only its slices.** MEASURED (GNU tar 1.35, busybox): given only the slices
that hold a member and its offset in the first, the file is identical. Fed to the end of the
last slice, tar exits 2 (GNU) and 1 (busybox), because the bytes after the member end inside
the next one; cut to the member's end, as in E4, it exits 0. With E4 this is #411's saving,
kept without any tool's slice arithmetic.

**REASONED.** The by-hand path of File 1 has two cases. Nothing damaged: decrypt the slices in
order into one `tar -x`. Something damaged: the table is a text file, and the ranges are an
`awk` line over it. The zero-fill has no place in either, and File 1 should say why a reader
must not reach for `-i`.

## 5. GNU tar as the writer

**Names that look like options.** MEASURED (1.30 and 1.35; files named `--checkpoint=1`,
`--checkpoint-action=exec=touch INJECTED`, `-T`, `--files-from=x`,
`--use-compress-program=sh`). With `--null -T -`: exit 0, nothing executed, all five archived
as members, with or without a leading `./`, on both versions. With a newline-separated list
and no `./`: 1.35 exits 2 and **acts on `--files-from=x`** (it read the file `x` as a second
list); 1.30 exits 64. `--verbatim-files-from` is accepted by both. *So: the list is
NUL-separated, always; `./`-prefixed names and `--verbatim-files-from` cost nothing more.*

**A file that changes while it is read.** MEASURED (a 2 GiB sparse file, changed when the
stream had passed 700 MB; one try each):

| Change | GNU tar 1.35 | GNU tar 1.30 | In the stream |
|---|---|---|---|
| truncated | exit 1, "File shrank by N bytes; padding with zeros" | exit 1, same | the header's size is honoured, the rest is zeros, the next member follows |
| appended to | exit 1, "file changed as we read it" | exit 1 | the header's size; the new bytes are not in it |
| replaced by another inode (`mv`) | **exit 0, no message** | exit 1 | the old inode's bytes |
| deleted | **exit 0, no message** | exit 1 | the old inode's bytes |
| a listed name that does not exist | exit 2, "Cannot stat" | exit 2 | no member |
| mode 000, as an ordinary user | exit 2, "Cannot open: Permission denied" | exit 2 | no member |

*So: exit 1 and exit 2 both refuse the stage. Exit 0 does not prove the files stood still.
Item 253 takes each file's sha256 from the stream, so a file that changed under tar would be
recorded with the hash of whatever was read; tapectl's own comparison of each file before and
after (device, inode, size, mtime, ctime), which it makes after dar today, stays and is what
decides.* REASONED from the two exit-0 rows.

**Special files.** MEASURED (1.30, 1.35): a socket is "ignored" with a warning, not stored,
exit 0. A FIFO and (as root) a character and a block device node are stored as members with no
data. *So: a socket in the list gives a list entry with no member, and the member check would
call a clean run short. Leaving sockets out changes what is archived, so it is asked (#561).*

**xattrs and ACLs.** MEASURED (1.30, 1.34): `--xattrs --acls` stores an ACL twice, as
`SCHILY.acl.access` (text) and as `SCHILY.xattr.system.posix_acl_access` (the raw xattr).
`--acls` alone stores the text form only; `--xattrs` alone the raw form only. Both readers
that restore ACLs restored the same ACL. NOT MEASURED: `trusted.*` and `security.*` (the
container lacked the capability).

**Determinism.** MEASURED: on 1.30 two runs over the same tree differ: the pax header's name
carries tar's process id (`./PaxHeaders.12/d`, `./PaxHeaders.16/d`). With
`--pax-option=exthdr.name=%d/PaxHeaders/%f` two runs of 1.30 are identical; 1.35 is identical
without it. The streams of 1.30 and 1.35 still differ from each other with that option and
`--numeric-owner` (a header checksum; the field behind it was not found). Without
`--numeric-owner` the user and group NAMES of the host's passwd go into every header (the
operator's login name on this host, empty in the container). *So: the format document fixes
`exthdr.name`; `--numeric-owner` changes what is archived and is asked (#561); and no pin or
fixture may hold data-zone bytes made by one tar version against another's. A Copy is identical
content, judged file by file, as it is today across two stagings.*

**No `fadvise`.** MEASURED (`strace -f -e trace=fadvise64`, 1.35): no call on create. The
seccomp filter of #441 has nothing to do under tar.

**Hard links.** MEASURED: with two names of one file, the name listed first carries the data
and the second is a link member with none; with only the second name listed it is an ordinary
member with the data. *So: excluding one name of a pair is safe, and item 256's dependence is
exactly "on the name listed first".*

**Sparse files.** MEASURED (1.30, 1.35): stored at full size (the 9 GiB and 96 GiB members
were holes). A reader with `-S` makes them sparse again.

**Sizes.** MEASURED: about 1,951 bytes of stream for each small file's headers and padding
(100 MB for 50,100 files of 100 bytes).

## 6. tapectl reading the stream as it passes

**Where a member starts.** MEASURED (GNU tar 1.35 output; a reference reader in 40 lines of
Python, checked by extracting each member from its start alone): in this configuration every
member is preceded by a pax extended header (512 bytes of header and 512 of records, more for
a long name or an ACL), and the member's own header follows. A member *starts at its first
extension header*: started at its own header instead, a member loses its long name, its
nanosecond time and its size when that is over 8 GiB. The reader must take the size from the
pax `size` record when there is one (the 96 GiB member), and treat links, directories and
devices as having no data.

**The `tar` crate.** REASONED from its source (0.4.46: `Entry::raw_header_position` is "the
starting position of the header of this entry", the 512-byte block; `Entries::raw(true)`
returns extension entries unprocessed): the default iterator gives the member's own header,
not its first extension header. tapectl either iterates raw and keeps the first extension
entry's position, or reads blocks itself. Either way the format document defines `start`, and
a test restores a long-named member from its `start` alone.

**The member check.** REASONED: tar is given exactly the snapshot's list, so the stream's
members must be that list in that order with those sizes. Anything else is a refusal. With the
before-and-after check of §5 this replaces #464's comparison with dar's catalogue.

## 7. The file table

**It is part of the plan.** REASONED from `volume-format-v2.md` §4.2: the envelopes are
written before the slices, so on a tape that ended short the table lists files of cut units
and of slices after `ended_after`. §4.2 already rules this for the MANIFEST and the index
("Everything written before the slices is the plan ... The seal withdraws those claims"), and
R1 to R4 carry over unchanged: a reader learns `ended_after` first and never restores a cut
unit. The table adds a claim of the same kind, not a new kind.

**Its size.** MEASURED on 50,100 rows: 121 bytes a row as tab-separated text (kind, size,
sha256, start, end, path); about 121 MB for a million files, in each tenant envelope and its
backup copy. The heir's script decrypts an envelope into its work directory, which is
`${TMPDIR:-/tmp}` today: at that scale a memory-backed `/tmp` fills, which is finding A1 of
the review (#520) at a larger size.

**Its order.** REASONED: rows in stream order, which is the list's bytewise order, let a
reader compute E4's runs in one pass. A folder's own member is not next to its contents
(`d`, `d-x`, `d.txt`, `d/...` sort in that order: MEASURED in E4's tree), so a folder is two
ranges.

**Absolute offsets.** REASONED: every slice but the last holds exactly `slice_size` bytes of
the stream, so a member's place can be stored as its start and end in the stream, and a reader
divides. The MANIFEST already gives each slice's plaintext size.

## 8. Interplay: the ruled design on the system that exists

An independent reader, given items 235 to 263, the Layout 2 format and session documents and
#555's measurements and none of the coordinator's suspicions, looked for a state, a command
and a wrong or undefined outcome in each of: the early warning and the short seal; ranges,
resume and the retry; everything that moves slices without re-staging; counting and consent;
the acceptance run; and contradictions inside the items. Everything in this section is
**REASONED** from the documents and the code at `bdb4493f`; nothing was run. Its whole report
is a comment on the issue that carries §9's rules.

**What holds.** W1 to W9 act on blocks, filemarks, entries and the seal and read no payload.
The seal grammar and the chain carry positions, sizes and ciphertext hashes only. A resume
re-sends frozen bytes, and the envelopes, the table included, are frozen with the Layout. The
retry after a refusal rewrites a tape file from its planned start. A spannable unit is staged
whole before its first tape, so its table is known in time. The next cartridge of a span is
accepted by a slice's sha256 under its seal, which also refuses a cartridge of another
staging. Copies are already counted per stage set and per slice, and no audit, policy or
release code reads the dar catalogue. The table depends on no tape position, so
`volume read-slices`, compaction, `volume rewrite` and a continuation can reuse a staging's
table unchanged: the gap below is where it is kept, not whether it stays valid.

**What does not hold, or has no rule.**

- **I1. The file table has no ruled home between tapes** (`interplay-1`). Item 254 puts it in
  the tenant envelopes and says nothing of where it is kept once staging ends. The dar
  catalogue it replaces is a file under `<home>/catalogs/`, and that store already produced
  #473 (a copy written after a rebuild carried no catalogues). Five routes write a tape
  without a table, or leave a staging without one: (a) `catalog rebuild` takes catalogues from
  the OPERATOR envelope (`src/volume/rebuild.rs:2424-2470`), the on-tape `catalog.db` has file
  rows per snapshot and no place for a staging's offsets (`src/db/ontape_catalog.rs:108-113`),
  and `volume read-slices` opens no envelope (`src/volume/write.rs:7887-7889`); (b) `db
  backup` copies the database, not the home's files, so a restored catalog with staging intact
  writes copy 2 with no table; (c) `build` packs a catalogue when it finds one and nothing
  otherwise, and the write only warns (`src/volume/build.rs:1102-1119`,
  `src/volume/write.rs:9839-9848`), which was tolerable for a convenience and is not for the
  heir's only per-file judge; (d) a tenant envelope unreadable in both copies is rebuilt from
  the operator envelope alone (`src/volume/rebuild.rs:944-970`); (e) slices are released tape
  by tape and no item says the table outlives the release. `volume rewrite` may erase the only
  copy first, which would make a missing table permanent.
- **I2. The table must say where a member ends** (`interplay-2`). Item 254 says "where it
  starts". A file whose header is in the last slice a short or range tape holds, with data in
  the next, passes a test on its start and restores truncated. "This tape holds F" must mean
  every slice from F's start to its end is at or before `ended_after`.
- **I3. On a short tape the table is a plan** (`interplay-5`), and item 260's "a source for the
  files whose slices it holds" collides with R4 for a cut unit, whose slices before the end are
  present and hash correctly. §4.2's list of withdrawn claims does not name the table.
- **I4. What a range tape's table lists is undefined** (`interplay-6`): only its own run, or the
  whole staging. A rebuild from one tape of a span needs the whole.
- **I5. Nothing restores what a range holds when the tape with slice 1 is gone**
  (`interplay-7`). `--restore --unit` on a later tape refuses before any slice
  (`volume-format-v2.md` §4.4), a recorded reading of 2026-11-14 kept that, and item 260 serves
  only a repair driven by a damaged-file list that a restore must first have written. The one
  prompt has no answer for a cartridge the heir does not have.
- **I6. Damage found after part of a slice is in tar** (`interplay-8`). The heir's pipeline is
  one pass; `age` fails at the first bad chunk after passing the chunks before it. No rule says
  what the script does with a slice partly delivered.
- **I7. The refusal of a diverging second staging is not carried by name** (`interplay-9`).
  Today a re-stage whose hash for any file but the unit dotfile differs from the version's
  recorded one is refused (`src/staging/validate.rs:1079-1088`). Items 253 and 254 take the hash
  from the stream and check a tape against its own table, and neither says the refusal stays.
  Without it two tapes from two stagings could each pass their own check with different bytes
  for one path of one version, and both count as copies.
- **I8. A hard link's row is underspecified** (`interplay-10`): what its sha256 is (no bytes of
  it go to tape), whether the check hashes the link name, the order of a repair that lists both
  names, and a link whose target is on an earlier cartridge.
- **I9. A sealed Layout 2 cartridge in the drive** (`interplay-11`). The contact check and
  `cartridge erase` decide what a cartridge is by parsing File 0 and the seal
  (`src/volume/session.rs:990-1087`, `src/volume/erase.rs:496-567`). A reader that refuses
  `layout_version = 2` may no longer parse them; then either `--force` overwrites a sealed
  volume that ADR-0003 protects, with no typed label, or the cartridge is foreign, or it is
  refused. Item 245 covers the two cartridges here; no item covers the rule.
- **I10. The acceptance run as the coordinator read it cannot be repeated, and its kit points
  nowhere** (`interplay-3`, `-4`, `-12`, `-13`). The reading of item 263 has an untagged
  candidate write the cartridges and the tag follow a passing drill. But: `first-run.sh
  --upgrade` installs only an annotated tag newer than the installed release
  (`scripts/first-run.sh:1077-1094`), so a fixed candidate can come only by another fresh
  start; on the first run's catalog the two cartridges are the only copies and neither can be
  erased or retired (`src/volume/erase.rs:516-520`, `src/policy/coverage.rs:2388-2413`); the
  Heir Kit and File 1 name `v2.0.0` and a release download that CI builds only on a tag
  (`src/crypto/escrow_kit.rs:92-103`, `src/volume/layout.rs:2144-2145`,
  `.github/workflows/release.yml:11-13`); the binary that wrote the tapes reports a version
  string of an untagged build for good; and item 243 ends the authorization "when the release
  is installed", which by the reading is before the first cartridge is written, while a
  drill's likeliest finding is an on-tape change. Items 238 and 262 gate production on #548,
  which is open when the run it gates starts.
- **I11. Two readings removed ruled defaults** (`interplay-15`). "A restore across cartridges
  streams" drops item 132's default (each tape's range kept in scratch as ciphertext) and item
  88's choice between that and the stream. The amendment's list of what is superseded does not
  name them. A reading cannot remove a ruling.
- **I12. The short seal is no longer a feature** (`interplay-14`, `-16`). Item 240 says §4.1 and
  §4.2 "do not change"; item 258(a) makes the short seal part of the layout with an empty
  `requires`. Rule S2 and §1.2's rule 6 are keyed on the `short-seal` entry, the seal grammar
  states `layout_version = 2`, and §1.7's `may_end_early` lines are not mentioned. §1.2's rules
  3 and 5 (a new layout bumps the magic as well as the version; a v3 File 0 carries no bare
  integers a v2 script could follow) are not named either.

## 9. What follows

**For the CTO** (each is an issue labelled `needs:cto`; the recommendation is after the arrow).

26. **The reader's primitive** (§4; #556). The recorded default, "every reader zero-fills a lost
    slice", and the wording of items 241 and 255 came from one GNU tar run. → Readers feed tar
    whole members by byte range from the file table, joined at member boundaries. No reader
    zero-fills, and none uses `--ignore-zeros`.
27. **The file table's home** (I1; #557). → It is kept in the catalog database, so that `db
    backup`, the Heir Kit's catalogue and every tape's operator `catalog.db` carry it beside the
    tenant envelope's copy; it outlives the release of its slices; and a write refuses, by
    name, a unit whose table it cannot supply.
28. **A tape that holds part of a spanning unit, alone** (I5; #558). → A restore may start on
    any tape of a span and takes the files that tape holds whole; at the prompt for the next
    cartridge the heir may answer that they do not have it, and the restore goes on and lists
    the rest. This changes §4.4's "where a restore starts".
29. **The restore across cartridges in the heir's script** (I11; #558). → It streams, with the
    one prompt; item 132's scratch default and item 88's choice are withdrawn. Asked because
    the reading of 2026-11-14 could not withdraw them.
30. **The acceptance run's order** (I10; #559). → `v2.0.0` is tagged when the build is complete
    and proven on test cartridges. That release is installed by the fresh start, CI builds its
    static binary, and the Heir Kit carries it. It writes both cartridges and the drill runs.
    Until a drill passes, the run is not production for items 238, 248 and 262. If it passes,
    the cartridges are production and the on-tape authorization ends there. If it fails, they
    are test articles; the fix is a new release, never a re-pushed tag (#546), and it moves the
    minor if on-tape bytes move; and the run is repeated by another fresh start, which mints
    new keys and discards the first kit. Tagging publishes a release binary, so a failed drill
    leaves a published 2.0.0 that never held production.
31. **A Layout 2 cartridge in the drive** (I9; #560). → 2.0.0 knows a tapectl volume of another
    layout when it meets one: it reads its label, never writes over it without `cartridge
    erase` and the typed label, and reads nothing else from it. That is consent, not
    compatibility.
32. **What is archived** (§5; #561). Owner names: → `--numeric-owner`, so only numeric ids are
    stored and the stream does not depend on the host's passwd. Sockets: → the snapshot leaves
    them out of every unit and says so, since tar skips them at exit 0.
33. **The heir's work directory** (§7; #561). → It is made inside the destination by default,
    and `TMPDIR` no longer decides it. This amends items 71 to 75 for Layout 3.

**Rules for the Layout 3 format document** (#562). These are mechanics: each follows from a
ruled item and a measurement or an anchor above, and none changes what is archived or a ruled
behaviour. They are inputs to Phase 1 of #554.

- *The writer.* The list is NUL-separated with `--null` and `--verbatim-files-from`, names
  prefixed `./` (§5). `--pax-option=exthdr.name=%d/PaxHeaders/%f` is fixed, and no pin or
  fixture holds data-zone bytes across tar versions (§5). An ACL is stored once. Exit 1 and
  exit 2 refuse the stage; tapectl's own check of every file before and after decides,
  whatever tar says (§5). The seccomp filter of #441 goes (§5). A staging whose stream hash for
  any path but the unit dotfile differs from the version's recorded sha256 is refused, as now
  (I7).
- *The table.* A row states where the member starts and where it ends, in bytes of the stream;
  a member starts at its first extension header (§6, I2). Rows are in stream order (§7). Every
  tape whose plan holds a slice of a staging carries that staging's whole table (I4). The table
  joins §4.2's list of what the seal withdraws; "this tape holds F" means every slice from F's
  start to its end is at or before `ended_after`; item 260 reaches a range and never a cut
  unit, for which R4 stands (I3). A hard link's row carries its target's sha256 and names the
  target; the check hashes the link name; a repair restores the target before the link; a
  link member is fed to tar only when its target's member is (I8, E4). `index/paths.tsv`
  states no sha256 for the unit dotfile.
- *The heir's script.* Each range is its own pipeline (E5). A slice that fails while it is
  being read ends that tar run; the next run starts at the first member after the slice; the
  per-file check decides which members that touch it are damaged (I6). MEASURED for the tar
  half (E4's last row); REASONED for the `age` half, which no test here drove.
- *The layout.* Rule S2, §1.2's rule 6 and §1.7 are restated without the `short-seal` entry;
  the magic and the layout version both move; a Layout 3 File 0 carries nothing a Layout 2
  script could follow (I12).
- *The plan.* The upgrade tool's harness has no 2.x predecessor until a second 2.x release
  exists.

**Confirmed, and not reopened:** item 253 (GNU tar writes the stream), item 240's writer's
half, item 254's table (with I1 to I4 as its missing rules), item 256 (hard links kept, at the
cost I8 lists), item 258.

## 10. Not measured

- The 9 GiB member's hash through bsdtar and through busybox (cut short).
- `age` failing in the middle of a slice while tar is reading (the `age` half of I6).
- Repeat tries of "replaced" and "deleted" on both tar versions (one try each).
- `trusted.*` and `security.*` xattrs.
- Which header field differs between 1.30 and 1.35 with numeric owners.
- Real files in the hundreds of gigabytes and millions of members in one unit.
- `--ignore-zeros` on bsdtar and busybox (E2 is GNU only).
- The operator's `age-keygen -y` check of the escrow paper (item 244), which is theirs.
