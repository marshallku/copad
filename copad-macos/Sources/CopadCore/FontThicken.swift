import CoreGraphics

/// `[terminal] font_thicken` — whether glyphs are rasterized with macOS font
/// smoothing (stem darkening). It thickens strokes for dark-on-light text
/// on an opaque background; copad draws light-on-dark, rasterizes white on
/// transparent and recolors by alpha, and blends without gamma correction —
/// all three make the darkening read as bold, blurred edges. So it is OFF
/// by default (as Ghostty's `font-thicken`). At 1x it is also what stops
/// each thin stem rounding to a random 1px-crisp / 2px-gray weight, so a
/// 1x display may prefer `auto` (on below 2x, off at Retina) or `true`.
public enum FontThicken: Equatable, Sendable {
    /// On below 2x backing scale, off at 2x and above.
    case auto
    case on
    /// The default.
    case off

    public func smooths(scale: CGFloat) -> Bool {
        switch self {
        case .auto: scale < 2
        case .on: true
        case .off: false
        }
    }

    /// A TOML value read as a bool (`true`/`false`) or a string (`"auto"`,
    /// `"true"`/`"on"`, `"false"`/`"off"`). `nil` for anything else.
    public static func parse(bool: Bool?, string: String?) -> FontThicken? {
        if let bool { return bool ? .on : .off }
        switch string?.lowercased() {
        case "auto": return .auto
        case "true", "on": return .on
        case "false", "off": return .off
        default: return nil
        }
    }
}
