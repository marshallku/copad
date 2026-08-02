import Foundation
import TOMLKit

/// Policy for OSC 52 clipboard writes from the PTY.
///
/// Background: SwiftTerm's `LocalProcessTerminalView` writes to `NSPasteboard.general`
/// unconditionally on OSC 52. That lets any program in the terminal silently overwrite
/// the user's clipboard. We intercept by replacing `terminalDelegate` with a proxy
/// that consults this policy. Default is `deny`; matches VTE's hardened default on
/// Linux (VTE has OSC 52 disabled unless explicitly opted in).
enum OSC52Policy: String, Decodable {
    case deny
    case allow
}

/// Where the tab bar sits relative to the content area. Linux supports
/// `top`/`bottom`/`left`/`right`; macOS now implements all four — the
/// vertical orientations (`left`/`right`) run the bar down one side with
/// a fixed width (`[tabs] width`), the horizontal ones across top/bottom.
enum TabsPosition: String, Decodable {
    case top
    case bottom
    case left
    case right

    /// True for the vertical orientations (`left`/`right`), where the tab
    /// bar runs down one side of the window with a fixed width instead of
    /// across the top/bottom with a fixed height.
    var isVertical: Bool { self == .left || self == .right }

    /// Decode permissively: an unrecognized value falls back to `.top`
    /// with a stderr warning rather than crashing.
    static func parse(_ raw: String) -> TabsPosition {
        if let p = TabsPosition(rawValue: raw.lowercased()) { return p }
        let msg = "[copad] [tabs] position '\(raw)' unrecognized — using 'top'\n"
        FileHandle.standardError.write(Data(msg.utf8))
        return .top
    }
}

/// `[statusbar]` config: enable + position + height. Same shape as Linux.
/// Position support is limited to `bottom` on macOS today (top deferred —
/// requires layout reshuffle around tab bar position).
struct StatusBarConfig {
    let enabled: Bool
    let position: String
    let height: Int

    static let defaults = StatusBarConfig(enabled: true, position: "bottom", height: 28)
}

struct CopadConfig {
    let shell: String
    let fontFamily: String
    let fontSize: Int
    /// macOS-only: when true, Option+key sends `ESC + key` to the PTY
    /// instead of routing through the system IME (which produces
    /// `¡™£¢`-style special chars on Option+1/2/3/4). Required for
    /// tmux/zsh/readline Meta bindings to fire. Default `true` because
    /// this is a dev terminal — users who need Option for diacritics
    /// can set `option_as_alt = false`.
    let optionAsAlt: Bool
    /// macOS-only: control-character keys that should bypass the
    /// `optionAsAlt` printable-only filter and send `ESC + <byte>` when
    /// pressed with Option (independent of `option_as_alt`). Default
    /// `["Return"]` so Opt+Return delivers `ESC + CR` — the sequence
    /// Claude Code, Python REPL, ipython, etc. accept as newline-in-
    /// prompt. Recognized names (case-insensitive): `Return`/`Enter`,
    /// `Escape`/`Esc`. Unknown names are ignored with a stderr warning.
    /// Arrows / Delete are intentionally NOT supported — they rely on
    /// Cocoa's `moveWordLeft:` / `deleteWordBackward:` key bindings
    /// translating Opt+← / Opt+⌫ to readline byte sequences, and a
    /// force-Meta override would steal those keystrokes.
    let forceMetaKeys: [String]
    /// `[terminal] close_on_exit` — close the pane when the PTY child
    /// (shell) exits. Cascades: last pane in tab → close tab; last
    /// tab in window → close window. `false` keeps the dead viewport
    /// so the user can read the exit message. Default `true` matches
    /// Linux's long-standing behavior.
    let closeOnExit: Bool
    let themeName: String
    let backgroundPath: String?
    let backgroundTint: Double
    /// Opacity of the background image layer itself (0.0 = invisible, 1.0 = fully visible).
    /// Distinct from `backgroundTint`, which darkens the image via an overlay.
    let backgroundOpacity: Double
    /// `[background] rotate_interval` — seconds between random wallpapers
    /// from the platform list file. 0 (default) disables the in-process
    /// rotation timer; `coctl background next` keeps working either way.
    /// Mirrors `copad_core::config::BackgroundConfig::rotate_interval`.
    let rotateInterval: UInt
    let osc52: OSC52Policy
    /// `[renderer] transparent_default_bg = true` makes default-bg cells
    /// transparent so a configured background image shows through blank
    /// cells. Off by default — cursor visibility against image
    /// backgrounds wins over aesthetic transparency. Cells with explicit
    /// ANSI bg colors and reverse-video cells still materialize opaquely
    /// (Zed pattern).
    let transparentDefaultBg: Bool
    /// `[renderer] gpu` — Metal render path. **Default on** since the
    /// slice-3 flip (docs/macos-gpu-renderer-plan.md): measured ~5.5×
    /// cheaper main-thread render than the CoreText painter, split
    /// coexistence + IME/selection/copy verified. Set `gpu = false` to
    /// opt back into the CoreText painter (kept as the fallback —
    /// 10a/10b pattern). Read at pane creation: existing panes keep
    /// their painter across config hot-reloads (the layer class is
    /// committed at view init), new panes pick up the new value. Falls
    /// back to CoreText automatically when no Metal device is available.
    let rendererGPU: Bool
    /// `[window] opacity` (0.0 = fully transparent, 1.0 = fully opaque,
    /// default 1.0). Controls the window itself + terminal default-bg
    /// cells (Ghostty model). Distinct from `backgroundOpacity` which
    /// only affects the optional background-image layer.
    let windowOpacity: Double
    /// `[window] blur = true`. macOS-only — when `windowOpacity < 1.0`
    /// the AppDelegate installs an `NSVisualEffectView` behind the
    /// content view so the desktop is blurred (Ghostty
    /// `background-blur-radius`). No effect when opacity = 1.0.
    let windowBlur: Bool
    /// Tier 1.4 — `[tabs] position` (top/bottom/left/right).
    let tabsPosition: TabsPosition
    /// `[tabs] width` — width in points of the vertical (`left`/`right`)
    /// tab bar. Ignored for `top`/`bottom`. Mirrors the Linux `width = 200`.
    let tabsWidth: Int
    /// Tier 4.2 — `[statusbar]` config (enabled/position/height). Modules
    /// themselves come from plugin manifests' `[[modules]]` declarations.
    let statusBar: StatusBarConfig
    /// Tier 1.2 — `[keybindings]` flat dict: combo string → command string.
    /// Compiled to `Keybindings.Binding` at AppDelegate init time and
    /// matched in the NSEvent local monitor. Empty when no `[keybindings]`
    /// section is present.
    let keybindings: [String: String]
    /// PR 5c — raw `[[triggers]]` array from config.toml, walked from the
    /// TOMLKit table tree into JSON-friendly `[[String: Any]]` so it can be
    /// JSON-encoded and shipped to the Rust trigger engine via FFI. We don't
    /// type each trigger statically because the schema allows arbitrary
    /// nested values under `params` / `when.payload_match` / `await.payload_match`,
    /// and the Rust side already has the canonical Deserialize impl.
    let triggers: [[String: Any]]

    /// `$XDG_CONFIG_HOME/copad/config.toml`, else `~/.config/copad/
    /// config.toml`. Mirrors `copad_core::config::CopadConfig::
    /// config_path()` so Swift renderer, Rust daemon, and coctl all
    /// agree on the canonical location.
    static func configPath() -> URL {
        let env = ProcessInfo.processInfo.environment["XDG_CONFIG_HOME"]
        let base: URL = if let env, !env.isEmpty {
            URL(fileURLWithPath: env)
        } else {
            FileManager.default.homeDirectoryForCurrentUser
                .appendingPathComponent(".config")
        }
        return base
            .appendingPathComponent("copad")
            .appendingPathComponent("config.toml")
    }

    /// Read + parse `~/.config/copad/config.toml`. Returns `.defaults`
    /// when the file is absent (first run) — matches the Linux
    /// `CopadConfig::load_from` contract. Throws on parse failure so
    /// the caller can decide whether to start with defaults (initial
    /// launch) or preserve the previously rendered live config (hot
    /// reload).
    static func load() throws -> CopadConfig {
        let configURL = configPath()
        guard let contents = try? String(contentsOf: configURL, encoding: .utf8) else {
            return .defaults
        }
        return try parse(contents)
    }

    /// Decode a TOML config string into CopadConfig. Throws only on a genuine
    /// TOML *syntax* error (an unparseable file); the error is written to stderr
    /// so the user sees the line/column. Unknown sections (e.g. `[[triggers]]`,
    /// `[keybindings]` from the Linux schema) are tolerated — we only read the
    /// fields the macOS app currently uses, and the rest stay intact for future
    /// parity work.
    ///
    /// Fields are read directly off the raw `TOMLTable` rather than strict-decoded
    /// into a Codable struct. This is deliberate, and both reasons were learned
    /// the hard way:
    ///   1. **Type coercion.** TOMLKit's strict decoder rejects an integer literal
    ///      for a `Double` field (`tint = 0` instead of `0.0`) and throws. That
    ///      throw USED to discard the ENTIRE config and silently fall back to
    ///      defaults — whose font is a non-Nerd family, so every glyph icon
    ///      rendered as tofu. Linux/serde coerces int→float, so a shared dotfiles
    ///      config that worked on Linux broke only on macOS. `tomlDouble`/`tomlInt`
    ///      coerce across TOML's number types the same way serde does.
    ///   2. **Graceful degradation.** Strict decoding is all-or-nothing: one bad
    ///      field nuked every other setting. Per-field reads fall back to that
    ///      field's own default and preserve the rest of the config.
    /// If you must diagnose a config, `comux doctor` lints these exact issues.
    static func parse(_ contents: String) throws -> CopadConfig {
        let table: TOMLTable
        do {
            table = try TOMLTable(string: contents)
        } catch {
            let msg = "[copad] config.toml parse failed (syntax error): "
                + "\(error.localizedDescription)\n"
            FileHandle.standardError.write(Data(msg.utf8))
            throw error
        }

        let d = CopadConfig.defaults
        let terminal = table["terminal"]?.table
        let theme = table["theme"]?.table
        let background = table["background"]?.table
        let security = table["security"]?.table
        let renderer = table["renderer"]?.table
        let window = table["window"]?.table
        let tabs = table["tabs"]?.table
        let statusbar = table["statusbar"]?.table

        let bgImage = tomlString(background, "path") ?? tomlString(background, "image")
        let bgPath: String? = if let bgImage, !bgImage.isEmpty { expandTilde(bgImage) } else { nil }
        let osc52 = tomlString(security, "osc52").flatMap(OSC52Policy.init(rawValue:))

        return CopadConfig(
            shell: tomlString(terminal, "shell") ?? d.shell,
            fontFamily: tomlString(terminal, "font_family") ?? d.fontFamily,
            fontSize: tomlInt(terminal, "font_size") ?? d.fontSize,
            optionAsAlt: tomlBool(terminal, "option_as_alt") ?? d.optionAsAlt,
            forceMetaKeys: tomlStringArray(terminal, "force_meta_keys") ?? d.forceMetaKeys,
            closeOnExit: tomlBool(terminal, "close_on_exit") ?? d.closeOnExit,
            themeName: tomlString(theme, "name") ?? d.themeName,
            backgroundPath: bgPath,
            backgroundTint: clamp01(tomlDouble(background, "tint") ?? d.backgroundTint),
            backgroundOpacity: clamp01(tomlDouble(background, "opacity") ?? d.backgroundOpacity),
            rotateInterval: tomlInt(background, "rotate_interval").map { UInt(max(0, $0)) }
                ?? d.rotateInterval,
            osc52: osc52 ?? d.osc52,
            // Smart default: a wallpaper config implies the user wants
            // to see the wallpaper, so default to transparent-default-
            // bg unless they explicitly say otherwise. Without this the
            // alacritty backend's opaque-default-fill design means the
            // wallpaper is invisible to anyone who didn't separately
            // know to set `[renderer] transparent_default_bg = true`.
            transparentDefaultBg: tomlBool(renderer, "transparent_default_bg")
                ?? (bgPath != nil ? true : d.transparentDefaultBg),
            rendererGPU: tomlBool(renderer, "gpu") ?? d.rendererGPU,
            windowOpacity: clamp01(tomlDouble(window, "opacity") ?? d.windowOpacity),
            windowBlur: tomlBool(window, "blur") ?? d.windowBlur,
            tabsPosition: tomlString(tabs, "position").map(TabsPosition.parse) ?? d.tabsPosition,
            // Clamp the layout boundary: a vertical tab pill is `barWidth - 8`
            // wide, so a width below the 8px inset yields a negative
            // (unsatisfiable) constraint, and anything under `collapsedBarWidth`
            // (44) makes the expanded bar narrower than its collapsed state.
            // Floor to the minimum usable width (matches the horizontal min pill).
            tabsWidth: max(CopadConfig.minTabsWidth, tomlInt(tabs, "width") ?? d.tabsWidth),
            statusBar: StatusBarConfig(
                enabled: tomlBool(statusbar, "enabled") ?? d.statusBar.enabled,
                position: tomlString(statusbar, "position") ?? d.statusBar.position,
                height: tomlInt(statusbar, "height") ?? d.statusBar.height,
            ),
            keybindings: parseKeybindings(from: contents),
            triggers: parseTriggersArray(from: contents),
        )
    }

    // MARK: - Lenient TOML field reads
    //
    // Read a single field off a section table, coercing across TOML's number
    // types the way serde does on Linux, so a shared config behaves identically
    // on both platforms. A missing OR wrong-typed field returns nil, letting the
    // caller fall back to that field's own default instead of aborting the whole
    // parse. See `parse(_:)` for why this replaced strict Codable decoding.

    private static func tomlString(_ table: TOMLTable?, _ key: String) -> String? {
        table?[key]?.string
    }

    private static func tomlBool(_ table: TOMLTable?, _ key: String) -> Bool? {
        table?[key]?.bool
    }

    /// Prefer an integer; tolerate a float literal (`font_size = 14.0`) by
    /// truncating. TOMLKit reports exactly one of `.int`/`.double` per value.
    /// `Int(exactly:)` (not `Int(_:)`) is deliberate: a raw `Int(dbl)` traps on
    /// `nan`/`inf`/out-of-range floats — all representable in TOML — which would
    /// turn one malformed field into a crash instead of a per-field fallback.
    private static func tomlInt(_ table: TOMLTable?, _ key: String) -> Int? {
        guard let v = table?[key] else { return nil }
        if let i = v.int { return i }
        if let dbl = v.double { return Int(exactly: dbl.rounded(.towardZero)) }
        return nil
    }

    /// Prefer a float; tolerate an integer literal (`tint = 0`) by widening —
    /// this is the coercion whose absence broke the whole config on macOS.
    private static func tomlDouble(_ table: TOMLTable?, _ key: String) -> Double? {
        guard let v = table?[key] else { return nil }
        if let dbl = v.double { return dbl }
        if let i = v.int { return Double(i) }
        return nil
    }

    /// A homogeneous string array, or nil. If ANY element is not a string the
    /// whole field is treated as malformed and returns nil (→ the caller's
    /// default) rather than silently dropping bad elements — otherwise
    /// `force_meta_keys = [1]` would collapse to `[]` and disable the default
    /// binding instead of falling back to it. An explicit `[]` is honored.
    private static func tomlStringArray(_ table: TOMLTable?, _ key: String) -> [String]? {
        guard let arr = table?[key]?.array else { return nil }
        var result: [String] = []
        for element in arr {
            guard let s = element.string else { return nil }
            result.append(s)
        }
        return result
    }

    /// JSON-friendly trigger list ready to ship through `CopadEngine.setTriggers`.
    /// Just exposes the parsed `[[triggers]]` array; kept as a static helper
    /// (rather than an instance method) so AppDelegate doesn't have to know
    /// the encoding rules.
    static func triggersJSON(from config: CopadConfig) -> [[String: Any]] {
        config.triggers
    }

    /// Minimum usable width for a vertical (`left`/`right`) tab bar. Below
    /// this the full-width tab pills would clip or, past the 8px inset, hit
    /// unsatisfiable Auto Layout constraints. Comfortably above
    /// `TabBarView.collapsedBarWidth` (44) so expanded is never narrower
    /// than collapsed. Matches the horizontal min pill width.
    static let minTabsWidth = 80

    static var defaults: CopadConfig {
        CopadConfig(
            shell: ProcessInfo.processInfo.environment["SHELL"] ?? "/bin/zsh",
            fontFamily: "JetBrains Mono",
            fontSize: 14,
            optionAsAlt: true,
            forceMetaKeys: ["Return"],
            closeOnExit: true,
            themeName: "catppuccin-mocha",
            backgroundPath: nil,
            backgroundTint: 0.6,
            backgroundOpacity: 1.0,
            rotateInterval: 0,
            osc52: .deny,
            transparentDefaultBg: false,
            rendererGPU: true,
            windowOpacity: 1.0,
            windowBlur: false,
            tabsPosition: .top,
            tabsWidth: 200,
            statusBar: .defaults,
            keybindings: [:],
            triggers: [],
        )
    }

    private static func clamp01(_ d: Double) -> Double {
        max(0, min(1, d))
    }

    private static func expandTilde(_ path: String) -> String {
        guard path.hasPrefix("~") else { return path }
        let home = FileManager.default.homeDirectoryForCurrentUser.path
        return home + path.dropFirst()
    }

    /// Walk the TOML `[[triggers]]` array into JSON-friendly `[[String: Any]]`.
    /// We can't use a plain Decodable struct because trigger entries contain
    /// arbitrary nested values (`params`, `payload_match`) that we don't want
    /// to type statically — Rust's `serde_json::Value` round-trips the same
    /// tree losslessly. Walks via `TOMLTable` opaque API so the values flow
    /// straight into `JSONSerialization`-compatible types.
    private static func parseTriggersArray(from contents: String) -> [[String: Any]] {
        guard let table = try? TOMLTable(string: contents),
              let arr = table["triggers"]?.array
        else {
            return []
        }
        var result: [[String: Any]] = []
        for value in arr {
            if let dict = tomlValueToDict(value) {
                result.append(dict)
            }
        }
        return result
    }

    private static func tomlValueToDict(_ v: TOMLValueConvertible) -> [String: Any]? {
        guard let table = v.table else { return nil }
        var dict: [String: Any] = [:]
        for key in table.keys {
            if let val = table[key], let any = tomlValueToAny(val) {
                dict[key] = any
            }
        }
        return dict
    }

    /// Walk the `[keybindings]` table into a `[combo: command]` dict. Same
    /// rationale as triggers — the schema is a flat string-to-string dict
    /// and we don't want a separate Decodable struct just for that.
    private static func parseKeybindings(from contents: String) -> [String: String] {
        guard let table = try? TOMLTable(string: contents),
              let kb = table["keybindings"]?.table
        else {
            return [:]
        }
        var dict: [String: String] = [:]
        for key in kb.keys {
            if let val = kb[key], let s = val.string {
                dict[key] = s
            }
        }
        return dict
    }

    private static func tomlValueToAny(_ v: TOMLValueConvertible) -> Any? {
        // Order matters: check leaf types before composites because TOMLValue
        // may report multiple accessors as non-nil for ambiguous cases.
        if let s = v.string { return s }
        if let i = v.int { return i }
        if let d = v.double { return d }
        if let b = v.bool { return b }
        if let arr = v.array {
            return arr.compactMap(tomlValueToAny)
        }
        if let table = v.table {
            var d: [String: Any] = [:]
            for key in table.keys {
                if let val = table[key], let any = tomlValueToAny(val) {
                    d[key] = any
                }
            }
            return d
        }
        return nil
    }
}

// Note: the former private `RawConfig`/`*Section` Codable shadow structs were
// removed when `parse(_:)` switched to lenient per-field `TOMLTable` reads. A
// single field with the wrong TOML number type (e.g. `tint = 0` for a Double)
// made the strict decoder throw and discard the entire config; the per-field
// reader coerces and degrades gracefully instead. `OSC52Policy` and
// `TabsPosition` remain as `Decodable` enums but are now decoded from a raw
// string via their `init(rawValue:)` / `parse(_:)` helpers, not a struct.
