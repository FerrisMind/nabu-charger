# update-driver.ps1 — обновление драйвера LN8000 поверх предыдущей версии
#
# Запуск от администратора на планшете из папки с новым пакетом:
#     .\update-driver.ps1
#
# Обновление безопасно: сначала сохраняется версия установленного пакета,
# затем ставится новый. Если после установки устройство не поднялось, скрипт
# сообщает об этом и показывает, как откатиться (uninstall + install старого).

[CmdletBinding()]
param(
  [string]$PackageDir = $PSScriptRoot,
  [string]$BackupDir = "$env:ProgramData\nabu-fastcharge\backup"
)

$ErrorActionPreference = 'Stop'
$inf = Join-Path $PackageDir 'ln8000_kmdf.inf'
if (-not (Test-Path -LiteralPath $inf)) { throw "не найден $inf" }

$identity = [Security.Principal.WindowsIdentity]::GetCurrent()
$principal = New-Object Security.Principal.WindowsPrincipal($identity)
if (-not $principal.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)) {
  throw 'нужны права администратора'
}

$version = (Get-Item -LiteralPath (Join-Path $PackageDir 'ln8000_kmdf.sys')).VersionInfo.FileVersion
Write-Host ("=== обновление до версии " + $version) -ForegroundColor Cyan

# 1. Сохраняем текущий установленный пакет — это и есть точка отката.
New-Item -ItemType Directory -Path $BackupDir -Force | Out-Null
$published = & pnputil /enum-drivers | Out-String
$blocks = $published -split "`r?`n`r?`n"
foreach ($block in $blocks) {
  if ($block -match 'ln8000_kmdf\.inf' -and $block -match 'oem\d+\.inf') {
    $oem = $Matches[0]
    & pnputil /export-driver $oem $BackupDir 2>&1 | ForEach-Object { "  $_" }
    Write-Host ("  сохранён $oem в $BackupDir") -ForegroundColor Green
  }
}

# 2. Ставим новый пакет.
Write-Host '=== установка нового пакета ===' -ForegroundColor Cyan
& pnputil /add-driver $inf /install
if ($LASTEXITCODE -ne 0) { throw "pnputil вернул код $LASTEXITCODE" }

# 3. Перезапускаем службу, чтобы драйвер перечитал конфигурацию.
& sc.exe stop  ln8000_kmdf | Out-Null
& sc.exe start ln8000_kmdf | ForEach-Object { "  $_" }

# 4. Проверяем, что устройство живо.
Write-Host '=== проверка ===' -ForegroundColor Cyan
$device = Get-PnpDevice -PresentOnly | Where-Object { $_.InstanceId -like '*ACPI\QCOM057E*' } | Select-Object -First 1
if ($device) {
  Write-Host ("  состояние: " + $device.Status)
  if ($device.Status -ne 'OK') {
    Write-Host '  устройство не в состоянии OK.' -ForegroundColor Yellow
    Write-Host "  откат: .\uninstall-driver.ps1, затем pnputil /add-driver $BackupDir\ln8000_kmdf.inf /install" -ForegroundColor Yellow
  } else {
    Write-Host '  обновление прошло успешно' -ForegroundColor Green
  }
} else {
  Write-Host '  устройство PEIC не найдено — проверьте загрузку UEFI' -ForegroundColor Yellow
}
