import Foundation


/// Only opaque backgrounds blend linearly: over the wallpaper or a translucent window Core
/// Animation composites the layer as sRGB-premultiplied, so those keep `native`.
public enum AlphaBlending: Equatable, Sendable {
    case native
    case linear

    public func blendsLinearly(transparentBackground: Bool) -> Bool {
        self == .linear && !transparentBackground
    }

    public static func parse(_ string: String?) -> AlphaBlending? {
        switch string?.lowercased() {
        case "native": .native
        case "linear": .linear
        default: nil
        }
    }

    /// The shader's `srgb_to_linear` is the same curve.
    public static func linearize(_ c: Float) -> Float {
        c <= 0.04045 ? c / 12.92 : pow((c + 0.055) / 1.055, 2.4)
    }
}
