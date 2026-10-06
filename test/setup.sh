#!/bin/sh
# Level-2 test environment for modbus-ups-bridge. See CHECKLIST.md.
#
#   ./setup.sh          prepare everything and start the virtual serial cable
#   ./setup.sh reset    clear logs, marker and simulated failures between scenarios
#   ./setup.sh stop     stop the virtual serial cable
#   ./setup.sh clean    stop and delete /tmp/mub-test entirely
#
# Needs: socat, python3 with venv (Debian: apt install socat python3-venv).
# Never needs root, and never touches the real service's files.

set -eu
HERE=$(cd "$(dirname "$0")" && pwd)
DIR=/tmp/mub-test

# Fake Proxmox hosts (see test/pve-bin/): proxmox-a with two VMs, proxmox-b with
# one, all shutting down cleanly a few seconds after the request. Scenarios
# N change a host's "vms" file to add hung or guest-agent-less VMs.
reset_proxmox() {
    rm -rf "$DIR/proxmox"
    mkdir -p "$DIR/proxmox/10.99.0.3" "$DIR/proxmox/10.99.0.4"
    printf '100|dc01|ok:3
101|app server|ok:6
' > "$DIR/proxmox/10.99.0.3/vms"
    printf '102|db01|ok:4
' > "$DIR/proxmox/10.99.0.4/vms"
    for id in 100 101; do echo on > "$DIR/proxmox/10.99.0.3/state_$id"; done
    echo on > "$DIR/proxmox/10.99.0.4/state_102"
}

# Fake ESXi hosts (legacy / backward compatibility)
reset_esxi() {
    rm -rf "$DIR/esxi"
    mkdir -p "$DIR/esxi/10.99.0.3" "$DIR/esxi/10.99.0.4"
    printf '1|dc01|ok:3
2|app server|ok:6
' > "$DIR/esxi/10.99.0.3/vms"
    printf '3|db01|ok:4
' > "$DIR/esxi/10.99.0.4/vms"
    for id in 1 2; do echo on > "$DIR/esxi/10.99.0.3/state_$id"; done
    echo on > "$DIR/esxi/10.99.0.4/state_3"
}

reset() {
    : > "$DIR/ssh.log"
    rm -f "$DIR/ssh-fail" "$DIR/ssh-hang" "$DIR/shutdown_fired"
    reset_proxmox
    reset_esxi
}

stop_socat() {
    if [ -f "$DIR/socat.pid" ]; then
        kill "$(cat "$DIR/socat.pid")" 2>/dev/null || true
        rm -f "$DIR/socat.pid"
    fi
    pkill -f "link=$DIR/ttyINV" 2>/dev/null || true
}

# socat exits when either end of the cable is closed (e.g. the simulator is
# stopped in scenario G), so keep restarting it. The link names stay the
# same; the bridge and simulator just reopen them.
#
# Runs as a separate `sh -c` process, fully detached from our stdin/stdout.
# A backgrounded shell function is not enough: dash keeps hidden copies of
# the caller's stdout in it, so `./setup.sh | tee log` would never finish.
start_cable() {
    DIR="$DIR" sh -c '
        while :; do
            socat pty,raw,echo=0,link="$DIR/ttyINV" pty,raw,echo=0,link="$DIR/ttyBR" 2>>"$DIR/socat.log"
            sleep 0.2
        done' < /dev/null > /dev/null 2>&1 &
    echo $! > "$DIR/socat.pid"
}

case "${1:-start}" in
start)
    command -v socat >/dev/null || { echo "socat missing: sudo apt install socat"; exit 1; }
    command -v python3 >/dev/null || { echo "python3 missing: sudo apt install python3"; exit 1; }
    mkdir -p "$DIR"

    if [ ! -x "$DIR/venv/bin/python" ]; then
        echo "creating Python venv with pinned pymodbus..."
        python3 -m venv "$DIR/venv" || { echo "venv failed: sudo apt install python3-venv"; exit 1; }
        "$DIR/venv/bin/pip" install -q -r "$HERE/requirements.txt"
    fi
    echo "checking the simulator's register map..."
    (cd "$HERE" && "$DIR/venv/bin/python" self_check.py > "$DIR/self_check.log" 2>&1) \
        || { cat "$DIR/self_check.log"; echo "simulator self-check FAILED"; exit 1; }

    chmod +x "$HERE/bin/ssh" "$HERE/run-bridge.sh" "$HERE/pve-bin/qm" "$HERE/pve-bin/systemctl" "$HERE/esxi-bin/vim-cmd" "$HERE/esxi-bin/esxcli" 2>/dev/null || true
    # Dummy SSH key for the test endpoints: the fake ssh never reads it, but
    # the bridge checks at startup that key files exist and are 0600.
    echo "dummy key for level-2 tests" > "$DIR/fake_key"
    chmod 600 "$DIR/fake_key"
    # Host keys are pinned, and the bridge checks at startup that every
    # endpoint is in known_hosts: give the four test hosts a (throwaway) key.
    rm -f "$DIR/fake_hostkey" "$DIR/fake_hostkey.pub"
    ssh-keygen -q -t ed25519 -N '' -f "$DIR/fake_hostkey"
    : > "$DIR/known_hosts"
    for h in 10.99.0.1 10.99.0.2 10.99.0.3 10.99.0.4; do
        echo "$h $(cut -d' ' -f1,2 "$DIR/fake_hostkey.pub")" >> "$DIR/known_hosts"
    done
    reset
    stop_socat
    : > "$DIR/socat.log"
    start_cable
    i=0
    while [ ! -e "$DIR/ttyBR" ] && [ $i -lt 50 ]; do sleep 0.1; i=$((i + 1)); done
    [ -e "$DIR/ttyBR" ] || { cat "$DIR/socat.log"; echo "socat did not start"; exit 1; }

    cat <<EOF

Ready. Virtual cable: $DIR/ttyINV (simulator) <-> $DIR/ttyBR (bridge)

Open four terminals in $HERE:
  1. inverter:  $DIR/venv/bin/python inverter_sim.py --serial $DIR/ttyINV
  2. WOL:       python3 wol_listen.py
  3. ssh log:   tail -f $DIR/ssh.log
  4. bridge:    ./run-bridge.sh           (Proxmox method vms_then_poweroff)
            or ./run-bridge.sh poweroff (Proxmox method poweroff)

Then work through CHECKLIST.md. Between scenarios: ./setup.sh reset
EOF
    ;;
reset)
    reset
    echo "cleared ssh.log, simulated failures, the test marker and the fake Proxmox hosts"
    ;;
stop)
    stop_socat
    echo "virtual serial cable stopped"
    ;;
clean)
    stop_socat
    rm -rf "$DIR"
    echo "removed $DIR"
    ;;
*)
    echo "usage: $0 [start|reset|stop|clean]"
    exit 1
    ;;
esac
