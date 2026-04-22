//! Build script: pre-compile the macOS Swift helper so users don't need swiftc.
//!
//! The compiled binary is written to OUT_DIR as `pay-helper` and embedded via
//! `include_bytes!` in the main crate. On non-macOS or when swiftc is
//! unavailable, we write an empty sentinel so `include_bytes!` still compiles.

fn main() {
    let out_dir = std::path::PathBuf::from(std::env::var("OUT_DIR").unwrap());
    let marker = out_dir.join("pay-helper");

    println!("cargo::rerun-if-changed=src/macos/helper.swift");

    #[cfg(target_os = "macos")]
    {
        let source = std::path::PathBuf::from("src/macos/helper.swift");

        let status = std::process::Command::new("swiftc")
            .args(["-O", "-o"])
            .arg(&marker)
            .arg(&source)
            .status();

        match status {
            Ok(s) if s.success() => {}
            Ok(s) => {
                println!(
                    "cargo::warning=swiftc failed (exit {s}), helper will be compiled at runtime"
                );
                std::fs::write(&marker, b"").ok();
            }
            Err(e) => {
                println!(
                    "cargo::warning=swiftc not found ({e}), helper will be compiled at runtime"
                );
                std::fs::write(&marker, b"").ok();
            }
        }
    }

    #[cfg(not(target_os = "macos"))]
    {
        std::fs::write(&marker, b"").ok();
    }
}
