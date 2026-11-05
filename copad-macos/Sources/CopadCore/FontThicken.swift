import CoreGraphics

public enum FontThicken: Equatable, Sendable {
    case auto
    case on
    case off

    public func smooths(scale: CGFloat) -> Bool {
        switch self {
        case .auto: scale < 2
        case .on: true
        case .off: false
        }
    }

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
