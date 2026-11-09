//! Configures Cargo for a WDK driver binary (include paths, linker flags), from the WDK
//! configuration of the workspace.

fn main() -> Result<(), wdk_build::ConfigError> {
    // `PsSetCreateProcessNotifyRoutineEx` refuses images without the force-integrity flag
    // (`IMAGE_DLLCHARACTERISTICS_FORCE_INTEGRITY`); the driver is signed anyway.
    println!("cargo:rustc-cdylib-link-arg=/INTEGRITYCHECK");
    wdk_build::configure_wdk_binary_build()
}
