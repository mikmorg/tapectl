# RESTORE.sh as tapectl 1.5.0 wrote it

`RESTORE-GOLD01-12.sh` is the RESTORE.sh (File 2) that tapectl v1.5.0 wrote
for a volume labelled `GOLD01` with 12 files: byte for byte the script whose
sha256 `tests/on_tape_golden.rs` pinned through 1.3.0, 1.4.0 and 1.5.0
(`95862b49b0621e5970ebd91c49a353231725009e1f527c691cf98b3a116a6315`). It was
assembled from `git show v1.5.0:src/volume/layout.rs` and
`git show v1.5.0:src/volume/restore_script.rs` the way
`generate_restore_script_v2` assembles it, and checked against that hash.

`tests/heir_restore_sh.rs` runs it against tapes this tapectl writes, to prove
ADR-0012 item 111: every tape written since 1.6.0 states
`requires = ["short-seal"]` in File 0, full or short, and a 1.5.0 RESTORE.sh
refuses it there, by name, before it reads anything else of it
(`volume-format-v2.md` §1.2, §6 "Since 1.6.0"). The test checks the hash first,
so the fixture cannot drift. Never edit the script: it is what an heir holding
a tape written by 1.3.0 to 1.5.0 actually has at its File 2.
