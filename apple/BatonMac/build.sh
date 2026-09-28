#!/bin/sh
# Build Baton.app: release daemon → xcodegen → xcodebuild (signed).
#   CARGO_TARGET_DIR   cargo output (default: <repo>/target)
#   DERIVED_DATA       xcodebuild -derivedDataPath (default: ./build)
#   CONFIG             Debug | Release (default: Release)
# Extra args go to xcodebuild (e.g. DEVELOPMENT_TEAM=… BATON_BUNDLE_ID=…).
set -eu
here="$(cd "$(dirname "$0")" && pwd)"
repo="$(cd "$here/../.." && pwd)"
target="${CARGO_TARGET_DIR:-$repo/target}"
derived="${DERIVED_DATA:-$here/build}"
config="${CONFIG:-Release}"

(cd "$repo" && CARGO_TARGET_DIR="$target" cargo build --release -p baton-daemon)
(cd "$here" && xcodegen generate --quiet)
xcodebuild -project "$here/BatonMac.xcodeproj" -scheme BatonMac -configuration "$config" \
  -derivedDataPath "$derived" -allowProvisioningUpdates \
  BATON_DAEMON_BIN="$target/release/baton" "$@" build | tail -n 5
echo "app: $derived/Build/Products/$config/Baton.app"
