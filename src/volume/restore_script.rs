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

/// Lists tenant/operator envelope positions in on-tape order (the file list
/// is already position-sorted, and the format's fixed zone order already
/// puts tenant envelope(s) before the operator envelope before its
/// backup). Stateless — no accumulator, no `END` block — so by the letter
/// of the classification this is plain field extraction; it is named
/// anyway because it is the one shared discovery step both
/// `--find-envelope` and `--restore` walk to locate the envelope that
/// opens under the supplied key.
pub(crate) const AWK_FIND_ENVELOPE: &str = r##"$2=="tenant_envelope" || $2=="operator_envelope" || $2=="operator_envelope_backup" { print $1 }"##;

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
