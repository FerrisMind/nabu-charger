#Requires -Version 5.1
<#
    receive-report.ps1 - receives a report from the tablet over the local network.

    Why: the agent cannot enter the tablet session itself, so the report has to
    arrive on its disk. This script brings up a small receiver and waits for
    one file (or text) that the tablet sends with an ordinary HTTP request.
    Nothing is installed and nothing is exposed: we listen only on our own network.

    Run on THIS computer (the one holding the project):
        .\receive-report.ps1
        .\receive-report.ps1 -OpenFirewall     # as administrator: open the port right away

    The script prints a ready-to-use command for the tablet - copy it there.
#>
[CmdletBinding()]
param(
    [int]$Port = 8765,
    [string]$OutDir = 'G:\nabu-fast-charge\12-reports',
    [int]$TimeoutSeconds = 3600,
    [switch]$OpenFirewall
)

$ErrorActionPreference = 'Stop'
New-Item -ItemType Directory -Path $OutDir -Force | Out-Null

function Get-LanAddress {
    # Take the address of the adapter that has a gateway: that is the real local
    # network, not virtual interfaces such as VirtualBox or Hyper-V.
    $config = Get-NetIPConfiguration -ErrorAction SilentlyContinue |
        Where-Object { $_.IPv4DefaultGateway -and $_.NetAdapter.Status -eq 'Up' }
    foreach ($item in $config) {
        $name = $item.InterfaceAlias
        if ($name -match 'Virtual|VMware|Loopback|vEthernet|Bluetooth') { continue }
        $address = $item.IPv4Address.IPAddress | Select-Object -First 1
        if ($address) { return $address }
    }
    $fallback = Get-NetIPAddress -AddressFamily IPv4 -ErrorAction SilentlyContinue |
        Where-Object { $_.IPAddress -notlike '127.*' -and $_.IPAddress -notlike '169.254.*' } |
        Select-Object -First 1
    if ($fallback) { return $fallback.IPAddress }
    return '127.0.0.1'
}

$isAdmin = ([Security.Principal.WindowsIdentity]::GetCurrent() |
    ForEach-Object { (New-Object Security.Principal.WindowsPrincipal($_)).IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator) })

if ($OpenFirewall) {
    try {
        New-NetFirewallRule -DisplayName "nabu report $Port" -Direction Inbound -LocalPort $Port `
            -Protocol TCP -Action Allow -ErrorAction Stop | Out-Null
        Write-Host "  firewall rule added for port $Port" -ForegroundColor Green
    } catch {
        Write-Host ("  rule not added: " + $_.Exception.Message) -ForegroundColor Yellow
    }
}

$prefixes = @("http://+:$Port/")
if (-not $isAdmin) {
    Write-Host '  without administrator rights we listen on localhost only' -ForegroundColor Yellow
    $prefixes = @("http://localhost:$Port/")
}

$listener = $null
foreach ($prefix in $prefixes) {
    try {
        $candidate = New-Object System.Net.HttpListener
        $candidate.Prefixes.Add($prefix)
        $candidate.Start()
        $listener = $candidate
        break
    } catch {
        Write-Host ("  could not listen on " + $prefix + ": " + $_.Exception.Message) -ForegroundColor Yellow
    }
}
if (-not $listener) {
    throw 'the receiver did not start: try running as administrator or change the port'
}

$address = Get-LanAddress
$url = "http://${address}:$Port/upload"

Write-Host ''
Write-Host '=== report receiver is up ===' -ForegroundColor Cyan
Write-Host ("  saving to   : " + $OutDir)
Write-Host ("  browser check: http://${address}:$Port/")
Write-Host ''
Write-Host '  Copy this to the tablet and run it there in PowerShell:' -ForegroundColor Green
Write-Host ''
Write-Host ("    `$report = Get-ChildItem `"`$env:ProgramData\nabu-fastcharge\report`" -Filter 'nabu-report-*.zip' | Sort-Object LastWriteTime -Descending | Select-Object -First 1")
Write-Host ("    Invoke-WebRequest -Uri '$url' -Method Post -InFile `$report.FullName -ContentType 'application/octet-stream'")
Write-Host ''
Write-Host '  If there is no archive, send the text report:' -ForegroundColor Green
Write-Host ("    `$txt = Get-ChildItem `"`$env:ProgramData\nabu-fastcharge\report`" -Filter 'nabu-report-*.txt' | Sort-Object LastWriteTime -Descending | Select-Object -First 1")
Write-Host ("    Invoke-WebRequest -Uri '$url' -Method Post -InFile `$txt.FullName -ContentType 'text/plain'")
Write-Host ''
Write-Host '  Or just paste the report text into this window and press Ctrl+Z, Enter.' -ForegroundColor Green
Write-Host ("  Waiting until " + (Get-Date).AddSeconds($TimeoutSeconds).ToString('HH:mm:ss') + ' ...')
Write-Host ''

$deadline = (Get-Date).AddSeconds($TimeoutSeconds)
$saved = $null
while ((Get-Date) -lt $deadline -and -not $saved) {
    $context = $listener.GetContext()
    $request = $context.Request
    $response = $context.Response

    if ($request.HttpMethod -eq 'POST') {
        $name = [IO.Path]::GetFileName($request.Headers['X-File-Name'])
        if ([string]::IsNullOrWhiteSpace($name)) {
            $extension = if ($request.ContentType -like '*zip*') { 'zip' } else { 'txt' }
            $name = "nabu-report-$(Get-Date -Format 'yyyy-MM-dd-HHmmss').$extension"
        }
        $target = Join-Path $OutDir $name

        $stream = $request.InputStream
        $file = [IO.File]::Create($target)
        try {
            $stream.CopyTo($file)
        } finally {
            $file.Close()
            $stream.Close()
        }

        $size = (Get-Item -LiteralPath $target).Length
        $answer = [Text.Encoding]::UTF8.GetBytes("accepted: $name ($size bytes)")
        $response.StatusCode = 200
        $response.ContentType = 'text/plain; charset=utf-8'
        $response.ContentLength64 = $answer.Length
        $response.KeepAlive = $false
        $response.OutputStream.Write($answer, 0, $answer.Length)
        $response.OutputStream.Flush()
        $response.OutputStream.Close()
        $response.Close()
        # Give the response time to leave before stopping the receiver, otherwise the client
        # sees a broken connection instead of the response.
        Start-Sleep -Milliseconds 800

        Write-Host ("  ACCEPTED: " + $target + '  (' + $size + ' bytes)') -ForegroundColor Green
        $saved = $target
    } else {
        $page = @"
The nabu report receiver is running.
Send the file like this (on the tablet, in PowerShell):

  Invoke-WebRequest -Uri '$url' -Method Post -InFile '<path to the report>' -ContentType 'application/octet-stream'

Saving to: $OutDir
"@
        $bytes = [Text.Encoding]::UTF8.GetBytes($page)
        $response.StatusCode = 200
        $response.ContentType = 'text/plain; charset=utf-8'
        $response.ContentLength64 = $bytes.Length
        $response.KeepAlive = $false
        $response.OutputStream.Write($bytes, 0, $bytes.Length)
        $response.OutputStream.Flush()
        $response.OutputStream.Close()
        $response.Close()
        Write-Host '  browser check request - response sent'
    }
}

$listener.Stop()
$listener.Close()

if ($saved) {
    Write-Host ''
    Write-Host ("Report received: " + $saved) -ForegroundColor Green
} else {
    Write-Host ''
    Write-Host 'The wait time expired, no file received.' -ForegroundColor Yellow
}
