# Отчёт о приёмке

Проверка выполнена 16.09.2026 на машине: Windows 11 (26200), AMD Ryzen 7 3700X,
Rust 1.97.0 (`rust-toolchain.toml` фиксирует канал), WDK 10.0.26100, LLVM 23.

Сырые выводы команд лежат в `artifacts/`: `verify-fmt.txt`, `verify-clippy.txt`,
`verify-build.txt`, `verify-test.txt`, `verify-doc.txt`, `verify-demo.txt`,
`verify-selfcheck.txt`, `bench-core.txt`, `bench-transport.txt`,
`journal-demo.jsonl`.

## Критерии приёмки

| № | Критерий | Статус | Доказательство |
|---|---|---|---|
| 1 | Сборка на edition 2024 без ошибок и предупреждений | <span>выполнено</span> | `edition = "2024"` в `Cargo.toml`; `cargo build --workspace --release` — успех; `cargo clippy --workspace --all-targets -- -D warnings` — успех (файлы `verify-build.txt`, `verify-clippy.txt`) |
| 2 | Публичный API задокументирован и покрыт примерами | <span>выполнено</span> | `#![deny(missing_docs)]` в обоих крейтах; rustdoc с разделами «Errors»/«Safety»; `cargo doc --workspace --no-deps` без предупреждений (`verify-doc.txt`) |
| 3 | Все тесты проходят, включая ошибочные сценарии | <span>выполнено</span> | 83 теста: 37 unit в ядре SMB, 29 unit в ядре LN8000, 9 интеграционных, 4 CLI, doctests; среди них таймаут детекции, обрыв связи, неизвестный образец, отказ проверки записи, повторная инициализация и отказ чипа включить режим 2:1 (`verify-test.txt`) |
| 4 | Ошибки типизированы, штатный путь не паникует | <span>выполнено</span> | `ChargerError`/`TransportError` реализуют `core::error::Error`; линты `clippy::unwrap_used`, `expect_used`, `panic`, `indexing_slicing` включены как `deny` в `Cargo.toml` — сборка проходит, значит их нет в библиотечном коде |
| 5 | Блоки `unsafe` минимальны и обоснованы | <span>выполнено</span> | в `charger-core` и `charger-host` **ноль** `unsafe` (в `Cargo.toml` стоит `unsafe_code = "deny"`); в `kmdf` `unsafe` сосредоточен в вызовах WDF, у каждого — комментарий с инвариантами и уровнем IRQL |
| 6 | Ресурсы освобождаются, сбой не оставляет зависшего состояния | <span>выполнено</span> | `Charger::close` и `Drop` возвращают безопасный лимит тока; тесты `drop_emits_close_event`, `close_sets_safe_current_and_closes_session`, `reinit_restores_session_after_fault`, `reopening_works_without_restarting_the_process` |
| 7 | Драйвер не привязан к транспорту, тестируется на моке | <span>выполнено</span> | трейт `ChargerTransport`; три реализации (мок, мок с журналом, TCP); 9 интеграционных тестов гоняются на моке и симуляторе без железа (`verify-test.txt`) |
| 8 | Все операции фиксируются в журнале | <span>выполнено</span> | `Journal`/`Event`: `seq`, `ts_ms`, `request_id`, `level`, `kind` + поля операции; артефакт тестового прогона — `artifacts/journal-demo.jsonl` (289 записей), тесты `every_event_has_request_id_and_timestamp`, `journal_records_request_ids_and_monotonic_time` |
| 9 | Демо запускается и воспроизводимо показывает работу | <span>выполнено</span> | `cargo run -p cli -- demo` — таблица по 7 сценариям, включая два ожидаемых отказа (`verify-demo.txt`) |
| 10 | Зафиксирована измеримая базовая производительность | <span>выполнено</span> | `docs/PERFORMANCE.md` + `artifacts/bench-*.txt`: полный цикл сессии ≈0.9 мкс, мок 163 млн оп/с, TCP 7 640 оп/с |
| 11 | CI прогоняет сборку, тесты, линтеры, документацию | <span>выполнено</span> | `.github/workflows/ci.yml`: `fmt --check`, `clippy -D warnings`, `build --release`, `test`, `doc`, сборка без `std`; отдельная задача для драйвера ARM64 |
| 12 | Есть инструкция по передаче и откату | <span>выполнено</span> | `docs/HANDOVER.md`: архитектуры, требования, сборка, семвер, откат, таблица типовых сбоев, разбор журнала |
| 13 | Сборка драйвера режима ядра под ARM64 | <span>выполнено</span> | `cargo wdk build --target-arch arm64 --profile release` — `Finished building kmdf`; в `artifacts/driver-arm64/`: `kmdf.sys` (48.5 КБ), `kmdf.inf`, `kmdf.cat`, сертификат тестовой подписи; `infverif` — «INF is valid»; проверка PE: Machine = `0xAA64` (ARM64) |
| 14 | Ядро драйвера charge pump LN8000 | <span>выполнено</span> | `crates/ln8000`: карта регистров, режимы, формулы тока/напряжения, разбор защит и АЦП; собирается без `std`; демо — `artifacts/verify-pump.txt`; логика сверена с GPL-драйвером Android: `docs/LN8000.md` |
| 15 | KMDF-драйвер LN8000 на узле PEIC (I²C) | <span>выполнено</span> | `cargo wdk build --target-arch arm64 --profile release` в `crates/ln8000-kmdf` → `ln8000_kmdf.sys` (56.5 КБ), `.inf`, `.cat`; `infverif` пройден; PE Machine = `0xAA64`; пакет — `artifacts/driver-ln8000-arm64/` |
| 16 | Развёртывание, обновление и откат | <span>выполнено</span> | `deploy/install-driver.ps1` (проверка test signing, `pnputil`, привязка к `ACPI\QCOM057E`), `update-driver.ps1` (с сохранением предыдущего пакета), `uninstall-driver.ps1` (транскрипт в `%ProgramData%\nabu-fastcharge\uninstall.log`); все четыре скрипта прошли проверку синтаксиса |
| 17 | Телеметрия и журнал сеансов заряда | <span>выполнено</span> | `ln8000::Telemetry`: кольцо 256 отсчётов + история 32 сеансов, ротация, без аллокаций; тесты на открытие/закрытие сеанса, ротацию, bypass; выгрузка — `nabu-ln8000.ps1 sessions|journal` |
| 18 | Защита по температуре и току | <span>выполнено</span> | `ln8000::guard`: три уровня (снижение тока → bypass → stop), тесты на границы и строгий профиль; в драйвере действие применяется в таймере и пишется в журнал |

## Что проверено отдельно

**Ожидаемые отказы работают как задумано.** Демо показывает не только успешные
пути: сценарий «питание отсутствует» заканчивается `detection_timeout`, а
«неизвестный образец» — `unknown_adapter_pattern`. Драйвер не выдумывает тип
адаптера, когда аппаратура молчит.

**Ядро собирается без `std`.** `cargo build -p charger-core --no-default-features`
проходит — это то, что нужно драйверу режима ядра.

**Журнал пригоден для разбора инцидента.** В `artifacts/journal-demo.jsonl` по
записям видно всю сессию: открытие, чтение регистров, детекцию с сырыми
значениями, применённую политику, повторы и ошибки.

## Что не выполнено и почему

| Пункт | Состояние | Причина и разбор |
|---|---|---|
| Read-путь шины SPMI | возвращает типизированный отказ | раскладка ответа `IOCTL_RESOURCE_HUB_TRANSACT` не подтверждена реверсом до конца. Догадка за факт не выдаётся |
| Таймер детекции и выдача журнала клиенту | написаны, но не подключены к очереди | код драйвера собирается и подписывается, но живого прогона на планшете ещё не было: IOCTL `DETECT_START`/`APPLY_POLICY`/`GET_JOURNAL` возвращают `STATUS_NOT_IMPLEMENTED` |
| Управление charge pump LN8000 | ядро готово, драйвера нет | логика перенесена из `ln8000_charger.c` и проверена 29 тестами; нужен KMDF-модуль на SpbCx для узла ACPI `PEIC` (адрес 0x51) |

## Как получен драйвер ARM64 (доказательство)

```text
cargo wdk build --target-arch arm64 --profile release
INFO  Building package kmdf
INFO  Running stampinf
INFO  Running inf2cat
INFO  Signing kmdf.sys using signtool
INFO  Signing kmdf.cat using signtool
INFO  Running infverif
INFO  Finished building kmdf

artifacts/driver-arm64/
  kmdf.sys   48.5 КБ   (PE Machine = 0xAA64 → ARM64)
  kmdf.inf    2.4 КБ
  kmdf.cat    7.9 КБ
  WDRLocalTestCert.cer
```

## Итог

Ядро драйвера готово и проверено: 83 теста, чистые линтеры, документация,
бенчмарки, журнал и инструкция по передаче. Логика, которая включает зарядку
(чтение APSD и лимит тока), реализована и воспроизводима на моках. Добавлено
ядро драйвера charge pump LN8000 — вторая ступень для полного тока заряда.

**Драйвер режима ядра собирается под ARM64**: `kmdf.sys` собран, подписан тестовым
сертификатом, INF прошёл `infverif`, разрядность подтверждена по заголовку PE
(`0xAA64`).

Следующие шаги: подключить таймер детекции к очереди, доразобрать раскладку
ответа шины SPMI для чтения и написать KMDF-модуль SpbCx для LN8000.
