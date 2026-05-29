import Darwin
import Foundation

/// Daemon client connection. Mirrors `copad-linux/src/gui_client.rs`.
///
/// Wire shapes are defined in `copad-core/src/protocol.rs`:
/// - Outbound: `Request {id, method, params}`
/// - Inbound: `Response {id, ok, result?, error?}`, `Invoke {id, invoke, params}`,
///   `Event {type, data, source?}`. Demuxed by presence of `invoke`/`ok`/`type`.
/// - `_ping` arrives as `Invoke {invoke: "_ping"}`; reply is a normal `Response`.
///
/// `@unchecked Sendable`: synchronous `isConnected` is required by
/// `ActionRegistry` fallback (every dispatch hits it), so an `actor` would
/// force the call site async. State guarded by `stateLock`.
final class DaemonClient: @unchecked Sendable {
    let capabilities: [String]
    let socketURL: URL

    var isConnected: Bool {
        stateLock.lock(); defer { stateLock.unlock() }
        return connected
    }

    /// True only when currently connected AND last ack reported
    /// `host_triggers=true`. Drop guard re-enables the local engine on
    /// disconnect but doesn't clear `lastAck`, so a bare `lastAck.hostTriggers`
    /// check would stay true after disconnect and incorrectly suppress
    /// local-engine-side reload.
    var hostTriggersActive: Bool {
        stateLock.lock(); defer { stateLock.unlock() }
        return connected && (lastAck?.hostTriggers ?? false)
    }

    private(set) var lastAck: RegisterAck?

    /// Forwarder needs subscribe access on the bus, not just publish, so
    /// `EventBus` is constructor-injected (lifecycle is intrinsic to
    /// DaemonClient — wouldn't make sense to swap mid-flight).
    private let eventBus: EventBus

    init(socket: URL, capabilities: [String], eventBus: EventBus) {
        socketURL = socket
        self.capabilities = capabilities
        self.eventBus = eventBus
    }

    /// Called by AppDelegate-side wiring on register / disconnect with the
    /// daemon's `host_triggers` flag. `true` ⇒ disable local engine (daemon
    /// owns trigger firing), `false` ⇒ re-enable local fallback. Invoked
    /// from background threads; callers must be thread-safe (CopadEngine's
    /// `setEnabled` is NSLock-guarded).
    var cutoverHandler: (@Sendable (Bool) -> Void)?

    /// Idempotent. Second call is a no-op.
    func start() {
        stateLock.lock()
        guard !started else { stateLock.unlock(); return }
        started = true
        stateLock.unlock()

        Thread.detachNewThread { [weak self] in
            self?.runForever()
        }
    }

    /// Forward an RPC to the daemon. Completion fires from any thread.
    ///
    /// **Silent** — does NOT trigger `ActionRegistry` completion fan-out;
    /// daemon-side `ActionRegistry` already publishes `<method>.completed`,
    /// and republishing here would double-fire once event bridging lands.
    func forward(method: String, params: [String: Any], completion: @escaping (Any?) -> Void) {
        stateLock.lock()
        guard connected, let writerCh = currentWriter else {
            stateLock.unlock()
            completion(RPCError(code: "daemon_unavailable", message: "daemon not connected"))
            return
        }
        let id = UUID().uuidString
        let gen = generation
        pending[id] = Pending(generation: gen, completion: completion)
        stateLock.unlock()

        timeoutQueue.asyncAfter(deadline: .now() + Self.requestTimeout) { [weak self] in
            self?.timeoutPending(id: id, gen: gen)
        }

        let req: [String: Any] = ["id": id, "method": method, "params": params]
        guard let line = encodeJSONLine(req) else {
            stateLock.lock()
            pending.removeValue(forKey: id)
            stateLock.unlock()
            completion(RPCError(code: "internal_error", message: "encode forward request: \(method)"))
            return
        }
        if !writerCh.send(line) {
            // Drop pending so the caller sees `daemon_unavailable` immediately
            // instead of a misleading 30s timeout for a request the daemon
            // never received.
            stateLock.lock()
            pending.removeValue(forKey: id)
            stateLock.unlock()
            completion(RPCError(code: "daemon_unavailable", message: "writer queue overflow or shutdown (forward of \(method))"))
        }
    }

    private let stateLock = NSLock()
    private var started = false
    private var connected = false
    private var generation: UInt64 = 0
    private var pending: [String: Pending] = [:]
    private var currentWriter: WriterChannel?

    private let timeoutQueue = DispatchQueue(label: "copad.daemonclient.timeout")

    private struct Pending {
        let generation: UInt64
        let completion: (Any?) -> Void
    }

    /// Body executes on main actor; closure ref is Sendable for storage.
    typealias InvokeHandler = @MainActor @Sendable (_ id: String, _ method: String, _ params: [String: Any], _ reply: @Sendable @escaping (String) -> Void) -> Void
    var invokeHandler: InvokeHandler?

    /// Called from the reader thread for every inbound `Event`. AppDelegate
    /// re-broadcasts on the local `EventBus` with a fresh `bridgeId` so PR 4b's
    /// outbound forwarder can echo-skip. `data` mirrors `serde_json::Value`
    /// (object/array/scalar/NSNull); the daemon wire has no `timestamp_ms`,
    /// so the local broadcast assigns one. `origin` carries the daemon-side
    /// trust-boundary tag — `external` for events that crossed the
    /// `events.publish` ingest, `internal` otherwise. The GUI trigger engine's
    /// `[security] accept_external` gate consumes this.
    typealias InboundEventHandler = @Sendable (
        _ type: String,
        _ source: String,
        _ data: Any?,
        _ origin: Origin,
    ) -> Void
    var inboundEventHandler: InboundEventHandler?

    private static let invokeQueueCap = 32 // mirrors Linux POOL_QUEUE
    private var invokeInFlight = 0 // guarded by stateLock

    /// Allowlist of bus event kinds forwarded to daemon via `_bus.publish`.
    /// Mirror of `copad-linux/src/gui_client.rs` `GUI_TO_DAEMON_FORWARD_KINDS`
    /// — keep in sync. `terminal.output` deliberately excluded
    /// (per-keystroke saturation, no trigger value).
    ///
    /// `tab.opened` is a macOS-publish artifact mapped to Linux `tab.created`
    /// at forward time so daemon-side triggers see the canonical name.
    private static let forwardKinds: Set<String> = [
        "pane.context_changed",
        "panel.exited", "panel.focused", "panel.title_changed",
        "tab.closed", "tab.created", "tab.opened", "tab.renamed",
        "terminal.cwd_changed", "terminal.shell_precmd", "terminal.shell_preexec",
        "webview.loaded", "webview.navigated", "webview.title_changed",
        "window.restored",
    ]
    private var forwarderChannel: TypedEventChannel? // per-connection

    /// 1 MiB including the trailing newline (mirror of
    /// `copad-daemon::socket::read_line_capped`).
    private static let maxFrameBytes = 1024 * 1024

    private static let backoffMin: TimeInterval = 0.1
    private static let backoffMax: TimeInterval = 5.0

    /// GUI-side safety belt for the forward path. Daemon owns the
    /// caller-visible per-method timeout
    /// (`gui_registry.rs::method_invoke_timeout`, 5s/120s); this fires
    /// only if the daemon never replies at all.
    private static let requestTimeout: TimeInterval = 30.0

    /// Slot leak protection for an Invoke handler that never completes.
    /// Daemon timeout fires sooner; matches Linux `pump_timeout`.
    private static let invokeHandlerSafetyTimeout: TimeInterval = 125.0

    // MARK: - Run loop

    private func runForever() {
        var backoff = Self.backoffMin
        while true {
            if let session = openConnection() {
                backoff = Self.backoffMin
                runReader(session: session)
                handleDisconnect()
            } else {
                if AutoSpawn.ensureRunning() {
                    log("auto-spawn succeeded — retrying connect immediately")
                    backoff = Self.backoffMin
                    continue
                }
                Thread.sleep(forTimeInterval: backoff)
                backoff = min(backoff * 2, Self.backoffMax)
            }
        }
    }

    private func openConnection() -> ConnectedSession? {
        guard let fd = UnixSocket.connect(path: socketURL.path(percentEncoded: false)) else {
            return nil
        }
        guard let ack = registerOn(fd: fd) else {
            close(fd)
            return nil
        }

        let writer = WriterChannel(fd: fd)
        writer.start()

        stateLock.lock()
        generation &+= 1
        connected = true
        currentWriter = writer
        lastAck = ack
        stateLock.unlock()
        log("registered with copadd: ack=\(ack)")

        // host_triggers cut-over: forwarder FIRST so events between
        // start-forwarder and engine-disable still reach daemon (Linux
        // ordering, gui_client.rs:226). Then disable local engine.
        if ack.hostTriggers {
            stateLock.lock()
            forwarderChannel = startForwarder(writer: writer)
            stateLock.unlock()
            cutoverHandler?(true)
        }

        return ConnectedSession(fd: fd, writer: writer)
    }

    private func startForwarder(writer: WriterChannel) -> TypedEventChannel {
        let channel = eventBus.subscribeTyped(kinds: Self.forwardKinds)
        Thread.detachNewThread { [weak self] in
            while let event = channel.receive() {
                // Echo skip: events republished from daemon carry bridgeId.
                guard event.bridgeId == nil else { continue }
                guard let line = self?.encodeBusPublish(event) else { continue }
                writer.sendBlocking(line)
            }
        }
        return channel
    }

    private func encodeBusPublish(_ event: BusEvent) -> String? {
        // macOS publishes `tab.opened`; Linux daemon expects `tab.created`.
        let kind = (event.type == "tab.opened") ? "tab.created" : event.type
        let req: [String: Any] = [
            "id": UUID().uuidString,
            "method": "_bus.publish",
            "params": [
                "kind": kind,
                "source": event.source,
                "timestamp_ms": event.timestampMs,
                "payload": event.data ?? NSNull(),
            ],
        ]
        return encodeJSONLine(req)
    }

    private func registerOn(fd: Int32) -> RegisterAck? {
        let reqId = UUID().uuidString
        let payload: [String: Any] = [
            "id": reqId,
            "method": "gui.register",
            "params": [
                "window_id": UUID().uuidString,
                "capabilities": capabilities,
                "want_primary": true,
                "version": "0.1.0",
                "protocol_version": 1,
                // Daemon's `GUI_ENV_ALLOWED_KEYS` is currently a Linux/
                // Wayland-only set (HYPRLAND_INSTANCE_SIGNATURE,
                // WAYLAND_DISPLAY, ...). None apply on macOS, so an empty
                // dict is the correct contract until daemon-side allowlist
                // grows macOS keys (separate trust-boundary review).
                "gui_env": [:] as [String: String],
            ],
        ]
        guard let line = encodeJSONLine(payload) else { return nil }
        if !writeLine(fd: fd, line: line) { return nil }

        var tv = timeval(tv_sec: 5, tv_usec: 0)
        _ = setsockopt(fd, SOL_SOCKET, SO_RCVTIMEO, &tv, socklen_t(MemoryLayout<timeval>.size))
        guard let ackLine = readLine(fd: fd) else {
            log("register ack: no response")
            return nil
        }
        guard let json = try? JSONSerialization.jsonObject(with: Data(ackLine.utf8)) as? [String: Any] else {
            log("register ack: malformed JSON: \(ackLine.prefix(200))")
            return nil
        }
        if let ackId = json["id"] as? String, ackId != reqId {
            log("register ack id mismatch: expected \(reqId), got \(ackId)")
            return nil
        }
        if (json["ok"] as? Bool) != true {
            log("register rejected: \(json["error"] ?? "<no error>")")
            return nil
        }
        let result = (json["result"] as? [String: Any]) ?? [:]
        var tv2 = timeval(tv_sec: 0, tv_usec: 0)
        _ = setsockopt(fd, SOL_SOCKET, SO_RCVTIMEO, &tv2, socklen_t(MemoryLayout<timeval>.size))
        return RegisterAck(
            clientId: (result["client_id"] as? String) ?? "",
            primary: (result["primary"] as? Bool) ?? false,
            daemonVersion: (result["daemon_version"] as? String) ?? "",
            protocolVersion: (result["protocol_version"] as? Int) ?? 0,
            hostTriggers: (result["host_triggers"] as? Bool) ?? false,
        )
    }

    private func runReader(session: ConnectedSession) {
        let fd = session.fd
        var buf = Data()
        let chunkSize = 8192
        var chunk = [UInt8](repeating: 0, count: chunkSize)
        outer: while true {
            let n = chunk.withUnsafeMutableBufferPointer { bp -> Int in
                Darwin.read(fd, bp.baseAddress, bp.count)
            }
            if n <= 0 {
                if n < 0 { log("reader read error: \(String(cString: strerror(errno)))") }
                break outer
            }
            buf.append(chunk, count: n)
            // `frameBytes` includes the trailing newline (matches
            // `copad-daemon::socket::read_line_capped`). Only the offending
            // frame closes the connection; valid frames already in the
            // buffer parse normally up to the offender.
            while true {
                if let nlIdx = buf.firstIndex(of: 0x0A) {
                    let frameBytes = nlIdx + 1
                    if frameBytes > Self.maxFrameBytes {
                        log("frame cap exceeded (\(frameBytes) > \(Self.maxFrameBytes)) — closing connection")
                        break outer
                    }
                    let lineData = buf.prefix(nlIdx)
                    buf.removeSubrange(0 ... nlIdx)
                    guard let line = String(data: lineData, encoding: .utf8), !line.trimmingCharacters(in: .whitespaces).isEmpty else { continue }
                    handleInboundLine(line, writer: session.writer)
                } else {
                    if buf.count > Self.maxFrameBytes {
                        log("partial frame exceeds cap (\(buf.count) > \(Self.maxFrameBytes)) — closing connection")
                        break outer
                    }
                    break
                }
            }
        }
        session.writer.shutdown()
        close(fd)
    }

    private func handleInboundLine(_ line: String, writer: WriterChannel) {
        guard let value = try? JSONSerialization.jsonObject(with: Data(line.utf8)) as? [String: Any] else {
            log("malformed line: \(line.prefix(200))")
            return
        }
        if let invokeMethod = value["invoke"] as? String {
            if invokeMethod == "_ping" {
                replyToPing(value: value, writer: writer)
            } else {
                guard let id = value["id"] as? String else {
                    log("Invoke missing id: method=\(invokeMethod) — dropping")
                    return
                }
                let params = (value["params"] as? [String: Any]) ?? [:]
                admitInvoke(id: id, method: invokeMethod, params: params)
            }
            return
        }
        if value["ok"] != nil, let id = value["id"] as? String {
            handleResponse(id: id, value: value)
            return
        }
        if let type = value["type"] as? String {
            let source = (value["source"] as? String) ?? "daemon"
            // Wire shape: `{type, data, source?, origin?}`. Older daemons
            // (pre-protocol-Event-origin) omit `origin`; treat as the safe
            // default `internal` matching serde's `#[default]`. Unknown
            // origin strings also fall through to `internal` rather than
            // refusing the event — drop-on-unknown would silently lose
            // events at a protocol bump.
            let origin: Origin = (value["origin"] as? String)
                .flatMap(Origin.init(rawValue:)) ?? .internal
            // `data` may be NSNull / array / scalar / dict — pass through
            // verbatim. AppDelegate's handler stamps a fresh bridge_id and
            // republishes onto local EventBus.
            inboundEventHandler?(type, source, value["data"], origin)
            return
        }
        log("ignoring unknown line: \(line.prefix(200))")
    }

    private func replyToPing(value: [String: Any], writer: WriterChannel) {
        guard let id = value["id"] as? String else { return }
        let params = value["params"] ?? [:]
        let response: [String: Any] = ["id": id, "ok": true, "result": params]
        guard let line = encodeJSONLine(response) else { return }
        writer.sendControl(line) // priority lane — bypasses bounded forward queue
    }

    /// Admit an inbound Invoke through a bounded gate, then dispatch on the
    /// main actor. Saturation and stale-generation invokes both reply
    /// `overloaded` so the daemon completes its pending entry without
    /// waiting for its own per-method timeout.
    ///
    /// The admission slot holds until the **response closure fires**, not
    /// when the Task body returns: async handlers (`webview.execute_js`,
    /// `claude.start`, `webview.screenshot`) complete later than the Task
    /// closure exits, so deferring slot release on Task return would defeat
    /// the bound. `SlotGuard.releaseOnce()` makes every terminal path
    /// (response / missing handler / stale-gen / safety net) idempotent.
    private func admitInvoke(id: String, method: String, params: [String: Any]) {
        stateLock.lock()
        let admittedGen = generation
        let writerSnapshot = currentWriter
        if invokeInFlight >= Self.invokeQueueCap {
            stateLock.unlock()
            sendInvokeError(id: id, code: "overloaded", message: "GUI invoke queue saturated", writer: writerSnapshot)
            return
        }
        invokeInFlight += 1
        stateLock.unlock()

        let slot = SlotGuard()
        let releaseSlot: @Sendable () -> Void = { [weak self] in
            guard slot.releaseOnce() else { return }
            self?.stateLock.lock()
            if let s = self { s.invokeInFlight = max(0, s.invokeInFlight - 1) }
            self?.stateLock.unlock()
        }

        // Slot leak guard: if a handler never completes (a buggy callback
        // that drops its completion), this fires after daemon-side timeout
        // has already given up. Late replies racing this no-op since
        // writer.send on a shut-down channel returns false anyway.
        timeoutQueue.asyncAfter(deadline: .now() + Self.invokeHandlerSafetyTimeout) {
            releaseSlot()
        }

        // params + writer cross the @MainActor hop without being mutated on
        // either side; SendableBox suppresses the strict-concurrency warning.
        let paramsBox = SendableBox(params)
        let writerBox = SendableBox(writerSnapshot)
        Task { @MainActor [weak self] in
            guard let self else { releaseSlot(); return }
            let curGen: UInt64 = {
                self.stateLock.lock(); defer { self.stateLock.unlock() }
                return self.generation
            }()
            if curGen != admittedGen {
                sendInvokeError(id: id, code: "overloaded", message: "GUI generation moved before dispatch", writer: writerBox.value)
                releaseSlot()
                return
            }
            guard let handler = invokeHandler else {
                sendInvokeError(id: id, code: "internal_error", message: "DaemonClient invokeHandler not set", writer: writerBox.value)
                releaseSlot()
                return
            }
            handler(id, method, paramsBox.value) { responseLine in
                _ = writerBox.value?.send(responseLine)
                releaseSlot()
            }
        }
    }

    /// Build + send an error Response for failures inside the Invoke admission
    /// `writer` is nil when the connection died between admission and the
    /// failure observation — daemon already failed its pending entry on
    /// disconnect, so silent drop is correct.
    private func sendInvokeError(id: String, code: String, message: String, writer: WriterChannel?) {
        guard let writer else { return }
        let resp: [String: Any] = [
            "id": id,
            "ok": false,
            "error": ["code": code, "message": message],
        ]
        guard let line = encodeJSONLine(resp) else { return }
        _ = writer.send(line)
    }

    private func handleResponse(id: String, value: [String: Any]) {
        stateLock.lock()
        guard let entry = pending.removeValue(forKey: id) else {
            stateLock.unlock()
            return
        }
        let curGen = generation
        stateLock.unlock()

        // Stale generation: connection was recycled before this reply
        // arrived. Caller already got daemon_unavailable on disconnect.
        if entry.generation != curGen { return }

        let ok = (value["ok"] as? Bool) ?? false
        if ok {
            entry.completion(value["result"] ?? [:])
        } else if let err = value["error"] as? [String: Any] {
            let code = (err["code"] as? String) ?? "unknown"
            let msg = (err["message"] as? String) ?? ""
            entry.completion(RPCError(code: code, message: msg))
        } else {
            entry.completion(RPCError(code: "malformed_response", message: "no ok field"))
        }
    }

    private func handleDisconnect() {
        // Flip `connected = false` BEFORE notifying the cutover handler so
        // any concurrent `hostTriggersActive` reader (e.g. ConfigWatcher
        // racing with disconnect) sees the new state. Otherwise local
        // trigger reload would skip during the gap between cutover restore
        // and the connected flip, leaving the engine running stale config.
        // Bump generation here too so a stale reply racing in is dropped
        // by handleResponse's gen check.
        stateLock.lock()
        let hadForwarder = forwarderChannel != nil
        connected = false
        generation &+= 1
        let drained = pending
        pending.removeAll()
        currentWriter?.shutdown()
        currentWriter = nil
        let closingForwarder = forwarderChannel
        forwarderChannel = nil
        stateLock.unlock()

        // Drop guard: re-enable local engine so GUI-only triggers keep
        // firing through the reconnect-backoff window. Mirror of Linux
        // HostTriggersGuard Drop impl.
        if hadForwarder {
            cutoverHandler?(false)
        }
        closingForwarder?.close()

        for (_, entry) in drained {
            entry.completion(RPCError(code: "daemon_unavailable", message: "daemon disconnected mid-flight"))
        }
        log("disconnected — drained \(drained.count) pending forward(s), forwarder=\(hadForwarder)")
    }

    private func timeoutPending(id: String, gen: UInt64) {
        stateLock.lock()
        guard let entry = pending[id], entry.generation == gen else {
            stateLock.unlock()
            return
        }
        pending.removeValue(forKey: id)
        stateLock.unlock()
        entry.completion(RPCError(code: "timeout", message: "daemon did not respond in \(Self.requestTimeout)s"))
    }

    // MARK: - Socket helpers (synchronous, blocking)

    private func writeLine(fd: Int32, line: String) -> Bool {
        let withNl = line.hasSuffix("\n") ? line : line + "\n"
        let bytes = Array(withNl.utf8)
        var written = 0
        while written < bytes.count {
            let n = bytes.withUnsafeBufferPointer { bp -> Int in
                Darwin.write(fd, bp.baseAddress!.advanced(by: written), bp.count - written)
            }
            if n <= 0 { return false }
            written += n
        }
        return true
    }

    /// Single-line blocking byte-by-byte read used only by the synchronous
    /// register-ack handshake. Cap semantics match the runReader loop and
    /// `copad-daemon::socket::read_line_capped`: `maxFrameBytes` includes
    /// the trailing newline. Returns nil on EOF, error, or oversized frame.
    private func readLine(fd: Int32) -> String? {
        var buf = Data()
        var byte: UInt8 = 0
        while true {
            if buf.count >= Self.maxFrameBytes {
                log("registerOn: read frame exceeded cap (\(buf.count) >= \(Self.maxFrameBytes)) — aborting register")
                return nil
            }
            let n = withUnsafeMutablePointer(to: &byte) { p -> Int in
                Darwin.read(fd, p, 1)
            }
            if n <= 0 { return nil }
            if byte == 0x0A { break }
            buf.append(byte)
        }
        return String(data: buf, encoding: .utf8)
    }

    private func encodeJSONLine(_ obj: [String: Any]) -> String? {
        guard let data = try? JSONSerialization.data(withJSONObject: obj, options: []),
              let s = String(data: data, encoding: .utf8) else { return nil }
        return s + "\n"
    }

    private func log(_ msg: String) {
        FileHandle.standardError.write(Data("[copad-daemonclient] \(msg)\n".utf8))
    }
}

// MARK: - Supporting types

/// Captured fields from the daemon's `gui.register` ack.
struct RegisterAck {
    let clientId: String
    let primary: Bool
    let daemonVersion: String
    let protocolVersion: Int
    let hostTriggers: Bool
}

/// Live socket-connection bundle for the GUI ↔ daemon connection.
/// Renamed from `Session` to avoid clashing with the module-level
/// `enum Session` introduced for tab-layout persistence.
private struct ConnectedSession {
    let fd: Int32
    let writer: WriterChannel
}

/// `releaseOnce()` returns `true` exactly once across concurrent calls.
/// Used to make Invoke admission slot decrement idempotent across racing
/// terminal paths (handler completion / safety-timeout / error early-out).
final class SlotGuard: @unchecked Sendable {
    private let lock = NSLock()
    private var released = false

    func releaseOnce() -> Bool {
        lock.lock(); defer { lock.unlock() }
        if released { return false }
        released = true
        return true
    }
}

/// Bounded normal-forward queue + unbounded control queue. Control frames
/// (heartbeat replies) drain ahead of normal forwards so a saturated normal
/// queue can't starve heartbeats and trigger a daemon-side disconnect.
///
/// `@unchecked Sendable`: NSCondition serializes mutations; drain thread
/// is the sole owner of `fd` writes.
final class WriterChannel: @unchecked Sendable {
    private let fd: Int32
    private let cond: NSCondition
    private var control: [String] = []
    private var normal: [String] = []
    private var stopped = false
    private static let normalCap = 256

    init(fd: Int32) {
        self.fd = fd
        cond = NSCondition()
    }

    func start() {
        Thread.detachNewThread { [weak self] in
            self?.runDrain()
        }
    }

    /// Returns `false` on overflow OR when the channel has been shut down —
    /// caller must fail-complete the corresponding pending entry. Silently
    /// dropping the oldest frame would let the caller wait for a reply the
    /// daemon never received.
    func send(_ line: String) -> Bool {
        cond.lock()
        defer { cond.unlock() }
        if stopped { return false }
        if normal.count >= Self.normalCap { return false }
        normal.append(line)
        cond.signal()
        return true
    }

    /// Blocks until the queue has capacity OR the channel is shut down.
    /// Used by the daemon-event forwarder so saturation back-pressures the
    /// bus rather than silently dropping events the daemon needs.
    func sendBlocking(_ line: String) {
        cond.lock()
        defer { cond.unlock() }
        while !stopped, normal.count >= Self.normalCap {
            cond.wait()
        }
        if stopped { return }
        normal.append(line)
        cond.signal()
    }

    /// Control frames (heartbeat replies). Tiny in practice (one `_ping` per
    /// daemon heartbeat interval), so unbounded is safe.
    func sendControl(_ line: String) {
        cond.lock()
        control.append(line)
        cond.signal()
        cond.unlock()
    }

    /// Wakes both the drain thread and any blocked `sendBlocking` callers
    /// so they exit via the `stopped` check.
    func shutdown() {
        cond.lock()
        stopped = true
        cond.broadcast()
        cond.unlock()
    }

    private func runDrain() {
        while true {
            cond.lock()
            while !stopped, control.isEmpty, normal.isEmpty {
                cond.wait()
            }
            if stopped, control.isEmpty, normal.isEmpty {
                cond.unlock()
                return
            }
            // Wake `sendBlocking` only when the *normal* queue crossed
            // from full to not-full — signaling on control-only dequeue or
            // every normal dequeue would burn cycles.
            let normalWasFull = normal.count >= Self.normalCap
            let next: String?
            let normalDequeued: Bool
            if !control.isEmpty {
                next = control.removeFirst()
                normalDequeued = false
            } else if !normal.isEmpty {
                next = normal.removeFirst()
                normalDequeued = true
            } else {
                next = nil
                normalDequeued = false
            }
            if normalWasFull, normalDequeued {
                cond.signal()
            }
            cond.unlock()

            guard let line = next else { continue }
            let withNl = line.hasSuffix("\n") ? line : line + "\n"
            let bytes = Array(withNl.utf8)
            var written = 0
            var failed = false
            while written < bytes.count {
                let n = bytes.withUnsafeBufferPointer { bp -> Int in
                    Darwin.write(fd, bp.baseAddress!.advanced(by: written), bp.count - written)
                }
                if n <= 0 { failed = true; break }
                written += n
            }
            if failed {
                // Reader thread will observe disconnect and trigger drain;
                // we just stop the writer.
                return
            }
        }
    }
}
