import AppKit
import Foundation

/// Tier 1.2 — custom keybindings parsed from `[keybindings]` in `config.toml`.
///
/// TOML shape (matches Linux verbatim):
///
/// ```toml
/// [keybindings]
/// "cmd+shift+g" = "spawn:~/scripts/grep-something.sh"
/// "ctrl+shift+m" = "spawn:~/scripts/toggle-mute.sh"
/// "cmd+e" = "action:webview.open url=https://example.com"
/// ```
///
/// Two value syntaxes:
///
/// - **`spawn:<cmd>`** — runs `<cmd>` via `sh -c` in a detached background
///   `Process` with `COPAD_SOCKET` injected so the script can call back via
///   `coctl`. Mirrors Linux's `spawn_command`.
/// - **`action:<method> [k=v ...]`** — dispatches `<method>` through the
///   `ActionRegistry` with `params` parsed from the `k=v` tail. Lets the
///   user wire keybindings into the same surface plugins + triggers
///   already use (`webview.open`, `git.list_workspaces`, etc.). Plain
///   strings only — no nested params; serializes to JSON via `[String: Any]`.
///
/// Modifier syntax supports both Linux convention (`ctrl`) and macOS-native
/// (`cmd`), plus `shift`/`alt`/`option`. Order doesn't matter (`cmd+shift+g`
/// == `shift+cmd+g`). Key is the unshifted character (e.g. `g`, not `G`).
///
/// Resolution order at keyDown time: built-in shortcuts (Cmd+T, Cmd+W, etc.)
/// fire FIRST via the menu bar, then this monitor sees the key event. So a
/// user binding `cmd+t` to a custom action gets shadowed by the built-in
/// "New Tab" — that's intentional (don't let users break the standard menu).
/// To override, change the menu's keyEquivalent or use a different combo.
enum Keybindings {
    /// Compiled binding ready to match against `NSEvent`.
    struct Binding {
        let modifiers: NSEvent.ModifierFlags
        let key: String // lowercased character, e.g. "g"
        /// Layout-position keyCode resolved from `key` at parse time when
        /// the name is in `nameToKeyCode`. `matches` prefers this so
        /// non-Latin IMEs (Korean, JP, …) — which translate `p` to `ㅖ`
        /// before the event reaches us — don't shadow the binding. nil
        /// for keys not in the table; falls back to char comparison.
        let keyCode: UInt16?
        let command: String // raw value from config: "spawn:..." or "action:..."
    }

    /// gdk-style key name → macOS `kVK_*` constant. Covers letters,
    /// digits, common punctuation, brackets, function keys, and the
    /// keys copad itself binds (Cmd+Shift+P palette, Cmd+T tab, …).
    /// Names are kept consistent with the Linux config schema so a
    /// shared `[keybindings]` block parses identically on both.
    ///
    /// Constants come from `<HIToolbox/Events.h>`; hardcoding them is
    /// stable across macOS versions (Apple's commit to ANSI keycode
    /// values predates OS X) and avoids pulling in `Carbon` solely for
    /// the kVK_* enum.
    ///
    /// **Naming nuances vs Linux**:
    /// - `backspace` → `kVK_Delete` (0x33) — the key labeled Delete on
    ///   mac keyboards. Mirrors gdk `BackSpace`.
    /// - `delete` → `kVK_ForwardDelete` (0x75) — the standalone Forward
    ///   Delete key. Mirrors gdk `Delete`.
    /// - `return` / `enter` both alias 0x24 (gdk uses `Return`).
    static let nameToKeyCode: [String: UInt16] = [
        "a": 0x00, "s": 0x01, "d": 0x02, "f": 0x03, "h": 0x04, "g": 0x05,
        "z": 0x06, "x": 0x07, "c": 0x08, "v": 0x09, "b": 0x0B, "q": 0x0C,
        "w": 0x0D, "e": 0x0E, "r": 0x0F, "y": 0x10, "t": 0x11,
        "o": 0x1F, "u": 0x20, "i": 0x22, "p": 0x23, "l": 0x25, "j": 0x26,
        "k": 0x28, "n": 0x2D, "m": 0x2E,
        "1": 0x12, "2": 0x13, "3": 0x14, "4": 0x15, "5": 0x17,
        "6": 0x16, "7": 0x1A, "8": 0x1C, "9": 0x19, "0": 0x1D,
        "equal": 0x18, "minus": 0x1B,
        "bracketright": 0x1E, "bracketleft": 0x21,
        "apostrophe": 0x27, "semicolon": 0x29,
        "backslash": 0x2A, "comma": 0x2B, "slash": 0x2C, "period": 0x2F,
        "grave": 0x32,
        "return": 0x24, "enter": 0x24,
        "tab": 0x30, "space": 0x31,
        "backspace": 0x33, "delete": 0x75,
        "escape": 0x35, "esc": 0x35,
        "left": 0x7B, "right": 0x7C, "down": 0x7D, "up": 0x7E,
        "f1": 0x7A, "f2": 0x78, "f3": 0x63, "f4": 0x76, "f5": 0x60,
        "f6": 0x61, "f7": 0x62, "f8": 0x64, "f9": 0x65, "f10": 0x6D,
        "f11": 0x67, "f12": 0x6F,
    ]

    /// Parse the raw TOML dict into compiled bindings. Invalid combos
    /// (unknown modifier, empty key) are dropped with a stderr warning so
    /// one typo doesn't disable the whole map.
    static func compile(_ raw: [String: String]) -> [Binding] {
        var out: [Binding] = []
        for (combo, command) in raw {
            guard let binding = parseCombo(combo, command: command) else {
                continue
            }
            out.append(binding)
        }
        return out
    }

    private static func parseCombo(_ combo: String, command: String) -> Binding? {
        let parts = combo.split(separator: "+").map { p in
            p.trimmingCharacters(in: .whitespaces).lowercased()
        }
        guard !parts.isEmpty else { return nil }

        var mods: NSEvent.ModifierFlags = []
        var key: String?
        for part in parts {
            switch part {
            case "cmd", "command", "meta": mods.insert(.command)
            case "ctrl", "control": mods.insert(.control)
            case "shift": mods.insert(.shift)
            case "alt", "option": mods.insert(.option)
            case "":
                continue
            default:
                if key != nil {
                    let msg = "[copad] keybinding '\(combo)' has multiple non-modifier keys (\(key!) and \(part)) — skipping\n"
                    FileHandle.standardError.write(Data(msg.utf8))
                    return nil
                }
                key = part
            }
        }
        guard let key, !key.isEmpty else {
            let msg = "[copad] keybinding '\(combo)' has no key — skipping\n"
            FileHandle.standardError.write(Data(msg.utf8))
            return nil
        }
        return Binding(modifiers: mods, key: key, keyCode: nameToKeyCode[key], command: command)
    }

    /// Compare an NSEvent against a binding. Modifiers must match exactly
    /// (so `cmd+g` doesn't fire on `cmd+shift+g` — that'd be surprising).
    /// When `binding.keyCode` is set (parsed key name was in
    /// `nameToKeyCode`), match on layout-position keyCode — this is
    /// IME-immune. For names outside the table, fall back to the
    /// historical `charactersIgnoringModifiers` comparison so unusual
    /// key names still work (with the IME caveat).
    static func matches(_ event: NSEvent, _ binding: Binding) -> Bool {
        // Mask out caps lock / numpad noise — only the four real modifier
        // flags are part of the binding contract.
        let interesting: NSEvent.ModifierFlags = [.command, .control, .shift, .option]
        let actualMods = event.modifierFlags.intersection(interesting)
        guard actualMods == binding.modifiers else { return false }
        if let bindingKeyCode = binding.keyCode {
            return event.keyCode == bindingKeyCode
        }
        let keyChar = (event.charactersIgnoringModifiers ?? "").lowercased()
        return keyChar == binding.key
    }

    /// Dispatch a binding's command. Called from the NSEvent local monitor
    /// after a match; runs on the main thread so action dispatch can hit
    /// the @MainActor `ActionRegistry` without hopping.
    @MainActor
    static func dispatch(_ binding: Binding, registry: ActionRegistry, socketPath: String) {
        let cmd = binding.command
        if let payload = cmd.stripPrefixIfMatches("spawn:") {
            spawn(payload, socketPath: socketPath)
        } else if let payload = cmd.stripPrefixIfMatches("action:") {
            invokeAction(payload, registry: registry)
        } else {
            let msg = "[copad] keybinding command '\(cmd)' has no spawn:/action: prefix — skipping\n"
            FileHandle.standardError.write(Data(msg.utf8))
        }
    }

    /// `spawn:` handler — mirrors Linux's `spawn_command`. Tilde-expand,
    /// run through `sh -c` so users can use shell features (pipes,
    /// redirects) without quoting headaches, detach (no stdin/out),
    /// inject `COPAD_SOCKET` so the spawned process can call back via
    /// `coctl --socket $COPAD_SOCKET ...`.
    private static func spawn(_ rawCmd: String, socketPath: String) {
        let cmd = rawCmd.replacingOccurrences(of: "~", with: NSHomeDirectory(), options: .anchored)
        let process = Process()
        process.executableURL = URL(fileURLWithPath: "/bin/sh")
        process.arguments = ["-c", cmd]
        var env = ProcessInfo.processInfo.environment
        env["COPAD_SOCKET"] = socketPath
        process.environment = env
        // Detach from our stdio so the child doesn't pipe to our terminal.
        // FileHandle(forUpdatingAtPath: "/dev/null") would be the fully
        // correct equivalent of Linux's Stdio::null, but Process accepts nil
        // and treats it as "inherit", which means the child inherits our
        // stderr — cheap visibility into spawn failures during dev.
        do {
            try process.run()
        } catch {
            let msg = "[copad] keybinding spawn failed for '\(rawCmd)': \(error)\n"
            FileHandle.standardError.write(Data(msg.utf8))
        }
    }

    /// `action:` handler — parses `<method> [k=v ...]` and dispatches via
    /// the registry. Plain string values only; the syntax is intentionally
    /// minimal because keybindings are a quick-launch surface, not a
    /// general-purpose RPC client. For complex calls users should
    /// `spawn:coctl call <method> --params '<json>'` instead.
    @MainActor
    private static func invokeAction(_ tail: String, registry: ActionRegistry) {
        let trimmed = tail.trimmingCharacters(in: .whitespaces)
        let parts = trimmed.split(separator: " ", omittingEmptySubsequences: true)
        guard let methodSub = parts.first else {
            FileHandle.standardError.write(Data("[copad] keybinding action: missing method\n".utf8))
            return
        }
        let method = String(methodSub)
        var params: [String: Any] = [:]
        for kv in parts.dropFirst() {
            let pair = kv.split(separator: "=", maxSplits: 1)
            guard pair.count == 2 else { continue }
            params[String(pair[0])] = String(pair[1])
        }
        registry.tryDispatchOrFallback(method, params: params) { result in
            if let err = result as? RPCError {
                let msg = "[copad] keybinding action \(method) failed: \(err.code) — \(err.message)\n"
                FileHandle.standardError.write(Data(msg.utf8))
            }
        }
    }
}

private extension String {
    /// Like `removingPrefix` but returns nil when the prefix doesn't match —
    /// lets `spawn:`/`action:` dispatch use a single guard ladder.
    func stripPrefixIfMatches(_ prefix: String) -> String? {
        guard hasPrefix(prefix) else { return nil }
        return String(dropFirst(prefix.count))
    }
}
