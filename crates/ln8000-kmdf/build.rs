//! LN8000 driver build script: configures the WDK library build.

fn main() -> Result<(), wdk_build::ConfigError> {
    wdk_build::configure_wdk_binary_build()?;

    // Battery class miniport + WMI helpers (Settings / Win32_Battery path).
    let wdk_root = std::env::var("WDKContentRoot").unwrap_or_else(|_| {
        r"C:\Program Files (x86)\Windows Kits\10".to_string()
    });
    let lib = format!(r"{wdk_root}\Lib\10.0.26100.0\km\arm64");
    println!("cargo:rustc-link-search=native={lib}");
    println!("cargo:rustc-link-lib=battc");
    println!("cargo:rustc-link-lib=wmilib");
    Ok(())
}
