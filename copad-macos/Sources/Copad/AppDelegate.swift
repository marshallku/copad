import AppKit
import CCopadFFI
import CopadCore
@preconcurrency import WebKit

@MainActor
class AppDelegate: NSObject, NSApplicationDelegate {
    var window: NSWindow?
    var tabVC: TabViewController?
    private let socketServer = SocketServer()
    private let eventBus = EventBus()
    private let actionRegistry = ActionRegistry()
    /// Tracks active panel + per-panel cwd for `{context.active_panel}` /
    /// `{context.active_cwd}` interpolation. Updated synchronously in `eventBus.onBroadcast`
    /// before each dispatch (apply-before-dispatch; see the wire-up below).
    private let contextService = ContextService()
    /// App-lifetime agent-status model behind the cockpit panel. One pump feeds
    /// it (`startAgentCockpitPump`); cockpit views observe it. See
    /// `docs/agent-cockpit.md`.
    private let agentCockpit = AgentCockpitModel()
    private weak var cockpitPanel: CockpitViewController?
    /// Rust trigger engine via FFI. Lazy because `copad_engine_create()` must run AFTER process
    /// startup; creating it at property-init risks a cold-launch race.
    private lazy var copadEngine = CopadEngine()
    private var configWatcher: ConfigWatcher?
    /// Compiled keybindings + the NSEvent monitor token. Hot-reload swaps `keybindings` in
    /// place; the monitor reads the latest via `self`.
    private var keybindings: [Keybindings.Binding] = []
    private var keybindingMonitor: Any?
    /// Cmd+Shift+P palette controller, held for the sheet's lifetime: `NSTableView` data
    /// source / delegate are unowned, so a transient controller would deallocate on `open`
    /// return and blank the table. Cleared from `endSheet` completion.
    private var commandPaletteController: CommandPaletteController?
    private var daemonClient: DaemonClient?
    /// Native wallpaper rotation (parity with `copad-linux/src/background.rs`). `armRotation`
    /// (re)builds `rotationTimer`; the live interval is tracked separately so manual
    /// `set`/`next`/`toggle` can restart the countdown without re-reading config.
    private var rotationTimer: DispatchSourceTimer?
    private var liveRotateInterval: UInt = 0

    /// Another terminal's identity, inherited when Copad is started from it (`open` in Ghostty,
    /// a shell in iTerm2). Left in the environment, every pane would claim to BE that terminal
    /// and programs that pick features by name (yazi's image driver) choose wrong. The pane
    /// identifies itself as `TERM_PROGRAM=copad` (copad-term sets it).
    private static let foreignTerminalVariables = [
        "TERM_PROGRAM", "TERM_PROGRAM_VERSION", "TERM_SESSION_ID", "LC_TERMINAL", "LC_TERMINAL_VERSION",
        "GHOSTTY_RESOURCES_DIR", "GHOSTTY_BIN_DIR", "GHOSTTY_SHELL_FEATURES", "GHOSTTY_SHELL_INTEGRATION_NO_SUDO",
        "KITTY_WINDOW_ID", "KITTY_PID", "KITTY_PUBLIC_KEY", "KITTY_INSTALLATION_DIR", "KITTY_LISTEN_ON",
        "ITERM_SESSION_ID", "ITERM_PROFILE", "WEZTERM_EXECUTABLE", "WEZTERM_EXECUTABLE_DIR", "WEZTERM_PANE",
        "WEZTERM_UNIX_SOCKET", "WEZTERM_CONFIG_DIR", "WEZTERM_CONFIG_FILE", "KONSOLE_VERSION", "WT_Session",
        "WARP_HONOR_PS1", "VSCODE_INJECTION", "TABBY_CONFIG_DIRECTORY", "ALACRITTY_WINDOW_ID",
        "ALACRITTY_SOCKET", "ALACRITTY_LOG", "TMUX", "TMUX_PANE", "STY", "WINDOW",
    ]

    func applicationWillFinishLaunching(_: Notification) {
        // Before any pane (or thread reading the environment) exists.
        for name in Self.foreignTerminalVariables {
            unsetenv(name)
        }
    }

    func applicationDidFinishLaunching(_: Notification) {
        CopadFFI.runSmokeTest()
        CopadTermFFI.runSmokeTest()

        // Wire the registry's completion fan-out bus BEFORE anything registers a handler, so
        // the very first dispatch (even a coctl racing socket startup or an onStartup plugin)
        // broadcasts `<method>.completed` / `.failed` on the bus the trigger engine listens to.
        // Idempotent; a setter because `eventBus` can't be built before `actionRegistry`
        // (Swift property init order).
        actionRegistry.setEventBus(eventBus)

        // Register first-party actions so the socket dispatcher hands off via tryDispatch
        // BEFORE the legacy switch fires.
        registerBuiltinActions()

        // The daemon owns plugin host + registry; with it down, plugin/registry actions surface
        // `daemon_unavailable` (the local engine still fires GUI-only triggers).
        daemonClient = DaemonClient(
            socket: CopadPaths.daemonSocket(),
            capabilities: ["tab", "split", "webview", "background", "statusbar", "terminal", "agent.ui", "plugin.open", "search", "session"],
            eventBus: eventBus,
        )
        // Capture the engine on the main actor: CopadEngine is `@unchecked Sendable`.
        let engine = copadEngine
        daemonClient?.cutoverHandler = { hostTriggers in
            engine.setEnabled(!hostTriggers)
        }
        actionRegistry.setFallbackHandler { [weak self] method, params, completion in
            guard let client = self?.daemonClient else {
                completion(RPCError(code: "daemon_unavailable", message: "DaemonClient not initialized"))
                return
            }
            if !client.isConnected {
                completion(RPCError(code: "daemon_unavailable", message: "daemon not connected (forward of \(method))"))
                return
            }
            client.forward(method: method, params: params, completion: completion)
        }
        // Daemon `Event`s (notifications, plugin completion fan-out) republish onto the local
        // bus with a fresh bridge_id marking them "already crossed", so the outbound forwarder
        // skips them instead of echoing. The local engine still fires triggers from the
        // republish unless `host_triggers=true` cuts it over. Capture `eventBus` directly:
        // `AppDelegate` is `@MainActor`, so a `@Sendable` closure capturing `self` would
        // violate isolation on `DaemonClient`'s reader thread; `EventBus` is thread-safe.
        let bus = eventBus
        daemonClient?.inboundEventHandler = { type, source, data, origin in
            bus.broadcast(
                event: type,
                source: source,
                data: data,
                bridgeId: UUID().uuidString,
                origin: origin,
            )
        }
        daemonClient?.invokeHandler = { [weak self] id, method, params, reply in
            guard let self else {
                let line = (try? JSONSerialization.data(withJSONObject: [
                    "id": id, "ok": false,
                    "error": ["code": "internal_error", "message": "AppDelegate gone"],
                ])).flatMap { String(data: $0, encoding: .utf8) }.map { $0 + "\n" }
                if let line { reply(line) }
                return
            }
            handleCommand(method: method, params: params, allowFallback: false, silentCompletion: true) { result in
                // nil is `unknown_method` (handlers `completion(nil)` for both missing handlers
                // and missing params); encoding it as ok:true would surface failures as silent
                // daemon successes.
                let payload: [String: Any] = if let err = result as? RPCError {
                    ["id": id, "ok": false, "error": ["code": err.code, "message": err.message]]
                } else if let result {
                    ["id": id, "ok": true, "result": result]
                } else {
                    ["id": id, "ok": false, "error": ["code": "unknown_method", "message": "no handler returned a value for \(method)"]]
                }
                if
                    let data = try? JSONSerialization.data(withJSONObject: payload),
                    let s = String(data: data, encoding: .utf8)
                {
                    reply(s + "\n")
                } else {
                    // Never silently drop a reply: the daemon would wait for its per-method
                    // timeout and the admission slot would hold to the safety net.
                    let fallback = "{\"id\":\"\(id)\",\"ok\":false,\"error\":{\"code\":\"internal_error\",\"message\":\"failed to encode response for \(method)\"}}\n"
                    reply(fallback)
                }
            }
        }
        // The invoke handler is installed BEFORE `start()` so the first inbound Invoke routes
        // through handleCommand. `start()` is deferred until the initial tab exists (see the
        // call below): an Invoke arriving earlier would no-op through the `guard let vc = tabVC`
        // arm.

        // Initial load: a parse failure falls back to `.defaults` so the app still starts.
        // `handleConfigChange` (hot reload) is stricter and keeps the previous live config.
        let config = (try? CopadConfig.load()) ?? .defaults
        let theme = CopadTheme.byName(config.themeName) ?? .default

        // Trigger engine via FFI: wire EventBus broadcasts (including plugin event.publish
        // forwards) into the Rust engine, which fires actions via the ActionRegistry callback.
        // The registry must already exist, the supervisor must have registered plugin
        // provides[] (so triggers can target plugin actions on the first event), and the config
        // must be loaded for the [[triggers]] array.
        copadEngine.actionRegistry = actionRegistry
        eventBus.onBroadcast = { [weak copadEngine, contextService] kind, source, data, origin in
            // EventBus.broadcast can fire from any thread (plugin reader thread, main actor);
            // dispatchEvent enters the Rust engine, which has its own RwLock. Log only on a
            // trigger match to keep heartbeat noise down. `source` carries the await-promotion
            // trust stamp (registry-synthesized completions arrive as "copad.action").
            //
            // Apply-before-dispatch: the event is applied to ContextService BEFORE the engine
            // sees it, and the post-apply snapshot is passed through FFI, so a `panel.focused`
            // trigger referencing `{context.active_panel}` resolves to the just-focused panel
            // (Linux `Pump::pump_all`, copad-linux/src/window.rs). ContextService is
            // `@unchecked Sendable` with an internal NSLock, so off-main callers are safe.
            contextService.apply(eventKind: kind, data: data)
            let context = contextService.snapshot()
            let n = copadEngine?.dispatchEvent(
                kind: kind,
                source: source,
                context: context,
                payload: data,
                origin: origin,
            ) ?? 0
            if n > 0 {
                FileHandle.standardError.write(Data("[copad-engine] event \(kind) fired \(n) trigger(s)\n".utf8))
            }
        }
        let triggerJSON = CopadConfig.triggersJSON(from: config)
        if let count = copadEngine.setTriggers(triggerJSON) {
            FileHandle.standardError.write(Data("[copad-engine] loaded \(count) trigger(s) from config.toml\n".utf8))
        }

        // Custom keybindings. Install BEFORE menu bar + window so the monitor catches the
        // first keystroke; menu keyEquivalents still take precedence over local monitors.
        // Hot-reload calls `applyKeybindings`.
        applyKeybindings(config.keybindings)
        installKeybindingMonitor()

        setupMenuBar()

        let window = NSWindow(
            contentRect: NSRect(x: 0, y: 0, width: 1200, height: 800),
            styleMask: [.titled, .closable, .resizable, .miniaturizable],
            backing: .buffered,
            defer: false,
        )
        window.title = "copad"
        window.center()
        window.isRestorable = false
        // Let theme.background show through the titlebar instead of OS chrome. Appearance is
        // forced by background luminance so traffic lights stay readable on dark themes.
        window.titlebarAppearsTransparent = true
        window.appearance = NSAppearance(named: isDark(theme.background) ? .darkAqua : .aqua)

        let vc = TabViewController(config: config, theme: theme)
        window.contentViewController = vc
        applyWindowTransparency(window, config: config, theme: theme)
        window.setContentSize(NSSize(width: 1200, height: 800))
        window.center()

        window.makeKeyAndOrderFront(nil)
        NSApp.activate(ignoringOtherApps: true)

        self.window = window
        tabVC = vc
        vc.eventBus = eventBus
        startAgentCockpitPump()
        // "+" popover's plugin-row dispatch, same outcome as the `plugin.open` RPC. Errors
        // are stderr-logged (the popover is fire-and-forget).
        vc.onOpenPlugin = { [weak self] name, panelName, mode in
            guard let self, let tab = tabVC else { return }
            let modeStr = switch mode {
            case .tab: "tab"
            case .splitH: "split_h"
            case .splitV: "split_v"
            }
            if case let .failure(err) = openPluginPanel(name: name, panelName: panelName, mode: modeStr, vc: tab) {
                FileHandle.standardError.write(Data("[copad] plugin.open via popover failed: \(err.code) \(err.message)\n".utf8))
            }
        }

        // Status bar modules: loaded after tabVC is built (it owns the StatusBarView) but
        // BEFORE the socket starts, so initial exec doesn't race a coctl command depending on
        // module state.
        if let bar = vc.statusBar, let client = daemonClient {
            let manifests = PluginManifestStore.discover()
            bar.loadModules(manifests, daemonClient: client, eventBus: eventBus)
        }

        startSocketServer()
        startConfigWatcher()
        // Factory to reopen a plugin panel by name on restore (decision #61). Lives here
        // because plugin construction needs the manifest store + action registry the pane
        // manager can't reach; MUST be set before the restore below. nil (plugin uninstalled)
        // gives an explicit placeholder.
        vc.pluginFactory = { [weak self, weak vc] name, panelName in
            guard let self, let vc,
                  let manifest = PluginManifestStore.discover().first(where: { $0.manifest.plugin.name == name })
            else { return nil }
            // Reopen the persisted panel of a multi-panel plugin; fall back to
            // the plugin's first panel.
            let panelDef = manifest.manifest.panels.first(where: { $0.name == panelName })
                ?? manifest.manifest.panels.first
            guard let panelDef else { return nil }
            return PluginPanelController(
                plugin: manifest,
                panelDef: panelDef,
                registry: self.actionRegistry,
                eventBus: self.eventBus,
                theme: vc.theme,
            )
        }
        // Reopen the agent cockpit on restore (a `.cockpit` leaf); track the panel so
        // toggleCockpit dedupes.
        vc.cockpitFactory = { [weak self] in
            guard let self, let tabVC = self.tabVC else { return nil }
            let cockpit = CockpitViewController(model: self.agentCockpit, tabVC: tabVC)
            self.cockpitPanel = cockpit
            return cockpit
        }
        // Restore the v3 session (decision #64): replay tabs + splits + per-leaf cwd, else seed
        // one terminal tab (an old v1/v2 file is rejected by the loader). MUST happen before
        // the daemon starts so GUI-owned Invokes see the restored layout, and before any
        // save-on-terminate can overwrite the snapshot with an empty one. Mirrors
        // `copad-linux/src/window.rs`.
        if let snap = Session.load(), !snap.tabs.isEmpty {
            vc.restoreSession(snap)
        } else {
            vc.openInitialTab()
        }

        // Start AFTER the initial tab exists so daemon Invokes for GUI-owned methods
        // (tab.list, terminal.exec, split.*) resolve against a real pane.
        daemonClient?.start()

        if let path = config.backgroundPath {
            vc.applyBackground(path: path, tint: config.backgroundTint, opacity: config.backgroundOpacity)
        }

        // Native wallpaper rotation: a static `image` is applied above, then the first rotation
        // tick takes over (as Linux's window.rs). No-op at interval 0.
        liveRotateInterval = config.rotateInterval
        if liveRotateInterval > 0 {
            rotateOnce()
        }
        armRotation()
    }

    // MARK: - Background rotation (Linux parity)

    /// (Re)build the rotation timer from `liveRotateInterval`; 0 stops it. Manual
    /// `background.set`/`next`/`toggle` call this too, restarting the countdown
    /// (`BackgroundLayer::arm_rotation`).
    private func armRotation() {
        rotationTimer?.cancel()
        rotationTimer = nil
        guard liveRotateInterval > 0 else { return }
        let timer = DispatchSource.makeTimerSource(queue: .main)
        // `.seconds(Int)`: clamp the UInt so an absurd config value can't overflow.
        let secs = Int(min(liveRotateInterval, UInt(Int.max)))
        timer.schedule(deadline: .now() + .seconds(secs), repeating: .seconds(secs))
        timer.setEventHandler { [weak self] in self?.rotateOnce() }
        timer.resume()
        rotationTimer = timer
    }

    /// One rotation tick: honor the shared mode flag, pick a random list image and apply it as a
    /// list pick. No-op when deactive or the list is missing/empty (`BackgroundLayer::rotate_once`).
    private func rotateOnce() {
        guard BackgroundRotator.isActive else { return }
        guard let img = BackgroundRotator.nextRandomImage(), let vc = tabVC else { return }
        vc.applyBackground(
            path: img,
            tint: vc.currentBackgroundTint,
            opacity: vc.currentBackgroundOpacity,
            fromList: true,
        )
    }

    func applicationWillTerminate(_: Notification) {
        // Persist tabs + splits + cwd (v3, decision #64) BEFORE engine shutdown, while panels
        // are live. An empty snapshot clears any prior file so a stale layout doesn't surface
        // (as `copad-linux/src/window.rs`). The debounced save covers crashes; this is the
        // orderly-quit flush.
        if let snap = tabVC?.snapshotSession() {
            if snap.tabs.isEmpty {
                Session.clear()
            } else if Session.save(snap) {
                TabViewController.gcHistoryBlobs(after: snap)
            }
        }
        // Order matters: (1) engine, clearing the C action callback so no in-flight
        // event.publish fires into a stale ActionRegistry; (2) supervisor, sending `shutdown`
        // to plugins; (3) socket; (4) config watcher.
        copadEngine.shutdown()
        tabVC?.statusBar?.shutdown()
        socketServer.stop()
        configWatcher?.stop()
        rotationTimer?.cancel()
        rotationTimer = nil
        if let token = keybindingMonitor {
            NSEvent.removeMonitor(token)
        }
    }

    func applicationShouldTerminateAfterLastWindowClosed(_: NSApplication) -> Bool {
        true
    }

    /// Rec. 601 luminance over [0,1]; a 0.5 threshold picks darkAqua for any reasonably dark
    /// background, aligning the traffic lights with theme.background.
    private func isDark(_ c: RGBColor) -> Bool {
        let lum = 0.299 * Double(c.r) + 0.587 * Double(c.g) + 0.114 * Double(c.b)
        return lum < 128
    }

    /// Apply `[window] opacity` + `[window] blur` to the main window (Ghostty model).
    /// Idempotent; at opacity 1.0 it removes any blur view and restores opaque chrome. The
    /// `NSVisualEffectView` becomes the bottom-most sibling behind the TabViewController's
    /// root; the controller's view is wrapped in a container so the blur can layer beneath
    /// without disturbing AutoLayout of subviews that assume it is the top-level container.
    private func applyWindowTransparency(_ window: NSWindow, config: CopadConfig, theme: CopadTheme) {
        let opacity = CGFloat(config.windowOpacity)
        let wantsTransparent = opacity < 1.0
        let bg = theme.background.nsColor.withAlphaComponent(opacity)

        window.isOpaque = !wantsTransparent
        window.hasShadow = true
        // backgroundColor alpha < 1 only takes effect when isOpaque = false; set unconditionally
        // so the chrome color is consistent at opacity 1.0.
        window.backgroundColor = bg

        installOrRemoveBlurView(window: window, enabled: wantsTransparent && config.windowBlur)
    }

    private func installOrRemoveBlurView(window: NSWindow, enabled: Bool) {
        guard let content = window.contentView else { return }
        let blurTag = "copad.blurView"

        let existing = content.subviews.first {
            ($0 as? NSVisualEffectView)?.identifier?.rawValue == blurTag
        } as? NSVisualEffectView

        if !enabled {
            existing?.removeFromSuperview()
            return
        }

        let blur = existing ?? {
            let v = NSVisualEffectView()
            v.identifier = NSUserInterfaceItemIdentifier(blurTag)
            // `.hudWindow` reads as dark, dense vibrancy; `.behindWindow` blends the desktop
            // behind the window (Ghostty pattern), while `.withinWindow` would only blur sibling
            // windows of the same app.
            v.material = .hudWindow
            v.blendingMode = .behindWindow
            v.state = .active
            v.autoresizingMask = [.width, .height]
            return v
        }()

        blur.frame = content.bounds
        if blur.superview !== content {
            content.addSubview(blur, positioned: .below, relativeTo: content.subviews.first)
        }
    }

    // MARK: - Menu Bar

    private func setupMenuBar() {
        let mainMenu = NSMenu()

        // App menu
        let appItem = NSMenuItem()
        mainMenu.addItem(appItem)
        let appMenu = NSMenu()
        appItem.submenu = appMenu
        appMenu.addItem(withTitle: "Quit copad", action: #selector(NSApplication.terminate(_:)), keyEquivalent: "q")

        // Shell menu (tab management)
        let shellItem = NSMenuItem()
        mainMenu.addItem(shellItem)
        let shellMenu = NSMenu(title: "Shell")
        shellItem.submenu = shellMenu

        let newTabItem = NSMenuItem(title: "New Tab", action: #selector(newTab), keyEquivalent: "t")
        newTabItem.target = self
        shellMenu.addItem(newTabItem)

        let newWebTabItem = NSMenuItem(title: "New Web Tab", action: #selector(newWebTab), keyEquivalent: "t")
        newWebTabItem.keyEquivalentModifierMask = [.command, .shift]
        newWebTabItem.target = self
        shellMenu.addItem(newWebTabItem)

        let closePaneItem = NSMenuItem(title: "Close Pane", action: #selector(closePane), keyEquivalent: "w")
        closePaneItem.target = self
        shellMenu.addItem(closePaneItem)

        let remoteItem = NSMenuItem(title: "Open Remote comux…", action: #selector(openRemoteComuxPrompt), keyEquivalent: "r")
        remoteItem.keyEquivalentModifierMask = [.command, .shift]
        remoteItem.target = self
        shellMenu.addItem(remoteItem)

        shellMenu.addItem(.separator())

        let splitHItem = NSMenuItem(title: "Split Pane Horizontally", action: #selector(splitHorizontal), keyEquivalent: "d")
        splitHItem.target = self
        shellMenu.addItem(splitHItem)

        let splitVItem = NSMenuItem(title: "Split Pane Vertically", action: #selector(splitVertical), keyEquivalent: "D")
        splitVItem.keyEquivalentModifierMask = [.command, .shift]
        splitVItem.target = self
        shellMenu.addItem(splitVItem)

        shellMenu.addItem(.separator())

        let nextPaneItem = NSMenuItem(title: "Next Pane", action: #selector(focusNextPane), keyEquivalent: "]")
        nextPaneItem.keyEquivalentModifierMask = [.command, .shift]
        nextPaneItem.target = self
        shellMenu.addItem(nextPaneItem)

        let prevPaneItem = NSMenuItem(title: "Previous Pane", action: #selector(focusPrevPane), keyEquivalent: "[")
        prevPaneItem.keyEquivalentModifierMask = [.command, .shift]
        prevPaneItem.target = self
        shellMenu.addItem(prevPaneItem)

        shellMenu.addItem(.separator())

        for i in 1 ... 9 {
            let item = NSMenuItem(title: "Tab \(i)", action: #selector(switchTabByNumber(_:)), keyEquivalent: "\(i)")
            item.target = self
            item.tag = i
            shellMenu.addItem(item)
        }

        // Edit menu: Cmd+C/V/A route through the responder chain (target=nil) to the terminal
        // view's copy:/paste:/selectAll:. Without this menu those keyEquivalents never
        // dispatch and the clipboard appears dead.
        let editItem = NSMenuItem()
        mainMenu.addItem(editItem)
        let editMenu = NSMenu(title: "Edit")
        editItem.submenu = editMenu
        editMenu.addItem(withTitle: "Copy", action: #selector(NSText.copy(_:)), keyEquivalent: "c")
        editMenu.addItem(withTitle: "Paste", action: #selector(NSText.paste(_:)), keyEquivalent: "v")
        editMenu.addItem(withTitle: "Select All", action: #selector(NSResponder.selectAll(_:)), keyEquivalent: "a")

        // Find menu
        let findItem = NSMenuItem()
        mainMenu.addItem(findItem)
        let findMenu = NSMenu(title: "Find")
        findItem.submenu = findMenu

        let findAction = NSMenuItem(title: "Find…", action: #selector(performFindPanelAction(_:)), keyEquivalent: "f")
        findAction.tag = Int(NSFindPanelAction.showFindPanel.rawValue)
        findMenu.addItem(findAction)

        let findNextAction = NSMenuItem(title: "Find Next", action: #selector(performFindPanelAction(_:)), keyEquivalent: "g")
        findNextAction.tag = Int(NSFindPanelAction.next.rawValue)
        findMenu.addItem(findNextAction)

        let findPrevAction = NSMenuItem(title: "Find Previous", action: #selector(performFindPanelAction(_:)), keyEquivalent: "G")
        findPrevAction.keyEquivalentModifierMask = NSEvent.ModifierFlags([.command, .shift])
        findPrevAction.tag = Int(NSFindPanelAction.previous.rawValue)
        findMenu.addItem(findPrevAction)

        // View menu (zoom + tab bar toggle)
        let viewItem = NSMenuItem()
        mainMenu.addItem(viewItem)
        let viewMenu = NSMenu(title: "View")
        viewItem.submenu = viewMenu

        let toggleTabBarItem = NSMenuItem(title: "Toggle Tab Bar", action: #selector(toggleTabBar), keyEquivalent: "b")
        toggleTabBarItem.keyEquivalentModifierMask = [.command, .shift]
        toggleTabBarItem.target = self
        viewMenu.addItem(toggleTabBarItem)

        let cockpitItem = NSMenuItem(title: "Agent Cockpit", action: #selector(toggleCockpit), keyEquivalent: "y")
        cockpitItem.keyEquivalentModifierMask = [.command, .shift]
        cockpitItem.target = self
        viewMenu.addItem(cockpitItem)

        let graphItem = NSMenuItem(title: "Work Graph", action: #selector(toggleWorkGraph), keyEquivalent: "g")
        graphItem.keyEquivalentModifierMask = [.command, .shift]
        graphItem.target = self
        viewMenu.addItem(graphItem)

        viewMenu.addItem(.separator())

        let zoomIn = NSMenuItem(title: "Zoom In", action: #selector(zoomIn), keyEquivalent: "=")
        zoomIn.target = self
        viewMenu.addItem(zoomIn)

        let zoomOut = NSMenuItem(title: "Zoom Out", action: #selector(zoomOut), keyEquivalent: "-")
        zoomOut.target = self
        viewMenu.addItem(zoomOut)

        let zoomReset = NSMenuItem(title: "Actual Size", action: #selector(zoomReset), keyEquivalent: "0")
        zoomReset.target = self
        viewMenu.addItem(zoomReset)

        NSApp.mainMenu = mainMenu
    }

    // MARK: - Tab / Pane Actions

    @objc private func openRemoteComuxPrompt() {
        tabVC?.promptOpenRemoteComux()
    }

    @objc private func newTab() {
        tabVC?.newTab()
    }

    @objc private func newWebTab() {
        tabVC?.newWebViewTab()
    }

    @objc private func closePane() {
        tabVC?.closeActivePane()
    }

    @objc private func splitHorizontal() {
        tabVC?.splitActivePane(orientation: .horizontal)
    }

    @objc private func splitVertical() {
        tabVC?.splitActivePane(orientation: .vertical)
    }

    @objc private func focusNextPane() {
        tabVC?.focusNextPane(direction: 1)
    }

    @objc private func focusPrevPane() {
        tabVC?.focusNextPane(direction: -1)
    }

    @objc private func switchTabByNumber(_ sender: NSMenuItem) {
        tabVC?.switchTab(to: sender.tag - 1)
    }

    // MARK: - Tab Bar Toggle

    @objc func toggleTabBar() {
        tabVC?.toggleTabBar(userInitiated: true)
    }

    /// The comux work-graph panel of the front window (decisions #204).
    @objc func toggleWorkGraph() {
        tabVC?.toggleComuxGraph(mode: "agent")
    }

    // MARK: - Agent cockpit

    /// Open the agent cockpit as a tab, or focus the existing one.
    @objc func toggleCockpit() {
        guard let tabVC else { return }
        if let existing = cockpitPanel, tabVC.activatePanel(id: existing.panelID) { return }
        let cockpit = CockpitViewController(model: agentCockpit, tabVC: tabVC)
        cockpitPanel = cockpit
        _ = tabVC.newPluginPanelTab(cockpit)
    }

    /// The single app-lifetime pump feeding the cockpit model: agent kinds mutate it,
    /// panel.exited evicts, panel.focused acknowledges, and every relevant event notifies
    /// observers so open cockpit views re-enumerate.
    private func startAgentCockpitPump() {
        let channel = eventBus.subscribe()
        let model = agentCockpit
        // macOS publishes `tab.opened` locally (Linux uses `tab.created`); include both plus
        // close/rename so the cockpit re-enumerates. Title renames post `.terminalTitleChanged`
        // (NotificationCenter, not the bus), which the cockpit view observes separately.
        let kinds: Set<String> = [
            "claude.working", "claude.awaiting_input", "claude.session_stopped",
            "panel.exited", "panel.focused",
            "tab.opened", "tab.created", "tab.closed", "tab.renamed", "panel.title_changed",
        ]
        Thread.detachNewThread {
            while let json = channel.receive() {
                guard let data = json.data(using: .utf8),
                      let obj = try? JSONSerialization.jsonObject(with: data) as? [String: Any],
                      let type = obj["event"] as? String, kinds.contains(type)
                else { continue }
                let payload = obj["data"] as? [String: Any] ?? [:]
                // Serial FIFO hop to the main actor so transitions apply in arrival order
                // (unstructured `Task`s don't preserve order).
                DispatchQueue.main.async {
                    MainActor.assumeIsolated {
                        switch type {
                        case "claude.working", "claude.awaiting_input", "claude.session_stopped":
                            _ = model.observe(kind: type, payload: payload)
                        case "panel.exited":
                            if let id = payload["panel_id"] as? String { _ = model.forget(id) }
                        case "panel.focused":
                            if let id = payload["panel_id"] as? String { _ = model.acknowledge(id) }
                        default:
                            break // tab lifecycle → just re-enumerate
                        }
                        model.notifyObservers()
                    }
                }
            }
        }
    }

    // MARK: - Find

    /// Cmd+F / Cmd+G / Cmd+Shift+G dispatch. The `tag` carries an `NSFindPanelAction` raw value
    /// (showFindPanel / next / previous). Non-terminal panes silently no-op.
    @objc func performFindPanelAction(_ sender: NSMenuItem) {
        guard let panel = tabVC?.activePaneManager?.activePane else { return }
        guard let alacritty = panel as? AlacrittyTerminalViewController else { return }
        let action = NSFindPanelAction(rawValue: UInt(sender.tag))
        switch action {
        case .next: alacritty.findNext()
        case .previous: alacritty.findPrevious()
        default: alacritty.toggleFindBar()
        }
    }

    // MARK: - Zoom Actions

    @objc private func zoomIn() {
        tabVC?.activeZoomable?.zoomIn()
    }

    @objc private func zoomOut() {
        tabVC?.activeZoomable?.zoomOut()
    }

    @objc private func zoomReset() {
        tabVC?.activeZoomable?.zoomReset()
    }

    // MARK: - Action Registry

    /// First-party `system.*` actions reachable through the registry.
    private func registerBuiltinActions() {
        // system.ffi_test: proxy to CopadFFI.callJSON, a smoke target reachable from
        // `coctl call` and registrable as a trigger action. Silent: a debug round-trip.
        actionRegistry.registerSilent("system.ffi_test") { params, completion in
            // Pass through the caller's params; send an empty object if absent so the FFI gets
            // valid JSON.
            let payload = params.isEmpty ? ["caller": "system.ffi_test"] : params
            if let echoed = CopadFFI.callJSON(payload) {
                completion(["echoed": echoed, "ffi_version": CopadFFI.version()])
            } else {
                completion(RPCError(
                    code: "ffi_error",
                    message: CopadFFI.lastError() ?? "CopadFFI.callJSON returned nil",
                ))
            }
        }

        // system.list_actions: every registered name, for diagnostics. Silent because UIs
        // polling it would flood the bus with `.completed` events.
        actionRegistry.registerSilent("system.list_actions") { [weak self] _, completion in
            guard let self else {
                completion(RPCError(code: "no_app", message: "AppDelegate gone"))
                return
            }
            completion([
                "count": actionRegistry.count,
                "names": actionRegistry.names(),
            ])
        }

        // `context.snapshot`: the live ContextService state, same wire shape as Linux. Silent
        // because tooling may poll it.
        actionRegistry.registerSilent("context.snapshot") { [weak self] _, completion in
            guard let self else {
                completion(RPCError(code: "no_app", message: "AppDelegate gone"))
                return
            }
            completion(contextService.snapshot())
        }

        // `claude.start` must be registered here: the trigger engine's C callback dispatches
        // only via `ActionRegistry.tryDispatch` (no legacy-switch fallthrough), so without it
        // the `git.worktree_add.completed -> claude.start` chain would stall. Noisy, not silent,
        // so chains can observe `claude.start.completed`.
        actionRegistry.register("claude.start") { [weak self] params, completion in
            guard let self else {
                completion(RPCError(code: "no_app", message: "AppDelegate gone"))
                return
            }
            ClaudeStart.dispatch(params: params, tabVC: tabVC, completion: completion)
        }

        // panel.report_cwd: the alacritty backend doesn't surface OSC 7 (Linux/VTE does) and
        // proc_pidinfo is EPERM on un-entitled builds, so the in-shell `copad-cwd` hook calls
        // this on every `chpwd`. params: `{"panel_id": "...", "cwd": "..."}`. Silent: prompts
        // fire it constantly.
        actionRegistry.registerSilent("panel.report_cwd") { [weak self] params, completion in
            guard let self else {
                completion(RPCError(code: "no_app", message: "AppDelegate gone"))
                return
            }
            guard let panelID = params["panel_id"] as? String, !panelID.isEmpty else {
                completion(RPCError(
                    code: "invalid_params",
                    message: "panel.report_cwd requires non-empty `panel_id` string",
                ))
                return
            }
            guard let cwd = params["cwd"] as? String, !cwd.isEmpty else {
                completion(RPCError(
                    code: "invalid_params",
                    message: "panel.report_cwd requires non-empty `cwd` string",
                ))
                return
            }
            guard let tabVC else {
                completion(RPCError(code: "no_app", message: "TabViewController gone"))
                return
            }
            // Find the panel by id across all tabs / splits and update its tracked cwd;
            // non-terminal panels are ignored (the hook is shell-only).
            let updated = tabVC.applyReportedCwd(panelID: panelID, cwd: cwd)
            completion(["updated": updated])
        }

        // notify.show: mirror of the Linux daemon registration (`copad-linux/src/window.rs`) so
        // the in-process trigger engine reaches the `osascript` notifier even without the
        // daemon. Silent: a toast is a side effect, not a workflow step.
        actionRegistry.registerSilent("notify.show") { params, completion in
            guard let title = params["title"] as? String, !title.isEmpty else {
                completion(RPCError(
                    code: "invalid_params",
                    message: "notify.show requires non-empty `title` string",
                ))
                return
            }
            let body: String
            switch params["body"] {
            case nil, is NSNull: body = ""
            case let s as String: body = s
            default:
                completion(RPCError(
                    code: "invalid_params",
                    message: "notify.show `body` must be a string",
                ))
                return
            }
            let level: Int32
            switch params["level"] {
            case nil, is NSNull: level = 0
            case let s as String:
                switch s {
                case "info": level = 0
                case "warn": level = 1
                case "error": level = 2
                default:
                    completion(RPCError(
                        code: "invalid_params",
                        message: "notify.show `level` must be one of `info`, `warn`, `error`",
                    ))
                    return
                }
            default:
                completion(RPCError(
                    code: "invalid_params",
                    message: "notify.show `level` must be a string",
                ))
                return
            }
            // The osascript spawn is sync (~10 ms): offload it, then bounce the completion back
            // to main where the socket server expects it. LAST_ERROR is thread-local, so read
            // it on the thread that called the FFI.
            DispatchQueue.global(qos: .userInitiated).async {
                let rc = title.withCString { titlePtr in
                    body.withCString { bodyPtr in
                        copad_ffi_notify_show(titlePtr, bodyPtr, level)
                    }
                }
                let err: String? = rc < 0
                    ? copad_ffi_last_error().map { String(cString: $0) }
                    : nil
                DispatchQueue.main.async {
                    switch rc {
                    case 0:
                        completion(["shown": true])
                    case 1:
                        completion(["shown": false, "reason": "no_notifier"])
                    default:
                        completion(RPCError(
                            code: "internal_error",
                            message: "notify subprocess: \(err ?? "<unknown>")",
                        ))
                    }
                }
            }
        }
    }

    // MARK: - Socket Server

    // MARK: - Config Watcher

    private func startConfigWatcher() {
        // Route through `CopadConfig.configPath()` so XDG_CONFIG_HOME overrides reach the
        // watcher too; the Swift load, Rust daemon, coctl and `copad --config-path` all use it.
        let configURL = CopadConfig.configPath()
        let watcher = ConfigWatcher(url: configURL)
        watcher.onChange = { [weak self] in self?.handleConfigChange() }
        watcher.start()
        configWatcher = watcher
    }

    private func handleConfigChange() {
        // As Linux's `window.rs::connect_changed`: on parse failure log and return early, so the
        // live config keeps rendering instead of resetting to defaults on a typo.
        let newConfig: CopadConfig
        do {
            newConfig = try CopadConfig.load()
        } catch {
            let msg = "[copad] config reload error: \(error.localizedDescription) — keeping previous config\n"
            FileHandle.standardError.write(Data(msg.utf8))
            return
        }
        let newTheme = CopadTheme.byName(newConfig.themeName) ?? .default
        tabVC?.applyConfig(newConfig, theme: newTheme)
        if let window {
            applyWindowTransparency(window, config: newConfig, theme: newTheme)
        }
        // Re-arm rotation with the possibly changed interval (0 stops it). A rotated wallpaper
        // survives via TabViewController.applyConfig's list-image guard.
        liveRotateInterval = newConfig.rotateInterval
        armRotation()
        // Skip the local trigger reload while the daemon owns triggers (it runs its own config
        // watcher and would race our setTriggers). The local engine keeps its previous set for
        // cut-over restore on daemon disconnect.
        if daemonClient?.hostTriggersActive == true {
            FileHandle.standardError.write(Data("[copad-engine] skipping local trigger reload — daemon owns triggers (host_triggers=true)\n".utf8))
        } else {
            let triggerJSON = CopadConfig.triggersJSON(from: newConfig)
            if let count = copadEngine.setTriggers(triggerJSON) {
                FileHandle.standardError.write(Data("[copad-engine] reloaded \(count) trigger(s) on config.toml change\n".utf8))
            }
        }
        applyKeybindings(newConfig.keybindings)
        eventBus.broadcast(event: "config.reloaded", data: ["theme": newTheme.name])
    }

    // MARK: - Keybindings

    private func applyKeybindings(_ raw: [String: String]) {
        keybindings = Keybindings.compile(raw)
        if !keybindings.isEmpty {
            FileHandle.standardError.write(Data("[copad] loaded \(keybindings.count) custom keybinding(s)\n".utf8))
        }
    }

    private func installKeybindingMonitor() {
        // .keyDown so repeats arrive too. The monitor returns the event when no binding
        // matches so the responder chain sees it; returning nil swallows it, only on a match.
        keybindingMonitor = NSEvent.addLocalMonitorForEvents(matching: .keyDown) { [weak self] event in
            guard let self else { return event }
            for binding in keybindings where Keybindings.matches(event, binding) {
                Keybindings.dispatch(binding, registry: actionRegistry, socketPath: socketServer.path)
                return nil
            }
            // cmd+shift+1...9 / 0 jump tabs. Bare Option is left to the terminal (opt+digit
            // conflicts with tmux Meta+digit). Only fires when that tab exists; user
            // `[keybindings]` win.
            if let idx = commandShiftDigitTabIndex(event), let vc = tabVC, idx < vc.tabCount {
                vc.switchTab(to: idx)
                return nil
            }
            if matchesCommandPaletteShortcut(event), openCommandPalette() {
                return nil
            }
            return event
        }
    }

    /// Map a Cmd+Shift+digit event to a sub-tab index (1...9 -> 0...8, 0 -> 9). nil unless
    /// Command+Shift are the ONLY modifiers. Matched on keyCode: Shift rewrites the character
    /// (Shift+1 -> "!").
    private func commandShiftDigitTabIndex(_ event: NSEvent) -> Int? {
        let interesting: NSEvent.ModifierFlags = [.command, .control, .shift, .option]
        guard event.modifierFlags.intersection(interesting) == [.command, .shift] else { return nil }
        guard let digit = Self.digitKeyCodeToValue[event.keyCode] else { return nil }
        return digit == 0 ? 9 : digit - 1
    }

    /// Layout-position keyCode -> the digit on the number row, derived from
    /// `Keybindings.nameToKeyCode`. Position-based so it holds when Shift alters the character.
    private static let digitKeyCodeToValue: [UInt16: Int] = {
        var map: [UInt16: Int] = [:]
        for n in 0...9 {
            if let code = Keybindings.nameToKeyCode[String(n)] {
                map[code] = n
            }
        }
        return map
    }()

    /// Cmd+Shift+P (VSCode/Zed convention). User keybindings run first, so a `cmd+shift+p`
    /// entry shadows the palette, as Linux's "user binding wins". The keyCode comes from the
    /// shared `Keybindings.nameToKeyCode`.
    private func matchesCommandPaletteShortcut(_ event: NSEvent) -> Bool {
        let interesting: NSEvent.ModifierFlags = [.command, .control, .shift, .option]
        let mods = event.modifierFlags.intersection(interesting)
        guard mods == [.command, .shift] else { return false }
        guard let pCode = Keybindings.nameToKeyCode["p"] else { return false }
        return event.keyCode == pCode
    }

    /// Returns true when the palette opened (the event must be swallowed). Repeats and re-entry
    /// while a sheet is attached return false so a held key doesn't stack modal state;
    /// `attachedSheet` also covers the `tab.close` confirm alert, which opens after the palette
    /// closes (`commandPaletteController` is already nil) and a second `beginSheet` would break
    /// the modal invariant.
    @MainActor
    private func openCommandPalette() -> Bool {
        guard
            let win = window,
            commandPaletteController == nil,
            win.attachedSheet == nil
        else { return false }
        let actions = CommandPalette.collectActions(registry: actionRegistry)
        let restore = win.firstResponder
        let controller = CommandPaletteController(
            parentWindow: win,
            actions: actions,
            restoreFocus: restore,
            dispatch: { [weak self] method in
                self?.handleCommand(method: method, params: [:]) { result in
                    // Legacy switch arms `completion(nil)` for missing params; surface a usable
                    // hint instead of a silent no-op.
                    if let err = result as? RPCError {
                        FileHandle.standardError.write(Data("[copad] command_palette: \(method) failed: \(err.code) — \(err.message)\n".utf8))
                    } else if result == nil {
                        FileHandle.standardError.write(Data("[copad] command_palette: \(method) returned no result — action likely needs params (palette dispatches with empty {})\n".utf8))
                    }
                }
            },
            onClose: { [weak self] in
                self?.commandPaletteController = nil
            },
        )
        commandPaletteController = controller
        controller.open()
        return true
    }

    private func startSocketServer() {
        socketServer.eventBus = eventBus
        socketServer.commandHandler = { [weak self] method, params, completion in
            self?.handleCommand(method: method, params: params, completion: completion)
        }
        socketServer.start()
    }

    /// Linux-parity terminal panel resolver (`resolve_terminal` in
    /// copad-linux/src/socket.rs). Priority: the panel named by `params["id"]` (`not_found`, or
    /// `wrong_panel_type` if it isn't a terminal); else the active pane if a terminal; else the
    /// first terminal across tabs (`firstTerminalPanel`); else `no_terminal`. Error codes match
    /// Linux verbatim so coctl / agent error handling is backend-agnostic.
    private func resolveTerminalPanel(
        params: [String: Any],
        vc: TabViewController,
    ) -> Result<any TerminalCapable, RPCError> {
        if let id = params["id"] as? String {
            guard let panel = vc.panel(id: id) else {
                return .failure(RPCError(code: "not_found", message: "Panel not found: \(id)"))
            }
            guard let term = panel as? TerminalCapable else {
                return .failure(RPCError(code: "wrong_panel_type", message: "Panel is not a terminal"))
            }
            return .success(term)
        }
        if let active = vc.activeTerminalPanel {
            return .success(active)
        }
        if let first = vc.firstTerminalPanel() {
            return .success(first)
        }
        return .failure(RPCError(code: "no_terminal", message: "No terminal panel found"))
    }

    /// Open a plugin-provided panel, shared by the `plugin.open` RPC and the `+` popover's
    /// plugin rows. `mode` is "tab" (default) / "split_h" / "split_v"; an unknown value falls
    /// through to "tab".
    private func openPluginPanel(
        name: String,
        panelName: String,
        mode: String,
        vc: TabViewController,
    ) -> Result<String, RPCError> {
        let manifests = PluginManifestStore.discover()
        guard let manifest = manifests.first(where: { $0.manifest.plugin.name == name }) else {
            return .failure(RPCError(code: "not_found", message: "plugin '\(name)' not installed"))
        }
        guard let panelDef = manifest.manifest.panels.first(where: { $0.name == panelName }) else {
            let available = manifest.manifest.panels.map(\.name).joined(separator: ", ")
            return .failure(RPCError(
                code: "not_found",
                message: "panel '\(panelName)' not in \(name) manifest (available: [\(available)])",
            ))
        }
        let panelController = PluginPanelController(
            plugin: manifest,
            panelDef: panelDef,
            registry: actionRegistry,
            eventBus: eventBus,
            theme: vc.theme,
        )
        let panelID: String? = switch mode {
        case "split_h":
            vc.splitActivePaneWithPluginPanel(panelController, orientation: .horizontal)
        case "split_v":
            vc.splitActivePaneWithPluginPanel(panelController, orientation: .vertical)
        default: // "tab"
            vc.newPluginPanelTab(panelController)
        }
        guard let panelID else {
            return .failure(RPCError(code: "internal_error", message: "no active tab to split into"))
        }
        return .success(panelID)
    }

    /// `allowFallback: false` makes the default arm return local `unknown_method` instead of
    /// forwarding to the daemon, required for daemon-originated Invokes so they don't recurse.
    /// `silentCompletion: true` suppresses local `<method>.completed` fan-out for daemon-routed
    /// invokes (the daemon publishes that itself).
    private func handleCommand(
        method: String,
        params: [String: Any],
        allowFallback: Bool = true,
        silentCompletion: Bool = false,
        completion: @escaping (Any?) -> Void,
    ) {
        if actionRegistry.tryDispatch(method, params: params, silentCompletion: silentCompletion, completion: completion) {
            return
        }

        guard let vc = tabVC else { completion(nil); return }

        // Browser RPCs pass through the shared core gate before any handler runs. `completion`
        // is REPLACED, not wrapped, so the rule is re-applied on the result: several handlers
        // answer from an async callback, and a read already in flight when a tab entered
        // `protected` must be suppressed.
        var completion = completion
        switch browserGate(method: method, params: params, in: vc, completion: completion) {
        case .notBrowser:
            break
        case let .refused(err):
            completion(err)
            return
        case let .allowed(deliver):
            completion = deliver
        }

        switch method {
        case "system.ping":
            completion(["status": "ok"])

        case "terminal.exec":
            guard let command = params["command"] as? String else { completion(nil); return }
            switch resolveTerminalPanel(params: params, vc: vc) {
            case let .failure(err): completion(err)
            case let .success(panel):
                panel.execCommand(command)
                completion(["ok": true])
            }

        case "terminal.feed":
            guard let text = params["text"] as? String else { completion(nil); return }
            switch resolveTerminalPanel(params: params, vc: vc) {
            case let .failure(err): completion(err)
            case let .success(panel):
                panel.feedText(text)
                completion(["ok": true])
            }

        case "terminal.state":
            switch resolveTerminalPanel(params: params, vc: vc) {
            case let .failure(err): completion(err)
            case let .success(panel): completion(panel.terminalState())
            }

        case "terminal.read":
            switch resolveTerminalPanel(params: params, vc: vc) {
            case let .failure(err): completion(err)
            case let .success(panel): completion(panel.readScreen())
            }

        case "terminal.history":
            let lines = params["lines"] as? Int ?? 100
            switch resolveTerminalPanel(params: params, vc: vc) {
            case let .failure(err): completion(err)
            case let .success(panel): completion(panel.history(lines: lines))
            }

        case "terminal.context":
            let historyLines = params["history_lines"] as? Int ?? 50
            switch resolveTerminalPanel(params: params, vc: vc) {
            case let .failure(err): completion(err)
            case let .success(panel): completion(panel.context(historyLines: historyLines))
            }

        // Diagnostic for the no-focus-theft audit. "Did the frontmost application change"
        // misses focus moving WITHIN copad (a browser command grabbing first responder would
        // swallow keystrokes while every system signal looks clean). This reports what the app
        // believes is focused, so a test can assert it is unchanged across every browser
        // command without touching global input state.
        case "window.focus_state":
            let window = NSApp.keyWindow ?? vc.view.window
            completion([
                "key_window": window?.title ?? "",
                "is_key": window?.isKeyWindow ?? false,
                // Unwrap first: `type(of: x as Any)` on an Optional reports `Optional<...>`, the
                // same string for every responder, which would blind this diagnostic.
                "first_responder": window?.firstResponder.map { String(describing: type(of: $0)) } ?? "none",
                "active_tab": vc.activeIndex,
                "active_pane": vc.activePaneID ?? "",
                "app_active": NSApp.isActive,
            ])

        case "tab.new":
            vc.newTab()
            completion(["ok": true])

        // The work-graph panel (decisions #204): `open` / `mode` set it; the answer is its
        // state, with the page's own read-out under `page`.
        case "comux.graph":
            if params["open"] != nil || params["mode"] != nil {
                let open = (params["open"] as? Bool) ?? true
                vc.setComuxGraph(open: open, mode: params["mode"] as? String, animated: false)
            }
            vc.comuxGraphStatus { completion($0) }

        // The notification dropdown under the top bar's bell (decisions #204).
        case "comux.notifications":
            if let open = params["open"] as? Bool { vc.setComuxNotifications(open: open) }
            completion(vc.comuxNotificationsStatus())

        // Open a remote comux natively (decisions #171). No `dest` = ask for it.
        case "comux.open_remote":
            let comux = (params["comux"] as? String) ?? "comux"
            guard let dest = params["dest"] as? String, !dest.isEmpty else {
                openRemoteComuxPrompt()
                completion(["ok": true, "prompted": true])
                return
            }
            if let err = vc.openRemoteComux(dest: dest, comux: comux) {
                completion(["ok": false, "error": err])
            } else {
                completion(["ok": true])
            }

        case "tab.close":
            vc.closeActivePane()
            completion(["ok": true])

        case "tab.switch":
            guard let index = params["index"] as? Int else { completion(nil); return }
            vc.switchTab(to: index)
            completion(["ok": true])

        // Focus one pane from anywhere and bring the app forward, for the notification jump
        // path: a toast raised by an agent in a comux pane knows the `COPAD_PANEL_ID` of the
        // hosting copad tab, and activating the APPLICATION alone would land on whichever tab
        // was active. One call rather than `tab.list` + index arithmetic, since the list can
        // change between the two.
        case "panel.focus":
            guard let panelID = params["panel_id"] as? String, !panelID.isEmpty else {
                completion(RPCError(
                    code: "invalid_params",
                    message: "panel.focus requires a non-empty `panel_id` string"))
                return
            }
            let focused = vc.activatePanel(id: panelID)
            if focused {
                // Order matters: activate the app first, then raise the window, or the
                // window comes forward behind whatever was already frontmost.
                NSApp.activate(ignoringOtherApps: true)
                vc.view.window?.makeKeyAndOrderFront(nil)
            }
            // `focused: false` is a SUCCESSFUL call that found nothing, not an error: the
            // panel id may belong to another copad instance, and the caller must tell that
            // from a broken request. `raised` is separate because on Linux the two come apart
            // (GTK under Wayland switches the tab but can't raise its window); AppKit can, so
            // the caller skips its compositor-level raise. Sent explicitly so the reader's
            // back-compat default doesn't become load-bearing.
            completion(["ok": true, "focused": focused, "raised": focused])

        case "tab.list":
            completion(vc.tabList())

        case "tab.info":
            completion(vc.tabInfo())

        case "tab.rename":
            guard let title = params["title"] as? String else { completion(nil); return }
            let index = params["index"] as? Int ?? vc.activeIndex
            vc.renameTab(at: index, title: title)
            completion(["ok": true])

        case "split.horizontal":
            vc.splitActivePane(orientation: .horizontal)
            completion(["ok": true])

        case "split.vertical":
            vc.splitActivePane(orientation: .vertical)
            completion(["ok": true])

        case "pane.focus_next":
            vc.focusNextPane(direction: 1)
            completion(["status": "ok"])

        case "pane.focus_prev":
            vc.focusNextPane(direction: -1)
            completion(["status": "ok"])

        case "session.list":
            completion(vc.sessionList())

        case "session.info":
            let index = params["index"] as? Int ?? vc.activeIndex
            completion(vc.sessionInfo(index: index))

        case "terminal.shell_precmd":
            let panelID = params["panel_id"] as? String ?? vc.activeTerminalPanel?.panelID ?? ""
            eventBus.broadcast(event: "terminal.shell_precmd", data: ["panel_id": panelID])
            completion(["ok": true])

        case "terminal.shell_preexec":
            let panelID = params["panel_id"] as? String ?? vc.activeTerminalPanel?.panelID ?? ""
            eventBus.broadcast(event: "terminal.shell_preexec", data: ["panel_id": panelID])
            completion(["ok": true])

        case "agent.approve":
            guard let message = params["message"] as? String else { completion(nil); return }
            let title = params["title"] as? String ?? "Agent Action"
            let actions = params["actions"] as? [String] ?? ["Approve", "Deny"]
            guard let win = window else { completion(["error": "no window"]); return }
            let alert = NSAlert()
            alert.messageText = title
            alert.informativeText = message
            for action in actions {
                alert.addButton(withTitle: action)
            }
            alert.beginSheetModal(for: win) { response in
                let idx = response.rawValue - 1000
                let chosen = actions.indices.contains(idx) ? actions[idx] : actions.last ?? "Deny"
                completion(["action": chosen, "index": idx])
            }
            // completion called async from sheet modal callback above — do not call here

        case "tabs.toggle_bar":
            vc.toggleTabBar(userInitiated: true)
            completion(["ok": true, "collapsed": vc.isTabBarCollapsed])

        case "background.set":
            guard let path = params["path"] as? String else { completion(nil); return }
            let tint = params["tint"] as? Double ?? 0.6
            let opacity = params["opacity"] as? Double ?? 1.0
            vc.applyBackground(path: path, tint: tint, opacity: opacity)
            // A manual pick restarts the countdown so the timer doesn't replace it a moment
            // later (Linux handle_bg_set).
            armRotation()
            completion(["ok": true])

        case "background.set_tint":
            guard let tint = params["tint"] as? Double else { completion(nil); return }
            vc.setTint(tint)
            completion(["ok": true])

        case "background.clear":
            vc.clearBackground()
            completion(["ok": true])

        // Same wire shape as Linux: both commands return `{status, mode}` so trigger configs can
        // detect deactive state without parsing messages.
        case "background.next":
            guard BackgroundRotator.isActive else {
                completion(["status": "ok", "mode": "deactive"])
                return
            }
            guard let img = BackgroundRotator.nextRandomImage() else {
                completion(RPCError(
                    code: "no_wallpapers",
                    message: "wallpaper list missing or empty (tried ~/Library/Caches/copad/wallpapers.txt and ~/.cache/terminal-wallpapers.txt)",
                ))
                return
            }
            vc.applyBackground(
                path: img,
                tint: vc.currentBackgroundTint,
                opacity: vc.currentBackgroundOpacity,
                fromList: true,
            )
            armRotation()
            completion(["status": "ok", "mode": "active", "path": img])

        case "background.toggle":
            let nowActive = BackgroundRotator.toggle()
            if nowActive {
                if let img = BackgroundRotator.nextRandomImage() {
                    vc.applyBackground(
                        path: img,
                        tint: vc.currentBackgroundTint,
                        opacity: vc.currentBackgroundOpacity,
                        fromList: true,
                    )
                }
            } else {
                vc.clearBackground()
            }
            armRotation()
            completion(["status": "ok", "mode": nowActive ? "active" : "deactive"])

        // Delete the displayed list-picked wallpaper from disk AND the list file(s), then
        // rotate. Only rotation / `next` / `toggle` picks; a manually `set` image or
        // `[background] image` is never deleted (Linux handle_bg_delete_current).
        case "background.delete_current":
            guard vc.currentBackgroundFromList, let img = vc.currentBackgroundPath else {
                completion(RPCError(
                    code: "no_current",
                    message: "No list-picked background to delete (manual/static images are never deleted)",
                ))
                return
            }
            do {
                try FileManager.default.removeItem(atPath: img)
            } catch let err as NSError where !(err.domain == NSCocoaErrorDomain && err.code == NSFileNoSuchFileError) {
                completion(RPCError(code: "io_error", message: "delete \(img): \(err.localizedDescription)"))
                return
            } catch {
                // NotFound: already gone, continue to list removal + rotation.
            }
            BackgroundRotator.removeFromList(img)
            if let next = BackgroundRotator.nextRandomImage() {
                vc.applyBackground(
                    path: next,
                    tint: vc.currentBackgroundTint,
                    opacity: vc.currentBackgroundOpacity,
                    fromList: true,
                )
                armRotation()
                completion(["status": "ok", "deleted": img, "next": next])
            } else {
                vc.clearBackground()
                completion(["status": "ok", "deleted": img, "next": NSNull()])
            }

        // MARK: WebView commands

        case "webview.open":
            let urlString = params["url"] as? String
            let url = urlString.flatMap { s -> URL? in
                let final = s.hasPrefix("http://") || s.hasPrefix("https://") || s.hasPrefix("file://") ? s : "https://" + s
                return URL(string: final)
            }
            let mode = params["mode"] as? String ?? "tab"
            // `background: true` creates the copad tab without switching to it. `mode` keeps
            // its meaning (a new COPAD tab or a split); changing `mode: "tab"` would break
            // existing callers. A tab inside an existing browser pane is `webview.tab.new`.
            let background = params["background"] as? Bool ?? false
            switch mode {
            case "split_h":
                vc.splitActivePaneWithWebView(
                    url: url, orientation: .horizontal, background: background,
                )
            case "split_v":
                vc.splitActivePaneWithWebView(
                    url: url, orientation: .vertical, background: background,
                )
            default: // "tab"
                vc.newWebViewTab(url: url, background: background)
            }
            completion(["ok": true])

        case "webview.navigate":
            guard let urlString = params["url"] as? String else {
                completion(RPCError(code: "invalid_params", message: "Missing 'url' param"))
                return
            }
            switch resolveBrowserTab(params, in: vc) {
            case let .failure(err): completion(err)
            case let .success(t):
                t.tab.navigate(to: urlString)
                completion(["status": "ok"])
            }

        case "webview.back":
            switch resolveBrowserTab(params, in: vc) {
            case let .failure(err): completion(err)
            case let .success(t):
                t.tab.goBack()
                completion(["status": "ok"])
            }

        case "webview.forward":
            switch resolveBrowserTab(params, in: vc) {
            case let .failure(err): completion(err)
            case let .success(t):
                t.tab.goForward()
                completion(["status": "ok"])
            }

        case "webview.reload":
            switch resolveBrowserTab(params, in: vc) {
            case let .failure(err): completion(err)
            case let .success(t):
                t.tab.reload()
                completion(["status": "ok"])
            }

        case "webview.execute_js":
            // Param name is `code` (Linux + copad-cli convention); `script` is accepted as a
            // fallback for older macOS-only callers.
            guard let code = (params["code"] as? String) ?? (params["script"] as? String) else {
                completion(RPCError(code: "invalid_params", message: "Missing 'code' param"))
                return
            }
            switch resolveBrowserTab(params, in: vc) {
            case let .failure(err): completion(err)
            case let .success(t):
                t.tab.executeJS(code) { result, error in
                    if let error {
                        completion(RPCError(code: "js_error", message: error.localizedDescription))
                    } else {
                        completion(["result": result ?? NSNull()])
                    }
                }
            }

        case "webview.get_content":
            switch resolveBrowserTab(params, in: vc) {
            case let .failure(err): completion(err)
            case let .success(t):
                t.tab.getContent { html in
                    completion(["html": html])
                }
            }

        case "webview.devtools":
            // Linux accepts `action: show/close/attach/detach`. WKWebView has no public API to
            // open the inspector; `developerExtrasEnabled` only enables right-click, Inspect
            // Element. The verb is accepted for parity: show/attach/detach mean "ensure
            // enabled", close is a no-op.
            let action = (params["action"] as? String) ?? "show"
            switch resolveBrowserTab(params, in: vc) {
            case let .failure(err): completion(err)
            case let .success(t):
                switch action {
                case "show", "attach", "detach", "toggle":
                    // The TARGET tab's configuration, not the pane's active one: every other
                    // page-level command honours `tab_id`, and toggling a different tab's
                    // inspector would be a surprise.
                    t.tab.toggleDevTools()
                    completion(["status": "ok"])
                case "close":
                    completion(["status": "ok"])
                default:
                    completion(RPCError(
                        code: "invalid_params",
                        message: "Unknown action: \(action). Use show/close/attach/detach/toggle",
                    ))
                }
            }

        case "webview.state":
            switch resolveBrowserTab(params, in: vc) {
            case let .failure(err): completion(err)
            case let .success(t):
                completion([
                    "tab_id": t.tab.id,
                    "url": t.tab.currentURL,
                    "title": t.tab.title,
                    "can_go_back": t.tab.canGoBack,
                    "can_go_forward": t.tab.canGoForward,
                    "is_loading": t.tab.isLoading,
                ])
            }

        // Webview interaction: each command builds the JS snippet (mirroring
        // copad-linux/src/webview.rs::js) and runs it via the executeJS bridge; the JSON-string
        // result is parsed back into `Any` so the wire format matches Linux.
        case "webview.query":
            guard let selector = params["selector"] as? String else {
                completion(RPCError(code: "invalid_params", message: "Missing 'selector' param"))
                return
            }
            runWebViewJS(WebViewJS.querySelector(selector), params: params, in: vc, completion: completion)

        case "webview.query_all":
            guard let selector = params["selector"] as? String else {
                completion(RPCError(code: "invalid_params", message: "Missing 'selector' param"))
                return
            }
            let limit = (params["limit"] as? Int) ?? 50
            runWebViewJS(WebViewJS.querySelectorAll(selector, limit: limit), params: params, in: vc, completion: completion)

        case "webview.get_styles":
            guard let selector = params["selector"] as? String else {
                completion(RPCError(code: "invalid_params", message: "Missing 'selector' param"))
                return
            }
            let properties = (params["properties"] as? [String]) ?? []
            runWebViewJS(WebViewJS.getStyles(selector, properties: properties), params: params, in: vc, completion: completion)

        case "webview.click":
            guard let selector = params["selector"] as? String else {
                completion(RPCError(code: "invalid_params", message: "Missing 'selector' param"))
                return
            }
            runWebViewJS(WebViewJS.click(selector), params: params, in: vc, completion: completion)

        case "webview.fill":
            guard let selector = params["selector"] as? String else {
                completion(RPCError(code: "invalid_params", message: "Missing 'selector' param"))
                return
            }
            guard let value = params["value"] as? String else {
                completion(RPCError(code: "invalid_params", message: "Missing 'value' param"))
                return
            }
            runWebViewJS(WebViewJS.fill(selector, value: value), params: params, in: vc, completion: completion)

        case "webview.scroll":
            let selector = params["selector"] as? String
            let x = (params["x"] as? Int) ?? 0
            let y = (params["y"] as? Int) ?? 0
            runWebViewJS(WebViewJS.scroll(selector: selector, x: x, y: y), params: params, in: vc, completion: completion)

        case "webview.page_info":
            runWebViewJS(WebViewJS.pageInfo(), params: params, in: vc, completion: completion)

        // Backstop for the set `browserGate` short-circuits above, so a caller reaching this
        // switch by another route still gets the truthful answer rather than `unknown_method`.
        case let m where Self.unimplementedBrowserMethods.contains(m):
            completion(RPCError(
                code: "unsupported_capability",
                message: "\(method) is not implemented on macOS yet",
            ))

        case "webview.tab.new":
            switch resolveBrowserTab(params, in: vc) {
            case let .failure(err): completion(err)
            case let .success(t):
                let raw = params["url"] as? String
                let url = raw.flatMap { s -> URL? in
                    let final = s.hasPrefix("http://") || s.hasPrefix("https://") || s.hasPrefix("file://")
                        ? s : "https://" + s
                    return URL(string: final)
                }
                // Background by DEFAULT here, unlike the toolbar's new-tab: agents reach this
                // method, and must not change what the human is looking at.
                let background = params["background"] as? Bool ?? true
                guard let tabID = t.pane.newTab(url: url, background: background) else {
                    completion(RPCError(
                        code: "refused",
                        message: "this pane already holds the maximum of \(WebViewController.maxTabs) tabs",
                    ))
                    return
                }
                completion(["tab_id": tabID, "panel_id": t.pane.panelID])
            }

        case "webview.tab.list":
            switch resolveBrowserTab(params, in: vc) {
            case let .failure(err): completion(err)
            case let .success(t):
                // A protected tab contributes BOOKKEEPING only. `tab.list` is `Meta` in core
                // (an agent must always be able to discover that a tab exists and is
                // protected), but url and title are page-derived, exactly what `webview.state`
                // is refused for; returning them would route an OAuth code or reset-token URL
                // around the protection. Profile-wide, not per-tab: sibling tabs share the
                // authenticated session, so an agent could navigate one to a sensitive endpoint
                // and read its URL back here.
                let anyProtected = WebViewController.anyProtected
                let list = t.pane.tabs.map { tab in
                    let isProtected = anyProtected || tab.tabMode == .protected
                    return [
                        "id": tab.id,
                        "url": isProtected ? "" : tab.currentURL,
                        "title": isProtected ? "" : tab.title,
                        "loading": tab.isLoading,
                        "mode": tab.tabMode.rawValue,
                    ] as [String: Any]
                }
                completion([
                    "panel_id": t.pane.panelID,
                    "tabs": list,
                    "active": t.pane.activeIndex,
                ])
            }

        case "webview.tab.select":
            guard let tabID = params["tab_id"] as? String, !tabID.isEmpty else {
                completion(RPCError(code: "invalid_params", message: "Missing 'tab_id' param"))
                return
            }
            switch resolveBrowserTab(params, in: vc) {
            case let .failure(err): completion(err)
            case let .success(t):
                if t.pane.selectTab(id: tabID) {
                    completion(["status": "ok", "active": t.pane.activeIndex])
                } else {
                    completion(RPCError(code: "tab_closed", message: "Tab not found: \(tabID)"))
                }
            }

        case "webview.tab.close":
            guard let tabID = params["tab_id"] as? String, !tabID.isEmpty else {
                completion(RPCError(code: "invalid_params", message: "Missing 'tab_id' param"))
                return
            }
            switch resolveBrowserTab(params, in: vc) {
            case let .failure(err): completion(err)
            case let .success(t):
                if let problem = t.pane.closeTab(id: tabID) {
                    // Refusing the last tab is deliberate: closing the PANE is the tab
                    // manager's job, and a pane with no tabs has no coherent snapshot.
                    completion(RPCError(code: "refused", message: problem))
                } else {
                    completion(["status": "ok", "remaining": t.pane.tabs.count])
                }
            }

        case "webview.tab.move":
            guard let tabID = params["tab_id"] as? String, !tabID.isEmpty else {
                completion(RPCError(code: "invalid_params", message: "Missing 'tab_id' param"))
                return
            }
            guard let index = params["index"] as? Int else {
                completion(RPCError(code: "invalid_params", message: "Missing 'index' param"))
                return
            }
            switch resolveBrowserTab(params, in: vc) {
            case let .failure(err): completion(err)
            case let .success(t):
                if let problem = t.pane.moveTab(id: tabID, to: index) {
                    completion(RPCError(code: "not_found", message: problem))
                } else {
                    completion(["status": "ok", "active": t.pane.activeIndex])
                }
            }

        case "webview.tab.protect":
            switch resolveBrowserTab(params, in: vc) {
            case let .failure(err): completion(err)
            case let .success(t):
                let on = params["on"] as? Bool ?? true
                // Answer only once the rebuild has finished. The transition tears the old web
                // view down, awaits an async cache purge and builds a new one; replying early
                // left `webView` nil and a following `reload`/`navigate`/`click` (all allowed
                // under protection) crashed.
                t.tab.setProtected(on) {
                    completion([
                        "mode": t.tab.tabMode.rawValue,
                        "document_generation": t.tab.documentGeneration,
                    ])
                }
            }

        case "browser.secret.list":
            // Metadata only: `CredentialRef` has no field that could carry the secret, which is
            // why this is safe to answer to an agent.
            let origin = params["origin"] as? String
            completion(["credentials": BrowserSecrets.list(origin: origin).map(\.wire)])

        case "browser.secret.fill":
            guard let credentialID = params["credential_id"] as? String, !credentialID.isEmpty else {
                completion(RPCError(code: "invalid_params", message: "Missing 'credential_id' param"))
                return
            }
            switch resolveBrowserTab(params, in: vc) {
            case let .failure(err): completion(err)
            case let .success(t):
                guard let credential = BrowserSecrets.find(id: credentialID) else {
                    completion(RPCError(code: "not_found", message: "No such credential: \(credentialID)"))
                    return
                }
                t.tab.fillCredential(credential) { result in
                    switch result {
                    case let .failure(err): completion(err)
                    case let .success(selector):
                        // The selector, never the value; no shape of this response carries the
                        // secret.
                        completion(["ok": true, "filled": [selector]])
                    }
                }
            }

        case "browser.secret.save":
            // The SECRET is read natively from the protected page, never a parameter, so an
            // agent calling this never handles or sees it.
            guard let username = params["username"] as? String, !username.isEmpty else {
                completion(RPCError(code: "invalid_params", message: "Missing 'username' param"))
                return
            }
            switch resolveBrowserTab(params, in: vc) {
            case let .failure(err): completion(err)
            case let .success(t):
                BrowserSecrets.saveFromPage(tab: t.tab, username: username) { result in
                    switch result {
                    case let .failure(err): completion(err)
                    case let .success(ref): completion(["credential": ref.wire])
                    }
                }
            }

        case "browser.secret.delete":
            guard let credentialID = params["credential_id"] as? String, !credentialID.isEmpty else {
                completion(RPCError(code: "invalid_params", message: "Missing 'credential_id' param"))
                return
            }
            BrowserSecrets.delete(id: credentialID) { result in
                switch result {
                case let .failure(err): completion(err)
                case let .success(removed): completion(["removed": removed])
                }
            }

        case "webview.net", "webview.console":
            switch resolveBrowserTab(params, in: vc) {
            case let .failure(err): completion(err)
            case let .success(t):
                var query: [String: Any] = [
                    "kind": method == "webview.net" ? "net" : "console",
                    "limit": params["limit"] as? Int ?? 200,
                ]
                if let since = params["since"] as? Int { query["since"] = since }
                if let contains = params["filter"] as? String { query["contains"] = contains }
                if let level = params["level"] as? String { query["level"] = level }
                // A `tab_id` scopes the READ as well as the target, so a background tab's
                // activity is answerable without selecting it.
                if let tabID = params["tab_id"] as? String, !tabID.isEmpty {
                    query["tab_id"] = tabID
                }
                let result = BrowserFFI.readLog(panelID: t.pane.panelID, query: query)
                completion([
                    "records": result.records,
                    // Declared on every response: fetch/XHR patching is not a packet log
                    // (subresources, sendBeacon, WebSocket frames and service-worker traffic
                    // are invisible), so an empty list must never read as "no request was made".
                    "coverage": result.coverage,
                ])
            }

        case "webview.clear_log":
            switch resolveBrowserTab(params, in: vc) {
            case let .failure(err): completion(err)
            case let .success(t):
                completion(["removed": BrowserFFI.clearLog(panelID: t.pane.panelID)])
            }

        case "webview.screenshot":
            switch resolveBrowserTab(params, in: vc) {
            case let .failure(err): completion(err)
            case let .success(t):
                let config = WKSnapshotConfiguration()
                // Default rect = visible area at full resolution (Linux's
                // SnapshotRegion::Visible, modulo pixel density). Works on a BACKGROUND tab
                // (`takeSnapshot` was measured to render correctly with no window), so
                // capturing it neither raises it nor disturbs the user.
                t.tab.webView.takeSnapshot(with: config) { image, error in
                    if let error {
                        completion(RPCError(code: "snapshot_failed", message: error.localizedDescription))
                        return
                    }
                    guard let image, image.size.width > 1, image.size.height > 1 else {
                        // A zero-sized snapshot means the view had no frame; say so rather than
                        // blaming the PNG encoder.
                        completion(RPCError(
                            code: "snapshot_failed",
                            message: "the tab has no rendered size yet",
                        ))
                        return
                    }
                    guard let tiff = image.tiffRepresentation,
                          let bitmap = NSBitmapImageRep(data: tiff),
                          let png = bitmap.representation(using: .png, properties: [:])
                    else {
                        completion(RPCError(code: "snapshot_failed", message: "could not encode PNG"))
                        return
                    }
                    completion([
                        "image_b64": png.base64EncodedString(),
                        "width": Int(image.size.width),
                        "height": Int(image.size.height),
                    ])
                }
            }

        case "plugin.open":
            // params: name (plugin name), panel (default "main"), mode (default "tab", also
            // "split_h"/"split_v"), the same vocabulary as `webview.open`.
            guard let name = params["name"] as? String else {
                completion(RPCError(code: "invalid_params", message: "Missing 'name' param"))
                return
            }
            let panelName = (params["panel"] as? String) ?? "main"
            let mode = (params["mode"] as? String) ?? "tab"
            switch openPluginPanel(name: name, panelName: panelName, mode: mode, vc: vc) {
            case let .success(panelID):
                completion(["status": "ok", "panel_id": panelID])
            case let .failure(error):
                completion(error)
            }

        case "statusbar.show":
            if let bar = vc.statusBar {
                completion(["visible": bar.setShown(true)])
            } else {
                completion(["visible": false, "note": "statusbar disabled in config"])
            }

        case "statusbar.hide":
            if let bar = vc.statusBar {
                completion(["visible": bar.setShown(false)])
            } else {
                completion(["visible": false, "note": "statusbar disabled in config"])
            }

        case "statusbar.toggle":
            if let bar = vc.statusBar {
                completion(["visible": bar.setShown(!bar.isShown)])
            } else {
                completion(["visible": false, "note": "statusbar disabled in config"])
            }

        default:
            if allowFallback {
                actionRegistry.tryDispatchOrFallback(method, params: params, completion: completion)
            } else {
                completion(RPCError(code: "unknown_method", message: "no local handler for \(method) (daemon-invoke path, fallback disabled)"))
            }
        }
    }

    /// Resolve the target webview, evaluate the JS snippet, parse the JSON-string result and
    /// pass it to completion (Linux's `run_js_command`).
    private func runWebViewJS(
        _ js: String,
        params: [String: Any],
        in vc: TabViewController,
        completion: @escaping (Any?) -> Void,
    ) {
        switch resolveBrowserTab(params, in: vc) {
        case let .failure(err):
            completion(err)
        case let .success(t):
            t.tab.executeJS(js) { result, error in
                if let error {
                    completion(RPCError(code: "js_error", message: error.localizedDescription))
                    return
                }
                // The snippets always JSON.stringify, so the completion gives a String; decode
                // it back into [String: Any] / [Any] / scalar.
                guard let str = result as? String else {
                    // Fallback: hand the raw value back for JS that returns a non-string (as
                    // Linux does).
                    completion(["result": result ?? NSNull()])
                    return
                }
                guard let data = str.data(using: .utf8),
                      let parsed = try? JSONSerialization.jsonObject(with: data, options: [.fragmentsAllowed])
                else {
                    completion(["raw": str])
                    return
                }
                completion(parsed)
            }
        }
    }

    /// Browser methods that are WIRED but not yet built on macOS. Registering them gives a
    /// `coctl` script a machine-readable "not implemented yet" instead of `unknown_method`,
    /// which looks like a typo.
    static let unimplementedBrowserMethods: Set<String> = [
        "webview.profile.list", "webview.profile.clear",
    ]

    /// Outcome of the shared browser authorization gate.
    private enum BrowserGateOutcome {
        /// Not a browser method — the dispatcher proceeds untouched.
        case notBrowser
        case refused(RPCError)
        /// Proceed, but deliver results through this replacement completion.
        case allowed(deliver: (Any?) -> Void)
    }

    /// Ask `copad-core` whether a browser RPC may run (`browser::authorize`, reached over FFI
    /// so Linux and macOS cannot answer differently), and prepare the delivery path.
    ///
    /// Two checks: at dispatch (refuse before the handler runs) and at delivery (re-ask, since
    /// `execute_js`, `screenshot`, `get_content` and the JS-snippet commands answer from an
    /// async callback, and a read issued just before a tab entered `protected` must not hand
    /// back the page).
    ///
    /// A permitted write has its result, and its error, replaced with a fixed response:
    /// `click` reporting "ok" vs "not found" is a selector oracle (probing
    /// `input[name=csrf][value^="a"]`, then `^="ab"`, reads a protected page one character at a
    /// time), and a refused-vs-succeeded distinction leaks the same bit.
    private func browserGate(
        method: String,
        params: [String: Any],
        in vc: TabViewController,
        completion: @escaping (Any?) -> Void,
    ) -> BrowserGateOutcome {
        guard method.hasPrefix("webview.") || method.hasPrefix("browser.") else {
            return .notBrowser
        }

        if Self.unimplementedBrowserMethods.contains(method) {
            return .refused(RPCError(
                code: "unsupported_capability",
                message: "\(method) is not implemented on macOS yet",
            ))
        }

        // A command with no resolvable target (`webview.open` creates one) is judged against
        // the profile only. The target is captured WEAKLY so the delivery check re-reads its
        // mode.
        let target: BrowserTab? = switch resolveBrowserTab(params, in: vc) {
        case let .success(pair): pair.tab
        case .failure: nil
        }
        let hadTarget = target != nil

        // `WebViewController.anyProtected`, not a walk over this window's panes: capture, event
        // and persistence paths ask the registry, and a second implementation of the predicate
        // can disagree (the window-walk did once; see `profileHasProtectedTab`: a pane in an
        // inactive copad tab has no `view.window`). The RPC gate must not be more permissive
        // than the capture script.
        let decision = BrowserFFI.authorize(
            method: method,
            mode: (target?.tabMode ?? .automation).rawValue,
            profileProtected: WebViewController.anyProtected,
        )
        guard decision.allowed else {
            return .refused(RPCError(
                code: decision.code ?? "tab_protected",
                message: decision.message ?? "refused by browser policy",
            ))
        }

        return .allowed(deliver: { [weak target, weak vc] result in
            // Re-evaluate against the state as it is NOW; the mode captured at dispatch is stale
            // by definition. Fail closed when the target or window went away mid-flight: a torn
            // down pane can't be asked its mode, and the profile scan only sees panels still in
            // the window, so "not found" must not read as "was never protected".
            guard let vc else {
                completion(RPCError(
                    code: "tab_closed",
                    message: "the window went away before \(method) could answer",
                ))
                return
            }
            if hadTarget, target == nil {
                completion(RPCError(
                    code: "tab_closed",
                    message: "the target pane closed before \(method) could answer",
                ))
                return
            }
            let now = BrowserFFI.authorize(
                method: method,
                mode: (target?.tabMode ?? .automation).rawValue,
                profileProtected: WebViewController.anyProtected,
            )
            guard now.allowed else {
                completion(RPCError(
                    code: now.code ?? "tab_protected",
                    message: now.message ?? "refused by browser policy",
                ))
                return
            }
            if now.opaqueWrite {
                completion(["status": "ok", "protected": true])
                return
            }
            completion(result)
        })
    }

    /// Resolves the target TAB for a browser command: `id` picks the pane (absent: the active
    /// pane), `tab_id` the tab within it (absent: that pane's active tab). Every page-level
    /// method takes a tab target because selecting a tab to operate on it would change what the
    /// user sees. It resolves ONCE, before any await, so a changed tab list can't silently
    /// retarget; `tab_closed` is the honest answer.
    private func resolveBrowserTab(
        _ params: [String: Any],
        in vc: TabViewController,
    ) -> Result<(pane: WebViewController, tab: BrowserTab), RPCError> {
        let pane: WebViewController
        if let id = params["id"] as? String, !id.isEmpty {
            guard let panel = vc.panel(id: id) else {
                return .failure(RPCError(code: "not_found", message: "Panel not found: \(id)"))
            }
            guard let webVC = panel as? WebViewController else {
                return .failure(RPCError(code: "wrong_panel_type", message: "Panel is not a webview"))
            }
            pane = webVC
        } else {
            // Linux's handlers require `id`; macOS defaults leniently so id-less coctl calls
            // keep working.
            guard let webVC = vc.activeWebView else {
                return .failure(RPCError(
                    code: "no_active_webview",
                    message: "No active webview and no 'id' provided",
                ))
            }
            pane = webVC
        }

        guard !pane.tabs.isEmpty else {
            return .failure(RPCError(code: "tab_closed", message: "the pane has no tabs"))
        }
        if let tabID = params["tab_id"] as? String, !tabID.isEmpty {
            guard let tab = pane.tab(id: tabID) else {
                return .failure(RPCError(code: "tab_closed", message: "Tab not found: \(tabID)"))
            }
            return .success((pane, tab))
        }
        return .success((pane, pane.activeTab))
    }
}
