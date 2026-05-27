import Darwin
import Foundation

/// Single-flight `copadd` auto-spawn helper. Copad.app-only;
/// `coctl` does not auto-spawn (matches Linux UX).
///
/// Flow:
///   1. `lockf(F_TLOCK)` `~/Library/Caches/copad/.spawn.lock` — held → bail.
///   2. Live socket probe — another process may have won the race between
///      our failed connect and lock acquisition.
///   3. Locate `copadd` binary (PATH, then `~/.cargo/bin/copadd`).
///   4. Detached spawn via `nohup copadd … &`.
///   5. `system.ping` probe with 3s budget — bind happens before plugin
///      activation but manifest discovery + command registration happen
///      before bind, so the daemon may need a moment to listen.
///
/// Returns true only when the daemon answers `system.ping` ok. False keeps
/// `DaemonClient` in disconnected state; `ActionRegistry` fallback then
/// surfaces `daemon_unavailable`.
enum AutoSpawn {
    static func ensureRunning() -> Bool {
        do {
            try CopadPaths.ensureRuntimeDir()
        } catch {
            log("ensureRuntimeDir: \(error)")
            return false
        }

        let lock: FileLock
        do {
            lock = try FileLock(path: CopadPaths.spawnLock())
        } catch {
            log("open spawn lock: \(error)")
            return false
        }

        do {
            // Don't wait under the lock — caller's reconnect loop polls;
            // the other spawner will either succeed or release.
            if try !lock.tryAcquire() {
                log("spawn lock held by another process — caller should retry connect")
                return false
            }
        } catch {
            log("flock acquire: \(error)")
            return false
        }
        defer { lock.release() }

        if probeSocket(timeout: 1.0) {
            log("daemon socket alive at lock-acquire (race winner) — skipping spawn")
            return true
        }

        guard let copaddPath = locateBinary() else {
            log("copadd binary not found in PATH, ~/.cargo/bin, or /opt/homebrew/bin — install via `cargo install --path copad-daemon` or `brew install --cask marshallku/copad/copad`")
            return false
        }
        if !spawnDetached(path: copaddPath) {
            return false
        }
        return waitForPing(budget: 3.0, perAttempt: 0.5)
    }

    // MARK: - Helpers

    private static func locateBinary() -> URL? {
        let env = ProcessInfo.processInfo.environment
        let pathString = env["PATH"] ?? "/usr/local/bin:/usr/bin:/bin"
        var dirs = pathString.split(separator: ":").map { String($0) }
        // Augment with the two install dirs that a Finder-launched .app
        // does NOT inherit from the user's shell env:
        // - `~/.cargo/bin` for the `scripts/install-macos.sh` path
        //   (cargo install --path copad-daemon).
        // - `/opt/homebrew/bin` for the Homebrew cask path. Intel Macs use
        //   `/usr/local/bin`, which is already in the `pathString` fallback
        //   above, so this only needs to add the arm64 prefix.
        for extra in ["\(NSHomeDirectory())/.cargo/bin", "/opt/homebrew/bin"] {
            if !dirs.contains(extra) { dirs.append(extra) }
        }
        let fm = FileManager.default
        for dir in dirs {
            let candidate = URL(fileURLWithPath: dir).appending(path: "copadd")
            if fm.isExecutableFile(atPath: candidate.path) { return candidate }
        }
        return nil
    }

    /// Detached spawn so the child outlives Copad.app.
    ///
    /// `copadd` honors inherited `COPAD_SOCKET` for its bind path, but
    /// `CopadPaths.daemonSocket()` ignores legacy per-GUI overrides — so
    /// the env we inherit could send the daemon to a different path than
    /// the client probes. Explicitly set the child's env to our resolved
    /// daemon socket to keep the two in sync.
    private static func spawnDetached(path: URL) -> Bool {
        let proc = Process()
        proc.executableURL = URL(fileURLWithPath: "/bin/sh")
        let escaped = path.path(percentEncoded: false).replacingOccurrences(of: "'", with: "'\\''")
        proc.arguments = ["-c", "nohup '\(escaped)' >/dev/null 2>&1 &"]
        var env = ProcessInfo.processInfo.environment
        env["COPAD_SOCKET"] = CopadPaths.daemonSocket().path(percentEncoded: false)
        proc.environment = env
        do {
            try proc.run()
        } catch {
            log("spawn fork: \(error)")
            return false
        }
        proc.waitUntilExit()
        return proc.terminationStatus == 0
    }

    /// Connect-only probe (no protocol traffic). Closes the fd on success.
    private static func probeSocket(timeout: TimeInterval) -> Bool {
        let deadline = Date().addingTimeInterval(timeout)
        while Date() < deadline {
            if let fd = connectOnce() {
                close(fd)
                return true
            }
            Thread.sleep(forTimeInterval: 0.1)
        }
        return false
    }

    private static func waitForPing(budget: TimeInterval, perAttempt: TimeInterval) -> Bool {
        let deadline = Date().addingTimeInterval(budget)
        var attempt = 0
        while Date() < deadline {
            attempt += 1
            if pingOnce(timeout: perAttempt) {
                log("daemon ack'd system.ping on attempt \(attempt)")
                return true
            }
            Thread.sleep(forTimeInterval: 0.2)
        }
        log("daemon did not ack system.ping within \(budget)s — auto-spawn FAILED, stays disconnected")
        return false
    }

    private static func connectOnce() -> Int32? {
        UnixSocket.connect(path: CopadPaths.daemonSocket().path(percentEncoded: false))
    }

    private static func pingOnce(timeout _: TimeInterval) -> Bool {
        guard let fd = connectOnce() else { return false }
        defer { close(fd) }
        let id = UUID().uuidString
        let req = "{\"id\":\"\(id)\",\"method\":\"system.ping\",\"params\":{}}\n"
        var bytes = Array(req.utf8)
        let sent = bytes.withUnsafeMutableBufferPointer { buf -> Int in
            Darwin.write(fd, buf.baseAddress, buf.count)
        }
        if sent <= 0 { return false }

        // SO_RCVTIMEO so a wedged daemon doesn't block — caller's loop
        // owns the per-attempt budget.
        var tv = timeval(tv_sec: 0, tv_usec: 500_000)
        _ = setsockopt(fd, SOL_SOCKET, SO_RCVTIMEO, &tv, socklen_t(MemoryLayout<timeval>.size))

        var buf = [UInt8](repeating: 0, count: 4096)
        let n = buf.withUnsafeMutableBufferPointer { bp -> Int in
            Darwin.read(fd, bp.baseAddress, bp.count)
        }
        if n <= 0 { return false }
        let line = String(decoding: buf.prefix(n), as: UTF8.self)
        return line.contains("\"ok\":true") && line.contains(id)
    }

    private static func log(_ msg: String) {
        FileHandle.standardError.write(Data("[copad-autospawn] \(msg)\n".utf8))
    }
}
