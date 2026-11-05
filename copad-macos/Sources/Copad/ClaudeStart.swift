import AppKit
import Foundation

/// `claude.start` socket action, mirroring `copad-linux/src/socket.rs::handle_claude_start`.
/// Spawns a tab at `workspace_path` running `tmux new-session -A -s <name> 'claude [--resume
/// <id>]'`. An optional `prompt` is pasted into the REPL via tmux load-buffer/paste-buffer.
/// `prompt` and `resume_session` are mutually exclusive: seeding text on top of a resumed
/// context would confuse claude.
///
/// `new-session -A` attaches if the session already exists and ignores the trailing `claude`
/// command, so a pre-existing shell session of the same name would execute the prompt as shell
/// input. Both gates must pass before pasting:
///
/// 1. `tmux capture-pane` contains claude-specific markers (`Anthropic`, `Try "`, `claude --`,
///    "claude code"). Generic `> ` or box-drawing markers are not enough; shells emit those.
/// 2. `#{pane_current_command}` is `claude` or `node`.
///
/// On failure it logs to stderr and does not paste; `claude.start` has already replied.
enum ClaudeStart {
    // MARK: - Public entry — dispatched from AppDelegate.handleCommand

    /// Validates params, builds the tmux command, opens a terminal tab with `cwd` +
    /// `initialInput`, and starts the prompt seeder if a prompt was given. The reply matches Linux
    /// byte-for-byte: `{panel_id, tab, tmux_session, workspace_path}`.
    @MainActor
    static func dispatch(
        params: [String: Any],
        tabVC: TabViewController?,
        completion: @escaping (Any?) -> Void,
    ) {
        guard let tabVC else {
            completion(RPCError(code: "internal_error", message: "claude.start: TabViewController missing"))
            return
        }

        guard let raw = params["workspace_path"] as? String, !raw.isEmpty else {
            if params["workspace_path"] != nil {
                completion(RPCError(code: "invalid_params", message: "'workspace_path' must be a non-empty string"))
            } else {
                completion(RPCError(code: "invalid_params", message: "Missing 'workspace_path' param"))
            }
            return
        }
        let canonURL: URL
        do {
            // Not realpath(3): `.isDirectory` is checked separately so a symlink to a file is
            // `invalid_params`, as on Linux.
            let resolved = URL(fileURLWithPath: raw).resolvingSymlinksInPath().standardizedFileURL
            var isDir: ObjCBool = false
            guard FileManager.default.fileExists(atPath: resolved.path, isDirectory: &isDir) else {
                completion(RPCError(code: "not_found", message: "workspace_path \(raw): no such file or directory"))
                return
            }
            if !isDir.boolValue {
                completion(RPCError(code: "invalid_params", message: "workspace_path \(resolved.path) is not a directory"))
                return
            }
            canonURL = resolved
        }

        let sessionName: String
        switch params["session_name"] {
        case let s as String where !s.isEmpty:
            if let err = validateTmuxSessionName(s) {
                completion(RPCError(code: "invalid_params", message: "session_name: \(err)"))
                return
            }
            sessionName = s
        case nil, is NSNull:
            sessionName = deriveSessionName(from: canonURL)
        case let other:
            completion(RPCError(
                code: "invalid_params",
                message: "'session_name' must be a string, got \(type(of: other))",
            ))
            return
        }

        let resumeSession: String?
        switch params["resume_session"] {
        case let s as String where !s.isEmpty:
            for scalar in s.unicodeScalars {
                if scalar.value < 0x20 || scalar.value == 0x7F {
                    completion(RPCError(
                        code: "invalid_params",
                        message: "resume_session contains control characters",
                    ))
                    return
                }
            }
            resumeSession = s
        case nil, is NSNull, is String:
            resumeSession = nil
        case let other:
            completion(RPCError(
                code: "invalid_params",
                message: "'resume_session' must be a string, got \(type(of: other))",
            ))
            return
        }

        let promptToSeed: String?
        switch params["prompt"] {
        case let s as String where !s.isEmpty:
            promptToSeed = s
        case nil, is NSNull, is String:
            promptToSeed = nil
        case let other:
            completion(RPCError(
                code: "invalid_params",
                message: "'prompt' must be a string, got \(type(of: other))",
            ))
            return
        }
        if promptToSeed != nil, resumeSession != nil {
            completion(RPCError(
                code: "invalid_params",
                message: "'prompt' and 'resume_session' are mutually exclusive — "
                    + "resume restores existing context; prompt seeds a new conversation",
            ))
            return
        }

        let claudeCmd = if let id = resumeSession {
            "claude --resume \(shellSingleQuote(id))"
        } else {
            "claude"
        }
        let tmuxCommand = "tmux new-session -A -s \(shellSingleQuote(sessionName)) "
            + "\(shellSingleQuote(claudeCmd))\n"

        let (panelID, tabIndex) = tabVC.newTerminalTab(
            cwd: canonURL.path,
            initialInput: tmuxCommand,
        )

        if let prompt = promptToSeed {
            spawnPromptSeeder(sessionName: sessionName, prompt: prompt)
        }

        completion([
            "panel_id": panelID,
            "tab": tabIndex,
            "tmux_session": sessionName,
            "workspace_path": canonURL.path,
        ])
    }

    // MARK: - Helpers (1:1 ports of Linux versions)

    /// POSIX-safe single-quote escape: wrap in `'...'`, replacing embedded `'` with `'\''`.
    static func shellSingleQuote(_ s: String) -> String {
        var out = "'"
        out.reserveCapacity(s.count + 2)
        for c in s {
            if c == "'" {
                out += "'\\''"
            } else {
                out.append(c)
            }
        }
        out.append("'")
        return out
    }

    /// Last 1-2 path components joined with `-`, lowercased and sanitized. Two, because
    /// `<worktree_root>/feature/foo` would otherwise collapse to `foo` and collide with siblings.
    static func deriveSessionName(from url: URL) -> String {
        // pathComponents includes "/" first (and a trailing "/" for directories); drop both.
        let parts = url.pathComponents.filter { $0 != "/" && !$0.isEmpty }
        let tail = parts.suffix(2)
        let joined = tail.joined(separator: "-")
        return sanitizeSessionName(joined)
    }

    /// ASCII alphanumerics and `-_`, lowercased; anything else becomes `-`, trimmed at the ends.
    /// An empty result falls back to `"claude"`.
    static func sanitizeSessionName(_ s: String) -> String {
        var out = ""
        out.reserveCapacity(s.count)
        for scalar in s.unicodeScalars {
            let ascii = scalar.isASCII ? Character(scalar) : "-"
            if ascii.isASCII, ascii.isLetter || ascii.isNumber || ascii == "-" || ascii == "_" {
                out.append(Character(ascii.lowercased()))
            } else {
                out.append("-")
            }
        }
        let trimmed = out.trimmingCharacters(in: CharacterSet(charactersIn: "-"))
        return trimmed.isEmpty ? "claude" : trimmed
    }

    /// Nil on a valid name, the error message otherwise.
    static func validateTmuxSessionName(_ s: String) -> String? {
        if s.isEmpty { return "cannot be empty" }
        if s.hasPrefix("-") { return "cannot start with '-' (would look like a flag)" }
        for c in s {
            let isASCIIAlphanum = c.isASCII && (c.isLetter || c.isNumber)
            if !isASCIIAlphanum, c != "-", c != "_" {
                return "invalid character '\(c)' (allowed: ASCII alphanumeric and - _)"
            }
        }
        return nil
    }

    // MARK: - Prompt seeder (background thread)

    /// Waits for both readiness gates, then delivers `prompt` via tmux load-buffer/paste-buffer
    /// and Enter. Failures log to stderr and never propagate. See the file header.
    static func spawnPromptSeeder(sessionName: String, prompt: String) {
        DispatchQueue.global(qos: .userInitiated).async {
            Thread.sleep(forTimeInterval: 0.4)

            let deadline = Date().addingTimeInterval(8.0)
            var sawClaudeMarker = false
            while Date() < deadline {
                if let captured = runTmux(["capture-pane", "-p", "-t", sessionName]) {
                    let lower = captured.lowercased()
                    if captured.contains("Anthropic")
                        || captured.contains("Try \"")
                        || captured.contains("claude --")
                        || lower.contains("claude code")
                    {
                        sawClaudeMarker = true
                        break
                    }
                }
                Thread.sleep(forTimeInterval: 0.2)
            }

            // Kernel-side foreground command; survives claude's banner scrolling off.
            let rawCmd = runTmux([
                "display-message", "-p", "-t", sessionName, "#{pane_current_command}",
            ]) ?? ""
            let currentCmd = rawCmd
                .trimmingCharacters(in: .whitespacesAndNewlines)
                .lowercased()
            let paneIsClaude = currentCmd == "claude" || currentCmd == "node"

            guard sawClaudeMarker, paneIsClaude else {
                let msg = "[claude.start] refusing to paste prompt into session "
                    + "\(sessionName.debugDescription): saw_claude_marker=\(sawClaudeMarker), "
                    + "pane_current_command=\(currentCmd.debugDescription). "
                    + "Pre-existing tmux session may be a shell or a non-claude process; "
                    + "user can paste the prompt manually.\n"
                FileHandle.standardError.write(Data(msg.utf8))
                return
            }

            // Going through a buffer keeps multi-line and special-character prompts safe.
            let tmpURL = FileManager.default.temporaryDirectory.appendingPathComponent(
                "copad-claude-\(UUID().uuidString).txt",
            )
            do {
                try prompt.data(using: .utf8)?.write(to: tmpURL, options: .atomic)
            } catch {
                FileHandle.standardError.write(Data(
                    "[claude.start] tempfile failed: \(error)\n".utf8,
                ))
                return
            }
            defer { try? FileManager.default.removeItem(at: tmpURL) }

            let bufName = "copad-claude-\(UUID().uuidString)"
            guard runTmuxStatus(["load-buffer", "-b", bufName, tmpURL.path]) else {
                FileHandle.standardError.write(Data(
                    "[claude.start] tmux load-buffer failed\n".utf8,
                ))
                return
            }
            // `-p` pastes as bracketed paste, so a multi-line prompt arrives as one paste instead
            // of one user turn per `\n`. `-d` drops the buffer afterwards.
            guard runTmuxStatus(["paste-buffer", "-t", sessionName, "-b", bufName, "-p", "-d"]) else {
                FileHandle.standardError.write(Data(
                    "[claude.start] tmux paste-buffer failed\n".utf8,
                ))
                return
            }

            // Long pastes collapse to `[Pasted text #1 +N lines]`: the first Enter expands the
            // placeholder and the second submits. A short paste is submitted by the first, and the
            // second is a no-op on an empty input.
            _ = runTmuxStatus(["send-keys", "-t", sessionName, "Enter"])
            Thread.sleep(forTimeInterval: 0.2)
            _ = runTmuxStatus(["send-keys", "-t", sessionName, "Enter"])
        }
    }

    /// Run `tmux <args>` and return stdout; nil on launch error or non-zero exit. Resolved via
    /// `/usr/bin/env` so a Homebrew tmux works without a hard-coded path.
    private static func runTmux(_ args: [String]) -> String? {
        let process = Process()
        process.executableURL = URL(fileURLWithPath: "/usr/bin/env")
        process.arguments = ["tmux"] + args
        let pipe = Pipe()
        process.standardOutput = pipe
        process.standardError = Pipe() // discard
        do {
            try process.run()
        } catch {
            return nil
        }
        process.waitUntilExit()
        let data = pipe.fileHandleForReading.readDataToEndOfFile()
        guard process.terminationStatus == 0 else { return nil }
        return String(data: data, encoding: .utf8)
    }

    /// Like `runTmux` but reports only success; for load-buffer / paste-buffer / send-keys.
    private static func runTmuxStatus(_ args: [String]) -> Bool {
        let process = Process()
        process.executableURL = URL(fileURLWithPath: "/usr/bin/env")
        process.arguments = ["tmux"] + args
        process.standardOutput = Pipe()
        process.standardError = Pipe()
        do {
            try process.run()
        } catch {
            return false
        }
        process.waitUntilExit()
        return process.terminationStatus == 0
    }
}
