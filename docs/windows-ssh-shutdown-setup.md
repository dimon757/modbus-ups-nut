# Windows Workstation SSH Setup for Automated UPS Shutdown

This guide details how to configure a domain-joined or standalone Windows 10/11 workstation to allow `modbus-ups-bridge` (or any UPS monitor) to initiate an automated graceful shutdown over SSH using public-key authentication.

---

## Architecture & Security Principles

1. **Dedicated Local Account (`UPS-shutdown`)**:
   - Creating a **local** account on the workstation (rather than an Active Directory domain account) ensures shutdown commands succeed even when domain controllers are unreachable or already offline during a power outage.
2. **Low-Privilege Account with Remote Shutdown Rights**:
   - Standard accounts avoid the Windows OpenSSH quirk where members of `Administrators` are forced to use `C:\ProgramData\ssh\administrators_authorized_keys`.
   - Because OpenSSH sessions operate under a **network logon context**, Windows checks **`SeRemoteShutdownPrivilege` ("Force shutdown from a remote system")** rather than local `SeShutdownPrivilege`.
   - By default, standard users do not possess `SeRemoteShutdownPrivilege`. Granting this specific right explicitly in Step 2 allows graceful shutdown over SSH while keeping the account otherwise unprivileged.
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

## Step 2: Create the Local `UPS-shutdown` User & Grant Remote Shutdown Right (Windows)

Run in **PowerShell (as Administrator)**:

```powershell
# 1. Prompt for a secure password and create user
$Password = Read-Host -AsSecureString "Enter password for UPS-shutdown"

New-LocalUser -Name "UPS-shutdown" `
              -Password $Password `
              -PasswordNeverExpires `
              -Description "Automated UPS shutdown account"

# 2. Grant 'Force shutdown from a remote system' (SeRemoteShutdownPrivilege)
# Required because OpenSSH executes as a network logon session
$ExportFile = "$env:TEMP\secpol.inf"
$DatabaseFile = "$env:TEMP\secpol.sdb"

secedit.exe /export /cfg "$ExportFile" /areas USER_RIGHTS
$userSid = (Get-LocalUser -Name "UPS-shutdown").SID.Value

$content = Get-Content "$ExportFile"
if ($content -match '^SeRemoteShutdownPrivilege\s*=') {
    $content = $content -replace '^(SeRemoteShutdownPrivilege\s*=.*)', "`$1,*$userSid"
} else {
    $content = $content -replace '(\[Privilege Rights\])', "`$1`r`nSeRemoteShutdownPrivilege = *$userSid"
}

if ($content -match '^SeShutdownPrivilege\s*=') {
    $content = $content -replace '^(SeShutdownPrivilege\s*=.*)', "`$1,*$userSid"
} else {
    $content = $content -replace '(\[Privilege Rights\])', "`$1`r`nSeShutdownPrivilege = *$userSid"
}

$content | Set-Content "$ExportFile" -Encoding Unicode

secedit.exe /import /cfg "$ExportFile" /db "$DatabaseFile" /overwrite
secedit.exe /configure /db "$DatabaseFile" /cfg "$ExportFile" /areas USER_RIGHTS

Remove-Item -LiteralPath $ExportFile -Force -ErrorAction SilentlyContinue
Remove-Item -LiteralPath $DatabaseFile -Force -ErrorAction SilentlyContinue

Write-Host "===> User created and Remote Shutdown privilege assigned!" -ForegroundColor Green
```

*(Alternatively via GUI: open `secpol.msc` → **Security Settings** → **Local Policies** → **User Rights Assignment** → double-click **"Force shutdown from a remote system"** → add `UPS-shutdown`).*

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

# 2. Grant Administrators access and take ownership if needed
takeown.exe /F $SshDir /R /D Y
icacls.exe $SshDir /grant "Administrators:(OI)(CI)F" /T /C

# 3. Strip inheritance and grant exclusive access atomically
icacls.exe $SshDir /inheritance:r /grant "Administrators:(OI)(CI)F" /grant "SYSTEM:(OI)(CI)F" /grant "UPS-shutdown:(OI)(CI)F"
icacls.exe $AuthKeys /inheritance:r /grant "Administrators:F" /grant "SYSTEM:F" /grant "UPS-shutdown:F"

# 4. Set owner back to UPS-shutdown
icacls.exe $SshDir /setowner "UPS-shutdown" /T /C
icacls.exe $AuthKeys /setowner "UPS-shutdown"

Write-Host "===> Permissions configured successfully!" -ForegroundColor Green
```

### Verify Configured ACLs

Verify that only `UPS-shutdown`, `SYSTEM`, and `Administrators` have access:

```powershell
icacls.exe "$SshDir"
icacls.exe "$AuthKeys"
```

**Expected output:**
```text
C:\Users\ups-shutdown\.ssh <ADMIN_OR_DOMAIN_USER>:(OI)(IO)(F)
                           <COMPUTERNAME>\UPS-shutdown:(OI)(CI)(F)
                           NT AUTHORITY\SYSTEM:(OI)(CI)(F)
                           BUILTIN\Administrators:(OI)(CI)(F)

Successfully processed 1 files; Failed processing 0 files

C:\Users\ups-shutdown\.ssh\authorized_keys <COMPUTERNAME>\UPS-shutdown:(F)
                                           NT AUTHORITY\SYSTEM:(F)
                                           BUILTIN\Administrators:(F)

Successfully processed 1 files; Failed processing 0 files
```

---

## Step 5: Test Key-Based Login & Remote Shutdown (Linux Bridge)

From the **Linux bridge terminal**, test the connection using the private key:

### 1. Test basic SSH connectivity:
```bash
sudo ssh -i /etc/modbus-ups-bridge/workstation_key UPS-shutdown@<WORKSTATION-IP> "whoami"
```

**Expected output:**
```text
<COMPUTERNAME>\ups-shutdown
```
*(Logs in immediately without asking for a Windows password!)*

### 2. Test the actual shutdown command:
Simulate the exact command that `modbus-ups-bridge` sends:
```bash
sudo ssh -i /etc/modbus-ups-bridge/workstation_key \
  -o BatchMode=yes \
  -o StrictHostKeyChecking=yes \
  -o ConnectTimeout=10 \
  UPS-shutdown@<WORKSTATION-IP> \
  'shutdown /s /t 60 /c "Inverter battery low, grid still down -- shutting down."'
```

On the Windows workstation, a dialog will pop up announcing the 60-second shutdown countdown.

> [!TIP]
> **Abort the test countdown on Windows:**
> Open PowerShell or CMD on the Windows workstation and run:
> ```powershell
> shutdown /a
> ```
> Windows will display a notification: *"Logoff is cancelled. The scheduled shutdown has been cancelled."*

---

## Step 6: Hardening with `ForceCommand` (Windows)

To guarantee that a compromised key can only execute the shutdown command and cannot open an interactive shell, lock it down in `C:\ProgramData\ssh\sshd_config`.

Run in **PowerShell (as Administrator)**:

```powershell
$cfg = "$env:ProgramData\ssh\sshd_config"

# 1. Append the lockdown block (using an array prevents terminal line-wrapping corruption)
@(
    "",
    "Match User ups-shutdown",
    '    ForceCommand shutdown /s /t 60 /c "Inverter battery low, grid still down -- shutting down."'
) | Add-Content -Path $cfg -Encoding ascii

# 2. Validate configuration syntax before restarting (returns blank if valid)
& "$env:SystemRoot\System32\OpenSSH\sshd.exe" -t

# 3. Restart OpenSSH service
Restart-Service sshd
```

*(Alternatively, you can manually open `C:\ProgramData\ssh\sshd_config` in Notepad as Administrator and paste the block at the **very end** of the file, ensuring the `ForceCommand` instruction is completely on a single line).*

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
| `Access is denied (5)` on `shutdown` | Missing `SeRemoteShutdownPrivilege` (OpenSSH runs as a network logon) | Grant "Force shutdown from a remote system" via `secpol.msc` or run the `secedit` PowerShell script in Step 2. |
| Host key verification prompt | Workstation host key not in known hosts | Scan and pin the workstation host key: `ssh-keyscan -H <WORKSTATION-IP> \| sudo tee -a /root/.ssh/known_hosts`. |
| `icacls: Access is denied` during Step 4 | Stripping inheritance locked out Administrator | Use `takeown.exe /F $SshDir /R /D Y` followed by `icacls.exe $SshDir /grant "Administrators:(OI)(CI)F" /T /C`, and supply `/inheritance:r` alongside `/grant` in one command. |
| `Failed to start service sshd` after Step 6 | Syntax error or broken multi-line in `sshd_config` | Run `& "$env:SystemRoot\System32\OpenSSH\sshd.exe" -t` to pinpoint the error. Ensure the `ForceCommand ... /c "..."` statement is on a single unbroken line. |

---

## Teardown & Complete Removal (Windows)

To completely remove the `UPS-shutdown` account, profile, and SSH lockdown from a workstation, run the following in **PowerShell (as Administrator)**:

```powershell
# 1. Revert sshd_config and restart SSH server
$sshdConfig = "$env:ProgramData\ssh\sshd_config"
if (Test-Path $sshdConfig) {
    $content = Get-Content -Path $sshdConfig -Raw
    $pattern = "(?ims)[\r\n]*Match\s+User\s+ups-shutdown\b.*?(?=[\r\n]+Match\b|\Z)"
    if ($content -match "(?i)Match\s+User\s+ups-shutdown") {
        $updated = [regex]::Replace($content, $pattern, "")
        Set-Content -Path $sshdConfig -Value $updated.TrimEnd() -NoNewline
        Restart-Service sshd -Force -ErrorAction SilentlyContinue
    }
}

# 2. Cleanly delete user profile and registry entries
Get-CimInstance -ClassName Win32_UserProfile | 
    Where-Object { $_.LocalPath -like "*ups-shutdown*" } | 
    Remove-CimInstance

# 3. Clean up any leftover profile directories in C:\Users
Get-ChildItem -Path "$env:SystemDrive\Users" -Directory -Filter "*ups-shutdown*" -ErrorAction SilentlyContinue |
    Remove-Item -Recurse -Force -ErrorAction SilentlyContinue

# 4. Remove local Windows user account
Remove-LocalUser -Name "UPS-shutdown" -ErrorAction SilentlyContinue
```

Alternatively, you can run the standalone [remove-ups-shutdown.ps1](../remove-ups-shutdown.ps1) script:
```powershell
powershell -ExecutionPolicy Bypass -File .\remove-ups-shutdown.ps1
```

