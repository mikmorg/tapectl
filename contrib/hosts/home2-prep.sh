#!/usr/bin/env bash
# contrib/hosts/home2-prep.sh — the one-time host preparation that must happen
# on home2 BEFORE `scripts/first-run.sh --profile contrib/hosts/home2.profile`.
#
# Everything here was decided with the operator on 2026-09-28 (the answers are
# in contrib/hosts/home2.profile). In order:
#
#   1  the drive belongs to home2: detach the LTO-6's SCSI hostdev from
#      vm-desk1 (live + persistent). The two hosts share no tape lock, so a
#      drive visible to both is a drive two writers can open.
#   2  the pre-redesign home of the 2026-09-13 attempt is deleted — only after
#      proving its catalog holds no volume (no tape was ever written with its
#      escrow identity) — with the Sep 12 binary and the old first-run log
#   3  the service user's home moves off /var (95% full) to
#      /srv/archive_meta/tapectl
#   4  /srv/acache is given back to root:root (the 2026-09-13 run chowned the
#      whole volume to tapectl); staging becomes /srv/acache/tapectl-staging
#   5  the catalog-backup directory /srv/local_backup/tapectl
#   6  packages: mtx (the lifecycle suite checks for it) and dar's build deps
#   7  dar 2.7.21 built from source into /usr/local, libdar linked statically
#      (so buster's dar 2.6.2 and its libdar are left exactly as they are)
#   8  a sudoers rule letting YOU run commands AS the service user without a
#      password (and nothing else): first-run.sh drives every tapectl command
#      through `sudo -u tapectl`, and the verify that follows a days-long write
#      would otherwise sit at a password prompt nobody is there to answer
#
# DRY RUN BY DEFAULT: it prints every command it would run (`would:`) and
# changes nothing. `--apply` does it, asking before each part; the deletion
# additionally needs the word typed. Idempotent: a part already done says so.
# Run it as mikmorg, in a terminal (sudo asks for the password).
set -euo pipefail

APPLY=0
while [ $# -gt 0 ]; do
  case "$1" in
    --apply) APPLY=1; shift ;;
    -h|--help) sed -n '2,/^set -euo/p' "$0" | sed '$d; s/^# \{0,1\}//'; exit 0 ;;
    *) echo "unknown option: $1" >&2; exit 2 ;;
  esac
done

SVC=tapectl
OLD_HOME_DIR=/var/lib/tapectl
NEW_HOME_DIR=/srv/archive_meta/tapectl
ACACHE=/srv/acache
STAGING=/srv/acache/tapectl-staging
BACKUP=/srv/local_backup/tapectl
VM=vm-desk1
HOSTDEV_XML="$(cd "$(dirname "$0")" && pwd)/home2-lto6-hostdev.xml"
DAR_VER=2.7.21
# sha256 of dar-2.7.21.tar.gz as downloaded from SourceForge over HTTPS on
# 2026-09-28 (trust on first use; the upstream .sig was not published there).
DAR_SHA256=b13a6645bb4f4bf3f36185a1ff3bc87a4eb27be5ce6487c953d84f075b4727b9
DAR_URL="https://sourceforge.net/projects/dar/files/dar/$DAR_VER/dar-$DAR_VER.tar.gz/download"
WORK="${XDG_CACHE_HOME:-$HOME/.cache}/tapectl"
STATE="${XDG_STATE_HOME:-$HOME/.local/state}/tapectl"

if [ -t 1 ]; then B=$'\e[1m'; D=$'\e[2m'; R=$'\e[0m'; Y=$'\e[33m'; G=$'\e[32m'; RD=$'\e[31m'; else B=""; D=""; R=""; Y=""; G=""; RD=""; fi
hdr()  { printf '\n%s== %s ==%s\n' "$B" "$*" "$R"; }
ok()   { printf '   %s✓ %s%s\n' "$G" "$*" "$R"; }
note() { printf '   %s%s%s\n' "$Y" "$*" "$R"; }
die()  { printf '   %s✗ %s%s\n' "$RD" "$*" "$R" >&2; exit 1; }
# run: in --apply, echo and execute; otherwise only say what would run.
run() {
  if [ "$APPLY" = 1 ]; then printf '   %s$ %s%s\n' "$B" "$*" "$R"; "$@"
  else printf '   %swould: %s%s\n' "$D" "$*" "$R"; fi
}
# part "question": in --apply, ask; in a dry run, always walk through it.
part() {
  [ "$APPLY" = 1 ] || return 0
  local ans; read -r -p "   $1 [y/N] " ans </dev/tty || true
  [[ "$ans" =~ ^[Yy] ]]
}

[ "$(hostname)" = home2 ] || die "this script prepares home2 only (this is $(hostname))"
[ "$(id -un)" != root ] || die "run it as your own user; it uses sudo where it must"
printf '%shome2 preparation%s — %s\n' "$B" "$R" "$([ "$APPLY" = 1 ] && echo "APPLYING (asks before each part)" || echo "dry run: nothing is changed; --apply to do it")"
id "$SVC" >/dev/null 2>&1 || die "no $SVC user — scripts/first-run.sh step 5 creates it (with this profile, at $NEW_HOME_DIR); run that first, then this"

# ------------------------------------------------------------------ 1
hdr "1  The drive belongs to home2"
NST_DEV="$(readlink -f /sys/class/scsi_tape/nst0/device 2>/dev/null || true)"
case "$NST_DEV" in
  */4:0:4:0) ok "the LTO-6 is nst0 at 4:0:4:0 here — scsi_host4 target 4 unit 0, as $HOSTDEV_XML says" ;;
  *) die "nst0 is not at 4:0:4:0 (${NST_DEV:-absent}) — the SCSI numbering moved; fix $HOSTDEV_XML (lsscsi -g) before detaching" ;;
esac
if virsh -c qemu:///system dumpxml "$VM" 2>/dev/null | grep "adapter name='scsi_host4'" >/dev/null; then
  note "$VM still has the drive as a hostdev: two hosts can open it, and they share no tape lock"
  note "before detaching: nothing on $VM may be using it (no gate, lifecycle suite, fill or tapectl run)"
  if part "Detach the LTO-6 from $VM now (live and persistent)?"; then
    run virsh -c qemu:///system detach-device "$VM" "$HOSTDEV_XML" --live --config
    [ "$APPLY" = 1 ] && { virsh -c qemu:///system dumpxml "$VM" | grep "adapter name='scsi_host4'" >/dev/null && die "still attached — read virsh's output"; ok "detached"; }
  fi
else ok "$VM does not have the drive"; fi

# ------------------------------------------------------------------ 2
hdr "2  The 2026-09-13 home, the Sep 12 binary and the old log"
OLD_TH="$OLD_HOME_DIR/.tapectl"
# $OLD_HOME_DIR is 0755, so the 0700 home inside it is visible without sudo.
if [ -e "$OLD_TH" ]; then
  if [ "$APPLY" = 1 ]; then
    # The premise of deleting it: no tape carries this home's escrow identity.
    NVOL="$(sudo -u "$SVC" python3 -c 'import sqlite3,sys
c=sqlite3.connect("file:"+sys.argv[1]+"?mode=ro", uri=True)
print(c.execute("select count(*) from volumes").fetchone()[0])' "$OLD_TH/tapectl.db")" || die "could not read $OLD_TH/tapectl.db — not deleting what I cannot inspect"
    [ "$NVOL" = 0 ] || die "$OLD_TH's catalog records $NVOL volume(s): tapes carry its escrow identity. NOT deleting — stop and look"
    ok "its catalog records 0 volumes: no tape was written with its escrow identity"
    run sudo du -sh "$OLD_TH"
    ans=""; read -r -p "   Type 'delete' to remove $OLD_TH for good (its keys, catalog and config): " ans </dev/tty || true
    if [ "$ans" = delete ]; then run sudo rm -rf "$OLD_TH"; ok "deleted"; else note "kept — the service home cannot move until it is gone or moved by hand"; fi
  else
    run sudo -u "$SVC" python3 -c '<count the volumes in the old catalog; refuse unless 0>'
    run sudo rm -rf "$OLD_TH"
  fi
else ok "no tapectl home at $OLD_TH"; fi
# Only the pre-redesign binary: it predates build identity, so its --version
# has no "(<commit>, <date>)". The one first-run step 3 installs has it and
# must never be offered for removal on a re-run of this script.
if [ -e /usr/local/bin/tapectl ] && ! /usr/local/bin/tapectl --version 2>/dev/null | grep '(' >/dev/null; then
  note "/usr/local/bin/tapectl is $(/usr/local/bin/tapectl --version 2>/dev/null || echo '?'), built $(stat -c %y /usr/local/bin/tapectl | cut -d' ' -f1) — first-run step 3 installs the current one"
  if part "Remove it, so nothing pre-redesign can run by accident?"; then run sudo rm -f /usr/local/bin/tapectl; fi
fi
# Once only: after the first set-aside, first-run.log is the current run's log.
if [ -e "$STATE/first-run.log" ] && [ ! -e "$STATE/first-run.log.pre-2026-09-28" ]; then
  if part "Set the old first-run log aside (first-run.log.pre-2026-09-28)?"; then run mv "$STATE/first-run.log" "$STATE/first-run.log.pre-2026-09-28"; fi
fi

# ------------------------------------------------------------------ 3
hdr "3  The service user's home: $NEW_HOME_DIR"
CUR_HOME="$(getent passwd "$SVC" | cut -d: -f6)"
if [ "$CUR_HOME" = "$NEW_HOME_DIR" ]; then ok "$SVC's home is already $NEW_HOME_DIR"
else
  mountpoint -q /srv/archive_meta || die "/srv/archive_meta is not mounted"
  if pgrep -u "$SVC" >/dev/null; then die "$SVC has running processes — stop them first (pgrep -u $SVC -a)"; fi
  note "$SVC's home is $CUR_HOME, on $(df --output=target "$CUR_HOME" | tail -1) ($(df -h --output=avail "$CUR_HOME" | tail -1 | tr -d ' ') free)"
  [ -e "$NEW_HOME_DIR" ] && die "$NEW_HOME_DIR already exists — usermod -m will not move onto it; look at it, then remove or rename it"
  if part "Move it to $NEW_HOME_DIR (usermod -d -m)?"; then
    if [ "$APPLY" = 1 ] && sudo test -e "$CUR_HOME/.tapectl"; then die "$CUR_HOME/.tapectl still exists — part 2 first"; fi
    # /srv/archive_meta is root 0750: the service user needs traverse only.
    run sudo setfacl -m "u:$SVC:x" /srv/archive_meta
    run sudo usermod -d "$NEW_HOME_DIR" -m "$SVC"
    [ "$APPLY" = 1 ] && { [ "$(getent passwd "$SVC" | cut -d: -f6)" = "$NEW_HOME_DIR" ] && sudo -u "$SVC" test -w "$NEW_HOME_DIR" || die "the move did not take — read usermod's output"; ok "home is $NEW_HOME_DIR"; }
  fi
fi

# ------------------------------------------------------------------ 4
hdr "4  Staging: $STAGING, and /srv/acache back to root"
if [ "$(stat -c %U "$ACACHE")" != root ]; then
  note "$ACACHE is owned by $(stat -c %U:%G "$ACACHE") (mode $(stat -c %a "$ACACHE")) — the 2026-09-13 run chowned it; it was root:root"
  if part "chown root:root $ACACHE (mode stays $(stat -c %a "$ACACHE"))?"; then run sudo chown root:root "$ACACHE"; fi
else ok "$ACACHE is owned by root"; fi
if part "Create $STAGING (tapectl, 0700) and give $SVC traverse on $ACACHE?"; then
  run sudo setfacl -m "u:$SVC:x" "$ACACHE"
  run sudo install -d -o "$SVC" -g "$SVC" -m 0700 "$STAGING"
fi

# ------------------------------------------------------------------ 5
hdr "5  The catalog-backup directory: $BACKUP"
if part "Create $BACKUP (tapectl, 0700)?"; then run sudo install -d -o "$SVC" -g "$SVC" -m 0700 "$BACKUP"; fi

# ------------------------------------------------------------------ 6
hdr "6  Packages"
PKGS=(mtx g++ make pkg-config zlib1g-dev libbz2-dev liblzo2-dev liblzma-dev libzstd-dev liblz4-dev librsync-dev libgcrypt20-dev libgpg-error-dev libargon2-dev)
MISSING=(); for p in "${PKGS[@]}"; do dpkg-query -W -f='${Status}' "$p" 2>/dev/null | grep "ok installed" >/dev/null || MISSING+=("$p"); done
if [ "${#MISSING[@]}" = 0 ]; then ok "all present"
elif part "apt-get install ${MISSING[*]}?"; then run sudo apt-get install -y "${MISSING[@]}"; fi

# ------------------------------------------------------------------ 7
hdr "7  dar $DAR_VER in /usr/local (libdar static)"
if [ -x /usr/local/bin/dar ] && /usr/local/bin/dar --version </dev/null 2>&1 | grep "dar version $DAR_VER" >/dev/null; then ok "/usr/local/bin/dar is $DAR_VER"
elif part "Download, verify, build and install dar $DAR_VER (a few minutes)?"; then
  run mkdir -p "$WORK"
  run curl -fsSL -o "$WORK/dar-$DAR_VER.tar.gz" "$DAR_URL"
  if [ "$APPLY" = 1 ]; then
    echo "$DAR_SHA256  $WORK/dar-$DAR_VER.tar.gz" | sha256sum -c - || die "checksum mismatch — not building"
  fi
  run rm -rf "$WORK/dar-$DAR_VER"
  run tar -xzf "$WORK/dar-$DAR_VER.tar.gz" -C "$WORK"
  # --disable-shared: dar carries its own libdar, so /usr/bin/dar (2.6.2) and
  # its libdar are untouched and ld.so never mixes the two.
  run bash -c "cd '$WORK/dar-$DAR_VER' && ./configure --prefix=/usr/local --disable-shared --enable-static \
    --disable-python-binding --disable-dar-static --disable-upx --disable-libcurl-linking --disable-gpgme-linking"
  run make -C "$WORK/dar-$DAR_VER" -j"$(nproc)"
  run sudo make -C "$WORK/dar-$DAR_VER" install-strip
  if [ "$APPLY" = 1 ]; then
    /usr/local/bin/dar --version </dev/null 2>&1 | grep "dar version $DAR_VER" >/dev/null || die "/usr/local/bin/dar is not $DAR_VER after install"
    ldd /usr/local/bin/dar | grep libdar >/dev/null && die "/usr/local/bin/dar links a shared libdar — the static build did not take"
    # every compression tapectl can ask dar for must be compiled in
    NO="$(/usr/local/bin/dar -V </dev/null 2>&1 | grep -E 'compression \(.*: *NO' || true)"
    [ -z "$NO" ] || die "dar $DAR_VER lacks: $NO"
    ok "dar $DAR_VER installed: $(/usr/local/bin/dar -V </dev/null 2>&1 | grep -cE 'compression \(.*: *YES') compressions compiled in, libdar static"
  fi
fi

# ------------------------------------------------------------------ 8
hdr "8  You may act as $SVC without a password"
SUDOERS=/etc/sudoers.d/tapectl-operator
RULE="$(id -un) ALL=($SVC) NOPASSWD: ALL"
if sudo -n -u "$SVC" true 2>/dev/null; then ok "$(id -un) can already run commands as $SVC without a password"
elif part "Install $SUDOERS: '$RULE' (validated with visudo first)?"; then
  # Scoped to the service user only: root-level sudo (setfacl, install,
  # usermod, systemctl) still asks, and those happen with you at the keyboard.
  TMP_RULE="$(mktemp)"; printf '%s\n' "$RULE" > "$TMP_RULE"
  run sudo visudo -cf "$TMP_RULE"
  run sudo install -m 0440 -o root -g root "$TMP_RULE" "$SUDOERS"
  rm -f "$TMP_RULE"
  [ "$APPLY" = 1 ] && { sudo -k; sudo -n -u "$SVC" true 2>/dev/null && ok "no password needed as $SVC" || die "the rule did not take — sudo -l shows what is in force"; }
fi

hdr "Next"
cat <<EOF
   cd ~/devel/git/tapectl && git pull
   tmux new -s tapectl        # staging, writing and verifying run for days
   scripts/first-run.sh --profile contrib/hosts/home2.profile --to 6
   ... read what steps 1-6 found, then, with paper ready for the escrow secret:
   scripts/first-run.sh --profile contrib/hosts/home2.profile --from 7 --to 12
   ... after writing the secret down: tmux clear-history (it is in the scrollback)
   ... after the rehearsal: unload EW7VWMVKF6 (mt -f <dev> offline), load a production cartridge
   scripts/first-run.sh --profile contrib/hosts/home2.profile --from 13
EOF
