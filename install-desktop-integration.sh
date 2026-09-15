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

# A development build used to install a user-unit drop-in that pointed at a
# binary named gonedrived-codex. Drop-ins override the unit we install below,
# so leave that known legacy override out of the active configuration. Keep a
# disabled copy for recovery instead of deleting a user's file.
migrate_legacy_daemon_override() {
    local override="$HOME/.config/systemd/user/gonedrived.service.d/codex-release.conf"
    [ -f "$override" ] || return 0
    grep -Eq '^[[:space:]]*ExecStart=[^[:space:]]*/gonedrived-codex[[:space:]]+run[[:space:]]*$' \
        "$override" || return 0

    local disabled="${override}.disabled"
    if [ -e "$disabled" ]; then
        disabled="${override}.disabled.$(date +%s).$$"
    fi
    mv "$override" "$disabled"
    echo "  disabled legacy daemon override -> $disabled"
}

install -d "$ICONS" "$APPS" "$EXTENSIONS" "$AUTOSTART"
install -m644 data/icons/hicolor/scalable/emblems/*.svg "$ICONS/"
install -m644 data/nautilus/gonedrive.py "$EXTENSIONS/"

migrate_legacy_daemon_override

# The application icon (OneDrive cloud + GNOME foot) and the plain cloud used
# for the sync folder in the file manager.
install -m644 data/icons/gonedrive-app.svg "$APPS/org.gonedrive.Gonedrive.svg"
install -m644 data/icons/onedrive.svg "$APPS/gonedrive.svg"
SYNC_ROOT="${GONEDRIVE_SYNC_ROOT:-$HOME/OneDrive}"
if [ -d "$SYNC_ROOT" ]; then
    gio set "$SYNC_ROOT" metadata::custom-icon "file://$APPS/gonedrive.svg" 2>/dev/null \
        && echo "  folder icon -> $SYNC_ROOT"
fi

# Start the tray front-end on login — silently: --background brings up the
# status-notifier item without popping the window. (Launching from the app menu
# has no such flag, so a deliberate open still shows the window.)
sed 's/^Exec=gonedrive$/Exec=gonedrive --background/' \
    data/org.gonedrive.Gonedrive.desktop > "$AUTOSTART/org.gonedrive.Gonedrive.desktop"

# Without this the new emblems resolve to nothing until the next login.
gtk-update-icon-cache -f -t "$HOME/.local/share/icons/hicolor" 2>/dev/null || true

echo "Installed:"
echo "  emblems   -> $ICONS"
echo "  extension -> $EXTENSIONS/gonedrive.py"
echo "  autostart -> $AUTOSTART/org.gonedrive.Gonedrive.desktop"
echo
echo "Restart the file manager to pick it up:"
echo "  nautilus -q"
