#!/bin/bash
# Build a Debian package of GOnedrive.
#
#   GONEDRIVE_CLIENT_ID=<your-entra-app-id> ./build-deb.sh
#
# Produces gonedrive_<version>_<arch>.deb in the repository root. When supplied,
# the client id is baked into the binaries so the package ships ready to sign
# in. Without it the package still builds; users must set `client_id` in their
# config before starting the daemon.
#
# The package installs:
#   /usr/bin/{gonedrived,gonedrive,gonedrive-hsm}
#   the privileged helper as a system service (enabled on install)
#   the sync daemon as a systemd *user* service (enabled for every user)
#   the app + folder icons, the emblems, the .desktop and a silent autostart,
#   and the Nautilus extension.
set -euo pipefail
umask 022
cd "$(dirname "$0")"

if [ -z "${GONEDRIVE_CLIENT_ID:-}" ]; then
    echo ">> GONEDRIVE_CLIENT_ID is unset; building a package that requires user configuration."
else
    echo ">> Building with the configured GONEDRIVE_CLIENT_ID (value hidden)."
fi

VERSION=$(grep -m1 '^version' Cargo.toml | sed -E 's/.*"([^"]+)".*/\1/')
ARCH=$(dpkg --print-architecture)
STAGE=$(mktemp -d)
chmod 0755 "$STAGE"
trap 'rm -rf "$STAGE"' EXIT

echo ">> Building release binaries (GOnedrive $VERSION, $ARCH)…"
cargo build --release --workspace

echo ">> Laying out the package tree…"

# --- binaries ---
install -Dm755 target/release/gonedrived    "$STAGE/usr/bin/gonedrived"
install -Dm755 target/release/gonedrive     "$STAGE/usr/bin/gonedrive"
install -Dm755 target/release/gonedrive-hsm "$STAGE/usr/bin/gonedrive-hsm"

# --- systemd units (ExecStart repointed from /usr/local/bin to /usr/bin) ---
install -d "$STAGE/usr/lib/systemd/system" "$STAGE/usr/lib/systemd/user"
sed 's#/usr/local/bin#/usr/bin#' data/gonedrive-hsm.service \
    > "$STAGE/usr/lib/systemd/system/gonedrive-hsm.service"
sed 's#/usr/local/bin#/usr/bin#' data/gonedrived.service \
    > "$STAGE/usr/lib/systemd/user/gonedrived.service"

# --- D-Bus activation: launching the panel starts the daemon if it is not up ---
install -Dm644 data/org.gonedrive.Daemon.service \
    "$STAGE/usr/share/dbus-1/services/org.gonedrive.Daemon.service"

# --- desktop entry, and a silent autostart of the tray ---
install -Dm644 data/org.gonedrive.Gonedrive.desktop \
    "$STAGE/usr/share/applications/org.gonedrive.Gonedrive.desktop"
install -d "$STAGE/etc/xdg/autostart"
sed 's/^Exec=gonedrive$/Exec=gonedrive --background/' data/org.gonedrive.Gonedrive.desktop \
    > "$STAGE/etc/xdg/autostart/org.gonedrive.Gonedrive.desktop"

# --- icons ---
APPS="$STAGE/usr/share/icons/hicolor/scalable/apps"
EMBLEMS="$STAGE/usr/share/icons/hicolor/scalable/emblems"
install -Dm644 data/icons/gonedrive-app.svg "$APPS/org.gonedrive.Gonedrive.svg"
install -Dm644 data/icons/onedrive.svg      "$APPS/gonedrive.svg"
install -d "$EMBLEMS"
install -m644 data/icons/hicolor/scalable/emblems/*.svg "$EMBLEMS/"

# --- Nautilus extension ---
install -Dm644 data/nautilus/gonedrive.py \
    "$STAGE/usr/share/nautilus-python/extensions/gonedrive.py"

# --- package metadata ---
INSTALLED_KB=$(du -sk "$STAGE" | cut -f1)
install -d "$STAGE/DEBIAN"
cat > "$STAGE/DEBIAN/control" <<CONTROL
Package: gonedrive
Version: $VERSION
Architecture: $ARCH
Maintainer: GOnedrive <noreply@example.com>
Installed-Size: $INSTALLED_KB
Section: net
Priority: optional
Depends: libc6, libgtk-4-1 (>= 4.18), libadwaita-1-0 (>= 1.7), python3 (>= 3.11), python3-gi, init-system-helpers
Recommends: python3-nautilus, gnome-shell-extension-appindicator
Description: OneDrive client for Linux with files-on-demand
 GOnedrive syncs Microsoft OneDrive two ways and supports files-on-demand:
 files appear on disk as placeholders and download only when opened, the way
 the Windows client works. A privileged helper handles the kernel's
 pre-content hooks; the sync daemon and the panel run unprivileged.
CONTROL

cat > "$STAGE/DEBIAN/postinst" <<'POSTINST'
#!/bin/sh
set -e
if [ "$1" = configure ]; then
    systemctl daemon-reload || true
    # The privileged helper (system service); restart so an upgrade takes.
    systemctl enable gonedrive-hsm.service || true
    systemctl restart gonedrive-hsm.service || true
    # The sync daemon runs per user; enable it for every user's session.
    systemctl --global enable gonedrived.service || true
    # Reload and restart every active user instance. A global enable alone does
    # not replace an already-running daemon during an upgrade.
    deb-systemd-invoke --user daemon-reload || true
    deb-systemd-invoke --user restart gonedrived.service || true
    gtk-update-icon-cache -f -t /usr/share/icons/hicolor 2>/dev/null || true
    update-desktop-database /usr/share/applications 2>/dev/null || true
fi
POSTINST

cat > "$STAGE/DEBIAN/prerm" <<'PRERM'
#!/bin/sh
set -e
if [ "$1" = remove ] || [ "$1" = purge ]; then
    deb-systemd-invoke --user stop gonedrived.service || true
    systemctl disable --now gonedrive-hsm.service || true
    systemctl --global disable gonedrived.service || true
fi
PRERM

cat > "$STAGE/DEBIAN/postrm" <<'POSTRM'
#!/bin/sh
set -e
if [ "$1" = remove ] || [ "$1" = purge ]; then
    systemctl daemon-reload 2>/dev/null || true
    deb-systemd-invoke --user daemon-reload || true
    gtk-update-icon-cache -f -t /usr/share/icons/hicolor 2>/dev/null || true
fi
POSTRM

chmod 755 "$STAGE/DEBIAN/postinst" "$STAGE/DEBIAN/prerm" "$STAGE/DEBIAN/postrm"

OUT="gonedrive_${VERSION}_${ARCH}.deb"
echo ">> Building $OUT…"
dpkg-deb --root-owner-group --build "$STAGE" "$OUT"
echo
echo "Done: $OUT"
echo "Install/update with:  sudo apt install --reinstall ./$OUT   (or: sudo dpkg -i $OUT)"
if [ -z "${GONEDRIVE_CLIENT_ID:-}" ]; then
    echo "Before sign-in: set client_id in ~/.config/gonedrive/config.toml"
else
    echo "Then sign in:  gonedrived login   (or open GOnedrive and click 登录)"
fi
