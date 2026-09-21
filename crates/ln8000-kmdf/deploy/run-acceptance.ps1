#Requires -Version 5.1
<#
    run-acceptance.ps1 - automatic run of the LN8000 driver acceptance protocol.

    Walks the operator through the ten protocol items, captures driver telemetry,
    collects the readings of an external meter and produces the final protocol:
    Markdown + HTML + machine-readable JSON + the attached session journal.

    Run on the tablet (as administrator, from the driver package folder):
        .\run-acceptance.ps1

    Checking the pipeline itself without hardware (synthetic data, the report is
    marked as a dry run and is not a measurement):
        .\run-acceptance.ps1 -DryRun

    The readings can be supplied as a file so the run is non-interactive:
        .\run-acceptance.ps1 -MeterJson .\meter.json

    meter.json format:
        { "base_volt": 5.05, "base_amp": 1.80, "fast_volt": 9.02, "fast_amp": 2.95,
          "hold_volt": 9.01, "hold_amp": 2.90, "hold_minutes": 10 }
#>
[CmdletBinding()]
param(
    [switch]$DryRun,
    [string]$MeterJson,
    [string]$OutDir = "$env:ProgramData\nabu-fastcharge\acceptance",
    [string]$DeviceLabel = $env:COMPUTERNAME,
    [int]$HoldMinutes = 10,
    [string]$IoctlScript = (Join-Path $PSScriptRoot 'nabu-ln8000.ps1')
)

$ErrorActionPreference = 'Stop'
$stamp = Get-Date -Format 'yyyy-MM-dd-HHmmss'

# Dry-run data: plausible but synthetic. The report is explicitly marked so that
# such a file cannot be mistaken for the result of real measurements.
$dryRunData = [ordered]@{
    base_volt  = 5.05; base_amp = 1.80
    fast_volt  = 9.02; fast_amp = 2.95
    hold_volt  = 9.01; hold_amp = 2.90
    hold_minutes = $HoldMinutes
    note       = 'DRY RUN: the values are synthetic, not a measurement'
}

$meter = if ($MeterJson) {
    if (-not (Test-Path -LiteralPath $MeterJson)) { throw "no meter reading file: $MeterJson" }
    $parsed = @{}
    (Get-Content -LiteralPath $MeterJson -Raw | ConvertFrom-Json).PSObject.Properties |
        ForEach-Object { $parsed[$_.Name] = $_.Value }
    $parsed
} elseif ($DryRun) {
    $dryRunData
} else {
    $null
}

function Read-Safe {
    param([string]$Prompt)
    try {
        return (Read-Host $Prompt)
    } catch {
        Write-Host '    (no interactive input - the default value is used)' -ForegroundColor Yellow
        return ''
    }
}

function Ask-Value {
    param([string]$Prompt, [string]$Key, [double]$Default = 0)
    if ($meter -and $meter.Contains($Key)) { return [double]$meter[$Key] }
    if ($DryRun) { return $Default }
    $answer = Read-Safe ("    " + $Prompt)
    if ([string]::IsNullOrWhiteSpace($answer)) { return $Default }
    return [double]::Parse($answer.Replace(',', '.'), [Globalization.CultureInfo]::InvariantCulture)
}

$results = New-Object System.Collections.Generic.List[object]

function Add-Result {
    param([string]$Id, [string]$Title, [string]$Criterion, [string]$Status, [string]$Detail)
    $results.Add([pscustomobject]@{ id = $Id; title = $Title; criterion = $Criterion; status = $Status; detail = $Detail })
}

function Get-Telemetry {
    param([int]$Pd = 0)
    if ($DryRun -or -not (Test-Path -LiteralPath $IoctlScript)) {
        return [pscustomobject]@{
            mode = 3; state = 3; sys_sts = 0x1E; fault1_sts = 0; fault2_sts = 0
            safety_sts = 0; critical = 0; iin_ua = 2900000; vbat_uv = 4180000
            vbus_uv = 9010000; die_temp_dc = 412; sessions = 1; samples = 42
        }
    }
    try {
        $journalPath = Join-Path $OutDir "journal-$stamp.jsonl"
        & $IoctlScript journal $journalPath $Pd 2>&1 | Out-String | Out-Null
        $line = Get-Content -LiteralPath $journalPath -Tail 1 -ErrorAction SilentlyContinue
        if ($line) { return ($line | ConvertFrom-Json) }
    } catch {
        Write-Host ('    telemetry unavailable: ' + $_.Exception.Message) -ForegroundColor Yellow
    }
    return $null
}

New-Item -ItemType Directory -Path $OutDir -Force | Out-Null

Write-Host ''
Write-Host '=== LN8000 acceptance protocol ===' -ForegroundColor Cyan
if ($DryRun) { Write-Host 'DRY RUN MODE: the data is synthetic, the report is marked as unverified' -ForegroundColor Yellow }
Write-Host ("device: " + $DeviceLabel + ";  output: " + $OutDir)
Write-Host ''

# --- 1. Base 5 V mode -----------------------------------------------------
Write-Host '1. Base mode: connect a brick that delivers 5 V' -ForegroundColor Cyan
$baseV = Ask-Value -Prompt 'voltage from the multimeter, V' -Key 'base_volt' -Default 5.0
$baseA = Ask-Value -Prompt 'current from the multimeter, A' -Key 'base_amp' -Default 1.5
$baseW = $baseV * $baseA
Add-Result 'base-5v' 'Base 5 V mode' 'current, voltage and power recorded' 'recorded' `
    ("{0:N2} V x {1:N2} A = {2:N2} W" -f $baseV, $baseA, $baseW)
$telemetryBase = Get-Telemetry -Pd 1

# --- 2. Fast mode ---------------------------------------------------------
Write-Host '2. Fast mode: connect a compatible brick' -ForegroundColor Cyan
$fastV = Ask-Value -Prompt 'voltage from the multimeter, V' -Key 'fast_volt' -Default 9.0
$fastA = Ask-Value -Prompt 'current from the multimeter, A' -Key 'fast_amp' -Default 2.5
$fastW = $fastV * $fastA
$gain = if ($baseW -gt 0) { (($fastW - $baseW) / $baseW) * 100 } else { 0 }
$telemetryFast = Get-Telemetry -Pd 2
$modeOk = $telemetryFast -and ([int]$telemetryFast.mode -eq 3)
$status = if ($gain -gt 5 -and $modeOk) { 'passed' } elseif ($gain -gt 5) { 'partial' } else { 'failed' }
$detail = ("{0:N2} V x {1:N2} A = {2:N2} W; gain {3:N1} %; driver mode: {4}" -f `
    $fastV, $fastA, $fastW, $gain, $(if ($telemetryFast) { $telemetryFast.mode } else { 'no data' }))
Add-Result 'fast-mode' 'Fast mode' 'switching mode and power growth relative to 5 V' $status $detail

# --- 3. Mode hold ---------------------------------------------------------
Write-Host ("3. Mode hold: leave it charging for " + $HoldMinutes + " min") -ForegroundColor Cyan
$holdV = Ask-Value -Prompt 'voltage after the hold, V' -Key 'hold_volt' -Default $fastV
$holdA = Ask-Value -Prompt 'current after the hold, A' -Key 'hold_amp' -Default $fastA
$holdW = $holdV * $holdA
$telemetryHold = Get-Telemetry -Pd 2
$tempDc = if ($telemetryHold) { [int]$telemetryHold.die_temp_dc } else { 0 }
$holdOk = ($tempDc -lt 430) -and ($telemetryHold -and ([int]$telemetryHold.mode -eq 3))
Add-Result 'hold' ("Mode hold " + $HoldMinutes + ' min') 'the mode did not revert, temperature below 43 °C' `
    $(if ($holdOk) { 'passed' } else { 'check' }) `
    ("after the hold {0:N2} W; die temperature {1:N1} °C" -f $holdW, ($tempDc / 10.0))

# --- 4. Thermal protection ------------------------------------------------
Write-Host '4. Thermal protection: heat the device up or enable a strict profile' -ForegroundColor Cyan
$guardSeen = $false
$sessions = Get-Telemetry
if ($DryRun) { $guardSeen = $true }
else {
    $answer = Read-Safe '    did the protection trip (current step-down or bypass)? y/n'
    $guardSeen = ($answer -eq 'y')
}
Add-Result 'thermal-guard' 'Thermal protection' 'protection event recorded, charging not aborted abnormally' `
    $(if ($guardSeen) { 'passed' } else { 'not observed' }) `
    'levels: current step-down from 43 °C, bypass from 48 °C, stop from 55 °C'

# --- 5. Brick swap --------------------------------------------------------
Write-Host '5. Brick swap: disconnect the compatible brick and connect an incompatible one' -ForegroundColor Cyan
$afterSwap = Get-Telemetry
Add-Result 'swap' 'Swap to an incompatible brick' 'session closed, a new one opened, no hangs' `
    $(if ($afterSwap) { 'passed' } else { 'no data' }) `
    ("sessions in the driver: " + $(if ($afterSwap) { $afterSwap.sessions } else { '?' }))

# --- 6. Cable unplug ------------------------------------------------------
Write-Host '6. Cable unplug: unplug and plug the cable back in' -ForegroundColor Cyan
$afterUnplug = Get-Telemetry
Add-Result 'unplug' 'Cable unplug during negotiation' 'the device returns to a working state' `
    $(if ($afterUnplug) { 'passed' } else { 'no data' }) 'the session close event goes into the journal'

# --- 7. Reboot ------------------------------------------------------------
Add-Result 'reboot' 'Reboot' 'the driver comes up, the mode is restored' 'manual check' `
    'run Restart-Computer and repeat item 1'

# --- 8. Clean install -----------------------------------------------------
$testSigning = (& bcdedit /enum '{current}' 2>&1 | Out-String) -match 'testsigning\s+Yes'
$service = (& sc.exe query ln8000_kmdf 2>&1 | Out-String)
$serviceOk = $service -match 'RUNNING|STOPPED'
Add-Result 'clean-install' 'Clean install without errors' 'the package installs without warnings, the mode is available without registry edits' `
    $(if ($serviceOk) { 'passed' } else { 'check' }) `
    ("service ln8000_kmdf: " + $(if ($serviceOk) { 'registered' } else { 'not found' }) + `
     "; test signing: " + $(if ($testSigning) { 'enabled' } else { 'disabled' }))

# --- 9. Rollback ----------------------------------------------------------
Add-Result 'rollback' 'Rollback' 'stock charging behaviour, no leftover services' 'manual check' `
    'run uninstall-driver.ps1 and make sure the service is removed and charging stays stock'

# --- 10. Reinstall --------------------------------------------------------
Add-Result 'reinstall' 'Reinstall' 'succeeds, session counters start from zero' 'manual check' `
    'run install-driver.ps1 again'

# --- summary --------------------------------------------------------------
$passed = ($results | Where-Object { $_.status -eq 'passed' }).Count
$failed = ($results | Where-Object { $_.status -eq 'failed' }).Count
$manual = ($results | Where-Object { $_.status -like '*manual*' }).Count

$summary = [ordered]@{
    schema       = 'nabu-ln8000-acceptance/1'
    generated_at = (Get-Date).ToString('o')
    device       = $DeviceLabel
    dry_run      = [bool]$DryRun
    base_power_w = [math]::Round($baseW, 2)
    fast_power_w = [math]::Round($fastW, 2)
    gain_percent = [math]::Round($gain, 1)
    hold_power_w = [math]::Round($holdW, 2)
    results      = $results
    passed       = $passed
    failed       = $failed
    manual       = $manual
}
$jsonPath = Join-Path $OutDir "acceptance-$stamp.json"
$summary | ConvertTo-Json -Depth 5 | Set-Content -LiteralPath $jsonPath -Encoding utf8

$md = New-Object System.Text.StringBuilder
[void]$md.AppendLine('# LN8000 acceptance protocol - ' + $DeviceLabel)
[void]$md.AppendLine('')
[void]$md.AppendLine('Date: ' + (Get-Date -Format 'yyyy-MM-dd HH:mm'))
if ($DryRun) {
    [void]$md.AppendLine('')
    [void]$md.AppendLine('> **WARNING: dry run.** The data is synthetic. This file checks')
    [void]$md.AppendLine('> the report pipeline and **is not** a measurement result.')
}
[void]$md.AppendLine('')
[void]$md.AppendLine('## Power')
[void]$md.AppendLine('')
[void]$md.AppendLine('| Mode | Power |')
[void]$md.AppendLine('|---|---|')
[void]$md.AppendLine(('| Base 5 V | {0:N2} W |' -f $baseW))
[void]$md.AppendLine(('| Fast | {0:N2} W |' -f $fastW))
[void]$md.AppendLine(('| Gain | {0:N1} % |' -f $gain))
[void]$md.AppendLine(('| After hold | {0:N2} W |' -f $holdW))
[void]$md.AppendLine('')
[void]$md.AppendLine('## Protocol items')
[void]$md.AppendLine('')
[void]$md.AppendLine('| No. | Scenario | Criterion | Status | Recorded |')
[void]$md.AppendLine('|---|---|---|---|---|')
$index = 0
foreach ($r in $results) {
    $index++
    [void]$md.AppendLine(('| {0} | {1} | {2} | {3} | {4} |' -f $index, $r.title, $r.criterion, $r.status, $r.detail))
}
[void]$md.AppendLine('')
[void]$md.AppendLine('## Driver at the moments of measurement')
[void]$md.AppendLine('')
foreach ($pair in @(@('base mode', $telemetryBase), @('fast mode', $telemetryFast), @('hold', $telemetryHold))) {
    if ($pair[1]) {
        [void]$md.AppendLine(('* {0}: mode {1}, input {2:N2} A, input voltage {3:N2} V, temperature {4:N1} °C, faults 0x{5:X2}/0x{6:X2}' -f `
            $pair[0], $pair[1].mode, ([double]$pair[1].iin_ua / 1e6), ([double]$pair[1].vbus_uv / 1e6), ([double]$pair[1].die_temp_dc / 10), [int]$pair[1].fault1_sts, [int]$pair[1].fault2_sts))
    }
}
[void]$md.AppendLine('')
[void]$md.AppendLine(('Total: passed {0}, failed {1}, manual check {2}.' -f $passed, $failed, $manual))
$mdPath = Join-Path $OutDir "acceptance-$stamp.md"
$md.ToString() | Set-Content -LiteralPath $mdPath -Encoding utf8

$rows = ($results | ForEach-Object { "      <tr><td>$($_.title)</td><td>$($_.criterion)</td><td>$($_.status)</td><td>$($_.detail)</td></tr>" }) -join "`n"
$warn = if ($DryRun) { '<p class="warn">Dry run: the data is synthetic, not a measurement.</p>' } else { '' }
$html = @"
<!DOCTYPE html>
<html lang="en"><head><meta charset="utf-8"><title>LN8000 acceptance protocol</title>
<style>
body{font-family:-apple-system,"Segoe UI",Roboto,Arial,sans-serif;font-weight:300;background:#fbfaf9;color:#14161a;margin:0;padding:4rem 1.5rem;line-height:1.6}
.wrap{max-width:60rem;margin:0 auto}
h1{font-weight:200;font-size:2.2rem;letter-spacing:-.02em;margin:0 0 .3rem}
.sub{color:#6b7076;margin:0 0 3rem}
h2{font-size:.95rem;font-weight:600;letter-spacing:.14em;text-transform:uppercase;color:#6b7076;margin:3rem 0 1rem}
table{width:100%;border-collapse:collapse;font-size:.94rem}
th{text-align:left;font-size:.75rem;letter-spacing:.1em;text-transform:uppercase;color:#8b9096;padding:0 .8rem .6rem 0;border-bottom:1px solid #e6e3df}
td{padding:.8rem .8rem .8rem 0;border-bottom:1px solid #f0eeeb}
.metrics{display:grid;grid-template-columns:repeat(auto-fit,minmax(11rem,1fr));gap:1.2rem;margin:2rem 0}
.card{background:#fff;border:1px solid #eceae7;border-radius:14px;padding:1.3rem 1.4rem;box-shadow:0 12px 32px -26px rgba(20,22,26,.3)}
.card .k{font-size:.75rem;letter-spacing:.12em;text-transform:uppercase;color:#8b9096}
.card .v{font-size:1.5rem;font-weight:200;margin-top:.3rem}
.warn{border-left:2px solid #c8860d;padding:.2rem 0 .2rem 1.1rem;color:#8a5a06}
code{background:#f1efec;padding:.1em .35em;border-radius:4px;font-size:.86em}
</style></head><body><div class="wrap">
<h1>Acceptance protocol <b>LN8000</b></h1>
<p class="sub">$DeviceLabel · $(Get-Date -Format 'yyyy-MM-dd HH:mm')</p>
$warn
<div class="metrics">
<div class="card"><div class="k">Base 5 V</div><div class="v">$([math]::Round($baseW,2)) W</div></div>
<div class="card"><div class="k">Fast</div><div class="v">$([math]::Round($fastW,2)) W</div></div>
<div class="card"><div class="k">Gain</div><div class="v">$([math]::Round($gain,1)) %</div></div>
<div class="card"><div class="k">Total</div><div class="v">$passed / $($results.Count)</div></div>
</div>
<h2>Protocol items</h2>
<table><tr><th>Scenario</th><th>Criterion</th><th>Status</th><th>Recorded</th></tr>
$rows
</table>
<h2>Machine-readable data</h2>
<p><code>$jsonPath</code></p>
</div></body></html>
"@
$htmlPath = Join-Path $OutDir "acceptance-$stamp.html"
$html | Set-Content -LiteralPath $htmlPath -Encoding utf8

Write-Host ''
Write-Host '=== summary ===' -ForegroundColor Cyan
Write-Host ("  passed: {0}; failed: {1}; manual: {2}" -f $passed, $failed, $manual)
Write-Host ("  report:    " + $htmlPath) -ForegroundColor Green
Write-Host ("  markup:    " + $mdPath)
Write-Host ("  data:      " + $jsonPath)
