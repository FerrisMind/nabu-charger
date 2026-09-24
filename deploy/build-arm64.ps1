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
# version, the WDK version and libclang are pinned, and every crate builds with
# /Brepro. Measured with a cleared ARM64 cache: the image is identical between
# two builds section by section, while the Authenticode signature, which carries
# a timestamp, and the PE checksum differ - so the SHA-256 of a signed .sys is
# not a build fingerprint. The verdict is written into the manifest.

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
  @{ Name = 'SMB block detection'; Crate = 'crates\kmdf';        Pkg = 'kmdf';        Package = 'kmdf_package';        Out = 'driver-arm64';        Deploy = $null },
  @{ Name = 'Charge pump LN8000'; Crate = 'crates\ln8000-kmdf'; Pkg = 'ln8000-kmdf'; Package = 'ln8000_kmdf_package'; Out = 'driver-ln8000-arm64'; Deploy = 'deploy' }
)

if (-not $env:LIBCLANG_PATH) { $env:LIBCLANG_PATH = 'C:\Program Files\LLVM\bin' }

# Outrank the tablet's DriverStore. Without STAMPINF_VERSION, cargo-wdk passes
# stampinf -v * (wall-clock), which can stamp below what is already installed
# and the install is then refused as "Outranked". Keep this in lockstep with the
# DriverVer line in crates/ln8000-kmdf/ln8000_kmdf.inx.
# Highest package on the tablet at the time of writing: oem167, which is
# 20.47.10.673 (oem166 is 20.47.10.672). This version (20.47.10.674) stamps above
# them; the next build after this one has to stamp above 674.
if (-not $env:STAMPINF_VERSION) {
  $env:STAMPINF_VERSION = '20.47.10.674'
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

# Windows PowerShell writes CRLF through Set-Content, and a CRLF checksum file is
# unusable to `sha256sum -c` on Linux and macOS: the CR ends up inside the file name and
# every line fails to open. The manifest is written with LF endings so the same file
# verifies on any platform.
function Write-LinesLf([string]$Path, [string[]]$Lines) {
  [IO.File]::WriteAllText($Path, (($Lines -join "`n") + "`n"), [Text.Encoding]::ASCII)
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
Write-LinesLf $sumPath $sumLines

Write-Host ''
Write-Host '=== reproducibility ===' -ForegroundColor Cyan

# Compares two signed images and returns the number of differing bytes that are
# not part of the Authenticode certificate table or the PE checksum field, which
# is the only difference a timestamped signature can produce. Returns -1 when the
# two images have different sizes.
function Compare-PeImage([byte[]]$A, [byte[]]$B) {
    if ($A.Length -ne $B.Length) { return -1 }
    $pe = [BitConverter]::ToInt32($A, 0x3C)
    $magic = [BitConverter]::ToUInt16($A, $pe + 24)
    $dd = $pe + 24 + $(if ($magic -eq 0x20B) { 112 } else { 96 })
    $certOff = [BitConverter]::ToUInt32($A, $dd + 32)
    $certSize = [BitConverter]::ToUInt32($A, $dd + 36)
    $sumOff = $pe + 24 + 64
    $diff = 0
    for ($i = 0; $i -lt $A.Length; $i++) {
        if ($A[$i] -eq $B[$i]) { continue }
        if ($i -ge $certOff -and $i -lt ($certOff + $certSize)) { continue }
        if ($i -ge $sumOff -and $i -lt ($sumOff + 4)) { continue }
        $diff++
    }
    return $diff
}

# An honest check: build again and compare the bytes.
#
# The first version of this block hashed the files in `artifacts\` before the
# second build and then hashed the same files again. The second build writes to
# the crate's `target\` tree and nothing copies it back, so the comparison was a
# file against itself and it always printed success. Each `.sys` is now read out
# of the package the second build just produced.
#
# Fixing the comparison exposed a second version of the same mistake: the second
# build was still a cache hit, so nothing was recompiled and the block could only
# ever see a re-signature. The loop below cleans the driver crate first, which is
# what makes the second image a second compilation.
$reproducible = $null
$reproNote = 'not checked (build skipped)'
if (-not $SkipBuild) {
    $pkgSys = @()
    foreach ($t in $targets) {
        $pkg = Join-Path $RepoRoot ($t.Crate + '\target\aarch64-pc-windows-msvc\release\' + $t.Package)
        foreach ($f in (Get-ChildItem $pkg -File -Filter '*.sys')) { $pkgSys += $f.FullName }
    }
    $first = @{}
    foreach ($p in $pkgSys) { $first[$p] = [IO.File]::ReadAllBytes($p) }

    foreach ($t in $targets) {
        Push-Location (Join-Path $RepoRoot $t.Crate)
        try {
            # The second build has to compile. Two weaker attempts failed before
            # this one: `cargo wdk build` on an unchanged tree finishes in 0.12 s
            # and only re-signs the file that is already there, and `cargo clean -p
            # <name>` does not invalidate what the nested package project reuses -
            # the comparison below would again be an image against itself, which is
            # the defect this block exists to remove. Removing the ARM64 release
            # directory takes the whole dependency cache with it, and the build's
            # own output is then checked for a `Compiling` line for the crate, so a
            # silent cache hit fails the check rather than passing it.
            #
            # A foreign CARGO_TARGET_DIR cannot be used for this: cargo-wdk fails
            # with NoWdkConfigurationsDetected and produces nothing.
            $env:CARGO_TARGET_DIR = $null
            $archRelease = Join-Path (Join-Path $RepoRoot $t.Crate) 'target\aarch64-pc-windows-msvc\release'
            if (Test-Path $archRelease) { Remove-Item -LiteralPath $archRelease -Recurse -Force }
            # The log is written through cmd, not with PowerShell redirection: with
            # $ErrorActionPreference = 'Stop' a native command's stderr line becomes
            # a terminating NativeCommandError (measured, both with `2>&1` and with
            # `*>`), and cargo writes its progress to stderr.
            $buildLogPath = Join-Path $env:TEMP ('nabu-build-' + $t.Pkg + '.log')
            $cmdLine = 'cargo wdk build --target-arch arm64 --profile release > "' + $buildLogPath + '" 2>&1'
            & cmd /c $cmdLine
            $buildOut = Get-Content -LiteralPath $buildLogPath -Raw
            if ($buildOut -notmatch ('Compiling ' + [regex]::Escape($t.Pkg) + ' v')) {
                throw ("the second build of '" + $t.Name + "' compiled nothing, so it cannot witness reproducibility")
            }
        } finally { Pop-Location }
    }

    $reproducible = $true
    # Three outcomes, and reading a zero from `Compare-PeImage` as proof of equality
    # is how this block first printed "bit-for-bit identical between builds,
    # signature included" for two files whose signatures differ.
    #
    # That helper deliberately ignores the certificate table and the checksum, so a
    # zero means "no difference outside those fields". The full bytes are hashed as
    # well, which separates the two cases: identical images, or an identical image
    # with a different signature. A nonzero count outside those fields is a real
    # difference and fails the check.
    $certOnly = $false
    foreach ($p in $pkgSys) {
        $leaf = Split-Path $p -Leaf
        $now = [IO.File]::ReadAllBytes($p)
        $diff = Compare-PeImage $first[$p] $now
        if ($diff -lt 0) {
            $reproducible = $false
            Write-Host ('  ' + $leaf + ': the image itself differs between two builds of the same source') -ForegroundColor Red
            continue
        }
        $before = [BitConverter]::ToString([System.Security.Cryptography.SHA256]::Create().ComputeHash($first[$p]))
        $after = [BitConverter]::ToString([System.Security.Cryptography.SHA256]::Create().ComputeHash($now))
        if ($before -eq $after) {
            Write-Host ('  ' + $leaf + ': bit-for-bit identical between builds, signature included') -ForegroundColor Green
            continue
        }
        $certOnly = $true
        Write-Host ('  ' + $leaf + ': image identical; only the signature and the checksum differ (' + $diff + ' bytes outside those fields)') -ForegroundColor Green
    }
    if (-not $reproducible) {
        $reproNote = 'NOT reproducible: the image differs between two builds of the same source'
    } elseif ($certOnly) {
        $reproNote = 'the image is identical between builds (compared through Compare-PeImage); only the Authenticode signature, which carries a timestamp, and the PE checksum differ'
    } else {
        $reproNote = 'bit-for-bit identical between builds, signature included'
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
Write-LinesLf $sumPath ($finalSums | Sort-Object -Unique)
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
