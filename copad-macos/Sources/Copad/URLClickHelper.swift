import AppKit
import Foundation

/// Cmd+click on a bare `https://...` in terminal output opens it in
/// the default browser. Phase 10b removed SwiftTerm — only the
/// alacritty backend uses this now, and it operates on the snapshot
/// utf8 buffer directly (no NSEvent monitor / SwiftTerm-view
/// reverse-engineering needed). The shared static helpers below stay
/// so the alacritty path doesn't have to re-implement the same
/// regex + trailing-punctuation trim.
enum URLClickHelper {
    /// Conservative-ish URL match: scheme + `://` + non-whitespace,
    /// terminating at characters that are usually trailing punctuation
    /// (`)`, `]`, `>`, `"`, `,`, `.`). This isn't RFC-compliant —
    /// terminals historically show messy URLs and we'd rather
    /// under-match than open the wrong target. Linux's VTE uses a
    /// similar heuristic.
    static let urlRegex: NSRegularExpression = // swiftlint:disable:next force_try
        try! NSRegularExpression(
            pattern: #"\bhttps?://[^\s<>"\)\]\,]+"#,
            options: [.caseInsensitive],
        )

    private static let trailingPunct: Set<Character> = [".", ",", ";", "!", "?", ":"]

    /// Drop a single trailing punctuation character that the regex
    /// didn't catch (URLs often end mid-sentence with a `.` or `!`).
    /// Conservative: only strip the very last char and only if it's in
    /// the set, so legitimate fragments aren't mangled.
    static func trimURLTrailingPunctuation(_ s: String) -> String {
        guard let last = s.last, trailingPunct.contains(last) else { return s }
        return String(s.dropLast())
    }
}
