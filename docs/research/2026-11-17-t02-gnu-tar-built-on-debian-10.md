# T0.2 GNU tar 1.35 built from the upstream release on Debian 10 (2026-11-17)

**Verdict: the rule holds. Item 275's way stands.** The tar built on Debian 10 with ACL support writes the ACL entries by number (`user:1:r--`, `group:2:r--`), and the system's 1.30 writes names (`user:daemon:r--`, `group:bin:r--`).

## Recipe
- Source: newest release on ftp.gnu.org/gnu/tar is 1.35. `tar-1.35.tar.xz`, sha256 `4d62ff37342ec7aed748535323930c7cf94acf71c3591882b26a7ea50f3edc16` (equals the plan's).
- Signature verified: yes. `tar-1.35.tar.xz.sig` against `https://ftp.gnu.org/gnu/gnu-keyring.gpg`: good signature, Sergey Poznyakoff, primary fingerprint 325F 650C 4C2B 6AD5 8807 327A 3602 B07F 55D0 C732 (key not web-of-trust certified, as expected).
- Container `debian:10-slim`; apt sources `deb http://archive.debian.org/debian buster main` and `deb http://archive.debian.org/debian-security buster/updates main`, with `Acquire::Check-Valid-Until "false";`.
- Packages (`apt-get install -y --no-install-recommends build-essential libacl1-dev libattr1-dev xz-utils acl`; `acl` is only for the test's `setfacl`, not for the build): build-essential 12.6, gcc 4:8.3.0-1, make 4.2.1-1.2, libacl1-dev 2.2.53-4 (libacl1 2.2.53-4), libattr1-dev 1:2.4.48-4 (libattr1 1:2.4.48-4), xz-utils 5.2.4-1+deb10u1, acl 2.2.53-4. System tar 1.30+dfsg-6+deb10u1.
- Build, as root, from the unpacked source:
  ```
  export FORCE_UNSAFE_CONFIGURE=1
  ./configure --prefix=/usr/local --with-posix-acls --with-xattrs --with-selinux=no
  make -j4
  make install
  ```
  config.h shows `HAVE_POSIX_ACLS` and `USE_ACL 1`. (Debian's gcc 8.3 builds it without patches. The two `--with-` switches are the configure names for ACL and xattr; both are also auto-detected here. T4.14 should keep them, so a missing libacl-dev fails configure rather than building a tar with no ACL support.)
  Note: the plan's build line should check for the configure result, not trust the version number (#563).

## Proof (step 4)
- `/usr/local/bin/tar --version`: `tar (GNU tar) 1.35`. `/bin/tar --version`: `tar (GNU tar) 1.30`, unchanged.
- `ldd /usr/local/bin/tar`: `libacl.so.1`, `libattr.so.1`, libc.
- File on an ext4 bind mount, `setfacl -m u:1:r,g:2:r f` (getfacl shows `user:daemon:r--`, `group:bin:r--`).
- `/usr/local/bin/tar --format=pax --numeric-owner --xattrs --acls --xattrs-exclude='system.posix_acl_*' -cf new.tar f` holds:
  ```
  SCHILY.acl.access=user::rw-
  user:1:r--
  group::r--
  group:2:r--
  mask::r--
  other::r--
  ```
  No name; no raw `system.posix_acl_*` record in the archive (excluded, as T0.1 requires).

## Negative control (step 5)
- `/bin/tar` (1.30) with `--format=pax --numeric-owner --xattrs --acls`, and again without `--numeric-owner`: both hold `user:daemon:r--` and `group:bin:r--` in `SCHILY.acl.access`, plus the raw `SCHILY.xattr.system.posix_acl_access` record. So the test sees names where names exist, and `--numeric-owner` does nothing for the ACL text on 1.30 (matches #563).

## make check (step 6)
Run: `make -j4 check TESTSUITEFLAGS=-j4`, exit 0: 210 tests successful, 34 skipped (as root in a container: lzip, lzop, zstd compressors not installed, unreadable-directory and read-only-dir tests, etc.). Logs: `/scratch/t02/keep/check.log`, `/scratch/t02/keep/inner.log`.

## Rule's verdict
Steps 4 and 5 both hold: the 1.35 tar built on Debian 10 writes ACL entries by number and the system tar writes names, so item 275's way (build into /usr/local, leave the system tar) stands.
