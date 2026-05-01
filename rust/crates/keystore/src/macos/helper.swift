import Foundation
import Security
import LocalAuthentication

let svc = "pay.sh"
let keypairPrefix = "keypair:"
let pubkeyPrefix = "pubkey:"

func main() {
    guard CommandLine.arguments.count >= 2 else {
        fputs("usage: pay.sh <command> [args...]\n", stderr); exit(1)
    }
    switch CommandLine.arguments[1] {
    case "store-keypair":
        guard CommandLine.arguments.count >= 4 else { fail("usage: store-keypair <account> <reason> (hex on stdin)") }
        guard let hex = readLine(strippingNewline: true) else { fail("no data on stdin") }
        doStoreKeypair(account: CommandLine.arguments[2], reason: CommandLine.arguments[3], hex: hex)
    case "read-keypair":
        guard CommandLine.arguments.count >= 4 else { fail("usage: read-keypair <account> <reason>") }
        doReadKeypair(account: CommandLine.arguments[2], reason: CommandLine.arguments[3])
    case "delete-keypair":
        guard CommandLine.arguments.count >= 4 else { fail("usage: delete-keypair <account> <reason>") }
        doDeleteKeypair(account: CommandLine.arguments[2], reason: CommandLine.arguments[3])
    case "store-pubkey":
        guard CommandLine.arguments.count >= 3 else { fail("usage: store-pubkey <account> (hex on stdin)") }
        guard let hex = readLine(strippingNewline: true) else { fail("no data on stdin") }
        doStorePubkey(account: CommandLine.arguments[2], hex: hex)
    case "read-pubkey":
        guard CommandLine.arguments.count >= 3 else { fail("usage: read-pubkey <account>") }
        doReadPubkey(account: CommandLine.arguments[2])
    case "delete-pubkey":
        guard CommandLine.arguments.count >= 3 else { fail("usage: delete-pubkey <account>") }
        doDeletePubkey(account: CommandLine.arguments[2])
    case "exists":
        guard CommandLine.arguments.count >= 3 else { fail("usage: exists <account>") }
        doExists(account: CommandLine.arguments[2])
    case "authenticate":
        guard CommandLine.arguments.count >= 3 else { fail("usage: authenticate <reason>") }
        doAuthenticate(reason: CommandLine.arguments[2])
        print("OK")
    case "check-biometrics":
        doCheckBiometrics()
    case "store", "read", "read-protected", "delete":
        fail("unsupported private key command")
    default:
        fail("unknown command: \(CommandLine.arguments[1])")
    }
}

func doStoreKeypair(account: String, reason: String, hex: String) {
    validateStorageKey(account, prefix: keypairPrefix)
    let data = hexToData(hex)
    guard data.count == 64 else { fail("keypair must be 64 bytes") }
    doAuthenticate(reason: reason)
    deleteItem(account: account)

    let s = SecItemAdd([
        kSecClass as String: kSecClassGenericPassword,
        kSecAttrService as String: svc,
        kSecAttrAccount as String: account,
        kSecValueData as String: data,
        kSecAttrAccess as String: keypairAccess(account: account)
    ] as CFDictionary, nil)
    guard s == errSecSuccess else { fail(errMsg(s)) }
    print("OK")
}

func doStorePubkey(account: String, hex: String) {
    validateStorageKey(account, prefix: pubkeyPrefix)
    let data = hexToData(hex)
    guard data.count == 32 else { fail("pubkey must be 32 bytes") }
    deleteItem(account: account)

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

func deleteItem(account: String) {
    let delStatus = SecItemDelete([
        kSecClass as String: kSecClassGenericPassword,
        kSecAttrService as String: svc,
        kSecAttrAccount as String: account
    ] as CFDictionary)
    if delStatus == -25244 {
        let p = Process(); p.executableURL = URL(fileURLWithPath: "/usr/bin/security")
        p.arguments = ["delete-generic-password", "-s", svc, "-a", account]
        try? p.run(); p.waitUntilExit()
    }
}

func doReadKeypair(account: String, reason: String) {
    validateStorageKey(account, prefix: keypairPrefix)
    doAuthenticate(reason: reason)
    var r: AnyObject?
    let s = SecItemCopyMatching([
        kSecClass as String: kSecClassGenericPassword,
        kSecAttrService as String: svc,
        kSecAttrAccount as String: account,
        kSecReturnData as String: true
    ] as CFDictionary, &r)
    guard s == errSecSuccess, let d = r as? Data else { fail(errMsg(s)) }
    guard d.count == 64 else { fail("keypair must be 64 bytes") }
    print(d.map { String(format: "%02x", $0) }.joined())
}

func doReadPubkey(account: String) {
    validateStorageKey(account, prefix: pubkeyPrefix)
    var r: AnyObject?
    let s = SecItemCopyMatching([
        kSecClass as String: kSecClassGenericPassword,
        kSecAttrService as String: svc,
        kSecAttrAccount as String: account,
        kSecReturnData as String: true
    ] as CFDictionary, &r)
    guard s == errSecSuccess, let d = r as? Data else { fail(errMsg(s)) }
    guard d.count == 32 else { fail("pubkey must be 32 bytes") }
    print(d.map { String(format: "%02x", $0) }.joined())
}

func doExists(account: String) {
    validateAnyStorageKey(account)
    let ctx = LAContext()
    ctx.interactionNotAllowed = true
    let s = SecItemCopyMatching([
        kSecClass as String: kSecClassGenericPassword,
        kSecAttrService as String: svc,
        kSecAttrAccount as String: account,
        kSecUseAuthenticationContext as String: ctx
    ] as CFDictionary, nil)
    print(s == errSecSuccess || s == errSecInteractionNotAllowed ? "yes" : "no")
}

func doDeleteKeypair(account: String, reason: String) {
    validateStorageKey(account, prefix: keypairPrefix)
    doAuthenticate(reason: reason)
    doDelete(account: account)
}

func doDeletePubkey(account: String) {
    validateStorageKey(account, prefix: pubkeyPrefix)
    doDelete(account: account)
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
    LAContext().evaluatePolicy(.deviceOwnerAuthentication, localizedReason: reason) { ok, e in
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

func keypairAccess(account: String) -> SecAccess {
    var access: SecAccess?
    // SecAccessControl moves this CLI helper onto the data-protection Keychain path,
    // which requires entitlements that local/ad-hoc command-line builds do not have.
    let status = SecAccessCreate("pay keypair \(account)" as CFString, nil, &access)
    guard status == errSecSuccess, let access else { fail("access: \(errMsg(status))") }
    return access
}

func validateAnyStorageKey(_ key: String) {
    if key.hasPrefix(keypairPrefix) {
        validateStorageKey(key, prefix: keypairPrefix)
    } else if key.hasPrefix(pubkeyPrefix) {
        validateStorageKey(key, prefix: pubkeyPrefix)
    } else {
        fail("unsupported storage key")
    }
}

func validateStorageKey(_ key: String, prefix: String) {
    guard key.hasPrefix(prefix) else { fail("invalid storage key namespace") }
    let account = String(key.dropFirst(prefix.count))
    guard !account.isEmpty else { fail("account name cannot be empty") }
    let allowed = account.utf8.allSatisfy { b in
        (b >= 48 && b <= 57) || (b >= 65 && b <= 90) || (b >= 97 && b <= 122) || b == 45 || b == 46 || b == 95
    }
    guard allowed else { fail("account name contains invalid characters") }
    guard !account.lowercased().hasSuffix(".pubkey") else { fail("account name uses reserved suffix") }
}

func hexToData(_ hex: String) -> Data {
    guard hex.count % 2 == 0 else { fail("hex string has odd length") }
    var d = Data()
    var i = hex.startIndex
    while i < hex.endIndex {
        let n = hex.index(i, offsetBy: 2)
        guard let b = UInt8(hex[i..<n], radix: 16) else { fail("invalid hex at offset \(hex.distance(from: hex.startIndex, to: i))") }
        d.append(b)
        i = n
    }
    return d
}

func errMsg(_ status: OSStatus) -> String { SecCopyErrorMessageString(status, nil) as String? ?? "error \(status)" }

func fail(_ msg: String) -> Never { fputs("ERROR:\(msg)\n", stderr); exit(1) }

main()
