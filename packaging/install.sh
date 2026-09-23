#!/usr/bin/env bash
# Installs Clearview for the current user: the application, its icons and
# launcher entry, and the background collector that records history.
#
# Everything lands under $HOME — nothing here needs root.
set -euo pipefail
cd "$(dirname "$0")/.."

PREFIX="${PREFIX:-$HOME/.local}"
ICONS="$PREFIX/share/icons/hicolor"

if [ ! -x target/release/clearview ]; then
  echo "Build it first:  cargo build --release" >&2
  exit 1
fi

install -Dm755 target/release/clearview             "$PREFIX/bin/clearview"
install -Dm755 target/release/clearview-collector   "$PREFIX/bin/clearview-collector"
install -Dm644 packaging/clearview.desktop          "$PREFIX/share/applications/clearview.desktop"
install -Dm644 packaging/icons/clearview.svg        "$ICONS/scalable/apps/clearview.svg"

# Raster sizes as well as the scalable one: panels and some launchers still
# prefer a pixel-exact icon over scaling an SVG down.
for size in 16 22 24 32 48 64 128 256 512; do
  if command -v rsvg-convert >/dev/null 2>&1; then
    tmp="$(mktemp)"
    rsvg-convert -w "$size" -h "$size" packaging/icons/clearview.svg -o "$tmp"
    install -Dm644 "$tmp" "$ICONS/${size}x${size}/apps/clearview.png"
    rm -f "$tmp"
  fi
done

# Without this the launcher keeps showing the old (missing) icon until it is
# restarted.
command -v gtk-update-icon-cache >/dev/null 2>&1 && \
  gtk-update-icon-cache -qtf "$ICONS" 2>/dev/null || true
command -v update-desktop-database >/dev/null 2>&1 && \
  update-desktop-database -q "$PREFIX/share/applications" 2>/dev/null || true

install -Dm644 packaging/clearview-collector.service \
  "$HOME/.config/systemd/user/clearview-collector.service"
systemctl --user daemon-reload
systemctl --user enable --now clearview-collector

echo "Installed. 'clearview' is on your PATH and in your application menu."
