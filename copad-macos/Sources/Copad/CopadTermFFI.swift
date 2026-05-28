import CCopadTerm
import Foundation

/// Thin Swift facade over the copad-term C-ABI staticlib.
///
/// Phase 1 scaffold (see docs/macos-renderer-migration-plan.md): the
/// underlying Rust crate is dependency-free and returns fixture data.
/// Phase 2 will swap in `alacritty_terminal::Term` + a real PTY.
///
/// Memory model:
///
/// - `copad_term_create` returns an opaque handle pointer that this
///   facade boxes into a `Handle` class. The class's `deinit` calls
///   `copad_term_destroy` exactly once.
/// - `snapshot()` returns a `Snapshot` class wrapping the opaque
///   snapshot pointer. Its `deinit` calls `copad_snapshot_destroy`.
/// - `Snapshot.rowRuns(_:)` and `Snapshot.rowUtf8(_:)` hand back
///   borrowed pointers valid until the `Snapshot` is deallocated.
///   Callers must finish using them before the Snapshot goes out of
///   scope (RAII via Swift's ARC).
enum CopadTermFFI {
    /// Static version string. No allocation.
    static func version() -> String {
        guard let cstr = copad_term_version() else { return "<null>" }
        return String(cString: cstr)
    }

    /// Smoke-test the bridge end-to-end: create handle, take snapshot,
    /// walk the fixture row, destroy in reverse order. Asserts the
    /// known fixture shape. Called once during app launch alongside
    /// `CopadFFI.runSmokeTest()`. `@MainActor` because the `Handle`
    /// + `Snapshot` classes are main-actor-isolated.
    @MainActor
    static func runSmokeTest() {
        FileHandle.standardError.write(Data("[copad-term] version = \(version())\n".utf8))
        guard let handle = Handle(cols: 80, rows: 24) else {
            FileHandle.standardError.write(Data("[copad-term] smoke FAIL: handle create returned NULL\n".utf8))
            return
        }
        guard let snap = handle.snapshot() else {
            FileHandle.standardError.write(Data("[copad-term] smoke FAIL: snapshot returned NULL\n".utf8))
            return
        }
        let rows = snap.rows
        let cols = snap.cols
        let runs = snap.rowRuns(0)
        let utf8 = snap.rowUtf8(0)
        let cursor = snap.cursor
        FileHandle.standardError.write(Data("[copad-term] snapshot \(cols)x\(rows), row0: \(runs.count) runs / \(utf8.count) utf8 bytes, cursor at (\(cursor.row),\(cursor.col)) style=\(cursor.style)\n".utf8))
    }

    @MainActor
    final class Handle {
        /// `nonisolated(unsafe)` so deinit (Swift 6 nonisolated) can
        /// free without crossing the actor boundary. Rust destroy has
        /// no thread-affinity.
        private nonisolated(unsafe) var ptr: OpaquePointer?

        init?(cols: UInt16, rows: UInt16, shell: String? = nil, cwd: String? = nil, panelID: String? = nil, socketPath: String? = nil) {
            let p = shell.withCStringOrNull { shellPtr in
                cwd.withCStringOrNull { cwdPtr in
                    panelID.withCStringOrNull { panelPtr in
                        socketPath.withCStringOrNull { socketPtr in
                            copad_term_create(cols, rows, shellPtr, cwdPtr, panelPtr, socketPtr)
                        }
                    }
                }
            }
            guard let p else { return nil }
            ptr = p
        }

        deinit {
            if let p = ptr {
                copad_term_destroy(p)
            }
        }

        func resize(cols: UInt16, rows: UInt16) {
            guard let ptr else { return }
            copad_term_resize(ptr, cols, rows)
        }

        func input(_ bytes: [UInt8]) {
            guard let ptr else { return }
            bytes.withUnsafeBufferPointer { buf in
                copad_term_input(ptr, buf.baseAddress, buf.count)
            }
        }

        /// Push one entry into the OSC color-query palette consulted by
        /// `Event::ColorRequest`. Index follows `vte::ansi::NamedColor`:
        /// 0-15 ANSI, 256 = foreground, 257 = background, 258 = cursor.
        /// Call from `applyTheme` so an OSC 4/10/11/12 query gets the
        /// color we actually render.
        func setPaletteEntry(_ index: UInt16, _ rgb: RGBColor) {
            guard let ptr else { return }
            _ = copad_term_set_palette_entry(ptr, index, rgb.r, rgb.g, rgb.b)
        }

        /// Bulk push the standard subset (16 ANSI + foreground +
        /// background + cursor). Cursor uses `theme.accent` because
        /// CopadTheme doesn't have a dedicated cursor color — same
        /// choice the alacritty renderer makes for cursor block fill.
        func applyPaletteFromTheme(_ theme: CopadTheme) {
            for (i, color) in theme.palette.enumerated() where i < 16 {
                setPaletteEntry(UInt16(i), color)
            }
            // NamedColor::Foreground = 256, Background = 257, Cursor = 258
            setPaletteEntry(256, theme.foreground)
            setPaletteEntry(257, theme.background)
            setPaletteEntry(258, theme.accent)
        }

        func snapshot() -> Snapshot? {
            guard let ptr, let snap = copad_term_snapshot(ptr) else { return nil }
            return Snapshot(ptr: snap)
        }

        /// True if the terminal grid has any damage since the previous
        /// call (and resets the internal damage state). False means
        /// nothing changed — renderer can skip the snapshot + draw.
        func takeDamage() -> Bool {
            guard let ptr else { return false }
            return copad_term_take_damage(ptr)
        }

        // MARK: - Selection

        enum SelectionKind: UInt8 {
            case simple = 0 // COPAD_SELECTION_SIMPLE
            case word = 1 // COPAD_SELECTION_SEMANTIC
            case line = 2 // COPAD_SELECTION_LINES
            case block = 3 // COPAD_SELECTION_BLOCK — rectangular (Option+drag, native macOS gesture)
        }

        enum CellSide: UInt8 {
            case left = 0 // COPAD_SIDE_LEFT
            case right = 1 // COPAD_SIDE_RIGHT
        }

        func selectionStart(row: UInt16, col: UInt16, side: CellSide, kind: SelectionKind) {
            guard let ptr else { return }
            copad_term_selection_start(ptr, row, col, side.rawValue, kind.rawValue)
        }

        func selectionUpdate(row: UInt16, col: UInt16, side: CellSide) {
            guard let ptr else { return }
            copad_term_selection_update(ptr, row, col, side.rawValue)
        }

        func selectionClear() {
            guard let ptr else { return }
            copad_term_selection_clear(ptr)
        }

        func selectionAll() {
            guard let ptr else { return }
            copad_term_selection_all(ptr)
        }

        /// Returns the current selection as a Swift string (or nil if
        /// nothing selected). Lifetime of the underlying Rust buffer
        /// is bounded by this call — we copy the bytes into a Swift
        /// String and free the buffer before returning.
        func selectionString() -> String? {
            guard let ptr else { return nil }
            guard let sPtr = copad_term_selection_string(ptr) else { return nil }
            defer { copad_string_destroy(sPtr) }
            var len = 0
            guard let bytes = copad_string_bytes(sPtr, &len), len > 0 else { return nil }
            let buf = UnsafeBufferPointer(start: bytes, count: len)
            return String(bytes: buf, encoding: .utf8)
        }

        /// Last `lines` rows of scrollback as plain text — `\n` between
        /// rows, no trailing newline. NUL cells render as space.
        ///
        /// Returns `""` when no scrollback exists or `lines == 0` (the
        /// FFI emits a non-NULL empty CopadString for these cases, so
        /// the JSON shape from `terminal.history` stays stable). Returns
        /// `nil` only when the handle is gone (`ptr == nil`) or the FFI
        /// returned NULL (defensive — shouldn't happen for a live
        /// handle).
        func history(lines: Int) -> String? {
            guard let ptr else { return nil }
            let count = lines < 0 ? 0 : lines
            guard let sPtr = copad_term_history(ptr, count) else { return nil }
            defer { copad_string_destroy(sPtr) }
            var len = 0
            guard let bytes = copad_string_bytes(sPtr, &len) else { return "" }
            if len == 0 { return "" }
            let buf = UnsafeBufferPointer(start: bytes, count: len)
            return String(bytes: buf, encoding: .utf8) ?? ""
        }

        /// True if any of alacritty's mouse-reporting modes is on
        /// (vim's `set mouse=a`, less, htop, etc.). Renderer uses this
        /// to leave mouse drag alone unless the user holds Shift.
        var mouseModeActive: Bool {
            guard let ptr else { return false }
            return copad_term_mouse_mode_active(ptr)
        }

        /// Encoding the TUI negotiated for forwarded mouse events. SGR
        /// is what tmux / nvim / iTerm2 default to; LEGACY (X10) and
        /// UTF8 are fallbacks for older programs. NONE = don't forward.
        enum MouseEncoding: UInt8 {
            case none = 0
            case legacy = 1
            case sgr = 2
            case utf8 = 3
        }

        var mouseEncoding: MouseEncoding {
            guard let ptr else { return .none }
            return MouseEncoding(rawValue: copad_term_mouse_encoding(ptr)) ?? .none
        }

        /// Highest mouse-reporting level negotiated by the TUI. Tiers
        /// stack: `.motion` ⊇ `.drag` ⊇ `.click`. Renderer uses this
        /// to gate event forwarding — press/release at any level,
        /// drag-with-button-held at `.drag`+, bare-motion at `.motion`.
        enum MouseReportLevel: UInt8 {
            case none = 0
            case click = 1
            case drag = 2
            case motion = 3
        }

        var mouseReportLevel: MouseReportLevel {
            guard let ptr else { return .none }
            return MouseReportLevel(rawValue: copad_term_mouse_report_level(ptr)) ?? .none
        }

        /// True when the terminal has `\e[?2004h` bracketed paste on.
        var bracketedPasteActive: Bool {
            guard let ptr else { return false }
            return copad_term_bracketed_paste_active(ptr)
        }

        /// Scroll the visible viewport. `lines` is positive to scroll
        /// older content into view (toward the top of history) and
        /// negative to scroll back toward the live bottom.
        func scrollLines(_ lines: Int32) {
            guard let ptr else { return }
            copad_term_scroll(ptr, UInt8(COPAD_SCROLL_DELTA), lines)
        }

        func scrollPageUp() {
            guard let ptr else { return }
            copad_term_scroll(ptr, UInt8(COPAD_SCROLL_PAGE_UP), 0)
        }

        func scrollPageDown() {
            guard let ptr else { return }
            copad_term_scroll(ptr, UInt8(COPAD_SCROLL_PAGE_DOWN), 0)
        }

        func scrollToTop() {
            guard let ptr else { return }
            copad_term_scroll(ptr, UInt8(COPAD_SCROLL_TOP), 0)
        }

        func scrollToBottom() {
            guard let ptr else { return }
            copad_term_scroll(ptr, UInt8(COPAD_SCROLL_BOTTOM), 0)
        }

        /// Current working directory of the PTY's child process,
        /// queried at call time via `proc_pidinfo` (macOS only). Returns
        /// nil when the shell has exited or the syscall failed. Cheap
        /// enough to call on every session-snapshot; no caching needed.
        func childCwd() -> String? {
            guard let ptr else { return nil }
            guard let sPtr = copad_term_child_cwd(ptr) else { return nil }
            defer { copad_string_destroy(sPtr) }
            var len = 0
            guard let bytes = copad_string_bytes(sPtr, &len), len > 0 else { return nil }
            let buf = UnsafeBufferPointer(start: bytes, count: len)
            return String(bytes: buf, encoding: .utf8)
        }

        /// Take the latest pending OSC 52 clipboard write request as a
        /// Swift string, or nil if nothing is pending. The renderer
        /// gates the actual NSPasteboard write on the user's policy.
        /// An empty-string write IS a valid OSC 52 request ("clear
        /// clipboard") and should reach the gate, so we don't filter
        /// on length here — only the Rust-side NULL "nothing pending"
        /// sentinel maps to Swift nil.
        func takeClipboardRequest() -> String? {
            guard let ptr else { return nil }
            guard let sPtr = copad_term_take_clipboard_request(ptr) else { return nil }
            defer { copad_string_destroy(sPtr) }
            var len = 0
            guard let bytes = copad_string_bytes(sPtr, &len) else { return "" }
            let buf = UnsafeBufferPointer(start: bytes, count: len)
            return String(bytes: buf, encoding: .utf8) ?? ""
        }
    }

    /// Wraps a Rust-owned snapshot pointer. Borrowed runs/utf8 are
    /// valid for the lifetime of this Snapshot object.
    @MainActor
    final class Snapshot {
        private nonisolated(unsafe) var ptr: OpaquePointer?

        fileprivate init(ptr: OpaquePointer) {
            self.ptr = ptr
        }

        deinit {
            if let p = ptr {
                copad_snapshot_destroy(p)
            }
        }

        var rows: UInt16 {
            guard let ptr else { return 0 }
            return copad_snapshot_rows(ptr)
        }

        var cols: UInt16 {
            guard let ptr else { return 0 }
            return copad_snapshot_cols(ptr)
        }

        /// Borrowed view into the row's run array. Buffer is valid
        /// only while this Snapshot is alive; copy out anything you
        /// need to keep.
        func rowRuns(_ row: UInt16) -> UnsafeBufferPointer<CopadRun> {
            guard let ptr else { return UnsafeBufferPointer(start: nil, count: 0) }
            var runsPtr: UnsafePointer<CopadRun>?
            let n = copad_snapshot_row_runs(ptr, row, &runsPtr)
            return UnsafeBufferPointer(start: runsPtr, count: n)
        }

        /// Borrowed view into the row's utf8 bytes. Same lifetime.
        func rowUtf8(_ row: UInt16) -> UnsafeBufferPointer<UInt8> {
            guard let ptr else { return UnsafeBufferPointer(start: nil, count: 0) }
            var len = 0
            let bytesPtr = copad_snapshot_row_utf8(ptr, row, &len)
            return UnsafeBufferPointer(start: bytesPtr, count: len)
        }

        var cursor: CopadCursor {
            guard let ptr else {
                return CopadCursor(row: 0, col: 0, style: 0, blink: 0, reserved: 0)
            }
            var out = CopadCursor(row: 0, col: 0, style: 0, blink: 0, reserved: 0)
            copad_snapshot_cursor(ptr, &out)
            return out
        }

        var selection: CopadSelectionRange {
            let empty = CopadSelectionRange(
                start_row: 0, start_col: 0, end_row: 0, end_col: 0,
                is_block: 0, present: 0, reserved: 0,
            )
            guard let ptr else { return empty }
            var out = empty
            copad_snapshot_selection(ptr, &out)
            return out
        }

        /// OSC 8 URI for the given 1-based hyperlink id (the value
        /// `CopadRun.hyperlink_id` carries). Returns nil for id == 0
        /// or an out-of-range id. Bytes are copied out before the
        /// snapshot's storage is reused.
        func hyperlinkURI(_ hyperlinkID: UInt32) -> String? {
            guard let ptr, hyperlinkID > 0 else { return nil }
            var len = 0
            guard let bytes = copad_snapshot_hyperlink_uri(ptr, hyperlinkID, &len), len > 0
            else { return nil }
            let buf = UnsafeBufferPointer(start: bytes, count: len)
            return String(bytes: buf, encoding: .utf8)
        }
    }
}

private extension String? {
    /// Run `body` with a borrowed C string (or NULL if self is nil).
    /// Used for optional `shell` / `cwd` parameters.
    func withCStringOrNull<R>(_ body: (UnsafePointer<CChar>?) -> R) -> R {
        switch self {
        case .none: body(nil)
        case let .some(str): str.withCString { body($0) }
        }
    }
}
