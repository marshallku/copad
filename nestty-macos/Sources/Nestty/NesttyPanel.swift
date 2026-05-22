import AppKit

/// Common interface for all panel types (terminal, webview, …).
/// Both TerminalViewController and WebViewController conform to this.
@MainActor
protocol NesttyPanel: AnyObject {
    /// Stable identifier for this panel (UUID string). Used in event payloads.
    var panelID: String { get }

    /// The root NSView managed by this panel (from NSViewController).
    var view: NSView { get }

    /// The view that should receive keyboard focus. For panels that
    /// wrap their input-handling view in a layout container, this is
    /// the inner view (e.g. the SwiftTerm `TerminalView` or the
    /// alacritty `AlacrittyRenderView`). External callers use this
    /// instead of `view` when calling `makeFirstResponder`, so the
    /// container being unfocusable doesn't break activation.
    var focusTarget: NSView { get }

    /// Title shown in the tab bar.
    var currentTitle: String { get }

    /// Called once after the panel's view is embedded and layout is resolved.
    func startIfNeeded()

    /// Background image (no-op for panels that don't support it).
    func applyBackground(path: String, tint: Double, opacity: Double)
    func clearBackground()
    func setTint(_ alpha: Double)

    /// NSViewController lifecycle (satisfied automatically by subclasses).
    func removeFromParent()
}

extension NesttyPanel {
    /// Default conformance: panels that don't have a separate focus
    /// target (e.g. WebViewController, or terminal panels where the
    /// root view IS the input-handling view) just hand back `view`.
    /// Override on panels whose root view is a layout container that
    /// shouldn't itself receive keyboard focus.
    var focusTarget: NSView {
        view
    }
}

/// Cmd+= / Cmd+- / Cmd+0 surface. Both terminal backends conform;
/// non-terminal panels (WebView, plugin) don't — AppDelegate uses
/// `activePane as? Zoomable` so the menu items become silent no-ops
/// when focus is on a non-terminal pane.
@MainActor
protocol Zoomable {
    func zoomIn()
    func zoomOut()
    func zoomReset()
}
