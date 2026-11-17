# T0.1: the readers of an ACL stored once

**Asked for** by the plan's task T0.1 (`docs/design/v3-implementation-plan.md`), #563's second criterion and item 273's rule.
Date: 2026-11-17. A measurement; nothing in the repository changed. Every fact is MEASURED. Scripts: `/scratch/t01/bin`
(`run_reader.sh`, `drive.sh`, `report.sh`, `keys.py`); the full per-run output is `/scratch/t01/report.txt`, each reader's rc and
stderr are under `/scratch/t01/data/res/`.

## 1. Verdict

**The decision rule is MET: Layout 3 keeps the TEXT record** (`SCHILY.acl.access` / `SCHILY.acl.default`). GNU tar 1.30 and bsdtar
3.6.2 both restore the access entries and the default entry from `text.tar` with their ordinary flags, as root and as an ordinary
user, exit 0, no warning. The positive control passed. The raw record alone is restored by NO reader with those flags.

**The writer's exact flags for the kept record** (with D2's list form, `./` names):
`tar -C SRC --format=pax --numeric-owner --no-recursion --null --verbatim-files-from -T LIST --pax-option=exthdr.name=%d/PaxHeaders/%f -c --xattrs --acls --xattrs-exclude='system.posix_acl_*' -f OUT`
(LIST is NUL-separated, `./`-prefixed). `--xattrs` stays so `user.*` is carried; the exclude drops the raw ACL record.

## 2. Method (repeatable)

| reader | where | version |
| --- | --- | --- |
| GNU tar 1.30 | `debian:10-slim` `sha256:bb3dc79fddbca7e8903248ab916bb775c96ec61014b3d02b4f06043b604726dc` (apt via archive.debian.org for `acl attr`) | `1.30+dfsg-6+deb10u1`; getfacl 2.2.53 |
| GNU tar 1.34 | `debian:12-slim` `sha256:734a72b3d6142a71993b4bc580ca01d8ca687f77a2c422d96a961d3376b179bc` | `1.34+dfsg-1.2+deb12u1` |
| GNU tar 1.35 | this host's `tar` | `tar (GNU tar) 1.35` (the writer too) |
| bsdtar | `debian:12-slim` + `libarchive-tools` | `bsdtar 3.6.2 - libarchive 3.6.2` (`3.6.2-1+deb12u5`) |
| BusyBox tar | this host's `busybox` (no image) | `/usr/bin/busybox` of Ubuntu |

1. Source `src/` (owner 1000): `f1` with `setfacl -m u:1:r,g:2:r`; `f2` plain; `f3` with `setfattr -n user.note -v x`; `dir`
   with `setfacl -d -m u:1:rx`. Ids 1 and 2 are `daemon` and `bin`; the pax records, not `getfacl`'s spelling, say what the archive
   holds, and every `getfacl` here is `-n`.
2. List (NUL-separated): `./ ./dir ./f1 ./f2 ./f3`. Writer base: the flags in section 1 with, per archive,
   `text.tar`: `--xattrs --acls --xattrs-exclude='system.posix_acl_*'`; `raw.tar`: `--xattrs`; `both.tar`: `--xattrs --acls` (control).
3. Readers' ordinary flags, each into an empty directory: GNU tars `tar -x --xattrs --acls`; bsdtar `bsdtar -x --xattrs --acls -p`;
   BusyBox `busybox tar -x`. Containers: `docker run -d --log-driver none -v /scratch/t01/data:/data IMAGE sleep`, then
   `docker exec [--user=1000:1000]`; nothing on standard output; host readers as root under `sudo`. Python `python3 -I`.
4. After each read: exit status, stderr, `getfacl -n -p` of `f1` and `dir`, `getfattr -d f3`.

## 3. Pax keys each archive carries (Python `tarfile.pax_headers`, SCHILY keys only)

| member | text.tar | raw.tar | both.tar |
| --- | --- | --- | --- |
| `./` | none | none | none |
| `./dir` | `SCHILY.acl.access` (`user::rwx group::rwx other::r-x`), `SCHILY.acl.default` (`user::rwx user:1:r-x group::rwx mask::rwx other::r-x`) | `SCHILY.xattr.system.posix_acl_default` | acl.access, acl.default, `SCHILY.xattr.system.posix_acl_default` |
| `./f1` | `SCHILY.acl.access` (`user::rw- user:1:r-- group::rw- group:2:r-- mask::rw- other::r--`) | `SCHILY.xattr.system.posix_acl_access` | acl.access and `SCHILY.xattr.system.posix_acl_access` |
| `./f2` | none | none | none |
| `./f3` | `SCHILY.xattr.user.note` = `x` | same | same |

`text.tar` carries the ACL by number (`user:1`, `group:2`), no `SCHILY.xattr.system.posix_acl_*` key, and still `user.note`.

## 4. The table

Every cell is identical for root and ordinary user (10 runs per archive were each 0 exit, empty stderr). Access = `f1` has
`user:1:r--` and `group:2:r--`; default = `dir` has `default:user:1:r-x`; note = `f3` `user.note="x"`.

| reader | text.tar | raw.tar | both.tar (control) | exit / warnings |
| --- | --- | --- | --- | --- |
| GNU tar 1.30 | access, default, note | none, none, note | access, default, note | 0 / none |
| GNU tar 1.34 | access, default, note | none, none, note | access, default, note | 0 / none |
| GNU tar 1.35 | access, default, note | none, none, note | access, default, note | 0 / none |
| bsdtar 3.6.2 | access, default, note | none, none, note | access, default, note | 0 / none |
| BusyBox tar | none, none, none | none, none, none | none, none, none | 0 / none |

BusyBox restores no ACL and no xattr from any archive (content only; it is the reader of last resort, as in T0.7). Under
`raw.tar` the ordinary flags restore only `user.note`: the raw record needs `--xattrs --xattrs-include='system.posix_acl_*'` on
GNU tar, and bsdtar did not restore it with `--xattrs --acls -p`.

## 5. Positive control

`both.tar` read by GNU tar 1.35 as root restored the access entries (`user:1:r--`, `group:2:r--`), the default entry
(`default:user:1:r-x`) and `user.note="x"`: passed, so the method is sound.

## 6. Also recorded (from T0.7, measured 2026-11-17)

GNU tar and BusyBox find a member by name only when it is spelled as the stream spells it, with the leading `./`. The archives
here were made with the writer's list form (`./` names), as D2 says.
