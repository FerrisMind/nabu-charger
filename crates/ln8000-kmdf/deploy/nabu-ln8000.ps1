# nabu-ln8000.ps1 — диагностика драйвера charge pump LN8000
#
# Открывает устройство \\.\nabu_ln8000 (символическая ссылка, созданная
# драйвером) и общается с ним через DeviceIoControl. Коды управления вычисляются
# по той же формуле, что в crates/ln8000-kmdf/src/ioctl.rs:
#     CTL_CODE(0x22, function, METHOD_BUFFERED, FILE_ANY_ACCESS)
#
# Команды:
#     .\nabu-ln8000.ps1 status            — режим, отказы, телеметрия, счётчики
#     .\nabu-ln8000.ps1 sessions          — сеансы заряда (текущий и последний)
#     .\nabu-ln8000.ps1 read <reg>        — прочитать регистр LN8000 (hex)
#     .\nabu-ln8000.ps1 write <reg> <val> — записать регистр (hex), диагностика
#     .\nabu-ln8000.ps1 journal <file>    — выгрузить журнал сеансов в файл JSONL
#     .\nabu-ln8000.ps1 limits <mA> [mV]  — задать лимиты: ток в мА, напряжение в мВ
#     .\nabu-ln8000.ps1 mode <1|2|3>      — режим: 1 standby, 2 bypass, 3 switching

[CmdletBinding()]
param(
  [Parameter(Position = 0)][string]$Command = 'status',
  [Parameter(Position = 1)][string]$Arg1,
  [Parameter(Position = 2)][string]$Arg2,
  [string]$DevicePath = '\\.\nabu_ln8000'
)

$ErrorActionPreference = 'Stop'

# --- контракт драйвера (см. crates/ln8000-kmdf/src/ioctl.rs) ---

function Get-CtlCode {
  param([int]$Function, [int]$Method = 0, [int]$Access = 0)
  return ((0x22 -shl 16) -bor ($Access -shl 14) -bor ($Function -shl 2) -bor $Method)
}

$IOCTL = @{
  GET_STATUS   = Get-CtlCode 0x810
  READ_REG     = Get-CtlCode 0x811
  WRITE_REG    = Get-CtlCode 0x812
  SET_LIMITS   = Get-CtlCode 0x813
  SET_MODE     = Get-CtlCode 0x814
  GET_SESSIONS = Get-CtlCode 0x815
  GET_SAMPLES  = Get-CtlCode 0x816
}

$STATUS_MAGIC = 0x4C4E3830  # "LN80"

# --- P/Invoke нативного API ---

if (-not ('NabuNative' -as [type])) {
  Add-Type -Namespace '' -Name 'NabuNative' -MemberDefinition @'
[DllImport("kernel32.dll", SetLastError = true, CharSet = CharSet.Unicode)]
public static extern IntPtr CreateFileW(string name, uint access, uint share, IntPtr security,
                                        uint disposition, uint flags, IntPtr template);
[DllImport("kernel32.dll", SetLastError = true)]
public static extern bool DeviceIoControl(IntPtr handle, uint code, byte[] input, uint inputSize,
                                          byte[] output, uint outputSize, out uint returned, IntPtr overlapped);
[DllImport("kernel32.dll", SetLastError = true)]
public static extern bool CloseHandle(IntPtr handle);
'@
}

function Open-Device {
  $handle = [NabuNative]::CreateFileW($DevicePath, 0xC0000000, 3, [IntPtr]::Zero, 3, 0, [IntPtr]::Zero)
  if ($handle -eq [IntPtr]::new(-1)) {
    throw ("не удалось открыть $DevicePath (код " + [Runtime.InteropServices.Marshal]::GetLastWin32Error() +
           "). Драйвер установлен и устройство PEIC доступно?")
  }
  return $handle
}

function Invoke-DeviceIoControl {
  param([IntPtr]$Handle, [uint32]$Code, [byte[]]$Input, [uint32]$OutputSize)
  $output = New-Object byte[] $OutputSize
  $returned = 0
  $ok = [NabuNative]::DeviceIoControl($Handle, $Code, $Input, [uint32]($Input.Length), $output, $OutputSize, [ref]$returned, [IntPtr]::Zero)
  return [pscustomobject]@{ Ok = $ok; Bytes = $output; Returned = $returned; Error = [Runtime.InteropServices.Marshal]::GetLastWin32Error() }
}

function Read-Struct {
  param([byte[]]$Bytes, [Type]$Type)
  $size = [Runtime.InteropServices.Marshal]::SizeOf($Type)
  $buffer = New-Object byte[] $size
  [Array]::Copy($Bytes, $buffer, [Math]::Min($Bytes.Length, $size))
  $handle = [Runtime.InteropServices.GCHandle]::Alloc($buffer, 'Pinned')
  try {
    return [Runtime.InteropServices.Marshal]::PtrToStructure($handle.AddrOfPinnedObject(), $Type)
  } finally {
    $handle.Free()
  }
}

# --- структуры ответов (порядок полей совпадает с Rust) ---

if (-not ('Ln8000Status' -as [type])) {
  Add-Type -TypeDefinition @'
using System.Runtime.InteropServices;
[StructLayout(LayoutKind.Sequential)]
public struct Ln8000Status {
    public uint Magic; public ushort Version; public byte OpMode; public byte State;
    public byte SysSts; public byte Fault1Sts; public byte Fault2Sts; public byte SafetySts;
    public byte CriticalFault; public byte Reserved0; public byte Reserved1; public byte Reserved2;
    public uint IinUa; public uint VbatUv; public uint VbusUv; public int DieTempDc;
    public ulong Sessions; public ulong Samples; public uint Writes; public uint Reads;
    public int LastError;
}
[StructLayout(LayoutKind.Sequential)]
public struct Ln8000Sessions {
    public ulong Total; public ulong CurrentMs; public uint CurrentPeakIinUa;
    public byte CurrentFast; public byte R0; public byte R1; public byte R2;
    public ulong LastMs; public uint LastPeakIinUa; public int LastPeakTempDc;
    public byte LastFast; public byte S0; public byte S1; public byte S2;
}
[StructLayout(LayoutKind.Sequential)]
public struct Ln8000Reg {
    public byte Addr; public byte Value; public byte R0; public byte R1; public int ErrorCode;
}
'@
}

function Invoke-Status {
  $handle = Open-Device
  try {
    $result = Invoke-DeviceIoControl -Handle $handle -Code $IOCTL.GET_STATUS -Input (New-Object byte[] 0) -OutputSize 64
    if (-not $result.Ok) { throw ("DeviceIoControl GET_STATUS: ошибка " + $result.Error) }
    $status = Read-Struct $result.Bytes ([Ln8000Status])
    if ($status.Magic -ne $STATUS_MAGIC) {
      Write-Host ("  ВНИМАНИЕ: неизвестная магия 0x{0:X8}" -f $status.Magic) -ForegroundColor Yellow
    }
    $modeName = switch ($status.OpMode) { 0 {'UNKNOWN'} 1 {'STANDBY'} 2 {'BYPASS 1:1'} 3 {'SWITCHING 2:1'} default {'?'} }
    $stateName = switch ($status.State) { 0 {'закрыта'} 1 {'опознан'} 2 {'настроен'} 3 {'switching'} 4 {'ОТКАЗ'} default {'?'} }
    Write-Host '=== LN8000: состояние ===' -ForegroundColor Cyan
    Write-Host ("  режим            : " + $modeName + " (" + $status.OpMode + ")")
    Write-Host ("  состояние сессии : " + $stateName)
    Write-Host ("  SYS_STS          : 0x{0:X2}" -f $status.SysSts)
    Write-Host ("  FAULT1/FAULT2    : 0x{0:X2} / 0x{1:X2}" -f $status.Fault1Sts, $status.Fault2Sts)
    Write-Host ("  SAFETY_STS       : 0x{0:X2}" -f $status.SafetySts)
    Write-Host ("  критичный отказ  : " + $(if ($status.CriticalFault -ne 0) { 'ДА' } else { 'нет' })) `
      -ForegroundColor $(if ($status.CriticalFault -ne 0) { 'Red' } else { 'Gray' })
    Write-Host ("  входной ток      : {0} мкА ({1:N2} А)" -f $status.IinUa, ($status.IinUa / 1e6))
    Write-Host ("  напряжение батареи: {0} мкВ ({1:N3} В)" -f $status.VbatUv, ($status.VbatUv / 1e6))
    Write-Host ("  напряжение входа : {0} мкВ ({1:N3} В)" -f $status.VbusUv, ($status.VbusUv / 1e6))
    Write-Host ("  температура крист.: {0} (0.1 °C)" -f $status.DieTempDc)
    Write-Host ("  сеансов/отсчётов : {0} / {1}" -f $status.Sessions, $status.Samples)
    Write-Host ("  записей/чтений   : {0} / {1}" -f $status.Writes, $status.Reads)
    if ($status.LastError -ne 0) {
      Write-Host ("  последняя ошибка : " + $status.LastError) -ForegroundColor Yellow
    }
    return $status
  } finally {
    [NabuNative]::CloseHandle($handle) | Out-Null
  }
}

function Invoke-Sessions {
  $handle = Open-Device
  try {
    $result = Invoke-DeviceIoControl -Handle $handle -Code $IOCTL.GET_SESSIONS -Input (New-Object byte[] 0) -OutputSize 48
    if (-not $result.Ok) { throw ("DeviceIoControl GET_SESSIONS: ошибка " + $result.Error) }
    $s = Read-Struct $result.Bytes ([Ln8000Sessions])
    Write-Host '=== LN8000: сеансы заряда ===' -ForegroundColor Cyan
    Write-Host ("  всего сеансов    : " + $s.Total)
    if ($s.CurrentMs -gt 0) {
      Write-Host ("  текущий          : {0:N1} с, пик {1:N2} А, быстрый режим: {2}" -f ($s.CurrentMs / 1000), ($s.CurrentPeakIinUa / 1e6), $(if ($s.CurrentFast -ne 0) { 'да' } else { 'нет' }))
    } else {
      Write-Host '  текущий          : питания нет'
    }
    if ($s.LastMs -gt 0) {
      Write-Host ("  последний        : {0:N1} с, пик {1:N2} А, пик темп. {2} (0.1 °C), быстрый режим: {3}" -f ($s.LastMs / 1000), ($s.LastPeakIinUa / 1e6), $s.LastPeakTempDc, $(if ($s.LastFast -ne 0) { 'да' } else { 'нет' }))
    }
    return $s
  } finally {
    [NabuNative]::CloseHandle($handle) | Out-Null
  }
}

function Invoke-ReadReg {
  param([int]$Address)
  $handle = Open-Device
  try {
    $input = New-Object byte[] 8
    $input[0] = [byte]$Address
    $result = Invoke-DeviceIoControl -Handle $handle -Code $IOCTL.READ_REG -Input $input -OutputSize 8
    if (-not $result.Ok) { throw ("DeviceIoControl READ_REG: ошибка " + $result.Error) }
    $reg = Read-Struct $result.Bytes ([Ln8000Reg])
    if ($reg.ErrorCode -ne 0) { throw ("драйвер вернул код ошибки " + $reg.ErrorCode) }
    Write-Host ("  регистр 0x{0:X2} = 0x{1:X2}" -f $reg.Addr, $reg.Value)
    return $reg.Value
  } finally {
    [NabuNative]::CloseHandle($handle) | Out-Null
  }
}

function Invoke-WriteReg {
  param([int]$Address, [int]$Value)
  $handle = Open-Device
  try {
    $input = New-Object byte[] 8
    $input[0] = [byte]$Address
    $input[1] = [byte]$Value
    $result = Invoke-DeviceIoControl -Handle $handle -Code $IOCTL.WRITE_REG -Input $input -OutputSize 8
    if (-not $result.Ok) { throw ("DeviceIoControl WRITE_REG: ошибка " + $result.Error) }
    Write-Host ("  записано 0x{0:X2} в 0x{1:X2}" -f $Value, $Address)
  } finally {
    [NabuNative]::CloseHandle($handle) | Out-Null
  }
}

function Invoke-Journal {
  param([string]$Path)
  Write-Host '=== журнал сеансов ===' -ForegroundColor Cyan
  $status = Invoke-Status
  $sessions = Invoke-Sessions
  $record = [ordered]@{
    exported_at = (Get-Date).ToString('o')
    host        = $env:COMPUTERNAME
    mode        = $status.OpMode
    state       = $status.State
    sys_sts     = $status.SysSts
    fault1_sts  = $status.Fault1Sts
    fault2_sts  = $status.Fault2Sts
    safety_sts  = $status.SafetySts
    critical    = $status.CriticalFault
    iin_ua      = $status.IinUa
    vbat_uv     = $status.VbatUv
    vbus_uv     = $status.VbusUv
    die_temp_dc = $status.DieTempDc
    sessions    = $sessions.Total
    samples     = $status.Samples
  }
  $json = $record | ConvertTo-Json -Compress
  Add-Content -LiteralPath $Path -Value $json -Encoding UTF8
  Write-Host ("  записано в " + $Path) -ForegroundColor Green
}

switch ($Command.ToLower()) {
  'status' { Invoke-Status | Out-Null }
  'sessions' { Invoke-Sessions | Out-Null }
  'read' {
    if (-not $Arg1) { throw 'укажите адрес регистра: read 1E' }
    Invoke-ReadReg ([Convert]::ToInt32($Arg1, 16)) | Out-Null
  }
  'write' {
    if (-not $Arg1 -or -not $Arg2) { throw 'укажите адрес и значение: write 1E 00' }
    Invoke-WriteReg ([Convert]::ToInt32($Arg1, 16)) ([Convert]::ToInt32($Arg2, 16))
  }
  'journal' {
    if (-not $Arg1) { throw 'укажите файл: journal out.jsonl' }
    Invoke-Journal $Arg1
  }
  default {
    Write-Host 'Команды: status | sessions | read <hex> | write <hex> <hex> | journal <file>'
    Write-Host ("Коды управления: GET_STATUS=0x{0:X6} READ=0x{1:X6} WRITE=0x{2:X6} SESSIONS=0x{3:X6}" -f `
      $IOCTL.GET_STATUS, $IOCTL.READ_REG, $IOCTL.WRITE_REG, $IOCTL.GET_SESSIONS)
  }
}
