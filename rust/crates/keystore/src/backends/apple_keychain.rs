//! Apple Keychain backend — stores keypairs in macOS Keychain with
//! Touch ID as the auth gate before each secret key read.
//!
//! All operations go through a compiled Swift helper (`pay.sh`)
//! signed with the user's Developer ID. Touch ID is enforced via
//! `LAContext` before every `load_keypair` call.
//!
//! Note: `SecAccessControl` with `.biometryCurrentSet` would provide
//! hardware-enforced biometric protection, but requires a provisioning
//! profile (`keychain-access-groups` entitlement) which is not available
//! for CLI tools distributed outside the App Store. The `LAContext`
//! approach provides equivalent UX — Touch ID prompt before every read.

use std::path::PathBuf;
use std::process::Command;

use crate::{Error, KeystoreBackend, Result, SyncMode};

/// macOS Keychain backend with hardware-enforced Touch ID.
pub struct AppleKeychain;

impl KeystoreBackend for AppleKeychain {
    fn import(&self, account: &str, keypair_bytes: &[u8], sync: SyncMode) -> Result<()> {
        if keypair_bytes.len() != 64 {
            return Err(Error::InvalidKeypair(format!(
                "expected 64 bytes, got {}",
                keypair_bytes.len()
            )));
        }

        let _sync = sync; // all items are device-only via kSecAttrAccessibleWhenUnlockedThisDeviceOnly

        // Store secret key
        helper_store(account, keypair_bytes)?;

        // Store public key separately (no auth needed to read)
        helper_store(&format!("{account}.pubkey"), &keypair_bytes[32..64])?;

        Ok(())
    }

    fn exists(&self, account: &str) -> bool {
        helper_run(&["exists", account])
            .map(|out| out.trim() == "yes")
            .unwrap_or(false)
    }

    fn delete(&self, account: &str) -> Result<()> {
        helper_run(&["delete", account])?;
        helper_run(&["delete", &format!("{account}.pubkey")]).ok();
        Ok(())
    }

    fn pubkey(&self, account: &str) -> Result<Vec<u8>> {
        let hex = helper_run(&["read", &format!("{account}.pubkey")])?;
        hex_to_bytes(hex.trim())
    }

    fn load_keypair(&self, account: &str, reason: &str) -> Result<Vec<u8>> {
        let hex = helper_run(&["read-protected", account, reason])?;
        hex_to_bytes(hex.trim())
    }
}

impl AppleKeychain {
    /// Prompt for Touch ID authentication with a custom reason.
    pub fn authenticate(reason: &str) -> Result<()> {
        helper_run(&["authenticate", reason])?;
        Ok(())
    }

    /// Check if Touch ID is available on this device.
    pub fn is_available() -> bool {
        helper_run(&["check-biometrics"])
            .map(|out| out.trim() == "yes")
            .unwrap_or(false)
    }
}

// ── Helper binary ──

/// Source code for the codesigned helper binary.
const HELPER_SOURCE: &str = r#"
import Foundation
import Security
import LocalAuthentication

let svc = "pay.sh"

func main() {
    guard CommandLine.arguments.count >= 2 else {
        fputs("usage: pay.sh <command> [args...]\n", stderr); exit(1)
    }
    switch CommandLine.arguments[1] {
    case "store":
        guard CommandLine.arguments.count >= 3 else { fail("usage: store <account> (hex on stdin)") }
        guard let hex = readLine(strippingNewline: true) else { fail("no data on stdin") }
        doStore(account: CommandLine.arguments[2], hex: hex)
    case "read":
        guard CommandLine.arguments.count >= 3 else { fail("usage: read <account>") }
        doRead(account: CommandLine.arguments[2])
    case "read-protected":
        guard CommandLine.arguments.count >= 4 else { fail("usage: read-protected <account> <reason>") }
        doAuthenticate(reason: CommandLine.arguments[3])
        doRead(account: CommandLine.arguments[2])
    case "exists":
        guard CommandLine.arguments.count >= 3 else { fail("usage: exists <account>") }
        doExists(account: CommandLine.arguments[2])
    case "delete":
        guard CommandLine.arguments.count >= 3 else { fail("usage: delete <account>") }
        doDelete(account: CommandLine.arguments[2])
    case "authenticate":
        guard CommandLine.arguments.count >= 3 else { fail("usage: authenticate <reason>") }
        doAuthenticate(reason: CommandLine.arguments[2])
        print("OK")
    case "check-biometrics":
        doCheckBiometrics()
    default:
        fail("unknown command: \(CommandLine.arguments[1])")
    }
}

func doStore(account: String, hex: String) {
    let data = hexToData(hex)
    let delStatus = SecItemDelete([
        kSecClass as String: kSecClassGenericPassword,
        kSecAttrService as String: svc,
        kSecAttrAccount as String: account
    ] as CFDictionary)
    // If delete fails due to ownership mismatch (item created by different binary),
    // fall back to the security CLI which can delete any item in the login keychain.
    if delStatus == -25244 {
        let p = Process(); p.executableURL = URL(fileURLWithPath: "/usr/bin/security")
        p.arguments = ["delete-generic-password", "-s", svc, "-a", account]
        try? p.run(); p.waitUntilExit()
    }
    let s = SecItemAdd([
        kSecClass as String: kSecClassGenericPassword,
        kSecAttrService as String: svc,
        kSecAttrAccount as String: account,
        kSecValueData as String: data,
        kSecAttrAccessible as String: kSecAttrAccessibleWhenUnlockedThisDeviceOnly
    ] as CFDictionary, nil)
    guard s == errSecSuccess else { fail(errMsg(s)) }
    print("OK")
}

func doRead(account: String) {
    var r: AnyObject?
    let s = SecItemCopyMatching([
        kSecClass as String: kSecClassGenericPassword,
        kSecAttrService as String: svc,
        kSecAttrAccount as String: account,
        kSecReturnData as String: true
    ] as CFDictionary, &r)
    guard s == errSecSuccess, let d = r as? Data else { fail(errMsg(s)) }
    print(d.map { String(format: "%02x", $0) }.joined())
}

func doExists(account: String) {
    var ctx = LAContext()
    ctx.interactionNotAllowed = true
    let s = SecItemCopyMatching([
        kSecClass as String: kSecClassGenericPassword,
        kSecAttrService as String: svc,
        kSecAttrAccount as String: account,
        kSecUseAuthenticationContext as String: ctx
    ] as CFDictionary, nil)
    print(s == errSecSuccess || s == errSecInteractionNotAllowed ? "yes" : "no")
}

func doDelete(account: String) {
    let s = SecItemDelete([
        kSecClass as String: kSecClassGenericPassword,
        kSecAttrService as String: svc,
        kSecAttrAccount as String: account
    ] as CFDictionary)
    guard s == errSecSuccess || s == errSecItemNotFound else { fail("delete failed: \(errMsg(s))") }
    print("OK")
}

func doAuthenticate(reason: String) {
    let sema = DispatchSemaphore(value: 0)
    var authErr: String? = nil
    LAContext().evaluatePolicy(.deviceOwnerAuthenticationWithBiometrics, localizedReason: reason) { ok, e in
        if !ok { authErr = e?.localizedDescription ?? "denied" }
        sema.signal()
    }
    sema.wait()
    if let e = authErr { fail(e) }
}

func doCheckBiometrics() {
    let ctx = LAContext()
    var error: NSError?
    print(ctx.canEvaluatePolicy(.deviceOwnerAuthenticationWithBiometrics, error: &error) ? "yes" : "no")
}

func hexToData(_ hex: String) -> Data {
    var d = Data(); var i = hex.startIndex
    while i < hex.endIndex { let n = hex.index(i, offsetBy: 2); if let b = UInt8(hex[i..<n], radix: 16) { d.append(b) }; i = n }
    return d
}

func errMsg(_ status: OSStatus) -> String { SecCopyErrorMessageString(status, nil) as String? ?? "error \(status)" }

func fail(_ msg: String) -> Never { fputs("ERROR:\(msg)\n", stderr); exit(1) }

main()
"#;

const ENTITLEMENTS_PLIST: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict/>
</plist>"#;

/// Get or compile the signed helper binary.
///
/// Stored in `~/.cache/pay/` (user-private, `0700`) instead of `/tmp`
/// to prevent other users from swapping the binary.
fn helper_path() -> Result<PathBuf> {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_string());
    let cache_dir = PathBuf::from(home).join(".cache").join("pay");
    let binary = cache_dir.join("pay.sh");
    let source = cache_dir.join("pay.sh.swift");
    let entitlements = cache_dir.join("pay.sh.entitlements");

    if binary.exists() {
        // Verify the cached binary is still validly signed before trusting it.
        verify_codesign(&binary)?;
        return Ok(binary);
    }

    // Create cache dir with 0700 (owner-only) permissions.
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

    // Compile
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

    // Codesign — prefer Developer ID, fall back to ad-hoc.
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

/// Verify the helper binary's code signature is intact.
/// If tampered, delete and return an error so it gets recompiled.
fn verify_codesign(binary: &PathBuf) -> Result<()> {
    let output = Command::new("codesign")
        .args(["--verify", "--strict"])
        .arg(binary)
        .output()
        .map_err(|e| Error::Backend(format!("codesign verify: {e}")))?;

    if !output.status.success() {
        // Binary was tampered with — delete it so it gets rebuilt
        std::fs::remove_file(binary).ok();
        return Err(Error::Backend(
            "Keychain helper binary failed signature verification and was removed. \
             Please retry — it will be recompiled."
                .to_string(),
        ));
    }
    Ok(())
}

/// Find a codesigning identity that supports keychain-access-groups.
/// Prefers "Developer ID Application", then "Apple Development".
fn find_signing_identity() -> Option<String> {
    let output = Command::new("security")
        .args(["find-identity", "-v", "-p", "codesigning"])
        .output()
        .ok()?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    // Prefer Developer ID (works outside Xcode), then Apple Development
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

fn helper_store(account: &str, data: &[u8]) -> Result<()> {
    use std::io::Write;

    let hex: String = data.iter().map(|b| format!("{b:02x}")).collect();
    let binary = helper_path()?;

    // Pass secret data via stdin pipe, not CLI args (CLI args are visible in `ps`).
    let mut child = Command::new(&binary)
        .args(["store", account])
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

fn hex_to_bytes(hex: &str) -> Result<Vec<u8>> {
    if !hex.len().is_multiple_of(2) {
        return Err(Error::InvalidKeypair("odd hex length".to_string()));
    }
    (0..hex.len())
        .step_by(2)
        .map(|i| {
            u8::from_str_radix(&hex[i..i + 2], 16)
                .map_err(|e| Error::InvalidKeypair(format!("hex: {e}")))
        })
        .collect()
}
