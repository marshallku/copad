import Foundation

/// Mirror of `copad-core::event_bus::Origin` (decisions #37). `external` marks events that
/// crossed the daemon's `events.publish` trust boundary. Defaults to `internal` like serde's
/// `#[default]`, so frames lacking the field get the safe value.
enum Origin: String, Codable {
    case `internal`
    case external
}

/// Mirror of `copad-core::event_bus::Event`. `data` is `serde_json::Value`-shaped.
struct BusEvent {
    let type: String
    let source: String
    let data: Any?
    let timestampMs: UInt64
    /// Non-nil for events republished from the daemon side; outbound forwarders skip them to
    /// break echo loops.
    let bridgeId: String?
    /// `external` for events that came through the daemon's `events.publish` ingest, which
    /// `[security] accept_external` gates on.
    let origin: Origin
}

/// Local broadcast hub with JSON-string channels (`SocketServer` wire output,
/// `PluginPanelController` JS bridge) and typed `BusEvent` channels. Per-subscriber FIFO;
/// cross-channel order is undefined because `onBroadcast` fires synchronously and subscribers
/// may broadcast reentrantly.
final class EventBus: @unchecked Sendable {
    private let lock = NSLock()
    private var jsonChannels: [EventChannel] = []
    private var typedChannels: [TypedEventChannel] = []

    /// Synchronous fan-out hook set by AppDelegate to feed the trigger engine. Both `source`
    /// (the trust stamp `try_promote_or_drop_preflight` gates on) and `origin` (consumed by
    /// `[security] accept_external`) MUST be forwarded: defaulting `origin` to `.internal` would
    /// launder daemon-forwarded `External` events into trusted local triggers.
    nonisolated(unsafe) var onBroadcast: (@Sendable (
        _ kind: String,
        _ source: String,
        _ data: Any?,
        _ origin: Origin,
    ) -> Void)?

    func subscribe() -> EventChannel {
        let ch = EventChannel()
        lock.withLock { jsonChannels.append(ch) }
        return ch
    }

    /// `kinds == nil` accepts every event. `broadcast` pre-checks the allowlist to avoid lock
    /// churn; the channel re-checks as a backstop for direct callers.
    func subscribeTyped(kinds: Set<String>? = nil) -> TypedEventChannel {
        let ch = TypedEventChannel(allowlist: kinds)
        lock.withLock { typedChannels.append(ch) }
        return ch
    }

    /// `bridgeId` is set only by inbound daemon-event republish so the outbound forwarder can
    /// skip echoes. The daemon wire `Event` carries no timestamp, so republished events use the
    /// local observation time.
    func broadcast(
        event: String,
        source: String = "macos.eventbus",
        data: Any? = nil,
        bridgeId: String? = nil,
        timestampMs: UInt64? = nil,
        origin: Origin = .internal,
    ) {
        let ts = timestampMs ?? UInt64(Date().timeIntervalSince1970 * 1000)
        let busEvent = BusEvent(
            type: event,
            source: source,
            data: data,
            timestampMs: ts,
            bridgeId: bridgeId,
            origin: origin,
        )

        onBroadcast?(event, source, data, origin)

        // Snapshot under lock so subscribers can reentrantly broadcast
        // without deadlock. Identity-based pruning so a concurrent
        // broadcaster's index-based removal can't drop the wrong channel.
        let (jsonSnap, typedSnap) = lock.withLock { (jsonChannels, typedChannels) }

        // Serialization failure ≠ subscriber death — skip JSON fanout for
        // this one event but keep the subscribers around for future
        // broadcasts. Pruning is driven only by `send` returning false
        // (channel closed).
        var deadJSON: [ObjectIdentifier] = []
        if let line = serializeWire(busEvent) {
            for ch in jsonSnap where !ch.send(line) {
                deadJSON.append(ObjectIdentifier(ch))
            }
        }

        var deadTyped: [ObjectIdentifier] = []
        for ch in typedSnap {
            guard ch.accepts(busEvent.type) else { continue }
            if !ch.send(busEvent) { deadTyped.append(ObjectIdentifier(ch)) }
        }

        if !deadJSON.isEmpty || !deadTyped.isEmpty {
            lock.withLock {
                if !deadJSON.isEmpty {
                    let dead = Set(deadJSON)
                    jsonChannels.removeAll { dead.contains(ObjectIdentifier($0)) }
                }
                if !deadTyped.isEmpty {
                    let dead = Set(deadTyped)
                    typedChannels.removeAll { dead.contains(ObjectIdentifier($0)) }
                }
            }
        }
    }

    /// Local `{"event", "data"}` wire; `.fragmentsAllowed` so scalar/null payloads serialize. The
    /// daemon wire uses `type`/`data`, translated at the DaemonClient boundary.
    private func serializeWire(_ event: BusEvent) -> String? {
        let payload: [String: Any] = ["event": event.type, "data": event.data ?? NSNull()]
        guard
            let data = try? JSONSerialization.data(withJSONObject: payload, options: [.fragmentsAllowed]),
            let s = String(data: data, encoding: .utf8)
        else { return nil }
        return s
    }
}

final class EventChannel: @unchecked Sendable {
    private var queue: [String] = []
    private let sema = DispatchSemaphore(value: 0)
    private let lock = NSLock()
    private var closed = false

    func send(_ event: String) -> Bool {
        lock.lock()
        guard !closed else { lock.unlock(); return false }
        queue.append(event)
        lock.unlock()
        sema.signal()
        return true
    }

    func receive() -> String? {
        sema.wait()
        return lock.withLock {
            if closed, queue.isEmpty { return nil }
            return queue.isEmpty ? nil : queue.removeFirst()
        }
    }

    func close() {
        lock.withLock { closed = true }
        sema.signal()
    }
}

/// Single-subscriber FIFO queue for typed `BusEvent` with optional kind allowlist. A filtered
/// event returns `true` from `send` (silent drop); `false` means `closed`, so EventBus doesn't
/// prune a channel just because an event didn't match its filter.
final class TypedEventChannel: @unchecked Sendable {
    let allowlist: Set<String>?

    private var queue: [BusEvent] = []
    private let sema = DispatchSemaphore(value: 0)
    private let lock = NSLock()
    private var closed = false

    init(allowlist: Set<String>? = nil) {
        self.allowlist = allowlist
    }

    func accepts(_ kind: String) -> Bool {
        allowlist == nil || allowlist!.contains(kind)
    }

    func send(_ event: BusEvent) -> Bool {
        guard accepts(event.type) else { return true }
        lock.lock()
        guard !closed else { lock.unlock(); return false }
        queue.append(event)
        lock.unlock()
        sema.signal()
        return true
    }

    func receive() -> BusEvent? {
        sema.wait()
        return lock.withLock {
            if closed, queue.isEmpty { return nil }
            return queue.isEmpty ? nil : queue.removeFirst()
        }
    }

    /// Drops queued backlog and makes receive() return nil; events stale at disconnect are dead.
    func close() {
        lock.lock()
        closed = true
        queue.removeAll()
        lock.unlock()
        sema.signal()
    }
}
