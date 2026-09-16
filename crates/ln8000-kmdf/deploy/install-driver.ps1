# install-driver.ps1 — установка драйвера charge pump LN8000 на планшет nabu
#
# Запуск: PowerShell от имени администратора на планшете, из папки с пакетом:
#     .\install-driver.ps1
#
# Что делает:
#   1) проверяет, что включена тестовая подпись драйверов (драйвер подписан
#      тестовым сертификатом WDK);
#   2) ставит пакет через pnputil;
#   3) проверяет, что устройство ACPI\QCOM057E получило драйвер ln8000_kmdf;
#   4) запускает службу и печатает состояние через диагностическую утилиту.
#
# Откат: .\uninstall-driver.ps1

[CmdletBinding()]
param(
  [string]$PackageDir = $PSScriptRoot,
  [switch]$SkipSignatureCheck
)

$ErrorActionPreference = 'Stop'
$service = 'ln8000_kmdf'
$hwid = 'ACPI\QCOM057E'

function Assert-Admin {
  $identity = [Security.Principal.WindowsIdentity]::GetCurrent()
  $principal = New-Object Security.Principal.WindowsPrincipal($identity)
  if (-not $principal.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)) {
    throw 'нужны права администратора: перезапустите PowerShell от имени администратора'
  }
}

function Get-TestSigning {
  $out = & bcdedit /enum '{current}' 2>&1 | Out-String
  return ($out -match 'testsigning\s+Yes')
}

Assert-Admin

$inf = Join-Path $PackageDir 'ln8000_kmdf.inf'
if (-not (Test-Path -LiteralPath $inf)) {
  throw "не найден $inf — положите рядом с скриптом пакет драйвера (inf, sys, cat)"
}

Write-Host '=== 1. проверка подписи ===' -ForegroundColor Cyan
if (-not $SkipSignatureCheck) {
  if (Get-TestSigning) {
    Write-Host '  тестовая подпись включена (testsigning Yes)' -ForegroundColor Green
  } else {
    Write-Host '  тестовая подпись ВЫКЛЮЧЕНА.' -ForegroundColor Yellow
    Write-Host '  Драйвер подписан тестовым сертификатом WDK, поэтому включите режим:' -ForegroundColor Yellow
    Write-Host '      bcdedit /set testsigning on' -ForegroundColor Yellow
    Write-Host '  и перезагрузите планшет, затем повторите установку.' -ForegroundColor Yellow
    throw 'тестовая подпись выключена'
  }
}

Write-Host '=== 2. установка пакета ===' -ForegroundColor Cyan
& pnputil /add-driver $inf /install
if ($LASTEXITCODE -ne 0) { throw "pnputil вернул код $LASTEXITCODE" }

Write-Host '=== 3. проверка привязки драйвера ===' -ForegroundColor Cyan
$device = Get-PnpDevice -PresentOnly | Where-Object { $_.InstanceId -like "*$hwid*" } | Select-Object -First 1
if (-not $device) {
  Write-Host "  устройство $hwid не найдено" -ForegroundColor Red
  Write-Host '  проверьте, что загружен UEFI-образ с узлом PEIC (см. docs/DEPLOY-LN8000.md)' -ForegroundColor Yellow
} else {
  $driverService = (Get-PnpDeviceProperty -InstanceId $device.InstanceId -KeyName 'DEVPKEY_Device_Service' -ErrorAction SilentlyContinue).Data
  Write-Host ("  устройство: " + $device.InstanceId)
  Write-Host ("  состояние : " + $device.Status)
  Write-Host ("  служба    : " + $driverService)
  if ($driverService -ne $service) {
    Write-Host "  ОЖИДАЛАСЬ служба $service — проверьте, что INF встал на этот _HID" -ForegroundColor Yellow
  }
}

Write-Host '=== 4. запуск службы ===' -ForegroundColor Cyan
& sc.exe start $service 2>&1 | ForEach-Object { "  $_" }

Write-Host '=== 5. состояние устройства ===' -ForegroundColor Cyan
$diag = Join-Path $PackageDir 'nabu-ln8000.ps1'
if (Test-Path -LiteralPath $diag) {
  & $diag status
} else {
  Write-Host '  диагностическая утилита nabu-ln8000.ps1 не найдена рядом со скриптом' -ForegroundColor Yellow
}

Write-Host ''
Write-Host 'Установка завершена. Дальше:' -ForegroundColor Green
Write-Host '  .\nabu-ln8000.ps1 status      — режим и телеметрия'
Write-Host '  .\nabu-ln8000.ps1 sessions    — сеансы заряда'
Write-Host '  .\nabu-ln8000.ps1 journal out.jsonl — выгрузка журнала'
