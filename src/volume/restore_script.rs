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
///
/// Each line printed is `number|tape_position|encrypted_bytes|sha256_encrypted`
/// for one slice of the chosen block, in manifest order; `tape_position` is
/// EMPTY for a slice the block lists with none, which is on another tape
/// (`volume-format-v2.md` sec 4.4, rule R6: never file 0). Standard error gets
/// one line, `VERSION MAY_SPAN`: the chosen block's `snapshot_version`, and 1
/// when its head states `may_span = true` (a unit that may run across tapes,
/// tapectl 1.7.0), else 0. `may_span` is read in the head only (#135). Given
/// `-v plain_out=FILE`, it also writes `number|size_bytes` there for each of
/// those slices, in the same order: every slice's plaintext size, which the
/// streamed restore cuts each slice at when it joins them for dar (#498).
pub(crate) const AWK_SELECT_VERSION: &str = r##"
    function flush() {
      if (in_s && num != "") {
        n[blk]++
        slice[blk, n[blk]] = num "|" tpos "|" eb "|" sha
        plain[blk, n[blk]] = num "|" sb
      }
      in_s = 0; num = ""; tpos = ""; eb = ""; sha = ""; sb = ""
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
    /^\[\[units\]\]/ { flush(); blk++; hit[blk] = 0; ver[blk] = -1; span[blk] = 0; in_u = 0; in_head = 1; next }
    /^\[/ { in_head = 0 }
    in_head && /^name = / {
      gsub(/"/, "", $3)
      if ($3 == unit) { in_u = 1; hit[blk] = 1 } else { in_u = 0 }
    }
    in_u && in_head && /^snapshot_version = / { ver[blk] = $3 + 0 }
    in_u && in_head && /^may_span = / { span[blk] = ($3 == "true") }
    in_u && /^\[\[units\.slices\]\]/ { flush(); in_s = 1; next }
    in_s && /^number = /           { num = $3 }
    in_s && /^tape_position = /    { tpos = $3 }
    in_s && /^encrypted_bytes = /  { eb = $3 }
    in_s && /^size_bytes = /       { sb = $3 }
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
        print best, span[pick] + 0 > "/dev/stderr"
        for (i = 1; i <= n[pick]; i++) print slice[pick, i]
        if (plain_out != "") for (i = 1; i <= n[pick]; i++) print plain[pick, i] > plain_out
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

/// One `unit|versions|slices|bytes|part` line per unit in an envelope
/// manifest, in first-seen order (issue #412 item 7): `--find-envelope`
/// prints it as a table for every envelope it opens, which with an operator
/// or escrow key is every unit on the tape. A unit stored in several snapshot
/// versions is one line, its versions joined with commas; slices and bytes are
/// summed over them.
///
/// `part` is empty unless a version block gives a `tape_position` to fewer of
/// its slices than it lists: a unit held across tapes of which this tape's
/// plan holds only a range (`volume-format-v2.md` §4.4; #478, the fresh read's
/// S2). It then names each such version and the run its plan holds, as
/// `version V, slices A to B of N`, joined with `; `. The plan only: this
/// reads no seal, so a tape that ended early (§4.2) may hold less.
pub(crate) const AWK_UNITS_TABLE: &str = r##"
    function close_block() {
      if (cur != "" && bn > 0 && bp < bn) {
        if (bp == 0) r = "version " bv ", none of its " bn " slices"
        else r = "version " bv ", slices " bf " to " bl " of " bn
        part[cur] = (part[cur] == "") ? r : part[cur] "; " r
      }
      bn = 0; bp = 0; bf = ""; bl = ""; bv = ""; num = ""
    }
    /^\[\[units\]\]/ { close_block(); in_head = 1; cur = ""; next }
    /^\[\[units\.slices\]\]/ { in_head = 0; num = ""; if (cur != "") n[cur]++; if (cur != "") bn++; next }
    /^\[/ { in_head = 0 }
    in_head && /^name = / {
      v = $3; gsub(/"/, "", v); cur = v
      if (!(cur in seen)) { seen[cur] = 1; order[++k] = cur }
    }
    in_head && /^snapshot_version = / && cur != "" {
      vers[cur] = (vers[cur] == "") ? $3 : vers[cur] "," $3
      bv = $3
    }
    !in_head && /^number = / && cur != "" { num = $3 }
    !in_head && /^tape_position = / && cur != "" { bp++; if (bf == "") bf = num; bl = num }
    /^encrypted_bytes = / && cur != "" { b[cur] += $3 }
    END {
      close_block()
      for (i = 1; i <= k; i++) {
        u = order[i]
        printf "%s|%s|%d|%.0f|%s\n", u, vers[u], n[u], b[u], part[u]
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
    function cut_here(row, names,    f, line) {
      if (!cut_read) {
        cut_read = 1
        f = ENVIRON["INDEX_CUT"]
        if (f != "") {
          while ((getline line < f) > 0) cutv[line] = 1
          close(f)
        }
      }
      if (val(row, names, "state") != "this-tape") return 0
      return ((val(row, names, "unit_uuid") "\t" val(row, names, "version")) in cutv)
    }
    function range_here(row, names,    f, line, q, key) {
      if (!range_read) {
        range_read = 1
        f = ENVIRON["INDEX_RANGE"]
        if (f != "") {
          while ((getline line < f) > 0) {
            split(line, q, "\t")
            rangev[q[1] "\t" q[2]] = q[3] "\t" q[4]
          }
          close(f)
        }
      }
      if (val(row, names, "state") != "this-tape") return ""
      key = val(row, names, "unit_uuid") "\t" val(row, names, "version")
      return (key in rangev) ? rangev[key] : ""
    }
    function partial(first, last, count) {
      if (first !~ /^[0-9]+$/ || last !~ /^[0-9]+$/ || count !~ /^[0-9]+$/) return 0
      return (first + 0 != 1 || last + 0 != count + 0)
    }
    function note_partial(    f) {
      f = ENVIRON["INDEX_PARTIAL"]
      if (f == "" || partial_noted) return
      partial_noted = 1
      print "1" >> f
      close(f)
    }
    function holder(row, names,    state, sealed, first, last, count, st, s, run, q, part, cut) {
      state = show(val(row, names, "state"))
      sealed = show(val(row, names, "sealed_at"))
      first = show(val(row, names, "first_slice"))
      last = show(val(row, names, "last_slice"))
      count = show(val(row, names, "num_slices"))
      run = range_here(row, names)
      if (run != "") { split(run, q, "\t"); first = q[1]; last = q[2] }
      cut = (state == "this-tape" && cut_here(row, names))
      part = !cut && (run != "" || partial(first, last, count))
      if (cut) st = "CUT on this tape: it ended early, before the last slice of this version, so it does not hold it whole"
      else if (state == "this-tape") st = "this tape"
      else if (state == "sealed") st = (sealed == "") ? "sealed" : "sealed " sealed
      else if (state == "quarantined") {
        st = "QUARANTINED: a verify proved this medium bad, it may still read in part"
        if (sealed != "") st = st "; sealed " sealed
      }
      else st = (state == "") ? "state not known" : "state " state
      if (part) { st = st ", a range: not the whole version"; note_partial() }
      s = "    tape " nk(show(val(row, names, "volume"))) "  (" st ")"
      if (first != "" && last != "") s = s "  slices " first "-" last (count == "" ? "" : " of " count)
      s = s "\n        volume uuid " nk(show(val(row, names, "volume_uuid")))
      s = s "; cartridge serial " nk(show(val(row, names, "cartridge_serial")))
      s = s "; generation " nk(show(val(row, names, "generation")))
      return s
    }
  "##;

/// The versions an envelope `MANIFEST.toml` lists that the tape does not hold
/// whole (`volume-format-v2.md` sec 4.2, 4.4), by which of each block's slices
/// the tape holds: a slice with no `tape_position` is on another tape (R6),
/// one whose `tape_position` is after `k` (the `ended_after` of a short tape,
/// or empty) is not on this one (R3), and the others are here. A block with
/// a slice not here is CUT, printed as `uuid TAB version`, unless it is a
/// RANGE: some of its slices are here and it states `may_span = true` (a unit
/// that may run across tapes, tapectl 1.7.0), or lists a slice with no
/// position. A range is appended to the file `RANGE_OUT` as `uuid TAB version
/// TAB first TAB last TAB slices`: the first and the last slice number here,
/// and how many slices the block lists. RESTORE.sh collects both for `--where`
/// and `--find`, whose index programs print a `this-tape` row of a cut version
/// as cut (`AWK_INDEX_COMMON`'s `cut_here`) and of a range as that run
/// (`range_here`), never as a holder of the whole version. `uuid`,
/// `snapshot_version` and `may_span` are read in the `[[units]]` head only, as
/// `AWK_SELECT_VERSION` reads them (#135).
pub(crate) const AWK_CUT_VERSIONS: &str = r##"
    function slice_done() {
      if (in_s && num != "") {
        n++
        if (tp == "") away++
        else if (k != "" && tp + 0 > k + 0) past++
        else { here++; if (first == "") first = num; last = num }
      }
      num = ""; tp = ""
    }
    function done_block() {
      slice_done()
      if (inblk && id != "" && away + past > 0) {
        if (here > 0 && (span || away > 0)) {
          if (rf != "") printf "%s\t%s\t%s\t%s\t%s\n", id, v, first, last, n >> rf
        } else printf "%s\t%s\n", id, v
      }
      inblk = 0; span = 0; id = ""; v = ""; in_s = 0
      n = 0; here = 0; away = 0; past = 0; first = ""; last = ""
    }
    BEGIN { rf = ENVIRON["RANGE_OUT"] }
    /^\[\[units\]\]/ { done_block(); inblk = 1; in_head = 1; next }
    /^\[\[units\.slices\]\]/ { slice_done(); in_head = 0; in_s = inblk; next }
    /^\[/ { slice_done(); in_head = 0; in_s = 0 }
    in_head && /^uuid = / { id = $3; gsub(/"/, "", id) }
    in_head && /^snapshot_version = / { v = $3 + 0 }
    in_head && /^may_span = / { span = ($3 == "true") }
    in_s && /^number = / { num = $3 }
    in_s && /^tape_position = / { tp = $3 }
    END { done_block(); if (rf != "") close(rf) }
  "##;

/// `--where UNIT` over one `index/units.tsv` (an envelope member,
/// `volume-format-v2.md` sec 1.5): every row for the unit, grouped by version,
/// with the slice range and count, the bytes, and each holding tape. The unit
/// name comes in through `WHERE_UNIT` in the environment, never `-v`, which
/// would process the backslashes of an escaped name; it is compared with the
/// DECODED `unit` column. Every unit name seen is appended to the file
/// `WHERE_KNOWN`, so a miss can list what the index does hold (every name, no
/// limit; `--where --all` adds each unit's newest version and holders).
/// `WHERE_TENANT` names the tenant of a tenant envelope, whose rows carry no
/// `tenant` column. Exit status 0 when the unit has rows, 1 when it has none, 2
/// when the file lacks the `unit` or `version` column.
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

/// `--where --all` over one `index/units.tsv` (`volume-format-v2.md` sec 1.5):
/// EVERY unit the index names, with no limit, so an heir can enumerate a box.
/// The whole file is read before anything is printed, and no row order is
/// relied on. The writer sorts rows by tenant, unit name, version (as a number)
/// and volume, so a unit that was deleted and made again under one name is two
/// units with two uuids whose rows interleave. A unit is therefore keyed by
/// tenant, name AND unit uuid, in the order first seen, and its newest version
/// is the highest by NUMBER (never by text, where `9` follows `10`). For each
/// unit it prints the newest version, whether this tape holds it (a holder row
/// in state `this-tape`), and every version with each tape that holds it, the
/// holders as `--where` prints them.
///
/// `ALL_TENANT` names the tenant of a tenant envelope, whose rows carry no
/// `tenant` column. Two files are written for the caller, who prints a summary
/// before the listing: `ALL_COUNTS` gets `UNITS ON_THIS_TAPE` (units listed;
/// units whose newest version this tape holds), and `ALL_ABSENT` gets, appended,
/// one line for each unit whose newest version this tape does not hold, with
/// the tapes that do. Exit status 0 when the file has units, 1 when it has
/// none, 2 when it lacks the `unit` or `version` column.
pub(crate) const AWK_INDEX_ALL: &str = r##"
    function holders_of(k,    j, r, s, lab, st, run, q) {
      s = ""
      for (j = 1; j <= rows[k]; j++) {
        split(row[k, j], r, "\t")
        lab = nk(show(val(r, h, "volume")))
        st = val(r, h, "state")
        run = range_here(r, h)
        if (st == "this-tape" && cut_here(r, h)) lab = lab " (CUT on this tape: not held whole)"
        else if (run != "") { split(run, q, "\t"); lab = lab " (this tape: slices " q[1] "-" q[2] " only)" }
        else if (st == "this-tape") lab = lab " (this tape)"
        else if (st == "quarantined") lab = lab " (QUARANTINED)"
        if (st != "this-tape" && partial(val(r, h, "first_slice"), val(r, h, "last_slice"), val(r, h, "num_slices")))
          lab = lab " (slices " show(val(r, h, "first_slice")) "-" show(val(r, h, "last_slice")) " only)"
        s = s (s == "" ? "" : ", ") lab
      }
      return (s == "") ? "no tape the index lists" : s
    }
    BEGIN {
      FS = "\t"
      tenant = ENVIRON["ALL_TENANT"]
      counts = ENVIRON["ALL_COUNTS"]
      absent = ENVIRON["ALL_ABSENT"]
    }
    NR == 1 {
      header($0, h)
      if (!("unit" in h) || !("version" in h)) { bad = 1; exit 2 }
      next
    }
    $0 == "" { next }
    {
      split($0, f, "\t")
      t = ("tenant" in h) ? val(f, h, "tenant") : tenant
      id = val(f, h, "unit_uuid")
      g = unesc(t) SUBSEP unesc(val(f, h, "unit")) SUBSEP id
      if (!(g in gnum)) { gnum[g] = ++ng; gten[ng] = t; gunit[ng] = val(f, h, "unit") }
      n = gnum[g]
      v = val(f, h, "version") + 0
      k = n SUBSEP v
      if (!(k in rows)) {
        rows[k] = 0
        nver[n]++
        ver[n, nver[n]] = v
        vtext[k] = val(f, h, "version")
        vslices[k] = val(f, h, "num_slices")
        vbytes[k] = val(f, h, "version_bytes")
        vuuid[k] = id
      }
      rows[k]++
      row[k, rows[k]] = $0
      if (val(f, h, "state") == "this-tape" && !cut_here(f, h) && range_here(f, h) == "") here[k] = 1
    }
    END {
      if (bad) exit 2
      for (n = 1; n <= ng; n++) {
        c = nver[n]
        for (i = 1; i <= c; i++) vs[i] = ver[n, i]
        for (i = 2; i <= c; i++) {
          x = vs[i]
          for (j = i - 1; j >= 1 && vs[j] > x; j--) vs[j + 1] = vs[j]
          vs[j + 1] = x
        }
        newest = vs[c]
        kn = n SUBSEP newest
        who = (gten[n] == "") ? "" : " (tenant " show(gten[n]) ")"
        onhere = (kn in here)
        if (onhere) tothere++
        print "Unit " show(gunit[n]) who ": newest version " show(vtext[kn]) ", " (onhere ? "on this tape" : "NOT on this tape")
        if (!onhere) printf "    %s%s, version %s, unit uuid %s: %s\n", show(gunit[n]), who, show(vtext[kn]), nk(show(vuuid[kn])), holders_of(kn) >> absent
        for (i = 1; i <= c; i++) {
          k = n SUBSEP vs[i]
          if (i > 1) print ""
          print "  Version " show(vtext[k]) (vs[i] == newest ? " (newest)" : "") ": " show(vslices[k]) " slice(s), " human(vbytes[k]) " (" show(vbytes[k]) " bytes); unit uuid " nk(show(vuuid[k]))
          for (j = 1; j <= rows[k]; j++) {
            split(row[k, j], r, "\t")
            print holder(r, h)
          }
        }
        print ""
      }
      print ng + 0, tothere + 0 > counts
      close(counts)
      close(absent)
      exit(ng > 0 ? 0 : 1)
    }
  "##;

/// The holders one `index/units.tsv` names for one version of a unit, each as
/// `--where` prints it (`AWK_INDEX_COMMON`'s `holder`), for the refusal of a
/// unit held across tapes (`volume-format-v2.md` sec 4.4): the tapes that hold
/// its other slices, with each one's run. `CHK_UNIT`, `CHK_VER` and `CHK_UUID`
/// name the version as `AWK_FILE_CHECK_LISTED` takes them. With `CHK_SLICE`
/// set, only the holders whose run holds that slice are printed (the tape to
/// start a restore across tapes from holds slice 1; the one asked for next
/// holds the next slice): a run is the `first_slice` and `last_slice` columns,
/// or, on the `this-tape` row of a range, the run this tape really holds
/// (`INDEX_RANGE`), and a row with no run known is not printed. Exit status 0
/// when the file names a holder (of that slice, with `CHK_SLICE`), 1 when it
/// does not, 2 when it lacks the `unit` or `version` column.
pub(crate) const AWK_INDEX_HOLDERS: &str = r##"
    BEGIN {
      FS = "\t"
      unit = ENVIRON["CHK_UNIT"]
      ver = ENVIRON["CHK_VER"] + 0
      uuid = ENVIRON["CHK_UUID"]
      want = ENVIRON["CHK_SLICE"]
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
      if (want != "") {
        lo = val(f, h, "first_slice")
        hi = val(f, h, "last_slice")
        run = range_here(f, h)
        if (run != "") { split(run, q, "\t"); lo = q[1]; hi = q[2] }
        if (lo !~ /^[0-9]+$/ || hi !~ /^[0-9]+$/) next
        if (want + 0 < lo + 0 || want + 0 > hi + 0) next
      }
      found = 1
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

/// Which of the wanted paths a unit version's dar catalogue holds, and which
/// slices a fill of them reads (`--fill-from`, ADR-0012 item 102). The input
/// is `dar -l CATALOGUE -N -Q -Tslice`, whose entry lines are the slice number
/// or range (none for an entry that has no data in a slice, such as a
/// symlink), a TAB, flag brackets, the ten-character permission string and
/// the path (which runs on over further lines when it holds a newline). The
/// wanted paths come from the file named by `FILL_WANTED`, one per line,
/// escaped as sec 1.5 escapes a path; an entry is matched when its path, so
/// escaped, equals one of them exactly.
///
/// Prints `P TAB escaped path` for each wanted path the catalogue holds and
/// `S TAB n` once for each slice the fill reads: the slices of each wanted
/// entry (a file in slices 3-5 needs all three), of every entry under a wanted
/// folder, and of the own entry of each folder above a wanted entry (dar
/// restores a folder before anything in it, and restores nothing in a folder
/// whose entry is in a slice it does not have). A slice that holds only other
/// files is not asked for.
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
    function under(e,    rest, off, i) {
      rest = e
      off = 0
      while ((i = index(rest, "/")) > 0) {
        off += i
        if (substr(e, 1, off - 1) in want) return 1
        rest = substr(rest, i + 1)
      }
      return 0
    }
    function need(r,    a, k, lo, hi, i) {
      if (r == "") return
      k = split(r, a, "-")
      lo = a[1] + 0
      hi = (k > 1) ? a[2] + 0 : lo
      for (i = lo; i <= hi; i++) if (!(i in mark)) { mark[i] = 1; print "S\t" i }
    }
    function above(e,    rest, off, i, d) {
      rest = e
      off = 0
      while ((i = index(rest, "/")) > 0) {
        off += i
        d = substr(e, 1, off - 1)
        if (d in dirsl) need(dirsl[d])
        rest = substr(rest, i + 1)
      }
    }
    function flush(    e) {
      if (!open) return
      open = 0
      e = esc(path)
      if (isdir) dirsl[e] = slices
      if ((e in want) && !(e in done)) {
        done[e] = 1
        print "P\t" e
        found[++nfound] = e
        need(slices)
      } else if (under(e)) {
        need(slices)
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
    /^([0-9]+(-[0-9]+)?)?\t / {
      flush()
      t = index($0, "\t")
      rest = substr($0, t + 1)
      if (match(rest, perm)) {
        slices = substr($0, 1, t - 1)
        isdir = (substr(rest, RSTART + 1, 1) == "d")
        path = substr(rest, RSTART + RLENGTH)
        open = 1
      }
      next
    }
    open { path = path "\n" $0 }
    END {
      flush()
      for (i = 1; i <= nfound; i++) above(found[i])
    }
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

/// The regular files of one version of a unit, from its `index/paths.tsv`, with
/// the sha256 each must have (the file check, ADR-0012 item 103). `CHK_UNIT`
/// and `CHK_VER` name the version. `CHK_ONLY` is empty (every file) or a file
/// of escaped paths: only a path equal to one of them, or under one as a
/// directory, is listed (`--path`, and a fill).
///
/// Writes, to the files named in the environment: `CHK_SUMS`, one line
/// `HASH TAB escaped path` for each file with a recorded hash, the path in the
/// escaped form the index gives it (the caller hashes each file on its own,
/// from standard input, so no name is ever read back from `sha256sum`); and
/// `CHK_COUNTS`, the four figures `ROWS FILES CHECKED NOHASH`: rows of the
/// version, regular files in scope, files to check, and files with no usable
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
      counts = ENVIRON["CHK_COUNTS"]
      only = ENVIRON["CHK_ONLY"]
      if (only != "") {
        while ((getline line < only) > 0) { want[unesc(line)] = 1; nonly++ }
        close(only)
      }
      printf "" > sums
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
      print hash "\t" raw > sums
    }
    END {
      if (bad) exit 2
      print rows + 0, files + 0, checked + 0, nohash + 0 > counts
      close(counts)
      close(sums)
    }
  "##;

/// The files dar named, with each folder among them that the index records
/// replaced by what is under it (ADR-0012 item 102). When the slice that holds
/// a folder's own entry is not there, dar names the folder, restores nothing in
/// it ("No file in this directory will be restored.") and names nothing under
/// it; a fill restores files, so the list names those. The input is the
/// version's `index/paths.tsv`. `CHK_UNIT` and `CHK_VER` name the version,
/// `CHK_LIST` is the file of escaped paths, one per line, and `CHK_ONLY` is
/// empty or a file of the escaped paths a `--path` restore asked for.
///
/// A path of the list that the index records as a directory (`kind` `d`) is
/// replaced by each entry under it that a fill restores: every one that is not
/// a directory, and every directory with nothing under it (the folder itself,
/// when it was empty); with `CHK_ONLY`, only those equal to one of its paths or
/// under one. Every other path of the list is printed as it stands. The list's
/// order is kept, a folder's entries in the index's order where the folder was,
/// each path once, escaped as sec 1.5 escapes a path; a path that leaves the
/// unit is never printed. Exit status 2 when the file lacks the `unit`,
/// `version`, `kind` or `path` column (the caller then keeps the list as dar
/// gave it).
pub(crate) const AWK_DAMAGED_DIRS: &str = r##"
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
    function leaves(p) {
      return (p ~ /^\// || p == ".." || p ~ /^\.\.\// || p ~ /\/\.\.\// || p ~ /\/\.\.$/)
    }
    function selected(p,    rest, off, i) {
      if (nonly == 0 || (p in want)) return 1
      rest = p
      off = 0
      while ((i = index(rest, "/")) > 0) {
        off += i
        if (substr(p, 1, off - 1) in want) return 1
        rest = substr(rest, i + 1)
      }
      return 0
    }
    function top(p, set,    rest, off, i, d) {
      rest = p
      off = 0
      while ((i = index(rest, "/")) > 0) {
        off += i
        d = substr(p, 1, off - 1)
        if (d in set) return d
        rest = substr(rest, i + 1)
      }
      return ""
    }
    function parent(p,    k, i) {
      k = 0
      while ((i = index(substr(p, k + 1), "/")) > 0) k += i
      return (k > 0) ? substr(p, 1, k - 1) : ""
    }
    function put(line) {
      if (!(line in printed)) { printed[line] = 1; print line }
    }
    BEGIN {
      FS = "\t"
      split("", listed)
      split("", isdir)
      unit = ENVIRON["CHK_UNIT"]
      ver = ENVIRON["CHK_VER"] + 0
      lf = ENVIRON["CHK_LIST"]
      while ((getline line < lf) > 0) { n++; ord[n] = line; listed[unesc(line)] = 1 }
      close(lf)
      only = ENVIRON["CHK_ONLY"]
      if (only != "") {
        while ((getline line < only) > 0) { want[unesc(line)] = 1; nonly++ }
        close(only)
      }
    }
    NR == 1 {
      header($0, h)
      if (!("unit" in h) || !("version" in h) || !("kind" in h) || !("path" in h)) { bad = 1; exit 2 }
      next
    }
    {
      split($0, f, "\t")
      if (unesc(val(f, h, "unit")) != unit) next
      if (val(f, h, "version") + 0 != ver) next
      p = unesc(val(f, h, "path"))
      k = val(f, h, "kind")
      if (k == "d" && (p in listed)) isdir[p] = 1
      if (top(p, listed) == "") next
      m++
      rp[m] = p
      rk[m] = k
      haschild[parent(p)] = 1
    }
    END {
      if (bad) exit 2
      for (j = 1; j <= m; j++) rt[j] = top(rp[j], isdir)
      for (i = 1; i <= n; i++) {
        p = unesc(ord[i])
        if (!(p in isdir)) { put(ord[i]); continue }
        if (!(p in haschild) && selected(p)) put(esc(p))
        for (j = 1; j <= m; j++) {
          if (rt[j] != p || leaves(rp[j]) || !selected(rp[j])) continue
          if (rk[j] == "d" && (rp[j] in haschild)) continue
          put(esc(rp[j]))
        }
      }
    }
  "##;

/// One dar header (the slice header, or the header dar writes on its standard
/// output), read from `od -An -v -tu1` of the bytes it starts: prints its
/// length, its flag (`T`, `N` or `E`), the slice size it records (`-` for
/// none) and the offset of its data-name label, one line; or, for anything
/// it does not recognise, what that is, and exits 1. RESTORE.sh joins a
/// unit's slices back into the one stream dar reads on its standard input
/// (`stream_unit`, issue #498, ADR-0012 item 181) with what this finds, and
/// refuses rather than guesses: the same checks, in the same order, as
/// `dar::slice`'s parser in tapectl (magic, 10-byte internal name, flag,
/// extension `T`, an infinint count of at most 16 fields, each a u16 type, an
/// infinint length and the value; type 1 the slice size, type 3 the 10-byte
/// data name, any other type refused). An infinint is zero or more `00` bytes,
/// one byte with a single bit set, then the value in 4-byte groups: `80` one
/// group, `40` two, each leading `00` eight more. A value wider than six
/// significant bytes is refused: awk counts in doubles, exact to 2^53. No
/// exponent operator: busybox awk can be built without it.
pub(crate) const AWK_DAR_HEADER: &str = r##"
    function bad(why) { print why; exit 1 }
    function byte() {
      if (p >= 4096) bad("a header longer than 4096 bytes")
      if (p >= n) bad("the header ends early")
      return b[p++]
    }
    function inf(   z, pre, lz, bit, w, v, i, c, sig) {
      z = 0
      pre = byte()
      while (pre == 0) { z++; pre = byte() }
      lz = -1
      bit = 128
      for (i = 0; i < 8; i++) {
        if (pre == bit) lz = i
        bit = bit / 2
      }
      if (lz < 0) bad("an integer width byte " pre " with more than one bit set")
      w = (z * 8 + lz + 1) * 4
      v = 0
      sig = 0
      for (i = 0; i < w; i++) {
        c = byte()
        if (c != 0 || sig > 0) sig++
        v = v * 256 + c
      }
      if (sig > 6) bad("an integer " w " bytes wide")
      return v
    }
    { for (i = 1; i <= NF; i++) b[n++] = $i + 0 }
    END {
      p = 0
      if (byte() != 0 || byte() != 0 || byte() != 0 || byte() != 123) bad("the data does not start with the magic number of dar")
      for (i = 0; i < 10; i++) byte()
      flag = byte()
      if (flag != 84 && flag != 78 && flag != 69) bad("an unknown slice flag " flag)
      if (byte() != 84) bad("a header extension that is not a list of fields")
      count = inf()
      if (count > 16) bad(count " header fields")
      dn = -1
      size = -1
      for (f = 0; f < count; f++) {
        t = byte() * 256
        t += byte()
        len = inf()
        if (len > 4096) bad("a header field " len " bytes long")
        at = p
        if (t == 3) {
          if (len != 10 || dn >= 0) bad("a data-name field " len " bytes long")
          for (i = 0; i < 10; i++) byte()
          dn = at
        } else if (t == 1) {
          if (size >= 0) bad("two slice-size fields")
          size = inf()
          if (p - at != len) bad("a slice-size field whose length disagrees with it")
        } else {
          bad("an unknown header field type " t)
        }
      }
      if (dn < 0) bad("no data-name field")
      printf "%d %s %s %d\n", p, (flag == 84 ? "T" : (flag == 78 ? "N" : "E")), (size < 0 ? "-" : sprintf("%.0f", size)), dn
    }
  "##;
