#!/bin/sh
# Compiles the Icon Composer package (Margin.icon) into build/Assets.car and
# build/Margin.icns, which Dioxus.toml copies into the app bundle. Needs Xcode.
set -eu
cd "$(dirname "$0")"
mkdir -p build
DEVELOPER_DIR="${DEVELOPER_DIR:-/Applications/Xcode.app/Contents/Developer}" xcrun actool Margin.icon \
  --compile build --platform macosx --minimum-deployment-target 14.0 \
  --app-icon Margin --output-partial-info-plist build/partial.plist >/dev/null
echo "icon: build/Assets.car, build/Margin.icns"
