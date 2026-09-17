#Requires -Version 5.1
<#
    bring-up.ps1 — один прогон на планшете: проверка → установка → сбор доказательств.

    Запуск (PowerShell от имени администратора, из папки комплекта):
        .\bring-up.ps1

    Что делает:
      1) фиксирует состояние системы (сборка Windows, архитектура, тестовая подпись);
      2) ищет узел ACPI\QCOM057E и службу драйвера;
      3) ставит драйвер, если рядом лежит пакет и он ещё не установлен;
      4) снимает телеметрию драйвера (режим, сеансы, журнал);
      5) с двумя интервалами измеряет заряд батареи через WMI — это показывает,
         идёт ли заряд вообще, без всякого мультиметра;
      6) складывает всё в ОДИН текстовый файл и один архив, которые нужно
         прислать назад.

    Ничего не пишет в прошивку и не меняет настройки питания. Откат —
    uninstall-driver.ps1 из этого же комплекта.
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
        Add-Content -LiteralPath $log -Value ("ОШИБКА: " + $_.Exception.Message)
        Write-Host ("  ошибка: " + $_.Exception.Message) -ForegroundColor Yellow
        return ''
    }
}

function Is-Admin {
    $identity = [Security.Principal.WindowsIdentity]::GetCurrent()
    $principal = New-Object Security.Principal.WindowsPrincipal($identity)
    return $principal.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)
}

Add-Content -LiteralPath $log -Value 'Отчёт по быстрой зарядке nabu (Xiaomi Pad 5)'
Add-Content -LiteralPath $log -Value ("Время запуска: " + (Get-Date -Format 'yyyy-MM-dd HH:mm:ss'))
Write-Host ''
Write-Host '=== сбор доказательств по зарядке nabu ===' -ForegroundColor Cyan
Write-Host ("отчёт будет здесь: " + $log)

$admin = Is-Admin
Capture 'ПРАВА' { if ($admin) { 'администратор: да' } else { 'администратор: НЕТ — часть шагов будет недоступна' } }

Capture 'СИСТЕМА' {
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

Capture 'ПОДПИСЬ ДРАЙВЕРОВ' { & bcdedit /enum '{current}' | Select-String -Pattern 'testsigning|nointegritychecks|hypervisorlaunchtype' }

Capture 'УЗЕЛ ACPI\QCOM057E' {
    $device = Get-PnpDevice -PresentOnly -ErrorAction SilentlyContinue |
        Where-Object { $_.InstanceId -like '*QCOM057E*' }
    if (-not $device) { return 'узел не найден — драйверу не на чем стартовать' }
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

Capture 'ПАКЕТ ДРАЙВЕРА В ХРАНИЛИЩЕ' {
    (& pnputil /enum-drivers | Out-String) -split "`r?`n`r?`n" |
        Where-Object { $_ -match 'ln8000|nabu' }
}

Capture 'СЛУЖБА ДРАЙВЕРА' {
    & sc.exe query ln8000_kmdf
    & sc.exe qc ln8000_kmdf
}

$inf = Join-Path $PackageDir 'ln8000_kmdf.inf'
$skipReason = ''
if ($SkipInstall) {
    $skipReason = 'указан -SkipInstall'
} elseif (-not (Test-Path -LiteralPath $inf)) {
    $skipReason = "нет файла $inf"
} elseif (-not $admin) {
    $skipReason = 'нет прав администратора'
}

if ([string]::IsNullOrEmpty($skipReason)) {
    Capture 'УСТАНОВКА ДРАЙВЕРА' {
        & pnputil /add-driver $inf /install
        & sc.exe start ln8000_kmdf
    }
} else {
    Capture 'УСТАНОВКА ДРАЙВЕРА' { 'пропущена: ' + $skipReason }
}

$diag = Join-Path $PackageDir 'nabu-ln8000.ps1'
if (Test-Path -LiteralPath $diag) {
    Capture 'ДРАЙВЕР: СОСТОЯНИЕ' { & $diag status }
    Capture 'ДРАЙВЕР: СЕАНСЫ' { & $diag sessions }
    Capture 'ДРАЙВЕР: СНИМОК В ЖУРНАЛ' { & $diag journal (Join-Path $OutDir "driver-journal-$stamp.jsonl") }
} else {
    Capture 'ДРАЙВЕР' { 'утилита nabu-ln8000.ps1 не найдена рядом со скриптом' }
}

Capture 'БАТАРЕЯ: ПЕРВЫЙ ЗАМЕР' {
    Get-CimInstance Win32_Battery -ErrorAction SilentlyContinue |
        Select-Object Name, DeviceID, BatteryStatus, EstimatedChargeRemaining,
                      EstimatedRunTime, DesignVoltage, Chemistry |
        Format-List
}

Section ("БАТАРЕЯ: ВТОРОЙ ЗАМЕР ЧЕРЕЗ " + $ChargeSampleSeconds + " с")
Write-Host ("  ждём " + $ChargeSampleSeconds + " с для оценки изменения заряда...") -ForegroundColor Yellow
$first = Get-CimInstance Win32_Battery -ErrorAction SilentlyContinue | Select-Object -First 1
Start-Sleep -Seconds $ChargeSampleSeconds
$second = Get-CimInstance Win32_Battery -ErrorAction SilentlyContinue | Select-Object -First 1
if ($first -and $second) {
    $delta = [int]$second.EstimatedChargeRemaining - [int]$first.EstimatedChargeRemaining
    if ($delta -gt 0) {
        $verdict = 'заряд РАСТЁТ'
    } elseif ($delta -lt 0) {
        $verdict = 'заряд ПАДАЕТ'
    } else {
        $verdict = 'заряд НЕ МЕНЯЕТСЯ'
    }
    $lines = @(
        ("начало: заряд " + $first.EstimatedChargeRemaining + " %, состояние " + $first.BatteryStatus),
        ("конец : заряд " + $second.EstimatedChargeRemaining + " %, состояние " + $second.BatteryStatus),
        ("разница: " + $delta + " % за " + $ChargeSampleSeconds + " с"),
        ("вывод  : " + $verdict)
    )
    $lines | ForEach-Object { Add-Content -LiteralPath $log -Value $_ }
    $lines | ForEach-Object { Write-Host ("  " + $_) }
} else {
    Add-Content -LiteralPath $log -Value 'WMI не вернул сведений о батарее'
}

Capture 'ОТЧЁТ WINDOWS О ПИТАНИИ' {
    $batteryReport = Join-Path $OutDir "battery-report-$stamp.html"
    & powercfg /batteryreport /output $batteryReport | Out-String
    "файл: $batteryReport"
}

$acceptance = Join-Path $PackageDir 'run-acceptance.ps1'
if ((Test-Path -LiteralPath $acceptance) -and $admin) {
    Capture 'ПРОТОКОЛ ПРИЁМКИ' { & $acceptance -OutDir $OutDir -DeviceLabel $env:COMPUTERNAME }
} else {
    Capture 'ПРОТОКОЛ ПРИЁМКИ' { 'пропущен: нет run-acceptance.ps1 или прав администратора' }
}

# --- итог ---------------------------------------------------------------
Section 'СОБРАННЫЕ ФАЙЛЫ'
$files = Get-ChildItem $OutDir -File | Where-Object { $_.LastWriteTime -gt (Get-Date).AddMinutes(-30) }
$files | ForEach-Object { Add-Content -LiteralPath $log -Value ($_.Name + '  ' + $_.Length + ' байт') }
$files | ForEach-Object { Write-Host ("  " + $_.Name) -ForegroundColor Green }

$archive = Join-Path $OutDir "nabu-report-$stamp.zip"
$archivePath = 'архив не создался — пришлите текстовый отчёт'
try {
    Compress-Archive -Path ($files | Where-Object { $_.Name -ne (Split-Path $archive -Leaf) }).FullName `
                     -DestinationPath $archive -Force
    $archivePath = $archive
    Write-Host ("  архив: " + $archive) -ForegroundColor Green
    Add-Content -LiteralPath $log -Value ("архив: " + $archive)
} catch {
    Add-Content -LiteralPath $log -Value ("архив не создан: " + $_.Exception.Message)
}

Section 'ЧТО ПРИСЛАТЬ'
$tail = @(
    'Пришлите, пожалуйста, два файла из папки:',
    ('  1) ' + $log),
    ('  2) ' + $archivePath),
    '',
    'Если передать файлы неудобно — достаточно скопировать сюда текст этого отчёта.',
    'В отчёте уже есть: сборка Windows, состояние узла и службы, телеметрия драйвера',
    'и главное — изменение заряда батареи за интервал.'
)
$tail | ForEach-Object { Add-Content -LiteralPath $log -Value $_; Write-Host $_ }

Write-Host ''
Write-Host ("Готово. Отчёт: " + $log) -ForegroundColor Cyan
