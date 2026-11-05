#!/usr/bin/env bash
# Build and run copad-macos as a proper .app bundle.
#
# SwiftPM cannot run cargo, so this wraps the Rust staticlib build and the Swift build.
set -e

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
cd "$SCRIPT_DIR"

# Package.swift links both libcopad_ffi.a and libcopad_term.a; build both or a copad-term change
# silently ships the stale .a.
(cd .. && cargo build --release -p copad-ffi -p copad-term)

swift build

APP_DIR=".build/debug/Copad.app"
CONTENTS="$APP_DIR/Contents"
MACOS="$CONTENTS/MacOS"
RESOURCES="$CONTENTS/Resources"

mkdir -p "$MACOS" "$RESOURCES"
cp .build/debug/Copad "$MACOS/Copad"

if [[ -f "Resources/AppIcon.icns" ]]; then
    cp "Resources/AppIcon.icns" "$RESOURCES/AppIcon.icns"
fi

cat > "$CONTENTS/Info.plist" << 'EOF'
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>CFBundleExecutable</key>
    <string>Copad</string>
    <key>CFBundleIconFile</key>
    <string>AppIcon</string>
    <key>CFBundleIdentifier</key>
    <string>com.marshall.copad</string>
    <key>CFBundleName</key>
    <string>copad</string>
    <key>CFBundleDisplayName</key>
    <string>copad</string>
    <key>CFBundleVersion</key>
    <string>1.0.0</string>
    <key>CFBundleShortVersionString</key>
    <string>1.0.0</string>
    <key>CFBundlePackageType</key>
    <string>APPL</string>
    <key>LSMinimumSystemVersion</key>
    <string>14.0</string>
    <key>NSHighResolutionCapable</key>
    <true/>
    <key>NSPrincipalClass</key>
    <string>NSApplication</string>
</dict>
</plist>
EOF

# Signed with a stable self-signed identity so TCC keeps granted permissions across rebuilds
# (swift's ad-hoc signature changes cdhash every build and TCC re-prompts).
"$SCRIPT_DIR/../scripts/codesign-dev.sh" "$APP_DIR"

pkill -x Copad 2>/dev/null || true
sleep 0.3

open -n "$APP_DIR"
