import Foundation

/// A key as comux receives it: crossterm's `KeyEvent` serde form (pinned by comux's
/// `wire_grammar::keys` test). The phone sends WHAT was pressed; comux encodes it for the
/// pane it lands in (`keys.rs`), so no escape sequence is ever built here.
struct KeyEvent: Equatable {
    enum Code: Equatable {
        case char(Character)
        case enter, esc, backspace, tab, backTab, delete, insert
        case up, down, left, right, home, end, pageUp, pageDown
        case f(Int)
    }

    struct Mods: OptionSet, Equatable {
        let rawValue: Int
        static let shift = Mods(rawValue: 1)
        static let control = Mods(rawValue: 2)
        static let alt = Mods(rawValue: 4)
    }

    var code: Code
    var mods: Mods = []

    /// crossterm's bitflags order, joined with `" | "`; empty = `""`.
    var modsString: String {
        var parts: [String] = []
        if mods.contains(.shift) { parts.append("SHIFT") }
        if mods.contains(.control) { parts.append("CONTROL") }
        if mods.contains(.alt) { parts.append("ALT") }
        return parts.joined(separator: " | ")
    }

    var codeJSON: Any {
        switch code {
        case let .char(c): return ["Char": String(c)]
        case let .f(n): return ["F": n]
        case .enter: return "Enter"
        case .esc: return "Esc"
        case .backspace: return "Backspace"
        case .tab: return "Tab"
        case .backTab: return "BackTab"
        case .delete: return "Delete"
        case .insert: return "Insert"
        case .up: return "Up"
        case .down: return "Down"
        case .left: return "Left"
        case .right: return "Right"
        case .home: return "Home"
        case .end: return "End"
        case .pageUp: return "PageUp"
        case .pageDown: return "PageDown"
        }
    }

    var json: [String: Any] {
        ["code": codeJSON, "modifiers": modsString, "kind": "Press", "state": ""]
    }

    /// The keys a run of committed TEXT stands for: one `Char` per character, `\n`/`\r`
    /// (a `\r\n` pair counts once) as Enter and `\t` as Tab — the same split comux's own
    /// `paste_keys` makes for a terminal without bracketed paste.
    static func keys(for text: String) -> [KeyEvent] {
        // crossterm's `Char` is ONE Unicode scalar, so a grapheme cluster (decomposed `é`, a
        // family emoji) goes as its scalars in order — the pane reassembles them. Newlines are
        // folded per scalar: `\r\n` counts once.
        var out: [KeyEvent] = []
        var prevCR = false
        for u in text.unicodeScalars {
            switch u {
            case "\r": out.append(KeyEvent(code: .enter)); prevCR = true; continue
            case "\n": if !prevCR { out.append(KeyEvent(code: .enter)) }
            case "\t": out.append(KeyEvent(code: .tab))
            default: out.append(KeyEvent(code: .char(Character(u))))
            }
            prevCR = false
        }
        return out
    }

    /// `Ctrl-<c>` — the prefix (`^b`) and every control chord.
    static func ctrl(_ c: Character) -> KeyEvent { KeyEvent(code: .char(c), mods: .control) }
}
