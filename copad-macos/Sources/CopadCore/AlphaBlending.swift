import Foundation


/// `[terminal] alpha_blending` — the color space the Metal renderer blends glyph edges
/// and translucent overlays in.
///
/// `native` blends the stored sRGB values directly, as macOS and most terminals do. For
/// light text on a dark background that leaves an anti-aliased edge DARKER than the
/// physically correct mix, so strokes read thinner. `linear` blends in linear light (an
/// `_srgb` drawable decodes, blends, re-encodes), which is the correct mix and makes the
/// same text read heavier. Only opaque backgrounds blend linearly: over the wallpaper or a
/// translucent window the layer is composited by Core Animation as sRGB-premultiplied, so
/// those keep `native`.
public enum AlphaBlending: Equatable, Sendable {
    case native
    case linear

    /// Whether a frame blends in linear light, given whether its background is transparent.
    public func blendsLinearly(transparentBackground: Bool) -> Bool {
        self == .linear && !transparentBackground
    }

    /// `"native"` or `"linear"`, any case. `nil` for anything else.
    public static func parse(_ string: String?) -> AlphaBlending? {
        switch string?.lowercased() {
        case "native": .native
        case "linear": .linear
        default: nil
        }
    }

    /// The sRGB transfer function, decoded: an encoded channel in 0...1 to linear light.
    /// The shader's `srgb_to_linear` is the same curve.
    public static func linearize(_ c: Float) -> Float {
        c <= 0.04045 ? c / 12.92 : pow((c + 0.055) / 1.055, 2.4)
    }
}
