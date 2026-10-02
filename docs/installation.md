# Installing the bridge -- from compilation to a running service

Step by step, from a freshly installed box to a running, verified bridge.
Written for the N2840 mini PC on **Debian 13 "trixie"** (see README,
"Target platform"). Plan about two hours, plus the on-site checks at the end.

## What you end up with

| Path | What | Permissions |
|---|---|---|
| `/usr/local/bin/modbus-ups-bridge` | The program | 755 |
| `/etc/modbus-ups-bridge/bridge.toml` | Its configuration | 600, root |
| `/etc/modbus-ups-bridge/proxmox_key`, `workstation_key` | SSH private keys | 600, root |
| `/etc/systemd/system/modbus-ups-bridge.service` | The service (starts at boot, restarts on failure) | 644 |
| `/etc/systemd/journald.conf.d/modbus-ups-bridge.conf` | Log retention and rotation | 644 |
| `/var/lib/modbus-ups-bridge/shutdown_fired` | Marker file, only while a shutdown awaits its wake-up | created by the service |
| `/root/.ssh/known_hosts` | The endpoints' SSH host keys | created by ssh |

The commands below that start with `sudo` need administrator rights;
the others run as your normal user.

## 1. BIOS settings on the bridge box

In the N2840's BIOS setup:

- **RS485 port mode:** set the COM port wired to the inverter to **RS485**
  (not RS232 or RS422).
- **Power On after AC Loss / Restore on AC Power Loss:** **Power On** -- the
  box must come back by itself when the inverter's output returns.
- **Watchdog:** if the BIOS has a watchdog/TCO option, leave it enabled.

## 2. Install Debian 13

1. Install Debian 13 amd64 from the netinst image. In "Software selection"
   tick only **SSH server** and **standard system utilities** (no desktop).
2. Network -- the box has two Ethernet ports: connect the **endpoints' LAN**
   to one. Give that port a **fixed IP** on the endpoints' subnet. Find the
   port names with `ip link` (e.g. `enp1s0`, `enp2s0`), then in
   `/etc/network/interfaces`, for example:
   ```
   auto enp1s0
   iface enp1s0 inet static
       address 192.168.1.10/24
   ```
   and `sudo systemctl restart networking`. The other port (e.g. for remote
   access) can stay on DHCP.
   **The network switch between the bridge and the endpoints must be
   powered from the inverter's output too** -- otherwise the shutdown
   commands can't reach the machines during an outage.
3. Update: `sudo apt update && sudo apt full-upgrade`.

## 3. Install the tools

```bash
sudo apt install git cargo openssh-client
```

`cargo` brings Rust 1.85 and the C toolchain it needs (the bridge needs
Rust 1.77 or newer). Optional but useful on site:

```bash
sudo apt install mbpoll wakeonlan tmux
```

(`mbpoll` for [register-verification.md](register-verification.md),
`wakeonlan` for [proxmox-shutdown-test.md](proxmox-shutdown-test.md), `tmux` for
several terminals over one SSH session.)

## 4. Get the code

As your normal user, in your home directory:

```bash
git clone https://github.com/dimon757/modbus-ups-bridge.git
```

The repository is private: Git asks for your GitHub user name and, as the
password, a **personal access token** (github.com → Settings → Developer
settings → Personal access tokens). No internet on the box? Clone on
another machine and copy the folder over (USB stick or `scp`).

```bash
cd modbus-ups-bridge
git log --oneline -1
```

Note the version (the first 7 characters) -- it tells later which code is
installed.

## 5. Test and build

```bash
cargo test
```

Must end with `test result: ok.` and `0 failed`. The first run downloads the
dependencies from crates.io -- the box needs internet for this step.

```bash
cargo build --release
```

On the N2840 this takes a few minutes (up to ~10 the first time). The
result is one file, `target/release/modbus-ups-bridge` (about 3 MB); it
needs nothing else at run time except `ssh`.

## 6. Install the program, service and config

```bash
sudo install -m 755 target/release/modbus-ups-bridge /usr/local/bin/
```
```bash
sudo install -m 644 systemd/modbus-ups-bridge.service /etc/systemd/system/
```
```bash
sudo install -d -m 700 /etc/modbus-ups-bridge
```
```bash
sudo install -m 600 config/bridge.toml.example /etc/modbus-ups-bridge/bridge.toml
```
```bash
sudo systemctl daemon-reload
```

Don't start the service yet -- the config still has example values.

## 7. Edit the configuration

```bash
sudo nano /etc/modbus-ups-bridge/bridge.toml
```

Every setting is commented in the file itself. Go through it top to bottom:

| Setting | Set to |
|---|---|
| `wol_broadcast_addr` | The endpoints' subnet broadcast + `:9`, e.g. `"192.168.1.255:9"` -- never `255.255.255.255` on this two-port box |
| `[modbus] device` | The RS485 port wired to the inverter: `dmesg \| grep ttyS` lists them (usually `/dev/ttyS0`) |
| `slave_id` | The inverter's Modbus address from its communication settings (normally 1) |
| `baud_rate`, `poll_interval_secs` | Leave: 9600, 5 |
| `[thresholds] inverter_cutoff_soc` | The inverter's own **battery Shutdown %** setting (read it off the inverter) |
| `low_battery_soc` | Well above that -- default 30 % with a 20 % cutoff. The bridge refuses to start if it isn't higher |
| Other thresholds | Leave the defaults unless you have a reason (see the comments) |
| `[[endpoints]]` | One block per machine, in shutdown order -- see below |
| `[watchdog]` | Keep it if `/dev/watchdog` exists (step 9), otherwise delete the section |
| `[proxmox] method` | Leave `"poweroff"` for now; the Proxmox test in step 12 decides whether to switch to `"vms_then_poweroff"` |
| `ssh_known_hosts_file` | Leave commented out (root's `~/.ssh/known_hosts`, filled in step 8) |

**Endpoints** -- one `[[endpoints]]` block per machine; any number of each
kind. Copy a block per machine and set:

- `name`: unique, used in the log (e.g. `"proxmox-a"`, `"workstation-1"`)
- `kind`: `"windows"` or `"proxmox"`
- `host`: its IP address
- `ssh_user`: `"root"` for Proxmox, `"ups-shutdown"` for Windows (README,
  "Windows-side setup")
- `ssh_key_path`: `/etc/modbus-ups-bridge/proxmox_key` or `.../workstation_key` (step 8)
- `shutdown_delay_secs`: 10 for Proxmox, 60 for Windows
- `mac_address`: the NIC's MAC address -- for Wake-on-LAN. On Windows:
  `getmac /v`; on Proxmox: `ip link`.

The order of the blocks is the shutdown order, `stagger_secs` (30 s)
apart. With several Proxmox hosts, putting them **first** gives them the most
time for their VMs.

## 8. SSH keys

One key for the Proxmox hosts, one for the workstations (or one per machine
if you prefer -- then adjust `ssh_key_path` per endpoint):

```bash
sudo ssh-keygen -t rsa -b 4096 -N "" -C modbus-ups-bridge-proxmox -f /etc/modbus-ups-bridge/proxmox_key
```
```bash
sudo ssh-keygen -t rsa -b 4096 -N "" -C modbus-ups-bridge-ws -f /etc/modbus-ups-bridge/workstation_key
```

No passphrase (`-N ""`): the service has nobody to type one. ssh-keygen
creates the private keys with permissions 600, which ssh requires; the
bridge checks this at startup and logs an error if not.

**Proxmox hosts** -- append the public key to each host (asks for the root
password once):

```bash
sudo cat /etc/modbus-ups-bridge/proxmox_key.pub | ssh root@<proxmox-ip> 'cat >> /root/.ssh/authorized_keys'
```

Then test the key **as root**, the way the service will use it -- this also
records the host's key in `/root/.ssh/known_hosts`:

```bash
sudo ssh -i /etc/modbus-ups-bridge/proxmox_key root@<proxmox-ip> 'pveversion'
```

The first time, ssh asks whether to trust the host's key -- answer `yes`.
It must then print the Proxmox version **without asking for a password**. On each
host also check `nohup` exists (`which nohup`).

**Windows workstations** -- follow README "Windows-side setup" (OpenSSH
server, the `ups-shutdown` account, the `ForceCommand` lockdown). Show the
public key and copy its single line into
`C:\Users\ups-shutdown\.ssh\authorized_keys` on each workstation:

```bash
sudo cat /etc/modbus-ups-bridge/workstation_key.pub
```

**Every endpoint's host key must be recorded** in `/root/.ssh/known_hosts`:
the bridge pins host keys (`StrictHostKeyChecking=yes`), so it will only
talk to machines recorded there -- and logs an error at startup for each one
missing. The Proxmox login test above records the Proxmox hosts. Record each
workstation's host key **without** logging in -- because of the
`ForceCommand` lockdown, a real login shuts the PC down:

```bash
sudo install -d -m 700 /root/.ssh
```
```bash
ssh-keyscan -H <workstation-ip> | sudo tee -a /root/.ssh/known_hosts
```

Test one real login when a shutdown of that PC is acceptable:
`sudo ssh -i /etc/modbus-ups-bridge/workstation_key ups-shutdown@<ip>` --
the PC must show its 60 s shutdown countdown.

## 9. Watchdog

```bash
ls -l /dev/watchdog
```

- **Present** -- keep the `[watchdog]` section in `bridge.toml`. `wdctl`
  shows the driver and timeout (expect `iTCO_wdt`, 30 s).
- **Missing** -- load the Intel driver and make it permanent:
  ```bash
  sudo modprobe iTCO_wdt
  ```
  ```bash
  echo iTCO_wdt | sudo tee /etc/modules-load.d/watchdog.conf
  ```
  If `/dev/watchdog` still doesn't appear, the BIOS doesn't expose it: use
  the software watchdog instead (`softdog` in the same two commands), or
  delete the `[watchdog]` section.

Leave `RuntimeWatchdogSec` **unset** in `/etc/systemd/system.conf` -- only one
program can hold the watchdog.

## 10. Logs (journal) -- retention and rotation

The bridge writes no log file of its own; systemd's journal stores its
output and rotates it within the limits in the provided file (keep on SSD,
max 1 GB, 5 GB always free, one year):

```bash
sudo install -d /etc/systemd/journald.conf.d
```
```bash
sudo install -m 644 systemd/journald-modbus-ups-bridge.conf /etc/systemd/journald.conf.d/modbus-ups-bridge.conf
```
```bash
sudo systemctl restart systemd-journald
```

Check with `journalctl --disk-usage`. No `logrotate` needed.

## 11. Start the service

```bash
sudo systemctl enable --now modbus-ups-bridge
```

```bash
systemctl status modbus-ups-bridge
```

must show `active (running)`. Then look at its log:

```bash
journalctl -u modbus-ups-bridge -n 30
```

Expect, within a few seconds:

```
loaded config from /etc/modbus-ups-bridge/bridge.toml (4 endpoint(s))
inverter: device type 0x0300, battery mode 1, cutoff 20% / 46.00 V
inverter: protocol version (reg 2) 0x0102 (1.2), reg 54 0 -- see docs/protocol-versions.md
inverter settings: inverter cutoff 20% SOC, shutdown sequence at 30% -- 10 points of margin
```

and **no `ERROR` lines** -- in particular none saying
`host key of ... is not in /root/.ssh/known_hosts` (redo step 8 for that
machine) or `SSH key ... can't be read`. If the status keeps saying `activating (auto-restart)`,
the bridge refused to start -- the journal says why (usually a config value;
the message names it). Fix, then `sudo systemctl restart modbus-ups-bridge`.

## 12. Verify on site

In this order:

1. [register-verification.md](register-verification.md) -- every register
   against the real inverter.
2. [proxmox-shutdown-test.md](proxmox-shutdown-test.md) -- the Proxmox shutdown
   command on one host with a test VM.
3. README "Deployment sketch", step 8 -- the full test: grid off at the
   inverter, shutdown sequence, grid back, Wake-on-LAN brings everything
   back.

Only then leave it unattended.

## Everyday commands

| Task | Command |
|---|---|
| Status | `systemctl status modbus-ups-bridge` |
| Log, live | `journalctl -u modbus-ups-bridge -f` |
| Log of an outage | `journalctl -u modbus-ups-bridge --since "2026-10-05 14:00" --until "2026-10-05 18:00"` |
| Log since the last reboot / the one before | `journalctl -u modbus-ups-bridge -b` / `-b -1` |
| After editing `bridge.toml` | `sudo systemctl restart modbus-ups-bridge` |
| Stop (disarms the watchdog cleanly) | `sudo systemctl stop modbus-ups-bridge` |
| Debug output for a while | `sudo systemctl edit modbus-ups-bridge`, add `[Service]` and `Environment=RUST_LOG=debug`, then restart; remove it afterwards |

## Updating to a new version

```bash
cd ~/modbus-ups-bridge && git pull
```
```bash
cargo test && cargo build --release
```
```bash
sudo systemctl stop modbus-ups-bridge
```
```bash
sudo install -m 755 target/release/modbus-ups-bridge /usr/local/bin/
```
```bash
sudo systemctl start modbus-ups-bridge
```

If the update changed `systemd/modbus-ups-bridge.service` (`git log` or the
release notes say so), also install it again as in step 6 and run
`sudo systemctl daemon-reload` before starting. Compare
`config/bridge.toml.example` with your `bridge.toml`: a renamed or removed
setting makes the bridge refuse to start, and the journal names it. Then
check the log as in step 11.

Don't update during an outage. Stopping the service mid-outage is safe --
the marker file makes it resume latched when it starts again.

## Uninstalling

```bash
sudo systemctl disable --now modbus-ups-bridge
```
```bash
sudo rm /etc/systemd/system/modbus-ups-bridge.service /usr/local/bin/modbus-ups-bridge /etc/systemd/journald.conf.d/modbus-ups-bridge.conf
```
```bash
sudo systemctl daemon-reload
```

`/etc/modbus-ups-bridge` (config and keys) and `/var/lib/modbus-ups-bridge`
are kept; delete them by hand if no longer needed. Remove the public keys
from the endpoints' `authorized_keys` files.
