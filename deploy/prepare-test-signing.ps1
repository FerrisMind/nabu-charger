<#
.SYNOPSIS
  Clears what stops cargo-wdk from signing a package, and reports where the build will get its
  test certificate.

.DESCRIPTION
  cargo-wdk test-signs every driver package with the certificate it finds in the *user*
  certificate store `WDRTestCertStore`, subject `WDRLocalTestCert`, and hands that store and
  subject to signtool. It uses a certificate the store already holds, exports its .cer into the
  package folder, and creates one with `makecert` when the store has none.

  A machine that has never built a driver has no such certificate, and two things go wrong:

  * the build stops inside the package step with `SignTool Error: File not found`. The package
    step skips certificate generation entirely when it finds a `WDRLocalTestCert.cer` in the
    output directory, and in CI that directory is restored from the build cache - so the file is
    there, and the store it would have been made for is not;
  * creating the certificate here instead is worse. `certmgr` and `signtool` read different
    stores on one GitHub runner, so cargo-wdk did not see the certificate this script had put in
    place, created a second one with the same subject, and signtool then refused the package
    with `Multiple certificates were found that meet all the given criteria`.

  So this script does not create anything: it removes the cached `.cer` files, so that
  cargo-wdk's own path runs - the certificate the store holds, or a new one from `makecert` -
  and prints what it found, which is what makes a later signing failure readable.

  The certificate is a test certificate in every case. The packages are not trusted on the
  machine that builds them, and the archive's INSTALL.md has the reader import the .cer it
  ships into the machine stores of the tablet, which is where the trust comes from.

.PARAMETER StoreName
  The user store cargo-wdk reads. Default WDRTestCertStore.

.PARAMETER Subject
  The subject cargo-wdk asks signtool for. Default CN=WDRLocalTestCert.

.EXAMPLE
  pwsh -NoProfile -File .\deploy\prepare-test-signing.ps1
#>
[CmdletBinding()]
param(
  [string]$StoreName = 'WDRTestCertStore',
  [string]$Subject = 'CN=WDRLocalTestCert'
)

$ErrorActionPreference = 'Stop'

function Get-StoreCertificate([string]$Name, [string]$WantedSubject) {
  try {
    $store = New-Object System.Security.Cryptography.X509Certificates.X509Store($Name, 'CurrentUser')
    $store.Open('ReadOnly')
    try {
      return @($store.Certificates | Where-Object { $_.Subject -eq $WantedSubject -and $_.HasPrivateKey }) | Select-Object -First 1
    } finally { $store.Close() }
  } catch {
    return $null
  }
}

Write-Host '=== the certificate the build will sign with ==='
$cert = Get-StoreCertificate $StoreName $Subject
if ($cert) {
  Write-Host "Cert:\CurrentUser\$StoreName already holds $Subject`: $($cert.Thumbprint)"
  Write-Host 'cargo-wdk will use it and export its .cer into the package folder'
} else {
  Write-Host "Cert:\CurrentUser\$StoreName holds no $Subject certificate"
  Write-Host 'cargo-wdk will create one with makecert'
}
# cargo-wdk puts the WDK's own bin directory on PATH for the tools it runs, so these are looked
# for in the tools directory rather than in this session's PATH.
$bin = Get-ChildItem 'C:\Program Files (x86)\Windows Kits\10\bin' -Directory -ErrorAction SilentlyContinue |
  Where-Object { $_.Name -match '^\d+\.\d+\.\d+\.\d+$' } |
  Sort-Object { [version]$_.Name } -Descending | Select-Object -First 1
foreach ($tool in 'makecert.exe', 'certmgr.exe', 'signtool.exe') {
  $path = if ($bin) { Join-Path $bin.FullName "x64\$tool" } else { $null }
  $found = if ($path -and (Test-Path -LiteralPath $path)) { $path } else { 'not in the newest tools version directory' }
  Write-Host ("  {0}: {1}" -f $tool, $found)
}

Write-Host '=== cached certificate files ==='
$repoRoot = Split-Path -Parent $PSScriptRoot
$removed = 0
# Two levels of directories under `target` (the architecture directory and the profile directory
# below it), one level below each profile for the package folder: no recursive walk over a build
# output that a cache restore has just made very large.
foreach ($crate in 'kmdf', 'ln8000-kmdf') {
  $targetRoot = Join-Path $repoRoot "crates\$crate\target"
  if (-not (Test-Path -LiteralPath $targetRoot)) { continue }
  $level1 = @(Get-ChildItem -LiteralPath $targetRoot -Directory -ErrorAction SilentlyContinue | Select-Object -ExpandProperty FullName)
  foreach ($dir in @($targetRoot) + $level1) {
    foreach ($probe in @($dir, (Join-Path $dir 'release'), (Join-Path $dir 'debug'))) {
      if (-not (Test-Path -LiteralPath $probe -PathType Container)) { continue }
      $candidates = @((Join-Path $probe 'WDRLocalTestCert.cer'))
      foreach ($leaf in Get-ChildItem -LiteralPath $probe -Directory -ErrorAction SilentlyContinue) {
        $candidates += (Join-Path $leaf.FullName 'WDRLocalTestCert.cer')
      }
      foreach ($candidate in $candidates) {
        if (Test-Path -LiteralPath $candidate -PathType Leaf) {
          Remove-Item -LiteralPath $candidate -Force
          Write-Host "  removed $($candidate.Substring($repoRoot.Length + 1))"
          $removed++
        }
      }
    }
  }
}
if ($removed -eq 0) { Write-Host '  none: the package step will look the certificate up in the store' }
else { Write-Host "  removed $removed file(s): the package step will export the .cer of the certificate it signs with" }
