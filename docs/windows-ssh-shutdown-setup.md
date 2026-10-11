# Windows Workstation SSH Setup for Automated UPS Shutdown

This guide details how to configure a domain-joined or standalone Windows 10/11 workstation to allow `modbus-ups-bridge` (or any UPS monitor) to initiate an automated graceful shutdown over SSH using public-key authentication.

---

## Architecture & Principles

1. **Dedicated Local Account (`UPS-shutdown`)**:
   - Creating a **local** account on the workstation (rather than an Active Directory domain account) ensures shutdown commands succeed even when domain controllers are unreachable or already offline during a power outage.
2. **Low-Privilege Account**:
   - In Windows workstation editions (10/11 Pro & Enterprise), members of the local `Users` group are granted `SeShutdownPrivilege` ("Shut down the system") by default.
   - Standard accounts avoid the Windows OpenSSH quirk where members of `Administrators` are forced to use `C:\ProgramData\ssh\administrators_authorized_keys`.
3. **Avoid Folder Collision (The Profile Rule)**:
   - **Never manually create `C:\Users\UPS-shutdown` before the user logs in.**
   - If you pre-create the folder, Windows detects a collision during first login and names the active profile `C:\Users\UPS-shutdown.<COMPUTERNAME>`.
   - Logging in once with password initializes the official Windows profile path cleanly.
4. **Strict NTFS ACLs & ASCII Encoding**:
   - OpenSSH strictly enforces permissions: inheritance must be stripped, and access must be restricted to `UPS-shutdown`, `SYSTEM`, and `Administrators`.
   - Keys must be written in pure ASCII (or UTF-8 without BOM) to avoid parser errors.

---

## Step 1: Install and Enable OpenSSH Server

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

## Step 2: Create the Local `UPS-shutdown` User

Run in **PowerShell (as Administrator)**:

```powershell
# Prompt for password
$Password = Read-Host -AsSecureString "Enter password for local user UPS-shutdown"

# Create local user with password that never expires
New-LocalUser -Name "UPS-shutdown" `
              -Password $Password `
              -PasswordNeverExpires `
              -Description "Automated UPS shutdown service account"
```

> [!IMPORTANT]
> Do **NOT** add this user to the `Administrators` group.
> Do **NOT** manually create `C:\Users\UPS-shutdown` yet.

---

## Step 3: Initialize the User Profile (Crucial Step)

Windows does not register a user's profile in the registry (`ProfileList`) until the account logs in for the first time.

From your **Linux bridge terminal**, initiate a one-time login using password:

```bash
ssh UPS-shutdown@<WORKSTATION-IP> "whoami"
```

Type the password you set in Step 2. When it outputs `<COMPUTERNAME>\ups-shutdown`, the Windows profile has been officially registered.

---

## Step 4: Configure `authorized_keys` in the Active Profile

Now that the profile exists, detect its exact path dynamically and install the public key.

On the Windows workstation, run in **PowerShell (as Administrator)**:

```powershell
# 1. Dynamically locate the official profile folder
$ProfilePath = (Get-ItemProperty 'HKLM:\SOFTWARE\Microsoft\Windows NT\CurrentVersion\ProfileList\*' | 
    Where-Object { $_.ProfileImagePath -like "*UPS-shutdown*" }).ProfileImagePath

if (-not $ProfilePath) {
    Write-Error "User profile not found! Make sure Step 3 (login once with password) succeeded."
    return
}

Write-Host "Configuring SSH key in: $ProfilePath\.ssh" -ForegroundColor Cyan

$SshDir = "$ProfilePath\.ssh"
$AuthKeys = "$SshDir\authorized_keys"

# 2. Create .ssh directory if needed
if (-not (Test-Path $SshDir)) {
    New-Item -ItemType Directory -Path $SshDir -Force | Out-Null
}

# 3. Paste your public key string below:
$PublicKey = "ssh-rsa AAAA... modbus-ups-bridge-ws"

# Write key in pure ASCII without BOM
[System.IO.File]::WriteAllText($AuthKeys, "$($PublicKey.Trim())`n", [System.Text.Encoding]::ASCII)

# 4. Apply strict NTFS permissions (Disable inheritance, allow User, SYSTEM, Administrators)
icacls.exe $SshDir /inheritance:r
icacls.exe $SshDir /grant:r "UPS-shutdown:(OI)(CI)F"
icacls.exe $SshDir /grant:r "SYSTEM:(OI)(CI)F"
icacls.exe $SshDir /grant:r "Administrators:(OI)(CI)F"

icacls.exe $AuthKeys /inheritance:r
icacls.exe $AuthKeys /grant:r "UPS-shutdown:F"
icacls.exe $AuthKeys /grant:r "SYSTEM:F"
icacls.exe $AuthKeys /grant:r "Administrators:F"

# 5. Set owner to the local user
icacls.exe $SshDir /setowner "UPS-shutdown"
icacls.exe $AuthKeys /setowner "UPS-shutdown"

Write-Host "===> Public key and ACLs successfully installed!" -ForegroundColor Green
```

---

## Step 5: Verify User Rights Assignment (Shutdown Privilege)

1. Open **Local Security Policy** (`secpol.msc`).
2. Navigate to: **Security Settings** → **Local Policies** → **User Rights Assignment**.
3. Check the policy **Shut down the system**:
   - By default, it contains `Administrators` and `Users`.
   - If your domain Group Policy (GPO) restricts this, explicitly add the local user `UPS-shutdown`.

---

## Step 6: Test Public Key Authentication from Linux

From your **Linux bridge terminal**, test key-based login.

> [!NOTE]
> Because private keys in `/etc/modbus-ups-bridge/` are owned by `root` with `0600` permissions, run test commands with `sudo`:

```bash
sudo ssh -i /etc/modbus-ups-bridge/workstation_key UPS-shutdown@<WORKSTATION-IP> "whoami"
```

**Expected output:**
```text
<COMPUTERNAME>\ups-shutdown
```
*(Connected without prompting for any Windows password!)*

---

## Step 7: Lock Down with `ForceCommand` (Production Hardening)

To ensure the private key cannot be used for an interactive shell or arbitrary commands, restrict the key in `C:\ProgramData\ssh\sshd_config` on the workstation:

1. Open `C:\ProgramData\ssh\sshd_config` (Notepad as Administrator).
2. Append to the **very end** of the file:

```text
Match User ups-shutdown
    ForceCommand shutdown /s /t 60 /c "Inverter battery low, grid still down -- shutting down."
```

3. Restart the SSH service in PowerShell:
```powershell
Restart-Service sshd
```

> [!IMPORTANT]
> `ForceCommand` overrides whatever command the client sends. Any SSH connection using this user will initiate a 60-second shutdown countdown.

---

## Step 8: Configure in `bridge.toml`

Add the workstation endpoint to your `bridge.toml`:

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
| `Load key ...: Permission denied` | Linux file permissions on private key | Use `sudo ssh` or ensure the bridge service user has read permissions on `/etc/modbus-ups-bridge/workstation_key`. |
| `Failed publickey ...` (password prompt) | Profile folder collision (`.BOBEV_REF` or `.000`) | Query `$ProfilePath` from registry (Step 4) and ensure `.ssh\authorized_keys` is in the active folder, not an orphaned pre-created folder. |
| `Failed publickey ...` | UTF-16 encoding with BOM | Save key using `[System.IO.File]::WriteAllText` with `[System.Text.Encoding]::ASCII`. |
| `Failed publickey ...` | User added to `Administrators` | Windows OpenSSH forces admin keys to `C:\ProgramData\ssh\administrators_authorized_keys`. Remove `UPS-shutdown` from `Administrators`. |
| `Access is denied (5)` on `shutdown` | Missing shutdown privilege | Check Local Security Policy (`secpol.msc` → *Shut down the system*) and grant it to `UPS-shutdown`. |
| Diagnostic debug log | Need exact server error | On Windows: `Stop-Service sshd` then `& "$env:SystemRoot\System32\OpenSSH\sshd.exe" -d`. |
