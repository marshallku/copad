import AppKit
import Foundation

/// Cmd+click on a bare `https://...` in terminal output opens it in the default browser.
enum URLClickHelper {
/// Deliberately conservative: stops at usual trailing punctuation, under-matching rather than
/// opening the wrong target. Not RFC-compliant.
    static let urlRegex: NSRegularExpression = // swiftlint:disable:next force_try
        try! NSRegularExpression(
            pattern: #"\bhttps?://[^\s<>"\)\]\,]+"#,
            options: [.caseInsensitive],
        )

    private static let trailingPunct: Set<Character> = [".", ",", ";", "!", "?", ":"]

    static func trimURLTrailingPunctuation(_ s: String) -> String {
        guard let last = s.last, trailingPunct.contains(last) else { return s }
        return String(s.dropLast())
    }
}
