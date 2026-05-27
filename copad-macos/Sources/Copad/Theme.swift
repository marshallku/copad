import AppKit
import CCopadFFI

struct RGBColor {
    let r: UInt8
    let g: UInt8
    let b: UInt8

    init(hex: String) {
        let h = hex.trimmingCharacters(in: CharacterSet(charactersIn: "#"))
        let padded = h.count >= 6 ? h : String(repeating: "0", count: 6 - h.count) + h
        r = UInt8(strtoul(String(padded.prefix(2)), nil, 16))
        g = UInt8(strtoul(String(padded.dropFirst(2).prefix(2)), nil, 16))
        b = UInt8(strtoul(String(padded.dropFirst(4).prefix(2)), nil, 16))
    }

    var nsColor: NSColor {
        NSColor(
            red: CGFloat(r) / 255.0,
            green: CGFloat(g) / 255.0,
            blue: CGFloat(b) / 255.0,
            alpha: 1.0,
        )
    }
}

/// UI-side model. Field layout mirrors `copad_core::theme::Theme` exactly.
/// Palette data lives in Rust; this struct is populated by decoding the JSON
/// returned by `copad_ffi_theme_get`.
struct CopadTheme {
    let name: String
    let foreground: RGBColor
    let background: RGBColor
    /// 16-color ANSI palette (8 normal + 8 bright)
    let palette: [RGBColor]

    // UI semantic colors
    let surface0: RGBColor // Darker bg (tab bar, panels)
    let surface1: RGBColor // Hover bg
    let surface2: RGBColor // Active/selected bg
    let overlay0: RGBColor // Borders, separators
    let text: RGBColor // Primary text (active tabs)
    let subtext0: RGBColor // Dim text (inactive tabs)
    let subtext1: RGBColor // Hover text
    let accent: RGBColor // Focus rings, active indicators
    let red: RGBColor // Destructive/error (close hover)

    /// Look up a built-in theme by name. Returns nil on unknown name —
    /// `copad_ffi_theme_get` returns NULL and the caller (currently
    /// `AppDelegate.applyConfig`) falls back to `.default`.
    static func byName(_ name: String) -> CopadTheme? {
        guard let cstr = name.withCString({ copad_ffi_theme_get($0) }) else { return nil }
        defer { copad_ffi_free_string(cstr) }
        return decode(jsonCString: cstr)
    }

    /// Matches `copad_core::theme::Theme::default()` ("catppuccin-mocha").
    /// Force-unwrap is intentional: a missing default means the FFI link
    /// or `copad_core::theme` is broken and the app cannot render — fail
    /// loud rather than silently substitute black-on-black.
    static var `default`: CopadTheme {
        guard let theme = byName("catppuccin-mocha") else {
            preconditionFailure("copad-core failed to return catppuccin-mocha theme — FFI broken")
        }
        return theme
    }

    /// JSON wire DTO. Decoder reads hex strings; `from(wire:)` maps to
    /// the UI model. Kept private — outside callers always go through
    /// `byName`.
    private struct Wire: Decodable {
        let name: String
        let foreground: String
        let background: String
        let palette: [String]
        let surface0: String
        let surface1: String
        let surface2: String
        let overlay0: String
        let text: String
        let subtext0: String
        let subtext1: String
        let accent: String
        let red: String
    }

    private static func decode(jsonCString: UnsafePointer<CChar>) -> CopadTheme? {
        let json = String(cString: jsonCString)
        guard let data = json.data(using: .utf8),
              let wire = try? JSONDecoder().decode(Wire.self, from: data),
              wire.palette.count == 16
        else { return nil }
        return CopadTheme(
            name: wire.name,
            foreground: RGBColor(hex: wire.foreground),
            background: RGBColor(hex: wire.background),
            palette: wire.palette.map { RGBColor(hex: $0) },
            surface0: RGBColor(hex: wire.surface0),
            surface1: RGBColor(hex: wire.surface1),
            surface2: RGBColor(hex: wire.surface2),
            overlay0: RGBColor(hex: wire.overlay0),
            text: RGBColor(hex: wire.text),
            subtext0: RGBColor(hex: wire.subtext0),
            subtext1: RGBColor(hex: wire.subtext1),
            accent: RGBColor(hex: wire.accent),
            red: RGBColor(hex: wire.red),
        )
    }
}
