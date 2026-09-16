//! Сборочный скрипт драйвера LN8000: настраивает сборку WDK-библиотеки.

fn main() -> Result<(), wdk_build::ConfigError> {
    wdk_build::configure_wdk_binary_build()
}
