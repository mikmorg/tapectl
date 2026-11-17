# T0.5 The decompressors on a slice compressed alone (measured 2026-11-17)

**Verdict for item 285: CONFIRMED.** For each of gzip, xz and zstd, a slice packed by this host's tool at its default level (gzip 6, xz 6, zstd 3) and at its highest (gzip 9, xz 9, zstd 19) was decoded to the cut by this host's tool (Ubuntu 24.04: gzip 1.12, xz 5.4.5, zstd 1.5.5) and by Debian 10's tool (gzip 1.9, xz 5.2.4, zstd 1.3.8), on every slice and on all four restore tests (5/5 each); the worst growth over bytes that do not shrink is 0.0157% (gzip), 0.0065% (xz) and 0.0026% (zstd) of a 4 MiB slice, far under 1%. The control passed (section 8). Two things beside the verdict, neither a failure of the rule: (a) the pristine `debian:10-slim` holds only gzip; bzip2, xz-utils, zstd, lzop, lz4 and busybox were INSTALLED from archive.debian.org packages (section 7), so "Debian 10's stock tool" means Debian 10's package, not a stock install; (b) of the four other algorithms (recorded, reopen nothing), **lzma-alone grows 1.36%** and bzip2 0.47% on bytes that do not shrink, and zstd `--ultra -22` and xz `-9e` also pass on Debian 10.

Not covered, by instruction: tapectl compresses in-process with libraries (item 286) and a library-made slice was not at hand. T4.5's tests and T4.16's gate legs prove library-made slices against the real tools. No slice here was made by tapectl.

## Method (repeatable)

Everything under `/scratch/t05`; scripts in `/scratch/t05/bin` (`python3 -I` for Python, bash for the rest; every process under `nice -n 10`; one thread per compressor/decompressor, no `-T` multi-thread modes, 4 processes at a time, the growth runs 2, never more than 6 in all; `bench_all.py`, `growth.py`, `read_all.sh` and `damage.py` ran partly at once, and the host carried other work, load about 5: **speeds are one thread, shared host, not a quiet-machine figure**).
1. `bin/build.py`: public data only, in `data/src/<kind>/`, up to 48 MB per kind: **text** = /usr/share/doc on this host (Ubuntu 24.04), .gz decompressed, text files only (2568 files) plus one 9 MiB concatenation of them; **source** = this repository's `src/` and `vendor/` (read-only copy) then /usr/include (3506 files); **dump** = a SQLite database made from the decompressed /usr/share/man pages (page, line number, line, length, md5, random timestamp; 8 tables), `sqlite3 .dump`, cut at line ends into files of about 6 MB, stopped at 48 MB; **images** = 247 real JPEG/PNG files over 20 kB found under /usr/share and /usr/lib; **random** = `random.Random(11).randbytes` (random bytes, labelled as such: one 10 MiB file and files of 1-3 MiB). **mixed** = every kth file of each kind (about 20 MB of each, plus the two big members text/big-concatenation.txt (9 MiB) and random/big-10MB.bin (10 MiB)); no dump file is in it (each is over 4 MiB; dump has its own rows).
2. Stream as #565's `measure.py`/#567's `aligned.py` make it: GNU tar 1.35, `--format=pax --numeric-owner --no-recursion --null --verbatim-files-from --files-from=<sorted ./names> --pax-option=exthdr.name=%d/PaxHeaders/%f,delete=atime,delete=ctime`. Cut at member starts with `aligned.py`'s rule, slice_size 4 MiB (the mixed stream: 39.2 MB, 12 slices, two runs of a long member); the same kind streams also as ONE slice (48-65 MB) as a proxy for a large slice. Hashes of each cut and of each file recorded.
3. `bin/bench_all.py` -> `bench.sh` + `conf.sh`: each slice compressed alone, one thread, with the commands in `conf.sh`: `gzip -n -6|-9`, `bzip2 -9` (its default and highest are the same), `lzop -3|-9`, `xz -T1 -6|-9 --check=crc64`, `xz --format=lzma -6|-9`, `zstd -T1 -q -3|-19 --check` (file argument, so the frame states its content size; checked: `Decompressed Size: 4 MiB`, `Check: XXH64`; the xz frame has CRC64, one stream, one block; the gzip header has no name), `lz4 -1|-12`. Extras that decide nothing: `xz -9e`, `zstd --ultra -22`. Time per slice for compress and for this host's decode; decode compared to the cut.
4. `bin/enc.py`: the real `age` 1.1.1 (`age -r`, then `age -d -i`) on every packed slice; the decrypted bytes are what the readers get (192 slices, all equal).
5. Readers (`bin/read.sh`, same script everywhere): host Ubuntu 24.04 tools; Debian 10 container (tools installed from the archive; gzip 1.9, bzip2 1.0.6, xz 5.2.4, zstd 1.3.8, lzop 1.03, lz4 1.8.3, tar 1.30); BusyBox 1.36.1 (this host's `/usr/bin/busybox`, applets and its own head/tail/tar/sha256sum) and BusyBox 1.30.1 (Debian 10's busybox-static); an Ubuntu 24.04 container with lz4 1.9.4 (lz4 is not on the host). Five tests per configuration: (1) every slice decoded alone equals the cut (hash) with exit 0; (2, 3) one small file restored from only its slice (decode the slice, `tail -c +off | head -c len`, 1024 zero bytes, `tar -x`), a text file and a source file, sha256 compared; (4, 5) a file larger than a slice (9 MiB text; 10 MiB random) from only its run (3 slices each). The exit statuses are recorded and never trusted: the file bytes decide.
6. `bin/damage.py`: one byte of the ciphertext flipped (xor 0x5A) in the first, a middle and the last 64 KiB chunk (chunk = 65,552 ciphertext bytes after the age header's `---` MAC line and the 16-byte nonce); `age -d` output, its exit and stderr captured, then each decoder on age's output, exit and bytes captured separately (no pipefail); correct prefix found by comparing with the cut; "members whole" = members of that slice that end inside the correct prefix, then re-checked by `tar -x` of exactly those byte ranges and sha256. Three slices: a compressible run piece of the 9 MiB text member, a slice of 46 members, a slice of 2 members of random-like data.
7. `bin/growth.py`: 40 random 4 MiB slices (Random(1000+i)) and one 1 GiB `os.urandom` file per configuration, size taken by `| wc -c`; the images and random sets' bench rows give the real-JPEG/PNG and random slices.
8. `bin/report.py` prints the tables below. Results also in `data/res-read.tsv`, `data/sets/*/res/*.bench.tsv`, `data/damage-*.json`, `data/growth.json` (bulk data deleted at the end).

Pass criteria, written before running: a reader row PASSES when the decoded slices equal the cut and the restored files equal their recorded sha256 (exit status recorded only). A damage row of the control (`store`) passes when age exits non-zero, its output is exactly the verified chunks before the flip, and the members whole are those inside that prefix.

## Deviations and limits

- lz4 has no tool on this host; it was packed in an `ubuntu:24.04` container (lz4 1.9.4, installed with apt) and read there and in Debian 10 (1.8.3). BusyBox has no zstd or lz4.
- Debian 10's stock image has only gzip: the other tools came from archive.debian.org packages (section 7).
- "The largest slice_size in use": the default is 1G (`default_slice_size`, 2026-10-06); the config tests accept 2G and refuse 30G; home2's own value could not be read from here. Growth is linear in the slice (compare the 4 MiB and 1 GiB points; the fixed part is a few dozen bytes), so the 1 GiB figure and the percentage scale to any size; 1 GiB was run on random bytes only (the real images total 51 MB).
- "Does not shrink": real JPEG/PNG shrink about 3% (97% of the cut), so the worst-growth rows rest on random bytes (and on the images/random slices' own figures); no real data here expands more than random does.
- The one-slice rows are 48-65 MB, not 1 GiB; compression ratios of the 1 GiB default slice will differ for kinds with long-range redundancy.
- Speeds: see the method; thread count 1 per process.
- The tar used to restore in Debian 10 was 1.30, on the host 1.35, in BusyBox its own.

## Reader x algorithm (5 tests each: every slice alone = the cut; small file x2 from only its slice; two files larger than a slice from only their run)

| config | host-ubuntu24 | ubuntu24-container | deb10 | busybox-1.36.1-host | busybox-1.30.1-deb10 |
|---|---|---|---|---|---|
| gzip-6 | PASS 5/5 | PASS 5/5 | PASS 5/5 | PASS 5/5 | PASS 5/5 |
| gzip-9 | PASS 5/5 | PASS 5/5 | PASS 5/5 | PASS 5/5 | PASS 5/5 |
| bzip2-9 | PASS 5/5 | n/a | PASS 5/5 | PASS 5/5 | PASS 5/5 |
| lzop-3 | PASS 5/5 | n/a | PASS 5/5 | PASS 5/5 | PASS 5/5 |
| lzop-9 | PASS 5/5 | n/a | PASS 5/5 | PASS 5/5 | PASS 5/5 |
| xz-6 | PASS 5/5 | n/a | PASS 5/5 | PASS 5/5 | PASS 5/5 |
| xz-9 | PASS 5/5 | n/a | PASS 5/5 | PASS 5/5 | PASS 5/5 |
| lzma-6 | PASS 5/5 | n/a | PASS 5/5 | PASS 5/5 | PASS 5/5 |
| lzma-9 | PASS 5/5 | n/a | PASS 5/5 | PASS 5/5 | PASS 5/5 |
| zstd-3 | PASS 5/5 | n/a | PASS 5/5 | n/a | n/a |
| zstd-19 | PASS 5/5 | n/a | PASS 5/5 | n/a | n/a |
| lz4-1 | n/a | PASS 5/5 | PASS 5/5 | n/a | n/a |
| lz4-12 | n/a | PASS 5/5 | PASS 5/5 | n/a | n/a |
| xz-9e | PASS 5/5 | n/a | PASS 5/5 | PASS 5/5 | PASS 5/5 |
| zstd-22 | PASS 5/5 | n/a | PASS 5/5 | n/a | n/a |
| store | PASS 5/5 | PASS 5/5 | PASS 5/5 | PASS 5/5 | PASS 5/5 |

Failing rows: none

## Size and speed by kind of data (packed/cut %, compress MB/s, decompress MB/s; one thread per process, nice 10, 4 processes at a time)


**slice_size 4 MiB (member-aligned cut)**

| config | text | source | dump | images | random | mixed |
|---|---|---|---|---|---|---|
| gzip-6 | 18.0% / 7.7 / 40 | 17.9% / 8.9 / 44 | 30.8% / 16.5 / 72 | 97.1% / 10.3 / 34 | 99.9% / 4.7 / 48 | 78.3% / 6.4 / 61 |
| gzip-9 | 17.8% / 6.8 / 64 | 17.7% / 3.1 / 28 | 30.6% / 12.1 / 79 | 97.1% / 15.8 / 86 | 99.9% / 3.9 / 34 | 78.3% / 5.4 / 47 |
| bzip2-9 | 13.7% / 3.7 / 8 | 13.6% / 2.3 / 14 | 23.4% / 6.3 / 15 | 97.5% / 3.7 / 8 | 100.4% / 1.9 / 5 | 77.6% / 2.3 / 6 |
| lzop-3 | 29.5% / 59.2 / 65 | 30.2% / 55.6 / 85 | 48.1% / 108.9 / 155 | 99.1% / 212.2 / 339 | 100.0% / 50.8 / 116 | 82.3% / 35.4 / 66 |
| lzop-9 | 20.6% / 3.1 / 48 | 20.5% / 2.3 / 129 | 36.7% / 5.0 / 145 | 97.7% / 6.4 / 269 | 100.0% / 2.9 / 176 | 79.4% / 1.6 / 90 |
| xz-6 | 12.7% / 1.2 / 25 | 13.0% / 1.3 / 59 | 21.9% / 0.8 / 24 | 96.6% / 2.1 / 24 | 100.0% / 1.8 / 110 | 76.9% / 1.4 / 13 |
| xz-9 | 12.7% / 1.1 / 21 | 13.0% / 1.3 / 53 | 21.9% / 1.1 / 31 | 96.6% / 1.6 / 21 | 100.0% / 2.0 / 133 | 76.9% / 1.0 / 14 |
| lzma-6 | 12.7% / 1.1 / 26 | 13.0% / 1.8 / 66 | 21.9% / 1.2 / 33 | 96.9% / 1.8 / 9 | 101.3% / 2.1 / 10 | 77.7% / 1.1 / 7 |
| lzma-9 | 12.7% / 1.0 / 23 | 13.0% / 1.7 / 61 | 21.9% / 0.8 / 26 | 96.9% / 2.1 / 11 | 101.3% / 2.1 / 10 | 77.7% / 1.1 / 11 |
| zstd-3 | 16.3% / 31.5 / 62 | 17.5% / 68.5 / 169 | 27.8% / 35.6 / 117 | 97.4% / 75.1 / 228 | 100.0% / 118.4 / 205 | 78.4% / 23.8 / 58 |
| zstd-19 | 13.1% / 0.6 / 35 | 13.3% / 1.3 / 170 | 23.1% / 1.0 / 105 | 96.6% / 2.4 / 135 | 99.9% / 2.9 / 202 | 77.1% / 1.2 / 75 |
| lz4-1 | 27.7% / 112.6 / 179 | 28.7% / 150.9 / 259 | 47.2% / 95.2 / 157 | 98.3% / 83.4 / 152 | 100.0% / 153.6 / 332 | 81.9% / 95.4 / 192 |
| lz4-12 | 20.4% / 6.7 / 213 | 20.5% / 7.0 / 252 | 37.2% / 4.6 / 183 | 97.7% / 10.2 / 170 | 100.0% / 15.1 / 336 | 79.5% / 9.5 / 222 |
| xz-9e | - | - | - | - | - | 76.9% / 0.9 / 28 |
| zstd-22 | - | - | - | - | - | 77.1% / 0.9 / 58 |

**whole kind in ONE slice (48-65 MB; proxy for a 1 GiB slice)**

| config | text | source | dump | images | random |
|---|---|---|---|---|---|
| gzip-6 | 18.0% / 17.1 / 101 | 17.9% / 26.5 / 150 | 30.8% / 19.0 / 101 | 97.1% / 16.8 / 94 | 99.9% / 20.1 / 145 |
| gzip-9 | 17.8% / 6.7 / 145 | 17.7% / 11.9 / 147 | 30.6% / 13.3 / 102 | 97.1% / 15.0 / 100 | 99.9% / 21.2 / 151 |
| bzip2-9 | 13.7% / 4.0 / 6 | 13.5% / 7.9 / 24 | 23.3% / 6.7 / 18 | 97.6% / 3.2 / 6 | 100.4% / 3.8 / 8 |
| lzop-3 | 29.6% / 45.7 / 253 | 30.2% / 240.7 / 283 | 48.1% / 132.6 / 283 | 99.0% / 254.5 / 621 | 100.0% / 360.2 / 892 |
| lzop-9 | 20.6% / 5.0 / 282 | 20.5% / 5.2 / 305 | 36.7% / 5.4 / 259 | 97.7% / 5.9 / 584 | 100.0% / 7.2 / 863 |
| xz-6 | 11.2% / 1.0 / 27 | 12.4% / 1.6 / 80 | 15.3% / 1.0 / 52 | 96.1% / 0.7 / 15 | 100.0% / 1.4 / 210 |
| xz-9 | 8.8% / 0.6 / 78 | 12.2% / 1.3 / 71 | 13.2% / 0.6 / 58 | 95.7% / 1.1 / 26 | 100.0% / 1.3 / 215 |
| lzma-6 | 11.2% / 1.2 / 76 | 12.4% / 1.5 / 69 | 15.3% / 0.8 / 52 | 96.4% / 1.4 / 11 | 101.3% / 1.5 / 10 |
| lzma-9 | 8.8% / 0.6 / 18 | 12.2% / 1.2 / 78 | 13.2% / 0.6 / 41 | 96.0% / 0.6 / 11 | 101.3% / 1.2 / 10 |
| zstd-3 | 16.0% / 36.3 / 135 | 17.4% / 111.1 / 285 | 27.7% / 66.9 / 314 | 97.4% / 133.8 / 821 | 100.0% / 235.1 / 905 |
| zstd-19 | 11.5% / 0.5 / 361 | 12.6% / 1.0 / 315 | 16.2% / 0.5 / 286 | 96.1% / 0.9 / 211 | 99.9% / 1.6 / 666 |
| lz4-1 | 27.7% / 205.9 / 634 | 28.7% / 242.8 / 643 | 47.2% / 171.7 / 553 | 98.3% / 297.4 / 573 | 100.0% / 363.7 / 920 |
| lz4-12 | 20.4% / 6.6 / 677 | 20.5% / 7.4 / 661 | 37.2% / 5.1 / 529 | 97.7% / 11.5 / 454 | 100.0% / 16.2 / 711 |

(each cell: packed size as % of the cut; compress MiB/s; decompress MiB/s of the cut; '(!rt)' = a slice failed the host round trip or a tool exited non-zero)

## Growth bound over bytes that do not shrink (bytes added; % of slice)

| config | 4 MiB, worst of 40 random slices | 4 MiB, worst slice of the images and random sets | 4 MiB worst | 1 GiB random | per 4 MiB block implied (1 GiB) | worst / slice (4 MiB) |
|---|---|---|---|---|---|---|
| gzip-6 | +658 (+0.0157%) | +658 on 4194304 B (random) | +658 (+0.0157%) | +173490 (+0.01616%) | +677.7 B | 0.0157% |
| gzip-9 | +658 (+0.0157%) | +658 on 4194304 B (random) | +658 (+0.0157%) | +173490 (+0.01616%) | +677.7 B | 0.0157% |
| bzip2-9 | +19806 (+0.4722%) | +19359 on 4194304 B (images) | +19806 (+0.4722%) | +4735399 (+0.44102%) | +18497.7 B | 0.4722% |
| lzop-3 | +234 (+0.0056%) | +234 on 4194304 B (random) | +234 (+0.0056%) | +49194 (+0.00458%) | +192.2 B | 0.0056% |
| lzop-9 | +234 (+0.0056%) | +234 on 4194304 B (random) | +234 (+0.0056%) | +49194 (+0.00458%) | +192.2 B | 0.0056% |
| xz-6 | +272 (+0.0065%) | +272 on 4194304 B (random) | +272 (+0.0065%) | +53224 (+0.00496%) | +207.9 B | 0.0065% |
| xz-9 | +272 (+0.0065%) | +272 on 4194304 B (random) | +272 (+0.0065%) | +53224 (+0.00496%) | +207.9 B | 0.0065% |
| lzma-6 | +57224 (+1.3643%) | +56949 on 4194304 B (random) | +57224 (+1.3643%) | +14600944 (+1.35982%) | +57034.9 B | 1.3643% |
| lzma-9 | +57224 (+1.3643%) | +56949 on 4194304 B (random) | +57224 (+1.3643%) | +14536285 (+1.35380%) | +56782.4 B | 1.3643% |
| zstd-3 | +110 (+0.0026%) | +110 on 4194304 B (images) | +110 (+0.0026%) | +24590 (+0.00229%) | +96.1 B | 0.0026% |
| zstd-19 | +109 (+0.0026%) | +109 on 4194304 B (random) | +109 (+0.0026%) | +24590 (+0.00229%) | +96.1 B | 0.0026% |
| lz4-1 | +19 (+0.0005%) | +19 on 4194304 B (images) | +19 (+0.0005%) | +1039 (+0.00010%) | +4.1 B | 0.0005% |
| lz4-12 | +19 (+0.0005%) | +19 on 4194304 B (images) | +19 (+0.0005%) | +1039 (+0.00010%) | +4.1 B | 0.0005% |
| xz-9e | +272 (+0.0065%) | +0 on 0 B (not run) | +272 (+0.0065%) | +53224 (+0.00496%) | +207.9 B | 0.0065% |
| zstd-22 | +109 (+0.0026%) | +0 on 0 B (not run) | +109 (+0.0026%) | +24590 (+0.00229%) | +96.1 B | 0.0026% |

## Damage, compressible: a slice from the middle of a 9 MiB text member (a run piece; no whole member inside). One flipped ciphertext byte, real age 1.1.1; exit statuses captured separately

| config | flip in chunk (of n) | age exit | age stdout bytes (= verified chunks x 65536) | reader | decoder exit | decoder bytes out | correct prefix of the cut | members whole / in slice | those restored equal |
|---|---|---|---|---|---|---|---|---|---|
| store | first (64) | 1 | 0 (0.0) | host | 0 | 0 | 0 | 0/0 | 0 |
| store | first (64) | 1 | 0 (0.0) | busybox | 0 | 0 | 0 | 0/0 | 0 |
| store | first (64) | 1 | 0 (0.0) | deb10 | 0 | 0 | 0 | 0/0 | 0 |
| store | middle (64) | 1 | 2097152 (32.0) | host | 0 | 2097152 | 2097152 | 0/0 | 0 |
| store | middle (64) | 1 | 2097152 (32.0) | busybox | 0 | 2097152 | 2097152 | 0/0 | 0 |
| store | middle (64) | 1 | 2097152 (32.0) | deb10 | 0 | 2097152 | 2097152 | 0/0 | 0 |
| store | last (64) | 1 | 4128768 (63.0) | host | 0 | 4128768 | 4128768 | 0/0 | 0 |
| store | last (64) | 1 | 4128768 (63.0) | busybox | 0 | 4128768 | 4128768 | 0/0 | 0 |
| store | last (64) | 1 | 4128768 (63.0) | deb10 | 0 | 4128768 | 4128768 | 0/0 | 0 |
| gzip-6 | first (13) | 1 | 0 (0.0) | host | 1 | 0 | 0 | 0/0 | 0 |
| gzip-6 | first (13) | 1 | 0 (0.0) | busybox | 1 | 0 | 0 | 0/0 | 0 |
| gzip-6 | first (13) | 1 | 0 (0.0) | deb10 | 1 | 0 | 0 | 0/0 | 0 |
| gzip-6 | middle (13) | 1 | 393216 (6.0) | host | 1 | 1496199 | 1496199 | 0/0 | 0 |
| gzip-6 | middle (13) | 1 | 393216 (6.0) | busybox | 1 | 1474560 | 1474560 | 0/0 | 0 |
| gzip-6 | middle (13) | 1 | 393216 (6.0) | deb10 | 1 | 1496199 | 1496199 | 0/0 | 0 |
| gzip-6 | last (13) | 1 | 786432 (12.0) | host | 1 | 4094854 | 4094854 | 0/0 | 0 |
| gzip-6 | last (13) | 1 | 786432 (12.0) | busybox | 1 | 4063232 | 4063232 | 0/0 | 0 |
| gzip-6 | last (13) | 1 | 786432 (12.0) | deb10 | 1 | 4094854 | 4094854 | 0/0 | 0 |
| gzip-9 | first (13) | 1 | 0 (0.0) | host | 1 | 0 | 0 | 0/0 | 0 |
| gzip-9 | first (13) | 1 | 0 (0.0) | busybox | 1 | 0 | 0 | 0/0 | 0 |
| gzip-9 | first (13) | 1 | 0 (0.0) | deb10 | 1 | 0 | 0 | 0/0 | 0 |
| gzip-9 | middle (13) | 1 | 393216 (6.0) | host | 1 | 1503164 | 1503164 | 0/0 | 0 |
| gzip-9 | middle (13) | 1 | 393216 (6.0) | busybox | 1 | 1474560 | 1474560 | 0/0 | 0 |
| gzip-9 | middle (13) | 1 | 393216 (6.0) | deb10 | 1 | 1503164 | 1503164 | 0/0 | 0 |
| gzip-9 | last (13) | 1 | 786432 (12.0) | host | 1 | 4136399 | 4136399 | 0/0 | 0 |
| gzip-9 | last (13) | 1 | 786432 (12.0) | busybox | 1 | 4128768 | 4128768 | 0/0 | 0 |
| gzip-9 | last (13) | 1 | 786432 (12.0) | deb10 | 1 | 4136399 | 4136399 | 0/0 | 0 |
| bzip2-9 | first (11) | 1 | 0 (0.0) | host | 2 | 0 | 0 | 0/0 | 0 |
| bzip2-9 | first (11) | 1 | 0 (0.0) | busybox | 1 | 0 | 0 | 0/0 | 0 |
| bzip2-9 | first (11) | 1 | 0 (0.0) | deb10 | 2 | 0 | 0 | 0/0 | 0 |
| bzip2-9 | middle (11) | 1 | 327680 (5.0) | host | 2 | 910000 | 910000 | 0/0 | 0 |
| bzip2-9 | middle (11) | 1 | 327680 (5.0) | busybox | 1 | 909312 | 909312 | 0/0 | 0 |
| bzip2-9 | middle (11) | 1 | 327680 (5.0) | deb10 | 2 | 910000 | 910000 | 0/0 | 0 |
| bzip2-9 | last (11) | 1 | 655360 (10.0) | host | 2 | 3615000 | 3615000 | 0/0 | 0 |
| bzip2-9 | last (11) | 1 | 655360 (10.0) | busybox | 1 | 3612672 | 3612672 | 0/0 | 0 |
| bzip2-9 | last (11) | 1 | 655360 (10.0) | deb10 | 2 | 3615000 | 3615000 | 0/0 | 0 |
| lzop-3 | first (21) | 1 | 0 (0.0) | host | 1 | 0 | 0 | 0/0 | 0 |
| lzop-3 | first (21) | 1 | 0 (0.0) | busybox | 1 | 0 | 0 | 0/0 | 0 |
| lzop-3 | first (21) | 1 | 0 (0.0) | deb10 | 1 | 0 | 0 | 0/0 | 0 |
| lzop-3 | middle (21) | 1 | 655360 (10.0) | host | 1 | 1310720 | 1310720 | 0/0 | 0 |
| lzop-3 | middle (21) | 1 | 655360 (10.0) | busybox | 1 | 1310720 | 1310720 | 0/0 | 0 |
| lzop-3 | middle (21) | 1 | 655360 (10.0) | deb10 | 1 | 1310720 | 1310720 | 0/0 | 0 |
| lzop-3 | last (21) | 1 | 1310720 (20.0) | host | 1 | 3932160 | 3932160 | 0/0 | 0 |
| lzop-3 | last (21) | 1 | 1310720 (20.0) | busybox | 1 | 3932160 | 3932160 | 0/0 | 0 |
| lzop-3 | last (21) | 1 | 1310720 (20.0) | deb10 | 1 | 3932160 | 3932160 | 0/0 | 0 |
| lzop-9 | first (15) | 1 | 0 (0.0) | host | 1 | 0 | 0 | 0/0 | 0 |
| lzop-9 | first (15) | 1 | 0 (0.0) | busybox | 1 | 0 | 0 | 0/0 | 0 |
| lzop-9 | first (15) | 1 | 0 (0.0) | deb10 | 1 | 0 | 0 | 0/0 | 0 |
| lzop-9 | middle (15) | 1 | 458752 (7.0) | host | 1 | 1310720 | 1310720 | 0/0 | 0 |
| lzop-9 | middle (15) | 1 | 458752 (7.0) | busybox | 1 | 1310720 | 1310720 | 0/0 | 0 |
| lzop-9 | middle (15) | 1 | 458752 (7.0) | deb10 | 1 | 1310720 | 1310720 | 0/0 | 0 |
| lzop-9 | last (15) | 1 | 917504 (14.0) | host | 1 | 3932160 | 3932160 | 0/0 | 0 |
| lzop-9 | last (15) | 1 | 917504 (14.0) | busybox | 1 | 3932160 | 3932160 | 0/0 | 0 |
| lzop-9 | last (15) | 1 | 917504 (14.0) | deb10 | 1 | 3932160 | 3932160 | 0/0 | 0 |
| xz-6 | first (10) | 1 | 0 (0.0) | host | 1 | 0 | 0 | 0/0 | 0 |
| xz-6 | first (10) | 1 | 0 (0.0) | busybox | 0 | 0 | 0 | 0/0 | 0 |
| xz-6 | first (10) | 1 | 0 (0.0) | deb10 | 1 | 0 | 0 | 0/0 | 0 |
| xz-6 | middle (10) | 1 | 327680 (5.0) | host | 1 | 1618542 | 1618542 | 0/0 | 0 |
| xz-6 | middle (10) | 1 | 327680 (5.0) | busybox | 1 | 1618480 | 1618480 | 0/0 | 0 |
| xz-6 | middle (10) | 1 | 327680 (5.0) | deb10 | 1 | 1618542 | 1618542 | 0/0 | 0 |
| xz-6 | last (10) | 1 | 589824 (9.0) | host | 1 | 4063796 | 4063796 | 0/0 | 0 |
| xz-6 | last (10) | 1 | 589824 (9.0) | busybox | 1 | 4063742 | 4063742 | 0/0 | 0 |
| xz-6 | last (10) | 1 | 589824 (9.0) | deb10 | 1 | 4063796 | 4063796 | 0/0 | 0 |
| xz-9 | first (10) | 1 | 0 (0.0) | host | 1 | 0 | 0 | 0/0 | 0 |
| xz-9 | first (10) | 1 | 0 (0.0) | busybox | 0 | 0 | 0 | 0/0 | 0 |
| xz-9 | first (10) | 1 | 0 (0.0) | deb10 | 1 | 0 | 0 | 0/0 | 0 |
| xz-9 | middle (10) | 1 | 327680 (5.0) | host | 1 | 1618542 | 1618542 | 0/0 | 0 |
| xz-9 | middle (10) | 1 | 327680 (5.0) | busybox | 1 | 1618480 | 1618480 | 0/0 | 0 |
| xz-9 | middle (10) | 1 | 327680 (5.0) | deb10 | 1 | 1618542 | 1618542 | 0/0 | 0 |
| xz-9 | last (10) | 1 | 589824 (9.0) | host | 1 | 4063796 | 4063796 | 0/0 | 0 |
| xz-9 | last (10) | 1 | 589824 (9.0) | busybox | 1 | 4063742 | 4063742 | 0/0 | 0 |
| xz-9 | last (10) | 1 | 589824 (9.0) | deb10 | 1 | 4063796 | 4063796 | 0/0 | 0 |
| lzma-6 | first (10) | 1 | 0 (0.0) | host | 1 | 0 | 0 | 0/0 | 0 |
| lzma-6 | first (10) | 1 | 0 (0.0) | busybox | 1 | 0 | 0 | 0/0 | 0 |
| lzma-6 | first (10) | 1 | 0 (0.0) | deb10 | 1 | 0 | 0 | 0/0 | 0 |
| lzma-6 | middle (10) | 1 | 327680 (5.0) | host | 1 | 1618795 | 1618795 | 0/0 | 0 |
| lzma-6 | middle (10) | 1 | 327680 (5.0) | busybox | 1 | 0 | 0 | 0/0 | 0 |
| lzma-6 | middle (10) | 1 | 327680 (5.0) | deb10 | 1 | 1618795 | 1618795 | 0/0 | 0 |
| lzma-6 | last (10) | 1 | 589824 (9.0) | host | 1 | 4064751 | 4064751 | 0/0 | 0 |
| lzma-6 | last (10) | 1 | 589824 (9.0) | busybox | 1 | 0 | 0 | 0/0 | 0 |
| lzma-6 | last (10) | 1 | 589824 (9.0) | deb10 | 1 | 4064751 | 4064751 | 0/0 | 0 |
| lzma-9 | first (10) | 1 | 0 (0.0) | host | 1 | 0 | 0 | 0/0 | 0 |
| lzma-9 | first (10) | 1 | 0 (0.0) | busybox | 1 | 0 | 0 | 0/0 | 0 |
| lzma-9 | first (10) | 1 | 0 (0.0) | deb10 | 1 | 0 | 0 | 0/0 | 0 |
| lzma-9 | middle (10) | 1 | 327680 (5.0) | host | 1 | 1618795 | 1618795 | 0/0 | 0 |
| lzma-9 | middle (10) | 1 | 327680 (5.0) | busybox | 1 | 0 | 0 | 0/0 | 0 |
| lzma-9 | middle (10) | 1 | 327680 (5.0) | deb10 | 1 | 1618795 | 1618795 | 0/0 | 0 |
| lzma-9 | last (10) | 1 | 589824 (9.0) | host | 1 | 4064751 | 4064751 | 0/0 | 0 |
| lzma-9 | last (10) | 1 | 589824 (9.0) | busybox | 1 | 0 | 0 | 0/0 | 0 |
| lzma-9 | last (10) | 1 | 589824 (9.0) | deb10 | 1 | 4064751 | 4064751 | 0/0 | 0 |
| zstd-3 | first (13) | 1 | 0 (0.0) | host | 1 | 0 | 0 | 0/0 | 0 |
| zstd-3 | first (13) | 1 | 0 (0.0) | deb10 | 1 | 0 | 0 | 0/0 | 0 |
| zstd-3 | middle (13) | 1 | 393216 (6.0) | host | 1 | 1441792 | 1441792 | 0/0 | 0 |
| zstd-3 | middle (13) | 1 | 393216 (6.0) | deb10 | 1 | 1441792 | 1441792 | 0/0 | 0 |
| zstd-3 | last (13) | 1 | 786432 (12.0) | host | 1 | 4063232 | 4063232 | 0/0 | 0 |
| zstd-3 | last (13) | 1 | 786432 (12.0) | deb10 | 1 | 4063232 | 4063232 | 0/0 | 0 |
| zstd-19 | first (10) | 1 | 0 (0.0) | host | 1 | 0 | 0 | 0/0 | 0 |
| zstd-19 | first (10) | 1 | 0 (0.0) | deb10 | 1 | 0 | 0 | 0/0 | 0 |
| zstd-19 | middle (10) | 1 | 327680 (5.0) | host | 1 | 1517827 | 1517827 | 0/0 | 0 |
| zstd-19 | middle (10) | 1 | 327680 (5.0) | deb10 | 1 | 1517827 | 1517827 | 0/0 | 0 |
| zstd-19 | last (10) | 1 | 589824 (9.0) | host | 1 | 3932160 | 3932160 | 0/0 | 0 |
| zstd-19 | last (10) | 1 | 589824 (9.0) | deb10 | 1 | 3932160 | 3932160 | 0/0 | 0 |
| lz4-1 | first (20) | 1 | 0 (0.0) | deb10 | 0 | 0 | 0 | 0/0 | 0 |
| lz4-1 | first (20) | 1 | 0 (0.0) | ub24 | 0 | 0 | 0 | 0/0 | 0 |
| lz4-1 | middle (20) | 1 | 655360 (10.0) | deb10 | 68 | 0 | 0 | 0/0 | 0 |
| lz4-1 | middle (20) | 1 | 655360 (10.0) | ub24 | 68 | 0 | 0 | 0/0 | 0 |
| lz4-1 | last (20) | 1 | 1245184 (19.0) | deb10 | 68 | 0 | 0 | 0/0 | 0 |
| lz4-1 | last (20) | 1 | 1245184 (19.0) | ub24 | 68 | 0 | 0 | 0/0 | 0 |
| lz4-12 | first (15) | 1 | 0 (0.0) | deb10 | 0 | 0 | 0 | 0/0 | 0 |
| lz4-12 | first (15) | 1 | 0 (0.0) | ub24 | 0 | 0 | 0 | 0/0 | 0 |
| lz4-12 | middle (15) | 1 | 458752 (7.0) | deb10 | 68 | 0 | 0 | 0/0 | 0 |
| lz4-12 | middle (15) | 1 | 458752 (7.0) | ub24 | 68 | 0 | 0 | 0/0 | 0 |
| lz4-12 | last (15) | 1 | 917504 (14.0) | deb10 | 68 | 0 | 0 | 0/0 | 0 |
| lz4-12 | last (15) | 1 | 917504 (14.0) | ub24 | 68 | 0 | 0 | 0/0 | 0 |

## Damage, members: a slice of 46 members (random 1-3 MB files, then source and text files). One flipped ciphertext byte, real age 1.1.1; exit statuses captured separately

| config | flip in chunk (of n) | age exit | age stdout bytes (= verified chunks x 65536) | reader | decoder exit | decoder bytes out | correct prefix of the cut | members whole / in slice | those restored equal |
|---|---|---|---|---|---|---|---|---|---|
| store | first (53) | 1 | 0 (0.0) | host | 0 | 0 | 0 | 0/46 | 0 |
| store | first (53) | 1 | 0 (0.0) | busybox | 0 | 0 | 0 | 0/46 | 0 |
| store | first (53) | 1 | 0 (0.0) | deb10 | 0 | 0 | 0 | 0/46 | 0 |
| store | middle (53) | 1 | 1703936 (26.0) | host | 0 | 1703936 | 1703936 | 0/46 | 0 |
| store | middle (53) | 1 | 1703936 (26.0) | busybox | 0 | 1703936 | 1703936 | 0/46 | 0 |
| store | middle (53) | 1 | 1703936 (26.0) | deb10 | 0 | 1703936 | 1703936 | 0/46 | 0 |
| store | last (53) | 1 | 3407872 (52.0) | host | 0 | 3407872 | 3407872 | 41/46 | 41 |
| store | last (53) | 1 | 3407872 (52.0) | busybox | 0 | 3407872 | 3407872 | 41/46 | 41 |
| store | last (53) | 1 | 3407872 (52.0) | deb10 | 0 | 3407872 | 3407872 | 41/46 | 41 |
| gzip-6 | first (44) | 1 | 0 (0.0) | host | 1 | 0 | 0 | 0/46 | 0 |
| gzip-6 | first (44) | 1 | 0 (0.0) | busybox | 1 | 0 | 0 | 0/46 | 0 |
| gzip-6 | first (44) | 1 | 0 (0.0) | deb10 | 1 | 0 | 0 | 0/46 | 0 |
| gzip-6 | middle (44) | 1 | 1441792 (22.0) | host | 1 | 1442875 | 1442875 | 0/46 | 0 |
| gzip-6 | middle (44) | 1 | 1441792 (22.0) | busybox | 1 | 1441792 | 1441792 | 0/46 | 0 |
| gzip-6 | middle (44) | 1 | 1441792 (22.0) | deb10 | 1 | 1442875 | 1442875 | 0/46 | 0 |
| gzip-6 | last (44) | 1 | 2818048 (43.0) | host | 1 | 3127840 | 3127840 | 20/46 | 20 |
| gzip-6 | last (44) | 1 | 2818048 (43.0) | busybox | 1 | 3112960 | 3112960 | 19/46 | 19 |
| gzip-6 | last (44) | 1 | 2818048 (43.0) | deb10 | 1 | 3127840 | 3127840 | 20/46 | 20 |
| gzip-9 | first (44) | 1 | 0 (0.0) | host | 1 | 0 | 0 | 0/46 | 0 |
| gzip-9 | first (44) | 1 | 0 (0.0) | busybox | 1 | 0 | 0 | 0/46 | 0 |
| gzip-9 | first (44) | 1 | 0 (0.0) | deb10 | 1 | 0 | 0 | 0/46 | 0 |
| gzip-9 | middle (44) | 1 | 1441792 (22.0) | host | 1 | 1442886 | 1442886 | 0/46 | 0 |
| gzip-9 | middle (44) | 1 | 1441792 (22.0) | busybox | 1 | 1441792 | 1441792 | 0/46 | 0 |
| gzip-9 | middle (44) | 1 | 1441792 (22.0) | deb10 | 1 | 1442886 | 1442886 | 0/46 | 0 |
| gzip-9 | last (44) | 1 | 2818048 (43.0) | host | 1 | 3129930 | 3129930 | 20/46 | 20 |
| gzip-9 | last (44) | 1 | 2818048 (43.0) | busybox | 1 | 3112960 | 3112960 | 19/46 | 19 |
| gzip-9 | last (44) | 1 | 2818048 (43.0) | deb10 | 1 | 3129930 | 3129930 | 20/46 | 20 |
| bzip2-9 | first (44) | 1 | 0 (0.0) | host | 2 | 0 | 0 | 0/46 | 0 |
| bzip2-9 | first (44) | 1 | 0 (0.0) | busybox | 1 | 0 | 0 | 0/46 | 0 |
| bzip2-9 | first (44) | 1 | 0 (0.0) | deb10 | 2 | 0 | 0 | 0/46 | 0 |
| bzip2-9 | middle (44) | 1 | 1441792 (22.0) | host | 2 | 900000 | 900000 | 0/46 | 0 |
| bzip2-9 | middle (44) | 1 | 1441792 (22.0) | busybox | 1 | 901120 | 901120 | 0/46 | 0 |
| bzip2-9 | middle (44) | 1 | 1441792 (22.0) | deb10 | 2 | 900000 | 900000 | 0/46 | 0 |
| bzip2-9 | last (44) | 1 | 2818048 (43.0) | host | 2 | 2700000 | 2700000 | 0/46 | 0 |
| bzip2-9 | last (44) | 1 | 2818048 (43.0) | busybox | 1 | 2699264 | 2699264 | 0/46 | 0 |
| bzip2-9 | last (44) | 1 | 2818048 (43.0) | deb10 | 2 | 2700000 | 2700000 | 0/46 | 0 |
| lzop-3 | first (46) | 1 | 0 (0.0) | host | 1 | 0 | 0 | 0/46 | 0 |
| lzop-3 | first (46) | 1 | 0 (0.0) | busybox | 1 | 0 | 0 | 0/46 | 0 |
| lzop-3 | first (46) | 1 | 0 (0.0) | deb10 | 1 | 0 | 0 | 0/46 | 0 |
| lzop-3 | middle (46) | 1 | 1507328 (23.0) | host | 1 | 1310720 | 1310720 | 0/46 | 0 |
| lzop-3 | middle (46) | 1 | 1507328 (23.0) | busybox | 1 | 1310720 | 1310720 | 0/46 | 0 |
| lzop-3 | middle (46) | 1 | 1507328 (23.0) | deb10 | 1 | 1310720 | 1310720 | 0/46 | 0 |
| lzop-3 | last (46) | 1 | 2949120 (45.0) | host | 1 | 3407872 | 3407872 | 41/46 | 41 |
| lzop-3 | last (46) | 1 | 2949120 (45.0) | busybox | 1 | 3407872 | 3407872 | 41/46 | 41 |
| lzop-3 | last (46) | 1 | 2949120 (45.0) | deb10 | 1 | 3407872 | 3407872 | 41/46 | 41 |
| lzop-9 | first (45) | 1 | 0 (0.0) | host | 1 | 0 | 0 | 0/46 | 0 |
| lzop-9 | first (45) | 1 | 0 (0.0) | busybox | 1 | 0 | 0 | 0/46 | 0 |
| lzop-9 | first (45) | 1 | 0 (0.0) | deb10 | 1 | 0 | 0 | 0/46 | 0 |
| lzop-9 | middle (45) | 1 | 1441792 (22.0) | host | 1 | 1310720 | 1310720 | 0/46 | 0 |
| lzop-9 | middle (45) | 1 | 1441792 (22.0) | busybox | 1 | 1310720 | 1310720 | 0/46 | 0 |
| lzop-9 | middle (45) | 1 | 1441792 (22.0) | deb10 | 1 | 1310720 | 1310720 | 0/46 | 0 |
| lzop-9 | last (45) | 1 | 2883584 (44.0) | host | 1 | 3145728 | 3145728 | 21/46 | 21 |
| lzop-9 | last (45) | 1 | 2883584 (44.0) | busybox | 1 | 3145728 | 3145728 | 21/46 | 21 |
| lzop-9 | last (45) | 1 | 2883584 (44.0) | deb10 | 1 | 3145728 | 3145728 | 21/46 | 21 |
| xz-6 | first (44) | 1 | 0 (0.0) | host | 1 | 0 | 0 | 0/46 | 0 |
| xz-6 | first (44) | 1 | 0 (0.0) | busybox | 0 | 0 | 0 | 0/46 | 0 |
| xz-6 | first (44) | 1 | 0 (0.0) | deb10 | 1 | 0 | 0 | 0/46 | 0 |
| xz-6 | middle (44) | 1 | 1441792 (22.0) | host | 1 | 1442248 | 1442248 | 0/46 | 0 |
| xz-6 | middle (44) | 1 | 1441792 (22.0) | busybox | 1 | 1442248 | 1442248 | 0/46 | 0 |
| xz-6 | middle (44) | 1 | 1441792 (22.0) | deb10 | 1 | 1442248 | 1442248 | 0/46 | 0 |
| xz-6 | last (44) | 1 | 2818048 (43.0) | host | 1 | 3196087 | 3196087 | 24/46 | 24 |
| xz-6 | last (44) | 1 | 2818048 (43.0) | busybox | 1 | 3196028 | 3196028 | 24/46 | 24 |
| xz-6 | last (44) | 1 | 2818048 (43.0) | deb10 | 1 | 3196087 | 3196087 | 24/46 | 24 |
| xz-9 | first (44) | 1 | 0 (0.0) | host | 1 | 0 | 0 | 0/46 | 0 |
| xz-9 | first (44) | 1 | 0 (0.0) | busybox | 0 | 0 | 0 | 0/46 | 0 |
| xz-9 | first (44) | 1 | 0 (0.0) | deb10 | 1 | 0 | 0 | 0/46 | 0 |
| xz-9 | middle (44) | 1 | 1441792 (22.0) | host | 1 | 1442248 | 1442248 | 0/46 | 0 |
| xz-9 | middle (44) | 1 | 1441792 (22.0) | busybox | 1 | 1442248 | 1442248 | 0/46 | 0 |
| xz-9 | middle (44) | 1 | 1441792 (22.0) | deb10 | 1 | 1442248 | 1442248 | 0/46 | 0 |
| xz-9 | last (44) | 1 | 2818048 (43.0) | host | 1 | 3196087 | 3196087 | 24/46 | 24 |
| xz-9 | last (44) | 1 | 2818048 (43.0) | busybox | 1 | 3196028 | 3196028 | 24/46 | 24 |
| xz-9 | last (44) | 1 | 2818048 (43.0) | deb10 | 1 | 3196087 | 3196087 | 24/46 | 24 |
| lzma-6 | first (45) | 1 | 0 (0.0) | host | 1 | 0 | 0 | 0/46 | 0 |
| lzma-6 | first (45) | 1 | 0 (0.0) | busybox | 1 | 0 | 0 | 0/46 | 0 |
| lzma-6 | first (45) | 1 | 0 (0.0) | deb10 | 1 | 0 | 0 | 0/46 | 0 |
| lzma-6 | middle (45) | 1 | 1441792 (22.0) | host | 1 | 1423786 | 1423786 | 0/46 | 0 |
| lzma-6 | middle (45) | 1 | 1441792 (22.0) | busybox | 1 | 0 | 0 | 0/46 | 0 |
| lzma-6 | middle (45) | 1 | 1441792 (22.0) | deb10 | 1 | 1423786 | 1423786 | 0/46 | 0 |
| lzma-6 | last (45) | 1 | 2883584 (44.0) | host | 1 | 3396716 | 3396716 | 39/46 | 39 |
| lzma-6 | last (45) | 1 | 2883584 (44.0) | busybox | 1 | 0 | 0 | 0/46 | 0 |
| lzma-6 | last (45) | 1 | 2883584 (44.0) | deb10 | 1 | 3396716 | 3396716 | 39/46 | 39 |
| lzma-9 | first (45) | 1 | 0 (0.0) | host | 1 | 0 | 0 | 0/46 | 0 |
| lzma-9 | first (45) | 1 | 0 (0.0) | busybox | 1 | 0 | 0 | 0/46 | 0 |
| lzma-9 | first (45) | 1 | 0 (0.0) | deb10 | 1 | 0 | 0 | 0/46 | 0 |
| lzma-9 | middle (45) | 1 | 1441792 (22.0) | host | 1 | 1423786 | 1423786 | 0/46 | 0 |
| lzma-9 | middle (45) | 1 | 1441792 (22.0) | busybox | 1 | 0 | 0 | 0/46 | 0 |
| lzma-9 | middle (45) | 1 | 1441792 (22.0) | deb10 | 1 | 1423786 | 1423786 | 0/46 | 0 |
| lzma-9 | last (45) | 1 | 2883584 (44.0) | host | 1 | 3396716 | 3396716 | 39/46 | 39 |
| lzma-9 | last (45) | 1 | 2883584 (44.0) | busybox | 1 | 0 | 0 | 0/46 | 0 |
| lzma-9 | last (45) | 1 | 2883584 (44.0) | deb10 | 1 | 3396716 | 3396716 | 39/46 | 39 |
| zstd-3 | first (44) | 1 | 0 (0.0) | host | 1 | 0 | 0 | 0/46 | 0 |
| zstd-3 | first (44) | 1 | 0 (0.0) | deb10 | 1 | 0 | 0 | 0/46 | 0 |
| zstd-3 | middle (44) | 1 | 1441792 (22.0) | host | 1 | 1441749 | 1441749 | 0/46 | 0 |
| zstd-3 | middle (44) | 1 | 1441792 (22.0) | deb10 | 1 | 1310720 | 1310720 | 0/46 | 0 |
| zstd-3 | last (44) | 1 | 2818048 (43.0) | host | 1 | 3014656 | 3014656 | 11/46 | 11 |
| zstd-3 | last (44) | 1 | 2818048 (43.0) | deb10 | 1 | 3014656 | 3014656 | 11/46 | 11 |
| zstd-19 | first (44) | 1 | 0 (0.0) | host | 1 | 0 | 0 | 0/46 | 0 |
| zstd-19 | first (44) | 1 | 0 (0.0) | deb10 | 1 | 0 | 0 | 0/46 | 0 |
| zstd-19 | middle (44) | 1 | 1441792 (22.0) | host | 1 | 1441792 | 1441792 | 0/46 | 0 |
| zstd-19 | middle (44) | 1 | 1441792 (22.0) | deb10 | 1 | 1441792 | 1441792 | 0/46 | 0 |
| zstd-19 | last (44) | 1 | 2818048 (43.0) | host | 1 | 3145728 | 3145728 | 21/46 | 21 |
| zstd-19 | last (44) | 1 | 2818048 (43.0) | deb10 | 1 | 3145728 | 3145728 | 21/46 | 21 |
| lz4-1 | first (46) | 1 | 0 (0.0) | deb10 | 0 | 0 | 0 | 0/46 | 0 |
| lz4-1 | first (46) | 1 | 0 (0.0) | ub24 | 0 | 0 | 0 | 0/46 | 0 |
| lz4-1 | middle (46) | 1 | 1507328 (23.0) | deb10 | 68 | 0 | 0 | 0/46 | 0 |
| lz4-1 | middle (46) | 1 | 1507328 (23.0) | ub24 | 68 | 0 | 0 | 0/46 | 0 |
| lz4-1 | last (46) | 1 | 2949120 (45.0) | deb10 | 68 | 0 | 0 | 0/46 | 0 |
| lz4-1 | last (46) | 1 | 2949120 (45.0) | ub24 | 68 | 0 | 0 | 0/46 | 0 |
| lz4-12 | first (45) | 1 | 0 (0.0) | deb10 | 0 | 0 | 0 | 0/46 | 0 |
| lz4-12 | first (45) | 1 | 0 (0.0) | ub24 | 0 | 0 | 0 | 0/46 | 0 |
| lz4-12 | middle (45) | 1 | 1441792 (22.0) | deb10 | 68 | 0 | 0 | 0/46 | 0 |
| lz4-12 | middle (45) | 1 | 1441792 (22.0) | ub24 | 68 | 0 | 0 | 0/46 | 0 |
| lz4-12 | last (45) | 1 | 2883584 (44.0) | deb10 | 68 | 0 | 0 | 0/46 | 0 |
| lz4-12 | last (45) | 1 | 2883584 (44.0) | ub24 | 68 | 0 | 0 | 0/46 | 0 |

## Damage, incompressible: a slice of 2 whole members of random/JPEG-like bytes. One flipped ciphertext byte, real age 1.1.1; exit statuses captured separately

| config | flip in chunk (of n) | age exit | age stdout bytes (= verified chunks x 65536) | reader | decoder exit | decoder bytes out | correct prefix of the cut | members whole / in slice | those restored equal |
|---|---|---|---|---|---|---|---|---|---|
| store | first (63) | 1 | 0 (0.0) | host | 0 | 0 | 0 | 0/2 | 0 |
| store | first (63) | 1 | 0 (0.0) | busybox | 0 | 0 | 0 | 0/2 | 0 |
| store | first (63) | 1 | 0 (0.0) | deb10 | 0 | 0 | 0 | 0/2 | 0 |
| store | middle (63) | 1 | 2031616 (31.0) | host | 0 | 2031616 | 2031616 | 0/2 | 0 |
| store | middle (63) | 1 | 2031616 (31.0) | busybox | 0 | 2031616 | 2031616 | 0/2 | 0 |
| store | middle (63) | 1 | 2031616 (31.0) | deb10 | 0 | 2031616 | 2031616 | 0/2 | 0 |
| store | last (63) | 1 | 4063232 (62.0) | host | 0 | 4063232 | 4063232 | 1/2 | 1 |
| store | last (63) | 1 | 4063232 (62.0) | busybox | 0 | 4063232 | 4063232 | 1/2 | 1 |
| store | last (63) | 1 | 4063232 (62.0) | deb10 | 0 | 4063232 | 4063232 | 1/2 | 1 |
| gzip-6 | first (63) | 1 | 0 (0.0) | host | 1 | 0 | 0 | 0/2 | 0 |
| gzip-6 | first (63) | 1 | 0 (0.0) | busybox | 1 | 0 | 0 | 0/2 | 0 |
| gzip-6 | first (63) | 1 | 0 (0.0) | deb10 | 1 | 0 | 0 | 0/2 | 0 |
| gzip-6 | middle (63) | 1 | 2031616 (31.0) | host | 1 | 2032605 | 2032605 | 0/2 | 0 |
| gzip-6 | middle (63) | 1 | 2031616 (31.0) | busybox | 1 | 2031616 | 2031616 | 0/2 | 0 |
| gzip-6 | middle (63) | 1 | 2031616 (31.0) | deb10 | 1 | 2032605 | 2032605 | 0/2 | 0 |
| gzip-6 | last (63) | 1 | 4063232 (62.0) | host | 1 | 4065233 | 4065233 | 1/2 | 1 |
| gzip-6 | last (63) | 1 | 4063232 (62.0) | busybox | 1 | 4063232 | 4063232 | 1/2 | 1 |
| gzip-6 | last (63) | 1 | 4063232 (62.0) | deb10 | 1 | 4065233 | 4065233 | 1/2 | 1 |
| gzip-9 | first (63) | 1 | 0 (0.0) | host | 1 | 0 | 0 | 0/2 | 0 |
| gzip-9 | first (63) | 1 | 0 (0.0) | busybox | 1 | 0 | 0 | 0/2 | 0 |
| gzip-9 | first (63) | 1 | 0 (0.0) | deb10 | 1 | 0 | 0 | 0/2 | 0 |
| gzip-9 | middle (63) | 1 | 2031616 (31.0) | host | 1 | 2032619 | 2032619 | 0/2 | 0 |
| gzip-9 | middle (63) | 1 | 2031616 (31.0) | busybox | 1 | 2031616 | 2031616 | 0/2 | 0 |
| gzip-9 | middle (63) | 1 | 2031616 (31.0) | deb10 | 1 | 2032619 | 2032619 | 0/2 | 0 |
| gzip-9 | last (63) | 1 | 4063232 (62.0) | host | 1 | 4065258 | 4065258 | 1/2 | 1 |
| gzip-9 | last (63) | 1 | 4063232 (62.0) | busybox | 1 | 4063232 | 4063232 | 1/2 | 1 |
| gzip-9 | last (63) | 1 | 4063232 (62.0) | deb10 | 1 | 4065258 | 4065258 | 1/2 | 1 |
| bzip2-9 | first (64) | 1 | 0 (0.0) | host | 2 | 0 | 0 | 0/2 | 0 |
| bzip2-9 | first (64) | 1 | 0 (0.0) | busybox | 1 | 0 | 0 | 0/2 | 0 |
| bzip2-9 | first (64) | 1 | 0 (0.0) | deb10 | 2 | 0 | 0 | 0/2 | 0 |
| bzip2-9 | middle (64) | 1 | 2097152 (32.0) | host | 2 | 1800000 | 1800000 | 0/2 | 0 |
| bzip2-9 | middle (64) | 1 | 2097152 (32.0) | busybox | 1 | 1798144 | 1798144 | 0/2 | 0 |
| bzip2-9 | middle (64) | 1 | 2097152 (32.0) | deb10 | 2 | 1800000 | 1800000 | 0/2 | 0 |
| bzip2-9 | last (64) | 1 | 4128768 (63.0) | host | 2 | 3600000 | 3600000 | 1/2 | 1 |
| bzip2-9 | last (64) | 1 | 4128768 (63.0) | busybox | 1 | 3600384 | 3600384 | 1/2 | 1 |
| bzip2-9 | last (64) | 1 | 4128768 (63.0) | deb10 | 2 | 3600000 | 3600000 | 1/2 | 1 |
| lzop-3 | first (63) | 1 | 0 (0.0) | host | 1 | 0 | 0 | 0/2 | 0 |
| lzop-3 | first (63) | 1 | 0 (0.0) | busybox | 1 | 0 | 0 | 0/2 | 0 |
| lzop-3 | first (63) | 1 | 0 (0.0) | deb10 | 1 | 0 | 0 | 0/2 | 0 |
| lzop-3 | middle (63) | 1 | 2031616 (31.0) | host | 1 | 1835008 | 1835008 | 0/2 | 0 |
| lzop-3 | middle (63) | 1 | 2031616 (31.0) | busybox | 1 | 1835008 | 1835008 | 0/2 | 0 |
| lzop-3 | middle (63) | 1 | 2031616 (31.0) | deb10 | 1 | 1835008 | 1835008 | 0/2 | 0 |
| lzop-3 | last (63) | 1 | 4063232 (62.0) | host | 1 | 3932160 | 3932160 | 1/2 | 1 |
| lzop-3 | last (63) | 1 | 4063232 (62.0) | busybox | 1 | 3932160 | 3932160 | 1/2 | 1 |
| lzop-3 | last (63) | 1 | 4063232 (62.0) | deb10 | 1 | 3932160 | 3932160 | 1/2 | 1 |
| lzop-9 | first (63) | 1 | 0 (0.0) | host | 1 | 0 | 0 | 0/2 | 0 |
| lzop-9 | first (63) | 1 | 0 (0.0) | busybox | 1 | 0 | 0 | 0/2 | 0 |
| lzop-9 | first (63) | 1 | 0 (0.0) | deb10 | 1 | 0 | 0 | 0/2 | 0 |
| lzop-9 | middle (63) | 1 | 2031616 (31.0) | host | 1 | 1835008 | 1835008 | 0/2 | 0 |
| lzop-9 | middle (63) | 1 | 2031616 (31.0) | busybox | 1 | 1835008 | 1835008 | 0/2 | 0 |
| lzop-9 | middle (63) | 1 | 2031616 (31.0) | deb10 | 1 | 1835008 | 1835008 | 0/2 | 0 |
| lzop-9 | last (63) | 1 | 4063232 (62.0) | host | 1 | 3932160 | 3932160 | 1/2 | 1 |
| lzop-9 | last (63) | 1 | 4063232 (62.0) | busybox | 1 | 3932160 | 3932160 | 1/2 | 1 |
| lzop-9 | last (63) | 1 | 4063232 (62.0) | deb10 | 1 | 3932160 | 3932160 | 1/2 | 1 |
| xz-6 | first (63) | 1 | 0 (0.0) | host | 1 | 0 | 0 | 0/2 | 0 |
| xz-6 | first (63) | 1 | 0 (0.0) | busybox | 0 | 0 | 0 | 0/2 | 0 |
| xz-6 | first (63) | 1 | 0 (0.0) | deb10 | 1 | 0 | 0 | 0/2 | 0 |
| xz-6 | middle (63) | 1 | 2031616 (31.0) | host | 1 | 2032037 | 2032037 | 0/2 | 0 |
| xz-6 | middle (63) | 1 | 2031616 (31.0) | busybox | 1 | 2032037 | 2032037 | 0/2 | 0 |
| xz-6 | middle (63) | 1 | 2031616 (31.0) | deb10 | 1 | 2032037 | 2032037 | 0/2 | 0 |
| xz-6 | last (63) | 1 | 4063232 (62.0) | host | 1 | 4064217 | 4064217 | 1/2 | 1 |
| xz-6 | last (63) | 1 | 4063232 (62.0) | busybox | 1 | 4064217 | 4064217 | 1/2 | 1 |
| xz-6 | last (63) | 1 | 4063232 (62.0) | deb10 | 1 | 4064217 | 4064217 | 1/2 | 1 |
| xz-9 | first (63) | 1 | 0 (0.0) | host | 1 | 0 | 0 | 0/2 | 0 |
| xz-9 | first (63) | 1 | 0 (0.0) | busybox | 0 | 0 | 0 | 0/2 | 0 |
| xz-9 | first (63) | 1 | 0 (0.0) | deb10 | 1 | 0 | 0 | 0/2 | 0 |
| xz-9 | middle (63) | 1 | 2031616 (31.0) | host | 1 | 2032037 | 2032037 | 0/2 | 0 |
| xz-9 | middle (63) | 1 | 2031616 (31.0) | busybox | 1 | 2032037 | 2032037 | 0/2 | 0 |
| xz-9 | middle (63) | 1 | 2031616 (31.0) | deb10 | 1 | 2032037 | 2032037 | 0/2 | 0 |
| xz-9 | last (63) | 1 | 4063232 (62.0) | host | 1 | 4064217 | 4064217 | 1/2 | 1 |
| xz-9 | last (63) | 1 | 4063232 (62.0) | busybox | 1 | 4064217 | 4064217 | 1/2 | 1 |
| xz-9 | last (63) | 1 | 4063232 (62.0) | deb10 | 1 | 4064217 | 4064217 | 1/2 | 1 |
| lzma-6 | first (64) | 1 | 0 (0.0) | host | 1 | 0 | 0 | 0/2 | 0 |
| lzma-6 | first (64) | 1 | 0 (0.0) | busybox | 1 | 0 | 0 | 0/2 | 0 |
| lzma-6 | first (64) | 1 | 0 (0.0) | deb10 | 1 | 0 | 0 | 0/2 | 0 |
| lzma-6 | middle (64) | 1 | 2097152 (32.0) | host | 1 | 2070421 | 2070421 | 0/2 | 0 |
| lzma-6 | middle (64) | 1 | 2097152 (32.0) | busybox | 1 | 0 | 0 | 0/2 | 0 |
| lzma-6 | middle (64) | 1 | 2097152 (32.0) | deb10 | 1 | 2070421 | 2070421 | 0/2 | 0 |
| lzma-6 | last (64) | 1 | 4128768 (63.0) | host | 1 | 4076319 | 4076319 | 1/2 | 1 |
| lzma-6 | last (64) | 1 | 4128768 (63.0) | busybox | 1 | 0 | 0 | 0/2 | 0 |
| lzma-6 | last (64) | 1 | 4128768 (63.0) | deb10 | 1 | 4076319 | 4076319 | 1/2 | 1 |
| lzma-9 | first (64) | 1 | 0 (0.0) | host | 1 | 0 | 0 | 0/2 | 0 |
| lzma-9 | first (64) | 1 | 0 (0.0) | busybox | 1 | 0 | 0 | 0/2 | 0 |
| lzma-9 | first (64) | 1 | 0 (0.0) | deb10 | 1 | 0 | 0 | 0/2 | 0 |
| lzma-9 | middle (64) | 1 | 2097152 (32.0) | host | 1 | 2070421 | 2070421 | 0/2 | 0 |
| lzma-9 | middle (64) | 1 | 2097152 (32.0) | busybox | 1 | 0 | 0 | 0/2 | 0 |
| lzma-9 | middle (64) | 1 | 2097152 (32.0) | deb10 | 1 | 2070421 | 2070421 | 0/2 | 0 |
| lzma-9 | last (64) | 1 | 4128768 (63.0) | host | 1 | 4076319 | 4076319 | 1/2 | 1 |
| lzma-9 | last (64) | 1 | 4128768 (63.0) | busybox | 1 | 0 | 0 | 0/2 | 0 |
| lzma-9 | last (64) | 1 | 4128768 (63.0) | deb10 | 1 | 4076319 | 4076319 | 1/2 | 1 |
| zstd-3 | first (63) | 1 | 0 (0.0) | host | 1 | 0 | 0 | 0/2 | 0 |
| zstd-3 | first (63) | 1 | 0 (0.0) | deb10 | 1 | 0 | 0 | 0/2 | 0 |
| zstd-3 | middle (63) | 1 | 2031616 (31.0) | host | 1 | 2031558 | 2031558 | 0/2 | 0 |
| zstd-3 | middle (63) | 1 | 2031616 (31.0) | deb10 | 1 | 1966080 | 1966080 | 0/2 | 0 |
| zstd-3 | last (63) | 1 | 4063232 (62.0) | host | 1 | 4063129 | 4063129 | 1/2 | 1 |
| zstd-3 | last (63) | 1 | 4063232 (62.0) | deb10 | 1 | 3932160 | 3932160 | 1/2 | 1 |
| zstd-19 | first (63) | 1 | 0 (0.0) | host | 1 | 0 | 0 | 0/2 | 0 |
| zstd-19 | first (63) | 1 | 0 (0.0) | deb10 | 1 | 0 | 0 | 0/2 | 0 |
| zstd-19 | middle (63) | 1 | 2031616 (31.0) | host | 1 | 2032828 | 2032828 | 0/2 | 0 |
| zstd-19 | middle (63) | 1 | 2031616 (31.0) | deb10 | 1 | 1966080 | 1966080 | 0/2 | 0 |
| zstd-19 | last (63) | 1 | 4063232 (62.0) | host | 1 | 4063232 | 4063232 | 1/2 | 1 |
| zstd-19 | last (63) | 1 | 4063232 (62.0) | deb10 | 1 | 4063232 | 4063232 | 1/2 | 1 |
| lz4-1 | first (63) | 1 | 0 (0.0) | deb10 | 0 | 0 | 0 | 0/2 | 0 |
| lz4-1 | first (63) | 1 | 0 (0.0) | ub24 | 0 | 0 | 0 | 0/2 | 0 |
| lz4-1 | middle (63) | 1 | 2031616 (31.0) | deb10 | 68 | 2031605 | 2031605 | 0/2 | 0 |
| lz4-1 | middle (63) | 1 | 2031616 (31.0) | ub24 | 68 | 2031605 | 2031605 | 0/2 | 0 |
| lz4-1 | last (63) | 1 | 4063232 (62.0) | deb10 | 68 | 4063221 | 4063221 | 1/2 | 1 |
| lz4-1 | last (63) | 1 | 4063232 (62.0) | ub24 | 68 | 4063221 | 4063221 | 1/2 | 1 |
| lz4-12 | first (63) | 1 | 0 (0.0) | deb10 | 0 | 0 | 0 | 0/2 | 0 |
| lz4-12 | first (63) | 1 | 0 (0.0) | ub24 | 0 | 0 | 0 | 0/2 | 0 |
| lz4-12 | middle (63) | 1 | 2031616 (31.0) | deb10 | 68 | 2031605 | 2031605 | 0/2 | 0 |
| lz4-12 | middle (63) | 1 | 2031616 (31.0) | ub24 | 68 | 2031605 | 2031605 | 0/2 | 0 |
| lz4-12 | last (63) | 1 | 4063232 (62.0) | deb10 | 68 | 4063221 | 4063221 | 1/2 | 1 |
| lz4-12 | last (63) | 1 | 4063232 (62.0) | ub24 | 68 | 4063221 | 4063221 | 1/2 | 1 |

## 7. Readers available per release

| algorithm | Debian 10 (stock debian:10-slim) | Debian 10 after `apt install` (package) | Ubuntu 24.04 (this host; stock docker image) | BusyBox applet (1.30.1 Debian 10, 1.36.1 host) |
|---|---|---|---|---|
| gzip | yes (gzip 1.9, required) | - | yes (1.12, required; image has it) | `gunzip`, both |
| bzip2 | no | bzip2 1.0.6 | host yes (1.0.8, standard); image no | `bunzip2`, both |
| xz | no | xz-utils 5.2.4 | host yes (5.4.5, standard); image no | `unxz`, both |
| lzma (alone) | no | xz-utils (`lzma`, `unlzma`, `xz --format=lzma`) | host yes; image no | `unlzma`, both |
| zstd | no | zstd 1.3.8 | host yes (1.5.5, optional); image no | none |
| lzop | no | lzop 1.03 | host yes (1.04, optional); image no | `lzop`, both |
| lz4 | no | lz4 1.8.3 | NOT on this host; ubuntu:24.04 package lz4 1.9.4 (not in the image) | none |

The tools of Debian 10 are not in its minimal image (a stock install has only gzip and tar 1.30); "stock install" for the Ubuntu 24.04 host means this full install, where gzip, bzip2 and xz have priority required/standard and zstd/lzop optional.

## 8. The positive control

A slice stored with no compression (`store`, decoder `cat`/`busybox cat`) went through the same harness first: 5/5 on every one of the five readers. The negative control: a copy with one byte of slice 0007 flipped made the every-slice test FAIL (11/12) on the host reader, so a failure is visible (the one small-file test whose flipped byte fell in an ignorable header field still extracted equal, which is why the every-slice test is the first of five). The damage rows of `store` behave as pass-criteria predicted: age exits 1 and writes exactly the verified chunks, `cat` exits 0 on the short input (so a decoder's exit alone says nothing for stored slices).

## Reading the damage rows

- `age -d` exits 1 in every case ("failed to decrypt and authenticate payload chunk") and writes the verified chunks before the flipped one: 0 bytes for the first chunk, k x 65536 for chunk k. No decoder ever wrote a byte that was not a correct prefix of the cut (checked on all 3 x 144 rows).
- After age stops, the decoder gets a short frame. Exit codes seen on the middle and last chunk: gzip 1, bzip2 2 (BusyBox 1), xz 1, lzma 1, zstd 1, lzop 1, lz4 68 (all on every reader). The decoder writes what it could decode, always a correct prefix of the cut.
- **Exit 0 on a short input, which misleads:** when the flip is in the first chunk the decoder gets EMPTY input. BusyBox `unxz` (1.30.1 and 1.36.1) and `lz4 -dc` (1.8.3 and 1.9.4) then exit 0 and write nothing; every other decoder exits non-zero (xz itself 1, `unlzma` 1, gzip 1, bzip2 2, zstd 1, lzop 1). So: judge by age's exit (always 1) and the per-file sha256, never by a decoder's exit alone.
- Members whole after damage: only those that end before the flipped chunk's decoded position; a flip in the last chunk loses only the tail (the 46-member slice: store 41/46, xz 24/46 whole, because the xz-packed stream decodes less than the bytes before the chunk).
