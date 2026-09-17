#Requires -Version 5.1
<#
    verify-package.ps1 — автономная проверка комплекта драйвера, без железа.

    Запуск на любом компьютере с Windows:
        .\verify-package.ps1

    Проверяет то, что можно проверить без планшета:
      * все ли файлы комплекта на месте;
      * действительно ли драйвер собран под ARM64 (по заголовку PE);
      * совпадают ли контрольные суммы с artifacts\SHA256SUMS.txt;
      * содержит ли INF нужный идентификатор оборудования и имя службы;
      * подписан ли пакет (наличие .cat и сертификата).

    Возвращает 0, если всё сходится, и 1 при первой несостыковке.
    Годится как приёмочный шлюз при передаче: его может запустить любой
    инженер и получить тот же ответ.
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
            Write-Host ("  [ок]  " + $Title) -ForegroundColor Green
        } else {
            Write-Host ("  [нет] " + $Title) -ForegroundColor Red
            $problems.Add($Title)
        }
    } catch {
        Write-Host ("  [нет] " + $Title + ' — ' + $_.Exception.Message) -ForegroundColor Red
        $problems.Add($Title + ': ' + $_.Exception.Message)
    }
}

Write-Host ''
Write-Host '=== проверка комплекта драйвера nabu ===' -ForegroundColor Cyan
Write-Host ("комплект : " + (Resolve-Path -LiteralPath $KitDir))
Write-Host ("суммы    : " + (Resolve-Path -LiteralPath $SumFile))
Write-Host ''

$required = @(
    'ln8000_kmdf.sys', 'ln8000_kmdf.inf', 'ln8000_kmdf.cat', 'WDRLocalTestCert.cer',
    'install-driver.ps1', 'update-driver.ps1', 'uninstall-driver.ps1',
    'nabu-ln8000.ps1', 'run-acceptance.ps1', 'bring-up.ps1', 'enable-remote.ps1'
)

Write-Host 'Файлы:' -ForegroundColor Cyan
foreach ($name in $required) {
    Check ("есть " + $name) { Test-Path -LiteralPath (Join-Path $KitDir $name) }
}

Write-Host ''
Write-Host 'Разрядность и подпись:' -ForegroundColor Cyan
Check 'драйвер собран под ARM64 (PE Machine = 0xAA64)' {
    $path = Join-Path $KitDir 'ln8000_kmdf.sys'
    $bytes = [IO.File]::ReadAllBytes($path)
    $peOffset = [BitConverter]::ToInt32($bytes, 0x3C)
    $machine = [BitConverter]::ToUInt16($bytes, $peOffset + 4)
    Write-Verbose ("Machine = 0x{0:X4}" -f $machine)
    $machine -eq 0xAA64
}

Check 'размер драйвера в разумных пределах (10..200 КБ)' {
    $size = (Get-Item -LiteralPath (Join-Path $KitDir 'ln8000_kmdf.sys')).Length
    $size -gt 10240 -and $size -lt 204800
}

Write-Host ''
Write-Host 'Содержимое INF:' -ForegroundColor Cyan
$infText = Get-Content -LiteralPath (Join-Path $KitDir 'ln8000_kmdf.inf') -Raw
Check 'INF знает идентификатор ACPI\QCOM057E' { $infText -match 'ACPI\\QCOM057E' }
Check 'INF поднимает службу ln8000_kmdf' { $infText -match 'ServiceBinary.*ln8000_kmdf\.sys' }
Check 'INF задаёт параметры профиля (IinLimitUa, VbatFloatUv)' {
    ($infText -match 'IinLimitUa') -and ($infText -match 'VbatFloatUv')
}
Check 'INF позволяет выбрать профиль защит (ProtectionProfile) без пересборки' {
    $infText -match 'ProtectionProfile'
}
Check 'INF задаёт период телеметрии' { $infText -match 'TelemetryMs' }
Check 'INF задаёт пороги защиты (температура и ток)' {
    ($infText -match 'TempReduceDc') -and ($infText -match 'TempBypassDc') -and ($infText -match 'TempStopDc') -and ($infText -match 'IinTargetUa')
}

Write-Host ''
Write-Host 'Контрольные суммы:' -ForegroundColor Cyan
$sums = @{}
foreach ($line in (Get-Content -LiteralPath $SumFile)) {
    $parts = $line -split '\s+', 2
    if ($parts.Count -eq 2 -and $parts[1].StartsWith($SumPrefix)) {
        $sums[$parts[1].Substring($SumPrefix.Length)] = $parts[0]
    }
}
Check ('в файле сумм есть записи для комплекта (' + $sums.Count + ' шт.)') { $sums.Count -ge $required.Count }

foreach ($name in $required) {
    if (-not $sums.ContainsKey($name)) {
        Write-Host ("  [нет] суммы для " + $name) -ForegroundColor Red
        $problems.Add('нет суммы для ' + $name)
        $checks++
        continue
    }
    Check ("сумма сходится: " + $name) {
        $actual = (Get-FileHash -LiteralPath (Join-Path $KitDir $name) -Algorithm SHA256).Hash.ToLower()
        $actual -eq $sums[$name]
    }
}

Write-Host ''
Write-Host 'Синтаксис скриптов:' -ForegroundColor Cyan
foreach ($name in $required | Where-Object { $_ -like '*.ps1' }) {
    Check ("разбирается без ошибок: " + $name) {
        $errors = $null
        $null = [System.Management.Automation.Language.Parser]::ParseFile(
            (Join-Path $KitDir $name), [ref]$null, [ref]$errors)
        $errors.Count -eq 0
    }
}

Write-Host ''
if ($problems.Count -eq 0) {
    Write-Host ("ИТОГ: комплект в порядке, проверок пройдено " + $checks) -ForegroundColor Green
    exit 0
}
Write-Host ("ИТОГ: проблем " + $problems.Count + " из " + $checks + " проверок") -ForegroundColor Red
$problems | ForEach-Object { Write-Host ("  - " + $_) }
exit 1
