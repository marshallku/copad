#!/usr/bin/env bash
# Regenerate platform icons from a single master PNG.
#
# Inputs:
#   assets/icons/copad.png  — square master, 1024x1024 (or larger)
#
# Outputs (checked in so a fresh checkout builds with icons without
# having ImageMagick / Python on the build host):
#   copad-linux/icons/hicolor/<size>x<size>/apps/copad.png   for size ∈ {16,22,24,32,48,64,128,256,512}
#   copad-macos/Resources/AppIcon.icns                        — multi-res .icns (PNG-encoded entries)
#
# Run this whenever the master PNG changes, then commit the regenerated
# files. -strip kills all ancillary chunks. Source PNG is a colorful
# raster (coffee mug + terminal prompt); palette quantization would
# visibly band the brown gradients, so we keep full truecolor with
# alpha and rely on PNG filter/compression for size.
#
# Requires: ImageMagick 7 (`magick`), Python 3 (for the .icns assembly
# — ImageMagick's icns coder writes a single-image .icns, which Finder
# treats as low-quality at high DPI).
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
SRC="$REPO_ROOT/assets/icons/copad.png"

if [[ ! -f "$SRC" ]]; then
    echo "error: master icon not found at $SRC" >&2
    exit 1
fi
if ! command -v magick >/dev/null 2>&1; then
    echo "error: ImageMagick 7 (magick) is required" >&2
    exit 1
fi
if ! command -v python3 >/dev/null 2>&1; then
    echo "error: python3 is required to build the .icns" >&2
    exit 1
fi

PNG_OPTS=(
    -strip
    -define png:compression-level=9
    -define png:compression-filter=5
    -define png:compression-strategy=1
    -define png:exclude-chunk=all
)

echo "==> Linux hicolor PNGs"
for size in 16 22 24 32 48 64 128 256 512; do
    out="$REPO_ROOT/copad-linux/icons/hicolor/${size}x${size}/apps/copad.png"
    mkdir -p "$(dirname "$out")"
    magick "$SRC" -resize "${size}x${size}" "${PNG_OPTS[@]}" "$out"
done

echo "==> macOS iconset (staging)"
ICONSET="$(mktemp -d)/copad.iconset"
mkdir -p "$ICONSET"
gen() {
    local size="$1"; shift
    local tmp="$ICONSET/.tmp-${size}.png"
    magick "$SRC" -resize "${size}x${size}" "${PNG_OPTS[@]}" "$tmp"
    for name in "$@"; do cp "$tmp" "$ICONSET/$name"; done
    rm "$tmp"
}
gen 16   icon_16x16.png
gen 32   icon_16x16@2x.png icon_32x32.png
gen 64   icon_32x32@2x.png
gen 128  icon_128x128.png
gen 256  icon_128x128@2x.png icon_256x256.png
gen 512  icon_256x256@2x.png icon_512x512.png
gen 1024 icon_512x512@2x.png

echo "==> macOS AppIcon.icns"
ICNS_OUT="$REPO_ROOT/copad-macos/Resources/AppIcon.icns"
mkdir -p "$(dirname "$ICNS_OUT")"
ICONSET="$ICONSET" ICNS_OUT="$ICNS_OUT" python3 - <<'PY'
import os, struct, pathlib

# Apple icns OSType → expected pixel size. PNG-encoded entries are
# valid for these types on macOS 10.7+ (Big Sur+ uses them exclusively).
TYPES = [
    (b"icp4", 16),
    (b"icp5", 32),
    (b"icp6", 64),
    (b"ic07", 128),
    (b"ic08", 256),
    (b"ic09", 512),
    (b"ic10", 1024),  # 512@2x
    (b"ic11", 32),    # 16@2x
    (b"ic12", 64),    # 32@2x
    (b"ic13", 256),   # 128@2x
    (b"ic14", 512),   # 256@2x
]
size_to_iconset_name = {
    16:   "icon_16x16.png",
    32:   "icon_32x32.png",
    64:   "icon_32x32@2x.png",
    128:  "icon_128x128.png",
    256:  "icon_256x256.png",
    512:  "icon_512x512.png",
    1024: "icon_512x512@2x.png",
}
iconset = pathlib.Path(os.environ["ICONSET"])
entries = []
for ostype, size in TYPES:
    data = (iconset / size_to_iconset_name[size]).read_bytes()
    entries.append(ostype + struct.pack(">I", 8 + len(data)) + data)
body = b"".join(entries)
icns = b"icns" + struct.pack(">I", 8 + len(body)) + body
pathlib.Path(os.environ["ICNS_OUT"]).write_bytes(icns)
print(f"  wrote {os.environ['ICNS_OUT']} ({len(icns)} bytes, {len(TYPES)} entries)")
PY

echo "Done. Commit the regenerated files under copad-linux/icons/ and copad-macos/Resources/."
