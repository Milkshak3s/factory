#!/usr/bin/env bash
# Wraps target/release/factory in Factory.app, ad-hoc signs it, and builds the DMG.
# Usage, from the repo root after `cargo build --release`: bash packaging/macos.sh <version>
set -euo pipefail

version="$1"
stage=target/package/macos
app="$stage/Factory.app"
dmg="dist/factory-$version-arm64.dmg"

rm -rf "$stage"
mkdir -p "$app/Contents/MacOS" "$app/Contents/Resources" dist
cp target/release/factory "$app/Contents/MacOS/factory"
cp target/release/factory.icns "$app/Contents/Resources/factory.icns"
sed "s/@VERSION@/$version/g" packaging/Info.plist > "$app/Contents/Info.plist"

codesign --force --deep --sign - "$app"

ln -s /Applications "$stage/Applications"

# hdiutil sometimes fails on hosted runners with "Resource busy", and that shouldn't sink a release.
for attempt in 1 2 3; do
  if hdiutil create -volname "Factory $version" -srcfolder "$stage" -ov -format UDZO "$dmg"; then
    exit 0
  fi
  echo "hdiutil create failed on attempt $attempt" >&2
  sleep 10
done
exit 1
