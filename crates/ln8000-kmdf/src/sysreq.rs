//! Системный запрос питания: не дать платформе уйти в Connected Standby.
//!
//! Живые данные 19.09.2026: планшет умер четыре раза за одно утро
//! (07:58, 08:25, 08:30, 08:42) — `Kernel-Power 41` с `BugcheckCode=0`,
//! `PowerButtonTimestamp=0`, `LongPowerButtonPressDetected=false` и
//! `ConnectedStandbyInProgress=true`, то есть ни краха, ни кнопки: система
//! входит в Modern Standby и не выходит. Каждый раз это происходило в простое
//! через 4–12 минут после загрузки. Платформа заявляет S0 low-power idle
//! единственным доступным состоянием (S1/S2/S3 недоступны), FADT несёт
//! `LOW_POWER_S0_IDLE_CAPABLE`, а источников пробуждения у порта нет — значит
//! вход в CS для неё односторонний.
//!
//! Что именно вводит систему в CS — **выключение экрана** (на CS-машинах это и
//! есть переход), поэтому оставлять экран горящим навсегда нельзя: это лишний
//! расход и нагрев. Запрос `PowerRequestSystemRequired` говорит диспетчеру
//! питания, что устройству нужно рабочее состояние S0, и система больше не
//! уходит в CS, хотя экран гаснет по своему таймауту.
//!
//! Запрос берётся на всё время жизни устройства (создаётся в
//! `EvtDevicePrepareHardware`, снимается в `EvtDeviceReleaseHardware`) и виден
//! снаружи как строка `SYSTEM` в `powercfg /requests` — это и есть живая
//! проверка.

use core::ptr;
use wdk_sys::{NTSTATUS, PVOID, ULONG, USHORT, WDFDEVICE};

/// `POWER_REQUEST_TYPE::PowerRequestSystemRequired`.
const POWER_REQUEST_SYSTEM_REQUIRED: i32 = 1;
/// `POWER_REQUEST_CONTEXT_VERSION`.
const POWER_REQUEST_CONTEXT_VERSION: ULONG = 0;
/// `POWER_REQUEST_CONTEXT_SIMPLE_STRING`: строка причины лежит прямо в контексте.
const POWER_REQUEST_CONTEXT_SIMPLE_STRING: ULONG = 1;

/// Текст причины (виден в `powercfg /requests`).
const REASON_TEXT: &str = "nabu ln8000 charge pump";
/// Длина строки причины в код-юнитах UTF-16.
const REASON_LEN: usize = REASON_TEXT.len();

/// ASCII → UTF-16 на этапе компиляции (в строке только ASCII).
const fn widen(text: &str) -> [u16; REASON_LEN] {
    let bytes = text.as_bytes();
    let mut out = [0u16; REASON_LEN];
    let mut i = 0;
    while i < REASON_LEN {
        out[i] = bytes[i] as u16;
        i += 1;
    }
    out
}

/// Строка причины в UTF-16; на неё смотрит `UNICODE_STRING` внутри контекста.
static REASON: [u16; REASON_LEN] = widen(REASON_TEXT);

/// `UNICODE_STRING` (x64: 2 + 2 + выравнивание + указатель = 16 байт).
#[repr(C)]
struct UnicodeString {
    length: USHORT,
    maximum_length: USHORT,
    buffer: *mut u16,
}

/// `COUNTED_REASON_CONTEXT` с вариантом `SimpleString`.
#[repr(C)]
struct CountedReasonContext {
    version: ULONG,
    flags: ULONG,
    simple_string: UnicodeString,
}

unsafe extern "C" {
    /// Создаёт объект запроса питания (`ntoskrnl`), PASSIVE_LEVEL.
    fn PoCreatePowerRequest(
        power_request: *mut PVOID,
        device_object: wdk_sys::PDEVICE_OBJECT,
        context: *mut CountedReasonContext,
    ) -> NTSTATUS;
    /// Ставит запрос питания; IRQL <= DISPATCH_LEVEL.
    fn PoSetPowerRequest(power_request: PVOID, request_type: i32) -> NTSTATUS;
    /// Снимает запрос питания.
    fn PoClearPowerRequest(power_request: PVOID, request_type: i32) -> NTSTATUS;
    /// Удаляет объект запроса питания.
    fn PoDeletePowerRequest(power_request: PVOID);
}

/// Действующий запрос (нулевой — не создан).
///
/// Живёт до [`release`]; устройство одно, поэтому и запрос один.
static mut REQUEST: PVOID = ptr::null_mut();

/// Берёт `PowerRequestSystemRequired` на устройство `device`.
///
/// Возвращает `STATUS_SUCCESS` и при повторном вызове, если запрос уже взят:
/// подготовка железа может выполняться несколько раз за загрузку.
/// Ошибки не фатальны — драйвер насоса обязан работать и без этой защиты, но
/// код возврата уходит в метку `SysReqSt`.
pub unsafe fn acquire(device: WDFDEVICE) -> NTSTATUS {
    if !unsafe { REQUEST }.is_null() {
        return wdk_sys::STATUS_SUCCESS;
    }
    let fdo: wdk_sys::PDEVICE_OBJECT =
        unsafe { wdk_sys::call_unsafe_wdf_function_binding!(WdfDeviceWdmGetDeviceObject, device) };
    if fdo.is_null() {
        return wdk_sys::STATUS_INVALID_PARAMETER;
    }
    // SAFETY: `REASON` — статический буфер, живёт дольше контекста; строка
    // только ASCII, поэтому байты равны код-юнитам.
    let mut context = CountedReasonContext {
        version: POWER_REQUEST_CONTEXT_VERSION,
        flags: POWER_REQUEST_CONTEXT_SIMPLE_STRING,
        simple_string: UnicodeString {
            length: USHORT::try_from(REASON_LEN * 2).unwrap_or(u16::MAX),
            maximum_length: USHORT::try_from(REASON_LEN * 2).unwrap_or(u16::MAX),
            buffer: REASON.as_ptr().cast_mut(),
        },
    };
    let mut request: PVOID = ptr::null_mut();
    // SAFETY: контекст и приёмник — локальные, `fdo` проверен на ноль.
    let created = unsafe { PoCreatePowerRequest(&mut request, fdo, &mut context) };
    if created < 0 {
        return created;
    }
    // SAFETY: объект создан; тип запроса — документированная константа.
    let set = unsafe { PoSetPowerRequest(request, POWER_REQUEST_SYSTEM_REQUIRED) };
    if set < 0 {
        unsafe { PoDeletePowerRequest(request) };
        return set;
    }
    unsafe {
        REQUEST = request;
    }
    wdk_sys::STATUS_SUCCESS
}

/// Снимает и удаляет запрос (вызывается из `EvtDeviceReleaseHardware`).
pub unsafe fn release() {
    let request = unsafe { REQUEST };
    if request.is_null() {
        return;
    }
    // SAFETY: объект наш и ещё не удалён.
    unsafe {
        let _ = PoClearPowerRequest(request, POWER_REQUEST_SYSTEM_REQUIRED);
        PoDeletePowerRequest(request);
        REQUEST = ptr::null_mut();
    }
}

