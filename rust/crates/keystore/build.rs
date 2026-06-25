//! Build script: pre-compile the macOS Swift helper so users don't need swiftc.
//!
//! The compiled binary is written to OUT_DIR as `pay-helper` and embedded via
//! `include_bytes!` in the main crate. On non-macOS or when swiftc is
//! unavailable, we write an empty sentinel so `include_bytes!` still compiles.

#[cfg(target_os = "macos")]
const CODESIGN: &str = "/usr/bin/codesign";

#[cfg(target_os = "macos")]
const SWIFTC: &str = "/usr/bin/swiftc";

#[cfg(target_os = "macos")]
const ENTITLEMENTS_PLIST: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict/>
</plist>"#;

fn main() {
    let out_dir = std::path::PathBuf::from(std::env::var("OUT_DIR").unwrap());
    let marker = out_dir.join("pay-helper");

    println!("cargo::rerun-if-changed=src/macos/helper.swift");

    #[cfg(target_os = "macos")]
    {
        let source = std::path::PathBuf::from("src/macos/helper.swift");
        let entitlements = out_dir.join("pay-helper.entitlements");

        if let Err(e) = std::fs::write(&entitlements, ENTITLEMENTS_PLIST) {
            println!("cargo::warning=failed to write helper entitlements ({e})");
            std::fs::write(&marker, b"").ok();
            return;
        }

        // Match the helper's CPU arch to the crate's target arch — otherwise
        // an arm64 build host produces an arm64 helper even when cross-compiling
        // for x86_64-apple-darwin, which fails with EBADARCH at runtime on Intel.
        let target_arch =
            std::env::var("CARGO_CFG_TARGET_ARCH").unwrap_or_else(|_| "x86_64".to_string());
        let swift_target = format!("{target_arch}-apple-macos11");

        let status = std::process::Command::new(SWIFTC)
            .args(["-O", "-target", &swift_target, "-o"])
            .arg(&marker)
            .arg(&source)
            .status();

        match status {
            Ok(s) if s.success() => {
                let sign = std::process::Command::new(CODESIGN)
                    .args(["-s", "-", "-f", "--entitlements"])
                    .arg(&entitlements)
                    .arg(&marker)
                    .status();

                match sign {
                    Ok(s) if s.success() => {}
                    Ok(s) => {
                        println!(
                            "cargo::warning=codesign failed (exit {s}), helper will be compiled at runtime"
                        );
                        std::fs::write(&marker, b"").ok();
                    }
                    Err(e) => {
                        println!(
                            "cargo::warning=codesign not found ({e}), helper will be compiled at runtime"
                        );
                        std::fs::write(&marker, b"").ok();
                    }
                }
            }
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
