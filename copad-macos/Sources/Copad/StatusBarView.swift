import AppKit
import Foundation

/// Tier 4.2 — Waybar-style status bar that mirrors `copad-linux/src/statusbar.rs`
/// in shape:
///
/// - 3 zones (left/center/right) laid out horizontally
/// - per-module `NSTextField` label, sorted by `order` within zone
/// - each module runs a shell command on a `DispatchSourceTimer`
/// - stdout parsed as either plain text or JSON `{text, tooltip}`
///
/// macOS-specific simplifications vs Linux:
///
/// - No CSS hot-reload. Theme colors applied once at view-build time. If the
///   user changes themes the bar will pick up new colors on `applyTheme`
///   (called by AppDelegate's hot-reload path; not yet wired).
/// - Position support is `bottom` only. Linux supports both — top requires
///   reshuffling around the tab bar layout we already do in `TabViewController`.
///   Defer until somebody asks.
/// - Module CSS class (`module.class`) ignored. Linux uses GTK CSS to scope
///   per-module styling; we don't have a clean macOS equivalent without
///   reaching for `NSAttributedString` per module. Module authors that want
///   color cues today should JSON-emit the text with markup or unicode
///   indicators instead.
@MainActor
final class StatusBarView: NSView {
    private let leftStack = NSStackView()
    private let centerStack = NSStackView()
    private let rightStack = NSStackView()
    private var runners: [StatusModuleRunner] = []
    /// Map from `<plugin>.<module>` → label so future event-driven updates
    /// (push from plugin instead of poll) can target a specific module.
    /// Not used yet; kept so the API doesn't have to change later.
    private var labels: [String: NSTextField] = [:]

    /// Backing store for `isHidden` so `statusbar.show/hide/toggle` reads
    /// stay consistent with what we set, even if AppKit's accessor races
    /// across animation states.
    private(set) var isShown: Bool = true

    private var theme: CopadTheme
    /// `[window] opacity` mirrored here so the bar bg blends with the
    /// transparent window. Updated via `applyWindowOpacity` on hot-reload.
    private var windowOpacity: Double

    init(theme: CopadTheme, windowOpacity: Double = 1.0) {
        self.theme = theme
        self.windowOpacity = windowOpacity
        super.init(frame: .zero)
        translatesAutoresizingMaskIntoConstraints = false
        wantsLayer = true
        layer?.backgroundColor = Self.barBg(theme: theme, opacity: windowOpacity)
        // 1px top edge so the bar visibly separates from the content above
        // even when the surface0/background contrast is low (Catppuccin
        // Mocha they're nearly the same shade).
        let separator = NSView()
        separator.translatesAutoresizingMaskIntoConstraints = false
        separator.wantsLayer = true
        separator.layer?.backgroundColor = theme.overlay0.nsColor.cgColor
        addSubview(separator)
        NSLayoutConstraint.activate([
            separator.topAnchor.constraint(equalTo: topAnchor),
            separator.leadingAnchor.constraint(equalTo: leadingAnchor),
            separator.trailingAnchor.constraint(equalTo: trailingAnchor),
            separator.heightAnchor.constraint(equalToConstant: 1),
        ])

        for stack in [leftStack, centerStack, rightStack] {
            stack.orientation = .horizontal
            stack.spacing = 12
            stack.translatesAutoresizingMaskIntoConstraints = false
            addSubview(stack)
        }

        // Three zones laid out in a row: left flush-left, center centered,
        // right flush-right. CenterX anchor on the center stack pins it
        // even as the side stacks grow/shrink with content.
        NSLayoutConstraint.activate([
            leftStack.leadingAnchor.constraint(equalTo: leadingAnchor, constant: 12),
            leftStack.centerYAnchor.constraint(equalTo: centerYAnchor),
            centerStack.centerXAnchor.constraint(equalTo: centerXAnchor),
            centerStack.centerYAnchor.constraint(equalTo: centerYAnchor),
            rightStack.trailingAnchor.constraint(equalTo: trailingAnchor, constant: -12),
            rightStack.centerYAnchor.constraint(equalTo: centerYAnchor),
        ])
    }

    @available(*, unavailable)
    required init?(coder _: NSCoder) {
        fatalError()
    }

    /// Build module labels + start their runners. Called once at app launch
    /// from `TabViewController.loadView`. Each `LoadedPluginManifest` may
    /// contribute zero or more modules.
    func loadModules(_ plugins: [LoadedPluginManifest], daemonClient: DaemonClient) {
        var byZone: [String: [(plugin: LoadedPluginManifest, module: PluginModuleDef)]] = [
            "left": [], "center": [], "right": [],
        ]
        for plugin in plugins {
            for module in plugin.manifest.modules {
                let zone = ["left", "center", "right"].contains(module.position) ? module.position : "right"
                byZone[zone, default: []].append((plugin, module))
            }
        }
        for zone in ["left", "center", "right"] {
            let entries = byZone[zone, default: []].sorted { $0.module.order < $1.module.order }
            let stack = stackForZone(zone)
            for (plugin, module) in entries {
                let label = NSTextField(labelWithString: "...")
                label.textColor = theme.text.nsColor
                label.font = .systemFont(ofSize: 12)
                label.alignment = .center
                stack.addArrangedSubview(label)
                let key = "\(plugin.manifest.plugin.name).\(module.name)"
                labels[key] = label
                let runner = StatusModuleRunner(
                    label: label,
                    pluginName: plugin.manifest.plugin.name,
                    moduleName: module.name,
                    interval: module.interval,
                    daemonClient: daemonClient,
                )
                runner.start()
                runners.append(runner)
            }
        }
        let total = runners.count
        if total > 0 {
            FileHandle.standardError.write(Data("[copad] statusbar: \(total) module(s) loaded\n".utf8))
        }
    }

    /// Stop every running module timer. Idempotent. Called from
    /// `applicationWillTerminate` so we don't leave child processes orphaned
    /// for the brief window between quit and process exit.
    func shutdown() {
        for r in runners {
            r.stop()
        }
        runners.removeAll()
    }

    /// `statusbar.show/hide/toggle` socket commands route through this.
    /// Returns the post-call visibility state.
    @discardableResult
    func setShown(_ shown: Bool) -> Bool {
        isShown = shown
        isHidden = !shown
        return shown
    }

    private func stackForZone(_ zone: String) -> NSStackView {
        switch zone {
        case "left": leftStack
        case "center": centerStack
        default: rightStack
        }
    }

    /// Hot-reload: `[window] opacity` and/or theme change. Recolors
    /// the bar bg with the alpha-tinted surface so the bar still reads
    /// as chrome but lets the desktop / blur bleed through.
    func applyWindowOpacity(_ opacity: Double, theme: CopadTheme) {
        self.theme = theme
        windowOpacity = opacity
        layer?.backgroundColor = Self.barBg(theme: theme, opacity: opacity)
    }

    private static func barBg(theme: CopadTheme, opacity: Double) -> CGColor {
        opacity < 1.0
            ? theme.surface0.nsColor.withAlphaComponent(CGFloat(opacity)).cgColor
            : theme.surface0.nsColor.cgColor
    }
}
