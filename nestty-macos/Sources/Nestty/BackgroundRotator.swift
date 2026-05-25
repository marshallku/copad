import CNesttyFFI
import Foundation

/// Random wallpaper rotation. Wire-equivalent to Linux's
/// `socket.rs::select_random_image` / `is_bg_active` / `toggle_bg_mode`,
/// both backed by the shared `nestty_core::background` primitives reached
/// through FFI. This file owns macOS-specific path resolution
/// (`~/Library/Caches/nestty/...` primary, `~/.cache/...` XDG fallback);
/// the actual file IO + entropy + write happen in Rust.
enum BackgroundRotator {
    /// macOS-native cache path: `~/Library/Caches/nestty/wallpapers.txt`.
    static var primaryListURL: URL {
        FileManager.default.homeDirectoryForCurrentUser
            .appendingPathComponent("Library/Caches/nestty/wallpapers.txt")
    }

    /// XDG-style fallback so users sharing dotfiles across Linux + macOS
    /// keep one wallpapers list. macOS-native wins on conflict (checked
    /// first inside `nestty_core::background::pick_random`).
    static var fallbackListURL: URL {
        FileManager.default.homeDirectoryForCurrentUser
            .appendingPathComponent(".cache/terminal-wallpapers.txt")
    }

    /// Mode file: `~/Library/Caches/nestty/bg-mode`. Missing = active.
    static var modeFileURL: URL {
        FileManager.default.homeDirectoryForCurrentUser
            .appendingPathComponent("Library/Caches/nestty/bg-mode")
    }

    static var isActive: Bool {
        let rc = modeFileURL.path.withCString { nestty_ffi_background_is_active($0) }
        // -1 (NULL / invalid UTF-8) is treated as active to match the
        // Linux default-on-missing behavior — failing closed here would
        // silently disable rotation if FFI rejected the path.
        return rc != 0
    }

    @discardableResult
    static func toggle() -> Bool {
        let rc = modeFileURL.path.withCString { nestty_ffi_background_toggle($0) }
        return rc == 1
    }

    /// Pick a random wallpaper path. Returns nil if no list exists or
    /// every line is blank. Caller decides whether to suppress on
    /// deactive mode (`background.next` socket handler chooses no-op).
    static func nextRandomImage() -> String? {
        let primary = primaryListURL.path
        let fallback = fallbackListURL.path
        return primary.withCString { primaryPtr in
            fallback.withCString { fallbackPtr in
                guard let cstr = nestty_ffi_background_next_random(primaryPtr, fallbackPtr) else {
                    return nil
                }
                defer { nestty_ffi_free_string(cstr) }
                return String(cString: cstr)
            }
        }
    }
}
