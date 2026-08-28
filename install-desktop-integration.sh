#!/bin/bash
# Install GOnedrive's file-manager integration for the current user.
#
# Nothing here needs root: emblems and the Nautilus extension both live under
# ~/.local/share, which is exactly where per-user integration belongs.
set -euo pipefail
cd "$(dirname "$0")"

ICONS="$HOME/.local/share/icons/hicolor/scalable/emblems"
APPS="$HOME/.local/share/icons/hicolor/scalable/apps"
EXTENSIONS="$HOME/.local/share/nautilus-python/extensions"
AUTOSTART="$HOME/.config/autostart"

install -d "$ICONS" "$APPS" "$EXTENSIONS" "$AUTOSTART"
install -m644 data/icons/hicolor/scalable/emblems/*.svg "$ICONS/"
install -m644 data/nautilus/gonedrive.py "$EXTENSIONS/"

# The OneDrive cloud, and the sync folder set to wear it in the file manager.
install -m644 data/icons/onedrive.svg "$APPS/gonedrive.svg"
SYNC_ROOT="${GONEDRIVE_SYNC_ROOT:-$HOME/OneDrive}"
if [ -d "$SYNC_ROOT" ]; then
    gio set "$SYNC_ROOT" metadata::custom-icon "file://$APPS/gonedrive.svg" 2>/dev/null \
        && echo "  folder icon -> $SYNC_ROOT"
fi

# Start the tray front-end on login. It is a persistent status-notifier item,
# not a window — without an autostart entry there is simply no tray until the
# user launches GOnedrive from the app menu by hand.
install -m644 data/org.gonedrive.Gonedrive.desktop "$AUTOSTART/org.gonedrive.Gonedrive.desktop"

# Without this the new emblems resolve to nothing until the next login.
gtk-update-icon-cache -f -t "$HOME/.local/share/icons/hicolor" 2>/dev/null || true

echo "Installed:"
echo "  emblems   -> $ICONS"
echo "  extension -> $EXTENSIONS/gonedrive.py"
echo "  autostart -> $AUTOSTART/org.gonedrive.Gonedrive.desktop"
echo
echo "Restart the file manager to pick it up:"
echo "  nautilus -q"
