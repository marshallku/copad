import Foundation

/// Mirror of `copad-core::event_bus::Origin`. Trust-boundary provenance.
/// `external` is set on events that crossed the daemon's `events.publish`
/// trust-boundary chokepoint; the trigger engine's `[security]
/// accept_external` gate runs on this field. Default `internal` matches
/// serde's `#[default]` so wire frames lacking the field deserialize as
/// the safe default. See decisions.md #37.
enum Origin: String, Codable {
    case `internal`
    case external
}

/// Mirror of `copad-core::event_bus::Event`. `data` is `serde_json::Value`-
/// shaped — object, array, scalar, or null all valid.
struct BusEvent {
    let type: String
    let source: String
    let data: Any?
    let timestampMs: UInt64
    /// nil for native broadcasts; non-nil for events republished from the
    /// daemon side. Outbound forwarders (PR 4b) skip events with bridgeId
    /// set to break echo loops.
    let bridgeId: String?
    /// Trust-boundary tag — `external` for events that came through the
    /// daemon's `events.publish` ingest; `internal` for everything else.
    /// Surfaces to subscribers (e.g. trigger engine) that gate on
    /// `[security] accept_external`. See `Origin` above.
    let origin: Origin
}

/// Local broadcast hub. Subscribers fall into two flavors:
/// - JSON-string channels for legacy consumers (`SocketServer` wire output,
///   `PluginPanelController` JS bridge) — wire shape `{"event", "data"}`.
/// - Typed `BusEvent` channels for consumers that need metadata (PR 4b
///   outbound forwarder gates on `bridgeId`).
///
/// Per-subscriber FIFO; cross-channel order undefined because `onBroadcast`
/// fires synchronously and subscribers may chain reentrant broadcasts.
final class EventBus: @unchecked Sendable {
    private let lock = NSLock()
    private var jsonChannels: [EventChannel] = []
    private var typedChannels: [TypedEventChannel] = []

    /// Synchronous fan-out hook. Set by AppDelegate to forward every event
    /// into the trigger engine without making EventBus aware of `CopadEngine`.
    /// `source` carries the trust stamp the engine's
    /// `try_promote_or_drop_preflight` gates on (`COMPLETION_EVENT_SOURCE` =
    /// `"copad.action"`); `origin` carries the trust-boundary tag the
    /// engine's `[security] accept_external` gate consumes. Both MUST be
    /// forwarded — defaulting `origin` to `.internal` here would launder
    /// daemon-forwarded `External` events into trusted local triggers.
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

    /// `kinds == nil` accepts every event; otherwise only events whose
    /// `type` is in the allowlist enqueue. Mirror of Linux's per-kind
    /// `subscribe_unbounded`. `EventBus.broadcast` pre-checks the allowlist
    /// to avoid lock churn for filtered drops; the channel internal also
    /// re-checks as a backstop against direct callers.
    func subscribeTyped(kinds: Set<String>? = nil) -> TypedEventChannel {
        let ch = TypedEventChannel(allowlist: kinds)
        lock.withLock { typedChannels.append(ch) }
        return ch
    }

    /// Broadcast an event to all live subscribers.
    ///
    /// `bridgeId` is nil for native broadcasts; only set by inbound
    /// daemon-event republish so PR 4b's forwarder can skip echoes.
    /// `timestampMs` defaults to `now`. Daemon wire `Event` does not carry
    /// a timestamp, so republished events also use the local observation
    /// time.
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

        // Engine hop first so a chained broadcast from a trigger callback
        // lands in the same logical tick.
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
            // Pre-check skips enqueue + lock for filtered events (e.g.
            // forwarder channel that excludes terminal.output).
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

    /// `{"event", "data"}` wire (local format). `.fragmentsAllowed` so
    /// scalar / null payloads serialize. Daemon wire uses `type`/`data`;
    /// translation happens at the DaemonClient boundary.
    private func serializeWire(_ event: BusEvent) -> String? {
        let payload: [String: Any] = ["event": event.type, "data": event.data ?? NSNull()]
        guard
            let data = try? JSONSerialization.data(withJSONObject: payload, options: [.fragmentsAllowed]),
            let s = String(data: data, encoding: .utf8)
        else { return nil }
        return s
    }
}

/// Single-subscriber FIFO queue for serialized JSON.
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

/// Single-subscriber FIFO queue for typed `BusEvent` with optional kind
/// allowlist. Allowlist-filtered events return `true` from `send` (silent
/// drop) — `false` is reserved for `closed` so EventBus doesn't prune a
/// channel just because the event didn't match its filter.
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

    /// Drops queued backlog and signals receive() to return nil. Drop guard
    /// for forwarder shutdown — stale events at disconnect are conceptually
    /// dead, daemon will see the next event after reconnect.
    func close() {
        lock.lock()
        closed = true
        queue.removeAll()
        lock.unlock()
        sema.signal()
    }
}
