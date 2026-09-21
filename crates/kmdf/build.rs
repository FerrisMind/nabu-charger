//! Driver build script: configures the WDK library build.
//!
//! We return `wdk_build::ConfigError` directly: in `wdk-build` 0.5.1 it is a
//! dedicated error type, and there is no need to wrap it in `anyhow`.

fn main() -> Result<(), wdk_build::ConfigError> {
    wdk_build::configure_wdk_binary_build()
}
