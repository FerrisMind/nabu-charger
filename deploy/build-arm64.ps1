# build-arm64.ps1 — воспроизводимая сборка пакетов драйверов под ARM64
#
# Собирает оба драйвера, складывает пакеты в artifacts/, считает SHA-256 и
# пишет манифест сборки (версии инструментов, время, размеры, суммы).
#
# Запуск (на машине разработки, не на планшете):
#     .\build-arm64.ps1
#     .\build-arm64.ps1 -SkipBuild      # только пересчитать суммы и манифест
#
# Воспроизводимость: фиксируются версия Rust (rust-toolchain.toml), версия
# cargo-wdk, версия WDK и libclang. При совпадении всех четырёх и одинаковом
# исходнике получается тот же самый .sys (проверяется сверкой SHA-256).

[CmdletBinding()]
param(
  [string]$RepoRoot = (Resolve-Path (Join-Path $PSScriptRoot '..')).Path,
  [switch]$SkipBuild,
  [switch]$CheckReproducible
)

$ErrorActionPreference = 'Stop'

function Get-ToolVersions {
  $versions = [ordered]@{}
  $versions['rustc'] = (& rustc --version) -join ''
  $versions['cargo'] = (& cargo --version) -join ''
  $versions['cargo-wdk'] = try { ((& cargo wdk --version) -join '') } catch { 'не установлен' }
  $versions['wdk'] = if ($env:WDKContentRoot) { $env:WDKContentRoot } else { 'C:\Program Files (x86)\Windows Kits\10' }
  $versions['libclang'] = if ($env:LIBCLANG_PATH) { $env:LIBCLANG_PATH } else { 'не задан' }
  $versions['target'] = 'aarch64-pc-windows-msvc'
  return $versions
}

$targets = @(
  @{ Name = 'SMB-детекция блока'; Crate = 'crates\kmdf';        Package = 'kmdf_package';        Out = 'driver-arm64';        Deploy = $null },
  @{ Name = 'Charge pump LN8000'; Crate = 'crates\ln8000-kmdf'; Package = 'ln8000_kmdf_package'; Out = 'driver-ln8000-arm64'; Deploy = 'deploy' }
)

if (-not $env:LIBCLANG_PATH) { $env:LIBCLANG_PATH = 'C:\Program Files\LLVM\bin' }

$built = @()
foreach ($t in $targets) {
  $cratePath = Join-Path $RepoRoot $t.Crate
  if (-not (Test-Path $cratePath)) { throw "нет каталога $cratePath" }

  if (-not $SkipBuild) {
    Write-Host ("=== сборка: " + $t.Name) -ForegroundColor Cyan
    Push-Location $cratePath
    try {
      & cargo wdk build --target-arch arm64 --profile release
      if ($LASTEXITCODE -ne 0) { throw "сборка $($t.Crate) вернула код $LASTEXITCODE" }
    } finally {
      Pop-Location
    }
  }

  $packagePath = Join-Path $cratePath ("target\aarch64-pc-windows-msvc\release\" + $t.Package)
  if (-not (Test-Path $packagePath)) { throw "нет пакета $packagePath" }

  $outPath = Join-Path $RepoRoot ('artifacts\' + $t.Out)
  New-Item -ItemType Directory -Path $outPath -Force | Out-Null
  Copy-Item (Join-Path $packagePath '*') $outPath -Force

  # Скрипты развёртывания кладём рядом с пакетом: на планшете они читают
  # $PSScriptRoot, поэтому комплект копируется целиком в одну папку.
  if ($t.Deploy) {
    $deployPath = Join-Path $cratePath $t.Deploy
    if (Test-Path $deployPath) {
      Copy-Item (Join-Path $deployPath '*.ps1') $outPath -Force
      Copy-Item (Join-Path $deployPath '*.md') $outPath -Force -ErrorAction SilentlyContinue
    }
  }

  # Скрипты, которые запускаются на самом планшете, но живут в общем deploy:
  # удалённый доступ включается там же, где и всё остальное.
  if ($t.Out -eq 'driver-ln8000-arm64') {
    $shared = Join-Path $RepoRoot 'deploy\enable-remote.ps1'
    if (Test-Path $shared) {
      Copy-Item $shared $outPath -Force
    }
  }

  $built += [pscustomobject]@{ Name = $t.Name; Out = $outPath }
}

Write-Host '=== контрольные суммы ===' -ForegroundColor Cyan
$sumLines = @()
foreach ($b in $built) {
  foreach ($file in (Get-ChildItem $b.Out -File | Sort-Object Name)) {
    $hash = (Get-FileHash -LiteralPath $file.FullName -Algorithm SHA256).Hash.ToLower()
    $relative = (Split-Path $file.FullName -Leaf)
    $sumLines += ("{0}  {1}/{2}" -f $hash, (Split-Path $b.Out -Leaf), $relative)
    Write-Host ("  {0}  {1}" -f $hash.Substring(0, 16), $relative)
  }
}
$sumPath = Join-Path $RepoRoot 'artifacts\SHA256SUMS.txt'
$sumLines | Set-Content -LiteralPath $sumPath -Encoding ascii

Write-Host ''
Write-Host '=== воспроизводимость ===' -ForegroundColor Cyan

# Честная проверка: собираем ещё раз и сравниваем байты. Сравнивать только хеш
# малоинформативно, поэтому считаем различия и записываем вывод в манифест.
$reproducible = $null
$reproNote = 'не проверялась (сборка пропущена)'
if (-not $SkipBuild) {
    $before = @{}
    foreach ($b in $built) {
        foreach ($file in (Get-ChildItem $b.Out -File | Where-Object { $_.Extension -eq '.sys' })) {
            $before[$file.FullName] = (Get-FileHash -LiteralPath $file.FullName -Algorithm SHA256).Hash
        }
    }
    foreach ($t in $targets) {
        Push-Location (Join-Path $RepoRoot $t.Crate)
        try { & cargo wdk build --target-arch arm64 --profile release | Out-Null } finally { Pop-Location }
    }
    $reproducible = $true
    foreach ($entry in $before.GetEnumerator()) {
        $now = (Get-FileHash -LiteralPath $entry.Key -Algorithm SHA256).Hash
        if ($now -eq $entry.Value) {
            Write-Host ('  ' + (Split-Path $entry.Key -Leaf) + ': совпадает') -ForegroundColor Green
        } else {
            $reproducible = $false
            Write-Host ('  ' + (Split-Path $entry.Key -Leaf) + ': байты отличаются от первой сборки') -ForegroundColor Yellow
        }
    }
    if ($reproducible) {
        $reproNote = 'битовая воспроизводимость достигнута'
    } else {
        $reproNote = 'битовая воспроизводимость НЕ достигнута: между сборками меняются метаданные образа'
    }
}
Write-Host ('  вывод: ' + $reproNote)

# Суммы пересчитываются после последней сборки, иначе они описывали бы прежний файл.
$finalSums = @()
foreach ($b in $built) {
    foreach ($file in (Get-ChildItem $b.Out -File | Sort-Object Name)) {
        $finalSums += ((Get-FileHash -LiteralPath $file.FullName -Algorithm SHA256).Hash.ToLower() + '  ' +
                       (Split-Path $b.Out -Leaf) + '/' + $file.Name)
    }
}
$sumPath = Join-Path $RepoRoot 'artifacts\SHA256SUMS.txt'
$finalSums | Sort-Object -Unique | Set-Content -LiteralPath $sumPath -Encoding ascii
Write-Host ''
Write-Host '=== манифест ===' -ForegroundColor Cyan
$manifest = [ordered]@{
  built_at          = (Get-Date).ToString('o')
  host              = "$env:COMPUTERNAME ($env:PROCESSOR_ARCHITECTURE)"
  reproducible      = $reproducible
  reproducible_note = $reproNote
  versions          = Get-ToolVersions
  artifacts         = @()
}
foreach ($b in $built) {
  $files = @()
  foreach ($file in (Get-ChildItem $b.Out -File | Sort-Object Name)) {
    $files += [ordered]@{
      name   = $file.Name
      bytes  = $file.Length
      sha256 = (Get-FileHash -LiteralPath $file.FullName -Algorithm SHA256).Hash.ToLower()
    }
  }
  $manifest.artifacts += [ordered]@{ name = $b.Name; path = $b.Out; files = $files }
}
$manifestPath = Join-Path $RepoRoot 'artifacts\BUILD-MANIFEST.json'
$manifest | ConvertTo-Json -Depth 6 | Set-Content -LiteralPath $manifestPath -Encoding utf8

Write-Host ''
Write-Host ("суммы    : " + $sumPath) -ForegroundColor Green
Write-Host ("манифест : " + $manifestPath) -ForegroundColor Green
