#!/usr/bin/env bash
# Builds the .deb from target/release/factory.
# Usage, from the repo root after `cargo build --release`: bash packaging/deb.sh <version>
set -euo pipefail

version="$1"
root=target/package/deb

rm -rf "$root"
mkdir -p "$root/DEBIAN" dist
install -D -m 755 target/release/factory "$root/usr/bin/factory"
install -D -m 644 packaging/factory.desktop "$root/usr/share/applications/factory.desktop"
install -D -m 644 target/release/factory.png "$root/usr/share/icons/hicolor/256x256/apps/factory.png"
sed "s/@VERSION@/$version/g" packaging/deb-control > "$root/DEBIAN/control"

# Ubuntu's dpkg-deb defaults to zstd, which older Debian releases can't read.
dpkg-deb --build --root-owner-group -Zxz "$root" "dist/factory-$version-x64.deb"
