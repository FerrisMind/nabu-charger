# uninstall-driver.ps1 — полное удаление драйвера LN8000 и возврат к штатному состоянию
#
# Запуск от администратора на планшете:
#     .\uninstall-driver.ps1
#
# Что делает:
#   1) останавливает службу ln8000_kmdf;
#   2) удаляет пакет драйвера из хранилища (pnputil /delete-driver /uninstall);
#   3) проверяет, что устройство ACPI\QCOM057E вернулось к штатному драйверу
#      (или осталось без драйвера — это ожидаемо, если штатного нет);
#   4) печатает, что осталось в системе, чтобы откат был проверяемым.
#
# Драйвер не пишет в прошивку и не меняет настройки питания, поэтому удаление
# возвращает устройство к поведению до установки.

[CmdletBinding()]
param(
  [string]$LogPath = "$env:ProgramData\nabu-fastcharge\uninstall.log"
)

$ErrorActionPreference = 'Stop'
$service = 'ln8000_kmdf'
$hwid = 'ACPI\QCOM057E'

$identity = [Security.Principal.WindowsIdentity]::GetCurrent()
$principal = New-Object Security.Principal.WindowsPrincipal($identity)
if (-not $principal.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)) {
  throw 'нужны права администратора'
}

New-Item -ItemType Directory -Path (Split-Path $LogPath) -Force | Out-Null
Start-Transcript -Path $LogPath -Force | Out-Null

try {
  Write-Host '=== 1. остановка службы ===' -ForegroundColor Cyan
  & sc.exe stop $service 2>&1 | ForEach-Object { "  $_" }

  Write-Host '=== 2. поиск пакета в хранилище ===' -ForegroundColor Cyan
  $published = & pnputil /enum-drivers | Out-String
  $blocks = $published -split "`r?`n`r?`n"
  $targets = @()
  foreach ($block in $blocks) {
    if ($block -match 'ln8000_kmdf\.inf') {
      if ($block -match 'oem\d+\.inf') { $targets += $Matches[0] }
    }
  }
  if ($targets.Count -eq 0) {
    Write-Host '  пакет ln8000_kmdf.inf в хранилище не найден — уже удалён?' -ForegroundColor Yellow
  } else {
    foreach ($oem in $targets) {
      Write-Host "  удаляю $oem"
      & pnputil /delete-driver $oem /uninstall 2>&1 | ForEach-Object { "    $_" }
    }
  }

  Write-Host '=== 3. состояние устройства после удаления ===' -ForegroundColor Cyan
  $device = Get-PnpDevice -PresentOnly | Where-Object { $_.InstanceId -like "*$hwid*" } | Select-Object -First 1
  if ($device) {
    $driverService = (Get-PnpDeviceProperty -InstanceId $device.InstanceId -KeyName 'DEVPKEY_Device_Service' -ErrorAction SilentlyContinue).Data
    Write-Host ("  устройство: " + $device.InstanceId)
    Write-Host ("  состояние : " + $device.Status)
    Write-Host ("  служба    : " + $driverService)
    if ($driverService -eq $service) {
      Write-Host '  ВНИМАНИЕ: устройство всё ещё на нашей службе — перезагрузите планшет' -ForegroundColor Yellow
    } else {
      Write-Host '  устройство вернулось к штатному драйверу (или осталось без него)' -ForegroundColor Green
    }
  } else {
    Write-Host "  устройства $hwid больше нет в системе (узел PEIC отсутствует)" -ForegroundColor Green
  }

  Write-Host '=== 4. что осталось в системе ===' -ForegroundColor Cyan
  Write-Host '  служба:'
  & sc.exe query $service 2>&1 | ForEach-Object { "    $_" }
  Write-Host '  файлы журнала драйвера (если выгружались):'
  if (Test-Path "$env:ProgramData\nabu-fastcharge") {
    Get-ChildItem "$env:ProgramData\nabu-fastcharge" | ForEach-Object { "    " + $_.Name }
  } else {
    Write-Host '    нет'
  }

  Write-Host ''
  Write-Host 'Удаление завершено. Журнал: ' -NoNewline -ForegroundColor Green
  Write-Host $LogPath
} finally {
  Stop-Transcript | Out-Null
}
