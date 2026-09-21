#Requires -Version 5.1
<#
    remote-bringup.ps1 - all the work on the tablet is done from this computer.

    Run (here, on the development computer):
        .\remote-bringup.ps1 -Computer 192.168.1.50 -User nabuagent -Password 'Nb!...'

    What it does, step by step:
      1) checks connectivity to the tablet and opens a remote management session;
      2) copies the driver package to the tablet;
      3) checks the signing mode and, if it was off, turns it on and asks
         you to reboot the tablet (after the reboot run the script again);
      4) installs the driver and starts the report collection (bring-up.ps1) right on the tablet;
      5) brings back the report, the archive, the acceptance protocol and the Windows
         power report into the folder G:\nabu-fast-charge\12-reports;
      6) with the -RemoveAccess switch removes the temporary access on the tablet.

    Caveat: WinRM remote management over the local network encrypts traffic
    weakly. It is fine for a trusted home network; do not do this on an open network.
#>
[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)][string]$Computer,
    [Parameter(Mandatory = $true)][string]$User,
    [Parameter(Mandatory = $true)][string]$Password,
    [string]$KitDir = (Join-Path $PSScriptRoot '..\artifacts\driver-ln8000-arm64'),
    [string]$RemoteDir = 'C:\nabu-ln8000',
    [string]$InboxDir = 'G:\nabu-fast-charge\12-reports',
    [int]$ChargeSampleSeconds = 120,
    [switch]$RemoveAccess,
    [switch]$SkipSignatureCheck
)

$ErrorActionPreference = 'Stop'

$kit = Resolve-Path -LiteralPath $KitDir
New-Item -ItemType Directory -Path $InboxDir -Force | Out-Null

$securePassword = ConvertTo-SecureString $Password -AsPlainText -Force
$credential = New-Object System.Management.Automation.PSCredential($User, $securePassword)

Write-Host ('=== tablet ' + $Computer + ' ===') -ForegroundColor Cyan

Write-Host '1. checking connectivity' -ForegroundColor Cyan
try {
    $wsman = Test-WSMan -ComputerName $Computer -Credential $credential -ErrorAction Stop
    Write-Host ('  connection OK: ' + $wsman.ProductVendor + ' ' + $wsman.ProductVersion)
} catch {
    throw ('tablet unreachable: ' + $_.Exception.Message +
           '. Check the address, that enable-remote.ps1 has been run on the tablet and that you are on the same network')
}

$options = New-PSSessionOption -OperationTimeout 300000 -IdleTimeout 600000
$session = New-PSSession -ComputerName $Computer -Credential $credential -SessionOption $options
Write-Host ('  session opened: ' + $session.Id)

try {
    Write-Host '2. copying the package' -ForegroundColor Cyan
    Invoke-Command -Session $session -ScriptBlock {
        param($dir)
        New-Item -ItemType Directory -Path $dir -Force | Out-Null
    } -ArgumentList $RemoteDir | Out-Null
    Copy-Item -Path (Join-Path $kit '*') -Destination $RemoteDir -ToSession $session -Force -Recurse
    Write-Host ('  files sent: ' + (Get-ChildItem $kit -File).Count)

    Write-Host '3. checking the driver signing mode' -ForegroundColor Cyan
    $signing = Invoke-Command -Session $session -ScriptBlock {
        $out = & bcdedit /enum '{current}' 2>&1 | Out-String
        [pscustomobject]@{ Enabled = ($out -match 'testsigning\s+Yes') }
    }
    if (-not $signing.Enabled -and -not $SkipSignatureCheck) {
        Write-Host '  mode is off - enabling it and rebooting the tablet' -ForegroundColor Yellow
        Invoke-Command -Session $session -ScriptBlock {
            & bcdedit /set testsigning on | Out-Null
            Restart-Computer -Force
        } | Out-Null
        Write-Host ''
        Write-Host '  The tablet is going for a reboot. Once it is up,' -ForegroundColor Yellow
        Write-Host '  run this same script again (it will see that the mode is already on).' -ForegroundColor Yellow
        return
    }
    Write-Host '  signing mode is on'

    Write-Host '4. starting the report collection on the tablet' -ForegroundColor Cyan
    $result = Invoke-Command -Session $session -ScriptBlock {
        param($dir, $seconds)
        Set-Location $dir
        $log = & (Join-Path $dir 'bring-up.ps1') -ChargeSampleSeconds $seconds 2>&1
        $reportDir = Join-Path $env:ProgramData 'nabu-fastcharge\report'
        [pscustomobject]@{
            Output = ($log | Out-String)
            Files  = (Get-ChildItem $reportDir -File -ErrorAction SilentlyContinue |
                        Sort-Object LastWriteTime -Descending | Select-Object -First 8 |
                        Select-Object Name, Length, FullName)
        }
    } -ArgumentList $RemoteDir, $ChargeSampleSeconds

    Write-Host '5. fetching the report' -ForegroundColor Cyan
    $copied = 0
    foreach ($file in $result.Files) {
        Copy-Item -LiteralPath $file.FullName -Destination $InboxDir -FromSession $session -Force
        $copied++
        Write-Host ('  received: ' + $file.Name)
    }

    Write-Host ''
    Write-Host ('=== key report lines (total files: ' + $copied + ') ===') -ForegroundColor Cyan
    $result.Output -split "`r?`n" |
        Where-Object { $_ -match 'output\s*:|difference|node|Problem|mode|charge|ERROR|failed' } |
        Select-Object -First 20 | ForEach-Object { Write-Host ('  ' + $_.Trim()) }

    Write-Host ''
    Write-Host ('Reports are in: ' + $InboxDir) -ForegroundColor Green
} finally {
    if ($RemoveAccess) {
        Write-Host '6. removing the temporary access on the tablet' -ForegroundColor Cyan
        try {
            Invoke-Command -Session $session -ScriptBlock {
                param($name)
                Remove-LocalUser -Name $name -ErrorAction SilentlyContinue
                Remove-NetFirewallRule -DisplayName 'nabu remote 5985' -ErrorAction SilentlyContinue
                Disable-PSRemoting -Force -ErrorAction SilentlyContinue
                'access removed'
            } -ArgumentList $User | ForEach-Object { Write-Host ('  ' + $_) }
        } catch {
            Write-Host ('  could not remove automatically: ' + $_.Exception.Message) -ForegroundColor Yellow
            Write-Host '  run this on the tablet: Remove-LocalUser -Name ' -NoNewline
            Write-Host $User
        }
    }
    Remove-PSSession -Session $session -ErrorAction SilentlyContinue
    Write-Host 'session closed'
}
