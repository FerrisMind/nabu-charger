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
  [switch]$SkipBuild
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
  @{ Name = 'SMB-детекция блока'; Crate = 'crates\kmdf';        Package = 'kmdf_package';        Out = 'driver-arm64' },
  @{ Name = 'Charge pump LN8000'; Crate = 'crates\ln8000-kmdf'; Package = 'ln8000_kmdf_package'; Out = 'driver-ln8000-arm64' }
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

Write-Host '=== манифест ===' -ForegroundColor Cyan
$manifest = [ordered]@{
  built_at   = (Get-Date).ToString('o')
  host       = "$env:COMPUTERNAME ($env:PROCESSOR_ARCHITECTURE)"
  versions   = Get-ToolVersions
  artifacts  = @()
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
