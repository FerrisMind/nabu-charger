#Requires -Version 5.1
<#
    enable-remote.ps1 — ОДНА команда на планшете, после которой работу делает агент.

    Запуск на планшете (PowerShell от имени администратора):
        .\enable-remote.ps1

    Что делает:
      1) включает службу удалённого управления WinRM и запускает её;
      2) открывает для неё порт в брандмауэре (только локальная сеть);
      3) создаёт отдельного локального администратора nabuagent со случайным
         паролем — он нужен только для этой работы и удаляется одной командой;
      4) печатает три строки, которые надо передать агенту.

    ЧТО ВАЖНО ЗНАТЬ ПЕРЕД ЗАПУСКОМ
      * WinRM по умолчанию шифрует передачу слабо: это приемлемо в доверенной
        домашней сети и НЕ приемлемо в открытой (кафе, гостиница).
      * Создаётся настоящий администратор планшета. Его можно и нужно удалить
        после работы: скрипт печатает готовую команду удаления.
      * Ничего не перепрошивается, разделы и загрузчик не трогаются.

    Если это не подходит — есть путь без удалённого доступа: run-acceptance.ps1
    и bring-up.ps1 собирают отчёт, который вы пришлёте файлом или текстом.
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
    throw 'нужны права администратора: перезапустите PowerShell от имени администратора'
}

Write-Host '=== включаю удалённое управление ===' -ForegroundColor Cyan

# 1. Служба WinRM. Включаем аккуратно: сначала смотрим состояние.
$service = Get-Service WinRM -ErrorAction SilentlyContinue
if (-not $service) {
    throw 'служба WinRM не найдена — на этой сборке Windows удалённое управление недоступно'
}
if ($service.Status -ne 'Running') {
    Set-Service WinRM -StartupType Automatic
    Start-Service WinRM
}
Write-Host ("  WinRM: " + (Get-Service WinRM).Status)

# 2. Приёмник запросов и правило брандмауэра.
try {
    $null = Get-ChildItem WSMan:\localhost\Listener -ErrorAction SilentlyContinue
    Enable-PSRemoting -Force -SkipNetworkProfileCheck | Out-Null
    Write-Host '  PSRemoting включён'
} catch {
    Write-Host ('  PSRemoting: ' + $_.Exception.Message) -ForegroundColor Yellow
}

$ruleName = "nabu remote $Port"
if (-not (Get-NetFirewallRule -DisplayName $ruleName -ErrorAction SilentlyContinue)) {
    New-NetFirewallRule -DisplayName $ruleName -Direction Inbound -LocalPort $Port `
        -Protocol TCP -Action Allow -Profile Private, Domain | Out-Null
    Write-Host ("  правило брандмауэра добавлено для порта $Port (только частные сети)")
} else {
    Write-Host '  правило брандмауэра уже есть'
}

# 3. Отдельный администратор для работы. Пароль случайный, живёт до удаления.
$password = $null
if (-not $SkipAccount) {
    $alphabet = 'abcdefghijkmnopqrstuvwxyzABCDEFGHJKLMNPQRSTUVWXYZ23456789'.ToCharArray()
    $raw = -join (1..20 | ForEach-Object { $alphabet | Get-Random })
    $password = "Nb!$raw"
    $secure = ConvertTo-SecureString $password -AsPlainText -Force

    $existing = Get-LocalUser -Name $UserName -ErrorAction SilentlyContinue
    if ($existing) {
        Set-LocalUser -Name $UserName -Password $secure
        Write-Host ("  пользователь " + $UserName + ' уже был — пароль обновлён')
    } else {
        New-LocalUser -Name $UserName -Password $secure -Description 'Временный доступ для отладки зарядки' `
            -PasswordNeverExpires | Out-Null
        Write-Host ("  пользователь " + $UserName + ' создан')
    }
    $admins = Get-LocalGroupMember -Group 'Администраторы' -ErrorAction SilentlyContinue
    if (-not $admins) { $admins = Get-LocalGroupMember -Group 'Administrators' -ErrorAction SilentlyContinue }
    if (-not ($admins | Where-Object { $_.Name -like "*$UserName" })) {
        Add-LocalGroupMember -Group 'Administrators' -Member $UserName
        Write-Host '  добавлен в группу администраторов'
    }
}

# 4. Что передать агенту.
$addresses = Get-NetIPConfiguration -ErrorAction SilentlyContinue |
    Where-Object { $_.IPv4DefaultGateway -and $_.NetAdapter.Status -eq 'Up' }
$address = 'не определён'
foreach ($item in $addresses) {
    if ($item.InterfaceAlias -match 'Virtual|VMware|Loopback|vEthernet') { continue }
    $candidate = $item.IPv4Address.IPAddress | Select-Object -First 1
    if ($candidate) { $address = $candidate; break }
}

Write-Host ''
Write-Host '==========================================================================' -ForegroundColor Green
Write-Host ' ПЕРЕДАЙТЕ АГЕНТУ ЭТИ ТРИ СТРОКИ:' -ForegroundColor Green
Write-Host '==========================================================================' -ForegroundColor Green
Write-Host ("  адрес планшета : " + $address)
Write-Host ("  пользователь   : " + $UserName)
if ($password) {
    Write-Host ("  пароль         : " + $password)
} else {
    Write-Host '  пароль         : не создавался (-SkipAccount)'
}
Write-Host ''
Write-Host ' УДАЛИТЬ ВРЕМЕННЫЙ ДОСТУП ПОСЛЕ РАБОТЫ (на планшете, от администратора):' -ForegroundColor Yellow
Write-Host ("   Remove-LocalUser -Name $UserName")
Write-Host ("   Remove-NetFirewallRule -DisplayName 'nabu remote $Port'")
Write-Host '   Disable-PSRemoting -Force'
Write-Host ''
Write-Host ' Проверить, что планшет доступен с компьютера агента:' -ForegroundColor Yellow
Write-Host ("   Test-WSMan -ComputerName " + $address + ' -Credential (Get-Credential)')
Write-Host ''
