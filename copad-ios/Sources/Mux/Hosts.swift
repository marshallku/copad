import CryptoKit
import Foundation
import NIOSSH
import Security

/// A machine running comux, reached over SSH. No password is ever stored: auth is this
/// device's key, which the user adds to the host's `authorized_keys` once.
struct MuxHost: Codable, Equatable, Identifiable {
    var id = UUID()
    var name: String
    var host: String
    var port: Int = 22
    var user: String
    /// The OpenSSH host key the user confirmed (TOFU). nil = never connected.
    var hostKey: String?
    /// What runs on the host for each channel. The default goes through a login shell so a
    /// comux in `~/.local/bin` / `~/.cargo/bin` is found on a non-interactive exec channel.
    var command: String = MuxHost.defaultCommand

    static let defaultCommand = "exec \"${SHELL:-/bin/sh}\" -lc 'exec comux proxy --stdio'"
}

/// Saved hosts, in UserDefaults (no secrets in them).
enum HostStore {
    private static let key = "copad.muxHosts"

    static func load() -> [MuxHost] {
        guard let d = UserDefaults.standard.data(forKey: key),
              let hosts = try? JSONDecoder().decode([MuxHost].self, from: d) else { return [] }
        return hosts
    }

    static func save(_ hosts: [MuxHost]) {
        if let d = try? JSONEncoder().encode(hosts) { UserDefaults.standard.set(d, forKey: key) }
    }
}

/// This device's SSH identity: an Ed25519 key generated on device and kept in the Keychain
/// (this-device-only, never synced or backed up). Secure Enclave keys are P-256 and only on
/// hardware; Ed25519 is what every OpenSSH accepts, so it is the default.
enum DeviceKey {
    private static let account = "copad.ssh.ed25519"
    private static let service = "com.marshall.copad.ios.ssh"

    /// The key, creating it on first use.
    static func load() throws -> Curve25519.Signing.PrivateKey {
        if let raw = read() { return try Curve25519.Signing.PrivateKey(rawRepresentation: raw) }
        let k = Curve25519.Signing.PrivateKey()
        try write(k.rawRepresentation)
        return k
    }

    /// Replace the key with one from a 32-byte seed (base64) — for importing an existing key,
    /// and for the simulator e2e, which authorizes a key it generated itself.
    static func importSeed(base64: String) throws {
        guard let seed = Data(base64Encoded: base64.trimmingCharacters(in: .whitespacesAndNewlines)),
              seed.count == 32 else { throw KeyError.badSeed }
        _ = try Curve25519.Signing.PrivateKey(rawRepresentation: seed)
        delete()
        try write(seed)
    }

    static func nioKey() throws -> NIOSSHPrivateKey { NIOSSHPrivateKey(ed25519Key: try load()) }

    /// `ssh-ed25519 AAAA… copad-ios` — the line to paste into `authorized_keys`.
    static func publicLine() throws -> String {
        let pub = NIOSSHPrivateKey(ed25519Key: try load()).publicKey
        return String(openSSHPublicKey: pub) + " copad-ios"
    }

    enum KeyError: LocalizedError {
        case badSeed, keychain(OSStatus)
        var errorDescription: String? {
            switch self {
            case .badSeed: return "Not a base64 Ed25519 seed (32 bytes)."
            case let .keychain(s): return "Keychain error \(s)."
            }
        }
    }

    private static func query() -> [String: Any] {
        [kSecClass as String: kSecClassGenericPassword,
         kSecAttrService as String: service,
         kSecAttrAccount as String: account]
    }

    private static func read() -> Data? {
        var q = query()
        q[kSecReturnData as String] = true
        var out: CFTypeRef?
        guard SecItemCopyMatching(q as CFDictionary, &out) == errSecSuccess else { return nil }
        return out as? Data
    }

    private static func write(_ d: Data) throws {
        var q = query()
        q[kSecValueData as String] = d
        q[kSecAttrAccessible as String] = kSecAttrAccessibleAfterFirstUnlockThisDeviceOnly
        let s = SecItemAdd(q as CFDictionary, nil)
        guard s == errSecSuccess else { throw KeyError.keychain(s) }
    }

    private static func delete() { SecItemDelete(query() as CFDictionary) }
}

/// Splits a byte stream into lines (`\n`), keeping a partial tail between chunks.
struct LineSplitter {
    private var buf = Data()

    mutating func feed(_ d: Data) -> [Data] {
        buf.append(d)
        var out: [Data] = []
        while let nl = buf.firstIndex(of: 0x0A) {
            let line = buf[buf.startIndex..<nl]
            if !line.isEmpty { out.append(Data(line)) }
            buf.removeSubrange(buf.startIndex...nl)
        }
        return out
    }
}
