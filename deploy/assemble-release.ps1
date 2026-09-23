# assemble-release.ps1 - assemble the installable ARM64 release archive
#
# Takes what build-arm64.ps1 produced in artifacts/ and packs it into
# artifacts/release/nabu-ln8000-driver-<version>-arm64.zip, with the LN8000 kit
# (installable) at ln8000/ and the SMB detection driver at smb-detection/.
#
# The script refuses to build an archive whose INF carries no device access
# policy or whose driver is not ARM64. Both have been true of this repository
# before, and both produce a package that installs and then misbehaves.
#
# Run:
#     .\assemble-release.ps1 -Version 0.3.0

[CmdletBinding()]
param(
  [Parameter(Mandatory = $true)]
  [string]$Version,
  [string]$RepoRoot = (Resolve-Path (Join-Path $PSScriptRoot '..')).Path,
  [switch]$KeepStaging
)

$ErrorActionPreference = 'Stop'

function Get-PeMachine([string]$Path) {
  $bytes = [IO.File]::ReadAllBytes($Path)
  $peOffset = [BitConverter]::ToInt32($bytes, 0x3C)
  return [BitConverter]::ToUInt16($bytes, $peOffset + 4)
}

# Windows PowerShell writes CRLF through Set-Content, and a CRLF checksum file is
# unusable to `sha256sum -c` on Linux and macOS: the CR ends up inside the file name and
# every line fails to open. Both checksum files this script produces are written with LF
# endings so the archive verifies on any platform.
function Write-LinesLf([string]$Path, [string[]]$Lines) {
  [IO.File]::WriteAllText($Path, (($Lines -join "`n") + "`n"), [Text.Encoding]::ASCII)
}

$ln8000Dir = Join-Path $RepoRoot 'artifacts\driver-ln8000-arm64'
$smbDir = Join-Path $RepoRoot 'artifacts\driver-arm64'
$manifestPath = Join-Path $RepoRoot 'artifacts\BUILD-MANIFEST.json'
$sumsPath = Join-Path $RepoRoot 'artifacts\SHA256SUMS.txt'

foreach ($p in @($ln8000Dir, $smbDir, $manifestPath, $sumsPath)) {
  if (-not (Test-Path $p)) {
    throw "missing $p - run deploy\build-arm64.ps1 first"
  }
}

# ---- guards: the two mistakes that have actually happened -------------------

$ln8000Inf = Join-Path $ln8000Dir 'ln8000_kmdf.inf'
$smbInf = Join-Path $smbDir 'kmdf.inf'

foreach ($inf in @($ln8000Inf, $smbInf)) {
  $text = Get-Content -LiteralPath $inf -Raw
  if ($text -notmatch 'HKR,,Security,,"D:P\(A;;GA;;;SY\)\(A;;GA;;;BA\)"') {
    throw ("refusing to package {0}: no device access policy in the INF. " +
           "This build predates the security change - rebuild before releasing.") -f $inf
  }
  $ver = ([regex]::Match($text, 'DriverVer\s*=\s*([^\r\n]+)')).Groups[1].Value.Trim()
  Write-Host ("  {0}: DriverVer = {1}" -f (Split-Path $inf -Leaf), $ver) -ForegroundColor DarkGray
}

foreach ($sys in @((Join-Path $ln8000Dir 'ln8000_kmdf.sys'), (Join-Path $smbDir 'kmdf.sys'))) {
  $machine = Get-PeMachine $sys
  if ($machine -ne 0xAA64) {
    throw ("refusing to package {0}: PE machine is 0x{1:X4}, not 0xAA64 (ARM64)" -f $sys, $machine)
  }
  Write-Host ("  {0}: machine 0x{1:X4} ARM64" -f (Split-Path $sys -Leaf), $machine) -ForegroundColor DarkGray
}

# ---- staging ---------------------------------------------------------------

$stageRoot = Join-Path $RepoRoot 'artifacts\release'
$name = "nabu-ln8000-driver-$Version-arm64"
$stage = Join-Path $stageRoot $name
if (Test-Path $stage) { Remove-Item -LiteralPath $stage -Recurse -Force }
New-Item -ItemType Directory -Path (Join-Path $stage 'ln8000') -Force | Out-Null
New-Item -ItemType Directory -Path (Join-Path $stage 'smb-detection') -Force | Out-Null

# The install scripts default -PackageDir to $PSScriptRoot and look for
# ln8000_kmdf.inf there, so the package and the scripts have to share a folder.
Copy-Item (Join-Path $ln8000Dir '*') (Join-Path $stage 'ln8000') -Force
Copy-Item (Join-Path $smbDir '*') (Join-Path $stage 'smb-detection') -Force

$smbNote = @'
# SMB detection driver - not installable

This driver builds and signs for ARM64 and is included because it is part of the
project, not because it is ready to install. There is no install procedure for
it, it has never been deployed on a tablet, and six of its seven control codes
(`READ_REG`, `WRITE_REG`, `SET_ICL`, `GET_JOURNAL`, `DETECT_START`,
`APPLY_POLICY`) return `STATUS_NOT_IMPLEMENTED`. Only `GET_STATUS` answers.

Install the driver in `../ln8000/` instead. That is the one this project has
verified on hardware.
'@
$smbNote | Set-Content -LiteralPath (Join-Path $stage 'smb-detection\NOTE.md') -Encoding utf8

Copy-Item -LiteralPath $manifestPath $stage -Force
Copy-Item -LiteralPath $sumsPath $stage -Force

# ---- INSTALL.md ------------------------------------------------------------

$install = @'
# LN8000 charge pump driver for the Xiaomi Pad 5 - ARM64

Version: __VERSION__
Driver:  __DRIVERVER__
Project: https://github.com/FerrisMind/nabu-charger

## What this is

A Windows on ARM64 driver for the LN8000 charge pump in the Xiaomi Pad 5
(`nabu`), on the ACPI node `PEIC` (`ACPI\QCOM057E`, I2C address 0x51). It brings
up the pump, negotiates with a Quick Charge / HVDCP brick, drives the 2:1
switching mode and publishes live telemetry: input and battery voltage, current,
die temperature, faults and mode.

Measured on hardware: fast charging from a Quick Charge brick, SoC 44 -> 85 per
cent, `Iin` 1.13-1.62 A, `Vin` 8.7-9.4 V with the pump in 2:1 switching.

## What you need

* Xiaomi Pad 5 running Windows 11 on ARM64
* **Test signing on and Secure Boot off** - these packages are test-signed, and
  Windows will not load them otherwise:
  ```powershell
  bcdedit /set testsigning on     # then reboot once
  ```
* An elevated PowerShell. The driver restricts its device object to `LocalSystem`
  and Administrators, so the tools here have to run elevated.

No ACPI change and no UEFI reflash is needed: the `PEIC` node is already
described in the tablet's DSDT.

## Install

```powershell
cd ln8000
.\install-driver.ps1           # checks the signing mode, installs, binds ACPI\QCOM057E, starts it
.\nabu-ln8000.ps1 status       # expect: mode SWITCHING 2:1, or BYPASS 1:1 as the safe fallback
```

## Update and roll back

```powershell
.\update-driver.ps1            # installs on top, keeps the previous package
.\uninstall-driver.ps1         # stops the service and removes the package
```

The driver writes nothing to firmware and changes no power settings, so removing
it returns the device to its pre-installation behaviour.

`update-driver.ps1` exports the installed package to
`%ProgramData%\nabu-fastcharge\backup` before installing the new one, and that export is
the rollback point. A package whose `DriverVer` is lower than the one already in the
store is refused as *Outranked*: install the newer archive, or delete the stored package
first.

## Verify the download

`SHA256SUMS.txt` lists every file in this archive. On Windows:

```powershell
Get-Content SHA256SUMS.txt | ForEach-Object {
  $hash, $file = $_ -split '  ', 2
  if ((Get-FileHash $file -Algorithm SHA256).Hash.ToLower() -ne $hash) { "MISMATCH: $file" }
}
```

On Linux or macOS: `sha256sum -c SHA256SUMS.txt`. The file uses LF line endings, so it
verifies on either.

## Known defects

These are open in this version. They are listed here rather than only in the
repository, because a reader of a release note should not have to find them.

* **The AC verdict can drop while the brick is still attached**, which resets the
  display backlight to its default. Measured with the cable motionless: AC -> DC
  -> AC within 2.647 s. Two routes have been seen. The doubled-VBUS veto in the
  driver's `online_raw`: when the pump leaves switching, `Vin` relaxes to
  `2 * VBAT`, which is the pump's normal operating point, and the veto reads it
  as "no adapter", after which the 8 s hold expires. And the driver's own
  engagement pulses collapsing the adapter for ~290 ms, which asserts the
  hardware's unplug bit. This version raises the release run to three ticks
  (750 ms), so one pulse window cannot release the flag any more, but the
  doubled-VBUS route is still open and neither change has been verified against
  this defect on hardware yet.
* **The AC verdict arrives seconds after the cable, not with it.** The verdict itself
  (`OnlineRaw`) is computed on the first tick that sees the adapter, but the state Windows
  reads follows the *next* tick, and the pump bring-up runs inside the telemetry tick and
  blocks it: measured 8.7 s from insertion to AC on a Quick Charge brick and 5.6 s on a
  plain 5 V one. The removal side is faster - a live capture on this version shows Windows
  on "battery" 1.1 s after the cable came out.
* **A plain 5 V brick shows AC but "charging" only after the pack's
  fuel counter steps up**, which can take minutes at 1.5 A. The LN8000 never
  enters a mode there (`LastEnableErr = -4`, `ModeNotReached`), so the platform's
  own buck carries the current and the pump sees none of it - `Iin` sits on its
  39 mA ADC floor. The charging flag therefore has no current witness on that
  supply and falls back to the state of charge rising. The pack does charge.
* The die temperature is published while the ADC hibernates, so a hibernating
  channel reads as **160.0 degrees**, which is not a real measurement.
* The LN8000's own VBAT register reads 42-43 mV low against the ADC.
* Power Delivery (USB-C) supplies provide AC but the pack does not gain
  capacity; negotiation above 5 V is left to the platform's Type-C part.
* The SMB detection driver in `smb-detection/` is not installable - see its note.

## Licence

GPL-2.0-or-later. The LN8000 logic is a port of the GPL-2.0-or-later Android
kernel driver for the same chip. What came from where is written out in
`PROVENANCE.md` in the repository:
https://github.com/FerrisMind/nabu-charger
'@

$drvVer = ([regex]::Match((Get-Content -LiteralPath $ln8000Inf -Raw), 'DriverVer\s*=\s*([^\r\n]+)')).Groups[1].Value.Trim()
$install = $install.Replace('__VERSION__', $Version).Replace('__DRIVERVER__', $drvVer)
$install | Set-Content -LiteralPath (Join-Path $stage 'INSTALL.md') -Encoding utf8

# ---- checksums over the staged tree, then the zip --------------------------

$lines = @()
foreach ($file in (Get-ChildItem -LiteralPath $stage -Recurse -File | Sort-Object FullName)) {
  if ($file.Name -eq 'SHA256SUMS.txt') { continue }
  $rel = $file.FullName.Substring($stage.Length + 1) -replace '\\', '/'
  $lines += ((Get-FileHash -LiteralPath $file.FullName -Algorithm SHA256).Hash.ToLower() + '  ' + $rel)
}
Write-LinesLf (Join-Path $stage 'SHA256SUMS.txt') $lines

$zip = Join-Path $stageRoot ($name + '.zip')
if (Test-Path $zip) { Remove-Item -LiteralPath $zip -Force }
# Compress-Archive on Windows PowerShell writes entry names with backslashes, which the
# ZIP specification does not allow and which makes `unzip` on Linux and macOS refuse the
# archive ("appears to use backslashes as path separators"). The entries are added by
# hand so their names carry forward slashes and the archive opens anywhere.
Add-Type -AssemblyName System.IO.Compression
Add-Type -AssemblyName System.IO.Compression.FileSystem
$archive = [IO.Compression.ZipFile]::Open($zip, [IO.Compression.ZipArchiveMode]::Create)
try {
  foreach ($file in (Get-ChildItem -LiteralPath $stage -Recurse -File | Sort-Object FullName)) {
    $rel = $file.FullName.Substring($stage.Length + 1) -replace '\\', '/'
    [void][IO.Compression.ZipFileExtensions]::CreateEntryFromFile(
      $archive, $file.FullName, $rel, [IO.Compression.CompressionLevel]::Optimal)
  }
} finally {
  $archive.Dispose()
}

$zipInfo = Get-Item -LiteralPath $zip
Write-Host ''
Write-Host ("archive : " + $zip) -ForegroundColor Green
Write-Host ("entries : {0}" -f $lines.Count) -ForegroundColor Green
Write-Host ("size    : {0:N0} bytes" -f $zipInfo.Length) -ForegroundColor Green
Write-Host ("sha256  : " + (Get-FileHash -LiteralPath $zip -Algorithm SHA256).Hash.ToLower()) -ForegroundColor Green

if (-not $KeepStaging) { Remove-Item -LiteralPath $stage -Recurse -Force }
