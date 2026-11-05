import Darwin
import Foundation

/// Single-flight `copadd` auto-spawn, Copad.app only (`coctl` does not auto-spawn).
///
/// Takes `lockf` on `~/Library/Caches/copad/.spawn.lock`, re-probes the socket (another process
/// may have won the race), locates `copadd` (PATH, `~/.local/bin`, `~/.cargo/bin`,
/// `/opt/homebrew/bin`), spawns it detached, then probes `system.ping` for 3s since the daemon
/// may need a moment to listen. Returns true only when the daemon answers ok.
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

        // If a copadd LaunchAgent is loaded, launchd owns the daemon (RunAtLoad + KeepAlive).
        // Self-spawning would race the socket bind and crash-loop the launchd one ("socket
        // already bound by another copadd"), so defer to it. Self-spawn only when no LaunchAgent
        // manages copadd (Homebrew cask, manual run, failed bootstrap).
        if launchAgentLoaded() {
            log("copadd LaunchAgent loaded — deferring to launchd instead of self-spawning")
            return waitForPing(budget: 5.0, perAttempt: 0.3)
        }

        guard let copaddPath = locateBinary() else {
            log("copadd binary not found in PATH, ~/.local/bin, ~/.cargo/bin, or /opt/homebrew/bin — install via the release installer (curl … | bash) or `cargo install --path copad-daemon`")
            return false
        }
        if !spawnDetached(path: copaddPath) {
            return false
        }
        return waitForPing(budget: 3.0, perAttempt: 0.5)
    }

    // MARK: - Helpers

    /// True when the copadd LaunchAgent is loaded (`launchctl print` exits 0), so launchd owns
    /// the daemon lifecycle.
    private static func launchAgentLoaded() -> Bool {
        let proc = Process()
        proc.executableURL = URL(fileURLWithPath: "/bin/launchctl")
        proc.arguments = ["print", "gui/\(getuid())/com.marshall.copad.daemon"]
        proc.standardOutput = FileHandle.nullDevice
        proc.standardError = FileHandle.nullDevice
        do {
            try proc.run()
            proc.waitUntilExit()
            return proc.terminationStatus == 0
        } catch {
            log("launchctl print probe failed: \(error)")
            return false
        }
    }

    private static func locateBinary() -> URL? {
        let env = ProcessInfo.processInfo.environment
        let pathString = env["PATH"] ?? "/usr/local/bin:/usr/bin:/bin"
        var dirs = pathString.split(separator: ":").map { String($0) }
        // A Finder-launched .app does not inherit the shell PATH: add `~/.local/bin` (install.sh),
        // `~/.cargo/bin` (scripts/install-macos.sh) and `/opt/homebrew/bin` (Homebrew cask;
        // `/usr/local/bin` is already in the fallback).
        for extra in ["\(NSHomeDirectory())/.local/bin", "\(NSHomeDirectory())/.cargo/bin", "/opt/homebrew/bin"] {
            if !dirs.contains(extra) { dirs.append(extra) }
        }
        let fm = FileManager.default
        for dir in dirs {
            let candidate = URL(fileURLWithPath: dir).appending(path: "copadd")
            if fm.isExecutableFile(atPath: candidate.path) { return candidate }
        }
        return nil
    }

    /// Detached spawn so the child outlives Copad.app. The child's `COPAD_SOCKET` is set to our
    /// resolved daemon socket, because `CopadPaths.daemonSocket()` ignores legacy per-GUI
    /// overrides and an inherited value could bind a different path than the client probes.
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

        // SO_RCVTIMEO so a wedged daemon cannot block; the caller's loop owns the attempt budget.
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
