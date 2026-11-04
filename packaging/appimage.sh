#!/usr/bin/env bash
# Builds the AppImage from target/release/factory. No libraries are bundled, because eframe loads
# X11, Wayland, GL and fontconfig from the host.
# Usage, from the repo root after `cargo build --release`:
#   APPIMAGETOOL_VERSION=<tag> bash packaging/appimage.sh <version>
set -euo pipefail

version="$1"
stage=target/package/appimage
appdir="$stage/Factory.AppDir"
tool="$stage/appimagetool"

rm -rf "$stage"
mkdir -p "$appdir/usr/bin" dist
install -m 755 target/release/factory "$appdir/usr/bin/factory"
install -m 755 packaging/AppRun "$appdir/AppRun"
install -m 644 packaging/factory.desktop "$appdir/factory.desktop"
install -m 644 target/release/factory.png "$appdir/factory.png"

curl --fail --location --silent --show-error --output "$tool" \
  "https://github.com/AppImage/AppImageKit/releases/download/$APPIMAGETOOL_VERSION/appimagetool-x86_64.AppImage"
chmod +x "$tool"

# The tool is an AppImage itself, and hosted runners have no FUSE to mount it with.
ARCH=x86_64 "$tool" --appimage-extract-and-run "$appdir" "dist/factory-$version-x64.AppImage"
