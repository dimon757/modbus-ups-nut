#!/usr/bin/env bash
set -e

DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT_DIR="$(dirname "$DIR")"

MODE="${1:-}"

usage() {
    echo "Usage: $0 [1win|3ep|status]"
    echo ""
    echo "Modes:"
    echo "  1win    Switch bridge.toml to 1 Windows 11 endpoint (no Proxmox)"
    echo "  3ep     Switch bridge.toml to 2 Windows 11 + 1 Proxmox endpoints (vms_then_poweroff)"
    echo "  status  Show current active configuration in bridge.toml"
    exit 1
}

case "$MODE" in
    1win|single|1)
        cp "$DIR/bridge-1win.toml" "$DIR/bridge.toml"
        cp "$ROOT_DIR/bridge-1win.toml" "$ROOT_DIR/bridge.toml" 2>/dev/null || true
        echo "Switched to: 1 Windows 11 endpoint (no Proxmox)"
        ;;
    3ep|default|3)
        cp "$DIR/bridge-3endpoints.toml" "$DIR/bridge.toml"
        cp "$ROOT_DIR/bridge-3endpoints.toml" "$ROOT_DIR/bridge.toml" 2>/dev/null || true
        echo "Switched to: 2 Windows 11 endpoints + 1 Proxmox (vms_then_poweroff)"
        ;;
    status)
        echo "Current test/bridge.toml endpoints:"
        grep -E 'name =|kind =|method =' "$DIR/bridge.toml"
        ;;
    *)
        usage
        ;;
esac
