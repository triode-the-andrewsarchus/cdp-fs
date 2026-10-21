#!/usr/bin/env bash
set -e

MOUNT_DIR=$(mktemp -d /tmp/cdp-fs-mount-XXXXXX)
USER_DATA_DIR=$(mktemp -d /tmp/cdp-fs-profile-XXXXXX)
BINARY="/home/vcarlson/cdp-fs/target/debug/cdp-fs"

cleanup() {
    echo "--- Cleaning up ---"
    if mountpoint -q "$MOUNT_DIR"; then
        echo "Unmounting $MOUNT_DIR..."
        fusermount -u "$MOUNT_DIR" || true
    fi
    if [ -n "$FS_PID" ] && kill -0 $FS_PID 2>/dev/null; then
        echo "Killing cdp-fs process $FS_PID..."
        kill $FS_PID 2>/dev/null || true
    fi
    if [ -n "$CHROME_PID" ] && kill -0 $CHROME_PID 2>/dev/null; then
        echo "Killing Chromium process $CHROME_PID..."
        kill $CHROME_PID 2>/dev/null || true
    fi
    rm -rf "$MOUNT_DIR" "$USER_DATA_DIR"
    echo "Cleanup complete."
}
trap cleanup EXIT

echo "=== Test 1: Mounting cdp-fs when Chrome is NOT running ==="
$BINARY "$MOUNT_DIR" --endpoint "http://localhost:9222" &
FS_PID=$!
sleep 1

# Check mountpoint is mounted
if ! mountpoint -q "$MOUNT_DIR"; then
    echo "FAIL: $MOUNT_DIR is not mounted"
    exit 1
fi
echo "✓ cdp-fs mounted successfully even when Chrome is not running"

# ls should return empty directory without error
CONTENTS=$(ls -A "$MOUNT_DIR")
if [ -n "$CONTENTS" ]; then
    echo "FAIL: Expected empty mountpoint, got: $CONTENTS"
    exit 1
fi
echo "✓ ls returned empty directory without error"

echo "=== Starting Headless Chromium ==="
chromium --headless=new --remote-debugging-port=9222 --user-data-dir="$USER_DATA_DIR" "https://example.com" &
CHROME_PID=$!
sleep 2

# Verify CDP endpoint responded
curl -s http://localhost:9222/json/version > /dev/null
echo "✓ Chromium DevTools is listening on port 9222"

echo "=== Test 2: Listing tabs in mountpoint ==="
# Allow cdp-fs to detect tabs
sleep 1
ls -la "$MOUNT_DIR"
TAB_DIR="$MOUNT_DIR/Example Domain"
if [ ! -d "$TAB_DIR" ]; then
    echo "FAIL: Expected directory '$TAB_DIR' to exist"
    exit 1
fi
echo "✓ Tab title folder found: '$TAB_DIR'"

echo "=== Test 3: Checking tab files ==="
ls -la "$TAB_DIR"
for file in url title text html eval screenshot.png id reload activate close; do
    if [ ! -f "$TAB_DIR/$file" ]; then
        echo "FAIL: File '$file' does not exist in tab directory"
        exit 1
    fi
done
echo "✓ All standard files exist in tab directory"

echo "=== Test 4: Testing 'cat url' and 'cat title' ==="
URL=$(cat "$TAB_DIR/url")
echo "URL: $URL"
if [[ "$URL" != *"example.com"* ]]; then
    echo "FAIL: Unexpected URL '$URL'"
    exit 1
fi

TITLE=$(cat "$TAB_DIR/title")
echo "Title: $TITLE"
if [ "$TITLE" != "Example Domain" ]; then
    echo "FAIL: Unexpected title '$TITLE'"
    exit 1
fi
echo "✓ URL and Title read correctly"

echo "=== Test 5: Testing 'cat text' and 'cat html' ==="
TEXT=$(cat "$TAB_DIR/text")
if [[ "$TEXT" != *"examples"* ]]; then
    echo "FAIL: Unexpected text content: '$TEXT'"
    exit 1
fi
echo "✓ 'text' read successfully (length: ${#TEXT} chars)"

HTML=$(cat "$TAB_DIR/html")
if [[ "$HTML" != *"<html"* ]]; then
    echo "FAIL: Unexpected HTML content: '$HTML'"
    exit 1
fi
echo "✓ 'html' read successfully (length: ${#HTML} chars)"

echo "=== Test 6: Testing 'cat id' ==="
TAB_ID=$(cat "$TAB_DIR/id")
echo "Tab Target ID: $TAB_ID"
if [ -z "$TAB_ID" ]; then
    echo "FAIL: Empty tab ID"
    exit 1
fi
echo "✓ Tab ID read successfully"

echo "=== Test 7: Testing 'cat screenshot.png' ==="
SCREENSHOT_FILE="/tmp/test_screen_$$.png"
cat "$TAB_DIR/screenshot.png" > "$SCREENSHOT_FILE"
PNG_SIZE=$(wc -c < "$SCREENSHOT_FILE")
echo "Screenshot PNG size: $PNG_SIZE bytes"
if [ "$PNG_SIZE" -lt 1000 ]; then
    echo "FAIL: Screenshot too small ($PNG_SIZE bytes)"
    rm -f "$SCREENSHOT_FILE"
    exit 1
fi
# Verify PNG magic bytes (\x89PNG)
PNG_HEADER=$(head -c 4 "$SCREENSHOT_FILE")
if [ "$PNG_HEADER" != "$(printf '\x89PNG')" ]; then
    echo "FAIL: Screenshot file does not have PNG header"
    rm -f "$SCREENSHOT_FILE"
    exit 1
fi
rm -f "$SCREENSHOT_FILE"
echo "✓ Live screenshot captured and verified as valid PNG"

echo "=== Test 8: Testing JavaScript evaluation via 'echo ... > eval' and 'cat eval' ==="
echo "21 * 2" > "$TAB_DIR/eval"
EVAL_OUT=$(cat "$TAB_DIR/eval")
echo "Eval result: $EVAL_OUT"
if [ "$EVAL_OUT" != "42" ]; then
    echo "FAIL: Expected '42', got '$EVAL_OUT'"
    exit 1
fi
echo "✓ JS eval succeeded"

echo "=== Test 9: Testing setting title via 'echo ... > title' ==="
echo "Updated Dynamic Title" > "$TAB_DIR/title"
sleep 0.5
NEW_TITLE=$(cat "$TAB_DIR/title")
echo "New title from cat: $NEW_TITLE"
if [ "$NEW_TITLE" != "Updated Dynamic Title" ]; then
    echo "FAIL: Expected 'Updated Dynamic Title', got '$NEW_TITLE'"
    exit 1
fi
echo "✓ Title update succeeded"

echo "=== Test 9b: Testing navigation via 'echo ... > url' ==="
echo "https://example.org" > "$TAB_DIR/url"
sleep 1
NAV_URL=$(cat "$TAB_DIR/url")
echo "Navigated URL: $NAV_URL"
if [[ "$NAV_URL" != *"example.org"* ]]; then
    echo "FAIL: Expected URL to contain example.org, got '$NAV_URL'"
    exit 1
fi
echo "✓ Navigation succeeded"

echo "=== Test 10: Testing creating new tab via 'mkdir' ==="
mkdir "$MOUNT_DIR/about:blank"
sleep 1
ls -la "$MOUNT_DIR"
echo "✓ New tab created via mkdir"

echo "=== Test 11: Testing 'rm -rf /mountpoint/tab-name' to close tab ==="
TAB_COUNT_BEFORE=$(ls -1 "$MOUNT_DIR" | wc -l)
echo "Tabs before rm: $TAB_COUNT_BEFORE"

# rm -rf the new tab
rm -rf "$MOUNT_DIR/about:blank"
sleep 1

TAB_COUNT_AFTER=$(ls -1 "$MOUNT_DIR" | wc -l)
echo "Tabs after rm: $TAB_COUNT_AFTER"
if [ "$TAB_COUNT_AFTER" -ge "$TAB_COUNT_BEFORE" ]; then
    echo "FAIL: Tab was not closed after rm -rf"
    exit 1
fi
echo "✓ 'rm -rf' successfully closed the tab!"

echo "=== Test 12: Testing accessing a closed tab ==="
if cat "$MOUNT_DIR/about:blank/url" 2>/dev/null; then
    echo "FAIL: Expected error reading closed tab"
    exit 1
else
    echo "✓ Gracefully returned error when reading closed tab (no crash)"
fi

echo "=========================================="
echo " ALL TESTS PASSED SUCCESSFULLY! "
echo "=========================================="
