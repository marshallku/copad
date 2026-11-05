import CCopadTerm
import Foundation

/// Thin Swift facade over the copad-term C-ABI staticlib.
///
/// `Handle` boxes the opaque `copad_term_create` pointer; its `deinit` calls
/// `copad_term_destroy` exactly once. `Snapshot` wraps the snapshot pointer and calls
/// `copad_snapshot_destroy`. `rowRuns(_:)` / `rowUtf8(_:)` return borrowed pointers valid only
/// until the `Snapshot` is deallocated.
enum CopadTermFFI {
    static func version() -> String {
        guard let cstr = copad_term_version() else { return "<null>" }
        return String(cString: cstr)
    }

    /// Smoke-test the bridge end to end, called once at launch alongside
    /// `CopadFFI.runSmokeTest()`. `@MainActor` because `Handle` and `Snapshot` are main-actor
    /// isolated.
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
        /// `nonisolated(unsafe)` so deinit can free without crossing the actor boundary; Rust
        /// destroy has no thread affinity.
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

        /// Push one entry into the OSC color-query palette behind `Event::ColorRequest`. Index
        /// follows `vte::ansi::NamedColor`: 0-15 ANSI, 256 foreground, 257 background, 258
        /// cursor. Call from `applyTheme` so OSC 4/10/11/12 queries get the rendered color.
        func setPaletteEntry(_ index: UInt16, _ rgb: RGBColor) {
            guard let ptr else { return }
            _ = copad_term_set_palette_entry(ptr, index, rgb.r, rgb.g, rgb.b)
        }

        /// Bulk push of the standard subset (16 ANSI + foreground + background + cursor).
        /// CopadTheme has no cursor color, so the cursor uses `theme.accent`, as the renderer does.
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

        /// True if the grid has damage since the previous call (resets the damage state). A thin
        /// wrapper over `takeDamageRows`: pick ONE of the two per frame, since calling both
        /// advances the FFI's prev-state twice.
        func takeDamage() -> Bool {
            guard let ptr else { return false }
            return copad_term_take_damage(ptr)
        }

        /// `.full` means redraw everything (display_offset shifted, alacritty signaled Full, or
        /// the dirty count exceeded the cap); `.rows` has distinct viewport rows in unspecified
        /// order, empty meaning nothing changed.
        enum DamageResult {
            case rows([UInt16])
            case full
        }

        /// Per-row damage drain into a `cap`-row buffer. Resets alacritty's damage state on
        /// every call.
        func takeDamageRows(cap: UInt16 = 256) -> DamageResult {
            guard let ptr else { return .rows([]) }
            // `cap == 0` is "drain-and-promote-to-Full" on the Rust side: no rows come back but
            // alacritty's damage state is consumed. Pass a null buffer explicitly, since a
            // 0-element ContiguousArray has no baseAddress and would skip the FFI altogether.
            if cap == 0 {
                _ = copad_term_take_damage_rows(ptr, nil, 0)
                return .full
            }
            // `cap` is a u16 in the FFI. ContiguousArray avoids Array COW noise per call.
            var buf = ContiguousArray<UInt16>(repeating: 0, count: Int(cap))
            let result = buf.withUnsafeMutableBufferPointer { bp -> Int32 in
                guard let base = bp.baseAddress else { return -1 }
                return copad_term_take_damage_rows(ptr, base, cap)
            }
            if result < 0 {
                return .full
            }
            let count = Int(result)
            if count == 0 {
                return .rows([])
            }
            return .rows(Array(buf.prefix(count)))
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

        // MARK: - Find

        /// Find the next (or previous with `forward = false`) match across grid and scrollback,
        /// scrolling to reveal it. The compiled regex is cached per `(pattern, caseSensitive)`.
        @discardableResult
        func searchNext(pattern: String, caseSensitive: Bool, forward: Bool = true) -> Bool {
            guard let ptr, !pattern.isEmpty else { return false }
            return pattern.withCString { cstr in
                copad_term_search_next(ptr, cstr, caseSensitive, forward)
            }
        }

        func searchClear() {
            guard let ptr else { return }
            copad_term_search_clear(ptr)
        }

        // MARK: - Child exit

        /// True iff the PTY child exited since the last call; the latch clears on read.
        /// AlacrittyTerminalViewController polls it and broadcasts `panel.exited` so
        /// ContextService can clean per-panel state.
        func takeChildExit() -> Bool {
            guard let ptr else { return false }
            return copad_term_take_child_exit(ptr)
        }

        /// The selection as a Swift string (nil if none); the Rust buffer is freed before return.
        func selectionString() -> String? {
            guard let ptr else { return nil }
            guard let sPtr = copad_term_selection_string(ptr) else { return nil }
            defer { copad_string_destroy(sPtr) }
            var len = 0
            guard let bytes = copad_string_bytes(sPtr, &len), len > 0 else { return nil }
            let buf = UnsafeBufferPointer(start: bytes, count: len)
            return String(bytes: buf, encoding: .utf8)
        }

        /// Last `lines` rows of scrollback as plain text. `""` when there is no scrollback or
        /// `lines == 0` (the FFI emits a non-NULL empty string so the `terminal.history` JSON
        /// shape stays stable); `nil` only when the handle is gone or the FFI returned NULL.
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

        var mouseModeActive: Bool {
            guard let ptr else { return false }
            return copad_term_mouse_mode_active(ptr)
        }

        /// Encoding the TUI negotiated for forwarded mouse events; NONE = don't forward.
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

        /// Highest mouse-reporting level negotiated; tiers stack `.motion` > `.drag` > `.click`.
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

        /// kitty keyboard protocol flags the program negotiated (0 = legacy keys).
        var keyboardFlags: UInt8 {
            guard let ptr else { return 0 }
            return copad_term_keyboard_flags(ptr)
        }

        func setKittyKeyboard(_ enabled: Bool) {
            guard let ptr else { return }
            copad_term_set_kitty_keyboard(ptr, enabled)
        }

        func setScrollback(_ lines: Int) {
            guard let ptr else { return }
            copad_term_set_scrollback(ptr, UInt32(clamping: max(0, lines)))
        }

        func setFocused(_ focused: Bool) {
            guard let ptr else { return }
            copad_term_set_focused(ptr, focused)
        }

        func setImages(_ enabled: Bool) {
            guard let ptr else { return }
            copad_term_set_images(ptr, enabled)
        }

        func setCellPixels(width: Int, height: Int) {
            guard let ptr, width > 0, height > 0 else { return }
            copad_term_set_cell_pixels(ptr, UInt32(clamping: width), UInt32(clamping: height))
        }

        enum KeyEncoding {
            /// Send the event's text the normal way (IME, insertText).
            case text
            case bytes([UInt8])
            case nothing
        }

        /// Encode one key event for the kitty keyboard protocol. `.text` whenever the
        /// protocol is off, so callers can try this first and fall back.
        func encodeKey(
            keyCode: UInt16, mods: UInt8, action: UInt8,
            unmodified: String, shifted: String, text: String
        ) -> KeyEncoding {
            guard let ptr else { return .text }
            var out = [UInt8](repeating: 0, count: 256)
            var outLen = 0
            let u = Array(unmodified.utf8), s = Array(shifted.utf8), t = Array(text.utf8)
            let code = u.withUnsafeBufferPointer { ub in
                s.withUnsafeBufferPointer { sb in
                    t.withUnsafeBufferPointer { tb in
                        var input = CopadKeyInput(
                            key_code: keyCode, mods: mods, action: action,
                            unmodified: ub.baseAddress, unmodified_len: ub.count,
                            shifted: sb.baseAddress, shifted_len: sb.count,
                            text: tb.baseAddress, text_len: tb.count
                        )
                        return out.withUnsafeMutableBufferPointer { ob in
                            copad_term_encode_key(ptr, &input, ob.baseAddress, ob.count, &outLen)
                        }
                    }
                }
            }
            switch Int32(code) {
            case COPAD_KEY_BYTES: return .bytes(Array(out[0 ..< outLen]))
            case COPAD_KEY_NOTHING: return .nothing
            default: return .text
            }
        }

        var bracketedPasteActive: Bool {
            guard let ptr else { return false }
            return copad_term_bracketed_paste_active(ptr)
        }

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

        /// Current working directory of the PTY's child via `proc_pidinfo` (macOS only); nil when
        /// the shell has exited or the syscall failed.
        func childCwd() -> String? {
            guard let ptr else { return nil }
            guard let sPtr = copad_term_child_cwd(ptr) else { return nil }
            defer { copad_string_destroy(sPtr) }
            var len = 0
            guard let bytes = copad_string_bytes(sPtr, &len), len > 0 else { return nil }
            let buf = UnsafeBufferPointer(start: bytes, count: len)
            return String(bytes: buf, encoding: .utf8)
        }

        /// Take the pending OSC 52 clipboard write, or nil if none. An empty string is a valid
        /// request ("clear clipboard") and must reach the policy gate, so only the Rust-side NULL
        /// maps to nil.
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

    /// Wraps a Rust-owned snapshot pointer.
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

        /// Borrowed; valid only while this Snapshot is alive.
        func rowRuns(_ row: UInt16) -> UnsafeBufferPointer<CopadRun> {
            guard let ptr else { return UnsafeBufferPointer(start: nil, count: 0) }
            var runsPtr: UnsafePointer<CopadRun>?
            let n = copad_snapshot_row_runs(ptr, row, &runsPtr)
            return UnsafeBufferPointer(start: runsPtr, count: n)
        }

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

        /// Current find match in viewport coordinates; `present == 0` when none is active or it
        /// scrolled out of view.
        var searchMatch: CopadSearchRange {
            let empty = CopadSearchRange(
                start_row: 0, start_col: 0, end_row: 0, end_col: 0,
                present: 0, _reserved: (0, 0, 0),
            )
            guard let ptr else { return empty }
            var out = empty
            copad_snapshot_search_match(ptr, &out)
            return out
        }

        /// Visible image runs in draw order (bottom-most first).
        var graphics: [CopadGraphic] {
            guard let ptr else { return [] }
            let n = copad_snapshot_graphic_count(ptr)
            guard n > 0 else { return [] }
            var out: [CopadGraphic] = []
            out.reserveCapacity(Int(n))
            var g = CopadGraphic()
            for i in 0 ..< n where copad_snapshot_graphic(ptr, i, &g) {
                out.append(g)
            }
            return out
        }

        /// Image `index` of this snapshot; its pixels are valid while the snapshot lives.
        func graphicImage(_ index: UInt32) -> CopadGraphicImage? {
            guard let ptr else { return nil }
            var out = CopadGraphicImage()
            return copad_snapshot_graphic_image(ptr, index, &out) ? out : nil
        }

        /// OSC 8 URI for the 1-based hyperlink id (`CopadRun.hyperlink_id`); nil for 0 or out of
        /// range.
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
