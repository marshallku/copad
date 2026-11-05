#!/bin/bash
set -e

echo "Building copad..."
cargo build --release -p copad-linux

echo "Installing binary..."
sudo install -Dm755 target/release/copad /usr/local/bin/copad

echo "Installing desktop entry..."
# Basename must match the GTK app_id so Wayland compositors map the window to this entry.
sudo install -Dm644 copad-linux/com.marshall.copad.desktop \
    /usr/share/applications/com.marshall.copad.desktop
# Remove a pre-rename "copad.desktop" so the launcher does not show two entries.
sudo rm -f /usr/share/applications/copad.desktop

echo "Installing hicolor icons..."
for size in 16 22 24 32 48 64 128 256 512; do
    sudo install -Dm644 \
        "copad-linux/icons/hicolor/${size}x${size}/apps/copad.png" \
        "/usr/share/icons/hicolor/${size}x${size}/apps/copad.png"
done
# Best-effort: skip if the tool is missing.
if command -v gtk-update-icon-cache >/dev/null 2>&1; then
    sudo gtk-update-icon-cache -q -t /usr/share/icons/hicolor || true
fi

echo "Done. copad is now available as a system terminal."
echo "You can set it as default with: gsettings set org.gnome.desktop.default-applications.terminal exec copad"
