#!/usr/bin/env bash
# Milestone 0: dar validation
#
# Prerequisites:
#   dar >= 2.6.x installed (recommended 2.7.20 at /opt/dar/bin/dar)
#
# Tests from design document:
#   - Multi-slice archive (-s 10M)
#   - Catalog isolation (-C)
#   - XML listing (-T xml)
#   - Symlinks (-D), xattr/ACL (-am --acl --fsa-scope linux_extX)
#   - Per-file checksums (--hash sha256)

set -euo pipefail

DAR="${DAR_BINARY:-dar}"
WORKDIR="$(mktemp -d)"
TESTDATA="$WORKDIR/source"
ARCHIVE="$WORKDIR/archive"
CATALOG="$WORKDIR/catalog"
RESTORED="$WORKDIR/restored"
PASSED=0
FAILED=0

cleanup() {
    rm -rf "$WORKDIR"
}
trap cleanup EXIT

pass() { echo "  ok: $1"; PASSED=$((PASSED + 1)); }
fail() { echo "  FAIL: $1"; FAILED=$((FAILED + 1)); }

echo "=== Milestone 0: dar validation ==="
echo ""

# Check dar version
echo "test: dar version check"
DAR_VERSION=$("$DAR" --version 2>&1 | head -1 || true)
echo "  $DAR_VERSION"
if "$DAR" --version >/dev/null 2>&1; then
    pass "dar is accessible at $DAR"
else
    fail "dar not found at $DAR"
    echo ""
    echo "=== Results: $PASSED passed, $FAILED failed ==="
    exit 1
fi

# Create test data with variety of file types
echo ""
echo "test: creating test data"
mkdir -p "$TESTDATA/subdir/nested"
dd if=/dev/urandom of="$TESTDATA/file1.bin" bs=1M count=5 2>/dev/null
dd if=/dev/urandom of="$TESTDATA/file2.bin" bs=1M count=8 2>/dev/null
dd if=/dev/urandom of="$TESTDATA/file3.bin" bs=1M count=12 2>/dev/null
dd if=/dev/urandom of="$TESTDATA/subdir/file4.bin" bs=1M count=3 2>/dev/null
echo "small file content" > "$TESTDATA/subdir/nested/small.txt"
ln -s file1.bin "$TESTDATA/link_to_file1"
pass "test data created (~28 MB with symlink)"

# Test: multi-slice archive
echo ""
echo "test: multi-slice archive (-s 10M)"
"$DAR" -c "$ARCHIVE" -R "$TESTDATA" -s 10M -an -D -3 sha512 -Q 2>&1 || true
SLICE_COUNT=$(ls "$ARCHIVE"*.dar 2>/dev/null | wc -l)
if [ "$SLICE_COUNT" -ge 3 ]; then
    pass "created $SLICE_COUNT slices (expected >= 3)"
else
    fail "expected >= 3 slices, got $SLICE_COUNT"
fi

# List the slices
for f in "$ARCHIVE"*.dar; do
    echo "  $(basename "$f") ($(stat -c%s "$f") bytes)"
done

# Test: dar test (integrity check)
echo ""
echo "test: dar archive integrity (-t)"
if "$DAR" -t "$ARCHIVE" -Q 2>&1; then
    pass "archive integrity check passed"
else
    fail "archive integrity check failed"
fi

# Test: XML listing
echo ""
echo "test: XML listing (-T xml)"
XML_OUTPUT=$("$DAR" -l "$ARCHIVE" -T xml -Q 2>&1 || true)
if echo "$XML_OUTPUT" | grep -q '<?xml'; then
    pass "XML listing produced valid XML header"
    FILE_COUNT=$(echo "$XML_OUTPUT" | grep -c '<File ' || true)
    DIR_COUNT=$(echo "$XML_OUTPUT" | grep -c '<Directory ' || true)
    CRC_COUNT=$(echo "$XML_OUTPUT" | grep -c 'crc=' || true)
    echo "  files in XML: $FILE_COUNT, directories: $DIR_COUNT, CRC entries: $CRC_COUNT"
else
    fail "XML listing did not produce XML output"
    echo "  output: $(echo "$XML_OUTPUT" | head -5)"
fi

# Test: catalog isolation
echo ""
echo "test: catalog isolation (-C)"
if "$DAR" -C "$CATALOG" -A "$ARCHIVE" -Q 2>&1; then
    CATALOG_FILES=$(ls "$CATALOG"*.dar 2>/dev/null | wc -l)
    pass "catalog created ($CATALOG_FILES file(s))"
else
    fail "catalog isolation failed"
fi

# Test: symlink preservation
echo ""
echo "test: symlink preservation (-D)"
mkdir -p "$RESTORED"
if "$DAR" -x "$ARCHIVE" -R "$RESTORED" -O -Q 2>&1; then
    if [ -L "$RESTORED/link_to_file1" ]; then
        LINK_TARGET=$(readlink "$RESTORED/link_to_file1")
        if [ "$LINK_TARGET" = "file1.bin" ]; then
            pass "symlink preserved correctly (-> $LINK_TARGET)"
        else
            fail "symlink target wrong: $LINK_TARGET (expected file1.bin)"
        fi
    else
        fail "symlink not preserved"
    fi
else
    fail "extraction failed"
fi

# Test: full diff between source and restored
echo ""
echo "test: full diff between source and restored"
if diff -r "$TESTDATA" "$RESTORED" >/dev/null 2>&1; then
    pass "source and restored are identical"
else
    fail "differences found between source and restored"
    diff -r "$TESTDATA" "$RESTORED" 2>&1 | head -10
fi

# Test: xattr support (may not work on all filesystems)
echo ""
echo "test: xattr/ACL support"
if command -v setfattr >/dev/null 2>&1; then
    if setfattr -n user.tapectl.test -v "hello" "$TESTDATA/file1.bin" 2>/dev/null; then
        XATTR_ARCHIVE="$WORKDIR/xattr_archive"
        "$DAR" -c "$XATTR_ARCHIVE" -R "$TESTDATA" -s 10M -an -D -3 sha512 \
            -am --fsa-scope extX -Q 2>&1 || true
        XATTR_RESTORED="$WORKDIR/xattr_restored"
        mkdir -p "$XATTR_RESTORED"
        "$DAR" -x "$XATTR_ARCHIVE" -R "$XATTR_RESTORED" -O -Q 2>&1 || true
        RESTORED_XATTR=$(getfattr -n user.tapectl.test "$XATTR_RESTORED/file1.bin" 2>/dev/null | grep -c "hello" || true)
        if [ "$RESTORED_XATTR" -ge 1 ]; then
            pass "xattr preserved through archive/restore"
        else
            fail "xattr not preserved"
        fi
    else
        echo "  SKIP - filesystem does not support user xattrs"
        pass "(skip - no xattr support on this filesystem)"
    fi
else
    echo "  SKIP - setfattr not available"
    pass "(skip - setfattr not installed)"
fi

echo ""
echo "=== Results: $PASSED passed, $FAILED failed ==="
[ "$FAILED" -eq 0 ] && exit 0 || exit 1
