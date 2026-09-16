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
| 3 | Все тесты проходят, включая ошибочные сценарии | <span>выполнено</span> | 52 теста: 37 unit в ядре, 9 интеграционных, 4 CLI, doctests; среди них таймаут детекции, обрыв связи, неизвестный образец, отказ проверки записи, повторная инициализация (`verify-test.txt`) |
| 4 | Ошибки типизированы, штатный путь не паникует | <span>выполнено</span> | `ChargerError`/`TransportError` реализуют `core::error::Error`; линты `clippy::unwrap_used`, `expect_used`, `panic`, `indexing_slicing` включены как `deny` в `Cargo.toml` — сборка проходит, значит их нет в библиотечном коде |
| 5 | Блоки `unsafe` минимальны и обоснованы | <span>выполнено</span> | в `charger-core` и `charger-host` **ноль** `unsafe` (в `Cargo.toml` стоит `unsafe_code = "deny"`); в `kmdf` `unsafe` сосредоточен в вызовах WDF, у каждого — комментарий с инвариантами и уровнем IRQL |
| 6 | Ресурсы освобождаются, сбой не оставляет зависшего состояния | <span>выполнено</span> | `Charger::close` и `Drop` возвращают безопасный лимит тока; тесты `drop_emits_close_event`, `close_sets_safe_current_and_closes_session`, `reinit_restores_session_after_fault`, `reopening_works_without_restarting_the_process` |
| 7 | Драйвер не привязан к транспорту, тестируется на моке | <span>выполнено</span> | трейт `ChargerTransport`; три реализации (мок, мок с журналом, TCP); 9 интеграционных тестов гоняются на моке и симуляторе без железа (`verify-test.txt`) |
| 8 | Все операции фиксируются в журнале | <span>выполнено</span> | `Journal`/`Event`: `seq`, `ts_ms`, `request_id`, `level`, `kind` + поля операции; артефакт тестового прогона — `artifacts/journal-demo.jsonl` (289 записей), тесты `every_event_has_request_id_and_timestamp`, `journal_records_request_ids_and_monotonic_time` |
| 9 | Демо запускается и воспроизводимо показывает работу | <span>выполнено</span> | `cargo run -p cli -- demo` — таблица по 7 сценариям, включая два ожидаемых отказа (`verify-demo.txt`) |
| 10 | Зафиксирована измеримая базовая производительность | <span>выполнено</span> | `docs/PERFORMANCE.md` + `artifacts/bench-*.txt`: полный цикл сессии ≈0.9 мкс, мок 163 млн оп/с, TCP 7 640 оп/с |
| 11 | CI прогоняет сборку, тесты, линтеры, документацию | <span>выполнено</span> | `.github/workflows/ci.yml`: `fmt --check`, `clippy -D warnings`, `build --release`, `test`, `doc`, сборка без `std`; отдельная задача для драйвера ARM64 |
| 12 | Есть инструкция по передаче и откату | <span>выполнено</span> | `docs/HANDOVER.md`: архитектуры, требования, сборка, семвер, откат, таблица типовых сбоев, разбор журнала |

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
| Сборка драйвера режима ядра (`cargo wdk build`) | остановлено на `wdk-sys` | связка зависимостей подобрана верно (`wdk 0.4.1` + `wdk-sys/wdk-build 0.5.1`), цель — `aarch64-pc-windows-msvc`, но `bindgen` под libclang 23 разбирает заголовки WDF неполно и код не компилируется. Требуется LLVM 17.x. Разбор и команды: `docs/HANDOVER.md`, раздел 6 |
| Read-путь шины SPMI | возвращает типизированный отказ | раскладка ответа `IOCTL_RESOURCE_HUB_TRANSACT` не подтверждена реверсом до конца. Догадка за факт не выдаётся |
| Управление charge pump LN8000 | вне этой версии | нужен отдельный драйвер I2C (0x51); протокол есть в исходниках Android |

## Итог

Ядро драйвера готово и проверено: 52 теста, чистые линтеры, документация,
бенчмарки, журнал и инструкция по передаче. Логика, которая включает зарядку
(чтение APSD и лимит тока), реализована и воспроизводима на моках. Ядровая
обвязка написана и упирается в версию LLVM — это единственный внешний блокер,
и он снимается установкой LLVM 17.x.
