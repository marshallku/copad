// Phase 1 scaffold — see copad-term/src/lib.rs and
// docs/macos-renderer-migration-plan.md §D3.
//
// Pointer ownership:
//   copad_term_create -> CopadHandle*       Rust-owned, free with copad_term_destroy
//   copad_term_snapshot -> CopadSnapshot*   Rust-owned, free with copad_snapshot_destroy
//   *const CopadRun from row_runs            Borrowed from snapshot, valid until snapshot_destroy
//   *const uint8_t   from row_utf8            Borrowed from snapshot, same lifetime
//   copad_term_version() -> const char*      Static, no free

#ifndef COPAD_TERM_H
#define COPAD_TERM_H

#include <stdbool.h>
#include <stddef.h>
#include <stdint.h>

typedef struct CopadHandle CopadHandle;
typedef struct CopadSnapshot CopadSnapshot;
typedef struct CopadString CopadString;

typedef struct {
    uint16_t start_col;        // inclusive
    uint16_t end_col;          // exclusive; wide CJK / ZWJ emoji span both cells in one run
    uint32_t utf8_offset;      // byte offset into the row's utf8 buffer
    uint32_t utf8_len;
    // Tagged color: MSB is the discriminator.
    //   0x00_00_00_00            default (renderer materializes theme fg/bg)
    //   0x01_00_00_NN            indexed N (0..15 palette, 16..231 cube, 232..255 grayscale)
    //   0xFF_RR_GG_BB            direct RGB (always opaque)
    uint32_t fg_rgba;
    uint32_t bg_rgba;          // same encoding; 0 = default-bg sentinel
    uint16_t flags;
    uint8_t  underline_style;  // 0=none 1=single 2=double 3=curly 4=dotted 5=dashed
    uint8_t  reserved;
    uint32_t underline_color_rgba; // same encoding as fg_rgba; 0 = use fg
    uint32_t hyperlink_id;     // 0 = none; opaque key into separate hyperlink table (Phase 4+)
} CopadRun;

// Flags bit layout — must match copad_term::flags:
//   1 << 0  BOLD
//   1 << 1  ITALIC
//   1 << 2  UNDERLINE
//   1 << 3  INVERSE          (reverse video — fg/bg swap after default-bg materialize)
//   1 << 4  DIM
//   1 << 5  STRIKE
//   1 << 6  BLINK
//   1 << 7  WIDE_LEADING
//   1 << 8  WIDE_TRAILING

typedef struct {
    uint16_t row;
    uint16_t col;
    uint8_t  style;     // 0=hidden 1=block 2=bar 3=underline
    uint8_t  blink;     // 0=steady 1=blink
    uint16_t reserved;
} CopadCursor;

// Active selection bounds. end_row / end_col are INCLUSIVE
// (alacritty's SelectionRange convention). Meaningful only when
// `present == 1`. `is_block == 1` flags block selection (deferred).
typedef struct {
    uint16_t start_row;
    uint16_t start_col;
    uint16_t end_row;
    uint16_t end_col;
    uint8_t  is_block;
    uint8_t  present;
    uint16_t reserved;
} CopadSelectionRange;

// Selection-start kind discriminator for copad_term_selection_start.
// BLOCK is the rectangular (column-major) variant tied to
// Option+drag in the renderer (Terminal.app / iTerm2 native gesture; Cmd is reserved for URL click) — alacritty's SelectionType::Block.
// Snapshot reports `CopadSelectionRange.is_block = 1` so the renderer
// can paint a rectangle instead of the row-wrapped span.
#define COPAD_SELECTION_SIMPLE   0
#define COPAD_SELECTION_SEMANTIC 1
#define COPAD_SELECTION_LINES    2
#define COPAD_SELECTION_BLOCK    3

// Side discriminator (which side of the cell the click landed on).
#define COPAD_SIDE_LEFT  0
#define COPAD_SIDE_RIGHT 1

// --- Terminal lifecycle ---

/// `panel_id` may be NULL; when set, it's exported to the child shell
/// as `COPAD_PANEL_ID` so the copad-cwd shell hook can target the
/// right panel via `coctl call panel.report_cwd`.
/// `socket_path` may be NULL; when set, it's exported as
/// `COPAD_SOCKET` so `coctl` dials the GUI per-instance socket
/// owning the panel (not the well-known daemon path).
CopadHandle* copad_term_create(uint16_t cols, uint16_t rows,
                                  const char* shell, const char* cwd,
                                  const char* panel_id,
                                  const char* socket_path,
                                  const char* tmux_session);
void copad_term_destroy(CopadHandle* handle);

void copad_term_input(CopadHandle* handle, const uint8_t* bytes, size_t len);

/// macOS only — query the PTY child's current working directory via
/// `proc_pidinfo(PROC_PIDVNODEPATHINFO)`. Returns a `CopadString*` the
/// caller MUST free with `copad_string_destroy`, or NULL if the shell
/// has exited / the syscall failed / not running on macOS. Cheaper than
/// parsing OSC 7 from PTY output and works even when the shell doesn't
/// emit OSC 7. Used by Swift session-snapshot to capture the cwd a
/// restored tab should land in.
CopadString* copad_term_child_cwd(CopadHandle* handle);

/// Populate one entry of the OSC color-query palette. Index follows
/// `vte::ansi::NamedColor`: 0-15 ANSI (normal + bright), 16-255 256-color,
/// 256 foreground, 257 background, 258 cursor. Re-call for the same
/// index to overwrite on theme hot-reload. Indices never set make
/// `Event::ColorRequest` (OSC 4/10/11/12) a silent no-op — apps fall
/// back to defaults rather than getting a color we don't draw.
/// Returns 0 on success, -1 on NULL handle.
int copad_term_set_palette_entry(CopadHandle* handle,
                                   uint16_t index,
                                   uint8_t r, uint8_t g, uint8_t b);

void copad_term_resize(CopadHandle* handle, uint16_t cols, uint16_t rows);

// Returns true if the grid has any pending damage since the last call;
// always resets internal damage state. Intended for CADisplayLink-driven
// renderers to skip work when nothing changed.
//
// Thin wrapper over `copad_term_take_damage_rows` — callers MUST NOT
// invoke both per frame (the rows variant advances the same internal
// prev-state, so two calls would observe each other's writes).
bool copad_term_take_damage(CopadHandle* handle);

// Per-row damage drain — viewport row indices that need repaint since
// the last call. Returns:
//   * -1 — Full repaint required (scrollback offset changed, alacritty
//          signaled TermDamage::Full, OR the dirty list would exceed
//          `cap`); renderer should redraw the whole view.
//   * 0..=cap — exact dirty row count. The first `count` slots of
//          `out_buf` hold distinct viewport row indices, in unspecified
//          order; rows beyond `count` are untouched.
// Resets alacritty's damage state every call so the next invocation
// only sees subsequent changes.
//
// `out_buf` must point to writable storage for at least `cap` uint16_t
// slots when `cap > 0`; with `cap == 0` it MAY be null and the function
// returns -1 immediately.
int32_t copad_term_take_damage_rows(CopadHandle* handle,
                                     uint16_t* out_buf,
                                     uint16_t cap);

// True iff the PTY child process exited since the last call. Clears
// the latch on read (a second poll returns false). The renderer
// should broadcast `panel.exited` when this returns true so
// copad-core's ContextService cleans up per-panel cwd state.
bool copad_term_take_child_exit(CopadHandle* handle);

// --- In-terminal find ---

// Current find match in viewport coordinates. Same shape as
// CopadSelectionRange minus is_block (find can't be block-shaped).
typedef struct {
    uint16_t start_row;
    uint16_t start_col;
    uint16_t end_row;
    uint16_t end_col;
    uint8_t present; // 0 = no active match (cleared or off-screen)
    uint8_t _reserved[3];
} CopadSearchRange;

// Find next/prev match of `pattern` in grid + scrollback. Pattern is
// treated as a fixed string: the FFI escapes regex metacharacters
// internally, so callers can pass raw user input from a find-bar
// text field without any pre-processing. `case_sensitive` controls
// the case-fold bias explicitly (overrides alacritty's smart-case
// default). Wraps around grid boundaries. Returns true if a match
// was found (and the viewport was scrolled to make it visible).
bool copad_term_search_next(CopadHandle* handle,
                             const char* pattern,
                             bool case_sensitive,
                             bool forward);

// Drop the cached regex + current match — the renderer's highlight
// vanishes on the next frame.
void copad_term_search_clear(CopadHandle* handle);

// Project the snapshot's cached match into the caller's storage.
// `present == 0` when no active match or the match scrolled out of
// view. Callable any number of times per snapshot.
void copad_snapshot_search_match(const CopadSnapshot* snap,
                                  CopadSearchRange* out);

// --- Snapshot ---

CopadSnapshot* copad_term_snapshot(CopadHandle* handle);
void copad_snapshot_destroy(CopadSnapshot* snap);

uint16_t copad_snapshot_rows(const CopadSnapshot* snap);
uint16_t copad_snapshot_cols(const CopadSnapshot* snap);

// Sets *out_runs to a borrowed pointer; returns the run count. Both
// the pointer and the underlying memory live until snapshot_destroy.
size_t copad_snapshot_row_runs(const CopadSnapshot* snap, uint16_t row,
                                 const CopadRun** out_runs);

// Borrowed pointer to the row's utf8 bytes; same lifetime.
const uint8_t* copad_snapshot_row_utf8(const CopadSnapshot* snap, uint16_t row,
                                         size_t* out_len);

void copad_snapshot_cursor(const CopadSnapshot* snap, CopadCursor* out);
void copad_snapshot_selection(const CopadSnapshot* snap, CopadSelectionRange* out);

// --- Selection control ---

// Begin a new selection at (row, col, side) with the given kind
// (COPAD_SELECTION_*). Replaces any existing selection.
void copad_term_selection_start(CopadHandle* handle, uint16_t row, uint16_t col,
                                  uint8_t side, uint8_t kind);
void copad_term_selection_update(CopadHandle* handle, uint16_t row, uint16_t col, uint8_t side);
void copad_term_selection_clear(CopadHandle* handle);
void copad_term_selection_all(CopadHandle* handle);

// Heap-allocated UTF-8 copy of the current selection. NULL when
// nothing selected. Caller frees with copad_string_destroy exactly
// once.
CopadString* copad_term_selection_string(CopadHandle* handle);

// Last N rows of scrollback above the viewport top, rendered as plain
// text — '\n' between rows, no trailing newline. NUL cells render as
// space. When N exceeds the populated scrollback, returns however
// many rows exist (clamped, no panic). When N==0 or no scrollback
// exists, returns a non-NULL but length-0 CopadString. Returns NULL
// only when the handle pointer is NULL. Caller frees with
// copad_string_destroy exactly once.
CopadString* copad_term_history(CopadHandle* handle, size_t lines);

const uint8_t* copad_string_bytes(const CopadString* s, size_t* out_len);
void copad_string_destroy(CopadString* s);

// Renderer policy queries.
bool copad_term_mouse_mode_active(CopadHandle* handle);
bool copad_term_bracketed_paste_active(CopadHandle* handle);

// Mouse-event encoding negotiated by the TUI (SGR / legacy / UTF8 /
// none). Returns 0 when no reporting mode is on; otherwise picks the
// matching mutually-exclusive encoding bit. Use to format the bytes
// for forwarded scroll-wheel / click / drag events.
#define COPAD_MOUSE_ENC_NONE   0
#define COPAD_MOUSE_ENC_LEGACY 1
#define COPAD_MOUSE_ENC_SGR    2
#define COPAD_MOUSE_ENC_UTF8   3
uint8_t copad_term_mouse_encoding(CopadHandle* handle);

// Highest mouse-reporting level the TUI has enabled. Tiers stack:
// MOTION ⊇ DRAG ⊇ CLICK. Renderer uses this to gate which AppKit
// mouse events should forward (press/release always at CLICK+,
// drag-while-button-held at DRAG+, bare motion at MOTION).
#define COPAD_MOUSE_LEVEL_NONE   0
#define COPAD_MOUSE_LEVEL_CLICK  1
#define COPAD_MOUSE_LEVEL_DRAG   2
#define COPAD_MOUSE_LEVEL_MOTION 3
uint8_t copad_term_mouse_report_level(CopadHandle* handle);

// Drain the most-recent pending OSC 52 clipboard-store request.
// Returns NULL when nothing pending. Caller frees with
// copad_string_destroy and gates the system clipboard write on
// the user's [security] osc52 policy.
CopadString* copad_term_take_clipboard_request(CopadHandle* handle);

// Scrollback navigation. `kind` selects the variant; `delta` is only
// consulted for COPAD_SCROLL_DELTA (positive = older content scrolls
// in; negative = newer).
#define COPAD_SCROLL_DELTA     0
#define COPAD_SCROLL_PAGE_UP   1
#define COPAD_SCROLL_PAGE_DOWN 2
#define COPAD_SCROLL_TOP       3
#define COPAD_SCROLL_BOTTOM    4
void copad_term_scroll(CopadHandle* handle, uint8_t kind, int32_t delta);

// OSC 8 hyperlink URI lookup. `hyperlink_id` is the run's 1-based
// index from the snapshot; 0 means "no hyperlink". URI bytes are
// borrowed from snapshot storage — copy before destroy.
uint32_t copad_snapshot_hyperlink_count(const CopadSnapshot* snap);
const uint8_t* copad_snapshot_hyperlink_uri(const CopadSnapshot* snap,
                                              uint32_t hyperlink_id,
                                              size_t* out_len);

// Static string, no free required.
const char* copad_term_version(void);

#endif
