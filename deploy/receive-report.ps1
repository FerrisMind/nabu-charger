#Requires -Version 5.1
<#
    receive-report.ps1 — принимает отчёт с планшета по локальной сети.

    Зачем: агент не может сам зайти в сессию планшета, поэтому отчёт должен
    приехать к нему на диск. Этот скрипт поднимает маленький приёмник и ждёт
    один файл (или текст), который планшет отправит обычным HTTP-запросом.
    Ничего не устанавливается и не открывается наружу: слушаем только свою сеть.

    Запуск на ЭТОМ компьютере (там, где лежит проект):
        .\receive-report.ps1
        .\receive-report.ps1 -OpenFirewall     # от администратора: сразу открыть порт

    Скрипт напечатает готовую команду для планшета — её нужно скопировать туда.
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
    # Берём адрес адаптера, у которого есть шлюз: это настоящая локальная сеть,
    # а не виртуальные интерфейсы вроде VirtualBox или Hyper-V.
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
        Write-Host "  правило брандмауэра добавлено для порта $Port" -ForegroundColor Green
    } catch {
        Write-Host ("  правило не добавлено: " + $_.Exception.Message) -ForegroundColor Yellow
    }
}

$prefixes = @("http://+:$Port/")
if (-not $isAdmin) {
    Write-Host '  без прав администратора слушаем только localhost' -ForegroundColor Yellow
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
        Write-Host ("  не удалось слушать " + $prefix + ": " + $_.Exception.Message) -ForegroundColor Yellow
    }
}
if (-not $listener) {
    throw 'приёмник не поднялся: попробуйте запустить от администратора или смените порт'
}

$address = Get-LanAddress
$url = "http://${address}:$Port/upload"

Write-Host ''
Write-Host '=== приёмник отчётов поднят ===' -ForegroundColor Cyan
Write-Host ("  сохраняю в : " + $OutDir)
Write-Host ("  проверка из браузера: http://${address}:$Port/")
Write-Host ''
Write-Host '  Скопируйте это на планшет и выполните там в PowerShell:' -ForegroundColor Green
Write-Host ''
Write-Host ("    `$report = Get-ChildItem `"`$env:ProgramData\nabu-fastcharge\report`" -Filter 'nabu-report-*.zip' | Sort-Object LastWriteTime -Descending | Select-Object -First 1")
Write-Host ("    Invoke-WebRequest -Uri '$url' -Method Post -InFile `$report.FullName -ContentType 'application/octet-stream'")
Write-Host ''
Write-Host '  Если архива нет, отправьте текстовый отчёт:' -ForegroundColor Green
Write-Host ("    `$txt = Get-ChildItem `"`$env:ProgramData\nabu-fastcharge\report`" -Filter 'nabu-report-*.txt' | Sort-Object LastWriteTime -Descending | Select-Object -First 1")
Write-Host ("    Invoke-WebRequest -Uri '$url' -Method Post -InFile `$txt.FullName -ContentType 'text/plain'")
Write-Host ''
Write-Host '  Или просто вставьте текст отчёта в это окно и нажмите Ctrl+Z, Enter.' -ForegroundColor Green
Write-Host ("  Жду до " + (Get-Date).AddSeconds($TimeoutSeconds).ToString('HH:mm:ss') + ' ...')
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
        $answer = [Text.Encoding]::UTF8.GetBytes("принято: $name ($size байт)")
        $response.StatusCode = 200
        $response.ContentType = 'text/plain; charset=utf-8'
        $response.ContentLength64 = $answer.Length
        $response.KeepAlive = $false
        $response.OutputStream.Write($answer, 0, $answer.Length)
        $response.OutputStream.Flush()
        $response.OutputStream.Close()
        $response.Close()
        # Даём ответу уйти до остановки приёмника, иначе клиент увидит
        # оборванное соединение вместо ответа.
        Start-Sleep -Milliseconds 800

        Write-Host ("  ПРИНЯТО: " + $target + '  (' + $size + ' байт)') -ForegroundColor Green
        $saved = $target
    } else {
        $page = @"
Приёмник отчётов nabu работает.
Отправьте файл так (на планшете, в PowerShell):

  Invoke-WebRequest -Uri '$url' -Method Post -InFile '<путь к отчёту>' -ContentType 'application/octet-stream'

Сохранение идёт в: $OutDir
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
        Write-Host '  запрос проверки из браузера — ответ отправлен'
    }
}

$listener.Stop()
$listener.Close()

if ($saved) {
    Write-Host ''
    Write-Host ("Отчёт получен: " + $saved) -ForegroundColor Green
} else {
    Write-Host ''
    Write-Host 'Время ожидания истекло, файл не получен.' -ForegroundColor Yellow
}
