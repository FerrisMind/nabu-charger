<#
.SYNOPSIS
  Makes sure a test-signing certificate exists where cargo-wdk looks for it.

.DESCRIPTION
  cargo-wdk test-signs every driver package with the certificate it finds in the *user*
  certificate store `WDRTestCertStore`, subject `WDRLocalTestCert`, and hands that store name
  to signtool. Neither a GitHub runner image nor a fresh machine is guaranteed to carry one, and
  a build that finds none stops inside the package step with `SignTool Error: File not found` -
  which is what the first release dry run did.

  A certificate is created in `Cert:\CurrentUser\My` and moved into the store through
  `certutil -importPFX`, which creates the store on the way in: both
  `New-SelfSignedCertificate -CertStoreLocation` and `Import-Certificate` refuse a store that
  does not exist yet.

.PARAMETER StoreName
  The user store cargo-wdk reads. Default WDRTestCertStore.

.PARAMETER Subject
  The subject cargo-wdk asks signtool for. Default CN=WDRLocalTestCert.

.PARAMETER Trust
  Also import the certificate into the user `Root` and `TrustedPublisher` stores, so that
  Authenticode verification returns Valid instead of UnknownError. The archive ships the .cer
  and INSTALL.md has the reader import it into the machine stores; this is that same step, for
  the machine that builds.

.PARAMETER ClearCachedCertFiles
  Remove `WDRLocalTestCert.cer` left in the build output by an earlier build. cargo-wdk reuses
  such a file without consulting the store, and the target directories are restored from the CI
  cache, so a stale file can be packaged beside a signature made with another key: the archive
  would carry a certificate that does not match its own .sys, and the install would fail with
  CERT_E_UNTRUSTEDROOT.

.EXAMPLE
  pwsh -NoProfile -File .\deploy\prepare-test-cert.ps1 -Trust -ClearCachedCertFiles
#>
[CmdletBinding()]
param(
  [string]$StoreName = 'WDRTestCertStore',
  [string]$Subject = 'CN=WDRLocalTestCert',
  [switch]$Trust,
  [switch]$ClearCachedCertFiles
)

$ErrorActionPreference = 'Stop'
# certutil writes progress to stderr; with $PSNativeCommandUseErrorActionPreference set (the
# default in newer PowerShell versions) that would throw before the exit code is read.
if (Get-Variable PSNativeCommandUseErrorActionPreference -ErrorAction SilentlyContinue) {
  $PSNativeCommandUseErrorActionPreference = $false
}

$storePath = "Cert:\CurrentUser\$StoreName"
$cert = $null
if (Test-Path $storePath) {
  $cert = Get-ChildItem $storePath |
    Where-Object { $_.Subject -eq $Subject -and $_.HasPrivateKey } |
    Select-Object -First 1
}

if ($cert) {
  Write-Host "test certificate already in ${storePath}: $($cert.Thumbprint)"
} else {
  Write-Host "no $Subject certificate in ${storePath}: creating one"
  $made = New-SelfSignedCertificate -Subject $Subject -CertStoreLocation 'Cert:\CurrentUser\My' `
    -Type CodeSigningCert -KeyUsage DigitalSignature -KeyExportPolicy Exportable `
    -NotAfter (Get-Date).AddYears(5)
  $pfx = Join-Path ([IO.Path]::GetTempPath()) 'WDRLocalTestCert.pfx'
  $password = [guid]::NewGuid().ToString('n')
  Export-PfxCertificate -Cert $made -FilePath $pfx `
    -Password (ConvertTo-SecureString $password -AsPlainText -Force) | Out-Null
  & certutil -user -f -p $password -importPFX $StoreName $pfx
  if ($LASTEXITCODE -ne 0) { throw "certutil could not put the certificate into ${StoreName} (exit code $LASTEXITCODE)" }
  Remove-Item -LiteralPath $pfx -Force
  Remove-Item -LiteralPath (Join-Path 'Cert:\CurrentUser\My' $made.Thumbprint) -Force
  $cert = Get-ChildItem $storePath |
    Where-Object { $_.Subject -eq $Subject -and $_.HasPrivateKey } |
    Select-Object -First 1
  if (-not $cert) { throw "the certificate did not land in ${storePath}" }
}

if ($Trust) {
  $cer = Join-Path ([IO.Path]::GetTempPath()) 'WDRLocalTestCert.cer'
  Export-Certificate -Cert $cert -FilePath $cer -Force | Out-Null
  foreach ($name in 'Root', 'TrustedPublisher') {
    $have = Get-ChildItem "Cert:\CurrentUser\$name" | Where-Object { $_.Thumbprint -eq $cert.Thumbprint }
    if ($have) { Write-Host "already trusted in $name" }
    else {
      Import-Certificate -FilePath $cer -CertStoreLocation "Cert:\CurrentUser\$name" | Out-Null
      Write-Host "imported into $name"
    }
  }
}

if ($ClearCachedCertFiles) {
  $repoRoot = Split-Path -Parent $PSScriptRoot
  foreach ($pattern in @(
      'crates\*\target\aarch64-pc-windows-msvc\release\WDRLocalTestCert.cer',
      'crates\*\target\aarch64-pc-windows-msvc\release\*_package\WDRLocalTestCert.cer',
      'crates\*\target\aarch64-pc-windows-msvc\debug\WDRLocalTestCert.cer',
      'crates\*\target\aarch64-pc-windows-msvc\debug\*_package\WDRLocalTestCert.cer',
      'crates\*\target\release\WDRLocalTestCert.cer',
      'crates\*\target\release\*_package\WDRLocalTestCert.cer')) {
    Get-ChildItem -Path (Join-Path $repoRoot $pattern) -ErrorAction SilentlyContinue |
      ForEach-Object { Write-Host "removing the cached $($_.FullName)"; Remove-Item -LiteralPath $_.FullName -Force }
  }
}

Write-Host "certificate: $($cert.Subject), thumbprint $($cert.Thumbprint), expires $($cert.NotAfter.ToString('yyyy-MM-dd'))"
