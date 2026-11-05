import CCopadFFI
import Foundation

/// Random wallpaper rotation over the shared `copad_core::background` primitives; this file owns
/// the macOS path resolution.
enum BackgroundRotator {
    static var primaryListURL: URL {
        FileManager.default.homeDirectoryForCurrentUser
            .appendingPathComponent("Library/Caches/copad/wallpapers.txt")
    }

/// XDG-style fallback so dotfiles shared with Linux keep one list; the macOS-native list wins.
    static var fallbackListURL: URL {
        FileManager.default.homeDirectoryForCurrentUser
            .appendingPathComponent(".cache/terminal-wallpapers.txt")
    }

    static var modeFileURL: URL {
        FileManager.default.homeDirectoryForCurrentUser
            .appendingPathComponent("Library/Caches/copad/bg-mode")
    }

    static var isActive: Bool {
        let rc = modeFileURL.path.withCString { copad_ffi_background_is_active($0) }
// -1 (NULL / invalid UTF-8) counts as active, as on Linux; failing closed would silently
// disable rotation if FFI rejected the path.
        return rc != 0
    }

    @discardableResult
    static func toggle() -> Bool {
        let rc = modeFileURL.path.withCString { copad_ffi_background_toggle($0) }
        return rc == 1
    }

/// Delete `entry` from both the macOS-native and XDG-fallback lists, since a rotation pick can
/// come from either. A missing entry is a no-op; returns true if any list changed.
    @discardableResult
    static func removeFromList(_ entry: String) -> Bool {
        var removed = false
        for list in [primaryListURL.path, fallbackListURL.path] {
            let rc = list.withCString { listPtr in
                entry.withCString { entryPtr in
                    copad_ffi_background_remove_from_list(listPtr, entryPtr)
                }
            }
            if rc == 1 { removed = true }
        }
        return removed
    }

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
