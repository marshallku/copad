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
    /// PR 9 / parity-plan Tier 2.2 — tracks active panel + per-panel cwd
    /// for `{context.active_panel}` / `{context.active_cwd}` trigger
    /// interpolation. Updated synchronously inside `eventBus.onBroadcast`
    /// before each dispatch; see comment near the wire-up below for the
    /// "apply-before-dispatch" ordering rationale (matches Linux's
    /// `Pump::pump_all` pattern).
    private let contextService = ContextService()
    /// App-lifetime agent-status model behind the cockpit panel. One pump feeds
    /// it (`startAgentCockpitPump`); cockpit views observe it. See
    /// `docs/agent-cockpit.md`.
    private let agentCockpit = AgentCockpitModel()
    private weak var cockpitPanel: CockpitViewController?
    /// PR 5c — Rust trigger engine via FFI. Lazy because the underlying
    /// copad_engine_create() must run AFTER process startup; constructing it
    /// at property-init time risks a cold-launch race. Created the first
    /// time `applicationDidFinishLaunching` references it.
    private lazy var copadEngine = CopadEngine()
    private var configWatcher: ConfigWatcher?
    /// Tier 1.2 — compiled keybindings + the active NSEvent monitor token.
    /// Hot-reload swaps `keybindings` in place; the monitor closure reads
    /// the latest snapshot via `self`.
    private var keybindings: [Keybindings.Binding] = []
    private var keybindingMonitor: Any?
    /// Cmd+Shift+P palette controller. Held for the sheet's lifetime —
    /// `NSTableView` data source / delegate are unowned references, so a
    /// transient stack-only controller would deallocate the moment `open`
    /// returns and the table would go blank. Cleared from `endSheet`
    /// completion in `CommandPaletteController.open`.
    private var commandPaletteController: CommandPaletteController?
    /// Constructed inside `applicationDidFinishLaunching`. Background
    /// reconnect loop is owned by the client.
    private var daemonClient: DaemonClient?
    /// Native wallpaper rotation (parity with Linux's glib timer in
    /// `copad-linux/src/background.rs`). `rotationTimer` ticks every
    /// `liveRotateInterval` seconds; `armRotation` (re)builds it. The live
    /// interval is tracked separately so manual `set`/`next`/`toggle` can
    /// restart the countdown without re-reading config.
    private var rotationTimer: DispatchSourceTimer?
    private var liveRotateInterval: UInt = 0

    func applicationDidFinishLaunching(_: Notification) {
        // PR 1 (Tier 2.1) FFI smoke test. Proves the Rust staticlib linked
        // correctly and a JSON round-trip survives the C-ABI boundary.
        // Remove once Tier 2.4 (TriggerEngine via FFI) replaces it with real
        // engine startup.
        CopadFFI.runSmokeTest()
        // Phase 1 (renderer migration) — sibling staticlib smoke. Proves
        // libcopad_term.a links alongside libcopad_ffi.a and the
        // handle/snapshot ABI round-trips. Real PTY + grid lands in Phase 2.
        CopadTermFFI.runSmokeTest()

        // PR 7 — wire the registry's completion fan-out bus BEFORE anything
        // registers an action handler. This way the very first dispatch
        // (whether from a coctl that races the socket startup or from an
        // onStartup plugin's first action) gets `<method>.completed` /
        // `.failed` broadcast on the same bus the trigger engine listens to.
        // Idempotent; mirrors Linux's `with_completion_bus(bus)` constructor
        // pattern but applied via setter so we don't have to construct
        // `eventBus` before `actionRegistry` (Swift property init order).
        actionRegistry.setEventBus(eventBus)

        // PR 2 (Tier 2.3) registry seam — register first-party actions so the
        // socket dispatcher can hand off to them via tryDispatch BEFORE the
        // legacy switch fires. Plugin host (PR 3) and trigger engine (PR 5)
        // will register additional actions through this same path.
        registerBuiltinActions()

        // Daemon owns plugin host + registry. Daemon-down ⇒ plugin/registry
        // actions surface `daemon_unavailable` (local engine still fires
        // GUI-only triggers).
        daemonClient = DaemonClient(
            socket: CopadPaths.daemonSocket(),
            capabilities: ["tab", "split", "webview", "background", "statusbar", "terminal", "agent.ui", "plugin.open", "search", "session"],
            eventBus: eventBus,
        )
        // Capture engine on main actor so the @Sendable closure body has
        // a sendable reference (CopadEngine itself is `@unchecked Sendable`).
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
        // Daemon `Event` (notifications, plugin completion fan-out, etc.)
        // republishes onto our local bus with a fresh bridge_id. The
        // bridge_id marks the event as "already crossed" so PR 4b's
        // outbound forwarder will skip it instead of echoing back.
        // Local copadEngine still fires triggers from this republish —
        // when daemon owns triggers (`host_triggers=true`), PR 4b cuts
        // the local engine over to suppress double-fire.
        // Capture `eventBus` directly — `AppDelegate` is `@MainActor`, so a
        // `@Sendable` closure capturing `self` would violate isolation when
        // invoked from `DaemonClient`'s reader thread. `EventBus` is
        // `@unchecked Sendable` and broadcast is thread-safe.
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
        // Install invoke handler BEFORE start() so the first inbound Invoke
        // routes through handleCommand instead of being dropped.
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
                // nil → unknown_method (handlers `completion(nil)` for both
                // missing handlers and missing params); encoding it as
                // ok:true would surface failures as silent daemon successes.
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
                    // Never silently drop a reply — daemon would wait until
                    // its per-method timeout and the admission slot would
                    // hold to the safety net.
                    let fallback = "{\"id\":\"\(id)\",\"ok\":false,\"error\":{\"code\":\"internal_error\",\"message\":\"failed to encode response for \(method)\"}}\n"
                    reply(fallback)
                }
            }
        }
        // `daemonClient.start()` is deliberately deferred until after the
        // initial tab exists — see start call site below. An Invoke
        // arriving before the GUI state is ready would silently no-op
        // through the `guard let vc = tabVC` arm.

        // Initial load: parse failure falls back to `.defaults` so the
        // app still starts — better than refusing to launch on a typo
        // in the user's config. `handleConfigChange` (hot reload) takes
        // the stricter path and preserves the previous live config.
        let config = (try? CopadConfig.load()) ?? .defaults
        let theme = CopadTheme.byName(config.themeName) ?? .default

        // PR 5c (Tier 2.4) trigger engine via FFI — wire EventBus broadcasts
        // (including plugin event.publish forwards) into the Rust trigger
        // engine, which fires actions via the ActionRegistry callback.
        // Order: registry must already exist (PR 2), supervisor must already
        // have registered plugin provides[] (above) so triggers can target
        // plugin actions on the very first event, config must be loaded so
        // the [[triggers]] array is available.
        copadEngine.actionRegistry = actionRegistry
        eventBus.onBroadcast = { [weak copadEngine, contextService] kind, source, data, origin in
            // EventBus.broadcast can fire from any thread (plugin reader
            // thread for event.publish, main actor for tab.opened, etc.).
            // dispatchEvent enters the Rust engine which has its own
            // RwLock — safe to call from any thread. Log only when a
            // trigger actually matches so heartbeat noise doesn't drown
            // the useful signal. `source` plumbs the await-promotion
            // trust stamp through (PR 7); registry-synthesized completion
            // events arrive with source = "copad.action".
            //
            // PR 9 — apply-before-dispatch ordering. Linux's
            // `Pump::pump_all` (`copad-linux/src/window.rs:589`) explicitly
            // drains context first, THEN dispatches triggers, so a
            // `panel.focused` trigger condition that references
            // `{context.active_panel}` resolves to the just-focused panel
            // rather than the previous one. macOS gets the same ordering
            // by applying the event to ContextService BEFORE the engine
            // sees it, then taking the post-apply snapshot to pass
            // through FFI. ContextService is `@unchecked Sendable` with
            // internal NSLock so off-main callers (plugin reader threads)
            // are safe.
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

        // Tier 1.2 — custom keybindings. Install BEFORE menu bar + window
        // so the monitor catches first-keystroke; built-in menu shortcuts
        // still take precedence (menu-driven keyEquivalents fire before
        // local monitors). Hot-reload calls `applyKeybindings` to swap.
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
        // Let theme.background show through the titlebar instead of OS
        // default chrome (black in dark mode, white in light). Force
        // appearance based on background luminance so traffic-light
        // buttons stay readable on dark themes like Catppuccin Mocha.
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
        // "+" popover's plugin-row dispatch — same outcome as the
        // `plugin.open` RPC. Errors stderr-log here (popover is
        // fire-and-forget; surfacing an alert needs UX design we
        // haven't tackled yet).
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

        // Tier 4.2 — status bar modules. Loaded AFTER tabVC is built (it
        // owns the StatusBarView) but BEFORE socket starts so the modules'
        // initial exec doesn't race a coctl command that depends on
        // module state. PluginManifestStore.discover() ran inside the
        // supervisor too — second walk is cheap.
        if let bar = vc.statusBar, let client = daemonClient {
            let manifests = PluginManifestStore.discover()
            bar.loadModules(manifests, daemonClient: client)
        }

        startSocketServer()
        startConfigWatcher()
        // Decision #61 slice 6: give the restore path a factory to reopen a
        // plugin panel by name (restore reopens the plugin's first panel).
        // Lives here because plugin construction needs the manifest store +
        // action registry the pane manager can't reach; MUST be set before the
        // restore below. nil (plugin uninstalled) → explicit placeholder.
        vc.pluginFactory = { [weak self, weak vc] name, restoreID in
            guard let self, let vc,
                  let manifest = PluginManifestStore.discover().first(where: { $0.manifest.plugin.name == name }),
                  let panelDef = manifest.manifest.panels.first
            else { return nil }
            return PluginPanelController(
                plugin: manifest,
                panelDef: panelDef,
                registry: self.actionRegistry,
                eventBus: self.eventBus,
                theme: vc.theme,
                restoreID: restoreID,
            )
        }
        // Session-persistence restore: if a snapshot exists, replay
        // tabs + splits + per-leaf cwd; otherwise seed a single
        // default terminal tab. MUST happen before the daemon starts
        // (next line) so GUI-owned Invokes (tab.list, split.*) see
        // the restored layout, and before any save-on-terminate path
        // can overwrite the persisted snapshot with an empty one.
        // Mirrors `copad-linux/src/window.rs` post-build sequence.
        // Decision #61 slice 3: restore through the v2 model. `loadV2`
        // migrates an existing v1 session.json forward automatically. A file
        // with no sub-tabs (or none at all) falls back to a fresh terminal.
        if let file = Session.loadV2(), !(file.sessions.first?.subTabs.isEmpty ?? true) {
            vc.restoreSessionV2(file)
        } else {
            vc.openInitialTab()
        }

        // Start AFTER the initial tab exists so daemon Invokes for
        // GUI-owned methods (tab.list, terminal.exec, split.*) resolve
        // against a real pane instead of empty/no-op success.
        daemonClient?.start()

        if let path = config.backgroundPath {
            vc.applyBackground(path: path, tint: config.backgroundTint, opacity: config.backgroundOpacity)
        }

        // Native wallpaper rotation: a static `image` is applied above;
        // the first rotation tick then takes over (matches Linux's
        // window.rs apply-at-start + arm sequence). No-op at interval 0.
        liveRotateInterval = config.rotateInterval
        if liveRotateInterval > 0 {
            rotateOnce()
        }
        armRotation()
    }

    // MARK: - Background rotation (Linux parity)

    /// (Re)build the rotation timer from `liveRotateInterval`. 0 stops it.
    /// Also the manual-change hook: `background.set`/`next`/`toggle` call
    /// this so the countdown restarts after a manual pick (Linux's
    /// `BackgroundLayer::arm_rotation`).
    private func armRotation() {
        rotationTimer?.cancel()
        rotationTimer = nil
        guard liveRotateInterval > 0 else { return }
        let timer = DispatchSource.makeTimerSource(queue: .main)
        // `.seconds(Int)` — clamp the UInt into Int range so an absurd
        // config value can't overflow the conversion.
        let secs = Int(min(liveRotateInterval, UInt(Int.max)))
        timer.schedule(deadline: .now() + .seconds(secs), repeating: .seconds(secs))
        timer.setEventHandler { [weak self] in self?.rotateOnce() }
        timer.resume()
        rotationTimer = timer
    }

    /// One rotation tick: honor the shared mode flag, pick a random list
    /// image, apply it as a list pick. No-op when deactive or the list is
    /// missing/empty. Mirrors `BackgroundLayer::rotate_once`.
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
        // Persist tabs + splits + cwd BEFORE engine shutdown so
        // snapshot reads land while panels are still live. An empty
        // snapshot (no terminal tabs left) clears any prior file so
        // a stale layout doesn't surface on next launch — same
        // contract as `copad-linux/src/window.rs`'s close handler.
        // Decision #61 slice 3: persist through the v2 model. The debounced
        // save covers mid-session crashes; this is the orderly-quit flush.
        if let file = tabVC?.snapshotSessionV2() {
            if file.sessions.first?.subTabs.isEmpty ?? true {
                Session.clear()
            } else {
                Session.saveV2(file)
            }
        }
        // Order matters:
        // 1. Engine first — clears the C action callback so no in-flight
        //    plugin event.publish can fire into a stale ActionRegistry.
        // 2. Supervisor — sends `shutdown` to plugins so they stop
        //    publishing further events.
        // 3. Socket — stops accepting new coctl connections.
        // 4. Config watcher — stops file watching.
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

    /// Rec. 601 luminance over [0,1]; threshold 0.5 picks darkAqua for any
    /// reasonably dark background. Used to align titlebar chrome (traffic
    /// lights) with whatever theme.background the user is running.
    private func isDark(_ c: RGBColor) -> Bool {
        let lum = 0.299 * Double(c.r) + 0.587 * Double(c.g) + 0.114 * Double(c.b)
        return lum < 128
    }

    /// Apply `[window] opacity` + `[window] blur` to the main window
    /// (Ghostty model). Idempotent: safe to call on initial launch and
    /// on hot-reload. When opacity = 1.0, removes any installed blur
    /// view and restores opaque chrome.
    ///
    /// Layout: `NSVisualEffectView` becomes the contentView's bottom-most
    /// sibling so it sits behind the TabViewController's root view. The
    /// TabViewController's root is the existing contentView — we wrap
    /// it in a container so the blur can layer beneath without
    /// disturbing AutoLayout of any subview that assumes the controller's
    /// view is the top-level container.
    private func applyWindowTransparency(_ window: NSWindow, config: CopadConfig, theme: CopadTheme) {
        let opacity = CGFloat(config.windowOpacity)
        let wantsTransparent = opacity < 1.0
        let bg = theme.background.nsColor.withAlphaComponent(opacity)

        window.isOpaque = !wantsTransparent
        window.hasShadow = true
        // backgroundColor alpha < 1 only takes effect when isOpaque = false.
        // Setting it unconditionally keeps the chrome color consistent on
        // the opacity = 1.0 path too.
        window.backgroundColor = bg

        installOrRemoveBlurView(window: window, enabled: wantsTransparent && config.windowBlur)
    }

    /// Place an `NSVisualEffectView` as the bottom-most subview of the
    /// window's contentView, sized to fill. Removes it when `enabled =
    /// false`. Tagged with a unique identifier so we don't double-install
    /// or accidentally remove an unrelated view.
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
            // `.hudWindow` reads as dark, dense vibrancy — pairs well
            // with Catppuccin Mocha / dark themes. `.behindWindow`
            // blends the desktop behind the window (Ghostty pattern);
            // `.withinWindow` would only blur sibling windows of the
            // same app, which is not what we want.
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

        shellMenu.addItem(.separator())

        let splitHItem = NSMenuItem(title: "Split Pane Horizontally", action: #selector(splitHorizontal), keyEquivalent: "d")
        splitHItem.target = self
        shellMenu.addItem(splitHItem)

        let splitVItem = NSMenuItem(title: "Split Pane Vertically", action: #selector(splitVertical), keyEquivalent: "D")
        splitVItem.keyEquivalentModifierMask = [.command, .shift]
        splitVItem.target = self
        shellMenu.addItem(splitVItem)

        shellMenu.addItem(.separator())

        // Tier 1.1 — pane focus navigation. Cmd+Shift+] / Cmd+Shift+[
        // cycle DFS-forward / DFS-backward over leaves of the active
        // tab's split tree. No-op on tabs with one pane.
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

        // Edit menu — Cmd+C/V/A route through NSResponder chain (target=nil)
        // to SwiftTerm's @objc open copy:/paste:/selectAll: in MacTerminalView.
        // Without this menu, those keyEquivalents never get dispatched and the
        // first responder never sees them, so clipboard appears dead.
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

    // MARK: - Agent cockpit

    /// Open the agent cockpit as a tab, or focus the existing one. The cockpit
    /// view observes `agentCockpit`; the pump below keeps that model current.
    @objc func toggleCockpit() {
        guard let tabVC else { return }
        if let existing = cockpitPanel, tabVC.activatePanel(id: existing.panelID) { return }
        let cockpit = CockpitViewController(model: agentCockpit, tabVC: tabVC)
        cockpitPanel = cockpit
        _ = tabVC.newPluginPanelTab(cockpit)
    }

    /// The single app-lifetime pump feeding the cockpit model. Reuses the
    /// untyped `subscribe()` + kind-filter pattern from PluginPanelController
    /// (Linux's `subscribe("*")` + filter). Agent kinds mutate the model;
    /// panel.exited evicts; panel.focused acknowledges; every relevant event
    /// then notifies observers so open cockpit views re-enumerate.
    private func startAgentCockpitPump() {
        let channel = eventBus.subscribe()
        let model = agentCockpit
        // macOS publishes `tab.opened` locally (Linux uses `tab.created`); include
        // both + close/rename so the cockpit re-enumerates on pane lifecycle. Title
        // renames post `.terminalTitleChanged` (a NotificationCenter signal, not a
        // bus event) — the cockpit view observes that separately.
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
                // Serial FIFO hop to the main actor so rapid transitions apply in
                // arrival order (unstructured `Task`s do not preserve order).
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

    /// Cmd+F / Cmd+G / Cmd+Shift+G dispatch. All three Find-menu
    /// items route here; the `tag` carries an `NSFindPanelAction`
    /// raw value telling us which one was hit (showFindPanel / next /
    /// previous). Alacritty is the only macOS terminal backend; the
    /// find bar is its own bottom-of-pane control. Non-terminal panes
    /// (webview, plugin) silently no-op.
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

    /// First-party `system.*` actions that should be reachable through
    /// the registry from day one. Plugin host (PR 3) and trigger engine
    /// (PR 5) will register their own handlers via the same registry.
    private func registerBuiltinActions() {
        // system.ffi_test — proxy to CopadFFI.callJSON. Two purposes:
        //   1. Proves the registry seam is reachable from `coctl call`,
        //      end-to-end through SocketServer.dispatch.
        //   2. Gives PR 5 (trigger engine via FFI) a smoke test target it
        //      can dispatch as a registered action — same code path as a
        //      plugin will use.
        // Silent: this is a debug round-trip with no workflow meaning;
        // firing `system.ffi_test.completed` would dirty the bus during
        // FFI smoke testing without enabling any meaningful chain.
        actionRegistry.registerSilent("system.ffi_test") { params, completion in
            // Pass through whatever the caller sent; if absent, send an
            // empty object so the FFI side still gets a valid JSON object.
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

        // system.list_actions — introspection for diagnostics. Returns
        // every name registered through the action registry. Mirrors
        // Linux's debug behavior of being able to query "what actions
        // exist right now". Silent because UIs that poll this on a timer
        // would flood the bus with `.completed` events that never drive
        // a meaningful trigger.
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

        // PR 9 — `context.snapshot` introspection. Returns the live
        // ContextService state ({active_panel, active_cwd}) — same wire
        // shape Linux's `context.snapshot` ships. Silent because some
        // tooling (status bar / agents) might poll this on a timer; firing
        // `context.snapshot.completed` per poll would dwarf real workflow
        // events on the bus.
        actionRegistry.registerSilent("context.snapshot") { [weak self] _, completion in
            guard let self else {
                completion(RPCError(code: "no_app", message: "AppDelegate gone"))
                return
            }
            completion(contextService.snapshot())
        }

        // PR 8 — register `claude.start` through the registry so the
        // trigger engine's C callback can reach it. Codex cross-review
        // CRITICAL: macOS trigger callback dispatches exclusively via
        // `ActionRegistry.tryDispatch` (no fallthrough to the legacy
        // switch-arm). Without this registration the Vision Flow 3
        // chain `git.worktree_add.completed → claude.start` would stall
        // at the second arrow because `tryDispatch` returns false for
        // unregistered actions. Noisy (not silent) so chained triggers
        // can observe `claude.start.completed` for downstream steps if
        // they want to.
        actionRegistry.register("claude.start") { [weak self] params, completion in
            guard let self else {
                completion(RPCError(code: "no_app", message: "AppDelegate gone"))
                return
            }
            ClaudeStart.dispatch(params: params, tabVC: tabVC, completion: completion)
        }

        // panel.report_cwd — Linux/VTE equivalent: shells emit OSC 7 and
        // VTE captures it natively. macOS alacritty backend doesn't
        // surface OSC 7 + proc_pidinfo is EPERM on un-entitled builds,
        // so the in-shell `copad-cwd` hook calls this action on every
        // `chpwd` instead. params: `{"panel_id": "...", "cwd": "..."}`.
        // Silent: shell prompts fire `chpwd` constantly; broadcasting
        // `.completed` per cd would flood the bus.
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
            // Find the panel by id across all tabs / splits and update
            // its tracked cwd. Both terminal backends carry a setter;
            // non-terminal panels (webview, plugin) are silently
            // ignored — the shell hook is shell-only by definition.
            let updated = tabVC.applyReportedCwd(panelID: panelID, cwd: cwd)
            completion(["updated": updated])
        }

        // notify.show — mirror of Linux daemon registration
        // (`copad-linux/src/window.rs:218`) so the macOS GUI's in-process
        // trigger engine reaches the same `osascript` notifier even when
        // the daemon isn't running. Silent: a desktop toast is a side-
        // effect, not a workflow step; chained triggers should not depend
        // on `notify.show.completed`.
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
            // osascript spawn is sync (~10 ms) — offload off the main
            // thread so the toast doesn't stall any concurrent UI work,
            // then bounce the completion back to main where the socket
            // server expects it. LAST_ERROR is thread-local, so it must
            // be read on the same thread that called the FFI.
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
        // Route through `CopadConfig.configPath()` so XDG_CONFIG_HOME
        // overrides also reach the watcher — otherwise the watcher would
        // observe the default ~/.config/ path while Swift load, Rust
        // daemon, coctl, and `copad --config-path` all use the XDG
        // location, and edits there wouldn't trigger live reload.
        let configURL = CopadConfig.configPath()
        let watcher = ConfigWatcher(url: configURL)
        watcher.onChange = { [weak self] in self?.handleConfigChange() }
        watcher.start()
        configWatcher = watcher
    }

    private func handleConfigChange() {
        // Match Linux's reload semantics (`window.rs::connect_changed`):
        // on parse failure, log and return early — the live config keeps
        // rendering instead of getting reset to defaults on a typo.
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
        // Re-arm rotation with the (possibly changed) interval — arming at
        // 0 stops the timer. A rotated wallpaper survives the reload via
        // TabViewController.applyConfig's list-image guard.
        liveRotateInterval = newConfig.rotateInterval
        armRotation()
        // Skip local trigger reload while daemon owns triggers. Daemon
        // runs its own config watcher and would race our setTriggers.
        // Local engine retains its previous trigger set, ready for cut-over
        // restore on daemon disconnect.
        if daemonClient?.hostTriggersActive == true {
            FileHandle.standardError.write(Data("[copad-engine] skipping local trigger reload — daemon owns triggers (host_triggers=true)\n".utf8))
        } else {
            let triggerJSON = CopadConfig.triggersJSON(from: newConfig)
            if let count = copadEngine.setTriggers(triggerJSON) {
                FileHandle.standardError.write(Data("[copad-engine] reloaded \(count) trigger(s) on config.toml change\n".utf8))
            }
        }
        // Reload keybindings — hot-swap into the existing monitor's snapshot.
        applyKeybindings(newConfig.keybindings)
        eventBus.broadcast(event: "config.reloaded", data: ["theme": newTheme.name])
    }

    // MARK: - Keybindings (Tier 1.2)

    private func applyKeybindings(_ raw: [String: String]) {
        keybindings = Keybindings.compile(raw)
        if !keybindings.isEmpty {
            FileHandle.standardError.write(Data("[copad] loaded \(keybindings.count) custom keybinding(s)\n".utf8))
        }
    }

    private func installKeybindingMonitor() {
        // .keyDown so we get repeats too; the local monitor returns the
        // event when no binding matches, so the standard responder chain
        // (menu shortcuts, terminal input) sees it normally. Returning nil
        // swallows the event — only on a positive match.
        keybindingMonitor = NSEvent.addLocalMonitorForEvents(matching: .keyDown) { [weak self] event in
            guard let self else { return event }
            for binding in keybindings where Keybindings.matches(event, binding) {
                Keybindings.dispatch(binding, registry: actionRegistry, socketPath: socketServer.path)
                return nil
            }
            // Decision #61 UI-B: opt+1…9 / opt+0 jump sub-tabs (tmux-window
            // style). Handled here, before the terminal sees the Option key,
            // and only when that sub-tab exists (else it falls through as a
            // normal Option keystroke). User `[keybindings]` above win.
            if let idx = optionDigitSubTabIndex(event), let vc = tabVC, idx < vc.tabCount {
                vc.switchTab(to: idx)
                return nil
            }
            if matchesCommandPaletteShortcut(event), openCommandPalette() {
                return nil
            }
            return event
        }
    }

    /// Map a bare-Option digit event to a sub-tab index: opt+1…9 → 0…8,
    /// opt+0 → 9 (the tenth). Returns nil unless Option is the ONLY modifier
    /// (cmd/ctrl/shift-digit are left to menus / the responder chain).
    private func optionDigitSubTabIndex(_ event: NSEvent) -> Int? {
        let interesting: NSEvent.ModifierFlags = [.command, .control, .shift, .option]
        guard event.modifierFlags.intersection(interesting) == [.option] else { return nil }
        guard
            let ch = event.charactersIgnoringModifiers,
            ch.count == 1,
            let digit = ch.first,
            let n = digit.wholeNumberValue,
            digit.isNumber
        else { return nil }
        return n == 0 ? 9 : n - 1
    }

    /// Cmd+Shift+P, mac convention (VSCode/Zed). User keybindings already
    /// run before this — a `cmd+shift+p` entry in `[keybindings]` shadows
    /// the palette, mirroring Linux's "user binding wins" precedence.
    /// keyCode source `Keybindings.nameToKeyCode` is shared so a future
    /// rename / re-mapping flows through both the user-binding parser and
    /// this built-in shortcut without divergence.
    private func matchesCommandPaletteShortcut(_ event: NSEvent) -> Bool {
        let interesting: NSEvent.ModifierFlags = [.command, .control, .shift, .option]
        let mods = event.modifierFlags.intersection(interesting)
        guard mods == [.command, .shift] else { return false }
        guard let pCode = Keybindings.nameToKeyCode["p"] else { return false }
        return event.keyCode == pCode
    }

    /// Returns true when the palette opened (event must be swallowed).
    /// Repeats and re-entry while a sheet is already attached return false
    /// so the held key doesn't stack modal state. `attachedSheet` covers
    /// the destructive-confirm alert window: when palette closes and the
    /// `tab.close` alert opens, `commandPaletteController` is already nil
    /// but the window still has the alert sheet attached — a second
    /// `beginSheet` would break the modal invariant.
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
                    // Legacy switch arms `completion(nil)` for missing
                    // params (e.g. `terminal.exec`, `tab.rename`); surface
                    // those as a usable hint instead of silent no-op so
                    // palette users learn the action needs params.
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

    /// Linux-parity terminal panel resolver. Mirrors
    /// `resolve_terminal` in copad-linux/src/socket.rs:1213.
    ///
    /// Priority:
    ///   1. If `params["id"]` is given → look up that exact panel; emit
    ///      `not_found` if no panel matches, `wrong_panel_type` if the
    ///      panel exists but isn't a terminal (e.g. webview / plugin).
    ///   2. Otherwise, prefer the active pane if it's a terminal.
    ///   3. Otherwise, fall back to the first terminal anywhere across
    ///      tabs (`firstTerminalPanel`).
    ///   4. If none of the above produces a terminal → `no_terminal`.
    ///
    /// Error codes intentionally match Linux verbatim so coctl / agent
    /// error handling can be backend-agnostic.
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

    /// Open a plugin-provided panel. Shared by the `plugin.open` RPC and
    /// the `+` popover's plugin-panel rows so both surfaces reach the
    /// same outcome with one source of truth.
    ///
    /// `mode` matches the RPC contract: "tab" (default) / "split_h" /
    /// "split_v". An unknown value falls through to "tab" — same forgiving
    /// shape as the inline implementation it replaces.
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

    /// `allowFallback: false` makes the default arm return local
    /// `unknown_method` instead of forwarding to daemon — required for
    /// daemon-originated Invokes so they don't recurse back through the
    /// fallback. `silentCompletion: true` suppresses local
    /// `<method>.completed` fan-out for daemon-routed invokes (daemon
    /// publishes that itself).
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

        case "tab.new":
            vc.newTab()
            completion(["ok": true])

        case "tab.close":
            vc.closeActivePane()
            completion(["ok": true])

        case "tab.switch":
            guard let index = params["index"] as? Int else { completion(nil); return }
            vc.switchTab(to: index)
            completion(["ok": true])

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

        // Tier 1.1 — pane focus navigation, also exposed over socket so
        // coctl + triggers can drive it (not just menu Cmd+Shift+]).
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
                // NSApplication.ModalResponse.alertFirstButtonReturn = 1000
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
            // A manual pick restarts the countdown so the timer doesn't
            // replace it a moment later (Linux handle_bg_set parity).
            armRotation()
            completion(["ok": true])

        case "background.set_tint":
            guard let tint = params["tint"] as? Double else { completion(nil); return }
            vc.setTint(tint)
            completion(["ok": true])

        case "background.clear":
            vc.clearBackground()
            completion(["ok": true])

        // Tier 1.3 — random wallpaper rotation. Same wire shape as Linux:
        // both commands return `{status, mode}` so trigger configs can
        // detect deactive state without parsing free-form messages.
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
            // Reuse the existing tint/opacity from the live state so a rotation
            // doesn't bake the defaults if the user customized them.
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

        // Delete the displayed list-picked wallpaper from disk AND the list
        // file(s), then rotate. Only operates on rotation / `next` / `toggle`
        // picks — a manually `set` image or `[background] image` is never
        // deleted (Linux handle_bg_delete_current parity).
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
                // NotFound — already gone, treat as success and continue to
                // list removal + rotation.
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
            switch mode {
            case "split_h":
                vc.splitActivePaneWithWebView(url: url, orientation: .horizontal)
            case "split_v":
                vc.splitActivePaneWithWebView(url: url, orientation: .vertical)
            default: // "tab"
                vc.newWebViewTab(url: url)
            }
            completion(["ok": true])

        case "webview.navigate":
            guard let urlString = params["url"] as? String else {
                completion(RPCError(code: "invalid_params", message: "Missing 'url' param"))
                return
            }
            switch resolveWebView(params, in: vc) {
            case let .failure(err): completion(err)
            case let .success(webVC):
                webVC.navigate(to: urlString)
                completion(["status": "ok"])
            }

        case "webview.back":
            switch resolveWebView(params, in: vc) {
            case let .failure(err): completion(err)
            case let .success(webVC):
                webVC.goBack()
                completion(["status": "ok"])
            }

        case "webview.forward":
            switch resolveWebView(params, in: vc) {
            case let .failure(err): completion(err)
            case let .success(webVC):
                webVC.goForward()
                completion(["status": "ok"])
            }

        case "webview.reload":
            switch resolveWebView(params, in: vc) {
            case let .failure(err): completion(err)
            case let .success(webVC):
                webVC.reload()
                completion(["status": "ok"])
            }

        case "webview.execute_js":
            // Param name is `code` (Linux + copad-cli convention). Older callers that
            // sent `script` get a fallback so existing macOS-only consumers don't break.
            guard let code = (params["code"] as? String) ?? (params["script"] as? String) else {
                completion(RPCError(code: "invalid_params", message: "Missing 'code' param"))
                return
            }
            switch resolveWebView(params, in: vc) {
            case let .failure(err): completion(err)
            case let .success(webVC):
                webVC.executeJS(code) { result, error in
                    if let error {
                        completion(RPCError(code: "js_error", message: error.localizedDescription))
                    } else {
                        completion(["result": result ?? NSNull()])
                    }
                }
            }

        case "webview.get_content":
            switch resolveWebView(params, in: vc) {
            case let .failure(err): completion(err)
            case let .success(webVC):
                webVC.getContent { html in
                    completion(["html": html])
                }
            }

        case "webview.devtools":
            // Linux accepts `action: show/close/attach/detach`. macOS WKWebView
            // exposes no public API to programmatically open the inspector
            // window — `developerExtrasEnabled` only enables the right-click
            // → "Inspect Element" menu. We accept the action verb for protocol
            // parity but treat show/attach/detach as "ensure enabled" and
            // close as "no-op" (the user closes the inspector window manually).
            let action = (params["action"] as? String) ?? "show"
            switch resolveWebView(params, in: vc) {
            case let .failure(err): completion(err)
            case let .success(webVC):
                switch action {
                case "show", "attach", "detach", "toggle":
                    webVC.toggleDevTools()
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
            switch resolveWebView(params, in: vc) {
            case let .failure(err): completion(err)
            case let .success(webVC):
                completion([
                    "url": webVC.currentURL,
                    "title": webVC.currentTitle,
                    "can_go_back": webVC.canGoBack,
                    "can_go_forward": webVC.canGoForward,
                    "is_loading": webVC.isLoading,
                ])
            }

        // Tier 4.3 — webview interaction. Each command builds the JS
        // snippet (mirroring copad-linux/src/webview.rs::js) and runs it
        // via the existing executeJS bridge. The JS returns a JSON string;
        // we parse it back into `Any` so the wire format stays homogenous
        // with Linux. Selector resolution is the same id/active fallback
        // as the navigation commands.
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
            // selector optional; if absent, scroll viewport to (x, y).
            let selector = params["selector"] as? String
            let x = (params["x"] as? Int) ?? 0
            let y = (params["y"] as? Int) ?? 0
            runWebViewJS(WebViewJS.scroll(selector: selector, x: x, y: y), params: params, in: vc, completion: completion)

        case "webview.page_info":
            runWebViewJS(WebViewJS.pageInfo(), params: params, in: vc, completion: completion)

        case "webview.screenshot":
            switch resolveWebView(params, in: vc) {
            case let .failure(err): completion(err)
            case let .success(webVC):
                let config = WKSnapshotConfiguration()
                // Default rect = visible area at full resolution. Linux's
                // SnapshotRegion::Visible matches this, modulo platform pixel
                // density differences.
                webVC.webView.takeSnapshot(with: config) { image, error in
                    if let error {
                        completion(RPCError(code: "snapshot_failed", message: error.localizedDescription))
                        return
                    }
                    guard let image,
                          let tiff = image.tiffRepresentation,
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
            // params: name (plugin name), panel (default "main"), mode
            // (default "tab", also supports "split_h"/"split_v"). Mirrors
            // the shape of `webview.open` so triggers + coctl scripts
            // can use the same param vocabulary across panel types.
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

        // Tier 4.2 — status bar visibility toggles. Match Linux's
        // `{visible: bool}` response shape.
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

    /// Helper: resolve the target webview, evaluate the JS snippet, parse
    /// the JSON-string result, and pass the parsed value to completion.
    /// Linux's `run_js_command` does the same shape; this is its mirror.
    private func runWebViewJS(
        _ js: String,
        params: [String: Any],
        in vc: TabViewController,
        completion: @escaping (Any?) -> Void,
    ) {
        switch resolveWebView(params, in: vc) {
        case let .failure(err):
            completion(err)
        case let .success(webVC):
            webVC.executeJS(js) { result, error in
                if let error {
                    completion(RPCError(code: "js_error", message: error.localizedDescription))
                    return
                }
                // The JS snippets always JSON.stringify their result, so
                // the WKWebView completion gives us a String here. Decode
                // back into [String: Any] / [Any] / scalar.
                guard let str = result as? String else {
                    // Fallback: hand the raw value back — covers JS that
                    // accidentally returns a non-string (the Linux side
                    // does the same passthrough).
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

    /// Resolves the target WebViewController for an `id`-aware webview command.
    ///
    /// - If `params["id"]` is a non-empty string, look it up across all tabs and
    ///   return the panel (errors out on `not_found` / `wrong_panel_type`,
    ///   matching Linux `socket.rs` codes).
    /// - If `id` is absent, fall back to the active webview. Linux's handlers
    ///   require `id`; macOS keeps the lenient default per the parity plan
    ///   (Tier 1.6) so existing coctl-without-id calls keep working.
    private func resolveWebView(
        _ params: [String: Any],
        in vc: TabViewController,
    ) -> Result<WebViewController, RPCError> {
        if let id = params["id"] as? String, !id.isEmpty {
            guard let panel = vc.panel(id: id) else {
                return .failure(RPCError(code: "not_found", message: "Panel not found: \(id)"))
            }
            guard let webVC = panel as? WebViewController else {
                return .failure(RPCError(code: "wrong_panel_type", message: "Panel is not a webview"))
            }
            return .success(webVC)
        }
        guard let webVC = vc.activeWebView else {
            return .failure(RPCError(
                code: "no_active_webview",
                message: "No active webview and no 'id' provided",
            ))
        }
        return .success(webVC)
    }
}
