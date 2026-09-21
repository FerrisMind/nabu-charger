#Requires -Version 5.1
<#
    verify-package.ps1 - standalone check of the driver package, without hardware.

    Run on any Windows computer:
        .\verify-package.ps1

    It checks what can be checked without the tablet:
      * whether all files of the package are present;
      * whether the driver really is built for ARM64 (by the PE header);
      * whether the checksums match artifacts\SHA256SUMS.txt;
      * whether the INF contains the required hardware ID and service name;
      * whether the package is signed (.cat and certificate present).

    Returns 0 if everything matches, and 1 on the first inconsistency.
    It works as an acceptance gate during handover: any engineer can run it
    and get the same answer.
#>
[CmdletBinding()]
param(
    [string]$KitDir = (Join-Path $PSScriptRoot '..\artifacts\driver-ln8000-arm64'),
    [string]$SumFile = (Join-Path $PSScriptRoot '..\artifacts\SHA256SUMS.txt'),
    [string]$SumPrefix = 'driver-ln8000-arm64/'
)

$ErrorActionPreference = 'Stop'
$problems = New-Object System.Collections.Generic.List[string]
$checks = 0

function Check {
    param([string]$Title, [scriptblock]$Test)
    $script:checks++
    try {
        $outcome = & $Test
        if ($outcome -eq $true) {
            Write-Host ("  [ok]  " + $Title) -ForegroundColor Green
        } else {
            Write-Host ("  [fail] " + $Title) -ForegroundColor Red
            $problems.Add($Title)
        }
    } catch {
        Write-Host ("  [fail] " + $Title + ' - ' + $_.Exception.Message) -ForegroundColor Red
        $problems.Add($Title + ': ' + $_.Exception.Message)
    }
}

Write-Host ''
Write-Host '=== nabu driver package check ===' -ForegroundColor Cyan
Write-Host ("package   : " + (Resolve-Path -LiteralPath $KitDir))
Write-Host ("checksums : " + (Resolve-Path -LiteralPath $SumFile))
Write-Host ''

$required = @(
    'ln8000_kmdf.sys', 'ln8000_kmdf.inf', 'ln8000_kmdf.cat', 'WDRLocalTestCert.cer',
    'install-driver.ps1', 'update-driver.ps1', 'uninstall-driver.ps1',
    'nabu-ln8000.ps1', 'run-acceptance.ps1', 'bring-up.ps1', 'enable-remote.ps1'
)

Write-Host 'Files:' -ForegroundColor Cyan
foreach ($name in $required) {
    Check ("present: " + $name) { Test-Path -LiteralPath (Join-Path $KitDir $name) }
}

Write-Host ''
Write-Host 'Bitness and signature:' -ForegroundColor Cyan
Check 'driver is built for ARM64 (PE Machine = 0xAA64)' {
    $path = Join-Path $KitDir 'ln8000_kmdf.sys'
    $bytes = [IO.File]::ReadAllBytes($path)
    $peOffset = [BitConverter]::ToInt32($bytes, 0x3C)
    $machine = [BitConverter]::ToUInt16($bytes, $peOffset + 4)
    Write-Verbose ("Machine = 0x{0:X4}" -f $machine)
    $machine -eq 0xAA64
}

Check 'driver size within a sane range (10..200 KB)' {
    $size = (Get-Item -LiteralPath (Join-Path $KitDir 'ln8000_kmdf.sys')).Length
    $size -gt 10240 -and $size -lt 204800
}

Write-Host ''
Write-Host 'INF contents:' -ForegroundColor Cyan
$infText = Get-Content -LiteralPath (Join-Path $KitDir 'ln8000_kmdf.inf') -Raw
Check 'INF knows the hardware ID ACPI\QCOM057E' { $infText -match 'ACPI\\QCOM057E' }
Check 'INF brings up the ln8000_kmdf service' { $infText -match 'ServiceBinary.*ln8000_kmdf\.sys' }
Check 'INF sets the profile parameters (IinLimitUa, VbatFloatUv)' {
    ($infText -match 'IinLimitUa') -and ($infText -match 'VbatFloatUv')
}
Check 'INF allows choosing the protection profile (ProtectionProfile) without a rebuild' {
    $infText -match 'ProtectionProfile'
}
Check 'INF sets the telemetry period' { $infText -match 'TelemetryMs' }
Check 'INF sets the protection thresholds (temperature and current)' {
    ($infText -match 'TempReduceDc') -and ($infText -match 'TempBypassDc') -and ($infText -match 'TempStopDc') -and ($infText -match 'IinTargetUa')
}

Write-Host ''
Write-Host 'Checksums:' -ForegroundColor Cyan
$sums = @{}
foreach ($line in (Get-Content -LiteralPath $SumFile)) {
    $parts = $line -split '\s+', 2
    if ($parts.Count -eq 2 -and $parts[1].StartsWith($SumPrefix)) {
        $sums[$parts[1].Substring($SumPrefix.Length)] = $parts[0]
    }
}
Check ('the checksum file has entries for the package (' + $sums.Count + ' items)') { $sums.Count -ge $required.Count }

foreach ($name in $required) {
    if (-not $sums.ContainsKey($name)) {
        Write-Host ("  [fail] no checksum for " + $name) -ForegroundColor Red
        $problems.Add('no checksum for ' + $name)
        $checks++
        continue
    }
    Check ("checksum matches: " + $name) {
        $actual = (Get-FileHash -LiteralPath (Join-Path $KitDir $name) -Algorithm SHA256).Hash.ToLower()
        $actual -eq $sums[$name]
    }
}

Write-Host ''
Write-Host 'Script syntax:' -ForegroundColor Cyan
foreach ($name in $required | Where-Object { $_ -like '*.ps1' }) {
    Check ("parses without errors: " + $name) {
        $errors = $null
        $null = [System.Management.Automation.Language.Parser]::ParseFile(
            (Join-Path $KitDir $name), [ref]$null, [ref]$errors)
        $errors.Count -eq 0
    }
}

Write-Host ''
if ($problems.Count -eq 0) {
    Write-Host ("RESULT: package OK, checks passed: " + $checks) -ForegroundColor Green
    exit 0
}
Write-Host ("RESULT: " + $problems.Count + " problems out of " + $checks + " checks") -ForegroundColor Red
$problems | ForEach-Object { Write-Host ("  - " + $_) }
exit 1
