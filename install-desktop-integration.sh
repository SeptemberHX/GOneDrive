#!/bin/bash
# Install GOnedrive's file-manager integration for the current user.
#
# Nothing here needs root: emblems and the Nautilus extension both live under
# ~/.local/share, which is exactly where per-user integration belongs.
set -euo pipefail
cd "$(dirname "$0")"

ICONS="$HOME/.local/share/icons/hicolor/scalable/emblems"
EXTENSIONS="$HOME/.local/share/nautilus-python/extensions"

install -d "$ICONS" "$EXTENSIONS"
install -m644 data/icons/hicolor/scalable/emblems/*.svg "$ICONS/"
install -m644 data/nautilus/gonedrive.py "$EXTENSIONS/"

# Without this the new emblems resolve to nothing until the next login.
gtk-update-icon-cache -f -t "$HOME/.local/share/icons/hicolor" 2>/dev/null || true

echo "Installed:"
echo "  emblems   -> $ICONS"
echo "  extension -> $EXTENSIONS/gonedrive.py"
echo
echo "Restart the file manager to pick it up:"
echo "  nautilus -q"
