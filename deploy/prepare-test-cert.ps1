<#
.SYNOPSIS
  Makes sure a test-signing certificate exists where cargo-wdk looks for it.

.DESCRIPTION
  cargo-wdk test-signs every driver package with the certificate it finds in the *user*
  certificate store `WDRTestCertStore`, subject `WDRLocalTestCert`, and hands that store name
  to signtool. Neither a GitHub runner image nor a fresh machine is guaranteed to carry one, and
  a build that finds none stops inside the package step with `SignTool Error: File not found` -
  which is what the first release dry run did.

  A certificate created by `New-SelfSignedCertificate` lives in `Cert:\CurrentUser\My`; it is
  put into the cargo-wdk store with the .NET store API, which creates a store that does not
  exist yet. `X509Store.Add` writes the certificate's key reference with it, so signtool can
  sign from the store the way it signs from `My`. This is the same thing `makecert -ss` does,
  without an external process: a certutil command that decides to ask a question would wait for
  an answer that never comes, which is how the first attempt at this step hung a runner.

.PARAMETER StoreName
  The user store cargo-wdk reads. Default WDRTestCertStore.

.PARAMETER Subject
  The subject cargo-wdk asks signtool for. Default CN=WDRLocalTestCert.

.PARAMETER ClearCachedCertFiles
  Remove `WDRLocalTestCert.cer` left in the build output by an earlier build. cargo-wdk reuses
  such a file without consulting the store, and the target directories are restored from the CI
  cache, so a stale file can be packaged beside a signature made with another key: the archive
  would carry a certificate that does not match its own .sys, and the install would fail with
  CERT_E_UNTRUSTEDROOT.

  Nothing here touches a trust store. Adding a certificate to `Root` raises a security-consent
  dialog, and a runner has nobody to answer it: the step that tried waited until its timeout.
  A test-signed driver is not trusted on the machine that builds it - `Authenticode` reads
  `UnknownError` there, on the runner and on a development machine alike - and the certificate
  becomes trusted where the driver is installed, through the .cer in the package.

.EXAMPLE
  pwsh -NoProfile -File .\deploy\prepare-test-cert.ps1 -ClearCachedCertFiles
#>
[CmdletBinding()]
param(
  [string]$StoreName = 'WDRTestCertStore',
  [string]$Subject = 'CN=WDRLocalTestCert',
  [switch]$ClearCachedCertFiles
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

function Add-ToStore([string]$Name, $Certificate) {
  $store = New-Object System.Security.Cryptography.X509Certificates.X509Store($Name, 'CurrentUser')
  # ReadWrite creates the store if it is not there yet; a certificate kept in a store that
  # already holds the same one is not added twice, so this is safe to repeat.
  $store.Open('ReadWrite')
  try { $store.Add($Certificate) } finally { $store.Close() }
}

Write-Host "store under test: Cert:\CurrentUser\$StoreName"
$cert = Get-StoreCertificate $StoreName $Subject
if ($cert) {
  Write-Host "certificate already there: $($cert.Thumbprint)"
} else {
  Write-Host 'no certificate in that store: creating one in Cert:\CurrentUser\My'
  $made = New-SelfSignedCertificate -Subject $Subject -CertStoreLocation 'Cert:\CurrentUser\My' `
    -Type CodeSigningCert -KeyUsage DigitalSignature -KeyExportPolicy Exportable `
    -NotAfter (Get-Date).AddYears(5)
  Write-Host "created $($made.Thumbprint); moving it into ${StoreName}"
  Add-ToStore $StoreName $made
  Remove-Item -LiteralPath (Join-Path 'Cert:\CurrentUser\My' $made.Thumbprint) -Force
  $cert = Get-StoreCertificate $StoreName $Subject
  if (-not $cert) { throw "the certificate did not land in Cert:\CurrentUser\$StoreName" }
  Write-Host "certificate in place: $($cert.Thumbprint), private key: $($cert.HasPrivateKey)"
}

if ($ClearCachedCertFiles) {
  $repoRoot = Split-Path -Parent $PSScriptRoot
  $removed = 0
  # Two levels of directories under `target` (the architecture directory and the profile
  # directory below it), one level below each profile for the package folder: no recursive
  # walk over a build output that a cache restore has just made very large.
  foreach ($crate in 'kmdf', 'ln8000-kmdf') {
    $targetRoot = Join-Path $repoRoot "crates\$crate\target"
    if (-not (Test-Path -LiteralPath $targetRoot)) { continue }
    $level1 = @(Get-ChildItem -LiteralPath $targetRoot -Directory -ErrorAction SilentlyContinue)
    foreach ($dir in @($targetRoot) + ($level1 | Select-Object -ExpandProperty FullName)) {
      foreach ($probe in @($dir, (Join-Path $dir 'release'), (Join-Path $dir 'debug'))) {
        if (-not (Test-Path -LiteralPath $probe -PathType Container)) { continue }
        $candidates = @((Join-Path $probe 'WDRLocalTestCert.cer'))
        foreach ($leaf in Get-ChildItem -LiteralPath $probe -Directory -ErrorAction SilentlyContinue) {
          $candidates += (Join-Path $leaf.FullName 'WDRLocalTestCert.cer')
        }
        foreach ($path in $candidates) {
          if (Test-Path -LiteralPath $path -PathType Leaf) {
            Remove-Item -LiteralPath $path -Force
            Write-Host "removed a cached $($path.Substring($repoRoot.Length + 1))"
            $removed++
          }
        }
      }
    }
  }
  Write-Host "cached certificate files removed: $removed"
}

Write-Host "certificate: $($cert.Subject), thumbprint $($cert.Thumbprint), expires $($cert.NotAfter.ToString('yyyy-MM-dd'))"
