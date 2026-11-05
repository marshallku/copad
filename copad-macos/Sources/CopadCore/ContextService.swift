import Foundation

/// Swift mirror of `copad_core::context::PaneContext`. Missing values are empty strings,
/// matching Rust's per-field `#[serde(default)]`.
///
/// Trust (decision #46): payloads come from `coctl event publish pane.context_changed` and are
/// stamped `Origin::External`. Any same-UID process can publish with an arbitrary `panel_id`;
/// treat as best-effort display data.
public struct PaneContext: Equatable, Sendable {
    public let panelID: String
    public let host: String
    public let cwd: String
    public let gitRemote: String
    public let branch: String
    public let tmuxSession: String
    public let paneCmd: String
    public let timestampMs: Int64
    public let version: UInt32

    /// Forward-compatible: unknown fields are ignored, missing ones default to empty/zero
    /// (Rust `#[serde(default)]`). `nil` when structurally unusable. An empty `panel_id` still
    /// parses; `ContextService.apply` drops it.
    public init?(payload: [String: Any]) {
        panelID = (payload["panel_id"] as? String) ?? ""
        host = (payload["host"] as? String) ?? ""
        cwd = (payload["cwd"] as? String) ?? ""
        gitRemote = (payload["git_remote"] as? String) ?? ""
        branch = (payload["branch"] as? String) ?? ""
        tmuxSession = (payload["tmux_session"] as? String) ?? ""
        paneCmd = (payload["pane_cmd"] as? String) ?? ""
        // JSONSerialization yields NSNumber; accept any numeric form, missing or non-numeric is 0.
        timestampMs = (payload["timestamp_ms"] as? NSNumber)?.int64Value ?? 0
        version = (payload["v"] as? NSNumber)?.uint32Value ?? 0
    }

    /// JSON-friendly form for `ContextService.snapshot()`, same wire bytes as Linux.
    public var asDictionary: [String: Any] {
        [
            "panel_id": panelID,
            "host": host,
            "cwd": cwd,
            "git_remote": gitRemote,
            "branch": branch,
            "tmux_session": tmuxSession,
            "pane_cmd": paneCmd,
            "timestamp_ms": timestampMs,
            "v": version,
        ]
    }
}

// ContextService is the Swift mirror of `copad_core::context::ContextService`. `active_cwd` and
// `pane_context` are derived from per-panel caches keyed by `activePanel`, so an event for a
// non-active panel caches silently until that panel becomes active. Update rules mirror
// `apply_event` in `copad-core/src/context.rs`.
//
// `AppDelegate.onBroadcast` MUST call `apply` BEFORE `copadEngine.dispatchEvent`: `onBroadcast`
// fires before channel fan-out, so a `panel.focused` trigger reading `{context.active_panel}`
// would otherwise see the previous panel. No timer is needed (Linux polls only because of its
// bus). Thread-safe via NSLock because `onBroadcast` may fire from any thread.

/// Mirror of `copad_core::context::Presence`. macOS has no idle-detection source, so the value
/// stays `.active` unless a caller flips it with `setPresence(_:)`; it is still emitted in
/// `snapshot()` to match Rust's wire shape.
public enum Presence: String, Sendable {
    case active
    case away
}

/// Mirror of `copad_core::context::ActiveDoc`, published by the shell's preexec hook when
/// `nvim <path>` runs. `path` is relative to the KB root so it can go straight to `kb.read`.
public struct ActiveDoc: Equatable, Sendable {
    public let panelID: String
    public let path: String
    public let timestampMs: Int64
    public let version: UInt32

    public init?(payload: [String: Any]) {
        panelID = (payload["panel_id"] as? String) ?? ""
        path = (payload["path"] as? String) ?? ""
        timestampMs = (payload["timestamp_ms"] as? NSNumber)?.int64Value ?? 0
        version = (payload["v"] as? NSNumber)?.uint32Value ?? 0
    }

    public var asDictionary: [String: Any] {
        [
            "panel_id": panelID,
            "path": path,
            "timestamp_ms": timestampMs,
            "v": version,
        ]
    }
}

public final class ContextService: @unchecked Sendable {
    private let lock = NSLock()
    private var activePanel: String?
    private var panelCwds: [String: String] = [:]
    private var paneContexts: [String: PaneContext] = [:]
    private var activeDocs: [String: ActiveDoc] = [:]
    private var presence: Presence = .active

    public init() {}

    /// Idempotent. Non-context kinds and non-dict payloads are ignored.
    public func apply(eventKind: String, data: Any?) {
        guard let data = data as? [String: Any] else { return }
        switch eventKind {
        case "panel.focused":
            guard let id = data["panel_id"] as? String, !id.isEmpty else { return }
            lock.withLock { activePanel = id }
        case "panel.exited":
            guard let id = data["panel_id"] as? String, !id.isEmpty else { return }
            lock.withLock {
                panelCwds.removeValue(forKey: id)
                paneContexts.removeValue(forKey: id)
                activeDocs.removeValue(forKey: id)
                if activePanel == id {
                    activePanel = nil
                }
            }
        case "terminal.cwd_changed":
            guard let id = data["panel_id"] as? String, !id.isEmpty,
                  let cwd = data["cwd"] as? String, !cwd.isEmpty
            else { return }
            lock.withLock { panelCwds[id] = cwd }
        case "pane.context_changed":
            guard let ctx = PaneContext(payload: data), !ctx.panelID.isEmpty else { return }
            lock.withLock { paneContexts[ctx.panelID] = ctx }
        case "doc.opened":
            guard let doc = ActiveDoc(payload: data),
                  !doc.panelID.isEmpty, !doc.path.isEmpty
            else { return }
            lock.withLock { activeDocs[doc.panelID] = doc }
        default:
            return
        }
    }

    public func activeDoc(panelID: String) -> ActiveDoc? {
        lock.lock()
        defer { lock.unlock() }
        return activeDocs[panelID]
    }

    public func currentActiveDoc() -> ActiveDoc? {
        lock.lock()
        defer { lock.unlock() }
        guard let panel = activePanel else { return nil }
        return activeDocs[panel]
    }

    public func paneContext(panelID: String) -> PaneContext? {
        lock.lock()
        defer { lock.unlock() }
        return paneContexts[panelID]
    }

    /// Nil when no panel is active or it has not emitted `pane.context_changed` yet.
    public func activePaneContext() -> PaneContext? {
        lock.lock()
        defer { lock.unlock() }
        guard let panel = activePanel else { return nil }
        return paneContexts[panel]
    }

    public func currentPresence() -> Presence {
        lock.lock()
        defer { lock.unlock() }
        return presence
    }

    /// Returns the previous value so callers can skip broadcasting `presence.changed` on a no-op.
    @discardableResult
    public func setPresence(_ next: Presence) -> Presence {
        lock.lock()
        defer { lock.unlock() }
        let prev = presence
        presence = next
        return prev
    }

    /// Point-in-time snapshot for trigger interpolation and `context.snapshot`; wire-shape parity
    /// with Rust `Context` (`copad-core/src/context.rs`). Missing keys round-trip to `null`
    /// via serde, as Linux serializes `Option::None`.
    public func snapshot() -> [String: Any] {
        lock.lock()
        defer { lock.unlock() }
        // `presence` always emits so the wire bytes match Rust's `Context` whether or not a
        // setter ever fired.
        var out: [String: Any] = ["presence": presence.rawValue]
        if let panel = activePanel {
            out["active_panel"] = panel
            if let cwd = panelCwds[panel] {
                out["active_cwd"] = cwd
            }
            if let ctx = paneContexts[panel] {
                out["pane_context"] = ctx.asDictionary
            }
            if let doc = activeDocs[panel] {
                out["active_doc"] = doc.asDictionary
            }
        }
        return out
    }
}
