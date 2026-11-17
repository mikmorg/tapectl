# T0.7: the readers on a slice that ends at a member's start

**Asked for** by the plan's task T0.7 (`docs/design/v3-implementation-plan.md`), the measurement item 283 (ADR-0012) owed:
#567 measured the cut at member starts with GNU tar 1.35 and BusyBox only. This note adds GNU tar 1.30, GNU tar 1.34 and
bsdtar, an ordinary user and root, and the cases (a) to (e) of the plan. Date: 2026-11-17.

**What this is.** A measurement, no code of tapectl, nothing in the repository changed. Every fact is MEASURED.
The scripts are in `/scratch/t07/bin` (`gen.py`, `run_reader.sh`, `check.py`, `table.py`, `drive.sh`, `controls.sh`);
the raw tables are `/scratch/t07/table_main.txt` and `table_nohl.txt`, every reader's rc, stderr and summary are under
`/scratch/t07/data/res/` (and `data/nohl/data/res/`).

## 1. Verdict

**The decision rule, applied as written, is NOT met, so it says: report the table and stop; item 283 goes back to the CTO.**
GNU tar 1.30 and bsdtar each exit non-zero in case (c), the last slice alone (1.30: exit 2; bsdtar: exit 1; the other
readers too). The cause is one hard link, not the missing end-of-archive blocks: the last slice holds the member
`zz/hardlink-to-t0001`, whose first name `docs/t0001.txt` is in a slice that run did not extract, and every reader
refuses to make the link ("Cannot hard link to ... No such file or directory"). The other files of that slice come out
whole. On the same unit with the hard link removed (section 6, a supplement, not part of the rule) every reader exits 0 in
(a) to (d), as an ordinary user and as root, with no warning at all. The coordinator and the CTO decide whether the rule's
"each exit 0 in (c)" is meant to cover a hard link whose target is elsewhere (the plan already records that "a hard-linked
name depends on the slices of the name archived first").

Everything else the rule asks for held: GNU tar 1.30 and bsdtar exit 0 in (a), (b), the slice before the last in (c)
and (d), and restore every file outside a left-out slice, as user and as root. BusyBox, held to content only, restores
every file.

**Step (e) (D10): BY ITS OFFSETS.** By name works for an ordinary name and for a name with a newline on every reader,
at exit 0, exactly the named member and nothing else, provided the name is spelled as the stream spells it
(`./docs/t1528.txt`). It fails for the third name: a folder's own member without what is under it. GNU tar (with
`--no-recursion`) and bsdtar (with `-n`) restore exactly the folder; **BusyBox tar extracts the folder and everything
under it in its slice (127 entries), also with `--no-recursion`** (the flag is accepted and ignored on extract). So not
every reader, BusyBox included, restores exactly the named member for all three names: the script cuts the member out of
its slice with `tail -c +N | head -c M`. Also measured: a name spelled without the leading `./` (`docs/t1528.txt`) is "not
found" by GNU tar (all three) and BusyBox, and works only in bsdtar; so a script that names members must spell them
`./name` as the stream has them.

## 2. Method (repeatable)

Host: Linux 6.8.0-142-generic, `/scratch` ext4 (user xattrs and ACLs work), age 1.1.1, getfacl 2.3.2.

| reader | where | version |
| --- | --- | --- |
| GNU tar 1.30 | `debian:10-slim`, digest `sha256:bb3dc79fddbca7e8903248ab916bb775c96ec61014b3d02b4f06043b604726dc` | `1.30+dfsg-6+deb10u1` (apt as installed, Debian 10.13) |
| GNU tar 1.34 | `debian:12-slim`, `sha256:734a72b3d6142a71993b4bc580ca01d8ca687f77a2c422d96a961d3376b179bc` | `1.34+dfsg-1.2+deb12u1` |
| GNU tar 1.35 | this host's `tar` | `tar (GNU tar) 1.35` |
| bsdtar | `debian:12-slim` plus `apt-get install libarchive-tools` | `bsdtar 3.6.2 - libarchive 3.6.2`, package `3.6.2-1+deb12u5` |
| BusyBox tar | this host's `busybox` (the image was not needed) | `BusyBox v1.36.1 (Ubuntu 1:1.36.1-6ubuntu3.1)` |

Reader command lines (`run_reader.sh`): `tar --xattrs --acls -x -C DIR` for the three GNU tars; `bsdtar -x -p -C DIR`
(`-p` is needed for an ordinary user to get xattrs and ACLs); `busybox tar -x -C DIR`. The slices are concatenated on the
reader's standard input. Containers: `docker run -d --log-driver none -v /scratch/t07/data:/data IMAGE sleep ...`, then
`docker exec [--user 1000:1000]`; no data on a container's standard output; results are files in the mount. Host readers
as root run under `sudo`. Python was run `python3 -I`.

1. `python3 -I bin/gen.py /scratch/t07` builds #567's unit (2,400 text files, 24 files of 0.56 to 0.72 MB, three files of
   2.6, 1.1 and 5.3 MB, empty folders, a symlink, a hard link whose first name is far away, 48 files with a user xattr and
   a named ACL entry) and adds: a path of 316 bytes (`longpath/aaaa.../bbbb.../cccc.../file.txt`, components of 100
   bytes), a file named `nl/line1<LF>line2.txt`, a FIFO `zz/fifo`, and a folder `acl_dir` with a default ACL entry
   `u:2:rx`. The stream is made with D2's command line (`--format=pax --numeric-owner --xattrs --acls --no-recursion
   --null --verbatim-files-from --files-from=LIST --pax-option=exthdr.name=%d/PaxHeaders/%f,delete=atime,delete=ctime`,
   names `./`-prefixed, sorted bytewise, `LC_ALL=C`, `TAR_OPTIONS` and `POSIXLY_CORRECT` removed) on GNU tar 1.35: 43,878,400
   bytes, 2,446 members, 49 ACL records, one default-ACL record.
2. The cut is D3's: members that fit go in; one that does not fit and is at most 1 MiB ends the slice before it; a longer
   one gets a run of 1 MiB pieces; the stream's tail (the end-of-archive blocks and padding) is placed as a member. Result:
   55 slices (min 5,120 bytes, max 1,048,576), 47 groups (a group is one slice or one long member's run; three runs).
   The plain cut at 1 MiB (42 slices) is the negative control.
3. `bash bin/controls.sh`, then `bash bin/drive.sh` (every reader as user and as root), then
   `sudo python3 -I bin/check.py /scratch/t07 TAG` for each of the ten tags and `table.py`. `check.py` judges files whole by
   sha256 against the source, the hard link by inode equality, folders' mtimes against the source's nanoseconds, xattr and
   named ACL on the 48 marked files, the default ACL on `acl_dir`, the FIFO's type, the symlink.
4. The supplement: `NOHL=1 python3 -I bin/gen.py /scratch/t07/data/nohl`, and `ROOT=/scratch/t07/data/nohl
   CT_DATA=/data/nohl/data bash bin/drive.sh`, then `check.py` and `table.py` on it.

The cases, as the plan: (a) each group in its own run, in order, into one directory (47 runs); (b) all groups in order in
one run, group 18 (slice a026, the one with the most files whole, 134) left out; (c) the last slice alone (a054, holds the
tail) and the one before it alone (a053); (d) each of the three long members' runs joined, alone; (e) one member named, out
of the slice that holds it: `./docs/t1528.txt` (slice a026, which holds 134 files), `./nl/line1<LF>line2.txt` (a054), the
folder `./docs/deep` (a012, with files under it in the same slice), in both spellings. "Files whole" is judged against what
the slices in the run cover, a hard link counted only where its first name is covered too.

## 3. Controls

| control | result |
| --- | --- |
| positive: the whole stream, GNU tar 1.35 | exit 0, 2,431 of 2,431 files whole. Held. |
| negative: the plain 1 MiB cut read as (a) by GNU tar 1.35 | 42 of 42 runs fail (exit 2): "This does not look like a tar archive", "Unexpected EOF in archive", "rmtlseek not stopped at a record boundary". Held, as #567 measured. |

## 4. The table: reader by case, user and root

The two columns of users gave identical results in every cell below (same exit status, same warnings, same files, same
restored metadata), so one row stands for both. Main unit, 2,431 files. "Failed" counts runs with exit status not 0.

| reader | (a) 47 runs | (b) one run, a26 left out | (c) a053 alone | (c) a054 alone (the last) | (d) the three runs, each alone |
| --- | --- | --- | --- | --- | --- |
| GNU tar 1.30 | 0 failed, 2431/2431 | exit 0, 2297/2297 | exit 0, 1/1 | **exit 2**, 2/2 files, hard link refused | exit 0, 1/1 each |
| GNU tar 1.34 | 0 failed, 2431/2431 | exit 0, 2297/2297 | exit 0, 1/1 | **exit 2**, same | exit 0, 1/1 each |
| GNU tar 1.35 | 0 failed, 2431/2431 | exit 0, 2297/2297 | exit 0, 1/1 | **exit 2**, same | exit 0, 1/1 each |
| bsdtar 3.6.2 | 0 failed, 2431/2431 | exit 0, 2297/2297 | exit 0, 1/1 | **exit 1**, same | exit 0, 1/1 each |
| BusyBox 1.36.1 | 0 failed, 2431/2431 | exit 0, 2297/2297 | exit 0, 1/1 | **exit 1**, same | exit 0, 1/1 each |

Warnings: none in any cell except the last slice alone. GNU tar there: `tar: ./zz/hardlink-to-t0001: Cannot hard link to
'./docs/t0001.txt': No such file or directory` and `tar: Exiting with failure status due to previous errors`; bsdtar:
`./zz/hardlink-to-t0001: Hard-link target './docs/t0001.txt' does not exist.: No such file or directory` and `bsdtar: Error
exit delayed from previous errors.`; BusyBox: `tar: can't create hardlink './zz/hardlink-to-t0001' to './docs/t0001.txt'`.
No reader warned about a missing end of archive in a slice that has none (c a053, d, b, and a's 46 runs without the tail).
No file outside the covered slices appeared anywhere ("outside +0").

Where the readers restore the rest (case (a), the whole unit in 47 runs; case (b) is the same except the left-out slice's
files):

| reader | hard link | xattrs and named ACLs (48 marked) | default ACL on `acl_dir` | folders' mtimes | FIFO, symlink |
| --- | --- | --- | --- | --- | --- |
| GNU tar 1.30, 1.34, 1.35 | kept as a link | 48/48 | yes | 9/13 | yes, yes |
| bsdtar 3.6.2 (`-p`) | kept as a link | 48/48 | yes | 9/13 | yes, yes |
| BusyBox 1.36.1 | kept as a link | 0/48 | no | 0/13 | yes, yes |

Case (b), one run: folders' mtimes 13/13 and xattr and ACL 46/46 (the 48 less the two in the left-out slice) for the four
tars that restore them, 0 for BusyBox. In (a) a folder's mtime is set when its own run ends, then a later run that adds to
it moves it again (9 of 13 kept). Root and user are the same for
all of it: root restoring the same numeric owners changed nothing measured.

## 5. Step (e): one member named, out of the slice that holds it

Slice of many in each case; the tree the reader left is compared with the member and its parent folders.

| reader | ordinary name | name with a newline | the folder's own member |
| --- | --- | --- | --- |
| GNU tar 1.30, 1.34, 1.35 | exit 0, exact, file whole | exit 0, exact, file whole | exit 0, exact, with `--no-recursion` |
| bsdtar 3.6.2 | exit 0, exact | exit 0, exact | exit 0, exact, with `-n` |
| BusyBox 1.36.1 | exit 0, exact | exit 0, exact | exit 0, **127 entries restored (everything under it)**; `--no-recursion` is accepted and does nothing here |

Spelling: the rows above name the member as the stream has it (`./docs/t1528.txt`). Without the `./`: GNU tar exits 2 with
"Not found in archive" for all three names (every version), BusyBox exits 1 "not found in archive" for all three, bsdtar
works for all three. A hard-link member named alone out of its slice (informational, not part of the rule) fails on every
reader for the same reason as (c): the first name is not there (GNU 2, bsdtar 1, BusyBox 1).

The newline name goes through a shell variable and a command-line argument unchanged. Verdict for step (e): by offsets.

## 6. Supplement: the same unit without the hard link

Not part of the rule. It isolates the cause of (c)'s failure. Same method, 2,430 files; the cut is the same (55 slices;
only the tail's offset moved by 1,536 bytes). Result: every reader, as user and as root, exits 0 and warns nothing in (a),
(b), (c) both slices, (d) all three runs; files whole 2430/2430 in (a), 2296/2296 in (b); the end-of-archive blocks in the
last slice alone give no warning. Restored metadata is as in section 4 (BusyBox nothing). Step (e) is the same (ordinary and
newline by name on all five; the folder exact on GNU tar and bsdtar, 127 entries on BusyBox).

## 7. Deviations and notes

- BusyBox tar is the host's, not the `busybox` image, as the task allowed.
- The slices are written as plaintext files and not encrypted with age: the readers see the decrypted cut, which is what is
  under test; encryption is neither read nor changed. (#567 showed the round trip.)
- `bsdtar` takes `-p` so that an ordinary user gets xattrs and ACLs back (without it, a non-root bsdtar does not restore
  them). The heir script's flags are not fixed yet; this is a choice of the measurement.
- The folder case was first run with a wrong option for bsdtar (an `echo -n` that printed nothing) and BusyBox (no option);
  both were corrected and the whole matrix run again from the start before anything in this note was taken.
- Containers and images used (`debian:10-slim`, `debian:12-slim`) were removed afterwards; nothing was installed on the host.
