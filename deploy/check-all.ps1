#Requires -Version 7.0
<#
    check-all.ps1 — единый прогон всех проверок проекта одной командой.

    Запуск:
        .\check-all.ps1
        .\check-all.ps1 -SkipBuilds     # без сборки драйверов (быстро)

    Что прогоняется и куда пишется результат:
      1. форматирование        cargo fmt --check
      2. линтер                cargo clippy --workspace -- -D warnings
      3. тесты                 cargo test --workspace
      4. сборка без std        оба ядра
      5. драйверы ARM64        cargo wdk build (оба) + SHA-256
      6. комплект              deploy/verify-package.ps1
      7. источники и дамп      deploy/verify-sources.ps1
      8. сборка                deploy/build-arm64.ps1 (проверка воспроизводимости)

    Отчёт: artifacts\check-all.txt, вердикт — в конце. Код возврата 0, если всё
    сошлось. Скрипт рассчитан на то, что его запустит сторонний инженер при
    передаче работ: он не требует ни планшета, ни доступа к железу.
#>
[CmdletBinding()]
param(
    [string]$Repo = (Join-Path $PSScriptRoot '..'),
    [string]$Out = (Join-Path $PSScriptRoot '..\artifacts\check-all.txt'),
    [switch]$SkipBuilds
)

$ErrorActionPreference = 'Continue'
$lines = New-Object System.Collections.Generic.List[string]
$failures = New-Object System.Collections.Generic.List[string]
$passed = 0

function Say {
    param([string]$Text = '')
    $lines.Add($Text) | Out-Null
    Write-Host $Text
}

function Step {
    param([string]$Title, [scriptblock]$Action)
    Say ''
    Say ('--- ' + $Title)
    $watch = [Diagnostics.Stopwatch]::StartNew()
    try {
        $result = & $Action
        $watch.Stop()
        if ($result -eq $true) {
            $script:passed++
            Say ('    результат: ок (' + [math]::Round($watch.Elapsed.TotalSeconds, 1) + ' с)')
        } else {
            $failures.Add($Title) | Out-Null
            Say ('    результат: ОШИБКА (' + [math]::Round($watch.Elapsed.TotalSeconds, 1) + ' с)')
        }
    } catch {
        $watch.Stop()
        $failures.Add($Title + ': ' + $_.Exception.Message) | Out-Null
        Say ('    результат: ИСКЛЮЧЕНИЕ: ' + $_.Exception.Message)
    }
}

$env:CARGO_TERM_COLOR = 'never'
$env:LIBCLANG_PATH = if ($env:LIBCLANG_PATH) { $env:LIBCLANG_PATH } else { 'C:\Program Files\LLVM\bin' }

Say '========================================================================='
Say ' Полный прогон проверок проекта nabu (без железа)'
Say (' Время: ' + (Get-Date -Format 'yyyy-MM-dd HH:mm:ss'))
Say (' Каталог: ' + (Resolve-Path -LiteralPath $Repo))
Say '========================================================================='

Push-Location $Repo
try {
    Step 'Форматирование (cargo fmt --check)' {
        $out = & cargo fmt --all --check 2>&1
        if ($out) { $out | Select-Object -First 10 | ForEach-Object { Say ('    ' + $_) } }
        $LASTEXITCODE -eq 0
    }

    Step 'Линтер (cargo clippy -D warnings)' {
        $out = & cargo clippy --workspace --all-targets -- -D warnings 2>&1
        $bad = $out | Select-String -Pattern '^error|^warning:'
        if ($bad) { $bad | Select-Object -First 10 | ForEach-Object { Say ('    ' + $_.Line) } }
        $LASTEXITCODE -eq 0
    }

    Step 'Тесты (cargo test --workspace)' {
        $out = & cargo test --workspace 2>&1
        $total = 0
        $failed = 0
        foreach ($line in $out) {
            if ($line -match 'test result: ok\. (\d+) passed') { $total += [int]$Matches[1] }
            if ($line -match 'test result: FAILED') { $failed++ }
        }
        Say ('    тестов пройдено: ' + $total + ', провалов: ' + $failed)
        ($LASTEXITCODE -eq 0) -and ($failed -eq 0)
    }

    Step 'Сборка ядер без стандартной библиотеки' {
        $core = & cargo build -p charger-core --no-default-features 2>&1; $c1 = $LASTEXITCODE
        $ln = & cargo build -p ln8000 --no-default-features 2>&1; $c2 = $LASTEXITCODE
        Say ('    charger-core: ' + $c1 + ', ln8000: ' + $c2)
        ($c1 -eq 0) -and ($c2 -eq 0)
    }
} finally {
    Pop-Location
}

if (-not $SkipBuilds) {
    Step 'Сборка драйверов под ARM64 и контрольные суммы' {
        $out = & (Join-Path $PSScriptRoot 'build-arm64.ps1') 2>&1
        $out | Select-String -Pattern 'Finished building|воспроизводимость|вывод:|ERROR' |
            ForEach-Object { Say ('    ' + $_.Line.Trim()) }
        $LASTEXITCODE -eq 0
    }
}

Step 'Автономная проверка комплекта' {
    $out = & (Join-Path $PSScriptRoot 'verify-package.ps1') 2>&1
    $out | Select-String -Pattern 'ИТОГ|РАСХ|нет суммы' | ForEach-Object { Say ('    ' + $_.Line.Trim()) }
    $LASTEXITCODE -eq 0
}

Step 'Сверка с источниками и живым дампом' {
    $out = & (Join-Path $PSScriptRoot 'verify-sources.ps1') 2>&1
    $out | Select-String -Pattern 'Итог|РАСХ' | ForEach-Object { Say ('    ' + $_.Line.Trim()) }
    $LASTEXITCODE -eq 0
}

Say ''
Say '========================================================================='
if ($failures.Count -eq 0) {
    Say (' ВЕРДИКТ: всё сошлось. Шагов пройдено: ' + $passed)
} else {
    Say (' ВЕРДИКТ: проблем ' + $failures.Count + ' из ' + ($passed + $failures.Count))
    foreach ($item in $failures) { Say ('   - ' + $item) }
}
Say (' Время: ' + (Get-Date -Format 'yyyy-MM-dd HH:mm:ss'))
Say '========================================================================='

$outPath = [IO.Path]::GetFullPath($Out)
New-Item -ItemType Directory -Path (Split-Path $outPath) -Force | Out-Null
$lines | Set-Content -LiteralPath $outPath -Encoding utf8
Write-Host ''
Write-Host ('Отчёт: ' + $outPath) -ForegroundColor Green

if ($failures.Count -gt 0) { exit 1 }
exit 0
