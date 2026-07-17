import Foundation
import Security

/// Secure persistence for the web-bridge bearer token. The token can drive the
/// workstation terminal, so it lives in the iOS Keychain — never UserDefaults.
/// `AfterFirstUnlock` so it survives reboots and is readable when a background
/// push wakes the app.
enum TokenStore {
    private static let service = "com.marshall.copad.ios.bearer"
    private static let account = "web-bridge"

    private static func baseQuery() -> [String: Any] {
        [
            kSecClass as String: kSecClassGenericPassword,
            kSecAttrService as String: service,
            kSecAttrAccount as String: account,
        ]
    }

    /// Persist the token. Updates in place when it already exists, adds it
    /// otherwise — never deletes first, so a failed write can't lose the prior
    /// value. Returns whether it succeeded.
    @discardableResult
    static func save(_ token: String) -> Bool {
        let data = Data(token.utf8)
        let update = SecItemUpdate(
            baseQuery() as CFDictionary,
            [kSecValueData as String: data] as CFDictionary,
        )
        if update == errSecSuccess { return true }
        guard update == errSecItemNotFound else { return false }
        var add = baseQuery()
        add[kSecValueData as String] = data
        add[kSecAttrAccessible as String] = kSecAttrAccessibleAfterFirstUnlock
        return SecItemAdd(add as CFDictionary, nil) == errSecSuccess
    }

    static func load() -> String? {
        var query = baseQuery()
        query[kSecReturnData as String] = true
        query[kSecMatchLimit as String] = kSecMatchLimitOne
        var result: AnyObject?
        guard SecItemCopyMatching(query as CFDictionary, &result) == errSecSuccess,
              let data = result as? Data,
              let token = String(data: data, encoding: .utf8)
        else { return nil }
        return token
    }

    /// Delete the token. Returns whether it's gone (success, or it was already
    /// absent). A failed delete must NOT be treated as cleared, or the old
    /// credential would resurrect on the next launch.
    @discardableResult
    static func clear() -> Bool {
        let status = SecItemDelete(baseQuery() as CFDictionary)
        return status == errSecSuccess || status == errSecItemNotFound
    }
}
