# Installing the bridge -- from Debian installation to a running service

Step by step, from a bare-metal mini PC to a running, verified bridge.
Written for the N2840 mini PC on **Debian 13 "trixie" amd64** (see README,
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

## 1. Hardware prerequisites, cabling & BIOS settings

### Hardware requirements
- **Box:** Fanless mini PC (Intel Celeron N2840, x86-64, 2 cores), 2–4 GB RAM, 64–128 GB SSD.
- **Network:** 2x 1 Gb Ethernet ports (one for the endpoints LAN, one for internet/management).
- **Serial:** Onboard RS485 port (COM1 / `/dev/ttyS0`) with automatic directional switching, or an industrial USB-to-RS485 adapter (`/dev/ttyUSB0`).

### Critical power rule
**The bridge box, the network switch, and all intermediate networking gear MUST be powered directly from the inverter's AC Load (UPS) output.**
If the network switch loses power when the grid fails, the bridge will be running on battery power but unable to send SSH shutdown commands to endpoints (Broadcom KB 421950 describes this failure).

### RS485 cabling to the inverter
- Wire twisted pair cable (A/Data+ and B/Data-) from the bridge's COM port to the Sunsynk/Deye inverter's designated **RS485 Modbus / Meter / Monitoring** port (check the inverter manual: typically RJ45 pins 7/8 or dedicated screw terminals).
- **Do NOT** connect to the battery BMS port (CAN/RS485 BMS).
- If the bridge later times out on every read despite correct baud rate and slave ID, swapping A and B wires is the classic first fix.

### BIOS settings on the bridge box
Enter the BIOS/UEFI setup on boot (usually Del, F2, or F11):
- **COM Port Mode:** Under SuperIO / Serial Configuration, set the COM port wired to the inverter to **RS485** mode (2-wire half-duplex). Do **not** use RS232 or RS422.
- **Power On after AC Loss / Restore on AC Power Loss:** Set to **Power On** (or **Always On**). If the battery cuts out completely, the box must restart automatically as soon as inverter AC output returns.
- **Watchdog:** If the BIOS has an Intel TCO / Watchdog option, set it to **Enabled**.
- **Power Management:** Disable ErP / Deep Sleep / aggressive PCIe ASPM that could put Ethernet NICs or UARTs to sleep.
- **Boot Mode:** UEFI (recommended) or Legacy.

## 2. Install Debian 13 "trixie" (step-by-step)

### Prepare installation media
1. Download the official Debian 13 "trixie" (or Debian 12 "bookworm") netinst ISO (`debian-*-amd64-netinst.iso`) from <https://www.debian.org/>.
2. Write the ISO to a USB flash drive using Rufus (choose **DD Image** mode when prompted) or balenaEtcher.

### Boot the installer
1. Insert the USB drive into the bridge box, power on, and press the boot menu key (F11/F12/Esc).
2. Select the UEFI USB drive.
3. At the boot menu, select **Install** (text mode is fast and reliable) or **Graphical install**.

### Walkthrough of installer prompts
1. **Language & Location:**
   - Select `English`, your territory/country, and your keyboard layout (e.g. `American English`).
2. **Network autoconfiguration:**
   - The installer detects both Ethernet interfaces (`eth0`/`enp1s0` and `eth1`/`enp2s0`).
   - Select the interface plugged into your router/DHCP network with internet access for downloading base packages.
3. **Hostname & Domain:**
   - Hostname: `modbus-bridge` (or your preferred name).
   - Domain: Leave blank or set your local network domain (e.g. `lan`).
4. **Root & User Account Setup (Debian Sudo Behavior):**
   - **Recommended:** Leave the **root password BLANK**. When the root password is left empty, the Debian installer disables direct root login, automatically installs `sudo`, and grants full administrative rights to the primary user account.
   - Enter your real name and username (e.g. `bridge` or `admin`).
   - Set a strong password for this user.
   - *(Note: If you do choose to set a root password, Debian will NOT install `sudo` by default; see step 3 for how to enable sudo manually).*
5. **Clock & Timezone:**
   - Select your local timezone.
6. **Disk Partitioning (SSD):**
   - Partitioning method: **Guided - use entire disk**.
   - Select the internal SSD.
   - Partitioning scheme: **All files in one partition (recommended for new users)**.
   - The installer creates:
     - EFI System Partition (~512 MB, FAT32)
     - Root partition `/` (ext4)
     - Swap partition (~1–2 GB)
   - Select **Finish partitioning and write changes to disk** and confirm with **Yes**.
7. **Package Manager Mirror:**
   - Select your country and default mirror (`deb.debian.org`). Leave HTTP proxy empty unless required.
8. **Popularity Contest:** Select **No**.
9. **Software Selection (`tasksel`) — CRITICAL:**
   - **UNCHECK** `Debian desktop environment` and any desktop GUI (`GNOME`, `Xfce`, etc.).
   - **CHECK** `SSH server`.
   - **CHECK** `standard system utilities`.
   - *Why:* A headless install runs in under 150 MB RAM, eliminates desktop power daemons that could suspend the CPU, and keeps the box lean and robust.
10. **Install GRUB Boot Loader:** Confirm installing GRUB to the primary SSD.
11. **Finish installation:** Select **Continue**, remove the USB flash drive, and allow the box to reboot into Debian.

## 3. Base system & dual-Ethernet network setup

Log in via console or SSH using the account created during installation.

### Verify sudo
Test that your user has sudo access:
```bash
sudo whoami
```
Must return `root`.

*(If you set a root password during the Debian install and sudo is missing, log in as root with `su -`, install sudo with `apt update && apt install -y sudo`, add your user with `usermod -aG sudo bridge`, then log out and back in as your user).*

### Full system update
```bash
sudo apt update && sudo apt full-upgrade -y
```

### Dual-Ethernet network configuration
The box has two Ethernet ports:
- **Port 1 (e.g. `enp1s0`):** Dedicated to the **Endpoints LAN** (connected to the network switch powering the workstations and Proxmox hosts). Must have a **static IP**.
- **Port 2 (e.g. `enp2s0`):** Connected to your management network / upstream router for internet access and remote administration (can use DHCP).

Run `ip link` to find the exact kernel interface names. Then edit `/etc/network/interfaces`:

```bash
sudo nano /etc/network/interfaces
```

Configure the file (adjust interface names and IP ranges to match your site):

```ini
# The loopback network interface
auto lo
iface lo inet loopback

# Port 1: Endpoints LAN (Static IP + Subnet Broadcast for WOL)
auto enp1s0
iface enp1s0 inet static
    address 192.168.1.10/24
    broadcast 192.168.1.255

# Port 2: Management / Internet access (DHCP or secondary static)
auto enp2s0
iface enp2s0 inet dhcp
```

**Why explicit `broadcast` is mandatory:**
Wake-on-LAN magic packets are UDP broadcasts. In `bridge.toml`, `wol_broadcast_addr` is set to your subnet broadcast (e.g. `"192.168.1.255:9"`). Setting `broadcast 192.168.1.255` on `enp1s0` ensures the Linux routing table transmits the WOL packets out through `enp1s0` to your endpoints. If generic `255.255.255.255` were used, the kernel would send it out the default-gateway interface (`enp2s0`), missing the endpoints entirely!

Apply the network configuration:
```bash
sudo systemctl restart networking
ip addr show
```

## 4. RS485 serial port setup & console conflict prevention

Onboard COM ports appear as `/dev/ttyS0`, `/dev/ttyS1`, etc. (USB adapters appear as `/dev/ttyUSB0`). Check what the kernel detected:
```bash
dmesg | grep ttyS
```

### CRITICAL: Disable systemd serial console (getty)
By default, Debian systemd often activates `serial-getty@ttyS0.service` if serial console support is detected.
**If left running, systemd outputs `login:` prompts to the serial port and reads incoming bytes from the inverter as login attempts, corrupting Modbus frames and causing constant timeouts!**

Stop, disable, and mask it immediately:
```bash
sudo systemctl stop serial-getty@ttyS0.service
sudo systemctl disable serial-getty@ttyS0.service
sudo systemctl mask serial-getty@ttyS0.service
```

### Add user to dialout group
Allow your normal user to access serial ports for manual testing:
```bash
sudo usermod -aG dialout $USER
```
*(Log out and log back in for group membership to take effect).*

### Install `mbpoll` and verify Modbus communication
Install `mbpoll` (Debian 13 includes version 1.5.2):
```bash
sudo apt install -y mbpoll
```

Test reading holding register 0 (device type; note `mbpoll` is 1-indexed, so register 0 is `-r 1`):
```bash
mbpoll -a 1 -b 9600 -d 8 -s 1 -p none -t 3 -r 1 -c 1 /dev/ttyS0
```
Expect:
```
[1]: 768
```
(`768` decimal = `0x0300` hex, confirming single-phase hybrid inverter).

Test reading holding register 184 (battery SOC, `-r 185` in mbpoll):
```bash
mbpoll -a 1 -b 9600 -d 8 -s 1 -p none -t 3 -r 185 -c 1 /dev/ttyS0
```
Expect: the current battery percentage (e.g. `[185]: 85`).

*If every read times out:* Swap the A and B wires on the RS485 connector.

## 5. Hardware watchdog & persistent journal setup

### Hardware watchdog
Check if `/dev/watchdog` is present:
```bash
ls -l /dev/watchdog
```
- **Present:** `wdctl` shows the driver and timeout (expect `iTCO_wdt`, 30 s).
- **Missing:** Load the Intel TCO driver and make it permanent across boots:
  ```bash
  sudo modprobe iTCO_wdt
  echo iTCO_wdt | sudo tee /etc/modules-load.d/watchdog.conf
  ```
  If `/dev/watchdog` still does not appear, the BIOS does not expose it: use the kernel software watchdog fallback:
  ```bash
  sudo modprobe softdog
  echo softdog | sudo tee /etc/modules-load.d/watchdog.conf
  ```

**Leave `RuntimeWatchdogSec` UNSET in `/etc/systemd/system.conf`** — only one process can hold the watchdog device at a time, and `modbus-ups-bridge` manages and feeds it directly.

### Persistent systemd journal setup
By default, minimal installs may store logs in RAM (`/run/log/journal`). Ensure logs survive power cuts and reboots on the SSD:
```bash
sudo mkdir -p /var/log/journal
sudo install -d /etc/systemd/journald.conf.d
sudo install -m 644 systemd/journald-modbus-ups-bridge.conf /etc/systemd/journald.conf.d/modbus-ups-bridge.conf
sudo systemctl restart systemd-journald
```
Verify with `journalctl --disk-usage`. No `logrotate` needed.

## 6. Install the tools

```bash
sudo apt install -y git cargo openssh-client mbpoll wakeonlan tmux build-essential
```

`cargo` brings Rust 1.85 and the C toolchain it needs (the bridge requires Rust 1.77 or newer).

## 7. Get the code

As your normal user, in your home directory:
```bash
git clone https://github.com/dimon757/modbus-ups-bridge.git ~/modbus-ups-bridge
cd ~/modbus-ups-bridge
git log --oneline -1
```
Note the commit hash (the first 7 characters) — it identifies the installed revision.

## 8. Test and build

```bash
cargo test
```
Must end with `test result: ok.` and `0 failed`. The first run downloads dependencies from crates.io (requires internet).

```bash
cargo build --release
```
On the N2840, compiling takes 5–10 minutes on the first run. The result is a single self-contained binary at `target/release/modbus-ups-bridge` (~3 MB).

## 9. Install the program, service and config

```bash
sudo install -m 755 target/release/modbus-ups-bridge /usr/local/bin/
sudo install -m 644 systemd/modbus-ups-bridge.service /etc/systemd/system/
sudo install -d -m 700 /etc/modbus-ups-bridge
sudo install -m 600 config/bridge.toml.example /etc/modbus-ups-bridge/bridge.toml
sudo systemctl daemon-reload
```

Don't start the service yet — configure it first in step 10.

## 10. Edit the configuration

```bash
sudo nano /etc/modbus-ups-bridge/bridge.toml
```

Go through the settings top to bottom:

| Setting | Set to |
|---|---|
| `wol_broadcast_addr` | The endpoints' subnet broadcast + `:9`, e.g. `"192.168.1.255:9"` -- never `255.255.255.255` on this two-port box |
| `[modbus] device` | The RS485 port wired to the inverter: `dmesg \| grep ttyS` lists them (usually `/dev/ttyS0`) |
| `slave_id` | The inverter's Modbus address from its communication settings (normally 1) |
| `baud_rate`, `poll_interval_secs` | Leave: 9600, 5 |
| `[thresholds] inverter_cutoff_soc` | The inverter's own **battery Shutdown %** setting (read it off the inverter) |
| `low_battery_soc` | Well above that -- default 30 % with a 20 % cutoff. The bridge refuses to start if it isn't higher |
| Other thresholds | Leave defaults unless you have specific requirements |
| `[[endpoints]]` | One block per machine, in shutdown order -- see below |
| `[watchdog]` | Keep it if `/dev/watchdog` exists (step 5), otherwise delete the section |
| `[proxmox] method` | Leave `"poweroff"` for now; the Proxmox test in step 13 decides whether to switch to `"vms_then_poweroff"` |
| `ssh_known_hosts_file` | Leave commented out (root's `~/.ssh/known_hosts`, filled in step 11) |

**Endpoints:** One `[[endpoints]]` block per machine in desired shutdown order:
- `name`: unique identifier (e.g. `"proxmox-a"`, `"workstation-1"`)
- `kind`: `"windows"` or `"proxmox"`
- `host`: IP address
- `ssh_user`: `"root"` for Proxmox, `"ups-shutdown"` for Windows
- `ssh_key_path`: `/etc/modbus-ups-bridge/proxmox_key` or `.../workstation_key`
- `shutdown_delay_secs`: 10 for Proxmox, 60 for Windows
- `mac_address`: NIC MAC address for Wake-on-LAN

With multiple Proxmox hosts, putting them **first** gives their guest VMs the maximum shutdown window.

## 11. SSH keys & endpoint registration

Generate dedicated passphrase-less SSH keypairs with 600 permissions in `/etc/modbus-ups-bridge/`:

```bash
sudo ssh-keygen -t rsa -b 4096 -N "" -C modbus-ups-bridge-proxmox -f /etc/modbus-ups-bridge/proxmox_key
sudo ssh-keygen -t rsa -b 4096 -N "" -C modbus-ups-bridge-ws -f /etc/modbus-ups-bridge/workstation_key
sudo chmod 600 /etc/modbus-ups-bridge/*_key
```

### Proxmox hosts
Append the public key to `/root/.ssh/authorized_keys` on each Proxmox host:
```bash
sudo cat /etc/modbus-ups-bridge/proxmox_key.pub | ssh root@<proxmox-ip> 'cat >> /root/.ssh/authorized_keys'
```

Test the login **as root** (this records the host key into `/root/.ssh/known_hosts`):
```bash
sudo ssh -i /etc/modbus-ups-bridge/proxmox_key root@<proxmox-ip> 'pveversion'
```
Answer `yes` to trust the host key. It must print the Proxmox version without prompting for a password. Also verify `nohup` is installed (`which nohup`).

### Windows workstations
1. Set up OpenSSH Server, create the local `ups-shutdown` account, and configure the `ForceCommand` lockdown in `sshd_config` (see README, "Windows-side setup").
2. Copy the public key into `C:\Users\ups-shutdown\.ssh\authorized_keys`:
   ```bash
   sudo cat /etc/modbus-ups-bridge/workstation_key.pub
   ```
3. **Record host keys in `/root/.ssh/known_hosts` without logging in:**
   Because `ForceCommand` initiates a shutdown upon login, scan and record the host key using `ssh-keyscan`:
   ```bash
   sudo install -d -m 700 /root/.ssh
   ssh-keyscan -H <workstation-ip> | sudo tee -a /root/.ssh/known_hosts
   ```
   *(Test one real login when a shutdown of that PC is acceptable: `sudo ssh -i /etc/modbus-ups-bridge/workstation_key ups-shutdown@<ip>`)*.

**Crucial:** The bridge pins host keys (`StrictHostKeyChecking=yes`). If an endpoint's key is not recorded in `/root/.ssh/known_hosts`, the bridge will refuse to connect and logs an error at startup.

## 12. Start the service

```bash
sudo systemctl enable --now modbus-ups-bridge
systemctl status modbus-ups-bridge
```
Must show `active (running)`. Inspect the journal:
```bash
journalctl -u modbus-ups-bridge -n 30
```
Expect startup logs confirming connection and settings margin:
```
loaded config from /etc/modbus-ups-bridge/bridge.toml (4 endpoint(s))
inverter: device type 0x0300, battery mode 1, cutoff 20% / 46.00 V
inverter: protocol version (reg 2) 0x0102 (1.2), reg 54 0, grid relay (reg 194) 1 -- see docs/protocol-versions.md
inverter settings: inverter cutoff 20% SOC, shutdown sequence at 30% -- 10 points of margin
```
Ensure there are **no `ERROR` lines**.

## 13. Verify on site

In this order:
1. [register-verification.md](register-verification.md) — verify each register against the real inverter.
2. [proxmox-shutdown-test.md](proxmox-shutdown-test.md) — verify Proxmox VM graceful shutdown and WOL wake.
3. README "Deployment sketch", step 8 — complete end-to-end rehearsal (breaker test or threshold override).

## Everyday commands

| Task | Command |
|---|---|
| Service status | `systemctl status modbus-ups-bridge` |
| Follow live logs | `journalctl -u modbus-ups-bridge -f` |
| View outage logs | `journalctl -u modbus-ups-bridge --since "2026-10-05 14:00" --until "2026-10-05 18:00"` |
| Logs from previous boot | `journalctl -u modbus-ups-bridge -b -1` |
| Restart after config change | `sudo systemctl restart modbus-ups-bridge` |
| Stop (disarms watchdog cleanly) | `sudo systemctl stop modbus-ups-bridge` |
| Enable temporary debug logs | `sudo systemctl edit modbus-ups-bridge` (add `[Service]` and `Environment=RUST_LOG=debug`), then restart |

## Updating to a new version

```bash
cd ~/modbus-ups-bridge && git pull
cargo test && cargo build --release
sudo systemctl stop modbus-ups-bridge
sudo install -m 755 target/release/modbus-ups-bridge /usr/local/bin/
sudo systemctl start modbus-ups-bridge
```
If `modbus-ups-bridge.service` changed, re-copy it to `/etc/systemd/system/` and run `sudo systemctl daemon-reload`.

## Troubleshooting common issues

| Symptom | Cause | Solution |
|---|---|---|
| `mbpoll` or bridge times out on every read | Inverted RS485 polarity | Swap the A and B wires on the RS485 terminal |
| Periodic Modbus CRC / framing errors | `serial-getty` running on the COM port | Run `sudo systemctl mask --now serial-getty@ttyS0.service` |
| Service exits with `activating (auto-restart)` | Config error or `low_battery_soc <= inverter_cutoff_soc` | Check `journalctl -u modbus-ups-bridge -n 20` for the specific validation error |
| `host key of ... is not in /root/.ssh/known_hosts` | Missing pinned host key | Run `ssh-keyscan -H <ip> \| sudo tee -a /root/.ssh/known_hosts` |
| WOL does not wake workstations | NIC or BIOS sleeping in S5 | Enable "Wake on Magic Packet" in Windows driver; disable ErP/Deep Sleep in BIOS |
| Wake-on-LAN packets not received | Directed broadcast sent out wrong NIC | Verify `broadcast 192.168.1.255` in `/etc/network/interfaces` and `wol_broadcast_addr = "192.168.1.255:9"` |

## Uninstalling

```bash
sudo systemctl disable --now modbus-ups-bridge
sudo rm /etc/systemd/system/modbus-ups-bridge.service /usr/local/bin/modbus-ups-bridge /etc/systemd/journald.conf.d/modbus-ups-bridge.conf
sudo systemctl daemon-reload
```
`/etc/modbus-ups-bridge` (config and keys) and `/var/lib/modbus-ups-bridge` are kept; delete them by hand if no longer needed. Remove the public keys from the endpoints' `authorized_keys` files.
