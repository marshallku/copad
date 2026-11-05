import CopadCore
import Foundation
import TOMLKit

/// Policy for OSC 52 clipboard writes from the PTY, applied via `drainClipboardRequests`.
///
/// Default `allow`, as in ghostty/kitty/wezterm/alacritty. `deny` silently breaks tools that copy
/// by emitting OSC 52 (comux drag-to-copy, nvim's osc52 provider, tmux `set-clipboard`, yank over
/// SSH), and the warning has nowhere to print for a Finder-launched .app. Clipboard reads are
/// never answered under either policy; that is the exfiltration path.
enum OSC52Policy: String, Decodable {
    case deny
    case allow
}

/// Where the tab bar sits: `top`/`bottom` (fixed height) or `left`/`right` (fixed `[tabs] width`).
enum TabsPosition: String, Decodable {
    case top
    case bottom
    case left
    case right

    /// True for `left`/`right`, where the bar runs down one side at a fixed width.
    var isVertical: Bool { self == .left || self == .right }

    /// Unrecognized values fall back to `.top` with a stderr warning.
    static func parse(_ raw: String) -> TabsPosition {
        if let p = TabsPosition(rawValue: raw.lowercased()) { return p }
        let msg = "[copad] [tabs] position '\(raw)' unrecognized — using 'top'\n"
        FileHandle.standardError.write(Data(msg.utf8))
        return .top
    }
}

/// `[statusbar]` config. Only `bottom` is supported on macOS.
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
    let fontThicken: FontThicken
    let alphaBlending: AlphaBlending
    /// macOS only: Option+key sends `ESC + key` instead of going through the IME (which turns
    /// Option+1 into `¡`), so tmux/zsh/readline Meta bindings fire. Default `true`.
    let optionAsAlt: Bool
    /// macOS only: control-character keys that send `ESC + <byte>` with Option regardless of
    /// `option_as_alt` (default `["Return"]`: the newline sequence Claude Code and Python REPLs
    /// accept). Names, case-insensitive: `Return`/`Enter`, `Escape`/`Esc`; unknown ones warn on
    /// stderr. Arrows and Delete are excluded: they rely on Cocoa's `moveWordLeft:` /
    /// `deleteWordBackward:` bindings, which a force-Meta would steal.
    let forceMetaKeys: [String]
    /// `[terminal] close_on_exit`: close the pane when the shell exits, cascading to the tab and
    /// window. `false` keeps the dead viewport so the exit message can be read.
    let closeOnExit: Bool
    /// `[terminal] kitty_keyboard`: kitty keyboard protocol negotiation (`CSI > flags u`), how
    /// Claude Code receives Shift+Enter. `false` leaves the query unanswered.
    let kittyKeyboard: Bool
    /// `[terminal] scrollback`: lines per pane, clamped to 0...100 000. Each kept line costs its
    /// full width in cells. Integers only.
    let scrollback: Int
    /// `[terminal] images`: kitty graphics protocol (yazi previews, `kitten icat`). `false`
    /// ignores image commands without replying so programs fall back to text.
    let images: Bool
    let themeName: String
    let backgroundPath: String?
    let backgroundTint: Double
    /// Opacity of the background image layer; `backgroundTint` darkens it via an overlay instead.
    let backgroundOpacity: Double
    /// `[background] rotate_interval`: seconds between random wallpapers; 0 disables the timer
    /// (`coctl background next` still works). Mirrors `BackgroundConfig::rotate_interval`.
    let rotateInterval: UInt
    let osc52: OSC52Policy
    /// `[renderer] transparent_default_bg = true` makes default-bg cells transparent so a
    /// background image shows through. Off by default so the cursor stays visible. Explicit ANSI
    /// bg and reverse-video cells stay opaque.
    let transparentDefaultBg: Bool
    /// `[renderer] gpu`: Metal render path, default on (about 5.5x cheaper main-thread render
    /// than CoreText; docs/macos-gpu-renderer-plan.md). `gpu = false` selects the CoreText
    /// painter. Read at pane creation, so existing panes keep their painter across hot-reloads.
    /// Falls back to CoreText when no Metal device exists.
    let rendererGPU: Bool
    /// `[window] opacity`, 0.0...1.0: the window and terminal default-bg cells. Distinct from
    /// `backgroundOpacity`, which only affects the background-image layer.
    let windowOpacity: Double
    /// `[window] blur = true` (macOS only): with `windowOpacity < 1.0`, AppDelegate puts an
    /// `NSVisualEffectView` behind the content to blur the desktop.
    let windowBlur: Bool
    let tabsPosition: TabsPosition
    /// `[tabs] width`: points for the vertical tab bar; ignored for `top`/`bottom`.
    let tabsWidth: Int
    /// `[statusbar]` config; modules come from plugin manifests' `[[modules]]`.
    let statusBar: StatusBarConfig
    /// `[keybindings]` combo to command, compiled to `Keybindings.Binding` at AppDelegate init.
    let keybindings: [String: String]
    /// `[browser] restore`: how much of a live webview URL may be persisted (`origin` default |
    /// `url` | `full`). Passed verbatim to the Rust canonicaliser via `BrowserFFI`, the only
    /// place it is interpreted (unknown falls back to `origin` there). Decision #100.
    let browserRestore: String
    /// `[browser] capture_bodies`: include bodies in the network log. Off by default; bodies are
    /// the likeliest place for a secret to end up in a file the agent can read.
    let browserCaptureBodies: Bool
    /// Raw `[[triggers]]` array as JSON-friendly values for the Rust trigger engine, which owns the
    /// schema (arbitrary nested values under `params` / `payload_match`).
    let triggers: [[String: Any]]

    /// `$XDG_CONFIG_HOME/copad/config.toml`, else `~/.config/copad/config.toml`; mirrors
    /// `copad_core::config::CopadConfig::config_path()`.
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

    /// Read + parse `~/.config/copad/config.toml`. Returns `.defaults` when absent. Throws on parse
    /// failure so the caller can start with defaults (launch) or keep the live config (reload).
    static func load() throws -> CopadConfig {
        let configURL = configPath()
        guard let contents = try? String(contentsOf: configURL, encoding: .utf8) else {
            return .defaults
        }
        return try parse(contents)
    }

    /// Decode a TOML config string. Throws only on a TOML syntax error, which is written to
    /// stderr. Unknown sections are tolerated.
    ///
    /// Fields are read off the raw `TOMLTable` instead of strict-decoded, for two reasons:
    ///   1. TOMLKit's strict decoder rejects an integer literal for a `Double` field (`tint = 0`).
    ///      That throw discarded the entire config and fell back to defaults (a non-Nerd font, so
    ///      glyph icons rendered as tofu), while Linux/serde coerces int to float.
    ///      `tomlDouble`/`tomlInt` coerce the same way.
    ///   2. Strict decoding is all-or-nothing; per-field reads fall back to that field's default.
    /// `comux doctor` lints these issues.
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
        let browser = table["browser"]?.table

        let bgImage = tomlString(background, "path") ?? tomlString(background, "image")
        let bgPath: String? = if let bgImage, !bgImage.isEmpty { expandTilde(bgImage) } else { nil }
        let osc52 = tomlString(security, "osc52").flatMap(OSC52Policy.init(rawValue:))

        return CopadConfig(
            shell: tomlString(terminal, "shell") ?? d.shell,
            fontFamily: tomlString(terminal, "font_family") ?? d.fontFamily,
            fontSize: tomlInt(terminal, "font_size") ?? d.fontSize,
            fontThicken: fontThicken(terminal) ?? d.fontThicken,
            alphaBlending: alphaBlending(terminal) ?? d.alphaBlending,
            optionAsAlt: tomlBool(terminal, "option_as_alt") ?? d.optionAsAlt,
            forceMetaKeys: tomlStringArray(terminal, "force_meta_keys") ?? d.forceMetaKeys,
            closeOnExit: tomlBool(terminal, "close_on_exit") ?? d.closeOnExit,
            kittyKeyboard: tomlBool(terminal, "kitty_keyboard") ?? d.kittyKeyboard,
            scrollback: terminal?["scrollback"]?.int.map { min(max($0, 0), 100_000) } ?? d.scrollback,
            images: tomlBool(terminal, "images") ?? d.images,
            themeName: tomlString(theme, "name") ?? d.themeName,
            backgroundPath: bgPath,
            backgroundTint: clamp01(tomlDouble(background, "tint") ?? d.backgroundTint),
            backgroundOpacity: clamp01(tomlDouble(background, "opacity") ?? d.backgroundOpacity),
            rotateInterval: tomlInt(background, "rotate_interval").map { UInt(max(0, $0)) }
                ?? d.rotateInterval,
            osc52: osc52 ?? d.osc52,
            // A wallpaper config implies the user wants to see it; without this the opaque
            // default fill hides it until `[renderer] transparent_default_bg = true` is set.
            transparentDefaultBg: tomlBool(renderer, "transparent_default_bg")
                ?? (bgPath != nil ? true : d.transparentDefaultBg),
            rendererGPU: tomlBool(renderer, "gpu") ?? d.rendererGPU,
            windowOpacity: clamp01(tomlDouble(window, "opacity") ?? d.windowOpacity),
            windowBlur: tomlBool(window, "blur") ?? d.windowBlur,
            tabsPosition: tomlString(tabs, "position").map(TabsPosition.parse) ?? d.tabsPosition,
            // Clamp: a vertical tab pill is `barWidth - 8` wide, so a smaller width gives a
            // negative constraint, and under `collapsedBarWidth` (44) the expanded bar would be
            // narrower than the collapsed one.
            tabsWidth: max(CopadConfig.minTabsWidth, tomlInt(tabs, "width") ?? d.tabsWidth),
            statusBar: StatusBarConfig(
                enabled: tomlBool(statusbar, "enabled") ?? d.statusBar.enabled,
                position: tomlString(statusbar, "position") ?? d.statusBar.position,
                height: tomlInt(statusbar, "height") ?? d.statusBar.height,
            ),
            keybindings: parseKeybindings(from: contents),
            browserRestore: tomlString(browser, "restore") ?? d.browserRestore,
            browserCaptureBodies: tomlBool(browser, "capture_bodies") ?? d.browserCaptureBodies,
            triggers: parseTriggersArray(from: contents),
        )
    }

    // MARK: - Lenient TOML field reads
    // Reads one field off a section table, coercing across TOML number types as serde does on
    // Linux. A missing or wrong-typed field returns nil so the caller falls back to that field's
    // own default. See `parse(_:)`.

    private static func tomlString(_ table: TOMLTable?, _ key: String) -> String? {
        table?[key]?.string
    }

    /// `[terminal] font_thicken`, warning (once per load) on a value it cannot read.
    private static func fontThicken(_ table: TOMLTable?) -> FontThicken? {
        guard let raw = table?["font_thicken"] else { return nil }
        if let parsed = FontThicken.parse(bool: raw.bool, string: raw.string) { return parsed }
        FileHandle.standardError.write(
            Data("[copad] [terminal] font_thicken must be true, false or \"auto\" — using the default (false)\n".utf8),
        )
        return nil
    }

    private static func alphaBlending(_ table: TOMLTable?) -> AlphaBlending? {
        guard let raw = table?["alpha_blending"] else { return nil }
        if let parsed = AlphaBlending.parse(raw.string) { return parsed }
        FileHandle.standardError.write(
            Data("[copad] [terminal] alpha_blending must be \"native\" or \"linear\" — using the default (native)\n".utf8),
        )
        return nil
    }

    private static func tomlBool(_ table: TOMLTable?, _ key: String) -> Bool? {
        table?[key]?.bool
    }

    /// Prefer an integer; truncate a float literal (`font_size = 14.0`). `Int(exactly:)` because a
    /// raw `Int(dbl)` traps on `nan`/`inf`/out-of-range values, which TOML can represent.
    private static func tomlInt(_ table: TOMLTable?, _ key: String) -> Int? {
        guard let v = table?[key] else { return nil }
        if let i = v.int { return i }
        if let dbl = v.double { return Int(exactly: dbl.rounded(.towardZero)) }
        return nil
    }

    /// Prefer a float; widen an integer literal (`tint = 0`), whose rejection once broke the
    /// whole config on macOS.
    private static func tomlDouble(_ table: TOMLTable?, _ key: String) -> Double? {
        guard let v = table?[key] else { return nil }
        if let dbl = v.double { return dbl }
        if let i = v.int { return Double(i) }
        return nil
    }

    /// A homogeneous string array, or nil. Any non-string element makes the whole field nil (the
    /// caller's default), so `force_meta_keys = [1]` does not collapse to `[]` and disable the
    /// default binding. An explicit `[]` is honored.
    private static func tomlStringArray(_ table: TOMLTable?, _ key: String) -> [String]? {
        guard let arr = table?[key]?.array else { return nil }
        var result: [String] = []
        for element in arr {
            guard let s = element.string else { return nil }
            result.append(s)
        }
        return result
    }

    static func triggersJSON(from config: CopadConfig) -> [[String: Any]] {
        config.triggers
    }

    /// Minimum usable width for a vertical tab bar: below it the pills clip or hit unsatisfiable
    /// Auto Layout constraints past the 8px inset. Above `TabBarView.collapsedBarWidth` (44) so
    /// expanded is never narrower than collapsed.
    static let minTabsWidth = 80

    static var defaults: CopadConfig {
        CopadConfig(
            shell: ProcessInfo.processInfo.environment["SHELL"] ?? "/bin/zsh",
            fontFamily: "JetBrains Mono",
            fontSize: 14,
            fontThicken: .off,
            alphaBlending: .native,
            optionAsAlt: true,
            forceMetaKeys: ["Return"],
            closeOnExit: true,
            kittyKeyboard: true,
            scrollback: 10_000,
            images: true,
            themeName: "catppuccin-mocha",
            backgroundPath: nil,
            backgroundTint: 0.6,
            backgroundOpacity: 1.0,
            rotateInterval: 0,
            osc52: .allow,
            transparentDefaultBg: false,
            rendererGPU: true,
            windowOpacity: 1.0,
            windowBlur: false,
            tabsPosition: .top,
            tabsWidth: 200,
            statusBar: .defaults,
            keybindings: [:],
            browserRestore: "origin",
            browserCaptureBodies: false,
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

    /// Walk the TOML `[[triggers]]` array into JSON-friendly values. Entries hold arbitrary nested
    /// values (`params`, `payload_match`) that are not typed statically; Rust's `serde_json::Value`
    /// round-trips the same tree.
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

    /// Walk the `[keybindings]` table into a `[combo: command]` dict.
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
        // Leaf types before composites: TOMLValue can report several accessors non-nil.
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
