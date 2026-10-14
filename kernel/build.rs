//! Select the per-arch linker script: `src/arch/<arch>/linker.ld`.

use std::env;
use std::path::PathBuf;

fn main() {
    let arch = env::var("CARGO_CFG_TARGET_ARCH").expect("CARGO_CFG_TARGET_ARCH");
    let os = env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    let dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap());
    println!("cargo:rerun-if-changed=build.rs");

    // Only bare-metal builds get the kernel link script.
    if os != "none" {
        return;
    }
    let script = dir.join("src").join("arch").join(&arch).join("linker.ld");
    println!("cargo:rerun-if-changed={}", script.display());
    if !script.exists() {
        panic!(
            "no linker script for target arch {arch}: {}",
            script.display()
        );
    }
    println!("cargo:rustc-link-arg-bins=-T{}", script.display());
    println!("cargo:rustc-link-arg-bins=-zmax-page-size=4096");
    println!("cargo:rustc-link-arg-bins=--no-pie");
}
