import AppKit
import Foundation

@main
struct CopadApp {
    static func main() {
// Must run before NSApplication.shared so these flags print and exit instead of launching the GUI.
        let args = CommandLine.arguments
        if handleCLIFlags(args) {
            return
        }

        let app = NSApplication.shared
        let delegate = AppDelegate()
        app.delegate = delegate
        app.run()
    }

    private static func handleCLIFlags(_ args: [String]) -> Bool {
        if args.contains("--version") || args.contains("-V") {
            let v = Bundle.main.infoDictionary?["CFBundleShortVersionString"] as? String ?? "dev"
            print("copad \(v)")
            return true
        }

        if args.contains("--config-path") {
            print(CopadConfig.configPath().path)
            return true
        }

        if args.contains("--init-config") {
            return runInitConfig()
        }

        if args.contains("--help") || args.contains("-h") {
            printUsage()
            return true
        }

        return false
    }

    private static func runInitConfig() -> Bool {
        let target = CopadConfig.configPath()
        let fm = FileManager.default

        if fm.fileExists(atPath: target.path) {
            print("Config already exists at: \(target.path)")
            return true
        }

        do {
            try fm.createDirectory(
                at: target.deletingLastPathComponent(),
                withIntermediateDirectories: true,
            )
// Swift `"""` strips the final newline; copad-core's template ends with one.
            try (defaultConfigTOML + "\n").write(to: target, atomically: true, encoding: .utf8)
            print("Config written to: \(target.path)")
        } catch {
            FileHandle.standardError.write(
                Data("Failed to write config: \(error.localizedDescription)\n".utf8),
            )
            exit(1)
        }
        return true
    }

    private static func printUsage() {
        let usage = """
        copad — cross-platform terminal emulator

        Usage:
          copad                 Launch the GUI app.
          copad --version       Print the version.
          copad --config-path   Print the config file path.
          copad --init-config   Write the default config if none exists.
          copad --help          Show this message.

        Companion tools:
          coctl                CLI for a running copad/daemon (sockets, panels, plugins).
          copadd                Background daemon (status bar, triggers, plugin runtime).
        """
        print(usage)
    }

/// Copy of copad-core's `write_default` template, kept in sync by hand.
    private static let defaultConfigTOML = """
    [terminal]
    # shell = "/bin/zsh"
    font_family = "JetBrainsMono Nerd Font Mono"
    font_size = 14

    [background]
    # image = "/path/to/wallpaper.jpg"
    # tint = 0.85
    # tint_color = "#1e1e2e"
    # opacity = 0.95

    [tabs]
    # position = "top"  # top, bottom, left, right
    # width = 120       # vertical tab width in pixels (left/right)
    # collapsed = true  # start with tab bar collapsed (icon-only)

    [theme]
    # Available: catppuccin-mocha, catppuccin-latte, catppuccin-frappe, catppuccin-macchiato,
    #            dracula, nord, tokyo-night, gruvbox-dark, one-dark, solarized-dark
    name = "catppuccin-mocha"

    [statusbar]
    # enabled = true       # Show/hide the status bar
    # position = "bottom"  # "top" or "bottom"
    # height = 28          # Height in pixels

    [keybindings]
    # Map key combos to shell commands (spawn:) — runs in background
    # "ctrl+shift+g" = "spawn:~/my-script.sh --next"
    # "ctrl+shift+m" = "spawn:~/my-script.sh --toggle"

    # [[triggers]]
    # name = "log-cwd"
    # action = "system.log"
    # # Interpolation tokens: {event.<payload-key>} reaches into the event's
    # # JSON payload; if missing there, falls back to {event.kind|source|timestamp_ms}.
    # params = { message = "[{event.timestamp_ms}] cwd: {event.cwd}" }
    # [triggers.when]
    # event_kind = "terminal.cwd_changed"
    """
}
