#!/usr/bin/env bash
# Build and run copad-macos as a proper .app bundle.
#
# The Copad executable links libcopad_ffi.a (Rust staticlib at
# <workspace>/target/release/libcopad_ffi.a). SwiftPM cannot run cargo
# itself from Package.swift, so this script wraps both build steps in
# the right order. Same wrapping in scripts/install-macos.sh.
set -e

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
cd "$SCRIPT_DIR"

# 1. Build the Rust staticlibs first so swift build's linker phase finds them.
#    Package.swift links BOTH libcopad_ffi.a AND libcopad_term.a (the alacritty
#    renderer/PTY lib), so both crates must be (re)built here — otherwise a
#    copad-term change (e.g. the terminal spawn) silently ships the stale .a.
(cd .. && cargo build --release -p copad-ffi -p copad-term)

# 2. Build the Swift app, which links the .a above via Package.swift's
#    linkerSettings (-L../target/release -lcopad_ffi).
swift build

APP_DIR=".build/debug/Copad.app"
CONTENTS="$APP_DIR/Contents"
MACOS="$CONTENTS/MacOS"
RESOURCES="$CONTENTS/Resources"

mkdir -p "$MACOS" "$RESOURCES"
cp .build/debug/Copad "$MACOS/Copad"

# Bundle icon — same shape as scripts/install-macos.sh. Copy from the
# checked-in .icns so the debug bundle picks up the same artwork as
# the release install.
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
    <string>0.1.0</string>
    <key>CFBundleShortVersionString</key>
    <string>0.1.0</string>
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

# Sign the bundle with a stable self-signed identity so macOS TCC keeps
# remembering granted permissions (Accessibility, Input Monitoring, …)
# across rebuilds. Without this step swift's ad-hoc linker signature
# changes cdhash every build and TCC re-prompts.
"$SCRIPT_DIR/../scripts/codesign-dev.sh" "$APP_DIR"

# Kill any running instance first so the rebuilt binary is used
pkill -x Copad 2>/dev/null || true
sleep 0.3

open -n "$APP_DIR"
