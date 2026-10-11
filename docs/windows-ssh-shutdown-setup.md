# Windows Workstation SSH Setup for Automated UPS Shutdown

This guide details how to configure a domain-joined or standalone Windows 10/11 workstation to allow `modbus-ups-bridge` (or any UPS monitor) to initiate an automated graceful shutdown over SSH using public-key authentication.

---

## Architecture & Security Principles

1. **Dedicated Local Account (`UPS-shutdown`)**:
   - Creating a **local** user on the workstation (rather than a domain user) ensures shutdown commands can be executed during a power outage even if the Active Directory Domain Controllers are unreachable or already offline.
2. **Low-Privilege Account**:
   - In Windows workstation editions (10/11 Pro & Enterprise), members of the local `Users` group are granted `SeShutdownPrivilege` ("Shut down the system") by default.
   - Avoiding the `Administrators` group keeps the attack surface minimal and avoids Windows OpenSSH redirecting public keys to `C:\ProgramData\ssh\administrators_authorized_keys`.
3. **Restricted Command (`ForceCommand`)**:
   - OpenSSH can be locked down so that logging in with the UPS key can only execute the shutdown command, preventing arbitrary remote shell access.

---

## Step 1: Install and Enable OpenSSH Server

Open **PowerShell as Administrator** on the Windows workstation:

```powershell
# 1. Install OpenSSH Server capability
Add-WindowsCapability -Online -Name OpenSSH.Server~~~~0.0.1.0

# 2. Set sshd service to automatic startup and start it
Set-Service -Name sshd -StartupType Automatic
Start-Service sshd

# 3. Confirm firewall rule is active (port 22)
Get-NetFirewallRule -Name *OpenSSH-Server* | Select-Object Name, Enabled, Direction, Action
```

---

## Step 2: Create the Local `UPS-shutdown` User

Run in PowerShell (as Administrator):

```powershell
# Prompt for a secure password
$Password = Read-Host -AsSecureString "Enter password for local user UPS-shutdown"

# Create local user with password that never expires
New-LocalUser -Name "UPS-shutdown" `
              -Password $Password `
              -PasswordNeverExpires `
              -Description "Automated UPS shutdown service account"
```

> [!NOTE]
> Do **not** add this user to the `Administrators` group. A standard user account is safer and simplifies key handling in Windows OpenSSH.

---

## Step 3: Configure `.ssh` and `authorized_keys` with Strict ACLs

Windows OpenSSH strictly checks NTFS Access Control Lists (ACLs). If permissions inherit from parent directories or allow write access to `Users` or `Authenticated Users`, **OpenSSH will reject the key**.

Run the following script to create the files and set strict permissions:

```powershell
$User = "UPS-shutdown"
$UserHome = "C:\Users\$User"
$SshDir = "$UserHome\.ssh"
$AuthKeys = "$SshDir\authorized_keys"

# 1. Create home directory and .ssh directory if they do not exist
if (-not (Test-Path $UserHome)) {
    New-Item -ItemType Directory -Path $UserHome -Force | Out-Null
}
if (-not (Test-Path $SshDir)) {
    New-Item -ItemType Directory -Path $SshDir -Force | Out-Null
}

# 2. Insert public key (replace with your actual public key string)
$PublicKey = "ssh-ed25519 AAAA... user@ups-controller"
Set-Content -Path $AuthKeys -Value $PublicKey -Encoding utf8 -Force

# 3. Strip inherited permissions and grant explicit access only to the user, SYSTEM, and Administrators
icacls.exe $SshDir /inheritance:r
icacls.exe $SshDir /grant:r "$($User):(OI)(CI)F"
icacls.exe $SshDir /grant:r "SYSTEM:(OI)(CI)F"
icacls.exe $SshDir /grant:r "Administrators:(OI)(CI)F"

icacls.exe $AuthKeys /inheritance:r
icacls.exe $AuthKeys /grant:r "$($User):F"
icacls.exe $AuthKeys /grant:r "SYSTEM:F"
icacls.exe $AuthKeys /grant:r "Administrators:F"
```

---

## Step 4: Verify Shutdown User Rights Assignment

Ensure the user is permitted to shut down the workstation:

1. Open **Local Security Policy** (`secpol.msc`).
2. Navigate to: **Security Settings** → **Local Policies** → **User Rights Assignment**.
3. Check the policy **Shut down the system**:
   - By default, it contains `Administrators` and `Users`.
   - If your domain Group Policy (GPO) restricts this, explicitly add the local user `UPS-shutdown`.

---

## Step 5: Lock Down with `ForceCommand` (Recommended)

To ensure the private key cannot be used to run arbitrary commands or obtain an interactive shell, lock it down in `C:\ProgramData\ssh\sshd_config`.

1. Open `C:\ProgramData\ssh\sshd_config` in a text editor (e.g. Notepad as Administrator).
2. Append the following block to the **very end** of the file (after any existing `Match` blocks):

```text
Match User ups-shutdown
    ForceCommand shutdown /s /t 60 /c "Inverter battery low, grid still down -- shutting down."
```

3. Restart the SSH service:
```powershell
Restart-Service sshd
```

> [!IMPORTANT]
> `ForceCommand` replaces whatever command the client requests over SSH. When configured, the countdown specified here (`/t 60`) governs the shutdown behavior.

---

## Step 6: Testing & Verification

Run these verification tests from the client device (UPS controller / Linux bridge host) holding the private key:

### 1. Test basic connectivity / whoami
*(Only applicable if `ForceCommand` is not yet configured or for preliminary test)*:

```bash
# Standard lookup:
ssh -i /path/to/private_key UPS-shutdown@<WORKSTATION-IP> "whoami"

# If the domain workstation requires local qualification:
ssh -i /path/to/private_key "WORKSTATION\UPS-shutdown"@<WORKSTATION-IP> "whoami"
```
*Expected output:* `workstation\ups-shutdown`

### 2. Test Graceful Shutdown (Abortable Test)
Trigger a shutdown with a 60-second delay so you have plenty of time to cancel:

```bash
ssh -i /path/to/private_key UPS-shutdown@<WORKSTATION-IP> "shutdown /s /t 60 /c \"UPS Shutdown Test\""
```

A Windows alert dialog will appear on the workstation: *"Inverter battery low / UPS Shutdown Test. Windows will shut down in 60 seconds."*

**Abort the test shutdown immediately:**
```bash
ssh -i /path/to/private_key UPS-shutdown@<WORKSTATION-IP> "shutdown /a"
```
A popup will notify that the logoff/shutdown was cancelled.

### 3. Verify bridge configuration (`bridge.toml`)
Ensure the endpoint in your `bridge.toml` matches:

```toml
[[endpoints]]
name = "workstation-1"
kind = "windows"
host = "192.168.1.150"
ssh_user = "UPS-shutdown"
ssh_key_path = "/etc/modbus-ups-bridge/workstation_key"
shutdown_delay_secs = 60
mac_address = "AA:BB:CC:DD:EE:FF"
```

---

## Troubleshooting

| Symptom | Cause | Solution |
|---|---|---|
| `Permission denied (publickey)` | File permissions too open on `.ssh` or `authorized_keys` | Re-run the `icacls` commands from Step 3 to disable inheritance and strip non-owner permissions. |
| `Permission denied (publickey)` | User is in `Administrators` group | Windows OpenSSH redirects admin keys to `C:\ProgramData\ssh\administrators_authorized_keys`. Remove `UPS-shutdown` from `Administrators` or edit `sshd_config` to comment out `AuthorizedKeysFile __PROGRAMDATA__/...`. |
| `Access is denied (5)` on `shutdown` | Missing shutdown privilege | Check Local Security Policy (`secpol.msc` → User Rights Assignment → *Shut down the system*) and add `UPS-shutdown`. |
| Host key verification failure | Bridge cannot verify workstation host key | Scan and store the host key on the bridge: `ssh-keyscan -H <WORKSTATION-IP> >> /root/.ssh/known_hosts`. |
| OpenSSH silent failure | Configuration or key format issue | On the Windows workstation, inspect **Event Viewer** → **Applications and Services Logs** → **OpenSSH** → **Operational**. |
