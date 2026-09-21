# update-driver.ps1 - update the LN8000 driver over a previous version
#
# Run as administrator on the tablet from the folder with the new package:
#     .\update-driver.ps1
#
# The update is safe: first the installed package version is saved, then the new
# one is installed. If the device does not come up after the install, the script
# reports it and shows how to roll back (uninstall + install of the old one).

[CmdletBinding()]
param(
  [string]$PackageDir = $PSScriptRoot,
  [string]$BackupDir = "$env:ProgramData\nabu-fastcharge\backup"
)

$ErrorActionPreference = 'Stop'
$inf = Join-Path $PackageDir 'ln8000_kmdf.inf'
if (-not (Test-Path -LiteralPath $inf)) { throw "not found: $inf" }

$identity = [Security.Principal.WindowsIdentity]::GetCurrent()
$principal = New-Object Security.Principal.WindowsPrincipal($identity)
if (-not $principal.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)) {
  throw 'administrator rights are required'
}

$version = (Get-Item -LiteralPath (Join-Path $PackageDir 'ln8000_kmdf.sys')).VersionInfo.FileVersion
Write-Host ("=== updating to version " + $version) -ForegroundColor Cyan

# 1. Save the currently installed package - this is the rollback point.
New-Item -ItemType Directory -Path $BackupDir -Force | Out-Null
$published = & pnputil /enum-drivers | Out-String
$blocks = $published -split "`r?`n`r?`n"
foreach ($block in $blocks) {
  if ($block -match 'ln8000_kmdf\.inf' -and $block -match 'oem\d+\.inf') {
    $oem = $Matches[0]
    & pnputil /export-driver $oem $BackupDir 2>&1 | ForEach-Object { "  $_" }
    Write-Host ("  saved $oem to $BackupDir") -ForegroundColor Green
  }
}

# 2. Install the new package.
Write-Host '=== installing the new package ===' -ForegroundColor Cyan
& pnputil /add-driver $inf /install
if ($LASTEXITCODE -ne 0) { throw "pnputil returned code $LASTEXITCODE" }

# 3. Restart the service so the driver re-reads its configuration.
& sc.exe stop  ln8000_kmdf | Out-Null
& sc.exe start ln8000_kmdf | ForEach-Object { "  $_" }

# 4. Check that the device is alive.
Write-Host '=== check ===' -ForegroundColor Cyan
$device = Get-PnpDevice -PresentOnly | Where-Object { $_.InstanceId -like '*ACPI\QCOM057E*' } | Select-Object -First 1
if ($device) {
  Write-Host ("  status: " + $device.Status)
  if ($device.Status -ne 'OK') {
    Write-Host '  the device is not in the OK state.' -ForegroundColor Yellow
    Write-Host "  rollback: .\uninstall-driver.ps1, then pnputil /add-driver $BackupDir\ln8000_kmdf.inf /install" -ForegroundColor Yellow
  } else {
    Write-Host '  the update succeeded' -ForegroundColor Green
  }
} else {
  Write-Host '  PEIC device not found - check the UEFI boot' -ForegroundColor Yellow
}
