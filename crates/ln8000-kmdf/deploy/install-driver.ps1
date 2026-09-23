# install-driver.ps1 - install the LN8000 charge pump driver on the nabu tablet
#
# Run: PowerShell as administrator on the tablet, from the folder with the package:
#     .\install-driver.ps1
#
# What it does:
#   1) checks that test signing of drivers is enabled (the driver is signed with
#      the WDK test certificate);
#   2) installs the package through pnputil;
#   3) checks that the ACPI\QCOM057E device got the ln8000_kmdf driver;
#   4) starts the service and prints the state through the diagnostic tool.
#
# Rollback: .\uninstall-driver.ps1

[CmdletBinding()]
param(
  [string]$PackageDir = $PSScriptRoot,
  [switch]$SkipSignatureCheck
)

$ErrorActionPreference = 'Stop'
$service = 'ln8000_kmdf'
$hwid = 'ACPI\QCOM057E'

function Assert-Admin {
  $identity = [Security.Principal.WindowsIdentity]::GetCurrent()
  $principal = New-Object Security.Principal.WindowsPrincipal($identity)
  if (-not $principal.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)) {
    throw 'administrator rights are required: restart PowerShell as administrator'
  }
}

function Get-TestSigning {
  $out = & bcdedit /enum '{current}' 2>&1 | Out-String
  return ($out -match 'testsigning\s+Yes')
}

# Secure Boot ignores the testsigning setting, so a test-signed package cannot load with
# it on. `Confirm-SecureBootUEFI` throws where the firmware is not UEFI (not this tablet),
# so $null means "cannot tell" and the check stays silent rather than guessing.
function Get-SecureBoot {
  try { return [bool](Confirm-SecureBootUEFI) } catch { return $null }
}

Assert-Admin

$inf = Join-Path $PackageDir 'ln8000_kmdf.inf'
if (-not (Test-Path -LiteralPath $inf)) {
  throw "not found: $inf - put the driver package (inf, sys, cat) next to the script"
}

Write-Host '=== 1. signature check ===' -ForegroundColor Cyan
if (-not $SkipSignatureCheck) {
  if (Get-TestSigning) {
    Write-Host '  test signing is enabled (testsigning Yes)' -ForegroundColor Green
  } else {
    Write-Host '  test signing is DISABLED.' -ForegroundColor Yellow
    Write-Host '  The driver is signed with the WDK test certificate, so enable the mode:' -ForegroundColor Yellow
    Write-Host '      bcdedit /set testsigning on' -ForegroundColor Yellow
    Write-Host '  and reboot the tablet, then repeat the installation.' -ForegroundColor Yellow
    throw 'test signing is disabled'
  }
  # Secure Boot ignores the testsigning setting entirely, so a test-signed package is
  # refused with it on even when the mode above reads Yes.
  if ((Get-SecureBoot) -eq $true) {
    Write-Host '  Secure Boot is ENABLED.' -ForegroundColor Yellow
    Write-Host '  A test-signed driver cannot load while Secure Boot is on:' -ForegroundColor Yellow
    Write-Host '  turn it off in the UEFI settings and repeat the installation.' -ForegroundColor Yellow
    throw 'Secure Boot is enabled'
  }
}

Write-Host '=== 2. installing the package ===' -ForegroundColor Cyan

# The WDK test certificate must be in the trusted stores, otherwise pnputil
# fails with 0x800B0109 (CERT_E_UNTRUSTEDROOT): test signing alone is not enough.
$cer = Join-Path $PackageDir 'WDRLocalTestCert.cer'
if (Test-Path -LiteralPath $cer) {
  $thumb = (Get-PfxCertificate -FilePath $cer).Thumbprint
  foreach ($store in @('Root', 'TrustedPublisher')) {
    $present = Get-ChildItem "Cert:\LocalMachine\$store" -ErrorAction SilentlyContinue |
      Where-Object { $_.Thumbprint -eq $thumb }
    if ($present) {
      Write-Host "  certificate already in $store" -ForegroundColor DarkGray
    } else {
      Import-Certificate -FilePath $cer -CertStoreLocation "Cert:\LocalMachine\$store" | Out-Null
      Write-Host "  certificate added to $store" -ForegroundColor Green
    }
  }
} else {
  Write-Host '  no WDRLocalTestCert.cer nearby - assuming the certificate is already installed' -ForegroundColor DarkGray
}

& pnputil /add-driver $inf /install
$code = $LASTEXITCODE
# 3010 - the package is installed, a reboot is needed; 1641 - a reboot is already pending.
# These are success codes, not an error.
if ($code -notin @(0, 3010, 1641)) { throw "pnputil returned code $code" }
if ($code -ne 0) {
  Write-Host "  package installed, a reboot is needed (code $code)" -ForegroundColor Yellow
}

Write-Host '=== 3. driver binding check ===' -ForegroundColor Cyan
$device = Get-PnpDevice -PresentOnly | Where-Object { $_.InstanceId -like "*$hwid*" } | Select-Object -First 1
if (-not $device) {
  Write-Host "  device $hwid not found" -ForegroundColor Red
  Write-Host '  check that a UEFI image with the PEIC node is loaded' -ForegroundColor Yellow
  Write-Host '  (the release archive ships INSTALL.md; the repository has docs/DEPLOY-LN8000.md)' -ForegroundColor Yellow
} else {
  $driverService = (Get-PnpDeviceProperty -InstanceId $device.InstanceId -KeyName 'DEVPKEY_Device_Service' -ErrorAction SilentlyContinue).Data
  Write-Host ("  device  : " + $device.InstanceId)
  Write-Host ("  status  : " + $device.Status)
  Write-Host ("  service : " + $driverService)
  if ($driverService -ne $service) {
    Write-Host "  expected service $service - check that the INF matched this _HID" -ForegroundColor Yellow
  }
}

Write-Host '=== 4. starting the service ===' -ForegroundColor Cyan
& sc.exe start $service 2>&1 | ForEach-Object { "  $_" }

Write-Host '=== 5. device state ===' -ForegroundColor Cyan
$diag = Join-Path $PackageDir 'nabu-ln8000.ps1'
if (Test-Path -LiteralPath $diag) {
  & $diag status
} else {
  Write-Host '  diagnostic tool nabu-ln8000.ps1 not found next to the script' -ForegroundColor Yellow
}

Write-Host ''
Write-Host 'Installation complete. Next:' -ForegroundColor Green
Write-Host '  .\nabu-ln8000.ps1 status      - mode and telemetry'
Write-Host '  .\nabu-ln8000.ps1 sessions    - charge sessions'
Write-Host '  .\nabu-ln8000.ps1 journal out.jsonl - export the journal'
