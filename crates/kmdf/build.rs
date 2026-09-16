//! Сборочный скрипт драйвера: настраивает сборку WDK-библиотеки.
//!
//! Возвращаем `wdk_build::ConfigError` напрямую: в `wdk-build` 0.5.1 это
//! собственный тип ошибки, и оборачивать его в `anyhow` не нужно.

fn main() -> Result<(), wdk_build::ConfigError> {
    wdk_build::configure_wdk_binary_build()
}
