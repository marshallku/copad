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

/// This device's SSH identity, kept in the Keychain (this-device-only, never synced or backed
/// up). On hardware it is a Secure Enclave P-256 key (`ecdsa-sha2-nistp256`, decisions #173):
/// the private key is generated inside the enclave and can never be read out, not even by this
/// app. The Simulator has no enclave and uses an Ed25519 key. A device that already had an
/// Ed25519 key keeps using it — its `authorized_keys` lines stay valid — until "New Secure
/// Enclave key" replaces it.
enum DeviceKey {
    enum Kind: String {
        case ed25519
        case secureEnclave = "se-p256"
        /// A software P-256 key: only ever imported (the simulator e2e proves the ECDSA path
        /// with it, since the Simulator has no enclave).
        case p256
    }

    private static let service = "com.marshall.copad.ios.ssh"
    private static let kindKey = "copad.ssh.kind"

    static var secureEnclaveAvailable: Bool { SecureEnclave.isAvailable }

    /// The key in use: the recorded kind; else an existing Ed25519 key; else a new key —
    /// Secure Enclave when there is one.
    static var kind: Kind {
        if let k = UserDefaults.standard.string(forKey: kindKey).flatMap(Kind.init) { return k }
        if read(.ed25519) != nil || !secureEnclaveAvailable { return .ed25519 }
        return .secureEnclave
    }

    static func nioKey() throws -> NIOSSHPrivateKey {
        switch kind {
        case .ed25519:
            if let raw = read(.ed25519) {
                return NIOSSHPrivateKey(ed25519Key: try Curve25519.Signing.PrivateKey(rawRepresentation: raw))
            }
            let k = Curve25519.Signing.PrivateKey()
            try write(.ed25519, k.rawRepresentation)
            return NIOSSHPrivateKey(ed25519Key: k)
        case .secureEnclave:
            if let blob = read(.secureEnclave) {
                return NIOSSHPrivateKey(secureEnclaveP256Key: try SecureEnclave.P256.Signing.PrivateKey(dataRepresentation: blob))
            }
            return NIOSSHPrivateKey(secureEnclaveP256Key: try newEnclaveKey())
        case .p256:
            guard let raw = read(.p256) else { throw KeyError.missing }
            return NIOSSHPrivateKey(p256Key: try P256.Signing.PrivateKey(rawRepresentation: raw))
        }
    }

    /// `ssh-ed25519 AAAA… copad-ios` / `ecdsa-sha2-nistp256 AAAA… copad-ios` — the line to
    /// paste into `authorized_keys`.
    static func publicLine() throws -> String {
        String(openSSHPublicKey: try nioKey().publicKey) + " copad-ios"
    }

    /// Replace this device's key with a new Secure Enclave key (hosts then need the new line).
    static func useNewSecureEnclaveKey() throws {
        guard secureEnclaveAvailable else { throw KeyError.noEnclave }
        delete(.secureEnclave)
        _ = try newEnclaveKey()
        UserDefaults.standard.set(Kind.secureEnclave.rawValue, forKey: kindKey)
    }

    /// Replace the key with one from a 32-byte Ed25519 seed (base64) — for importing an existing
    /// key, and for the simulator e2e, which authorizes a key it generated itself.
    static func importSeed(base64: String) throws {
        let seed = try bytes(base64)
        _ = try Curve25519.Signing.PrivateKey(rawRepresentation: seed)
        delete(.ed25519)
        try write(.ed25519, seed)
        UserDefaults.standard.set(Kind.ed25519.rawValue, forKey: kindKey)
    }

    /// Replace the key with a software P-256 key from its 32-byte scalar (base64): the
    /// simulator e2e's way through the ECDSA path the Secure Enclave key takes on hardware.
    static func importP256(base64: String) throws {
        let raw = try bytes(base64)
        _ = try P256.Signing.PrivateKey(rawRepresentation: raw)
        delete(.p256)
        try write(.p256, raw)
        UserDefaults.standard.set(Kind.p256.rawValue, forKey: kindKey)
    }

    enum KeyError: LocalizedError {
        case badSeed, missing, noEnclave, keychain(OSStatus)
        var errorDescription: String? {
            switch self {
            case .badSeed: return "Not a base64 32-byte key."
            case .missing: return "The imported key is gone from the Keychain."
            case .noEnclave: return "This device has no Secure Enclave."
            case let .keychain(s): return "Keychain error \(s)."
            }
        }
    }

    private static func bytes(_ base64: String) throws -> Data {
        guard let d = Data(base64Encoded: base64.trimmingCharacters(in: .whitespacesAndNewlines)),
              d.count == 32 else { throw KeyError.badSeed }
        return d
    }

    /// A new enclave key, usable only while the device is unlocked and only on this device. No
    /// biometry: every SSH connect would otherwise ask for Face ID.
    private static func newEnclaveKey() throws -> SecureEnclave.P256.Signing.PrivateKey {
        var err: Unmanaged<CFError>?
        guard let access = SecAccessControlCreateWithFlags(
            nil, kSecAttrAccessibleWhenUnlockedThisDeviceOnly, .privateKeyUsage, &err) else {
            throw err.map { $0.takeRetainedValue() as Error } ?? KeyError.noEnclave
        }
        let k = try SecureEnclave.P256.Signing.PrivateKey(compactRepresentable: false, accessControl: access)
        // The blob is the enclave's wrapped handle, useless off this device.
        try write(.secureEnclave, k.dataRepresentation)
        return k
    }

    private static func query(_ kind: Kind) -> [String: Any] {
        [kSecClass as String: kSecClassGenericPassword,
         kSecAttrService as String: service,
         kSecAttrAccount as String: "copad.ssh.\(kind.rawValue)"]
    }

    private static func read(_ kind: Kind) -> Data? {
        var q = query(kind)
        q[kSecReturnData as String] = true
        var out: CFTypeRef?
        guard SecItemCopyMatching(q as CFDictionary, &out) == errSecSuccess else { return nil }
        return out as? Data
    }

    private static func write(_ kind: Kind, _ d: Data) throws {
        var q = query(kind)
        q[kSecValueData as String] = d
        q[kSecAttrAccessible as String] = kSecAttrAccessibleAfterFirstUnlockThisDeviceOnly
        let s = SecItemAdd(q as CFDictionary, nil)
        guard s == errSecSuccess else { throw KeyError.keychain(s) }
    }

    private static func delete(_ kind: Kind) { SecItemDelete(query(kind) as CFDictionary) }
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
