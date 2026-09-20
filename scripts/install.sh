#!/usr/bin/env bash
# kuadrat install / upgrade / uninstall.
#
# The one entry point for going from a built binary (or a release artifact)
# to a running systemd service. Idempotent by construction: re-running with a
# newer binary in place IS the upgrade — the unit is rewritten, daemon-reload
# runs, and the service restarts onto the new binary.
#
#   sudo bash scripts/install.sh                        # from this checkout (builds if needed)
#   sudo bash scripts/install.sh /path/to/kuadrat-v0.2.0 \
#                              /path/to/kuadrat.service  # from a release artifact
#   sudo bash scripts/install.sh --uninstall            # remove binary, unit, and service
#
# Ships `packaging/kuadrat.service` rather than installing it from the build
# (docs/design/2026-08-11-phase-3-h7-serve.md: "The unit is shipped and
# documented, not installed by the build") — this script is that ship.
set -uo pipefail

PREFIX_BIN=/usr/local/bin
UNIT_DEST=/etc/systemd/system/kuadrat.service
REPO=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
BIN_DEFAULT="$REPO/target/release/kuadrat"
UNIT_DEFAULT="$REPO/packaging/kuadrat.service"

[ "$(id -u)" -eq 0 ] || { echo "FATAL: run as root (sudo)"; exit 1; }
command -v systemctl >/dev/null 2>&1 || { echo "FATAL: systemctl not found — kuadrat needs systemd (cgroups v2)"; exit 1; }
command -v podman >/dev/null 2>&1 || echo "WARN: podman not on PATH — apps will not deploy until it is installed"

if [ "${1:-}" = "--uninstall" ]; then
  systemctl disable --now kuadrat >/dev/null 2>&1 || true
  rm -f "$UNIT_DEST" "$PREFIX_BIN/kuadrat"
  systemctl daemon-reload
  echo "kuadrat uninstalled: binary, unit, and service removed."
  exit 0
fi

BIN="${1:-$BIN_DEFAULT}"
UNIT="${2:-$UNIT_DEFAULT}"

if [ ! -x "$BIN" ]; then
  echo "no prebuilt binary at $BIN — building the release binary…"
  (cd "$REPO" && PATH="$HOME/.cargo/bin:$PATH" cargo build --release >/dev/null) \
    || { echo "FATAL: cargo build --release failed"; exit 1; }
  BIN="$BIN_DEFAULT"
fi
[ -f "$UNIT" ] || { echo "FATAL: unit file not found at $UNIT"; exit 1; }

# Sanity: it must answer `--version` and name itself kuadrat — an artifact
# download that picked up the wrong file fails here, not on first boot.
"$BIN" --version | grep -q kuadrat || { echo "FATAL: $BIN is not a kuadrat binary"; exit 1; }

install -D -m755 "$BIN" "$PREFIX_BIN/kuadrat"
install -D -m644 "$UNIT" "$UNIT_DEST"
systemctl daemon-reload
systemctl enable kuadrat >/dev/null 2>&1
systemctl restart kuadrat
systemctl --no-pager -l status kuadrat | head -4

VERSION=$("$PREFIX_BIN/kuadrat" --version)
echo "kuadrat installed: $PREFIX_BIN/kuadrat ($VERSION), service active."