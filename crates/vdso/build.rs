//! Build the vDSO image for both arches (decision D75).
//!
//! `image/vdso.rs` is compiled on its own with the pinned toolchain's
//! `rustc` (`--emit=obj`, so nothing from `core` is linked in: any call
//! into a library function is an undefined symbol and fails the link) and
//! linked by the toolchain's `rust-lld` into a shared object. No system
//! compiler or linker is involved. The images land in `OUT_DIR` as
//! `vdso-<arch>.so` and are embedded by `src/lib.rs`.

use std::env;
use std::path::{Path, PathBuf};
use std::process::Command;

const ARCHES: [&str; 2] = ["x86_64", "aarch64"];

fn run(cmd: &mut Command) {
    let status = cmd
        .status()
        .unwrap_or_else(|e| panic!("cannot run {:?}: {e}", cmd.get_program()));
    assert!(status.success(), "{cmd:?} failed: {status}");
}

fn main() {
    let dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap());
    let out = PathBuf::from(env::var("OUT_DIR").unwrap());
    let rustc = env::var("RUSTC").unwrap_or_else(|_| "rustc".into());
    let host = env::var("HOST").unwrap();
    let image = dir.join("image");
    for f in [
        "vdso.rs",
        "chacha.rs",
        "vdso.lds",
        "x86_64.map",
        "aarch64.map",
    ] {
        println!("cargo:rerun-if-changed={}", image.join(f).display());
    }
    println!(
        "cargo:rerun-if-changed={}",
        dir.join("src/vvar.rs").display()
    );
    println!("cargo:rerun-if-changed=build.rs");

    let sysroot = Command::new(&rustc)
        .args(["--print", "sysroot"])
        .output()
        .expect("rustc --print sysroot");
    let sysroot = String::from_utf8(sysroot.stdout).unwrap();
    let lld = Path::new(sysroot.trim())
        .join("lib/rustlib")
        .join(&host)
        .join("bin/rust-lld");
    assert!(
        lld.exists(),
        "rust-lld not found at {} (the toolchain ships it with rust-std)",
        lld.display()
    );

    for arch in ARCHES {
        let obj = out.join(format!("vdso-{arch}.o"));
        let so = out.join(format!("vdso-{arch}.so"));
        // Plain rustc: no RUSTFLAGS/RUSTC_WRAPPER (clippy) from the outer
        // build, which target the kernel or the host.
        run(Command::new(&rustc)
            .env_remove("RUSTC_WORKSPACE_WRAPPER")
            .env_remove("RUSTC_WRAPPER")
            .args([
                "--edition=2024",
                "--crate-type=lib",
                "--crate-name=patina_vdso_image",
                "--emit=obj",
                "-Copt-level=s",
                "-Cpanic=abort",
                "-Crelocation-model=pic",
                "-Ccodegen-units=1",
                "-Coverflow-checks=off",
                "-Cdebug-assertions=off",
                "-Cforce-unwind-tables=no",
                "-Cdebuginfo=0",
                "-Dwarnings",
            ])
            .arg(format!("--target={arch}-unknown-none"))
            .arg("-o")
            .arg(&obj)
            .arg(image.join("vdso.rs")));
        run(Command::new(&lld)
            .args([
                "-flavor",
                "gnu",
                "-shared",
                "-Bsymbolic",
                "--hash-style=sysv",
                "-z",
                "max-page-size=4096",
                "-z",
                "noexecstack",
                "-z",
                "norelro",
                "-z",
                "rodynamic",
                "--no-undefined",
                "--build-id=none",
                "-soname",
                "linux-vdso.so.1",
            ])
            .arg("-T")
            .arg(image.join("vdso.lds"))
            .arg("--version-script")
            .arg(image.join(format!("{arch}.map")))
            .arg("-o")
            .arg(&so)
            .arg(&obj));
    }
}
