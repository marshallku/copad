# macOS renderer migration plan — SwiftTerm → alacritty_terminal + custom renderer

**Status:** Plan v2 (codex-plan round 1 + 2 reflected)
**Decision:** see `docs/decisions.md#31`
**Estimated effort:** 5-6 months MVP through Phase 7 (Phase 9 Metal deferred)

## Goal

Replace SwiftTerm (the terminal-emulation core inside `copad-macos`) with `alacritty_terminal` (Rust crate) wrapped behind a thin C FFI, and implement a first-party AppKit/CoreText renderer on top. CoreText for v1; Metal for v2 only if scrollback perf demands it.

Success criteria:

1. All four SwiftTerm blockers from decision #31 are gone (cursor visible against any image background; Korean/Japanese IME preedit visible during composition; reverse-video over transparent bg renders correctly; per-cell rendering hooks available for future smart-cursor / minimum-contrast features).
2. Feature parity with current SwiftTerm-based macOS app: panes, tabs, splits, scrollback, mouse selection, OSC 52, OSC 8 hyperlinks, font hot-reload, theme hot-reload, background image, status bar — all working under the new renderer.
3. Performance equal or better on representative workloads: cold startup, `cat` of a 100k-line file, `vim` opening a 10k-line buffer, `tmux` resize.
4. SwiftTerm dependency removed from `Package.swift` after one release of co-existence.

## Approach (codex-validated hybrid path)

Keep SwiftTerm as the production renderer. Build the alacritty renderer behind a runtime config flag. Migrate by vertical slices: each slice ships a working piece behind the flag, the user can opt in to compare, and we don't flip the default until every blocker phase has parity.

Why vertical slices over horizontal layers:

- Each slice is independently testable end-to-end (PTY → grid → render → user input).
- A horizontal-layer plan ("first build all of Rust, then all of Swift, then wire up") delays the first usable demo by months and hides integration bugs.
- Each slice gives the user a tangible toggle to try.
- If we abandon mid-migration, we still have a working SwiftTerm path.

## Architectural decisions

### D1. New Rust crate `copad-term` (not extending `copad-ffi`)

Existing `copad-ffi` exists to expose `copad-core`'s TriggerEngine + ActionRegistry over a C ABI. Terminal emulation is a separate large surface (PTY, grid, scrollback, parsing) that does not belong mixed into the same crate.

`copad-term/` will be a new workspace member containing:

- `Cargo.toml` with `alacritty_terminal` dependency, `crate-type = ["staticlib"]`
- `src/lib.rs` — C-ABI entry points
- `src/term.rs` — wraps `alacritty_terminal::Term`, owns PTY thread
- `src/snapshot.rs` — immutable grid snapshot for FFI consumers
- `src/input.rs` — keystroke → escape-sequence mapping

Swift side adds a new `CCopadTerm` clang-module target in `Package.swift` (parallel to existing `CCopadFFI`), with its own header + dummy.c + linker flag.

### D2. Threading model

- Rust owns one dedicated PTY-reader thread per terminal handle. It pumps PTY bytes into `alacritty_terminal::Term` (which is `Sync`-safe under `Mutex`).
- Render snapshots are taken on the Swift main thread by calling Rust FFI. Rust acquires the Term lock, walks the dirty range, fills a snapshot struct, releases the lock. Snapshot lifetime is bounded by the FFI call (no shared mutable state across threads).
- User keystrokes from Swift main thread → Rust input function → bytes written to PTY master via the same lock or a separate writer channel.
- No callbacks from Rust to Swift in v1 (Swift polls for dirty via the snapshot). Callbacks via FFI become viable later if polling proves wasteful.

### D3. FFI surface (initial) — row+run oriented

Codex round 2 flagged per-cell POD as under-modeled for real-world terminal rendering (combining marks, ZWJ emoji, wide cells, ligatures, hyperlink IDs, underline color/style all need expression). The boundary is row+run oriented, with row-contiguous UTF-8 to avoid per-run pointer lifetime issues.

```c
typedef void* copad_term_t;
typedef void* copad_snapshot_t;

typedef struct {
    uint16_t start_col;      // inclusive
    uint16_t end_col;        // exclusive; wide CJK = single run spanning both cells
    uint32_t utf8_offset;    // offset into the row's utf8 buffer
    uint32_t utf8_len;       // byte length within the row buffer
    uint32_t fg_rgba;
    uint32_t bg_rgba;        // sentinel 0 = default-bg (renderer materializes per [renderer] policy)
    uint16_t flags;          // bold/italic/underline/inverse/dim/strike/blink/wide-leading/wide-trailing
    uint8_t  underline_style; // 0=none 1=single 2=double 3=curly 4=dotted 5=dashed
    uint8_t  reserved;
    uint32_t underline_color_rgba; // 0 = use fg
    uint32_t hyperlink_id;   // 0 = none, opaque key into a separate hyperlink table
} copad_run_t;

copad_term_t copad_term_create(uint16_t cols, uint16_t rows,
                                  const char* shell, const char* cwd);
void copad_term_destroy(copad_term_t);

void copad_term_input(copad_term_t, const uint8_t* bytes, size_t len);
void copad_term_resize(copad_term_t, uint16_t cols, uint16_t rows);

copad_snapshot_t copad_term_snapshot(copad_term_t);
void copad_snapshot_destroy(copad_snapshot_t);

uint16_t copad_snapshot_rows(copad_snapshot_t);
uint16_t copad_snapshot_cols(copad_snapshot_t);

// Row-contiguous accessor pair: caller passes row index, gets back
// a borrowed pointer to that row's run array AND a borrowed pointer
// to the row's utf8 buffer. Both are valid until the snapshot is
// destroyed. Swift draws from borrowed memory during the frame, then
// calls copad_snapshot_destroy. No per-cell allocations.
size_t copad_snapshot_row_runs(copad_snapshot_t, uint16_t row, const copad_run_t** runs);
const uint8_t* copad_snapshot_row_utf8(copad_snapshot_t, uint16_t row, size_t* len);

void copad_snapshot_cursor(copad_snapshot_t, copad_cursor_t* out);
```

The snapshot owns all buffers (`Vec<u8>` per row + `Vec<copad_run_t>` per row). Swift borrows them during the frame; `copad_snapshot_destroy` releases. Combining marks and ZWJ emoji land inside `utf8` as actual byte sequences; ligature decisions happen in Swift via CoreText. Wide cells span columns within a single run (`start_col=N, end_col=N+2`).

The full surface grows over phases — selection getters in Phase 4, IME helpers in Phase 6, hyperlink table lookups in Phase 7.

### D4. Feature flag

Config gains a new section:

```toml
[renderer]
backend = "swiftterm"  # or "alacritty"
```

At pane construction time in `TabViewController`, branch on the config value and instantiate either `TerminalViewController` (current SwiftTerm-based) or `AlacrittyTerminalViewController` (new). Both conform to `CopadPanel` so the rest of the app — `PaneManager`, `SplitNode`, socket commands, daemon Invokes — sees no difference.

This means each phase ends with a runnable comparison: same shell, same config, two windows side-by-side, swap with a config edit + restart.

### D5. Renderer technology

CoreText for v1 (Phases 2–7). Reasons: native macOS font shaping handles ligatures and emoji correctly; AppKit drawing is well-understood; debugging via Quartz Debug; no GPU pipeline setup tax in the first 3 months.

Metal for v2 (Phase 8, deferred): only if measured scrollback render perf is a real bottleneck. iTerm2 ships both paths; we follow the same staged pattern.

### D6. Damage tracking

`alacritty_terminal::Term::damage` returns the dirty range (lines that changed since the last clear). Swift renderer uses this to redraw only changed cells per frame. CADisplayLink at 60 Hz drives a check; if no damage, no draw. Initial implementation may use `NSView.needsDisplay = true` for the whole bounds and refine later — measure first.

## Phases

Phase ordering reflects codex round 2 critique C4 (the user's original blocker — cursor invisibility on image bg — must ship in the first user-visible slice, not waiting until Phase 6) and C5 (IME ordering: must precede automation parity because IME constrains renderer composition, cursor rect reporting, and first-responder behavior — decisions made in earlier slices may need to be undone if IME comes last).

### Phase 0 — Pre-flight spike

**Scope:** one throwaway macOS binary that proves the three things that would invalidate the entire plan if broken:

1. **Dual staticlib linking** — link both existing `copad_ffi` AND new `copad_term` staticlibs into the same Swift binary. Call one symbol from each. No duplicate-symbol errors. Archive + sign + run on a clean machine.
2. **Snapshot ABI lifetime** — Rust creates a snapshot containing one row of runs (red cell + reverse-video cell + wide CJK cell + a combining-mark or ZWJ-emoji sample). Swift gets borrowed pointers via `copad_snapshot_row_runs` + `copad_snapshot_row_utf8`, draws from them during a frame, then calls `copad_snapshot_destroy`. Run with leaks instrument — no leaks, no dangling pointers, no crashes.
3. **Cursor + reverse-video over image bg** — Swift draws the one row over an `NSImageView`, with a cursor block as a separate overlay path. Reverse-video cell materializes `bg=default → theme.background` correctly. Cursor visible against arbitrary image content.

Intentionally ugly throwaway code in a `spikes/` subfolder. Not Production.

**Acceptance:** binary runs, all three behaviors verified visually + via leaks instrument. Document gotchas (build flags, link order, Rust std symbol behavior) for Phase 1.

**Risk:** dual-staticlib symbol collision blows up here. Mitigation: this IS the mitigation — the spike's only job is to surface it now, not after we've built half the renderer.

**Estimated:** 1 week.

### Phase 1 — `copad-term` crate scaffold + FFI handle/snapshot wiring

**Scope:** create `copad-term/` workspace member with full FFI surface from §D3 stubbed out (handle creation, no PTY yet — fixture snapshots returned). Add `CCopadTerm` Swift target in `Package.swift`. Wire `install-macos.sh` to build both staticlibs.

**Acceptance:** `swift build` produces a binary; Swift host can create a handle, request a snapshot containing a hardcoded fixture row, walk runs + utf8, destroy snapshot, destroy handle, no leaks.

**Risk:** `alacritty_terminal` may have platform-conditional dependencies or build-script behavior that conflicts with workspace build. Mitigation: spike (Phase 0) already proved linking; Phase 1 only adds the Cargo dep.

**Estimated:** 1 week.

### Phase 2 — PTY + grid via test harness

**Scope:** wire `alacritty_terminal::Term` + PTY spawn behind FFI. Snapshots are now real, sourced from Term's grid. No Swift rendering — verification is purely via a test harness that calls `copad_term_input("ls\n")` and reads back grid cells containing "ls".

**Acceptance:** spawn shell, type `printf 'hello'`, snapshot row 0 cols 0-4 contains 'h','e','l','l','o'. Resize: change cols/rows, snapshot reflects new dims after a re-prompt.

**Risk:** PTY threading on Rust side may interact badly with main-thread Swift waiting on FFI calls. Mitigation: minimal locking design — `Arc<Mutex<Term>>`, snapshot is a copy (row buffers cloned into `Vec` owned by the snapshot handle).

**Estimated:** 2 weeks.

### Phase 3 — Plain text + cursor + colors + transparency + reverse-video + image bg (the original-blocker slice)

**Scope:** the merged big slice that proves the original cursor-invisibility blocker is fixed end-to-end:

- `AlacrittyTerminalViewController` Swift class, custom `NSView` subclass that draws cells from snapshots via CoreText `CTLineCreateWithAttributedString` + `CTLineDraw`.
- Feature flag wired (`[renderer] backend = "alacritty"`); both renderers coexist; toggle via config + restart.
- ANSI palette wired through theme; full attribute support (bold/italic/underline/strikethrough/dim/blink/inverse).
- Zed pattern: cells with `bg=default` materialize to `theme.background` opaque BEFORE inverse swap, so reverse-video over transparent bg renders correctly (the fix for WezTerm #1076 / Microsoft Terminal #7014 class).
- Opt-in transparency: `[renderer] transparent_default_bg = true` makes default-bg cells transparent (letting image show through ONLY those cells). Off by default — cursor visibility wins.
- Cursor rendering (block/bar/underline + DECSCUSR + `\e[?25l/h`). Cursor draws as opaque theme.accent over the cell's already-materialized bg, so cursor is visible against any image regardless of `transparent_default_bg` setting.
- Background image layer hierarchy ported from existing `TerminalViewController`.

**Acceptance:** with `renderer.backend = "alacritty"`, open Copad, set image bg, run Claude Code — cursor block clearly visible. `printf '\e[7mreverse\e[0m\n'` shows visible reverse-video text against image bg. Toggle `transparent_default_bg` — visible difference in whether image shows through blank cells. Damage tracking land here too (`alacritty_terminal::Term::damage` driving partial repaints; CADisplayLink at 60 Hz, no draw when no damage).

**Risk:** CoreText shaping cost per row × full-screen redraw may be too slow at first. Mitigation: profile after first prompt renders; gate optimization on real numbers. Metal escape hatch is Phase 9.

**Estimated:** 6 weeks. The "big slice" — biggest one until Phase 6 IME.

### Phase 4 — Selection + clipboard + OSC 52 + OSC 8 + URL click

**Scope:** mouse drag selection, double-click word, triple-click line, Cmd+A. Selection rendering (theme.surface2 highlight). Cmd+C copies, Cmd+V pastes through the PTY. OSC 52 clipboard write gated by the same `[security] osc52` policy already in place for SwiftTerm path. OSC 8 hyperlinks (hyperlink_id field in snapshot runs → table lookup → Cmd+click opens URL). Plain-text URL detection (matches existing `URLClickHelper`).

**Acceptance:** select arbitrary text region, Cmd+C, paste into other app — content matches. Triple-click line works in shell output, `less`, `vim`. OSC 52 from a `printf` gated correctly. `printf '\e]8;;https://example.com\e\\click\e]8;;\e\\\n'` makes "click" Cmd-clickable.

**Risk:** mouse hit-testing across split panes + tab bar gestures may need careful event-handling ordering. Mitigation: reuse existing `URLClickHelper` + `PaneManager.installURLClickMonitor` patterns.

**Estimated:** 2 weeks.

### Phase 5 — Scrollback + mouse wheel + keyboard navigation

**Scope:** scrollback buffer access via `alacritty_terminal::Term`'s built-in scrollback. Mouse wheel scrolls history. Cmd+Up/Down (line), Shift+PgUp/PgDn (page), Cmd+Home/End (start/end of history). Scroll-to-bottom on PTY output (per terminal convention; configurable later via `[renderer] scroll_to_bottom_on_output`).

**Acceptance:** `cat large-file.txt` populates scrollback; wheel scrolls; typing returns to bottom. Scrollback persists across resize. Memory bounded by `alacritty_terminal`'s scrollback cap (configurable).

**Estimated:** 2 weeks.

### Phase 6 — IME (NSTextInputClient, Swift-side preedit composited)

**Scope:** real `NSTextInputClient` implementation. Swift holds preedit state (`preeditText: String?`, `preeditRange: NSRange`, `compositionStart: GridPosition`) per the codex round 2 preferred shape (option b — neither NSView overlay nor Term mutation). Renderer composites preedit text over the cursor cell with theme.surface1 highlight before drawing. On `insertText` (commit), preedit clears and bytes flow to PTY. `firstRect(forCharacterRange:)` returns the cell's screen rect so candidate window positions correctly.

Methods: `setMarkedText`, `unmarkText`, `markedRange`, `hasMarkedText`, `attributedSubstring`, `firstRect(forCharacterRange:)`, `selectedRange`, `validAttributesForMarkedText`, `characterIndex(for:)`.

Edge cases to cover explicitly:
- Composition cancel (Esc) — preedit clears, no PTY bytes sent
- Dead keys (Option+E then E = é) — handled via insertText with replacement range
- Focus change mid-composition (Cmd+Tab away then back) — commit on focus loss
- Pane switch mid-composition (Cmd+Shift+] in copad) — commit before pane swap
- Resize during composition — preedit cell position updates with new geometry
- Multi-pane simultaneous composition (only the focused pane has IME state)
- Synchronized output (`\e[?2026h`) during composition — preedit overlays the rendered snapshot regardless of sync state

Why it comes BEFORE automation/socket parity: codex round 2 — IME shape constraints (composition state, cursor rect reporting, first-responder behavior) bleed into renderer composition. Locking down automation APIs first may force IME to fight earlier decisions.

**Acceptance:** type "안녕하세요" in Korean 2-Set IME — preedit progression visible cell-by-cell, final commit replaces preedit with the literal bytes in PTY. Japanese 3-stage composition (hiragana → kanji candidate window → commit) works. Candidate window opens near the cursor cell, not screen origin. Focus changes commit cleanly. Resize during composition keeps preedit at the correct cell.

**Risk:** IME edge cases multiply. Mitigation: reference iTerm2 source; build manual test matrix in `docs/ime-test-matrix.md`.

**Estimated:** 6 weeks. (Codex round 1 said 4 was undercounted; codex round 2 confirmed 6.)

### Phase 7 — Automation / socket API parity

**Scope:** make the new renderer honor the full copad automation surface that `TerminalViewController` currently exposes:

- `terminal.state` — cols, rows, cursor row/col, title
- `terminal.read` — visible screen text (concat of row utf8)
- `terminal.history` — N lines of scrollback
- `terminal.context` — N lines + cursor position (for AI context capture)
- `terminal.exec`, `terminal.feed` — write to PTY
- `terminal.shell_precmd`, `terminal.shell_preexec` — OSC 133-style shell integration events
- Title change events (OSC 0/1/2) → broadcast to EventBus
- cwd events (OSC 7) → ContextService.apply
- Process termination → emit `processTerminated` event
- Seeded `cwd` + `initialInput` for `claude.start` flow
- OSC 52 policy hot-reload (`setOSC52Policy`)
- Font family + size hot-reload (`applyFont`)
- Theme hot-reload (`applyTheme`)
- Pane focus tracking (`activePanel` / `activeTerminal` accessors)

Each automation API needs an integration test using `coctl call` against a running Copad with `renderer.backend = "alacritty"`. The existing test inventory for SwiftTerm path is the parity baseline.

**Acceptance:** every `coctl call terminal.*` command that works on SwiftTerm path produces identical output shape on alacritty path. Daemon Invokes route to the right pane regardless of backend.

**Estimated:** 3 weeks. Larger than it looks because of integration-test surface.

### Phase 8 — Ligatures audit + deferred features

**Scope:** verify CoreText ligature support landed correctly in Phase 3 (it should be free when we use proper `CTLine` with the right font); add undercurl color (extended ANSI sequence `\e[58:2::R:G:Bm`); polish font fallback for emoji + CJK; any feature deferred from earlier phases that doesn't fit Sixel/kitty graphics scope.

**Acceptance:** `printf 'fi != !=\n'` with JetBrains Mono renders ligatures. `printf '\e[4:3m\e[58:2::255:0:0mcurly red underline\e[0m\n'` renders correctly.

**Estimated:** 1-2 weeks.

### Phase 9 — Metal renderer (deferred / optional)

**Scope:** only if Phase 3-8 measurements show CoreText is a bottleneck for workloads users hit. Pattern: glyph atlas texture, indexed quad batching, fragment shader for per-cell attributes (mirrors iTerm2's `iTermMetalDriver.m`).

**Acceptance:** N/A — gated on measurement.

**Estimated:** 4-6 weeks if needed.

### Phase 10 — Default swap + SwiftTerm removal

**Scope:** flip the default `[renderer] backend` to "alacritty". Keep SwiftTerm path as a fallback for one release cycle (in case of regression reports). Next release: delete `TerminalViewController` (SwiftTerm-based), `CopadTerminalView`, `Package.swift` SwiftTerm dependency, and the renderer config flag (no longer a choice).

**Acceptance:** one release cycle of bug reports settled, no regression escalations. Codebase shrinks by ~1500 lines of SwiftTerm wrapping.

**Estimated:** 1 week per step (flip + cleanup), spread over 2 releases.

## Risks (cross-cutting)

| # | Risk | Mitigation |
|---|---|---|
| R1 | `alacritty_terminal` API breaks across versions (maintainer doesn't support external use per #2132) | Pin specific version in `Cargo.toml`; vendor a fork in `vendor/alacritty_terminal/` if upstream diverges; budget for it. |
| R2 | CoreText per-row render too slow for large output | Profile during Phase 3; damage tracking lands same phase via `Term::damage`; Metal path is Phase 9 escape hatch. |
| R3 | IME edge cases (sync output, dead keys, candidate window positioning across split panes, focus changes mid-composition) | Reference iTerm2 source for known-good behavior; build manual test matrix in `docs/ime-test-matrix.md` during Phase 6. |
| R4 | FFI overhead per row × full-screen redraw amplifies latency | Row-contiguous utf8 + run array (§D3); Swift borrows during frame, no per-cell allocations. Batch reads only if profiling shows need. |
| R5 | Swift main thread can starve Rust PTY thread if rendering is heavy | Render off-main only if profiling proves the need; CADisplayLink-based throttling for now. |
| R6 | Migration takes longer than 6 months and Linux churn requires touching renderer code | Daemon-first architecture isolates renderer from Linux churn. Re-evaluate at month 4 if behind schedule. |
| R7 | Dual staticlib linking surfaces a Rust std symbol collision or build-flag conflict | Phase 0 spike's #1 task is to prove this. Mitigation paths if it fails: (a) merge `copad-term` into `copad-ffi` instead of separate crate, (b) build one staticlib as a Rust dynamic library, (c) cdylib instead. |
| R8 | Phase 3 (6 weeks) underestimates the combined slice; original-blocker proof slips, user loses trust | Half-way through Phase 3, demo the cursor-over-image fix even if other Phase 3 items aren't done. If the slice is genuinely 8+ weeks, peel scrollback (Phase 5) earlier to ship the cursor fix sooner. |

## Out of scope (explicitly deferred)

- Sixel / Kitty graphics protocol (defer to v2; complex spec, low user demand for copad's audience)
- GPU rendering (Phase 8, gated on measurement)
- Multi-window terminals (single tab = single PTY)
- Remote terminal protocols (mosh, ssh-mux)
- Built-in tmux replacement

## Codex-plan pressure-test results (Step 2)

**Round 1 (5 critical findings, all accepted):**

- **C1** D1 dual-staticlib link risk → added Phase 0 spike + R7 fallback paths
- **C2** D3 cell ABI under-modeled → §D3 redesigned row+run oriented with row-contiguous utf8
- **C3** Plan missed automation/socket parity → added Phase 7 explicitly
- **C4** Phase ordering wrong for stated blocker → Phase 3 merged colors+transparency+cursor+image to ship the original-blocker fix in the first user-visible slice
- **C5** IME estimate + shape too optimistic → 4w → 6w; "option b" (Swift-side preedit composited) chosen explicitly

**Round 2 (3 refinements, all accepted):**

- Spike scope addition: validate ABI ownership/lifetime via leaks instrument (one snapshot create + Swift borrow + draw + destroy round-trip, no leaks)
- ABI refinement: row-contiguous utf8 owned by snapshot (not per-run pointers — eliminates lifetime/alloc class of bugs)
- Phase reorder: IME at Phase 6, NOT after automation parity (IME constrains renderer composition + cursor rect + first-responder; later locking would force IME to fight earlier decisions)

No round 3 — refinements were clear and actionable.

## Total estimate

- Phase 0 spike: 1w
- Phases 1-2 (scaffold + PTY): 3w
- Phase 3 (big slice — original blocker fix): 6w
- Phases 4-5 (selection/clipboard + scrollback): 4w
- Phase 6 (IME): 6w
- Phase 7 (automation parity): 3w
- Phase 8 (ligatures audit + deferred): 1-2w
- Phase 9 (Metal): 4-6w if needed
- Phase 10 (default swap + cleanup): 2 release cycles

**MVP (Phases 0-7): 23 weeks ≈ 5.75 months.** Phase 9 deferred. Aligns with codex round 1's "3-4 month MVP" being optimistic for a solo dev; 4-6 month conservative was right.
