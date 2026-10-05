# Plaintext-free staging, reading sources once, and the dar catalogues on tape

*Research for the CTO's rulings of 2026-10-05 on #370 (stream dar straight into
encryption, provably no plaintext on the staging device), #364 (read sources once,
reversing #354's refusal-before-dar), #418 (dar catalogues at the front of the tape),
and the added question on duplicated per-file metadata (#380, #381). Research only: no
product code changed. Author: research agent, 2026-10-06.*

Every measurement was taken on vm-desk1 (Ubuntu, kernel 6.8, dar **2.7.13**, age
**1.1.1**, ext4) against small test trees under `/scratch/research-plainfree/`, and,
for the positive control, the real `tapectl` built from master `f3a34d2` (1.0.7,
debug build). Facts are tagged **MEASURED** (run here, numbers below), **CODE** (read
in this repository at `f3a34d2`), **DOC** (dar's man page as installed, `man dar`), or
**INFERRED** (reasoned, not run). Absolute timings on this VM say nothing about home2;
only the byte counts, file counts and behaviours carry over.

---

## 1. Summary

**Recommendation.** Run dar with its archive on **standard output**
(`dar -c - … --retry-on-change 0 -@ <catalogue>`), and let tapectl cut that stream into
dar slices itself, writing each slice's 50-byte dar slice header and 1-byte trailer
around the payload, and encrypting each slice in-process straight into its `.age`
file. Measured here: the cut slices are byte-for-byte what `dar_xform` produces from
the same stream, apart from the random 10-byte internal name. `dar -t` and `dar -x`
accept them, and the extract is identical to the source. A raw scan of the staging
filesystem image finds **0** of the planted markers, where tapectl 1.0.7 leaves
**215** after deleting its plaintext slices. The on-tape slice format does not change
and RESTORE.sh does not change. The isolated catalogue is written by dar's on-the-fly
isolation (`-@`) directly to the tapectl home, so it never touches the staging device.

**Three facts measured here decide the design:**

1. **Per-slice FIFOs cannot work.** dar opens every slice `O_RDWR|O_CREAT|O_EXCL`. A
   FIFO created in advance gets `EEXIST`, and dar then opens it `O_RDONLY` to read its
   header, which deadlocks. With `-w`, dar unlinks the FIFOs and creates regular files.
   dar also calls `lseek` on every slice, which a pipe refuses (§3.2).
2. **dar rewinds its slices when a file changes during the read.** Under the default
   `--retry-on-change`, dar reopened slice 1 (`O_RDWR|O_CREAT`, no `O_EXCL`), seeked
   back to byte 209, re-created slices 2–7 and wrote the file again (§3.3). So **every**
   design that consumes a slice as soon as dar closes it is unsafe without
   `--retry-on-change 0`: the per-slice FUSE target, an LD_PRELOAD shim, a tmpfs with
   the `-E` hook, and **#367's planned `-E` pipelining**. On standard output dar cannot
   seek, so the stream stays sequential either way.
3. **The slice layer is separable from the archive.** A sliced archive's payload
   (each slice minus its header and trailer) is byte-identical to the `dar -c -` stream
   minus its 38-byte header and trailer, apart from the random label (§3.4). Slicing is
   framing around one logical stream, so tapectl can apply it.

**#418 is largely built already.** Every tenant envelope carries `catalogs/` with that
tenant's isolated dar catalogues for the volume. The operator envelope and its backup
carry all of them, plus `catalog.db`. All of these sit in the encrypted zone directly
after the front index (`build.rs` `catalogs_for_tenant`/`catalogs_for_all`,
volume-format-v2 §1). What is missing is any **heir tool that reads them**: RESTORE.sh
and RECOVERY.md never mention them. Also missing are a backup copy of each tenant
envelope and a cross-volume index (§6–§7).

**The duplication question (§6)** has a short answer. On tape the duplication is
deliberate and costs about 140 MiB per 181k-file tape (≈0.006 % of an LTO-6). The real
growth is in `tapectl.db`: about 40 MiB per 181k-file *version*, every version, which
is #380's problem. The proposal makes paths interned and membership rows narrow, and
makes dar's catalogue the single authority for the attributes dar already records
(permissions, owners, xattrs, special files, hard links) instead of copying them into
the database.

The numbered CTO decisions are in §9.

---

## 2. Q1 — where staging writes plaintext derived from user data today

The tapectl home (`~/.tapectl`, or `--home`) and the staging directory
(`[staging] directory`) are separate settings. On home2 they are separate devices: the
staging directory is `/srv/acache`, a RAID0 under LUKS (#364). Sensitive means it
reveals content, file names, paths, unit or tenant names, or per-file sizes.

| # | Artifact | Writer (CODE) | Device | Contents | Sensitive | Lifetime |
|---|---|---|---|---|---|---|
| 1 | Plaintext dar slices `{uuid12}_v{V}_s{id}.N.dar` | `staging/mod.rs:617` → `dar/create.rs:37-94` (`dar -c <staging>/<base> -s …`) | **staging** | the whole archive: file data, and the internal catalogue in the last slice | **yes: content, names, sizes, owners** | All exist together once dar ends. Each is unlinked after its `.age` is recorded (`record_encrypted_slice`, `mod.rs:915`). **Blocks persist after unlink** (§8, MEASURED). |
| 2 | Isolated catalogue `catalogs/{uuid8}/{uuid8}_v{V}.1.dar` | `dar -C … -A <staging archive>` (`dar/create.rs:227`, `mod.rs:664`) | home (reads #1 from staging) | every path, size, mode, owner, times, per-file CRC, xattr/FSA presence, hard-link groups | yes: names and metadata | permanent; the input to every envelope build |
| 3 | `.dar.age` slices | `encrypt_file_streaming` (`mod.rs:1663`) | staging | ciphertext | no | until the volume write or `staging clean` |
| 4 | Probe `.tapectl-stage-probe-<pid>` | `prepare_staging_dir` (`mod.rs:1292`) | staging | fixed string | no | removed at once |
| 5 | **`sessions/<label>-<uuid>/catalog_snapshot.db`** | `volume write` (`volume/write.rs:1507-1522`) | **staging** | plaintext SQLite: tenants, units, snapshots (incl. `source_path`), every file's path, size, mtime and sha256, stage-set recipient lists | **yes** | **until `staging clean`** reclaims the session directory (`staging/clean.rs:702-747`); nothing removes it after confirm. Blocks persist after that. |
| 6 | Session zones: ID thunk, guide, RESTORE.sh, front index, seal, `layout.json` | `volume/build.rs` | staging | the public on-tape zones; layout.json has staging paths (`{uuid12}_…`), sizes and ciphertext hashes | no (public by the isolation invariant) | until `staging clean` |
| 7 | Session envelopes (`*_envelope*`) | `materialize_envelope_streaming` (`build.rs:917`) | staging | age ciphertext; the tar is built inside the age stream | no | until `staging clean` |
| 8 | `clone-{label}-{unit_name}/` (read-slices), `compact-{label}/` | `volume/write.rs:4545-4547`, `:4757` | staging | ciphertext slices, but **the unit name is in the directory name** | **yes (unit name)** | until clean |
| 9 | `tapectl.db` (+ WAL) | everywhere | home | `files` (path, size, mtime, sha256, type), FTS, units, tenants, … | yes | permanent |
| 10 | Stage report `stage-reports/*.txt` | `write_stage_report` (`mod.rs:1072`) | home | unit and tenant names, per-slice `sha256_plain` | yes (names) | permanent |
| 11 | Session log `logs/<UTC>-<cmd>-<pid>.log` | `progress.rs` + INFO tracing tee | home | the dar command line (source path, exclude masks), unit names, and errors naming relative paths (`BITROT suspected: <path>`, `validate.rs:266`) | yes (names) | permanent |
| 12 | dar's own temp files | none observed: strace shows no `O_CREAT` besides the slices (MEASURED) | — | — | — | — |
| 13 | SQLite temp files (large sorts) | SQLite | `SQLITE_TMPDIR`/`/var/tmp`/`/tmp` (root device) | fragments of query results | possible | transient |
| 14 | `catalog rebuild` scratch `$TMPDIR/tapectl-rebuild-<pid>` | `cli/catalog.rs:588` | root/tmpfs | decrypted envelope members, incl. `catalog.db` | yes | DR path only, not staging |
| 15 | Swap; core dumps | kernel; systemd-coredump | swap device; `/var/lib/systemd/coredump` | any plaintext buffer of tapectl or dar | yes | out of tapectl's control |

Restore (`restore unit`, RESTORE.sh) writes plaintext into `--to` and its scratch
directory (#406), never the staging directory. That holds unless an operator points
`--to` at the staging device; the gate in §8 should cover that case with a refusal.

**Positive control, MEASURED (E14).** tapectl 1.0.7 `stage create` was run with the
staging directory on a fresh `sync,nodiscard` loop-mounted ext4 image. The unit had 210
files and 50 MiB, with a marker planted in file contents and file names. After the
stage, only `.age` files were visible in the directory. Grepping the **raw image**
found: content marker 6/6, small-file marker 200/200, 215 marker hits in total, and the
unit name `secretunit` once (inside the archived `.tapectl-unit.toml`). The home device
held the marker in `tapectl.db` and `catalogs/…/…_v1.1.dar`, as the table predicts.

**Items to fix for the requirement** (it concerns the staging device): #1 is the core
of #370. #5 and #8 are separate and easy. Items 2, 9, 10 and 11 are on the home device
and need a ruling (§9 D5). Swap and core dumps (#15) need an operational rule on home2
(§9 D6).

---

## 3. Q2 — handing dar's output to age without plaintext on disk

### 3.1 What dar does to a slice it writes (MEASURED, E1b)

`strace -f` of `dar -c arch -R src -s 4M -an -D -Q --fsa-scope extX`, with the syscalls
on each slice's descriptor collapsed:

```
arch.1.dar: open(O_RDWR|O_CREAT|O_EXCL) ; write x17 total=50 ; lseek(0, SEEK_CUR)=50 ;
            write x41 total=4194253 ; lseek(0, SEEK_SET)=0 ; lseek(4194303, SEEK_CUR)=4194303 ;
            lseek(0, SEEK_CUR)=4194303 ; write x1 total=1 ; close
arch.2.dar: open(O_RDWR|O_CREAT|O_EXCL) ; write x59 total=4194303 ; lseek(0, SEEK_SET)=0 ;
            lseek(4194303, SEEK_CUR)=4194303 ; lseek(0, SEEK_CUR)=4194303 ; write x1 total=1 ; close
arch.8.dar: open(O_RDWR|O_CREAT|O_EXCL) ; write x39 total=2168689 ; lseek(0, SEEK_END)=2168689 ;
            lseek(0, SEEK_CUR)=2168689 ; write x1 total=9 ; lseek(0, SEEK_END)=2168698 ;
            lseek(0, SEEK_CUR)=2168698 ; write x1 total=1 ; close
```

In the normal case the writes are strictly sequential: every seek lands on the current
position, and dar never reads, stats, fsyncs or reopens a finished slice. dar does
seek, though, and opens with `O_EXCL`. The last byte of every slice is a flag: `N` for
non-terminal, `T` for the last slice. The 50-byte header is identical in every slice of
a set (§3.4).

### 3.2 Candidates, measured

| Option | Result | On-tape bytes |
|---|---|---|
| **A. Per-slice FIFOs** as slice targets | **Fails.** Without `-w`: `EEXIST`, then dar opens the FIFO `O_RDONLY` to read its header and blocks forever (E2a, killed at the timeout). With `-w`: dar `unlink`s every `arch.N.dar` up front and creates regular files (E2b). It would also `lseek` a pipe (`ESPIPE`). | n/a |
| **B. `-E` hook** (encrypt and delete each finished slice) | Works mechanically. The hook runs **synchronously** after `close` and before the next slice's `open`, with `%c` = `operation`/`last_slice` (E3c). **The slice is already on disk when the hook runs**, so this is #367's pipelining, not #370's remedy. It is **unsafe with dar's default retry-on-change** (§3.3). | unchanged |
| **C. tmpfs for the plaintext slices** + B | No dar change, and slices stay byte-identical. Costs RAM of at least one slice (10 GiB at the default `slice_size`) plus the next being written. tmpfs pages can be swapped, so swap must be off or encrypted, and `--retry-on-change 0` is required (§3.3). Waiting for dar to finish before encrypting needs RAM of the whole unit (470 GiB for keepsake/video), so that variant is impossible. | unchanged |
| **D. FUSE target** (tapectl mounts a write-only filesystem whose `create`+`write` feed age) | Not built. INFERRED from §3.1: the kernel resolves `lseek` locally for FUSE files (FUSE forwards only `SEEK_DATA`/`SEEK_HOLE`), so dar's seeks never reach the daemon. Writes arrive in order, and the daemon can fail closed (`EIO`) on any write that is not at the current end. Costs: a new dependency (`fuser`), `/dev/fuse` and `fusermount3` for the `tapectl` service user, stale-mount cleanup after a crash. Needs `--retry-on-change 0`. | unchanged |
| **E. LD_PRELOAD shim** around open/lseek/write | Not built. It would work for the same reason as D, but breaks with a static dar or a libc change. **Lab instrument only**, never product. | unchanged |
| **F. `dar -c -` + tapectl cuts slices with dar's own framing** (recommended) | **Works** (E9, E13). Details in §3.4. | **unchanged**: valid, self-standing dar slices |
| **G. `dar -c -` + tapectl cuts raw chunks** (no dar headers), or `dar_split` | Works for a full sequential restore (`cat` the chunks into `dar -x - --sequential-read`). A chunk is **not a dar slice**, so RESTORE.sh, RECOVERY.md and the system guide must change, dar's direct-access restore (jump to a file's slice) is lost, and slices stop being self-standing. | **format change (minor version, golden re-pin)** |
| H. libdar API with a custom "entrepot" (C++ FFI) | Not pursued: a C++ FFI surface far heavier than F. | unchanged |

`dar -c - -s 4M` is refused outright: `Parse error: Slicing (-s option), is not
compatible with archive on standard output` (E3a, MEASURED). So dar cannot slice its
own stream, and option F does the slicing in tapectl.

### 3.3 The rewind: what dar does when a file changes while it is read (MEASURED, E8)

A 300 MiB file was `touch`ed every 10 ms while dar archived it with `-s 50M`. Under the
**default** `--retry-on-change` (3 retries), dar wrote slices 1–7. Then it reopened
slice 1 (`open(O_RDWR|O_CREAT|O_EXCL)` → `EEXIST` → `open(O_RDONLY)` →
`open(O_RDWR|O_CREAT)`), seeked to offset 209 (the start of `big.bin`'s data),
re-created slices 2–7 with `O_EXCL` (so it had unlinked them), and saved the file
again. It did this twice and then succeeded with `dirty="no"`. The man page confirms
this is intended: "since release 2.5.0, in normal condition no byte is wasted when a
file changed at the time it was read for backup, except when doing a backup to pipe"
(DOC, `-_`/`--retry-on-change`).

With `--retry-on-change 0`, each slice was opened exactly once, and dar recorded
`big.bin` with `dirty="yes"` and **exit code 11**: "some saved files have changed while
dar was reading them" (DOC, EXIT CODES).

Consequences:

- Any design that encrypts and deletes a slice as soon as dar closes it (B, C, D, E,
  and #367 as written) **must** pass `--retry-on-change 0`. Otherwise a retry reopens a
  slice that is already gone and dar silently writes a broken archive.
- On standard output (F, G), dar cannot seek, so a retry appends a second copy of the
  file ("wasted bytes"). The stream stays sequential and valid even with retries on.
  `--retry-on-change 0` is still recommended for F: no wasted bytes, and exit 11 maps
  cleanly to tapectl's existing **DIRTY** refusal.
- Behaviour change to rule on (§9 D3): today a file that changes during dar's read is
  silently re-saved, up to 3 times. With retry 0, the stage of an actively changing
  source is refused. Today's tapectl already fails on exit 11 (`create_archive` treats
  any non-zero exit as failure, `dar/create.rs:96`), so only the retry window
  disappears.

### 3.4 Option F in detail: slice framing is separable (MEASURED, E3b/E9)

- `dar -c -` (no `-s`) produced 31,528,458 bytes, the same size as the single-slice
  file archive of the same tree. `cmp` shows 9 differing bytes, all in the random label
  and its repeats.
- The stream's header is 38 bytes: magic `0000007b`, a 10-byte internal name, flag
  `T`, `T` (no extension), a TLV list holding the data name. A multi-slice header is 50
  bytes: magic, internal name, **`E` ("flag at the end")**, `T`, and a TLV list holding
  the slice size and the data name. Every slice of a set carries the **same** 50 bytes;
  the per-slice difference is only the trailing `N`/`T` byte.
- Payload equivalence: concatenating slices 1–8 minus their 50-byte headers and 1-byte
  trailers gives 31,528,419 bytes. The stream minus its 38-byte header and trailer gives
  31,528,419 bytes. They differ only in **10 bytes, all label positions**. dar's
  catalogue records logical offsets, so slicing does not change the archive.
- **Hand re-slicing vs dar's own tool:** the prototype `reslice.py` (header template
  from any dar slice of the same `-s`, labels swapped for the stream's) produced 8
  slices. Compared with `dar_xform -s 4M` of the same stream, **each slice differs in
  exactly 2 bytes: the internal name**, which dar_xform re-randomises. `dar -t` passed,
  and `dar -x` gave a tree identical to the source (`diff -r`).
- **End to end (E13):** `dar -c - … | stream_slicer.py` (cut, then pipe each slice into
  `age -r` writing `.dar.age`) restored identically after `age -d` → `dar -t` →
  `dar -x` → `diff -r`. The on-the-fly catalogue matched the cut slices: `dar -t … -A
  <onfly catalogue>` passed. A catalogue from another run was refused with "do not
  correspond to the same data", which is the positive control for the label check.

**How tapectl would get the header right without hand-encoding dar's format.**
Before each stage, run dar on an **empty directory** with the same `-s` string and read
the 50-byte header from that slice (E13 used exactly this). This gives tapectl:

- the header layout of the installed dar version;
- the slice size **as dar parses the operator's string**, as an infinint in the TLV.
  This keeps issue #59's rule that dar, not tapectl, parses `-s`.

tapectl then substitutes the stream's two labels and checks the stream header's magic
and `TT` flags. Any mismatch is a refusal, never a guess. A unit test should pin
tapectl's framing against `dar_xform` output for 1 MiB, 4 MiB and 10 GiB slice sizes,
so a dar upgrade that changes the sar header fails a test rather than a tape.

**The isolated catalogue under F (MEASURED, E5/E10).**

- `-@ <path>` (on-the-fly isolation) works while the archive goes to standard output.
  It writes the catalogue through a single `O_EXCL` open, sequentially, to any path, so
  it can go straight to `<home>/catalogs`.
- Its listing is identical to the classic `dar -C` catalogue (md5 of the sorted
  `dar -l`, 49,797-entry tree).
- dar always compresses an on-the-fly catalogue (bzip2 if available, DOC). Re-isolating
  it with `dar -C <final> -A <onfly> -znone` reproduces the classic catalogue: same
  5,512,451 bytes, same listing, 22 bytes of label/time difference.
- Isolating from a stream instead (`dar -C cat -A - --sequential-read`) produced a
  catalogue whose listing differs (sequential-read catalogue), so it is not recommended.

**Throughput (indicative only, MEASURED E13, 31 MiB tree, sync-mounted loop ext4).**

- Today's shape: 9.8 s, 99 MiB written to the staging device.
- F: 3.8 s, 59 MiB written.
- With a normal (async) mount: 1.9 s against 1.1 s.

The structural gain is what transfers to home2: the staging disk sees one ciphertext
write instead of a plaintext write, a plaintext read-back and a ciphertext write. The
home2 benchmark #364 asks for is still owed.

### 3.5 Recommended Q2 design

```
dar -c - -R <src> -an -D -Q --fsa-scope … [-u "*"] [-X/-P masks] -N --retry-on-change 0 \
    -@ <home>/catalogs/<uuid8>/<base>.onfly
   │ stdout (a pipe; nothing on disk)
   ▼
tapectl slicer thread: check the 38-byte stream header; per slice N:
   header(50, from dar's own empty-dir template, labels swapped) + payload chunk + 'N'/'T'
   │ bounded in-RAM queue (no plaintext on disk; RAM = queue bound)
   ▼
per-slice encryptor: tee → sha256_plain hasher thread
                      age StreamWriter → tee → sha256_encrypted hasher → <staging>/<base>.N.dar.age.partial
                      fsync → rename to .dar.age → stage_slices row
dar exit 0  → re-isolate catalogue uncompressed on the home device (dar -C … -znone), remove .onfly
dar exit 11 → DIRTY refusal (same text family as validate.rs)
other / any slicer error → kill dar, remove every .partial and .age of the set
```

- `-N` (ignore any darrc) is #412 item 9, and belongs in the same argv.
- Slices are produced in order, so slice N's encryption overlaps dar producing slice
  N+1, bounded by RAM, not disk. That is #367's goal without its plaintext.
- Encryption stays one age stream per slice, with the slice as the unit: no format
  change.

**Make the guarantee structural, not just tested.** Give staging a `StagingDir` type
whose only way to create a file is `create_encrypted(name, recipients) -> AgeWriter`,
plus the fixed probe. Give dar's argv builder no path under the staging directory at
all: the archive argument is the literal `-`, asserted by a unit test. Plaintext on the
staging device then becomes unrepresentable in the staging module, and §8's tests are
evidence that the construction holds.

**Also required for the staging-device requirement:**

- Move `catalog_snapshot.db` (#5) off staging: build it in the tapectl home (e.g.
  `<home>/tmp/`, removed after `build()`), or in memory with SQLite `serialize`
  streamed into the tar member.
- Rename `clone-{label}-{unit_name}` (#8) to use the unit uuid.

---

## 4. Q3 — reading the source once

### 4.1 What #354 bought, and what reversing it loses

Today `validate_source` (`validate.rs:67`) reads and hashes every file **before** dar,
in database path order. Before dar starts it refuses:

- **BITROT** (same size, different hash against an existing baseline);
- **DIRTY** (size changed);
- **MISSING**;

and it measures `nonzero_bytes`, the true lower bound on dar's archive that drives the
pre-dar space refusal (`check_staging_space`, `mod.rs:566`). Then dar reads everything
again (#364 comment, 2026-10-05).

What we lose by reversing it:

1. **Refusal before any dar work.** A BITROT, DIRTY or MISSING file is now found while
   dar runs, so the dar work done before it is wasted. The waste is bounded by the
   hasher's lead plus the bad file's position in the walk, and dar is killed at once.
2. **The `nonzero_bytes` lower bound before dar.** Under F the staging device holds
   only ciphertext, about the archive size. That is bounded **above** by the
   snapshot's apparent size plus small overhead, and a sparse file only makes it
   smaller. The check becomes: free ≥ upper bound → proceed; otherwise ask (Tier-2) or
   `--yes`, and an `ENOSPC` mid-stream is a clean abort that removes the set's `.age`
   files. Peak staging use also drops by one plaintext slice (today: whole plaintext
   archive + 1 ciphertext slice; F: ciphertext only).
3. **A hash computed with no dar in flight.** Nothing else, as §4.3 shows.

### 4.2 Options

| Option | Single read? | Integrity binding of `files.sha256` to the archived bytes | Verdict |
|---|---|---|---|
| (a) Status quo: hash pass, then dar | no (2 disk reads) | **none**: two reads minutes to hours apart, and a change between them is never detected (#364 comment) | the baseline being replaced |
| (b) **Concurrent hasher in dar's walk order, bounded lead** | yes from disk (dar's read hits page cache) | **strong with the stat rule below**: ctime cannot be set by a writer, and dar's own retry-0 dirty check covers dar's read | **recommended** |
| (c) FUSE read-only passthrough of the source that hashes as dar reads | exactly one read | exact | rejected: FUSE does not pass `FS_IOC_GETFLAGS` (extX FSA, `preserve_fsa`); inode/hard-link fidelity, sparse holes, xattrs and throughput at 160+ MB/s all at risk |
| (d) LD_PRELOAD on dar's `read()` | exactly one read | exact | rejected: fragile, as in §3.2 E |
| (e) Drop the sha256 and rely on dar's per-file CRC | yes | CRC only, not a cryptographic hash; dar's XML shows a 32-bit `crc="0d33b6b7"` for 5 MiB files (MEASURED) | rejected: loses BITROT detection across re-stages, `checksum_mode = "sha256"` (`content_match.rs`), and `files.sha256` on tape |
| (f) Parse dar's stream to hash file data | yes | exact | rejected: tapectl would own dar's archive format, not just its 50-byte slice frame |

**dar's read order (MEASURED, E6).** dar opens source files in **readdir order,
depth-first pre-order**: identical to `find -type f` order, and different from sorted
order. It opens each entry twice: once `O_RDONLY|O_NOATIME` for FSA, then once to read.
`validate.rs` today walks in path-sorted database order, so the hasher must walk with
`std::fs::read_dir` order (walkdir unsorted). It must apply the same masks dar does
(`exclude::dar_masks`, `walk_directory`), so the two walks select the same files.

**Recommended rule for (b), modelled on git's "racy timestamp" handling:**

1. The hasher opens each regular file, `fstat`s it (`ino`, `size`, `mtime_ns`,
   `ctime_ns`), hashes it, and `fstat`s it again. Any difference → DIRTY.
2. **Lead control.** The hasher stays at most `L` bytes (e.g. 1–2 GiB, well under free
   page cache) ahead of dar. Under F, dar's progress is the stream byte count tapectl
   already reads. N hasher workers (#366) may run ahead in walk order, with results
   collected in walk order.
3. dar runs with `--retry-on-change 0`. A file that changed during **dar's** read gives
   exit 11 → DIRTY.
4. After dar exits 0, a **stat-only sweep** re-stats every hashed file. `ctime_ns` and
   `size` must equal the hash-time values, or DIRTY. A write between the hash and dar's
   read bumps ctime, which no unprivileged writer can set back.
5. "Racy" files, those whose hash-time `ctime_ns` falls within one timestamp tick of the
   hash start, are re-hashed in the sweep. That closes the coarse-timestamp hole.
6. Optional cross-check: dar's catalogue (`dar -l -T xml -ay`, exact sizes, MEASURED;
   3.8 s for 49,797 entries) lists exactly the file set the hasher hashed, with
   matching sizes and `dirty="no"`.

**What the guarantee becomes.** `files.sha256` equals the bytes dar archived, unless a
writer bypassed the VFS (raw block device, root changing the clock) or the page cache
was incoherent. That is stronger than today, where no check links the two reads at
all. Read-path bitrot is caught as before on re-stage: the hash differs from the
recorded baseline at an unchanged size. Within one run, dar and the hasher read the
same cached pages.

`sha256_plain` (per slice) becomes a tee on its own thread inside the encryptor, as in
§3.5. Whether to keep it at all is the #364 comment's separate question.

---

## 5. Q4 — what is on tape today (corrects #418's premise)

**CODE.**

- `build.rs:822-839`: `catalogs_for_tenant` filters the batch's units by tenant;
  `catalogs_for_all` takes every unit.
- `build.rs:917-1000`: `materialize_envelope_streaming` writes `MANIFEST.toml`,
  `RECOVERY.md`, `[PLAN.toml]`, `catalogs/<file>` for each isolated catalogue, and
  `[catalog.db]` into a tar **inside the age stream**.
- Tenant envelopes go to tenant + operator + escrow. The operator envelope and its
  backup go to operator + escrow, and carry every catalogue plus `catalog.db`.
- volume-format-v2 §1 puts all envelopes in the MIDDLE zone (files 4…j+1), straight
  after the front index and before every slice. That is "at the front", by decision D2.

So for every volume **today**:

| Copy | Where | Encrypted to | Isolation |
|---|---|---|---|
| tenant T's units' catalogues | tenant envelope T (1 copy) | T + op + esc | holds: `catalogs_for_tenant` filters by `tenant_id` |
| all units' catalogues + `catalog.db` | operator envelope **and** operator backup envelope (2 copies) | op + esc | operator-only |
| each unit's internal catalogue | inside its last data slice | T + op + esc | inherent to dar |

**What is not there:**

1. Any reader. RESTORE.sh, RECOVERY.md and the system guide never mention
   `catalogs/` (`grep`, CODE). An heir does not know the catalogues exist.
2. A second copy of the tenant envelope. #412 item 6 already proposes one.
3. A **cross-volume** index: each tape indexes only itself.

**What a catalogue alone gives an heir (MEASURED, E12, plus the DOC).**

- `dar -l <catalogue>` lists a unit's files, with owners, permissions, times and sizes,
  **without reading any data slice**: a few MiB from the envelope instead of reading
  the last slice from tape.
- `dar -l <catalogue> -T slicing` gives **the slice(s) holding each file** (e.g.
  `2-3 … secret_…_6.bin`). That is #412 item 2's `--path`: read only those slices plus
  the last.
- `dar -x <archive> -A <catalogue>` rescues a damaged internal catalogue: "the
  catalogue will be read from the archive given with -A instead of using the internal
  catalogue". dar checks the data-name label, refusing a catalogue from another run
  (MEASURED).
- Limits, MEASURED: direct-access `dar -x`/`-l` with **the last slice missing** aborts
  even with `-A`, because dar asks for the last slice. With `--sequential-read`, dar
  extracted a slice-1 file from slices 1–3 alone and then stopped. A truncated tape
  needs sequential mode, which is #412 item 1's streaming restore.

**Sizes at production scale.** Calibrated on `/scratch/audit-db/srcunit`: 49,797
entries with production-shaped paths, the shape of the largest L6-0001 unit. MEASURED:

| Form | bytes/entry | at 181,444 entries |
|---|---|---|
| classic isolated catalogue (`dar -C`, uncompressed) | 110.7, +3 for multi-MiB files (E11) | **≈ 19.7 MiB** |
| on-the-fly catalogue (bzip2, dar's fixed choice) | 25.6, +15 for real CRCs (E11) | ≈ 7.1 MiB |
| `dar_manager` database, one archive added | 11.4 | ≈ 2.0 MiB |
| main `tapectl.db` `files` + autoindex + FTS (audit catalog `home-s181k`, dbstat) | 137 + 55 + 37 = 229 | ≈ 39.6 MiB **per version** |
| on-tape `catalog.db` `files` (INFERRED from the table without autoindex/FTS) | ≈ 140–190 | ≈ 25–33 MiB |

On an L6-0001-shaped tape (181k files, one version each), per-file metadata on tape is:

- the internal catalogues: ≈ 20 MiB;
- the tenant envelopes: ≈ 20 MiB;
- the operator envelope and its backup: 2 × (20 + ~30) ≈ 100 MiB.

That is **≈ 140 MiB of 2.5 TB (≈ 0.006 %)**. Size is not a reason to add or remove
anything on tape.

---

## 6. The duplicated per-file metadata, examined (the CTO's added question)

### 6.1 The copies (the coordinator's list, verified and corrected)

| # | Copy | Correction | Holds |
|---|---|---|---|
| 1 | `tapectl.db` `files` (+ `files_fts`, + `UNIQUE(snapshot_id,path)` autoindex), one row per entry per version (`001_initial.sql:137`, `005` adds `file_type`/`link_target`) | confirmed. `manifest_entries` (a 7th copy) **was dropped by migration 027** | path, size, mtime, sha256, is_directory, file_type, link_target |
| 2 | dar's internal catalogue, end of the last slice | confirmed | everything dar knows (below) |
| 3 | isolated catalogue | **on the home device**, `<home>/catalogs/<uuid8>/`, not staging. It is *produced from* the plaintext archive on staging. One per snapshot, reused by later stage sets (`mod.rs:637-670`) | same as #2, minus file data |
| 4 | #3 in each tenant envelope | confirmed (`catalogs_for_tenant`) | same |
| 5 | #3 in the operator envelope | confirmed, **and again in the operator backup envelope (5b)** | same |
| 6 | on-tape `catalog.db` `files` in the operator envelope (and its backup) | confirmed (`ontape_catalog.rs:91-99`). Note it lacks `file_type`/`link_target` (#381) | path, size, sha256, modified_at, is_directory |
| 7 | the Heir Kit's full `tapectl.db` (ADR-0009), and every `db backup` | not in the list | = #1 |

**What each side records that the other does not:**

- **Only tapectl:** the per-file **sha256**; the unit/version/tenant binding; that the
  row existed at `snapshot create`, before any dar run.
- **Only dar:** permissions, uid/gid and names, atime/ctime, per-file data CRC, EA and
  FSA presence plus CRCs (the values sit in the archive), hard-link groups, symlink
  targets and every special file type (both also in tapectl's `file_type`/`link_target`
  since 005), sparse and dirty flags, and **which slice holds each file and at what
  offset**.

### 6.2 What each copy alone provides

| Copy | Its readers (CODE) | What breaks without it | Deliberate or accidental |
|---|---|---|---|
| 1 `tapectl.db` files | `catalog ls/search/locate/stats` (`cli/catalog.rs:421,461,824`); `restore --file` precheck (`volume/restore.rs:391-406`); staging validation and the BITROT baseline (`validate.rs:83`); new-version detection (`content_match.rs:170-210`, collection fingerprints); `snapshot diff` (`operations.rs:3165`); `unit check-integrity` (`:70`); the source of copy 6 | every catalog query, dirty detection, re-stage validation | deliberate. **Its per-version growth is the problem (#380)** |
| 2 internal catalogue | dar itself on every `-x`/`-t`/`-l` | dar cannot restore without it (or #3 via `-A`) | inherent |
| 3 home catalogues | only the envelope build (`build.rs:867`) | envelopes would have no `catalogs/`; nothing else reads them | deliberate as a build input. **Unused for queries** |
| 4 tenant envelope | nobody yet (heir tooling would be first, #412) | the tenant's only file index that needs no data slice: file listing, per-file slice location, catalogue rescue | deliberate (v4 §8, v2 §2), **latent** |
| 5/5b operator envelopes | nobody yet | the same for the operator and escrow heir across all tenants | deliberate redundancy (the backup envelope exists for exactly this) |
| 6 on-tape `catalog.db` | `catalog rebuild --from-volume` (`rebuild.rs:963,1379`); RECOVERY.md's sqlite3 section | DR of the main catalog's `files` rows, and the **sha256 baseline**, the only copy of it on tape | deliberate (#83). **Within the operator envelope, its path/size/mtime duplicate copy 5; only sha256 and tapectl ids are unique** |
| 7 Heir Kit / backups | heirs, DR | — | deliberate |

The one accidental duplication: copies 5 and 6 share an envelope, so path, size and
mtime are stored twice there. It costs ~25 MiB per copy on tape, which does not matter.
It does matter for #381: every attribute added to `files` (special file types, xattrs)
would be a **third** encoding of something dar already records authoritatively.

### 6.3 Could `files` be derived from the dar catalogue?

**Not wholly.**

1. The rows must exist at `snapshot create`, before any dar run: staging validation,
   dirty detection and `snapshot diff` all compare a fresh walk with them. dar
   catalogues exist only after staging.
2. Full-text search across all units needs an index. Running
   `dar -l -T xml -ay` costs ~14 s per 181k-entry unit (3.8 s for 49,797, MEASURED),
   per query.
3. dar has no sha256.

**Thinly, yes.** The database keeps what only it can (identity, membership, size,
mtime, sha256, kind, link target), and dar's catalogue answers the detail:

- permissions, owners, xattrs, FSA, hard links, special-file detail;
- slice location, via `dar -l <home catalogue> -T xml -ay` on demand, e.g. a future
  `catalog ls --long` or `restore --file` choosing slices.

**The opposite (drop the isolated catalogue in favour of `catalog.db`).** That would
cost the tenant heir their only index: `catalog.db` is operator-only because it spans
tenants. It would also cost dar-native rescue (`-A`) and per-file slice location. It
saves ~20 MiB per version on the host and on tape. **Not recommended.**

**Dropping `catalog.db`'s `files` in favour of the operator envelope's catalogues.**
Rebuild would parse dar XML for path, size, mtime and kind, but sha256 exists nowhere
else on tape, so a slim `(path, sha256)` table would remain. That saves about 15 MiB
per copy at the price of a dar-XML parser in the DR path. **Not worth it.** Slim the
table to the interned shape instead (§6.4).

### 6.4 One coherent proposal for the files representation (#380 option A, extended)

The CTO prefers #380's option A and asked how to make the schema scale to large file
counts. Proposal:

```sql
-- distinct paths per unit; FTS indexes these, not every version's rows
CREATE TABLE paths (
  id      INTEGER PRIMARY KEY,
  unit_id INTEGER NOT NULL REFERENCES units(id),
  path    TEXT NOT NULL,
  UNIQUE(unit_id, path)
);
CREATE VIRTUAL TABLE paths_fts USING fts5(path, content='paths', content_rowid='id');

-- membership: one narrow row per (version, path); no rowid, no second index
CREATE TABLE file_versions (
  snapshot_id INTEGER NOT NULL REFERENCES snapshots(id),
  path_id     INTEGER NOT NULL REFERENCES paths(id),
  kind        INTEGER NOT NULL,      -- 0 dir, 1 regular, 2 symlink, 3 special (#381's CHECK)
  size_bytes  INTEGER NOT NULL,
  mtime_ns    INTEGER,
  sha256      BLOB,                   -- 32 bytes, not 64 hex chars
  link_target TEXT,                   -- symlinks only; NULL otherwise
  PRIMARY KEY (snapshot_id, path_id)
) WITHOUT ROWID;
```

- **Scale (INFERRED, sized from the measured 229 B/row today).** A membership row is
  roughly 8+4+1+8+8+33 bytes plus b-tree overhead, ≈ 70–80 B per version-entry
  against ≈ 229 B today, about **3× smaller**. FTS and path text grow only with
  **distinct** paths, so a re-versioned unit adds no FTS rows and no path text. At
  #380's 5.46M-row stress case that is ≈ 0.4 GiB instead of ≈ 1.2 GiB, and search
  scans distinct paths.
- If re-versioning ever dominates, option B (range rows: `first_version`,
  `last_version` per unchanged file) can be layered on the same `paths` table later.
  Recommend A now, as ruled.
- **#381 resolved by authority, not by columns.** `kind` and `link_target` stay,
  because dirty detection needs them. **Permissions, owners, xattrs, FSA, device
  numbers and hard-link groups do not enter the database**: the dar catalogue is their
  authority, and on tape, in every envelope. The CTO's "other special file types,
  xattrs" are therefore already preserved and already on tape, encrypted; tapectl needs
  only a reader for them (`dar -l` on the home catalogue).
- **The on-tape `catalog.db`** takes the same narrow shape (`paths` +
  `file_versions`, with `kind` and `link_target`) as a **third shape-probed
  generation** (`detect_generation`). This answers #381's on-tape half in the same
  change. That is a minor-version on-tape change (§7).
- **`<home>/catalogs`** becomes a queried store (detail on demand), not only a build
  input. Keep it for the life of the version.

---

## 7. #418 recommendation (after §6) and its cost in 1.1.0 vs later

**There is nothing to add for "an encrypted copy of the dar catalogues at the front":
it exists, per tenant and for the operator, as §5 shows.** What is worth doing:

| Item | Value | On-tape | In 1.1.0 (with #396/#405/#412) | Later |
|---|---|---|---|---|
| R1. RESTORE.sh `--list` and `--path` read `catalogs/` from the envelope; RECOVERY.md and the guide say the catalogues exist and how to use them (`dar -l`, `-T slicing`, `-A` rescue) | turns latent copies 4/5 into heir value | RESTORE.sh/RECOVERY.md bytes (golden re-pin) | **cheap**: #412 item 2 already plans `--path`; same re-pin | a second re-pin |
| R2. A backup copy of each tenant envelope | redundancy for the tenant's only index and keys-to-slices map (#412 item 6) | layout: one more envelope per tenant, front-index entries, RESTORE.sh's envelope search | moderate; best in the same re-pin, since RESTORE.sh changes anyway | a further minor version |
| R3. `catalog.db` third generation (narrow shape + `kind`/`link_target`, §6.4) | #381's on-tape half; smaller | operator-envelope content (shape-probed) | only if §6.4 is ruled now; otherwise it would delay 1.1.0 | 1.2.0 with the #380 migration (natural pairing) |
| R4. Cross-volume, per-tenant index: every tape's tenant envelope carries that tenant's catalogues (or a `dar_manager` db, ≈ 2 MiB per 181k entries per archive) for **all** its units on all volumes | any one tape locates everything | new envelope member, growing with the archive | **no**: needs its own design (which versions, growth, operator vs tenant) | 1.2.0+ |
| R5. Keep catalogues uncompressed (re-isolate the `-@` output) vs ship bzip2 | bzip2 cuts ~65 % but needs a dar built with bzip2 | catalogue member bytes | n/a: comes with the staging change | — |

Cost logic: 1.1.0 already moves RESTORE.sh's golden pin under the CTO's review (#396,
#405, #412). R1, and R2 if cheap, share that one re-pin. Doing them later costs a
second re-pin and a second minor version, and tapes written in between lack them. R3
and R4 change envelope content that needs a design ruling first, so put them in 1.2.0
rather than delay 1.1.0.

**The staging change itself (§3.5) does not change the slice format or RESTORE.sh.**
It does change one on-tape string, MANIFEST.toml's `dar_command`, to `dar -c - …
--retry-on-change 0 -@ …` (and `-N`, #412 item 9). The version rule ("the minor moves
when generated on-tape bytes change") can be read either way here (§9 D2).

---

## 8. Q5 — making "no plaintext on the staging device" provable

**A finding about method (MEASURED, E13 first run).** With a default (async) ext4 mount,
today's write-encrypt-delete shape left **0** marker hits on the raw image. The
plaintext slices were deleted before writeback, so delayed allocation never put them on
disk. Production does write them: a unit takes far longer than the 30 s dirty-expiry
(E14 used a `sync` mount and found 215 hits). **A scan without forced writeback proves
nothing.** The staging filesystem under test must be mounted `sync` (or the harness
must `sync` between dar and encryption), with `nodiscard`.

Three layers, strongest last:

1. **By construction** (§3.5): `StagingDir` only creates age writers, and dar's argv
   has `-` as its archive. Unit tests assert both: the argv contains `-c -`, and no
   argument is under the staging directory.
2. **Structural audit, ungated, runs in `cargo test`.** Stage a unit with a staging
   directory watched by inotify (`IN_CREATE`, `IN_CLOSE_WRITE`, `IN_MOVED_TO`). Assert:
   - every file ever created matches `*.dar.age.partial` / `*.dar.age` or the probe;
   - every closed `.age` starts with `age-encryption.org/v1\n`;
   - nothing else appeared, even briefly. inotify reports a create even for a file
     deleted at once.

   Also: a `volume write` against `MemStore` asserts the session directory holds no
   `.db` file (item #5). The test needs `nix`'s `inotify` feature, a feature addition
   (CTO approval, worktree-agent rule 5). It is independent of compression, which
   matters because an archive set with `compression = gzip` would hide a verbatim
   marker from layer 3.
3. **Raw-device marker scan, gated** (sudo/loop; a `scripts/` gate step alongside the
   mhvtl gate, or `TAPECTL_LOOP_STAGING=1`):
   - Make a 256–512 MiB image, `mkfs.ext4 -E nodiscard`, `mount -o loop,sync,nodiscard`,
     and set it as `[staging] directory`.
   - Fixture with a random per-run token in: file contents (small, multi-slice, sparse,
     hard-linked), file and directory names, the unit name, the tenant name, an xattr
     value and a symlink target. Use `compression = "none"` so a leak would be
     verbatim.
   - Run `stage create`, and on mhvtl `volume write`, `volume read-slices` and
     `staging clean`. Then `umount` and `grep -a -c` the **image file**.
   - **Positive control, every run:** before staging, the harness writes a canary file
     containing the token onto the image and deletes it. The scan must find the canary,
     which proves deleted blocks are visible in this filesystem configuration.
   - **One-time positive control against the old code:** the same harness with tapectl
     1.0.7 finds the token (E14 here: 215 hits).
   - Pass = canary found, product token count 0.

**Operational conditions** a test cannot cover (§9 D6): swap must be off or encrypted
on home2, core dumps of tapectl and dar disabled (`LimitCORE=0` on the service unit),
and `--to`/`--scratch` for restores must not point into the staging device (a cheap
refusal).

---

## 9. Decisions for the CTO

1. **Q2 mechanism.** Adopt option F: `dar -c -`, tapectl frames dar slices using a
   header template from dar itself, and encrypts in-process. Rejected: A (FIFOs,
   impossible) and G (raw chunks, a format change). Alternatives to rule on if F is
   declined: C (tmpfs, needs RAM ≥ 2 × slice_size and swap off) or D (FUSE, a new
   dependency).
2. **Version for the staging change.** Slices and RESTORE.sh are unchanged, but
   MANIFEST.toml's `dar_command` text changes. Choose one:
   (a) a **patch** release ("format unchanged"); or
   (b) bundle the argv change (`-c -`, `--retry-on-change 0`, `-@`, `-N`) into
   **1.1.0** with #412 item 9's `-N`, even if the pipeline lands later; or
   (c) a 1.2.0 minor.
3. **`--retry-on-change 0`.** A file changing during dar's read refuses the stage
   (DIRTY, exit 11) instead of dar silently re-saving it. This is required by every
   streaming design, **including #367's `-E` plan**, which should be amended.
4. **Q3: drop the pre-dar pass.** Hash concurrently in dar's readdir order with a
   bounded lead, plus the ns-ctime stat rule and the post-dar stat sweep (§4.2), and
   accept the losses in §4.1. Choose the lead bound `L` and whether `nonzero_bytes`'s
   pre-dar refusal becomes a Tier-2 ask on the apparent-size upper bound.
5. **The home device.** The requirement as stated covers the staging device. The tapectl
   home also holds filename-grade plaintext: `tapectl.db`, `catalogs/`, stage reports,
   session logs. Decide whether that device stays encrypted, or whether the home-side
   catalogue and logs should also change.
6. **Operational rules on home2:** swap off or encrypted, core dumps off for the
   service, `--to`/`--scratch` refused inside the staging directory.
7. **Off-staging fixes to make the requirement true** (code-only): build
   `catalog_snapshot.db` in the home or in memory, and name `clone-*` directories by
   unit uuid.
8. **Q5 proof.** Approve the three layers in §8. Approve `nix`'s `inotify` feature for
   the ungated audit. Approve a sudo/loop gate step with the canary positive control.
9. **Catalogue compression.** Keep envelope catalogues uncompressed (re-isolate `-@`
   output with `-znone`, no on-tape change), or ship dar's bzip2 on-the-fly catalogues
   (≈ 65 % smaller; requires bzip2 in the heir's dar).
10. **#418 scope.** Record that the encrypted catalogues already ride every envelope at
    the front. Put R1 (heir tooling reads `catalogs/`) and, if cheap, R2 (tenant
    envelope backup) into 1.1.0's single re-pin. Defer R3 (`catalog.db` generation 3)
    and R4 (cross-volume tenant index) to 1.2.0.
11. **Files representation (#380 + #381).** Adopt §6.4: interned `paths` with FTS on
    distinct paths, a `WITHOUT ROWID` `file_versions` membership row with a BLOB
    sha256, and `kind` + `link_target` with #381's CHECK. dar's catalogue is the
    **authority** for permissions, owners, xattrs, FSA and hard links, which stay out of
    the database. Decide whether the on-tape `catalog.db` follows as generation 3 in the
    same release.
12. **Retention of `<home>/catalogs`.** Keep each catalogue for the life of its version,
    as the detail store behind `catalog ls --long`/`restore --file`, rather than
    treating it as disposable after the last copy is written.

---

## Appendix A — experiments (all under `/scratch/research-plainfree/`)

| Id | Script | What it established |
|---|---|---|
| E1/E1b | `e1_baseline.sh`, `e1b_trace.sh`, `slicefd.py` | dar's per-slice syscalls (§3.1) |
| E2 | `e2_fifo.sh` | FIFOs: EEXIST and blocked `O_RDONLY` open; `-w` unlinks them |
| E3 | `e3_stdout_hook.sh` | `-s` refused on stdout; stdout = single-slice archive; `-E` is synchronous, after close |
| E5 | `e5_catalogue.sh` | `-@` with stdout; listing equivalence; catalogues hold no content |
| E6 | `e6_order.sh` | dar reads in readdir (find) order, opens each file twice |
| E8 | `e8_change.sh` | the rewind under default retry; retry 0 → dirty, exit 11 |
| E9 | `e9_reslice.sh`, `reslice.py` | hand framing = dar_xform minus internal name; `dar -t`/`-x` OK |
| E10/E11 | `e10_catsize.sh`, `e11_catdelta.sh` | catalogue sizes per entry; re-isolation; dar_manager size |
| E12 | `e12_rescue.sh` | what an isolated catalogue does and does not rescue |
| E13 | `e13_markerscan.sh`, `stream_slicer.py` | raw-image scan, old vs F; restore round-trip; method finding on async mounts |
| E14 | `e14_tapectl_positive.sh` | tapectl 1.0.7 positive control: 215 hits on the staging image |

The core of option F, as run in E13 (a lab prototype in Python, not product code):

```bash
dar -c t -R empty_dir -s 4M -an -D -Q          # header template from dar itself
dar -c - -R src -an -D -Q --fsa-scope extX --retry-on-change 0 -@ home/catalogs/u \
  | python3 stream_slicer.py staging/u 4194304 t.1.dar "$AGE_RECIPIENT"
# restore check: age -d each slice → dar -t staging/u → dar -x → diff -r src x
```
