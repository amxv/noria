#!/usr/bin/env bash
set -euo pipefail
root="$(cd "$(dirname "$0")/../.." && pwd)"
out="${1:-$root/dist/Noria.app}"
arch="${2:-arm64}"
version="${NORIA_VERSION:-$(cat "$root/VERSION")}"
case "$arch" in arm64|x86_64) ;; *) echo 'unsupported macOS architecture' >&2; exit 1;; esac
case "$out" in /|"$HOME"|"$root") echo 'unsafe app build target' >&2; exit 1;; esac
mkdir -p "$out/Contents/MacOS"
cp "$root/apps/menubar/Info.plist" "$out/Contents/Info.plist"
for key in CFBundleShortVersionString CFBundleVersion NoriaVersion; do /usr/libexec/PlistBuddy -c "Set :$key $version" "$out/Contents/Info.plist"; done
swiftc -O -target "$arch-apple-macos13.0" \
 "$root/apps/menubar/Sources/Models.swift" \
 "$root/apps/menubar/Sources/SystemIntegration.swift" \
 "$root/apps/menubar/Sources/QuotaRowView.swift" \
 "$root/apps/menubar/Sources/AppDelegate.swift" \
 "$root/apps/menubar/Sources/main.swift" \
 -o "$out/Contents/MacOS/noria-menubar"
codesign --force --sign - "$out"
codesign --verify --deep --strict "$out"
