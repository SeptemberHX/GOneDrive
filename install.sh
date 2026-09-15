#!/bin/bash
# GOnedrive installer.
#
#   sudo ./install.sh          # binaries + privileged helper service
#   ./install.sh --user        # per-user pieces (daemon unit, Nautilus, icons)
#
# Split on purpose: only the helper needs root, and mixing the two under sudo
# would litter root-owned files through the user's home.
set -euo pipefail
cd "$(dirname "$0")"

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
