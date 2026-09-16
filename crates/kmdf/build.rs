//! Сборочный скрипт драйвера: передаёт конфигурацию WDK в `wdk-build`.

fn main() -> anyhow::Result<()> {
    wdk_build::configure_wdk_binary_build()
}
