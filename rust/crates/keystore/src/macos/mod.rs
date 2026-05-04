//! macOS: Touch ID authentication + Apple Keychain storage.

use crate::{AuthGate, AuthIntent, Error, Result, SecretStore, Zeroizing};
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

const HELPER_SOURCE: &str = include_str!("helper.swift");
const CODESIGN: &str = "/usr/bin/codesign";
const SWIFTC: &str = "/usr/bin/swiftc";

const ENTITLEMENTS_PLIST: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict/>
</plist>"#;

// ── Touch ID auth gate ──────────────────────────────────────────────────────

pub struct TouchId;

impl AuthGate for TouchId {
    fn authenticate(&self, intent: &AuthIntent) -> Result<()> {
        let binary = helper_path()?;
        let message = intent.prompt_message();
        let output = Command::new(&binary)
            .args(["authenticate", &message])
            .output()
            .map_err(|e| Error::Backend(format!("pay.sh: {e}")))?;

        if output.status.success() {
            Ok(())
        } else {
            let err = extract_error(&output.stderr);
            if is_user_cancel(&err) {
                Err(Error::AuthDenied(err))
            } else {
                Err(Error::Backend(touch_id_unavailable_guidance(&err)))
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

fn touch_id_unavailable_guidance(err: &str) -> String {
    format!(
        "{err}\n\nTouch ID is required to use macOS Keychain with pay. Make sure Touch ID is available and configured on this Mac, then try again."
    )
}

// ── Apple Keychain store ────────────────────────────────────────────────────

pub struct AppleKeychainStore;

impl SecretStore for AppleKeychainStore {
    fn store(&self, key: &str, data: &[u8]) -> Result<()> {
        helper_store(key, data)
    }

    fn load(&self, key: &str) -> Result<Zeroizing<Vec<u8>>> {
        let hex = Zeroizing::new(helper_run(&["read", key])?);
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
//
// The cached helper at `~/.cache/pay/pay.sh` is executable code that receives
// key material on stdin. Existing cache contents are only reused when they
// exactly match the signed helper embedded in this build and have private file
// metadata. Otherwise the helper is replaced atomically from trusted bytes or
// rebuilt from the embedded Swift source before it is executed.

/// Pre-compiled Swift helper binary embedded at build time.
/// Empty if swiftc was not available during `cargo build`.
const EMBEDDED_HELPER: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/pay-helper"));

fn helper_path() -> Result<PathBuf> {
    let home = std::env::var_os("HOME")
        .filter(|home| !home.is_empty())
        .ok_or_else(|| Error::Backend("HOME is required for macOS Keychain helper".to_string()))?;
    let cache_dir = PathBuf::from(home).join(".cache").join("pay");
    let binary = cache_dir.join("pay.sh");
    let entitlements = cache_dir.join("pay.sh.entitlements");

    prepare_cache_dir(&cache_dir)?;
    if cached_helper_is_current(&binary)? {
        return Ok(binary);
    }

    install_helper(&cache_dir, &binary, &entitlements)?;
    Ok(binary)
}

fn install_helper(cache_dir: &Path, binary: &Path, entitlements: &Path) -> Result<()> {
    if !EMBEDDED_HELPER.is_empty() {
        write_file_atomically(cache_dir, binary, EMBEDDED_HELPER, 0o700)?;
    } else {
        compile_helper_atomically(cache_dir, binary, entitlements)?;
    }

    validate_helper_file(binary)?;
    verify_codesign(binary)?;
    Ok(())
}

fn compile_helper_atomically(cache_dir: &Path, binary: &Path, entitlements: &Path) -> Result<()> {
    write_file_atomically(
        cache_dir,
        entitlements,
        ENTITLEMENTS_PLIST.as_bytes(),
        0o600,
    )?;

    let source = unused_temp_path(cache_dir, "pay.sh.swift")?;
    let tmp_binary = unused_temp_path(cache_dir, "pay.sh")?;

    let result = (|| {
        write_file_exclusive(&source, HELPER_SOURCE.as_bytes(), 0o600)?;

        let compile = Command::new(SWIFTC)
            .args(["-O", "-o"])
            .arg(&tmp_binary)
            .arg(&source)
            .output()
            .map_err(|e| {
                Error::Backend(format!(
                    "swiftc not found: {e}. Install Xcode Command Line Tools: xcode-select --install"
                ))
            })?;

        if !compile.status.success() {
            let stderr = String::from_utf8_lossy(&compile.stderr);
            return Err(Error::Backend(format!("swiftc failed: {stderr}")));
        }

        fs::set_permissions(&tmp_binary, fs::Permissions::from_mode(0o700))
            .map_err(|e| Error::Backend(format!("Failed to set helper permissions: {e}")))?;
        codesign_binary(&tmp_binary, entitlements)?;
        fs::set_permissions(&tmp_binary, fs::Permissions::from_mode(0o700))
            .map_err(|e| Error::Backend(format!("Failed to set helper permissions: {e}")))?;
        validate_helper_file(&tmp_binary)?;
        fs::rename(&tmp_binary, binary)
            .map_err(|e| Error::Backend(format!("Failed to install helper binary: {e}")))?;
        Ok(())
    })();

    fs::remove_file(&source).ok();
    if result.is_err() {
        fs::remove_file(&tmp_binary).ok();
    }
    result
}

fn prepare_cache_dir(cache_dir: &Path) -> Result<()> {
    match fs::symlink_metadata(cache_dir) {
        Ok(metadata) => {
            if metadata.file_type().is_symlink() {
                return Err(Error::Backend(format!(
                    "Refusing to use symlinked Keychain helper cache dir: {}",
                    cache_dir.display()
                )));
            }
            if !metadata.is_dir() {
                return Err(Error::Backend(format!(
                    "Keychain helper cache path is not a directory: {}",
                    cache_dir.display()
                )));
            }
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            let mut builder = fs::DirBuilder::new();
            builder.recursive(true).mode(0o700);
            builder
                .create(cache_dir)
                .map_err(|e| Error::Backend(format!("Failed to create cache dir: {e}")))?;
        }
        Err(e) => {
            return Err(Error::Backend(format!(
                "Failed to inspect Keychain helper cache dir: {e}"
            )));
        }
    }

    fs::set_permissions(cache_dir, fs::Permissions::from_mode(0o700))
        .map_err(|e| Error::Backend(format!("Failed to set cache dir permissions: {e}")))?;
    validate_cache_dir(cache_dir)
}

fn validate_cache_dir(cache_dir: &Path) -> Result<()> {
    let metadata = fs::symlink_metadata(cache_dir)
        .map_err(|e| Error::Backend(format!("Failed to inspect cache dir: {e}")))?;

    if metadata.file_type().is_symlink() {
        return Err(Error::Backend(format!(
            "Refusing to use symlinked Keychain helper cache dir: {}",
            cache_dir.display()
        )));
    }
    if !metadata.is_dir() {
        return Err(Error::Backend(format!(
            "Keychain helper cache path is not a directory: {}",
            cache_dir.display()
        )));
    }
    if metadata.uid() != current_euid() {
        return Err(Error::Backend(format!(
            "Keychain helper cache dir is not owned by the current user: {}",
            cache_dir.display()
        )));
    }
    if metadata.mode() & 0o077 != 0 {
        return Err(Error::Backend(format!(
            "Keychain helper cache dir has unsafe permissions: {}",
            cache_dir.display()
        )));
    }

    Ok(())
}

fn cached_helper_is_current(binary: &Path) -> Result<bool> {
    match fs::symlink_metadata(binary) {
        Ok(_) => {}
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(e) => {
            return Err(Error::Backend(format!(
                "Failed to inspect Keychain helper binary: {e}"
            )));
        }
    }

    if let Err(e) = validate_helper_file(binary) {
        remove_cached_helper(binary, &e.to_string())?;
        return Ok(false);
    }

    if EMBEDDED_HELPER.is_empty() || !file_equals(binary, EMBEDDED_HELPER)? {
        remove_cached_helper(binary, "helper does not match embedded build artifact")?;
        return Ok(false);
    }

    verify_codesign(binary)?;
    Ok(true)
}

fn validate_helper_file(binary: &Path) -> Result<()> {
    let metadata = fs::symlink_metadata(binary)
        .map_err(|e| Error::Backend(format!("Failed to inspect helper binary: {e}")))?;

    if metadata.file_type().is_symlink() {
        return Err(Error::Backend(format!(
            "Refusing to use symlinked Keychain helper binary: {}",
            binary.display()
        )));
    }
    if !metadata.is_file() {
        return Err(Error::Backend(format!(
            "Keychain helper path is not a regular file: {}",
            binary.display()
        )));
    }
    if metadata.uid() != current_euid() {
        return Err(Error::Backend(format!(
            "Keychain helper binary is not owned by the current user: {}",
            binary.display()
        )));
    }
    if metadata.nlink() != 1 {
        return Err(Error::Backend(format!(
            "Refusing to use hard-linked Keychain helper binary: {}",
            binary.display()
        )));
    }
    if metadata.mode() & 0o077 != 0 {
        return Err(Error::Backend(format!(
            "Keychain helper binary has unsafe permissions: {}",
            binary.display()
        )));
    }

    Ok(())
}

fn remove_cached_helper(binary: &Path, reason: &str) -> Result<()> {
    fs::remove_file(binary).map_err(|e| {
        Error::Backend(format!(
            "Failed to remove untrusted Keychain helper binary ({reason}): {e}"
        ))
    })
}

fn file_equals(path: &Path, expected: &[u8]) -> Result<bool> {
    let actual =
        fs::read(path).map_err(|e| Error::Backend(format!("Failed to read helper binary: {e}")))?;
    Ok(actual == expected)
}

fn write_file_atomically(
    cache_dir: &Path,
    destination: &Path,
    contents: &[u8],
    mode: u32,
) -> Result<()> {
    let tmp = unused_temp_path(
        cache_dir,
        destination
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("pay"),
    )?;

    let result = (|| {
        write_file_exclusive(&tmp, contents, mode)?;
        fs::rename(&tmp, destination)
            .map_err(|e| Error::Backend(format!("Failed to install helper file: {e}")))?;
        Ok(())
    })();

    if result.is_err() {
        fs::remove_file(&tmp).ok();
    }
    result
}

fn write_file_exclusive(path: &Path, contents: &[u8], mode: u32) -> Result<()> {
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(mode)
        .open(path)
        .map_err(|e| Error::Backend(format!("Failed to create helper file: {e}")))?;
    file.write_all(contents)
        .map_err(|e| Error::Backend(format!("Failed to write helper file: {e}")))?;
    file.sync_all()
        .map_err(|e| Error::Backend(format!("Failed to sync helper file: {e}")))?;
    fs::set_permissions(path, fs::Permissions::from_mode(mode))
        .map_err(|e| Error::Backend(format!("Failed to set helper file permissions: {e}")))?;
    Ok(())
}

fn unused_temp_path(cache_dir: &Path, stem: &str) -> Result<PathBuf> {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or_default();
    for attempt in 0..32 {
        let path = cache_dir.join(format!(
            ".{stem}.{}.{}.tmp",
            std::process::id(),
            nanos + attempt
        ));
        match fs::symlink_metadata(&path) {
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(path),
            Err(e) => {
                return Err(Error::Backend(format!(
                    "Failed to inspect helper temp path: {e}"
                )));
            }
            Ok(_) => {}
        }
    }
    Err(Error::Backend(
        "Failed to allocate a helper temp path".to_string(),
    ))
}

fn codesign_binary(binary: &Path, entitlements: &Path) -> Result<()> {
    let sign = Command::new(CODESIGN)
        .args(["-s", "-", "-f", "--entitlements"])
        .arg(entitlements)
        .arg(binary)
        .output()
        .map_err(|e| Error::Backend(format!("codesign: {e}")))?;

    if !sign.status.success() {
        let stderr = String::from_utf8_lossy(&sign.stderr);
        return Err(Error::Backend(format!("codesign failed: {stderr}")));
    }
    Ok(())
}

fn verify_codesign(binary: &Path) -> Result<()> {
    let output = Command::new(CODESIGN)
        .args(["--verify", "--strict"])
        .arg(binary)
        .output()
        .map_err(|e| Error::Backend(format!("codesign verify: {e}")))?;

    if !output.status.success() {
        std::fs::remove_file(binary).ok();
        return Err(Error::Backend(
            "Keychain helper binary failed signature verification and was removed. \
             Please retry; it will be reinstalled."
                .to_string(),
        ));
    }
    Ok(())
}

fn current_euid() -> u32 {
    unsafe extern "C" {
        fn geteuid() -> u32;
    }

    // SAFETY: geteuid has no preconditions and does not write through pointers.
    unsafe { geteuid() }
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
        if is_user_cancel(&err) {
            Err(Error::AuthDenied(err))
        } else {
            Err(Error::Backend(err))
        }
    }
}

/// Detect Apple Keychain / LocalAuthentication "user cancelled" messages.
/// Covers our own "denied" sentinel, Touch ID cancel, and SecItemCopyMatching
/// cancel (errSecUserCanceled → "User canceled the operation.").
fn is_user_cancel(msg: &str) -> bool {
    let m = msg.to_lowercase();
    m == "denied" || m.contains("cancel")
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;

    struct TestDir(PathBuf);

    impl TestDir {
        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TestDir {
        fn drop(&mut self) {
            fs::remove_dir_all(&self.0).ok();
        }
    }

    fn test_dir(name: &str) -> TestDir {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|duration| duration.as_nanos())
            .unwrap_or_default();
        let path = std::env::temp_dir().join(format!(
            "pay-keystore-{name}-{}-{nanos}",
            std::process::id()
        ));
        fs::create_dir(&path).expect("create temp dir");
        TestDir(path)
    }

    #[test]
    fn validate_helper_file_accepts_private_regular_file() {
        let dir = test_dir("regular");
        let helper = dir.path().join("pay.sh");
        fs::write(&helper, b"helper").expect("write helper");
        fs::set_permissions(&helper, fs::Permissions::from_mode(0o700))
            .expect("set helper permissions");

        validate_helper_file(&helper).expect("private regular helper");
    }

    #[test]
    fn validate_helper_file_rejects_symlink() {
        let dir = test_dir("symlink");
        let target = dir.path().join("target");
        let helper = dir.path().join("pay.sh");
        fs::write(&target, b"helper").expect("write target");
        symlink(&target, &helper).expect("create symlink");

        let err = validate_helper_file(&helper).expect_err("symlink rejected");
        assert!(err.to_string().contains("symlinked"));
    }

    #[test]
    fn validate_helper_file_rejects_hard_link() {
        let dir = test_dir("hardlink");
        let helper = dir.path().join("pay.sh");
        let linked = dir.path().join("linked");
        fs::write(&helper, b"helper").expect("write helper");
        fs::set_permissions(&helper, fs::Permissions::from_mode(0o700))
            .expect("set helper permissions");
        fs::hard_link(&helper, &linked).expect("create hard link");

        let err = validate_helper_file(&helper).expect_err("hard link rejected");
        assert!(err.to_string().contains("hard-linked"));
    }

    #[test]
    fn cached_helper_is_current_discards_wrong_helper() {
        let dir = test_dir("wrong-helper");
        let helper = dir.path().join("pay.sh");
        fs::write(&helper, b"not the embedded helper").expect("write helper");
        fs::set_permissions(&helper, fs::Permissions::from_mode(0o700))
            .expect("set helper permissions");

        assert!(!cached_helper_is_current(&helper).expect("helper check"));
        assert!(!helper.exists());
    }

    #[test]
    fn validate_cache_dir_rejects_group_or_other_permissions() {
        let dir = test_dir("dir-perms");
        fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o770))
            .expect("set cache permissions");

        let err = validate_cache_dir(dir.path()).expect_err("unsafe directory rejected");
        assert!(err.to_string().contains("unsafe permissions"));
    }
}
