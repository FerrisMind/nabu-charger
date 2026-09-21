# uninstall-driver.ps1 - full removal of the LN8000 driver and return to the stock state
#
# Run as administrator on the tablet:
#     .\uninstall-driver.ps1
#
# What it does:
#   1) stops the ln8000_kmdf service;
#   2) removes the driver package from the store (pnputil /delete-driver /uninstall);
#   3) checks that the ACPI\QCOM057E device is back on the stock driver
#      (or left without a driver - expected if there is no stock one);
#   4) prints what is left in the system so the rollback can be verified.
#
# The driver does not write to firmware and does not change power settings, so
# removal returns the device to its pre-install behaviour.

[CmdletBinding()]
param(
  [string]$LogPath = "$env:ProgramData\nabu-fastcharge\uninstall.log"
)

$ErrorActionPreference = 'Stop'
$service = 'ln8000_kmdf'
$hwid = 'ACPI\QCOM057E'

$identity = [Security.Principal.WindowsIdentity]::GetCurrent()
$principal = New-Object Security.Principal.WindowsPrincipal($identity)
if (-not $principal.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)) {
  throw 'administrator rights are required'
}

New-Item -ItemType Directory -Path (Split-Path $LogPath) -Force | Out-Null
Start-Transcript -Path $LogPath -Force | Out-Null

try {
  Write-Host '=== 1. stopping the service ===' -ForegroundColor Cyan
  & sc.exe stop $service 2>&1 | ForEach-Object { "  $_" }

  Write-Host '=== 2. looking for the package in the store ===' -ForegroundColor Cyan
  $published = & pnputil /enum-drivers | Out-String
  $blocks = $published -split "`r?`n`r?`n"
  $targets = @()
  foreach ($block in $blocks) {
    if ($block -match 'ln8000_kmdf\.inf') {
      if ($block -match 'oem\d+\.inf') { $targets += $Matches[0] }
    }
  }
  if ($targets.Count -eq 0) {
    Write-Host '  package ln8000_kmdf.inf not found in the store - already removed?' -ForegroundColor Yellow
  } else {
    foreach ($oem in $targets) {
      Write-Host "  removing $oem"
      & pnputil /delete-driver $oem /uninstall 2>&1 | ForEach-Object { "    $_" }
    }
  }

  Write-Host '=== 3. device state after removal ===' -ForegroundColor Cyan
  $device = Get-PnpDevice -PresentOnly | Where-Object { $_.InstanceId -like "*$hwid*" } | Select-Object -First 1
  if ($device) {
    $driverService = (Get-PnpDeviceProperty -InstanceId $device.InstanceId -KeyName 'DEVPKEY_Device_Service' -ErrorAction SilentlyContinue).Data
    Write-Host ("  device  : " + $device.InstanceId)
    Write-Host ("  status  : " + $device.Status)
    Write-Host ("  service : " + $driverService)
    if ($driverService -eq $service) {
      Write-Host '  WARNING: the device is still on our service - reboot the tablet' -ForegroundColor Yellow
    } else {
      Write-Host '  the device is back on the stock driver (or left without one)' -ForegroundColor Green
    }
  } else {
    Write-Host "  device $hwid is no longer in the system (the PEIC node is absent)" -ForegroundColor Green
  }

  Write-Host '=== 4. what is left in the system ===' -ForegroundColor Cyan
  Write-Host '  service:'
  & sc.exe query $service 2>&1 | ForEach-Object { "    $_" }
  Write-Host '  driver journal files (if any were exported):'
  if (Test-Path "$env:ProgramData\nabu-fastcharge") {
    Get-ChildItem "$env:ProgramData\nabu-fastcharge" | ForEach-Object { "    " + $_.Name }
  } else {
    Write-Host '    none'
  }

  Write-Host ''
  Write-Host 'Removal complete. Log: ' -NoNewline -ForegroundColor Green
  Write-Host $LogPath
} finally {
  Stop-Transcript | Out-Null
}
