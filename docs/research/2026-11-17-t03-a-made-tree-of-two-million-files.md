# T0.3 Real scale: a made tree of two million files (measured 2026-11-17)

Verdict: item 253 is NOT reopened. No content mismatch, no member placed wrongly, no tool failed or ran out of memory.
Neither report-to-CTO threshold crossed: the table text is 277 MB (limit 1 GiB); finding one path's row takes at most 14.3 s with awk and 0.7 s with grep (limit 60 s).

## Method
- Host: vm-desk1, load 7-19 (shared), GNU tar 1.35, age 1.1.1, Python 3.12, ext4 on /scratch. Every step under `nice -n 10 ionice -c3`. Scripts: `/scratch/t03/bin/{make_tree.py,pipe.py,verify.py,run_all.sh,run2.sh}`.
- Tree (seeded, `make_tree.py`): 2,000,000 small files of 200-3,999 bytes in 100 top folders (about 20,000 files each), 20,282 folders (the 100 tops included) of mixed depth (1-6 levels, some names with "é ü"), 1 in 100 files with a name component of 200-237 bytes (20,000), 1 in 53 with a space in the name, 1 in 1000 with a named ACL entry (`setfacl -m u:2:r`, 2,000), one hard link pair per top folder (100), ten files of 1 GiB + 3 MB (first ten top folders; sparse with six 100 kB random blocks each, so they cost little disk but are read as full bytes, no `--sparse`), and a dotfile at the root. Total 2,000,011 regular-file members, 20,282 folder members, 100 hard-link members; list of 2,020,393 entries (`./`-prefixed, NUL-separated, bytewise sorted, folders as own entries, root `.` omitted).
- Writer (D2): `tar --create --file=- --format=pax --numeric-owner --xattrs --acls --xattrs-exclude='system.posix_acl_*' --no-recursion --null --verbatim-files-from --files-from=LIST --pax-option=exthdr.name=%d/PaxHeaders/%f,delete=atime,delete=ctime`, cwd the tree, `LC_ALL=C`, TAR_OPTIONS/POSIXLY_CORRECT removed. Stream: 18,575,595,520 bytes (17.30 GiB).
- Reader/cutter (`pipe.py`, my own Python; the research note's reader is not shipped as a file): parses extension headers (x/g) and the member header from the stream as it passes, takes `size` and `path`/`linkpath` from pax records, cuts at member starts per D3 with slice_size 1 GiB, writes the D4 table (kind slice start end size sha256 path [target]; `\\ \t \n \r` escaped as `volume::index::escape` does) and the D5 id map. Hard-link rows carry the target's sha256 (the reader was told the 100 target paths, as tapectl would know them from the snapshot). Each member is checked against the list name in order and against `lstat` of the tree (kind, and size for files).
- Pipeline (full pass): tar create -> reader/cutter -> per slice `age -r` -> `age -d -i` -> join -> `tar -x -O` -> per-member sha256. Nothing on disk but the two lists; slices flow through pipes (relay threads count ciphertext bytes and hash both sides; each slice's cut sha256 equals its decrypted sha256, 37/37). Sample (step 4): the 1000-ish chosen members are carved by their table offsets from the decrypted slice (+1024 zero bytes) and extracted alone by `tar -x --xattrs --acls`.
- Check: direct `find -type f | xargs sha256sum` walk (2,000,111 names = 2,000,011 files + 100 hard-link names) against (a) the table's sha256 column, (b) the extraction's per-member hashes, (c) the sample.

## Results
| stage | wall |
|---|---|
| make tree | 5 m 49 s |
| list (find + sort -z) | 6 s |
| direct sha256sum walk | 18 m 25 s (ran beside the table pass) |
| table pass (tar create + reader, slices to nowhere) | 18 m 25 s (1,101 s reader) |
| full pass (create, cut, age -e, age -d, join, tar -x -O, hash) | 27 m 09 s (1,629 s) |
| verify (table vs walk, D3 recomputed from rows, sample) | 91 s |
| tree deleted, batches by top folder | 11:23:35Z - 11:29:31Z (fully deleted 2026-11-17 11:29:31 UTC) |

Peak memory (VmHWM, sampled every 0.5 s): tar create 203.9 MiB (208,748 KiB); age -e 6.4 MiB; age -d 11.6 MiB; tar -x 2.5 MiB; Python reader/cutter/relay 71.3 MB in the full pass (67.4 MB in the table pass; table is streamed to disk, nothing grows with member count except the 100-entry hard-link map). No process came near any memory limit.
CPU seconds (full pass): tar create 372, age -e 241, age -d 203, tar -x 61, driver 1,408 (hashing the stream twice, relays, 2M row formats).

Slices: 37 on this cut against 18 for a plain 1 GiB cut (stream 17.30 GiB). The extra 19: each of the ten larger-than-slice files is a run of two slices (1 GiB + 3.0-3.1 MB remainder) and the member after a run starts a new slice, so each run leaves one small slice (11 slices under 64 MiB, the 11th the tail); min 3,584 bytes, median 78,190,592, max 1,073,741,824. Bytes on tape are unchanged (the slices joined are the stream). Ciphertext growth: age only.

File table (D4): 277,372,041 bytes (264.5 MiB), 2,020,395 lines; the table pass and the full pass produced byte-identical tables; made inside the 1,101 s reader (about 55 % of that is waiting on tar create). Lookups (page-cache warm, after the files had been read once; early-exit awk `$7==p`, `grep -m1 -F -- "<TAB>path"`): row near the file start 0.01 s both; row at about 50 % of the file: awk 6.9 s, grep 0.10 s; last row: awk 13.8 s, grep 0.26 s; absent path: awk 14.3 s, grep 0.72 s (`lookups.txt`). Note awk -F'\t' is a full field split of every row.

Id map: `u 1000 (no name)`, `g 1000 (no name)`, one row each (--numeric-owner).

## Checks
- Table sha256 vs the direct walk: 0 mismatches over 2,000,011 file rows; 100 hard-link rows carry the target's hash, 0 mismatches.
- Extraction hashes (tar -x -O, split by each member's size): 2,000,011 members, 0 mismatches, 0 short reads, 0 extra bytes, tar exit 0.
- Placement: rows contiguous (start = previous end), 0 breaks; last end 18,575,586,304 + 9,216-byte tail = stream_bytes; the slice column recomputed from the rows by D3 agrees on all 2,020,393 rows; 0 list/lstat disagreements (name order, kind, file size).
- Step 4 sample, 988 members read from their slice alone with offsets from the table: 972 regular files (incl. 16 long names, 10 with a named ACL) restored whole, 0 bad, ACL entry kept 10/10; 11 folders exit 0; 5 hard-link members alone exit 2 ("Cannot hard link to ... No such file"), the known limit (target in another slice/not in the carved bytes, as T0.7 case c); none are failures of the rule. Members larger than a slice were not sampled alone; they were proven by the full extraction hash (ten 1 GiB + 3 MB files, each a run of two slices).
- Decrypted slice sha256 equals the cut sha256 for 37/37 slices.

## Deviations
1. The extractor is `tar -x -O` with the reader's per-member sizes (checked against `lstat` of the tree) splitting the stream, not `--to-command`. A first full pass with `--to-command='printf "%s\n" "$TAR_FILENAME"; exec sha256sum'` ran at about 6.5k lines/min under host contention (a fork and two execs per member; extrapolated 8+ hours) and was killed at about 365,000 of 4.0 million lines (about 180,000 files, no mismatch checked on that partial). Under `--to-command` hard-link members produce no output (no data); noted by test on a small tree. `tar -x -O` still exercised tar's parse of the whole 2,020,393-member stream at this scale.
2. Files were not written to disk by the extractor (disk limits); `-O` discards content, so extraction of metadata (modes, xattrs) at scale is not covered; the sample covered ACL and long names.
3. Pipeline process count: driver + tar create + age -e + age -d + tar -x = 5 (the host limit was 4 parallel); the walk ran beside the table pass. Load stayed 7-19.
4. The ten big files are sparse (disk), read as full bytes; their content is mostly zeros.
5. Ownership of the two tracked lists: `special.txt` (22,110 rows) is kept.
6. My reader is not the research note's 40-line reference (§6); the `tar` header parse is mine. No tapectl built or run; no drive touched. BROKER NOTICE not sent by me (the coordinator relayed the broker's limits).

## Cleanup
Tree, stream lists, the walk, both tables, the sample and extraction files deleted. Kept under /scratch/t03: this note, `bin/`, and small results in `work/` (stats_*.json, verify.json, verify.out, lookups.txt, stages.log, sample.idx, special.txt, table_head_sample.tsv, extract_head_sample.txt). /scratch/t03 is 5.3 MB. /scratch free: 59 GB at start, 47 GB at the low point (about 12 GB used), 55 GB after cleanup.
