import CCopadFFI
import Foundation

/// Random wallpaper rotation. Wire-equivalent to Linux's
/// `socket.rs::select_random_image` / `is_bg_active` / `toggle_bg_mode`,
/// both backed by the shared `copad_core::background` primitives reached
/// through FFI. This file owns macOS-specific path resolution
/// (`~/Library/Caches/copad/...` primary, `~/.cache/...` XDG fallback);
/// the actual file IO + entropy + write happen in Rust.
enum BackgroundRotator {
    /// macOS-native cache path: `~/Library/Caches/copad/wallpapers.txt`.
    static var primaryListURL: URL {
        FileManager.default.homeDirectoryForCurrentUser
            .appendingPathComponent("Library/Caches/copad/wallpapers.txt")
    }

    /// XDG-style fallback so users sharing dotfiles across Linux + macOS
    /// keep one wallpapers list. macOS-native wins on conflict (checked
    /// first inside `copad_core::background::pick_random`).
    static var fallbackListURL: URL {
        FileManager.default.homeDirectoryForCurrentUser
            .appendingPathComponent(".cache/terminal-wallpapers.txt")
    }

    /// Mode file: `~/Library/Caches/copad/bg-mode`. Missing = active.
    static var modeFileURL: URL {
        FileManager.default.homeDirectoryForCurrentUser
            .appendingPathComponent("Library/Caches/copad/bg-mode")
    }

    static var isActive: Bool {
        let rc = modeFileURL.path.withCString { copad_ffi_background_is_active($0) }
        // -1 (NULL / invalid UTF-8) is treated as active to match the
        // Linux default-on-missing behavior — failing closed here would
        // silently disable rotation if FFI rejected the path.
        return rc != 0
    }

    @discardableResult
    static func toggle() -> Bool {
        let rc = modeFileURL.path.withCString { copad_ffi_background_toggle($0) }
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
                guard let cstr = copad_ffi_background_next_random(primaryPtr, fallbackPtr) else {
                    return nil
                }
                defer { copad_ffi_free_string(cstr) }
                return String(cString: cstr)
            }
        }
    }
}
