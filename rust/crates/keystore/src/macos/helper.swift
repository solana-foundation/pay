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
