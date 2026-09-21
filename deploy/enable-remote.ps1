#Requires -Version 5.1
<#
    enable-remote.ps1 - ONE command on the tablet, after which the agent does the work.

    Run on the tablet (PowerShell as administrator):
        .\enable-remote.ps1

    What it does:
      1) enables the WinRM remote management service and starts it;
      2) opens a firewall port for it (local network only);
      3) creates a separate local administrator nabuagent with a random
         password - it is needed only for this job and is removed with one command;
      4) prints the three lines that must be handed to the agent.

    WHAT TO KNOW BEFORE RUNNING IT
      * By default WinRM encrypts the transfer weakly: that is acceptable on a trusted
        home network and NOT acceptable on an open one (cafe, hotel).
      * A real tablet administrator is created. It can and should be removed
        after the work: the script prints a ready-to-use removal command.
      * Nothing is reflashed, partitions and the bootloader are not touched.

    If this does not suit you, there is a path without remote access: run-acceptance.ps1
    and bring-up.ps1 collect a report that you send as a file or as text.
#>
[CmdletBinding()]
param(
    [string]$UserName = 'nabuagent',
    [int]$Port = 5985,
    [switch]$SkipAccount
)

$ErrorActionPreference = 'Stop'

$identity = [Security.Principal.WindowsIdentity]::GetCurrent()
$principal = New-Object Security.Principal.WindowsPrincipal($identity)
if (-not $principal.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)) {
    throw 'administrator rights required: restart PowerShell as administrator'
}

Write-Host '=== enabling remote management ===' -ForegroundColor Cyan

# 1. The WinRM service. Enable it carefully: first look at the status.
$service = Get-Service WinRM -ErrorAction SilentlyContinue
if (-not $service) {
    throw 'WinRM service not found - remote management is unavailable on this Windows build'
}
if ($service.Status -ne 'Running') {
    Set-Service WinRM -StartupType Automatic
    Start-Service WinRM
}
Write-Host ("  WinRM: " + (Get-Service WinRM).Status)

# 2. The request listener and the firewall rule.
try {
    $null = Get-ChildItem WSMan:\localhost\Listener -ErrorAction SilentlyContinue
    Enable-PSRemoting -Force -SkipNetworkProfileCheck | Out-Null
    Write-Host '  PSRemoting enabled'
} catch {
    Write-Host ('  PSRemoting: ' + $_.Exception.Message) -ForegroundColor Yellow
}

$ruleName = "nabu remote $Port"
if (-not (Get-NetFirewallRule -DisplayName $ruleName -ErrorAction SilentlyContinue)) {
    New-NetFirewallRule -DisplayName $ruleName -Direction Inbound -LocalPort $Port `
        -Protocol TCP -Action Allow -Profile Private, Domain | Out-Null
    Write-Host ("  firewall rule added for port $Port (private networks only)")
} else {
    Write-Host '  firewall rule already exists'
}

# 3. A separate administrator for the job. The password is random and lives until removal.
$password = $null
if (-not $SkipAccount) {
    $alphabet = 'abcdefghijkmnopqrstuvwxyzABCDEFGHJKLMNPQRSTUVWXYZ23456789'.ToCharArray()
    $raw = -join (1..20 | ForEach-Object { $alphabet | Get-Random })
    $password = "Nb!$raw"
    $secure = ConvertTo-SecureString $password -AsPlainText -Force

    $existing = Get-LocalUser -Name $UserName -ErrorAction SilentlyContinue
    if ($existing) {
        Set-LocalUser -Name $UserName -Password $secure
        Write-Host ("  user " + $UserName + ' already existed - password updated')
    } else {
        New-LocalUser -Name $UserName -Password $secure -Description 'Temporary access for charging debugging' `
            -PasswordNeverExpires | Out-Null
        Write-Host ("  user " + $UserName + ' created')
    }
    # The built-in administrators group is named in the OS display language, so it is
    # addressed by its well-known SID: on a localized Windows the name does not resolve.
    $admins = Get-LocalGroupMember -SID 'S-1-5-32-544' -ErrorAction SilentlyContinue
    if (-not ($admins | Where-Object { $_.Name -like "*$UserName" })) {
        Add-LocalGroupMember -SID 'S-1-5-32-544' -Member $UserName
        Write-Host '  added to the administrators group'
    }
}

# 4. What to hand to the agent.
$addresses = Get-NetIPConfiguration -ErrorAction SilentlyContinue |
    Where-Object { $_.IPv4DefaultGateway -and $_.NetAdapter.Status -eq 'Up' }
$address = 'not determined'
foreach ($item in $addresses) {
    if ($item.InterfaceAlias -match 'Virtual|VMware|Loopback|vEthernet') { continue }
    $candidate = $item.IPv4Address.IPAddress | Select-Object -First 1
    if ($candidate) { $address = $candidate; break }
}

Write-Host ''
Write-Host '==========================================================================' -ForegroundColor Green
Write-Host ' HAND THESE THREE LINES TO THE AGENT:' -ForegroundColor Green
Write-Host '==========================================================================' -ForegroundColor Green
Write-Host ("  tablet address : " + $address)
Write-Host ("  user           : " + $UserName)
if ($password) {
    Write-Host ("  password       : " + $password)
} else {
    Write-Host '  password       : not created (-SkipAccount)'
}
Write-Host ''
Write-Host ' REMOVE THE TEMPORARY ACCESS AFTER THE WORK (on the tablet, as administrator):' -ForegroundColor Yellow
Write-Host ("   Remove-LocalUser -Name $UserName")
Write-Host ("   Remove-NetFirewallRule -DisplayName 'nabu remote $Port'")
Write-Host '   Disable-PSRemoting -Force'
Write-Host ''
Write-Host ' To check that the tablet is reachable from the agent computer:' -ForegroundColor Yellow
Write-Host ("   Test-WSMan -ComputerName " + $address + ' -Credential (Get-Credential)')
Write-Host ''
