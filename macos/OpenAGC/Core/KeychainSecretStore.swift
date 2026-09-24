import Foundation
import Security

/// OpenAGC's secrets in the macOS Keychain (spec §12): generic passwords
/// under one service, readable only after first unlock, never synced.
///
/// Uses the data-protection keychain when the app is signed with a team
/// (release builds). Ad-hoc signed development builds lack the entitlement
/// (errSecMissingEntitlement), so they fall back to the login keychain.
final class KeychainSecretStore: @unchecked Sendable {
    let service: String
    private let lock = NSLock()
    private var useDataProtection: Bool?

    init(service: String = "ai.actual.openagc") {
        self.service = service
    }

    func get(_ key: String) throws(KeychainError) -> String? {
        var result: CFTypeRef?
        let status = run {
            var query = baseQuery(key)
            query[kSecReturnData as String] = true
            query[kSecMatchLimit as String] = kSecMatchLimitOne
            return SecItemCopyMatching(query as CFDictionary, &result)
        }
        switch status {
        case errSecSuccess:
            guard let data = result as? Data, let value = String(data: data, encoding: .utf8) else {
                throw KeychainError(status: errSecDecode, operation: "decode")
            }
            return value
        case errSecItemNotFound:
            return nil
        default:
            throw KeychainError(status: status, operation: "read")
        }
    }

    func set(_ key: String, _ value: String) throws(KeychainError) {
        let data = Data(value.utf8)
        let update = run {
            SecItemUpdate(baseQuery(key) as CFDictionary, [kSecValueData as String: data] as CFDictionary)
        }
        if update == errSecSuccess { return }
        guard update == errSecItemNotFound else { throw KeychainError(status: update, operation: "update") }
        let status = run {
            var add = baseQuery(key)
            add[kSecValueData as String] = data
            add[kSecAttrAccessible as String] = kSecAttrAccessibleAfterFirstUnlockThisDeviceOnly
            add[kSecAttrLabel as String] = "OpenAGC: \(key)"
            return SecItemAdd(add as CFDictionary, nil)
        }
        guard status == errSecSuccess else { throw KeychainError(status: status, operation: "add") }
    }

    func delete(_ key: String) throws(KeychainError) {
        let status = run { SecItemDelete(baseQuery(key) as CFDictionary) }
        guard status == errSecSuccess || status == errSecItemNotFound else {
            throw KeychainError(status: status, operation: "delete")
        }
    }

    private func baseQuery(_ key: String) -> [String: Any] {
        var q: [String: Any] = [
            kSecClass as String: kSecClassGenericPassword,
            kSecAttrService as String: service,
            kSecAttrAccount as String: key,
        ]
        if dataProtection { q[kSecUseDataProtectionKeychain as String] = true }
        return q
    }

    private var dataProtection: Bool {
        lock.lock()
        defer { lock.unlock() }
        return useDataProtection ?? true
    }

    /// Run a Keychain call. If the data-protection keychain reports a
    /// missing entitlement (ad-hoc signed builds), switch to the login
    /// keychain for the rest of the process and retry. Reads can report
    /// "not found" instead, so any call may trigger the switch.
    private func run(_ call: () -> OSStatus) -> OSStatus {
        let status = call()
        guard status == errSecMissingEntitlement else { return status }
        lock.lock()
        let wasDataProtection = useDataProtection ?? true
        useDataProtection = false
        lock.unlock()
        return wasDataProtection ? call() : status
    }
}

struct KeychainError: Error, CustomStringConvertible {
    let status: OSStatus
    let operation: String

    var description: String {
        let message = SecCopyErrorMessageString(status, nil) as String? ?? "OSStatus \(status)"
        return "Keychain \(operation) failed: \(message)"
    }
}
