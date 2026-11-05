import AppKit
import Foundation

/// Waybar-style status bar mirroring `copad-linux/src/statusbar.rs`: left/center/right zones of
/// per-module labels sorted by `order`, each module running a shell command on a timer whose
/// stdout is plain text or JSON `{text, tooltip}`.
///
/// Theme colors are applied imperatively via `applyTheme`. `module.class` is parsed into the
/// manifest (`copad-core/src/plugin.rs`) but unused on both platforms; module authors wanting
/// color cues should emit markup or unicode indicators.
@MainActor
final class StatusBarView: NSView {
    private let leftStack = NSStackView()
    private let centerStack = NSStackView()
    private let rightStack = NSStackView()
    private var runners: [StatusModuleRunner] = []
    private var labels: [String: NSTextField] = [:]
    /// The pomodoro `timer` module, rendered as a native ring; held for re-color and teardown.
    private var pomodoroRing: PomodoroRingView?
    private let separator = NSView()

    /// Backing store for `isHidden`, so `statusbar.show/hide/toggle` reads do not depend on
    /// AppKit's accessor across animation states.
    private(set) var isShown: Bool = true

    private var theme: CopadTheme
    private var windowOpacity: Double

    init(theme: CopadTheme, windowOpacity: Double = 1.0) {
        self.theme = theme
        self.windowOpacity = windowOpacity
        super.init(frame: .zero)
        translatesAutoresizingMaskIntoConstraints = false
        wantsLayer = true
        layer?.backgroundColor = Self.barBg(theme: theme, opacity: windowOpacity)
        // 1px top edge: surface0 and the background are nearly the same shade in Catppuccin Mocha.
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

        // Center stack is pinned by centerX so it stays put as the side stacks resize.
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

    func loadModules(
        _ plugins: [LoadedPluginManifest],
        daemonClient: DaemonClient,
        eventBus: EventBus,
    ) {
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
                // The pomodoro `timer` module gets a native ring instead of the poll-text label
                // (Linux keeps the poll text); match both plugin and module name.
                if plugin.manifest.plugin.name == "pomodoro", module.name == "timer" {
                    let ring = PomodoroRingView(
                        theme: theme,
                        daemonClient: daemonClient,
                        eventBus: eventBus,
                    )
                    stack.addArrangedSubview(ring)
                    pomodoroRing = ring
                    continue
                }
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

    /// Stop every module timer. Idempotent; called from `applicationWillTerminate` so no child
    /// process is orphaned.
    func shutdown() {
        for r in runners {
            r.stop()
        }
        runners.removeAll()
        pomodoroRing?.stop()
        pomodoroRing = nil
    }

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

    /// Hot-reload of `[window] opacity` and/or theme; goes through `applyTheme`.
    func applyWindowOpacity(_ opacity: Double, theme: CopadTheme) {
        windowOpacity = opacity
        applyTheme(theme)
    }

    /// Hot-reload theme: re-colors the bar bg (at the current `windowOpacity`), the separator and
    /// every module label's text color. Label contents belong to `StatusModuleRunner`.
    func applyTheme(_ newTheme: CopadTheme) {
        theme = newTheme
        layer?.backgroundColor = Self.barBg(theme: newTheme, opacity: windowOpacity)
        separator.layer?.backgroundColor = newTheme.overlay0.nsColor.cgColor
        for label in labels.values {
            label.textColor = newTheme.text.nsColor
        }
        pomodoroRing?.applyTheme(newTheme)
    }

    private static func barBg(theme: CopadTheme, opacity: Double) -> CGColor {
        opacity < 1.0
            ? theme.surface0.nsColor.withAlphaComponent(CGFloat(opacity)).cgColor
            : theme.surface0.nsColor.cgColor
    }
}
