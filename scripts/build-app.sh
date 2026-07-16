#!/bin/bash
# Build RSD.app — the native search palette. Output: dist/RSD.app
set -euo pipefail
cd "$(dirname "$0")/.."
cargo build --release -p rsd-daemon -p rsd-worker
cd app
swift build -c release
APP=../dist/RSD.app
rm -rf "$APP"
mkdir -p "$APP/Contents/MacOS"
cp "$(swift build -c release --show-bin-path)/RSD" "$APP/Contents/MacOS/RSD"
cp ../target/release/rsd-daemon ../target/release/rsd-worker "$APP/Contents/MacOS/"
cat > "$APP/Contents/Info.plist" <<'PLIST'
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
  <key>CFBundleName</key><string>RSD</string>
  <key>CFBundleDisplayName</key><string>RSD</string>
  <key>CFBundleIdentifier</key><string>dev.rsd.app</string>
  <key>CFBundleExecutable</key><string>RSD</string>
  <key>CFBundlePackageType</key><string>APPL</string>
  <key>CFBundleShortVersionString</key><string>0.1.0</string>
  <key>LSMinimumSystemVersion</key><string>14.0</string>
  <key>NSHighResolutionCapable</key><true/>
  <key>LSUIElement</key><true/>
</dict></plist>
PLIST
codesign --force --deep -s - "$APP" 2>/dev/null || true
echo "built dist/RSD.app"
