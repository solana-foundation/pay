//! macOS: Touch ID authentication + Apple Keychain storage.

use crate::{AuthGate, Error, Result, SecretStore, Zeroizing};
use std::path::PathBuf;
use std::process::Command;

const HELPER_SOURCE: &str = include_str!("helper.swift");

const ENTITLEMENTS_PLIST: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict/>
</plist>"#;

// ── Touch ID auth gate ──────────────────────────────────────────────────────

pub struct TouchId;

impl AuthGate for TouchId {
    fn authenticate(&self, reason: &str) -> Result<()> {
        let binary = helper_path()?;
        let output = Command::new(&binary)
            .args(["authenticate", reason])
            .output()
            .map_err(|e| Error::Backend(format!("pay.sh: {e}")))?;

        if output.status.success() {
            Ok(())
        } else {
            let err = extract_error(&output.stderr);
            if err == "denied" {
                Err(Error::AuthDenied(err))
            } else {
                Err(Error::Backend(err))
            }
        }
    }

    fn is_available(&self) -> bool {
        helper_path()
            .ok()
            .and_then(|binary| {
                Command::new(&binary)
                    .args(["check-biometrics"])
                    .output()
                    .ok()
            })
            .map(|out| String::from_utf8_lossy(&out.stdout).trim() == "yes")
            .unwrap_or(false)
    }
}

// ── Apple Keychain store ────────────────────────────────────────────────────

pub struct AppleKeychainStore;

impl SecretStore for AppleKeychainStore {
    fn store(&self, key: &str, data: &[u8]) -> Result<()> {
        helper_store(key, data)
    }

    fn load(&self, key: &str) -> Result<Zeroizing<Vec<u8>>> {
        let hex = helper_run(&["read", key])?;
        crate::store::hex_decode(hex.trim()).map(Zeroizing::new)
    }

    fn exists(&self, key: &str) -> bool {
        helper_run(&["exists", key])
            .map(|out| out.trim() == "yes")
            .unwrap_or(false)
    }

    fn delete(&self, key: &str) -> Result<()> {
        helper_run(&["delete", key])?;
        Ok(())
    }
}

// ── Swift helper management ─────────────────────────────────────────────────

fn helper_path() -> Result<PathBuf> {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_string());
    let cache_dir = PathBuf::from(home).join(".cache").join("pay");
    let binary = cache_dir.join("pay.sh");
    let source = cache_dir.join("pay.sh.swift");
    let entitlements = cache_dir.join("pay.sh.entitlements");

    if binary.exists() {
        verify_codesign(&binary)?;
        return Ok(binary);
    }

    std::fs::create_dir_all(&cache_dir)
        .map_err(|e| Error::Backend(format!("Failed to create cache dir: {e}")))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&cache_dir, std::fs::Permissions::from_mode(0o700)).ok();
    }

    std::fs::write(&source, HELPER_SOURCE)
        .map_err(|e| Error::Backend(format!("Failed to write helper source: {e}")))?;
    std::fs::write(&entitlements, ENTITLEMENTS_PLIST)
        .map_err(|e| Error::Backend(format!("Failed to write entitlements: {e}")))?;

    let compile = Command::new("swiftc")
        .args(["-O", "-o"])
        .arg(&binary)
        .arg(&source)
        .output()
        .map_err(|e| Error::Backend(format!("swiftc: {e}")))?;

    if !compile.status.success() {
        let stderr = String::from_utf8_lossy(&compile.stderr);
        return Err(Error::Backend(format!("swiftc failed: {stderr}")));
    }

    let identity = find_signing_identity().unwrap_or_else(|| "-".to_string());
    let sign = Command::new("codesign")
        .args(["-s", &identity, "-f", "--entitlements"])
        .arg(&entitlements)
        .arg(&binary)
        .output()
        .map_err(|e| Error::Backend(format!("codesign: {e}")))?;

    if !sign.status.success() {
        let stderr = String::from_utf8_lossy(&sign.stderr);
        return Err(Error::Backend(format!("codesign failed: {stderr}")));
    }

    Ok(binary)
}

fn verify_codesign(binary: &PathBuf) -> Result<()> {
    let output = Command::new("codesign")
        .args(["--verify", "--strict"])
        .arg(binary)
        .output()
        .map_err(|e| Error::Backend(format!("codesign verify: {e}")))?;

    if !output.status.success() {
        std::fs::remove_file(binary).ok();
        return Err(Error::Backend(
            "Keychain helper binary failed signature verification and was removed. \
             Please retry — it will be recompiled."
                .to_string(),
        ));
    }
    Ok(())
}

fn find_signing_identity() -> Option<String> {
    let output = Command::new("security")
        .args(["find-identity", "-v", "-p", "codesigning"])
        .output()
        .ok()?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    for prefix in ["Developer ID Application", "Apple Development"] {
        for line in stdout.lines() {
            if let Some(start) = line.find('"')
                && let Some(end) = line[start + 1..].find('"')
            {
                let name = &line[start + 1..start + 1 + end];
                if name.starts_with(prefix) {
                    return Some(name.to_string());
                }
            }
        }
    }
    None
}

fn helper_run(args: &[&str]) -> Result<String> {
    let binary = helper_path()?;
    let output = Command::new(&binary)
        .args(args)
        .output()
        .map_err(|e| Error::Backend(format!("pay.sh: {e}")))?;

    if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stdout).to_string())
    } else {
        let err = extract_error(&output.stderr);
        if err == "denied" {
            Err(Error::AuthDenied(err))
        } else {
            Err(Error::Backend(err))
        }
    }
}

fn helper_store(key: &str, data: &[u8]) -> Result<()> {
    use std::io::Write;

    let hex = crate::store::hex_encode(data);
    let binary = helper_path()?;

    let mut child = Command::new(&binary)
        .args(["store", key])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|e| Error::Backend(format!("pay.sh: {e}")))?;

    if let Some(mut stdin) = child.stdin.take() {
        stdin
            .write_all(hex.as_bytes())
            .map_err(|e| Error::Backend(format!("stdin write: {e}")))?;
        stdin
            .write_all(b"\n")
            .map_err(|e| Error::Backend(format!("stdin write: {e}")))?;
    }

    let output = child
        .wait_with_output()
        .map_err(|e| Error::Backend(format!("pay.sh: {e}")))?;

    if !output.status.success() {
        return Err(Error::Backend(extract_error(&output.stderr)));
    }
    Ok(())
}

fn extract_error(stderr: &[u8]) -> String {
    let s = String::from_utf8_lossy(stderr);
    s.lines()
        .find(|l| l.starts_with("ERROR:"))
        .map(|l| l.strip_prefix("ERROR:").unwrap_or("unknown").to_string())
        .unwrap_or_else(|| s.trim().to_string())
}
