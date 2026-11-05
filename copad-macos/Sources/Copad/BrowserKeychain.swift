import Foundation
import Security

/// The one place a website password exists outside the page.
///
/// There is no plaintext fallback. Decision #54 put Slack bot tokens in a `0600` file because
/// they are revocable in one click and read by a plugin that cannot answer a Keychain prompt;
/// website passwords are neither, and a `0600` file is readable by the same-user agent this
/// design keeps them from. An unavailable Keychain DISABLES the feature.
///
/// Tiers:
/// - The data-protection keychain (`kSecUseDataProtectionKeychain`) is preferred: it scopes
///   items to the signing identity, while the legacy keychain binds an ACL and partition list
///   to the binary's cdhash, so a rebuilt binary can PROMPT (decision #54).
/// - It needs a `keychain-access-groups` entitlement, which needs a team identifier. A
///   Developer ID build has one; a self-signed dev build does not and fails with
///   `errSecMissingEntitlement` (-34018). Adding the entitlement to a self-signed bundle makes
///   AMFI kill the process on launch (measured). Hence the fallback to the legacy file
///   keychain, which also keeps this path exercised outside releases.
/// - The last tier is the deprecated pre-unified `SecKeychain*` API (what the `security` CLI
///   uses), which needs no entitlement.
///
/// Legacy caveat: an item saved by one build and read by a rebuild may prompt, because the
/// cdhash changed. Within one run it does not.
enum BrowserKeychain {
    /// How long a keychain call may take before it is treated as unavailable. This is the
    /// decision #54 failure: a legacy keychain read whose ACL does not list the calling binary
    /// raises a SecurityAgent prompt, and an unanswered prompt blocks indefinitely. On the main
    /// actor (where every RPC handler lives) that freezes the whole GUI, which was measured. So
    /// every operation runs off the main thread and is abandoned after this long, surfacing as
    /// `secret_backend_unavailable`.
    private static let timeout: TimeInterval = 3

    /// Run a keychain operation off the main thread, bounded, and deliver the result back on the
    /// main actor.
    ///
    /// Asynchronous, not "off-thread but awaited": blocking on a semaphore would only shorten
    /// the freeze, because every credential caller is on the main actor. The work is NOT
    /// cancelled on timeout (a blocked `SecItemCopyMatching` can't be cancelled), it is
    /// abandoned, and a late completion finds `delivered` already true and does nothing.
    private static func bounded<T: Sendable>(
        _ fallback: T,
        _ body: @escaping @Sendable () -> T,
        then deliver: @escaping @MainActor (T) -> Void,
    ) {
        let state = UncheckedBox<Bool>(false)
        let lock = NSLock()
        let finish: @Sendable (T) -> Void = { value in
            lock.lock()
            let already = state.value
            state.value = true
            lock.unlock()
            guard !already else { return }
            Task { @MainActor in deliver(value) }
        }
        DispatchQueue.global(qos: .userInitiated).async { finish(body()) }
        DispatchQueue.global(qos: .userInitiated).asyncAfter(deadline: .now() + timeout) {
            lock.lock()
            let already = state.value
            lock.unlock()
            guard !already else { return }
            FileHandle.standardError.write(Data(
                "[copad-keychain] a keychain call did not return within \(timeout)s — most likely an unanswered access prompt; treating the backend as unavailable\n".utf8,
            ))
            finish(fallback)
        }
    }

    private final class UncheckedBox<T>: @unchecked Sendable {
        var value: T
        init(_ value: T) { self.value = value }
    }

    /// One service for every browser credential (account = credential id), distinct from the
    /// plugins' service names so an audit can tell website passwords from plugin API tokens.
    private static let service = "com.marshall.copad.browser"

    enum Failure: Error {
        case unavailable(String)
        case notFound
    }

    /// The origin and slot travel WITH the secret, not in the index. The index is a plain JSON
    /// file anything running as the user (including the agent) can rewrite; re-pointing an
    /// entry at an attacker-chosen origin would keep its keychain account (lookup is by id
    /// alone) while the validator and the injection's origin check accepted the new
    /// destination. The keychain is the trusted store, so the binding lives there.
    struct Stored: Sendable {
        let secret: String
        let origin: String
        let slot: String
    }

    static func save(
        id: String,
        secret: String,
        origin: String,
        slot: String,
        then deliver: @escaping @MainActor (Result<Void, Failure>) -> Void,
    ) {
        let envelope: [String: String] = ["secret": secret, "origin": origin, "slot": slot]
        guard let data = try? JSONSerialization.data(withJSONObject: envelope) else {
            Task { @MainActor in deliver(.failure(.unavailable("credential could not be encoded"))) }
            return
        }
        bounded(.failure(.unavailable("keychain timed out")), {
            saveBlocking(id: id, data: data)
        }, then: deliver)
    }

    static func read(
        id: String,
        then deliver: @escaping @MainActor (Result<Stored, Failure>) -> Void,
    ) {
        bounded(.failure(.unavailable("keychain timed out")), { readBlocking(id: id) }, then: deliver)
    }

    static func delete(
        id: String,
        then deliver: @escaping @MainActor (Result<Void, Failure>) -> Void = { _ in },
    ) {
        bounded(.failure(.unavailable("keychain timed out")), { deleteBlocking(id: id) }, then: deliver)
    }

    // MARK: - Blocking implementations (always off the main thread)

    private static func saveBlocking(id: String, data: Data) -> Result<Void, Failure> {
        // UPDATE first, add only if absent. Delete-then-add is destructive: if the add failed,
        // the old password would be gone while its index entry still promised it. An update
        // that matches nothing is `errSecItemNotFound`, the signal to add.
        let updated = withKeychain { dataProtection in
            SecItemUpdate(
                baseQuery(id: id, dataProtection: dataProtection) as CFDictionary,
                [kSecValueData as String: data] as CFDictionary,
            )
        }
        if updated == errSecSuccess { return .success(()) }
        if updated != errSecItemNotFound, updated != errSecMissingEntitlement {
            return .failure(.unavailable(describe(updated)))
        }
        let status = withKeychain { dataProtection in
            var attrs = baseQuery(id: id, dataProtection: dataProtection)
            attrs[kSecValueData as String] = data
            // Available when unlocked and never synced to iCloud: the password is for THIS
            // machine only.
            attrs[kSecAttrAccessible as String] = kSecAttrAccessibleWhenUnlocked
            return SecItemAdd(attrs as CFDictionary, nil)
        }
        if status == errSecMissingEntitlement {
            return legacySave(id: id, data: data)
        }
        guard status == errSecSuccess else {
            return .failure(.unavailable(describe(status)))
        }
        return .success(())
    }

    /// The returned `String` is the ONLY place the plaintext exists in this
    /// process, and the caller hands it straight to the page and drops it. It
    /// never crosses an RPC boundary.
    private static func readBlocking(id: String) -> Result<Stored, Failure> {
        var out: CFTypeRef?
        let status = withKeychain { dataProtection in
            var query = baseQuery(id: id, dataProtection: dataProtection)
            query[kSecReturnData as String] = true
            query[kSecMatchLimit as String] = kSecMatchLimitOne
            return SecItemCopyMatching(query as CFDictionary, &out)
        }
        if status == errSecMissingEntitlement { return legacyRead(id: id) }
        if status == errSecItemNotFound { return .failure(.notFound) }
        guard status == errSecSuccess, let data = out as? Data else {
            return .failure(.unavailable(describe(status)))
        }
        return decode(data)
    }

    private static func deleteBlocking(id: String) -> Result<Void, Failure> {
        let status = withKeychain { dataProtection in
            SecItemDelete(baseQuery(id: id, dataProtection: dataProtection) as CFDictionary)
        }
        if status == errSecSuccess || status == errSecItemNotFound { return .success(()) }
        if status == errSecMissingEntitlement { return legacyDelete(id: id) }
        return .failure(.unavailable(describe(status)))
    }

    private static func baseQuery(id: String, dataProtection: Bool) -> [String: Any] {
        var q: [String: Any] = [
            kSecClass as String: kSecClassGenericPassword,
            kSecAttrService as String: service,
            kSecAttrAccount as String: id,
        ]
        if dataProtection {
            q[kSecUseDataProtectionKeychain as String] = true
        }
        return q
    }

    /// Which keychain this process ended up on, resolved on the first operation by trying the
    /// preferred one and watching for `errSecMissingEntitlement`. Operations run on global
    /// queues, so concurrent requests reach this from several threads: guarded by `backendLock`.
    private nonisolated(unsafe) static var useDataProtectionStorage: Bool?
    private static let backendLock = NSLock()

    private static var useDataProtection: Bool? {
        get {
            backendLock.lock()
            defer { backendLock.unlock() }
            return useDataProtectionStorage
        }
        set {
            backendLock.lock()
            useDataProtectionStorage = newValue
            backendLock.unlock()
        }
    }

    private static func withKeychain(_ body: (Bool) -> OSStatus) -> OSStatus {
        if let known = useDataProtection {
            return body(known)
        }
        let status = body(true)
        if status == errSecMissingEntitlement || status == errSecParam {
            let fallback = body(false)
            // Only remember the fallback if it actually worked; a failure for
            // some other reason should not pin us to the legacy keychain.
            if fallback == errSecSuccess || fallback == errSecItemNotFound {
                useDataProtection = false
            }
            return fallback
        }
        if status == errSecSuccess || status == errSecItemNotFound {
            useDataProtection = true
        }
        return status
    }

    // MARK: - Pre-unified Keychain API
    //
    // Entitlement-free, and the only tier that works in a self-signed dev build; kept behind the
    // modern paths so a signed release never touches it. The `SecKeychain*` deprecation
    // warnings are left visible on purpose: annotating these helpers `@available(deprecated:)`
    // would be a lie (the wrapped API is deprecated, not they) and only move the warnings.

    private static func legacySave(id: String, data: Data) -> Result<Void, Failure> {
        // Same rule as the modern path: modify in place when the item exists,
        // so a failed write cannot destroy the password that was already there.
        let svc = Array(self.service.utf8)
        let acct = Array(id.utf8)
        var existing: SecKeychainItem?
        let found = SecKeychainFindGenericPassword(
            nil,
            UInt32(svc.count), svc.map { CChar(bitPattern: $0) },
            UInt32(acct.count), acct.map { CChar(bitPattern: $0) },
            nil, nil, &existing,
        )
        if found == errSecSuccess, let existing {
            let bytes = [UInt8](data)
            let status = SecKeychainItemModifyAttributesAndData(
                existing, nil, UInt32(bytes.count), bytes,
            )
            guard status == errSecSuccess else {
                return .failure(.unavailable(describe(status)))
            }
            return .success(())
        }
        let service = Array(service.utf8)
        let account = Array(id.utf8)
        let bytes = [UInt8](data)
        let status = SecKeychainAddGenericPassword(
            nil,
            UInt32(service.count), service.map { CChar(bitPattern: $0) },
            UInt32(account.count), account.map { CChar(bitPattern: $0) },
            UInt32(bytes.count), bytes,
            nil,
        )
        guard status == errSecSuccess else {
            return .failure(.unavailable(describe(status)))
        }
        return .success(())
    }

    private static func legacyRead(id: String) -> Result<Stored, Failure> {
        let service = Array(self.service.utf8)
        let account = Array(id.utf8)
        var length: UInt32 = 0
        var bytes: UnsafeMutableRawPointer?
        let status = SecKeychainFindGenericPassword(
            nil,
            UInt32(service.count), service.map { CChar(bitPattern: $0) },
            UInt32(account.count), account.map { CChar(bitPattern: $0) },
            &length, &bytes, nil,
        )
        if status == errSecItemNotFound { return .failure(.notFound) }
        guard status == errSecSuccess, let bytes else {
            return .failure(.unavailable(describe(status)))
        }
        defer { SecKeychainItemFreeContent(nil, bytes) }
        return decode(Data(bytes: bytes, count: Int(length)))
    }

    private static func legacyDelete(id: String) -> Result<Void, Failure> {
        let service = Array(self.service.utf8)
        let account = Array(id.utf8)
        var item: SecKeychainItem?
        let found = SecKeychainFindGenericPassword(
            nil,
            UInt32(service.count), service.map { CChar(bitPattern: $0) },
            UInt32(account.count), account.map { CChar(bitPattern: $0) },
            nil, nil, &item,
        )
        if found == errSecItemNotFound { return .success(()) }
        guard found == errSecSuccess, let item else {
            return .failure(.unavailable(describe(found)))
        }
        let status = SecKeychainItemDelete(item)
        guard status == errSecSuccess else {
            return .failure(.unavailable(describe(status)))
        }
        return .success(())
    }

    private static func decode(_ data: Data) -> Result<Stored, Failure> {
        guard let obj = try? JSONSerialization.jsonObject(with: data) as? [String: String],
              let secret = obj["secret"], let origin = obj["origin"], let slot = obj["slot"]
        else {
            // An item written by an older build, or by hand, has no envelope.
            // Refuse it rather than guessing an origin — a credential whose
            // binding cannot be established must not be fillable anywhere.
            return .failure(.unavailable("stored credential has no origin binding"))
        }
        return .success(Stored(secret: secret, origin: origin, slot: slot))
    }

    private static func describe(_ status: OSStatus) -> String {
        let message = SecCopyErrorMessageString(status, nil) as String? ?? "unknown"
        return "\(message) (OSStatus \(status))"
    }
}
