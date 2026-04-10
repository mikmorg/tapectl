# Milestone 0: External Dependency Validation

Standalone programs that validate every external tool and crate before writing tapectl code.

## Tests

### 1. age crate (`age-validate/`)
```bash
cd age-validate && cargo run --release
# For 1GB streaming test:
LARGE_FILE_MB=1024 cargo run --release
```
**Status: PASS** — keypair gen, multi-recipient, 1GB streaming all verified.

### 2. dar (`dar-validate.sh`)
```bash
# Requires: dar >= 2.6.x
DAR_BINARY=/opt/dar/bin/dar ./dar-validate.sh
```

### 3. Tape ioctl (`tape-validate/`)
```bash
# Requires: mhvtl or real tape drive at /dev/nst0
cd tape-validate && cargo run --release -- /dev/nst0
```

### 4. Full round-trip (`roundtrip/`)
```bash
# Requires: dar + tape device
cd roundtrip && cargo run --release -- --dar /opt/dar/bin/dar --device /dev/nst0
# For production-scale test:
cargo run --release -- --source-size 2048 --slice-size 1G
```

## Checklist

- [x] age: keypair generation
- [x] age: multi-recipient encrypt (2 recipients)
- [x] age: decrypt with each recipient independently
- [x] age: streaming encrypt/decrypt of 1 GB (73 MB/s encrypt, 78 MB/s decrypt)
- [x] age: CLI interop (crate encrypt, `age` binary decrypt)
- [x] dar 2.7.13: multi-slice archive (-s 10M) — 3 slices
- [x] dar: catalog isolation (-C)
- [x] dar: XML listing (-T xml) with per-file CRC
- [x] dar: symlink preservation (-D)
- [x] dar: xattr preservation (--fsa-scope extX)
- [x] dar: sha512 slice hashing (-3 sha512)
- [x] tape: mhvtl built from source, /dev/nst0 + /dev/sg1
- [x] tape: MTSETBLK, MTWEOFI, MTWEOF write/read/filemark
- [x] tape: MTIOCGET position tracking + MTFSF seek
- [x] tape: sg_logs error counters + sg_read_attr MAM data
- [x] round-trip: dar -> encrypt -> tape -> read -> decrypt -> extract
- [x] round-trip: 3 slices (~70 MB each, 200 MB total)
- [x] round-trip: both recipients decrypt independently
- [x] round-trip: diff -r zero differences
- [x] round-trip: sha256 disk == tape (all 3 slices MATCH)
