# Windows Workstation SSH Setup for Automated UPS Shutdown

This guide details how to configure a domain-joined or standalone Windows 10/11 workstation so that `modbus-ups-bridge` (or any UPS monitor) can initiate an automated graceful shutdown over SSH using public-key authentication, and can wake the workstation again with Wake-on-LAN (WoL) once power is restored.

**Placeholders used in this guide**

| Placeholder | Meaning |
| --- | --- |
| `<WORKSTATION-IP>` | IP address of the Windows workstation |
| `<BRIDGE-IP>` | IP address of the Linux bridge (`UPS-Bridge`) |
| `<COMPUTERNAME>` | Windows computer name |

---

## Architecture & Security Principles

1. **Dedicated local account (`ups-shutdown`)**:
   - A **local** account (rather than an Active Directory domain account) ensures shutdown commands succeed even when domain controllers are unreachable or already offline during a power outage.
2. **Low-privilege account**:
   - In Windows workstation editions (10/11 Pro & Enterprise), members of the local `Users` group are granted `SeShutdownPrivilege` ("Shut down the system") by default.
   - A standard account avoids the Windows OpenSSH quirk where members of `Administrators` are forced to use `C:\ProgramData\ssh\administrators_authorized_keys`.
3. **Avoid profile collisions**:
   - **Never manually create `C:\Users\ups-shutdown` before the user logs in.**
   - Pre-creating the directory makes Windows create the active profile as `C:\Users\ups-shutdown.<COMPUTERNAME>`, leading to path mismatches.
   - Initializing the profile on first login lets Windows create and register it cleanly.
4. **Direct transfer via SCP (prevents key corruption)**:
   - Copy-pasting public keys via terminal or clipboard frequently introduces line breaks and leading spaces, which corrupts the base64 data.
   - Transferring the public key file with `scp` guarantees an exact, single-line file.
5. **Harden last**:
   - `ForceCommand` (Step 7) turns *every* login of `ups-shutdown` into a shutdown, including test logins and `scp` transfers. Complete and test everything else first.
6. **Accepted risk**:
   - Anyone holding the private key can shut the workstation down, but nothing more. Keep the key readable by root only and use one key pair per workstation if you want to limit the impact of a leak.

---

## Prerequisites

- Windows 10/11 Pro or Enterprise, with local administrator access. This guide covers the Windows side of step 11 in [installation.md](installation.md).
- A Linux bridge with the OpenSSH client (`ssh`, `scp`, `ssh-keygen`) and a WoL tool (`wakeonlan` or `etherwake`).
- The bridge and the workstation reachable over the network. For WoL, they must also share a broadcast domain (see Step 8).

---

## Step 1: Generate the Key Pair (Linux Bridge)

On the Linux bridge, create a dedicated, passphrase-less ed25519 key. It has no passphrase because the bridge service must use it unattended.

```bash
sudo mkdir -p /etc/modbus-ups-bridge

sudo ssh-keygen -t ed25519 -N "" \
    -C "modbus-ups-bridge shutdown key" \
    -f /etc/modbus-ups-bridge/workstation_key

# Private key readable by root only; public key world-readable
sudo chown root:root /etc/modbus-ups-bridge/workstation_key /etc/modbus-ups-bridge/workstation_key.pub
sudo chmod 600 /etc/modbus-ups-bridge/workstation_key
sudo chmod 644 /etc/modbus-ups-bridge/workstation_key.pub
```

Check the result:

```bash
sudo ls -l /etc/modbus-ups-bridge/
sudo cat /etc/modbus-ups-bridge/workstation_key.pub   # must be ONE line: ssh-ed25519 AAAA... comment
```

> [!NOTE]
> ed25519 is supported by the OpenSSH server shipped with Windows 10 (1809+) and Windows 11. For several workstations, either reuse this key or generate one per host (e.g. `workstation-1_key`) and set `ssh_key_path` accordingly in Step 9.

---

## Step 2: Install and Enable OpenSSH Server (Windows)

On the Windows workstation, open **PowerShell as Administrator**:

```powershell
# 1. Install OpenSSH Server capability
Add-WindowsCapability -Online -Name OpenSSH.Server~~~~0.0.1.0

# 2. Configure automatic startup and start the service
Set-Service -Name sshd -StartupType Automatic
Start-Service sshd

# 3. Verify the firewall rule for port 22
Get-NetFirewallRule -Name *OpenSSH-Server* | Select-Object Name, Enabled, Direction, Action
```

**Optional but recommended:** accept SSH only from the bridge.

```powershell
Set-NetFirewallRule -Name OpenSSH-Server-In-TCP -RemoteAddress <BRIDGE-IP>
```

If the rule does not exist, create it:

```powershell
New-NetFirewallRule -Name 'OpenSSH-Server-In-TCP' -DisplayName 'OpenSSH Server (sshd)' `
    -Enabled True -Direction Inbound -Protocol TCP -Action Allow `
    -LocalPort 22 -RemoteAddress <BRIDGE-IP>
```

> [!NOTE]
> `Add-WindowsCapability` downloads the component from Windows Update. On isolated networks, install it from a Features-on-Demand ISO or use the official Win32-OpenSSH MSI instead.

---

## Step 3: Create the Local `ups-shutdown` User (Windows)

Run in **PowerShell (as Administrator)**:

```powershell
# Prompt for a secure password
$Password = Read-Host -AsSecureString "Enter password for ups-shutdown"

# Create local user
New-LocalUser -Name "ups-shutdown" `
              -Password $Password `
              -PasswordNeverExpires `
              -UserMayNotChangePassword `
              -Description "Automated UPS shutdown account"
```

> [!IMPORTANT]
> - Do **NOT** add this user to the `Administrators` group.
> - Do **NOT** manually create any folder in `C:\Users`.

---

## Step 4: Pin Host Key, Initialize Profile, Transfer Key (Linux Bridge)

Run all commands with `sudo`, because the bridge service runs as root and uses root's `known_hosts`.

### 1. Pin the workstation's host key

The bridge calls `ssh` with `StrictHostKeyChecking=yes`, so an endpoint whose key is not recorded is never contacted. Unless `ssh_known_hosts_file` is set in `bridge.toml`, the bridge uses root's `/root/.ssh/known_hosts`.

```bash
sudo mkdir -p -m 700 /root/.ssh
ssh-keyscan -H <WORKSTATION-IP> | sudo tee -a /root/.ssh/known_hosts
```

Optional: verify the fingerprint against the workstation (trust on first use otherwise).

```bash
# On the bridge
ssh-keyscan -t ed25519 <WORKSTATION-IP> 2>/dev/null | ssh-keygen -lf -
```

```powershell
# On the workstation
ssh-keygen -lf C:\ProgramData\ssh\ssh_host_ed25519_key.pub
```

### 2. Initialize the Windows profile and create the `.ssh` folder

```bash
sudo ssh ups-shutdown@<WORKSTATION-IP> "powershell -Command New-Item -ItemType Directory -Path .ssh -Force"
```

*(Enter the `ups-shutdown` password when prompted. This first login registers the Windows profile and creates the `.ssh` folder.)*

### 3. Copy the public key directly with SCP

```bash
sudo scp /etc/modbus-ups-bridge/workstation_key.pub ups-shutdown@<WORKSTATION-IP>:.ssh/authorized_keys
```

*(Enter the `ups-shutdown` password again when prompted.)*

> [!TIP]
> `scp` transfers the key byte-for-byte: no line wrapping, no missing characters, no clipboard indentation artifacts.

---

## Step 5: Apply Strict NTFS ACLs (Windows)

Windows OpenSSH strictly verifies NTFS permissions. If permissions are inherited from parent folders, OpenSSH silently rejects the key.

Run in **PowerShell (as Administrator)**. The script finds the profile through the user's SID, so it works even if the profile folder is named `ups-shutdown.<COMPUTERNAME>`, and it uses well-known SIDs so it also works on localized Windows (German, Russian, ...).

```powershell
# 1. Locate the profile via the user's SID (exact match)
$sid = (Get-LocalUser -Name 'ups-shutdown').SID.Value
$ProfilePath = (Get-ItemProperty `
    "HKLM:\SOFTWARE\Microsoft\Windows NT\CurrentVersion\ProfileList\$sid" `
    -ErrorAction Stop).ProfileImagePath

$SshDir   = Join-Path $ProfilePath '.ssh'
$AuthKeys = Join-Path $SshDir 'authorized_keys'

if (-not (Test-Path $AuthKeys)) {
    throw "authorized_keys not found at $AuthKeys - redo Step 4."
}

# 2. Strip inheritance and grant exclusive access
#    *S-1-5-18 = SYSTEM, *S-1-5-32-544 = Administrators
icacls.exe $SshDir /inheritance:r
icacls.exe $SshDir /grant:r "*${sid}:(OI)(CI)F" "*S-1-5-18:(OI)(CI)F" "*S-1-5-32-544:(OI)(CI)F"

icacls.exe $AuthKeys /inheritance:r
icacls.exe $AuthKeys /grant:r "*${sid}:F" "*S-1-5-18:F" "*S-1-5-32-544:F"

# 3. Set owner
icacls.exe $SshDir   /setowner "ups-shutdown"
icacls.exe $AuthKeys /setowner "ups-shutdown"

# 4. Verify
icacls.exe $SshDir
icacls.exe $AuthKeys
Write-Host "===> Permissions configured. Profile: $ProfilePath" -ForegroundColor Green
```

> [!NOTE]
> If the registry lookup fails, the profile does not exist yet. The first SSH login in Step 4 creates it.

---

## Step 6: Test Key-Based Login (Linux Bridge)

Do this **before** Step 7. After hardening, every login triggers a shutdown.

```bash
sudo ssh -i /etc/modbus-ups-bridge/workstation_key \
    -o IdentitiesOnly=yes -o PasswordAuthentication=no \
    ups-shutdown@<WORKSTATION-IP> "whoami"
```

**Expected output:**

```
<computername>\ups-shutdown
```

`PasswordAuthentication=no` makes a key problem fail loudly instead of silently falling back to a password prompt.

**Optional: verify the shutdown privilege** (still before Step 7):

```bash
KEY="-i /etc/modbus-ups-bridge/workstation_key -o IdentitiesOnly=yes"
sudo ssh $KEY ups-shutdown@<WORKSTATION-IP> "shutdown /s /t 300 /c test"
sudo ssh $KEY ups-shutdown@<WORKSTATION-IP> "shutdown /a"
```

If the first command fails with `Access is denied (5)`, see the troubleshooting table. If the abort fails, run `shutdown /a` on the workstation itself.

---

## Step 7: Hardening with `ForceCommand` (Windows)

To guarantee that a compromised key can only trigger the shutdown and cannot open an interactive shell, lock it down in `C:\ProgramData\ssh\sshd_config`:

1. Open `C:\ProgramData\ssh\sshd_config` (Notepad as Administrator).
2. Append this block at the **very end** of the file:

```
Match User ups-shutdown
    ForceCommand shutdown /s /t 60 /c "Inverter battery low, grid still down - shutting down."
    PermitTTY no
    AllowTcpForwarding no
    AllowAgentForwarding no
    X11Forwarding no
```

3. Validate the configuration, then restart the service:

```powershell
& "$env:WINDIR\System32\OpenSSH\sshd.exe" -t
Restart-Service sshd
```

> [!WARNING]
> After this step, **any** successful login as `ups-shutdown` starts a 60-second shutdown countdown. That includes `whoami` tests, `scp`/`sftp` transfers, and `ssh` with any command. To cancel a countdown, run `shutdown /a` on the workstation (or in an administrator session) within 60 seconds.

Notes:

- **Countdown:** `/t 60` is the authoritative delay. `ForceCommand` replaces whatever command the bridge sends, so `shutdown_delay_secs` in `bridge.toml` is ignored for this endpoint. Change the delay here, not there.
- **No abort by the bridge:** once the command is accepted, the bridge never sends `shutdown /a`. If grid power returns during the countdown, the workstation still shuts down and is then woken by the bridge's Wake-on-LAN rounds (Step 8).
- **Implied `/f`:** any `/t` above 0 implies `/f`, so when the countdown ends, open applications are closed without a chance to save. Use a longer `/t` if users need time to react.
- **Changing the key later:** temporarily comment out the `Match` block, run `Restart-Service sshd`, re-run Step 4.3 and Step 5, then restore the block and restart again.

---

## Step 8: Wake-on-LAN Prerequisites

The bridge can power the workstation back on with a WoL magic packet once the grid is restored. This only works if the workstation is configured for it.

### 1. BIOS / UEFI

- Enable **Wake on LAN** (also labelled *Power On by PCI-E/PCI Device* or *Wake on PCIe/LAN*).
- Disable **ErP / EuP Ready** (deep-sleep power saving), which cuts standby power to the network card.
- Set **Restore on AC Power Loss** (*AC Power Recovery*) to **Power On**. WoL cannot wake a PC whose power supply lost AC completely, for example when the UPS battery ran flat. This setting is the reliable fallback for that case.

### 2. Windows network adapter

Open PowerShell as Administrator.

```powershell
# Find the physical Ethernet adapter and its MAC address
Get-NetAdapter -Physical | Select-Object Name, InterfaceDescription, MacAddress, Status

# Show the WoL-related properties offered by the driver
Get-NetAdapterAdvancedProperty -Name "Ethernet" | Where-Object DisplayName -match "Wake|Magic|Shutdown|Energy|Green"

# Enable magic-packet wake (adapter name from above)
Enable-NetAdapterPowerManagement -Name "Ethernet" -WakeOnMagicPacket Enabled
```

Also check in **Device Manager → Network adapters → *your NIC* → Properties**:

- *Power Management*: **Allow this device to wake the computer** and **Only allow a magic packet to wake the computer** are both ticked.
- *Advanced*: **Wake on Magic Packet** and (on Realtek NICs) **Shutdown Wake-On-Lan** are *Enabled*; **Energy Efficient Ethernet** / **Green Ethernet** are *Disabled*.

### 3. Disable Fast Startup

`shutdown /s` performs a hybrid shutdown while Fast Startup is on, and some network cards do not wake reliably from that state.

```powershell
powercfg /hibernate off
```

Alternatively: *Control Panel → Power Options → Choose what the power buttons do → untick "Turn on fast startup"*. Note that `powercfg /hibernate off` also disables hibernation.

Confirm the NIC is armed for wake:

```powershell
powercfg /devicequery wake_armed
```

The adapter must appear in the list.

### 4. Network requirements

- A magic packet is a UDP **broadcast** (port 9 or 7). The bridge must be in the same broadcast domain (VLAN/subnet) as the workstation, or your router must forward directed broadcasts or relay WoL.
- Use the MAC address of the **physical Ethernet** adapter, not Wi-Fi or a virtual adapter. `Get-NetAdapter` prints it as `AA-BB-CC-DD-EE-FF`. In `bridge.toml`, use colons.

### 5. Test WoL manually

Shut the workstation down completely, then from the bridge:

```bash
wakeonlan AA:BB:CC:DD:EE:FF
# or
sudo etherwake -i eth0 AA:BB:CC:DD:EE:FF
```

The workstation should start within a few seconds. Fix any failure here before relying on automation.

---

## Step 9: Configure `bridge.toml`

Add the Windows workstation to the `bridge.toml` configuration on the bridge:

```toml
[[endpoints]]
name = "workstation-1"
kind = "windows"
host = "192.168.100.16"
ssh_user = "ups-shutdown"
ssh_key_path = "/etc/modbus-ups-bridge/workstation_key"
shutdown_delay_secs = 60                # ignored when ForceCommand is set; keep equal to /t in Step 7
mac_address = "AA:BB:CC:DD:EE:FF"       # physical NIC MAC, used for Wake-on-LAN
```

---

## Troubleshooting Quick-Reference

| Symptom | Root cause | Solution |
| --- | --- | --- |
| `Load key ...: Permission denied` | Linux file permissions on the private key | Run the command with `sudo`. Make sure the key is owned by root with mode `600`. The background service runs as root and reads it automatically. |
| `ups-shutdown@... password:` prompt | Key not accepted: corrupted `authorized_keys`, loose NTFS permissions, or wrong file location | Re-run Step 5, then check the OpenSSH event log (see below). To re-transfer the key, follow the *Changing the key later* note in Step 7 first. |
| `Permission denied (publickey)` | NTFS permissions too loose | Re-run the `icacls.exe` script in Step 5 to strip inheritance and remove unauthorized users. |
| `Permission denied (publickey)` | User is an Administrator | Windows OpenSSH redirects admin keys to `C:\ProgramData\ssh\administrators_authorized_keys`. Remove `ups-shutdown` from `Administrators`. |
| `Access is denied (5)` on `shutdown` | Missing shutdown privilege | Check Local Security Policy (`secpol.msc` → *User Rights Assignment* → *Shut down the system*) and make sure `ups-shutdown` or `Users` is listed. On domain-joined machines a GPO can override it; check with `gpresult /h report.html`. |
| `A system shutdown has already been scheduled (1190)` | A countdown is already running | Not an error: the bridge logs "treating as accepted" and moves on. To cancel manually, run `shutdown /a`. |
| A test login or `scp` suddenly starts a shutdown | `ForceCommand` from Step 7 is active | Cancel with `shutdown /a`. Follow *Changing the key later* in Step 7 before transferring files. |
| Host key verification prompt | Workstation host key not in known hosts | Pin it: `ssh-keyscan -H <WORKSTATION-IP> \| sudo tee -a /root/.ssh/known_hosts`. |
| `REMOTE HOST IDENTIFICATION HAS CHANGED` | Windows was reinstalled or the host keys were regenerated | Remove the old entry with `sudo ssh-keygen -R <WORKSTATION-IP>`, then re-pin the key. |
| Workstation does not wake | Fast Startup on, BIOS WoL off, PSU lost AC power, or the bridge is in a different broadcast domain | Re-check Step 8. For full power loss, set *Restore on AC Power Loss* to *Power On*. |

### Diagnostics

**Server log (Windows):**

```powershell
Get-WinEvent -LogName 'OpenSSH/Operational' -MaxEvents 30 | Format-List TimeCreated, Message
```

**Foreground debug mode (Windows):** handles one connection, then exits.

```powershell
Stop-Service sshd
& "$env:WINDIR\System32\OpenSSH\sshd.exe" -d
# connect from the bridge, read the output, then:
Start-Service sshd
```

**Client verbose mode (bridge):**

```bash
sudo ssh -vvv -i /etc/modbus-ups-bridge/workstation_key ups-shutdown@<WORKSTATION-IP>
```
