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
    var ctrlArmed = false { didSet { bar.setCtrl(ctrlArmed) } }
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
        flush()
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
        flush()
    }

    /// Send the committed text and empty the view.
    private func flush() {
        terminal?.marked = ""
        let t = text ?? ""
        guard !t.isEmpty else { return }
        text = ""
        emit(t)
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
            b.layer.cornerRadius = 6
            b.accessibilityIdentifier = id
            b.addAction(UIAction { _ in action() }, for: .touchUpInside)
            if id == "key-ctrl" { ctrlButtonRef(b) }
            stack.addArrangedSubview(b)
        }
        scroll.addSubview(stack)
        addSubview(scroll)
        scroll.translatesAutoresizingMaskIntoConstraints = false
        stack.translatesAutoresizingMaskIntoConstraints = false
        NSLayoutConstraint.activate([
            scroll.leadingAnchor.constraint(equalTo: leadingAnchor),
            scroll.trailingAnchor.constraint(equalTo: trailingAnchor),
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
        ctrlRef?.backgroundColor = on ? UIColor.systemBlue.withAlphaComponent(0.5) : UIColor.secondarySystemBackground
    }

    private func send(_ k: KeyEvent) { proxy?.key(k) }

    /// The prefix and the chord key in one tap.
    private func chord(_ c: Character) {
        proxy?.key(.ctrl("b"))
        proxy?.key(KeyEvent(code: .char(c)))
    }
}
