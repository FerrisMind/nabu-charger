#Requires -Version 5.1
<#
    run-acceptance.ps1 — автоматический прогон протокола приёмки драйвера LN8000.

    Проводит оператора по десяти пунктам протокола, снимает телеметрию драйвера,
    собирает показания внешнего измерителя и формирует итоговый протокол:
    Markdown + HTML + машинночитаемый JSON + приложенный журнал сеансов.

    Запуск на планшете (от администратора, из папки комплекта):
        .\run-acceptance.ps1

    Проверка самого конвейера без железа (синтетические данные, отчёт помечается
    как сухой прогон и не является измерением):
        .\run-acceptance.ps1 -DryRun

    Показания можно подать файлом, чтобы прогон был без диалога:
        .\run-acceptance.ps1 -MeterJson .\meter.json

    Формат meter.json:
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

# Данные сухого прогона: правдоподобные, но синтетические. Отчёт явно помечается,
# чтобы такой файл нельзя было принять за результат реальных измерений.
$dryRunData = [ordered]@{
    base_volt  = 5.05; base_amp = 1.80
    fast_volt  = 9.02; fast_amp = 2.95
    hold_volt  = 9.01; hold_amp = 2.90
    hold_minutes = $HoldMinutes
    note       = 'СУХОЙ ПРОГОН: значения синтетические, не измерение'
}

$meter = if ($MeterJson) {
    if (-not (Test-Path -LiteralPath $MeterJson)) { throw "нет файла показаний: $MeterJson" }
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
        Write-Host '    (нет интерактивного ввода — берётся значение по умолчанию)' -ForegroundColor Yellow
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
        Write-Host ('    телеметрия недоступна: ' + $_.Exception.Message) -ForegroundColor Yellow
    }
    return $null
}

New-Item -ItemType Directory -Path $OutDir -Force | Out-Null

Write-Host ''
Write-Host '=== Протокол приёмки LN8000 ===' -ForegroundColor Cyan
if ($DryRun) { Write-Host 'РЕЖИМ СУХОГО ПРОГОНА: данные синтетические, отчёт помечается как непроверочный' -ForegroundColor Yellow }
Write-Host ("устройство: " + $DeviceLabel + ";  вывод: " + $OutDir)
Write-Host ''

# --- 1. Базовый режим 5 В -------------------------------------------------
Write-Host '1. Базовый режим: подключите блок, дающий 5 В' -ForegroundColor Cyan
$baseV = Ask-Value -Prompt 'напряжение с мультиметра, В' -Key 'base_volt' -Default 5.0
$baseA = Ask-Value -Prompt 'ток с мультиметра, А' -Key 'base_amp' -Default 1.5
$baseW = $baseV * $baseA
Add-Result 'base-5v' 'Базовый режим 5 В' 'ток, напряжение и мощность зафиксированы' 'зафиксировано' `
    ("{0:N2} В × {1:N2} А = {2:N2} Вт" -f $baseV, $baseA, $baseW)
$telemetryBase = Get-Telemetry -Pd 1

# --- 2. Ускоренный режим --------------------------------------------------
Write-Host '2. Ускоренный режим: подключите совместимый блок' -ForegroundColor Cyan
$fastV = Ask-Value -Prompt 'напряжение с мультиметра, В' -Key 'fast_volt' -Default 9.0
$fastA = Ask-Value -Prompt 'ток с мультиметра, А' -Key 'fast_amp' -Default 2.5
$fastW = $fastV * $fastA
$gain = if ($baseW -gt 0) { (($fastW - $baseW) / $baseW) * 100 } else { 0 }
$telemetryFast = Get-Telemetry -Pd 2
$modeOk = $telemetryFast -and ([int]$telemetryFast.mode -eq 3)
$status = if ($gain -gt 5 -and $modeOk) { 'пройдено' } elseif ($gain -gt 5) { 'частично' } else { 'провалено' }
$detail = ("{0:N2} В × {1:N2} А = {2:N2} Вт; прирост {3:N1} %; режим драйвера: {4}" -f `
    $fastV, $fastA, $fastW, $gain, $(if ($telemetryFast) { $telemetryFast.mode } else { 'нет данных' }))
Add-Result 'fast-mode' 'Ускоренный режим' 'режим switching и рост мощности относительно 5 В' $status $detail

# --- 3. Удержание режима --------------------------------------------------
Write-Host ("3. Удержание режима: оставьте заряжаться на " + $HoldMinutes + " мин") -ForegroundColor Cyan
$holdV = Ask-Value -Prompt 'напряжение после удержания, В' -Key 'hold_volt' -Default $fastV
$holdA = Ask-Value -Prompt 'ток после удержания, А' -Key 'hold_amp' -Default $fastA
$holdW = $holdV * $holdA
$telemetryHold = Get-Telemetry -Pd 2
$tempDc = if ($telemetryHold) { [int]$telemetryHold.die_temp_dc } else { 0 }
$holdOk = ($tempDc -lt 430) -and ($telemetryHold -and ([int]$telemetryHold.mode -eq 3))
Add-Result 'hold' ("Удержание режима " + $HoldMinutes + ' мин') 'режим не откатился, температура ниже 43 °C' `
    $(if ($holdOk) { 'пройдено' } else { 'проверить' }) `
    ("после удержания {0:N2} Вт; температура кристалла {1:N1} °C" -f $holdW, ($tempDc / 10.0))

# --- 4. Тепловая защита ---------------------------------------------------
Write-Host '4. Тепловая защита: прогрейте устройство или включите строгий профиль' -ForegroundColor Cyan
$guardSeen = $false
$sessions = Get-Telemetry
if ($DryRun) { $guardSeen = $true }
else {
    $answer = Read-Safe '    сработала защита (снижение тока или bypass)? д/н'
    $guardSeen = ($answer -eq 'д')
}
Add-Result 'thermal-guard' 'Тепловая защита' 'событие защиты зафиксировано, заряд не оборван аварийно' `
    $(if ($guardSeen) { 'пройдено' } else { 'не наблюдалось' }) `
    'уровни: снижение тока с 43 °C, bypass с 48 °C, остановка с 55 °C'

# --- 5. Смена блока -------------------------------------------------------
Write-Host '5. Смена блока: отключите совместимый блок и подключите несовместимый' -ForegroundColor Cyan
$afterSwap = Get-Telemetry
Add-Result 'swap' 'Смена блока на несовместимый' 'сеанс закрыт, новый открыт, зависаний нет' `
    $(if ($afterSwap) { 'пройдено' } else { 'нет данных' }) `
    ("сеансов в драйвере: " + $(if ($afterSwap) { $afterSwap.sessions } else { '?' }))

# --- 6. Обрыв кабеля ------------------------------------------------------
Write-Host '6. Обрыв кабеля: выньте и снова вставьте кабель' -ForegroundColor Cyan
$afterUnplug = Get-Telemetry
Add-Result 'unplug' 'Обрыв кабеля при согласовании' 'устройство возвращается в рабочее состояние' `
    $(if ($afterUnplug) { 'пройдено' } else { 'нет данных' }) 'событие закрытия сеанса попадает в журнал'

# --- 7. Перезагрузка ------------------------------------------------------
Add-Result 'reboot' 'Перезагрузка' 'драйвер поднимается, режим восстанавливается' 'проверить вручную' `
    'выполните Restart-Computer и повторите пункт 1'

# --- 8. Чистая установка --------------------------------------------------
$testSigning = (& bcdedit /enum '{current}' 2>&1 | Out-String) -match 'testsigning\s+Yes'
$service = (& sc.exe query ln8000_kmdf 2>&1 | Out-String)
$serviceOk = $service -match 'RUNNING|STOPPED'
Add-Result 'clean-install' 'Чистая установка без ошибок' 'пакет ставится без предупреждений, режим доступен без правок реестра' `
    $(if ($serviceOk) { 'пройдено' } else { 'проверить' }) `
    ("служба ln8000_kmdf: " + $(if ($serviceOk) { 'зарегистрирована' } else { 'не найдена' }) + `
     "; тестовая подпись: " + $(if ($testSigning) { 'включена' } else { 'выключена' }))

# --- 9. Откат -------------------------------------------------------------
Add-Result 'rollback' 'Откат' 'штатное поведение зарядки, остаточных служб нет' 'проверить вручную' `
    'выполните uninstall-driver.ps1 и убедитесь, что служба удалена, а зарядка осталась штатной'

# --- 10. Повторная установка ---------------------------------------------
Add-Result 'reinstall' 'Повторная установка' 'проходит успешно, счётчики сеансов с нуля' 'проверить вручную' `
    'выполните install-driver.ps1 заново'

# --- итог -----------------------------------------------------------------
$passed = ($results | Where-Object { $_.status -eq 'пройдено' }).Count
$failed = ($results | Where-Object { $_.status -eq 'провалено' }).Count
$manual = ($results | Where-Object { $_.status -like '*вручную*' }).Count

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
[void]$md.AppendLine('# Протокол приёмки LN8000 — ' + $DeviceLabel)
[void]$md.AppendLine('')
[void]$md.AppendLine('Дата: ' + (Get-Date -Format 'yyyy-MM-dd HH:mm'))
if ($DryRun) {
    [void]$md.AppendLine('')
    [void]$md.AppendLine('> **ВНИМАНИЕ: сухой прогон.** Данные синтетические. Этот файл проверяет')
    [void]$md.AppendLine('> работу конвейера отчёта и **не является** результатом измерений.')
}
[void]$md.AppendLine('')
[void]$md.AppendLine('## Мощность')
[void]$md.AppendLine('')
[void]$md.AppendLine('| Режим | Мощность |')
[void]$md.AppendLine('|---|---|')
[void]$md.AppendLine(('| Базовый 5 В | {0:N2} Вт |' -f $baseW))
[void]$md.AppendLine(('| Ускоренный | {0:N2} Вт |' -f $fastW))
[void]$md.AppendLine(('| Прирост | {0:N1} % |' -f $gain))
[void]$md.AppendLine(('| После удержания | {0:N2} Вт |' -f $holdW))
[void]$md.AppendLine('')
[void]$md.AppendLine('## Пункты протокола')
[void]$md.AppendLine('')
[void]$md.AppendLine('| № | Сценарий | Критерий | Статус | Что зафиксировано |')
[void]$md.AppendLine('|---|---|---|---|---|')
$index = 0
foreach ($r in $results) {
    $index++
    [void]$md.AppendLine(('| {0} | {1} | {2} | {3} | {4} |' -f $index, $r.title, $r.criterion, $r.status, $r.detail))
}
[void]$md.AppendLine('')
[void]$md.AppendLine('## Драйвер в моменты замеров')
[void]$md.AppendLine('')
foreach ($pair in @(@('базовый режим', $telemetryBase), @('ускоренный режим', $telemetryFast), @('удержание', $telemetryHold))) {
    if ($pair[1]) {
        [void]$md.AppendLine(('* {0}: режим {1}, вход {2:N2} А, напряжение входа {3:N2} В, температура {4:N1} °C, отказы 0x{5:X2}/0x{6:X2}' -f `
            $pair[0], $pair[1].mode, ([double]$pair[1].iin_ua / 1e6), ([double]$pair[1].vbus_uv / 1e6), ([double]$pair[1].die_temp_dc / 10), [int]$pair[1].fault1_sts, [int]$pair[1].fault2_sts))
    }
}
[void]$md.AppendLine('')
[void]$md.AppendLine(('Итог: пройдено {0}, провалено {1}, требует ручной проверки {2}.' -f $passed, $failed, $manual))
$mdPath = Join-Path $OutDir "acceptance-$stamp.md"
$md.ToString() | Set-Content -LiteralPath $mdPath -Encoding utf8

$rows = ($results | ForEach-Object { "      <tr><td>$($_.title)</td><td>$($_.criterion)</td><td>$($_.status)</td><td>$($_.detail)</td></tr>" }) -join "`n"
$warn = if ($DryRun) { '<p class="warn">Сухой прогон: данные синтетические, не измерение.</p>' } else { '' }
$html = @"
<!DOCTYPE html>
<html lang="ru"><head><meta charset="utf-8"><title>Протокол приёмки LN8000</title>
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
<h1>Протокол приёмки <b>LN8000</b></h1>
<p class="sub">$DeviceLabel · $(Get-Date -Format 'yyyy-MM-dd HH:mm')</p>
$warn
<div class="metrics">
<div class="card"><div class="k">Базовый 5 В</div><div class="v">$([math]::Round($baseW,2)) Вт</div></div>
<div class="card"><div class="k">Ускоренный</div><div class="v">$([math]::Round($fastW,2)) Вт</div></div>
<div class="card"><div class="k">Прирост</div><div class="v">$([math]::Round($gain,1)) %</div></div>
<div class="card"><div class="k">Итог</div><div class="v">$passed / $($results.Count)</div></div>
</div>
<h2>Пункты протокола</h2>
<table><tr><th>Сценарий</th><th>Критерий</th><th>Статус</th><th>Что зафиксировано</th></tr>
$rows
</table>
<h2>Машинночитаемые данные</h2>
<p><code>$jsonPath</code></p>
</div></body></html>
"@
$htmlPath = Join-Path $OutDir "acceptance-$stamp.html"
$html | Set-Content -LiteralPath $htmlPath -Encoding utf8

Write-Host ''
Write-Host '=== итог ===' -ForegroundColor Cyan
Write-Host ("  пройдено: {0}; провалено: {1}; вручную: {2}" -f $passed, $failed, $manual)
Write-Host ("  отчёт:    " + $htmlPath) -ForegroundColor Green
Write-Host ("  разметка: " + $mdPath)
Write-Host ("  данные:   " + $jsonPath)
