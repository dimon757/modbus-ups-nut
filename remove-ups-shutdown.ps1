# ==============================================================================
# remove-ups-shutdown.ps1
# Completely remove the UPS-shutdown account, profile, and SSH lockdown config
# Run in PowerShell as Administrator on the target Windows workstation.
# ==============================================================================

# Ensure script is run with elevated privileges
$currentPrincipal = New-Object Security.Principal.WindowsPrincipal([Security.Principal.WindowsIdentity]::GetCurrent())
if (-not $currentPrincipal.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)) {
    Write-Error "This script must be executed in an elevated PowerShell session (Run as Administrator)."
    exit 1
}

Write-Host "==========================================================" -ForegroundColor Cyan
Write-Host " Starting complete cleanup of 'ups-shutdown' components..." -ForegroundColor Cyan
Write-Host "==========================================================" -ForegroundColor Cyan

# ------------------------------------------------------------------------------
# 1. Revert sshd_config ForceCommand / Match User block
# ------------------------------------------------------------------------------
$sshdConfigPath = "$env:ProgramData\ssh\sshd_config"
if (Test-Path $sshdConfigPath) {
    Write-Host "`n[1/4] Checking $sshdConfigPath..." -ForegroundColor Yellow
    $configContent = Get-Content -Path $sshdConfigPath -Raw
    
    # Matches 'Match User ups-shutdown' up until the next 'Match' block or end of file
    $pattern = "(?ims)[\r\n]*Match\s+User\s+ups-shutdown\b.*?(?=[\r\n]+Match\b|\Z)"
    if ($configContent -match "(?i)Match\s+User\s+ups-shutdown") {
        $updatedConfig = [regex]::Replace($configContent, $pattern, "")
        Set-Content -Path $sshdConfigPath -Value $updatedConfig.TrimEnd() -NoNewline
        Write-Host "  -> Removed 'Match User ups-shutdown' block from sshd_config." -ForegroundColor Green
        
        $sshdService = Get-Service -Name sshd -ErrorAction SilentlyContinue
        if ($sshdService -and $sshdService.Status -eq 'Running') {
            Restart-Service -Name sshd -Force
            Write-Host "  -> Restarted OpenSSH SSH Server (sshd) service." -ForegroundColor Green
        }
    } else {
        Write-Host "  -> No 'Match User ups-shutdown' block found in sshd_config." -ForegroundColor Gray
    }
} else {
    Write-Host "`n[1/4] sshd_config not found at $sshdConfigPath (skipped)." -ForegroundColor Gray
}

# ------------------------------------------------------------------------------
# 2. Remove User Profile cleanly from WMI / Registry ProfileList
# ------------------------------------------------------------------------------
Write-Host "`n[2/4] Checking for registered user profiles matching 'ups-shutdown'..." -ForegroundColor Yellow
$userProfiles = Get-CimInstance -ClassName Win32_UserProfile | Where-Object { $_.LocalPath -like "*ups-shutdown*" }
if ($userProfiles) {
    foreach ($profile in $userProfiles) {
        Write-Host "  -> Deleting user profile at $($profile.LocalPath)..." -ForegroundColor Cyan
        Remove-CimInstance -InputObject $profile
        Write-Host "  -> User profile $($profile.LocalPath) deleted." -ForegroundColor Green
    }
} else {
    Write-Host "  -> No active profile registry entry found for ups-shutdown." -ForegroundColor Gray
}

# ------------------------------------------------------------------------------
# 3. Clean up any leftover profile folders on disk in C:\Users
# ------------------------------------------------------------------------------
Write-Host "`n[3/4] Checking for orphaned directories in C:\Users..." -ForegroundColor Yellow
$leftoverDirs = Get-ChildItem -Path "$env:SystemDrive\Users" -Directory -Filter "*ups-shutdown*" -ErrorAction SilentlyContinue
if ($leftoverDirs) {
    foreach ($dir in $leftoverDirs) {
        Write-Host "  -> Removing folder: $($dir.FullName)..." -ForegroundColor Cyan
        Remove-Item -Path $dir.FullName -Recurse -Force -ErrorAction SilentlyContinue
        Write-Host "  -> Folder removed." -ForegroundColor Green
    }
} else {
    Write-Host "  -> No orphaned folders found in $env:SystemDrive\Users." -ForegroundColor Gray
}

# ------------------------------------------------------------------------------
# 4. Delete the local Windows user account
# ------------------------------------------------------------------------------
Write-Host "`n[4/4] Checking for local user 'UPS-shutdown'..." -ForegroundColor Yellow
$localUser = Get-LocalUser -Name "UPS-shutdown" -ErrorAction SilentlyContinue
if ($localUser) {
    Remove-LocalUser -Name "UPS-shutdown"
    Write-Host "  -> Local user 'UPS-shutdown' removed successfully." -ForegroundColor Green
} else {
    Write-Host "  -> Local user 'UPS-shutdown' does not exist." -ForegroundColor Gray
}

Write-Host "`n==========================================================" -ForegroundColor Green
Write-Host " Teardown completed successfully! " -ForegroundColor Green
Write-Host "==========================================================" -ForegroundColor Green
