# nabu-fastcharge

[English](README.md) | **Русский** | [Português (Brasil)](README.pt-BR.md)

> Это перевод [английского README](README.md). Он основной: если тексты
> разойдутся, верен английский.

Драйвер зарядки для **Xiaomi Pad 5 (nabu, Snapdragon 860) под Windows on
ARM64**.

Под Windows планшет не заряжается ни от одного блока питания — ни от USB-A
(Quick Charge), ни от USB-C (Power Delivery). Причина не в «отсутствии быстрой
зарядки», а в том, что **аппаратная детекция адаптера (APSD) в PMIC выполняется,
но ни один компонент Windows её результат не читает**, поэтому лимит входного
тока не поднимается и заряд не идёт.

Драйвер закрывает ровно этот пробел: читает результат детекции и применяет
политику тока.

Ценность этой работы не в драйвере, а в том, что удалось выяснить про железо:
[docs/FINDINGS.md](docs/FINDINGS.md) собирает шесть находок про зарядку на этой
платформе, каждая со ссылкой на код или на замер. Начинайте оттуда, если переносите
Windows на планшет класса nabu, а не пользуетесь этим драйвером.

## Состав

| Крейт | Что это | Проверка |
|---|---|---|
| [`crates/core`](crates/core) | ядро логики SMB: детекция APSD, политика тока, состояния, таймауты, журнал. Без `std`, без `unsafe` | 37 unit-тестов |
| [`crates/ln8000`](crates/ln8000) | ядро драйвера charge pump LN8000 (I²C 0x51): регистры, режимы, защиты, АЦП, телеметрия сеансов, тепловая защита. Без `std`, без `unsafe` | 125 unit-тестов, 6 интеграционных тестов, 2 doctests |
| [`crates/spb`](crates/spb) | типы SPB и сборка списка передач, общие для драйверов режима ядра; объявлены вручную, потому что `wdk-sys` их не генерирует | 10 unit-тестов |
| [`crates/ln8000-kmdf`](crates/ln8000-kmdf) | KMDF-драйвер LN8000 на узле ACPI `PEIC` поверх I²C (SPB / Resource Hub) плюс скрипты установки и диагностики | собран под ARM64 |
| [`crates/host`](crates/host) | хост-слой: транспорты (мок, TCP), журнал JSONL, `tracing`, симулятор устройства, бенчмарки | 9 интеграционных тестов, 2 doctests |
| [`crates/cli`](crates/cli) | утилита `nabu-charger`: `demo`, `detect`, `sim`, `pump`, `verify` | 4 теста CLI |
| [`crates/kmdf`](crates/kmdf) | драйвер режима ядра (KMDF) для ARM64 через шину SPMI | собран: `kmdf.sys` ARM64, подписан, `infverif` пройден |

`cargo test --workspace` прогоняет 195 тестов — unit, интеграционные и doctests —
в пяти крейтах корневого воркспейса. Оба драйвера режима ядра объявляют
собственный воркспейс: им нужны WDK и `cargo-wdk`, которых в обычном окружении CI
нет.

## Быстрый старт

```powershell
# 1. Проверки (то же, что гоняет CI)
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace

# 2. Демонстрация: все типы адаптеров на мок-транспорте + журнал
cargo run -p cli -- --journal artifacts/journal-demo.jsonl demo

# 3. Charge pump LN8000 на мок-шине I²C (настройка -> режим 2:1 -> статус -> АЦП)
cargo run -p cli -- pump --profile qc35

# 4. Самопроверка таблиц и математики драйвера
cargo run -p cli -- verify

# 5. Стенд без железа: симулятор устройства + реальный TCP-транспорт
cargo run -p cli -- sim --adapter hvdcp3 --listen 127.0.0.1:9700
cargo run -p cli -- detect --transport tcp --addr 127.0.0.1:9700
```

Вывод `pump` на мок-шине. Мок детерминирован, поэтому этот вывод воспроизводится
байт в байт:

```text
bus           : mock
state         : probed
after configuration: configured
mode          : SWITCHING (code 3)
SYS_STS       : 0x04 (current loop: no, voltage loop: no)
faults        : none

ADC readings (mock), alarm channels:
  iin       ADC1     489000 uA
  vin       ADC3     192000 uV
  vbat      ADC6    3340000 uV

operations    : writes 33, reads 84
after standby : STANDBY
```

Вывод `demo`. Два отказа здесь намеренные: они проверяют путь ошибки.

```text
scenario   result adapter    current,µA pump   note
------------------------------------------------------------------------------
HVDCP3P5   ok     HVDCP3     3000000 yes    Quick Charge 3.0, 9 V and 3 A, charge pump possible
HVDCP3     ok     HVDCP3     3000000 yes    Quick Charge 3.0, 9 V and 3 A, charge pump possible
HVDCP2     ok     HVDCP2     1500000 no     Quick Charge 2.0, 9 V and 1.5 A
DCP        ok     DCP        1500000 no     charging-only port, BC1.2 1.5 A
SDP        ok     SDP        500000  no     standard USB port, 500 mA limit
DETACHED   failure -          -       -      error detection_timeout - no power: a timeout is expected
UNKNOWN    failure -          -       -      error unknown_adapter_pattern - unknown pattern: a failure is expected

journal: artifacts\journal-demo.jsonl
```

Программа печатает по-английски, поэтому вывод выше приведён дословно, без
перевода.

`verify` заканчивается строкой:

```text
self-check: passed (policies, current grid, APSD decoding, error path)
```

## Как это работает

```text
клиент (IOCTL) ──► драйвер KMDF ──► ядро логики ──► транспорт ──► \Device\RESOURCE_HUB (SPMI) ──► SMB в PM8150B
                                     │
                                     ├─ читает APSD_STATUS / APSD_RESULT_STATUS
                                     ├─ разбирает тип адаптера (таблица из Android)
                                     └─ пишет лимит входного тока и напряжение QC2
```

Ядро не знает ни про Windows, ни про ввод-вывод: оно работает поверх трейта
[`ChargerTransport`](crates/core/src/transport.rs), а время и журнал получает
извне. Поэтому вся логика, включая таймауты и восстановление после сбоев,
проверяется без железа.

Подробности: [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md),
[docs/REGISTERS.md](docs/REGISTERS.md), [docs/LN8000.md](docs/LN8000.md).

## Ограничения и честные оговорки

* **Набор IOCTL драйвера SMB ещё не разведён.** `GET_STATUS` отвечает.
  `READ_REG` возвращает структуру, у которой `error_code` равен
  `STATUS_NOT_IMPLEMENTED`, а `WRITE_REG`, `SET_ICL` и `GET_JOURNAL` возвращают
  `STATUS_NOT_IMPLEMENTED` напрямую. Детекция и применение политики идут по
  таймеру bring-up, поэтому `DETECT_START` и `APPLY_POLICY` отвечают так же.
  Логика за этими точками входа написана и покрыта тестами на моках — осталась
  проводка в ядре.
* **Раскладка ответа шины SPMI не подтверждена реверсом.** Транспорт собирает
  чтение регистра как «адрес 16 бит, затем байт», и драйвер держит поверх него
  живой `Charger`, но кадрирование ответа шины не доказано, поэтому
  [docs/REGISTERS.md](docs/REGISTERS.md) и
  [docs/SPMI-PATH.md](docs/SPMI-PATH.md) держат показания регистров
  предварительными. Догадка не выдаётся за факт.
* **Сборка драйверов режима ядра требует WDK.** `kmdf.sys` подписан, его INF
  прошёл `infverif`, разрядность подтверждена по полю машины в PE (`0xAA64`).
  Пакет кладётся в `artifacts/driver-arm64/`. Для `bindgen` нужен LLVM **17.x**.
* **Согласование напряжения (PD) остаётся за Type-C-частью платформы.** Без неё
  charge pump может держать уже согласованное напряжение или работать в bypass
  от 5 В, но не выдаст полные 33 Вт.
* **Драйвер LN8000 развёрнут и измеряется, а не закончен.** KMDF-драйвер для узла
  ACPI `PEIC` (адрес I²C 0x51) собран и установлен на планшете.
  [docs/STATE-2026-09-17.md](docs/STATE-2026-09-17.md) перечисляет ещё открытые
  дефекты, а [docs/DEPLOY-LN8000.md](docs/DEPLOY-LN8000.md) — процедура установки
  и диагностики.

## Сборка драйвера

```powershell
rustup target add aarch64-pc-windows-msvc
cargo install cargo-wdk --locked
$env:LIBCLANG_PATH = "C:\Program Files\LLVM\bin"   # нужен LLVM 17.0.6

# драйвер SMB-детекции (детекция блока и лимит входного тока)
cd crates/kmdf        ; cargo wdk build --target-arch arm64 --profile release

# драйвер charge pump LN8000 (узел PEIC, I2C 0x51)
cd crates/ln8000-kmdf ; cargo wdk build --target-arch arm64 --profile release
```

Пакеты кладутся в `artifacts/driver-arm64/` (SMB) и
`artifacts/driver-ln8000-arm64/` (LN8000). Оба собираются локально и в git не
хранятся.

Установка и диагностика LN8000 — [docs/DEPLOY-LN8000.md](docs/DEPLOY-LN8000.md):
`install-driver.ps1`, `nabu-ln8000.ps1 status|sessions|read|write|journal`,
`run-acceptance.ps1` (автоматический протокол приёмки) и `uninstall-driver.ps1`.
Скрипт PowerShell, в котором есть не-ASCII текст, хранится в UTF-8 с BOM: иначе
PowerShell 5.1 читает его как ANSI и ломает разбор кавычек.

## Лицензия и политика доступа

**Лицензия: GPL-2.0-or-later** ([LICENSE](LICENSE)). Логика LN8000 — порт
GPL-2.0-or-later драйвера из ядра Android для того же чипа, поэтому репозиторий не
может быть MIT/Apache. Что откуда взято, расписано в
[PROVENANCE.md](PROVENANCE.md).

**Политика доступа: насос открыт только для `LocalSystem` и администраторов.**
Драйвер ставит такой дескриптор на объект устройства, INF — такой же на узел
устройства, потому что все коды управления объявлены `FILE_ANY_ACCESS`, а проверки
того, кто именно пришёл, в драйвере нет. Поэтому скриптам в `deploy/` нужен
повышенный уровень прав; чтению меток телеметрии — нет, это значения реестра.
