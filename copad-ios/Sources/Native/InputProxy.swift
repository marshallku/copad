import UIKit

/// The terminal's keyboard. A real (invisible) `UITextView` is first responder so iOS's own
/// text system — Korean IME included — composes normally; only what it COMMITS is sent.
///
/// This is the native answer to the bug #122 worked around in the PWA (Korean arriving as
/// jamo): the syllable being composed lives in this view as marked text, drawn at the cursor by
/// the terminal, and reaches comux only when the IME commits it. Anything that changes where
/// input goes calls `commitComposition()` first, so a syllable can never land in the next pane.
/// Where the keyboard's output goes — the session, or a recorder in tests.
@MainActor
protocol KeySink: AnyObject {
    func sendText(_ text: String)
    func sendKey(_ k: KeyEvent)
    func send(_ msg: ClientMsg)
}

final class InputProxy: UITextView, UITextViewDelegate {
    weak var session: KeySink?
    weak var terminal: TerminalView?
    /// Sticky Ctrl from the key bar: applies to the next typed character.
    /// Arming it sends a held syllable first, so Ctrl lands on the character typed next.
    var ctrlArmed = false {
        willSet { if newValue && !ctrlArmed { commitComposition() } }
        didSet { bar.setCtrl(ctrlArmed) }
    }
    private lazy var bar = KeyBar(proxy: self)

    init() {
        super.init(frame: .zero, textContainer: nil)
        delegate = self
        autocorrectionType = .no
        autocapitalizationType = .none
        spellCheckingType = .no
        smartQuotesType = .no
        smartDashesType = .no
        smartInsertDeleteType = .no
        inlinePredictionType = .no
        keyboardAppearance = .dark
        backgroundColor = .clear
        textColor = .clear
        tintColor = .clear
        inputAccessoryView = bar
        accessibilityIdentifier = "terminalInput"
    }

    required init?(coder: NSCoder) { fatalError() }

    /// The syllable still being composed, if any.
    var markedText: String {
        guard let r = markedTextRange else { return "" }
        return text(in: r) ?? ""
    }

    func textViewDidChange(_ tv: UITextView) {
        if markedTextRange != nil {
            terminal?.marked = markedText
            return
        }
        flush(holdingSyllable: true)
    }

    func textView(_ tv: UITextView, shouldChangeTextIn range: NSRange, replacementText t: String) -> Bool {
        // Return: commit whatever is pending, then a real Enter key (not a "\n" character).
        if t == "\n" {
            commitComposition()
            session?.sendKey(KeyEvent(code: .enter))
            return false
        }
        return true
    }

    override func deleteBackward() {
        // Nothing local to delete: the backspace is the pane's.
        if (text ?? "").isEmpty, markedTextRange == nil {
            session?.sendKey(KeyEvent(code: .backspace))
            return
        }
        super.deleteBackward()
    }

    override func paste(_ sender: Any?) {
        commitComposition()
        if let s = UIPasteboard.general.string, !s.isEmpty { session?.send(.paste(s)) }
    }

    /// Commit the IME's in-progress text (if any) and send everything committed.
    func commitComposition() {
        if markedTextRange != nil { unmarkText() }
        flush(holdingSyllable: false)
        clear()
    }

    /// What of this view's text the pane already has. The view keeps the line typed since the
    /// last commit rather than emptying as it sends: the Korean keyboard remembers what it
    /// composed, and after the held character is deleted it rewrites the one before (한 → 하)
    /// whether or not the view still has it, so emptying the view left a ghost syllable on
    /// screen and the sent one in the pane.
    private var sent = ""

    /// Send what changed. The iPhone's Korean keyboard composes WITHOUT marked text: it rewrites
    /// the last character in place (ㅎ → 하 → 한, and 한 + ㅏ → 하나), so a trailing Hangul
    /// character is held, drawn like marked text, until the next character, a key or a commit
    /// shows it is final. A rewrite of something already sent — a delete decomposing the
    /// previous syllable — erases it in the pane with backspaces first.
    private func flush(holdingSyllable: Bool) {
        var line = text ?? ""
        var held = ""
        // A plain delete leaves what remains sent: nothing new to hold.
        if holdingSyllable, !sent.hasPrefix(line), let last = line.last, Self.isHangul(last) {
            held = String(last)
            line.removeLast()
        }
        terminal?.marked = held
        let common = zip(sent, line).prefix { $0 == $1 }.count
        for _ in common..<sent.count { session?.sendKey(KeyEvent(code: .backspace)) }
        let added = String(line.dropFirst(common))
        sent = line
        if !added.isEmpty { emit(added) }
    }

    /// Start a new line: the view empties and the keyboard is told, so it drops what it composed.
    private func clear() {
        sent = ""
        terminal?.marked = ""
        guard !(text ?? "").isEmpty else { return }
        inputDelegate?.textWillChange(self)
        text = ""
        inputDelegate?.textDidChange(self)
    }

    /// Hangul syllables and jamo: what the Korean keyboard may still rewrite.
    static func isHangul(_ c: Character) -> Bool {
        c.unicodeScalars.contains { s in
            (0xAC00...0xD7A3).contains(s.value) || (0x1100...0x11FF).contains(s.value)
                || (0x3130...0x318F).contains(s.value)
        }
    }

    private func emit(_ t: String) {
        guard ctrlArmed, let first = t.first else {
            session?.sendText(t)
            return
        }
        ctrlArmed = false
        session?.sendKey(.ctrl(Character(first.lowercased())))
        let rest = String(t.dropFirst())
        if !rest.isEmpty { session?.sendText(rest) }
        // A control character is not text a later delete may take back.
        clear()
    }

    /// Send a key from the bar or a hardware chord, after settling any composition.
    func key(_ k: KeyEvent) {
        commitComposition()
        session?.sendKey(k)
    }

    // MARK: hardware keyboard

    override var keyCommands: [UIKeyCommand]? {
        var cmds: [UIKeyCommand] = [
            UIKeyCommand(input: UIKeyCommand.inputEscape, modifierFlags: [], action: #selector(hw(_:))),
            UIKeyCommand(input: UIKeyCommand.inputUpArrow, modifierFlags: [], action: #selector(hw(_:))),
            UIKeyCommand(input: UIKeyCommand.inputDownArrow, modifierFlags: [], action: #selector(hw(_:))),
            UIKeyCommand(input: UIKeyCommand.inputLeftArrow, modifierFlags: [], action: #selector(hw(_:))),
            UIKeyCommand(input: UIKeyCommand.inputRightArrow, modifierFlags: [], action: #selector(hw(_:))),
            UIKeyCommand(input: "\t", modifierFlags: [], action: #selector(hw(_:))),
            UIKeyCommand(input: "\t", modifierFlags: .shift, action: #selector(hw(_:))),
        ]
        for c in "abcdefghijklmnopqrstuvwxyz[]\\" {
            cmds.append(UIKeyCommand(input: String(c), modifierFlags: .control, action: #selector(hw(_:))))
        }
        cmds.forEach { $0.wantsPriorityOverSystemBehavior = true }
        return cmds
    }

    @objc private func hw(_ c: UIKeyCommand) {
        let code: KeyEvent.Code
        switch c.input {
        case UIKeyCommand.inputEscape: code = .esc
        case UIKeyCommand.inputUpArrow: code = .up
        case UIKeyCommand.inputDownArrow: code = .down
        case UIKeyCommand.inputLeftArrow: code = .left
        case UIKeyCommand.inputRightArrow: code = .right
        case "\t": code = c.modifierFlags.contains(.shift) ? .backTab : .tab
        case let s?: code = .char(Character(s))
        default: return
        }
        var mods: KeyEvent.Mods = []
        if c.modifierFlags.contains(.control) { mods.insert(.control) }
        if c.modifierFlags.contains(.shift), code != .backTab { mods.insert(.shift) }
        key(KeyEvent(code: code, mods: mods))
    }
}

/// The accessory row above the soft keyboard: keys a phone keyboard lacks, and whole comux
/// chords in one tap (a soft keyboard cannot hold a modifier across two taps — #122).
final class KeyBar: UIInputView {
    private weak var proxy: InputProxy?

    init(proxy: InputProxy) {
        self.proxy = proxy
        super.init(frame: CGRect(x: 0, y: 0, width: 320, height: 44), inputViewStyle: .keyboard)
        let scroll = UIScrollView()
        scroll.showsHorizontalScrollIndicator = false
        let stack = UIStackView()
        stack.axis = .horizontal
        stack.spacing = 6
        let items: [(String, String, () -> Void)] = [
            ("esc", "key-esc", { [weak self] in self?.send(.init(code: .esc)) }),
            ("tab", "key-tab", { [weak self] in self?.send(.init(code: .tab)) }),
            ("ctrl", "key-ctrl", { [weak self] in self?.proxy?.ctrlArmed.toggle() }),
            ("↑", "key-up", { [weak self] in self?.send(.init(code: .up)) }),
            ("↓", "key-down", { [weak self] in self?.send(.init(code: .down)) }),
            ("←", "key-left", { [weak self] in self?.send(.init(code: .left)) }),
            ("→", "key-right", { [weak self] in self?.send(.init(code: .right)) }),
            ("^b", "key-prefix", { [weak self] in self?.send(.ctrl("b")) }),
            ("^f", "key-switcher", { [weak self] in self?.send(.ctrl("f")) }),
            ("^b a", "key-center", { [weak self] in self?.chord("a") }),
            ("^b c", "key-newtab", { [weak self] in self?.chord("c") }),
            ("^b n", "key-next", { [weak self] in self?.chord("n") }),
            ("^b p", "key-prev", { [weak self] in self?.chord("p") }),
            ("^b z", "key-zoom", { [weak self] in self?.chord("z") }),
            ("^c", "key-ctrlc", { [weak self] in self?.send(.ctrl("c")) }),
        ]
        for (title, id, action) in items {
            let b = UIButton(type: .system)
            b.setTitle(title, for: .normal)
            b.titleLabel?.font = .monospacedSystemFont(ofSize: 15, weight: .medium)
            b.contentEdgeInsets = UIEdgeInsets(top: 6, left: 10, bottom: 6, right: 10)
            b.backgroundColor = UIColor.secondarySystemBackground
            b.tintColor = .label
            b.layer.cornerRadius = 6
            b.accessibilityIdentifier = id
            b.addAction(UIAction { _ in action() }, for: .touchUpInside)
            if id == "key-ctrl" { ctrlButtonRef(b) }
            stack.addArrangedSubview(b)
        }
        scroll.addSubview(stack)
        addSubview(scroll)
        // Pinned outside the scrolling row so it is always one tap away. Tapping the terminal
        // brings the keyboard back.
        let hide = UIButton(type: .system)
        hide.setImage(UIImage(systemName: "keyboard.chevron.compact.down"), for: .normal)
        hide.tintColor = .secondaryLabel
        hide.accessibilityIdentifier = "key-hide"
        hide.accessibilityLabel = "Hide keyboard"
        hide.addAction(UIAction { [weak self] _ in
            self?.proxy?.commitComposition()
            self?.proxy?.resignFirstResponder()
        }, for: .touchUpInside)
        addSubview(hide)
        hide.translatesAutoresizingMaskIntoConstraints = false
        scroll.translatesAutoresizingMaskIntoConstraints = false
        stack.translatesAutoresizingMaskIntoConstraints = false
        NSLayoutConstraint.activate([
            hide.trailingAnchor.constraint(equalTo: trailingAnchor, constant: -4),
            hide.centerYAnchor.constraint(equalTo: centerYAnchor),
            hide.widthAnchor.constraint(equalToConstant: 44),
            hide.heightAnchor.constraint(equalToConstant: 44),
            scroll.leadingAnchor.constraint(equalTo: leadingAnchor),
            scroll.trailingAnchor.constraint(equalTo: hide.leadingAnchor),
            scroll.topAnchor.constraint(equalTo: topAnchor),
            scroll.bottomAnchor.constraint(equalTo: bottomAnchor),
            stack.leadingAnchor.constraint(equalTo: scroll.contentLayoutGuide.leadingAnchor, constant: 6),
            stack.trailingAnchor.constraint(equalTo: scroll.contentLayoutGuide.trailingAnchor, constant: -6),
            stack.centerYAnchor.constraint(equalTo: scroll.centerYAnchor),
            stack.heightAnchor.constraint(equalToConstant: 34),
        ])
    }

    required init?(coder: NSCoder) { fatalError() }

    private weak var ctrlRef: UIButton?
    private func ctrlButtonRef(_ b: UIButton) { ctrlRef = b }

    func setCtrl(_ on: Bool) {
        ctrlRef?.backgroundColor = on ? UIColor.tertiaryLabel : UIColor.secondarySystemBackground
    }

    private func send(_ k: KeyEvent) { proxy?.key(k) }

    /// The prefix and the chord key in one tap.
    private func chord(_ c: Character) {
        proxy?.key(.ctrl("b"))
        proxy?.key(KeyEvent(code: .char(c)))
    }
}
