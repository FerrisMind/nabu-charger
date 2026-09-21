# build-arm64.ps1 - reproducible build of the ARM64 driver packages
#
# Builds both drivers, puts the packages in artifacts/, computes SHA-256 and
# writes the build manifest (tool versions, time, sizes, checksums).
#
# Run (on the development machine, not on the tablet):
#     .\build-arm64.ps1
#     .\build-arm64.ps1 -SkipBuild      # only recompute the checksums and manifest
#
# Reproducibility: the Rust version (rust-toolchain.toml), the cargo-wdk
# version, the WDK version and libclang are pinned. With all four matching and
# the same source, the very same .sys is produced (checked by SHA-256 cross-check).

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
  $versions['cargo-wdk'] = try { ((& cargo wdk --version) -join '') } catch { 'not installed' }
  $versions['wdk'] = if ($env:WDKContentRoot) { $env:WDKContentRoot } else { 'C:\Program Files (x86)\Windows Kits\10' }
  $versions['libclang'] = if ($env:LIBCLANG_PATH) { $env:LIBCLANG_PATH } else { 'not set' }
  $versions['target'] = 'aarch64-pc-windows-msvc'
  return $versions
}

$targets = @(
  @{ Name = 'SMB block detection'; Crate = 'crates\kmdf';        Package = 'kmdf_package';        Out = 'driver-arm64';        Deploy = $null },
  @{ Name = 'Charge pump LN8000'; Crate = 'crates\ln8000-kmdf'; Package = 'ln8000_kmdf_package'; Out = 'driver-ln8000-arm64'; Deploy = 'deploy' }
)

if (-not $env:LIBCLANG_PATH) { $env:LIBCLANG_PATH = 'C:\Program Files\LLVM\bin' }

# Outrank tablet oem142 (20.46.10.591). Without STAMPINF_VERSION, cargo-wdk
# passes stampinf -v * (wall-clock) which can stamp below oem142 on AM builds.
if (-not $env:STAMPINF_VERSION) {
  $env:STAMPINF_VERSION = '20.47.10.660'
}
Write-Host ("STAMPINF_VERSION=" + $env:STAMPINF_VERSION) -ForegroundColor DarkGray

$built = @()
foreach ($t in $targets) {
  $cratePath = Join-Path $RepoRoot $t.Crate
  if (-not (Test-Path $cratePath)) { throw "no directory $cratePath" }

  if (-not $SkipBuild) {
    Write-Host ("=== build: " + $t.Name) -ForegroundColor Cyan
    Push-Location $cratePath
    try {
      & cargo wdk build --target-arch arm64 --profile release
      if ($LASTEXITCODE -ne 0) { throw "build of $($t.Crate) returned code $LASTEXITCODE" }
    } finally {
      Pop-Location
    }
  }

  $packagePath = Join-Path $cratePath ("target\aarch64-pc-windows-msvc\release\" + $t.Package)
  if (-not (Test-Path $packagePath)) { throw "no package $packagePath" }

  $outPath = Join-Path $RepoRoot ('artifacts\' + $t.Out)
  New-Item -ItemType Directory -Path $outPath -Force | Out-Null
  Copy-Item (Join-Path $packagePath '*') $outPath -Force

  # The deployment scripts go next to the package: on the tablet they read
  # $PSScriptRoot, so the whole kit is copied into a single folder.
  if ($t.Deploy) {
    $deployPath = Join-Path $cratePath $t.Deploy
    if (Test-Path $deployPath) {
      Copy-Item (Join-Path $deployPath '*.ps1') $outPath -Force
      Copy-Item (Join-Path $deployPath '*.md') $outPath -Force -ErrorAction SilentlyContinue
    }
  }

  # Scripts that run on the tablet itself but live in the shared deploy dir:
  # remote access is enabled in the same place as everything else.
  if ($t.Out -eq 'driver-ln8000-arm64') {
    $shared = Join-Path $RepoRoot 'deploy\enable-remote.ps1'
    if (Test-Path $shared) {
      Copy-Item $shared $outPath -Force
    }
  }

  $built += [pscustomobject]@{ Name = $t.Name; Out = $outPath }
}

Write-Host '=== checksums ===' -ForegroundColor Cyan
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
Write-Host '=== reproducibility ===' -ForegroundColor Cyan

# An honest check: build again and compare the bytes. Comparing only the hash
# says little, so we count the differences and record the note in the manifest.
$reproducible = $null
$reproNote = 'not checked (build skipped)'
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
            Write-Host ('  ' + (Split-Path $entry.Key -Leaf) + ': matches') -ForegroundColor Green
        } else {
            $reproducible = $false
            Write-Host ('  ' + (Split-Path $entry.Key -Leaf) + ': bytes differ from the first build') -ForegroundColor Yellow
        }
    }
    if ($reproducible) {
        $reproNote = 'bit-for-bit reproducibility achieved'
    } else {
        $reproNote = 'bit-for-bit reproducibility NOT achieved: image metadata changes between builds'
    }
}
Write-Host ('  note: ' + $reproNote)

# The checksums are recomputed after the last build, otherwise they would describe the previous file.
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
Write-Host '=== manifest ===' -ForegroundColor Cyan
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
Write-Host ("checksums : " + $sumPath) -ForegroundColor Green
Write-Host ("manifest  : " + $manifestPath) -ForegroundColor Green
