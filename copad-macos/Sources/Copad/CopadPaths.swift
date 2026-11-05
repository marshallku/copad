import Foundation

/// Mirrors `copad-core::paths::daemon_socket_path()`, duplicated to avoid an FFI call.
enum CopadPaths {
    static func runtimeDir() -> URL {
        let home = FileManager.default.homeDirectoryForCurrentUser
        return home.appending(path: "Library/Caches/copad")
    }

    /// A per-instance GUI socket is ignored (it doesn't speak the daemon wire protocol; a child
    /// shell inherits it from Copad.app). `AutoSpawn` forks `copadd` with this env, so client and
    /// daemon must resolve the path identically.
    static func daemonSocket() -> URL {
        if let override = ProcessInfo.processInfo.environment["COPAD_SOCKET"], !override.isEmpty {
            let url = URL(fileURLWithPath: override)
            if !isLegacyPerInstanceSocket(url) {
                return url
            }
        }
        return runtimeDir().appending(path: "socket")
    }

    /// Mirror of `copad-core::paths::is_legacy_per_instance_socket`.
    static func isLegacyPerInstanceSocket(_ url: URL) -> Bool {
        let s = url.path(percentEncoded: false)
        if let middle = s.stripPrefix("/tmp/copad-")?.stripSuffix(".sock"),
           !middle.isEmpty, middle.allSatisfy(\.isASCIIDigit)
        {
            return true
        }
        let runtime = runtimeDir().path(percentEncoded: false)
        if let trimmed = s.stripPrefix(runtime + "/"),
           let middle = trimmed.stripPrefix("gui-")?.stripSuffix(".sock"),
           !middle.isEmpty, middle.allSatisfy(\.isASCIIDigit)
        {
            return true
        }
        return false
    }

    static func spawnLock() -> URL {
        runtimeDir().appending(path: ".spawn.lock")
    }

    static func ensureRuntimeDir() throws {
        let dir = runtimeDir()
        try FileManager.default.createDirectory(
            at: dir,
            withIntermediateDirectories: true,
            attributes: [.posixPermissions: 0o700],
        )
    }
}

private extension String {
    func stripPrefix(_ prefix: String) -> String? {
        guard hasPrefix(prefix) else { return nil }
        return String(dropFirst(prefix.count))
    }

    func stripSuffix(_ suffix: String) -> String? {
        guard hasSuffix(suffix) else { return nil }
        return String(dropLast(suffix.count))
    }
}

private extension Character {
    var isASCIIDigit: Bool {
        isASCII && isNumber
    }
}
