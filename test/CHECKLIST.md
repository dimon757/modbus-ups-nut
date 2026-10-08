# Level-2 test checklist

This runs the real bridge binary end to end, with its real clock, serial port,
Modbus traffic, `ssh` calls, Wake-on-LAN and marker file. Only the outside
world is simulated:

| Real thing | Replaced by |
|---|---|
| Sunsynk inverter on RS485 | `inverter_sim.py` on a `socat` virtual serial cable |
| SSH to the 3 machines | `bin/ssh`, which writes to `/tmp/mub-test/ssh.log` -- and, for the Proxmox host, runs the bridge's `qm`/`systemctl` commands against simulated VMs (scenarios N) |
| Wake-on-LAN on the site LAN | packets to `127.0.0.1:40009`, shown by `wol_listen.py` |
| Real waits (60 s / 180 s / 30 s / 2 min / 300 s) | 5 s / 10 s / 2 s / 5 s / 5 s (`bridge-test.toml`) |
| `/var/lib/modbus-ups-bridge/shutdown_fired` | `/tmp/mub-test/shutdown_fired` |

Nothing here needs root, touches the network, or can shut down or reboot
anything. It's safe on the N2840 itself -- but stop the real service first
(`sudo systemctl stop modbus-ups-bridge`) so the two don't fight over the
log and a serial port.

## Setup (once per session)

Needs any Linux machine with `socat`, `python3`, `python3-venv` and the Rust
toolchain (`sudo apt install socat python3-venv cargo` on Debian 13).

```bash
cd test
./setup.sh
```

It creates a Python venv with the pinned pymodbus, checks the simulator's
register map (`self_check.py`), starts the virtual serial cable and prints
the four commands to run, one per terminal:

1. **inverter**: `/tmp/mub-test/venv/bin/python inverter_sim.py --serial /tmp/mub-test/ttyINV`
   -- type commands at the `sim>` prompt (`help` lists them)
2. **Wake-on-LAN**: `python3 wol_listen.py`
3. **ssh log**: `tail -f /tmp/mub-test/ssh.log`
4. **bridge**: `./run-bridge.sh` -- debug log, one line per poll (every 1 s)

Between scenarios, `./setup.sh reset` clears the ssh log, the marker and any
simulated SSH failures. When done: `./setup.sh clean`.

Timings below assume the test config: grid-lost wait **5 s**, recovery wait
**10 s**, **2 s** between endpoints, Wake-on-LAN **4 rounds 5 s apart**.

---

## A. Startup and normal running

**Do:** start the simulator, then the bridge. Change nothing.

**Expect** in the bridge log:
- [ ] `loaded config from .../bridge-test.toml (3 endpoint(s))`
- [ ] `inverter: device type 0x0300, battery mode 1, cutoff 20% / 46.00 V`
- [ ] `inverter: protocol version (reg 2) 0x0102 (1.2), reg 54 0 -- see docs/protocol-versions.md`
- [ ] `inverter settings: inverter cutoff 20% SOC, shutdown sequence at 30% -- 10 points of margin`
- [ ] every second: `soc=80.0% grid=230.0V relay=closed load=500W batt=0W on_battery=false low_battery=false`
- [ ] no state changes, nothing in ssh.log, nothing in the WOL listener

## B. Short grid blip -- ignored

**Do:** `outage`, then `restore` within 3 s.

**Expect:**
- [ ] `state: Idle -> GridLostDebouncing`, then `state: GridLostDebouncing -> Idle`
- [ ] never `OnBattery`; nothing in ssh.log or WOL

## C. Outage that never reaches low SOC -- no shutdown, no wake-up

**Do:** `outage`; wait for `-> OnBattery` (5 s); `soc 50`; `restore`; wait 10 s.

**Expect:**
- [ ] `GridLostDebouncing -> OnBattery` about 5 s after `outage`
- [ ] after `restore`: `OnBattery -> RecoveryDebouncing`, 10 s later `RecoveryDebouncing -> Idle`
- [ ] **no** `Wake-on-LAN round` lines, nothing in the WOL listener, nothing in ssh.log

## D. Full outage: shutdown, latch, recovery, wake-up

**D1. Shutdown fires.** `./setup.sh reset`, then `outage`; wait for `OnBattery`; then `soc 25`.
- [ ] about 1 s after `soc 25` (the second low reading -- one alone is never enough):
      `SOC 25.0% <= threshold 30.0% (2 of the last 3 readings) while on battery -- firing shutdown sequence`
- [ ] `/tmp/mub-test/shutdown_fired` exists (it is written before any ssh call)
- [ ] `shutdown sequence starting: 3 endpoint(s)`
- [ ] ssh.log gets 3 lines, **2 s apart**, in config order:
  - `ups-shutdown@10.99.0.1: shutdown /s /t 60 /c "Inverter battery low, ..."`
  - `ups-shutdown@10.99.0.2: shutdown /s /t 60 ...`
  - `root@10.99.0.3: nohup sh -c 'sleep 10; /sbin/poweroff' > /tmp/ups-shutdown.log 2>&1 < /dev/null &`
- [ ] `shutdown sequence complete`

**D2. Latched -- no second shutdown.** `soc 22`, then `soc 21`.
- [ ] ssh.log does **not** grow; `low_battery=true` in the poll lines

**D3. Grid back -- wake-up, even though the battery is still low.** `restore` (SOC still 21); wait 10 s.
- [ ] `ShutdownLatched -> RecoveryDebouncing` right away, 10 s later `RecoveryDebouncing -> Idle`
- [ ] `Wake-on-LAN round 1/4` ... `round 4/4`, 5 s apart
- [ ] WOL listener: 4 rounds of 3 packets (`AA:BB:CC:00:00:01` ... `:03`), all `ok`
- [ ] `/tmp/mub-test/shutdown_fired` disappears only **after** round 4

## E. Grid flickers back, then drops with SOC already low (regression)

This is the bug found in the original code: the shutdown used to be skipped here.

**Do:** `./setup.sh reset`; `soc 35`; `outage`; wait for `OnBattery`; then quickly (all within 10 s): `restore`, `soc 30`, `outage`.

**Expect:**
- [ ] `OnBattery -> RecoveryDebouncing` after `restore`
- [ ] on the second `outage`: `firing shutdown sequence` and `RecoveryDebouncing -> ShutdownLatched`
- [ ] 3 lines in ssh.log

## F. Bridge restarts mid-outage -- remembers the shutdown

**Do:** `./setup.sh reset`; run D1 (outage, `soc 25`, 3 ssh lines). Stop the bridge (Ctrl+C) and start it again with `./run-bridge.sh`.

**Expect:**
- [ ] `/tmp/mub-test/shutdown_fired exists: a previous run shut the endpoints down and never finished waking them -- resuming latched, Wake-on-LAN will follow recovery`
- [ ] **no** new ssh.log lines (no second shutdown)
- [ ] then `restore` + `soc 40`: after 10 s, 4 Wake-on-LAN rounds and the marker is removed

## F2. Bridge restarts mid-sequence -- resumes remaining endpoints

Simulates a bridge reboot, crash, or watchdog reset after the first two endpoints were dispatched.

**Do:** `./setup.sh reset`. Write an incomplete manifest:
```bash
printf '# shutdown sequence in progress\ndispatched: ws-1\ndispatched: ws-2\n' > /tmp/mub-test/shutdown_fired
```
Set simulator to outage (`outage`, `soc 25`), then start the bridge (`./run-bridge.sh`).

**Expect:**
- [ ] `/tmp/mub-test/shutdown_fired indicates incomplete shutdown: 2 endpoint(s) already dispatched, 1 remaining: ["proxmox"]`
- [ ] `grid still down: resuming shutdown sequence for 1 remaining endpoint(s)`
- [ ] `ssh.log` gets shutdown calls **only** for `proxmox` (10.99.0.3) -- no re-dispatching `ws-1` or `ws-2`
- [ ] `shutdown sequence complete` and `/tmp/mub-test/shutdown_fired` is updated with `completed`
- [ ] then `restore` + `soc 40`: after 10 s, 4 Wake-on-LAN rounds wake all endpoints and the marker is deleted

## F3. Bridge restarts mid-sequence during grid voltage flicker

Simulates a restart when the grid flickers back for just one reading during an outage. The bridge must not discard the remaining sequence.

**Do:** `./setup.sh reset`. Write an incomplete manifest:
```bash
printf '# shutdown sequence in progress\ndispatched: ws-1\ndispatched: ws-2\n' > /tmp/mub-test/shutdown_fired
```
Set simulator to grid restored (`restore`, `soc 25`), then start the bridge (`./run-bridge.sh`). After 2 seconds, simulate outage returning (`outage`).

**Expect:**
- [ ] `/tmp/mub-test/shutdown_fired indicates incomplete shutdown: 2 endpoint(s) already dispatched, 1 remaining: ["proxmox"]`
- [ ] `grid currently up; holding 1 remaining shutdown(s) pending recovery confirmation`
- [ ] on `outage`: `grid still down: resuming shutdown sequence for 1 remaining endpoint(s)`
- [ ] `ssh.log` receives shutdown commands only for `proxmox`
- [ ] `shutdown sequence complete` and marker file marked `completed`
- [ ] then `restore` + `soc 40`: after 10 s, Wake-on-LAN rounds sent and marker deleted

## F4. Failed endpoint retried & Proxmox timing

Simulates an endpoint failing (or a bridge restart during long Proxmox VM shutdowns): failed endpoints and mid-flight VMs must not be ticked off in the manifest.

**Do:** `./setup.sh reset`. Add failure for ws-2: `echo 10.99.0.2 > /tmp/mub-test/ssh-fail`. Start bridge with VM method: `./run-bridge.sh vms`. Trigger outage: `outage`, `soc 25`. While Proxmox VMs are stopping, inspect `/tmp/mub-test/shutdown_fired`. Stop bridge (Ctrl+C). Remove failure: `rm /tmp/mub-test/ssh-fail`. Restart bridge: `./run-bridge.sh vms`.

**Expect:**
- [ ] `shutting down ws-1` succeeds; `failed to shut down ws-2` logged
- [ ] during Proxmox VM shutdown, `/tmp/mub-test/shutdown_fired` contains `dispatched: ws-1`, but neither `ws-2` nor `proxmox`
- [ ] on restart: `indicates incomplete shutdown: 1 endpoint(s) already dispatched, 2 remaining: ["ws-2", "proxmox"]`
- [ ] `ws-2` is retried and accepted
- [ ] `proxmox` completes VM shutdown and host poweroff
- [ ] `shutdown sequence complete` and marker file marked `completed`

## F5. Failed endpoint retried after sequence finishes (completed withheld)

Simulates an endpoint that fails permanently or exhausts its retry budget while the rest of the sequence completes to the end. The bridge must withhold `completed` so that any subsequent restart (e.g. minutes or hours later during an ongoing outage) still identifies the failed endpoint as incomplete and retries it, rather than skipping it.

**Do:** `./setup.sh reset`. Add failure for ws-2: `echo 10.99.0.2 > /tmp/mub-test/ssh-fail`. Start bridge: `./run-bridge.sh`. Trigger outage: `outage`, `soc 25`. Wait for the full shutdown sequence to finish (`shutdown sequence complete`). Inspect `/tmp/mub-test/shutdown_fired`: verify `completed` is absent and `ws-2` is not dispatched. Stop bridge (Ctrl+C). Remove failure: `rm /tmp/mub-test/ssh-fail`. Restart bridge: `./run-bridge.sh`.

**Expect:**
- [ ] during initial outage, ws-1, proxmox succeed; ws-2 exhausts retry budget and fails
- [ ] bridge logs: `shutdown sequence finished: 2/3 endpoint(s) succeeded; marker left incomplete for retry on restart`
- [ ] `/tmp/mub-test/shutdown_fired` contains `dispatched` for ws-1, proxmox, but NOT `completed`
- [ ] on restart: `indicates incomplete shutdown: 2 endpoint(s) already dispatched, 1 remaining: ["ws-2"]`
- [ ] `resuming shutdown sequence for 1 remaining endpoint(s)`
- [ ] `ws-2` is retried and succeeds: `ws-2: shutdown command accepted`
- [ ] bridge logs: `shutdown sequence complete: all 1 endpoint(s) succeeded`
- [ ] `/tmp/mub-test/shutdown_fired` now contains `completed`
- [ ] restore grid: Wake-on-LAN fires and marker file is removed

## G. Inverter goes silent -- no shutdown on missing data

**Do:** stop the simulator (Ctrl+C) for ~15 s, then start it again.

**Expect:**
- [ ] `modbus poll failed: timed out reading register 0x00b8 -- reconnecting`
- [ ] `could not read required inverter settings: timed out reading register 0x0000`
- [ ] `inverter settings unreadable (1 in a row) -- reconnecting and trying again`, then
      `inverter settings could not be read 2 times in a row -- monitoring WITHOUT a settings check ...`
- [ ] repeats; **no** state change, nothing in ssh.log
- [ ] after the simulator restarts: `inverter: device type 0x0300 ...` and normal poll lines again
      (a restarted simulator is back at grid 230 V / SOC 80 %)

## H. Garbage SOC reading -- treated as a bad read, not a low battery

**Do:** `outage`; wait for `OnBattery`; `set 184 150`; after a few seconds `soc 60`, `restore`.

**Expect:**
- [ ] `modbus poll failed: battery SOC register read 150 (valid range 0-100) -- reconnecting`
- [ ] **no** shutdown while it reads 150

## I. Inverter cutoff leaves no margin -- logged as an error

**Do:** `set 217 30`, then restart the bridge. Afterwards: `set 217 20`.

**Expect:**
- [ ] ERROR `inverter settings: inverter cuts its output at 30% SOC, but low_battery_soc is 30% -- ...`
- [ ] ERROR `inverter settings: config inverter_cutoff_soc is 20% but the inverter is set to 30% -- ...`
- [ ] ERROR `inverter settings have errors (see above) -- monitoring continues anyway; ...`
- [ ] the bridge keeps running and polling (with `strict_inverter_checks = true` it would refuse: see T)

## J. Inverter in voltage mode -- warning

**Do:** `set 213 0`, restart the bridge. Afterwards: `set 213 1`.

**Expect:**
- [ ] WARN `inverter settings: inverter manages the battery by VOLTAGE: it cuts output at 46.00 V ...`

## K. New outage during the Wake-on-LAN rounds -- rounds cancelled

**Do:** `./setup.sh reset`; run D1, then `restore` + `soc 40`. As soon as `Wake-on-LAN round 1/4` appears: `outage`, wait 5 s, `soc 25`.

**Expect:**
- [ ] `cancelling pending Wake-on-LAN resends` when the shutdown fires -- no rounds after
      that line (one more round may still arrive during the 5 s grid-lost wait)
- [ ] a new shutdown fires (4 more ssh.log lines); `/tmp/mub-test/shutdown_fired` exists again

## L. One endpoint unreachable -- the rest still shut down

**Do:** `./setup.sh reset`; `echo 10.99.0.3 > /tmp/mub-test/ssh-fail`; run D1.

**Expect:**
- [ ] `failed to shut down proxmox-a: ssh to 10.99.0.3 exited Some(255): stderr=ssh: connect to host 10.99.0.3 port 22: Connection timed out (simulated)`
- [ ] ssh.log: `root@10.99.0.3 FAILED (simulated)`, and proxmox-b is still shut down 2 s later

## M. One endpoint hangs, and the grid returns mid-sequence

**Do:** `./setup.sh reset`; `echo 10.99.0.2 > /tmp/mub-test/ssh-hang`; run D1. As soon as ws-2's `HANGING` line appears in ssh.log: `restore`, `soc 40`.

**Expect:**
- [ ] the bridge keeps polling every second while ws-2 hangs (it isn't blocked)
- [ ] ~10 s after `restore`: `recovery confirmed -- stopping the rest of the shutdown sequence`
- [ ] proxmox-a and proxmox-b are **never** contacted (no ssh.log lines for them)
- [ ] when ws-2's hung SSH call reaches its 60 s timeout: `recovery confirmed -- not starting the remaining endpoints`
      (machines already being shut down are left to finish; only new ones are held back)
- [ ] Wake-on-LAN rounds follow as in D3

To see the 60 s SSH timeout itself, leave the grid down instead: after 60 s,
`failed to shut down ws-2: ssh to 10.99.0.2 timed out after 60s`, and the
sequence carries on with proxmox-a.

---

---

## N. Proxmox method `vms_then_poweroff`

These scenarios use the second Proxmox method. Stop the bridge and start it
with the other test config (`[proxmox] method = "vms_then_poweroff"`,
`vm_shutdown_timeout_secs = 15`):

```bash
./run-bridge.sh vms
```

The fake ssh now **executes** the bridge's commands for proxmox (10.99.0.3)
against simulated VMs (`test/pve-bin/`): after
`./setup.sh reset`, proxmox has `dc01` (VM 100, shuts down 3 s after the request) and
`app server` (VM 101, 6 s). Each simulated VM is a line in
`/tmp/mub-test/proxmox/<host>/vms` (`<id>|<name>|<behaviour>`); its state is in
`state_<id>`, and `host` appears once the host has been powered off.
In ssh.log, each executed command ends with `-> exit <code>`.

### N1. VMs shut down, then systemctl poweroff

**Do:** `./setup.sh reset`; `outage`; wait for `OnBattery`; `soc 25`.

- [ ] `shutting down proxmox (10.99.0.3) via Proxmox: VMs first, then poweroff`
- [ ] `proxmox: 2 VM(s) registered, 2 running: dc01, app server`, then `guest shutdown requested for VM ...` for each
- [ ] `proxmox: VM dc01 is off`, `VM app server is off`
- [ ] `proxmox: host power-off scheduled via systemctl poweroff`
- [ ] `shutdown sequence complete`
- [ ] `cat /tmp/mub-test/proxmox/10.99.0.3/host` shows `poweroff via systemctl`

### N2. A hung VM and a VM without QEMU Guest Agent

**Do:** `./setup.sh reset`, then add a VM that never shuts down and one without the guest agent to proxmox:

```bash
echo "109|stuck-vm|hang" >> /tmp/mub-test/proxmox/10.99.0.3/vms; echo on > /tmp/mub-test/proxmox/10.99.0.3/state_109
echo "108|no-tools-vm|notools" >> /tmp/mub-test/proxmox/10.99.0.3/vms; echo on > /tmp/mub-test/proxmox/10.99.0.3/state_108
```

then `outage`, wait for `OnBattery`, `soc 25`.

- [ ] `proxmox: guest shutdown of VM no-tools-vm failed ... -- will be powered off`
- [ ] the other VMs go off normally
- [ ] as soon as the other VMs are off:
      `proxmox: VM no-tools-vm could not be shut down gracefully -- powering it off hard`
- [ ] 15 s after the requests: `proxmox: VM stuck-vm still running after 15 s -- powering it off hard`
- [ ] host is powered off (`host power-off scheduled via systemctl poweroff`) -- a stuck VM never keeps a host running into the inverter's cutoff

### N3. systemctl poweroff refused (fallback to /sbin/poweroff)

**Do:** `./setup.sh reset`; `touch /tmp/mub-test/proxmox/10.99.0.3/systemctl-refuse`; `outage`; wait for `OnBattery`; `soc 25`.

- [ ] `proxmox: systemctl poweroff refused (...) -- falling back to /sbin/poweroff`
- [ ] `proxmox: host power-off scheduled via /sbin/poweroff (10 s)`
- [ ] `cat /tmp/mub-test/proxmox/10.99.0.3/host` shows `poweroff via /sbin/poweroff`

### N4. Grid back while a host is still shutting its VMs down (Safety Verification & VM Restart)

**Do:** `./setup.sh reset`; add the hung VM to proxmox as in N2 (first line only); `outage`;
wait for `OnBattery`; `soc 25`; about 2 s later `restore` and `soc 40`.

- [ ] ~10 s after `restore`: `recovery confirmed -- stopping the rest of the shutdown sequence`
      and `recovery confirmed while VM shutdowns were in progress -- aborting local waits; no hard stop or host poweroff will be issued`
- [ ] Wake-on-LAN rounds start (`Wake-on-LAN round 1/4`)
- [ ] proxmox **aborts destructive steps**: `qm stop` (hard VM power-off) is **never** executed
- [ ] host power-off (`systemctl poweroff` or `/sbin/poweroff`) is **never** executed -- the host stays running and running guests are not killed on power restoration
- [ ] **stopped VMs are restarted**: `restarting ... stopped VM(s)` logged and `qm start` executed for VMs that stopped before recovery, bringing them back to running without requiring a host power cycle

### N5. One Proxmox host unreachable

**Do:** `./setup.sh reset`; `echo 10.99.0.3 > /tmp/mub-test/ssh-fail`; `outage`; wait for `OnBattery`; `soc 25`.

- [ ] `failed to shut down proxmox: listing VMs: ssh to 10.99.0.3 exited Some(255): ...`
- [ ] ws-1 and ws-2 completed while proxmox failed gracefully

---

## O. Host booting on wakeup (SSH connection retry without head-of-line blocking)

Simulates an endpoint that is still booting up from a previous Wake-on-LAN round when a new outage fires. Its SSH daemon is not yet ready, returning `Connection refused`. The bridge delegates it to a background retry task (retrying every 2 s up to `ssh_connect_retry_secs`) and immediately advances to the next endpoint without delaying it (`need_stagger = false`). Once sshd becomes available, the background retry succeeds.

**Do:** `./setup.sh reset`; `echo 2 > /tmp/mub-test/ssh-booting-10.99.0.1`; `outage`; wait for `OnBattery`; `soc 25`.

**Expect:**
- [ ] `ws-1: SSH connection failed ... -- host may still be booting; continuing retries in background (5s retry budget remaining)`
- [ ] `ws-2` is dispatched immediately without waiting for `ws-1`
- [ ] `ws-1` background retries until the `ssh-booting` counter is exhausted, then logs `ws-1: shutdown command accepted (after background retry)`
- [ ] sequence proceeds through `proxmox`
- [ ] `shutdown sequence complete` with all endpoints recorded in marker file
- [ ] `restore`, `soc 80`: Wake-on-LAN fires on recovery and clears marker

---

## P. Server booting with extended per-endpoint retry budget

Simulates a heavy server or Proxmox host that takes longer to boot than standard workstations. The host has an explicit per-endpoint `ssh_connect_retry_secs = 12` configured (overriding global 6 s). While it retries in the background, subsequent endpoints proceed immediately, and the server successfully shuts down once sshd finishes booting within its extended window.

**Do:** `./setup.sh reset`; `echo 3 > /tmp/mub-test/ssh-booting-10.99.0.3`; `outage`; wait for `OnBattery`; `soc 25`.

**Expect:**
- [ ] `proxmox` initial connect returns transient failure; logs background retry with extended budget: `continuing retries in background (11s retry budget remaining)`
- [ ] `proxmox` background task succeeds within its 12s budget: `proxmox: shutdown command accepted (after background retry)`
- [ ] all 3 endpoints successfully complete and are recorded in the marker file
- [ ] sequence finishes with `shutdown sequence complete`

---

## Q. Grid back while a slow VM is still shutting down -- it is restarted too

**Do:** `./setup.sh reset`; add a slow VM to the fake host:
`echo "110|slow-vm|ok:14" >> /tmp/mub-test/proxmox/10.99.0.3/vms; echo on > /tmp/mub-test/proxmox/10.99.0.3/state_110`;
start `./run-bridge.sh vms`; `outage`; wait for `OnBattery`; `soc 25`; one second after
`guest shutdown requested for VM slow-vm`: `restore`, `soc 40`.

**Expect:**
- [ ] `recovery confirmed while VM shutdowns were in progress -- aborting local waits ...`
- [ ] `recovery cancelled the shutdown -- watching 3 VM(s) and restarting any that stop`
- [ ] dc01 and app server are restarted first; **slow-vm is restarted a few seconds later**, once its
      shutdown has finished (a single look at that moment would have found it still "running")
- [ ] `restart watch finished: 3 VM(s) restarted`
- [ ] `qm start 110` in ssh.log; **no** `qm stop`; the host is not powered off

## R. sshd drops the VM shutdown logins -- retried, nothing hard-stopped

**Do:** `./setup.sh reset`; `echo 2 > /tmp/mub-test/ssh-flaky-qm-shutdown-10.99.0.3`;
`./run-bridge.sh vms`; `outage`; wait for `OnBattery`; `soc 25`.

**Expect:**
- [ ] `SSH connection for `qm shutdown ...` failed (... kex_exchange_identification ...) -- retrying in 2s`
      (twice: once per dropped call)
- [ ] both VMs still shut down gracefully; `host power-off scheduled via systemctl poweroff`
- [ ] **no** `powering it off hard`, no `qm stop` in ssh.log

## S. Inverter settings problems -- default mode keeps protecting the site

Needs the simulator's `mute`/`unmute` commands. Restart the bridge between the parts.

**Do (a):** `set 0 0x0500`, start the bridge. **Expect:** `inverter data cannot be trusted (wrong device
type 0x0500, expected 0x0300) -- refusing to operate`, retried every 5 s, **no** `soc=` poll lines.
`set 0 0x0300` -> polling starts within 5 s.

**Do (b):** `set 217 15`, start the bridge, then `outage`, `soc 25`. **Expect:**
- [ ] ERROR `inverter settings: config inverter_cutoff_soc is 20% but the inverter is set to 15% -- ...`
- [ ] ERROR `... monitoring continues anyway`, and poll lines
- [ ] `firing shutdown sequence` -- the site is still protected. Afterwards `set 217 20`, `restore`.

**Do (c):** `mute 0 213 217 220`, start the bridge. **Expect:**
- [ ] `inverter settings unreadable (1 in a row) -- reconnecting and trying again`
- [ ] `... monitoring WITHOUT a settings check ...` and normal poll lines
- [ ] after `unmute`: within ~60 s `trying the inverter settings check again`, then
      `inverter: device type 0x0300 ...`

## T. strict_inverter_checks = true -- refuses instead of running unchecked

Put `strict_inverter_checks = true` on its own line under `wol_broadcast_addr` (above `[modbus]`).

**Do (a):** `set 217 15`, start the bridge. **Expect:** `strict_inverter_checks is enabled and the
inverter settings have errors -- refusing to operate` every 5 s, **no** poll lines. `set 217 20` -> polling starts.

**Do (b):** `mute 0 213 217 220`, start the bridge. **Expect:** `inverter settings unreadable (2 in a row)`,
`(3 in a row)`, ... -- **never** `monitoring WITHOUT a settings check`, no poll lines. `unmute` -> polling starts.

## U. Sagging grid -- voltage still 150 V, but the inverter's grid relay is open

The real-world failure this covers: on a weak grid the inverter opens its
grid relay and runs from the battery while the voltage register still reads
well above `grid_lost_voltage`. Voltage alone would never notice.

**Do:** from normal running (A): `grid 150`; wait 7 s; `relay 0`; wait for
`OnBattery`; `soc 25`; wait for the sequence to finish; then `relay 1`; then `restore`.

**Expect:**
- [ ] after `grid 150`: log shows `grid=150.0V relay=closed` and **no** `state:` line, even after 7 s
- [ ] after `relay 0`: `state: Idle -> GridLostDebouncing`, then `-> OnBattery` (voltage unchanged at 150 V)
- [ ] after `soc 25`: `firing shutdown sequence`, marker file exists, all endpoints shut down
- [ ] after `relay 1` (voltage still 150 V): `state: ShutdownLatched -> RecoveryDebouncing`
- [ ] after `restore`: `-> Idle`, then Wake-on-LAN rounds, marker removed

To try the fallback by hand: `set 194 7` (an undefined value) makes the bridge
ignore the relay again -- `relay=n/a` in the log -- and decide on the voltage
alone (and logs a warning at the next connect).

## V. Comms loss while on battery -- fail-safe shutdown fired

If the inverter stops answering over RS485 (adapter unplugged, cable severed,
inverter locked up) while the site is running on battery, the battery is draining
unseen. After `comms_loss_shutdown_secs` (5 s in test config), the bridge fires
the shutdown sequence fail-safe rather than waiting for the hard cutoff.

**Do:** from normal running: `outage`; wait for `OnBattery`; then stop the simulator (Ctrl+C).

**Expect:**
- [ ] `state: Idle -> GridLostDebouncing`, then `-> OnBattery`
- [ ] after the simulator stops: `modbus poll failed: timed out reading register ...`
- [ ] ~5 s after the last reading:
      `battery state unknown, firing shutdown sequence (comms_loss_shutdown_secs = 5)`
- [ ] `state machine triggered shutdown sequence`
- [ ] marker file exists, SSH commands dispatched to all endpoints in `ssh.log`
- [ ] when simulator is restarted (default 230 V): `inverter: device type 0x0300`,
      `state: ShutdownLatched -> RecoveryDebouncing`, then `-> Idle`, WOL rounds sent

## W. Windows shutdown already scheduled (error 1190) -- treated as accepted

Windows `shutdown /s` returns error 1190 ("A system shutdown has already been scheduled")
if a shutdown was already initiated (e.g., following a previous SSH retry that timed out
client-side after dispatch). The bridge recognises this and treats it as successfully accepted.

**Do:**
```bash
echo 1 > /tmp/mub-test/ssh-already-scheduled-10.99.0.1
```
Then `outage`; wait for `OnBattery`; `soc 25`.

**Expect:**
- [ ] `firing shutdown sequence`
- [ ] `ws-1: a shutdown is already scheduled on the host (error 1190) -- treating as accepted`
- [ ] `ws-1` marked as dispatched in manifest without retrying
- [ ] sequence proceeds to `ws-2` and `proxmox`; `shutdown sequence complete`

---

## X. Restart mid-sequence while the inverter is silent -- the waiting endpoint is still shut down

**Do:** `./setup.sh reset`; `echo 10.99.0.2 > /tmp/mub-test/ssh-fail`; run the bridge; `outage`; wait for
`OnBattery`; `soc 25`; wait for `shutdown sequence finished: 2/3 ... marker left incomplete`. Stop the
bridge, `rm /tmp/mub-test/ssh-fail`, **stop the simulator**, start the bridge again.

**Expect:**
- [ ] `indicates incomplete shutdown: 2 endpoint(s) already dispatched, 1 remaining: ["ws-2"]`
- [ ] after `comms_loss_shutdown_secs`: `no valid inverter reading for N s after a restart that interrupted a shutdown -- battery state unknown`
- [ ] `inverter silent: resuming shutdown sequence for 1 remaining endpoint(s)`, then `ws-2: shutdown command accepted`
- [ ] start the simulator again (grid up): `ShutdownLatched -> RecoveryDebouncing -> Idle`

---

## Y. Restart mid-sequence with grid up, then comms lost -- waiting endpoint spared

**Do:** `./setup.sh reset`; write `# shutdown sequence in progress\ndispatched: ws-1\ndispatched: ws-2\n` to `/tmp/mub-test/shutdown_fired`; run simulator (`restore`, `soc 80`); run the bridge. Wait for `grid currently up; holding 1 remaining shutdown(s)`. **Stop the simulator** (inverter goes silent) before recovery debounce completes; wait 8 s. Then start simulator (`restore`, `soc 80`).

**Expect:**
- [ ] `indicates incomplete shutdown: 2 endpoint(s) already dispatched, 1 remaining: ["proxmox"]`
- [ ] `grid currently up; holding 1 remaining shutdown(s) pending recovery confirmation`
- [ ] inverter silence does NOT trigger `inverter silent: resuming shutdown sequence`: `proxmox` is spared
- [ ] simulator restored: `state: RecoveryDebouncing -> Idle`
- [ ] `recovery confirmed: 1 remaining endpoint(s) were spared from shutdown`
- [ ] Wake-on-LAN rounds run and clear the marker; `10.99.0.3` is never contacted

---

## Z. Late endpoint command acceptance after recovery -- marker write skipped, not resurrected

**Do:** `./setup.sh reset`; `echo 28 > /tmp/mub-test/ssh-delay-10.99.0.1`; run simulator (`restore`, `soc 80`); run the bridge; `outage`; wait for `OnBattery`; `soc 25`. When `shutting down ws-1` logs, immediately `restore` and `soc 80`. Wait for recovery to confirm and Wake-on-LAN round 4/4 to complete and clear the marker.

**Expect:**
- [ ] `recovery confirmed -- stopping the rest of the shutdown sequence`
- [ ] `Wake-on-LAN round 1/4` ... `round 4/4` clears the marker
- [ ] delayed `ws-1: shutdown command accepted` finishes after recovery and does NOT recreate the marker
- [ ] `recovery confirmed -- not starting the remaining endpoints`: `ws-2` and `proxmox` are spared
- [ ] `/tmp/mub-test/shutdown_fired` does not exist after recovery completes
- [ ] bridge restart sees clean state without residual marker

---

## What this does and doesn't prove

**Proves:** the decision logic running on the real clock; the Modbus RTU
traffic over a serial line; the exact commands handed to `ssh`; Wake-on-LAN
packets and rounds; the marker file surviving a restart; that failures
(silent inverter, garbage data, a dead or hung endpoint) never cause a wrong
shutdown or stall the bridge.

**Doesn't prove:** that the real Sunsynk answers the same way, that SSH logins
work on the real machines, or that they actually power off and wake up. That
is level 3, on site -- see "Deployment sketch" step 8 in the main README.
