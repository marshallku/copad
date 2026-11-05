import CCopadFFI
import Foundation

/// Thin Swift facade over the copad-ffi staticlib, so callers never touch C pointers. Rust
/// strings are copied into Swift and freed immediately via `copad_ffi_free_string`, except two
/// that Rust owns and must not be freed: `version()`'s static string and `lastError()`'s
/// thread-local slot.
enum CopadFFI {
    static func version() -> String {
        guard let cstr = copad_ffi_version() else { return "<null>" }
        return String(cString: cstr)
    }

    /// Round-trip a JSON document through Rust, which adds an `echoed_at` epoch-millis field.
    /// Returns nil on failure and writes the FFI error to stderr.
    static func callJSON(_ payload: [String: Any]) -> [String: Any]? {
        guard let inputData = try? JSONSerialization.data(withJSONObject: payload),
              let inputStr = String(data: inputData, encoding: .utf8)
        else {
            FileHandle.standardError.write(Data("[copad-ffi] failed to serialize input JSON\n".utf8))
            return nil
        }

        return inputStr.withCString { (cstr: UnsafePointer<CChar>) -> [String: Any]? in
            guard let resultPtr = copad_ffi_call_json(cstr) else {
                let err = lastError() ?? "<no error message>"
                FileHandle.standardError.write(Data("[copad-ffi] call_json returned NULL: \(err)\n".utf8))
                return nil
            }
            defer { copad_ffi_free_string(resultPtr) }
            let resultStr = String(cString: resultPtr)
            guard let resultData = resultStr.data(using: .utf8),
                  let parsed = try? JSONSerialization.jsonObject(with: resultData) as? [String: Any]
            else {
                FileHandle.standardError.write(Data("[copad-ffi] failed to parse FFI response: \(resultStr)\n".utf8))
                return nil
            }
            return parsed
        }
    }

    static func lastError() -> String? {
        guard let cstr = copad_ffi_last_error() else { return nil }
        return String(cString: cstr)
    }

    /// Smoke-test the link and a basic round-trip at app launch. Logs to stderr and never crashes.
    static func runSmokeTest() {
        FileHandle.standardError.write(Data("[copad-ffi] version = \(version())\n".utf8))
        if let echo = callJSON(["hello": "from swift", "spike": true]) {
            FileHandle.standardError.write(Data("[copad-ffi] echo round-trip = \(echo)\n".utf8))
        }
    }
}

// MARK: - CopadEngine

/// Swift wrapper around the Rust `TriggerEngine` exposed via copad-ffi.
///
/// `init()` creates the engine and retains `self` so the C action callback can cast `user_data`
/// back. `setTriggers` hot-reloads (the engine swaps the list atomically). `dispatchEvent` matches
/// triggers and fires the callback synchronously per match. After `shutdown()` the instance must
/// not be used.
///
/// Not `@MainActor`: EventBus.broadcast fires from any thread (plugin reader threads forward
/// `event.publish`). The Rust engine is thread-safe (internal `RwLock`), and the callback hops to
/// the main actor before touching `ActionRegistry`. `@unchecked Sendable` because the
/// OpaquePointer handle is not Sendable but has a single owner.
final class CopadEngine: @unchecked Sendable {
    private var handle: OpaquePointer?
    /// Set once by AppDelegate right after construction and read from the C callback thread, which
    /// has no Swift closure context; `nonisolated(unsafe)` because no lock is needed.
    nonisolated(unsafe) var actionRegistry: ActionRegistry?

    /// Gates `dispatchEvent`. The daemon-first cut-over sets this to false when `gui.register`
    /// reports `host_triggers=true`, so daemon and local triggers do not double-fire; the drop
    /// guard in `DaemonClient.handleDisconnect` sets it back. Engine state (await preflight)
    /// survives the cycle, unlike `setTriggers([])`. Chained workflows spanning a cut-over are
    /// unsupported.
    private var enabled = true
    private let enableLock = NSLock()

    init() {
        // copad_engine_create returns OpaquePointer? directly; no cast.
        guard let h = copad_engine_create() else {
            FileHandle.standardError.write(Data("[copad-engine] copad_engine_create returned NULL\n".utf8))
            return
        }
        handle = h

        // `user_data` is a retained pointer to self (`passRetained` here, released in `shutdown`),
        // so the engine can dereference it for the instance's lifetime.
        let userData = Unmanaged.passRetained(self).toOpaque()
        copad_engine_set_action_callback(
            handle,
            CopadEngine.actionCallback,
            userData,
        )
    }

    // No deinit: the retained `self` pointer handed to Rust means deinit would only run after
    // `shutdown()`, which AppDelegate must call. A fallback destroy would race that pointer.

    /// Replace the engine's trigger list with dicts shaped like the TOML `[[triggers]]` blocks.
    /// Returns the loaded count, or nil on JSON encoding failure.
    @discardableResult
    func setTriggers(_ triggers: [[String: Any]]) -> Int? {
        guard let handle else { return nil }
        guard let data = try? JSONSerialization.data(withJSONObject: triggers),
              let str = String(data: data, encoding: .utf8)
        else {
            FileHandle.standardError.write(Data("[copad-engine] failed to encode triggers JSON\n".utf8))
            return nil
        }
        return str.withCString { ptr in
            let count = copad_engine_set_triggers(handle, ptr)
            if count < 0 {
                let err = CopadFFI.lastError() ?? "<no message>"
                FileHandle.standardError.write(Data("[copad-engine] setTriggers failed: \(err)\n".utf8))
                return nil
            }
            return Int(count)
        }
    }

    /// Cut-over hook called from `DaemonClient` register / disconnect; safe from any thread.
    func setEnabled(_ value: Bool) {
        enableLock.lock(); defer { enableLock.unlock() }
        enabled = value
    }

    /// `source` crosses the trust boundary gating await-promotion:
    /// `copad_core::trigger::try_promote_or_drop_preflight` rejects completion-suffixed events
    /// whose source is not `COMPLETION_EVENT_SOURCE`. Pass `ContextService.snapshot()` taken after
    /// `apply`-ing the current event, the apply-before-dispatch order of Linux's `Pump::pump_all`.
    @discardableResult
    func dispatchEvent(
        kind: String,
        source: String = "macos.eventbus",
        context: [String: Any]? = nil,
        payload: Any?,
        origin: Origin = .internal,
    ) -> Int {
        enableLock.lock()
        let isEnabled = enabled
        enableLock.unlock()
        guard isEnabled else { return 0 }
        guard let handle else { return 0 }
        // `.fragmentsAllowed` so scalar / array / null payloads serialize.
        let payloadObject: Any = payload ?? NSNull()
        let payloadStr: String = if let data = try? JSONSerialization.data(withJSONObject: payloadObject, options: [.fragmentsAllowed]),
                                    let s = String(data: data, encoding: .utf8)
        {
            s
        } else {
            "null"
        }
        // An empty context becomes NULL, which is cheaper than a "{}" serde round-trip.
        let contextStr: String? = if let ctx = context, !ctx.isEmpty,
                                     let data = try? JSONSerialization.data(withJSONObject: ctx),
                                     let s = String(data: data, encoding: .utf8)
        {
            s
        } else {
            nil
        }
        let originTag: Int32 = (origin == .external) ? 1 : 0
        return kind.withCString { kindPtr in
            source.withCString { sourcePtr in
                payloadStr.withCString { payloadPtr in
                    let dispatch: (UnsafePointer<CChar>?) -> Int32 = { ctxPtr in
                        copad_engine_dispatch_event(
                            handle,
                            kindPtr,
                            sourcePtr,
                            ctxPtr,
                            payloadPtr,
                            originTag,
                        )
                    }
                    let n: Int32 = if let ctx = contextStr {
                        ctx.withCString { dispatch($0) }
                    } else {
                        dispatch(nil)
                    }
                    return n < 0 ? 0 : Int(n)
                }
            }
        }
    }

    var triggerCount: Int {
        guard let handle else { return 0 }
        let n = copad_engine_count_triggers(handle)
        return n < 0 ? 0 : Int(n)
    }

    /// Tear down the engine; the instance is unusable afterwards. Called from
    /// `applicationWillTerminate`.
    func shutdown() {
        guard let handle else { return }
        // Clear the callback first so an in-flight dispatch cannot call a stale pointer.
        copad_engine_set_action_callback(
            handle,
            nil,
            nil,
        )
        // Reclaim the retained self pointer so ARC can finalize the instance.
        Unmanaged<CopadEngine>.passUnretained(self).release()
        copad_engine_destroy(handle)
        self.handle = nil
    }

    // MARK: - C callback trampoline

    /// `@convention(c)`, so it cannot capture context: `user_data` carries the CopadEngine. The
    /// engine calls from whatever thread invoked dispatchEvent; this hops to the main actor before
    /// touching `ActionRegistry`.
    private static let actionCallback: copad_action_callback = { userData, actionPtr, paramsPtr in
        guard let userData, let actionPtr, let paramsPtr else { return }
        // Borrowed: the engine owns the retained pointer, released only by `shutdown`.
        let unmanaged = Unmanaged<CopadEngine>.fromOpaque(userData)
        let engine = unmanaged.takeUnretainedValue()
        let actionName = String(cString: actionPtr)
        let paramsJson = String(cString: paramsPtr)

        // Decode into the dict shape ActionRegistry handlers get from socket dispatch.
        let params: [String: Any] = if let data = paramsJson.data(using: .utf8),
                                       let dict = (try? JSONSerialization.jsonObject(with: data)) as? [String: Any]
        {
            dict
        } else {
            [:]
        }

        // Fire-and-forget, matching the Rust side's `{queued: true}`; ActionRegistry is
        // @MainActor-isolated, hence the hop.
        let registryRef = engine.actionRegistry
        Task { @MainActor in
            guard let registry = registryRef else {
                FileHandle.standardError.write(Data("[copad-engine] action callback fired but no ActionRegistry attached: \(actionName)\n".utf8))
                return
            }
            registry.tryDispatchOrFallback(actionName, params: params) { _ in }
        }
    }
}
