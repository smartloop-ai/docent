//! Link options for the packaged layout, matching the smartloop crate's own
//! for its `slp` binary: release archives ship the Vulkan loader in `lib/`
//! next to the executable, so end users need only their GPU driver.

fn main() {
    match std::env::var("CARGO_CFG_TARGET_OS").as_deref() {
        // Found through `$ORIGIN/lib` first; without the folder (cargo
        // install) the loader falls back to the system library paths.
        Ok("linux") => println!("cargo:rustc-link-arg-bins=-Wl,-rpath,$ORIGIN/lib"),
        // Room in the Mach-O header for install_name_tool, should a build
        // with the vulkan feature bundle MoltenVK.
        Ok("macos") => println!("cargo:rustc-link-arg-bins=-Wl,-headerpad_max_install_names"),
        _ => {}
    }
}
