//! Named `awk` fragments assembled into RESTORE.sh (File 2 on every tape) by
//! `layout::generate_restore_script_v2`. Split out so the heir-path rules
//! these programs encode have an interface tests can reach BY NAME, instead
//! of finding an invocation's shell line and slicing the generated script's
//! text between its first two single quotes (architecture review
//! 2026-09-11, candidate C6). Six RESTORE.sh defect commits in the last
//! sixty were in fragments reachable from Rust only that way.
//!
//! **The generated script's bytes are unchanged.** Every const here holds
//! EXACTLY the program text that sat between the invocation's single quotes
//! before this split, byte for byte, including leading/trailing whitespace.
//! `tests/on_tape_golden.rs` pins `generate_restore_script_v2`'s SHA-256 and
//! must pass unchanged; `layout.rs`'s
//! `every_named_awk_fragment_is_present_verbatim_in_the_assembled_script`
//! checks each const is a verbatim substring of the assembled script.
//!
//! **2026-09-22 — the script's bytes HAVE since changed, once, by CTO
//! ruling.** The paragraph above is a claim about the C6 split itself, and it
//! remains true of that commit: the split moved no bytes. It is not a standing
//! promise that `RESTORE_SH_SHA256` never moves. Issues #288 (repeated
//! `--key`), #291 (every envelope failure carries its context) and #218 (the
//! carried `MB` -> `MiB`) landed together under one authorised re-pin. No awk
//! fragment below was touched by it, so the verbatim-substring check still
//! holds for every const here — which is the property this module exists to
//! keep, and the reason the re-pin did not have to reach into this file.
//!
//! **No fragment may contain an apostrophe.** Every program below is
//! single-quoted in the shell it is assembled into, so a `'` anywhere
//! inside it — including inside an awk comment — would end the quoting
//! early and corrupt RESTORE.sh on every tape it is written to (the #135
//! near-miss: an awk comment almost carried an em-dash contraction).
//! `layout.rs`'s `no_named_fragment_contains_an_apostrophe` enforces this
//! for every const below, and `no_awk_placeholder_survives_assembly`
//! confirms every `__AWK_*__` placeholder used in the template gets
//! substituted.
//!
//! Trivial single-purpose `awk` one-liners (a plain `{ print $1 }` field
//! extraction from `sha256sum` output, or a same-shape lookup-by-key) stay
//! inline in the template in `layout.rs` — see that function's doc comment
//! for which, and why naming them would not add anything a reader could not
//! already see in the one-liner itself.

/// Parses `[[files]]` blocks (the front index, File 3, or the seal marker's
/// embedded copy) into `position|type|size_bytes|sha256_encrypted` lines.
/// Both rung 1 (`try_front_index`) and rung 2 (`try_seal_copy`) of the
/// degradation ladder (`docs/design/volume-format-v2.md` sec 2.5 / sec 4)
/// feed their raw TOML through this one parser, so the two rungs cannot
/// silently diverge on what a `[[files]]` entry means. Either the `type` or
/// the `size_bytes`/`sha256_encrypted` fields may be empty — the front
/// index's own entry and the seal marker's own entry are self-referential
/// and carry neither size nor hash.
pub(crate) const AWK_PARSE_FILE_LIST: &str = r##"
    /^\[\[files\]\]/ {
      if (p != "") print p "|" t "|" s "|" h
      p = ""; t = ""; s = ""; h = ""
    }
    /^position = /         { p = $3 }
    /^type = /             { t = $3; gsub(/"/, "", t) }
    /^size_bytes = /       { s = $3 }
    /^sha256_encrypted = / { h = $3; gsub(/"/, "", h) }
    END { if (p != "") print p "|" t "|" s "|" h }
  "##;

/// Front-index self-consistency check
/// (`docs/design/volume-format-v2.md` sec 2.5): positions strictly
/// increasing from 0 (a total index has no gaps), exactly one
/// `front_index` entry and it is at position 3, exactly one `seal_marker`
/// entry and it is last, and — when an expected total is supplied — the
/// entry count matches it. A subtly wrong map fails loudly (nonzero exit)
/// rather than silently misleading recovery: this is the gate
/// `establish_files` relies on before it trusts either rung's output well
/// enough to use it to locate envelopes and slices. No `exit` appears in
/// the main per-record block (awk still runs `END` after a mid-block
/// `exit`) — flags are accumulated and only checked in `END`, where `exit`
/// really does terminate.
pub(crate) const AWK_CHECK_FILE_LIST: &str = r##"
    {
      row = NR - 1
      if ($1 !~ /^[0-9]+$/)   { badpos = 1 }
      else if ($1 + 0 != row) { badcontig = 1 }
      if ($2 == "front_index") {
        fi_count++
        if ($1 + 0 != 3) { badfipos = 1 }
      }
      if ($2 == "seal_marker") { seal_count++; seal_row = NR }
      last_row = NR
    }
    END {
      if (NR == 0)              { print "empty file list" > "/dev/stderr"; exit 1 }
      if (badpos)               { print "a position value is not a non-negative integer" > "/dev/stderr"; exit 1 }
      if (badcontig)            { print "positions are not contiguous from 0" > "/dev/stderr"; exit 1 }
      if (fi_count + 0 != 1)    { print "expected exactly one front_index entry, found " fi_count+0 > "/dev/stderr"; exit 1 }
      if (badfipos)             { print "front_index entry not at position 3" > "/dev/stderr"; exit 1 }
      if (seal_count + 0 != 1)  { print "expected exactly one seal_marker entry, found " seal_count+0 > "/dev/stderr"; exit 1 }
      if (seal_row != last_row) { print "seal_marker entry is not the last file" > "/dev/stderr"; exit 1 }
      if (expected != "" && NR != expected + 0) {
        print "entry count " NR " does not match expected total " expected > "/dev/stderr"; exit 1
      }
    }
  "##;

/// Lists every envelope position, primaries and backups, in on-tape order
/// (the file list is already position-sorted, and the format's fixed zone
/// order puts the tenant envelope(s), then the operator envelope and its
/// backup, then each tenant envelope's backup: every primary before every
/// backup, `volume-format-v2.md` sec 1.3). The walk over these skips a
/// backup whose primary was already tried, by its equal hash, and reads it
/// when the primary was damaged or would not read. Stateless — no accumulator, no `END` block — so by the letter
/// of the classification this is plain field extraction; it is named
/// anyway because it is the one shared discovery step both
/// `--find-envelope` and `--restore` walk to locate the envelope that
/// opens under the supplied key.
pub(crate) const AWK_FIND_ENVELOPE: &str = r##"$2=="tenant_envelope" || $2=="operator_envelope" || $2=="operator_envelope_backup" || $2=="tenant_envelope_backup" { print $1 }"##;

/// Does an envelope's decrypted `MANIFEST.toml` list a given unit under
/// `[[units]]`? Issue #127: the escrow recipient (ADR-0005) is on every
/// envelope, so a universal key decrypts all of them, and the first
/// envelope on tape may belong to a different tenant than the one named by
/// `--unit`. `do_restore` uses this to keep trying envelopes until it finds
/// the one whose manifest actually lists the requested unit, instead of
/// trusting the first envelope the key happens to open.
pub(crate) const AWK_MANIFEST_HAS_UNIT: &str = r##"
    /^\[\[units\]\]/ { in_u = 1; next }
    in_u && /^name = / { gsub(/"/, "", $3); if ($3 == u) found = 1; in_u = 0 }
    /^\[/             { in_u = 0 }
    END { exit(found ? 0 : 1) }
  "##;

/// Collects the unit names listed under `[[units]]` in a manifest, in
/// first-seen order, deduplicated via `!seen[]`. Issue #133: a volume can
/// carry the SAME unit under several snapshot versions — one `[[units]]`
/// block per version — and listing it once per block made a single unit
/// look like several, pushing the plain `--restore` (no `--unit`) form
/// into the "multiple units" branch and telling the heir to disambiguate a
/// name from itself.
pub(crate) const AWK_UNIT_LIST: &str = r##"
    /^\[\[units\]\]/ { in_u = 1; next }
    # !seen[] — a volume can carry the SAME unit in several snapshot versions,
    # one [[units]] block each. Listing it once per block made a single unit
    # look like several and pushed the plain --restore (no --unit) form into
    # the "multiple units" branch, printing the same name twice (#133).
    in_u && /^name = / { gsub(/"/, "", $3); if (!seen[$3]++) print $3; in_u = 0 }
    /^\[/              { in_u = 0 }
  "##;

/// Selects exactly one snapshot version's slices for a target unit from
/// `MANIFEST.toml`. Issue #131: a volume can carry the SAME unit twice —
/// two snapshot versions, two stage sets — and collecting every matching
/// `[[units]]` block concatenated both versions' slices and handed the mix
/// to `dar`; when the versions had different recipients (a `tenant
/// reassign` between them) the first slice was one the supplied key could
/// not open, and the failure read as "wrong key" on the heir path, where
/// there is no database to fall back on. Issue #135 hardened the
/// selector's `in_head` guard so a stray `name` key outside the
/// `[[units]]` head — for example a `name` field added to a
/// `[[units.slices]]` row — cannot silently retarget which unit is being
/// selected; `in_head` tracks POSITION (are we in the key/value region
/// directly under `[[units]]`?) separately from `in_u`, which tracks
/// IDENTITY (is this block the unit we want?).
pub(crate) const AWK_SELECT_VERSION: &str = r##"
    function flush() {
      if (in_s && num != "") {
        n[blk]++
        slice[blk, n[blk]] = num "|" tpos "|" eb "|" sha
      }
      in_s = 0; num = ""; tpos = ""; eb = ""; sha = ""
    }
    # in_head tracks POSITION (are we in the key/value region directly under
    # [[units]]?); in_u tracks IDENTITY (is this block the unit we want?).
    # Separate on purpose: name and snapshot_version belong to the block head,
    # so honouring them anywhere else — a name key added to [[units.slices]],
    # or to some future table — would silently retarget the selector and drop
    # the remaining slices. A wrong answer with no error is the worst failure
    # this script can produce (#135).
    #
    # No apostrophes in these comments: the whole program is single-quoted in
    # the shell, so one would end it mid-awk and break the generated script.
    /^\[\[units\]\]/ { flush(); blk++; hit[blk] = 0; ver[blk] = -1; in_u = 0; in_head = 1; next }
    /^\[/ { in_head = 0 }
    in_head && /^name = / {
      gsub(/"/, "", $3)
      if ($3 == unit) { in_u = 1; hit[blk] = 1 } else { in_u = 0 }
    }
    in_u && in_head && /^snapshot_version = / { ver[blk] = $3 + 0 }
    in_u && /^\[\[units\.slices\]\]/ { flush(); in_s = 1; next }
    in_s && /^number = /           { num = $3 }
    in_s && /^tape_position = /    { tpos = $3 }
    in_s && /^encrypted_bytes = /  { eb = $3 }
    in_s && /^sha256_encrypted = / { gsub(/"/, "", $3); sha = $3 }
    END {
      flush()
      best = -1; pick = 0
      for (b = 1; b <= blk; b++) {
        if (!hit[b]) continue
        if (want != "") { if (ver[b] == want + 0) { pick = b; best = ver[b] } }
        else if (ver[b] >= best) { best = ver[b]; pick = b }
      }
      if (pick) {
        print best > "/dev/stderr"
        for (i = 1; i <= n[pick]; i++) print slice[pick, i]
      }
    }
  "##;

/// Reads `mt status` into `FILE BLOCK`, or prints nothing (issue #396). The
/// script trusts its tape cursor only when the drive agrees with it, as
/// tapectl checks `MTIOCGET` since 1.0.5. mt-st prints
/// `File number=3, block number=0, partition=0.` on one line; GNU cpio mt
/// prints `file number = 3` and `block number = 0` on two. Matched
/// case-insensitively with either spacing; anything else yields no output,
/// and the script then rewinds before every read, as it always did.
pub(crate) const AWK_MT_POSITION: &str = r##"
    { l = tolower($0) }
    l ~ /file number *=/ {
      s = l; sub(/.*file number *= */, "", s); sub(/[^-0-9].*/, "", s); f = s
    }
    l ~ /block number *=/ {
      s = l; sub(/.*block number *= */, "", s); sub(/[^-0-9].*/, "", s); b = s
    }
    END { if (f ~ /^-?[0-9]+$/ && b ~ /^-?[0-9]+$/) print f, b }
  "##;

/// One `unit|versions|slices|bytes` line per unit in an envelope manifest,
/// in first-seen order (issue #412 item 7): `--find-envelope` prints it as a
/// table for every envelope it opens, which with an operator or escrow key
/// is every unit on the tape. A unit stored in several snapshot versions is
/// one line, its versions joined with commas; slices and bytes are summed
/// over them.
pub(crate) const AWK_UNITS_TABLE: &str = r##"
    /^\[\[units\]\]/ { in_head = 1; cur = ""; next }
    /^\[\[units\.slices\]\]/ { in_head = 0; if (cur != "") n[cur]++; next }
    /^\[/ { in_head = 0 }
    in_head && /^name = / {
      v = $3; gsub(/"/, "", v); cur = v
      if (!(cur in seen)) { seen[cur] = 1; order[++k] = cur }
    }
    in_head && /^snapshot_version = / && cur != "" {
      vers[cur] = (vers[cur] == "") ? $3 : vers[cur] "," $3
    }
    /^encrypted_bytes = / && cur != "" { b[cur] += $3 }
    END {
      for (i = 1; i <= k; i++) {
        u = order[i]
        printf "%s|%s|%d|%.0f\n", u, vers[u], n[u], b[u]
      }
    }
  "##;

/// The `uuid` of one version of a unit in an envelope manifest (issue #418):
/// the envelope's dar catalogue for it is `catalogs/<first 8 of uuid>_v<N>`,
/// which `--list` and `--path` read. Matches `name` and `snapshot_version`
/// only in the `[[units]]` head, as `AWK_SELECT_VERSION` does (#135).
pub(crate) const AWK_UNIT_UUID: &str = r##"
    function done_head() {
      if (hit && v == want + 0 && id != "") { print id; found = 1 }
      hit = 0; v = -1; id = ""
    }
    /^\[\[units\]\]/ { if (!found) done_head(); in_head = 1; next }
    /^\[/ { if (in_head && !found) done_head(); in_head = 0; next }
    in_head && /^name = / { x = $3; gsub(/"/, "", x); hit = (x == unit) }
    in_head && /^uuid = / { id = $3; gsub(/"/, "", id) }
    in_head && /^snapshot_version = / { v = $3 + 0 }
    END { if (in_head && !found) done_head() }
  "##;

/// The functions both index programs share (`--where`, `--find`;
/// `docs/design/volume-format-v2.md` sec 1.5): the sec 1.5 escapes decoded by
/// ONE left-to-right scan (never chained substitutions, which turn the escaped
/// text of a backslash followed by `t` into a TAB), a printable form of a
/// decoded value, a human size, and the header-name column lookup. Columns are
/// found by the header line's names and an unknown column is ignored, so the
/// index can gain columns without a `requires` entry.
///
/// A decoded value is shown with TAB, LF and CR written as `\t`, `\n` and `\r`
/// and any other control byte as `?`: a path may hold a real newline, which
/// would break one-row-per-line output and could forge a row. The match is on
/// the decoded text; only what is printed is made visible.
pub(crate) const AWK_INDEX_COMMON: &str = r##"
    function unesc(s,    out, i, n, c, d) {
      if (index(s, "\\") == 0) return s
      out = ""
      n = length(s)
      for (i = 1; i <= n; i++) {
        c = substr(s, i, 1)
        if (c == "\\" && i < n) {
          d = substr(s, i + 1, 1)
          if (d == "t")  { out = out "\t"; i++; continue }
          if (d == "n")  { out = out "\n"; i++; continue }
          if (d == "r")  { out = out "\r"; i++; continue }
          if (d == "\\") { out = out "\\"; i++; continue }
        }
        out = out c
      }
      return out
    }
    function show(s,    out, i, n, c) {
      s = unesc(s)
      if (s !~ /[[:cntrl:]]/) return s
      out = ""
      n = length(s)
      for (i = 1; i <= n; i++) {
        c = substr(s, i, 1)
        if (c == "\t") out = out "\\t"
        else if (c == "\n") out = out "\\n"
        else if (c == "\r") out = out "\\r"
        else if (c ~ /[[:cntrl:]]/) out = out "?"
        else out = out c
      }
      return out
    }
    function human(b) {
      if (b !~ /^[0-9]+$/) return "size not known"
      if (b + 0 >= 1073741824) return sprintf("%.1f GiB", b / 1073741824)
      if (b + 0 >= 1048576) return sprintf("%.1f MiB", b / 1048576)
      return sprintf("%.1f KiB", b / 1024)
    }
    function header(line, names,    n, f, i) {
      n = split(line, f, "\t")
      for (i = 1; i <= n; i++) names[f[i]] = i
      return n
    }
    function val(f, names, key) {
      if (!(key in names)) return ""
      return f[names[key]]
    }
    function nk(s) {
      return (s == "") ? "not known" : s
    }
    function holder(row, names,    state, sealed, first, last, count, st, s) {
      state = show(val(row, names, "state"))
      sealed = show(val(row, names, "sealed_at"))
      first = show(val(row, names, "first_slice"))
      last = show(val(row, names, "last_slice"))
      count = show(val(row, names, "num_slices"))
      if (state == "this-tape") st = "this tape"
      else if (state == "sealed") st = (sealed == "") ? "sealed" : "sealed " sealed
      else if (state == "quarantined") {
        st = "QUARANTINED: a verify proved this medium bad, it may still read in part"
        if (sealed != "") st = st "; sealed " sealed
      }
      else st = (state == "") ? "state not known" : "state " state
      s = "    tape " nk(show(val(row, names, "volume"))) "  (" st ")"
      if (first != "" && last != "") s = s "  slices " first "-" last (count == "" ? "" : " of " count)
      s = s "\n        volume uuid " nk(show(val(row, names, "volume_uuid")))
      s = s "; cartridge serial " nk(show(val(row, names, "cartridge_serial")))
      s = s "; generation " nk(show(val(row, names, "generation")))
      return s
    }
  "##;

/// `--where UNIT` over one `index/units.tsv` (an envelope member,
/// `volume-format-v2.md` sec 1.5): every row for the unit, grouped by version,
/// with the slice range and count, the bytes, and each holding tape. The unit
/// name comes in through `WHERE_UNIT` in the environment, never `-v`, which
/// would process the backslashes of an escaped name; it is compared with the
/// DECODED `unit` column. Every unit name seen is appended to the file
/// `WHERE_KNOWN`, so a miss can list what the index does hold. `WHERE_TENANT`
/// names the tenant of a tenant envelope, whose rows carry no `tenant` column.
/// Exit status 0 when the unit has rows, 1 when it has none, 2 when the file
/// lacks the `unit` or `version` column.
pub(crate) const AWK_INDEX_WHERE: &str = r##"
    BEGIN {
      FS = "\t"
      want = ENVIRON["WHERE_UNIT"]
      known = ENVIRON["WHERE_KNOWN"]
      tenant = ENVIRON["WHERE_TENANT"]
    }
    NR == 1 {
      header($0, h)
      if (!("unit" in h) || !("version" in h)) { bad = 1; exit 2 }
      next
    }
    {
      split($0, f, "\t")
      u = unesc(val(f, h, "unit"))
      if (!(u in seen)) { seen[u] = 1; print show(val(f, h, "unit")) >> known }
      if (u != want) next
      found = 1
      t = ("tenant" in h) ? val(f, h, "tenant") : tenant
      if (!started || t != lastt) {
        if (started) print ""
        print "Unit " show(val(f, h, "unit")) (t == "" ? "" : " (tenant " show(t) ")") ":"
        started = 1; lastt = t; lastv = ""; vseen = 0
      }
      v = val(f, h, "version")
      if (!vseen || v != lastv) {
        print ""
        print "  Version " show(v) ": " show(val(f, h, "num_slices")) " slice(s), " human(val(f, h, "version_bytes")) " (" show(val(f, h, "version_bytes")) " bytes); unit uuid " nk(show(val(f, h, "unit_uuid")))
        lastv = v; vseen = 1
      }
      print holder(f, h)
    }
    END {
      if (bad) exit 2
      exit(found ? 0 : 1)
    }
  "##;

/// `--find TEXT` over one tenant envelope: `FIND_UNITS` is its
/// `index/units.tsv` (or `/dev/null`), the program's input its
/// `index/paths.tsv`. Prints the rows whose DECODED path contains the
/// literal text in `FIND_TEXT` (`index()`, not a pattern), at most
/// `FIND_LIMIT` of them, each as unit, version, kind, size and path, then the
/// tapes holding each printed version. `FIND_WHO` names the envelope. The
/// numbers printed and the numbers matched are written to the file
/// `FIND_COUNTS`, so the caller can say how many matches it did not print and
/// share one limit across envelopes. Exit status 2 when `paths.tsv` lacks a
/// `unit`, `version` or `path` column.
pub(crate) const AWK_INDEX_FIND: &str = r##"
    BEGIN {
      FS = "\t"
      needle = ENVIRON["FIND_TEXT"]
      limit = ENVIRON["FIND_LIMIT"] + 0
      uf = ENVIRON["FIND_UNITS"]
      cf = ENVIRON["FIND_COUNTS"]
      who = ENVIRON["FIND_WHO"]
      if ((getline line < uf) > 0) {
        header(line, uh)
        while ((getline line < uf) > 0) {
          split(line, r, "\t")
          k = unesc(val(r, uh, "unit")) SUBSEP val(r, uh, "version")
          nh[k]++
          hold[k, nh[k]] = line
        }
      }
      close(uf)
    }
    NR == 1 {
      header($0, ph)
      if (!("unit" in ph) || !("version" in ph) || !("path" in ph)) { bad = 1; exit 2 }
      next
    }
    {
      p = $(ph["path"])
      if (index(unesc(p), needle) == 0) next
      total++
      if (shown >= limit) next
      shown++
      if (shown == 1) print "In the index of " who ":"
      us = $(ph["unit"])
      vs = $(ph["version"])
      printf "  %s  v%s  %s  %s bytes  %s\n", show(us), show(vs), (("kind" in ph) ? show($(ph["kind"])) : "?"), (("size_bytes" in ph) ? show($(ph["size_bytes"])) : "?"), show(p)
      k = unesc(us) SUBSEP vs
      if (!(k in want)) { want[k] = 1; order[++nord] = k; dunit[k] = show(us); dver[k] = show(vs) }
    }
    END {
      if (bad) exit 2
      for (i = 1; i <= nord; i++) {
        k = order[i]
        print ""
        print "  Tapes holding " dunit[k] " version " dver[k] ":"
        if (nh[k] + 0 == 0) print "    none is listed in this envelope"
        for (j = 1; j <= nh[k]; j++) {
          split(hold[k, j], r, "\t")
          print holder(r, uh)
        }
      }
      print shown + 0, total + 0 > cf
      close(cf)
    }
  "##;

/// The files dar could not restore, from the log of one extraction (ADR-0012
/// item 102). dar prints `Error while restoring PATH : REASON` for a file whose
/// data failed its CRC or whose slice was cut short, and, when a slice was not
/// there at all, `PATH not restored (user choice)` (the form `DAMAGED_SKIPS=1`
/// adds: with every slice present the same words mean a file that already
/// existed). PATH is absolute: dar prints the `-R` it was given verbatim, so
/// `DAMAGED_ROOT` (that directory, without a trailing slash) is cut from the
/// front. A path may hold a newline, so a message continues over lines until
/// it ends: the REASON follows the LAST ` : ` of the message (a name may hold
/// one), and a skip ends with its fixed words.
///
/// Prints one path per line, relative to the unit root and escaped as the
/// index is (`volume-format-v2.md` sec 1.5: backslash, TAB, LF and CR only),
/// each once. Writes `ERRORS SKIPPED UNMAPPED FAILED` to the file
/// `DAMAGED_COUNTS`: the messages of each kind, the ones that could not be
/// turned into a path (so the list is short of them), and the figure dar
/// itself gave for `inode(s) failed to restore`.
pub(crate) const AWK_DAR_DAMAGED: &str = r##"
    function esc(s,    out, i, n, c) {
      out = ""
      n = length(s)
      for (i = 1; i <= n; i++) {
        c = substr(s, i, 1)
        if (c == "\\") out = out "\\\\"
        else if (c == "\t") out = out "\\t"
        else if (c == "\n") out = out "\\n"
        else if (c == "\r") out = out "\\r"
        else out = out c
      }
      return out
    }
    function lastsep(s,    p, q) {
      p = 0
      while ((q = index(substr(s, p + 1), " : ")) > 0) p = p + q
      return p
    }
    function emit(path,    e) {
      if (index(path, root) != 1 || length(path) <= rl) { unmapped++; return }
      e = esc(substr(path, rl + 1))
      if (!(e in seen)) { seen[e] = 1; print e }
    }
    BEGIN {
      root = ENVIRON["DAMAGED_ROOT"] "/"
      rl = length(root)
      skips = (ENVIRON["DAMAGED_SKIPS"] == "1")
      counts = ENVIRON["DAMAGED_COUNTS"]
      tail = " not restored (user choice)"
      tl = length(tail)
    }
    {
      if (acc != "" && (index($0, "Error while restoring ") == 1 || $0 ~ /^ -----/ || (kind == "S" && index($0, root) == 1))) {
        unmapped++; acc = ""; kind = ""
      }
      if (acc == "") {
        if (index($0, "Error while restoring ") == 1) { acc = substr($0, 23); kind = "E" }
        else if (skips && index($0, root) == 1) { acc = $0; kind = "S" }
        else {
          if ($0 ~ /^ [0-9]+ inode\(s\) failed to restore/) failed = $1 + 0
          next
        }
      } else {
        acc = acc "\n" $0
      }
      if (kind == "E") {
        p = lastsep(acc)
        if (p > 0) { emit(substr(acc, 1, p - 1)); errs++; acc = ""; kind = "" }
      } else {
        n = length(acc)
        if (n >= tl && substr(acc, n - tl + 1) == tail) { emit(substr(acc, 1, n - tl)); skipped++; acc = ""; kind = "" }
      }
    }
    END {
      if (acc != "") unmapped++
      print errs + 0, skipped + 0, unmapped + 0, failed + 0 > counts
      close(counts)
    }
  "##;

/// Which of the wanted paths a unit version's dar catalogue holds, and in
/// which slices (`--fill-from`, ADR-0012 item 102). The input is
/// `dar -l CATALOGUE -N -Q -Tslice`, whose entry lines are the slice number or
/// range, a TAB, flag brackets, the ten-character permission string and the
/// path (which runs on over further lines when it holds a newline). The
/// wanted paths come from the file named by `FILL_WANTED`, one per line,
/// escaped as sec 1.5 escapes a path; an entry is matched when its path, so
/// escaped, equals one of them exactly.
///
/// Prints `P TAB escaped path` for each wanted path the catalogue holds and
/// `S TAB n` once for each slice holding data of one of them: a file in
/// slices 3-5 needs all three, and a slice that holds only other files is not
/// asked for.
pub(crate) const AWK_FILL_LISTING: &str = r##"
    function esc(s,    out, i, n, c) {
      out = ""
      n = length(s)
      for (i = 1; i <= n; i++) {
        c = substr(s, i, 1)
        if (c == "\\") out = out "\\\\"
        else if (c == "\t") out = out "\\t"
        else if (c == "\n") out = out "\\n"
        else if (c == "\r") out = out "\\r"
        else out = out c
      }
      return out
    }
    function flush(    e, a, k, lo, hi, i) {
      if (!open) return
      open = 0
      e = esc(path)
      if ((e in want) && !(e in done)) {
        done[e] = 1
        print "P\t" e
        k = split(slices, a, "-")
        lo = a[1] + 0
        hi = (k > 1) ? a[2] + 0 : lo
        for (i = lo; i <= hi; i++) if (!(i in mark)) { mark[i] = 1; print "S\t" i }
      }
    }
    BEGIN {
      wf = ENVIRON["FILL_WANTED"]
      while ((getline line < wf) > 0) want[line] = 1
      close(wf)
      perm = " [^ ][-rwxsStT][-rwxsStT][-rwxsStT][-rwxsStT][-rwxsStT][-rwxsStT][-rwxsStT][-rwxsStT][-rwxsStT] "
    }
    $0 == "-----" { flush(); stopped = 1 }
    stopped { next }
    /^[0-9]+(-[0-9]+)?\t / {
      flush()
      t = index($0, "\t")
      rest = substr($0, t + 1)
      if (match(rest, perm)) {
        slices = substr($0, 1, t - 1)
        path = substr(rest, RSTART + RLENGTH)
        open = 1
      }
      next
    }
    open { path = path "\n" $0 }
    END { flush() }
  "##;

/// Does an envelope's `index/units.tsv` list one version of a unit? (The file
/// check, ADR-0012 item 103: the `paths.tsv` beside it is that version's file
/// list.) `CHK_UNIT` is the unit name, `CHK_VER` the version and `CHK_UUID` the
/// unit uuid the manifest gives for it (or empty). The unit is compared by its
/// DECODED name; when the row and the manifest both state a uuid they must
/// agree, so two tenants who each have a unit of one name are not mixed up.
/// Exit status 0 when listed, 1 when not, 2 when the file lacks the `unit` or
/// `version` column.
pub(crate) const AWK_FILE_CHECK_LISTED: &str = r##"
    BEGIN {
      FS = "\t"
      unit = ENVIRON["CHK_UNIT"]
      ver = ENVIRON["CHK_VER"] + 0
      uuid = ENVIRON["CHK_UUID"]
    }
    NR == 1 {
      header($0, h)
      if (!("unit" in h) || !("version" in h)) { bad = 1; exit 2 }
      next
    }
    {
      split($0, f, "\t")
      if (unesc(val(f, h, "unit")) != unit) next
      if (val(f, h, "version") + 0 != ver) next
      u = val(f, h, "unit_uuid")
      if (uuid != "" && u != "" && u != uuid) next
      found = 1
      exit 0
    }
    END {
      if (bad) exit 2
      exit(found ? 0 : 1)
    }
  "##;

/// The regular files of one version of a unit, from its `index/paths.tsv`, as
/// the list `sha256sum -c` takes (the file check, ADR-0012 item 103). `CHK_UNIT`
/// and `CHK_VER` name the version. `CHK_ONLY` is empty (every file) or a file
/// of escaped paths: only a path equal to one of them, or under one as a
/// directory, is listed (`--path`, and a fill).
///
/// Writes, to the files named in the environment: `CHK_SUMS`, one line
/// `HASH  NAME` for each file with a recorded hash whose name `sha256sum -c`
/// takes as it stands; `CHK_SPECIAL`, `HASH TAB escaped path` for each whose
/// name holds a backslash, a newline or a carriage return (these are hashed one
/// at a time, by the caller); and `CHK_COUNTS`, the five figures `ROWS FILES
/// CHECKED SPECIAL NOHASH`: rows of the version, regular files in scope,
/// files to check, those among them in `CHK_SPECIAL`, and files with no usable
/// hash recorded (or a path that leaves the unit, which is never hashed). A
/// column that is absent, as in an index written before the hash was added,
/// reads as no hash. Exit status 2 when `unit`, `version` or `path` is missing.
pub(crate) const AWK_FILE_CHECK_LIST: &str = r##"
    function selected(p,    rest, off, i) {
      if (p in want) return 1
      rest = p
      off = 0
      while ((i = index(rest, "/")) > 0) {
        off += i
        if (substr(p, 1, off - 1) in want) return 1
        rest = substr(rest, i + 1)
      }
      return 0
    }
    function leaves(p) {
      return (p ~ /^\// || p == ".." || p ~ /^\.\.\// || p ~ /\/\.\.\// || p ~ /\/\.\.$/)
    }
    BEGIN {
      FS = "\t"
      unit = ENVIRON["CHK_UNIT"]
      ver = ENVIRON["CHK_VER"] + 0
      sums = ENVIRON["CHK_SUMS"]
      special = ENVIRON["CHK_SPECIAL"]
      counts = ENVIRON["CHK_COUNTS"]
      only = ENVIRON["CHK_ONLY"]
      if (only != "") {
        while ((getline line < only) > 0) { want[unesc(line)] = 1; nonly++ }
        close(only)
      }
      printf "" > sums
      printf "" > special
    }
    NR == 1 {
      header($0, h)
      if (!("unit" in h) || !("version" in h) || !("path" in h)) { bad = 1; exit 2 }
      next
    }
    {
      split($0, f, "\t")
      if (unesc(val(f, h, "unit")) != unit) next
      if (val(f, h, "version") + 0 != ver) next
      rows++
      if (val(f, h, "kind") != "f") next
      raw = val(f, h, "path")
      p = unesc(raw)
      if (nonly > 0 && !selected(p)) next
      files++
      hash = val(f, h, "sha256")
      if (length(hash) != 64 || hash !~ /^[0-9a-f]+$/ || leaves(p)) { nohash++; next }
      checked++
      if (index(p, "\\") > 0 || index(p, "\n") > 0 || index(p, "\r") > 0) {
        nspecial++
        print hash "\t" raw > special
      } else {
        print hash "  " p > sums
      }
    }
    END {
      if (bad) exit 2
      print rows + 0, files + 0, checked + 0, nspecial + 0, nohash + 0 > counts
      close(counts)
      close(sums)
      close(special)
    }
  "##;

/// Which files `sha256sum -c --quiet` failed, named as the index names them.
/// The program reads the list that was checked (`CHK_SUMS`, lines of `HASH
/// NAME`, as its input) and the checker's report (the file `CHK_REPORT`: the
/// failures only, each `NAME: FAILED` or `NAME: FAILED open or read`). It
/// prints each failed file of the list, escaped as the index escapes a path,
/// to the file `CHK_BAD` in the list's order, and `NAMED UNMATCHED STRAY` to
/// `CHK_COUNTS`: failures found in the list, failures whose name was not in
/// it, and report lines that were not a failure at all. Only the failures are
/// held in memory.
pub(crate) const AWK_FILE_CHECK_FAILED: &str = r##"
    function esc(s,    out, i, n, c) {
      out = ""
      n = length(s)
      for (i = 1; i <= n; i++) {
        c = substr(s, i, 1)
        if (c == "\\") out = out "\\\\"
        else if (c == "\t") out = out "\\t"
        else if (c == "\n") out = out "\\n"
        else if (c == "\r") out = out "\\r"
        else out = out c
      }
      return out
    }
    function ends(s, t) {
      return length(s) > length(t) && substr(s, length(s) - length(t) + 1) == t
    }
    BEGIN {
      report = ENVIRON["CHK_REPORT"]
      bad = ENVIRON["CHK_BAD"]
      counts = ENVIRON["CHK_COUNTS"]
      open_fail = ": FAILED open or read"
      fail = ": FAILED"
      while ((getline line < report) > 0) {
        if (ends(line, open_fail)) name = substr(line, 1, length(line) - length(open_fail))
        else if (ends(line, fail)) name = substr(line, 1, length(line) - length(fail))
        else { stray++; continue }
        if (!(name in failed)) { failed[name] = 1; nfail++ }
      }
      close(report)
      printf "" > bad
    }
    {
      name = substr($0, 67)
      if (name in failed) { print esc(name) > bad; named++; delete failed[name] }
    }
    END {
      print named + 0, nfail - named, stray + 0 > counts
      close(counts)
      close(bad)
    }
  "##;
