import Foundation

/// Mirrors `copad-core::paths::daemon_socket_path()`. Duplicated here
/// rather than read through FFI to avoid a bridge call for a constant path.
enum CopadPaths {
    static func runtimeDir() -> URL {
        let home = FileManager.default.homeDirectoryForCurrentUser
        return home.appending(path: "Library/Caches/copad")
    }

    /// Honors `COPAD_SOCKET` unless it points at a per-instance GUI socket
    /// (which doesn't speak the daemon wire protocol — happens when
    /// Copad.app's child shell inherits the env). Otherwise
    /// `<runtime_dir>/socket`.
    ///
    /// `AutoSpawn` inherits this process's env when forking `copadd`, so
    /// the daemon binds whatever `COPAD_SOCKET` resolves to here — keeping
    /// client + daemon paths in sync requires the same resolution on both
    /// sides.
    static func daemonSocket() -> URL {
        if let override = ProcessInfo.processInfo.environment["COPAD_SOCKET"], !override.isEmpty {
            let url = URL(fileURLWithPath: override)
            if !isLegacyPerInstanceSocket(url) {
                return url
            }
        }
        return runtimeDir().appending(path: "socket")
    }

    /// `/tmp/copad-{PID}.sock` (legacy) or `<runtime_dir>/gui-{PID}.sock`.
    /// Mirror of `copad-core::paths::is_legacy_per_instance_socket`.
    static func isLegacyPerInstanceSocket(_ url: URL) -> Bool {
        let s = url.path(percentEncoded: false)
        // /tmp/copad-{digits}.sock
        if let middle = s.stripPrefix("/tmp/copad-")?.stripSuffix(".sock"),
           !middle.isEmpty, middle.allSatisfy(\.isASCIIDigit)
        {
            return true
        }
        // <runtime_dir>/gui-{digits}.sock
        let runtime = runtimeDir().path(percentEncoded: false)
        if let trimmed = s.stripPrefix(runtime + "/"),
           let middle = trimmed.stripPrefix("gui-")?.stripSuffix(".sock"),
           !middle.isEmpty, middle.allSatisfy(\.isASCIIDigit)
        {
            return true
        }
        return false
    }

    /// Single-flight lock for copadd auto-spawn.
    static func spawnLock() -> URL {
        runtimeDir().appending(path: ".spawn.lock")
    }

    /// Ensure the runtime dir exists with restrictive perms. Idempotent.
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
