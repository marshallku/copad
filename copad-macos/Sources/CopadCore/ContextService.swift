import Foundation

/// Swift mirror of `copad_core::context::PaneContext` (the Phase 22.1 local
/// context-bridge payload). All fields are non-optional `String` — missing
/// or undeterminable values are empty strings `""`, matching the Rust
/// `#[serde(default)]` per-field behavior so round-trips are clean.
///
/// Trust note (decision #46): payloads originate from
/// `coctl event publish pane.context_changed '<json>'` and are stamped
/// `Origin::External` by the daemon. Any same-UID process on the
/// workstation can publish with an arbitrary `panel_id`; treat as
/// best-effort display data, not authoritative state.
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

    /// Parse a `serde_json::Value`-shaped dict. Forward-compatible:
    /// unknown fields are ignored, missing fields default to empty /
    /// zero (matches Rust `#[serde(default)]`). Returns `nil` when the
    /// payload is structurally unusable; an empty `panel_id` parses to
    /// a non-nil PaneContext (the caller is responsible for dropping it
    /// — see `ContextService.apply` `pane.context_changed` arm).
    public init?(payload: [String: Any]) {
        panelID = (payload["panel_id"] as? String) ?? ""
        host = (payload["host"] as? String) ?? ""
        cwd = (payload["cwd"] as? String) ?? ""
        gitRemote = (payload["git_remote"] as? String) ?? ""
        branch = (payload["branch"] as? String) ?? ""
        tmuxSession = (payload["tmux_session"] as? String) ?? ""
        paneCmd = (payload["pane_cmd"] as? String) ?? ""
        // JSONSerialization decodes JSON numbers as NSNumber — accept
        // any numeric form (`Int`, `Int64`, `NSNumber`, `Double`); cast
        // to Int64. Missing or non-numeric → 0 (parity with Rust default).
        timestampMs = (payload["timestamp_ms"] as? NSNumber)?.int64Value ?? 0
        version = (payload["v"] as? NSNumber)?.uint32Value ?? 0
    }

    /// JSON-friendly form used in `ContextService.snapshot()`. Mirrors
    /// the `PaneContext` serde shape so consumers serializing the
    /// snapshot get the same wire bytes as Linux.
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

/// PR 9 / parity-plan Tier 2.2 — Swift mirror of `copad_core::context::ContextService`.
/// Tracks the user's currently-focused panel + that panel's cwd so trigger
/// interpolation `{context.active_panel}` / `{context.active_cwd}` resolves
/// to live values on macOS the way it already does on Linux.
///
/// Wire-shape parity with Linux's `Context` struct (`copad-core/src/context.rs`):
/// `snapshot()` returns `["active_panel": String?, "active_cwd": String?,
/// "pane_context": [...]?]`. `active_cwd` and `pane_context` are *derived*
/// from per-panel caches keyed by `activePanel`, not stored on Context;
/// that means a `terminal.cwd_changed` / `pane.context_changed` for a
/// non-active panel caches silently and surfaces only when that panel
/// becomes active. Same semantics as Linux.
///
/// Update rules (mirror of `apply_event` in `copad-core/src/context.rs:147`):
/// - `panel.focused` (payload `panel_id`) → set `activePanel`
/// - `panel.exited` (payload `panel_id`) → drop cwd + pane_context entries;
///   if it matched `activePanel`, null that out too
/// - `terminal.cwd_changed` (payload `panel_id`, `cwd`) → cache cwd
///   keyed by `panel_id`
/// - `pane.context_changed` (payload `PaneContext`-shaped dict) → cache
///   the full payload keyed by `panel_id`. Empty / missing `panel_id`
///   drops the event. Trust note: events stamped `Origin::External`
///   (see decision #46) — any same-UID process can publish, so
///   consumers should treat `pane_context` as best-effort display data.
///
/// **Apply-before-dispatch ordering** (codex pressure-test finding): macOS's
/// `EventBus.onBroadcast` fires synchronously per broadcast, BEFORE channel
/// fan-out. If `ContextService` were itself an `EventBus` subscriber, a
/// `panel.focused` trigger condition checking `{context.active_panel}` would
/// resolve to the *previous* panel because the channel hasn't been read yet.
/// Solution: `AppDelegate.onBroadcast` calls `contextService.apply` BEFORE
/// `copadEngine.dispatchEvent`, taking the post-apply snapshot to pass through
/// FFI. This mirrors Linux's `Pump::pump_all` (`copad-linux/src/window.rs:589`)
/// which explicitly "drain context first, then dispatch."
///
/// **No timer.** Linux uses a 100ms GTK timer to drain bounded event-bus
/// channels into ContextService — that's a Linux bus constraint. macOS's
/// `onBroadcast` already fires synchronously per event; polling would be
/// pure cost with no semantic benefit.
///
/// Concurrency: `@unchecked Sendable` + internal NSLock. `onBroadcast` may
/// fire from any thread (plugin reader thread, main, etc.); both `apply` and
/// `snapshot` need to be safe from anywhere. Same posture as `EventBus.swift`.
/// Mirror of `copad_core::context::Presence` — workstation-presence tag
/// surfaced through `Context.presence`. v1 macOS has no idle-detection
/// signal so the value is always `.active`, but the field is tracked +
/// emitted in `snapshot()` so the wire shape matches Rust's. When an
/// idle-detection source (e.g., a future plugin observing
/// `NSWorkspaceWillSleepNotification`) needs to flip it, call
/// `setPresence(_:)`.
public enum Presence: String, Sendable {
    case active
    case away
}

/// Phase 22.3 — Swift mirror of `copad_core::context::ActiveDoc`. Published
/// from the shell's preexec hook when `nvim <path>` is invoked. `path` is
/// relative to the KB root so it can be passed directly to `kb.read`.
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

    /// Apply one bus event to the context. Idempotent. Non-context kinds
    /// and non-dict payloads are silently ignored — bus carries
    /// `serde_json::Value`-shaped payloads (object/array/scalar/null).
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

    /// Cached pane_context for a specific panel id, if any.
    public func paneContext(panelID: String) -> PaneContext? {
        lock.lock()
        defer { lock.unlock() }
        return paneContexts[panelID]
    }

    /// Pane_context of the currently active panel, derived from
    /// `activePanel`. Returns nil when no panel is active OR when the
    /// active panel has not emitted a `pane.context_changed` event yet.
    public func activePaneContext() -> PaneContext? {
        lock.lock()
        defer { lock.unlock() }
        guard let panel = activePanel else { return nil }
        return paneContexts[panel]
    }

    /// Current workstation-presence value. Always `.active` until a
    /// future caller flips it via `setPresence`.
    public func currentPresence() -> Presence {
        lock.lock()
        defer { lock.unlock() }
        return presence
    }

    /// Returns the previous value so callers can decide whether to
    /// broadcast a `presence.changed` event (avoid emitting on no-op,
    /// mirroring Rust `ContextService::set_presence`).
    @discardableResult
    public func setPresence(_ next: Presence) -> Presence {
        lock.lock()
        defer { lock.unlock() }
        let prev = presence
        presence = next
        return prev
    }

    /// Point-in-time snapshot for trigger interpolation + the
    /// `context.snapshot` socket command. Wire-shape parity with Rust
    /// `Context` (`copad-core/src/context.rs:53`):
    /// `{active_panel?, active_cwd?, pane_context?}`. `[String: Any]`
    /// can't carry nil, so missing keys round-trip to `null` via serde
    /// the same way Linux serializes `Option::None`.
    public func snapshot() -> [String: Any] {
        lock.lock()
        defer { lock.unlock() }
        // `presence` always emits — matches Rust `Context.presence` which
        // is `#[serde(default)]` `Presence::Active`, so the wire bytes
        // are identical regardless of whether a setter ever fired. The
        // engine's `Context` deserializer falls back to `Active` for a
        // missing key, but emitting the value keeps the macOS shape in
        // lockstep with Linux for any consumer that introspects it.
        var out: [String: Any] = ["presence": presence.rawValue]
        if let panel = activePanel {
            out["active_panel"] = panel
            // `active_cwd` + `pane_context` are derived from the active
            // panel's per-panel cache. Same semantics as Linux's
            // `ContextService::snapshot`.
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
