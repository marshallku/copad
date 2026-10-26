import CryptoKit
import Foundation
import Security

/// End-to-end encrypted pushes (decisions #177). Shared by the app and its notification service
/// extension.
///
/// The app makes one AES-256 key per saved host and sends it to that host's comux with
/// `push-register` over SSH. comux seals each notification's title, body and pane with it; the
/// push relay and Apple carry only a placeholder alert plus `sealed` (nonce ‖ ciphertext ‖ tag)
/// and `kid` (the host id). The keys live in the Keychain under the access group both targets
/// share, readable after the first unlock so the extension can open a push on a locked phone.
enum PushKeys {
    private static let service = "com.marshall.copad.ios.push"

    /// The shared Keychain access group (`CopadKeychainGroup` in each target's Info.plist: the
    /// team-prefixed group in release builds, the bare one in ad-hoc debug builds).
    static var group: String? { Bundle.main.object(forInfoDictionaryKey: "CopadKeychainGroup") as? String }

    private static func query(_ host: String) -> [String: Any] {
        var q: [String: Any] = [kSecClass as String: kSecClassGenericPassword,
                                kSecAttrService as String: service,
                                kSecAttrAccount as String: host]
        if let group { q[kSecAttrAccessGroup as String] = group }
        return q
    }

    static func key(host: String) -> SymmetricKey? {
        var q = query(host)
        q[kSecReturnData as String] = true
        var out: AnyObject?
        guard SecItemCopyMatching(q as CFDictionary, &out) == errSecSuccess,
              let data = out as? Data, data.count == 32 else { return nil }
        return SymmetricKey(data: data)
    }

    /// This host's key (base64), made on first use.
    static func ensure(host: String) throws -> String {
        if let k = key(host: host) { return k.withUnsafeBytes { Data($0) }.base64EncodedString() }
        let data = SymmetricKey(size: .bits256).withUnsafeBytes { Data($0) }
        var add = query(host)
        add[kSecValueData as String] = data
        add[kSecAttrAccessible as String] = kSecAttrAccessibleAfterFirstUnlockThisDeviceOnly
        let status = SecItemAdd(add as CFDictionary, nil)
        guard status == errSecSuccess else {
            throw NSError(domain: NSOSStatusErrorDomain, code: Int(status),
                          userInfo: [NSLocalizedDescriptionKey: "Could not store the push key (\(status))."])
        }
        return data.base64EncodedString()
    }

    static func remove(host: String) { SecItemDelete(query(host) as CFDictionary) }
}

enum PushEnvelope {
    /// What a push says once opened: authenticated by the host's key.
    struct Opened: Equatable {
        var host: String
        var title: String
        var body: String
        var pane: String
    }

    static let aadPrefix = Data("comux-push-v1\0".utf8)

    /// Open a push's `sealed` with the key `kid` names. nil when anything is missing or the tag
    /// doesn't verify — then nothing in the push is trusted.
    static func open(_ info: [AnyHashable: Any], key: (String) -> SymmetricKey? = PushKeys.key) -> Opened? {
        guard let sealed = info["sealed"] as? String, let host = info["kid"] as? String,
              let k = key(host) else { return nil }
        return open(sealed: sealed, host: host, key: k)
    }

    static func open(sealed: String, host: String, key: SymmetricKey) -> Opened? {
        guard let raw = Data(base64Encoded: sealed),
              let box = try? AES.GCM.SealedBox(combined: raw),
              let plain = try? AES.GCM.open(box, using: key, authenticating: aadPrefix + Data(host.utf8)),
              let obj = try? JSONSerialization.jsonObject(with: plain) as? [String: Any] else { return nil }
        return Opened(host: host, title: obj["title"] as? String ?? "", body: obj["body"] as? String ?? "",
                      pane: obj["pane"] as? String ?? "")
    }
}
