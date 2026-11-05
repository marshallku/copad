import AppKit
import Foundation
@preconcurrency import WebKit

/// Host-rendered plugin panel mirroring `copad-linux/src/plugin_panel.rs`: loads the plugin's
/// `panel.html` in a WKWebView and injects `window.copad` at document start.
///
/// The JS contract must match Linux byte-for-byte so one `panel.html` serves both platforms:
///
/// ```js
/// window.copad = {
///     panel: { id, name, plugin },
///     async call(method, params = {}) -> result, // throws Error{code, message}
///     on(type, callback),
///     off(type, callback),
///     // _handleEvent(type, data) — internal, called by host
/// }
/// ```
///
/// `call` goes through `WKScriptMessageHandlerWithReply` into `ActionRegistry.tryDispatch`, so
/// plugin actions and the `coctl call` surface are available. EventBus events are pushed back via
/// `copad._handleEvent(type, data)` on `evaluateJavaScript`.
@MainActor
final class PluginPanelController: NSViewController, CopadPanel {
    let panelID: String
    private(set) var currentTitle: String

    /// Persisted in the session snapshot (decision #61) so restore reopens the plugin by name.
    let pluginName: String
    private let panelName: String
    /// The panel of a multi-panel plugin, persisted so restore reopens the same one.
    var currentPanelName: String { panelName }
    private let panelFileURL: URL
    private let theme: CopadTheme
    private weak var registry: ActionRegistry?
    private weak var eventBus: EventBus?

    private var webView: WKWebView!
    private var started = false
    private var eventChannel: EventChannel?
    /// WKWebView's content controller holds the handler strongly; it gets a weak proxy.
    private var bridgeHandler: BridgeHandler?

    init(
        plugin: LoadedPluginManifest,
        panelDef: PluginPanelDef,
        registry: ActionRegistry,
        eventBus: EventBus,
        theme: CopadTheme,
        restoreID: String? = nil,
    ) {
        panelID = restoreID ?? UUID().uuidString
        pluginName = plugin.manifest.plugin.name
        panelName = panelDef.name
        currentTitle = panelDef.title
        panelFileURL = plugin.dir.appendingPathComponent(panelDef.file)
        self.registry = registry
        self.eventBus = eventBus
        self.theme = theme
        super.init(nibName: nil, bundle: nil)
    }

    @available(*, unavailable)
    required init?(coder _: NSCoder) {
        fatalError()
    }

    override func loadView() {
        let config = WKWebViewConfiguration()
        config.preferences.setValue(true, forKey: "developerExtrasEnabled")

        let handler = BridgeHandler(controller: self)
        bridgeHandler = handler
        config.userContentController.addScriptMessageHandler(
            handler,
            contentWorld: .page,
            name: "copad",
        )

        // Injected at document start so panel scripts can use window.copad immediately. Keep in
        // sync with Linux's build_bridge_js.
        let bridgeJS = Self.buildBridgeJS(
            pluginName: pluginName,
            panelName: panelName,
            panelID: panelID,
        )
        let userScript = WKUserScript(
            source: bridgeJS,
            injectionTime: .atDocumentStart,
            forMainFrameOnly: false,
        )
        config.userContentController.addUserScript(userScript)

        // Without the theme CSS, `var(--copad-bg)` resolves to nothing and WKWebView falls back to
        // its default font.
        let themeCSS = Self.buildThemeCSS(theme: theme)
        let themeJS = Self.buildStyleInjectionJS(css: themeCSS)
        let themeScript = WKUserScript(
            source: themeJS,
            injectionTime: .atDocumentStart,
            forMainFrameOnly: false,
        )
        config.userContentController.addUserScript(themeScript)

        let wv = WKWebView(frame: .zero, configuration: config)
        wv.translatesAutoresizingMaskIntoConstraints = false
        // Transparent so the window background image shows through; plugins wanting an opaque card
        // style set a background on inner elements.
        wv.setValue(false, forKey: "drawsBackground")
        webView = wv
        view = wv
    }

    func startIfNeeded() {
        guard !started else { return }
        started = true

        // loadFileURL grants read access to the file only; allow the plugin dir so sibling assets
        // resolve.
        webView.loadFileURL(
            panelFileURL,
            allowingReadAccessTo: panelFileURL.deletingLastPathComponent(),
        )

        startEventForwarding()
    }

    // MARK: - CopadPanel no-ops

    func applyBackground(path _: String, tint _: Double, opacity _: Double) {}
    func clearBackground() {}
    func setTint(_: Double) {}

    // MARK: - Event forwarding

    private func startEventForwarding() {
        guard let bus = eventBus else { return }
        let channel = bus.subscribe()
        eventChannel = channel
        // EventChannel.receive() blocks, so run it on a background thread and hop to main.
        Thread.detachNewThread { [weak self] in
            while let json = channel.receive() {
                // EventBus serializes `{"event": kind, "data": {...}}`; the plugin contract takes
                // (type, data) separately.
                guard let data = json.data(using: .utf8),
                      let obj = try? JSONSerialization.jsonObject(with: data) as? [String: Any],
                      let type = obj["event"] as? String
                else { continue }
                let payload = obj["data"] ?? NSNull()
                let typeJSON = Self.jsonString(type) ?? "\"\""
                let dataJSON = (try? String(
                    data: JSONSerialization.data(withJSONObject: payload, options: [.fragmentsAllowed]),
                    encoding: .utf8,
                )) ?? "null"
                let js = "if (window.copad && window.copad._handleEvent) copad._handleEvent(\(typeJSON), \(dataJSON))"
                let jsBox = SendableBox(js)
                DispatchQueue.main.async { [weak self] in
                    self?.webView.evaluateJavaScript(jsBox.value, completionHandler: nil)
                }
            }
        }
    }

    // MARK: - Bridge dispatch (called from BridgeHandler on main)

    fileprivate func handleBridgeMessage(_ message: [String: Any], reply: @escaping (String) -> Void) {
        guard let method = message["method"] as? String else {
            reply(Self.errorJSON(code: "parse_error", message: "missing 'method' field"))
            return
        }
        let params = (message["params"] as? [String: Any]) ?? [:]
        guard let registry else {
            reply(Self.errorJSON(code: "internal_error", message: "ActionRegistry gone"))
            return
        }
        registry.tryDispatchOrFallback(method, params: params) { result in
            if let err = result as? RPCError {
                reply(Self.errorJSON(code: err.code, message: err.message))
            } else if let result {
                reply(Self.successJSON(result: result))
            } else {
                reply(Self.successJSON(result: NSNull()))
            }
        }
    }

    // MARK: - Static helpers

    private static func buildThemeCSS(theme: CopadTheme) -> String {
        let bg = hex(theme.background)
        let text = hex(theme.text)
        return """
        :root {
            --copad-bg: \(bg);
            --copad-fg: \(text);
            --copad-surface0: \(hex(theme.surface0));
            --copad-surface1: \(hex(theme.surface1));
            --copad-surface2: \(hex(theme.surface2));
            --copad-overlay0: \(hex(theme.overlay0));
            --copad-text: \(text);
            --copad-subtext0: \(hex(theme.subtext0));
            --copad-subtext1: \(hex(theme.subtext1));
            --copad-accent: \(hex(theme.accent));
            --copad-red: \(hex(theme.red));
        }
        html, body {
            background-color: transparent;
            color: \(text);
            font-family: system-ui, -apple-system, sans-serif;
            margin: 0;
            padding: 0;
        }
        """
    }

    private static func buildStyleInjectionJS(css: String) -> String {
        // document.head may not exist yet at documentStart, so fall back to documentElement.
        let cssJSON = jsonString(css) ?? "\"\""
        return """
        (() => {
            const style = document.createElement('style');
            style.setAttribute('data-copad-theme', '1');
            style.textContent = \(cssJSON);
            (document.head || document.documentElement).appendChild(style);
        })()
        """
    }

    private nonisolated static func hex(_ c: RGBColor) -> String {
        String(format: "#%02x%02x%02x", c.r, c.g, c.b)
    }

    private static func buildBridgeJS(pluginName: String, panelName: String, panelID: String) -> String {
        let id = jsonString(panelID) ?? "\"\""
        let name = jsonString(panelName) ?? "\"\""
        let plugin = jsonString(pluginName) ?? "\"\""
        return """
        (() => {
            const _listeners = {};
            window.copad = {
                panel: {
                    id: \(id),
                    name: \(name),
                    plugin: \(plugin),
                },
                async call(method, params = {}) {
                    const resp = await window.webkit.messageHandlers.copad.postMessage(
                        JSON.stringify({ method, params })
                    );
                    const parsed = JSON.parse(resp);
                    if (!parsed.ok) {
                        const err = new Error(parsed.error?.message || "Unknown error");
                        err.code = parsed.error?.code;
                        throw err;
                    }
                    return parsed.result;
                },
                on(type, callback) {
                    if (!_listeners[type]) _listeners[type] = [];
                    _listeners[type].push(callback);
                },
                off(type, callback) {
                    if (!_listeners[type]) return;
                    _listeners[type] = _listeners[type].filter(cb => cb !== callback);
                },
                _handleEvent(type, data) {
                    const cbs = _listeners[type] || [];
                    for (const cb of cbs) {
                        try { cb(data); } catch (e) { console.error("copad event handler error:", e); }
                    }
                    const wildcards = _listeners["*"] || [];
                    for (const cb of wildcards) {
                        try { cb(type, data); } catch (e) { console.error("copad event handler error:", e); }
                    }
                },
            };
        })()
        """
    }

    private nonisolated static func jsonString(_ s: String) -> String? {
        guard let data = try? JSONSerialization.data(withJSONObject: s, options: [.fragmentsAllowed]),
              let str = String(data: data, encoding: .utf8)
        else { return nil }
        return str
    }

    private static func successJSON(result: Any) -> String {
        let dict: [String: Any] = ["ok": true, "result": result]
        guard let data = try? JSONSerialization.data(withJSONObject: dict, options: [.fragmentsAllowed]),
              let str = String(data: data, encoding: .utf8)
        else { return #"{"ok":true,"result":null}"# }
        return str
    }

    private static func errorJSON(code: String, message: String) -> String {
        let dict: [String: Any] = [
            "ok": false,
            "error": ["code": code, "message": message],
        ]
        guard let data = try? JSONSerialization.data(withJSONObject: dict),
              let str = String(data: data, encoding: .utf8)
        else { return #"{"ok":false,"error":{"code":"internal_error","message":"json encode failed"}}"# }
        return str
    }
}

// MARK: - Bridge handler

/// Bridges `window.webkit.messageHandlers.copad.postMessage(json)` into
/// `PluginPanelController.handleBridgeMessage`. Held weakly so the WKWebView's strong reference
/// does not keep the controller alive past panel close. `@unchecked Sendable` because the
/// controller reference is weak and only read on main.
private final class BridgeHandler: NSObject, WKScriptMessageHandlerWithReply, @unchecked Sendable {
    weak var controller: PluginPanelController?

    init(controller: PluginPanelController) {
        self.controller = controller
        super.init()
    }

    @MainActor
    func userContentController(
        _: WKUserContentController,
        didReceive message: WKScriptMessage,
        replyHandler: @escaping @MainActor @Sendable (Any?, String?) -> Void,
    ) {
        guard let str = message.body as? String,
              let data = str.data(using: .utf8),
              let dict = (try? JSONSerialization.jsonObject(with: data)) as? [String: Any]
        else {
            replyHandler(nil, "bridge message was not JSON-encoded {method, params}")
            return
        }
        guard let controller else {
            replyHandler(nil, "PluginPanelController gone")
            return
        }
        // handleBridgeMessage's completion may fire on any thread; reply on main, as WKWebView
        // wants. SendableBox carries the closure across the @Sendable hop.
        let replyBox = SendableBox(replyHandler)
        controller.handleBridgeMessage(dict) { replyJson in
            DispatchQueue.main.async {
                replyBox.value(replyJson, nil)
            }
        }
    }
}
