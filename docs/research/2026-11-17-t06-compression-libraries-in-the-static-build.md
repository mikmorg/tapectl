# T0.6 The compression libraries in the static build (2026-11-17)

Measured outside the repository in `/scratch/t06/crate` (sources, Cargo.toml, Cargo.lock kept; `target` deleted).
Toolchain `cargo +1.94.1` (the repo's pin; the host's default `stable` lacks the musl target std, only 1.94.1 has it),
target `x86_64-unknown-linux-musl`, `/usr/bin/musl-gcc`. Debug profile only. Nothing installed on the host.

## Table

| Algorithm | Crate and version | Licence | Builds C source | Static (ldd) | Tool reads lib frame | Lib reads tool frame | Size added (debug) |
|---|---|---|---|---|---|---|---|
| gzip | flate2 1.1.11 (miniz_oxide 0.9.1 backend, default) | MIT OR Apache-2.0 (miniz_oxide MIT OR Zlib OR Apache-2.0) | no, pure Rust | yes | yes, levels 6 and 1 | yes, levels 6 and 1 | +793 KB (4,207,856 -> 5,001,112) |
| xz | liblzma 0.4.9 (liblzma-sys 0.4.9) | MIT OR Apache-2.0 (liblzma-sys bundles xz C source) | yes, built by musl-gcc | yes | yes, levels 6 and 0 | yes, levels 6 and 0 | +526 KB (-> 4,733,952) |
| zstd | zstd 0.14.4 (zstd-sys 2.1.3+zstd.1.5.7) | BSD-3-Clause (zstd-sys BSD-3-Clause AND BSD-2-Clause) | yes, built by musl-gcc | yes | yes, levels 3 and 19 | yes, levels 3 and 19 | +3.61 MB (-> 7,818,264); C is compiled at opt-level 0 in a debug build, so this overstates a release build |

All three together: 9,088,472 bytes, `static-pie linked`, `ldd`: "statically linked". Baseline (no library): 4,207,856.
Last releases (crates.io `updated_at`): flate2 2026-11-06, liblzma 2026-10-19 (liblzma-sys 2026-09-14), zstd 2026-11-12 (zstd-sys same day).
`cargo audit` (advisory db 1509 advisories, 21 crates): no advisory, exit 0.
Failure recorded: the first build with the default `cargo` failed (`can't find crate for core`): the host's default toolchain `stable` has no musl target; `cargo +1.94.1` (the pinned toolchain) builds. Not a library failure; release.yml installs the target via rust-toolchain action.

## Frame checks (D15)
Input: 20.8 MB (project sources and docs, 3 MB random, a binary). Program writes each frame the D15 way:
gzip one member, header `1f8b 08 00 00000000 00 ff` (no file name, mtime 0; OS byte 0xff = unknown, the gzip tool writes 03; every reader ignores it);
xz one stream, `xz -lv` says Check CRC64, one stream; zstd one frame (`zstd -lv`: Frames 1, Check XXH64, Decompressed Size stated). Single thread each.
Round trip, `cmp` identical in all 12: gzip 1.12, xz 5.4.5, zstd 1.5.5 CLIs on this host.
Not done: a stated xz/zstd cross-check against the oldest heir tools (T0.5's job).

## Method (repeatable)
1. `cargo new crate`; features `gz`,`xz`,`zst` pulling `flate2 = "1"`, `liblzma = "0.4"`, `zstd = "0.14"`.
2. Program: `t06 c ALGO LEVEL in out` / `t06 d ALGO in out`; gzip `GzBuilder::new().mtime(0)` (no filename);
   xz `write::XzEncoder::new(w, level)` (CRC64 default); zstd `Encoder::include_checksum(true)`, `set_pledged_src_size`, `include_contentsize(true)`.
3. `CARGO_PROFILE_DEV_DEBUG=line-tables-only CARGO_TARGET_DIR=/scratch/t06/target flock /scratch/tapectl-build.lock cargo +1.94.1 build --target x86_64-unknown-linux-musl --features gz,xz,zst`; sizes from builds with `--features ""`, `gz`, `xz`, `zst` each.
4. `ldd`, `file`; for each algo and two levels compress with the program, decompress with the tool, `cmp`; compress with the tool, decompress with the program, `cmp`.
5. `cargo audit` in the crate.

## Verdicts (decision rule applied)
- gzip: ACCEPTED (flate2 1.1.11, pure Rust; static, both directions at levels 6 and 1, audit clean).
- xz: ACCEPTED (liblzma 0.4.9, bundled C by musl-gcc; static, both directions at levels 6 and 0, audit clean).
- zstd: ACCEPTED (zstd 0.14.4, bundled C by musl-gcc; static, both directions at levels 3 and 19, audit clean).
No fallback to the host's tools is needed.
