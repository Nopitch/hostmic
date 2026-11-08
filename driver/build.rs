//! Configures Cargo for a WDK driver binary (include paths, linker flags), from the WDK
//! configuration of the workspace.

fn main() -> Result<(), wdk_build::ConfigError> {
    wdk_build::configure_wdk_binary_build()
}
