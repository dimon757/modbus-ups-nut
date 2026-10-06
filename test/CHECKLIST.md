# Level-2 test checklist

This runs the real bridge binary end to end, with its real clock, serial port,
Modbus traffic, `ssh` calls, Wake-on-LAN and marker file. Only the outside
world is simulated:

| Real thing | Replaced by |
|---|---|
| Sunsynk inverter on RS485 | `inverter_sim.py` on a `socat` virtual serial cable |
| SSH to the 4 machines | `bin/ssh`, which writes to `/tmp/mub-test/ssh.log` -- and, for the two Proxmox hosts, runs the bridge's `qm`/`systemctl` commands against simulated VMs (scenarios N) |
| Wake-on-LAN on the site LAN | packets to `127.0.0.1:40009`, shown by `wol_listen.py` |
| Real waits (60 s / 180 s / 30 s / 2 min) | 5 s / 10 s / 2 s / 5 s (`bridge-test.toml`) |
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
- [ ] `loaded config from .../bridge-test.toml (4 endpoint(s))`
- [ ] `inverter: device type 0x0300, battery mode 1, cutoff 20% / 46.00 V`
- [ ] `inverter: protocol version (reg 2) 0x0102 (1.2), reg 54 0 -- see docs/protocol-versions.md`
- [ ] `inverter settings: inverter cutoff 20% SOC, shutdown sequence at 30% -- 10 points of margin`
- [ ] every second: `soc=80.0% grid=230.0V load=500W batt=0W on_battery=false low_battery=false`
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
- [ ] `shutdown sequence starting: 4 endpoint(s)`
- [ ] ssh.log gets 4 lines, **2 s apart**, in config order:
  - `ups-shutdown@10.99.0.1: shutdown /s /t 60 /c "Inverter battery low, ..."`
  - `ups-shutdown@10.99.0.2: shutdown /s /t 60 ...`
  - `root@10.99.0.3: nohup sh -c 'sleep 10; /sbin/poweroff' > /tmp/ups-shutdown.log 2>&1 < /dev/null &`
  - `root@10.99.0.4: nohup sh -c ...`
- [ ] `shutdown sequence complete`

**D2. Latched -- no second shutdown.** `soc 22`, then `soc 21`.
- [ ] ssh.log does **not** grow; `low_battery=true` in the poll lines

**D3. Grid back -- wake-up, even though the battery is still low.** `restore` (SOC still 21); wait 10 s.
- [ ] `ShutdownLatched -> RecoveryDebouncing` right away, 10 s later `RecoveryDebouncing -> Idle`
- [ ] `Wake-on-LAN round 1/4` ... `round 4/4`, 5 s apart
- [ ] WOL listener: 4 rounds of 4 packets (`AA:BB:CC:00:00:01` ... `:04`), all `ok`
- [ ] `/tmp/mub-test/shutdown_fired` disappears only **after** round 4

## E. Grid flickers back, then drops with SOC already low (regression)

This is the bug found in the original code: the shutdown used to be skipped here.

**Do:** `./setup.sh reset`; `soc 35`; `outage`; wait for `OnBattery`; then quickly (all within 10 s): `restore`, `soc 30`, `outage`.

**Expect:**
- [ ] `OnBattery -> RecoveryDebouncing` after `restore`
- [ ] on the second `outage`: `firing shutdown sequence` and `RecoveryDebouncing -> ShutdownLatched`
- [ ] 4 lines in ssh.log

## F. Bridge restarts mid-outage -- remembers the shutdown

**Do:** `./setup.sh reset`; run D1 (outage, `soc 25`, 4 ssh lines). Stop the bridge (Ctrl+C) and start it again with `./run-bridge.sh`.

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
- [ ] `/tmp/mub-test/shutdown_fired indicates incomplete shutdown: 2 endpoint(s) already dispatched, 2 remaining: ["proxmox-a", "proxmox-b"]`
- [ ] `grid still down: resuming shutdown sequence for 2 remaining endpoint(s)`
- [ ] `ssh.log` gets shutdown calls **only** for `proxmox-a` (10.99.0.3) and `proxmox-b` (10.99.0.4) -- no re-dispatching `ws-1` or `ws-2`
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
- [ ] `/tmp/mub-test/shutdown_fired indicates incomplete shutdown: 2 endpoint(s) already dispatched, 2 remaining: ["proxmox-a", "proxmox-b"]`
- [ ] `grid currently up; holding 2 remaining shutdown(s) pending recovery confirmation`
- [ ] on `outage`: `grid still down: resuming shutdown sequence for 2 remaining endpoint(s)`
- [ ] `ssh.log` receives shutdown commands only for `proxmox-a` and `proxmox-b`
- [ ] `shutdown sequence complete` and marker file marked `completed`
- [ ] then `restore` + `soc 40`: after 10 s, Wake-on-LAN rounds sent and marker deleted

## F4. Failed endpoint retried & Proxmox timing

Simulates an endpoint failing (or a bridge restart during long Proxmox VM shutdowns): failed endpoints and mid-flight VMs must not be ticked off in the manifest.

**Do:** `./setup.sh reset`. Add failure for ws-2: `echo 10.99.0.2 > /tmp/mub-test/ssh-fail`. Start bridge with VM method: `./run-bridge.sh vms`. Trigger outage: `outage`, `soc 25`. While Proxmox VMs are stopping, inspect `/tmp/mub-test/shutdown_fired`. Stop bridge (Ctrl+C). Remove failure: `rm /tmp/mub-test/ssh-fail`. Restart bridge: `./run-bridge.sh vms`.

**Expect:**
- [ ] `shutting down ws-1` succeeds; `failed to shut down ws-2` logged
- [ ] during Proxmox VM shutdown, `/tmp/mub-test/shutdown_fired` contains `dispatched: ws-1`, but neither `ws-2` nor `proxmox-a`
- [ ] on restart: `indicates incomplete shutdown: 1 endpoint(s) already dispatched, 3 remaining: ["ws-2", "proxmox-a", "proxmox-b"]`
- [ ] `ws-2` is retried and accepted
- [ ] `proxmox-a` and `proxmox-b` complete VM shutdown and host poweroff
- [ ] `shutdown sequence complete` and marker file marked `completed`

## F5. Failed endpoint retried after sequence finishes (completed withheld)

Simulates an endpoint that fails permanently or exhausts its retry budget while the rest of the sequence completes to the end. The bridge must withhold `completed` so that any subsequent restart (e.g. minutes or hours later during an ongoing outage) still identifies the failed endpoint as incomplete and retries it, rather than skipping it.

**Do:** `./setup.sh reset`. Add failure for ws-2: `echo 10.99.0.2 > /tmp/mub-test/ssh-fail`. Start bridge: `./run-bridge.sh`. Trigger outage: `outage`, `soc 25`. Wait for the full shutdown sequence to finish (`shutdown sequence complete`). Inspect `/tmp/mub-test/shutdown_fired`: verify `completed` is absent and `ws-2` is not dispatched. Stop bridge (Ctrl+C). Remove failure: `rm /tmp/mub-test/ssh-fail`. Restart bridge: `./run-bridge.sh`.

**Expect:**
- [ ] during initial outage, ws-1, proxmox-a, proxmox-b succeed; ws-2 exhausts retry budget and fails
- [ ] bridge logs: `shutdown sequence finished: 3/4 endpoint(s) succeeded; marker left incomplete for retry on restart`
- [ ] `/tmp/mub-test/shutdown_fired` contains `dispatched` for ws-1, proxmox-a, proxmox-b, but NOT `completed`
- [ ] on restart: `indicates incomplete shutdown: 3 endpoint(s) already dispatched, 1 remaining: ["ws-2"]`
- [ ] `resuming shutdown sequence for 1 remaining endpoint(s)`
- [ ] `ws-2` is retried and succeeds: `ws-2: shutdown command accepted`
- [ ] bridge logs: `shutdown sequence complete: all 1 endpoint(s) succeeded`
- [ ] `/tmp/mub-test/shutdown_fired` now contains `completed`
- [ ] restore grid: Wake-on-LAN fires and marker file is removed

## G. Inverter goes silent -- no shutdown on missing data

**Do:** stop the simulator (Ctrl+C) for ~15 s, then start it again.

**Expect:**
- [ ] `modbus poll failed: timed out reading register 0x00b8 -- reconnecting`
- [ ] `could not read inverter settings: timed out reading register 0x0000`
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
- [ ] WARN `inverter settings: config inverter_cutoff_soc is 20% but the inverter is set to 30% -- ...`
- [ ] the bridge keeps running (logs only)

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

The fake ssh now **executes** the bridge's commands for proxmox-a (10.99.0.3)
and proxmox-b (10.99.0.4) against simulated VMs (`test/pve-bin/`): after
`./setup.sh reset`, proxmox-a has `dc01` (VM 100, shuts down 3 s after the request) and
`app server` (VM 101, 6 s), proxmox-b has `db01` (VM 102, 4 s). Each simulated VM is a line in
`/tmp/mub-test/proxmox/<host>/vms` (`<id>|<name>|<behaviour>`); its state is in
`state_<id>`, and `host` appears once the host has been powered off.
In ssh.log, each executed command ends with `-> exit <code>`.

### N1. VMs shut down, then systemctl poweroff

**Do:** `./setup.sh reset`; `outage`; wait for `OnBattery`; `soc 25`.

- [ ] `shutting down proxmox-a (10.99.0.3) via Proxmox: VMs first, then poweroff`
- [ ] `proxmox-a: 2 VM(s) registered, 2 running: dc01, app server`, then `guest shutdown requested for VM ...` for each
- [ ] proxmox-b starts 2 s later **while proxmox-a is still waiting** (the hosts run in parallel)
- [ ] `proxmox-a: VM dc01 is off`, `VM app server is off`, `proxmox-b: VM db01 is off`
- [ ] `proxmox-a: host power-off scheduled via systemctl poweroff` -- same for proxmox-b
- [ ] `shutdown sequence complete` only after both hosts are done
- [ ] `cat /tmp/mub-test/proxmox/*/host` shows `poweroff via systemctl` for both

### N2. A hung VM and a VM without QEMU Guest Agent

**Do:** `./setup.sh reset`, then add a VM that never shuts down to proxmox-a
and one without the guest agent to proxmox-b:

```bash
echo "109|stuck-vm|hang" >> /tmp/mub-test/proxmox/10.99.0.3/vms; echo on > /tmp/mub-test/proxmox/10.99.0.3/state_109
echo "108|no-tools-vm|notools" >> /tmp/mub-test/proxmox/10.99.0.4/vms; echo on > /tmp/mub-test/proxmox/10.99.0.4/state_108
```

then `outage`, wait for `OnBattery`, `soc 25`.

- [ ] `proxmox-b: guest shutdown of VM no-tools-vm failed ... -- will be powered off`
- [ ] the other VMs go off normally
- [ ] as soon as proxmox-b's other VM is off (no point waiting for a VM that refused):
      `proxmox-b: VM no-tools-vm could not be shut down gracefully -- powering it off hard`
- [ ] 15 s after the requests: `proxmox-a: VM stuck-vm still running after 15 s -- powering it off hard`
- [ ] **both hosts are still powered off** (`host power-off scheduled via systemctl poweroff`) -- a stuck VM never
      keeps a host running into the inverter's cutoff

### N3. systemctl poweroff refused (fallback to /sbin/poweroff)

**Do:** `./setup.sh reset`; `touch /tmp/mub-test/proxmox/10.99.0.4/systemctl-refuse`; `outage`; wait for `OnBattery`; `soc 25`.

- [ ] `proxmox-b: systemctl poweroff refused (...) -- falling back to /sbin/poweroff`
- [ ] `proxmox-b: host power-off scheduled via /sbin/poweroff (10 s)`; proxmox-a uses systemctl poweroff as in N1
- [ ] `cat /tmp/mub-test/proxmox/10.99.0.4/host` shows `poweroff via /sbin/poweroff`

### N4. Grid back while a host is still shutting its VMs down (Safety Verification)

**Do:** `./setup.sh reset`; add the hung VM to proxmox-a as in N2 (first line only); `outage`;
wait for `OnBattery`; `soc 25`; about 2 s later `restore` and `soc 40`.

- [ ] ~10 s after `restore`: `recovery confirmed -- stopping the rest of the shutdown sequence`
      and `recovery confirmed while VM shutdowns were in progress -- aborting local waits; no hard stop or host poweroff will be issued`
- [ ] Wake-on-LAN rounds start (`Wake-on-LAN round 1/4`)
- [ ] proxmox-a **aborts destructive steps**: `qm stop` (hard VM power-off) is **never** executed
- [ ] host power-off (`systemctl poweroff` or `/sbin/poweroff`) is **never** executed -- the host stays running and running guests are not killed on power restoration

### N5. One Proxmox host unreachable

**Do:** `./setup.sh reset`; `echo 10.99.0.4 > /tmp/mub-test/ssh-fail`; `outage`; wait for `OnBattery`; `soc 25`.

- [ ] `failed to shut down proxmox-b: listing VMs: ssh to 10.99.0.4 exited Some(255): ...`
- [ ] proxmox-a is shut down normally, as in N1

---

## O. Host booting on wakeup (SSH connection retry without head-of-line blocking)

Simulates an endpoint that is still booting up from a previous Wake-on-LAN round when a new outage fires. Its SSH daemon is not yet ready, returning `Connection refused`. The bridge delegates it to a background retry task (retrying every 2 s up to `ssh_connect_retry_secs`) and immediately advances to the next endpoint without delaying it (`need_stagger = false`). Once sshd becomes available, the background retry succeeds.

**Do:** `./setup.sh reset`; `echo 2 > /tmp/mub-test/ssh-booting-10.99.0.1`; `outage`; wait for `OnBattery`; `soc 25`.

**Expect:**
- [ ] `ws-1: SSH connection failed ... -- host may still be booting; continuing retries in background (5s retry budget remaining)`
- [ ] `ws-2` is dispatched immediately without waiting for `ws-1`
- [ ] `ws-1` background retries until the `ssh-booting` counter is exhausted, then logs `ws-1: shutdown command accepted (after background retry)`
- [ ] sequence proceeds through `proxmox-a` and `proxmox-b`
- [ ] `shutdown sequence complete` with all endpoints recorded in marker file
- [ ] `restore`, `soc 80`: Wake-on-LAN fires on recovery and clears marker

---

## P. Server booting with extended per-endpoint retry budget

Simulates a heavy server or Proxmox host that takes longer to boot than standard workstations. The host has an explicit per-endpoint `ssh_connect_retry_secs = 12` configured (overriding global 6 s). While it retries in the background, subsequent endpoints proceed immediately, and the server successfully shuts down once sshd finishes booting within its extended window.

**Do:** `./setup.sh reset`; `echo 3 > /tmp/mub-test/ssh-booting-10.99.0.3`; `outage`; wait for `OnBattery`; `soc 25`.

**Expect:**
- [ ] `proxmox-a` initial connect returns transient failure; logs background retry with extended budget: `continuing retries in background (11s retry budget remaining)`
- [ ] `proxmox-b` is dispatched immediately without delay
- [ ] `proxmox-a` background task succeeds within its 12s budget: `proxmox-a: shutdown command accepted (after background retry)`
- [ ] all 4 endpoints successfully complete and are recorded in the marker file
- [ ] sequence finishes with `shutdown sequence complete`

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
