import AppKit
import Foundation

/// Custom keybindings parsed from `[keybindings]` in `config.toml`, same shape as Linux:
///
/// ```toml
/// [keybindings]
/// "cmd+shift+g" = "spawn:~/scripts/grep-something.sh"
/// "cmd+e" = "action:webview.open url=https://example.com"
/// ```
///
/// - `spawn:<cmd>` runs `<cmd>` via `sh -c` in a detached `Process` with `COPAD_SOCKET` injected.
/// - `action:<method> [k=v ...]` dispatches through `ActionRegistry` with string params.
///
/// Modifiers are `ctrl`/`cmd`/`shift`/`alt`/`option` in any order; the key is the unshifted
/// character. Built-in menu shortcuts (Cmd+T, Cmd+W) fire first, so a binding on one of them is
/// shadowed on purpose.
enum Keybindings {
    struct Binding {
        let modifiers: NSEvent.ModifierFlags
        let key: String // lowercased character, e.g. "g"
        /// Layout-position keyCode resolved from `key` at parse time. `matches` prefers it so
        /// non-Latin IMEs (which turn `p` into `ㅖ`) cannot shadow the binding; nil falls back to
        /// character comparison.
        let keyCode: UInt16?
        let command: String // raw value from config: "spawn:..." or "action:..."
    }

    /// gdk-style key name to macOS `kVK_*` constant, with names consistent with the Linux config
    /// schema. Hardcoded rather than importing `Carbon` for the enum (`<HIToolbox/Events.h>`).
    ///
    /// `backspace` is `kVK_Delete` (0x33, gdk `BackSpace`); `delete` is `kVK_ForwardDelete`
    /// (0x75, gdk `Delete`); `return` and `enter` both alias 0x24.
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

    /// Parse the raw TOML dict into bindings. An invalid combo is dropped with a stderr warning
    /// so one typo does not disable the map.
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

    /// Modifiers must match exactly (`cmd+g` must not fire on `cmd+shift+g`). Uses the
    /// layout-position keyCode when set (immune to IMEs); otherwise falls back to
    /// `charactersIgnoringModifiers`.
    static func matches(_ event: NSEvent, _ binding: Binding) -> Bool {
        // Mask out caps lock / numpad noise.
        let interesting: NSEvent.ModifierFlags = [.command, .control, .shift, .option]
        let actualMods = event.modifierFlags.intersection(interesting)
        guard actualMods == binding.modifiers else { return false }
        if let bindingKeyCode = binding.keyCode {
            return event.keyCode == bindingKeyCode
        }
        let keyChar = (event.charactersIgnoringModifiers ?? "").lowercased()
        return keyChar == binding.key
    }

    /// Dispatch a binding's command on the main thread, where the @MainActor registry lives.
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

    /// `spawn:` handler, mirroring Linux's `spawn_command`: tilde-expands, runs through `sh -c`,
    /// detaches, and injects `COPAD_SOCKET` so the process can call back via `coctl`.
    private static func spawn(_ rawCmd: String, socketPath: String) {
        let cmd = rawCmd.replacingOccurrences(of: "~", with: NSHomeDirectory(), options: .anchored)
        let process = Process()
        process.executableURL = URL(fileURLWithPath: "/bin/sh")
        process.arguments = ["-c", cmd]
        var env = ProcessInfo.processInfo.environment
        env["COPAD_SOCKET"] = socketPath
        process.environment = env
        // Process treats nil stdio as "inherit", so the child shares our stderr, which keeps spawn
        // failures visible.
        do {
            try process.run()
        } catch {
            let msg = "[copad] keybinding spawn failed for '\(rawCmd)': \(error)\n"
            FileHandle.standardError.write(Data(msg.utf8))
        }
    }

    /// `action:` handler: parses `<method> [k=v ...]` with plain string values and dispatches via
    /// the registry. Complex calls should use `spawn:coctl call <method> --params '<json>'`.
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
    /// Like `removingPrefix` but nil when the prefix does not match.
    func stripPrefixIfMatches(_ prefix: String) -> String? {
        guard hasPrefix(prefix) else { return nil }
        return String(dropFirst(prefix.count))
    }
}
