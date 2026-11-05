import AppKit
import Foundation

extension Notification.Name {
    static let terminalTitleChanged = Notification.Name("copad.terminalTitleChanged")
    /// A webview pane's persisted URL or scroll offset is stale. Layout changes were the only
    /// thing that scheduled a session save, so a pane's URL was otherwise saved only if an
    /// unrelated structural change followed.
    static let webviewStateChanged = Notification.Name("copad.webviewStateChanged")
}

@MainActor
protocol CopadPanel: AnyObject {
    var panelID: String { get }

    var view: NSView { get }

    /// External callers focus this instead of `view`, which may be an unfocusable layout
    /// container.
    var focusTarget: NSView { get }

    var currentTitle: String { get }

    func startIfNeeded()

    func applyBackground(path: String, tint: Double, opacity: Double)
    func clearBackground()
    func setTint(_ alpha: Double)

    func removeFromParent()
}

extension CopadPanel {
    var focusTarget: NSView {
        view
    }
}

/// Cmd+= / Cmd+- / Cmd+0 surface. Non-terminal panels don't conform, so the menu items no-op
/// there.
@MainActor
protocol Zoomable {
    func zoomIn()
    func zoomOut()
    func zoomReset()
}

/// Backend-agnostic surface for the `terminal.*` socket methods. `panel as? TerminalCapable ==
/// nil` is how AppDelegate emits `wrong_panel_type`, matching Linux's `as_terminal()` check in
/// copad-linux/src/socket.rs::resolve_terminal.
@MainActor
protocol TerminalCapable: CopadPanel {
    func feedText(_ text: String)
    func execCommand(_ command: String)
    func terminalState() -> [String: Any]
    func readScreen() -> [String: Any]
    func history(lines: Int) -> [String: Any]
    func context(historyLines: Int) -> [String: Any]
    /// Title override set via `tab.rename`; suppresses auto-derived title updates (OSC 0/2).
    var customTitle: String? { get }
    func setCustomTitle(_ title: String)
}
