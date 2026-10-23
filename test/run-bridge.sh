#!/bin/sh
# Builds and runs the real bridge against the level-2 test setup: test config,
# fake ssh first on PATH, debug logging. Run ./setup.sh first.

set -eu
HERE=$(cd "$(dirname "$0")" && pwd)
REPO=$(dirname "$HERE")

[ -e /tmp/mub-test/ttyBR ] || { echo "no virtual serial cable -- run ./setup.sh first"; exit 1; }

# ./run-bridge.sh          -> bridge.toml (active switched config; default vms_then_poweroff)
# ./run-bridge.sh vms      -> bridge-test-vms.toml (Proxmox method vms_then_poweroff)
# ./run-bridge.sh poweroff -> bridge-test.toml     (Proxmox method poweroff)
# ./run-bridge.sh 1win     -> bridge-1win.toml     (1 Windows 11 endpoint, no Proxmox)
case "${1:-}" in
    "") CONFIG="$HERE/bridge.toml" ;;
    vms) CONFIG="$HERE/bridge-test-vms.toml" ;;
    poweroff) CONFIG="$HERE/bridge-test.toml" ;;
    1win) CONFIG="$HERE/bridge-1win.toml" ;;
    *) echo "usage: $0 [vms|poweroff|1win]"; exit 1 ;;
esac

cargo build --quiet --manifest-path "$REPO/Cargo.toml"
PATH="$HERE/bin:$PATH" RUST_LOG="${RUST_LOG:-modbus_ups_bridge=debug,info}" \
    exec "$REPO/target/debug/modbus-ups-bridge" "$CONFIG"
