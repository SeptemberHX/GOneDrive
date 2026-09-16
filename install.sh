#!/bin/bash
# GOnedrive installer, for working from a source tree.
#
#   sudo ./install.sh          # binaries + privileged helper service
#   ./install.sh --user        # per-user pieces (daemon unit, Nautilus, icons)
#
# Split on purpose: only the helper needs root, and mixing the two under sudo
# would litter root-owned files through the user's home.
#
# Mutually exclusive with the .deb (see build-deb.sh). This script puts
# binaries in /usr/local/bin, which precedes /usr/bin on the default PATH, and
# unit and D-Bus service files under $HOME, which shadow the package's copies
# in /usr/lib and /usr/share. Both together mean `apt upgrade gonedrive`
# installs a new package that never runs: the old binaries keep starting, with
# nothing on screen to say so.
set -euo pipefail
cd "$(dirname "$0")"

# Refuse rather than shadow. GONEDRIVE_ALLOW_BESIDE_DEB=1 overrides, for the
# case where you know you are testing a build against an installed package.
if [ "${GONEDRIVE_ALLOW_BESIDE_DEB:-0}" != 1 ] \
    && dpkg-query -W -f='${Status}' gonedrive 2>/dev/null | grep -q "^install ok installed$"; then
    cat >&2 <<'MSG'
The gonedrive .deb is installed on this system.

Installing from source beside it shadows the packaged binaries and unit files,
so later package upgrades appear to do nothing. Pick one:

  sudo apt remove gonedrive      # then re-run this script, or
  GONEDRIVE_ALLOW_BESIDE_DEB=1 ./install.sh   # I know, do it anyway
MSG
    exit 1
fi

if [ "${1:-}" = "--user" ]; then
    [ "$(id -u)" = 0 ] && { echo "run the --user half as yourself, not root"; exit 1; }
    install -Dm644 data/gonedrived.service "$HOME/.config/systemd/user/gonedrived.service"
    # D-Bus activation: opening the panel starts the daemon when it is not up.
    install -Dm644 data/org.gonedrive.Daemon.service \
        "$HOME/.local/share/dbus-1/services/org.gonedrive.Daemon.service"
    ./install-desktop-integration.sh
    systemctl --user daemon-reload
    systemctl --user enable gonedrived.service
    systemctl --user restart gonedrived.service
    echo
    echo "User setup done. Sign in with: gonedrived login"
    echo "Then restart the service:      systemctl --user restart gonedrived"
    exit 0
fi

[ "$(id -u)" = 0 ] || { echo "system install needs root: sudo ./install.sh"; exit 1; }
for f in target/release/gonedrived target/release/gonedrive target/release/gonedrive-hsm; do
    [ -x "$f" ] || { echo "missing $f — build first: cargo build --release"; exit 1; }
done

install -Dm755 target/release/gonedrived  /usr/local/bin/gonedrived
install -Dm755 target/release/gonedrive   /usr/local/bin/gonedrive
install -Dm755 target/release/gonedrive-hsm /usr/local/bin/gonedrive-hsm
install -Dm644 data/gonedrive-hsm.service /etc/systemd/system/gonedrive-hsm.service
install -Dm644 data/org.gonedrive.Gonedrive.desktop /usr/local/share/applications/org.gonedrive.Gonedrive.desktop

systemctl daemon-reload
systemctl enable gonedrive-hsm.service
# `enable --now` starts a stopped service but leaves a RUNNING one on its old
# binary — an upgrade that silently doesn't upgrade. Restart is unconditional.
systemctl restart gonedrive-hsm.service
systemctl --no-pager --lines 0 status gonedrive-hsm.service | head -3

echo
echo "System install done. Now, as your normal user:"
echo "  ./install.sh --user"
