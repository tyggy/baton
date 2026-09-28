#!/bin/sh
# Xcode build phase: copy the release `baton` daemon into the app bundle,
# sign it with the app's identity (hardened runtime) so the app's signature
# seals it, and install the LaunchAgent plist SMAppService registers.
set -eu
: "${BATON_DAEMON_BIN:?}" "${TARGET_BUILD_DIR:?}" "${CONTENTS_FOLDER_PATH:?}"
if [ ! -x "$BATON_DAEMON_BIN" ]; then
  echo "error: daemon binary not found at $BATON_DAEMON_BIN — run 'cargo build --release' (or ./build.sh), or set BATON_DAEMON_BIN" >&2
  exit 1
fi
contents="$TARGET_BUILD_DIR/$CONTENTS_FOLDER_PATH"
mkdir -p "$contents/Helpers" "$contents/Library/LaunchAgents"
dest="$contents/Helpers/baton"
# Replace (new inode), never overwrite in place: a running daemon mapped
# from the old file would be killed with OS_REASON_CODESIGNING.
rm -f "$dest"
cp "$BATON_DAEMON_BIN" "$dest"
chmod 755 "$dest"

plist="$contents/Library/LaunchAgents/$BATON_DAEMON_LABEL.plist"
sed -e "s/__LABEL__/$BATON_DAEMON_LABEL/g" -e "s/__BUNDLE_ID__/$PRODUCT_BUNDLE_IDENTIFIER/g" \
  "$SRCROOT/LaunchAgents/daemon.plist" > "$plist"
plutil -lint "$plist" >/dev/null

identity="${EXPANDED_CODE_SIGN_IDENTITY:-}"
[ -n "$identity" ] || identity="-"
codesign --force --sign "$identity" --options runtime --timestamp=none \
  --identifier "$BATON_DAEMON_LABEL" "$dest"
echo "embedded $BATON_DAEMON_BIN -> $dest (signed: $identity)"
