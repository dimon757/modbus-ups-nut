# Windows Workstation SSH Setup for Automated UPS Shutdown

This guide details how to configure a domain-joined or standalone Windows 10/11 workstation to allow `modbus-ups-bridge` (or any UPS monitor) to initiate an automated graceful shutdown over SSH using public-key authentication.

---

## Architecture & Security Principles

1. **Dedicated Local Account (`UPS-shutdown`)**:
   - Creating a **local** account on the workstation (rather than an Active Directory domain account) ensures shutdown commands succeed even when domain controllers are unreachable or already offline during a power outage.
2. **Low-Privilege Account**:
   - In Windows workstation editions (10/11 Pro & Enterprise), members of the local `Users` group are granted `SeShutdownPrivilege` ("Shut down the system") by default.
   - Standard accounts avoid the Windows OpenSSH quirk where members of `Administrators` are forced to use `C:\ProgramData\ssh\administrators_authorized_keys`.
3. **Avoid Profile Collisions**:
   - **Never manually create `C:\Users\UPS-shutdown` before the user logs in.**
   - Pre-creating the directory causes Windows to create the active profile as `C:\Users\UPS-shutdown.<COMPUTERNAME>`, leading to path mismatches.
   - Initializing the profile on first login ensures Windows creates and registers the profile cleanly.
4. **Direct Transfer via SCP (Prevents Key Corruption)**:
   - Copy-pasting public keys via terminal or clipboard frequently introduces line-breaks and leading indentation spaces, which corrupts the cryptographic base64 data.
   - Transferring the public key file directly using `scp` guarantees an exact, byte-for-byte pristine single-line file.

---

## Step 1: Install and Enable OpenSSH Server (Windows)

On the Windows workstation, open **PowerShell as Administrator**:

```powershell
# 1. Install OpenSSH Server capability
Add-WindowsCapability -Online -Name OpenSSH.Server~~~~0.0.1.0

# 2. Configure automatic startup and start service
Set-Service -Name sshd -StartupType Automatic
Start-Service sshd

# 3. Verify firewall allows port 22
Get-NetFirewallRule -Name *OpenSSH-Server* | Select-Object Name, Enabled, Direction, Action
```

---

## Step 2: Create the Local `UPS-shutdown` User (Windows)

Run in **PowerShell (as Administrator)**:

```powershell
# Prompt for a secure password
$Password = Read-Host -AsSecureString "Enter password for UPS-shutdown"

# Create local user
New-LocalUser -Name "UPS-shutdown" `
              -Password $Password `
              -PasswordNeverExpires `
              -Description "Automated UPS shutdown account"
```

> [!IMPORTANT]
> - Do **NOT** add this user to the `Administrators` group.
> - Do **NOT** manually create any folder in `C:\Users`.

---

## Step 3: Initialize Profile and Transfer Key via SCP (Linux Bridge)

From your **Linux bridge terminal (`bridge@UPS-Bridge`)**:

### 1. Initialize the Windows profile & create `.ssh` folder
```bash
ssh UPS-shutdown@<WORKSTATION-IP> "powershell -Command New-Item -ItemType Directory -Path .ssh -Force"
```
*(Enter the password for `UPS-shutdown` when prompted. This logs in for the first time, registers the Windows profile, and creates the `.ssh` folder).*

### 2. Copy the public key directly using SCP
Transfer the public key file directly from Linux to Windows:

```bash
sudo scp /etc/modbus-ups-bridge/workstation_key.pub UPS-shutdown@<WORKSTATION-IP>:.ssh/authorized_keys
```
*(Enter your Linux sudo password if prompted, then the Windows `UPS-shutdown` password).*

> [!TIP]
> Using `scp` transfers the key cleanly with no line-wrapping, no missing characters, and no clipboard indentation artifacts.

---

## Step 4: Apply Strict NTFS ACLs (Windows)

Windows OpenSSH strictly verifies NTFS permissions. If permissions are inherited from parent folders, OpenSSH will silently reject the key.

On the Windows workstation, run in **PowerShell (as Administrator)**:

```powershell
# 1. Automatically locate the active profile directory
$ProfilePath = (Get-ItemProperty 'HKLM:\SOFTWARE\Microsoft\Windows NT\CurrentVersion\ProfileList\*' | 
    Where-Object { $_.ProfileImagePath -like "*UPS-shutdown*" }).ProfileImagePath

$SshDir = "$ProfilePath\.ssh"
$AuthKeys = "$SshDir\authorized_keys"

# 2. Disable inheritance and grant exclusive access to User, SYSTEM, and Administrators
icacls.exe $SshDir /inheritance:r
icacls.exe $SshDir /grant:r "UPS-shutdown:(OI)(CI)F"
icacls.exe $SshDir /grant:r "SYSTEM:(OI)(CI)F"
icacls.exe $SshDir /grant:r "Administrators:(OI)(CI)F"

icacls.exe $AuthKeys /inheritance:r
icacls.exe $AuthKeys /grant:r "UPS-shutdown:F"
icacls.exe $AuthKeys /grant:r "SYSTEM:F"
icacls.exe $AuthKeys /grant:r "Administrators:F"

# 3. Set owner
icacls.exe $SshDir /setowner "UPS-shutdown"
icacls.exe $AuthKeys /setowner "UPS-shutdown"

Write-Host "===> Permissions configured successfully!" -ForegroundColor Green
```

---

## Step 5: Test Key-Based Login (Linux Bridge)

From the **Linux bridge terminal**, test the connection using the private key:

```bash
sudo ssh -i /etc/modbus-ups-bridge/workstation_key UPS-shutdown@<WORKSTATION-IP> "whoami"
```

**Expected output:**
```text
<COMPUTERNAME>\ups-shutdown
```
*(Logs in immediately without asking for a Windows password!)*

---

## Step 6: Hardening with `ForceCommand` (Windows)

To guarantee that a compromised key can only execute the shutdown command and cannot open an interactive shell, lock it down in `C:\ProgramData\ssh\sshd_config`:

1. Open `C:\ProgramData\ssh\sshd_config` (Notepad as Administrator).
2. Append this block to the **very end** of the file:

```text
Match User ups-shutdown
    ForceCommand shutdown /s /t 60 /c "Inverter battery low, grid still down -- shutting down."
```

3. Restart the OpenSSH service in PowerShell:
```powershell
Restart-Service sshd
```

> [!NOTE]
> `ForceCommand` overrides any command sent over SSH. When configured, any SSH connection by `UPS-shutdown` triggers a 60-second shutdown countdown.

---

## Step 7: Configure in `bridge.toml`

Add the Windows workstation to your `bridge.toml` configuration on the bridge:

```toml
[[endpoints]]
name = "workstation-1"
kind = "windows"
host = "192.168.100.16"
ssh_user = "UPS-shutdown"
ssh_key_path = "/etc/modbus-ups-bridge/workstation_key"
shutdown_delay_secs = 60
mac_address = "AA:BB:CC:DD:EE:FF"  # Workstation MAC address for Wake-on-LAN
```

---

## Troubleshooting Quick-Reference

| Symptom | Root Cause | Solution |
|---|---|---|
| `Load key ...: Permission denied` | Linux file permissions on private key | Run command with `sudo` (`sudo ssh ...`). The background service runs as root and will read it automatically. |
| `UPS-shutdown@... password:` prompt | Corrupted key in `authorized_keys` | Re-transfer the key directly using `sudo scp /path/to/key.pub UPS-shutdown@<IP>:.ssh/authorized_keys` to avoid clipboard/wrapping issues. |
| `Permission denied (publickey)` | NTFS permissions too loose | Re-run the `icacls.exe` script in Step 4 to strip inheritance and remove unauthorized users. |
| `Permission denied (publickey)` | User is an Administrator | Windows OpenSSH redirects admin keys to `C:\ProgramData\ssh\administrators_authorized_keys`. Remove `UPS-shutdown` from `Administrators`. |
| `Access is denied (5)` on `shutdown` | Missing shutdown privilege | Check Local Security Policy (`secpol.msc` → *User Rights Assignment* → *Shut down the system*) and ensure `UPS-shutdown` or `Users` is listed. |
| Host key verification prompt | Workstation host key not in known hosts | Scan and pin the workstation host key: `ssh-keyscan -H <WORKSTATION-IP> \| sudo tee -a /root/.ssh/known_hosts`. |
