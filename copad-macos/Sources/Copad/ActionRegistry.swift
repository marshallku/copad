import Foundation

/// Mirrors `copad-core::action_registry::ActionRegistry`. `AppDelegate.handleCommand` tries it
/// first and falls through to the legacy switch on a miss. With an `EventBus` wired, every
/// dispatched action broadcasts `<method>.completed` / `<method>.failed` BEFORE the caller's
/// completion fires, so triggers chaining on `<action>.completed` compose as on Linux. Legacy
/// match-arm actions (`webview.*`, `terminal.*`, `tab.*`) bypass the registry and emit nothing.
///
/// `@MainActor`, but the completion wrapper may fire on any thread (plugin reader thread);
/// that is safe because `EventBus.broadcast` is lock-protected and the Rust engine has its own
/// `RwLock`.
///
/// Not ported from Linux: `register_blocking` (the socket thread already waits on a semaphore
/// for the main-actor completion; a worker per dispatch would risk deadlock once triggers
/// re-enter) and sync `invoke`/`try_invoke`.
@MainActor
final class ActionRegistry {
    /// `completion` MUST be called exactly once, with the success result or an `RPCError` for a
    /// JSON-RPC error envelope. Twice races with the socket-server semaphore signal; zero times
    /// hangs the calling client forever.
    typealias Handler = (_ params: [String: Any], _ completion: @escaping (Any?) -> Void) -> Void

    private struct Entry {
        let handler: Handler
        /// Skip `<name>.completed` / `<name>.failed` broadcast, for high-frequency introspection
        /// actions (`system.ping`, `system.list_actions`).
        let silent: Bool
    }

    private var entries: [String: Entry] = [:]

    /// nil until `setEventBus(_:)` runs (AppDelegate does it at launch); the wrapper no-ops when nil.
    private weak var eventBus: EventBus?

    /// Idempotent; last writer wins. Must run before the first non-silent dispatch or that
    /// dispatch's completion event is dropped.
    func setEventBus(_ bus: EventBus) {
        eventBus = bus
    }

    /// Last writer wins, as on Linux. macOS has no supervisor-level conflict detection (Linux
    /// does it in `resolve_provides`), so a plugin can silently overwrite another's name.
    func register(_ name: String, handler: @escaping Handler) {
        entries[name] = Entry(handler: handler, silent: false)
    }

    /// Like `register` but suppresses completion-event broadcast, for polled introspection
    /// actions (`system.list_actions`, `system.ping`) that would flood the bus.
    func registerSilent(_ name: String, handler: @escaping Handler) {
        entries[name] = Entry(handler: handler, silent: true)
    }

    /// Dispatches `method` and returns `true` if a handler is registered. On a miss it returns
    /// `false` WITHOUT touching `completion`; the caller owns the fall-through.
    ///
    /// The handler's completion is wrapped: success broadcasts `<method>.completed` (non-dict
    /// values as `["value": value]`), an `RPCError` broadcasts `<method>.failed` with
    /// `{code, message}`. The broadcast happens BEFORE the original completion so a chained
    /// trigger runs in the same logical tick as the response.
    @discardableResult
    func tryDispatch(
        _ method: String,
        params: [String: Any],
        silentCompletion: Bool = false,
        completion: @escaping (Any?) -> Void,
    ) -> Bool {
        guard let entry = entries[method] else { return false }
        // `silentCompletion` mirrors Linux `gui_client.rs`'s
        // `silent_completion: true` — daemon-routed invokes already publish
        // `<method>.completed` daemon-side, so a local republish would
        // double-fire once the event bridge lands.
        let bus = (entry.silent || silentCompletion) ? nil : eventBus
        let wrapped: (Any?) -> Void = { [weak bus] value in
            if let bus {
                Self.publishCompletion(bus: bus, method: method, value: value)
            }
            completion(value)
        }
        entry.handler(params, wrapped)
        return true
    }

    /// Like `tryDispatch`, but routes misses through the fallback handler (`setFallbackHandler`),
    /// and completes with `RPCError(unknown_method, ...)` when neither matches. `SocketServer`
    /// keeps its own legacy-switch path and calls the fallback only at its default arm.
    func tryDispatchOrFallback(
        _ method: String,
        params: [String: Any],
        completion: @escaping (Any?) -> Void,
    ) {
        if tryDispatch(method, params: params, completion: completion) { return }
        if let fallback = fallbackHandler {
            fallback(method, params, completion)
        } else {
            completion(RPCError(code: "unknown_method", message: "no handler for \(method)"))
        }
    }

    func setFallbackHandler(_ handler: @escaping FallbackHandler) {
        fallbackHandler = handler
    }

    typealias FallbackHandler = (String, [String: Any], @escaping (Any?) -> Void) -> Void
    private var fallbackHandler: FallbackHandler?

    func has(_ name: String) -> Bool {
        entries[name] != nil
    }

    /// Sorted alphabetically so consumers can diff successive snapshots.
    func names() -> [String] {
        entries.keys.sorted()
    }

    var count: Int {
        entries.count
    }

    /// Translate a completed dispatch into a bus event, as Linux's `publish_completion`:
    /// `<action>.completed` on success, `<action>.failed` (`{code, message}`) on `RPCError`.
    /// A non-dict result is wrapped as `["value": result]` so triggers can address
    /// `{event.value}`. The source stamp `COMPLETION_EVENT_SOURCE` must match
    /// `copad-core/src/action_registry.rs`: the trigger engine gates await-chain promotion on
    /// it, and anything else stalls chains silently. `nonisolated` because the completion may
    /// fire on a plugin reader thread; without it Swift 6 would infer main-actor isolation.
    private nonisolated static func publishCompletion(bus: EventBus, method: String, value: Any?) {
        let kind: String
        let data: [String: Any]
        if let err = value as? RPCError {
            kind = "\(method).failed"
            data = ["code": err.code, "message": err.message]
        } else {
            kind = "\(method).completed"
            switch value {
            case let dict as [String: Any]:
                data = dict
            case .none:
                data = [:]
            case let .some(other):
                data = ["value": other]
            }
        }
        bus.broadcast(event: kind, source: completionEventSource, data: data)
    }

    /// Mirror of `copad_core::action_registry::COMPLETION_EVENT_SOURCE`, hand-kept in sync: the
    /// trust-boundary marker for await promotion. `nonisolated` so `publishCompletion` can read
    /// it off-main.
    nonisolated static let completionEventSource = "copad.action"
}
