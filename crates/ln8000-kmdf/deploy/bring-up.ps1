#Requires -Version 5.1
<#
    bring-up.ps1 - one run on the tablet: check -> install -> collect evidence.

    Run (PowerShell as administrator, from the driver package folder):
        .\bring-up.ps1

    What it does:
      1) records the system state (Windows build, architecture, test signing);
      2) looks for the ACPI\QCOM057E node and the driver service;
      3) installs the driver if a package lies nearby and it is not installed yet;
      4) captures driver telemetry (mode, sessions, journal);
      5) measures the battery charge through WMI at two intervals - that shows
         whether charging happens at all, without any multimeter;
      6) puts everything into ONE text file and one archive to be sent back.

    It writes nothing to firmware and changes no power settings. Rollback:
    uninstall-driver.ps1 from this same package.
#>
[CmdletBinding()]
param(
    [string]$OutDir = "$env:ProgramData\nabu-fastcharge\report",
    [switch]$SkipInstall,
    [int]$ChargeSampleSeconds = 120,
    [string]$PackageDir = $PSScriptRoot
)

$ErrorActionPreference = 'Continue'
$stamp = Get-Date -Format 'yyyy-MM-dd-HHmmss'
New-Item -ItemType Directory -Path $OutDir -Force | Out-Null
$log = Join-Path $OutDir "nabu-report-$stamp.txt"

function Section {
    param([string]$Title)
    Add-Content -LiteralPath $log -Value ''
    Add-Content -LiteralPath $log -Value ('=' * 70)
    Add-Content -LiteralPath $log -Value $Title
    Add-Content -LiteralPath $log -Value ('=' * 70)
    Write-Host $Title -ForegroundColor Cyan
}

function Capture {
    param([string]$Title, [scriptblock]$Action)
    Section $Title
    try {
        $output = & $Action 2>&1 | Out-String
        Add-Content -LiteralPath $log -Value $output
        return $output
    } catch {
        Add-Content -LiteralPath $log -Value ("ERROR: " + $_.Exception.Message)
        Write-Host ("  error: " + $_.Exception.Message) -ForegroundColor Yellow
        return ''
    }
}

function Is-Admin {
    $identity = [Security.Principal.WindowsIdentity]::GetCurrent()
    $principal = New-Object Security.Principal.WindowsPrincipal($identity)
    return $principal.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)
}

# The charge measurement reads GetSystemPowerStatus, the API the tray and Settings use.
# Win32_Battery is the obvious source and it is the wrong one here: on this tablet it
# returns no instances even when a working battery is published (measured 24.09.2026 with
# the driver's battery in place and GetSystemPowerStatus reporting 52 %).
function Get-PowerStatus {
    if (-not ('NabuBringUp.PowerStatus' -as [type])) {
        Add-Type -Namespace NabuBringUp -Name PowerStatus -MemberDefinition @'
[StructLayout(LayoutKind.Sequential)]
public struct SPS { public byte AC; public byte Flag; public byte Life; public byte Full; public uint Run; public uint FullRun; public uint Flags; public uint Sec; }
[DllImport("kernel32.dll", SetLastError=true)] public static extern bool GetSystemPowerStatus(out SPS s);
'@ -ErrorAction Stop
    }
    $s = New-Object NabuBringUp.PowerStatus+SPS
    $ok = [NabuBringUp.PowerStatus]::GetSystemPowerStatus([ref]$s)
    if (-not $ok) { return $null }
    return [pscustomobject]@{
        Ok      = $ok
        ACLine  = $s.AC
        Flag    = $s.Flag
        LifePct = $s.Life
        FullPct = $s.Full
        Notes   = (Get-PowerStatusNote $s.AC $s.Flag $s.Life)
    }
}

function Get-PowerStatusNote {
    param([int]$AC, [int]$Flag, [int]$Life)
    if ($Flag -eq 128) { return 'no system battery (BatteryFlag 128)' }
    if ($Life -eq 255) { return 'charge unknown (LifePercent 255)' }
    $bits = @()
    if ($Flag -band 8) { $bits += 'charging' }
    if ($Flag -band 1) { $bits += 'high' }
    if ($Flag -band 2) { $bits += 'low' }
    if ($Flag -band 4) { $bits += 'critical' }
    if (-not $bits.Count) { $bits += 'mid level' }
    return ('battery present, ' + ($bits -join '/'))
}

# One shape for the two sources, so the interval comparison does not care which answered.
function Get-ChargeView {
    $ps = Get-PowerStatus
    if ($ps -and $ps.LifePct -ne 255) {
        return [pscustomobject]@{ Pct = [int]$ps.LifePct; AC = $ps.ACLine; Note = $ps.Notes }
    }
    $w = Get-CimInstance Win32_Battery -ErrorAction SilentlyContinue | Select-Object -First 1
    if ($w) {
        return [pscustomobject]@{
            Pct  = [int]$w.EstimatedChargeRemaining
            AC   = 'unknown'
            Note = 'Win32_Battery: ' + $w.Name + ', status ' + $w.BatteryStatus
        }
    }
    return $null
}

Add-Content -LiteralPath $log -Value 'Fast charging report for nabu (Xiaomi Pad 5)'
Add-Content -LiteralPath $log -Value ("Start time: " + (Get-Date -Format 'yyyy-MM-dd HH:mm:ss'))
Write-Host ''
Write-Host '=== collecting charging evidence for nabu ===' -ForegroundColor Cyan
Write-Host ("the report will be here: " + $log)

$admin = Is-Admin
Capture 'RIGHTS' { if ($admin) { 'administrator: yes' } else { 'administrator: NO - some steps will be unavailable' } }

Capture 'SYSTEM' {
    $os = Get-CimInstance Win32_OperatingSystem -ErrorAction SilentlyContinue
    [pscustomobject]@{
        ComputerName   = $env:COMPUTERNAME
        Caption        = $os.Caption
        Version        = $os.Version
        BuildNumber    = $os.BuildNumber
        Architecture   = $env:PROCESSOR_ARCHITECTURE
        PowerShell     = $PSVersionTable.PSVersion.ToString()
        OSArchitecture = $os.OSArchitecture
    } | Format-List
}

Capture 'DRIVER SIGNING' { & bcdedit /enum '{current}' | Select-String -Pattern 'testsigning|nointegritychecks|hypervisorlaunchtype' }

Capture 'ACPI\QCOM057E NODE' {
    $device = Get-PnpDevice -PresentOnly -ErrorAction SilentlyContinue |
        Where-Object { $_.InstanceId -like '*QCOM057E*' }
    if (-not $device) { return 'node not found - the driver has nothing to start on' }
    foreach ($item in $device) {
        $service = (Get-PnpDeviceProperty -InstanceId $item.InstanceId -KeyName 'DEVPKEY_Device_Service' -ErrorAction SilentlyContinue).Data
        $problem = (Get-PnpDeviceProperty -InstanceId $item.InstanceId -KeyName 'DEVPKEY_Device_ProblemCode' -ErrorAction SilentlyContinue).Data
        [pscustomobject]@{
            InstanceId = $item.InstanceId
            Status     = $item.Status
            Class      = $item.Class
            Service    = $service
            Problem    = $problem
        }
    }
}

Capture 'DRIVER PACKAGE IN THE STORE' {
    (& pnputil /enum-drivers | Out-String) -split "`r?`n`r?`n" |
        Where-Object { $_ -match 'ln8000|nabu' }
}

Capture 'DRIVER SERVICE' {
    & sc.exe query ln8000_kmdf
    & sc.exe qc ln8000_kmdf
}

Capture 'PMIC PLATFORM (qcpmic*, qcspmi)' {
    # The charger stack this driver negotiates through. Community "usbfix"
    # packages replace qcpmicext8150.sys inside the DriverStore, which changes
    # the layer the pump driver depends on - the file size and the write date
    # below are what tells those machines apart.
    foreach ($d in (Get-CimInstance Win32_SystemDriver -ErrorAction SilentlyContinue |
            Where-Object { $_.Name -match 'qcpmic|qcspmi' })) {
        $path = $d.PathName -replace '^\\\?\?\\', ''
        $info = ''
        if ($path -and (Test-Path -LiteralPath $path)) {
            $item = Get-Item -LiteralPath $path
            $info = (' | ' + $item.Name + ' ' + $item.Length + ' bytes, ver ' +
                $item.VersionInfo.FileVersion + ', ' + $item.LastWriteTime.ToString('yyyy-MM-dd'))
        }
        ($d.Name + ' | ' + $d.State + $info)
    }
    if (Test-Path 'C:\drivers\qcpmicext8150_fix20.sys') {
        'usbfix fix20 DETECTED: C:\drivers\qcpmicext8150_fix20.sys exists.'
        'The PMIC driver is a community replacement - the charger stack differs'
        'from the one this driver was verified on.'
    }
    if (Test-Path 'C:\usbfix_backup') {
        $names = Get-ChildItem 'C:\usbfix_backup' -ErrorAction SilentlyContinue |
            Select-Object -ExpandProperty Name
        'usbfix backup folder present: ' + ($names -join ', ')
    }
}

$inf = Join-Path $PackageDir 'ln8000_kmdf.inf'
$skipReason = ''
if ($SkipInstall) {
    $skipReason = '-SkipInstall was given'
} elseif (-not (Test-Path -LiteralPath $inf)) {
    $skipReason = "no file $inf"
} elseif (-not $admin) {
    $skipReason = 'no administrator rights'
}

if ([string]::IsNullOrEmpty($skipReason)) {
    Capture 'DRIVER INSTALLATION' {
        & pnputil /add-driver $inf /install
        & sc.exe start ln8000_kmdf
    }
} else {
    Capture 'DRIVER INSTALLATION' { 'skipped: ' + $skipReason }
}

$diag = Join-Path $PackageDir 'nabu-ln8000.ps1'
if (Test-Path -LiteralPath $diag) {
    Capture 'DRIVER: STATE' { & $diag status }
    Capture 'DRIVER: SESSIONS' { & $diag sessions }
    Capture 'DRIVER: JOURNAL SNAPSHOT' { & $diag journal (Join-Path $OutDir "driver-journal-$stamp.jsonl") }
} else {
    Capture 'DRIVER' { 'tool nabu-ln8000.ps1 not found next to the script' }
}

Capture 'BATTERY DEVICES' {
    # Two batteries in Windows is the symptom this section exists for: the
    # platform stack may publish one of its own, and this driver publishes
    # another unless PublishBattery=0. The three sources disagree on purpose -
    # that disagreement is itself evidence.
    $list = @(Get-PnpDevice -Class Battery -ErrorAction SilentlyContinue)
    'battery-class devnodes: ' + $list.Count
    $list | ForEach-Object { ($_.Status + ' | ' + $_.FriendlyName + ' | ' + $_.InstanceId) }
    'Win32_Battery instances: ' + @(Get-CimInstance Win32_Battery -ErrorAction SilentlyContinue).Count
    $ps = Get-PowerStatus
    if ($ps) {
        'GetSystemPowerStatus: AC line ' + $ps.ACLine + ', flag ' + $ps.Flag + ', charge ' +
            $ps.LifePct + ' %, ' + $ps.Notes
        '  (this is the source the tray icon and Settings read; Win32_Battery can stay empty)'
    } else {
        'GetSystemPowerStatus did not answer'
    }
}

Capture 'DRIVER MARKS (Device Parameters)' {
    # Every value the driver writes for a post-mortem: the SPMI probes
    # (SpmiProbeSuperuser, HvdcpVia), the session counters, the thermal guard and
    # the battery flags. This is what a remote report is read from.
    $base = 'HKLM:\SYSTEM\CurrentControlSet\Enum\ACPI\QCOM057E'
    if (-not (Test-Path $base)) { return 'no ACPI\QCOM057E enum key - the node never came up' }
    Get-ChildItem $base -ErrorAction SilentlyContinue | ForEach-Object {
        $params = Get-ItemProperty -Path (Join-Path $_.PSPath 'Device Parameters') -ErrorAction SilentlyContinue
        if (-not $params) { return 'no Device Parameters values yet' }
        'instance ' + $_.PSChildName
        $params.PSObject.Properties |
            Where-Object { $_.Name -notmatch '^PS' } |
            Sort-Object Name |
            ForEach-Object { '  ' + $_.Name + ' = ' + $_.Value }
    }
}

Capture 'BATTERY: FIRST MEASUREMENT' {
    $ps = Get-PowerStatus
    if ($ps) { 'GetSystemPowerStatus: charge ' + $ps.LifePct + ' %, AC line ' + $ps.ACLine + ', ' + $ps.Notes }
    Get-CimInstance Win32_Battery -ErrorAction SilentlyContinue |
        Select-Object Name, DeviceID, BatteryStatus, EstimatedChargeRemaining,
                      EstimatedRunTime, DesignVoltage, Chemistry |
        Format-List
}

Section ("BATTERY: SECOND MEASUREMENT AFTER " + $ChargeSampleSeconds + " s")
Write-Host ("  waiting " + $ChargeSampleSeconds + " s to estimate the charge change...") -ForegroundColor Yellow
$first = Get-ChargeView
Start-Sleep -Seconds $ChargeSampleSeconds
$second = Get-ChargeView
if ($first -and $second) {
    $delta = [int]$second.Pct - [int]$first.Pct
    if ($delta -gt 0) {
        $verdict = 'charge is RISING'
    } elseif ($delta -lt 0) {
        $verdict = 'charge is FALLING'
    } else {
        $verdict = 'charge is UNCHANGED'
    }
    # The percentage is whole numbers, so a slow charge reads as UNCHANGED over a short
    # interval. The driver's own marks carry a finer witness: the pack's gauge current and
    # charge flag, which move long before the percentage does.
    $marks = Get-ItemProperty -Path 'HKLM:\SYSTEM\CurrentControlSet\Enum\ACPI\QCOM057E\*\Device Parameters' -ErrorAction SilentlyContinue
    if ($marks) {
        $fine = 'marks: BattPct ' + $marks.BattPct + ', GaugeCharging ' + $marks.GaugeCharging +
                ', SocRaw ' + $marks.SocRaw + ', FgIbatUa ' + $marks.FgIbatUa +
                ', CellVbatMv ' + $marks.CellVbatMv
    } else {
        $fine = 'marks: no Device Parameters values under ACPI\QCOM057E'
    }
    $lines = @(
        ("start  : charge " + $first.Pct + " %, AC line " + $first.AC + ", " + $first.Note),
        ("end    : charge " + $second.Pct + " %, AC line " + $second.AC + ", " + $second.Note),
        ("delta  : " + $delta + " % over " + $ChargeSampleSeconds + " s"),
        ("verdict: " + $verdict),
        $fine
    )
    $lines | ForEach-Object { Add-Content -LiteralPath $log -Value $_ }
    $lines | ForEach-Object { Write-Host ("  " + $_) }
} else {
    Add-Content -LiteralPath $log -Value 'no power source answered: neither GetSystemPowerStatus nor Win32_Battery'
}

Capture 'WINDOWS POWER REPORT' {
    $batteryReport = Join-Path $OutDir "battery-report-$stamp.html"
    & powercfg /batteryreport /output $batteryReport | Out-String
    "file: $batteryReport"
}

$acceptance = Join-Path $PackageDir 'run-acceptance.ps1'
if ((Test-Path -LiteralPath $acceptance) -and $admin) {
    Capture 'ACCEPTANCE PROTOCOL' { & $acceptance -OutDir $OutDir -DeviceLabel $env:COMPUTERNAME }
} else {
    Capture 'ACCEPTANCE PROTOCOL' { 'skipped: no run-acceptance.ps1 or no administrator rights' }
}

# --- summary ------------------------------------------------------------
Section 'FILES COLLECTED'
$files = Get-ChildItem $OutDir -File | Where-Object { $_.LastWriteTime -gt (Get-Date).AddMinutes(-30) }
$files | ForEach-Object { Add-Content -LiteralPath $log -Value ($_.Name + '  ' + $_.Length + ' bytes') }
$files | ForEach-Object { Write-Host ("  " + $_.Name) -ForegroundColor Green }

$archive = Join-Path $OutDir "nabu-report-$stamp.zip"
$archivePath = 'the archive was not created - send the text report'
try {
    Compress-Archive -Path ($files | Where-Object { $_.Name -ne (Split-Path $archive -Leaf) }).FullName `
                     -DestinationPath $archive -Force
    $archivePath = $archive
    Write-Host ("  archive: " + $archive) -ForegroundColor Green
    Add-Content -LiteralPath $log -Value ("archive: " + $archive)
} catch {
    Add-Content -LiteralPath $log -Value ("archive not created: " + $_.Exception.Message)
}

Section 'WHAT TO SEND'
$tail = @(
    'Please send back two files from the folder:',
    ('  1) ' + $log),
    ('  2) ' + $archivePath),
    '',
    'If sending files is inconvenient, it is enough to copy the text of this report here.',
    'The report already contains: the Windows build, the state of the node and the service,',
    'the driver telemetry and, most importantly, the battery charge change over the interval.'
)
$tail | ForEach-Object { Add-Content -LiteralPath $log -Value $_; Write-Host $_ }

Write-Host ''
Write-Host ("Done. Report: " + $log) -ForegroundColor Cyan
