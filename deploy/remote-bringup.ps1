#Requires -Version 5.1
<#
    remote-bringup.ps1 — вся работа на планшете выполняется с этого компьютера.

    Запуск (здесь, на компьютере разработки):
        .\remote-bringup.ps1 -Computer 192.168.1.50 -User nabuagent -Password 'Nb!...'

    Что делает по шагам:
      1) проверяет связь с планшетом и открывает сессию удалённого управления;
      2) копирует комплект драйвера на планшет;
      3) проверяет режим подписи и, если он был выключен, включает его и просит
         вас перезагрузить планшет (после перезагрузки запустите скрипт снова);
      4) ставит драйвер и запускает сбор отчёта (bring-up.ps1) прямо на планшете;
      5) приносит отчёт, архив, протокол приёмки и отчёт Windows о питании в
         папку G:\nabu-fast-charge\12-reports;
      6) по ключу -RemoveAccess удаляет временный доступ на планшете.

    Оговорка: удалённое управление по WinRM в локальной сети шифрует трафик
    слабо. Годится для доверенной домашней сети; в открытой сети так делать не надо.
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

Write-Host ('=== планшет ' + $Computer + ' ===') -ForegroundColor Cyan

Write-Host '1. проверяю связь' -ForegroundColor Cyan
try {
    $wsman = Test-WSMan -ComputerName $Computer -Credential $credential -ErrorAction Stop
    Write-Host ('  связь есть: ' + $wsman.ProductVendor + ' ' + $wsman.ProductVersion)
} catch {
    throw ('планшет недоступен: ' + $_.Exception.Message +
           '. Проверьте адрес, что на планшете выполнен enable-remote.ps1 и что вы в одной сети')
}

$options = New-PSSessionOption -OperationTimeout 300000 -IdleTimeout 600000
$session = New-PSSession -ComputerName $Computer -Credential $credential -SessionOption $options
Write-Host ('  сессия открыта: ' + $session.Id)

try {
    Write-Host '2. копирую комплект' -ForegroundColor Cyan
    Invoke-Command -Session $session -ScriptBlock {
        param($dir)
        New-Item -ItemType Directory -Path $dir -Force | Out-Null
    } -ArgumentList $RemoteDir | Out-Null
    Copy-Item -Path (Join-Path $kit '*') -Destination $RemoteDir -ToSession $session -Force -Recurse
    Write-Host ('  файлов отправлено: ' + (Get-ChildItem $kit -File).Count)

    Write-Host '3. проверяю режим подписи драйверов' -ForegroundColor Cyan
    $signing = Invoke-Command -Session $session -ScriptBlock {
        $out = & bcdedit /enum '{current}' 2>&1 | Out-String
        [pscustomobject]@{ Enabled = ($out -match 'testsigning\s+Yes') }
    }
    if (-not $signing.Enabled -and -not $SkipSignatureCheck) {
        Write-Host '  режим выключен — включаю и перезагружаю планшет' -ForegroundColor Yellow
        Invoke-Command -Session $session -ScriptBlock {
            & bcdedit /set testsigning on | Out-Null
            Restart-Computer -Force
        } | Out-Null
        Write-Host ''
        Write-Host '  Планшет уходит на перезагрузку. Когда он поднимется —' -ForegroundColor Yellow
        Write-Host '  запустите этот же скрипт ещё раз (он увидит, что режим уже включён).' -ForegroundColor Yellow
        return
    }
    Write-Host '  режим подписи включён'

    Write-Host '4. запускаю сбор отчёта на планшете' -ForegroundColor Cyan
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

    Write-Host '5. забираю отчёт' -ForegroundColor Cyan
    $copied = 0
    foreach ($file in $result.Files) {
        Copy-Item -LiteralPath $file.FullName -Destination $InboxDir -FromSession $session -Force
        $copied++
        Write-Host ('  получено: ' + $file.Name)
    }

    Write-Host ''
    Write-Host ('=== ключевые строки отчёта (всего файлов: ' + $copied + ') ===') -ForegroundColor Cyan
    $result.Output -split "`r?`n" |
        Where-Object { $_ -match 'вывод\s*:|разница|узел|Problem|режим|заряд|ОШИБКА|не удалось' } |
        Select-Object -First 20 | ForEach-Object { Write-Host ('  ' + $_.Trim()) }

    Write-Host ''
    Write-Host ('Отчёты лежат в: ' + $InboxDir) -ForegroundColor Green
} finally {
    if ($RemoveAccess) {
        Write-Host '6. убираю временный доступ на планшете' -ForegroundColor Cyan
        try {
            Invoke-Command -Session $session -ScriptBlock {
                param($name)
                Remove-LocalUser -Name $name -ErrorAction SilentlyContinue
                Remove-NetFirewallRule -DisplayName 'nabu remote 5985' -ErrorAction SilentlyContinue
                Disable-PSRemoting -Force -ErrorAction SilentlyContinue
                'доступ убран'
            } -ArgumentList $User | ForEach-Object { Write-Host ('  ' + $_) }
        } catch {
            Write-Host ('  не удалось убрать автоматически: ' + $_.Exception.Message) -ForegroundColor Yellow
            Write-Host '  выполните на планшете: Remove-LocalUser -Name ' -NoNewline
            Write-Host $User
        }
    }
    Remove-PSSession -Session $session -ErrorAction SilentlyContinue
    Write-Host 'сессия закрыта'
}
